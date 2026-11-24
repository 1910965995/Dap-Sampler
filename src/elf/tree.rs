use std::collections::{HashMap, HashSet};
use super::dwarf::{DwarfResult, DwarfVarInfo, DwarfTypeInfo};
use super::types::map_dwarf_type;
use super::parser::{ElfSymbolRaw, ElfVariable};
use crate::pipeline::sample::ValueType;

/// 类型展开的最大递归深度（struct 嵌套 + 数组维数）
///
/// 损坏/构造的 DWARF 可能出现类型环（如 DW_TAG_array_type 的元素类型
/// 指回自身），没有深度上限会无限递归直至栈溢出（进程 abort）。
const MAX_EXPAND_DEPTH: usize = 32;
/// 单次 ELF 加载展开的叶子总数上限（防类型环/指数爆炸导致内存耗尽）
const MAX_EXPANDED_LEAVES: usize = 4096;
/// 单次 ELF 加载的展开节点总数上限
///
/// 深度上限不限制节点总数：类型环（如数组元素指回自身）是
/// 分支数^深度 量级的节点，且永远到不了叶子——必须有总工作量上限。
/// 正常工程远达不到（叶子 ≤ 4096，每个叶子约 2-3 个节点）。
const MAX_EXPAND_NODES: usize = 100_000;

/// 解析后的类型（沿 typedef/const/volatile 链找到最终类型）
#[derive(Debug)]
enum ResolvedType {
    Base(String, u32),
    Struct(u64),
    Array(u64),
    Pointer(u32),
    /// 枚举：底层整数类型（None = DWARF 未提供 DW_AT_type）
    Enum { byte_size: u32, base: Option<Box<ResolvedType>> },
    Unknown,
}

/// 结合 ELF 符号表和 DWARF 类型信息构建变量树
pub fn build_from_dwarf(
    elf_symbols: &[ElfSymbolRaw],
    dwarf: &DwarfResult,
) -> Result<Vec<ElfVariable>, String> {
    // 1. 交叉验证：只保留 DWARF 变量中名字也出现在 ELF 符号表的
    let elf_names: HashSet<&str> = elf_symbols.iter()
        .map(|s| s.name.as_str())
        .collect();

    let valid_dwarf_vars: Vec<&DwarfVarInfo> = dwarf.variables.iter()
        .filter(|v| elf_names.contains(v.name.as_str()) && v.address != 0)
        .collect();

    log::info!(
        "cross-validation: ELF symbols={}, DWARF variables={}, validated={} (filtered: name mismatch or address=0)",
        elf_symbols.len(),
        dwarf.variables.len(),
        valid_dwarf_vars.len()
    );
    if !dwarf.variables.is_empty() && valid_dwarf_vars.is_empty() {
        log::warn!("DWARF has {} variables but none match the ELF symbol table, check build options (-g and keep the symbol table)", dwarf.variables.len());
    }

    // 2. 对每个变量，解析类型链并展开
    let mut result = Vec::new();
    let mut skipped = 0usize;
    // 节点/叶子预算跨变量共享：一旦耗尽（必然是类型环或病态体积），
    // 整个展开中止
    let mut node_budget = MAX_EXPAND_NODES;
    for dv in &valid_dwarf_vars {
        let resolved = resolve_type(dv.type_offset, &dwarf.types);
        let before = result.len();
        // 返回 true = 预算耗尽，中止整个展开
        // （仅在入口检查 output.len() 无法阻止分支递归继续空转）
        if expand_variable(dv, &resolved, &dwarf.types, "", &mut result, 0, &mut node_budget) {
            log::warn!(
                "expansion budget exhausted (node limit {} / leaf limit {}), {} variables left unexpanded",
                MAX_EXPAND_NODES,
                MAX_EXPANDED_LEAVES,
                valid_dwarf_vars.len() - skipped - 1
            );
            break;
        }
        if result.len() == before {
            skipped += 1;
            log::warn!(
                "failed to expand variable '{}' (type_offset=0x{:X}), skipped",
                dv.name, dv.type_offset
            );
        }
    }
    log::info!(
        "type expansion: {} variables in, {} leaves out, {} skipped",
        valid_dwarf_vars.len(), result.len(), skipped
    );

    // 3. 先按 (path, address) 排序，使相同条目相邻
    //    （dedup_by 仅移除相邻重复，必须先 sort 再 dedup）
    result.sort_by(|a, b| {
        a.path.cmp(&b.path)
            .then_with(|| a.address.cmp(&b.address))
    });

    // 4. 去重（同名同地址的变量只保留一个）
    result.dedup_by(|a, b| a.path == b.path && a.address == b.address);
    disambiguate_duplicate_paths(&mut result);
    Ok(result)
}

/// 无 DWARF 时回退到纯符号表模式
pub fn build_from_symbols_only(
    elf_symbols: &[ElfSymbolRaw],
) -> Result<Vec<ElfVariable>, String> {
    let mut result = Vec::new();
    let mut skipped = 0usize;
    for sym in elf_symbols {
        if sym.address == 0 || sym.size == 0 {
            continue;
        }
        // 只支持 1/2/4/8 字节（AHB-AP 以 32 位字访问，8 字节占两个字）。
        // 其它宽度没有可靠的解释方式，跳过并汇总告警——旧实现会对任意
        // 宽度都给一个猜测类型，读出来是垃圾值。
        if !matches!(sym.size, 1 | 2 | 4 | 8) {
            skipped += 1;
            continue;
        }
        let value_type = match sym.size {
            // 无类型信息时只能按大小猜。4/8 字节最常见的是 float/double，
            // 但整型同样常见——这里沿用历史行为选浮点。关键是 8 字节
            // 必须映射成 Float64（占两个 32 位字），否则只读到低半字。
            8 => ValueType::Float64,
            4 => ValueType::Float,
            2 => ValueType::Uint16,
            1 => ValueType::Uint8,
            _ => unreachable!(),
        };
        result.push(ElfVariable {
            name: sym.name.clone(),
            path: sym.name.clone(),
            address: sym.address,
            byte_size: sym.size,
            value_type,
            parent_path: None,
            source_file: None,
        });
    }
    if skipped > 0 {
        log::warn!(
            "symbol-table mode: skipped {} symbols with unsupported size (only 1/2/4/8 bytes supported)",
            skipped
        );
    }
    // 先按 (path, address) 排序去重（符号表可能含同名同地址的重复条目），
    // 再对同 path 不同地址的残留冲突消歧
    result.sort_by(|a, b| {
        a.path.cmp(&b.path)
            .then_with(|| a.address.cmp(&b.address))
    });
    result.dedup_by(|a, b| a.path == b.path && a.address == b.address);
    disambiguate_duplicate_paths(&mut result);
    Ok(result)
}

/// 为「同 path 不同地址」的重复变量消歧（两个 .c 文件的同名 static）。
///
/// 浏览器树、通道名、工程文件、写入解析全部以 path 为唯一标识：
/// 重复 path 会让第二个变量永远无法被选中，且 UI 无从分辨两个条目。
/// 优先用源文件名消歧（文件名中的 '.' 替换为 '_'，避免被树路径分段
/// `split_var_path` 拆开）；文件名不可用或消歧后仍冲突时退回地址后缀
/// （地址在去重后必然互不相同，因此后缀一定唯一）。
fn disambiguate_duplicate_paths(vars: &mut [ElfVariable]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in vars.iter() {
        *counts.entry(v.path.clone()).or_insert(0) += 1;
    }
    // 已占用集合：包含全部现有 path（含未冲突变量），保证消歧结果
    // 不会与任何其它变量的 path 撞车
    let mut used: HashSet<String> = vars.iter().map(|v| v.path.clone()).collect();
    for v in vars.iter_mut() {
        if counts.get(v.path.as_str()).copied().unwrap_or(0) <= 1 {
            continue;
        }
        let base = v.path.clone();
        // 首选：源文件名后缀，如 "status (a_c)"
        let candidate = v.source_file.as_deref().map(|f| {
            let file = f
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(f)
                .replace('.', "_");
            format!("{} ({})", base, file)
        });
        match candidate {
            Some(c) if used.insert(c.clone()) => v.path = c,
            _ => {
                // 兜底：地址后缀，如 "status @0x20000010"
                // （地址在去重后必然互不相同，因此后缀一定唯一）
                let c = format!("{} @0x{:08X}", base, v.address);
                used.insert(c.clone());
                v.path = c;
            }
        }
    }
}

/// 沿 typedef/const/volatile 链找到最终类型
fn resolve_type(offset: u64, types: &HashMap<u64, DwarfTypeInfo>) -> ResolvedType {
    let mut current = offset;
    let mut depth = 0;
    loop {
        if depth > 32 {
            return ResolvedType::Unknown; // 防止无限循环
        }
        depth += 1;
        match types.get(&current) {
            Some(DwarfTypeInfo::Alias { target_offset, .. }) => {
                current = *target_offset;
            }
            Some(DwarfTypeInfo::Base { name, byte_size }) => {
                return ResolvedType::Base(name.clone(), *byte_size);
            }
            Some(DwarfTypeInfo::Struct { .. }) => {
                return ResolvedType::Struct(current);
            }
            Some(DwarfTypeInfo::Array { .. }) => {
                return ResolvedType::Array(current);
            }
            Some(DwarfTypeInfo::Pointer { byte_size }) => {
                return ResolvedType::Pointer(*byte_size);
            }
            Some(DwarfTypeInfo::Enum { byte_size, base_type_offset, .. }) => {
                // 枚举按底层整数类型解释（DW_AT_type）。不能把它当 Base：
                // 枚举名不在名称映射表里，4 字节会被猜成 float。
                let base = base_type_offset.map(|off| Box::new(resolve_type(off, types)));
                return ResolvedType::Enum { byte_size: *byte_size, base };
            }
            None => {
                return ResolvedType::Unknown;
            }
        }
    }
}

/// 将变量展开为叶子条目
///
/// `depth` 是递归深度：损坏的 DWARF 可能含类型环，必须设上限，
/// 否则无限递归导致栈溢出。
fn expand_variable(
    dv: &DwarfVarInfo,
    resolved: &ResolvedType,
    types: &HashMap<u64, DwarfTypeInfo>,
    parent_path: &str,
    output: &mut Vec<ElfVariable>,
    depth: usize,
    budget: &mut usize,
) -> bool {
    // 节点预算：无论是否产生叶子，每个展开节点都计入工作量
    if *budget == 0 {
        return true;
    }
    *budget -= 1;
    if depth > MAX_EXPAND_DEPTH {
        log::warn!("type expansion depth exceeds {MAX_EXPAND_DEPTH}, truncated: {}", dv.name);
        return false;
    }
    let path = if parent_path.is_empty() {
        dv.name.clone()
    } else {
        format!("{}.{}", parent_path, dv.name)
    };

    match resolved {
        ResolvedType::Base(name, byte_size) => {
            // 不支持的类型宽度：显式跳过并告警，而不是用错误宽度读出一串
            // 垃圾值。当前只支持 1/2/4/8 字节的基类型（AHB-AP 以 32 位字
            // 为单位访问，8 字节类型占两个字）。
            if !matches!(*byte_size, 1 | 2 | 4 | 8) {
                log::warn!(
                    "skipping unsupported variable '{}': base type '{}' is {} bytes (only 1/2/4/8 supported)",
                    dv.name, name, byte_size
                );
                return false;
            }
            // 全局叶子预算：防类型环/巨型嵌套产生的指数级展开
            if output.len() >= MAX_EXPANDED_LEAVES {
                return true;
            }
            let vt = map_dwarf_type(name, *byte_size);
            output.push(ElfVariable {
                name: dv.name.clone(),
                path,
                address: dv.address,
                byte_size: *byte_size,
                value_type: vt,
                parent_path: if parent_path.is_empty() { None } else { Some(parent_path.to_string()) },
                source_file: dv.source_file.clone(),
            });
            false
        }

        ResolvedType::Struct(type_offset) => {
            if let Some(DwarfTypeInfo::Struct { members, .. }) = types.get(type_offset) {
                // 限制展开深度，防止递归过深
                if parent_path.matches('.').count() < 4 {
                    for member in members {
                        let member_type = resolve_type(member.type_offset, types);
                        let member_dv = DwarfVarInfo {
                            name: member.name.clone(),
                            // 饱和运算：畸形 DWARF 的 member_offset 可能巨大
                            address: dv.address.saturating_add(member.member_offset),
                            type_offset: member.type_offset,
                            source_file: dv.source_file.clone(),
                            source_line: dv.source_line,
                        };
                        if expand_variable(&member_dv, &member_type, types, &path, output, depth + 1, budget) {
                            return true;
                        }
                    }
                }
            }
            false
        }

        ResolvedType::Array(type_offset) => {
            if let Some(DwarfTypeInfo::Array { element_type_offset, element_count, .. }) = types.get(type_offset) {
                let elem_type = resolve_type(*element_type_offset, types);
                let elem_size = get_type_byte_size(&elem_type, types);
                // 限制数组展开数量（避免超大数组）
                let count = (*element_count).min(256);
                for i in 0..count {
                    let elem_dv = DwarfVarInfo {
                        name: format!("[{}]", i),
                        // 饱和运算：i * elem_size 在畸形 DWARF 下可能溢出
                        address: dv.address.saturating_add(i.saturating_mul(elem_size)),
                        type_offset: *element_type_offset,
                        source_file: dv.source_file.clone(),
                        source_line: dv.source_line,
                    };
                    let elem_path = format!("{}[{}]", path, i);
                    if expand_variable(
                        &DwarfVarInfo { name: elem_path, ..elem_dv },
                        &elem_type, types, "",
                        output,
                        depth + 1,
                        budget,
                    ) {
                        return true;
                    }
                }
            }
            false
        }

        ResolvedType::Pointer(byte_size) => {
            // 指针类型：作为 uint32 显示
            if output.len() >= MAX_EXPANDED_LEAVES {
                return true;
            }
            output.push(ElfVariable {
                name: dv.name.clone(),
                path,
                address: dv.address,
                byte_size: *byte_size,
                value_type: ValueType::Uint32,
                parent_path: if parent_path.is_empty() { None } else { Some(parent_path.to_string()) },
                source_file: dv.source_file.clone(),
            });
            false
        }

        ResolvedType::Enum { byte_size, base } => {
            // 枚举按底层整数类型解释。底层类型可用时用它（通常是
            // unsigned int / int）；不可用时按宽度给有符号整数
            // （C 枚举默认底层是 int，不是 float）。
            if output.len() >= MAX_EXPANDED_LEAVES {
                return true;
            }
            let (value_type, size) = match base.as_deref() {
                Some(ResolvedType::Base(name, base_size)) => {
                    let vt = map_dwarf_type(name, *base_size);
                    // 只有当映射结果确实是整数类型时才采用（防止底层被
                    // 误判成 float）
                    if matches!(
                        vt,
                        ValueType::Int8
                            | ValueType::Uint8
                            | ValueType::Int16
                            | ValueType::Uint16
                            | ValueType::Int32
                            | ValueType::Uint32
                            | ValueType::Int64
                            | ValueType::Uint64
                    ) {
                        (vt, *base_size)
                    } else {
                        (enum_by_width(*byte_size), *byte_size)
                    }
                }
                _ => (enum_by_width(*byte_size), *byte_size),
            };
            // 不支持的宽度：显式跳过（与 Base 分支一致）
            if !matches!(size, 1 | 2 | 4 | 8) {
                log::warn!(
                    "skipping enum variable '{}': unsupported width {} bytes",
                    dv.name, size
                );
                return false;
            }
            output.push(ElfVariable {
                name: dv.name.clone(),
                path,
                address: dv.address,
                byte_size: size,
                value_type,
                parent_path: if parent_path.is_empty() { None } else { Some(parent_path.to_string()) },
                source_file: dv.source_file.clone(),
            });
            false
        }

        ResolvedType::Unknown => {
            // 无法解析类型，使用默认 uint32 保留变量（不丢弃）
            if output.len() >= MAX_EXPANDED_LEAVES {
                return true;
            }
            output.push(ElfVariable {
                name: dv.name.clone(),
                path,
                address: dv.address,
                byte_size: 4,
                value_type: ValueType::Uint32,
                parent_path: if parent_path.is_empty() { None } else { Some(parent_path.to_string()) },
                source_file: dv.source_file.clone(),
            });
            false
        }
    }
}

/// 枚举在缺少底层类型时的回退：按宽度给有符号整数
///
/// C 枚举默认底层类型是 int（有符号）。绝不能按宽度猜 float——那会让
/// 波形显示成无意义浮点数。
fn enum_by_width(byte_size: u32) -> ValueType {
    match byte_size {
        1 => ValueType::Int8,
        2 => ValueType::Int16,
        8 => ValueType::Int64,
        _ => ValueType::Int32,
    }
}

/// 获取类型的字节大小
fn get_type_byte_size(resolved: &ResolvedType, types: &HashMap<u64, DwarfTypeInfo>) -> u32 {
    match resolved {
        ResolvedType::Base(_, size) => *size,
        ResolvedType::Struct(offset) => {
            if let Some(DwarfTypeInfo::Struct { byte_size, .. }) = types.get(offset) {
                *byte_size
            } else {
                4
            }
        }
        ResolvedType::Array(offset) => {
            if let Some(DwarfTypeInfo::Array { byte_size, .. }) = types.get(offset) {
                *byte_size
            } else {
                4
            }
        }
        ResolvedType::Pointer(size) => *size,
        ResolvedType::Enum { byte_size, .. } => *byte_size,
        ResolvedType::Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::dwarf::StructMember;

    fn dv(name: &str, address: u32, type_offset: u64) -> DwarfVarInfo {
        DwarfVarInfo {
            name: name.to_string(),
            address,
            type_offset,
            source_file: None,
            source_line: None,
        }
    }

    #[test]
    fn cyclic_array_type_terminates() {
        // 损坏 DWARF：DW_TAG_array_type 的元素类型指回自身。
        // 旧实现无限递归 → 栈溢出（进程 abort）；现在必须有界终止。
        let mut types = HashMap::new();
        types.insert(
            10u64,
            DwarfTypeInfo::Array { element_type_offset: 10, element_count: 4, byte_size: 4 },
        );
        let resolved = resolve_type(10, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        let exhausted = expand_variable(&dv("a", 0x2000_0000, 10), &resolved, &types, "", &mut out, 0, &mut budget);
        assert!(exhausted || out.len() <= MAX_EXPANDED_LEAVES, "expansion must be bounded");
        // 纯类型环不产生叶子，靠节点预算终止
        assert_eq!(budget, 0, "cycle must consume node budget and stop");
    }

    #[test]
    fn struct_array_cycle_terminates() {
        // struct A { B[1] b; } 且 B 为 A 的数组 —— struct/数组混合类型环。
        // 数组分支重置路径深度，靠点号计数无法防护，递归深度上限兜底。
        let mut types = HashMap::new();
        types.insert(
            20u64,
            DwarfTypeInfo::Struct {
                name: "A".to_string(),
                byte_size: 4,
                members: vec![StructMember {
                    name: "b".to_string(),
                    type_offset: 10,
                    member_offset: 0,
                }],
            },
        );
        types.insert(
            10u64,
            DwarfTypeInfo::Array { element_type_offset: 20, element_count: 1, byte_size: 4 },
        );
        let resolved = resolve_type(20, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        let exhausted = expand_variable(&dv("a", 0x2000_0000, 20), &resolved, &types, "", &mut out, 0, &mut budget);
        assert!(exhausted || out.len() <= MAX_EXPANDED_LEAVES, "expansion must be bounded");
    }

    #[test]
    fn exponential_nesting_capped_by_leaf_budget() {
        // 合法但有病态体积的输入：8 成员 struct 的每个成员都是该 struct
        // 的数组。点号深度限制下约为 8^4 个叶子，全局叶子预算必须兜底。
        let mut types = HashMap::new();
        types.insert(
            20u64,
            DwarfTypeInfo::Struct {
                name: "A".to_string(),
                byte_size: 4,
                members: (0..8)
                    .map(|i| StructMember {
                        name: format!("m{i}"),
                        type_offset: 10,
                        member_offset: 0,
                    })
                    .collect(),
            },
        );
        types.insert(
            10u64,
            DwarfTypeInfo::Array { element_type_offset: 20, element_count: 1, byte_size: 4 },
        );
        let resolved = resolve_type(20, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        expand_variable(&dv("a", 0x2000_0000, 20), &resolved, &types, "", &mut out, 0, &mut budget);
        assert!(
            out.len() <= MAX_EXPANDED_LEAVES,
            "leaf budget must cap exponential expansion, got {}",
            out.len()
        );
        assert_eq!(budget, 0, "pathological growth must drain node budget");
    }

    #[test]
    fn normal_nested_struct_still_expands() {
        // 防护不能破坏正常展开：两层 struct + 数组应正常产出叶子
        let mut types = HashMap::new();
        types.insert(
            20u64,
            DwarfTypeInfo::Struct {
                name: "Outer".to_string(),
                byte_size: 8,
                members: vec![
                    StructMember { name: "x".to_string(), type_offset: 30, member_offset: 0 },
                    StructMember { name: "arr".to_string(), type_offset: 10, member_offset: 4 },
                ],
            },
        );
        types.insert(
            10u64,
            DwarfTypeInfo::Array { element_type_offset: 30, element_count: 2, byte_size: 4 },
        );
        types.insert(30u64, DwarfTypeInfo::Base { name: "float".to_string(), byte_size: 4 });
        let resolved = resolve_type(20, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        expand_variable(&dv("v", 0x2000_0000, 20), &resolved, &types, "", &mut out, 0, &mut budget);
        // v.x + v.arr[0] + v.arr[1]
        assert_eq!(out.len(), 3, "normal expansion unaffected: {:?}", out);
        assert_eq!(out[0].path, "v.x");
        assert_eq!(out[1].path, "v.arr[0]");
        assert_eq!(out[2].path, "v.arr[1]");
    }

    /// 构造测试用 ElfVariable（path 与 name 初始相同）
    fn ev(path: &str, address: u32, source_file: Option<&str>) -> ElfVariable {
        ElfVariable {
            name: path.to_string(),
            path: path.to_string(),
            address,
            byte_size: 4,
            value_type: ValueType::Float,
            parent_path: None,
            source_file: source_file.map(|s| s.to_string()),
        }
    }

    #[test]
    fn duplicate_paths_disambiguated_by_source_file() {
        // 两个 .c 文件的同名 static：path 相同、地址不同（原缺陷 ⑦，
        // 第二个变量此前永远不可达且 UI 无从分辨）
        let mut vars = vec![
            ev("status", 0x2000_0010, Some("./src/motor_a.c")),
            ev("status", 0x2000_0020, Some("./src/motor_b.c")),
        ];
        disambiguate_duplicate_paths(&mut vars);
        assert_eq!(vars[0].path, "status (motor_a_c)");
        assert_eq!(vars[1].path, "status (motor_b_c)");
        // 裸名保留不变（供 elf_var_matches 顶层兜底）
        assert_eq!(vars[0].name, "status");
        assert_eq!(vars[1].name, "status");
        // 文件名中的 '.' 替换为 '_'，不能被树路径分段拆开
        let segs = crate::ui::variable_browser::split_var_path(&vars[0].path);
        assert_eq!(segs.len(), 1, "消歧后缀不得产生子节点: {:?}", segs);
    }

    #[test]
    fn duplicate_paths_without_source_file_use_address_suffix() {
        let mut vars = vec![
            ev("status", 0x2000_0010, None),
            ev("status", 0x2000_0020, None),
        ];
        disambiguate_duplicate_paths(&mut vars);
        assert_eq!(vars[0].path, "status @0x20000010");
        assert_eq!(vars[1].path, "status @0x20000020");
    }

    #[test]
    fn duplicate_paths_same_basename_fall_back_to_address() {
        // 两个目录下的同名文件：文件名后缀撞车——先占位者拿到文件名
        // 后缀，后者退回地址后缀，保证最终 path 一定互不相同
        let mut vars = vec![
            ev("status", 0x10, Some("d1/a.c")),
            ev("status", 0x20, Some("d2/a.c")),
        ];
        disambiguate_duplicate_paths(&mut vars);
        assert_eq!(vars[0].path, "status (a_c)");
        assert_eq!(vars[1].path, "status @0x00000020");
        assert_ne!(vars[0].path, vars[1].path);
    }

    #[test]
    fn unique_paths_are_left_untouched() {
        let mut vars = vec![
            ev("status", 0x10, Some("a.c")),
            ev("speed", 0x14, None),
        ];
        disambiguate_duplicate_paths(&mut vars);
        assert_eq!(vars[0].path, "status");
        assert_eq!(vars[1].path, "speed");
    }

    #[test]
    fn build_from_symbols_only_dedups_and_disambiguates() {
        use crate::elf::parser::ElfSymbolRaw;
        let symbols = vec![
            ElfSymbolRaw { name: "status".to_string(), address: 0x10, size: 4 },
            // 符号表常见的同名同地址重复条目：应去重而不是消歧
            ElfSymbolRaw { name: "status".to_string(), address: 0x10, size: 4 },
            ElfSymbolRaw { name: "status".to_string(), address: 0x20, size: 4 },
            ElfSymbolRaw { name: "speed".to_string(), address: 0x30, size: 4 },
        ];
        let vars = build_from_symbols_only(&symbols).unwrap();
        let status: Vec<&ElfVariable> = vars.iter().filter(|v| v.name == "status").collect();
        assert_eq!(status.len(), 2, "同名同地址去重后剩 2 个不同地址实例");
        let paths: Vec<&str> = status.iter().map(|v| v.path.as_str()).collect();
        assert!(paths[0] != paths[1], "两个实例的 path 必须可区分: {:?}", paths);
    }

    #[test]
    fn symbols_only_maps_eight_byte_to_float64() {
        // 旧实现把 size==8 映射成 Float（4 字节），只读一个 32 位字：
        // double 只拿到低半部分，显示为无意义浮点
        use crate::elf::parser::ElfSymbolRaw;
        let symbols = vec![
            ElfSymbolRaw { name: "d".to_string(), address: 0x2000_0000, size: 8 },
            ElfSymbolRaw { name: "i".to_string(), address: 0x2000_0008, size: 4 },
        ];
        let vars = build_from_symbols_only(&symbols).unwrap();
        let d = vars.iter().find(|v| v.name == "d").unwrap();
        assert_eq!(d.value_type, ValueType::Float64);
        assert_eq!(d.value_type.byte_size(), 8, "must read two 32-bit words");
        assert_eq!(d.byte_size, 8);
        let i = vars.iter().find(|v| v.name == "i").unwrap();
        assert_eq!(i.value_type.byte_size(), 4);
    }

    #[test]
    fn symbols_only_skips_unsupported_widths() {
        // 3/16/24 字节符号没有可靠解释方式：跳过而不是猜一个类型读出垃圾
        use crate::elf::parser::ElfSymbolRaw;
        let symbols = vec![
            ElfSymbolRaw { name: "arr3".to_string(), address: 0x10, size: 3 },
            ElfSymbolRaw { name: "big".to_string(), address: 0x20, size: 16 },
            ElfSymbolRaw { name: "ok".to_string(), address: 0x30, size: 4 },
        ];
        let vars = build_from_symbols_only(&symbols).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "ok");
    }

    // ---- 枚举类型 ----

    /// 构造 enum 类型并展开一个枚举变量
    fn expand_enum(byte_size: u32, base: Option<(u64, DwarfTypeInfo)>) -> Vec<ElfVariable> {
        let mut types = HashMap::new();
        let mut base_offset = None;
        if let Some((off, info)) = base {
            types.insert(off, info);
            base_offset = Some(off);
        }
        types.insert(
            100u64,
            DwarfTypeInfo::Enum {
                name: "MotorState".to_string(),
                byte_size,
                base_type_offset: base_offset,
            },
        );
        let resolved = resolve_type(100, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        expand_variable(&dv("state", 0x2000_0000, 100), &resolved, &types, "", &mut out, 0, &mut budget);
        out
    }

    #[test]
    fn enum_uses_underlying_integer_type() {
        // DWARF: enum MotorState : unsigned int
        let out = expand_enum(
            4,
            Some((200, DwarfTypeInfo::Base { name: "unsigned int".to_string(), byte_size: 4 })),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value_type, ValueType::Uint32, "enum must follow base type, not float");
        assert_eq!(out[0].byte_size, 4);
    }

    #[test]
    fn enum_without_base_type_falls_back_to_signed_int() {
        // 无 DW_AT_type：C 枚举默认底层是 int（有符号）——绝不能猜 float
        let out = expand_enum(4, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value_type, ValueType::Int32);
        // 1/2 字节枚举同理
        assert_eq!(expand_enum(1, None)[0].value_type, ValueType::Int8);
        assert_eq!(expand_enum(2, None)[0].value_type, ValueType::Int16);
    }

    #[test]
    fn enum_with_non_integer_base_falls_back_by_width() {
        // 畸形 DWARF：枚举底层类型指向 float。不能用 float 解释枚举值
        // （会显示成极小的反规范化数），退回按宽度给有符号整数
        let out = expand_enum(
            4,
            Some((200, DwarfTypeInfo::Base { name: "float".to_string(), byte_size: 4 })),
        );
        assert_eq!(out[0].value_type, ValueType::Int32);
    }

    #[test]
    fn enum_through_typedef_resolves() {
        // typedef enum MotorState State_t; —— resolve_type 必须穿过 Alias
        let mut types = HashMap::new();
        types.insert(
            200u64,
            DwarfTypeInfo::Base { name: "unsigned char".to_string(), byte_size: 1 },
        );
        types.insert(
            100u64,
            DwarfTypeInfo::Enum {
                name: "MotorState".to_string(),
                byte_size: 1,
                base_type_offset: Some(200),
            },
        );
        types.insert(300u64, DwarfTypeInfo::Alias { name: "State_t".to_string(), target_offset: 100 });
        let resolved = resolve_type(300, &types);
        let mut out = Vec::new();
        let mut budget = MAX_EXPAND_NODES;
        expand_variable(&dv("st", 0x2000_0000, 300), &resolved, &types, "", &mut out, 0, &mut budget);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value_type, ValueType::Uint8);
        assert_eq!(out[0].byte_size, 1);
    }
}
