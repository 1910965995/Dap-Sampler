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

/// 每周期为交互式写入预留的请求数
///
/// 读取块若占满 MAX_TRANSFER_REQUESTS，写入预算 max_writes = 0，用户的
/// 交互写入（Watch 面板）将永远排队得不到执行——静默故障。预留 4 条
/// 恰好容纳一条最宽的 64 位写入（写 TAR + 写 DRW × 2 字）。
const WRITE_BUDGET_REQUESTS: usize = 4;

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

/// 计算一条 DAP_Transfer 命令的读取块请求数
///
/// 读取块按变量展开为 `words` 组 `[写 TAR, 读 DRW]`。每个字之前都重写
/// TAR，而不是依赖 AP 的 CSW AddrInc 位自增——AHB-AP 的 CSW 复位值由
/// 目标/探针固件决定，代码从未配置过它；若 AddrInc 未使能，第二次 DRW
/// 访问会重复命中同一地址（64 位变量的高字恒等于低字）。
fn read_block_requests(accesses: &[MemAccess]) -> usize {
    accesses.iter().map(|a| 2 * a.words as usize).sum()
}

/// 校验采样配置能否安全编码进流水线（启动前调用，返回明确错误）
///
/// 两条硬约束，违反任一都会产生静默故障而非显式报错：
/// - 变量数 > [`MAX_VALUES`]：`Sample.values` 是定长数组，`Sample::new`
///   会静默截断超出的值——波形上通道永远是一条平线却看不出原因
/// - 读块请求数超过预算：DAP_Transfer 的计数字段是 u8，且命令总长受
///   USB 端点包大小限制（MAX_TRANSFER_REQUESTS 已是保守值）；同时为
///   交互式写入预留 WRITE_BUDGET_REQUESTS 条，否则写入永久饿死
///
/// 旧实现只有线程内的 debug_assert!（release 下编译剔除），超限时
/// 命令直接损坏且无任何提示。
fn validate_config(accesses: &[MemAccess]) -> Result<()> {
    if accesses.len() > MAX_VALUES {
        return Err(Error::Other(format!(
            "Too many sampling variables: {} requested, max {} (sample slots would be silently truncated)",
            accesses.len(),
            MAX_VALUES
        )));
    }
    let requests = read_block_requests(accesses);
    let max_read_requests = MAX_TRANSFER_REQUESTS - WRITE_BUDGET_REQUESTS;
    if requests > max_read_requests {
        return Err(Error::Other(format!(
            "Sampling variables need {} transfer requests per cycle, exceeding the {}-request budget ({} reserved for interactive writes). Remove some channels or use narrower variable types",
            requests, max_read_requests, WRITE_BUDGET_REQUESTS
        )));
    }
    Ok(())
}

/// 读取块的请求类型序列（true = AP 读，false = 写）
///
/// 读取块对所有采样周期完全相同，预先构造一次。切帧时按设备实际执行的
/// 请求前缀模拟 posted-read 的数据产出，因此不能简单用 `count/2` 估算。
fn build_read_layout(accesses: &[MemAccess]) -> Vec<bool> {
    let mut layout = Vec::with_capacity(read_block_requests(accesses));
    for acc in accesses {
        for _ in 0..acc.words {
            layout.push(false); // 写 TAR
            layout.push(true);  // 读 DRW
        }
    }
    layout
}

/// 计算一条 DAP_Transfer 响应的字节长度
///
/// 响应 = `[0x05, count, status]` + 本次实际取回的数据字节。
///
/// 关键：DAPLink 的 AP 读是 **posted read**——读请求先把访问挂起，
/// 下一次 SWD 传输才取回上一次的数据，命令以 OK 结束时再补一次
/// RDBUFF 读。因此数据字数不是“读请求数 / 2”，必须按请求序列模拟：
///
/// - 每条**写**请求：若之前挂起了一个 AP 读，先取回 1 字（无论写成功与否）
/// - 每条**读**请求：若之前挂起了 AP 读，先取回 1 字，然后挂起本次读
/// - 命令以 OK 结束时：若仍有挂起的读，末尾补取 1 字
/// - 命令以 FAULT/WAIT 中止时：不做末尾补读（这次取数失败，不携带数据）
///
/// `count` 是设备实际执行的请求数——FAULT/WAIT 重试耗尽时序列提前中止，
/// 可能小于发出的请求数。旧实现按 `count/2` 估算读数：对 64 位变量会
/// 少算一半（一个 double 实际 2 个字只算 1 个），切帧偏短后下一条响应
/// 的头字节被吃进当前帧，字节流永久错位。
///
/// `write_reqs`：本条命令写入块的请求数；`read_layout`：读取块请求序列。
fn transfer_frame_len(
    count: usize,
    status: u8,
    write_reqs: usize,
    read_layout: &[bool],
) -> usize {
    let mut words = 0usize;
    let mut posted = false;

    // 写入块：每条写请求前若有挂起的 AP 读，先 drain 出 1 字
    for _ in 0..write_reqs.min(count) {
        if posted {
            words += 1;
            posted = false;
        }
    }

    // 读取块：只模拟设备实际执行的前缀
    let executed_reads = count.saturating_sub(write_reqs);
    for &is_read in read_layout.iter().take(executed_reads) {
        if is_read {
            if posted {
                words += 1;
            }
            posted = true;
        } else if posted {
            words += 1;
            posted = false;
        }
    }

    // OK 收尾：补取最后挂起的 AP 读
    if status == TRANSFER_OK && posted {
        words += 1;
    }
    3 + 4 * words
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

/// 流水线发给 UI 的非致命故障通知
///
/// `thread_error` 用于致命错误（自动停止采集）；`PipelineNotice` 用于
/// 「采集继续，但本次操作/数据受影响」的故障——提交线程跳过写入请求、
/// 接收线程协议失步丢弃数据等。这些故障此前只写日志，用户视线在波形
/// 上时完全无感知。
#[derive(Debug, Clone)]
pub enum PipelineNotice {
    /// 提交线程跳过一个写入请求（变量索引 + 原因）
    WriteSkipped { var_idx: usize, reason: String },
    /// 接收线程丢弃了数据（协议失步/解析失败，已自动恢复同步）
    DataDropped { message: String },
}

/// 追加一条通知（上限 16 条：防止故障风暴刷爆 UI 弹窗）
fn push_notice(notices: &Mutex<Vec<PipelineNotice>>, notice: PipelineNotice) {
    if let Ok(mut q) = notices.lock() {
        if q.len() < 16 {
            q.push(notice);
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
    /// 读取块的请求类型序列（false = 写 TAR，true = 读 DRW）
    ///
    /// 与协议无关的纯布局描述：提交线程据此构造请求字节，接收线程据此
    /// 模拟 DAPLink 的 posted-read 数据产出（切帧）。
    read_layout: Vec<bool>,
    /// 目标采样间隔（微秒）
    interval_us: u64,
    /// 停止标志（主线程设为 false 时，子线程退出）
    running: Arc<AtomicBool>,
    /// 写入请求队列（主线程推送，提交线程消费）
    /// 元素: (变量索引, 原始位模式)
    write_queue: Arc<Mutex<VecDeque<(usize, u64)>>>,
    /// 各变量最近一次读回的**原始对齐整字**（按 32 位字展开）。
    ///
    /// 提交线程对子字变量做读-改-写时用它合并变量的字节 lane。
    /// 注意必须存“原始整字”而不是 lane 提取后的值：u16@+2 读回
    /// `0x2222_1111`，若存提取后的 `0x2222`，写回时会把相邻变量的
    /// 低半字覆盖成 0。
    last_words: Arc<Mutex<Vec<Option<[u32; 3]>>>>,
    /// 每条已发出命令的**实际写请求数**（提交线程入队、接收线程出队）。
    ///
    /// 只统计真正编码进命令的写入：被跳过的写入（未采样到旧值、未对齐
    /// 等）不占请求，若仍按队列长度计，切帧会把这些请求误当成读请求。
    writes_per_cmd: Arc<Mutex<VecDeque<u8>>>,
    /// 在途命令计数（提交线程发出命令后 +1，接收线程解析对应响应后 -1）。
    /// 背压控制：提交线程在途数达到 MAX_IN_FLIGHT 时跳过本轮
    in_flight: Arc<AtomicUsize>,
}

impl PipelineEngine {
    /// 从已初始化的 SWD 链路创建流水线引擎
    ///
    /// `usb` 和 `dap` 来自 `SwdLink::into_parts()`。
    /// `addresses` 是一组目标内存地址（1-[`MAX_VALUES`] 个，启动时经
    /// [`validate_config`] 校验），`types` 与之一一对应
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
        let read_layout = build_read_layout(&accesses);
        Self {
            usb,
            dap,
            accesses,
            read_layout,
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
            read_layout: self.read_layout.clone(),
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
        // 配置校验（变量数 / 读块请求预算）：超限必须显式报错，
        // 而不是静默截断采样值或损坏 USB 命令
        validate_config(&self.accesses)?;

        // 重置停止标志（上一次 stop 可能已将其置为 false）
        self.running.store(true, Ordering::SeqCst);
        // 复位在途计数器
        self.in_flight.store(0, Ordering::SeqCst);
        // 清空写入数队列（上一次运行的残留条目会污染本会话的切帧）
        if let Ok(mut q) = self.writes_per_cmd.lock() {
            q.clear();
        }
        // 清零旧值缓存：上一次运行的采样值可能已过期（目标重新烧录、
        // 变量地址/类型变化），拿它做读-改-写会写坏相邻变量。
        // 写入端在缓存为空时会跳过并告警，等本轮采样填好后再写。
        if let Ok(mut lw) = self.last_words.lock() {
            lw.clear();
        }
        // 排空 USB IN 端点残留响应（上一次运行的命令响应可能仍留在
        // 端点缓冲中，同一设备被快速 Stop→Start 重连时尤其如此）。
        // 否则新会话会把旧响应当新数据消费，且写入数队列与响应失去
        // 对应关系，切帧从此错位。
        self.usb.flush_input();

        let ring = Arc::new(RingBuffer::new(capacity));
        let start_time = Instant::now();

        // 子线程致命错误槽（USB 写失败、设备拔出等）。此前子线程直接
        // break 退出且无任何上报通道，UI 会永远显示 Running（原缺陷 ⑨）。
        // UI 每帧轮询 thread_error() / threads_alive() 触发自动停止。
        let thread_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // 非致命故障通知（写入被跳过、协议失步丢数据等）：只写日志用户
        // 无感知，UI 每帧 drain 后转为错误弹窗
        let notices: Arc<Mutex<Vec<PipelineNotice>>> = Arc::new(Mutex::new(Vec::new()));

        // 时间戳完全由接收线程的 seq 计算：
        //   timestamp = seq * interval_us / 1_000_000
        // 这是理想网格时间戳，不受 USB 写入/响应抖动影响，
        // 保证显示的采样间隔严格一致。无需跨线程队列传递。

        let submit = self.spawn_submit_thread(
            start_time,
            Arc::clone(&thread_error),
            Arc::clone(&notices),
        )?;
        let collect = self.spawn_collect_thread(
            Arc::clone(&ring),
            Arc::clone(&thread_error),
            Arc::clone(&notices),
        )?;

        Ok(PipelineHandle {
            submit_handle: submit,
            collect_handle: collect,
            ring_buffer: ring,
            running: Arc::clone(&self.running),
            write_queue: Arc::clone(&self.write_queue),
            thread_error,
            notices,
        })
    }

    /// 启动提交线程
    fn spawn_submit_thread(
        &self,
        start_time: Instant,
        thread_error: Arc<Mutex<Option<String>>>,
        notices: Arc<Mutex<Vec<PipelineNotice>>>,
    ) -> Result<JoinHandle<()>> {
        let usb = Arc::clone(&self.usb);
        let interval_us = self.interval_us;
        let accesses = self.accesses.clone();
        let running = Arc::clone(&self.running);
        let write_queue = Arc::clone(&self.write_queue);
        let last_words = Arc::clone(&self.last_words);
        let writes_per_cmd = Arc::clone(&self.writes_per_cmd);
        let in_flight = Arc::clone(&self.in_flight);

        // 预构建读取块请求（每次采样都一样）
        //
        // 每个 32 位字单独发一组 [写 TAR, 读 DRW]：不依赖 AHB-AP 的
        // CSW.AddrInc 位自增。CSW 的复位值由目标与探针固件决定，本程序
        // 从未配置过它；若 AddrInc 未使能，连续两次 DRW 访问会命中同一
        // 地址，64 位变量的高字将恒等于低字（且不会产生任何错误，属于
        // 静默错误）。多写一次 TAR 的代价远小于数据错误的代价。
        // 子字变量（u8/u16）写对齐字地址，读回整字后按 lane 提取。
        // AHB-AP 不支持非对齐访问：对非字对齐地址发起 32 位 DRW 读行为
        // 不可预测（实际芯片通常回退到对齐字），必须对齐后 lane 提取。
        let requests: Vec<TransferRequest> = accesses
            .iter()
            .flat_map(|acc| {
                (0..acc.words).flat_map(move |i| {
                    let addr = acc.aligned_addr.wrapping_add(4 * u32::from(i));
                    [
                        TransferRequest::write_ap(AP_REG_TAR, addr),
                        TransferRequest::read_ap(AP_REG_DRW),
                    ]
                })
            })
            .collect();

        let dap_index = self.dap.dap_index;

        // 预构建读取请求字节（固定部分，每次采样都一样）
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
                    // 每次写入最多占 4 条请求（64 位：TAR+DRW 各两次）
                    let max_writes = MAX_TRANSFER_REQUESTS
                        .saturating_sub(requests.len())
                        / 4;
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
                    // 实际编码进命令的写请求数（TAR/DRW 各算一条）。
                    // 切帧必须用这个值：被跳过的写入不占请求。
                    let mut write_reqs = 0usize;
                    for &(var_idx, value) in &pending_writes {
                        let Some(acc) = accesses.get(var_idx) else {
                            log::warn!("write request skipped: variable index {} has no aligned-access info", var_idx);
                            push_notice(
                                &notices,
                                PipelineNotice::WriteSkipped {
                                    var_idx,
                                    reason: "no aligned-access info".to_string(),
                                },
                            );
                            continue;
                        };

                        if acc.words > 1 {
                            // 64 位变量：AHB-AP 无 64 位访问，拆成低/高两个
                            // 32 位字写。只有 4 字节对齐（变量独占完整的
                            // 两个整字）才能安全写入；非 4 字节对齐时首尾字
                            // 与相邻变量共享，需要跨字读-改-写，直接拒绝。
                            if acc.aligned_addr & 3 != 0 {
                                log::warn!(
                                    "write request skipped: 8-byte data of variable {} is not 4-byte aligned (0x{:08X})",
                                    var_idx, acc.aligned_addr
                                );
                                push_notice(
                                    &notices,
                                    PipelineNotice::WriteSkipped {
                                        var_idx,
                                        reason: format!(
                                            "8-byte data is not 4-byte aligned (0x{:08X})",
                                            acc.aligned_addr
                                        ),
                                    },
                                );
                                continue;
                            }
                            let (low, high) = MemAccess::split_wide(value);
                            // 每个字写前都重写 TAR：与读取路径同理，不依赖
                            // CSW.AddrInc 自增
                            for (i, word) in [low, high].into_iter().enumerate() {
                                let addr = acc.aligned_addr.wrapping_add(4 * i as u32);
                                cmd.push(TransferRequest::write_ap(AP_REG_TAR, addr).request_byte());
                                cmd.extend_from_slice(&addr.to_le_bytes());
                                cmd.push(TransferRequest::write_ap(AP_REG_DRW, word).request_byte());
                                cmd.extend_from_slice(&word.to_le_bytes());
                            }
                            n_requests += 4;
                            write_reqs += 4;
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
                            write_reqs += 2;
                            continue;
                        }

                        // 子字变量（u8/u16）：用最近一次采样的**原始整字**
                        // 合并，只覆盖本变量 lane，不踩相邻变量
                        let last = words_guard
                            .as_ref()
                            .and_then(|w| w.get(var_idx))
                            .copied()
                            .flatten();
                        let Some(old) = last else {
                            log::warn!(
                                "write request skipped: variable {} has no sampled old value for read-modify-write",
                                var_idx
                            );
                            push_notice(
                                &notices,
                                PipelineNotice::WriteSkipped {
                                    var_idx,
                                    reason: "no sampled old value yet (write again after the first sample arrives)".to_string(),
                                },
                            );
                            continue;
                        };
                        let write_data = acc.merge(old[0], value as u32);
                        let w_tar = TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr);
                        let w_drw = TransferRequest::write_ap(AP_REG_DRW, write_data);
                        cmd.push(w_tar.request_byte());
                        cmd.extend_from_slice(&acc.aligned_addr.to_le_bytes());
                        cmd.push(w_drw.request_byte());
                        cmd.extend_from_slice(&write_data.to_le_bytes());
                        n_requests += 2;
                        write_reqs += 2;
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
                            "submit thread injecting {} write requests (seq={})",
                            pending_writes.len(), seq
                        );
                    }
                    match usb.write_nonblock(&cmd) {
                        Ok(n) if n == cmd.len() => {} // 发送成功
                        Ok(n) => {
                            // 短写：只有部分命令字节到达设备，设备按不完整
                            // 命令解析，响应流必然失步。按致命错误处理并
                            // 上报 UI（此前短写被当作成功，协议从此错乱）。
                            let msg = format!(
                                "USB short write: {}/{} bytes",
                                n, cmd.len()
                            );
                            log::error!("submit thread {} (seq={}), exiting", msg, seq);
                            if running.load(Ordering::Relaxed) {
                                if let Ok(mut slot) = thread_error.lock() {
                                    slot.get_or_insert(msg);
                                }
                            }
                            break;
                        }
                        Err(Error::Usb(rusb::Error::Timeout)) => {
                            // 写超时（0 字节发出）：设备忙或已拔出。本周期
                            // 没有命令发出、也就没有对应响应，直接跳过；
                            // 未消费的写入请求重新入队。单次超时不应终止
                            // 采集（此前任何写错误都会让线程静默退出）。
                            if !running.load(Ordering::Relaxed) {
                                break;
                            }
                            log::warn!("submit thread write timeout (seq={}), skipping this cycle", seq);
                            if !pending_writes.is_empty() {
                                if let Ok(mut wq) = write_queue.lock() {
                                    for (idx, v) in pending_writes.iter().rev() {
                                        wq.push_front((*idx, *v));
                                    }
                                }
                            }
                            seq += 1;
                            let target = start + Duration::from_micros(interval_us * seq);
                            let now = Instant::now();
                            if now < target {
                                spin_sleep::sleep(target - now);
                            }
                            continue;
                        }
                        Err(e) => {
                            if !running.load(Ordering::Relaxed) {
                                // 停止中遇到的超时/错误，正常退出
                                log::info!("submit thread received stop signal, exiting");
                                break;
                            }
                            // 致命错误（设备拔出、管道错误等）：上报 UI
                            // 后退出，不再无声死亡（原缺陷 ⑨）
                            let msg = format!("USB write failed: {}", e);
                            log::error!("submit thread USB write failed (seq={}): {}, exiting", seq, e);
                            if let Ok(mut slot) = thread_error.lock() {
                                slot.get_or_insert(msg);
                            }
                            break;
                        }
                    }
                    // 命令已发出：记录本条命令**实际**的写请求数（接收线程
                    // 按它计算响应长度），在途 +1。必须用 write_reqs 而不是
                    // pending_writes.len()——被跳过的写入（未采样到旧值、
                    // 地址未对齐）没有编码进命令，若按队列长度计，切帧会把
                    // 这些不存在的请求当成读请求处理。
                    if let Ok(mut q) = writes_per_cmd.lock() {
                        q.push_back(write_reqs as u8);
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

                log::info!("submit thread exited, {} requests submitted in total", seq);
            })
            .map_err(|e| Error::PipelineThread(e.to_string()))?;

        Ok(handle)
    }

    /// 启动接收线程
    fn spawn_collect_thread(
        &self,
        ring: Arc<RingBuffer>,
        thread_error: Arc<Mutex<Option<String>>>,
        notices: Arc<Mutex<Vec<PipelineNotice>>>,
    ) -> Result<JoinHandle<()>> {
        let usb = Arc::clone(&self.usb);
        let running = Arc::clone(&self.running);
        let accesses = self.accesses.clone();
        let num_vars = accesses.len();
        let read_layout = self.read_layout.clone();
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
                // 致命错误日志限频：拔线后每次读都会立即失败，不限频会
                // 每秒打出数十万条 warn 刷爆日志文件（原缺陷 ⑩）
                let mut last_err_log: Option<Instant> = None;

                while running.load(Ordering::Relaxed) {
                    // 读取 USB 响应（超时 200ms，允许优雅退出）
                    let n = match usb.read_timeout(&mut buf, Duration::from_millis(200)) {
                        Ok(n) => n,
                        Err(Error::Usb(rusb::Error::Timeout)) => {
                            // rusb 对 0 字节超时返回 Err(Timeout)：设备暂无
                            // 响应的例行空转，静默继续（read_timeout 阻塞
                            // 至多 200ms，天然限速）
                            continue;
                        }
                        Err(e) => {
                            // 设备拔出 / 管道错误等致命错误：上报 UI（触发
                            // 自动停止并提示）、限频日志、短暂休眠避免忙转。
                            // 循环继续检查 running 标志以便优雅退出。
                            if running.load(Ordering::Relaxed) {
                                if let Ok(mut slot) = thread_error.lock() {
                                    slot.get_or_insert(format!("USB read failed: {}", e));
                                }
                                let now = Instant::now();
                                if last_err_log
                                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
                                {
                                    log::warn!("collect thread USB read failed (acquisition will stop): {}", e);
                                    last_err_log = Some(now);
                                }
                                thread::sleep(Duration::from_millis(50));
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
                                "collect thread protocol desync: response header 0x{:02X} is not DAP_Transfer, dropping {} bytes to resync",
                                data[offset], data.len() - offset
                            );
                            push_notice(
                                &notices,
                                PipelineNotice::DataDropped {
                                    message: format!(
                                        "Protocol desync: dropped {} bytes of responses and resynchronized",
                                        data.len() - offset
                                    ),
                                },
                            );
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        }
                        let count = data[offset + 1] as usize;
                        let status = data[offset + 2];
                        // 本条命令的写请求数：提交线程每发一条命令入队一条。
                        // 只 peek——响应可能跨多次 USB 读，完整消费后才出队。
                        let write_reqs = write_counts_front(&writes_per_cmd);
                        let Some(write_reqs) = write_reqs else {
                            log::error!("collect thread write-count queue empty, dropping {} bytes to resync", data.len() - offset);
                            push_notice(
                                &notices,
                                PipelineNotice::DataDropped {
                                    message: format!(
                                        "Protocol desync: dropped {} bytes of responses and resynchronized",
                                        data.len() - offset
                                    ),
                                },
                            );
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        };
                        // 请求总数上限 = 写请求 + 读取块请求数
                        let total_requests = write_reqs + read_layout.len();
                        if count > total_requests {
                            log::error!(
                                "collect thread protocol desync: count={} exceeds command request count {}, dropping {} bytes to resync",
                                count, total_requests, data.len() - offset
                            );
                            push_notice(
                                &notices,
                                PipelineNotice::DataDropped {
                                    message: format!(
                                        "Protocol desync: dropped {} bytes of responses and resynchronized",
                                        data.len() - offset
                                    ),
                                },
                            );
                            offset = data.len();
                            if let Ok(mut q) = writes_per_cmd.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            break;
                        }
                        let need = transfer_frame_len(count, status, write_reqs, &read_layout);
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
                                log::error!("collect thread parse failed (seq={}): {}", seq, e);
                                push_notice(
                                    &notices,
                                    PipelineNotice::DataDropped {
                                        message: format!(
                                            "Failed to parse a response (seq={}): {}, data dropped",
                                            seq, e
                                        ),
                                    },
                                );
                                seq += 1;
                                continue;
                            }
                        };

                        if resp.status != TRANSFER_OK {
                            log::warn!(
                                "collect thread got non-OK status (seq={}): status={}, count={}",
                                seq, resp.status, resp.count
                            );
                            seq += 1;
                            continue;
                        }

                        // 时间戳完全由 seq 计算（理想网格时间戳），
                        // 不受 USB 写入/响应抖动影响，保证显示间隔严格一致。
                        let timestamp_sec = (interval_us * seq) as f64 / 1_000_000.0;

                        // 响应的数据区按“读请求的完成顺序”排列，每个 64 位
                        // 变量占 2-3 个连续字（32 位及更窄占 1 个）。按
                        // accesses.words 顺序把读回的 32 位字归组后再拼值。
                        //
                        // 注意：FAULT/WAIT 提前中止时，只有已完成的读请求
                        // 携带数据，尾部变量缺失数据——用 0 填充并保持
                        // 通道位置对齐（不做位移，否则波形通道会串号）。
                        let mut vals = [0u64; MAX_VALUES];
                        {
                            let words = resp.values();
                            let mut wi = 0usize;
                            let mut lw_guard = last_words.lock().ok();
                            if let Some(lw) = lw_guard.as_mut() {
                                lw.resize(num_vars, None);
                            }
                            for (vi, acc) in accesses.iter().enumerate().take(MAX_VALUES) {
                                let need = acc.words as usize;
                                if wi + need > words.len() {
                                    // 数据不完整：本变量及其后的变量都缺失。
                                    // 必须 break 而不是 continue——continue 不
                                    // 推进 wi，会让后续窄变量误吃本变量残留的
                                    // 字（通道串号）。
                                    break;
                                }
                                let buf = &words[wi..wi + need];
                                wi += need;
                                vals[vi] = acc.extract_wide(buf);
                                // 旧值保存**原始对齐字**（不是 lane 提取后
                                // 的值）：子字变量写回时要做读-改-写合并
                                // 整字中的其它 lane。
                                if let Some(lw) = lw_guard.as_mut() {
                                    let mut raw = [0u32; 3];
                                    raw[..need].copy_from_slice(buf);
                                    lw[vi] = Some(raw);
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
                            "collect thread kept {} bytes of incomplete response for next parse",
                            data.len() - offset
                        );
                    }
                }

                log::info!("collect thread exited, {} samples received in total", seq);
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
    /// 子线程致命错误槽（第一个错误的描述）。Some = 采集无法继续
    thread_error: Arc<Mutex<Option<String>>>,
    /// 非致命故障通知队列（写入被跳过、协议失步丢数据）
    notices: Arc<Mutex<Vec<PipelineNotice>>>,
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

    /// 子线程致命错误（USB 写失败、设备拔出等）
    ///
    /// Some = 采集无法继续，UI 应停止采集并向用户展示原因。
    pub fn thread_error(&self) -> Option<String> {
        self.thread_error.lock().ok().and_then(|s| s.clone())
    }

    /// 取走全部非致命故障通知（写入被跳过、协议失步丢数据等）
    ///
    /// UI 每帧轮询，转为错误弹窗提示——这些故障此前只写日志，
    /// 用户视线不在底部提示上时完全无感知。
    pub fn drain_notices(&self) -> Vec<PipelineNotice> {
        match self.notices.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }

    /// 两个工作线程是否都仍在运行
    ///
    /// 子线程异常退出（panic、无法恢复的错误）的兜底检测：即便没有
    /// 走 thread_error 通道，UI 也能发现「线程没了但状态还是 Running」。
    pub fn threads_alive(&self) -> bool {
        !self.submit_handle.is_finished() && !self.collect_handle.is_finished()
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
        log::info!("waiting for submit thread to exit...");
        let _ = self.submit_handle.join();
        log::info!("submit thread exited");
        log::info!("waiting for collect thread to exit...");
        let _ = self.collect_handle.join();
        log::info!("collect thread exited");
        log::info!("pipeline stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按类型构造读取块布局（与 PipelineEngine::new 内部一致）
    fn layout_for(types: &[ValueType]) -> Vec<bool> {
        let accesses: Vec<MemAccess> = types
            .iter()
            .map(|t| MemAccess::from_addr_size(0, t.byte_size()))
            .collect();
        build_read_layout(&accesses)
    }

    /// DAPLink posted-read 参考模型：独立于被测实现，逐请求模拟
    /// （任何一处实现偏离都会被交叉验证抓到）
    fn reference_frame_len(count: usize, status: u8, write_reqs: usize, layout: &[bool]) -> usize {
        let mut words = 0usize;
        let mut posted = false;
        let step = |is_read: bool, words: &mut usize, posted: &mut bool| {
            if is_read {
                if *posted { *words += 1; }
                *posted = true;
            } else if *posted {
                *words += 1;
                *posted = false;
            }
        };
        for _ in 0..write_reqs.min(count) {
            step(false, &mut words, &mut posted);
        }
        for &is_read in layout.iter().take(count.saturating_sub(write_reqs)) {
            step(is_read, &mut words, &mut posted);
        }
        if status == TRANSFER_OK && posted {
            words += 1;
        }
        3 + 4 * words
    }

    #[test]
    fn frame_len_full_ok_response() {
        let layout = layout_for(&[ValueType::Float; 4]);
        // 全成功：4 变量 × [TAR, DRW] = 8 请求，各返回 1 字
        assert_eq!(transfer_frame_len(8, TRANSFER_OK, 0, &layout), 3 + 16);
        // 1 条 32 位写入（TAR + DRW = 2 请求）+ 4 变量
        assert_eq!(transfer_frame_len(10, TRANSFER_OK, 2, &layout), 3 + 16);
    }

    #[test]
    fn frame_len_fault_mid_sequence() {
        let layout = layout_for(&[ValueType::Float; 4]);
        // 请求序列: [WTAR,WDRW] + [TAR0,DRW0,TAR1,DRW1,TAR2,DRW2,TAR3,DRW3]
        // 第 5 条请求（TAR1）处 FAULT：写块已 drain 一次，DRW0 挂起后
        // 在 TAR1 前取回 → 1 个字
        assert_eq!(transfer_frame_len(5, TRANSFER_FAULT, 2, &layout), 3 + 4);
        // 第 4 条（DRW0）处 FAULT：DRW0 挂起，但 TRY 失败→ 不返回数据
        assert_eq!(transfer_frame_len(4, TRANSFER_FAULT, 2, &layout), 3);
        // 写块内 FAULT：无读数据
        assert_eq!(transfer_frame_len(1, TRANSFER_FAULT, 2, &layout), 3);
    }

    #[test]
    fn frame_len_fault_within_var_pair() {
        let layout = layout_for(&[ValueType::Float; 4]);
        // 0 写，count=3 → TAR0,DRW0,TAR1 → DRW0 在 TAR1 前取回，1 字
        assert_eq!(transfer_frame_len(3, TRANSFER_FAULT, 0, &layout), 3 + 4);
        // OK 结束但只执行到 TAR1（异常设备）：挂起的 DRW0 由收尾补读
        assert_eq!(transfer_frame_len(3, TRANSFER_OK, 0, &layout), 3 + 4);
    }

    #[test]
    fn frame_len_64bit_reads_two_words_per_variable() {
        // 这是旧实现最严重的 bug：按 count/2 估算会把 double 的第二字漏掉
        let layout = layout_for(&[ValueType::Float64]);
        // [TAR0, DRW0, TAR1, DRW1]：DRW0 在 TAR1 前取回，DRW1 由收尾补读
        assert_eq!(layout.len(), 4);
        assert_eq!(transfer_frame_len(4, TRANSFER_OK, 0, &layout), 3 + 8);

        let layout = layout_for(&[ValueType::Float64; 8]);
        // 8 变量 × 2 字 = 16 字
        assert_eq!(layout.len(), 32);
        assert_eq!(transfer_frame_len(32, TRANSFER_OK, 0, &layout), 3 + 64);
    }

    #[test]
    fn frame_len_mixed_width_layout() {
        use ValueType::*;
        let layout = layout_for(&[Float64, Float, Uint16]);
        // 3 变量：double 2 字 + float 1 字 + u16 1 字 = 4 字
        assert_eq!(layout.len(), 4 + 2 + 2);
        assert_eq!(transfer_frame_len(layout.len(), TRANSFER_OK, 0, &layout), 3 + 16);
    }

    #[test]
    fn frame_len_matches_reference_model_exhaustively() {
        use ValueType::*;
        // 覆盖各种通道宽度组合、各种写入数、各种中止点与状态：
        // 被测实现必须与独立参考模型逐字节一致
        let type_sets: [&[ValueType]; 6] = [
            &[Float; 4],
            &[Float64],
            &[Float64; 8],
            &[Float64, Float, Uint16],
            &[Uint64; 3],
            &[Int8, Float64, Int16, Uint32],
        ];
        for types in type_sets {
            let layout = layout_for(types);
            let total_reads = layout.len();
            for write_reqs in [0usize, 2, 3, 4, 6] {
                for count in 0..=(write_reqs + total_reads) {
                    for status in [TRANSFER_OK, TRANSFER_FAULT, TRANSFER_WAIT] {
                        // 设备语义：OK 时必执行完所有请求；非 OK 时提前中止
                        if status == TRANSFER_OK && count != write_reqs + total_reads {
                            continue;
                        }
                        if status != TRANSFER_OK && count == write_reqs + total_reads {
                            continue;
                        }
                        assert_eq!(
                            transfer_frame_len(count, status, write_reqs, &layout),
                            reference_frame_len(count, status, write_reqs, &layout),
                            "types={types:?} write_reqs={write_reqs} count={count} status={status}"
                        );
                    }
                }
            }
        }
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
        let layout = layout_for(&[ValueType::Float; 2]);
        let n_write_reqs = 2usize; // 一条 32 位写入

        // FAULT：写块(TAR,DRW) → DRW0 挂起 → TAR1 前取回 1 字 → 中止
        let mut fault = vec![DAP_TRANSFER, 5u8, TRANSFER_FAULT];
        fault.extend_from_slice(&0xAAAA_0001u32.to_le_bytes());
        // OK：2 写 + 4 读请求 = 6，2 个读数据
        let mut ok = vec![DAP_TRANSFER, (n_write_reqs + layout.len()) as u8, TRANSFER_OK];
        ok.extend_from_slice(&0xBBBB_0001u32.to_le_bytes());
        ok.extend_from_slice(&0xBBBB_0002u32.to_le_bytes());
        let mut stream = fault.clone();
        stream.extend_from_slice(&ok);

        // 按接收线程的算法切帧
        let mut offset = 0;
        let mut frames = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back(n_write_reqs as u8);
        queue.push_back(0u8); // 第二条命令无写入
        while offset < stream.len() {
            assert!(stream.len() - offset >= 3);
            assert_eq!(stream[offset], DAP_TRANSFER, "frame echo byte");
            let count = stream[offset + 1] as usize;
            let status = stream[offset + 2];
            let wr = queue.front().copied().unwrap() as usize;
            let need = transfer_frame_len(count, status, wr, &layout);
            assert!(
                stream.len() - offset >= need,
                "frame incomplete at offset {offset}"
            );
            frames.push(stream[offset..offset + need].to_vec());
            offset += need;
            queue.pop_front();
        }
        assert_eq!(frames.len(), 2, "both frames must be sliced exactly");
        assert_eq!(offset, stream.len(), "no trailing bytes");

        let r1 = DapProtocol::parse_transfer_response(&frames[0]).unwrap();
        assert_eq!(r1.status, TRANSFER_FAULT);
        assert_eq!(r1.count, 5);
        assert_eq!(r1.values(), &[0xAAAA_0001]);

        let r2 = DapProtocol::parse_transfer_response(&frames[1]).unwrap();
        assert_eq!(r2.status, TRANSFER_OK);
        assert_eq!(r2.values(), &[0xBBBB_0001, 0xBBBB_0002]);
    }

    // ---- 64 位类型：请求布局与响应归组 ----

    #[test]
    fn read_block_scales_with_type_width() {
        use ValueType::*;
        let plain: Vec<MemAccess> =
            [Float; 4].iter().map(|t| MemAccess::from_addr_size(0, t.byte_size())).collect();
        assert_eq!(read_block_requests(&plain), 8);
        // 一个 double 占两组 [TAR,DRW]（2 字），其余 3 个 float 各一组
        let mixed: Vec<MemAccess> =
            [Float64, Float, Float, Float].iter().map(|t| MemAccess::from_addr_size(0, t.byte_size())).collect();
        assert_eq!(read_block_requests(&mixed), 2 * 2 + 3 * 2);
    }

    #[test]
    fn validate_config_accepts_budget_fitting_configs() {
        // 16 个 32 位变量 = 32 条请求；15 个 64 位变量 = 60 条请求，
        // 恰好用满读块预算（64 - 4 条写预留）且留出每周期一条宽写入
        let narrow: Vec<MemAccess> = (0..MAX_VALUES as u32)
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 4, 4))
            .collect();
        assert_eq!(read_block_requests(&narrow), 32);
        assert!(validate_config(&narrow).is_ok());

        let wide: Vec<MemAccess> = (0..15u32)
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 8, 8))
            .collect();
        assert_eq!(read_block_requests(&wide), 60);
        assert!(validate_config(&wide).is_ok());
    }

    #[test]
    fn validate_config_rejects_too_many_variables() {
        // 17 个变量超过 MAX_VALUES：Sample 定长槽会静默截断，必须显式报错
        let accesses: Vec<MemAccess> = (0..=(MAX_VALUES as u32))
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 4, 4))
            .collect();
        let err = validate_config(&accesses).unwrap_err().to_string();
        assert!(
            err.contains("Too many sampling variables")
                && err.contains(&MAX_VALUES.to_string()),
            "错误信息应说明变量数上限，实际 {err:?}"
        );
    }

    #[test]
    fn validate_config_rejects_read_block_eating_write_budget() {
        // 16 个 64 位变量 = 64 条请求：读块吃满 64 条会把每周期写预算
        // 压到 0，交互式写入永远饿死——必须显式报错（读块上限实为 60 条）
        let accesses: Vec<MemAccess> = (0..16u32)
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 8, 8))
            .collect();
        assert_eq!(read_block_requests(&accesses), 64);
        let err = validate_config(&accesses).unwrap_err().to_string();
        assert!(
            err.contains("exceeding the") && err.contains("interactive writes"),
            "错误信息应说明请求预算与写预留，实际 {err:?}"
        );
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
        let words = [lo, hi];

        let mut vals = [0u64; 2];
        let mut wi = 0usize;
        for (vi, acc) in accesses.iter().enumerate() {
            let need = acc.words as usize;
            if wi + need > words.len() {
                break; // 数据不完整：本变量及后续变量都缺失
            }
            vals[vi] = acc.extract_wide(&words[wi..wi + need]);
            wi += need;
        }
        assert_eq!(ValueType::Float64.to_f64(vals[0]), 1.5);
        // 通道 1 无数据但位置保持（值为 0），不是把通道 2 的数据填到通道 1
        assert_eq!(vals[1], 0u64);
    }

    #[test]
    fn truncated_wide_variable_does_not_shift_narrow_channel() {
        // 回归：旧实现用 continue 不推进游标，导致 double 缺失时
        // 后面的 u16 误吃了 double 残留的字（通道串号）
        let accesses = [
            MemAccess::from_addr_size(0x2000_0000, 8), // 需要 2 字
            MemAccess::from_addr_size(0x2000_0010, 2), // 需要 1 字
        ];
        // 只回了 1 个字：double 不完整，u16 也拿不到数据
        let words = [0x1111_2222u32];
        let mut vals = [0u64; 2];
        let mut wi = 0usize;
        for (vi, acc) in accesses.iter().enumerate() {
            let need = acc.words as usize;
            if wi + need > words.len() {
                break;
            }
            vals[vi] = acc.extract_wide(&words[wi..wi + need]);
            wi += need;
        }
        assert_eq!(vals[0], 0, "incomplete double must stay 0");
        assert_eq!(vals[1], 0, "u16 must not consume the double's leftover word");
    }

    #[test]
    fn parse_rejects_wrong_echo() {
        // 字节流失步时响应头不是 0x05：必须显式报错而非解析出垃圾数据
        let bad = vec![0x06u8, 2, 1, 0, 0, 0, 0];
        assert!(DapProtocol::parse_transfer_response(&bad).is_err());
    }

    #[test]
    fn pipeline_handle_exposes_thread_health_and_error() {
        // UI 依赖两个信号做兜底检测（原缺陷 ⑨：线程死了 UI 仍显示 Running）：
        // thread_error（致命错误槽）与 threads_alive（is_finished 兜底）
        use std::sync::mpsc;

        let (tx1, rx1) = mpsc::channel::<()>();
        let (tx2, rx2) = mpsc::channel::<()>();
        let j1 = thread::spawn(move || {
            let _ = rx1.recv();
        });
        let j2 = thread::spawn(move || {
            let _ = rx2.recv();
        });
        let te: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let notices: Arc<Mutex<Vec<PipelineNotice>>> = Arc::new(Mutex::new(Vec::new()));
        let handle = PipelineHandle {
            submit_handle: j1,
            collect_handle: j2,
            ring_buffer: Arc::new(RingBuffer::new(16)),
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::new(Mutex::new(VecDeque::new())),
            thread_error: Arc::clone(&te),
            notices: Arc::clone(&notices),
        };

        // 两个线程都阻塞在 recv 上：必须判定为存活
        assert!(handle.threads_alive(), "运行中的线程不应被判定为退出");
        assert!(handle.thread_error().is_none(), "无错误时错误槽应为空");
        assert!(handle.drain_notices().is_empty(), "无通知时队列应为空");

        // 子线程通过槽上报致命错误，UI 读取到完整描述
        *te.lock().unwrap() = Some("USB read failed: boom".to_string());
        assert_eq!(handle.thread_error().as_deref(), Some("USB read failed: boom"));

        // 非致命通知队列：push 有上限（故障风暴不刷爆 UI），drain 取走并清空
        for i in 0..20 {
            push_notice(
                &notices,
                PipelineNotice::WriteSkipped {
                    var_idx: i,
                    reason: "test".to_string(),
                },
            );
        }
        assert_eq!(notices.lock().unwrap().len(), 16, "通知队列应有上限");
        assert_eq!(handle.drain_notices().len(), 16, "drain 应取走全部通知");
        assert!(handle.drain_notices().is_empty(), "drain 后应清空");

        // 线程退出后 is_finished 兜底检测生效（未走错误通道的死法也能发现）
        drop(tx1);
        drop(tx2);
        for _ in 0..100 {
            if !handle.threads_alive() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!handle.threads_alive(), "线程退出后不应被判定为存活");
        // join 回收线程资源（join 消费句柄，故放在断言之后）
        let _ = handle.submit_handle.join();
        let _ = handle.collect_handle.join();
    }
}
