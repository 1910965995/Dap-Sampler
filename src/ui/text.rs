//! UI 面板共享的文本布局工具

use std::sync::Arc;

/// 单行布局文本，超宽时以 "…" 截断。
///
/// 使用 epaint 的 `TextWrapping`（`max_rows = 1`）做真正的字形级截断，
/// 避免 `painter.text` 不裁剪导致文本溢出到相邻控件。
pub fn layout_elided(
    ctx: &egui::Context,
    text: &str,
    max_w: f32,
    font: egui::FontId,
    color: egui::Color32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ctx.fonts(|f| f.layout_job(job))
}

/// 空状态提示：在当前可用区域内水平+垂直居中显示一行灰色文字
///
/// 占满剩余空间，供侧栏 Variables / Channels / Cursor 标签页的空态使用。
pub fn centered_hint(ui: &mut egui::Ui, text: &str) {
    let color = egui::Color32::from_rgb(0x6e, 0x76, 0x82);
    let font = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .cloned()
        .unwrap_or_else(|| egui::FontId::proportional(12.5));
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
    let pos = rect.center() - galley.size() * 0.5;
    ui.painter().galley(pos, galley, color);
}
