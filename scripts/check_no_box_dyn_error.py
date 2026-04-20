#!/usr/bin/env python3
"""Fail if a public signature returns or accepts `Box<dyn Error>`.

Library crates must expose `thiserror`-style enums with matchable variants
so callers can recover. `Box<dyn Error>` strips that ability and is
permitted only in the binary crate (`logfwd`) and in benches/profilers.

See `dev-docs/CODE_STYLE.md` -> Error Handling.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys


REPO_ROOT = Path(__file__).resolve().parent.parent
CRATES_ROOT = REPO_ROOT / "crates"

# Crates allowed to use Box<dyn Error> in public signatures. The binary
# crate (`logfwd`) is the application shell; benches/profilers and
# experimental sensor binaries are not library APIs.
ALLOWED_CRATES = {
    "logfwd",
    "logfwd-bench",
    "logfwd-competitive-bench",
    "logfwd-test-utils",
    "logfwd-ebpf-proto",
}

# Files within otherwise-restricted crates that are exempt (binaries,
# examples, benches, tests). These are not part of the library surface.
EXEMPT_PATH_PARTS = {"bin", "benches", "examples", "tests"}

# Match `pub` items whose signature mentions Box<dyn Error...>. Tolerates
# `pub fn`, `pub(crate) fn`, `pub async fn`, type aliases, and trait
# methods. Multiline match for wrapped signatures.
PUB_BOX_DYN_ERROR = re.compile(
    r"^[ \t]*pub(?:\([^)]*\))?[^;{]*Box\s*<\s*dyn\s+(?:std::error::|core::error::|alloc::error::)?Error",
    re.MULTILINE,
)


def crate_name(path: Path) -> str:
    rel = path.relative_to(CRATES_ROOT)
    # crate dir is the first path component
    return rel.parts[0]


def is_exempt(path: Path) -> bool:
    crate = crate_name(path)
    if crate in ALLOWED_CRATES:
        return True
    parts = set(path.relative_to(CRATES_ROOT).parts)
    return bool(parts & EXEMPT_PATH_PARTS)


def rust_files() -> list[Path]:
    return sorted(CRATES_ROOT.rglob("*.rs"))


def relpath(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def find_violations() -> list[str]:
    violations: list[str] = []
    for path in rust_files():
        if is_exempt(path):
            continue
        text = path.read_text(encoding="utf-8")
        for match in PUB_BOX_DYN_ERROR.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            snippet = match.group(0).strip().splitlines()[0]
            violations.append(f"{relpath(path)}:{line}: {snippet}")
    return violations


def main() -> int:
    offenders = find_violations()
    if not offenders:
        print("No-Box<dyn Error>-in-public-signatures guard OK.")
        return 0

    print(
        "Public-signature Box<dyn Error> guard failed. "
        "Library crates must expose thiserror enums with matchable variants. "
        "See dev-docs/CODE_STYLE.md -> Error Handling.",
        file=sys.stderr,
    )
    print("Violations:", file=sys.stderr)
    for entry in offenders:
        print(f"  - {entry}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
