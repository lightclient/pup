pub mod bot;
pub mod dm;
pub mod outbox;
pub mod render;
pub mod topics;
pub mod turn_tracker;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use pup_core::types::{IncomingMessage, MessageSource, SessionEvent, SessionInfo};
use pup_core::ChatBackend;
use pup_ipc::SendMode;
use tokio::sync::mpsc;
use tracing::{debug, info, info_span, warn, Instrument};

use bot::{BotClient, Update};
use dm::{DmCommand, DmState, ResolveResult};
use outbox::{Outbox, OutboxOp};
use render::{escape_html, format_history, format_user_message};
use topics::TopicsManager;
use turn_tracker::TurnTracker;

/// Configuration for the Telegram backend.
#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub allowed_user_ids: Vec<i64>,
    pub dm_enabled: bool,
    pub topics_enabled: bool,
    pub supergroup_id: Option<i64>,
    pub topic_icon: String,
    pub max_message_length: usize,
    pub edit_interval_ms: u64,
    pub verbose: bool,
    pub history_turns: usize,
    /// Path to `~/.pi/pup/` — used to scan for live sessions on startup.
    pub socket_dir: PathBuf,
    /// Path to the topics state file for persisting the topic mapping.
    pub topics_state_path: PathBuf,
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
}

impl TelegramBackend {
    pub fn new(config: TelegramConfig) -> Self {
        let bot = BotClient::new(&config.bot_token);
        let outbox = Outbox::new(bot.clone(), config.edit_interval_ms);
        let mut turn_tracker = TurnTracker::new(config.edit_interval_ms);
        turn_tracker.set_verbose(config.verbose);
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
            && topics.thread_for_session(session_id).is_some() {
                return Some(topics.chat_id());
            }

        // DM mode: use the DM chat if attached to this session.
        if self.dm.attached.as_deref() == Some(session_id) {
            return self.dm_chat_id;
        }

        None
    }

    /// Handle a Telegram update (message or callback query).
    async fn handle_update(&mut self, update: Update) {
        // Handle callback queries (cancel button).
        if let Some(cb) = update.callback_query {
            if !self.is_allowed(cb.from.id) {
                return;
            }
            if let Some(data) = &cb.data
                && let Some(session_id) = data.strip_prefix("cancel:") {
                    let _ = self.bot.answer_callback_query(&cb.id, Some("Cancelling…")).await;
                    let _ = self.incoming_tx.send(IncomingMessage {
                        session_id: session_id.to_owned(),
                        text: String::new(),
                        mode: SendMode::Steer,
                        is_cancel: true,
                    }).await;
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
        let Some(ref text) = message.text else {
            return;
        };

        let chat_id = message.chat.id;

        // Topics mode: message in a forum topic.
        if let Some(thread_id) = message.message_thread_id
            && let Some(ref topics) = self.topics
                && let Some(session_id) = topics.session_for_thread(thread_id) {
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

                    // Check for /cancel command in topic.
                    let cmd = cleaned_text.trim().split(' ').next().unwrap_or("");
                    if cmd == "/cancel" {
                        let _ = self.incoming_tx.send(IncomingMessage {
                            session_id,
                            text: String::new(),
                            mode: SendMode::Steer,
                            is_cancel: true,
                        }).await;
                        return;
                    }

                    // Determine send mode.
                    let (msg_text, mode) = if let Some(stripped) = cleaned_text.strip_prefix(">>") {
                        (stripped.trim().to_owned(), SendMode::FollowUp)
                    } else {
                        (cleaned_text, SendMode::Steer)
                    };

                    let _ = self.incoming_tx.send(IncomingMessage {
                        session_id,
                        text: msg_text,
                        mode,
                        is_cancel: false,
                    }).await;
                    return;
                }

        // DM mode.
        if self.config.dm_enabled {
            self.dm_chat_id = Some(chat_id);
            self.handle_dm_message(chat_id, text).await;
        }
    }

    /// Handle a DM message (commands or forwarding).
    async fn handle_dm_message(&mut self, chat_id: i64, text: &str) {
        let cmd = dm::parse_command(text);

        match cmd {
            DmCommand::List => {
                self.dm.last_list = self.sessions.clone();
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
                            .unwrap_or(&sid[..8.min(sid.len())]);
                        self.dm.attached = Some(sid.clone());
                        self.send_dm(
                            chat_id,
                            &format!("Attached to <b>{}</b>", escape_html(name)),
                        );
                        info!(session_id = %sid, "DM attached");
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
                    let _ = self.incoming_tx.send(IncomingMessage {
                        session_id: sid.clone(),
                        text: String::new(),
                        mode: SendMode::Steer,
                        is_cancel: true,
                    }).await;
                    self.send_dm(chat_id, "Cancelling…");
                } else {
                    self.send_dm(chat_id, "Not attached to any session.");
                }
            }
            DmCommand::Verbose { toggle } => {
                self.dm.verbose = toggle.unwrap_or(!self.dm.verbose);
                let state = if self.dm.verbose { "on" } else { "off" };
                self.send_dm(chat_id, &format!("Verbose mode: <b>{state}</b>"));
            }
            DmCommand::Help => {
                self.send_dm(chat_id, &DmState::format_help());
            }
            DmCommand::Message { text, mode } => {
                if let Some(ref sid) = self.dm.attached {
                    let _ = self.incoming_tx.send(IncomingMessage {
                        session_id: sid.clone(),
                        text,
                        mode,
                        is_cancel: false,
                    }).await;
                } else {
                    self.send_dm(
                        chat_id,
                        "Not attached. Use /ls and /attach first.",
                    );
                }
            }
        }
    }

    /// Quick helper to enqueue a DM text message.
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
            && let Some(thread_id) = topics.thread_for_session(session_id) {
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

                // Check for forum_topic_created service messages with our icon.
                if let Some(ref ftc) = msg.forum_topic_created
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
                ("verbose".to_owned(), "Toggle tool call visibility".to_owned()),
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
                ("new".to_owned(), "Start a new session".to_owned()),
                ("compact".to_owned(), "Compact session context".to_owned()),
                ("name".to_owned(), "Set session name".to_owned()),
                ("quit".to_owned(), "Quit pi session".to_owned()),
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

            info!(
                dm_enabled = self.config.dm_enabled,
                topics_enabled = self.config.topics_enabled,
                "telegram backend initialized"
            );
            Ok(())
        }
        .instrument(span)
        .await
    }

    async fn handle_event(&mut self, event: SessionEvent) -> Result<()> {
        match event {
            SessionEvent::Connected { ref info } => {
                self.sessions.push(info.clone());

                // Topics mode: create (or reuse) a topic and post recent history.
                if let Some(ref mut topics) = self.topics {
                    match topics.create_topic(&self.bot, info).await {
                        Ok((thread_id, reused)) => {
                            // Only post history for newly created topics.
                            // Reused topics already have their history from the
                            // previous daemon run.
                            if !reused && !info.history.is_empty() {
                                let msgs = format_history(
                                    &info.history,
                                    self.config.history_turns,
                                );
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
            SessionEvent::Disconnected { ref session_id } => {
                self.sessions.retain(|s| s.session_id != *session_id);

                // Topics mode: delete the topic.
                if let Some(ref mut topics) = self.topics
                    && let Err(e) = topics.delete_topic(&self.bot, session_id).await {
                        warn!(error = %e, "failed to delete topic");
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
                // Update local session info.
                if let Some(existing) = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.session_id == info.session_id)
                {
                    *existing = info.clone();
                }

                // Topics mode: rename the topic.
                if let Some(ref mut topics) = self.topics
                    && let Err(e) = topics.rename_topic(&self.bot, info).await {
                        warn!(error = %e, "failed to rename topic");
                    }
            }
            SessionEvent::SessionReset { ref session_id } => {
                info!(session_id, "session reset");
                // End any in-progress turn cleanly.
                self.outbox.clear_edit_cooldown();
                self.turn_tracker.end_turn(session_id, &mut self.outbox);
                // Post a notification in the topic.
                self.send_to_topic(session_id, "🔄 <i>Session reset</i>");

                if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id {
                        self.send_dm(chat_id, "🔄 <i>Session reset</i>");
                    }
            }
            SessionEvent::AgentStart { ref session_id } => {
                debug!(session_id, "agent started");

                // Start a new turn — the tracker will send the first
                // Telegram message lazily on the first tool/delta event.
                // Also starts a typing indicator loop in the background.
                if let Some(ref topics) = self.topics {
                    if let Some(thread_id) = topics.thread_for_session(session_id) {
                        self.turn_tracker.start_turn(
                            session_id,
                            topics.chat_id(),
                            Some(thread_id),
                            &self.bot,
                        );
                    }
                } else if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id
                {
                    self.turn_tracker
                        .start_turn(session_id, chat_id, None, &self.bot);
                }
            }
            SessionEvent::AgentEnd { ref session_id } => {
                debug!(session_id, "agent ended");
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
            SessionEvent::MessageDelta {
                ref session_id,
                ref text,
                ..
            } => {
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
                self.turn_tracker
                    .message_end_with_content(session_id, content, &mut self.outbox);
            }
            SessionEvent::ToolStart {
                ref session_id,
                ref tool_name,
                ref args,
                ..
            } => {
                self.turn_tracker
                    .tool_start(session_id, tool_name, args, &mut self.outbox);
            }
            SessionEvent::ToolEnd {
                ref session_id,
                ref tool_name,
                is_error,
                ..
            } => {
                self.turn_tracker.tool_end(
                    session_id,
                    tool_name,
                    is_error,
                    &mut self.outbox,
                );
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
                        && let Some(chat_id) = self.dm_chat_id {
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
            // Poll for Telegram updates (long-polls for up to 30s).
            match self
                .bot
                .get_updates(self.update_offset, 30)
                .await
            {
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

            // Drain any incoming messages generated by handle_update.
            if let Some(ref mut rx) = self.incoming_rx {
                match rx.try_recv() {
                    Ok(msg) => return Ok(Some(msg)),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        // No messages this cycle, continue long-polling.
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
                }
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("shutting down telegram backend");
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
