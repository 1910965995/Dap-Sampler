# DAP Sampler CSV 数据导出设计

日期：2026-08-02
状态：已批准

## 背景与动机

用户需要将采集到的缓冲区数据导出为 CSV 文件，便于用 Excel / Origin / pandas 等工具离线分析。当前缓冲区（DisplayBuffer）可保留最多 `buffer_capacity` 个采样点（默认 200k，最大 1M），波形只能可视化浏览，无法导出原始数据。

## 需求

- `Files(F)` 下拉框新增 "Export data to…" 菜单项
- 点击后将**当前 DisplayBuffer 中的全部样本点**导出为 CSV 文件
- 每行：时间戳（秒）+ 各通道按类型解释后的值（与波形显示一致）
- 空缓冲时给出错误提示；导出成功/失败在底部状态栏反馈

## 设计

### 1. 菜单与交互流程

`Files(F)` 下拉框在 "Import ELF File…" 之后新增菜单项 **"Export data to…"**（整行可点击、左对齐、无图标，与现有菜单项一致）。

app.rs 新增方法 `export_data(&mut self)`，点击流程：

1. **空缓冲检查**：`display_buf.is_empty()` → `set_error("No data to export: buffer is empty")`，不弹对话框
2. **保存对话框**：`rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("dap-sampler-data.csv").save_file()`；取消 → 静默返回
3. **写入**：调用 `export::write_csv(file, samples, names, types)`
4. **反馈**：成功 → `set_status("Data exported: N samples to <path>")`；失败 → `set_error("Failed to export data: <err>")`

**数据与列定义**：数据来自 `self.display_buf.all()`（全部缓冲点）；列定义**不用** `channel_panel.channels`（运行中增删通道会导致顺序与 `Sample.values()` 错位），而用 **`WaveformPanel` 的采集快照**——`self.waveform.channel_names()` + `self.waveform.value_types()`（Start 时按采集通道创建，之后 name/type 不变，与 `Sample.values()` 顺序严格一致）。**全部通道都导出**（与可见性无关）。

### 2. CSV 格式

- **编码**：UTF-8 带 BOM（`\u{FEFF}`）——兼容中文 Excel 直接打开不乱码
- **换行**：`\r\n`（Windows/Excel 兼容）
- **表头**：`time_s,<通道1名>,<通道2名>,...`
  - 通道名含逗号 / 双引号 / 换行时：双引号包裹，内部双引号翻倍（标准 CSV 转义）
- **数据行**：每行一个采样点：`<时间戳>,<值1>,<值2>,...`
  - 时间戳：`s.timestamp_sec`（秒，f64，Rust 默认最短表示）
  - 值：按 `value_types` 解释后的 f64（float → `3.14`，int16 → `-123`，uint8 → `255`），与波形显示一致
  - 非有限值（NaN/Inf）：原样输出 `NaN` / `inf`（不跳过、不报错——CSV 是原始数据导出，保真优先）
- **性能**：`BufWriter` 流式写入，不构建大字符串；1M 点 ≈ 20MB，秒级完成；导出在 UI 线程执行（一次性操作，可接受的短暂停顿）

示例（2 通道 float + int16，2 个采样点，浮点按 Rust 最短表示）：

```csv
time_s,pid.kp,motor.speed
0,3.14,-123
0.0001,3.15,-122
```

### 3. 模块与测试

**新文件 `src/export.rs`**（纯函数，不依赖 egui，可单元测试）：

```rust
/// 将采样点写入 CSV（UTF-8 BOM + \r\n 换行，兼容 Excel）
///
/// 返回写入的样本点数。表头 = time_s + 各通道名（names 与 types 必须
/// 与 samples 的 values 顺序一致，调用方用采集快照保证）。
pub fn write_csv<W: std::io::Write>(
    writer: W,
    samples: &[Sample],
    names: &[String],
    types: &[ValueType],
) -> std::io::Result<usize>
```

- 内部流程：写 BOM → 写表头 → 逐行写 `timestamp,val1,val2,...`（值用 `s.as_f64s_typed(types)`）
- 辅助函数 `fn csv_field(name: &str) -> String`：含逗号/引号/换行时加引号包裹、内部引号翻倍，否则原样
- `src/lib.rs` 注册 `pub mod export;`

**测试**（`src/export.rs` 内 `#[cfg(test)]`）：

1. 表头正确：2 通道 → `time_s,pid.kp,motor.speed\r\n`；含逗号通道名 → 引号包裹
2. 值按类型解释：float 通道 3.14、int16 通道 -123、uint8 通道 255
3. 空缓冲：只有表头，返回 0
4. 特殊字符转义：通道名含逗号/引号 → 正确包裹与翻倍
5. round-trip：导出的 CSV 可被简单解析器解析回相同的值序列

**app.rs 接线**：`Files(F)` 菜单加 "Export data to…" → `export_data()` 方法，数据来自 `self.display_buf.all()` + `self.waveform.channel_names()/value_types()`。

## 非目标

- 不做列选择（导出全部通道）
- 不做范围选择（只导出光标之间/视野内的数据）
- 不导出 seq 序号列
- 不做增量导出/追加模式
- 不引入 csv crate 依赖

## 测试

- `src/export.rs` 单元测试（上述 5 项）
- 现有全部测试保持通过
- 编译通过、clippy 零警告、release 构建成功
