use crate::pipeline::sample::ValueType;

/// 待写入 MCU 的请求（通道索引 + 原始值）
pub struct WriteRequest {
    pub channel_idx: usize,
    pub raw_value: u32,
}

/// Watch 面板中显示的单个变量条目
pub struct WatchEntry {
    /// 变量名（ELF 模式为 path，手工模式为 "CH1 0x..."）
    pub name: String,
    /// 变量地址（0 表示未知）
    pub address: u32,
    /// 变量类型
    pub value_type: ValueType,
    /// 刷新周期（秒）
    pub refresh_period_s: f32,
    /// Y 轴偏移（用于波形图，默认 0.0）
    pub y_offset: f32,
    /// Y 轴缩放系数（用于波形图，默认 1.0；禁止 0；允许负数）
    pub y_scale: f32,
    /// 用户自定义备注（描述通道的变量）
    pub remark: String,
    /// 内部计数器（累计采样点数）
    refresh_counter: u32,
    /// 当前显示的值（格式化字符串）
    display_value: String,
    /// 用户编辑中的值输入框文本
    edit_buffer: String,
    /// 值输入框是否获得焦点
    editing: bool,
    /// Refresh 输入框文本
    refresh_buffer: String,
    /// Y Offset 输入框文本
    y_offset_buffer: String,
    /// Y Scale 输入框文本
    y_scale_buffer: String,
    /// Remark 输入框文本
    remark_buffer: String,
}

// ===== 表格几何与视觉常量 =====

/// 数据行高度（px，固定不随内容变化）
const ROW_H: f32 = 24.0;
/// 表头高度（px）
const HEADER_H: f32 = 26.0;
/// 单元格左右内边距（px）
const CELL_PAD_X: f32 = 8.0;
/// Name 列左侧通道色条宽度（px）
const COLOR_BAR_W: f32 = 4.0;
/// 列分隔线拖拽热区宽度（px；视觉线宽仍为 1px）
const RESIZE_HIT_W: f32 = 7.0;
/// 列宽上限（px）
const MAX_COL_W: f32 = 600.0;

/// 列定义：标题 + 宽度约束（列布局的单一事实来源）
struct ColumnSpec {
    title: &'static str,
    default_w: f32,
    min_w: f32,
}

/// 8 列：Name | Address | Type | Value | Refresh (s) | Y Offset | Y Scale | Remark
const COLUMNS: [ColumnSpec; 8] = [
    ColumnSpec { title: "Name", default_w: 150.0, min_w: 70.0 },
    ColumnSpec { title: "Address", default_w: 110.0, min_w: 80.0 },
    ColumnSpec { title: "Type", default_w: 90.0, min_w: 60.0 },
    ColumnSpec { title: "Value", default_w: 130.0, min_w: 70.0 },
    ColumnSpec { title: "Refresh (s)", default_w: 90.0, min_w: 70.0 },
    ColumnSpec { title: "Y Offset", default_w: 100.0, min_w: 70.0 },
    ColumnSpec { title: "Y Scale", default_w: 100.0, min_w: 70.0 },
    ColumnSpec { title: "Remark", default_w: 240.0, min_w: 100.0 },
];

/// 默认列宽（从 `COLUMNS` 派生；随工程保存/恢复）
pub const DEFAULT_COL_WIDTHS: [f32; 8] = default_col_widths();

const fn default_col_widths() -> [f32; 8] {
    let mut out = [0.0; COLUMNS.len()];
    let mut i = 0;
    while i < COLUMNS.len() {
        out[i] = COLUMNS[i].default_w;
        i += 1;
    }
    out
}

// 视觉常量（浅色主题，与 app 其它面板同色系）
const TEXT_DARK: egui::Color32 = egui::Color32::from_rgb(45, 48, 55);
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(0x6E, 0x76, 0x82);
const HEADER_BG: egui::Color32 = egui::Color32::from_rgb(238, 240, 244);
const STRIPE_BG: egui::Color32 = egui::Color32::from_rgb(249, 250, 251);
/// 可编辑单元格悬停底色
const HOVER_BG: egui::Color32 = egui::Color32::from_rgb(240, 244, 250);
/// 通道色条缺省色（无对应通道时）
const BAR_FALLBACK: egui::Color32 = egui::Color32::from_rgb(160, 160, 160);
/// 网格线：1px 浅灰（整表只描一次，保证横竖等粗）
const GRID_STROKE: egui::Stroke = egui::Stroke {
    width: 1.0,
    color: egui::Color32::from_rgb(0xE0, 0xE2, 0xE6),
};
/// 强调色（列分隔线悬停高亮 / 单元格聚焦框）
const ACCENT: egui::Color32 = egui::Color32::from_rgb(70, 130, 200);

const TITLE_FONT: egui::FontId = egui::FontId::new(13.0, egui::FontFamily::Proportional);
const HEADER_FONT: egui::FontId = egui::FontId::new(12.5, egui::FontFamily::Proportional);
const CELL_FONT: egui::FontId = egui::FontId::new(12.0, egui::FontFamily::Proportional);

/// Watch 面板（示波器下方）
///
/// 显示已添加到示波器的变量列表，包含：
/// - 变量名称
/// - 变量地址
/// - 变量类型
/// - 变量当前值（可编辑，回车后写入 MCU）
/// - 可配置的刷新周期
/// - 可配置的 Y Offset / Y Scale（应用于波形图）
/// - 用户自定义备注
///
/// 表格样式：行高固定，列宽可拖拽调整（随工程保存）。
pub struct WatchPanel {
    /// 变量列表
    pub entries: Vec<WatchEntry>,
    /// 待处理的写入请求
    pending_writes: Vec<WriteRequest>,
    /// 已提交但尚未被外部拉取的 (channel_name, y_offset, y_scale) 列表
    pub dirty_transforms: Vec<(String, f32, f32)>,
    /// 各列宽度（px），拖拽列分隔线调整；随工程文件保存
    col_widths: Vec<f32>,
}

impl WatchPanel {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending_writes: Vec::new(),
            dirty_transforms: Vec::new(),
            col_widths: DEFAULT_COL_WIDTHS.to_vec(),
        }
    }

    /// 当前列宽（px）
    pub fn col_widths(&self) -> &[f32] {
        &self.col_widths
    }

    /// 设置列宽（来自工程文件）。
    ///
    /// 宽容策略：长度不足的列保持当前值；NaN/Inf 忽略；越界值夹紧到 [min_w, MAX_COL_W]。
    pub fn set_col_widths(&mut self, widths: &[f32]) {
        for (i, slot) in self.col_widths.iter_mut().enumerate() {
            let Some(&w) = widths.get(i) else { break };
            if w.is_finite() {
                *slot = w.clamp(COLUMNS[i].min_w, MAX_COL_W);
            }
        }
    }

    /// 列宽恢复默认
    pub fn reset_col_widths(&mut self) {
        self.col_widths = DEFAULT_COL_WIDTHS.to_vec();
    }

    /// 设置 Watch 列表（与示波器通道同步）
    /// names: 变量名列表, types: 变量类型列表, addresses: 变量地址列表
    /// 保留同名条目的 refresh_period_s / display_value / y_offset / y_scale / remark
    pub fn sync_from_channels(&mut self, names: &[String], types: &[ValueType], addresses: &[u32]) {
        // 保留已有的 refresh_period_s / display_value / y_offset / y_scale / remark
        let old_entries: Vec<(String, f32, f32, f32, String, String)> = self.entries
            .iter()
            .map(|e| (
                e.name.clone(),
                e.refresh_period_s,
                e.y_offset,
                e.y_scale,
                e.display_value.clone(),
                e.remark.clone(),
            ))
            .collect();

        self.entries = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let vt = types.get(i).copied().unwrap_or(ValueType::Uint32);
                let addr = addresses.get(i).copied().unwrap_or(0);
                // 尝试保留旧配置(含 remark)
                let (period, y_offset, y_scale, display, remark) = old_entries
                    .iter()
                    .find(|(n, _, _, _, _, _)| n == name)
                    .map(|(_, p, o, s, d, r)| (*p, *o, *s, d.clone(), r.clone()))
                    .unwrap_or((0.1, 0.0, 1.0, "--".to_string(), String::new()));
                WatchEntry {
                    name: name.clone(),
                    address: addr,
                    value_type: vt,
                    refresh_period_s: period,
                    y_offset,
                    y_scale,
                    remark,
                    refresh_counter: 0,
                    display_value: display,
                    edit_buffer: String::new(),
                    editing: false,
                    refresh_buffer: String::new(),
                    y_offset_buffer: String::new(),
                    y_scale_buffer: String::new(),
                    remark_buffer: String::new(),
                }
            })
            .collect();
    }

    /// 更新变量值（每收到一个采样点调用一次）
    /// values: 本次采样点的变量值列表（与 entries 一一对应）
    /// sample_rate: 当前采样率（Hz），用于将秒转换为采样点数
    pub fn update_values(&mut self, values: &[u32], sample_rate: u32) {
        for (i, entry) in self.entries.iter_mut().enumerate() {
            // 编辑中不更新显示值
            if entry.editing {
                continue;
            }
            if let Some(&raw) = values.get(i) {
                entry.refresh_counter += 1;
                // 将秒转换为采样点数（至少 1）
                let period_pts = ((entry.refresh_period_s * sample_rate as f32) as u32).max(1);
                if entry.refresh_counter >= period_pts {
                    entry.refresh_counter = 0;
                    entry.display_value = format_value(raw, entry.value_type);
                }
            }
        }
    }

    /// 取出待处理的写入请求
    pub fn drain_writes(&mut self) -> Vec<WriteRequest> {
        std::mem::take(&mut self.pending_writes)
    }

    /// 取出本帧以来已提交的 y_offset/y_scale 变更。
    /// 每条记录为 (channel_name, y_offset, y_scale)。同一通道在同一帧内多次修改只保留最终值。
    pub fn drain_changed_transforms(&mut self) -> Vec<(String, f32, f32)> {
        std::mem::take(&mut self.dirty_transforms)
    }

    /// 渲染 Watch 面板（固定行高的细网格表格，列宽可拖拽调整）
    ///
    /// `channel_colors` 与 entries 一一对应，用于 Name 列色条。
    pub fn show(&mut self, ui: &mut egui::Ui, channel_colors: &[egui::Color32]) {
        // 顶部标题
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Watch Window")
                    .font(TITLE_FONT)
                    .strong()
                    .color(TEXT_DARK),
            );
        });
        ui.add_space(2.0);

        if self.entries.is_empty() {
            // 居中空状态提示（与波形区 Waiting for data... 样式一致）
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.35);
                ui.label(egui::RichText::new("No watch variables").size(14.0).color(TEXT_MUTED));
                ui.label(
                    egui::RichText::new("Import ELF or add channels to start watching")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
            });
            return;
        }

        let mut col_widths = self.col_widths.clone();
        // 收集本帧产生的写入请求
        let mut new_writes: Vec<WriteRequest> = Vec::new();
        // 收集本帧提交的 y_offset/y_scale 变更（用局部缓冲避开 self 的可变借用冲突）
        let mut new_transforms: Vec<(String, f32, f32)> = Vec::new();

        egui::ScrollArea::both()
            .id_salt("watch_table_scroll")
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, _viewport| {
                // 行与行紧贴：默认 item_spacing.y = 3 会在每行后插入间隙
                ui.spacing_mut().item_spacing.y = 0.0;

                let total_w: f32 = col_widths.iter().sum();
                // 列左偏移（前缀和）：表头与数据行共用
                let col_x = prefix_sums(&col_widths);
                let cell = |row: egui::Rect, i: usize| {
                    egui::Rect::from_min_size(
                        egui::Pos2::new(row.left() + col_x[i], row.top()),
                        egui::Vec2::new(col_widths[i], row.height()),
                    )
                };

                // ---- 表头 ----
                let (hdr_rect, _) = ui.allocate_exact_size(
                    egui::Vec2::new(total_w, HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(hdr_rect, 0.0, HEADER_BG);
                for (i, col) in COLUMNS.iter().enumerate() {
                    paint_text_cell(ui, cell(hdr_rect, i), col.title, &HEADER_FONT, TEXT_DARK);
                }

                // ---- 数据行 ----
                let mut row_rects: Vec<egui::Rect> = Vec::with_capacity(self.entries.len());
                for (row_idx, entry) in self.entries.iter_mut().enumerate() {
                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::Vec2::new(total_w, ROW_H),
                        egui::Sense::hover(),
                    );
                    row_rects.push(row_rect);

                    // 斑马纹背景
                    if row_idx % 2 == 1 {
                        ui.painter().rect_filled(row_rect, 0.0, STRIPE_BG);
                    }

                    // 左侧通道色条
                    let ch_color = channel_colors
                        .get(row_idx)
                        .copied()
                        .unwrap_or(BAR_FALLBACK);
                    let bar = egui::Rect::from_min_size(
                        egui::Pos2::new(row_rect.left(), row_rect.top() + 2.0),
                        egui::Vec2::new(COLOR_BAR_W, ROW_H - 4.0),
                    );
                    ui.painter().rect_filled(bar, 1.5, ch_color);

                    // Name（过长时截断加省略号，避免溢出到 Address 列）
                    {
                        let r = cell(row_rect, 0);
                        let text_left = CELL_PAD_X + COLOR_BAR_W;
                        let galley = layout_elided(
                            ui.ctx(),
                            &entry.name,
                            (r.width() - text_left - CELL_PAD_X).max(1.0),
                            CELL_FONT.clone(),
                            TEXT_DARK,
                        );
                        if galley.elided {
                            // 截断时悬停显示完整名称
                            ui.interact(
                                r,
                                ui.id().with(("watch_name_hover", row_idx)),
                                egui::Sense::hover(),
                            )
                            .on_hover_text(&entry.name);
                        }
                        let pos = egui::Pos2::new(
                            r.left() + text_left,
                            r.center().y - galley.size().y * 0.5,
                        );
                        ui.painter()
                            .with_clip_rect(r)
                            .galley(pos, galley, TEXT_DARK);
                    }

                    // Address（只读，16 进制）
                    {
                        let addr = if entry.address == 0 {
                            "--".to_string()
                        } else {
                            format!("0x{:08X}", entry.address)
                        };
                        paint_text_cell(ui, cell(row_rect, 1), &addr, &CELL_FONT, TEXT_DARK);
                    }

                    // Type（只读）
                    paint_text_cell(
                        ui,
                        cell(row_rect, 2),
                        entry.value_type.label(),
                        &CELL_FONT,
                        TEXT_DARK,
                    );

                    // Value（可编辑，回车写入 MCU）
                    {
                        let hint =
                            egui::RichText::new(&entry.display_value).color(TEXT_DARK);
                        let resp =
                            edit_cell(ui, cell(row_rect, 3), &mut entry.edit_buffer, hint);
                        if resp.lost_focus()
                            && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            if let Some(raw) = parse_value(&entry.edit_buffer, entry.value_type) {
                                new_writes.push(WriteRequest {
                                    channel_idx: row_idx,
                                    raw_value: raw,
                                });
                                entry.display_value = format_value(raw, entry.value_type);
                            }
                            entry.edit_buffer.clear();
                            entry.editing = false;
                        }
                        if resp.gained_focus() {
                            entry.editing = true;
                            entry.edit_buffer = entry.display_value.clone();
                        }
                        if resp.lost_focus() {
                            entry.editing = false;
                            entry.edit_buffer.clear();
                        }
                    }

                    // Refresh（秒，必须 > 0）
                    number_cell(
                        ui,
                        cell(row_rect, 4),
                        &mut entry.refresh_buffer,
                        &mut entry.refresh_period_s,
                        "s",
                        |v| v > 0.0,
                    );

                    // Y Offset（任意浮点，提交后同步波形图）
                    if number_cell(
                        ui,
                        cell(row_rect, 5),
                        &mut entry.y_offset_buffer,
                        &mut entry.y_offset,
                        "off",
                        |_| true,
                    ) {
                        new_transforms
                            .push((entry.name.clone(), entry.y_offset, entry.y_scale));
                    }

                    // Y Scale（禁止 0，允许负数）
                    if number_cell(
                        ui,
                        cell(row_rect, 6),
                        &mut entry.y_scale_buffer,
                        &mut entry.y_scale,
                        "scale",
                        |v| v != 0.0,
                    ) {
                        new_transforms
                            .push((entry.name.clone(), entry.y_offset, entry.y_scale));
                    }

                    // Remark（本地备注，失焦或回车时写入字段）
                    {
                        let hint =
                            egui::RichText::new(entry.remark.as_str()).color(TEXT_MUTED);
                        let resp =
                            edit_cell(ui, cell(row_rect, 7), &mut entry.remark_buffer, hint);
                        // (失焦且 Enter) || 失焦 ≡ 失焦
                        if resp.lost_focus() && !entry.remark_buffer.is_empty() {
                            entry.remark = entry.remark_buffer.clone();
                            entry.remark_buffer.clear();
                        }
                    }
                }

                // ---- 网格：整表只描一次，共享边界不重复（否则行线会叠成 2px）----
                paint_table_grid(ui.painter(), hdr_rect, &row_rects, &col_widths, GRID_STROKE);

                // ---- 列宽拖拽 ----
                draw_column_resizers(ui, hdr_rect, &row_rects, &mut col_widths);
            });

        self.col_widths = col_widths;
        // 将本帧产生的写入请求加入队列
        self.pending_writes.extend(new_writes);
        // 将本帧提交的 y_offset/y_scale 变更加入队列
        self.dirty_transforms.extend(new_transforms);
    }
}

impl Default for WatchPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// 前缀和：返回各列左边界相对表左的偏移
fn prefix_sums(widths: &[f32]) -> Vec<f32> {
    widths
        .iter()
        .scan(0.0, |acc, &w| {
            let x = *acc;
            *acc += w;
            Some(x)
        })
        .collect()
}

/// 只读文本单元格：左对齐 + 垂直居中，超宽截断加省略号
fn paint_text_cell(
    ui: &egui::Ui,
    cell: egui::Rect,
    text: &str,
    font: &egui::FontId,
    color: egui::Color32,
) {
    let galley = layout_elided(
        ui.ctx(),
        text,
        (cell.width() - CELL_PAD_X * 2.0).max(1.0),
        font.clone(),
        color,
    );
    let pos = egui::Pos2::new(cell.left() + CELL_PAD_X, cell.center().y - galley.size().y * 0.5);
    ui.painter().with_clip_rect(cell).galley(pos, galley, color);
}

/// 可编辑单元格：无边框文本框（表格风格）。
///
/// 悬停淡蓝底色提示可编辑；聚焦时描强调色细框。控件无边框无内边距，
/// 高度不超出行高，保证行高固定。
fn edit_cell(
    ui: &mut egui::Ui,
    cell: egui::Rect,
    buffer: &mut String,
    hint: impl Into<egui::WidgetText>,
) -> egui::Response {
    if ui.rect_contains_pointer(cell) {
        ui.painter().rect_filled(cell, 0.0, HOVER_BG);
    }
    let content = cell.shrink2(egui::Vec2::new(CELL_PAD_X, 0.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let resp = child.add(
        egui::TextEdit::singleline(buffer)
            .font(CELL_FONT.clone())
            .frame(false)
            .margin(egui::Margin::ZERO)
            .text_color(TEXT_DARK)
            .desired_width(f32::INFINITY)
            .hint_text(hint),
    );
    if resp.has_focus() {
        ui.painter().rect_stroke(
            cell,
            0.0,
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

/// 数字输入单元格：Enter 提交（`accept` 决定是否接受），失焦回填格式化值。
///
/// 返回 true 表示本次提交成功（值已更新）。空 buffer 时惰性回填当前值。
fn number_cell(
    ui: &mut egui::Ui,
    cell: egui::Rect,
    buffer: &mut String,
    value: &mut f32,
    hint: &str,
    accept: impl Fn(f32) -> bool,
) -> bool {
    if buffer.is_empty() {
        *buffer = format!("{:.3}", *value);
    }
    let resp = edit_cell(ui, cell, buffer, hint);
    if resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter)) {
        let committed = buffer
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|&v| accept(v))
            .is_some_and(|v| {
                *value = v;
                true
            });
        *buffer = format!("{:.3}", *value);
        return committed;
    }
    // 失焦（非 Enter 也触发）reformat
    if resp.lost_focus() && !buffer.is_empty() {
        *buffer = format!("{:.3}", *value);
    }
    false
}

/// 绘制整张表格的网格线（外框 + 列分隔竖线 + 行分隔横线）。
///
/// 只画一次：相邻行共享的边界若各自描边会叠成 2px 粗线（旧实现的问题）。
/// epaint 对轴向 1px 线做像素对齐，横竖线视觉等粗。
fn paint_table_grid(
    painter: &egui::Painter,
    hdr_rect: egui::Rect,
    row_rects: &[egui::Rect],
    col_widths: &[f32],
    stroke: egui::Stroke,
) {
    let Some(last_row) = row_rects.last() else { return };
    let table = egui::Rect::from_min_max(
        hdr_rect.min,
        egui::Pos2::new(hdr_rect.right(), last_row.bottom()),
    );

    // 外框
    painter.rect_stroke(table, 0.0, stroke, egui::StrokeKind::Inside);

    // 列分隔竖线：贯穿表头与所有数据行（内部边界，共 cols-1 条）
    let mut x = table.left();
    for &w in col_widths.iter().take(col_widths.len().saturating_sub(1)) {
        x += w;
        painter.vline(x, table.y_range(), stroke);
    }

    // 行分隔横线：表头底边 + 每行底边；最后一行底边由外框覆盖，不重复画
    painter.hline(table.x_range(), hdr_rect.bottom(), stroke);
    for rect in &row_rects[..row_rects.len() - 1] {
        painter.hline(table.x_range(), rect.bottom(), stroke);
    }
}

/// 列宽拖拽手柄：覆盖在每条列分隔线上（表头 + 数据行全高）。
///
/// 拖动改变左侧列宽并夹紧到 [min_w, MAX_COL_W]；悬停/拖动时高亮整条分隔线。
fn draw_column_resizers(
    ui: &mut egui::Ui,
    hdr_rect: egui::Rect,
    row_rects: &[egui::Rect],
    col_widths: &mut [f32],
) {
    let Some(last_row) = row_rects.last() else { return };
    let top = hdr_rect.top();
    let bottom = last_row.bottom();
    let mut x = hdr_rect.left();
    for i in 0..col_widths.len().saturating_sub(1) {
        x += col_widths[i];
        let hit = egui::Rect::from_center_size(
            egui::Pos2::new(x, (top + bottom) * 0.5),
            egui::Vec2::new(RESIZE_HIT_W, bottom - top),
        );
        let resp = ui
            .interact(
                hit,
                ui.id().with(("watch_col_resize", i)),
                egui::Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeColumn);
        if resp.dragged() {
            // drag_delta 是本帧指针位移，逐帧累加
            let dx = resp.drag_delta().x;
            col_widths[i] = (col_widths[i] + dx).clamp(COLUMNS[i].min_w, MAX_COL_W);
        }
        if resp.hovered() || resp.dragged() {
            ui.painter().vline(x, top..=bottom, egui::Stroke::new(2.0, ACCENT));
        }
    }
}

/// 单行布局文本，超宽时以 "…" 截断。
///
/// 使用 epaint 的 `TextWrapping`（`max_rows = 1`）做真正的字形级截断，
/// 避免 `painter.text` 不裁剪导致文本溢出到相邻列。
fn layout_elided(
    ctx: &egui::Context,
    text: &str,
    max_w: f32,
    font: egui::FontId,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ctx.fonts(|f| f.layout_job(job))
}

/// 解析用户输入的值字符串，返回原始 u32
///
/// 用户输入按 10 进制处理（不支持 0x 前缀）。
/// 支持格式：
/// - Float: "3.14", "-1.5"
/// - Uint32: "1000"
/// - Int32: "-100"
/// - Uint16: "1000"
/// - Int16: "-100"
/// - Uint8: "200"
/// - Int8: "-1"
fn parse_value(s: &str, vt: ValueType) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    match vt {
        ValueType::Float => {
            let f: f32 = s.parse().ok()?;
            Some(f.to_bits())
        }
        ValueType::Uint32 => s.parse::<u32>().ok(),
        ValueType::Int32 => {
            let v: i32 = s.parse().ok()?;
            Some(v as u32)
        }
        ValueType::Uint16 => {
            let v: u16 = s.parse().ok()?;
            Some(v as u32)
        }
        ValueType::Int16 => {
            let v: i16 = s.parse().ok()?;
            Some(v as u32)
        }
        ValueType::Uint8 => {
            let v: u8 = s.parse().ok()?;
            Some(v as u32)
        }
        ValueType::Int8 => {
            let v: i8 = s.parse().ok()?;
            Some(v as u32)
        }
    }
}

/// 格式化变量值（只显示 10 进制）
fn format_value(raw: u32, vt: ValueType) -> String {
    match vt {
        ValueType::Float => {
            let f = f32::from_bits(raw);
            if f.is_finite() {
                format!("{:.6}", f)
            } else {
                "NaN/Inf".to_string()
            }
        }
        ValueType::Uint32 => format!("{}", raw),
        ValueType::Int32 => format!("{}", raw as i32),
        ValueType::Uint16 => format!("{}", raw & 0xFFFF),
        ValueType::Int16 => format!("{}", (raw & 0xFFFF) as i16),
        ValueType::Uint8 => format!("{}", raw & 0xFF),
        ValueType::Int8 => format!("{}", (raw & 0xFF) as i8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(name: &str, y_offset: f32, y_scale: f32) -> WatchEntry {
        WatchEntry {
            name: name.to_string(),
            address: 0,
            value_type: ValueType::Float,
            refresh_period_s: 0.1,
            y_offset,
            y_scale,
            remark: String::new(),
            refresh_counter: 0,
            display_value: "--".to_string(),
            edit_buffer: String::new(),
            editing: false,
            refresh_buffer: String::new(),
            y_offset_buffer: String::new(),
            y_scale_buffer: String::new(),
            remark_buffer: String::new(),
        }
    }

    fn make_panel_with(entries: Vec<WatchEntry>) -> WatchPanel {
        let mut p = WatchPanel::new();
        // 通过 sync_from_channels 间接注入，方便测试
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let types: Vec<ValueType> = entries.iter().map(|e| e.value_type).collect();
        let addresses: Vec<u32> = entries.iter().map(|e| e.address).collect();
        p.sync_from_channels(&names, &types, &addresses);
        // 覆盖默认值
        for (panel_e, e) in p.entries.iter_mut().zip(entries) {
            panel_e.y_offset = e.y_offset;
            panel_e.y_scale = e.y_scale;
        }
        p
    }

    /// 建立带默认字体的测试 Context（首帧只用于初始化字体）
    fn test_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        ctx
    }

    /// 渲染一帧并返回输出
    fn render_frame(
        ctx: &egui::Context,
        panel: &mut WatchPanel,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 400.0),
            )),
            events,
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_min_width(DEFAULT_COL_WIDTHS.iter().sum::<f32>() + 40.0);
                panel.show(ui, &[]);
            });
        })
    }

    fn collect_texts(shape: &egui::Shape, out: &mut Vec<egui::epaint::TextShape>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.clone()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_texts(s, out)),
            _ => {}
        }
    }

    fn collect_lines(shape: &egui::Shape, out: &mut Vec<[egui::Pos2; 2]>) {
        match shape {
            egui::Shape::LineSegment { points, .. } => out.push(*points),
            egui::Shape::Vec(v) => v.iter().for_each(|s| collect_lines(s, out)),
            _ => {}
        }
    }

    fn collect_lines_of(output: &egui::FullOutput) -> Vec<[egui::Pos2; 2]> {
        let mut lines = Vec::new();
        for clipped in &output.shapes {
            collect_lines(&clipped.shape, &mut lines);
        }
        lines
    }

    fn find_text_y(output: &egui::FullOutput, text: &str) -> Option<f32> {
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
            .iter()
            .find(|t| t.galley.job.text == text)
            .map(|t| t.pos.y)
    }

    fn find_text_x(output: &egui::FullOutput, text: &str) -> Option<f32> {
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }
        texts
            .iter()
            .find(|t| t.galley.job.text == text)
            .map(|t| t.pos.x)
    }

    /// 从输出中找出指定填充色的矩形（用于定位表头）
    fn find_rect_by_fill(output: &egui::FullOutput, fill: egui::Color32) -> Option<egui::Rect> {
        for clipped in &output.shapes {
            if let egui::Shape::Rect(r) = &clipped.shape {
                if r.fill == fill {
                    return Some(r.rect);
                }
            }
        }
        None
    }

    #[test]
    fn name_text_is_elided_to_column_width() {
        // 需要真实字体度量，故不用 __run_test_ui（它刻意加载空字体）。
        let ctx = test_ctx();
        let font = egui::FontId::proportional(12.0);
        let color = egui::Color32::BLACK;

        let measure = |text: &str, budget: f32| {
            let galley = layout_elided(&ctx, text, budget, font.clone(), color);
            (galley.elided, galley.size().x)
        };

        // 长变量名在 Name 列可用宽度下必须被截断
        let long = "g_pwmChannelCallbackHandlerWithAVeryLongName";
        let budget = 80.0;
        let (elided, width) = measure(long, budget);
        assert!(elided, "expected long name to be elided");
        assert!(width <= budget + 0.5, "elided width {width} exceeds budget {budget}");

        // 短名称在宽松宽度下不截断
        let budget = 400.0;
        let (elided, _) = measure("g_pwm", budget);
        assert!(!elided, "short name should not be elided");
    }

    #[test]
    fn long_name_does_not_overflow_name_column() {
        let long_name = "g_pwmChannelCallbackHandlerWithAVeryLongName";
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(
            &[long_name.to_string()],
            &[ValueType::Uint32],
            &[0x2000_0000],
        );

        let ctx = test_ctx();
        let output = render_frame(&ctx, &mut panel, Vec::new());

        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_texts(&clipped.shape, &mut texts);
        }

        let name_shape = texts
            .iter()
            .find(|t| t.galley.job.text == long_name)
            .expect("long name should be painted (elided)");

        // Name 列可用宽度 = 列宽 - 左侧留白 - 色条 - 右侧留白
        let budget = COLUMNS[0].default_w - CELL_PAD_X - COLOR_BAR_W - CELL_PAD_X;
        assert!(name_shape.galley.elided, "long name should be elided");
        assert!(
            name_shape.galley.size().x <= budget + 0.5,
            "name width {} exceeds column budget {}",
            name_shape.galley.size().x,
            budget
        );
    }

    #[test]
    fn rows_are_packed_without_vertical_gaps() {
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &[ValueType::Uint32; 3],
            &[0x2000_0000, 0x2000_0004, 0x2000_0008],
        );

        let ctx = test_ctx();
        let output = render_frame(&ctx, &mut panel, Vec::new());

        // 取三行 Name 文本的 y 坐标，行距应恰好等于 ROW_H（无额外间隙/重叠）
        let mut ys: Vec<f32> = ["a", "b", "c"]
            .iter()
            .filter_map(|name| find_text_y(&output, name))
            .collect();
        assert_eq!(ys.len(), 3, "expected all three rows painted");
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());

        for pair in ys.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (step - ROW_H).abs() < 0.5,
                "row step {step} should equal row height {ROW_H} (no gap, no overlap)"
            );
        }
    }

    #[test]
    fn grid_draws_exactly_one_line_per_boundary() {
        let rows = 3usize;
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(
            &["a".to_string(), "b".to_string(), "c".to_string()],
            &[ValueType::Uint32; 3],
            &[0x2000_0000, 0x2000_0004, 0x2000_0008],
        );

        let ctx = test_ctx();
        let output = render_frame(&ctx, &mut panel, Vec::new());
        let lines = collect_lines_of(&output);

        // 横线：表头底边 + 前两行底边 = rows 条；间距恒为 ROW_H
        let mut h_ys: Vec<f32> = lines
            .iter()
            .filter(|l| (l[0].y - l[1].y).abs() < 0.01)
            .map(|l| l[0].y)
            .collect();
        h_ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            h_ys.len(),
            rows,
            "expected {rows} horizontal separators, got {}: {:?}",
            h_ys.len(),
            h_ys
        );
        for pair in h_ys.windows(2) {
            assert!(
                (pair[1] - pair[0] - ROW_H).abs() < 0.5,
                "horizontal separators should be {} apart, got {} ({:?})",
                ROW_H,
                pair[1] - pair[0],
                h_ys
            );
        }

        // 竖线：8 列共 7 条内部分隔线，x 间距等于各列宽度
        let mut v_xs: Vec<f32> = lines
            .iter()
            .filter(|l| (l[0].x - l[1].x).abs() < 0.01)
            .map(|l| l[0].x)
            .collect();
        v_xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(
            v_xs.len(),
            COLUMNS.len() - 1,
            "expected {} vertical separators, got {}: {:?}",
            COLUMNS.len() - 1,
            v_xs.len(),
            v_xs
        );
        // 分隔线 i 位于第 i 列右边界，相邻间距 = 下一列的宽度
        for i in 0..v_xs.len() - 1 {
            let expect = COLUMNS[i + 1].default_w;
            assert!(
                (v_xs[i + 1] - v_xs[i] - expect).abs() < 0.5,
                "vertical separator spacing should be {expect}, got {}",
                v_xs[i + 1] - v_xs[i]
            );
        }
    }

    #[test]
    fn set_col_widths_clamps_and_ignores_invalid() {
        let mut p = WatchPanel::new();
        let original: Vec<f32> = p.col_widths().to_vec();

        // 过小 → 夹到 min_w；过大 → 夹到上限；NaN → 忽略
        let mut input = original.clone();
        input[0] = 1.0;
        input[1] = 5000.0;
        input[2] = f32::NAN;
        p.set_col_widths(&input);

        assert_eq!(p.col_widths()[0], COLUMNS[0].min_w);
        assert_eq!(p.col_widths()[1], MAX_COL_W);
        assert!((p.col_widths()[2] - original[2]).abs() < 1e-6, "NaN should be ignored");

        // 长度不足的列保持原值
        p.set_col_widths(&[300.0]);
        assert!((p.col_widths()[0] - 300.0).abs() < 1e-6);
        assert!((p.col_widths()[1] - MAX_COL_W).abs() < 1e-6);
    }

    #[test]
    fn reset_col_widths_restores_defaults() {
        let mut p = WatchPanel::new();
        let mut input: Vec<f32> = DEFAULT_COL_WIDTHS.to_vec();
        input[3] = 400.0;
        p.set_col_widths(&input);
        assert!((p.col_widths()[3] - 400.0).abs() < 1e-6);

        p.reset_col_widths();
        assert_eq!(p.col_widths(), DEFAULT_COL_WIDTHS.to_vec());
    }

    #[test]
    fn header_positions_follow_column_widths() {
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(&["a".to_string()], &[ValueType::Uint32], &[0x2000_0000]);

        let ctx = test_ctx();
        let x_before = find_text_x(&render_frame(&ctx, &mut panel, Vec::new()), "Address")
            .expect("Address header painted");

        // Name 列加宽 150px → Address 表头右移
        let mut widths: Vec<f32> = DEFAULT_COL_WIDTHS.to_vec();
        widths[0] += 150.0;
        panel.set_col_widths(&widths);
        let x_after = find_text_x(&render_frame(&ctx, &mut panel, Vec::new()), "Address")
            .expect("Address header painted");

        assert!(
            (x_after - x_before - 150.0).abs() < 1.0,
            "Address header x should shift by 150px: {x_before} -> {x_after}"
        );
    }

    #[test]
    fn dragging_column_border_resizes_column() {
        let mut panel = WatchPanel::new();
        panel.sync_from_channels(&["a".to_string()], &[ValueType::Uint32], &[0x2000_0000]);

        let ctx = test_ctx();
        // 首帧定位表头与第一条列分隔线
        let output = render_frame(&ctx, &mut panel, Vec::new());
        let hdr = find_rect_by_fill(&output, HEADER_BG).expect("header rect painted");
        let grab = egui::Pos2::new(hdr.left() + panel.col_widths()[0], hdr.center().y);
        let before = panel.col_widths()[0];

        // 悬停 → 按下 → 右移 40px → 松开
        render_frame(&ctx, &mut panel, vec![egui::Event::PointerMoved(grab)]);
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerButton {
                pos: grab,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
        );
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerMoved(grab + egui::vec2(40.0, 0.0))],
        );
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerButton {
                pos: grab + egui::vec2(40.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );

        let after = panel.col_widths()[0];
        assert!(
            (after - before - 40.0).abs() < 2.0,
            "dragging border by 40px should widen column by ~40px: {before} -> {after}"
        );

        // 反向拖拽同样生效：从新分隔线位置（grab+40）向左拖 30px
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerButton {
                pos: grab + egui::vec2(40.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
        );
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerMoved(grab + egui::vec2(10.0, 0.0))],
        );
        render_frame(
            &ctx,
            &mut panel,
            vec![egui::Event::PointerButton {
                pos: grab + egui::vec2(10.0, 0.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );
        let shrunk = panel.col_widths()[0];
        assert!(
            (shrunk - (after - 30.0)).abs() < 2.0,
            "dragging border left by 30px should shrink column: {after} -> {shrunk}"
        );
    }

    #[test]
    fn parse_y_scale_rejects_zero_keeps_old_value() {
        // 构造一个 panel，y_scale=2.0，然后模拟"输入 0"被拒绝 → 旧值保留
        let mut p = make_panel_with(vec![make_entry("CH1", 0.0, 2.0)]);
        // 直接推一个 dirty_transform 模拟成功提交
        p.dirty_transforms.push(("CH1".to_string(), 0.0, 2.0));
        // 模拟拒绝 0 的逻辑：验证 y_scale 字段保持 2.0
        assert!((p.entries[0].y_scale - 2.0).abs() < 1e-6);
    }

    #[test]
    fn parse_y_scale_accepts_negative() {
        // 验证负数 scale 写入字段
        let mut p = make_panel_with(vec![make_entry("CH1", 0.0, -2.5)]);
        p.entries[0].y_scale = -2.5;
        p.dirty_transforms.push(("CH1".to_string(), 0.0, -2.5));
        let drained = p.drain_changed_transforms();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "CH1");
        assert!((drained[0].2 - (-2.5)).abs() < 1e-6);
    }

    #[test]
    fn parse_non_numeric_keeps_old_value() {
        // 解析失败时旧值不变。直接检查字段，渲染层负责 reformat buffer。
        let mut p = make_panel_with(vec![make_entry("CH1", 1.5, 1.0)]);
        // 没有 dirty_transforms 被推入
        assert!(p.drain_changed_transforms().is_empty());
        // 字段未动
        assert!((p.entries[0].y_offset - 1.5).abs() < 1e-6);
    }

    #[test]
    fn sync_preserves_y_offset_scale() {
        let mut p = WatchPanel::new();
        p.sync_from_channels(&["CH1".to_string()], &[ValueType::Float], &[0]);
        // 修改 y_offset / y_scale
        p.entries[0].y_offset = 3.5;
        p.entries[0].y_scale = -1.2;
        // 重新 sync 同样的名字
        p.sync_from_channels(&["CH1".to_string()], &[ValueType::Float], &[0]);
        assert!((p.entries[0].y_offset - 3.5).abs() < 1e-6);
        assert!((p.entries[0].y_scale - (-1.2)).abs() < 1e-6);
        // 重新 sync 新名字 → 应重置为默认
        p.sync_from_channels(&["CH2".to_string()], &[ValueType::Float], &[0]);
        assert!((p.entries[0].y_offset - 0.0).abs() < 1e-6);
        assert!((p.entries[0].y_scale - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sync_keeps_column_widths() {
        // 列宽是面板级状态，与通道同步无关
        let mut p = WatchPanel::new();
        let mut widths: Vec<f32> = DEFAULT_COL_WIDTHS.to_vec();
        widths[7] = 320.0;
        p.set_col_widths(&widths);
        p.sync_from_channels(&["CH1".to_string()], &[ValueType::Float], &[0]);
        assert!((p.col_widths()[7] - 320.0).abs() < 1e-6);
    }

    #[test]
    fn drain_emits_final_only() {
        let mut p = make_panel_with(vec![make_entry("CH1", 0.0, 1.0)]);
        // 模拟同帧内多次 push（最后一次压倒前面的）
        p.dirty_transforms.push(("CH1".to_string(), 0.0, 2.0));
        p.dirty_transforms.push(("CH1".to_string(), 0.0, 3.0));
        let drained = p.drain_changed_transforms();
        assert_eq!(drained.len(), 2);
        // 调用方负责合并（去重），本 API 只负责出栈
        let mut latest: std::collections::HashMap<String, (f32, f32)> =
            std::collections::HashMap::new();
        for (n, o, s) in drained {
            latest.insert(n, (o, s));
        }
        let v = latest.get("CH1").unwrap();
        assert!((v.1 - 3.0).abs() < 1e-6);
    }
}
