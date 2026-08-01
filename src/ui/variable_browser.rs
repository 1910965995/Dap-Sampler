use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
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

/// Display text for a tree node.
///
/// Array index segments (`[0]`) are shown as `array_name[0]` (using the
/// last identifier before the index in `path`), not bare `[0]`.
pub fn node_display_label(label: &str, path: &str) -> String {
    if !label.starts_with('[') {
        return label.to_string();
    }
    if let Some(prefix) = path.strip_suffix(label) {
        if prefix.is_empty() {
            return label.to_string();
        }
        if let Some(dot) = prefix.rfind('.') {
            format!("{}{}", &prefix[dot + 1..], label)
        } else {
            format!("{}{}", prefix, label)
        }
    } else {
        // Fallback: keep path if it already looks right
        path.to_string()
    }
}

fn cmp_tree_label(a: &str, b: &str) -> Ordering {
    match (parse_array_index(a), parse_array_index(b)) {
        (Some(ia), Some(ib)) => ia.cmp(&ib),
        _ => a.cmp(b),
    }
}

fn sort_tree_children(nodes: &mut [TreeNode]) {
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
    /// 已勾选的变量路径（与通道面板联动，仅叶子路径）
    pub checked_paths: HashSet<String>,
    /// 用户展开的容器路径（搜索强制展开时不写入此集合）
    expanded: HashSet<String>,
    /// 分组树缓存：每个 source_file 分组一棵已构建的树。
    /// ELF 未变化时直接复用，避免每帧重建（数千变量时每帧 clone + 建树开销可观）
    tree_cache: Option<Vec<(String, Vec<TreeNode>)>>,
    /// 树缓存对应的 ELF 文件路径（检测 ELF 是否变化）
    cache_key: Option<String>,
    /// 搜索过滤结果缓存（仅搜索词变化时重建过滤）
    filtered: Vec<(String, Vec<TreeNode>)>,
    /// 过滤缓存对应的搜索词
    filter_query: String,
}

impl VariableBrowser {
    pub fn new() -> Self {
        Self {
            search_text: String::new(),
            checked_paths: HashSet::new(),
            expanded: HashSet::new(),
            tree_cache: None,
            cache_key: None,
            filtered: Vec::new(),
            filter_query: String::new(),
        }
    }

    /// 渲染变量浏览器 UI
    ///
    /// 返回 `Vec<(path, is_checked)>` 表示本帧用户勾选/取消的变更（仅叶子路径）。
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        elf_ctx: Option<&ElfContext>,
    ) -> Vec<(String, bool)> {
        if elf_ctx.is_none() {
            // 与 Cursor 面板提示保持一致的灰色
            ui.label(
                egui::RichText::new("No ELF file loaded")
                    .size(13.0)
                    .color(egui::Color32::from_rgb(0x8b, 0x94, 0x9e)),
            );
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

        // ELF 变化 → 重建分组树缓存（一次性成本，后续帧直接复用）
        if self.cache_key.as_deref() != Some(ctx.file_path.as_str()) {
            self.rebuild_cache(ctx);
        }
        // 搜索词变化 → 重建过滤缓存
        if self.search_text != self.filter_query {
            self.rebuild_filter_cache();
        }

        let force_expand = !self.search_text.is_empty();

        // 无搜索时直接渲染完整树缓存（零拷贝借用）；搜索时渲染过滤缓存
        let visible_groups: &[(String, Vec<TreeNode>)] = if self.search_text.is_empty() {
            match self.tree_cache.as_ref() {
                Some(groups) => groups,
                None => return changes,
            }
        } else {
            &self.filtered
        };

        egui::ScrollArea::vertical()
            .id_salt("variable_scroll")
            .auto_shrink([false, true])
            .max_height(400.0)
            .show(ui, |ui| {
                for (g, roots) in visible_groups {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(g)
                            .small()
                            .color(egui::Color32::from_rgb(110, 118, 129)),
                    );

                    for node in roots {
                        render_node(
                            ui,
                            node,
                            force_expand,
                            &mut self.expanded,
                            &mut self.checked_paths,
                            &mut changes,
                        );
                    }
                }
            });

        changes
    }

    /// 按 source_file 分组并构建每组的变量树（仅 ELF 变化时调用）。
    ///
    /// 结果缓存于 tree_cache；ELF 未变化时 show() 直接复用，避免每帧
    /// clone 全部变量 + 重建树。
    fn rebuild_cache(&mut self, ctx: &ElfContext) {
        // 按 source_file 首次出现顺序分组
        let mut group_order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<ElfVariable>> = HashMap::new();
        for var in &ctx.variables {
            let g = var
                .source_file
                .clone()
                .unwrap_or_else(|| "Globals".to_string());
            if !groups.contains_key(&g) {
                group_order.push(g.clone());
            }
            groups.entry(g).or_default().push(var.clone());
        }

        // 每个分组构建一次树（树构建含排序，仅在此处执行一次）
        let tree_groups: Vec<(String, Vec<TreeNode>)> = group_order
            .into_iter()
            .filter_map(|g| groups.remove(&g).map(|vars| (g, build_var_tree(&vars))))
            .collect();

        self.cache_key = Some(ctx.file_path.clone());
        self.tree_cache = Some(tree_groups);
        // 过滤缓存失效（下一帧按新树重建）
        self.filter_query.clear();
        self.filtered.clear();
    }

    /// 重建搜索过滤缓存（搜索词变化时调用）。
    ///
    /// 无搜索时清空过滤缓存（show 直接渲染完整树）；有搜索时对每组
    /// 树执行 filter_tree 并丢弃无匹配的分组。
    fn rebuild_filter_cache(&mut self) {
        self.filter_query = self.search_text.clone();
        self.filtered.clear();
        if self.search_text.is_empty() {
            return;
        }
        if let Some(groups) = self.tree_cache.as_ref() {
            for (g, roots) in groups {
                let f = filter_tree(roots, &self.search_text);
                if !f.is_empty() {
                    self.filtered.push((g.clone(), f));
                }
            }
        }
    }

    /// 获取已勾选的路径集合
    pub fn checked(&self) -> &HashSet<String> {
        &self.checked_paths
    }

    /// 清空选择与展开状态
    pub fn clear(&mut self) {
        self.checked_paths.clear();
        self.expanded.clear();
    }
}

impl Default for VariableBrowser {
    fn default() -> Self {
        Self::new()
    }
}

/// 递归渲染树节点：容器仅展开/折叠；叶子为复选框
///
/// 树数据只读（借用 show() 中的缓存），展开/勾选状态通过参数传入，
/// 避免与缓存产生借用冲突（渲染时 tree_cache 为不可变借用）。
fn render_node(
    ui: &mut egui::Ui,
    node: &TreeNode,
    force_expand: bool,
    expanded: &mut HashSet<String>,
    checked_paths: &mut HashSet<String>,
    changes: &mut Vec<(String, bool)>,
) {
    let display = node_display_label(&node.label, &node.path);

    // 有子节点即视为容器（不显示复选框）
    if !node.children.is_empty() {
        let mut is_expanded = force_expand || expanded.contains(&node.path);

        ui.horizontal(|ui| {
            // 占位收窄（约 2.4 个字母宽）保证名称左对齐；
            // 三角按完整 icon 尺寸绘制，避免跟着占位缩小看不清
            let font_id = egui::TextStyle::Body.resolve(ui.style());
            let letter_w = ui.fonts(|f| f.glyph_width(&font_id, 'M'));
            let full_icon_w = ui.spacing().icon_width;
            let slot_w = (full_icon_w - letter_w * 2.4).max(6.0);
            let icon_size = egui::vec2(slot_w, full_icon_w);
            let (icon_rect, icon_resp) =
                ui.allocate_exact_size(icon_size, egui::Sense::click());

            let tri_color = ui.visuals().text_color();
            let c = icon_rect.center();
            let s = full_icon_w * 0.22 * 1.1;
            let points = if is_expanded {
                // ▼
                vec![
                    egui::pos2(c.x - s, c.y - s * 0.6),
                    egui::pos2(c.x + s, c.y - s * 0.6),
                    egui::pos2(c.x, c.y + s * 0.8),
                ]
            } else {
                // ▶
                vec![
                    egui::pos2(c.x - s * 0.5, c.y - s),
                    egui::pos2(c.x - s * 0.5, c.y + s),
                    egui::pos2(c.x + s * 0.9, c.y),
                ]
            };
            ui.painter().add(egui::Shape::convex_polygon(
                points,
                tri_color,
                egui::Stroke::NONE,
            ));

            let r_label = ui.selectable_label(false, &display);
            if (icon_resp.clicked() || r_label.clicked()) && !force_expand {
                if is_expanded {
                    expanded.remove(&node.path);
                    is_expanded = false;
                } else {
                    expanded.insert(node.path.clone());
                    is_expanded = true;
                }
            }
        });

        if is_expanded {
            let id = ui.make_persistent_id(("var_tree", &node.path));
            ui.indent(id, |ui| {
                for child in &node.children {
                    render_node(ui, child, force_expand, expanded, checked_paths, changes);
                }
            });
        }
    } else {
        // 叶子：复选框 + 标签（路径用于勾选状态；数组元素显示为 name[i]）
        ui.horizontal(|ui| {
            let mut is_checked = checked_paths.contains(&node.path);
            if ui.checkbox(&mut is_checked, &display).changed() {
                if is_checked {
                    checked_paths.insert(node.path.clone());
                } else {
                    checked_paths.remove(&node.path);
                }
                changes.push((node.path.clone(), is_checked));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::ValueType;

    fn var(name: &str, source: Option<&str>) -> ElfVariable {
        ElfVariable {
            name: name.to_string(),
            path: name.to_string(),
            address: 0x100,
            byte_size: 4,
            value_type: ValueType::Float,
            parent_path: None,
            source_file: source.map(|s| s.to_string()),
        }
    }

    fn sample_ctx() -> ElfContext {
        ElfContext {
            file_path: "test.elf".to_string(),
            variables: vec![
                var("a", Some("main.c")),
                var("b", Some("util.c")),
                var("c", None),
            ],
        }
    }

    #[test]
    fn rebuild_cache_groups_by_source_file() {
        let mut browser = VariableBrowser::new();
        browser.rebuild_cache(&sample_ctx());

        let groups = browser.tree_cache.as_ref().unwrap();
        // 分组顺序按首次出现：main.c, util.c, Globals
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, "main.c");
        assert_eq!(groups[1].0, "util.c");
        assert_eq!(groups[2].0, "Globals");
        // 每个分组一棵树，各 1 个叶子
        assert_eq!(groups[0].1.len(), 1);
        assert!(groups[0].1[0].is_leaf);
        assert_eq!(groups[2].1[0].label, "c");
    }

    #[test]
    fn rebuild_cache_detects_elf_change() {
        let mut browser = VariableBrowser::new();
        browser.rebuild_cache(&sample_ctx());
        assert_eq!(browser.cache_key.as_deref(), Some("test.elf"));
        assert_eq!(browser.tree_cache.as_ref().unwrap().len(), 3);

        // 不同 ELF → 重建（缓存内容替换）
        let ctx2 = ElfContext { file_path: "other.elf".to_string(), variables: vec![] };
        browser.rebuild_cache(&ctx2);
        assert_eq!(browser.cache_key.as_deref(), Some("other.elf"));
        assert!(browser.tree_cache.as_ref().unwrap().is_empty());
    }

    #[test]
    fn rebuild_filter_cache_filters_and_skips_empty_groups() {
        let mut browser = VariableBrowser::new();
        browser.rebuild_cache(&sample_ctx());

        // 搜索 "a" → 仅 main.c 分组命中
        browser.search_text = "a".to_string();
        browser.rebuild_filter_cache();
        assert_eq!(browser.filtered.len(), 1);
        assert_eq!(browser.filtered[0].0, "main.c");
        assert_eq!(browser.filtered[0].1[0].label, "a");

        // 搜索 "b" → 仅 util.c
        browser.search_text = "b".to_string();
        browser.rebuild_filter_cache();
        assert_eq!(browser.filtered.len(), 1);
        assert_eq!(browser.filtered[0].0, "util.c");

        // 无匹配 → 空
        browser.search_text = "zzz".to_string();
        browser.rebuild_filter_cache();
        assert!(browser.filtered.is_empty());

        // 清空搜索 → 过滤缓存清空（show 直接渲染完整树）
        browser.search_text.clear();
        browser.rebuild_filter_cache();
        assert!(browser.filtered.is_empty());
        assert_eq!(browser.filter_query, "");
    }
}
