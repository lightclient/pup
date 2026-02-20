#![allow(clippy::print_stdout)]

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

use crate::config::default_config_path;

/// Run the interactive setup wizard.
pub(crate) async fn run_setup() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = stdin.lock();

    println!("pup — setup");
    println!("============\n");

    // ── Telegram setup ──────────────────────────────────────────

    println!("── Telegram ──\n");

    // Bot token
    print!("1. Create a bot via @BotFather and paste the token.\n   Bot token: ");
    stdout.flush()?;
    let mut bot_token = String::new();
    reader.read_line(&mut bot_token)?;
    let bot_token = bot_token.trim().to_owned();

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
    print!("2. Get your Telegram user ID from @userinfobot.\n   User ID: ");
    stdout.flush()?;
    let mut user_id_str = String::new();
    reader.read_line(&mut user_id_str)?;
    let user_id: i64 = user_id_str
        .trim()
        .parse()
        .context("Invalid user ID — must be a number")?;
    println!("   ✓ Saved\n");

    // Topics mode
    print!("3. Topics mode (optional):\n   Enable topics? [y/N]: ");
    stdout.flush()?;
    let mut topics_answer = String::new();
    reader.read_line(&mut topics_answer)?;
    let topics_enabled = topics_answer.trim().eq_ignore_ascii_case("y");

    let mut supergroup_id: Option<i64> = None;

    if topics_enabled {
        print!("   Supergroup chat ID: ");
        stdout.flush()?;
        let mut sg_str = String::new();
        reader.read_line(&mut sg_str)?;
        let sg_id: i64 = sg_str
            .trim()
            .parse()
            .context("Invalid supergroup ID — must be a number")?;
        supergroup_id = Some(sg_id);

        // Verify
        let me = bot.get_me().await?;
        match pup_telegram::topics::TopicsManager::validate(&bot, sg_id, me.id).await {
            Ok(()) => println!("   ✓ Supergroup verified, bot has permissions\n"),
            Err(e) => {
                println!("   ⚠ Warning: {e}");
                println!("   (You can fix this later and re-run setup)\n");
            }
        }
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
    config.push_str(&format!("bot_token = \"{bot_token}\"\n"));
    config.push_str(&format!("allowed_user_ids = [{user_id}]\n\n"));
    config.push_str("[backends.telegram.dm]\nenabled = true\n\n");

    if topics_enabled {
        config.push_str("[backends.telegram.topics]\n");
        config.push_str("enabled = true\n");
        if let Some(sg) = supergroup_id {
            config.push_str(&format!("supergroup_id = {sg}\n"));
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
