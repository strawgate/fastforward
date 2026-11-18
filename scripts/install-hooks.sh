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

# With core.hooksPath set (often a global, shared hooks directory), installing
# would overwrite hooks that other repositories use. Refuse instead.
HOOKS_PATH="$(git config --get core.hooksPath || true)"
if [ -n "$HOOKS_PATH" ]; then
    cat >&2 <<EOF
error: core.hooksPath is set to '$HOOKS_PATH', so hooks would be written there
       and could replace hooks other repositories share. Not installing.
       Call scripts/ci-local.sh pre-commit / pre-push from your own hooks instead,
       or unset core.hooksPath for this repo and re-run.
EOF
    exit 1
fi

HOOKS_DIR="$(git rev-parse --git-path hooks)"
mkdir -p "$HOOKS_DIR"
MARKER="# installed by scripts/install-hooks.sh"

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
