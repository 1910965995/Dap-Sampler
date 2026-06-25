use crate::pipeline::sample::ValueType;

/// 将 DWARF base_type 名称映射为 ValueType
///
/// 覆盖常见的 ARM Cortex-M 嵌入式 C 类型。
/// 未识别的类型按字节大小推断（2→u16, 4→u32, 1→u8）。
pub fn map_dwarf_type(name: &str, byte_size: u32) -> ValueType {
    match name {
        "float" | "double" => ValueType::Float,
        "int" | "signed int" | "int32_t" | "long" => ValueType::Int32,
        "unsigned int" | "uint32_t" | "unsigned long" | "size_t" => ValueType::Uint32,
        "short" | "signed short" | "int16_t" => ValueType::Int16,
        "unsigned short" | "uint16_t" | "wchar_t" => ValueType::Uint16,
        "signed char" | "int8_t" => ValueType::Int8,
        "char" | "unsigned char" | "uint8_t" | "bool" | "_Bool" => ValueType::Uint8,
        _ => {
            // 回退：按字节大小推断
            match byte_size {
                8 => ValueType::Float,   // double
                4 => ValueType::Float,   // 猜 float（嵌入式常见）
                2 => ValueType::Uint16,
                1 => ValueType::Uint8,
                _ => ValueType::Uint32,  // 兜底
            }
        }
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
    }
}
