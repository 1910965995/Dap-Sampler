// ============================================================
// DAP 命令码
// ============================================================
pub const DAP_INFO: u8 = 0x00;
pub const DAP_LED: u8 = 0x01;        // DAP_HostStatus / DAP_LED
pub const DAP_CONNECT: u8 = 0x02;
pub const DAP_DISCONNECT: u8 = 0x03;
pub const DAP_TRANSFER_CONFIGURE: u8 = 0x04;
pub const DAP_TRANSFER: u8 = 0x05;
pub const DAP_SWJ_CLOCK: u8 = 0x11;
pub const DAP_SWJ_PINS: u8 = 0x10;
pub const DAP_SWJ_SEQUENCE: u8 = 0x12;
pub const DAP_SWD_CONFIGURE: u8 = 0x13;

// ============================================================
// DAP_Info 请求 ID
// ============================================================
pub const INFO_ID_VENDOR: u8 = 0x01;
pub const INFO_ID_PRODUCT: u8 = 0x02;
pub const INFO_ID_SER_NUM: u8 = 0x03;
pub const INFO_ID_FW_VER: u8 = 0x04;
pub const INFO_ID_DEVICE_VENDOR: u8 = 0x05;
pub const INFO_ID_DEVICE_NAME: u8 = 0x06;
pub const INFO_ID_CAPABILITIES: u8 = 0xF0;
pub const INFO_ID_PACKET_COUNT: u8 = 0xFE;
pub const INFO_ID_PACKET_SIZE: u8 = 0xFF;

/// DAP_Connect 模式
/// DAP_Connect 模式（CMSIS-DAP 标准定义）
pub const CONNECT_MODE_AUTO: u8 = 0x00;  // 自动检测
pub const CONNECT_MODE_SWD: u8 = 0x01;   // 强制 SWD
pub const CONNECT_MODE_JTAG: u8 = 0x02;  // 强制 JTAG

/// DAP_Transfer 响应状态码（CMSIS-DAP v2 标准定义）
pub const TRANSFER_OK: u8 = 0x01;      // DAP_TRANSFER_OK
pub const TRANSFER_WAIT: u8 = 0x02;    // DAP_TRANSFER_WAIT
pub const TRANSFER_FAULT: u8 = 0x04;   // DAP_TRANSFER_FAULT
pub const TRANSFER_PROTOCOL_ERR: u8 = 0x07; // SWD protocol error

// ============================================================
// SWD 请求字节编码（CMSIS-DAP v2 标准，参考 Keil 抓包验证）
//
// Bit 0: APnDP   (0=DP, 1=AP)      ← 注意：APnDP 在 bit 0！
// Bit 1: RnW     (0=写, 1=读)      ← 注意：RnW 在 bit 1！
// Bit 2: A2      (寄存器地址 bit 2)
// Bit 3: A3      (寄存器地址 bit 3)
// ============================================================

/// 构建 SWD 请求字节（已通过 Keil USB 抓包验证）
/// 位编码: [A3, A2, RnW, APnDP] 从高到低
pub fn make_request(rnw: bool, apndp: bool, a2: bool, a3: bool) -> u8 {
    let mut val = 0u8;
    if apndp { val |= 1 << 0; }  // APnDP 在 bit 0
    if rnw { val |= 1 << 1; }    // RnW 在 bit 1
    if a2 { val |= 1 << 2; }     // A2 在 bit 2
    if a3 { val |= 1 << 3; }     // A3 在 bit 3
    val
}

/// 从寄存器地址提取位
fn reg_a2(addr: u8) -> bool { (addr >> 2) & 0x01 != 0 }
fn reg_a3(addr: u8) -> bool { (addr >> 3) & 0x01 != 0 }

// ============================================================
// DP 寄存器地址
// ============================================================
/// DP 寄存器地址: bits[3:2] = A3,A2
pub const DP_REG_DPIDR: u8 = 0x00;
/// DP ABORT 寄存器（只写），与 DPIDR 同地址 0x00，靠 RnW 区分读写
pub const DP_REG_ABORT: u8 = 0x00;
pub const DP_REG_CTRL_STAT: u8 = 0x04;
pub const DP_REG_SELECT: u8 = 0x08;
pub const DP_REG_RDBUFF: u8 = 0x0C;

/// DP ABORT 写值：清除全部 sticky 错误标志
///
/// - DAPABORT(0x01) 不置位——那是主动中止传输，不是清错
/// - STKCMPCLR(0x02)：清 STICKYCMP
/// - STKERRCLR(0x04)：清 STICKYERR
/// - WDERRCLR(0x08)：清 WDATAERR
/// - ORUNERRCLR(0x10)：清 STICKYORUN
///
/// 调试会话残留的 sticky 标志会让后续所有 DP/AP 访问持续返回 FAULT，
/// 必须在配置 AP 之前清掉（OpenOCD `swd_clear_sticky_errors()` 同值）。
pub const DP_ABORT_CLEAR_STICKY: u32 = 0x1E;

/// 构造 DP SELECT 写值（ADIv5）：APSEL=[31:24]，APBANKSEL=[7:4]
///
/// DAP_Transfer 请求字节只能携带 A[3:2] 两位（见 `make_request`），
/// AP 地址空间的 A[7:4] 必须经由 APBANKSEL 下发。访问 IDR(0xFC) 这类
/// 高 bank 寄存器前必须先切 bank。
pub fn dp_select_value(ap: u8, bank: u8) -> u32 {
    ((ap as u32) << 24) | (((bank as u32) & 0x0F) << 4)
}

/// 读 DP 寄存器请求字节
pub fn req_read_dp(addr: u8) -> u8 {
    make_request(true, false, reg_a2(addr), reg_a3(addr))
}

/// 写 DP 寄存器请求字节
pub fn req_write_dp(addr: u8) -> u8 {
    make_request(false, false, reg_a2(addr), reg_a3(addr))
}

// ============================================================
// AP 寄存器地址
// ============================================================
pub const AP_REG_CSW: u8 = 0x00;
pub const AP_REG_TAR: u8 = 0x04;
pub const AP_REG_DRW: u8 = 0x0C;
/// AP Identification Register（只读），位于 AP 空间偏移 **0xFC**
///
/// 0xFC = bank 0xF + 页内偏移 0xC：请求字节只能带 A[3:2]，所以常量取
/// 页内偏移 0x0C，bank 部分另由 [`AP_BANK_IDR`] 经 DP SELECT 下发。
///
/// 只写 0x0C 而不切 bank 是最容易犯的错：那会落到 bank 0 的 DRW，
/// 读回的是 TAR 指向的内存内容，AP 扫描「成功」是假阳性。
pub const AP_REG_IDR: u8 = 0x0C;

/// AP IDR 所在的 APBANKSEL（0xFC 的高 4 位）
pub const AP_BANK_IDR: u8 = 0x0F;

/// AHB-AP CSW 配置值（32 位传输 + privileged + debug master）
///
/// - SIZE = 0b010（32 位访问）
/// - AddrInc = 0（每次访问前由软件重写 TAR，不依赖自增）
/// - HPROT[1] = 1（privileged，bit25）
/// - MasterType = 1（debug，bit29）
///
/// CSW.SIZE 复位值是 0（8 位）：不配置就写 32 位变量只会更新低字节，
/// 读 32 位也依赖 AHB-AP 的未定义行为。
pub const AHB_AP_CSW_32BIT: u32 = 0x2300_0002;

/// 读 AP 寄存器请求字节
pub fn req_read_ap(addr: u8) -> u8 {
    make_request(true, true, reg_a2(addr), reg_a3(addr))
}

/// 写 AP 寄存器请求字节
pub fn req_write_ap(addr: u8) -> u8 {
    make_request(false, true, reg_a2(addr), reg_a3(addr))
}

// ============================================================
// DP CTRL/STAT 寄存器位定义（ARM ADI v5 规范）
// ============================================================
pub const CSYSPWRUPREQ: u32 = 1 << 30;   // System power-up request
pub const CDBGPWRUPREQ: u32 = 1 << 28;   // Debug power-up request
pub const CSYSPWRUPACK: u32 = 1 << 31;   // System power-up acknowledge (只读)
pub const CDBGPWRUPACK: u32 = 1 << 29;   // Debug power-up acknowledge (只读)
pub const CDBGRSTREQ: u32 = 1 << 26;     // Debug reset request
pub const CDBGRSTACK: u32 = 1 << 27;     // Debug reset acknowledge (只读)

// ============================================================
// Transfer 请求/响应 结构体
// ============================================================

/// Transfer 请求描述
#[derive(Debug, Clone)]
pub struct TransferRequest {
    pub rnw: bool,
    pub apndp: bool,
    pub reg_addr: u8,
    pub write_data: Option<u32>,
}

impl TransferRequest {
    pub fn read_dp(reg_addr: u8) -> Self {
        Self { rnw: true, apndp: false, reg_addr, write_data: None }
    }

    pub fn write_dp(reg_addr: u8, data: u32) -> Self {
        Self { rnw: false, apndp: false, reg_addr, write_data: Some(data) }
    }

    pub fn read_ap(reg_addr: u8) -> Self {
        Self { rnw: true, apndp: true, reg_addr, write_data: None }
    }

    pub fn write_ap(reg_addr: u8, data: u32) -> Self {
        Self { rnw: false, apndp: true, reg_addr, write_data: Some(data) }
    }

    /// 编码为 DAP_Transfer 请求字节
    pub fn request_byte(&self) -> u8 {
        make_request(self.rnw, self.apndp, reg_a2(self.reg_addr), reg_a3(self.reg_addr))
    }
}

/// Transfer 响应
///
/// `data` 使用定长数组，使 DAP_Transfer 响应解析在采样热路径上零堆分配。
/// 容量按最坏情况取：每个通道最多占 3 个 32 位字（8 字节类型跨越
/// 非 4 字节对齐边界时），因此 `MAX_VALUES * 3`。
///
/// 注意：这个容量只管“响应里能装多少字”，与通道数（`MAX_VALUES`）不同——
/// 8 个 double 通道的完整响应携带 16 个字，若按通道数截断会静默丢掉
/// 后半段通道的高字。
#[derive(Debug)]
pub struct TransferResponse {
    pub status: u8,
    pub count: u8,
    pub data: [u32; crate::pipeline::sample::MAX_VALUES * 3],
    /// 实际携带的读取值个数
    pub len: usize,
}

impl TransferResponse {
    /// 本次响应实际携带的读取值切片
    pub fn values(&self) -> &[u32] {
        &self.data[..self.len]
    }
}

/// DAP 设备信息
#[derive(Debug)]
pub struct DapInfo {
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub fw_version: String,
    pub packet_size: u16,
    pub packet_count: u8,
}
