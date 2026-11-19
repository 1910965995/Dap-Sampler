/// 变量值类型
///
/// 决定如何将原始位模式解释为可显示的 f64 数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Float,
    Int32,
    Uint32,
    Int16,
    Uint16,
    Int8,
    Uint8,
    /// 64 位双精度浮点（占两个 32 位对齐字）
    Float64,
    /// 64 位有符号整数
    Int64,
    /// 64 位无符号整数
    Uint64,
}

impl ValueType {
    /// 从字符串解析类型名称
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "float" | "f32" => Ok(ValueType::Float),
            "int32" | "i32" => Ok(ValueType::Int32),
            "uint32" | "u32" => Ok(ValueType::Uint32),
            "int16" | "i16" => Ok(ValueType::Int16),
            "uint16" | "u16" => Ok(ValueType::Uint16),
            "int8" | "i8" => Ok(ValueType::Int8),
            "uint8" | "u8" => Ok(ValueType::Uint8),
            "double" | "float64" | "f64" => Ok(ValueType::Float64),
            "int64" | "i64" | "long long" => Ok(ValueType::Int64),
            "uint64" | "u64" | "unsigned long long" => Ok(ValueType::Uint64),
            _ => Err(format!(
                "未知类型: {}（支持: float, int32, uint32, int16, uint16, int8, uint8, double, int64, uint64）",
                s
            )),
        }
    }

    /// 返回类型的显示标签
    pub fn label(&self) -> &'static str {
        match self {
            ValueType::Float => "float",
            ValueType::Int32 => "int32",
            ValueType::Uint32 => "uint32",
            ValueType::Int16 => "int16",
            ValueType::Uint16 => "uint16",
            ValueType::Int8 => "int8",
            ValueType::Uint8 => "uint8",
            ValueType::Float64 => "double",
            ValueType::Int64 => "int64",
            ValueType::Uint64 => "uint64",
        }
    }

    /// 类型字节大小（用于内存访问对齐）
    pub fn byte_size(&self) -> u32 {
        match self {
            ValueType::Float | ValueType::Int32 | ValueType::Uint32 => 4,
            ValueType::Int16 | ValueType::Uint16 => 2,
            ValueType::Int8 | ValueType::Uint8 => 1,
            ValueType::Float64 | ValueType::Int64 | ValueType::Uint64 => 8,
        }
    }

    /// 该类型是否占 8 字节（需要读取两个 32 位对齐字）
    pub fn is_64bit(&self) -> bool {
        matches!(
            self,
            ValueType::Float64 | ValueType::Int64 | ValueType::Uint64
        )
    }

    /// 将原始位模式按本类型转换为 f64
    ///
    /// 64 位类型读取 `u64` 原始位模式（由引擎把两个对齐字按小端拼好）；
    /// 32 位及更窄类型只用低 32 位。
    pub fn to_f64(&self, raw: u64) -> f64 {
        match self {
            ValueType::Float => f64::from(f32::from_bits(raw as u32)),
            ValueType::Int32 => raw as u32 as i32 as f64,
            ValueType::Uint32 => raw as u32 as f64,
            ValueType::Int16 => (raw as u16) as i16 as f64,
            ValueType::Uint16 => (raw as u16) as f64,
            ValueType::Int8 => (raw as u8) as i8 as f64,
            ValueType::Uint8 => (raw as u8) as f64,
            ValueType::Float64 => f64::from_bits(raw),
            ValueType::Int64 => raw as i64 as f64,
            ValueType::Uint64 => raw as f64,
        }
    }
}

/// 单次采样支持的最大变量数（与上层 8 通道上限一致，见 main.rs/app.rs 校验）
pub const MAX_VALUES: usize = 8;

/// 单次采样点（包含多个变量的读取值）
///
/// `values` 使用定长数组而非 `Vec<u64>`：采样热路径（20kHz 下每秒 2 万次）
/// 零堆分配——clone/copy 退化为一次 memcpy，环形缓冲区与显示缓冲区的
/// 拷贝成本同步消失。
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// 全局采样序号（接收线程分配，单调递增）
    pub seq: u64,
    /// 真实时间戳（秒，从采集开始计时，由接收线程记录）
    pub timestamp_sec: f64,
    /// 各变量读取值（原始位模式，由上层按类型解释为 float/int）
    ///
    /// 每通道恒占一个 `u64` 槽：8 字节通道存放拼好的 64 位位模式，
    /// 32 位及更窄类型只用低 32 位。这样通道位置与值槽一一对应，
    /// 波形/导出/面板的取下标逻辑无需感知类型宽度。
    values: [u64; MAX_VALUES],
    /// 实际有效的变量个数（≤ MAX_VALUES）
    len: usize,
}

impl Sample {
    /// 从切片构造采样点（复制最多 MAX_VALUES 个值，超出部分截断）
    pub fn new(seq: u64, timestamp_sec: f64, values: &[u64]) -> Self {
        let len = values.len().min(MAX_VALUES);
        let mut arr = [0u64; MAX_VALUES];
        arr[..len].copy_from_slice(&values[..len]);
        Self { seq, timestamp_sec, values: arr, len }
    }

    /// 本次采样点实际携带的变量值切片（长度 = 变量数）
    pub fn values(&self) -> &[u64] {
        &self.values[..self.len]
    }

    /// 将原始位模式数组按 f32 解释
    pub fn as_floats(&self) -> Vec<f32> {
        self.values().iter().map(|&v| f32::from_bits(v as u32)).collect()
    }

    /// 将原始 u32 值数组解释为 f64 数组（egui_plot 使用 f64 坐标）
    ///
    /// 每个值的低 32 位被解释为 f32 的位模式，然后提升为 f64。
    pub fn as_f64s(&self) -> Vec<f64> {
        self.values().iter().map(|&v| f64::from(f32::from_bits(v as u32))).collect()
    }

    /// 根据类型列表将 u32 值转换为 f64 数组
    ///
    /// 支持不同变量使用不同类型解释（如 float、int32、uint16 等）。
    /// 若类型列表短于 values，缺失位置默认 float。
    pub fn as_f64s_typed(&self, types: &[ValueType]) -> Vec<f64> {
        self.values().iter().enumerate().map(|(i, &v)| {
            let t = types.get(i).copied().unwrap_or(ValueType::Float);
            t.to_f64(v)
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_size_matches_type_width() {
        assert_eq!(ValueType::Float.byte_size(), 4);
        assert_eq!(ValueType::Int32.byte_size(), 4);
        assert_eq!(ValueType::Uint32.byte_size(), 4);
        assert_eq!(ValueType::Int16.byte_size(), 2);
        assert_eq!(ValueType::Uint16.byte_size(), 2);
        assert_eq!(ValueType::Int8.byte_size(), 1);
        assert_eq!(ValueType::Uint8.byte_size(), 1);
        assert_eq!(ValueType::Float64.byte_size(), 8);
        assert_eq!(ValueType::Int64.byte_size(), 8);
        assert_eq!(ValueType::Uint64.byte_size(), 8);
    }

    #[test]
    fn is_64bit_flags_wide_types_only() {
        for t in [ValueType::Float64, ValueType::Int64, ValueType::Uint64] {
            assert!(t.is_64bit(), "{t:?} should be 64-bit");
        }
        for t in [
            ValueType::Float, ValueType::Int32, ValueType::Uint32,
            ValueType::Int16, ValueType::Uint16, ValueType::Int8, ValueType::Uint8,
        ] {
            assert!(!t.is_64bit(), "{t:?} should not be 64-bit");
        }
    }

    #[test]
    fn to_f64_interprets_64bit_types() {
        // double：原始位模式直接重解释
        assert_eq!(ValueType::Float64.to_f64(1.5f64.to_bits()), 1.5);
        // int64：有符号
        assert_eq!(ValueType::Int64.to_f64((-7i64) as u64), -7.0);
        // uint64：无符号
        assert_eq!(ValueType::Uint64.to_f64(9_007_199_254_740_993u64), 9_007_199_254_740_993.0);
        // 32 位类型忽略高 32 位（引擎对子字类型只填低字）
        assert_eq!(ValueType::Float.to_f64(u64::from(1.5f32.to_bits())), 1.5);
        assert_eq!(ValueType::Int32.to_f64(0xFFFF_FFFF_FFFF_FFFF), -1.0);
    }

    #[test]
    fn label_parse_round_trip_64bit() {
        for t in [ValueType::Float64, ValueType::Int64, ValueType::Uint64] {
            assert_eq!(ValueType::parse(t.label()).unwrap(), t);
        }
    }

    #[test]
    fn to_f64_u16_reads_low_lane_of_word() {
        // MemAccess.extract 产出的值低位对齐：u16 类型只解释低 16 位
        assert_eq!(ValueType::Uint16.to_f64(0x2222_1111), 0x1111 as f64);
        assert_eq!(ValueType::Int16.to_f64(0x0000_FFFF), -1.0);
        assert_eq!(ValueType::Uint8.to_f64(0xFFFF_FF22), 0x22 as f64);
    }
}
