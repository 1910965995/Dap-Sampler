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
