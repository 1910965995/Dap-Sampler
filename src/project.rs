//! 工程文件：保存/恢复 DAP Sampler 工作现场（JSON 格式，版本号可迁移）

use serde::{Deserialize, Serialize};
use crate::error::{Error, Result};
use crate::pipeline::sample::ValueType;
use crate::ui::waveform::WaveformDisplayMode;

/// 当前支持的工程文件版本（将来格式变化时递增并做迁移）
pub const PROJECT_VERSION: u32 = 1;

/// 工程文件顶层结构（与 .dapsproj 文件内容一一对应）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFile {
    /// 格式版本号（读入时校验）
    pub version: u32,
    /// ELF 固件路径（None = 手工地址模式）
    pub elf_path: Option<String>,
    /// 采样率 (Hz)
    pub sample_rate_hz: u32,
    /// SWD 时钟 (MHz)
    pub swd_clock_mhz: u32,
    /// 显示窗口大小（采样点数）
    pub window_size: usize,
    /// 波形显示模式（小写："line" / "point"）
    pub display_mode: String,
    /// 变量列表（Watch 通道）
    pub variables: Vec<ProjectVariable>,
}

/// 单个变量（对应 Watch 面板一行）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectVariable {
    /// 变量名（ELF 模式为变量 path，手工模式为 "CH1 0x..." 格式）
    pub name: String,
    /// 内存地址
    pub address: u32,
    /// 变量类型（小写字符串，如 "float"、"int16"）
    pub value_type: String,
    /// 刷新周期（秒）
    pub refresh_period_s: f32,
    /// Y 轴偏移
    pub y_offset: f32,
    /// Y 轴缩放（禁止 0；允许负数）
    pub y_scale: f32,
    /// 用户自定义备注
    pub remark: String,
}

impl ProjectFile {
    /// 序列化为 pretty JSON（字段按声明顺序输出，便于 diff）
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("ProjectFile 序列化不应失败")
    }

    /// 从 JSON 字符串反序列化，并校验版本号
    pub fn from_json(text: &str) -> Result<ProjectFile> {
        let file: ProjectFile = serde_json::from_str(text)
            .map_err(|e| Error::Other(format!("工程文件解析失败: {}", e)))?;
        if file.version != PROJECT_VERSION {
            return Err(Error::Other(format!(
                "工程文件版本不支持: 文件版本 {}, 当前支持版本 {}",
                file.version, PROJECT_VERSION
            )));
        }
        Ok(file)
    }

    /// 解析显示模式（未知字符串宽容回退 Line，不整体报错）
    pub fn display_mode(&self) -> WaveformDisplayMode {
        match self.display_mode.to_lowercase().as_str() {
            "point" => WaveformDisplayMode::Point,
            _ => WaveformDisplayMode::Line,
        }
    }
}

impl ProjectVariable {
    /// 解析变量类型（未知字符串宽容回退 Uint32，不整体报错）
    pub fn value_type(&self) -> ValueType {
        ValueType::parse(&self.value_type).unwrap_or(ValueType::Uint32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::sample::ValueType;
    use crate::ui::waveform::WaveformDisplayMode;

    fn sample_file() -> ProjectFile {
        ProjectFile {
            version: PROJECT_VERSION,
            elf_path: Some("C:/fw/firmware.elf".to_string()),
            sample_rate_hz: 5000,
            swd_clock_mhz: 10,
            window_size: 2000,
            display_mode: "line".to_string(),
            variables: vec![
                ProjectVariable {
                    name: "pid.kp".to_string(),
                    address: 0x2000_0100,
                    value_type: "float".to_string(),
                    refresh_period_s: 0.1,
                    y_offset: 0.0,
                    y_scale: -2.5, // 允许负缩放
                    remark: "比例系数".to_string(), // 中文
                },
                ProjectVariable {
                    name: "motor.speed".to_string(),
                    address: 0x2000_0104,
                    value_type: "int16".to_string(),
                    refresh_period_s: 0.05,
                    y_offset: 100.0,
                    y_scale: 1.0,
                    remark: String::new(),
                },
            ],
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let file = sample_file();
        let json = file.to_json();
        let restored = ProjectFile::from_json(&json).expect("round-trip 不应失败");
        assert_eq!(restored, file);
        // 中文 remark 完好
        assert_eq!(restored.variables[0].remark, "比例系数");
        // 负 y_scale 完好
        assert_eq!(restored.variables[0].y_scale, -2.5);
    }

    #[test]
    fn round_trip_manual_mode_without_elf() {
        let mut file = sample_file();
        file.elf_path = None;
        file.variables = vec![];
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored, file);
        assert!(restored.elf_path.is_none());
        assert!(restored.variables.is_empty());
    }

    #[test]
    fn corrupted_json_returns_err() {
        let err = ProjectFile::from_json("{ not valid json !!").unwrap_err();
        assert!(err.to_string().contains("解析失败"), "错误应说明解析失败: {}", err);
    }

    #[test]
    fn unsupported_version_returns_err() {
        let mut file = sample_file();
        file.version = 99;
        let json = file.to_json();
        let err = ProjectFile::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("版本不支持"), "错误应说明版本不支持: {}", err);
    }

    #[test]
    fn invalid_value_type_falls_back_to_uint32() {
        let mut file = sample_file();
        file.variables[0].value_type = "banana".to_string();
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored.variables[0].value_type(), ValueType::Uint32);
    }

    #[test]
    fn invalid_display_mode_falls_back_to_line() {
        let mut file = sample_file();
        file.display_mode = "3d".to_string();
        let restored = ProjectFile::from_json(&file.to_json()).unwrap();
        assert_eq!(restored.display_mode(), WaveformDisplayMode::Line);
    }

    #[test]
    fn value_type_round_trip_through_label() {
        for vt in [ValueType::Float, ValueType::Int32, ValueType::Uint32,
                   ValueType::Int16, ValueType::Uint16, ValueType::Int8, ValueType::Uint8] {
            let s = vt.label().to_string();
            assert_eq!(ValueType::parse(&s).unwrap(), vt);
        }
    }
}
