/// 测试 dap::commands — SWD 请求字节编码、寄存器地址、传输常量
use dap_sampler::dap::commands::*;

// ================================================================
// make_request — SWD 请求字节编码
// ================================================================
// Bit layout: [7:4]=unused, [3]=A3, [2]=A2, [1]=RnW, [0]=APnDP

#[test]
fn make_request_rnw_read_dp_reg0() {
    // Read DP register 0 (DPIDR): RnW=1, APnDP=0, A2=0, A3=0
    let byte = make_request(true, false, false, false);
    assert_eq!(byte, 0b_0000_0010); // bit 1 = RnW=1
}

#[test]
fn make_request_rnw_write_dp_reg0() {
    // Write DP register 0: RnW=0, APnDP=0, A2=0, A3=0
    let byte = make_request(false, false, false, false);
    assert_eq!(byte, 0b_0000_0000);
}

#[test]
fn make_request_apndp_set() {
    // Read AP register: RnW=1, APnDP=1, A2=0, A3=0
    let byte = make_request(true, true, false, false);
    assert_eq!(byte, 0b_0000_0011); // bit 0=APnDP=1, bit 1=RnW=1
}

#[test]
fn make_request_a2_set() {
    // Read DP register 4 (CTRL/STAT): RnW=1, APnDP=0, A2=1, A3=0
    let byte = make_request(true, false, true, false);
    assert_eq!(byte, 0b_0000_0110); // bit 2=A2=1, bit 1=RnW=1
}

#[test]
fn make_request_a3_set() {
    // Read DP register 8 (SELECT): RnW=1, APnDP=0, A2=0, A3=1
    let byte = make_request(true, false, false, true);
    assert_eq!(byte, 0b_0000_1010); // bit 3=A3=1, bit 1=RnW=1
}

#[test]
fn make_request_all_bits() {
    // Write AP register 0xC (DRW): RnW=0, APnDP=1, A2=1, A3=1
    let byte = make_request(false, true, true, true);
    assert_eq!(byte, 0b_0000_1101); // APnDP=1, A2=1, A3=1, RnW=0
}

#[test]
fn make_request_all_16_combinations_unique() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for rnw in [false, true] {
        for apndp in [false, true] {
            for a2 in [false, true] {
                for a3 in [false, true] {
                    let b = make_request(rnw, apndp, a2, a3);
                    assert!(seen.insert(b), "Duplicate byte: 0x{:02X}", b);
                    // Validate bit positions
                    assert_eq!(b & 1, apndp as u8);
                    assert_eq!((b >> 1) & 1, rnw as u8);
                    assert_eq!((b >> 2) & 1, a2 as u8);
                    assert_eq!((b >> 3) & 1, a3 as u8);
                }
            }
        }
    }
    assert_eq!(seen.len(), 16);
}

// ================================================================
// req_read_dp / req_write_dp — DP 寄存器请求字节
// ================================================================

#[test]
fn req_read_dp_dpidr() {
    // DPIDR = 0x00, so A2=A3=0, RnW=1, APnDP=0
    assert_eq!(req_read_dp(0x00), 0b_0000_0010);
}

#[test]
fn req_read_dp_ctrl_stat() {
    // CTRL/STAT = 0x04, bits[3:2] = 01, so A2=1, A3=0
    assert_eq!(req_read_dp(0x04), 0b_0000_0110);
}

#[test]
fn req_read_dp_select() {
    // SELECT = 0x08, bits[3:2] = 10, so A2=0, A3=1
    assert_eq!(req_read_dp(0x08), 0b_0000_1010);
}

#[test]
fn req_read_dp_rdbuff() {
    // RDBUFF = 0x0C, bits[3:2] = 11, so A2=1, A3=1
    assert_eq!(req_read_dp(0x0C), 0b_0000_1110);
}

#[test]
fn req_write_dp_ctrl_stat() {
    // CTRL/STAT write: RnW=0, APnDP=0, A2=1, A3=0
    assert_eq!(req_write_dp(0x04), 0b_0000_0100);
}

#[test]
fn req_write_dp_select() {
    assert_eq!(req_write_dp(0x08), 0b_0000_1000);
}

// ================================================================
// req_read_ap / req_write_ap — AP 寄存器请求字节
// ================================================================

#[test]
fn req_read_ap_drw() {
    // DRW = 0x0C, bits[3:2] = 11, RnW=1, APnDP=1
    assert_eq!(req_read_ap(0x0C), 0b_0000_1111);
}

#[test]
fn req_write_ap_tar() {
    // TAR = 0x04, bits[3:2] = 01, RnW=0, APnDP=1
    assert_eq!(req_write_ap(0x04), 0b_0000_0101);
}

#[test]
fn req_write_ap_csw() {
    // CSW = 0x00, bits[3:2] = 00, RnW=0, APnDP=1
    assert_eq!(req_write_ap(0x00), 0b_0000_0001);
}

// ================================================================
// TransferRequest 构造器与 request_byte
// ================================================================

#[test]
fn transfer_request_read_dp() {
    let req = TransferRequest::read_dp(DP_REG_DPIDR);
    assert!(req.rnw);
    assert!(!req.apndp);
    assert_eq!(req.reg_addr, DP_REG_DPIDR);
    assert_eq!(req.write_data, None);
    assert_eq!(req.request_byte(), req_read_dp(DP_REG_DPIDR));
}

#[test]
fn transfer_request_write_dp() {
    let req = TransferRequest::write_dp(DP_REG_CTRL_STAT, 0x50000000);
    assert!(!req.rnw);
    assert!(!req.apndp);
    assert_eq!(req.reg_addr, DP_REG_CTRL_STAT);
    assert_eq!(req.write_data, Some(0x50000000));
    assert_eq!(req.request_byte(), req_write_dp(DP_REG_CTRL_STAT));
}

#[test]
fn transfer_request_read_ap() {
    let req = TransferRequest::read_ap(AP_REG_DRW);
    assert!(req.rnw);
    assert!(req.apndp);
    assert_eq!(req.reg_addr, AP_REG_DRW);
    assert_eq!(req.write_data, None);
    assert_eq!(req.request_byte(), req_read_ap(AP_REG_DRW));
}

#[test]
fn transfer_request_write_ap() {
    let req = TransferRequest::write_ap(AP_REG_TAR, 0x20000100);
    assert!(!req.rnw);
    assert!(req.apndp);
    assert_eq!(req.reg_addr, AP_REG_TAR);
    assert_eq!(req.write_data, Some(0x20000100));
    assert_eq!(req.request_byte(), req_write_ap(AP_REG_TAR));
}

// ================================================================
// 寄存器地址常量
// ================================================================

#[test]
fn dp_register_addresses() {
    assert_eq!(DP_REG_DPIDR, 0x00);
    assert_eq!(DP_REG_CTRL_STAT, 0x04);
    assert_eq!(DP_REG_SELECT, 0x08);
    assert_eq!(DP_REG_RDBUFF, 0x0C);
}

#[test]
fn ap_register_addresses() {
    assert_eq!(AP_REG_CSW, 0x00);
    assert_eq!(AP_REG_TAR, 0x04);
    assert_eq!(AP_REG_DRW, 0x0C);
    // IDR 在 AP 空间偏移 0xFC = bank 0xF + 页内偏移 0xC。
    // 请求字节只携带 A[3:2]，所以常量取页内偏移，bank 由 AP_BANK_IDR
    // 经 DP SELECT 下发；两者必须同时使用，只写 0x0C 会落到 DRW。
    assert_eq!(AP_REG_IDR, 0x0C);
    assert_eq!(AP_BANK_IDR, 0x0F);
}

#[test]
fn ap_idr_requires_bank_f_select() {
    // 请求字节只编码 A[3:2]：0x0C 与 0xFC 编码相同，靠 SELECT 区分 bank
    assert_eq!(req_read_ap(AP_REG_IDR), 0x0F);
    assert_eq!(req_read_ap(0xFC), 0x0F);
    // APBANKSEL 必须落在 bits[7:4]
    assert_eq!(dp_select_value(0, AP_BANK_IDR), 0xF0);
    // 还原 bank 0 才是干净的后续内存访问状态
    assert_eq!(dp_select_value(0, 0), 0x0000_0000);
    // APSEL 在 bits[31:24]，与 bank 位互不干扰
    assert_eq!(dp_select_value(1, AP_BANK_IDR), 0x0100_00F0);
}

/// 模拟目标侧如何把 (DP SELECT 写值, AP 请求字节) 解析成 AP 空间偏移。
///
/// 请求字节只有 A[3:2]；A[7:4] 来自 SELECT.APBANKSEL。这个函数让我们
/// 能在无硬件的情况下验证「到底读到了哪个寄存器」。
fn resolve_ap_offset(select: u32, request_byte: u8) -> u32 {
    let bank = ((select >> 4) & 0x0F) << 4;
    let a32 = u32::from((request_byte >> 2) & 0b11) << 2;
    bank | a32
}

#[test]
fn ap_bank_select_resolves_idr_offset() {
    let idr_read = req_read_ap(AP_REG_IDR);

    // 正确做法：先切 bank 0xF，同一请求字节才真正落到偏移 0xFC
    let select_idr = dp_select_value(0, AP_BANK_IDR);
    assert_eq!(resolve_ap_offset(select_idr, idr_read), 0xFC);

    // 只把常量从 0x0C 改成 0xFC 而不切 bank，线上效果完全一样：
    // 解析结果仍是 0x0C（DRW），读到的是 TAR 指向的内存而不是 IDR。
    let select_bank0 = dp_select_value(0, 0);
    assert_eq!(resolve_ap_offset(select_bank0, idr_read), 0x0C);
    assert_ne!(resolve_ap_offset(select_bank0, idr_read), 0xFC);

    // 还原 bank 0 后，CSW/TAR/DRW 的解析结果才是它们自己
    assert_eq!(
        resolve_ap_offset(select_bank0, req_write_ap(AP_REG_CSW)),
        0x00
    );
    assert_eq!(
        resolve_ap_offset(select_bank0, req_write_ap(AP_REG_TAR)),
        0x04
    );
    assert_eq!(
        resolve_ap_offset(select_bank0, req_read_ap(AP_REG_DRW)),
        0x0C
    );
}

#[test]
fn dp_abort_clear_sticky_bit_layout() {
    // 0x1E = STKCMPCLR | STKERRCLR | WDERRCLR | ORUNERRCLR
    assert_eq!(DP_ABORT_CLEAR_STICKY, 0x1E);
    // 清错写值不得包含 DAPABORT（0x01），那是主动中止而非清错
    assert_eq!(DP_ABORT_CLEAR_STICKY & 0x01, 0);
    // ABORT 与 DPIDR 同地址，靠 RnW 方向区分
    assert_eq!(DP_REG_ABORT, DP_REG_DPIDR);
    let abort_req = TransferRequest::write_dp(DP_REG_ABORT, DP_ABORT_CLEAR_STICKY);
    assert!(!abort_req.rnw, "ABORT is write-only");
    assert_eq!(abort_req.request_byte(), req_write_dp(DP_REG_DPIDR));
}

// ================================================================
// CTRL/STAT 位定义（互不重叠）
// ================================================================

#[test]
fn ctrl_stat_bits_non_overlapping() {
    let bits = vec![
        CSYSPWRUPREQ,
        CDBGPWRUPREQ,
        CSYSPWRUPACK,
        CDBGPWRUPACK,
        CDBGRSTREQ,
        CDBGRSTACK,
    ];
    // All bits should have exactly one bit set
    for &b in &bits {
        assert_eq!(b.count_ones(), 1, "Bit 0x{:08X} should be a single bit", b);
    }
    // All bits should be unique (no overlap)
    use std::collections::HashSet;
    let set: HashSet<u32> = bits.into_iter().collect();
    assert_eq!(set.len(), 6);
}

#[test]
fn ctrl_stat_powerup_request_value() {
    // CDBGPWRUPREQ | CSYSPWRUPREQ should be bits 28 and 30 set
    let combined = CSYSPWRUPREQ | CDBGPWRUPREQ;
    assert_eq!(combined, 0x50000000);
}

// ================================================================
// DAP 命令码常量
// ================================================================

#[test]
fn command_bytes_sanity() {
    assert_eq!(DAP_INFO, 0x00);
    assert_eq!(DAP_LED, 0x01);
    assert_eq!(DAP_CONNECT, 0x02);
    assert_eq!(DAP_DISCONNECT, 0x03);
    assert_eq!(DAP_TRANSFER_CONFIGURE, 0x04);
    assert_eq!(DAP_TRANSFER, 0x05);
    assert_eq!(DAP_SWJ_CLOCK, 0x11);
    assert_eq!(DAP_SWJ_PINS, 0x10);
    assert_eq!(DAP_SWJ_SEQUENCE, 0x12);
    assert_eq!(DAP_SWD_CONFIGURE, 0x13);
}

// ================================================================
// 连接模式
// ================================================================

#[test]
fn connect_modes() {
    assert_eq!(CONNECT_MODE_AUTO, 0x00);
    assert_eq!(CONNECT_MODE_SWD, 0x01);
    assert_eq!(CONNECT_MODE_JTAG, 0x02);
}

// ================================================================
// Transfer 状态码
// ================================================================

#[test]
fn transfer_status_codes() {
    assert_eq!(TRANSFER_OK, 0x01);
    assert_eq!(TRANSFER_WAIT, 0x02);
    assert_eq!(TRANSFER_FAULT, 0x04);
    assert_eq!(TRANSFER_PROTOCOL_ERR, 0x07);
}
