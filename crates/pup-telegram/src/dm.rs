use pup_core::SessionInfo;
use pup_ipc::SendMode;

/// DM mode state: tracks which session (if any) the user is attached to,
/// and the session list from the last `/ls` command.
#[derive(Debug, Default)]
pub struct DmState {
    /// Currently attached session ID.
    pub attached: Option<String>,
    /// Session list snapshot from last `/ls` (for index-based attach).
    pub last_list: Vec<SessionInfo>,
    /// Verbose mode (show tool calls).
    pub verbose: bool,
}

/// Parsed DM command.
#[derive(Debug)]
pub enum DmCommand {
    List,
    Attach {
        reference: String,
    },
    Detach,
    Cancel,
    Verbose {
        toggle: Option<bool>,
    },
    Help,
    /// Not a command — a plain message to forward to the attached session.
    Message {
        text: String,
        mode: SendMode,
    },
}

/// Parse a DM text into a command or message.
pub fn parse_command(text: &str) -> DmCommand {
    let trimmed = text.trim();

    if trimmed.starts_with('/') {
        let (raw_cmd, args) = match trimmed.split_once(' ') {
            Some((c, a)) => (c, a.trim()),
            None => (trimmed, ""),
        };

        // Strip @botname suffix that Telegram appends when the user picks
        // a command from the autocomplete menu (e.g. "/ls@my_pup_bot").
        let cmd = raw_cmd.split('@').next().unwrap_or(raw_cmd);

        match cmd {
            "/ls" | "/list" => DmCommand::List,
            "/attach" => DmCommand::Attach {
                reference: args.to_owned(),
            },
            "/detach" => DmCommand::Detach,
            "/cancel" => DmCommand::Cancel,
            "/verbose" => {
                let toggle = match args {
                    "on" | "true" | "1" => Some(true),
                    "off" | "false" | "0" => Some(false),
                    _ => None,
                };
                DmCommand::Verbose { toggle }
            }
            "/help" | "/start" => DmCommand::Help,
            _ => DmCommand::Message {
                text: trimmed.to_owned(),
                mode: SendMode::Steer,
            },
        }
    } else if let Some(rest) = trimmed.strip_prefix(">>") {
        // Follow-up prefix
        DmCommand::Message {
            text: rest.trim().to_owned(),
            mode: SendMode::FollowUp,
        }
    } else {
        DmCommand::Message {
            text: trimmed.to_owned(),
            mode: SendMode::Steer,
        }
    }
}

impl DmState {
    /// Try to resolve a session reference to a session ID.
    ///
    /// Supports:
    /// - Index from last `/ls` (e.g., "1", "2")
    /// - Session name match
    /// - Session ID prefix match
    pub fn resolve_session<'a>(
        &self,
        reference: &str,
        sessions: &'a [SessionInfo],
    ) -> ResolveResult<'a> {
        // Try index first.
        if let Ok(idx) = reference.parse::<usize>()
            && idx >= 1
            && idx <= self.last_list.len()
        {
            let target_id = &self.last_list[idx - 1].session_id;
            if let Some(session) = sessions.iter().find(|s| s.session_id == *target_id) {
                return ResolveResult::Found(session);
            }
            return ResolveResult::NotFound;
        }

        // Try name match.
        let by_name: Vec<&SessionInfo> = sessions
            .iter()
            .filter(|s| {
                s.session_name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(reference))
            })
            .collect();

        match by_name.len() {
            1 => return ResolveResult::Found(by_name[0]),
            n if n > 1 => return ResolveResult::Ambiguous(by_name),
            _ => {}
        }

        // Try ID prefix match.
        let by_prefix: Vec<&SessionInfo> = sessions
            .iter()
            .filter(|s| s.session_id.starts_with(reference))
            .collect();

        match by_prefix.len() {
            1 => ResolveResult::Found(by_prefix[0]),
            n if n > 1 => ResolveResult::Ambiguous(by_prefix),
            _ => ResolveResult::NotFound,
        }
    }

    /// Format the session list for display.
    pub fn format_session_list(sessions: &[SessionInfo]) -> String {
        use std::fmt::Write;

        if sessions.is_empty() {
            return "No active sessions.".to_owned();
        }

        let mut out = String::from("<b>Active sessions:</b>\n\n");
        for (i, session) in sessions.iter().enumerate() {
            let name = session
                .session_name
                .as_deref()
                .unwrap_or_else(|| &session.session_id[..8.min(session.session_id.len())]);
            let cwd_short = session.cwd.rsplit('/').next().unwrap_or(&session.cwd);

            let _ = write!(
                out,
                "<b>{}</b>. {} <i>({})</i>",
                i + 1,
                pup_telegram_escape_html(name),
                pup_telegram_escape_html(cwd_short),
            );
            out.push('\n');
        }
        out.push_str("\nUse /attach &lt;number&gt; to connect.");
        out
    }

    /// Format the help message.
    pub fn format_help() -> String {
        [
            "<b>pup — Telegram bridge for pi</b>",
            "",
            "<b>Commands:</b>",
            "/ls — List active pi sessions",
            "/attach &lt;ref&gt; — Attach to a session (name, index, or ID prefix)",
            "/detach — Detach from current session",
            "/cancel — Abort the current agent operation",
            "/status — Show session status (model, context usage)",
            "/verbose [on|off] — Toggle verbose mode (thinking + tools)",
            "/help — Show this help",
            "",
            "<b>Messaging:</b>",
            "Type normally to send a message (interrupts agent).",
            "Prefix with &gt;&gt; to queue as follow-up.",
        ]
        .join("\n")
    }
}

/// Result of resolving a session reference.
#[derive(Debug)]
pub enum ResolveResult<'a> {
    Found(&'a SessionInfo),
    Ambiguous(Vec<&'a SessionInfo>),
    NotFound,
}

/// Escape HTML for Telegram (re-export from render module logic).
fn pup_telegram_escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_list() {
        assert!(matches!(parse_command("/ls"), DmCommand::List));
        assert!(matches!(parse_command("/list"), DmCommand::List));
    }

    #[test]
    fn test_parse_attach() {
        match parse_command("/attach myproject") {
            DmCommand::Attach { reference } => assert_eq!(reference, "myproject"),
            _ => panic!("expected Attach"),
        }
    }

    #[test]
    fn test_parse_follow_up() {
        match parse_command(">> some follow-up") {
            DmCommand::Message { text, mode } => {
                assert_eq!(text, "some follow-up");
                assert_eq!(mode, SendMode::FollowUp);
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn test_parse_plain_message() {
        match parse_command("hello world") {
            DmCommand::Message { text, mode } => {
                assert_eq!(text, "hello world");
                assert_eq!(mode, SendMode::Steer);
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn test_parse_verbose() {
        match parse_command("/verbose on") {
            DmCommand::Verbose { toggle } => assert_eq!(toggle, Some(true)),
            _ => panic!("expected Verbose"),
        }
    }

    #[test]
    fn test_parse_status_forwarded_to_session() {
        // /status is not a DM-level command — it's forwarded to the pi session
        // where the extension handles it and broadcasts a notification.
        match parse_command("/status") {
            DmCommand::Message { text, mode } => {
                assert_eq!(text, "/status");
                assert_eq!(mode, SendMode::Steer);
            }
            _ => panic!("expected Message (forwarded to session)"),
        }
    }

    #[test]
    fn test_parse_commands_with_bot_suffix() {
        // Telegram appends @botname when user picks from autocomplete in groups.
        assert!(matches!(parse_command("/ls@my_pup_bot"), DmCommand::List));
        assert!(matches!(
            parse_command("/cancel@my_pup_bot"),
            DmCommand::Cancel
        ));
        assert!(matches!(
            parse_command("/detach@my_pup_bot"),
            DmCommand::Detach
        ));
        assert!(matches!(parse_command("/help@my_pup_bot"), DmCommand::Help));
        assert!(matches!(
            parse_command("/start@my_pup_bot"),
            DmCommand::Help
        ));
        match parse_command("/verbose@my_pup_bot on") {
            DmCommand::Verbose { toggle } => assert_eq!(toggle, Some(true)),
            _ => panic!("expected Verbose"),
        }
        match parse_command("/attach@my_pup_bot myproject") {
            DmCommand::Attach { reference } => assert_eq!(reference, "myproject"),
            _ => panic!("expected Attach"),
        }
    }
}
