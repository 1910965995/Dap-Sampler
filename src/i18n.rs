//! 轻量级多语言支持（gettext 风格）
//!
//! 设计要点（面向后续扩展新语言）：
//! - **英文即 key**：用户可见文本以英文原文作为查找键，`t(key)` 在英文
//!   模式下直接返回 key 本身——新增界面字符串无需先补任何翻译即可工作；
//! - **每个语言一张 `(key, 译文)` 表**：新增语言只需在 `LANGUAGE_TABLES`
//!   加一张表 + 一个 `Language` 枚举成员，查找逻辑零改动；
//! - **缺失回退**：某语言缺某条翻译时回退英文 key，界面永不出现空文本；
//! - **带参翻译**：`tf(key, args)` 按顺序把译文中的 `{}` 替换为参数——
//!   数值/精度由调用方先 `format!` 好再传入，键中只保留 `{}` 占位；
//! - 语言状态是全局 `AtomicU8`（egui UI 单线程使用），设置窗口切换后
//!   下一帧全部文本即切换。

use std::sync::atomic::{AtomicU8, Ordering};

/// 支持的语言
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    Chinese,
}

/// 全部语言（设置窗口下拉框按此顺序展示）
pub const ALL_LANGUAGES: [Language; 2] = [Language::English, Language::Chinese];

impl Language {
    /// 语言的自命名（国际化惯例：下拉框里用各自语言显示）
    pub fn label(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Chinese => "简体中文",
        }
    }

    /// 存储用语言代码（工程文件持久化用）
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Chinese => "zh",
        }
    }

    /// 从语言代码解析（未知代码回退英文）
    pub fn from_code(code: &str) -> Language {
        match code {
            "zh" => Language::Chinese,
            _ => Language::English,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 切换当前语言（下一帧全部文本生效）
pub fn set_language(lang: Language) {
    CURRENT.store(lang as u8, Ordering::SeqCst);
}

/// 当前语言
pub fn current() -> Language {
    match CURRENT.load(Ordering::SeqCst) {
        1 => Language::Chinese,
        _ => Language::English,
    }
}

/// 翻译一段用户可见文本
///
/// 英文模式直接返回 key 本身（英文即 key）；其他语言查对应表，缺失条目
/// 回退英文——新增字符串/新语言永远不会因漏翻而出现空文本。
pub fn t(key: &'static str) -> &'static str {
    match current() {
        Language::English => key,
        Language::Chinese => lookup(ZH_TABLE, key).unwrap_or(key),
    }
}

/// 翻译 + 按顺序填空：译文中的 `{}` 依次替换为 `args`。
///
/// 例：`tf("Sample rate changed to {} Hz", &[&rate.to_string()])`
/// 数值精度由调用方先 `format!` 好再传入（如 `&format!("{:.6}", v)`），
/// 键里只写 `{}` 占位。翻译时请保持占位符的相对顺序。
pub fn tf(key: &'static str, args: &[&str]) -> String {
    let mut s = t(key).to_string();
    for a in args {
        s = s.replacen("{}", a, 1);
    }
    s
}

/// 查表：线性扫描。表约百余条、每帧调用量为可见文本数（百余次），
/// 总代价微秒级，远低于一帧的渲染成本，无需哈希表/排序二分。
fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
}

/// 中文翻译表（key = 英文原文）
///
/// 值与英文完全相同的条目可以不写（回退即英文）。
/// 带参数的 key 中 `{}` 为占位符，顺序必须与英文一致。
const ZH_TABLE: &[(&str, &str)] = &[
    // ---- 菜单栏 ----
    ("Project(P)", "工程(P)"),
    ("New Project", "新建工程"),
    ("Open Project…", "打开工程…"),
    ("Save Project", "保存工程"),
    ("Files(F)", "文件(F)"),
    ("Import ELF File…", "导入 ELF 文件…"),
    ("Export data to…", "导出数据到…"),
    ("Oscilloscope(O)", "示波器(O)"),
    ("New Oscilloscope", "新建示波器"),
    ("Settings(S)", "设置(S)"),
    ("General Settings…", "常规设置…"),
    ("Help(H)", "帮助(H)"),
    ("About", "关于"),
    // ---- 采集控制 / 参数 ----
    ("\u{25b6} Start", "▶ 开始"),
    ("\u{23f9} Stop", "⏹ 停止"),
    ("\u{23f8} Follow: ON", "⏸ 跟随: 开"),
    ("\u{25b6} Follow: OFF", "▶ 跟随: 关"),
    ("Rate:", "采样率:"),
    ("Window:", "窗口:"),
    ("SWD Clock:", "SWD 时钟:"),
    ("Display:", "显示:"),
    ("Line", "折线"),
    ("Point", "散点"),
    ("Language:", "语言:"),
    ("Language changed", "语言已切换"),
    // ---- 通用按钮 / 窗口 ----
    ("Yes", "是"),
    ("No", "否"),
    ("Change ELF…", "更换 ELF…"),
    ("General Settings", "常规设置"),
    ("Buffer capacity:", "缓冲容量:"),
    ("≈ {} s of history @ {} Hz", "≈ {} 秒历史数据 @ {} Hz"),
    ("Current ELF:", "当前 ELF:"),
    ("No ELF file loaded", "未加载 ELF 文件"),
    ("Warning", "警告"),
    ("Error", "错误"),
    ("About DAP Sampler", "关于 DAP Sampler"),
    (
        "High-speed variable sampler & visualization tool\nbased on the CMSIS-DAP v2 protocol for ARM Cortex-M",
        "基于 CMSIS-DAP v2 协议的高速变量采样\n与可视化工具（面向 ARM Cortex-M）",
    ),
    ("Version: v{}", "版本: v{}"),
    // ---- 工程 / ELF ----
    ("New project created (config cleared)", "已新建工程（配置已清空）"),
    ("ELF loaded: {} variables", "ELF 已加载: {} 个变量"),
    ("ELF load failed: {}", "ELF 加载失败: {}"),
    ("ELF reloaded: {} variables", "ELF 已重新加载: {} 个变量"),
    ("ELF reload failed: {}", "ELF 重新加载失败: {}"),
    ("ELF file is missing: {}", "ELF 文件缺失: {}"),
    ("Data exported: {} samples to {}", "已导出数据: {} 个采样点到 {}"),
    ("No data to export: buffer is empty", "没有可导出的数据: 缓冲区为空"),
    ("Failed to export data: {}", "导出数据失败: {}"),
    ("Project saved: {}", "工程已保存: {}"),
    ("Failed to save project: {}", "保存工程失败: {}"),
    ("Project opened: {}", "工程已打开: {}"),
    ("Failed to read project file: {}", "读取工程文件失败: {}"),
    (
        "{} variables in project not found in ELF, skipped: {}",
        "工程中 {} 个变量在 ELF 中未找到，已跳过: {}",
    ),
    ("Using saved variable addresses (ELF not synchronized)", "使用工程记录的地址（ELF 未同步）"),
    ("ELF synchronized: {} addresses updated, {} variables removed", "ELF 已同步: 更新 {} 个地址，删除 {} 个变量"),
    ("ELF Synchronization", "ELF 同步"),
    ("The ELF file has changed. Should it be synchronized?", "ELF 文件已变化，是否同步？"),
    ("• {} variables have different addresses", "• {} 个变量地址不一致"),
    ("• {} variables not found in the ELF file", "• {} 个变量在 ELF 文件中未找到"),
    ("ELF Changed", "ELF 已变化"),
    ("The ELF file has changed on disk. Reload it?", "磁盘上的 ELF 文件已变化，是否重新加载？"),
    (
        "ELF file not found or failed to load:\n{}\n\nThe project will continue with saved variable addresses.",
        "ELF 文件未找到或加载失败:\n{}\n\n工程将继续使用已保存的变量地址。",
    ),
    // ---- 采集 ----
    ("Acquisition started", "开始采集"),
    ("Acquisition stopped", "采集已停止"),
    (
        "DAP-Link did not confirm disconnect: the device may still appear occupied to other tools. Unplug and replug the DAP-Link if Keil cannot connect.",
        "DAP-Link 未确认断开：设备在其他工具中可能仍显示被占用。若 Keil 无法连接，请重新插拔 DAP-Link。",
    ),
    ("Failed to start acquisition: {}", "启动采集失败: {}"),
    ("Acquisition aborted: {}", "采集异常中止: {}"),
    ("Buffer capacity changed to {} points (applies on next Start)", "缓冲容量已改为 {} 点（下次开始采集时生效）"),
    // ---- 连接排障提示 ----
    (
        "Note: Check that the DAP-Link is connected, the WinUSB driver is installed, the device is not occupied by Keil/OpenOCD/pyOCD or other programs, the target board is powered, and the SWD wiring is correct; or try a lower SWD clock.",
        "注意: 请检查 DAP-Link 是否连接、WinUSB 驱动是否已安装、设备是否被 Keil/OpenOCD/pyOCD 等程序占用、目标板是否供电、SWD 接线是否正确；或尝试降低 SWD 时钟。",
    ),
    // ---- 示波器 scope ----
    ("Scope {}", "示波器 {}"),
    ("Switched to {}", "已切换到 {}"),
    ("Switched to {} (no channels: press Start after adding variables)", "已切换到 {}（无通道: 添加变量后点击开始）"),
    ("Last oscilloscope removed, created a new empty one", "已删除最后一个示波器，新建了一个空示波器"),
    ("Oscilloscope removed, now on {}", "示波器已删除，当前为 {}"),
    // ---- 变量 / 通道 ----
    ("No sampling channel selected: load an ELF and check variables, or specify addresses with --addresses",
        "未选择采样通道: 请加载 ELF 并勾选变量，或用 --addresses 指定地址"),
    ("Channels selected but no matching variable addresses found: check ELF symbols/debug info",
        "已选择通道但未找到匹配的变量地址: 请检查 ELF 符号/调试信息"),
    ("Channels ({}/{})", "通道（{}/{}）"),
    ("No channels", "没有通道"),
    ("Only {} channels can be watched at a time. {}: {}", "一次最多观测 {} 个通道。{}: {}"),
    ("Only {} channels can be watched at a time.\n{}: {}", "一次最多观测 {} 个通道。\n{}: {}"),
    ("Unselected channel variable", "超限未加入的通道变量"),
    ("Unselected channel variables", "超限未加入的通道变量"),
    ("Changed variable", "变更的变量"),
    ("Changed variables", "变更的变量"),
    ("Channel Limit", "通道数量超限"),
    ("Acquisition Running", "采集运行中"),
    ("Stop acquisition before changing watched variables", "请先停止采集再修改观测变量"),
    ("Stop acquisition before changing watched variables.\n{}: {}", "请先停止采集再修改观测变量。\n{}: {}"),
    ("Stop acquisition before removing channels", "请先停止采集再删除通道"),
    ("Globals", "全局变量"),
    ("🔍 Search variable name...", "🔍 搜索变量名..."),
    ("Remove channel", "删除通道"),
    ("Hide channel", "隐藏通道"),
    ("Show channel", "显示通道"),
    // ---- Watch 面板 ----
    ("Name", "名称"),
    ("Address", "地址"),
    ("Type", "类型"),
    ("Value", "值"),
    ("Refresh (s)", "刷新 (s)"),
    ("Y Offset", "Y 偏移"),
    ("Y Scale", "Y 缩放"),
    ("Remark", "备注"),
    ("Watch Window", "Watch 窗口"),
    ("No watch variables", "没有 Watch 变量"),
    ("Import ELF or add channels to start watching", "导入 ELF 或添加通道后开始观测"),
    // ---- 参数变更提示 ----
    ("Sample rate changed to {} Hz", "采样率已改为 {} Hz"),
    ("SWD clock changed to {} MHz (applies on next DAP-Link connection)", "SWD 时钟已改为 {} MHz（下次连接 DAP-Link 时生效）"),
    ("Display window size changed to {} points", "显示窗口大小已改为 {} 点"),
    ("Display mode switched to {}", "显示模式已切换为 {}"),
    // ---- 写入 ----
    ("Write failed: no sampling channels", "写入失败: 没有采样通道"),
    ("Write to '{}' was skipped: {}", "对 '{}' 的写入已跳过: {}"),
    // ---- 波形叠加信息 ----
    ("Actual Rate: {} Hz ({}%)", "实际采样率: {} Hz（{}%）"),
    ("Samples: {} ({}s)", "采样点: {}（{} 秒）"),
    ("Progress: {}%", "进度: {}%"),
    ("Overwritten: {}", "已覆盖: {}"),
    (
        "DAP-Link throughput insufficient, actual sample rate is below target.\nThe waveform time axis uses real timestamps, so the shape should be correct.\nLower the sample rate for more uniform sampling intervals.",
        "DAP-Link 吞吐不足，实际采样率低于目标值。\n波形时间轴使用真实时间戳，形状应仍然正确。\n可降低采样率以获得更均匀的采样间隔。",
    ),
    (
        "UI consumption is slower than sampling, the ring buffer has overwritten old samples (data loss).\nUsually caused by window dragging, modal dialogs or an overly high sample rate.",
        "界面消费速度慢于采样速度，环形缓冲已覆盖旧数据（数据丢失）。\n通常由窗口拖动、模态弹窗或采样率过高引起。",
    ),
    // ---- 光标面板 ----
    ("Delta", "差值"),
    ("Cursor 1", "光标 1"),
    ("Cursor 2", "光标 2"),
    ("⚠ Cursor 1 @ seq {} slid out of buffer range {}", "⚠ 光标 1（序号 {}）已滑出缓冲区范围 {}"),
    ("⚠ Cursor 2 slid out of buffer range", "⚠ 光标 2 已滑出缓冲区范围"),
    ("(buffer cleared)", "（缓冲已清空）"),
    ("Click waveform to place cursor", "点击波形放置光标"),
    // ---- 写入失败（按场景细分）----
    ("Write failed: channel '{}' is not in the current sampling set", "写入失败: 通道 '{}' 不在当前采样集合中"),
    ("Write failed: SWD initialization failed: {}", "写入失败: SWD 初始化失败: {}"),
    ("Write failed: channel '{}' is not in the Watch list", "写入失败: 通道 '{}' 不在 Watch 列表中"),
    ("Write failed for channel '{}': {}", "写入通道 '{}' 失败: {}"),
    ("Write failed: cannot connect to DAP-Link: {}", "写入失败: 无法连接 DAP-Link: {}"),
    // ---- 波形坐标轴 ----
    ("Time (s)", "时间（秒）"),
    ("Time (s) [window: {}s]", "时间（秒）[窗口: {} 秒]"),
    // ---- 波形空态 / 侧栏标签 ----
    ("Waiting for data...", "等待数据..."),
    ("Press Start to begin sampling", "点击「开始」以开始采样"),
    ("Variables", "变量"),
    ("Channels", "通道"),
    ("Cursor", "光标"),
    ("ELF load failed, continuing with saved variable addresses: {}", "ELF 加载失败，继续使用已保存的变量地址: {}"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_returns_key_itself() {
        set_language(Language::English);
        assert_eq!(t("Acquisition started"), "Acquisition started");
        // 表中缺失的 key 也回退为 key 本身
        assert_eq!(t("a string that is not in any table"), "a string that is not in any table");
    }

    #[test]
    fn chinese_translates_known_keys() {
        set_language(Language::Chinese);
        assert_eq!(t("Acquisition started"), "开始采集");
        assert_eq!(t("Project(P)"), "工程(P)");
        assert_eq!(t("\u{25b6} Start"), "▶ 开始");
        set_language(Language::English);
    }

    #[test]
    fn chinese_falls_back_to_english_for_missing_keys() {
        set_language(Language::Chinese);
        assert_eq!(t("a string that is not in any table"), "a string that is not in any table");
        set_language(Language::English);
    }

    #[test]
    fn tf_replaces_placeholders_in_order() {
        set_language(Language::English);
        assert_eq!(
            tf("Sample rate changed to {} Hz", &["5000"]),
            "Sample rate changed to 5000 Hz"
        );
        set_language(Language::Chinese);
        assert_eq!(
            tf("Sample rate changed to {} Hz", &["5000"]),
            "采样率已改为 5000 Hz"
        );
        // 多参数按顺序替换
        assert_eq!(
            tf("Data exported: {} samples to {}", &["100", "C:/out.csv"]),
            "已导出数据: 100 个采样点到 C:/out.csv"
        );
        set_language(Language::English);
    }

    #[test]
    fn zh_table_has_no_duplicate_keys() {
        // 线性查找取第一条匹配：重复 key 会让后面的条目永远失效，属于笔误
        for (i, (k1, _)) in ZH_TABLE.iter().enumerate() {
            for (k2, _) in ZH_TABLE[i + 1..].iter() {
                assert_ne!(k1, k2, "重复的翻译 key: {k1:?}");
            }
        }
    }

    #[test]
    fn language_code_round_trip() {
        for lang in ALL_LANGUAGES {
            assert_eq!(Language::from_code(lang.code()), lang);
        }
        assert_eq!(Language::from_code("fr"), Language::English);
    }
}
