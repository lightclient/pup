//! High-level service that manages all Claude Code sessions and bridges them
//! to `pup_core::SessionEvent`s.
//!
//! This is the Claude Code agent backend. It:
//! - Runs discovery to find Claude Code sessions
//! - Manages inspector connections for message injection
//! - Polls transcript watchers for conversation events
//! - Publishes core `SessionEvent`s to the event bus

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use pup_core::router::SessionRegistry;
use pup_core::types::{IncomingMessage, MessageSource, SessionEvent};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::discovery::{ClaudeDiscovery, DiscoveredSession, DiscoveryEvent};
use crate::session::{ClaudeSession, InspectorState};

/// The Claude Code integration service.
///
/// Runs as a tokio task alongside the pi session manager. Implements the
/// agent backend pattern: it publishes `SessionEvent`s to the event bus
/// and consumes `IncomingMessage`s from the message router.
#[derive(Debug)]
pub struct ClaudeService {
    /// Path to `~/.claude/projects/`.
    projects_dir: PathBuf,
    /// Active sessions.
    sessions: HashMap<String, ClaudeSession>,
    /// Shared registry of active CC session IDs (for the message router).
    registry: SessionRegistry,
    /// Channel to push `SessionEvent`s to the event bus.
    event_tx: mpsc::Sender<SessionEvent>,
    /// Channel to receive messages from the message router.
    message_rx: mpsc::Receiver<IncomingMessage>,
    /// Discovery event receiver.
    discovery_rx: mpsc::Receiver<DiscoveryEvent>,
    /// Discovery event sender (passed to the discovery task).
    discovery_tx: mpsc::Sender<DiscoveryEvent>,
    /// Recently injected messages per session, used to detect echoes.
    /// When a message is injected, its text is pushed here. When the
    /// transcript watcher reports a matching `UserMessage`, it is marked
    /// as an echo so chat channels don't redisplay it.
    injected_messages: HashMap<String, VecDeque<String>>,
}

impl ClaudeService {
    /// Create a new Claude Code service.
    ///
    /// Returns `(service, message_sender, session_registry)`.
    /// - `message_sender`: used by the message router to send messages to CC sessions
    /// - `session_registry`: shared set of CC session IDs for routing
    pub fn new(
        projects_dir: PathBuf,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> (Self, mpsc::Sender<IncomingMessage>, SessionRegistry) {
        let (message_tx, message_rx) = mpsc::channel(64);
        let (discovery_tx, discovery_rx) = mpsc::channel(64);
        let registry = pup_core::new_registry();

        let service = Self {
            projects_dir,
            sessions: HashMap::new(),
            registry: SessionRegistry::clone(&registry),
            event_tx,
            message_rx,
            discovery_rx,
            discovery_tx,
            injected_messages: HashMap::new(),
        };

        (service, message_tx, registry)
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

                    // Messages from chat channels (via the message router).
                    Some(msg) = self.message_rx.recv() => {
                        self.handle_message(msg).await;
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
                    .send(SessionEvent::Disconnected {
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
                            .send(SessionEvent::Notification {
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
            .send(SessionEvent::Connected { info })
            .await
            .is_err()
        {
            warn!(session_id, "event channel closed");
            return;
        }

        // Check for --dangerously-skip-permissions.
        if discovered.pid.is_some() && !discovered.dangerously_skip_permissions {
            warn!(
                session_id,
                "Claude Code was not started with --dangerously-skip-permissions"
            );
            let _ = self
                .event_tx
                .send(SessionEvent::Notification {
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
                .send(SessionEvent::Notification {
                    session_id: session_id.clone(),
                    text: "🔗 Connected to Claude Code session (bidirectional)".into(),
                })
                .await;
        } else {
            let _ = self.event_tx.send(SessionEvent::Notification {
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
                .send(SessionEvent::Disconnected {
                    session_id: session_id.to_owned(),
                })
                .await;
        }
    }

    /// Handle an incoming message from a chat channel.
    async fn handle_message(&mut self, msg: IncomingMessage) {
        match msg {
            IncomingMessage::Cancel { ref session_id } => {
                if let Some(session) = self.sessions.get_mut(session_id)
                    && let Err(e) = session.inject_escape().await
                {
                    warn!(session_id, error = %e, "failed to inject Escape for cancel");
                }
            }
            IncomingMessage::Send {
                ref session_id,
                ref text,
                ..
            } => {
                // Skip pi-specific slash commands that don't apply to Claude Code.
                let trimmed = text.trim();
                if trimmed.starts_with("/name")
                    || trimmed.starts_with("/compact")
                    || trimmed.starts_with("/new")
                    || trimmed.starts_with("/exit")
                    || trimmed.starts_with("/quit")
                {
                    info!(
                        session_id,
                        text = trimmed,
                        "skipping pi slash command for Claude Code session"
                    );
                    return;
                }

                match self.sessions.get_mut(session_id) {
                    Some(session) => match session.inject_message(text).await {
                        Ok(()) => {
                            // Remember the injected text for echo detection.
                            let queue = self
                                .injected_messages
                                .entry(session_id.clone())
                                .or_default();
                            queue.push_back(text.clone());
                            // Cap the queue to avoid unbounded growth.
                            while queue.len() > 32 {
                                queue.pop_front();
                            }
                        }
                        Err(e) => {
                            warn!(session_id, error = %e, "Claude Code injection failed");
                            let _ = self
                                .event_tx
                                .send(SessionEvent::Notification {
                                    session_id: session_id.clone(),
                                    text: format!("⚠️ Failed to send message: {e}"),
                                })
                                .await;
                        }
                    },
                    None => {
                        warn!(session_id, "no Claude Code session found");
                    }
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
                let event = self.patch_echo(event);
                if self.event_tx.send(event).await.is_err() {
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
                    .send(SessionEvent::Notification {
                        session_id: session.session_id.clone(),
                        text: "🔗 Inspector connected — bidirectional mode enabled".into(),
                    })
                    .await;
            }
        }
    }

    /// Check if a `UserMessage` event was caused by our injection and patch
    /// the `echo` / `source` fields accordingly.
    fn patch_echo(&mut self, event: SessionEvent) -> SessionEvent {
        match event {
            SessionEvent::UserMessage {
                session_id,
                content,
                ..
            } => {
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

                SessionEvent::UserMessage {
                    session_id,
                    content,
                    echo,
                    source: if echo {
                        MessageSource::Extension
                    } else {
                        MessageSource::Interactive
                    },
                }
            }
            other => other,
        }
    }
}
