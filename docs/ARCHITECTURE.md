# pup — Architecture

Pickup your coding agent sessions on the go.

Bridge between coding agent sessions and chat platforms. Supports two agent
types:

1. **Pi sessions** — via a TypeScript extension that runs inside each pi session,
   exposing session state and streaming events over a Unix domain socket.
2. **Claude Code sessions** — via transcript file watching and optional
   `BUN_INSPECT` WebSocket injection for bidirectional control.

Both feed into a single **daemon** (Rust) that routes everything through one or
more chat **backends** (Telegram first, others later).

---

## Table of Contents

- [Glossary](#glossary)
- [Overview](#overview)
- [Component 1 — Pi Extension](#component-1--pi-extension)
  - [Pi SDK Hooks Used](#pi-sdk-hooks-used)
  - [Socket Protocol](#socket-protocol)
  - [Events (server → client)](#events-server--client)
  - [Commands (client → server)](#commands-client--server)
  - [Discovery & Lifecycle](#discovery--lifecycle)
- [Component 2 — Daemon (Rust)](#component-2--daemon-rust)
  - [Crate Structure](#crate-structure)
  - [pup-core — The Backend Trait](#pup-core--the-backend-trait)
  - [Session Manager](#session-manager)
  - [Session Discovery](#session-discovery)
  - [IPC Client](#ipc-client)
  - [pup-claude — Claude Code Integration](#pup-claude--claude-code-integration)
  - [pup-telegram — Telegram Backend](#pup-telegram--telegram-backend)
  - [Configuration](#configuration)
  - [Setup Wizard](#setup-wizard)
  - [Linting & Tooling](#linting--tooling)
  - [Observability](#observability)
- [Data Flow](#data-flow)
- [Error Handling & Edge Cases](#error-handling--edge-cases)
- [Security & Access Control](#security--access-control)
- [Concurrency Model](#concurrency-model)
- [State & Persistence](#state--persistence)
- [Echo Suppression & Message Attribution](#echo-suppression--message-attribution)
- [Connection Resilience](#connection-resilience)
- [Graceful Shutdown](#graceful-shutdown)
- [Testing Strategy](#testing-strategy)
- [Build & Installation](#build--installation)
- [Open Questions / Future Work](#open-questions--future-work)

---

## Glossary

| Term | Meaning |
|------|---------|
| **pi** | The coding agent TUI (`@mariozechner/pi-coding-agent`) |
| **Claude Code** | Anthropic's CLI coding agent (`claude`) — supported via `pup-claude` |
| **pup** | This project — the bridge between agent sessions and chat platforms |
| **extension** | TypeScript module loaded into a pi session (our "Component 1") |
| **daemon** | Long-running Rust process that connects to extensions and drives chat backends |
| **backend** | A chat platform integration (Telegram, Discord, etc.) |
| **session** | An agent session (conversation + tool history stored as JSONL) — either pi or Claude Code |
| **transcript** | Claude Code's `.jsonl` conversation log in `~/.claude/projects/` |
| **inspector** | Bun's `BUN_INSPECT` WebSocket debugger, used to inject stdin into Claude Code |
| **steer** | Interrupt the agent mid-stream to deliver a message immediately |
| **follow-up** | Queue a message until the agent finishes its current work |
| **turn** | One LLM response plus any resulting tool calls |
| **topic** | Telegram forum topic — one per session in topics mode |
| **DM mode** | Telegram DM-based interaction with `/attach` / `/detach` |

---

## Overview

```
  pi sessions                         Claude Code sessions
┌─────────────┐  ┌─────────────┐    ┌─────────────┐  ┌─────────────┐
│ pi session 1│  │ pi session N│    │ CC session 1│  │ CC session N│
│  + extension│  │  + extension│    │   (TUI ✓)   │  │   (TUI ✓)   │
│    (TUI ✓)  │  │    (TUI ✓)  │    └──────┬──────┘  └──────┬──────┘
└──────┬──────┘  └──────┬──────┘           │ transcript      │ transcript
       │ unix sock      │ unix sock        │ + inspector?    │ + inspector?
       │                │                  │                 │
       └────────┬───────┘                  └────────┬────────┘
                │                                   │
         ┌──────┴──────┐                   ┌────────┴────────┐
         │ pi sessions │                   │  Claude Code    │
         │  (pup-core) │                   │  (pup-claude)   │
         └──────┬──────┘                   └────────┬────────┘
                │         SessionEvent              │
                └──────────────┬────────────────────┘
                               │
                        ┌──────┴──────┐
                        │     pup     │
                        │   (Rust)    │
                        │             │
                        │  ┌────────┐ │     future:
                        │  │telegram │ │     ┌──────────┐
                        │  └────┬───┘ │     │ discord  │
                        └───────┼─────┘     │ slack    │
                                │           │ signal   │
                                ▼           └──────────┘
                         ┌───────────┐
                         │  phone     │
                         └───────────┘
```

Both agent types feed the same `SessionEvent` stream to backends. Pi sessions
use a Unix socket IPC protocol (bidirectional by default). Claude Code sessions
use transcript file watching (read path) with optional `BUN_INSPECT` WebSocket
injection (write path). The TUI for each agent continues to work normally.

---

## Component 1 — Pi Extension

**Location:** `extension/index.ts`
**Install:** symlink or copy into `~/.pi/agent/extensions/pup/`

The extension hooks into pi's event system and creates a Unix domain socket
server. It is always active when loaded (no flag gating) — the overhead is
negligible when no clients are connected.

The extension is backend-agnostic. It knows nothing about Telegram or any other
chat platform. It exposes raw session state and events over a simple protocol.

### Pi SDK Hooks Used

The extension uses these pi APIs to tap into session state and events:

**Event subscriptions** (`pi.on()`):

| Pi Event | What the extension does |
|----------|------------------------|
| `session_start` | Create socket server, emit `hello` + `history` to any connecting clients |
| `session_shutdown` | Emit `session_end`, tear down socket server, remove socket file + aliases |
| `agent_start` | Emit `agent_start` to all connected clients |
| `agent_end` | Emit `agent_end` to all connected clients |
| `turn_start` | Emit `turn_start { turn_index }` |
| `turn_end` | Emit `turn_end { turn_index }` |
| `message_start` | Emit `message_start { role, message_id }` |
| `message_update` | Extract `text_delta` from `event.assistantMessageEvent`, emit `message_delta` |
| `message_end` | Emit `message_end` with final content |
| `tool_execution_start` | Emit `tool_start { tool_call_id, tool_name, args }` |
| `tool_execution_update` | Emit `tool_update` with partial result content |
| `tool_execution_end` | Emit `tool_end { tool_call_id, tool_name, content, is_error }` |
| `model_select` | Emit `model_changed { model }` |
| `input` | When `event.source !== "extension"`, emit as `user_message` event so backends can show TUI-typed prompts |

**State access** (`ctx.sessionManager`):

| Method | Used for |
|--------|----------|
| `getBranch()` | Walk current branch to reconstruct last N turns for `history` event |
| `getSessionId()` | Socket filename and `hello` payload |
| `getSessionFile()` | Include in `hello` for session identification |
| `getCwd()` | Include in `hello` so backends can show working directory |
| `getSessionName()` | Alias symlink and `hello` payload |

**Message sending** (`pi.sendUserMessage()`):

When the daemon sends a `send` command over IPC, the extension calls
`pi.sendUserMessage(text, { deliverAs })`. This makes the message appear in the
pi TUI as if the user typed it, and triggers the agent to process it. The
`deliverAs` option maps directly to pi's steer/follow-up semantics:

- `mode: "steer"` → `pi.sendUserMessage(text, { deliverAs: "steer" })` — interrupts
- `mode: "follow_up"` → `pi.sendUserMessage(text, { deliverAs: "followUp" })` — queued

**Session name tracking:**

The extension watches for name changes via `model_select`-style events and also
polls `pi.getSessionName()` on a 1-second interval (cheap, string comparison).
When the name changes, it updates the `.alias` symlink and emits
`session_name_changed`.

### Socket Protocol

- **Transport:** Unix domain socket at `~/.pi/pup/<session-id>.sock`
- **Framing:** Newline-delimited JSON (one JSON object per `\n`-terminated line)
- **Direction:**
  - Server → Client: events (pushed continuously)
  - Client → Server: commands (request/response)
- **Multiple clients:** Supported. Each connected client independently receives
  the full event stream and can send commands.
- **Handshake:** On connect, the server immediately sends a `hello` event
  followed by a `history` event. No client hello required.

### Events (server → client)

All events: `{ "type": "event", "event": "<name>", "data": { ... } }`

**Connection events** (sent immediately on connect):

| Event | Data | Notes |
|-------|------|-------|
| `hello` | `{ session_id, session_name?, cwd, model?, session_file?, thinking_level }` | Always first |
| `history` | `{ turns: Turn[], streaming: boolean, partial_text?: string }` | Last N turns. If `streaming` is true, `partial_text` contains the accumulated assistant response so far. |

**Streaming events** (sent as they happen):

| Event | Data |
|-------|------|
| `agent_start` | `{}` |
| `agent_end` | `{}` |
| `turn_start` | `{ turn_index }` |
| `turn_end` | `{ turn_index }` |
| `message_start` | `{ role, message_id }` |
| `message_delta` | `{ message_id, text }` |
| `message_end` | `{ message_id, role, content }` |
| `tool_start` | `{ tool_call_id, tool_name, args }` |
| `tool_update` | `{ tool_call_id, tool_name, content }` |
| `tool_end` | `{ tool_call_id, tool_name, content, is_error }` |
| `session_name_changed` | `{ name }` |
| `model_changed` | `{ model }` |
| `user_message` | `{ content, source, echo }` |
| `session_end` | `{}` |

**`Turn` object** (used in `history`):

```typescript
interface Turn {
  user: { content: string; timestamp: number } | null;
  assistant: { content: string; timestamp: number } | null;
  tool_calls: {
    tool_call_id: string;
    tool_name: string;
    args: Record<string, unknown>;
    content: string;
    is_error: boolean;
  }[];
}
```

### Commands (client → server)

All commands: `{ "type": "<command>", "id"?: string, ... }`
All responses: `{ "type": "response", "command": "<command>", "id"?: string, "success": boolean, "data"?: ..., "error"?: string }`

| Command | Parameters | Description |
|---------|-----------|-------------|
| `send` | `{ message, mode?: "steer" \| "follow_up" }` | Send a user message. Uses `pi.sendUserMessage()` so it appears in the TUI as if the user typed it. |
| `abort` | `{}` | Cancel current agent operation |
| `get_info` | `{}` | Returns current session info (same shape as `hello` data) |
| `get_history` | `{ turns?: number }` | Returns last N turns |

### Discovery & Lifecycle

The extension manages socket files and alias symlinks in `~/.pi/pup/`:

- `<session-id>.sock` — the actual socket
- `<session-name>.alias` — symlink to `<session-id>.sock`

| Pi Event | Extension Action |
|----------|-----------------|
| `session_start` | Create socket, create alias if session has a name |
| `session_switch` | Tear down old socket, create new one |
| `session_shutdown` | Emit `session_end`, close server, remove socket + aliases |

Alias is synced on every event dispatch (cheap check) and via a 1-second
interval timer, matching the pattern from the existing `control.ts` extension.

**History reconstruction:** On client connect, the extension walks
`ctx.sessionManager.getBranch()` to extract the last N turns (default 5,
configurable via `get_history`). The branch walk filters for `message` type
entries and groups them into `Turn` objects (user prompt + assistant response +
tool calls). If the agent is currently streaming, the extension also includes
the in-progress partial message assembled from accumulated `message_update`
deltas.

**User input forwarding:** The extension subscribes to `input` events. When a
user types in the pi TUI (`source: "interactive"`), the extension emits a
`user_message` event to all connected clients so backends can display it. When
the source is `"extension"` (i.e., the message was injected by this very
extension via `pi.sendUserMessage()`), the extension checks its echo-tracking
set and tags the event with `echo: true` to prevent feedback loops. See
[Echo Suppression](#echo-suppression--message-attribution) for details.

---

## Component 2 — Daemon (Rust)

### Crate Structure

Following the workspace pattern from [coop](https://github.com/lightclient/coop):

```
daemon/
├── Cargo.toml                    # workspace root
├── Cargo.lock
├── .cargo/config.toml            # sccache + mold linker
├── clippy.toml
├── .rustfmt.toml
├── rust-toolchain.toml
│
├── crates/
│   ├── pup-ipc/                  # IPC protocol types + client
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── protocol.rs       # ClientMessage / ServerMessage enums
│   │       └── client.rs         # Unix socket client, reconnection
│   │
│   ├── pup-core/                 # Backend trait, shared types, session manager
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── backend.rs        # ChatBackend trait
│   │       ├── session.rs        # SessionManager — owns IPC connections
│   │       ├── discovery.rs      # Watch socket dir for new/removed sockets
│   │       ├── render.rs         # Markdown → plain text / common transforms
│   │       └── types.rs          # SessionInfo, SessionEvent, IncomingMessage
│   │
│   ├── pup-claude/               # Claude Code session integration
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # Module exports
│   │       ├── injector.rs       # ClaudeService — main loop, event routing
│   │       ├── discovery.rs      # Process + transcript scanning
│   │       ├── transcript.rs     # .jsonl parser + TranscriptWatcher
│   │       ├── inspector.rs      # BUN_INSPECT WebSocket client
│   │       └── session.rs        # Per-session state + inspector state machine
│   │
│   ├── pup-telegram/             # Telegram backend implementation
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # impl ChatBackend for TelegramBackend
│   │       ├── bot.rs            # BotClient — thin reqwest wrapper
│   │       ├── dm.rs             # DM mode (/ls, /attach, /detach)
│   │       ├── topics.rs         # Topics mode (create/delete per session)
│   │       ├── outbox.rs         # Rate-limited message send/edit queue
│   │       ├── render.rs         # Markdown → Telegram HTML
│   │       └── streaming.rs      # Accumulate deltas, rate-limited edits
│   │
│   └── pup-daemon/               # Main binary — wires everything together
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs           # CLI entry (start / setup)
│           ├── config.rs         # Config loading + validation
│           └── setup.rs          # Interactive setup wizard
```

### pup-core — The Backend Trait

The core crate defines the interface between the session manager and chat
backends. Backends are compiled in (not dynamically loaded) — the daemon's
`main.rs` instantiates the configured backends and passes them to the session
manager. This is a seam, not a plugin system.

```rust
/// Events the session manager pushes to backends.
enum SessionEvent {
    /// A new pi session was discovered and connected.
    Connected { info: SessionInfo },
    /// A pi session disconnected (exited).
    Disconnected { session_id: String },
    /// Session metadata changed (name, model, etc).
    InfoChanged { info: SessionInfo },
    /// Agent started processing a prompt.
    AgentStart { session_id: String },
    /// Agent finished processing.
    AgentEnd { session_id: String },
    /// A new assistant message began streaming.
    MessageStart { session_id: String, message_id: String },
    /// Streaming text delta for an in-progress assistant message.
    MessageDelta { session_id: String, message_id: String, text: String },
    /// An assistant message finished.
    MessageEnd { session_id: String, message_id: String, content: String },
    /// A tool started executing.
    ToolStart { session_id: String, tool_call_id: String, tool_name: String,
                args: serde_json::Value },
    /// Streaming partial output from a tool.
    ToolUpdate { session_id: String, tool_call_id: String, tool_name: String,
                 content: String },
    /// A tool finished executing.
    ToolEnd { session_id: String, tool_call_id: String, tool_name: String,
              content: String, is_error: bool },
    /// A user message was sent (from pi TUI or another backend).
    /// `echo` is true if this message originated from pup (via IPC send command).
    UserMessage { session_id: String, content: String, echo: bool,
                  source: MessageSource },
}

#[derive(Debug, Clone)]
enum MessageSource {
    /// Typed in the pi TUI
    Interactive,
    /// Sent via pup IPC (from some backend)
    Extension,
}

/// Info about a connected pi session.
struct SessionInfo {
    session_id: String,
    session_name: Option<String>,
    cwd: String,
    model: Option<String>,
    history: Vec<Turn>,
}

/// A message from the chat backend directed at a pi session.
struct IncomingMessage {
    session_id: String,
    text: String,
    mode: SendMode,        // Steer or FollowUp
}

/// What backends implement.
#[async_trait]
trait ChatBackend: Send {
    /// Called once at startup after config is loaded.
    async fn init(&mut self) -> Result<()>;

    /// Receive the next session event. The session manager calls this in a
    /// loop. Backends should process the event (send Telegram messages, etc.)
    /// and return quickly. Heavy work (API calls) should be spawned or queued.
    async fn handle_event(&mut self, event: SessionEvent) -> Result<()>;

    /// Poll for incoming messages from the chat platform. Returns None if
    /// the backend has shut down. The session manager routes returned messages
    /// to the appropriate IPC connection.
    async fn poll_incoming(&mut self) -> Result<Option<IncomingMessage>>;

    /// Provide a snapshot of active sessions for /ls commands, etc.
    /// Called by the backend itself via a handle to the session manager.
    fn session_list(&self) -> &[SessionInfo];

    /// Graceful shutdown.
    async fn shutdown(&mut self) -> Result<()>;
}
```

**Why a trait and not just channels?** The trait makes the contract explicit and
testable. Each backend method has clear semantics. But internally, backends are
free to use channels, task spawning, etc.

**What stays out of the trait:**

- Rendering (each backend has its own formatting constraints)
- Rate limiting (Telegram's limits are very different from Discord's)
- Authentication / setup (backend-specific config)
- DM vs topics distinction (Telegram-specific concept)

The trait deals only in backend-agnostic session events and incoming messages.

### Session Manager

The session manager (`pup-core::session`) is the hub. It:

1. Runs the discovery loop (watches `~/.pi/pup/` for sockets)
2. Owns all IPC connections (one per pi session)
3. Reads IPC events from each connection, translates them to `SessionEvent`s
4. Fans out each `SessionEvent` to all registered backends
5. Reads `IncomingMessage`s from each backend, routes to the correct IPC
   connection

```
                    ┌──────────────────────┐
                    │   Session Manager    │
                    │                      │
   sockets ───────►│  discovery loop      │
                    │        │             │
                    │  ┌─────▼──────┐      │
                    │  │ IPC conns  │      │
                    │  │ (per sess) │      │
                    │  └─────┬──────┘      │
                    │        │ SessionEvent│
                    │  ┌─────▼──────┐      │
                    │  │  fan-out   │      │
                    │  └──┬─────┬───┘      │
                    │     │     │          │
                    └─────┼─────┼──────────┘
                          │     │
                    ┌─────▼┐  ┌▼──────┐
                    │ tg   │  │future │
                    │ back │  │backend│
                    └──┬───┘  └───┬───┘
                       │          │
       IncomingMessage │          │ IncomingMessage
                       └────┬─────┘
                            │
                    ┌───────▼──────────────┐
                    │  Session Manager     │
                    │  routes to IPC conn  │
                    └──────────────────────┘
```

Each backend runs in its own tokio task. Communication between the session
manager and backends uses `tokio::sync::mpsc` channels:

- **Session manager → backend:** `mpsc::Sender<SessionEvent>` (one per backend)
- **Backend → session manager:** `mpsc::Sender<IncomingMessage>` (shared)

The session manager select-loops over:
- IPC reader events from all connections
- `IncomingMessage`s from the shared incoming channel
- Discovery events (new socket / removed socket)

### Session Discovery

Watches `~/.pi/pup/` for socket files:

1. **On startup:** Enumerate all `.sock` files. Probe each with a connect
   attempt (connect + immediate disconnect, same approach as `control.ts`'s
   `isSocketAlive`). Connect to live ones.
2. **Ongoing:** Use `notify` crate to watch the directory. When a new `.sock`
   appears, probe and connect. When one disappears, clean up.
3. **Also resolves `.alias` symlinks** to map session names to IDs.

### IPC Client

The `pup-ipc` crate provides typed IPC communication, following the pattern
from `coop-ipc`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Send { message: String, mode: Option<String>, id: Option<String> },
    Abort { id: Option<String> },
    GetInfo { id: Option<String> },
    GetHistory { turns: Option<u32>, id: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Event { event: String, data: serde_json::Value },
    Response { command: String, id: Option<String>, success: bool,
              data: Option<serde_json::Value>, error: Option<String> },
}
```

The `IpcClient` connects to a Unix socket, splits into reader/writer halves
(like coop), and provides:
- `async fn recv(&mut self) -> Result<ServerMessage>` — reads next line, deserializes
- `async fn send(&mut self, msg: ClientMessage) -> Result<()>` — serializes, writes line

### pup-claude — Claude Code Integration

The `pup-claude` crate adds support for Claude Code sessions without requiring
a pi extension. It discovers Claude Code sessions by watching transcript files
and process state, then feeds the same `SessionEvent` stream to backends.

Unlike pi sessions (which use a bidirectional Unix socket), Claude Code
integration is asymmetric:

- **Read path** (always available): parse `.jsonl` transcript files for
  conversation events.
- **Write path** (requires `BUN_INSPECT`): inject keystrokes into the Claude
  Code TUI via Bun's WebKit Inspector Protocol.

#### Architecture Overview

```
~/.claude/projects/                        /proc/<pid>/
├── -root-myproject/                       ├── cmdline  → "claude\0..."
│   ├── <session-uuid>.jsonl  ◄──poll──    ├── environ  → "BUN_INSPECT=ws://..."
│   └── ...                                └── cwd      → /root/myproject
│
└── -home-user-code/
    └── <session-uuid>.jsonl

        │                                          │
        │ TranscriptWatcher (500ms poll)           │ Process scan (5s)
        ▼                                          ▼
┌──────────────────────────────────────────────────────────┐
│                    ClaudeService                         │
│                                                          │
│  ┌─────────────────┐  ┌───────────────────────────────┐  │
│  │   Discovery      │  │  Per-session state            │  │
│  │   (5s interval)  │  │  ┌──────────────────────┐    │  │
│  │                  │  │  │ TranscriptWatcher     │    │  │
│  │  proc scan ──────┤  │  │  (file offset tracking│    │  │
│  │  transcript scan─┤  │  │   + entry → event)   │    │  │
│  └─────────────────┘  │  ├──────────────────────┤    │  │
│                        │  │ InspectorClient?     │    │  │
│                        │  │  (WebSocket to Bun)  │    │  │
│                        │  └──────────────────────┘    │  │
│                        └───────────────────────────────┘  │
│                                                          │
│  convert_event() ──► mpsc<SessionEvent> ──► backends     │
└──────────────────────────────────────────────────────────┘
```

#### Session Discovery

Two strategies run in parallel every 5 seconds:

**1. Process scanning** (`find_claude_processes`):

Iterates `/proc/<pid>/` entries looking for Claude Code processes. A process is
identified as Claude Code if its cmdline contains `claude/versions/`,
`claude-code`, or starts with `claude\0`. For each match, the scanner reads:

| Source | Data |
|--------|------|
| `/proc/<pid>/cmdline` | Process identification |
| `/proc/<pid>/environ` | `BUN_INSPECT` WebSocket URL, `PWD` |
| `/proc/<pid>/cwd` | Fallback working directory |

**2. Transcript scanning** (`find_recent_transcripts`):

Watches `~/.claude/projects/<project-slug>/` for `.jsonl` files modified in the
last 5 minutes. The project slug encodes the working directory path
(`/root/myproject` → `-root-myproject`). This is lossy — directory names
containing `-` are ambiguous — but sufficient for display.

**Deduplication:** Multiple transcript files may exist for the same Claude Code
process (e.g., old sessions that were resumed). The scanner picks the most
recently modified transcript per PID to avoid duplicate tracking.

**Stale session detection:** Sessions are marked gone when:
- The transcript is no longer found in a scan
- The transcript has been inactive longer than 60 seconds AND the process is dead

Recently-gone sessions are suppressed for 10 minutes to prevent rediscovery
loops (e.g., a transcript file that's still on disk after the process exits).

**Same-PID replacement:** If a process starts a new session (same PID, different
transcript), the old session is automatically disconnected and replaced.

**Discovery events:**

| Event | When |
|-------|------|
| `SessionAppeared` | New transcript + process match found |
| `SessionGone` | Session inactive or process exited |
| `InspectorDiscovered` | `BUN_INSPECT` URL found for a session that previously had none |

#### Transcript Parsing

**File format:** Claude Code writes all conversation data to
`~/.claude/projects/<slug>/<session-uuid>.jsonl`. Each line is an independent
JSON object with a `type` field.

**Entry types:**

| `type` field | Parsed as | Description |
|-------------|-----------|-------------|
| `"user"` (string content) | `UserText` | User prompt text |
| `"user"` (array with `tool_result`) | `ToolResult` | Tool execution result |
| `"assistant"` | `Assistant` | Model response — text, thinking, and tool_use blocks |
| `"file-history-snapshot"` | `Ignored` | File backup metadata |
| `"progress"` | `Ignored` | Subagent/task progress |

**Assistant entry structure:**

An assistant entry's `message.content` is an array of typed blocks:

```json
[
  { "type": "thinking", "thinking": "let me think..." },
  { "type": "text", "text": "Here's the answer." },
  { "type": "tool_use", "id": "toolu_01", "name": "Bash", "input": {"command": "ls"} }
]
```

Multiple assistant entries may share the same `message.id` (the API message ID)
as Claude Code writes incremental updates. The watcher tracks the latest state
per API message ID and deduplicates accordingly.

#### Transcript Watcher

`TranscriptWatcher` polls a single `.jsonl` file for new content every 500ms.
It uses file offset tracking — each poll seeks to where it left off and reads
any new complete lines (partial lines are left for the next poll).

**State tracking:**

```rust
struct TranscriptWatcher {
    offset: u64,                                    // File position
    seen_messages: HashMap<String, AssistantState>,  // Per API message
    seen_tool_starts: HashSet<String>,               // Deduplicate ToolStart
    pending_message_id: Option<String>,              // Awaiting MessageEnd
    agent_started: bool,                             // Current turn state
}
```

**Event generation:**

| Transcript entry | Emitted events |
|-----------------|----------------|
| `UserText` | Flush pending → `UserMessage` |
| `Assistant` (first for this `message.id`) | `AgentStart` (if first in turn) → `MessageStart` → `ToolStart` per new tool |
| `Assistant` (update for existing `message.id`) | `ToolStart` for any new tool_use blocks |
| `ToolResult` | Flush pending → `ToolEnd` |
| 3s inactivity | Flush pending → `MessageEnd` + `AgentEnd` |

**"Flush pending"** means: if there's an in-progress assistant message, emit
`MessageEnd` with the accumulated text. This happens when a `UserText` or
`ToolResult` entry arrives (indicating the previous assistant turn is complete),
or after 3 seconds of inactivity (the stale timeout).

**No streaming deltas:** Unlike the pi extension which emits `MessageDelta`
events in real-time, the transcript watcher only sees complete entries. Backends
receive `MessageStart` followed by `MessageEnd` with the full text — no
intermediate edits. This means Telegram renders Claude Code responses as a
single message rather than streaming edits.

**History parsing:** On session connect, `parse_history()` reads the entire
transcript to reconstruct `Turn` objects (user message + assistant response +
tool calls). This is the same format used by pi sessions, so backends can render
a catch-up summary.

#### Inspector Client (BUN_INSPECT)

The `InspectorClient` connects to Bun's WebKit Inspector Protocol over
WebSocket. This is the write path — it enables sending messages to Claude Code
from chat platforms.

**Connection:**

```
claude process (Bun runtime)
  └─ BUN_INSPECT=ws://127.0.0.1:9229/<id>
       └─ WebSocket ← InspectorClient
            └─ Runtime.evaluate("process.stdin.push(...)")
```

The client connects and immediately verifies with `1+1 = 2`. If verification
fails, the connection is rejected.

**Message injection** (`inject_stdin`):

Injecting a message requires three separate `process.stdin.push()` calls,
because Ink's TUI input handler processes each push as a discrete event:

| Step | What | Why |
|------|------|-----|
| 1. `\x15` × 2 (Ctrl+U) | Clear existing input | 50ms delay between, ensures clean slate |
| 2. Message text (hex-encoded) | Push the actual message | Hex encoding avoids JS string escaping issues |
| 3. `\x0d` (Enter) | Submit | Triggers Ink's submit handler |

**Cancel** (`inject_escape`): Sends `\x1b` (Escape) to abort the current
operation.

**Availability:** The inspector is only available when Claude Code was launched
with the `BUN_INSPECT` environment variable set. Without it, the session is
read-only — backends can display events but cannot send messages.

#### Inspector State Machine

Each session tracks its inspector connection through a state machine:

```
                         ┌───────────────┐
     (no BUN_INSPECT) ──►│  Unavailable  │
                         └───────────────┘

                         ┌───────────────┐
  (URL found in /proc) ──►│  Discovered  │
                         └───────┬───────┘
                                 │ connect_inspector()
                          ┌──────┴──────┐
                     ┌────▼────┐   ┌────▼────┐
                     │Connected│   │  Lost   │
                     │ (ready) │   │(backoff)│
                     └─────────┘   └────┬────┘
                                        │ retry (5s tick)
                                        └─► Discovered
```

| State | Meaning |
|-------|---------|
| `Unavailable` | No `BUN_INSPECT` URL known. Read-only mode. |
| `Discovered` | URL found, connection not yet attempted. |
| `Connected` | WebSocket active, injection available. |
| `Lost` | Connection failed. Exponential backoff: 2s → 4s → 8s → … → 30s max. |

The `ClaudeService` retries `Lost` and `Discovered` connections every 5 seconds.
A successful reconnection emits a notification to backends.

#### ClaudeService (Integration Layer)

`ClaudeService` (`injector.rs`) is the main entry point. It runs as a tokio task
alongside the pi session manager, producing the same `SessionEvent` stream.

**Run loop** (`tokio::select!`):

| Arm | Source | Action |
|-----|--------|--------|
| Discovery events | `mpsc<DiscoveryEvent>` | Connect/disconnect sessions, update inspector URLs |
| Commands | `mpsc<ClaudeCommand>` | Inject messages or cancel via inspector |
| Transcript poll (500ms) | Timer | Poll all `TranscriptWatcher`s, emit `SessionEvent`s |
| Inspector retry (5s) | Timer | Reconnect `Lost`/`Discovered` inspectors |
| Shutdown | `watch<bool>` | Emit `Disconnected` for all sessions, exit |

**Session lifecycle:**

| Event | Actions |
|-------|---------|
| `SessionAppeared` | Create `ClaudeSession`, parse history, connect inspector (if URL available), emit `Connected` + capability notification |
| `SessionGone` | Remove session, emit `Disconnected` |
| `InspectorDiscovered` | Update session's inspector URL, attempt connection |

**Commands:**

```rust
enum ClaudeCommand {
    InjectMessage { session_id, text, reply: Sender<Result<()>> },
    Cancel { session_id },
}
```

`InjectMessage` routes to `inspector.inject_stdin()`. `Cancel` sends Escape.
Both require a connected inspector — otherwise they return an error.

**Event conversion:** Internal `pup_claude::SessionEvent` variants are mapped to
`pup_core::SessionEvent` at the boundary via `convert_event()`. The mapping is
straightforward except:

- `thinking` blocks from assistant messages are currently discarded
- `ToolEnd` events from transcripts don't carry `tool_name` (set to empty
  string) because tool result entries in the transcript don't include it

#### Session Registry

A `SessionRegistry` (`Arc<RwLock<HashSet<String>>>`) tracks which session IDs
belong to Claude Code sessions. The daemon uses this to route incoming messages
— if a session ID is in the registry, the message goes to `ClaudeService` via
`ClaudeCommand::InjectMessage` instead of the pi session manager's IPC
connection.

### pup-telegram — Telegram Backend

Implements `ChatBackend` for Telegram. This crate contains everything
Telegram-specific. Nothing in `pup-core` or `pup-ipc` knows about Telegram.

#### Bot Client

**No framework.** Direct `reqwest` calls to `https://api.telegram.org/bot<token>/`.

Long polling (`getUpdates` with timeout) drives `poll_incoming()`. Outgoing
calls go through the outbox.

Methods used:
- `getUpdates` — poll for messages
- `sendMessage` — send new message
- `editMessageText` — update message in-place (streaming)
- `deleteMessage` — clean up
- `createForumTopic` / `deleteForumTopic` — topic lifecycle
- `getChat` / `getChatMember` — validate setup
- `setMyCommands` — register bot commands

#### DM Mode

**Commands:**

| Command | Description |
|---------|-------------|
| `/ls` or `/list` | List active pi sessions with index numbers |
| `/attach <ref>` | Attach to a session by name, index, or ID prefix |
| `/detach` | Detach from current session |
| `/cancel` | Abort the current agent operation |
| `/verbose [on\|off]` | Toggle verbose mode (show tool calls) |
| `/help` | Show available commands |

When attached, non-command messages become `IncomingMessage`s routed to the
session. Session events stream into the DM chat.

**Session references** for `/attach` are flexible:

| Input | Resolution |
|-------|-----------|
| `/attach 1` | Index from last `/ls` output |
| `/attach myproject` | Match against session name (set via pi's `/name`) |
| `/attach a3f` | Prefix match against session ID |

If the reference is ambiguous (matches multiple sessions), the bot replies with
the matching sessions and asks the user to be more specific.

**Auto-detach:** When the attached session disconnects (pi exits), the bot sends
a "Session ended" message and automatically detaches. The user doesn't need to
`/detach` manually.

**Notifications while detached:** Optionally (config `dm.notify_on_idle`), the
bot sends a brief notification when a session's agent finishes processing
(`agent_end`), so the user knows there's output to review. These notifications
include the session name and a one-line preview.

#### Topics Mode

Requires a Telegram **supergroup with topics enabled**.

| Session Event | Telegram Action |
|---------------|----------------|
| `Connected` | `createForumTopic` named `📎 <session-name or short-id>` |
| `InfoChanged` (name) | Rename topic |
| `Disconnected` | `deleteForumTopic` immediately |

Within a topic: full event stream rendered, user messages forwarded as
`IncomingMessage`s. No `/attach` / `/detach` needed.

**Topic naming:**

| Session state | Topic name |
|---------------|-----------|
| Has name "myproject" | `📎 myproject` |
| No name, cwd `/home/user/code/foo` | `📎 foo` |
| No name, no useful cwd | `📎 a3f29b` (short session ID) |

If a name collision occurs (two sessions named "myproject"), the second gets a
suffix: `📎 myproject (2)`.

On `InfoChanged` (session renamed), the topic is renamed via
`editForumTopic`. The old name is not preserved.

**Validation on startup** (inspired by takopi's `_validate_topics_setup`):
- Verify bot is admin in the supergroup
- Verify `can_manage_topics` permission
- Verify the chat is a supergroup with topics enabled

#### Outbox & Rate Limiting

Inspired by takopi's `TelegramOutbox`. All Telegram API calls go through a
priority queue:

```rust
struct Outbox {
    queue: BinaryHeap<OutboxOp>,
    min_interval: Duration,      // 33ms global (~30 msg/sec)
    edit_cooldown: Duration,     // 1.5s per (chat_id, message_id) pair
    last_send: Instant,
    last_edit: HashMap<(i64, i64), Instant>,
}
```

Priority: **Send > Delete > Edit**.

On `429 Too Many Requests`, the outbox respects `Retry-After`.

#### Rendering

Telegram HTML parse mode.

**Markdown → Telegram HTML:**
- `**bold**` → `<b>bold</b>`
- `` `code` `` → `<code>code</code>`
- ` ```blocks``` ` → `<pre>blocks</pre>` (language hint dropped)
- `[text](url)` → `<a href="url">text</a>`
- Headers → `<b>header</b>\n`

**User messages from pi TUI** (not from Telegram):
```html
👤 <i>user prompt text here</i>
```

**Tool calls** (verbose mode):
```html
<b>bash</b>
<pre>ls -la</pre>
<pre>file1.txt
file2.txt
file3.txt
. . . (15 more lines)</pre>
```

Tool output is streamed via `tool_update` events and shown incrementally.
The number of output lines per tool call is controlled by `tool_output_lines`
(default: 10). Set to `"all"` to show complete output.

**Truncation:** `MAX_BODY_CHARS = 3500` (safety margin under Telegram's 4096
limit). Long messages split at paragraph/code-fence boundaries. Code fences
closed before split, reopened after (takopi pattern). Continuations get
`(continued 2/3)` headers.

**Cancel button:** Inline keyboard with "cancel" during streaming. Removed on
`message_end`.

#### Streaming Edits

1. On `MessageStart`: send placeholder (`⏳`), store `message_id`.
2. On `MessageDelta`: accumulate text. If ≥1.5s since last edit, enqueue
   `editMessageText` in the outbox.
3. On `MessageEnd`: final `editMessageText` with complete content + remove
   cancel button.

If the response completes in < 1.5s, only one API call is made.

Tool calls are batched into a single "tools" message per turn, updated at the
same cadence.

**Cancel button implementation:**

The placeholder message sent on `MessageStart` includes an inline keyboard:

```json
{
  "inline_keyboard": [[
    { "text": "✖ Cancel", "callback_data": "cancel:<session_id>" }
  ]]
}
```

Each `editMessageText` during streaming preserves this keyboard. On
`MessageEnd`, the final edit removes the keyboard (`reply_markup` omitted or
set to `{"inline_keyboard": []}`).

When the user taps Cancel, Telegram sends a `callback_query` update. The
backend parses `cancel:<session_id>`, sends an `IncomingMessage` with a special
cancel flag, and the session manager translates it to `ClientMessage::Abort`
over IPC. The bot also calls `answerCallbackQuery` with text "Cancelling…".

#### Bidirectional Messaging

Non-command messages in Telegram become `IncomingMessage { session_id, text, mode: Steer }`.

**Follow-up prefix:** `>>` prefix → `mode: FollowUp` (queued until agent
finishes rather than interrupting).

**Cancel:** `/cancel` → session manager sends `ClientMessage::Abort` over IPC.

### Configuration

**File:** `~/.config/pup/config.toml`

```toml
[pup]
socket_dir = "~/.pi/pup"

[display]
verbose = false                     # Show tool calls by default
history_turns = 5                   # Turns to replay on attach/topic create
tool_output_lines = 10              # Lines of tool output per call (or "all")

[streaming]
edit_interval_ms = 1500             # Min ms between message edits

# ── Telegram backend ──────────────────────────────────────────────

[backends.telegram]
enabled = true
bot_token = "123456:ABC-..."
allowed_user_ids = [12345678]

[backends.telegram.dm]
enabled = true

[backends.telegram.topics]
enabled = true
supergroup_id = -1001234567890
topic_icon = "📎"

[backends.telegram.display]
max_message_length = 3500           # Telegram-specific (< 4096 limit)

# ── Future: Discord backend ───────────────────────────────────────
# [backends.discord]
# enabled = false
# bot_token = "..."
# guild_id = 123456
```

Config is structured so that `[pup]`, `[display]`, and `[streaming]` are global
defaults. Each backend lives under `[backends.<name>]` and can override display
settings. The daemon reads `[backends.*]`, and for each enabled one,
instantiates the corresponding `ChatBackend` implementation.

### Setup Wizard

`pup setup` walks the user through backend configuration:

```
pup — setup
============

Which backends do you want to configure?
  [x] Telegram
  [ ] (more coming soon)

── Telegram ──

1. Create a bot via @BotFather and paste the token.
   Bot token: 123456:ABC-...
   ✓ Verified: @my_pi_bot

2. Get your Telegram user ID from @userinfobot.
   User ID: 12345678
   ✓ Saved

3. Topics mode (optional):
   Enable topics? [y/N]: y
   Supergroup chat ID: -1001234567890
   ✓ Supergroup verified, bot has permissions

Config saved to ~/.config/pup/config.toml
Run `pup` to start.
```

### Linting & Tooling

Following coop's strict configuration:

**`.cargo/config.toml`:**
```toml
[build]
rustc-wrapper = "sccache"

[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

**`rust-toolchain.toml`:**
```toml
[toolchain]
channel = "stable"
```

**`.rustfmt.toml`:**
```toml
edition = "2024"
max_width = 100
tab_spaces = 4
use_field_init_shorthand = true
use_try_shorthand = true
```

**`clippy.toml`:**
```toml
cognitive-complexity-threshold = 25
too-many-arguments-threshold = 6
type-complexity-threshold = 250
```

**Workspace `Cargo.toml` lints** (matching coop):
```toml
[workspace.lints.rust]
elided_lifetimes_in_paths = "warn"
missing_debug_implementations = "warn"
single_use_lifetimes = "warn"
trivial_numeric_casts = "warn"
unreachable_pub = "warn"
unsafe_code = "deny"
unused_import_braces = "warn"
unused_lifetimes = "warn"
unused_macro_rules = "warn"
unused_qualifications = "warn"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
doc_markdown = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
module_name_repetitions = "allow"
must_use_candidate = "allow"
derive_partial_eq_without_eq = "warn"
needless_pass_by_value = "warn"
or_fun_call = "warn"
redundant_clone = "warn"
significant_drop_tightening = "warn"
clone_on_ref_ptr = "warn"
dbg_macro = "warn"
if_then_some_else_none = "warn"
map_err_ignore = "warn"
needless_raw_strings = "warn"
print_stderr = "warn"
print_stdout = "warn"
rest_pat_in_fully_bound_structs = "warn"
str_to_string = "warn"
undocumented_unsafe_blocks = "warn"
unneeded_field_pattern = "warn"
unwrap_used = "warn"
```

**Key dependencies:**
```toml
[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
clap = { version = "4", features = ["derive"] }
futures = "0.3"
notify = "7"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.26"            # WebSocket client for BUN_INSPECT
chrono = { version = "0.4", default-features = false, features = ["std"] }
toml = "0.8"
tracing = "0.1"
tracing-appender = "0.2"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

### Observability

JSONL tracing for agent debugging. Agents (and humans) need to quickly diagnose
issues like: why did a session not appear, why did a Telegram message fail to
send, why is streaming laggy, why did a backend crash. Adapted from
[coop's otel-plan](https://github.com/lightclient/coop/blob/main/docs/otel-plan.md).

#### Design Principles

1. **JSONL is the primary debug interface.** AI agents read `traces.jsonl`
   directly. Every span and event is machine-parseable NDJSON.
2. **Console is a superset.** Everything in JSONL also appears on console (at
   appropriate level). Console adds human-friendly formatting and colors.
3. **Activated by environment.** `PUP_TRACE_FILE=traces.jsonl` enables file
   output. Without it, behavior is identical to running without tracing — zero
   overhead.
4. **No feature without traces.** All new code must include tracing
   instrumentation. This is the debugging interface, not an afterthought.

#### Tracing Subscriber Setup

Module: `pup-daemon/src/tracing_setup.rs`

Layered `tracing_subscriber::Registry`:

| Layer | Activation | Filter | Format |
|-------|-----------|--------|--------|
| **Console** | Always | `RUST_LOG` (default `info`) | `fmt()` compact, no target |
| **JSONL file** | `PUP_TRACE_FILE` env var | `RUST_LOG` or default `debug` | `fmt::layer().json()` with `FmtSpan::CLOSE`, `with_span_list(true)`, `with_file(true)`, `with_line_number(true)` |

No OTLP exporter for now — pup is a local daemon, not a distributed system.
Can be added later behind a cargo feature if needed.

```rust
pub fn init() -> Result<TracingGuard> { ... }
```

Returns a guard held alive in `main()` for non-blocking writer flush on
shutdown.

#### Span Hierarchy

```
session_manager                              (socket_dir)
├── discovery_scan                           (socket_count, alive_count)
│   └── socket_probe                         (session_id, path, alive)
├── session_connect                          (session_id)
│   └── ipc_handshake                        (session_id, session_name, cwd)
├── session_disconnect                       (session_id, reason)
├── ipc_recv                                 (session_id, event_type)
├── event_fanout                             (session_id, event_type, backend_count)
│   └── backend_handle_event                 (backend, event_type)
└── route_incoming                           (session_id, backend, mode)
    └── ipc_send                             (session_id, command)

claude_service
├── claude_discovery                         (dir)
│   └── scan                                (transcript_count, process_count)
├── connect_session                          (session_id, path, inspector?)
│   └── parse_history                        (turns, model)
├── disconnect_session                       (session_id)
├── handle_command                           (command_type, session_id)
│   └── inject_stdin / inject_escape         (session_id)
├── poll_all_transcripts                     (session_count)
│   └── transcript_poll                      (session_id, events_emitted)
└── retry_inspector_connections              (session_count)
    └── inspector_connect                    (session_id, url, success)

telegram_backend                             (bot_username, dm_enabled, topics_enabled)
├── init                                     (supergroup_id, user_count)
│   └── validate_topics                      (chat_id, is_admin, can_manage_topics)
├── poll_updates                             (timeout, update_count)
│   └── handle_update                        (update_id, chat_id, from_user)
├── topic_create                             (session_id, topic_name)
│   └── telegram_api                         (method=createForumTopic, status)
├── topic_delete                             (session_id, topic_id)
│   └── telegram_api                         (method=deleteForumTopic, status)
├── handle_event                             (session_id, event_type)
│   ├── streaming_start                      (session_id, message_id)
│   ├── streaming_edit                       (session_id, accumulated_len, edit_num)
│   └── streaming_end                        (session_id, final_len, total_edits)
├── outbox_flush                             (queue_len, op_type)
│   └── telegram_api                         (method, chat_id, status, retry_after?)
├── render                                   (input_len, output_len, split_count)
└── backend_restart                          (attempt, backoff_ms)
```

#### Instrumentation by Crate

**`pup-ipc`** — `tracing` only (no subscriber):
- `IpcClient::connect()` — `debug_span!("ipc_connect", path)`
- `IpcClient::recv()` — `trace!` (very chatty, only in JSONL at trace level)
- `IpcClient::send()` — `debug!(command, session_id)`

**`pup-core`** — `tracing` only:
- `SessionManager::run()` — `info_span!("session_manager", socket_dir)`
- Discovery scan — `info_span!("discovery_scan")` with counts
- Socket probe — `debug_span!("socket_probe", session_id, path)` with `alive`
- Connect — `info!(session_id, session_name, "session connected")`
- Disconnect — `info!(session_id, reason, "session disconnected")`
- Fan-out — `debug_span!("event_fanout", session_id, event_type)`
- Route incoming — `info_span!("route_incoming", session_id, backend, mode)`

**`pup-telegram`** — `tracing` only:
- `TelegramBackend::init()` — `info_span!("telegram_init")`
- `validate_topics()` — `info_span!("validate_topics", chat_id)` with
  `is_admin`, `can_manage_topics`
- `poll_updates()` — `debug_span!("poll_updates")` with `update_count`
- Every Bot API call — `debug_span!("telegram_api", method, chat_id)` with
  `status`, `retry_after`
- Topic create/delete — `info!` with session_id, topic_name/id
- Streaming lifecycle — `debug!` on start/edit/end with accumulated length
- Outbox flush — `debug_span!("outbox_flush", queue_len, op_type)`
- Rate limit hit — `warn!(method, retry_after, "rate limited")`
- Backend crash/restart — `error!(err, "backend crashed")`,
  `info!(attempt, backoff_ms, "restarting backend")`

**`pup-claude`** — `tracing` only:
- `ClaudeService::run()` — `info_span!("claude_service")`
- `ClaudeDiscovery::run()` — `info_span!("claude_discovery", dir)`
- Discovery scan — `debug!` on scan failures
- Session connect — `info!(session_id, path, "connecting Claude Code session")`
- Session disconnect — `info!(session_id, "Claude Code session disconnected")`
- Inspector connect — `info!(session_id, "inspector connected")`,
  `warn!(session_id, error, "inspector connect failed")`
- Late inspector discovery — `info!(session_id, url, pid, "late inspector discovery")`
- Stale session replacement — `info!(old_session, new_session, pid, "replacing stale CC session")`
- Transcript poll errors — `debug!(session_id, error, "transcript poll failed")`

**`pup-daemon`** — owns the subscriber setup:
- `main()` — `info!(config_path, backends, "starting pup")`
- Config load — `debug!(config_path, "loaded config")`
- Shutdown — `info!("shutting down")`

#### Log Levels

| Level | What goes here |
|-------|---------------|
| `error` | Backend crash, IPC connection broken unexpectedly, config invalid |
| `warn` | Telegram 429 rate limit, API call failed (retryable), stale socket |
| `info` | Session connect/disconnect, backend init/shutdown, topic create/delete, incoming message routed |
| `debug` | Every Telegram API call, outbox flush, streaming edits, discovery scan, event fan-out |
| `trace` | Raw IPC recv/send (every JSON line), raw Telegram update payloads |

#### Agent Debugging Recipes

```bash
# Run pup with JSONL tracing
PUP_TRACE_FILE=traces.jsonl pup

# Tail live
tail -f traces.jsonl | jq -r '[.timestamp, .level, .fields.message // .span.name] | join(" ")'

# Show errors
grep '"level":"ERROR"' traces.jsonl | jq .

# Show all session connect/disconnect
grep -E '"session connected"|"session disconnected"' traces.jsonl | jq .

# Show Telegram API failures
grep '"telegram_api"' traces.jsonl | jq 'select(.fields.status != 200)'

# Show rate limiting events
grep '"rate limited"' traces.jsonl | jq '{method: .fields.method, retry_after: .fields.retry_after}'

# Show incoming messages routed to sessions
grep '"route_incoming"' traces.jsonl | jq '{session: .span.session_id, backend: .span.backend, mode: .span.mode}'

# Show backend crashes
grep '"backend crashed"' traces.jsonl | jq .

# Show full lifecycle of a specific session
SESSION=abc123 grep "$SESSION" traces.jsonl | jq .

# Clear
rm -f traces.jsonl
```

#### `.gitignore`

```
traces.jsonl
*.jsonl
```

---

## Data Flow

### Viewing a streaming response (Topics mode)

```
pi LLM response
  │
  ├─ extension: message_start(assistant)
  │    └─ IPC → session manager → SessionEvent::MessageStart
  │         └─ fan-out → TelegramBackend::handle_event
  │              └─ outbox.enqueue(Send) → topic gets placeholder message
  │
  ├─ extension: message_delta("Hello ")
  │    └─ IPC → session manager → SessionEvent::MessageDelta
  │         └─ TelegramBackend accumulates text
  │
  ├─ extension: message_delta("world, ")
  │    └─ TelegramBackend accumulates, 1.5s elapsed →
  │         outbox.enqueue(Edit, "Hello world, ")
  │
  └─ extension: message_end(full_content)
       └─ SessionEvent::MessageEnd
            └─ outbox.enqueue(Edit, final content, remove cancel button)
```

### Sending a message from Telegram

```
User types in Telegram topic
  │
  └─ getUpdates → TelegramBackend::poll_incoming
       │
       └─ returns IncomingMessage { session_id, text, Steer }
            │
            └─ session manager routes to IPC connection
                 │
                 └─ IpcClient::send(ClientMessage::Send { message, mode })
                      │
                      └─ extension: pi.sendUserMessage(text)
                           │
                           └─ pi processes as normal user prompt
                                (TUI shows the message too)
```

### Viewing a Claude Code response (Topics mode)

```
Claude Code writes assistant entry to transcript
  │
  ├─ TranscriptWatcher: poll() sees new line at offset N
  │    └─ parse_line() → Assistant { api_message_id, text, tool_uses }
  │         └─ process_assistant() → SessionEvent::AgentStart
  │                                + SessionEvent::MessageStart
  │                                + SessionEvent::ToolStart (per tool)
  │              └─ convert_event() → pup_core::SessionEvent
  │                   └─ event_tx → fan-out → TelegramBackend::handle_event
  │                        └─ outbox.enqueue(Send) → topic gets message
  │
  └─ 3s stale timeout (no more transcript activity)
       └─ maybe_flush_stale() → SessionEvent::MessageEnd + AgentEnd
            └─ TelegramBackend: final message with complete content
```

Note: Unlike pi sessions which stream `MessageDelta` events, Claude Code
sessions emit `MessageStart` + `MessageEnd` with full text (no intermediate
edits). The backend receives the complete response in one shot.

### Sending a message to Claude Code from Telegram

```
User types in Telegram topic
  │
  └─ getUpdates → TelegramBackend::poll_incoming
       │
       └─ returns IncomingMessage { session_id, text, Steer }
            │
            └─ daemon checks SessionRegistry:
                 session_id in claude_registry?
                 │
                 ├─ YES → ClaudeCommand::InjectMessage { session_id, text }
                 │         │
                 │         └─ ClaudeService::handle_command
                 │              └─ session.inject_message(text)
                 │                   └─ InspectorClient::inject_stdin
                 │                        ├─ Ctrl+U × 2 (clear)
                 │                        ├─ hex-encoded text (push)
                 │                        └─ Enter (submit)
                 │                             └─ Claude Code processes prompt
                 │
                 └─ NO → IPC route to pi session (existing flow)
```

### Session discovery (topics mode)

```
User starts `pi` with the extension loaded
  │
  └─ extension: creates ~/.pi/pup/<session-id>.sock
       │
       └─ session manager: notify watcher fires
            │
            ├─ probe socket, connect, receive hello + history
            │
            └─ SessionEvent::Connected → fan-out to all backends
                 │
                 └─ TelegramBackend: createForumTopic("📎 <name>")

User exits `pi`
  │
  └─ extension: emits session_end, removes .sock
       │
       └─ session manager: IPC reader returns EOF
            │
            └─ SessionEvent::Disconnected → fan-out
                 │
                 └─ TelegramBackend: deleteForumTopic(topic_id)
```

### Claude Code session discovery

```
User starts `claude` in a project directory
  │
  └─ Claude Code writes to ~/.claude/projects/-root-myproject/<uuid>.jsonl
       │
       └─ ClaudeDiscovery: scan() finds recently-modified .jsonl
            │
            ├─ find_claude_processes(): match PID via cwd / session ID
            │    └─ read BUN_INSPECT from /proc/<pid>/environ
            │
            └─ DiscoveryEvent::SessionAppeared
                 │
                 └─ ClaudeService: connect_session()
                      ├─ parse_history() → Turn objects
                      ├─ connect inspector (if BUN_INSPECT available)
                      └─ emit SessionEvent::Connected → fan-out
                           └─ TelegramBackend: createForumTopic("📎 myproject")

User exits `claude`
  │
  └─ Process exits, transcript goes stale (60s timeout)
       │
       └─ ClaudeDiscovery: scan() detects gone
            └─ DiscoveryEvent::SessionGone
                 └─ ClaudeService: disconnect_session()
                      └─ SessionEvent::Disconnected → fan-out
                           └─ TelegramBackend: deleteForumTopic(topic_id)
```

---

## Error Handling & Edge Cases

| Scenario | Behavior |
|----------|----------|
| **Daemon starts, sessions already running** | Enumerate sockets, connect, receive `hello` + `history`, fire `Connected` to all backends |
| **Daemon restarts mid-turn** | Reconnects, receives `hello` + `history` with partial state. Backends receive `Connected` with current history, handle gracefully (Telegram: new message for remainder). |
| **Pi session exits** | Extension emits `session_end`, removes socket. Session manager fires `Disconnected`. Each backend cleans up (Telegram: deletes topic). |
| **Pi session starts while daemon running** | `notify` watcher fires, session manager connects, fires `Connected`. |
| **Backend API call fails** | Backend logs at warn level, continues. Telegram: next edit carries accumulated text. |
| **Telegram 429 rate limit** | Outbox pauses for `Retry-After` duration. Other backends unaffected. |
| **User sends message to ended session** | Backend replies "Session has ended." (backend-specific response mechanism). |
| **Unauthorized user** | Backend ignores. Telegram: only `allowed_user_ids` interact. |
| **Long message** | Backend-specific splitting. Telegram: split at paragraph/fence boundaries at 3500 chars. |
| **Socket directory doesn't exist** | Both extension and daemon create it (`mkdir -p`). |
| **Config missing** | Print message directing user to `pup setup`. |
| **Extension loaded but no daemon** | Socket sits idle. Negligible overhead. |
| **One backend crashes** | Session manager logs the error. Other backends continue. The crashed backend's task is restarted after a delay. |
| **Multiple backends enabled** | All receive all events. Each renders independently. A user message from Telegram goes to the session; the Discord backend sees it as a `UserMessage` event and can render it too. |
| **Claude Code session without BUN_INSPECT** | Read-only mode. Backends display events but cannot inject messages. Backend replies "inspector not available" on send attempt. |
| **Claude Code inspector disconnects** | State transitions to `Lost`. Exponential backoff retry (2s → 30s). Notification emitted on reconnection. |
| **Claude Code transcript goes stale** | After 60s inactivity + dead process, session marked gone. 10-minute suppression prevents rediscovery loops. |
| **Claude Code process restarts same session** | New PID detected, old session replaced. Inspector URL re-discovered from new process environ. |
| **Multiple Claude Code transcripts for same PID** | Deduplication: only the most recently modified transcript is tracked per PID. |

---

## Security & Access Control

### Threat Model

Pup bridges local pi sessions to the internet (via Telegram). The threat model:

- **Trusted:** The local machine. Unix sockets are filesystem-protected. Anyone
  who can connect to `~/.pi/pup/*.sock` already has shell access.
- **Untrusted:** The internet-facing chat platform. Arbitrary users can message
  the bot.

### Access Controls

**Telegram `allowed_user_ids`:** The primary gate. The daemon ignores all
updates from users not in this list. This is checked in `poll_updates()` before
any processing occurs — unknown users never reach `IncomingMessage`.

```rust
fn is_allowed(&self, user_id: i64) -> bool {
    self.config.allowed_user_ids.contains(&user_id)
}
```

**Topics mode supergroup:** The bot only operates in the configured supergroup.
Messages from other chats are silently ignored.

**DM mode:** Only allowed user IDs can interact via DM. The bot does not respond
to `/start` from unknown users (no error either — silent ignore to avoid
information leakage).

### What Is NOT Protected

- **Message content is not encrypted** between Telegram and the bot. Telegram
  provides transport encryption, but Telegram servers see plaintext. Don't send
  secrets via the bot.
- **No per-session authorization.** Any allowed user can interact with any
  session. This is intentional for the single-user design — the `allowed_user_ids`
  list is the trust boundary.
- **The bot token is sensitive.** Anyone with the token can impersonate the bot.
  Stored in `~/.config/pup/config.toml` with `0600` permissions.

### Socket Directory Permissions

The extension creates `~/.pi/pup/` with mode `0700`. Socket files inherit this.
The daemon verifies directory permissions on startup and warns if they're too
open.

---

## Concurrency Model

The daemon is built on tokio. Each logical concern runs in its own task:

```
main()
  │
  ├─ config::load()
  ├─ tracing_setup::init()
  │
  ├─ SessionManager::run()                    [spawned task]
  │    │
  │    ├─ discovery_loop()                    [spawned task]
  │    │    └─ notify watcher + initial scan
  │    │
  │    ├─ per-session IPC reader              [spawned task per session]
  │    │    └─ IpcClient::recv() loop
  │    │
  │    ├─ per-backend event consumer          [spawned task per backend]
  │    │    └─ mpsc::Receiver<SessionEvent> loop
  │    │
  │    ├─ per-backend incoming poller         [spawned task per backend]
  │    │    └─ poll_incoming() → mpsc::Sender<IncomingMessage>
  │    │
  │    └─ main select! loop:
  │         ├─ IPC events (from per-session tasks via mpsc)
  │         ├─ IncomingMessages (from backends via mpsc)
  │         ├─ Discovery events (new/removed sockets)
  │         └─ Shutdown signal (SIGINT/SIGTERM)
  │
  └─ ClaudeService::run()                    [spawned task]
       │
       ├─ ClaudeDiscovery::run()             [spawned task]
       │    └─ scan() every 5s (proc + transcript)
       │
       └─ main select! loop:
            ├─ DiscoveryEvents (session appeared/gone/inspector)
            ├─ ClaudeCommands (inject message, cancel)
            ├─ Transcript poll timer (500ms)
            ├─ Inspector retry timer (5s)
            └─ Shutdown signal
```

### Task Communication

| Channel | Type | Direction |
|---------|------|-----------|
| IPC reader → session manager | `mpsc<(SessionId, IpcEvent)>` | Per-session task sends parsed events |
| Session manager → backend | `mpsc<SessionEvent>` | One sender per backend, session manager fans out |
| Backend → session manager | `mpsc<IncomingMessage>` | Shared sender, all backends write to same channel |
| Discovery → session manager | `mpsc<DiscoveryEvent>` | New socket / removed socket notifications |
| Claude discovery → Claude service | `mpsc<DiscoveryEvent>` | Session appeared / gone / inspector found |
| Daemon → Claude service | `mpsc<ClaudeCommand>` | Inject message / cancel |
| Claude service → backends | `mpsc<SessionEvent>` | Shared with pi session manager's event channel |

All channels are bounded. Back-pressure from a slow backend does not block other
backends or the IPC readers. If a backend's channel fills up, the session
manager logs a warning and drops the event (the backend is likely dead and will
be restarted).

### Telegram Backend Internal Tasks

The Telegram backend spawns its own internal tasks:

```
TelegramBackend
  ├─ update_poller                [spawned task]
  │    └─ getUpdates long-poll loop
  │
  └─ outbox_flusher               [spawned task]
       └─ BinaryHeap drain loop with rate limiting
```

The `update_poller` sends parsed updates to the backend via an internal channel.
The `outbox_flusher` consumes outbox operations and makes API calls at the
configured rate.

---

## State & Persistence

### What's Persisted

| Data | Where | Survives |
|------|-------|----------|
| Pi session history | `~/.pi/agent/sessions/.../*.jsonl` | Everything (pi manages this) |
| Claude Code transcripts | `~/.claude/projects/<slug>/*.jsonl` | Everything (Claude Code manages this) |
| Daemon config | `~/.config/pup/config.toml` | Daemon restarts |
| Telegram topic → session mapping | In-memory `HashMap` | **Nothing** — rebuilt on restart |
| DM attachment state | In-memory | **Nothing** — detached on restart |
| Outbox queue | In-memory | **Nothing** — pending messages lost on crash |
| Streaming accumulator | In-memory | **Nothing** — partial messages lost on crash |
| Claude session registry | In-memory `HashSet` | **Nothing** — rebuilt from discovery |
| Transcript watcher offsets | In-memory `u64` per session | **Nothing** — history reparsed on restart |
| Inspector connections | In-memory WebSocket | **Nothing** — reconnected on restart |

### What Happens on Daemon Restart

1. **Config reloaded.** Same as fresh start.
2. **Discovery runs.** All live sockets found, connected. Each connection gets
   `hello` + `history` from the extension.
3. **Backends receive `Connected` events.** Telegram creates new topics for each
   session (or re-creates them in topics mode). DM mode starts detached.
4. **In-progress streams are not resumed.** If a session was mid-stream when the
   daemon crashed, the backend sees it as a new connection with partial history.
   The next `message_end` or `agent_end` event arrives normally.
5. **No message deduplication.** The daemon doesn't track "what was already sent
   to Telegram." On restart, it doesn't replay history — it only forwards new
   events. The `history` in the `Connected` event is available for backends to
   render a catch-up summary if desired.

### Topic Lifecycle & Stale State

Topics are ephemeral. They map 1:1 to live sessions:

- Session appears → topic created
- Session disappears → topic deleted **immediately**
- Daemon restarts → old topics are orphaned (no way to reclaim them). The
  backend creates new topics for reconnected sessions.

To avoid orphaned topics accumulating, the backend can optionally clean up
topics it doesn't recognize on startup (behind a config flag, off by default —
destructive).

---

## Echo Suppression & Message Attribution

When a user sends a message from Telegram, it flows:

```
Telegram → daemon → IPC send command → extension → pi.sendUserMessage()
```

Pi then processes it and emits events including `input` (with
`source: "extension"`). The extension sees this `input` event.

**Problem:** Without suppression, the extension would forward this `input` event
back to all connected clients as a `user_message` event, causing the Telegram
backend to display "👤 *user typed:*" for a message the user just sent from
Telegram.

**Solution:** The extension tracks in-flight messages sent via IPC `send`
commands. When the corresponding `input` event fires with `source: "extension"`,
the extension:

1. Recognizes it as an echo (matches pending text)
2. Emits the event with an `echo: true` flag
3. Removes it from the pending set

The daemon's event fan-out checks this flag. The backend that originated the
message skips rendering it. Other backends (e.g., Discord) still render it as a
"user message from Telegram" so they have full context.

```typescript
// Extension-side echo tracking
const pendingSends = new Set<string>();

// On IPC send command:
pendingSends.add(normalizeText(message));

// On input event:
pi.on("input", (event, ctx) => {
  if (event.source === "extension") {
    const normalized = normalizeText(event.text);
    if (pendingSends.delete(normalized)) {
      // This is an echo — tag it
      broadcastEvent("user_message", { content: event.text, echo: true });
      return;
    }
  }
  broadcastEvent("user_message", { content: event.text, echo: false });
});
```

Messages typed in the pi TUI have `source: "interactive"` and are never
echoes — they always get forwarded to all backends.

---

## Connection Resilience

### IPC Connection (extension ↔ daemon)

**Extension side:** The Unix socket server is fire-and-forget. If a client
disconnects, the extension removes it from the broadcast list. No reconnection
logic needed — the daemon reconnects as a new client.

**Daemon side:** The `IpcClient` handles connection drops:

1. `recv()` returns `EOF` → session manager fires `Disconnected`.
2. The session manager removes the connection from its map.
3. If the socket file still exists (pi didn't exit, maybe the read failed),
   the discovery loop will re-detect it and reconnect on the next scan
   (within 1–2 seconds).
4. If the socket file was removed (pi exited), no reconnection — the session
   is gone.

**No exponential backoff for IPC.** These are local Unix sockets. If a
connection fails, it's either because pi exited (permanent) or because of a
transient filesystem issue (rare, next scan fixes it).

### Claude Code Inspector Connection

The inspector WebSocket connection has its own resilience model:

| Failure | Behavior |
|---------|----------|
| Initial connection fails | State → `Lost`, retry with exponential backoff (2s base) |
| Connection drops mid-session | State → `Lost`, next retry tick attempts reconnection |
| `BUN_INSPECT` not available | State stays `Unavailable`, session is read-only |
| Process restarts with new URL | Discovery detects new PID, resets to `Discovered` |

Backoff: 2s → 4s → 8s → 16s → 30s (capped). Retries run every 5 seconds via a
timer in the `ClaudeService` run loop.

### Telegram API Connection

The `update_poller` task handles Telegram API failures:

| Failure | Behavior |
|---------|----------|
| Network error (timeout, DNS, TCP reset) | Log at warn, retry after 5s |
| HTTP 429 (rate limited) | Respect `Retry-After` header, pause all API calls |
| HTTP 5xx (server error) | Retry after 5s, up to 3 times, then backoff to 30s |
| HTTP 401 (invalid token) | Log at error, shut down backend |
| HTTP 409 (conflict — another instance polling) | Log at error, shut down backend |

The outbox handles per-request failures independently — a failed `editMessage`
doesn't block `sendMessage` calls.

### Backend Crash Recovery

If a backend task panics or returns an error:

1. Session manager logs the error at `error` level.
2. Other backends continue unaffected.
3. The crashed backend is restarted after a delay: 1s, 2s, 4s, 8s, … capped
   at 60s (exponential backoff with jitter).
4. On restart, the backend receives `Connected` events for all currently-live
   sessions (same as daemon startup).
5. The backoff resets after 5 minutes of successful operation.

---

## Graceful Shutdown

Shutdown is triggered by SIGINT or SIGTERM. The ordering:

```
1. Session manager receives shutdown signal
2. Stop discovery loop (no new connections)
3. For each backend (in parallel):
   a. backend.shutdown()
      - Telegram: flush outbox (best-effort, 5s timeout)
      - Telegram topics mode: do NOT delete topics on shutdown
        (they'll be orphaned, but that's better than data loss
        if the daemon restarts quickly)
4. For each IPC connection:
   a. Drop the connection (extension sees client disconnect, not session_end)
5. Flush tracing (JSONL writer guard dropped)
6. Exit
```

**Topics on shutdown vs session disconnect:**

- Session exits (pi closes) → extension emits `session_end` → daemon deletes topic ✓
- Daemon shuts down gracefully → topics left alive (orphaned) — acceptable
- Daemon crashes → topics left alive (orphaned) — same outcome

This means after a daemon restart, there may be stale topics from the previous
run alongside new topics. The optional cleanup-on-startup config handles this.

---

## Testing Strategy

### Unit Tests

**`pup-ipc`:**
- Serialization round-trips for all `ClientMessage` / `ServerMessage` variants
- Newline-delimited framing edge cases (empty lines, partial writes, very long
  lines)
- `IpcClient` connect/recv/send with a mock Unix socket (tokio `UnixListener`
  in a temp dir)

**`pup-core`:**
- `SessionEvent` construction from raw IPC events
- Discovery: mock a directory with `.sock` and `.alias` files, verify detected
  sessions
- Fan-out: verify all backends receive events, verify ordering preserved
- Render: markdown → plain text transforms

**`pup-telegram`:**
- `render::to_telegram_html()` — markdown input → expected HTML output, covering
  bold, code, fences, links, headers
- `render::split_message()` — verify splits at paragraph boundaries, code fences
  closed/reopened, continuation headers correct
- `outbox::Outbox` — verify priority ordering (send > delete > edit), verify
  rate limiting pauses, verify 429 handling
- `streaming` — accumulator tests: deltas → expected edit intervals, fast
  completion (< 1.5s) → single edit
- `dm::parse_command()` — command parsing for all DM commands
- `topics::topic_name()` — name generation from session info

**`pup-claude`:**
- `transcript::parse_line()` — user text, assistant (with thinking + tool_use
  blocks), tool results, ignored entry types
- `transcript::TranscriptWatcher` — offset tracking, stale flush, event
  generation sequence
- `discovery::slug_to_path()` — project slug → filesystem path conversion

**`pup-daemon`:**
- Config parsing: valid TOML, missing fields, invalid values, env var
  interpolation
- Setup wizard: mock stdin/stdout, verify generated config

### Integration Tests

**Extension ↔ Daemon:**
- Start a real pi session with the extension loaded
- Connect the daemon's `IpcClient` to the socket
- Verify `hello` + `history` received
- Send a `send` command, verify pi processes it
- Verify streaming events flow through

**Claude Code transcript parsing** (`#[ignore]` — requires real files):
- Parse a real `.jsonl` transcript, verify entry counts
- Load full history via `parse_history()`, verify turn reconstruction
- Connect to a live inspector, verify `1+1` eval and message injection

**Daemon ↔ Telegram (mock):**
- Stand up a mock Telegram Bot API server (simple HTTP server that responds to
  `getUpdates`, `sendMessage`, etc.)
- Wire the Telegram backend to the mock
- Push session events through the backend, verify correct API calls made
- Simulate incoming Telegram messages, verify `IncomingMessage` produced

### End-to-End Tests

- Start pi with extension → start daemon with mock Telegram → send message
  from "Telegram" → verify pi receives it → verify response streams back to
  "Telegram"
- Session connect/disconnect lifecycle → verify topic create/delete
- Multiple sessions → verify correct routing

### Test Infrastructure

```
daemon/
├── tests/
│   ├── common/
│   │   ├── mod.rs
│   │   ├── mock_telegram.rs     # Mock Bot API HTTP server
│   │   ├── mock_ipc.rs          # Fake extension socket server
│   │   └── fixtures/            # Sample IPC event sequences
│   ├── ipc_integration.rs
│   ├── telegram_integration.rs
│   └── e2e.rs
```

---

## Build & Installation

### Building from Source

```bash
cd daemon
cargo build --release
# Binary at: target/release/pup
```

### Dependencies

- **Rust stable** (via `rust-toolchain.toml`)
- **mold** linker (optional, faster linking — falls back to default linker)
- **sccache** (optional, caches compilation)

### Installation

```bash
# 1. Install the extension
mkdir -p ~/.pi/agent/extensions/pup
cp extension/index.ts ~/.pi/agent/extensions/pup/

# 2. Install the daemon
cargo install --path daemon/crates/pup-daemon
# Or copy the binary:
cp daemon/target/release/pup ~/.local/bin/

# 3. Run setup
pup setup

# 4. Start the daemon
pup
```

### Running

```bash
# Foreground (default)
pup

# With JSONL tracing
PUP_TRACE_FILE=traces.jsonl pup

# Verbose console output
RUST_LOG=debug pup

# Setup wizard
pup setup
```

The daemon is designed to be long-running. Start it in a tmux/screen session or
as a systemd user service:

```ini
# ~/.config/systemd/user/pup.service
[Unit]
Description=pup — pi session bridge
After=network.target

[Service]
ExecStart=%h/.local/bin/pup
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info
Environment=PUP_TRACE_FILE=%h/.local/share/pup/traces.jsonl

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now pup
journalctl --user -u pup -f
```

---

## Open Questions / Future Work

1. **Image support.** Pi sessions can include images. Forward as platform-native
   photos/attachments. Would need a new `SessionEvent::Image` variant.

2. **Extension UI proxying.** When a pi extension triggers `ctx.ui.confirm()` or
   `ctx.ui.select()`, forward to chat platforms as interactive elements (Telegram
   inline keyboards, Discord buttons). Needs new protocol events + commands.

3. **Cost tracking.** Forward token usage / cost from `agent_end` events.
   Backend-specific rendering.

4. **Multi-user.** Current design is single-user. Multi-user would need per-user
   permissions per backend and potentially separate session visibility.

5. **Remote access.** Currently requires daemon on same machine (Unix sockets).
   TCP/WebSocket transport would enable remote access.

6. **Additional backends.** Discord (threads as sessions, similar to Telegram
   topics), Slack (threads), Signal (via signal-cli), Matrix.

7. **Voice messages.** Takopi supports voice transcription via Whisper. Could
   add as a backend-level feature.

8. **Backend-to-backend.** If both Telegram and Discord are connected, a message
   sent from Telegram shows up in Discord via the `UserMessage` event. This is
   essentially free with the fan-out architecture.

9. **Claude Code streaming deltas.** Currently, Claude Code responses arrive as
   complete `MessageEnd` events (no `MessageDelta`). This means no streaming
   edits in Telegram — the full response appears at once. Streaming could be
   added by watching the transcript file more aggressively (sub-second polling
   or `inotify`) and emitting deltas as partial assistant entries arrive.

10. **Claude Code `tool_name` in `ToolEnd`.** Transcript `tool_result` entries
    don't include the tool name, only the `tool_use_id`. The current workaround
    sets `tool_name` to empty string. This could be fixed by maintaining a
    lookup from `tool_use_id` → `tool_name` populated during `ToolStart`.

11. **Claude Code thinking block forwarding.** Assistant thinking blocks are
    parsed but currently discarded at the `convert_event()` boundary. Backends
    could optionally render thinking content (e.g., in verbose mode).

12. **Cross-platform process discovery.** Claude Code discovery currently uses
    `/proc/` (Linux-only). macOS support would need `sysctl` or `ps`-based
    scanning.
