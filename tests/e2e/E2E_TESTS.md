# pup E2E Test Suite

End-to-end tests for pup. Each test case is executed by an LLM agent using
the tools described below. Tests use **real pi sessions** with the real
extension, not mocks.

## Tools

| Tool | Purpose |
|------|---------|
| `tests/e2e/tg.py` | Telegram user client — send/read messages, list topics |
| `pi` | Real pi coding agent sessions (run in tmux) |
| pup daemon | The system under test (run in tmux) |
| tmux | Run long-lived processes safely |

## Running `tg.py`

```bash
cd /root/handoff/main && uv run tests/e2e/tg.py <command>
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
SOCKET="$SOCKET_DIR/pup-e2e.sock"
```

## Isolated socket directory

To prevent the test pi sessions from mixing with your personal pi sessions,
the extension supports `PUP_SOCKET_DIR` as an env override for its socket
directory. Set this when starting test pi sessions AND in the pup config:

```bash
PUP_SOCKET_DIR=/tmp/pup-e2e-sockets
export PUP_SOCKET_DIR
mkdir -p "$PUP_SOCKET_DIR"
```

The pup `--config` flag should point to a test config with matching
`socket_dir`:

```toml
[pup]
socket_dir = "/tmp/pup-e2e-sockets"
```

## Key concept: topics map to pi processes, not sessions

A Telegram topic is tied to a running **pi process**, not a pi session.
The extension generates a stable `INSTANCE_ID` (UUID) when it loads.
This ID persists across `/new` and `/compact` because the pi process
keeps running. The socket filename is `<INSTANCE_ID>.sock`.

- **Pi starts** → topic created
- **`/new` or `/compact`** → topic stays, "🔄 Session reset" posted
- **Pi exits** → topic deleted

## Starting pi sessions in tmux

Pi sessions must be started in tmux windows so they remain interactive and
safe. Each pi session gets its own tmux window in a shared tmux server.
Give each session a temp working directory so it doesn't interfere with
anything.

```bash
# Create a temp dir for the session to work in
WORK=$(mktemp -d)

# Start pi in a new tmux window (PUP_SOCKET_DIR isolates from other sessions)
tmux -S "$SOCKET" new-window -t e2e -n "pi-NAME"
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME \
  "cd $WORK && PUP_SOCKET_DIR=$PUP_SOCKET_DIR pi --dangerously-skip-permissions" Enter
```

After pi starts, name the session with `/name`:

```bash
sleep 3
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME "/name NAME" Enter
```

To send a prompt to pi (making it generate activity):

```bash
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME \
  "say hello world" Enter
```

To exit a pi session (Ctrl-D, not `/exit` — the agent intercepts `/exit`
as a user message instead of treating it as a pi slash command):

```bash
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME C-c   # interrupt if mid-turn
sleep 0.5
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME C-d   # EOF → pi exits
sleep 2
tmux -S "$SOCKET" send-keys -t e2e:pi-NAME "exit" Enter  # close the shell
```

## Starting pup in tmux

```bash
tmux -S "$SOCKET" new-session -d -s e2e -n pup
sleep 1
tmux -S "$SOCKET" send-keys -t e2e:pup \
  "cd /root/handoff/main && RUST_LOG=info ./target/debug/pup --config /tmp/pup-e2e-config.toml" Enter
```

Wait for `telegram backend started` in the output before proceeding:

```bash
# Poll for startup
for i in $(seq 1 20); do
  sleep 1
  tmux -S "$SOCKET" capture-pane -p -t e2e:pup | grep -q "telegram backend started" && break
done
```

## Setup (before all tests)

1. Build pup: `cd /root/handoff/main && cargo build -q 2>&1`
2. Verify `tg.py me` works
3. Start the tmux server and pup (see above)

## Teardown (after all tests)

1. Exit all pi sessions (Ctrl-D)
2. Wait a few seconds for topic cleanup
3. Stop pup (`Ctrl-C`)
4. Kill tmux: `tmux -S "$SOCKET" kill-server`
5. Verify no leftover topics besides "General"

## Automated runner

```bash
cd /root/handoff/main
bash tests/e2e/run_e2e.sh           # run all tests
bash tests/e2e/run_e2e.sh "t01 t03" # run specific tests
```

The runner handles setup/teardown automatically: starts pup with an
isolated `PUP_SOCKET_DIR`, runs the requested tests, and cleans up.

---

## Test Cases

### T01 — Topic created when pi session starts

**Steps:**
1. Start a pi session in tmux, name it `e2e-t01`
2. Wait up to 15s for pup to discover it (check pup logs for
   `session connected`)
3. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- A topic containing `e2e-t01` appears in the topic list

**Cleanup:**
- `/exit` the pi session
- Wait for the topic to be deleted

---

### T02 — Topic deleted when pi session exits

**Steps:**
1. Start a pi session, name it `e2e-t02`
2. Wait for its topic to appear
3. `/exit` the pi session
4. Wait up to 15s

**Expected:**
- The `e2e-t02` topic no longer appears in `tg.py topics`

**Note:** Topic deletion uses a 30-second grace period. When the pi process
exits and the IPC connection breaks, the topic is scheduled for deletion
but not immediately removed. After 30 seconds with no replacement session
in the same working directory, the topic is deleted. The test must wait
long enough for the grace period to expire.

---

### T03 — User message forwarded to pi session

**Steps:**
1. Start a pi session, name it `e2e-t03`
2. Wait for its topic to appear; note the topic ID
3. Send a message via `tg.py send SUPERGROUP TOPIC_ID "say exactly PINEAPPLE"`
4. Wait up to 30s for the bot to post a response in the topic

**Expected:**
- The pi session receives the message and starts processing
- `tg.py history SUPERGROUP TOPIC_ID` shows a bot response containing
  `PINEAPPLE`

---

### T04 — Multiple parallel sessions get separate topics

**Steps:**
1. Start pi session A, name it `e2e-t04-alpha`
2. Start pi session B, name it `e2e-t04-beta`
3. Start pi session C, name it `e2e-t04-gamma`
4. Wait for all three topics to appear

**Expected:**
- `tg.py topics SUPERGROUP` shows three topics, one for each session
- Each topic has the correct session name

**Additional routing check:**
5. Send `"ping alpha"` to alpha's topic
6. Send `"ping beta"` to beta's topic
7. Wait for responses in each topic

**Expected:**
- Each topic gets a response only from its own session
- No cross-talk between sessions

**Cleanup:**
- `/exit` all three sessions
- Verify all three topics are deleted

---

### T05 — Session rename updates topic title

**Steps:**
1. Start a pi session, name it `e2e-t05-before`
2. Wait for topic `e2e-t05-before` to appear
3. Rename the session: send `/name e2e-t05-after` in the pi TUI
4. Wait up to 15s

**Expected:**
- `tg.py topics SUPERGROUP` shows a topic with `e2e-t05-after`
- No topic with `e2e-t05-before` exists

---

### T06 — History posted when session has prior conversation

**Steps:**
1. Start a pi session, name it `e2e-t06`
2. Send a prompt in the pi TUI (not via Telegram): `"say MANGO"`
3. Wait for the agent to finish responding
4. Now restart pup (Ctrl-C, then start again)
5. Wait for pup to reconnect and recreate the topic; note the topic ID

**Expected:**
- `tg.py history SUPERGROUP TOPIC_ID` shows history messages containing
  `MANGO` — the prior conversation was posted on topic creation

---

### T07 — Follow-up mode with >> prefix

**Steps:**
1. Start a pi session, name it `e2e-t07`
2. Wait for topic; note the topic ID
3. Send a prompt to make the agent busy: via the pi TUI, type a prompt that
   will take a while (e.g. `"count from 1 to 100 slowly, one per line"`)
4. While the agent is streaming, send `">> after you finish, say COCONUT"`
   via `tg.py send SUPERGROUP TOPIC_ID ">> after you finish, say COCONUT"`
5. Wait for the agent to finish both the original and follow-up

**Expected:**
- The follow-up message is queued (not interrupting)
- Eventually `tg.py history SUPERGROUP TOPIC_ID` shows a response containing
  `COCONUT`

---

### T08 — /cancel aborts the agent

**Steps:**
1. Start a pi session, name it `e2e-t08`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "write a very long essay about the history of computing"`
4. Wait 3-5s for streaming to start
5. Send `/cancel` via `tg.py send SUPERGROUP TOPIC_ID "/cancel"`

**Expected:**
- The agent stops generating
- Check pup logs for evidence of the abort being sent

---

### T09 — Tool calls visible in verbose mode

Pup is configured with `verbose = true`.

**Steps:**
1. Start a pi session, name it `e2e-t09`
2. Wait for topic; note the topic ID
3. Send a prompt that triggers tool use:
   `tg.py send SUPERGROUP TOPIC_ID "run: echo E2E_TOOL_TEST"`
4. Wait for the response

**Expected:**
- `tg.py history SUPERGROUP TOPIC_ID` shows messages containing both:
  - A tool call message mentioning `bash` or `Bash` and `E2E_TOOL_TEST`
  - The agent's text response

---

### T09b — /verbose on mid-turn takes effect immediately

Toggling verbose mode while the agent is actively streaming must take
effect for the current turn — not just the next one.

**Background:** `set_verbose` updates both the `TurnTracker` default and
all active `TurnState` entries. Before the fix, it only set the tracker
default, so each turn's `verbose` flag was frozen at `start_turn()` time
and never updated.

**Steps:**
1. Start a pi session, name it `e2e-t09b`
2. Wait for topic; note the topic ID
3. Ensure verbose is off: send `/verbose off` in the topic
4. Send a prompt that triggers tool use and takes a few seconds:
   `tg.py send SUPERGROUP TOPIC_ID "run these commands one by one: echo TOOL_A, echo TOOL_B, echo TOOL_C, then summarize"`
5. Wait 2–3s for the agent to start working (check that an agent turn
   message appears in the topic)
6. While the agent is mid-turn, toggle verbose on:
   `tg.py send SUPERGROUP TOPIC_ID "/verbose on"`
7. Wait for the agent to finish (up to 60s)
8. Read the topic history:
   `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- The bot confirms "Verbose mode: **on**" in the topic
- Tool calls that happen **after** the toggle are visible in the bot's
  streaming message (e.g., 🔧 **Bash** with the echo commands)
- Tool calls that happened **before** the toggle may not be shown
  (they were processed in non-verbose mode and not tracked) — this is
  acceptable
- The final message includes the agent's text response
- Verbose mode persists for subsequent turns — send another prompt
  that triggers a tool call and verify tools are shown from the start

**To verify the fix worked (not the old broken behavior):**
- In the old code, toggling verbose mid-turn had zero effect: the turn
  kept its snapshot from `start_turn()`, and all tool/thinking handlers
  checked the per-turn flag. The streaming message would show only the
  final text with no tool indicators.
- With the fix, `set_verbose` propagates to active turns immediately,
  so subsequent `tool_start`/`thinking_delta` events are rendered.

---

### T10 — Typing indicator shown during agent turn

**Steps:**
1. Start a pi session, name it `e2e-t10`
2. Wait for topic; note the topic ID
3. Send a prompt that will take a few seconds:
   `tg.py send SUPERGROUP TOPIC_ID "write a haiku about rust programming"`
4. Immediately (within 1-2s) check the chat action status by reading
   messages — the bot should show "typing…" in the Telegram UI

**Expected:**
- While the agent is working, the bot shows a "typing…" indicator in the
  topic (Telegram shows this as `<bot> is typing...` under the chat header)
- The typing indicator is refreshed every ~4 seconds (the spawn loop in
  `turn_tracker.rs`)
- After the agent finishes (`agent_end`), the typing indicator stops

**How to verify:**
- Use `tg.py wait` to wait for the response — if it arrives, the turn ended
- Check pup logs for `send_chat_action` calls during the turn
- Visually: open Telegram on your phone and watch for the typing bubble

---

### T11 — Cancel button present during streaming, removed after

**Steps:**
1. Start a pi session, name it `e2e-t11`
2. Wait for topic; note the topic ID
3. Send a prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "explain the concept of ownership in rust in detail"`
4. While the agent is streaming (within 5-10s), read the topic messages:
   `tg.py history SUPERGROUP TOPIC_ID`
5. Note the bot's response message — it should have an inline keyboard
   with a "✖ Cancel" button (visible in Telegram UI)
6. Wait for the agent to finish
7. Read topic messages again

**Expected:**
- During streaming: the bot's message has a `✖ Cancel` inline keyboard
  button attached (callback_data = `cancel:<session_id>`)
- After the agent finishes (`end_turn`): the cancel button is removed
  (the final edit uses `empty_keyboard()`)
- The final message has no inline keyboard

**How to verify programmatically:**
- `tg.py history` includes `reply_markup` on messages when present. During
  streaming, the bot's message should have:
  ```json
  "reply_markup": {
    "type": "ReplyInlineMarkup",
    "buttons": [{"text": "✖ Cancel", "callback_data": "cancel:<session_id>"}]
  }
  ```
- After the turn ends, the message should have either no `reply_markup`
  field, or an empty buttons list

---

### T12 — Message ordering is correct

Agent messages (tool calls, text responses) must appear in the topic in the
same order the extension emits them, and must be visible promptly.

**Steps:**
1. Start a pi session, name it `e2e-t12`
2. Wait for topic; note the topic ID
3. Send a prompt that triggers multiple tool calls and a final response:
   `tg.py send SUPERGROUP TOPIC_ID "run these three commands in order: echo FIRST, echo SECOND, echo THIRD, then summarize what you did"`
4. Wait for the agent to complete (up to 60s)
5. Read the full topic history:
   `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- Messages appear in chronological order:
  1. The user's message
  2. Bot messages with tool calls / agent responses in the order they were
     executed (FIRST before SECOND before THIRD)
  3. The final summary response
- No messages are out of order or interleaved incorrectly
- Each agent turn is a single message (edited in place), not scattered
  across multiple messages

---

### T13 — Response available immediately after message_end

The final content should be posted promptly when the agent finishes — not
delayed by the edit throttle interval.

**Steps:**
1. Start a pi session, name it `e2e-t13`
2. Wait for topic; note the topic ID
3. Send a prompt that produces a short, fast response:
   `tg.py send SUPERGROUP TOPIC_ID "reply with only the word BANANA"`
4. Start a timer
5. Wait for the bot response:
   `tg.py wait SUPERGROUP --topic TOPIC_ID --contains BANANA --timeout 30`
6. Note how long it took

**Expected:**
- The response appears within a few seconds of the agent finishing (not
  delayed by the 1.5s edit throttle — `message_end_with_content` calls
  `flush(outbox, 0)` which bypasses the throttle)
- The response should be visible within ~5s of sending the prompt (for a
  trivial response like this)

---

### T14 — Concurrent prompts to different sessions respond independently

**Steps:**
1. Start pi session A, name it `e2e-t14-a`
2. Start pi session B, name it `e2e-t14-b`
3. Wait for both topics; note their topic IDs
4. Send prompts to both at roughly the same time:
   `tg.py send SUPERGROUP TOPIC_A "reply with only APPLE"`
   `tg.py send SUPERGROUP TOPIC_B "reply with only ORANGE"`
5. Wait for both responses (up to 30s each)

**Expected:**
- Session A's topic gets a response containing `APPLE`
- Session B's topic gets a response containing `ORANGE`
- No cross-contamination — APPLE doesn't appear in B's topic or vice versa
- Both responses arrive — one doesn't block the other
- Typing indicators show in both topics simultaneously

---

## Session Reset Tests

These tests verify that `/new` and `/compact` in the pi TUI do **not**
create new topics. A topic is tied to a running pi process, not a pi session.

### T15 — /new preserves the topic

**Steps:**
1. Start a pi session, name it `e2e-t15`
2. Wait for topic to appear; note the topic ID
3. Send a prompt in the pi TUI: `"say BEFORE_RESET"`
4. Wait for the response to appear in the topic
5. Send `/new` in the pi TUI:
   `tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-t15 "/new" Enter`
6. Wait 5s
7. List topics: `tg.py topics SUPERGROUP`
8. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- The topic list still contains exactly one topic for this session (same
  topic ID as step 2 — no deletion and recreation)
- No second topic was created
- The topic history shows a `🔄 Session reset` message from the bot
- The earlier `BEFORE_RESET` conversation is still visible in the topic
  history (it was posted before the reset)

---

### T16 — Messages work after /new

**Steps:**
1. Start a pi session, name it `e2e-t16`
2. Wait for topic to appear; note the topic ID
3. Send `/new` in the pi TUI
4. Wait 5s for the reset to propagate
5. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say exactly AFTER_RESET"`
6. Wait for the response:
   `tg.py wait SUPERGROUP --topic TOPIC_ID --contains AFTER_RESET --timeout 30`

**Expected:**
- The pi session receives the Telegram message and responds
- The response appears in the same topic
- The topic was never deleted/recreated (same topic ID throughout)

---

### T17 — Multiple /new in sequence preserve the same topic

**Steps:**
1. Start a pi session, name it `e2e-t17`
2. Wait for topic to appear; note the topic ID
3. Send `/new` in the pi TUI; wait 3s
4. Send `/new` again; wait 3s
5. Send `/new` a third time; wait 3s
6. List topics: `tg.py topics SUPERGROUP`
7. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`
8. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say exactly STILL_ALIVE"`
9. Wait for response

**Expected:**
- Only one topic exists for this session (same topic ID as step 2)
- The topic history contains three `🔄 Session reset` messages
- The session still responds to messages after three resets
- No orphaned or duplicate topics

---

### T18 — /compact preserves the topic

**Steps:**
1. Start a pi session, name it `e2e-t18`
2. Wait for topic to appear; note the topic ID
3. Send a prompt in the pi TUI to build up some conversation:
   `"say BEFORE_COMPACT"`
4. Wait for the agent to finish
5. Send `/compact` in the pi TUI:
   `tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-t18 "/compact" Enter`
6. Wait 5s
7. List topics: `tg.py topics SUPERGROUP`
8. Send a message via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "say exactly AFTER_COMPACT"`
9. Wait for response

**Expected:**
- The topic list still contains exactly one topic for this session (same
  topic ID — no deletion and recreation)
- If `/compact` triggers a session reset, a `🔄 Session reset` message
  appears in the topic
- The session responds to the Telegram message after compact

**Note:** Whether `/compact` triggers `session_shutdown` + `session_start`
depends on pi's internals. If it does, this test verifies the topic survives.
If it doesn't, the topic trivially stays (nothing happened to disturb it).
Either way the topic must persist.

---

### T19 — /new then /exit deletes the topic

Verifies that `/new` doesn't break the eventual cleanup when pi exits.

**Steps:**
1. Start a pi session, name it `e2e-t19`
2. Wait for topic to appear; note the topic ID
3. Send `/new` in the pi TUI; wait 3s
4. Verify the topic still exists
5. `/exit` the pi session
6. Wait up to 15s

**Expected:**
- After `/new`, the topic persists (as expected)
- After `/exit`, the topic is deleted — the pi process has exited, the IPC
  connection breaks, and the daemon cleans up the topic
- `tg.py topics SUPERGROUP` no longer shows `e2e-t19`

---

### T20 — Session name persists (or updates) across /new

**Steps:**
1. Start a pi session, name it `e2e-t20-orig`
2. Wait for topic `e2e-t20-orig` to appear
3. Send `/new` in the pi TUI; wait 5s
4. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- The topic still exists
- The topic name still contains the session name (if pi preserves the name
  across `/new`) or falls back to repo/cwd-based naming (if pi resets the
  name)
- Either way, only one topic exists for this pi process

---

## Standard Tests (continued)

### T21 — Topic created for session with no name (fallback naming)

**Steps:**
1. Start a pi session without naming it (skip the `/name` step)
2. Wait for pup to discover it
3. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- A topic is created with a fallback name (derived from the working
  directory, git repo/branch, or a short ID prefix)
- The topic icon prefix (📎) is present

---

## Telegram Bot Command Tests

These tests verify all pup bot commands work correctly. The bot registers
commands via `setMyCommands` at startup: `/ls`, `/attach`, `/detach`,
`/cancel`, `/verbose`, `/help`.

**Telegram command routing dynamics:**

In **DM mode**, all slash commands go through `parse_command()` and are
handled by the DM handler. The full set of DM commands is available.

In **topics mode** (supergroup), slash commands behave differently:
- `/cancel` is the **only** command intercepted by pup in a topic. It's
  special-cased before the general message handler.
- All other text (including unrecognized `/foo` commands) is forwarded to
  the pi session as a regular message via IPC `send`.
- Telegram's bot command menu (the `/` autocomplete popup) shows all
  registered commands in both DMs and groups, but in a topic context pup
  only handles `/cancel` itself — the rest go to pi.

This means: in a topic, typing `/help` sends the literal text "/help" to
pi (which may or may not do anything with it). Typing `/ls` sends "/ls"
to pi. Only `/cancel` is intercepted.

### C01 — /cancel in a topic aborts the agent

**Steps:**
1. Start a pi session, name it `e2e-c01`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "write a very long and detailed story about a robot"`
4. Wait 3-5s for streaming to start
5. Send `/cancel` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/cancel"`
6. Wait 5s

**Expected:**
- The agent stops generating
- Pup sends an `Abort` command over IPC to the pi session
- The turn tracker finalizes the partial message
- Check pup logs for the abort being sent

---

### C02 — /cancel with no active agent turn

**Steps:**
1. Start a pi session, name it `e2e-c02`
2. Wait for topic; note the topic ID (session should be idle)
3. Send `/cancel` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/cancel"`
4. Wait 3s

**Expected:**
- Pup sends an `Abort` command via IPC (fire-and-forget)
- Pi ignores it (nothing to cancel)
- No crash, no error message in the topic
- The session remains functional — send a normal message and verify

---

### C03 — /cancel via inline keyboard button

**Steps:**
1. Start a pi session, name it `e2e-c03`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram
4. Wait for streaming to start — verify the bot's message has a "✖ Cancel"
   inline keyboard button
5. Click the "✖ Cancel" button (this sends a `callback_query` to the bot)
   — this must be done via the Telegram app/UI, not `tg.py`

**Expected:**
- The bot receives the callback query with data `cancel:<session_id>`
- The bot answers the callback query with "Cancelling…"
- An `Abort` is sent to the session
- The agent stops

**Alternative (programmatic test):**
If clicking buttons isn't possible via `tg.py`, verify by:
- Checking that the bot's streaming message has `reply_markup` with the
  cancel button (via `tg.py history`)
- Using the Telegram Bot API directly to simulate a callback query

---

### C04 — Plain messages in a topic are forwarded (not treated as commands)

This verifies that non-`/cancel` commands are forwarded to pi, not
consumed by the bot.

**Steps:**
1. Start a pi session, name it `e2e-c04`
2. Wait for topic; note the topic ID
3. Send various messages that are NOT `/cancel`:
   - `tg.py send SUPERGROUP TOPIC_ID "hello"`
   - `tg.py send SUPERGROUP TOPIC_ID "/ls"`
   - `tg.py send SUPERGROUP TOPIC_ID "/help"`
   - `tg.py send SUPERGROUP TOPIC_ID "/verbose on"`
   - `tg.py send SUPERGROUP TOPIC_ID "/attach foo"`
4. Wait for responses

**Expected:**
- `"hello"` → forwarded to pi, pi responds normally
- `"/ls"` → forwarded to pi as literal text (pi may show its own session
  list or treat it as unknown)
- `"/help"` → forwarded to pi as literal text
- `"/verbose on"` → forwarded to pi as literal text
- `"/attach foo"` → forwarded to pi as literal text
- None of these trigger pup's DM-mode command handling — they are all
  sent as user messages to the pi session via IPC
- The bot does NOT reply with a session list, help text, or any pup
  command response in the topic

---

### C05 — >> follow-up prefix in a topic

**Steps:**
1. Start a pi session, name it `e2e-c05`
2. Wait for topic; note the topic ID
3. Send a prompt to keep the agent busy:
   `tg.py send SUPERGROUP TOPIC_ID "count from 1 to 50, one per line"`
4. While streaming, send a follow-up:
   `tg.py send SUPERGROUP TOPIC_ID ">> after counting, say PAPAYA"`
5. Wait for both to complete

**Expected:**
- The first message is delivered as `mode: Steer`
- The `>>` message is delivered as `mode: FollowUp` (text: "after
  counting, say PAPAYA" — the `>>` prefix is stripped)
- The follow-up is queued until the agent finishes the first task
- Eventually `PAPAYA` appears in the topic

---

### C06 — DM mode: /ls lists sessions

**Steps:**
1. Start two pi sessions: `e2e-c06-a` and `e2e-c06-b`
2. Wait for both to connect
3. Send `/ls` in a DM to the bot:
   `tg.py send BOT_DM_CHAT_ID "/ls"`
4. Wait for response

**Expected:**
- The bot replies with an HTML-formatted session list showing:
  - `1. e2e-c06-a (dir_name)`
  - `2. e2e-c06-b (dir_name)`
- The list includes session names and shortened cwd paths
- The message ends with "Use /attach <number> to connect."

---

### C07 — DM mode: /attach and /detach

**Steps:**
1. Start a pi session, name it `e2e-c07`
2. Wait for it to connect
3. Send `/ls` in DM to see the session
4. Send `/attach 1` in DM (attach by index)
5. Verify the bot responds with "Attached to **e2e-c07**"
6. Send a plain message in DM: `"say GRAPEFRUIT"`
7. Wait for the response in DM
8. Send `/detach` in DM
9. Verify the bot responds with "Detached."

**Expected:**
- After attach: the bot confirms attachment
- Plain messages are forwarded to the attached session
- The session's response appears in the DM chat
- After detach: the bot confirms detachment
- Subsequent plain messages get "Not attached. Use /ls and /attach first."

---

### C08 — DM mode: /attach by name and ID prefix

**Steps:**
1. Start a pi session, name it `e2e-c08`
2. Wait for it to connect
3. Send `/attach e2e-c08` in DM (attach by name)
4. Verify attachment
5. Send `/detach`
6. Find the session's INSTANCE_ID from pup logs (or `/ls` output)
7. Send `/attach <first-4-chars-of-id>` in DM (attach by ID prefix)
8. Verify attachment

**Expected:**
- Both attachment methods work
- Name match is case-insensitive

---

### C09 — DM mode: /attach with empty/invalid reference

**Steps:**
1. Send `/attach` (no argument) in DM
2. Send `/attach nonexistent_session` in DM
3. Start two sessions both named `e2e-c09`, then `/attach e2e-c09`

**Expected:**
- Empty: bot responds "Usage: /attach <name|index|id>"
- Not found: bot responds "Session not found."
- Ambiguous: bot responds with "Ambiguous — matches: ..." listing the
  matching sessions

---

### C10 — DM mode: /cancel while attached

**Steps:**
1. Start a pi session, name it `e2e-c10`
2. Attach to it in DM
3. Send a long prompt in DM
4. Wait for streaming to start
5. Send `/cancel` in DM
6. Wait 5s

**Expected:**
- The agent is aborted
- The bot responds "Cancelling…"
- The session stops generating

---

### C11 — DM mode: /cancel while not attached

**Steps:**
1. Send `/cancel` in DM (not attached to any session)

**Expected:**
- The bot responds "Not attached to any session."

---

### C12 — DM mode: /verbose toggle

**Steps:**
1. Send `/verbose` in DM (toggle)
2. Note the response (on or off)
3. Send `/verbose on` in DM
4. Note the response
5. Send `/verbose off` in DM
6. Note the response

**Expected:**
- `/verbose` (no arg) toggles the current state
- `/verbose on` sets verbose to on
- `/verbose off` sets verbose to off
- Each response shows "Verbose mode: **on**" or "Verbose mode: **off**"

---

### C13 — DM mode: /help and /start

**Steps:**
1. Send `/help` in DM
2. Send `/start` in DM

**Expected:**
- Both return the same help text listing all available commands
- The help text includes /ls, /attach, /detach, /cancel, /verbose, /help
- The help text describes the `>>` follow-up prefix

---

### C14 — DM mode: auto-detach on session exit

**Steps:**
1. Start a pi session, name it `e2e-c14`
2. Attach to it in DM
3. `/exit` the pi session from the TUI
4. Wait 10s
5. Check DM messages

**Expected:**
- The bot sends "📴 Session ended. Detached." in the DM
- The user is automatically detached
- Subsequent messages get "Not attached. Use /ls and /attach first."

---

### C15 — DM mode: messages while not attached

**Steps:**
1. Ensure no session is attached in DM
2. Send a plain message in DM: `"hello"`

**Expected:**
- The bot responds "Not attached. Use /ls and /attach first."

---

## Pi Slash Commands via Telegram

These tests verify that pi's own slash commands (not pup bot commands)
work when sent from Telegram in a topic. Since topics mode only intercepts
`/cancel`, all other `/` messages are forwarded to pi as regular text.
Pi interprets its own slash commands from this text.

**Key dynamics:**
- Telegram sends the full text (e.g., `/name foo`) to the bot
- Pup's topic handler does NOT parse it as a bot command (only `/cancel`
  is special)
- Pup forwards it via IPC `send` to pi as `mode: Steer`
- Pi's `input` handler receives it and processes the slash command
- Pi fires events (e.g., `session_name_changed`, `session_end`) that flow
  back through the extension → IPC → pup → Telegram

### S01 — /name via Telegram renames the topic

See S09 for the current test — kept here as a cross-reference.

---

### S02 — /exit via Telegram kills the session

See S10 for the current test — kept here as a cross-reference.

---

### S03 — /new via Telegram shows unsupported notification

The `/new` command requires `ExtensionCommandContext.newSession()` which
is not available to extension IPC handlers (only to `registerCommand`
handlers). The extension intercepts the command and broadcasts an error
notification instead of forwarding it to the LLM.

**Steps:**
1. Start a pi session, name it `e2e-s03`
2. Wait for topic; note the topic ID
3. Send `/new` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/new"`
4. Wait 10s
5. Read topic history:
   `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- The bot posts a notification containing `not available via remote access`
- The `/new` text is NOT forwarded to the LLM (the agent does not see it)
- The topic persists (same topic ID)
- The session remains functional — send a follow-up message and verify

---

### S04 — /compact via Telegram compacts the session

The extension handles `/compact` directly via `ctx.compact()`.

**Steps:**
1. Start a pi session, name it `e2e-s04`
2. Wait for topic; note the topic ID
3. Send a prompt via Telegram to build conversation:
   `tg.py send SUPERGROUP TOPIC_ID "say BEFORE_COMPACT"`
4. Wait for response
5. Send `/compact` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/compact"`
6. Wait 15s for compaction to complete
7. List topics: `tg.py topics SUPERGROUP`
8. Send a follow-up message:
   `tg.py send SUPERGROUP TOPIC_ID "say AFTER_COMPACT"`
9. Wait for response

**Expected:**
- The extension calls `ctx.compact()` — the text `/compact` is NOT
  forwarded to the LLM
- Topic persists (same topic ID)
- If compaction triggers `session_shutdown` + `session_start`, the
  extension broadcasts `session_reset` and `🔄 Session reset` appears
- The session responds to the follow-up message after compaction

---

### S05 — /model via Telegram

The extension handles `/model <name>` (with args) by calling
`pi.setModel()`. Without args, it shows an unsupported notification
since the interactive model selector requires the TUI.

**Steps:**
1. Start a pi session, name it `e2e-s05`
2. Wait for topic; note the topic ID
3. Send `/model` (no args) via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/model"`
4. Wait 5s
5. Read topic history

**Expected:**
- The bot posts a notification containing `not available via remote access`
- The `/model` text is NOT forwarded to the LLM

---

### S06 — Unknown slash command via Telegram is forwarded to the LLM

Commands not recognized by the extension (not in the TUI's built-in list)
are passed through to `sendUserMessage()` and reach the LLM.

**Steps:**
1. Start a pi session, name it `e2e-s06`
2. Wait for topic; note the topic ID
3. Send an unknown command via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/nonexistent"`
4. Wait for response

**Expected:**
- The extension does NOT intercept `/nonexistent` (it's not a known
  TUI command)
- The text is forwarded to the LLM via `sendUserMessage()`
- The LLM responds (it sees `/nonexistent` as user text)
- No crash on either side
- The session remains functional

---

### S07 — TUI-only commands via Telegram show notification

Commands that require the pi TUI (interactive selectors, clipboard, etc.)
are intercepted by the extension and produce an error notification.

**Steps:**
1. Start a pi session, name it `e2e-s07`
2. Wait for topic; note the topic ID
3. Send TUI-only commands via Telegram:
   - `tg.py send SUPERGROUP TOPIC_ID "/settings"`
   - `tg.py send SUPERGROUP TOPIC_ID "/copy"`
   - `tg.py send SUPERGROUP TOPIC_ID "/session"`
   - `tg.py send SUPERGROUP TOPIC_ID "/hotkeys"`
4. Wait 10s
5. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- Each command produces a notification containing `not available via
  remote access` and `requires pi TUI`
- None of the commands are forwarded to the LLM
- The session remains functional afterward

---

### S08 — ExtensionCommandContext commands via Telegram show notification

Commands that require `ExtensionCommandContext` (not available to IPC
handlers) are intercepted and produce a specific error notification.

**Steps:**
1. Start a pi session, name it `e2e-s08`
2. Wait for topic; note the topic ID
3. Send commands that need `ExtensionCommandContext`:
   - `tg.py send SUPERGROUP TOPIC_ID "/new"`
   - `tg.py send SUPERGROUP TOPIC_ID "/fork"`
   - `tg.py send SUPERGROUP TOPIC_ID "/tree"`
   - `tg.py send SUPERGROUP TOPIC_ID "/resume"`
   - `tg.py send SUPERGROUP TOPIC_ID "/reload"`
4. Wait 10s
5. Read topic history: `tg.py history SUPERGROUP TOPIC_ID`

**Expected:**
- Each command produces a notification containing `not available via
  remote access` and `upstream pi API change`
- None of the commands are forwarded to the LLM
- The session remains functional afterward

---

### S09 — /name via Telegram renames the topic

(Moved from old S01 — tests the supported `/name` command.)

**Steps:**
1. Start a pi session, name it `e2e-s09-original`
2. Wait for topic `e2e-s09-original`; note the topic ID
3. Send `/name e2e-s09-renamed` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/name e2e-s09-renamed"`
4. Wait up to 15s

**Expected:**
- The extension calls `pi.setSessionName("e2e-s09-renamed")`
- The extension detects the name change (via polling)
- The extension broadcasts `session_name_changed`
- Pup renames the topic via `editForumTopic`
- `tg.py topics SUPERGROUP` shows the topic as `📎 e2e-s09-renamed`
- The topic ID is unchanged (same topic, just renamed)

---

### S10 — /exit via Telegram kills the session and deletes the topic

**Steps:**
1. Start a pi session, name it `e2e-s10`
2. Wait for topic; note the topic ID
3. Send `/exit` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/exit"`
4. Wait up to 15s

**Expected:**
- The extension calls `ctx.shutdown()`
- The pi process exits
- The IPC connection breaks
- Pup detects the disconnect and deletes the topic
- `tg.py topics SUPERGROUP` no longer shows `e2e-s10`

---

### S11 — /cancel@botname works in group topics

In Telegram groups, commands are sent with `@botname` suffix when the user
picks them from the autocomplete menu, e.g., `/cancel@my_pup_bot`. Pup
strips the `@botname` suffix before matching commands.

**Steps:**
1. Start a pi session, name it `e2e-s07`
2. Wait for topic; note the topic ID
3. Find the bot's username from config or `tg.py` output
4. Send a long prompt via Telegram to start the agent
5. Wait for streaming to start
6. Send `/cancel@BOTUSERNAME` in the topic:
   `tg.py send SUPERGROUP TOPIC_ID "/cancel@my_pup_bot"`
7. Wait 5s

**Expected:**
- Pup strips the `@my_pup_bot` suffix and recognizes `/cancel`
- The agent is aborted (same behavior as plain `/cancel`)
- The cancel is NOT forwarded to pi as a literal message

---

### S12 — Command sent to General topic (not a pup topic)

Messages in the supergroup's "General" topic (or any non-pup topic)
should not be processed by pup.

**Steps:**
1. Start a pi session, name it `e2e-s08`
2. Wait for topic
3. Send a message to the General topic (thread_id = 1 or no thread):
   `tg.py send SUPERGROUP 1 "this is in General"`
4. Wait 5s

**Expected:**
- Pup receives the update but `session_for_thread(1)` returns None
  (General is not mapped to a session)
- The message is not forwarded to any session
- If DM mode is also enabled on this chat, it may fall through to DM
  handling — but for a supergroup, DM mode should not apply
- No crash, no errors

---

## Robustness Tests

These test edge cases around startup, restarts, and socket recovery.

### R01 — Pup restart picks up existing sessions

**Steps:**
1. Start a pi session, name it `e2e-r01`
2. Wait for topic to appear
3. Stop pup (Ctrl-C)
4. Wait 3s
5. Start pup again
6. Wait for startup

**Expected:**
- Pup discovers the existing session on startup (check logs for
  `session connected` with `e2e-r01`)
- A topic is created (or reused if persisted) for the session
- Sending a message via Telegram to the topic works end-to-end

**Note:** The pi extension's socket filename is a stable `INSTANCE_ID`,
so pup sees the same socket across restarts as long as the pi process
is still running.

---

### R02 — Socket directory deleted while sessions running

This tests whether pi extensions recover when `~/.pi/pup/` is wiped.

**Steps:**
1. Start a pi session, name it `e2e-r02`
2. Wait for topic to appear; verify socket exists:
   `ls ~/.pi/pup/*.sock`
3. Stop pup (Ctrl-C)
4. Delete the socket directory: `rm -rf ~/.pi/pup`
5. Wait 10s for the extension's `socketCheckTimer` to detect the loss
   and recreate the socket
6. Check: `ls ~/.pi/pup/*.sock`
7. Start pup again
8. Wait for startup

**Expected:**
- The extension detects its socket file is missing (via the 2-second
  `socketCheckTimer` interval) and recreates `~/.pi/pup/` and the socket
- `ls ~/.pi/pup/*.sock` shows a socket file after the recovery
- Pup discovers the session and creates a topic
- Message routing works end-to-end

---

### R03 — Socket files deleted but directory preserved

**Steps:**
1. Start a pi session, name it `e2e-r03`
2. Wait for topic to appear
3. Stop pup (Ctrl-C)
4. Delete just the sockets: `rm -f ~/.pi/pup/*.sock ~/.pi/pup/*.alias`
5. Wait 10s for the extension to detect and recreate
6. Check: `ls ~/.pi/pup/*.sock`
7. Start pup again
8. Wait for startup

**Expected:**
- The extension detects its socket file is missing and recreates it
- Pup discovers the session and creates a topic
- Message routing works

---

### R04 — Pup starts before any pi sessions

**Steps:**
1. Ensure no pi sessions are running (`rm -f ~/.pi/pup/*.sock`)
2. Start pup
3. Wait for startup to complete
4. Start a pi session, name it `e2e-r04`
5. Wait up to 15s

**Expected:**
- Pup starts cleanly with no sessions
- When the pi session starts, pup discovers it via filesystem watcher
- Topic is created

---

### R05 — Multiple pup restarts with sessions running

**Steps:**
1. Start two pi sessions: `e2e-r05-a` and `e2e-r05-b`
2. Wait for both topics
3. Stop pup, start pup (restart 1)
4. Wait for both topics to reappear
5. Send a message to each topic, verify responses
6. Stop pup, start pup (restart 2)
7. Wait for both topics to reappear
8. Send a message to each topic, verify responses

**Expected:**
- Each restart picks up both sessions
- Topics are recreated (or reused from persisted state)
- Message routing works correctly after each restart
- No leaked/orphaned topics accumulate (check topic count)

---

### R06 — Session exits during pup downtime

**Steps:**
1. Start a pi session, name it `e2e-r06`
2. Wait for topic to appear
3. Stop pup
4. Exit the pi session (`/exit`)
5. Start pup again
6. Wait for startup

**Expected:**
- On startup, pup scans the socket directory
- The socket is gone (the pi process exited, OS closed the socket; pup's
  discovery scan probes it and removes the stale file if still present)
- Pup starts with zero sessions
- No stale topic is created
- The old topic from step 2 may be orphaned (this is documented/expected
  behavior — pup cleans up topics it knows about via persisted state, but
  may miss topics from a previous era)

---

### R07 — Session starts and exits rapidly before pup connects

**Steps:**
1. Ensure pup is running
2. Start a pi session, name it `e2e-r07`
3. Immediately `/exit` the session (within 1-2 seconds)
4. Wait 10s

**Expected:**
- If pup connected in time: topic created then deleted
- If pup didn't connect in time: stale socket cleaned up on next scan,
  no topic created
- Either way: no crash, no orphaned topics
- Check pup logs for errors

---

### R08 — Extension socket recovery after directory wipe (with running pup)

**Steps:**
1. Start a pi session, name it `e2e-r08`
2. Ensure pup is running; wait for topic to appear; note topic ID
3. `rm -rf ~/.pi/pup`
4. Wait up to 10s for the extension to detect the loss and recreate the
   socket (the `socketCheckTimer` polls every 2 seconds)
5. Check: `ls ~/.pi/pup/*.sock`
6. Wait for pup to discover the recreated socket (up to 10s — the
   discovery periodic rescan runs every 5 seconds)
7. Check pup logs for a new `session connected`
8. List topics and verify a topic exists for the session

**Expected:**
- The extension detects the socket file is gone and recreates both the
  directory and socket file
- Pup discovers the new socket and reconnects
- A new topic is created (the old one from the previous connection is
  cleaned up or orphaned)
- Message routing works after recovery

---

### R09 — Pup handles corrupt/partial socket files

**Steps:**
1. Ensure pup is running
2. Create a fake socket file: `touch ~/.pi/pup/fake-session.sock`
3. Wait 5s
4. Check pup logs

**Expected:**
- Pup probes the fake socket, finds it's not alive
- Pup removes the stale socket (logged as `removing stale socket`)
- No crash, no topic created

---

### R10 — Many sessions simultaneously

**Steps:**
1. Start 5 pi sessions: `e2e-r10-1` through `e2e-r10-5`
2. Wait for all 5 topics to appear
3. Send a message to each topic
4. Wait for all responses

**Expected:**
- All 5 topics created
- All 5 messages routed correctly
- All 5 responses appear in their correct topics
- No message cross-talk

**Cleanup:**
- Exit all 5 sessions
- Verify all 5 topics deleted

---

### R11 — /new during pup downtime

Tests that a session reset while pup is stopped doesn't cause problems
when pup restarts.

**Steps:**
1. Start a pi session, name it `e2e-r11`
2. Wait for topic to appear
3. Stop pup (Ctrl-C)
4. Send `/new` in the pi TUI; wait 3s
5. Start pup again
6. Wait for startup

**Expected:**
- Pup discovers the session on startup (same socket, since the extension
  uses a stable `INSTANCE_ID`)
- A topic is created (or reused from persisted state)
- The session is functional — sending a message via Telegram works
- No crash, no orphaned topics

**Note:** The daemon missed the `session_reset` event (it wasn't running),
but the socket stayed alive, so the session is picked up normally.

---

## Grace Period Tests

When a pi session disconnects, pup waits 30 seconds before deleting the
topic. If a new pi session starts in the same working directory within
that window, the topic is transferred to the new session instead of being
deleted and recreated. This handles pi restarts gracefully.

### G01 — Pi restart in same cwd reuses topic

A pi session is restarted (killed and relaunched in the same directory).
The new session should reclaim the old topic.

**Steps:**
1. Create a working directory: `WORK=$(mktemp -d)`
2. Start a pi session in `$WORK`, name it `e2e-g01`
3. Wait for topic to appear; note the topic ID
4. Send a message to verify it works:
   `tg.py send SUPERGROUP TOPIC_ID "say BEFORE_RESTART"`
5. Wait for response
6. Exit the pi session (Ctrl-D)
7. Immediately (within a few seconds) start a new pi session in the
   same `$WORK` directory
8. Name it `e2e-g01-after`
9. Wait up to 15s

**Expected:**
- The old topic is NOT deleted (grace period holds it)
- The new session claims the old topic (matched by cwd)
- The topic is renamed to reflect the new session name (`e2e-g01-after`)
- `tg.py topics SUPERGROUP` shows exactly one topic
- The topic ID is the same as in step 3
- Sending a message to the topic works:
  `tg.py send SUPERGROUP TOPIC_ID "say AFTER_RESTART"`
- The response appears in the same topic

---

### G02 — Topic deleted after grace period expires (no reconnect)

A pi session exits and no replacement appears within 30 seconds.

**Steps:**
1. Start a pi session, name it `e2e-g02`
2. Wait for topic to appear
3. Exit the pi session
4. Wait 35 seconds (past the 30s grace period)

**Expected:**
- The topic is deleted after the grace period expires
- `tg.py topics SUPERGROUP` shows no topics (besides General)

---

### G03 — Grace period: new session in different cwd gets new topic

A pi session exits in dir A, and a new session starts in dir B within
the grace period. The new session should NOT reclaim the old topic.

**Steps:**
1. Create two directories: `WORK_A=$(mktemp -d)` and `WORK_B=$(mktemp -d)`
2. Start a pi session in `$WORK_A`, name it `e2e-g03-a`
3. Wait for topic; note its topic ID
4. Exit the pi session
5. Immediately start a new pi session in `$WORK_B`, name it `e2e-g03-b`
6. Wait for topic

**Expected:**
- The new session (in `$WORK_B`) does NOT reclaim the old topic (different cwd)
- A new topic is created for `e2e-g03-b`
- After 30s, the old topic for `e2e-g03-a` is deleted
- Eventually only one topic remains

---

### G04 — Graceful pup shutdown preserves topic mapping

When pup shuts down gracefully while topics are in the grace period,
the mappings are restored so they survive across pup restarts.

**Steps:**
1. Start a pi session, name it `e2e-g04`
2. Wait for topic; note the topic ID
3. Exit the pi session (topic enters grace period)
4. Immediately stop pup gracefully (Ctrl-C) — within the grace period
5. Start a new pi session in the same cwd
6. Restart pup

**Expected:**
- Pup's graceful shutdown calls `cancel_all_pending`, restoring the
  topic mapping to `topics_state.json`
- On restart, pup reuses the persisted topic for the new session
- The topic ID is preserved (same topic, not deleted and recreated)
- Sending a message works

---

### G05 — Multiple sessions: only the matching cwd reclaims

Two sessions running in different directories. One exits and restarts.
Only the restarted session's topic should be reclaimed.

**Steps:**
1. Create two dirs: `WORK_A=$(mktemp -d)` and `WORK_B=$(mktemp -d)`
2. Start pi session A in `$WORK_A`, name it `e2e-g05-a`
3. Start pi session B in `$WORK_B`, name it `e2e-g05-b`
4. Wait for both topics; note their topic IDs
5. Exit session A
6. Immediately restart a new session in `$WORK_A`, name it `e2e-g05-a2`
7. Wait up to 15s

**Expected:**
- Session B's topic is unaffected (still active, same topic ID)
- The new session in `$WORK_A` reclaims session A's topic
- Two topics exist total (one for B, one for the reclaimed A)
- Both topics are functional (send messages, get responses)

---

## Name Continuity Tests

These tests verify that session names survive across `/new`, pi restarts,
and pup restarts. The name is treated as a property of the *workspace*
(identified by working directory), not of an individual session.

Three mechanisms work together:
1. **Extension**: remembers `lastKnownName` and re-applies it after `/new`
2. **Grace period**: `PendingDeletion` carries the name; `claim_pending_topic`
   returns it so the daemon can push `/name` via IPC
3. **Persistent cache**: `cwd_names` in `PersistedState` maps cwd→name,
   surviving pup restarts

### N01 — /new preserves the session name

The session name should survive `/new` within the same pi process.

**Steps:**
1. Start a pi session, name it `e2e-n01-named`
2. Wait for topic `e2e-n01-named` to appear; note the topic ID
3. Send `/new` in the pi TUI:
   `tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-n01 "/new" Enter`
4. Wait 10s for the reset to propagate
5. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- The topic still exists (same topic ID — the socket is stable)
- The topic title still contains `e2e-n01-named`
- The extension re-applied the name via `pi.setSessionName(lastKnownName)`
- A `🔄 Session reset` message appears in the topic

---

### N02 — Multiple /new preserve the name

Name should persist through repeated `/new` commands.

**Steps:**
1. Start a pi session, name it `e2e-n02-sticky`
2. Wait for topic `e2e-n02-sticky`
3. Send `/new` three times (with short pauses):
   ```bash
   tmux send "/new" Enter; sleep 5
   tmux send "/new" Enter; sleep 5
   tmux send "/new" Enter; sleep 5
   ```
4. Wait 10s
5. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- The topic still contains `e2e-n02-sticky` after three resets
- Only one topic exists
- The session name is shown correctly in the topic title

---

### N03 — Name restored after pi restart (same cwd, within grace period)

When pi exits and restarts in the same directory within the 30s grace
period, the new session should inherit the old session's name.

**Steps:**
1. Create a working directory: `WORK=$(mktemp -d)`
2. Start a pi session in `$WORK`, name it `e2e-n03-persist`
3. Wait for topic `e2e-n03-persist`; note the topic ID
4. Exit the pi session (Ctrl-D)
5. Immediately (within a few seconds) start a new pi session in `$WORK`
   — do NOT name it
6. Wait up to 15s

**Expected:**
- The new session claims the old topic (grace period + cwd match)
- The daemon sends `/name e2e-n03-persist` via IPC to restore the name
- The topic title still contains `e2e-n03-persist`
- The topic ID is the same as step 3
- `tg.py topics SUPERGROUP` shows exactly one topic with the old name

---

### N04 — Name restored after pup restart

When pup restarts while a session is running, the name should be
restored from the persistent `cwd_names` cache if the session has no
name (e.g., after a `/new`).

**Steps:**
1. Start a pi session, name it `e2e-n04-cached`
2. Wait for topic `e2e-n04-cached`
3. Send `/new` in the pi TUI (name is re-applied by the extension, but
   let's verify the daemon's cache too)
4. Wait 5s
5. Stop pup (Ctrl-C)
6. Wait 2s
7. Start pup again
8. Wait for startup
9. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- The session is rediscovered on pup restart
- The topic title contains `e2e-n04-cached` — either because the
  extension already re-applied the name (and pup got it in `hello`),
  or because the daemon's `cwd_names` cache restored it
- The session is functional: send a message, get a response

---

### N05 — /name via Telegram updates the persistent cache

When the user renames a session via Telegram, the new name should be
persisted in `cwd_names` so it survives future restarts.

**Steps:**
1. Start a pi session, name it `e2e-n05-orig`
2. Wait for topic; note the topic ID
3. Rename via Telegram: `tg.py send SUPERGROUP TOPIC_ID "/name e2e-n05-renamed"`
4. Wait 10s for the rename to propagate
5. Verify topic title: `tg.py topics SUPERGROUP` shows `e2e-n05-renamed`
6. Stop pup (Ctrl-C); wait 2s
7. Start pup again; wait for startup
8. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- After step 5: topic renamed to `e2e-n05-renamed`
- After step 8: topic still shows `e2e-n05-renamed` (the `InfoChanged`
  event persisted the name in `cwd_names`)

---

### N06 — Name inherited across both pi and pup restart

Full restart scenario: both pi and pup are restarted. The name should
survive via the persistent `cwd_names` cache.

**Steps:**
1. Create a working directory: `WORK=$(mktemp -d)`
2. Start a pi session in `$WORK`, name it `e2e-n06-survive`
3. Wait for topic `e2e-n06-survive`
4. Stop pup (Ctrl-C)
5. Exit the pi session
6. Wait 3s
7. Start a new pi session in `$WORK` — do NOT name it
8. Start pup
9. Wait for startup
10. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- Pup loads `cwd_names` from the persisted state file
- The new session (in the same `$WORK`) connects with no name
- Pup finds a `cwd_names` entry for `$WORK` → `e2e-n06-survive`
- Pup sends `/name e2e-n06-survive` to the session via IPC
- The topic title contains `e2e-n06-survive`

---

### N07 — New /name overrides inherited name

If the user explicitly sets a new name, it should override the cached
name for that cwd.

**Steps:**
1. Create a working directory: `WORK=$(mktemp -d)`
2. Start a pi session in `$WORK`, name it `e2e-n07-old`
3. Wait for topic; exit the session
4. Start a new pi session in `$WORK` (name should be inherited as
   `e2e-n07-old`)
5. Wait for topic with `e2e-n07-old`; note the topic ID
6. Rename in the pi TUI: `/name e2e-n07-new`
7. Wait 10s
8. Verify topic: `tg.py topics SUPERGROUP` shows `e2e-n07-new`
9. Exit the session; start another new session in `$WORK`
10. Wait for topic

**Expected:**
- Step 5: the inherited name `e2e-n07-old` is applied
- Step 8: the topic is renamed to `e2e-n07-new`
- Step 10: the newest name `e2e-n07-new` is inherited (the cache was
  updated when the user renamed in step 6)

---

### N08 — /compact preserves the session name

`/compact` triggers a session reset similar to `/new`. The name should
survive.

**Steps:**
1. Start a pi session, name it `e2e-n08-compact`
2. Wait for topic `e2e-n08-compact`; note the topic ID
3. Send a prompt to build context: `"say hello"`
4. Wait for response
5. Send `/compact` in the pi TUI
6. Wait 15s
7. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- The topic still exists (same topic ID)
- The topic title still contains `e2e-n08-compact`
- The extension re-applied the name after the session reset

---

## Pedantic Tests

Stress tests, crash scenarios, race conditions, and adversarial edge cases.
These go beyond "does the happy path work" and ask "what breaks when things
go wrong."

### Crash & Kill Scenarios

### P01 — SIGKILL pup mid-stream

The daemon is violently killed while an agent is actively streaming a
response. No graceful shutdown, no cleanup.

**Steps:**
1. Start a pi session, name it `e2e-p01`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "write a very detailed essay about the history of every programming language"`
4. Wait 3-5s for streaming to start (verify a bot message appears)
5. `kill -9 $(pgrep -f 'target/debug/pup')` — hard kill pup
6. Wait 3s
7. Start pup again
8. Wait for startup; note the new topic ID for the session

**Expected:**
- Pup dies immediately — no graceful shutdown, partial message left in
  the old topic
- On restart, pup discovers the still-running pi session (socket is alive)
- A new topic is created (or the old one reused via persisted state)
- The session is functional: send a message, get a response
- The old topic may have a partial/orphaned message — this is acceptable
- No crash on restart, no infinite loop

---

### P02 — SIGKILL pi mid-stream

The pi process is killed while the agent is streaming. The daemon must
detect the broken IPC connection and clean up.

**Steps:**
1. Start a pi session in tmux, name it `e2e-p02`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram
4. Wait 3-5s for streaming
5. Find the pi process and kill it: `kill -9 $(pgrep -f 'pi.*dangerously')`
   (or kill the specific tmux window's shell)
6. Wait up to 15s

**Expected:**
- Pup detects the broken IPC connection (read returns EOF or error)
- Pup emits `Disconnected` and deletes the topic
- The partial message in the topic is lost (topic deleted)
- Pup logs show `session disconnected` with a reason
- No crash, no leaked state

---

### P03 — Machine reboot simulation (kill everything)

Simulate a hard power loss — kill pup, kill all pi sessions, delete the
socket directory.

**Steps:**
1. Start two pi sessions: `e2e-p03-a`, `e2e-p03-b`
2. Wait for both topics
3. Kill pup: `kill -9 $(pgrep -f 'target/debug/pup')`
4. Kill all pi sessions: `tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-p03-a "exit" Enter`
   (repeat for b)
5. `rm -rf ~/.pi/pup`
6. Wait 5s
7. Start pup
8. Start two new pi sessions: `e2e-p03-a2`, `e2e-p03-b2`
9. Wait for topics

**Expected:**
- Pup starts cleanly despite missing socket directory (it creates it)
- The old topics may be orphaned (no live sessions to map to)
- New sessions are discovered and get new topics
- No crash, no corruption of `topics_state.json`
- No stale state from the previous run causes problems

---

### P04 — Pup crashes and restarts in a tight loop

Pup is restarted rapidly multiple times while a session is running. Tests
that state doesn't accumulate or corrupt.

**Steps:**
1. Start a pi session, name it `e2e-p04`
2. Start pup, wait for topic
3. Stop pup (Ctrl-C)
4. Immediately start pup again (no delay)
5. Repeat steps 3-4 five times in rapid succession
6. On the final start, wait for startup to complete
7. List topics; send a message to the session's topic

**Expected:**
- Each restart picks up the session (same socket, stable INSTANCE_ID)
- At most one topic exists per session at any time (old ones cleaned up
  via persisted state or stale topic cleanup)
- No duplicate topics accumulate
- Message routing works on the final restart
- No corruption of `topics_state.json` from partial writes

---

### P05 — Pup killed exactly during topic creation API call

Pup is killed while the `createForumTopic` API call is in flight.

**Steps:**
1. Start pup with extra logging: `RUST_LOG=debug`
2. Start a pi session, name it `e2e-p05`
3. Watch the logs. As soon as `creating topic` appears, immediately
   `kill -9 $(pgrep -f 'target/debug/pup')`
4. Wait 3s
5. Start pup again
6. Wait for startup

**Expected:**
- If the topic was created before pup died:
  - On restart, pup either reuses it (persisted state) or creates a new
    one (stale topic scan should clean up the orphan)
- If the topic was NOT created:
  - On restart, pup creates the topic normally
- Either way: no duplicate topics, no crash
- Check `topics_state.json` — it may be empty or partial; pup should
  handle both gracefully

---

### P06 — Pi killed during pup startup scan

Pi exits exactly while pup is scanning for live sockets on startup.

**Steps:**
1. Start a pi session, name it `e2e-p06`
2. Stop pup if running
3. Start pup
4. Immediately (within 1-2s) `/exit` the pi session
5. Wait for pup startup to complete

**Expected:**
- Pup may or may not have connected before the session died:
  - If connected: topic created then immediately deleted
  - If not connected: socket found dead during scan, removed
- No crash, no orphaned topic
- Pup starts cleanly and is ready for new sessions

---

### Topic Integrity

### P07 — User manually deletes a topic in Telegram

The user deletes a pup-managed topic via the Telegram UI while the session
is still running.

**Steps:**
1. Start a pi session, name it `e2e-p07`
2. Wait for topic; note the topic ID
3. Delete the topic manually via Telegram (use the supergroup admin UI)
4. Wait 5s
5. Send a prompt in the pi TUI to trigger activity: `"say ORPHANED"`
6. Check pup logs

**Expected:**
- Pup tries to send/edit a message in the deleted topic
- The Telegram API returns an error (topic not found / thread not found)
- Pup logs a warning but does NOT crash
- The session remains connected (IPC is fine — only the topic is gone)
- Subsequent events for this session fail silently (warnings in logs)
- If the session disconnects and reconnects (or pup restarts), a new
  topic should be created

---

### P08 — User renames a topic in Telegram

The user renames a pup-managed topic via the Telegram admin UI.

**Steps:**
1. Start a pi session, name it `e2e-p08`
2. Wait for topic
3. Rename the topic manually in Telegram to "USER_RENAMED"
4. Rename the pi session: `/name e2e-p08-new` in the pi TUI
5. Wait 5s

**Expected:**
- When the pi session name changes, pup calls `editForumTopic` to rename
  the topic back to `📎 e2e-p08-new`
- The user's manual rename is overwritten
- No crash, no duplicate topics

---

### P09 — Two sessions with identical names

**Steps:**
1. Start pi session A in dir `/tmp/a`, name it `e2e-p09-same`
2. Start pi session B in dir `/tmp/b`, name it `e2e-p09-same`
3. Wait for both topics

**Expected:**
- Two distinct topics exist:
  - `📎 e2e-p09-same`
  - `📎 e2e-p09-same (2)`
- Messages sent to each topic route to the correct session
- No collision or confusion

---

### P10 — Topic creation fails (API error)

Simulate a scenario where topic creation fails (e.g., by temporarily
revoking the bot's `can_manage_topics` permission, or by rate limiting).

**Steps:**
1. If possible, revoke the bot's `can_manage_topics` in the supergroup
2. Start a pi session, name it `e2e-p10`
3. Wait 10s
4. Check pup logs

**Expected:**
- Pup logs a warning about failing to create the topic
- The pi session is still connected via IPC (the failure is topic-side)
- No crash
- If the permission is restored and pup is restarted, the topic is created

**Alternative (rate limit simulation):**
- Start 10+ sessions rapidly to trigger Telegram's rate limit
- Pup should queue/retry topic creation, not crash

---

### P11 — Topic deleted by Telegram (spam/abuse auto-moderation)

Some Telegram supergroup bots or auto-moderation might delete topics. This
tests the same scenario as P07 but from the perspective of an external force.

**Steps:**
1. Start a pi session, name it `e2e-p11`
2. Wait for topic
3. Use a supergroup admin bot (or manual admin action) to delete the topic
4. Send a prompt via the pi TUI
5. Check pup logs for errors
6. Restart pup
7. Wait for a new topic to be created

**Expected:**
- Same as P07 — errors are logged, no crash
- On restart, a new topic is created for the still-running session

---

### Socket & IPC Edge Cases

### P12 — Socket file permissions changed

The socket file's permissions are changed so the daemon can't connect.

**Steps:**
1. Start a pi session, name it `e2e-p12`
2. Wait for topic
3. Stop pup
4. `chmod 000 ~/.pi/pup/*.sock`
5. Start pup
6. Wait 10s
7. Check pup logs
8. Restore permissions: `chmod 700 ~/.pi/pup/*.sock`
9. Wait for pup's periodic rescan (5s) to re-probe the socket

**Expected:**
- On startup, pup probes the socket and gets a permission denied error
- Pup logs a warning, treats it as a dead socket
- After restoring permissions, the next rescan finds the socket alive
  and connects
- Topic is created, message routing works

---

### P13 — Socket directory is a file (not a directory)

**Steps:**
1. Stop pup
2. `rm -rf ~/.pi/pup && touch ~/.pi/pup` (create a file where the
   directory should be)
3. Start pup
4. Check what happens

**Expected:**
- Pup's discovery fails to create or read the directory
- Pup logs an error
- Pup should not crash — it may run with zero sessions
- After fixing: `rm ~/.pi/pup && mkdir -p ~/.pi/pup`, pup (or the
  extension) recreates the directory and resumes

---

### P14 — IPC protocol violation (garbage data on socket)

A rogue process connects to the extension's socket and sends garbage.

**Steps:**
1. Start a pi session, name it `e2e-p14`
2. Find the socket: `ls ~/.pi/pup/*.sock`
3. Send garbage: `echo "NOT JSON AT ALL" | socat - UNIX-CONNECT:$SOCKET_PATH`
4. Wait 5s
5. Verify pup is still running and the topic still works

**Expected:**
- The extension (or daemon's IPC reader) gets a JSON parse error
- The garbage is ignored (the extension sends error response or ignores)
- The rogue connection is closed
- The daemon's real connection is unaffected (it's a separate client)
- No crash on either side

---

### P15 — Multiple daemon instances connect to the same extension

Two pup daemons accidentally started, both trying to control the same
sessions.

**Steps:**
1. Start a pi session, name it `e2e-p15`
2. Start pup instance 1 (normal)
3. Start pup instance 2 in another tmux window (different PID, same config)
4. Wait for both to stabilize
5. Send a message to the topic

**Expected:**
- Both daemons connect to the extension's socket (it supports multiple
  clients)
- Both receive events and create topics (likely two topics for the same
  session, or one if the bot can't create a second with the same name)
- Messages may be duplicated or conflict
- **This is a known unsupported configuration** — the test documents the
  behavior, not a requirement. At minimum: no crash, no data corruption.

---

### P16 — Extension socket replaced by a different process

Another process creates a socket at the same path the extension uses.

**Steps:**
1. Start a pi session, name it `e2e-p16`
2. Wait for topic
3. Stop pup
4. Delete the real socket: `rm ~/.pi/pup/*.sock`
5. Create a fake socket at the same path:
   `socat UNIX-LISTEN:$OLD_SOCKET_PATH,fork /dev/null &`
6. Start pup
7. Wait for pup to connect to the fake socket

**Expected:**
- Pup connects to the fake socket (probe succeeds)
- Pup reads from it but gets no hello/history events (the fake server
  sends nothing)
- Pup times out or gets an unexpected stream
- Pup logs warnings, does not create a topic
- No crash

---

### Session Reset Stress

### P17 — /new while agent is mid-stream

**Steps:**
1. Start a pi session, name it `e2e-p17`
2. Wait for topic; note the topic ID
3. Send a long prompt via Telegram
4. Wait 3-5s for streaming to start
5. Send `/new` in the pi TUI (interrupting the agent mid-stream)
6. Wait 5s
7. List topics
8. Send a new message via Telegram to the same topic

**Expected:**
- The agent's current turn is aborted
- The daemon receives `agent_end` (if pi cleans up) then `session_reset`
- The turn tracker finalizes the partial message in the topic
- `🔄 Session reset` message appears
- The same topic (same ID) persists
- The new message is routed to the new (empty) session and gets a response

---

### P18 — /new sent from Telegram shows unsupported notification

The extension intercepts `/new` and returns an error notification because
`newSession()` is only available on `ExtensionCommandContext`.

**Steps:**
1. Start a pi session, name it `e2e-p18`
2. Wait for topic; note the topic ID
3. Send `/new` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/new"`
4. Wait 10s
5. List topics
6. Read topic history

**Expected:**
- The extension intercepts `/new` and broadcasts a notification event
- The bot posts a message containing `not available via remote access`
- The topic persists (no deletion/recreation)
- The `/new` text is NOT forwarded to the LLM
- The session is functional afterward

---

### P19 — /compact sent from Telegram works via extension API

The extension handles `/compact` directly via `ctx.compact()`.

**Steps:**
1. Start a pi session, name it `e2e-p19`
2. Wait for topic; note the topic ID
3. Send a prompt to build some conversation:
   `tg.py send SUPERGROUP TOPIC_ID "say BEFORE_COMPACT_P19"`
4. Wait for response
5. Send `/compact` via Telegram:
   `tg.py send SUPERGROUP TOPIC_ID "/compact"`
6. Wait 15s
7. List topics

**Expected:**
- The extension calls `ctx.compact()` — `/compact` is NOT forwarded
  to the LLM
- Topic persists (same topic ID)
- Session is functional afterward — send a message and verify response

---

### P20 — Rapid /new spam (10 resets in 10 seconds)

**Steps:**
1. Start a pi session, name it `e2e-p20`
2. Wait for topic; note the topic ID
3. Rapidly send `/new` 10 times:
   ```bash
   for i in $(seq 1 10); do
     tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-p20 "/new" Enter
     sleep 1
   done
   ```
4. Wait 15s for all resets to propagate
5. List topics
6. Read topic history
7. Send a message and verify response

**Expected:**
- Only one topic exists (same topic ID throughout)
- Topic history contains 10 `🔄 Session reset` messages
- No duplicate topics, no orphaned topics
- The session still responds to messages after all resets
- No crash in pup (the daemon handles rapid `session_reset` events)

---

### P21 — /new followed immediately by /exit

**Steps:**
1. Start a pi session, name it `e2e-p21`
2. Wait for topic; note the topic ID
3. Send `/new` in the pi TUI
4. Immediately (within 500ms) send `/exit`:
   ```bash
   tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-p21 "/new" Enter
   sleep 0.5
   tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-p21 "/exit" Enter
   ```
5. Wait 15s
6. List topics

**Expected:**
- The topic is ultimately deleted (pi exited, IPC connection broke)
- Pup may or may not have seen the `session_reset` event before the
  disconnect — either ordering is fine
- No crash, no orphaned topic

---

### State Persistence & Recovery

### P22 — topics_state.json deleted between restarts

**Steps:**
1. Start a pi session, name it `e2e-p22`
2. Wait for topic
3. Stop pup
4. `rm -f ~/.pi/pup/topics_state.json`
5. Start pup
6. Wait for startup

**Expected:**
- Pup starts with empty state (no persisted topic mappings)
- Pup discovers the still-running session, creates a new topic
- The old topic from the first run is orphaned (pup doesn't know about it)
- The startup scan may clean it up if it's in the `getUpdates` history
- No crash

---

### P23 — topics_state.json is corrupt JSON

**Steps:**
1. Stop pup
2. `echo "NOT {VALID JSON" > ~/.pi/pup/topics_state.json`
3. Start pup
4. Wait for startup

**Expected:**
- Pup fails to parse the state file
- Pup falls back to empty state (the `load_state` function handles this)
- Pup starts normally, discovers sessions, creates topics
- No crash

---

### P24 — topics_state.json points to deleted topics

The state file references topic IDs that no longer exist in Telegram
(e.g., they were manually deleted, or Telegram garbage-collected them).

**Steps:**
1. Start a pi session, name it `e2e-p24`
2. Wait for topic
3. Stop pup
4. Delete the topic manually in Telegram
5. Start pup (the state file still maps the session to the deleted topic)

**Expected:**
- On startup, pup tries to reuse the persisted topic by calling
  `editForumTopic` (a probe/rename)
- The API call fails (topic not found)
- Pup falls back to creating a new topic (the `create_topic` logic
  handles this in the "persisted topic gone" branch)
- No crash, no stale mapping

---

### P25 — topics_state.json has stale entries for dead sessions

The state file maps session IDs that no longer exist to topic IDs.

**Steps:**
1. Start a pi session, name it `e2e-p25`
2. Wait for topic
3. `/exit` the pi session
4. Wait for topic deletion
5. Edit `topics_state.json` to re-add the mapping:
   ```bash
   echo '{"topics":{"fake-session-id":99999},"known_threads":[99999],"scan_checkpoint":0}' \
     > ~/.pi/pup/topics_state.json
   ```
6. Start pup
7. Wait for startup

**Expected:**
- On startup, pup checks live sessions vs persisted state
- `fake-session-id` has no matching `.sock` file
- Pup's `cleanup_stale_topics` deletes the stale thread (or fails
  gracefully if the thread doesn't exist in Telegram)
- No crash, clean state after startup

---

### P26 — Disk full — state file write fails

**Steps:**
1. Start a pi session, name it `e2e-p26`
2. Simulate disk full: mount a tiny tmpfs or use `fallocate` to fill the
   partition (this is hard to do safely in practice)
3. Trigger a state save (e.g., the session connects and a topic is created)
4. Check pup logs

**Expected:**
- Pup tries to write `topics_state.json` and gets an I/O error
- Pup logs a warning (`failed to save topics state`)
- Pup continues operating — state persistence is best-effort
- If disk space is restored, the next state save succeeds

---

### Race Conditions & Timing

### P27 — Session connects and disconnects faster than topic creation

The Telegram API call to create a topic takes time. If the session
disconnects before the topic is created, pup must handle the race.

**Steps:**
1. Start pup
2. Start a pi session, name it `e2e-p27`
3. Immediately `/exit` (within 1 second)
4. Wait 15s
5. List topics

**Expected:**
- If the topic was created before the session died: it's immediately
  deleted
- If the session died before the topic API call returned: pup may log a
  warning about deleting a topic for a session that was never fully set up
- Either way: no orphaned topic, no crash

---

### P28 — Two sessions appear simultaneously

Two pi sessions start at exactly the same time (within the same discovery
scan interval).

**Steps:**
1. Start pup
2. In rapid succession (within 1 second):
   ```bash
   tmux -S "$SOCKET" new-window -t e2e -n "pi-p28-a"
   tmux -S "$SOCKET" send-keys -t e2e:pi-p28-a "cd $(mktemp -d) && pi --dangerously-skip-permissions" Enter
   tmux -S "$SOCKET" new-window -t e2e -n "pi-p28-b"
   tmux -S "$SOCKET" send-keys -t e2e:pi-p28-b "cd $(mktemp -d) && pi --dangerously-skip-permissions" Enter
   ```
3. Name them: `/name e2e-p28-a` and `/name e2e-p28-b`
4. Wait 15s

**Expected:**
- Both sessions discovered (in the same or adjacent scan cycles)
- Both get separate topics
- No topic creation race (the outbox serializes API calls)
- Both functional

---

### P29 — Session reset while pup is processing a Telegram update

A Telegram message arrives for a session at the exact moment the session
resets via `/new`.

**Steps:**
1. Start a pi session, name it `e2e-p29`
2. Wait for topic; note the topic ID
3. Send a message via Telegram: `"say RACE_CONDITION"`
4. Simultaneously send `/new` in the pi TUI
5. Wait 15s
6. Check topic history

**Expected:**
- The message may be delivered to the old session (and lost on reset) or
  to the new session (and processed normally)
- Either behavior is acceptable — the key requirement is no crash, no
  topic deletion, and the topic remains functional afterward
- A subsequent message should always work

---

### P30 — Telegram API returns 429 during topic creation

Pup hits Telegram's rate limit while creating topics.

**Steps:**
1. Start 5+ pi sessions in rapid succession (within 5 seconds)
2. Wait for pup to create topics for all of them

**Expected:**
- Some `createForumTopic` calls may get 429 responses
- Pup's outbox handles the retry (respects `Retry-After`)
- All topics are eventually created (within 30-60s)
- No sessions are permanently stuck without a topic

---

### P31 — Discovery scan and session reset overlap

The extension's `socketCheckTimer` recreates the socket at the same time
as a session reset.

**Steps:**
1. Start a pi session, name it `e2e-p31`
2. Wait for topic
3. Delete just the socket file: `rm ~/.pi/pup/*.sock`
4. Immediately send `/new` in the pi TUI
5. Wait 15s

**Expected:**
- The extension detects the missing socket (socketCheckTimer) and
  recreates it
- The `/new` triggers `session_shutdown` + `session_start` which updates
  `savedCtx`
- These two events may interleave, but:
  - The socket is recreated (either by the timer or the existing server
    handles it since only the file was deleted, not the listener)
  - The topic persists (the daemon's IPC connection may or may not break
    depending on whether the socket listener was affected)
- If the IPC connection broke: pup reconnects on the next discovery scan
  and creates/reuses a topic
- If it survived: the topic is the same one, no disruption

---

### Resource Exhaustion

### P32 — Very long agent response (100KB+ text)

**Steps:**
1. Start a pi session, name it `e2e-p32`
2. Wait for topic; note the topic ID
3. Send a prompt that produces a very long response:
   `tg.py send SUPERGROUP TOPIC_ID "generate a list of 1000 random words, one per line"`
4. Wait for completion (up to 120s)
5. Read topic history

**Expected:**
- The response is split across multiple Telegram messages (the
  `split_message` function splits at `MAX_BODY_CHARS = 3500`)
- Each chunk is under the Telegram limit (4096 chars)
- The final content is complete (no truncation unless pi's own output
  is truncated)
- No OOM, no hang

---

### P33 — Session produces thousands of tool calls

**Steps:**
1. Start a pi session, name it `e2e-p33`
2. Wait for topic; note the topic ID
3. Send a prompt that triggers many tool calls:
   `tg.py send SUPERGROUP TOPIC_ID "run echo one, then echo two, ..., run 20 separate echo commands with different numbers"`
4. Wait for completion

**Expected:**
- The turn tracker accumulates all tool calls
- In verbose mode, the Telegram message grows but is truncated/split
  as needed
- In non-verbose mode, only the final text is shown
- No OOM from accumulating too many tool events
- The final message is delivered

---

### P34 — Agent runs for 10+ minutes continuously

A long-running agent turn that exceeds typical timeout expectations.

**Steps:**
1. Start a pi session, name it `e2e-p34`
2. Wait for topic; note the topic ID
3. Send a prompt that takes a very long time (e.g., a complex multi-step
   task)
4. Wait 10+ minutes

**Expected:**
- The typing indicator keeps refreshing (every 4 seconds)
- Message edits continue at the configured interval (1.5s)
- The IPC connection stays alive (no idle timeout)
- The Telegram message is updated throughout
- The final message is delivered when the agent finishes

---

### Adversarial Input

### P35 — Message with Telegram HTML injection

**Steps:**
1. Start a pi session, name it `e2e-p35`
2. Wait for topic; note the topic ID
3. Send a message with HTML-like content:
   `tg.py send SUPERGROUP TOPIC_ID "reply with <b>bold</b> and <script>alert(1)</script>"`
4. Wait for response

**Expected:**
- The message is forwarded to pi literally (including the HTML tags)
- Pi's response may include HTML-like content
- Pup's renderer escapes HTML entities (`<`, `>`, `&`) before sending
  to Telegram
- The Telegram message displays the literal text, not rendered HTML
- No parse_mode error from Telegram

---

### P36 — Message with emoji and Unicode edge cases

**Steps:**
1. Start a pi session, name it `e2e-p36`
2. Wait for topic; note the topic ID
3. Send messages with various Unicode content:
   - `tg.py send SUPERGROUP TOPIC_ID "🤖🎉 emoji test"`
   - `tg.py send SUPERGROUP TOPIC_ID "العربية 中文 日本語"`
   - `tg.py send SUPERGROUP TOPIC_ID "zero-width: ​ joiner: ‍"`
4. Wait for responses

**Expected:**
- All messages forwarded to pi intact
- Responses rendered correctly in Telegram
- No encoding errors, no crashes
- `split_message` doesn't split in the middle of a multi-byte character

---

### P37 — Empty and whitespace-only messages

**Steps:**
1. Start a pi session, name it `e2e-p37`
2. Wait for topic; note the topic ID
3. Send a whitespace-only message: `tg.py send SUPERGROUP TOPIC_ID " "`
4. Send an extremely long message (4000+ chars):
   `tg.py send SUPERGROUP TOPIC_ID "$(python3 -c 'print("A" * 4500)')"`

**Expected:**
- Whitespace-only: forwarded to pi (which may ignore it) or the bot
  handles it gracefully
- Extremely long: forwarded to pi in full (IPC has no message size limit),
  pi processes it normally
- No crash either way

---

### P38 — Slash commands in topics handled by extension

The pup extension intercepts all known pi TUI slash commands via IPC
before they reach `sendUserMessage()`. Commands are either executed
(supported), rejected with a notification (unsupported), or forwarded
to the LLM (unknown).

**Steps:**
1. Start a pi session, name it `e2e-p38`
2. Wait for topic; note the topic ID
3. Send supported commands:
   - `tg.py send SUPERGROUP TOPIC_ID "/name test-name-p38"`
     → should rename session and topic
4. Wait 10s; verify topic renamed: `tg.py topics SUPERGROUP`
5. Send unsupported commands:
   - `tg.py send SUPERGROUP TOPIC_ID "/tree"`
   - `tg.py send SUPERGROUP TOPIC_ID "/settings"`
6. Wait 5s; read history: `tg.py history SUPERGROUP TOPIC_ID`
7. Send unknown command:
   - `tg.py send SUPERGROUP TOPIC_ID "/unknowncommand"`
8. Wait for LLM response
9. Send `/cancel@my_pup_bot` (with @bot suffix)

**Expected:**
- `/name test-name-p38` → extension calls `pi.setSessionName()`, topic
  renamed to contain `test-name-p38`
- `/tree` → notification with `not available via remote access` appears
  in the topic; NOT forwarded to LLM
- `/settings` → notification with `not available via remote access`;
  NOT forwarded to LLM
- `/unknowncommand` → NOT intercepted by extension, forwarded to LLM
  via `sendUserMessage()`, LLM responds
- `/cancel@my_pup_bot` → `@bot` suffix stripped, recognized as `/cancel`,
  handled by the Telegram backend directly (abort sent via IPC)
- No crash, correct routing

---

### P39 — Message with >> prefix edge cases

**Steps:**
1. Start a pi session, name it `e2e-p39`
2. Wait for topic; note the topic ID
3. Send follow-up edge cases:
   - `tg.py send SUPERGROUP TOPIC_ID ">>"` (empty follow-up)
   - `tg.py send SUPERGROUP TOPIC_ID ">> "` (whitespace follow-up)
   - `tg.py send SUPERGROUP TOPIC_ID ">>>triple"` (triple >)
   - `tg.py send SUPERGROUP TOPIC_ID "> single"` (single > — NOT follow-up)

**Expected:**
- `>>` with no content: forwarded as follow-up with empty text (pi may
  ignore it)
- `>> ` with whitespace: forwarded as follow-up with empty/whitespace text
- `>>>triple`: the first `>>` is stripped, `>triple` is sent as follow-up
- `> single`: NOT a follow-up — single `>` doesn't trigger follow-up mode,
  sent as a normal steer message with the full text `> single`
- No crash on any of these

---

## Rate Limiting & Outbox Tests

These tests verify that the outbox rate limiter prevents Telegram 429
errors when multiple sessions stream simultaneously in the same
supergroup. The outbox uses a per-chat token bucket (18 ops/min, smooth
refill at 0.3 tokens/sec) with edit coalescing so stale edits never
waste tokens.

**Key dynamics:**

- All topics in a supergroup share one `chat_id`, so they share one
  token bucket. Telegram's group limit is 20 messages/min; the budget
  of 18/min leaves headroom.
- The token bucket starts full (18 tokens). With 3 sessions, the first
  ~10 seconds have fluid streaming (initial burst), then steady state
  settles to ~6 edits/min per session (~1 every 10s).
- Edit coalescing ensures that when the budget blocks edits, only the
  latest content per message is kept. When a token becomes available,
  it sends current content — never stale text from seconds ago.
- The `flush_one()` loop skips budget-blocked entries to avoid
  head-of-line blocking: if session A's chat is over budget but session
  B's isn't, session B's operations still go through.

### RL01 — Three concurrent sessions produce no 429 errors

The primary scenario that caused the original bug: 3 sessions streaming
in the same supergroup, generating ~120 edits/min collectively, exceeding
Telegram's 20/min group limit.

**Steps:**
1. Start three pi sessions: `e2e-rl01-a`, `e2e-rl01-b`, `e2e-rl01-c`
2. Wait for all three topics to appear; note their topic IDs
3. Send long prompts to all three simultaneously (within 2 seconds):
   ```bash
   tg.py send SUPERGROUP TOPIC_A "write a detailed essay about the history of the internet"
   tg.py send SUPERGROUP TOPIC_B "write a detailed essay about the history of programming languages"
   tg.py send SUPERGROUP TOPIC_C "write a detailed essay about the history of operating systems"
   ```
4. Wait for all three agents to finish (up to 120s)
5. Grep pup logs for `rate limited` or `429`:
   ```bash
   tmux -S "$SOCKET" capture-pane -p -t e2e:pup -S -500 | grep -i "rate.limit\|429"
   ```
6. Read all three topic histories:
   ```bash
   tg.py history SUPERGROUP TOPIC_A
   tg.py history SUPERGROUP TOPIC_B
   tg.py history SUPERGROUP TOPIC_C
   ```

**Expected:**
- No `rate limited` or `429` entries in pup logs
- All three topics have complete bot responses (the final edit for each
  session contains the full essay text)
- No cross-contamination between topics
- The total number of API calls logged is ≤ 18/min sustained (may burst
  higher in the first ~10 seconds from the initial token fill)

---

### RL02 — All sessions receive updates during concurrent streaming

Verify that no session is starved — each session's topic gets at least
some edits during streaming, even under rate pressure.

**Steps:**
1. Start three pi sessions: `e2e-rl02-a`, `e2e-rl02-b`, `e2e-rl02-c`
2. Wait for all three topics
3. Start pup with `RUST_LOG=debug` to capture per-operation outbox logs
4. Send long prompts to all three simultaneously
5. While agents are streaming (after ~15s), read each topic:
   ```bash
   tg.py history SUPERGROUP TOPIC_A
   tg.py history SUPERGROUP TOPIC_B
   tg.py history SUPERGROUP TOPIC_C
   ```
6. Wait for all agents to finish
7. Read final topic histories
8. Count `outbox_flush` log entries per topic to verify distribution:
   ```bash
   tmux -S "$SOCKET" capture-pane -p -t e2e:pup -S -2000 | grep "outbox_flush" | wc -l
   ```

**Expected:**
- After 15s of concurrent streaming (past the initial burst), each
  topic's bot message contains _some_ streaming content — no session
  is completely starved
- The final histories for all three topics contain complete responses
- The outbox log shows operations for all three sessions (not just one
  monopolizing the budget)
- The `pending_edits` FIFO rotates fairly: roughly equal edit counts
  per session in the logs

---

### RL03 — Finished session's final edit delivered promptly

When one session finishes while others are still streaming, its final
edit (removing the cancel keyboard and showing complete content) must
go through without being blocked behind the other sessions' edits.

**Steps:**
1. Start three pi sessions: `e2e-rl03-a`, `e2e-rl03-b`, `e2e-rl03-c`
2. Wait for all three topics; note their topic IDs
3. Send a **short** prompt to session A:
   `tg.py send SUPERGROUP TOPIC_A "reply with only the word KIWI"`
4. Simultaneously send **long** prompts to B and C:
   ```bash
   tg.py send SUPERGROUP TOPIC_B "write a very detailed essay about databases"
   tg.py send SUPERGROUP TOPIC_C "write a very detailed essay about compilers"
   ```
5. Wait for session A's response:
   `tg.py wait SUPERGROUP --topic TOPIC_A --contains KIWI --timeout 30`
6. Note how long session A took to show its final response
7. Check session A's message in the topic — verify the cancel keyboard
   is removed (no inline keyboard, or empty buttons list)

**Expected:**
- Session A finishes within ~10s (a trivial response)
- Session A's final edit goes through promptly — not delayed until B
  and C finish streaming. The `end_turn` path calls
  `clear_edit_cooldown()` and enqueues the final edit, which gets the
  next available token.
- The final message for session A has no cancel keyboard (`empty_keyboard`)
- Sessions B and C continue streaming unaffected

---

### RL04 — Edit coalescing under rate pressure

Verify that when the budget is exhausted and multiple edit updates
accumulate, only the latest content is shown — no stale intermediate
edits appear in the topic.

**Steps:**
1. Start three pi sessions: `e2e-rl04-a`, `e2e-rl04-b`, `e2e-rl04-c`
2. Wait for all three topics
3. Send long prompts to all three simultaneously
4. Wait for all agents to finish (up to 120s)
5. Read each topic's final message content
6. Check pup logs for the edit count:
   ```bash
   tmux -S "$SOCKET" capture-pane -p -t e2e:pup -S -2000 | grep "editMessageText" | wc -l
   ```

**Expected:**
- The total `editMessageText` calls over the entire run is well below
  what it would be without rate limiting (3 sessions × 40 edits/min =
  120/min → should be capped at ~18/min sustained)
- Each topic's final message contains the **complete** response — no
  truncation from a stale intermediate edit surviving as the final state
- The edit count per session is roughly equal (round-robin fairness)

**Note:** Edit coalescing is invisible to the user — they just see
fewer intermediate updates. The key property is that every update that
_does_ go through shows the latest accumulated text, not text from
10 seconds ago. If coalescing were broken, you'd see a final message
that's missing the last few paragraphs (because a stale edit was sent
after the fresh one).

---

### RL05 — Sends take priority over edits under rate pressure

When a new session starts (triggering a `sendMessage`) while other
sessions are streaming (generating edits), the send should go through
first because sends have higher priority in the outbox heap.

**Steps:**
1. Start two pi sessions: `e2e-rl05-a`, `e2e-rl05-b`
2. Wait for both topics
3. Send long prompts to both to saturate the budget:
   ```bash
   tg.py send SUPERGROUP TOPIC_A "write a very long detailed essay about networks"
   tg.py send SUPERGROUP TOPIC_B "write a very long detailed essay about cryptography"
   ```
4. Wait 15s (past the initial burst, budget should be tight)
5. Start a third pi session: `e2e-rl05-c`
6. Wait for session C's topic to appear; note how long it takes:
   ```bash
   for i in $(seq 1 30); do
     sleep 1
     tg.py topics SUPERGROUP | grep -q "rl05-c" && break
   done
   ```
7. Send a message to session C's topic to verify it works

**Expected:**
- Session C's topic is created within a few seconds of the session
  starting — the `sendMessage` for the initial turn message gets
  priority over the pending edits for sessions A and B
- The `sendMessage` consumes a token from the shared bucket, but it
  goes through ahead of queued edits (Send priority=3 > Edit priority=1)
- Session C responds to messages normally

---

### RL06 — Single session does not hit rate limits

Baseline test: a single session streaming alone should never trigger
rate limiting (1 edit per 1.5s = 40/min, but the budget is 18/min, so
the bucket eventually throttles — verify this produces smooth updates,
not 429 errors).

**Steps:**
1. Start one pi session: `e2e-rl06`
2. Wait for topic; note the topic ID
3. Send a long prompt:
   `tg.py send SUPERGROUP TOPIC_ID "write a comprehensive guide to rust programming, covering ownership, borrowing, lifetimes, traits, and async"`
4. Wait for the agent to finish (up to 120s)
5. Check pup logs for 429 errors
6. Read the topic history

**Expected:**
- No 429 errors in pup logs
- The response is complete in the topic
- The initial ~50 seconds have frequent updates (~1 per 1.5s, consuming
  the initial 18-token burst plus refill)
- After the burst, updates slow to ~1 per 3.3s (token refill rate)
- The user sees a smooth slowdown, not a sudden freeze followed by a
  burst (this is the token bucket advantage over the old sliding window)

---

### RL07 — Budget recovers after sessions finish

After concurrent sessions finish, the token bucket should refill to
capacity. A new session starting afterward should have the full burst
budget available.

**Steps:**
1. Start three pi sessions and send prompts to all three (same as RL01)
2. Wait for all three to finish
3. Wait 60 seconds (full bucket refill: 18 tokens at 0.3/sec = 60s)
4. Start a new pi session: `e2e-rl07-fresh`
5. Wait for topic; send a long prompt
6. Observe the streaming updates

**Expected:**
- The new session streams with the full initial burst (updates every
  1.5s for the first ~50s)
- No residual rate limiting from the previous sessions
- The token bucket has refilled to capacity (18 tokens)
- No 429 errors

---

### RL08 — Delete operations go through during edit pressure

When a session finishes and its topic is cleaned up (or a session exits),
the `deleteMessage` operation should go through even if edits for other
sessions are pending. Deletes have priority 2 (between Send=3 and Edit=1).

**Steps:**
1. Start three pi sessions: `e2e-rl08-a`, `e2e-rl08-b`, `e2e-rl08-c`
2. Wait for all three topics
3. Send long prompts to all three
4. While A and B are streaming, exit session C:
   ```bash
   tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-rl08-c C-c
   sleep 0.5
   tmux -S "$SOCKET" send-keys -t e2e:pi-e2e-rl08-c C-d
   ```
5. Wait up to 45s (30s grace period + buffer)
6. List topics: `tg.py topics SUPERGROUP`

**Expected:**
- Session C's topic is deleted after the grace period despite A and B
  still actively streaming
- The delete operation consumed a token from the shared bucket but
  went through with priority over pending edits
- Sessions A and B continue streaming (their edits resume after the
  delete)

---

### RL09 — No head-of-line blocking across different chats

This test only applies when sessions are in **different chats** (e.g.,
one in a supergroup and one in a DM). Verify that an over-budget chat
doesn't block operations for other chats.

**Steps:**
1. Configure pup with both DM mode and topics mode enabled
2. Start two pi sessions: `e2e-rl09-topic` and `e2e-rl09-dm`
3. Wait for `e2e-rl09-topic`'s topic in the supergroup
4. Attach to `e2e-rl09-dm` via DM: `/attach e2e-rl09-dm`
5. Send long prompts to both:
   - Topic: `tg.py send SUPERGROUP TOPIC_ID "write a long essay about AI"`
   - DM: `tg.py send DM_CHAT_ID "write a long essay about ML"`
6. Wait for both to finish
7. Check pup logs for 429 errors

**Expected:**
- The supergroup and DM have **separate** token buckets (different
  `chat_id`s)
- Both streams update independently — the supergroup being at its
  budget limit doesn't block DM edits, and vice versa
- No 429 errors
- Both responses are complete

**Note:** This tests the `flush_one()` head-of-line blocking fix. The
old code would pop the first entry from the heap, check budget, and if
blocked, push it back and return `false` — blocking ALL operations. The
fix scans past budget-blocked entries to find one that can go through.

---

### RL10 — Outbox drains completely at end of turn

When an agent finishes, the `AgentEnd` handler runs
`while self.outbox.flush_one().await {}` to drain remaining operations.
Verify this completes — the final edit and any overflow sends all go
through.

**Steps:**
1. Start a pi session: `e2e-rl10`
2. Wait for topic; note the topic ID
3. Send a prompt that produces a **very long** response (longer than
   one Telegram message):
   `tg.py send SUPERGROUP TOPIC_ID "generate a numbered list from 1 to 500"`
4. Wait for the agent to finish (up to 120s)
5. Read the topic history:
   `tg.py history SUPERGROUP TOPIC_ID`
6. Count the bot messages in the topic

**Expected:**
- The response is split across multiple Telegram messages (each under
  4096 chars / 3500 body chars)
- All overflow chunks are delivered (the `end_turn` code enqueues
  overflow `Send` operations, and the drain loop flushes them all)
- The first message has no cancel keyboard (final edit removed it)
- Overflow messages have no keyboard
- The numbers go from 1 through 500 across all chunks (no gaps from
  dropped overflow sends)

---

### RL11 — Rapid short prompts don't exhaust budget permanently

If a user sends many short prompts in quick succession (each producing
a fast response), the budget should recover between bursts.

**Steps:**
1. Start a pi session: `e2e-rl11`
2. Wait for topic; note the topic ID
3. Send 10 short prompts in quick succession (1 second apart):
   ```bash
   for i in $(seq 1 10); do
     tg.py send SUPERGROUP TOPIC_ID "reply with only the number $i"
     sleep 1
   done
   ```
4. Wait for all responses (up to 60s)
5. Verify all 10 responses are in the topic:
   `tg.py history SUPERGROUP TOPIC_ID`
6. Check pup logs for 429 errors

**Expected:**
- The first several responses arrive quickly (initial burst budget)
- Later responses may be slightly delayed as the budget tightens
- All 10 responses eventually appear in the topic
- No 429 errors
- Each response appears as a separate turn (a new bot message per
  agent turn, not edits to the same message)
- The content is correct: responses contain the numbers 1 through 10
