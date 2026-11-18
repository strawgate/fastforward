#!/usr/bin/env python3
"""Run one shard of a crate's Kani harnesses.

CI splits each crate's Kani harnesses across several runners. This script
lists the crate's harnesses with `cargo kani list`, sorts them, takes every
Nth one (round-robin, so the slow harnesses of one module spread across
shards), and verifies only those with `cargo kani --exact --harness ...`.

New harnesses are picked up automatically; no hand-maintained lists.

Usage:
    scripts/kani_shard.py --crate ffwd-core --shard 1 --of 4 [--features kani-slow]
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Unstable features every proof in the workspace may use. Must match for
# listing and verification, or compilation of contract proofs fails.
UNSTABLE = ["-Z", "function-contracts", "-Z", "mem-predicates", "-Z", "stubbing"]


def list_harnesses(crate_dir: Path, cargo_args: list[str]) -> list[str]:
    """Return every standard and contract harness name in the crate.

    `cargo kani list` takes no cargo options itself; they must come before
    the `list` subcommand. The -Z flags are needed on both sides: before
    `list` they enable the features for compilation when cargo options are
    present, after it when none are.
    """
    out = crate_dir / "kani-list.json"
    out.unlink(missing_ok=True)
    subprocess.run(
        ["cargo", "kani", *cargo_args, *UNSTABLE, "list", *UNSTABLE, "--format", "json"],
        cwd=crate_dir,
        check=True,
    )
    data = json.loads(out.read_text())
    out.unlink()
    names: set[str] = set()
    for key in ("standard-harnesses", "contract-harnesses"):
        for per_file in data.get(key, {}).values():
            names.update(per_file)
    return sorted(names)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--crate", required=True, help="crate name under crates/")
    parser.add_argument("--shard", type=int, required=True, help="1-based shard index")
    parser.add_argument("--of", type=int, required=True, help="total shard count")
    parser.add_argument("--features", default="", help="cargo features, e.g. kani-slow")
    parser.add_argument("--jobs", type=int, default=2, help="parallel CBMC processes")
    parser.add_argument("--lib", action="store_true", help="verify only the lib target")
    args = parser.parse_args()

    if not 1 <= args.shard <= args.of:
        parser.error("--shard must be in [1, --of]")

    crate_dir = REPO / "crates" / args.crate
    features = ["--features", args.features] if args.features else []
    target = ["--lib"] if args.lib else []
    harnesses = list_harnesses(crate_dir, [*features, *target])
    mine = harnesses[args.shard - 1 :: args.of]
    print(
        f"{args.crate}: shard {args.shard}/{args.of} runs "
        f"{len(mine)} of {len(harnesses)} harnesses",
        flush=True,
    )
    if not mine:
        return 0

    cmd = [
        "cargo",
        "kani",
        "--solver",
        "kissat",
        "--jobs",
        str(args.jobs),
        "--output-format",
        "terse",
        *UNSTABLE,
        *features,
        *target,
        "--exact",
    ]
    for name in mine:
        cmd += ["--harness", name]
    return subprocess.run(cmd, cwd=crate_dir, env=os.environ).returncode


if __name__ == "__main__":
    sys.exit(main())
