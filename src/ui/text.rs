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
