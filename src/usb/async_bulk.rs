//! libusb 原生异步 Bulk 传输会话
//!
//! # 为什么需要它
//!
//! 同步 bulk 调用（`rusb::write_bulk` / `read_bulk`）在 Windows + WinUSB 上
//! 每次都要经过 libusb 事件锁的一次完整往返，实测固定开销约 40µs，且与
//! 命令内容无关（几乎零工作量的 `DAP_Info` 与真实 SWD 读耗时相同）。
//! 采样引擎每条命令做一次同步写，于是采样率被压在 ~22kHz。
//!
//! 改用 libusb 原生异步 API 后的实测对照（同一设备、同一条命令）：
//!
//! | 方式 | 单条完整往返 | 上限 |
//! |---|---|---|
//! | 同步 write+read | 84 µs | 11.9 kHz |
//! | 异步 submit_transfer（池深 8~16） | 40 µs | 24~25 kHz |
//! | 异步 submit 调用本身耗时 | 2.2 µs | — |
//!
//! # 结构
//!
//! - **OUT 池**：每个槽位一条 bulk OUT transfer。`submit()` 找到空闲槽位，
//!   把命令字节拷进槽位缓冲后 `libusb_submit_transfer` 立即返回（≈2µs）。
//!   槽位全部占用 = 设备跟不上，调用方据此跳过本轮（背压语义不变）。
//! - **IN 池**：始终保持挂起。设备把响应写回 IN 端点时，回调把数据推进
//!   响应队列并立刻重新挂起，因此响应捕获与命令提交完全解耦。
//! - **事件泵线程**：唯一调用 `libusb_handle_events_*` 的线程，驱动所有
//!   回调；20ms 超时保证 Stop 能及时退出。
//!
//! 响应顺序由设备保证 FIFO（与提交顺序一致），调用方仍按原有
//! `writes_per_cmd` 队列逐条切帧，切帧逻辑无需改动。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusb::constants::*;
use rusb::ffi;

use super::transfer::BulkTransfer;
use crate::error::*;

/// 单条 OUT 命令的最大字节数（硬上限）
///
/// 引擎按设备上报的 packet_size 分包后，单包不会超过 packet_size；
/// 这里只是 libusb 槽位缓冲的物理上限，取 CMSIS-DAP 规范允许的最大
/// 包长（32768，见 DAP.c 的 `DAP_PACKET_SIZE > 32768` 校验）。
pub const MAX_PACKET_SIZE_HARD_LIMIT: u16 = 32768;

/// OUT 槽位缓冲实际分配大小
///
/// 取硬上限：设备上报 512/1024 时也要能装下整条命令。槽位数很少
/// （= MAX_IN_FLIGHT），1KB 级别的分配对内存无影响。
const OUT_BUF_SIZE: usize = MAX_PACKET_SIZE_HARD_LIMIT as usize;
/// IN 读取缓冲：必须 ≥ 设备上报的 packet_size，否则长响应会被截断
const IN_BUF_SIZE: usize = MAX_PACKET_SIZE_HARD_LIMIT as usize;

const SLOT_FREE: u8 = 0;
const SLOT_PENDING: u8 = 1;

/// Windows/MSVC 下 libc::timeval 为两个 32 位字段（c_long = i32）。
/// 用 64 位会错位：libusb 会把 tv_usec 读成 0，事件泵退化成忙轮询。
#[repr(C)]
struct Timeval {
    tv_sec: i32,
    tv_usec: i32,
}

/// 会话共享状态（回调与消费者之间）
struct SessionInner {
    /// 已完成的 IN 响应（按到达顺序）
    responses: Mutex<VecDeque<Vec<u8>>>,
    /// 新响应到达通知
    cv: Condvar,
    /// 致命错误（设备拔出等），首次出现后由消费者取走
    fatal: Mutex<Option<String>>,
    /// 是否允许回调重新挂起 IN transfer（Stop 前置 false）
    running: AtomicBool,
}

/// 一个 transfer 槽位
///
/// 缓冲由本结构持有，transfer 内部只存裸指针；槽位本身 Box 分配，
/// 地址稳定，回调通过 `user_data` 反查槽位。
struct Slot {
    xfer: *mut ffi::libusb_transfer,
    buf: Vec<u8>,
    state: AtomicU8,
    inner: *const SessionInner,
    ep: u8,
}

impl Slot {
    /// 配置（或重新配置）transfer 的静态字段
    unsafe fn configure(
        &self,
        handle: *mut ffi::libusb_device_handle,
        timeout_ms: u32,
        cb: ffi::libusb_transfer_cb_fn,
    ) {
        let t = self.xfer;
        (*t).dev_handle = handle;
        (*t).flags = 0;
        (*t).endpoint = self.ep;
        (*t).transfer_type = LIBUSB_TRANSFER_TYPE_BULK;
        (*t).timeout = timeout_ms;
        (*t).length = self.buf.len() as i32;
        (*t).buffer = self.buf.as_ptr() as *mut u8;
        (*t).callback = cb;
        (*t).user_data = self as *const Slot as *mut c_void;
    }
}

extern "system" fn out_callback(x: *mut ffi::libusb_transfer) {
    unsafe {
        let slot = &*((*x).user_data as *const Slot);
        let status = (*x).status;

        // OUT 失败意味着该命令的字节没有完整到达设备，它的响应永远不会
        // 出现。若静默丢弃，writes_per_cmd 队列会多出一条、与后续响应
        // 永久错位（切帧全乱）。因此除"取消"外一律升级为致命错误：
        // 消费者取到后上报 UI 并停止采集（与旧同步实现的"USB write
        // failed"语义一致）。
        if status != LIBUSB_TRANSFER_COMPLETED && status != LIBUSB_TRANSFER_CANCELLED {
            let inner = &*slot.inner;
            if let Ok(mut f) = inner.fatal.lock() {
                f.get_or_insert_with(|| {
                    format!("USB write failed (async transfer status={})", status)
                });
            }
            inner.cv.notify_all();
        }
        slot.state.store(SLOT_FREE, Ordering::SeqCst);
    }
}

extern "system" fn in_callback(x: *mut ffi::libusb_transfer) {
    unsafe {
        let slot = &*((*x).user_data as *const Slot);
        let inner = &*slot.inner;
        let status = (*x).status;

        if status == LIBUSB_TRANSFER_COMPLETED {
            let n = ((*x).actual_length.max(0) as usize).min(slot.buf.len());
            if n > 0 {
                let data = slot.buf[..n].to_vec();
                if let Ok(mut q) = inner.responses.lock() {
                    // 上限保护：消费者长时间停摆时不无限增长
                    if q.len() < 4096 {
                        q.push_back(data);
                    }
                }
                inner.cv.notify_one();
            }
        } else if status == LIBUSB_TRANSFER_NO_DEVICE {
            if let Ok(mut f) = inner.fatal.lock() {
                f.get_or_insert_with(|| "USB device disconnected".to_string());
            }
            inner.cv.notify_all();
        } else if status != LIBUSB_TRANSFER_CANCELLED && status != LIBUSB_TRANSFER_TIMED_OUT {
            log::warn!("async IN transfer failed: status={}", status);
        }

        // 重新挂起，保持 IN 端点始终有人收
        if inner.running.load(Ordering::SeqCst) && status != LIBUSB_TRANSFER_CANCELLED {
            (*x).length = slot.buf.len() as i32;
            if ffi::libusb_submit_transfer(x) != 0 {
                slot.state.store(SLOT_FREE, Ordering::SeqCst);
            }
        } else {
            slot.state.store(SLOT_FREE, Ordering::SeqCst);
        }
    }
}

/// 异步 Bulk 会话
///
/// 生命周期：`start()` 创建 → 采样期间 `submit()` / `next_response()` →
/// `stop()` 取消所有在途传输、回收槽位、结束事件泵。
pub struct AsyncSession {
    inner: Box<SessionInner>,
    /// 槽位必须 Box 分配：libusb transfer 的 `user_data` 保存槽位裸指针，
    /// 槽位地址在会话生命周期内必须稳定（Vec 重分配会让指针失效）。
    #[allow(clippy::vec_box)]
    out_slots: Vec<Box<Slot>>,
    #[allow(clippy::vec_box)]
    in_slots: Vec<Box<Slot>>,
    pump: Mutex<Option<JoinHandle<()>>>,
    /// 事件泵退出标志（与 inner.running 分开：取消传输期间仍需泵事件）
    pump_shutdown: Arc<AtomicBool>,
    stopped: AtomicBool,
}

// transfer 裸指针与槽位内裸指针只在会话生命周期内使用，
// 会话停止时保证事件泵已退出、所有传输已回收，故跨线程传递安全。
unsafe impl Send for AsyncSession {}
unsafe impl Sync for AsyncSession {}

impl AsyncSession {
    /// 创建异步会话并启动事件泵
    ///
    /// `out_pool`：在途 OUT 命令上限（= 原有 MAX_IN_FLIGHT 语义）
    /// `in_pool`：常驻挂起的 IN transfer 数（≥2 可平滑背靠背响应）
    pub fn start(
        usb: &Arc<BulkTransfer>,
        out_pool: usize,
        in_pool: usize,
    ) -> Result<Self> {
        let handle = usb.raw_handle();
        let ctx = usb.raw_context();
        if handle.is_null() || ctx.is_null() {
            return Err(Error::Other("USB handle unavailable for async session".into()));
        }

        let inner = Box::new(SessionInner {
            responses: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            fatal: Mutex::new(None),
            running: AtomicBool::new(true),
        });
        let inner_ptr: *const SessionInner = &*inner;

        let alloc_slots = |count: usize, ep: u8, len: usize| -> Result<Vec<Box<Slot>>> {
            let mut v: Vec<Box<Slot>> = Vec::with_capacity(count);
            for _ in 0..count {
                let xfer = unsafe { ffi::libusb_alloc_transfer(0) };
                if xfer.is_null() {
                    // 回收已分配的，避免泄漏
                    for s in &v {
                        unsafe { ffi::libusb_free_transfer(s.xfer) };
                    }
                    return Err(Error::Other("libusb_alloc_transfer failed".into()));
                }
                v.push(Box::new(Slot {
                    xfer,
                    buf: vec![0u8; len],
                    state: AtomicU8::new(SLOT_FREE),
                    inner: inner_ptr,
                    ep,
                }));
            }
            Ok(v)
        };

        let out_slots = alloc_slots(out_pool.max(1), usb.ep_out(), OUT_BUF_SIZE)?;
        let in_slots = alloc_slots(in_pool.max(1), usb.ep_in(), IN_BUF_SIZE)?;
        // 配置 OUT（暂不提交）与 IN（立即常驻挂起）
        for s in &out_slots {
            unsafe { s.configure(handle, 2000, out_callback) };
        }
        for s in &in_slots {
            unsafe { s.configure(handle, 1000, in_callback) };
            if unsafe { ffi::libusb_submit_transfer(s.xfer) } != 0 {
                log::warn!("failed to prime async IN transfer");
            } else {
                s.state.store(SLOT_PENDING, Ordering::SeqCst);
            }
        }

        // 事件泵线程：唯一调用 libusb_handle_events 的地方
        let pump_shutdown = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::clone(&pump_shutdown);
        let ctx_addr = ctx as usize;
        let pump = thread::Builder::new()
            .name("dap-usb-events".into())
            .spawn(move || {
                let ctx = ctx_addr as *mut ffi::libusb_context;
                while !shutdown.load(Ordering::SeqCst) {
                    let tv = Timeval { tv_sec: 0, tv_usec: 20_000 };
                    let rc = unsafe {
                        ffi::libusb_handle_events_timeout_completed(
                            ctx,
                            &tv as *const Timeval as *const _,
                            std::ptr::null_mut(),
                        )
                    };
                    if rc < 0 && rc != LIBUSB_ERROR_INTERRUPTED && rc != LIBUSB_ERROR_TIMEOUT {
                        log::warn!("libusb_handle_events returned {}", rc);
                    }
                }
            })
            .map_err(|e| Error::PipelineThread(e.to_string()))?;

        log::info!(
            "async USB session started (out_pool={}, in_pool={})",
            out_slots.len(),
            in_slots.len()
        );

        Ok(Self {
            inner,
            out_slots,
            in_slots,
            pump: Mutex::new(Some(pump)),
            pump_shutdown,
            stopped: AtomicBool::new(false),
        })
    }

    /// 提交一条命令
    ///
    /// 返回 `Ok(true)` = 已挂入空闲 OUT 槽位（设备侧稍后处理）；
    /// `Ok(false)` = 在途已满（设备跟不上），调用方应跳过本轮。
    pub fn submit(&self, data: &[u8]) -> Result<bool> {
        if self.stopped.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if data.len() > OUT_BUF_SIZE {
            return Err(Error::Other(format!(
                "command too large for async transfer: {} > {} bytes",
                data.len(),
                OUT_BUF_SIZE
            )));
        }

        for slot in &self.out_slots {
            if slot
                .state
                .compare_exchange(SLOT_FREE, SLOT_PENDING, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            // 槽位已被本线程独占：拷贝命令字节并调整长度后提交。
            // 只有 submit 线程会调用本方法，槽位在 PENDING 期间不会被
            // 任何人写入，故对 buf 的裸指针写入是安全的。
            unsafe {
                let dst = slot.buf.as_ptr() as *mut u8;
                std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
                (*slot.xfer).length = data.len() as i32;
                (*slot.xfer).buffer = dst;
            }
            let rc = unsafe { ffi::libusb_submit_transfer(slot.xfer) };
            if rc != 0 {
                slot.state.store(SLOT_FREE, Ordering::SeqCst);
                return Err(Error::Other(format!(
                    "libusb_submit_transfer failed: error {}",
                    rc
                )));
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// 等待下一条完整响应（IN transfer 完成的原始字节）
    ///
    /// 超时返回 `Ok(None)`；设备拔出等致命错误返回 `Err`。
    /// fatal 槽用 `peek` 语义（不清空）：提交线程同样需要感知该错误，
    /// 由谁先发现谁上报，另一个线程随后也会看到同一个原因。
    pub fn next_response(&self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(err) = self.inner.fatal.lock().ok().and_then(|f| f.clone()) {
                return Err(Error::Other(err));
            }
            {
                let mut q = match self.inner.responses.lock() {
                    Ok(q) => q,
                    Err(_) => return Ok(None),
                };
                if let Some(d) = q.pop_front() {
                    return Ok(Some(d));
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                // 等待新响应；唤醒后释放队列锁并重新检查致命错误
                let _ = self.inner.cv.wait_timeout(q, deadline - now);
            }
        }
    }

    /// 是否已出现致命错误（供提交线程轮询）
    pub fn fatal_error(&self) -> Option<String> {
        self.inner.fatal.lock().ok().and_then(|f| f.clone())
    }

    /// 停止会话：取消在途传输、回收槽位、结束事件泵
    ///
    /// 必须在所有使用本会话的线程退出后调用（引擎在 join 完
    /// 提交/接收线程后再调用）。
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        // 1. 禁止回调重新挂起 IN
        self.inner.running.store(false, Ordering::SeqCst);

        // 2. 取消所有在途传输（事件泵仍在运行，会派发 CANCELLED 回调）
        for s in self.out_slots.iter().chain(self.in_slots.iter()) {
            unsafe { ffi::libusb_cancel_transfer(s.xfer) };
        }

        // 3. 等所有槽位归还（回调把它们置 FREE），最多 2s
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let all_free = self
                .out_slots
                .iter()
                .chain(self.in_slots.iter())
                .all(|s| s.state.load(Ordering::SeqCst) == SLOT_FREE);
            if all_free {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        // 4. 结束事件泵（此后不会再有任何回调）
        self.pump_shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.pump.lock() {
            if let Some(h) = guard.take() {
                let _ = h.join();
            }
        }

        // 5. 释放 transfer 资源
        for s in self.out_slots.iter().chain(self.in_slots.iter()) {
            unsafe { ffi::libusb_free_transfer(s.xfer) };
        }
        log::info!("async USB session stopped");
    }
}

impl Drop for AsyncSession {
    fn drop(&mut self) {
        if !self.stopped.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}
