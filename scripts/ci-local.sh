#!/usr/bin/env bash
# Run the same checks as the CI "Lint" job locally, without needing `just`.
#
# Usage:
#   scripts/ci-local.sh pre-commit   # fast (a few seconds): fmt, TOML, typos, python guardrails
#   scripts/ci-local.sh pre-push     # pre-commit + CI Lint parity: xtask verify, clippy,
#                                    # rustdoc, cargo deny, cargo machete
#   scripts/ci-local.sh test         # full workspace tests, as in CI "Test (Linux)"
#   scripts/ci-local.sh all          # pre-push + test
#
# The command list mirrors .github/workflows/ci.yml (job `lint`). When you
# change that job, change this script too.
#
# Environment:
#   FFWD_SKIP_HOOKS=1        skip everything (same effect as `git commit/push --no-verify`)
#   FFWD_STRICT_TOOLS=1      fail (instead of warn) when an optional tool is missing
#   FFWD_SKIP_TOOLCHAIN_CHECK=1  don't compare rustc with rust-toolchain.toml
set -euo pipefail

mode="${1:-pre-push}"

if [ "${FFWD_SKIP_HOOKS:-0}" = "1" ]; then
    echo "ci-local: FFWD_SKIP_HOOKS=1, skipping $mode checks"
    exit 0
fi

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# CI runs with these; mirror them so builds and tests behave the same.
export FFWD_DISABLE_DEFAULT_CHECKPOINTS=1
# .cargo/config.toml sets rustc-wrapper = "sccache". Without sccache every
# cargo command fails, so drop the wrapper when it is not installed.
if [ -z "${RUSTC_WRAPPER+x}" ] && ! command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=""
fi

missing=()
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
need() {
    # need <binary> <install hint>: returns 1 (and records) when missing.
    if command -v "$1" >/dev/null 2>&1; then
        return 0
    fi
    echo "    SKIP: $1 not installed ($2)"
    missing+=("$1")
    return 1
}

check_toolchain() {
    [ "${FFWD_SKIP_TOOLCHAIN_CHECK:-0}" = "1" ] && return 0
    local want have
    want="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
    have="$(rustc --version | awk '{print $2}')"
    case "$want" in
        stable | beta | nightly* | "") return 0 ;;
    esac
    # Accept "1.100" matching "1.100.0" and exact matches.
    if [ "$have" = "$want" ] || [ "${have%.*}" = "$want" ]; then
        return 0
    fi
    cat >&2 <<EOF
ci-local: rustc $have is active, but CI uses $want (rust-toolchain.toml).
          Clippy lints differ between versions, so results would not match CI.
          Fix: install rustup (it reads rust-toolchain.toml automatically), or run
               rustup toolchain install $want --component clippy rustfmt
          Or set FFWD_SKIP_TOOLCHAIN_CHECK=1 to run anyway.
EOF
    exit 1
}

run_pre_commit() {
    step "cargo fmt --check"
    cargo fmt --check

    step "TOML format (taplo check)"
    if need taplo "cargo install taplo-cli --locked"; then taplo check; fi

    step "Typos"
    if need typos "cargo install typos-cli --locked"; then typos; fi

    step "Python guardrails"
    python3 scripts/check_workspace_inherited_default_features.py
    python3 scripts/check_max_source_lines.py
    python3 scripts/check_no_raw_payload_injection.py
    python3 scripts/verify_kani_boundary_contract.py
    python3 scripts/verify_tlc_matrix_contract.py
    python3 scripts/verify_proptest_regressions.py
    python3 scripts/verify_verification_trigger_contract.py
}

run_pre_push() {
    run_pre_commit

    step "Actionlint"
    if need actionlint "go install github.com/rhysd/actionlint/cmd/actionlint@latest"; then actionlint; fi

    step "Xtask structural verification"
    cargo xtask verify

    step "Clippy (cargo clippy --workspace -- -D warnings)"
    cargo clippy --workspace -- -D warnings

    step "Rustdoc warnings"
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

    step "cargo deny check"
    if need cargo-deny "cargo install cargo-deny --locked"; then cargo deny check; fi

    step "cargo machete"
    if need cargo-machete "cargo install cargo-machete --locked"; then cargo machete; fi
}

run_test() {
    step "Tests (workspace)"
    if command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run --workspace --profile ci
    else
        echo "    cargo-nextest not installed; falling back to cargo test"
        cargo test --workspace --no-fail-fast
    fi
}

case "$mode" in
    pre-commit) run_pre_commit ;;
    pre-push) check_toolchain; run_pre_push ;;
    test) check_toolchain; run_test ;;
    all) check_toolchain; run_pre_push; run_test ;;
    *)
        echo "usage: $0 {pre-commit|pre-push|test|all}" >&2
        exit 2
        ;;
esac

if [ "${#missing[@]}" -gt 0 ]; then
    echo ""
    echo "ci-local: $mode passed, but skipped checks for missing tools: ${missing[*]}"
    echo "          CI runs them; install hints are printed above."
    if [ "${FFWD_STRICT_TOOLS:-0}" = "1" ]; then
        exit 1
    fi
else
    echo ""
    echo "ci-local: $mode passed"
fi
