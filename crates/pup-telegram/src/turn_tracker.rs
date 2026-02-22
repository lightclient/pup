use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::bot::BotClient;
use crate::outbox::{Outbox, OutboxOp};
use crate::render::{
    cancel_keyboard, empty_keyboard, escape_html, split_message, to_telegram_html, MAX_BODY_CHARS,
};

/// A tracked tool call for verbose rendering.
#[derive(Debug)]
struct TrackedTool {
    tool_name: String,
    args: serde_json::Value,
    #[allow(dead_code)]
    content: Option<String>,
    is_error: bool,
    done: bool,
}

/// State for one agent turn in one session.
#[derive(Debug)]
struct TurnState {
    /// Chat ID where this message lives.
    chat_id: i64,
    /// Thread ID for topic mode (None for DMs).
    thread_id: Option<i64>,
    /// Session ID (for cancel button).
    session_id: String,
    /// Telegram message ID of our single status message (set after send).
    telegram_message_id: Option<i64>,
    /// Whether the initial send is still in flight.
    send_pending: bool,
    /// Channel to receive the sent message ID.
    send_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<crate::bot::SentMessage>>>,
    /// Accumulated streaming text for the current assistant message.
    streaming_text: String,
    /// Last time we sent an edit to Telegram.
    last_edit: Instant,
    /// Whether content has changed since the last edit.
    dirty: bool,
    /// Sender to stop the typing indicator loop (dropped on turn end).
    typing_stop: Option<tokio::sync::watch::Sender<bool>>,
    /// Whether to show tool call details.
    verbose: bool,
    /// Tracked tool calls in order.
    tools: Vec<TrackedTool>,
}

impl TurnState {
    /// Try to resolve the sent message ID if the send is still pending.
    fn try_resolve_message_id(&mut self) {
        if !self.send_pending {
            return;
        }
        if let Some(mut rx) = self.send_rx.take() {
            match rx.try_recv() {
                Ok(Ok(sent)) => {
                    self.telegram_message_id = Some(sent.message_id);
                    self.send_pending = false;
                }
                Ok(Err(_)) => {
                    self.send_pending = false;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    self.send_rx = Some(rx);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.send_pending = false;
                }
            }
        }
    }

    /// Render the current turn state as Telegram HTML.
    fn render(&self) -> String {
        let mut parts = Vec::new();

        // Verbose tool call summaries.
        if self.verbose {
            for tool in &self.tools {
                let mut line = format!("🔧 <b>{}</b>", escape_html(&tool.tool_name));
                // Show command or path arg if present.
                if let Some(cmd) = tool.args.get("command").and_then(|v| v.as_str()) {
                    let truncated = if cmd.len() > 200 { &cmd[..200] } else { cmd };
                    line.push_str(&format!("\n<pre>{}</pre>", escape_html(truncated)));
                } else if let Some(path) = tool.args.get("path").and_then(|v| v.as_str()) {
                    line.push_str(&format!(" <code>{}</code>", escape_html(path)));
                }
                if tool.is_error {
                    line.push_str(" ❌");
                } else if tool.done {
                    line.push_str(" ✓");
                } else {
                    line.push_str(" ⏳");
                }
                parts.push(line);
            }
        }

        // Streaming text.
        if !self.streaming_text.is_empty() {
            let html = to_telegram_html(&self.streaming_text);
            if !html.is_empty() {
                parts.push(html);
            }
        }

        if parts.is_empty() {
            "…".to_owned()
        } else {
            parts.join("\n\n")
        }
    }

    /// Enqueue an edit (or initial send) for the current state.
    fn flush(&mut self, outbox: &mut Outbox, edit_interval_ms: u64) {
        if !self.dirty {
            return;
        }

        // Throttle edits.
        let elapsed = self.last_edit.elapsed().as_millis() as u64;
        if elapsed < edit_interval_ms {
            return;
        }

        self.try_resolve_message_id();

        let Some(tg_msg_id) = self.telegram_message_id else {
            return;
        };

        let rendered = self.render();
        let chunks = split_message(&rendered, MAX_BODY_CHARS);
        let display = chunks.first().cloned().unwrap_or_else(|| "…".to_owned());

        outbox.enqueue(OutboxOp::Edit {
            chat_id: self.chat_id,
            message_id: tg_msg_id,
            text: display,
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(cancel_keyboard(&self.session_id)),
        });

        self.last_edit = Instant::now();
        self.dirty = false;
    }
}

/// Manages per-session turn state — one Telegram message per agent turn.
#[derive(Debug)]
pub struct TurnTracker {
    /// Active turns keyed by session_id.
    turns: HashMap<String, TurnState>,
    /// Minimum interval between edits.
    edit_interval_ms: u64,
    /// Whether to show tool call details.
    verbose: bool,
}

impl TurnTracker {
    pub fn new(edit_interval_ms: u64) -> Self {
        Self {
            turns: HashMap::new(),
            edit_interval_ms,
            verbose: false,
        }
    }

    /// Enable or disable verbose mode (tool call visibility).
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    /// Check if a turn is being tracked for the given session.
    pub fn has_turn(&self, session_id: &str) -> bool {
        self.turns.contains_key(session_id)
    }

    /// Start tracking a new agent turn.
    ///
    /// If `existing_typing` is provided (e.g. a pre-turn typing loop that
    /// was already running), it is reused instead of spawning a new one.
    /// Otherwise a fresh background typing indicator loop is spawned.
    /// Does NOT send a content message yet — that happens on the first
    /// tool/delta event.
    pub fn start_turn(
        &mut self,
        session_id: &str,
        chat_id: i64,
        thread_id: Option<i64>,
        bot: &BotClient,
        existing_typing: Option<tokio::sync::watch::Sender<bool>>,
    ) {
        let stop_tx = if let Some(tx) = existing_typing {
            tx
        } else {
            // Spawn typing indicator loop. Dropping the tx end stops it.
            let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
            let bot = bot.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = bot.send_chat_action(chat_id, "typing", thread_id).await {
                        debug!(error = %e, "typing indicator failed");
                    }
                    tokio::select! {
                        _ = stop_rx.changed() => break,
                        _ = tokio::time::sleep(Duration::from_secs(4)) => {}
                    }
                }
            });
            stop_tx
        };

        self.turns.insert(
            session_id.to_owned(),
            TurnState {
                chat_id,
                thread_id,
                session_id: session_id.to_owned(),
                telegram_message_id: None,
                send_pending: false,
                send_rx: None,
                streaming_text: String::new(),
                last_edit: Instant::now(),
                dirty: false,
                typing_stop: Some(stop_tx),
                verbose: self.verbose,
                tools: Vec::new(),
            },
        );
    }

    /// Ensure the turn's Telegram message exists; send it if not.
    fn ensure_message(&mut self, session_id: &str, initial_text: &str, outbox: &mut Outbox) {
        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        // Already sent or pending.
        if state.telegram_message_id.is_some() || state.send_pending {
            return;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();

        outbox.enqueue(OutboxOp::Send {
            chat_id: state.chat_id,
            text: initial_text.to_owned(),
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(cancel_keyboard(&state.session_id)),
            message_thread_id: state.thread_id,
            result_tx: Some(tx),
        });

        state.send_pending = true;
        state.send_rx = Some(rx);
        state.last_edit = Instant::now();
    }

    /// Note that a tool started. Ensures the Telegram message exists.
    pub fn tool_start(
        &mut self,
        session_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        outbox: &mut Outbox,
    ) {
        if let Some(state) = self.turns.get_mut(session_id) {
            if state.verbose {
                state.tools.push(TrackedTool {
                    tool_name: tool_name.to_owned(),
                    args: args.clone(),
                    content: None,
                    is_error: false,
                    done: false,
                });
                state.dirty = true;
            }
        }
        self.ensure_message(session_id, "…", outbox);
        // Flush the tool start state.
        if let Some(state) = self.turns.get_mut(session_id) {
            state.flush(outbox, self.edit_interval_ms);
        }
    }

    /// Note that a tool finished.
    pub fn tool_end(
        &mut self,
        session_id: &str,
        tool_name: &str,
        is_error: bool,
        outbox: &mut Outbox,
    ) {
        if let Some(state) = self.turns.get_mut(session_id) {
            if state.verbose {
                // Find the last matching tool and mark it done.
                for tool in state.tools.iter_mut().rev() {
                    if tool.tool_name == tool_name && !tool.done {
                        tool.done = true;
                        tool.is_error = is_error;
                        break;
                    }
                }
                state.dirty = true;
                state.flush(outbox, self.edit_interval_ms);
            }
        }
    }

    /// Accumulate a streaming text delta.
    pub fn message_delta(&mut self, session_id: &str, text: &str, outbox: &mut Outbox) {
        // If no message sent yet, send with the first chunk of text.
        let initial = {
            let needs_send = self
                .turns
                .get(session_id)
                .map_or(false, |s| s.telegram_message_id.is_none() && !s.send_pending);
            if needs_send {
                // Render current state (tools + text) for the initial send.
                let state = self.turns.get(session_id).unwrap();
                let mut preview = state.render();
                let delta_html = to_telegram_html(text);
                if !delta_html.is_empty() {
                    if !preview.is_empty() {
                        preview.push_str("\n\n");
                    }
                    preview.push_str(&delta_html);
                }
                if preview.is_empty() {
                    preview = "…".to_owned();
                }
                Some(preview)
            } else {
                None
            }
        };
        if let Some(initial_text) = initial {
            self.ensure_message(session_id, &initial_text, outbox);
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        state.streaming_text.push_str(text);
        state.dirty = true;
        state.flush(outbox, self.edit_interval_ms);
    }

    /// Handle the end of a streaming message. Sets the complete final content
    /// and forces an edit so the message is up to date, but does NOT finalize
    /// the turn (more tools/messages may follow).
    pub fn message_end_with_content(
        &mut self,
        session_id: &str,
        content: &str,
        outbox: &mut Outbox,
    ) {
        // If no deltas were received (e.g. very short response with extended
        // thinking), ensure a Telegram message is created with the final content.
        if !content.is_empty() {
            let html = to_telegram_html(content);
            let initial = if html.is_empty() { "…".to_owned() } else { html };
            self.ensure_message(session_id, &initial, outbox);
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        if !content.is_empty() {
            state.streaming_text = content.to_owned();
        }
        state.dirty = true;
        // Force an immediate edit (bypass throttle) so the final content
        // is shown promptly.
        state.flush(outbox, 0);
    }

    /// Finalize the turn: send the last edit with the complete content
    /// and remove the cancel keyboard.
    pub fn end_turn(&mut self, session_id: &str, outbox: &mut Outbox) {
        let Some(mut state) = self.turns.remove(session_id) else {
            return;
        };

        // Stop the typing indicator (dropping the sender closes the channel).
        state.typing_stop.take();

        // Resolve message ID if still pending.
        state.try_resolve_message_id();

        // Final render.
        let rendered = state.render();
        let chunks = split_message(&rendered, MAX_BODY_CHARS);

        if let Some(tg_msg_id) = state.telegram_message_id {
            // Edit the existing message with final content and remove cancel keyboard.
            if let Some(first) = chunks.first() {
                outbox.enqueue(OutboxOp::Edit {
                    chat_id: state.chat_id,
                    message_id: tg_msg_id,
                    text: first.clone(),
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: Some(empty_keyboard()),
                });
            }

            // Overflow chunks as separate messages.
            for chunk in chunks.iter().skip(1) {
                outbox.enqueue(OutboxOp::Send {
                    chat_id: state.chat_id,
                    text: chunk.clone(),
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: None,
                    message_thread_id: state.thread_id,
                    result_tx: None,
                });
            }
        } else if !state.streaming_text.is_empty() {
            // No Telegram message was ever created (e.g. no deltas arrived,
            // or the send is still in flight). Send the final content as a
            // new message without a cancel keyboard.
            for (i, chunk) in chunks.iter().enumerate() {
                outbox.enqueue(OutboxOp::Send {
                    chat_id: state.chat_id,
                    text: chunk.clone(),
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: if i == 0 { Some(empty_keyboard()) } else { None },
                    message_thread_id: state.thread_id,
                    result_tx: None,
                });
            }
        }
    }

    /// Get all session IDs with active turns.
    pub fn active_sessions(&self) -> Vec<String> {
        self.turns.keys().cloned().collect()
    }
}
