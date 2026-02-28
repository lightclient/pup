#!/usr/bin/env bash
# Install git hooks that match CI checks.
# Run once after cloning: ./scripts/install-hooks.sh
set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOK_DIR="$REPO_ROOT/.git/hooks"

install_hook() {
    local src="$REPO_ROOT/scripts/hooks/$1"
    local dst="$HOOK_DIR/$1"
    cp "$src" "$dst"
    chmod +x "$dst"
    echo "Installed $1 hook"
}

install_hook pre-push

echo "Done. Hooks installed to $HOOK_DIR"
