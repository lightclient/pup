# pup E2E Test Suite — Claude Code Sessions

End-to-end tests for pup's Claude Code integration (`pup-claude`). Each test
case is executed by an LLM agent. Tests use **real Claude Code TUI sessions**
with `BUN_INSPECT` enabled, not mocks.

These tests cover the `pup-claude` crate: transcript tailing (read path),
inspector injection (write path), discovery, and session lifecycle.

## Tools

| Tool | Purpose |
|------|---------|
| `tests/e2e/tg.py` | Telegram user client — send/read messages, list topics |
| Claude Code TUI | Real Claude Code sessions (run in tmux) |
| pup daemon | The system under test (run in tmux) |
| tmux | Run long-lived processes safely |

## Running `tg.py`

```bash
cd /root/pup/main && uv run tests/e2e/tg.py <command>
```

## Constants

- **Supergroup ID**: read from `~/.config/pup/config.toml` field
  `backends.telegram.topics.supergroup_id`
- **Bot token**: read from `~/.config/pup/config.toml` field
  `backends.telegram.bot_token`
- **Bot user ID**: call `curl -s https://api.telegram.org/bot<TOKEN>/getMe`
  to get the bot's numeric user ID

## Socket & tmux conventions

All tmux sessions use a private socket to avoid conflicts:

```bash
SOCKET_DIR=${TMPDIR:-/tmp}/claude-tmux-sockets
mkdir -p "$SOCKET_DIR"
SOCKET="$SOCKET_DIR/pup-e2e-claude.sock"
```

## Key concept: Claude Code sessions vs pi sessions

Claude Code sessions differ from pi sessions in several fundamental ways:

| Aspect | pi session | Claude Code session |
|--------|-----------|-------------------|
| Discovery | IPC socket in `~/.pi/pup/` | `/proc` scan + transcript file scan |
| Read path | Streaming IPC events | Transcript `.jsonl` polling (500ms) |
| Write path | IPC `send` command | `process.stdin.push()` via inspector |
| Message delivery | Streaming deltas | Complete messages (all-at-once) |
| Typing indicators | Real-time events | Inferred from transcript activity |
| Cancel/abort | IPC `abort` command | `\x1b` (Escape) via inspector |
| Session name | Via IPC events | Derived from cwd |
| Inspector requirement | None | `BUN_INSPECT` env var at launch |

## Starting Claude Code sessions in tmux

Claude Code sessions must be started with `BUN_INSPECT` to enable the
write path. Without it, sessions are read-only (transcript tailing only).

```bash
# Create a temp dir for the session to work in
WORK=$(mktemp -d)

# Start Claude Code with inspector enabled
tmux -S "$SOCKET" new-window -t e2e -n "cc-NAME"
tmux -S "$SOCKET" send-keys -t e2e:cc-NAME \
  "cd $WORK && BUN_INSPECT='ws://127.0.0.1:0/\$RANDOM' claude" Enter

# Wait for the TUI to start (trust prompt + welcome screen)
sleep 10

# Accept trust prompt if needed
tmux -S "$SOCKET" send-keys -t e2e:cc-NAME Enter
sleep 5
```

To send a prompt to Claude Code (via tmux, simulating keyboard):

```bash
tmux -S "$SOCKET" send-keys -t e2e:cc-NAME \
  "say hello world" Enter
```

To exit a Claude Code session:

```bash
tmux -S "$SOCKET" send-keys -t e2e:cc-NAME "/exit" Enter
sleep 2
```

## Starting pup in tmux

```bash
tmux -S "$SOCKET" new-session -d -s e2e -n pup
sleep 1
tmux -S "$SOCKET" send-keys -t e2e:pup \
  "cd /root/pup/main && RUST_LOG=info ./target/debug/pup --config /tmp/pup-e2e-config.toml" Enter
```

Wait for the daemon to start:

```bash
for i in $(seq 1 20); do
  sleep 1
  tmux -S "$SOCKET" capture-pane -p -t e2e:pup | grep -q "telegram backend started" && break
done
```

## Setup (before all tests)

1. Build pup: `cd /root/pup/main && cargo build -q 2>&1`
2. Verify `tg.py me` works
3. Start the tmux server and pup (see above)

## Teardown (after all tests)

1. Exit all Claude Code sessions
2. Wait a few seconds for cleanup
3. Stop pup (`Ctrl-C`)
4. Kill tmux: `tmux -S "$SOCKET" kill-server`
5. Verify no leftover topics besides "General"

---

## Test Cases

### Discovery & Connection

### CT01 — Topic created when Claude Code session discovered

**Steps:**
1. Start a Claude Code session in tmux with `BUN_INSPECT` enabled
2. Send a prompt to create transcript content:
   `tmux send-keys "say hello" Enter`
3. Wait up to 30s for pup to discover the session (check pup logs for
   `discovered Claude Code session`)
4. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- A topic appears for the Claude Code session
- The topic name is derived from the working directory (since Claude Code
  has no `/name` equivalent)
- Pup logs show `discovered Claude Code session` with the session ID
  and transcript path

---

### CT02 — Topic removed when Claude Code session exits

**Steps:**
1. Start a Claude Code session; wait for topic to appear
2. Exit the session: `tmux send-keys "/exit" Enter`
3. Wait up to 90s (60s inactive timeout + 30s grace period)

**Expected:**
- The topic is deleted after the inactive timeout
- Pup logs show `Claude Code session gone`

**Note:** Claude Code session cleanup relies on the transcript file
becoming stale (no writes for 60s) AND the process no longer running.
This is slower than pi's socket-based cleanup.

---

### CT03 — Read-only session without BUN_INSPECT

**Steps:**
1. Start a Claude Code session WITHOUT `BUN_INSPECT`:
   `tmux send-keys "cd $WORK && claude" Enter`
2. Wait for the session to start; send a prompt in the TUI
3. Wait for pup to discover the session (via transcript scanning)
4. List topics

**Expected:**
- A topic is created (discovery works via transcript file scanning)
- Pup posts a notification: `👁 Connected to Claude Code session
  (read-only — launch with BUN_INSPECT for bidirectional)`
- The topic shows Claude's responses (read path works via transcript)

---

### CT04 — Bidirectional session with BUN_INSPECT

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT` enabled
2. Wait for the session to start; send any prompt to generate activity
3. Wait for pup to discover and connect
4. List topics

**Expected:**
- A topic is created
- Pup posts a notification: `🔗 Connected to Claude Code session
  (bidirectional)`
- Pup connected to the inspector WebSocket (check logs for `inspector
  connected`)

---

### Message Injection (Write Path)

### CT05 — Telegram message injected into Claude Code TUI

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Note the topic ID
3. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say exactly PINEAPPLE"`
4. Wait up to 30s for the bot to post a response

**Expected:**
- The message appears in the Claude Code TUI input field and is submitted
- Claude processes it and responds
- `tg.py history SUPERGROUP TOPIC_ID` shows a bot response containing
  `PINEAPPLE`

---

### CT06 — Injected message clears existing input first

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Type some text in the TUI input field (but don't submit):
   `tmux send-keys "some partial text" `  (no Enter)
3. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say exactly MANGO"`
4. Wait for response

**Expected:**
- The injection sends `\x15` (Ctrl+U) first, clearing the partial input
- Then the Telegram message text is pushed and submitted
- Claude responds to the Telegram message, not the partial text
- Response contains `MANGO`

---

### CT07 — Multi-line message injection

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a multi-line message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "please echo these lines:\nline one\nline two\nline three"`
3. Wait for response

**Expected:**
- The multi-line text is pushed to stdin with `\n` characters intact
- Claude receives all three lines as part of the prompt
- The response references the multi-line content

---

### CT08 — Message with special characters injected correctly

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send messages with special characters:
   - `tg.py send SUPERGROUP TOPIC_ID "say: it's a \"test\" with 'quotes'"`
   - `tg.py send SUPERGROUP TOPIC_ID "say: $HOME && echo \n backslash-n"`
   - `tg.py send SUPERGROUP TOPIC_ID "say: 🤖 emoji test 中文"`
3. Wait for responses to each

**Expected:**
- All characters are preserved through the hex-encoding injection
- Quotes, dollar signs, backslashes, emoji, and CJK characters all
  appear in Claude's responses
- No injection failures or garbled text

---

### CT09 — Injection while Claude is busy (queued message)

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a long prompt via TUI:
   `tmux send-keys "write a detailed essay about programming" Enter`
3. While Claude is actively responding, send via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "after you finish, say COCONUT"`
4. Wait for both to complete (up to 90s)

**Expected:**
- The Telegram message text is pushed to stdin while Claude is busy
- It appears in the input field
- After the current turn finishes, the `\r` submit takes effect
- Claude processes the queued message
- `COCONUT` eventually appears in the topic

---

### CT10 — Injection when session has no inspector (graceful failure)

**Steps:**
1. Start a Claude Code session WITHOUT `BUN_INSPECT` (read-only)
2. Wait for topic; note the topic ID
3. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say hello"`
4. Wait 10s

**Expected:**
- Pup attempts to inject but has no inspector connection
- The bot posts a notification explaining injection is not available:
  something like "Cannot send messages — session is read-only"
- No crash
- The session continues in read-only mode

---

### Transcript Tailing (Read Path)

### CT11 — Assistant text response appears in topic

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Note the topic ID
3. Send a prompt via TUI: `tmux send-keys "say exactly BANANA" Enter`
4. Wait up to 30s
5. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- The bot posts a message containing `BANANA`
- The message appears as a complete text (not streaming deltas)
- Delivery latency is ≤ 4 seconds (3s stale timeout + 500ms poll)

---

### CT12 — Tool use visible in topic

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Note the topic ID
3. Send a prompt that triggers a tool call:
   `tg.py send SUPERGROUP TOPIC_ID "run: echo TOOL_TEST_123"`
4. Wait for response (up to 30s)
5. Read topic history

**Expected:**
- In verbose mode: the topic shows tool call information (tool name,
  possibly the command)
- The tool result appears (content of the echo command)
- Claude's final text response appears
- Tool events are derived from the transcript (tool_use blocks in
  assistant entries, tool_result blocks in user entries)

---

### CT13 — Multiple tool calls in sequence visible

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt triggering multiple tools:
   `tg.py send SUPERGROUP TOPIC_ID "run echo FIRST, then run echo SECOND, then summarize"`
3. Wait for response (up to 60s)
4. Read topic history

**Expected:**
- Both tool calls appear in order (FIRST before SECOND)
- Claude's summary response appears after the tool calls
- No events are lost or out of order

---

### CT14 — User message echo from TUI appears in topic

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Note the topic ID
3. Type a prompt directly in the TUI (not via Telegram):
   `tmux send-keys "say TYPED_IN_TUI" Enter`
4. Wait for response (up to 30s)
5. Read topic history

**Expected:**
- The user's message appears in the topic (echoed from the transcript)
- Claude's response appears afterward
- Messages typed in the TUI are visible from Telegram, maintaining a
  unified conversation view

---

### Typing Indicators

### CT15 — Typing indicator during Claude response

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Note the topic ID
3. Send a prompt that takes several seconds:
   `tg.py send SUPERGROUP TOPIC_ID "explain the concept of ownership in rust"`
4. While Claude is responding (within 2-5s), check for typing indicator

**Expected:**
- While Claude is actively writing to the transcript, the bot shows
  a typing indicator in the topic
- The typing indicator starts within ~500ms of the first transcript
  entry (the poll interval)
- The typing indicator stops within ~3s of the last transcript entry
  (the stale-flush timeout)

**How to verify:**
- Check pup logs for `send_chat_action` calls during the turn
- Visually observe the typing indicator in Telegram

---

### CT16 — AgentStart and AgentEnd from transcript activity

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say hello"`
3. Wait for response
4. Check pup logs

**Expected:**
- `AgentStart` is emitted when the first assistant transcript entry
  appears (within ~500ms of Claude starting)
- `AgentEnd` is emitted after the stale-flush timeout (3s after the
  last transcript entry)
- These events drive the typing indicator lifecycle

---

### CT17 — Multi-turn conversation typing indicators

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send three prompts in sequence (wait for each response):
   - `tg.py send SUPERGROUP TOPIC_ID "say one"`
   - Wait for response
   - `tg.py send SUPERGROUP TOPIC_ID "say two"`
   - Wait for response
   - `tg.py send SUPERGROUP TOPIC_ID "say three"`
   - Wait for response
3. Check pup logs

**Expected:**
- Each turn shows `AgentStart` → transcript activity → `AgentEnd`
- Typing indicators start and stop for each turn independently
- No typing indicator "leaks" from one turn to the next

---

### Session Lifecycle

### CT18 — History loaded when session has prior conversation

**Steps:**
1. Start a Claude Code session; send a prompt in the TUI: `"say GRAPE"`
2. Wait for the response
3. Stop pup (if running), then start pup
4. Wait for pup to discover the session and create a topic; note topic ID
5. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- The topic history includes the prior `GRAPE` conversation
- `parse_history()` reconstructed turns from the `.jsonl` file
- The model name appears in the session info

---

### CT19 — Session continues working after inspector reconnect

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a message via Telegram; verify it works
3. Stop pup, then restart pup
4. Wait for pup to rediscover the session
5. Send another message via Telegram

**Expected:**
- After pup restart, the inspector reconnects to the same WebSocket URL
- The session transitions: Lost → Connecting → Ready
- Message injection works after reconnection
- Pup logs show `inspector connected` on the second connection

---

### CT20 — Inspector backoff on connection failure

**Steps:**
1. Start a Claude Code session with a `BUN_INSPECT` URL
2. Kill the Claude Code process (but leave the transcript file)
3. Observe pup logs for inspector connection attempts

**Expected:**
- The first connection attempt fails immediately
- Subsequent attempts use exponential backoff (2s → 4s → 8s → ... → 30s max)
- Pup does not spin-loop on failed connections
- Check pup logs for `inspector connect failed` with increasing intervals

---

### CT21 — Multiple Claude Code sessions get separate topics

**Steps:**
1. Start Claude Code session A in `/tmp/work-a`:
   `BUN_INSPECT='ws://127.0.0.1:9230/$RANDOM' claude`
2. Start Claude Code session B in `/tmp/work-b`:
   `BUN_INSPECT='ws://127.0.0.1:9231/$RANDOM' claude`
3. Send prompts in both TUIs to create transcripts
4. Wait for pup to discover both
5. List topics

**Expected:**
- Two distinct topics exist, one per session
- Each topic's name reflects its working directory
- Sending a message to topic A routes to session A (and vice versa)
- No cross-talk between sessions

---

### Cancel / Escape

### CT22 — Cancel via Escape injection

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a long prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "write a very long essay about computing history"`
3. Wait 5s for streaming to start
4. Send `/cancel` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/cancel"`
5. Wait 10s

**Expected:**
- Pup injects `\x1b` (Escape) via `process.stdin.push()` to interrupt
  the current turn
- Claude Code's TUI handles Escape as a cancel/interrupt
- The agent stops generating
- No crash

**Note:** Claude Code's cancel behavior via Escape may differ from pi's
IPC abort. The exact behavior depends on Claude Code's Ink input handler.
The test verifies that the injection itself works — the cancel semantics
are Claude Code's responsibility.

---

### CT23 — Cancel with no active turn

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Session is idle (not processing)
3. Send `/cancel` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/cancel"`
4. Wait 5s

**Expected:**
- Pup injects `\x1b` (Escape) into stdin
- Claude Code's TUI receives it but has nothing to cancel
- No crash, no error
- The session remains functional — send a subsequent message and verify

---

### Concurrent Sessions (pi + Claude Code)

### CT24 — Pi and Claude Code sessions coexist

**Steps:**
1. Start a pi session, name it `e2e-ct24-pi`
2. Start a Claude Code session with `BUN_INSPECT`
3. Send a prompt in the Claude Code TUI to create a transcript
4. Wait for both topics to appear
5. Send a message to the pi topic:
   `tg.py send SUPERGROUP PI_TOPIC "say APPLE"`
6. Send a message to the Claude Code topic:
   `tg.py send SUPERGROUP CC_TOPIC "say ORANGE"`
7. Wait for both responses

**Expected:**
- Two distinct topics exist (one pi, one Claude Code)
- The pi topic responds via IPC (streaming deltas)
- The Claude Code topic responds via transcript tailing (complete messages)
- No cross-contamination: APPLE in pi topic, ORANGE in CC topic
- Both typing indicators work independently

---

### CT25 — Pi and Claude Code sessions in same working directory

**Steps:**
1. Start a pi session in `/tmp/shared-work`, name it `e2e-ct25-pi`
2. Start a Claude Code session in `/tmp/shared-work` with `BUN_INSPECT`
3. Wait for both to be discovered

**Expected:**
- Two separate topics are created (different session types, different
  discovery paths)
- They don't interfere with each other
- Messages route to the correct session
- The pi session uses the IPC socket; the Claude Code session uses
  transcript + inspector

---

### Edge Cases

### CT26 — Very long Claude Code response

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt producing a very long response:
   `tg.py send SUPERGROUP TOPIC_ID "list the first 200 prime numbers"`
3. Wait for completion (up to 120s)
4. Read topic history

**Expected:**
- The full response is posted in the topic
- If it exceeds Telegram's 4096-char limit, it's split across messages
- No content is lost
- The transcript watcher correctly identifies the complete message via
  the stale-flush timeout

---

### CT27 — Claude Code session with thinking/extended thinking

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt that triggers extended thinking:
   `tg.py send SUPERGROUP TOPIC_ID "solve this step by step: what is 17 * 23?"`
3. Wait for response
4. Read topic history

**Expected:**
- The transcript watcher parses both `thinking` and `text` blocks
- In verbose mode, thinking content may be shown
- In non-verbose mode, only the final text appears
- The `MessageEnd` event includes both text and thinking content

---

### CT28 — Transcript file grows very large

**Steps:**
1. Start a Claude Code session; send many prompts to build a large
   transcript (or use a session with existing large history)
2. Restart pup
3. Wait for pup to discover and parse history

**Expected:**
- `parse_history()` processes the entire file without OOM or excessive
  delay
- The transcript watcher starts polling from the end of file (not
  re-reading the entire history on each poll)
- Topic creation includes history turns

---

### CT29 — Inspector WebSocket URL with special path

**Steps:**
1. Start Claude Code with a complex `BUN_INSPECT` path:
   `BUN_INSPECT='ws://127.0.0.1:9229/a-b-c/test/session' claude`
2. Wait for pup to discover and connect to the inspector

**Expected:**
- Discovery correctly extracts the full WebSocket URL from
  `/proc/<pid>/environ`
- The WebSocket connection succeeds with the complex path
- Injection works normally

---

### CT30 — Transcript watcher handles partial line at EOF

During active streaming, Claude Code may write a partial JSON line
that's not yet terminated with a newline.

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt that triggers a long streaming response
3. During streaming, pup's transcript watcher polls

**Expected:**
- The watcher reads only complete lines (ending with `\n`)
- Partial lines at EOF are buffered until the next poll
- No JSON parse errors from partial content
- Once the line is complete, it's parsed normally on the next poll
- No data loss — the offset advances only past complete lines

---

## Robustness Tests

### CR01 — Claude Code process killed mid-response

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a long prompt via Telegram
3. Wait for streaming to start
4. Kill the Claude Code process: `kill -9 $(pgrep -f 'claude')`
5. Wait 90s

**Expected:**
- The inspector WebSocket drops (connection lost)
- The transcript stops being written to
- After the inactive timeout (60s), pup marks the session as gone
- The topic is eventually deleted
- Pup logs show `inspector connect failed` and `Claude Code session gone`
- No crash

---

### CR02 — Pup restart picks up running Claude Code session

**Steps:**
1. Start a Claude Code session; send a prompt to create transcript
2. Wait for pup to discover it and create a topic
3. Stop pup (Ctrl-C)
4. Wait 3s
5. Start pup again
6. Wait for pup to rediscover the session

**Expected:**
- Pup discovers the session via transcript file scanning (recently
  modified `.jsonl` file)
- A topic is created
- If `BUN_INSPECT` is set, the inspector reconnects
- The session is fully functional

---

### CR03 — Pup handles multiple Claude Code versions

**Steps:**
1. Start a Claude Code session with version A (e.g., v2.1.34)
2. Wait for topic; verify it works
3. Exit the session
4. Start a new Claude Code session with a different version
5. Wait for pup to discover the new session

**Expected:**
- `process.stdin.push()` works identically across versions (it's a
  Node.js/Bun API, not version-specific)
- Transcript format is compatible across versions (same JSONL structure)
- No version-specific failures

---

### CR04 — Transcript file deleted while session is running

**Steps:**
1. Start a Claude Code session; wait for pup to discover it
2. Delete the transcript file:
   `rm ~/.claude/projects/-*/<session-id>.jsonl`
3. Send a prompt in the TUI (Claude Code creates a new entry)
4. Wait 10s

**Expected:**
- The transcript watcher detects the file is gone (stat fails)
- It handles the error gracefully (returns empty events)
- If Claude Code recreates the file, the watcher picks up from offset 0
- No crash

---

### CR05 — `/proc` scanning with many processes

**Steps:**
1. Ensure the system has many running processes (normal for a dev machine)
2. Start a Claude Code session with `BUN_INSPECT`
3. Measure how long discovery takes (check pup logs for timing)

**Expected:**
- Discovery completes within a few seconds despite many `/proc` entries
- Only processes with `claude` in their cmdline are inspected
- No excessive CPU or I/O from scanning

---

### CR06 — Concurrent injection and TUI typing

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Start typing in the TUI (but don't press Enter):
   `tmux send-keys "partial typed text" `
3. Simultaneously send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say INJECTED"`
4. Wait for response

**Expected:**
- The injection sends `\x15` (Ctrl+U) first, clearing the partial TUI
  input
- The Telegram message is then pushed and submitted
- Claude responds to the injected message
- The partial TUI text is lost (overwritten by the injection)
- This is acceptable — Telegram messages take priority over unsent TUI
  input

---

### CR07 — Inspector port conflict

**Steps:**
1. Start Claude Code session A with `BUN_INSPECT='ws://127.0.0.1:9229/a'`
2. Start Claude Code session B with `BUN_INSPECT='ws://127.0.0.1:9229/b'`
   (same port, different path)
3. Wait for pup to discover both

**Expected:**
- If Bun allows multiple WebSocket paths on the same port: both sessions
  connect and work independently
- If Bun rejects the second bind: only one session gets an inspector
  connection; the other falls back to read-only mode
- Pup handles both cases gracefully (check logs for which scenario
  occurred)
- Using port 0 (random port) avoids this entirely — document this as
  the recommended configuration

---

### CR08 — Stale transcript from previous session

**Steps:**
1. Start a Claude Code session; send some prompts; exit the session
2. Wait a few minutes (the transcript file exists but is stale)
3. Start pup (or restart it)
4. Check if pup tries to connect to the stale session

**Expected:**
- Discovery only considers transcript files modified within the last
  5 minutes as "active"
- Old transcript files are ignored
- No topic is created for the stale session
- No inspector connection is attempted

---

## Permission Prompt Tests

### CT31 — Injection while permission prompt is active

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt that triggers a permission prompt (e.g., a tool that
   needs approval)
3. While the permission prompt is showing, send a Telegram message:
   `tg.py send SUPERGROUP TOPIC_ID "say hello"`
4. Observe the TUI

**Expected:**
- The injected text goes to whatever input is focused (the permission
  prompt's input handler)
- The behavior depends on Claude Code's prompt type:
  - Yes/No prompt: characters may type into it (potentially auto-answering)
  - Text input: characters appear in the input field
- **This is a known edge case** — the test documents the behavior.
  Future work: detect permission prompts and either queue the injection
  or warn the user.

---

### CT32 — Follow-up prefix (>>) via Telegram

**Steps:**
1. Start a Claude Code session with `BUN_INSPECT`; wait for topic
2. Send a prompt to keep the agent busy:
   `tg.py send SUPERGROUP TOPIC_ID "count from 1 to 50, one per line"`
3. While Claude is working, send a follow-up:
   `tg.py send SUPERGROUP TOPIC_ID ">> after counting, say PAPAYA"`
4. Wait for both to complete

**Expected:**
- The first message is injected immediately
- The follow-up text (after stripping `>>`) is queued and injected
  after Claude finishes the first turn
- `PAPAYA` eventually appears in the topic

**Note:** Follow-up mode (`>>` → `mode: FollowUp`) may need special
handling for Claude Code sessions. Unlike pi's IPC which has a native
`FollowUp` mode, Claude Code's stdin injection just pushes text to the
input field. The `\r` submit happens immediately even if Claude is busy.
The behavior depends on whether the TUI input field is active during
streaming. This test documents the actual behavior.
