---
title: "Building & Testing"
description: "Quick reference for building and testing logfwd"
---

This page is a quick reference. For workspace layout, profiling, and hard-won development lessons, see [Contributing](/memagent/development/contributing/).

## Prerequisites

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install just
just install-tools
```

## Common commands

```bash
just build          # Release binary
just test           # All tests
just lint           # fmt + clippy + toml + deny + typos
just ci             # Full CI suite (lint + test)
just bench          # Criterion microbenchmarks
```

## Quick iteration

```bash
cargo test -p logfwd-core                  # Core crate only (fastest)
cargo test -p logfwd-io -- tail            # Specific test subset
just clippy                                # Lint only (no test)
```

## Project structure

```
crates/logfwd/           # Binary entry point, CLI
crates/logfwd-runtime/   # Async pipeline orchestration
crates/logfwd-core/      # Scanner, parsers, state machine (no_std)
crates/logfwd-arrow/     # Arrow builders, SIMD backends
crates/logfwd-config/    # YAML config parser
crates/logfwd-io/        # File tailing, network inputs, checkpointing
crates/logfwd-output/    # Output sinks (OTLP, HTTP, ES, Loki, stdout)
crates/logfwd-transform/ # SQL transforms via DataFusion, UDFs
crates/logfwd-bench/     # Benchmarks and profiling tools
```
