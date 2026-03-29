# Copilot Agent Instructions for logfwd

## Before Starting Any Task

1. **Read the project documentation thoroughly** before making changes:
   - `README.md` — project overview, performance targets, quick start
   - `DEVELOPING.md` — codebase structure, key design decisions, build/test/benchmark instructions
   - `TODO.md` — current project state, what's built, what's remaining
   - `docs/ARCHITECTURE.md` — v2 Arrow pipeline architecture
   - `docs/SCANNER_AND_TRANSFORM_DESIGN.md` — scanner and transform internals
   - `docs/RESEARCH_BENCHMARKS.md` — benchmark methodology and results

2. **Understand the context fully.** Read the entire issue or PR conversation, including all comments and review threads. Understand what is being asked before writing any code.

## Working on Issues

- Re-read the issue description and every comment before proposing a solution.
- If the issue references specific files, modules, or performance numbers, verify them against the actual codebase — do not assume they are current.
- Consider how your changes interact with the zero-allocation hot path and the Arrow-based pipeline architecture.

## Working on Pull Requests

- Read every file changed in the PR and understand the intent of each change.
- Read all review comments and conversation threads before responding or making changes.
- When addressing review feedback, re-read the reviewer's comment to make sure you understand exactly what they are asking for.

## Code Quality Requirements

- **Run `cargo test` before considering any change complete.** All tests must pass.
- **Run `cargo clippy` and fix any warnings** introduced by your changes.
- This is a performance-critical Rust project. Do not introduce per-line heap allocations in the hot path.
- Follow existing code style and patterns — no async runtime, no unnecessary abstractions.
- Do not add dependencies without justification.

## Double-Check Your Work

- After writing code, review your own changes line by line before committing.
- Verify that your changes compile (`cargo check`) and pass tests (`cargo test`).
- If you modified a module, re-read the module's existing tests to ensure your changes don't break assumptions.
- If your change affects performance-sensitive code paths, note this explicitly.
- Re-read the original issue or review comment one final time to confirm you addressed everything that was asked.
