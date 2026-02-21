use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, debug_span, info, info_span, warn, Instrument};

use crate::types::DiscoveryEvent;

/// Watches `~/.pi/pup/` for socket files and emits discovery events.
#[derive(Debug)]
pub struct Discovery {
    socket_dir: PathBuf,
    tx: mpsc::Sender<DiscoveryEvent>,
    known: HashSet<String>,
}

impl Discovery {
    pub fn new(socket_dir: PathBuf, tx: mpsc::Sender<DiscoveryEvent>) -> Self {
        Self {
            socket_dir,
            tx,
            known: HashSet::new(),
        }
    }

    /// Run the discovery loop. This does an initial scan, then watches for
    /// filesystem changes. Runs until the sender is dropped or an error occurs.
    pub async fn run(mut self) -> Result<()> {
        let span = info_span!("discovery", socket_dir = %self.socket_dir.display());
        async {
            // Ensure socket directory exists.
            tokio::fs::create_dir_all(&self.socket_dir)
                .await
                .with_context(|| {
                    format!(
                        "failed to create socket directory {}",
                        self.socket_dir.display()
                    )
                })?;

            // Initial scan.
            self.scan().await?;

            // Set up filesystem watcher.
            let (fs_tx, mut fs_rx) = mpsc::channel::<notify::Result<Event>>(64);
            let mut watcher = RecommendedWatcher::new(
                move |res| {
                    let _ = fs_tx.blocking_send(res);
                },
                notify::Config::default()
                    .with_poll_interval(Duration::from_secs(2)),
            )
            .context("failed to create filesystem watcher")?;

            watcher
                .watch(&self.socket_dir, RecursiveMode::NonRecursive)
                .with_context(|| {
                    format!("failed to watch {}", self.socket_dir.display())
                })?;

            info!("watching for socket changes");

            // Periodic rescan interval (catches missed events, e.g. after
            // the directory is deleted and recreated).
            let mut rescan_interval = tokio::time::interval(Duration::from_secs(5));
            rescan_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Skip the first immediate tick.
            rescan_interval.tick().await;

            // React to filesystem events + periodic rescan.
            loop {
                tokio::select! {
                    event = fs_rx.recv() => {
                        match event {
                            Some(Ok(event)) => {
                                self.handle_fs_event(&event).await;
                            }
                            Some(Err(e)) => {
                                warn!(error = %e, "filesystem watcher error");
                            }
                            None => {
                                debug!("filesystem watcher channel closed");
                                break;
                            }
                        }
                    }
                    _ = rescan_interval.tick() => {
                        // Re-ensure the directory exists (it may have been
                        // deleted and recreated by an extension).
                        let _ = tokio::fs::create_dir_all(&self.socket_dir).await;
                        if let Err(e) = self.scan().await {
                            debug!(error = %e, "periodic rescan failed");
                        }
                        // Re-watch in case the directory was recreated (new inode).
                        let _ = watcher.watch(&self.socket_dir, RecursiveMode::NonRecursive);
                    }
                }
            }

            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Scan the socket directory for existing `.sock` files.
    async fn scan(&mut self) -> Result<()> {
        let span = debug_span!("discovery_scan");
        async {
            let mut dir = tokio::fs::read_dir(&self.socket_dir)
                .await
                .with_context(|| {
                    format!("failed to read {}", self.socket_dir.display())
                })?;

            let mut socket_count = 0u32;
            let mut alive_count = 0u32;

            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                    continue;
                }
                socket_count += 1;

                if let Some(session_id) = socket_session_id(&path) {
                    let alive = probe_socket(&path).await;
                    debug!(session_id, alive, "probed socket");

                    if alive {
                        alive_count += 1;
                        if self.known.insert(session_id.clone()) {
                            let _ = self
                                .tx
                                .send(DiscoveryEvent::SocketAppeared {
                                    session_id,
                                    path: path.clone(),
                                })
                                .await;
                        }
                    } else {
                        // Stale socket — clean up.
                        warn!(path = %path.display(), "removing stale socket");
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                }
            }

            debug!(socket_count, alive_count, "initial scan complete");
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Handle a filesystem event from the notify watcher.
    async fn handle_fs_event(&mut self, event: &Event) {
        match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                for path in &event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                        continue;
                    }
                    if let Some(session_id) = socket_session_id(path) {
                        if self.known.contains(&session_id) {
                            continue;
                        }
                        // Small delay for the socket to become ready.
                        tokio::time::sleep(Duration::from_millis(100)).await;

                        let alive = probe_socket(path).await;
                        debug!(session_id, alive, "new socket detected");

                        if alive && self.known.insert(session_id.clone()) {
                            let _ = self
                                .tx
                                .send(DiscoveryEvent::SocketAppeared {
                                    session_id,
                                    path: path.clone(),
                                })
                                .await;
                        }
                    }
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                        continue;
                    }
                    if let Some(session_id) = socket_session_id(path)
                        && self.known.remove(&session_id) {
                            debug!(session_id, "socket removed");
                            let _ = self
                                .tx
                                .send(DiscoveryEvent::SocketRemoved { session_id })
                                .await;
                        }
                }
            }
            _ => {}
        }
    }
}

/// Extract the session ID from a socket path like `<session-id>.sock`.
fn socket_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(ToOwned::to_owned)
}

/// Probe a socket to check if it's alive. Connects and immediately disconnects.
async fn probe_socket(path: &Path) -> bool {
    let span = debug_span!("socket_probe", path = %path.display());
    async {
        match tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path)).await {
            Ok(Ok(_stream)) => true,
            Ok(Err(e)) => {
                debug!(error = %e, "probe failed");
                false
            }
            Err(_) => {
                debug!("probe timed out");
                false
            }
        }
    }
    .instrument(span)
    .await
}

/// Resolve `.alias` symlinks in the socket directory to map session names to IDs.
pub async fn resolve_aliases(socket_dir: &Path) -> Vec<(String, String)> {
    let mut aliases = Vec::new();
    let Ok(mut dir) = tokio::fs::read_dir(socket_dir).await else {
        return aliases;
    };

    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("alias") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(target) = tokio::fs::read_link(&path).await
            && let Some(session_id) = socket_session_id(&target) {
                aliases.push((name.to_owned(), session_id));
            }
    }

    aliases
}
