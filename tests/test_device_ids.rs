/// 测试 usb::device 的已知设备白名单
///
/// 白名单是「产品字符串里没写 CMSIS-DAP」的探针能被枚举到的唯一途径，
/// 因此条目缺失、重复或写错 VID/PID 都会直接表现为「设备不识别」。
use dap_sampler::usb::device::KNOWN_DEVICES;
use std::collections::HashSet;

#[test]
fn known_devices_has_no_duplicates() {
    let unique: HashSet<_> = KNOWN_DEVICES.iter().copied().collect();
    assert_eq!(
        unique.len(),
        KNOWN_DEVICES.len(),
        "duplicate (vid, pid) entries in KNOWN_DEVICES"
    );
}

#[test]
fn known_devices_contains_common_probes() {
    // ARM mbed DAPLink（绝大多数克隆板沿用）
    assert!(KNOWN_DEVICES.contains(&(0x0d28, 0x0204)));
    // NXP LPC-Link2
    assert!(KNOWN_DEVICES.contains(&(0x1fc9, 0x0132)));
    // AT-Link (AT32)
    assert!(KNOWN_DEVICES.contains(&(0x2e88, 0x0001)));
    // GigaDevice GD-Link（社区反馈的无法识别设备）
    assert!(KNOWN_DEVICES.contains(&(0x28e9, 0x0698)));
}

#[test]
fn known_devices_vid_pid_are_nonzero() {
    // VID 0 是非法值，PID 0 通常表示未分配；命中这种条目只会误判
    for &(vid, pid) in KNOWN_DEVICES {
        assert_ne!(vid, 0, "zero VID entry in KNOWN_DEVICES");
        assert_ne!(pid, 0, "zero PID entry for VID 0x{:04X}", vid);
    }
}
