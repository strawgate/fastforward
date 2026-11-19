------------------------- MODULE TailLifecycle -------------------------
EXTENDS Naturals

\* Minimal control-plane model for the tail EOF + error-backoff reducers and
\* the truncation-generation reducers that give truncated files a new
\* checkpoint identity.
\* This mirrors the pure reducers in `crates/ffwd-io/src/tail/state.rs`.

CONSTANTS
    Threshold,       \* EOF idle-poll threshold (expected: 2)
    MaxIdle,         \* finite bound for model checking
    MaxOffset,       \* finite offset/file-size bound for shutdown EOF checks
    MaxErrors,       \* finite bound for model checking
    InitialBackoff,  \* expected: 100
    MaxBackoff,      \* expected: 5000
    MaxGeneration    \* truncation generation cap (production: 65535)

VARIABLES
    eofEmitted,
    idlePolls,
    fileOffset,
    fileSize,
    consecutiveErrors,
    backoffMs,
    generation,      \* truncation generation: part of the file's SourceId
    lastAction

vars == <<eofEmitted, idlePolls, fileOffset, fileSize, consecutiveErrors, backoffMs, generation, lastAction>>

Min(a, b) ==
    IF a < b THEN a ELSE b

IncIdle(i) ==
    IF i < MaxIdle THEN i + 1 ELSE MaxIdle

IncErrors(e) ==
    IF e < MaxErrors THEN e + 1 ELSE MaxErrors

RECURSIVE Pow2(_)
Pow2(n) ==
    IF n = 0 THEN 1 ELSE 2 * Pow2(n - 1)

\* `next_truncation_generation`: advance by one, saturating at the cap.
NextGeneration(g) ==
    IF g < MaxGeneration THEN g + 1 ELSE MaxGeneration

BackoffDelay(errors) ==
    IF errors = 0 THEN
        0
    ELSE
        Min(MaxBackoff, InitialBackoff * Pow2(Min(errors - 1, 6)))

Init ==
    /\ eofEmitted = FALSE
    /\ idlePolls = 0
    /\ fileOffset = 0
    /\ fileSize = 0
    /\ consecutiveErrors = 0
    /\ backoffMs = 0
    /\ generation = 0
    /\ lastAction = "Init"

DataStep ==
    /\ eofEmitted' = FALSE
    /\ idlePolls' = 0
    /\ UNCHANGED <<fileOffset, fileSize, consecutiveErrors, backoffMs, generation>>
    /\ lastAction' = "Data"

NoDataEmit ==
    LET nextIdle == IncIdle(idlePolls) IN
    /\ ~eofEmitted
    /\ nextIdle >= Threshold
    /\ eofEmitted' = TRUE
    /\ idlePolls' = nextIdle
    /\ UNCHANGED <<fileOffset, fileSize, consecutiveErrors, backoffMs, generation>>
    /\ lastAction' = "NoDataEmit"

NoDataNoEmit ==
    LET nextIdle == IncIdle(idlePolls) IN
    /\ eofEmitted \/ nextIdle < Threshold
    /\ eofEmitted' = eofEmitted
    /\ idlePolls' = nextIdle
    /\ UNCHANGED <<fileOffset, fileSize, consecutiveErrors, backoffMs, generation>>
    /\ lastAction' = "NoDataNoEmit"

ShutdownEmit ==
    \E nextOffset \in 0..MaxOffset, nextSize \in 0..MaxOffset:
        /\ ~eofEmitted
        /\ nextOffset >= nextSize
        /\ fileOffset' = nextOffset
        /\ fileSize' = nextSize
        /\ eofEmitted' = TRUE
        /\ UNCHANGED <<idlePolls, consecutiveErrors, backoffMs, generation>>
        /\ lastAction' = "ShutdownEmit"

ShutdownNoEmit ==
    \E nextOffset \in 0..MaxOffset, nextSize \in 0..MaxOffset:
        /\ nextOffset < nextSize
        /\ ~eofEmitted
        /\ fileOffset' = nextOffset
        /\ fileSize' = nextSize
        /\ eofEmitted' = eofEmitted
        /\ UNCHANGED <<idlePolls, consecutiveErrors, backoffMs, generation>>
        /\ lastAction' = "ShutdownNoEmit"

ErrorStep ==
    LET nextErrors == IncErrors(consecutiveErrors) IN
    /\ consecutiveErrors' = nextErrors
    /\ backoffMs' = BackoffDelay(nextErrors)
    /\ UNCHANGED <<eofEmitted, idlePolls, fileOffset, fileSize, generation>>
    /\ lastAction' = "Error"

CleanStep ==
    /\ consecutiveErrors > 0
    /\ consecutiveErrors' = 0
    /\ backoffMs' = 0
    /\ UNCHANGED <<eofEmitted, idlePolls, fileOffset, fileSize, generation>>
    /\ lastAction' = "Clean"

\* Truncation (or a same-inode rewrite) detected: rewind to 0, reset EOF
\* state as for data, and move to the next generation so the new contents
\* get a new SourceId.
TruncateStep ==
    /\ fileOffset' = 0
    /\ eofEmitted' = FALSE
    /\ idlePolls' = 0
    /\ generation' = NextGeneration(generation)
    /\ UNCHANGED <<fileSize, consecutiveErrors, backoffMs>>
    /\ lastAction' = "Truncate"

\* Restoring a checkpoint stored for generation g of this file:
\* `should_adopt_restored_generation` adopts only strictly newer generations.
RestoreStep ==
    \E g \in 0..MaxGeneration, nextOffset \in 0..MaxOffset:
        IF g > generation THEN
            /\ generation' = g
            /\ fileOffset' = nextOffset
            /\ UNCHANGED <<eofEmitted, idlePolls, fileSize, consecutiveErrors, backoffMs>>
            /\ lastAction' = "Restore"
        ELSE
            /\ UNCHANGED <<eofEmitted, idlePolls, fileOffset, fileSize, consecutiveErrors, backoffMs, generation>>
            /\ lastAction' = "RestoreIgnored"

Next ==
    \/ DataStep
    \/ NoDataEmit
    \/ NoDataNoEmit
    \/ ShutdownEmit
    \/ ShutdownNoEmit
    \/ ErrorStep
    \/ CleanStep
    \/ TruncateStep
    \/ RestoreStep

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ eofEmitted \in BOOLEAN
    /\ idlePolls \in 0..MaxIdle
    /\ fileOffset \in 0..MaxOffset
    /\ fileSize \in 0..MaxOffset
    /\ consecutiveErrors \in 0..MaxErrors
    /\ backoffMs \in 0..MaxBackoff
    /\ generation \in 0..MaxGeneration
    /\ lastAction \in {"Init", "Data", "NoDataEmit", "NoDataNoEmit", "ShutdownEmit", "ShutdownNoEmit", "Error", "Clean", "Truncate", "Restore", "RestoreIgnored"}

EofEmissionRequiresThreshold ==
    lastAction = "NoDataEmit" =>
    /\ eofEmitted
    /\ idlePolls >= Threshold

DataResetsEofState ==
    lastAction = "Data" =>
    /\ ~eofEmitted
    /\ idlePolls = 0

ShutdownEofRequiresCaughtUp ==
    lastAction = "ShutdownEmit" =>
    /\ eofEmitted
    /\ fileOffset >= fileSize

ShutdownBehindSuppressesEof ==
    lastAction = "ShutdownNoEmit" =>
    /\ fileOffset < fileSize
    /\ ~eofEmitted

BackoffZeroIffNoErrors ==
    (consecutiveErrors = 0) <=> (backoffMs = 0)

BackoffDelayConsistent ==
    consecutiveErrors > 0 => backoffMs = BackoffDelay(consecutiveErrors)

TruncateRewindsAndResetsEof ==
    lastAction = "Truncate" =>
    /\ fileOffset = 0
    /\ ~eofEmitted
    /\ idlePolls = 0

\* Offsets for one SourceId never go back: the generation (part of the
\* SourceId) never decreases, and every rewind to 0 moves to a new
\* generation unless the cap is reached.
GenerationNeverDecreases ==
    [][generation' >= generation]_vars

TruncateAdvancesGeneration ==
    [][lastAction' = "Truncate" => generation' = NextGeneration(generation)]_vars

=============================================================================
