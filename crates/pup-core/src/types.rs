use pup_ipc::{Turn, SendMode};
use serde::{Deserialize, Serialize};

/// Events the session manager pushes to backends.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A new pi session was discovered and connected.
    Connected { info: SessionInfo },
    /// A pi session disconnected (exited).
    Disconnected { session_id: String },
    /// Session metadata changed (name, model, etc).
    InfoChanged { info: SessionInfo },
    /// Agent started processing a prompt.
    AgentStart { session_id: String },
    /// Agent finished processing.
    AgentEnd { session_id: String },
    /// A new assistant message began streaming.
    MessageStart {
        session_id: String,
        message_id: String,
    },
    /// Streaming text delta for an in-progress assistant message.
    MessageDelta {
        session_id: String,
        message_id: String,
        text: String,
    },
    /// An assistant message finished.
    MessageEnd {
        session_id: String,
        message_id: String,
        content: String,
    },
    /// A tool started executing.
    ToolStart {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// A tool finished executing.
    ToolEnd {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    /// The pi session was reset (/new or /compact). Same process, new conversation.
    SessionReset { session_id: String },
    /// A user message was sent (from pi TUI or another backend).
    /// `echo` is true if this message originated from pup (via IPC send command).
    UserMessage {
        session_id: String,
        content: String,
        echo: bool,
        source: MessageSource,
    },
}

/// Where a user message originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// Typed in the pi TUI.
    Interactive,
    /// Sent via pup IPC (from some backend).
    Extension,
}

impl MessageSource {
    pub fn from_str(s: &str) -> Self {
        match s {
            "extension" => Self::Extension,
            _ => Self::Interactive,
        }
    }
}

/// Info about a connected pi session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    pub session_name: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
    pub history: Vec<Turn>,
    pub streaming: bool,
    pub partial_text: Option<String>,
}

/// A message from a chat backend directed at a pi session.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub session_id: String,
    pub text: String,
    pub mode: SendMode,
    /// If true, this is a cancel/abort request rather than a message.
    pub is_cancel: bool,
}

/// Discovery events for the session manager.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A new socket was found.
    SocketAppeared { session_id: String, path: std::path::PathBuf },
    /// A socket was removed.
    SocketRemoved { session_id: String },
}
