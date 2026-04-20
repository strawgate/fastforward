#!/usr/bin/env python3
"""Fail if `panic!`, `todo!`, or `unimplemented!` appears outside test code
in production crates (`logfwd-runtime`, `logfwd-output`).

Rationale: a production panic crashes the running pipeline. These macros
are acceptable in tests, behind `// ALLOW-PANIC: <reason>` (genuinely
unreachable invariants), or in `#[cfg(test)]` modules and `#[test]`
functions.

This is a conservative regex-based AST walk:

  * Tracks brace depth.
  * When `#[cfg(test)]` precedes a `mod ... {` or `fn ... {`, the
    introduced scope is treated as test code until braces close.
  * `#[test]`, `#[tokio::test]`, `#[proptest]`, and other `*::test`
    attributes are treated identically.
  * `unreachable!` is allowed (it expresses an invariant, not a panic).
  * Same-line comment `// ALLOW-PANIC: <reason>` opts a line out
    explicitly.

See `dev-docs/CODE_STYLE.md` -> Error Handling.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys


REPO_ROOT = Path(__file__).resolve().parent.parent
CRATES_ROOT = REPO_ROOT / "crates"

# Production crates whose src/ is subject to the guard. Tests/benches
# in these crates are still allowed via the in-file test-context tracking.
GUARDED_CRATES = ("logfwd-runtime", "logfwd-output")

# Macros that constitute a hard production panic.
PANIC_MACROS = re.compile(r"\b(panic|todo|unimplemented)!\s*\(")

# Test attributes that designate a `mod` or `fn` as test-only scope.
TEST_ATTR = re.compile(
    r"^[ \t]*#\[\s*(?:cfg\s*\(\s*test\s*\)|test|tokio::test|"
    r"proptest|tracing_test::traced_test|[A-Za-z_:]*::test)\s*[\],]"
)

MOD_OR_FN_OPEN = re.compile(r"\b(?:mod|fn|impl)\b[^;{]*\{")

ALLOW_MARKER = "// ALLOW-PANIC:"


def relpath(path: Path) -> str:
    return path.relative_to(REPO_ROOT).as_posix()


def collect_files() -> list[Path]:
    files: list[Path] = []
    for crate in GUARDED_CRATES:
        crate_src = CRATES_ROOT / crate / "src"
        if not crate_src.exists():
            continue
        for path in sorted(crate_src.rglob("*.rs")):
            files.append(path)
    return files


def strip_comments_and_strings(line: str) -> str:
    """Strip // line comments and string literals so braces inside strings
    don't affect depth tracking. Conservative: doesn't handle block comments
    or escape-aware string parsing perfectly, but the codebase does not use
    raw braces inside strings in ways that affect our heuristic.
    """
    # Strip line comments (after the first //).
    comment_idx = line.find("//")
    if comment_idx != -1:
        line = line[:comment_idx]
    # Strip simple double-quoted strings.
    line = re.sub(r'"(?:[^"\\]|\\.)*"', '""', line)
    # Strip char literals.
    line = re.sub(r"'(?:[^'\\]|\\.)'", "''", line)
    return line


def find_violations_in_file(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()

    # Stack of brace depths at which a test scope was opened. A panic is
    # allowed iff this stack is non-empty (inside any test scope).
    test_scope_stack: list[int] = []
    depth = 0
    pending_test_attr = False  # True if previous non-blank, non-attr line was a test attribute

    violations: list[str] = []

    for lineno, raw_line in enumerate(lines, start=1):
        stripped_line = strip_comments_and_strings(raw_line)
        bare = raw_line.strip()

        if not bare or bare.startswith("//"):
            continue

        is_attr_line = bare.startswith("#[") or bare.startswith("#![")
        if is_attr_line:
            if TEST_ATTR.match(raw_line):
                pending_test_attr = True
            # multiple attributes can stack; keep pending state
            continue

        # Track brace deltas for this line. Order matters: we record the
        # depth at the time of the *opening* brace of any test scope.
        opens = stripped_line.count("{")
        closes = stripped_line.count("}")

        # Detect a mod/fn/impl declaration that opens a new scope.
        opens_scope = bool(MOD_OR_FN_OPEN.search(stripped_line))

        # If a test attribute was pending, the next item it annotates is
        # this line. If this line opens a scope, that scope is a test scope.
        if pending_test_attr and opens_scope:
            # The opening brace is at current depth; the scope is from
            # depth+1 inwards. Pop it when depth drops back to current.
            test_scope_stack.append(depth)
        pending_test_attr = False if opens_scope or not is_attr_line else pending_test_attr

        # Apply opens before checking panics, then closes after.
        depth_before_line = depth
        depth += opens

        # Check this line for panics.
        if PANIC_MACROS.search(stripped_line):
            if ALLOW_MARKER not in raw_line:
                in_test_scope = bool(test_scope_stack)
                if not in_test_scope:
                    snippet = bare[:120]
                    violations.append(f"{relpath(path)}:{lineno}: {snippet}")

        depth -= closes
        # Close any test scopes whose enter-depth is now >= current depth.
        while test_scope_stack and depth <= test_scope_stack[-1]:
            test_scope_stack.pop()

    return violations


def main() -> int:
    offenders: list[str] = []
    for path in collect_files():
        offenders.extend(find_violations_in_file(path))

    if not offenders:
        print("No-panic-in-production guard OK.")
        return 0

    print(
        "Production-panic guard failed in logfwd-runtime / logfwd-output. "
        "Use `?`, structured error variants, or `// ALLOW-PANIC: <reason>` "
        "for genuinely unreachable invariants. See dev-docs/CODE_STYLE.md "
        "-> Error Handling.",
        file=sys.stderr,
    )
    print("Violations:", file=sys.stderr)
    for entry in offenders:
        print(f"  - {entry}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
