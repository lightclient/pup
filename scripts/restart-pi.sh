#!/usr/bin/env bash
# Restart the pi session in tmux pane %31.
# Kills the pi process and restarts it.

PANE="%31"
WORKDIR="/root/handoff/main"

# Find and kill the pi process in that pane
PI_PID=$(tmux display-message -t "$PANE" -p '#{pane_pid}')
echo "Pane PID: $PI_PID"

# The pane's direct child is pi (or a shell running pi)
# Find the actual pi process
PI_PROC=$(pgrep -P "$PI_PID" -x pi 2>/dev/null || ps -p "$PI_PID" -o pid= 2>/dev/null)
echo "Pi process: $PI_PROC"

# Kill pi gracefully, then force if needed
if [ -n "$PI_PROC" ]; then
    kill "$PI_PROC" 2>/dev/null
    sleep 3
    kill -9 "$PI_PROC" 2>/dev/null
    sleep 2
fi

# Wait for the pane to show a shell prompt
sleep 2

# Restart pi (the pup extension is loaded automatically from
# ~/.pi/agent/extensions/pup/ — do NOT also use -e, which
# would load it a second time and create duplicate sockets/topics).
tmux send-keys -t "$PANE" "cd $WORKDIR && pi" Enter

echo "pi restarted in pane $PANE"
