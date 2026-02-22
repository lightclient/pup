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
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│ pi session 1│     │ pi session 2│     │ pi session N│
│  + extension│     │  + extension│     │  + extension│
└──────┬──────┘     └──────┬──────┘     └──────┬──────┘
       │ unix sock         │ unix sock         │ unix sock
       └───────────┬───────┴───────────────────┘
                   │
            ┌──────┴──────┐
            │    pup      │
            │   daemon    │
            │  ┌────────┐ │
            │  │telegram │ │
            │  └────┬───┘ │
            └───────┼─────┘
                    ▼
              📱 phone
```

## How it works

Two components:

1. **Pi extension** (`extension/index.ts`) — loads into each pi session,
   exposes events over a Unix socket at `~/.pi/pup/<session-id>.sock`
2. **Daemon** (Rust) — discovers those sockets, connects, and routes
   everything to Telegram

The pi TUI keeps working normally. The extension is zero-overhead when no
clients are connected.

## Install

### Extension

Install the pi extension so every pi session exposes its socket:

```bash
# via pi's package manager (recommended)
pi install git:github.com/anthropics/pup

# or manually
mkdir -p ~/.pi/agent/extensions/pup
cp extension/index.ts ~/.pi/agent/extensions/pup/
```

### Daemon

```bash
# build
cargo build --release

# run setup (creates ~/.config/pup/config.toml)
./target/release/pup setup

# start
./target/release/pup
```

## Telegram modes

**DM mode** — interact with sessions through direct messages:

- `/ls` — list active pi sessions
- `/attach <name|index|id>` — attach to a session
- `/detach` — detach
- `/cancel` — abort current operation
- `/verbose [on|off]` — show/hide tool calls

Plain messages are forwarded to the attached session. Prefix with `>>` for
follow-up (queued until the agent finishes instead of interrupting).

**Topics mode** — one forum topic per session in a Telegram supergroup. No
attach/detach needed. Topics are created/deleted automatically as sessions
start and stop.

Both modes can run simultaneously.

## Configuration

`~/.config/pup/config.toml`:

```toml
[pup]
socket_dir = "~/.pi/pup"

[display]
verbose = false
history_turns = 5

[streaming]
edit_interval_ms = 1500

[backends.telegram]
enabled = true
bot_token = "123456:ABC-..."
allowed_user_ids = [12345678]

[backends.telegram.dm]
enabled = true

[backends.telegram.topics]
enabled = true
supergroup_id = -1001234567890
```

## Project structure

```
├── package.json                Pi package manifest (for pi install)
├── extension/index.ts          Pi extension (TypeScript)
├── crates/
│   ├── pup-ipc/                IPC protocol types + Unix socket client
│   ├── pup-core/               Backend trait, session manager, discovery
│   ├── pup-telegram/           Telegram backend
│   └── pup-daemon/             Binary, config, setup wizard, tracing
└── docs/
    └── ARCHITECTURE.md         Full design document
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

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design: protocol
spec, data flow diagrams, concurrency model, error handling, security model,
and testing strategy.
