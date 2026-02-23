# Claude Code Message Injection via BUN_INSPECT

## Design Specification

### Overview

Inject user messages into a **running Claude Code TUI session** by leveraging
Bun's built-in JavaScript inspector protocol. No modifications to Claude Code
source are required — only an environment variable at launch time.

**Proven approach** (tested on Claude Code v2.1.34):

1. Launch Claude Code with `BUN_INSPECT` env var to expose a WebKit Inspector
   Protocol WebSocket
2. Connect from pup to the inspector WebSocket
3. Use `Runtime.evaluate` to call `process.stdin.push()` with message text
4. The TUI receives the input identically to keyboard entry
5. Send `\r` to submit — Claude processes it as a normal user message

This is **four lines of code** on the injection side. No debugger breakpoints,
no closure walking, no minified-name hunting.

```
Telegram ──► pup daemon ──► Inspector WebSocket ──► Runtime.evaluate
                                                        │
                                                   process.stdin.push("msg\r")
                                                        │
                                                   Ink input handler
                                                        │
                                                   TUI conversation loop
```

### Proof of Concept (verified working)

```python
# Connect to inspector
ws = websockets.connect("ws://127.0.0.1:9229/pup", max_size=100*1024*1024)

# Inject a message - this is ALL it takes
await ws.send(json.dumps({
    "id": 1,
    "method": "Runtime.evaluate",
    "params": {
        "expression": 'process.stdin.push(Buffer.from("what is 2+2?\\r"))',
        "returnByValue": True
    }
}))

# Result: TUI shows "what is 2+2?" in input, submits it, Claude responds "4"
```

---

### 1. Launching Claude Code

The user launches Claude Code with the inspector enabled:

```bash
BUN_INSPECT="ws://127.0.0.1:9229/pup" claude
```

- `BUN_INSPECT` — tells Bun to start the inspector WebSocket on the given
  address. Port 9229 is the conventional debugger port.
- The path segment (`/pup`) is the WebSocket endpoint path.

**Options for setup:**

**Option A: Shell alias (simplest)**
```bash
# Add to ~/.bashrc or ~/.zshrc
alias claude='BUN_INSPECT="ws://127.0.0.1:9229/$RANDOM" claude'
```

**Option B: Pup wrapper script**
```bash
#!/bin/bash
# ~/.local/bin/claude-pup
export BUN_INSPECT="ws://127.0.0.1:9229/$(uuidgen)"
exec claude "$@"
```

**Option C: Fixed port via pup config**
```toml
# ~/.config/pup/config.toml
[claude-code]
inspect_port = 9229
```

**Impact on Claude Code:** None. `BUN_INSPECT` is a Bun runtime feature.
Claude Code is unaware of it. The TUI works identically. Overhead is one
dormant WebSocket listener thread.

**Verified:** Inspector WebSocket binds and accepts connections on Bun
standalone binaries (tested with Claude Code v2.1.34).

---

### 2. Inspector Discovery

Pup needs to find the inspector WebSocket URL for active Claude Code sessions.

#### Primary: `/proc` environ scanning

1. Find Claude Code PIDs: `pgrep -f "claude"` or scan `/proc/*/cmdline`
2. Read `/proc/<pid>/environ` (null-delimited) to extract `BUN_INSPECT` value
3. The value IS the WebSocket URL: `ws://127.0.0.1:9229/pup`
4. Verify connectivity by attempting WebSocket handshake

#### Alternative: Fixed port convention

If using a fixed port (e.g., 9229), pup tries connecting to
`ws://127.0.0.1:9229/<path>` for known paths, or enumerates paths by
scanning `/proc/<pid>/environ` for the path component.

#### Alternative: BUN_INSPECT_NOTIFY

Bun supports `BUN_INSPECT_NOTIFY=unix:///path/to/sock` which sends the
actual bound URL to a Unix socket after startup. Pup could listen on a
well-known socket path. Useful when using port 0 (random port).

---

### 3. Inspector Connection

Pup connects via WebSocket using the WebKit Inspector Protocol.

**Protocol:** JSON messages over WebSocket. Request/response with integer IDs.

```json
→ {"id": 1, "method": "Runtime.evaluate", "params": {"expression": "1+1", "returnByValue": true}}
← {"id": 1, "result": {"result": {"type": "number", "value": 2}}}
```

**Connection sequence:**
1. WebSocket connect to `ws://127.0.0.1:<port>/<path>`
2. Send `Runtime.evaluate` with `"1+1"` to verify connection
3. Ready for injection

That's it. No `Debugger.enable` needed. No script parsing. No breakpoints.

**Key detail:** WebSocket max message size must be configured high (~100MB)
because the inspector may send large responses if `Debugger` domain is
enabled (10MB+ script sources). For `Runtime.evaluate` only, messages are
small.

---

### 4. Message Injection

Injecting a message is a single `Runtime.evaluate` call:

```json
{
    "id": 2,
    "method": "Runtime.evaluate",
    "params": {
        "expression": "process.stdin.push(Buffer.from(\"hello from telegram\\r\"))",
        "returnByValue": true
    }
}
```

#### How it works

`process.stdin` in the Claude Code TUI is a `ReadStream` (TTY). Ink (the
React-for-terminals framework) reads from it via a `readable` event listener.

`process.stdin.push(data)` writes bytes into the stream's internal read
buffer and emits a `readable` event. Ink's input handler reads the bytes
and processes them exactly as keyboard input:

- Printable characters → appended to the text input field
- `\r` (carriage return) → triggers submit (Enter key)
- `\x15` (Ctrl+U) → clears the input line
- `\x1b` (Escape) → cancel/escape actions

The message goes through the **full TUI input pipeline**: the Ink text input
component receives the characters, the submit handler fires, the
`UserPromptSubmit` hook runs, and the message enters the conversation queue.
It's indistinguishable from keyboard input.

#### Message escaping

The message text from Telegram may contain quotes, newlines, backslashes,
Unicode, etc. It must be safely embedded in the JS expression.

**Strategy:** Use `JSON.stringify` on the Rust side to produce a safe string,
then embed:

```rust
let escaped = serde_json::to_string(&message_text).unwrap();
let expr = format!(
    "process.stdin.push(Buffer.from(JSON.parse({}) + \"\\r\"))",
    serde_json::to_string(&escaped).unwrap()
);
```

This double-encodes: the outer JSON.stringify produces a safe JS literal,
the inner `JSON.parse` reconstructs the original text at runtime.

**Simpler alternative:** Encode the message as a hex string:

```rust
let hex: String = message_text.bytes()
    .chain(b"\r".iter().copied())
    .map(|b| format!("{:02x}", b))
    .collect();
let expr = format!(
    "process.stdin.push(Buffer.from('{}', 'hex'))",
    hex
);
```

This avoids all quoting issues entirely.

#### Multi-line messages

Telegram messages can be multi-line. In the Claude Code TUI, multi-line
input is entered by... just typing newlines (the input field supports them).
`\n` characters in the push buffer will appear as newlines in the input field.
Only `\r` triggers submit.

```
process.stdin.push(Buffer.from("line 1\nline 2\nline 3\r"))
```

#### Handling edge cases

**Claude is busy (processing a turn):**
The text appears in the input field. When the user can next submit (after the
current turn), the `\r` triggers submit. This matches the TUI behavior — users
can type ahead while Claude is working.

**Claude is waiting for user input:**
The text appears in the input field and `\r` immediately submits it. Claude
begins processing.

**Claude is in a permission prompt:**
The push goes to stdin. The permission prompt's input handler receives it.
The characters may type into whatever input is focused. This needs care —
see "Permission Prompts" below.

**Input field already has text:**
The pushed text is appended to whatever is already in the input field. To
replace, send `\x15` (Ctrl+U) first to clear the line:

```
process.stdin.push(Buffer.from("\x15my new message\r"))
```

**Multiple rapid injections:**
Each push goes to the stdin buffer. They're processed in order as the event
loop reads from the buffer. Safe for sequential messages, but avoid injecting
while Claude is still processing a previous injection.

---

### 5. Reading Responses

For reading Claude Code's responses back to Telegram, use **transcript
tailing** (as designed in `claude-code-session-integration-plan.md`):

1. Watch `~/.claude/projects/<project>/<session>.jsonl`
2. Poll at 500ms intervals
3. Parse assistant entries, group by `message.id`
4. Emit `MessageChunk` / `MessageEnd` events when content stabilizes (3s
   stale timeout — no new transcript entries for 3 seconds)
5. Map to pup protocol and forward to Telegram

The inspector is **only needed for the write path** (injection). The read
path is file-based and completely independent.

#### Session ID discovery

The transcript file path requires the session ID. Discover it via:
- `SessionStart` hook (fires when Claude starts, includes session ID)
- Scanning `~/.claude/projects/` for recently-modified `.jsonl` files
- Reading `/proc/<pid>/environ` for session-related env vars

---

### 6. Session Lifecycle

```
┌─────────────────────────────────────────────────────────┐
│                    pup daemon                           │
│                                                         │
│  ┌─────────────┐   ┌──────────────┐   ┌────────────┐  │
│  │  Discovery   │──►│  Inspector   │──►│  Injector   │  │
│  │  Service     │   │  Connector   │   │  Service    │  │
│  └──────┬──────┘   └──────────────┘   └────────────┘  │
│         │                                     │        │
│  ┌──────┴──────┐                ┌─────────────┴──┐    │
│  │  Transcript  │               │  Telegram      │    │
│  │  Watcher     │──────────────►│  Backend       │    │
│  └─────────────┘               └────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

#### State machine per session

```
                    ┌─────────┐
                    │ Unknown │
                    └────┬────┘
                         │ Inspector URL discovered (proc scan)
                         ▼
                    ┌──────────┐
              ┌────►│Connecting │
              │     └────┬─────┘
              │          │ WebSocket connected + Runtime.evaluate("1+1") OK
              │          ▼
              │     ┌─────────┐
              │     │  Ready  │ ◄── Can inject messages via stdin.push
              │     └────┬────┘
              │          │ WebSocket drops / process exits
              │          ▼
              │     ┌─────────┐
              └─────│  Lost   │ ── retry after backoff
                    └─────────┘
```

No "Capturing" state needed. Connection = ready.

#### Reconnection

If the inspector WebSocket drops:
1. Reconnect to the same URL
2. Verify with `Runtime.evaluate("1+1")` → should return `2`
3. Resume injection

If the Claude Code process restarts:
1. Discovery service detects new PID / new inspector URL
2. Connect to new inspector
3. Transcript watcher follows the new session file

---

### 7. Version Compatibility

**The `process.stdin.push()` approach is version-independent.** It relies on:

- `process.stdin` existing (Node.js/Bun standard API) ✅
- `process.stdin.push()` working on TTY streams (Node.js Readable API) ✅
- `\r` being interpreted as Enter by Ink ✅
- `BUN_INSPECT` being supported by Bun's runtime ✅

None of these depend on Claude Code's internal minified names, React
component structure, or queue implementation. The injection works at the
**stdin stream level**, below all of Claude Code's application logic.

**Tested on:** Claude Code v2.1.34 (Bun standalone binary, Linux x64)

**Expected to work on:** Any Claude Code version that:
- Runs on Bun (all current versions)
- Uses Ink for TUI (all current versions)
- Reads from process.stdin (fundamental requirement)

---

### 8. Security Considerations

**Inspector exposure:**
- Binds to `127.0.0.1` only — not accessible from the network
- The WebSocket path includes a user-chosen token
- Only processes running as the same user can connect

**Message injection:**
- Injected text goes through the full TUI input pipeline
- `UserPromptSubmit` hooks fire normally
- Permission prompts are respected (the injected text hits whatever input
  is focused, not bypassing prompts)
- The injection is literally typing — no elevated privileges

**Inspector left enabled accidentally:**
- Minimal attack surface: localhost only
- No worse than running `node --inspect` (common in development)
- An attacker with local access could `Runtime.evaluate` arbitrary JS in the
  Claude Code process. This is equivalent to the attacker being able to run
  code as the same user, which they already can.

**Mitigation:** The wrapper script can generate a random path per session,
making the WebSocket URL unguessable even from localhost:

```bash
export BUN_INSPECT="ws://127.0.0.1:9229/$(head -c 32 /dev/urandom | base64 | tr -dc a-zA-Z0-9)"
```

---

### 9. Permission Prompts & Special UI States

When Claude is showing a permission prompt (e.g., "Allow tool X?"), the TUI
has a different input handler focused. Injected stdin data goes to whatever
component is focused:

- **Permission prompt (Yes/No):** Typing `y` + `\r` would approve. Typing
  `n` + `\r` would deny. This is powerful but dangerous.
- **Multi-select menus:** Arrow keys + Enter navigate options.
- **File path input:** Text goes into the path field.

**Recommendation:** Pup should detect the current UI state before injecting.
Options:
1. **Transcript-based:** Check if the last transcript entry is a tool_use
   without a corresponding tool_result — indicates a permission prompt.
2. **Inspector-based:** `Runtime.evaluate` to check app state (e.g.,
   `queuedCommands` length, loading state).
3. **Simple guard:** Only inject when Claude is idle at the main input prompt.

---

### 10. Implementation Plan

#### pup-claude crate

New crate: `pup-claude` in the pup workspace.

```
pup-claude/
├── src/
│   ├── lib.rs
│   ├── discovery.rs       # Find Claude Code processes + inspector URLs
│   ├── inspector.rs       # WebSocket client, Runtime.evaluate wrapper
│   ├── injector.rs        # High-level inject(session, message) API
│   ├── transcript.rs      # JSONL transcript parser + watcher
│   └── session.rs         # Session state machine + lifecycle
├── Cargo.toml
└── tests/
    ├── transcript_test.rs
    └── inspector_test.rs  # Mock WebSocket server for testing
```

#### Dependencies

- `tokio-tungstenite` — WebSocket client for inspector connection
- `serde_json` — JSON-RPC message handling
- `notify` or polling — transcript file watching

#### Phases

**Phase 1: Proof of concept (done ✅)**
- Verified `BUN_INSPECT` works on Bun standalone binaries
- Verified `Runtime.evaluate` works
- Verified `process.stdin.push()` injects messages into TUI
- Verified Claude processes injected messages normally

**Phase 2: Transcript tailing (read path)**
- Parse `.jsonl` format
- Watch for new entries via polling
- Map to pup protocol events (`MessageChunk`, `MessageEnd`)
- Wire into `pup-daemon` session discovery

**Phase 3: Inspector client (write path)**
- WebSocket client for WebKit Inspector Protocol
- `Runtime.evaluate` wrapper with message escaping
- Connection lifecycle (connect, verify, reconnect)
- Session state machine

**Phase 4: Integration**
- Combine read (transcript) and write (inspector) paths
- Wire into pup-daemon as a new session type alongside Pi
- Telegram backend routing: detect Claude Code sessions, inject messages
- Status reporting ("🔗 Connected to Claude Code session")

**Phase 5: Polish**
- `claude-pup` wrapper script with auto-setup
- Permission prompt detection and safe injection guards
- Clear input field before injection (`\x15` prefix)
- Multi-line message handling
- Graceful degradation when inspector unavailable (read-only mode)

---

### 11. Resolved Questions

1. **Does `BUN_INSPECT` work with Bun standalone binaries?**
   ✅ **Yes.** Verified on Claude Code v2.1.34. Port binds, WebSocket accepts
   connections, `Runtime.evaluate` works.

2. **Can `process.stdin.push()` inject text into the TUI?**
   ✅ **Yes.** Text appears in the input field. `\r` triggers submit. Claude
   processes the message normally. Conversation stays linear.

3. **Is the Debugger domain needed?**
   ❌ **No.** `Runtime.evaluate` alone is sufficient. No breakpoints, no
   script source parsing, no function capture needed.

4. **Are minified internal names a concern?**
   ❌ **No.** The approach uses `process.stdin.push()` which is a stable
   Node.js/Bun API. It doesn't depend on any Claude Code internals.

### 12. Open Questions

1. **Inspector overhead at scale.** Is there measurable performance impact
   from `BUN_INSPECT` being enabled? Likely negligible (dormant when no
   client connected), but should be measured.

2. **Concurrent inspector clients.** Can pup and VS Code connect
   simultaneously? Most inspector implementations are single-client.

3. **`BUN_INSPECT_NOTIFY` format.** What exact message does Bun send to the
   notify socket? Needed for the random-port discovery path.

4. **Permission prompt injection safety.** How to reliably detect when the
   TUI is in a permission prompt vs. the main input. Transcript-based
   detection is likely sufficient.

5. **Windows/macOS compatibility.** `process.stdin.push()` should work
   identically, but `/proc` scanning for discovery is Linux-only. macOS
   needs `sysctl` or `lsof` based discovery.
