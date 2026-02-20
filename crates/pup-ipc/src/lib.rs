pub mod client;
pub mod protocol;

pub use client::IpcClient;
pub use protocol::{
    ClientMessage, HelloData, HistoryData, IpcEvent, SendMode, ServerMessage, ToolCall, Turn,
    TurnMessage,
};
