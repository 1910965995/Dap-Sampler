// 在 release 模式下使用 GUI 子系统，双击运行不弹控制台窗口。
// CLI 命令通过 attach_parent_console() 重新挂回父进程控制台以恢复输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{Parser, Subcommand};

use dap_sampler::dap::swd::SwdLink;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "dap-sampler", about = "CMSIS-DAP v2 High-Speed Variable Sampler")]
struct Cli {
    /// Also write logs to a file (logs are invisible in the GUI; use this to troubleshoot connection issues, e.g. --log-file dap-sampler.log)
    #[arg(long, global = true)]
    log_file: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List all connected CMSIS-DAP devices
    List,
    /// Parse an ELF file and print the variable tree (debug)
    Dump {
        /// ELF file path
        elf: String,
    },
    /// Connect to the device and show debug info
    Info {
        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
    /// Diagnose CMSIS-DAP USB connection, interfaces, endpoints and optional claim
    Diag {
        /// Claim the CMSIS-DAP interface and release it immediately (checks driver/permissions/occupancy)
        #[arg(long)]
        claim: bool,
    },
    /// Read the value at a memory address
    Read {
        /// Memory address (hex, e.g. 0x20000100)
        address: String,
        /// Parse the value as float
        #[arg(long)]
        float: bool,
        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
    /// Write a value to a memory address
    Write {
        /// Memory address (hex, e.g. 0x20000100)
        address: String,
        /// Value to write (u32 hex/decimal, or float when --float is set)
        #[arg(long)]
        value: String,
        /// Parse the value as float
        #[arg(long)]
        float: bool,
        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
    /// Watch a memory address continuously
    Monitor {
        /// Memory address (hex)
        address: String,
        /// Sample rate (Hz)
        #[arg(long, default_value = "5000")]
        rate: u32,
        /// Number of samples (default: unlimited)
        #[arg(long)]
        count: Option<u32>,
        /// Parse the value as float
        #[arg(long)]
        float: bool,
        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
    /// High-speed pipeline sampling (P2) with multiple variables
    Sample {
        /// Memory address list (hex, comma-separated, e.g. 0x20000100,0x20000104)
        #[arg(long, value_delimiter = ',')]
        addresses: Vec<String>,

        /// Sample rate (Hz)
        #[arg(long, default_value = "5000")]
        rate: u32,

        /// Number of samples (default: unlimited)
        #[arg(long)]
        count: Option<u64>,

        /// Parse all values as float
        #[arg(long)]
        float: bool,

        /// Output CSV file path (default: stdout)
        #[arg(long)]
        output: Option<String>,

        /// Sample timestamp mode: "grid" (ideal equal spacing, default) or
        /// "real" (actual submit time; skipped cycles show up as larger gaps)
        #[arg(long, default_value = "grid")]
        timestamp: String,

        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
    /// Launch the GUI waveform display (P3/P4)
    Gui {
        /// Memory address list (hex, comma-separated)
        #[arg(long, value_delimiter = ',')]
        addresses: Vec<String>,

        /// Sample rate (Hz)
        #[arg(long, default_value = "5000")]
        rate: u32,

        /// Number of samples (default: unlimited)
        #[arg(long)]
        count: Option<u64>,

        /// Variable type list (comma-separated, e.g. float,int32,uint32)
        ///
        /// One type per address, all default to float.
        /// Supported: float, int32, uint32, int16, uint16, int8, uint8
        #[arg(long, value_delimiter = ',')]
        r#type: Option<Vec<String>>,

        /// ELF firmware file path (P4: enables the variable browser)
        #[arg(long)]
        elf: Option<String>,

        /// Sample timestamp mode: "grid" (default) or "real".
        /// Can also be changed later in Settings → Advanced Settings…
        #[arg(long, default_value = "grid")]
        timestamp: String,

        /// SWD clock frequency (MHz, 1-50)
        #[arg(long, default_value = "1")]
        swd_clock: u32,
    },
}

fn main() -> anyhow::Result<()> {
    // GUI 子系统下，尝试挂回父进程控制台（从 cmd/PowerShell 运行 CLI 命令时恢复输出）。
    // 双击运行时无父控制台，AttachConsole 失败，静默跳过——GUI 不受影响。
    #[cfg(windows)]
    attach_parent_console();

    let cli = Cli::parse();

    // 初始化日志：默认输出到 stderr；指定 --log-file 时扇出到文件
    // （GUI 子系统下日志在控制台不可见，写文件后才能排查连接/采样问题）
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    logger.format_timestamp_millis();
    let mut log_path: Option<String> = None;
    if let Some(path) = cli.log_file.as_deref() {
        match std::fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                logger.target(env_logger::Target::Pipe(Box::new(TeeWriter {
                    file: std::sync::Mutex::new(file),
                })));
                log_path = Some(path.to_string());
            }
            Err(e) => eprintln!("warning: failed to open log file '{}': {}", path, e),
        }
    }
    logger.init();
    if let Some(path) = log_path {
        log::info!("logs will also be written to: {}", path);
    }

    match cli.command.unwrap_or(Command::Gui {
        addresses: vec![],
        rate: 5000,
        count: None,
        r#type: None,
        elf: None,
        timestamp: "grid".to_string(),
        swd_clock: 1,
    }) {
        Command::List => cmd_list(),
        Command::Dump { elf } => cmd_dump(&elf),
        Command::Info { swd_clock } => cmd_info(swd_clock),
        Command::Diag { claim } => cmd_diag(claim),
        Command::Read { address, float, swd_clock } => cmd_read(&address, float, swd_clock),
        Command::Write { address, value, float, swd_clock } => cmd_write(&address, &value, float, swd_clock),
        Command::Monitor { address, rate, count, float, swd_clock } => cmd_monitor(&address, rate, count, float, swd_clock),
        Command::Sample { addresses, rate, count, float, output, timestamp, swd_clock } => {
            cmd_sample(&addresses, rate, count, float, output.as_deref(), &timestamp, swd_clock)
        }
        Command::Gui { addresses, rate, count, r#type, elf, timestamp, swd_clock } => {
            cmd_gui(&addresses, rate, count, r#type.as_deref(), elf.as_deref(), &timestamp, swd_clock)
        }
    }
}

/// 在 GUI 子系统下尝试挂回父进程的控制台。
///
/// - 从 cmd/PowerShell 运行 CLI 命令时，`AttachConsole` 成功，
///   随后重定向 stdin/stdout/stderr 到该控制台，CLI 输出恢复正常。
/// - 双击运行时无父控制台，`AttachConsole` 失败，直接返回——GUI 不受影响。
#[cfg(windows)]
fn attach_parent_console() {
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFFFFFF;
    const STD_INPUT_HANDLE: u32 = 0xFFFFFFF6;  // (DWORD)-10
    const STD_OUTPUT_HANDLE: u32 = 0xFFFFFFF5; // (DWORD)-11
    const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4;  // (DWORD)-12

    unsafe extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
    }
    unsafe extern "C" {
        fn _open_osfhandle(osfhandle: isize, flags: i32) -> i32;
        fn _dup2(fd1: i32, fd2: i32) -> i32;
        fn _close(fd: i32) -> i32;
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // 无父控制台（双击运行），静默跳过
        }

        // 将控制台句柄重定向到 C 运行时的 fd 0/1/2
        for (std_handle, target_fd) in [
            (STD_OUTPUT_HANDLE, 1),
            (STD_ERROR_HANDLE, 2),
            (STD_INPUT_HANDLE, 0),
        ] {
            let h = GetStdHandle(std_handle);
            if h != -1 && h != 0 {
                let fd = _open_osfhandle(h, 0);
                if fd != -1 {
                    _dup2(fd, target_fd);
                    _close(fd);
                }
            }
        }
    }
}

/// 扇出写入器：env_logger 的输出同时写到 stderr 与日志文件。
///
/// GUI 子系统（windows_subsystem = "windows"）下 stderr 无控制台句柄，
/// 写入失败被忽略——文件仍能拿到全部日志；CLI 下则两者都有。
struct TeeWriter {
    file: std::sync::Mutex<std::fs::File>,
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(buf);
        }
        let _ = std::io::stderr().write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
        let _ = std::io::stderr().flush();
        Ok(())
    }
}

fn cmd_dump(elf_path: &str) -> anyhow::Result<()> {
    use dap_sampler::elf::ElfParser;

    println!("📦 Parsing ELF: {}", elf_path);
    let ctx = ElfParser::load(elf_path)
        .map_err(|e| anyhow::anyhow!("ELF load failed: {}", e))?;

    println!("total variables: {}", ctx.variables.len());
    for var in &ctx.variables {
        println!(
            "  {:<40} addr=0x{:08X} size={:<4} type={:?}  src={:?}",
            var.path, var.address, var.byte_size, var.value_type, var.source_file
        );
    }
    Ok(())
}

fn cmd_list() -> anyhow::Result<()> {
    let devices = dap_sampler::usb::device::list_devices()?;
    if devices.is_empty() {
        println!("⚠️  No CMSIS-DAP v2 device found");
        println!("  Make sure the DAP-Link is connected via USB");
        println!();
        println!("  Note: the device must support CMSIS-DAP v2 (bulk transfers)");
        println!("  Check Device Manager to see whether it is recognized as a WinUSB device");
    } else {
        println!("🔍 Found {} CMSIS-DAP v2 device(s):\n", devices.len());
        for (i, dev) in devices.iter().enumerate() {
            println!("  [{}] {:04X}:{:04X}  bus={} addr={}", i, dev.vid, dev.pid, dev.bus_number, dev.address);
            println!("       Manufacturer: {}", if dev.manufacturer.is_empty() { "(none)" } else { &dev.manufacturer });
            println!("       Product:      {}", if dev.product.is_empty() { "(none)" } else { &dev.product });
            println!("       Serial:       {}", if dev.serial.is_empty() { "(none)" } else { &dev.serial });
            println!();
        }
    }
    Ok(())
}

fn cmd_diag(claim: bool) -> anyhow::Result<()> {
    let diagnostics = dap_sampler::usb::device::diagnose_devices(claim)?;

    println!("🔎 CMSIS-DAP USB diagnostics");
    println!("   claim test: {}", if claim { "enabled" } else { "disabled (use --claim to check driver/permissions/occupancy)" });
    println!();

    if diagnostics.is_empty() {
        println!("⚠️  No CMSIS-DAP candidate device found");
        println!("   Check the USB connection and whether the DAP-Link shows up in Device Manager;");
        println!("   if the device is CMSIS-DAP v1 HID, this tool only supports CMSIS-DAP v2 bulk.\n");
        return Ok(());
    }

    for (i, dev) in diagnostics.iter().enumerate() {
        println!("[{}] {:04X}:{:04X}  bus={} addr={}", i, dev.vid, dev.pid, dev.bus_number, dev.address);
        println!("    known VID/PID: {}", yes_no(dev.known_vid_pid));
        println!("    open:          {}", yes_no(dev.open_ok));
        if let Some(e) = &dev.open_error {
            println!("      error: {}", e);
            println!("      hint:  {}", usb_error_hint(e));
        }
        println!("    Manufacturer:  {}", empty_or(&dev.manufacturer));
        println!("    Product:       {}", empty_or(&dev.product));
        println!("    Serial:        {}", empty_or(&dev.serial));
        println!("    product CMSIS: {}", yes_no(dev.product_has_cmsis_dap));
        println!("    Bulk interface:{}", yes_no(dev.has_bulk_candidate));
        if let Some(e) = &dev.config_error {
            println!("      config error: {}", e);
            println!("      hint:         {}", usb_error_hint(e));
        }
        match (dev.interface_number, dev.ep_out, dev.ep_in) {
            (Some(iface), Some(ep_out), Some(ep_in)) => {
                println!("      interface: {}", iface);
                println!("      endpoints: OUT=0x{:02X}, IN=0x{:02X}", ep_out, ep_in);
            }
            _ => {
                println!("      interface: (no CMSIS-DAP v2 bulk interface found)");
            }
        }

        if let Some(ok) = dev.claim_ok {
            println!("    claim:         {}", yes_no(ok));
            if let Some(e) = &dev.claim_error {
                println!("      error: {}", e);
                println!("      hint:  {}", usb_error_hint(e));
            }
        }
        println!();
    }

    println!("Diagnostics summary:");
    println!("  - open=false: usually the WinUSB driver is missing, permissions are lacking, or the device is occupied/disconnected.");
    println!("  - bulk interface=false: the device may be CMSIS-DAP v1 HID, or the firmware/interface is not CMSIS-DAP v2 bulk.");
    println!("  - claim=false: usually occupied by Keil/OpenOCD/pyOCD or another dap-sampler instance, or a driver mismatch.");
    println!("  - USB/claim OK but GUI still fails to init: check target power, SWDIO/SWCLK/GND/RESET wiring, chip read protection, and the SWD clock.\n");

    Ok(())
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

fn empty_or(s: &str) -> &str {
    if s.is_empty() { "(none/unreadable)" } else { s }
}

fn usb_error_hint(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("access") || lower.contains("denied") {
        "Check that the Windows driver is WinUSB and that you have permission to access the USB interface."
    } else if lower.contains("busy") {
        "The device interface may be occupied by Keil/OpenOCD/pyOCD or another program; close it and retry."
    } else if lower.contains("no device") || lower.contains("not found") {
        "The device may be disconnected or re-enumerated, or the interface/endpoint may not match."
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "Device not responding; check the USB cable, DAP-Link firmware and target power, or replug the device."
    } else if lower.contains("pipe") {
        "USB pipe error; possibly an endpoint mismatch, device firmware fault, or an occupied interface."
    } else {
        "Compare diag --claim output between a good and a bad DAP-Link; focus on open, bulk interface and claim."
    }
}

fn cmd_info(swd_clock_mhz: u32) -> anyhow::Result<()> {
    println!("🔌 Connecting to DAP-Link...");
    let mut swd = SwdLink::new()?;
    println!("⚡ Initializing SWD @ {} MHz ...", swd_clock_mhz);
    let clock_hz = swd_clock_mhz.saturating_mul(1_000_000);
    let device_info = swd.init_with_clock(clock_hz)?;

    println!();
    println!("✅ Connected!");
    println!("  Target: {}", device_info.target_info);
    println!("  DPIDR = 0x{:08X}", device_info.dpidr);
    println!("  AP0 IDR = 0x{:08X}", device_info.ap_idr);
    println!();
    println!("  Use the read command to read memory:");
    println!("  dap-sampler read 0x20000000");

    Ok(())
}

fn cmd_read(address: &str, as_float: bool, swd_clock_mhz: u32) -> anyhow::Result<()> {
    let addr = parse_address(address)?;

    println!("🔌 Connecting to DAP-Link...");
    let mut swd = SwdLink::new()?;
    let clock_hz = swd_clock_mhz.saturating_mul(1_000_000);
    swd.init_with_clock(clock_hz)?;

    if as_float {
        let value = swd.read_float(addr)?;
        println!("📊 Address 0x{:08X} = {} (float)", addr, value);
    } else {
        let value = swd.read_memory(addr)?;
        println!("📊 Address 0x{:08X} = 0x{:08X} (u32)", addr, value);
        let as_f32 = f32::from_bits(value);
        println!("                   ≈ {} (float)", as_f32);
    }

    Ok(())
}

fn cmd_write(address: &str, value: &str, as_float: bool, swd_clock_mhz: u32) -> anyhow::Result<()> {
    let addr = parse_address(address)?;

    let (raw_value, display) = if as_float {
        let v: f32 = value
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid float value '{}': {}", value, e))?;
        (v.to_bits(), format!("{} (float)", v))
    } else {
        let v = parse_address(value)?;
        (v, format!("0x{:08X} ({})", v, v))
    };

    println!("🔌 Connecting to DAP-Link...");
    let mut swd = SwdLink::new()?;
    let clock_hz = swd_clock_mhz.saturating_mul(1_000_000);
    swd.init_with_clock(clock_hz)?;

    swd.write_memory(addr, raw_value)?;
    println!("✅ Written: 0x{:08X} = {}", addr, display);

    Ok(())
}

fn cmd_monitor(address: &str, rate: u32, count: Option<u32>, as_float: bool, swd_clock_mhz: u32) -> anyhow::Result<()> {
    let addr = parse_address(address)?;
    let interval_us = (1_000_000.0 / rate as f64) as u64;

    println!("🔌 Connecting to DAP-Link...");
    let mut swd = SwdLink::new()?;
    let clock_hz = swd_clock_mhz.saturating_mul(1_000_000);
    swd.init_with_clock(clock_hz)?;

    println!("📈 Watching 0x{:08X} @ {} Hz (interval {} us)", addr, rate, interval_us);
    if as_float {
        println!("   format: float");
    } else {
        println!("   format: u32");
    }
    println!("   seq | time (ms) | value");
    println!("   {}", "-".repeat(45));

    let max_samples = count.unwrap_or(u32::MAX);
    let start = std::time::Instant::now();
    // 高采样率下 thread::sleep 精度不足（Windows 下约 15.6ms 时钟周期），
    // 改用 spin_sleep 自旋等待，5kHz（200µs 间隔）以上也能精确命中目标时刻
    let sleeper = spin_sleep::SpinSleeper::default();

    for seq in 0..max_samples {
        let value_raw = swd.read_memory(addr)?;

        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let display_value = if as_float {
            format!("{}", f32::from_bits(value_raw))
        } else {
            format!("0x{:08X} ({})", value_raw, value_raw)
        };

        println!("   {:>6} | {:>8.2} | {}", seq, elapsed_ms, display_value);

        // 控制采样间隔
        let elapsed = start.elapsed();
        let target = std::time::Duration::from_micros(interval_us * (seq + 1) as u64);
        if elapsed < target {
            sleeper.sleep(target - elapsed);
        }
    }

    Ok(())
}

fn parse_address(s: &str) -> anyhow::Result<u32> {
    let s = s.trim();
    // 如果以 0x/0X 开头，按十六进制解析
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16)
            .map_err(|e| anyhow::anyhow!("invalid address format '{}': {}", s, e));
    }
    // 否则先尝试十进制（PowerShell 会把 0x0000000c 转成十进制 "12"），
    // 再回退到十六进制
    if let Ok(v) = s.parse::<u32>() {
        return Ok(v);
    }
    u32::from_str_radix(s, 16)
        .map_err(|e| anyhow::anyhow!("invalid address format '{}': {}", s, e))
}

fn cmd_sample(
    address_strs: &[String],
    rate: u32,
    count: Option<u64>,
    as_float: bool,
    output_path: Option<&str>,
    timestamp: &str,
    swd_clock_mhz: u32,
) -> anyhow::Result<()> {
    use std::io::Write;
    use dap_sampler::pipeline::engine::PipelineEngine;

    // 解析地址列表
    let addresses: Vec<u32> = address_strs
        .iter()
        .map(|s| parse_address(s))
        .collect::<anyhow::Result<Vec<_>>>()?;

    if addresses.is_empty() {
        anyhow::bail!("at least one memory address is required");
    }
    if addresses.len() > dap_sampler::pipeline::sample::MAX_VALUES {
        anyhow::bail!(
            "at most {} variables supported, got: {}",
            dap_sampler::pipeline::sample::MAX_VALUES,
            addresses.len()
        );
    }

    println!("\u{1f50c} Connecting to DAP-Link...");
    let mut swd = SwdLink::new()?;
    let clock_hz = swd_clock_mhz.saturating_mul(1_000_000);
    swd.init_with_clock(clock_hz)?;

    let (usb, dap) = swd.into_parts();

    let interval_us = 1_000_000.0 / rate as f64;
    println!("\u{1f680} Starting pipeline capture:");
    println!("   variables: {}", addresses.len());
    for (i, &addr) in addresses.iter().enumerate() {
        println!("   [{}] 0x{:08X}", i, addr);
    }
    println!("   rate: {} Hz (interval {:.0} us)", rate, interval_us);
    println!("   mode:   {}", if as_float { "float" } else { "u32 (hex)" });
    if let Some(path) = output_path {
        println!("   output: {}", path);
    }

    // 创建引擎并启动（CLI 模式无类型概念，全部按 u32 整字访问）
    let usb = Arc::new(usb);
    let types = vec![dap_sampler::pipeline::sample::ValueType::Uint32; addresses.len()];
    let ts_mode = dap_sampler::pipeline::engine::TimestampMode::from_label(timestamp);
    println!("   timestamps: {}", ts_mode.label());
    let engine = PipelineEngine::new(usb, dap, addresses.clone(), &types, rate, ts_mode);
    let handle = engine.start(dap_sampler::ui::controls::DEFAULT_BUFFER_CAPACITY)?;

    // 准备输出
    let mut writer: Box<dyn Write> = match output_path {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout()),
    };

    // CSV 表头
    write!(writer, "seq,time_ms")?;
    for i in 0..addresses.len() {
        write!(writer, ",var{}", i)?;
    }
    writeln!(writer)?;

    let max_samples = count.unwrap_or(u64::MAX);
    let mut total_collected: u64 = 0;
    let mut sample_buf: Vec<dap_sampler::pipeline::sample::Sample> =
        (0..1024).map(|_| dap_sampler::pipeline::sample::Sample::default()).collect();

    let mut last_progress = 0u64;
    // 最后一个已写出样本的真实时间戳（用于结束时报告实际时长）
    let mut last_sample_ts: Option<f64> = None;

    while total_collected < max_samples {
        let n = handle.drain_samples(&mut sample_buf);
        if n == 0 {
            // 暂无数据，短暂等待
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }

        for sample in &sample_buf[..n] {
            let time_ms = sample.timestamp_sec * 1000.0;
            write!(writer, "{},{}", sample.seq, time_ms)?;
            // 直接遍历定长切片，避免 as_floats 的每样本分配
            if as_float {
                for &v in sample.values() {
                    write!(writer, ",{}", f32::from_bits(v as u32))?;
                }
            } else {
                for &v in sample.values() {
                    write!(writer, ",0x{:08X}", v as u32)?;
                }
            }
            writeln!(writer)?;

            last_sample_ts = Some(sample.timestamp_sec);
            total_collected += 1;
            if total_collected >= max_samples {
                break;
            }
        }

        // 进度提示（每秒打印一次）
        if total_collected / (rate as u64) > last_progress {
            last_progress = total_collected / (rate as u64);
            log::info!(
                "captured {} samples ({:.1}s), {} available in buffer",
                total_collected,
                last_sample_ts.unwrap_or(0.0),
                handle.available_samples()
            );
        }
    }

    // 停止流水线
    handle.stop();

    let actual_count = total_collected;
    println!("\n\u{2705} capture finished: {} samples", actual_count);
    if actual_count > 0 {
        // 用真实时间戳报告时长：时间戳来自实际提交时刻，按目标采样率
        // 换算会在吞吐不足时低估实际经过的时间
        let actual_duration = last_sample_ts.unwrap_or_else(|| {
            actual_count as f64 / rate as f64
        });
        println!("   actual duration: {:.3}s", actual_duration);
    }

    Ok(())
}

fn cmd_gui(
    address_strs: &[String],
    rate: u32,
    count: Option<u64>,
    type_strs: Option<&[String]>,
    elf_path: Option<&str>,
    timestamp: &str,
    swd_clock_mhz: u32,
) -> anyhow::Result<()> {
    use dap_sampler::ui::app::DapSamplerApp;
    use dap_sampler::elf::ElfParser;

    // 加载 ELF（如果提供）
    let elf_ctx = if let Some(path) = elf_path {
        println!("📦 Loading ELF: {}", path);
        Some(ElfParser::load(path).map_err(|e| anyhow::anyhow!("ELF load failed: {}", e))?)
    } else {
        None
    };

    // 手工地址模式（--addresses）：仅解析地址，不在此处连接 USB
    // USB 连接推迟到用户在 GUI 中点击 Start 时再进行
    let manual_addresses: Vec<u32> = if !address_strs.is_empty() {
        let addresses: Vec<u32> = address_strs
            .iter()
            .map(|s| parse_address(s))
            .collect::<anyhow::Result<Vec<_>>>()?;

        if addresses.len() > dap_sampler::pipeline::sample::MAX_VALUES {
            anyhow::bail!(
                "at most {} variables supported, got: {}",
                dap_sampler::pipeline::sample::MAX_VALUES,
                addresses.len()
            );
        }
        addresses
    } else {
        vec![]
    };

    // 解析变量类型列表
    // 未指定 --type 时，手工模式默认 Uint32（原始内存值）
    let manual_types: Vec<dap_sampler::pipeline::sample::ValueType> = if !manual_addresses.is_empty() {
        if let Some(types) = type_strs {
            types
                .iter()
                .map(|s| dap_sampler::pipeline::sample::ValueType::parse(s).unwrap_or(dap_sampler::pipeline::sample::ValueType::Uint32))
                .collect()
        } else {
            vec![dap_sampler::pipeline::sample::ValueType::Uint32; manual_addresses.len()]
        }
    } else {
        vec![]
    };

    // 启动 egui 窗口（此时不连接 USB 设备）
    let mut app = DapSamplerApp::new(
        manual_addresses,
        address_strs.to_vec(),
        rate,
        count,
        elf_ctx,
        manual_types,
        swd_clock_mhz,
    );
    // CLI 指定的时间戳模式（默认 grid；窗口中可在 Settings → Advanced Settings… 再改）
    app.set_timestamp_mode(dap_sampler::pipeline::engine::TimestampMode::from_label(timestamp));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_icon(std::sync::Arc::new(load_window_icon())),
        ..Default::default()
    };

    eframe::run_native(
        "DAP Sampler",
        options,
        Box::new(move |cc| {
            setup_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ).map_err(|e| anyhow::anyhow!("GUI failed: {}", e))?;

    Ok(())
}

/// 加载窗口图标（运行时显示在标题栏/任务栏）。
///
/// 图标以 PNG 形式编译期嵌入（include_bytes!），运行时用 image crate 解码为 RGBA。
/// exe 文件本身的图标由 build.rs + winres 单独嵌入，两者使用同一份源图。
fn load_window_icon() -> egui::IconData {
    const ICON_PNG: &[u8] = include_bytes!("../icon/icon_256.png");
    match image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            egui::IconData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
            }
        }
        Err(e) => {
            log::error!("failed to load window icon: {}", e);
            egui::IconData::default()
        }
    }
}

/// Load a CJK-compatible font from the Windows system so Chinese text renders correctly.
fn setup_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Try common Windows CJK fonts in order of preference.
    let candidates: &[(&str, u32)] = &[
        ("C:\\Windows\\Fonts\\msyh.ttc", 0),   // Microsoft YaHei (collection)
        ("C:\\Windows\\Fonts\\simhei.ttf", 0), // SimHei
        ("C:\\Windows\\Fonts\\simsun.ttc", 0),  // SimSun (collection)
        ("C:\\Windows\\Fonts\\Deng.ttf", 0),    // DengXian
    ];

    for (path, index) in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData {
                    font: std::borrow::Cow::Owned(data),
                    index: *index,
                    tweak: Default::default(),
                }),
            );
            // Prepend CJK font so it takes priority for CJK glyphs,
            // while the default Latin font still handles ASCII.
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                if let Some(font_list) = fonts.families.get_mut(&family) {
                    font_list.push("cjk".to_owned());
                }
            }
            log::info!("Loaded CJK font from {}", path);
            break;
        }
    }

    ctx.set_fonts(fonts);
}
