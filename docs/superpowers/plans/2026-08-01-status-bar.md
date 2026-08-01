# 底部状态栏 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 DAP Sampler 界面最底部增加一行横跨窗口的状态栏，集中显示用户关键操作（ELF 导入、参数设置、工程操作、采集启停）与故障提示。

**Architecture:** 在 `DapSamplerApp` 中用 `status_text: Option<String>` + `status_is_error: bool` 替代现有 `last_error` 字段；`set_status()`（信息，默认色）与 `set_error()`（错误，红色）两个方法统一记录消息；底部新增 `TopBottomPanel::bottom("status_bar")` 渲染最新一条消息；移除工具栏下方状态行中的旧 Error 显示。

**Tech Stack:** Rust 2021、egui 0.31（TopBottomPanel）

**Spec:** `docs/superpowers/specs/2026-08-01-status-bar-design.md`

---

### Task 1: 状态消息字段与方法（TDD）

**Files:**
- Modify: `src/ui/app.rs:73`（字段定义）、`:212`（new 初始化）、`:220-224`（set_error）、`:228`、`:316`、`:473`（清空点）、`:944-955`（旧 Error 渲染块，删除）
- Test: `src/ui/app.rs` 末尾新增 `#[cfg(test)] mod status_tests`

- [ ] **Step 1: 写失败测试**（app.rs 末尾追加）

```rust
#[cfg(test)]
mod status_tests {
    use super::*;

    fn make_app() -> DapSamplerApp {
        DapSamplerApp::new(vec![], vec![], 5000, None, None, vec![], 1)
    }

    #[test]
    fn set_status_records_info_message() {
        let mut app = make_app();
        app.set_status("采样率已变更为 5000 Hz");
        assert_eq!(app.status_text.as_deref(), Some("采样率已变更为 5000 Hz"));
        assert!(!app.status_is_error);
    }

    #[test]
    fn set_error_records_error_message() {
        let mut app = make_app();
        app.set_error("未选择任何采样通道");
        assert_eq!(app.status_text.as_deref(), Some("未选择任何采样通道"));
        assert!(app.status_is_error);
    }

    #[test]
    fn set_status_overwrites_previous_message() {
        let mut app = make_app();
        app.set_error("旧错误");
        app.set_status("新信息");
        assert_eq!(app.status_text.as_deref(), Some("新信息"));
        assert!(!app.status_is_error);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test status_tests 2>&1 | tail -5`
Expected: 编译失败（`status_text`/`set_status` 不存在）

- [ ] **Step 3: 实现字段与方法**

替换 `src/ui/app.rs:73` 的字段：

```rust
    /// 底部状态栏消息（None = 空）
    status_text: Option<String>,
    /// 状态消息是否为错误（错误显示红色）
    status_is_error: bool,
```

替换 `set_error`（原 220-224 行）并新增 `set_status`：

```rust
    fn set_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::error!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = true;
    }

    /// 记录一条普通状态消息（显示在底部状态栏，默认文字色）
    fn set_status(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{}", message);
        self.status_text = Some(message);
        self.status_is_error = false;
    }
```

- [ ] **Step 4: 更新所有旧字段引用**

a) `new()` 初始化（原 212 行）：`last_error: None,` → `status_text: None, status_is_error: false,`

b) 三处清空点 `self.last_error = None;`（new_project 228 行、open_project 316 行、start_acquisition 473 行）→ `self.status_text = None;`

c) 删除 show_status_row 末尾的旧 Error 渲染块（944-955 行）：

```rust
            // 错误提示
            if let Some(err) = &self.last_error {
                ui.colored_label(
                    egui::Color32::from_rgb(0xff, 0x55, 0x55),
                    format!("Error: {}", err),
                ).on_hover_text(
                    "请检查：DAP-Link 是否连接、是否安装 WinUSB 驱动、是否被 Keil/OpenOCD/pyOCD/其他程序占用、目标板是否供电、SWD 接线是否正确，或尝试降低 SWD Clock。"
                );
            }
```

（整块删除，含前一行 `// 错误提示` 注释）

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test status_tests 2>&1 | tail -5`
Expected: 3 个测试全部 PASS

- [ ] **Step 6: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(ui): 状态消息字段 status_text/status_is_error 与 set_status/set_error 方法"
```

---

### Task 2: 底部状态栏 UI

**Files:**
- Modify: `src/ui/app.rs`（watch_panel 之后、CentralPanel 之前插入）

- [ ] **Step 1: 添加状态栏 panel**

在 `egui::TopBottomPanel::bottom("watch_panel")` 的 `.show(ctx, ...)` 结束之后（`drain_changed_transforms` 之前）插入：

```rust
        // ---- 底部状态栏（Watch 面板之下、界面最底部）----
        egui::TopBottomPanel::bottom("status_bar")
            .resizable(false)
            .exact_height(24.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(msg) = &self.status_text {
                        let color = if self.status_is_error {
                            egui::Color32::from_rgb(0xff, 0x55, 0x55)
                        } else {
                            ui.visuals().text_color()
                        };
                        ui.label(egui::RichText::new(msg).color(color));
                    }
                });
            });
```

- [ ] **Step 2: 编译验证**

Run: `cargo check 2>&1 | tail -3`
Expected: `Finished`，无错误

- [ ] **Step 3: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(ui): 界面最底部新增状态栏，错误红色/信息默认色"
```

---

### Task 3: 消息来源接线

**Files:**
- Modify: `src/ui/app.rs`（约 8 处插入点）

- [ ] **Step 1: ELF 导入（Import ELF File… 菜单，约 1012-1016 行）**

```rust
                            match ElfParser::load(&path) {
                                Ok(ctx) => {
                                    log::info!("ELF loaded: {} variables", ctx.variables.len());
                                    app.set_status(format!("ELF loaded: {} variables", ctx.variables.len()));
                                    app.elf_ctx = Some(ctx);
                                }
                                Err(e) => {
                                    log::error!("ELF load failed: {}", e);
                                    app.set_error(format!("ELF load failed: {}", e));
                                }
                            }
```

- [ ] **Step 2: 参数变更（CentralPanel 检测处，约 1147/1153/1160 行）**

采样率（原 `log::info!("采样率已变更为 {} Hz", self.rate_hz);` 后追加）：

```rust
                    self.set_status(format!("采样率已变更为 {} Hz", self.rate_hz));
```

SWD 时钟（原 log 后追加）：

```rust
                    self.set_status(format!("SWD 时钟已变更为 {} MHz（下次连接 DAP-Link 时生效）", self.swd_clock_mhz));
```

窗口大小（原 log 后追加）：

```rust
                    self.set_status(format!("显示窗口大小已变更为 {} 个点", self.window_size));
```

Display 模式（`if mode != self.display_mode` 块内 `self.waveform.set_display_mode(mode);` 后追加）：

```rust
                    let mode_label = match mode {
                        WaveformDisplayMode::Line => "Line",
                        WaveformDisplayMode::Point => "Point",
                    };
                    self.set_status(format!("显示模式已切换为 {}", mode_label));
```

- [ ] **Step 3: 采集启停**

a) `start_acquisition` 成功处（`self.controls.set_running();` 附近，约 585 行）追加：

```rust
                self.set_status("开始采集");
```

b) `pause_acquisition`（约 618 行）方法体末尾追加：

```rust
        self.set_status("采集已暂停");
```

c) `stop_acquisition`（约 598 行）方法体末尾追加：

```rust
        self.set_status("采集已停止");
```

- [ ] **Step 4: 工程操作**

a) `new_project` 中 `log::info!("已新建工程（清空配置）");`（258 行）后追加：

```rust
        self.set_status("已新建工程（清空配置）");
```

b) `save_project` 中 `log::info!("工程已保存: {}", path.display());`（308 行）后追加：

```rust
                self.set_status(format!("工程已保存: {}", path.display()));
```

c) `open_project` 中 `log::info!("已打开工程: {}", path.display());`（469 行）后追加：

```rust
        self.set_status(format!("已打开工程: {}", path.display()));
```

- [ ] **Step 5: 编译 + 测试验证**

Run: `cargo check 2>&1 | tail -3 && cargo test 2>&1 | awk '/test result: ok/{n+=$4} /test result: FAILED/{f=1} END{if(f) print "FAILED"; else print "total passed:", n}'`
Expected: 编译通过，183 个测试全部通过（180 既有 + 3 新增）

- [ ] **Step 6: Commit**

```bash
git add src/ui/app.rs
git commit -m "feat(ui): 状态栏接入操作与故障消息（ELF/参数/采集/工程）"
```

---

### Task 4: 全量验证

**Files:** 无

- [ ] **Step 1: clippy**

Run: `cargo clippy --all-targets 2>&1 | awk '/^warning/{w++} /^error/{e++} END{print "warnings:", w+0, "errors:", e+0}'`
Expected: `warnings: 0 errors: 0`

- [ ] **Step 2: release 构建**

Run: `cargo build --release 2>&1 | tail -2`
Expected: `Finished`，生成 `target/release/dap-sampler.exe`

- [ ] **Step 3: 人工验证清单**（用户操作，非自动化）

- 点 Start（无通道）→ 底部状态栏红色显示"未选择任何采样通道…"
- 导入 ELF → 状态栏显示 `ELF loaded: N variables`
- 修改 Rate/Window/SWD Clock/Display → 状态栏逐条更新
- Start/Pause/Stop → 状态栏显示对应消息
- 新建/保存/打开工程 → 状态栏显示对应消息
- 状态行（工具栏下方）不再出现红色 Error
