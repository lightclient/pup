pub mod bot;
pub mod dm;
pub mod outbox;
pub mod render;
pub mod topics;
pub mod turn_tracker;
pub(crate) mod whisper;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use pup_core::ChatBackend;
use pup_core::types::{IncomingMessage, MessageSource, SessionEvent, SessionInfo};
use pup_ipc::SendMode;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span, warn};

use bot::{BotClient, Update};
use dm::{DmCommand, DmState, ResolveResult};
use outbox::{ChatBudget, Outbox, OutboxOp};
use render::{escape_html, format_history, format_user_message};
use topics::TopicsManager;
use turn_tracker::TurnTracker;

/// A pending interactive prompt for a command that requires an argument.
/// When the user selects such a command from the Telegram menu without
/// providing an argument, the daemon asks them for input and stores this
/// state until their next message.
#[derive(Debug, Clone)]
struct PendingPrompt {
    /// The slash command name (without the leading `/`).
    command: String,
    /// If true, this is a pup-level command handled locally (e.g. /verbose).
    /// If false, the completed command is forwarded to the session via IPC.
    local: bool,
}

/// If a slash command requires an argument and was invoked without one,
/// return the question to ask the user.
fn prompt_for_command(cmd: &str) -> Option<&'static str> {
    match cmd {
        "name" => Some("What name would you like to set?"),
        _ => None,
    }
}

/// Format help text for topic mode (commands available inside a session topic).
fn format_topics_help() -> String {
    [
        "<b>pup — Telegram bridge for pi</b>",
        "",
        "<b>Commands:</b>",
        "/cancel — Abort the current agent operation",
        "/status — Show session status (model, context usage)",
        "/verbose [on|off] — Toggle verbose mode (thinking + tools)",
        "/thinking [on|off] — Toggle thinking/reasoning display",
        "/tools [on|off] — Toggle tool call display",
        "/compact — Compact session context",
        "/name &lt;name&gt; — Set session name",
        "/new — Start a new session",
        "/quit — Quit pi session",
        "/help — Show this help",
        "",
        "<b>Messaging:</b>",
        "Type normally to send a message (interrupts agent).",
        "Prefix with &gt;&gt; to queue as follow-up.",
    ]
    .join("\n")
}

/// Spawn background typing indicator loops for a session, one per
/// destination.  Running until the stored senders are dropped.  Does
/// nothing if loops are already running for the given session.
fn spawn_typing_loops(
    pre_turn_typing: &mut HashMap<String, Vec<tokio::sync::watch::Sender<bool>>>,
    bot: &BotClient,
    chat_budget: &ChatBudget,
    session_id: &str,
    destinations: &[(i64, Option<i64>)],
) {
    if pre_turn_typing.contains_key(session_id) || destinations.is_empty() {
        return;
    }
    let mut senders = Vec::with_capacity(destinations.len());
    for &(chat_id, thread_id) in destinations {
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let bot = bot.clone();
        let budget = chat_budget.clone();
        let sid = session_id.to_owned();
        tokio::spawn(async move {
            loop {
                // Only send if the shared per-chat budget allows it.
                if budget.try_consume(chat_id) {
                    let _ = bot.send_chat_action(chat_id, "typing", thread_id).await;
                }
                tokio::select! {
                    () = async { let _ = stop_rx.changed().await; } => break,
                    () = tokio::time::sleep(std::time::Duration::from_secs(4)) => {}
                }
            }
            debug!(session_id = %sid, "pre-turn typing indicator stopped");
        });
        senders.push(stop_tx);
    }
    pre_turn_typing.insert(session_id.to_owned(), senders);
}

/// Configuration for the Telegram backend.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Vec<i64>,
    pub dm_enabled: bool,
    pub topics_enabled: bool,
    pub supergroup_id: Option<i64>,
    pub topic_icon: String,
    pub max_message_length: usize,
    pub edit_interval_ms: u64,
    pub thinking: bool,
    pub tools: bool,
    pub history_turns: usize,
    /// Path to `~/.pi/pup/` — used to scan for live sessions on startup.
    pub socket_dir: PathBuf,
    /// Path to the topics state file for persisting the topic mapping.
    pub topics_state_path: PathBuf,
    /// Enable local voice-to-text transcription via whisper.cpp.
    pub voice: bool,
    /// How many tool calls to keep in the rendered message.
    pub tool_call_limit: turn_tracker::ToolCallLimit,
    /// How many lines of tool output to show per tool call.
    pub tool_output_lines: turn_tracker::ToolOutputLines,
}

/// The Telegram chat backend.
#[derive(Debug)]
pub struct TelegramBackend {
    config: TelegramConfig,
    bot: BotClient,
    bot_user_id: i64,
    /// Outbox for rate-limited API calls.
    outbox: Outbox,
    /// Per-session turn tracker (single message per agent turn).
    turn_tracker: TurnTracker,
    /// DM state (per-user, but we only support one user for now).
    dm: DmState,
    /// Topics manager (if topics mode enabled).
    topics: Option<TopicsManager>,
    /// Update offset for long polling.
    update_offset: i64,
    /// Known sessions for the /ls command.
    sessions: Vec<SessionInfo>,
    /// Incoming messages to send to the session manager.
    incoming_tx: mpsc::Sender<IncomingMessage>,
    incoming_rx: Option<mpsc::Receiver<IncomingMessage>>,
    /// Chat ID for DM mode (set on first message from allowed user).
    dm_chat_id: Option<i64>,
    /// Pending interactive prompts, keyed by session ID.
    pending_prompts: HashMap<String, PendingPrompt>,
    /// Pending DM-level interactive prompt (not per-session, e.g. /verbose).
    pending_dm_prompt: Option<PendingPrompt>,
    /// Whisper transcriber for voice messages (loaded on start).
    transcriber: Option<Arc<Mutex<whisper::Transcriber>>>,
    /// Pre-turn typing indicators, keyed by session ID.
    /// Active from message receipt until AgentStart (where the turn
    /// tracker starts its own typing loop).
    pre_turn_typing: HashMap<String, Vec<tokio::sync::watch::Sender<bool>>>,
}

impl TelegramBackend {
    pub fn new(config: TelegramConfig) -> Self {
        let bot = BotClient::new(&config.bot_token);
        let chat_budget = ChatBudget::new();
        let outbox = Outbox::new(bot.clone(), config.edit_interval_ms, chat_budget);
        let mut turn_tracker = TurnTracker::new(config.edit_interval_ms);
        turn_tracker.set_default_thinking(config.thinking);
        turn_tracker.set_default_tools(config.tools);
        turn_tracker.set_tool_call_limit(config.tool_call_limit);
        turn_tracker.set_tool_output_lines(config.tool_output_lines);
        let (incoming_tx, incoming_rx) = mpsc::channel(64);

        let topics = if config.topics_enabled {
            config.supergroup_id.map(|id| {
                TopicsManager::new(
                    id,
                    config.topic_icon.clone(),
                    config.topics_state_path.clone(),
                )
            })
        } else {
            None
        };

        Self {
            config,
            bot,
            bot_user_id: 0,
            outbox,
            turn_tracker,
            dm: DmState::default(),
            topics,
            update_offset: 0,
            sessions: Vec::new(),
            incoming_tx,
            incoming_rx: Some(incoming_rx),
            dm_chat_id: None,
            pending_prompts: HashMap::new(),
            pending_dm_prompt: None,
            transcriber: None,
            pre_turn_typing: HashMap::new(),
        }
    }

    /// Create a backend with a custom [`BotClient`] (for testing).
    #[cfg(test)]
    fn with_bot(config: TelegramConfig, bot: BotClient) -> Self {
        let outbox = Outbox::new(bot.clone(), config.edit_interval_ms, ChatBudget::new());
        let mut turn_tracker = TurnTracker::new(config.edit_interval_ms);
        turn_tracker.set_default_thinking(config.thinking);
        turn_tracker.set_default_tools(config.tools);
        turn_tracker.set_tool_call_limit(config.tool_call_limit);
        turn_tracker.set_tool_output_lines(config.tool_output_lines);
        let (incoming_tx, incoming_rx) = mpsc::channel(64);

        Self {
            config,
            bot,
            bot_user_id: 0,
            outbox,
            turn_tracker,
            dm: DmState::default(),
            topics: None,
            update_offset: 0,
            sessions: Vec::new(),
            incoming_tx,
            incoming_rx: Some(incoming_rx),
            dm_chat_id: None,
            pending_prompts: HashMap::new(),
            pending_dm_prompt: None,
            transcriber: None,
            pre_turn_typing: HashMap::new(),
        }
    }

    /// Check if a user ID is allowed.
    fn is_allowed(&self, user_id: i64) -> bool {
        self.config.allowed_user_ids.contains(&user_id)
    }

    /// Get the chat ID for sending messages about a session.
    #[allow(dead_code)]
    fn chat_id_for_session(&self, session_id: &str) -> Option<i64> {
        // Topics mode: use the supergroup.
        if let Some(ref topics) = self.topics
            && topics.thread_for_session(session_id).is_some()
        {
            return Some(topics.chat_id());
        }

        // DM mode: use the DM chat if attached to this session.
        if self.dm.attached.as_deref() == Some(session_id) {
            return self.dm_chat_id;
        }

        None
    }

    /// Delete any topics whose grace period has expired.
    async fn check_pending_deletions(&mut self) {
        let expired = match self.topics {
            Some(ref mut topics) => topics.drain_expired(),
            None => return,
        };
        let chat_id = self
            .topics
            .as_ref()
            .expect("topics checked above")
            .chat_id();
        for (session_id, thread_id) in expired {
            info!(session_id = %session_id, thread_id, "deleting expired topic");
            if let Err(e) = self.bot.delete_forum_topic(chat_id, thread_id).await {
                warn!(
                    session_id = %session_id,
                    thread_id,
                    error = %e,
                    "failed to delete expired topic"
                );
            }
        }
    }

    /// Handle a Telegram update (message or callback query).
    #[allow(clippy::too_many_lines)]
    async fn handle_update(&mut self, update: Update) {
        // Handle callback queries (cancel button).
        if let Some(cb) = update.callback_query {
            if !self.is_allowed(cb.from.id) {
                return;
            }
            if let Some(data) = &cb.data
                && let Some(session_id) = data.strip_prefix("cancel:")
            {
                // Send the abort to the session FIRST — the Telegram API
                // call below can take 250ms–2s and the agent may finish
                // before the abort arrives if we wait.
                let _ = self
                    .incoming_tx
                    .send(IncomingMessage {
                        session_id: session_id.to_owned(),
                        text: String::new(),
                        mode: SendMode::Steer,
                        is_cancel: true,
                    })
                    .await;
                // Answer the callback query in the background so we don't
                // block the poll loop.
                let bot = self.bot.clone();
                let cb_id = cb.id.clone();
                tokio::spawn(async move {
                    let _ = bot.answer_callback_query(&cb_id, Some("Cancelling…")).await;
                });
            }
            return;
        }

        // Handle messages.
        let Some(message) = update.message else {
            return;
        };
        let Some(ref from) = message.from else {
            return;
        };
        if !self.is_allowed(from.id) {
            return;
        }

        let chat_id = message.chat.id;

        // Handle voice messages: transcribe and treat as text.
        if let Some(ref voice) = message.voice {
            self.handle_voice_message(chat_id, &message, voice).await;
            return;
        }

        let Some(ref text) = message.text else {
            return;
        };

        // Topics mode: message in a forum topic.
        if let Some(thread_id) = message.message_thread_id
            && let Some(ref topics) = self.topics
            && let Some(session_id) = topics.session_for_thread(thread_id)
        {
            let session_id = session_id.to_owned();

            // Strip @botname suffix from slash commands. Telegram
            // appends @botname when the user picks a command from the
            // autocomplete menu in a group (e.g. "/new@my_bot" → "/new").
            let cleaned_text = if text.starts_with('/') {
                let first_word_end = text.find(' ').unwrap_or(text.len());
                let first_word = &text[..first_word_end];
                if let Some(at_pos) = first_word.find('@') {
                    format!("{}{}", &text[..at_pos], &text[first_word_end..])
                } else {
                    text.clone()
                }
            } else {
                text.clone()
            };

            let trimmed = cleaned_text.trim();

            // /cancel always takes effect immediately and clears any
            // pending interactive prompt.
            if trimmed.split_whitespace().next() == Some("/cancel") {
                self.pending_prompts.remove(&session_id);
                let _ = self
                    .incoming_tx
                    .send(IncomingMessage {
                        session_id,
                        text: String::new(),
                        mode: SendMode::Steer,
                        is_cancel: true,
                    })
                    .await;
                return;
            }

            // Handle /help locally.
            if trimmed == "/help" || trimmed == "/start" {
                self.pending_prompts.remove(&session_id);
                self.outbox.enqueue(OutboxOp::Send {
                    chat_id: topics.chat_id(),
                    text: format_topics_help(),
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: None,
                    message_thread_id: Some(thread_id),
                    result_tx: None,
                });
                return;
            }

            // Handle /verbose, /thinking, /tools locally (pup-level commands).
            #[allow(clippy::type_complexity)]
            let display_cmd: Option<(&str, fn(&mut TurnTracker, &str, bool))> =
                if trimmed == "/verbose" || trimmed.starts_with("/verbose ") {
                    Some(("verbose", TurnTracker::set_verbose))
                } else if trimmed == "/thinking" || trimmed.starts_with("/thinking ") {
                    Some(("thinking", TurnTracker::set_thinking))
                } else if trimmed == "/tools" || trimmed.starts_with("/tools ") {
                    Some(("tools", TurnTracker::set_tools))
                } else {
                    None
                };
            if let Some((cmd_key, setter)) = display_cmd {
                self.pending_prompts.remove(&session_id);
                let args = trimmed
                    .strip_prefix(&format!("/{cmd_key}"))
                    .expect("checked above")
                    .trim();
                if args.is_empty() {
                    self.pending_prompts.insert(
                        session_id.clone(),
                        PendingPrompt {
                            command: cmd_key.to_owned(),
                            local: true,
                        },
                    );
                    let prompt = match cmd_key {
                        "thinking" => {
                            "Show thinking/reasoning content while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                        }
                        "tools" => {
                            "Show tool call details while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                        }
                        _ => {
                            "Verbose mode shows thinking and tool calls while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                        }
                    };
                    self.outbox.enqueue(OutboxOp::Send {
                        chat_id: topics.chat_id(),
                        text: format!("<i>{prompt}</i>"),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: None,
                        message_thread_id: Some(thread_id),
                        result_tx: None,
                    });
                } else {
                    let on = matches!(args, "on" | "true" | "1" | "yes");
                    setter(&mut self.turn_tracker, &session_id, on);
                    let label = if on { "on" } else { "off" };
                    let name = match cmd_key {
                        "thinking" => "Thinking",
                        "tools" => "Tools",
                        _ => "Verbose",
                    };
                    self.outbox.enqueue(OutboxOp::Send {
                        chat_id: topics.chat_id(),
                        text: format!("{name}: <b>{label}</b>"),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: None,
                        message_thread_id: Some(thread_id),
                        result_tx: None,
                    });
                }
                return;
            }

            // Start typing immediately so the user sees activity
            // while we parse, transcribe, or wait for the agent.
            {
                let mut dests = vec![(topics.chat_id(), Some(thread_id))];
                if self.dm.attached.as_deref() == Some(&session_id)
                    && let Some(dm_cid) = self.dm_chat_id
                {
                    dests.push((dm_cid, None));
                }
                spawn_typing_loops(
                    &mut self.pre_turn_typing,
                    &self.bot,
                    &self.outbox.chat_budget(),
                    &session_id,
                    &dests,
                );
            }

            // If the user sent a non-command and there's a pending
            // prompt, treat their reply as the argument.
            if trimmed.starts_with('/') {
                // A new slash command cancels any pending prompt.
                self.pending_prompts.remove(&session_id);
            } else if let Some(prompt) = self.pending_prompts.remove(&session_id) {
                if prompt.local {
                    // Handle pup-level command locally.
                    self.pre_turn_typing.remove(&session_id);
                    let on = matches!(trimmed, "on" | "true" | "1" | "yes");
                    let (name, setter): (&str, fn(&mut TurnTracker, &str, bool)) =
                        match prompt.command.as_str() {
                            "thinking" => ("Thinking", TurnTracker::set_thinking),
                            "tools" => ("Tools", TurnTracker::set_tools),
                            _ => ("Verbose", TurnTracker::set_verbose),
                        };
                    setter(&mut self.turn_tracker, &session_id, on);
                    let label = if on { "on" } else { "off" };
                    self.outbox.enqueue(OutboxOp::Send {
                        chat_id: topics.chat_id(),
                        text: format!("{name}: <b>{label}</b>"),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: None,
                        message_thread_id: Some(thread_id),
                        result_tx: None,
                    });
                } else {
                    let full_cmd = format!("/{} {}", prompt.command, trimmed);
                    let _ = self
                        .incoming_tx
                        .send(IncomingMessage {
                            session_id,
                            text: full_cmd,
                            mode: SendMode::Steer,
                            is_cancel: false,
                        })
                        .await;
                }
                return;
            }

            // If this is a slash command that needs an argument but
            // was invoked without one, start an interactive prompt.
            if let Some(after_slash) = trimmed.strip_prefix('/') {
                let (cmd_name, args) = match after_slash.split_once(' ') {
                    Some((c, a)) => (c, a.trim()),
                    None => (after_slash, ""),
                };
                if args.is_empty()
                    && let Some(question) = prompt_for_command(cmd_name)
                {
                    self.pre_turn_typing.remove(&session_id);
                    self.pending_prompts.insert(
                        session_id.clone(),
                        PendingPrompt {
                            command: cmd_name.to_owned(),
                            local: false,
                        },
                    );
                    self.outbox.enqueue(OutboxOp::Send {
                        chat_id: topics.chat_id(),
                        text: format!("<i>{question}</i>"),
                        parse_mode: Some("HTML".to_owned()),
                        reply_markup: None,
                        message_thread_id: Some(thread_id),
                        result_tx: None,
                    });
                    return;
                }
            }

            // Determine send mode.
            let (msg_text, mode) = if let Some(stripped) = cleaned_text.strip_prefix(">>") {
                (stripped.trim().to_owned(), SendMode::FollowUp)
            } else {
                (cleaned_text, SendMode::Steer)
            };

            // Echo user message to the DM so both channels show
            // the full conversation.
            if self.dm.attached.as_deref() == Some(session_id.as_str())
                && let Some(dm_cid) = self.dm_chat_id
            {
                self.send_dm(dm_cid, &format_user_message(&msg_text));
            }

            let _ = self
                .incoming_tx
                .send(IncomingMessage {
                    session_id,
                    text: msg_text,
                    mode,
                    is_cancel: false,
                })
                .await;
            return;
        }

        // DM mode.
        if self.config.dm_enabled {
            self.dm_chat_id = Some(chat_id);
            self.handle_dm_message(chat_id, text).await;
        }
    }

    /// Handle a DM message (commands or forwarding).
    #[allow(clippy::too_many_lines)]
    async fn handle_dm_message(&mut self, chat_id: i64, text: &str) {
        // Any slash command cancels pending interactive prompts.
        if text.trim().starts_with('/') {
            self.pending_dm_prompt = None;
            if let Some(sid) = &self.dm.attached {
                self.pending_prompts.remove(sid.as_str());
            }
        }

        let cmd = dm::parse_command(text);

        match cmd {
            DmCommand::List => {
                self.dm.last_list.clone_from(&self.sessions);
                let msg = DmState::format_session_list(&self.sessions);
                self.outbox.enqueue(OutboxOp::Send {
                    chat_id,
                    text: msg,
                    parse_mode: Some("HTML".to_owned()),
                    reply_markup: None,
                    message_thread_id: None,
                    result_tx: None,
                });
            }
            DmCommand::Attach { reference } => {
                if reference.is_empty() {
                    self.send_dm(chat_id, "Usage: /attach &lt;name|index|id&gt;");
                    return;
                }
                match self.dm.resolve_session(&reference, &self.sessions) {
                    ResolveResult::Found(session) => {
                        let sid = session.session_id.clone();
                        let name = session
                            .session_name
                            .as_deref()
                            .unwrap_or_else(|| &sid[..8.min(sid.len())]);
                        self.dm.attached = Some(sid.clone());
                        self.send_dm(
                            chat_id,
                            &format!("Attached to <b>{}</b>", escape_html(name)),
                        );
                        info!(session_id = %sid, "DM attached");

                        // If the session has an active turn, add the DM
                        // as a destination so the user sees live updates
                        // immediately (not just from the next turn).
                        if self.turn_tracker.has_turn(&sid) {
                            let budget = self.outbox.chat_budget();
                            self.turn_tracker.add_destination(
                                &sid,
                                chat_id,
                                None,
                                &self.bot,
                                &budget,
                                &mut self.outbox,
                            );
                        }
                    }
                    ResolveResult::Ambiguous(matches) => {
                        let names: Vec<String> = matches
                            .iter()
                            .map(|s| {
                                s.session_name
                                    .as_deref()
                                    .unwrap_or(&s.session_id)
                                    .to_owned()
                            })
                            .collect();
                        self.send_dm(
                            chat_id,
                            &format!(
                                "Ambiguous — matches: {}",
                                names
                                    .iter()
                                    .map(|n| escape_html(n))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        );
                    }
                    ResolveResult::NotFound => {
                        self.send_dm(chat_id, "Session not found.");
                    }
                }
            }
            DmCommand::Detach => {
                if self.dm.attached.is_some() {
                    self.dm.attached = None;
                    self.send_dm(chat_id, "Detached.");
                    info!("DM detached");
                } else {
                    self.send_dm(chat_id, "Not attached to any session.");
                }
            }
            DmCommand::Cancel => {
                if let Some(ref sid) = self.dm.attached {
                    let _ = self
                        .incoming_tx
                        .send(IncomingMessage {
                            session_id: sid.clone(),
                            text: String::new(),
                            mode: SendMode::Steer,
                            is_cancel: true,
                        })
                        .await;
                    self.send_dm(chat_id, "Cancelling…");
                } else {
                    self.send_dm(chat_id, "Not attached to any session.");
                }
            }
            DmCommand::Verbose { toggle }
            | DmCommand::Thinking { toggle }
            | DmCommand::Tools { toggle } => {
                let (cmd_key, name, setter): (&str, &str, fn(&mut TurnTracker, &str, bool)) =
                    match cmd {
                        DmCommand::Thinking { .. } => {
                            ("thinking", "Thinking", TurnTracker::set_thinking)
                        }
                        DmCommand::Tools { .. } => ("tools", "Tools", TurnTracker::set_tools),
                        _ => ("verbose", "Verbose", TurnTracker::set_verbose),
                    };
                if let Some(ref sid) = self.dm.attached {
                    if let Some(on) = toggle {
                        setter(&mut self.turn_tracker, sid, on);
                        let label = if on { "on" } else { "off" };
                        self.send_dm(chat_id, &format!("{name}: <b>{label}</b>"));
                    } else {
                        self.pending_dm_prompt = Some(PendingPrompt {
                            command: cmd_key.to_owned(),
                            local: true,
                        });
                        let prompt = match cmd_key {
                            "thinking" => {
                                "Show thinking/reasoning content while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                            }
                            "tools" => {
                                "Show tool call details while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                            }
                            _ => {
                                "Verbose mode shows thinking and tool calls while the agent works.\n\nReply <b>on</b> or <b>off</b>."
                            }
                        };
                        self.send_dm(chat_id, &format!("<i>{prompt}</i>"));
                    }
                } else {
                    self.send_dm(chat_id, "Not attached to any session.");
                }
            }
            DmCommand::Help => {
                self.send_dm(chat_id, &DmState::format_help());
            }
            DmCommand::Message { text, mode } => {
                // Check DM-level pending prompt first (e.g. /verbose).
                if !text.starts_with('/')
                    && let Some(prompt) = self.pending_dm_prompt.take()
                {
                    if let Some(ref sid) = self.dm.attached {
                        let on = matches!(text.trim(), "on" | "true" | "1" | "yes");
                        let (name, setter): (&str, fn(&mut TurnTracker, &str, bool)) =
                            match prompt.command.as_str() {
                                "thinking" => ("Thinking", TurnTracker::set_thinking),
                                "tools" => ("Tools", TurnTracker::set_tools),
                                _ => ("Verbose", TurnTracker::set_verbose),
                            };
                        setter(&mut self.turn_tracker, sid, on);
                        let label = if on { "on" } else { "off" };
                        self.send_dm(chat_id, &format!("{name}: <b>{label}</b>"));
                    } else {
                        self.send_dm(chat_id, "Not attached to any session.");
                    }
                    return;
                }

                if let Some(sid) = self.dm.attached.clone() {
                    // Start typing immediately in all destinations.
                    {
                        let mut dests = vec![(chat_id, None)];
                        if let Some(ref topics) = self.topics
                            && let Some(thread_id) = topics.thread_for_session(&sid)
                        {
                            dests.push((topics.chat_id(), Some(thread_id)));
                        }
                        spawn_typing_loops(
                            &mut self.pre_turn_typing,
                            &self.bot,
                            &self.outbox.chat_budget(),
                            &sid,
                            &dests,
                        );
                    }

                    // Check for pending prompt completion.
                    if !text.starts_with('/')
                        && let Some(prompt) = self.pending_prompts.remove(&sid)
                    {
                        if prompt.local {
                            self.pre_turn_typing.remove(&sid);
                            let on = matches!(text.trim(), "on" | "true" | "1" | "yes");
                            let (name, setter): (&str, fn(&mut TurnTracker, &str, bool)) =
                                match prompt.command.as_str() {
                                    "thinking" => ("Thinking", TurnTracker::set_thinking),
                                    "tools" => ("Tools", TurnTracker::set_tools),
                                    _ => ("Verbose", TurnTracker::set_verbose),
                                };
                            setter(&mut self.turn_tracker, &sid, on);
                            let label = if on { "on" } else { "off" };
                            self.send_dm(chat_id, &format!("{name}: <b>{label}</b>"));
                        } else {
                            let full_cmd = format!("/{} {}", prompt.command, text);
                            let _ = self
                                .incoming_tx
                                .send(IncomingMessage {
                                    session_id: sid,
                                    text: full_cmd,
                                    mode: SendMode::Steer,
                                    is_cancel: false,
                                })
                                .await;
                        }
                        return;
                    }

                    // Check if this is a pi slash command that needs an argument.
                    let msg_trimmed = text.trim();
                    if let Some(after_slash) = msg_trimmed.strip_prefix('/') {
                        let (cmd_name, args) = match after_slash.split_once(' ') {
                            Some((c, a)) => (c.split('@').next().unwrap_or(c), a.trim()),
                            None => (after_slash.split('@').next().unwrap_or(after_slash), ""),
                        };
                        if args.is_empty()
                            && let Some(question) = prompt_for_command(cmd_name)
                        {
                            self.pre_turn_typing.remove(&sid);
                            self.pending_prompts.insert(
                                sid,
                                PendingPrompt {
                                    command: cmd_name.to_owned(),
                                    local: false,
                                },
                            );
                            self.send_dm(chat_id, &format!("<i>{question}</i>"));
                            return;
                        }
                    }

                    // Echo user message to the topic so both channels
                    // show the full conversation.
                    self.send_to_topic(&sid, &format_user_message(&text));

                    let _ = self
                        .incoming_tx
                        .send(IncomingMessage {
                            session_id: sid,
                            text,
                            mode,
                            is_cancel: false,
                        })
                        .await;
                } else {
                    self.send_dm(chat_id, "Not attached. Use /ls and /attach first.");
                }
            }
        }
    }

    /// Quick helper to enqueue a DM text message.
    /// Ensure the turn tracker has state for this session.
    /// Auto-creates turn state when the daemon connected mid-turn
    /// (missed AgentStart) or when a steer message continues an
    /// existing turn without a new AgentStart.
    fn ensure_turn(&mut self, session_id: &str) {
        if self.turn_tracker.has_turn(session_id) {
            return;
        }

        let mut destinations = Vec::new();
        if let Some(ref topics) = self.topics
            && let Some(thread_id) = topics.thread_for_session(session_id)
        {
            destinations.push((topics.chat_id(), Some(thread_id)));
        }
        if self.dm.attached.as_deref() == Some(session_id)
            && let Some(chat_id) = self.dm_chat_id
        {
            destinations.push((chat_id, None));
        }

        if destinations.is_empty() {
            return;
        }

        debug!(
            session_id,
            dests = destinations.len(),
            "auto-creating turn state (missed AgentStart)"
        );
        self.pre_turn_typing.remove(session_id);
        let budget = self.outbox.chat_budget();
        self.turn_tracker
            .start_turn(session_id, &destinations, &self.bot, &budget);
    }

    fn send_dm(&mut self, chat_id: i64, text: &str) {
        self.outbox.enqueue(OutboxOp::Send {
            chat_id,
            text: text.to_owned(),
            parse_mode: Some("HTML".to_owned()),
            reply_markup: None,
            message_thread_id: None,
            result_tx: None,
        });
    }

    /// Send a message to a session's topic (if topics mode).
    fn send_to_topic(&mut self, session_id: &str, text: &str) {
        if let Some(ref topics) = self.topics
            && let Some(thread_id) = topics.thread_for_session(session_id)
        {
            self.outbox.enqueue(OutboxOp::Send {
                chat_id: topics.chat_id(),
                text: text.to_owned(),
                parse_mode: Some("HTML".to_owned()),
                reply_markup: None,
                message_thread_id: Some(thread_id),
                result_tx: None,
            });
        }
    }

    /// Handle a voice message: download, transcribe, and forward as text.
    #[allow(clippy::too_many_lines)]
    async fn handle_voice_message(
        &mut self,
        chat_id: i64,
        message: &bot::Message,
        voice: &bot::Voice,
    ) {
        // Figure out where this message should go, and a helper to
        // reply in the right place (topic or DM).
        let (session_id, thread_id) = if let Some(tid) = message.message_thread_id
            && let Some(ref topics) = self.topics
            && let Some(sid) = topics.session_for_thread(tid)
        {
            (sid.to_owned(), Some(tid))
        } else if let Some(ref sid) = self.dm.attached {
            (sid.clone(), None)
        } else {
            self.send_dm(chat_id, "Not attached. Use /ls and /attach first.");
            return;
        };

        // Start typing immediately — covers download + transcription time.
        {
            let mut dests = vec![(chat_id, thread_id)];
            if let Some(ref topics) = self.topics
                && let Some(tid) = topics.thread_for_session(&session_id)
                && thread_id != Some(tid)
            {
                dests.push((topics.chat_id(), Some(tid)));
            }
            if self.dm.attached.as_deref() == Some(&session_id)
                && let Some(dm_cid) = self.dm_chat_id
                && thread_id.is_some()
            {
                dests.push((dm_cid, None));
            }
            spawn_typing_loops(
                &mut self.pre_turn_typing,
                &self.bot,
                &self.outbox.chat_budget(),
                &session_id,
                &dests,
            );
        }

        // Macro-like helper: reply to the right place.
        macro_rules! reply {
            ($text:expr) => {
                if thread_id.is_some() {
                    self.send_to_topic(&session_id, $text);
                } else {
                    self.send_dm(chat_id, $text);
                }
            };
        }

        if self.transcriber.is_none() {
            self.pre_turn_typing.remove(&session_id);
            reply!(
                "⚠️ Voice messages are not supported. \
                    Set <code>voice = true</code> under <code>[backends.telegram]</code> in your pup config to enable transcription."
            );
            return;
        }

        debug!(
            file_id = %voice.file_id,
            duration = voice.duration,
            "transcribing voice message"
        );

        // Download the voice file.
        let ogg_data = match self.download_voice(&voice.file_id).await {
            Ok(data) => data,
            Err(e) => {
                warn!(error = %e, "failed to download voice message");
                self.pre_turn_typing.remove(&session_id);
                reply!(&format!(
                    "⚠️ Failed to download voice message: <code>{}</code>",
                    escape_html(&e.to_string()),
                ));
                return;
            }
        };

        // Convert OGG/Opus → 16 kHz PCM and transcribe.
        let text = match self.transcribe_audio(ogg_data).await {
            Ok(t) if t.is_empty() => {
                self.pre_turn_typing.remove(&session_id);
                reply!("⚠️ Could not recognise any speech in this voice message.");
                return;
            }
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "voice transcription failed");
                self.pre_turn_typing.remove(&session_id);
                reply!(&format!(
                    "⚠️ Voice transcription failed: <code>{}</code>",
                    escape_html(&e.to_string()),
                ));
                return;
            }
        };

        info!(session_id, chars = text.len(), "voice message transcribed");

        // Show the transcribed text in all destinations.
        let preview = format!("🎙️ <i>{}</i>", escape_html(&text));
        self.send_to_topic(&session_id, &preview);
        if self.dm.attached.as_deref() == Some(session_id.as_str())
            && let Some(dm_cid) = self.dm_chat_id
        {
            self.send_dm(dm_cid, &preview);
        }

        // Forward the transcribed text to the session.
        let _ = self
            .incoming_tx
            .send(IncomingMessage {
                session_id,
                text,
                mode: SendMode::Steer,
                is_cancel: false,
            })
            .await;
    }

    /// Download a Telegram voice file by file_id.
    async fn download_voice(&self, file_id: &str) -> Result<Vec<u8>> {
        let file_info = self.bot.get_file(file_id).await?;
        let file_path = file_info
            .file_path
            .context("Telegram returned no file_path")?;
        self.bot.download_file(&file_path).await
    }

    /// Decode OGG/Opus audio and run whisper transcription.
    async fn transcribe_audio(&self, ogg_data: Vec<u8>) -> Result<String> {
        let transcriber = Arc::clone(self.transcriber.as_ref().context("no transcriber loaded")?);
        // Both decoding and inference are CPU-bound.
        tokio::task::spawn_blocking(move || {
            let pcm = whisper::decode_ogg_opus(&ogg_data)?;
            let t = transcriber
                .lock()
                .map_err(|e| anyhow::anyhow!("transcriber lock poisoned: {e}"))?;
            t.transcribe(&pcm)
        })
        .await
        .context("transcription task panicked")?
    }

    /// Drain `getUpdates` from `offset`, recording any `forum_topic_created`
    /// service messages in the supergroup whose name matches our icon.
    ///
    /// Returns the new offset to use for normal polling (one past the last
    /// update we consumed). Does NOT process messages — they're stale.
    async fn scan_updates_for_topics(
        bot: &BotClient,
        mut offset: i64,
        supergroup_id: i64,
        topic_icon: &str,
        topics: &mut TopicsManager,
    ) -> i64 {
        let mut total_updates = 0u64;
        let mut discovered = 0u64;

        loop {
            // timeout=0 → return immediately with whatever's available.
            let updates = match bot.get_updates(offset, 0).await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "failed to scan updates for topics");
                    break;
                }
            };

            if updates.is_empty() {
                break;
            }

            for update in &updates {
                if update.update_id >= offset {
                    offset = update.update_id + 1;
                }
                total_updates += 1;

                // Only look at messages in our supergroup.
                let Some(ref msg) = update.message else {
                    continue;
                };
                if msg.chat.id != supergroup_id {
                    continue;
                }

                // Check for forum_topic_created service messages with our icon prefix.
                // When no icon is configured, skip discovery — rely on persisted state.
                if !topic_icon.is_empty()
                    && let Some(ref ftc) = msg.forum_topic_created
                    && ftc.name.starts_with(topic_icon)
                    && let Some(thread_id) = msg.message_thread_id
                {
                    debug!(
                        thread_id,
                        topic_name = %ftc.name,
                        "discovered topic from update scan"
                    );
                    topics.record_discovered_thread(thread_id);
                    discovered += 1;
                }
            }
        }

        topics.set_scan_checkpoint(offset);

        info!(
            total_updates,
            discovered,
            new_offset = offset,
            "topic scan complete"
        );

        offset
    }
}

/// Enumerate session IDs from `.sock` files in the socket directory.
async fn scan_live_sessions(socket_dir: &Path) -> HashSet<String> {
    let mut live = HashSet::new();
    let Ok(mut dir) = tokio::fs::read_dir(socket_dir).await else {
        return live;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sock")
            && let Some(session_id) = path.file_stem().and_then(|s| s.to_str())
        {
            live.insert(session_id.to_owned());
        }
    }
    live
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl ChatBackend for TelegramBackend {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn init(&mut self) -> Result<()> {
        let span = info_span!("telegram_init");
        async {
            // Get bot info.
            let me = self.bot.get_me().await.context("failed to get bot info")?;
            self.bot_user_id = me.id;
            info!(
                bot_id = me.id,
                bot_username = ?me.username,
                "telegram bot connected"
            );

            // Register commands for DMs (pup-specific commands).
            let dm_commands = vec![
                ("ls".to_owned(), "List active pi sessions".to_owned()),
                ("attach".to_owned(), "Attach to a session".to_owned()),
                ("detach".to_owned(), "Detach from session".to_owned()),
                ("cancel".to_owned(), "Cancel current operation".to_owned()),
                ("status".to_owned(), "Show session status (model, context usage)".to_owned()),
                ("verbose".to_owned(), "Toggle verbose mode (thinking + tools)".to_owned()),
                ("thinking".to_owned(), "Toggle thinking/reasoning display".to_owned()),
                ("tools".to_owned(), "Toggle tool call display".to_owned()),
                ("help".to_owned(), "Show help".to_owned()),
            ];
            let _ = self
                .bot
                .set_my_commands_scoped(
                    &dm_commands,
                    &serde_json::json!({"type": "all_private_chats"}),
                )
                .await;

            // Register commands for group topics (pi slash commands).
            // These are forwarded to pi via IPC and executed by the extension.
            let group_commands = vec![
                ("cancel".to_owned(), "Cancel current operation".to_owned()),
                ("status".to_owned(), "Show session status (model, context usage)".to_owned()),
                ("new".to_owned(), "Start a new session".to_owned()),
                ("compact".to_owned(), "Compact session context".to_owned()),
                ("name".to_owned(), "Set session name".to_owned()),
                ("verbose".to_owned(), "Toggle verbose mode (thinking + tools)".to_owned()),
                ("thinking".to_owned(), "Toggle thinking/reasoning display".to_owned()),
                ("tools".to_owned(), "Toggle tool call display".to_owned()),
                ("quit".to_owned(), "Quit pi session".to_owned()),
                ("help".to_owned(), "Show available commands".to_owned()),
            ];
            let _ = self
                .bot
                .set_my_commands_scoped(
                    &group_commands,
                    &serde_json::json!({"type": "all_group_chats"}),
                )
                .await;

            // Default commands (fallback).
            let default_commands = vec![
                ("cancel".to_owned(), "Cancel current operation".to_owned()),
                ("help".to_owned(), "Show help".to_owned()),
            ];
            let _ = self.bot.set_my_commands(&default_commands).await;

            // Validate topics setup if enabled, scan for orphaned topics,
            // and clean up stale ones.
            if let Some(ref mut topics) = self.topics {
                TopicsManager::validate(&self.bot, topics.chat_id(), self.bot_user_id)
                    .await
                    .context("topics validation failed")?;

                // Drain pending getUpdates from our checkpoint to discover
                // topic creations we might have missed (crashes, pre-persistence
                // era, etc.). Only records threads whose name matches our icon.
                let scan_offset = topics.scan_checkpoint();
                let supergroup_id = topics.chat_id();
                let icon = topics.topic_icon().to_owned();
                let new_offset = Self::scan_updates_for_topics(
                    &self.bot,
                    scan_offset,
                    supergroup_id,
                    &icon,
                    topics,
                )
                .await;

                // Start normal polling from wherever the scan left off.
                if new_offset > self.update_offset {
                    self.update_offset = new_offset;
                }

                // Determine which sessions are currently alive.
                let live_sessions = scan_live_sessions(&self.config.socket_dir).await;
                info!(live_count = live_sessions.len(), "scanned for live sessions");

                // Delete every known topic that doesn't match a live session.
                topics
                    .cleanup_stale_topics(&self.bot, &live_sessions)
                    .await;
            }

            // Load whisper model for voice transcription if enabled.
            if self.config.voice {
                let cache_dir = self.config.socket_dir.join("whisper");
                match whisper::Transcriber::new(None, None, &cache_dir).await {
                    Ok(t) => {
                        info!("whisper transcriber loaded");
                        self.transcriber = Some(Arc::new(Mutex::new(t)));
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to load whisper — voice messages will be ignored");
                    }
                }
            }

            info!(
                dm_enabled = self.config.dm_enabled,
                topics_enabled = self.config.topics_enabled,
                voice = self.transcriber.is_some(),
                "telegram backend initialized"
            );
            Ok(())
        }
        .instrument(span)
        .await
    }

    async fn handle_event(&mut self, event: SessionEvent) -> Result<()> {
        // Check for expired pending topic deletions on every event.
        self.check_pending_deletions().await;

        match event {
            SessionEvent::Connected { ref info } => {
                self.sessions.push(info.clone());

                // Topics mode: create (or reuse) a topic and post recent history.
                if let Some(ref mut topics) = self.topics {
                    // Check if a recently-disconnected session in the same cwd
                    // has a topic we can reuse (handles pi restarts).
                    if let Some((thread_id, remembered_name)) =
                        topics.claim_pending_topic(&info.session_id, &info.cwd)
                    {
                        // Restore the session name if the new session doesn't
                        // have one. Try the grace-period name first, then the
                        // persistent cwd→name cache.
                        if info.session_name.is_none() {
                            let name_to_restore = remembered_name.or_else(|| {
                                topics.last_name_for_cwd(&info.cwd).map(ToOwned::to_owned)
                            });
                            if let Some(name) = name_to_restore {
                                info!(
                                    session_id = %info.session_id,
                                    name = %name,
                                    thread_id,
                                    "restoring session name from previous session"
                                );
                                let _ = self
                                    .incoming_tx
                                    .send(IncomingMessage {
                                        session_id: info.session_id.clone(),
                                        text: format!("/name {name}"),
                                        mode: SendMode::Steer,
                                        is_cancel: false,
                                    })
                                    .await;
                            }
                        }

                        // Rename the topic to reflect the new session's info.
                        if let Err(e) = topics.rename_topic(&self.bot, info).await {
                            warn!(error = %e, "failed to rename reclaimed topic");
                        }
                    } else {
                        // No pending topic to reclaim — check persistent name
                        // cache and restore the name before creating the topic.
                        if info.session_name.is_none()
                            && let Some(name) =
                                topics.last_name_for_cwd(&info.cwd).map(ToOwned::to_owned)
                        {
                            info!(
                                session_id = %info.session_id,
                                name = %name,
                                "restoring session name from cwd cache"
                            );
                            let _ = self
                                .incoming_tx
                                .send(IncomingMessage {
                                    session_id: info.session_id.clone(),
                                    text: format!("/name {name}"),
                                    mode: SendMode::Steer,
                                    is_cancel: false,
                                })
                                .await;
                        }

                        match topics.create_topic(&self.bot, info).await {
                            Ok((thread_id, reused)) => {
                                // Only post history for newly created topics.
                                // Reused topics already have their history from the
                                // previous daemon run.
                                if !reused && !info.history.is_empty() {
                                    let msgs =
                                        format_history(&info.history, self.config.history_turns);
                                    for msg in msgs {
                                        self.outbox.enqueue(OutboxOp::Send {
                                            chat_id: topics.chat_id(),
                                            text: msg,
                                            parse_mode: Some("HTML".to_owned()),
                                            reply_markup: None,
                                            message_thread_id: Some(thread_id),
                                            result_tx: None,
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to create topic");
                            }
                        }
                    }
                }
            }
            SessionEvent::Disconnected { ref session_id } => {
                self.pre_turn_typing.remove(session_id);

                // Grab cwd and name before removing from our session list.
                let (cwd, name) = self
                    .sessions
                    .iter()
                    .find(|s| s.session_id == *session_id)
                    .map(|s| (s.cwd.clone(), s.session_name.clone()))
                    .unwrap_or_default();

                self.sessions.retain(|s| s.session_id != *session_id);
                self.pending_prompts.remove(session_id.as_str());

                // Topics mode: schedule deletion with grace period so a
                // restarted pi session in the same cwd can reclaim the topic.
                if let Some(ref mut topics) = self.topics {
                    if !cwd.is_empty() {
                        topics.schedule_deletion(session_id, &cwd, name.as_deref());
                    } else if let Err(e) = topics.delete_topic(&self.bot, session_id).await {
                        warn!(error = %e, "failed to delete topic");
                    }
                }

                // DM mode: auto-detach.
                if self.dm.attached.as_deref() == Some(session_id.as_str()) {
                    self.dm.attached = None;
                    if let Some(chat_id) = self.dm_chat_id {
                        self.send_dm(chat_id, "📴 Session ended. Detached.");
                    }
                }
            }
            SessionEvent::InfoChanged { ref info } => {
                // The session responded with a metadata change (e.g. /name,
                // /model) without starting an agent turn.  Stop any pre-turn
                // typing indicator that was spawned when the message arrived.
                self.pre_turn_typing.remove(&info.session_id);

                // Update local session info.
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == info.session_id)
                {
                    *existing = info.clone();
                }

                // Topics mode: rename the topic and persist the name.
                if let Some(ref mut topics) = self.topics {
                    if let Some(ref name) = info.session_name {
                        topics.remember_name(&info.cwd, name);
                    }
                    if let Err(e) = topics.rename_topic(&self.bot, info).await {
                        warn!(error = %e, "failed to rename topic");
                    }
                }
            }
            SessionEvent::SessionReset { ref session_id } => {
                info!(session_id, "session reset");
                self.pre_turn_typing.remove(session_id);
                self.pending_prompts.remove(session_id.as_str());
                // End any in-progress turn cleanly.
                self.outbox.clear_edit_cooldown();
                self.turn_tracker.end_turn(session_id, &mut self.outbox);
                // Post a notification in the topic.
                self.send_to_topic(session_id, "🔄 <i>Session reset</i>");

                if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id
                {
                    self.send_dm(chat_id, "🔄 <i>Session reset</i>");
                }
            }
            SessionEvent::AgentStart { ref session_id } => {
                debug!(session_id, "agent started");

                // Drop the pre-turn typing loop — the turn tracker will
                // spawn fresh typing loops for all destinations.
                self.pre_turn_typing.remove(session_id);

                // Collect all destinations for this session (topic + DM).
                let mut destinations = Vec::new();
                if let Some(ref topics) = self.topics
                    && let Some(thread_id) = topics.thread_for_session(session_id)
                {
                    destinations.push((topics.chat_id(), Some(thread_id)));
                }
                if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id
                {
                    destinations.push((chat_id, None));
                }

                // Start a new turn — the tracker will send the first
                // Telegram message lazily on the first tool/delta event.
                if !destinations.is_empty() {
                    let budget = self.outbox.chat_budget();
                    self.turn_tracker
                        .start_turn(session_id, &destinations, &self.bot, &budget);
                }
            }
            SessionEvent::AgentEnd { ref session_id } => {
                debug!(session_id, "agent ended");
                self.pre_turn_typing.remove(session_id);
                self.ensure_turn(session_id);
                // Clear edit cooldowns so the final edit (removing the cancel
                // keyboard) goes through even if a recent content edit just ran.
                self.outbox.clear_edit_cooldown();
                self.turn_tracker.end_turn(session_id, &mut self.outbox);
                // Flush any remaining outbox operations.
                while self.outbox.flush_one().await {}
            }
            SessionEvent::MessageStart { .. } => {
                // Nothing to do — the turn tracker already has the message.
            }
            SessionEvent::ThinkingDelta {
                ref session_id,
                ref text,
                ..
            } => {
                debug!(session_id, len = text.len(), "thinking_delta");
                self.ensure_turn(session_id);
                self.turn_tracker
                    .thinking_delta(session_id, text, &mut self.outbox);
            }
            SessionEvent::MessageDelta {
                ref session_id,
                ref text,
                ..
            } => {
                debug!(session_id, len = text.len(), "message_delta");
                self.ensure_turn(session_id);
                self.turn_tracker
                    .message_delta(session_id, text, &mut self.outbox);
            }
            SessionEvent::MessageEnd {
                ref session_id,
                ref content,
                ..
            } => {
                // Update the turn with the final content and flush an
                // edit. Does NOT finalize the turn — AgentEnd does that.
                self.ensure_turn(session_id);
                self.turn_tracker
                    .message_end_with_content(session_id, content, &mut self.outbox);
            }
            SessionEvent::ToolStart {
                ref session_id,
                ref tool_name,
                ref args,
                ..
            } => {
                self.ensure_turn(session_id);
                self.turn_tracker
                    .tool_start(session_id, tool_name, args, &mut self.outbox);
            }
            SessionEvent::ToolUpdate {
                ref session_id,
                ref tool_name,
                ref content,
                ..
            } => {
                self.ensure_turn(session_id);
                self.turn_tracker
                    .tool_update(session_id, tool_name, content, &mut self.outbox);
            }
            SessionEvent::ToolEnd {
                ref session_id,
                ref tool_name,
                ref content,
                is_error,
                ..
            } => {
                self.ensure_turn(session_id);
                self.turn_tracker.tool_end(
                    session_id,
                    tool_name,
                    content,
                    is_error,
                    &mut self.outbox,
                );
            }
            SessionEvent::Notification {
                ref session_id,
                ref text,
            } => {
                // The session responded with a notification (e.g. /status,
                // unsupported command) without starting an agent turn.  Stop
                // any pre-turn typing indicator.
                self.pre_turn_typing.remove(session_id);

                let html = format!("<i>{}</i>", escape_html(text));
                self.send_to_topic(session_id, &html);

                if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id
                {
                    self.send_dm(chat_id, &html);
                }
            }
            SessionEvent::UserMessage {
                ref session_id,
                ref content,
                echo,
                source,
            } => {
                // Skip echoes (messages we sent).
                if echo {
                    return Ok(());
                }

                // Show user messages from the TUI.
                if source == MessageSource::Interactive {
                    let msg = format_user_message(content);
                    self.send_to_topic(session_id, &msg);

                    if self.dm.attached.as_deref() == Some(session_id.as_str())
                        && let Some(chat_id) = self.dm_chat_id
                    {
                        self.send_dm(chat_id, &msg);
                    }
                }
            }
        }

        // Flush outbox after each event batch.
        while self.outbox.flush_one().await {}

        Ok(())
    }

    async fn recv_incoming(&mut self) -> Result<Option<IncomingMessage>> {
        loop {
            // Check for expired pending topic deletions (~once per second).
            self.check_pending_deletions().await;

            // Short poll (1s) so the outer select! loop can preempt us
            // between iterations to process session events (agent
            // responses, typing indicators, etc.).
            match self.bot.get_updates(self.update_offset, 1).await {
                Ok(updates) => {
                    for update in updates {
                        if update.update_id >= self.update_offset {
                            self.update_offset = update.update_id + 1;
                        }
                        self.handle_update(update).await;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to poll Telegram updates");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }

            // Flush any outbox messages enqueued by handle_update
            // (e.g. interactive prompts for commands like /name).
            while self.outbox.flush_one().await {}

            // Drain any incoming messages generated by handle_update.
            if let Some(ref mut rx) = self.incoming_rx {
                match rx.try_recv() {
                    Ok(msg) => return Ok(Some(msg)),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        // No messages — loop back for another short poll.
                        // The select! in the main loop can preempt us at
                        // the next .await (the getUpdates call).
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
                }
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("shutting down telegram backend");

        // Cancel any pending topic deletions so their mappings are
        // preserved across pup restarts.  If the sessions are still
        // alive when pup comes back, their topics will be reused.
        if let Some(ref mut topics) = self.topics {
            topics.cancel_all_pending();
        }

        // Best-effort flush of outbox.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.outbox.is_empty() && tokio::time::Instant::now() < deadline {
            if !self.outbox.flush_one().await {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::clone_on_ref_ptr,
    clippy::too_many_lines,
    clippy::significant_drop_tightening
)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    /// A recorded API call from the mock server.
    #[derive(Debug, Clone)]
    struct ApiCall {
        method: String,
        body: String,
    }

    /// Start a mock Telegram Bot API server.
    ///
    /// Returns the base URL and a handle to the recorded API calls.
    /// `updates` are returned on the first `getUpdates`; subsequent calls
    /// block until the server is dropped (simulating long-poll).
    async fn mock_telegram_api(
        updates: Vec<serde_json::Value>,
    ) -> (String, Arc<StdMutex<Vec<ApiCall>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}/bottest");
        let calls: Arc<StdMutex<Vec<ApiCall>>> = Arc::new(StdMutex::new(Vec::new()));

        let calls_bg = calls.clone();
        let updates = Arc::new(StdMutex::new(Some(updates)));

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let calls = calls_bg.clone();
                let updates = updates.clone();

                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);

                    // Handle potentially multiple requests per connection.
                    loop {
                        // Read request line.
                        let mut request_line = String::new();
                        match reader.read_line(&mut request_line).await {
                            Ok(0) | Err(_) => break,
                            _ => {}
                        }

                        // Read headers.
                        let mut content_length: usize = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.is_err() {
                                return;
                            }
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some(val) = line
                                .strip_prefix("content-length:")
                                .or_else(|| line.strip_prefix("Content-Length:"))
                            {
                                content_length = val.trim().parse().unwrap_or(0);
                            }
                        }

                        // Read body.
                        let mut body = vec![0u8; content_length];
                        if reader.read_exact(&mut body).await.is_err() {
                            break;
                        }
                        let body_str = String::from_utf8_lossy(&body).to_string();

                        // Extract API method from path.
                        let method = request_line
                            .split(' ')
                            .nth(1)
                            .unwrap_or("")
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_owned();

                        calls.lock().unwrap().push(ApiCall {
                            method: method.clone(),
                            body: body_str,
                        });

                        let response_json = match method.as_str() {
                            "getUpdates" => {
                                let batch = updates.lock().unwrap().take();
                                if let Some(batch) = batch {
                                    // First call: return the canned updates.
                                    serde_json::json!({
                                        "ok": true,
                                        "result": batch
                                    })
                                } else {
                                    // Subsequent calls: hang (long-poll).
                                    tokio::time::sleep(Duration::from_secs(120)).await;
                                    return;
                                }
                            }
                            "sendMessage" => serde_json::json!({
                                "ok": true,
                                "result": {
                                    "message_id": 999,
                                    "chat": {"id": 1, "type": "private"}
                                }
                            }),
                            _ => serde_json::json!({"ok": true, "result": true}),
                        };

                        let payload = serde_json::to_string(&response_json).unwrap();
                        let http = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             \r\n\
                             {}",
                            payload.len(),
                            payload,
                        );
                        if writer.write_all(http.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        (base_url, calls)
    }

    fn test_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: "test".to_owned(),
            allowed_user_ids: vec![12345],
            dm_enabled: true,
            topics_enabled: false,
            supergroup_id: None,
            topic_icon: String::new(),
            max_message_length: 4096,
            edit_interval_ms: 100,
            thinking: false,
            tools: false,
            history_turns: 0,
            socket_dir: PathBuf::from("/tmp"),
            topics_state_path: PathBuf::from("/tmp/topics.json"),
            voice: false,
            tool_call_limit: turn_tracker::ToolCallLimit::default(),
            tool_output_lines: turn_tracker::ToolOutputLines::default(),
        }
    }

    /// Start a mock Telegram Bot API server where `answerCallbackQuery`
    /// takes a configurable delay before responding.
    ///
    /// All other methods behave identically to [`mock_telegram_api`].
    async fn mock_telegram_api_slow_callback(
        updates: Vec<serde_json::Value>,
        callback_delay: Duration,
    ) -> (String, Arc<StdMutex<Vec<ApiCall>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}/bottest");
        let calls: Arc<StdMutex<Vec<ApiCall>>> = Arc::new(StdMutex::new(Vec::new()));

        let calls_bg = calls.clone();
        let updates = Arc::new(StdMutex::new(Some(updates)));

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let calls = calls_bg.clone();
                let updates = updates.clone();
                let callback_delay = callback_delay;

                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);

                    loop {
                        let mut request_line = String::new();
                        match reader.read_line(&mut request_line).await {
                            Ok(0) | Err(_) => break,
                            _ => {}
                        }

                        let mut content_length: usize = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.is_err() {
                                return;
                            }
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some(val) = line
                                .strip_prefix("content-length:")
                                .or_else(|| line.strip_prefix("Content-Length:"))
                            {
                                content_length = val.trim().parse().unwrap_or(0);
                            }
                        }

                        let mut body = vec![0u8; content_length];
                        if reader.read_exact(&mut body).await.is_err() {
                            break;
                        }
                        let body_str = String::from_utf8_lossy(&body).to_string();

                        let method = request_line
                            .split(' ')
                            .nth(1)
                            .unwrap_or("")
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_owned();

                        calls.lock().unwrap().push(ApiCall {
                            method: method.clone(),
                            body: body_str,
                        });

                        let response_json = match method.as_str() {
                            "getUpdates" => {
                                let batch = updates.lock().unwrap().take();
                                if let Some(batch) = batch {
                                    serde_json::json!({
                                        "ok": true,
                                        "result": batch
                                    })
                                } else {
                                    tokio::time::sleep(Duration::from_secs(120)).await;
                                    return;
                                }
                            }
                            "answerCallbackQuery" => {
                                // Simulate real-world Telegram API latency.
                                tokio::time::sleep(callback_delay).await;
                                serde_json::json!({"ok": true, "result": true})
                            }
                            "sendMessage" => serde_json::json!({
                                "ok": true,
                                "result": {
                                    "message_id": 999,
                                    "chat": {"id": 1, "type": "private"}
                                }
                            }),
                            _ => serde_json::json!({"ok": true, "result": true}),
                        };

                        let payload = serde_json::to_string(&response_json).unwrap();
                        let http = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             \r\n\
                             {}",
                            payload.len(),
                            payload,
                        );
                        if writer.write_all(http.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        (base_url, calls)
    }

    /// The cancel button (callback query) must dispatch the abort
    /// `IncomingMessage` immediately — without waiting for the
    /// `answerCallbackQuery` API call to complete.
    ///
    /// This test uses a mock server that adds a 2-second delay to
    /// `answerCallbackQuery`. The abort must arrive on the incoming
    /// channel well before that delay elapses.
    #[tokio::test]
    async fn test_cancel_button_dispatches_abort_before_answering_callback() {
        let session_id = "test-session-cancel";

        let updates = vec![serde_json::json!({
            "update_id": 1,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 12345, "first_name": "Test"},
                "data": format!("cancel:{session_id}")
            }
        })];

        let (base_url, api_calls) =
            mock_telegram_api_slow_callback(updates, Duration::from_secs(2)).await;

        let bot = BotClient::with_base_url(&base_url);
        let mut backend = TelegramBackend::with_bot(test_config(), bot);

        // recv_incoming internally calls handle_update which sends the
        // abort to incoming_tx.  It then tries to drain incoming_rx.
        // With the fix, the abort should be ready instantly even though
        // answerCallbackQuery takes 2 seconds.
        let start = tokio::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_millis(500), backend.recv_incoming()).await;
        let elapsed = start.elapsed();

        // The 500ms timeout must NOT fire — the abort should arrive
        // almost immediately (well under the 2s callback delay).
        let msg = result
            .expect("abort must arrive within 500ms (answerCallbackQuery is 2s)")
            .expect("recv_incoming should not error")
            .expect("should return an IncomingMessage, not None");

        assert!(msg.is_cancel, "expected is_cancel=true");
        assert_eq!(msg.session_id, session_id);
        assert!(
            elapsed < Duration::from_millis(500),
            "abort took {elapsed:?} — should be near-instant, not blocked by answerCallbackQuery"
        );

        // Give the spawned answerCallbackQuery task a moment to complete
        // in the background.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        let calls = api_calls.lock().unwrap();
        let cb_calls: Vec<&ApiCall> = calls
            .iter()
            .filter(|c| c.method == "answerCallbackQuery")
            .collect();
        assert!(
            !cb_calls.is_empty(),
            "answerCallbackQuery should still be called (in the background)"
        );
    }

    /// An interactive command like `/name` (which requires an argument)
    /// must send its prompt to the user immediately — not wait until the
    /// next incoming message.  This is an end-to-end test: the mock
    /// Telegram API returns a single `/name` update, and we assert that
    /// `sendMessage` with the prompt text is called before the next
    /// (blocking) `getUpdates`.
    #[tokio::test]
    async fn test_interactive_prompt_sent_immediately() {
        let updates = vec![serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 100,
                "from": {"id": 12345, "first_name": "Test"},
                "chat": {"id": 99999, "type": "private"},
                "text": "/name"
            }
        })];

        let (base_url, api_calls) = mock_telegram_api(updates).await;

        let bot = BotClient::with_base_url(&base_url);
        let mut backend = TelegramBackend::with_bot(test_config(), bot);
        backend.dm.attached = Some("test-session".to_owned());
        backend.dm_chat_id = Some(99999);

        // recv_incoming loops forever (second getUpdates hangs), so
        // use a timeout.  The prompt must be sent within this window.
        let _ = tokio::time::timeout(Duration::from_secs(5), backend.recv_incoming()).await;

        let calls = api_calls.lock().unwrap();
        let send_calls: Vec<&ApiCall> =
            calls.iter().filter(|c| c.method == "sendMessage").collect();

        assert!(
            !send_calls.is_empty(),
            "sendMessage must be called with the interactive prompt \
             (got only: {calls:?})"
        );
        assert!(
            send_calls[0].body.contains("What name"),
            "prompt text missing from sendMessage body: {}",
            send_calls[0].body,
        );
    }
}
