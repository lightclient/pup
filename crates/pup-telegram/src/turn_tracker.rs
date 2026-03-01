use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::bot::BotClient;
use crate::outbox::{ChatBudget, Outbox, OutboxOp};
use crate::render::{
    MAX_BODY_CHARS, cancel_keyboard, empty_keyboard, escape_html, split_message, to_telegram_html,
};

/// How many completed tool calls to accumulate before freezing the
/// current message and starting a new one below it.
///
/// This controls the "page size" — the number of completed tool calls
/// that appear in a single message.  When the limit is reached the
/// message is edited to its final form (keyboard removed) and a fresh
/// "live" message is sent below it so the progress indicator stays at
/// the bottom of the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallLimit {
    /// Freeze after N completed tool calls per page.
    Last(usize),
    /// Never freeze — all tool calls in a single message.
    All,
}

impl Default for ToolCallLimit {
    fn default() -> Self {
        Self::Last(3)
    }
}

/// How many lines of tool output to show per tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputLines {
    /// Show the first N lines, then ". . . (M more lines)".
    First(usize),
    /// Show all lines.
    All,
}

impl Default for ToolOutputLines {
    fn default() -> Self {
        Self::First(10)
    }
}

/// Truncate tool output to at most `limit` lines.
///
/// If the output exceeds the limit, returns the first `limit` lines
/// followed by a `. . . (N more lines)` indicator.
/// If `limit` is `ToolOutputLines::All`, returns the input unchanged.
pub fn truncate_tool_output(output: &str, limit: ToolOutputLines) -> String {
    let max = match limit {
        ToolOutputLines::All => return output.to_owned(),
        ToolOutputLines::First(n) => n,
    };

    if max == 0 {
        let total = output.lines().count();
        if total == 0 {
            return String::new();
        }
        return format!(". . . ({total} more lines)");
    }

    let mut lines = output.lines();
    let mut kept: Vec<&str> = Vec::with_capacity(max);
    for _ in 0..max {
        match lines.next() {
            Some(l) => kept.push(l),
            None => return output.to_owned(),
        }
    }

    // Count remaining lines.
    let remaining: usize = lines.count();
    if remaining == 0 {
        return output.to_owned();
    }

    kept.push("");
    let mut result = kept.join("\n");
    let indicator = format!(". . . ({remaining} more lines)");
    result.push_str(&indicator);
    result
}

/// A tracked tool call for verbose rendering.
#[derive(Debug)]
struct TrackedTool {
    tool_name: String,
    args: serde_json::Value,
    /// Accumulated tool output (from `tool_update` deltas and/or `tool_end`).
    content: String,
    is_error: bool,
    done: bool,
}

// ── Multi-destination support ─────────────────────────────────

/// A single Telegram chat destination for a turn's messages.
///
/// A turn can target multiple destinations simultaneously (e.g. a forum
/// topic AND a DM chat).  Each destination has its own Telegram message
/// and typing indicator.
///
/// ## Freeze-and-advance
///
/// When the "page" of tool calls fills up, the current live message is
/// **frozen** (edited to its final content, keyboard removed) and a new
/// message is sent below it.  This keeps the progress indicator at the
/// bottom of the chat while allowing the user to scroll up through the
/// full tool-call history.
#[derive(Debug)]
struct Destination {
    /// Chat ID where messages are sent.
    chat_id: i64,
    /// Thread ID for topic mode (`None` for DMs).
    thread_id: Option<i64>,
    /// Message ID of the "live" message currently being edited.
    live_message_id: Option<i64>,
    /// Whether the live-message send is still in flight.
    send_pending: bool,
    /// Channel to receive the sent message ID.
    send_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<crate::bot::SentMessage>>>,
    /// Sender to stop the typing indicator loop (dropped on turn end).
    typing_stop: Option<tokio::sync::watch::Sender<bool>>,
}

impl Destination {
    /// Try to resolve the sent message ID if the send is still pending.
    fn try_resolve_message_id(&mut self) {
        if !self.send_pending {
            return;
        }
        if let Some(mut rx) = self.send_rx.take() {
            match rx.try_recv() {
                Ok(Ok(sent)) => {
                    self.live_message_id = Some(sent.message_id);
                    self.send_pending = false;
                }
                Ok(Err(_)) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.send_pending = false;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    self.send_rx = Some(rx);
                }
            }
        }
    }

    /// Whether this destination has a live message (resolved or pending).
    fn has_live_message(&self) -> bool {
        self.live_message_id.is_some() || self.send_pending
    }
}

// ── Turn state ────────────────────────────────────────────────

/// State for one agent turn in one session.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct TurnState {
    /// Session ID (for cancel button).
    session_id: String,
    /// Destinations to send messages to (topic, DM, or both).
    destinations: Vec<Destination>,
    /// Accumulated streaming text for the current assistant message.
    streaming_text: String,
    /// Whether the model is currently in a thinking/reasoning phase.
    thinking: bool,
    /// Accumulated thinking/chain-of-thought text.
    thinking_text: String,
    /// Last time we sent an edit to Telegram.
    last_edit: Instant,
    /// Whether content has changed since the last edit.
    dirty: bool,
    /// Whether to show thinking/reasoning content.
    show_thinking: bool,
    /// Whether to show tool call details.
    show_tools: bool,
    /// How many completed tool calls per page before freezing.
    tool_call_limit: ToolCallLimit,
    /// How many lines of tool output to show per tool call.
    tool_output_lines: ToolOutputLines,
    /// Tracked tool calls in order.
    tools: Vec<TrackedTool>,
    /// Index into `tools` where the current page starts.
    /// Tools before this index have been frozen into earlier messages.
    page_start: usize,
    /// Length of the display text at the last flush (for change detection).
    last_display_len: usize,
    /// Whether the streaming message is complete (show full text, not
    /// just complete paragraphs).
    streaming_complete: bool,
}

/// Find the byte position just past the last sentence-ending punctuation
/// that is followed by a space (e.g. `". "`, `"! "`, `"? "`).
///
/// Returns `None` when no such boundary exists.
fn rfind_sentence_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    // Need at least 2 bytes for "X " where X is punctuation.
    if bytes.len() < 2 {
        return None;
    }
    // Walk backwards looking for `. `, `! `, `? `.
    for i in (0..bytes.len() - 1).rev() {
        if (bytes[i] == b'.' || bytes[i] == b'!' || bytes[i] == b'?') && bytes[i + 1] == b' ' {
            return Some(i + 2); // include the punctuation and the space
        }
    }
    None
}

impl TurnState {
    /// Return the streaming text to display, snapped to a natural boundary.
    ///
    /// During active streaming we trim back to the last "clean" break so
    /// that Telegram edits never show a half-finished sentence.  The
    /// hierarchy (best → worst) is:
    ///
    ///  1. Paragraph break (`\n\n`)
    ///  2. Line break (`\n`) — catches bullet lists, headings, etc.
    ///  3. Sentence-ending punctuation followed by a space (`. `, `! `, `? `)
    ///
    /// If none of those exist the full (short) buffer is returned so the
    /// very first sentence still streams naturally.
    ///
    /// Once the streaming message is complete (`streaming_complete` is set),
    /// the full text is always returned.
    fn display_text(&self) -> &str {
        if self.streaming_complete {
            return &self.streaming_text;
        }

        let text = &self.streaming_text;

        // 1. Paragraph break — strongest signal.
        if let Some(pos) = text.rfind("\n\n") {
            return &text[..pos + 2];
        }

        // 2. Line break — bullet lists, numbered items, headings.
        if let Some(pos) = text.rfind('\n') {
            return &text[..=pos];
        }

        // 3. Sentence-ending punctuation followed by a space.
        //    Search backwards for `. `, `! `, or `? `.
        if let Some(pos) = rfind_sentence_end(text) {
            return &text[..pos];
        }

        // Nothing found — show what we have (first sentence still
        // streaming in).
        text
    }

    /// Tools on the current page (from `page_start` to end).
    fn current_page_tools(&self) -> &[TrackedTool] {
        &self.tools[self.page_start..]
    }

    /// Whether the current page is full and should be frozen.
    fn page_full(&self) -> bool {
        match self.tool_call_limit {
            ToolCallLimit::All => false,
            ToolCallLimit::Last(n) => {
                // Only freeze when all tools on the page are done and we've
                // reached the limit.  Don't freeze while a tool is in-progress
                // since its output is still streaming.
                let page = self.current_page_tools();
                let completed = page.iter().filter(|t| t.done).count();
                completed >= n && page.iter().all(|t| t.done)
            }
        }
    }

    /// Render tool calls from the given range as Telegram HTML.
    fn render_tools(&self, tools: &[TrackedTool]) -> Vec<String> {
        let mut parts = Vec::new();
        if !self.show_tools {
            return parts;
        }
        for tool in tools {
            use std::fmt::Write;
            let status = if !tool.done {
                "▸ "
            } else if tool.is_error {
                "✗ "
            } else {
                "✓ "
            };
            let mut line = format!("{status}<b>{}</b>", escape_html(&tool.tool_name));
            // Show command or path arg if present.
            if let Some(cmd) = tool.args.get("command").and_then(|v| v.as_str()) {
                let truncated = if cmd.len() > 200 {
                    let end = cmd.floor_char_boundary(200);
                    &cmd[..end]
                } else {
                    cmd
                };
                let _ = write!(line, "\n<pre>{}</pre>", escape_html(truncated));
            } else if let Some(path) = tool.args.get("path").and_then(|v| v.as_str()) {
                let _ = write!(line, " <code>{}</code>", escape_html(path));
            }
            // Show tool output (truncated by line limit).
            if !tool.content.is_empty() {
                let truncated = truncate_tool_output(&tool.content, self.tool_output_lines);
                if !truncated.is_empty() {
                    let _ = write!(line, "\n<pre>{}</pre>", escape_html(&truncated));
                }
            }
            parts.push(line);
        }
        parts
    }

    /// Render the current page (tools on page + thinking + streaming text).
    ///
    /// If `include_text` is true, the streaming assistant text is appended
    /// (used during the turn). If false, only tools/thinking are rendered
    /// (used at turn end when the text goes to a separate message).
    fn render_parts(&self, include_text: bool) -> String {
        let mut parts = Vec::new();

        // Tool call summaries for the current page.
        parts.extend(self.render_tools(self.current_page_tools()));

        // Thinking content.
        //
        // Shown when `show_thinking` is on and either:
        //   - The model is actively thinking (no text accumulated yet), or
        //   - Thinking text exists (persists through tool calls until
        //     response text starts streaming).
        if self.show_thinking {
            if self.thinking && self.thinking_text.is_empty() && self.streaming_text.is_empty() {
                // Still waiting for the first thinking delta.
                parts.push("<i>Thinking…</i>".to_owned());
            } else if !self.thinking_text.is_empty() && self.streaming_text.is_empty() {
                const MAX_THINKING_DISPLAY: usize = 2000;
                let display = if self.thinking_text.len() > MAX_THINKING_DISPLAY {
                    let start = self.thinking_text.len() - MAX_THINKING_DISPLAY;
                    let safe_start = self.thinking_text.ceil_char_boundary(start);
                    format!("…{}", &self.thinking_text[safe_start..])
                } else {
                    self.thinking_text.clone()
                };
                parts.push(format!("<i>{}</i>", escape_html(&display)));
            }
        }

        // Streaming text.
        if include_text && !self.streaming_text.is_empty() {
            let display = self.display_text();
            if !display.is_empty() {
                let html = to_telegram_html(display);
                if !html.is_empty() {
                    parts.push(html);
                }
            }
        }

        if parts.is_empty() {
            "…".to_owned()
        } else {
            parts.join("\n\n")
        }
    }

    /// Render content for a frozen page (tools in the range, plus
    /// thinking text on the first page).
    fn render_frozen_page(&self, tool_range: std::ops::Range<usize>) -> String {
        let mut parts = Vec::new();

        // Include thinking text on the first page (page_start was 0
        // when this page was live, so tool_range.start == 0).
        if self.show_thinking && tool_range.start == 0 && !self.thinking_text.is_empty() {
            const MAX_THINKING_DISPLAY: usize = 2000;
            let display = if self.thinking_text.len() > MAX_THINKING_DISPLAY {
                let start = self.thinking_text.len() - MAX_THINKING_DISPLAY;
                let safe_start = self.thinking_text.ceil_char_boundary(start);
                format!("…{}", &self.thinking_text[safe_start..])
            } else {
                self.thinking_text.clone()
            };
            parts.push(format!("<i>{}</i>", escape_html(&display)));
        }

        let tools = &self.tools[tool_range];
        parts.extend(self.render_tools(tools));

        if parts.is_empty() {
            "…".to_owned()
        } else {
            parts.join("\n\n")
        }
    }

    /// Render the full live-message content (tools + thinking + text).
    fn render(&self) -> String {
        self.render_parts(true)
    }

    /// Enqueue an edit for the live message in all destinations.
    fn flush(&mut self, outbox: &mut Outbox, edit_interval_ms: u64) {
        if !self.dirty {
            return;
        }

        #[allow(clippy::cast_possible_truncation)]
        let elapsed = self.last_edit.elapsed().as_millis() as u64;
        if elapsed < edit_interval_ms {
            return;
        }

        let rendered = self.render();
        let chunks = split_message(&rendered, MAX_BODY_CHARS);
        let display = chunks.first().cloned().unwrap_or_else(|| "…".to_owned());
        let keyboard = cancel_keyboard(&self.session_id);

        for dest in &mut self.destinations {
            dest.try_resolve_message_id();

            let Some(msg_id) = dest.live_message_id else {
                continue;
            };

            outbox.enqueue(OutboxOp::Edit {
                chat_id: dest.chat_id,
                message_id: msg_id,
                text: display.clone(),
                parse_mode: Some("HTML".to_owned()),
                reply_markup: Some(keyboard.clone()),
            });
        }

        self.last_edit = Instant::now();
        self.dirty = false;
    }

    /// Freeze the current live message.
    ///
    /// The live message is edited to show only the tools on the current
    /// page (no streaming text, keyboard removed).  The page pointer is
    /// advanced and the live message ID is cleared so the next event
    /// that needs a message (tool_start, message_delta, etc.) will
    /// create a fresh one below via `ensure_message`.
    ///
    /// This avoids the dual-cancel-button problem that would occur if
    /// we sent a new message here: the outbox processes sends before
    /// edits, so the new cancel button would appear before the old one
    /// is removed.
    fn freeze_and_advance(&mut self, outbox: &mut Outbox) {
        let page_range = self.page_start..self.tools.len();
        let frozen_html = self.render_frozen_page(page_range);
        let chunks = split_message(&frozen_html, MAX_BODY_CHARS);
        let frozen_display = chunks.first().cloned().unwrap_or_else(|| "…".to_owned());

        // Advance the page pointer.
        self.page_start = self.tools.len();

        for dest in &mut self.destinations {
            dest.try_resolve_message_id();

            // Freeze the current live message (remove keyboard).
            if let Some(msg_id) = dest.live_message_id {
                outbox.enqueue(OutboxOp::Edit {
                    chat_id: dest.chat_id,
                    message_id: msg_id,
                    text: frozen_display.clone(),
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: Some(empty_keyboard()),
                });
            }

            // Clear the live message so ensure_message creates a new
            // one on the next event.
            dest.live_message_id = None;
            dest.send_pending = false;
            dest.send_rx = None;
        }

        self.last_edit = Instant::now();
        self.dirty = false;
    }
}

/// Manages per-session turn state.
///
/// During a turn the tracker maintains a "live" Telegram message at the
/// bottom of the chat that shows current progress (thinking, active tool
/// call, streaming text).  When a page of tool calls fills up the live
/// message is frozen (edited to its final form) and a new live message
/// is sent below it, so the user can scroll back through the full
/// tool-call history.
#[derive(Debug)]
pub struct TurnTracker {
    /// Active turns keyed by session_id.
    turns: HashMap<String, TurnState>,
    /// Minimum interval between edits.
    edit_interval_ms: u64,
    /// Default thinking display for sessions without an override.
    default_thinking: bool,
    /// Default tools display for sessions without an override.
    default_tools: bool,
    /// Per-session thinking overrides (persists across turns).
    session_thinking: HashMap<String, bool>,
    /// Per-session tools overrides (persists across turns).
    session_tools: HashMap<String, bool>,
    /// How many completed tool calls per page before freezing.
    tool_call_limit: ToolCallLimit,
    /// How many lines of tool output to show per tool call.
    tool_output_lines: ToolOutputLines,
}

impl TurnTracker {
    pub fn new(edit_interval_ms: u64) -> Self {
        Self {
            turns: HashMap::new(),
            edit_interval_ms,
            default_thinking: false,
            default_tools: false,
            session_thinking: HashMap::new(),
            session_tools: HashMap::new(),
            tool_call_limit: ToolCallLimit::default(),
            tool_output_lines: ToolOutputLines::default(),
        }
    }

    /// Set the tool call display limit.
    pub fn set_tool_call_limit(&mut self, limit: ToolCallLimit) {
        self.tool_call_limit = limit;
        for state in self.turns.values_mut() {
            state.tool_call_limit = limit;
        }
    }

    /// Set the tool output line limit.
    pub fn set_tool_output_lines(&mut self, limit: ToolOutputLines) {
        self.tool_output_lines = limit;
        for state in self.turns.values_mut() {
            state.tool_output_lines = limit;
        }
    }

    /// Set the default thinking display for sessions without an override.
    pub fn set_default_thinking(&mut self, on: bool) {
        self.default_thinking = on;
    }

    /// Set the default tools display for sessions without an override.
    pub fn set_default_tools(&mut self, on: bool) {
        self.default_tools = on;
    }

    /// Set both thinking and tools defaults at once.
    pub fn set_default_verbose(&mut self, on: bool) {
        self.default_thinking = on;
        self.default_tools = on;
    }

    /// Enable or disable thinking display for a specific session.
    pub fn set_thinking(&mut self, session_id: &str, on: bool) {
        self.session_thinking.insert(session_id.to_owned(), on);
        if let Some(state) = self.turns.get_mut(session_id) {
            state.show_thinking = on;
        }
    }

    /// Enable or disable tools display for a specific session.
    pub fn set_tools(&mut self, session_id: &str, on: bool) {
        self.session_tools.insert(session_id.to_owned(), on);
        if let Some(state) = self.turns.get_mut(session_id) {
            state.show_tools = on;
        }
    }

    /// Enable or disable both thinking and tools for a session.
    pub fn set_verbose(&mut self, session_id: &str, on: bool) {
        self.set_thinking(session_id, on);
        self.set_tools(session_id, on);
    }

    /// Get the effective thinking setting for a session.
    pub fn is_thinking(&self, session_id: &str) -> bool {
        self.session_thinking
            .get(session_id)
            .copied()
            .unwrap_or(self.default_thinking)
    }

    /// Get the effective tools setting for a session.
    pub fn is_tools(&self, session_id: &str) -> bool {
        self.session_tools
            .get(session_id)
            .copied()
            .unwrap_or(self.default_tools)
    }

    /// Whether either thinking or tools is enabled.
    pub fn is_verbose(&self, session_id: &str) -> bool {
        self.is_thinking(session_id) || self.is_tools(session_id)
    }

    /// Check if a turn is being tracked for the given session.
    pub fn has_turn(&self, session_id: &str) -> bool {
        self.turns.contains_key(session_id)
    }

    /// Start tracking a new agent turn with one or more destinations.
    pub fn start_turn(
        &mut self,
        session_id: &str,
        destinations: &[(i64, Option<i64>)],
        bot: &BotClient,
        chat_budget: &ChatBudget,
    ) {
        let dests: Vec<Destination> = destinations
            .iter()
            .map(|&(chat_id, thread_id)| {
                let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
                let bot = bot.clone();
                let budget = chat_budget.clone();
                tokio::spawn(async move {
                    loop {
                        if budget.try_consume(chat_id)
                            && let Err(e) = bot.send_chat_action(chat_id, "typing", thread_id).await
                        {
                            debug!(error = %e, "typing indicator failed");
                        }
                        tokio::select! {
                            _ = stop_rx.changed() => break,
                            () = tokio::time::sleep(Duration::from_secs(4)) => {}
                        }
                    }
                });

                Destination {
                    chat_id,
                    thread_id,
                    live_message_id: None,
                    send_pending: false,
                    send_rx: None,
                    typing_stop: Some(stop_tx),
                }
            })
            .collect();

        self.turns.insert(
            session_id.to_owned(),
            TurnState {
                session_id: session_id.to_owned(),
                destinations: dests,
                streaming_text: String::new(),
                thinking: false,
                thinking_text: String::new(),
                last_edit: Instant::now(),
                dirty: false,
                show_thinking: self.is_thinking(session_id),
                show_tools: self.is_tools(session_id),
                tool_call_limit: self.tool_call_limit,
                tool_output_lines: self.tool_output_lines,
                tools: Vec::new(),
                page_start: 0,
                last_display_len: 0,
                streaming_complete: false,
            },
        );
    }

    /// Add a destination to an existing turn (e.g. `/attach` mid-turn).
    #[allow(clippy::too_many_arguments)]
    pub fn add_destination(
        &mut self,
        session_id: &str,
        chat_id: i64,
        thread_id: Option<i64>,
        bot: &BotClient,
        chat_budget: &ChatBudget,
        outbox: &mut Outbox,
    ) {
        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        if state
            .destinations
            .iter()
            .any(|d| d.chat_id == chat_id && d.thread_id == thread_id)
        {
            return;
        }

        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        {
            let bot = bot.clone();
            let budget = chat_budget.clone();
            tokio::spawn(async move {
                loop {
                    if budget.try_consume(chat_id)
                        && let Err(e) = bot.send_chat_action(chat_id, "typing", thread_id).await
                    {
                        debug!(error = %e, "typing indicator failed");
                    }
                    tokio::select! {
                        _ = stop_rx.changed() => break,
                        () = tokio::time::sleep(Duration::from_secs(4)) => {}
                    }
                }
            });
        }

        let rendered = state.render();
        let keyboard = cancel_keyboard(&state.session_id);
        let (tx, rx) = tokio::sync::oneshot::channel();

        outbox.enqueue(OutboxOp::Send {
            chat_id,
            text: rendered,
            parse_mode: Some("HTML".to_owned()),
            reply_markup: Some(keyboard),
            message_thread_id: thread_id,
            result_tx: Some(tx),
        });

        state.destinations.push(Destination {
            chat_id,
            thread_id,
            live_message_id: None,
            send_pending: true,
            send_rx: Some(rx),
            typing_stop: Some(stop_tx),
        });
    }

    /// Ensure the live message exists in all destinations; send it if not.
    fn ensure_message(&mut self, session_id: &str, initial_text: &str, outbox: &mut Outbox) {
        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        let keyboard = cancel_keyboard(&state.session_id);
        let mut sent_any = false;

        for dest in &mut state.destinations {
            if dest.has_live_message() {
                continue;
            }

            let (tx, rx) = tokio::sync::oneshot::channel();

            outbox.enqueue(OutboxOp::Send {
                chat_id: dest.chat_id,
                text: initial_text.to_owned(),
                parse_mode: Some("HTML".to_owned()),
                reply_markup: Some(keyboard.clone()),
                message_thread_id: dest.thread_id,
                result_tx: Some(tx),
            });

            dest.send_pending = true;
            dest.send_rx = Some(rx);
            sent_any = true;
        }

        if sent_any {
            state.last_edit = Instant::now();
        }
    }

    /// If the current page is full, freeze the live message and start
    /// a new one.
    fn maybe_freeze(&mut self, session_id: &str, outbox: &mut Outbox) {
        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };
        if state.page_full() {
            state.freeze_and_advance(outbox);
        }
    }

    /// Note that a tool started. Ensures the Telegram message exists.
    pub fn tool_start(
        &mut self,
        session_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        outbox: &mut Outbox,
    ) {
        let show_tools = self.turns.get(session_id).is_some_and(|s| s.show_tools);
        if !show_tools {
            return;
        }
        if let Some(state) = self.turns.get_mut(session_id) {
            state.tools.push(TrackedTool {
                tool_name: tool_name.to_owned(),
                args: args.clone(),
                content: String::new(),
                is_error: false,
                done: false,
            });
            state.dirty = true;
        }
        self.ensure_message(session_id, "…", outbox);
        if let Some(state) = self.turns.get_mut(session_id) {
            state.flush(outbox, self.edit_interval_ms);
        }
    }

    /// Accumulate streaming tool output.
    pub fn tool_update(
        &mut self,
        session_id: &str,
        tool_name: &str,
        content: &str,
        outbox: &mut Outbox,
    ) {
        if let Some(state) = self.turns.get_mut(session_id)
            && state.show_tools
        {
            for tool in state.tools.iter_mut().rev() {
                if tool.tool_name == tool_name && !tool.done {
                    tool.content.push_str(content);
                    break;
                }
            }
            state.dirty = true;
            state.flush(outbox, self.edit_interval_ms);
        }
    }

    /// Note that a tool finished.
    pub fn tool_end(
        &mut self,
        session_id: &str,
        tool_name: &str,
        content: &str,
        is_error: bool,
        outbox: &mut Outbox,
    ) {
        if let Some(state) = self.turns.get_mut(session_id)
            && state.show_tools
        {
            for tool in state.tools.iter_mut().rev() {
                if tool.tool_name == tool_name && !tool.done {
                    tool.done = true;
                    tool.is_error = is_error;
                    if tool.content.is_empty() && !content.is_empty() {
                        content.clone_into(&mut tool.content);
                    }
                    break;
                }
            }
            state.dirty = true;
            state.flush(outbox, self.edit_interval_ms);
        }
        // Freeze the page if it's full after this tool completed.
        self.maybe_freeze(session_id, outbox);
    }

    /// Note that thinking/reasoning content is streaming.
    pub fn thinking_delta(&mut self, session_id: &str, text: &str, outbox: &mut Outbox) {
        let show_thinking = self.turns.get(session_id).is_some_and(|s| s.show_thinking);
        if let Some(state) = self.turns.get_mut(session_id) {
            state.thinking = true;
            state.thinking_text.push_str(text);
        }
        if show_thinking {
            self.ensure_message(session_id, "<i>Thinking…</i>", outbox);
            if let Some(state) = self.turns.get_mut(session_id) {
                state.dirty = true;
                state.flush(outbox, self.edit_interval_ms);
            }
        }
    }

    /// Accumulate a streaming text delta.
    pub fn message_delta(&mut self, session_id: &str, text: &str, outbox: &mut Outbox) {
        let verbose = self
            .turns
            .get(session_id)
            .is_some_and(|s| s.show_thinking || s.show_tools);

        if verbose {
            #[allow(clippy::if_then_some_else_none)]
            let initial = {
                let needs_send = self
                    .turns
                    .get(session_id)
                    .is_some_and(|s| s.destinations.iter().any(|d| !d.has_live_message()));
                if needs_send {
                    let state = self.turns.get(session_id).expect("session must exist");
                    let mut preview = state.render();
                    let delta_html = to_telegram_html(text);
                    if !delta_html.is_empty() {
                        if !preview.is_empty() {
                            preview.push_str("\n\n");
                        }
                        preview.push_str(&delta_html);
                    }
                    if preview.is_empty() {
                        "…".clone_into(&mut preview);
                    }
                    Some(preview)
                } else {
                    None
                }
            };
            if let Some(initial_text) = initial {
                self.ensure_message(session_id, &initial_text, outbox);
            }
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        // First text delta means thinking is done.
        state.thinking = false;
        state.streaming_text.push_str(text);

        if state.show_thinking || state.show_tools {
            let new_display_len = state.display_text().len();
            if new_display_len != state.last_display_len {
                state.last_display_len = new_display_len;
                state.dirty = true;
            }
            state.flush(outbox, self.edit_interval_ms);
        }
    }

    /// Handle the end of a streaming message.
    pub fn message_end_with_content(
        &mut self,
        session_id: &str,
        content: &str,
        outbox: &mut Outbox,
    ) {
        let verbose = self
            .turns
            .get(session_id)
            .is_some_and(|s| s.show_thinking || s.show_tools);

        if verbose && !content.is_empty() {
            let html = to_telegram_html(content);
            let initial = if html.is_empty() {
                "…".to_owned()
            } else {
                html
            };
            self.ensure_message(session_id, &initial, outbox);
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        if !content.is_empty() {
            content.clone_into(&mut state.streaming_text);
        }

        state.streaming_complete = true;

        if state.show_thinking || state.show_tools {
            state.dirty = true;
            state.flush(outbox, 0);
        }
    }

    /// Finalize the turn.
    ///
    /// The live message is edited to show only the current page's
    /// tools/thinking summary (keyboard removed).  The final assistant
    /// text is sent as a separate message below so the user can scroll
    /// back through the tool trace.
    #[allow(clippy::too_many_lines)]
    pub fn end_turn(&mut self, session_id: &str, outbox: &mut Outbox) {
        let Some(mut state) = self.turns.remove(session_id) else {
            return;
        };

        // Stop all typing indicators.
        for dest in &mut state.destinations {
            dest.typing_stop.take();
        }

        // Resolve pending message IDs.
        for dest in &mut state.destinations {
            dest.try_resolve_message_id();
        }

        // Check for verbose content on the *current page* only.
        // Earlier pages were already frozen into their own messages.
        let has_verbose_on_page = (state.show_tools && state.page_start < state.tools.len())
            || (state.show_thinking && !state.thinking_text.is_empty());
        let has_text = !state.streaming_text.is_empty();

        // Render the current page's tools/thinking (no streaming text).
        #[allow(clippy::if_then_some_else_none)]
        let summary_chunks =
            has_verbose_on_page.then(|| split_message(&state.render_parts(false), MAX_BODY_CHARS));

        let text_chunks = if has_text {
            let html = to_telegram_html(&state.streaming_text);
            if html.is_empty() {
                None
            } else {
                Some(split_message(&html, MAX_BODY_CHARS))
            }
        } else {
            None
        };

        #[allow(clippy::if_then_some_else_none)]
        let rendered_chunks = (has_text && !has_verbose_on_page)
            .then(|| split_message(&state.render(), MAX_BODY_CHARS));

        #[allow(clippy::if_not_else, clippy::if_then_some_else_none)]
        let no_text_chunks =
            (!has_text).then(|| split_message(&state.render_parts(false), MAX_BODY_CHARS));

        for dest in &state.destinations {
            if let Some(msg_id) = dest.live_message_id {
                if has_verbose_on_page && has_text {
                    // Edit the live message to show only tools/thinking.
                    if let Some(ref chunks) = summary_chunks
                        && let Some(first) = chunks.first()
                    {
                        outbox.enqueue(OutboxOp::Edit {
                            chat_id: dest.chat_id,
                            message_id: msg_id,
                            text: first.clone(),
                            parse_mode: Some("HTML".to_owned()),
                            reply_markup: Some(empty_keyboard()),
                        });
                    }

                    // Send final assistant text as a separate message.
                    if let Some(ref chunks) = text_chunks {
                        for chunk in chunks {
                            outbox.enqueue(OutboxOp::Send {
                                chat_id: dest.chat_id,
                                text: chunk.clone(),
                                parse_mode: Some("HTML".to_owned()),
                                reply_markup: None,
                                message_thread_id: dest.thread_id,
                                result_tx: None,
                            });
                        }
                    }
                } else if has_text {
                    // No verbose content on this page: just remove keyboard.
                    if let Some(ref chunks) = rendered_chunks {
                        if let Some(first) = chunks.first() {
                            outbox.enqueue(OutboxOp::Edit {
                                chat_id: dest.chat_id,
                                message_id: msg_id,
                                text: first.clone(),
                                parse_mode: Some("HTML".to_owned()),
                                reply_markup: Some(empty_keyboard()),
                            });
                        }
                        for chunk in chunks.iter().skip(1) {
                            outbox.enqueue(OutboxOp::Send {
                                chat_id: dest.chat_id,
                                text: chunk.clone(),
                                parse_mode: Some("HTML".to_owned()),
                                reply_markup: None,
                                message_thread_id: dest.thread_id,
                                result_tx: None,
                            });
                        }
                    }
                } else {
                    // No text at all (tools-only turn).
                    if let Some(ref chunks) = no_text_chunks
                        && let Some(first) = chunks.first()
                    {
                        outbox.enqueue(OutboxOp::Edit {
                            chat_id: dest.chat_id,
                            message_id: msg_id,
                            text: first.clone(),
                            parse_mode: Some("HTML".to_owned()),
                            reply_markup: Some(empty_keyboard()),
                        });
                    }
                }
            } else if has_text {
                let chunks = text_chunks.as_deref().unwrap_or(&[]);
                for chunk in chunks {
                    outbox.enqueue(OutboxOp::Send {
                        chat_id: dest.chat_id,
                        text: chunk.clone(),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: None,
                        message_thread_id: dest.thread_id,
                        result_tx: None,
                    });
                }
            }
        }
    }

    /// Get all session IDs with active turns.
    pub fn active_sessions(&self) -> Vec<String> {
        self.turns.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_tool_output ────────────────────────────────────

    #[test]
    fn truncate_empty_input() {
        assert_eq!(truncate_tool_output("", ToolOutputLines::First(10)), "");
    }

    #[test]
    fn truncate_fewer_lines_than_limit() {
        let input = "line1\nline2\nline3";
        assert_eq!(
            truncate_tool_output(input, ToolOutputLines::First(10)),
            input
        );
    }

    #[test]
    fn truncate_exact_limit() {
        let input = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
        assert_eq!(
            truncate_tool_output(input, ToolOutputLines::First(10)),
            input
        );
    }

    #[test]
    fn truncate_over_limit() {
        let input = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12";
        let result = truncate_tool_output(input, ToolOutputLines::First(10));
        assert_eq!(
            result,
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n. . . (2 more lines)"
        );
    }

    #[test]
    fn truncate_one_over_limit() {
        let input = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk";
        let result = truncate_tool_output(input, ToolOutputLines::First(10));
        assert_eq!(result, "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n. . . (1 more lines)");
    }

    #[test]
    fn truncate_all_shows_everything() {
        let input = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12";
        assert_eq!(truncate_tool_output(input, ToolOutputLines::All), input);
    }

    #[test]
    fn truncate_limit_zero() {
        let input = "a\nb\nc";
        let result = truncate_tool_output(input, ToolOutputLines::First(0));
        assert_eq!(result, ". . . (3 more lines)");
    }

    #[test]
    fn truncate_limit_zero_empty() {
        assert_eq!(truncate_tool_output("", ToolOutputLines::First(0)), "");
    }

    #[test]
    fn truncate_limit_one() {
        let input = "first\nsecond\nthird";
        let result = truncate_tool_output(input, ToolOutputLines::First(1));
        assert_eq!(result, "first\n. . . (2 more lines)");
    }

    #[test]
    fn truncate_many_over_default() {
        let lines: Vec<String> = (1..=25).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let result = truncate_tool_output(&input, ToolOutputLines::default());
        let expected_lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        let expected = format!("{}\n. . . (15 more lines)", expected_lines.join("\n"));
        assert_eq!(result, expected);
    }

    #[test]
    fn truncate_single_line_within_limit() {
        assert_eq!(
            truncate_tool_output("hello", ToolOutputLines::First(10)),
            "hello"
        );
    }

    #[test]
    fn truncate_preserves_empty_lines() {
        let input = "a\n\nb\n\nc\n\nd\n\ne\n\nf\n\ng";
        let result = truncate_tool_output(input, ToolOutputLines::First(5));
        assert_eq!(result, "a\n\nb\n\nc\n. . . (8 more lines)");
    }

    // ── ToolOutputLines default ─────────────────────────────────

    #[test]
    fn tool_output_lines_default_is_10() {
        assert_eq!(ToolOutputLines::default(), ToolOutputLines::First(10));
    }

    // ── Helper to build a TurnState for rendering tests ─────────

    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn make_render_state(
        show_thinking: bool,
        show_tools: bool,
        tools: Vec<TrackedTool>,
        streaming_text: &str,
        thinking: bool,
        thinking_text: &str,
        tool_call_limit: ToolCallLimit,
        tool_output_lines: ToolOutputLines,
        streaming_complete: bool,
    ) -> TurnState {
        TurnState {
            session_id: "s1".to_owned(),
            destinations: vec![Destination {
                chat_id: 1,
                thread_id: None,
                live_message_id: None,
                send_pending: false,
                send_rx: None,
                typing_stop: None,
            }],
            streaming_text: streaming_text.to_owned(),
            thinking,
            thinking_text: thinking_text.to_owned(),
            last_edit: Instant::now(),
            dirty: false,
            show_thinking,
            show_tools,
            tool_call_limit,
            tool_output_lines,
            tools,
            page_start: 0,
            last_display_len: 0,
            streaming_complete,
        }
    }

    // ── TurnTracker tool output rendering ──────────────────────

    #[test]
    fn render_tool_with_output() {
        let state = make_render_state(
            false,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls -la"}),
                content: "file1.txt\nfile2.txt\nfile3.txt\nfile4.txt\nfile5.txt".to_owned(),
                is_error: false,
                done: true,
            }],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(3),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("<b>Bash</b>"));
        assert!(rendered.contains("ls -la"));
        assert!(rendered.contains("file1.txt"));
        assert!(rendered.contains("file3.txt"));
        assert!(rendered.contains(". . . (2 more lines)"));
        assert!(!rendered.contains("file4.txt"));
    }

    #[test]
    fn render_tool_with_output_all_lines() {
        let state = make_render_state(
            false,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                content: "a\nb\nc\nd\ne".to_owned(),
                is_error: false,
                done: true,
            }],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::All,
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("a\nb\nc\nd\ne"));
        assert!(!rendered.contains(". . ."));
    }

    #[test]
    fn render_tool_no_output() {
        let state = make_render_state(
            false,
            true,
            vec![TrackedTool {
                tool_name: "Read".to_owned(),
                args: serde_json::json!({"path": "/tmp/foo.txt"}),
                content: String::new(),
                is_error: false,
                done: false,
            }],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("<b>Read</b>"));
        assert!(rendered.contains("/tmp/foo.txt"));
        assert!(!rendered.contains("<pre>"));
    }

    #[test]
    fn render_nonverbose_hides_tool_output() {
        let state = make_render_state(
            false,
            false,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "echo hi"}),
                content: "hi".to_owned(),
                is_error: false,
                done: true,
            }],
            "hello",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(!rendered.contains("Bash"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn render_tool_output_html_escaped() {
        let state = make_render_state(
            false,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "echo '<html>'"}),
                content: "<html>&amp;".to_owned(),
                is_error: false,
                done: true,
            }],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::All,
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("&lt;html&gt;&amp;amp;"));
    }

    // ── Multi-byte char boundary safety ─────────────────────────

    #[test]
    fn render_thinking_with_multibyte_chars_does_not_panic() {
        let line = "┌─────────────┐\n";
        let mut thinking = String::new();
        while thinking.len() < 3000 {
            thinking.push_str(line);
        }

        let state = make_render_state(
            true,
            false,
            Vec::new(),
            "",
            true,
            &thinking,
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("…"));
        assert!(rendered.contains("<i>"));
    }

    #[test]
    fn render_tool_command_with_multibyte_chars_does_not_panic() {
        let cmd: String = "日本語テスト".repeat(50);
        assert!(cmd.len() > 200);

        let state = make_render_state(
            false,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": cmd}),
                content: String::new(),
                is_error: false,
                done: false,
            }],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("<b>Bash</b>"));
    }

    // ── display_text natural-break buffering ──────────────────────

    fn make_text_state(text: &str, complete: bool) -> TurnState {
        make_render_state(
            true,
            true,
            Vec::new(),
            text,
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            complete,
        )
    }

    // ── rfind_sentence_end unit tests ───────────────────────────

    #[test]
    fn sentence_end_period_space() {
        assert_eq!(rfind_sentence_end("Hello world. Next"), Some(13));
    }

    #[test]
    fn sentence_end_exclamation() {
        assert_eq!(rfind_sentence_end("Wow! Next"), Some(5));
    }

    #[test]
    fn sentence_end_question() {
        assert_eq!(rfind_sentence_end("Really? Yes"), Some(8));
    }

    #[test]
    fn sentence_end_none_when_no_space_after() {
        assert_eq!(rfind_sentence_end("end."), None);
    }

    #[test]
    fn sentence_end_none_empty() {
        assert_eq!(rfind_sentence_end(""), None);
    }

    #[test]
    fn sentence_end_picks_last() {
        assert_eq!(rfind_sentence_end("A. B. C still wri"), Some(6));
    }

    // ── display_text ────────────────────────────────────────────

    #[test]
    fn display_text_no_break_shows_all() {
        // Very first sentence still streaming — nothing to snap to.
        let state = make_text_state("Hello world still writing", false);
        assert_eq!(state.display_text(), "Hello world still writing");
    }

    #[test]
    fn display_text_paragraph_break() {
        let state = make_text_state("First paragraph.\n\nSecond being wri", false);
        assert_eq!(state.display_text(), "First paragraph.\n\n");
    }

    #[test]
    fn display_text_line_break() {
        // No paragraph break, but a single newline (e.g. bullet list).
        // Snaps to the last complete line.
        let state = make_text_state("- item one\n- item two still wr", false);
        assert_eq!(state.display_text(), "- item one\n");
    }

    #[test]
    fn display_text_line_break_preferred_over_sentence() {
        // Both a line break and sentence end — line break wins (higher priority).
        let state = make_text_state("Done sentence. More text\nBullet still wr", false);
        assert_eq!(state.display_text(), "Done sentence. More text\n");
    }

    #[test]
    fn display_text_sentence_boundary() {
        // No line/paragraph break, but a sentence boundary exists.
        let state = make_text_state("First sentence. Second sentence still wri", false);
        assert_eq!(state.display_text(), "First sentence. ");
    }

    #[test]
    fn display_text_multiple_paragraphs() {
        let state = make_text_state("Para 1.\n\nPara 2.\n\nPara 3 still writing", false);
        assert_eq!(state.display_text(), "Para 1.\n\nPara 2.\n\n");
    }

    #[test]
    fn display_text_ending_with_break() {
        let state = make_text_state("Complete paragraph.\n\n", false);
        assert_eq!(state.display_text(), "Complete paragraph.\n\n");
    }

    #[test]
    fn display_text_streaming_complete_shows_everything() {
        let state = make_text_state("First paragraph.\n\nSecond paragraph still wri", true);
        assert_eq!(
            state.display_text(),
            "First paragraph.\n\nSecond paragraph still wri"
        );
    }

    #[test]
    fn display_text_empty() {
        let state = make_text_state("", false);
        assert_eq!(state.display_text(), "");
    }

    #[test]
    fn render_uses_display_text_not_full_streaming_text() {
        let state = make_text_state("Done sentence.\n\nPartial next", false);
        let rendered = state.render_parts(true);
        assert!(rendered.contains("Done sentence."));
        assert!(!rendered.contains("Partial next"));
    }

    #[test]
    fn render_complete_shows_full_text() {
        let state = make_text_state("Done paragraph.\n\nPartial next", true);
        let rendered = state.render_parts(true);
        assert!(rendered.contains("Done paragraph."));
        assert!(rendered.contains("Partial next"));
    }

    // ── Status icons ────────────────────────────────────────────

    #[test]
    fn render_tool_shows_status_icons() {
        let state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "Bash".to_owned(),
                    args: serde_json::json!({"command": "ls"}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "Bash".to_owned(),
                    args: serde_json::json!({"command": "false"}),
                    content: String::new(),
                    is_error: true,
                    done: true,
                },
                TrackedTool {
                    tool_name: "Read".to_owned(),
                    args: serde_json::json!({"path": "/tmp/f"}),
                    content: String::new(),
                    is_error: false,
                    done: false,
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(rendered.contains("✓ <b>Bash</b>"));
        assert!(rendered.contains("✗ <b>Bash</b>"));
        assert!(rendered.contains("▸ <b>Read</b>"));
    }

    // ── Thinking persistence ────────────────────────────────────

    #[test]
    fn thinking_text_persists_through_tool_calls() {
        // Thinking happened, then tools started — thinking flag is
        // still true, streaming text is empty.  The thinking text
        // should appear alongside the tools.
        let state = make_render_state(
            true,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                content: String::new(),
                is_error: false,
                done: false,
            }],
            "",
            true, // thinking flag still on
            "Let me check the files",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(
            rendered.contains("Let me check the files"),
            "thinking text must appear alongside tools: {rendered}"
        );
        assert!(rendered.contains("<b>Bash</b>"));
    }

    #[test]
    fn thinking_text_persists_after_thinking_flag_cleared() {
        // Thinking phase ended (thinking=false) but no response text
        // yet — only tools running.  Thinking text should still show.
        let state = make_render_state(
            true,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                content: String::new(),
                is_error: false,
                done: true,
            }],
            "",
            false, // thinking flag off (phase ended)
            "Let me check the files",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(
            rendered.contains("Let me check the files"),
            "thinking text must persist after thinking phase ends: {rendered}"
        );
    }

    #[test]
    fn thinking_text_hidden_once_response_starts() {
        // Response text started streaming — thinking text should
        // disappear in favor of the response.
        let state = make_render_state(
            true,
            true,
            vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                content: "files".to_owned(),
                is_error: false,
                done: true,
            }],
            "Here are the results.",
            false,
            "Let me check the files",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        let rendered = state.render_parts(true);
        assert!(
            !rendered.contains("Let me check the files"),
            "thinking text should be hidden once response starts: {rendered}"
        );
        assert!(rendered.contains("Here are the results."));
    }

    #[test]
    fn frozen_first_page_includes_thinking() {
        let state = make_render_state(
            true,
            true,
            vec![TrackedTool {
                tool_name: "A".to_owned(),
                args: serde_json::json!({}),
                content: String::new(),
                is_error: false,
                done: true,
            }],
            "",
            true,
            "My reasoning here",
            ToolCallLimit::Last(1),
            ToolOutputLines::First(10),
            false,
        );

        let frozen = state.render_frozen_page(0..1);
        assert!(
            frozen.contains("My reasoning here"),
            "frozen first page must include thinking: {frozen}"
        );
    }

    #[test]
    fn frozen_later_page_excludes_thinking() {
        let mut state = make_render_state(
            true,
            true,
            vec![
                TrackedTool {
                    tool_name: "A".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "B".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
            ],
            "",
            true,
            "My reasoning here",
            ToolCallLimit::Last(1),
            ToolOutputLines::First(10),
            false,
        );

        state.page_start = 1;
        let frozen = state.render_frozen_page(1..2);
        assert!(
            !frozen.contains("My reasoning here"),
            "frozen later page must not include thinking: {frozen}"
        );
    }

    // ── Page rendering ──────────────────────────────────────────

    #[test]
    fn render_only_shows_current_page_tools() {
        let mut state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "Bash".to_owned(),
                    args: serde_json::json!({"command": "ls"}),
                    content: "old".to_owned(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "Read".to_owned(),
                    args: serde_json::json!({"path": "/new"}),
                    content: String::new(),
                    is_error: false,
                    done: false,
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        // Advance the page so tool[0] is frozen.
        state.page_start = 1;
        let rendered = state.render_parts(true);
        // Only the current page tool should appear.
        assert!(!rendered.contains("ls"));
        assert!(!rendered.contains("old"));
        assert!(rendered.contains("<b>Read</b>"));
    }

    #[test]
    fn page_full_triggers_when_all_done() {
        let state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "A".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "B".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "C".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::Last(3),
            ToolOutputLines::First(10),
            false,
        );

        assert!(state.page_full());
    }

    #[test]
    fn page_not_full_with_in_progress_tool() {
        let state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "A".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "B".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "C".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: false, // still running
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::Last(3),
            ToolOutputLines::First(10),
            false,
        );

        assert!(!state.page_full());
    }

    #[test]
    fn page_never_full_with_limit_all() {
        let state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "A".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "B".to_owned(),
                    args: serde_json::json!({}),
                    content: String::new(),
                    is_error: false,
                    done: true,
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        assert!(!state.page_full());
    }

    #[test]
    fn render_frozen_page_contains_only_range() {
        let state = make_render_state(
            false,
            true,
            vec![
                TrackedTool {
                    tool_name: "First".to_owned(),
                    args: serde_json::json!({"command": "a"}),
                    content: "out-a".to_owned(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "Second".to_owned(),
                    args: serde_json::json!({"command": "b"}),
                    content: "out-b".to_owned(),
                    is_error: false,
                    done: true,
                },
                TrackedTool {
                    tool_name: "Third".to_owned(),
                    args: serde_json::json!({"command": "c"}),
                    content: "out-c".to_owned(),
                    is_error: false,
                    done: true,
                },
            ],
            "",
            false,
            "",
            ToolCallLimit::All,
            ToolOutputLines::First(10),
            false,
        );

        // Freeze just the first two tools.
        let frozen = state.render_frozen_page(0..2);
        assert!(frozen.contains("First"));
        assert!(frozen.contains("out-a"));
        assert!(frozen.contains("Second"));
        assert!(frozen.contains("out-b"));
        assert!(!frozen.contains("Third"));
        assert!(!frozen.contains("out-c"));
    }
}
