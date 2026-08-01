use std::sync::Arc;
use std::time::Instant;
use crate::pipeline::sample::Sample;
use crate::pipeline::sample::ValueType;
use crate::pipeline::engine::{PipelineEngine, PipelineHandle};
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::swd::SwdLink;
use crate::elf::{ElfContext, ElfParser};
use super::display_buffer::DisplayBuffer;
use super::waveform::{WaveformPanel, WaveformDisplayMode};
use super::controls::{ControlPanel, AcquisitionState, AcquisitionCommand};
use super::cursor::{CursorResult, CursorState};
use super::variable_browser::VariableBrowser;
use super::channel_panel::ChannelPanel;
use super::watch_panel::WatchPanel;
use crate::project::{ProjectFile, ProjectVariable, PROJECT_VERSION};

/// 示波器风格通道色板（高明度、色相均分、暗底可读；避免灰与近邻红粉）
const CHANNEL_COLORS: [egui::Color32; 8] = [
    egui::Color32::from_rgb(255, 196, 72),  // 琥珀黄
    egui::Color32::from_rgb(64, 196, 255),  // 天蓝
    egui::Color32::from_rgb(255, 112, 168), // 玫红
    egui::Color32::from_rgb(80, 220, 140),  // 翠绿
    egui::Color32::from_rgb(200, 140, 255), // 淡紫
    egui::Color32::from_rgb(255, 148, 64),  // 橙
    egui::Color32::from_rgb(72, 230, 220),  // 青
    egui::Color32::from_rgb(180, 200, 255), // 冰蓝
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
    /// 关于窗口是否显示（帮助 → 关于）
    show_about: bool,
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

        Self {
            pipeline: None,
            active_usb: None,  // USB 连接推迟到 Start 时建立
            manual_addresses,
            acquisition_start: None,
            display_buf: DisplayBuffer::new(200_000),
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
            target_count,
            display_mode,
            window_size: 2000,
            status_text: None,
            status_is_error: false,
            selected_channel: None,
            project_path: None,
            hover_menu: None,
            show_about: false,
        }
    }

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
        self.stop_acquisition();
        // 清空 ELF / 通道 / Watch / 手工模式
        self.elf_ctx = None;
        self.variable_browser.clear();
        self.channel_panel.channels.clear();
        self.watch_panel.entries.clear();
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();
        self.current_addresses.clear();
        // 参数回默认（与 DapSamplerApp::new 一致：5000 Hz / 2000 点 / 1 MHz / Line）
        self.rate_hz = 5000;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.window_size = 2000;
        self.controls.sample_rate = self.rate_hz;
        self.controls.window_size = self.window_size;
        self.controls.swd_clock_mhz = 1;
        self.swd_clock_mhz = 1;
        let mode = WaveformDisplayMode::Line;
        self.display_mode = mode;
        self.waveform = WaveformPanel::new(vec![], vec![], self.interval_us, vec![], mode);
        // 清空运行时状态
        self.display_buf = DisplayBuffer::new(self.window_size);
        self.cursor.clear();
        self.controls.update_count(0);
        self.controls.set_stopped();
        self.selected_channel = None;
        self.controls.target_count = None;
        self.project_path = None;
        log::info!("已新建工程（清空配置）");
    }

    /// 保存当前配置为工程文件（已有路径直接覆盖，否则弹保存对话框）
    fn save_project(&mut self) {
        let file = ProjectFile {
            version: PROJECT_VERSION,
            elf_path: self.elf_ctx.as_ref().map(|c| c.file_path.clone()),
            sample_rate_hz: self.controls.sample_rate,
            swd_clock_mhz: self.controls.swd_clock_mhz,
            window_size: self.controls.window_size,
            display_mode: self.waveform.display_mode().label().to_string(),
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
                log::info!("工程已保存: {}", path.display());
            }
            Err(e) => self.set_error(format!("保存工程失败: {}", e)),
        }
    }

    /// 打开工程文件：恢复 ELF、通道、Watch 配置与全局参数
    fn open_project(&mut self, path: &std::path::Path) {
        self.status_text = None;
        self.stop_acquisition();

        // 1. 读取并解析（失败不动当前状态）
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.set_error(format!("读取工程文件失败: {}", e));
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

        // 2. 加载 ELF（失败仅警告，用保存的地址继续）
        let elf_loaded = if let Some(elf_path) = &file.elf_path {
            match ElfParser::load(elf_path) {
                Ok(ctx) => {
                    self.elf_ctx = Some(ctx);
                    log::info!("ELF 已加载: {}", elf_path);
                    true
                }
                Err(e) => {
                    self.elf_ctx = None;
                    self.set_error(format!("ELF 加载失败，已用保存的变量地址继续: {}", e));
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
                    ctx.variables.iter().any(|ev| ev.path == v.name || ev.name == v.name)
                });
                if exists {
                    self.variable_browser.checked_paths.insert(v.name.clone());
                } else {
                    missing.push(v.name.clone());
                }
            }
            if !missing.is_empty() {
                self.set_error(format!(
                    "工程中 {} 个变量在 ELF 中不存在，已跳过: {}",
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
        let mode = file.display_mode();
        self.waveform.set_display_mode(mode);
        self.display_mode = mode;

        // 6. 清空运行时状态，回到 Idle
        self.display_buf = DisplayBuffer::new(self.window_size);
        self.cursor.clear();
        self.controls.update_count(0);
        self.selected_channel = None;
        self.controls.target_count = None;

        // 7. 记录工程路径
        self.project_path = Some(path.display().to_string());
        log::info!("已打开工程: {}", path.display());
    }

    fn start_acquisition(&mut self) {
        self.status_text = None;
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
                // ELF 模式：从 channel_panel 动态获取
                let channels = &self.channel_panel.channels;
                if channels.is_empty() {
                    self.set_error("未选择任何采样通道：请加载 ELF 并勾选变量，或通过 --addresses 指定地址");
                    return;
                }

                let addresses: Vec<u32> = channels
                    .iter()
                    .filter_map(|ch| {
                        self.elf_ctx.as_ref().and_then(|ctx| {
                            ctx.variables
                                .iter()
                                .find(|v| v.path == ch.name || v.name == ch.name)
                                .map(|v| v.address)
                        })
                    })
                    .collect();

                if addresses.is_empty() {
                    self.set_error("已选择通道但没有找到对应变量地址：请检查 ELF 符号/调试信息是否匹配");
                    return;
                }

                let names: Vec<String> = channels.iter().map(|c| c.name.clone()).collect();
                let colors: Vec<egui::Color32> = channels.iter().map(|c| c.color).collect();
                let types: Vec<ValueType> = channels.iter().map(|c| c.value_type).collect();
                (addresses, names, colors, types)
            };

        if addresses.is_empty() {
            self.set_error("未选择任何采样通道：请加载 ELF 并勾选变量，或通过 --addresses 指定地址");
            return;
        }

        // 保存当前通道地址列表（用于 Watch 写入）
        let prev_addresses = std::mem::replace(&mut self.current_addresses, addresses.clone());

        // 2. 连接 USB + 初始化 SWD（仅在尚未连接时）
        if self.active_usb.is_none() {
            let mut swd = match SwdLink::new() {
                Ok(s) => s,
                Err(e) => {
                    self.set_error(format!("连接 DAP-Link 失败: {}", e));
                    return;
                }
            };
            let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
            if let Err(e) = swd.init_with_clock(clock_hz) {
                self.set_error(format!("SWD 初始化失败: {}", e));
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
        let keep_history = self.controls.state == AcquisitionState::Paused && config_unchanged;

        self.waveform =
            WaveformPanel::new(channel_names.clone(), channel_colors, self.interval_us, types.clone(), self.display_mode);

        // 4. 数据缓冲区：Pause 恢复且配置未变时保留历史（含光标与计数），否则清空重建
        if keep_history {
            self.display_buf.set_max_samples(self.window_size);
        } else {
            self.display_buf = DisplayBuffer::new(self.window_size);
            self.cursor.clear();
            self.controls.update_count(0);
        }

        // 5. 创建 PipelineEngine
        let engine = PipelineEngine::new(
            Arc::clone(self.active_usb.as_ref().unwrap()),
            DapProtocol::new(),
            addresses,
            self.rate_hz,
        );

        // 同步 Watch 面板
        self.watch_panel.sync_from_channels(&channel_names, &types, &self.current_addresses);

        // 7. 启动采集
        match engine.start() {
            Ok(handle) => {
                self.pipeline = Some(handle);
                self.controls.set_running();
                self.acquisition_start = Some(Instant::now());
            }
            Err(e) => self.set_error(format!("启动采集失败: {}", e)),
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
    }

    fn pause_acquisition(&mut self) {
        self.stop_pipeline();
        self.controls.set_paused();
        // Pause 时保持 USB 连接，恢复采集更快
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

        if self.current_addresses.is_empty() {
            log::warn!("写入请求被忽略：无通道地址");
            return;
        }

        if let Some(ref handle) = self.pipeline {
            for w in &writes {
                if let Some(&addr) = self.current_addresses.get(w.channel_idx) {
                    handle.queue_write(addr, w.raw_value);
                    log::info!(
                        "已排队写入通道 {}: 0x{:08X} → 0x{:08X}",
                        w.channel_idx, w.raw_value, addr
                    );
                } else {
                    log::warn!("通道索引 {} 超出地址范围", w.channel_idx);
                }
            }
        } else {
            // 采集未运行时，直接通过 SwdLink 写入
            match SwdLink::new() {
                Ok(mut swd) => {
                    let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
                    if let Err(e) = swd.init_with_clock(clock_hz) {
                        log::error!("写入前 SWD 初始化失败: {}", e);
                        return;
                    }
                    for w in &writes {
                        if let Some(&addr) = self.current_addresses.get(w.channel_idx) {
                            match swd.write_memory(addr, w.raw_value) {
                                Ok(()) => log::info!(
                                    "已写入通道 {}: 0x{:08X} → 0x{:08X}",
                                    w.channel_idx, w.raw_value, addr
                                ),
                                Err(e) => log::error!(
                                    "写入通道 {} 失败: {}", w.channel_idx, e
                                ),
                            }
                        }
                    }
                }
                Err(e) => log::error!("写入时连接 DAP-Link 失败: {}", e),
            }
        }
    }

    fn apply_variable_changes(&mut self, changes: Vec<(String, bool)>) {
        for (path, is_checked) in changes {
            if is_checked {
                if !self.channel_panel.has_channel(&path) {
                    let ch_count = self.channel_panel.channel_count();
                    if ch_count >= 8 {
                        continue;
                    }
                    let color = CHANNEL_COLORS[ch_count % CHANNEL_COLORS.len()];
                    let value_type = self.elf_ctx.as_ref()
                        .and_then(|ctx| ctx.variables.iter().find(|v| v.path == path))
                        .map(|v| v.value_type)
                        .unwrap_or(ValueType::Float);
                    self.channel_panel.add_channel(path, color, value_type);
                }
            } else {
                self.channel_panel.remove_by_name(&path);
            }
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
                        .find(|v| v.path == ch.name || v.name == ch.name)
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
            egui::RichText::new("Click waveform to place cursor 1\nClick again to place cursor 2")
                .size(10.0)
                .color(egui::Color32::from_rgb(0x8b, 0x94, 0x9e)),
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
                egui::Color32::from_rgb(0xe8, 0xa4, 0x00),
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
                egui::Color32::from_rgb(0xe8, 0xa4, 0x00),
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

    /// 状态信息行：波形工具按钮 + 采集状态
    ///
    /// 位于工具栏（SWD Clock / Display）下方、波形图上方，
    /// 集中显示 Follow / Auto Fit Y / Window 信息与采样状态。
    fn show_status_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            // 波形工具
            let follow_text = if self.waveform.is_follow() {
                "\u{23f8} Follow: ON"
            } else {
                "\u{25b6} Follow: OFF"
            };
            if ui.button(follow_text).clicked() {
                self.waveform.set_follow(!self.waveform.is_follow());
            }
            if ui.button("Auto Fit Y").clicked() {
                self.waveform.request_auto_fit_y();
            }
            // 实际采样率（运行中）
            if self.controls.state == AcquisitionState::Running && self.controls.actual_rate_hz > 0.0 {
                let pct = self.controls.actual_rate_hz / self.controls.sample_rate as f64 * 100.0;
                let rate_label = format!("Actual Rate: {:.0} Hz ({:.0}%)", self.controls.actual_rate_hz, pct);
                if pct < 90.0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xff, 0xa0, 0x3c),
                        rate_label,
                    ).on_hover_text(
                        "DAP-Link 吞吐量不足，实际采样率低于目标。\n波形时间轴已使用真实时间戳，波形形状应正确。\n降低采样率可获得更均匀的采样间隔。"
                    );
                } else {
                    ui.label(rate_label);
                }
            }

            // 采样信息
            if let Some(target) = self.controls.target_count {
                let pct = (self.controls.total_samples as f64 / target as f64 * 100.0).min(100.0);
                ui.label(format!("Progress: {:.0}%", pct));
            }
            if self.controls.total_samples > 0 {
                let duration = self.controls.total_samples as f64 / self.controls.sample_rate.max(1) as f64;
                ui.label(format!("Samples: {} ({:.1}s)", self.controls.total_samples, duration));
            }

            // 消费端落后提示：环形缓冲覆盖了旧采样点（数据丢失）
            if let Some(ref handle) = self.pipeline {
                let overwritten = handle.overwritten_samples();
                if overwritten > 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xff, 0xa0, 0x3c),
                        format!("Overwritten: {}", overwritten),
                    ).on_hover_text(
                        "UI 消费速度低于采样速度，环形缓冲区已覆盖旧采样点（数据丢失）。\n通常由窗口拖拽、对话框阻塞或采样率过高引起。"
                    );
                }
            }

        });
    }
}

impl eframe::App for DapSamplerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                self.hover_menu(ui, "file", "File(F)", |ui, app| {
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
                self.hover_menu(ui, "tools", "Import(I) / Export(E)", |ui, app| {
                    if menu_item(ui, "Import ELF File…").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("ELF", &["elf", "axf", "out", ""])
                            .pick_file()
                        {
                            match ElfParser::load(&path) {
                                Ok(ctx) => {
                                    log::info!("ELF loaded: {} variables", ctx.variables.len());
                                    app.elf_ctx = Some(ctx);
                                }
                                Err(e) => log::error!("ELF load failed: {}", e),
                            }
                        }
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
                                    self.apply_variable_changes(changes);
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
                if let Some(cmd) = self.controls.show(ui) {
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

                // 检测采样率变化
                if self.controls.sample_rate != self.rate_hz {
                    self.rate_hz = self.controls.sample_rate;
                    self.interval_us = 1_000_000.0 / self.rate_hz as f64;
                    self.waveform.set_interval(self.interval_us);
                    log::info!("采样率已变更为 {} Hz", self.rate_hz);
                }

                // 检测 SWD 时钟变化（仅记录日志，下次连接时生效）
                if self.controls.swd_clock_mhz != self.swd_clock_mhz {
                    self.swd_clock_mhz = self.controls.swd_clock_mhz;
                    log::info!("SWD 时钟已变更为 {} MHz（下次连接 DAP-Link 时生效）", self.swd_clock_mhz);
                }

                // 检测窗口大小变化
                if self.controls.window_size != self.window_size {
                    self.window_size = self.controls.window_size;
                    self.display_buf.set_max_samples(self.window_size);
                    log::info!("显示窗口大小已变更为 {} 个点", self.window_size);
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
                }
            });

            ui.separator();

            // 状态信息行（工具栏 SWD Clock / Display 下方）
            self.show_status_row(ui);

            ui.separator();

            // 波形
            let available_width = ui.available_width();
            let buffer_offset = self.display_buf.oldest_seq();
            if let Some(seq) = self.waveform.show(
                ui,
                self.display_buf.all(),
                buffer_offset,
                available_width,
                has_new_data,
                self.selected_channel,
            ) {
                self.cursor.click(seq);
            }
        });

        // ---- About 窗口（Help → About）----
        if self.show_about {
            egui::Window::new("About DAP Sampler")
                .open(&mut self.show_about)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.heading("DAP Sampler");
                    ui.add_space(4.0);
                    ui.label(format!("Version: v{}", env!("CARGO_PKG_VERSION")));
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
    fn set_status_records_info_message() {
        let mut app = make_app();
        app.set_status("采样率已变更为 5000 Hz");
        assert_eq!(app.status_text.as_deref(), Some("采样率已变更为 5000 Hz"));
        assert!(!app.status_is_error);
    }

    #[test]
    fn set_error_records_error_message() {
        let mut app = make_app();
        app.set_error("未选择任何采样通道");
        assert_eq!(app.status_text.as_deref(), Some("未选择任何采样通道"));
        assert!(app.status_is_error);
    }

    #[test]
    fn set_status_overwrites_previous_message() {
        let mut app = make_app();
        app.set_error("旧错误");
        app.set_status("新信息");
        assert_eq!(app.status_text.as_deref(), Some("新信息"));
        assert!(!app.status_is_error);
    }
}
