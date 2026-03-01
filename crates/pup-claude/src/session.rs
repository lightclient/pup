//! Claude Code session state machine.
//!
//! Each Claude Code session has:
//! - A transcript watcher (read path — always available)
//! - An optional inspector connection (write path — needs `BUN_INSPECT`)
//!
//! The session state machine tracks the lifecycle:
//! `Discovered → Connecting → Ready → Lost → (retry)`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use pup_core::types::SessionEvent;
use tracing::{info, warn};

use crate::inspector::InspectorClient;
use crate::transcript::TranscriptWatcher;

// ── Session state ───────────────────────────────────────────────────────────

/// Connection state for a Claude Code session's inspector.
#[derive(Debug)]
pub enum InspectorState {
    /// No inspector URL known (session has no `BUN_INSPECT`).
    Unavailable,
    /// Inspector URL known, not yet connected.
    Discovered { url: String },
    /// Connected to inspector, ready to inject.
    Connected { client: InspectorClient },
    /// Connection lost, will retry.
    Lost {
        url: String,
        last_attempt: Instant,
        backoff: Duration,
    },
}

/// A tracked Claude Code session.
#[derive(Debug)]
pub struct ClaudeSession {
    pub session_id: String,
    pub pid: Option<u32>,
    pub cwd: String,
    pub model: Option<String>,
    pub transcript_path: PathBuf,
    pub watcher: TranscriptWatcher,
    pub inspector: InspectorState,
}

impl ClaudeSession {
    /// Create a new session from a discovered transcript file.
    pub fn new(session_id: String, transcript_path: PathBuf, cwd: String) -> Result<Self> {
        let watcher = TranscriptWatcher::new(session_id.clone(), transcript_path.clone())?;

        Ok(Self {
            session_id,
            pid: None,
            cwd,
            model: None,
            transcript_path,
            watcher,
            inspector: InspectorState::Unavailable,
        })
    }

    /// Set the inspector URL (discovered from process environ).
    pub fn set_inspector_url(&mut self, url: String) {
        match &self.inspector {
            InspectorState::Unavailable => {
                info!(session_id = %self.session_id, url = %url, "inspector URL discovered");
                self.inspector = InspectorState::Discovered { url };
            }
            InspectorState::Lost { .. } => {
                // Reset backoff with new URL.
                self.inspector = InspectorState::Discovered { url };
            }
            _ => {} // Already connected or already have URL.
        }
    }

    /// Try to connect the inspector. Returns true if connected.
    pub async fn connect_inspector(&mut self) -> bool {
        let url = match &self.inspector {
            InspectorState::Discovered { url } => url.clone(),
            InspectorState::Lost {
                url,
                last_attempt,
                backoff,
            } => {
                if last_attempt.elapsed() < *backoff {
                    return false; // Still in backoff.
                }
                url.clone()
            }
            _ => return matches!(&self.inspector, InspectorState::Connected { .. }),
        };

        match InspectorClient::connect(&url).await {
            Ok(client) => {
                info!(session_id = %self.session_id, "inspector connected");
                self.inspector = InspectorState::Connected { client };
                true
            }
            Err(e) => {
                warn!(session_id = %self.session_id, error = %e, "inspector connect failed");
                let backoff = match &self.inspector {
                    InspectorState::Lost { backoff, .. } => {
                        (*backoff * 2).min(Duration::from_secs(30))
                    }
                    _ => Duration::from_secs(2),
                };
                self.inspector = InspectorState::Lost {
                    url,
                    last_attempt: Instant::now(),
                    backoff,
                };
                false
            }
        }
    }

    /// Inject a message into the Claude Code TUI. Returns an error if the
    /// inspector is not connected.
    pub async fn inject_message(&mut self, text: &str) -> Result<()> {
        let client = match &mut self.inspector {
            InspectorState::Connected { client } => client,
            InspectorState::Unavailable => {
                anyhow::bail!(
                    "no inspector available for this session (was Claude Code launched with BUN_INSPECT?)"
                );
            }
            _ => {
                anyhow::bail!("inspector not connected");
            }
        };

        client.inject_stdin(text).await
    }

    /// Send Escape to the Claude Code TUI to cancel the current operation.
    pub async fn inject_escape(&mut self) -> Result<()> {
        let client = match &mut self.inspector {
            InspectorState::Connected { client } => client,
            InspectorState::Unavailable => {
                anyhow::bail!("no inspector available for this session");
            }
            _ => {
                anyhow::bail!("inspector not connected");
            }
        };

        client.inject_escape().await
    }

    /// Check if message injection is available.
    pub fn can_inject(&self) -> bool {
        matches!(&self.inspector, InspectorState::Connected { .. })
    }

    /// Poll the transcript watcher for new events.
    ///
    /// Returns core `SessionEvent`s directly. `UserMessage` events have
    /// `echo: false` — the caller should check for injected-message echoes
    /// and patch them before forwarding.
    pub fn poll_transcript(&mut self) -> Result<Vec<SessionEvent>> {
        self.watcher.poll()
    }
}
