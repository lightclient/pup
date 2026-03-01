use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{Instrument, info, info_span, warn};

use crate::types::{IncomingMessage, SessionEvent};

/// Shared set of session IDs owned by an agent backend.
///
/// Agent backends (pi session manager, Claude Code service, …) register
/// sessions here when they connect and remove them when they disconnect.
/// The [`MessageRouter`] checks these registries to dispatch incoming
/// messages to the correct backend.
pub type SessionRegistry = Arc<RwLock<HashSet<String>>>;

/// Create a new empty session registry.
pub fn new_registry() -> SessionRegistry {
    Arc::new(RwLock::new(HashSet::new()))
}

/// Handle for routing messages to a specific agent backend.
///
/// Each agent backend registers one of these with the [`MessageRouter`].
#[derive(Debug)]
pub struct AgentHandle {
    /// Human-readable name for logging.
    pub name: &'static str,
    /// Channel to send messages to this agent's sessions.
    pub message_tx: mpsc::Sender<IncomingMessage>,
    /// Registry of session IDs this agent owns.
    pub registry: SessionRegistry,
    /// If true, this agent receives messages for unknown sessions (fallback).
    pub is_default: bool,
}

impl AgentHandle {
    /// Check if this agent owns the given session ID.
    pub fn owns_session(&self, session_id: &str) -> bool {
        self.registry
            .read()
            .ok()
            .is_some_and(|set| set.contains(session_id))
    }
}

/// Fans out session events from agent backends to all chat channels.
///
/// Agent backends push events via [`EventBus::sender`]. The bus copies
/// each event to every subscribed chat channel receiver.
#[derive(Debug)]
pub struct EventBus {
    /// Sender that agent backends clone to push events.
    tx: mpsc::Sender<SessionEvent>,
    /// One sender per subscribed chat channel.
    channel_txs: Vec<mpsc::Sender<SessionEvent>>,
    /// Receiver for agent-pushed events (consumed by [`EventBus::run`]).
    rx: Option<mpsc::Receiver<SessionEvent>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    /// Create a new event bus.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            tx,
            channel_txs: Vec::new(),
            rx: Some(rx),
        }
    }

    /// Get a sender for agent backends to push events through.
    ///
    /// Clone this and pass it to each agent backend.
    pub fn sender(&self) -> mpsc::Sender<SessionEvent> {
        self.tx.clone()
    }

    /// Subscribe a chat channel. Returns the event receiver the channel
    /// should read from.
    pub fn subscribe(&mut self) -> mpsc::Receiver<SessionEvent> {
        let (tx, rx) = mpsc::channel(256);
        self.channel_txs.push(tx);
        rx
    }

    /// Run the fan-out loop. Reads events from agent backends and copies
    /// them to all subscribed chat channels.
    ///
    /// Returns when all agent senders are dropped (no more events).
    pub async fn run(mut self) {
        let mut rx = self
            .rx
            .take()
            .expect("EventBus::run must only be called once");

        let span = info_span!("event_bus");
        async {
            info!(subscribers = self.channel_txs.len(), "event bus started");
            while let Some(event) = rx.recv().await {
                for tx in &self.channel_txs {
                    if tx.send(event.clone()).await.is_err() {
                        warn!("event bus: subscriber channel closed");
                    }
                }
            }
            info!("event bus stopped");
        }
        .instrument(span)
        .await;
    }
}

/// Routes incoming messages from chat channels to agent backends.
///
/// When a user sends a message via a chat channel (e.g. Telegram), the
/// router checks each agent backend's [`SessionRegistry`] to find the
/// owner and forwards the message. If no agent claims the session, the
/// message goes to the default agent (typically pi).
///
/// # Adding a new agent backend
///
/// 1. Create an [`AgentHandle`] with a [`SessionRegistry`] and message channel
/// 2. Call [`MessageRouter::add_agent`] before starting the router
/// 3. The router will automatically dispatch messages to your backend
#[derive(Debug)]
pub struct MessageRouter {
    agents: Vec<AgentHandle>,
}

impl Default for MessageRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageRouter {
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// Register an agent backend with the router.
    ///
    /// The first agent added with `is_default: true` is the fallback for
    /// messages that no agent claims.
    pub fn add_agent(&mut self, handle: AgentHandle) {
        self.agents.push(handle);
    }

    /// Route a single message to the appropriate agent.
    async fn route(&self, msg: IncomingMessage) {
        let session_id = msg.session_id().to_owned();

        // Find the agent that owns this session.
        for agent in &self.agents {
            if agent.owns_session(&session_id) {
                if agent.message_tx.send(msg).await.is_err() {
                    warn!(agent = agent.name, session_id, "agent channel closed");
                }
                return;
            }
        }

        // No agent claims this session — use the default.
        if let Some(agent) = self.agents.iter().find(|a| a.is_default) {
            if agent.message_tx.send(msg).await.is_err() {
                warn!(
                    agent = agent.name,
                    session_id, "default agent channel closed"
                );
            }
        } else {
            warn!(session_id, "no agent owns session and no default agent");
        }
    }

    /// Run the router, consuming messages until the receiver closes.
    pub async fn run(self, mut message_rx: mpsc::Receiver<IncomingMessage>) {
        let span = info_span!("message_router");
        async {
            info!(agents = self.agents.len(), "message router started");
            while let Some(msg) = message_rx.recv().await {
                self.route(msg).await;
            }
            info!("message router stopped");
        }
        .instrument(span)
        .await;
    }
}
