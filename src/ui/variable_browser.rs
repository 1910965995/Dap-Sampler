use std::collections::HashSet;
use crate::elf::{ElfContext, ElfVariable};

/// Split a variable path into display segments.
///
/// Supports `a.b`, `arr[0]`, and `arr[1].field`. On unparseable input,
/// returns the whole path as a single segment.
pub fn split_var_path(path: &str) -> Vec<String> {
    if path.is_empty() {
        return vec![String::new()];
    }

    let mut segments = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    let mut ident_start: Option<usize> = None;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'.' => {
                if let Some(s) = ident_start.take() {
                    segments.push(path[s..i].to_string());
                }
                i += 1;
            }
            b'[' => {
                if let Some(s) = ident_start.take() {
                    segments.push(path[s..i].to_string());
                }
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b']' {
                    i += 1;
                    segments.push(path[start..i].to_string());
                } else {
                    segments.push(path[start..].to_string());
                    break;
                }
            }
            _ => {
                if ident_start.is_none() {
                    ident_start = Some(i);
                }
                i += 1;
            }
        }
    }
    if let Some(s) = ident_start {
        segments.push(path[s..].to_string());
    }

    if segments.is_empty() {
        vec![path.to_string()]
    } else {
        segments
    }
}

/// 变量浏览器状态（左侧「变量」标签页的内容）
pub struct VariableBrowser {
    /// 搜索框文字
    search_text: String,
    /// 已勾选的变量路径（与通道面板联动）
    pub checked_paths: HashSet<String>,
}

impl VariableBrowser {
    pub fn new() -> Self {
        Self {
            search_text: String::new(),
            checked_paths: HashSet::new(),
        }
    }

    /// 渲染变量浏览器 UI
    ///
    /// 返回 `Vec<(path, is_checked)>` 表示本帧用户勾选/取消的变更。
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        elf_ctx: Option<&ElfContext>,
    ) -> Vec<(String, bool)> {
        if elf_ctx.is_none() {
            ui.label("No ELF file loaded");
            return vec![];
        }

        let ctx = elf_ctx.unwrap();
        let mut changes = Vec::new();

        // 搜索框
        ui.add(
            egui::TextEdit::singleline(&mut self.search_text)
                .hint_text("🔍 Search variable name..."),
        );
        ui.add_space(4.0);

        // 统计信息
        let total = ctx.variables.len();
        let checked_count = self.checked_paths.len();
        ui.label(format!("Variables: {} | Selected: {}", total, checked_count));

        ui.separator();

        // 构建过滤后的显示列表（仅按变量名搜索）
        let filtered: Vec<&ElfVariable> = if self.search_text.is_empty() {
            ctx.variables.iter().collect()
        } else {
            let lower = self.search_text.to_lowercase();
            ctx.variables
                .iter()
                .filter(|v| {
                    v.name.to_lowercase().contains(&lower)
                        || v.path.to_lowercase().contains(&lower)
                })
                .collect()
        };

        // 按 source_file 分组显示，ScrollArea 限制最大高度
        egui::ScrollArea::vertical()
            .id_salt("variable_scroll")
            .auto_shrink([false, true])
            .max_height(400.0)
            .show(ui, |ui| {
                let mut current_group: Option<String> = None;

                for var in &filtered {
                    let group = var
                        .source_file
                        .clone()
                        .unwrap_or_else(|| "Globals".to_string());

                    if current_group.as_deref() != Some(&group) {
                        current_group = Some(group.clone());
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&group)
                                .small()
                                .color(egui::Color32::from_rgb(110, 118, 129)),
                        );
                    }

                    // 单行：Checkbox + 变量名
                    let indent = if var.parent_path.is_some() { 20.0 } else { 8.0 };

                    ui.horizontal(|ui| {
                        ui.add_space(indent);
                        let mut is_checked = self.checked_paths.contains(&var.path);
                        if ui.checkbox(&mut is_checked, &var.name).changed() {
                            if is_checked {
                                self.checked_paths.insert(var.path.clone());
                            } else {
                                self.checked_paths.remove(&var.path);
                            }
                            changes.push((var.path.clone(), is_checked));
                        }
                    });
                }
            });

        changes
    }

    /// 获取已勾选的路径集合
    pub fn checked(&self) -> &HashSet<String> {
        &self.checked_paths
    }

    /// 清空选择
    pub fn clear(&mut self) {
        self.checked_paths.clear();
    }
}
