//! Wire-level protobuf primitives for OTLP projection decoding.
//!
//! Contains the low-level field iteration, varint decoding, and group
//! skipping logic that the higher-level OTLP decoder builds on.

use ffwd_arrow::columnar::plan::FieldHandle;
use wide::u8x16;

use super::ProjectionError;

#[derive(Clone, Copy)]
pub(super) enum WireField<'a> {
    Varint(u64),
    Fixed64(u64),
    Len(&'a [u8]),
    Fixed32(u32),
}

#[derive(Clone, Copy)]
pub(super) enum WireAny<'a> {
    String(&'a [u8]),
    Bool(bool),
    Int(i64),
    Double(f64),
    Bytes(&'a [u8]),
    ArrayRaw(&'a [u8]),
    KvListRaw(&'a [u8]),
}

#[derive(Clone, Copy)]
pub(super) enum StringStorage {
    Decoded,
    #[cfg(any(feature = "otlp-research", test))]
    InputView,
}

/// Skip a varint starting at the beginning of the slice.
/// Returns the number of bytes consumed.
pub(super) fn skip_varint_simd(buf: &[u8]) -> Result<usize, ProjectionError> {
    let mut pos = 0;
    while pos + 16 <= buf.len() {
        let chunk = u8x16::from(&buf[pos..pos + 16]);
        // A byte is terminal if its value is < 128 (high bit not set).
        let mask = chunk.simd_lt(u8x16::splat(128)).to_bitmask();
        if mask != 0 {
            return Ok(pos + (mask.trailing_zeros() as usize) + 1);
        }
        pos += 16;
    }
    // Fallback to scalar for tail.
    while pos < buf.len() {
        let b = buf[pos];
        pos += 1;
        if b < 128 {
            return Ok(pos);
        }
    }
    Err(ProjectionError::Invalid("truncated varint"))
}

/// Fast skip of a length-delimited field.
/// Returns the number of bytes consumed (varint len + payload len).
pub(super) fn skip_len_simd(input: &[u8]) -> Result<usize, ProjectionError> {
    let mut input_ptr = input;
    let len = read_varint(&mut input_ptr)? as usize;
    let varint_consumed = input.len() - input_ptr.len();
    if input_ptr.len() < len {
        return Err(ProjectionError::Invalid("truncated length-delimited field"));
    }
    Ok(varint_consumed + len)
}

/// Fast structural scan using GB/s SIMD to count LogRecords and Attributes.
///
/// Returns (log_record_count, total_attribute_count).
pub(super) fn count_structural_density_simd(input: &[u8]) -> (usize, usize) {
    // 0x12 = Tag 2 (log_records), Wire 2
    // 0x32 = Tag 6 (attributes), Wire 2
    let logs = memchr::memchr_iter(0x12, input).count();
    let attrs = memchr::memchr_iter(0x32, input).count();
    (logs, attrs)
}

/// Speculative warp decoder for standard OTLP LogRecords.
///
/// If the first 21+ bytes match the expected OTLP LogRecord "Skeleton"
/// (timestamp fixed64, observed_timestamp fixed64, severity_number varint),
/// it decodes them in a single SIMD pass and returns the number of bytes consumed.
pub(super) fn try_decode_log_record_warp(
    input: &[u8],
    out_ts: &mut Option<u64>,
    out_obs_ts: &mut Option<u64>,
    out_sev: &mut Option<u64>,
) -> Option<usize> {
    if input.len() < 21 {
        return None;
    }
    // Expected skeleton:
    // [0x09, TS(8)], [0x59, OBS_TS(8)], [0x10, SEV(1)]
    // Tag 0x09 = (1 << 3) | 1 (TimeUnixNano)
    // Tag 0x59 = (11 << 3) | 1 (ObservedTimeUnixNano)
    // Tag 0x10 = (2 << 3) | 0 (SeverityNumber)
    if input[0] == 0x09 && input[9] == 0x59 && input[18] == 0x10 {
        if input[19] < 128 {
            *out_ts = Some(u64::from_le_bytes(input[1..9].try_into().unwrap()));
            *out_obs_ts = Some(u64::from_le_bytes(input[10..18].try_into().unwrap()));
            *out_sev = Some(input[19] as u64);
            return Some(20);
        }
    }
    None
}

pub(super) fn for_each_field<'a>(
    mut input: &'a [u8],
    mut visit: impl FnMut(u32, WireField<'a>) -> Result<(), ProjectionError>,
) -> Result<(), ProjectionError> {
    while !input.is_empty() {
        let key = read_varint(&mut input)?;
        let field = decode_field_number(key)?;
        let wire_type = (key & 0x07) as u8;
        match wire_type {
            0 => visit(field, WireField::Varint(read_varint(&mut input)?))?,
            1 => {
                if input.len() < 8 {
                    return Err(ProjectionError::Invalid("truncated fixed64 field"));
                }
                let (bytes, rest) = input.split_at(8);
                input = rest;
                visit(
                    field,
                    WireField::Fixed64(u64::from_le_bytes(
                        bytes.try_into().expect("fixed64 slice has 8 bytes"),
                    )),
                )?;
            }
            2 => {
                let len = usize::try_from(read_varint(&mut input)?)
                    .map_err(|_e| ProjectionError::Invalid("protobuf length exceeds usize"))?;
                if input.len() < len {
                    return Err(ProjectionError::Invalid("truncated length-delimited field"));
                }
                let (bytes, rest) = input.split_at(len);
                input = rest;
                visit(field, WireField::Len(bytes))?;
            }
            5 => {
                if input.len() < 4 {
                    return Err(ProjectionError::Invalid("truncated fixed32 field"));
                }
                let (bytes, rest) = input.split_at(4);
                input = rest;
                visit(
                    field,
                    WireField::Fixed32(u32::from_le_bytes(
                        bytes.try_into().expect("fixed32 slice has 4 bytes"),
                    )),
                )?;
            }
            3 => skip_group(&mut input, field)?,
            4 => return Err(ProjectionError::Invalid("unexpected protobuf end group")),
            _ => return Err(ProjectionError::Invalid("invalid protobuf wire type")),
        }
    }
    Ok(())
}

fn decode_field_number(key: u64) -> Result<u32, ProjectionError> {
    const PROTOBUF_MAX_FIELD_NUMBER: u64 = 0x1FFF_FFFF;

    let field = key >> 3;
    if field == 0 {
        return Err(ProjectionError::Invalid("protobuf field number zero"));
    }
    if field > PROTOBUF_MAX_FIELD_NUMBER {
        return Err(ProjectionError::Invalid(
            "protobuf field number out of range",
        ));
    }
    u32::try_from(field).map_err(|_e| ProjectionError::Invalid("protobuf field number overflow"))
}

pub(super) const PROTOBUF_MAX_GROUP_DEPTH: usize = 64;

fn skip_group(input: &mut &[u8], start_field: u32) -> Result<(), ProjectionError> {
    let mut field_stack = [0u32; PROTOBUF_MAX_GROUP_DEPTH];
    let mut depth = 1usize;
    field_stack[0] = start_field;

    while !input.is_empty() {
        let key = read_varint(input)?;
        let field = decode_field_number(key)?;
        let wire_type = (key & 0x07) as u8;
        match wire_type {
            0 => {
                let _ = read_varint(input)?;
            }
            1 => {
                if input.len() < 8 {
                    return Err(ProjectionError::Invalid("truncated fixed64 field"));
                }
                *input = &input[8..];
            }
            2 => {
                let len = usize::try_from(read_varint(input)?)
                    .map_err(|_e| ProjectionError::Invalid("protobuf length exceeds usize"))?;
                if input.len() < len {
                    return Err(ProjectionError::Invalid("truncated length-delimited field"));
                }
                *input = &input[len..];
            }
            3 => {
                if depth == PROTOBUF_MAX_GROUP_DEPTH {
                    return Err(ProjectionError::Invalid("protobuf group nesting too deep"));
                }
                field_stack[depth] = field;
                depth += 1;
            }
            4 => {
                if field != field_stack[depth - 1] {
                    return Err(ProjectionError::Invalid("mismatched protobuf end group"));
                }
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            5 => {
                if input.len() < 4 {
                    return Err(ProjectionError::Invalid("truncated fixed32 field"));
                }
                *input = &input[4..];
            }
            _ => return Err(ProjectionError::Invalid("invalid protobuf wire type")),
        }
    }
    Err(ProjectionError::Invalid("unterminated protobuf group"))
}

#[inline(always)]
pub(super) fn read_varint(input: &mut &[u8]) -> Result<u64, ProjectionError> {
    let mut result = 0u64;
    for i in 0..10 {
        let (b, rest) = input
            .split_first()
            .ok_or(ProjectionError::Invalid("truncated varint"))?;
        *input = rest;
        result |= (*b as u64 & 0x7F) << (i * 7);
        if *b < 128 {
            return Ok(result);
        }
    }
    Err(ProjectionError::Invalid("varint overflow"))
}

pub(super) fn require_utf8(input: &[u8]) -> Result<&str, ProjectionError> {
    simdutf8::basic::from_utf8(input).map_err(|_e| ProjectionError::Invalid("invalid utf8"))
}

pub(super) fn validate_utf8(input: &[u8]) -> Result<&[u8], ProjectionError> {
    simdutf8::basic::from_utf8(input).map_err(|_e| ProjectionError::Invalid("invalid utf8"))?;
    Ok(input)
}

pub(super) fn subslice_range(parent: &[u8], child: &[u8]) -> (usize, usize) {
    let start = child.as_ptr() as usize - parent.as_ptr() as usize;
    (start, child.len())
}

#[derive(Default)]
pub(super) struct WireScratch {
    pub(super) decimal: Vec<u8>,
    pub(super) json: Vec<u8>,
    pub(super) resource_key: Vec<u8>,
    pub(super) attr_ranges: Vec<(usize, usize)>,
    pub(super) attr_field_cache: Vec<AttrFieldCache>,
}

#[derive(Default)]
pub(super) struct AttrFieldCache {
    pub(super) key: Vec<u8>,
    pub(super) handle: Option<FieldHandle>,
}
