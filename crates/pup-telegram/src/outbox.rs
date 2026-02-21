use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, debug_span, warn, Instrument};

use crate::bot::{BotClient, SentMessage};

/// Priority for outbox operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpPriority {
    Send = 3,
    Delete = 2,
    Edit = 1,
}

impl Ord for OpPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

impl PartialOrd for OpPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An operation to be sent through the outbox.
#[derive(Debug)]
pub enum OutboxOp {
    Send {
        chat_id: i64,
        text: String,
        parse_mode: Option<String>,
        reply_markup: Option<serde_json::Value>,
        message_thread_id: Option<i64>,
        /// Callback to receive the sent message ID.
        result_tx: Option<tokio::sync::oneshot::Sender<Result<SentMessage>>>,
    },
    Edit {
        chat_id: i64,
        message_id: i64,
        text: String,
        parse_mode: Option<String>,
        reply_markup: Option<serde_json::Value>,
    },
    Delete {
        chat_id: i64,
        message_id: i64,
    },
}

impl OutboxOp {
    fn priority(&self) -> OpPriority {
        match self {
            Self::Send { .. } => OpPriority::Send,
            Self::Delete { .. } => OpPriority::Delete,
            Self::Edit { .. } => OpPriority::Edit,
        }
    }
}

/// Wrapper for heap ordering.
#[derive(Debug)]
struct HeapEntry {
    op: OutboxOp,
    priority: OpPriority,
    seq: u64, // FIFO within same priority
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for HeapEntry {}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq)) // Lower seq = older = higher priority
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Rate-limited outbox for Telegram API calls.
///
/// All Telegram API calls go through this queue. Priority: Send > Delete > Edit.
/// Rate limiting: global min interval (~33ms) and per-message edit cooldown.
#[derive(Debug)]
pub struct Outbox {
    bot: BotClient,
    queue: BinaryHeap<HeapEntry>,
    seq_counter: u64,
    min_interval: Duration,
    edit_cooldown: Duration,
    last_send: Option<Instant>,
    last_edit: HashMap<(i64, i64), Instant>,
    retry_until: Option<Instant>,
}

impl Outbox {
    pub fn new(bot: BotClient, edit_cooldown_ms: u64) -> Self {
        Self {
            bot,
            queue: BinaryHeap::new(),
            seq_counter: 0,
            min_interval: Duration::from_millis(33), // ~30 msg/sec
            edit_cooldown: Duration::from_millis(edit_cooldown_ms),
            last_send: None,
            last_edit: HashMap::new(),
            retry_until: None,
        }
    }

    /// Enqueue an operation.
    pub fn enqueue(&mut self, op: OutboxOp) {
        let priority = op.priority();
        let seq = self.seq_counter;
        self.seq_counter += 1;
        self.queue.push(HeapEntry { op, priority, seq });
    }

    /// Number of pending operations.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Clear the per-message edit cooldown so the next edit goes through
    /// immediately. Used at end-of-turn to ensure the final edit (removing
    /// the cancel keyboard) isn't blocked by a recent content edit.
    pub fn clear_edit_cooldown(&mut self) {
        self.last_edit.clear();
    }

    /// Flush one operation from the queue, respecting rate limits.
    ///
    /// Returns `true` if an operation was executed, `false` if the queue is
    /// empty or rate-limited.
    pub async fn flush_one(&mut self) -> bool {
        // Check global rate limit (429 retry).
        if let Some(until) = self.retry_until {
            if Instant::now() < until {
                return false;
            }
            self.retry_until = None;
        }

        // Check min interval.
        if let Some(last) = self.last_send {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval.checked_sub(elapsed).unwrap()).await;
            }
        }

        // Find the next eligible operation.
        let Some(entry) = self.queue.pop() else {
            return false;
        };

        // For edits, check per-message cooldown.
        if let OutboxOp::Edit {
            chat_id,
            message_id,
            ..
        } = &entry.op
        {
            let key = (*chat_id, *message_id);
            if let Some(last) = self.last_edit.get(&key)
                && last.elapsed() < self.edit_cooldown {
                    // Re-enqueue; we'll try again later.
                    self.queue.push(entry);
                    return false;
                }
        }

        let span = debug_span!("outbox_flush", queue_len = self.queue.len());
        async {
            self.last_send = Some(Instant::now());
            self.execute(entry.op).await;
            true
        }
        .instrument(span)
        .await
    }

    /// Execute a single outbox operation.
    async fn execute(&mut self, op: OutboxOp) {
        match op {
            OutboxOp::Send {
                chat_id,
                text,
                parse_mode,
                reply_markup,
                message_thread_id,
                result_tx,
            } => {
                let result = self
                    .bot
                    .send_message(
                        chat_id,
                        &text,
                        parse_mode.as_deref(),
                        reply_markup.as_ref(),
                        message_thread_id,
                    )
                    .await;
                if let Err(ref e) = result {
                    self.handle_error(e);
                }
                if let Some(tx) = result_tx {
                    let _ = tx.send(result);
                }
            }
            OutboxOp::Edit {
                chat_id,
                message_id,
                text,
                parse_mode,
                reply_markup,
            } => {
                let key = (chat_id, message_id);
                match self
                    .bot
                    .edit_message_text(
                        chat_id,
                        message_id,
                        &text,
                        parse_mode.as_deref(),
                        reply_markup.as_ref(),
                    )
                    .await
                {
                    Ok(_) => {
                        self.last_edit.insert(key, Instant::now());
                    }
                    Err(e) => {
                        self.handle_error(&e);
                    }
                }
            }
            OutboxOp::Delete {
                chat_id,
                message_id,
            } => {
                if let Err(e) = self.bot.delete_message(chat_id, message_id).await {
                    self.handle_error(&e);
                }
            }
        }
    }

    /// Handle an API error, setting retry_until for 429s.
    fn handle_error(&mut self, error: &anyhow::Error) {
        let msg = error.to_string();
        if msg.contains("rate limited") {
            // Parse retry_after from error message.
            if let Some(secs) = msg
                .split("retry after ")
                .nth(1)
                .and_then(|s| s.trim_end_matches('s').parse::<u64>().ok())
            {
                warn!(retry_after = secs, "outbox pausing for rate limit");
                self.retry_until = Some(Instant::now() + Duration::from_secs(secs));
            } else {
                self.retry_until = Some(Instant::now() + Duration::from_secs(5));
            }
        } else {
            debug!(error = %error, "outbox operation failed");
        }
    }
}
