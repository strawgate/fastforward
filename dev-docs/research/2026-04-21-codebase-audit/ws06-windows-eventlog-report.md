# WS06 — Windows Event Log Feasibility Report (Issue #1476)

> **Date:** 2026-04-21
> **Status:** Active research (design only; no implementation)
> **Context:** Evaluate feasibility of adding a Windows Event Log input for `logfwd` using Win32 WEVT APIs and Rust bindings.

---

## Executive summary

**Recommendation: GO (phased).**

Windows Event Log ingestion is feasible with acceptable technical risk if implemented in phases:

1. **MVP (recommended):** `EvtSubscribe` (push mode) + `EvtRenderEventXml` + bookmark persistence as XML.
2. **Optimization phase:** `EvtRenderEventValues` with explicit render contexts for hot-path extraction and lower parse overhead.
3. **Hardening phase:** richer channel/query UX, strict gap detection, and expanded test matrix.

Key reasons:

- Win32 APIs are stable since Vista/Server 2008 and explicitly support subscription, rendering, and bookmarks.
- Current Rust ecosystem provides direct API surface via `windows`/`windows-sys` bindings.
- Bookmark persistence model can map cleanly to checkpoint semantics with a new Windows-native checkpoint payload.
- Linux CI can cross-compile Windows artifacts (`*-windows-gnu`) even if runtime tests require Windows hosts.

---

## 1) API accessibility from Rust

### 1.1 Win32 API capability recap

The canonical Windows Event Log API (`wevtapi`) provides all primitives needed for tailing-like ingestion:

- **Subscribe:** `EvtSubscribe` supports both **push** (callback) and **poll** models; can start at oldest record, future only, or after bookmark.
- **Enumerate:** `EvtNext` consumes query/subscription result handles in batches.
- **Render:** `EvtRender` supports XML (`EvtRenderEventXml`) and structured property extraction (`EvtRenderEventValues` via `EVT_VARIANT[]`).
- **Checkpointing:** `EvtCreateBookmark` / `EvtUpdateBookmark` / `EvtRender(...EvtRenderBookmark...)` support durable resume state.

### 1.2 Rust bindings status

#### `windows` / `windows-sys`

Feasible and mature path.

- The `windows` crate module `windows::Win32::System::EventLog` exposes Event Log structs, constants, callback type aliases, and functions including:
  - `EvtSubscribe`, `EvtNext`, `EvtRender`, `EvtCreateBookmark`, `EvtUpdateBookmark`.
- This indicates no custom C shim is required for baseline integration.

**Implication:** Prefer `windows-sys` for minimal overhead FFI-style calls, or `windows` for richer wrappers. Either supports this project.

#### `winlog` / `winlog2`

Not a fit for ingestion.

- `winlog` and `winlog2` are primarily **logging backends that write messages to Windows Event Log**, not subscription readers.
- They do not remove the need to call `EvtSubscribe`/`EvtNext` for collection.

#### `evtx`

Useful but **offline-only** for this use case.

- `evtx` is a strong crate for parsing `.evtx` files.
- It is useful for offline backfills or fixture generation, but does **not** replace live subscription against channels.

### 1.3 FFI complexity assessment

Complexity is **moderate**, not high:

- Handle lifetime discipline (`EvtClose` on subscription/event/context/bookmark handles).
- Two-step buffer sizing patterns (`ERROR_INSUFFICIENT_BUFFER`) around `EvtRender`.
- Callback ABI + thread-safety in push mode.
- UTF-16 strings and XML conversion.

No blocking complexity found (no kernel driver, no COM mandatory layer, no undocumented APIs).

---

## 2) Data model mapping to logfwd

## 2.1 Source event shape from Windows

The Windows System schema exposes (among others):

- `EventID`
- `Level`
- `TimeCreated` (`SystemTime`)
- `Provider` (`Name`/`Guid`)
- `Channel`
- `Computer`
- `Security/@UserID`
- `EventRecordID`
- plus `EventData` / `UserData` payloads.

This is sufficient to build deterministic log records with stable identity fields.

### 2.2 Proposed canonical mapping (OTLP-like orientation)

Suggested mapping for each emitted log record:

- **Timestamp:** `System/TimeCreated/@SystemTime` → `time_unix_nano`
- **SeverityText:** from `Level` symbolic mapping (`Critical`, `Error`, etc.)
- **SeverityNumber:** mapped per table below
- **Body:**
  - MVP: entire rendered XML string (lossless)
  - Optional: provider-formatted message if `EvtFormatMessage` is later added
- **Attributes:**
  - `windows.event.channel` ← `System/Channel`
  - `windows.event.provider` ← `System/Provider/@Name`
  - `windows.event.provider_guid` ← `System/Provider/@Guid` (if present)
  - `windows.event.id` ← `System/EventID`
  - `windows.event.record_id` ← `System/EventRecordID`
  - `windows.event.computer` ← `System/Computer`
  - `windows.event.user_sid` ← `System/Security/@UserID` (if present)
  - `windows.event.task`, `windows.event.opcode`, `windows.event.keywords` when available
  - extracted `EventData` / `UserData` keys as namespaced attrs

### 2.3 Windows level → OTLP severity mapping

Windows predefined levels: Critical=1, Error=2, Warning=3, Informational=4, Verbose=5.
OpenTelemetry severity ranges: TRACE (1–4), DEBUG (5–8), INFO (9–12), WARN (13–16), ERROR (17–20), FATAL (21–24).

**Recommended mapping (conservative, monotonic):**

| Windows Level | Meaning | OTLP SeverityText | OTLP SeverityNumber |
|---|---|---|---:|
| 0 | LogAlways / unspecified | (omit or `INFO`) | 0 or 9 |
| 1 | Critical | `FATAL` | 21 |
| 2 | Error | `ERROR` | 17 |
| 3 | Warning | `WARN` | 13 |
| 4 | Informational | `INFO` | 9 |
| 5 | Verbose | `DEBUG` (or TRACE via config) | 5 |
| 6–15 | Reserved | `INFO` default + raw level attr | 9 |
| 16–255 | Provider-defined | configurable; default `INFO` + raw level attr | 9 |

**Note:** preserve original raw numeric level in `windows.event.level_raw` to avoid semantic loss.

### 2.4 Structured extraction strategy from EventData/UserData XML

Two viable approaches:

- **MVP:** render full event XML, parse `<EventData><Data Name="...">` and `<UserData>` via XML parser.
- **Optimized:** use `EvtCreateRenderContext` + `EvtRenderEventValues` to pull selected XPaths into typed `EVT_VARIANT`s.

Recommendation:

- Start with XML (simpler, robust across providers).
- Add optional render-context fast path later for high-volume channels.

---

## 3) Subscription and checkpoint design

### 3.1 Channel selection

Support explicit channel list:

- default: `Application`, `System`
- opt-in: `Security` (permission-sensitive)
- custom channels allowed

Important constraint: subscription API path is for **Admin/Operational** channels (not Analytic/Debug in normal usage).

### 3.2 Query filtering model

Expose either:

1. simple channel + optional minimal predicates (level/event-id/provider), or
2. advanced raw XPath / structured XML query.

Recommendation: provide both with validation; advanced users need raw XPath/structured XML parity.

### 3.3 Push vs pull

- **Default:** push model (`EvtSubscribe` callback), lower control-plane complexity.
- **Fallback/advanced:** poll model (`SignalEvent` + `EvtNext`) for environments that prefer explicit polling loops.

### 3.4 Bookmark/checkpoint persistence

Windows-native resume should be bookmark-centric, not byte-offset-centric.

Proposed checkpoint record per (source/channel/query):

```json
{
  "kind": "windows_eventlog",
  "source_id": "winlog:Application:hash(query)",
  "bookmark_xml": "<BookmarkList>...</BookmarkList>",
  "last_event_record_id": "123456",
  "updated_at_unix_nano": "..."
}
```

Flow:

1. On startup, load persisted `bookmark_xml`.
2. `EvtCreateBookmark(bookmark_xml)`.
3. `EvtSubscribe(..., Bookmark, ..., EvtSubscribeStartAfterBookmark)`.
4. After successful emission, call `EvtUpdateBookmark(bookmark, event)`.
5. Periodically render bookmark (`EvtRender(...EvtRenderBookmark...)`) and persist.

### 3.5 Mapping to `SourceId/ByteOffset` model

Current byte-offset model is file-stream-centric. For Windows Event Log:

- **Keep `SourceId`** semantics (channel + query identity).
- Replace `ByteOffset` meaning with backend-specific resume token:
  - `ByteOffset` can be unused/sentinel for this source type, OR
  - evolve checkpoint schema to tagged union with `{ kind: file | windows_eventlog, ... }`.

**Recommended:** tagged union checkpoint model (future-proof for journald/kafka/etc.).

---

## 4) Cross-compilation strategy (Linux CI → Windows targets)

### 4.1 Target choices

- `x86_64-pc-windows-msvc`: tier-1 target, but non-Windows-host cross-compilation is documented as possible yet **not supported**.
- `x86_64-pc-windows-gnu`: easier cross-compilation from Linux when MinGW/LLVM toolchain is installed.

### 4.2 Practical CI recommendation

1. **Compile check in Linux CI:** `x86_64-pc-windows-gnu`.
2. **Runtime/integration tests:** Windows CI runners (GitHub Actions `windows-latest`).
3. Optionally add `windows-msvc` build job on Windows host for release parity.

### 4.3 Feature-gating layout

- Add Windows collector module behind `cfg(target_os = "windows")`.
- Provide a stub/no-op or compile-time exclusion on non-Windows.
- Keep shared normalization/mapping logic platform-agnostic where possible.

### 4.4 Testing without Windows host

On Linux/macOS:

- Unit-test pure mapping/parsing code with canned XML fixtures.
- Snapshot tests for severity mapping and attribute extraction.
- Cross-compile smoke tests for Windows targets.

On Windows:

- Integration tests using known channels (`Application`) and generated test events.
- Resume tests validating bookmark persistence and restart semantics.

---

## 5) Effort estimate

Assuming one engineer familiar with Rust + telemetry pipelines.

### Phase A — MVP (2 to 3 weeks)

- Windows-only source scaffolding
- `EvtSubscribe` push ingest loop
- XML render path
- basic schema mapping
- bookmark persistence + recovery
- unit tests + minimal Windows integration tests

### Phase B — Production hardening (1 to 2 weeks)

- error taxonomy + retries
- strict mode gap detection
- more channel/query validation
- checkpoint durability tuning
- observability metrics for the collector

### Phase C — Optimization (1 to 2 weeks)

- `EvtRenderEventValues` fast path
- optional `EvtFormatMessage`
- perf tuning for high event rates

**Total:** ~4 to 7 engineer-weeks for robust production readiness.

---

## 6) Go / no-go recommendation

## Decision: **GO**

Rationale:

- API coverage exists and is stable.
- Rust bindings are readily available and current.
- Checkpointing model is solvable with bookmark XML persistence.
- CI strategy is straightforward with split compile/runtime coverage.

### Main risks to manage

- Bookmark correctness and persistence frequency tradeoffs.
- Channel permissions (especially `Security`) and operator UX.
- XML parsing cost at very high throughput.

### Risk mitigations

- Start with safe XML MVP + clear backpressure/drop metrics.
- Add configurable flush interval for bookmark writes.
- Add fast-path rendering only when needed by benchmarks.

---

## Suggested implementation shape (non-binding)

- `input/windows_eventlog/` module (Windows-only)
  - `subscriber.rs` (EvtSubscribe loop)
  - `renderer.rs` (XML/value rendering)
  - `bookmark.rs` (create/update/render/persist)
  - `mapping.rs` (event → normalized record)
- shared checkpoint trait extended to typed checkpoint payloads.

---

## References (accessed 2026-04-21)

1. Microsoft Learn — `EvtSubscribe` (push/poll, flags, channel/query constraints): https://learn.microsoft.com/en-us/windows/win32/api/winevt/nf-winevt-evtsubscribe
2. Microsoft Learn — `EvtNext`: https://learn.microsoft.com/en-us/windows/win32/api/winevt/nf-winevt-evtnext
3. Microsoft Learn — `EvtRender`: https://learn.microsoft.com/en-us/windows/win32/api/winevt/nf-winevt-evtrender
4. Microsoft Learn — `EvtCreateBookmark`: https://learn.microsoft.com/en-us/windows/win32/api/winevt/nf-winevt-evtcreatebookmark
5. Microsoft Learn — `EvtUpdateBookmark`: https://learn.microsoft.com/en-us/windows/win32/api/winevt/nf-winevt-evtupdatebookmark
6. Microsoft Learn — `SystemPropertiesType` schema: https://learn.microsoft.com/en-us/windows/win32/wes/eventschema-systempropertiestype-complextype
7. Microsoft Learn — `LevelType` predefined severities: https://learn.microsoft.com/en-us/windows/win32/wes/eventmanifestschema-leveltype-complextype
8. windows crate docs (`windows::Win32::System::EventLog` module, functions/constants): https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/System/EventLog/index.html
9. crates.io API — `windows`: https://crates.io/api/v1/crates/windows
10. crates.io API — `windows-sys`: https://crates.io/api/v1/crates/windows-sys
11. crates.io API — `winlog`: https://crates.io/api/v1/crates/winlog
12. crates.io API — `winlog2`: https://crates.io/api/v1/crates/winlog2
13. crates.io API — `evtx`: https://crates.io/api/v1/crates/evtx
14. OpenTelemetry Logs Data Model (SeverityNumber ranges): https://opentelemetry.io/docs/specs/otel/logs/data-model/
15. rustc book — `*-pc-windows-msvc`: https://doc.rust-lang.org/beta/rustc/platform-support/windows-msvc.html
16. rustc book — `*-windows-gnu`: https://doc.rust-lang.org/rustc/platform-support/windows-gnu.html
