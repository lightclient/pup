# Claude Code Session Integration Plan

## Overview

Integrate Claude Code sessions into pup via **transcript file tailing** and
**hooks for lifecycle events**, giving message-level (not token-level) access
to Claude Code sessions while the TUI runs interactively.

### Capabilities: pi vs Claude Code

| Capability | pi + pup extension | Claude Code |
|---|---|---|
| Assistant messages | Streaming deltas | Complete messages (all-at-once) |
| Thinking/reasoning | Streaming deltas | Complete blocks (all-at-once) |
| Tool calls | Start/update/end | Start (transcript) + end (tool_result) |
| User messages | Real-time with echo | Via transcript tailing (~1s lag) |
| Agent lifecycle | Real-time events | Via hooks (SessionStart, Stop) |
| Session name | Name change events | Not available |
| Send messages | Via IPC socket | **Not possible** |
| Cancel/abort | Via IPC socket | **Not possible** |

**Key constraint:** Claude Code sessions are **read-only**. Pup observes
but cannot send messages to or control a running Claude Code TUI.

---

## Architecture

    Claude Code TUI (interactive)
    |-- Writes: ~/.claude/projects/<proj>/<session>.jsonl
    |-- Fires hooks --> pup hook bridge socket
          |                        |
          | file writes            | hook invocations
          v                        v
    pup-daemon
    |-- ClaudeCodeDiscovery  -- finds active .jsonl files
    |-- TranscriptWatcher    -- tails .jsonl, emits SessionEvents
    |-- HookBridge           -- receives hook events via socket
    |-- SessionManager       -- handles both pi and CC sessions
    +-- Backends (unchanged) -- Telegram, etc.

---

## Phase 1: Transcript Parser (new `pup-claude` crate)

New crate at `crates/pup-claude/`.

### Claude Code transcript format

Each `.jsonl` at `~/.claude/projects/<proj>/<session-id>.jsonl` has these
entry types:

**`type: "user"`** -- User text messages and tool results.- User text: `message.content` is a string
- Tool results: `message.content` is an array containing objects with
  `type: "tool_result"`, `tool_use_id`, `content`, and `is_error`

**`type: "assistant"`** -- Streamed incrementally. Claude Code writes
**multiple entries per API message** as content blocks arrive. Each shares
the same `message.id` (Anthropic API message ID). Content blocks include:
- `{type: "text", text: "..."}` -- assistant text
- `{type: "thinking", thinking: "..."}` -- extended thinking
- `{type: "tool_use", id: "toolu_...", name: "Bash", input: {...}}` -- tool calls

Typical sequence for one API call:

    assistant  msg_01X  [text: "(no content)"]           <- placeholder
    assistant  msg_01X  [thinking: "Let me..."]          <- thinking appears
    assistant  msg_01X  [tool_use: {name: "Bash", ...}]  <- tool call
    user       -        [tool_result: {...}]              <- tool finished

**`type: "progress"`** -- Subagent/task activity. Ignore initially.

**`type: "file-history-snapshot"`** -- File backup metadata. Ignore.

### Key parsing rules

1. Each line is independent JSON
2. `type` field determines the entry variant
3. `message.id` groups assistant entries for the same API response
4. The **last** entry for a given `message.id` has the complete content
5. `stop_reason` is always `null` in observed data -- do NOT rely on it
6. A `user` entry following assistant entries signals the turn boundary
7. Metadata on each entry: `uuid`, `parentUuid`, `sessionId`, `timestamp`, `cwd`, `version`

### Data structures (Rust)

    pub enum TranscriptEntry {
        UserText { uuid, session_id, timestamp, content, cwd },
        ToolResult { uuid, session_id, timestamp, tool_use_id, content, is_error },
        AssistantMessage {
            uuid, session_id, timestamp,
            api_message_id,  // the Anthropic message.id
            model,
            text: String,           // joined text blocks
            thinking: String,       // joined thinking blocks
            tool_uses: Vec<ToolUseBlock>,
        },
        Progress { session_id },
        Ignored,
    }

    pub struct ToolUseBlock {
        pub id: String,       // "toolu_01..."
        pub name: String,     // "Bash", "Read", etc.
        pub input: serde_json::Value,
    }

    pub fn parse_line(line: &str) -> Result<TranscriptEntry>;

---

## Phase 2: Transcript Watcher

### Design

    pub struct TranscriptWatcher {
        session_id: String,
        path: PathBuf,
        offset: u64,                                // byte position in file
        seen_api_messages: HashMap<String, ApiMsgState>,
        seen_tool_starts: HashSet<String>,           // tool_use IDs emitted
        pending_message_id: Option<String>,          // awaiting MessageEnd
        last_activity: Instant,                      // for stale-flush timeout
    }

    struct ApiMsgState {
        text: String,
        thinking: String,
        tool_use_count: usize,
    }

### Polling

Poll every **500ms**. Not inotify because Claude Code writes at very high
frequency during streaming and we only need message-level granularity.

    impl TranscriptWatcher {
        pub fn poll(&mut self) -> Result<Vec<SessionEvent>> {
            let len = fs::metadata(&self.path)?.len();
            if len <= self.offset { return Ok(vec![]); }

            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(self.offset))?;
            let mut events = Vec::new();

            for line in BufReader::new(file).lines() {
                let line = line?;
                self.offset += line.len() as u64 + 1;
                if let Ok(entry) = parse_line(&line) {
                    events.extend(self.process_entry(entry));
                }
            }
            // Stale-flush: if no activity for 3s, emit MessageEnd for pending
            if self.pending_message_id.is_some()
                && self.last_activity.elapsed() > Duration::from_secs(3) {
                events.extend(self.flush_pending());
            }
            Ok(events)
        }
    }

### Entry -> SessionEvent mapping

**UserText:**
- Flush any pending assistant MessageEnd first
- Emit `SessionEvent::UserMessage { content, echo: false, source: Interactive }`

**AssistantMessage (new api_message_id):**
- Flush any pending MessageEnd for the previous message
- Emit `SessionEvent::MessageStart { message_id: api_message_id }`
- For each tool_use block: emit `SessionEvent::ToolStart { tool_name, args }`
- Record as pending (MessageEnd deferred until next entry signals completion)

**AssistantMessage (same api_message_id, new content):**
- For each NEW tool_use block (not previously seen): emit `ToolStart`
- Update tracked state

**ToolResult:**
- Flush pending MessageEnd: `SessionEvent::MessageEnd { content: full_text }`
- Emit `SessionEvent::ToolEnd { tool_call_id, tool_name, content, is_error }`

**Stale flush (3s timeout):**
- If `pending_message_id` and no new entries for 3 seconds:
  emit `SessionEvent::MessageEnd` -- catches the final assistant message
  in a turn where the agent stops (no subsequent tool_result to trigger flush)

### No streaming deltas

We **cannot** produce `MessageDelta` or `ThinkingDelta`. The backend sees:

    MessageStart -> (nothing) -> MessageEnd with full content

Telegram messages appear all-at-once. The turn tracker handles this -- it
just won't do progressive edits for Claude Code sessions.

---

## Phase 3: Hook Bridge

Hooks give real-time lifecycle events that supplement the transcript. They
fire immediately (not subject to file-write latency).

### Hook events to use

| Hook | Input fields | Pup mapping |
|---|---|---|
| SessionStart | session_id, cwd | Connected |
| Stop | stop_reason | AgentEnd |
| PreToolUse | tool_name, tool_input, tool_use_id | ToolStart (faster) |
| PostToolUse | tool_name, tool_input, response | ToolEnd (faster) |
| Notification | message, type | Notification |
| UserPromptSubmit | user_prompt, session_id | UserMessage + AgentStart |
| SessionEnd | session_id | Disconnected |

### Hook bridge socket

The daemon listens on `~/.pup/hooks.sock`. Hook scripts connect, write one
JSON line, and exit. Protocol:

    {"event":"session_start","session_id":"abc","cwd":"/root/project","ts":"..."}
    {"event":"tool_start","session_id":"abc","tool_name":"Bash","tool_use_id":"toolu_01...","args":{...}}
    {"event":"tool_end","session_id":"abc","tool_use_id":"toolu_01...","content":"...","is_error":false}
    {"event":"agent_end","session_id":"abc"}
    {"event":"user_prompt","session_id":"abc","content":"do the thing"}
    {"event":"notification","session_id":"abc","text":"..."}
    {"event":"session_end","session_id":"abc"}

### Hook scripts

A single generic bridge script handles all hook types. Installed to
`~/.claude/hooks/pup/bridge.sh`:

    #!/bin/bash
    SOCKET="${PUP_HOOK_SOCKET:-$HOME/.pup/hooks.sock}"
    [ -S "$SOCKET" ] || exit 0

    INPUT=$(cat)
    EVENT="${CLAUDE_HOOK_EVENT:-unknown}"
    SESSION_ID=$(echo "$INPUT" | jq -r '.session_id // empty')

    # Forward to pup daemon
    echo "{\"event\":\"$EVENT\",\"session_id\":\"$SESSION_ID\",\"data\":$INPUT}" \
        | socat - UNIX-CONNECT:"$SOCKET"

    exit 0  # never block the agent

### Claude Code hooks configuration

Installed to `~/.claude/settings.json` (or project-level `.claude/settings.json`):

    {
      "hooks": {
        "SessionStart": [{"hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "Stop": [{"hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "UserPromptSubmit": [{"hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "Notification": [{"hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}],
        "SessionEnd": [{"hooks": [{"type": "command",
            "command": "~/.claude/hooks/pup/bridge.sh", "timeout": 5}]}]
      }
    }

All hooks use `exit 0` -- they never block the agent. The timeout is 5s as
a safety net (the script should complete in <100ms).

### Deduplication: hooks vs transcript

Both the transcript watcher and hook bridge can emit events for the same
action (e.g., tool start). The session manager deduplicates:

1. Hook events have priority (lower latency)
2. When a hook-sourced `ToolStart` arrives, record the `tool_use_id`
3. When the transcript watcher later sees the same tool_use_id, skip it
4. If hooks are not configured, the transcript watcher is the sole source
5. Dedup window: 30 seconds (if a hook event arrived, suppress matching
   transcript event within that window)

---

## Phase 4: Session Discovery

### Finding active Claude Code sessions

Claude Code sessions live at `~/.claude/projects/<project-slug>/<uuid>.jsonl`.
The project slug is derived from the project path (e.g., `-root-myproject`).

Discovery strategy:

1. **Lockfile approach:** Check for `.lock` files next to `.jsonl` files.
   Claude Code may hold a lockfile while the session is active. If so, this
   is the most reliable signal.

2. **Modification time:** Watch for `.jsonl` files modified in the last 60s.
   Poll `~/.claude/projects/` recursively every 5 seconds. A file that
   stops being modified for >60s is considered inactive.

3. **Process check:** Look for running `claude` processes and match their
   session IDs to `.jsonl` files (from `/proc/<pid>/cmdline` or `ps`).

4. **Hook-based:** The `SessionStart` hook is the most reliable signal.
   When it fires, the daemon immediately knows a new session is active and
   which `.jsonl` file to watch. `SessionEnd` signals the session is done.

**Recommended:** Use hooks (option 4) as primary discovery, with modification
time (option 2) as fallback for sessions started before the daemon.

### Discovery state machine

    Idle
      |-- SessionStart hook fires --> Active (start TranscriptWatcher)
      |-- .jsonl modified recently --> Active (start TranscriptWatcher)

    Active
      |-- SessionEnd hook fires   --> Disconnecting (flush, emit Disconnected)
      |-- .jsonl not modified 60s --> Disconnecting (flush, emit Disconnected)
      |-- poll() returns events   --> (emit events, stay Active)

    Disconnecting
      |-- flush complete          --> Idle (cleanup)

---

## Phase 5: SessionManager Integration

### Unified session model

The existing `SessionManager` handles pi sessions via IPC sockets. Extend it
to also manage Claude Code sessions via transcript watchers + hook bridge.

Add a `SessionKind` enum to distinguish the two:

    enum SessionKind {
        Pi {
            cmd_tx: mpsc::Sender<ClientMessage>,  // can send commands
        },
        ClaudeCode {
            watcher: TranscriptWatcher,            // read-only
        },
    }

    struct SessionConnection {
        info: SessionInfo,
        kind: SessionKind,
    }

### Changes to SessionManager::run()

The main select loop gains two new arms:

    loop {
        tokio::select! {
            // ... existing: shutdown, discovery, ipc_rx, incoming_rx ...

            // NEW: Hook bridge events
            Some(hook_event) = hook_rx.recv() => {
                self.handle_hook_event(hook_event).await;
            }

            // NEW: Transcript watcher poll timer
            _ = transcript_poll_interval.tick() => {
                self.poll_transcript_watchers().await;
            }
        }
    }

The `poll_transcript_watchers()` method iterates all `ClaudeCode` sessions,
calls `watcher.poll()`, and fans out resulting `SessionEvent`s to backends.

### Handling incoming messages (send to CC sessions)

When a backend tries to send a message to a Claude Code session, the
`route_incoming()` method detects it's a `ClaudeCode` session and responds
with a notification:

    SessionEvent::Notification {
        session_id,
        text: "Cannot send messages to Claude Code sessions (read-only)".into(),
    }

This surfaces clearly in Telegram so the user understands the limitation.

### Connected event and initial history

When a Claude Code session is first discovered:

1. Parse the existing .jsonl file from the beginning to build history
2. Extract session metadata: `sessionId`, `cwd`, `model`, `version`
3. Build `SessionInfo` with reconstructed `Turn` history
4. Emit `SessionEvent::Connected { info }`
5. Start the `TranscriptWatcher` at the current file offset (skip already-parsed content)

History reconstruction walks the entries and groups them into turns:
- A `UserText` starts a new turn
- Subsequent `AssistantMessage` + `ToolResult` entries fill in the turn
- Same logic as the existing pi extension's `getHistory()` but in Rust

---

## Phase 6: Configuration

### Config additions

New section in `~/.config/pup/config.toml`:

    [claude_code]
    enabled = true
    # Where Claude Code stores sessions
    projects_dir = "~/.claude/projects"
    # Poll interval for transcript watching (ms)
    poll_interval_ms = 500
    # How long before an inactive session is considered dead (seconds)
    inactive_timeout_s = 60
    # Enable hook bridge socket
    hooks = true
    # Hook bridge socket path
    hooks_socket = "~/.pup/hooks.sock"

### Hook auto-installation

On daemon startup (if `claude_code.enabled` and `claude_code.hooks`):

1. Create `~/.claude/hooks/pup/bridge.sh` if it doesn't exist
2. Read `~/.claude/settings.json`
3. Merge pup's hook entries into the `hooks` section (additive, never
   overwrite existing hooks for the same event)
4. Write back `~/.claude/settings.json`

On daemon shutdown: optionally clean up (configurable). Default: leave hooks
in place so they're ready for the next daemon start.

---

## Phase 7: Telegram UX Adjustments

### Read-only indicator

Claude Code sessions should be visually distinct in Telegram:

- Topic title: use a different icon (e.g. "🔍" instead of "📎") to signal
  read-only status
- When user tries to send a message: reply with
  "This is a Claude Code session (read-only). Use the Claude Code TUI to
  interact with this session."
- Slash commands that require sending (/cancel, /name, /compact, etc.)
  return a notification explaining they're unavailable

### Message rendering

Since messages arrive complete (not streaming):
- The turn tracker can skip the progressive-edit flow entirely
- Post the full assistant message as a single Telegram message
- Tool calls still render normally (ToolStart/ToolEnd pairs)
- Thinking blocks render if verbose mode is on

### No cancel button

The inline cancel button in Telegram tool messages should be hidden for CC
sessions since we cannot send abort commands.

---

## Implementation Order

### Step 1: `pup-claude` crate with transcript parser
- Parse all four entry types
- Unit tests against real .jsonl data from `~/.claude/projects/`
- ~2 days

### Step 2: TranscriptWatcher with polling
- File tailing logic
- Entry-to-SessionEvent mapping with dedup state machine
- Stale-flush logic
- Unit tests with synthetic .jsonl files
- ~2 days

### Step 3: Session discovery (modification-time based)
- Scan `~/.claude/projects/` for recently-modified .jsonl files
- Start/stop TranscriptWatchers
- ~1 day

### Step 4: SessionManager integration
- Add `SessionKind` enum
- Wire transcript watchers into the select loop
- Handle Connected (with history reconstruction) and Disconnected
- Read-only message routing
- ~2 days

### Step 5: Hook bridge
- Unix socket listener in daemon
- Bridge shell script
- Hook auto-installation in ~/.claude/settings.json
- Deduplication logic (hook vs transcript)
- ~2 days

### Step 6: Config and Telegram UX
- Config additions
- Read-only topic indicators
- Message rendering for non-streaming content
- ~1 day

### Step 7: Testing and polish
- Integration testing with live Claude Code sessions
- Edge cases: session resume, compact, subagents
- Documentation
- ~2 days

**Total estimate: ~12 days**

---

## Edge Cases and Risks

### Transcript file rotation
Claude Code does not rotate .jsonl files -- they grow indefinitely per session.
A resumed session appends to the same file. No special handling needed.

### Concurrent file access
Claude Code writes; pup reads. Since we use seek-to-offset and only read
complete lines, there's no corruption risk. A partial line at the end of file
is handled by the line iterator (incomplete line is buffered until next poll).

### Subagent transcripts
Claude Code writes subagent transcripts to `<session>/subagents/agent-<id>.jsonl`.
Initially ignore these. Future work: watch subagent files and emit nested
ToolUpdate events.

### Session resume
When Claude Code resumes a session (`--resume` or `--continue`), it appends
to the existing .jsonl file. The watcher's offset handles this naturally --
it only processes new content appended after the watcher started.

### Multiple Claude Code instances
Multiple Claude Code TUIs can run simultaneously with different session IDs.
Each gets its own .jsonl file and its own TranscriptWatcher. The hook bridge
routes by `session_id`.

### Hook script not installed
If the user hasn't run pup setup or deleted the hook scripts, the transcript
watcher is the sole event source. This provides degraded but functional
coverage (higher latency, no SessionStart/SessionEnd events). The daemon
logs a warning at startup if hooks are configured but the bridge script is
missing.

### Large transcript files
For sessions with thousands of turns, initial history parsing could be slow.
Limit history reconstruction to the last N entries (configurable, default 100).
The file offset for the TranscriptWatcher starts at the current end of file,
so ongoing polling is always fast regardless of file size.

### socat dependency
The bridge.sh script uses `socat` to write to the Unix socket. If socat is
not available, fall back to a Python one-liner or a small compiled helper
binary shipped with pup. Alternatively, the bridge script could write to a
named pipe or a file-drop directory that the daemon watches.

---

## Files to create/modify

### New files
- `crates/pup-claude/Cargo.toml`
- `crates/pup-claude/src/lib.rs`
- `crates/pup-claude/src/transcript.rs` -- entry parsing
- `crates/pup-claude/src/watcher.rs` -- file tailing + event mapping
- `crates/pup-claude/src/discovery.rs` -- finding active sessions
- `crates/pup-claude/src/hooks.rs` -- hook bridge socket listener
- `assets/hooks/pup/bridge.sh` -- hook bridge script template

### Modified files
- `Cargo.toml` (workspace) -- add pup-claude member
- `crates/pup-daemon/Cargo.toml` -- depend on pup-claude
- `crates/pup-daemon/src/main.rs` -- start hook bridge + CC discovery
- `crates/pup-daemon/src/config.rs` -- add [claude_code] config section
- `crates/pup-core/src/session.rs` -- add SessionKind, transcript poll arm
- `crates/pup-core/src/types.rs` -- possibly add CC-specific fields to SessionInfo
- `crates/pup-telegram/src/lib.rs` -- read-only UX, hide cancel button for CC
