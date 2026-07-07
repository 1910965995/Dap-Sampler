/// 测试 dap::protocol — DAP 协议响应解析、命令构建
use dap_sampler::dap::protocol::DapProtocol;
use dap_sampler::dap::commands::*;

fn dap() -> DapProtocol {
    DapProtocol { dap_index: 0 }
}

// ================================================================
// parse_transfer_response
// ================================================================

#[test]
fn parse_transfer_ok_single_read() {
    // Response: [cmd_echo=0x05, count=1, status=0x01(OK), data=0x12345678 LE]
    let resp = [0x05, 0x01, 0x01, 0x78, 0x56, 0x34, 0x12];
    let result = DapProtocol::parse_transfer_response(&resp).unwrap();
    assert_eq!(result.status, TRANSFER_OK);
    assert_eq!(result.count, 1);
    assert_eq!(result.data.len(), 1);
    assert_eq!(result.data[0], 0x12345678);
}

#[test]
fn parse_transfer_ok_multiple_reads() {
    // 2 reads: values 0xAAAAAAAA and 0xBBBBBBBB
    let resp = [
        0x05, // cmd_echo
        0x04, // count=4 (2 writes + 2 reads)
        0x01, // status=OK
        0xAA, 0xAA, 0xAA, 0xAA, // first read value
        0xBB, 0xBB, 0xBB, 0xBB, // second read value
    ];
    let result = DapProtocol::parse_transfer_response(&resp).unwrap();
    assert_eq!(result.status, TRANSFER_OK);
    assert_eq!(result.data.len(), 2);
    assert_eq!(result.data[0], 0xAAAAAAAA);
    assert_eq!(result.data[1], 0xBBBBBBBB);
}

#[test]
fn parse_transfer_wait_status() {
    let resp = [0x05, 0x00, TRANSFER_WAIT];
    let result = DapProtocol::parse_transfer_response(&resp).unwrap();
    assert_eq!(result.status, TRANSFER_WAIT);
    assert_eq!(result.count, 0);
    assert!(result.data.is_empty());
}

#[test]
fn parse_transfer_fault_status() {
    let resp = [0x05, 0x01, TRANSFER_FAULT, 0x00, 0x00, 0x00, 0x00];
    let result = DapProtocol::parse_transfer_response(&resp).unwrap();
    assert_eq!(result.status, TRANSFER_FAULT);
}

#[test]
fn parse_transfer_too_short() {
    // Less than 3 bytes
    let resp = [0x05, 0x01];
    let result = DapProtocol::parse_transfer_response(&resp);
    assert!(result.is_err());
}

#[test]
fn parse_transfer_empty() {
    let resp: [u8; 0] = [];
    let result = DapProtocol::parse_transfer_response(&resp);
    assert!(result.is_err());
}

#[test]
fn parse_transfer_zero_data() {
    // Valid OK with no read data (all writes)
    let resp = [0x05, 0x02, 0x01]; // count=2 writes, status=OK, no data
    let result = DapProtocol::parse_transfer_response(&resp).unwrap();
    assert_eq!(result.status, TRANSFER_OK);
    assert_eq!(result.count, 2);
    assert!(result.data.is_empty());
}

// ================================================================
// parse_info_response
// ================================================================

#[test]
fn parse_info_ok_with_content() {
    let resp = [0x00, 0x00, 0x41, 0x42, 0x43]; // status=OK, content="ABC"
    let (status, content) = DapProtocol::parse_info_response(&resp).unwrap();
    assert_eq!(status, 0x00);
    assert_eq!(content, vec![0x41, 0x42, 0x43]);
}

#[test]
fn parse_info_ok_empty_content() {
    let resp = [0x00, 0x00];
    let (status, content) = DapProtocol::parse_info_response(&resp).unwrap();
    assert_eq!(status, 0x00);
    assert!(content.is_empty());
}

#[test]
fn parse_info_too_short() {
    let resp = [0x00];
    let result = DapProtocol::parse_info_response(&resp);
    assert!(result.is_err());
}

#[test]
fn parse_info_error_status() {
    let resp = [0x00, 0xFF]; // non-zero status = error
    let (status, content) = DapProtocol::parse_info_response(&resp).unwrap();
    assert_eq!(status, 0xFF);
    assert!(content.is_empty());
}

// ================================================================
// parse_connect_response
// ================================================================

#[test]
fn parse_connect_swd_mode() {
    let resp = [0x02, 0x01]; // port=1 = SWD
    assert_eq!(DapProtocol::parse_connect_response(&resp).unwrap(), 1);
}

#[test]
fn parse_connect_not_connected() {
    let resp = [0x02, 0x00]; // port=0 = not connected
    assert_eq!(DapProtocol::parse_connect_response(&resp).unwrap(), 0);
}

#[test]
fn parse_connect_jtag_mode() {
    let resp = [0x02, 0x02]; // port=2 = JTAG
    assert_eq!(DapProtocol::parse_connect_response(&resp).unwrap(), 2);
}

#[test]
fn parse_connect_too_short() {
    let resp = [0x02];
    assert!(DapProtocol::parse_connect_response(&resp).is_err());
}

// ================================================================
// parse_clock_response
// ================================================================

#[test]
fn parse_clock_success() {
    let resp = [0x11, 0x00];
    assert_eq!(DapProtocol::parse_clock_response(&resp).unwrap(), 0);
}

#[test]
fn parse_clock_error() {
    let resp = [0x11, 0x01];
    assert_eq!(DapProtocol::parse_clock_response(&resp).unwrap(), 1);
}

// ================================================================
// parse_transfer_configure_response
// ================================================================

#[test]
fn parse_transfer_configure_success() {
    let resp = [0x04, 0x00];
    assert!(DapProtocol::parse_transfer_configure_response(&resp).is_ok());
}

#[test]
fn parse_transfer_configure_error() {
    let resp = [0x04, 0x01];
    assert!(DapProtocol::parse_transfer_configure_response(&resp).is_err());
}

// ================================================================
// parse_swd_configure_response
// ================================================================

#[test]
fn parse_swd_configure_success() {
    let resp = [0x13, 0x00];
    assert!(DapProtocol::parse_swd_configure_response(&resp).is_ok());
}

#[test]
fn parse_swd_configure_error() {
    let resp = [0x13, 0x01];
    assert!(DapProtocol::parse_swd_configure_response(&resp).is_err());
}

// ================================================================
// parse_host_status_response
// ================================================================

#[test]
fn parse_host_status_any() {
    let resp = [0x01, 0x00, 0x01];
    // Always Ok (some DAP-Link may not support)
    assert!(DapProtocol::parse_host_status_response(&resp).is_ok());
}

#[test]
fn parse_host_status_short() {
    let resp = [0x01];
    // Should not panic; may or may not be ok
    let _ = DapProtocol::parse_host_status_response(&resp);
}

// ================================================================
// 命令构建（build_* 方法）
// ================================================================

#[test]
fn build_info_request() {
    let cmd = dap().build_info_request(0x01);
    assert_eq!(cmd, vec![DAP_INFO, 0x01]);
}

#[test]
fn build_connect_request_swd() {
    let cmd = dap().build_connect_request(CONNECT_MODE_SWD);
    assert_eq!(cmd, vec![DAP_CONNECT, CONNECT_MODE_SWD]);
}

#[test]
fn build_clock_request_10mhz() {
    let cmd = dap().build_clock_request(10_000_000);
    assert_eq!(cmd[0], DAP_SWJ_CLOCK);
    // 10_000_000 in little-endian u32
    let freq = u32::from_le_bytes([cmd[1], cmd[2], cmd[3], cmd[4]]);
    assert_eq!(freq, 10_000_000);
}

#[test]
fn build_transfer_configure_request() {
    let cmd = DapProtocol::build_transfer_configure_request(0, 100, 0);
    assert_eq!(cmd[0], DAP_TRANSFER_CONFIGURE);
    assert_eq!(cmd[1], 0); // idle_cycles
    let wait_retry = u16::from_le_bytes([cmd[2], cmd[3]]);
    assert_eq!(wait_retry, 100);
    let match_retry = u16::from_le_bytes([cmd[4], cmd[5]]);
    assert_eq!(match_retry, 0);
}

#[test]
fn build_swd_configure_request() {
    let cmd = DapProtocol::build_swd_configure_request(0x00);
    assert_eq!(cmd, vec![DAP_SWD_CONFIGURE, 0x00]);
}

#[test]
fn build_host_status_connect_led_on() {
    let cmd = DapProtocol::build_host_status_request(0, 1);
    assert_eq!(cmd, vec![DAP_LED, 0, 1]);
}

#[test]
fn build_host_status_running_led_on() {
    let cmd = DapProtocol::build_host_status_request(1, 1);
    assert_eq!(cmd, vec![DAP_LED, 1, 1]);
}

#[test]
fn build_pins_request() {
    let cmd = dap().build_pins_request(0x80, 0x80, 100_000);
    assert_eq!(cmd[0], DAP_SWJ_PINS);
    assert_eq!(cmd[1], 0x80); // Pin Output: nRESET high
    assert_eq!(cmd[2], 0x80); // Pin Select: nRESET
    let wait = u32::from_le_bytes([cmd[3], cmd[4], cmd[5], cmd[6]]);
    assert_eq!(wait, 100_000);
}

#[test]
fn build_swj_sequence_51_bits() {
    // 51 bits = 7 bytes (56 bits), but only 51 bits are valid
    // The function calculates bit_count from byte count: min(7*8, 256) = 56
    // The caller is responsible for passing the right number of bytes
    let data = vec![0xFF; 7];
    let cmd = dap().build_swj_sequence_request(&data);
    assert_eq!(cmd[0], DAP_SWJ_SEQUENCE);
    assert_eq!(cmd[1], 56); // 7 bytes × 8 bits = 56 (caller truncates to 51 in practice)
    assert_eq!(cmd.len(), 2 + 7);
}

#[test]
fn build_swj_sequence_exact_bytes() {
    // When we want exactly N bits, we should pass the right number of bytes
    // For 16 bits = 2 bytes
    let data = vec![0x9E, 0xE7];
    let cmd = dap().build_swj_sequence_request(&data);
    assert_eq!(cmd[0], DAP_SWJ_SEQUENCE);
    assert_eq!(cmd[1], 16); // exactly 16 bits
    assert_eq!(cmd[2], 0x9E);
    assert_eq!(cmd[3], 0xE7);
}

// ================================================================
// build_transfer_request
// ================================================================

#[test]
fn build_transfer_read_only() {
    let requests = vec![
        TransferRequest::read_dp(DP_REG_DPIDR),
        TransferRequest::read_ap(AP_REG_DRW),
    ];
    let cmd = dap().build_transfer_request(&requests);
    assert_eq!(cmd[0], DAP_TRANSFER);
    assert_eq!(cmd[1], 0); // dap_index
    assert_eq!(cmd[2], 2); // request count
    // Request bytes: read DPIDR (0x02), read DRW (0x0F)
    assert_eq!(cmd[3], req_read_dp(DP_REG_DPIDR));
    assert_eq!(cmd[4], req_read_ap(AP_REG_DRW));
    // No write data
    assert_eq!(cmd.len(), 5);
}

#[test]
fn build_transfer_with_writes() {
    let requests = vec![
        TransferRequest::write_ap(AP_REG_TAR, 0x20000100),
        TransferRequest::read_ap(AP_REG_DRW),
    ];
    let cmd = dap().build_transfer_request(&requests);
    assert_eq!(cmd[0], DAP_TRANSFER);
    assert_eq!(cmd[2], 2); // count
    assert_eq!(cmd[3], req_write_ap(AP_REG_TAR));
    // Write data: 0x20000100 in LE
    assert_eq!(&cmd[4..8], &[0x00, 0x01, 0x00, 0x20]);
    assert_eq!(cmd[8], req_read_ap(AP_REG_DRW));
    assert_eq!(cmd.len(), 9);
}

#[test]
fn build_transfer_multiple_variables() {
    // 4 variables (= 8 requests: 4 write TAR + 4 read DRW)
    let addresses = vec![0x20000100u32, 0x20000104, 0x20000108, 0x2000010c];
    let requests: Vec<TransferRequest> = addresses
        .iter()
        .flat_map(|&addr| {
            vec![
                TransferRequest::write_ap(AP_REG_TAR, addr),
                TransferRequest::read_ap(AP_REG_DRW),
            ]
        })
        .collect();
    let cmd = dap().build_transfer_request(&requests);
    assert_eq!(cmd[0], DAP_TRANSFER);
    assert_eq!(cmd[2], 8); // 8 requests
    // Verify structure: header (3 bytes) + requests
    // First request: write TAR (1 byte request + 4 bytes data)
    assert_eq!(cmd[3], req_write_ap(AP_REG_TAR)); // 0x05
    // cmd[4..8] = 0x20000100 in LE: [0x00, 0x01, 0x00, 0x20]
    assert_eq!(&cmd[4..8], &[0x00, 0x01, 0x00, 0x20]);
    // Second request: read DRW (1 byte)
    assert_eq!(cmd[8], req_read_ap(AP_REG_DRW)); // 0x0F
    // Third request: write TAR with next address
    assert_eq!(cmd[9], req_write_ap(AP_REG_TAR)); // 0x05
    // cmd[10..14] = 0x20000104 in LE: [0x04, 0x01, 0x00, 0x20]
    assert_eq!(&cmd[10..14], &[0x04, 0x01, 0x00, 0x20]);
    assert_eq!(cmd[14], req_read_ap(AP_REG_DRW)); // 0x0F
}
