use serde::{Deserialize, Serialize};

// ── Client → Server (commands) ──────────────────────────────────────────────

/// Messages sent from the daemon to the extension over IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Send a user message to the pi session.
    Send {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<SendMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Abort the current agent operation.
    Abort {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Request current session info.
    GetInfo {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Request session history.
    GetHistory {
        #[serde(skip_serializing_if = "Option::is_none")]
        turns: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
}

/// Delivery mode for user messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    Steer,
    FollowUp,
}

// ── Server → Client (events + responses) ────────────────────────────────────

/// Messages sent from the extension to the daemon over IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// A session event.
    Event {
        event: String,
        data: serde_json::Value,
    },
    /// A response to a client command.
    Response {
        command: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

// ── Parsed event types ──────────────────────────────────────────────────────

/// Strongly-typed representation of events parsed from `ServerMessage::Event`.
#[derive(Debug, Clone)]
pub enum IpcEvent {
    Hello(HelloData),
    History(HistoryData),
    AgentStart,
    AgentEnd,
    TurnStart {
        turn_index: u64,
    },
    TurnEnd {
        turn_index: u64,
    },
    MessageStart {
        role: String,
        message_id: String,
    },
    MessageDelta {
        message_id: String,
        text: String,
    },
    ThinkingDelta {
        message_id: String,
        text: String,
    },
    MessageEnd {
        message_id: String,
        role: String,
        content: String,
    },
    ToolStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolUpdate {
        tool_call_id: String,
        tool_name: String,
        content: String,
    },
    ToolEnd {
        tool_call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    SessionNameChanged {
        name: String,
    },
    ModelChanged {
        model: String,
    },
    UserMessage {
        content: String,
        source: String,
        echo: bool,
    },
    SessionEnd,
    /// The pi session was reset (/new or /compact). Same process, new conversation.
    SessionReset,
    /// A notification from the extension (command errors, status messages, etc).
    Notification {
        text: String,
    },
    /// An event type we don't specifically handle.
    Unknown {
        event: String,
        data: serde_json::Value,
    },
}

/// Data sent in the `hello` event on connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloData {
    pub session_id: String,
    #[serde(default)]
    pub session_name: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub thinking_level: Option<String>,
}

/// Data sent in the `history` event on connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryData {
    pub turns: Vec<Turn>,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub partial_text: Option<String>,
}

/// A single conversation turn (user prompt + assistant response + tool calls).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub user: Option<TurnMessage>,
    pub assistant: Option<TurnMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

/// A user or assistant message within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMessage {
    pub content: String,
    /// Timestamp — accepts both integer (epoch ms) and ISO 8601 string.
    #[serde(deserialize_with = "deserialize_flexible_timestamp")]
    pub timestamp: u64,
}

/// Deserialize a timestamp that may be either a u64 (epoch ms) or an ISO 8601 string.
fn deserialize_flexible_timestamp<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct TimestampVisitor;

    impl de::Visitor<'_> for TimestampVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a u64 epoch-ms or an ISO 8601 date string")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            Ok(v.cast_unsigned())
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<u64, E> {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Ok(v as u64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            // Try parsing as ISO 8601 (e.g. "2026-02-21T15:11:53.270Z").
            // Fall back to treating it as a numeric string.
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(v) {
                Ok(dt.timestamp_millis().cast_unsigned())
            } else if let Ok(n) = v.parse::<u64>() {
                Ok(n)
            } else {
                Err(de::Error::custom(format!("unrecognised timestamp: {v}")))
            }
        }
    }

    deserializer.deserialize_any(TimestampVisitor)
}

/// A tool call within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

impl IpcEvent {
    /// Parse a raw `ServerMessage::Event` into a typed `IpcEvent`.
    #[allow(clippy::too_many_lines)]
    pub fn parse(event: &str, data: &serde_json::Value) -> Self {
        match event {
            "hello" => match serde_json::from_value(data.clone()) {
                Ok(hello) => Self::Hello(hello),
                Err(_) => Self::Unknown {
                    event: event.to_owned(),
                    data: data.clone(),
                },
            },
            "history" => match serde_json::from_value(data.clone()) {
                Ok(history) => Self::History(history),
                Err(_) => Self::Unknown {
                    event: event.to_owned(),
                    data: data.clone(),
                },
            },
            "agent_start" => Self::AgentStart,
            "agent_end" => Self::AgentEnd,
            "turn_start" => Self::TurnStart {
                turn_index: data
                    .get("turn_index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            },
            "turn_end" => Self::TurnEnd {
                turn_index: data
                    .get("turn_index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            },
            "message_start" => Self::MessageStart {
                role: data
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                message_id: data
                    .get("message_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "message_delta" => Self::MessageDelta {
                message_id: data
                    .get("message_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                text: data
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "thinking_delta" => Self::ThinkingDelta {
                message_id: data
                    .get("message_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                text: data
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "message_end" => Self::MessageEnd {
                message_id: data
                    .get("message_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                role: data
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                content: data
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "tool_start" => Self::ToolStart {
                tool_call_id: data
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                tool_name: data
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                args: data.get("args").cloned().unwrap_or(serde_json::Value::Null),
            },
            "tool_update" => Self::ToolUpdate {
                tool_call_id: data
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                tool_name: data
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                content: data
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "tool_end" => Self::ToolEnd {
                tool_call_id: data
                    .get("tool_call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                tool_name: data
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                content: data
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                is_error: data
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            },
            "session_name_changed" => Self::SessionNameChanged {
                name: data
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "model_changed" => Self::ModelChanged {
                model: data
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            "user_message" => Self::UserMessage {
                content: data
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                source: data
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("interactive")
                    .to_owned(),
                echo: data
                    .get("echo")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            },
            "session_end" => Self::SessionEnd,
            "session_reset" => Self::SessionReset,
            "notification" => Self::Notification {
                text: data
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            },
            _ => Self::Unknown {
                event: event.to_owned(),
                data: data.clone(),
            },
        }
    }
}
