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
use super::sample::Sample;
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

/// 流水线采集引擎
///
/// 双线程模型：
/// - 提交线程：以固定间隔构造 DAP_Transfer 命令并非阻塞写入 USB
/// - 接收线程：从 USB 读取响应、解析采样数据、推入环形缓冲区
pub struct PipelineEngine {
    usb: Arc<BulkTransfer>,
    dap: DapProtocol,
    addresses: Vec<u32>,
    /// 目标采样间隔（微秒）
    interval_us: u64,
    /// 停止标志（主线程设为 false 时，子线程退出）
    running: Arc<AtomicBool>,
    /// 写入请求队列（主线程推送，提交线程消费）
    /// 元素: (地址, 数据)
    write_queue: Arc<Mutex<VecDeque<(u32, u32)>>>,
    /// 在途命令计数（提交线程发出命令后 +1，接收线程解析对应响应后 -1）。
    /// 背压控制：提交线程在途数达到 MAX_IN_FLIGHT 时跳过本轮
    in_flight: Arc<AtomicUsize>,
}

impl PipelineEngine {
    /// 从已初始化的 SWD 链路创建流水线引擎
    ///
    /// `usb` 和 `dap` 来自 `SwdLink::into_parts()`。
    /// `addresses` 是一组目标内存地址（1-8 个）。
    /// `rate_hz` 是目标采样率。
    pub fn new(
        usb: Arc<BulkTransfer>,
        dap: DapProtocol,
        addresses: Vec<u32>,
        rate_hz: u32,
    ) -> Self {
        let interval_us = (1_000_000.0 / rate_hz as f64) as u64;
        Self {
            usb,
            dap,
            addresses,
            interval_us,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::new(Mutex::new(VecDeque::new())),
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
            addresses: self.addresses.clone(),
            interval_us: (1_000_000.0 / rate_hz as f64) as u64,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::clone(&self.write_queue),
            // 独立计数器：新旧引擎互不干扰（背压信号各自独立）
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 启动流水线，返回控制句柄
    ///
    /// 可重复调用（Stop 后再次 Start），不消费 self。
    pub fn start(&self, capacity: usize) -> Result<PipelineHandle> {
        // 重置停止标志（上一次 stop 可能已将其置为 false）
        self.running.store(true, Ordering::SeqCst);
        // 复位在途计数器（上一次运行可能残留未消费的命令，如停止时
        // USB IN 端点中尚未读取的响应）
        self.in_flight.store(0, Ordering::SeqCst);

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
        let addresses = self.addresses.clone();
        let running = Arc::clone(&self.running);
        let write_queue = Arc::clone(&self.write_queue);
        let in_flight = Arc::clone(&self.in_flight);

        // 预构建 TransferRequest 数组（每次采样都一样）
        // 每变量 = 写 TAR + 读 DRW，共 2 条请求
        let requests: Vec<TransferRequest> = addresses
            .iter()
            .flat_map(|&addr| {
                vec![
                    TransferRequest::write_ap(AP_REG_TAR, addr),
                    TransferRequest::read_ap(AP_REG_DRW),
                ]
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
        let mut pending_writes: Vec<(u32, u32)> = Vec::with_capacity(24);
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
                    let max_writes = (MAX_TRANSFER_REQUESTS.saturating_sub(requests.len())) / 2;
                    pending_writes.clear();
                    if let Ok(mut wq) = write_queue.lock() {
                        while pending_writes.len() < max_writes {
                            if let Some((addr, data)) = wq.pop_front() {
                                pending_writes.push((addr, data));
                            } else {
                                break;
                            }
                        }
                    }

                    // 构建 request 列表：[写入请求...] + [读取请求...]
                    // 写入: write TAR + write DRW (2 requests, 无返回数据)
                    // 读取: write TAR + read DRW (2 requests, 1 返回数据)
                    let total_requests = requests.len() + pending_writes.len() * 2;
                    // 安全断言：total_requests 必须在 u8 范围内
                    debug_assert!(total_requests <= MAX_TRANSFER_REQUESTS);

                    cmd.clear();
                    // 头部（total_requests 含本次写入）
                    cmd.push(DAP_TRANSFER);
                    cmd.push(dap_index);
                    cmd.push(total_requests as u8);

                    // 先写入请求
                    for &(addr, data) in &pending_writes {
                        let w_tar = TransferRequest::write_ap(AP_REG_TAR, addr);
                        let w_drw = TransferRequest::write_ap(AP_REG_DRW, data);
                        cmd.push(w_tar.request_byte());
                        cmd.extend_from_slice(&addr.to_le_bytes());
                        cmd.push(w_drw.request_byte());
                        cmd.extend_from_slice(&data.to_le_bytes());
                    }

                    // 再读取请求（复用预构建字节，跳过头部）
                    cmd.extend_from_slice(&read_part[3..]);

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
                    // 命令已发出：在途 +1（接收线程解析对应响应后 -1）
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
        let addresses = self.addresses.clone();
        let num_vars = addresses.len();
        let interval_us = self.interval_us;
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

                // 每个 DAP_Transfer 响应的固定长度：
                // [DAP_TRANSFER, count, status, data...] = 3 + 4*num_vars 字节
                // 每变量一个 u32 读数据（写 TAR 不返回数据）
                let resp_len = 3 + 4 * num_vars;

                while running.load(Ordering::Relaxed) {
                    // 读取 USB 响应（超时 200ms，允许优雅退出）
                    let n = match usb.read_timeout(&mut buf, Duration::from_millis(200)) {
                        Ok(n) => n,
                        Err(e) => {
                            // 超时可能只是暂无数据，检查运行标志后继续
                            if running.load(Ordering::Relaxed) {
                                log::warn!("接收线程读超时: {}", e);
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

                    // 一次 read_bulk 可能读到多个响应（USB Bulk 合并短包），
                    // 按 resp_len 循环解析所有完整响应
                    let mut offset = 0;
                    while offset + resp_len <= data.len() {
                        let chunk = &data[offset..offset + resp_len];
                        offset += resp_len;

                        // 解析单个响应
                        let resp = match DapProtocol::parse_transfer_response(chunk) {
                            Ok(r) => r,
                            Err(e) => {
                                log::error!("接收线程解析失败 (seq={}): {}", seq, e);
                                // 该命令的响应已被消费（无论解析是否成功），在途 -1
                                in_flight.fetch_sub(1, Ordering::SeqCst);
                                seq += 1;
                                continue;
                            }
                        };
                        // 在途 -1：每条发出的命令都会收到一条响应（含非 OK 状态），
                        // 在此统一递减，避免提交线程因计数泄漏而停摆
                        in_flight.fetch_sub(1, Ordering::SeqCst);

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

                        // 定长数组写入：零堆分配
                        let sample = Sample::new(seq, timestamp_sec, &resp.data[..resp.len]);

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

/// 流水线控制句柄
///
/// 主线程通过此句柄消费环形缓冲区中的采样数据。
pub struct PipelineHandle {
    submit_handle: JoinHandle<()>,
    collect_handle: JoinHandle<()>,
    ring_buffer: Arc<RingBuffer>,
    running: Arc<AtomicBool>,
    write_queue: Arc<Mutex<VecDeque<(u32, u32)>>>,
}

impl PipelineHandle {
    /// 推入一个写入请求（地址 + 数据）
    ///
    /// 提交线程会在下一个采样周期将此写入操作与读取操作合并为一条
    /// DAP_Transfer 命令，不影响正常采样。
    pub fn queue_write(&self, address: u32, data: u32) {
        if let Ok(mut wq) = self.write_queue.lock() {
            wq.push_back((address, data));
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
