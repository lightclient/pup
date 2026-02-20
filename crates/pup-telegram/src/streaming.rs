use std::collections::HashMap;
use std::time::Instant;

use crate::outbox::{Outbox, OutboxOp};
use crate::render::{cancel_keyboard, empty_keyboard, split_message, to_telegram_html, MAX_BODY_CHARS};

/// Tracks in-progress streaming messages for a session.
#[derive(Debug)]
pub struct StreamState {
    /// Accumulated text so far.
    pub text: String,
    /// Telegram message ID of the placeholder (set after send).
    pub telegram_message_id: Option<i64>,
    /// Chat ID where this message lives.
    pub chat_id: i64,
    /// Session ID for cancel button.
    pub session_id: String,
    /// Last time we sent an edit.
    pub last_edit: Instant,
    /// Number of edits sent.
    pub edit_count: u32,
    /// Whether the send request is still in-flight.
    pub send_pending: bool,
    /// Channel to receive the sent message ID.
    pub send_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<crate::bot::SentMessage>>>,
}

/// Manages streaming state across all sessions.
#[derive(Debug)]
pub struct StreamingManager {
    /// Active streams keyed by IPC message_id.
    streams: HashMap<String, StreamState>,
    /// Minimum interval between edits.
    edit_interval_ms: u64,
}

impl StreamingManager {
    pub fn new(edit_interval_ms: u64) -> Self {
        Self {
            streams: HashMap::new(),
            edit_interval_ms,
        }
    }

    /// Start a new stream: send a placeholder message.
    pub fn start(
        &mut self,
        message_id: &str,
        session_id: &str,
        chat_id: i64,
        outbox: &mut Outbox,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();

        outbox.enqueue(OutboxOp::Send {
            chat_id,
            text: "⏳".to_owned(),
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(cancel_keyboard(session_id)),
            message_thread_id: None,
            result_tx: Some(tx),
        });

        self.streams.insert(
            message_id.to_owned(),
            StreamState {
                text: String::new(),
                telegram_message_id: None,
                chat_id,
                session_id: session_id.to_owned(),
                last_edit: Instant::now(),
                edit_count: 0,
                send_pending: true,
                send_rx: Some(rx),
            },
        );
    }

    /// Start a new stream in a forum topic.
    pub fn start_in_topic(
        &mut self,
        message_id: &str,
        session_id: &str,
        chat_id: i64,
        thread_id: i64,
        outbox: &mut Outbox,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();

        outbox.enqueue(OutboxOp::Send {
            chat_id,
            text: "⏳".to_owned(),
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(cancel_keyboard(session_id)),
            message_thread_id: Some(thread_id),
            result_tx: Some(tx),
        });

        self.streams.insert(
            message_id.to_owned(),
            StreamState {
                text: String::new(),
                telegram_message_id: None,
                chat_id,
                session_id: session_id.to_owned(),
                last_edit: Instant::now(),
                edit_count: 0,
                send_pending: true,
                send_rx: Some(rx),
            },
        );
    }

    /// Accumulate a text delta. Enqueues an edit if enough time has passed.
    pub fn delta(&mut self, message_id: &str, text: &str, outbox: &mut Outbox) {
        let Some(state) = self.streams.get_mut(message_id) else {
            return;
        };

        // Try to resolve the sent message ID if pending.
        if state.send_pending
            && let Some(mut rx) = state.send_rx.take() {
                match rx.try_recv() {
                    Ok(Ok(sent)) => {
                        state.telegram_message_id = Some(sent.message_id);
                        state.send_pending = false;
                    }
                    Ok(Err(_)) => {
                        state.send_pending = false;
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                        state.send_rx = Some(rx);
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        state.send_pending = false;
                    }
                }
            }

        state.text.push_str(text);

        // Check if enough time has passed for an edit.
        let elapsed = state.last_edit.elapsed().as_millis() as u64;
        if elapsed < self.edit_interval_ms {
            return;
        }

        let Some(tg_msg_id) = state.telegram_message_id else {
            return;
        };

        // Render and enqueue edit.
        let html = to_telegram_html(&state.text);
        let chunks = split_message(&html, MAX_BODY_CHARS);
        // Only edit with the first chunk during streaming (keep it simple).
        let display = chunks.first().cloned().unwrap_or_default();

        outbox.enqueue(OutboxOp::Edit {
            chat_id: state.chat_id,
            message_id: tg_msg_id,
            text: display,
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(cancel_keyboard(&state.session_id)),
        });

        state.last_edit = Instant::now();
        state.edit_count += 1;
    }

    /// Finalize a stream: send the final edit with complete content.
    pub fn end(&mut self, message_id: &str, content: &str, outbox: &mut Outbox) {
        let Some(mut state) = self.streams.remove(message_id) else {
            return;
        };

        // Resolve message ID if still pending.
        if state.send_pending
            && let Some(mut rx) = state.send_rx.take()
                && let Ok(Ok(sent)) = rx.try_recv() {
                    state.telegram_message_id = Some(sent.message_id);
                }

        let Some(tg_msg_id) = state.telegram_message_id else {
            return;
        };

        let html = to_telegram_html(content);
        let chunks = split_message(&html, MAX_BODY_CHARS);

        if let Some(first) = chunks.first() {
            // Final edit removes cancel keyboard.
            outbox.enqueue(OutboxOp::Edit {
                chat_id: state.chat_id,
                message_id: tg_msg_id,
                text: first.clone(),
                parse_mode: Some("HTML".to_owned()),
                reply_markup: Some(empty_keyboard()),
            });
        }

        // Send additional chunks as new messages.
        for chunk in chunks.iter().skip(1) {
            outbox.enqueue(OutboxOp::Send {
                chat_id: state.chat_id,
                text: chunk.clone(),
                parse_mode: Some("HTML".to_owned()),
                reply_markup: None,
                message_thread_id: None,
                result_tx: None,
            });
        }
    }

    /// Check if a message is currently streaming.
    pub fn is_streaming(&self, message_id: &str) -> bool {
        self.streams.contains_key(message_id)
    }

    /// Get all active stream session IDs.
    pub fn active_sessions(&self) -> Vec<String> {
        self.streams
            .values()
            .map(|s| s.session_id.clone())
            .collect()
    }
}
