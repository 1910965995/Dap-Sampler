use dap_sampler::elf::ElfParser;

fn fixture_path() -> std::path::PathBuf {
    // Prefer checked-in fixture; fall back to the desktop repro file.
    let local = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("STM32H743.axf");
    if local.exists() {
        return local;
    }
    std::path::PathBuf::from(r"C:\Users\Lucas\Desktop\Temp\STM32H743.axf")
}

#[test]
fn test_load_stm32h743_axf_parses_named_variables() {
    let path = fixture_path();
    if !path.exists() {
        // Optional local repro fixture (Keil .axf). Skip when absent so CI stays green.
        eprintln!(
            "skip: AXF fixture not found at {} (place STM32H743.axf under tests/fixtures/ to enable)",
            path.display()
        );
        return;
    }

    let ctx = ElfParser::load(&path).expect("AXF should load");
    assert!(
        !ctx.variables.is_empty(),
        "expected variables from Keil AXF DWARF (DebugStrRef names), got 0"
    );

    let paths: Vec<&str> = ctx.variables.iter().map(|v| v.path.as_str()).collect();
    assert!(
        paths.iter().any(|p| *p == "SystemCoreClock" || p.starts_with("SystemCoreClock.")),
        "missing SystemCoreClock in {:?}",
        paths.iter().take(20).collect::<Vec<_>>()
    );
    assert!(
        paths.iter().any(|p| p.starts_with("params") || *p == "params"),
        "missing params in {:?}",
        paths.iter().take(20).collect::<Vec<_>>()
    );

    // Addresses should be real RAM/flash, not zero.
    let sys = ctx
        .variables
        .iter()
        .find(|v| v.path == "SystemCoreClock" || v.name == "SystemCoreClock")
        .expect("SystemCoreClock present");
    assert_eq!(sys.address, 0x2400_0008);
}
