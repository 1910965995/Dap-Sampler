use std::collections::HashMap;
use gimli::{
    self, Dwarf, EndianSlice, LittleEndian,
    DW_AT_name, DW_AT_type, DW_AT_location, DW_AT_decl_file,
    DW_AT_decl_line, DW_AT_byte_size, DW_AT_data_member_location,
    DW_AT_upper_bound, DW_AT_count,
    DW_TAG_variable, DW_TAG_base_type, DW_TAG_typedef,
    DW_TAG_const_type, DW_TAG_volatile_type, DW_TAG_restrict_type,
    DW_TAG_structure_type, DW_TAG_union_type, DW_TAG_array_type,
    DW_TAG_subrange_type, DW_TAG_pointer_type,
    DW_TAG_enumeration_type,
    DwTag,
};

/// DWARF 解析结果
pub struct DwarfResult {
    pub variables: Vec<DwarfVarInfo>,
    pub types: HashMap<u64, DwarfTypeInfo>,
    pub source_files: Vec<String>,
}

pub struct DwarfVarInfo {
    pub name: String,
    pub address: u32,
    pub type_offset: u64,
    pub source_file: Option<String>,
    pub source_line: Option<u32>,
}

pub enum DwarfTypeInfo {
    Base { name: String, byte_size: u32 },
    Alias { name: String, target_offset: u64 },
    Struct { name: String, byte_size: u32, members: Vec<StructMember> },
    Array { element_type_offset: u64, element_count: u32, byte_size: u32 },
    Pointer { byte_size: u32 },
}

pub struct StructMember {
    pub name: String,
    pub type_offset: u64,
    pub member_offset: u32,
}

type R<'a> = EndianSlice<'a, LittleEndian>;

/// Raw DIE info collected from flat iteration
struct RawDie {
    offset: u64,
    tag: DwTag,
    depth: usize,
    name: Option<String>,
    byte_size: Option<u32>,
    type_ref: Option<u64>,
    address: Option<u32>,
    member_offset: Option<u32>,
    upper_bound: Option<u32>,
    count: Option<u32>,
    source_file: Option<String>,
    source_line: Option<u32>,
}

/// 从 ELF section 字节解析 DWARF
#[allow(clippy::too_many_arguments)]
pub fn parse_dwarf_sections<'a>(
    debug_info: &'a [u8],
    debug_abbrev: &'a [u8],
    debug_str: &'a [u8],
    debug_str_offsets: &'a [u8],
    debug_line: &'a [u8],
    debug_ranges: &'a [u8],
    debug_rnglists: &'a [u8],
    debug_addr: &'a [u8],
) -> Result<DwarfResult, String> {
    let le = LittleEndian;
    let dwarf = Dwarf::<R>::load(|id| -> Result<R<'a>, gimli::Error> {
        match id {
            gimli::SectionId::DebugInfo => Ok(EndianSlice::new(debug_info, le)),
            gimli::SectionId::DebugAbbrev => Ok(EndianSlice::new(debug_abbrev, le)),
            gimli::SectionId::DebugStr => Ok(EndianSlice::new(debug_str, le)),
            gimli::SectionId::DebugStrOffsets => Ok(EndianSlice::new(debug_str_offsets, le)),
            gimli::SectionId::DebugLine => Ok(EndianSlice::new(debug_line, le)),
            gimli::SectionId::DebugRanges => Ok(EndianSlice::new(debug_ranges, le)),
            gimli::SectionId::DebugRngLists => Ok(EndianSlice::new(debug_rnglists, le)),
            gimli::SectionId::DebugAddr => Ok(EndianSlice::new(debug_addr, le)),
            _ => Ok(EndianSlice::new(&[], le)),
        }
    })
    .map_err(|e| format!("加载 DWARF sections 失败: {}", e))?;

    let mut result = DwarfResult {
        variables: Vec::new(),
        types: HashMap::new(),
        source_files: Vec::new(),
    };

    let mut units = dwarf.units();
    while let Some(header) = units.next().map_err(|e| format!("遍历 unit 失败: {}", e))? {
        let unit = dwarf.unit(header).map_err(|e| format!("读取 unit 失败: {}", e))?;

        // 提取源文件列表
        if let Some(ref program) = unit.line_program {
            let hdr = program.header();
            for file_entry in hdr.file_names() {
                if let Some(name) = dwarf_attr_string(&dwarf, file_entry.path_name()) {
                    if !result.source_files.contains(&name) {
                        result.source_files.push(name);
                    }
                }
            }
        }

        // 使用 flat entries 遍历（避免 entries_tree 的借用问题）
        collect_flat_entries(&unit, &dwarf, &mut result)?;
    }

    Ok(result)
}

/// 使用 entries() 平铺遍历，收集所有 DIE 信息
fn collect_flat_entries<'a>(
    unit: &gimli::Unit<R<'a>>,
    dwarf: &Dwarf<R<'a>>,
    result: &mut DwarfResult,
) -> Result<(), String> {
    // 第一遍：收集所有 raw DIE（需要跟踪 depth）
    let mut raw_dies: Vec<RawDie> = Vec::new();

    let mut entries = unit.entries();
    while let Some((delta, entry)) = entries.next_dfs().map_err(|e| format!("遍历 DIE 失败: {}", e))? {
        let offset = entry.offset().0 as u64;
        let tag = entry.tag();

        // 提取通用属性
        let name = entry_attr_string(entry, DW_AT_name);
        let byte_size = attr_u32(entry, DW_AT_byte_size);
        let type_ref = attr_type_ref(entry);

        // 提取变量地址
        let address = if tag == DW_TAG_variable {
            extract_address_from_entry(entry, unit)?
        } else {
            None
        };

        // 提取 struct member 偏移
        let member_offset = if tag == gimli::DW_TAG_member {
            attr_u32(entry, DW_AT_data_member_location)
        } else {
            None
        };

        // 提取 array subrange
        let (upper_bound, count) = if tag == DW_TAG_subrange_type {
            (attr_u32(entry, DW_AT_upper_bound), attr_u32(entry, DW_AT_count))
        } else {
            (None, None)
        };

        // 提取源文件信息
        let source_file = if tag == DW_TAG_variable {
            extract_decl_file_from_entry(entry, unit, dwarf)
        } else {
            None
        };

        let source_line = if tag == DW_TAG_variable {
            entry_attr(entry, DW_AT_decl_line)
                .and_then(|v| match v {
                    gimli::AttributeValue::Udata(val) => Some(val as u32),
                    _ => None,
                })
        } else {
            None
        };

        // 跟踪深度变化
        let depth = if raw_dies.is_empty() {
            0usize
        } else {
            let prev_depth = raw_dies.last().map(|d| d.depth).unwrap_or(0) as isize;
            (prev_depth + delta).max(0) as usize
        };

        raw_dies.push(RawDie {
            offset, tag, depth, name, byte_size, type_ref,
            address, member_offset, upper_bound, count,
            source_file, source_line,
        });
    }

    // 第二遍：处理收集到的 DIE
    let var_total = raw_dies.iter().filter(|d| d.tag == DW_TAG_variable).count();
    let var_with_addr = raw_dies.iter()
        .filter(|d| d.tag == DW_TAG_variable && d.address.is_some())
        .count();
    log::info!(
        "DWARF DIE: DW_TAG_variable 共 {} 个, 有地址 {} 个, 无地址 {} 个",
        var_total, var_with_addr, var_total - var_with_addr
    );
    process_raw_dies(&raw_dies, result);
    Ok(())
}

/// 处理收集到的 DIE，构建变量和类型信息
#[allow(non_upper_case_globals)]
fn process_raw_dies(raw_dies: &[RawDie], result: &mut DwarfResult) {
    let len = raw_dies.len();
    let mut i = 0;
    while i < len {
        let die = &raw_dies[i];

        match die.tag {
            DW_TAG_variable => {
                if let (Some(name), Some(address), Some(type_offset)) =
                    (&die.name, die.address, die.type_ref)
                {
                    result.variables.push(DwarfVarInfo {
                        name: name.clone(),
                        address,
                        type_offset,
                        source_file: die.source_file.clone(),
                        source_line: die.source_line,
                    });
                }
            }

            DW_TAG_base_type => {
                let name = die.name.clone().unwrap_or_else(|| "unknown".to_string());
                let byte_size = die.byte_size.unwrap_or(4);
                result.types.insert(die.offset, DwarfTypeInfo::Base { name, byte_size });
            }

            DW_TAG_typedef | DW_TAG_const_type | DW_TAG_volatile_type | DW_TAG_restrict_type => {
                let name = die.name.clone().unwrap_or_default();
                if let Some(target) = die.type_ref {
                    result.types.insert(die.offset, DwarfTypeInfo::Alias {
                        name,
                        target_offset: target,
                    });
                }
            }

            DW_TAG_enumeration_type => {
                // 枚举类型：按底层整数大小处理，等同于 base_type
                let name = die.name.clone().unwrap_or_else(|| "enum".to_string());
                let byte_size = die.byte_size.unwrap_or(4);
                result.types.insert(die.offset, DwarfTypeInfo::Base { name, byte_size });
            }

            DW_TAG_structure_type | DW_TAG_union_type => {
                let name = die.name.clone().unwrap_or_else(|| "anonymous".to_string());
                let byte_size = die.byte_size.unwrap_or(0);
                let parent_depth = die.depth;

                // 收集直接子成员
                let mut members = Vec::new();
                let mut j = i + 1;
                while j < len && raw_dies[j].depth > parent_depth {
                    if raw_dies[j].tag == gimli::DW_TAG_member && raw_dies[j].depth == parent_depth + 1 {
                        members.push(StructMember {
                            name: raw_dies[j].name.clone().unwrap_or_default(),
                            type_offset: raw_dies[j].type_ref.unwrap_or(0),
                            member_offset: raw_dies[j].member_offset.unwrap_or(0),
                        });
                    }
                    j += 1;
                }

                result.types.insert(die.offset, DwarfTypeInfo::Struct {
                    name, byte_size, members,
                });
            }

            DW_TAG_array_type => {
                if let Some(elem_type) = die.type_ref {
                    let byte_size = die.byte_size.unwrap_or(0);
                    let parent_depth = die.depth;

                    // 查找 subrange 子节点
                    let mut element_count: u32 = 0;
                    let mut j = i + 1;
                    while j < len && raw_dies[j].depth > parent_depth {
                        if raw_dies[j].tag == DW_TAG_subrange_type {
                            if let Some(count) = raw_dies[j].count {
                                element_count = count;
                            } else if let Some(upper) = raw_dies[j].upper_bound {
                                element_count = upper + 1;
                            }
                        }
                        j += 1;
                    }

                    result.types.insert(die.offset, DwarfTypeInfo::Array {
                        element_type_offset: elem_type,
                        element_count,
                        byte_size,
                    });
                }
            }

            DW_TAG_pointer_type => {
                let byte_size = die.byte_size.unwrap_or(4);
                result.types.insert(die.offset, DwarfTypeInfo::Pointer { byte_size });
            }

            _ => {}
        }

        i += 1;
    }
}

// ─── 辅助函数 ────────────────────────────────────────────────────────────────

fn extract_address_from_entry<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
    unit: &gimli::Unit<R<'a>>,
) -> Result<Option<u32>, String> {
    let val = match entry_attr(entry, DW_AT_location) {
        Some(v) => v,
        None => return Ok(None),
    };

    match val {
        gimli::AttributeValue::Exprloc(expr) => {
            let encoding = unit.encoding();
            let mut ops = expr.operations(encoding);
            if let Some(op) = ops.next().map_err(|e| format!("{}", e))? {
                if let gimli::Operation::Address { address } = op {
                    return Ok(Some(address as u32));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn attr_type_ref<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
) -> Option<u64> {
    let val = entry_attr(entry, DW_AT_type)?;
    match val {
        gimli::AttributeValue::UnitRef(offset) => Some(offset.0 as u64),
        gimli::AttributeValue::DebugInfoRef(offset) => Some(offset.0 as u64),
        _ => None,
    }
}

fn extract_decl_file_from_entry<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
    unit: &gimli::Unit<R<'a>>,
    dwarf: &Dwarf<R<'a>>,
) -> Option<String> {
    let val = entry_attr(entry, DW_AT_decl_file)?;
    match val {
        gimli::AttributeValue::FileIndex(idx) => {
            let program = unit.line_program.as_ref()?;
            let header = program.header();
            let file = header.file(idx)?;
            dwarf_attr_string(dwarf, file.path_name())
        }
        _ => None,
    }
}

fn entry_attr<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
    attr: gimli::DwAt,
) -> Option<gimli::AttributeValue<R<'a>>> {
    entry.attr_value(attr).ok().flatten()
}

fn entry_attr_string<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
    attr: gimli::DwAt,
) -> Option<String> {
    let val = entry_attr(entry, attr)?;
    match val {
        gimli::AttributeValue::String(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    }
}

fn dwarf_attr_string<'a>(
    dwarf: &Dwarf<R<'a>>,
    val: gimli::AttributeValue<R<'a>>,
) -> Option<String> {
    match val {
        gimli::AttributeValue::String(s) => Some(s.to_string_lossy().into_owned()),
        gimli::AttributeValue::DebugStrRef(offset) => {
            dwarf.debug_str.get_str(offset).ok()
                .map(|s| s.to_string_lossy().into_owned())
        }
        _ => None,
    }
}

fn attr_u32<'a>(
    entry: &gimli::DebuggingInformationEntry<'a, 'a, R<'a>, usize>,
    attr: gimli::DwAt,
) -> Option<u32> {
    let val = entry_attr(entry, attr)?;
    match val {
        gimli::AttributeValue::Udata(s) => Some(s as u32),
        gimli::AttributeValue::Data1(s) => Some(s as u32),
        gimli::AttributeValue::Data2(s) => Some(s as u32),
        gimli::AttributeValue::Data4(s) => Some(s),
        _ => None,
    }
}
