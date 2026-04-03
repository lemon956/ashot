use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::document::{Color, DefaultTool};

const CONFIG_DIR_NAME: &str = "ashot";
const CONFIG_FILE_NAME: &str = "config.toml";

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub default_save_dir: PathBuf,
    pub filename_template: String,
    pub auto_copy: bool,
    pub post_capture_open_editor: bool,
    pub pin_after_save: bool,
    pub default_tool: DefaultTool,
    pub default_color: Color,
    pub default_stroke_width: u32,
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::AppConfig;

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
}
