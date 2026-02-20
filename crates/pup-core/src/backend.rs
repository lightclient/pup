use anyhow::Result;
use async_trait::async_trait;

use crate::types::SessionEvent;

/// What chat backends implement.
///
/// Backends are compiled in (not dynamically loaded). The daemon's main.rs
/// instantiates the configured backends and passes them to the session manager.
///
/// Each backend runs in its own tokio task. Communication between the session
/// manager and backends uses `tokio::sync::mpsc` channels.
#[async_trait]
pub trait ChatBackend: Send + 'static {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Called once at startup after config is loaded.
    async fn init(&mut self) -> Result<()>;

    /// Receive and handle a session event. The session manager calls this for
    /// each event. Heavy work (API calls) should be spawned or queued internally.
    async fn handle_event(&mut self, event: SessionEvent) -> Result<()>;

    /// Receive incoming messages from the chat platform.
    ///
    /// This is called in a loop by the backend's incoming-poller task.
    /// Returns `None` if the backend has shut down.
    async fn recv_incoming(&mut self) -> Result<Option<crate::types::IncomingMessage>>;

    /// Graceful shutdown.
    async fn shutdown(&mut self) -> Result<()>;
}
