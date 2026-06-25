use std::collections::HashMap;
use dap_sampler::elf::tree::{build_from_dwarf, build_from_symbols_only};
use dap_sampler::elf::dwarf::{DwarfResult, DwarfVarInfo, DwarfTypeInfo, StructMember};
use dap_sampler::elf::parser::ElfSymbolRaw;
use dap_sampler::pipeline::sample::ValueType;

fn make_elf_symbol(name: &str, address: u32, size: u32) -> ElfSymbolRaw {
    ElfSymbolRaw {
        name: name.to_string(),
        address,
        size,
    }
}

fn make_dwarf_var(name: &str, address: u32, type_offset: u64) -> DwarfVarInfo {
    DwarfVarInfo {
        name: name.to_string(),
        address,
        type_offset,
        source_file: None,
        source_line: None,
    }
}

#[test]
fn test_build_from_symbols_only_basic() {
    let symbols = vec![
        make_elf_symbol("speed", 0x20000100, 4),
        make_elf_symbol("counter", 0x20000104, 2),
        make_elf_symbol("flag", 0x20000106, 1),
    ];

    let result = build_from_symbols_only(&symbols).unwrap();
    assert_eq!(result.len(), 3);

    // 按 path 排序后检查
    let by_name: HashMap<&str, _> = result.iter().map(|v| (v.name.as_str(), v)).collect();
    assert_eq!(by_name["speed"].value_type, ValueType::Float);
    assert_eq!(by_name["speed"].address, 0x20000100);
    assert_eq!(by_name["counter"].value_type, ValueType::Uint16);
    assert_eq!(by_name["flag"].value_type, ValueType::Uint8);
}

#[test]
fn test_build_from_symbols_only_skip_zero() {
    let symbols = vec![
        make_elf_symbol("valid", 0x20000100, 4),
        make_elf_symbol("zero_addr", 0x0, 4),
        make_elf_symbol("zero_size", 0x20000104, 0),
    ];

    let result = build_from_symbols_only(&symbols).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].name, "valid");
}

#[test]
fn test_build_from_dwarf_basic_types() {
    let elf_symbols = vec![
        make_elf_symbol("temperature", 0x20000200, 4),
        make_elf_symbol("count", 0x20000204, 4),
    ];

    let mut types = HashMap::new();
    types.insert(1, DwarfTypeInfo::Base {
        name: "float".to_string(),
        byte_size: 4,
    });
    types.insert(2, DwarfTypeInfo::Base {
        name: "int32_t".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("temperature", 0x20000200, 1),
            make_dwarf_var("count", 0x20000204, 2),
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    assert_eq!(result.len(), 2);

    let by_name: HashMap<&str, _> = result.iter().map(|v| (v.name.as_str(), v)).collect();
    assert_eq!(by_name["temperature"].value_type, ValueType::Float);
    assert_eq!(by_name["count"].value_type, ValueType::Int32);
}

#[test]
fn test_build_from_dwarf_struct_expansion() {
    let elf_symbols = vec![
        make_elf_symbol("pid", 0x20000300, 12),
    ];

    let mut types = HashMap::new();
    types.insert(1, DwarfTypeInfo::Struct {
        name: "PID".to_string(),
        byte_size: 12,
        members: vec![
            StructMember { name: "kp".to_string(), type_offset: 2, member_offset: 0 },
            StructMember { name: "ki".to_string(), type_offset: 2, member_offset: 4 },
            StructMember { name: "kd".to_string(), type_offset: 2, member_offset: 8 },
        ],
    });
    types.insert(2, DwarfTypeInfo::Base {
        name: "float".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("pid", 0x20000300, 1),
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    // struct 展开为 3 个叶子
    assert_eq!(result.len(), 3);

    let by_path: HashMap<&str, _> = result.iter().map(|v| (v.path.as_str(), v)).collect();
    assert!(by_path.contains_key("pid.kp"));
    assert!(by_path.contains_key("pid.ki"));
    assert!(by_path.contains_key("pid.kd"));

    assert_eq!(by_path["pid.kp"].address, 0x20000300);
    assert_eq!(by_path["pid.ki"].address, 0x20000304);
    assert_eq!(by_path["pid.kd"].address, 0x20000308);
    assert_eq!(by_path["pid.kp"].value_type, ValueType::Float);
}

#[test]
fn test_build_from_dwarf_array_expansion() {
    let elf_symbols = vec![
        make_elf_symbol("buffer", 0x20000400, 16),
    ];

    let mut types = HashMap::new();
    types.insert(1, DwarfTypeInfo::Array {
        element_type_offset: 2,
        element_count: 4,
        byte_size: 16,
    });
    types.insert(2, DwarfTypeInfo::Base {
        name: "float".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("buffer", 0x20000400, 1),
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    assert_eq!(result.len(), 4);

    let by_path: HashMap<&str, _> = result.iter().map(|v| (v.path.as_str(), v)).collect();
    assert!(by_path.contains_key("buffer[0]"));
    assert!(by_path.contains_key("buffer[1]"));
    assert!(by_path.contains_key("buffer[2]"));
    assert!(by_path.contains_key("buffer[3]"));

    assert_eq!(by_path["buffer[0]"].address, 0x20000400);
    assert_eq!(by_path["buffer[1]"].address, 0x20000404);
}

#[test]
fn test_build_from_dwarf_typedef_chain() {
    let elf_symbols = vec![
        make_elf_symbol("my_var", 0x20000500, 4),
    ];

    let mut types = HashMap::new();
    // typedef chain: my_type → const_type → uint32_t
    types.insert(10, DwarfTypeInfo::Alias {
        name: "my_type".to_string(),
        target_offset: 20,
    });
    types.insert(20, DwarfTypeInfo::Alias {
        name: "".to_string(), // const wrapper
        target_offset: 30,
    });
    types.insert(30, DwarfTypeInfo::Base {
        name: "uint32_t".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("my_var", 0x20000500, 10),
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].value_type, ValueType::Uint32);
}

#[test]
fn test_build_from_dwarf_filter_by_elf_symbols() {
    // DWARF 变量名不在 ELF 符号表 → 被过滤
    let elf_symbols = vec![
        make_elf_symbol("visible_var", 0x20000600, 4),
    ];

    let mut types = HashMap::new();
    types.insert(1, DwarfTypeInfo::Base {
        name: "float".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("visible_var", 0x20000600, 1),
            make_dwarf_var("hidden_var", 0x20000604, 1), // 不在 ELF 符号表
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].name, "visible_var");
}

#[test]
fn test_build_from_dwarf_skip_zero_address() {
    let elf_symbols = vec![
        make_elf_symbol("good", 0x20000700, 4),
        make_elf_symbol("bad", 0x0, 4),
    ];

    let mut types = HashMap::new();
    types.insert(1, DwarfTypeInfo::Base {
        name: "float".to_string(),
        byte_size: 4,
    });

    let dwarf = DwarfResult {
        variables: vec![
            make_dwarf_var("good", 0x20000700, 1),
            make_dwarf_var("bad", 0x0, 1), // 地址为 0 → 跳过
        ],
        types,
        source_files: vec![],
    };

    let result = build_from_dwarf(&elf_symbols, &dwarf).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].name, "good");
}
