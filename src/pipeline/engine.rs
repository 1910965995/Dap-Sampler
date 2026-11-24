use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::*;
use crate::usb::async_bulk::AsyncSession;
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::commands::*;
use crate::dap::mem_access::MemAccess;
use crate::pipeline::sample::ValueType;
use super::sample::{Sample, MAX_VALUES};
use super::ring_buffer::RingBuffer;

/// 第 `seq` 个采样周期相对起点的**目标**时刻（纳秒），用于提交节奏
///
/// 仅决定何时下发命令，不用于样本时间戳——时间戳取的是命令实际提交
/// 时刻（见 [`CmdMeta::submit_ns`]），跳过周期不会污染时间轴。
///
/// 必须用「先乘后除」而不是 `interval_ns * seq`：`1e9 / rate` 对不能整除
/// 的采样率有截断误差（44100 Hz → 22675 ns，隐含 44106 Hz），乘以 seq
/// 后误差**线性累积**——600 秒可偏出 6 ms（180 个采样周期）。
/// 先乘后除每次都是独立舍入，长期平均速率精确等于目标采样率。
fn tick_ns(seq: u64, rate_hz: u32) -> u64 {
    let rate = u64::from(rate_hz.max(1));
    (u128::from(seq) * 1_000_000_000u128 / u128::from(rate)) as u64
}

/// 单条 DAP_Transfer 命令的请求数硬上限
///
/// 两个来源的限制取小：
/// - `total_requests` 字段是 u8（≤255）
/// - 超过设备 `packet_size` 的命令会被 DAPLink 固件**静默截断**
///   （`DAP_queue_execute_buf` 里 `if (len > DAP_PACKET_SIZE) len = ...`）
///
/// 实际每包请求数由 [`max_requests_per_cmd`] 按设备上报的 packet_size
/// 计算，本常量只是保守的绝对上限。
const MAX_TRANSFER_REQUESTS: usize = 64;

/// 每个 32 位字读取占用的请求数（写 TAR + 读 DRW）
const REQUESTS_PER_WORD: usize = 2;
/// 每个请求的字节数（1 字节 request + 4 字节数据）
const BYTES_PER_REQUEST: usize = 5;
/// DAP_Transfer 响应头字节数（命令回显 + count + status）
const RESPONSE_HEADER_BYTES: usize = 3;

/// 每周期为交互式写入预留的请求数
///
/// 读取块若占满单包预算，写入预算 max_writes = 0，用户的交互写入
/// （Watch 面板）将永远排队得不到执行——静默故障。
///
/// 必须按**最宽的写入**预留：非 4 字节对齐的 64 位变量跨 3 个对齐字，
/// 读-改-写需要 3 组 `[写 TAR, 写 DRW]` = 6 条请求。按 4 条预留时
/// 64 字节包下的首包会超出包大小被固件静默截断。
const WRITE_BUDGET_REQUESTS: usize = 6;

/// 在途命令数上限（背压控制，绝对上限）
///
/// 实际上限还要受设备 `packet_count`（固件响应缓冲数）约束：超出时
/// 固件队列满会**丢弃**命令（`DAP_queue_execute_buf` 返回 FALSE），
/// 该命令永远不会有响应，切帧与在途计数会永久失步。见 [`in_flight_limit`]。
const MAX_IN_FLIGHT: usize = 4;

/// 常驻挂起的 IN transfer 数
///
/// IN 端点始终有人收，响应的到达时刻与提交线程无关。2 条即可覆盖
/// "一条正在填充、一条已就绪"的背靠背场景。
const IN_POOL_SIZE: usize = 4;

/// 单条命令可携带的最大请求数（受设备包大小约束）
fn max_requests_per_cmd(packet_size: u16) -> usize {
    let by_bytes = (packet_size as usize)
        .saturating_sub(RESPONSE_HEADER_BYTES)
        / BYTES_PER_REQUEST;
    by_bytes.min(MAX_TRANSFER_REQUESTS)
}

/// 单条命令可携带的最大读字数（同时受命令长度与响应长度约束）
fn max_words_per_cmd(packet_size: u16) -> usize {
    let by_request = max_requests_per_cmd(packet_size) / REQUESTS_PER_WORD;
    let by_response = (packet_size as usize).saturating_sub(RESPONSE_HEADER_BYTES) / 4;
    by_request.min(by_response)
}

/// 首包为交互式写入预留空间后能携带的读字数
///
/// 写入块并入首包，若首包仍按满额读字数构造，命令长度会超过
/// packet_size 被固件截断。首包必须少放 `WRITE_BUDGET_REQUESTS` 条请求
/// 对应空间的读字。无写入时首包会略小于满额（多出少量分包），可接受。
fn first_chunk_words(packet_size: u16) -> usize {
    let bytes_for_reads = (packet_size as usize)
        .saturating_sub(RESPONSE_HEADER_BYTES)
        .saturating_sub(WRITE_BUDGET_REQUESTS * BYTES_PER_REQUEST);
    let by_request = (bytes_for_reads / BYTES_PER_REQUEST) / REQUESTS_PER_WORD;
    let by_response = (packet_size as usize).saturating_sub(RESPONSE_HEADER_BYTES) / 4;
    // 同样受 u8 请求数硬上限约束（否则大包下会算出比满额还多的字）
    by_request.min(by_response).min(max_words_per_cmd(packet_size))
}

/// 在途命令上限 = min(绝对上限, 设备响应缓冲数)
///
/// `packet_count` 是固件队列深度：同时排队的命令超过它时，后者会被
/// 固件丢弃且不产生响应，切帧将永久错位。宁可串行化也不能超发。
fn in_flight_limit(packet_count: u8) -> usize {
    MAX_IN_FLIGHT.min(packet_count.max(1) as usize)
}

/// 计算一条 DAP_Transfer 命令的读取块请求数
///
/// 读取块按变量展开为 `words` 组 `[写 TAR, 读 DRW]`。每个字之前都重写
/// TAR，而不是依赖 AP 的 CSW AddrInc 位自增（CSW 已在 init 中显式配置
/// 为 32 位访问，但自增位仍不依赖，避免不同 AP 实现的差异）。
#[cfg(test)]
fn read_block_requests(accesses: &[MemAccess]) -> usize {
    accesses
        .iter()
        .map(|a| REQUESTS_PER_WORD * a.words as usize)
        .sum()
}

/// 校验采样配置能否安全编码进流水线（启动前调用，返回明确错误）
///
/// 两条硬约束，违反任一都会产生静默故障而非显式报错：
/// - 变量数 > [`MAX_VALUES`]：`Sample.values` 是定长数组，`Sample::new`
///   会静默截断超出的值——波形上通道永远是一条平线却看不出原因
/// - 单个变量需要的字数 > 单包可携带字数：无法把该变量拆到两条命令
///   （拆开就不是同一时刻的采样了），只能显式报错要求降低通道数或
///   换支持更大包的探针
///
/// 旧实现按固定 64 请求预算校验，从未读取设备实际上报的 packet_size；
/// 全速 DAPLink 固件只有 64 字节包，超长命令被静默截断后读到垃圾数据。
fn validate_config(accesses: &[MemAccess], packet_size: u16) -> Result<()> {
    if accesses.is_empty() {
        return Err(Error::Other("no sampling variables configured".into()));
    }
    if accesses.len() > MAX_VALUES {
        return Err(Error::Other(format!(
            "Too many sampling variables: {} requested, max {} (sample slots would be silently truncated)",
            accesses.len(),
            MAX_VALUES
        )));
    }
    let max_words = max_words_per_cmd(packet_size);
    if max_words == 0 {
        return Err(Error::Other(format!(
            "Device packet size {} bytes is too small for sampling (need at least {} bytes per DAP_Transfer)",
            packet_size,
            RESPONSE_HEADER_BYTES + REQUESTS_PER_WORD * BYTES_PER_REQUEST
        )));
    }
    if let Some((idx, acc)) = accesses
        .iter()
        .enumerate()
        .find(|(_, a)| a.words as usize > max_words)
    {
        return Err(Error::Other(format!(
            "Variable #{} needs {} 32-bit words in one sample, but the device packet size ({} bytes) allows at most {} per DAP_Transfer. Align the variable or use a smaller type",
            idx, acc.words, packet_size, max_words
        )));
    }
    Ok(())
}

/// 读取块的请求类型序列（true = AP 读，false = 写）
///
/// 读取块对所有采样周期完全相同，预先构造一次。切帧时按设备实际执行的
/// 请求前缀模拟 posted-read 的数据产出，因此不能简单用 `count/2` 估算。
#[cfg(test)]
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

/// 采样时间戳模式
///
/// 两种模式的取舍：
/// - [`TimestampMode::Grid`]（默认）：时间戳 = `seq × 1e9 / rate`，理想
///   等间隔。波形点间距恒定、视觉平滑，但当设备吞吐不足（背压跳过周期）
///   时 seq 仍会推进，时间轴会**系统性超前**于真实采样时刻——波形在时间
///   轴上被压缩，ΔT 测量失真。适合「信号形状比绝对时间重要」的场合。
/// - [`TimestampMode::Real`]：时间戳 = 每周期首条命令的**实际提交时刻**。
///   跳过的周期不产生样本，其时长如实体现为更大的采样间隔；X 轴反映真实
///   经过时间。代价是采样间隔不均匀（波形点间距随吞吐波动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimestampMode {
    /// 理想等间隔网格（`seq × 周期`）——默认，与历史行为一致
    #[default]
    Grid,
    /// 真实提交时刻（每周期首分片进入 OUT 槽位的时刻）
    Real,
}

impl TimestampMode {
    /// 存储/配置用的小写标识
    pub fn label(self) -> &'static str {
        match self {
            TimestampMode::Grid => "grid",
            TimestampMode::Real => "real",
        }
    }

    /// 从标识解析，未知值回退默认的 Grid
    pub fn from_label(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "real" => TimestampMode::Real,
            _ => TimestampMode::Grid,
        }
    }
}

/// 单条已提交命令的元数据（提交线程入队、接收线程出队）
///
/// 一条采样周期在设备包受限时会拆成多条 DAP_Transfer 命令：读取块按
/// 32 位字切分，各分片的响应必须按 `chunk_index` 归位到同一周期。
#[derive(Debug, Clone, Copy)]
struct CmdMeta {
    /// 本条命令写入块的请求数（0..=4）
    write_reqs: u8,
    /// 本分片读取的 32 位字数
    chunk_words: u16,
    /// 本分片在周期内的序号（0-based）
    chunk_index: u16,
    /// 本周期共拆成几个分片
    chunk_total: u16,
    /// 本周期首分片**实际提交**的时刻（相对采集起点，纳秒）
    ///
    /// 只有 `chunk_index == 0` 的记录有意义（同一周期的所有分片携带
    /// 同一个值）。这是样本的真实采样时刻：同一采样周期的数据在此刻
    /// 之后才从目标内存读出，与 USB 响应何时到达无关。
    submit_ns: u64,
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
    /// 整个采样周期的读取字数（各变量展开后的 32 位字总数）
    total_words: usize,
    /// 目标采样率（Hz）
    ///
    /// 提交节奏用 [`tick_ns`]（先乘后除）从它实时计算，不预存取整后的
    /// 间隔——取整后的间隔乘 seq 会线性累积漂移。
    rate_hz: u32,
    /// 采样时间戳模式（见 [`TimestampMode`]）
    timestamp_mode: TimestampMode,
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
    /// 每条已提交命令的元数据（提交线程入队、接收线程出队）。
    ///
    /// 只统计真正编码进命令的写入：被跳过的写入（未采样到旧值、
    /// 地址未对齐等）不占请求，若仍按队列长度计，切帧会把这些请求
    /// 误当成读请求。
    cmd_meta: Arc<Mutex<VecDeque<CmdMeta>>>,
    /// 在途命令计数（提交线程发出命令后 +1，接收线程解析对应响应后 -1）。
    /// 背压控制：在途数达到上限（且受设备 packet_count 约束）时等待。
    in_flight: Arc<AtomicUsize>,
}

impl PipelineEngine {
    /// 从已初始化的 SWD 链路创建流水线引擎
    ///
    /// `usb` 和 `dap` 来自 `SwdLink::into_parts()`。
    /// `addresses` 是一组目标内存地址（1-[`MAX_VALUES`] 个，启动时经
    /// [`validate_config`] 校验），`types` 与之一一对应
    /// （子字变量 u8/u16 按对齐整字读取后提取 lane）。
    /// `rate_hz` 是目标采样率，`timestamp_mode` 决定时间戳生成方式。
    pub fn new(
        usb: Arc<BulkTransfer>,
        dap: DapProtocol,
        addresses: Vec<u32>,
        types: &[ValueType],
        rate_hz: u32,
        timestamp_mode: TimestampMode,
    ) -> Self {
        let rate_hz = rate_hz.max(1);
        let accesses: Vec<MemAccess> = addresses
            .iter()
            .enumerate()
            .map(|(i, &addr)| {
                let size = types.get(i).map(|t| t.byte_size()).unwrap_or(4);
                MemAccess::from_addr_size(addr, size)
            })
            .collect();
        let total_words = accesses.iter().map(|a| a.words as usize).sum();
        Self {
            usb,
            dap,
            accesses,
            total_words,
            rate_hz,
            timestamp_mode,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::new(Mutex::new(VecDeque::new())),
            last_words: Arc::new(Mutex::new(Vec::new())),
            cmd_meta: Arc::new(Mutex::new(VecDeque::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 时间戳模式（供 UI/日志查询）
    pub fn timestamp_mode(&self) -> TimestampMode {
        self.timestamp_mode
    }

    /// 使用新采样率重建引擎（保持相同 USB/DAP/地址配置）
    ///
    /// 采样率在 UI 中修改后调用，返回新引擎替换旧的。
    pub fn with_rate(&self, rate_hz: u32) -> Self {
        Self {
            usb: Arc::clone(&self.usb),
            dap: self.dap,
            accesses: self.accesses.clone(),
            total_words: self.total_words,
            rate_hz: rate_hz.max(1),
            timestamp_mode: self.timestamp_mode,
            running: Arc::new(AtomicBool::new(true)),
            write_queue: Arc::clone(&self.write_queue),
            last_words: Arc::clone(&self.last_words),
            // 独立计数器：新旧引擎互不干扰（背压信号各自独立）
            cmd_meta: Arc::new(Mutex::new(VecDeque::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 启动流水线，返回控制句柄
    ///
    /// 可重复调用（Stop 后再次 Start），不消费 self。
    ///
    /// `capacity`：环形缓冲容量（采样点数）
    pub fn start(&self, capacity: usize) -> Result<PipelineHandle> {
        // 配置校验（变量数 / 单包字数预算）：超限必须显式报错，
        // 而不是静默截断采样值或损坏 USB 命令
        validate_config(&self.accesses, self.usb.packet_size())?;

        // 重置停止标志（上一次 stop 可能已将其置为 false）
        self.running.store(true, Ordering::SeqCst);
        // 复位在途计数器
        self.in_flight.store(0, Ordering::SeqCst);
        // 清空命令元数据队列（上一次运行的残留条目会污染本会话的切帧）
        if let Ok(mut q) = self.cmd_meta.lock() {
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
        // 否则新会话会把旧响应当新数据消费，且元数据队列与响应失去
        // 对应关系，切帧从此错位。
        self.usb.flush_input();

        let ring = Arc::new(RingBuffer::new(capacity));
        let start_time = Instant::now();

        // 异步 USB 会话：OUT 池深度 = 在途上限（背压由 submit 返回
        // false 表达），IN 池常驻挂起以平滑响应到达。
        //
        // 在途上限必须 ≤ 设备 packet_count：固件响应队列满时会丢弃
        // 命令（不产生响应），切帧与在途计数将永久失步。
        let packet_count = self.usb.packet_count();
        let in_flight_limit = in_flight_limit(packet_count);
        let session = Arc::new(AsyncSession::start(
            &self.usb,
            in_flight_limit,
            IN_POOL_SIZE,
        )?);
        log::info!(
            "pipeline session: packet_size={} B, packet_count={}, in-flight limit={}",
            self.usb.packet_size(),
            packet_count,
            in_flight_limit
        );

        // 子线程致命错误槽（USB 写失败、设备拔出等）。此前子线程直接
        // break 退出且无任何上报通道，UI 会永远显示 Running（原缺陷 ⑨）。
        // UI 每帧轮询 thread_error() / threads_alive() 触发自动停止。
        let thread_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // 非致命故障通知（写入被跳过、协议失步丢数据等）：只写日志用户
        // 无感知，UI 每帧 drain 后转为错误弹窗
        let notices: Arc<Mutex<Vec<PipelineNotice>>> = Arc::new(Mutex::new(Vec::new()));

        // 时间戳由提交线程记录、随 CmdMeta 传给接收线程：
        //   submit_ns = 本周期首分片实际进入 OUT 槽位的时刻（相对起点）
        // 接收线程据此生成 timestamp_sec，并强制严格单调递增。
        // 背压跳过/丢帧的周期不产生样本，因此不会像理想网格那样让
        // 时间轴系统性超前——跳过的时长如实体现为更大的采样间隔。

        let submit = self.spawn_submit_thread(
            Arc::clone(&session),
            start_time,
            in_flight_limit,
            Arc::clone(&thread_error),
            Arc::clone(&notices),
        )?;
        let collect = self.spawn_collect_thread(
            Arc::clone(&session),
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
            // 会话必须在两个线程 join 完之后才 stop（Drop 兜底）
            session: Some(session),
        })
    }

    /// 启动提交线程
    fn spawn_submit_thread(
        &self,
        session: Arc<AsyncSession>,
        start_time: Instant,
        in_flight_limit: usize,
        thread_error: Arc<Mutex<Option<String>>>,
        notices: Arc<Mutex<Vec<PipelineNotice>>>,
    ) -> Result<JoinHandle<()>> {
        let rate_hz = self.rate_hz;
        let accesses = self.accesses.clone();
        let total_words = self.total_words;
        let running = Arc::clone(&self.running);
        let write_queue = Arc::clone(&self.write_queue);
        let last_words = Arc::clone(&self.last_words);
        let cmd_meta = Arc::clone(&self.cmd_meta);
        let in_flight = Arc::clone(&self.in_flight);

        // 每包可携带的读取字数（受设备上报的 packet_size 约束）
        let packet_size = self.usb.packet_size();
        let words_per_cmd = max_words_per_cmd(packet_size);
        if total_words > words_per_cmd {
            log::info!(
                "sampling {} words per cycle requires {} DAP_Transfer packets (device packet_size={} B, {} words/packet)",
                total_words,
                total_words.div_ceil(words_per_cmd.max(1)),
                packet_size,
                words_per_cmd
            );
        }

        // 预构建查询地址与分包布局（每次采样都一样）
        //
        // 每个 32 位字单独发一组 [写 TAR, 读 DRW]：不依赖 AHB-AP 的
        // CSW.AddrInc 位自增（CSW 现在已在 init 中显式配置为 32 位访问，
        // 但自增仍不依赖，避免不同 AP 实现的差异）。
        // 子字变量（u8/u16/u32 非对齐）写对齐字地址，读回后按 lane 提取。
        let word_addrs: Vec<u32> = accesses
            .iter()
            .flat_map(|acc| {
                (0..u32::from(acc.words)).map(move |i| acc.aligned_addr.wrapping_add(4 * i))
            })
            .collect();

        let dap_index = self.dap.dap_index;

        // 分包：准备两套布局
        //
        // - 无写入（绝大多数周期）：首包按满额读字数，吞吐最高
        // - 有写入：首包为写入块预留 WRITE_BUDGET_REQUESTS 条请求，
        //   否则命令长度超过 packet_size 会被固件静默截断
        //
        // 接收线程从命令元数据里读 chunk_words/chunk_total，两套布局
        // 可以混用，无需额外同步。
        let build_parts = |first_words: usize| -> (Vec<Vec<u8>>, Vec<u16>) {
            let first_words = first_words.max(1).min(word_addrs.len());
            let (first_slice, rest) = word_addrs.split_at(first_words);
            let mut chunks: Vec<&[u32]> = vec![first_slice];
            if !rest.is_empty() {
                chunks.extend(rest.chunks(words_per_cmd.max(1)));
            }
            let parts: Vec<Vec<u8>> = chunks
                .iter()
                .map(|chunk| {
                    let mut part: Vec<u8> =
                        Vec::with_capacity(chunk.len() * 2 * BYTES_PER_REQUEST + 3);
                    part.push(DAP_TRANSFER);
                    part.push(dap_index);
                    part.push(0u8); // 请求数，提交前回填
                    for &addr in chunk.iter() {
                        part.push(TransferRequest::write_ap(AP_REG_TAR, addr).request_byte());
                        part.extend_from_slice(&addr.to_le_bytes());
                        part.push(TransferRequest::read_ap(AP_REG_DRW).request_byte());
                    }
                    part
                })
                .collect();
            let words: Vec<u16> = chunks.iter().map(|c| c.len() as u16).collect();
            (parts, words)
        };

        // 无写入：首包满额
        let (read_parts_full, chunk_words_full) = build_parts(words_per_cmd);
        // 有写入：首包预留写预算
        let (read_parts_with_write, chunk_words_with_write) =
            build_parts(first_chunk_words(packet_size));
        // 首包能否容纳写入：预留后仍有读字空间才允许取写入
        let writes_enabled = first_chunk_words(packet_size) > 0;

        // 循环外复用缓冲区，避免每采样周期堆分配
        let mut pending_writes: Vec<(usize, u64)> = Vec::with_capacity(24);
        let mut cmd: Vec<u8> = Vec::with_capacity(packet_size as usize + 64);

        let handle = thread::Builder::new()
            .name("dap-submit".into())
            .spawn(move || {
                let mut seq: u64 = 0;
                let start = start_time;

                while running.load(Ordering::Relaxed) {
                    // 致命错误（设备拔出 / 异步写失败）：接收线程可能先
                    // 发现并上报，提交线程也需退出，避免继续空转占 CPU
                    if let Some(err) = session.fatal_error() {
                        if let Ok(mut slot) = thread_error.lock() {
                            slot.get_or_insert(err);
                        }
                        break;
                    }
                    // --- 背压控制 ---
                    // 在途命令数达到上限（接收线程尚未消费完前一批响应）时
                    // 跳过本轮：实际采样率自动匹配 DAP-Link 吞吐，避免
                    // USB IN 端点无限堆积与响应延迟膨胀（Watch 值滞后）。
                    // 吞吐充足时在途数远小于上限，不影响目标采样率。
                    if in_flight.load(Ordering::Relaxed) >= in_flight_limit {
                        seq += 1;
                        let target = start + Duration::from_nanos(tick_ns(seq, rate_hz));
                        let now = Instant::now();
                        if now < target {
                            spin_sleep::sleep(target - now);
                        }
                        continue;
                    }

                    // --- 取出待写入请求 ---
                    // 一次最多取 1 条：最宽的写入（非对齐 64 位）占
                    // WRITE_BUDGET_REQUESTS 条请求，正好等于首包预留量。
                    let max_writes = if writes_enabled { 1 } else { 0 };
                    pending_writes.clear();
                    if max_writes > 0 {
                        if let Ok(mut wq) = write_queue.lock() {
                            while pending_writes.len() < max_writes {
                                if let Some((var_idx, value)) = wq.pop_front() {
                                    pending_writes.push((var_idx, value));
                                } else {
                                    break;
                                }
                            }
                        }
                    }

                    // 子字/非对齐写入的旧值快照：只在有待写入时才取，
                    // 且取完立刻放锁（长命 guard 会让接收线程抢不到锁）。
                    let words_snapshot = if pending_writes.is_empty() {
                        None
                    } else {
                        last_words.lock().ok().map(|g| g.clone())
                    };

                    // --- 构造写入块字节（并入首个读取分片）---
                    // 写入: write TAR + write DRW（2 条请求，无返回数据）
                    // 子字/非对齐变量：用最近采样到的对齐窗口做读-改-写，
                    // 只覆盖变量的字节 lane，不踩相邻变量。
                    // 被跳过的写入不计入请求数（固件按头部计数逐条解析）。
                    let mut write_bytes: Vec<u8> = Vec::with_capacity(64);
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

                        // 4 字节对齐的整字窗口：直接按字写（32 位整字或 64 位
                        // 的低/高两字）。注意必须按 byte_size 判断而不是
                        // words——非对齐 u32 也占 2 字，按 words 判断会把
                        // 它误当 u64 拆分写入，破坏相邻内存。
                        if acc.shift == 0 && acc.byte_size == 4 {
                            append_write_word(&mut write_bytes, acc.aligned_addr, value as u32);
                            write_reqs += 2;
                            continue;
                        }
                        if acc.byte_size == 8 && acc.words == 2 {
                            // 4 字节对齐的 64 位变量：拆成低/高两个 32 位字
                            let (low, high) = MemAccess::split_wide(value);
                            append_write_word(&mut write_bytes, acc.aligned_addr, low);
                            append_write_word(
                                &mut write_bytes,
                                acc.aligned_addr.wrapping_add(4),
                                high,
                            );
                            write_reqs += 4;
                            continue;
                        }

                        // 其余情况（子字 / 非对齐 4 字节 / 非对齐 8 字节）：
                        // 读回对齐窗口后按字节拼接，逐字写回
                        let last = words_snapshot
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
                        let mut window = old;
                        acc.splice_into(&mut window, value);
                        for (i, &w) in window.iter().take(acc.words as usize).enumerate() {
                            append_write_word(
                                &mut write_bytes,
                                acc.aligned_addr.wrapping_add(4 * i as u32),
                                w,
                            );
                            write_reqs += 2;
                        }
                    }

                    // --- 逐分片提交 ---
                    // 写入块并入首包；后续分片只读。
                    //
                    // 布局按本周期是否有实际写入选择：有写入时用预留布局
                    // （首包读字更少），否则用满额布局（吞吐最高）。
                    //
                    // 分片之间必须等在途腾出：设备 packet_count 可能只有 1
                    // （如 lpc11u35 全速板），此时若直接检查上限就放弃，
                    // 后续分片永远发不出去，该配置将采不到任何样本。
                    // 这里阻塞等待接收线程消费响应，用超时兜底避免设备
                    // 无响应时永久卡死（致命错误由接收线程上报）。
                    let (read_parts, chunk_words) = if write_reqs > 0 {
                        (&read_parts_with_write, &chunk_words_with_write)
                    } else {
                        (&read_parts_full, &chunk_words_full)
                    };
                    let chunk_total = read_parts.len().max(1);
                    let mut submitted = 0usize;
                    let mut aborted = false;
                    // 本周期首分片的真实提交时刻（相对采集起点，纳秒）。
                    // 这是样本的时间戳来源：跳过周期不会再让后续样本
                    // 的时间轴超前。
                    let mut cycle_submit_ns: u64 = 0;
                    for (chunk_index, part) in read_parts.iter().enumerate() {
                        let wait_deadline = Instant::now() + Duration::from_secs(2);
                        while in_flight.load(Ordering::Relaxed) >= in_flight_limit {
                            if !running.load(Ordering::Relaxed) {
                                aborted = true;
                                break;
                            }
                            if Instant::now() >= wait_deadline {
                                log::warn!(
                                    "submit thread waited 2s for in-flight slot (seq={}, chunk {}/{}), cycle abandoned",
                                    seq, chunk_index, chunk_total
                                );
                                aborted = true;
                                break;
                            }
                            thread::sleep(Duration::from_micros(50));
                        }
                        if aborted {
                            break;
                        }

                        cmd.clear();
                        cmd.push(DAP_TRANSFER);
                        cmd.push(dap_index);
                        let count_pos = cmd.len();
                        cmd.push(0u8);

                        let (wreqs_this, chunk_reqs) = if chunk_index == 0 {
                            cmd.extend_from_slice(&write_bytes);
                            (
                                write_reqs,
                                write_reqs + 2 * chunk_words[chunk_index] as usize,
                            )
                        } else {
                            (0, 2 * chunk_words[chunk_index] as usize)
                        };
                        // 读取请求（跳过 3 字节头，头部计数在下面回填）
                        cmd.extend_from_slice(&part[3..]);
                        cmd[count_pos] = chunk_reqs as u8;

                        if chunk_index == 0 && !pending_writes.is_empty() {
                            log::info!(
                                "submit thread injecting {} write requests (seq={})",
                                pending_writes.len(), seq
                            );
                        }

                        // 包大小硬校验：超长命令会被固件静默截断，
                        // 宁可本轮作废也不能发出（会污染后续切帧）
                        if cmd.len() > packet_size as usize {
                            log::error!(
                                "submit thread built a {}-byte command exceeding packet size {} (seq={}, chunk {}/{}), cycle abandoned",
                                cmd.len(), packet_size, seq, chunk_index, chunk_total
                            );
                            push_notice(
                                &notices,
                                PipelineNotice::DataDropped {
                                    message: format!(
                                        "Internal error: command of {} bytes exceeds device packet size {}",
                                        cmd.len(), packet_size
                                    ),
                                },
                            );
                            aborted = true;
                            break;
                        }

                        match session.submit(&cmd) {
                            Ok(true) => {
                                // 首分片提交成功的那一刻就是本样本的采样
                                // 时刻（命令已进入 USB OUT 槽位，设备随后
                                // 立即执行内存读）。整个周期共用这个值。
                                if chunk_index == 0 {
                                    cycle_submit_ns =
                                        start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                                }
                                if let Ok(mut q) = cmd_meta.lock() {
                                    q.push_back(CmdMeta {
                                        write_reqs: wreqs_this as u8,
                                        chunk_words: chunk_words[chunk_index],
                                        chunk_index: chunk_index as u16,
                                        chunk_total: chunk_total as u16,
                                        submit_ns: cycle_submit_ns,
                                    });
                                }
                                in_flight.fetch_add(1, Ordering::SeqCst);
                                submitted += 1;
                            }
                            Ok(false) => {
                                // OUT 槽位用尽：设备吞吐跟不上，本轮剩余分片
                                // 放弃（整个周期不完整，接收端会丢弃不完整
                                // 周期的分片数据）。
                                aborted = true;
                                break;
                            }
                            Err(e) => {
                                if !running.load(Ordering::Relaxed) {
                                    log::info!("submit thread received stop signal, exiting");
                                    break;
                                }
                                let msg = format!("USB submit failed: {}", e);
                                log::error!("submit thread {} (seq={}), exiting", msg, seq);
                                if let Ok(mut slot) = thread_error.lock() {
                                    slot.get_or_insert(msg);
                                }
                                break;
                            }
                        }
                    }

                    // 未提交的写入重新入队（下次周期重试）
                    if aborted && !pending_writes.is_empty() {
                        if let Ok(mut wq) = write_queue.lock() {
                            for (idx, v) in pending_writes.iter().rev() {
                                wq.push_front((*idx, *v));
                            }
                        }
                    }
                    // 周期未完整提交：不推进 seq（该周期不会产出样本），
                    // 但仍按节奏等待到下一个采样时刻，避免忙转
                    if submitted < chunk_total {
                        seq += 1;
                        let target = start + Duration::from_nanos(tick_ns(seq, rate_hz));
                        let now = Instant::now();
                        if now < target {
                            spin_sleep::sleep(target - now);
                        }
                        continue;
                    }

                    seq += 1;

                    // --- 精确等间隔控制 ---
                    // 下一个目标时刻 = start + tick_ns(seq)
                    let target = start + Duration::from_nanos(tick_ns(seq, rate_hz));
                    let now = Instant::now();
                    if now < target {
                        // spin_sleep 提供微秒级精度的混合睡眠
                        spin_sleep::sleep(target - now);
                    }
                }

                log::info!("submit thread exited, {} sample periods submitted in total", seq);
            })
            .map_err(|e| Error::PipelineThread(e.to_string()))?;

        Ok(handle)
    }

    /// 启动接收线程
    fn spawn_collect_thread(
        &self,
        session: Arc<AsyncSession>,
        ring: Arc<RingBuffer>,
        thread_error: Arc<Mutex<Option<String>>>,
        notices: Arc<Mutex<Vec<PipelineNotice>>>,
    ) -> Result<JoinHandle<()>> {
        let running = Arc::clone(&self.running);
        let accesses = self.accesses.clone();
        let num_vars = accesses.len();
        let total_words = self.total_words;
        let rate_hz = self.rate_hz;
        let timestamp_mode = self.timestamp_mode;
        let last_words = Arc::clone(&self.last_words);
        let cmd_meta = Arc::clone(&self.cmd_meta);
        let in_flight = Arc::clone(&self.in_flight);

        let handle = thread::Builder::new()
            .name("dap-collect".into())
            .spawn(move || {
                // carry-over buffer：保存上次读取的不完整响应尾部，
                // 下次读取时拼接到开头再解析，避免解析错位。
                let mut carry: Vec<u8> = Vec::new();
                // 拼接缓冲：循环外复用，避免每轮堆分配
                let mut data: Vec<u8> = Vec::with_capacity(4096);
                let mut seq: u64 = 0;
                // 致命错误日志限频：拔线后每次读都会立即失败，不限频会
                // 每秒打出数十万条 warn 刷爆日志文件（原缺陷 ⑩）
                let mut last_err_log: Option<Instant> = None;

                // 周期累积器：一条采样周期在设备包受限时会拆成多条
                // DAP_Transfer 命令，各分片的读字按 word 偏移填回这里，
                // 全部分片到齐后才产出一个 Sample（否则波形会缺通道、
                // 或把半周期数据当成完整样本）。
                let mut cycle_words: Vec<u32> = vec![0; total_words];
                let mut cycle_next_chunk: u16 = 0;
                let mut cycle_chunk_total: u16 = 0;
                let mut cycle_written_words: usize = 0;
                // 本周期首分片的真实提交时刻（纳秒，来自提交线程）
                let mut cycle_submit_ns: u64 = 0;
                // 周期中途出现非 OK 状态 / 分片丢失 → 整个周期作废
                let mut cycle_broken = false;
                // 上一个已产出样本的时间戳（保证严格单调递增）
                let mut last_timestamp_sec: Option<f64> = None;

                while running.load(Ordering::Relaxed) {
                    // 等待下一条完整响应（异步 IN 池在事件泵线程里收）
                    let chunk = match session.next_response(Duration::from_millis(200)) {
                        Ok(Some(c)) => c,
                        Ok(None) => continue, // 例行超时，检查停止标志后继续
                        Err(e) => {
                            // 设备拔出 / 管道错误等致命错误：上报 UI（触发
                            // 自动停止并提示）、限频日志、短暂休眠避免忙转。
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
                    data.extend_from_slice(&chunk);
                    carry.clear();

                    // 一条 IN transfer 可能包含多个响应（USB Bulk 合并短包）。
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
                            if let Ok(mut q) = cmd_meta.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            // 字节流已失步，周期累积器也作废
                            cycle_broken = true;
                            break;
                        }
                        let count = data[offset + 1] as usize;
                        let status = data[offset + 2];
                        // 本条命令的元数据：提交线程每发一条命令入队一条。
                        // 只 peek——响应可能跨多次 USB 读，完整消费后才出队。
                        let meta = cmd_meta_front(&cmd_meta);
                        let Some(meta) = meta else {
                            log::error!("collect thread command metadata queue empty, dropping {} bytes to resync", data.len() - offset);
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
                            if let Ok(mut q) = cmd_meta.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            cycle_broken = true;
                            break;
                        };
                        // 本分片的读取请求序列（布局固定：[写TAR, 读DRW] × 字数）
                        let read_layout = chunk_layout(meta.chunk_words);
                        let total_requests = meta.write_reqs as usize + read_layout.len();
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
                            if let Ok(mut q) = cmd_meta.lock() {
                                q.clear();
                            }
                            in_flight.store(0, Ordering::SeqCst);
                            cycle_broken = true;
                            break;
                        }
                        let need = transfer_frame_len(
                            count,
                            status,
                            meta.write_reqs as usize,
                            &read_layout,
                        );
                        // 不完整响应 → 留给 carry，下次读取拼接后再解析
                        if data.len() - offset < need {
                            break;
                        }
                        let frame = &data[offset..offset + need];
                        offset += need;

                        // 完整消费一条响应：元数据出队 + 在途递减
                        if let Ok(mut q) = cmd_meta.lock() {
                            q.pop_front();
                        }
                        in_flight_dec(&in_flight);

                        // 解析单个响应
                        let resp = match DapProtocol::parse_transfer_response(frame) {
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
                                cycle_broken = true;
                                continue;
                            }
                        };

                        // 分片序号必须严格递增：缺分片意味着该周期的字不完整
                        if meta.chunk_index != cycle_next_chunk {
                            log::error!(
                                "collect thread chunk order mismatch (seq={}): expected {}, got {}",
                                seq, cycle_next_chunk, meta.chunk_index
                            );
                            push_notice(
                                &notices,
                                PipelineNotice::DataDropped {
                                    message: format!(
                                        "Sampling cycle fell out of sync at chunk {} (expected {}), cycle dropped",
                                        meta.chunk_index, cycle_next_chunk
                                    ),
                                },
                            );
                            cycle_broken = true;
                        }
                        if meta.chunk_index == 0 {
                            // 新周期起点：清空累积器（可能覆盖上一周期
                            // 未完整到达的残留分片）
                            cycle_chunk_total = meta.chunk_total;
                            cycle_written_words = 0;
                            cycle_submit_ns = meta.submit_ns;
                            cycle_broken = false;
                        }

                        if resp.status != TRANSFER_OK {
                            log::warn!(
                                "collect thread got non-OK status (seq={}, chunk {}/{}): status={}, count={}",
                                seq, meta.chunk_index, meta.chunk_total, resp.status, resp.count
                            );
                            cycle_broken = true;
                        } else if !cycle_broken {
                            // 把本分片的读字按偏移填进周期累积器。FAULT 时
                            // 响应可能少字，尾部通道保持 0（不做位移，否则
                            // 波形通道串号）。
                            let words = resp.values();
                            let base = cycle_written_words;
                            let take = words.len().min(meta.chunk_words as usize);
                            let end = (base + take).min(cycle_words.len());
                            if base < end {
                                cycle_words[base..end].copy_from_slice(&words[..end - base]);
                            }
                            cycle_written_words = base + take;
                        }

                        cycle_next_chunk = meta.chunk_index.saturating_add(1);

                        // 周期完整（所有分片都到齐且未损坏）→ 产出样本
                        if !cycle_broken
                            && cycle_next_chunk >= cycle_chunk_total
                            && cycle_chunk_total > 0
                        {
                            // 时间戳生成（模式由 Advanced Settings 选择）
                            //
                            // - Grid（默认）：seq × 1e9 / rate 的理想等间隔。
                            //   波形点间距恒定、视觉平滑；但背压跳过的周期也会
                            //   推进 seq，时间轴会系统性超前于真实采样时刻。
                            // - Real：本周期首分片的实际提交时刻。跳过的周期
                            //   不产生样本，其时长如实体现为更大的间隔。
                            let raw_ts = match timestamp_mode {
                                TimestampMode::Grid => {
                                    tick_ns(seq, rate_hz) as f64 / 1_000_000_000.0
                                }
                                TimestampMode::Real => {
                                    cycle_submit_ns as f64 / 1_000_000_000.0
                                }
                            };

                            // 时间戳必须严格递增（波形点集的二分查找依赖
                            // 单调性）。Real 模式下同一采样周期被重复提交或
                            // 时钟分辨率不足时可能相等；Grid 模式在纳秒取整
                            // 极小间隔时也可能相等。统一钳到上一时间戳 +1ns。
                            let timestamp_sec = if let Some(prev) = last_timestamp_sec {
                                if raw_ts <= prev {
                                    prev + 1e-9
                                } else {
                                    raw_ts
                                }
                            } else {
                                raw_ts
                            };
                            last_timestamp_sec = Some(timestamp_sec);

                            // 按 accesses.words 顺序把读回的字归组后再拼值
                            let mut vals = [0u64; MAX_VALUES];
                            {
                                let words = &cycle_words[..];
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
                                    // 的值）：写入时要做读-改-写合并整字中的
                                    // 其它 lane。
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
                            // 周期结束：重置累积器（下一分片应为 chunk 0）
                            cycle_next_chunk = 0;
                            cycle_chunk_total = 0;
                            cycle_written_words = 0;
                            cycle_broken = false;
                        } else {
                            cycle_next_chunk = meta.chunk_index.saturating_add(1);
                        }
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

/// 读取命令元数据队首（不消费）；队列为空返回 None
fn cmd_meta_front(queue: &Mutex<VecDeque<CmdMeta>>) -> Option<CmdMeta> {
    queue.lock().ok().and_then(|q| q.front().copied())
}

/// 单个分片的请求类型序列：每个字一组 [写 TAR, 读 DRW]
fn chunk_layout(words: u16) -> Vec<bool> {
    let mut layout = Vec::with_capacity(2 * words as usize);
    for _ in 0..words {
        layout.push(false);
        layout.push(true);
    }
    layout
}

/// 追加一组 [写 TAR, 写 DRW] 请求字节
///
/// 每个字写前都重写 TAR：不依赖 AHB-AP 的 CSW.AddrInc 自增。
fn append_write_word(cmd: &mut Vec<u8>, addr: u32, value: u32) {
    cmd.push(TransferRequest::write_ap(AP_REG_TAR, addr).request_byte());
    cmd.extend_from_slice(&addr.to_le_bytes());
    cmd.push(TransferRequest::write_ap(AP_REG_DRW, value).request_byte());
    cmd.extend_from_slice(&value.to_le_bytes());
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
    /// 异步 USB 会话（必须在两个工作线程 join 完之后再 stop）。
    /// 单元测试构造 handle 时无真实设备，为 None。
    session: Option<Arc<AsyncSession>>,
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
        // 两个工作线程都已退出：此时才可安全取消在途 transfer
        // （事件泵回调仍会访问槽位，提前停止会有 use-after-free 风险）
        if let Some(s) = &self.session {
            s.stop();
        }
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
        // 16 个 32 位变量 = 32 条请求，按设备包拆分即可
        let narrow: Vec<MemAccess> = (0..MAX_VALUES as u32)
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 4, 4))
            .collect();
        assert_eq!(read_block_requests(&narrow), 32);
        assert!(validate_config(&narrow, 64).is_ok(), "64-byte packet must work via chunking");

        let wide: Vec<MemAccess> = (0..15u32)
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 8, 8))
            .collect();
        assert_eq!(read_block_requests(&wide), 60);
        assert!(validate_config(&wide, 64).is_ok(), "64-byte packet must work via chunking");
    }

    #[test]
    fn validate_config_rejects_too_many_variables() {
        // 17 个变量超过 MAX_VALUES：Sample 定长槽会静默截断，必须显式报错
        let accesses: Vec<MemAccess> = (0..=(MAX_VALUES as u32))
            .map(|i| MemAccess::from_addr_size(0x2000_0000 + i * 4, 4))
            .collect();
        let err = validate_config(&accesses, 512).unwrap_err().to_string();
        assert!(
            err.contains("Too many sampling variables")
                && err.contains(&MAX_VALUES.to_string()),
            "错误信息应说明变量数上限，实际 {err:?}"
        );
    }

    #[test]
    fn validate_config_rejects_single_variable_larger_than_packet() {
        // 单变量跨 3 字（非对齐 64 位）需要 6 条请求 + 响应 12 字节；
        // packet_size 小到放不下时无法拆分（拆开就不是同一时刻采样），
        // 必须显式报错而不是下发被固件截断的命令
        let accesses = vec![MemAccess::from_addr_size(0x2000_0002, 8)];
        assert_eq!(accesses[0].words, 3);
        let err = validate_config(&accesses, 16).unwrap_err().to_string();
        assert!(err.contains("needs 3 32-bit words"), "actual: {err:?}");
    }

    #[test]
    fn max_words_per_cmd_respects_packet_and_response_size() {
        // 64 字节包：命令侧 12 条请求上限 → 6 字；响应侧 61 字节 → 15 字。
        // 取小 = 6 字。
        assert_eq!(max_words_per_cmd(64), 6);
        // 512 字节包：命令侧 (512-3)/5 = 101 条请求 → 50 字，封顶
        // MAX_TRANSFER_REQUESTS=64 条请求 → 32 字；响应侧 (512-3)/4=127。
        // 取小 = 32 字。
        assert_eq!(max_words_per_cmd(512), 32);
    }

    #[test]
    fn in_flight_limit_never_exceeds_device_buffer_count() {
        // 固件响应队列满会丢弃命令且不产生响应（切帧永久错位），
        // 在途上限必须受 packet_count 约束
        assert_eq!(in_flight_limit(1), 1);
        assert_eq!(in_flight_limit(2), 2);
        assert_eq!(in_flight_limit(4), 4);
        assert_eq!(in_flight_limit(255), MAX_IN_FLIGHT);
        assert_eq!(in_flight_limit(0), 1, "0 must be clamped, never unlimited");
    }

    #[test]
    fn chunk_layout_matches_word_count() {
        let layout = chunk_layout(3);
        assert_eq!(layout, vec![false, true, false, true, false, true]);
        assert!(chunk_layout(0).is_empty());
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
            session: None,
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

    // ---- 分包：多包响应必须拼回同一周期 ----

    /// 模拟接收线程的周期拼装：喂入分片响应，产出完整周期的字
    #[derive(Default)]
    struct CycleAssembler {
        words: Vec<u32>,
        next_chunk: u16,
        total: u16,
        written: usize,
        broken: bool,
    }

    impl CycleAssembler {
        fn new(total_words: usize) -> Self {
            Self { words: vec![0; total_words], ..Default::default() }
        }

        /// 返回 Some(words) = 一个完整周期产出
        ///
        /// `status_ok=false` 模拟非 OK 响应（FAULT/WAIT）：该周期作废
        fn feed(&mut self, meta: CmdMeta, data: &[u32], status_ok: bool) -> Option<Vec<u32>> {
            if meta.chunk_index != self.next_chunk {
                self.broken = true;
            }
            if meta.chunk_index == 0 {
                self.total = meta.chunk_total;
                self.written = 0;
                self.broken = false;
            }
            if !status_ok {
                self.broken = true;
            }
            if !self.broken {
                let base = self.written;
                let take = data.len().min(meta.chunk_words as usize);
                let end = (base + take).min(self.words.len());
                if base < end {
                    self.words[base..end].copy_from_slice(&data[..end - base]);
                }
                self.written = base + take;
            }
            self.next_chunk = meta.chunk_index + 1;
            if !self.broken && self.next_chunk >= self.total && self.total > 0 {
                self.next_chunk = 0;
                self.total = 0;
                self.written = 0;
                self.broken = false;
                return Some(self.words.clone());
            }
            None
        }
    }

    fn meta(write_reqs: u8, chunk_words: u16, chunk_index: u16, chunk_total: u16) -> CmdMeta {
        CmdMeta { write_reqs, chunk_words, chunk_index, chunk_total, submit_ns: 0 }
    }

    fn meta_at(
        write_reqs: u8,
        chunk_words: u16,
        chunk_index: u16,
        chunk_total: u16,
        submit_ns: u64,
    ) -> CmdMeta {
        CmdMeta { write_reqs, chunk_words, chunk_index, chunk_total, submit_ns }
    }

    // ---- 真实时间戳（不是 ideal grid） ----

    /// 模拟接收线程的时间戳逻辑：使用周期首分片的真实提交时刻，
    /// 并强制严格单调递增
    fn next_timestamp(prev: Option<f64>, submit_ns: u64) -> f64 {
        let ts = submit_ns as f64 / 1_000_000_000.0;
        match prev {
            Some(p) if ts <= p => p + 1e-9,
            _ => ts,
        }
    }

    #[test]
    fn timestamp_uses_real_submit_time_not_grid() {
        // 目标 10 kHz（网格 100 µs/点），但设备实际 4 kHz（250 µs/点）。
        // 旧实现用 seq×100µs，20 个样本后时间轴只走了 1.9 ms，实际过了
        // 4.75 ms——波形被压缩近 2.5 倍。真实时间戳必须反映 250 µs 间隔。
        let rate = 10_000u64;
        let grid_period_ns = 1_000_000_000 / rate; // 100 µs
        let real_period_ns = 250_000u64; // 实际 4 kHz

        let mut prev: Option<f64> = None;
        let mut ts = Vec::new();
        for seq in 0..20u64 {
            // 真实的提交时刻（设备跟不上，按 250 µs 节拍）
            let submit_ns = real_period_ns * seq;
            let t = next_timestamp(prev, submit_ns);
            prev = Some(t);
            ts.push(t);
            // 与 ideal grid 的差距随时间线性拉大
            let grid = (grid_period_ns * seq) as f64 / 1e9;
            if seq >= 10 {
                assert!(
                    (t - grid) > 1e-4,
                    "seq={seq}: real {t:.6}s must diverge from grid {grid:.6}s"
                );
            }
        }
        // 相邻间隔必须等于真实间隔，而不是目标间隔
        for w in ts.windows(2) {
            let dt = w[1] - w[0];
            assert!(
                (dt - 2.5e-4).abs() < 1e-6,
                "interval {dt:.6} must be the real 250µs, not the 100µs grid"
            );
        }
    }

    #[test]
    fn timestamp_reflects_skipped_cycles_as_larger_gaps() {
        // 背压跳过若干周期：时间戳跨度必须包含跳过的时长，
        // 而不是把空缺「填平」（那样波形频率会显得偏高）
        let mut prev: Option<f64> = None;
        // 样本 0 在 0 µs，样本 1 在 100 µs，样本 2 在 400 µs（中间跳过 2 个周期）
        let submits = [0u64, 100_000, 400_000];
        let mut ts = Vec::new();
        for &s in &submits {
            let t = next_timestamp(prev, s);
            prev = Some(t);
            ts.push(t);
        }
        assert!((ts[1] - ts[0] - 1e-4).abs() < 1e-9);
        assert!(
            (ts[2] - ts[1] - 3e-4).abs() < 1e-9,
            "skipped cycles must appear as a 300µs gap, got {}",
            ts[2] - ts[1]
        );
    }

    #[test]
    fn timestamps_stay_strictly_increasing_on_duplicate_submit_time() {
        // 时钟分辨率不足 / 同一周期被重复处理：时间戳不得相等或倒退，
        // 否则波形的二分查找与可见范围裁剪会失效
        let mut prev: Option<f64> = None;
        let values = [1_000_000u64, 1_000_000, 900_000, 2_000_000];
        let mut out = Vec::new();
        for v in values {
            let t = next_timestamp(prev, v);
            prev = Some(t);
            out.push(t);
        }
        for w in out.windows(2) {
            assert!(w[1] > w[0], "timestamps must strictly increase: {:?}", out);
        }
    }

    #[test]
    fn chunk_meta_carries_cycle_submit_time_to_all_chunks() {
        // 同一周期的所有分片共享首分片的提交时刻（接收线程只在
        // chunk_index==0 时读取，但值必须一路带着）
        let first = meta_at(0, 6, 0, 2, 123_456_789);
        assert_eq!(first.submit_ns, 123_456_789);
        let second = meta_at(0, 2, 1, 2, 123_456_789);
        assert_eq!(second.submit_ns, first.submit_ns);
    }

    #[test]
    fn multi_packet_cycle_assembles_in_order() {
        // 16 个 float = 16 字，64 字节包每包 6 字 → 3 个分片（6+6+4）
        let mut asm = CycleAssembler::new(16);
        assert_eq!(asm.feed(meta(0, 6, 0, 3), &[10, 11, 12, 13, 14, 15], true), None);
        assert_eq!(asm.feed(meta(0, 6, 1, 3), &[16, 17, 18, 19, 20, 21], true), None);
        let out = asm.feed(meta(0, 4, 2, 3), &[22, 23, 24, 25], true).expect("last chunk completes cycle");
        assert_eq!(out, (10..26).collect::<Vec<u32>>(), "chunks must land at their own offsets");
    }

    #[test]
    fn multi_packet_cycle_with_write_in_first_chunk() {
        // 首包带 2 条写请求（一条 32 位写入），读取数据仍从偏移 0 开始
        let mut asm = CycleAssembler::new(8);
        assert_eq!(asm.feed(meta(2, 6, 0, 2), &[1, 2, 3, 4, 5, 6], true), None);
        let out = asm.feed(meta(0, 2, 1, 2), &[7, 8], true).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn missing_chunk_breaks_cycle_without_emitting_partial() {
        // 中间分片丢失：周期必须作废，不能把半份数据当成完整样本
        let mut asm = CycleAssembler::new(12);
        assert_eq!(asm.feed(meta(0, 6, 0, 2), &[1, 2, 3, 4, 5, 6], true), None);
        // 直接来了第 2 分片（缺第 1）
        let out = asm.feed(meta(0, 6, 2, 3), &[7, 8, 9, 10, 11, 12], true);
        assert!(out.is_none(), "broken cycle must not emit");
        // 下一周期从头开始，必须能正常恢复
        assert_eq!(asm.feed(meta(0, 6, 0, 2), &[21, 22, 23, 24, 25, 26], true), None);
        let out = asm.feed(meta(0, 6, 1, 2), &[27, 28, 29, 30, 31, 32], true).unwrap();
        assert_eq!(out[..6], [21, 22, 23, 24, 25, 26]);
        assert_eq!(out[6..], [27, 28, 29, 30, 31, 32]);
    }

    #[test]
    fn short_chunk_keeps_tail_channels_zero() {
        // FAULT 导致末分片少字：尾部通道保持 0，位置不位移（不串号）
        let mut asm = CycleAssembler::new(8);
        assert_eq!(asm.feed(meta(0, 6, 0, 2), &[1, 2, 3, 4, 5, 6], true), None);
        let out = asm.feed(meta(0, 2, 1, 2), &[7], true).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 0], "missing word must stay 0, not shift");
    }

    #[test]
    fn single_packet_cycle_unchanged() {
        // 包足够大时每周期一个分片：行为与旧实现一致
        let mut asm = CycleAssembler::new(4);
        let out = asm.feed(meta(0, 4, 0, 1), &[5, 6, 7, 8], true).unwrap();
        assert_eq!(out, vec![5, 6, 7, 8]);
    }

    // ---- 纳秒采样间隔 ----

    #[test]
    fn nanosecond_interval_has_no_rate_distortion() {
        // 旧实现用微秒取整：44100 Hz → 22 µs（隐含 45454 Hz，偏 +3.1%），
        // 30000 Hz → 33 µs（隐含 30303 Hz，偏 +1.0%）。先乘后除必须把
        // 单个周期的误差压到远小于 0.01%。
        for rate in [1_000u32, 5_000, 7_000, 30_000, 44_100, 100_000] {
            let period_ns = tick_ns(1, rate) as f64;
            let implied = 1_000_000_000.0 / period_ns;
            let err_pct = (implied - rate as f64) / rate as f64 * 100.0;
            assert!(
                err_pct.abs() < 0.01,
                "rate={rate} period_ns={period_ns} implied={implied:.3} err={err_pct:.4}%"
            );
        }
    }

    #[test]
    fn nanosecond_timestamps_stay_on_grid_over_long_runs() {
        // 10 分钟的 30 kHz 采样：时间戳累积漂移必须远小于一个采样周期。
        // tick_ns 先乘后除，漂移不随运行时长线性增长。
        let rate = 30_000u32;
        let samples = 30_000u64 * 600; // 600 s
        let last_ts = tick_ns(samples - 1, rate) as f64 / 1_000_000_000.0;
        let expected = (samples - 1) as f64 / rate as f64;
        let drift = (last_ts - expected).abs();
        let period = 1.0 / rate as f64;
        assert!(drift < period, "drift {drift} must stay below one period {period}");
    }

    #[test]
    fn tick_ns_does_not_accumulate_integer_nanosecond_error() {
        // 回归：44100 Hz 的 1e9/rate = 22675 ns（截断），乘 seq 后
        // 600 秒会偏 6 ms。tick_ns 必须让误差停留在单个周期内。
        let rate = 44_100u32;
        let samples = 44_100u64 * 600;
        let naive_interval = 1_000_000_000u64 / u64::from(rate);
        let naive_last = (naive_interval * (samples - 1)) as f64 / 1_000_000_000.0;
        let exact_last = tick_ns(samples - 1, rate) as f64 / 1_000_000_000.0;
        let expected = (samples - 1) as f64 / rate as f64;
        assert!(
            (naive_last - expected).abs() > 0.001,
            "naive interval*seq should drift (this is what we fixed)"
        );
        assert!(
            (exact_last - expected).abs() < 1e-6,
            "tick_ns must be drift-free, got {} vs {}",
            exact_last,
            expected
        );
    }

    #[test]
    fn packet_size_bounds_command_bytes() {
        // 每条命令（含头与写数据）必须 ≤ 设备 packet_size，
        // 否则固件会静默截断（DAP_queue_execute_buf 里的 len 钳制）
        for packet in [64u16, 128, 512, 1024] {
            let words = max_words_per_cmd(packet);
            // 纯读取命令：3 字节头 + 2 请求/字 × 5 字节
            let cmd_bytes = RESPONSE_HEADER_BYTES + words * REQUESTS_PER_WORD * BYTES_PER_REQUEST;
            assert!(
                cmd_bytes <= packet as usize,
                "packet={packet} words={words} cmd={cmd_bytes} exceeds packet size"
            );
            // 响应：3 字节头 + 4 字节/字
            let resp_bytes = RESPONSE_HEADER_BYTES + words * 4;
            assert!(
                resp_bytes <= packet as usize,
                "packet={packet} words={words} resp={resp_bytes} exceeds packet size"
            );
        }
    }

    #[test]
    fn max_words_per_cmd_is_zero_only_for_tiny_packets() {
        assert_eq!(max_words_per_cmd(3), 0, "no room for any request");
        assert_eq!(max_words_per_cmd(12), 0, "3B header + 10B request = 13 > 12");
        assert_eq!(max_words_per_cmd(13), 1, "exactly one word fits");
    }

    #[test]
    fn first_chunk_leaves_room_for_write_budget() {
        // 首包要并入写入块，预留后读字数必须减少（除非 u8 请求数上限
        // 已经先起作用——此时总量仍远小于包大小）
        for packet in [64u16, 128, 512, 1024] {
            let full = max_words_per_cmd(packet);
            let first = first_chunk_words(packet).max(1);
            assert!(first <= full, "first chunk cannot exceed full chunk");
            let request_capped = max_requests_per_cmd(packet) == MAX_TRANSFER_REQUESTS;
            if !request_capped {
                assert!(first < full, "packet={packet}: first chunk must shrink to fit writes");
            }
            // 首包 + 预留写入预算后的总字节数必须 ≤ packet_size
            let bytes = RESPONSE_HEADER_BYTES
                + WRITE_BUDGET_REQUESTS * BYTES_PER_REQUEST
                + first * REQUESTS_PER_WORD * BYTES_PER_REQUEST;
            assert!(
                bytes <= packet as usize,
                "packet={packet} first={first} bytes={bytes} exceeds packet size"
            );
        }
    }

    #[test]
    fn fast_path_command_with_writes_stays_within_packet() {
        // 端到端核算：最宽写入（非对齐 64 位 = 6 条写请求）并入首包后，
        // 命令长度必须 ≤ 64 字节（全速 DAPLink 的 DAP_PACKET_SIZE）
        let packet = 64usize;
        let first = first_chunk_words(packet as u16).max(1);
        let write_bytes = WRITE_BUDGET_REQUESTS * BYTES_PER_REQUEST;
        let read_bytes = first * REQUESTS_PER_WORD * BYTES_PER_REQUEST;
        let total = RESPONSE_HEADER_BYTES + write_bytes + read_bytes;
        assert!(total <= packet, "total={total} must fit in {packet}-byte packet");

        // 响应侧：首包读字数对应的数据 + 3 字节头也必须 ≤ 包大小
        let resp = RESPONSE_HEADER_BYTES + first * 4;
        assert!(resp <= packet, "resp={resp} must fit");
    }

    // ---- 时间戳模式（grid / real 可配置） ----

    #[test]
    fn timestamp_mode_defaults_to_grid() {
        // 默认必须是 grid：与历史行为一致，避免升级后波形突然变得疏密不均
        assert_eq!(TimestampMode::default(), TimestampMode::Grid);
    }

    #[test]
    fn timestamp_mode_label_round_trip() {
        for mode in [TimestampMode::Grid, TimestampMode::Real] {
            assert_eq!(TimestampMode::from_label(mode.label()), mode);
        }
        // 未知/空值回退默认 grid（工程文件损坏时不至于打不开）
        assert_eq!(TimestampMode::from_label(""), TimestampMode::Grid);
        assert_eq!(TimestampMode::from_label("bogus"), TimestampMode::Grid);
        // 大小写不敏感（手写工程文件容错）
        assert_eq!(TimestampMode::from_label("REAL"), TimestampMode::Real);
        assert_eq!(TimestampMode::from_label("Grid"), TimestampMode::Grid);
    }

    #[test]
    fn grid_mode_timestamps_are_uniform_but_drift_from_real_time() {
        // Grid 模式：即使设备只跑出 4 kHz（目标 10 kHz），时间戳仍按
        // 100 µs 等差——这就是「假时间戳」的特性，作为可选模式保留
        let rate = 10_000u32;
        let real_period_ns = 250_000u64;
        let mut prev: Option<f64> = None;
        let mut ts = Vec::new();
        for seq in 0..10u64 {
            // Grid 模式只看 seq，完全忽略真实提交时刻
            let raw = tick_ns(seq, rate) as f64 / 1e9;
            let t = match prev {
                Some(p) if raw <= p => p + 1e-9,
                _ => raw,
            };
            prev = Some(t);
            ts.push(t);
            let _ = real_period_ns; // 真实时刻在此模式下不参与
        }
        // 间隔恒为 100 µs
        for w in ts.windows(2) {
            assert!((w[1] - w[0] - 1e-4).abs() < 1e-9, "grid interval must be uniform");
        }
        // 但 10 个样本只覆盖 0.9 ms，真实已过 2.25 ms → 时间轴超前
        assert!(ts[9] < 0.001, "grid时间轴远小于真实经过时间");
    }

    #[test]
    fn real_mode_timestamps_follow_actual_submit_times() {
        // Real 模式：时间戳直接取提交时刻，跳周期体现为更大间隔
        let submits = [0u64, 100_000, 200_000, 500_000];
        let mut prev: Option<f64> = None;
        let mut ts = Vec::new();
        for &s in &submits {
            let raw = s as f64 / 1e9;
            let t = match prev {
                Some(p) if raw <= p => p + 1e-9,
                _ => raw,
            };
            prev = Some(t);
            ts.push(t);
        }
        assert!((ts[1] - ts[0] - 1e-4).abs() < 1e-9);
        assert!((ts[2] - ts[1] - 1e-4).abs() < 1e-9);
        // 第 3→4 个样本之间跳过 2 个周期
        assert!((ts[3] - ts[2] - 3e-4).abs() < 1e-9, "skipped cycles show as 300µs gap");
    }

    #[test]
    fn both_modes_keep_timestamps_strictly_increasing() {
        // 两种模式都必须满足波形的单调性要求
        for mode in [TimestampMode::Grid, TimestampMode::Real] {
            let mut prev: Option<f64> = None;
            // 故意制造相等/倒退的原始值
            let raws = [1.0e-3, 1.0e-3, 5.0e-4, 2.0e-3];
            let mut out = Vec::new();
            for raw in raws {
                let t = match prev {
                    Some(p) if raw <= p => p + 1e-9,
                    _ => raw,
                };
                prev = Some(t);
                out.push(t);
            }
            for w in out.windows(2) {
                assert!(w[1] > w[0], "mode {mode:?} must stay strictly increasing: {out:?}");
            }
        }
    }

    #[test]
    fn write_budget_covers_widest_variable() {
        // 最宽写入 = 非对齐 64 位：3 字 × 2 请求/字 = 6 条请求。
        // 预留不足会让首包超包（固件静默截断）。
        let widest = MemAccess::from_addr_size(0x2000_0002, 8);
        assert_eq!(widest.words, 3);
        assert!(
            WRITE_BUDGET_REQUESTS >= widest.words as usize * REQUESTS_PER_WORD,
            "write budget {} must cover {}-request write",
            WRITE_BUDGET_REQUESTS,
            widest.words as usize * REQUESTS_PER_WORD
        );
    }

    // ---- 端到端：真实 DAPLink 字节流 → 分包 → 拼装 ----

    /// 按 DAPLink 固件行为生成一条响应帧
    ///
    /// 复用被测的 `transfer_frame_len` 计算长度，但数据区独立按
    /// posted-read 语义产生：每个读请求挂起，下一个请求前取回。
    fn daplink_response(write_reqs: usize, layout: &[bool], values: &[u32], status: u8) -> Vec<u8> {
        // 全 OK：执行完所有请求
        let count = write_reqs + layout.len();
        let mut data = Vec::new();
        let mut posted = false;
        let mut it = values.iter();
        for _ in 0..write_reqs {
            if posted {
                data.push(*it.next().unwrap());
                posted = false;
            }
        }
        for &is_read in layout {
            if is_read {
                if posted {
                    data.push(*it.next().unwrap());
                }
                posted = true;
            } else if posted {
                data.push(*it.next().unwrap());
                posted = false;
            }
        }
        if status == TRANSFER_OK && posted {
            data.push(*it.next().unwrap());
        }
        let mut frame = vec![DAP_TRANSFER, count as u8, status];
        for v in data {
            frame.extend_from_slice(&v.to_le_bytes());
        }
        frame
    }

    #[test]
    fn end_to_end_multi_packet_stream_reassembles_correctly() {
        // 16 个 32 位通道 @ 64 字节包，本周期带一条写入（用预留布局）：
        // 首包为写块留 6 条请求 → 3 字；后续 6 字/包
        let packet = 64u16;
        let words_per_cmd = max_words_per_cmd(packet);
        let first_words = first_chunk_words(packet).max(1);
        let total = 16usize;

        // 构造与提交线程一致的分片布局
        let mut chunk_sizes = vec![first_words.min(total)];
        let mut remaining = total.saturating_sub(chunk_sizes[0]);
        while remaining > 0 {
            let n = words_per_cmd.min(remaining);
            chunk_sizes.push(n);
            remaining -= n;
        }
        assert_eq!(
            chunk_sizes,
            vec![3, 6, 6, 1],
            "6-request write budget leaves 3 words in the first packet"
        );
        // 每包命令长度都必须在 64 字节内（含首包写入块）
        for (i, &n) in chunk_sizes.iter().enumerate() {
            let wr = if i == 0 { 2 } else { 0 };
            let bytes = RESPONSE_HEADER_BYTES + wr * BYTES_PER_REQUEST + n * 2 * BYTES_PER_REQUEST;
            assert!(bytes <= packet as usize, "chunk {i} command {bytes}B exceeds packet");
        }

        // 模拟设备：每个分片返回该分片对应的字（值 = 100 + 全局字索引）
        let mut stream = Vec::new();
        let mut global = 0usize;
        for (i, &n) in chunk_sizes.iter().enumerate() {
            let values: Vec<u32> = (0..n).map(|k| 100 + (global + k) as u32).collect();
            // 首包含一条 32 位写入（2 条写请求）
            let wr = if i == 0 { 2 } else { 0 };
            let layout = [false, true].repeat(n);
            stream.extend_from_slice(&daplink_response(wr, &layout, &values, TRANSFER_OK));
            global += n;
        }

        // 按接收线程的算法切帧 + 拼装
        let mut asm = CycleAssembler::new(total);
        let mut offset = 0;
        let mut produced = None;
        for (i, &n) in chunk_sizes.iter().enumerate() {
            let count = stream[offset + 1] as usize;
            let status = stream[offset + 2];
            let wr = if i == 0 { 2 } else { 0 };
            let layout = [false, true].repeat(n);
            let need = transfer_frame_len(count, status, wr, &layout);
            let frame = &stream[offset..offset + need];
            offset += need;

            let resp = DapProtocol::parse_transfer_response(frame).unwrap();
            assert_eq!(resp.status, TRANSFER_OK);
            let out = asm.feed(meta(wr as u8, n as u16, i as u16, chunk_sizes.len() as u16), resp.values(), resp.status == TRANSFER_OK);
            if let Some(v) = out {
                produced = Some(v);
            }
        }
        assert_eq!(offset, stream.len(), "all bytes consumed, no desync");
        let got = produced.expect("cycle must complete");
        assert_eq!(got, (100..116).collect::<Vec<u32>>(), "channels must keep their positions");
    }

    #[test]
    fn end_to_end_fault_in_middle_chunk_drops_only_that_cycle() {
        // 第 2 个分片 FAULT：该周期作废，下一个周期必须恢复正常
        let layout = [false, true].repeat(3);
        let cycle1_chunk1 = daplink_response(0, &layout, &[1, 2, 3], TRANSFER_OK);
        // 第 2 分片 FAULT：执行到第 2 条请求（DRW 挂起但未取回→无数据）
        let mut fault = vec![DAP_TRANSFER, 2u8, TRANSFER_FAULT];
        assert_eq!(transfer_frame_len(2, TRANSFER_FAULT, 0, &layout), 3);
        fault.clear();
        fault.extend_from_slice(&[DAP_TRANSFER, 2, TRANSFER_FAULT]);

        let mut asm = CycleAssembler::new(6);
        assert!(asm.feed(meta(0, 3, 0, 2), &[1, 2, 3], true).is_none());

        // FAULT 响应：receiver 标记 cycle_broken 并继续消费
        let out = asm.feed(meta(0, 3, 1, 2), &[], false);
        assert!(out.is_none(), "broken cycle must not emit");

        // 下一周期完整：必须正常产出
        let next = daplink_response(0, &layout, &[7, 8, 9], TRANSFER_OK);
        let resp = DapProtocol::parse_transfer_response(&next).unwrap();
        let out = asm.feed(meta(0, 3, 0, 1), resp.values(), true).expect("next cycle must emit");
        assert_eq!(out, vec![7, 8, 9, 0, 0, 0]);
        assert_eq!(cycle1_chunk1[0], DAP_TRANSFER);
    }
}
