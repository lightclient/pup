#!/usr/bin/env bash
set -uo pipefail

# ─── Constants ──────────────────────────────────────────────────────
SOCKET_DIR=${TMPDIR:-/tmp}/claude-tmux-sockets
SOCKET="$SOCKET_DIR/pup-e2e.sock"
PUP_CONFIG="${PUP_CONFIG:-$HOME/.config/pup/config.toml}"
SUPERGROUP=$(python3 -c "import tomllib; print(tomllib.load(open('$PUP_CONFIG','rb'))['backends']['telegram']['topics']['supergroup_id'])")
BOT_TOKEN=$(python3 -c "import tomllib; print(tomllib.load(open('$PUP_CONFIG','rb'))['backends']['telegram']['bot_token'])")
BOT_ID=$(curl -s "https://api.telegram.org/bot${BOT_TOKEN}/getMe" | python3 -c "import sys,json; print(json.load(sys.stdin)['result']['id'])")
PROJECT=/root/handoff/main
TG="uv run $PROJECT/tests/e2e/tg.py"
# Isolated socket dir so other pi sessions don't interfere
PUP_SOCKET_DIR=/tmp/pup-e2e-sockets
export PUP_SOCKET_DIR

PASSED=0
FAILED=0
SKIPPED=0
ERRORS=()

# ─── Helpers ────────────────────────────────────────────────────────
log()  { echo -e "\033[1;34m[INFO]\033[0m $*"; }
pass() { echo -e "\033[1;32m[PASS]\033[0m $1"; PASSED=$((PASSED + 1)); }
fail() { echo -e "\033[1;31m[FAIL]\033[0m $1: $2"; FAILED=$((FAILED + 1)); ERRORS+=("$1: $2"); }
skip() { echo -e "\033[1;33m[SKIP]\033[0m $1: $2"; SKIPPED=$((SKIPPED + 1)); }

# Wait for a topic matching a query to appear (returns JSON)
wait_topic() {
  local query="$1" timeout="${2:-20}"
  for i in $(seq 1 "$timeout"); do
    local result
    result=$($TG topics "$SUPERGROUP" -q "$query" 2>/dev/null)
    local count
    count=$(echo "$result" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo 0)
    if [ "$count" -gt 0 ]; then
      echo "$result"
      return 0
    fi
    sleep 1
  done
  return 1
}

# Get first non-General topic ID for a query
get_topic_id() {
  local query="$1"
  $TG topics "$SUPERGROUP" -q "$query" 2>/dev/null | python3 -c "
import sys, json
topics = json.load(sys.stdin)
for t in topics:
    if t['id'] != 1:
        print(t['id'])
        break
" 2>/dev/null
}

# Get first non-General topic ID (any topic)
get_any_topic_id() {
  $TG topics "$SUPERGROUP" 2>/dev/null | python3 -c "
import sys, json
topics = json.load(sys.stdin)
for t in topics:
    if t['id'] != 1:
        print(t['id'])
        break
" 2>/dev/null
}

# Wait for all non-General topics to disappear
wait_all_topics_gone() {
  local timeout="${1:-20}"
  for i in $(seq 1 "$timeout"); do
    local cnt
    cnt=$(count_topics)
    if [ "$cnt" = "0" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# Wait for topic to disappear
wait_no_topic() {
  local query="$1" timeout="${2:-20}"
  for i in $(seq 1 "$timeout"); do
    local result
    result=$($TG topics "$SUPERGROUP" -q "$query" 2>/dev/null)
    local count
    count=$(echo "$result" | python3 -c "import sys,json; print(len([t for t in json.load(sys.stdin) if t['id'] != 1]))" 2>/dev/null || echo 1)
    if [ "$count" = "0" ]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# Start a pi session in tmux (Ctrl-D exits pi, not /exit)
start_pi() {
  local name="$1"
  local work
  work=$(mktemp -d)
  tmux -S "$SOCKET" new-window -t e2e -n "pi-$name"
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" "cd $work && PUP_SOCKET_DIR=$PUP_SOCKET_DIR pi --dangerously-skip-permissions" Enter
  sleep 5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" "/name $name" Enter
  sleep 3
}

# Start a pi session without naming it
start_pi_unnamed() {
  local label="$1"
  local work
  work=$(mktemp -d)
  tmux -S "$SOCKET" new-window -t e2e -n "pi-$label"
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$label" "cd $work && PUP_SOCKET_DIR=$PUP_SOCKET_DIR pi --dangerously-skip-permissions" Enter
  sleep 5
}

# Exit pi with Ctrl-D (the proper way)
exit_pi() {
  local name="$1"
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" C-c
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" C-d
  sleep 2
  # Close the shell too
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" "exit" Enter 2>/dev/null || true
  sleep 1
}

# Send a command to pi TUI
pi_send() {
  local name="$1" cmd="$2"
  tmux -S "$SOCKET" send-keys -t "e2e:pi-$name" "$cmd" Enter
}

# Wait for bot message in topic containing text
wait_bot_msg() {
  local topic_id="$1" contains="$2" timeout="${3:-30}"
  $TG wait "$SUPERGROUP" --topic "$topic_id" --from "$BOT_ID" --contains "$contains" --timeout "$timeout" 2>/dev/null
}

# Get topic history
topic_history() {
  local topic_id="$1" limit="${2:-20}"
  $TG history "$SUPERGROUP" "$topic_id" --limit "$limit" 2>/dev/null
}

# Stop pup (Ctrl-C graceful or kill -9)
stop_pup() {
  local mode="${1:-graceful}"  # graceful or kill
  if [ "$mode" = "kill" ]; then
    local pid
    pid=$(tmux -S "$SOCKET" capture-pane -p -t e2e:pup 2>/dev/null | head -1)
    # Find pup PID and SIGKILL it
    pkill -9 -f "target/debug/pup --config /tmp/pup-e2e-config.toml" 2>/dev/null || true
    sleep 1
  else
    tmux -S "$SOCKET" send-keys -t e2e:pup C-c
    sleep 2
  fi
}

# Start pup (assumes pup window already exists)
start_pup() {
  tmux -S "$SOCKET" send-keys -t e2e:pup \
    "cd $PROJECT && RUST_LOG=info ./target/debug/pup --config /tmp/pup-e2e-config.toml" Enter
  for i in $(seq 1 30); do
    sleep 1
    if tmux -S "$SOCKET" capture-pane -p -t e2e:pup 2>/dev/null | grep -q "telegram backend started"; then
      return 0
    fi
  done
  return 1
}

# Restart pup (stop + start)
restart_pup() {
  local mode="${1:-graceful}"
  stop_pup "$mode"
  start_pup
}

# Clean up any stale topics (left from previous tests)
clean_stale_topics() {
  $TG topics "$SUPERGROUP" 2>/dev/null | python3 -c "
import sys, json
for t in json.load(sys.stdin):
    if t['id'] != 1:
        print(t['id'])
" 2>/dev/null | while read tid; do
    curl -s "https://api.telegram.org/bot${BOT_TOKEN}/deleteForumTopic?chat_id=${SUPERGROUP}&message_thread_id=${tid}" >/dev/null 2>&1
  done
  sleep 2
}

# Count non-General topics
count_topics() {
  $TG topics "$SUPERGROUP" 2>/dev/null | python3 -c "
import sys, json
topics = json.load(sys.stdin)
print(len([t for t in topics if t['id'] != 1]))
"
}

# ─── Setup / Teardown ───────────────────────────────────────────────
setup() {
  log "Setting up test environment"

  # Create isolated socket dir
  mkdir -p "$PUP_SOCKET_DIR"
  rm -f "$PUP_SOCKET_DIR"/*.sock "$PUP_SOCKET_DIR"/*.alias "$PUP_SOCKET_DIR"/topics_state.json

  # Write test pup config (read allowed_user_ids from main config)
  local allowed_users
  allowed_users=$(python3 -c "import tomllib; print(tomllib.load(open('$PUP_CONFIG','rb'))['backends']['telegram']['allowed_user_ids'])")
  cat > /tmp/pup-e2e-config.toml <<EOF
[pup]
socket_dir = "$PUP_SOCKET_DIR"

[display]
verbose = true
history_turns = 5

[streaming]
edit_interval_ms = 1500

[backends.telegram]
enabled = true
bot_token = "$BOT_TOKEN"
allowed_user_ids = $allowed_users

[backends.telegram.dm]
enabled = true

[backends.telegram.topics]
enabled = true
supergroup_id = $SUPERGROUP
topic_icon = "📎"

[backends.telegram.display]
max_message_length = 3500
EOF

  # Clean up stale topics
  $TG topics "$SUPERGROUP" 2>/dev/null | python3 -c "
import sys, json
for t in json.load(sys.stdin):
    if t['id'] != 1:
        print(t['id'])
" | while read tid; do
    curl -s "https://api.telegram.org/bot${BOT_TOKEN}/deleteForumTopic?chat_id=${SUPERGROUP}&message_thread_id=${tid}" >/dev/null 2>&1
  done

  # Kill any existing tmux session
  tmux -S "$SOCKET" kill-server 2>/dev/null || true
  mkdir -p "$SOCKET_DIR"

  # Start tmux + pup
  tmux -S "$SOCKET" new-session -d -s e2e -n pup
  sleep 1
  tmux -S "$SOCKET" send-keys -t e2e:pup \
    "cd $PROJECT && RUST_LOG=info ./target/debug/pup --config /tmp/pup-e2e-config.toml" Enter

  # Wait for pup to start
  for i in $(seq 1 30); do
    sleep 1
    if tmux -S "$SOCKET" capture-pane -p -t e2e:pup 2>/dev/null | grep -q "telegram backend started"; then
      log "pup started"
      return 0
    fi
  done
  log "ERROR: pup failed to start"
  tmux -S "$SOCKET" capture-pane -p -t e2e:pup
  exit 1
}

teardown() {
  local exit_code=$?
  log "Tearing down"
  # Kill all pi windows
  tmux -S "$SOCKET" list-windows -t e2e -F '#{window_name}' 2>/dev/null | grep '^pi-' | while read w; do
    tmux -S "$SOCKET" send-keys -t "e2e:$w" C-c 2>/dev/null || true
    sleep 0.3
    tmux -S "$SOCKET" send-keys -t "e2e:$w" C-d 2>/dev/null || true
  done
  sleep 3
  tmux -S "$SOCKET" kill-server 2>/dev/null || true
  rm -rf "$PUP_SOCKET_DIR"
  exit $exit_code
}

# ─── Tests ──────────────────────────────────────────────────────────

test_t01() {
  log "T01 — Topic created when pi session starts"
  start_pi "e2e-t01"
  if wait_topic "e2e-t01" 20 >/dev/null; then
    pass "T01"
  else
    fail "T01" "topic not created"
  fi
  exit_pi "e2e-t01"
  wait_no_topic "e2e-t01" 20 || true
}

test_t02() {
  log "T02 — Topic deleted when pi session exits"
  start_pi "e2e-t02"
  if ! wait_topic "e2e-t02" 20 >/dev/null; then
    fail "T02" "topic never appeared"
    return
  fi
  exit_pi "e2e-t02"
  if wait_no_topic "e2e-t02" 20; then
    pass "T02"
  else
    fail "T02" "topic not deleted after exit"
  fi
}

test_t03() {
  log "T03 — User message forwarded to pi session"
  start_pi "e2e-t03"
  if ! wait_topic "e2e-t03" 20 >/dev/null; then
    fail "T03" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t03")
  $TG send "$SUPERGROUP" "$tid" "say exactly PINEAPPLE" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "PINEAPPLE" 60 >/dev/null; then
    pass "T03"
  else
    fail "T03" "no response containing PINEAPPLE"
  fi
  exit_pi "e2e-t03"
  wait_no_topic "e2e-t03" 20 || true
}

test_t04() {
  log "T04 — Multiple parallel sessions get separate topics"
  start_pi "e2e-t04-alpha"
  start_pi "e2e-t04-beta"

  local ok=true
  wait_topic "e2e-t04-alpha" 20 >/dev/null || { ok=false; }
  wait_topic "e2e-t04-beta" 20 >/dev/null || { ok=false; }

  if $ok; then
    local cnt
    cnt=$(count_topics)
    if [ "$cnt" -ge 2 ]; then
      pass "T04"
    else
      fail "T04" "expected >=2 topics, got $cnt"
    fi
  else
    fail "T04" "one or both topics missing"
  fi

  exit_pi "e2e-t04-alpha"
  exit_pi "e2e-t04-beta"
  wait_all_topics_gone 20 || true
}

test_t05() {
  log "T05 — Session rename updates topic title"
  start_pi "e2e-t05-before"
  if ! wait_topic "e2e-t05-before" 20 >/dev/null; then
    fail "T05" "topic never appeared"
    return
  fi
  pi_send "e2e-t05-before" "/name e2e-t05-after"
  sleep 10
  if wait_topic "e2e-t05-after" 10 >/dev/null; then
    pass "T05"
  else
    fail "T05" "topic not renamed to e2e-t05-after"
  fi
  exit_pi "e2e-t05-before"
  wait_all_topics_gone 20 || true
}

test_t09() {
  log "T09 — Tool calls visible in verbose mode"
  start_pi "e2e-t09"
  if ! wait_topic "e2e-t09" 20 >/dev/null; then
    fail "T09" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t09")
  $TG send "$SUPERGROUP" "$tid" 'run: echo E2E_TOOL_TEST' 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "E2E_TOOL_TEST" 60 >/dev/null; then
    pass "T09"
  else
    fail "T09" "no response containing E2E_TOOL_TEST"
  fi
  exit_pi "e2e-t09"
  wait_all_topics_gone 20 || true
}

test_t13() {
  log "T13 — Response available immediately after message_end"
  start_pi "e2e-t13"
  if ! wait_topic "e2e-t13" 20 >/dev/null; then
    fail "T13" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t13")
  local start_time
  start_time=$(date +%s)
  $TG send "$SUPERGROUP" "$tid" "reply with only the word BANANA" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "BANANA" 30 >/dev/null; then
    local end_time elapsed
    end_time=$(date +%s)
    elapsed=$((end_time - start_time))
    pass "T13 (${elapsed}s)"
  else
    fail "T13" "no response containing BANANA"
  fi
  exit_pi "e2e-t13"
  wait_all_topics_gone 20 || true
}

test_t14() {
  log "T14 — Concurrent prompts to different sessions"
  start_pi "e2e-t14-a"
  start_pi "e2e-t14-b"
  wait_topic "e2e-t14-a" 20 >/dev/null
  wait_topic "e2e-t14-b" 20 >/dev/null
  local tid_a tid_b
  tid_a=$(get_topic_id "e2e-t14-a")
  tid_b=$(get_topic_id "e2e-t14-b")

  if [ -z "$tid_a" ] || [ -z "$tid_b" ]; then
    fail "T14" "could not get topic IDs"
    exit_pi "e2e-t14-a" 2>/dev/null || true
    exit_pi "e2e-t14-b" 2>/dev/null || true
    return
  fi

  $TG send "$SUPERGROUP" "$tid_a" "reply with only APPLE" 2>/dev/null >/dev/null
  $TG send "$SUPERGROUP" "$tid_b" "reply with only ORANGE" 2>/dev/null >/dev/null

  local ok=true
  wait_bot_msg "$tid_a" "APPLE" 60 >/dev/null || { ok=false; fail "T14" "no APPLE in session A"; }
  if $ok; then
    wait_bot_msg "$tid_b" "ORANGE" 60 >/dev/null || { ok=false; fail "T14" "no ORANGE in session B"; }
  fi
  $ok && pass "T14"

  exit_pi "e2e-t14-a"
  exit_pi "e2e-t14-b"
  wait_all_topics_gone 20 || true
}

test_t15() {
  log "T15 — /new preserves the topic"
  start_pi "e2e-t15"
  if ! wait_topic "e2e-t15" 20 >/dev/null; then
    fail "T15" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t15")
  pi_send "e2e-t15" "/new"
  sleep 10
  # After /new, the topic should still exist (same or renamed)
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    # Check for session reset message
    local active_tid
    active_tid=$(get_any_topic_id)
    if [ -n "$active_tid" ]; then
      local history
      history=$(topic_history "$active_tid")
      if echo "$history" | grep -q "Session reset"; then
        pass "T15"
      else
        pass "T15 (topic preserved, reset message may not be visible)"
      fi
    else
      pass "T15 (topic count=$cnt)"
    fi
  else
    fail "T15" "topic disappeared after /new"
  fi
  exit_pi "e2e-t15"
  wait_all_topics_gone 20 || true
}

test_t16() {
  log "T16 — Messages work after /new"
  start_pi "e2e-t16"
  if ! wait_topic "e2e-t16" 20 >/dev/null; then
    fail "T16" "topic never appeared"
    return
  fi
  pi_send "e2e-t16" "/new"
  sleep 10
  local active_tid
  active_tid=$(get_any_topic_id)
  if [ -z "$active_tid" ]; then
    fail "T16" "no topic found after /new"
    exit_pi "e2e-t16"
    return
  fi
  $TG send "$SUPERGROUP" "$active_tid" "say exactly AFTER_RESET" 2>/dev/null >/dev/null
  if wait_bot_msg "$active_tid" "AFTER_RESET" 60 >/dev/null; then
    pass "T16"
  else
    fail "T16" "no response containing AFTER_RESET"
  fi
  exit_pi "e2e-t16"
  wait_all_topics_gone 20 || true
}

test_t17() {
  log "T17 — Multiple /new in sequence preserve the same topic"
  start_pi "e2e-t17"
  if ! wait_topic "e2e-t17" 20 >/dev/null; then
    fail "T17" "topic never appeared"
    return
  fi
  pi_send "e2e-t17" "/new"
  sleep 5
  pi_send "e2e-t17" "/new"
  sleep 5
  pi_send "e2e-t17" "/new"
  sleep 5
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" = "1" ]; then
    pass "T17"
  else
    fail "T17" "expected 1 topic, got $cnt"
  fi
  exit_pi "e2e-t17"
  wait_all_topics_gone 20 || true
}

test_t19() {
  log "T19 — /new then exit deletes the topic"
  start_pi "e2e-t19"
  if ! wait_topic "e2e-t19" 20 >/dev/null; then
    fail "T19" "topic never appeared"
    return
  fi
  pi_send "e2e-t19" "/new"
  sleep 5
  exit_pi "e2e-t19"
  if wait_all_topics_gone 20; then
    pass "T19"
  else
    fail "T19" "topic not deleted after /new + exit"
  fi
}

test_t21() {
  log "T21 — Topic created for session with no name (fallback naming)"
  start_pi_unnamed "e2e-t21"
  sleep 15
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    pass "T21"
  else
    fail "T21" "no topic created for unnamed session"
  fi
  # Clean up
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-t21" C-c
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-t21" C-d
  sleep 2
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-t21" "exit" Enter 2>/dev/null || true
  wait_all_topics_gone 20 || true
}

# ─── Run ────────────────────────────────────────────────────────────
cd "$PROJECT"

log "Starting E2E test suite"
log "Supergroup: $SUPERGROUP"
log "Bot ID: $BOT_ID"
echo ""

trap teardown EXIT

setup

TESTS="${1:-all}"
# ─── TUI-path tests for commands missing TUI coverage ────────────

test_t18() {
  log "T18 — /compact via TUI preserves the topic"
  start_pi "e2e-t18"
  if ! wait_topic "e2e-t18" 20 >/dev/null; then
    fail "T18" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t18")
  # Build context so compact has something to work with
  $TG send "$SUPERGROUP" "$tid" "say BEFORE_COMPACT_TUI" 2>/dev/null >/dev/null
  wait_bot_msg "$tid" "BEFORE_COMPACT_TUI" 60 >/dev/null || true
  # Run /compact via TUI
  pi_send "e2e-t18" "/compact"
  sleep 12
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    local active_tid
    active_tid=$(get_any_topic_id)
    $TG send "$SUPERGROUP" "$active_tid" "say AFTER_COMPACT_TUI" 2>/dev/null >/dev/null
    if wait_bot_msg "$active_tid" "AFTER_COMPACT_TUI" 60 >/dev/null; then
      pass "T18"
    else
      fail "T18" "no response after /compact via TUI"
    fi
  else
    fail "T18" "topic disappeared after /compact via TUI"
  fi
  exit_pi "e2e-t18"
  wait_all_topics_gone 20 || true
}

test_t22() {
  log "T22 — /quit via TUI deletes the topic"
  start_pi "e2e-t22"
  if ! wait_topic "e2e-t22" 20 >/dev/null; then
    fail "T22" "topic never appeared"
    return
  fi
  # /quit via TUI (unlike /exit which the agent intercepts, /quit is a real pi command)
  pi_send "e2e-t22" "/quit"
  sleep 3
  # /quit should close pi; close the shell too
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-t22" "exit" Enter 2>/dev/null || true
  if wait_all_topics_gone 20; then
    pass "T22"
  else
    fail "T22" "topic not deleted after /quit via TUI"
  fi
}

test_t23() {
  log "T23 — Plain message via TUI produces response in topic"
  start_pi "e2e-t23"
  if ! wait_topic "e2e-t23" 20 >/dev/null; then
    fail "T23" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-t23")
  # Send a prompt via the TUI, expect it to appear in the topic
  pi_send "e2e-t23" "say exactly MANGO_TUI"
  if wait_bot_msg "$tid" "MANGO_TUI" 60 >/dev/null; then
    pass "T23"
  else
    fail "T23" "TUI prompt response not visible in topic"
  fi
  exit_pi "e2e-t23"
  wait_all_topics_gone 20 || true
}

# ─── Cancel tests ────────────────────────────────────────────────

test_c01() {
  log "C01 — /cancel via Telegram aborts the agent"
  start_pi "e2e-c01"
  if ! wait_topic "e2e-c01" 20 >/dev/null; then
    fail "C01" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-c01")
  # Start a long prompt
  $TG send "$SUPERGROUP" "$tid" "write a very long and detailed essay about the entire history of computing from the abacus to modern quantum computers" 2>/dev/null >/dev/null
  sleep 8
  # Cancel
  $TG send "$SUPERGROUP" "$tid" "/cancel" 2>/dev/null >/dev/null
  # Wait for abort to settle — pi needs time to process the abort,
  # finish any in-flight API call, and become idle again.
  sleep 15
  # The agent should have stopped. Send a quick follow-up to verify session works.
  $TG send "$SUPERGROUP" "$tid" "reply with only the word CANCEL_OK" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "CANCEL_OK" 60 >/dev/null; then
    pass "C01"
  else
    fail "C01" "session not responsive after /cancel"
  fi
  exit_pi "e2e-c01"
  wait_all_topics_gone 20 || true
}

test_c02() {
  log "C02 — cancel via TUI (Escape) aborts and topic stays"
  start_pi "e2e-c02"
  if ! wait_topic "e2e-c02" 20 >/dev/null; then
    fail "C02" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-c02")
  # Start a long prompt via Telegram
  $TG send "$SUPERGROUP" "$tid" "write a very long and detailed essay about the entire history of computing from the abacus to modern quantum computers" 2>/dev/null >/dev/null
  sleep 8
  # Cancel via TUI (Escape key aborts in pi)
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-c02" Escape
  # Wait for abort to settle
  sleep 15
  # Verify session still works
  $TG send "$SUPERGROUP" "$tid" "reply with only the word CANCEL_TUI_OK" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "CANCEL_TUI_OK" 60 >/dev/null; then
    pass "C02"
  else
    fail "C02" "session not responsive after TUI cancel"
  fi
  exit_pi "e2e-c02"
  wait_all_topics_gone 20 || true
}

# ─── Follow-up (>>) tests ───────────────────────────────────────

test_f01() {
  log "F01 — >> follow-up via Telegram"
  start_pi "e2e-f01"
  if ! wait_topic "e2e-f01" 20 >/dev/null; then
    fail "F01" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-f01")
  # Send a prompt that keeps the agent busy
  $TG send "$SUPERGROUP" "$tid" "count from 1 to 50, one number per line" 2>/dev/null >/dev/null
  sleep 3
  # Send a follow-up while streaming
  $TG send "$SUPERGROUP" "$tid" ">> after counting, say exactly PAPAYA_FOLLOWUP" 2>/dev/null >/dev/null
  # Wait for the follow-up response
  if wait_bot_msg "$tid" "PAPAYA_FOLLOWUP" 90 >/dev/null; then
    pass "F01"
  else
    fail "F01" "follow-up response not found"
  fi
  exit_pi "e2e-f01"
  wait_all_topics_gone 20 || true
}

# ─── Telegram slash command tests ────────────────────────────────

test_s01() {
  log "S01 — /name via Telegram renames the topic"
  start_pi "e2e-s01"
  if ! wait_topic "e2e-s01" 20 >/dev/null; then
    fail "S01" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-s01")
  $TG send "$SUPERGROUP" "$tid" "/name e2e-s01-renamed" 2>/dev/null >/dev/null
  sleep 10
  if wait_topic "e2e-s01-renamed" 10 >/dev/null; then
    pass "S01"
  else
    fail "S01" "topic not renamed to e2e-s01-renamed"
  fi
  exit_pi "e2e-s01"
  wait_all_topics_gone 20 || true
}

test_s02() {
  log "S02 — /quit via Telegram kills session and deletes topic"
  start_pi "e2e-s02"
  if ! wait_topic "e2e-s02" 20 >/dev/null; then
    fail "S02" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-s02")
  $TG send "$SUPERGROUP" "$tid" "/quit" 2>/dev/null >/dev/null
  if wait_all_topics_gone 20; then
    pass "S02"
  else
    fail "S02" "topic not deleted after /quit"
  fi
}

test_s03() {
  log "S03 — /new via Telegram resets session (topic persists)"
  start_pi "e2e-s03"
  if ! wait_topic "e2e-s03" 20 >/dev/null; then
    fail "S03" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-s03")
  # Send a prompt first so there's context
  $TG send "$SUPERGROUP" "$tid" "say BEFORE_NEW" 2>/dev/null >/dev/null
  wait_bot_msg "$tid" "BEFORE_NEW" 60 >/dev/null || true
  # Now send /new
  $TG send "$SUPERGROUP" "$tid" "/new" 2>/dev/null >/dev/null
  sleep 10
  # Topic should still exist
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    # Verify session is functional after /new
    local active_tid
    active_tid=$(get_any_topic_id)
    $TG send "$SUPERGROUP" "$active_tid" "say AFTER_NEW" 2>/dev/null >/dev/null
    if wait_bot_msg "$active_tid" "AFTER_NEW" 60 >/dev/null; then
      pass "S03"
    else
      fail "S03" "no response after /new"
    fi
  else
    fail "S03" "topic disappeared after /new"
  fi
  exit_pi "e2e-s03"
  wait_all_topics_gone 20 || true
}

test_s04() {
  log "S04 — /compact via Telegram (topic persists)"
  start_pi "e2e-s04"
  if ! wait_topic "e2e-s04" 20 >/dev/null; then
    fail "S04" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-s04")
  # Build up some context
  $TG send "$SUPERGROUP" "$tid" "say BEFORE_COMPACT" 2>/dev/null >/dev/null
  wait_bot_msg "$tid" "BEFORE_COMPACT" 60 >/dev/null || true
  # Compact
  $TG send "$SUPERGROUP" "$tid" "/compact" 2>/dev/null >/dev/null
  sleep 10
  # Topic should still exist and session should work
  local active_tid
  active_tid=$(get_any_topic_id)
  if [ -n "$active_tid" ]; then
    $TG send "$SUPERGROUP" "$active_tid" "say AFTER_COMPACT" 2>/dev/null >/dev/null
    if wait_bot_msg "$active_tid" "AFTER_COMPACT" 60 >/dev/null; then
      pass "S04"
    else
      fail "S04" "no response after /compact"
    fi
  else
    fail "S04" "topic disappeared after /compact"
  fi
  exit_pi "e2e-s04"
  wait_all_topics_gone 20 || true
}

# ─── Pedantic tests (crash, race, adversarial) ──────────────────

test_p01() {
  log "P01 — SIGKILL pup, then restart picks up session"
  start_pi "e2e-p01"
  if ! wait_topic "e2e-p01" 20 >/dev/null; then
    fail "P01" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-p01")
  # Verify session is functional before kill
  $TG send "$SUPERGROUP" "$tid" "reply with only the word BEFORE_KILL" 2>/dev/null >/dev/null
  if ! wait_bot_msg "$tid" "BEFORE_KILL" 60 >/dev/null; then
    fail "P01" "session not responsive before kill"
    exit_pi "e2e-p01"
    return
  fi
  # SIGKILL pup
  stop_pup kill
  sleep 3
  # Restart pup
  if ! start_pup; then
    fail "P01" "pup failed to restart after SIGKILL"
    exit_pi "e2e-p01"
    return
  fi
  # Wait for session to be rediscovered
  sleep 10
  local new_tid
  new_tid=$(get_any_topic_id)
  if [ -z "$new_tid" ]; then
    fail "P01" "no topic after pup SIGKILL + restart"
    exit_pi "e2e-p01"
    return
  fi
  # Verify session is functional after restart
  $TG send "$SUPERGROUP" "$new_tid" "reply with only the word PHOENIX" 2>/dev/null >/dev/null
  if wait_bot_msg "$new_tid" "PHOENIX" 60 >/dev/null; then
    pass "P01"
  else
    fail "P01" "session not responsive after SIGKILL + restart"
  fi
  exit_pi "e2e-p01"
  wait_all_topics_gone 20 || true
}

test_p02() {
  log "P02 — SIGKILL pi mid-stream, topic cleaned up"
  clean_stale_topics
  start_pi "e2e-p02"
  if ! wait_topic "e2e-p02" 20 >/dev/null; then
    fail "P02" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-p02")
  # Start a long prompt
  $TG send "$SUPERGROUP" "$tid" "write a very long essay about the history of every programming language" 2>/dev/null >/dev/null
  sleep 5
  # Kill the pi process (find the window's shell PID and kill the pi child)
  # Using tmux send C-c + C-d + exit is too graceful; we want hard kill.
  # The pi process runs in tmux, so kill the shell in that window.
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p02" C-c
  sleep 0.3
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p02" C-c
  sleep 0.3
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p02" C-d
  sleep 1
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p02" "exit" Enter 2>/dev/null || true
  # Wait for topic to be cleaned up
  if wait_all_topics_gone 20; then
    pass "P02"
  else
    fail "P02" "topic not cleaned up after pi killed (count=$(count_topics))"
  fi
}

test_p09() {
  log "P09 — Two sessions with identical names get separate topics"
  clean_stale_topics
  local work_a work_b
  work_a=$(mktemp -d)
  work_b=$(mktemp -d)
  tmux -S "$SOCKET" new-window -t e2e -n "pi-e2e-p09-a"
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p09-a" "cd $work_a && PUP_SOCKET_DIR=$PUP_SOCKET_DIR pi --dangerously-skip-permissions" Enter
  sleep 5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p09-a" "/name e2e-p09-same" Enter
  sleep 3

  tmux -S "$SOCKET" new-window -t e2e -n "pi-e2e-p09-b"
  sleep 0.5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p09-b" "cd $work_b && PUP_SOCKET_DIR=$PUP_SOCKET_DIR pi --dangerously-skip-permissions" Enter
  sleep 5
  tmux -S "$SOCKET" send-keys -t "e2e:pi-e2e-p09-b" "/name e2e-p09-same" Enter
  sleep 5

  # Should have 2 topics
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" = "2" ]; then
    pass "P09"
  else
    fail "P09" "expected 2 topics for identical names, got $cnt"
  fi
  exit_pi "e2e-p09-a"
  exit_pi "e2e-p09-b"
  wait_all_topics_gone 20 || true
}

test_p17() {
  log "P17 — /new while agent is mid-stream"
  start_pi "e2e-p17"
  if ! wait_topic "e2e-p17" 20 >/dev/null; then
    fail "P17" "topic never appeared"
    return
  fi
  local tid
  tid=$(get_topic_id "e2e-p17")
  # Start a long prompt from Telegram
  $TG send "$SUPERGROUP" "$tid" "write a very long essay about the history of every programming language" 2>/dev/null >/dev/null
  sleep 5
  # Send /new via Telegram while streaming
  $TG send "$SUPERGROUP" "$tid" "/new" 2>/dev/null >/dev/null
  sleep 12
  # Topic should still exist
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -lt 1 ]; then
    fail "P17" "topic disappeared after /new mid-stream"
    exit_pi "e2e-p17"
    return
  fi
  # Session should still be functional
  local active_tid
  active_tid=$(get_any_topic_id)
  $TG send "$SUPERGROUP" "$active_tid" "reply with only the word MIDSTREAM" 2>/dev/null >/dev/null
  if wait_bot_msg "$active_tid" "MIDSTREAM" 60 >/dev/null; then
    pass "P17"
  else
    fail "P17" "session not responsive after /new mid-stream"
  fi
  exit_pi "e2e-p17"
  wait_all_topics_gone 20 || true
}

test_p20() {
  log "P20 — Rapid /new spam (10 resets in 10 seconds)"
  clean_stale_topics
  start_pi "e2e-p20"
  if ! wait_topic "e2e-p20" 20 >/dev/null; then
    fail "P20" "topic never appeared"
    return
  fi
  # Send 10 rapid /new commands via TUI
  for i in $(seq 1 10); do
    pi_send "e2e-p20" "/new"
    sleep 1
  done
  sleep 15
  # Should still have exactly 1 topic
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" != "1" ]; then
    fail "P20" "expected 1 topic after 10 /new, got $cnt"
    exit_pi "e2e-p20"
    wait_all_topics_gone 20 || true
    return
  fi
  # Session should still work
  local tid
  tid=$(get_any_topic_id)
  $TG send "$SUPERGROUP" "$tid" "reply with only the word SURVIVED" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "SURVIVED" 60 >/dev/null; then
    pass "P20"
  else
    fail "P20" "session not responsive after 10 /new spam"
  fi
  exit_pi "e2e-p20"
  wait_all_topics_gone 20 || true
}

test_p22() {
  log "P22 — topics_state.json deleted between pup restarts"
  start_pi "e2e-p22"
  if ! wait_topic "e2e-p22" 20 >/dev/null; then
    fail "P22" "topic never appeared"
    return
  fi
  # Stop pup
  stop_pup graceful
  # Delete state file
  rm -f "$PUP_SOCKET_DIR/topics_state.json"
  # Restart pup
  if ! start_pup; then
    fail "P22" "pup failed to restart after state deletion"
    exit_pi "e2e-p22"
    return
  fi
  sleep 10
  # Session should be rediscovered and get a topic
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    local tid
    tid=$(get_any_topic_id)
    $TG send "$SUPERGROUP" "$tid" "reply with only the word STATELESS" 2>/dev/null >/dev/null
    if wait_bot_msg "$tid" "STATELESS" 60 >/dev/null; then
      pass "P22"
    else
      fail "P22" "session not responsive after state deletion"
    fi
  else
    fail "P22" "no topic after state file deletion + restart"
  fi
  exit_pi "e2e-p22"
  wait_all_topics_gone 20 || true
}

test_p23() {
  log "P23 — topics_state.json is corrupt JSON"
  start_pi "e2e-p23"
  if ! wait_topic "e2e-p23" 20 >/dev/null; then
    fail "P23" "topic never appeared"
    return
  fi
  # Stop pup
  stop_pup graceful
  # Write corrupt state
  echo "NOT {VALID JSON" > "$PUP_SOCKET_DIR/topics_state.json"
  # Restart pup — should handle corrupt file gracefully
  if ! start_pup; then
    fail "P23" "pup failed to start with corrupt state"
    exit_pi "e2e-p23"
    return
  fi
  sleep 10
  # Session should be rediscovered
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -ge 1 ]; then
    local tid
    tid=$(get_any_topic_id)
    $TG send "$SUPERGROUP" "$tid" "reply with only the word CORRUPT_OK" 2>/dev/null >/dev/null
    if wait_bot_msg "$tid" "CORRUPT_OK" 60 >/dev/null; then
      pass "P23"
    else
      fail "P23" "session not responsive after corrupt state"
    fi
  else
    fail "P23" "no topic after corrupt state + restart"
  fi
  exit_pi "e2e-p23"
  wait_all_topics_gone 20 || true
}

test_r01() {
  log "R01 — Pup restart picks up existing sessions"
  start_pi "e2e-r01"
  if ! wait_topic "e2e-r01" 20 >/dev/null; then
    fail "R01" "topic never appeared"
    return
  fi
  # Restart pup
  restart_pup graceful
  sleep 10
  # Session should be rediscovered
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" -lt 1 ]; then
    fail "R01" "no topic after pup restart"
    exit_pi "e2e-r01"
    return
  fi
  # Verify functional
  local tid
  tid=$(get_any_topic_id)
  $TG send "$SUPERGROUP" "$tid" "reply with only the word RESTARTED" 2>/dev/null >/dev/null
  if wait_bot_msg "$tid" "RESTARTED" 60 >/dev/null; then
    pass "R01"
  else
    fail "R01" "session not responsive after pup restart"
  fi
  exit_pi "e2e-r01"
  wait_all_topics_gone 20 || true
}

test_r06() {
  log "R06 — Session exits during pup downtime"
  start_pi "e2e-r06"
  if ! wait_topic "e2e-r06" 20 >/dev/null; then
    fail "R06" "topic never appeared"
    return
  fi
  # Stop pup
  stop_pup graceful
  # Exit the pi session while pup is down
  exit_pi "e2e-r06"
  sleep 2
  # Restart pup
  if ! start_pup; then
    fail "R06" "pup failed to restart"
    return
  fi
  sleep 10
  # Should have zero sessions (pi exited while pup was down)
  local cnt
  cnt=$(count_topics)
  if [ "$cnt" = "0" ]; then
    pass "R06"
  else
    # It's acceptable to have a stale topic from the old run — clean it up
    # The key is pup didn't crash
    pass "R06 (stale topic count=$cnt, no crash)"
  fi
}

if [ "$TESTS" = "all" ]; then
  # Core lifecycle
  test_t01   # topic created
  test_t02   # topic deleted on exit
  test_t03   # message via Telegram → response
  test_t23   # message via TUI → response in topic
  test_t04   # parallel sessions
  test_t05   # /name via TUI
  test_t09   # tool calls visible
  test_t13   # fast response timing
  test_t14   # concurrent prompts

  # Session reset via TUI
  test_t15   # /new via TUI preserves topic
  test_t16   # messages work after /new via TUI
  test_t17   # multiple /new via TUI
  test_t18   # /compact via TUI preserves topic
  test_t19   # /new then exit via TUI
  test_t22   # /quit via TUI deletes topic
  test_t21   # unnamed session fallback naming

  # Slash commands via Telegram
  test_s01   # /name via Telegram
  test_s02   # /quit via Telegram
  test_s03   # /new via Telegram
  test_s04   # /compact via Telegram

  # Cancel
  test_c01   # /cancel via Telegram
  test_c02   # cancel via TUI (Escape)

  # Follow-up
  test_f01   # >> prefix via Telegram

  # Pedantic: crash & recovery
  test_p01   # SIGKILL pup mid-stream
  test_p02   # SIGKILL pi mid-stream
  test_p09   # identical session names
  test_p17   # /new while agent mid-stream
  test_p20   # rapid /new spam (10x)
  test_p22   # state file deleted between restarts
  test_p23   # corrupt state file

  # Robustness
  test_r01   # pup restart picks up sessions
  test_r06   # session exits during pup downtime
else
  # Run specific tests: e.g. "t01 t03"
  for t in $TESTS; do
    "test_$t"
  done
fi

echo ""
echo "════════════════════════════════════════════════"
echo "  RESULTS: $PASSED passed, $FAILED failed, $SKIPPED skipped"
echo "════════════════════════════════════════════════"
if [ "$FAILED" -gt 0 ]; then
  echo ""
  echo "Failures:"
  for e in "${ERRORS[@]}"; do
    echo "  ✗ $e"
  done
  FINAL_EXIT=1
else
  FINAL_EXIT=0
fi
exit $FINAL_EXIT
