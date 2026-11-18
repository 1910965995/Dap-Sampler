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
    /// 缓冲区容量（历史数据保留量，采样点数上限）
    #[serde(default = "default_buffer_capacity")]
    pub buffer_capacity: usize,
    /// 波形显示模式（小写："line" / "point"）
    pub display_mode: String,
    /// Watch 面板列宽（px，按 Name/Address/.../Remark 列顺序）
    #[serde(default = "default_watch_col_widths")]
    pub watch_col_widths: Vec<f32>,
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

/// 旧版工程文件缺失 buffer_capacity 字段时的默认值（serde default 用）
fn default_buffer_capacity() -> usize {
    crate::ui::controls::DEFAULT_BUFFER_CAPACITY
}

/// 旧版工程文件缺失 watch_col_widths 字段时的默认列宽（serde default 用）
fn default_watch_col_widths() -> Vec<f32> {
    crate::ui::watch_panel::DEFAULT_COL_WIDTHS.to_vec()
}

impl ProjectFile {
    /// 序列化为 pretty JSON（字段按声明顺序输出，便于 diff）
    ///
    /// 浮点字段（y_offset / y_scale / refresh_period_s）可能来自用户输入，
    /// 非有限浮点数（NaN/Infinity）先主动拒绝，避免写出损坏文件，而不是 panic。
    pub fn to_json(&self) -> Result<String> {
        for v in &self.variables {
            if !v.y_offset.is_finite() || !v.y_scale.is_finite() || !v.refresh_period_s.is_finite() {
                return Err(Error::Other(format!(
                    "Project file serialization failed: variable \"{}\" contains non-finite float (NaN/Infinity)",
                    v.name
                )));
            }
        }
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::Other(format!("Project file serialization failed: {}", e)))
    }

    /// 从 JSON 字符串反序列化，并校验版本号
    pub fn from_json(text: &str) -> Result<ProjectFile> {
        let file: ProjectFile = serde_json::from_str(text)
            .map_err(|e| Error::Other(format!("Project file parse failed: {}", e)))?;
        if file.version != PROJECT_VERSION {
            return Err(Error::Other(format!(
                "Unsupported project file version: file version {}, current supported version {}",
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

    fn sample_file() -> ProjectFile {
        ProjectFile {
            version: PROJECT_VERSION,
            elf_path: Some("C:/fw/firmware.elf".to_string()),
            sample_rate_hz: 5000,
            swd_clock_mhz: 10,
            window_size: 2000,
            buffer_capacity: 200_000,
            display_mode: "line".to_string(),
            watch_col_widths: crate::ui::watch_panel::DEFAULT_COL_WIDTHS.to_vec(),
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
        let json = file.to_json().unwrap();
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
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored, file);
        assert!(restored.elf_path.is_none());
        assert!(restored.variables.is_empty());
    }

    #[test]
    fn corrupted_json_returns_err() {
        let err = ProjectFile::from_json("{ not valid json !!").unwrap_err();
        assert!(err.to_string().contains("parse failed"), "error should mention parse failure: {}", err);
    }

    #[test]
    fn unsupported_version_returns_err() {
        let mut file = sample_file();
        file.version = 99;
        let json = file.to_json().unwrap();
        let err = ProjectFile::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("Unsupported project file version"), "error should mention unsupported version: {}", err);
    }

    #[test]
    fn invalid_value_type_falls_back_to_uint32() {
        let mut file = sample_file();
        file.variables[0].value_type = "banana".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.variables[0].value_type(), ValueType::Uint32);
    }

    #[test]
    fn invalid_display_mode_falls_back_to_line() {
        let mut file = sample_file();
        file.display_mode = "3d".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.display_mode(), WaveformDisplayMode::Line);
    }

    #[test]
    fn display_mode_uppercase_parses() {
        let mut file = sample_file();
        file.display_mode = "POINT".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.display_mode(), WaveformDisplayMode::Point);
    }

    #[test]
    fn nan_value_serialization_returns_err() {
        let mut file = sample_file();
        file.variables[0].y_scale = f32::NAN;
        assert!(file.to_json().is_err());
    }

    #[test]
    fn value_type_round_trip_through_label() {
        for vt in [ValueType::Float, ValueType::Int32, ValueType::Uint32,
                   ValueType::Int16, ValueType::Uint16, ValueType::Int8, ValueType::Uint8] {
            let s = vt.label().to_string();
            assert_eq!(ValueType::parse(&s).unwrap(), vt);
        }
    }

    #[test]
    fn watch_col_widths_round_trip() {
        let mut file = sample_file();
        file.watch_col_widths = vec![200.0, 100.0, 80.0, 120.0, 90.0, 100.0, 100.0, 260.0];
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.watch_col_widths, file.watch_col_widths);
    }

    #[test]
    fn old_project_without_watch_col_widths_gets_defaults() {
        // 模拟旧版工程 JSON（无 watch_col_widths 字段）
        let json = r#"{
            "version": 1,
            "elf_path": null,
            "sample_rate_hz": 5000,
            "swd_clock_mhz": 10,
            "window_size": 2000,
            "display_mode": "line",
            "variables": []
        }"#;
        let restored = ProjectFile::from_json(json).unwrap();
        assert_eq!(
            restored.watch_col_widths,
            crate::ui::watch_panel::DEFAULT_COL_WIDTHS.to_vec()
        );
    }

    #[test]
    fn buffer_capacity_round_trip() {
        let mut file = sample_file();
        file.buffer_capacity = 500_000;
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.buffer_capacity, 500_000);
    }

    #[test]
    fn old_project_without_buffer_capacity_defaults_to_200k() {
        // 模拟旧版工程 JSON（无 buffer_capacity 字段）
        let json = r#"{
            "version": 1,
            "elf_path": null,
            "sample_rate_hz": 5000,
            "swd_clock_mhz": 10,
            "window_size": 2000,
            "display_mode": "line",
            "variables": []
        }"#;
        let restored = ProjectFile::from_json(json).unwrap();
        // 与 controls.rs 的 DEFAULT_BUFFER_CAPACITY（200_000）保持一致
        assert_eq!(restored.buffer_capacity, 200_000);
    }
}
