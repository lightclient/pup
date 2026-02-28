//! High-level service that manages all Claude Code sessions and bridges them
//! to `pup_core::SessionEvent`s.
//!
//! This is the main entry point for the pup daemon. It:
//! - Runs discovery to find Claude Code sessions
//! - Manages inspector connections for message injection
//! - Polls transcript watchers for conversation events
//! - Converts internal events to `pup_core` types for backend fan-out

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::discovery::{ClaudeDiscovery, DiscoveredSession, DiscoveryEvent};
use crate::session::{ClaudeSession, InspectorState};

/// Messages from pup-daemon to the Claude service.
#[derive(Debug)]
pub enum ClaudeCommand {
    /// Inject a user message into a Claude Code session.
    InjectMessage {
        session_id: String,
        text: String,
        /// Reply channel for success/error.
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Cancel/abort the current turn (sends Escape to the TUI).
    Cancel { session_id: String },
}

/// Shared registry of Claude Code session IDs, used by the message router
/// to determine whether an incoming message should go to the Claude service
/// or the pi session manager.
pub type SessionRegistry = Arc<RwLock<HashSet<String>>>;

/// The Claude Code integration service.
///
/// Runs as a tokio task alongside the main session manager.
#[derive(Debug)]
pub struct ClaudeService {
    /// Path to `~/.claude/projects/`.
    projects_dir: PathBuf,
    /// Active sessions.
    sessions: HashMap<String, ClaudeSession>,
    /// Shared registry of active CC session IDs.
    registry: SessionRegistry,
    /// Channel to push `pup_core::SessionEvent`s to backends.
    event_tx: mpsc::Sender<pup_core::types::SessionEvent>,
    /// Channel to receive commands from the daemon.
    command_rx: mpsc::Receiver<ClaudeCommand>,
    /// Discovery event receiver.
    discovery_rx: mpsc::Receiver<DiscoveryEvent>,
    /// Discovery event sender (passed to the discovery task).
    discovery_tx: mpsc::Sender<DiscoveryEvent>,
    /// Recently injected messages per session, used to detect echoes.
    /// When a message is injected via `InjectMessage`, its text is pushed
    /// here. When the transcript watcher reports a matching `UserMessage`,
    /// it is marked as an echo so backends don't redisplay it.
    injected_messages: HashMap<String, VecDeque<String>>,
}

impl ClaudeService {
    /// Create a new Claude Code service.
    ///
    /// Returns `(service, command_sender, session_registry)`.
    /// - `command_sender`: used by the daemon to send injection/cancel commands
    /// - `session_registry`: shared set of CC session IDs for message routing
    pub fn new(
        projects_dir: PathBuf,
        event_tx: mpsc::Sender<pup_core::types::SessionEvent>,
    ) -> (Self, mpsc::Sender<ClaudeCommand>, SessionRegistry) {
        let (command_tx, command_rx) = mpsc::channel(64);
        let (discovery_tx, discovery_rx) = mpsc::channel(64);
        let registry = Arc::new(RwLock::new(HashSet::new()));

        let service = Self {
            projects_dir,
            sessions: HashMap::new(),
            registry: Arc::clone(&registry),
            event_tx,
            command_rx,
            discovery_rx,
            discovery_tx,
            injected_messages: HashMap::new(),
        };

        (service, command_tx, registry)
    }

    /// Run the service. This is the main loop.
    pub async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        let span = info_span!("claude_service");
        async {
            info!("starting Claude Code service");

            // Spawn discovery loop.
            let discovery =
                ClaudeDiscovery::new(self.projects_dir.clone(), self.discovery_tx.clone());
            tokio::spawn(async move {
                if let Err(e) = discovery.run(Duration::from_secs(5)).await {
                    error!(error = %e, "Claude discovery loop failed");
                }
            });

            // Transcript poll timer.
            let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
            poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Inspector connection retry timer.
            let mut retry_interval = tokio::time::interval(Duration::from_secs(5));
            retry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Shutdown.
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            info!("Claude service shutting down");
                            break;
                        }
                    }

                    // Discovery events.
                    Some(event) = self.discovery_rx.recv() => {
                        self.handle_discovery(event).await;
                    }

                    // Commands from session manager.
                    Some(cmd) = self.command_rx.recv() => {
                        self.handle_command(cmd).await;
                    }

                    // Poll transcripts.
                    _ = poll_interval.tick() => {
                        self.poll_all_transcripts().await;
                    }

                    // Retry inspector connections.
                    _ = retry_interval.tick() => {
                        self.retry_inspector_connections().await;
                    }
                }
            }

            // Emit disconnect for all sessions.
            for session_id in self.sessions.keys() {
                let _ = self
                    .event_tx
                    .send(pup_core::types::SessionEvent::Disconnected {
                        session_id: session_id.clone(),
                    })
                    .await;
            }

            info!("Claude service stopped");
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Handle a discovery event.
    async fn handle_discovery(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::SessionAppeared(discovered) => {
                self.connect_session(discovered).await;
            }
            DiscoveryEvent::SessionGone { session_id } => {
                self.disconnect_session(&session_id).await;
            }
            DiscoveryEvent::InspectorDiscovered {
                session_id,
                inspector_url,
                pid,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    info!(
                        session_id,
                        url = inspector_url,
                        pid,
                        "late inspector discovery — connecting"
                    );
                    session.pid = Some(pid);
                    session.set_inspector_url(inspector_url);
                    if session.connect_inspector().await {
                        let _ = self
                            .event_tx
                            .send(pup_core::types::SessionEvent::Notification {
                                session_id: session_id.clone(),
                                text: "🔗 Inspector connected — bidirectional mode enabled".into(),
                            })
                            .await;
                    }
                }
            }
        }
    }

    /// Connect to a newly discovered Claude Code session.
    async fn connect_session(&mut self, discovered: DiscoveredSession) {
        if self.sessions.contains_key(&discovered.session_id) {
            // Already tracking. But maybe update inspector URL.
            if let Some(url) = &discovered.inspector_url
                && let Some(session) = self.sessions.get_mut(&discovered.session_id)
            {
                session.set_inspector_url(url.clone());
            }
            return;
        }

        let session_id = discovered.session_id.clone();
        info!(session_id, path = %discovered.transcript_path.display(), "connecting Claude Code session");

        let mut session = match ClaudeSession::new(
            session_id.clone(),
            discovered.transcript_path,
            discovered.cwd.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(session_id, error = %e, "failed to create Claude session");
                return;
            }
        };

        session.pid = discovered.pid;

        // Parse existing transcript for history.
        let (model, history) = match session.watcher.parse_history() {
            Ok(h) => h,
            Err(e) => {
                warn!(session_id, error = %e, "failed to parse transcript history");
                (None, Vec::new())
            }
        };

        session.model.clone_from(&model);

        // Set inspector URL if available.
        if let Some(url) = &discovered.inspector_url {
            session.set_inspector_url(url.clone());
            // Try connecting immediately.
            session.connect_inspector().await;
        }

        let can_inject = session.can_inject();

        // Emit Connected event.
        let info = pup_core::types::SessionInfo {
            session_id: session_id.clone(),
            session_name: None,
            cwd: discovered.cwd,
            model,
            history,
            streaming: false,
            partial_text: None,
        };

        if self
            .event_tx
            .send(pup_core::types::SessionEvent::Connected { info })
            .await
            .is_err()
        {
            warn!(session_id, "event channel closed");
            return;
        }

        // Check for --dangerously-skip-permissions. Without it, Claude Code
        // will prompt for tool-use confirmations in the TUI which pup cannot
        // answer, so we warn the user.
        if discovered.pid.is_some() && !discovered.dangerously_skip_permissions {
            warn!(
                session_id,
                "Claude Code was not started with --dangerously-skip-permissions"
            );
            let _ = self
                .event_tx
                .send(pup_core::types::SessionEvent::Notification {
                    session_id: session_id.clone(),
                    text: "⚠️ Claude Code was not started with --dangerously-skip-permissions. \
                       Permission prompts cannot be handled remotely — messages sent from \
                       here may get stuck. Restart Claude Code with \
                       --dangerously-skip-permissions to enable full remote control."
                        .into(),
                })
                .await;
        }

        // Notify about injection capability.
        if can_inject {
            let _ = self
                .event_tx
                .send(pup_core::types::SessionEvent::Notification {
                    session_id: session_id.clone(),
                    text: "🔗 Connected to Claude Code session (bidirectional)".into(),
                })
                .await;
        } else {
            let _ = self.event_tx.send(pup_core::types::SessionEvent::Notification {
                session_id: session_id.clone(),
                text: "👁 Connected to Claude Code session (read-only — launch with BUN_INSPECT for bidirectional)".into(),
            }).await;
        }

        if let Ok(mut reg) = self.registry.write() {
            reg.insert(session_id.clone());
        }
        self.sessions.insert(session_id, session);
    }

    /// Disconnect from a session.
    async fn disconnect_session(&mut self, session_id: &str) {
        if self.sessions.remove(session_id).is_some() {
            self.injected_messages.remove(session_id);
            if let Ok(mut reg) = self.registry.write() {
                reg.remove(session_id);
            }
            info!(session_id, "Claude Code session disconnected");
            let _ = self
                .event_tx
                .send(pup_core::types::SessionEvent::Disconnected {
                    session_id: session_id.to_owned(),
                })
                .await;
        }
    }

    /// Handle a command from the daemon.
    async fn handle_command(&mut self, cmd: ClaudeCommand) {
        match cmd {
            ClaudeCommand::InjectMessage {
                session_id,
                text,
                reply,
            } => {
                let result = match self.sessions.get_mut(&session_id) {
                    Some(session) => match session.inject_message(&text).await {
                        Ok(()) => {
                            // Remember the injected text so we can mark the
                            // corresponding transcript entry as an echo.
                            let queue = self
                                .injected_messages
                                .entry(session_id.clone())
                                .or_default();
                            queue.push_back(text);
                            // Cap the queue to avoid unbounded growth if the
                            // transcript watcher falls behind.
                            while queue.len() > 32 {
                                queue.pop_front();
                            }
                            Ok(())
                        }
                        Err(e) => Err(e.to_string()),
                    },
                    None => Err(format!("no Claude Code session with id {session_id}")),
                };
                let _ = reply.send(result).await;
            }
            ClaudeCommand::Cancel { session_id } => {
                if let Some(session) = self.sessions.get_mut(&session_id)
                    && let Err(e) = session.inject_escape().await
                {
                    warn!(session_id, error = %e, "failed to inject Escape for cancel");
                }
            }
        }
    }

    /// Poll all transcript watchers for new events.
    async fn poll_all_transcripts(&mut self) {
        let session_ids: Vec<String> = self.sessions.keys().cloned().collect();

        for session_id in session_ids {
            let events = {
                let Some(session) = self.sessions.get_mut(&session_id) else {
                    continue;
                };
                match session.poll_transcript() {
                    Ok(events) => events,
                    Err(e) => {
                        debug!(session_id, error = %e, "transcript poll failed");
                        continue;
                    }
                }
            };

            for event in events {
                let core_event = self.convert_event(event);
                if self.event_tx.send(core_event).await.is_err() {
                    warn!("event channel closed");
                    return;
                }
            }
        }
    }

    /// Retry inspector connections for sessions in `Lost` state.
    async fn retry_inspector_connections(&mut self) {
        for session in self.sessions.values_mut() {
            if matches!(
                &session.inspector,
                InspectorState::Lost { .. } | InspectorState::Discovered { .. }
            ) && session.connect_inspector().await
            {
                let _ = self
                    .event_tx
                    .send(pup_core::types::SessionEvent::Notification {
                        session_id: session.session_id.clone(),
                        text: "🔗 Inspector connected — bidirectional mode enabled".into(),
                    })
                    .await;
            }
        }
    }

    /// Convert a `pup_claude::SessionEvent` to a `pup_core::SessionEvent`,
    /// detecting echoed messages that were injected from a backend.
    fn convert_event(
        &mut self,
        event: crate::session::SessionEvent,
    ) -> pup_core::types::SessionEvent {
        match event {
            crate::session::SessionEvent::UserMessage {
                session_id,
                content,
            } => {
                // Check if this message was injected by us (from a backend).
                // If so, mark it as an echo so the backend doesn't redisplay it.
                let echo = self
                    .injected_messages
                    .get_mut(&session_id)
                    .and_then(|queue| {
                        queue
                            .iter()
                            .position(|t| *t == content)
                            .map(|idx| queue.remove(idx))
                    })
                    .is_some();

                pup_core::types::SessionEvent::UserMessage {
                    session_id,
                    content,
                    echo,
                    source: if echo {
                        pup_core::types::MessageSource::Extension
                    } else {
                        pup_core::types::MessageSource::Interactive
                    },
                }
            }
            crate::session::SessionEvent::AgentStart { session_id } => {
                pup_core::types::SessionEvent::AgentStart { session_id }
            }
            crate::session::SessionEvent::AgentEnd { session_id } => {
                pup_core::types::SessionEvent::AgentEnd { session_id }
            }
            crate::session::SessionEvent::MessageStart {
                session_id,
                message_id,
            } => pup_core::types::SessionEvent::MessageStart {
                session_id,
                message_id,
            },
            crate::session::SessionEvent::MessageEnd {
                session_id,
                message_id,
                text,
                ..
            } => pup_core::types::SessionEvent::MessageEnd {
                session_id,
                message_id,
                content: text,
            },
            crate::session::SessionEvent::ToolStart {
                session_id,
                tool_use_id,
                tool_name,
                input,
            } => pup_core::types::SessionEvent::ToolStart {
                session_id,
                tool_call_id: tool_use_id,
                tool_name,
                args: input,
            },
            crate::session::SessionEvent::ToolEnd {
                session_id,
                tool_use_id,
                content,
                is_error,
            } => pup_core::types::SessionEvent::ToolEnd {
                session_id,
                tool_call_id: tool_use_id,
                tool_name: String::new(), // Not available from transcript tool_result entries.
                content,
                is_error,
            },
        }
    }
}
