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
    /// How many tool calls to keep in the rendered message.
    /// A number (e.g. 3) keeps only the last N. The string "all" keeps all.
    /// Default: 3.
    #[serde(default = "default_tool_calls")]
    pub tool_calls: ToolCallsValue,
    /// How many lines of tool output to show per tool call.
    /// A number (e.g. 10) shows the first N lines. The string "all" shows all.
    /// Default: 10.
    #[serde(default = "default_tool_output_lines")]
    pub tool_output_lines: ToolOutputLinesValue,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            history_turns: 5,
            tool_calls: default_tool_calls(),
            tool_output_lines: default_tool_output_lines(),
        }
    }
}

/// Represents the `tool_calls` config value: either a number or "all".
#[derive(Debug, Clone)]
pub(crate) enum ToolCallsValue {
    Last(usize),
    All,
}

impl<'de> Deserialize<'de> for ToolCallsValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct ToolCallsVisitor;

        impl de::Visitor<'_> for ToolCallsVisitor {
            type Value = ToolCallsValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a positive integer or the string \"all\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                #[allow(clippy::cast_possible_truncation)]
                Ok(ToolCallsValue::Last(v as usize))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(de::Error::custom("tool_calls must be non-negative"));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Ok(ToolCallsValue::Last(v as usize))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.eq_ignore_ascii_case("all") {
                    Ok(ToolCallsValue::All)
                } else {
                    Err(de::Error::custom(
                        "expected a number or \"all\" for tool_calls",
                    ))
                }
            }
        }

        deserializer.deserialize_any(ToolCallsVisitor)
    }
}

fn default_tool_calls() -> ToolCallsValue {
    ToolCallsValue::Last(3)
}

/// Represents the `tool_output_lines` config value: either a number or "all".
#[derive(Debug, Clone)]
pub(crate) enum ToolOutputLinesValue {
    First(usize),
    All,
}

impl<'de> Deserialize<'de> for ToolOutputLinesValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;

        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = ToolOutputLinesValue;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a non-negative integer or the string \"all\"")
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                #[allow(clippy::cast_possible_truncation)]
                Ok(ToolOutputLinesValue::First(v as usize))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(de::Error::custom("tool_output_lines must be non-negative"));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                Ok(ToolOutputLinesValue::First(v as usize))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.eq_ignore_ascii_case("all") {
                    Ok(ToolOutputLinesValue::All)
                } else {
                    Err(de::Error::custom(
                        "expected a number or \"all\" for tool_output_lines",
                    ))
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn default_tool_output_lines() -> ToolOutputLinesValue {
    ToolOutputLinesValue::First(10)
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
            .map_or_else(default_max_message_length, |d| d.max_message_length);

        let socket_dir = self.socket_dir();

        let tool_call_limit = match &self.display.tool_calls {
            ToolCallsValue::All => pup_telegram::turn_tracker::ToolCallLimit::All,
            ToolCallsValue::Last(n) => pup_telegram::turn_tracker::ToolCallLimit::Last(*n),
        };

        let tool_output_lines = match &self.display.tool_output_lines {
            ToolOutputLinesValue::All => pup_telegram::turn_tracker::ToolOutputLines::All,
            ToolOutputLinesValue::First(n) => pup_telegram::turn_tracker::ToolOutputLines::First(*n),
        };

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
            tool_call_limit,
            tool_output_lines,
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
#[allow(clippy::unreadable_literal)]
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

    #[test]
    fn test_tool_calls_default() {
        let toml = r#"
[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(config.display.tool_calls, ToolCallsValue::Last(3)));
    }

    #[test]
    fn test_tool_calls_number() {
        let toml = r#"
[display]
tool_calls = 5

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(config.display.tool_calls, ToolCallsValue::Last(5)));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(tg.tool_call_limit, pup_telegram::turn_tracker::ToolCallLimit::Last(5));
    }

    #[test]
    fn test_tool_calls_all() {
        let toml = r#"
[display]
tool_calls = "all"

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(config.display.tool_calls, ToolCallsValue::All));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(tg.tool_call_limit, pup_telegram::turn_tracker::ToolCallLimit::All);
    }

    #[test]
    fn test_tool_output_lines_default() {
        let toml = r#"
[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(
            config.display.tool_output_lines,
            ToolOutputLinesValue::First(10)
        ));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(
            tg.tool_output_lines,
            pup_telegram::turn_tracker::ToolOutputLines::First(10)
        );
    }

    #[test]
    fn test_tool_output_lines_number() {
        let toml = r#"
[display]
tool_output_lines = 5

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(
            config.display.tool_output_lines,
            ToolOutputLinesValue::First(5)
        ));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(
            tg.tool_output_lines,
            pup_telegram::turn_tracker::ToolOutputLines::First(5)
        );
    }

    #[test]
    fn test_tool_output_lines_all() {
        let toml = r#"
[display]
tool_output_lines = "all"

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(
            config.display.tool_output_lines,
            ToolOutputLinesValue::All
        ));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(
            tg.tool_output_lines,
            pup_telegram::turn_tracker::ToolOutputLines::All
        );
    }

    #[test]
    fn test_tool_output_lines_zero() {
        let toml = r#"
[display]
tool_output_lines = 0

[backends.telegram]
enabled = true
bot_token = "123456:ABC"
allowed_user_ids = [12345678]
"#;
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(matches!(
            config.display.tool_output_lines,
            ToolOutputLinesValue::First(0)
        ));
        let tg = config.telegram_config().expect("telegram config");
        assert_eq!(
            tg.tool_output_lines,
            pup_telegram::turn_tracker::ToolOutputLines::First(0)
        );
    }
}
