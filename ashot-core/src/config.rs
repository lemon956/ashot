use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    document::{Color, DefaultTool},
    ocr::{
        OcrBackend, default_ocr_backend, default_ocr_filter_symbols, default_ocr_languages,
        default_ocr_space_engine,
    },
};

const CONFIG_DIR_NAME: &str = "ashot";
const CONFIG_FILE_NAME: &str = "config.toml";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AppearanceMode {
    System,
    Light,
    Dark,
}

pub fn default_appearance_mode() -> AppearanceMode {
    AppearanceMode::System
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("the current environment does not expose XDG base directories")]
    MissingXdgDirs,
    #[error("failed to create config directory at {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read config file at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to write config file at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    pub default_save_dir: PathBuf,
    pub filename_template: String,
    pub auto_copy: bool,
    pub post_capture_open_editor: bool,
    pub pin_after_save: bool,
    pub default_tool: DefaultTool,
    pub default_color: Color,
    pub default_stroke_width: u32,
    #[serde(default)]
    pub recent_colors: Vec<Color>,
    #[serde(default)]
    pub favorite_colors: Vec<Color>,
    #[serde(default = "default_pin_scale")]
    pub last_pin_scale: f64,
    #[serde(default = "default_pin_opacity")]
    pub last_pin_opacity: f64,
    #[serde(default = "default_appearance_mode")]
    pub appearance_mode: AppearanceMode,
    #[serde(default = "default_ocr_backend")]
    pub ocr_backend: OcrBackend,
    #[serde(default = "default_ocr_languages")]
    pub ocr_languages: Vec<String>,
    #[serde(default)]
    pub ocr_space_api_key: String,
    #[serde(default = "default_ocr_space_engine")]
    pub ocr_space_engine: u8,
    #[serde(default = "default_ocr_filter_symbols")]
    pub ocr_filter_symbols: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_save_dir: default_save_dir(),
            filename_template: "Screenshot_%Y-%m-%d_%H-%M-%S.png".to_string(),
            auto_copy: true,
            post_capture_open_editor: true,
            pin_after_save: false,
            default_tool: DefaultTool::Arrow,
            default_color: Color::rgba(232, 62, 38, 255),
            default_stroke_width: 4,
            recent_colors: Vec::new(),
            favorite_colors: Vec::new(),
            last_pin_scale: default_pin_scale(),
            last_pin_opacity: default_pin_opacity(),
            appearance_mode: default_appearance_mode(),
            ocr_backend: OcrBackend::Tesseract,
            ocr_languages: default_ocr_languages(),
            ocr_space_api_key: String::new(),
            ocr_space_engine: default_ocr_space_engine(),
            ocr_filter_symbols: default_ocr_filter_symbols(),
        }
    }
}

impl AppConfig {
    pub fn config_dir() -> Result<PathBuf, ConfigError> {
        let base = BaseDirs::new().ok_or(ConfigError::MissingXdgDirs)?;
        Ok(base.config_dir().join(CONFIG_DIR_NAME))
    }

    pub fn config_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::config_dir()?.join(CONFIG_FILE_NAME))
    }

    pub fn load_or_create() -> Result<Self, ConfigError> {
        Self::load_or_create_at(Self::config_path()?)
    }

    pub fn load_or_create_at(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        if path.exists() {
            return Self::load_from(&path);
        }

        let config = Self::default();
        config.save_to(&path)?;
        Ok(config)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let data = fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        let config = toml::from_str(&data)
            .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })?;
        Ok(config)
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(Self::config_path()?)
    }

    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| ConfigError::CreateDir { path: parent.to_path_buf(), source })?;
        }
        if !self.default_save_dir.exists() {
            fs::create_dir_all(&self.default_save_dir).map_err(|source| {
                ConfigError::CreateDir { path: self.default_save_dir.clone(), source }
            })?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(path, content)
            .map_err(|source| ConfigError::Write { path: path.to_path_buf(), source })
    }

    pub fn restore_defaults(&mut self) {
        *self = Self::default();
    }
}

fn default_save_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join("Pictures").join("Screenshots"))
        .unwrap_or_else(|| PathBuf::from("./Screenshots"))
}

fn default_pin_scale() -> f64 {
    1.0
}

fn default_pin_opacity() -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::ocr::OcrBackend;

    use super::{AppConfig, AppearanceMode};

    #[test]
    fn load_or_create_round_trip() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let config = AppConfig::load_or_create_at(&path).expect("create config");
        assert!(path.exists());
        assert!(config.default_save_dir.exists());

        let loaded = AppConfig::load_from(&path).expect("load config");
        assert_eq!(config, loaded);
    }

    #[test]
    fn ocr_defaults_to_offline_chinese_and_english() {
        let config = AppConfig::default();

        assert_eq!(config.ocr_backend, OcrBackend::Tesseract);
        assert_eq!(config.ocr_languages, vec!["chi_sim".to_string(), "eng".to_string()]);
        assert_eq!(config.ocr_space_engine, 2);
        assert!(config.ocr_filter_symbols);
        assert!(config.ocr_space_api_key.is_empty());
    }

    #[test]
    fn legacy_config_without_ocr_fields_loads_with_defaults() {
        let dir = tempdir().expect("tempdir");
        let save_dir = dir.path().join("shots");
        std::fs::create_dir_all(&save_dir).expect("save dir");
        let path = dir.path().join("legacy.toml");
        std::fs::write(
            &path,
            format!(
                r#"
default_save_dir = "{}"
filename_template = "Screenshot.png"
auto_copy = true
post_capture_open_editor = true
pin_after_save = false
default_tool = "Arrow"
default_color = {{ r = 232, g = 62, b = 38, a = 255 }}
default_stroke_width = 4
"#,
                save_dir.display()
            ),
        )
        .expect("legacy config");

        let loaded = AppConfig::load_from(&path).expect("load legacy config");

        assert_eq!(loaded.ocr_backend, OcrBackend::Tesseract);
        assert_eq!(loaded.ocr_languages, vec!["chi_sim".to_string(), "eng".to_string()]);
        assert!(loaded.ocr_filter_symbols);
    }

    #[test]
    fn appearance_mode_defaults_to_follow_system_for_new_and_legacy_config() {
        let config = AppConfig::default();
        assert_eq!(config.appearance_mode, AppearanceMode::System);

        let dir = tempdir().expect("tempdir");
        let save_dir = dir.path().join("shots");
        std::fs::create_dir_all(&save_dir).expect("save dir");
        let path = dir.path().join("legacy-appearance.toml");
        std::fs::write(
            &path,
            format!(
                r#"
default_save_dir = "{}"
filename_template = "Screenshot.png"
auto_copy = true
post_capture_open_editor = true
pin_after_save = false
default_tool = "Arrow"
default_color = {{ r = 232, g = 62, b = 38, a = 255 }}
default_stroke_width = 4
"#,
                save_dir.display()
            ),
        )
        .expect("legacy config");

        let loaded = AppConfig::load_from(&path).expect("load legacy config");

        assert_eq!(loaded.appearance_mode, AppearanceMode::System);
    }

    #[test]
    fn appearance_mode_round_trips_and_restore_defaults_returns_to_system() {
        let dir = tempdir().expect("tempdir");
        let save_dir = dir.path().join("shots");
        std::fs::create_dir_all(&save_dir).expect("save dir");
        let path = dir.path().join("appearance.toml");
        let mut config = AppConfig {
            default_save_dir: save_dir,
            appearance_mode: AppearanceMode::Dark,
            ..AppConfig::default()
        };

        config.save_to(&path).expect("save config");
        let loaded = AppConfig::load_from(&path).expect("load config");
        assert_eq!(loaded.appearance_mode, AppearanceMode::Dark);

        config.restore_defaults();
        assert_eq!(config.appearance_mode, AppearanceMode::System);
    }

    #[test]
    fn persistent_editor_state_round_trips() {
        let dir = tempdir().expect("tempdir");
        let save_dir = dir.path().join("shots");
        std::fs::create_dir_all(&save_dir).expect("save dir");
        let path = dir.path().join("config.toml");
        let config = AppConfig {
            default_save_dir: save_dir,
            recent_colors: vec![
                crate::document::Color::rgba(1, 2, 3, 255),
                crate::document::Color::rgba(4, 5, 6, 128),
            ],
            favorite_colors: vec![crate::document::Color::rgba(7, 8, 9, 255)],
            last_pin_scale: 1.75,
            last_pin_opacity: 0.65,
            ..AppConfig::default()
        };

        config.save_to(&path).expect("save config");
        let loaded = AppConfig::load_from(&path).expect("load config");

        assert_eq!(loaded.recent_colors, config.recent_colors);
        assert_eq!(loaded.favorite_colors, config.favorite_colors);
        assert_eq!(loaded.last_pin_scale, 1.75);
        assert_eq!(loaded.last_pin_opacity, 0.65);
    }

    #[test]
    fn legacy_config_without_editor_state_loads_with_defaults() {
        let dir = tempdir().expect("tempdir");
        let save_dir = dir.path().join("shots");
        std::fs::create_dir_all(&save_dir).expect("save dir");
        let path = dir.path().join("legacy-editor-state.toml");
        std::fs::write(
            &path,
            format!(
                r#"
default_save_dir = "{}"
filename_template = "Screenshot.png"
auto_copy = true
post_capture_open_editor = true
pin_after_save = false
default_tool = "Arrow"
default_color = {{ r = 232, g = 62, b = 38, a = 255 }}
default_stroke_width = 4
"#,
                save_dir.display()
            ),
        )
        .expect("legacy config");

        let loaded = AppConfig::load_from(&path).expect("load legacy config");

        assert!(loaded.recent_colors.is_empty());
        assert!(loaded.favorite_colors.is_empty());
        assert_eq!(loaded.last_pin_scale, 1.0);
        assert_eq!(loaded.last_pin_opacity, 1.0);
    }
}
