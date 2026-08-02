# DAP Sampler ELF 变化检测与地址同步设计

日期：2026-08-02
状态：已批准

## 背景与动机

打开工程时，Watch 窗口显示的地址来自工程文件保存值，而实际采样用的地址来自 ELF 文件重新查询——固件重编译后变量地址变化时，两者不一致，Watch 显示会误导用户。此外，Idle 状态下 ELF 文件被修改（重新编译覆盖）时用户无感知，重新采样可能用了旧解析结果。

## 需求

1. **打开工程时**：ELF 加载成功后，若工程变量与 ELF 变量存在差异（地址不同 / ELF 中缺失），弹窗询问 "The elf file has changed. Should it be synchronized"，提供 Yes / No 按钮
   - **Yes**：地址统一用 ELF 文件的值；ELF 中缺失的变量删除
   - **No**：统一用工程文件记录的地址（采样与 Watch 都用手工快照）
2. **Idle 状态检测**：ELF 文件 mtime 变化（被修改/删除）时弹窗提示 "The ELF file has changed on disk. Reload it?"，提供 Yes / No
   - **Yes**：重新解析 ELF 文件（更新 elf_ctx）
   - **No**：保持现状，记录快照（文件再次变化时才再弹）

## 设计

### 1. 数据模型与差异检测

app.rs 新增：

```rust
/// 打开工程时 ELF 与工程变量的差异（触发同步询问弹窗）
#[derive(Clone)]
struct ElfSyncPrompt {
    /// 地址不同的变量：(变量名, 工程地址, ELF 地址)
    address_diffs: Vec<(String, u32, u32)>,
    /// ELF 中不存在的变量名（选 Yes 时删除）
    missing: Vec<String>,
}
```

纯函数：

```rust
/// 比较工程变量与 ELF 变量：地址不同或 ELF 中缺失 → 返回差异；无差异 → None
fn compute_elf_diffs(
    project_vars: &[ProjectVariable],
    elf_vars: &[ElfVariable],
) -> Option<ElfSyncPrompt>
```

- 匹配规则与 `collect_channel_addresses` 一致：`v.path == ev.path || v.name == ev.name`
- 找到但地址不同 → `address_diffs`；找不到 → `missing`；两者皆空 → None（不弹窗）

DapSamplerApp 新增字段：

```rust
/// 打开工程时的 ELF 同步询问（Some = 弹窗挂起等待用户选择）
elf_sync_prompt: Option<ElfSyncPrompt>,
/// Idle 状态下检测 ELF 文件变化的弹窗（文件 mtime 变化）
elf_change_prompt: bool,
/// 上次记录的 ELF 文件修改时间（变化检测基准）
elf_mtime: Option<std::time::SystemTime>,
```

### 2. 打开工程的同步弹窗流程

**open_project**（ELF 加载成功分支 `elf_loaded = true`）：

- 现有恢复流程之后调用 `compute_elf_diffs(&file.variables, &elf_ctx.variables)`：
  - `Some(diff)` → `self.elf_sync_prompt = Some(diff)`（弹窗挂起；open_project 正常完成，Watch 先按工程值恢复）
  - `None` → 无弹窗，行为与现状一致
- `open_project` 开头清空 `elf_sync_prompt`（防残留）

**弹窗渲染**（update，居中，右上角 X 关闭，与其他窗口并列）：

```
Window "ELF Synchronization"
The elf file has changed. Should it be synchronized?
  • N variables have different addresses
  • M variables not found in the ELF file
[ Yes ]  [ No ]
```

**Yes → `apply_elf_sync(&prompt)`**（统一 ELF 地址）：

1. 删除缺失变量：`for name in &prompt.missing { self.channel_panel.remove_by_name(name); }`
2. Watch 重建：`sync_from_channels(names, types, collect_channel_addresses())`（ELF 上下文下返回 ELF 地址，覆盖第 4 步写入的工程值）
3. `manual_addresses.clear()`（确保后续 Start 走 ELF 分支）
4. `set_status("ELF synchronized: N addresses updated, M variables removed")`
5. 更新 `elf_mtime` 快照

**No → `use_project_addresses()`**（统一工程地址）：

1. `manual_addresses` / `manual_channel_names` / `manual_value_types` 取工程文件值
2. `channel_panel` 按工程变量重建（`add_channel`）
3. Watch `sync_from_channels(names, types, manual_addresses)`
4. `elf_ctx` 保留（变量浏览器仍可浏览 ELF；采集用手工快照 → Start 时地址 = 工程值）
5. `set_status("Using saved variable addresses (ELF not synchronized)")`
6. 更新 `elf_mtime` 快照

### 3. Idle 状态 ELF 文件变化检测

**mtime 快照更新时机**：`import_elf` 成功、`open_project` ELF 加载成功、同步弹窗 Yes/No 应用后、Reload 成功/失败后。

**检测逻辑**（update 每帧，全部条件满足时）：

```rust
if self.controls.state == AcquisitionState::Idle
    && self.elf_ctx.is_some()
    && self.elf_sync_prompt.is_none()
    && !self.elf_change_prompt
    && self.elf_warning.is_none()
{
    let current = self.elf_ctx.as_ref()
        .and_then(|ctx| std::fs::metadata(&ctx.file_path).ok())
        .map(|m| m.modified().unwrap_or(std::time::UNIX_EPOCH));
    if current != self.elf_mtime {
        self.elf_change_prompt = true;
    }
}
```

- 文件被修改（mtime 变）、被删除（current = None ≠ 快照）都触发
- Running/Paused 或其他弹窗存在时不检测

**轮询唤醒**（update 末尾）：

```rust
if self.controls.state == AcquisitionState::Idle
    && self.elf_ctx.is_some()
    && self.elf_sync_prompt.is_none()
    && !self.elf_change_prompt
{
    ctx.request_repaint_after(std::time::Duration::from_secs(1));
}
```

每秒唤醒一次做检测（一次 `fs::metadata`，开销可忽略）。

**弹窗**（与同步弹窗互斥，居中，右上角 X）：

```
Window "ELF Changed"
The ELF file has changed on disk. Reload it?
[ Yes ]  [ No ]
```

- **Yes → reload**：重新 `ElfParser::load` → 成功 `elf_ctx = Some(new)` + `set_status("ELF reloaded: N variables")`；失败 `set_error("ELF reload failed: ...")`；随后更新 mtime 快照
- **No → 保持现状**：关闭弹窗 + 更新 mtime 快照（记录当前状态，文件再次变化时才再弹）

**弹窗 X 关闭语义**：两个弹窗的右上角 X 均等价于 No（同步弹窗 = 采用工程地址；变化弹窗 = 保持现状 + 更新快照），避免留下"Watch 显示与采样地址不一致"或"重复弹窗"的中间状态。

### 4. 测试

- `compute_elf_diffs` 纯函数：无差异 → None；地址不同 → Some(diffs)；缺失 → Some(missing)；混合；匹配规则一致
- `apply_elf_sync`：缺失变量从 channel_panel / Watch 移除；Watch 地址更新为 ELF 地址；manual_addresses 保持清空
- No 分支：manual_addresses / manual_channel_names / manual_value_types = 工程值；Watch 地址 = 工程值；elf_ctx 保留
- 现有 `open_project_missing_elf_sets_error_status`（ELF 缺失弹窗）保持通过（该场景 `elf_loaded = false`，不触发同步弹窗）

## 非目标

- 不做 Running/Paused 状态下的 ELF 变化检测
- 不做文件内容级对比（仅 mtime 变化检测；差异判定在打开工程时按变量地址比对）
- 不自动保存同步结果到工程文件（同步仅影响当前会话；用户可手动 Save Project 持久化）

## 测试

- 上述单元测试
- 现有全部测试保持通过
- 编译通过、clippy 零警告、release 构建成功
