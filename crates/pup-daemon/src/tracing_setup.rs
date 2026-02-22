use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Guard that must be held alive for non-blocking file writer flush.
pub(crate) struct TracingGuard {
    _guards: Vec<WorkerGuard>,
}

impl std::fmt::Debug for TracingGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracingGuard").finish()
    }
}

/// Initialize the tracing subscriber with layered outputs.
///
/// - Console layer: always active, controlled by `RUST_LOG` (default: `info`).
/// - JSONL file layer: activated by `PUP_TRACE_FILE` env var.
pub(crate) fn init() -> Result<TracingGuard> {
    let mut guards = Vec::new();

    // Console layer.
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = fmt::layer()
        .compact()
        .with_target(false)
        .with_filter(console_filter);

    // JSONL file layer (optional).
    if let Ok(trace_file) = std::env::var("PUP_TRACE_FILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace_file)
            .with_context(|| format!("failed to open trace file: {trace_file}"))?;

        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        guards.push(guard);

        let file_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));

        let file_layer = fmt::layer()
            .json()
            .with_span_list(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(non_blocking)
            .with_filter(file_filter);

        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(console_layer).init();
    }

    Ok(TracingGuard { _guards: guards })
}
