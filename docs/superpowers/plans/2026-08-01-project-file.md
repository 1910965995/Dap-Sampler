# 工程保存/打开功能 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 DAP Sampler 增加工程保存/打开功能——把 ELF 路径、Watch 变量（含 y_offset/y_scale/remark/refresh）与全局参数（采样率/SWD 时钟/窗口/显示模式）保存为 `.dapsproj` JSON 文件，并支持一键恢复。

**Architecture:** 新增独立 `src/project.rs` 模块承载纯数据模型与 JSON 序列化（serde 派生，含版本校验与宽容回退），与 UI 完全解耦、可独立单测；`app.rs` 只增加三个操作方法（`new_project`/`save_project`/`open_project`）+ 顶部三个按钮，打开流程复用现有 `apply_variable_changes`（勾选→通道→Watch 同步）与 `stop_acquisition`。

**Tech Stack:** Rust 2021、serde 1 + serde_json 1（新增依赖）、egui 0.31（rfd 0.15 文件对话框，已有）、`ValueType::parse/label`（已有，sample.rs）

**设计文档:** `docs/superpowers/specs/2026-08-01-project-file-design.md`（已批准）

---

### Task 1: 添加 serde 依赖

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: 在 `[dependencies]` 添加 serde 与 serde_json**

在 `Cargo.toml` 的 `[dependencies]`（`anyhow = "1"` 之后）添加：

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: 验证依赖可解析**

Run: `cargo check 2>&1 | tail -3`
Expected: `Finished` 或仅显示重新编译结果，无 error。

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: 添加 serde + serde_json 依赖（工程文件 JSON 序列化）"
```

---

### Task 2: `src/project.rs` 数据模型 + 序列化（TDD）

**Files:**
- Create: `src/project.rs`
- Modify: `src/lib.rs`
- Test: `src/project.rs` 内 `#[cfg(test)]`

- [ ] **Step 1: 先写失败测试**

在 `src/lib.rs` 末尾追加模块导出：

```rust
pub mod project;
```

创建 `src/project.rs`，先只写测试部分（`use` 顶部导入），此时 `ProjectFile` 等类型不存在，编译必然失败：

```rust
//! 工程文件：保存/恢复 DAP Sampler 工作现场（JSON 格式，版本号可迁移）

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::ValueType;
    use crate::ui::waveform::WaveformDisplayMode;

    fn sample_file() -> ProjectFile {
        ProjectFile {
            version: PROJECT_VERSION,
            elf_path: Some("C:/fw/firmware.elf".to_string()),
            sample_rate_hz: 5000,
            swd_clock_mhz: 10,
            window_size: 2000,
            display_mode: "line".to_string(),
            variables: vec![
                ProjectVariable {
                    name: "pid.kp".to_string(),
                    address: 0x2000_0100,
                    value_type: "float".to_string(),
                    refresh_period_s: 0.1,
                    y_offset: 0.0,
                    y_scale: -2.5, // 允许负缩放
                    remark: "比例系数".to_string(), // 中文
                },
                ProjectVariable {
                    name: "motor.speed".to_string(),
                    address: 0x2000_0104,
                    value_type: "int16".to_string(),
                    refresh_period_s: 0.05,
                    y_offset: 100.0,
                    y_scale: 1.0,
                    remark: String::new(),
                },
            ],
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let file = sample_file();
        let json = file.to_json();
        let restored = ProjectFile::from_json(&json).expect("round-trip 不应失败");
        assert_eq!(restored, file);
        // 中文 remark 完好
        assert_eq!(restored.variables[0].remark, "比例系数");
        // 负 y_scale 完好
        assert_eq!(restored.variables[0].y_scale, -2.5);
    }

    #[test]
    fn round_trip_manual_mode_without_elf() {
        let mut file = sample_file();
        file.elf_path = None;
        file.variables = vec![];
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored, file);
        assert!(restored.elf_path.is_none());
        assert!(restored.variables.is_empty());
    }

    #[test]
    fn corrupted_json_returns_err() {
        let err = ProjectFile::from_json("{ not valid json !!").unwrap_err();
        assert!(err.to_string().contains("解析失败"), "错误应说明解析失败: {}", err);
    }

    #[test]
    fn unsupported_version_returns_err() {
        let mut file = sample_file();
        file.version = 99;
        let json = file.to_json();
        let err = ProjectFile::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("版本不支持"), "错误应说明版本不支持: {}", err);
    }

    #[test]
    fn invalid_value_type_falls_back_to_uint32() {
        let mut file = sample_file();
        file.variables[0].value_type = "banana".to_string();
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored.variables[0].value_type(), ValueType::Uint32);
    }

    #[test]
    fn invalid_display_mode_falls_back_to_line() {
        let mut file = sample_file();
        file.display_mode = "3d".to_string();
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored.display_mode(), WaveformDisplayMode::Line);
    }

    #[test]
    fn value_type_round_trip_through_label() {
        for vt in [ValueType::Float, ValueType::Int32, ValueType::Uint32,
                   ValueType::Int16, ValueType::Uint16, ValueType::Int8, ValueType::Uint8] {
            let s = vt.label().to_string();
            assert_eq!(ValueType::parse(&s).unwrap(), vt);
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test project:: 2>&1 | tail -5`
Expected: 编译错误 `cannot find type ProjectFile` / `cannot find value PROJECT_VERSION`。

- [ ] **Step 3: 实现数据模型**

在 `src/project.rs` 的测试代码之前写入实现（完整替换文件内容为以下实现 + 测试）：

```rust
//! 工程文件：保存/恢复 DAP Sampler 工作现场（JSON 格式，版本号可迁移）

use serde::{Deserialize, Serialize};
use crate::error::{Error, Result};
use crate::pipeline::sample::ValueType;
use crate::ui::waveform::WaveformDisplayMode;

/// 当前支持的工程文件版本（将来格式变化时递增并做迁移）
pub const PROJECT_VERSION: u32 = 1;

/// 工程文件顶层结构（与 .dapsproj 文件内容一一对应）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFile {
    /// 格式版本号（读入时校验）
    pub version: u32,
    /// ELF 固件路径（None = 手工地址模式）
    pub elf_path: Option<String>,
    /// 采样率 (Hz)
    pub sample_rate_hz: u32,
    /// SWD 时钟 (MHz)
    pub swd_clock_mhz: u32,
    /// 显示窗口大小（采样点数）
    pub window_size: usize,
    /// 波形显示模式（小写："line" / "point"）
    pub display_mode: String,
    /// 变量列表（Watch 通道）
    pub variables: Vec<ProjectVariable>,
}

/// 单个变量（对应 Watch 面板一行）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectVariable {
    /// 变量名（ELF 模式为变量 path，手工模式为 "CH1 0x..." 格式）
    pub name: String,
    /// 内存地址
    pub address: u32,
    /// 变量类型（小写字符串，如 "float"、"int16"）
    pub value_type: String,
    /// 刷新周期（秒）
    pub refresh_period_s: f32,
    /// Y 轴偏移
    pub y_offset: f32,
    /// Y 轴缩放（禁止 0；允许负数）
    pub y_scale: f32,
    /// 用户自定义备注
    pub remark: String,
}

impl ProjectFile {
    /// 序列化为 pretty JSON（字段按声明顺序输出，便于 diff）
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("ProjectFile 序列化不应失败")
    }

    /// 从 JSON 字符串反序列化，并校验版本号
    pub fn from_json(text: &str) -> Result<ProjectFile> {
        let file: ProjectFile = serde_json::from_str(text)
            .map_err(|e| Error::Other(format!("工程文件解析失败: {}", e)))?;
        if file.version != PROJECT_VERSION {
            return Err(Error::Other(format!(
                "工程文件版本不支持: 文件版本 {}, 当前支持版本 {}",
                file.version, PROJECT_VERSION
            )));
        }
        Ok(file)
    }

    /// 解析显示模式（未知字符串宽容回退 Line，不整体报错）
    pub fn display_mode(&self) -> WaveformDisplayMode {
        match self.display_mode.to_lowercase().as_str() {
            "point" => WaveformDisplayMode::Point,
            _ => WaveformDisplayMode::Line,
        }
    }
}

impl ProjectVariable {
    /// 解析变量类型（未知字符串宽容回退 Uint32，不整体报错）
    pub fn value_type(&self) -> ValueType {
        ValueType::parse(&self.value_type).unwrap_or(ValueType::Uint32)
    }
}
```

（保留 Step 1 的测试代码在文件末尾 `#[cfg(test)] mod tests { ... }`）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test project:: 2>&1 | tail -8`
Expected: `7 passed; 0 failed`（6 个新测试 + 1 个 label 往返测试）。

- [ ] **Step 5: 全量测试 + Commit**

Run: `cargo test 2>&1 | tail -3`
Expected: 全部通过（171 个既有测试 + 7 个新测试）。

```bash
git add src/project.rs src/lib.rs
git commit -m "feat(project): 工程文件数据模型与 JSON 序列化（版本校验 + 宽容回退）"
```

---

### Task 3: `WaveformDisplayMode::label()`（保存时序列化显示模式）

**Files:**
- Modify: `src/ui/waveform.rs`（`WaveformDisplayMode` enum 定义处，约 5-11 行）

- [ ] **Step 1: 给 `WaveformDisplayMode` 加 `label()` 方法**

在 `src/ui/waveform.rs` 的 enum 定义后（`ChannelInfo` 之前）添加：

```rust
impl WaveformDisplayMode {
    /// 小写字符串（与 ProjectFile::display_mode() 的解析互逆）
    pub fn label(&self) -> &'static str {
        match self {
            WaveformDisplayMode::Line => "line",
            WaveformDisplayMode::Point => "point",
        }
    }
}
```

- [ ] **Step 2: 验证编译与既有测试**

Run: `cargo test 2>&1 | tail -3`
Expected: 全部通过。

- [ ] **Step 3: Commit**

```bash
git add src/ui/waveform.rs
git commit -m "feat(waveform): WaveformDisplayMode::label() 小写字符串序列化"
```

---

### Task 4: `app.rs` 三个操作方法（new / save / open）

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: 添加字段与导入**

顶部 `use` 区域（`use super::watch_panel::WatchPanel;` 之后）添加：

```rust
use crate::project::{ProjectFile, ProjectVariable, PROJECT_VERSION};
```

结构体 `DapSamplerApp` 字段（`selected_channel: Option<usize>,` 之后）添加：

```rust
    /// 当前工程文件路径（None = 未保存过；保存时已有路径则直接覆盖）
    project_path: Option<String>,
```

构造函数 `Self { ... }` 中（`selected_channel: None,` 之后）添加：

```rust
            project_path: None,
```

- [ ] **Step 2: 实现 `new_project()`**

在 `set_error` 方法（约 166-170 行）之后添加：

```rust
    /// 新建工程：清空当前全部配置，参数回默认值
    fn new_project(&mut self) {
        if self.pipeline.is_some() {
            self.stop_acquisition();
        }
        // 清空 ELF / 通道 / Watch / 手工模式
        self.elf_ctx = None;
        self.variable_browser.clear();
        self.channel_panel.channels.clear();
        self.watch_panel.entries.clear();
        self.manual_addresses.clear();
        self.manual_channel_names.clear();
        self.manual_value_types.clear();
        self.current_addresses.clear();
        // 参数回默认（与 DapSamplerApp::new 一致：5000 Hz / 2000 点 / 1 MHz / Line）
        self.rate_hz = 5000;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.window_size = 2000;
        self.controls.sample_rate = self.rate_hz;
        self.controls.window_size = self.window_size;
        self.controls.swd_clock_mhz = 1;
        self.swd_clock_mhz = 1;
        let mode = WaveformDisplayMode::Line;
        self.display_mode = mode;
        self.waveform = WaveformPanel::new(vec![], vec![], self.interval_us, vec![], mode);
        // 清空运行时状态
        self.display_buf = DisplayBuffer::new(self.window_size);
        self.cursor.clear();
        self.controls.update_count(0);
        self.controls.set_stopped();
        self.selected_channel = None;
        self.project_path = None;
        log::info!("已新建工程（清空配置）");
    }
```

- [ ] **Step 3: 实现 `save_project()`**

在 `new_project` 之后添加：

```rust
    /// 保存当前配置为工程文件（已有路径直接覆盖，否则弹保存对话框）
    fn save_project(&mut self) {
        let file = ProjectFile {
            version: PROJECT_VERSION,
            elf_path: self.elf_ctx.as_ref().map(|c| c.file_path.clone()),
            sample_rate_hz: self.controls.sample_rate,
            swd_clock_mhz: self.controls.swd_clock_mhz,
            window_size: self.controls.window_size,
            display_mode: self.waveform.display_mode().label().to_string(),
            variables: self
                .watch_panel
                .entries
                .iter()
                .map(|e| ProjectVariable {
                    name: e.name.clone(),
                    address: e.address,
                    value_type: e.value_type.label().to_string(),
                    refresh_period_s: e.refresh_period_s,
                    y_offset: e.y_offset,
                    y_scale: e.y_scale,
                    remark: e.remark.clone(),
                })
                .collect(),
        };
        let json = file.to_json();

        // 目标路径：已保存过则直接覆盖；否则弹保存对话框
        let path = match self.project_path.clone() {
            Some(p) => Some(p),
            None => rfd::FileDialog::new()
                .add_filter("DAP Sampler Project", &["dapsproj"])
                .set_file_name("dap-sampler.dapsproj")
                .save_file(),
        };
        let Some(path) = path else {
            return; // 用户取消对话框
        };

        match std::fs::write(&path, json) {
            Ok(()) => {
                self.project_path = Some(path.display().to_string());
                log::info!("工程已保存: {}", path.display());
            }
            Err(e) => self.set_error(format!("保存工程失败: {}", e)),
        }
    }
```

注意：`rfd::FileDialog::save_file()` 返回 `Option<PathBuf>`，存入 `project_path: Option<String>` 时用 `path.display().to_string()` 转换。

- [ ] **Step 4: 实现 `open_project()`**

在 `save_project` 之后添加：

```rust
    /// 打开工程文件：恢复 ELF、通道、Watch 配置与全局参数
    fn open_project(&mut self, path: &std::path::Path) {
        if self.pipeline.is_some() {
            self.stop_acquisition();
        }

        // 1. 读取并解析（失败不动当前状态）
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.set_error(format!("读取工程文件失败: {}", e));
                return;
            }
        };
        let file = match ProjectFile::from_json(&text) {
            Ok(f) => f,
            Err(e) => {
                self.set_error(e.to_string());
                return;
            }
        };

        // 2. 加载 ELF（失败仅警告，用保存的地址继续）
        let elf_loaded = if let Some(elf_path) = &file.elf_path {
            match ElfParser::load(elf_path) {
                Ok(ctx) => {
                    self.elf_ctx = Some(ctx);
                    log::info!("ELF 已加载: {}", elf_path);
                    true
                }
                Err(e) => {
                    self.elf_ctx = None;
                    self.set_error(format!("ELF 加载失败，已用保存的变量地址继续: {}", e));
                    false
                }
            }
        } else {
            self.elf_ctx = None;
            false
        };

        // 3. 恢复通道与 Watch 列表
        if elf_loaded {
            // ELF 模式：勾选全部工程变量 → 复用 apply_variable_changes 重建通道
            self.variable_browser.clear();
            for v in &file.variables {
                self.variable_browser.checked_paths.insert(v.name.clone());
            }
            let changes: Vec<(String, bool)> =
                file.variables.iter().map(|v| (v.name.clone(), true)).collect();
            self.apply_variable_changes(changes);
        } else {
            // 手工模式（或 ELF 加载失败降级）：用工程里的地址/类型直接恢复
            self.manual_addresses = file.variables.iter().map(|v| v.address).collect();
            self.manual_value_types =
                file.variables.iter().map(|v| v.value_type()).collect();
            self.manual_channel_names = file.variables.iter().map(|v| v.name.clone()).collect();
            self.channel_panel.channels.clear();
            for (i, v) in file.variables.iter().enumerate() {
                let color = CHANNEL_COLORS[i % CHANNEL_COLORS.len()];
                self.channel_panel
                    .add_channel(v.name.clone(), color, v.value_type());
            }
            let names: Vec<String> = self
                .channel_panel
                .channels
                .iter()
                .map(|c| c.name.clone())
                .collect();
            let types: Vec<ValueType> = self
                .channel_panel
                .channels
                .iter()
                .map(|c| c.value_type)
                .collect();
            self.watch_panel
                .sync_from_channels(&names, &types, &self.manual_addresses);
        }

        // 4. 覆盖 Watch 配置字段（y_offset/y_scale/refresh/remark 以工程为准）
        for (entry, pv) in self.watch_panel.entries.iter_mut().zip(&file.variables) {
            entry.name = pv.name.clone();
            entry.address = pv.address;
            entry.value_type = pv.value_type();
            entry.refresh_period_s = pv.refresh_period_s;
            entry.y_offset = pv.y_offset;
            entry.y_scale = pv.y_scale;
            entry.remark = pv.remark.clone();
        }

        // 5. 恢复全局参数（controls 与 app 字段、waveform 三方同步）
        self.controls.sample_rate = file.sample_rate_hz;
        self.rate_hz = file.sample_rate_hz;
        self.interval_us = 1_000_000.0 / self.rate_hz as f64;
        self.waveform.set_interval(self.interval_us);
        self.controls.swd_clock_mhz = file.swd_clock_mhz;
        self.swd_clock_mhz = file.swd_clock_mhz;
        self.controls.window_size = file.window_size;
        self.window_size = file.window_size;
        let mode = file.display_mode();
        self.waveform.set_display_mode(mode);
        self.display_mode = mode;

        // 6. 清空运行时状态，回到 Idle
        self.display_buf = DisplayBuffer::new(self.window_size);
        self.cursor.clear();
        self.controls.update_count(0);
        self.selected_channel = None;

        // 7. 记录工程路径
        self.project_path = Some(path.display().to_string());
        log::info!("已打开工程: {}", path.display());
    }
```

注意：`file` 在第 4、5 步仍被借用（zip 迭代与字段读取），`apply_variable_changes` 只借用 `self` 的可变引用，与 `file` 无冲突——`file` 是独立局部变量。若编译器报借用冲突，将第 3 步的 `changes` 改为先 `let vars = file.variables.clone();` 再使用。

- [ ] **Step 5: 验证编译与既有测试**

Run: `cargo test 2>&1 | tail -5`
Expected: 编译通过，全部测试通过（新方法无单测，靠编译 + 后续人工验证）。

- [ ] **Step 6: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(app): 工程 new/save/open 三个操作方法（复用 apply_variable_changes 恢复通道）"
```

---

### Task 5: 顶部三个按钮 + 工程名显示

**Files:**
- Modify: `src/ui/app.rs`（`update` 中 header 部分，约 516-527 行）

- [ ] **Step 1: 在 header 加按钮与工程名**

在 `update()` 的 header `TopBottomPanel` 中，`ui.heading("DAP Sampler"); ui.separator();` 之后、`// 状态指示` 注释之前插入：

```rust
                // 工程操作：新建 / 打开 / 保存
                if ui.button("\u{1f4c2} 新建工程").clicked() {
                    self.new_project();
                }
                if ui.button("\u{1f4c2} 打开工程").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("DAP Sampler Project", &["dapsproj"])
                        .pick_file()
                    {
                        self.open_project(&path);
                    }
                }
                if ui.button("\u{1f4be} 保存工程").clicked() {
                    self.save_project();
                }
                let proj_name = self
                    .project_path
                    .as_ref()
                    .and_then(|p| std::path::Path::new(p).file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "(未保存)".to_string());
                ui.label(
                    egui::RichText::new(format!("project: {}", proj_name))
                        .small()
                        .color(egui::Color32::from_rgb(0x8b, 0x94, 0x9e)),
                );

                ui.separator();

                // 状态指示（带颜色圆点）
```

（emoji 说明：`📂` = `\u{1f4c2}`，`💾` = `\u{1f4be}`，与现有 `\u{1f4c1} Open ELF` 按钮风格一致。）

- [ ] **Step 2: 验证编译**

Run: `cargo check 2>&1 | tail -3`
Expected: `Finished`，无 error。

- [ ] **Step 3: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(app): 顶部菜单栏新增 新建工程/打开工程/保存工程 按钮与工程名显示"
```

---

### Task 6: 全量验证 + Release 构建

**Files:** 无（仅验证）

- [ ] **Step 1: 全量测试**

Run: `cargo test 2>&1 | tail -5`
Expected: 全部通过（178 个：171 既有 + 7 新增）。

- [ ] **Step 2: clippy 零警告**

Run: `cargo clippy --all-targets 2>&1 | tail -5`
Expected: `Finished`，无 warning。

- [ ] **Step 3: Release 构建**

Run: `cargo build --release 2>&1 | tail -3`
Expected: `Finished`，生成 `target/release/dap-sampler.exe`。

- [ ] **Step 4: 人工验证（需要硬件 DAP-Link）**

1. 启动 GUI → 加载 ELF、勾选变量、调好 y_offset/y_scale/remark/refresh → 点 `💾 保存工程` → 保存为 `test.dapsproj`
2. 检查 `test.dapsproj` 内容：`elf_path`、`variables`（含全部配置字段）、`sample_rate_hz` 等齐全
3. 点 `📁 新建工程` → 界面清空、参数回默认
4. 点 `📂 打开工程` → 选择 `test.dapsproj` → Watch 列表、Y 轴配置、备注、参数全部恢复
5. 点 Start 验证可正常采集
6. 再次点 `💾 保存工程` → 不弹对话框直接覆盖（project_path 已记录）
7. 手工把 `test.dapsproj` 改坏（删一个逗号）→ 打开 → 顶部显示「解析失败」错误且界面不变

- [ ] **Step 5: 若验证发现问题，修复并重新跑 Step 1-3；无问题则提交收尾**

```bash
git status --short   # 确认工作区干净
git log --oneline -6
```

---

## 自审记录

- **Spec 覆盖**：数据模型（Task 2）/ 依赖（Task 1）/ 三按钮 + 三方法（Task 4、5）/ 错误处理（Task 4 各分支）/ 测试 6+1 项（Task 2）/ 非目标（无对应任务，符合 YAGNI）
- **类型一致性**：`ProjectFile` 字段名与设计文档 JSON schema 一致；`apply_variable_changes`、`stop_acquisition`、`update_count`、`set_stopped`、`set_interval`、`set_display_mode` 均为现有方法（已在 app.rs 中确认存在）；`ElfContext.file_path` 已存在（parser.rs:10）
- **占位符**：无 TBD/TODO；每步含完整代码
