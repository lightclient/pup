use std::collections::HashMap;

use anyhow::Result;
use pup_core::SessionInfo;
use tracing::{debug, info, warn};

use crate::bot::BotClient;

/// Manages Telegram forum topics — one per pi session.
#[derive(Debug)]
pub struct TopicsManager {
    /// Supergroup chat ID.
    chat_id: i64,
    /// Topic icon prefix.
    topic_icon: String,
    /// session_id → thread_id mapping.
    session_topics: HashMap<String, i64>,
    /// thread_id → session_id reverse mapping.
    thread_sessions: HashMap<i64, String>,
    /// Track topic names to detect collisions.
    topic_names: HashMap<String, u32>,
}

impl TopicsManager {
    pub fn new(chat_id: i64, topic_icon: String) -> Self {
        Self {
            chat_id,
            topic_icon,
            session_topics: HashMap::new(),
            thread_sessions: HashMap::new(),
            topic_names: HashMap::new(),
        }
    }

    /// Get the supergroup chat ID.
    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    /// Get the thread ID for a session, if a topic exists.
    pub fn thread_for_session(&self, session_id: &str) -> Option<i64> {
        self.session_topics.get(session_id).copied()
    }

    /// Get the session ID for a thread, if known.
    pub fn session_for_thread(&self, thread_id: i64) -> Option<&str> {
        self.thread_sessions.get(&thread_id).map(String::as_str)
    }

    /// Generate a topic name from session info.
    pub fn topic_name(&mut self, info: &SessionInfo) -> String {
        let base = if let Some(ref name) = info.session_name {
            name.clone()
        } else {
            // Use last component of cwd, or short session ID.
            let cwd_name = info
                .cwd
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("");

            if cwd_name.is_empty() || cwd_name == "~" {
                info.session_id[..6.min(info.session_id.len())].to_owned()
            } else {
                cwd_name.to_owned()
            }
        };

        let full_base = format!("{} {base}", self.topic_icon);

        // Handle collisions.
        let count = self.topic_names.entry(base).or_insert(0);
        *count += 1;

        if *count > 1 {
            format!("{full_base} ({count})")
        } else {
            full_base
        }
    }

    /// Create a topic for a new session.
    pub async fn create_topic(
        &mut self,
        bot: &BotClient,
        info: &SessionInfo,
    ) -> Result<i64> {
        let name = self.topic_name(info);
        info!(session_id = %info.session_id, topic_name = %name, "creating topic");

        let topic = bot.create_forum_topic(self.chat_id, &name).await?;
        let thread_id = topic.message_thread_id;

        self.session_topics
            .insert(info.session_id.clone(), thread_id);
        self.thread_sessions
            .insert(thread_id, info.session_id.clone());

        info!(session_id = %info.session_id, thread_id, "topic created");
        Ok(thread_id)
    }

    /// Delete the topic for a disconnected session.
    pub async fn delete_topic(
        &mut self,
        bot: &BotClient,
        session_id: &str,
    ) -> Result<()> {
        let Some(thread_id) = self.session_topics.remove(session_id) else {
            debug!(session_id, "no topic to delete");
            return Ok(());
        };

        self.thread_sessions.remove(&thread_id);

        info!(session_id, thread_id, "deleting topic");
        match bot.delete_forum_topic(self.chat_id, thread_id).await {
            Ok(_) => {}
            Err(e) => {
                warn!(session_id, thread_id, error = %e, "failed to delete topic");
            }
        }
        Ok(())
    }

    /// Rename a topic when session info changes.
    pub async fn rename_topic(
        &mut self,
        bot: &BotClient,
        info: &SessionInfo,
    ) -> Result<()> {
        let Some(&thread_id) = self.session_topics.get(&info.session_id) else {
            return Ok(());
        };

        let name = self.topic_name(info);
        info!(session_id = %info.session_id, thread_id, new_name = %name, "renaming topic");

        bot.edit_forum_topic(self.chat_id, thread_id, &name).await?;
        Ok(())
    }

    /// Validate that the bot has the required permissions in the supergroup.
    pub async fn validate(bot: &BotClient, chat_id: i64, bot_user_id: i64) -> Result<()> {
        let member = bot.get_chat_member(chat_id, bot_user_id).await?;

        if member.status != "administrator" && member.status != "creator" {
            anyhow::bail!(
                "Bot is not an admin in the supergroup (status: {})",
                member.status
            );
        }

        if member.can_manage_topics != Some(true) {
            anyhow::bail!("Bot does not have 'can_manage_topics' permission");
        }

        info!("topics validation passed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_name_with_session_name() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned());
        let info = SessionInfo {
            session_id: "abc123".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/home/user/code".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info), "📎 myproject");
    }

    #[test]
    fn test_topic_name_from_cwd() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned());
        let info = SessionInfo {
            session_id: "abc123".to_owned(),
            session_name: None,
            cwd: "/home/user/code/foo".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info), "📎 foo");
    }

    #[test]
    fn test_topic_name_collision() {
        let mut mgr = TopicsManager::new(-1001234, "📎".to_owned());
        let info1 = SessionInfo {
            session_id: "aaa".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/tmp".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        let info2 = SessionInfo {
            session_id: "bbb".to_owned(),
            session_name: Some("myproject".to_owned()),
            cwd: "/tmp".to_owned(),
            model: None,
            history: vec![],
            streaming: false,
            partial_text: None,
        };
        assert_eq!(mgr.topic_name(&info1), "📎 myproject");
        assert_eq!(mgr.topic_name(&info2), "📎 myproject (2)");
    }
}
