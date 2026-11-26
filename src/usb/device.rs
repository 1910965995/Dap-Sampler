use rusb::{Context, Device, UsbContext};
use crate::error::*;

/// CMSIS-DAP v2 已知设备 VID/PID
///
/// **这不是准入白名单**，只是「型号提示」：命中条目会把置信度提升到
/// [`Confidence::Likely`]，让没有可靠字符串证据的老设备仍能连上。
/// 真正决定识别的是 USB 字符串（接口/产品/制造商）中的 "CMSIS-DAP"
/// 标记，见 [`identify`]。
///
/// 新设备**不需要**往这里加条目：只要接口字符串写了 CMSIS-DAP 就能
/// 被识别。加条目的后果是——万一 VID/PID 与某台根本不能讲 DAP 的
/// 设备重合，会让它被当成探针去连接，报出莫名其妙的超时。
pub const KNOWN_DEVICES: &[(u16, u16)] = &[
    (0x0d28, 0x0204),  // ARM mbed DAPLink
    (0x0d28, 0x0205),  // ARM mbed DAPLink (alternative)
    (0x1fc9, 0x0132),  // NXP LPC-Link2
    (0x03eb, 0x2111),  // Atmel EDBG
    (0x2e88, 0x0001),  // AT-Link (AT32)
    (0x0483, 0x374b),  // ST-LINK/V2-1
    (0x0483, 0x374c),  // ST-LINK/V2-1 (alt)
    (0x0483, 0x374d),  // ST-LINK/V3 (alt)
    (0x0483, 0x374e),  // ST-LINK/V3 (alt 2)
    (0xc251, 0x1001),  // J-Link (CMSIS-DAP mode)
    (0xc251, 0x1002),  // J-Link (CMSIS-DAP mode alt)
];

/// USB 字符串里标识 CMSIS-DAP 的标记（比对前统一转大写）
const CMSIS_MARKER: &str = "CMSIS-DAP";

/// 设备被判为 CMSIS-DAP 探针的置信度
///
/// 排序即优先级：`Confirmed` 最可信，`Possible` 永远不自动连接。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// 字符串证据命中：接口/产品/制造商字符串里写了 CMSIS-DAP
    Confirmed,
    /// 仅 VID/PID 命中已知型号，没有任何字符串佐证
    Likely,
    /// 只有结构特征（非 CDC/MSC/HID 类 + Bulk IN/OUT），无法确认
    Possible,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::Confirmed => "confirmed",
            Confidence::Likely => "likely",
            Confidence::Possible => "unknown",
        }
    }

    /// 是否允许自动连接
    ///
    /// `Possible` 刻意排除：CH340、蓝牙适配器等设备在结构上与 DAP
    /// 完全一致，自动连接会把它们当作探针，产生难以理解的超时错误。
    pub fn auto_connectable(self) -> bool {
        matches!(self, Confidence::Confirmed | Confidence::Likely)
    }
}

/// 设备上的 CMSIS-DAP 候选接口
#[derive(Debug, Clone, PartialEq)]
pub struct DapInterface {
    pub interface_number: u8,
    pub ep_out: u8,
    pub ep_in: u8,
    /// 接口字符串（iInterface），部分固件为空
    pub name: String,
}

/// USB 设备信息
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub bus_number: u8,
    pub address: u8,
    /// DAP 候选接口信息（列出时必然存在）
    pub interface: DapInterface,
    pub confidence: Confidence,
    /// 置信度的判定依据，用于展示和排障
    pub evidence: &'static str,
}

/// USB 设备诊断信息（用于定位 CMSIS-DAP 连接失败位置）
#[derive(Debug, Clone)]
pub struct UsbDeviceDiagnostic {
    pub vid: u16,
    pub pid: u16,
    pub bus_number: u8,
    pub address: u8,
    pub known_vid_pid: bool,
    pub open_ok: bool,
    pub open_error: Option<String>,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub product_has_cmsis_dap: bool,
    /// 接口字符串是否含 CMSIS-DAP
    pub interface_has_cmsis_dap: bool,
    /// 接口字符串原文（部分固件为空）
    pub interface_name: String,
    pub confidence: Confidence,
    pub evidence: &'static str,
    pub has_bulk_candidate: bool,
    pub config_error: Option<String>,
    pub interface_number: Option<u8>,
    pub ep_out: Option<u8>,
    pub ep_in: Option<u8>,
    pub claim_ok: Option<bool>,
    pub claim_error: Option<String>,
}

/// 列出总线上的 CMSIS-DAP 候选设备（含未识别的，按置信度排序）
///
/// 不再按「白名单 or 产品字符串」做硬过滤——那样会让新设备彻底隐形，
/// 用户连「程序看见过它」都无从确认。所有具备 Bulk 端点对的设备都会
/// 返回并附上置信度，由调用方决定展示方式。
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let mut result = Vec::new();

    for device in devices.iter() {
        // 单台设备取描述符失败（热插拔瞬态）不应中断整个枚举
        let desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };

        // 打开失败也要继续：接口字符串读不到，但结构信息仍可展示
        let handle = device.open().ok();
        let Some(identified) = identify(&device, handle.as_ref()) else {
            continue; // 没有任何 Bulk 端点对，不可能是 CMSIS-DAP v2
        };

        let manufacturer = handle
            .as_ref()
            .map(|h| read_string_descriptor(h, desc.manufacturer_string_index()))
            .unwrap_or_default();
        let product = handle
            .as_ref()
            .map(|h| read_string_descriptor(h, desc.product_string_index()))
            .unwrap_or_default();
        let serial = handle
            .as_ref()
            .map(|h| read_string_descriptor(h, desc.serial_number_string_index()))
            .unwrap_or_default();

        result.push(DeviceInfo {
            vid: desc.vendor_id(),
            pid: desc.product_id(),
            manufacturer,
            product,
            serial,
            bus_number: device.bus_number(),
            address: device.address(),
            interface: identified.interface,
            confidence: identified.confidence,
            evidence: identified.evidence,
        });
    }

    // 置信度高的排前面（Confidence 的 Ord 即优先级）
    result.sort_by(|a, b| a.confidence.cmp(&b.confidence).then(a.bus_number.cmp(&b.bus_number)));
    Ok(result)
}

/// 根据 VID/PID 判断是否为已知型号
fn is_known_device(vid: u16, pid: u16) -> bool {
    KNOWN_DEVICES.contains(&(vid, pid))
}

/// 已知的非 CMSIS-DAP 接口类
const CDC_CLASS: u8 = 0x02;      // Communication Device Class
const CDC_DATA_CLASS: u8 = 0x0A; // CDC Data Class
const MSC_CLASS: u8 = 0x08;      // Mass Storage Class
const HUB_CLASS: u8 = 0x09;      // Hub Class
const AUDIO_CLASS: u8 = 0x01;    // Audio Class
const HID_CLASS: u8 = 0x03;      // Human Interface Device

/// 接口是否可能是 CMSIS-DAP v2（Bulk 端点对 + 非已知类）
fn is_cmsis_dap_candidate(alt: &rusb::InterfaceDescriptor) -> bool {
    let class = alt.class_code();
    // 排除已知的非 CMSIS-DAP 接口类
    if class == CDC_CLASS || class == CDC_DATA_CLASS || class == MSC_CLASS
        || class == HUB_CLASS || class == AUDIO_CLASS || class == HID_CLASS {
        return false;
    }
    // 必须有 Bulk IN + Bulk OUT
    let mut has_bulk_in = false;
    let mut has_bulk_out = false;
    for ep in alt.endpoint_descriptors() {
        if ep.transfer_type() == rusb::TransferType::Bulk {
            match ep.direction() {
                rusb::Direction::In => has_bulk_in = true,
                rusb::Direction::Out => has_bulk_out = true,
            }
        }
    }
    has_bulk_in && has_bulk_out
}

/// 找出设备上第一个符合条件的 DAP 候选接口
///
/// `handle` 为 `None`（设备打不开）时接口字符串留空，其余信息照常返回。
fn find_dap_interface(
    device: &Device<Context>,
    handle: Option<&rusb::DeviceHandle<Context>>,
) -> Option<DapInterface> {
    let config = device.config_descriptor(0).ok()?;
    for interface in config.interfaces() {
        for alt in interface.descriptors() {
            if !is_cmsis_dap_candidate(&alt) {
                continue;
            }

            let mut ep_out = 0u8;
            let mut ep_in = 0u8;
            for ep in alt.endpoint_descriptors() {
                if ep.transfer_type() != rusb::TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    rusb::Direction::Out => ep_out = ep.address(),
                    rusb::Direction::In => ep_in = ep.address(),
                }
            }
            if ep_out == 0 || ep_in == 0 {
                continue;
            }

            let name = handle
                .and_then(|h| {
                    alt.description_string_index()
                        .and_then(|i| h.read_string_descriptor_ascii(i).ok())
                })
                .map(|s| s.trim_end_matches(['\0', ' ']).to_string())
                .unwrap_or_default();

            return Some(DapInterface {
                interface_number: alt.interface_number(),
                ep_out,
                ep_in,
                name,
            });
        }
    }
    None
}

/// 设备级字符串里是否出现 CMSIS-DAP 标记（逐段比对，忽略大小写）
fn device_strings_mention_cmsis(
    device: &Device<Context>,
    handle: &rusb::DeviceHandle<Context>,
) -> (bool, bool) {
    let Ok(desc) = device.device_descriptor() else {
        return (false, false);
    };
    let product = read_string_descriptor(handle, desc.product_string_index());
    let manufacturer = read_string_descriptor(handle, desc.manufacturer_string_index());
    (contains_marker(&product), contains_marker(&manufacturer))
}

fn contains_marker(s: &str) -> bool {
    !s.is_empty() && s.to_uppercase().contains(CMSIS_MARKER)
}

struct Identified {
    confidence: Confidence,
    evidence: &'static str,
    interface: DapInterface,
}

/// 判定一台设备是否为 CMSIS-DAP 探针
///
/// 判据按可靠性排序：
/// 1. **接口字符串**含 CMSIS-DAP —— 协议实现自然带出，几乎不会漏写
/// 2. 产品字符串含 CMSIS-DAP —— 厂商可能起别名（如 GD-Link 的
///    `GDLinker_V2`），所以不能只靠它
/// 3. 制造商字符串含 CMSIS-DAP
/// 4. VID/PID 命中已知型号 —— 只能证明「大概率是」，不证明协议能讲通
///
/// 返回 `None` 表示这台设备连 Bulk 端点对都没有，不可能是 DAP v2。
fn identify(
    device: &Device<Context>,
    handle: Option<&rusb::DeviceHandle<Context>>,
) -> Option<Identified> {
    let interface = find_dap_interface(device, handle)?;

    let mut evidence: Option<&'static str> = None;
    if contains_marker(&interface.name) {
        evidence = Some("interface string mentions CMSIS-DAP");
    }

    // 设备级字符串只在能打开时读得到；打不开不影响「结构上像探针」的判断
    if let Some(h) = handle {
        let (product_hit, mfr_hit) = device_strings_mention_cmsis(device, h);
        if product_hit {
            evidence.get_or_insert("product string mentions CMSIS-DAP");
        }
        if mfr_hit {
            evidence.get_or_insert("manufacturer string mentions CMSIS-DAP");
        }
    }

    let confidence = if evidence.is_some() {
        Confidence::Confirmed
    } else {
        match device.device_descriptor() {
            Ok(desc) if is_known_device(desc.vendor_id(), desc.product_id()) => {
                evidence.get_or_insert("VID/PID is in the known-model list");
                Confidence::Likely
            }
            _ => {
                evidence.get_or_insert(if handle.is_none() {
                    "device could not be opened: USB strings unreadable"
                } else {
                    "bulk interface present, but no CMSIS-DAP marker in any USB string"
                });
                Confidence::Possible
            }
        }
    };

    Some(Identified {
        confidence,
        evidence: evidence.unwrap_or("unknown"),
        interface,
    })
}

/// 诊断 USB 总线上的 CMSIS-DAP 候选设备。
///
/// 默认只读取描述符和接口信息；`try_claim=true` 时会短暂 claim 候选接口并立即释放，
/// 用于定位驱动/权限/占用问题。
pub fn diagnose_devices(try_claim: bool) -> Result<Vec<UsbDeviceDiagnostic>> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let mut result = Vec::new();

    for device in devices.iter() {
        let desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        let vid = desc.vendor_id();
        let pid = desc.product_id();
        let known_vid_pid = is_known_device(vid, pid);

        let mut open_ok = false;
        let mut open_error = None;
        let mut manufacturer = String::new();
        let mut product = String::new();
        let mut serial = String::new();
        let mut handle = None;

        match device.open() {
            Ok(h) => {
                open_ok = true;
                manufacturer = read_string_descriptor(&h, desc.manufacturer_string_index());
                product = read_string_descriptor(&h, desc.product_string_index());
                serial = read_string_descriptor(&h, desc.serial_number_string_index());
                handle = Some(h);
            }
            Err(e) => {
                open_error = Some(e.to_string());
            }
        }

        let identified = identify(&device, handle.as_ref());
        // 没有 Bulk 端点对的设备与排障无关，跳过避免刷屏
        let Some(identified) = identified else {
            continue;
        };

        let interface_number = Some(identified.interface.interface_number);
        let ep_out = Some(identified.interface.ep_out);
        let ep_in = Some(identified.interface.ep_in);
        let interface_name = identified.interface.name.clone();

        let mut claim_ok = None;
        let mut claim_error = None;
        if try_claim {
            match (handle.as_ref(), interface_number) {
                (Some(h), Some(iface)) => match h.claim_interface(iface) {
                    Ok(()) => {
                        claim_ok = Some(true);
                        if let Err(e) = h.release_interface(iface) {
                            claim_error = Some(format!("release interface {} failed: {}", iface, e));
                        }
                    }
                    Err(e) => {
                        claim_ok = Some(false);
                        claim_error = Some(e.to_string());
                    }
                },
                (None, _) => {
                    claim_ok = Some(false);
                    claim_error = Some(
                        open_error
                            .clone()
                            .unwrap_or_else(|| "device cannot be opened, cannot claim interface".to_string()),
                    );
                }
                (_, None) => {
                    claim_ok = Some(false);
                    claim_error = Some("no CMSIS-DAP v2 bulk interface found, cannot claim".to_string());
                }
            }
        }

        result.push(UsbDeviceDiagnostic {
            vid,
            pid,
            bus_number: device.bus_number(),
            address: device.address(),
            known_vid_pid,
            open_ok,
            open_error,
            product_has_cmsis_dap: contains_marker(&product),
            interface_has_cmsis_dap: contains_marker(&interface_name),
            interface_name,
            confidence: identified.confidence,
            evidence: identified.evidence,
            manufacturer,
            product,
            serial,
            has_bulk_candidate: true,
            config_error: None,
            interface_number,
            ep_out,
            ep_in,
            claim_ok,
            claim_error,
        });
    }

    result.sort_by(|a, b| a.confidence.cmp(&b.confidence).then(a.bus_number.cmp(&b.bus_number)));
    Ok(result)
}

/// 打开并返回置信度最高的 CMSIS-DAP v2 设备
///
/// 多台设备同时插入时按置信度排序，`Confirmed` 优先于 `Likely`；
/// `Possible`（结构像但无任何证据）永不自动连接，避免误连串口设备。
///
/// 返回 (DeviceHandle, interface_num)，调用方负责在释放时 release_interface。
pub fn open_first_device() -> Result<(rusb::DeviceHandle<Context>, u8)> {
    let context = Context::new()?;

    // 第一遍：只读描述符，算出每台设备的置信度
    let mut candidates: Vec<Candidate> = Vec::new();
    {
        let devices = context.devices()?;
        for device in devices.iter() {
            let handle = device.open().ok();
            let Some(identified) = identify(&device, handle.as_ref()) else {
                continue;
            };
            if !identified.confidence.auto_connectable() {
                log::debug!(
                    "skipping {:04X}:{:04X} ({}: {})",
                    device.device_descriptor().map(|d| d.vendor_id()).unwrap_or(0),
                    device.device_descriptor().map(|d| d.product_id()).unwrap_or(0),
                    identified.confidence.label(),
                    identified.evidence
                );
                continue;
            }
            candidates.push(Candidate {
                confidence: identified.confidence,
                bus_number: device.bus_number(),
                address: device.address(),
                interface_number: identified.interface.interface_number,
                evidence: identified.evidence,
            });
        }
    }

    candidates.sort_by_key(|c| c.confidence);
    let Some(best) = candidates.first() else {
        return Err(Error::DeviceNotFound);
    };

    log::info!(
        "selected device bus={} addr={} ({}: {})",
        best.bus_number,
        best.address,
        best.confidence.label(),
        best.evidence
    );

    // 第二遍：按 (bus, address) 定位并 claim
    let devices = context.devices()?;
    for device in devices.iter() {
        if device.bus_number() != best.bus_number || device.address() != best.address {
            continue;
        }
        let handle = device.open()?;
        log::info!("claiming interface {}", best.interface_number);
        handle.claim_interface(best.interface_number)?;
        return Ok((handle, best.interface_number));
    }

    Err(Error::DeviceNotFound)
}

struct Candidate {
    confidence: Confidence,
    bus_number: u8,
    address: u8,
    interface_number: u8,
    evidence: &'static str,
}

/// 查找指定接口上的 Bulk 端点地址
pub fn find_bulk_endpoints(
    handle: &rusb::DeviceHandle<Context>,
    interface_number: u8,
) -> Result<(u8, u8)> {
    let device = handle.device();
    let config = device.config_descriptor(0)?;
    for interface in config.interfaces() {
        for alt in interface.descriptors() {
            if alt.interface_number() != interface_number || !is_cmsis_dap_candidate(&alt) {
                continue;
            }
            let mut ep_out = 0u8;
            let mut ep_in = 0u8;
            for ep in alt.endpoint_descriptors() {
                if ep.transfer_type() != rusb::TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    rusb::Direction::Out => ep_out = ep.address(),
                    rusb::Direction::In => ep_in = ep.address(),
                }
            }
            if ep_out != 0 && ep_in != 0 {
                return Ok((ep_out, ep_in));
            }
        }
    }
    Err(Error::DeviceNotFound)
}

/// 读取 USB 字符串描述符（仅 ASCII；厂商/产品字符串通常为 ASCII）
///
/// 部分固件（GD-Link 实测）会按固定长度上报字符串，尾部补 NUL，
/// 显示成不可见空格；不裁剪会导致按序列号匹配设备时失败。
fn read_string_descriptor(
    handle: &rusb::DeviceHandle<Context>,
    idx: Option<u8>,
) -> String {
    match idx {
        Some(i) if i > 0 => {
            // 尝试读 ASCII（厂商/产品字符串通常为 ASCII）
            match handle.read_string_descriptor_ascii(i) {
                Ok(s) => s.trim_end_matches(['\0', ' ']).to_string(),
                Err(_) => String::new(),
            }
        }
        _ => String::new(),
    }
}
