//! Discover active Claude Code sessions by scanning processes and transcript files.
//!
//! Two discovery strategies:
//! 1. **Process scanning**: Find `claude` processes, read `/proc/<pid>/environ`
//!    for `BUN_INSPECT` URLs and session working directories.
//! 2. **Transcript scanning**: Watch `~/.claude/projects/` for recently-modified
//!    `.jsonl` files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span};

/// A discovered Claude Code session.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    /// The session UUID (from the transcript filename).
    pub session_id: String,
    /// Path to the `.jsonl` transcript file.
    pub transcript_path: PathBuf,
    /// Working directory (from the transcript or process).
    pub cwd: String,
    /// Inspector WebSocket URL (from `BUN_INSPECT` env var), if available.
    pub inspector_url: Option<String>,
    /// Process ID of the Claude Code process, if found.
    pub pid: Option<u32>,
}

/// Events from the discovery loop.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A new Claude Code session was found.
    SessionAppeared(DiscoveredSession),
    /// A session is no longer active (process exited, transcript stale).
    SessionGone { session_id: String },
    /// An inspector URL was discovered for a session that previously had none.
    InspectorDiscovered {
        session_id: String,
        inspector_url: String,
        pid: u32,
    },
}

/// Watches for Claude Code sessions by scanning processes and transcript files.
#[derive(Debug)]
pub struct ClaudeDiscovery {
    projects_dir: PathBuf,
    tx: mpsc::Sender<DiscoveryEvent>,
    /// Known sessions: session_id → last seen info.
    known: HashMap<String, KnownSession>,
    /// Sessions that were recently marked as gone, to prevent immediate
    /// rediscovery. Maps session_id → time gone was emitted.
    recently_gone: HashMap<String, std::time::Instant>,
    /// How long before an inactive transcript is considered dead.
    inactive_timeout: Duration,
}

#[derive(Debug)]
struct KnownSession {
    pid: Option<u32>,
    #[allow(dead_code)]
    transcript_path: PathBuf,
    last_modified: std::time::SystemTime,
}

impl ClaudeDiscovery {
    /// Create a new discovery service.
    ///
    /// `projects_dir` is typically `~/.claude/projects/`.
    pub fn new(projects_dir: PathBuf, tx: mpsc::Sender<DiscoveryEvent>) -> Self {
        Self {
            projects_dir,
            tx,
            known: HashMap::new(),
            recently_gone: HashMap::new(),
            inactive_timeout: Duration::from_secs(60),
        }
    }

    /// Run the discovery loop. Scans every `interval` for changes.
    pub async fn run(mut self, interval: Duration) -> Result<()> {
        let span = info_span!("claude_discovery", dir = %self.projects_dir.display());
        async {
            info!("starting Claude Code discovery");
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tick.tick().await;
                if let Err(e) = self.scan().await {
                    debug!(error = %e, "discovery scan failed");
                }
            }
        }
        .instrument(span)
        .await
    }

    /// Perform one discovery scan.
    async fn scan(&mut self) -> Result<()> {
        // 0. Clean up stale entries from the recently_gone set (after 10 min).
        let gone_ttl = Duration::from_secs(600);
        self.recently_gone
            .retain(|_, when| when.elapsed() < gone_ttl);

        // 1. Find all recent transcript files.
        let transcripts = find_recent_transcripts(&self.projects_dir).await?;

        // 2. Find Claude Code processes with BUN_INSPECT.
        let processes = find_claude_processes().await;

        // 3. Match transcripts to processes and deduplicate.
        //
        // Multiple transcript files may exist for the same Claude Code process
        // (e.g., old sessions that were resumed). We only track the most recently
        // modified transcript per process to avoid duplicate injections.
        let mut seen_ids = std::collections::HashSet::new();

        // First pass: match transcripts to processes and pick the best per PID.
        struct Candidate {
            session_id: String,
            path: PathBuf,
            modified: std::time::SystemTime,
            cwd: String,
            inspector_url: Option<String>,
            pid: Option<u32>,
        }

        let mut best_per_pid: HashMap<u32, Candidate> = HashMap::new();
        let mut no_pid_candidates: Vec<Candidate> = Vec::new();

        for (session_id, path, modified, cwd_from_dir) in &transcripts {
            // Skip sessions that were recently marked as gone to prevent
            // rediscovery loops. They can come back if the process restarts
            // (the recently_gone entry expires after 10 min, or the PID
            // changes, indicating a genuinely new process).
            if self.recently_gone.contains_key(session_id) {
                continue;
            }

            let mut inspector_url = None;
            let mut pid = None;
            let mut cwd = cwd_from_dir.clone();

            for proc in &processes {
                if proc.cwd == *cwd_from_dir || proc.session_ids.contains(session_id) {
                    inspector_url.clone_from(&proc.inspector_url);
                    pid = Some(proc.pid);
                    if !proc.cwd.is_empty() {
                        cwd.clone_from(&proc.cwd);
                    }
                    break;
                }
            }

            let candidate = Candidate {
                session_id: session_id.clone(),
                path: path.clone(),
                modified: *modified,
                cwd,
                inspector_url,
                pid,
            };

            if let Some(p) = pid {
                // Keep only the most recently modified transcript per PID.
                let dominated = best_per_pid
                    .get(&p)
                    .is_some_and(|existing| existing.modified >= *modified);
                if !dominated {
                    best_per_pid.insert(p, candidate);
                }
            } else {
                no_pid_candidates.push(candidate);
            }
        }

        // Collect the winning candidates.
        let candidates: Vec<Candidate> = best_per_pid
            .into_values()
            .chain(no_pid_candidates)
            .collect();

        for c in &candidates {
            seen_ids.insert(c.session_id.clone());

            // Cross-scan deduplication: if this candidate has a PID that
            // matches an already-known session with a DIFFERENT session ID,
            // the old session should be replaced (it's a stale transcript
            // from the same Claude Code process).
            if let Some(pid) = c.pid {
                let stale: Vec<String> = self
                    .known
                    .iter()
                    .filter(|(sid, ks)| {
                        *sid != &c.session_id && ks.pid == Some(pid)
                    })
                    .map(|(sid, _)| sid.clone())
                    .collect();

                for stale_sid in stale {
                    info!(
                        old_session = stale_sid,
                        new_session = c.session_id,
                        pid,
                        "replacing stale CC session (same PID, different session ID)"
                    );
                    self.known.remove(&stale_sid);
                    seen_ids.remove(&stale_sid);
                    let _ = self
                        .tx
                        .send(DiscoveryEvent::SessionGone { session_id: stale_sid })
                        .await;
                }
            }

            // Check if we already know about this session.
            if let Some(known) = self.known.get(&c.session_id) {
                let mut changed = false;
                if c.modified > known.last_modified {
                    changed = true;
                }
                // Check if the PID or inspector URL changed (e.g., a new
                // Claude Code process picked up the same session).
                let pid_changed = c.pid.is_some() && c.pid != known.pid;
                let inspector_newly_found = c.inspector_url.is_some() && pid_changed;

                if changed || pid_changed {
                    if let Some(k) = self.known.get_mut(&c.session_id) {
                        k.last_modified = c.modified;
                        if pid_changed {
                            k.pid = c.pid;
                        }
                    }
                }

                if inspector_newly_found {
                    if let (Some(url), Some(pid)) = (&c.inspector_url, c.pid) {
                        info!(
                            session_id = c.session_id,
                            url,
                            pid,
                            "inspector URL discovered for existing session"
                        );
                        let _ = self
                            .tx
                            .send(DiscoveryEvent::InspectorDiscovered {
                                session_id: c.session_id.clone(),
                                inspector_url: url.clone(),
                                pid,
                            })
                            .await;
                    }
                }

                continue;
            }

            info!(
                session_id = c.session_id,
                path = %c.path.display(),
                inspector = ?c.inspector_url,
                pid = ?c.pid,
                "discovered Claude Code session"
            );

            self.known.insert(
                c.session_id.clone(),
                KnownSession {
                    pid: c.pid,
                    transcript_path: c.path.clone(),
                    last_modified: c.modified,
                },
            );

            let _ = self
                .tx
                .send(DiscoveryEvent::SessionAppeared(DiscoveredSession {
                    session_id: c.session_id.clone(),
                    transcript_path: c.path.clone(),
                    cwd: c.cwd.clone(),
                    inspector_url: c.inspector_url.clone(),
                    pid: c.pid,
                }))
                .await;
        }

        // 4. Check for sessions that have gone away.
        let now = std::time::SystemTime::now();
        let mut gone = Vec::new();

        for (session_id, known) in &self.known {
            if !seen_ids.contains(session_id) {
                gone.push(session_id.clone());
                continue;
            }

            // Check if the transcript has been inactive too long.
            if let Ok(elapsed) = now.duration_since(known.last_modified) {
                if elapsed > self.inactive_timeout {
                    // Also check if the process is still running.
                    let proc_alive = known.pid.is_some_and(|p| process_alive(p));
                    if !proc_alive {
                        gone.push(session_id.clone());
                    }
                }
            }
        }

        for session_id in gone {
            info!(session_id, "Claude Code session gone");
            self.known.remove(&session_id);
            self.recently_gone
                .insert(session_id.clone(), std::time::Instant::now());
            let _ = self
                .tx
                .send(DiscoveryEvent::SessionGone {
                    session_id,
                })
                .await;
        }

        Ok(())
    }
}

/// Info about a running Claude Code process.
#[derive(Debug)]
struct ClaudeProcess {
    pid: u32,
    cwd: String,
    inspector_url: Option<String>,
    session_ids: Vec<String>,
}

/// Find running Claude Code processes and extract their `BUN_INSPECT` URLs.
async fn find_claude_processes() -> Vec<ClaudeProcess> {
    let mut result = Vec::new();

    let Ok(mut dir) = tokio::fs::read_dir("/proc").await else {
        return result;
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let name = entry.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        // Check if this is a Claude Code process.
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let Ok(cmdline) = tokio::fs::read(&cmdline_path).await else {
            continue;
        };

        let cmdline_str = String::from_utf8_lossy(&cmdline);
        // Match the Claude Code binary specifically. The cmdline contains
        // the full path like `/root/.local/share/claude/versions/2.1.34`.
        // We check for both "claude" in the path AND "versions/" to avoid
        // matching child processes (like rust-analyzer) that are launched
        // by Claude Code and happen to have "claude" in their ancestor path.
        let is_claude_binary = cmdline_str.contains("claude/versions/")
            || cmdline_str.contains("claude-code")
            || cmdline_str.starts_with("claude\0");
        if !is_claude_binary {
            continue;
        }

        // Read environment variables.
        let environ_path = format!("/proc/{pid}/environ");
        let Ok(environ) = tokio::fs::read(&environ_path).await else {
            continue;
        };

        let mut inspector_url = None;
        let mut cwd = String::new();
        let session_ids = Vec::new();

        for entry in environ.split(|&b| b == 0) {
            let s = String::from_utf8_lossy(entry);
            if let Some(url) = s.strip_prefix("BUN_INSPECT=") {
                inspector_url = Some(url.to_owned());
            }
            if let Some(c) = s.strip_prefix("PWD=") {
                cwd = c.to_owned();
            }
        }

        // Try to get CWD from /proc/<pid>/cwd symlink.
        if cwd.is_empty() {
            if let Ok(link) = tokio::fs::read_link(format!("/proc/{pid}/cwd")).await {
                cwd = link.to_string_lossy().into_owned();
            }
        }

        result.push(ClaudeProcess {
            pid,
            cwd,
            inspector_url,
            session_ids,
        });
    }

    result
}

/// Find recently-modified `.jsonl` transcript files under the projects directory.
///
/// Returns `(session_id, path, modified_time, cwd_derived_from_dir_name)`.
async fn find_recent_transcripts(
    projects_dir: &Path,
) -> Result<Vec<(String, PathBuf, std::time::SystemTime, String)>> {
    let mut result = Vec::new();

    let Ok(mut project_dirs) = tokio::fs::read_dir(projects_dir).await else {
        return Ok(result);
    };

    while let Ok(Some(project_entry)) = project_dirs.next_entry().await {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        // Project directory name is like "-root-myproject" (path with / replaced by -).
        let dir_name = project_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Convert dir name back to a path: "-root-myproject" → "/root/myproject"
        let cwd = slug_to_path(dir_name);

        let Ok(mut files) = tokio::fs::read_dir(&project_path).await else {
            continue;
        };

        while let Ok(Some(file_entry)) = files.next_entry().await {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let Ok(metadata) = file_entry.metadata().await else {
                continue;
            };

            let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);

            // Only consider files modified in the last 5 minutes as "active".
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age > Duration::from_secs(300) {
                continue;
            }

            // Session ID is the filename without extension.
            let session_id = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();

            if session_id.is_empty() {
                continue;
            }

            result.push((session_id, file_path, modified, cwd.clone()));
        }
    }

    Ok(result)
}

/// Convert a Claude Code project slug back to a filesystem path.
///
/// Claude Code uses the project path with `/` replaced by `-` and a leading `-`.
/// e.g., `/root/pup/main` → `-root-pup-main`
fn slug_to_path(slug: &str) -> String {
    if slug.is_empty() {
        return String::new();
    }
    // Replace leading `-` and subsequent `-` with `/`.
    // This is lossy (directory names with `-` are ambiguous) but good enough
    // for display purposes.
    let path = slug.replace('-', "/");
    if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    }
}

/// Check if a process is still alive.
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slug_to_path() {
        assert_eq!(slug_to_path("-root-pup-main"), "/root/pup/main");
        assert_eq!(slug_to_path("-root"), "/root");
        assert_eq!(slug_to_path(""), "");
    }
}
