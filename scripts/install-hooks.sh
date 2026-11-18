#!/usr/bin/env bash
# Install git hooks that run scripts/ci-local.sh. Works without `just`,
# from any worktree, and is safe to re-run.
#
#   pre-commit -> scripts/ci-local.sh pre-commit   (fmt, TOML, typos, guardrails; seconds)
#   pre-push   -> scripts/ci-local.sh pre-push     (CI Lint parity: clippy, rustdoc, deny, ...)
#
# Bypass once with `git commit --no-verify` / `git push --no-verify`,
# or set FFWD_SKIP_HOOKS=1. Remove with `scripts/install-hooks.sh --uninstall`.
set -euo pipefail

HOOKS_DIR="$(git rev-parse --git-path hooks)"
mkdir -p "$HOOKS_DIR"
MARKER="# installed by scripts/install-hooks.sh"

if [ "$(git config --get core.hooksPath || true)" != "" ]; then
    echo "warning: core.hooksPath is set to '$(git config --get core.hooksPath)'; git will not run hooks from $HOOKS_DIR" >&2
fi

for hook in pre-commit pre-push; do
    target="$HOOKS_DIR/$hook"
    if [ "${1:-}" = "--uninstall" ]; then
        if [ -f "$target" ] && grep -q "$MARKER" "$target"; then
            rm -f "$target"
            echo "removed $target"
        fi
        continue
    fi
    if [ -f "$target" ] && ! grep -q "$MARKER" "$target"; then
        mv "$target" "$target.bak"
        echo "backed up existing $hook hook to $target.bak"
    fi
    cat >"$target" <<HOOK
#!/bin/sh
$MARKER
exec "\$(git rev-parse --show-toplevel)/scripts/ci-local.sh" $hook
HOOK
    chmod +x "$target"
    echo "installed $target"
done
