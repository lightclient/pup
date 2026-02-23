//! WebKit Inspector Protocol client for Bun's `BUN_INSPECT` debugger.
//!
//! This connects to the inspector WebSocket and provides `Runtime.evaluate`
//! for injecting messages into the Claude Code TUI via `process.stdin.push()`.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::debug;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsSource = SplitStream<WsStream>;

/// Client for the Bun/JSC WebKit Inspector Protocol.
#[derive(Debug)]
pub struct InspectorClient {
    url: String,
    sink: WsSink,
    source: WsSource,
    next_id: AtomicU64,
}

impl InspectorClient {
    /// Connect to an inspector WebSocket.
    ///
    /// The URL should be like `ws://127.0.0.1:9229/pup`.
    pub async fn connect(url: &str) -> Result<Self> {
        debug!(url, "connecting to inspector");

        let (ws, _response) = connect_async(url)
            .await
            .with_context(|| format!("failed to connect to inspector at {url}"))?;

        let (sink, source) = ws.split();

        let mut client = Self {
            url: url.to_owned(),
            sink,
            source,
            next_id: AtomicU64::new(1),
        };

        // Verify the connection works.
        let result = client
            .evaluate("1+1")
            .await
            .context("inspector verification failed")?;

        if result.get("value") != Some(&Value::Number(2.into())) {
            bail!("inspector verification: expected 2, got {result}");
        }

        debug!("inspector connected and verified");
        Ok(client)
    }

    /// Evaluate a JavaScript expression via `Runtime.evaluate`.
    ///
    /// Returns the `result` object from the response.
    pub async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let msg = json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": expression,
                "returnByValue": true
            }
        });

        self.sink
            .send(Message::Text(msg.to_string().into()))
            .await
            .context("failed to send to inspector")?;

        // Read responses until we get one matching our ID.
        loop {
            let frame = self
                .source
                .next()
                .await
                .context("inspector WebSocket closed")?
                .context("inspector WebSocket error")?;

            let Message::Text(text) = frame else {
                continue;
            };

            let response: Value =
                serde_json::from_str(&text).context("invalid JSON from inspector")?;

            // Skip event notifications (no "id" field).
            let Some(resp_id) = response.get("id").and_then(Value::as_u64) else {
                continue;
            };

            if resp_id != id {
                // Response for a different request — shouldn't happen in our
                // sequential usage, but skip it.
                debug!(expected = id, got = resp_id, "skipping mismatched response");
                continue;
            }

            // Check for error.
            if let Some(error) = response.get("error") {
                bail!(
                    "inspector error: {}",
                    error.get("message").and_then(Value::as_str).unwrap_or("unknown")
                );
            }

            return Ok(response
                .get("result")
                .and_then(|r| r.get("result"))
                .cloned()
                .unwrap_or(Value::Null));
        }
    }

    /// Inject text into the Claude Code TUI via `process.stdin.push()`.
    ///
    /// Sends three separate pushes:
    /// 1. `\x15` (Ctrl+U) — clear any existing input
    /// 2. The message text
    /// 3. `\r` (Enter) — submit
    ///
    /// These must be separate pushes because Ink's input handler processes
    /// each push as a discrete input event. A single push with text + `\r`
    /// doesn't reliably trigger submit.
    pub async fn inject_stdin(&mut self, text: &str) -> Result<()> {
        // Step 1: Clear any existing input (Ctrl+U = 0x15).
        // Send Ctrl+U twice to ensure the input is fully cleared even if
        // the TUI is in an intermediate state (e.g., after an interruption).
        self.evaluate("process.stdin.push(Buffer.from('15', 'hex'))")
            .await
            .context("stdin clear failed")?;

        // Small delay to let Ink process the clear event before we push text.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.evaluate("process.stdin.push(Buffer.from('15', 'hex'))")
            .await
            .context("stdin clear 2 failed")?;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Step 2: Push the message text (hex-encoded to avoid escaping issues).
        let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
        let expression = format!("process.stdin.push(Buffer.from('{hex}', 'hex'))");
        self.evaluate(&expression)
            .await
            .context("stdin text push failed")?;

        // Small delay before Enter to let Ink process the text input.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Step 3: Push Enter to submit (CR = 0x0d).
        let result = self
            .evaluate("process.stdin.push(Buffer.from('0d', 'hex'))")
            .await
            .context("stdin enter push failed")?;

        debug!(?result, "stdin injection complete");
        Ok(())
    }

    /// Send Escape (`\x1b` = 0x1b) to cancel the current operation.
    pub async fn inject_escape(&mut self) -> Result<()> {
        self.evaluate("process.stdin.push(Buffer.from('1b', 'hex'))")
            .await
            .context("escape injection failed")?;
        Ok(())
    }

    /// Check if the connection is still alive.
    pub async fn ping(&mut self) -> bool {
        self.evaluate("1").await.is_ok()
    }

    /// Get the inspector URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}
