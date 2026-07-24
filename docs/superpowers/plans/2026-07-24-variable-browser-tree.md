# Variable Browser Tree UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the left-panel variable browser show structs/arrays as a collapsible tree (leaf-only checkboxes), rebuilt from existing flat ELF paths.

**Architecture:** Keep `ElfContext.variables` as a flat leaf list. In `variable_browser.rs`, add pure helpers (`split_var_path`, `build_var_tree`, `filter_tree`) that turn paths like `pid.kp` / `buffer[0]` into a `TreeNode` forest, then render with egui expand/collapse. Search prunes to matching branches and force-expands ancestors without permanently mutating user expand state.

**Tech Stack:** Rust 2021, egui 0.31, existing `ElfVariable` / integration tests under `tests/`.

**Spec:** `docs/superpowers/specs/2026-07-24-variable-browser-tree-design.md`

---

## File structure

| File | Role |
|------|------|
| `src/ui/variable_browser.rs` | Path split, tree build, search filter, expand state, tree UI |
| `tests/test_variable_browser.rs` | Integration tests for pure helpers + checkbox change behavior |
| `src/ui/app.rs` | No change expected (`show()` still returns `Vec<(String, bool)>`) |
| `src/elf/*` | No change |

---

### Task 1: Path splitting (`split_var_path`)

**Files:**
- Modify: `src/ui/variable_browser.rs`
- Create: `tests/test_variable_browser.rs`

- [ ] **Step 1: Write the failing tests**

Create `tests/test_variable_browser.rs`:

```rust
use dap_sampler::ui::variable_browser::split_var_path;

#[test]
fn split_simple_name() {
    assert_eq!(split_var_path("temperature"), vec!["temperature"]);
}

#[test]
fn split_struct_member() {
    assert_eq!(split_var_path("pid.kp"), vec!["pid", "kp"]);
}

#[test]
fn split_array_index() {
    assert_eq!(split_var_path("buffer[0]"), vec!["buffer", "[0]"]);
}

#[test]
fn split_nested_array_member() {
    assert_eq!(
        split_var_path("motors[1].speed"),
        vec!["motors", "[1]", "speed"]
    );
}

#[test]
fn split_multi_member() {
    assert_eq!(split_var_path("a.b.c"), vec!["a", "b", "c"]);
}

#[test]
fn split_empty_falls_back_to_single_empty() {
    // empty input: return single empty segment (caller may skip)
    assert_eq!(split_var_path(""), vec![""]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_variable_browser split_ -- --nocapture`

Expected: compile fail — `split_var_path` not found / module path wrong.

- [ ] **Step 3: Implement `split_var_path`**

In `src/ui/variable_browser.rs`, add (keep existing `VariableBrowser` code; append helpers above or below the impl):

```rust
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
                // consume through matching ']'
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b']' {
                    i += 1; // include ']'
                    segments.push(path[start..i].to_string());
                } else {
                    // no closing bracket — treat rest as one segment
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test test_variable_browser split_ -- --nocapture`

Expected: all `split_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/variable_browser.rs tests/test_variable_browser.rs
git commit -m "feat(ui): add split_var_path for variable tree segments"
```

---

### Task 2: Tree node type + `build_var_tree`

**Files:**
- Modify: `src/ui/variable_browser.rs`
- Modify: `tests/test_variable_browser.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_variable_browser.rs`:

```rust
use dap_sampler::elf::ElfVariable;
use dap_sampler::pipeline::sample::ValueType;
use dap_sampler::ui::variable_browser::{build_var_tree, TreeNode};

fn leaf(path: &str, address: u32) -> ElfVariable {
    let segs = dap_sampler::ui::variable_browser::split_var_path(path);
    let name = segs.last().cloned().unwrap_or_else(|| path.to_string());
    ElfVariable {
        name,
        path: path.to_string(),
        address,
        byte_size: 4,
        value_type: ValueType::Float,
        parent_path: None,
        source_file: None,
    }
}

fn find_child<'a>(node: &'a TreeNode, label: &str) -> Option<&'a TreeNode> {
    node.children.iter().find(|c| c.label == label)
}

#[test]
fn build_tree_struct_members() {
    let vars = vec![
        leaf("pid.kp", 0x20000300),
        leaf("pid.ki", 0x20000304),
        leaf("pid.kd", 0x20000308),
    ];
    let roots = build_var_tree(&vars);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].label, "pid");
    assert!(!roots[0].is_leaf);
    assert_eq!(roots[0].path, "pid");
    assert_eq!(roots[0].children.len(), 3);
    assert!(roots[0].children.iter().all(|c| c.is_leaf));
    let kp = find_child(&roots[0], "kp").unwrap();
    assert_eq!(kp.path, "pid.kp");
    assert_eq!(kp.variable_index, Some(0));
}

#[test]
fn build_tree_array_elements() {
    let vars = vec![
        leaf("buffer[0]", 0x20000400),
        leaf("buffer[1]", 0x20000404),
        leaf("buffer[2]", 0x20000408),
        leaf("buffer[3]", 0x2000040C),
    ];
    let roots = build_var_tree(&vars);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].label, "buffer");
    assert_eq!(roots[0].children.len(), 4);
    assert_eq!(roots[0].children[0].label, "[0]");
    assert_eq!(roots[0].children[0].path, "buffer[0]");
    assert!(roots[0].children[0].is_leaf);
}

#[test]
fn build_tree_nested() {
    let vars = vec![leaf("motors[1].speed", 0x20001000)];
    let roots = build_var_tree(&vars);
    assert_eq!(roots[0].label, "motors");
    let idx = find_child(&roots[0], "[1]").unwrap();
    assert!(!idx.is_leaf);
    let speed = find_child(idx, "speed").unwrap();
    assert!(speed.is_leaf);
    assert_eq!(speed.path, "motors[1].speed");
}

#[test]
fn build_tree_array_index_numeric_sort() {
    let vars = vec![
        leaf("buf[10]", 0x10),
        leaf("buf[2]", 0x2),
        leaf("buf[1]", 0x1),
    ];
    let roots = build_var_tree(&vars);
    let labels: Vec<_> = roots[0].children.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["[1]", "[2]", "[10]"]);
}

#[test]
fn build_tree_plain_and_struct_siblings() {
    let vars = vec![
        leaf("temperature", 0x100),
        leaf("pid.kp", 0x200),
    ];
    let roots = build_var_tree(&vars);
    // sorted by label: pid, temperature
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0].label, "pid");
    assert_eq!(roots[1].label, "temperature");
    assert!(roots[1].is_leaf);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_variable_browser build_tree_ -- --nocapture`

Expected: FAIL — `build_var_tree` / `TreeNode` missing.

- [ ] **Step 3: Implement `TreeNode`, sort helper, and `build_var_tree`**

Add to `src/ui/variable_browser.rs`:

```rust
/// One node in the variable browser tree (UI-only; rebuilt from flat leaves).
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub label: String,
    pub path: String,
    pub is_leaf: bool,
    /// Index into the source `variables` slice for leaves; `None` for containers.
    pub variable_index: Option<usize>,
    pub children: Vec<TreeNode>,
}

fn parse_array_index(label: &str) -> Option<u64> {
    let s = label.strip_prefix('[')?.strip_suffix(']')?;
    s.parse().ok()
}

fn cmp_tree_label(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_array_index(a), parse_array_index(b)) {
        (Some(ia), Some(ib)) => ia.cmp(&ib),
        _ => a.cmp(b),
    }
}

fn sort_tree_children(nodes: &mut [TreeNode]) {
    nodes.sort_by(|a, b| cmp_tree_label(&a.label, &b.label));
    for n in nodes.iter_mut() {
        sort_tree_children(&mut n.children);
    }
}

/// Join parent path + next segment (`.` for members, nothing before `[i]`).
fn join_path(parent: &str, segment: &str) -> String {
    if parent.is_empty() {
        segment.to_string()
    } else if segment.starts_with('[') {
        format!("{}{}", parent, segment)
    } else {
        format!("{}.{}", parent, segment)
    }
}

/// Build a forest of tree nodes from a flat leaf variable list (order by path insert).
pub fn build_var_tree(variables: &[crate::elf::ElfVariable]) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();

    for (idx, var) in variables.iter().enumerate() {
        let segments = split_var_path(&var.path);
        if segments.is_empty() || (segments.len() == 1 && segments[0].is_empty()) {
            continue;
        }

        let mut children_ref = &mut roots;
        let mut prefix = String::new();

        for (si, seg) in segments.iter().enumerate() {
            let is_last = si + 1 == segments.len();
            prefix = join_path(&prefix, seg);

            let pos = children_ref.iter().position(|n| n.label == *seg);
            let pos = match pos {
                Some(p) => p,
                None => {
                    children_ref.push(TreeNode {
                        label: seg.clone(),
                        path: prefix.clone(),
                        is_leaf: is_last,
                        variable_index: if is_last { Some(idx) } else { None },
                        children: Vec::new(),
                    });
                    children_ref.len() - 1
                }
            };

            if is_last {
                // Ensure leaf metadata if node already existed as container (shouldn't)
                children_ref[pos].is_leaf = children_ref[pos].children.is_empty();
                if children_ref[pos].is_leaf {
                    children_ref[pos].variable_index = Some(idx);
                    children_ref[pos].path = var.path.clone();
                }
            } else {
                children_ref[pos].is_leaf = false;
                children_ref[pos].variable_index = None;
                children_ref = &mut children_ref[pos].children;
            }
        }
    }

    sort_tree_children(&mut roots);
    roots
}
```

**Important:** The nested `children_ref` reborrow pattern above may not compile in Rust (multiple mutable borrows through index). Prefer an index-based insert without holding nested mutable refs, e.g. recursive helper:

```rust
fn insert_leaf(nodes: &mut Vec<TreeNode>, segments: &[String], full_path: &str, var_index: usize) {
    if segments.is_empty() {
        return;
    }
    let seg = &segments[0];
    let is_last = segments.len() == 1;
    let path = /* compute from walking — pass parent_path */;
    // find or create node for seg, then recurse into children with &segments[1..]
}

pub fn build_var_tree(variables: &[crate::elf::ElfVariable]) -> Vec<TreeNode> {
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
            path: if is_last { full_path.to_string() } else { path.clone() },
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
        insert_path(&mut nodes[pos].children, &segments[1..], var_index, full_path, &child_parent);
    }
}
```

Use the recursive `insert_path` version in the actual implementation (compiles cleanly).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test test_variable_browser build_tree_ -- --nocapture`

Expected: all PASS. Fix sort/path join if any assertion fails.

- [ ] **Step 5: Commit**

```bash
git add src/ui/variable_browser.rs tests/test_variable_browser.rs
git commit -m "feat(ui): build collapsible variable tree from flat ELF paths"
```

---

### Task 3: Search filter (`filter_tree`)

**Files:**
- Modify: `src/ui/variable_browser.rs`
- Modify: `tests/test_variable_browser.rs`

- [ ] **Step 1: Write the failing tests**

```rust
use dap_sampler::ui::variable_browser::filter_tree;

#[test]
fn filter_keeps_ancestors_of_match() {
    let vars = vec![
        leaf("pid.kp", 0x1),
        leaf("pid.ki", 0x2),
        leaf("buffer[0]", 0x3),
        leaf("temperature", 0x4),
    ];
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "kp");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].label, "pid");
    assert_eq!(filtered[0].children.len(), 1);
    assert_eq!(filtered[0].children[0].label, "kp");
}

#[test]
fn filter_case_insensitive() {
    let vars = vec![leaf("pid.Kp", 0x1)];
    // path is pid.Kp — match with lowercase query
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "kp");
    assert_eq!(filtered.len(), 1);
}

#[test]
fn filter_empty_query_clones_all() {
    let vars = vec![leaf("a", 0x1), leaf("b", 0x2)];
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "");
    assert_eq!(filtered.len(), roots.len());
}

#[test]
fn filter_matches_path_substring() {
    let vars = vec![leaf("motors[1].speed", 0x1)];
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "motors[1]");
    assert_eq!(filtered.len(), 1);
    assert!(!filtered[0].children.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test test_variable_browser filter_ -- --nocapture`

Expected: FAIL — `filter_tree` missing.

- [ ] **Step 3: Implement `filter_tree`**

```rust
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

    let filtered_children: Vec<TreeNode> = node
        .children
        .iter()
        .filter_map(|c| filter_node(c, q))
        .collect();

    if self_match || !filtered_children.is_empty() {
        Some(TreeNode {
            label: node.label.clone(),
            path: node.path.clone(),
            is_leaf: node.is_leaf && filtered_children.is_empty(),
            variable_index: node.variable_index,
            children: if self_match && filtered_children.is_empty() && !node.children.is_empty() {
                // self matched container: keep all original children so user can browse
                node.children.clone()
            } else if self_match && !node.children.is_empty() && filtered_children.is_empty() {
                node.children.clone()
            } else {
                filtered_children
            },
            // Spec: keep matching leaves and ancestors. If only descendants match,
            // children = filtered_children. If container label matches, still OK to
            // show only filtered descendants OR full children — prefer filtered when
            // descendants also filtered; when self_match on container with no child
            // match, show all children.
        })
    } else {
        None
    }
}
```

**Clarify container self-match behavior (spec-aligned, simpler):**

Prefer this cleaner rule (implement this, not the nested if above):

```rust
fn filter_node(node: &TreeNode, q: &str) -> Option<TreeNode> {
    let self_match =
        node.path.to_lowercase().contains(q) || node.label.to_lowercase().contains(q);

    if node.children.is_empty() {
        return if self_match { Some(node.clone()) } else { None };
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
```

For leaf `self_match`, first branch returns clone. Adjust tests if container self-match shows all children (e.g. query `"pid"` shows kp/ki/kd).

Add optional test:

```rust
#[test]
fn filter_container_name_shows_all_children() {
    let vars = vec![
        leaf("pid.kp", 0x1),
        leaf("pid.ki", 0x2),
        leaf("other.x", 0x3),
    ];
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "pid");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].children.len(), 2);
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test test_variable_browser filter_ -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ui/variable_browser.rs tests/test_variable_browser.rs
git commit -m "feat(ui): filter variable tree branches for search"
```

---

### Task 4: Wire tree rendering into `VariableBrowser::show`

**Files:**
- Modify: `src/ui/variable_browser.rs` (struct + `show`)

- [ ] **Step 1: Extend `VariableBrowser` state**

```rust
pub struct VariableBrowser {
    search_text: String,
    pub checked_paths: HashSet<String>,
    /// User-expanded container paths (not used while search is active for force-expand).
    expanded: HashSet<String>,
}

impl VariableBrowser {
    pub fn new() -> Self {
        Self {
            search_text: String::new(),
            checked_paths: HashSet::new(),
            expanded: HashSet::new(),
        }
    }
}
```

- [ ] **Step 2: Replace flat list rendering in `show`**

Keep search box, counts, separator, ScrollArea. Inside ScrollArea:

1. Group `ctx.variables` by `source_file` (same as today: `unwrap_or_else(|| "Globals".to_string())`).
2. For each group, collect indices or clone references of variables in that group.
3. `let roots = build_var_tree(group_vars)` — easiest approach: partition into `Vec<Vec<&ElfVariable>>` or build from filtered flat list **per group**.

**Grouping approach:**

```rust
// Build ordered group list
use std::collections::BTreeMap;
let mut groups: BTreeMap<String, Vec<&ElfVariable>> = BTreeMap::new();
// preserve first-seen order of groups: use Vec<(String, Vec<usize>)> instead if order matters
```

Match existing behavior: iterate variables in order, emit group header when `source_file` changes. So:

```rust
// 1) Optionally filter leaves by search for grouping input?
// Spec: filter on tree after build. Build full tree per group from all group leaves,
// then filter_tree if search non-empty.

let mut current_group: Option<String> = None;
let mut group_buffer: Vec<&ElfVariable> = Vec::new();

let flush_group = |ui: &mut egui::Ui, group_name: &str, buf: &[&ElfVariable], ...| {
    if buf.is_empty() { return; }
    ui.add_space(4.0);
    ui.label(egui::RichText::new(group_name).small().color(...));
    let owned: Vec<ElfVariable> = buf.iter().map(|v| (*v).clone()).collect();
    // ElfVariable is Clone — already #[derive(Clone)]
    let mut roots = build_var_tree(&owned);
    // Problem: variable_index points into `owned`, not ctx.variables.
    // For checkbox we only need path from TreeNode; variable_index optional for show.
    if !self.search_text.is_empty() {
        roots = filter_tree(&roots, &self.search_text);
    }
    let force_expand = !self.search_text.is_empty();
    for node in &roots {
        self.render_node(ui, node, force_expand, &mut changes);
    }
};
```

**Index issue:** `build_var_tree` stores index into the slice passed in. For UI we only need `node.path` for checkboxes. That is enough — do not require global index in `show`.

Implement `render_node`:

```rust
fn render_node(
    &mut self,
    ui: &mut egui::Ui,
    node: &TreeNode,
    force_expand: bool,
    changes: &mut Vec<(String, bool)>,
) {
    if node.is_leaf || node.children.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let mut is_checked = self.checked_paths.contains(&node.path);
            if ui.checkbox(&mut is_checked, &node.label).changed() {
                if is_checked {
                    self.checked_paths.insert(node.path.clone());
                } else {
                    self.checked_paths.remove(&node.path);
                }
                changes.push((node.path.clone(), is_checked));
            }
        });
        return;
    }

    let expanded = force_expand || self.expanded.contains(&node.path);
    let id = ui.make_persistent_id(("var_tree", &node.path));
    // Manual header so we don't put a checkbox on the parent:
    ui.horizontal(|ui| {
        let icon = if expanded { "▼" } else { "▶" };
        if ui
            .add(egui::Label::new(format!("{} {}", icon, node.label)).sense(egui::Sense::click()))
            .clicked()
        {
            if !force_expand {
                if expanded {
                    self.expanded.remove(&node.path);
                } else {
                    self.expanded.insert(node.path.clone());
                }
            }
        }
    });
    if expanded {
        ui.indent(id, |ui| {
            for child in &node.children {
                self.render_node(ui, child, force_expand, changes);
            }
        });
    }
}
```

**Note:** When `force_expand` is true, clicking parent should not toggle `expanded` (or can no-op). When search clears, previous `expanded` restored automatically.

Remove old flat-loop indent logic (`parent_path` based).

- [ ] **Step 3: Compile**

Run: `cargo build`

Expected: success. Fix borrow issues (`render_node` may need to collect expand toggles differently if borrow checker complains — e.g. return expand path toggles).

If `ui.indent` API differs in egui 0.31, use:

```rust
ui.horizontal(|ui| { /* icon+label */ });
if expanded {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.vertical(|ui| {
            for child in &node.children {
                self.render_node(ui, child, force_expand, changes);
            }
        });
    });
}
```

- [ ] **Step 4: Run full test suite**

Run: `cargo test`

Expected: all tests PASS (including prior suites).

- [ ] **Step 5: Commit**

```bash
git add src/ui/variable_browser.rs
git commit -m "feat(ui): render variable browser as collapsible tree"
```

---

### Task 5: Grouping + search integration tests; smoke checklist

**Files:**
- Modify: `tests/test_variable_browser.rs` if needed
- Optionally small unit test that multi-group build doesn't mix paths

- [ ] **Step 1: Add regression test for leaf checkbox path stability**

```rust
#[test]
fn leaf_paths_match_original_variables() {
    let vars = vec![
        leaf("pid.kp", 1),
        leaf("buffer[0]", 2),
        leaf("temperature", 3),
    ];
    let roots = build_var_tree(&vars);
    let mut paths = Vec::new();
    fn collect_leaves(n: &TreeNode, out: &mut Vec<String>) {
        if n.is_leaf {
            out.push(n.path.clone());
        }
        for c in &n.children {
            collect_leaves(c, out);
        }
    }
    for r in &roots {
        collect_leaves(r, &mut paths);
    }
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "buffer[0]".to_string(),
            "pid.kp".to_string(),
            "temperature".to_string()
        ]
    );
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test`

Expected: PASS.

- [ ] **Step 3: Manual GUI smoke (if hardware/ELF available)**

1. `cargo run -- gui` (or release binary)
2. Load ELF with struct + array globals
3. Confirm collapsed by default; expand `pid` / array
4. Check one leaf → appears in channels
5. Search member name → only branch visible, expanded
6. Clear search → tree returns; expand state for previously user-expanded nodes retained

- [ ] **Step 4: Final commit if any test-only fixes**

```bash
git add tests/test_variable_browser.rs src/ui/variable_browser.rs
git commit -m "test(ui): cover variable tree leaf paths and search branches"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|------------------|------|
| UI rebuild tree from flat paths | Task 2, 4 |
| Leaf-only checkbox | Task 4 `render_node` |
| Expand/collapse, default collapsed | Task 4 `expanded` HashSet |
| Search filter + auto-expand ancestors | Task 3 + Task 4 `force_expand` |
| Group by source_file | Task 4 flush_group |
| Path segments `.` and `[i]` | Task 1 |
| Numeric array sort | Task 2 |
| No elf/tree.rs changes | All tasks |
| app.rs change API unchanged | Task 4 keeps return type |
| Tests for split/build/filter | Tasks 1–3, 5 |

## Notes for implementers

- `ElfVariable` is already `Clone` (`#[derive(Debug, Clone)]` in `parser.rs`).
- Do not mark non-leaf paths as checked; never insert container paths into `checked_paths`.
- While search is active, do not write force-expanded paths into `expanded`.
- Prefer recursive `insert_path` over nested `&mut` reborrows.
- Keep Chinese UI strings/comments style consistent with surrounding file.
