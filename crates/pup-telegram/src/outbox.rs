use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{Instrument, debug, debug_span, warn};

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

    fn chat_id(&self) -> i64 {
        match self {
            Self::Send { chat_id, .. }
            | Self::Edit { chat_id, .. }
            | Self::Delete { chat_id, .. } => *chat_id,
        }
    }
}

/// Wrapper for heap ordering (sends and deletes only).
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

/// Per-chat token bucket for rate limiting.
///
/// Telegram Bot API limits
/// (https://core.telegram.org/bots/faq#my-bot-is-hitting-limits-how-do-i-avoid-this):
///   - Single chat: avoid more than ~1 message/second (bursts ok, then 429s)
///   - Groups: no more than 20 messages per minute
///   - Bulk broadcast: ~30 messages/second globally
///
/// The group limit (20/min) is the binding constraint for supergroups with
/// topics. We target 18/min (one every 3.33s) to leave headroom.
#[derive(Debug)]
struct TokenBucket {
    /// Available tokens (fractional, accumulates over time).
    tokens: f64,
    /// Maximum burst size.
    capacity: f64,
    /// Tokens added per second (= capacity / 60 for a per-minute rate).
    rate: f64,
    /// Last time tokens were refilled.
    last_refill: Instant,
}

/// Default per-chat budget: 18 operations per minute.
const DEFAULT_CHAT_BUDGET: f64 = 18.0;

/// Shared per-chat rate limiter.
///
/// All Telegram API calls for a given chat should go through this so that
/// the token budget is shared between the outbox (messages, edits, deletes)
/// and background tasks (typing indicators, topic management).
#[derive(Debug, Clone)]
pub struct ChatBudget(Arc<Mutex<HashMap<i64, TokenBucket>>>);

impl ChatBudget {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Try to consume one token for `chat_id`. Returns `true` if allowed.
    pub fn try_consume(&self, chat_id: i64) -> bool {
        self.0
            .lock()
            .expect("chat budget lock poisoned")
            .entry(chat_id)
            .or_insert_with(|| TokenBucket::new(DEFAULT_CHAT_BUDGET))
            .try_consume()
    }
}

impl Default for ChatBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenBucket {
    fn new(per_minute: f64) -> Self {
        Self {
            tokens: per_minute, // start full
            capacity: per_minute,
            rate: per_minute / 60.0,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
    }

    /// Try to consume one token. Returns `true` if successful.
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rate-limited outbox for Telegram API calls.
///
/// All Telegram API calls go through this queue. Priority: Send > Delete > Edit.
///
/// Rate limiting is layered:
///   1. **Global min interval** (~33ms between any two API calls)
///   2. **Per-message edit cooldown** (configurable, default 1500ms)
///   3. **Per-chat token bucket** (18 ops/min, refills smoothly)
///
/// Edits are coalesced: only the latest edit per `(chat_id, message_id)` is
/// kept. Superseded edits are silently dropped.
///
/// When the top-priority operation is blocked (budget or cooldown), lower
/// entries and other chats are still tried — no head-of-line blocking.
#[derive(Debug)]
pub struct Outbox {
    bot: BotClient,
    /// Priority queue for sends and deletes.
    queue: BinaryHeap<HeapEntry>,
    /// Coalesced edits: only the latest per message.
    pending_edits: EditMap,
    seq_counter: u64,
    min_interval: Duration,
    edit_cooldown: Duration,
    last_send: Option<Instant>,
    last_edit: HashMap<(i64, i64), Instant>,
    retry_until: Option<Instant>,
    /// Shared per-chat token buckets (also used by typing indicator tasks).
    chat_budget: ChatBudget,
}

/// Coalesced edit map — stores only the latest edit per message.
///
/// Edits are keyed by `(chat_id, message_id)`. An insertion-order queue
/// tracks which messages have pending edits so they can be flushed FIFO.
#[derive(Debug, Default)]
struct EditMap {
    /// Latest edit content per message.
    entries: HashMap<(i64, i64), OutboxOp>,
    /// FIFO order of distinct messages with pending edits.
    order: VecDeque<(i64, i64)>,
}

impl EditMap {
    /// Insert or replace an edit for a message.
    fn upsert(&mut self, key: (i64, i64), op: OutboxOp) {
        if self.entries.insert(key, op).is_none() {
            // New message — add to FIFO order.
            self.order.push_back(key);
        }
        // If the key already existed, only the content is replaced; FIFO
        // position stays the same.
    }

    /// Pop the next eligible edit (respecting per-message cooldown and
    /// per-chat budget).
    ///
    /// Returns `None` if there are no edits, or all are blocked.
    fn pop_eligible(
        &mut self,
        last_edit: &HashMap<(i64, i64), Instant>,
        cooldown: Duration,
        chat_budget: &ChatBudget,
    ) -> Option<OutboxOp> {
        let len = self.order.len();
        for _ in 0..len {
            let Some(key) = self.order.pop_front() else {
                break;
            };

            // Per-message cooldown.
            let cooldown_blocked = last_edit.get(&key).is_some_and(|t| t.elapsed() < cooldown);
            if cooldown_blocked {
                self.order.push_back(key);
                continue;
            }

            // Per-chat budget.
            let chat_id = key.0;
            if !chat_budget.try_consume(chat_id) {
                self.order.push_back(key);
                continue;
            }

            // Found an eligible edit.
            if let Some(op) = self.entries.remove(&key) {
                return Some(op);
            }
            // Entry was removed externally (shouldn't happen), skip.
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Outbox {
    pub fn new(bot: BotClient, edit_cooldown_ms: u64, chat_budget: ChatBudget) -> Self {
        Self {
            bot,
            queue: BinaryHeap::new(),
            pending_edits: EditMap::default(),
            seq_counter: 0,
            min_interval: Duration::from_millis(33), // ~30 msg/sec
            edit_cooldown: Duration::from_millis(edit_cooldown_ms),
            last_send: None,
            last_edit: HashMap::new(),
            retry_until: None,
            chat_budget,
        }
    }

    /// Get a clone of the shared chat budget for use by background tasks.
    pub fn chat_budget(&self) -> ChatBudget {
        self.chat_budget.clone()
    }

    /// Enqueue an operation.
    pub fn enqueue(&mut self, op: OutboxOp) {
        if let OutboxOp::Edit {
            chat_id,
            message_id,
            ..
        } = &op
        {
            // Coalesce: only keep the latest edit per message.
            self.pending_edits.upsert((*chat_id, *message_id), op);
        } else {
            let priority = op.priority();
            let seq = self.seq_counter;
            self.seq_counter += 1;
            self.queue.push(HeapEntry { op, priority, seq });
        }
    }

    /// Number of pending operations.
    pub fn len(&self) -> usize {
        self.queue.len() + self.pending_edits.len()
    }

    /// Whether the outbox is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.pending_edits.is_empty()
    }

    /// Clear the per-message edit cooldown so the next edit goes through
    /// immediately. Used at end-of-turn to ensure the final edit (removing
    /// the cancel keyboard) isn't blocked by a recent content edit.
    pub fn clear_edit_cooldown(&mut self) {
        self.last_edit.clear();
    }

    /// Try to consume a token for `chat_id`. Returns `true` if allowed.
    fn chat_try_consume(&mut self, chat_id: i64) -> bool {
        self.chat_budget.try_consume(chat_id)
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
                tokio::time::sleep(
                    self.min_interval
                        .checked_sub(elapsed)
                        .expect("elapsed < min_interval"),
                )
                .await;
            }
        }

        // Try sends/deletes first (from the heap), skipping budget-blocked
        // entries to avoid head-of-line blocking.
        if !self.queue.is_empty() {
            let mut deferred = Vec::new();
            while let Some(entry) = self.queue.pop() {
                let chat_id = entry.op.chat_id();
                if self.chat_try_consume(chat_id) {
                    // Put back the deferred entries.
                    for d in deferred {
                        self.queue.push(d);
                    }
                    let span = debug_span!("outbox_flush", queue_len = self.queue.len());
                    return async {
                        self.last_send = Some(Instant::now());
                        self.execute(entry.op).await;
                        true
                    }
                    .instrument(span)
                    .await;
                }
                deferred.push(entry);
            }
            // All entries were budget-blocked — put them all back.
            for d in deferred {
                self.queue.push(d);
            }
        }

        // Try pending edits (coalesced, cooldown + budget checked inside).
        if let Some(op) = self.pending_edits.pop_eligible(
            &self.last_edit,
            self.edit_cooldown,
            &self.chat_budget,
        ) {
            let span = debug_span!("outbox_flush", pending_edits = self.pending_edits.len());
            return async {
                self.last_send = Some(Instant::now());
                self.execute(op).await;
                true
            }
            .instrument(span)
            .await;
        }

        false
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ── EditMap tests ───────────────────────────────────────────

    /// Enqueuing multiple edits for the same message should coalesce —
    /// only the latest text is retained.
    #[test]
    fn test_edit_coalescing() {
        let mut map = EditMap::default();
        let key = (100, 200);

        map.upsert(
            key,
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 200,
                text: "first".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        map.upsert(
            key,
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 200,
                text: "second".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        map.upsert(
            key,
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 200,
                text: "third".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );

        // Should only have 1 entry.
        assert_eq!(map.len(), 1);

        // Pop should return the latest content.
        let empty_cooldown = HashMap::new();
        let budget = ChatBudget::new();
        let op = map
            .pop_eligible(&empty_cooldown, Duration::ZERO, &budget)
            .unwrap();
        if let OutboxOp::Edit { text, .. } = op {
            assert_eq!(text, "third");
        } else {
            panic!("expected Edit");
        }

        assert!(map.is_empty());
    }

    /// Edits for different messages are independent.
    #[test]
    fn test_edit_coalescing_different_messages() {
        let mut map = EditMap::default();

        map.upsert(
            (100, 1),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 1,
                text: "msg1-v2".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        map.upsert(
            (100, 1),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 1,
                text: "msg1-v3".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        map.upsert(
            (100, 2),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 2,
                text: "msg2-v1".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );

        assert_eq!(map.len(), 2);

        // FIFO order: msg 1 first, then msg 2.
        let empty = HashMap::new();
        let budget = ChatBudget::new();
        let op1 = map
            .pop_eligible(&empty, Duration::ZERO, &budget)
            .unwrap();
        if let OutboxOp::Edit {
            message_id, text, ..
        } = op1
        {
            assert_eq!(message_id, 1);
            assert_eq!(text, "msg1-v3");
        } else {
            panic!("expected Edit");
        }

        let op2 = map
            .pop_eligible(&empty, Duration::ZERO, &budget)
            .unwrap();
        if let OutboxOp::Edit {
            message_id, text, ..
        } = op2
        {
            assert_eq!(message_id, 2);
            assert_eq!(text, "msg2-v1");
        } else {
            panic!("expected Edit");
        }

        assert!(map.is_empty());
    }

    /// Edits blocked by cooldown are skipped; eligible edits still go through.
    #[test]
    fn test_edit_cooldown_skip() {
        let mut map = EditMap::default();
        map.upsert(
            (100, 1),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 1,
                text: "blocked".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        map.upsert(
            (100, 2),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 2,
                text: "ok".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );

        // Message 1 was edited recently, message 2 was not.
        let mut last_edit = HashMap::new();
        last_edit.insert((100_i64, 1_i64), Instant::now());

        let budget = ChatBudget::new();
        let op = map
            .pop_eligible(&last_edit, Duration::from_secs(10), &budget)
            .unwrap();
        if let OutboxOp::Edit {
            message_id, text, ..
        } = op
        {
            assert_eq!(message_id, 2);
            assert_eq!(text, "ok");
        } else {
            panic!("expected Edit");
        }

        // Message 1 is still pending (blocked by cooldown).
        assert_eq!(map.len(), 1);
    }

    // ── TokenBucket tests ───────────────────────────────────────

    #[test]
    fn test_token_bucket_basic() {
        let mut bucket = TokenBucket::new(10.0);
        // Starts full — 10 tokens available.
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        // 11th should fail.
        assert!(!bucket.try_consume());
    }

    #[test]
    fn test_token_bucket_refill() {
        let mut bucket = TokenBucket::new(60.0); // 1 token/sec
        // Drain all tokens.
        for _ in 0..60 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());

        // Simulate time passing.
        bucket.last_refill -= Duration::from_secs(5);
        // Should have ~5 tokens now.
        assert!(bucket.try_consume());
    }

    /// Edits for a budget-exhausted chat are skipped; edits for other
    /// chats still go through (no head-of-line blocking).
    #[test]
    fn test_edit_budget_skip_across_chats() {
        let mut map = EditMap::default();
        // Chat 100: will be over budget.
        map.upsert(
            (100, 1),
            OutboxOp::Edit {
                chat_id: 100,
                message_id: 1,
                text: "chat100".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );
        // Chat 200: has budget.
        map.upsert(
            (200, 1),
            OutboxOp::Edit {
                chat_id: 200,
                message_id: 1,
                text: "chat200".to_owned(),
                parse_mode: None,
                reply_markup: None,
            },
        );

        let empty_cooldown = HashMap::new();
        let budget = ChatBudget::new();
        // Exhaust chat 100's budget.
        while budget.try_consume(100) {}

        // Should skip chat 100 and return chat 200's edit.
        let op = map
            .pop_eligible(&empty_cooldown, Duration::ZERO, &budget)
            .unwrap();
        assert_eq!(op.chat_id(), 200);

        // Chat 100's edit is still pending.
        assert_eq!(map.len(), 1);
    }
}
