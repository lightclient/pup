use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::bot::BotClient;
use crate::outbox::{Outbox, OutboxOp};
use crate::render::{
    cancel_keyboard, empty_keyboard, split_message, to_telegram_html, MAX_BODY_CHARS,
};

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
        // Show streaming text if we have any, otherwise a placeholder.
        if !self.streaming_text.is_empty() {
            let html = to_telegram_html(&self.streaming_text);
            if !html.is_empty() {
                return html;
            }
        }
        "…".to_owned()
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
}

impl TurnTracker {
    pub fn new(edit_interval_ms: u64) -> Self {
        Self {
            turns: HashMap::new(),
            edit_interval_ms,
        }
    }

    /// Start tracking a new agent turn.
    ///
    /// Spawns a background typing indicator loop that keeps the "typing…"
    /// status alive in Telegram until the turn ends. Does NOT send a
    /// content message yet — that happens on the first tool/delta event.
    pub fn start_turn(
        &mut self,
        session_id: &str,
        chat_id: i64,
        thread_id: Option<i64>,
        bot: &BotClient,
    ) {
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
    pub fn tool_start(&mut self, session_id: &str, _tool_name: &str, outbox: &mut Outbox) {
        self.ensure_message(session_id, "…", outbox);
    }

    /// Note that a tool finished. No-op (typing indicator covers visibility).
    pub fn tool_end(
        &mut self,
        _session_id: &str,
        _tool_name: &str,
        _is_error: bool,
        _outbox: &mut Outbox,
    ) {
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

        let Some(tg_msg_id) = state.telegram_message_id else {
            return;
        };

        // Final render.
        let rendered = state.render();
        let chunks = split_message(&rendered, MAX_BODY_CHARS);

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
    }

    /// Check if a session has an active turn.
    pub fn has_turn(&self, session_id: &str) -> bool {
        self.turns.contains_key(session_id)
    }

    /// Get all session IDs with active turns.
    pub fn active_sessions(&self) -> Vec<String> {
        self.turns.keys().cloned().collect()
    }
}
