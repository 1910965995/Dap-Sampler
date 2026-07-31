# DAP Sampler 工程保存/打开功能设计

日期：2026-08-01
状态：已获用户批准（2026-08-01）

## 背景与目标

用户每次使用 DAP Sampler 时都需要重新加载 ELF、勾选变量、重新设置 Y 轴偏移/缩放、备注等，重复劳动。本功能让用户把当前工作现场保存为工程文件（`.dapsproj`），下次直接打开即可恢复全部配置，无需从头添加。

## 需求（用户原话要点）

- 保存当前工程：记录 **ELF 文件路径**、**Watch 窗口变量**（含 Y 轴偏移、Y 轴缩放、备注、刷新时间）
- 支持打开工程：恢复全部配置
- 菜单栏三个按钮：**新建工程、打开工程、保存工程**
- 全局参数（采样率、SWD 时钟、窗口大小、Line/Point 显示模式）一并保存（用户已确认）

## 已确认的决策

| 决策点 | 结论 |
|--------|------|
| 保存范围 | 全部保存：ELF 路径 + Watch 变量配置 + 采样率/SWD 时钟/窗口大小/显示模式 |
| 文件格式 | JSON（引入 serde + serde_json） |
| 实现架构 | 独立 `src/project.rs` 模块（方案 A），UI 逻辑只进 `app.rs` |

## 1. 数据模型（新文件 `src/project.rs`）

```json
{
  "version": 1,
  "elf_path": "C:/fw/firmware.elf",
  "sample_rate_hz": 5000,
  "swd_clock_mhz": 10,
  "window_size": 2000,
  "display_mode": "line",
  "variables": [
    {
      "name": "pid.kp",
      "address": 536871168,
      "value_type": "float",
      "refresh_period_s": 0.1,
      "y_offset": 0.0,
      "y_scale": 2.0,
      "remark": "比例系数"
    }
  ]
}
```

- 结构体：`ProjectFile`（顶层）+ `ProjectVariable`（变量项），均派生 `Serialize`/`Deserialize`
- `value_type` / `display_mode` 用 `#[serde(rename_all = "lowercase")]` 序列化为小写字符串；
  读入时恢复规则（宽容回退，不因单个字段失败而整体报错）：
  - `value_type` 用现有 `ValueType::parse`，解析失败回退 `Uint32`
  - `display_mode` 解析失败回退 `Line`
- `version` 字段 = 1，读入时校验；`version != 1` 报错「工程文件版本不支持」

### 模块接口

```rust
pub struct ProjectFile { ... }        // 字段同 JSON schema
pub struct ProjectVariable { ... }

impl ProjectFile {
    pub fn to_json(&self) -> String;                            // serde_json 序列化（pretty 格式）
    pub fn from_json(text: &str) -> Result<ProjectFile>;        // 反序列化 + version 校验
}
```

- 复用 crate 现有 `Error` 类型（`error.rs`），错误信息中文
- 模式判定：`elf_path: Some` = ELF 模式；`None` = 手工地址模式（打开时恢复 `manual_*` 字段）

## 2. 依赖

`Cargo.toml` `[dependencies]` 新增：

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

其余零新增（文件对话框用项目已有的 `rfd`）。

## 3. UI 集成（`src/ui/app.rs`）

### 新字段

```rust
project_path: Option<String>,   // 当前工程文件路径；None = 未保存过
```

### 三个按钮

顶部 header（标题 "DAP Sampler" 之后、状态圆点之前）加：

- `📁 新建工程` — `new_project()`
- `📂 打开工程` — rfd 打开对话框（过滤 `.dapsproj`）→ `open_project(path)`
- `💾 保存工程` — `save_project()`

按钮旁小字显示当前工程文件名（如 `project: firmware.dapsproj`），无工程时显示 `project: (未保存)`。

### 三个操作方法

**`new_project()`**
1. 若正在采集 → `stop_acquisition()`（安全释放 USB）
2. 清空：`elf_ctx = None`、`variable_browser.clear()`、`channel_panel.channels` 清空、
   `watch_panel.entries` 清空、`manual_addresses` / `manual_channel_names` / `manual_value_types` 清空
3. 参数回默认：采样率 5000 Hz、窗口 2000 点、SWD 时钟 1 MHz、显示模式 Line
   （与 `DapSamplerApp::new` 默认一致；`controls` 与 `waveform` 同步）
4. `project_path = None`；清空 `display_buf` / `cursor` / 计数

**`save_project()`**
1. 收集数据：
   - `elf_path`：`elf_ctx.as_ref().map(|c| c.file_path.clone())`（`ElfContext.file_path` 已存在，加载时即记录）
   - `variables`：遍历 `watch_panel.entries`，取 `name / address / value_type / refresh_period_s / y_offset / y_scale / remark`
   - 全局参数：`controls.sample_rate` / `controls.swd_clock_mhz` / `controls.window_size` / `waveform.display_mode()`
2. 目标路径：`project_path` 已有 → 直接覆盖写入；否则 rfd 保存对话框
   （默认文件名 `dap-sampler.dapsproj`，过滤 `.dapsproj`）
3. 写文件（pretty JSON）；成功 → `project_path = Some(path)`；失败 → `set_error`，路径不变

**`open_project(path)`**
1. 若正在采集 → `stop_acquisition()`
2. `ProjectFile::from_json` 解析失败 → `set_error`（显示原因），**当前状态不动**，直接返回
3. 加载 ELF（`elf_path` 为 Some 时）：
   - 成功 → `elf_ctx = Some(ctx)`，`variable_browser` 重新加载变量树
   - 失败 → `set_error` 警告（ELF 缺失/损坏），**继续**：变量地址已在工程里，仍可正常采样
4. 恢复通道与 Watch：
   - **ELF 模式**（`elf_path` 存在）：`variable_browser.checked_paths` = 全部变量名，
     构造 `changes: Vec<(String, bool)>`（全 true）调用现有 `apply_variable_changes`（app.rs:399）
     —— 该方法按勾选重建 `channel_panel` 并 `sync_from_channels` 重建 `watch_panel`，
     自带 8 通道上限保护（工程文件被手工编辑超限时自动截断，与 GUI 限制一致）
   - **手工模式**（`elf_path` 为 None）：恢复 `manual_addresses` / `manual_channel_names`（格式 `CH{i+1} 0x...`，与构造器一致）/ `manual_value_types`，重建 `channel_panel` 与 `watch_panel`
   - 两种模式最后都用 `ProjectVariable` 覆盖 `watch_panel.entries` 的配置字段
     （name/address/type 以工程为准，`display_value`/编辑缓冲等运行时状态重置）
5. 恢复参数：`controls.sample_rate`、`controls.swd_clock_mhz`、`controls.window_size`，
   `waveform.set_interval`（按恢复后的采样率）、`waveform` 显示模式
6. 清空运行时状态：`display_buf` 重建、`cursor.clear()`、计数归零、`state = Idle`、`selected_channel = None`
7. `project_path = Some(path)`；状态栏提示「已打开工程」

## 4. 错误处理

| 场景 | 行为 |
|------|------|
| JSON 损坏 / 解析失败 | `set_error` 显示具体原因，当前状态不动 |
| `version != 1` | `set_error`「工程文件版本不支持」 |
| ELF 路径缺失/损坏 | `set_error` 警告，但用工程内地址继续，可正常采样 |
| 保存路径不可写 | `set_error`，`project_path` 不变 |
| 打开/新建时正在采集 | 自动先 `stop_acquisition()` |

## 5. 测试

`project.rs` 单元测试（`src/project.rs` 内 `#[cfg(test)]`）：

1. **round-trip 全字段一致**：含中文 remark、负 y_scale、`value_type = "int16"`、手工模式（`elf_path = None`）
2. **损坏 JSON** → `Err`，错误信息包含原因
3. **版本不匹配**（`version = 99`）→ `Err`「版本不支持」
4. **空 variables 列表** → 序列化/反序列化正常
5. **非法 value_type 字符串** → 回退 `Uint32`（不报错）
6. **非法 display_mode** → 回退 `Line`（不报错）

现有 171 个测试保持全绿。

## 6. 非目标（YAGNI）

- 不保存采样数据/波形历史（只保存配置）
- 不做「未保存修改」确认对话框
- 不做多工程最近打开列表
- 不做 ELF 路径相对化（保存绝对路径；用户移动工程文件+ELF 时自行处理）

## 7. 涉及文件

| 文件 | 改动 |
|------|------|
| `Cargo.toml` | +serde、+serde_json |
| `src/project.rs` | 新增：数据模型 + 序列化 + 单测 |
| `src/lib.rs` | 导出 `project` 模块 |
| `src/ui/app.rs` | 3 个按钮 + 3 个方法 + `project_path` 字段（ELF 路径复用 `ElfContext.file_path`，勾选恢复复用 `apply_variable_changes`） |
