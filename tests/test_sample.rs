/// 测试 pipeline::sample::Sample 结构体
use dap_sampler::pipeline::sample::Sample;

// ================================================================
// timestamp_sec
// ================================================================

#[test]
fn timestamp_sec_zero_seq() {
    let sample = Sample { seq: 0, values: vec![0x42C80000] };
    assert!((sample.timestamp_sec(50.0) - 0.0).abs() < 1e-9);
}

#[test]
fn timestamp_sec_50us_interval() {
    let sample = Sample { seq: 100, values: vec![] };
    // 100 * 50μs = 5000μs = 0.005s
    assert!((sample.timestamp_sec(50.0) - 0.005).abs() < 1e-9);
}

#[test]
fn timestamp_sec_one_second() {
    let sample = Sample { seq: 20_000, values: vec![] };
    // 20_000 * 50μs = 1_000_000μs = 1.0s
    assert!((sample.timestamp_sec(50.0) - 1.0).abs() < 1e-9);
}

#[test]
fn timestamp_sec_different_rate() {
    // 1kHz = 1000μs interval
    let sample = Sample { seq: 500, values: vec![] };
    assert!((sample.timestamp_sec(1000.0) - 0.5).abs() < 1e-9);
}

#[test]
fn timestamp_sec_max_seq_no_panic() {
    // u64::MAX should not overflow; cast to f64 may lose precision but won't panic
    let sample = Sample { seq: u64::MAX, values: vec![] };
    let t = sample.timestamp_sec(1.0);
    assert!(t.is_finite());
}

// ================================================================
// as_floats
// ================================================================

#[test]
fn as_floats_basic_conversion() {
    // 3.14 as f32 = 0x4048F5C3
    let sample = Sample { seq: 0, values: vec![0x4048F5C3] };
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
    let sample = Sample { seq: 0, values: vec![] };
    let floats = sample.as_floats();
    assert!(floats.is_empty());
}

#[test]
fn as_floats_nan_infinity() {
    // NaN = 0x7FC00000, +Inf = 0x7F800000, -Inf = 0xFF800000
    let sample = Sample {
        seq: 0,
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
    let s1 = Sample { seq: 5, values: vec![0xDEADBEEF, 0xCAFEBABE] };
    let s2 = s1.clone();
    assert_eq!(s2.seq, 5);
    assert_eq!(s2.values.len(), 2);
    assert_eq!(s2.values[0], 0xDEADBEEF);
    assert_eq!(s2.values[1], 0xCAFEBABE);
}

#[test]
fn sample_debug_format() {
    let sample = Sample { seq: 3, values: vec![0x42] };
    let s = format!("{:?}", sample);
    // Debug format shows struct fields: seq, values
    assert!(s.contains("3"), "Debug output should contain seq=3: {}", s);
    // values show as decimal: 0x42 = 66
    assert!(s.contains("66"), "Debug output should contain value 66: {}", s);
}
