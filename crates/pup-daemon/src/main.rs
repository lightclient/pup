mod config;
mod setup;
mod tracing_setup;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pup_core::{
    AgentHandle, EventBus, IncomingMessage, MessageRouter, SessionManager, new_registry,
};
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

    // ── Event bus: agent backends → chat channels ───────────────

    let mut event_bus = EventBus::new();

    // ── Message router: chat channels → agent backends ──────────

    let mut router = MessageRouter::new();
    let (message_tx, message_rx) = mpsc::channel::<IncomingMessage>(64);

    // ── Initialize pi session manager ───────────────────────────

    let pi_registry = new_registry();
    let (pi_msg_tx, pi_msg_rx) = mpsc::channel::<IncomingMessage>(64);

    router.add_agent(AgentHandle {
        name: "pi",
        message_tx: pi_msg_tx,
        registry: pup_core::SessionRegistry::clone(&pi_registry),
        is_default: true, // pi is the fallback for unknown sessions
    });

    // ── Initialize Claude Code service ──────────────────────────

    if config.claude_code.enabled {
        let projects_dir = config.claude_projects_dir();
        info!(projects_dir = %projects_dir.display(), "starting Claude Code service");

        let (service, cc_msg_tx, cc_registry) =
            pup_claude::ClaudeService::new(projects_dir, event_bus.sender());

        router.add_agent(AgentHandle {
            name: "claude",
            message_tx: cc_msg_tx,
            registry: cc_registry,
            is_default: false,
        });

        let shutdown_rx_cc = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = service.run(shutdown_rx_cc).await {
                error!(error = %e, "Claude Code service failed");
            }
        });
    }

    // ── Initialize Telegram chat channel ────────────────────────

    if let Some(tg_config) = config.telegram_config() {
        let event_rx = event_bus.subscribe();
        let message_tx_clone = message_tx.clone();
        let shutdown_rx_clone = shutdown_rx.clone();

        tokio::spawn(async move {
            let backend = TelegramBackend::new(tg_config);
            pup_core::ChatChannel::run(backend, event_rx, message_tx_clone, shutdown_rx_clone)
                .await;
        });
    } else {
        info!("no telegram backend configured");
    }

    // Drop the original message_tx so the router stops when all channels exit.
    drop(message_tx);

    // ── Start the event bus ─────────────────────────────────────

    // Drop the bus sender from main so the bus stops when all agent backends exit.
    let bus_sender = event_bus.sender();
    tokio::spawn(async move {
        event_bus.run().await;
    });

    // ── Start the message router ────────────────────────────────

    tokio::spawn(async move {
        router.run(message_rx).await;
    });

    // ── Start pi session manager ────────────────────────────────

    let session_manager = SessionManager::new(socket_dir, bus_sender, pi_msg_rx, pi_registry);

    session_manager.run(shutdown_rx).await?;

    info!("pup exited cleanly");
    Ok(())
}
