use std::cmp::Ordering;
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

/// Node in the collapsible variable tree built from flat ELF variable paths.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub label: String,
    pub path: String,
    pub is_leaf: bool,
    pub variable_index: Option<usize>,
    pub children: Vec<TreeNode>,
}

fn parse_array_index(label: &str) -> Option<u64> {
    let s = label.strip_prefix('[')?.strip_suffix(']')?;
    s.parse().ok()
}

fn cmp_tree_label(a: &str, b: &str) -> Ordering {
    match (parse_array_index(a), parse_array_index(b)) {
        (Some(ia), Some(ib)) => ia.cmp(&ib),
        _ => a.cmp(b),
    }
}

fn sort_tree_children(nodes: &mut Vec<TreeNode>) {
    nodes.sort_by(|a, b| cmp_tree_label(&a.label, &b.label));
    for node in nodes.iter_mut() {
        sort_tree_children(&mut node.children);
    }
}

fn join_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else if segment.starts_with('[') {
        format!("{}{}", parent, segment)
    } else {
        format!("{}.{}", parent, segment)
    }
}

fn insert_path(
    nodes: &mut Vec<TreeNode>,
    segments: &[String],
    var_index: usize,
    full_path: &str,
    parent_path: &str,
) {
    let seg = &segments[0];
    let is_last = segments.len() == 1;
    let path = join_path(parent_path, seg);
    let pos = if let Some(p) = nodes.iter().position(|n| n.label == *seg) {
        p
    } else {
        nodes.push(TreeNode {
            label: seg.clone(),
            path: if is_last {
                full_path.to_string()
            } else {
                path.clone()
            },
            is_leaf: is_last,
            variable_index: if is_last { Some(var_index) } else { None },
            children: Vec::new(),
        });
        nodes.len() - 1
    };

    if is_last {
        nodes[pos].path = full_path.to_string();
        nodes[pos].variable_index = Some(var_index);
        if nodes[pos].children.is_empty() {
            nodes[pos].is_leaf = true;
        }
    } else {
        nodes[pos].is_leaf = false;
        nodes[pos].variable_index = None;
        let child_parent = nodes[pos].path.clone();
        insert_path(
            &mut nodes[pos].children,
            &segments[1..],
            var_index,
            full_path,
            &child_parent,
        );
    }
}

/// Build a collapsible tree from flat ELF variable leaves using path segments.
pub fn build_var_tree(variables: &[ElfVariable]) -> Vec<TreeNode> {
    let mut roots = Vec::new();
    for (idx, var) in variables.iter().enumerate() {
        let segments = split_var_path(&var.path);
        if segments.is_empty() || segments[0].is_empty() {
            continue;
        }
        insert_path(&mut roots, &segments, idx, &var.path, "");
    }
    sort_tree_children(&mut roots);
    roots
}

/// Return a pruned copy of the tree: keep nodes that match `query` (case-insensitive
/// against `path` or `label`) or have a matching descendant.
/// Empty query returns a full clone.
pub fn filter_tree(nodes: &[TreeNode], query: &str) -> Vec<TreeNode> {
    if query.is_empty() {
        return nodes.to_vec();
    }
    let q = query.to_lowercase();
    nodes.iter().filter_map(|n| filter_node(n, &q)).collect()
}

fn filter_node(node: &TreeNode, q: &str) -> Option<TreeNode> {
    let self_match =
        node.path.to_lowercase().contains(q) || node.label.to_lowercase().contains(q);

    if node.children.is_empty() {
        return if self_match {
            Some(node.clone())
        } else {
            None
        };
    }

    let filtered_children: Vec<TreeNode> = node
        .children
        .iter()
        .filter_map(|c| filter_node(c, q))
        .collect();

    if self_match {
        // Container name matched: show full children so structure is browsable
        Some(TreeNode {
            label: node.label.clone(),
            path: node.path.clone(),
            is_leaf: false,
            variable_index: None,
            children: node.children.clone(),
        })
    } else if !filtered_children.is_empty() {
        Some(TreeNode {
            label: node.label.clone(),
            path: node.path.clone(),
            is_leaf: false,
            variable_index: None,
            children: filtered_children,
        })
    } else {
        None
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
