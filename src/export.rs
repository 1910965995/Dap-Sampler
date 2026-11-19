//! 数据导出：将采样点写入 CSV（UTF-8 BOM + \r\n 换行，兼容 Excel）

use crate::pipeline::sample::{Sample, ValueType};
use std::io::Write;

/// 将采样点写入 CSV（UTF-8 BOM + \r\n 换行，兼容 Excel）
///
/// 表头 = `time_s` + 各通道名；每行 = 时间戳（秒）+ 各通道按类型解释的值。
/// 返回写入的样本点数。`names` 与 `types` 必须与 `samples` 的 values 顺序一致
/// （调用方用采集快照保证，勿用运行中可变的通道面板）。
pub fn write_csv<W: Write>(
    writer: W,
    samples: &[Sample],
    names: &[String],
    types: &[ValueType],
) -> std::io::Result<usize> {
    // 契约校验：列定义（通道数）不应超过采样点最大变量数（debug 构建尽早发现接线错误）
    debug_assert!(names.len() <= crate::pipeline::sample::MAX_VALUES);
    let mut w = std::io::BufWriter::new(writer);
    // UTF-8 BOM：兼容中文 Excel 直接打开不乱码
    w.write_all("\u{FEFF}".as_bytes())?;
    // 表头
    write!(w, "time_s")?;
    for name in names {
        write!(w, ",{}", csv_field(name))?;
    }
    w.write_all(b"\r\n")?;
    // 数据行（逐值格式化，避免每行分配 Vec）
    for s in samples {
        // 契约校验：列定义（通道数）应与每个采样点的值数一致（防 ELF 快照过期导致列错位）
        debug_assert_eq!(names.len(), s.values().len(), "column count mismatch with sample values");
        write!(w, "{}", s.timestamp_sec)?;
        for (i, &raw) in s.values().iter().enumerate() {
            let t = types.get(i).copied().unwrap_or(ValueType::Float);
            write!(w, ",{}", t.to_f64(raw))?;
        }
        w.write_all(b"\r\n")?;
    }
    w.flush()?;
    Ok(samples.len())
}

/// CSV 字段转义：含逗号/双引号/换行时双引号包裹，内部双引号翻倍
fn csv_field(name: &str) -> String {
    if name.contains(',') || name.contains('"') || name.contains('\n') || name.contains('\r') {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::{Sample, ValueType};

    /// 2 个采样点：float 1.5/2.5（f32 可精确表示）+ int16 -123/-122
    fn make_samples() -> Vec<Sample> {
        vec![
            Sample::new(0, 0.0, &[u64::from(1.5f32.to_bits()), u64::from((-123i16) as u16)]),
            Sample::new(1, 0.0001, &[u64::from(2.5f32.to_bits()), u64::from((-122i16) as u16)]),
        ]
    }

    #[test]
    fn header_and_values_interpreted_by_type() {
        let samples = make_samples();
        let names = vec!["pid.kp".to_string(), "motor.speed".to_string()];
        let types = vec![ValueType::Float, ValueType::Int16];
        let mut out = Vec::new();
        let n = write_csv(&mut out, &samples, &names, &types).unwrap();
        assert_eq!(n, 2);
        let text = String::from_utf8(out).unwrap();
        // UTF-8 BOM 开头
        assert!(text.starts_with('\u{FEFF}'), "should start with BOM");
        let lines: Vec<&str> = text
            .trim_start_matches('\u{FEFF}')
            .trim_end()
            .split("\r\n")
            .collect();
        assert_eq!(lines[0], "time_s,pid.kp,motor.speed");
        assert_eq!(lines[1], "0,1.5,-123");
        assert_eq!(lines[2], "0.0001,2.5,-122");
    }

    #[test]
    fn empty_samples_writes_header_only() {
        let names = vec!["ch1".to_string()];
        let types = vec![ValueType::Float];
        let mut out = Vec::new();
        let n = write_csv(&mut out, &[], &names, &types).unwrap();
        assert_eq!(n, 0);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "\u{FEFF}time_s,ch1\r\n");
    }

    #[test]
    fn csv_field_quotes_special_chars() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("pid,kp"), "\"pid,kp\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("a\nb"), "\"a\nb\"");
        assert_eq!(csv_field("a\rb"), "\"a\rb\"");
    }

    #[test]
    fn nan_and_inf_output_as_is() {
        let samples = vec![
            Sample::new(0, 0.0, &[u64::from(f32::NAN.to_bits())]),
            Sample::new(1, 0.0001, &[u64::from(f32::INFINITY.to_bits())]),
        ];
        let names = vec!["ch1".to_string()];
        let types = vec![ValueType::Float];
        let mut out = Vec::new();
        write_csv(&mut out, &samples, &names, &types).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.trim_end().split("\r\n").collect();
        assert_eq!(lines[1], "0,NaN");
        assert_eq!(lines[2], "0.0001,inf");
    }

    #[test]
    fn round_trip_parse_back() {
        let samples = make_samples();
        let names = vec!["a".to_string(), "b".to_string()];
        let types = vec![ValueType::Float, ValueType::Int16];
        let mut out = Vec::new();
        write_csv(&mut out, &samples, &names, &types).unwrap();
        let text = String::from_utf8(out).unwrap();
        let body = text.trim_start_matches('\u{FEFF}').trim_end();
        let mut rows = body.split("\r\n");
        let header = rows.next().unwrap();
        assert_eq!(header, "time_s,a,b");
        for (i, row) in rows.enumerate() {
            let fields: Vec<&str> = row.split(',').collect();
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[0], format!("{}", samples[i].timestamp_sec));
            let vals = samples[i].as_f64s_typed(&types);
            assert_eq!(fields[1], format!("{}", vals[0]));
            assert_eq!(fields[2], format!("{}", vals[1]));
        }
    }

    #[test]
    fn types_shorter_than_values_fall_back_to_float() {
        // types 只有 1 个（Int16），样本有 2 个值 → 第 2 个按默认 float 解释
        let samples = vec![Sample::new(0, 0.0, &[u64::from((-5i16) as u16), u64::from(1.5f32.to_bits())])];
        let names = vec!["a".to_string(), "b".to_string()];
        let types = vec![ValueType::Int16];
        let mut out = Vec::new();
        write_csv(&mut out, &samples, &names, &types).unwrap();
        let text = String::from_utf8(out).unwrap();
        let body = text.trim_start_matches('\u{FEFF}');
        let lines: Vec<&str> = body.trim_end().split("\r\n").collect();
        assert_eq!(lines[1], "0,-5,1.5");
    }

    #[test]
    fn exports_64bit_columns() {
        // double + int64 + uint64 各一列，CSV 按类型格式化
        let d = 2.5f64;
        let samples = vec![Sample::new(
            0,
            0.0,
            &[d.to_bits(), (-9i64) as u64, 18_446_744_073_709_551_615u64],
        )];
        let names = vec!["d".to_string(), "i".to_string(), "u".to_string()];
        let types = vec![ValueType::Float64, ValueType::Int64, ValueType::Uint64];
        let mut out = Vec::new();
        write_csv(&mut out, &samples, &names, &types).unwrap();
        let text = String::from_utf8(out).unwrap();
        let body = text.trim_start_matches('\u{FEFF}');
        let lines: Vec<&str> = body.trim_end().split("\r\n").collect();
        assert_eq!(lines[0], "time_s,d,i,u");
        // CSV 经 f64 中转：u64 超过 2^53 后无法精确表示（f64 精度上限）
        assert_eq!(lines[1], "0,2.5,-9,18446744073709552000");
    }
}
