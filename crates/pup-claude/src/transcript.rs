//! Claude Code `.jsonl` transcript parser and file watcher.
//!
//! Claude Code writes all conversation data to
//! `~/.claude/projects/<project-slug>/<session-uuid>.jsonl`.
//!
//! Each line is an independent JSON object with a `type` field:
//! - `"user"` — user messages and tool results
//! - `"assistant"` — assistant responses (may appear multiple times per API call)
//! - `"file-history-snapshot"` — file backup metadata (ignored)
//! - `"progress"` — subagent/task progress (ignored)

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::debug;

use crate::session::SessionEvent;

// ── Transcript entry types ──────────────────────────────────────────────────

/// A parsed transcript entry.
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    /// User text message.
    UserText {
        uuid: String,
        session_id: String,
        timestamp: String,
        content: String,
    },
    /// Tool result (user entry with tool_result blocks).
    ToolResult {
        uuid: String,
        session_id: String,
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Assistant response (may appear multiple times per API message).
    Assistant {
        uuid: String,
        session_id: String,
        timestamp: String,
        api_message_id: String,
        model: String,
        text_blocks: Vec<String>,
        thinking_blocks: Vec<String>,
        tool_uses: Vec<ToolUseBlock>,
    },
    /// Ignored entry types (snapshots, progress, etc.).
    Ignored,
}

/// A tool_use block from an assistant message.
#[derive(Debug, Clone)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Parse a single JSONL line into a `TranscriptEntry`.
pub fn parse_line(line: &str) -> Result<TranscriptEntry> {
    let v: Value = serde_json::from_str(line).context("invalid JSON")?;

    let entry_type = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");

    match entry_type {
        "user" => parse_user_entry(&v),
        "assistant" => parse_assistant_entry(&v),
        _ => Ok(TranscriptEntry::Ignored),
    }
}

fn parse_user_entry(v: &Value) -> Result<TranscriptEntry> {
    let uuid = str_field(v, "uuid");
    let session_id = str_field(v, "sessionId");
    let timestamp = str_field(v, "timestamp");
    let message = &v["message"];
    let content = &message["content"];

    // Content can be a string (user text) or an array (tool results).
    if let Some(text) = content.as_str() {
        return Ok(TranscriptEntry::UserText {
            uuid,
            session_id,
            timestamp,
            content: text.to_owned(),
        });
    }

    if let Some(blocks) = content.as_array() {
        // Look for tool_result blocks. Return the first one found.
        // (Multiple tool results in one entry are rare but possible;
        //  we emit one TranscriptEntry per tool_result.)
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let result_content = match block.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                return Ok(TranscriptEntry::ToolResult {
                    uuid,
                    session_id,
                    tool_use_id,
                    content: result_content,
                    is_error,
                });
            }
        }

        // Array but no tool_result — might be text blocks or image blocks.
        // Extract text content.
        let mut text = String::new();
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }
        if !text.is_empty() {
            return Ok(TranscriptEntry::UserText {
                uuid,
                session_id,
                timestamp,
                content: text,
            });
        }
    }

    // Fallback: stringified content.
    Ok(TranscriptEntry::UserText {
        uuid,
        session_id,
        timestamp,
        content: content.to_string(),
    })
}

fn parse_assistant_entry(v: &Value) -> Result<TranscriptEntry> {
    let uuid = str_field(v, "uuid");
    let session_id = str_field(v, "sessionId");
    let timestamp = str_field(v, "timestamp");
    let message = &v["message"];

    let api_message_id = message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let content_blocks = message
        .get("content")
        .and_then(Value::as_array);

    let mut text_blocks = Vec::new();
    let mut thinking_blocks = Vec::new();
    let mut tool_uses = Vec::new();

    if let Some(blocks) = content_blocks {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        text_blocks.push(t.to_owned());
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        thinking_blocks.push(t.to_owned());
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    tool_uses.push(ToolUseBlock { id, name, input });
                }
                _ => {}
            }
        }
    }

    Ok(TranscriptEntry::Assistant {
        uuid,
        session_id,
        timestamp,
        api_message_id,
        model,
        text_blocks,
        thinking_blocks,
        tool_uses,
    })
}

fn str_field(v: &Value, field: &str) -> String {
    v.get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

// ── Transcript Watcher ──────────────────────────────────────────────────────

/// Tracks state for a single assistant API message across multiple transcript entries.
#[derive(Debug)]
struct AssistantMessageState {
    text: String,
    thinking: String,
    tool_use_count: usize,
    model: String,
}

/// Watches a `.jsonl` transcript file and emits `SessionEvent`s.
#[derive(Debug)]
pub struct TranscriptWatcher {
    session_id: String,
    path: PathBuf,
    offset: u64,
    /// Track the latest state per API message ID.
    seen_messages: HashMap<String, AssistantMessageState>,
    /// Tool use IDs we've already emitted `ToolStart` for.
    seen_tool_starts: HashSet<String>,
    /// The API message ID we're currently streaming (pending `MessageEnd`).
    pending_message_id: Option<String>,
    /// Last time we saw any new transcript content.
    last_activity: Instant,
    /// Whether we've emitted the initial `AgentStart` for the current turn.
    agent_started: bool,
}

impl TranscriptWatcher {
    /// Create a new watcher. Starts at the current end of the file (only watches
    /// new content). Use `new_from_beginning` to parse history.
    pub fn new(session_id: String, path: PathBuf) -> Result<Self> {
        let offset = std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(0);

        Ok(Self {
            session_id,
            path,
            offset,
            seen_messages: HashMap::new(),
            seen_tool_starts: HashSet::new(),
            pending_message_id: None,
            last_activity: Instant::now(),
            agent_started: false,
        })
    }

    /// Create a watcher starting from the beginning of the file (for history replay).
    pub fn new_from_beginning(session_id: String, path: PathBuf) -> Self {
        Self {
            session_id,
            path,
            offset: 0,
            seen_messages: HashMap::new(),
            seen_tool_starts: HashSet::new(),
            pending_message_id: None,
            last_activity: Instant::now(),
            agent_started: false,
        }
    }

    /// Poll the transcript file for new entries. Returns any new `SessionEvent`s.
    ///
    /// Call this on a timer (e.g. every 500ms).
    pub fn poll(&mut self) -> Result<Vec<SessionEvent>> {
        let metadata = std::fs::metadata(&self.path);
        let file_len = match metadata {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e).context("failed to stat transcript file"),
        };

        if file_len <= self.offset {
            // No new data. Check stale timeout.
            return Ok(self.maybe_flush_stale());
        }

        let file = std::fs::File::open(&self.path)
            .context("failed to open transcript file")?;
        let mut reader = BufReader::new(file);
        reader
            .seek(SeekFrom::Start(self.offset))
            .context("failed to seek in transcript file")?;

        let mut events = Vec::new();
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .context("failed to read transcript line")?;

            if bytes_read == 0 {
                break;
            }

            // Only process complete lines (ending with newline).
            if !line.ends_with('\n') {
                // Partial line — don't advance offset, try again next poll.
                break;
            }

            self.offset += bytes_read as u64;
            self.last_activity = Instant::now();

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_line(trimmed) {
                Ok(entry) => events.extend(self.process_entry(entry)),
                Err(e) => {
                    debug!(error = %e, "skipping unparseable transcript line");
                }
            }
        }

        // Check stale flush even if we processed entries.
        events.extend(self.maybe_flush_stale());

        Ok(events)
    }

    /// Process a parsed transcript entry into session events.
    fn process_entry(&mut self, entry: TranscriptEntry) -> Vec<SessionEvent> {
        match entry {
            TranscriptEntry::UserText {
                content,
                session_id,
                ..
            } => {
                let mut events = Vec::new();
                // Flush any pending assistant message.
                events.extend(self.flush_pending());
                self.agent_started = false;
                // Use the session_id from the transcript if we don't have one yet.
                if self.session_id.is_empty() && !session_id.is_empty() {
                    self.session_id = session_id;
                }
                events.push(SessionEvent::UserMessage {
                    session_id: self.session_id.clone(),
                    content,
                });
                events
            }
            TranscriptEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                let mut events = Vec::new();
                // A tool result means the pending assistant message is done.
                events.extend(self.flush_pending());
                events.push(SessionEvent::ToolEnd {
                    session_id: self.session_id.clone(),
                    tool_use_id,
                    content,
                    is_error,
                });
                events
            }
            TranscriptEntry::Assistant {
                api_message_id,
                model,
                text_blocks,
                thinking_blocks,
                tool_uses,
                ..
            } => {
                self.process_assistant(
                    api_message_id,
                    model,
                    text_blocks,
                    thinking_blocks,
                    tool_uses,
                )
            }
            TranscriptEntry::Ignored => vec![],
        }
    }

    /// Process an assistant transcript entry.
    fn process_assistant(
        &mut self,
        api_message_id: String,
        model: String,
        text_blocks: Vec<String>,
        thinking_blocks: Vec<String>,
        tool_uses: Vec<ToolUseBlock>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();

        // Emit AgentStart on the first assistant entry of a turn.
        if !self.agent_started {
            self.agent_started = true;
            events.push(SessionEvent::AgentStart {
                session_id: self.session_id.clone(),
            });
        }

        let is_new_message = !self.seen_messages.contains_key(&api_message_id);

        if is_new_message {
            // New API message — flush any previous pending message first.
            if self.pending_message_id.is_some() {
                events.extend(self.flush_pending());
            }

            events.push(SessionEvent::MessageStart {
                session_id: self.session_id.clone(),
                message_id: api_message_id.clone(),
            });
        }

        // Emit ToolStart for any new tool_use blocks.
        for tool in &tool_uses {
            if self.seen_tool_starts.insert(tool.id.clone()) {
                events.push(SessionEvent::ToolStart {
                    session_id: self.session_id.clone(),
                    tool_use_id: tool.id.clone(),
                    tool_name: tool.name.clone(),
                    input: tool.input.clone(),
                });
            }
        }

        // Update tracked state.
        let joined_text = text_blocks.join("\n");
        let joined_thinking = thinking_blocks.join("\n");

        self.seen_messages
            .entry(api_message_id.clone())
            .and_modify(|state| {
                state.text = joined_text.clone();
                state.thinking = joined_thinking.clone();
                state.tool_use_count = tool_uses.len();
                if !model.is_empty() {
                    state.model.clone_from(&model);
                }
            })
            .or_insert(AssistantMessageState {
                text: joined_text,
                thinking: joined_thinking,
                tool_use_count: tool_uses.len(),
                model,
            });

        self.pending_message_id = Some(api_message_id);

        events
    }

    /// Flush the pending assistant message, emitting `MessageEnd` and possibly
    /// `AgentEnd`.
    fn flush_pending(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();

        if let Some(msg_id) = self.pending_message_id.take() {
            if let Some(state) = self.seen_messages.get(&msg_id) {
                events.push(SessionEvent::MessageEnd {
                    session_id: self.session_id.clone(),
                    message_id: msg_id,
                    text: state.text.clone(),
                    thinking: if state.thinking.is_empty() {
                        None
                    } else {
                        Some(state.thinking.clone())
                    },
                });
            }
        }

        events
    }

    /// If no new content has arrived for the stale timeout, flush pending.
    fn maybe_flush_stale(&mut self) -> Vec<SessionEvent> {
        if self.pending_message_id.is_some()
            && self.last_activity.elapsed() > Duration::from_secs(3)
        {
            let mut events = self.flush_pending();
            // Also emit AgentEnd since the turn appears to be complete.
            if self.agent_started {
                self.agent_started = false;
                events.push(SessionEvent::AgentEnd {
                    session_id: self.session_id.clone(),
                });
            }
            events
        } else {
            vec![]
        }
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the transcript file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parse existing transcript content for initial history.
    /// Returns the model name (if found) and a list of turns.
    pub fn parse_history(&mut self) -> Result<(Option<String>, Vec<pup_ipc::Turn>)> {
        let file = std::fs::File::open(&self.path)
            .context("failed to open transcript for history")?;
        let reader = BufReader::new(file);

        let mut turns: Vec<pup_ipc::Turn> = Vec::new();
        let mut current_turn: Option<pup_ipc::Turn> = None;
        let mut model: Option<String> = None;
        let mut tool_results: HashMap<String, (String, bool)> = HashMap::new();

        for line_result in reader.lines() {
            let line = line_result.context("failed to read line")?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let entry = match parse_line(trimmed) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match entry {
                TranscriptEntry::UserText {
                    content, timestamp, ..
                } => {
                    // Finalize any current turn.
                    if let Some(turn) = current_turn.take() {
                        turns.push(turn);
                    }
                    let ts = parse_timestamp(&timestamp);
                    current_turn = Some(pup_ipc::Turn {
                        user: Some(pup_ipc::TurnMessage {
                            content,
                            timestamp: ts,
                        }),
                        assistant: None,
                        tool_calls: Vec::new(),
                    });
                }
                TranscriptEntry::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    tool_results.insert(tool_use_id, (content, is_error));
                }
                TranscriptEntry::Assistant {
                    text_blocks,
                    tool_uses,
                    model: entry_model,
                    timestamp,
                    ..
                } => {
                    if !entry_model.is_empty() {
                        model = Some(entry_model);
                    }
                    let ts = parse_timestamp(&timestamp);
                    let text = text_blocks.join("\n");

                    // Ensure we have a turn to attach to.
                    let turn = current_turn.get_or_insert_with(|| pup_ipc::Turn {
                        user: None,
                        assistant: None,
                        tool_calls: Vec::new(),
                    });

                    // Update assistant message (last one wins for text).
                    if !text.is_empty() {
                        turn.assistant = Some(pup_ipc::TurnMessage {
                            content: text,
                            timestamp: ts,
                        });
                    }

                    // Add tool calls.
                    for tool in tool_uses {
                        let (result_content, is_error) = tool_results
                            .remove(&tool.id)
                            .unwrap_or_default();
                        turn.tool_calls.push(pup_ipc::ToolCall {
                            tool_call_id: tool.id,
                            tool_name: tool.name,
                            args: tool.input,
                            content: result_content,
                            is_error,
                        });
                    }
                }
                TranscriptEntry::Ignored => {}
            }
        }

        // Finalize last turn.
        if let Some(turn) = current_turn {
            turns.push(turn);
        }

        // Set offset to end of file so poll() only sees new content.
        self.offset = std::fs::metadata(&self.path)
            .map(|m| m.len())
            .unwrap_or(0);

        Ok((model, turns))
    }
}

/// Parse an ISO 8601 timestamp string to epoch milliseconds.
fn parse_timestamp(ts: &str) -> u64 {
    // Simple parse: try to extract epoch ms from ISO 8601.
    // Format: "2026-02-22T16:15:46.051Z"
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        #[allow(clippy::cast_sign_loss)]
        let ms = dt.timestamp_millis() as u64;
        ms
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_text() {
        let line = r#"{"parentUuid":null,"isSidechain":false,"userType":"external","cwd":"/root","sessionId":"abc","version":"2.1.34","type":"user","message":{"role":"user","content":"hello world"},"uuid":"u1","timestamp":"2026-02-22T16:15:46.051Z"}"#;
        let entry = parse_line(line).unwrap();
        match entry {
            TranscriptEntry::UserText { content, uuid, session_id, .. } => {
                assert_eq!(content, "hello world");
                assert_eq!(uuid, "u1");
                assert_eq!(session_id, "abc");
            }
            other => panic!("expected UserText, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_assistant() {
        let line = r#"{"parentUuid":"u1","sessionId":"abc","version":"2.1.34","message":{"model":"claude-opus-4-6","id":"msg_01X","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"let me think"},{"type":"text","text":"Hello!"},{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls"}}],"stop_reason":null},"type":"assistant","uuid":"a1","timestamp":"2026-02-22T16:15:48.044Z"}"#;
        let entry = parse_line(line).unwrap();
        match entry {
            TranscriptEntry::Assistant {
                api_message_id,
                model,
                text_blocks,
                thinking_blocks,
                tool_uses,
                ..
            } => {
                assert_eq!(api_message_id, "msg_01X");
                assert_eq!(model, "claude-opus-4-6");
                assert_eq!(text_blocks, vec!["Hello!"]);
                assert_eq!(thinking_blocks, vec!["let me think"]);
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].name, "Bash");
                assert_eq!(tool_uses[0].id, "toolu_01");
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_tool_result() {
        let line = r#"{"sessionId":"abc","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"file.txt","is_error":false}]},"uuid":"u2","timestamp":"2026-02-22T16:16:00.000Z"}"#;
        let entry = parse_line(line).unwrap();
        match entry {
            TranscriptEntry::ToolResult { tool_use_id, content, is_error, .. } => {
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, "file.txt");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_snapshot_ignored() {
        let line = r#"{"type":"file-history-snapshot","messageId":"x","snapshot":{}}"#;
        let entry = parse_line(line).unwrap();
        assert!(matches!(entry, TranscriptEntry::Ignored));
    }
}
