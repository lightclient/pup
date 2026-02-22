use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::bot::BotClient;
use crate::outbox::{Outbox, OutboxOp};
use crate::render::{
    cancel_keyboard, empty_keyboard, escape_html, split_message, to_telegram_html, MAX_BODY_CHARS,
};

/// How many recent tool calls to keep in the rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallLimit {
    /// Keep only the last N tool calls.
    Last(usize),
    /// Keep all tool calls.
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
    // Replace the trailing empty-string join with the indicator line.
    let indicator = format!(". . . ({remaining} more lines)");
    // The last element was "" so the join appended "\n" at the end.
    // We want to replace that trailing empty segment with the indicator.
    // Actually, `kept` ended with "" so result ends with "\n".
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
    /// Whether the model is currently in a thinking/reasoning phase.
    thinking: bool,
    /// Accumulated thinking/chain-of-thought text.
    thinking_text: String,
    /// Last time we sent an edit to Telegram.
    last_edit: Instant,
    /// Whether content has changed since the last edit.
    dirty: bool,
    /// Sender to stop the typing indicator loop (dropped on turn end).
    typing_stop: Option<tokio::sync::watch::Sender<bool>>,
    /// Whether to show tool call details.
    verbose: bool,
    /// How many tool calls to keep in the rendered message.
    tool_call_limit: ToolCallLimit,
    /// How many lines of tool output to show per tool call.
    tool_output_lines: ToolOutputLines,
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
    ///
    /// If `include_text` is true, the streaming assistant text is appended
    /// (used during the turn). If false, only tools/thinking are rendered
    /// (used at turn end when the text goes to a separate message).
    fn render_parts(&self, include_text: bool) -> String {
        let mut parts = Vec::new();

        // Verbose tool call summaries (limited to the configured window).
        if self.verbose {
            let tools: &[TrackedTool] = match self.tool_call_limit {
                ToolCallLimit::All => &self.tools,
                ToolCallLimit::Last(n) => {
                    let start = self.tools.len().saturating_sub(n);
                    &self.tools[start..]
                }
            };
            for tool in tools {
                let mut line = format!("<b>{}</b>", escape_html(&tool.tool_name));
                // Show command or path arg if present.
                if let Some(cmd) = tool.args.get("command").and_then(|v| v.as_str()) {
                    let truncated = if cmd.len() > 200 { &cmd[..200] } else { cmd };
                    line.push_str(&format!("\n<pre>{}</pre>", escape_html(truncated)));
                } else if let Some(path) = tool.args.get("path").and_then(|v| v.as_str()) {
                    line.push_str(&format!(" <code>{}</code>", escape_html(path)));
                }
                // Show tool output (truncated by line limit).
                if !tool.content.is_empty() {
                    let truncated = truncate_tool_output(&tool.content, self.tool_output_lines);
                    if !truncated.is_empty() {
                        line.push_str(&format!(
                            "\n<pre>{}</pre>",
                            escape_html(&truncated)
                        ));
                    }
                }
                parts.push(line);
            }
        }

        // Thinking content (shown while model is reasoning, before response text).
        if self.thinking && self.streaming_text.is_empty() {
            if self.thinking_text.is_empty() {
                parts.push("<i>Thinking…</i>".to_owned());
            } else {
                // Show the tail of the thinking text (most recent reasoning).
                // Cap at 2000 chars to leave room for tools/formatting within
                // Telegram's 4096 char message limit.
                const MAX_THINKING_DISPLAY: usize = 2000;
                let display = if self.thinking_text.len() > MAX_THINKING_DISPLAY {
                    let start = self.thinking_text.len() - MAX_THINKING_DISPLAY;
                    // Don't split mid-char.
                    let safe_start = self.thinking_text[start..]
                        .char_indices()
                        .next()
                        .map(|(i, _)| start + i)
                        .unwrap_or(start);
                    format!("…{}", &self.thinking_text[safe_start..])
                } else {
                    self.thinking_text.clone()
                };
                parts.push(format!("<i>{}</i>", escape_html(&display)));
            }
        }

        // Streaming text (only during the turn, not at finalization).
        if include_text && !self.streaming_text.is_empty() {
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

    /// Render the full turn state (tools + thinking + text) for mid-turn edits.
    fn render(&self) -> String {
        self.render_parts(true)
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
    /// How many tool calls to keep in the rendered message.
    tool_call_limit: ToolCallLimit,
    /// How many lines of tool output to show per tool call.
    tool_output_lines: ToolOutputLines,
}

impl TurnTracker {
    pub fn new(edit_interval_ms: u64) -> Self {
        Self {
            turns: HashMap::new(),
            edit_interval_ms,
            verbose: false,
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

    /// Enable or disable verbose mode (tool call visibility).
    ///
    /// Updates the tracker default AND all currently active turns so the
    /// change takes effect immediately — not just on the next turn.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
        for state in self.turns.values_mut() {
            state.verbose = verbose;
        }
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
                thinking: false,
                thinking_text: String::new(),
                last_edit: Instant::now(),
                dirty: false,
                typing_stop: Some(stop_tx),
                verbose: self.verbose,
                tool_call_limit: self.tool_call_limit,
                tool_output_lines: self.tool_output_lines,
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
        let verbose = self.turns.get(session_id).map_or(false, |s| s.verbose);
        if !verbose {
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
            && state.verbose
        {
            // Find the last matching in-progress tool and append content.
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
            && state.verbose
        {
            // Find the last matching tool and mark it done.
            for tool in state.tools.iter_mut().rev() {
                if tool.tool_name == tool_name && !tool.done {
                    tool.done = true;
                    tool.is_error = is_error;
                    // Use the final content from tool_end if we didn't
                    // accumulate anything via tool_update deltas.
                    if tool.content.is_empty() && !content.is_empty() {
                        content.clone_into(&mut tool.content);
                    }
                    break;
                }
            }
            state.dirty = true;
            state.flush(outbox, self.edit_interval_ms);
        }
    }

    /// Note that thinking/reasoning content is streaming.
    pub fn thinking_delta(&mut self, session_id: &str, text: &str, outbox: &mut Outbox) {
        let verbose = self.turns.get(session_id).map_or(false, |s| s.verbose);
        if let Some(state) = self.turns.get_mut(session_id) {
            state.thinking = true;
            state.thinking_text.push_str(text);
        }
        if verbose {
            self.ensure_message(session_id, "<i>Thinking…</i>", outbox);
            if let Some(state) = self.turns.get_mut(session_id) {
                state.dirty = true;
                state.flush(outbox, self.edit_interval_ms);
            }
        }
    }

    /// Accumulate a streaming text delta.
    pub fn message_delta(&mut self, session_id: &str, text: &str, outbox: &mut Outbox) {
        let verbose = self.turns.get(session_id).map_or(false, |s| s.verbose);

        if verbose {
            // If no message sent yet, send with the first chunk of text.
            let initial = {
                let needs_send = self
                    .turns
                    .get(session_id)
                    .map_or(false, |s| s.telegram_message_id.is_none() && !s.send_pending);
                if needs_send {
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
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        // First text delta means thinking is done.
        state.thinking = false;
        state.streaming_text.push_str(text);

        if state.verbose {
            state.dirty = true;
            state.flush(outbox, self.edit_interval_ms);
        }
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
        let verbose = self.turns.get(session_id).map_or(false, |s| s.verbose);

        if verbose {
            // If no deltas were received (e.g. very short response with extended
            // thinking), ensure a Telegram message is created with the final content.
            if !content.is_empty() {
                let html = to_telegram_html(content);
                let initial = if html.is_empty() { "…".to_owned() } else { html };
                self.ensure_message(session_id, &initial, outbox);
            }
        }

        let Some(state) = self.turns.get_mut(session_id) else {
            return;
        };

        if !content.is_empty() {
            state.streaming_text = content.to_owned();
        }

        if state.verbose {
            state.dirty = true;
            // Force an immediate edit (bypass throttle) so the final content
            // is shown promptly.
            state.flush(outbox, 0);
        }
    }

    /// Finalize the turn.
    ///
    /// The existing Telegram message is updated to show only the
    /// tools/thinking summary (with the cancel keyboard removed).
    /// The final assistant text is sent as a **separate** message so
    /// the user can scroll back to the tool trace independently.
    pub fn end_turn(&mut self, session_id: &str, outbox: &mut Outbox) {
        let Some(mut state) = self.turns.remove(session_id) else {
            return;
        };

        // Stop the typing indicator (dropping the sender closes the channel).
        state.typing_stop.take();

        // Resolve message ID if still pending.
        state.try_resolve_message_id();

        let has_verbose_content = state.verbose && (!state.tools.is_empty() || !state.thinking_text.is_empty());
        let has_text = !state.streaming_text.is_empty();

        if let Some(tg_msg_id) = state.telegram_message_id {
            if has_verbose_content && has_text {
                // Edit the existing message to show only tools/thinking
                // (strip the streaming text that was shown during the turn).
                let summary = state.render_parts(false);
                let summary_chunks = split_message(&summary, MAX_BODY_CHARS);
                if let Some(first) = summary_chunks.first() {
                    outbox.enqueue(OutboxOp::Edit {
                        chat_id: state.chat_id,
                        message_id: tg_msg_id,
                        text: first.clone(),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: Some(empty_keyboard()),
                    });
                }

                // Send final assistant text as a separate message.
                let text_html = to_telegram_html(&state.streaming_text);
                if !text_html.is_empty() {
                    let text_chunks = split_message(&text_html, MAX_BODY_CHARS);
                    for chunk in &text_chunks {
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
            } else if has_text {
                // Non-verbose or no tool/thinking content: the existing
                // message already has the text — just remove the keyboard.
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
            } else {
                // No text at all (tools-only turn) — just remove the keyboard.
                let rendered = state.render_parts(false);
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
            }
        } else if has_text {
            // No Telegram message was ever created (e.g. no deltas arrived,
            // or the send is still in flight). Send the final content as a
            // new message.
            let text_html = to_telegram_html(&state.streaming_text);
            let chunks = split_message(&text_html, MAX_BODY_CHARS);
            for chunk in &chunks {
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
        assert_eq!(
            result,
            "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n. . . (1 more lines)"
        );
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
        assert_eq!(
            truncate_tool_output("", ToolOutputLines::First(0)),
            ""
        );
    }

    #[test]
    fn truncate_limit_one() {
        let input = "first\nsecond\nthird";
        let result = truncate_tool_output(input, ToolOutputLines::First(1));
        assert_eq!(result, "first\n. . . (2 more lines)");
    }

    #[test]
    fn truncate_many_over_default() {
        // 25 lines, default limit is 10
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
        // 13 lines (some empty), limit 5
        let result = truncate_tool_output(input, ToolOutputLines::First(5));
        assert_eq!(result, "a\n\nb\n\nc\n. . . (8 more lines)");
    }

    // ── ToolOutputLines default ─────────────────────────────────

    #[test]
    fn tool_output_lines_default_is_10() {
        assert_eq!(ToolOutputLines::default(), ToolOutputLines::First(10));
    }

    // ── TurnTracker tool output rendering ──────────────────────

    #[test]
    fn render_tool_with_output() {
        let state = TurnState {
            chat_id: 1,
            thread_id: None,
            session_id: "s1".to_owned(),
            telegram_message_id: None,
            send_pending: false,
            send_rx: None,
            streaming_text: String::new(),
            thinking: false,
            thinking_text: String::new(),
            last_edit: Instant::now(),
            dirty: false,
            typing_stop: None,
            verbose: true,
            tool_call_limit: ToolCallLimit::All,
            tool_output_lines: ToolOutputLines::First(3),
            tools: vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls -la"}),
                content: "file1.txt\nfile2.txt\nfile3.txt\nfile4.txt\nfile5.txt".to_owned(),
                is_error: false,
                done: true,
            }],
        };

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
        let state = TurnState {
            chat_id: 1,
            thread_id: None,
            session_id: "s1".to_owned(),
            telegram_message_id: None,
            send_pending: false,
            send_rx: None,
            streaming_text: String::new(),
            thinking: false,
            thinking_text: String::new(),
            last_edit: Instant::now(),
            dirty: false,
            typing_stop: None,
            verbose: true,
            tool_call_limit: ToolCallLimit::All,
            tool_output_lines: ToolOutputLines::All,
            tools: vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "ls"}),
                content: "a\nb\nc\nd\ne".to_owned(),
                is_error: false,
                done: true,
            }],
        };

        let rendered = state.render_parts(true);
        assert!(rendered.contains("a\nb\nc\nd\ne"));
        assert!(!rendered.contains(". . ."));
    }

    #[test]
    fn render_tool_no_output() {
        let state = TurnState {
            chat_id: 1,
            thread_id: None,
            session_id: "s1".to_owned(),
            telegram_message_id: None,
            send_pending: false,
            send_rx: None,
            streaming_text: String::new(),
            thinking: false,
            thinking_text: String::new(),
            last_edit: Instant::now(),
            dirty: false,
            typing_stop: None,
            verbose: true,
            tool_call_limit: ToolCallLimit::All,
            tool_output_lines: ToolOutputLines::First(10),
            tools: vec![TrackedTool {
                tool_name: "Read".to_owned(),
                args: serde_json::json!({"path": "/tmp/foo.txt"}),
                content: String::new(),
                is_error: false,
                done: false,
            }],
        };

        let rendered = state.render_parts(true);
        assert!(rendered.contains("<b>Read</b>"));
        assert!(rendered.contains("/tmp/foo.txt"));
        // No <pre> block for empty content.
        assert!(!rendered.contains("<pre>"));
    }

    #[test]
    fn render_nonverbose_hides_tool_output() {
        let state = TurnState {
            chat_id: 1,
            thread_id: None,
            session_id: "s1".to_owned(),
            telegram_message_id: None,
            send_pending: false,
            send_rx: None,
            streaming_text: "hello".to_owned(),
            thinking: false,
            thinking_text: String::new(),
            last_edit: Instant::now(),
            dirty: false,
            typing_stop: None,
            verbose: false,
            tool_call_limit: ToolCallLimit::All,
            tool_output_lines: ToolOutputLines::First(10),
            tools: vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "echo hi"}),
                content: "hi".to_owned(),
                is_error: false,
                done: true,
            }],
        };

        let rendered = state.render_parts(true);
        // Non-verbose: no tool header or output, just the streaming text.
        assert!(!rendered.contains("Bash"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn render_tool_output_html_escaped() {
        let state = TurnState {
            chat_id: 1,
            thread_id: None,
            session_id: "s1".to_owned(),
            telegram_message_id: None,
            send_pending: false,
            send_rx: None,
            streaming_text: String::new(),
            thinking: false,
            thinking_text: String::new(),
            last_edit: Instant::now(),
            dirty: false,
            typing_stop: None,
            verbose: true,
            tool_call_limit: ToolCallLimit::All,
            tool_output_lines: ToolOutputLines::All,
            tools: vec![TrackedTool {
                tool_name: "Bash".to_owned(),
                args: serde_json::json!({"command": "echo '<html>'"}),
                content: "<html>&amp;".to_owned(),
                is_error: false,
                done: true,
            }],
        };

        let rendered = state.render_parts(true);
        assert!(rendered.contains("&lt;html&gt;&amp;amp;"));
    }
}
