mod config;
mod setup;
mod tracing_setup;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pup_claude::injector::{ClaudeCommand, ClaudeService, SessionRegistry};
use pup_core::{ChatBackend, IncomingMessage, SessionEvent, SessionManager};
use pup_telegram::TelegramBackend;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "pup", about = "Pickup your pi sessions on the go")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config file (default: ~/.config/pup/config.toml)
    #[arg(long, short)]
    config: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Interactive setup wizard
    Setup,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Setup) => {
            setup::run_setup().await?;
            return Ok(());
        }
        None => {}
    }

    // Initialize tracing.
    let _tracing_guard = tracing_setup::init()?;

    // Load config.
    let config =
        config::Config::load(cli.config.as_deref()).context("failed to load configuration")?;

    let socket_dir = config.socket_dir();
    info!(
        config_path = %cli.config.as_deref().map_or_else(|| "default".to_owned(), |p| p.display().to_string()),
        socket_dir = %socket_dir.display(),
        "starting pup"
    );

    // Set up shutdown signal.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("received shutdown signal");
        let _ = shutdown_tx.send(true);
    });

    // Set up backend channels.
    // Incoming messages from backends go through a router that dispatches to
    // either the pi session manager or the Claude Code service.
    let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingMessage>(64);
    let mut backend_txs: Vec<mpsc::Sender<SessionEvent>> = Vec::new();

    // ── Initialize Telegram backend ─────────────────────────────

    if let Some(tg_config) = config.telegram_config() {
        let (event_tx, mut event_rx) = mpsc::channel::<SessionEvent>(256);
        backend_txs.push(event_tx);

        let incoming_tx_clone = incoming_tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();

        tokio::spawn(async move {
            let mut backend = TelegramBackend::new(tg_config);

            // Initialize.
            if let Err(e) = backend.init().await {
                error!(error = %e, "telegram backend init failed");
                return;
            }

            info!("telegram backend started");

            // Main backend loop.
            loop {
                let mut shutdown_watch = shutdown_rx_clone.clone();
                tokio::select! {
                    // Handle session events.
                    Some(event) = event_rx.recv() => {
                        if let Err(e) = backend.handle_event(event).await {
                            error!(error = %e, "telegram handle_event failed");
                        }
                    }
                    // Poll for incoming Telegram messages.
                    result = backend.recv_incoming() => {
                        match result {
                            Ok(Some(msg)) => {
                                let _ = incoming_tx_clone.send(msg).await;
                            }
                            Ok(None) => {
                                info!("telegram backend shut down");
                                break;
                            }
                            Err(e) => {
                                error!(error = %e, "telegram recv_incoming failed");
                            }
                        }
                    }
                    // Shutdown.
                    _ = shutdown_watch.changed() => {
                        if *shutdown_watch.borrow() {
                            let _ = backend.shutdown().await;
                            break;
                        }
                    }
                }
            }
        });
    } else {
        info!("no telegram backend configured");
    }

    // Drop the original incoming_tx so channels close when all backends exit.
    drop(incoming_tx);

    // ── Initialize Claude Code service ──────────────────────────

    let mut cc_cmd_tx: Option<mpsc::Sender<ClaudeCommand>> = None;
    let mut cc_registry: Option<SessionRegistry> = None;

    if config.claude_code.enabled {
        let projects_dir = config.claude_projects_dir();
        info!(projects_dir = %projects_dir.display(), "starting Claude Code service");

        // Create an event channel for the CC service. A bridge task fans out
        // CC events to all backend channels.
        let (cc_event_tx, mut cc_event_rx) = mpsc::channel::<SessionEvent>(256);
        let cc_backend_txs: Vec<mpsc::Sender<SessionEvent>> =
            backend_txs.iter().map(mpsc::Sender::clone).collect();

        // Fan-out bridge: CC events → all backends.
        tokio::spawn(async move {
            while let Some(event) = cc_event_rx.recv().await {
                for tx in &cc_backend_txs {
                    if tx.send(event.clone()).await.is_err() {
                        warn!("CC event fan-out: backend channel closed");
                    }
                }
            }
        });

        let (service, cmd_tx, registry) = ClaudeService::new(projects_dir, cc_event_tx);
        cc_cmd_tx = Some(cmd_tx);
        cc_registry = Some(registry);

        let shutdown_rx_cc = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = service.run(shutdown_rx_cc).await {
                error!(error = %e, "Claude Code service failed");
            }
        });
    }

    // ── Message router ──────────────────────────────────────────
    //
    // Routes incoming messages from backends to either:
    // - Claude Code service (if session_id is a CC session)
    // - Pi session manager (otherwise)

    let (pi_incoming_tx, pi_incoming_rx) = mpsc::channel::<IncomingMessage>(64);

    {
        let cc_cmd = cc_cmd_tx.clone();
        let cc_reg = cc_registry.clone();

        tokio::spawn(async move {
            let mut incoming_rx = incoming_rx;
            while let Some(msg) = incoming_rx.recv().await {
                let is_cc = cc_reg
                    .as_ref()
                    .and_then(|r| r.read().ok())
                    .is_some_and(|set| set.contains(&msg.session_id));

                if is_cc {
                    if let Some(cc_tx) = &cc_cmd {
                        if msg.is_cancel {
                            let _ = cc_tx
                                .send(ClaudeCommand::Cancel {
                                    session_id: msg.session_id,
                                })
                                .await;
                        } else {
                            // Skip pi-specific slash commands that shouldn't
                            // be injected into the Claude Code TUI.
                            let text = msg.text.trim();
                            if text.starts_with("/name")
                                || text.starts_with("/compact")
                                || text.starts_with("/new")
                                || text.starts_with("/exit")
                            {
                                info!(
                                    session_id = msg.session_id,
                                    text, "skipping pi slash command for Claude Code session"
                                );
                            } else {
                                let (reply_tx, mut reply_rx) = mpsc::channel(1);
                                let _ = cc_tx
                                    .send(ClaudeCommand::InjectMessage {
                                        session_id: msg.session_id.clone(),
                                        text: msg.text,
                                        reply: reply_tx,
                                    })
                                    .await;

                                // Check result asynchronously.
                                let sid = msg.session_id;
                                tokio::spawn(async move {
                                    if let Some(Err(e)) = reply_rx.recv().await {
                                        warn!(
                                            session_id = sid,
                                            error = e,
                                            "Claude Code injection failed"
                                        );
                                    }
                                });
                            }
                        }
                    }
                } else if pi_incoming_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });
    }

    // ── Start session manager (pi sessions) ─────────────────────

    let session_manager = SessionManager::new(socket_dir, backend_txs, pi_incoming_rx);

    session_manager.run(shutdown_rx).await?;

    info!("pup exited cleanly");
    Ok(())
}
