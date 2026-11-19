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
        "交叉验证: ELF符号={}, DWARF变量={}, 通过验证={} (被过滤: 名称不匹配或地址为0)",
        elf_symbols.len(),
        dwarf.variables.len(),
        valid_dwarf_vars.len()
    );
    if !dwarf.variables.is_empty() && valid_dwarf_vars.is_empty() {
        log::warn!("DWARF 有 {} 个变量但无一个匹配 ELF 符号表, 请检查编译选项 (可能需要 -g 并保留符号表)", dwarf.variables.len());
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
                "展开预算耗尽（节点上限 {} / 叶子上限 {}），剩余 {} 个变量未展开",
                MAX_EXPAND_NODES,
                MAX_EXPANDED_LEAVES,
                valid_dwarf_vars.len() - skipped - 1
            );
            break;
        }
        if result.len() == before {
            skipped += 1;
            log::warn!(
                "变量 '{}' 展开失败 (type_offset=0x{:X}), 跳过",
                dv.name, dv.type_offset
            );
        }
    }
    log::info!(
        "类型展开: 输入 {} 个变量, 输出 {} 个叶子, 跳过 {} 个",
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
    Ok(result)
}

/// 无 DWARF 时回退到纯符号表模式
pub fn build_from_symbols_only(
    elf_symbols: &[ElfSymbolRaw],
) -> Result<Vec<ElfVariable>, String> {
    let mut result = Vec::new();
    for sym in elf_symbols {
        if sym.address == 0 || sym.size == 0 {
            continue;
        }
        let value_type = match sym.size {
            8 => ValueType::Float,
            4 => ValueType::Float,   // 嵌入式常见 float
            2 => ValueType::Uint16,
            1 => ValueType::Uint8,
            _ => ValueType::Uint32,
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
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
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
        log::warn!("类型展开深度超过 {MAX_EXPAND_DEPTH}，截断: {}", dv.name);
        return false;
    }
    let path = if parent_path.is_empty() {
        dv.name.clone()
    } else {
        format!("{}.{}", parent_path, dv.name)
    };

    match resolved {
        ResolvedType::Base(name, byte_size) => {
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
}
