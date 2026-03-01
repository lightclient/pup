use std::future::Future;

use tokio::sync::{mpsc, watch};

use crate::types::{IncomingMessage, SessionEvent};

/// A chat channel bridges agent sessions to a messaging platform (Telegram, Discord, etc.).
///
/// Implementations receive session events (agent output) and produce
/// incoming messages (user input) via channels. The daemon wires up
/// the channels at startup — implementations just run their event loop.
///
/// # Adding a new chat channel
///
/// 1. Create a new crate (e.g. `pup-discord`)
/// 2. Implement `ChatChannel` for your backend struct
/// 3. Wire it up in `pup-daemon/src/main.rs`
///
/// The daemon calls [`ChatChannel::run`] in a spawned task. The channel
/// runs until `shutdown_rx` signals or `event_rx` closes.
pub trait ChatChannel: Send + 'static {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Run the channel until shutdown.
    ///
    /// - `event_rx`: session events from agent backends (pi, Claude Code, …)
    /// - `message_tx`: user messages to route back to agent backends
    /// - `shutdown_rx`: daemon shutdown signal
    fn run(
        self,
        event_rx: mpsc::Receiver<SessionEvent>,
        message_tx: mpsc::Sender<IncomingMessage>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> impl Future<Output = ()> + Send;
}
