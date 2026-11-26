use log::{info, warn};
use crate::error::*;
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::commands::*;
use crate::dap::mem_access::MemAccess;

/// SWD 链路层：封装 SWD 初始化和内存访问
pub struct SwdLink {
    usb: BulkTransfer,
    dap: DapProtocol,
    ap_index: u8,
}

/// 连接后获取的设备信息
#[derive(Debug)]
pub struct DeviceInfo {
    pub dpidr: u32,
    pub ap_idr: u32,
    pub target_info: String,
}

impl SwdLink {
    /// 创建 SWD 链路（自动连接 USB 设备）
    pub fn new() -> Result<Self> {
        let usb = BulkTransfer::open()?;
        let dap = DapProtocol::new();
        Ok(Self { usb, dap, ap_index: 0 })
    }

    /// 获取 USB 传输层引用
    pub fn usb(&self) -> &BulkTransfer {
        &self.usb
    }

    /// 获取协议层引用
    pub fn dap(&self) -> &DapProtocol {
        &self.dap
    }

    /// 消费 SwdLink，提取底层 USB 传输和协议处理器
    /// 用于流水线引擎接管已初始化的 SWD 链路
    pub fn into_parts(self) -> (BulkTransfer, DapProtocol) {
        (self.usb, self.dap)
    }

    // --------------------------------------------------------
    // SWD 初始化流程
    // --------------------------------------------------------
    /// 完整的 SWD 初始化序列（精确匹配 Keil USB 抓包流程）
    ///
    /// `swd_clock_hz` 为目标 SWD 时钟频率（Hz），常用值：1M/2M/5M/10M/20M/30M/50M。
    pub fn init_with_clock(&mut self, swd_clock_hz: u32) -> Result<DeviceInfo> {
        info!("=== SWD init start ===");

        // 0. 先断开之前可能残留的连接（清理 DAP-Link 状态）
        let mut buf = [0u8; 64];
        self.usb.write(&[DAP_DISCONNECT])?;
        let _ = self.usb.read(&mut buf);
        std::thread::sleep(std::time::Duration::from_millis(100));

        // 1. 查询 DAP-Link 信息
        let dap_info = self.dap.query_info(self.usb())?;
        info!("DAP-Link: {} {} v{}",
            dap_info.vendor, dap_info.product, dap_info.fw_version);
        // 记录设备包大小/缓冲数：采集引擎据此分包与限制在途命令。
        // 不记录就会按 64 字节硬编码，超过固件包大小的命令会被静默截断。
        self.usb.set_packet_info(dap_info.packet_size, dap_info.packet_count);

        // 2. SWD 连接
        self.swd_connect()?;

        // 3. 设置 SWD 时钟（由调用方指定，单位 Hz）
        let clock = swd_clock_hz;
        let cmd = self.dap.build_clock_request(clock);
        self.usb.write(&cmd)?;
        let n = self.usb.read(&mut buf)?;
        DapProtocol::parse_clock_response(&buf[..n])?;
        info!("SWD clock: {} MHz", clock as f32 / 1_000_000.0);

        // 4. DAP_TransferConfigure (idle=0, wait_retry=100, match_retry=0)
        let cfg_cmd = DapProtocol::build_transfer_configure_request(0, 100, 0);
        self.usb.write(&cfg_cmd)?;
        let n = self.usb.read(&mut buf)?;
        DapProtocol::parse_transfer_configure_response(&buf[..n])?;

        // 5. DAP_SWD_Configure (turnaround=1, 无 dataPhase)
        let swd_cfg = DapProtocol::build_swd_configure_request(0x00);
        self.usb.write(&swd_cfg)?;
        let n = self.usb.read(&mut buf)?;
        DapProtocol::parse_swd_configure_response(&buf[..n])?;

        // 6. DAP_HostStatus (Connect LED on)
        let host_cmd = DapProtocol::build_host_status_request(0, 1);
        self.usb.write(&host_cmd)?;
        let n = self.usb.read(&mut buf)?;
        DapProtocol::parse_host_status_response(&buf[..n])?;

        // 7. 显式发送 SWJ line reset + JTAG-to-SWD + line reset。
        // 某些定制 CMSIS-DAP（如 HW-Link_META）不会在 DAP_Connect 后自动完成
        // 目标侧 SWD 序列初始化，直接读 DPIDR 会返回 DAP_TRANSFER_PROTOCOL_ERROR。
        self.swd_line_reset()?;

        // 7b. SWD idle 周期：在 SWJ 序列结束后发送 8 个 SWCLK 周期（TMS=0），
        // 让目标侧 SWD 状态机从复位状态退出到 idle。某些定制 DAP-Link（如 HW-Link_META）
        // 在缺少这段 idle 时第一个 DAP_Transfer 会返回 protocol error (status=7)。
        // 此序列通过对齐 Keil USB 抓包验证。
        self.swd_idle_cycles(8)?;

        // 8. 读取 DPIDR
        let dpidr = self.read_dpidr()?;
        info!("DPIDR = 0x{:08X}", dpidr);

        // 8b. 两次 DPIDR 读取之间也需要 idle（对齐 Keil）
        self.swd_idle_cycles(8)?;

        // 9. 再次读取 DPIDR（确认连接稳定）
        let _ = self.read_dpidr()?;

        // 10. 调试电源上电（写 CTRL/STAT = CSYSPWRUPREQ | CDBGPWRUPREQ）
        self.power_up_debug()?;

        // 11. 写 DP SELECT = 0（APSEL=0、APBANKSEL=0）
        self.select_ap(0, 0)?;

        // 11b. 配置 AHB-AP CSW：32 位传输 + privileged + debug master
        //
        // CSW.SIZE 复位值是 0（8 位访问）：不配置的话写 32 位变量只会
        // 更新低字节，读 32 位依赖 AHB-AP 的未定义行为（多数实现会回绕
        // 到对齐字，但写一定只写 8 位）。这一步必须在任何内存访问之前。
        self.configure_ahb_ap_csw()?;

        // 12. 扫描 AP
        let ap_idr = self.scan_ap()?;
        info!("AP{} IDR = 0x{:08X}", self.ap_index, ap_idr);

        // 13. 验证内存读取（读向量表地址 0x00000000）
        match self.read_memory(0x00000000) {
            Ok(v) => info!("verify read (0x00000000) = 0x{:08X} ✓", v),
            Err(e) => warn!("verify read failed: {}", e),
        }

        info!("=== SWD init done ===");
        Ok(DeviceInfo {
            dpidr,
            ap_idr,
            target_info: format!("Cortex-M DPIDR=0x{:08X}", dpidr),
        })
    }

    /// 使用默认 1 MHz 时钟进行 SWD 初始化（向后兼容包装）
    pub fn init(&mut self) -> Result<DeviceInfo> {
        self.init_with_clock(1_000_000)
    }

    /// SWD 连接
    fn swd_connect(&self) -> Result<()> {
        let cmd = self.dap.build_connect_request(CONNECT_MODE_SWD);
        info!("DAP_Connect request: {:02X?}", cmd);
        self.usb.write(&cmd)?;
        let mut buf = [0u8; 64];
        let n = self.usb.read(&mut buf)?;
        info!("DAP_Connect response ({} bytes): {:02X?}", n, &buf[..n]);
        let mode = DapProtocol::parse_connect_response(&buf[..n])?;
        info!("DAP_Connect returned: {}", mode);
        Ok(())
    }

    /// SWD 线复位 + JTAG-to-SWD 切换
    /// SWD 线复位（精确匹配 Keil 抓包序列：Line Reset + JTAG-to-SWD + Line Reset）
    fn swd_line_reset(&self) -> Result<()> {
        info!("sending SWD init sequence...");
        let mut buf = [0u8; 64];

        // 1. Line Reset: 51 bits TMS=1 (Keil: 12 33 FF FF FF FF FF FF FF)
        let cmd1 = vec![DAP_SWJ_SEQUENCE, 51, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        self.usb.write(&cmd1)?;
        let n = self.usb.read(&mut buf)?;
        info!("SWJ Line Reset response ({} bytes): {:02X?}", n, &buf[..n]);

        // 2. JTAG-to-SWD 切换: 16 bits (Keil: 12 10 9E E7)
        let cmd2 = vec![DAP_SWJ_SEQUENCE, 16, 0x9E, 0xE7];
        self.usb.write(&cmd2)?;
        let n = self.usb.read(&mut buf)?;
        info!("SWJ JTAG-to-SWD response ({} bytes): {:02X?}", n, &buf[..n]);

        // 3. Line Reset: 51 bits TMS=1 (Keil: 12 33 FF FF FF FF FF FF FF)
        let cmd3 = vec![DAP_SWJ_SEQUENCE, 51, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        self.usb.write(&cmd3)?;
        let n = self.usb.read(&mut buf)?;
        info!("SWJ Line Reset 2 response ({} bytes): {:02X?}", n, &buf[..n]);

        info!("SWD sequence done");
        Ok(())
    }

    /// 发送 N 个 SWCLK 周期，TMS/SWDIO 保持低电平（SWD idle 周期）。
    ///
    /// ARM SWD 协议要求在 JTAG-to-SWD 切换后至少有 2 个 SWCLK 周期
    /// 让目标侧 DP 从复位状态进入 idle 状态。某些定制 DAP-Link/HW-Link 需
    /// 更长的 idle（8 个周期），否则第一次 DAP_Transfer 会返回 protocol error。
    fn swd_idle_cycles(&self, cycles: u8) -> Result<()> {
        let cmd = vec![DAP_SWJ_SEQUENCE, cycles, 0x00];
        self.usb.write(&cmd)?;
        let mut buf = [0u8; 64];
        let _ = self.usb.read(&mut buf)?;
        Ok(())
    }

    /// 读取 DPIDR 寄存器
    fn read_dpidr(&self) -> Result<u32> {
        let req = TransferRequest::read_dp(DP_REG_DPIDR);
        let resp = self.dap.execute_transfer(self.usb(), &[req])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }
        Ok(resp.data.first().copied().unwrap_or(0))
    }

    /// 调试电源上电
    ///
    /// 不需要读 RDBUFF 冲刷流水线：DAPLink 固件的 `DAP_SWD_Transfer`
    /// 对 AP 读会先 post 再读 DP_RDBUFF 取出数据后才组响应，DP 读直接
    /// 返回；一条 `DAP_Transfer` 结束时 RDBUFF 里不会残留待取数据。
    fn power_up_debug(&self) -> Result<()> {
        // 清除残留的 sticky 错误（STICKYERR/STICKYORUN/WDATAERR/STICKYCMP），
        // 否则上一次会话遗留的标志会让本次所有 DP/AP 访问持续返回 FAULT。
        let abort_req = TransferRequest::write_dp(DP_REG_ABORT, DP_ABORT_CLEAR_STICKY);
        let abort_resp = self.dap.execute_transfer(self.usb(), &[abort_req])?;
        if abort_resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(abort_resp.status, abort_resp.count));
        }

        // 请求系统调试电源（写 CTRL/STAT）
        let req1 = TransferRequest::write_dp(DP_REG_CTRL_STAT, CSYSPWRUPREQ | CDBGPWRUPREQ);
        let resp = self.dap.execute_transfer(self.usb(), &[req1])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::Swd(format!("debug power-up request failed: status={}, count={}", resp.status, resp.count)));
        }

        // 读 CTRL/STAT 确认电源就绪
        // 先校验 status：FAULT 时 data 无意义，直接判 ACK 位会报成
        // 「电源没起来」，掩盖真正的总线访问失败
        let req2 = TransferRequest::read_dp(DP_REG_CTRL_STAT);
        let resp = self.dap.execute_transfer(self.usb(), &[req2])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }
        let ctrl_stat = resp.values().first().copied().unwrap_or(0);

        if (ctrl_stat & (CSYSPWRUPACK | CDBGPWRUPACK)) != (CSYSPWRUPACK | CDBGPWRUPACK) {
            return Err(Error::Swd(format!("debug power not ready: CTRL/STAT=0x{:08X}", ctrl_stat)));
        }
        info!("debug power ready: CTRL/STAT=0x{:08X}", ctrl_stat);
        Ok(())
    }

    /// 选择 AP 端口及其寄存器 bank
    ///
    /// DP SELECT: APSEL=[31:24]、APBANKSEL=[7:4]。请求字节只带 A[3:2]，
    /// A[7:4] 全靠 APBANKSEL，所以访问 bank 0 以外的寄存器必须先切。
    fn select_ap(&self, ap: u8, bank: u8) -> Result<()> {
        let req = TransferRequest::write_dp(DP_REG_SELECT, dp_select_value(ap, bank));
        let resp = self.dap.execute_transfer(self.usb(), &[req])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::Swd(format!("failed to select AP{} bank{}", ap, bank)));
        }
        Ok(())
    }

    /// 配置 AHB-AP 的 CSW 为 32 位访问
    ///
    /// 前置条件：DP SELECT 已指向 APSEL=0、APBANKSEL=0（init 流程先写
    /// SELECT=0 再调用本函数；scan_ap 结束后也会还原 bank 0）。
    /// 写后读回校验：某些 AP 不支持某些位组合，读回不符时明确告警
    /// （否则后续所有 32 位读写都会静默只作用于低 8 位）。
    fn configure_ahb_ap_csw(&self) -> Result<()> {
        let req = TransferRequest::write_ap(AP_REG_CSW, AHB_AP_CSW_32BIT);
        let resp = self.dap.execute_transfer(self.usb(), &[req])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::Swd(format!(
                "failed to configure AHB-AP CSW: status={}, count={}",
                resp.status, resp.count
            )));
        }

        // 读回校验 SIZE 字段（bits[2:0]）
        let read_req = TransferRequest::read_ap(AP_REG_CSW);
        let resp = self.dap.execute_transfer(self.usb(), &[read_req])?;
        if resp.status != TRANSFER_OK {
            return Err(Error::Swd(format!(
                "failed to read back AHB-AP CSW: status={}, count={}",
                resp.status, resp.count
            )));
        }
        let csw = resp.values().first().copied().unwrap_or(0);
        if csw & 0x7 != 0x2 {
            return Err(Error::Swd(format!(
                "AHB-AP rejected 32-bit CSW (read back 0x{:08X}): memory access would be limited to 8-bit, aborting",
                csw
            )));
        }
        info!("AHB-AP CSW = 0x{:08X} (32-bit transfers)", csw);
        Ok(())
    }

    /// 扫描 AP 端口
    ///
    /// IDR 位于 AP 空间偏移 0xFC（bank 0xF + 页内 0xC），必须先把
    /// DP SELECT.APBANKSEL 切到 0xF；读完立刻还原 bank 0，因为后续
    /// CSW/TAR/DRW 访问全部依赖 bank 0。
    fn scan_ap(&mut self) -> Result<u32> {
        self.select_ap(0, AP_BANK_IDR)?;

        let req = TransferRequest::read_ap(AP_REG_IDR);
        let resp = self.dap.execute_transfer(self.usb(), &[req])?;

        if resp.status != TRANSFER_OK || resp.len == 0 {
            // 还原 bank 0 再返回错误，避免上层拿到一个 bank 停在 0xF 的链接
            let _ = self.select_ap(0, 0);
            return Err(Error::Swd("AP scan failed".into()));
        }

        let idr = resp.values()[0];
        // 还原 bank 0：下面的 CSW/TAR/DRW 访问都假定 APBANKSEL=0
        self.select_ap(0, 0)?;

        // IDR 全 0 或全 1 说明 AP 不存在 / 总线错误
        if idr == 0 || idr == u32::MAX {
            log::warn!("AP{} IDR = 0x{:08X} looks invalid", self.ap_index, idr);
        }

        self.ap_index = 0;
        Ok(idr)
    }

    // --------------------------------------------------------
    // 内存读取
    // --------------------------------------------------------
    /// 读取 32 位内存值（单地址）
    pub fn read_memory(&self, address: u32) -> Result<u32> {
        // 确保 AP 已选中
        // 两条请求打包到一次 DAP_Transfer:
        // 1. 写 AP TAR = address
        // 2. 读 AP DRW → 获取数据
        let requests = [
            TransferRequest::write_ap(AP_REG_TAR, address),
            TransferRequest::read_ap(AP_REG_DRW),
        ];

        let resp = self.dap.execute_transfer(self.usb(), &requests)?;

        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }

        // response.data 只包含读操作的结果
        Ok(resp.data.first().copied().unwrap_or(0))
    }

    /// 批量读取多个地址
    ///
    /// 按设备 packet_size 分批：全速 DAPLink 固件只有 64 字节包，
    /// 8 个变量一次读需要 83 字节命令，会被静默截断。
    pub fn read_memory_batch(&self, addresses: &[u32]) -> Result<Vec<u32>> {
        let packet_size = self.usb.packet_size() as usize;
        // 每字 2 条请求 × 5 字节 + 3 字节头
        let max_words = packet_size
            .saturating_sub(3)
            .checked_div(2 * 5)
            .unwrap_or(0)
            .max(1);
        let mut out = Vec::with_capacity(addresses.len());
        for chunk in addresses.chunks(max_words) {
            let mut requests = Vec::with_capacity(chunk.len() * 2);
            for &addr in chunk {
                requests.push(TransferRequest::write_ap(AP_REG_TAR, addr));
                requests.push(TransferRequest::read_ap(AP_REG_DRW));
            }
            let resp = self.dap.execute_transfer(self.usb(), &requests)?;
            if resp.status != TRANSFER_OK {
                return Err(Error::TransferFailed(resp.status, resp.count));
            }
            out.extend_from_slice(resp.values());
        }
        Ok(out)
    }

    /// 写入 32 位内存值（单地址）
    ///
    /// 通过 SWD 向目标地址写入一个 32 位值：
    /// 1. 写 AP TAR = address（设置目标地址）
    /// 2. 写 AP DRW = data（写入数据）
    pub fn write_memory(&self, address: u32, data: u32) -> Result<()> {
        let requests = [
            TransferRequest::write_ap(AP_REG_TAR, address),
            TransferRequest::write_ap(AP_REG_DRW, data),
        ];

        let resp = self.dap.execute_transfer(self.usb(), &requests)?;

        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }

        Ok(())
    }

    /// 写入一个变量（按字节大小 1/2/4/8）
    ///
    /// AHB-AP 不支持非对齐访问，也没有 64 位访问：
    /// - 4 字节对齐的 32 位变量：直接写整字
    /// - 4 字节对齐的 64 位变量：拆成低/高两个 32 位字写
    /// - 其余情况（子字 / 非对齐 32 位 / 非对齐 64 位）：读回对齐窗口
    ///   后按字节拼接再写回，不破坏相邻变量
    ///
    /// 每个字之前都重写 TAR，不依赖 CSW.AddrInc 自增。
    pub fn write_variable(&self, address: u32, byte_size: u32, value: u64) -> Result<()> {
        let acc = MemAccess::from_addr_size(address, byte_size);

        // 4 字节对齐的 32 位整字：直接写，无需旧值
        if acc.shift == 0 && acc.byte_size == 4 {
            return self.write_memory(acc.aligned_addr, value as u32);
        }
        // 4 字节对齐的 64 位变量：拆成低/高两个 32 位字
        if acc.byte_size == 8 && acc.words == 2 {
            let (low, high) = MemAccess::split_wide(value);
            let requests = [
                TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr),
                TransferRequest::write_ap(AP_REG_DRW, low),
                TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr.wrapping_add(4)),
                TransferRequest::write_ap(AP_REG_DRW, high),
            ];
            let resp = self.dap.execute_transfer(self.usb(), &requests)?;
            if resp.status != TRANSFER_OK {
                return Err(Error::TransferFailed(resp.status, resp.count));
            }
            return Ok(());
        }

        // 子字 / 非对齐变量：读回对齐窗口 → 按字节拼接 → 写回
        let words = acc.words as usize;
        let mut requests = Vec::with_capacity(words * 4);
        for i in 0..words as u32 {
            let addr = acc.aligned_addr.wrapping_add(4 * i);
            requests.push(TransferRequest::write_ap(AP_REG_TAR, addr));
            requests.push(TransferRequest::read_ap(AP_REG_DRW));
        }
        let resp = self.dap.execute_transfer(self.usb(), &requests)?;
        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }
        let read = resp.values();
        if read.len() < words {
            return Err(Error::Other(format!(
                "read-modify-write for 0x{:08X} got {} words, expected {}",
                address,
                read.len(),
                words
            )));
        }
        let mut window = [0u32; 3];
        window[..words].copy_from_slice(&read[..words]);
        acc.splice_into(&mut window, value);

        let mut writes = Vec::with_capacity(words * 4);
        for (i, &w) in window.iter().take(words).enumerate() {
            let addr = acc.aligned_addr.wrapping_add(4 * i as u32);
            writes.push(TransferRequest::write_ap(AP_REG_TAR, addr));
            writes.push(TransferRequest::write_ap(AP_REG_DRW, w));
        }
        let resp = self.dap.execute_transfer(self.usb(), &writes)?;
        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }
        Ok(())
    }

    /// 读取内存并解析为 float
    pub fn read_float(&self, address: u32) -> Result<f32> {
        let raw = self.read_memory(address)?;
        Ok(f32::from_bits(raw))
    }
}

/// 在**已初始化的 SWD 会话**上写入一个变量（按字节大小 1/2/4/8）
///
/// 与 [`SwdLink::write_variable`] 相同的写入逻辑（子字读-改-写、64 位拆字），
/// 但不拥有连接——不重连、不重新初始化。供 Pause 保持 USB 连接时的 Watch
/// 写入复用：重连的初始化序列会让目标 MCU 复位。
///
/// 调用前提：该会话已完成 [`SwdLink::init_with_clock`]（DP 已上电、AP 已
/// 选中）；写入请求自带 TAR 设置，自包含，不依赖会话的 TAR/AP 状态。
pub fn write_variable_on_session(
    dap: &DapProtocol,
    usb: &BulkTransfer,
    address: u32,
    byte_size: u32,
    value: u64,
) -> Result<()> {
    let execute = |requests: &[TransferRequest]| -> Result<()> {
        let resp = dap.execute_transfer(usb, requests)?;
        if resp.status != TRANSFER_OK {
            return Err(Error::TransferFailed(resp.status, resp.count));
        }
        Ok(())
    };
    let acc = MemAccess::from_addr_size(address, byte_size);

    // 4 字节对齐的 32 位整字：直接写
    if acc.shift == 0 && acc.byte_size == 4 {
        return execute(&[
            TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr),
            TransferRequest::write_ap(AP_REG_DRW, value as u32),
        ]);
    }
    // 4 字节对齐的 64 位变量：拆成低/高两个 32 位字
    if acc.byte_size == 8 && acc.words == 2 {
        let (low, high) = MemAccess::split_wide(value);
        return execute(&[
            TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr),
            TransferRequest::write_ap(AP_REG_DRW, low),
            TransferRequest::write_ap(AP_REG_TAR, acc.aligned_addr.wrapping_add(4)),
            TransferRequest::write_ap(AP_REG_DRW, high),
        ]);
    }

    // 子字 / 非对齐变量：读回对齐窗口 → 按字节拼接 → 写回
    let words = acc.words as usize;
    let mut reads = Vec::with_capacity(words * 2);
    for i in 0..words as u32 {
        reads.push(TransferRequest::write_ap(
            AP_REG_TAR,
            acc.aligned_addr.wrapping_add(4 * i),
        ));
        reads.push(TransferRequest::read_ap(AP_REG_DRW));
    }
    let resp = dap.execute_transfer(usb, &reads)?;
    if resp.status != TRANSFER_OK {
        return Err(Error::TransferFailed(resp.status, resp.count));
    }
    let read = resp.values();
    if read.len() < words {
        return Err(Error::Other(format!(
            "read-modify-write for 0x{:08X} got {} words, expected {}",
            address,
            read.len(),
            words
        )));
    }
    let mut window = [0u32; 3];
    window[..words].copy_from_slice(&read[..words]);
    acc.splice_into(&mut window, value);

    let mut writes = Vec::with_capacity(words * 2);
    for (i, &w) in window.iter().take(words).enumerate() {
        writes.push(TransferRequest::write_ap(
            AP_REG_TAR,
            acc.aligned_addr.wrapping_add(4 * i as u32),
        ));
        writes.push(TransferRequest::write_ap(AP_REG_DRW, w));
    }
    execute(&writes)
}
