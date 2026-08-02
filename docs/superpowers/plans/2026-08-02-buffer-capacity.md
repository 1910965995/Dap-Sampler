# 缓冲区容量设置与波形历史回看 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Window 参数只控制波形视野宽度，新增可配置的缓冲区容量（Settings 菜单 → General Settings 窗口），暂停后拖动可回看全部历史。

**Architecture:** 双参数分离——`buffer_capacity`（默认 200k，1k~1M，随工程保存，下次 Start 生效）控制环形缓冲 + DisplayBuffer 容量；`window_size` 只作视野宽度。波形绘制时对 cached_points 按可视 X 范围二分切片（零拷贝），Y 轴改为每帧按视野重扫。设计文档：`docs/superpowers/specs/2026-08-02-buffer-capacity-design.md`。

**Tech Stack:** Rust / egui 0.31 / egui_plot / serde / eframe

---

## 文件结构

| 文件 | 职责 | 改动 |
|------|------|------|
| `src/project.rs` | 工程文件序列化 | 新增 `buffer_capacity` 字段 + serde default + 测试 |
| `src/ui/controls.rs` | 控制面板 + 常量 | 新增 `BUFFER_CAPACITY_MIN/MAX/DEFAULT` 常量 |
| `src/ui/app.rs` | 主应用 | 字段、工程存取、Settings 菜单、General Settings 窗口、DisplayBuffer 容量、config 一致性、传 view_points |
| `src/pipeline/engine.rs` | 采样引擎 | `start(capacity)` 参数化 |
| `src/ui/waveform.rs` | 波形渲染 | view_points 视野、渲染裁剪、Y 轴视野重扫 |

---

### Task 1: ProjectFile 增加 buffer_capacity 字段（TDD）

**Files:**
- Modify: `src/project.rs`

- [ ] **Step 1: 写失败测试**——在 `src/project.rs` 的 `tests` 模块 `sample_file()` 的 `ProjectFile` 字面量中添加 `buffer_capacity: 200_000`，并新增两个测试：

```rust
#[test]
fn buffer_capacity_round_trip() {
    let mut file = sample_file();
    file.buffer_capacity = 500_000;
    let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
    assert_eq!(restored.buffer_capacity, 500_000);
}

#[test]
fn old_project_without_buffer_capacity_defaults_to_200k() {
    // 模拟旧版工程 JSON（无 buffer_capacity 字段）
    let json = r#"{
        "version": 1,
        "elf_path": null,
        "sample_rate_hz": 5000,
        "swd_clock_mhz": 10,
        "window_size": 2000,
        "display_mode": "line",
        "variables": []
    }"#;
    let restored = ProjectFile::from_json(json).unwrap();
    // 与 controls.rs 的 DEFAULT_BUFFER_CAPACITY（200_000）保持一致（该常量在 Task 2 定义）
    assert_eq!(restored.buffer_capacity, 200_000);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test project 2>&1 | tail -20`
Expected: FAIL——`sample_file` 字面量缺少字段 `buffer_capacity`（编译错误）。

- [ ] **Step 3: 实现**——先在 `src/ui/controls.rs` 的 `WINDOW_MAX` 定义之后（`src/ui/controls.rs:41` 附近）添加常量（Task 1 的 default 函数依赖它）：

```rust
/// 缓冲区容量范围
pub const BUFFER_CAPACITY_MIN: usize = 1_000;
pub const BUFFER_CAPACITY_MAX: usize = 1_000_000;
/// 默认缓冲区容量（≈10 秒 @ 20kHz）
pub const DEFAULT_BUFFER_CAPACITY: usize = 200_000;
```

再改 `src/project.rs` 的 `ProjectFile` 结构体，在 `window_size` 字段之后新增：

```rust
    /// 显示窗口大小（采样点数）
    pub window_size: usize,
    /// 缓冲区容量（历史数据保留量，采样点数上限）
    #[serde(default = "default_buffer_capacity")]
    pub buffer_capacity: usize,
```

并在 `impl ProjectFile` 块**之外**（结构体定义之后）新增**模块级自由函数**（serde 的 `default` 属性不支持关联函数）：

```rust
/// 旧版工程文件缺失 buffer_capacity 字段时的默认值（serde default 用）
fn default_buffer_capacity() -> usize {
    crate::ui::controls::DEFAULT_BUFFER_CAPACITY
}
```

同时在 `sample_file()` 的字面量中补 `buffer_capacity: 200_000`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test project 2>&1 | tail -20`
Expected: PASS，全部 project 测试通过（含两个新测试）。

- [ ] **Step 5: Commit**

```bash
git add src/project.rs src/ui/controls.rs
git commit -m "feat(project): 工程文件新增 buffer_capacity 字段（serde 默认 200k 兼容旧文件）"
```

---

### Task 2: app.rs 数据模型与工程存取

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: app.rs 新增字段**——`DapSamplerApp` 结构体 `show_about: bool` 之前添加（`src/ui/app.rs:83` 附近）：

```rust
    /// 缓冲区容量（历史保留量，采样点数上限；下次 Start 生效）
    buffer_capacity: usize,
    /// 设置窗口是否显示（设置 → 常规设置）
    show_settings: bool,
```

构造函数 `Self { ... }` 中（`show_about: false` 附近）添加：

```rust
            buffer_capacity: crate::ui::controls::DEFAULT_BUFFER_CAPACITY,
            show_settings: false,
```

- [ ] **Step 2: new_project 回默认值**——`src/ui/app.rs:256` `self.window_size = 2000;` 之后添加：

```rust
        self.buffer_capacity = crate::ui::controls::DEFAULT_BUFFER_CAPACITY;
```

并把 `src/ui/app.rs:265` 改为：

```rust
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
```

- [ ] **Step 3: save_project 写入**——`src/ui/app.rs:282` `window_size: self.controls.window_size,` 之后添加：

```rust
            buffer_capacity: self.buffer_capacity,
```

- [ ] **Step 4: open_project 恢复**——`src/ui/app.rs:470` `self.window_size = self.controls.window_size;` 之后添加：

```rust
        self.buffer_capacity = file.buffer_capacity
            .clamp(crate::ui::controls::BUFFER_CAPACITY_MIN, crate::ui::controls::BUFFER_CAPACITY_MAX);
```

并把 `src/ui/app.rs:476` 改为：

```rust
        self.display_buf = DisplayBuffer::new(self.buffer_capacity);
```

- [ ] **Step 5: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过（exit code 0）。

- [ ] **Step 6: Commit**

```bash
git add src/ui/controls.rs src/ui/app.rs
git commit -m "feat(app): buffer_capacity 字段与工程存取（新建/保存/打开）"
```

---

### Task 3: 环形缓冲参数化

**Files:**
- Modify: `src/pipeline/engine.rs`
- Modify: `src/ui/app.rs`

- [ ] **Step 1: engine.start 签名加容量参数**——`src/pipeline/engine.rs:97`：

```rust
    pub fn start(&self) -> Result<PipelineHandle> {
```

改为：

```rust
    pub fn start(&self, capacity: usize) -> Result<PipelineHandle> {
```

`src/pipeline/engine.rs:104`：

```rust
        let ring = Arc::new(RingBuffer::new(200_000)); // 10秒 @ 20kHz
```

改为：

```rust
        let ring = Arc::new(RingBuffer::new(capacity));
```

- [ ] **Step 2: app.rs 调用处传容量**——`src/ui/app.rs:596`：

```rust
        match engine.start() {
```

改为：

```rust
        match engine.start(self.buffer_capacity) {
```

- [ ] **Step 3: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add src/pipeline/engine.rs src/ui/app.rs
git commit -m "feat(engine): 环形缓冲容量参数化，由 buffer_capacity 决定"
```

---

### Task 4: start_acquisition 缓冲容量与 Window 语义分离

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: start_acquisition 中 DisplayBuffer 用 buffer_capacity**——`src/ui/app.rs:577`（keep_history 分支）：

```rust
        if keep_history {
            self.display_buf.set_max_samples(self.window_size);
        } else {
            self.display_buf = DisplayBuffer::new(self.window_size);
```

改为：

```rust
        if keep_history {
            self.display_buf.set_max_samples(self.buffer_capacity);
        } else {
            self.display_buf = DisplayBuffer::new(self.buffer_capacity);
```

- [ ] **Step 2: 移除 Window 变化时对缓冲容量的修改**——`src/ui/app.rs:1225-1228` 的 Window 参数处理块。当前代码：

```rust
                if self.controls.window_size != self.window_size {
                    self.window_size = self.controls.window_size;
                    self.display_buf.set_max_samples(self.window_size);
                    self.set_status(format!("Display window size changed to {} points", self.window_size));
                }
```

改为（Window 只更新视野宽度，不再触碰缓冲容量）：

```rust
                if self.controls.window_size != self.window_size {
                    self.window_size = self.controls.window_size;
                    self.set_status(format!("Display window size changed to {} points", self.window_size));
                }
```

- [ ] **Step 3: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(app): DisplayBuffer 容量改用 buffer_capacity，Window 仅控制视野宽度"
```

---

### Task 5: Pause→Start 配置一致性包含容量

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: config_unchanged 增加容量一致性**——`src/ui/app.rs:570`：

```rust
        let keep_history = self.controls.state == AcquisitionState::Paused && config_unchanged;
```

改为：

```rust
        // Pause 期间改了缓冲区容量 → 视为配置变化，恢复时重建缓冲
        let keep_history = self.controls.state == AcquisitionState::Paused
            && config_unchanged
            && self.display_buf.max_samples() == self.buffer_capacity;
```

（`display_buf.max_samples()` 返回当前实际容量；Settings 里改过 `buffer_capacity` 后两者不等，走重建分支。）

- [ ] **Step 2: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过。

- [ ] **Step 3: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(app): Pause 恢复时容量已变化则重建缓冲（与下次 Start 生效语义一致）"
```

---

### Task 6: Settings 菜单与 General Settings 窗口

**Files:**
- Modify: `src/ui/app.rs`

- [ ] **Step 1: 菜单栏插入 Settings**——`src/ui/app.rs:1050`（Files 菜单块结束）与 `1052`（Help 菜单注释）之间插入：

```rust
                // ---- 设置菜单 ----
                self.hover_menu(ui, "settings", "Settings(S)", |ui, app| {
                    if menu_item(ui, "General Settings…").clicked() {
                        app.show_settings = true;
                        return true;
                    }
                    false
                });
```

- [ ] **Step 2: General Settings 窗口**——在 About 窗口块（`src/ui/app.rs:1276` `if self.show_about {` 之前）插入：

```rust
        // ---- General Settings 窗口（Settings → General Settings…）----
        if self.show_settings {
            let was_running = self.controls.state != AcquisitionState::Idle;
            let old_capacity = self.buffer_capacity;
            egui::Window::new("General Settings")
                .open(&mut self.show_settings)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Buffer capacity:");
                        ui.add(
                            egui::DragValue::new(&mut self.buffer_capacity)
                                .range(
                                    crate::ui::controls::BUFFER_CAPACITY_MIN
                                        ..=crate::ui::controls::BUFFER_CAPACITY_MAX,
                                )
                                .clamp_existing_to_range(true)
                                .speed(1_000.0)
                                .suffix(" pts"),
                        );
                    });
                    ui.add_space(4.0);
                    // 实时换算：容量 / 采样率 = 可回看秒数
                    let seconds =
                        self.buffer_capacity as f64 / self.controls.sample_rate.max(1) as f64;
                    ui.label(
                        egui::RichText::new(format!(
                            "≈ {:.1} s of history @ {} Hz",
                            seconds, self.controls.sample_rate
                        ))
                        .size(11.0)
                        .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                    );
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Applies on the next Start.")
                            .size(11.0)
                            .color(egui::Color32::from_rgb(0x6e, 0x76, 0x82)),
                    );
                });
            // 采集运行/暂停中改动容量 → 提示下次 Start 生效
            if was_running && self.buffer_capacity != old_capacity {
                self.set_status(format!(
                    "Buffer capacity changed to {} points (applies on next Start)",
                    self.buffer_capacity
                ));
            }
        }
```

- [ ] **Step 3: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(ui): Settings(S) 菜单 + General Settings 窗口（缓冲区容量设置）"
```

---

### Task 7: waveform.rs 视野窗口、渲染裁剪与 Y 轴视野重扫（TDD）

**Files:**
- Modify: `src/ui/waveform.rs`
- Modify: `src/ui/app.rs`（show 调用处一行）

- [ ] **Step 1: 写失败测试**——`src/ui/waveform.rs` 的 `tests` 模块末尾新增：

```rust
    #[test]
    fn visible_range_returns_inner_slice_with_border_points() {
        // 10 个点，x = 0..10
        let pts: Vec<PlotPoint> = (0..10u32)
            .map(|i| PlotPoint::new(i as f64, 0.0))
            .collect();
        // 视野 [3.2, 6.8]：内部点 4..6，边界各扩展 1 → [3, 7]
        let (s, e) = visible_range(&pts, 3.2, 6.8);
        assert_eq!((s, e), (3, 7));
        // 视野覆盖全部：内部 0..10，扩展后 [0, 10)
        let (s, e) = visible_range(&pts, -1.0, 100.0);
        assert_eq!((s, e), (0, 10));
        // 视野完全在数据左侧：内部空，扩展后 [0, 1)
        let (s, e) = visible_range(&pts, -50.0, -10.0);
        assert_eq!((s, e), (0, 1));
        // 视野完全在数据右侧：内部空，扩展后 [8, 10)
        let (s, e) = visible_range(&pts, 50.0, 90.0);
        assert_eq!((s, e), (8, 10));
        // 空点集
        let (s, e) = visible_range(&[], 0.0, 1.0);
        assert_eq!((s, e), (0, 0));
    }

    #[test]
    fn rescan_y_max_scans_visible_window_only() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        // 值：视野外 [0,2) 有峰值 100；视野内 [2,4) 值 3
        let buffer: Vec<Sample> = (0..4u64)
            .map(|i| {
                let v = if i < 2 { 100.0 } else { 3.0 };
                Sample::new(i, i as f64 * 0.001, &[v.to_bits()])
            })
            .collect();
        // 初始人为抬高，验证重扫会修正
        panel.auto_y_max_abs = 200.0;
        panel.rescan_y_max(&buffer, 0.002, 0.004);
        assert!((panel.auto_y_max_abs - 3.0).abs() < 1e-9, "视野外峰值不应影响 Y 轴");
        // 空 buffer 不 panic、保持原值
        panel.auto_y_max_abs = 5.0;
        panel.rescan_y_max(&[], 0.0, 1.0);
        assert!((panel.auto_y_max_abs - 5.0).abs() < 1e-9);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test waveform 2>&1 | tail -20`
Expected: FAIL——`visible_range` / 带 x 范围的 `rescan_y_max` 未定义。

- [ ] **Step 3: 实现**——`src/ui/waveform.rs` 修改如下：

**(a) show() 签名**：`src/ui/waveform.rs:128-136`，把 `_visible_width: f32` 参数替换为 `view_points: usize`：

```rust
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        buffer: &[Sample],
        buffer_offset: u64,
        view_points: usize,
        has_new_data: bool,
        selected_channel: Option<usize>,
    ) -> WaveformOutput {
```

**(b) 删除 rescan_counter 字段与常量**：`src/ui/waveform.rs:56-57` 的 `rescan_counter: u32` 字段、`75` 行的 `RESCAN_EVERY_FRAMES` 常量、构造 `Self` 中的 `rescan_counter: 0`（`112` 行）、`src/ui/waveform.rs:212-220` 的定期重扫块：

```rust
        // 定期 Y 轴重扫：增量模式 Y 轴基准只增不减，峰值移出窗口后
        // 按当前窗口全量重扫修正（避免 Y 轴越拉越松）
        if has_new_data {
            self.rescan_counter += 1;
            if self.rescan_counter >= RESCAN_EVERY_FRAMES {
                self.rescan_counter = 0;
                self.rescan_y_max(buffer);
            }
        }
```

整块删除。

**(c) auto_scroll 视野宽度**：`src/ui/waveform.rs:264-286` 区域，drift 检测与 auto_scroll 窗口时长改用 `view_len`：

```rust
            // 用户拖拽/缩放检测：如果 X 轴右边界偏离了最新时间戳（超过半个窗口），
            // 说明用户在手动操作，自动暂停跟随
            if auto_scroll && buffer_len > 0 {
                let view_len = view_points.min(buffer_len);
                let window_duration = view_len as f64 * interval_us / 1_000_000.0;
                if let Some(prev) = prev_x_max {
                    // X 右边界变化超过 1% 窗口时长 → 用户在操作
                    let drift = (cur_x_max - prev).abs();
                    if drift > window_duration * 0.01 {
                        self.auto_scroll = false;
                    }
                }
            }

            // 自动滚动：X 轴跟随最新数据，窗口宽度 = min(视野点数, 可用数据)
            if auto_scroll && buffer_len > 0 {
                let view_len = view_points.min(buffer_len);
                let window_duration = view_len as f64 * interval_us / 1_000_000.0;
                let new_bounds = PlotBounds::from_min_max(
                    [latest_ts - window_duration, -y_bound],
                    [latest_ts, y_bound],
                );
                plot_ui.set_plot_bounds(new_bounds);
                self.last_x_max = Some(latest_ts);
            } else {
                self.last_x_max = Some(cur_x_max);
            }

            // 记录可视范围（用于下一帧的 X 轴标签）
            let bounds = plot_ui.plot_bounds();
            self.last_x_range = Some(bounds.max()[0] - bounds.min()[0]);

            // Y 轴自适应跟随视野：每帧按当前可视范围重扫（视野外峰值不影响）
            let vis_bounds = plot_ui.plot_bounds();
            self.rescan_y_max(buffer, vis_bounds.min()[0], vis_bounds.max()[0]);
```

**(d) 渲染裁剪**：`src/ui/waveform.rs:295-331` 渲染循环改为（关键行 `let visible = ...`）：

```rust
            // 捕获鼠标坐标（用于光标点击）
            let pointer_coord = plot_ui.pointer_coordinate();

            // 渲染各通道：选中加粗；有选中时其它通道降透明度
            // 只把可视范围内的点送入渲染管线（边界各扩展 1 点避免线段断裂），
            // 视野外不可见部分不绘制 —— 不是降采样，视野内每个原始点仍 100% 绘制。
            let vis_bounds = plot_ui.plot_bounds();
            let vis_min = vis_bounds.min()[0];
            let vis_max = vis_bounds.max()[0];
            let has_selection = selected_channel.is_some();
            for (ch_idx, ch) in self.channels.iter().enumerate() {
                if !ch.visible {
                    continue;
                }
                let points = &self.cached_points[ch_idx];
                if points.is_empty() {
                    continue;
                }
                let (start, end) = visible_range(points, vis_min, vis_max);
                if start >= end {
                    continue;
                }
                let visible = &points[start..end];
                let is_sel = selected_channel == Some(ch_idx);
                let color = if has_selection && !is_sel {
                    dim_color(ch.color, 0.38)
                } else {
                    ch.color
                };
                let line_w = if is_sel { 2.8 } else { 1.4 };
                let point_r = if is_sel { 2.4 } else { 1.5 };
                match self.display_mode {
                    WaveformDisplayMode::Line => {
                        plot_ui.line(
                            Line::new(visible)
                                .name(&ch.name)
                                .color(color)
                                .width(line_w),
                        );
                    }
                    WaveformDisplayMode::Point => {
                        plot_ui.points(
                            Points::new(visible)
                                .name(&ch.name)
                                .color(color)
                                .radius(point_r),
                        );
                    }
                }
            }
```

**(e) rebuild_cache 移除 Y 轴累计**：`src/ui/waveform.rs:383` 起，删除 `y_max_abs` 相关行：

```rust
        // 全量重建时重新累计 y_max_abs；仅脏通道更新时保持已有值
        // （非脏通道的 raw_f 未变，无需重新累计）
        let mut y_max_abs = if all_dirty { 0.0 } else { self.auto_y_max_abs };
        let mut found_non_finite = false;
```

改为：

```rust
        let mut found_non_finite = false;
```

循环体内的 `abs_val` 累计块：

```rust
                let abs_val = raw_f.abs();
                if abs_val > y_max_abs {
                    y_max_abs = abs_val;
                }
```

删除（两处：rebuild_cache 与 incremental_update）。

`src/ui/waveform.rs:428-434` 尾部：

```rust
        // Y 轴自适应: 仍按 raw_f,不被 offset/scale 推偏
        if all_dirty {
            self.auto_y_max_abs = if y_max_abs > 0.0 { y_max_abs } else { 1.0 };
        }
        // 记录本次重建是否遇到非有限值：
        // 决定后续帧是否退回每帧全量重建（避免增量计数错位）
        self.nan_in_stream = found_non_finite;
```

改为：

```rust
        // 记录本次重建是否遇到非有限值：
        // 决定后续帧是否退回每帧全量重建（避免增量计数错位）
        self.nan_in_stream = found_non_finite;
```

**(f) incremental_update 移除 Y 轴累计**：`src/ui/waveform.rs:449-450`：

```rust
        // Y 轴基准只增不减（定期重扫修正峰值移出窗口的情况）
        let mut y_max_abs = self.auto_y_max_abs;
```

删除；循环体内：

```rust
                let abs_val = raw_f.abs();
                if abs_val > y_max_abs {
                    y_max_abs = abs_val;
                }
                pts.push(PlotPoint::new(s.timestamp_sec, raw_f * scale + offset));
```

改为：

```rust
                pts.push(PlotPoint::new(s.timestamp_sec, raw_f * scale + offset));
```

尾部 `src/ui/waveform.rs:480`：

```rust
        self.auto_y_max_abs = y_max_abs;
        self.cache_buffer_len = buffer.len();
        self.cache_offset = buffer_offset;
```

改为：

```rust
        self.cache_buffer_len = buffer.len();
        self.cache_offset = buffer_offset;
```

**(g) rescan_y_max 改造为按视野扫**：`src/ui/waveform.rs:505-528` 整个函数替换为：

```rust
    /// 按可视 X 范围重扫 Y 轴基准（每帧调用，Y 轴跟随当前视野）。
    ///
    /// 只统计视野内的原始值（不被 offset/scale 推偏），视野外历史峰值不影响当前 Y 轴。
    /// buffer 时间戳单调递增，二分定位视野区间后扫描。
    fn rescan_y_max(&mut self, buffer: &[Sample], x_min: f64, x_max: f64) {
        if buffer.is_empty() {
            return;
        }
        let i_start = buffer.partition_point(|s| s.timestamp_sec < x_min);
        let i_end = buffer.partition_point(|s| s.timestamp_sec <= x_max);
        if i_start >= i_end {
            return;
        }
        let mut y_max_abs = 0.0f64;
        for (ch_idx, ch) in self.channels.iter().enumerate() {
            if !ch.visible {
                continue;
            }
            let vt = self.value_types.get(ch_idx).copied().unwrap_or(ValueType::Float);
            for s in &buffer[i_start..i_end] {
                let raw = s.values().get(ch_idx).copied().unwrap_or(0);
                let raw_f = vt.to_f64(raw);
                if raw_f.is_finite() {
                    let a = raw_f.abs();
                    if a > y_max_abs {
                        y_max_abs = a;
                    }
                }
            }
        }
        self.auto_y_max_abs = if y_max_abs > 0.0 { y_max_abs } else { 1.0 };
    }
```

**(h) 新增 visible_range 辅助函数**：放在 `dim_color` 函数（`src/ui/waveform.rs:618`）之前：

```rust
/// 计算可视范围内点的切片索引 `[start, end)`（含边界各 1 点扩展，避免线段断裂）。
///
/// `points.x` 必须单调递增（时间戳序列）。视野完全在数据左侧/右侧时
/// 返回含 1 个边界点的切片；空点集返回 `(0, 0)`。
fn visible_range(points: &[PlotPoint], vis_min: f64, vis_max: f64) -> (usize, usize) {
    if points.is_empty() {
        return (0, 0);
    }
    let start = points.partition_point(|p| p.x < vis_min).saturating_sub(1);
    let end = (points.partition_point(|p| p.x <= vis_max) + 1).min(points.len());
    (start, end)
}
```

**(i) app.rs 调用处**：`src/ui/app.rs:1259-1266`：

```rust
            let wave_out = self.waveform.show(
                ui,
                self.display_buf.all(),
                buffer_offset,
                available_width,
                has_new_data,
                self.selected_channel,
            );
```

改为（传视野点数）：

```rust
            let wave_out = self.waveform.show(
                ui,
                self.display_buf.all(),
                buffer_offset,
                self.controls.window_size,
                has_new_data,
                self.selected_channel,
            );
```

- [ ] **Step 4: 更新受影响的旧测试**——`src/ui/waveform.rs` tests 模块中：

**(a) `auto_y_uses_raw_not_transformed`（659-665 行）** 整段替换为：

```rust
    #[test]
    fn rescan_y_uses_raw_not_transformed() {
        // rescan 统计 raw 值，不应受 offset/scale 影响
        let mut panel = make_panel(vec![("CH1", 1.0e6, 1.0)]);
        let buffer = make_buffer(1, &[1.0f32.to_bits()]);
        panel.rescan_y_max(&buffer, 0.0, 0.001);
        // raw_f = 1.0, 不应受 offset 影响
        assert!((panel.auto_y_max_abs - 1.0).abs() < 1e-6);
    }
```

**(b) `rescan_y_max_resets_drifted_scale`（807-813 行）** 整段替换为：

```rust
    #[test]
    fn rescan_y_max_resets_drifted_scale() {
        let mut panel = make_panel(vec![("CH1", 0.0, 1.0)]);
        panel.auto_y_max_abs = 100.0; // 人为抬高（模拟峰值已移出视野）
        let buf = make_buffer(5, &[3.0f32.to_bits()]);
        panel.rescan_y_max(&buf, 0.0, 0.005);
        assert!((panel.auto_y_max_abs - 3.0).abs() < 1e-9);
    }
```

**(c) `incremental_update_trims_and_appends` 中 744 行** 删除：

```rust
        // Y 轴基准增长到新点的最大值
        assert!((panel.auto_y_max_abs - 2.0).abs() < 1e-9);
```

**(d) `incremental_update_applies_transform_to_new_points` 中 771 行** 删除：

```rust
        // Y 轴基准 = 新点 raw 的最大值 8.0
        assert!((panel.auto_y_max_abs - 8.0).abs() < 1e-9);
```

- [ ] **Step 5: 运行全部测试**

Run: `cargo test 2>&1 | tail -25`
Expected: PASS——全部测试通过（含新增 visible_range / rescan_y_max 测试，且旧测试更新后通过）。

- [ ] **Step 6: 编译检查**

Run: `cargo build 2>&1 | tail -5`
Expected: 编译通过，无警告。

- [ ] **Step 7: Commit**

```bash
git add src/ui/waveform.rs src/ui/app.rs
git commit -m "feat(waveform): Window 控制视野宽度、渲染裁剪可视区、Y 轴跟随视野重扫"
```

---

### Task 8: 全量验证

**Files:** 无（验证任务）

- [ ] **Step 1: 全量测试**

Run: `cargo test 2>&1 | tail -15`
Expected: 全部通过，0 failed。

- [ ] **Step 2: clippy 零警告**

Run: `cargo clippy --all-targets 2>&1 | tail -15`
Expected: 无 warning。

- [ ] **Step 3: release 构建**

Run: `cargo build --release 2>&1 | tail -5`
Expected: 构建成功。

- [ ] **Step 4: 人工验证清单**（用户操作，构建者提供 exe 路径提示）

1. 打开 General Settings → 调大缓冲区容量（如 500k）→ Save Project → Open Project → 容量恢复 500k
2. Start 采样 → Pause → 往回拖动 → 可看到超过一个 Window 宽度的历史数据
3. 拖动到不同位置 → Y 轴随视野段自适应
4. 运行中点 Follow → 回到最新数据，框内显示 window 个点
5. 运行中改容量 → 状态栏提示 applies on next Start；Pause→Start 恢复时缓冲重建
6. 旧工程文件（无 buffer_capacity 字段）打开 → 默认 200k，不报错

- [ ] **Step 5: 收尾说明**——将验证结果汇总报告，等待用户确认后再决定是否提交剩余改动（如无剩余改动则无需操作）。
