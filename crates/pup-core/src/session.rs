use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use pup_ipc::{ClientMessage, IpcClient, IpcEvent, ServerMessage};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::discovery::Discovery;
use crate::router::SessionRegistry;
use crate::types::{DiscoveryEvent, IncomingMessage, MessageSource, SessionEvent, SessionInfo};

/// Internal message from per-session IPC reader tasks.
#[derive(Debug)]
enum IpcReaderMsg {
    Event {
        session_id: String,
        event: IpcEvent,
    },
    Disconnected {
        session_id: String,
        error: Option<String>,
    },
}

/// Handle to a connected IPC session for sending commands.
#[derive(Debug)]
struct SessionConnection {
    info: SessionInfo,
    cmd_tx: mpsc::Sender<ClientMessage>,
}

/// The session manager is the hub that connects IPC sessions to chat channels.
///
/// It runs the discovery loop, owns all IPC connections, reads events from each,
/// publishes them to the [`EventBus`](crate::router::EventBus), and routes
/// incoming messages from chat channels to the correct IPC connection.
#[derive(Debug)]
pub struct SessionManager {
    socket_dir: PathBuf,
    sessions: HashMap<String, SessionConnection>,
    /// Registry of connected session IDs (shared with the message router).
    registry: SessionRegistry,
    /// Sender to push `SessionEvent`s to the event bus.
    event_tx: mpsc::Sender<SessionEvent>,
    /// Receiver for incoming messages routed to this backend.
    incoming_rx: mpsc::Receiver<IncomingMessage>,
    /// Receiver for IPC reader messages.
    ipc_rx: mpsc::Receiver<IpcReaderMsg>,
    ipc_tx: mpsc::Sender<IpcReaderMsg>,
    /// Discovery events.
    discovery_rx: mpsc::Receiver<DiscoveryEvent>,
    discovery_tx: mpsc::Sender<DiscoveryEvent>,
}

impl SessionManager {
    /// Create a new session manager.
    ///
    /// - `socket_dir`: path to `~/.pi/pup/`
    /// - `event_tx`: sender to the event bus (fan-out to chat channels)
    /// - `incoming_rx`: messages routed to this backend by the message router
    /// - `registry`: shared session registry for the message router
    pub fn new(
        socket_dir: PathBuf,
        event_tx: mpsc::Sender<SessionEvent>,
        incoming_rx: mpsc::Receiver<IncomingMessage>,
        registry: SessionRegistry,
    ) -> Self {
        let (ipc_tx, ipc_rx) = mpsc::channel(256);
        let (discovery_tx, discovery_rx) = mpsc::channel(64);
        Self {
            socket_dir,
            sessions: HashMap::new(),
            registry,
            event_tx,
            incoming_rx,
            ipc_rx,
            ipc_tx,
            discovery_rx,
            discovery_tx,
        }
    }

    /// Run the session manager. This is the main select loop.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let span = info_span!("session_manager", socket_dir = %self.socket_dir.display());
        async {
            info!("starting session manager");

            // Spawn discovery loop.
            let discovery = Discovery::new(self.socket_dir.clone(), self.discovery_tx.clone());
            tokio::spawn(async move {
                if let Err(e) = discovery.run().await {
                    error!(error = %e, "discovery loop failed");
                }
            });

            loop {
                tokio::select! {
                    // Shutdown signal.
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("shutdown signal received");
                            break;
                        }
                    }

                    // Discovery events: new or removed sockets.
                    Some(event) = self.discovery_rx.recv() => {
                        match event {
                            DiscoveryEvent::SocketAppeared { session_id, path } => {
                                if let Err(e) = self.connect_session(&session_id, &path).await {
                                    warn!(session_id, error = %e, "failed to connect to session");
                                }
                            }
                            DiscoveryEvent::SocketRemoved { session_id } => {
                                self.disconnect_session(&session_id, "socket removed").await;
                            }
                        }
                    }

                    // IPC reader events from connected sessions.
                    Some(msg) = self.ipc_rx.recv() => {
                        match msg {
                            IpcReaderMsg::Event { session_id, event } => {
                                self.handle_ipc_event(&session_id, event).await;
                            }
                            IpcReaderMsg::Disconnected { session_id, error } => {
                                let reason = error.as_deref().unwrap_or("connection closed");
                                self.disconnect_session(&session_id, reason).await;
                            }
                        }
                    }

                    // Incoming messages from chat channels (via the router).
                    Some(msg) = self.incoming_rx.recv() => {
                        self.route_incoming(msg).await;
                    }
                }
            }

            // Graceful shutdown: drop all sessions.
            let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
            for sid in session_ids {
                self.disconnect_session(&sid, "daemon shutting down").await;
            }

            info!("session manager stopped");
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Connect to a session's IPC socket.
    async fn connect_session(&mut self, session_id: &str, path: &std::path::Path) -> Result<()> {
        if self.sessions.contains_key(session_id) {
            debug!(session_id, "already connected, skipping");
            return Ok(());
        }

        info!(session_id, path = %path.display(), "connecting to session");

        let mut client = IpcClient::connect(path)
            .await
            .context("IPC connect failed")?;

        // Read hello + history events to build SessionInfo.
        let mut info = SessionInfo {
            session_id: session_id.to_owned(),
            session_name: None,
            cwd: String::new(),
            model: None,
            history: Vec::new(),
            streaming: false,
            partial_text: None,
        };

        // Read the hello event (should be first).
        if let Some(msg) = client.recv().await?
            && let ServerMessage::Event { event, data } = &msg
        {
            let parsed = IpcEvent::parse(event, data);
            if let IpcEvent::Hello(hello) = parsed {
                info.session_id = hello.session_id;
                info.session_name = hello.session_name;
                info.cwd = hello.cwd;
                info.model = hello.model;
            }
        }

        // Read the history event (should be second).
        if let Some(msg) = client.recv().await?
            && let ServerMessage::Event { event, data } = &msg
        {
            let parsed = IpcEvent::parse(event, data);
            if let IpcEvent::History(history) = parsed {
                info.history = history.turns;
                info.streaming = history.streaming;
                info.partial_text = history.partial_text;
            }
        }

        info!(
            session_id,
            session_name = ?info.session_name,
            cwd = %info.cwd,
            turns = info.history.len(),
            "session connected"
        );

        // Set up command channel for this session.
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientMessage>(32);

        // Spawn IPC reader/writer task.
        let ipc_tx_clone = self.ipc_tx.clone();
        let sid = session_id.to_owned();
        let reader = client;
        tokio::spawn(async move {
            let mut client = reader;
            loop {
                tokio::select! {
                    // Read next IPC event.
                    result = client.recv() => {
                        match result {
                            Ok(Some(ServerMessage::Event { event, data })) => {
                                let parsed = IpcEvent::parse(&event, &data);
                                let _ = ipc_tx_clone.send(IpcReaderMsg::Event {
                                    session_id: sid.clone(),
                                    event: parsed,
                                }).await;
                            }
                            Ok(Some(ServerMessage::Response { .. })) => {
                                // Command responses — currently we fire-and-forget.
                                debug!("received command response (ignored)");
                            }
                            Ok(None) => {
                                let _ = ipc_tx_clone.send(IpcReaderMsg::Disconnected {
                                    session_id: sid.clone(),
                                    error: None,
                                }).await;
                                break;
                            }
                            Err(e) => {
                                let _ = ipc_tx_clone.send(IpcReaderMsg::Disconnected {
                                    session_id: sid.clone(),
                                    error: Some(e.to_string()),
                                }).await;
                                break;
                            }
                        }
                    }
                    // Send a command to the extension.
                    Some(cmd) = cmd_rx.recv() => {
                        if let Err(e) = client.send(&cmd).await {
                            warn!(error = %e, "failed to send IPC command");
                        }
                    }
                }
            }
        });

        // Update registry and emit Connected.
        if let Ok(mut reg) = self.registry.write() {
            reg.insert(session_id.to_owned());
        }

        let connected_event = SessionEvent::Connected { info: info.clone() };
        self.publish(connected_event).await;

        self.sessions
            .insert(session_id.to_owned(), SessionConnection { info, cmd_tx });

        Ok(())
    }

    /// Disconnect from a session.
    async fn disconnect_session(&mut self, session_id: &str, reason: &str) {
        if self.sessions.remove(session_id).is_some() {
            if let Ok(mut reg) = self.registry.write() {
                reg.remove(session_id);
            }
            info!(session_id, reason, "session disconnected");
            self.publish(SessionEvent::Disconnected {
                session_id: session_id.to_owned(),
            })
            .await;
        }
    }

    /// Handle a parsed IPC event from a session.
    #[allow(clippy::too_many_lines)]
    async fn handle_ipc_event(&mut self, session_id: &str, event: IpcEvent) {
        let session_event = match event {
            IpcEvent::AgentStart => SessionEvent::AgentStart {
                session_id: session_id.to_owned(),
            },
            IpcEvent::AgentEnd => SessionEvent::AgentEnd {
                session_id: session_id.to_owned(),
            },
            IpcEvent::MessageStart { role, message_id } => {
                if role == "assistant" {
                    SessionEvent::MessageStart {
                        session_id: session_id.to_owned(),
                        message_id,
                    }
                } else {
                    return;
                }
            }
            IpcEvent::MessageDelta { message_id, text } => SessionEvent::MessageDelta {
                session_id: session_id.to_owned(),
                message_id,
                text,
            },
            IpcEvent::ThinkingDelta { message_id, text } => SessionEvent::ThinkingDelta {
                session_id: session_id.to_owned(),
                message_id,
                text,
            },
            IpcEvent::MessageEnd {
                message_id,
                role,
                content,
            } => {
                if role == "assistant" {
                    SessionEvent::MessageEnd {
                        session_id: session_id.to_owned(),
                        message_id,
                        content,
                    }
                } else {
                    return;
                }
            }
            IpcEvent::ToolStart {
                tool_call_id,
                tool_name,
                args,
            } => SessionEvent::ToolStart {
                session_id: session_id.to_owned(),
                tool_call_id,
                tool_name,
                args,
            },
            IpcEvent::ToolEnd {
                tool_call_id,
                tool_name,
                content,
                is_error,
            } => SessionEvent::ToolEnd {
                session_id: session_id.to_owned(),
                tool_call_id,
                tool_name,
                content,
                is_error,
            },
            IpcEvent::SessionNameChanged { name } => {
                // Update local info.
                if let Some(conn) = self.sessions.get_mut(session_id) {
                    conn.info.session_name = Some(name.clone());
                    let info = conn.info.clone();
                    self.publish(SessionEvent::InfoChanged { info }).await;
                }
                return;
            }
            IpcEvent::ModelChanged { model } => {
                if let Some(conn) = self.sessions.get_mut(session_id) {
                    conn.info.model = Some(model);
                    let info = conn.info.clone();
                    self.publish(SessionEvent::InfoChanged { info }).await;
                }
                return;
            }
            IpcEvent::UserMessage {
                content,
                source,
                echo,
            } => SessionEvent::UserMessage {
                session_id: session_id.to_owned(),
                content,
                echo,
                source: source.parse().unwrap_or(MessageSource::Interactive),
            },
            IpcEvent::SessionReset => SessionEvent::SessionReset {
                session_id: session_id.to_owned(),
            },
            IpcEvent::Notification { text } => SessionEvent::Notification {
                session_id: session_id.to_owned(),
                text,
            },
            IpcEvent::SessionEnd => {
                self.disconnect_session(session_id, "session ended").await;
                return;
            }
            IpcEvent::ToolUpdate {
                tool_call_id,
                tool_name,
                content,
            } => SessionEvent::ToolUpdate {
                session_id: session_id.to_owned(),
                tool_call_id,
                tool_name,
                content,
            },
            // Hello/History are handled during connect, not in the event stream.
            IpcEvent::Hello(_)
            | IpcEvent::History(_)
            | IpcEvent::TurnStart { .. }
            | IpcEvent::TurnEnd { .. }
            | IpcEvent::Unknown { .. } => return,
        };

        self.publish(session_event).await;
    }

    /// Publish a session event to the event bus.
    async fn publish(&self, event: SessionEvent) {
        if self.event_tx.send(event).await.is_err() {
            warn!("event bus channel closed");
        }
    }

    /// Route an incoming message to the correct session.
    async fn route_incoming(&self, msg: IncomingMessage) {
        let session_id = msg.session_id().to_owned();
        let span = info_span!("route_incoming", session_id = %session_id);
        async {
            let Some(conn) = self.sessions.get(&session_id) else {
                warn!("no session found for incoming message");
                return;
            };

            let cmd = match msg {
                IncomingMessage::Cancel { .. } => ClientMessage::Abort { id: None },
                IncomingMessage::Send { text, mode, .. } => ClientMessage::Send {
                    message: text,
                    mode: Some(mode),
                    id: None,
                },
            };

            if let Err(e) = conn.cmd_tx.send(cmd).await {
                warn!(error = %e, "failed to route incoming message");
            } else {
                info!("message routed to session");
            }
        }
        .instrument(span)
        .await;
    }

    /// Get a list of currently connected sessions.
    pub fn session_list(&self) -> Vec<SessionInfo> {
        self.sessions.values().map(|c| c.info.clone()).collect()
    }
}
