use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use tracing::{debug, debug_span, warn, Instrument};

/// Thin wrapper around the Telegram Bot API using raw reqwest calls.
#[derive(Debug, Clone)]
pub struct BotClient {
    http: reqwest::Client,
    base_url: String,
}

/// Result of a Telegram API call.
#[derive(Debug, serde::Deserialize)]
pub struct TgResponse<T> {
    pub ok: bool,
    pub result: Option<T>,
    pub description: Option<String>,
    pub error_code: Option<i32>,
    #[serde(default)]
    pub parameters: Option<TgResponseParameters>,
}

#[derive(Debug, serde::Deserialize)]
pub struct TgResponseParameters {
    pub retry_after: Option<u64>,
}

/// A Telegram Update from `getUpdates`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

/// A Telegram message.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
    pub message_thread_id: Option<i64>,
    /// Present on service messages when a forum topic is created.
    pub forum_topic_created: Option<ForumTopicCreatedMsg>,
}

/// Service message: a forum topic was created.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ForumTopicCreatedMsg {
    pub name: String,
}

/// A Telegram user.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct User {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

/// A Telegram chat.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
}

/// A callback query from inline keyboard.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub data: Option<String>,
    pub message: Option<Message>,
}

/// A sent message result.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
    pub chat: Chat,
}

/// A forum topic.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ForumTopic {
    pub message_thread_id: i64,
    pub name: String,
}

/// Chat member info.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatMember {
    pub status: String,
    pub can_manage_topics: Option<bool>,
}

/// Wrapper for getChatMember result.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatMemberResult {
    pub status: String,
    #[serde(default)]
    pub can_manage_topics: Option<bool>,
}

impl BotClient {
    pub fn new(token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: format!("https://api.telegram.org/bot{token}"),
        }
    }

    /// Make a Telegram Bot API call.
    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<T> {
        let span = debug_span!("telegram_api", method);
        async {
            let url = format!("{}/{method}", self.base_url);
            let resp = self
                .http
                .post(&url)
                .json(params)
                .send()
                .await
                .with_context(|| format!("HTTP request to {method} failed"))?;

            let status = resp.status();
            let body: TgResponse<T> = resp
                .json()
                .await
                .with_context(|| format!("{method} response parse failed"))?;

            if !body.ok {
                let code = body.error_code.unwrap_or(status.as_u16().into());
                let desc = body.description.unwrap_or_default();

                if code == 429 {
                    let retry_after = body
                        .parameters
                        .and_then(|p| p.retry_after)
                        .unwrap_or(5);
                    warn!(method, retry_after, "rate limited");
                    bail!("rate limited: retry after {retry_after}s");
                }

                bail!("Telegram API error {code}: {desc}");
            }

            body.result
                .with_context(|| format!("{method} returned ok=true but no result"))
        }
        .instrument(span)
        .await
    }

    /// Long-poll for updates.
    pub async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            &serde_json::json!({
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["message", "callback_query"]
            }),
        )
        .await
    }

    /// Send a text message.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        reply_markup: Option<&serde_json::Value>,
        message_thread_id: Option<i64>,
    ) -> Result<SentMessage> {
        let mut params = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(pm) = parse_mode {
            params["parse_mode"] = pm.into();
        }
        if let Some(rm) = reply_markup {
            params["reply_markup"] = rm.clone();
        }
        if let Some(tid) = message_thread_id {
            params["message_thread_id"] = tid.into();
        }
        self.call("sendMessage", &params).await
    }

    /// Edit a text message.
    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
        reply_markup: Option<&serde_json::Value>,
    ) -> Result<SentMessage> {
        let mut params = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        if let Some(pm) = parse_mode {
            params["parse_mode"] = pm.into();
        }
        if let Some(rm) = reply_markup {
            params["reply_markup"] = rm.clone();
        }
        self.call("editMessageText", &params).await
    }

    /// Delete a message.
    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<bool> {
        self.call(
            "deleteMessage",
            &serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }),
        )
        .await
    }

    /// Create a forum topic.
    pub async fn create_forum_topic(
        &self,
        chat_id: i64,
        name: &str,
    ) -> Result<ForumTopic> {
        debug!(chat_id, name, "creating forum topic");
        self.call(
            "createForumTopic",
            &serde_json::json!({
                "chat_id": chat_id,
                "name": name,
            }),
        )
        .await
    }

    /// Edit a forum topic name.
    pub async fn edit_forum_topic(
        &self,
        chat_id: i64,
        message_thread_id: i64,
        name: &str,
    ) -> Result<bool> {
        self.call(
            "editForumTopic",
            &serde_json::json!({
                "chat_id": chat_id,
                "message_thread_id": message_thread_id,
                "name": name,
            }),
        )
        .await
    }

    /// Delete a forum topic.
    pub async fn delete_forum_topic(
        &self,
        chat_id: i64,
        message_thread_id: i64,
    ) -> Result<bool> {
        debug!(chat_id, message_thread_id, "deleting forum topic");
        self.call(
            "deleteForumTopic",
            &serde_json::json!({
                "chat_id": chat_id,
                "message_thread_id": message_thread_id,
            }),
        )
        .await
    }

    /// Get chat member info.
    pub async fn get_chat_member(
        &self,
        chat_id: i64,
        user_id: i64,
    ) -> Result<ChatMemberResult> {
        self.call(
            "getChatMember",
            &serde_json::json!({
                "chat_id": chat_id,
                "user_id": user_id,
            }),
        )
        .await
    }

    /// Answer a callback query.
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<bool> {
        let mut params = serde_json::json!({
            "callback_query_id": callback_query_id,
        });
        if let Some(t) = text {
            params["text"] = t.into();
        }
        self.call("answerCallbackQuery", &params).await
    }

    /// Set bot commands.
    pub async fn set_my_commands(
        &self,
        commands: &[(String, String)],
    ) -> Result<bool> {
        let cmds: Vec<serde_json::Value> = commands
            .iter()
            .map(|(cmd, desc)| {
                serde_json::json!({
                    "command": cmd,
                    "description": desc,
                })
            })
            .collect();
        self.call(
            "setMyCommands",
            &serde_json::json!({ "commands": cmds }),
        )
        .await
    }

    /// Get bot info (getMe).
    pub async fn get_me(&self) -> Result<User> {
        self.call("getMe", &serde_json::json!({})).await
    }
}
