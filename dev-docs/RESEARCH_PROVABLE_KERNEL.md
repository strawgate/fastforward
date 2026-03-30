# Provable Kernel Design — Working Document

Status: Research phase. Deep research agents running on architecture
patterns, Kani limits, enforcement mechanisms, and boundary design.

## The idea

Separate the codebase into a **proven kernel** crate and an **outer
shell**. The kernel contains every function where a bug means silent
data corruption or loss. The shell contains IO, async orchestration,
Arrow integration, config parsing — things that fail loudly.

The kernel has strict rules:
- No IO, no async, no unsafe
- Minimal dependencies (ideally no_std + alloc, or std with enforced restrictions)
- Every public function has a Kani proof or proptest harness
- Guarantees are enforced by CI — the kernel can't degrade over time

## What goes in the kernel (current thinking)

**Byte classification** (~200 lines)
- `find_quotes_and_backslashes()` — SIMD character detection
- `compute_real_quotes()` — escape detection with block carry
- `prefix_xor()` — parallel prefix XOR for string interior mask
- `ChunkIndex::new()` — composition of above into structural index

Why: if any of this is wrong, the scanner silently misparses every
JSON string with backslashes. All downstream data is corrupted.

**JSON parsing** (~170 lines after restructuring)
- `parse_json_line()` — new pure function: line bytes → typed field list
- Type inference: string vs int vs float vs null vs nested
- Key extraction, value boundary detection
- Currently tangled with builder dispatch in `scan_line()`

Why: type misclassification silently puts data in the wrong column.
Wrong value boundaries silently truncate or extend field values.

**Line boundary detection** (~180 lines)
- `JsonParser::process()` — partial line carry across read boundaries
- `RawParser::process()` — same with JSON escaping
- `CriParser::process()` — CRI format parsing + partial carry

Why: wrong line boundaries silently merge or split log records.
Partial line carry bugs silently lose data at read chunk boundaries.

**CRI parsing and reassembly** (~160 lines)
- `parse_cri_line()` — space-delimited field extraction
- `CriReassembler::feed()` — partial line buffering and emission

Why: wrong CRI parsing silently corrupts Kubernetes log metadata.
Wrong reassembly silently loses or merges partial container log lines.

**Number parsing** (~55 lines)
- `parse_int_fast()` — i64 with overflow detection
- `parse_float_fast()` — f64 via std

Why: overflow bugs silently produce wrong numeric values.

**Timestamp parsing** (~80 lines)
- `parse_timestamp_nanos()` — ISO 8601 → nanoseconds since epoch
- `days_from_civil()` — Gregorian date → days since epoch

Why: date math bugs put timestamps off by days or years.

**Protobuf wire format** (~150 lines)
- `encode_varint()`, `varint_len()`, `decode_varint()`
- `encode_tag()`, `encode_bytes_field()`, `bytes_field_size()`
- OTLP log record encoding (after extraction from OtlpSink)

Why: wire format bugs corrupt the entire OTLP payload. Downstream
collectors reject or misinterpret the data.

**Pipeline state machine** (~100 lines, new)
- `PipelineState` + `Event` → `(PipelineState, Action)`
- Encodes: when to flush, how to drain, shutdown ordering

Why: wrong state transitions silently abandon buffered data.
This is where we've already had 3 bugs (data loss, deadlock,
flush starvation).

**Column accumulator logic** (~100 lines, new extraction)
- `ColumnAccumulator` — tracks (row_id, value) pairs per field
- Null bitmap computation
- Duplicate key detection (first-write-wins within a row)

Why: wrong null bitmaps silently produce null values for rows that
had data, or non-null values for rows that didn't. Duplicate key
bugs silently overwrite values.

**Estimated total: ~1,200-1,400 lines**

## What stays outside the kernel

| Component | Why |
|-----------|-----|
| Arrow builders (StreamingBuilder, StorageBuilder) | Depend on Arrow crate types |
| SqlTransform | DataFusion — async, complex, external project |
| File tailer | IO |
| Output sinks (OTLP, JSON, stdout) | IO + HTTP |
| Pipeline async loop | tokio, channels |
| Config parsing | serde, YAML |
| Diagnostics / metrics | OTel, HTTP server |
| CLI / signal handling | User-facing, OS interaction |

## Architecture decisions (resolved by research)

1. **`#![no_std]` + alloc + hashbrown.** Deep research unanimous:
   no_std gives structural enforcement — compiler blocks IO, not
   lints. hashbrown solves HashMap gap (same impl as std since 1.36).
   CI: `cargo build --target thumbv6m-none-eabi` catches std leaks.
   Reverses fast agent recommendation. This is the rustls/s2n-quic
   pattern.

2. **Generic trait boundary + secondary type API.** Rename ScanBuilder
   to FieldSink, move to kernel. Zero allocation via monomorphization.
   Also provide ParsedLine type API for testing/flexible consumers
   (serde Visitor + serde_json::Value dual pattern).

3. **Scalar fallback in kernel, SIMD outside.** Kani proves scalar.
   proptest verifies SIMD matches scalar. `#![forbid(unsafe_code)]`
   in kernel; SIMD lives in logfwd-core behind cfg.

4. **ChunkIndex lives in kernel.** Bitmask logic is pure and provable.

5. **memchr: no_std compatible, stub for Kani.** memchr supports
   `default-features = false` for no_std. Kani stubs it with naive
   loop via `stub_verified`. Contracts define expected behavior.

6. **Kani native contracts only.** `requires`/`ensures` +
   `proof_for_contract` + `stub_verified` for compositional proofs.
   Kani contracts are more powerful than the `contracts` crate and
   avoid redundancy.

7. **TLA+ for pipeline liveness.** Kani cannot prove temporal
   properties. TLA+ specification needed for batching/timeout/shutdown
   protocol. Design-level artifact, not code.

8. **Proof coverage CI.** Every public kernel function must have a
   Kani proof. CI script checks and fails on unproven functions.

9. **SAT solver per-harness.** Default Kissat (up to 265x faster
   than MiniSat). s2n-quic uses `kani::solver(kissat)` per-harness.

10. **Investigate Verus/Vest.** Vest (verified parsing library) may
    push text parsing past Kani's ~16 byte limit. Future work.

11. **~60-80% of kernel logic provable** via Kani + contracts.
    Higher than fast agent's 15-25% estimate. The difference:
    compositional verification via function contracts dramatically
    extends reach beyond raw input-size limits.

## Research findings so far

### Kani practical limits (agent completed)

**Kani's sweet spot**: u64 bitmask operations (exhaustive, seconds),
state machine single-step transitions (exhaustive), varint roundtrips
(exhaustive for all u64), bounded accumulators (N≤50 rows).

**Kani's hard limits**:
- memchr: NOT SUPPORTED (issue #632). Must be stubbed with naive loop.
  This affects scanner, format parsers, and CRI parser — all use memchr.
- HashMap: intractable for symbolic verification. Field_index must use
  a simpler data structure in kernel (sorted Vec, BTreeMap, or static
  array) if we want Kani to verify functions that reference it.
- Loops over &[u8]: ~32 bytes max before timeout. JSON parsing with
  realistic inputs (100+ bytes) is not Kani-verifiable.
- Nested function calls: deep call stacks with dyn dispatch timeout.
  Static dispatch (monomorphized) works. Function contracts help.

**Implications for kernel design**:
- escape detection (compute_real_quotes): PROVABLE. Pure u64 bitmask ops.
- prefix_xor: PROVABLE. Single u64 input.
- parse_int_fast: PROVABLE for ≤20 byte inputs (i64 max is 19 digits).
- parse_timestamp_nanos / days_from_civil: PROVABLE for bounded dates.
- varint encode/decode: PROVABLE exhaustively.
- pipeline state machine: PROVABLE for single transitions, bounded for
  short sequences.
- parse_json_line: NOT Kani-provable at realistic sizes. proptest with
  serde_json oracle instead.
- FormatParser state machines: NOT Kani-provable (uses memchr).
  proptest-state-machine instead.
- CRI parser: NOT Kani-provable (uses memchr). proptest instead.

**Alternative tools for what Kani can't handle**:
- Verus (SMT-based): supports loop invariants for unbounded proofs.
  Asterinas OS proved 2,000 lines with it (3:1 spec-to-code ratio).
- Flux (refinement types): compile-time bounds checking. Research stage.
- proptest-state-machine: generate random operation sequences, compare
  against reference model. Best practical option for parsers/builders.

**Realistic estimate**: 15-25% of kernel logic is formally provable
with Kani. The rest needs proptest + fuzzing.

### Enforcement + crate mechanics (agent completed)

**Use `std`, not `no_std`.** Enforce no-IO via Clippy `disallowed-types`
and `disallowed-methods` in a per-crate `clippy.toml`. This is how real
projects handle it. `no_std` buys us nothing if we need alloc (Vec,
String, BTreeMap), and the ergonomic costs are real.

**Recommended crate-level lints:**
```toml
# Cargo.toml [lints] section
[lints.rust]
unsafe_code = "forbid"
missing_docs = "deny"
unused_must_use = "deny"

[lints.clippy]
all = "deny"
pedantic = "deny"
disallowed_types = "deny"
disallowed_methods = "deny"
```

**clippy.toml bans IO operations:**
```toml
disallowed-types = [
    { path = "std::fs::File", reason = "kernel: no IO" },
    { path = "std::net::TcpStream", reason = "kernel: no IO" },
    ...
]
disallowed-methods = [
    { path = "std::fs::read", reason = "kernel: no IO" },
    { path = "std::thread::spawn", reason = "kernel: no threads" },
    ...
]
```

**Dependency restriction**: cargo-deny can't enforce per-crate within a
workspace. Use CI script: `cargo metadata | jq ... | check against allowlist`.

**Kani CI**: GitHub Action `model-checking/kani-github-action@v1.1`.
Cache `~/.cargo/bin/kani*` and `~/.kani/` for fast reruns. Linux only.

**Dual contract system**:
- `contracts` crate `debug_requires`/`debug_ensures` — runtime assertions
  in debug builds, stripped in release.
- Kani native `#[kani::requires]`/`#[kani::ensures]` — formal proof.
  Use `stub_verified` for modular composition: prove each function's
  contract, then stub verified functions when proving their callers.

**cargo-mutants**: validates whether proofs/tests catch real mutations.
Complements formal verification — Kani says "this property holds";
cargo-mutants says "your tests would notice if someone changed this."

**bolero**: unified harness for proptest + Kani. Same `#[test]` runs as
random test via `cargo test` AND as Kani proof via `cargo kani --tests`.
Pattern: `#[cfg_attr(kani, kani::proof)]` on the test function.

### Boundary design patterns (agent completed)

**We already have the right pattern.** The existing `ScanBuilder` trait
in scanner.rs IS the kernel/shell boundary:

```rust
pub(crate) trait ScanBuilder {
    fn begin_row(&mut self);
    fn end_row(&mut self);
    fn resolve_field(&mut self, key: &[u8]) -> usize;
    fn append_str_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_int_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_float_by_idx(&mut self, idx: usize, value: &[u8]);
    fn append_null_by_idx(&mut self, idx: usize);
    fn append_raw(&mut self, line: &[u8]);
}
```

Generic `B: ScanBuilder` (monomorphized, not dyn) gives:
- Zero allocation at the boundary
- Full inlining via monomorphization
- Kani can verify for a concrete mock type
- Trait defined in kernel, implemented in outer crate

**Don't use SmallVec/Vec intermediary.** The scanner discovers fields
incrementally — there's no natural "return a batch of fields" point.
Callback/trait is strictly better: zero allocation, zero intermediary,
data flows directly from scanner to builder in registers/stack.

**Zero-copy is preserved.** Kernel passes `&[u8]` slices from the
input buffer. StreamingBuilder stores (offset, len) views. Final
StringViewArray references the original bytes::Bytes buffer.

**arrow-json uses the "Tape" pattern**: parse JSON into a flat
Vec<TapeElement> intermediary, then decode into Arrow arrays. This
is the type-boundary approach. Works but requires a copy. Our
callback approach is faster.

**Error handling**: kernel returns a simple `#[non_exhaustive] Copy`
enum. No strings, no allocation. Outer crate wraps with thiserror
and adds context (byte offset, line number).

**For evolution**: use default method implementations on the trait
to add new field types without breaking existing impls. Mark enums
`#[non_exhaustive]`.

## Open questions (updated)

### Resolved

1. **no_std or std?** → Use std. Enforce no-IO via clippy.toml
   `disallowed-types`/`disallowed-methods`. (Study 2)

2. **Trait or type boundary?** → Trait (generic, not dyn). We already
   have `ScanBuilder`. Rename to `FieldSink`, move to kernel. (Study 4)

3. **SIMD in the kernel?** → Scalar fallback in kernel. SIMD stays
   outside, behind a trait or cfg. Kani proofs on scalar path.
   proptest verifies SIMD path matches scalar. (Studies 1, 3)

4. **memchr in kernel?** → Stub memchr with a Kani contract. Kernel
   code uses memchr normally. When running under Kani, memchr is
   replaced with a naive loop via `stub_verified`. (Study 3)

5. **HashMap in kernel?** → Use BTreeMap or sorted Vec for field_index
   in kernel code. Or stub HashMap for Kani. Real HashMap used at
   runtime, Kani verifies with simpler structure. (Study 3)

### Still open

6. **Modular verification depth**: How many levels of stub_verified
   composition is practical? Can we chain: prove compute_real_quotes →
   stub in ChunkIndex::new → stub in parse_json_line? (Waiting: Study 1)

7. **CI run time**: How long does `cargo kani` take on ~20 proof
   harnesses? Is it fast enough for every PR? (Need to prototype)

8. **Migration order**: What order minimizes breakage when extracting
   the kernel crate? (Need to prototype)

## What each research study tells us

### Study 1: Proven kernel architectures (COMPLETED)
rustls, s2n-quic, CosmWasm, Wasmtime, Ockam, Ferrocene analyzed.
Three strategies: compiler-enforced purity (rustls), formal proofs
in core (s2n-quic), DSL for verifiable rules (Cranelift ISLE).
Key finding: orphan rule is the #1 crate-splitting blocker. Plan
type boundaries to avoid trait impls on foreign types.

### Study 2: Enforcement + crate mechanics (COMPLETED)
Concrete Cargo.toml for no_std kernel with bolero + Kani. 7-job CI
pipeline. Proof coverage script. cargo-vet per-crate policy. 5-PR
migration plan starting with lowest-dependency modules. Key finding:
mutation testing (cargo-mutants) weekly, not every PR.

### Study 3: Kani limits + max provability (COMPLETED)
60-80% of pure logic provable via contracts. JSON parsing ~16 bytes
max. State machines exhaustive single-step. Varint full u64 range.
Temporal properties need TLA+. Verus/Vest for pushing parser
verification further. "A partially verified data processing kernel
would be novel in the field."

### Study 4: Boundary design patterns (COMPLETED)
Visitor pattern (serde model) is definitive. ring's error opacity
for kernel errors. SmallVec worse than Vec in benchmarks. Buffer
reuse eliminates allocation cost. Arena (bumpalo) as alternative.
Roundtrip property (kernel → Arrow → kernel = identity) is the
single most powerful boundary test.

## Known risks

- **Over-engineering**: The kernel boundary adds complexity. If the
  proofs don't catch real bugs, it's pure cost.

- **Kani might be too slow**: If proofs take 30+ minutes in CI,
  they'll be disabled or ignored.

- **Boundary type duplication**: Kernel types + Arrow types means
  every value is represented twice in the type system.

- **Performance regression**: Extra allocation at the boundary could
  slow the hot path below 1M lines/sec target.

- **Migration pain**: Moving code between crates breaks imports,
  requires updating all callers, may uncover hidden coupling.

## Concrete kernel design

### Crate structure

```
logfwd-kernel/
  Cargo.toml           # no_std+alloc, forbid(unsafe_code), hashbrown, memchr
  src/
    lib.rs             # #![forbid(unsafe_code)], pub modules
    sink.rs            # FieldSink trait (the boundary)
    classify.rs        # ChunkIndex, compute_real_quotes, prefix_xor
    parse.rs           # scan_into, scan_line (generic over FieldSink)
    format.rs          # JsonParser, RawParser, CriParser
    cri.rs             # parse_cri_line, CriReassembler
    number.rs          # parse_int_fast, parse_float_fast
    timestamp.rs       # parse_timestamp_nanos, days_from_civil
    wire/
      mod.rs
      varint.rs        # encode_varint, varint_len, decode_varint
      tag.rs           # encode_tag, encode_bytes_field, bytes_field_size
      otlp.rs          # encode_log_record (typed fields → protobuf)
    pipeline/
      mod.rs
      state.rs         # PipelineState, Event, Action, next_action
      token.rs         # BatchToken (#[must_use], linear ack)
    accumulator.rs     # ColumnAccumulator (null bitmap, dedup detection)
    error.rs           # ParseError (#[non_exhaustive], Copy)
    config.rs          # ScanConfig, FieldSpec (no serde, just types)
```

### What each component gets

| Component | Kani proof | proptest | contracts | cargo-mutants |
|-----------|-----------|----------|-----------|---------------|
| compute_real_quotes | Exhaustive (all u64×u64×bool) | Oracle vs naive impl | requires/ensures | Yes |
| prefix_xor | Exhaustive (all u64) | Oracle vs naive | ensures | Yes |
| parse_int_fast | Exhaustive (≤20 bytes) | Oracle vs std parse | ensures(overflow detected) | Yes |
| parse_float_fast | — | Oracle vs std parse | — | Yes |
| days_from_civil | All valid dates [1970,2100] | Oracle vs chrono | ensures(no overflow) | Yes |
| parse_timestamp_nanos | Bounded (≤32 bytes) | Oracle vs chrono | ensures(no overflow) | Yes |
| varint encode/decode | Exhaustive roundtrip (all u64) | — | ensures(roundtrip) | Yes |
| encode_tag | Exhaustive (field≤2^16, wire≤7) | — | ensures | Yes |
| bytes_field_size | Exhaustive (bounded) | — | ensures(matches encode) | Yes |
| scan_line | Bounded (≤128 byte line, ≤8 fields) | Oracle vs serde_json | — | Yes |
| parse_cri_line | Bounded (≤256 bytes) | Fuzz + oracle | ensures(valid slices) | Yes |
| CriReassembler | — | State machine (random feed/reset) | debug_ensures | Yes |
| JsonParser | — | State machine (random process/reset) | debug_ensures(no data loss) | Yes |
| RawParser | — | State machine + JSON validity | debug_ensures | Yes |
| CriParser | — | State machine (random chunks) | debug_ensures | Yes |
| PipelineState | Exhaustive single-step | Random event sequences | ensures(no data abandoned) | Yes |
| BatchToken | — | — | Compile-time (#[must_use] + Drop) | — |
| ColumnAccumulator | Bounded (≤30 rows, ≤8 fields) | State machine | ensures(bitmap correct) | Yes |

### Verification tiers

**Tier 1 — Exhaustive (Kani proves for ALL inputs):**
- compute_real_quotes, prefix_xor: all possible u64 bitmask inputs
- parse_int_fast: all possible ≤20 byte numeric strings
- varint encode/decode: all u64 values
- PipelineState single-step: all (State, Event) pairs

**Tier 2 — Bounded (Kani proves for inputs up to size N):**
- scan_line: all JSON lines ≤128 bytes with ≤8 fields
- parse_cri_line: all CRI lines ≤256 bytes
- days_from_civil: all valid dates in [1970, 2100]
- ColumnAccumulator: all operation sequences with ≤30 rows, ≤8 fields

**Tier 3 — Statistical (proptest, not proof but high confidence):**
- FormatParser state machines: random chunk sequences, oracle comparison
- CriReassembler: random feed/reset sequences, line conservation
- scan_line for large inputs: oracle vs serde_json with 10,000+ cases
- Pipeline event sequences: random events, verify no-data-abandoned

**Tier 4 — Compile-time (Rust type system):**
- BatchToken: #[must_use] + Drop panic on unconsumed token
- ParseError: #[non_exhaustive], Copy, no allocation
- FieldSink: sealed trait, default methods for evolution
- clippy.toml: IO operations banned at compile time

### Implementation phases

**Phase 0: Prototype Kani (1-2 days)**
Before committing to the full kernel extraction, prove ONE thing
with Kani to validate the tooling:
- Add Kani to workspace dev-deps
- Write proof for prefix_xor (trivial, should complete in seconds)
- Write proof for compute_real_quotes (the real test)
- Set up CI with kani-github-action
- Measure: how long do proofs take? Does CI work?

If this phase reveals Kani is impractical (too slow, too fragile,
CI setup too painful), we stop here and proceed with proptest-only.

**Phase 1: Create logfwd-kernel crate (3-4 days)**
- Create crate with std, forbid(unsafe_code), clippy.toml
- Move chunk_classify.rs (scalar path only) → kernel/classify.rs
- Move format.rs → kernel/format.rs
- Move cri.rs → kernel/cri.rs
- Move scan_config.rs parse functions → kernel/number.rs
- Move otlp.rs wire functions → kernel/wire/
- Define FieldSink trait in kernel/sink.rs
- Update logfwd-core to depend on kernel, re-export as needed
- All existing tests must still pass

**Phase 2: Kani proofs for Tier 1 (3-4 days)**
- Exhaustive proofs for bitmask operations
- Exhaustive proofs for varint roundtrip
- Exhaustive proofs for parse_int_fast
- Kani contracts on all Tier 1 functions
- CI runs proofs on every PR

**Phase 3: Restructure scan_line + add FieldSink (3-4 days)**
- Refactor scan_line to call through generic FieldSink
- Move scan_into and scan_line to kernel (generic over FieldSink)
- StreamingBuilder and StorageBuilder implement FieldSink in core
- Add Kani bounded proof for scan_line (≤128 bytes)
- Add proptest oracle vs serde_json for scan_line

**Phase 4: Extract pipeline state machine (2-3 days)**
- Define PipelineState, Event, Action in kernel
- Extract decisions from run_async into pure next_action()
- Kani proof for all single-step transitions
- proptest for random event sequences
- Wire run_async to call next_action for decisions

**Phase 5: proptest state machines + contracts (3-4 days)**
- proptest-state-machine for FormatParser (JsonParser, CriParser)
- proptest-state-machine for CriReassembler
- proptest-state-machine for ColumnAccumulator
- Add `contracts` crate debug_requires/debug_ensures on all kernel fns
- cargo-mutants validation pass

**Phase 6: BatchToken + linear types (1-2 days)**
- Implement BatchToken with #[must_use] + Drop
- Wire into pipeline state machine
- Compile-time enforcement of ack/nack

**Total: ~16-21 days (with Phase 0 as go/no-go gate)**

## What we're NOT doing

- Verifying DataFusion SQL (too complex, external project)
- Verifying Arrow builder internals (Arrow's responsibility)
- Verifying async/tokio behavior (use integration tests instead)
- Proving the entire pipeline end-to-end (intractable)
- Building a custom proof assistant or DSL
