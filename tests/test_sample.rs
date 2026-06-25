/// 测试 pipeline::sample::Sample 结构体
use dap_sampler::pipeline::sample::Sample;

// ================================================================
// timestamp_sec 字段
// ================================================================

#[test]
fn timestamp_sec_field_basic() {
    let sample = Sample { seq: 0, timestamp_sec: 0.0, values: vec![0x42C80000] };
    assert!((sample.timestamp_sec - 0.0).abs() < 1e-9);
}

#[test]
fn timestamp_sec_field_5ms() {
    let sample = Sample { seq: 100, timestamp_sec: 0.005, values: vec![] };
    assert!((sample.timestamp_sec - 0.005).abs() < 1e-9);
}

#[test]
fn timestamp_sec_field_one_second() {
    let sample = Sample { seq: 20_000, timestamp_sec: 1.0, values: vec![] };
    assert!((sample.timestamp_sec - 1.0).abs() < 1e-9);
}

// ================================================================
// as_floats
// ================================================================

#[test]
fn as_floats_basic_conversion() {
    // 3.14 as f32 = 0x4048F5C3
    let sample = Sample { seq: 0, timestamp_sec: 0.0, values: vec![0x4048F5C3] };
    let floats = sample.as_floats();
    assert_eq!(floats.len(), 1);
    let expected = f32::from_bits(0x4048F5C3);
    assert_eq!(floats[0], expected);
}

#[test]
fn as_floats_multiple_values() {
    // 1.0 = 0x3F800000, -2.5 = 0xC0200000, 0.0 = 0x00000000
    let sample = Sample {
        seq: 1,
        timestamp_sec: 0.001,
        values: vec![0x3F800000, 0xC0200000, 0x00000000],
    };
    let floats = sample.as_floats();
    assert_eq!(floats.len(), 3);
    assert!((floats[0] - 1.0).abs() < 1e-6);
    assert!((floats[1] - (-2.5)).abs() < 1e-6);
    assert!((floats[2] - 0.0).abs() < 1e-6);
}

#[test]
fn as_floats_empty() {
    let sample = Sample { seq: 0, timestamp_sec: 0.0, values: vec![] };
    let floats = sample.as_floats();
    assert!(floats.is_empty());
}

#[test]
fn as_floats_nan_infinity() {
    // NaN = 0x7FC00000, +Inf = 0x7F800000, -Inf = 0xFF800000
    let sample = Sample {
        seq: 0,
        timestamp_sec: 0.0,
        values: vec![0x7FC00000, 0x7F800000, 0xFF800000],
    };
    let floats = sample.as_floats();
    assert_eq!(floats.len(), 3);
    assert!(floats[0].is_nan());
    assert!(floats[1].is_infinite() && floats[1].is_sign_positive());
    assert!(floats[2].is_infinite() && floats[2].is_sign_negative());
}

// ================================================================
// Sample construction traits
// ================================================================

#[test]
fn sample_clone() {
    let s1 = Sample { seq: 5, timestamp_sec: 0.005, values: vec![0xDEADBEEF, 0xCAFEBABE] };
    let s2 = s1.clone();
    assert_eq!(s2.seq, 5);
    assert_eq!(s2.values.len(), 2);
    assert_eq!(s2.values[0], 0xDEADBEEF);
    assert_eq!(s2.values[1], 0xCAFEBABE);
}

#[test]
fn sample_debug_format() {
    let sample = Sample { seq: 3, timestamp_sec: 0.003, values: vec![0x42] };
    let s = format!("{:?}", sample);
    // Debug format shows struct fields: seq, values
    assert!(s.contains("3"), "Debug output should contain seq=3: {}", s);
    // values show as decimal: 0x42 = 66
    assert!(s.contains("66"), "Debug output should contain value 66: {}", s);
}
