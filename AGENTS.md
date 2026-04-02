# Agent Guide

## Start here

- `README.md` — what logfwd does, performance targets
- `DEVELOPING.md` — build/test/bench commands, hard-won lessons
- `dev-docs/ARCHITECTURE.md` — pipeline data flow, scanner stages, crate map
- `dev-docs/DESIGN.md` — vision, target architecture, architecture decisions
- `dev-docs/VERIFICATION.md` — TLA+, Kani, proptest — when to use each, proof requirements, per-module status

## Reference docs

| Doc | When to read |
|-----|-------------|
| [`dev-docs/CRATE_RULES.md`](dev-docs/CRATE_RULES.md) | Per-crate rules enforced by CI |
| [`dev-docs/CODE_STYLE.md`](dev-docs/CODE_STYLE.md) | Naming, error handling, hot path rules |
| [`dev-docs/SCANNER_CONTRACT.md`](dev-docs/SCANNER_CONTRACT.md) | Scanner input requirements, output guarantees, limitations |
| [`dev-docs/PR_PROCESS.md`](dev-docs/PR_PROCESS.md) | Copilot assignment, PR triage, review criteria, merge process |
| [`dev-docs/PHASES.md`](dev-docs/PHASES.md) | Roadmap — what's done, what's in progress, what's next |
| [`dev-docs/references/arrow-v54.md`](dev-docs/references/arrow-v54.md) | RecordBatch, StringViewArray, schema evolution |
| [`dev-docs/references/datafusion-v45.md`](dev-docs/references/datafusion-v45.md) | SessionContext, MemTable, UDF creation, SQL execution |
| [`dev-docs/references/tokio-async-patterns.md`](dev-docs/references/tokio-async-patterns.md) | Runtime, bounded channels, CancellationToken, select! safety |
| [`dev-docs/references/opentelemetry-otlp.md`](dev-docs/references/opentelemetry-otlp.md) | OTLP protobuf nesting, HTTP vs gRPC |
| [`dev-docs/references/kani-verification.md`](dev-docs/references/kani-verification.md) | Kani proofs, function contracts, solver selection |

## Issue labels

| Label | Meaning |
|-------|---------|
| `bug` | Broken behavior |
| `enhancement` | New feature or improvement |
| `performance` | Performance optimization |
| `production` | Required for production readiness |
| `research` | Needs investigation before implementation |

## Before submitting

- `just ci` must pass
- See `dev-docs/VERIFICATION.md` for proof requirements (Kani, TLA+, proptest)
- See `dev-docs/CODE_STYLE.md` for style rules
