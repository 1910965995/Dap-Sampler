# DAP Sampler 缓冲区容量设置与波形历史回看设计

日期：2026-08-02
状态：已批准

## 背景与动机

当前显示缓冲区（DisplayBuffer）容量与 Window 参数耦合：容量 = `window_size`（默认 2000 点 @ 20kHz ≈ 0.1 秒），超过即丢弃最旧数据。用户暂停波形后往回拖动时，只能看到 buffer 中仅存的 window 个点——更早的数据已被丢弃，无法回看历史波形。

目标：让 Window 参数只控制"示波器框内显示多少个点"（视野宽度），历史数据单独保留，暂停后拖动可浏览全部历史。

## 需求

- 暂停/停止后拖动波形，可回看更早的历史数据（上限 = 缓冲区容量）
- Window 参数只控制示波器框内显示的数据点数（视野宽度）
- 新增 Settings(S) 菜单（位于 Files(F) 与 Help(H) 之间），展开含 "General Settings..."，点击弹出 General Settings 窗口
- General Settings 窗口可设置"缓冲区容量"：默认 200_000 点，范围 1_000 ~ 1_000_000 点
- 缓冲区容量随工程保存/加载（与 window_size 一致）
- 缓冲区容量改动在**下次 Start 生效**（与 SWD Clock 一致），运行中修改不打断当前采集

## 设计

### 1. 数据模型与工程兼容

`DapSamplerApp` 新增字段：

```rust
/// 缓冲区容量（历史数据保留量，采样率相关的采样点数上限）
buffer_capacity: usize,  // 默认 200_000
```

常量放 `controls.rs`（与 `WINDOW_MIN/MAX` 同处）：

```rust
pub const BUFFER_CAPACITY_MIN: usize = 1_000;
pub const BUFFER_CAPACITY_MAX: usize = 1_000_000;
pub const DEFAULT_BUFFER_CAPACITY: usize = 200_000;
```

`ProjectFile` 新增字段 `buffer_capacity: usize`，反序列化兼容旧工程：

```rust
#[serde(default = "default_buffer_capacity")]
pub buffer_capacity: usize,
```

- `default_buffer_capacity()` 返回 `DEFAULT_BUFFER_CAPACITY`；旧工程文件缺字段时自动用 200k，**不 bump `PROJECT_VERSION`**（保持 1）
- `save_project` 写入 `self.buffer_capacity`；`open_project` 恢复（clamp 到 1k~1M）；`new_project` 回默认 200k

**Window 参数语义变化**：`window_size` 不再控制缓冲容量，只作视野宽度。移除运行中改 Window 时对 `DisplayBuffer::set_max_samples` 的调用（app.rs:1227），视野即时变化，不影响已存历史。

### 2. 菜单与 General Settings 窗口

菜单栏顺序变为：`Project(P) | Files(F) | Settings(S) | Help(H)`。

- Settings(S) 展开后一项 "General Settings..."（整行可点击、左对齐、无图标，与现有菜单项一致）
- 点击 → `show_settings = true`，弹出 `egui::Window::new("General Settings")`（与 About 窗口样式一致：不可折叠、可关闭）

窗口内容（仅一项，YAGNI）：

- `Buffer capacity:` + `DragValue`（范围 1k~1M，speed 1000，suffix ` pts`）
- 下一行小字实时换算：`≈ 10.0 s @ 20000 Hz`（按当前采样率换算）
- 修改直接更新 `self.buffer_capacity`

生效时机（与 SWD Clock 一致）：

- Idle 状态改：直接生效（下次 Start 用新容量），不提示
- Running/Paused 状态改：状态栏提示 `Buffer capacity changed to N points (applies on next Start)`，当前采集不受影响
- 关闭窗口无额外动作（值已实时更新）

**Pause → Start 恢复一致性**（app.rs:570 `config_unchanged` 判断）：把 `buffer_capacity` 纳入判断——Pause 期间改了容量即视为配置变化，恢复 Start 时重建缓冲（丢弃旧历史）。

### 3. 缓冲与数据流

**环形缓冲参数化**（engine.rs:104）：

```rust
// 硬编码 RingBuffer::new(200_000) 改为：
pub fn start(&self, capacity: usize) -> Result<PipelineHandle> {
    let ring = Arc::new(RingBuffer::new(capacity));
    ...
}
```

`start_acquisition` 调用时传 `self.buffer_capacity`。

**DisplayBuffer 容量**（app.rs）：

- `start_acquisition` 中 `DisplayBuffer::new(self.window_size)` → `DisplayBuffer::new(self.buffer_capacity)`
- `new_project` 分支（app.rs:265）同样改为 `buffer_capacity`
- Pause→Start 保留历史分支（app.rs:575）同样用 `buffer_capacity`

两个缓冲容量保持一致 = `buffer_capacity`；"能回看的历史"完整等于环形缓冲中未被覆盖的数据（默认 200k @ 20kHz ≈ 10 秒）。

数据流不变：`采样线程 → 环形缓冲 → 每帧 drain → DisplayBuffer → 波形`。

### 4. 波形视野与渲染（waveform.rs）

**`show()` 增加视野参数**：

```rust
pub fn show(&mut self, ui, buffer, buffer_offset, view_points: usize, has_new_data, selected_channel) -> WaveformOutput
```

由 app.rs 传入 `self.controls.window_size`。

**auto_scroll（Follow）视野宽度**：

- 现在：X 轴窗口 = 整个 buffer 宽度（`buffer.len() * interval`）
- 改为：X 轴窗口 = `min(view_points, buffer.len())` 个点对应时长，右边界仍 = 最新时间戳

**渲染裁剪**：

- `cached_points` 仍缓存全部历史（增量更新逻辑不变）
- 绘制时按当前可视 X 范围 `[x_min, x_max]` 对每通道缓存二分查找，只把视野内切片传给 `Line::new`（`PlotPoint` 是 Copy，切片引用零拷贝）
- 边界处各扩展 1 个点，避免线段在框边缘断裂
- 非降采样：视野内每个原始点仍 100% 绘制，仅视野外不可见部分不送入渲染管线

**Y 轴自适应跟随视野**：

- 移除 `rebuild_cache` / `incremental_update` 中的 y_max 累计逻辑（原基于全量历史，历史变大后 Y 轴被历史峰值撑松）
- 改为每帧按当前可视 X 范围扫 buffer 原始值求 max_abs（2000 点 × 8 通道 ≈ 16k 次比较，可忽略）
- **仅 Follow（auto_scroll）模式下自动适配**：Follow 开启时每帧按当前视野重扫并应用；手动拖动/暂停回看时 Y 轴保持用户控制（不动），避免与用户手动 Y 缩放操作冲突（用户已确认此行为，2026-08-02）
- `rescan_counter` 定期重扫机制删除（每帧重扫替代）
- 保留"按 raw 值、不被 offset/scale 推偏"的语义

**拖拽/缩放检测与 Follow 按钮**行为不变：用户拖动 → 自动关闭跟随；点 Follow → 回到最新数据。

### 5. 光标与边界情况

- 光标：无改动。点击定位用 `timestamp_sec` 二分查找；视野外部分由 plot 自动裁剪
- 视野 > 历史（如 Window=100k 但刚 Start 只有 500 点）→ `min` 逻辑兜底，显示全部现有数据
- 暂停后 pipeline 仍在跑，历史持续累积到 buffer_capacity，拖动可回看已累积历史
- buffer_capacity 设为 1k 时，Window > 1k 则视野退化为全部历史（min 兜底）
- 采样率 100kHz + 容量 1M：约 10s 历史，PlotPoint 内存 ≈ 1M × 8ch × 16B ≈ 128MB（预期上限）

## 非目标

- 不做波形降采样（保持"原始样本点 100% 绘制"约束）
- 不做多级历史（如磁盘落盘、按需分页）
- General Settings 窗口暂不放其他设置项
- 不改动光标交互、不改动 Follow/拖拽检测逻辑

## 测试

- `project.rs`：round_trip 含 `buffer_capacity`；旧 JSON（无该字段）反序列化 → 默认 200k；`new_project` 回默认值
- `waveform.rs`：auto_scroll 视野宽度 = `min(view_points, len)`；视野裁剪切片范围正确（含边界扩展点）；Y 轴视野重扫（不同视野段 → max_abs 跟随该段）；现有增量缓存测试保留
- `engine.rs`：`start(capacity)` 参数化后环形缓冲容量正确
- `display_buffer.rs`：现有 `set_max_samples` 测试保留
- 编译通过、clippy 零警告、release 构建成功
