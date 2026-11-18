#!/usr/bin/env python3
"""Plan which Kani shards CI runs for a set of changed files.

A crate's proofs run when a file in that crate, or in any workspace crate it
depends on (transitively), changed. Changes to workspace-wide inputs such as
Cargo.lock or the CI workflow run every shard. Changes elsewhere (docs, book,
tla, ...) run none.

Prints a JSON list for `strategy.matrix.include`, one object per shard:
    {"crate": "ffwd-core", "shard": 1, "of": 6, "lib": false}

Usage:
    scripts/kani_plan.py --all
    scripts/kani_plan.py --changed-files '["crates/ffwd-io/src/x.rs"]'
    scripts/kani_plan.py --changed-files-from changed.json
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Crates with Kani proofs -> number of CI shards. Raise a count when that
# crate's shards get slow; scripts/kani_shard.py splits harnesses evenly.
SHARDS: dict[str, int] = {
    "ffwd-kani": 2,
    "ffwd-core": 6,
    "ffwd-arrow": 4,
    "ffwd-types": 2,
    # Lifecycle/registry/reload state-machine proofs take minutes each.
    "ffwd-runtime": 6,
    "ffwd-io": 2,
    "ffwd-output": 2,
    "ffwd-diagnostics": 1,
    "ffwd": 1,
}

# ffwd-arrow has a binary with required-features that cargo-kani cannot
# skip, so only its library target is verified.
LIB_ONLY = {"ffwd-arrow"}

# Changes here can affect every proof.
GLOBAL_INPUTS = {
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/config.toml",
    ".github/workflows/ci.yml",
    "scripts/kani_plan.py",
    "scripts/kani_shard.py",
    "dev-docs/verification/kani-boundary-contract.toml",
    "scripts/verify_kani_boundary_contract.py",
    "scripts/verify_verification_trigger_contract.py",
}

# dorny/paths-filter lists PR files through the GitHub API, which stops at
# 3,000 files. A list that long may be truncated, so plan everything.
MAX_LISTED_FILES = 3000


def workspace_manifests() -> list[Path]:
    """Manifests of the root workspace members (globs expanded, excludes dropped)."""
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
    excluded = {(ROOT / path).resolve() for path in workspace.get("exclude", [])}
    manifests: set[Path] = set()
    for pattern in workspace.get("members", []):
        for member in ROOT.glob(pattern):
            manifest = member / "Cargo.toml"
            if manifest.is_file() and member.resolve() not in excluded:
                manifests.add(manifest)
    return sorted(manifests)


def workspace_crates() -> dict[str, tuple[str, set[str]]]:
    """Map crate name -> (crate dir relative to ROOT, workspace deps)."""
    crates: dict[str, tuple[str, set[str]]] = {}
    raw: dict[str, tuple[str, dict]] = {}
    for manifest in workspace_manifests():
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        name = data.get("package", {}).get("name")
        if name:
            raw[name] = (manifest.parent.relative_to(ROOT).as_posix(), data)
    for name, (rel_dir, data) in raw.items():
        deps: set[str] = set()
        for section in ("dependencies", "build-dependencies"):
            for dep_key, spec in data.get(section, {}).items():
                dep_name = spec.get("package", dep_key) if isinstance(spec, dict) else dep_key
                if dep_name in raw:
                    deps.add(dep_name)
        for target in data.get("target", {}).values():
            for section in ("dependencies", "build-dependencies"):
                for dep_key, spec in target.get(section, {}).items():
                    dep_name = spec.get("package", dep_key) if isinstance(spec, dict) else dep_key
                    if dep_name in raw:
                        deps.add(dep_name)
        crates[name] = (rel_dir, deps)
    return crates


def transitive_deps(crates: dict[str, tuple[str, set[str]]], name: str) -> set[str]:
    seen: set[str] = set()
    stack = [name]
    while stack:
        current = stack.pop()
        if current in seen or current not in crates:
            continue
        seen.add(current)
        stack.extend(crates[current][1])
    return seen


def affected_kani_crates(changed: list[str]) -> set[str]:
    if len(changed) >= MAX_LISTED_FILES or any(path in GLOBAL_INPUTS for path in changed):
        return set(SHARDS)
    crates = workspace_crates()
    touched = {
        name
        for name, (rel_dir, _) in crates.items()
        if any(path == rel_dir or path.startswith(rel_dir + "/") for path in changed)
    }
    return {crate for crate in SHARDS if transitive_deps(crates, crate) & touched}


def build_matrix(selected: set[str]) -> list[dict]:
    return [
        {"crate": crate, "shard": shard, "of": count, "lib": crate in LIB_ONLY}
        for crate, count in SHARDS.items()
        if crate in selected
        for shard in range(1, count + 1)
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--all", action="store_true", help="plan every shard")
    group.add_argument("--changed-files", help="JSON list of changed paths")
    group.add_argument(
        "--changed-files-from", help="file holding a JSON list of changed paths"
    )
    args = parser.parse_args()

    if args.all:
        selected = set(SHARDS)
    else:
        raw = (
            Path(args.changed_files_from).read_text(encoding="utf-8")
            if args.changed_files_from
            else args.changed_files
        )
        selected = affected_kani_crates(json.loads(raw.strip() or "[]"))
    print(json.dumps(build_matrix(selected), separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
