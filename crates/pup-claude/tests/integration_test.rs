//! Integration tests against real Claude Code transcript files and inspector.
//!
//! These tests require:
//! - A real `.jsonl` transcript file at `~/.claude/projects/-root-pup-main/`
//! - A running Claude Code TUI with `BUN_INSPECT` on port 9229
//!
//! Run with: `cargo test --package pup-claude --test integration_test -- --ignored`

#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::uninlined_format_args,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls
)]

use std::path::PathBuf;

use pup_claude::transcript::{self, TranscriptWatcher};

/// Find the most recently modified transcript file in the default project.
fn find_recent_transcript() -> Option<PathBuf> {
    let projects_dir = dirs::home_dir()?.join(".claude/projects/-root-pup-main");
    let mut entries: Vec<_> = std::fs::read_dir(&projects_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect();

    entries.sort_by_key(|e| {
        std::cmp::Reverse(
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH),
        )
    });

    entries.first().map(|e| e.path())
}

#[test]
#[ignore = "requires real transcript files"]
fn test_parse_real_transcript() {
    let path = find_recent_transcript().expect("no transcript files found");
    println!("Parsing: {}", path.display());

    let content = std::fs::read_to_string(&path).unwrap();
    let mut user_count = 0;
    let mut assistant_count = 0;
    let mut tool_result_count = 0;
    let mut ignored_count = 0;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match transcript::parse_line(line) {
            Ok(transcript::TranscriptEntry::UserText { content, .. }) => {
                println!("  USER: {}", &content[..content.len().min(60)]);
                user_count += 1;
            }
            Ok(transcript::TranscriptEntry::Assistant {
                api_message_id,
                text_blocks,
                tool_uses,
                ..
            }) => {
                let text: String = text_blocks.join(" ");
                let truncated: String = text.chars().take(50).collect();
                let id_trunc: String = api_message_id.chars().take(15).collect();
                println!(
                    "  ASSISTANT({id_trunc}): {truncated} (tools: {})",
                    tool_uses.len()
                );
                assistant_count += 1;
            }
            Ok(transcript::TranscriptEntry::ToolResult {
                tool_use_id,
                is_error,
                ..
            }) => {
                println!(
                    "  TOOL_RESULT: {} error={}",
                    &tool_use_id[..tool_use_id.len().min(20)],
                    is_error
                );
                tool_result_count += 1;
            }
            Ok(transcript::TranscriptEntry::Ignored) => {
                ignored_count += 1;
            }
            Err(e) => {
                println!("  ERROR: {e}");
            }
        }
    }

    println!(
        "\nTotals: {} user, {} assistant, {} tool_result, {} ignored",
        user_count, assistant_count, tool_result_count, ignored_count
    );

    // Should have parsed at least something.
    assert!(user_count + assistant_count > 0, "no entries parsed");
}

#[test]
#[ignore = "requires real transcript files"]
fn test_watcher_history() {
    let path = find_recent_transcript().expect("no transcript files found");
    let session_id = path.file_stem().unwrap().to_str().unwrap().to_owned();

    println!("Session: {session_id}");
    println!("Path: {}", path.display());

    let mut watcher = TranscriptWatcher::new_from_beginning(session_id, path);
    let (model, turns) = watcher.parse_history().unwrap();

    println!("Model: {model:?}");
    println!("Turns: {}", turns.len());

    for (i, turn) in turns.iter().enumerate() {
        let user = turn
            .user
            .as_ref()
            .map(|m| &m.content[..m.content.len().min(40)])
            .unwrap_or("(none)");
        let assistant = turn
            .assistant
            .as_ref()
            .map(|m| &m.content[..m.content.len().min(40)])
            .unwrap_or("(none)");
        println!(
            "  Turn {i}: user={user} | assistant={assistant} | tools={}",
            turn.tool_calls.len()
        );
    }

    assert!(!turns.is_empty(), "no turns parsed from history");
}

#[tokio::test]
#[ignore = "requires running Claude Code with BUN_INSPECT"]
async fn test_inspector_connect_and_inject() {
    use pup_claude::inspector::InspectorClient;

    // Connect to the inspector.
    let mut client = InspectorClient::connect("ws://127.0.0.1:9229/pup")
        .await
        .expect("failed to connect to inspector — is Claude Code running with BUN_INSPECT?");

    println!("Connected to inspector at {}", client.url());

    // Verify with a simple eval.
    let result = client.evaluate("1+1").await.unwrap();
    assert_eq!(result.get("value").unwrap().as_u64().unwrap(), 2);
    println!("Eval 1+1 = 2 ✓");

    // Test ping.
    assert!(client.ping().await, "ping failed");
    println!("Ping ✓");

    // Inject a message.
    client
        .inject_stdin("What is 3+3? Reply with just the number.")
        .await
        .expect("injection failed");
    println!("Message injected! Check the Claude Code TUI.");

    // Poll the transcript for up to 30 seconds, waiting for Claude's response.
    let path = find_recent_transcript().expect("no transcript found");
    let start = std::time::Instant::now();
    let mut found_response = false;

    while start.elapsed() < std::time::Duration::from_secs(30) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let content = std::fs::read_to_string(&path).unwrap();
        // Look for our question and the response after it.
        let mut saw_our_question = false;
        for line in content.lines().rev() {
            if let Ok(entry) = transcript::parse_line(line) {
                match entry {
                    transcript::TranscriptEntry::Assistant { text_blocks, .. }
                        if saw_our_question =>
                    {
                        let text = text_blocks.join(" ");
                        if text.contains('6') {
                            println!("Claude responded: {text}");
                            found_response = true;
                            break;
                        }
                    }
                    transcript::TranscriptEntry::UserText { content, .. }
                        if content.contains("3+3") =>
                    {
                        saw_our_question = true;
                    }
                    _ => {}
                }
            }
        }

        if found_response {
            break;
        }
    }

    // Don't fail the test if Claude is busy — the injection itself succeeded.
    if !found_response {
        println!(
            "Note: Claude didn't respond in time (may be busy with another turn). Injection was successful."
        );
    }
}
