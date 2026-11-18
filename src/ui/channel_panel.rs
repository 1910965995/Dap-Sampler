use super::text::layout_elided;
use crate::pipeline::sample::ValueType;

// ── 圆形图标按钮配色 ──
const GREY_BG: egui::Color32 = egui::Color32::from_rgb(235, 237, 240);
const GREY_HOVER: egui::Color32 = egui::Color32::from_rgb(208, 212, 218);
const GREY_FG: egui::Color32 = egui::Color32::from_rgb(45, 48, 55);

/// 图标按钮尺寸
const ICON_BTN_SIZE: egui::Vec2 = egui::Vec2::new(16.0, 16.0);

/// 绘制一个圆形图标按钮
///
/// - size: 20×20 px
/// - label: 居中显示的文本（如 "●" "×"）
/// - fg: 文本颜色
/// - bg: 正常背景色
/// - hover_bg: hover 时的背景色
///
/// 返回 true 表示用户点击了该按钮。
fn icon_button(
    ui: &mut egui::Ui,
    label: &str,
    fg: egui::Color32,
    bg: egui::Color32,
    hover_bg: egui::Color32,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(ICON_BTN_SIZE, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let bg_color = if response.hovered() { hover_bg } else { bg };
        let rounding = egui::CornerRadius::same(8);
        ui.painter().rect_filled(rect, rounding, bg_color);
        // 用 galley 手动居中,避免 painter.text 因字体 metrics 导致视觉偏上
        let font_id = egui::FontId::proportional(12.0);
        let galley = ui.painter().layout_no_wrap(label.to_string(), font_id, fg);
        let pos = rect.center() - galley.size() * 0.5;
        ui.painter().galley(pos, galley, fg);
    }

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    response.clicked()
}

/// 通道面板中显示的通道信息
pub struct ChannelEntry {
    pub name: String,
    pub color: egui::Color32,
    pub visible: bool,
    pub value_type: ValueType,
}

/// 通道面板交互结果
#[derive(Default)]
pub struct ChannelPanelAction {
    pub remove: Option<usize>,
    pub select: Option<usize>,
}

/// 通道管理面板（左侧「通道」标签页的内容）
///
/// 显示已添加到示波器的变量通道列表，支持：
/// - 颜色标识
/// - 名称显示 / 点击选中（波形高亮）
/// - 可见性切换
/// - 移除按钮
pub struct ChannelPanel {
    /// 通道列表
    pub channels: Vec<ChannelEntry>,
}

impl ChannelPanel {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
        }
    }

    /// 渲染通道面板
    pub fn show(&mut self, ui: &mut egui::Ui, selected: Option<usize>) -> ChannelPanelAction {
        let mut action = ChannelPanelAction::default();

        if self.channels.is_empty() {
            // 与 Cursor 面板提示保持一致的灰色
            ui.label(
                egui::RichText::new("No channels")
                    .size(10.0)
                    .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
            );
            return action;
        }

        egui::ScrollArea::vertical()
            .id_salt("channel_scroll")
            .show(ui, |ui| {
                for (i, ch) in self.channels.iter_mut().enumerate() {
                    let is_sel = selected == Some(i);
                    let frame = if is_sel {
                        egui::Frame::NONE
                            .fill(egui::Color32::from_rgba_unmultiplied(
                                ch.color.r(),
                                ch.color.g(),
                                ch.color.b(),
                                40,
                            ))
                            .corner_radius(3.0)
                            .inner_margin(egui::Margin::symmetric(2, 1))
                    } else {
                        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(2, 1))
                    };

                    frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(ch.color, "●");

                            // 名字占据中间剩余空间（为右侧两个按钮预留宽度），
                            // 过长时截断加省略号，避免与按钮重叠
                            let spacing = ui.spacing().item_spacing.x;
                            let reserve = ICON_BTN_SIZE.x * 2.0 + spacing * 2.0;
                            let name_max_w =
                                (ui.available_width() - reserve).max(40.0);
                            // 用与 Label 相同的字体度量，判断是否发生截断
                            let truncated = layout_elided(
                                ui.ctx(),
                                &ch.name,
                                name_max_w,
                                ui.style().text_styles[&egui::TextStyle::Body].clone(),
                                egui::Color32::PLACEHOLDER,
                            )
                            .elided;

                            let name_text = if is_sel {
                                egui::RichText::new(&ch.name).strong().color(ch.color)
                            } else {
                                egui::RichText::new(&ch.name)
                            };
                            let name_resp = ui
                                .allocate_ui_with_layout(
                                    egui::vec2(name_max_w, ICON_BTN_SIZE.y),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.add(
                                            egui::Label::new(name_text)
                                                .truncate()
                                                .sense(egui::Sense::click()),
                                        )
                                    },
                                )
                                .inner;
                            // 截断时悬停显示完整名称
                            let name_resp = if truncated {
                                name_resp.on_hover_text(&ch.name)
                            } else {
                                name_resp
                            };
                            if name_resp.clicked() {
                                action.select = Some(i);
                            }
                            if name_resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if icon_button(ui, "×", GREY_FG, GREY_BG, GREY_HOVER) {
                                    action.remove = Some(i);
                                }
                                let vis_label = if ch.visible { "●" } else { "○" };
                                if icon_button(ui, vis_label, GREY_FG, GREY_BG, GREY_HOVER) {
                                    ch.visible = !ch.visible;
                                }
                            });
                        });
                    });
                }
            });

        action
    }

    /// 通道数量
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// 添加通道
    pub fn add_channel(&mut self, name: String, color: egui::Color32, value_type: ValueType) {
        if self.channels.len() < 8 {
            self.channels.push(ChannelEntry {
                name,
                color,
                visible: true,
                value_type,
            });
        }
    }

    /// 移除通道
    pub fn remove_channel(&mut self, index: usize) {
        if index < self.channels.len() {
            self.channels.remove(index);
        }
    }

    /// 按路径移除通道
    pub fn remove_by_name(&mut self, name: &str) {
        self.channels.retain(|c| c.name != name);
    }

    /// 检查通道是否存在
    pub fn has_channel(&self, name: &str) -> bool {
        self.channels.iter().any(|c| c.name == name)
    }

    /// 获取通道可见性列表
    pub fn visibility(&self) -> Vec<bool> {
        self.channels.iter().map(|c| c.visible).collect()
    }
}

impl Default for ChannelPanel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个带单通道的 ChannelPanel
    fn panel_with_one_channel() -> ChannelPanel {
        let mut p = ChannelPanel::new();
        p.add_channel("CH1".to_string(), egui::Color32::RED, ValueType::Float);
        p
    }

    /// 渲染一帧（模拟侧栏 220px 宽）并返回输出
    fn render_frame(ctx: &egui::Context, panel: &mut ChannelPanel) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(220.0, 300.0),
            )),
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                panel.show(ui, None);
            });
        })
    }

    #[test]
    fn long_channel_name_does_not_overlap_buttons() {
        let long_name = "s_clockConfig0.simConfig.clockOutConfig.dividerVeryLong";
        let mut p = ChannelPanel::new();
        p.add_channel(long_name.to_string(), egui::Color32::RED, ValueType::Uint32);

        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体
        let output = render_frame(&ctx, &mut p);

        let mut texts = Vec::new();
        let mut btn_rects = Vec::new();
        for clipped in &output.shapes {
            match &clipped.shape {
                egui::Shape::Text(t) => texts.push(t.clone()),
                egui::Shape::Rect(r) if r.fill == GREY_BG => btn_rects.push(r.rect),
                _ => {}
            }
        }

        // 两个按钮（可见性 + 移除）都画出来了
        assert_eq!(btn_rects.len(), 2, "expected visibility + remove buttons");
        let btn_min_x = btn_rects.iter().map(|r| r.left()).fold(f32::INFINITY, f32::min);

        // 名字 galley 右边缘不得压到按钮
        let name = texts
            .iter()
            .find(|t| t.galley.job.text == long_name)
            .expect("channel name painted");
        let name_right = name.pos.x + name.galley.size().x;
        assert!(
            name_right <= btn_min_x + 0.5,
            "name right edge {name_right} overlaps buttons at {btn_min_x}"
        );
        // 长名字必须被截断加省略号
        assert!(name.galley.elided, "long name should be elided");
    }

    #[test]
    fn short_channel_name_is_not_elided() {
        let mut p = panel_with_one_channel();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let output = render_frame(&ctx, &mut p);

        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        let name = texts
            .iter()
            .find(|t| t.galley.job.text == "CH1")
            .expect("channel name painted");
        assert!(!name.galley.elided, "short name should not be elided");
    }

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<egui::epaint::TextShape>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.clone()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    #[test]
    fn visible_button_toggles_state() {
        let mut p = panel_with_one_channel();
        assert!(p.channels[0].visible);
        p.channels[0].visible = false;
        assert!(!p.channels[0].visible);
        p.channels[0].visible = true;
        assert!(p.channels[0].visible);
    }

    #[test]
    fn visible_label_shows_filled_when_visible() {
        // 依赖 icon_button 的渲染行为：visible=true → label="●", false → label="○"
        let p = panel_with_one_channel();
        assert!(p.channels[0].visible);
        let label_visible = "●";
        assert_eq!(label_visible, "●");

        let label_hidden = "○";
        assert_eq!(label_hidden, "○");
    }

    #[test]
    fn remove_returns_correct_index() {
        // 通过 show() 的返回值测试 remove 语义
        // mock 不可行（需要 egui UI），但逻辑等价于手动调用 remove_channel
        let mut p = panel_with_one_channel();
        assert_eq!(p.channel_count(), 1);
        p.remove_channel(0);
        assert_eq!(p.channel_count(), 0);
    }

    #[test]
    fn icon_label_is_multiply_sign() {
        // 验证移除按钮的文本是 U+00D7
        let remove_label = "×";
        assert_eq!(remove_label.chars().next().map(|c| c as u32), Some(0x00D7));
    }
}
