use rusb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use crate::error::*;
use crate::usb::device;

/// USB Bulk 传输封装
pub struct BulkTransfer {
    handle: rusb::DeviceHandle<rusb::Context>,
    ep_out: u8,
    ep_in: u8,
    timeout: Duration,
    /// 已 claim 的 USB 接口号，释放时需要 release_interface
    interface_num: u8,
    /// 是否已显式 release（避免 Drop 时重复释放）。
    /// 原子类型使 release() 可以只借 &self——经 Arc 直接调用，不再依赖
    /// Arc::try_unwrap（旧实现在还有其他 Arc 引用时静默跳过释放）。
    released: AtomicBool,
}

impl BulkTransfer {
    /// 创建 BulkTransfer 实例（自动发现并连接设备）
    pub fn open() -> Result<Self> {
        let (handle, interface_num) = device::open_first_device()?;
        let (ep_out, ep_in) = device::find_bulk_endpoints(&handle)?;
        log::info!("bulk endpoints: OUT=0x{:02X}, IN=0x{:02X}", ep_out, ep_in);
        let transfer = Self {
            handle,
            ep_out,
            ep_in,
            timeout: Duration::from_millis(1000),
            interface_num,
            released: AtomicBool::new(false),
        };
        // 清空 USB IN 端点缓冲区中可能残留的数据
        // （流水线非阻塞写入后可能残留未读响应）
        transfer.flush_input();
        Ok(transfer)
    }

    /// 显式释放 USB 接口和设备
    ///
    /// 返回 `true` 表示固件已确认 DAP_Disconnect（收到 0x03 回显）；
    /// `false` 表示未确认——部分厂家固件在流水线高负载停止后不处理
    /// 排队命令，调试会话残留，设备在其他工具（Keil/pyOCD 等）看来
    /// 仍处于占用状态，需要重新插拔恢复。
    ///
    /// 释放序列对齐 Keil 停止调试会话的行为（这是两个厂家 DAP-Link
    /// 在 Keil 下都能正常释放、在我们的应用下表现不一致的根源——
    /// Keil 只在会话空闲时断开，旧实现在流水线仍有在途响应时就断开，
    /// 且从不确认固件真的收到了 DAP_Disconnect）：
    ///
    /// 1. 排空 IN 端点全部残留响应（等待 100ms 静默，总上限 2s），
    ///    让固件先处理完在途 DAP_Transfer，断开命令不会被淹没
    /// 2. DAP_Disconnect 并**确认回显**，未确认则重试（最多 3 次）
    /// 3. DAP_HostStatus(connected=0, running=0) 关闭连接状态
    /// 4. release_interface
    pub fn release(&self) -> bool {
        if self.released.swap(true, Ordering::SeqCst) {
            return true;
        }
        let mut buf = [0u8; 1024];

        // 1. 排空 USB IN 端点的所有残留响应
        //    高速采样时可能有上千个未读响应堆积在缓冲区中；停止瞬间
        //    固件可能仍在执行在途命令（WAIT 重试），响应会晚到，因此
        //    以 100ms 无数据为静默判据，并设总上限防止固件异常时卡死
        let drain_deadline = Instant::now() + Duration::from_secs(2);
        let mut drained = 0usize;
        loop {
            match self.handle.read_bulk(self.ep_in, &mut buf, Duration::from_millis(100)) {
                Ok(n) if n > 0 => drained += n,
                _ => break,
            }
            if Instant::now() >= drain_deadline {
                log::warn!("drain hit 2s deadline, IN endpoint may still have pending responses");
                break;
            }
        }
        if drained > 0 {
            log::info!("drained {} stale USB bytes", drained);
        }

        // 2. 发送 DAP_Disconnect 命令，清理 CMSIS-DAP 连接状态。
        //    读到的回显必须是 0x03（命令码）——之前只读不验证，厂家
        //    固件丢掉这条命令时应用毫无感知，固件永远停在连接态。
        log::info!("sending DAP_Disconnect command");
        let disconnect_cmd = [crate::dap::commands::DAP_DISCONNECT];
        let mut confirmed = false;
        for attempt in 1..=3 {
            if let Err(e) = self.handle.write_bulk(self.ep_out, &disconnect_cmd, Duration::from_millis(200)) {
                log::warn!("DAP_Disconnect send failed (attempt {}): {}", attempt, e);
                continue;
            }
            // 跳过迟到的旧响应，直到读到 0x03 回显（上限 600ms）
            let echo_deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < echo_deadline {
                match self.handle.read_bulk(self.ep_in, &mut buf, Duration::from_millis(200)) {
                    Ok(n) if n > 0 && buf[0] == crate::dap::commands::DAP_DISCONNECT => {
                        confirmed = true;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            if confirmed {
                break;
            }
            log::warn!("DAP_Disconnect not confirmed (attempt {}), retrying", attempt);
        }
        if confirmed {
            log::info!("DAP_Disconnect confirmed by firmware");
        } else {
            log::warn!(
                "DAP_Disconnect NOT confirmed after 3 attempts: firmware may keep the debug \
                 session active and the device stays occupied for other tools until replug"
            );
        }

        // 3. 发送 DAP_HostStatus (connected=0, running=0)，关闭连接状态
        let host_status_cmd = [crate::dap::commands::DAP_LED, 0x00, 0x00];
        if self.handle.write_bulk(self.ep_out, &host_status_cmd, Duration::from_millis(200)).is_ok() {
            let _ = self.handle.read_bulk(self.ep_in, &mut buf, Duration::from_millis(200));
        }

        // 4. 释放 USB 接口
        log::info!("releasing USB interface {}", self.interface_num);
        if let Err(e) = self.handle.release_interface(self.interface_num) {
            log::warn!("release_interface failed: {}", e);
        }

        log::info!("DAP-Link released");
        confirmed
    }

    /// 设置超时
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Bulk 写（发送命令）
    pub fn write(&self, data: &[u8]) -> Result<usize> {
        log::debug!("USB OUT ({} bytes): {:02X?}", data.len(), data);
        let n = self.handle.write_bulk(self.ep_out, data, self.timeout)?;
        Ok(n)
    }

    /// Bulk 写（带 200ms 超时）
    ///
    /// 注意：rusb 中 timeout < 1ms 会无限阻塞。
    /// 此处使用 200ms 超时，确保提交线程在收到停止标志后能及时退出，
    /// 避免高采样率下 write 永久阻塞导致 join 卡死、USB 设备无法释放。
    pub fn write_nonblock(&self, data: &[u8]) -> Result<usize> {
        let n = self.handle.write_bulk(self.ep_out, data, Duration::from_millis(200))?;
        Ok(n)
    }

    /// Bulk 读（接收响应）
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let n = self.handle.read_bulk(self.ep_in, buf, self.timeout)?;
        log::debug!("USB IN: {} bytes", n);
        Ok(n)
    }

    /// Bulk 读（可指定超时）
    pub fn read_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let n = self.handle.read_bulk(self.ep_in, buf, timeout)?;
        Ok(n)
    }

    /// 清空 USB IN 端点缓冲区中的残留数据
    ///
    /// 流水线非阻塞写入后，IN 端点可能残留未读响应。
    /// 新连接时调用此方法可避免读到陈旧数据导致协议失步。
    pub fn flush_input(&self) {
        let mut buf = [0u8; 1024];
        let mut flushed = 0;
        while let Ok(n) = self.handle.read_bulk(self.ep_in, &mut buf, Duration::from_millis(10)) {
            flushed += n;
        }
        if flushed > 0 {
            log::info!("cleared {} bytes of stale USB buffer data", flushed);
        }
    }
}

// rusb::DeviceHandle 是 Send + Sync 的，所以 BulkTransfer 也可以
unsafe impl Send for BulkTransfer {}
unsafe impl Sync for BulkTransfer {}

impl Drop for BulkTransfer {
    /// 兜底释放：如果未显式调用 release()，Drop 时自动执行。
    ///
    /// 这确保即使调用方遗漏显式释放（或仍有其他 Arc 引用），
    /// USB 接口也能在最后一个引用释放时被正确释放，避免 DAP-Link 卡死。
    fn drop(&mut self) {
        if !self.released.load(Ordering::SeqCst) {
            log::warn!("BulkTransfer not explicitly released, auto-releasing on Drop");
            let _ = self.release();
        }
    }
}
