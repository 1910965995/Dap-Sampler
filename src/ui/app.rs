use std::sync::Arc;
use std::time::Instant;
use crate::pipeline::sample::Sample;
use crate::pipeline::sample::ValueType;
use crate::pipeline::sample::MAX_VALUES;
use crate::pipeline::engine::{PipelineEngine, PipelineHandle, PipelineNotice};
use crate::usb::transfer::BulkTransfer;
use crate::dap::protocol::DapProtocol;
use crate::dap::swd::SwdLink;
use crate::elf::{ElfContext, ElfParser, ElfVariable};
use super::display_buffer::DisplayBuffer;
use super::waveform::{WaveformPanel, WaveformDisplayMode};
use super::controls::{ControlPanel, AcquisitionState, AcquisitionCommand};
use super::cursor::{CursorResult, CursorState};
use super::variable_browser::VariableBrowser;
use super::channel_panel::{ChannelPanel, ChannelEntry};
use super::watch_panel::{WatchPanel, WatchEntry};
use crate::project::{ProjectFile, ProjectScope, ProjectVariable, PROJECT_VERSION};
use crate::i18n::{t, tf};

/// Idle 状态下 ELF 文件变化检测的轮询间隔
const ELF_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 居中警告弹窗（标题 + 多行正文）
#[derive(Debug, Clone)]
struct WarningPopup {
    title: &'static str,
    message: String,
}

/// 示波器风格通道色板（v1.1.0 配色：红/绿/蓝/橙/品红/青/灰/粉；
/// v1.4.x 扩至 16 色以覆盖 16 通道上限：紫/黄绿/青绿/棕/玫红/深蓝/橄榄/炭黑）
const CHANNEL_COLORS: [egui::Color32; 16] = [
    egui::Color32::from_rgb(255, 68, 68),   // 红
    egui::Color32::from_rgb(68, 255, 68),   // 绿
    egui::Color32::from_rgb(68, 68, 255),   // 蓝
    egui::Color32::from_rgb(255, 200, 68),  // 橙
    egui::Color32::from_rgb(255, 68, 255),  // 品红
    egui::Color32::from_rgb(68, 255, 255),  // 青
    egui::Color32::from_rgb(180, 180, 180), // 灰
    egui::Color32::from_rgb(255, 150, 150), // 粉
    egui::Color32::from_rgb(130, 80, 255),  // 紫
    egui::Color32::from_rgb(140, 220, 30),  // 黄绿
    egui::Color32::from_rgb(0, 150, 136),   // 青绿
    egui::Color32::from_rgb(140, 90, 40),   // 棕
    egui::Color32::from_rgb(220, 40, 120),  // 玫红
    egui::Color32::from_rgb(40, 90, 200),   // 深蓝
    egui::Color32::from_rgb(140, 140, 40),  // 橄榄
    egui::Color32::from_rgb(60, 60, 60),    // 炭黑
];

/// 连接类故障的排障建议（仅随 USB/SWD 连接失败类错误显示）
const CONNECTION_HINT: &str = "Note: Check that the DAP-Link is connected, the WinUSB driver is installed, the device is not occupied by Keil/OpenOCD/pyOCD or other programs, the target board is powered, and the SWD wiring is correct; or try a lower SWD clock.";

/// 侧栏竖排标签列宽度（px，标签按钮铺满该列；列与竖分隔线间距 2px）
const SIDEBAR_TAB_W: f32 = 16.0;

/// 统一浅色主题与滚动条风格（每帧应用，覆盖 egui 默认值）
///
/// 滚动条：egui 默认悬停/拖动手柄取 widgets.fg_stroke（浅色主题下是
/// 纯黑），10px 粗黑条与浅色主题不搭。改用浮动条 + 不透明度调灰——
/// 手柄颜色仍是主题的 fg_stroke，半透明叠在白底上呈现为灰阶；闲置
/// 时是常显的细灰线，悬停展开、拖动半透明中灰。不透明度字段只作用于
/// 滚动条，不会牵动按钮/文字等其它控件配色（fg_stroke 本身未改）。
fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = egui::Color32::WHITE;
    visuals.window_fill = egui::Color32::WHITE;
    // 整体背景变白后，按钮/输入框默认底色（230 灰）显得突兀，调浅至接近白
    visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(246);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(246);
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(236);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(236);
    // CollapsingHeader 展开体（Variables/Channels/Cursor）左侧的缩进参考
    // 竖线：egui 0.31 默认开启（indent_has_left_vline），界面扁平风格下
    // 显得杂乱，去掉
    visuals.indent_has_left_vline = false;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.scroll = egui::style::ScrollStyle {
            // 闲置 3px 细灰线（25% 不透明度，靠近滚动区加深到 35%），
            // 悬停展开到 8px，拖动 35%
            bar_width: 8.0,
            floating_width: 3.0,
            dormant_handle_opacity: 0.25,
            active_handle_opacity: 0.35,
            interact_handle_opacity: 0.35,
            dormant_background_opacity: 0.0,
            active_background_opacity: 0.2,
            interact_background_opacity: 0.3,
            ..egui::style::ScrollStyle::floating()
        };
    });
}

/// 侧栏竖排文字标签：文字旋转 90° 自上而下书写，与标签条平行
///
/// 返回 true 表示被点击。选中态浅灰底 + 深色字，悬停浅灰底。
fn vertical_tab(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let font = egui::FontId::proportional(12.0);
    let galley =
        ui.painter().layout_no_wrap(label.to_string(), font, egui::Color32::PLACEHOLDER);
    let pad_y = 8.0;
    // 标签高度必须按**文本长度**（size.x）算：文字旋转 90° 后长边变为竖向，
    // 若误用行高（size.y ≈ 14px）标签矩形会远小于旋转文字，相邻标签文字
    // 全部重叠
    let size = egui::vec2(SIDEBAR_TAB_W, galley.size().x + pad_y * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            egui::Color32::from_gray(225)
        } else if resp.hovered() {
            egui::Color32::from_gray(240)
        } else {
            egui::Color32::TRANSPARENT
        };
        if bg != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, egui::CornerRadius::same(4), bg);
        }
        let color = if selected {
            egui::Color32::from_rgb(45, 48, 55)
        } else if resp.hovered() {
            egui::Color32::from_rgb(70, 74, 82)
        } else {
            egui::Color32::from_rgb(110, 118, 128)
        };
        // 旋转 +90°（屏幕坐标系下顺时针）：文字自上而下书写。
        // 旋转后包围盒宽 = galley 行高、高 = galley 文本长度，
        // 中心对齐到标签矩形中心
        let (gw, gh) = (galley.size().x, galley.size().y);
        let pos = rect.center() + egui::vec2(gh * 0.5, -gw * 0.5);
        let mut shape = egui::epaint::TextShape::new(pos, galley, color);
        shape.angle = std::f32::consts::FRAC_PI_2;
        // galley 以 PLACEHOLDER 排版，双保险：既设 fallback_color 也显式
        // 覆写文字颜色（hover/选中态切换颜色时保证生效）
        shape.override_text_color = Some(color);
        ui.painter().add(shape);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// scope 标签条标签高度（px）
const SCOPE_TAB_H: f32 = 22.0;

/// 横向 scope 标签：文字水平书写，右侧带删除 ×（配色与左栏竖排标签同款）
///
/// 整签可点（切换 scope），双击进入改名，× 是**独立的子控件**（后注册，
/// 命中优先于整签），点击删除 scope。
///
/// `id` 必须传 scope 的稳定 uid（而非布局序号自动 id）：egui 的点击判定
/// 是「按下帧记录的控件 id，在松开帧按 id 匹配」，自动 id 随内容增删移位
/// ——删除一个标签后下一个标签继承其 id，松开帧的 clicked 会落到错误
/// 的标签上，甚至落到 "+" 按钮上（删最后一个 → 松开时 "+" 被点击 →
/// 删掉又立即重建同名标签，表现为"关不掉"）。
/// 返回 (单击, 双击, ×命中)。
fn scope_tab(ui: &mut egui::Ui, id: egui::Id, label: &str, selected: bool) -> (bool, bool, bool) {
    const PAD_X: f32 = 8.0;
    const X_HIT: f32 = 16.0;
    let font = egui::FontId::proportional(12.0);
    let galley =
        ui.painter().layout_no_wrap(label.to_string(), font, egui::Color32::PLACEHOLDER);
    let galley_w = galley.size().x;
    let size = egui::vec2(galley_w + PAD_X * 2.0 + X_HIT, SCOPE_TAB_H);
    // 只推进布局游标（hover 感知不参与点击），真正的交互响应用显式 id
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    // 整签交互（先注册，× 后注册覆盖其上，× 区域内的点击归 ×）
    let resp = ui.interact(rect, id, egui::Sense::click());
    // × 子区域：独立控件，精确的按钮式点击语义
    let x_center = egui::pos2(rect.right() - X_HIT * 0.5, rect.center().y);
    let x_rect = egui::Rect::from_center_size(x_center, egui::vec2(X_HIT, SCOPE_TAB_H));
    let x_resp = ui.interact(x_rect, id.with("close"), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            egui::Color32::from_gray(225)
        } else if resp.hovered() {
            egui::Color32::from_gray(240)
        } else {
            egui::Color32::TRANSPARENT
        };
        if bg != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, egui::CornerRadius::same(4), bg);
        }
        let color = if selected {
            egui::Color32::from_rgb(45, 48, 55)
        } else if resp.hovered() {
            egui::Color32::from_rgb(70, 74, 82)
        } else {
            egui::Color32::from_rgb(110, 118, 128)
        };
        // 文字垂直居中于标签矩形，左侧留出内边距
        let text_pos = egui::pos2(rect.left() + PAD_X, rect.center().y - galley.size().y * 0.5);
        ui.painter().galley(text_pos, galley, color);

        // 右侧小 ×：1px 矢量线段，悬停在 × 上时加深（与
        // Channels 面板眼睛/× 按钮的矢量画法同一风格）
        let x_color = if x_resp.hovered() {
            egui::Color32::from_rgb(45, 48, 55)
        } else {
            color
        };
        let d = 3.0;
        let stroke = egui::Stroke::new(1.0, x_color);
        ui.painter().line_segment(
            [egui::pos2(x_center.x - d, x_center.y - d), egui::pos2(x_center.x + d, x_center.y + d)],
            stroke,
        );
        ui.painter().line_segment(
            [egui::pos2(x_center.x - d, x_center.y + d), egui::pos2(x_center.x + d, x_center.y - d)],
            stroke,
        );
    }
    if resp.hovered() || x_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    // 只认真正的 click 事件：按下未松开（is_pointer_button_down_on）期间
    // interact_pointer_pos 每帧都是 Some，若不做 clicked 门控会连删多个
    (resp.clicked(), resp.double_clicked(), x_resp.clicked())
}

#[cfg(test)]
mod sidebar_tab_tests {
    use super::*;

    /// 旋转后（angle=π/2）文字包围盒：x ∈ [pos.x-行高, pos.x]，y ∈ [pos.y, pos.y+文本长]
    fn rotated_bbox(t: &egui::epaint::TextShape) -> egui::Rect {
        let (gw, gh) = (t.galley.size().x, t.galley.size().y);
        egui::Rect::from_min_max(
            egui::pos2(t.pos.x - gh, t.pos.y),
            egui::pos2(t.pos.x, t.pos.y + gw),
        )
    }

    #[test]
    fn vertical_tabs_do_not_overlap() {
        // 回归：标签高度曾误用行高（size.y）而非文本长度（size.x），三个
        // 标签矩形塌成 ~30px，旋转文字溢出互相重叠（用户截图反馈）
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(100.0, 400.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(SIDEBAR_TAB_W, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        for label in ["Variables", "Channels", "Cursor"] {
                            let _ = vertical_tab(ui, label, false);
                        }
                    },
                );
            });
        });

        let mut boxes: Vec<egui::Rect> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                let bbox = rotated_bbox(t);
                // "Cursor" 最短也有 ~34px；旧的塌缩 bug 下相邻文字会互相
                // 重叠，由下方两两断言捕获
                assert!(
                    bbox.height() > 24.0,
                    "竖排文字包围盒应有文本长度那么高，实际 {bbox:?}"
                );
                boxes.push(bbox);
            }
        }
        assert_eq!(boxes.len(), 3, "应绘制 3 个标签文字");

        boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
        for w in boxes.windows(2) {
            assert!(
                w[1].top() >= w[0].bottom(),
                "标签文字包围盒重叠: {:?} vs {:?}",
                w[0],
                w[1]
            );
        }
    }
}

/// 侧栏竖排标签对应的分区
#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Variables,
    Channels,
    Cursor,
}

/// 单个示波器 scope 的持久配置
///
/// 多 scope 共享 ELF 文件、Buffer capacity 与唯一的 DAP 连接；每个 scope
/// 独立持有变量集（≤ MAX_VALUES）与 Rate/Window/SWD Clock/Display 参数。
/// 同一时刻只有一个 scope 实际采样，因此引擎层完全复用单 scope 模型。
///
/// 数据存放约定：活动 scope 的变量勾选 / 通道 / Watch 行「活数据」就在
/// `variable_browser.checked_paths` / `channel_panel.channels` /
/// `watch_panel.entries` 三个面板字段里（所有现有逻辑零改动即作用于活动
/// scope）；非活动 scope 的数据存档在各自 Scope 里，切换时双向拷贝搬运。
struct Scope {
    /// 创建时分配的自增 uid（标签控件 id 用它）
    ///
    /// egui 按布局序号生成自动 id：删除一个标签后，下一个标签会继承被删
    /// 标签的 id，「松开鼠标那一帧的 clicked」就会落到错误的标签上（点
    /// scope3 的 × 删掉 scope4、点最后一个 × 删掉又立即重建同名标签等）。
    /// uid 与内容/位置无关、永不复用，彻底杜绝串扰。
    id: u64,
    /// 标签条显示名称（"Scope 1"，可重命名）
    name: String,
    /// 该 scope 勾选的 ELF 变量（与 variable_browser.checked_paths 同步）
    checked_paths: std::collections::HashSet<String>,
    /// 通道列表（≤ MAX_VALUES）
    channels: Vec<ChannelEntry>,
    /// Watch 表行（按 scope 隔离，切换时与 watch_panel.entries 互换）
    watch_entries: Vec<WatchEntry>,
    /// 采样率 (Hz)
    rate_hz: u32,
    /// 显示窗口大小（采样点数）
    window_size: usize,
    /// SWD 时钟 (MHz)
    swd_clock_mhz: u32,
    /// 波形显示模式
    display_mode: WaveformDisplayMode,
    /// 按名字覆盖的采样地址（打开工程选「使用工程地址」时生成；
    /// None = 按 ELF 解析地址）
    saved_addresses: Option<std::collections::HashMap<String, u32>>,
}

impl Scope {
    /// 新建空 scope，参数取默认值（与新建工程一致：5000 Hz / 2000 点 / 1 MHz / Line）
    ///
    /// uid 为 0 占位，由 `DapSamplerApp::make_scope` 分配真实值。
    fn new(name: String) -> Self {
        Self {
            id: 0,
            name,
            checked_paths: Default::default(),
            channels: Vec::new(),
            watch_entries: Vec::new(),
            rate_hz: 5000,
            window_size: 2000,
            swd_clock_mhz: 1,
            display_mode: WaveformDisplayMode::Line,
            saved_addresses: None,
        }
    }
}

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
    /// 侧栏当前选中的竖排标签分区
    sidebar_active_tab: SidebarTab,
    /// 所有示波器 scope（多 scope 共享 ELF / Buffer capacity / DAP 连接）
    scopes: Vec<Scope>,
    /// 当前激活的 scope 索引（恒有 scopes.len() ≥ 1 且 active_scope < len）
    active_scope: usize,
    /// 正在改名的 scope（索引 + 编辑缓冲；None = 无改名进行中）
    scope_rename: Option<(usize, String)>,
    /// 改名编辑框是否需要在下一帧请求焦点（开始改名后的首帧置位）
    scope_rename_needs_focus: bool,
    /// 下一个 scope 的自增 uid（1 起；0 为占位值）
    next_scope_uid: u64,
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
    /// 状态消息是否为警告（黄色显示，不弹窗）
    ///
    /// 用于协议失步丢数据这类**自恢复**的非致命事件；错误（红）优先级
    /// 更高，set_status / set_error 会清除警告态
    status_is_warning: bool,
    /// 状态栏错误附带的排障建议（悬停时显示）
    ///
    /// 仅连接类错误（USB 打开失败、SWD 初始化失败、采集异常中止）设置，
    /// 其余错误不带——「检查 DAP-Link 连接」对通道超限这类 UI 错误毫无
    /// 意义，无差别拼接会误导排查方向。每条新消息（set_error/set_status/
    /// 直接清空）都会清掉上一条的附带建议。
    status_hint: Option<String>,
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
    /// 居中警告弹窗内容（Some = 显示）
    ///
    /// 两处来源：通道数超限（Channel Limit）、运行/暂停中变更 Watch 变量
    /// （Acquisition Running）。标题随来源不同，正文为多行红色文本。
    warning_popup: Option<WarningPopup>,
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

/// ELF 变量是否匹配给定名称（工程/通道里存的是浏览器树中的完整路径）
///
/// 主匹配是 path 精确相等；裸名兜底仅限**顶层变量**（parent_path 为
/// None）。struct 成员的裸名会与全局变量重名（全局 `status` 与成员
/// `s.status`），且 path 排序使成员叶子排在前面——若按裸名兜底匹配
/// 成员，全局变量 `status` 的通道会绑定到成员地址（原缺陷 ⑦）。
fn elf_var_matches(ev: &ElfVariable, name: &str) -> bool {
    ev.path == name || (ev.parent_path.is_none() && ev.name == name)
}

/// 比较工程变量与 ELF 变量：地址不同或 ELF 中缺失 → 返回差异；无差异 → None
///
/// 匹配规则与 collect_channel_addresses 一致（见 elf_var_matches）。
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
/// 匹配规则与 collect_channel_addresses 一致（见 elf_var_matches）。
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

        // 默认 scope（"Scope 1"）：参数取 CLI 注入值，与全局镜像保持一致
        let mut default_scope = Scope::new(tf("Scope {}", &["1"]));
        default_scope.id = 1;
        default_scope.rate_hz =
            rate_hz.clamp(crate::ui::controls::RATE_MIN, crate::ui::controls::RATE_MAX);
        default_scope.window_size = 2000;
        default_scope.swd_clock_mhz = swd_clock_mhz;
        default_scope.display_mode = display_mode;

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
            sidebar_active_tab: SidebarTab::Variables,
            scopes: vec![default_scope],
            active_scope: 0,
            scope_rename: None,
            scope_rename_needs_focus: false,
            next_scope_uid: 1,
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
            status_text: Some(t("Ready").to_string()),
            status_is_error: false,
            status_is_warning: false,
            status_hint: None,
            selected_channel: None,
            project_path: None,
            hover_menu: None,
            buffer_capacity: crate::ui::controls::DEFAULT_BUFFER_CAPACITY,
            show_settings: false,
            show_about: false,
            elf_warning: None,
            show_elf_warning: false,
            warning_popup: None,
            elf_sync_prompt: None,
            elf_change_prompt: false,
            elf_mtime: initial_elf_mtime,
            elf_sync_open: false,
            last_project_variables: Vec::new(),
        }
    }

    /// 状态栏内容（单行，超宽显示省略号 + 完整内容悬停提示）
    ///
    /// 状态栏固定单行：多行会把面板撑高并挤压波形区。
    /// `Label::truncate` 在可用宽度内截断并补 "…"，完整内容通过
    /// 悬停提示查看（错误消息额外附带排障建议）。
    fn status_bar_ui(&self, ui: &mut egui::Ui) {
        let Some(msg) = &self.status_text else {
            return;
        };
        let color = if self.status_is_error {
            egui::Color32::from_rgb(0xff, 0x55, 0x55)
        } else if self.status_is_warning {
            // 警告用琥珀黄（与波形区"光标滑出缓冲"的警告色一致）
            egui::Color32::from_rgb(0xc0, 0x82, 0x00)
        } else {
            ui.visuals().text_color()
        };
        // egui 的 TopDown 布局对普通控件不应用 main_align（初始游标把主轴
        // 延伸到无穷），固定 24px 栏高下文字贴顶。手动把标签行垂直居中：
        // 偏移 = (可用高度 - 行高) / 2，水平仍保持面板默认的左对齐
        let label_h = ui.text_style_height(&egui::TextStyle::Body);
        ui.add_space(((ui.available_height() - label_h) * 0.5).max(0.0));
        let label =
            ui.add(egui::Label::new(egui::RichText::new(msg).color(color)).truncate());
        // 被截断时悬停显示完整消息；仅连接类错误附带排障建议
        // （按错误类型，不再无差别拼接——通道超限等 UI 错误与连接无关）
        let tip = self.status_tooltip(msg);
        label.on_hover_text(tip);
    }

    /// 状态栏悬停提示：仅连接类错误（status_hint 已设置）附带排障建议
    fn status_tooltip(&self, msg: &str) -> String {
        if self.status_is_error {
            match &self.status_hint {
                Some(hint) => format!("{}\n\n{}", msg, hint),
                None => msg.to_string(),
            }
        } else {
            msg.to_string()
        }
    }

    /// 通道数上限弹窗（勾选超过 16 个变量时）
    ///
    /// 底部红字容易被忽略，用居中弹窗明确告知被拒绝的变量。
    /// 右上角 X 关闭；vscroll(false) 防止内容区撑高窗口。
    fn show_warning_popup(&mut self, ctx: &egui::Context) {
        let Some(popup) = self.warning_popup.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new(popup.title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .vscroll(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.label(
                    egui::RichText::new(popup.message)
                        .color(egui::Color32::from_rgb(0xd0, 0x50, 0x50)),
                );
            });
        if !open {
            self.warning_popup = None;
        }
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.set_error_fields(message.into(), None);
    }

    /// 记录连接类错误：悬停状态栏时附带排障建议
    ///
    /// 仅 USB/SWD 连接失败、采集异常中止这类硬件相关错误使用；
    /// 排障建议全部围绕 DAP-Link 连接，拼到无关错误上会误导排查方向。
    fn set_error_with_hint(&mut self, message: impl Into<String>, hint: impl Into<String>) {
        self.set_error_fields(message.into(), Some(hint.into()));
    }

    /// 写入错误状态，并弹出通用错误弹窗
    ///
    /// 只看底部提示很容易漏掉故障（尤其采集中视线在波形上），所以每条
    /// 错误都弹窗提醒。覆盖语义：后发生的错误替换弹窗内容，保证最新
    /// 故障可见；需要专属标题/文案的调用方（通道超限、运行态拒绝）在
    /// set_error 之后自行覆盖 warning_popup。
    fn set_error_fields(&mut self, message: String, hint: Option<String>) {
        log::error!("{}", message);
        self.status_text = Some(message.clone());
        self.status_is_error = true;
        self.status_is_warning = false;
        // 每条新消息都不继承上一条的排障建议
        self.status_hint = hint.clone();
        let message = match hint {
            Some(h) => format!("{}\n\n{}", message, h),
            None => message,
        };
        self.warning_popup = Some(WarningPopup {
            title: t("Error"),
            message,
        });
    }

    /// 记录一条普通状态消息（显示在底部状态栏，默认文字色）
    fn set_status(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = false;
        self.status_is_warning = false;
        self.status_hint = None;
    }

    /// 记录一条警告消息（状态栏黄字，不弹窗）
    ///
    /// 用于协议失步丢数据这类**自恢复**的非致命事件：丢掉的是几个采样
    /// 点，引擎随即重新对齐继续采样，弹窗打断操作不成比例；且失步可能
    /// 连续发生，逐次弹窗会刷屏。
    fn set_warning(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = false;
        self.status_is_warning = true;
        self.status_hint = None;
    }

    /// 新建工程：清空当前全部配置，参数回默认值
    fn new_project(&mut self) {
        self.status_text = None;
        self.status_is_error = false;
        self.status_hint = None;
        self.elf_warning = None;
        self.show_elf_warning = false;
        self.elf_sync_prompt = None;
        self.elf_change_prompt = false;
        self.elf_sync_open = false;
        self.last_project_variables = Vec::new();
        self.elf_mtime = None;
        self.stop_acquisition();
        // 清空 ELF / 手工模式
        self.elf_ctx = None;
        self.variable_browser.clear();
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();
        // 重置为单个默认空 scope：通道 / Watch / 参数全部回默认
        // （与启动一致：5000 Hz / 2000 点 / 1 MHz / Line）
        self.buffer_capacity = crate::ui::controls::DEFAULT_BUFFER_CAPACITY;
        self.scopes = vec![self.make_scope(tf("Scope {}", &["1"]))];
        self.active_scope = 0;
        self.scope_rename = None;
        self.scope_rename_needs_focus = false;
        self.load_scope_views(0);
        self.watch_panel.reset_col_widths();
        self.controls.target_count = None;
        self.project_path = None;
        self.set_status(t("New project created (config cleared)"));
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
                    self.set_status(tf("ELF loaded: {} variables", &[&variable_count.to_string()]));
                }
                Err(e) => {
                    self.set_error(tf("ELF load failed: {}", &[&e.to_string()]));
                }
            }
        }
    }

    /// 将缓冲区全部数据导出为 CSV（Files → Export data to…）
    fn export_data(&mut self) {
        if self.display_buf.is_empty() {
            self.set_error(t("No data to export: buffer is empty"));
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
            Ok(n) => self.set_status(tf("Data exported: {} samples to {}", &[&n.to_string(), &path.display().to_string()])),
            Err(e) => self.set_error(tf("Failed to export data: {}", &[&e.to_string()])),
        }
    }

    /// 保存当前配置为工程文件（已有路径直接覆盖，否则弹保存对话框）
    fn save_project(&mut self) {
        // 活动 scope 的活数据在面板里，先存档再统一序列化
        self.archive_active_scope();
        let file = ProjectFile {
            version: PROJECT_VERSION,
            elf_path: self.elf_ctx.as_ref().map(|c| c.file_path.clone()),
            buffer_capacity: self.buffer_capacity,
            watch_col_widths: self.watch_panel.col_widths().to_vec(),
            language: crate::i18n::current().code().to_string(),
            scopes: self
                .scopes
                .iter()
                .map(|scope| ProjectScope {
                    name: scope.name.clone(),
                    sample_rate_hz: scope.rate_hz,
                    swd_clock_mhz: scope.swd_clock_mhz,
                    window_size: scope.window_size,
                    display_mode: scope.display_mode.label().to_string(),
                    variables: scope
                        .watch_entries
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
                self.set_status(tf("Project saved: {}", &[&path.display().to_string()]));
            }
            Err(e) => self.set_error(tf("Failed to save project: {}", &[&e.to_string()])),
        }
    }

    /// 打开工程文件：恢复 ELF、多 scope 配置与 Watch 全局设置
    fn open_project(&mut self, path: &std::path::Path) {
        self.status_text = None;
        self.status_is_error = false;
        self.status_hint = None;
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
                self.set_error(tf("Failed to read project file: {}", &[&e.to_string()]));
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
        // 恢复界面语言（旧工程文件无此字段 → 英文默认）
        crate::i18n::set_language(crate::i18n::Language::from_code(&file.language));
        // 所有 scope 的变量并集：ELF 同步 diff 与「使用工程地址」都按名字
        // 作用于全部 scope（同名变量共享同一地址）
        let all_variables: Vec<ProjectVariable> = file
            .scopes
            .iter()
            .flat_map(|s| s.variables.iter().cloned())
            .collect();
        self.last_project_variables = all_variables.clone();
        // 打开工程后地址一律走 ELF 解析或每 scope 的覆盖表，
        // CLI 手工快照不再参与
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();

        // 2. 加载 ELF（失败仅警告，用工程记录的地址继续）
        let elf_loaded = if let Some(elf_path) = &file.elf_path {
            match ElfParser::load(elf_path) {
                Ok(ctx) => {
                    // 记录 mtime 快照（Idle 变化检测基准）
                    self.elf_mtime = std::fs::metadata(elf_path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    // 检测工程变量与 ELF 的差异（地址不同/缺失）→ 挂起同步询问
                    // 注意：必须在 ctx 移入 elf_ctx 之前借用 ctx.variables
                    self.elf_sync_prompt = compute_elf_diffs(&all_variables, &ctx.variables);
                    self.elf_sync_open = self.elf_sync_prompt.is_some();
                    self.elf_ctx = Some(ctx);
                    log::info!("ELF loaded: {}", elf_path);
                    true
                }
                Err(e) => {
                    self.elf_ctx = None;
                    // 只写状态栏不弹通用错误弹窗：下方 elf_warning 已是
                    // 专属弹窗，两个弹窗叠加反而干扰
                    log::error!("ELF load failed, continuing with saved variable addresses: {}", e);
                    self.status_text = Some(tf("ELF load failed, continuing with saved variable addresses: {}", &[&e.to_string()]));
                    self.status_is_error = true;
                    self.status_is_warning = false;
                    self.status_hint = None;
                    // 弹窗警告：状态栏红字容易被忽略，ELF 缺失是打开工程的关键异常
                    self.elf_warning = Some(tf(
                        "ELF file not found or failed to load:\n{}\n\nThe project will continue with saved variable addresses.",
                        &[elf_path],
                    ));
                    self.show_elf_warning = true;
                    false
                }
            }
        } else {
            self.elf_ctx = None;
            false
        };

        // 3. 逐 scope 重建（面板作暂存区，复用现有恢复逻辑），再汇总装入
        let mut restored: Vec<Scope> = Vec::new();
        let mut missing_all: Vec<String> = Vec::new();
        for p_scope in &file.scopes {
            let (scope, missing) = self.restore_scope_from_project(p_scope, elf_loaded);
            // ELF 加载失败时所有变量都按「缺失」跳过（改用工程地址采样），
            // 不再叠加 "not found in ELF" 错误覆盖上面的 ELF 加载失败状态
            if elf_loaded {
                missing_all.extend(missing);
            }
            restored.push(scope);
        }
        if !missing_all.is_empty() {
            self.set_error(tf(
                "{} variables in project not found in ELF, skipped: {}",
                &[&missing_all.len().to_string(), &missing_all.join(", ")],
            ));
        }
        if restored.is_empty() {
            restored.push(self.make_scope(tf("Scope {}", &["1"])));
        }
        self.scopes = restored;
        self.active_scope = 0;
        self.scope_rename = None;
        self.scope_rename_needs_focus = false;

        // 4. 恢复全局设置：Buffer capacity 与 Watch 列宽
        //（旧版工程无这些字段时为默认值；非法值在面板内夹紧）
        self.buffer_capacity = file
            .buffer_capacity
            .clamp(crate::ui::controls::BUFFER_CAPACITY_MIN, crate::ui::controls::BUFFER_CAPACITY_MAX);
        self.watch_panel.set_col_widths(&file.watch_col_widths);

        // 5. 清空运行时状态，回到 Idle（激活第一个 scope）
        self.load_scope_views(0);
        self.controls.target_count = None;

        // 6. 记录工程路径
        self.project_path = Some(path.display().to_string());
        if !self.status_is_error {
            self.set_status(tf("Project opened: {}", &[&path.display().to_string()]));
        }
    }

    /// 用工程文件里单个 scope 的配置，经面板暂存区重建出完整 Scope
    ///
    /// 面板（勾选 / 通道 / Watch 行）此时只作暂存区：复用现有恢复逻辑构建
    /// 通道与 Watch 行，再拷贝进返回的 Scope。调用方随后用 load_scope_views
    /// 重新灌入要激活的 scope。
    /// 返回 (Scope, ELF 中缺失被跳过的变量名列表)。
    fn restore_scope_from_project(
        &mut self,
        p_scope: &ProjectScope,
        elf_loaded: bool,
    ) -> (Scope, Vec<String>) {
        let mut scope = self.make_scope(p_scope.name.clone());
        scope.rate_hz = p_scope
            .sample_rate_hz
            .clamp(crate::ui::controls::RATE_MIN, crate::ui::controls::RATE_MAX);
        scope.window_size = p_scope
            .window_size
            .clamp(crate::ui::controls::WINDOW_MIN, crate::ui::controls::WINDOW_MAX);
        scope.swd_clock_mhz = p_scope.swd_clock_mhz.clamp(1, 50);
        scope.display_mode = p_scope.display_mode();

        // 面板暂存区清空后按工程变量重建
        self.variable_browser.checked_paths.clear();
        self.channel_panel.channels.clear();
        self.watch_panel.entries.clear();

        // ELF 已加载：只恢复其中真实存在的变量（固件符号可能已变化），
        // 缺失者告警跳过；ELF 未加载：全部按工程变量恢复通道（地址走覆盖表）
        let mut missing: Vec<String> = Vec::new();
        for v in &p_scope.variables {
            let in_elf = self.elf_ctx.as_ref().is_some_and(|ctx| {
                ctx.variables.iter().any(|ev| elf_var_matches(ev, &v.name))
            });
            let make_channel = if elf_loaded { in_elf } else { true };
            if make_channel {
                if elf_loaded {
                    self.variable_browser.checked_paths.insert(v.name.clone());
                }
                let color = self.pick_channel_color();
                self.channel_panel.add_channel(v.name.clone(), color, v.value_type());
            } else {
                missing.push(v.name.clone());
            }
        }

        // Watch 行：地址按恢复来源取值（ELF 已加载 → ELF 解析；否则用工程地址）
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
        let addresses: Vec<u32> = if elf_loaded {
            self.collect_channel_addresses()
        } else {
            p_scope
                .variables
                .iter()
                .filter(|v| !missing.contains(&v.name))
                .map(|v| v.address)
                .collect()
        };
        self.watch_panel.sync_from_channels(&names, &types, &addresses);
        // 覆盖 Watch 配置字段（y_offset/y_scale/refresh/remark 以工程为准）
        self.restore_watch_config(&p_scope.variables);

        // ELF 未加载（路径缺失/解析失败）：采样地址只能用工程记录值，
        // 生成按名字的覆盖表，Start 时绕过 ELF 解析
        if !elf_loaded {
            scope.saved_addresses = Some(
                p_scope
                    .variables
                    .iter()
                    .filter(|v| !missing.contains(&v.name))
                    .map(|v| (v.name.clone(), v.address))
                    .collect(),
            );
        }

        scope.checked_paths = self.variable_browser.checked_paths.clone();
        scope.channels = self.channel_panel.channels.clone();
        scope.watch_entries = self.watch_panel.entries.clone();
        (scope, missing)
    }

    /// 用工程变量按**名字**回查，恢复 Watch 行的地址/类型/显示配置
    ///
    /// 必须按名字回查，不能按位置 zip：ELF 模式下缺失变量不会恢复成
    /// 通道（Watch 行数 = 工程变量数 − 缺失数），按位置 zip 会把缺失
    /// 变量之后的行整体错位——比如工程变量 [A, B, C] 而 B 缺失时，
    /// 行 [A, C] 的第 2 行会被改成 B 的名字/地址/类型（原缺陷 ⑧）。
    /// 错位行可交互、可写入：写入会命中错误的 MCU 地址，破坏目标内存。
    fn restore_watch_config(&mut self, file_vars: &[ProjectVariable]) {
        let unmatched = self
            .watch_panel
            .entries
            .iter()
            .filter(|e| !file_vars.iter().any(|pv| pv.name == e.name))
            .count();
        if unmatched > 0 {
            log::warn!("{} watch rows have no matching project variable, keeping current config", unmatched);
        }
        for entry in self.watch_panel.entries.iter_mut() {
            let Some(pv) = file_vars.iter().find(|pv| pv.name == entry.name) else {
                continue;
            };
            entry.address = pv.address;
            entry.value_type = pv.value_type();
            entry.refresh_period_s = pv.refresh_period_s;
            entry.y_offset = pv.y_offset;
            entry.y_scale = pv.y_scale;
            entry.remark = pv.remark.clone();
        }
    }

    /// 同步弹窗选 Yes：地址统一用 ELF 文件的值，缺失变量从所有 scope 删除
    fn apply_elf_sync(&mut self, prompt: &ElfSyncPrompt) {
        // 1. 删除 ELF 中不存在的变量：活动 scope 走面板，其余 scope 改存档
        for name in &prompt.missing {
            self.channel_panel.remove_by_name(name);
            self.variable_browser.checked_paths.remove(name);
        }
        if !prompt.missing.is_empty() {
            self.selected_channel = None;
        }
        let missing: std::collections::HashSet<&str> =
            prompt.missing.iter().map(|s| s.as_str()).collect();
        for (i, scope) in self.scopes.iter_mut().enumerate() {
            if i == self.active_scope {
                // 活动 scope 的数据在面板里，上面已处理；地址覆盖表直接清
                scope.saved_addresses = None;
                continue;
            }
            scope.channels.retain(|c| !missing.contains(c.name.as_str()));
            scope.checked_paths.retain(|p| !missing.contains(p.as_str()));
            scope.watch_entries.retain(|e| !missing.contains(e.name.as_str()));
            // 地址以 ELF 为准：清掉工程地址覆盖表
            scope.saved_addresses = None;
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
        self.set_status(tf(
            "ELF synchronized: {} addresses updated, {} variables removed",
            &[&prompt.address_diffs.len().to_string(), &prompt.missing.len().to_string()],
        ));
        self.refresh_elf_mtime();
    }

    /// 同步弹窗选 No（或 X 关闭）：所有 scope 的采样地址统一改用工程记录值
    ///
    /// 多 scope 下按名字生成每 scope 的地址覆盖表（Start 时优先使用），
    /// 通道 / 勾选保持不动；活动 scope 的 Watch 行地址同步为工程值，
    /// 其余 scope 的 Watch 行地址在下次切换/采样时自然对齐。
    fn use_project_addresses(&mut self, variables: &[ProjectVariable]) {
        let map: std::collections::HashMap<String, u32> = variables
            .iter()
            .map(|v| (v.name.clone(), v.address))
            .collect();
        for scope in &mut self.scopes {
            scope.saved_addresses = Some(map.clone());
        }
        // 活动 scope 的 Watch 行地址同步为工程值（按名字回查，未命中留 0）
        let names: Vec<String> = self.channel_panel.channels.iter().map(|c| c.name.clone()).collect();
        let types: Vec<ValueType> = self.channel_panel.channels.iter().map(|c| c.value_type).collect();
        let addresses: Vec<u32> = self
            .channel_panel
            .channels
            .iter()
            .map(|c| map.get(&c.name).copied().unwrap_or(0))
            .collect();
        self.watch_panel.sync_from_channels(&names, &types, &addresses);
        self.set_status(t("Using saved variable addresses (ELF not synchronized)"));
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
                self.set_error(tf("ELF file is missing: {}", &[&path]));
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
        // 活动 scope 的数据在面板里；非活动 scope 直接清理存档
        let stale: Vec<String> = self
            .variable_browser
            .checked_paths
            .iter()
            .filter(|p| !elf_vars.iter().any(|ev| elf_var_matches(ev, p)))
            .cloned()
            .collect();
        if !stale.is_empty() {
            for name in &stale {
                self.variable_browser.checked_paths.remove(name);
                self.channel_panel.remove_by_name(name);
            }
            self.selected_channel = None;
        }
        for (i, scope) in self.scopes.iter_mut().enumerate() {
            if i == self.active_scope {
                continue;
            }
            let dead: Vec<String> = scope
                .checked_paths
                .iter()
                .filter(|p| !elf_vars.iter().any(|ev| elf_var_matches(ev, p)))
                .cloned()
                .collect();
            for name in &dead {
                scope.checked_paths.remove(name);
            }
            scope.channels.retain(|c| !dead.contains(&c.name));
            scope.watch_entries.retain(|e| !dead.contains(&e.name));
        }
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
                self.set_status(tf("ELF reloaded: {} variables", &[&variable_count.to_string()]));
            }
            Err(e) => {
                self.set_error(tf("ELF reload failed: {}", &[&e.to_string()]));
            }
        }
        self.refresh_elf_mtime();
        self.elf_change_prompt = false;
    }

    fn start_acquisition(&mut self) {
        self.status_text = None;
        self.status_is_error = false;
        self.status_hint = None;
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
                // types/addresses 四个列表始终一一对齐（匹配失败的通道整组跳过）。
                // 地址先查「使用工程地址」覆盖表（打开工程选 No 时生成，
                // ELF 缺失/差异也能按工程值采样），再按 ELF 解析
                let channels = &self.channel_panel.channels;
                if channels.is_empty() {
                    self.set_error(t("No sampling channel selected: load an ELF and check variables, or specify addresses with --addresses"));
                    return;
                }
                let overrides = self.scopes[self.active_scope].saved_addresses.clone();
                let matched: Vec<(&_, u32)> = channels
                    .iter()
                    .filter_map(|ch| {
                        if let Some(&addr) = overrides.as_ref().and_then(|m| m.get(&ch.name)) {
                            return Some((ch, addr));
                        }
                        self.elf_ctx.as_ref().and_then(|ctx| {
                            ctx.variables
                                .iter()
                                .find(|v| elf_var_matches(v, &ch.name))
                                .map(|v| (ch, v.address))
                        })
                    })
                    .collect();

                if matched.is_empty() {
                    self.set_error(t("Channels selected but no matching variable addresses found: check ELF symbols/debug info"));
                    return;
                }

                let addresses: Vec<u32> = matched.iter().map(|(_, a)| *a).collect();
                let names: Vec<String> = matched.iter().map(|(c, _)| c.name.clone()).collect();
                let colors: Vec<egui::Color32> = matched.iter().map(|(c, _)| c.color).collect();
                let types: Vec<ValueType> = matched.iter().map(|(c, _)| c.value_type).collect();
                (addresses, names, colors, types)
            };

        if addresses.is_empty() {
            self.set_error(t("No sampling channel selected: load an ELF and check variables, or specify addresses with --addresses"));
            return;
        }

        // 保存当前通道地址/名字/类型列表（用于 Watch 写入的子字对齐处理与名字解析）
        self.current_addresses = addresses.clone();
        self.current_names = channel_names.clone();
        self.current_types = types.clone();

        // 2. 连接 USB + 初始化 SWD（仅在尚未连接时）
        if self.active_usb.is_none() {
            let mut swd = match SwdLink::new() {
                Ok(s) => s,
                Err(e) => {
                    self.set_error_with_hint(
                        tf("Failed to connect to DAP-Link: {}", &[&e.to_string()]),
                        t(CONNECTION_HINT),
                    );
                    return;
                }
            };
            let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
            if let Err(e) = swd.init_with_clock(clock_hz) {
                self.set_error_with_hint(
                    tf("SWD initialization failed: {}", &[&e.to_string()]),
                    t(CONNECTION_HINT),
                );
                return;
            }
            let (usb, _) = swd.into_parts();
            self.active_usb = Some(Arc::new(usb));
        }

        // 3. 更新波形面板（通道名和类型）
        // 重建通道集时按名保留图例加粗状态（被删除的通道自然消失）
        let bold_names = self.waveform.bold_names().clone();
        self.waveform =
            WaveformPanel::new(channel_names.clone(), channel_colors, self.interval_us, types.clone(), self.display_mode);
        self.waveform.set_bold_names(bold_names);

        // 4. 每次开始都清空数据缓冲区重新采样（含光标与计数）；
        // Stop 后冻结的波形在屏上保留，直到下一次 Start
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
        self.cursor.clear();
        self.controls.update_count(0);

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
                self.set_status(t("Acquisition started"));
            }
            Err(e) => self.set_error(tf("Failed to start acquisition: {}", &[&e.to_string()])),
        }
    }

    fn stop_pipeline(&mut self) {
        if let Some(handle) = self.pipeline.take() {
            log::info!("stopping pipeline, waiting for worker threads to exit...");
            handle.stop();
            log::info!("pipeline stop complete");
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
        self.set_status(t("Acquisition stopped"));
    }

    // ---- 多 scope 管理 ----

    /// 把面板视图中的活动 scope 活数据与参数镜像存档回 scopes[active_scope]
    ///
    /// 面板三个 pub 字段（checked_paths / channels / watch entries）在任意
    /// 时刻就是活动 scope 的当前数据；这里拷贝存档（面板保持不动），供
    /// 切换 / 保存工程时取用。
    fn archive_active_scope(&mut self) {
        let idx = self.active_scope;
        let Some(scope) = self.scopes.get_mut(idx) else {
            return;
        };
        scope.checked_paths = self.variable_browser.checked_paths.clone();
        scope.channels = self.channel_panel.channels.clone();
        scope.watch_entries = self.watch_panel.entries.clone();
        scope.rate_hz = self.rate_hz;
        scope.window_size = self.window_size;
        scope.swd_clock_mhz = self.swd_clock_mhz;
        scope.display_mode = self.display_mode;
    }

    /// 把 scopes[idx] 的存档灌回面板视图与参数镜像，并清空运行时状态
    ///
    /// 波形 / 缓冲 / 光标不跨 scope 保留（切换即清空）：DisplayBuffer 与
    /// cursor 全局单份，重置为空；波形面板重建为空通道集（Start 时再按
    /// 通道集重建，与打开工程后的行为一致）。
    fn load_scope_views(&mut self, idx: usize) {
        let Some(scope) = self.scopes.get(idx) else {
            return;
        };
        self.variable_browser.checked_paths = scope.checked_paths.clone();
        self.channel_panel.channels = scope.channels.clone();
        self.watch_panel.entries = scope.watch_entries.clone();
        self.rate_hz = scope.rate_hz;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.window_size = scope.window_size;
        self.swd_clock_mhz = scope.swd_clock_mhz;
        self.display_mode = scope.display_mode;
        self.controls.sample_rate = self.rate_hz;
        self.controls.window_size = self.window_size;
        self.controls.swd_clock_mhz = self.swd_clock_mhz;
        self.waveform = WaveformPanel::new(
            vec![],
            vec![],
            self.interval_us,
            vec![],
            self.display_mode,
        );
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
        self.cursor.clear();
        self.selected_channel = None;
        self.controls.update_count(0);
        self.has_new_data = false;
        // 引擎快照（地址/名字/类型）属于上一个 scope 的采集会话，清空；
        // Watch 写入在重新 Start 之前应当失败而不是写错变量
        self.current_addresses.clear();
        self.current_names.clear();
        self.current_types.clear();
    }

    /// 切换完成后的收尾：按「采样状态跟随切换」规则决定是否自动开始
    ///
    /// 切换前在采集/暂停 → 用新 scope 自己的参数自动重新开始；本来就空闲
    /// → 保持空闲。新 scope 没有通道时无法采样，提示先添加变量。
    fn finish_scope_switch(&mut self, was_running: bool) {
        let name = self.scopes[self.active_scope].name.clone();
        if was_running && !self.scopes[self.active_scope].channels.is_empty() {
            self.set_status(tf("Switched to {}", &[&name]));
            self.start_acquisition();
        } else if was_running {
            self.set_status(tf(
                "Switched to {} (no channels: press Start after adding variables)",
                &[&name],
            ));
        } else {
            self.set_status(tf("Switched to {}", &[&name]));
        }
    }

    /// 切换到 scopes[idx]（点击 scope 标签）
    ///
    /// 每个 scope 的 SWD Clock 独立，连接必须以新时钟重新初始化，因此
    /// 切换总是先停止采集并释放 USB（自动开始的 Start 会重新连接）。
    fn switch_to_scope(&mut self, idx: usize) {
        if idx >= self.scopes.len() || idx == self.active_scope {
            return;
        }
        let was_running = self.controls.state != AcquisitionState::Idle;
        self.stop_acquisition();
        self.archive_active_scope();
        self.active_scope = idx;
        self.load_scope_views(idx);
        self.finish_scope_switch(was_running);
    }

    /// 创建新 scope（首个未占用编号命名）并切换过去
    fn create_scope(&mut self) {
        let was_running = self.controls.state != AcquisitionState::Idle;
        self.commit_scope_rename();
        self.stop_acquisition();
        self.archive_active_scope();
        let name = self.next_scope_name();
        let scope = self.make_scope(name);
        self.scopes.push(scope);
        self.active_scope = self.scopes.len() - 1;
        self.load_scope_views(self.active_scope);
        self.finish_scope_switch(was_running);
    }

    /// 删除 scopes[idx]；删除活动 scope 时切换到相邻 scope（采样先停止）。
    /// 始终保持至少一个 scope：删光后自动重建默认空 scope。
    fn delete_scope(&mut self, idx: usize) {
        if idx >= self.scopes.len() {
            return;
        }
        self.commit_scope_rename();
        let was_running = self.controls.state != AcquisitionState::Idle;
        self.scopes.remove(idx);
        if self.scopes.is_empty() {
            let scope = self.make_scope(self.next_scope_name());
            self.scopes.push(scope);
            self.active_scope = 0;
            self.load_scope_views(0);
            self.set_status(t("Last oscilloscope removed, created a new empty one"));
            return;
        }
        if idx == self.active_scope {
            // 删除的是活动 scope：数据在面板里随删除丢弃，进入相邻 scope
            let new_idx = idx.min(self.scopes.len() - 1);
            self.active_scope = new_idx;
            self.load_scope_views(new_idx);
            self.finish_scope_switch(was_running);
        } else {
            // 删除非活动 scope：只需修正活动索引（被删项在前时索引前移）
            if idx < self.active_scope {
                self.active_scope -= 1;
            }
            let name = self.scopes[self.active_scope].name.clone();
            self.set_status(tf("Oscilloscope removed, now on {}", &[&name]));
        }
    }

    /// 创建带自增 uid 的空 scope（uid 用于标签控件的稳定 id）
    fn make_scope(&mut self, name: String) -> Scope {
        self.next_scope_uid += 1;
        let mut scope = Scope::new(name);
        scope.id = self.next_scope_uid;
        scope
    }

    /// 下一个未占用的 scope 名称（"Scope N"，N 取首个空位）
    fn next_scope_name(&self) -> String {
        let mut n = 1;
        loop {
            let candidate = tf("Scope {}", &[&n.to_string()]);
            if self.scopes.iter().all(|s| s.name != candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// 提交正在进行的标签改名（空名/纯空白回退原名）
    fn commit_scope_rename(&mut self) {
        if let Some((idx, name)) = self.scope_rename.take() {
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                if let Some(scope) = self.scopes.get_mut(idx) {
                    scope.name = trimmed;
                }
            }
        }
        self.scope_rename_needs_focus = false;
    }

    /// scope 标签条（菜单栏下方横向一排）：点击切换、双击改名、× 删除、+ 新建
    fn show_scope_tabs(&mut self, ui: &mut egui::Ui) {
        // 本帧动作先收集、循环结束后统一执行：删除会改变索引，边渲染边
        // 执行会让同一帧内后续标签的点击落到错误的 scope 上
        let mut switch_to = None;
        let mut start_rename = None;
        let mut delete_idx = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for i in 0..self.scopes.len() {
                if self.scope_rename.as_ref().map(|r| r.0) == Some(i) {
                    // 改名编辑态：原地 TextEdit，回车/失焦提交，Esc 取消
                    let Some((_, text)) = self.scope_rename.as_mut() else {
                        unreachable!("rename entry must exist for the renaming tab");
                    };
                    let resp = ui.add(
                        egui::TextEdit::singleline(text)
                            .desired_width(120.0)
                            .font(egui::TextStyle::Body),
                    );
                    // 仅在改名开始的首帧请求焦点：之后每帧都请求会把用户
                    // 点到别处的焦点再抢回来，lost_focus 提交就永远不触发
                    if self.scope_rename_needs_focus {
                        resp.request_focus();
                        self.scope_rename_needs_focus = false;
                    }
                    let (enter, esc) = ui.ctx().input(|inp| {
                        (
                            inp.key_pressed(egui::Key::Enter),
                            inp.key_pressed(egui::Key::Escape),
                        )
                    });
                    if esc {
                        self.scope_rename = None;
                        self.scope_rename_needs_focus = false;
                    } else if resp.lost_focus() || enter {
                        self.commit_scope_rename();
                    }
                    continue;
                }
                let (name, selected) = (self.scopes[i].name.clone(), i == self.active_scope);
                // 标签控件 id 用 scope 的稳定 uid：删除/重排后不复用，
                // 「松开帧按 id 匹配 clicked」不会落到别的标签上
                let tab_id = egui::Id::new(("scope_tab", self.scopes[i].id));
                let (clicked, double_clicked, x_clicked) = scope_tab(ui, tab_id, &name, selected);
                if x_clicked {
                    delete_idx = Some(i);
                } else if double_clicked {
                    start_rename = Some((i, name));
                } else if clicked && !selected {
                    switch_to = Some(i);
                }
            }
            // "+" 快捷新建
            if ui.small_button("+").clicked() {
                self.create_scope();
            }
        });
        // 结构性操作（删除/切换/开始改名）先提交未完成的改名，避免把
        // 改名缓冲带进新的 scope 状态
        if delete_idx.is_some() || switch_to.is_some() || start_rename.is_some() {
            self.commit_scope_rename();
        }
        if let Some(i) = delete_idx {
            self.delete_scope(i);
        } else if let Some((i, name)) = start_rename {
            self.scope_rename = Some((i, name));
            self.scope_rename_needs_focus = true;
        } else if let Some(i) = switch_to {
            self.switch_to_scope(i);
        }
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

    /// 生成「写入被跳过」的用户提示（引擎只报变量索引，这里映射为通道名）
    fn write_skip_message(&self, var_idx: usize, reason: &str) -> String {
        let name = self
            .current_names
            .get(var_idx)
            .cloned()
            .unwrap_or_else(|| format!("channel #{}", var_idx));
        tf("Write to '{}' was skipped: {}", &[&name, reason])
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
            log::warn!("write request ignored: no sampling channels");
            self.set_error(t("Write failed: no sampling channels"));
            return;
        }

        if self.pipeline.is_some() {
            // 采集中：按通道名解析为引擎快照中的变量索引。
            // 名字是稳定标识——通道增删后行号会整体移位，按行号写入会命中
            // 错误的地址；名字解析失败说明该通道不在本次采集集合中，丢弃。
            // 先解析并报错（set_error 需要可变借用 self），最后再取
            // handle 入队（handle 借用 self.pipeline，两者不能交叉）
            let mut resolved = Vec::with_capacity(writes.len());
            for w in &writes {
                match resolve_channel_index(&self.current_names, &w.channel_name) {
                    Some(idx) => resolved.push((idx, w)),
                    None => {
                        log::warn!(
                            "write request dropped: channel '{}' not in the current sampling set",
                            w.channel_name
                        );
                        self.set_error(tf(
                            "Write failed: channel '{}' is not in the current sampling set",
                            &[&w.channel_name],
                        ));
                    }
                }
            }
            if let Some(ref handle) = self.pipeline {
                for (idx, w) in resolved {
                    let addr = self.current_addresses[idx];
                    handle.queue_write(idx, w.raw_value);
                    log::info!(
                        "queued write for channel {} ({}): 0x{:08X} → 0x{:08X}",
                        idx, w.channel_name, w.raw_value, addr
                    );
                }
            }
        } else {
            // 采集未运行时，直接通过 SwdLink 写入。
            // 按 Watch 行的实时地址写入（current_* 是上次运行的快照，已过期）
            match SwdLink::new() {
                Ok(mut swd) => {
                    let clock_hz = self.swd_clock_mhz.saturating_mul(1_000_000);
                    if let Err(e) = swd.init_with_clock(clock_hz) {
                        log::error!("SWD initialization failed before write: {}", e);
                        self.set_error_with_hint(
                            tf("Write failed: SWD initialization failed: {}", &[&e.to_string()]),
                            t(CONNECTION_HINT),
                        );
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
                                "write request dropped: channel '{}' not in the Watch list",
                                w.channel_name
                            );
                            self.set_error(tf(
                                "Write failed: channel '{}' is not in the Watch list",
                                &[&w.channel_name],
                            ));
                            continue;
                        };
                        match swd.write_variable(entry.address, entry.value_type.byte_size(), w.raw_value) {
                            Ok(()) => log::info!(
                                "wrote channel '{}': 0x{:08X} → 0x{:08X} ({} bytes)",
                                w.channel_name, w.raw_value, entry.address, entry.value_type.byte_size()
                            ),
                            Err(e) => {
                                log::error!(
                                    "failed to write channel '{}': {}", w.channel_name, e
                                );
                                self.set_error(tf(
                                    "Write failed for channel '{}': {}",
                                    &[&w.channel_name, &e.to_string()],
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("failed to connect to DAP-Link for write: {}", e);
                    self.set_error_with_hint(
                        tf("Write failed: cannot connect to DAP-Link: {}", &[&e.to_string()]),
                        t(CONNECTION_HINT),
                    );
                }
            }
        }
    }

    /// 运行/暂停中收到变量勾选变更：引擎快照固定，一律拒绝
    ///
    /// 复选框在渲染层点击时已先行翻转 `checked_paths`，若只报错不回滚，
    /// 「勾上了但实际没采」的状态会永久脱节。这里把每条变更取反恢复，
    /// 并用与通道超限一致的弹窗强度提醒（底部红字不够醒目）。
    fn reject_variable_changes_while_running(&mut self, changes: &[(String, bool)]) {
        for (path, is_checked) in changes {
            // 变更记录的是点击后的新状态，回滚即取反
            if *is_checked {
                self.variable_browser.checked_paths.remove(path);
            } else {
                self.variable_browser.checked_paths.insert(path.clone());
            }
        }

        let label = if changes.len() == 1 {
            t("Changed variable")
        } else {
            t("Changed variables")
        };
        let names = changes
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        // 状态栏：单行，超宽由 Label::truncate 显示省略号。
        // set_error 会先弹通用错误弹窗，下面再用专属弹窗覆盖
        self.set_error(t("Stop acquisition before changing watched variables"));
        self.warning_popup = Some(WarningPopup {
            title: t("Acquisition Running"),
            message: tf(
                "Stop acquisition before changing watched variables.\n{}: {}",
                &[label, &names],
            ),
        });
    }

    /// 为新通道选色：取调色板中最靠前的、当前未被任何通道占用的颜色
    ///
    /// 不能按「当前通道数取模」分配：删除中间通道后按数量取模会重复
    /// 分配尾部颜色——4 通道（红绿蓝黄）删掉第一个后再添加，count=3
    /// 仍取第 4 色，与现存通道撞色。按未占用色选取还天然复用被释放
    /// 的颜色：删除红色后，新通道优先拿回红色。
    /// 兜底分支仅在通道数超过调色板长度时才会走到（当前上限 16 =
    /// 调色板长度，正常不可达），此时退回按数量取模。
    fn pick_channel_color(&self) -> egui::Color32 {
        let used: Vec<egui::Color32> = self
            .channel_panel
            .channels
            .iter()
            .map(|c| c.color)
            .collect();
        CHANNEL_COLORS
            .iter()
            .copied()
            .find(|c| !used.contains(c))
            .unwrap_or(CHANNEL_COLORS[self.channel_panel.channel_count() % CHANNEL_COLORS.len()])
    }

    fn apply_variable_changes(&mut self, changes: Vec<(String, bool)>) {
        // 超限勾选必须回滚：复选框在渲染层点击时已先行置位，若这里静默跳过，
        // 「勾上了但没有通道」的状态会永久脱节。
        let mut rejected: Vec<String> = Vec::new();

        for (path, is_checked) in changes {
            if is_checked {
                if !self.channel_panel.has_channel(&path) {
                    let ch_count = self.channel_panel.channel_count();
                    if ch_count >= MAX_VALUES {
                        // 回滚勾选状态，保持 UI 与真实通道数一致
                        self.variable_browser.checked_paths.remove(&path);
                        rejected.push(path);
                        continue;
                    }
                    let color = self.pick_channel_color();
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
            let label = if rejected.len() == 1 {
                t("Unselected channel variable")
            } else {
                t("Unselected channel variables")
            };
            let names = rejected.join(", ");
            // 状态栏：单行，超宽由 Label::truncate 显示省略号。
            // set_error 会先弹通用错误弹窗，下面再用专属弹窗覆盖
            self.set_error(tf(
                "Only {} channels can be watched at a time. {}: {}",
                &[&MAX_VALUES.to_string(), label, &names],
            ));
            // 专属弹窗：多行，信息完整
            self.warning_popup = Some(WarningPopup {
                title: t("Channel Limit"),
                message: tf(
                    "Only {} channels can be watched at a time.\n{}: {}",
                    &[&MAX_VALUES.to_string(), label, &names],
                ),
            });
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
        // 未放置任何光标时：提示文字在标签页内容区内居中占满
        if self.cursor.cursor1.is_none() && self.cursor.cursor2.is_none() {
            super::text::centered_hint(ui, t("Click waveform to place cursor"));
            return;
        }
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
        let c1 = self.cursor.cursor1;
        let c2 = self.cursor.cursor2;
        // 光标滑出缓冲提示用的缓冲区范围（提前取值，滚动闭包内只持有所有权值）
        let range_text = if self.display_buf.is_empty() {
            t("(buffer cleared)").to_string()
        } else {
            format!(
                "[{} ~ {}]",
                self.display_buf.oldest_seq(),
                self.display_buf.next_seq() - 1
            )
        };

        // 读数比侧栏可见区域高（多通道 × 三段内容）时整体滚动，
        // 否则底部内容会被裁掉看不到
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            // ---- Delta ----
            if let Some((dt, dv)) = delta {
                ui.label(egui::RichText::new(t("Delta")).strong());
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
                ui.label(egui::RichText::new(t("Cursor 1")).strong());
                // 时刻紧跟标题：通道读数多时也不会被挤到看不见的位置
                ui.label(egui::RichText::new(format!("T: {:.6} s", r.time_sec)).monospace());
                for (i, v) in r.values.iter().enumerate() {
                    let color = colors.get(i).copied().unwrap_or(egui::Color32::WHITE);
                    ui.horizontal(|ui| {
                        ui.colored_label(color, "●");
                        ui.label(egui::RichText::new(format!("CH{}: {:.4}", i + 1, v)).monospace());
                    });
                }
            } else if let Some(seq) = c1 {
                // 光标存在但已滑出缓冲范围（窗口滚动/清空后）：给出可见提示，
                // 而不是静默不显示读数
                ui.colored_label(
                    egui::Color32::from_rgb(0xc0, 0x82, 0x00),
                    tf("⚠ Cursor 1 @ seq {} slid out of buffer range {}", &[&seq.to_string(), &range_text]),
                );
            }

            // ---- Cursor 2 ----
            if let Some(r) = r2 {
                ui.label(egui::RichText::new(t("Cursor 2")).strong());
                ui.label(egui::RichText::new(format!("T: {:.6} s", r.time_sec)).monospace());
                for (i, v) in r.values.iter().enumerate() {
                    let color = colors.get(i).copied().unwrap_or(egui::Color32::WHITE);
                    ui.horizontal(|ui| {
                        ui.colored_label(color, "●");
                        ui.label(egui::RichText::new(format!("CH{}: {:.4}", i + 1, v)).monospace());
                    });
                }
            } else if c2.is_some() {
                // Cursor2 已滑出缓冲（窗口滚动后）
                ui.colored_label(
                    egui::Color32::from_rgb(0xc0, 0x82, 0x00),
                    t("⚠ Cursor 2 slid out of buffer range"),
                );
            }
        });
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
                    Some(t(
                        "DAP-Link throughput insufficient, actual sample rate is below target.\nThe waveform time axis uses real timestamps, so the shape should be correct.\nLower the sample rate for more uniform sampling intervals.",
                    )),
                )
            } else {
                (ui.visuals().text_color(), None)
            };
            items.push((
                tf("Actual Rate: {} Hz ({}%)", &[&format!("{:.0}", self.controls.actual_rate_hz), &format!("{:.0}", pct)]),
                color,
                tip,
            ));
        }
        if self.controls.total_samples > 0 {
            let duration = self.controls.total_samples as f64 / self.controls.sample_rate.max(1) as f64;
            items.push((
                tf("Samples: {} ({}s)", &[&self.controls.total_samples.to_string(), &format!("{:.1}", duration)]),
                ui.visuals().text_color(),
                None,
            ));
        }
        // 采样进度（限次采集时显示）
        if let Some(target) = self.controls.target_count {
            let pct = (self.controls.total_samples as f64 / target as f64 * 100.0).min(100.0);
            items.push((tf("Progress: {}%", &[&format!("{:.0}", pct)]), ui.visuals().text_color(), None));
        }
        // 消费端落后提示：环形缓冲覆盖了旧采样点（数据丢失）
        if let Some(ref handle) = self.pipeline {
            let overwritten = handle.overwritten_samples();
            if overwritten > 0 {
                items.push((
                    tf("Overwritten: {}", &[&overwritten.to_string()]),
                    egui::Color32::from_rgb(0xd0, 0x7a, 0x00),
                    Some(t(
                        "UI consumption is slower than sampling, the ring buffer has overwritten old samples (data loss).\nUsually caused by window dragging, modal dialogs or an overly high sample rate.",
                    )),
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
    /// 窗口清屏色：eframe 默认近黑（rgb(12,12,12)），任何面板未覆盖的
    /// 区域都会显示为黑色（拖大 Watch 后曾出现黑框）。本应用全白主题，
    /// 清屏色也用白色兜底。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::WHITE.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 统一浅色主题 + 滚动条灰色风格（见 apply_theme 注释）
        apply_theme(ctx);
        // Idle 状态检测 ELF 文件变化（无其他弹窗占用时）
        if self.controls.state == AcquisitionState::Idle
            && self.elf_ctx.is_some()
            && self.elf_sync_prompt.is_none()
            && !self.elf_change_prompt
            && self.elf_warning.is_none()
        {
            self.check_elf_changed();
        }
        // 子线程健康检查：USB 写失败、设备拔出导致的致命错误/线程退出
        // 在此兜底——否则 UI 会永远显示 Running，只有日志文件里有记录
        //（原缺陷 ⑨⑩）。先停止再报错，避免 set_status 覆盖错误信息。
        if self.controls.state != AcquisitionState::Idle {
            let fatal = if let Some(ref handle) = self.pipeline {
                handle.thread_error().or_else(|| {
                    if handle.threads_alive() {
                        None
                    } else {
                        Some("acquisition thread exited unexpectedly".to_string())
                    }
                })
            } else {
                None
            };
            if let Some(msg) = fatal {
                self.stop_acquisition();
                // 中止原因基本是 USB 读/写失败（设备拔出等），附带连接排障建议
                self.set_error_with_hint(tf("Acquisition aborted: {}", &[&msg]), t(CONNECTION_HINT));
            }
        }
        // 引擎非致命故障通知：写入被跳过（用户主动发起的写失败，弹窗
        // 提醒）与协议失步丢数据（自恢复的非致命事件，降级为状态栏黄字，
        // 不弹窗打断——失步可能连续发生，逐次弹窗会刷屏）
        if let Some(ref handle) = self.pipeline {
            for notice in handle.drain_notices() {
                match notice {
                    PipelineNotice::WriteSkipped { var_idx, reason } => {
                        self.set_error(self.write_skip_message(var_idx, &reason));
                    }
                    PipelineNotice::DataDropped { message } => {
                        self.set_warning(message);
                    }
                }
            }
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
                self.hover_menu(ui, "file", t("Project(P)"), |ui, app| {
                    if menu_item(ui, t("New Project")).clicked() {
                        app.new_project();
                        return true;
                    }
                    if menu_item(ui, t("Open Project…")).clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("DAP Sampler Project", &["dapsproj"])
                            .pick_file()
                        {
                            app.open_project(&path);
                        }
                        return true;
                    }
                    if menu_item(ui, t("Save Project")).clicked() {
                        app.save_project();
                        return true;
                    }
                    false
                });

                // ---- 导入/导出菜单 ----
                self.hover_menu(ui, "tools", t("Files(F)"), |ui, app| {
                    if menu_item(ui, t("Import ELF File…")).clicked() {
                        app.import_elf();
                        return true;
                    }
                    if menu_item(ui, t("Export data to…")).clicked() {
                        app.export_data();
                        return true;
                    }
                    false
                });

                // ---- 示波器菜单：多 scope 管理 ----
                self.hover_menu(ui, "oscilloscope", t("Oscilloscope(O)"), |ui, app| {
                    if menu_item(ui, t("New Oscilloscope")).clicked() {
                        app.create_scope();
                        return true;
                    }
                    false
                });

                // ---- 设置菜单 ----
                self.hover_menu(ui, "settings", t("Settings(S)"), |ui, app| {
                    if menu_item(ui, t("General Settings…")).clicked() {
                        app.show_settings = true;
                        return true;
                    }
                    false
                });

                // ---- 帮助菜单 ----
                self.hover_menu(ui, "help", t("Help(H)"), |ui, app| {
                    if menu_item(ui, t("About")).clicked() {
                        app.show_about = true;
                        return true;
                    }
                    false
                });
            });
        });

        // ---- scope 标签条（菜单栏下方横向一排，多示波器切换）----
        // top 面板按声明顺序自上而下堆叠：在 header 之后声明即紧贴菜单栏
        egui::TopBottomPanel::top("scope_tabs").show(ctx, |ui| {
            self.show_scope_tabs(ui);
        });

        // ---- 底部状态栏（横跨整个窗口宽度，界面最底部）----
        // egui 0.31 面板规则：bottom panel 宽度避让已声明的侧边栏；
        // 先于 SidePanel::left 声明才能横跨全宽（含侧边栏下方）。
        // 堆叠语义：先声明的 bottom panel 在最底部，后声明的叠在它上方，
        // 因此状态栏先于 watch_panel 声明，紧贴窗口底边。
        //
        // 固定单行高度：状态栏消息为单行（超宽由 Label::truncate 显示 "…"），
        // 面板高度不随内容变化，避免挤压波形区。
        egui::TopBottomPanel::bottom("status_bar")
            .resizable(false)
            .exact_height(24.0)
            .show(ctx, |ui| self.status_bar_ui(ui));

        // ---- 左侧面板：竖排标签条 + 分区内容 ----
        // 标签条竖排在最左（文字旋转 90° 自上而下，与标签条平行），点击
        // 标签切换右侧内容区显示的分区，同一时间只显示一个分区
        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(220.0)
            .width_range(220.0..=400.0)
            // 默认面板内边距左右各 8px，标签列会整体被推离窗口边缘；
            // 收窄为左 2 / 右 6 / 上下 2，标签列贴边显示
            .frame(egui::Frame::NONE.fill(egui::Color32::WHITE).inner_margin(
                egui::Margin { left: 2, right: 6, top: 2, bottom: 2 },
            ))
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                    // 标签列→竖线间距 2px。必须在分配标签列**之前**设置：
                    // egui 推进布局游标用的是“前一个控件分配完那一刻”的间距，
                    // 若分配之后再改，列与竖线之间已被旧的 8px 间距顶开
                    //（右侧留白 bug）
                    let saved_gap = ui.spacing().item_spacing.x;
                    ui.spacing_mut().item_spacing.x = 2.0;

                    // --- 竖排标签条 ---
                    ui.allocate_ui_with_layout(
                        egui::vec2(SIDEBAR_TAB_W, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            for tab in [
                                SidebarTab::Variables,
                                SidebarTab::Channels,
                                SidebarTab::Cursor,
                            ] {
                                let label = match tab {
                                    SidebarTab::Variables => t("Variables"),
                                    SidebarTab::Channels => t("Channels"),
                                    SidebarTab::Cursor => t("Cursor"),
                                };
                                if vertical_tab(ui, label, self.sidebar_active_tab == tab) {
                                    self.sidebar_active_tab = tab;
                                }
                            }
                        },
                    );

                    // 手绘 1px 竖线（egui Separator 自带 ~4px 内边距）
                    let (line_rect, _) = ui.allocate_exact_size(
                        egui::vec2(1.0, ui.available_height()),
                        egui::Sense::hover(),
                    );
                    ui.painter().line_segment(
                        [
                            egui::pos2(line_rect.center().x, line_rect.top()),
                            egui::pos2(line_rect.center().x, line_rect.bottom()),
                        ],
                        ui.visuals().widgets.noninteractive.bg_stroke,
                    );
                    // 内容区恢复正常水平间距
                    ui.spacing_mut().item_spacing.x = saved_gap;

                    // --- 内容区：当前选中分区 ---
                    ui.vertical(|ui| {
                        match self.sidebar_active_tab {
                            SidebarTab::Variables => {
                                let changes = self
                                    .variable_browser
                                    .show(ui, self.elf_ctx.as_ref());
                                if !changes.is_empty() {
                                    // 采集中通道集合被引擎快照固定：增删会破坏
                                    // Watch 行与采样数据的对应关系，必须先停止
                                    if self.controls.state != AcquisitionState::Idle {
                                        self.reject_variable_changes_while_running(&changes);
                                    } else {
                                        self.apply_variable_changes(changes);
                                    }
                                }
                            }
                            SidebarTab::Channels => {
                                // 空态时隐藏计数标题，让 "No channels" 真正
                                // 居中占满整个内容区
                                if self.channel_panel.channel_count() > 0 {
                                    ui.strong(tf(
                                        "Channels ({}/{})",
                                        &[
                                            &self.channel_panel.channel_count().to_string(),
                                            &MAX_VALUES.to_string(),
                                        ],
                                    ));
                                }
                                let action =
                                    self.channel_panel.show(ui, self.selected_channel);
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
                                        self.set_error(t("Stop acquisition before removing channels"));
                                    } else {
                                        let name = self.channel_panel.channels[remove_idx]
                                            .name
                                            .clone();
                                        self.variable_browser.checked_paths.remove(&name);
                                        self.channel_panel.remove_channel(remove_idx);
                                        if let Some(sel) = self.selected_channel {
                                            if sel == remove_idx {
                                                self.selected_channel = None;
                                            } else if sel > remove_idx {
                                                self.selected_channel = Some(sel - 1);
                                            }
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
                                        let addresses = self.collect_channel_addresses();
                                        self.watch_panel.sync_from_channels(
                                            &names, &types, &addresses,
                                        );
                                    }
                                }
                            }
                            SidebarTab::Cursor => {
                                self.show_cursor_info(ui);
                            }
                        }
                    });
                });
            });


        // ---- 底部 Watch 面板 ----
        egui::TopBottomPanel::bottom("watch_panel")
            .resizable(true)
            .default_height(180.0)
            .min_height(180.0)
            .show(ctx, |ui| {
                // egui 的上下面板只把内容填充到 height_range.min（180px）：
                // 向上拖高 Watch 后，内容高度不变，面板余下区域不会被绘制，
                // 显示为窗口清屏色（黑色）。这里强制内容撑满面板高度。
                ui.set_min_height(ui.max_rect().height());
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
                            if self.controls.state == AcquisitionState::Idle {
                                self.start_acquisition();
                            }
                        }
                        AcquisitionCommand::Stop => self.stop_acquisition(),
                    }
                }

                // Follow 按钮（Start/Stop 之后、Rate 之前）
                let follow_text = if self.waveform.is_follow() {
                    t("\u{23f8} Follow: ON")
                } else {
                    t("\u{25b6} Follow: OFF")
                };
                if ui.button(follow_text).clicked() {
                    self.waveform.set_follow(!self.waveform.is_follow());
                }

                self.controls.show_params(ui);

                // 检测采样率变化（参数归属当前 scope：写镜像字段的同时写回存档）
                if self.controls.sample_rate != self.rate_hz {
                    self.rate_hz = self.controls.sample_rate;
                    self.interval_us = 1_000_000.0 / self.rate_hz as f64;
                    self.waveform.set_interval(self.interval_us);
                    self.scopes[self.active_scope].rate_hz = self.rate_hz;
                    self.set_status(tf("Sample rate changed to {} Hz", &[&self.rate_hz.to_string()]));
                }

                // 检测 SWD 时钟变化（仅记录日志，下次连接时生效）
                if self.controls.swd_clock_mhz != self.swd_clock_mhz {
                    self.swd_clock_mhz = self.controls.swd_clock_mhz;
                    self.scopes[self.active_scope].swd_clock_mhz = self.swd_clock_mhz;
                    self.set_status(tf("SWD clock changed to {} MHz (applies on next DAP-Link connection)", &[&self.swd_clock_mhz.to_string()]));
                }

                // 检测窗口大小变化
                if self.controls.window_size != self.window_size {
                    self.window_size = self.controls.window_size;
                    self.scopes[self.active_scope].window_size = self.window_size;
                    self.set_status(tf("Display window size changed to {} points", &[&self.window_size.to_string()]));
                }

                // 波形显示模式切换
                ui.label(t("Display:"));
                let mut mode = self.display_mode;
                egui::ComboBox::from_id_salt("display_mode")
                    .selected_text(match mode {
                        WaveformDisplayMode::Line => t("Line"),
                        WaveformDisplayMode::Point => t("Point"),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut mode, WaveformDisplayMode::Line, t("Line"));
                        ui.selectable_value(&mut mode, WaveformDisplayMode::Point, t("Point"));
                    });
                if mode != self.display_mode {
                    self.display_mode = mode;
                    self.scopes[self.active_scope].display_mode = mode;
                    self.waveform.set_display_mode(mode);
                    let mode_label = match mode {
                        WaveformDisplayMode::Line => t("Line"),
                        WaveformDisplayMode::Point => t("Point"),
                    };
                    self.set_status(tf("Display mode switched to {}", &[mode_label]));
                }
            });

            // 波形（与上方工具栏留出一点间距：分隔线移除后波形贴得太近）
            ui.add_space(8.0);
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
            // 语言是否在本帧被切换（窗口闭包内置位，闭包外提交状态栏提示）
            let mut lang_changed = None;
            egui::Window::new(t("General Settings"))
                .open(&mut self.show_settings)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(500.0);
                    // ---- 语言切换（立即生效，下一帧全部文本切换）----
                    // 状态栏提示在窗口闭包外提交：闭包与 .open(&mut self...)
                    // 的字段借用冲突，不能在里面调用 self 方法
                    ui.horizontal(|ui| {
                        ui.label(t("Language:"));
                        let mut lang = crate::i18n::current();
                        egui::ComboBox::from_id_salt("language_combo")
                            .selected_text(lang.label())
                            .show_ui(ui, |ui| {
                                for l in crate::i18n::ALL_LANGUAGES {
                                    ui.selectable_value(&mut lang, l, l.label());
                                }
                            });
                        if lang != crate::i18n::current() {
                            crate::i18n::set_language(lang);
                            lang_changed = Some(lang);
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(t("Buffer capacity:"));
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
                        egui::RichText::new(tf(
                            "≈ {} s of history @ {} Hz",
                            &[&format!("{:.1}", seconds), &self.controls.sample_rate.to_string()],
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
                        ui.label(t("Current ELF:"));
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
                                        egui::RichText::new(t("No ELF file loaded"))
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
                        if ui.button(t("Change ELF…")).clicked() {
                            change_elf = true;
                        }
                    });
                });
            // 重新选择 ELF（闭包外调用，避免与窗口 .open 的字段借用冲突）
            if change_elf {
                self.import_elf();
            }
            // 语言切换提示（同样在闭包外提交，理由同上）
            if lang_changed.is_some() {
                self.set_status(t("Language changed"));
            }
            // 采集运行/暂停中改动容量 → 提示下次 Start 生效
            if was_active && self.buffer_capacity != old_capacity {
                self.set_status(tf(
                    "Buffer capacity changed to {} points (applies on next Start)",
                    &[&self.buffer_capacity.to_string()],
                ));
            }
        }

        // ---- ELF 同步询问弹窗（打开工程时变量地址与 ELF 不一致）----
        // X 关闭等价于 No（采用工程地址）
        if let Some(prompt) = self.elf_sync_prompt.clone() {
            let mut chose_yes = false;
            let mut chose_no = false;
            egui::Window::new(t("ELF Synchronization"))
                .open(&mut self.elf_sync_open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(380.0);
                    ui.label(t("The ELF file has changed. Should it be synchronized?"));
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
                        if ui.button(t("Yes")).clicked() {
                            chose_yes = true;
                        }
                        if ui.button(t("No")).clicked() {
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
            egui::Window::new(t("ELF Changed"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(t("The ELF file has changed on disk. Reload it?"));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("Yes")).clicked() {
                            chose_yes = true;
                        }
                        if ui.button(t("No")).clicked() {
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

        self.show_warning_popup(ctx);

        // ---- ELF 加载失败警告弹窗（打开工程时 ELF 缺失，状态栏红字易被忽略）----
        // 右上角 X 关闭（.open 关联 show_elf_warning，X 点击自动置 false）
        if let Some(msg) = self.elf_warning.clone() {
            egui::Window::new(t("Warning"))
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
            egui::Window::new(t("About DAP Sampler"))
                .open(&mut self.show_about)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.set_min_width(400.0);
                    ui.heading("DAP Sampler");
                    ui.add_space(4.0);
                    ui.label(tf("Version: v{}", &[env!("CARGO_PKG_VERSION")]));
                    ui.add_space(2.0);
                    // 开源仓库地址（可点击打开浏览器）
                    ui.horizontal(|ui| {
                        ui.label(t("github:"));
                        ui.hyperlink("https://github.com/1910965995/Dap-Sampler");
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(
                        t("High-speed variable sampler & visualization tool\nbased on the CMSIS-DAP v2 protocol for ARM Cortex-M"),
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

    #[test]
    fn set_warning_records_warning_without_popup() {
        // 协议失步丢数据是自恢复的非致命事件：状态栏黄字即可，不弹窗
        let mut app = make_app();
        app.set_warning("Protocol desync: dropped 11 bytes of responses and resynchronized");
        assert_eq!(
            app.status_text.as_deref(),
            Some("Protocol desync: dropped 11 bytes of responses and resynchronized")
        );
        assert!(app.status_is_warning);
        assert!(!app.status_is_error);
        assert!(app.warning_popup.is_none(), "警告不应弹出错误弹窗");
    }

    #[test]
    fn newer_message_clears_warning_state() {
        let mut app = make_app();
        app.set_warning("Protocol desync: dropped 11 bytes of responses and resynchronized");
        // 后续普通消息清除警告态
        app.set_status("Acquisition started");
        assert!(!app.status_is_warning);
        // 后续错误覆盖警告态
        app.set_warning("stale warning");
        app.set_error("real error");
        assert!(app.status_is_error);
        assert!(!app.status_is_warning);
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn switch_scope_preserves_per_scope_state() {
        let mut app = make_app();
        // scope 1：一个通道 + 独立参数 + 显示模式
        app.channel_panel.add_channel("v1".to_string(), egui::Color32::RED, ValueType::Float);
        app.watch_panel.sync_from_channels(&["v1".to_string()], &[ValueType::Float], &[0x10]);
        app.rate_hz = 10_000;
        app.controls.sample_rate = 10_000;
        app.window_size = 500;
        app.display_mode = WaveformDisplayMode::Point;
        app.create_scope(); // -> "Scope 2"，切到新 scope
        assert_eq!(app.scopes.len(), 2);
        assert_eq!(app.active_scope, 1);
        // 切走后面板清空、参数回新 scope 默认值、运行时状态清空
        assert!(app.channel_panel.channels.is_empty());
        assert!(app.watch_panel.entries.is_empty());
        assert_eq!(app.controls.sample_rate, 5000);
        assert_eq!(app.controls.window_size, 2000);
        assert!(app.display_buf.is_empty());
        assert!(app.cursor.cursor1.is_none());
        // 切回 scope 1：通道 / Watch 行 / 参数 / 显示模式全部恢复
        app.switch_to_scope(0);
        assert_eq!(app.active_scope, 0);
        assert_eq!(app.channel_panel.channel_count(), 1);
        assert_eq!(app.channel_panel.channels[0].name, "v1");
        assert_eq!(app.watch_panel.entries.len(), 1);
        assert_eq!(app.rate_hz, 10_000);
        assert_eq!(app.window_size, 500);
        assert_eq!(app.controls.sample_rate, 10_000);
        assert_eq!(app.display_mode, WaveformDisplayMode::Point);
    }

    #[test]
    fn checked_paths_are_per_scope() {
        // 变量树勾选状态跟随 scope：各自独立，互不串扰
        let mut app = make_app();
        app.variable_browser.checked_paths.insert("a".to_string());
        app.create_scope();
        app.variable_browser.checked_paths.insert("b".to_string());
        app.switch_to_scope(0);
        assert!(app.variable_browser.checked_paths.contains("a"));
        assert!(!app.variable_browser.checked_paths.contains("b"));
        app.switch_to_scope(1);
        assert!(app.variable_browser.checked_paths.contains("b"));
        assert!(!app.variable_browser.checked_paths.contains("a"));
    }

    #[test]
    fn switch_scope_clears_cursor_and_selection() {
        // 波形/光标/选中通道不跨 scope 保留（切换即清空）
        let mut app = make_app();
        app.selected_channel = Some(0);
        app.cursor.click(42);
        app.create_scope();
        assert!(app.cursor.cursor1.is_none());
        assert_eq!(app.selected_channel, None);
    }

    #[test]
    fn delete_active_scope_moves_to_neighbor() {
        let mut app = make_app();
        app.create_scope(); // 2 个，active=1
        app.create_scope(); // 3 个，active=2
        app.delete_scope(2); // 删除活动 scope
        assert_eq!(app.scopes.len(), 2);
        assert_eq!(app.active_scope, 1);
        assert_eq!(app.scopes[1].name, "Scope 2");
    }

    #[test]
    fn delete_earlier_scope_adjusts_active_index() {
        let mut app = make_app();
        app.create_scope(); // 2 个，active=1
        app.delete_scope(0); // 删除非活动且在前的 scope
        assert_eq!(app.scopes.len(), 1);
        assert_eq!(app.active_scope, 0);
        assert_eq!(app.scopes[0].name, "Scope 2");
    }

    #[test]
    fn delete_last_scope_recreates_empty_default() {
        let mut app = make_app();
        app.channel_panel.add_channel("v1".to_string(), egui::Color32::RED, ValueType::Float);
        app.delete_scope(0);
        assert_eq!(app.scopes.len(), 1);
        assert_eq!(app.active_scope, 0);
        assert_eq!(app.scopes[0].name, "Scope 1");
        assert!(app.channel_panel.channels.is_empty());
        assert!(app.variable_browser.checked_paths.is_empty());
    }

    #[test]
    fn next_scope_name_skips_taken_numbers() {
        let mut app = make_app();
        assert_eq!(app.next_scope_name(), "Scope 2");
        // 用户把 Scope 1 改名后，新 scope 可以补回空位
        app.scopes[0].name = "Motor".to_string();
        assert_eq!(app.next_scope_name(), "Scope 1");
    }

    #[test]
    fn rename_commit_trims_and_empty_reverts() {
        let mut app = make_app();
        app.scope_rename = Some((0, "  Motor loop  ".to_string()));
        app.commit_scope_rename();
        assert_eq!(app.scopes[0].name, "Motor loop");
        // 空白名回退原名
        app.scope_rename = Some((0, "   ".to_string()));
        app.commit_scope_rename();
        assert_eq!(app.scopes[0].name, "Motor loop");
    }

    #[test]
    fn project_round_trip_preserves_scopes() {
        let mut app = make_app();
        // scope 1：一个通道 + 独立参数（无 ELF → 保存后走工程地址覆盖表）
        app.channel_panel.add_channel("pid.kp".to_string(), egui::Color32::RED, ValueType::Float);
        app.watch_panel.sync_from_channels(&["pid.kp".to_string()], &[ValueType::Float], &[0x2000_0100]);
        app.rate_hz = 8000;
        app.controls.sample_rate = 8000;
        app.window_size = 1500;
        app.create_scope();
        app.scopes[1].name = "Motor".to_string();

        let dir = std::env::temp_dir();
        let path = dir.join(format!("scopes_rt_{}.dapsproj", std::process::id()));
        app.project_path = Some(path.display().to_string());
        app.save_project();

        // 全新应用打开同一工程
        let mut app2 = make_app();
        app2.open_project(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(app2.scopes.len(), 2, "两个 scope 都应恢复");
        assert_eq!(app2.scopes[0].rate_hz, 8000);
        assert_eq!(app2.scopes[0].window_size, 1500);
        assert_eq!(app2.scopes[1].name, "Motor");
        // 活动 scope 0：通道与 Watch 行恢复，无 ELF → 生成工程地址覆盖表
        assert_eq!(app2.channel_panel.channel_count(), 1);
        assert_eq!(app2.channel_panel.channels[0].name, "pid.kp");
        assert_eq!(app2.watch_panel.entries[0].address, 0x2000_0100);
        assert_eq!(
            app2.scopes[0].saved_addresses.as_ref().and_then(|m| m.get("pid.kp")),
            Some(&0x2000_0100)
        );
        // 切到 scope 1：空通道、空 Watch
        app2.switch_to_scope(1);
        assert_eq!(app2.active_scope, 1);
        assert!(app2.channel_panel.channels.is_empty());
        assert!(app2.watch_panel.entries.is_empty());
    }
}

#[cfg(test)]
mod scope_tab_tests {
    use super::*;

    #[test]
    fn horizontal_tabs_do_not_overlap() {
        // 回归：标签宽度必须按文本 galley 宽（size.x）计算；若误用行高
        // 标签会窄到文字溢出互相重叠。三个长短悬殊的名称两两不重叠
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 100.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, label) in ["A", "Motor loop", "电流环 Current"].into_iter().enumerate() {
                        let id = egui::Id::new(("scope_tab", i));
                        let _ = scope_tab(ui, id, label, false);
                    }
                });
            });
        });

        let mut boxes: Vec<egui::Rect> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                // 水平文字包围盒 = pos + galley 尺寸
                boxes.push(egui::Rect::from_min_size(t.pos, t.galley.size()));
            }
        }
        assert_eq!(boxes.len(), 3, "应绘制 3 个标签文字");
        for b in &boxes {
            assert!(b.width() > 5.0, "文字应实际渲染，包围盒 {:?}", b);
        }
        boxes.sort_by(|a, b| a.left().total_cmp(&b.left()));
        for w in boxes.windows(2) {
            assert!(
                w[1].left() >= w[0].right(),
                "标签文字包围盒重叠: {:?} vs {:?}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn tab_width_scales_with_text_length() {
        // 长文本标签应显著宽于短文本标签（宽度来自文本而非固定值）
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 100.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, label) in ["A", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"].into_iter().enumerate() {
                        let id = egui::Id::new(("scope_tab", i));
                        let _ = scope_tab(ui, id, label, false);
                    }
                });
            });
        });
        let mut widths: Vec<(String, f32)> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                let bbox = egui::Rect::from_min_size(t.pos, t.galley.size());
                widths.push((t.galley.text().to_string(), bbox.width()));
            }
        }
        assert_eq!(widths.len(), 2, "应绘制 2 个标签文字");
        let short = widths.iter().find(|(t, _)| t == "A").expect("短文本").1;
        let long = widths
            .iter()
            .find(|(t, _)| t.starts_with("AAA"))
            .expect("长文本")
            .1;
        assert!(
            long > short + 100.0,
            "长文本标签应更宽: {} vs {}",
            long,
            short
        );
    }

    // ---- × 删除交互回归（egui 交互模拟）----

    /// 渲染一帧 scope 标签条
    fn frame(app: &mut DapSamplerApp, ctx: &egui::Context, raw: egui::RawInput) -> egui::FullOutput {
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                app.show_scope_tabs(ui);
            });
        })
    }

    /// 从绘制形状中提取各标签 × 的中心点（每个 × 画 2 条线段，中点相同）
    fn x_centers(output: &egui::FullOutput) -> Vec<egui::Pos2> {
        let mut mids: Vec<egui::Pos2> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::LineSegment { points, .. } = &clipped.shape {
                mids.push(egui::pos2(
                    (points[0].x + points[1].x) * 0.5,
                    (points[0].y + points[1].y) * 0.5,
                ));
            }
        }
        mids.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
        mids.dedup_by(|a, b| (a.x - b.x).abs() < 0.5 && (a.y - b.y).abs() < 0.5);
        mids
    }

    /// 模拟在 scopes[click_idx] 标签的 × 上完整点击（悬停→按下→按住数帧→松开），
    /// 返回结束后的 scope 名称列表
    fn click_x_and_collect(click_idx: usize) -> Vec<String> {
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1);
        app.create_scope();
        app.create_scope();
        app.create_scope();
        assert_eq!(app.scopes.len(), 4);

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 300.0));
        // 首帧：布局标签并取各 × 中心
        let output = frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), ..Default::default() });
        let centers = x_centers(&output);
        assert_eq!(centers.len(), 4, "4 个标签应绘制 4 个 ×");
        let target = centers[click_idx];

        let button = egui::PointerButton::Primary;
        let ev_move = egui::Event::PointerMoved(target);
        let ev_press = egui::Event::PointerButton { pos: target, button, pressed: true, modifiers: egui::Modifiers::default() };
        let ev_release = egui::Event::PointerButton { pos: target, button, pressed: false, modifiers: egui::Modifiers::default() };

        // 悬停帧
        frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), events: vec![ev_move], ..Default::default() });
        // 按下帧
        frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), events: vec![ev_press], ..Default::default() });
        // 按住不放若干帧（回归：按住期间每帧删一个的级联删除）
        for _ in 0..3 {
            frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), ..Default::default() });
        }
        // 松开帧
        frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), events: vec![ev_release], ..Default::default() });
        // 松开后两帧（回归：id 串扰导致松开帧/下一帧误删或误建）
        for _ in 0..2 {
            frame(&mut app, &ctx, egui::RawInput { screen_rect: Some(screen), ..Default::default() });
        }
        app.scopes.iter().map(|s| s.name.clone()).collect()
    }

    #[test]
    fn clicking_x_deletes_exactly_that_scope() {
        // 回归：× 删除曾出现两类 bug——
        // ① 按住期间 interact_pointer_pos 每帧有效 → 级联删除；
        // ② 标签用布局序号自动 id，删除后下一个标签继承被删 id，
        //    松开帧 clicked 落到错误标签（点 scope3 删 scope4、点最后
        //    一个关不掉还可能触发 "+" 重建）。逐个位置验证恰好删一个。
        let cases = [
            (0usize, vec!["Scope 2", "Scope 3", "Scope 4"]),
            (1usize, vec!["Scope 1", "Scope 3", "Scope 4"]),
            (2usize, vec!["Scope 1", "Scope 2", "Scope 4"]),
            (3usize, vec!["Scope 1", "Scope 2", "Scope 3"]),
        ];
        for (idx, expected) in cases {
            let names = click_x_and_collect(idx);
            assert_eq!(
                names, expected,
                "点击第 {} 个标签的 × 应只删除 Scope {}",
                idx + 1,
                idx + 1
            );
        }
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
    fn over_limit_check_is_rejected_and_rolled_back() {
        // 前 MAX_VALUES 个正常加入通道
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES);
        assert!(!app.status_is_error, "上限以内不应报错");

        // 第 MAX_VALUES+1 个必须被拒绝：通道数不变
        let overflow_name = format!("var{}", MAX_VALUES);
        app.apply_variable_changes(vec![(overflow_name.clone(), true)]);
        assert_eq!(
            app.channel_panel.channel_count(),
            MAX_VALUES,
            "超出上限的通道不得加入"
        );
        // 勾选状态必须回滚，否则 UI 显示已勾选但实际没通道
        assert!(
            !app.variable_browser.checked_paths.contains(&overflow_name),
            "被拒绝的变量必须回滚勾选状态"
        );
        // 必须有明确提示
        assert!(app.status_is_error, "超限勾选必须给出错误提示");
        let limit_msg = format!("Only {} channels", MAX_VALUES);
        let status = app.status_text.as_deref().unwrap_or("");
        assert!(
            status.contains(&limit_msg) && status.contains("Unselected channel variable"),
            "状态栏应说明通道上限与被取消的变量，实际 {:?}",
            app.status_text
        );
        // 状态栏固定单行（超宽由 Label::truncate 补 "…"）
        assert!(
            !status.contains('\n'),
            "状态栏必须是单行，实际 {:?}",
            app.status_text
        );
        // 弹窗多行，且列出被拒变量
        let popup = app.warning_popup.as_ref().map(|w| w.message.as_str()).unwrap_or("");
        assert!(
            popup.contains(&limit_msg) && popup.contains(&overflow_name),
            "弹窗应列出被拒绝的变量，实际 {:?}",
            app.warning_popup
        );
        assert!(
            popup.contains('\n'),
            "弹窗应保留多行排版，实际 {:?}",
            app.warning_popup
        );
        assert!(
            popup.contains("Unselected channel variable"),
            "弹窗应使用 Unselected channel variable 措辞，实际 {:?}",
            app.warning_popup
        );
        // "(not added)" 措辞已移除
        assert!(
            !popup.contains("(not added)") && !status.contains("(not added)"),
            "提示中不应再出现 (not added)"
        );
    }

    /// 状态栏单行截断（补 "…"），弹窗保留多行完整信息
    #[test]
    fn status_bar_truncates_while_popup_wraps() {
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        // 用超长变量名确保单行必定放不下
        let long_name = "SystemCoreClockWithAnExtremelyLongVariableNameForTesting";
        app.apply_variable_changes(vec![(long_name.to_string(), true)]);

        // 数据层：状态栏单行、弹窗多行
        let status = app.status_text.as_deref().unwrap_or("");
        let popup = app.warning_popup.as_ref().map(|w| w.message.as_str()).unwrap_or("");
        assert!(!status.contains('\n'), "状态栏必须是单行: {status:?}");
        assert!(popup.contains('\n'), "弹窗应是多行: {popup:?}");

        // 渲染层：状态栏宽度受限时以 "…" 结尾
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        ctx.set_visuals(egui::Visuals::light());
        let raw = egui::RawInput {
            // 故意窄：状态栏放不下完整消息
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(420.0, 400.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(raw.clone(), |ctx| {
            egui::TopBottomPanel::bottom("status_bar")
                .resizable(false)
                .exact_height(24.0)
                .show(ctx, |ui| app.status_bar_ui(ui));
        });
        let out = ctx.run(raw, |ctx| {
            egui::TopBottomPanel::bottom("status_bar")
                .resizable(false)
                .exact_height(24.0)
                .show(ctx, |ui| app.status_bar_ui(ui));
        });

        let texts: Vec<egui::epaint::TextShape> = out
            .shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(!texts.is_empty(), "状态栏应渲染文字");
        let galley = &texts[0].galley;
        assert!(
            galley.elided,
            "宽度不足时状态栏应被截断（elided），实际文本 {:?}",
            galley.job.text
        );
        // 单行：只会有一行
        assert_eq!(galley.rows.len(), 1, "状态栏只能渲染一行");
    }

    #[test]
    fn rejection_message_pluralizes_multiple_variables() {
        // 一次勾选多个超限变量时，文案用复数形式
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        let extra: Vec<(String, bool)> = (0..2)
            .map(|i| (format!("extra{i}"), true))
            .collect();
        app.apply_variable_changes(extra);

        let popup = app.warning_popup.as_ref().map(|w| w.message.as_str()).unwrap_or("");
        assert!(
            popup.contains("Unselected channel variables:"),
            "多个变量应用复数措辞，实际 {:?}",
            app.warning_popup
        );
        assert!(popup.contains("extra0") && popup.contains("extra1"));
    }

    #[test]
    fn unchecking_frees_a_slot() {
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);

        // 取消一个 → 腾出位置
        app.apply_variable_changes(vec![("var0".to_string(), false)]);
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES - 1);

        // 现在超出上限的新变量可以正常加入
        let new_var = format!("var{}", MAX_VALUES);
        app.apply_variable_changes(vec![(new_var.clone(), true)]);
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES);
        assert!(app.variable_browser.checked_paths.contains(&new_var));
        assert!(!app.status_is_error, "腾出位置后不应报错");
        assert!(
            app.warning_popup.is_none(),
            "成功加入时不应弹窗"
        );
    }

    /// 任意时刻所有通道颜色必须互不相同（用户报告：删第一个通道后再
    /// 添加，新通道仍是第 4 色，与现存撞色）
    fn assert_colors_all_distinct(app: &DapSamplerApp, context: &str) {
        let colors: Vec<egui::Color32> = app
            .channel_panel
            .channels
            .iter()
            .map(|c| c.color)
            .collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "{context}: 通道 {} 与 {} 颜色重复 ({:?})",
                    i, j, colors
                );
            }
        }
    }

    #[test]
    fn new_channel_after_delete_gets_unused_color() {
        // 复现用户操作序列：加 4 个变量（红绿蓝黄）→ ×掉第 1 个（红）→
        // 新增变量。旧逻辑按通道数取模：count=3 仍取第 4 色（黄），
        // 与现存黄通道撞色；再删蓝再加，又是黄
        let mut app = make_app();
        let initial: Vec<(String, bool)> = (0..4)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(initial);
        assert_eq!(app.channel_panel.channel_count(), 4);
        // 初始四色 = 调色板前 4 个，互不相同
        for (i, ch) in app.channel_panel.channels.iter().enumerate() {
            assert_eq!(ch.color, CHANNEL_COLORS[i], "初始第 {i} 个通道应为调色板第 {i} 色");
        }
        assert_colors_all_distinct(&app, "初始");

        // ×掉第 1 个（红），新增变量：新通道应拿回被释放的红色，
        // 不得与现存绿/蓝/黄重复
        app.apply_variable_changes(vec![("var0".to_string(), false)]);
        assert_eq!(app.channel_panel.channel_count(), 3);
        app.apply_variable_changes(vec![("var4".to_string(), true)]);
        assert_eq!(app.channel_panel.channel_count(), 4);
        assert_colors_all_distinct(&app, "删红后新增");
        let new_ch = app
            .channel_panel
            .channels
            .iter()
            .find(|c| c.name == "var4")
            .expect("var4 应已加入通道");
        assert_eq!(new_ch.color, CHANNEL_COLORS[0], "新通道应复用被释放的红色");

        // 再删蓝（var2），新增变量：同样不得撞色
        app.apply_variable_changes(vec![("var2".to_string(), false)]);
        assert_eq!(app.channel_panel.channel_count(), 3);
        app.apply_variable_changes(vec![("var5".to_string(), true)]);
        assert_eq!(app.channel_panel.channel_count(), 4);
        assert_colors_all_distinct(&app, "删蓝后新增");
        let new_ch = app
            .channel_panel
            .channels
            .iter()
            .find(|c| c.name == "var5")
            .expect("var5 应已加入通道");
        assert_eq!(new_ch.color, CHANNEL_COLORS[2], "新通道应复用被释放的蓝色");
    }

    #[test]
    fn batch_add_colors_are_distinct() {
        // 一次勾选满额通道时逐个取未占用色，不得重复（覆盖全部 16 色）
        let mut app = make_app();
        let batch: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(batch);
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES);
        assert_colors_all_distinct(&app, &format!("一次加入 {} 个", MAX_VALUES));
    }

    #[test]
    fn applying_popup_dismissal_clears_warning() {
        // 弹窗内容为 None 时不渲染；用户点 OK / X 后应清空
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        app.apply_variable_changes(vec![(format!("var{}", MAX_VALUES), true)]);
        assert!(app.warning_popup.is_some());

        // 模拟关闭
        app.warning_popup = None;
        assert!(app.warning_popup.is_none());
    }

    #[test]
    fn running_check_is_rejected_rolled_back_and_popup_shown() {
        // 回归：运行态勾选变更只发底部红字——复选框不回滚（勾上了但实际
        // 没采）、也没有弹窗提醒，与通道超限的提醒强度不一致
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES);

        // 模拟运行态：渲染层点击已先行翻转 checked_paths，随后上报变更
        app.controls.state = AcquisitionState::Running;
        let new_var = format!("var{}", MAX_VALUES);
        app.variable_browser.checked_paths.insert(new_var.clone());
        app.reject_variable_changes_while_running(&[(new_var.clone(), true)]);

        // 勾选状态必须回滚
        assert!(
            !app.variable_browser.checked_paths.contains(&new_var),
            "运行态勾选必须回滚复选框状态"
        );
        // 通道集合不变
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES);
        // 状态栏：单行错误提示
        assert!(app.status_is_error, "运行态变更必须给出错误提示");
        let status = app.status_text.as_deref().unwrap_or("");
        assert!(
            status.contains("Stop acquisition before changing watched variables"),
            "状态栏应说明需先停止采集，实际 {:?}",
            app.status_text
        );
        assert!(!status.contains('\n'), "状态栏必须单行，实际 {:?}", status);
        // 弹窗：标题 + 多行正文，列出被拒绝的变量
        let popup = app.warning_popup.as_ref().expect("运行态拒绝必须弹窗");
        assert_eq!(popup.title, "Acquisition Running");
        assert!(
            popup.message.contains("Stop acquisition") && popup.message.contains(&new_var),
            "弹窗应说明原因并列出变量，实际 {:?}",
            popup.message
        );
        assert!(popup.message.contains('\n'), "弹窗应保留多行排版");
    }

    #[test]
    fn running_uncheck_is_rolled_back_too() {
        // 运行态取消勾选同样是结构性变更：通道保留，复选框恢复勾选
        let mut app = make_app();
        let first_n: Vec<(String, bool)> = (0..MAX_VALUES)
            .map(|i| (format!("var{i}"), true))
            .collect();
        app.apply_variable_changes(first_n);
        assert!(app.variable_browser.checked_paths.contains("var0"));

        app.controls.state = AcquisitionState::Running;
        // 渲染层点击已把 var0 从勾选集中移除，随后上报变更
        app.variable_browser.checked_paths.remove("var0");
        app.reject_variable_changes_while_running(&[("var0".to_string(), false)]);

        assert!(
            app.variable_browser.checked_paths.contains("var0"),
            "运行态取消勾选必须回滚复选框状态"
        );
        assert_eq!(app.channel_panel.channel_count(), MAX_VALUES, "通道不得被移除");
        assert!(app.warning_popup.is_some(), "运行态拒绝必须弹窗");
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
    fn use_project_addresses_sets_saved_address_overrides() {
        // 多 scope 语义：按名字生成每 scope 的地址覆盖表，通道/勾选不动；
        // 活动 scope 的 Watch 行地址同步为工程值
        let elf = ElfContext {
            file_path: "C:/fw/fw.elf".to_string(),
            variables: vec![elf_var("pid.kp", 0x2000_0200)],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.create_scope(); // 现在有两个 scope，覆盖表应作用于全部
        app.channel_panel.add_channel("pid.kp".to_string(), egui::Color32::RED, ValueType::Float);
        app.watch_panel.sync_from_channels(
            &["pid.kp".to_string()],
            &[ValueType::Float],
            &[0x2000_0200],
        );
        let vars = vec![project_var("pid.kp", 0x2000_0100)];
        app.use_project_addresses(&vars);
        // 手工快照不参与（地址走 ELF 解析或覆盖表）
        assert!(app.manual_addresses.is_empty());
        // 通道保持不动
        assert!(app.channel_panel.has_channel("pid.kp"));
        // 所有 scope 都拿到覆盖表
        for scope in &app.scopes {
            let overrides = scope.saved_addresses.as_ref().expect("应生成地址覆盖表");
            assert_eq!(overrides.get("pid.kp"), Some(&0x2000_0100));
        }
        // 活动 scope 的 Watch 行地址更新为工程值
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

    #[test]
    fn elf_var_matches_member_does_not_shadow_global() {
        // 全局 status 与 struct 成员 s.status 裸名相同，且 path 排序使
        // 成员在前（"s.status" < "status"）：裸名兜底若命中成员，全局
        // 通道会绑定到成员地址（原缺陷 ⑦）
        let mut member = elf_var("status", 0x2000_0010);
        member.path = "s.status".to_string();
        member.parent_path = Some("s".to_string());
        let global = elf_var("status", 0x2000_0020);

        // 成员只能按完整 path 匹配，不得响应裸名
        assert!(elf_var_matches(&member, "s.status"));
        assert!(!elf_var_matches(&member, "status"), "成员裸名不得匹配全局名");
        // 顶层变量裸名兜底仍然有效
        assert!(elf_var_matches(&global, "status"));

        // 端到端：collect_channel_addresses 用同一匹配规则，工程名 "status"
        // 必须解析到全局变量地址，而不是排在变量列表前面的成员叶子
        let elf = ElfContext {
            file_path: "C:/fw/fw.elf".to_string(),
            variables: vec![member, global],
        };
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, Some(elf), vec![], 1);
        app.channel_panel.add_channel("status".to_string(), egui::Color32::RED, ValueType::Float);
        let addrs = app.collect_channel_addresses();
        assert_eq!(
            addrs,
            vec![0x2000_0020],
            "应绑定全局 status (0x20000020) 而非排在前面成员 s.status (0x20000010)"
        );
    }

    #[test]
    fn restore_watch_config_keeps_rows_aligned_when_project_has_missing_vars() {
        // 工程变量 [A, B, C]，B 在 ELF 中缺失 → 通道/Watch 只有 [A, C]。
        // 旧实现按位置 zip（entries zip 完整工程列表）会把 C 行改成 B
        // 的名字/地址/类型，错位行可交互、可写入真实 MCU 地址（原缺陷 ⑧）
        let mut va = project_var("A", 0x100);
        va.y_scale = 1.5;
        let mut vb = project_var("B", 0x104);
        vb.y_scale = 2.5;
        let mut vc = project_var("C", 0x108);
        vc.y_scale = 3.5;
        vc.remark = "keep me".to_string();

        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1);
        app.channel_panel.add_channel("A".to_string(), egui::Color32::RED, ValueType::Float);
        app.channel_panel.add_channel("C".to_string(), egui::Color32::GREEN, ValueType::Float);
        app.watch_panel.sync_from_channels(
            &["A".to_string(), "C".to_string()],
            &[ValueType::Float, ValueType::Float],
            &[0x100, 0x108],
        );

        app.restore_watch_config(&[va, vb, vc]);

        assert_eq!(app.watch_panel.entries.len(), 2, "缺失变量不得产生新行");
        let a = app.watch_panel.entries.iter().find(|e| e.name == "A").unwrap();
        let c = app.watch_panel.entries.iter().find(|e| e.name == "C").unwrap();
        assert!((a.y_scale - 1.5).abs() < 1e-9, "A 行应恢复自己的配置");
        assert!((c.y_scale - 3.5).abs() < 1e-9, "C 行应恢复自己的配置，不得错位成 B 的");
        assert_eq!(c.address, 0x108, "C 行地址不得被 B 的地址覆盖");
        assert_eq!(c.remark, "keep me");
        assert!(
            app.watch_panel.entries.iter().all(|e| e.name != "B"),
            "缺失变量不得改名任何行"
        );
    }

    #[test]
    fn restore_watch_config_falls_back_gracefully_for_unknown_rows() {
        // Watch 行在工程变量中找不到对应项：保留当前配置，不 panic、不错位
        let mut app = DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1);
        app.channel_panel.add_channel("X".to_string(), egui::Color32::RED, ValueType::Float);
        app.watch_panel.sync_from_channels(
            &["X".to_string()],
            &[ValueType::Float],
            &[0x42],
        );
        app.restore_watch_config(&[project_var("A", 0x100)]);
        let x = app.watch_panel.entries.iter().find(|e| e.name == "X").unwrap();
        assert_eq!(x.address, 0x42, "找不到对应工程变量的行应保留当前配置");
        assert_eq!(x.value_type, ValueType::Float);
    }
}

#[cfg(test)]
mod status_theme_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn status_tooltip_appends_hint_only_when_set() {
        // 通道超限这类 UI 错误不带连接排障建议——「检查 DAP-Link 连接」
        // 对它们毫无意义，无差别拼接会误导排查方向（用户报告）
        let mut app = make_app();
        app.set_error(format!(
            "Only {} channels can be watched at a time. Unselected channel variable: s_uartIrqId[1]",
            MAX_VALUES
        ));
        let tip = app.status_tooltip(app.status_text.as_deref().unwrap());
        assert!(
            !tip.contains("Note:") && !tip.contains("DAP-Link"),
            "UI 类错误不得附带连接排障建议，实际 {tip:?}"
        );
        assert!(!tip.contains('\n'), "无建议时悬停提示就是消息本身: {tip:?}");

        // 连接类错误附带排障建议
        app.set_error_with_hint("SWD initialization failed: timeout", CONNECTION_HINT);
        let tip = app.status_tooltip(app.status_text.as_deref().unwrap());
        assert!(tip.starts_with("SWD initialization failed: timeout"));
        assert!(tip.contains("\n\nNote: Check that the DAP-Link"), "connection errors should carry the hint: {tip:?}");
    }

    #[test]
    fn new_message_clears_previous_hint() {
        let mut app = make_app();
        app.set_error_with_hint("Failed to connect to DAP-Link: no device", CONNECTION_HINT);
        assert!(app.status_hint.is_some());

        // 换成普通状态/普通错误后，旧建议不得残留
        app.set_status("Acquisition stopped");
        assert!(app.status_hint.is_none(), "set_status 应清掉旧建议");
        let tip = app.status_tooltip(app.status_text.as_deref().unwrap());
        assert!(!tip.contains("Note:"));

        app.set_error(format!(
            "Only {} channels can be watched at a time",
            MAX_VALUES
        ));
        assert!(app.status_hint.is_none(), "set_error 应清掉旧建议");
    }

    #[test]
    fn apply_theme_configures_gray_scrollbars() {
        // 滚动条悬停/拖动手柄默认取 widgets.fg_stroke（浅色主题纯黑），
        // 主题改为浮动条 + 半透明灰阶
        let ctx = egui::Context::default();
        apply_theme(&ctx);
        let style = ctx.style();
        let scroll = &style.spacing.scroll;
        assert!(scroll.floating, "应启用浮动滚动条");
        assert_eq!(scroll.bar_width, 8.0, "悬停展开宽度");
        assert_eq!(scroll.floating_width, 3.0, "闲置细条宽度");
        assert!(
            scroll.dormant_handle_opacity > 0.0,
            "闲置时应显示细灰线而不是完全隐藏"
        );
        assert!(
            scroll.interact_handle_opacity < 1.0,
            "悬停/拖动手柄应为半透明灰而非纯黑实心"
        );
        // 浅色主题基础仍然生效
        assert!(!style.visuals.dark_mode);
        assert_eq!(style.visuals.panel_fill, egui::Color32::WHITE);
    }
}

#[cfg(test)]
mod fault_popup_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn every_error_shows_a_popup() {
        // 故障必须可感知：所有 set_error 都弹通用错误弹窗，不能只有底部
        // 一行提示（用户报告：发生故障时视线在波形上根本注意不到）
        let mut app = make_app();
        app.set_error_with_hint("SWD initialization failed: timeout", CONNECTION_HINT);
        let popup = app.warning_popup.as_ref().expect("错误必须弹窗");
        assert_eq!(popup.title, "Error");
        assert!(popup.message.starts_with("SWD initialization failed: timeout"));
        assert!(
            popup.message.contains("Note: Check that the DAP-Link"),
            "连接类错误弹窗应附带排障建议，实际 {:?}",
            popup.message
        );

        // 非连接类错误也弹窗，但不带排障建议
        app.warning_popup = None;
        app.set_error("Failed to export data: io error");
        let popup = app.warning_popup.as_ref().expect("错误必须弹窗");
        assert_eq!(popup.title, "Error");
        assert!(
            !popup.message.contains("Note:"),
            "UI 类错误不得附带连接排障建议，实际 {:?}",
            popup.message
        );
    }

    #[test]
    fn write_skip_message_maps_index_to_channel_name() {
        // 引擎通知只带变量索引；UI 映射为通道名，索引越界时兜底不 panic
        let mut app = make_app();
        app.current_names = vec!["pid.kp".to_string(), "speed".to_string()];
        assert_eq!(
            app.write_skip_message(1, "no sampled old value yet"),
            "Write to 'speed' was skipped: no sampled old value yet"
        );
        assert_eq!(
            app.write_skip_message(7, "8-byte data is not 4-byte aligned (0x20000002)"),
            "Write to 'channel #7' was skipped: 8-byte data is not 4-byte aligned (0x20000002)"
        );
    }
}
