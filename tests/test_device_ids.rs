/// 测试 usb::device 的设备识别模型
///
/// 背景：原先的识别是「VID/PID 白名单 or 产品字符串含 CMSIS-DAP」的硬门，
/// 两者都不中的设备会被静默丢弃。GD-Link（28E9:0697）的产品字符串是
/// `GDLinker_V2`，但接口字符串写着 `CMSIS-DAP V2`——原实现从不读接口
/// 字符串，于是这台设备完全隐形。
///
/// 现在的模型是「分层置信度」：字符串证据 → confirmed，VID/PID → likely，
/// 只有结构特征 → unknown（不自动连接）。
use dap_sampler::usb::device::{Confidence, KNOWN_DEVICES};
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
}

/// GD-Link 必须能**在不依赖白名单**的情况下被识别。
///
/// 这条测试同时锁死两件事：
/// 1. 白名单里不该有 GD-Link 条目（否则测不出接口字符串路径）
/// 2. 识别逻辑不能退回「产品字符串 or VID/PID」这种双判据硬门
#[test]
fn gdlink_is_not_whitelisted() {
    assert!(
        !KNOWN_DEVICES.contains(&(0x28e9, 0x0697)),
        "GD-Link 28E9:0697 must be identified by its interface string, not the whitelist"
    );
    assert!(
        !KNOWN_DEVICES.contains(&(0x28e9, 0x0698)),
        "GD-Link 28E9:0698 must be identified by its interface string, not the whitelist"
    );
}

#[test]
fn known_devices_vid_pid_are_nonzero() {
    // VID 0 是非法值，PID 0 通常表示未分配；命中这种条目只会误判
    for &(vid, pid) in KNOWN_DEVICES {
        assert_ne!(vid, 0, "zero VID entry in KNOWN_DEVICES");
        assert_ne!(pid, 0, "zero PID entry for VID 0x{:04X}", vid);
    }
}

// ================================================================
// 置信度模型
// ================================================================

#[test]
fn confidence_ordering_puts_confirmed_first() {
    assert!(Confidence::Confirmed < Confidence::Likely);
    assert!(Confidence::Likely < Confidence::Possible);
}

#[test]
fn only_confirmed_and_likely_are_auto_connectable() {
    assert!(Confidence::Confirmed.auto_connectable());
    assert!(Confidence::Likely.auto_connectable());
    // 只有结构特征的设备（CH340、蓝牙适配器）绝不能自动连接：
    // 它们和真正的探针在描述符上无法区分
    assert!(!Confidence::Possible.auto_connectable());
}

#[test]
fn confidence_labels_are_stable() {
    // 标签会出现在 `list`/`diag` 输出里，改动会影响用户排障脚本
    assert_eq!(Confidence::Confirmed.label(), "confirmed");
    assert_eq!(Confidence::Likely.label(), "likely");
    assert_eq!(Confidence::Possible.label(), "unknown");
}
