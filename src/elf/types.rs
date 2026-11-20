use crate::pipeline::sample::ValueType;

/// 将 DWARF base_type 名称映射为 ValueType
///
/// 覆盖 GCC/Clang/armclang 与 Keil 的基类型命名（两家命名差异很大：
/// GCC 用 "long int"/"short int"/"long long int"，Keil 用 "long"/"short"），
/// 未识别的类型按字节大小回退。
///
/// 注意：名称映射与 byte_size 必须一致——同名类型在不同 ABI 下宽度不同
/// （如 ILP32 下 long 为 4 字节），所以按名称给出类型后还要用宽度做一次
/// 校验，避免把 8 字节的 `long long` 当成 4 字节解释。
pub fn map_dwarf_type(name: &str, byte_size: u32) -> ValueType {
    // 先按名称得到候选类型，再用实际宽度校验（防 ABI 差异与畸形 DWARF）
    let by_name: Option<ValueType> = match name {
        "float" => Some(ValueType::Float),
        "double" => Some(ValueType::Float64),
        // 注意：long double 在 ARM 上是 8 字节（同 double），在 x86 上是
        // 10/12/16 字节——只接受 8 字节，其他宽度交给宽度回退逻辑
        "long double" => Some(ValueType::Float64),
        "int" | "signed int" | "int32_t" | "long" | "long int" | "signed long"
        | "signed long int" => Some(ValueType::Int32),
        "unsigned int" | "uint32_t" | "unsigned long" | "long unsigned int"
        | "unsigned long int" | "size_t" | "unsigned" => Some(ValueType::Uint32),
        "short" | "short int" | "signed short" | "signed short int" | "int16_t" => {
            Some(ValueType::Int16)
        }
        "unsigned short" | "short unsigned int" | "unsigned short int" | "uint16_t"
        | "wchar_t" => Some(ValueType::Uint16),
        "signed char" | "int8_t" => Some(ValueType::Int8),
        "char" | "unsigned char" | "uint8_t" | "bool" | "_Bool" => Some(ValueType::Uint8),
        "long long" | "long long int" | "signed long long" | "int64_t" => {
            Some(ValueType::Int64)
        }
        "unsigned long long" | "long long unsigned int" | "unsigned long long int"
        | "uint64_t" => Some(ValueType::Uint64),
        _ => None,
    };

    // 宽度校验：名称给的类型宽度必须与实际字节大小一致，否则以宽度为准
    if let Some(vt) = by_name {
        if vt.byte_size() == byte_size {
            return vt;
        }
        log::debug!(
            "type name '{}' maps to {:?} ({} bytes) but DWARF size is {}, falling back by width",
            name,
            vt,
            vt.byte_size(),
            byte_size
        );
    }

    match byte_size {
        8 => ValueType::Float64, // 8 字节未知类型：按 double 解释最常见
        4 => ValueType::Float,   // 猜 float（嵌入式常见）
        2 => ValueType::Uint16,
        1 => ValueType::Uint8,
        _ => ValueType::Uint32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_types() {
        assert_eq!(map_dwarf_type("float", 4), ValueType::Float);
        assert_eq!(map_dwarf_type("int32_t", 4), ValueType::Int32);
        assert_eq!(map_dwarf_type("uint16_t", 2), ValueType::Uint16);
        assert_eq!(map_dwarf_type("uint8_t", 1), ValueType::Uint8);
    }

    #[test]
    fn test_fallback_by_size() {
        assert_eq!(map_dwarf_type("my_custom_type", 4), ValueType::Float);
        assert_eq!(map_dwarf_type("unknown", 2), ValueType::Uint16);
        assert_eq!(map_dwarf_type("unknown", 8), ValueType::Float64);
    }

    #[test]
    fn gcc_base_type_names_are_recognized() {
        // GCC/Clang/armclang 的 DWARF 命名（旧实现全部落到宽度兜底，
        // 导致 long 被当成 float 显示乱码）
        assert_eq!(map_dwarf_type("long int", 4), ValueType::Int32);
        assert_eq!(map_dwarf_type("signed long int", 4), ValueType::Int32);
        assert_eq!(map_dwarf_type("long unsigned int", 4), ValueType::Uint32);
        assert_eq!(map_dwarf_type("unsigned long int", 4), ValueType::Uint32);
        assert_eq!(map_dwarf_type("short int", 2), ValueType::Int16);
        assert_eq!(map_dwarf_type("short unsigned int", 2), ValueType::Uint16);
        assert_eq!(map_dwarf_type("unsigned short int", 2), ValueType::Uint16);
    }

    #[test]
    fn wide_types_map_to_64bit_variants() {
        assert_eq!(map_dwarf_type("double", 8), ValueType::Float64);
        assert_eq!(map_dwarf_type("long double", 8), ValueType::Float64);
        assert_eq!(map_dwarf_type("long long int", 8), ValueType::Int64);
        assert_eq!(map_dwarf_type("signed long long", 8), ValueType::Int64);
        assert_eq!(map_dwarf_type("int64_t", 8), ValueType::Int64);
        assert_eq!(map_dwarf_type("long long unsigned int", 8), ValueType::Uint64);
        assert_eq!(map_dwarf_type("uint64_t", 8), ValueType::Uint64);
    }

    #[test]
    fn name_is_validated_against_actual_width() {
        // long double 在 x86 上是 16 字节：宽校验失败，按宽度回退（未知 16 字节 → uint32 兜底）
        assert_eq!(map_dwarf_type("long double", 16), ValueType::Uint32);
        // double 若是 4 字节（畸形 DWARF）：宽校验失败，按 4 字节回退 float
        assert_eq!(map_dwarf_type("double", 4), ValueType::Float);
        // 名称给 4 字节但实际 8 字节：按宽度回退到 8 字节类型
        assert_eq!(map_dwarf_type("uint32_t", 8), ValueType::Float64);
    }
}
