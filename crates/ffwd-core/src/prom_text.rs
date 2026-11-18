// prom_text.rs — Prometheus text exposition format scanner.
//
// Parses the Prometheus text format (version 0.0.4, plus the OpenMetrics
// text subset that matters for scraping) straight into a `ScanBuilder`, one
// row per sample. Strings (metric names, label values, TYPE tokens) are
// passed as subslices of the input buffer so Arrow builders can create
// zero-copy string views.
//
// Stage 1 is a SIMD classifier (`wide::u8x16`, portable across SSE2, AVX2,
// NEON and WASM) that turns each 64-byte block into a single u64 mask of
// "interesting" bytes:
//
//   '\n'  ' '  '\t'  '"'  '\\'  '{'  '}'  '='  ','
//
// Stage 2 is a hand-written line walker that jumps between interesting
// positions with `trailing_zeros` instead of looking at every byte. Metric
// names, label names, numbers and most label-value bytes are skipped in O(1)
// per 64-byte block. Quote/escape state is tracked by the walker, not by the
// classifier, because Prometheus comment lines (HELP text) may contain
// unbalanced quotes and every newline terminates a line unconditionally.
//
// Error semantics match Prometheus: any malformed line fails the whole
// exposition. Rows that were already appended stay in the builder, so callers
// must discard the batch (e.g. call `begin_batch` again) on `Err`.

use alloc::vec::Vec;

use ffwd_lint_attrs::verified;
use wide::u8x16;

use crate::scan_config::parse_int_fast;
use crate::scanner::ScanBuilder;

/// Column holding the metric name of each sample.
pub const PROM_NAME_FIELD: &[u8] = b"__name__";
/// Column holding the sample value as `Float64`.
pub const PROM_VALUE_FIELD: &[u8] = b"__value__";
/// Column holding the optional exposition timestamp (Unix milliseconds).
pub const PROM_TIMESTAMP_FIELD: &[u8] = b"__timestamp_ms__";
/// Column holding the metric family type from `# TYPE` (e.g. `counter`).
/// In OpenMetrics, the `<family>_created` sample of a counter, histogram or
/// summary gets [`PROM_TYPE_CREATED`] instead: it is the series' start time,
/// not a measurement.
pub const PROM_TYPE_FIELD: &[u8] = b"__type__";
/// [`PROM_TYPE_FIELD`] value of an OpenMetrics start-time (`_created`) sample.
pub const PROM_TYPE_CREATED: &[u8] = b"created";
/// Column the Arrow layer appends with each sample's series id. Reserved here
/// so an exposition label of that name is prefixed instead of overwritten.
pub const PROM_SERIES_ID_FIELD: &[u8] = b"__series_id__";

/// Prefix for labels whose name collides with a synthetic column
/// (`__value__`, `__timestamp_ms__`, `__type__`, `__series_id__`). Such labels are invalid in
/// Prometheus (the `__` prefix is reserved) but are kept rather than dropped.
/// Quoted label names that already start with this prefix get it once more,
/// so `"label:__value__"` and a renamed `__value__` never share a column.
pub const PROM_RESERVED_LABEL_PREFIX: &[u8] = b"label:";

/// Metric type declared by a `# TYPE` line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FamilyKind {
    Counter,
    Gauge,
    Histogram,
    GaugeHistogram,
    Summary,
    Info,
    StateSet,
    Untyped,
}

impl FamilyKind {
    /// Parse a type token (case-insensitive, like Prometheus). Covers the
    /// text 0.0.4 types and the OpenMetrics ones.
    fn parse(token: &[u8]) -> Option<Self> {
        const KINDS: [(&[u8], FamilyKind); 9] = [
            (b"counter", FamilyKind::Counter),
            (b"gauge", FamilyKind::Gauge),
            (b"histogram", FamilyKind::Histogram),
            (b"gaugehistogram", FamilyKind::GaugeHistogram),
            (b"summary", FamilyKind::Summary),
            (b"info", FamilyKind::Info),
            (b"stateset", FamilyKind::StateSet),
            (b"untyped", FamilyKind::Untyped),
            (b"unknown", FamilyKind::Untyped),
        ];
        KINDS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(token))
            .map(|&(_, kind)| kind)
    }

    /// Sample-name suffixes that belong to a family of this type.
    fn suffixes(self) -> &'static [&'static [u8]] {
        match self {
            Self::Counter => &[b"_total", b"_created"],
            Self::Histogram => &[b"_bucket", b"_sum", b"_count", b"_created"],
            Self::Summary => &[b"_sum", b"_count", b"_created"],
            Self::GaugeHistogram => &[b"_bucket", b"_gcount", b"_gsum"],
            Self::Info => &[b"_info"],
            Self::Gauge | Self::StateSet | Self::Untyped => &[],
        }
    }
}

/// True when `sample` is the family `family` itself or one of the component
/// series its type allows (e.g. `_bucket` only for histograms). `_created`
/// series exist only in OpenMetrics; in Prometheus text a `foo_created` next
/// to `# TYPE foo histogram` is an unrelated series.
#[inline]
fn belongs_to_family(sample: &[u8], family: &[u8], kind: FamilyKind, openmetrics: bool) -> bool {
    match sample.strip_prefix(family) {
        Some([]) => true,
        Some(b"_created") if !openmetrics => false,
        Some(rest) => kind.suffixes().contains(&rest),
        None => false,
    }
}

/// Legacy metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
#[inline(always)]
fn is_valid_metric_name(name: &[u8]) -> bool {
    match name.split_first() {
        Some((first, rest)) => {
            (first.is_ascii_alphabetic() || matches!(first, b'_' | b':'))
                && rest
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':'))
        }
        None => false,
    }
}

/// Legacy label name: `[a-zA-Z_][a-zA-Z0-9_]*`.
#[inline(always)]
fn is_valid_label_name(name: &[u8]) -> bool {
    match name.split_first() {
        Some((first, rest)) => {
            (first.is_ascii_alphabetic() || *first == b'_')
                && rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        }
        None => false,
    }
}

#[inline(always)]
fn is_reserved_column(key: &[u8]) -> bool {
    key == PROM_VALUE_FIELD
        || key == PROM_TIMESTAMP_FIELD
        || key == PROM_TYPE_FIELD
        || key == PROM_SERIES_ID_FIELD
}

/// Builder extension used by the Prometheus scanner for pre-parsed numbers.
///
/// The JSON scanner hands numbers to builders as raw ASCII. Prometheus values
/// need format-specific parsing (`+Inf`, `NaN`, exponents), so the scanner
/// parses them itself and passes typed values.
pub trait PromBuilder: ScanBuilder {
    /// Append an already-parsed `f64` at the given column index.
    fn append_f64_by_idx(&mut self, idx: usize, value: f64);
    /// Append an already-parsed `i64` at the given column index.
    fn append_i64_by_idx(&mut self, idx: usize, value: i64);
}

/// Options for [`scan_prom_text`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromScanConfig {
    /// Emit the `# TYPE` of the enclosing family into [`PROM_TYPE_FIELD`].
    pub emit_type: bool,
    /// Parse timestamps as OpenMetrics float seconds instead of Prometheus
    /// text integer milliseconds. Set this when the scrape response is
    /// `application/openmetrics-text`. Either way [`PROM_TIMESTAMP_FIELD`]
    /// holds milliseconds. Only OpenMetrics treats `# EOF` as the end of the
    /// exposition; in Prometheus text it is an ordinary comment.
    pub openmetrics: bool,
}

impl Default for PromScanConfig {
    fn default() -> Self {
        Self {
            emit_type: true,
            openmetrics: false,
        }
    }
}

/// What went wrong while scanning an exposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromErrorKind {
    /// A sample line had no metric name.
    MissingName,
    /// A sample line had no value.
    MissingValue,
    /// The sample value is not a valid float.
    InvalidValue,
    /// The timestamp is not a valid integer.
    InvalidTimestamp,
    /// A label set was malformed (missing `=`, `"`, `,` or `}`).
    InvalidLabels,
    /// A quoted string ran into a newline or the end of input.
    UnterminatedString,
    /// Unexpected bytes after the value / timestamp.
    TrailingGarbage,
    /// An unquoted metric or label name has a byte outside the legacy
    /// Prometheus name charset. Quote the name to use other characters.
    InvalidName,
    /// A `# TYPE` line is missing its name or has an unknown type.
    InvalidType,
    /// The exposition is not valid UTF-8.
    InvalidUtf8,
    /// An OpenMetrics exemplar (`# {labels} value [timestamp]`) is malformed.
    InvalidExemplar,
    /// An OpenMetrics exposition ended without its `# EOF` line, so it may
    /// be truncated.
    MissingEof,
    /// An OpenMetrics exposition has content after its `# EOF` line.
    DataAfterEof,
}

/// A scan failure with its 1-based line number and byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromParseError {
    /// 1-based line number of the failing line.
    pub line: usize,
    /// Byte offset where the error was detected.
    pub offset: usize,
    /// Failure category.
    pub kind: PromErrorKind,
}

/// Counters returned by a successful scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromScanStats {
    /// Number of samples (rows) emitted.
    pub samples: usize,
    /// Number of lines visited, including comments and blank lines.
    pub lines: usize,
}

/// Reusable scratch state for [`scan_prom_text`].
///
/// Keep one per input and reuse it across scrapes so the hot path performs no
/// allocations after warm-up.
#[derive(Debug, Default)]
pub struct PromScratch {
    /// Unescaped bytes for label values / names that contained `\` escapes.
    decoded: Vec<u8>,
    /// Per label position: (key start, key len, column idx) from the previous
    /// sample. Label sets repeat in the same order within a family, so a byte
    /// compare against the previous key avoids a hash lookup per label.
    key_cache: Vec<(usize, usize, usize)>,
    /// Decoded name of the current `# TYPE` family.
    family_name: Vec<u8>,
}

impl PromScratch {
    /// Create empty scratch state.
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------------------------------------------------------------------------
// Stage 1: classification
// ---------------------------------------------------------------------------

/// Return true when `b` is one of the bytes the walker must stop at.
#[inline(always)]
#[verified(kani = "verify_prom_structural_mask_simd_eq_scalar")]
pub const fn is_prom_structural(b: u8) -> bool {
    matches!(
        b,
        b'\n' | b' ' | b'\t' | b'"' | b'\\' | b'{' | b'}' | b'=' | b','
    )
}

/// Scalar reference for [`prom_structural_mask`]. Bit `i` is set when
/// `block[i]` satisfies [`is_prom_structural`].
#[verified(kani = "verify_prom_structural_mask_simd_eq_scalar")]
pub fn prom_structural_mask_scalar(block: &[u8; 64]) -> u64 {
    let mut mask = 0u64;
    for (i, &b) in block.iter().enumerate() {
        if is_prom_structural(b) {
            mask |= 1u64 << i;
        }
    }
    mask
}

#[inline(always)]
fn chunk_mask(c: u8x16) -> u64 {
    let m = c.simd_eq(u8x16::splat(b'\n'))
        | c.simd_eq(u8x16::splat(b' '))
        | c.simd_eq(u8x16::splat(b'\t'))
        | c.simd_eq(u8x16::splat(b'"'))
        | c.simd_eq(u8x16::splat(b'\\'))
        | c.simd_eq(u8x16::splat(b'{'))
        | c.simd_eq(u8x16::splat(b'}'))
        | c.simd_eq(u8x16::splat(b'='))
        | c.simd_eq(u8x16::splat(b','));
    u64::from(m.to_bitmask() as u16)
}

/// SIMD classification of one 64-byte block (see [`prom_structural_mask_scalar`]).
#[inline]
#[verified(kani = "verify_prom_structural_mask_simd_eq_scalar")]
pub fn prom_structural_mask(block: &[u8; 64]) -> u64 {
    let c0 = u8x16::new(block[0..16].try_into().expect("block is 64 bytes"));
    let c1 = u8x16::new(block[16..32].try_into().expect("block is 64 bytes"));
    let c2 = u8x16::new(block[32..48].try_into().expect("block is 64 bytes"));
    let c3 = u8x16::new(block[48..64].try_into().expect("block is 64 bytes"));
    chunk_mask(c0) | (chunk_mask(c1) << 16) | (chunk_mask(c2) << 32) | (chunk_mask(c3) << 48)
}

/// Lazily classifies 64-byte blocks and answers "next structural byte at or
/// after `pos`". Only the most recent block mask is cached; positions must be
/// queried in non-decreasing block order for best performance, but any order
/// is correct.
struct Cursor<'a> {
    buf: &'a [u8],
    /// Index of the 64-byte block `mask` describes; `None` before the first load.
    block: Option<usize>,
    mask: u64,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            block: None,
            mask: 0,
        }
    }

    #[inline(always)]
    fn load(&self, block: usize) -> u64 {
        let start = block << 6;
        match self.buf.get(start..start + 64) {
            Some(slice) => {
                let arr: &[u8; 64] = slice.try_into().expect("slice is 64 bytes");
                prom_structural_mask(arr)
            }
            None => {
                let mut tail = [0u8; 64];
                let rest = self.buf.get(start..).unwrap_or(&[]);
                tail[..rest.len()].copy_from_slice(rest);
                prom_structural_mask(&tail)
            }
        }
    }

    /// Position of the next structural byte at or after `pos`, or
    /// `buf.len()` when there is none.
    #[inline(always)]
    fn next_from(&mut self, mut pos: usize) -> usize {
        let len = self.buf.len();
        loop {
            if pos >= len {
                return len;
            }
            let block = pos >> 6;
            if self.block != Some(block) {
                self.block = Some(block);
                self.mask = self.load(block);
            }
            let m = self.mask & (u64::MAX << (pos & 63));
            if m != 0 {
                return (block << 6) + m.trailing_zeros() as usize;
            }
            pos = (block + 1) << 6;
        }
    }
}

// ---------------------------------------------------------------------------
// Number parsing
// ---------------------------------------------------------------------------

const POW10: [f64; 16] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
];

/// Parse a Prometheus sample value.
///
/// Integers and plain decimals with at most 15 significant digits take an
/// exact fast path (Clinger: both operands are exactly representable, so the
/// single division is correctly rounded). Everything else — exponents,
/// `NaN`, `+Inf`, `-Inf`, long mantissas — falls back to `core`'s
/// correctly-rounded parser.
#[inline]
// Exactness of the integer fast path: `verify_parse_prom_value_integers`.
#[verified(kani = "verify_parse_prom_value_no_panic")]
pub fn parse_prom_value(bytes: &[u8]) -> Option<f64> {
    let (neg, digits) = match bytes.first() {
        Some(b'-') => (true, bytes.get(1..)?),
        Some(b'+') => (false, bytes.get(1..)?),
        Some(_) => (false, bytes),
        None => return None,
    };
    if !digits.is_empty() && digits.len() <= 16 {
        let mut mantissa: u64 = 0;
        let mut frac_len: usize = 0;
        let mut seen_dot = false;
        let mut n_digits = 0usize;
        let mut ok = true;
        for &b in digits {
            if b.is_ascii_digit() {
                mantissa = mantissa * 10 + u64::from(b - b'0');
                n_digits += 1;
                if seen_dot {
                    frac_len += 1;
                }
            } else if b == b'.' && !seen_dot {
                seen_dot = true;
            } else {
                ok = false;
                break;
            }
        }
        if ok && n_digits > 0 && n_digits <= 15 {
            let v = mantissa as f64 / POW10[frac_len];
            return Some(if neg { -v } else { v });
        }
    }
    parse_prom_value_fallback(bytes, digits)
}

/// Slow path of [`parse_prom_value`]: `core`'s float parser, which handles
/// exponents, `NaN`, `Inf` and long mantissas. `digits` is `bytes` without
/// its sign. Kept separate so Kani proofs can stub `core`'s parser, whose
/// state space is too large for CBMC; `core` itself is trusted.
fn parse_prom_value_fallback(bytes: &[u8], digits: &[u8]) -> Option<f64> {
    let s = core::str::from_utf8(bytes).ok()?;
    let v = s.parse::<f64>().ok()?;
    // `core` rounds out-of-range literals such as `1e999` to infinity;
    // Prometheus rejects them. Only an explicit `Inf` may be infinite.
    if v.is_infinite()
        && !(digits.eq_ignore_ascii_case(b"inf") || digits.eq_ignore_ascii_case(b"infinity"))
    {
        return None;
    }
    Some(v)
}

/// Parse an OpenMetrics timestamp (float seconds) into Unix milliseconds,
/// rounded to the nearest millisecond.
fn parse_seconds_as_ms(bytes: &[u8]) -> Option<i64> {
    let ms = parse_prom_value(bytes)? * 1000.0;
    // Also rejects NaN and infinities.
    if !(-9.2e18..9.2e18).contains(&ms) {
        return None;
    }
    Some(if ms >= 0.0 {
        (ms + 0.5) as i64
    } else {
        (ms - 0.5) as i64
    })
}

// ---------------------------------------------------------------------------
// Stage 2: line walker
// ---------------------------------------------------------------------------

#[inline(always)]
fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

#[inline(always)]
fn skip_blanks(buf: &[u8], mut pos: usize) -> usize {
    while let Some(&b) = buf.get(pos) {
        if !is_blank(b) {
            break;
        }
        pos += 1;
    }
    pos
}

/// Decode Prometheus string escapes (`\\`, `\"`, `\n`) into `out`.
/// Unknown escapes are kept verbatim.
fn unescape_into(src: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while let Some(&b) = src.get(i) {
        if b == b'\\'
            && let Some(&next) = src.get(i + 1)
        {
            match next {
                b'n' => out.push(b'\n'),
                b'\\' => out.push(b'\\'),
                b'"' => out.push(b'"'),
                other => {
                    out.push(b'\\');
                    out.push(other);
                }
            }
            i += 2;
            continue;
        }
        out.push(b);
        i += 1;
    }
}

/// A quoted string span: `start..end` excludes the quotes.
#[derive(Clone, Copy)]
struct Quoted {
    start: usize,
    end: usize,
    escaped: bool,
}

struct Walker<'a, 'b, B: PromBuilder> {
    buf: &'a [u8],
    cur: Cursor<'a>,
    builder: &'b mut B,
    scratch: &'b mut PromScratch,
    line: usize,
    name_idx: usize,
    value_idx: usize,
    ts_idx: usize,
    type_idx: Option<usize>,
    /// Parse timestamps as OpenMetrics float seconds.
    openmetrics: bool,
    /// Current `# TYPE` type token span and kind. The family name itself is
    /// decoded into `scratch.family_name`.
    family: Option<(usize, usize, FamilyKind)>,
    /// Span of the last bare metric name that passed validation. Series of
    /// one family sit on consecutive lines, so most names skip the check.
    last_name: Option<(usize, usize)>,
}

impl<B: PromBuilder> Walker<'_, '_, B> {
    #[inline(always)]
    fn err(&self, offset: usize, kind: PromErrorKind) -> PromParseError {
        PromParseError {
            line: self.line,
            offset,
            kind,
        }
    }

    #[inline(always)]
    fn byte(&self, pos: usize) -> Option<u8> {
        self.buf.get(pos).copied()
    }

    /// Read a quoted string whose opening quote is at `pos - 1`.
    /// Returns the span and the position after the closing quote.
    #[inline(always)]
    fn read_quoted(&mut self, start: usize) -> Result<(Quoted, usize), PromParseError> {
        let mut pos = start;
        let mut escaped = false;
        loop {
            let q = self.cur.next_from(pos);
            match self.byte(q) {
                Some(b'"') => {
                    return Ok((
                        Quoted {
                            start,
                            end: q,
                            escaped,
                        },
                        q + 1,
                    ));
                }
                Some(b'\\') => {
                    // An escape may not swallow the line break or run off the end.
                    match self.byte(q + 1) {
                        Some(b'\n') | None => {
                            return Err(self.err(q + 1, PromErrorKind::UnterminatedString));
                        }
                        Some(_) => {}
                    }
                    escaped = true;
                    pos = q + 2;
                }
                Some(b'\n') | None => {
                    return Err(self.err(q, PromErrorKind::UnterminatedString));
                }
                Some(_) => pos = q + 1,
            }
        }
    }

    #[inline(always)]
    fn append_str_span(&mut self, idx: usize, start: usize, end: usize, escaped: bool) {
        let raw = &self.buf[start..end];
        if escaped {
            unescape_into(raw, &mut self.scratch.decoded);
            self.builder
                .append_decoded_str_by_idx(idx, &self.scratch.decoded);
        } else {
            self.builder.append_str_by_idx(idx, raw);
        }
    }

    /// Resolve a label key, using the per-position cache when the key bytes
    /// match the previous sample's key at the same position.
    /// Keys are validated only on a cache miss: a hit means the same bytes
    /// were already validated.
    #[inline(always)]
    fn resolve_key(
        &mut self,
        slot: usize,
        start: usize,
        end: usize,
    ) -> Result<usize, PromParseError> {
        let key = &self.buf[start..end];
        if let Some(&(cs, cl, idx)) = self.scratch.key_cache.get(slot)
            && self.buf.get(cs..cs + cl) == Some(key)
        {
            return Ok(idx);
        }
        if !is_valid_label_name(key) {
            return Err(self.err(start, PromErrorKind::InvalidName));
        }
        let idx = self.builder.resolve_field(key);
        let entry = (start, end - start, idx);
        if let Some(e) = self.scratch.key_cache.get_mut(slot) {
            *e = entry;
        } else {
            self.scratch.key_cache.push(entry);
        }
        Ok(idx)
    }

    /// Resolve a label key that cannot be used as-is: escaped keys are
    /// decoded, and keys equal to a synthetic column name are prefixed with
    /// [`PROM_RESERVED_LABEL_PREFIX`] so they never overwrite or get dropped
    /// by `__value__` / `__timestamp_ms__` / `__type__`.
    fn resolve_owned_key(&mut self, start: usize, end: usize, escaped: bool) -> usize {
        let mut k = core::mem::take(&mut self.scratch.decoded);
        let raw = &self.buf[start..end];
        if escaped {
            unescape_into(raw, &mut k);
        } else {
            k.clear();
            k.extend_from_slice(raw);
        }
        if is_reserved_column(&k) || k.starts_with(PROM_RESERVED_LABEL_PREFIX) {
            k.splice(0..0, PROM_RESERVED_LABEL_PREFIX.iter().copied());
        }
        let idx = self.builder.resolve_field(&k);
        self.scratch.decoded = k;
        idx
    }

    /// Parse `{...}` starting just after `{`. Returns position after `}` and,
    /// for the UTF-8 quoted-name syntax (`{"my.metric", a="b"}`), the name.
    fn labels(
        &mut self,
        mut pos: usize,
        has_name: bool,
    ) -> Result<(usize, Option<Quoted>), PromParseError> {
        let mut quoted_name: Option<Quoted> = None;
        let mut slot = 0usize;
        loop {
            pos = skip_blanks(self.buf, pos);
            let (key, key_escaped, key_quoted, after) = match self.byte(pos) {
                Some(b'}') => return Ok((pos + 1, quoted_name)),
                Some(b'"') => {
                    let (q, after) = self.read_quoted(pos + 1)?;
                    ((q.start, q.end), q.escaped, true, after)
                }
                Some(_) => {
                    let q = self.cur.next_from(pos);
                    ((pos, q), false, false, q)
                }
                None => return Err(self.err(pos, PromErrorKind::InvalidLabels)),
            };
            pos = skip_blanks(self.buf, after);
            match self.byte(pos) {
                Some(b'=') => {
                    if key.0 == key.1 {
                        return Err(self.err(pos, PromErrorKind::InvalidLabels));
                    }
                    pos = skip_blanks(self.buf, pos + 1);
                    if self.byte(pos) != Some(b'"') {
                        return Err(self.err(pos, PromErrorKind::InvalidLabels));
                    }
                    let (val, after) = self.read_quoted(pos + 1)?;
                    let raw_key = &self.buf[key.0..key.1];
                    if !key_escaped && raw_key == PROM_NAME_FIELD {
                        // `{__name__="m"}` names the metric (Prometheus 3 form).
                        if has_name || quoted_name.is_some() {
                            return Err(self.err(key.0, PromErrorKind::InvalidLabels));
                        }
                        quoted_name = Some(val);
                        pos = skip_blanks(self.buf, after);
                        match self.byte(pos) {
                            Some(b',') => {
                                pos += 1;
                                continue;
                            }
                            Some(b'}') => return Ok((pos + 1, quoted_name)),
                            _ => return Err(self.err(pos, PromErrorKind::InvalidLabels)),
                        }
                    }
                    // Quoted keys skip the legacy charset check and the
                    // position cache (they are rare, Prometheus 3 UTF-8 names).
                    let idx = if key_quoted || is_reserved_column(raw_key) {
                        self.resolve_owned_key(key.0, key.1, key_escaped)
                    } else {
                        self.resolve_key(slot, key.0, key.1)?
                    };
                    slot += 1;
                    self.append_str_span(idx, val.start, val.end, val.escaped);
                    pos = skip_blanks(self.buf, after);
                    match self.byte(pos) {
                        Some(b',') => pos += 1,
                        Some(b'}') => return Ok((pos + 1, quoted_name)),
                        _ => return Err(self.err(pos, PromErrorKind::InvalidLabels)),
                    }
                }
                // A bare quoted string is the metric name, allowed only when
                // the line has no name yet (`up{"other"}` is an error).
                Some(c @ (b',' | b'}'))
                    if !has_name
                        && quoted_name.is_none()
                        && self.byte(key.0.wrapping_sub(1)) == Some(b'"') =>
                {
                    quoted_name = Some(Quoted {
                        start: key.0,
                        end: key.1,
                        escaped: key_escaped,
                    });
                    pos += 1;
                    if c == b'}' {
                        return Ok((pos, quoted_name));
                    }
                }
                _ => return Err(self.err(pos, PromErrorKind::InvalidLabels)),
            }
        }
    }

    /// Handle a `#` line starting at `pos`. Returns the position after the
    /// line, or `None` when `# EOF` ends an OpenMetrics exposition. Other
    /// comments, and `# EOF` in Prometheus text, are ignored.
    fn comment(&mut self, pos: usize) -> Result<Option<usize>, PromParseError> {
        let end = crate::byte_search::find_byte(self.buf, b'\n', pos).unwrap_or(self.buf.len());
        // Tokenize what follows the `#`. Like Prometheus' parser, blanks
        // between `#` and the keyword are optional (`#TYPE` is a TYPE line).
        let after_hash = (pos + 1).min(end);
        let is_spaced = self.byte(after_hash).is_some_and(is_blank);
        let line = &self.buf[after_hash..end];
        let mut toks = line
            .split(|&b| is_blank(b))
            .filter(|t| !t.is_empty())
            .map(|t| {
                let start = t.as_ptr() as usize - self.buf.as_ptr() as usize;
                (start, start + t.len())
            });
        let keyword = toks.next();
        match keyword.map(|(s, e)| &self.buf[s..e]) {
            Some(b"TYPE") => {
                let after_kw = keyword.map_or(end, |(_, e)| e);
                self.family = Some(self.parse_type_line(after_kw, end)?);
            }
            // Only a bare `# EOF` in OpenMetrics ends the exposition;
            // `# EOF pending`, `#EOF`, or any `# EOF` in Prometheus text, is
            // an ordinary comment.
            Some(b"EOF") if self.openmetrics && is_spaced && toks.next().is_none() => {
                return Ok(None);
            }
            _ => {}
        }
        Ok(Some(end + 1))
    }

    /// Parse the `<name> <type>` part of a `# TYPE` line in `pos..end`.
    /// The name may be quoted (UTF-8 metric names) and is decoded into
    /// `scratch.family_name`. A missing or (unquoted) invalid name, a missing
    /// or unknown type, or anything after the type is an error, as in
    /// Prometheus.
    fn parse_type_line(
        &mut self,
        pos: usize,
        end: usize,
    ) -> Result<(usize, usize, FamilyKind), PromParseError> {
        let token_end = |from: usize| {
            let mut i = from;
            while i < end && self.byte(i).is_some_and(|b| !is_blank(b)) {
                i += 1;
            }
            i
        };
        let p = skip_blanks(self.buf, pos);
        let invalid = |at: usize| PromParseError {
            line: self.line,
            offset: at,
            kind: PromErrorKind::InvalidType,
        };
        let (name_start, name_end, escaped, after) = if self.byte(p) == Some(b'"') {
            let mut q = p + 1;
            let mut escaped = false;
            loop {
                if q >= end {
                    return Err(invalid(p));
                }
                match self.byte(q) {
                    Some(b'\\') => {
                        escaped = true;
                        q += 2;
                    }
                    Some(b'"') => break,
                    _ => q += 1,
                }
            }
            (p + 1, q, escaped, q + 1)
        } else {
            let e = token_end(p);
            (p, e, false, e)
        };
        if name_end <= name_start || name_end > end {
            return Err(invalid(p));
        }
        let ts = skip_blanks(self.buf, after);
        let te = token_end(ts);
        let kind = if ts < te && te <= end {
            FamilyKind::parse(&self.buf[ts..te])
        } else {
            None
        };
        let Some(kind) = kind else {
            return Err(invalid(ts));
        };
        let rest = skip_blanks(self.buf, te);
        if rest < end {
            return Err(invalid(rest));
        }
        let raw = &self.buf[name_start..name_end];
        // Unquoted names use the legacy charset, as on sample lines.
        if p == name_start && !is_valid_metric_name(raw) {
            return Err(invalid(p));
        }
        let family = &mut self.scratch.family_name;
        if escaped {
            unescape_into(raw, family);
        } else {
            family.clear();
            family.extend_from_slice(raw);
        }
        Ok((ts, te, kind))
    }

    /// Parse one sample line starting at `pos` (first non-blank byte).
    /// The row is closed on error too, so the caller can discard the batch.
    #[inline(always)]
    fn sample(&mut self, pos: usize) -> Result<usize, PromParseError> {
        self.builder.begin_row();
        let r = self.sample_row(pos);
        if let Ok(next) = r {
            // Raw sample line, for builders that capture lines.
            let line_end = (next - 1).min(self.buf.len());
            self.builder.append_line(&self.buf[pos..line_end]);
        }
        self.builder.end_row();
        r
    }

    #[inline(always)]
    fn sample_row(&mut self, pos: usize) -> Result<usize, PromParseError> {
        // Metric name (bare) unless the line starts with `{`.
        let (mut name, mut p) = if self.byte(pos) == Some(b'{') {
            (None, pos)
        } else {
            let q = self.cur.next_from(pos);
            let bare = &self.buf[pos..q];
            if self
                .last_name
                .is_none_or(|(ls, le)| self.buf.get(ls..le) != Some(bare))
            {
                if !is_valid_metric_name(bare) {
                    return Err(self.err(pos, PromErrorKind::InvalidName));
                }
                self.last_name = Some((pos, q));
            }
            (
                Some(Quoted {
                    start: pos,
                    end: q,
                    escaped: false,
                }),
                q,
            )
        };
        p = skip_blanks(self.buf, p);
        if self.byte(p) == Some(b'{') {
            let (after, quoted) = self.labels(p + 1, name.is_some())?;
            if name.is_none() {
                name = quoted;
            }
            p = skip_blanks(self.buf, after);
        }
        let Some(name) = name else {
            return Err(self.err(pos, PromErrorKind::MissingName));
        };
        if name.start == name.end {
            return Err(self.err(pos, PromErrorKind::MissingName));
        }
        self.append_str_span(self.name_idx, name.start, name.end, name.escaped);

        // Value.
        let vend = self.cur.next_from(p);
        if vend == p {
            return Err(self.err(p, PromErrorKind::MissingValue));
        }
        let value = parse_prom_value(&self.buf[p..vend])
            .ok_or_else(|| self.err(p, PromErrorKind::InvalidValue))?;
        self.builder.append_f64_by_idx(self.value_idx, value);
        p = skip_blanks(self.buf, vend);

        // Optional timestamp, then optional OpenMetrics exemplar.
        match self.byte(p) {
            None | Some(b'\n') => {}
            Some(b'#') => p = self.skip_exemplar(p)?,
            Some(_) => {
                let tend = self.cur.next_from(p);
                let raw_ts = &self.buf[p..tend];
                let raw_ts = match raw_ts {
                    [b'+', rest @ ..] if rest.first().is_some_and(u8::is_ascii_digit) => rest,
                    other => other,
                };
                let ts = if self.openmetrics {
                    parse_seconds_as_ms(raw_ts)
                } else {
                    parse_int_fast(raw_ts)
                }
                .ok_or_else(|| self.err(p, PromErrorKind::InvalidTimestamp))?;
                self.builder.append_i64_by_idx(self.ts_idx, ts);
                p = skip_blanks(self.buf, tend);
                if self.byte(p) == Some(b'#') {
                    p = self.skip_exemplar(p)?;
                }
            }
        }
        match self.byte(p) {
            None | Some(b'\n') => {}
            Some(_) => return Err(self.err(p, PromErrorKind::TrailingGarbage)),
        }

        if let (Some(type_idx), Some((ts, te, kind))) = (self.type_idx, self.family) {
            let raw = &self.buf[name.start..name.end];
            let decoded: &[u8] = if name.escaped {
                unescape_into(raw, &mut self.scratch.decoded);
                &self.scratch.decoded
            } else {
                raw
            };
            if belongs_to_family(decoded, &self.scratch.family_name, kind, self.openmetrics) {
                let is_created = self.openmetrics
                    && matches!(
                        kind,
                        FamilyKind::Counter | FamilyKind::Histogram | FamilyKind::Summary
                    )
                    && decoded.len() > self.scratch.family_name.len()
                    && decoded.ends_with(b"_created");
                // The raw name ends in the same bytes as the decoded one
                // (escapes never produce letters), so the type value can
                // borrow the buffer like every other value.
                let ty = if is_created && raw.ends_with(PROM_TYPE_CREATED) {
                    &self.buf[name.end - PROM_TYPE_CREATED.len()..name.end]
                } else {
                    &self.buf[ts..te]
                };
                self.builder.append_str_by_idx(type_idx, ty);
            }
        }
        Ok(p + 1)
    }

    /// Validate and skip an OpenMetrics exemplar (` # {labels} value
    /// [timestamp]`) whose `#` is at `pos`. Returns the position of the line
    /// end. Exemplars are not emitted; they are checked so a malformed line
    /// still rejects the scrape. Anything after the sample that is not
    /// `# {` is trailing garbage.
    #[inline]
    fn skip_exemplar(&mut self, pos: usize) -> Result<usize, PromParseError> {
        let open = skip_blanks(self.buf, pos + 1);
        if open == pos + 1 || self.byte(open) != Some(b'{') {
            return Err(self.err(pos, PromErrorKind::TrailingGarbage));
        }
        let bad = |w: &Self, at: usize| w.err(at, PromErrorKind::InvalidExemplar);
        // Label set: `}` or `name="value"` pairs separated by `,`.
        let mut p = skip_blanks(self.buf, open + 1);
        if self.byte(p) != Some(b'}') {
            loop {
                let key_end = if self.byte(p) == Some(b'"') {
                    self.read_quoted(p + 1)?.1
                } else {
                    let mut e = p;
                    while self
                        .byte(e)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
                    {
                        e += 1;
                    }
                    if !is_valid_label_name(&self.buf[p..e]) {
                        return Err(bad(self, p));
                    }
                    e
                };
                p = skip_blanks(self.buf, key_end);
                if self.byte(p) != Some(b'=') {
                    return Err(bad(self, p));
                }
                p = skip_blanks(self.buf, p + 1);
                if self.byte(p) != Some(b'"') {
                    return Err(bad(self, p));
                }
                p = skip_blanks(self.buf, self.read_quoted(p + 1)?.1);
                match self.byte(p) {
                    Some(b',') => p = skip_blanks(self.buf, p + 1),
                    Some(b'}') => break,
                    _ => return Err(bad(self, p)),
                }
                // A trailing comma before `}` is allowed.
                if self.byte(p) == Some(b'}') {
                    break;
                }
            }
        }
        // `p` is at `}`. Value, then an optional float-seconds timestamp.
        let token = |w: &Self, from: usize| {
            let mut e = from;
            while w.byte(e).is_some_and(|b| !is_blank(b) && b != b'\n') {
                e += 1;
            }
            e
        };
        let vs = skip_blanks(self.buf, p + 1);
        let ve = token(self, vs);
        if vs == p + 1 || ve == vs || parse_prom_value(&self.buf[vs..ve]).is_none() {
            return Err(bad(self, vs));
        }
        let mut p = skip_blanks(self.buf, ve);
        if p > ve && !matches!(self.byte(p), None | Some(b'\n')) {
            let te = token(self, p);
            if parse_seconds_as_ms(&self.buf[p..te]).is_none() {
                return Err(bad(self, p));
            }
            p = skip_blanks(self.buf, te);
        }
        match self.byte(p) {
            None | Some(b'\n') => Ok(p),
            Some(_) => Err(bad(self, p)),
        }
    }
}

/// Scan a Prometheus text exposition into `builder`, one row per sample.
///
/// Columns: [`PROM_NAME_FIELD`], one column per label name (zero-copy
/// strings), [`PROM_VALUE_FIELD`] (`f64`), [`PROM_TIMESTAMP_FIELD`] (`i64`
/// milliseconds, only when present) and, when enabled,
/// [`PROM_TYPE_FIELD`].
///
/// `buf` must be valid UTF-8, as the text format requires; anything else is
/// rejected with [`PromErrorKind::InvalidUtf8`] before any row is emitted.
///
/// The caller must have started a batch on `builder` over the same `buf`.
/// On `Err`, the builder holds a partial batch that must be discarded.
// Output correctness is also proven by `verify_scan_prom_text_name_value_oracle`.
#[verified(kani = "verify_scan_prom_text_no_panic")]
pub fn scan_prom_text<B: PromBuilder>(
    buf: &[u8],
    config: &PromScanConfig,
    scratch: &mut PromScratch,
    builder: &mut B,
) -> Result<PromScanStats, PromParseError> {
    if let Err(e) = core::str::from_utf8(buf) {
        let offset = e.valid_up_to();
        let line = buf
            .get(..offset)
            .map_or(0, |b| b.iter().fold(0, |n, &c| n + usize::from(c == b'\n')))
            + 1;
        return Err(PromParseError {
            line,
            offset,
            kind: PromErrorKind::InvalidUtf8,
        });
    }
    scratch.key_cache.clear();
    let name_idx = builder.resolve_field(PROM_NAME_FIELD);
    let value_idx = builder.resolve_field(PROM_VALUE_FIELD);
    let ts_idx = builder.resolve_field(PROM_TIMESTAMP_FIELD);
    let type_idx = config
        .emit_type
        .then(|| builder.resolve_field(PROM_TYPE_FIELD));
    let mut w = Walker {
        buf,
        cur: Cursor::new(buf),
        builder,
        scratch,
        line: 0,
        name_idx,
        value_idx,
        ts_idx,
        type_idx,
        openmetrics: config.openmetrics,
        family: None,
        last_name: None,
    };
    let mut stats = PromScanStats::default();
    let mut pos = 0usize;
    let mut saw_eof = false;
    while pos < buf.len() {
        w.line += 1;
        pos = skip_blanks(buf, pos);
        match w.byte(pos) {
            None => break,
            Some(b'\n') => pos += 1,
            Some(b'#') => match w.comment(pos)? {
                Some(next) => pos = next,
                None => {
                    // `# EOF` must be the last line; only blank space may
                    // follow it.
                    let line_end =
                        crate::byte_search::find_byte(buf, b'\n', pos).unwrap_or(buf.len());
                    let rest = buf.get(line_end..).unwrap_or_default();
                    if let Some(off) = rest.iter().position(|&b| !is_blank(b) && b != b'\n') {
                        return Err(w.err(line_end + off, PromErrorKind::DataAfterEof));
                    }
                    saw_eof = true;
                    break;
                }
            },
            Some(_) => {
                pos = w.sample(pos)?;
                stats.samples += 1;
            }
        }
    }
    // OpenMetrics requires the `# EOF` line; without it the body may have
    // been cut short.
    if config.openmetrics && !saw_eof {
        return Err(w.err(buf.len(), PromErrorKind::MissingEof));
    }
    stats.lines = w.line;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use core::fmt::Write as _;
    use proptest::prelude::*;

    /// Records every builder call as (column name, rendered value) per row.
    #[derive(Debug, Default)]
    struct Rec {
        names: Vec<Vec<u8>>,
        rows: Vec<Vec<(String, String)>>,
        cur: Vec<(String, String)>,
        open_rows: i32,
        lines: Vec<String>,
    }

    impl Rec {
        fn push(&mut self, idx: usize, v: String) {
            let k = String::from_utf8(self.names[idx].clone()).expect("should succeed");
            self.cur.push((k, v));
        }
    }

    impl ScanBuilder for Rec {
        fn begin_row(&mut self) {
            self.open_rows += 1;
            self.cur.clear();
        }
        fn end_row(&mut self) {
            self.open_rows -= 1;
            let row = core::mem::take(&mut self.cur);
            self.rows.push(row);
        }
        fn resolve_field(&mut self, key: &[u8]) -> usize {
            if let Some(i) = self.names.iter().position(|n| n == key) {
                return i;
            }
            self.names.push(key.to_vec());
            self.names.len() - 1
        }
        fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) {
            self.push(idx, String::from_utf8_lossy(value).into_owned());
        }
        fn append_int_by_idx(&mut self, _: usize, _: &[u8]) {
            unreachable!()
        }
        fn append_float_by_idx(&mut self, _: usize, _: &[u8]) {
            unreachable!()
        }
        fn append_bool_by_idx(&mut self, _: usize, _: bool) {
            unreachable!()
        }
        fn append_null_by_idx(&mut self, _: usize) {
            unreachable!()
        }
        fn append_line(&mut self, line: &[u8]) {
            self.lines.push(String::from_utf8_lossy(line).into_owned());
        }
    }

    impl PromBuilder for Rec {
        fn append_f64_by_idx(&mut self, idx: usize, value: f64) {
            self.push(idx, alloc::format!("{value:?}"));
        }
        fn append_i64_by_idx(&mut self, idx: usize, value: i64) {
            self.push(idx, value.to_string());
        }
    }

    fn scan(input: &str) -> Result<Vec<Vec<(String, String)>>, PromParseError> {
        scan_rows(input, &PromScanConfig::default())
    }

    fn scan_om(input: &str) -> Result<Vec<Vec<(String, String)>>, PromParseError> {
        let config = PromScanConfig {
            openmetrics: true,
            ..PromScanConfig::default()
        };
        scan_rows(input, &config)
    }

    fn scan_rows(
        input: &str,
        config: &PromScanConfig,
    ) -> Result<Vec<Vec<(String, String)>>, PromParseError> {
        let mut rec = Rec::default();
        let mut scratch = PromScratch::new();
        scan_prom_text(input.as_bytes(), config, &mut scratch, &mut rec)?;
        Ok(rec.rows)
    }

    fn row(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn simple_counter_with_type() {
        let rows = scan(
            "# HELP http_requests_total The total number of \"requests\".\n\
             # TYPE http_requests_total counter\n\
             http_requests_total{method=\"post\",code=\"200\"} 1027 1395066363000\n\
             http_requests_total{method=\"post\",code=\"400\"}    3 1395066363000\n",
        )
        .expect("should succeed");
        assert_eq!(
            rows,
            vec![
                row(&[
                    ("method", "post"),
                    ("code", "200"),
                    ("__name__", "http_requests_total"),
                    ("__value__", "1027.0"),
                    ("__timestamp_ms__", "1395066363000"),
                    ("__type__", "counter"),
                ]),
                row(&[
                    ("method", "post"),
                    ("code", "400"),
                    ("__name__", "http_requests_total"),
                    ("__value__", "3.0"),
                    ("__timestamp_ms__", "1395066363000"),
                    ("__type__", "counter"),
                ]),
            ]
        );
    }

    #[test]
    fn bare_sample_without_labels() {
        let rows = scan("up 1\n").expect("should succeed");
        assert_eq!(rows, vec![row(&[("__name__", "up"), ("__value__", "1.0")])]);
    }

    #[test]
    fn escapes_and_special_values() {
        let rows = scan(
            "msdos_file_access_time_seconds{path=\"C:\\\\DIR\\\\FILE.TXT\",error=\"Cannot find file:\\n\\\"FILE.TXT\\\"\"} 1.458255915e9\n\
             a +Inf\nb -Inf\nc NaN\n",
        )
        .expect("should succeed");
        assert_eq!(rows[0][0], ("path".into(), "C:\\DIR\\FILE.TXT".into()));
        assert_eq!(
            rows[0][1],
            ("error".into(), "Cannot find file:\n\"FILE.TXT\"".into())
        );
        assert_eq!(rows[0][3], ("__value__".into(), "1458255915.0".into()));
        assert_eq!(rows[1][1].1, "inf");
        assert_eq!(rows[2][1].1, "-inf");
        assert_eq!(rows[3][1].1, "NaN");
    }

    #[test]
    fn histogram_suffixes_inherit_family_type() {
        let rows = scan(
            "# TYPE rpc histogram\nrpc_bucket{le=\"0.5\"} 1\nrpc_bucket{le=\"+Inf\"} 2\nrpc_sum 3.5\nrpc_count 2\n# TYPE other gauge\nunrelated 1\n",
        )
        .expect("should succeed");
        for r in &rows[..4] {
            assert!(r.contains(&("__type__".into(), "histogram".into())));
        }
        assert!(!rows[4].iter().any(|(k, _)| k == "__type__"));
    }

    #[test]
    fn plus_signed_timestamp_is_accepted() {
        let rows = scan("up 1 +1395066363000\n").expect("should succeed");
        assert_eq!(
            rows[0][2],
            ("__timestamp_ms__".into(), "1395066363000".into())
        );
        assert_eq!(
            scan("up 1 +-5\n").expect_err("bad").kind,
            PromErrorKind::InvalidTimestamp
        );
        assert_eq!(
            scan("up 1 +\n").expect_err("bad").kind,
            PromErrorKind::InvalidTimestamp
        );
    }

    #[test]
    fn quoted_type_name_matches_quoted_samples() {
        let rows = scan("# TYPE \"my.metric\" gauge\n{\"my.metric\"} 1\n# TYPE \"a b\" counter\n{\"a b_total\"} 2\n")
            .expect("should succeed");
        assert!(rows[0].contains(&("__type__".into(), "gauge".into())));
        assert!(rows[1].contains(&("__type__".into(), "counter".into())));
    }

    #[test]
    fn unrelated_metric_sharing_a_prefix_gets_no_type() {
        let rows = scan("# TYPE foo counter\nfoo 1\nfoo_total 2\nfoobar 3\nfoo_bucketx 4\n")
            .expect("should succeed");
        let has_type = |r: &Vec<(String, String)>| r.iter().any(|(k, _)| k == "__type__");
        assert!(has_type(&rows[0]));
        assert!(has_type(&rows[1]));
        assert!(!has_type(&rows[2]));
        assert!(!has_type(&rows[3]));
    }

    #[test]
    fn blank_lines_comments_trailing_comma_and_no_final_newline() {
        let rows =
            scan("\n# just a comment\n  \nfoo{a=\"1\",} 2\n\tbar 3").expect("should succeed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], row(&[("__name__", "bar"), ("__value__", "3.0")]));
    }

    #[test]
    fn openmetrics_eof_and_exemplar() {
        let rows = scan_om("foo_bucket{le=\"1\"} 3 # {trace_id=\"abc\"} 0.5 123\nbar 1\n# EOF\n")
            .expect("should succeed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][2], ("__value__".into(), "3.0".into()));
        // The final line feed is optional.
        assert_eq!(scan_om("bar 1\n# EOF").expect("no final newline").len(), 1);
    }

    #[test]
    fn openmetrics_data_after_eof_is_rejected() {
        let err = scan_om("m 1\n# EOF\nn 2\n").expect_err("data after EOF");
        assert_eq!(err.kind, PromErrorKind::DataAfterEof);
        // Trailing blank lines are tolerated.
        assert_eq!(scan_om("m 1\n# EOF\n\n").expect("blank tail").len(), 1);
    }

    #[test]
    fn utf8_quoted_metric_and_label_names() {
        let rows = scan("{\"my.metric\", \"label.with.dots\"=\"v\"} 1\n").expect("should succeed");
        assert_eq!(
            rows,
            vec![row(&[
                ("label.with.dots", "v"),
                ("__name__", "my.metric"),
                ("__value__", "1.0"),
            ])]
        );
    }

    #[test]
    fn labels_named_like_synthetic_columns_are_kept_under_prefix() {
        let rows = scan("m{__value__=\"raw\",__type__=\"t\",__timestamp_ms__=\"x\"} 1\n")
            .expect("should succeed");
        assert_eq!(
            rows,
            vec![row(&[
                ("label:__value__", "raw"),
                ("label:__type__", "t"),
                ("label:__timestamp_ms__", "x"),
                ("__name__", "m"),
                ("__value__", "1.0"),
            ])]
        );
    }

    #[test]
    fn name_label_sets_metric_name_and_rejects_duplicates() {
        let rows = scan("{__name__=\"my.metric\",a=\"b\"} 1\n").expect("should succeed");
        assert_eq!(
            rows,
            vec![row(&[
                ("a", "b"),
                ("__name__", "my.metric"),
                ("__value__", "1.0")
            ])]
        );
        let err = scan("foo{__name__=\"bar\"} 1\n").expect_err("duplicate name");
        assert_eq!(err.kind, PromErrorKind::InvalidLabels);
    }

    #[test]
    fn label_values_may_contain_structural_bytes() {
        let rows = scan("m{a=\"x, y={z}\",b=\"=\"} 1\n").expect("should succeed");
        assert_eq!(rows[0][0], ("a".into(), "x, y={z}".into()));
        assert_eq!(rows[0][1], ("b".into(), "=".into()));
    }

    #[test]
    fn long_lines_cross_block_boundaries() {
        let long = "v".repeat(200);
        let input = alloc::format!("m{{a=\"{long}\",b=\"{long}\"}} 42 7\n");
        let rows = scan(&input).expect("should succeed");
        assert_eq!(rows[0][0].1, long);
        assert_eq!(rows[0][1].1, long);
        assert_eq!(rows[0][4], ("__timestamp_ms__".into(), "7".into()));
    }

    #[test]
    fn malformed_lines_are_errors() {
        let cases: &[(&str, PromErrorKind)] = &[
            ("foo\n", PromErrorKind::MissingValue),
            ("foo{a=\"1\"}\n", PromErrorKind::MissingValue),
            ("foo abc\n", PromErrorKind::InvalidValue),
            ("foo 1 abc\n", PromErrorKind::InvalidTimestamp),
            ("foo 1 2 3\n", PromErrorKind::TrailingGarbage),
            ("foo{a=1} 1\n", PromErrorKind::InvalidLabels),
            ("foo{a=\"1\" 1\n", PromErrorKind::InvalidLabels),
            ("foo{a=\"1} 1\n", PromErrorKind::UnterminatedString),
            ("{a=\"1\"} 1\n", PromErrorKind::MissingName),
            ("foo{=\"1\"} 1\n", PromErrorKind::InvalidLabels),
            ("9bad 1\n", PromErrorKind::InvalidName),
            ("foo-bar 1\n", PromErrorKind::InvalidName),
            ("m{bad-name=\"x\"} 1\n", PromErrorKind::InvalidName),
            ("m{a:b=\"x\"} 1\n", PromErrorKind::InvalidName),
            ("m{a=\"x\\\n\"} 1\n", PromErrorKind::UnterminatedString),
            ("m{a=\"x\\", PromErrorKind::UnterminatedString),
        ];
        for (input, kind) in cases {
            let err = scan(input).expect_err(input);
            assert_eq!(err.kind, *kind, "{input:?}");
            assert_eq!(err.line, 1);
        }
        assert_eq!(scan("ok 1\nbad\n").expect_err("should fail").line, 2);
    }

    #[test]
    fn failed_sample_closes_its_row() {
        for input in ["bad\n", "m{a=\"1\" 1\n", "9bad 1\n", "m 1 x\n"] {
            let mut rec = Rec::default();
            let mut scratch = PromScratch::new();
            scan_prom_text(
                input.as_bytes(),
                &PromScanConfig::default(),
                &mut scratch,
                &mut rec,
            )
            .expect_err(input);
            assert_eq!(rec.open_rows, 0, "{input:?}");
        }
    }

    #[test]
    fn quoted_names_bypass_legacy_charset() {
        let rows = scan("{\"my.metric\", \"a-b\"=\"x\"} 1\nok:sub_1{_l1=\"y\"} 2\n")
            .expect("should parse");
        assert_eq!(rows.len(), 2);
    }

    fn scan_with(input: &[u8], config: PromScanConfig) -> Result<Rec, PromParseError> {
        let mut rec = Rec::default();
        let mut scratch = PromScratch::new();
        scan_prom_text(input, &config, &mut scratch, &mut rec)?;
        Ok(rec)
    }

    fn type_of(row: &[(String, String)]) -> Option<&str> {
        row.iter()
            .find(|(k, _)| k == "__type__")
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn eof_with_trailing_tokens_is_an_ordinary_comment() {
        let rows = scan_om("up 1\n# EOF pending\nother 2\n# EOF\n").expect("should parse");
        assert_eq!(rows.len(), 2);
        let rows = scan_om("up 1\n#  EOF  \n").expect("padded EOF still ends the body");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn openmetrics_without_eof_is_rejected() {
        let err = scan_om("up 1\n").expect_err("no EOF");
        assert_eq!(err.kind, PromErrorKind::MissingEof);
        let err = scan_om("up 1\n#EOF\n").expect_err("#EOF is not the EOF line");
        assert_eq!(err.kind, PromErrorKind::MissingEof);
        assert_eq!(scan("up 1\n").expect("text needs no EOF").len(), 1);
    }

    #[test]
    fn type_directive_needs_no_space_after_hash() {
        let rows = scan("#TYPE requests counter\nrequests 1\n#\tTYPE g gauge\ng 2\n")
            .expect("should parse");
        let types: Vec<_> = rows.iter().map(|r| type_of(r)).collect();
        assert_eq!(types, vec![Some("counter"), Some("gauge")]);
    }

    #[test]
    fn eof_is_an_ordinary_comment_in_prometheus_text() {
        let rows = scan("a 1\n# EOF\nb 2\n").expect("should parse");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn malformed_type_lines_are_errors() {
        for input in [
            "# TYPE up invalid\nup 1\n",
            "# TYPE up\nup 1\n",
            "# TYPE\nup 1\n",
            "# TYPE up gauge extra\nup 1\n",
            "# TYPE \"up gauge\n",
            "# TYPE 1up gauge\n",
            "# TYPE up-down gauge\n",
        ] {
            let err = scan(input).expect_err(input);
            assert_eq!(err.kind, PromErrorKind::InvalidType, "{input:?}");
        }
        let rows = scan("# TYPE up GAUGE\n# HELP up anything goes\n#comment\nup 1\n")
            .expect("should parse");
        assert_eq!(type_of(&rows[0]), Some("GAUGE"));
    }

    #[test]
    fn escaped_type_name_keeps_its_type() {
        let rows = scan("# TYPE \"a\\\"b\" gauge\n{\"a\\\"b\"} 1\n").expect("should parse");
        assert_eq!(rows[0][0], ("__name__".into(), "a\"b".into()));
        assert_eq!(type_of(&rows[0]), Some("gauge"));
    }

    #[test]
    fn family_suffixes_depend_on_declared_type() {
        let rows = scan(
            "# TYPE g gauge\ng 1\ng_total 2\n# TYPE c counter\nc_total 1\nc_bucket 2\n\
             # TYPE s summary\ns{quantile=\"0.5\"} 1\ns_count 2\ns_bucket 3\n",
        )
        .expect("should parse");
        let types: Vec<_> = rows.iter().map(|r| type_of(r)).collect();
        assert_eq!(
            types,
            vec![
                Some("gauge"),
                None,
                Some("counter"),
                None,
                Some("summary"),
                Some("summary"),
                None
            ]
        );
    }

    #[test]
    fn openmetrics_created_samples_are_typed_created() {
        let rows = scan_om(
            "# TYPE jobs counter\njobs_total 3\njobs_created 1.7e9\n\
             # TYPE lat histogram\nlat_bucket{le=\"+Inf\"} 1\nlat_count 1\nlat_created{a=\"b\"} 1.7e9\n\
             # TYPE \"my.metric\" counter\n{\"my.metric_total\"} 3\n{\"my.metric_created\"} 1.7e9\n\
             # TYPE w_created counter\nw_created 4\n# EOF\n",
        )
        .expect("should parse");
        let types: Vec<_> = rows.iter().map(|r| type_of(r)).collect();
        assert_eq!(
            types,
            vec![
                Some("counter"),
                Some("created"),
                Some("histogram"),
                Some("histogram"),
                Some("created"),
                Some("counter"),
                Some("created"),
                Some("counter"),
            ]
        );
    }

    #[test]
    fn malformed_openmetrics_created_sample_is_rejected() {
        let err = scan_om("# TYPE jobs counter\njobs_created not-a-number\n# EOF\n")
            .expect_err("bad value");
        assert_eq!(err.kind, PromErrorKind::InvalidValue);
    }

    #[test]
    fn created_series_is_kept_in_prometheus_text() {
        let rows =
            scan("# TYPE foo_total counter\nfoo_total 3\nfoo_created 4\n").expect("should parse");
        assert_eq!(rows.len(), 2);
        // Prometheus text has no `_created` component: these are untyped.
        for body in [
            "# TYPE foo histogram\nfoo_count 1\nfoo_created 4\n",
            "# TYPE foo summary\nfoo_count 1\nfoo_created 4\n",
            "# TYPE foo counter\nfoo 1\nfoo_created 4\n",
        ] {
            let rows = scan(body).expect("should parse");
            assert_eq!(rows.len(), 2);
            assert_eq!(type_of(&rows[1]), None, "{body}: foo_created is untyped");
        }
    }

    #[test]
    fn quoted_name_after_bare_name_is_an_error() {
        let err = scan("up{\"other\"} 1\n").expect_err("two names");
        assert_eq!(err.kind, PromErrorKind::InvalidLabels);
    }

    #[test]
    fn quoted_label_with_reserved_prefix_does_not_collide() {
        let rows = scan("m{__value__=\"a\",\"label:__value__\"=\"b\"} 1\n").expect("should parse");
        assert_eq!(rows[0][0], ("label:__value__".into(), "a".into()));
        assert_eq!(rows[0][1], ("label:label:__value__".into(), "b".into()));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for input in ["m 1e999\n", "m -1e999\n"] {
            assert_eq!(
                scan(input).expect_err(input).kind,
                PromErrorKind::InvalidValue
            );
        }
        let rows = scan("a +Inf\nb -inf\nc Infinity\n").expect("explicit infinities");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn exemplars_need_hash_space_brace() {
        assert!(scan("m 1 # {a=\"b\"} 2\n").is_ok());
        assert!(scan("m 1 5 # {a=\"b\"} 2 3\n").is_ok());
        assert!(scan("m 1 # {} 2 1.5\n").is_ok());
        assert!(scan("m 1 # {a=\"x\\\"y\", \"b.c\"=\"d\",} 2\n").is_ok());
        for input in ["m 1 # trailing\n", "m 1 #{a=\"b\"} 2\n", "m 1 5 #\n"] {
            assert_eq!(
                scan(input).expect_err(input).kind,
                PromErrorKind::TrailingGarbage,
                "{input:?}"
            );
        }
    }

    #[test]
    fn malformed_exemplars_are_rejected() {
        for input in [
            "m 1 # {a=b} 2\n",
            "m 1 # {a=\"b\" 2\n",
            "m 1 # {1a=\"b\"} 2\n",
            "m 1 # {a=\"b\"}\n",
            "m 1 # {a=\"b\"}2\n",
            "m 1 # {a=\"b\"} x\n",
            "m 1 # {a=\"b\"} 2 later\n",
            "m 1 # {a=\"b\"} 2 3 4\n",
        ] {
            assert_eq!(
                scan(input).expect_err(input).kind,
                PromErrorKind::InvalidExemplar,
                "{input:?}"
            );
        }
    }

    #[test]
    fn openmetrics_timestamps_are_seconds() {
        let config = PromScanConfig {
            openmetrics: true,
            ..PromScanConfig::default()
        };
        let rec = scan_with(
            b"a 1 1520879607.789\nb 2 1520879607\nc 3 1.5e3\n# EOF\n",
            config,
        )
        .expect("should parse");
        let ts: Vec<_> = rec.rows.iter().map(|r| r[2].1.clone()).collect();
        assert_eq!(ts, vec!["1520879607789", "1520879607000", "1500000"]);
        let err = scan_with(b"a 1 NaN\n# EOF\n", config).expect_err("nan timestamp");
        assert_eq!(err.kind, PromErrorKind::InvalidTimestamp);
        // Text format stays integer milliseconds.
        let err = scan("a 1 1.5\n").expect_err("float timestamp");
        assert_eq!(err.kind, PromErrorKind::InvalidTimestamp);
    }

    #[test]
    fn invalid_utf8_is_rejected_before_any_row() {
        let input = b"ok 1\nm{a=\"\xff\"} 1\n";
        let mut rec = Rec::default();
        let mut scratch = PromScratch::new();
        let err = scan_prom_text(input, &PromScanConfig::default(), &mut scratch, &mut rec)
            .expect_err("invalid utf-8");
        assert_eq!(err.kind, PromErrorKind::InvalidUtf8);
        assert_eq!(err.line, 2);
        assert!(rec.rows.is_empty());
    }

    #[test]
    fn each_sample_emits_its_raw_line() {
        let rec = scan_with(
            b"# TYPE up gauge\n  up 1\nm{a=\"b\"} 2 3\nlast 4",
            PromScanConfig::default(),
        )
        .expect("should parse");
        assert_eq!(rec.lines, vec!["up 1", "m{a=\"b\"} 2 3", "last 4"]);
    }

    #[test]
    fn value_fast_path_matches_core_parser() {
        for s in [
            "0",
            "1",
            "-1",
            "1027",
            "0.5",
            "-0.25",
            "123456789012345",
            "3.14159265358979",
            "1234567.000001",
            "0.000000000000001",
            "1e3",
            "1.5e-07",
            "+3",
            "007.50",
        ] {
            assert_eq!(
                parse_prom_value(s.as_bytes()).map(f64::to_bits),
                s.parse::<f64>().ok().map(f64::to_bits),
                "{s}"
            );
        }
        assert_eq!(parse_prom_value(b""), None);
        assert_eq!(parse_prom_value(b"-"), None);
        assert_eq!(parse_prom_value(b"1.2.3"), None);
    }

    proptest! {
        // No failure persistence: it calls `getcwd`, which Miri's isolation
        // rejects. Fewer cases under Miri, where each one is ~100x slower.
        #![proptest_config(ProptestConfig {
            failure_persistence: None,
            cases: if cfg!(miri) { 32 } else { 256 },
            .. ProptestConfig::default()
        })]
        #[test]
        fn simd_mask_eq_scalar(block in proptest::collection::vec(any::<u8>(), 64)) {
            let arr: [u8; 64] = block.try_into().expect("should succeed");
            prop_assert_eq!(prom_structural_mask(&arr), prom_structural_mask_scalar(&arr));
        }

        #[test]
        fn simd_mask_eq_scalar_structural_heavy(
            block in proptest::collection::vec(
                prop::sample::select(b"\n \t\"\\{}=,a#".to_vec()), 64)
        ) {
            let arr: [u8; 64] = block.try_into().expect("should succeed");
            prop_assert_eq!(prom_structural_mask(&arr), prom_structural_mask_scalar(&arr));
        }

        #[test]
        fn decimal_fast_path_is_correctly_rounded(int in 0u64..1_000_000_000, frac in 0u64..1_000_000, w in 1usize..7) {
            let s = alloc::format!("{int}.{frac:0w$}");
            prop_assert_eq!(
                parse_prom_value(s.as_bytes()).map(f64::to_bits),
                s.parse::<f64>().ok().map(f64::to_bits)
            );
        }

        #[test]
        fn never_panics_on_arbitrary_input(input in proptest::collection::vec(
            prop::sample::select(b"\n \t\"\\{}=,a1.#eE+-ITPYH".to_vec()), 0..300)
        ) {
            let mut rec = Rec::default();
            let mut scratch = PromScratch::new();
            let _ = scan_prom_text(&input, &PromScanConfig::default(), &mut scratch, &mut rec);
        }

        #[test]
        fn roundtrips_generated_expositions(
            samples in proptest::collection::vec(
                (
                    "[a-z_][a-z0-9_]{0,20}",
                    // Reserved `__` keys (`__name__`, `__type__`) have their
                    // own tests; keep them out of the round trip.
                    proptest::collection::vec(("[a-z][a-z0-9_]{0,8}", "[ -~]{0,30}"), 0..5),
                    -1.0e12f64..1.0e12,
                    proptest::option::of(0i64..2_000_000_000_000),
                ),
                1..20,
            )
        ) {
            let mut text = String::new();
            let mut expected = Vec::new();
            for (name, labels, value, ts) in &samples {
                let mut seen = alloc::collections::BTreeSet::new();
                let labels: Vec<_> = labels.iter().filter(|(k, _)| seen.insert(k.clone())).collect();
                text.push_str(name);
                let mut exp = Vec::new();
                if !labels.is_empty() {
                    text.push('{');
                    for (i, (k, v)) in labels.iter().enumerate() {
                        if i > 0 { text.push(','); }
                        let esc = v.replace('\\', "\\\\").replace('"', "\\\"");
                        write!(text, "{k}=\"{esc}\"").expect("write");
                        exp.push((k.clone(), v.clone()));
                    }
                    text.push('}');
                }
                exp.insert(0, ("__name__".to_string(), name.clone()));
                write!(text, " {value:?}").expect("write");
                exp.push(("__value__".to_string(), alloc::format!("{value:?}")));
                if let Some(ts) = ts {
                    write!(text, " {ts}").expect("write");
                    exp.push(("__timestamp_ms__".to_string(), ts.to_string()));
                }
                text.push('\n');
                expected.push(exp);
            }
            let got = scan(&text).expect("should succeed");
            // Builder-level duplicate suppression is not modeled by `Rec`, so
            // compare as label sets keyed by first occurrence.
            for (g, e) in got.iter().zip(&expected) {
                let mut g2: Vec<_> = Vec::new();
                for kv in g {
                    if !g2.iter().any(|(k, _): &(String, String)| *k == kv.0) {
                        g2.push(kv.clone());
                    }
                }
                let mut e2 = e.clone();
                g2.sort();
                e2.sort();
                prop_assert_eq!(g2, e2);
            }
            prop_assert_eq!(got.len(), expected.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Kani proofs
// ---------------------------------------------------------------------------

#[cfg(kani)]
mod verification {
    use super::*;

    /// SIMD classifier agrees with the scalar specification on every block.
    #[kani::proof]
    #[kani::unwind(65)]
    pub(super) fn verify_prom_structural_mask_simd_eq_scalar() {
        let block: [u8; 64] = kani::any();
        let simd = prom_structural_mask(&block);
        let scalar = prom_structural_mask_scalar(&block);
        assert_eq!(simd, scalar);
        for (i, &b) in block.iter().enumerate() {
            assert_eq!((scalar >> i) & 1 == 1, is_prom_structural(b));
        }
        kani::cover!(scalar != 0, "some structural byte");
        kani::cover!(scalar == 0, "no structural bytes");
    }

    /// Short integer values take the fast path and are exact.
    #[kani::stub(parse_prom_value_fallback, parse_prom_value_fallback_stub)]
    #[kani::proof]
    #[kani::unwind(6)]
    pub(super) fn verify_parse_prom_value_integers() {
        let n: u16 = kani::any();
        let mut buf = [0u8; 5];
        let mut len = 0;
        let mut v = n;
        loop {
            buf[4 - len] = b'0' + (v % 10) as u8;
            len += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let parsed = parse_prom_value(&buf[5 - len..]);
        assert_eq!(parsed, Some(f64::from(n)));
        kani::cover!(n > 9999, "five digits");
    }

    /// Signed values with a decimal point take the fast path and are exact
    /// when the fraction is zero: `[+-]n.0` and `[+-]n.00` equal `±n`.
    /// The oracle is the integer itself, independent of the parser's
    /// division. Rounding of non-zero fractions is covered by the
    /// `decimal_fast_path_is_correctly_rounded` proptest (see VERIFICATION.md).
    #[kani::stub(parse_prom_value_fallback, parse_prom_value_fallback_stub)]
    #[kani::proof]
    #[kani::unwind(8)]
    pub(super) fn verify_parse_prom_value_signed_decimal_exact() {
        let n: u8 = kani::any();
        let sign: u8 = kani::any();
        let zeros: usize = kani::any();
        kani::assume(sign < 3);
        kani::assume(zeros <= 2);
        let mut buf = [0u8; 8];
        let mut len = 0;
        if sign == 1 {
            buf[0] = b'-';
            len = 1;
        } else if sign == 2 {
            buf[0] = b'+';
            len = 1;
        }
        let mut digits = [0u8; 3];
        let mut n_digits = 0;
        let mut v = n;
        loop {
            digits[2 - n_digits] = b'0' + v % 10;
            n_digits += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        buf[len..len + n_digits].copy_from_slice(&digits[3 - n_digits..]);
        len += n_digits;
        if zeros > 0 {
            buf[len] = b'.';
            len += 1;
            for _ in 0..zeros {
                buf[len] = b'0';
                len += 1;
            }
        }
        let expected = if sign == 1 {
            -f64::from(n)
        } else {
            f64::from(n)
        };
        let parsed = parse_prom_value(&buf[..len]);
        assert!(parsed.is_some_and(|p| p.to_bits() == expected.to_bits()));
        kani::cover!(sign == 1 && zeros > 0, "negative decimal");
        kani::cover!(
            sign == 2 && zeros == 2,
            "explicit plus, two fraction digits"
        );
    }

    /// Stub for `core`'s float parser: any result. Oracle proofs that use
    /// it also show the fast path is taken, since an arbitrary fallback
    /// result would break their exact assertions.
    fn parse_prom_value_fallback_stub(_bytes: &[u8], _digits: &[u8]) -> Option<f64> {
        if kani::any() { Some(kani::any()) } else { None }
    }

    /// Stub for the SIMD block classifier in the scanner proofs. Their
    /// inputs are at most 8 bytes, and the scanner zero-pads the tail block,
    /// so only the first 8 bytes can be structural; the stub classifies those
    /// with the scalar predicate, unrolled. SIMD ≡ scalar for all 64 bytes is
    /// proven separately by `verify_prom_structural_mask_simd_eq_scalar`.
    #[cfg_attr(not(feature = "kani-slow"), allow(dead_code))]
    fn prom_structural_mask_stub(block: &[u8; 64]) -> u64 {
        let bit = |i: usize| u64::from(is_prom_structural(block[i])) << i;
        bit(0) | bit(1) | bit(2) | bit(3) | bit(4) | bit(5) | bit(6) | bit(7)
    }

    struct NullBuilder;
    impl ScanBuilder for NullBuilder {
        fn begin_row(&mut self) {}
        fn end_row(&mut self) {}
        fn resolve_field(&mut self, _: &[u8]) -> usize {
            0
        }
        fn append_str_by_idx(&mut self, _: usize, _: &[u8]) {}
        fn append_int_by_idx(&mut self, _: usize, _: &[u8]) {}
        fn append_float_by_idx(&mut self, _: usize, _: &[u8]) {}
        fn append_bool_by_idx(&mut self, _: usize, _: bool) {}
        fn append_null_by_idx(&mut self, _: usize) {}
        fn append_line(&mut self, _: &[u8]) {}
    }
    impl PromBuilder for NullBuilder {
        fn append_f64_by_idx(&mut self, _: usize, _: f64) {}
        fn append_i64_by_idx(&mut self, _: usize, _: i64) {}
    }

    /// The value parser never panics on any input of up to 8 bytes.
    #[kani::stub(parse_prom_value_fallback, parse_prom_value_fallback_stub)]
    #[kani::proof]
    #[kani::unwind(10)]
    pub(super) fn verify_parse_prom_value_no_panic() {
        let buf: [u8; 8] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= buf.len());
        let r = parse_prom_value(&buf[..len]);
        kani::cover!(r.is_some(), "parsed");
        kani::cover!(r.is_none(), "rejected");
    }

    /// Records the name and value of each row, for the oracle proof.
    struct OneRow {
        name: [u8; 4],
        name_len: usize,
        value: Option<f64>,
        rows: usize,
    }
    impl ScanBuilder for OneRow {
        fn begin_row(&mut self) {}
        fn end_row(&mut self) {
            self.rows += 1;
        }
        fn resolve_field(&mut self, key: &[u8]) -> usize {
            if key == PROM_NAME_FIELD {
                0
            } else if key == PROM_VALUE_FIELD {
                1
            } else {
                2
            }
        }
        fn append_str_by_idx(&mut self, idx: usize, value: &[u8]) {
            if idx == 0 && value.len() <= self.name.len() {
                self.name[..value.len()].copy_from_slice(value);
                self.name_len = value.len();
            }
        }
        fn append_int_by_idx(&mut self, _: usize, _: &[u8]) {}
        fn append_float_by_idx(&mut self, _: usize, _: &[u8]) {}
        fn append_bool_by_idx(&mut self, _: usize, _: bool) {}
        fn append_null_by_idx(&mut self, _: usize) {}
        fn append_line(&mut self, _: &[u8]) {}
    }
    impl PromBuilder for OneRow {
        fn append_f64_by_idx(&mut self, idx: usize, value: f64) {
            if idx == 1 {
                self.value = Some(value);
            }
        }
        fn append_i64_by_idx(&mut self, _: usize, _: i64) {}
    }

    /// Oracle: for every `m <digit>\n` line, the scanner emits exactly one
    /// row named `m` whose value is the digit.
    // Gated behind `kani-slow` (nightly and `ci:full`): over 10 min under
    // CBMC because of scratch `Vec` growth. `cfg_attr` keeps the function, so
    // `#[verified]` on `scan_prom_text` still resolves.
    #[cfg_attr(
        feature = "kani-slow",
        kani::proof,
        kani::unwind(10),
        kani::stub(prom_structural_mask, prom_structural_mask_stub),
        kani::stub(parse_prom_value_fallback, parse_prom_value_fallback_stub)
    )]
    #[cfg_attr(not(feature = "kani-slow"), allow(dead_code))]
    pub(super) fn verify_scan_prom_text_name_value_oracle() {
        let d: u8 = kani::any();
        kani::assume(d.is_ascii_digit());
        let buf = [b'm', b' ', d, b'\n'];
        let mut out = OneRow {
            name: [0; 4],
            name_len: 0,
            value: None,
            rows: 0,
        };
        let mut scratch = PromScratch::new();
        let r = scan_prom_text(&buf, &PromScanConfig::default(), &mut scratch, &mut out);
        assert!(r.is_ok());
        assert_eq!(out.rows, 1);
        assert_eq!(&out.name[..out.name_len], b"m");
        assert_eq!(out.value, Some(f64::from(d - b'0')));
        kani::cover!(d == b'9', "largest digit");
    }

    /// The scanner never panics when any one byte of a minimal sample
    /// (`m 1\\n`) is replaced by an arbitrary byte. Inputs of any length are
    /// covered by the `never_panics_on_arbitrary_input` proptest; fully
    /// symbolic buffers exceed CBMC's budget because of scratch `Vec` growth.
    // Gated behind `kani-slow` (nightly and `ci:full`): over 10 min under
    // CBMC because of scratch `Vec` growth. `cfg_attr` keeps the function, so
    // `#[verified]` on `scan_prom_text` still resolves.
    #[cfg_attr(
        feature = "kani-slow",
        kani::proof,
        kani::unwind(10),
        kani::stub(prom_structural_mask, prom_structural_mask_stub),
        kani::stub(parse_prom_value_fallback, parse_prom_value_fallback_stub)
    )]
    #[cfg_attr(not(feature = "kani-slow"), allow(dead_code))]
    pub(super) fn verify_scan_prom_text_no_panic() {
        let pos: usize = kani::any();
        kani::assume(pos < 4);
        let mut buf = [b'm', b' ', b'1', b'\n'];
        buf[pos] = kani::any();
        let mut scratch = PromScratch::new();
        let r = scan_prom_text(
            &buf,
            &PromScanConfig::default(),
            &mut scratch,
            &mut NullBuilder,
        );
        kani::cover!(r.is_ok(), "accepted input");
        kani::cover!(r.is_err(), "rejected input");
    }
}
