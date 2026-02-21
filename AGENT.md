# Agent notes

## ⚠️  CRITICAL: Never block on pup

`pup` is a long-running daemon that **never exits on its own**.  Every bash
call that touches pup must return quickly.

### Correct pattern: run pup in tmux

```bash
SOCKET_DIR=${TMPDIR:-/tmp}/claude-tmux-sockets
mkdir -p "$SOCKET_DIR"
SOCKET="$SOCKET_DIR/pup.sock"

# Start pup in a tmux session
tmux -S "$SOCKET" new -d -s pup
# (wait for shell, then send command)
sleep 2
tmux -S "$SOCKET" send-keys -t pup:1.1 \
  "cd /root/handoff/main && RUST_LOG=info ./target/debug/pup" Enter

# Inspect output (non-blocking)
sleep 5
tmux -S "$SOCKET" capture-pane -p -J -t pup:1.1 -S -30

# Kill when done
tmux -S "$SOCKET" send-keys -t pup:1.1 C-c
tmux -S "$SOCKET" kill-server
```

For quick smoke tests, `timeout` also works:

```bash
timeout 15 ./target/debug/pup 2>/tmp/pup.log; true
grep -E '(topic|session|error)' /tmp/pup.log
```

### NEVER do any of these

```bash
# ❌ Backgrounding pup — stderr bleeds into bash tool and it hangs:
./target/debug/pup 2>/tmp/log &

# ❌ nohup — same problem:
nohup ./target/debug/pup > /tmp/log 2>&1 &

# ❌ Running pup without timeout — blocks forever:
./target/debug/pup 2>/tmp/log
```

### Key log patterns to grep for

| Condition | Pattern |
|---|---|
| Backend survived startup | `telegram backend started` without a subsequent `telegram backend shut down` |
| Topic created | `topic created` |
| Session picked up | `session connected` |
| History posted | `sendMessage` lines right after `topic created` for a session with `turns > 0` |
| Channel overflow | `backend channel full or closed` |
| Errors | `error`, `WARN`, `failed` |

## Building

```bash
cargo build          # debug
cargo test           # all tests
cargo test -p pup-telegram  # just telegram crate tests
cargo clippy         # lint
```

## Config

Bot token and supergroup ID live in `~/.config/pup/config.toml`.

## Socket directory

Sessions expose IPC sockets in `~/.pi/pup/`. Stale `.sock` files from dead
sessions should be removed before restarting pup to avoid connection errors:

```bash
rm -f /root/.pi/pup/*.sock
```
