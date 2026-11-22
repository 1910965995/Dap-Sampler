/// 采集状态
use crate::i18n::t;
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AcquisitionState {
    /// 空闲（未采样；连接可以存在也可以不存在）
    Idle,
    /// 采集中
    Running,
}

/// 用户操作命令
#[derive(Debug, Clone, Copy)]
pub enum AcquisitionCommand {
    /// 连接设备（USB + SWD 初始化，目标 MCU 会复位）
    Connect,
    /// 断开并释放设备
    Disconnect,
    /// 开始采样（要求已连接）
    Start,
    /// 停止采样（不释放设备）
    Stop,
}

/// 控制面板
///
/// 提供开始/暂停/停止按钮，显示采样率和已采集点数。
pub struct ControlPanel {
    pub state: AcquisitionState,
    pub sample_rate: u32,
    pub total_samples: u64,
    pub target_count: Option<u64>,
    /// 实际达到的采样率（由外部计算后更新）
    pub actual_rate_hz: f64,
    /// 显示窗口大小（采样点数），仅作波形视野宽度（历史保留由 buffer_capacity 决定）
    pub window_size: usize,
    /// SWD 时钟频率（MHz）
    pub swd_clock_mhz: u32,
}

/// 采样率范围
pub const RATE_MIN: u32 = 1;
pub const RATE_MAX: u32 = 100_000;
/// 窗口大小范围
pub const WINDOW_MIN: usize = 1;
pub const WINDOW_MAX: usize = 100_000;
/// 缓冲区容量范围
pub const BUFFER_CAPACITY_MIN: usize = 1_000;
pub const BUFFER_CAPACITY_MAX: usize = 1_000_000;
/// 默认缓冲区容量（≈10 秒 @ 20kHz）
pub const DEFAULT_BUFFER_CAPACITY: usize = 200_000;

/// SWD 时钟可选档位（MHz）
pub const SWD_CLOCK_OPTIONS: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 30, 50];

impl ControlPanel {
    pub fn new(sample_rate: u32, target_count: Option<u64>, swd_clock_mhz: u32) -> Self {
        let sample_rate = sample_rate.clamp(RATE_MIN, RATE_MAX);
        Self {
            state: AcquisitionState::Idle,
            sample_rate,
            total_samples: 0,
            target_count,
            actual_rate_hz: 0.0,
            window_size: 2000,
            swd_clock_mhz,
        }
    }

    /// 渲染采集控制按钮，返回用户操作
    ///
    /// 两个翻转按钮：`Connect ↔ Disconnect`（连接生命周期）与
    /// `Start ↔ Stop`（采样生命周期，未连接时 Start 禁用——必须先
    /// Connect；Stop 只停采样不释放设备，再次 Start 不重连不复位）。
    /// 连接时序由用户显式控制：Connect 的初始化序列会让目标 MCU 复位，
    /// 因此采样被 Stop 后无需重新连接即可继续 Start。
    pub fn show_buttons(
        &mut self,
        ui: &mut egui::Ui,
        connected: bool,
    ) -> Option<AcquisitionCommand> {
        let mut cmd = None;

        // --- 连接控制：Connect ↔ Disconnect ---
        let connect_label = if connected {
            t("\u{23f9} Disconnect")
        } else {
            t("\u{25b6} Connect")
        };
        if ui.button(connect_label).clicked() {
            cmd = Some(if connected {
                AcquisitionCommand::Disconnect
            } else {
                AcquisitionCommand::Connect
            });
        }

        // --- 采样控制：Start ↔ Stop（未连接时 Start 禁用）---
        let sampling = self.state == AcquisitionState::Running;
        let start_label = if sampling {
            t("\u{23f9} Stop")
        } else {
            t("\u{25b6} Start")
        };
        let resp = ui.add_enabled(connected, egui::Button::new(start_label));
        let resp = if !connected {
            resp.on_disabled_hover_text(t("Connect the DAP-Link first"))
        } else {
            resp
        };
        if resp.clicked() {
            cmd = Some(if sampling {
                AcquisitionCommand::Stop
            } else {
                AcquisitionCommand::Start
            });
        }

        cmd
    }

    /// 渲染参数输入（仅 Idle 时可修改）：Rate / Window / SWD Clock
    pub fn show_params(&mut self, ui: &mut egui::Ui) {
        // --- 参数输入（仅 Idle 时可修改）---
        let is_idle = self.state == AcquisitionState::Idle;
        ui.add_enabled_ui(is_idle, |ui| {
            ui.label(t("Rate:"));
            ui.add(
                egui::DragValue::new(&mut self.sample_rate)
                    .range(RATE_MIN..=RATE_MAX)
                    .clamp_existing_to_range(true)
                    .speed(100.0)
                    .suffix(" Hz")
            );
            ui.label(t("Window:"));
            ui.add(
                egui::DragValue::new(&mut self.window_size)
                    .range(WINDOW_MIN..=WINDOW_MAX)
                    .clamp_existing_to_range(true)
                    .speed(10.0)
                    .suffix(" pts")
            );
            ui.label(t("SWD Clock:"));
            egui::ComboBox::from_id_salt("swd_clock_combo")
                .selected_text(format!("{} MHz", self.swd_clock_mhz))
                .show_ui(ui, |ui| {
                    for &mhz in SWD_CLOCK_OPTIONS {
                        ui.selectable_value(&mut self.swd_clock_mhz, mhz, format!("{} MHz", mhz));
                    }
                });
        });
    }

    /// 更新状态（由外部调用）
    pub fn set_running(&mut self) {
        self.state = AcquisitionState::Running;
    }

    pub fn set_stopped(&mut self) {
        self.state = AcquisitionState::Idle;
    }

    pub fn update_count(&mut self, total: u64) {
        self.total_samples = total;
    }
}
