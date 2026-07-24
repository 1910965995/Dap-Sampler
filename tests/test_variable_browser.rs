use dap_sampler::elf::ElfVariable;
use dap_sampler::pipeline::sample::ValueType;
use dap_sampler::ui::variable_browser::{
    build_var_tree, filter_tree, split_var_path, TreeNode,
};

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

fn collect_leaves(n: &TreeNode, out: &mut Vec<String>) {
    if n.is_leaf {
        out.push(n.path.clone());
    }
    for c in &n.children {
        collect_leaves(c, out);
    }
}

#[test]
fn leaf_paths_match_original_variables() {
    let vars = vec![
        leaf("pid.kp", 1),
        leaf("buffer[0]", 2),
        leaf("temperature", 3),
    ];
    let roots = build_var_tree(&vars);
    let mut paths = Vec::new();
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

#[test]
fn filter_preserves_leaf_checkbox_path() {
    let vars = vec![
        leaf("pid.kp", 1),
        leaf("pid.ki", 2),
        leaf("buffer[0]", 3),
        leaf("temperature", 4),
    ];
    let roots = build_var_tree(&vars);
    let filtered = filter_tree(&roots, "kp");
    let mut paths = Vec::new();
    for r in &filtered {
        collect_leaves(r, &mut paths);
    }
    assert_eq!(paths, vec!["pid.kp".to_string()]);
}

use dap_sampler::ui::variable_browser::node_display_label;

#[test]
fn display_label_plain() {
    assert_eq!(node_display_label("kp", "pid.kp"), "kp");
}

#[test]
fn display_label_array_element() {
    assert_eq!(node_display_label("[0]", "buffer[0]"), "buffer[0]");
}

#[test]
fn display_label_nested_array_element() {
    assert_eq!(node_display_label("[1]", "motors[1]"), "motors[1]");
}

#[test]
fn display_label_array_of_struct_member() {
    // leaf member still shows member name
    assert_eq!(node_display_label("speed", "motors[1].speed"), "speed");
}
