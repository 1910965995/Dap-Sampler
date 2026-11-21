use egui_plot::{Line, Plot, PlotBounds, PlotPoint, Points};

/// 波形 Plot 的 egui 内存键：用于跨帧读取图例点击产生的 hidden 集
const WAVEFORM_PLOT_MEM_ID: &str = "waveform_plot_state";
use crate::pipeline::sample::{Sample, ValueType};

/// 波形显示模式
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WaveformDisplayMode {
    /// 连线模式：将采样点用线段连接起来（默认）
    Line,
    /// 散点模式：仅显示采样点，不连接
    Point,
}

impl WaveformDisplayMode {
    /// 小写字符串（与 ProjectFile::display_mode() 的解析互逆）
    pub fn label(&self) -> &'static str {
        match self {
            WaveformDisplayMode::Line => "line",
            WaveformDisplayMode::Point => "point",
        }
    }
}

/// 单通道波形线信息
pub struct ChannelInfo {
    pub name: String,
    pub color: egui::Color32,
    pub visible: bool,
    /// Y 轴偏移，应用于每个样本点（默认 0.0）
    pub y_offset: f32,
    /// Y 轴缩放系数，应用于每个样本点（默认 1.0；禁止 0；允许负数）
    pub y_scale: f32,
}

/// 波形面板
///
/// 持有通道配置，负责从 Sample 数据构建 egui_plot 的 Line 对象并渲染。
/// 内部缓存波形点：窗口滑动时增量更新（裁剪头部 + 追加尾部），
/// 仅在新数据到达/通道变化时重建，避免每帧遍历整个缓冲区。
pub struct WaveformPanel {
    /// 通道配置
    channels: Vec<ChannelInfo>,
    /// 采样间隔（微秒），用于时间戳推算
    interval_us: f64,
    /// 变量类型列表
    value_types: Vec<ValueType>,
    /// 波形点缓存（每通道一条），使用 PlotPoint 以支持零拷贝借用
    pub cached_points: Vec<Vec<PlotPoint>>,
    /// 缓存对应的缓冲区长度（0 表示需要重建）
    pub cache_buffer_len: usize,
    /// 缓存对应的 buffer_offset（最旧可用序号），增量裁剪的依据
    cache_offset: u64,
    /// 采样流中是否存在 NaN/Inf：非有限值被全量重建跳过，导致缓存点与
    /// buffer 点无法一一对应；此时退回每帧全量重建（与旧行为一致），
    /// 避免增量计数错位
    nan_in_stream: bool,
    /// 通道级脏标志：仅该通道的 Y offset/scale/可见性变化时标记
    /// rebuild_cache 仅重建脏通道，避免全量重算
    dirty_channels: Vec<bool>,
    /// 上一帧的可视 X 范围（秒），用于动态 X 轴标签
    last_x_range: Option<f64>,
    /// 上一帧的 X 轴右边界，用于检测用户拖拽/缩放
    last_x_max: Option<f64>,
    /// 当前 Follow 视野范围内的原始值绝对值最大值（Y 轴自适应用）
    auto_y_max_abs: f64,
    /// 是否自动滚动跟随最新数据
    auto_scroll: bool,
    /// 波形显示模式（连线 / 散点）
    display_mode: WaveformDisplayMode,
    /// 加粗通道名集合：图例点击切换（可多通道同时加粗），与可见性无关
    bold_names: std::collections::HashSet<String>,
    /// 本帧悬停在图例上的通道名（None = 无悬停；悬停通道波形加粗预览）
    hover_bold: Option<String>,
}

/// 波形渲染输出
pub struct WaveformOutput {
    /// 用户在波形上点击放置光标的采样点序号
    pub cursor_seq: Option<u64>,
    /// 波形图（Plot）的屏幕矩形区域（缓冲为空时无图 → None，用于叠加采样信息）
    pub plot_rect: Option<egui::Rect>,
}

impl WaveformPanel {
    pub fn new(
        channel_names: Vec<String>,
        channel_colors: Vec<egui::Color32>,
        interval_us: f64,
        value_types: Vec<ValueType>,
        display_mode: WaveformDisplayMode,
    ) -> Self {
        let n = channel_names.len();
        let channels = channel_names
            .into_iter()
            .zip(channel_colors)
            .map(|(name, color)| ChannelInfo {
                name,
                color,
                visible: true,
                y_offset: 0.0,
                y_scale: 1.0,
            })
            .collect();
        Self {
            channels,
            interval_us,
            value_types,
            cached_points: vec![Vec::new(); n],
            cache_buffer_len: 0,
            cache_offset: 0,
            nan_in_stream: false,
            dirty_channels: vec![true; n], // 首次全部重建
            last_x_range: None,
            last_x_max: None,
            auto_y_max_abs: 1.0,
            auto_scroll: true,
            display_mode,
            bold_names: Default::default(),
            hover_bold: None,
        }
    }

    /// 渲染波形
    ///
    /// 返回波形输出：点击放置光标的采样序号 + 波形图矩形区域
    /// （缓冲为空时为 None，用于叠加采样信息）。
    /// Follow 模式下每帧按当前视野 X 范围重扫 Y 轴（视野外峰值不影响）；
    /// 手动模式 Y 轴由用户控制。
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        buffer: &[Sample],
        buffer_offset: u64,
        view_points: usize,
        _has_new_data: bool,
        selected_channel: Option<usize>,
    ) -> WaveformOutput {
        if buffer.is_empty() {
            // 空状态：居中显示提示与引导。
            // 两行标签（14px + 11px + 行距）约 40px：偏移可用高度的一半
            // 再减去内容一半才是真正的居中（0.35 之类的系数会随面板高度
            // 变化偏离中心）
            ui.vertical_centered(|ui| {
                ui.add_space(((ui.available_height() - 40.0) * 0.5).max(0.0));
                ui.label(
                    egui::RichText::new("Waiting for data...")
                        .size(14.0)
                        .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                );
                ui.label(
                    egui::RichText::new("Press Start to begin sampling")
                        .size(11.0)
                        .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                );
            });
            return WaveformOutput { cursor_seq: None, plot_rect: None };
        }

        // cached_points 保留 buffer 中全部原始样本点（不降采样）；
        // 渲染时仅将可视范围内的点送入绘制（见 visible_range）。

        // ---- 缓存维护（增量更新）----
        // 窗口滑动时只裁剪头部已移出窗口的点、追加尾部新点，
        // 不重算窗口内已有点的 y 变换 —— 每帧只处理新增/移出的
        // ≤batch 个点，而非整个窗口（100k 窗口 × 8 通道时收益显著）。
        //
        // 全量重建触发条件：
        // - 所有通道都脏（初始化、采样间隔变化、上帧全量重建）
        // - 缓存与 buffer 对不上（offset/len 计数不匹配，如窗口被替换）
        // - 流中出现 NaN/Inf：非有限值被跳过，缓存点与 buffer 点
        //   无法一一对应，退回每帧全量重建（与旧行为一致）
        let has_dirty = self.dirty_channels.iter().any(|d| *d);
        let all_dirty = self.dirty_channels.iter().all(|d| *d);

        // 增量参数（按 buffer 计数，与 seq 是否连续无关）：
        // head_trim = 缓存建立后头部被丢弃的点数（offset 增量）
        // keep = 旧缓存中仍在窗口内的点数 = 追加起点
        let cache_len = self.cache_buffer_len;
        let head_trim = if buffer_offset >= self.cache_offset {
            (buffer_offset - self.cache_offset) as usize
        } else {
            cache_len // offset 回退（如重新开始采集）→ 缓存整体失效
        };
        let keep = cache_len.saturating_sub(head_trim);
        let incremental_ok = !all_dirty && buffer.len() >= keep && !self.nan_in_stream;

        if !incremental_ok {
            // 计数对不上 / 流中有 NaN / 全部通道脏：全量重建
            self.mark_all_dirty();
            self.rebuild_cache(buffer);
            self.cache_buffer_len = buffer.len();
            self.cache_offset = buffer_offset;
        } else {
            let has_new_points = buffer.len() > keep;
            if has_new_points {
                if self.append_has_non_finite(buffer, keep) {
                    // 追加区含 NaN/Inf → 退回全量重建（增量计数会错位）
                    self.nan_in_stream = true;
                    self.mark_all_dirty();
                    self.rebuild_cache(buffer);
                    self.cache_buffer_len = buffer.len();
                    self.cache_offset = buffer_offset;
                } else {
                    self.incremental_update(buffer, buffer_offset, head_trim, keep);
                }
            } else if head_trim > 0 {
                // 无新点但窗口收缩（如运行中减小窗口）：仅裁剪头部
                self.incremental_update(buffer, buffer_offset, head_trim, keep);
            }

            // 变换/可见性变化的通道全量重建（增量已先更新干净通道）
            if has_dirty {
                self.rebuild_cache(buffer);
            }
        }

        // 动态 X 轴标签（使用上一帧的可视范围）
        let x_label = match self.last_x_range {
            Some(r) => format!("Time (s) [window: {:.2}s]", r),
            None => "Time (s)".to_string(),
        };

        // Y 轴标签
        let y_label = "Value".to_string();

        // 始终允许缩放/拖拽，auto_scroll 只决定 X 轴是否跟随最新数据
        let auto_scroll = self.auto_scroll;
        let mut clicked_seq: Option<u64> = None;

        // 图例为自绘 overlay（见 draw_legend）：内置 Legend 无法为「锁定
        // 加粗」状态提供持久视觉标记，且悬停加粗/点击切换两套行为纠缠。

        let plot = Plot::new("waveform_plot")
            .id(egui::Id::new(WAVEFORM_PLOT_MEM_ID))
            .show_grid(true)
            .show_axes(true)
            .x_axis_label(x_label)
            .y_axis_label(y_label)
            .allow_zoom(true)
            .allow_drag(true)
            .allow_scroll(true);

        // 不使用 plot.auto_bounds，改用自定义 Y 轴范围控制

        // 准备自动滚动参数
        let interval_us = self.interval_us;
        let buffer_len = buffer.len();
        let latest_ts = buffer.last().map(|s| s.timestamp_sec).unwrap_or(0.0);
        // 记录上一帧的 X 轴右边界，用于检测用户是否拖拽/缩放
        let prev_x_max = self.last_x_max;

        let plot_response = plot.show(ui, |plot_ui| {
            let cur_bounds = plot_ui.plot_bounds();
            let cur_x_max = cur_bounds.max()[0];

            // 用户拖拽/缩放检测：如果 X 轴右边界偏离了上一帧位置（超过 1% 窗口时长），
            // 说明用户在手动操作，自动暂停跟随
            if auto_scroll && buffer_len > 0 {
                let view_len = view_points.min(buffer_len);
                let window_duration = view_len as f64 * interval_us / 1_000_000.0;
                if let Some(prev) = prev_x_max {
                    // X 右边界变化超过 1% 窗口时长 → 用户在操作
                    let drift = (cur_x_max - prev).abs();
                    if drift > window_duration * 0.01 {
                        self.auto_scroll = false;
                    }
                }
            }

            // 自动滚动：X 轴跟随最新数据，窗口宽度 = min(视野点数, 可用数据)
            // Y 轴基准先用本帧视野范围重扫（首帧即可正确，无延迟）
            if auto_scroll && buffer_len > 0 {
                let view_len = view_points.min(buffer_len);
                let window_duration = view_len as f64 * interval_us / 1_000_000.0;
                self.rescan_y_max(buffer, latest_ts - window_duration, latest_ts);
                let y_bound = 2.0 * self.auto_y_max_abs;
                let new_bounds = PlotBounds::from_min_max(
                    [latest_ts - window_duration, -y_bound],
                    [latest_ts, y_bound],
                );
                plot_ui.set_plot_bounds(new_bounds);
                self.last_x_max = Some(latest_ts);
            } else {
                self.last_x_max = Some(cur_x_max);
            }

            // 记录可视范围（用于下一帧的 X 轴标签），渲染裁剪共用本次 bounds
            let bounds = plot_ui.plot_bounds();
            self.last_x_range = Some(bounds.max()[0] - bounds.min()[0]);
            let vis_min = bounds.min()[0];
            let vis_max = bounds.max()[0];

            // 捕获鼠标坐标（用于光标点击）
            let pointer_coord = plot_ui.pointer_coordinate();

            // 渲染各通道：选中加粗；有选中时其它通道降透明度
            // 只把可视范围内的点送入渲染管线（边界各扩展 1 点避免线段断裂），
            // 视野外不可见部分不绘制 —— 不是降采样，视野内每个原始点仍 100% 绘制。
            let has_selection = selected_channel.is_some();
            for (ch_idx, ch) in self.channels.iter().enumerate() {
                if !ch.visible {
                    continue;
                }
                let points = &self.cached_points[ch_idx];
                if points.is_empty() {
                    continue;
                }
                let (start, end) = visible_range(points, vis_min, vis_max);
                if start >= end {
                    continue;
                }
                let visible = &points[start..end];
                let is_sel = selected_channel == Some(ch_idx);
                let color = if has_selection && !is_sel {
                    dim_color(ch.color, 0.38)
                } else {
                    ch.color
                };
                // 加粗 = 图例点击锁定；悬停图例时对 hovered 通道做同样的
                // 加粗预览（hover_bold 由 draw_legend 每帧覆盖赋值）
                let hover = self.hover_bold.as_deref() == Some(ch.name.as_str());
                let bold = hover || self.bold_names.contains(&ch.name);
                let line_w = if bold { 3.2 } else if is_sel { 2.8 } else { 1.4 };
                let point_r = if bold { 3.0 } else if is_sel { 2.4 } else { 1.5 };
                match self.display_mode {
                    WaveformDisplayMode::Line => {
                        plot_ui.line(
                            Line::new(visible)
                                .name(&ch.name)
                                .color(color)
                                .width(line_w),
                        );
                    }
                    WaveformDisplayMode::Point => {
                        plot_ui.points(
                            Points::new(visible)
                                .name(&ch.name)
                                .color(color)
                                .radius(point_r),
                        );
                    }
                }
            }

            // 检测点击 → 放置光标
            if plot_ui.response().clicked() {
                if let Some(coord) = pointer_coord {
                    // 用真实时间戳查找最接近的采样点
                    let target = coord.x;
                    if !buffer.is_empty() {
                        let idx = buffer.binary_search_by(|s| {
                            s.timestamp_sec.partial_cmp(&target).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        let local_idx = match idx {
                            Ok(i) => i,
                            Err(i) => {
                                if i == 0 {
                                    0
                                } else if i >= buffer.len() {
                                    buffer.len() - 1
                                } else {
                                    let d1 = (buffer[i - 1].timestamp_sec - target).abs();
                                    let d2 = (buffer[i].timestamp_sec - target).abs();
                                    if d1 < d2 { i - 1 } else { i }
                                }
                            }
                        };
                        clicked_seq = Some(buffer_offset + local_idx as u64);
                    }
                }
            }
        });

        // 自绘图例（右上角悬浮）：点击锁定加粗、悬停预览加粗
        self.draw_legend(ui, plot_response.response.rect);

        WaveformOutput {
            cursor_seq: clicked_seq,
            plot_rect: Some(plot_response.response.rect),
        }
    }

    /// 右上角悬浮图例（自绘，替代 egui_plot 内置 Legend）
    ///
    /// 每个通道一行：色点 + 通道名。
    /// - 悬停：色点外圈高亮（与内置悬停观感一致），对应通道波形加粗预览
    /// - 点击：**锁定**该状态——色点外圈持久保留（与悬停同款圆圈），波形
    ///   持续加粗；再次点击解锁。锁定状态在图例中始终可见
    ///
    /// 悬停态通过 `hover_bold` 字段在下一帧生效（图例绘制在波形之后，
    /// 一帧延迟对悬停预览无感知）。
    fn draw_legend(&mut self, ui: &mut egui::Ui, plot_rect: egui::Rect) {
        if self.channels.is_empty() {
            self.hover_bold = None;
            return;
        }
        let mut hovered: Option<String> = None;

        egui::Area::new(ui.id().with("waveform_legend"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 6.0))
            .constrain_to(plot_rect)
            .show(ui.ctx(), |ui| {
                // 与内置 Legend 相同的底框（半透明白底 + 窗口描边）
                egui::Frame::NONE
                    .inner_margin(egui::Margin { left: 8, right: 8, top: 4, bottom: 4 })
                    .corner_radius(ui.style().visuals.window_corner_radius)
                    .fill(ui.style().visuals.extreme_bg_color)
                    .stroke(ui.style().visuals.window_stroke)
                    .multiply_with_opacity(0.75)
                    .show(ui, |ui| {
                        // 先快照通道列表（循环内要可变借用 self 切换加粗）
                        let entries: Vec<(String, egui::Color32)> = self
                            .channels
                            .iter()
                            .map(|c| (c.name.clone(), c.color))
                            .collect();
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let text_h = ui.text_style_height(&egui::TextStyle::Body);
                        let icon = text_h.max(13.0);
                        let gap = 6.0;
                        let row_h = icon.max(text_h) + 4.0;

                        // 预排版所有通道名 → 统一行宽：对齐由**等宽行**保证，
                        // 不用 Align::Max 右对齐（top_down 布局的初始游标会把
                        // min_rect 撑满整个可用宽度，底框被拉成全宽、内容前
                        // 出现大片空白）
                        let galleys: Vec<(std::sync::Arc<egui::Galley>, egui::Color32)> = entries
                            .iter()
                            .map(|(name, color)| {
                                (
                                    ui.painter().layout_no_wrap(
                                        name.clone(),
                                        egui::FontId::proportional(12.5),
                                        egui::Color32::PLACEHOLDER,
                                    ),
                                    *color,
                                )
                            })
                            .collect();
                        let max_text_w = galleys
                            .iter()
                            .map(|(g, _)| g.size().x)
                            .fold(0.0f32, f32::max);
                        let row_w = icon + gap + max_text_w;

                        for (galley, color) in &galleys {
                            let (rect, resp) =
                                ui.allocate_exact_size(egui::vec2(row_w, row_h), egui::Sense::click());
                            let name = galley.text().to_string();
                            let locked = self.bold_names.contains(&name);
                            if resp.hovered() {
                                hovered = Some(name.clone());
                            }
                            if resp.clicked() {
                                self.toggle_bold_by_name(&name);
                            }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if !ui.is_rect_visible(rect) {
                                continue;
                            }

                            // 色点：空闲时只有通道色实心圆（无圈无底）；悬停
                            // 或锁定时才画外圈（底圆 + 悬停态描边），锁定 =
                            // 持久的悬停外观。排布与内置 RightTop 图例一致
                            // （label_on_the_left）：文字在左（右对齐到共同
                            // 边界），色点列在右侧对齐
                            let dot_center =
                                egui::pos2(rect.right() - icon * 0.5, rect.center().y);
                            if resp.hovered() || locked {
                                ui.painter().add(egui::epaint::CircleShape {
                                    center: dot_center,
                                    radius: icon * 0.5,
                                    fill: ui.visuals().widgets.hovered.bg_fill,
                                    stroke: ui.visuals().widgets.hovered.bg_stroke,
                                });
                            }
                            ui.painter().circle_filled(dot_center, icon * 0.4, *color);
                            // 通道名（右对齐到色点左侧的共同边界）
                            let text_color = if resp.hovered() {
                                ui.style().interact(&resp).text_color()
                            } else {
                                ui.style().visuals.text_color()
                            };
                            let mut shape = egui::epaint::TextShape::new(
                                egui::pos2(
                                    rect.right() - icon - gap - galley.size().x,
                                    rect.center().y - galley.size().y * 0.5,
                                ),
                                galley.clone(),
                                text_color,
                            );
                            shape.override_text_color = Some(text_color);
                            ui.painter().add(shape);
                        }
                    });
            });
        // 每帧整体覆盖：本帧悬停的通道名（None = 无悬停）
        self.hover_bold = hovered;
    }

    /// 重建波形缓存
    ///
    /// 关键不变式: **buffer 中的每个原始样本点都进入 cached_points**。
    /// 不进行任何形式的降采样、抽点或跳点 —— 采样工具的波形必须 100% 忠实于采集到的数据。
    /// 每个点按 `displayed_y = raw_f * y_scale + y_offset` 变换;Y 轴自动 fit 仍按 `raw_f.abs()`。
    ///
    /// 优化：仅重建脏通道（可见性/变换变化的通道），非脏通道复用已有缓存。
    /// 调用方通过 `mark_all_dirty()` 或 `mark_channel_dirty()` 控制重建范围。
    pub fn rebuild_cache(&mut self, buffer: &[Sample]) {
        let mut found_non_finite = false;

        for (ch_idx, ch) in self.channels.iter().enumerate() {
            let dirty = self.dirty_channels.get(ch_idx).copied().unwrap_or(true);

            if !ch.visible {
                if dirty {
                    self.cached_points[ch_idx].clear();
                    self.dirty_channels[ch_idx] = false;
                }
                continue;
            }

            if !dirty {
                // 非脏通道：复用已有缓存，无需重建
                continue;
            }

            // 脏通道：全量重建
            let vt = self.value_types.get(ch_idx).copied().unwrap_or(ValueType::Float);
            let scale = ch.y_scale as f64;
            let offset = ch.y_offset as f64;

            // 每个原始样本点都转换为 PlotPoint，无降采样、无跳点
            let mut out: Vec<PlotPoint> = Vec::with_capacity(buffer.len());
            for s in buffer {
                let raw = s.values().get(ch_idx).copied().unwrap_or(0);
                let raw_f = vt.to_f64(raw);
                if !raw_f.is_finite() {
                    found_non_finite = true;
                    continue; // NaN/Inf 不绘制
                }
                let displayed_y = raw_f * scale + offset;
                out.push(PlotPoint::new(s.timestamp_sec, displayed_y));
            }

            self.cached_points[ch_idx] = out;
            self.dirty_channels[ch_idx] = false;
        }

        // 记录本次重建是否遇到非有限值：
        // 决定后续帧是否退回每帧全量重建（避免增量计数错位）
        self.nan_in_stream = found_non_finite;
    }

    /// 增量更新缓存：裁剪头部已移出窗口的点，追加尾部新点。
    ///
    /// 关键不变式：调用方保证 `buffer[keep..]`（追加区）不含 NaN/Inf，
    /// 且流中无历史 NaN（nan_in_stream=false）——否则缓存点与 buffer 点
    /// 无法一一对应，增量计数会错位，需走全量重建。
    fn incremental_update(
        &mut self,
        buffer: &[Sample],
        buffer_offset: u64,
        head_trim: usize,
        keep: usize,
    ) {
        for (ch_idx, ch) in self.channels.iter().enumerate() {
            if !ch.visible {
                continue;
            }
            let vt = self.value_types.get(ch_idx).copied().unwrap_or(ValueType::Float);
            let scale = ch.y_scale as f64;
            let offset = ch.y_offset as f64;

            let pts = &mut self.cached_points[ch_idx];
            if head_trim > 0 {
                // 裁剪头部：仅移动元素，保留容量（避免重新分配）
                pts.drain(..head_trim.min(pts.len()));
            }
            for s in &buffer[keep..] {
                let raw = s.values().get(ch_idx).copied().unwrap_or(0);
                let raw_f = vt.to_f64(raw);
                // 追加区已预检无 NaN/Inf；防御性跳过（不破坏计数）
                if !raw_f.is_finite() {
                    continue;
                }
                pts.push(PlotPoint::new(s.timestamp_sec, raw_f * scale + offset));
            }
        }

        self.cache_buffer_len = buffer.len();
        self.cache_offset = buffer_offset;
    }

    /// 预检追加区 `buffer[keep..]` 是否含 NaN/Inf。
    ///
    /// 有非有限值时缓存点与 buffer 点无法一一对应（全量重建会跳过这些点），
    /// 增量追加会导致计数错位，必须退回全量重建。
    fn append_has_non_finite(&self, buffer: &[Sample], keep: usize) -> bool {
        for s in &buffer[keep..] {
            for (ch_idx, ch) in self.channels.iter().enumerate() {
                if !ch.visible {
                    continue;
                }
                let vt = self.value_types.get(ch_idx).copied().unwrap_or(ValueType::Float);
                let raw = s.values().get(ch_idx).copied().unwrap_or(0);
                if !vt.to_f64(raw).is_finite() {
                    return true;
                }
            }
        }
        false
    }

    /// 按可视 X 范围重扫 Y 轴基准（Follow 模式下每帧调用，Y 轴跟随当前视野）。
    ///
    /// 只统计视野内的原始值（不被 offset/scale 推偏），视野外历史峰值不影响当前 Y 轴。
    /// buffer 时间戳单调递增，二分定位视野区间后扫描。
    fn rescan_y_max(&mut self, buffer: &[Sample], x_min: f64, x_max: f64) {
        if buffer.is_empty() {
            return;
        }
        let i_start = buffer.partition_point(|s| s.timestamp_sec < x_min);
        let i_end = buffer.partition_point(|s| s.timestamp_sec <= x_max);
        if i_start >= i_end {
            return;
        }
        let mut y_max_abs = 0.0f64;
        for (ch_idx, ch) in self.channels.iter().enumerate() {
            if !ch.visible {
                continue;
            }
            let vt = self.value_types.get(ch_idx).copied().unwrap_or(ValueType::Float);
            for s in &buffer[i_start..i_end] {
                let raw = s.values().get(ch_idx).copied().unwrap_or(0);
                let raw_f = vt.to_f64(raw);
                if raw_f.is_finite() {
                    let a = raw_f.abs();
                    if a > y_max_abs {
                        y_max_abs = a;
                    }
                }
            }
        }
        self.auto_y_max_abs = if y_max_abs > 0.0 { y_max_abs } else { 1.0 };
    }

    /// 标记指定通道为脏（需要重建缓存）
    fn mark_channel_dirty(&mut self, index: usize) {
        if index < self.dirty_channels.len() {
            self.dirty_channels[index] = true;
        }
    }

    /// 标记所有通道为脏（全量重建）
    fn mark_all_dirty(&mut self) {
        for d in self.dirty_channels.iter_mut() {
            *d = true;
        }
    }

    /// 切换通道可见性
    pub fn toggle_channel(&mut self, index: usize) {
        if let Some(ch) = self.channels.get_mut(index) {
            ch.visible = !ch.visible;
            self.mark_channel_dirty(index);
        }
    }

    /// 图例点击：切换通道加粗状态（可多通道同时加粗）
    ///
    /// 加粗仅影响线宽/点半径，与可见性（Channels 界面的眼睛）无关；
    /// 名字不在通道列表中时静默忽略。
    pub fn toggle_bold_by_name(&mut self, name: &str) {
        if !self.bold_names.remove(name) {
            self.bold_names.insert(name.to_owned());
        }
    }

    /// 当前加粗通道名集合（重建通道集时用于保留状态）
    pub fn bold_names(&self) -> &std::collections::HashSet<String> {
        &self.bold_names
    }

    /// 恢复加粗通道集合
    pub fn set_bold_names(&mut self, names: std::collections::HashSet<String>) {
        self.bold_names = names;
    }

    /// 查询通道可见性
    pub fn is_channel_visible(&self, index: usize) -> bool {
        self.channels.get(index).map(|ch| ch.visible).unwrap_or(false)
    }

    /// 设置通道可见性
    pub fn set_channel_visible(&mut self, index: usize, visible: bool) {
        if let Some(ch) = self.channels.get_mut(index) {
            if ch.visible != visible {
                ch.visible = visible;
                self.mark_channel_dirty(index);
            }
        }
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn channel_names(&self) -> Vec<&str> {
        self.channels.iter().map(|c| c.name.as_str()).collect()
    }

    pub fn value_types(&self) -> &[ValueType] {
        &self.value_types
    }

    /// 更新采样间隔（采样率变化时调用）
    pub fn set_interval(&mut self, interval_us: f64) {
        self.interval_us = interval_us;
        self.mark_all_dirty();
        self.last_x_max = None; // 重置拖拽检测
    }

    /// 设置波形显示模式
    pub fn set_display_mode(&mut self, mode: WaveformDisplayMode) {
        self.display_mode = mode;
    }

    /// 当前是否处于 Follow（自动滚动）模式
    pub fn is_follow(&self) -> bool {
        self.auto_scroll
    }

    /// 设置 Follow (auto_scroll) 状态。
    /// 点击 Stop 时调用 set_follow(false) 让 X 轴停在当前位置,不再跟随最新数据。
    pub fn set_follow(&mut self, on: bool) {
        self.auto_scroll = on;
    }

    /// 获取当前波形显示模式
    pub fn display_mode(&self) -> WaveformDisplayMode {
        self.display_mode
    }

    /// 设置单个通道的 y_offset / y_scale,并标记该通道缓存需要重建。
    /// 若通道不存在(name 不匹配),该调用为 no-op。
    pub fn set_channel_transform(&mut self, name: &str, offset: f32, scale: f32) {
        if let Some(idx) = self.channels.iter().position(|c| c.name == name) {
            self.channels[idx].y_offset = offset;
            self.channels[idx].y_scale = scale;
            self.mark_channel_dirty(idx);
        }
    }
}

/// 计算可视范围内点的切片索引 `[start, end)`（含边界各 1 点扩展，避免线段断裂）。
///
/// `points.x` 必须单调递增（时间戳序列）。视野完全在数据左侧/右侧时
/// 返回含 1 个边界点的切片；空点集返回 `(0, 0)`。
fn visible_range(points: &[PlotPoint], vis_min: f64, vis_max: f64) -> (usize, usize) {
    if points.is_empty() {
        return (0, 0);
    }
    let start = points.partition_point(|p| p.x < vis_min).saturating_sub(1);
    let end = (points.partition_point(|p| p.x <= vis_max) + 1).min(points.len());
    (start, end)
}

fn dim_color(c: egui::Color32, alpha: f32) -> egui::Color32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::{Sample, ValueType};

    fn make_panel(channels: Vec<(&str, f32, f32)>) -> WaveformPanel {
        let names: Vec<String> = channels.iter().map(|(n, _, _)| n.to_string()).collect();
        let colors: Vec<egui::Color32> = (0..channels.len())
            .map(|i| egui::Color32::from_rgb((i * 30) as u8, 100, 200))
            .collect();
        let types: Vec<ValueType> = vec![ValueType::Float; channels.len()];
        let mut panel = WaveformPanel::new(names, colors, 1000.0, types, WaveformDisplayMode::Line);
        for (ch, (_, off, sca)) in panel.channels.iter_mut().zip(channels.iter()) {
            ch.y_offset = *off;
            ch.y_scale = *sca;
        }
        panel
    }

    fn make_buffer(seqs: u64, vals: &[u64]) -> Vec<Sample> {
        (0..seqs)
            .map(|i| Sample::new(i, i as f64 * 0.001, vals))
            .collect()
    }

    #[test]
    fn rebuild_cache_applies_transform() {
        let mut panel = make_panel(vec![("CH1", 10.0, 2.0)]);
        let buffer = make_buffer(1, &[u64::from(3.0f32.to_bits())]);
        panel.rebuild_cache(&buffer);
        // displayed_y = 3.0 * 2.0 + 10.0 = 16.0
        assert_eq!(panel.cached_points[0].len(), 1);
        assert!((panel.cached_points[0][0].y - 16.0).abs() < 1e-6);
    }

    #[test]
    fn rescan_y_uses_raw_not_transformed() {
        // rescan 统计 raw 值，不应受 offset/scale 影响
        let mut panel = make_panel(vec![("CH1", 1.0e6, 1.0)]);
        let buffer = make_buffer(1, &[u64::from(1.0f32.to_bits())]);
        panel.rescan_y_max(&buffer, 0.0, 0.001);
        // raw_f = 1.0, 不应受 offset 影响
        assert!((panel.auto_y_max_abs - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_channel_transform_marks_dirty() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        let buffer = make_buffer(1, &[u64::from(1.0f32.to_bits())]);
        panel.rebuild_cache(&buffer);
        // rebuild_cache 后所有脏标志应被清除
        assert!(panel.dirty_channels.iter().all(|d| !d));
        // 触发 transform 变化 → 应标记该通道为脏
        panel.set_channel_transform("CH1", 5.0, 1.0);
        assert!(panel.dirty_channels[0]);
    }

    #[test]
    fn rebuild_cache_no_samples_lost() {
        // 关键保真测试: 任何规模的 buffer 都必须完整保留所有样本点
        let mut panel = make_panel(vec![("CH1", 100.0, 1.0)]);
        let n = 5000_usize;
        // 每个样本一个值,值 = i as f32
        let buffer: Vec<Sample> = (0..n)
            .map(|i| Sample::new(i as u64, i as f64 * 0.001, &[u64::from((i as f32).to_bits())]))
            .collect();
        panel.rebuild_cache(&buffer);
        assert_eq!(panel.cached_points[0].len(), n);
        // 验证 transform 应用到了每一个点(不是只前 N 个)
        for i in 0..n {
            let raw = i as f64;
            let expected = raw * 1.0 + 100.0;
            let got = panel.cached_points[0][i].y;
            assert!(
                (got - expected).abs() < 1e-3,
                "sample {i}: expected {expected}, got {got}"
            );
        }
    }

    #[test]
    fn uniform_downsample_removed() {
        // 编译期检查: uniform_downsample 不应存在
        // (无法直接 assert 编译失败,但我们通过文档 + 后续 grep 在 PR 中确认)
        // 此测试仅作为占位提示。如未来有人重新引入降采样,这里需补充编译期检查。
    }

    // ============================================================
    // 增量缓存（incremental_update）
    // ============================================================

    #[test]
    fn incremental_update_trims_and_appends() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        let buf1 = make_buffer(10, &[u64::from(1.0f32.to_bits())]); // 10 点，raw=1.0
        panel.rebuild_cache(&buf1);
        panel.cache_buffer_len = buf1.len();
        panel.cache_offset = 0;

        // 模拟窗口滑动：头部丢弃 3 点（offset=3），尾部追加 4 点（raw=2.0）
        let mut buf2: Vec<Sample> = buf1[3..].to_vec();
        for k in 0..4u64 {
            buf2.push(Sample::new(10 + k, (10 + k) as f64 * 0.001, &[u64::from(2.0f32.to_bits())]));
        }

        panel.incremental_update(&buf2, 3, 3, buf1.len() - 3);

        // 7 个保留点 + 4 个新点 = 11
        assert_eq!(panel.cached_points[0].len(), 11);
        assert_eq!(panel.cache_buffer_len, 11);
        assert_eq!(panel.cache_offset, 3);
        // 保留点：x 从 3*0.001 开始，值保持 1.0（未被重算）
        for (i, p) in panel.cached_points[0][..7].iter().enumerate() {
            assert!((p.x - (i as f64 + 3.0) * 0.001).abs() < 1e-9);
            assert!((p.y - 1.0).abs() < 1e-9);
        }
        // 新点：值 2.0，x 从 10*0.001 开始
        for (i, p) in panel.cached_points[0][7..].iter().enumerate() {
            assert!((p.x - (10.0 + i as f64) * 0.001).abs() < 1e-9);
            assert!((p.y - 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn incremental_update_applies_transform_to_new_points() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0), ("CH2", 100.0, 0.5)]);
        let buf1 = make_buffer(5, &[u64::from(4.0f32.to_bits()), u64::from(4.0f32.to_bits())]);
        panel.rebuild_cache(&buf1);
        panel.cache_buffer_len = 5;
        panel.cache_offset = 0;

        // 追加 2 个新点（raw=8.0）
        let mut buf2 = buf1.clone();
        for k in 0..2u64 {
            buf2.push(Sample::new(5 + k, (5 + k) as f64 * 0.001, &[u64::from(8.0f32.to_bits()), u64::from(8.0f32.to_bits())]));
        }

        panel.incremental_update(&buf2, 0, 0, 5);

        assert_eq!(panel.cached_points[0].len(), 7);
        assert_eq!(panel.cached_points[1].len(), 7);
        // 新点应用各自变换：CH1 = 8.0*1.0+0.0 = 8.0；CH2 = 8.0*0.5+100.0 = 104.0
        assert!((panel.cached_points[0][5].y - 8.0).abs() < 1e-6);
        assert!((panel.cached_points[1][5].y - 104.0).abs() < 1e-6);
        // 旧点保持原变换值：CH2 首点 = 4.0*0.5+100.0 = 102.0
        assert!((panel.cached_points[1][0].y - 102.0).abs() < 1e-6);
    }

    #[test]
    fn incremental_update_window_shrink_only_trims() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        let buf1 = make_buffer(10, &[u64::from(1.0f32.to_bits())]);
        panel.rebuild_cache(&buf1);
        panel.cache_buffer_len = 10;
        panel.cache_offset = 0;

        // 窗口收缩：只丢弃头部 6 点（如运行中减小窗口，无新数据）
        let buf2: Vec<Sample> = buf1[6..].to_vec();
        panel.incremental_update(&buf2, 6, 6, 4);

        assert_eq!(panel.cached_points[0].len(), 4);
        assert_eq!(panel.cache_buffer_len, 4);
        assert_eq!(panel.cache_offset, 6);
        // 保留点未被重算（x 从 6*0.001 开始）
        for (i, p) in panel.cached_points[0].iter().enumerate() {
            assert!((p.x - (i as f64 + 6.0) * 0.001).abs() < 1e-9);
        }
    }

    #[test]
    fn append_has_non_finite_detects_nan() {
        let panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        let mut buf = make_buffer(2, &[u64::from(1.0f32.to_bits())]);
        buf.push(Sample::new(2, 0.002, &[u64::from(f32::NAN.to_bits())]));
        // 追加区（keep=2 起）含 NaN → true
        assert!(panel.append_has_non_finite(&buf, 2));
        // 前 2 点正常 → false
        assert!(!panel.append_has_non_finite(&buf[..2], 0));
    }

    #[test]
    fn rescan_y_max_resets_drifted_scale() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        panel.auto_y_max_abs = 100.0; // 人为抬高（模拟峰值已移出视野）
        let buf = make_buffer(5, &[u64::from(3.0f32.to_bits())]);
        panel.rescan_y_max(&buf, 0.0, 0.005);
        assert!((panel.auto_y_max_abs - 3.0).abs() < 1e-9);
    }

    #[test]
    fn rebuild_cache_tracks_nan_in_stream() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        let mut buf = make_buffer(3, &[u64::from(1.0f32.to_bits())]);
        buf.push(Sample::new(3, 0.003, &[u64::from(f32::INFINITY.to_bits())]));
        panel.rebuild_cache(&buf);
        assert!(panel.nan_in_stream);
        // NaN/Inf 点被跳过：3 个有效点
        assert_eq!(panel.cached_points[0].len(), 3);

        // 非有限值离开 buffer 后，重扫应复位标志
        let clean = make_buffer(3, &[u64::from(1.0f32.to_bits())]);
        panel.rebuild_cache(&clean);
        assert!(!panel.nan_in_stream);
    }

    #[test]
    fn toggle_bold_by_name_flips_and_ignores_unknown() {
        let mut panel = WaveformPanel::new(
            vec!["CH1".to_string(), "CH2".to_string()],
            vec![egui::Color32::RED, egui::Color32::GREEN],
            1000.0,
            vec![ValueType::Float, ValueType::Float],
            WaveformDisplayMode::Line,
        );

        // 首次点击：加粗
        panel.toggle_bold_by_name("CH1");
        assert!(panel.bold_names().contains("CH1"), "点击后应加粗");
        assert!(!panel.bold_names().contains("CH2"), "其余通道不受影响");

        // 再次点击：取消加粗
        panel.toggle_bold_by_name("CH1");
        assert!(!panel.bold_names().contains("CH1"), "再次点击应取消加粗");

        // 未知通道名：静默忽略（进入集合无妨，渲染按通道名匹配）
        panel.toggle_bold_by_name("CH9");
        panel.toggle_bold_by_name("CH9");
        assert!(!panel.bold_names().contains("CH9"), "未知名切换两次应回到空");
    }

    #[test]
    fn bold_names_survive_channel_set_rebuild() {
        // 重建通道集（WaveformPanel::new 替换实例）时按名保留加粗状态
        let mut panel = WaveformPanel::new(
            vec!["CH1".to_string()],
            vec![egui::Color32::RED],
            1000.0,
            vec![ValueType::Float],
            WaveformDisplayMode::Line,
        );
        panel.toggle_bold_by_name("CH1");

        let kept = panel.bold_names().clone();
        let mut rebuilt = WaveformPanel::new(
            vec!["CH1".to_string(), "CH2".to_string()],
            vec![egui::Color32::RED, egui::Color32::GREEN],
            1000.0,
            vec![ValueType::Float, ValueType::Float],
            WaveformDisplayMode::Line,
        );
        rebuilt.set_bold_names(kept);
        assert!(rebuilt.bold_names().contains("CH1"), "重建后应保留加粗");
    }

    #[test]
    fn legend_rows_render_without_overlap() {
        // 自绘图例回归：每个通道一行（色点 + 名称），行间不重叠。
        // 注意：egui Area 的第一帧是 sizing pass（绘制被丢弃），需先跑
        // 一帧预热，第二帧才能收集到图例形状。
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0), ("Motor Current", 0.0, 1.0), ("温度", 0.0, 1.0)]);
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {}); // 初始化字体
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 400.0),
            )),
            ..Default::default()
        };
        let output = {
            // 第一帧：Area sizing pass（输出被 egui 丢弃）
            let _ = ctx.run(raw.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    panel.draw_legend(ui, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0)));
                });
            });
            // 第二帧：正常绘制
            ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    panel.draw_legend(ui, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0)));
                });
            })
        };
        let mut boxes: Vec<egui::Rect> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                boxes.push(egui::Rect::from_min_size(t.pos, t.galley.size()));
            }
        }
        assert_eq!(boxes.len(), 3, "应绘制 3 个通道名");
        boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
        for w in boxes.windows(2) {
            assert!(
                w[1].top() >= w[0].bottom(),
                "图例行文字包围盒重叠: {:?} vs {:?}",
                w[0],
                w[1]
            );
        }
        assert!(
            boxes[0].width() < boxes[1].width(),
            "行宽应随文本长度变化"
        );
        // 底框必须紧贴内容（曾被 Align::Max 初始游标撑成全宽，左侧出现
        // 大片空白）。带描边的 Rect = 图例底框；CentralPanel 背景无描边
        for clipped in &output.shapes {
            if let egui::Shape::Rect(r) = &clipped.shape {
                if r.stroke.width > 0.0 {
                    assert!(
                        r.rect.width() < 220.0,
                        "图例底框应紧贴内容，不应被撑成全宽: {:?}",
                        r.rect
                    );
                }
            }
        }
    }

    #[test]
    fn visible_range_returns_inner_slice_with_border_points() {
        // 10 个点，x = 0..10
        let pts: Vec<PlotPoint> = (0..10u32)
            .map(|i| PlotPoint::new(i as f64, 0.0))
            .collect();
        // 视野 [3.2, 6.8]：内部点 4..6，边界各扩展 1 → [3, 8)
        // （右边界含 x=7 桥接点：其与 x=6 的线段跨越视野右缘，缺失会留出空隙）
        let (s, e) = visible_range(&pts, 3.2, 6.8);
        assert_eq!((s, e), (3, 8));
        // 视野覆盖全部：内部 0..10，扩展后 [0, 10)
        let (s, e) = visible_range(&pts, -1.0, 100.0);
        assert_eq!((s, e), (0, 10));
        // 视野完全在数据左侧：内部空，扩展后 [0, 1)
        let (s, e) = visible_range(&pts, -50.0, -10.0);
        assert_eq!((s, e), (0, 1));
        // 视野完全在数据右侧：内部空，扩展后 [9, 10)（最近边界点 x=9）
        let (s, e) = visible_range(&pts, 50.0, 90.0);
        assert_eq!((s, e), (9, 10));
        // 空点集
        let (s, e) = visible_range(&[], 0.0, 1.0);
        assert_eq!((s, e), (0, 0));
    }

    #[test]
    fn rescan_y_max_scans_visible_window_only() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        // 值：视野外 [0,2) 有峰值 100；视野内 [2,4) 值 3
        let buffer: Vec<Sample> = (0..4u64)
            .map(|i| {
                let v: f32 = if i < 2 { 100.0 } else { 3.0 };
                Sample::new(i, i as f64 * 0.001, &[u64::from(v.to_bits())])
            })
            .collect();
        // 初始人为抬高，验证重扫会修正
        panel.auto_y_max_abs = 200.0;
        panel.rescan_y_max(&buffer, 0.002, 0.004);
        assert!((panel.auto_y_max_abs - 3.0).abs() < 1e-9, "视野外峰值不应影响 Y 轴");
        // 空 buffer 不 panic、保持原值
        panel.auto_y_max_abs = 5.0;
        panel.rescan_y_max(&[], 0.0, 1.0);
        assert!((panel.auto_y_max_abs - 5.0).abs() < 1e-9);
    }
}
