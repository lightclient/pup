use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use pup_core::SessionInfo;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::bot::BotClient;

/// How long to wait after a session disconnects before deleting its topic.
/// If a new session with the same working directory connects within this
/// window, the topic is transferred instead of deleted.
const DELETION_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// A topic whose session disconnected recently, awaiting either a
/// replacement session (matched by cwd) or final deletion.
#[derive(Debug)]
struct PendingDeletion {
    /// Working directory of the disconnected session.
    cwd: String,
    /// Telegram thread ID of the topic.
    thread_id: i64,
    /// When to give up waiting and delete.
    deadline: Instant,
    /// Session name at disconnect time, for restoring on the new session.
    name: Option<String>,
}

/// Resolve git repo name and branch from a working directory.
async fn resolve_git_info(cwd: &str) -> Option<(String, String)> {
    let root = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !root.status.success() {
        return None;
    }
    let root_path = String::from_utf8_lossy(&root.stdout).trim().to_owned();
    let repo_name = root_path.rsplit('/').find(|s| !s.is_empty())?.to_owned();

    let branch = tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !branch.status.success() {
        return None;
    }
    let branch_name = String::from_utf8_lossy(&branch.stdout).trim().to_owned();

    Some((repo_name, branch_name))
}

// ── Persisted state ────────────────────────────────────────────

/// State persisted to disk between daemon restarts.
///
/// Tracks the active session→topic mapping, all thread IDs we've ever created
/// or discovered as ours, and a `getUpdates` checkpoint so we only scan new
/// updates on each startup.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    /// Active session_id → thread_id mapping.
    #[serde(default)]
    topics: HashMap<String, i64>,
    /// All thread IDs we've ever created or discovered as ours (via
    /// `forum_topic_created` service messages matching our icon).
    /// Superset of the values in `topics`.
    #[serde(default)]
    known_threads: HashSet<i64>,
    /// Last `update_id` processed during the startup scan.
    /// On the next startup we resume from here.
    #[serde(default)]
    scan_checkpoint: i64,
    /// Last known session name per working directory.
    /// Used to restore names across pi/pup restarts.
    #[serde(default)]
    cwd_names: HashMap<String, String>,
}

// ── Topics manager ─────────────────────────────────────────────

/// Manages Telegram forum topics — one per pi session.
///
/// Persists state to a JSON file so that stale topics from previous daemon
/// runs can be cleaned up on startup. On each startup:
///
/// 1. Load persisted state (known thread IDs + scan checkpoint).
/// 2. Drain `getUpdates` from the checkpoint to discover any topic creations
///    the bot made since the last run (catches pre-persistence orphans and
///    any gaps from crashes).
/// 3. Delete every known thread ID that doesn't correspond to a live pi
///    session (identified by `.sock` files in the socket directory).
/// 4. Save the cleaned state and new checkpoint.
#[derive(Debug)]
pub struct TopicsManager {
    /// Supergroup chat ID.
    chat_id: i64,
    /// Topic icon prefix (e.g. "📎").
    topic_icon: String,
    /// session_id → thread_id for currently active topics.
    session_topics: HashMap<String, i64>,
    /// thread_id → session_id reverse mapping.
    thread_sessions: HashMap<i64, String>,
    /// All thread IDs we believe we own (created or discovered).
    known_threads: HashSet<i64>,
    /// Track topic names to detect collisions within a run.
    topic_names: HashMap<String, u32>,
    /// Last scanned `update_id`.
    scan_checkpoint: i64,
    /// Path to the JSON state file.
    state_path: PathBuf,
    /// Topics pending deletion after session disconnect (grace period).
    pending_deletions: HashMap<String, PendingDeletion>,
    /// Last known session name per working directory.
    /// Persisted so names survive pi and pup restarts.
    cwd_names: HashMap<String, String>,
}

impl TopicsManager {
    pub fn new(chat_id: i64, topic_icon: String, state_path: PathBuf) -> Self {
        let state = Self::load_state(&state_path);

        let thread_sessions: HashMap<i64, String> = state
            .topics
            .iter()
            .map(|(sid, &tid)| (tid, sid.clone()))
            .collect();

        if !state.known_threads.is_empty() {
            info!(
                active = state.topics.len(),
                known = state.known_threads.len(),
                checkpoint = state.scan_checkpoint,
                path = %state_path.display(),
                "loaded persisted topics state"
            );
        }

        Self {
            chat_id,
            topic_icon,
            session_topics: state.topics,
            thread_sessions,
            known_threads: state.known_threads,
            topic_names: HashMap::new(),
            scan_checkpoint: state.scan_checkpoint,
            state_path,
            pending_deletions: HashMap::new(),
            cwd_names: state.cwd_names,
        }
    }

    /// Load persisted state from disk.
    ///
    /// Handles migration from the old format (bare `HashMap<String, i64>`)
    /// by wrapping it into the new structure.
    fn load_state(path: &Path) -> PersistedState {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return PersistedState::default();
        };

        // Try new format first.
        if let Ok(state) = serde_json::from_str::<PersistedState>(&raw) {
            return state;
        }

        // Fall back to old format: bare {"session_id": thread_id, ...}
        if let Ok(old) = serde_json::from_str::<HashMap<String, i64>>(&raw) {
            let known_threads: HashSet<i64> = old.values().copied().collect();
            info!(migrated = old.len(), "migrated old topics state format");
            return PersistedState {
                topics: old,
                known_threads,
                scan_checkpoint: 0,
                cwd_names: HashMap::new(),
            };
        }

        PersistedState::default()
    }

    /// Persist current state to disk.
    fn save_state(&self) {
        let state = PersistedState {
            topics: self.session_topics.clone(),
            known_threads: self.known_threads.clone(),
            scan_checkpoint: self.scan_checkpoint,
            cwd_names: self.cwd_names.clone(),
        };
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string(&state) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.state_path, &json) {
                    warn!(
                        path = %self.state_path.display(),
                        error = %e,
                        "failed to save topics state"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize topics state");
            }
        }
    }

    // ── Startup scanning ───────────────────────────────────────

    /// Return the `getUpdates` offset to resume scanning from.
    pub fn scan_checkpoint(&self) -> i64 {
        self.scan_checkpoint
    }

    /// Record thread IDs discovered during the startup `getUpdates` scan.
    ///
    /// Only records threads whose `forum_topic_created` name starts with our
    /// icon, so we never accidentally claim non-pup topics.
    pub fn record_discovered_thread(&mut self, thread_id: i64) {
        self.known_threads.insert(thread_id);
    }

    /// Advance the scan checkpoint after processing updates.
    pub fn set_scan_checkpoint(&mut self, update_id: i64) {
        if update_id >= self.scan_checkpoint {
            self.scan_checkpoint = update_id + 1;
        }
    }

    /// Return our topic icon prefix so callers can filter service messages.
    pub fn topic_icon(&self) -> &str {
        &self.topic_icon
    }

    // ── Cleanup ────────────────────────────────────────────────

    /// Delete all known topics that don't correspond to a live pi session.
    ///
    /// `live_session_ids` is the set of session IDs that currently have a
    /// `.sock` file in the socket directory.
    pub async fn cleanup_stale_topics(
        &mut self,
        bot: &BotClient,
        live_session_ids: &HashSet<String>,
    ) {
        // Thread IDs for sessions that are still alive — don't touch these.
        let live_threads: HashSet<i64> = self
            .session_topics
            .iter()
            .filter(|(sid, _)| live_session_ids.contains(*sid))
            .map(|(_, &tid)| tid)
            .collect();

        // Every known thread not in the live set is stale.
        let stale_threads: Vec<i64> = self
            .known_threads
            .iter()
            .filter(|tid| !live_threads.contains(tid))
            .copied()
            .collect();

        if stale_threads.is_empty() {
            debug!("no stale topics to clean up");
        } else {
            info!(count = stale_threads.len(), "cleaning up stale topics");

            for thread_id in &stale_threads {
                info!(thread_id, "deleting stale topic");
                match bot.delete_forum_topic(self.chat_id, *thread_id).await {
                    Ok(_) => {
                        info!(thread_id, "stale topic deleted");
                    }
                    Err(e) => {
                        warn!(
                            thread_id,
                            error = %e,
                            "failed to delete stale topic (may already be gone)"
                        );
                    }
                }
                self.known_threads.remove(thread_id);
            }
        }

        // Remove stale entries from session_topics.
        let stale_sessions: Vec<String> = self
            .session_topics
            .keys()
            .filter(|sid| !live_session_ids.contains(*sid))
            .cloned()
            .collect();
        for sid in &stale_sessions {
            if let Some(tid) = self.session_topics.remove(sid) {
                self.thread_sessions.remove(&tid);
            }
        }

        self.save_state();
    }

    // ── Runtime topic management ───────────────────────────────

    /// Get the supergroup chat ID.
    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    /// Get the thread ID for a session, if a topic exists.
    pub fn thread_for_session(&self, session_id: &str) -> Option<i64> {
        self.session_topics.get(session_id).copied()
    }

    /// Get the session ID for a thread, if known.
    pub fn session_for_thread(&self, thread_id: i64) -> Option<&str> {
        self.thread_sessions.get(&thread_id).map(String::as_str)
    }

    /// Generate a topic name from session info.
    ///
    /// Prefers `session_name`, then `repo/branch` from git, then the last
    /// component of the cwd, and finally a short session ID prefix.
    pub async fn topic_name(&mut self, info: &SessionInfo) -> String {
        let base = if let Some(ref name) = info.session_name {
            name.clone()
        } else if let Some((repo, branch)) = resolve_git_info(&info.cwd).await {
            format!("{repo}/{branch}")
        } else {
            let cwd_name = info.cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");

            if cwd_name.is_empty() || cwd_name == "~" {
                info.session_id[..6.min(info.session_id.len())].to_owned()
            } else {
                cwd_name.to_owned()
            }
        };

        let full_base = format!("{} {base}", self.topic_icon);

        let count = self.topic_names.entry(base).or_insert(0);
        *count += 1;

        if *count > 1 {
            format!("{full_base} ({count})")
        } else {
            full_base
        }
    }

    /// Create or reuse a topic for a session.
    ///
    /// If a persisted mapping exists and the topic is still alive in Telegram,
    /// reuses it (renaming to the current name). Otherwise creates a new one.
    /// Returns `(thread_id, reused)`.
    pub async fn create_topic(
        &mut self,
        bot: &BotClient,
        info: &SessionInfo,
    ) -> Result<(i64, bool)> {
        // Check for an existing persisted topic.
        if let Some(&thread_id) = self.session_topics.get(&info.session_id) {
            let name = self.topic_name(info).await;
            // Verify it still exists by attempting a rename.
            match bot.edit_forum_topic(self.chat_id, thread_id, &name).await {
                Ok(_) => {
                    // Topic still alive — reuse it.
                    self.thread_sessions
                        .insert(thread_id, info.session_id.clone());
                    info!(
                        session_id = %info.session_id,
                        thread_id,
                        topic_name = %name,
                        "reusing existing topic"
                    );
                    return Ok((thread_id, true));
                }
                Err(e) if e.to_string().contains("TOPIC_NOT_MODIFIED") => {
                    // Name unchanged — topic still exists, reuse it.
                    self.thread_sessions
                        .insert(thread_id, info.session_id.clone());
                    debug!(
                        session_id = %info.session_id,
                        thread_id,
                        "reusing existing topic (name unchanged)"
                    );
                    return Ok((thread_id, true));
                }
                Err(e) => {
                    // Topic gone — clean up stale mapping.
                    warn!(
                        session_id = %info.session_id,
                        thread_id,
                        error = %e,
                        "persisted topic gone, creating new one"
                    );
                    self.session_topics.remove(&info.session_id);
                    self.thread_sessions.remove(&thread_id);
                    self.known_threads.remove(&thread_id);
                }
            }
        }

        let name = self.topic_name(info).await;
        info!(session_id = %info.session_id, topic_name = %name, "creating topic");

        let topic = bot.create_forum_topic(self.chat_id, &name).await?;
        let thread_id = topic.message_thread_id;

        self.session_topics
            .insert(info.session_id.clone(), thread_id);
        self.thread_sessions
            .insert(thread_id, info.session_id.clone());
        self.known_threads.insert(thread_id);

        self.save_state();

        info!(session_id = %info.session_id, thread_id, "topic created");
        Ok((thread_id, false))
    }

    /// Delete the topic for a disconnected session.
    pub async fn delete_topic(&mut self, bot: &BotClient, session_id: &str) -> Result<()> {
        let Some(thread_id) = self.session_topics.remove(session_id) else {
            debug!(session_id, "no topic to delete");
            return Ok(());
        };

        self.thread_sessions.remove(&thread_id);
        self.known_threads.remove(&thread_id);
        self.save_state();

        info!(session_id, thread_id, "deleting topic");
        match bot.delete_forum_topic(self.chat_id, thread_id).await {
            Ok(_) => {}
            Err(e) => {
                warn!(session_id, thread_id, error = %e, "failed to delete topic");
            }
        }
        Ok(())
    }

    // ── Grace period for session restarts ──────────────────────

    /// Schedule a topic for delayed deletion instead of removing it
    /// immediately.  If a new session with the same working directory
    /// connects within [`DELETION_GRACE_PERIOD`], the topic is reused.
    ///
    /// `name` is the session name at disconnect time, stored so it can be
    /// restored on the replacement session and persisted in `cwd_names`.
    ///
    /// Returns the thread ID if the topic was scheduled, `None` if the
    /// session had no topic.
    pub fn schedule_deletion(
        &mut self,
        session_id: &str,
        cwd: &str,
        name: Option<&str>,
    ) -> Option<i64> {
        let thread_id = self.session_topics.remove(session_id)?;
        self.thread_sessions.remove(&thread_id);
        // Keep in known_threads so startup cleanup can find it if pup crashes.

        // Remember the name for this cwd so it survives pup restarts too.
        if let Some(n) = name {
            self.cwd_names.insert(cwd.to_owned(), n.to_owned());
        }

        info!(
            session_id,
            thread_id,
            grace_secs = DELETION_GRACE_PERIOD.as_secs(),
            "topic scheduled for deletion"
        );

        self.pending_deletions.insert(
            session_id.to_owned(),
            PendingDeletion {
                cwd: cwd.to_owned(),
                thread_id,
                deadline: Instant::now() + DELETION_GRACE_PERIOD,
                name: name.map(ToOwned::to_owned),
            },
        );

        self.save_state();
        Some(thread_id)
    }

    /// Try to match a newly connected session to a recently-disconnected
    /// one by working directory.  If found, transfers the topic to the new
    /// session and cancels the pending deletion.
    ///
    /// Returns `(thread_id, remembered_name)` if a match was found.
    pub fn claim_pending_topic(
        &mut self,
        new_session_id: &str,
        cwd: &str,
    ) -> Option<(i64, Option<String>)> {
        let matching_key = self
            .pending_deletions
            .iter()
            .find(|(_, pd)| pd.cwd == cwd)
            .map(|(k, _)| k.clone());

        let old_session_id = matching_key?;
        let pd = self.pending_deletions.remove(&old_session_id)?;

        info!(
            new_session_id,
            old_session_id = %old_session_id,
            thread_id = pd.thread_id,
            "new session claimed pending topic"
        );

        self.session_topics
            .insert(new_session_id.to_owned(), pd.thread_id);
        self.thread_sessions
            .insert(pd.thread_id, new_session_id.to_owned());
        self.save_state();

        Some((pd.thread_id, pd.name))
    }

    // ── Name persistence per cwd ─────────────────────────────────

    /// Record a session name for a working directory.
    /// Persisted to disk so it survives pup restarts.
    pub fn remember_name(&mut self, cwd: &str, name: &str) {
        self.cwd_names.insert(cwd.to_owned(), name.to_owned());
        self.save_state();
    }

    /// Look up the last known session name for a working directory.
    pub fn last_name_for_cwd(&self, cwd: &str) -> Option<&str> {
        self.cwd_names.get(cwd).map(String::as_str)
    }

    /// Drain pending deletions whose grace period has expired.
    /// Returns `(session_id, thread_id)` pairs that should be deleted.
    pub fn drain_expired(&mut self) -> Vec<(String, i64)> {
        let now = Instant::now();
        let expired: Vec<String> = self
            .pending_deletions
            .iter()
            .filter(|(_, pd)| now >= pd.deadline)
            .map(|(k, _)| k.clone())
            .collect();

        if expired.is_empty() {
            return Vec::new();
        }

        let mut result = Vec::new();
        for sid in expired {
            if let Some(pd) = self.pending_deletions.remove(&sid) {
                self.known_threads.remove(&pd.thread_id);
                info!(
                    session_id = %sid,
                    thread_id = pd.thread_id,
                    "grace period expired"
                );
                result.push((sid, pd.thread_id));
            }
        }

        self.save_state();
        result
    }

    /// Cancel all pending deletions, restoring their topic mappings.
    ///
    /// Called during graceful shutdown so that topic state is preserved
    /// across pup restarts.  If pup reconnects to the same sessions on
    /// startup, the topics will be reused instead of deleted and recreated.
    pub fn cancel_all_pending(&mut self) {
        if self.pending_deletions.is_empty() {
            return;
        }
        info!(
            count = self.pending_deletions.len(),
            "restoring pending topic deletions for shutdown"
        );
        for (session_id, pd) in self.pending_deletions.drain() {
            self.session_topics.insert(session_id.clone(), pd.thread_id);
            self.thread_sessions.insert(pd.thread_id, session_id);
        }
        self.save_state();
    }

    /// Rename a topic when session info changes.
    pub async fn rename_topic(&mut self, bot: &BotClient, info: &SessionInfo) -> Result<()> {
        let Some(&thread_id) = self.session_topics.get(&info.session_id) else {
            return Ok(());
        };

        let name = self.topic_name(info).await;
        info!(session_id = %info.session_id, thread_id, new_name = %name, "renaming topic");

        match bot.edit_forum_topic(self.chat_id, thread_id, &name).await {
            Ok(_) => Ok(()),
            // Name unchanged — not an error.
            Err(e) if e.to_string().contains("TOPIC_NOT_MODIFIED") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Validate that the bot has the required permissions in the supergroup.
    pub async fn validate(bot: &BotClient, chat_id: i64, bot_user_id: i64) -> Result<()> {
        let member = bot.get_chat_member(chat_id, bot_user_id).await?;

        if member.status != "administrator" && member.status != "creator" {
            anyhow::bail!(
                "Bot is not an admin in the supergroup (status: {})",
                member.status
            );
        }

        if member.can_manage_topics != Some(true) {
            anyhow::bail!("Bot does not have 'can_manage_topics' permission");
        }

        info!("topics validation passed");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
mod tests {
    use super::*;

    /// State path that discards writes and returns empty on read.
    fn test_state_path() -> PathBuf {
        PathBuf::from("/dev/null")
    }

    #[tokio::test]
    async fn test_topic_name_with_session_name() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned(), test_state_path());
        let info = SessionInfo {
            session_id: "abc123".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/home/user/code".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info).await, "📎 myproject");
    }

    #[tokio::test]
    async fn test_topic_name_from_cwd() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned(), test_state_path());
        let info = SessionInfo {
            session_id: "abc123".to_owned(),
            session_name: None,
            cwd: "/home/user/code/foo".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info).await, "📎 foo");
    }

    #[tokio::test]
    async fn test_topic_name_collision() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned(), test_state_path());
        let info1 = SessionInfo {
            session_id: "aaa".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/tmp".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        let info2 = SessionInfo {
            session_id: "bbb".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/tmp".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info1).await, "📎 myproject");
        assert_eq!(mgr.topic_name(&info2).await, "📎 myproject (2)");
    }
}
