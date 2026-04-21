# macOS OSLog / Unified Logging Feasibility Report (Issue #1478)

> **Status:** Active research
> **Date:** 2026-04-21
> **Context:** Feasibility of adding a macOS-native log source for `logfwd`
- **Audience:** logfwd maintainers (input pipeline, OTLP mapping, platform support)

---

## Executive summary

A **macOS-native Unified Logging input is feasible**, but implementation risk varies sharply by integration path:

1. **Best low-risk path (recommended):** shell out to `log stream --style ndjson` for live ingestion and `log show --style ndjson` for bounded backfills.
2. **Higher-control path:** call **OSLogStore** via Objective-C interop (`objc2` + `objc2-os-log`) for typed entries and archive support.
3. **Not recommended as primary source for this feature:** Endpoint Security (ES) framework. ES is useful for security telemetry but is not a general replacement for Unified Logging and has entitlement friction.

A practical recommendation is a **phased rollout**:

- **Phase 1:** CLI adapter (`log stream`) behind a macOS feature flag.
- **Phase 2:** optional OSLogStore backend for richer filtering/replay if needed.

**Go/No-Go:** **GO**, with caveats on permissions and product expectations about “how much system log data is visible without elevated privileges.”

---

## 1) API accessibility assessment (what works from Rust)

## A. Unified Logging via CLI (`log stream`, `log show`)

### What works

The macOS `log(1)` tool supports:

- `log stream` for live view.
- `log show` for querying historical/persisted data.
- `--predicate` filtering (NSPredicate syntax).
- output styles including `json`/`ndjson` for machine ingestion.
- level controls (`default|info|debug`) for stream and info/debug toggles for show.

This is directly suitable for a Rust subprocess-based input.

### Rust integration shape

- Spawn child process with `std::process::Command`.
- Read stdout line-by-line (`BufRead`) in NDJSON mode.
- Parse with `serde_json`.
- Add restart/backoff if child exits unexpectedly.

### Pros

- Minimal FFI complexity.
- Fastest path to production.
- Uses Apple-supported operator tooling.

### Cons

- Depends on CLI output schema stability across macOS versions.
- Requires careful handling of startup flags and buffering.
- Privilege-dependent visibility still applies.

---

## B. OSLogStore API (programmatic)

### What works

OSLogStore provides programmatic access to unified logs and supports:

- Local store access (`localStore...`) when permitted.
- Scoped stores.
- Position-based iteration (`positionWithDate`, `positionWithTimeInterval...`).
- predicate-filtered enumeration.
- logarchive-backed stores.

In Rust, this is reachable via Objective-C interop. The `objc2-os-log` crate exposes key OSLog types including `OSLogStore`, `OSLogEntryLog`, `OSLogEntryFromProcess`, and `OSLogEntryWithPayload`.

### Rust integration shape

- `objc2` + `objc2-foundation` + `objc2-os-log`.
- `unsafe` calls into Objective-C methods.
- Convert Objective-C strings/dates/collections into Rust-owned values.
- Maintain per-source cursor/position semantics in app state.

### Pros

- Strongly typed fields and explicit iteration APIs.
- Better long-term control than parsing CLI text.
- Built-in archive access is attractive for backfill/offline analysis.

### Cons

- Higher implementation complexity (`unsafe`, objc runtime model, memory management semantics).
- Access to local store is permission-gated; admin requirement is documented.
- API behavior can differ subtly across macOS releases.

---

## C. Endpoint Security framework relevance

Endpoint Security is a **different telemetry plane** (security event stream), not the unified logging event store. It is relevant if logfwd later wants high-fidelity process/file/auth telemetry, but it should not be treated as a direct substitute for OSLog ingest.

It also requires restricted entitlement (`com.apple.developer.endpoint-security.client`) and deployment model complexity (system extension / provisioning / user or MDM approvals depending on deployment). That makes it a poor first step for general log ingestion.

---

## 2) Data model mapping to logfwd `InputEvent`

Because this workspace does not include the `logfwd` `InputEvent` definition, this section maps OSLog fields to a **canonical event envelope** and OTLP log semantics. Treat this as a design contract draft for adaptation.

## Candidate source fields available

From OSLog API surface and `log` tooling:

- timestamp/date
- process name
- processIdentifier (pid)
- sender (binary image)
- threadIdentifier
- subsystem
- category
- formatString
- components (structured payload pieces)
- composed message text
- level: `Undefined | Debug | Info | Notice | Error | Fault`

## Proposed mapping (canonical)

- `InputEvent.timestamp` ← OSLog entry date
- `InputEvent.body` / `message` ← composed message (fallback: format string)
- `InputEvent.severity_text` ← OSLog level name
- `InputEvent.severity_number` ← mapped OTLP severity number (below)
- `InputEvent.attributes["process.name"]` ← process
- `InputEvent.attributes["process.pid"]` ← processIdentifier
- `InputEvent.attributes["thread.id"]` ← threadIdentifier
- `InputEvent.attributes["log.logger"]` or `"sender.image"` ← sender
- `InputEvent.attributes["oslog.subsystem"]` ← subsystem
- `InputEvent.attributes["oslog.category"]` ← category
- `InputEvent.attributes["oslog.format_string"]` ← formatString
- `InputEvent.attributes["oslog.components"]` ← serialized components (optional)
- `InputEvent.attributes["source.platform"]` ← `macos.unified_logging`

## OSLog level → OTLP severity mapping (recommended)

- `Fault` → `FATAL` range (e.g., 21)
- `Error` → `ERROR` range (e.g., 17)
- `Notice` → `INFO` high / `WARN` low (recommend `INFO2`/`INFO3`, e.g., 10)
- `Info` → `INFO` (e.g., 9)
- `Debug` → `DEBUG` (e.g., 5)
- `Undefined` → `UNSPECIFIED` (0) with `severity_text="UNDEFINED"`

Rationale: Apple `Notice` is “significant normal condition,” semantically closer to informational than hard warning for most pipelines.

## Structured vs unstructured messages

- OSLog supports format-string + argument components.
- Some components may be redacted/unavailable depending on privacy annotations and permissions.
- Preserve raw message text for searchability; attach structured payload when available.

---

## 3) Permissions / entitlements / security boundaries

## Unified Logging (`log` CLI / OSLogStore)

- Access to unified logs is permission-gated.
- OSLogStore local-store access explicitly documents admin requirement and can fail when process lacks access.
- In practice, visibility differs by account type, execution context (interactive/user/daemon), and whether system areas are restricted.

## SIP / platform constraints

- System Integrity Protection (SIP) and platform security controls limit some deep system visibility/behavior.
- Expect partial data for non-privileged contexts.
- Design should degrade gracefully (emit diagnostics on dropped/permission-denied paths).

## Endpoint Security

- Requires restricted entitlement: `com.apple.developer.endpoint-security.client`.
- Additional signing/provisioning/deployment requirements apply.
- Not viable for “just ship a log source quickly” unless product is already in privileged security-agent posture.

---

## 4) Recommended approach (API vs CLI vs framework)

## Recommendation: **CLI-first, API-second, ES out-of-scope**

### Phase 1 (recommended now): `log stream` input

Implement a macOS input that executes:

- `log stream --style ndjson --color none ...`
- optional `--level` and `--predicate`

Why:

- Lowest engineering lift.
- Operationally transparent; users can reproduce command-line behavior.
- Sufficient for near-real-time ingestion.

### Phase 1.5 (optional): bounded historical catch-up

Use `log show --style ndjson --last ...` for startup catch-up windows.

### Phase 2 (if needed): OSLogStore backend

Build native API backend when one or more are true:

- Need tighter control over cursoring/replay.
- Need archive-based ingestion as first-class feature.
- CLI schema/behavior drift creates maintenance burden.

### Explicit non-recommendation for this issue

Do **not** scope Endpoint Security into this feature; treat it as separate roadmap item (“macOS security telemetry input”).

---

## 5) Effort estimate

Assuming an existing modular input architecture similar to current Linux/journald/eBPF sources.

## Option A: CLI-first implementation

- **Design + spike:** 1–2 days
- **Implementation:** 2–4 days
- **Tests (unit/integration on macOS runner):** 2–4 days
- **Docs + hardening:** 1–2 days

**Total:** ~1.5 to 2.5 weeks calendar (single engineer, with review).

## Option B: OSLogStore-first implementation

- **FFI/API spike:** 3–5 days
- **Implementation:** 5–8 days
- **Stability/compat testing across macOS versions:** 4–7 days
- **Docs + hardening:** 2–3 days

**Total:** ~3 to 5 weeks calendar.

## Option C: Endpoint Security-based input (separate project)

- **Engineering:** 4–8+ weeks
- **Plus:** entitlement/program onboarding and deployment work (variable, can dominate schedule).

---

## 6) Go / No-Go recommendation

## Recommendation: **GO** (with CLI-first scope)

### Why GO

- There is a clear ingestion path today (`log stream` NDJSON).
- Rust can integrate quickly via subprocess without unsafe FFI.
- A typed OSLogStore path exists later if/when needed.

### Key risks and mitigations

1. **Permission variability** (what logs are visible):
   - Mitigate with explicit startup diagnostics and docs for privilege expectations.
2. **CLI schema drift across macOS versions:**
   - Mitigate by tolerant parsing and versioned fixture tests from real command outputs.
3. **Volume spikes / CPU overhead:**
   - Mitigate with predicates, bounded queues, backpressure/drop metrics, and rate counters.

### No-Go criteria (if encountered)

Re-evaluate if product requires complete system-wide privileged visibility without acceptable deployment constraints, or if subprocess policy forbids CLI-based collectors.

---

## Performance considerations and operational notes

- `log stream` is push-like and generally better than polling for real-time use.
- `log show` can be expensive on wide windows; keep startup backfills bounded.
- Predicate selectivity is critical for CPU/memory cost control.
- Prefer NDJSON for streaming parser simplicity and low incremental parse overhead.

---

## Suggested implementation checklist (non-code)

1. Define user-facing config:
   - `level` (`default|info|debug`)
   - `predicate` (string)
   - `include_fields` / redaction behavior
   - restart/backoff policy
2. Define canonical mapping contract + OTLP severity conversion.
3. Prepare macOS CI fixture corpus from `log stream --style ndjson` samples.
4. Add troubleshooting doc section:
   - required privileges
   - typical permission failure messages
   - predicate examples

---

## Sources

- Apple `log(1)` man page mirror (Xcode man pages): https://keith.github.io/xcode-man-pages/log.1.html
- Apple OS logging overview: https://developer.apple.com/documentation/os/logging
- Apple OSLogStore: https://developer.apple.com/documentation/oslog/oslogstore
- Apple Endpoint Security overview: https://developer.apple.com/documentation/EndpointSecurity
- Apple endpoint-security entitlement page: https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.developer.endpoint-security.client
- Apple restricted entitlement daemon signing note: https://developer.apple.com/documentation/xcode/signing-a-daemon-with-a-restricted-entitlement
- Rust `objc2` crate docs: https://docs.rs/objc2/latest/objc2/
- Rust `objc2-os-log` crate docs: https://docs.rs/objc2-os-log/latest/objc2_os_log/
- `objc2-os-log` OSLogStore details: https://docs.rs/objc2-os-log/latest/objc2_os_log/struct.OSLogStore.html
- `objc2-os-log` entry metadata traits:
  - https://docs.rs/objc2-os-log/latest/objc2_os_log/trait.OSLogEntryFromProcess.html
  - https://docs.rs/objc2-os-log/latest/objc2_os_log/trait.OSLogEntryWithPayload.html
- Rust `endpoint-sec` crate docs: https://docs.rs/endpoint-sec/latest/endpoint_sec/

