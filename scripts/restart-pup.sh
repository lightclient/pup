#!/usr/bin/env bash
set -euo pipefail

PANE="0:pi.2"
PUP_DIR="/root/handoff/main"
PUP_BIN="target/release/pup"

# Verify the binary exists
if [[ ! -x "$PUP_DIR/$PUP_BIN" ]]; then
    echo "FATAL: $PUP_DIR/$PUP_BIN not found or not executable" >&2
    exit 1
fi

# Send Ctrl-C to gracefully stop pup, wait for it to exit
echo "Sending Ctrl-C to pup in pane $PANE..."
tmux send-keys -t "$PANE" C-c
sleep 2

# Check if it actually stopped
STILL_RUNNING=$(pgrep -f 'target/release/pup' || true)
if [[ -n "$STILL_RUNNING" ]]; then
    echo "Pup still running (pid $STILL_RUNNING), sending SIGTERM..."
    kill "$STILL_RUNNING" 2>/dev/null || true
    sleep 2
    # Last resort
    if kill -0 "$STILL_RUNNING" 2>/dev/null; then
        echo "SIGKILL..."
        kill -9 "$STILL_RUNNING" 2>/dev/null || true
        sleep 1
    fi
fi

# Restart pup in the same pane with the same env
echo "Starting pup..."
tmux send-keys -t "$PANE" "PUP_TRACE_FILE=trace.jsonl $PUP_BIN" Enter

# Wait and verify it came up
sleep 3
NEW_PID=$(pgrep -f 'target/release/pup' || true)
if [[ -n "$NEW_PID" ]]; then
    echo "OK: pup is running (pid $NEW_PID)"
else
    echo "WARNING: pup doesn't appear to be running yet, check pane $PANE"
fi
