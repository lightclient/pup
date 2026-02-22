#![allow(clippy::print_stdout)]

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

use crate::config::default_config_path;

/// Read a line from stdin (blocking).
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().lock().read_line(&mut buf)?;
    Ok(buf.trim().to_owned())
}

/// Run the interactive setup wizard.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_setup() -> Result<()> {
    use std::fmt::Write;
    println!("pup — setup");
    println!("============\n");

    // ── Telegram setup ──────────────────────────────────────────

    println!("── Telegram ──\n");

    // Bot token
    let bot_token =
        prompt("1. Create a bot via @BotFather and paste the token.\n   Bot token: ")?;

    if bot_token.is_empty() {
        anyhow::bail!("Bot token is required.");
    }

    // Verify the token
    let bot = pup_telegram::bot::BotClient::new(&bot_token);
    match bot.get_me().await {
        Ok(me) => {
            println!(
                "   ✓ Verified: @{}\n",
                me.username.as_deref().unwrap_or("unknown")
            );
        }
        Err(e) => {
            anyhow::bail!("Invalid bot token: {e}");
        }
    }

    // User ID
    let user_id_str =
        prompt("2. Get your Telegram user ID from @userinfobot.\n   User ID: ")?;
    let user_id: i64 = user_id_str
        .parse()
        .context("Invalid user ID — must be a number")?;
    println!("   ✓ Saved\n");

    // Topics mode
    let topics_answer = prompt("3. Topics mode (optional):\n   Enable topics? [y/N]: ")?;
    let topics_enabled = topics_answer.eq_ignore_ascii_case("y");

    let mut supergroup_id: Option<i64> = None;

    if topics_enabled {
        // Drain any old updates so we only see fresh ones.
        let _ = bot.get_updates(0, 0).await;
        let drain = bot.get_updates(0, 0).await.unwrap_or_default();
        let mut offset: i64 = drain
            .iter()
            .map(|u| u.update_id + 1)
            .max()
            .unwrap_or(0);

        println!("   Add the bot to your supergroup as an admin (with Manage Topics),");
        println!("   then send any message in the group. Waiting...");
        io::stdout().flush()?;

        let sg_id = loop {
            let updates = bot.get_updates(offset, 30).await.unwrap_or_default();
            for update in &updates {
                if update.update_id >= offset {
                    offset = update.update_id + 1;
                }
                if let Some(ref msg) = update.message
                    && msg.chat.chat_type == "supergroup" {
                        break; // not the outer loop — handled below
                    }
            }
            // Check if any update contained a supergroup message.
            if let Some(sg) = updates.iter().find_map(|u| {
                u.message
                    .as_ref()
                    .filter(|m| m.chat.chat_type == "supergroup")
                    .map(|m| m.chat.id)
            }) {
                break sg;
            }
        };

        println!("   ✓ Detected supergroup: {sg_id}");

        // Verify permissions — loop until the bot has what it needs.
        let me = bot.get_me().await?;
        loop {
            match pup_telegram::topics::TopicsManager::validate(&bot, sg_id, me.id).await {
                Ok(()) => {
                    println!("   ✓ Bot has required permissions\n");
                    break;
                }
                Err(e) => {
                    println!("   ✗ {e}");
                    println!("   Make the bot an admin with \"Manage Topics\" permission.");
                    prompt("   Press Enter to re-check...")?;
                }
            }
        }

        supergroup_id = Some(sg_id);
    }

    // ── Generate config ─────────────────────────────────────────

    let config_path = default_config_path()?;
    let config_dir = config_path
        .parent()
        .context("config path has no parent")?;

    std::fs::create_dir_all(config_dir)?;

    let mut config = String::new();
    config.push_str("[pup]\nsocket_dir = \"~/.pi/pup\"\n\n");
    config.push_str("[display]\nverbose = false\nhistory_turns = 5\n\n");
    config.push_str("[streaming]\nedit_interval_ms = 1500\n\n");
    config.push_str("[backends.telegram]\n");
    config.push_str("enabled = true\n");
    let _ = writeln!(config, "bot_token = \"{bot_token}\"");
    let _ = writeln!(config, "allowed_user_ids = [{user_id}]\n");
    config.push_str("[backends.telegram.dm]\nenabled = true\n\n");

    if topics_enabled {
        config.push_str("[backends.telegram.topics]\n");
        config.push_str("enabled = true\n");
        if let Some(sg) = supergroup_id {
            let _ = writeln!(config, "supergroup_id = {sg}");
        }
        config.push_str("topic_icon = \"📎\"\n\n");
    }

    config.push_str("[backends.telegram.display]\nmax_message_length = 3500\n");

    // Write config with restricted permissions.
    write_config_file(&config_path, &config)?;

    println!("Config saved to {}", config_path.display());
    println!("Run `pup` to start.");

    Ok(())
}

/// Write config file with 0600 permissions.
fn write_config_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }

    info!(path = %path.display(), "config written");
    Ok(())
}
