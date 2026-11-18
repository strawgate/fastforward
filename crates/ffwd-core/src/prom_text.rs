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
pub const PROM_TYPE_FIELD: &[u8] = b"__type__";

/// Prefix for labels whose name collides with a synthetic column
/// (`__value__`, `__timestamp_ms__`, `__type__`). Such labels are invalid in
/// Prometheus (the `__` prefix is reserved) but are kept rather than dropped.
pub const PROM_RESERVED_LABEL_PREFIX: &[u8] = b"label:";

/// Sample-name suffixes that belong to a declared family (histogram,
/// summary, gauge histogram, OpenMetrics counter / info / `_created`).
const FAMILY_SUFFIXES: [&[u8]; 8] = [
    b"_bucket",
    b"_sum",
    b"_count",
    b"_total",
    b"_created",
    b"_info",
    b"_gcount",
    b"_gsum",
];

/// True when `sample` is the family `family` itself or one of its component
/// series (`family` + a suffix from [`FAMILY_SUFFIXES`]).
#[inline]
fn belongs_to_family(sample: &[u8], family: &[u8]) -> bool {
    match sample.strip_prefix(family) {
        Some([]) => true,
        Some(rest) => FAMILY_SUFFIXES.contains(&rest),
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
    key == PROM_VALUE_FIELD || key == PROM_TIMESTAMP_FIELD || key == PROM_TYPE_FIELD
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
}

impl Default for PromScanConfig {
    fn default() -> Self {
        Self { emit_type: true }
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
    block: usize,
    mask: u64,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            block: usize::MAX,
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
            if block != self.block {
                self.block = block;
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
#[verified(kani = "verify_parse_prom_value_integers")]
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
    let s = core::str::from_utf8(bytes).ok()?;
    s.parse::<f64>().ok()
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
    /// Current `# TYPE` family name span and type token span.
    family: Option<(usize, usize, usize, usize)>,
    /// Span of the last bare metric name that passed validation. Series of
    /// one family sit on consecutive lines, so most names skip the check.
    last_name: (usize, usize),
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
        if is_reserved_column(&k) {
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
                Some(c @ (b',' | b'}'))
                    if quoted_name.is_none() && self.byte(key.0.wrapping_sub(1)) == Some(b'"') =>
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
    /// line, or `None` when `# EOF` ends the exposition.
    fn comment(&mut self, pos: usize) -> Option<usize> {
        let end = crate::byte_search::find_byte(self.buf, b'\n', pos).unwrap_or(self.buf.len());
        let line = &self.buf[pos..end];
        // Tokenize: "#", keyword, name, rest.
        let mut toks = line
            .split(|&b| is_blank(b))
            .filter(|t| !t.is_empty())
            .map(|t| {
                let start = t.as_ptr() as usize - self.buf.as_ptr() as usize;
                (start, start + t.len())
            });
        let _hash = toks.next();
        let keyword = toks.next();
        match keyword.map(|(s, e)| &self.buf[s..e]) {
            Some(b"TYPE") => {
                let after_kw = keyword.map_or(end, |(_, e)| e);
                self.family = self.parse_type_line(after_kw, end);
            }
            Some(b"EOF") => return None,
            _ => {}
        }
        Some(end + 1)
    }

    /// Parse the `<name> <type>` part of a `# TYPE` line in `pos..end`.
    /// The name may be quoted (UTF-8 metric names); quoted names that contain
    /// escapes are not matched against samples, so they yield `None`.
    fn parse_type_line(&self, pos: usize, end: usize) -> Option<(usize, usize, usize, usize)> {
        let token_end = |from: usize| {
            let mut i = from;
            while i < end && self.byte(i).is_some_and(|b| !is_blank(b)) {
                i += 1;
            }
            i
        };
        let p = skip_blanks(self.buf, pos);
        let (name_start, name_end, after) = if self.byte(p) == Some(b'"') {
            let mut q = p + 1;
            loop {
                match self.byte(q) {
                    _ if q >= end => return None,
                    Some(b'\\') => return None,
                    Some(b'"') => break,
                    _ => q += 1,
                }
            }
            (p + 1, q, q + 1)
        } else {
            let e = token_end(p);
            (p, e, e)
        };
        let ts = skip_blanks(self.buf, after);
        let te = token_end(ts);
        (name_end > name_start && te > ts && ts < end).then_some((name_start, name_end, ts, te))
    }

    /// Parse one sample line starting at `pos` (first non-blank byte).
    /// The row is closed on error too, so the caller can discard the batch.
    #[inline(always)]
    fn sample(&mut self, pos: usize) -> Result<usize, PromParseError> {
        self.builder.begin_row();
        let r = self.sample_row(pos);
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
            let (ls, le) = self.last_name;
            if le == ls || self.buf.get(ls..le) != Some(bare) {
                if !is_valid_metric_name(bare) {
                    return Err(self.err(pos, PromErrorKind::InvalidName));
                }
                self.last_name = (pos, q);
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
            Some(b'#') => p = self.skip_exemplar(p),
            Some(_) => {
                let tend = self.cur.next_from(p);
                let raw_ts = &self.buf[p..tend];
                let raw_ts = match raw_ts {
                    [b'+', rest @ ..] if rest.first().is_some_and(u8::is_ascii_digit) => rest,
                    other => other,
                };
                let ts = parse_int_fast(raw_ts)
                    .ok_or_else(|| self.err(p, PromErrorKind::InvalidTimestamp))?;
                self.builder.append_i64_by_idx(self.ts_idx, ts);
                p = skip_blanks(self.buf, tend);
                if self.byte(p) == Some(b'#') {
                    p = self.skip_exemplar(p);
                }
            }
        }
        match self.byte(p) {
            None | Some(b'\n') => {}
            Some(_) => return Err(self.err(p, PromErrorKind::TrailingGarbage)),
        }

        if let (Some(type_idx), Some((fs, fe, ts, te))) = (self.type_idx, self.family)
            && belongs_to_family(&self.buf[name.start..name.end], &self.buf[fs..fe])
        {
            self.builder.append_str_by_idx(type_idx, &self.buf[ts..te]);
        }
        Ok(p + 1)
    }

    #[inline]
    fn skip_exemplar(&self, pos: usize) -> usize {
        crate::byte_search::find_byte(self.buf, b'\n', pos).unwrap_or(self.buf.len())
    }
}

/// Scan a Prometheus text exposition into `builder`, one row per sample.
///
/// Columns: [`PROM_NAME_FIELD`], one column per label name (zero-copy
/// strings), [`PROM_VALUE_FIELD`] (`f64`), [`PROM_TIMESTAMP_FIELD`] (`i64`
/// milliseconds, only when present) and, when enabled,
/// [`PROM_TYPE_FIELD`].
///
/// The caller must have started a batch on `builder` over the same `buf`.
/// On `Err`, the builder holds a partial batch that must be discarded.
#[verified(kani = "verify_scan_prom_text_no_panic")]
pub fn scan_prom_text<B: PromBuilder>(
    buf: &[u8],
    config: &PromScanConfig,
    scratch: &mut PromScratch,
    builder: &mut B,
) -> Result<PromScanStats, PromParseError> {
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
        family: None,
        last_name: (0, 0),
    };
    let mut stats = PromScanStats::default();
    let mut pos = 0usize;
    while pos < buf.len() {
        w.line += 1;
        pos = skip_blanks(buf, pos);
        match w.byte(pos) {
            None => break,
            Some(b'\n') => pos += 1,
            Some(b'#') => match w.comment(pos) {
                Some(next) => pos = next,
                None => break,
            },
            Some(_) => {
                pos = w.sample(pos)?;
                stats.samples += 1;
            }
        }
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
    #[derive(Default)]
    struct Rec {
        names: Vec<Vec<u8>>,
        rows: Vec<Vec<(String, String)>>,
        cur: Vec<(String, String)>,
        open_rows: i32,
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
        fn append_line(&mut self, _: &[u8]) {}
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
        let mut rec = Rec::default();
        let mut scratch = PromScratch::new();
        scan_prom_text(
            input.as_bytes(),
            &PromScanConfig::default(),
            &mut scratch,
            &mut rec,
        )?;
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
        let rows = scan(
            "foo_bucket{le=\"1\"} 3 # {trace_id=\"abc\"} 0.5 123\nbar 1\n# EOF\nignored garbage {\n",
        )
        .expect("should succeed");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][2], ("__value__".into(), "3.0".into()));
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
                    proptest::collection::vec(("[a-z_][a-z0-9_]{0,8}", "[ -~]{0,30}"), 0..5),
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
    fn verify_prom_structural_mask_simd_eq_scalar() {
        let block: [u8; 64] = kani::any();
        let simd = prom_structural_mask(&block);
        let scalar = prom_structural_mask_scalar(&block);
        assert_eq!(simd, scalar);
        for (i, &b) in block.iter().enumerate() {
            assert_eq!((scalar >> i) & 1 == 1, is_prom_structural(b));
        }
        kani::cover!(scalar != 0, "some structural byte");
    }

    /// Short integer values take the fast path and are exact.
    #[kani::proof]
    #[kani::unwind(6)]
    fn verify_parse_prom_value_integers() {
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

    /// The scanner never panics on any 8-byte input.
    #[kani::proof]
    #[kani::unwind(10)]
    fn verify_scan_prom_text_no_panic() {
        let buf: [u8; 8] = kani::any();
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
