//! 工程文件：保存/恢复 DAP Sampler 工作现场（JSON 格式，版本号可迁移）
//!
//! v2 起支持多示波器 scope：每个 scope 携带独立的采样参数与变量列表，
//! ELF 路径 / Buffer capacity / Watch 列宽仍为全局共享。

use serde::{Deserialize, Serialize};
use crate::error::{Error, Result};
use crate::pipeline::sample::ValueType;
use crate::ui::waveform::WaveformDisplayMode;

/// 当前支持的工程文件版本（将来格式变化时递增并做迁移）
pub const PROJECT_VERSION: u32 = 2;

/// 工程文件顶层结构（与 .dapsproj 文件内容一一对应，v2）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFile {
    /// 格式版本号（读入时校验）
    pub version: u32,
    /// ELF 固件路径（None = 手工地址模式）
    pub elf_path: Option<String>,
    /// 缓冲区容量（历史数据保留量，采样点数上限；所有 scope 共享）
    #[serde(default = "default_buffer_capacity")]
    pub buffer_capacity: usize,
    /// Watch 面板列宽（px，按 Name/Address/.../Remark 列顺序；所有 scope 共享）
    #[serde(default = "default_watch_col_widths")]
    pub watch_col_widths: Vec<f32>,
    /// 示波器 scope 列表（v2 起每个 scope 独立参数与变量）
    #[serde(default)]
    pub scopes: Vec<ProjectScope>,
    /// 界面语言代码（"en" / "zh"；空 = 英文。缺省字段向后兼容旧文件）
    #[serde(default)]
    pub language: String,
}

/// 单个示波器 scope 的配置
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectScope {
    /// scope 名称（标签条显示，可重命名）
    pub name: String,
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

/// v1 工程文件结构（单 scope，参数与变量在顶层）——仅用于读取旧文件并迁移
#[derive(Debug, Deserialize)]
struct ProjectFileV1 {
    #[allow(dead_code)]
    version: u32,
    elf_path: Option<String>,
    sample_rate_hz: u32,
    swd_clock_mhz: u32,
    window_size: usize,
    #[serde(default = "default_buffer_capacity")]
    buffer_capacity: usize,
    #[serde(default = "default_watch_col_widths")]
    watch_col_widths: Vec<f32>,
    display_mode: String,
    #[serde(default)]
    variables: Vec<ProjectVariable>,
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
        for scope in &self.scopes {
            for v in &scope.variables {
                if !v.y_offset.is_finite() || !v.y_scale.is_finite() || !v.refresh_period_s.is_finite() {
                    return Err(Error::Other(format!(
                        "Project file serialization failed: scope \"{}\" variable \"{}\" contains non-finite float (NaN/Infinity)",
                        scope.name, v.name
                    )));
                }
            }
        }
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::Other(format!("Project file serialization failed: {}", e)))
    }

    /// 从 JSON 字符串反序列化，并校验版本号
    ///
    /// v1（单 scope，参数在顶层）自动迁移为一个名为 "Scope 1" 的 scope；
    /// 其余未知版本报错。
    pub fn from_json(text: &str) -> Result<ProjectFile> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| Error::Other(format!("Project file parse failed: {}", e)))?;
        let version = value
            .get("version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                Error::Other("Project file parse failed: missing version field".to_string())
            })?;
        if version == PROJECT_VERSION as u64 {
            let file: ProjectFile = serde_json::from_value(value)
                .map_err(|e| Error::Other(format!("Project file parse failed: {}", e)))?;
            Ok(file)
        } else if version == 1 {
            let v1: ProjectFileV1 = serde_json::from_value(value)
                .map_err(|e| Error::Other(format!("Project file parse failed: {}", e)))?;
            Ok(migrate_v1(v1))
        } else {
            Err(Error::Other(format!(
                "Unsupported project file version: file version {}, current supported version {}",
                version, PROJECT_VERSION
            )))
        }
    }
}

/// v1 → v2 迁移：顶层参数与变量打包为一个 scope
fn migrate_v1(v1: ProjectFileV1) -> ProjectFile {
    ProjectFile {
        version: PROJECT_VERSION,
        elf_path: v1.elf_path,
        language: String::new(),
        buffer_capacity: v1.buffer_capacity,
        watch_col_widths: v1.watch_col_widths,
        scopes: vec![ProjectScope {
            name: "Scope 1".to_string(),
            sample_rate_hz: v1.sample_rate_hz,
            swd_clock_mhz: v1.swd_clock_mhz,
            window_size: v1.window_size,
            display_mode: v1.display_mode,
            variables: v1.variables,
        }],
    }
}

impl ProjectScope {
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

    /// v2 示例文件：两个 scope，参数与变量各不相同
    fn sample_file() -> ProjectFile {
        let var = |name: &str, addr: u32, vtype: &str, remark: &str| ProjectVariable {
            name: name.to_string(),
            address: addr,
            value_type: vtype.to_string(),
            refresh_period_s: 0.1,
            y_offset: 0.0,
            y_scale: 1.0,
            remark: remark.to_string(),
        };
        ProjectFile {
            version: PROJECT_VERSION,
            elf_path: Some("C:/fw/firmware.elf".to_string()),
            buffer_capacity: 200_000,
            language: "en".to_string(),
            watch_col_widths: crate::ui::watch_panel::DEFAULT_COL_WIDTHS.to_vec(),
            scopes: vec![
                ProjectScope {
                    name: "Scope 1".to_string(),
                    sample_rate_hz: 5000,
                    swd_clock_mhz: 10,
                    window_size: 2000,
                    display_mode: "line".to_string(),
                    variables: vec![
                        var("pid.kp", 0x2000_0100, "float", "比例系数"),
                        var("motor.speed", 0x2000_0104, "int16", ""),
                    ],
                },
                ProjectScope {
                    name: "电流环".to_string(),
                    sample_rate_hz: 20_000,
                    swd_clock_mhz: 1,
                    window_size: 5000,
                    display_mode: "point".to_string(),
                    variables: vec![var("motor.current", 0x2000_0200, "float", "")],
                },
            ],
        }
    }

    /// v1 示例 JSON（顶层参数 + 变量，无 scopes 字段）
    fn sample_v1_json() -> String {
        r#"{
            "version": 1,
            "elf_path": "C:/fw/firmware.elf",
            "sample_rate_hz": 5000,
            "swd_clock_mhz": 10,
            "window_size": 2000,
            "buffer_capacity": 300000,
            "display_mode": "line",
            "variables": [
                {
                    "name": "pid.kp",
                    "address": 2684355328,
                    "value_type": "float",
                    "refresh_period_s": 0.1,
                    "y_offset": 0.0,
                    "y_scale": -2.5,
                    "remark": "比例系数"
                }
            ]
        }"#
        .to_string()
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let file = sample_file();
        let json = file.to_json().unwrap();
        let restored = ProjectFile::from_json(&json).expect("round-trip 不应失败");
        assert_eq!(restored, file);
        // 中文 remark 完好
        assert_eq!(restored.scopes[0].variables[0].remark, "比例系数");
        // scope 名称与参数完好
        assert_eq!(restored.scopes[1].name, "电流环");
        assert_eq!(restored.scopes[1].sample_rate_hz, 20_000);
    }

    #[test]
    fn v1_file_migrates_to_single_scope() {
        let restored = ProjectFile::from_json(&sample_v1_json()).expect("v1 迁移不应失败");
        assert_eq!(restored.version, PROJECT_VERSION);
        assert_eq!(restored.scopes.len(), 1);
        let scope = &restored.scopes[0];
        assert_eq!(scope.name, "Scope 1");
        assert_eq!(scope.sample_rate_hz, 5000);
        assert_eq!(scope.swd_clock_mhz, 10);
        assert_eq!(scope.window_size, 2000);
        assert_eq!(scope.display_mode(), WaveformDisplayMode::Line);
        assert_eq!(scope.variables.len(), 1);
        // 负 y_scale 完好
        assert_eq!(scope.variables[0].y_scale, -2.5);
        // 全局字段完好
        assert_eq!(restored.buffer_capacity, 300_000);
        assert_eq!(restored.elf_path.as_deref(), Some("C:/fw/firmware.elf"));
    }

    #[test]
    fn v1_file_without_optional_fields_gets_defaults() {
        // 模拟更旧的 v1 JSON（无 buffer_capacity / watch_col_widths）
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
        assert_eq!(restored.buffer_capacity, 200_000);
        assert_eq!(
            restored.watch_col_widths,
            crate::ui::watch_panel::DEFAULT_COL_WIDTHS.to_vec()
        );
        assert_eq!(restored.scopes.len(), 1);
        assert!(restored.scopes[0].variables.is_empty());
    }

    #[test]
    fn round_trip_manual_mode_without_elf() {
        let mut file = sample_file();
        file.elf_path = None;
        for scope in &mut file.scopes {
            scope.variables = vec![];
        }
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored, file);
        assert!(restored.elf_path.is_none());
        assert!(restored.scopes.iter().all(|s| s.variables.is_empty()));
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
        file.scopes[0].variables[0].value_type = "banana".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.scopes[0].variables[0].value_type(), ValueType::Uint32);
    }

    #[test]
    fn invalid_display_mode_falls_back_to_line() {
        let mut file = sample_file();
        file.scopes[0].display_mode = "3d".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.scopes[0].display_mode(), WaveformDisplayMode::Line);
    }

    #[test]
    fn display_mode_uppercase_parses() {
        let mut file = sample_file();
        file.scopes[1].display_mode = "POINT".to_string();
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.scopes[1].display_mode(), WaveformDisplayMode::Point);
    }

    #[test]
    fn nan_value_serialization_returns_err() {
        let mut file = sample_file();
        file.scopes[0].variables[0].y_scale = f32::NAN;
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
    fn buffer_capacity_round_trip() {
        let mut file = sample_file();
        file.buffer_capacity = 500_000;
        let restored = ProjectFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(restored.buffer_capacity, 500_000);
    }
}
