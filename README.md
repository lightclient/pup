<p align="center">
  <img src="docs/pup.png" alt="pup logo" width="200">
</p>

<p align="center">
  <a href="https://github.com/lightclient/pup/actions/workflows/ci.yml"><img src="https://github.com/lightclient/pup/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/lightclient/pup/releases/latest"><img src="https://img.shields.io/github/v/release/lightclient/pup" alt="Release"></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%2FApache--2.0-blue" alt="License: MIT/Apache-2.0"></a>
</p>

# pup

Pickup your [pi](https://github.com/badlogic/pi) sessions on the go.

pup bridges pi coding agent sessions to chat platforms so you can monitor and
interact with them from your phone. Telegram first, others later.

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│ pi session 1 │     │ pi session 2 │     │ pi session N │
│  + extension │     │  + extension │     │  + extension │
└──────┬───────┘     └──────┬───────┘     └──────┬───────┘
       │ unix sock          │ unix sock          │ unix sock
       └────────────────────┼────────────────────┘
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

Two components:

1. **Pi extension** (`extension/index.ts`) — loads into each pi session,
   exposes events over a Unix socket at `~/.pi/pup/<session-id>.sock`
2. **Daemon** (Rust) — discovers those sockets, connects, and routes
   everything to Telegram

The pi TUI keeps working normally. The extension is zero-overhead when no
clients are connected.

## Quick start

```bash
# install the pi extension
pi install git:github.com/lightclient/pup

# build the daemon
cargo build --release

# interactive setup (creates ~/.config/pup/config.toml)
./target/release/pup setup

# start
./target/release/pup
```

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
