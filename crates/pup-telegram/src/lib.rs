pub mod bot;
pub mod dm;
pub mod outbox;
pub mod render;
pub mod streaming;
pub mod topics;

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
use render::{escape_html, format_tool_call, format_user_message};
use streaming::StreamingManager;
use topics::TopicsManager;

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
}

/// The Telegram chat backend.
#[derive(Debug)]
pub struct TelegramBackend {
    config: TelegramConfig,
    bot: BotClient,
    bot_user_id: i64,
    /// Outbox for rate-limited API calls.
    outbox: Outbox,
    /// Streaming message manager.
    streaming: StreamingManager,
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
        let streaming = StreamingManager::new(config.edit_interval_ms);
        let (incoming_tx, incoming_rx) = mpsc::channel(64);

        let topics = if config.topics_enabled {
            config.supergroup_id.map(|id| {
                TopicsManager::new(id, config.topic_icon.clone())
            })
        } else {
            None
        };

        Self {
            config,
            bot,
            bot_user_id: 0,
            outbox,
            streaming,
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

                    // Check for /cancel command in topic.
                    if text.trim() == "/cancel" {
                        let _ = self.incoming_tx.send(IncomingMessage {
                            session_id,
                            text: String::new(),
                            mode: SendMode::Steer,
                            is_cancel: true,
                        }).await;
                        return;
                    }

                    // Determine send mode.
                    let (msg_text, mode) = if let Some(stripped) = text.strip_prefix(">>") {
                        (stripped.trim().to_owned(), SendMode::FollowUp)
                    } else {
                        (text.clone(), SendMode::Steer)
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

            // Register commands.
            let commands = vec![
                ("ls".to_owned(), "List active pi sessions".to_owned()),
                ("attach".to_owned(), "Attach to a session".to_owned()),
                ("detach".to_owned(), "Detach from session".to_owned()),
                ("cancel".to_owned(), "Cancel current operation".to_owned()),
                ("verbose".to_owned(), "Toggle tool call visibility".to_owned()),
                ("help".to_owned(), "Show help".to_owned()),
            ];
            let _ = self.bot.set_my_commands(&commands).await;

            // Validate topics setup if enabled.
            if let Some(ref topics) = self.topics {
                TopicsManager::validate(&self.bot, topics.chat_id(), self.bot_user_id)
                    .await
                    .context("topics validation failed")?;
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

                // Topics mode: create a topic.
                if let Some(ref mut topics) = self.topics
                    && let Err(e) = topics.create_topic(&self.bot, info).await {
                        warn!(error = %e, "failed to create topic");
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
            SessionEvent::AgentStart { ref session_id } => {
                debug!(session_id, "agent started");
            }
            SessionEvent::AgentEnd { ref session_id } => {
                debug!(session_id, "agent ended");
                // Flush any remaining outbox operations.
                while self.outbox.flush_one().await {}
            }
            SessionEvent::MessageStart {
                ref session_id,
                ref message_id,
            } => {
                // Start streaming: send placeholder.
                if let Some(ref topics) = self.topics {
                    if let Some(thread_id) = topics.thread_for_session(session_id) {
                        self.streaming.start_in_topic(
                            message_id,
                            session_id,
                            topics.chat_id(),
                            thread_id,
                            &mut self.outbox,
                        );
                    }
                } else if self.dm.attached.as_deref() == Some(session_id.as_str())
                    && let Some(chat_id) = self.dm_chat_id {
                        self.streaming
                            .start(message_id, session_id, chat_id, &mut self.outbox);
                    }
            }
            SessionEvent::MessageDelta {
                ref message_id,
                ref text,
                ..
            } => {
                self.streaming.delta(message_id, text, &mut self.outbox);
            }
            SessionEvent::MessageEnd {
                ref message_id,
                ref content,
                ..
            } => {
                self.streaming.end(message_id, content, &mut self.outbox);
            }
            SessionEvent::ToolStart {
                ref session_id,
                ref tool_name,
                ..
            } => {
                if self.dm.verbose || self.config.verbose {
                    debug!(session_id, tool_name, "tool started");
                }
            }
            SessionEvent::ToolEnd {
                ref session_id,
                ref tool_name,
                ref content,
                is_error,
                ..
            } => {
                if self.dm.verbose || self.config.verbose {
                    let msg = format_tool_call(tool_name, &serde_json::Value::Null, content, is_error);

                    // Send to topic.
                    self.send_to_topic(session_id, &msg);

                    // Send to DM if attached.
                    if self.dm.attached.as_deref() == Some(session_id.as_str())
                        && let Some(chat_id) = self.dm_chat_id {
                            self.send_dm(chat_id, &msg);
                        }
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
        // Poll for Telegram updates.
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
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(None),
            }
        }

        Ok(None)
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
