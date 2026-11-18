#!/usr/bin/env bash
# Run the turmoil simulation suite across a shard of seeds and record every
# failing (seed, test) pair so nightly failures can be replayed locally.
#
# Usage:
#   scripts/turmoil-seed-sweep.sh <shard-index> <shard-count> <total-seeds> <out-dir>
#
# Environment:
#   TURMOIL_SWEEP_BUDGET_SECS  Stop starting new seeds after this many seconds
#                              (default 3000). Unrun seeds are recorded in
#                              <out-dir>/skipped_seeds.txt instead of the job
#                              being cancelled by the runner timeout.
#   TURMOIL_SEED_TIMEOUT_SECS  Per-seed wall-clock limit (default 600). A seed
#                              that exceeds it is recorded as a TIMEOUT failure.
#   TURMOIL_SIM_BIN            Prebuilt turmoil_sim test binary. When unset the
#                              script builds it with cargo.
#
# Outputs (all inside <out-dir>, which is self-describing so the report job
# does not depend on how upload/download-artifact lays out directories):
#   shard.txt          "<index>/<count>"
#   failures.tsv       one "<seed>\t<test name>" line per failing test; the
#                      second field is "TIMEOUT ..." or "UNKNOWN ..." when no
#                      test name is available (see logs/seed-<N>.log)
#   failed_seeds.txt   one failing seed per line
#   skipped_seeds.txt  seeds not run because the time budget ran out
#   summary.txt        run/failed/skipped counts
#   logs/seed-<N>.log  full test output for each failing seed
#
# Exit status: 1 if any seed failed, 0 otherwise.

set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 <shard-index> <shard-count> <total-seeds> <out-dir>" >&2
  exit 2
fi

SHARD_INDEX=$1
SHARD_COUNT=$2
TOTAL_SEEDS=$3
OUT_DIR=$4
BUDGET_SECS=${TURMOIL_SWEEP_BUDGET_SECS:-3000}
SEED_TIMEOUT_SECS=${TURMOIL_SEED_TIMEOUT_SECS:-600}

is_uint() { [[ $1 =~ ^[0-9]+$ ]]; }
if ! is_uint "$SHARD_INDEX" || ! is_uint "$SHARD_COUNT" || ! is_uint "$TOTAL_SEEDS" \
  || [ "$SHARD_COUNT" -lt 1 ] || [ "$SHARD_INDEX" -ge "$SHARD_COUNT" ]; then
  echo "ERROR: need 0 <= shard-index < shard-count and numeric total-seeds" >&2
  exit 2
fi

mkdir -p "$OUT_DIR/logs"
: >"$OUT_DIR/failures.tsv"
: >"$OUT_DIR/failed_seeds.txt"
: >"$OUT_DIR/skipped_seeds.txt"
echo "${SHARD_INDEX}/${SHARD_COUNT}" >"$OUT_DIR/shard.txt"

BIN=${TURMOIL_SIM_BIN:-}
if [ -z "$BIN" ]; then
  # Cargo diagnostics stay on stderr; a failed build falls through to the
  # missing-binary error below instead of aborting under pipefail.
  BIN=$(cargo test -p ffwd --no-default-features --features turmoil --test turmoil_sim --no-run --message-format=json \
    | jq -r 'select(.executable != null and (.target.kind // [] | index("test"))) | .executable' \
    | sed -n '1p') || BIN=""
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "ERROR: could not find turmoil test binary" >&2
  exit 2
fi
echo "Test binary: $BIN"
echo "Shard ${SHARD_INDEX}/${SHARD_COUNT}: seeds where ((seed-1) % ${SHARD_COUNT}) == ${SHARD_INDEX}"

start=$(date +%s)
run=0
failed=0
skipped=0
seed_out=$(mktemp)
trap 'rm -f "$seed_out"' EXIT

for seed in $(seq 1 "$TOTAL_SEEDS"); do
  if [ $(((seed - 1) % SHARD_COUNT)) -ne "$SHARD_INDEX" ]; then
    continue
  fi
  if [ $(($(date +%s) - start)) -ge "$BUDGET_SECS" ]; then
    echo "$seed" >>"$OUT_DIR/skipped_seeds.txt"
    skipped=$((skipped + 1))
    continue
  fi
  run=$((run + 1))

  # Judge pass/fail by the test binary's exit status only. libtest ends its
  # output with a blank line, so inspecting the last line is not reliable.
  status=0
  TURMOIL_SEED=$seed timeout --kill-after=30 "$SEED_TIMEOUT_SECS" \
    "$BIN" --test-threads=1 >"$seed_out" 2>&1 || status=$?

  if [ "$status" -ne 0 ]; then
    failed=$((failed + 1))
    cp "$seed_out" "$OUT_DIR/logs/seed-${seed}.log"
    echo "$seed" >>"$OUT_DIR/failed_seeds.txt"

    # libtest ends with a "failures:" block listing one test per indented
    # line just before "test result:". Captured test output can also contain
    # "failures:" lines, so keep only the last block seen before the summary.
    tests=$(awk '
      /^failures:$/ { list = ""; inblock = 1; next }
      /^test result:/ { exit }
      inblock && /^    [^ ]/ { sub(/^    /, ""); list = list $0 "\n"; next }
      inblock && /^$/ { next }
      inblock { inblock = 0; list = "" }
      END { printf "%s", list }
    ' "$seed_out")
    if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
      tests="TIMEOUT after ${SEED_TIMEOUT_SECS}s"
    elif [ -z "$tests" ]; then
      tests="UNKNOWN (exit status ${status})"
    fi
    while IFS= read -r t; do
      printf '%s\t%s\n' "$seed" "$t" >>"$OUT_DIR/failures.tsv"
      echo "FAILED shard ${SHARD_INDEX} seed ${seed}: ${t}"
    done <<<"$tests"
    tail -20 "$seed_out"
  fi

  if [ $((run % 25)) -eq 0 ]; then
    echo "=== Shard ${SHARD_INDEX} progress: ${run} runs, ${failed} failures, $(($(date +%s) - start))s elapsed ==="
  fi
done

{
  echo "run=${run}"
  echo "failed=${failed}"
  echo "skipped=${skipped}"
} >"$OUT_DIR/summary.txt"
echo "=== Shard ${SHARD_INDEX} results: ${failed} failures out of ${run} runs (${skipped} skipped for time budget) ==="

if [ "$failed" -gt 0 ]; then
  exit 1
fi
