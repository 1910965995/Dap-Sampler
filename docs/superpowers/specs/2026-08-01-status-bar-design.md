# DAP Sampler 底部状态栏设计

日期：2026-08-01
状态：已批准

## 背景与动机

当前用户的关键操作（ELF 导入、参数设置等）只在 `log::info!` 输出到控制台，GUI 内不可见；故障提示（如"未选择任何采样通道"）则显示在工具栏下方状态行末尾，甚至叠加在波形区域中央，位置不固定、观感混乱。

目标：在界面最底部增加一行横跨窗口的状态栏，集中显示用户关键操作与故障提示，让操作反馈一目了然。

## 需求

- 界面最底部一行状态栏，横跨整个窗口宽度，仅显示一行文字
- 只显示**最新一条**消息（新消息覆盖旧消息），纯文字、不带时间戳
- 显示内容：
  - 导入 ELF 解析出的变量数（如 `ELF loaded: 336 variables`）
  - 采样率设置变更
  - Window（显示窗口点数）设置变更
  - SWD 时钟设置变更
  - Display 模式设置变更
  - 采集开始 / 暂停 / 停止
  - 工程新建 / 保存 / 打开
  - 所有故障提示（错误）
- 移除现有工具栏下方状态行末尾的红色 Error 显示（错误统一移到底部状态栏，避免两处重复）

## 设计

### 数据模型

`DapSamplerApp` 中新增两个字段，替代现有 `last_error: Option<String>`：

```rust
/// 底部状态栏消息（None = 空）
status_text: Option<String>,
/// 状态消息是否为错误（错误显示红色）
status_is_error: bool,
```

新增两个方法：

```rust
/// 记录一条普通状态消息（底部状态栏，默认文字色）
fn set_status(&mut self, message: impl Into<String>);
/// 记录一条错误消息（底部状态栏，红色）
fn set_error(&mut self, message: impl Into<String>);
```

- `set_error` 复用现有 `set_error` 的全部 13 处调用点（语义不变，仅显示位置改变）
- `set_status` 插入约 8 处操作点（见"消息来源"）
- 原 `self.last_error = None` 的清空点（新建工程 / 打开工程 / 开始采集前）改为 `self.status_text = None`，语义不变（重置状态栏）

### UI 布局

- `egui::TopBottomPanel::bottom("status_bar")`，`resizable(false)`、`exact_height(24.0)`
- 声明位置：在 `watch_panel`（底部 Watch 面板）**之前**、`CentralPanel` **之前** show——egui 0.31 中**先声明的 bottom panel 位于界面最底部**，后声明的叠在其上方；因此状态栏贴窗口底边，Watch 面板叠在状态栏上方；CentralPanel 自动让出空间
- 内容：一行左对齐文字；错误消息红色 `Color32::from_rgb(0xff, 0x55, 0x55)`（与现有错误色一致），普通消息用主题默认文字色
- 消息为空时状态栏显示为空（占位不显示文字）

### 消息来源

| 事件 | 消息示例 | 类型 |
|------|---------|------|
| 导入 ELF 成功 | `ELF loaded: 336 variables` | 信息 |
| 导入 ELF 失败 | `ELF load failed: <err>` | 错误 |
| 采样率变更 | `采样率已变更为 5000 Hz` | 信息 |
| 窗口大小变更 | `显示窗口大小已变更为 2000 个点` | 信息 |
| SWD 时钟变更 | `SWD 时钟已变更为 1 MHz（下次连接 DAP-Link 时生效）` | 信息 |
| Display 模式变更 | `显示模式已切换为 Point` | 信息 |
| 采集开始 / 暂停 / 停止 | `开始采集` / `采集已暂停` / `采集已停止` | 信息 |
| 工程新建 / 保存 / 打开 | `已新建工程` / `已保存工程: xxx.dapsproj` / `已打开工程: xxx.dapsproj` | 信息 |
| 各类故障 | `未选择任何采样通道：请加载 ELF 并勾选变量...` 等 | 错误 |

所有插入点均复用现有 `log::info!` / `set_error` 的检测位置，不新增逻辑分支。

### 错误处理

- 错误消息统一走 `set_error`（红色），显示在底部状态栏
- 移除 `show_status_row` 末尾的 Error 渲染块及其 hover 提示
- 消息无持久化（不写文件、不跨会话保留）

## 非目标

- 不做多条消息历史/滚动显示
- 不做时间戳前缀
- 不持久化状态栏消息
- 不捕获 CLI 模式的日志输出

## 测试

- 无复杂逻辑，不新增单元测试；现有 180 个测试保持通过
- 编译通过、clippy 零警告、release 构建成功
