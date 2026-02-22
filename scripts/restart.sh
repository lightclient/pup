#!/bin/bash
# Restart pup in pane %30 (bottom), then restart pi in pane %31 (top).
# This script is sent to a tmux pane so it runs outside of pi.

set -e

# Kill the old pup daemon.
tmux send-keys -t %30 C-c
sleep 1

# Start the new pup daemon.
tmux send-keys -t %30 'OPUS_STATIC=1 target/release/pup' Enter

# Kill the current pi session (top pane).
tmux send-keys -t %31 C-c
sleep 1

# Restart pi in the same pane.
tmux send-keys -t %31 'pi' Enter
