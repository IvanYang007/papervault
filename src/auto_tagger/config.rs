use serde::{Deserialize, Serialize};

/// Configuration for the auto-tagging feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoTagConfig {
    /// Whether auto-tagging is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Provider name (e.g., "deepseek").
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Model name (e.g., "deepseek-chat").
    #[serde(default = "default_model")]
    pub model: String,
    /// API endpoint URL.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// Environment variable name for the API key.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Maximum number of retries for failed API calls.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Request timeout in seconds.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_secs: u64,
    /// Maximum tags per document.
    #[serde(default = "default_max_tags")]
    pub max_tags_per_doc: usize,
    /// Maximum words of document text sent to the API (≈ one page).
    #[serde(default = "default_max_text_words")]
    pub max_text_words: usize,
    /// Whether to enable the model's thinking (chain-of-thought) mode.
    /// DeepSeek's `thinking` parameter defaults to enabled; tag extraction
    /// does not need reasoning, and thinking burned tokens (empty responses,
    /// latency, cost) — so the default is false (thinking disabled).
    #[serde(default)]
    pub thinking_enabled: bool,
    /// Output token budget. Generous headroom only matters when thinking is
    /// enabled (worst measured reasoning: ~14.6K tokens); with thinking
    /// disabled the output is just the tag JSON.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
}

fn default_provider() -> String {
    "deepseek".into()
}
fn default_model() -> String {
    "deepseek-v4-flash".into()
}
fn default_endpoint() -> String {
    "https://api.deepseek.com/v1/chat/completions".into()
}
fn default_api_key_env() -> String {
    "DEEPSEEK_API_KEY".into()
}
fn default_max_retries() -> u32 {
    3
}
fn default_request_timeout() -> u64 {
    // LLM generation on a loaded origin regularly takes 30-60s (and a
    // reasoning model with a large token budget can take minutes); the
    // old 30s read timeout killed real requests (Windows reports the
    // read timeout as os error 10060, which looked like a network outage).
    240
}
fn default_max_tags() -> usize {
    5
}
fn default_max_tokens() -> usize {
    // Worst measured reasoning: ~14.6K tokens on a 500-word document.
    24000
}
fn default_max_text_words() -> usize {
    // ≈ one page of a typical letter: enough for the AI to understand the
    // topic, small enough to keep calls fast and cheap.
    500
}

impl Default for AutoTagConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_provider(),
            model: default_model(),
            endpoint: default_endpoint(),
            api_key_env: default_api_key_env(),
            max_retries: default_max_retries(),
            request_timeout_secs: default_request_timeout(),
            max_tags_per_doc: default_max_tags(),
            max_text_words: default_max_text_words(),
            thinking_enabled: false,
            max_tokens: default_max_tokens(),
        }
    }
}

impl AutoTagConfig {
    /// Returns the path to the auto-tag config file.
    fn config_path() -> std::path::PathBuf {
        let base = dirs_next::config_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        base.join("papervault").join("auto_tag.json")
    }

    /// Load config from disk, returning default if the file doesn't exist.
    pub fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => match serde_json::from_str(&contents) {
                    Ok(config) => config,
                    Err(e) => {
                        tracing::warn!("auto_tag.json parse error, using defaults: {}", e);
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
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, contents)?;
        std::fs::rename(&tmp_path, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_default_when_no_file() {
        let config = AutoTagConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.provider, "deepseek");
        assert_eq!(config.model, "deepseek-v4-flash");
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auto_tag.json");

        // Override config path for testing
        let config = AutoTagConfig {
            enabled: true,
            model: "deepseek-flash".into(),
            max_retries: 5,
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        std::fs::write(&path, &json).unwrap();

        let loaded: AutoTagConfig = serde_json::from_str(&json).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.model, "deepseek-flash");
        assert_eq!(loaded.max_retries, 5);
    }

    #[test]
    fn load_malformed_json_returns_default() {
        let result: Result<AutoTagConfig, _> = serde_json::from_str("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn api_key_env_name_stored_not_value() {
        let config = AutoTagConfig::default();
        assert_eq!(config.api_key_env, "DEEPSEEK_API_KEY");
        // The actual key value is never in the config
    }

    #[test]
    fn default_is_disabled() {
        let config = AutoTagConfig::default();
        assert!(!config.enabled);
    }
}
