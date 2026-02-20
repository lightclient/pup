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

```bash
# 1. Install the extension
mkdir -p ~/.pi/agent/extensions/pup
cp extension/index.ts ~/.pi/agent/extensions/pup/

# 2. Build the daemon
cargo build --release
# binary at: target/release/pup

# 3. Run setup (creates ~/.config/pup/config.toml)
./target/release/pup setup

# 4. Start
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
