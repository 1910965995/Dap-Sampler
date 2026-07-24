# 变量浏览器树状图设计

**日期:** 2026-07-24  
**状态:** 已批准  
**方案:** A — UI 侧从扁平 path 重建树

## 1. 背景

DAP Sampler 左侧「变量浏览器」在加载 ELF 后列出 DWARF 解析出的全局变量，供用户勾选后采样。

后端（`src/elf/tree.rs`）已将结构体成员、数组元素展开为**叶子** `ElfVariable`，path 形如：

- 结构体：`pid.kp`、`pid.ki`
- 数组：`buffer[0]`、`buffer[1]`
- 嵌套：`motors[1].speed`

当前 UI（`src/ui/variable_browser.rs`）按扁平列表渲染，仅对有 `parent_path` 的项增加固定缩进，**不能折叠/展开**，结构关系不清晰。

## 2. 目标

将结构体与数组以**可折叠树**展示，用户可展开到某一结构体成员或数组元素；仅叶子可勾选用于采样。

### 2.1 范围内

- 在 `variable_browser` 内从扁平 `ElfVariable` 列表按 path 重建树并渲染
- 展开/折叠状态（默认折叠）
- 搜索：只显示匹配分支，并自动展开祖先
- 保留按 `source_file` 分组（缺省 `Globals`）
- 勾选叶子 path 与通道面板联动语义不变

### 2.2 范围外

- 修改 `tree.rs` / DWARF 展开逻辑
- 父节点勾选、整对象采样
- 父节点类型名 / sizeof 等元数据展示（可后续迭代）
- 改动采样引擎、Watch、通道上限等

### 2.3 成功标准

- `pid` 可展开看到 `kp/ki/kd`；`buffer` 可展开看到 `[0]..[n]`
- 勾选叶子后 path 仍进入通道（与现有 `app.rs` 行为一致）
- 搜索 `kp` 时仅见相关分支且祖先已展开
- `cargo test` 通过；为 path 解析与建树补充单测

## 3. 交互约定

| 项 | 约定 |
|----|------|
| 父节点（结构体名 / 数组名 / 中间路径段） | 仅 ▶/▼ 展开折叠，**无 checkbox** |
| 叶子 | checkbox + 显示名（成员名或 `[i]`） |
| 默认展开 | 全部折叠 |
| 搜索 | 大小写不敏感匹配 name 或 path；只保留匹配叶子及其祖先；祖先**强制展开**（不永久改写用户折叠偏好） |
| 分组 | 仍按 `source_file`，组内为树 |

## 4. 架构

**方案 A：只改 UI，不改 ELF 数据模型。**

```
ElfContext.variables (扁平叶子)
        │
        ▼
  path 分段 + 建树 (纯函数)
        │
        ▼
  搜索过滤 / 临时展开
        │
        ▼
  egui 递归渲染
        │
        ▼
  勾选变更 Vec<(path, bool)> → app.rs（不变）
```

不引入新的 `ElfContext` 树结构；不改 `build_from_dwarf` 输出。

## 5. 数据模型（UI 内部）

```rust
struct TreeNode {
    label: String,           // "pid" | "kp" | "[0]"
    path: String,            // 完整 path："pid.kp" | "buffer[0]"
    is_leaf: bool,
    // 叶子持有对应变量引用或索引；容器节点无
    variable_index: Option<usize>,
    children: Vec<TreeNode>,
}
```

`VariableBrowser` 状态扩展：

```rust
pub struct VariableBrowser {
    search_text: String,
    pub checked_paths: HashSet<String>,
    expanded: HashSet<String>,  // 用户展开的节点 path
}
```

## 6. Path 解析

将完整 path 拆成段，同时支持 `.` 成员与 `[i]` 下标：

| path | 段 |
|------|-----|
| `temperature` | `["temperature"]` |
| `pid.kp` | `["pid", "kp"]` |
| `buffer[0]` | `["buffer", "[0]"]` |
| `motors[1].speed` | `["motors", "[1]", "speed"]` |

规则建议：

1. 用正则或扫描：标识符一段；`[数字]` 单独一段；`.` 仅作分隔不入段。
2. 非法/无法解析的 path：作为单段叶子，避免丢变量。

建树：

1. 按 `source_file` 分组。
2. 组内对每个叶子 path 逐段插入：中间段无变量则为容器节点；最后一段为叶子并挂上 `ElfVariable`。
3. 同 path 节点合并 children。
4. 子节点排序：label 形如 `[N]` 的按 N 数值升序，其余按 label 字典序；排序稳定。

显示名 `label` 取**最后一段**，避免数组展开时 `name` 字段已是完整 path 导致重复显示。

## 7. 搜索过滤

当 `search_text` 非空：

1. 在**完整树**上标记：叶子若 `name` 或 `path` 包含查询串（小写比较）则命中。
2. 剪枝：仅保留「自身命中或子孙命中」的节点。
3. 渲染时，剪枝结果中所有非叶子节点视为展开（临时），**不必**写入 `expanded`；清空搜索后恢复用户 `expanded`。

当 `search_text` 为空：按 `expanded` 决定是否显示 children。

## 8. UI 渲染

保持现有：

- 搜索框 hint
- `Variables: N | Selected: M`
- `ScrollArea` + `max_height`

变更：

- 组标题下递归画树，而非扁平 for 循环。
- 行布局：
  - 容器：indent + 展开控件 + label
  - 叶子：indent + checkbox + label
- 控件 id 使用 path（`id_salt` / `Id::new(("var_tree", path))`），避免折叠状态串扰。
- 返回值仍为 `Vec<(String, bool)>` 勾选变更，供 `app.rs` 使用。

实现可用 egui `collapsing` 或自绘三角 + 条件画 children；自绘更易统一「无 checkbox 的父节点」与搜索临时展开。

## 9. 与现有代码的衔接

| 模块 | 影响 |
|------|------|
| `src/ui/variable_browser.rs` | 主要改动 |
| `src/ui/app.rs` | 原则上不改；仍消费 `show()` 的 changes |
| `src/elf/*` | 不改 |
| 通道 / Watch | 不改；仍只认叶子 path |

注意：当前数组展开在 `tree.rs` 中元素 `name` 可能为完整 path 风格；UI 一律用 path 最后一段作 label，不依赖 `name` 与 path 一致。

## 10. 测试计划

纯函数优先，便于单测：

1. **path 分段**：上表用例 + 边界（空串、仅 `[0]`、连续点等降级策略）。
2. **建树**：结构体 3 成员、数组 4 元素、嵌套 `a[1].b`；断言 children 结构与叶子 address 关联正确。
3. **搜索剪枝**：匹配深层叶子时祖先保留、无关兄弟删除。
4. **回归**：现有 `tests/test_variable_tree.rs` 等全绿。

测试可放在 `tests/test_variable_browser.rs`，或将 helper 标为 `pub(crate)` / 同 crate 单测。

## 11. 风险与缓解

| 风险 | 缓解 |
|------|------|
| path 含非常规字符导致分段错误 | 解析失败时整 path 单叶；加单测 |
| 超大数组（展开上限 256）树很深/很宽 | 默认折叠；ScrollArea；后续可虚拟列表 |
| egui id 冲突导致展开错乱 | path 作为稳定 id |
| 搜索临时展开与用户 expanded 混淆 | 搜索态不写 `expanded`，或渲染层覆盖 |

## 12. 实现顺序建议

1. 实现 `split_path` + `build_tree` 纯函数与单测
2. 实现搜索剪枝纯函数与单测
3. 改 `VariableBrowser::show` 渲染树 + 展开状态
4. 手动/GUI 冒烟：加载含 struct/array 的 ELF，勾选叶子进通道
5. `cargo test` 全量

## 13. 决策记录

- **父节点勾选：** 否（仅叶子）— 用户选择 A
- **搜索行为：** 过滤分支 + 自动展开祖先 — 用户选择 A
- **实现路线：** UI 重建树，不改 ELF 树构建 — 用户选择 A
