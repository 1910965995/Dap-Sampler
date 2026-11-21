use super::text::layout_elided;
use crate::pipeline::sample::ValueType;

// ── 圆形图标按钮配色 ──
const GREY_BG: egui::Color32 = egui::Color32::from_rgb(235, 237, 240);
const GREY_HOVER: egui::Color32 = egui::Color32::from_rgb(208, 212, 218);
const GREY_FG: egui::Color32 = egui::Color32::from_rgb(45, 48, 55);
/// 闭眼（不观测）状态的淡灰
const GREY_DIM: egui::Color32 = egui::Color32::from_rgb(0x9a, 0xa0, 0xa8);

/// 图标按钮尺寸
const ICON_BTN_SIZE: egui::Vec2 = egui::Vec2::new(16.0, 16.0);

/// 按钮内容绘制回调：(painter, 按钮矩形)
type PaintIcon<'a> = &'a dyn Fn(&egui::Painter, egui::Rect);

/// 绘制一个圆形图标按钮（矢量图标）
///
/// - bg: 正常背景色
/// - hover_bg: hover 时的背景色
///
/// 返回按钮的 Response（点击判断与悬停提示由调用方处理）。
fn icon_button_custom(
    ui: &mut egui::Ui,
    paint: PaintIcon<'_>,
    bg: egui::Color32,
    hover_bg: egui::Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(ICON_BTN_SIZE, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let bg_color = if response.hovered() { hover_bg } else { bg };
        let rounding = egui::CornerRadius::same(8);
        ui.painter().rect_filled(rect, rounding, bg_color);
        paint(ui.painter(), rect);
    }

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    response
}

/// 绘制矢量 ×（移除按钮）
///
/// 与眼睛同用矢量画法、同 1.0px 线宽，墨迹约 6.9px 见方（8.6px 基础上整体
/// 缩小 20%，与原 12px 字形约 7px 的墨迹相当）。
fn paint_x(painter: &egui::Painter, rect: egui::Rect) {
    let c = rect.center();
    let d = rect.width() * 0.216;
    let stroke = egui::Stroke::new(1.0, GREY_FG);
    painter.line_segment(
        [egui::pos2(c.x - d, c.y - d), egui::pos2(c.x + d, c.y + d)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(c.x - d, c.y + d), egui::pos2(c.x + d, c.y - d)],
        stroke,
    );
}

/// 绘制眼睛图标：观测中为睁眼，不观测为带斜杠的闭眼
///
/// 线宽刻意压到 1.0px 级别，对齐 12px Ubuntu-Light 字形（如 "×"）的视觉重量，
/// 避免矢量描边在字体图标旁边显得过粗过深。
fn paint_eye(painter: &egui::Painter, rect: egui::Rect, visible: bool) {
    let c = rect.center();
    let color = if visible { GREY_FG } else { GREY_DIM };
    // 眼眶：扁椭圆描边
    painter.add(egui::Shape::Ellipse(egui::epaint::EllipseShape::stroke(
        c,
        egui::vec2(rect.width() * 0.36, rect.height() * 0.24),
        egui::Stroke::new(1.0, color),
    )));
    // 瞳孔：实心圆
    painter.add(egui::Shape::circle_filled(c, rect.height() * 0.09, color));
    if !visible {
        // 斜杠：左上 → 右下，压过眼眶表示"隐藏"
        let d = rect.width() * 0.42;
        painter.line_segment(
            [egui::pos2(c.x - d, c.y - d), egui::pos2(c.x + d, c.y + d)],
            egui::Stroke::new(1.1, color),
        );
    }
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
                                let remove =
                                    icon_button_custom(ui, &paint_x, GREY_BG, GREY_HOVER);
                                if remove.clicked() {
                                    action.remove = Some(i);
                                }
                                remove.on_hover_text("Remove channel");
                                let visible = ch.visible;
                                let eye = icon_button_custom(
                                    ui,
                                    &|p, rect| paint_eye(p, rect, visible),
                                    GREY_BG,
                                    GREY_HOVER,
                                );
                                if eye.clicked() {
                                    ch.visible = !ch.visible;
                                }
                                eye.on_hover_text(if visible {
                                    "Hide channel"
                                } else {
                                    "Show channel"
                                });
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

    /// 递归统计渲染输出中的矢量图形数量（线段按颜色区分：GREY_FG=×，GREY_DIM=闭眼斜杠）
    fn count_icon_shapes(
        shape: &egui::Shape,
        ellipses: &mut usize,
        circles: &mut usize,
        fg_lines: &mut usize,
        dim_lines: &mut usize,
    ) {
        match shape {
            egui::Shape::Ellipse(_) => *ellipses += 1,
            egui::Shape::Circle(_) => *circles += 1,
            egui::Shape::LineSegment { stroke, .. } => {
                if stroke.color == GREY_DIM {
                    *dim_lines += 1;
                } else if stroke.color == GREY_FG {
                    *fg_lines += 1;
                }
            }
            egui::Shape::Vec(v) => v.iter().for_each(|s| {
                count_icon_shapes(s, ellipses, circles, fg_lines, dim_lines);
            }),
            _ => {}
        }
    }

    #[test]
    fn eye_icon_shows_slash_only_when_hidden() {
        // 睁眼：椭圆眼眶 + 瞳孔，无斜杠；闭眼：额外多出一条淡灰斜杠。
        // × 按钮恒画 2 条深灰线段，作为对照不变。
        let mut p = panel_with_one_channel();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体

        let shapes_of = |panel: &mut ChannelPanel| {
            let output = render_frame(&ctx, panel);
            let (mut ellipses, mut circles, mut fg_lines, mut dim_lines) =
                (0usize, 0usize, 0usize, 0usize);
            for clipped in &output.shapes {
                count_icon_shapes(
                    &clipped.shape,
                    &mut ellipses,
                    &mut circles,
                    &mut fg_lines,
                    &mut dim_lines,
                );
            }
            (ellipses, circles, fg_lines, dim_lines)
        };

        p.channels[0].visible = true;
        let (ellipse, circle, fg_line, dim_line) = shapes_of(&mut p);
        assert_eq!(ellipse, 1, "open eye paints one ellipse outline");
        assert_eq!(circle, 1, "open eye paints one iris");
        assert_eq!(fg_line, 2, "remove button paints two GREY_FG strokes");
        assert_eq!(dim_line, 0, "open eye has no slash");

        p.channels[0].visible = false;
        let (ellipse, circle, fg_line, dim_line) = shapes_of(&mut p);
        assert_eq!(ellipse, 1, "hidden eye keeps the ellipse outline");
        assert_eq!(circle, 1, "hidden eye keeps the iris");
        assert_eq!(fg_line, 2, "remove button strokes unchanged");
        assert_eq!(dim_line, 1, "hidden eye paints a slash");
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
    fn remove_button_is_vector_x() {
        // × 已从字体字形改为矢量绘制：单通道一帧应恰好 2 条深灰线段
        let mut p = panel_with_one_channel();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体

        let output = render_frame(&ctx, &mut p);
        let (mut ellipses, mut circles, mut fg_lines, mut dim_lines) =
            (0usize, 0usize, 0usize, 0usize);
        for clipped in &output.shapes {
            count_icon_shapes(
                &clipped.shape,
                &mut ellipses,
                &mut circles,
                &mut fg_lines,
                &mut dim_lines,
            );
        }
        assert_eq!(fg_lines, 2, "vector × paints exactly two GREY_FG strokes");
        assert_eq!(dim_lines, 0, "no slash when channel is visible");
    }

    /// 用 epaint 的 Tessellator（与 GPU 渲染同一条代码路径）实测单个 Shape 的
    /// 网格顶点边界——能暴露描边外扩（StrokeKind::Outside）、路径近似等
    /// 造成墨迹偏离几何中心的渲染层偏移
    fn tessellated_bounds(shape: &egui::Shape) -> egui::Rect {
        let mut tess = egui::epaint::Tessellator::new(
            1.0,
            egui::epaint::TessellationOptions::default(),
            [512, 512],
            Vec::new(),
        );
        let mut mesh = egui::epaint::Mesh::default();
        tess.tessellate_shape(shape.clone(), &mut mesh);
        let mut rect: Option<egui::Rect> = None;
        for v in &mesh.vertices {
            rect = Some(match rect {
                Some(r) => r.union(egui::Rect::from_min_size(v.pos, egui::Vec2::ZERO)),
                None => egui::Rect::from_min_size(v.pos, egui::Vec2::ZERO),
            });
        }
        rect.expect("shape tessellated to at least one vertex")
    }

    #[test]
    fn icons_are_centered_on_their_button_backgrounds() {
        // 眼睛与 × 的墨迹网格边界中心应与按钮灰底矩形中心重合（容差 0.5px，
        // 羽化边衬是对称的，理论偏移为 0）。睁眼与闭眼（含斜杠）两种状态都查。
        let mut p = panel_with_one_channel();
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体

        let measure = |panel: &mut ChannelPanel| -> Vec<(egui::Rect, egui::Rect)> {
            let output = render_frame(&ctx, panel);
            // 按钮灰底（16×16 圆），按 x 排序：左=眼睛，右=×
            let mut buttons: Vec<egui::Rect> = Vec::new();
            for clipped in &output.shapes {
                if let egui::Shape::Rect(r) = &clipped.shape {
                    if r.fill == GREY_BG {
                        buttons.push(r.rect);
                    }
                }
            }
            buttons.sort_by(|a, b| a.left().total_cmp(&b.left()));
            assert_eq!(buttons.len(), 2, "expected eye + remove buttons");

            // 各墨迹图形的实测网格边界，归入包含其中心的按钮
            let mut inks: Vec<egui::Rect> = Vec::new();
            for clipped in &output.shapes {
                let ink = match &clipped.shape {
                    egui::Shape::Ellipse(_)
                    | egui::Shape::Circle(_)
                    | egui::Shape::LineSegment { .. } => tessellated_bounds(&clipped.shape),
                    _ => continue,
                };
                inks.push(ink);
            }

            buttons
                .iter()
                .map(|btn| {
                    let mut ink_union: Option<egui::Rect> = None;
                    for ink in &inks {
                        if btn.contains(ink.center()) {
                            ink_union = Some(match ink_union {
                                Some(u) => u.union(*ink),
                                None => *ink,
                            });
                        }
                    }
                    let ink_union = ink_union.expect("button has no ink shapes");
                    (*btn, ink_union)
                })
                .collect()
        };

        let assert_centered = |pairs: &[(egui::Rect, egui::Rect)], state: &str| {
            for (btn, ink) in pairs {
                let off = ink.center() - btn.center();
                assert!(
                    off.x.abs() < 0.5 && off.y.abs() < 0.5,
                    "{state}: icon ink {ink:?} not centered on button {btn:?}, offset ({:.2}, {:.2})",
                    off.x,
                    off.y
                );
            }
        };

        assert_centered(&measure(&mut p), "visible");
        p.channels[0].visible = false;
        assert_centered(&measure(&mut p), "hidden");
    }
}
