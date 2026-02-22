use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::debug;

/// Top-level configuration file structure.
#[derive(Debug, Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub pup: PupConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub backends: BackendsConfig,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PupConfig {
    #[serde(default = "default_socket_dir")]
    pub socket_dir: String,
}

impl Default for PupConfig {
    fn default() -> Self {
        Self {
            socket_dir: default_socket_dir(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DisplayConfig {
    #[serde(default)]
    pub verbose: bool,
    #[serde(default = "default_history_turns")]
    pub history_turns: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            history_turns: 5,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamingConfig {
    #[serde(default = "default_edit_interval_ms")]
    pub edit_interval_ms: u64,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            edit_interval_ms: 1500,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct BackendsConfig {
    #[serde(default)]
    pub telegram: Option<TelegramBackendConfig>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramBackendConfig {
    #[serde(default)]
    pub enabled: bool,
    pub bot_token: String,
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    #[serde(default)]
    pub dm: Option<TelegramDmConfig>,
    #[serde(default)]
    pub topics: Option<TelegramTopicsConfig>,
    #[serde(default)]
    pub display: Option<TelegramDisplayConfig>,
    /// Enable local voice-to-text via whisper.cpp (default: true).
    #[serde(default = "default_true")]
    pub voice: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramDmConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramTopicsConfig {
    #[serde(default)]
    pub enabled: bool,
    pub supergroup_id: Option<i64>,
    #[serde(default = "default_topic_icon")]
    pub topic_icon: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramDisplayConfig {
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,
}

fn default_socket_dir() -> String {
    "~/.pi/pup".to_owned()
}

fn default_history_turns() -> usize {
    5
}

fn default_edit_interval_ms() -> u64 {
    1500
}

fn default_true() -> bool {
    true
}

fn default_topic_icon() -> String {
    "📎".to_owned()
}

fn default_max_message_length() -> usize {
    3500
}

impl Config {
    /// Load configuration from the default path or a specified path.
    pub(crate) fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_owned(),
            None => default_config_path()?,
        };

        if !config_path.exists() {
            anyhow::bail!(
                "Config not found at {}. Run `pup setup` to create one.",
                config_path.display()
            );
        }

        debug!(path = %config_path.display(), "loading config");
        let content =
            std::fs::read_to_string(&config_path).with_context(|| {
                format!("failed to read {}", config_path.display())
            })?;

        let config: Self = toml::from_str(&content).with_context(|| {
            format!("failed to parse {}", config_path.display())
        })?;

        Ok(config)
    }

    /// Resolve the socket directory path, expanding `~`.
    pub(crate) fn socket_dir(&self) -> PathBuf {
        expand_tilde(&self.pup.socket_dir)
    }

    /// Build a `TelegramConfig` from the loaded config.
    pub(crate) fn telegram_config(&self) -> Option<pup_telegram::TelegramConfig> {
        let tg = self.backends.telegram.as_ref()?;
        if !tg.enabled {
            return None;
        }

        let dm_enabled = tg.dm.as_ref().is_none_or(|d| d.enabled);
        let topics_enabled = tg.topics.as_ref().is_some_and(|t| t.enabled);
        let supergroup_id = tg.topics.as_ref().and_then(|t| t.supergroup_id);
        let topic_icon = tg
            .topics
            .as_ref().map_or_else(default_topic_icon, |t| t.topic_icon.clone());
        let max_message_length = tg
            .display
            .as_ref()
            .map_or(default_max_message_length(), |d| d.max_message_length);

        let socket_dir = self.socket_dir();

        Some(pup_telegram::TelegramConfig {
            bot_token: tg.bot_token.clone(),
            allowed_user_ids: tg.allowed_user_ids.clone(),
            dm_enabled,
            topics_enabled,
            supergroup_id,
            topic_icon,
            max_message_length,
            edit_interval_ms: self.streaming.edit_interval_ms,
            verbose: self.display.verbose,
            history_turns: self.display.history_turns,
            topics_state_path: socket_dir.join("topics_state.json"),
            socket_dir,
            voice: tg.voice,
        })
    }
}

/// Get the default config file path: `~/.config/pup/config.toml`.
pub(crate) fn default_config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .context("could not determine config directory")?
        .join("pup");
    Ok(config_dir.join("config.toml"))
}

/// Expand `~` in a path string to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        let tg = config.backends.telegram.expect("telegram");
        assert!(tg.enabled);
        assert_eq!(tg.bot_token, "123456:ABC");
        assert_eq!(tg.allowed_user_ids, vec![12345678]);
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[pup]
socket_dir = "~/.pi/pup"

[display]
verbose = false
history_turns = 5

[streaming]
edit_interval_ms = 1500

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]

[backends.telegram.dm]
enabled = true

[backends.telegram.topics]
enabled = true
supergroup_id = -1001234567890
topic_icon = "📎"

[backends.telegram.display]
max_message_length = 3500
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.pup.socket_dir, "~/.pi/pup");
        assert_eq!(config.streaming.edit_interval_ms, 1500);
        let tg = config.telegram_config().expect("telegram config");
        assert!(tg.dm_enabled);
        assert!(tg.topics_enabled);
        assert_eq!(tg.supergroup_id, Some(-1001234567890));
    }

    #[test]
    fn test_expand_tilde() {
        let path = expand_tilde("~/.pi/pup");
        // Should start with the home dir, not ~.
        assert!(!path.to_str().unwrap_or("").starts_with('~'));
    }
}
