use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::*;
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::commands::*;
use crate::dap::mem_access::MemAccess;
use crate::pipeline::sample::ValueType;
use super::sample::{Sample, MAX_VALUES};
use super::ring_buffer::RingBuffer;

/// 单次 DAP_Transfer 命令的最大请求数（保守上限，避免超过 USB 包大小）
///
/// CMSIS-DAP v2 单包请求受 packet_size 限制。每个请求 5 字节
/// （1 字节 request + 4 字节 data），64 个请求 = 320 字节，远小于
/// 典型 packet_size (512/1024)。同时避免 total_requests as u8 截断。
const MAX_TRANSFER_REQUESTS: usize = 64;

/// 在途命令数上限（背压控制）
///
/// 提交线程最多领先接收线程 MAX_IN_FLIGHT 条命令。当 DAP-Link 吞吐不足
/// （SWD 时钟低、变量多、USB 全速带宽受限）时，提交线程自动跳过采样
/// 周期，实际采样率平滑匹配设备吞吐，避免：
/// - USB IN 端点无限堆积（停止时要排空数千条残留响应）
/// - 响应延迟膨胀（Watch 面板显示的值越来越滞后）
///
/// 吞吐充足时该值不会成为瓶颈（在途数远小于上限）。
const MAX_IN_FLIGHT: usize = 4;

/// 计算一条 DAP_Transfer 响应的字节长度
///
/// 响应 = [0x05, count, status] + 每个成功读请求 4 字节。
/// 设备在 FAULT/WAIT 重试耗尽时提前中止序列，`count` = 实际执行的
/// 请求数，可能小于发出的请求数——因此响应长度必须按 count 动态
/// 计算：固定长度切帧会把下一条响应的头几个字节吃进当前块，造成
/// 字节流永久错位（垃圾样本 + 在途计数失配）。
///
/// 命令内请求布局：[写 TAR + 写 DRW] × `n_writes`（不返回数据）
/// + [写 TAR + 读 DRW] × `num_vars`（每变量返回 4 字节）
fn transfer_frame_len(count: usize, n_writes: usize, num_vars: usize) -> usize {
    let write_reqs = n_writes * 2;
    // 读块内已执行的请求数；奇数位（TAR 之后）是 DRW 读，返回数据
    let read_reqs = count.saturating_sub(write_reqs);
    let reads = (read_reqs / 2).min(num_vars);
    3 + 4 * reads
}

/// 在途计数递减（防下溢：为 0 时不动作）
///
/// 协议失步后丢弃缓冲时，计数可能先于响应归零；usize 直接 fetch_sub
/// 会回绕成巨大值，使提交线程的背压判断永久成立（采样停摆）。
fn in_flight_dec(counter: &AtomicUsize) {
    let mut cur = counter.load(Ordering::SeqCst);
    while cur > 0 {
        match counter.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return,
            Err(actual) => cur = actual,
        }
    }
}

/// 流水线采集引擎
///
/// 双线程模型：
/// - 提交线程：以固定间隔构造 DAP_Transfer 命令并非阻塞写入 USB
/// - 接收线程：从 USB 读取响应、解析采样数据、推入环形缓冲区
pub struct PipelineEngine {
    usb: Arc<BulkTransfer>,
    dap: DapProtocol,
    /// 各变量的对齐访问信息（子字变量按所在对齐整字访问 + lane 提取）
    accesses: Vec<MemAccess>,
    /// 各变量的类型（决定读取请求数与提取方式）
    types: Vec<ValueType>,
    /// 目标采样间隔（微秒）
    interval_us: u64,
    /// 停止标志（主线程设为 false 时，子线程退出）
    running: Arc<AtomicBool>,
    /// 写入请求队列（主线程推送，提交线程消费）
    /// 元素: (变量索引, 原始位模式)
    write_queue: Arc<Mutex<VecDeque<(usize, u64)>>>,
    /// 各变量的最近一次原始位模式（接收线程更新；提交线程对子字/64 位
    /// 变量做读-改-写时取同批的旧值合并用）
    last_words: Arc<Mutex<Vec<Option<u64>>>>,
    /// 每条已发出命令中的写入请求数（提交线程入队、接收线程出队）。
    /// 响应长度按 count + 该值动态计算（写入请求不返回数据）
    writes_per_cmd: Arc<Mutex<VecDeque<u8>>>,
    /// 在途命令计数（提交线程发出命令后 +1，接收线程解析对应响应后 -1）。
    /// 背压控制：提交线程在途数达到 MAX_IN_FLIGHT 时跳过本轮
    in_flight: Arc<AtomicUsize>,
}

impl PipelineEngine {
    /// 从已初始化的 SWD 链路创建流水线引擎
    ///
    /// `usb` 和 `dap` 来自 `SwdLink::into_parts()`。
    /// `addresses` 是一组目标内存地址（1-8 个），`types` 与之一一对应
    /// （子字变量 u8/u16 按对齐整字读取后提取 lane）。
    /// `rate_hz` 是目标采样率。
    pub fn new(
        usb: Arc<BulkTransfer>,
        dap: DapProtocol,
        addresses: Vec<u32>,
        types: &[ValueType],
        rate_hz: u32,
    ) -> Self {
        let interval_us = (1_000_000.0 / rate_hz as f64) as u64;
        let accesses: Vec<MemAccess> = addresses
            .iter()
            .enumerate()
            .map(|(i, &addr)| {
                let size = types.get(i).map(|t| t.byte_size()).unwrap_or(4);
                MemAccess::from_addr_size(addr, size)
            })
            .collect();
        Self {
            usb,
            dap,
            accesses,
            types: types.to_vec(),
            interval_us,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::new(Mutex::new(VecDeque::new())),
            last_words: Arc::new(Mutex::new(Vec::new())),
            writes_per_cmd: Arc::new(Mutex::new(VecDeque::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 使用新采样率重建引擎（保持相同 USB/DAP/地址配置）
    ///
    /// 采样率在 UI 中修改后调用，返回新引擎替换旧的。
    pub fn with_rate(&self, rate_hz: u32) -> Self {
        Self {
            usb: Arc::clone(&self.usb),
            dap: self.dap,
            accesses: self.accesses.clone(),
            types: self.types.clone(),
            interval_us: (1_000_000.0 / rate_hz as f64) as u64,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::clone(&self.write_queue),
            last_words: Arc::clone(&self.last_words),
            // 独立计数器：新旧引擎互不干扰（背压信号各自独立）
            writes_per_cmd: Arc::new(Mutex::new(VecDeque::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 启动流水线，返回控制句柄
    ///
    /// 可重复调用（Stop 后再次 Start），不消费 self。
    ///
    /// `capacity`：环形缓冲容量（采样点数）
    pub fn start(&self, capacity: usize) -> Result<PipelineHandle> {
        // 重置停止标志（上一次 stop 可能已将其置为 false）
        self.running.store(true, Ordering::SeqCst);
        // 复位在途计数器
        self.in_flight.store(0, Ordering::SeqCst);
        // 清空写入数队列（上一次运行的残留条目会污染本会话的切帧）
        if let Ok(mut q) = self.writes_per_cmd.lock() {
            q.clear();
        }
        // 排空 USB IN 端点残留响应（上一次运行的命令响应可能仍留在
        // 端点缓冲中——Pause/Resume 复用同一 USB 句柄时尤其如此）。
        // 否则新会话会把旧响应当新数据消费，且写入数队列与响应失去
        // 对应关系，切帧从此错位。
        self.usb.flush_input();

        let ring = Arc::new(RingBuffer::new(capacity));
        let start_time = Instant::now();

        // 时间戳完全由接收线程的 seq 计算：
        //   timestamp = seq * interval_us / 1_000_000
        // 这是理想网格时间戳，不受 USB 写入/响应抖动影响，
        // 保证显示的采样间隔严格一致。无需跨线程队列传递。

        let submit = self.spawn_submit_thread(start_time)?;
        let collect = self.spawn_collect_thread(Arc::clone(&ring))?;

        Ok(PipelineHandle {
            submit_handle: submit,
            collect_handle: collect,
            ring_buffer: ring,
            running: Arc::clone(&self.running),
            write_queue: Arc::clone(&self.write_queue),
        })
    }

    /// 启动提交线程
    fn spawn_submit_thread(
        &self,
        start_time: Instant,
    ) -> Result<JoinHandle<()>> {
        let usb = Arc::clone(&self.usb);
        let interval_us = self.interval_us;
        let accesses = self.accesses.clone();
        let running = Arc::clone(&self.running);
        let write_queue = Arc::clone(&self.write_queue);
        let last_words = Arc::clone(&self.last_words);
        let writes_per_cmd = Arc::clone(&self.writes_per_cmd);
        let in_flight = Arc::clone(&self.in_flight);

        // 预构建 TransferRequest 数组（每次采样都一样）
        // 每变量 = 写 TAR + 读 DRW，共 2 条请求。
        // 子字变量（u8/u16）写对齐字地址，读回整字后由接收线程按 lane 提取。
        // AHB-AP 不支持非对齐访问：对非字对齐地址发起 32 位 DRW 读行为
        // 不可预测（实际芯片通常回退到对齐字），必须对齐后 lane 提取。
        // 每变量 = 写 TAR + 读 DRW × words。
        // 32 位及更窄：1 次读；8 字节类型：2-3 次读（AHB-AP 无 64 位访问）。
        let requests: Vec<TransferRequest> = accesses
            .iter()
            .flat_map(|acc| {
                let mut reqs = vec![TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr)];
                for _ in 0..acc.words {
                    reqs.push(TransferRequest::read_ap(AP_REG_DRW));
                }
                reqs
            })
            .collect();



        let dap_index = self.dap.dap_index;

        // 预构建读取请求字节（固定部分，每次采样都一样）
        // 每变量 = 写 TAR + 读 DRW，共 2 条请求。
        // 头部第 3 字节（total_requests）在循环内按写入数量重建。
        let mut read_part: Vec<u8> = Vec::with_capacity(requests.len() * 5 + 3);
        read_part.push(DAP_TRANSFER);
        read_part.push(dap_index);
        read_part.push(requests.len() as u8);
        for req in &requests {
            read_part.push(req.request_byte());
            if !req.rnw {
                let data = req.write_data.unwrap_or(0);
                read_part.extend_from_slice(&data.to_le_bytes());
            }
        }

        // 循环外复用缓冲区，避免每采样周期堆分配
        let mut pending_writes: Vec<(usize, u64)> = Vec::with_capacity(24);
        let mut cmd: Vec<u8> = Vec::with_capacity(read_part.len() + 24 * 10);

        let handle = thread::Builder::new()
            .name("dap-submit".into())
            .spawn(move || {
                let mut seq: u64 = 0;
                let start = start_time;

                while running.load(Ordering::Relaxed) {
                    // --- 背压控制 ---
                    // 在途命令数达到上限（接收线程尚未消费完前一批响应）时
                    // 跳过本轮：实际采样率自动匹配 DAP-Link 吞吐，避免
                    // USB IN 端点无限堆积与响应延迟膨胀（Watch 值滞后）。
                    // 吞吐充足时在途数远小于上限，不影响目标采样率。
                    if in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
                        seq += 1;
                        let target = start + Duration::from_micros(interval_us * seq);
                        let now = Instant::now();
                        if now < target {
                            spin_sleep::sleep(target - now);
                        }
                        continue;
                    }

                    // --- 构造 DAP_Transfer 命令 ---
                    // 检查是否有待写入的请求，有则附加到命令前部。
                    // 限制每轮消耗的写入数，确保 total_requests 不超过
                    // MAX_TRANSFER_REQUESTS（避免 u8 截断 + USB 包超限）。
                    // 每次写入最多占 1 条写 TAR + 2 条写 DRW（64 位字需两次）
                    let max_writes = (MAX_TRANSFER_REQUESTS.saturating_sub(requests.len())) / 3;
                    pending_writes.clear();
                    if let Ok(mut wq) = write_queue.lock() {
                        while pending_writes.len() < max_writes {
                            if let Some((var_idx, value)) = wq.pop_front() {
                                pending_writes.push((var_idx, value));
                            } else {
                                break;
                            }
                        }
                    }

                    // 构建 request 列表：[写入请求...] + [读取请求...]
                    // 写入: write TAR + write DRW (2 requests, 无返回数据)
                    // 读取: write TAR + read DRW (2 requests, 1 返回数据)
                    // 子字变量：用最近采样到的整字做读-改-写，只写变量的
                    // 字节 lane，不踩相邻变量。被跳过的写入不计入头部总数
                    // （固件按头部计数逐条解析请求字节，计数必须与字节一致）。
                    cmd.clear();
                    cmd.push(DAP_TRANSFER);
                    cmd.push(dap_index);
                    let count_pos = cmd.len();
                    cmd.push(0u8); // 头部第 3 字节：实际请求数，末尾回填

                    let words_guard = last_words.lock().ok();
                    let mut n_requests = 0usize;
                    for &(var_idx, value) in &pending_writes {
                        let Some(acc) = accesses.get(var_idx) else {
                            log::warn!("写入请求被跳过：变量索引 {} 无对齐访问信息", var_idx);
                            continue;
                        };

                        if acc.words > 1 {
                            // 64 位变量：AHB-AP 无 64 位访问，拆成低/高两个
                            // 32 位字写。自然对齐（8 字节）时变量独占两个字，
                            // 无需读-改-写；未对齐时首尾字与相邻变量共享，
                            // 无法安全写入 → 跳过并告警（ELF 的 double 恒为
                            // 8 字节对齐，此分支仅防御畸形输入）。
                            if acc.aligned_addr & 7 != 0 {
                                log::warn!(
                                    "写入请求被跳过：变量 {} 的 8 字节数据未 8 字节对齐 (0x{:08X})",
                                    var_idx, acc.aligned_addr
                                );
                                continue;
                            }
                            let (low, high) = MemAccess::split_wide(value);
                            let w_tar = TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr);
                            let w_drw_lo = TransferRequest::write_ap(AP_REG_DRW, low);
                            let w_drw_hi = TransferRequest::write_ap(AP_REG_DRW, high);
                            cmd.push(w_tar.request_byte());
                            cmd.extend_from_slice(&acc.aligned_addr.to_le_bytes());
                            cmd.push(w_drw_lo.request_byte());
                            cmd.extend_from_slice(&low.to_le_bytes());
                            cmd.push(w_drw_hi.request_byte());
                            cmd.extend_from_slice(&high.to_le_bytes());
                            n_requests += 3;
                            continue;
                        }

                        if acc.mask == u32::MAX {
                            // 32 位整字：直接写，无需旧值
                            let w_tar = TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr);
                            let w_drw = TransferRequest::write_ap(AP_REG_DRW, value as u32);
                            cmd.push(w_tar.request_byte());
                            cmd.extend_from_slice(&acc.aligned_addr.to_le_bytes());
                            cmd.push(w_drw.request_byte());
                            cmd.extend_from_slice(&(value as u32).to_le_bytes());
                            n_requests += 2;
                            continue;
                        }

                        // 子字变量（u8/u16）：用最近一次采样的整字合并，
                        // 只覆盖本变量 lane，不踩相邻变量
                        let last = words_guard
                            .as_ref()
                            .and_then(|w| w.get(var_idx))
                            .copied()
                            .flatten();
                        let Some(old) = last else {
                            log::warn!(
                                "写入请求被跳过：变量 {} 尚未采样到旧值，无法读-改-写",
                                var_idx
                            );
                            continue;
                        };
                        let write_data = acc.merge(old as u32, value as u32);
                        let w_tar = TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr);
                        let w_drw = TransferRequest::write_ap(AP_REG_DRW, write_data);
                        cmd.push(w_tar.request_byte());
                        cmd.extend_from_slice(&acc.aligned_addr.to_le_bytes());
                        cmd.push(w_drw.request_byte());
                        cmd.extend_from_slice(&write_data.to_le_bytes());
                        n_requests += 2;
                    }

                    // 再读取请求（复用预构建字节，跳过头部）
                    cmd.extend_from_slice(&read_part[3..]);
                    n_requests += requests.len();
                    debug_assert!(n_requests <= MAX_TRANSFER_REQUESTS);
                    // 回填实际请求数
                    cmd[count_pos] = n_requests as u8;

                    // --- 发送命令（200ms 超时）---
                    // write_nonblock 使用 200ms 超时，确保停止标志能被及时检查到
                    if !pending_writes.is_empty() {
                        log::info!(
                            "提交线程注入 {} 个写入请求 (seq={})",
                            pending_writes.len(), seq
                        );
                    }
                    if let Err(e) = usb.write_nonblock(&cmd) {
                        if !running.load(Ordering::Relaxed) {
                            // 停止中遇到的超时/错误，正常退出
                            log::info!("提交线程收到停止信号，退出");
                            break;
                        }
                        log::error!("提交线程 USB 写失败 (seq={}): {}", seq, e);
                        break;
                    }
                    // 命令已发出：记录本条命令的写入数（接收线程按它计算
                    // 响应长度），在途 +1
                    if let Ok(mut q) = writes_per_cmd.lock() {
                        q.push_back(pending_writes.len() as u8);
                    }
                    in_flight.fetch_add(1, Ordering::SeqCst);

                    seq += 1;

                    // --- 精确等间隔控制 ---
                    // 下一个目标时刻 = start + interval_us * seq
                    let target = start + Duration::from_micros(interval_us * seq);
                    let now = Instant::now();
                    if now < target {
                        // spin_sleep 提供微秒级精度的混合睡眠
                        spin_sleep::sleep(target - now);
                    }
                }

                log::info!("提交线程退出，共提交 {} 个请求", seq);
            })
            .map_err(|e| Error::PipelineThread(e.to_string()))?;

        Ok(handle)
    }

    /// 启动接收线程
    fn spawn_collect_thread(
        &self,
        ring: Arc<RingBuffer>,
    ) -> Result<JoinHandle<()>> {
        let usb = Arc::clone(&self.usb);
        let running = Arc::clone(&self.running);
        let accesses = self.accesses.clone();
        let num_vars = accesses.len();
        let interval_us = self.interval_us;
        let last_words = Arc::clone(&self.last_words);
        let writes_per_cmd = Arc::clone(&self.writes_per_cmd);
        let in_flight = Arc::clone(&self.in_flight);

        let handle = thread::Builder::new()
            .name("dap-collect".into())
            .spawn(move || {
                let mut buf = vec![0u8; 4096];
                // carry-over buffer：保存上次读取的不完整响应尾部，
                // 下次读取时拼接到开头再解析，避免解析错位。
                let mut carry: Vec<u8> = Vec::new();
                // 拼接缓冲：循环外复用，避免每轮堆分配
                let mut data: Vec<u8> = Vec::with_capacity(4096);
                let mut seq: u64 = 0;

                while running.load(Ordering::Relaxed) {
                    // 读取 USB 响应（超时 200ms，允许优雅退出）
                    let n = match usb.read_timeout(&mut buf, Duration::from_millis(200)) {
                        Ok(n) => n,
                        Err(e) => {
                            // 超时可能只是暂无数据，检查运行标志后继续。
                            // 设备拔出等错误同样在此处记录——UI 侧仍显示
                            // Running，由用户手动停止（见 README 已知限制）。
                            if running.load(Ordering::Relaxed) {
                                log::warn!("接收线程读失败: {}", e);
                            }
                            continue;
                        }
                    };

                    // 拼接 carry + 本次读取的数据到一个连续缓冲区
                    // （复用缓冲区，仅重置长度，不重新分配）
                    data.clear();
                    data.extend_from_slice(&carry);
                    data.extend_from_slice(&buf[..n]);
                    carry.clear();

                    // 一次 read_bulk 可能读到多个响应（USB Bulk 合并短包）。
                    // 响应长度是动态的（FAULT/WAIT 时设备提前中止，count
                    // 小于发出的请求数），必须按响应头 [0x05, count, status]
                    // 逐条计算长度，绝不能按固定长度切帧。
                    let mut offset = 0;
                    while offset < data.len() {
                        // 头部不足 3 字节 → 不完整，留给 carry
                        if data.len() - offset < 3 {
                            break;
                        }
                        // 响应头必须是 DAP_Transfer 回显；不是则字节流已失步
                        // （此前从未发生失步时不应到达此处）。丢弃整个缓冲
                        // 并复位在途计数，等待重新对齐。
                        if data[offset] != DAP_TRANSFER {
                            log::error!(
                                "接收线程协议失步：响应头 0x{:02X} 不是 DAP_Transfer，丢弃 {} 字节重新同步",
                                data[offset], data.len() - offset
                            );
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        }
                        let count = data[offset + 1] as usize;
                        // 本条命令的写入数：提交线程每发一条命令入队一条。
                        // 只 peek——响应可能跨多次 USB 读，完整消费后才出队。
                        let n_writes = write_counts_front(&writes_per_cmd);
                        let Some(n_writes) = n_writes else {
                            log::error!("接收线程写入数队列为空，丢弃 {} 字节重新同步", data.len() - offset);
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        };
                        let total_requests = 2 * (n_writes + num_vars);
                        if count > total_requests {
                            log::error!(
                                "接收线程协议失步：count={} 超过命令请求数 {}，丢弃 {} 字节重新同步",
                                count, total_requests, data.len() - offset
                            );
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        }
                        let need = transfer_frame_len(count, n_writes, num_vars);
                        // 不完整响应 → 留给 carry，下次读取拼接后再解析
                        if data.len() - offset < need {
                            break;
                        }
                        let chunk = &data[offset..offset + need];
                        offset += need;

                        // 完整消费一条响应：写入数出队 + 在途递减
                        if let Ok(mut q) = writes_per_cmd.lock() {
                            q.pop_front();
                        }
                        in_flight_dec(&in_flight);

                        // 解析单个响应
                        let resp = match DapProtocol::parse_transfer_response(chunk) {
                            Ok(r) => r,
                            Err(e) => {
                                log::error!("接收线程解析失败 (seq={}): {}", seq, e);
                                seq += 1;
                                continue;
                            }
                        };

                        if resp.status != TRANSFER_OK {
                            log::warn!(
                                "接收线程收到非 OK 状态 (seq={}): status={}, count={}",
                                seq, resp.status, resp.count
                            );
                            seq += 1;
                            continue;
                        }

                        // 时间戳完全由 seq 计算（理想网格时间戳），
                        // 不受 USB 写入/响应抖动影响，保证显示间隔严格一致。
                        let timestamp_sec = (interval_us * seq) as f64 / 1_000_000.0;

                        // 响应的数据区按“读请求的完成顺序”排列，而每个变量
                        // 可能占 1-2 个读请求（64 位类型两次 DRW 读）。按
                        // read_offsets 把读回的 32 位字归组后再拼成变量值。
                        //
                        // 注意：FAULT/WAIT 提前中止时，只有已完成的读请求
                        // 携带数据，此时尾部变量缺失数据——用 0 填充并保持
                        // 通道位置对齐（不做位移，否则波形通道会串号）。
                        let mut vals = [0u64; MAX_VALUES];
                        {
                            // 数据区只包含成功完成的读请求，按变量顺序归组：
                            // 每个变量消耗 reads_per_var 个 32 位字
                            // （32 位及更窄 1 个，64 位 2 个）。
                            // FAULT 提前中止时尾部变量无数据，保持 0 且
                            // 不移位（否则通道会串号）。
                            let words = resp.values();
                            let mut wi = 0usize;
                            let mut lw_guard = last_words.lock().ok();
                            if let Some(lw) = lw_guard.as_mut() {
                                lw.resize(num_vars, None);
                            }
                            for (vi, acc) in accesses.iter().enumerate().take(MAX_VALUES) {
                                let need = acc.words as usize;
                                if wi + need > words.len() {
                                    // 该变量数据缺失：位置保持 0
                                    continue;
                                }
                                let buf = &words[wi..wi + need];
                                wi += need;
                                vals[vi] = acc.extract_wide(buf);
                                if let Some(lw) = lw_guard.as_mut() {
                                    lw[vi] = Some(vals[vi]);
                                }
                            }
                        }

                        let n = accesses.len().min(MAX_VALUES);
                        let sample = Sample::new(seq, timestamp_sec, &vals[..n]);

                        ring.push(sample);
                        seq += 1;
                    }

                    // 保存不完整的尾部到 carry，下次读取时拼接
                    if offset < data.len() {
                        carry.extend_from_slice(&data[offset..]);
                        log::debug!(
                            "接收线程保留 {} 字节不完整响应到下次解析",
                            data.len() - offset
                        );
                    }
                }

                log::info!("接收线程退出，共接收 {} 个采样点", seq);
            })
            .map_err(|e| Error::PipelineThread(e.to_string()))?;

        Ok(handle)
    }
}

/// 读取写入数队首（不消费）；队列为空返回 None
fn write_counts_front(queue: &Mutex<VecDeque<u8>>) -> Option<usize> {
    queue.lock().ok().and_then(|q| q.front().copied()).map(|v| v as usize)
}

/// 流水线控制句柄
///
/// 主线程通过此句柄消费环形缓冲区中的采样数据。
pub struct PipelineHandle {
    submit_handle: JoinHandle<()>,
    collect_handle: JoinHandle<()>,
    ring_buffer: Arc<RingBuffer>,
    running: Arc<AtomicBool>,
    write_queue: Arc<Mutex<VecDeque<(usize, u64)>>>,
}

impl PipelineHandle {
    /// 推入一个写入请求（变量索引 + 值）
    ///
    /// 提交线程会在下一个采样周期将此写入操作与读取操作合并为一条
    /// DAP_Transfer 命令，不影响正常采样。
    /// 子字变量（u8/u16）由提交线程读-改-写：用最近采样到的对齐整字
    /// 合并变量的字节 lane，不破坏相邻变量。
    pub fn queue_write(&self, var_idx: usize, value: u64) {
        if let Ok(mut wq) = self.write_queue.lock() {
            wq.push_back((var_idx, value));
        }
    }

    /// 从环形缓冲区批量读取采样点
    ///
    /// 返回实际读取的数量（可能少于 buf.len()）。
    pub fn drain_samples(&self, buf: &mut [Sample]) -> usize {
        self.ring_buffer.pop_batch(buf)
    }

    /// 查询缓冲区中可用的采样点数
    pub fn available_samples(&self) -> usize {
        self.ring_buffer.available()
    }

    /// 查询已采集的总采样点数
    pub fn total_samples(&self) -> usize {
        self.ring_buffer.total_written()
    }

    /// 查询环形缓冲累计覆盖（丢弃）的采样点数
    ///
    /// 消费端（UI）跟不上采样速度时非零，用于状态栏提示数据丢失。
    pub fn overwritten_samples(&self) -> u64 {
        self.ring_buffer.overwritten()
    }

    /// 优雅停止采集
    ///
    /// 设置停止标志，等待两个子线程退出。
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        log::info!("等待提交线程退出...");
        let _ = self.submit_handle.join();
        log::info!("提交线程已退出");
        log::info!("等待接收线程退出...");
        let _ = self.collect_handle.join();
        log::info!("接收线程已退出");
        log::info!("流水线已停止");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_len_full_ok_response() {
        // 全部成功：count = 2*n_writes + 2*num_vars，每变量 4 字节
        assert_eq!(transfer_frame_len(8, 0, 4), 3 + 16);
        // 1 条写入（2 请求）+ 4 变量（8 请求）= 10 条
        assert_eq!(transfer_frame_len(10, 1, 4), 3 + 16);
    }

    #[test]
    fn frame_len_fault_mid_sequence() {
        // n_writes=1（2 条写请求）+ 4 变量（8 条读请求）
        // 请求序列: [WTAR,WDRW] + [TAR0,DRW0,TAR1,DRW1,TAR2,DRW2,TAR3,DRW3]
        // 在第 5 条请求处 FAULT：2 写 + TAR0,DRW0,TAR1 → 仅 DRW0 返回数据
        assert_eq!(transfer_frame_len(5, 1, 4), 3 + 4);
        // 在第 4 条请求处 FAULT：2 写 + TAR0,DRW0 → 1 个读数据
        assert_eq!(transfer_frame_len(4, 1, 4), 3 + 4);
        // 在写块内 FAULT：无读数据
        assert_eq!(transfer_frame_len(1, 1, 4), 3);
    }

    #[test]
    fn frame_len_fault_within_var_pair() {
        // count 恰好停在变量对之间（TAR 已执行、DRW 未执行）：该变量无数据
        // n_writes=0，count=3 → TAR0,DRW0,TAR1 → 仅 1 个读数据
        assert_eq!(transfer_frame_len(3, 0, 4), 3 + 4);
    }

    #[test]
    fn in_flight_dec_never_underflows() {
        // 协议失步/多余响应时计数可能先归零：直接 fetch_sub 会回绕成
        // usize::MAX，使背压判断永久成立（采样停摆）
        let c = AtomicUsize::new(0);
        in_flight_dec(&c);
        assert_eq!(c.load(Ordering::SeqCst), 0);

        let c = AtomicUsize::new(2);
        in_flight_dec(&c);
        in_flight_dec(&c);
        in_flight_dec(&c);
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn fault_frame_does_not_break_stream_framing() {
        // 模拟字节流：一条 FAULT 短响应 + 一条完整 OK 响应拼接。
        // 旧实现按固定长度切帧会把第二条响应的头字节吃进第一条，
        // 造成字节流永久错位；动态切帧必须恰好切出两条。
        let num_vars = 2usize;
        let n_writes = 1usize;

        // FAULT 响应：2 写 + TAR0,DRW0,TAR1 后中止 → count=5，仅 1 个读数据
        let mut fault = vec![DAP_TRANSFER, 5u8, TRANSFER_FAULT];
        fault.extend_from_slice(&0xAAAA_0001u32.to_le_bytes());
        // OK 响应：count = 2*(1+2) = 6，2 个读数据
        let mut ok = vec![DAP_TRANSFER, (2 * (n_writes + num_vars)) as u8, TRANSFER_OK];
        ok.extend_from_slice(&0xBBBB_0001u32.to_le_bytes());
        ok.extend_from_slice(&0xBBBB_0002u32.to_le_bytes());
        let mut stream = fault.clone();
        stream.extend_from_slice(&ok);

        // 按接收线程的算法切帧
        let mut offset = 0;
        let mut frames = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back(n_writes as u8);
        queue.push_back(0u8); // 第二条命令无写入
        while offset < stream.len() {
            assert!(stream.len() - offset >= 3);
            assert_eq!(stream[offset], DAP_TRANSFER, "frame echo byte");
            let count = stream[offset + 1] as usize;
            let nw = queue.front().copied().unwrap() as usize;
            let need = transfer_frame_len(count, nw, num_vars);
            assert!(
                stream.len() - offset >= need,
                "frame incomplete at offset {offset}"
            );
            frames.push(stream[offset..offset + need].to_vec());
            offset += need;
            queue.pop_front();
        }
        assert_eq!(frames.len(), 2, "both frames must be sliced exactly");

        let r1 = DapProtocol::parse_transfer_response(&frames[0]).unwrap();
        assert_eq!(r1.status, TRANSFER_FAULT);
        assert_eq!(r1.count, 5);
        assert_eq!(r1.values(), &[0xAAAA_0001]);

        let r2 = DapProtocol::parse_transfer_response(&frames[1]).unwrap();
        assert_eq!(r2.status, TRANSFER_OK);
        assert_eq!(r2.values(), &[0xBBBB_0001, 0xBBBB_0002]);
    }

    // ---- 64 位类型：请求布局与响应归组 ----

    /// 计算读取块的请求数（每变量 写TAR + words×读DRW）
    fn read_block_requests(types: &[ValueType]) -> usize {
        types
            .iter()
            .map(|t| 1 + MemAccess::from_addr_size(0, t.byte_size()).words as usize)
            .sum()
    }

    #[test]
    fn read_block_scales_with_type_width() {
        use ValueType::*;
        assert_eq!(read_block_requests(&[Float, Float, Float, Float]), 8);
        // 一个 double 多占一个读请求：1(写TAR)+2(读DRW) + 4×2 = 11
        assert_eq!(read_block_requests(&[Float64, Float, Float, Float]), 3 + 6);
    }

    #[test]
    fn response_values_group_by_variable_width() {
        // 混合宽度响应：double(2 字) + float(1 字) + u16(1 字)
        // 引擎按 accesses.words 顺序消费读回的字
        let accesses = [
            MemAccess::from_addr_size(0x2000_0000, 8),
            MemAccess::from_addr_size(0x2000_0008, 4),
            MemAccess::from_addr_size(0x2000_000E, 2),
        ];
        let d = 3.25f64;
        let (d_low, d_high) = MemAccess::split_wide(d.to_bits());
        let words = [d_low, d_high, 1.5f32.to_bits(), 0x2222_1111];

        let mut vals = [0u64; 3];
        let mut wi = 0usize;
        for (vi, acc) in accesses.iter().enumerate() {
            let need = acc.words as usize;
            let buf = &words[wi..wi + need];
            wi += need;
            vals[vi] = acc.extract_wide(buf);
        }
        assert_eq!(ValueType::Float64.to_f64(vals[0]), 3.25);
        assert_eq!(ValueType::Float.to_f64(vals[1]), 1.5);
        // 变量位于字内高 16 位（addr+2）：extract 后低位对齐
        assert_eq!(vals[2], 0x0000_2222);
        assert_eq!(ValueType::Uint16.to_f64(vals[2]), 0x2222u64 as f64);
    }

    #[test]
    fn truncated_response_keeps_channel_positions() {
        // FAULT 提前中止：只返回了第一个变量（double 的两个字）
        // 后续变量必须保持 0 且不位移，否则波形通道会串号
        let accesses = [
            MemAccess::from_addr_size(0x2000_0000, 8),
            MemAccess::from_addr_size(0x2000_0008, 4),
        ];
        let d = 1.5f64;
        let (lo, hi) = MemAccess::split_wide(d.to_bits());
        // 只回了一个字（高字缺失）→ 该变量数据不完整，也按缺失处理
        let words = [lo, hi];

        let mut vals = [0u64; 2];
        let mut wi = 0usize;
        for (vi, acc) in accesses.iter().enumerate() {
            let need = acc.words as usize;
            if wi + need > words.len() {
                continue; // 保持 0
            }
            vals[vi] = acc.extract_wide(&words[wi..wi + need]);
            wi += need;
        }
        assert_eq!(ValueType::Float64.to_f64(vals[0]), 1.5);
        // 通道 1 无数据但位置保持（值为 0），不是把通道 2 的数据填到通道 1
        assert_eq!(vals[1], 0u64);
    }

    #[test]
    fn parse_rejects_wrong_echo() {
        // 字节流失步时响应头不是 0x05：必须显式报错而非解析出垃圾数据
        let bad = vec![0x06u8, 2, 1, 0, 0, 0, 0];
        assert!(DapProtocol::parse_transfer_response(&bad).is_err());
    }
}
