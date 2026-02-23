<p align="center">
  <img src="docs/pup.png" alt="pup logo" width="200">
</p>

<p align="center">
  <a href="https://github.com/lightclient/pup/actions/workflows/ci.yml"><img src="https://github.com/lightclient/pup/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/lightclient/pup/releases/latest"><img src="https://img.shields.io/github/v/release/lightclient/pup" alt="Release"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License: MIT/Apache-2.0"></a>
</p>

# pup

Pickup your [pi](https://github.com/badlogic/pi) and [Claude Code](https://docs.anthropic.com/en/docs/claude-code) sessions on the go.

pup bridges coding agent sessions to chat platforms so you can monitor and
interact with them from your phone.

```
┌──────────────┐     ┌──────────────┐     ┌──────────────────┐
│ pi session 1 │     │ pi session N │     │ Claude Code sess │
│  + extension │     │  + extension │     │  (auto-detected) │
└──────┬───────┘     └──────┬───────┘     └────────┬─────────┘
       │ unix sock          │ unix sock            │ transcript +
       │                    │                      │ inspector
       └────────────────────┼──────────────────────┘
                            │
                    ┌───────┴───────┐
                    │      pup      │
                    │    daemon     │
                    │ ┌──────────┐  │
                    │ │ telegram │  │
                    │ └─────┬────┘  │
                    └───────┼───────┘
                            ▼
                          phone
```

## How it works

**Pi sessions** use a dedicated extension:

1. **Pi extension** (`extension/index.ts`) — loads into each pi session,
   exposes events over a Unix socket at `~/.pi/pup/<session-id>.sock`
2. **Daemon** (Rust) — discovers those sockets, connects, and routes
   everything to Telegram

**Claude Code sessions** are detected automatically — no extension needed:

1. **Discovery** — scans `~/.claude/projects/` for active transcript files
   and matches them to running Claude Code processes
2. **Transcript tailing** — reads the `.jsonl` transcript for conversation
   events (user messages, assistant responses, tool calls)
3. **Message injection** — sends messages into the Claude Code TUI via
   Bun's inspector protocol (requires `BUN_INSPECT` env var)

Both pi and Claude Code TUIs keep working normally.

## Quick start

```bash
# install the pi extension
pi install git:github.com/lightclient/pup

# install the daemon
cargo install --git https://github.com/lightclient/pup pup-daemon

# interactive setup (creates ~/.config/pup/config.toml)
pup setup

# start
pup
```

## Claude Code support

Claude Code sessions are discovered automatically when pup is running. No
extension or plugin is required.

**Read-only mode** (just monitoring):

```bash
# Start Claude Code normally — pup will detect the transcript and stream
# conversation events to Telegram.
claude
```

**Bidirectional mode** (monitoring + send messages from Telegram):

```bash
# Launch with BUN_INSPECT so pup can inject messages into the TUI.
BUN_INSPECT="127.0.0.1:0/pup" claude --dangerously-skip-permissions
```

`BUN_INSPECT` exposes a WebSocket that pup connects to for injecting
keystrokes into the Claude Code TUI via `process.stdin.push()`.
`--dangerously-skip-permissions` is required for bidirectional mode because
pup cannot answer the interactive permission prompts that Claude Code
normally shows before tool use.

### Configuration

Claude Code integration is enabled by default. To disable or customize:

```toml
[claude_code]
enabled = true                          # default: true
projects_dir = "~/.claude/projects"     # default: ~/.claude/projects
```

### Capabilities

| Feature | pi + extension | Claude Code |
|---|---|---|
| Assistant messages | Streaming (token-level) | Complete (message-level) |
| Thinking/reasoning | Streaming | Complete |
| Tool calls | Start / update / end | Start + end |
| Send messages | ✅ via IPC | ✅ via inspector (needs `BUN_INSPECT`) |
| Cancel / abort | ✅ via IPC | ✅ via inspector |
| Session name | ✅ | — |

## Configuration

`~/.config/pup/config.toml`:

```toml
[pup]
# Directory where pi session sockets are created
socket_dir = "~/.pi/pup"

[display]
# Show tool calls in messages by default
verbose = false
# Number of conversation turns to show when attaching
history_turns = 5
# How many tool calls to keep in rendered messages (number or "all")
tool_calls = 3
# How many lines of tool output to show per tool call (number or "all")
tool_output_lines = 10

[streaming]
# Minimum interval between Telegram message edits (ms)
edit_interval_ms = 1500

[claude_code]
# Auto-discover Claude Code sessions (default: true)
enabled = true
# Path to Claude Code projects directory
projects_dir = "~/.claude/projects"

[backends.telegram]
enabled = true
bot_token = "123456:ABC-..."
allowed_user_ids = [12345678]
# Enable local voice-to-text via whisper.cpp
voice = true

[backends.telegram.dm]
enabled = true

[backends.telegram.topics]
enabled = true
supergroup_id = -1001234567890
# Emoji icon for auto-created forum topics
topic_icon = "📎"

[backends.telegram.display]
# Maximum message length before truncation
max_message_length = 3500
```

## Development

```bash
cargo test          # run tests
cargo clippy        # lint
cargo build         # debug build

# run with tracing
PUP_TRACE_FILE=traces.jsonl cargo run

# verbose console
RUST_LOG=debug cargo run
```

For faster local builds, copy the example cargo config:

```bash
cp .cargo/config.toml.example .cargo/config.toml
```

This enables sccache and the mold linker (install both first).

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design: protocol
spec, data flow diagrams, concurrency model, error handling, security model,
and testing strategy.
