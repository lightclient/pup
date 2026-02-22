mod config;
mod setup;
mod tracing_setup;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pup_core::{ChatBackend, IncomingMessage, SessionEvent, SessionManager};
use pup_telegram::TelegramBackend;
use tokio::sync::{mpsc, watch};
use tracing::{error, info};

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
    let config = config::Config::load(cli.config.as_deref())
        .context("failed to load configuration")?;

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

    // Drop the original incoming_tx so the session manager's rx will eventually close
    // when all backends drop their clones.
    drop(incoming_tx);

    // ── Start session manager ───────────────────────────────────

    let session_manager = SessionManager::new(socket_dir, backend_txs, incoming_rx);

    session_manager.run(shutdown_rx).await?;

    info!("pup exited cleanly");
    Ok(())
}
