# CSV 数据导出 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Files(F) 菜单新增 "Export data to…"，将 DisplayBuffer 全部样本点导出为 CSV（UTF-8 BOM + 时间戳 + 各通道按类型解释的值）。

**Architecture:** 新建 `src/export.rs` 纯函数模块（`write_csv<W: Write>`，不依赖 egui，可单元测试），app.rs 菜单点击调用 `export_data()` 方法接线（空缓冲检查 → 保存对话框 → 写文件 → 状态栏反馈）。列定义用 WaveformPanel 采集快照（`channel_names()` + `value_types()`），保证与 `Sample.values()` 顺序一致。设计文档：`docs/superpowers/specs/2026-08-02-export-csv-design.md`。

**Tech Stack:** Rust / std::io::Write / rfd 文件对话框

---

## 文件结构

| 文件 | 职责 | 改动 |
|------|------|------|
| `src/export.rs` | CSV 生成纯函数（新建） | 新增 write_csv + csv_field + 测试 |
| `src/lib.rs` | 模块注册 | 新增 `pub mod export;` |
| `src/ui/app.rs` | 菜单接线 | Files(F) 加菜单项 + export_data 方法 |

---

### Task 1: export.rs 模块与 write_csv 纯函数（TDD）

**Files:**
- Create: `src/export.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: 写失败测试**——创建 `src/export.rs`，先只写测试模块（不含实现，`write_csv` 未定义）：

```rust
//! 数据导出：将采样点写入 CSV（UTF-8 BOM + \r\n 换行，兼容 Excel）

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::{Sample, ValueType};

    /// 2 个采样点：float 1.5/2.5（f32 可精确表示）+ int16 -123/-122
    fn make_samples() -> Vec<Sample> {
        vec![
            Sample::new(0, 0.0, &[1.5f32.to_bits(), (-123i16) as u16 as u32]),
            Sample::new(1, 0.0001, &[2.5f32.to_bits(), (-122i16) as u16 as u32]),
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
        let lines: Vec<&str> = text.trim_end().split("\r\n").collect();
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
    }

    #[test]
    fn nan_and_inf_output_as_is() {
        let samples = vec![
            Sample::new(0, 0.0, &[f32::NAN.to_bits()]),
            Sample::new(1, 0.0001, &[f32::INFINITY.to_bits()]),
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
        let body = text.trim_start_matches('\u{FEFF}');
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
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test export 2>&1 | tail -20`
Expected: FAIL——`write_csv` / `csv_field` 未定义（编译错误 E0425）。用 `echo ${PIPESTATUS[0]}` 确认真实失败（管道会掩盖 exit code）。

- [ ] **Step 3: 实现**——`src/export.rs` 在测试模块之前添加实现：

```rust
//! 数据导出：将采样点写入 CSV（UTF-8 BOM + \r\n 换行，兼容 Excel）

use std::io::Write;
use crate::pipeline::sample::{Sample, ValueType};

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
    // ...（Step 1 的测试内容原样保留）
}
```

同时 `src/lib.rs` 在 `pub mod project;` 之后添加：

```rust
pub mod export;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test export 2>&1 | tail -20`
Expected: PASS，5 个 export 测试全部通过。

- [ ] **Step 5: Commit**

```bash
git add src/export.rs src/lib.rs
git commit -m "feat(export): CSV 导出纯函数模块（write_csv，UTF-8 BOM + 类型解释值）"
```

---

### Task 2: app.rs 菜单接线与 export_data 方法

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: Files(F) 菜单新增菜单项**——`src/ui/app.rs` 的 Files 菜单块（"Import ELF File…" 的 `return true;` 之后、`false` 之前）插入：

```rust
                    if menu_item(ui, "Export data to…").clicked() {
                        app.export_data();
                        return true;
                    }
```

- [ ] **Step 2: 新增 export_data 方法**——放在 `save_project` 方法之前（约 `src/ui/app.rs:276` 附近，按内容定位"/// 保存当前配置为工程文件"之前）：

```rust
    /// 将缓冲区全部数据导出为 CSV（Files → Export data to…）
    fn export_data(&mut self) {
        if self.display_buf.is_empty() {
            self.set_error("No data to export: buffer is empty");
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("dap-sampler-data.csv")
            .save_file()
        else {
            return; // 用户取消
        };
        // 借用作用域：display_buf / waveform 的借用在此结束，之后才能 &mut self 反馈状态
        let result = {
            let samples = self.display_buf.all();
            // 列定义用采集快照（Start 时创建，与 Sample.values() 顺序一致）
            let names: Vec<String> = self
                .waveform
                .channel_names()
                .into_iter()
                .map(String::from)
                .collect();
            let types = self.waveform.value_types().to_vec();
            std::fs::File::create(&path)
                .and_then(|f| crate::export::write_csv(f, samples, &names, &types))
        };
        match result {
            Ok(n) => self.set_status(format!("Data exported: {} samples to {}", n, path.display())),
            Err(e) => self.set_error(format!("Failed to export data: {}", e)),
        }
    }
```

- [ ] **Step 3: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过，无警告。用 `echo ${PIPESTATUS[0]}` 确认真实 exit code。

- [ ] **Step 4: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(ui): Files 菜单新增 Export data to…（缓冲区数据导出 CSV）"
```

---

### Task 3: 全量验证

**Files:** 无（验证任务）

- [ ] **Step 1: 全量测试**

Run: `cargo test 2>&1 | command grep -E "test result|^error" | head -20`
Expected: 全部 ok，0 failed（用 `echo ${PIPESTATUS[0]}` 确认真实 exit code 为 0）。

- [ ] **Step 2: clippy 零警告**

Run: `cargo clippy --all-targets 2>&1 | command grep -cE "^warning|^error"`
Expected: 输出 `0`。

- [ ] **Step 3: release 构建**

Run: `cargo build --release 2>&1 | tail -3`
Expected: 构建成功（exit code 0）。

- [ ] **Step 4: 人工验证清单**（用户操作，构建者提供 exe 路径提示）

1. 采集数据后 → Files(F) → Export data to… → 保存 CSV → 用 Excel 打开验证 BOM/中文列名/数据
2. 空缓冲时点击 → 状态栏显示 "No data to export: buffer is empty"
3. 运行中（Paused/Running）导出 → 导出当前缓冲快照，正常
4. 取消保存对话框 → 无任何提示，静默返回
5. 导出后状态栏显示 "Data exported: N samples to <path>"
