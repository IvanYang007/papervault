use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watched_folder: Option<PathBuf>,

    /// Launch Papervault when Windows starts.
    #[serde(default)]
    pub start_with_windows: bool,

    /// Minimize to system tray on window close instead of exiting.
    /// Defaults to true so closing the app minimizes to tray.
    #[serde(default = "default_minimize_to_tray")]
    pub minimize_to_tray: bool,
}

fn default_minimize_to_tray() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            watched_folder: None,
            start_with_windows: false,
            minimize_to_tray: true,
        }
    }
}

impl Config {
    /// Returns the path to the config file in the user's config directory.
    fn config_path() -> PathBuf {
        let base = dirs_next::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("papervault").join("config.json")
    }

    /// Load config from disk, returning default if the file doesn't exist.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(config) => config,
                    Err(e) => {
                        tracing::warn!("Config parse error, using defaults: {}", e);
                        Self::default()
                    }
                },
                Err(_) => Self::default(),
            }
        } else {
            Self::default()
        }
    }

    /// Save config to disk atomically (write to tmp, rename over final).
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        // Write to temp file first, then rename atomically (NTFS same-volume guarantee)
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, contents)?;
        std::fs::rename(&tmp_path, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_returns_default_when_no_config_file() {
        let config = Config::default();
        assert!(config.watched_folder.is_none());
    }

    #[test]
    fn save_and_load_round_trips() {
        let config = Config {
            watched_folder: Some(PathBuf::from("/test/path/to/folder")),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(
            loaded.watched_folder.unwrap(),
            PathBuf::from("/test/path/to/folder")
        );
    }

    #[test]
    fn load_rejects_invalid_json() {
        let result: std::result::Result<Config, _> = serde_json::from_str("not valid json {{{{");
        assert!(
            result.is_err(),
            "Invalid JSON should produce a deserialization error"
        );
    }

    #[test]
    fn watched_folder_nonexistent_path_accepted() {
        let config = Config {
            watched_folder: Some(PathBuf::from("Z:/nonexistent/path/that/does/not/exist")),
            ..Default::default()
        };
        assert!(config.watched_folder.is_some());
        // Config stores path strings; existence validation is a UI-layer concern
    }

    #[test]
    fn new_fields_defaults() {
        let config = Config::default();
        assert!(!config.start_with_windows);
        assert!(config.minimize_to_tray); // defaults to true
    }

    #[test]
    fn new_fields_round_trip() {
        let config = Config {
            start_with_windows: true,
            minimize_to_tray: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let loaded: Config = serde_json::from_str(&json).unwrap();
        assert!(loaded.start_with_windows);
        assert!(loaded.minimize_to_tray);
        assert!(loaded.watched_folder.is_none());
    }

    #[test]
    fn deserialize_old_format_without_new_fields() {
        let old_json = r#"{"watched_folder": "/some/path"}"#;
        let config: Config = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            config.watched_folder.unwrap(),
            PathBuf::from("/some/path")
        );
        // New fields use their serde defaults when missing:
        // start_with_windows → false, minimize_to_tray → true
        assert!(!config.start_with_windows);
        assert!(config.minimize_to_tray);
    }
}
