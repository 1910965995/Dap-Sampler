/// 变量值类型
///
/// 决定如何将 u32 原始值解释为可显示的 f64 数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    Float,
    Int32,
    Uint32,
    Int16,
    Uint16,
    Int8,
    Uint8,
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
            _ => Err(format!("未知类型: {}（支持: float, int32, uint32, int16, uint16, int8, uint8）", s)),
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
        }
    }

    /// 将 u32 原始值按本类型转换为 f64
    pub fn to_f64(&self, raw: u32) -> f64 {
        match self {
            ValueType::Float => f64::from(f32::from_bits(raw)),
            ValueType::Int32 => raw as i32 as f64,
            ValueType::Uint32 => raw as f64,
            ValueType::Int16 => (raw as i16) as f64,
            ValueType::Uint16 => (raw as u16) as f64,
            ValueType::Int8 => (raw as i8) as f64,
            ValueType::Uint8 => (raw as u8) as f64,
        }
    }
}

/// 单次采样支持的最大变量数（与上层 8 通道上限一致，见 main.rs/app.rs 校验）
pub const MAX_VALUES: usize = 8;

/// 单次采样点（包含多个变量的读取值）
///
/// `values` 使用定长数组而非 `Vec<u32>`：采样热路径（20kHz 下每秒 2 万次）
/// 零堆分配——clone/copy 退化为一次 memcpy，环形缓冲区与显示缓冲区的
/// 拷贝成本同步消失。
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// 全局采样序号（接收线程分配，单调递增）
    pub seq: u64,
    /// 真实时间戳（秒，从采集开始计时，由接收线程记录）
    pub timestamp_sec: f64,
    /// 各变量读取值（u32 原始值，由上层按需解释为 float/int）
    values: [u32; MAX_VALUES],
    /// 实际有效的变量个数（≤ MAX_VALUES）
    len: usize,
}

impl Sample {
    /// 从切片构造采样点（复制最多 MAX_VALUES 个值，超出部分截断）
    pub fn new(seq: u64, timestamp_sec: f64, values: &[u32]) -> Self {
        let len = values.len().min(MAX_VALUES);
        let mut arr = [0u32; MAX_VALUES];
        arr[..len].copy_from_slice(&values[..len]);
        Self { seq, timestamp_sec, values: arr, len }
    }

    /// 本次采样点实际携带的变量值切片（长度 = 变量数）
    pub fn values(&self) -> &[u32] {
        &self.values[..self.len]
    }

    /// 将原始 u32 值数组解释为 f32 数组
    pub fn as_floats(&self) -> Vec<f32> {
        self.values().iter().map(|&v| f32::from_bits(v)).collect()
    }

    /// 将原始 u32 值数组解释为 f64 数组（egui_plot 使用 f64 坐标）
    ///
    /// 每个 u32 被解释为 f32 的位模式，然后提升为 f64。
    pub fn as_f64s(&self) -> Vec<f64> {
        self.values().iter().map(|&v| f64::from(f32::from_bits(v))).collect()
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
