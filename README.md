# DAP Sampler

> 基于 CMSIS-DAP v2 协议的高速变量采样与可视化工具，专为 ARM Cortex-M 调试场景打造。

通过 DAP-Link 仿真器直接读取 MCU 内存变量，无需修改目标固件、无需串口日志、无需额外 I/O 引脚。支持 CLI 命令行和 GUI 可视化两种使用方式，从快速内存 peek/poke 到多变量高速波形采集一应俱全。

## 特性

### CLI 命令行

| 子命令 | 功能 |
|--------|------|
| `list` | 列出所有已连接的 CMSIS-DAP v2 设备（VID/PID/序列号/厂商信息） |
| `info` | 连接设备并显示调试信息（DPIDR、AP IDR） |
| `read <addr>` | 读取 32 位内存值，支持 `--float` 解析为浮点 |
| `write <addr> --value <v>` | 写入内存值，支持 u32（hex/dec）和 float 类型 |
| `monitor <addr>` | 连续监视单个变量，可指定采样率和次数 |
| `sample --addresses ...` | 高速流水线采样，支持最多 8 个变量，输出 CSV |
| `gui` | 启动 GUI 可视化界面（默认行为，双击 exe 即启动） |

### GUI 可视化

- **实时波形显示**：多通道同时显示，支持 float / int32 / uint32 / int16 / uint16 / int8 / uint8 七种数据类型
- **采样率可调**：1–100 kHz，最高 100000 个采样点
- **ELF 变量浏览器**：加载固件 ELF 文件后自动解析 DWARF 调试信息，按名称勾选变量即可采样，无需手动查地址
- **Watch 监视面板**：6 列表格（Name / Address / Type / Value / Refresh(s) / Remark），支持在线修改变量值、自定义备注、可调列宽和面板高度
- **通道面板**：每通道独立颜色、显示开关、Y 轴偏移/缩放
- **游标测量**：左右两个游标，自动显示两点间时间差和值差
- **Follow 模式**：跟随最新数据滚动，自动 Y 轴自适应
- **SWD 时钟可调**：1/2/5/10/20/30/50 MHz 六档可选（仅 Idle 状态可改）

### 工程化细节

- **双线程流水线引擎**：USB 采集线程 + 渲染线程，无锁环形缓冲区，避免数据竞争
- **精确时间戳**：基于 `seq × period_ns` 计算，确保波形等间隔
- **DAP-Link 安全释放**：`BulkTransfer` 实现 `Drop` trait，停止采集或关闭窗口时自动释放 USB 设备
- **响应解析鲁棒性**：携带缓冲区处理 USB 不完整响应，多响应合并解析
- **单文件分发**：依赖静态链接，整个程序只有一个 `dap-sampler.exe`（约 7.6 MB），无需安装运行时
- **跨版本测试覆盖**：8 个测试文件覆盖协议、环形缓冲、采样、UI、变量树、Watch 转换等模块

## 硬件要求

- **仿真器**：支持 CMSIS-DAP v2（Bulk 传输）的 DAP-Link
- **目标 MCU**：ARM Cortex-M 系列（STM32F103C8T6 / AC7840X 均验证通过）
- **USB 驱动**：Windows 下需 WinUSB 驱动（可用 [Zadig](https://zadig.akeo.ie/) 安装）

## 快速开始

### 1. CLI 命令行

```powershell
# 列出设备
dap-sampler.exe list

# 连接并查看信息
dap-sampler.exe info

# 读取内存
dap-sampler.exe read 0x20000000
dap-sampler.exe read 0x20000000 --float

# 写入内存（修改变量）
dap-sampler.exe write 0x20000000 --value 0x0000000C
dap-sampler.exe write 0x20000000 --value 3.14 --float

# 连续监视（10 kHz, 100 个点）
dap-sampler.exe monitor 0x20000000 --rate 10000 --count 100

# 多变量流水线采样，输出到 CSV
dap-sampler.exe sample --addresses 0x20000100,0x20000104 --rate 5000 --output data.csv
```

### 2. GUI 可视化

```powershell
# 双击 dap-sampler.exe，或命令行启动
dap-sampler.exe gui

# 启动时加载 ELF 并预设变量
dap-sampler.exe gui --elf firmware.elf --addresses 0x20000100,0x20000104 --rate 5000
```

GUI 启动后：
1. 在左侧 **变量浏览器** 加载 ELF，勾选要采样的变量；或在 **通道面板** 直接输入地址
2. 选择 SWD 时钟频率（默认 10 MHz，仅 Idle 状态可改）
3. 设置采样率和采样点数
4. 点击 **Start** 开始采集，**Stop** 停止
5. Watch 面板双击 Value 列可在线修改变量值

### 3. 从源码构建

```bash
# 克隆仓库
git clone <repo-url> && cd Dap-Sampler

# Debug 构建
cargo build

# Release 构建（单文件 exe，含图标）
cargo build --release

# 运行测试
cargo test
```

## 使用建议

- **SWD 时钟**：10 MHz 的情况下能做到15khz单个4bytes变量采集，如果需要更高的采集速率需要提高SWD时钟频率。
- **类型选择**：浮点变量用 `--float` 或 `float` 类型；整型变量按实际宽度选择 int32/uint16 等
- **PowerShell 兼容**：地址可用 `0x20000000` 或十进制形式，PowerShell 自动转换也能识别

## 已知限制

- 仅支持 CMSIS-DAP v2（Bulk 传输），不支持 v1（HID）
- 单次最多 8 个变量
- 采样率上限 100 kHz
- 目标 MCU 开启读保护（RDP）时无法访问

## 项目结构

```
Dap-Sampler/
├── build.rs                  构建脚本：嵌入 exe 图标（仅 Windows）
├── Cargo.toml                依赖与元数据
├── icon/
│   ├── icon.jpg              原始图标（1024×1024）
│   ├── icon2.png             替代图标源
│   ├── icon.ico              多尺寸 ICO（嵌入 exe）
│   └── icon_256.png          256×256 PNG（运行时窗口图标）
├── src/
│   ├── main.rs               CLI 入口（list/info/read/write/monitor/sample/gui）
│   ├── lib.rs                库入口 + 模块导出
│   ├── error.rs              统一错误类型
│   ├── usb/                  USB Bulk 传输封装
│   │   ├── device.rs         设备发现（VID/PID + 字符串匹配）
│   │   └── transfer.rs       BulkTransfer（DROP 兜底释放）
│   ├── dap/                  CMSIS-DAP v2 协议层
│   │   ├── commands.rs       DAP 命令常量、寄存器定义、请求编码
│   │   ├── protocol.rs       协议打包/解析（含 carry-over 缓冲）
│   │   └── swd.rs            SWD 操作（初始化/内存读写/时钟可调）
│   ├── pipeline/             P2 流水线采集引擎
│   │   ├── engine.rs         双线程引擎（采集 + 渲染）
│   │   ├── ring_buffer.rs    无锁 SPSC 环形缓冲
│   │   └── sample.rs         Sample 数据结构与类型转换
│   ├── ui/                   P3/P4 egui GUI
│   │   ├── app.rs            主应用与状态管理
│   │   ├── controls.rs       工具栏（采样率/窗口/SWD 时钟）
│   │   ├── channel_panel.rs  通道面板
│   │   ├── watch_panel.rs    Watch 6 列表格
│   │   ├── variable_browser.rs  ELF 变量树
│   │   ├── waveform.rs       波形渲染
│   │   ├── cursor.rs         游标测量
│   │   └── display_buffer.rs 脏通道标记 + 降采样缓冲
│   └── elf/                  P4 ELF/DWARF 解析
│       ├── parser.rs         ELF 文件读取
│       ├── dwarf.rs          DWARF 调试信息解析
│       ├── tree.rs           变量树构建（去重/排序）
│       └── types.rs          类型映射到 DAP 值类型
└── tests/                    8 个测试文件
    ├── test_commands.rs
    ├── test_protocol.rs
    ├── test_ring_buffer.rs
    ├── test_sample.rs
    ├── test_variable_tree.rs
    ├── test_watch_transform.rs
    ├── test_ui.rs
    └── test_integration.rs
```

## 协议栈

```
┌──────────────────────┐
│   CLI / GUI           │
├──────────────────────┤
│   SwdLink            │  SWD 高级操作
├──────────────────────┤
│   DapProtocol        │  CMSIS-DAP 协议
├──────────────────────┤
│   BulkTransfer       │  USB Bulk 传输
├──────────────────────┤
│   rusb (libusb)      │  USB 底层
├──────────────────────┤
│   DAP-Link 仿真器     │
├──────────────────────┤
│   ARM Cortex-M 目标   │
└──────────────────────┘
```

## 技术栈

- **语言**：Rust 2021
- **USB**：rusb 0.9（libusb 1.0 后端）
- **GUI**：egui 0.31 + eframe + egui_plot
- **ELF/DWARF**：object 0.36 + gimli 0.31
- **CLI**：clap 4
- **资源嵌入**：winres（Windows exe 图标）

## 版本演进

- **P1**：基础通信（list / info / read / monitor），SWD 初始化与内存读写
- **P2**：双线程流水线采集引擎，无锁环形缓冲区，多变量 CSV 输出
- **P3**：egui GUI 实时波形显示，通道/游标/Follow 模式
- **P4**：ELF/DWARF 变量浏览器、Watch 监视面板、UI 重构与性能优化
- **v1.0.0**：增加 write 子命令、SWD 时钟可调、采样率上限扩至 100kHz、Windows 优化（无控制台弹出 + 圆角图标嵌入）

## 许可

本项目为内部工具，遵循项目所有者制定的许可条款。
