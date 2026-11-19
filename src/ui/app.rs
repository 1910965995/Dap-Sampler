use std::sync::Arc;
use std::time::Instant;
use crate::pipeline::sample::Sample;
use crate::pipeline::sample::ValueType;
use crate::pipeline::engine::{PipelineEngine, PipelineHandle};
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::swd::SwdLink;
use crate::elf::{ElfContext, ElfParser, ElfVariable};
use super::display_buffer::DisplayBuffer;
use super::waveform::{WaveformPanel, WaveformDisplayMode};
use super::controls::{ControlPanel, AcquisitionState, AcquisitionCommand};
use super::cursor::{CursorResult, CursorState};
use super::variable_browser::VariableBrowser;
use super::channel_panel::ChannelPanel;
use super::watch_panel::WatchPanel;
use crate::project::{ProjectFile, ProjectVariable, PROJECT_VERSION};

/// Idle 状态下 ELF 文件变化检测的轮询间隔
const ELF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 示波器风格通道色板（v1.1.0 配色：红/绿/蓝/橙/品红/青/灰/粉）
const CHANNEL_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(255, 68, 68),   // 红
    egui::Color32::from_rgb(68, 255, 68),   // 绿
    egui::Color32::from_rgb(68, 68, 255),   // 蓝
    egui::Color32::from_rgb(255, 200, 68),  // 橙
    egui::Color32::from_rgb(255, 68, 255),  // 品红
    egui::Color32::from_rgb(68, 255, 255),  // 青
    egui::Color32::from_rgb(180, 180, 180), // 灰
    egui::Color32::from_rgb(255, 150, 150), // 粉
];

/// DAP Sampler 主应用
pub struct DapSamplerApp {
    pipeline: Option<PipelineHandle>,
    /// 活跃的 USB 连接（Idle 时为 None，Start 时建立，Stop 时释放）
    active_usb: Option<Arc<BulkTransfer>>,
    /// 手工模式地址（通过 --addresses 传入）
    manual_addresses: Vec<u32>,
    /// 采集开始时间（用于计算实际采样率）
    acquisition_start: Option<Instant>,
    display_buf: DisplayBuffer,
    waveform: WaveformPanel,
    controls: ControlPanel,
    cursor: CursorState,
    temp_buf: Vec<Sample>,
    interval_us: f64,
    has_new_data: bool,

    // P4: ELF / 变量浏览器
    elf_ctx: Option<ElfContext>,
    variable_browser: VariableBrowser,
    channel_panel: ChannelPanel,
    /// Watch 面板（示波器下方）
    watch_panel: WatchPanel,
    /// 左侧面板各模块的展开/收起状态
    var_browser_open: bool,
    channels_open: bool,
    cursor_open: bool,
    #[allow(dead_code)]
    manual_channel_names: Vec<String>,
    manual_value_types: Vec<ValueType>,
    rate_hz: u32,
    /// SWD 时钟频率（MHz），由 ControlPanel 修改后同步到本字段
    swd_clock_mhz: u32,
    /// 当前采集的通道地址列表（与通道一一对应，用于 Watch 写入）
    current_addresses: Vec<u32>,
    /// 当前采集的通道名列表（与 current_addresses 一一对应）。
    /// Watch 写入按名字解析到引擎快照索引：行号在通道增删后会移位，
    /// 名字是稳定标识。
    current_names: Vec<String>,
    /// 当前采集的通道类型列表（与 current_addresses 一一对应，
    /// 用于子字变量的对齐读写）
    current_types: Vec<ValueType>,
    #[allow(dead_code)]
    target_count: Option<u64>,
    /// 波形显示模式
    display_mode: WaveformDisplayMode,
    /// 显示窗口大小（采样点数）
    window_size: usize,
    /// 底部状态栏消息（None = 空）
    status_text: Option<String>,
    /// 状态消息是否为错误（错误显示红色）
    status_is_error: bool,
    /// 侧栏选中的通道（波形加粗高亮；再次点击取消）
    selected_channel: Option<usize>,
    /// 当前工程文件路径（None = 未保存过；保存时已有路径则直接覆盖）
    project_path: Option<String>,
    /// 当前悬停展开的菜单（None = 无；鼠标移出按钮与菜单区域后自动收起）
    hover_menu: Option<&'static str>,
    /// 缓冲区容量（历史保留量，采样点数上限；下次 Start 生效）
    buffer_capacity: usize,
    /// 设置窗口是否显示（设置 → 常规设置）
    show_settings: bool,
    /// 关于窗口是否显示（帮助 → 关于）
    show_about: bool,
    /// ELF 加载失败警告弹窗内容（Some = 显示；打开工程时 ELF 缺失时弹出）
    elf_warning: Option<String>,
    /// 警告弹窗是否显示（右上角 X 关闭）
    show_elf_warning: bool,
    /// 通道数上限弹窗内容（Some = 显示；勾选超过 8 个变量时弹出）
    channel_limit_warning: Option<String>,
    /// 打开工程时的 ELF 同步询问（Some = 弹窗挂起等待用户选择）
    elf_sync_prompt: Option<ElfSyncPrompt>,
    /// Idle 状态下检测 ELF 文件变化的弹窗（文件 mtime 变化）
    elf_change_prompt: bool,
    /// 上次记录的 ELF 文件修改时间（变化检测基准）
    elf_mtime: Option<std::time::SystemTime>,
    /// ELF 同步弹窗是否显示（右上角 X 关闭 → 等价 No）
    elf_sync_open: bool,
    /// 最近一次打开工程时的变量列表（同步弹窗选 No 时用工程地址恢复）
    last_project_variables: Vec<ProjectVariable>,
}

/// 截断长路径：超过 max_chars 字符时保留前面部分并追加 "..."
fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut s: String = path.chars().take(keep).collect();
    s.push_str("...");
    s
}

/// 打开工程时 ELF 与工程变量的差异（触发同步询问弹窗）
#[derive(Clone)]
struct ElfSyncPrompt {
    /// 地址不同的变量：(变量名, 工程地址, ELF 地址)
    address_diffs: Vec<(String, u32, u32)>,
    /// ELF 中不存在的变量名（选 Yes 时删除）
    missing: Vec<String>,
}

/// ELF 变量是否匹配给定名称（path 或 name 相等）
fn elf_var_matches(ev: &ElfVariable, name: &str) -> bool {
    ev.path == name || ev.name == name
}

/// 比较工程变量与 ELF 变量：地址不同或 ELF 中缺失 → 返回差异；无差异 → None
///
/// 匹配规则与 collect_channel_addresses 一致：path 或 name 相等即匹配。
/// 将写入请求的通道名解析为采集快照中的变量索引
///
/// 名字是 Watch 写入的稳定标识：通道增删后表格行号整体移位，
/// 按行号写入会命中错误的地址；按名字解析失败则该通道已不在
/// 采集集合中，调用方应丢弃写入。
fn resolve_channel_index(names: &[String], name: &str) -> Option<usize> {
    names.iter().position(|n| n == name)
}

/// 比较工程变量与 ELF 变量：地址不同或 ELF 中缺失 → 返回差异；无差异 → None
///
/// 匹配规则与 collect_channel_addresses 一致：path 或 name 相等即匹配。
fn compute_elf_diffs(
    project_vars: &[ProjectVariable],
    elf_vars: &[ElfVariable],
) -> Option<ElfSyncPrompt> {
    let mut address_diffs = Vec::new();
    let mut missing = Vec::new();
    for pv in project_vars {
        match elf_vars
            .iter()
            .find(|ev| elf_var_matches(ev, &pv.name))
        {
            Some(ev) if ev.address != pv.address => {
                address_diffs.push((pv.name.clone(), pv.address, ev.address));
            }
            Some(_) => {}
            None => missing.push(pv.name.clone()),
        }
    }
    if address_diffs.is_empty() && missing.is_empty() {
        None
    } else {
        Some(ElfSyncPrompt { address_diffs, missing })
    }
}

/// 菜单项：整行（全宽 × 22px）可点击、文字左对齐
///
/// 不能用 `add_sized`（其内部强制居中布局），需用 `allocate_ui`
/// 继承菜单的左对齐布局，否则文字会居中显示。
fn menu_item(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.allocate_ui(egui::vec2(ui.available_width(), 22.0), |ui| {
        ui.add(egui::SelectableLabel::new(false, text))
    })
    .inner
}

/// 菜单按钮：默认无底色（与菜单栏背景一致），悬停或展开时
/// 用与普通按钮相同的高亮色（widgets.hovered.weak_bg_fill）。
///
/// 不能用 `SelectableLabel`（其选中/悬停背景取自 selection.bg_fill，
/// 默认主题下是蓝色），需手动绘制背景。
fn menu_button(ui: &mut egui::Ui, title: &str, open: bool) -> egui::Response {
    let galley = egui::WidgetText::from(egui::RichText::new(title))
        .into_galley(ui, None, f32::INFINITY, egui::TextStyle::Button);
    let padding = ui.spacing().button_padding;
    let size = galley.size() + padding * 2.0;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.visuals();
        if open || response.hovered() || response.is_pointer_button_down_on() {
            let bg = if response.is_pointer_button_down_on() {
                visuals.widgets.active.weak_bg_fill
            } else {
                visuals.widgets.hovered.weak_bg_fill
            };
            ui.painter().rect(
                rect,
                visuals.widgets.hovered.corner_radius,
                bg,
                egui::Stroke::NONE,
                egui::StrokeKind::Inside,
            );
        }
        let text_pos = rect.min + padding;
        ui.painter().galley(text_pos, galley, visuals.text_color());
    }
    response
}

impl DapSamplerApp {
    pub fn new(
        manual_addresses: Vec<u32>,
        addresses: Vec<String>,
        rate_hz: u32,
        target_count: Option<u64>,
        elf_ctx: Option<ElfContext>,
        manual_types: Vec<ValueType>,
        swd_clock_mhz: u32,
    ) -> Self {
        let interval_us = 1_000_000.0 / rate_hz as f64;

        let manual_channel_names: Vec<String> = addresses
            .iter()
            .enumerate()
            .map(|(i, a)| format!("CH{} {}", i + 1, a))
            .collect();
        let manual_value_types: Vec<ValueType> = if manual_types.is_empty() {
            vec![ValueType::Uint32; addresses.len()]
        } else {
            manual_types
        };

        let display_mode = WaveformDisplayMode::Line;

        let num_channels = manual_channel_names.len();
        let channel_colors: Vec<egui::Color32> = (0..num_channels)
            .map(|i| CHANNEL_COLORS[i % CHANNEL_COLORS.len()])
            .collect();

        let waveform = if !manual_channel_names.is_empty() {
            WaveformPanel::new(
                manual_channel_names.clone(),
                channel_colors,
                interval_us,
                manual_value_types.clone(),
                display_mode,
            )
        } else {
            WaveformPanel::new(vec![], vec![], interval_us, vec![], display_mode)
        };

        let mut channel_panel = ChannelPanel::new();
        if elf_ctx.is_none() {
            for (i, name) in manual_channel_names.iter().enumerate() {
                let color = CHANNEL_COLORS[i % CHANNEL_COLORS.len()];
                let vt = manual_value_types.get(i).copied().unwrap_or(ValueType::Uint32);
                channel_panel.add_channel(name.clone(), color, vt);
            }
        }

        // 初始化 Watch 面板
        let mut watch_panel = WatchPanel::new();
        if !manual_channel_names.is_empty() {
            watch_panel.sync_from_channels(&manual_channel_names, &manual_value_types, &manual_addresses);
        }

        // --elf 启动路径：构造时即记录 mtime 快照，避免首帧 Idle 误判文件变化
        let initial_elf_mtime = elf_ctx
            .as_ref()
            .and_then(|c| std::fs::metadata(&c.file_path).ok())
            .and_then(|m| m.modified().ok());

        Self {
            pipeline: None,
            active_usb: None,  // USB 连接推迟到 Start 时建立
            manual_addresses,
            acquisition_start: None,
            display_buf: DisplayBuffer::new(crate::ui::controls::DEFAULT_BUFFER_CAPACITY),
            waveform,
            controls: ControlPanel::new(rate_hz, target_count, swd_clock_mhz),
            cursor: CursorState::new(),
            temp_buf: (0..1024).map(|_| Sample::default()).collect(),
            interval_us,
            has_new_data: false,
            elf_ctx,
            variable_browser: VariableBrowser::new(),
            channel_panel,
            watch_panel,
            var_browser_open: true,
            channels_open: true,
            cursor_open: true,
            manual_channel_names,
            manual_value_types,
            rate_hz,
            swd_clock_mhz,
            current_addresses: Vec::new(),
            current_names: Vec::new(),
            current_types: Vec::new(),
            target_count,
            display_mode,
            window_size: 2000,
            // 启动时状态栏默认显示 Ready
            status_text: Some("Ready".to_string()),
            status_is_error: false,
            selected_channel: None,
            project_path: None,
            hover_menu: None,
            buffer_capacity: crate::ui::controls::DEFAULT_BUFFER_CAPACITY,
            show_settings: false,
            show_about: false,
            elf_warning: None,
            show_elf_warning: false,
            channel_limit_warning: None,
            elf_sync_prompt: None,
            elf_change_prompt: false,
            elf_mtime: initial_elf_mtime,
            elf_sync_open: false,
            last_project_variables: Vec::new(),
        }
    }

    /// 记录一条错误消息（显示在底部状态栏，红色）
    fn set_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::error!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = true;
    }

    /// 记录一条普通状态消息（显示在底部状态栏，默认文字色）
    fn set_status(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = false;
    }

    /// 新建工程：清空当前全部配置，参数回默认值
    fn new_project(&mut self) {
        self.status_text = None;
        self.status_is_error = false;
        self.elf_warning = None;
        self.show_elf_warning = false;
        self.elf_sync_prompt = None;
        self.elf_change_prompt = false;
        self.elf_sync_open = false;
        self.last_project_variables = Vec::new();
        self.elf_mtime = None;
        self.stop_acquisition();
        // 清空 ELF / 通道 / Watch / 手工模式
        self.elf_ctx = None;
        self.variable_browser.clear();
        self.channel_panel.channels.clear();
        self.watch_panel.entries.clear();
        self.watch_panel.reset_col_widths();
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();
        self.current_addresses.clear();
        self.current_names.clear();
        self.current_types.clear();
        // 参数回默认（与 DapSamplerApp::new 一致：5000 Hz / 2000 点 / 1 MHz / Line）
        self.rate_hz = 5000;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.window_size = 2000;
        self.buffer_capacity = crate::ui::controls::DEFAULT_BUFFER_CAPACITY;
        self.controls.sample_rate = self.rate_hz;
        self.controls.window_size = self.window_size;
        self.controls.swd_clock_mhz = 1;
        self.swd_clock_mhz = 1;
        let mode = WaveformDisplayMode::Line;
        self.display_mode = mode;
        self.waveform = WaveformPanel::new(vec![], vec![], self.interval_us, vec![], mode);
        // 清空运行时状态
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
        self.cursor.clear();
        self.controls.update_count(0);
        self.controls.set_stopped();
        self.selected_channel = None;
        self.controls.target_count = None;
        self.project_path = None;
        self.set_status("New project created (config cleared)");
    }

    /// 弹出 ELF 文件选择对话框并加载
    ///
    /// 共用入口：Files → Import ELF File… 与 Settings → Change ELF…
    fn import_elf(&mut self) {
        // 重新导入 ELF 时清除挂起的文件变化弹窗（防止残留弹窗误导）
        self.elf_change_prompt = false;
        self.elf_sync_prompt = None;
        self.elf_sync_open = false;
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("ELF", &["elf", "axf", "out", ""])
            .pick_file()
        {
            match ElfParser::load(&path) {
                Ok(ctx) => {
                    let variable_count = ctx.variables.len();
                    self.elf_ctx = Some(ctx);
                    self.refresh_elf_mtime();
                    self.set_status(format!("ELF loaded: {} variables", variable_count));
                }
                Err(e) => {
                    self.set_error(format!("ELF load failed: {}", e));
                }
            }
        }
    }

    /// 将缓冲区全部数据导出为 CSV（Files → Export data to…）
    fn export_data(&mut self) {
        if self.display_buf.is_empty() {
            self.set_error("No data to export: buffer is empty");
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("dap-sampler-data.csv")
            .save_file()
        else {
            return; // 用户取消
        };
        // 借用作用域：display_buf / waveform 的借用在此结束，之后才能 &mut self 反馈状态
        let result = {
            let samples = self.display_buf.all();
            // 列定义用采集快照（Start 时创建，与 Sample.values() 顺序一致）
            let names: Vec<String> = self
                .waveform
                .channel_names()
                .into_iter()
                .map(String::from)
                .collect();
            let types = self.waveform.value_types().to_vec();
            std::fs::File::create(&path)
                .and_then(|f| crate::export::write_csv(f, samples, &names, &types))
        };
        match result {
            Ok(n) => self.set_status(format!("Data exported: {} samples to {}", n, path.display())),
            Err(e) => self.set_error(format!("Failed to export data: {}", e)),
        }
    }

    /// 保存当前配置为工程文件（已有路径直接覆盖，否则弹保存对话框）
    fn save_project(&mut self) {
        let file = ProjectFile {
            version: PROJECT_VERSION,
            elf_path: self.elf_ctx.as_ref().map(|c| c.file_path.clone()),
            sample_rate_hz: self.controls.sample_rate,
            swd_clock_mhz: self.controls.swd_clock_mhz,
            window_size: self.controls.window_size,
            buffer_capacity: self.buffer_capacity,
            display_mode: self.waveform.display_mode().label().to_string(),
            watch_col_widths: self.watch_panel.col_widths().to_vec(),
            variables: self
                .watch_panel
                .entries
                .iter()
                .map(|e| ProjectVariable {
                    name: e.name.clone(),
                    address: e.address,
                    value_type: e.value_type.label().to_string(),
                    refresh_period_s: e.refresh_period_s,
                    y_offset: e.y_offset,
                    y_scale: e.y_scale,
                    remark: e.remark.clone(),
                })
                .collect(),
        };
        let json = match file.to_json() {
            Ok(j) => j,
            Err(e) => {
                self.set_error(e.to_string());
                return;
            }
        };

        // 目标路径：已保存过则直接覆盖；否则弹保存对话框
        let path = match self.project_path.clone() {
            Some(p) => Some(std::path::PathBuf::from(p)),
            None => rfd::FileDialog::new()
                .add_filter("DAP Sampler Project", &["dapsproj"])
                .set_file_name("dap-sampler.dapsproj")
                .save_file(),
        };
        let Some(path) = path else {
            return; // 用户取消对话框
        };

        match std::fs::write(&path, json) {
            Ok(()) => {
                self.project_path = Some(path.display().to_string());
                self.set_status(format!("Project saved: {}", path.display()));
            }
            Err(e) => self.set_error(format!("Failed to save project: {}", e)),
        }
    }

    /// 打开工程文件：恢复 ELF、通道、Watch 配置与全局参数
    fn open_project(&mut self, path: &std::path::Path) {
        self.status_text = None;
        self.status_is_error = false;
        self.elf_warning = None;
        self.show_elf_warning = false;
        self.elf_sync_prompt = None;
        self.elf_change_prompt = false;
        self.elf_sync_open = false;
        self.stop_acquisition();

        // 1. 读取并解析（失败不动当前状态）
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.set_error(format!("Failed to read project file: {}", e));
                return;
            }
        };
        let file = match ProjectFile::from_json(&text) {
            Ok(f) => f,
            Err(e) => {
                self.set_error(e.to_string());
                return;
            }
        };
        // 暂存工程变量（同步弹窗选 No 时用工程地址恢复）
        self.last_project_variables = file.variables.clone();

        // 2. 加载 ELF（失败仅警告，用保存的地址继续）
        let elf_loaded = if let Some(elf_path) = &file.elf_path {
            match ElfParser::load(elf_path) {
                Ok(ctx) => {
                    // 记录 mtime 快照（Idle 变化检测基准）
                    self.elf_mtime = std::fs::metadata(elf_path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    // 检测工程变量与 ELF 的差异（地址不同/缺失）→ 挂起同步询问
                    // 注意：必须在 ctx 移入 elf_ctx 之前借用 ctx.variables
                    self.elf_sync_prompt = compute_elf_diffs(&file.variables, &ctx.variables);
                    self.elf_sync_open = self.elf_sync_prompt.is_some();
                    self.elf_ctx = Some(ctx);
                    log::info!("ELF 已加载: {}", elf_path);
                    true
                }
                Err(e) => {
                    self.elf_ctx = None;
                    self.set_error(format!("ELF load failed, continuing with saved variable addresses: {}", e));
                    // 弹窗警告：状态栏红字容易被忽略，ELF 缺失是打开工程的关键异常
                    self.elf_warning = Some(format!(
                        "ELF file not found or failed to load:\n{}\n\nThe project will continue with saved variable addresses.",
                        elf_path
                    ));
                    self.show_elf_warning = true;
                    false
                }
            }
        } else {
            self.elf_ctx = None;
            false
        };

        // 3. 恢复通道与 Watch 列表
        if elf_loaded {
            // ELF 模式：勾选工程变量 → 复用 apply_variable_changes 重建通道
            self.manual_addresses.clear();
            self.manual_channel_names.clear();
            self.manual_value_types.clear();
            self.variable_browser.clear();
            // 只恢复 ELF 中真实存在的变量（固件符号可能已变化），缺失者警告跳过
            let mut missing: Vec<String> = Vec::new();
            for v in &file.variables {
                let exists = self.elf_ctx.as_ref().is_some_and(|ctx| {
                    ctx.variables.iter().any(|ev| elf_var_matches(ev, &v.name))
                });
                if exists {
                    self.variable_browser.checked_paths.insert(v.name.clone());
                } else {
                    missing.push(v.name.clone());
                }
            }
            if !missing.is_empty() {
                self.set_error(format!(
                    "{} variables in project not found in ELF, skipped: {}",
                    missing.len(),
                    missing.join(", ")
                ));
            }
            // 先移除不在工程内的旧通道，再添加工程变量通道
            let project_names: std::collections::HashSet<&str> =
                file.variables.iter().map(|v| v.name.as_str()).collect();
            let mut changes: Vec<(String, bool)> = self
                .channel_panel
                .channels
                .iter()
                .filter(|c| !project_names.contains(c.name.as_str()))
                .map(|c| (c.name.clone(), false))
                .collect();
            changes.extend(
                file
                    .variables
                    .iter()
                    .filter(|v| !missing.contains(&v.name))
                    .map(|v| (v.name.clone(), true)),
            );
            self.apply_variable_changes(changes);
        } else {
            // 手工模式（或 ELF 加载失败降级）：用工程里的地址/类型直接恢复
            self.manual_addresses = file.variables.iter().map(|v| v.address).collect();
            self.manual_value_types =
                file.variables.iter().map(|v| v.value_type()).collect();
            self.manual_channel_names = file.variables.iter().map(|v| v.name.clone()).collect();
            self.variable_browser.clear();
            self.channel_panel.channels.clear();
            for (i, v) in file.variables.iter().enumerate() {
                let color = CHANNEL_COLORS[i % CHANNEL_COLORS.len()];
                self.channel_panel
                    .add_channel(v.name.clone(), color, v.value_type());
            }
            let names: Vec<String> = self
                .channel_panel
                .channels
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let types: Vec<ValueType> = self
                .channel_panel
                .channels
                .iter()
                .map(|c| c.value_type)
                .collect();
            self.watch_panel
                .sync_from_channels(&names, &types, &self.manual_addresses);
        }

        // 4. 覆盖 Watch 配置字段（y_offset/y_scale/refresh/remark 以工程为准）
        if self.watch_panel.entries.len() != file.variables.len() {
            log::warn!(
                "工程变量数与通道数不一致（{} vs {}），多余变量被忽略",
                file.variables.len(),
                self.watch_panel.entries.len()
            );
        }
        for (entry, pv) in self.watch_panel.entries.iter_mut().zip(&file.variables) {
            entry.name = pv.name.clone();
            entry.address = pv.address;
            entry.value_type = pv.value_type();
            entry.refresh_period_s = pv.refresh_period_s;
            entry.y_offset = pv.y_offset;
            entry.y_scale = pv.y_scale;
            entry.remark = pv.remark.clone();
        }
        // 4.5 恢复 Watch 列宽（旧版工程无此字段时为默认宽度；非法值在面板内夹紧）
        self.watch_panel.set_col_widths(&file.watch_col_widths);

        // 5. 恢复全局参数（controls 与 app 字段、waveform 三方同步）
        self.controls.sample_rate = file.sample_rate_hz
            .clamp(crate::ui::controls::RATE_MIN, crate::ui::controls::RATE_MAX);
        self.rate_hz = self.controls.sample_rate;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.waveform.set_interval(self.interval_us);
        self.controls.swd_clock_mhz = file.swd_clock_mhz.clamp(1, 50);
        self.swd_clock_mhz = self.controls.swd_clock_mhz;
        self.controls.window_size = file.window_size
            .clamp(crate::ui::controls::WINDOW_MIN, crate::ui::controls::WINDOW_MAX);
        self.window_size = self.controls.window_size;
        self.buffer_capacity = file.buffer_capacity
            .clamp(crate::ui::controls::BUFFER_CAPACITY_MIN, crate::ui::controls::BUFFER_CAPACITY_MAX);
        let mode = file.display_mode();
        self.waveform.set_display_mode(mode);
        self.display_mode = mode;

        // 6. 清空运行时状态，回到 Idle
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
        self.cursor.clear();
        self.controls.update_count(0);
        self.selected_channel = None;
        self.controls.target_count = None;

        // 7. 记录工程路径
        self.project_path = Some(path.display().to_string());
        if !self.status_is_error {
            self.set_status(format!("Project opened: {}", path.display()));
        }
    }

    /// 同步弹窗选 Yes：地址统一用 ELF 文件的值，缺失变量删除
    fn apply_elf_sync(&mut self, prompt: &ElfSyncPrompt) {
        // 1. 删除 ELF 中不存在的变量（通道 + 勾选 + Watch）
        for name in &prompt.missing {
            self.channel_panel.remove_by_name(name);
            self.variable_browser.checked_paths.remove(name);
        }
        if !prompt.missing.is_empty() {
            self.selected_channel = None;
        }
        // 2. 确保地址收集走 ELF 分支（手工快照清空必须在 collect 之前，防误用旧手工地址）
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();
        // 3. Watch 重建：地址用 ELF 查询结果（collect_channel_addresses 在 ELF 上下文返回 ELF 地址）
        let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
        let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
        let addresses = self.collect_channel_addresses();
        self.watch_panel.sync_from_channels(&names, &types, &addresses);
        // 4. 反馈
        self.set_status(format!(
            "ELF synchronized: {} addresses updated, {} variables removed",
            prompt.address_diffs.len(),
            prompt.missing.len()
        ));
        self.refresh_elf_mtime();
    }

    /// 同步弹窗选 No（或 X 关闭）：统一用工程记录的地址（切手工快照，采样与 Watch 一致）
    fn use_project_addresses(&mut self, variables: &[ProjectVariable]) {
        self.manual_addresses = variables.iter().map(|v| v.address).collect();
        self.manual_channel_names = variables.iter().map(|v| v.name.clone()).collect();
        self.manual_value_types = variables.iter().map(|v| v.value_type()).collect();
        self.variable_browser.clear();
        self.channel_panel.channels.clear();
        for (i, v) in variables.iter().enumerate() {
            let color = CHANNEL_COLORS[i % CHANNEL_COLORS.len()];
            self.channel_panel.add_channel(v.name.clone(), color, v.value_type());
        }
        let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
        let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
        self.watch_panel.sync_from_channels(&names, &types, &self.manual_addresses);
        self.set_status("Using saved variable addresses (ELF not synchronized)");
        self.refresh_elf_mtime();
    }

    /// 从当前 ELF 文件路径刷新 mtime 快照（Idle 变化检测基准）
    fn refresh_elf_mtime(&mut self) {
        self.elf_mtime = self.elf_ctx
            .as_ref()
            .and_then(|c| std::fs::metadata(&c.file_path).ok())
            .and_then(|m| m.modified().ok());
    }

    /// 检测 ELF 文件 mtime 变化（Idle 时调用）；变化 → 置 elf_change_prompt
    fn check_elf_changed(&mut self) {
        let current = self.elf_ctx
            .as_ref()
            .and_then(|c| std::fs::metadata(&c.file_path).ok())
            .and_then(|m| m.modified().ok());
        if current != self.elf_mtime {
            if current.is_none() && self.elf_mtime.is_some() {
                // 文件被删除：Reload 问答无意义（点 Yes 必然失败），直接错误提示；
                // 同时更新快照（None），避免每秒重复报错
                let path = self
                    .elf_ctx
                    .as_ref()
                    .map(|c| c.file_path.clone())
                    .unwrap_or_default();
                self.set_error(format!("ELF file is missing: {}", path));
                self.refresh_elf_mtime();
            } else {
                // 文件被修改（mtime 变化）→ 弹 Reload 问答
                self.elf_change_prompt = true;
            }
        }
    }

    /// 移除勾选与通道中在新 ELF 里已不存在的变量（reload/ELF 更新后调用）
    ///
    /// 变量从 ELF 删除后，Variables 勾选统计、Channels 面板应与新内容一致，
    /// 否则残留变量会在 Start 时因找不到地址被静默跳过。
    fn remove_stale_variables(&mut self, elf_vars: &[ElfVariable]) {
        let stale: Vec<String> = self
            .variable_browser
            .checked_paths
            .iter()
            .filter(|p| !elf_vars.iter().any(|ev| elf_var_matches(ev, p)))
            .cloned()
            .collect();
        if stale.is_empty() {
            return;
        }
        for name in &stale {
            self.variable_browser.checked_paths.remove(name);
            self.channel_panel.remove_by_name(name);
        }
        self.selected_channel = None;
    }

    /// ELF Changed 弹窗选 Yes：重新解析 ELF 文件
    fn reload_elf(&mut self) {
        let path = self.elf_ctx.as_ref().map(|c| c.file_path.clone());
        let Some(path) = path else { return };
        match ElfParser::load(&path) {
            Ok(ctx) => {
                let variable_count = ctx.variables.len();
                // 清理新 ELF 中已不存在的勾选/通道（Variables/Channels 跟随新内容）
                self.remove_stale_variables(&ctx.variables);
                self.elf_ctx = Some(ctx);
                // Watch 立即反映新 ELF 的地址（与同步弹窗 Yes 行为一致）
                let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
                let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
                let addresses = self.collect_channel_addresses();
                self.watch_panel.sync_from_channels(&names, &types, &addresses);
                self.set_status(format!("ELF reloaded: {} variables", variable_count));
            }
            Err(e) => {
                self.set_error(format!("ELF reload failed: {}", e));
            }
        }
        self.refresh_elf_mtime();
        self.elf_change_prompt = false;
    }

    fn start_acquisition(&mut self) {
        self.status_text = None;
        self.status_is_error = false;
        // 1. 确定要采集的地址和通道信息
        let (addresses, channel_names, channel_colors, types) =
            if !self.manual_addresses.is_empty() {
                // 手工模式：使用预解析的地址
                let names: Vec<String> = self.manual_channel_names.clone();
                let colors: Vec<egui::Color32> = (0..names.len())
                    .map(|i| CHANNEL_COLORS[i % CHANNEL_COLORS.len()])
                    .collect();
                let types = self.manual_value_types.clone();
                (self.manual_addresses.clone(), names, colors, types)
            } else {
                // ELF 模式：从 channel_panel 动态获取。
                // 以"匹配成功"为单位收集 (通道, 地址)，保证 names/colors/
                // types/addresses 四个列表始终一一对齐（匹配失败的通道整组跳过）
                let channels = &self.channel_panel.channels;
                if channels.is_empty() {
                    self.set_error("No sampling channel selected: load an ELF and check variables, or specify addresses with --addresses");
                    return;
                }
                let matched: Vec<(&_, u32)> = channels
                    .iter()
                    .filter_map(|ch| {
                        self.elf_ctx.as_ref().and_then(|ctx| {
                            ctx.variables
                                .iter()
                                .find(|v| elf_var_matches(v, &ch.name))
                                .map(|v| (ch, v.address))
                        })
                    })
                    .collect();

                if matched.is_empty() {
                    self.set_error("Channels selected but no matching variable addresses found: check ELF symbols/debug info");
                    return;
                }

                let addresses: Vec<u32> = matched.iter().map(|(_, a)| *a).collect();
                let names: Vec<String> = matched.iter().map(|(c, _)| c.name.clone()).collect();
                let colors: Vec<egui::Color32> = matched.iter().map(|(c, _)| c.color).collect();
                let types: Vec<ValueType> = matched.iter().map(|(c, _)| c.value_type).collect();
                (addresses, names, colors, types)
            };

        if addresses.is_empty() {
            self.set_error("No sampling channel selected: load an ELF and check variables, or specify addresses with --addresses");
            return;
        }

        // 保存当前通道地址/名字/类型列表（用于 Watch 写入的子字对齐处理与名字解析）
        let prev_addresses = std::mem::replace(&mut self.current_addresses, addresses.clone());
        self.current_names = channel_names.clone();
        self.current_types = types.clone();

        // 2. 连接 USB + 初始化 SWD（仅在尚未连接时）
        if self.active_usb.is_none() {
            let mut swd = match SwdLink::new() {
                Ok(s) => s,
                Err(e) => {
                    self.set_error(format!("Failed to connect to DAP-Link: {}", e));
                    return;
                }
            };
            let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
            if let Err(e) = swd.init_with_clock(clock_hz) {
                self.set_error(format!("SWD initialization failed: {}", e));
                return;
            }
            let (usb, _) = swd.into_parts();
            self.active_usb = Some(Arc::new(usb));
        }

        // 3. 更新波形面板（通道名和类型）
        // Paused→Start 且通道配置未变时保留历史波形数据；
        // 其余情况（Idle→Start 或通道增删/改名/改类型）清空缓冲重新开始
        let old_names = self.waveform.channel_names();
        let config_unchanged = prev_addresses == addresses
            && self.waveform.value_types() == types.as_slice()
            && old_names.len() == channel_names.len()
            && old_names
                .iter()
                .zip(&channel_names)
                .all(|(a, b)| *a == b.as_str());
        // Pause 期间改了缓冲区容量 → 视为配置变化，恢复时重建缓冲
        let keep_history = self.controls.state == AcquisitionState::Paused
            && config_unchanged
            && self.display_buf.max_samples() == self.buffer_capacity;

        self.waveform =
            WaveformPanel::new(channel_names.clone(), channel_colors, self.interval_us, types.clone(), self.display_mode);

        // 4. 数据缓冲区：Pause 恢复且配置未变时保留历史（含光标与计数），否则清空重建
        if keep_history {
            self.display_buf.set_max_samples(self.buffer_capacity);
        } else {
            self.display_buf = DisplayBuffer::new(self.buffer_capacity);
            self.cursor.clear();
            self.controls.update_count(0);
        }

        // 5. 创建 PipelineEngine
        let engine = PipelineEngine::new(
            Arc::clone(self.active_usb.as_ref().unwrap()),
            DapProtocol::new(),
            addresses,
            &types,
            self.rate_hz,
        );

        // 同步 Watch 面板
        self.watch_panel.sync_from_channels(&channel_names, &types, &self.current_addresses);

        // 7. 启动采集
        match engine.start(self.buffer_capacity) {
            Ok(handle) => {
                self.pipeline = Some(handle);
                self.controls.set_running();
                self.acquisition_start = Some(Instant::now());
                self.set_status("Acquisition started");
            }
            Err(e) => self.set_error(format!("Failed to start acquisition: {}", e)),
        }
    }

    fn stop_pipeline(&mut self) {
        if let Some(handle) = self.pipeline.take() {
            log::info!("开始停止流水线，等待子线程退出...");
            handle.stop();
            log::info!("流水线停止完成");
        }
        self.acquisition_start = None;
        self.controls.actual_rate_hz = 0.0;
    }

    fn stop_acquisition(&mut self) {
        self.stop_pipeline();
        self.controls.set_stopped();
        // 释放 USB 设备：发送 DAP_Disconnect + release_interface
        // 不发送 DAP_Disconnect 会导致 DAP-Link 固件认为调试器仍连接着
        if let Some(usb) = self.active_usb.take() {
            match Arc::try_unwrap(usb) {
                Ok(mut bulk) => {
                    bulk.release();
                }
                Err(arc) => {
                    if let Some(bulk) = Arc::get_mut(&mut arc.clone()) {
                        bulk.release();
                    }
                    drop(arc);
                }
            }
        }
        self.set_status("Acquisition stopped");
    }

    fn pause_acquisition(&mut self) {
        self.stop_pipeline();
        self.controls.set_paused();
        // Pause 时保持 USB 连接，恢复采集更快
        self.set_status("Acquisition paused");
    }

    fn drain_pipeline(&mut self) {
        let n = if let Some(ref handle) = self.pipeline {
            handle.drain_samples(&mut self.temp_buf)
        } else {
            0
        };
        if n > 0 {
            // 更新 Watch 面板的变量值
            for sample in &self.temp_buf[..n] {
                self.watch_panel.update_values(sample.values(), self.rate_hz);
            }
            self.display_buf.push_batch(&self.temp_buf[..n]);
            self.controls.update_count(self.display_buf.next_seq());
            self.has_new_data = true;
        }

        // 处理 Watch 面板的写入请求
        self.process_pending_writes();
    }

    /// 处理 Watch 表格中的值写入请求
    ///
    /// 通过 pipeline 的写入队列注入，不停止采集。
    /// 写入操作会在下一个采样周期与读取操作合并为一条 DAP_Transfer 命令。
    fn process_pending_writes(&mut self) {
        let writes = self.watch_panel.drain_writes();
        if writes.is_empty() {
            return;
        }

        if self.current_names.is_empty() {
            log::warn!("写入请求被忽略：无采集通道");
            return;
        }

        if let Some(ref handle) = self.pipeline {
            // 采集中：按通道名解析为引擎快照中的变量索引。
            // 名字是稳定标识——通道增删后行号会整体移位，按行号写入会命中
            // 错误的地址；名字解析失败说明该通道不在本次采集集合中，丢弃。
            for w in &writes {
                match resolve_channel_index(&self.current_names, &w.channel_name) {
                    Some(idx) => {
                        let addr = self.current_addresses[idx];
                        handle.queue_write(idx, w.raw_value);
                        log::info!(
                            "已排队写入通道 {} ({}): 0x{:08X} → 0x{:08X}",
                            idx, w.channel_name, w.raw_value, addr
                        );
                    }
                    None => {
                        log::warn!(
                            "写入请求被丢弃：通道 '{}' 不在当前采集集合中",
                            w.channel_name
                        );
                    }
                }
            }
        } else {
            // 采集未运行时，直接通过 SwdLink 写入。
            // 按 Watch 行的实时地址写入（current_* 是上次运行的快照，已过期）
            match SwdLink::new() {
                Ok(mut swd) => {
                    let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
                    if let Err(e) = swd.init_with_clock(clock_hz) {
                        log::error!("写入前 SWD initialization failed: {}", e);
                        return;
                    }
                    for w in &writes {
                        let Some(entry) = self
                            .watch_panel
                            .entries
                            .iter()
                            .find(|e| e.name == w.channel_name)
                        else {
                            log::warn!(
                                "写入请求被丢弃：通道 '{}' 不在 Watch 列表中",
                                w.channel_name
                            );
                            continue;
                        };
                        match swd.write_variable(entry.address, entry.value_type.byte_size(), w.raw_value) {
                            Ok(()) => log::info!(
                                "已写入通道 '{}': 0x{:08X} → 0x{:08X} ({} bytes)",
                                w.channel_name, w.raw_value, entry.address, entry.value_type.byte_size()
                            ),
                            Err(e) => log::error!(
                                "写入通道 '{}' 失败: {}", w.channel_name, e
                            ),
                        }
                    }
                }
                Err(e) => log::error!("写入时Failed to connect to DAP-Link: {}", e),
            }
        }
    }

    fn apply_variable_changes(&mut self, changes: Vec<(String, bool)>) {
        // 超限勾选必须回滚：复选框在渲染层已先行置位，若这里静默跳过，
        // 「勾上了但没有通道」的状态会永久脱节（用户以为在监视，实际没采）。
        let mut rejected: Vec<String> = Vec::new();

        for (path, is_checked) in changes {
            if is_checked {
                if !self.channel_panel.has_channel(&path) {
                    let ch_count = self.channel_panel.channel_count();
                    if ch_count >= 8 {
                        // 回滚勾选状态，保持 UI 与真实通道数一致
                        self.variable_browser.checked_paths.remove(&path);
                        rejected.push(path);
                        continue;
                    }
                    let color = CHANNEL_COLORS[ch_count % CHANNEL_COLORS.len()];
                    let value_type = self.elf_ctx.as_ref()
                        .and_then(|ctx| ctx.variables.iter().find(|v| v.path == path))
                        .map(|v| v.value_type)
                        .unwrap_or(ValueType::Float);
                    self.channel_panel.add_channel(path.clone(), color, value_type);
                    // 维持不变量「有通道 ⟺ 已勾选」：渲染层在点击时已置位，
                    // 这里补齐以覆盖非 UI 调用路径（两者行为保持一致）
                    self.variable_browser.checked_paths.insert(path);
                }
            } else {
                self.channel_panel.remove_by_name(&path);
            }
        }

        if !rejected.is_empty() {
            let msg = format!(
                "At most 8 channels can be watched.\n\nUnchecked: {}",
                rejected.join(", ")
            );
            // 状态栏红字 + 居中弹窗：弹窗更醒目，避免用户漏看而
            // 误以为变量在监视中
            self.set_error(msg.clone());
            self.channel_limit_warning = Some(msg);
        }

        // 同步 Watch 面板
        let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
        let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
        let addresses = self.collect_channel_addresses();
        self.watch_panel.sync_from_channels(&names, &types, &addresses);
    }

    /// 从 elf_ctx 中收集当前 channel_panel 各通道对应的地址。
    /// 手工模式下使用 manual_addresses。
    fn collect_channel_addresses(&self) -> Vec<u32> {
        if !self.manual_addresses.is_empty() {
            return self.manual_addresses.clone();
        }
        self.channel_panel
            .channels
            .iter()
            .filter_map(|ch| {
                self.elf_ctx.as_ref().and_then(|ctx| {
                    ctx.variables
                        .iter()
                        .find(|v| elf_var_matches(v, &ch.name))
                        .map(|v| v.address)
                })
            })
            .collect()
    }

    fn sync_channel_visibility(&mut self) {
        let vis = self.channel_panel.visibility();
        for (i, visible) in vis.iter().enumerate() {
            self.waveform.set_channel_visible(i, *visible);
        }
    }

    fn show_cursor_info(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("Click waveform to place cursor")
                .size(10.0)
                .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
        );
        let types = self.waveform.value_types().to_vec();
        let colors: Vec<egui::Color32> = self.channel_panel.channels.iter().map(|c| c.color).collect();
        let buffer = self.display_buf.all();
        let offset = self.display_buf.oldest_seq();

        let r1 = self.cursor.get_result(buffer, offset, &types);
        let r2 = self.cursor.cursor2.and_then(|seq2| {
            let local_idx2 = seq2.checked_sub(offset)? as usize;
            let sample2 = buffer.get(local_idx2)?;
            Some(CursorResult {
                seq: seq2,
                time_sec: sample2.timestamp_sec,
                values: sample2.as_f64s_typed(&types),
            })
        });
        let delta = self.cursor.delta(buffer, offset, &types);

        // ---- Delta ----
        if let Some((dt, dv)) = delta {
            ui.separator();
            ui.label(egui::RichText::new("Delta").strong());
            ui.label(egui::RichText::new(format!("\u{0394}T: {:.6} s", dt)).monospace());
            for (i, v) in dv.iter().enumerate() {
                let color = colors.get(i).copied().unwrap_or(egui::Color32::WHITE);
                ui.horizontal(|ui| {
                    ui.colored_label(color, "●");
                    ui.label(egui::RichText::new(format!("dCH{}: {:.4}", i + 1, v)).monospace());
                });
            }
        }

        // ---- Cursor 1 ----
        if let Some(r) = r1 {
            ui.separator();
            ui.label(egui::RichText::new("Cursor 1").strong());
            for (i, v) in r.values.iter().enumerate() {
                let color = colors.get(i).copied().unwrap_or(egui::Color32::WHITE);
                ui.horizontal(|ui| {
                    ui.colored_label(color, "●");
                    ui.label(egui::RichText::new(format!("CH{}: {:.4}", i + 1, v)).monospace());
                });
            }
            ui.label(egui::RichText::new(format!("T: {:.6} s", r.time_sec)).monospace());
        } else if let Some(seq) = self.cursor.cursor1 {
            // 光标存在但已滑出缓冲范围（窗口滚动/清空后）：给出可见提示，
            // 而不是静默不显示读数
            ui.separator();
            let range_text = if self.display_buf.is_empty() {
                "(缓冲已清空)".to_string()
            } else {
                format!(
                    "[{} ~ {}]",
                    self.display_buf.oldest_seq(),
                    self.display_buf.next_seq() - 1
                )
            };
            ui.colored_label(
                egui::Color32::from_rgb(0xc0, 0x82, 0x00),
                format!("⚠ Cursor 1 @ seq {} 已滑出缓冲范围 {}", seq, range_text),
            );
        }

        // ---- Cursor 2 ----
        if let Some(r) = r2 {
            ui.separator();
            ui.label(egui::RichText::new("Cursor 2").strong());
            for (i, v) in r.values.iter().enumerate() {
                let color = colors.get(i).copied().unwrap_or(egui::Color32::WHITE);
                ui.horizontal(|ui| {
                    ui.colored_label(color, "●");
                    ui.label(egui::RichText::new(format!("CH{}: {:.4}", i + 1, v)).monospace());
                });
            }
            ui.label(egui::RichText::new(format!("T: {:.6} s", r.time_sec)).monospace());
        } else if self.cursor.cursor2.is_some() {
            // Cursor2 已滑出缓冲（窗口滚动后）
            ui.separator();
            ui.colored_label(
                egui::Color32::from_rgb(0xc0, 0x82, 0x00),
                "⚠ Cursor 2 已滑出缓冲范围",
            );
        }
    }

    /// 悬停自动展开/收起的菜单按钮
    ///
    /// 鼠标悬停在按钮或其弹出菜单上时保持展开，移出后自动收起；
    /// `content` 返回 true 表示有菜单项被点击执行（立即收起菜单）。
    fn hover_menu(
        &mut self,
        ui: &mut egui::Ui,
        id: &'static str,
        title: &str,
        content: impl FnOnce(&mut egui::Ui, &mut DapSamplerApp) -> bool,
    ) {
        let open = self.hover_menu == Some(id);
        // 菜单按钮：默认无底色（与菜单栏背景一致），悬停或展开时高亮
        let response = menu_button(ui, title, open);

        if open {
            let area = egui::Area::new(ui.id().with(("menu_popup", id)))
                .order(egui::Order::Foreground)
                .fixed_pos(response.rect.left_bottom())
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(180.0);
                        // 菜单项文字左对齐（egui 默认布局是水平居中）
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            content(ui, self)
                        })
                    })
                });
            let action_clicked = area.inner.inner.inner;
            let menu_rect = area.response.rect;
            // 点击菜单项执行动作 → 立即收起；否则指针移出按钮与菜单区域 → 收起
            let pointer = ui.ctx().pointer_hover_pos();
            let hover_inside = pointer.is_some_and(|pos| {
                response.rect.union(menu_rect).expand(2.0).contains(pos)
            });
            if action_clicked || !hover_inside {
                self.hover_menu = None;
            }
        } else if response.hovered() {
            self.hover_menu = Some(id);
        }
    }

    /// 波形图左上角叠加采样信息（Actual Rate / Samples / Progress / Overwritten）
    ///
    /// 透明背景、图例风格小字，与通道图例（右上角）对称；仅在有数据时显示。
    fn show_plot_overlay(&mut self, ui: &mut egui::Ui, plot_rect: egui::Rect) {
        // (文字, 颜色, 悬停提示)
        let mut items: Vec<(String, egui::Color32, Option<&'static str>)> = Vec::new();
        if self.controls.state == AcquisitionState::Running && self.controls.actual_rate_hz > 0.0 {
            let pct = self.controls.actual_rate_hz / self.controls.sample_rate as f64 * 100.0;
            let (color, tip) = if pct < 90.0 {
                (
                    egui::Color32::from_rgb(0xd0, 0x7a, 0x00),
                    Some(
                        "DAP-Link 吞吐量不足，实际采样率低于目标。\n波形时间轴已使用真实时间戳，波形形状应正确。\n降低采样率可获得更均匀的采样间隔。",
                    ),
                )
            } else {
                (ui.visuals().text_color(), None)
            };
            items.push((
                format!("Actual Rate: {:.0} Hz ({:.0}%)", self.controls.actual_rate_hz, pct),
                color,
                tip,
            ));
        }
        if self.controls.total_samples > 0 {
            let duration = self.controls.total_samples as f64 / self.controls.sample_rate.max(1) as f64;
            items.push((
                format!("Samples: {} ({:.1}s)", self.controls.total_samples, duration),
                ui.visuals().text_color(),
                None,
            ));
        }
        // 采样进度（限次采集时显示）
        if let Some(target) = self.controls.target_count {
            let pct = (self.controls.total_samples as f64 / target as f64 * 100.0).min(100.0);
            items.push((format!("Progress: {:.0}%", pct), ui.visuals().text_color(), None));
        }
        // 消费端落后提示：环形缓冲覆盖了旧采样点（数据丢失）
        if let Some(ref handle) = self.pipeline {
            let overwritten = handle.overwritten_samples();
            if overwritten > 0 {
                items.push((
                    format!("Overwritten: {}", overwritten),
                    egui::Color32::from_rgb(0xd0, 0x7a, 0x00),
                    Some(
                        "UI 消费速度低于采样速度，环形缓冲区已覆盖旧采样点（数据丢失）。\n通常由窗口拖拽、对话框阻塞或采样率过高引起。",
                    ),
                ));
            }
        }
        if items.is_empty() {
            return;
        }

        egui::Area::new(ui.id().with("plot_rate_overlay"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 6.0))
            .constrain_to(plot_rect)
            .show(ui.ctx(), |ui| {
                // 固定最小宽度，防止 label 按窄宽度换行（每项一行完整显示）
                ui.set_min_width(240.0);
                for (text, color, tip) in items {
                    let label = ui.label(
                        egui::RichText::new(text)
                            .size(11.0)
                            .color(color),
                    );
                    if let Some(tip) = tip {
                        label.on_hover_text(tip);
                    }
                }
            });
    }
}

impl eframe::App for DapSamplerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 统一浅色主题（与 Watch/通道面板的浅色风格一致）；面板/窗口底色改为纯白
        let mut visuals = egui::Visuals::light();
        visuals.panel_fill = egui::Color32::WHITE;
        visuals.window_fill = egui::Color32::WHITE;
        // 整体背景变白后，按钮/输入框默认底色（230 灰）显得突兀，调浅至接近白
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(246);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(246);
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(236);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(236);
        ctx.set_visuals(visuals);
        // Idle 状态检测 ELF 文件变化（无其他弹窗占用时）
        if self.controls.state == AcquisitionState::Idle
            && self.elf_ctx.is_some()
            && self.elf_sync_prompt.is_none()
            && !self.elf_change_prompt
            && self.elf_warning.is_none()
        {
            self.check_elf_changed();
        }
        self.drain_pipeline();

        // 计算实际采样率
        if let Some(start) = self.acquisition_start {
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.1 {
                self.controls.actual_rate_hz = self.controls.total_samples as f64 / elapsed;
            }
        }

        if let Some(target) = self.controls.target_count {
            if self.controls.total_samples >= target && self.pipeline.is_some() {
                self.stop_acquisition();
            }
        }

        // ---- 顶部菜单栏：文件/工具 悬停菜单 + 右侧状态信息 ----
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;

                // ---- 文件菜单：工程操作 ----
                self.hover_menu(ui, "file", "Project(P)", |ui, app| {
                    if menu_item(ui, "New Project").clicked() {
                        app.new_project();
                        return true;
                    }
                    if menu_item(ui, "Open Project…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("DAP Sampler Project", &["dapsproj"])
                            .pick_file()
                        {
                            app.open_project(&path);
                        }
                        return true;
                    }
                    if menu_item(ui, "Save Project").clicked() {
                        app.save_project();
                        return true;
                    }
                    false
                });

                // ---- 导入/导出菜单 ----
                self.hover_menu(ui, "tools", "Files(F)", |ui, app| {
                    if menu_item(ui, "Import ELF File…").clicked() {
                        app.import_elf();
                        return true;
                    }
                    if menu_item(ui, "Export data to…").clicked() {
                        app.export_data();
                        return true;
                    }
                    false
                });

                // ---- 设置菜单 ----
                self.hover_menu(ui, "settings", "Settings(S)", |ui, app| {
                    if menu_item(ui, "General Settings…").clicked() {
                        app.show_settings = true;
                        return true;
                    }
                    false
                });

                // ---- 帮助菜单 ----
                self.hover_menu(ui, "help", "Help(H)", |ui, app| {
                    if menu_item(ui, "About").clicked() {
                        app.show_about = true;
                        return true;
                    }
                    false
                });
            });
        });

        // ---- 底部状态栏（横跨整个窗口宽度，界面最底部）----
        // egui 0.31 面板规则：bottom panel 宽度避让已声明的侧边栏；
        // 先于 SidePanel::left 声明才能横跨全宽（含侧边栏下方）。
        // 堆叠语义：先声明的 bottom panel 在最底部，后声明的叠在它上方，
        // 因此状态栏先于 watch_panel 声明，紧贴窗口底边。
        egui::TopBottomPanel::bottom("status_bar")
            .resizable(false)
            .exact_height(24.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(msg) = &self.status_text {
                        let color = if self.status_is_error {
                            egui::Color32::from_rgb(0xff, 0x55, 0x55)
                        } else {
                            ui.visuals().text_color()
                        };
                        let label = ui.label(egui::RichText::new(msg).color(color));
                        // 错误消息悬停时显示排障提示
                        if self.status_is_error {
                            label.on_hover_text(
                                "请检查：DAP-Link 是否连接、是否安装 WinUSB 驱动、是否被 Keil/OpenOCD/pyOCD/其他程序占用、目标板是否供电、SWD 接线是否正确，或尝试降低 SWD Clock。"
                            );
                        }
                    }
                });
            });

        // ---- 左侧面板：变量浏览器 + 通道 + 光标（可展开/收起）----
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(220.0)
            .width_range(220.0..=400.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // --- 变量浏览器模块 ---
                        egui::CollapsingHeader::new("Variables")
                            .default_open(self.var_browser_open)
                            .id_salt("var_browser_header")
                            .show(ui, |ui| {
                                let changes = self.variable_browser.show(
                                    ui,
                                    self.elf_ctx.as_ref(),
                                );
                                if !changes.is_empty() {
                                    // 采集中通道集合被引擎快照固定：增删会破坏
                                    // Watch 行与采样数据的对应关系，必须先停止
                                    if self.controls.state != AcquisitionState::Idle {
                                        self.set_error(
                                            "Stop acquisition before changing watched variables",
                                        );
                                    } else {
                                        self.apply_variable_changes(changes);
                                    }
                                }
                            });

                        ui.separator();

                        // --- 通道模块 ---
                        egui::CollapsingHeader::new(
                            format!("Channels ({}/8)", self.channel_panel.channel_count())
                        )
                            .default_open(self.channels_open)
                            .id_salt("channels_header")
                            .show(ui, |ui| {
                                let action = self.channel_panel.show(ui, self.selected_channel);
                                if let Some(idx) = action.select {
                                    self.selected_channel = if self.selected_channel == Some(idx) {
                                        None
                                    } else {
                                        Some(idx)
                                    };
                                }
                                if let Some(remove_idx) = action.remove {
                                    // 采集中通道集合被引擎快照固定：删除会使
                                    // Watch 行号与采样数据错位，必须先停止
                                    if self.controls.state != AcquisitionState::Idle {
                                        self.set_error(
                                            "Stop acquisition before removing channels",
                                        );
                                    } else {
                                        let name = self.channel_panel.channels[remove_idx].name.clone();
                                        self.variable_browser.checked_paths.remove(&name);
                                        self.channel_panel.remove_channel(remove_idx);
                                        if let Some(sel) = self.selected_channel {
                                            if sel == remove_idx {
                                                self.selected_channel = None;
                                            } else if sel > remove_idx {
                                                self.selected_channel = Some(sel - 1);
                                            }
                                        }
                                        let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
                                        let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
                                        let addresses = self.collect_channel_addresses();
                                        self.watch_panel.sync_from_channels(&names, &types, &addresses);
                                    }
                                }
                            });

                        ui.separator();

                        // --- 光标模块 ---
                        egui::CollapsingHeader::new("Cursor")
                            .default_open(self.cursor_open)
                            .id_salt("cursor_header")
                            .show(ui, |ui| {
                                self.show_cursor_info(ui);
                            });
                    });
            });

        // ---- 底部 Watch 面板 ----
        egui::TopBottomPanel::bottom("watch_panel")
            .resizable(true)
            .default_height(180.0)
            .min_height(180.0)
            .show(ctx, |ui| {
                let colors: Vec<egui::Color32> =
                    self.channel_panel.channels.iter().map(|c| c.color).collect();
                self.watch_panel.show(ui, &colors);
            });

        // 把本帧 watch 表中提交的 y_offset/y_scale 变更应用到波形图
        for (name, offset, scale) in self.watch_panel.drain_changed_transforms() {
            self.waveform.set_channel_transform(&name, offset, scale);
        }

        // ---- 中央：工具栏 + 波形 ----
        self.sync_channel_visibility();
        let has_new_data = self.has_new_data;
        self.has_new_data = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            // 工具栏：采集控制 + 参数 + 显示模式
            ui.horizontal_wrapped(|ui| {
                if let Some(cmd) = self.controls.show_buttons(ui) {
                    match cmd {
                        AcquisitionCommand::Start => {
                            if self.controls.state == AcquisitionState::Idle
                                || self.controls.state == AcquisitionState::Paused
                            {
                                self.start_acquisition();
                            }
                        }
                        AcquisitionCommand::Pause => self.pause_acquisition(),
                        AcquisitionCommand::Stop => self.stop_acquisition(),
                    }
                }

                // Follow 按钮（Start/Stop 之后、Rate 之前）
                let follow_text = if self.waveform.is_follow() {
                    "\u{23f8} Follow: ON"
                } else {
                    "\u{25b6} Follow: OFF"
                };
                if ui.button(follow_text).clicked() {
                    self.waveform.set_follow(!self.waveform.is_follow());
                }

                self.controls.show_params(ui);

                // 检测采样率变化
                if self.controls.sample_rate != self.rate_hz {
                    self.rate_hz = self.controls.sample_rate;
                    self.interval_us = 1_000_000.0 / self.rate_hz as f64;
                    self.waveform.set_interval(self.interval_us);
                    self.set_status(format!("Sample rate changed to {} Hz", self.rate_hz));
                }

                // 检测 SWD 时钟变化（仅记录日志，下次连接时生效）
                if self.controls.swd_clock_mhz != self.swd_clock_mhz {
                    self.swd_clock_mhz = self.controls.swd_clock_mhz;
                    self.set_status(format!("SWD clock changed to {} MHz (applies on next DAP-Link connection)", self.swd_clock_mhz));
                }

                // 检测窗口大小变化
                if self.controls.window_size != self.window_size {
                    self.window_size = self.controls.window_size;
                    self.set_status(format!("Display window size changed to {} points", self.window_size));
                }

                // 波形显示模式切换
                ui.label("Display:");
                let mut mode = self.display_mode;
                egui::ComboBox::from_id_salt("display_mode")
                    .selected_text(match mode {
                        WaveformDisplayMode::Line => "Line",
                        WaveformDisplayMode::Point => "Point",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut mode, WaveformDisplayMode::Line, "Line");
                        ui.selectable_value(&mut mode, WaveformDisplayMode::Point, "Point");
                    });
                if mode != self.display_mode {
                    self.display_mode = mode;
                    self.waveform.set_display_mode(mode);
                    let mode_label = match mode {
                        WaveformDisplayMode::Line => "Line",
                        WaveformDisplayMode::Point => "Point",
                    };
                    self.set_status(format!("Display mode switched to {}", mode_label));
                }
            });

            ui.separator();

            // 波形
            let buffer_offset = self.display_buf.oldest_seq();
            let wave_out = self.waveform.show(
                ui,
                self.display_buf.all(),
                buffer_offset,
                self.controls.window_size,
                has_new_data,
                self.selected_channel,
            );
            if let Some(seq) = wave_out.cursor_seq {
                self.cursor.click(seq);
            }
            // 波形图右上角叠加：Actual Rate / Samples（透明背景，图例风格）
            if let Some(rect) = wave_out.plot_rect {
                self.show_plot_overlay(ui, rect);
            }
        });

        // ---- General Settings 窗口（Settings → General Settings…）----
        if self.show_settings {
            let was_active = self.controls.state != AcquisitionState::Idle;
            let old_capacity = self.buffer_capacity;
            let mut change_elf = false;
            egui::Window::new("General Settings")
                .open(&mut self.show_settings)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(500.0);
                    ui.horizontal(|ui| {
                        ui.label("Buffer capacity:");
                        ui.add(
                            egui::DragValue::new(&mut self.buffer_capacity)
                                .range(
                                    crate::ui::controls::BUFFER_CAPACITY_MIN
                                        ..=crate::ui::controls::BUFFER_CAPACITY_MAX,
                                )
                                .clamp_existing_to_range(true)
                                .speed(1_000.0)
                                .suffix(" pts"),
                        );
                    });
                    ui.add_space(4.0);
                    // 实时换算：容量 / 采样率 = 可回看秒数
                    let seconds =
                        self.buffer_capacity as f64 / self.controls.sample_rate.max(1) as f64;
                    ui.label(
                        egui::RichText::new(format!(
                            "≈ {:.1} s of history @ {} Hz",
                            seconds, self.controls.sample_rate
                        ))
                        .size(11.0)
                        .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                    );
                    ui.add_space(6.0);
                    ui.separator();
                    // ---- ELF 固件：显示当前路径（文本框样式，超长截断），可重新选择 ----
                    // 先取出路径字符串，避免与按钮的 &mut self 借用冲突
                    let elf_path = self
                        .elf_ctx
                        .as_ref()
                        .map(|c| c.file_path.clone())
                        .unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label("Current ELF:");
                        // 文本框样式：TextEdit 同款背景（extreme_bg_color）+ 浅灰边框
                        let text_style = ui.style().clone();
                        // 预留按钮宽度（约 110px），路径框占满剩余宽度
                        let path_width = (ui.available_width() - 110.0).max(140.0);
                        let truncated = truncate_path(&elf_path, (path_width / 6.5) as usize);
                        egui::Frame::NONE
                            .fill(text_style.visuals.extreme_bg_color)
                            .stroke(text_style.visuals.widgets.noninteractive.bg_stroke)
                            .corner_radius(text_style.visuals.widgets.noninteractive.corner_radius)
                            .inner_margin(egui::Margin::symmetric(6, 3))
                            .show(ui, |ui| {
                                ui.set_min_width(path_width - 12.0);
                                if elf_path.is_empty() {
                                    ui.label(
                                        egui::RichText::new("No ELF file loaded")
                                            .size(11.0)
                                            .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new(&truncated)
                                            .size(11.0)
                                            .color(egui::Color32::from_rgb(0x40, 0x40, 0x40)),
                                    );
                                }
                            });
                        if ui.button("Change ELF…").clicked() {
                            change_elf = true;
                        }
                    });
                });
            // 重新选择 ELF（闭包外调用，避免与窗口 .open 的字段借用冲突）
            if change_elf {
                self.import_elf();
            }
            // 采集运行/暂停中改动容量 → 提示下次 Start 生效
            if was_active && self.buffer_capacity != old_capacity {
                self.set_status(format!(
                    "Buffer capacity changed to {} points (applies on next Start)",
                    self.buffer_capacity
                ));
            }
        }

        // ---- ELF 同步询问弹窗（打开工程时变量地址与 ELF 不一致）----
        // X 关闭等价于 No（采用工程地址）
        if let Some(prompt) = self.elf_sync_prompt.clone() {
            let mut chose_yes = false;
            let mut chose_no = false;
            egui::Window::new("ELF Synchronization")
                .open(&mut self.elf_sync_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(380.0);
                    ui.label("The ELF file has changed. Should it be synchronized?");
                    ui.add_space(6.0);
                    if !prompt.address_diffs.is_empty() {
                        ui.label(egui::RichText::new(format!(
                            "• {} variables have different addresses",
                            prompt.address_diffs.len()
                        )).size(11.0).color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)));
                    }
                    if !prompt.missing.is_empty() {
                        ui.label(egui::RichText::new(format!(
                            "• {} variables not found in the ELF file",
                            prompt.missing.len()
                        )).size(11.0).color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)));
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            chose_yes = true;
                        }
                        if ui.button("No").clicked() {
                            chose_no = true;
                        }
                    });
                });
            if chose_yes {
                self.apply_elf_sync(&prompt);
                self.elf_sync_prompt = None;
            } else if chose_no || !self.elf_sync_open {
                // No 按钮或 X 关闭 → 统一用工程地址
                let vars = self.last_project_variables.clone();
                self.use_project_addresses(&vars);
                self.elf_sync_prompt = None;
            }
        }

        // ---- ELF 文件变化弹窗（Idle 检测到 mtime 变化）----
        // X 关闭等价于 No（保持现状 + 更新快照）
        if self.elf_change_prompt {
            let mut chose_yes = false;
            let mut chose_no = false;
            let mut open = true;
            egui::Window::new("ELF Changed")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label("The ELF file has changed on disk. Reload it?");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            chose_yes = true;
                        }
                        if ui.button("No").clicked() {
                            chose_no = true;
                        }
                    });
                });
            if chose_yes {
                self.reload_elf();
            } else if chose_no || !open {
                // No 或 X 关闭：保持现状 + 更新快照（文件再次变化才再弹）
                self.elf_change_prompt = false;
                self.refresh_elf_mtime();
            }
        }

        // ---- 通道数上限弹窗（勾选超过 8 个变量时）----
        // 底部红字容易被忽略，这里用居中弹窗明确告知被拒绝的变量
        if let Some(msg) = self.channel_limit_warning.clone() {
            let mut open = true;
            let mut close_clicked = false;
            egui::Window::new("Channel Limit")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(380.0);
                    ui.label(
                        egui::RichText::new(msg)
                            .color(egui::Color32::from_rgb(0xd0, 0x50, 0x50)),
                    );
                    ui.add_space(6.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("OK").clicked() {
                            close_clicked = true;
                        }
                    });
                });
            if !open || close_clicked {
                self.channel_limit_warning = None;
            }
        }

        // ---- ELF 加载失败警告弹窗（打开工程时 ELF 缺失，状态栏红字易被忽略）----
        // 右上角 X 关闭（.open 关联 show_elf_warning，X 点击自动置 false）
        if let Some(msg) = self.elf_warning.clone() {
            egui::Window::new("Warning")
                .open(&mut self.show_elf_warning)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(360.0);
                    ui.label(egui::RichText::new(msg).color(egui::Color32::from_rgb(0xd0, 0x50, 0x50)));
                });
            // X 关闭后清除内容，避免残留
            if !self.show_elf_warning {
                self.elf_warning = None;
            }
        }

        // ---- About 窗口（Help → About）----
        if self.show_about {
            egui::Window::new("About DAP Sampler")
                .open(&mut self.show_about)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(400.0);
                    ui.heading("DAP Sampler");
                    ui.add_space(4.0);
                    ui.label(format!("Version: v{}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(2.0);
                    // 开源仓库地址（可点击打开浏览器）
                    ui.horizontal(|ui| {
                        ui.label("github:");
                        ui.hyperlink("https://github.com/1910965995/Dap-Sampler");
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(
                        "High-speed variable sampler & visualization tool\nbased on the CMSIS-DAP v2 protocol for ARM Cortex-M",
                    );
                    ui.add_space(6.0);
                });
        }

        // 非 Running 状态时强制关闭 Follow,让 X 轴停在当前位置
        if self.controls.state != AcquisitionState::Running {
            self.waveform.set_follow(false);
        }

        // 仅在需要持续刷新时请求重绘：
        // - 采集中：波形需要持续更新
        // - 有新数据：确保最后一帧数据被绘制
        // Idle 状态时让 egui 进入低功耗等待模式，避免 CPU 100% 占用
        if self.controls.state == AcquisitionState::Running || has_new_data {
            ctx.request_repaint();
        }
        // Idle + 有 ELF + 无弹窗时每秒唤醒检测文件变化（Idle 默认低功耗不重绘）
        if self.controls.state == AcquisitionState::Idle
            && self.elf_ctx.is_some()
            && self.elf_sync_prompt.is_none()
            && !self.elf_change_prompt
            && self.elf_warning.is_none()
        {
            ctx.request_repaint_after(ELF_POLL_INTERVAL);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_acquisition();
    }
}

/// 确保 DapSamplerApp 被销毁时释放 USB 设备（即使 on_exit 未被调用）
impl Drop for DapSamplerApp {
    fn drop(&mut self) {
        if let Some(handle) = self.pipeline.take() {
            handle.stop();
        }
        // 显式释放 USB：发送 DAP_Disconnect + release_interface
        if let Some(usb) = self.active_usb.take() {
            match Arc::try_unwrap(usb) {
                Ok(mut bulk) => { bulk.release(); }
                Err(arc) => {
                    if let Some(bulk) = Arc::get_mut(&mut arc.clone()) { bulk.release(); }
                    drop(arc);
                }
            }
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn app_starts_with_ready_status() {
        let app = make_app();
        assert_eq!(app.status_text.as_deref(), Some("Ready"));
        assert!(!app.status_is_error);
    }

    #[test]
    fn set_status_records_info_message() {
        let mut app = make_app();
        app.set_status("Sample rate changed to 5000 Hz");
        assert_eq!(app.status_text.as_deref(), Some("Sample rate changed to 5000 Hz"));
        assert!(!app.status_is_error);
    }

    #[test]
    fn set_error_records_error_message() {
        let mut app = make_app();
        app.set_error("No sampling channel selected");
        assert_eq!(app.status_text.as_deref(), Some("No sampling channel selected"));
        assert!(app.status_is_error);
    }

    #[test]
    fn set_status_overwrites_previous_message() {
        let mut app = make_app();
        app.set_error("old error");
        app.set_status("new message");
        assert_eq!(app.status_text.as_deref(), Some("new message"));
        assert!(!app.status_is_error);
    }
}

#[cfg(test)]
mod open_project_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn truncate_path_shortens_long_paths() {
        // 未超长：原样返回
        assert_eq!(truncate_path("abc", 10), "abc");
        // 超长：保留前面部分 + "..."
        assert_eq!(truncate_path("abcdefghij", 8), "abcde...");
        // 极窄：只剩 "..."
        assert_eq!(truncate_path("abcdefghij", 3), "...");
        // 中文按字符截断
        assert_eq!(truncate_path("中文路径测试", 4), "中...");
        // 边界：恰好等于 max_chars 不截断
        assert_eq!(truncate_path("abcd", 4), "abcd");
    }

    #[test]
    fn open_project_missing_elf_sets_error_status() {
        let mut app = make_app();
        let dir = std::env::temp_dir();
        let proj = dir.join(format!("missing_elf_{}.dapsproj", std::process::id()));
        let json = r#"{
            "version": 1,
            "elf_path": "C:/definitely/not/exists/fw.elf",
            "sample_rate_hz": 5000,
            "swd_clock_mhz": 10,
            "window_size": 2000,
            "buffer_capacity": 200000,
            "display_mode": "line",
            "variables": [
                {"name": "pid.kp", "address": 536871168, "value_type": "float", "refresh_period_s": 0.1, "y_offset": 0.0, "y_scale": 1.0, "remark": ""}
            ]
        }"#;
        std::fs::write(&proj, json).unwrap();
        app.open_project(&proj);
        std::fs::remove_file(&proj).ok();
        assert!(app.status_is_error, "ELF 缺失时应设置错误状态，实际 status_text={:?}", app.status_text);
        assert!(
            app.status_text.as_deref().unwrap_or("").contains("ELF load failed"),
            "错误消息应提及 ELF load failed，实际 {:?}",
            app.status_text
        );
        assert!(
            app.elf_warning.is_some(),
            "ELF 缺失时应弹出警告窗口，实际 elf_warning={:?}",
            app.elf_warning
        );
        assert!(
            app.elf_warning.as_deref().unwrap_or("").contains("C:/definitely/not/exists/fw.elf"),
            "警告弹窗应包含 ELF 路径"
        );
    }
}

#[cfg(test)]
mod channel_limit_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn ninth_check_is_rejected_and_rolled_back() {
        // 前 8 个正常加入通道
        let mut app = make_app();
        let first8: Vec<(String, bool)> = (0..8)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first8);
        assert_eq!(app.channel_panel.channel_count(), 8);
        assert!(!app.status_is_error, "8 个以内不应报错");

        // 第 9 个必须被拒绝：通道数不变
        app.apply_variable_changes(vec![("var8".to_string(), true)]);
        assert_eq!(
            app.channel_panel.channel_count(),
            8,
            "第 9 个通道不得加入"
        );
        // 勾选状态必须回滚，否则 UI 显示已勾选但实际没通道
        assert!(
            !app.variable_browser.checked_paths.contains("var8"),
            "被拒绝的变量必须回滚勾选状态"
        );
        // 必须有明确提示
        assert!(app.status_is_error, "超限勾选必须给出错误提示");
        assert!(
            app.status_text.as_deref().unwrap_or("").contains("At most 8 channels"),
            "错误消息应说明通道上限，实际 {:?}",
            app.status_text
        );
        // 弹窗比底部红字醒目：必须同时挂出弹窗内容
        let popup = app.channel_limit_warning.as_deref().unwrap_or("");
        assert!(
            popup.contains("At most 8 channels") && popup.contains("var8"),
            "应弹出提示并列出被拒绝的变量，实际 {:?}",
            app.channel_limit_warning
        );
    }

    #[test]
    fn unchecking_frees_a_slot() {
        let mut app = make_app();
        let first8: Vec<(String, bool)> = (0..8)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first8);

        // 取消一个 → 腾出位置
        app.apply_variable_changes(vec![("var0".to_string(), false)]);
        assert_eq!(app.channel_panel.channel_count(), 7);

        // 现在第 9 个可以正常加入
        app.apply_variable_changes(vec![("var8".to_string(), true)]);
        assert_eq!(app.channel_panel.channel_count(), 8);
        assert!(app.variable_browser.checked_paths.contains("var8"));
        assert!(!app.status_is_error, "腾出位置后不应报错");
        assert!(
            app.channel_limit_warning.is_none(),
            "成功加入时不应弹窗"
        );
    }

    #[test]
    fn applying_popup_dismissal_clears_warning() {
        // 弹窗内容为 None 时不渲染；用户点 OK / X 后应清空
        let mut app = make_app();
        let first8: Vec<(String, bool)> = (0..8)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first8);
        app.apply_variable_changes(vec![("var9".to_string(), true)]);
        assert!(app.channel_limit_warning.is_some());

        // 模拟关闭
        app.channel_limit_warning = None;
        assert!(app.channel_limit_warning.is_none());
    }
}

#[cfg(test)]
mod write_resolve_tests {
    use super::*;
    use crate::ui::watch_panel::WriteRequest;

    #[test]
    fn resolve_channel_index_maps_name_to_snapshot_index() {
        let names = vec!["first_init".to_string(), "g_HseClkFreq".to_string()];
        assert_eq!(resolve_channel_index(&names, "first_init"), Some(0));
        assert_eq!(resolve_channel_index(&names, "g_HseClkFreq"), Some(1));
    }

    #[test]
    fn resolve_channel_index_rejects_unknown_channel() {
        // 通道被删除后，按行号写入会命中错误地址；按名字解析必须失败
        let names = vec!["first_init".to_string(), "g_HseClkFreq".to_string()];
        assert_eq!(resolve_channel_index(&names, "removed_var"), None);
        assert_eq!(resolve_channel_index(&[], "first_init"), None);
    }

    #[test]
    fn write_request_carries_channel_name_not_row_index() {
        // 回归：行号在通道增删后整体移位，写入请求必须携带名字
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(
            &["first_init".to_string(), "g_HseClkFreq".to_string()],
            &[ValueType::Uint8, ValueType::Uint32],
            &[0x1FFF_0204, 0x1FFF_017C],
        );
        // 模拟在第二行编辑后产生的写入请求（与 show() 中的构造一致）
        let req = WriteRequest {
            channel_name: panel.entries[1].name.clone(),
            raw_value: 42,
        };
        assert_eq!(req.channel_name, "g_HseClkFreq");
        // 模拟删除第一行后的重新同步：行号移位，但名字仍指向同一变量
        panel.sync_from_channels(
            &["g_HseClkFreq".to_string()],
            &[ValueType::Uint32],
            &[0x1FFF_017C],
        );
        assert_eq!(
            panel.entries.iter().position(|e| e.name == req.channel_name),
            Some(0)
        );
    }
}

#[cfg(test)]
mod elf_sync_tests {
    use super::*;
    use crate::elf::ElfVariable;

    fn project_var(name: &str, address: u32) -> ProjectVariable {
        ProjectVariable {
            name: name.to_string(),
            address,
            value_type: "float".to_string(),
            refresh_period_s: 0.1,
            y_offset: 0.0,
            y_scale: 1.0,
            remark: String::new(),
        }
    }

    fn elf_var(name: &str, address: u32) -> ElfVariable {
        ElfVariable {
            name: name.to_string(),
            path: name.to_string(),
            address,
            byte_size: 4,
            value_type: ValueType::Float,
            parent_path: None,
            source_file: None,
        }
    }

    #[test]
    fn compute_elf_diffs_none_when_identical() {
        let project = vec![project_var("pid.kp", 0x2000_0100), project_var("speed", 0x2000_0104)];
        let elf = vec![elf_var("pid.kp", 0x2000_0100), elf_var("speed", 0x2000_0104)];
        assert!(compute_elf_diffs(&project, &elf).is_none());
    }

    #[test]
    fn compute_elf_diffs_detects_address_changes() {
        let project = vec![project_var("pid.kp", 0x2000_0100)];
        let elf = vec![elf_var("pid.kp", 0x2000_0200)];
        let diff = compute_elf_diffs(&project, &elf).expect("应有差异");
        assert_eq!(diff.address_diffs, vec![("pid.kp".to_string(), 0x2000_0100, 0x2000_0200)]);
        assert!(diff.missing.is_empty());
    }

    #[test]
    fn compute_elf_diffs_detects_missing_variables() {
        let project = vec![project_var("pid.kp", 0x2000_0100), project_var("old_var", 0x2000_0108)];
        let elf = vec![elf_var("pid.kp", 0x2000_0100)];
        let diff = compute_elf_diffs(&project, &elf).expect("应有差异");
        assert!(diff.address_diffs.is_empty());
        assert_eq!(diff.missing, vec!["old_var".to_string()]);
    }

    #[test]
    fn compute_elf_diffs_mixed() {
        let project = vec![project_var("a", 0x100), project_var("b", 0x104), project_var("gone", 0x108)];
        let elf = vec![elf_var("a", 0x200), elf_var("b", 0x104)];
        let diff = compute_elf_diffs(&project, &elf).expect("应有差异");
        assert_eq!(diff.address_diffs, vec![("a".to_string(), 0x100, 0x200)]);
        assert_eq!(diff.missing, vec!["gone".to_string()]);
    }

    #[test]
    fn compute_elf_diffs_matches_by_name_or_path() {
        // ELF 用 path 匹配工程 name（变量路径带前缀时 name 与 path 不同）
        let project = vec![project_var("pid.kp", 0x2000_0100)];
        let mut ev = elf_var("kp", 0x2000_0100);
        ev.path = "pid.kp".to_string();
        assert!(compute_elf_diffs(&project, &[ev]).is_none());
    }

    #[test]
    fn apply_elf_sync_updates_watch_addresses_and_removes_missing() {
        let elf = ElfContext {
            file_path: "C:/fw/fw.elf".to_string(),
            variables: vec![elf_var("pid.kp", 0x2000_0200)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        // 模拟打开工程后的状态：通道/Watch 用旧地址，且含一个 ELF 中缺失的变量
        app.channel_panel.add_channel("pid.kp".to_string(), egui::Color32::RED, ValueType::Float);
        app.channel_panel.add_channel("gone".to_string(), egui::Color32::GREEN, ValueType::Float);
        app.watch_panel.sync_from_channels(
            &["pid.kp".to_string(), "gone".to_string()],
            &[ValueType::Float, ValueType::Float],
            &[0x2000_0100, 0x2000_0108],
        );
        let prompt = ElfSyncPrompt {
            address_diffs: vec![("pid.kp".to_string(), 0x2000_0100, 0x2000_0200)],
            missing: vec!["gone".to_string()],
        };
        app.apply_elf_sync(&prompt);
        // 缺失变量被删除
        assert!(!app.channel_panel.has_channel("gone"));
        assert!(!app.watch_panel.entries.iter().any(|e| e.name == "gone"));
        // Watch 地址更新为 ELF 地址
        let entry = app.watch_panel.entries.iter().find(|e| e.name == "pid.kp").unwrap();
        assert_eq!(entry.address, 0x2000_0200);
        // 手工地址清空（后续走 ELF 分支）
        assert!(app.manual_addresses.is_empty());
    }

    #[test]
    fn use_project_addresses_switches_to_manual_snapshot() {
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1);
        let vars = vec![project_var("pid.kp", 0x2000_0100)];
        app.use_project_addresses(&vars);
        assert_eq!(app.manual_addresses, vec![0x2000_0100]);
        assert_eq!(app.manual_channel_names, vec!["pid.kp".to_string()]);
        assert!(app.channel_panel.has_channel("pid.kp"));
        let entry = app.watch_panel.entries.iter().find(|e| e.name == "pid.kp").unwrap();
        assert_eq!(entry.address, 0x2000_0100);
    }

    #[test]
    fn remove_stale_variables_cleans_checked_and_channels() {
        let elf = ElfContext {
            file_path: "C:/fw/fw.elf".to_string(),
            variables: vec![elf_var("keep", 0x100)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.variable_browser.checked_paths.insert("keep".to_string());
        app.variable_browser.checked_paths.insert("gone".to_string());
        app.channel_panel.add_channel("keep".to_string(), egui::Color32::RED, ValueType::Float);
        app.channel_panel.add_channel("gone".to_string(), egui::Color32::GREEN, ValueType::Float);
        app.selected_channel = Some(1);
        // 新 ELF 只有 keep：gone 应从勾选与通道中移除
        app.remove_stale_variables(&[elf_var("keep", 0x100)]);
        assert!(app.variable_browser.checked_paths.contains("keep"));
        assert!(!app.variable_browser.checked_paths.contains("gone"), "已删变量不应残留勾选");
        assert!(app.channel_panel.has_channel("keep"));
        assert!(!app.channel_panel.has_channel("gone"), "已删变量不应残留通道");
        assert!(app.selected_channel.is_none(), "通道删除后选中态应清空");
        // 无 stale 时 selected_channel 保留
        app.selected_channel = Some(0);
        app.remove_stale_variables(&[elf_var("keep", 0x100)]);
        assert_eq!(app.selected_channel, Some(0));
    }

    #[test]
    fn check_elf_changed_detects_mtime_change() {
        // 临时 ELF 文件：初始快照 → 修改 mtime → 检测到变化
        let dir = std::env::temp_dir();
        let elf_path = dir.join(format!("elf_changed_test_{}.elf", std::process::id()));
        std::fs::write(&elf_path, b"ELF").unwrap();
        let elf = ElfContext {
            file_path: elf_path.display().to_string(),
            variables: vec![elf_var("pid.kp", 0x2000_0100)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.elf_mtime = std::fs::metadata(&elf_path).ok().and_then(|m| m.modified().ok());
        // 显式修改 mtime（+10s，避免文件系统粒度问题）
        let f = std::fs::File::options().write(true).open(&elf_path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::now() + std::time::Duration::from_secs(10),
        )).unwrap();
        drop(f);
        app.check_elf_changed();
        assert!(app.elf_change_prompt, "mtime 变化应触发弹窗");
        std::fs::remove_file(&elf_path).ok();
    }

    #[test]
    fn check_elf_changed_no_change_no_prompt() {
        let dir = std::env::temp_dir();
        let elf_path = dir.join(format!("elf_unchanged_test_{}.elf", std::process::id()));
        std::fs::write(&elf_path, b"ELF").unwrap();
        let elf = ElfContext {
            file_path: elf_path.display().to_string(),
            variables: vec![elf_var("pid.kp", 0x2000_0100)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.elf_mtime = std::fs::metadata(&elf_path).ok().and_then(|m| m.modified().ok());
        app.check_elf_changed();
        assert!(!app.elf_change_prompt, "mtime 未变不应触发弹窗");
        std::fs::remove_file(&elf_path).ok();
    }

    #[test]
    fn check_elf_changed_missing_file_reports_error_not_prompt() {
        // 文件被删除 → 直接错误提示，不弹 Reload 问答（点 Yes 必然失败）
        let dir = std::env::temp_dir();
        let elf_path = dir.join(format!("elf_missing_test_{}.elf", std::process::id()));
        std::fs::write(&elf_path, b"ELF").unwrap();
        let elf = ElfContext {
            file_path: elf_path.display().to_string(),
            variables: vec![elf_var("pid.kp", 0x2000_0100)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.elf_mtime = std::fs::metadata(&elf_path).ok().and_then(|m| m.modified().ok());
        std::fs::remove_file(&elf_path).unwrap();
        app.check_elf_changed();
        assert!(!app.elf_change_prompt, "文件删除不应弹 Reload 弹窗");
        assert!(app.status_is_error, "应直接提示错误");
        assert!(
            app.status_text.as_deref().unwrap_or("").contains("missing"),
            "错误消息应提及 missing，实际 {:?}",
            app.status_text
        );
        // 快照已更新为 None → 再次检测不重复报错（状态文本保持不变）
        app.status_text = None;
        app.status_is_error = false;
        app.check_elf_changed();
        assert!(!app.status_is_error, "快照更新后不应重复报错");
        assert!(app.elf_mtime.is_none(), "快照应更新为 None（文件不存在）");
    }
}
