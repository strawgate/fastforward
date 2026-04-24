import subprocess
import re

def run_cmd(cmd):
    subprocess.run(cmd, shell=True, check=True)

# We are starting from a completely clean state.
run_cmd("git reset --hard HEAD")
run_cmd("git clean -fd")

# 1. logfwd-core json_scanner.rs
with open("crates/logfwd-core/src/json_scanner.rs", "r") as f:
    core = f.read()

core = core.replace(
'''pub fn scan_streaming<B: ScanBuilder>(buf: &[u8], config: &ScanConfig, builder: &mut B) {''',
'''pub fn scan_streaming<B: ScanBuilder>(buf: &[u8], config: &ScanConfig, builder: &mut B, scratch: &mut ScanScratch) {'''
)

core = core.replace(
'''    let mut real_quotes = alloc::vec::Vec::with_capacity(num_blocks);
    let mut open_brace = alloc::vec::Vec::with_capacity(num_blocks);
    let mut close_brace = alloc::vec::Vec::with_capacity(num_blocks);
    let mut open_bracket = alloc::vec::Vec::with_capacity(num_blocks);
    let mut close_bracket = alloc::vec::Vec::with_capacity(num_blocks);
    let mut line_ranges = alloc::vec::Vec::new();
    let mut line_start: usize = 0;''',
'''    scratch.clear();
    let mut line_start: usize = 0;'''
)

core = core.replace(
'''        real_quotes.push(processed.real_quotes);
        open_brace.push(processed.open_brace);
        close_brace.push(processed.close_brace);
        open_bracket.push(processed.open_bracket);
        close_bracket.push(processed.close_bracket);''',
'''        scratch.real_quotes.push(processed.real_quotes);
        scratch.open_brace.push(processed.open_brace);
        scratch.close_brace.push(processed.close_brace);
        scratch.open_bracket.push(processed.open_bracket);
        scratch.close_bracket.push(processed.close_bracket);'''
)

core = core.replace(
'''            if line_end > line_start {
                line_ranges.push((line_start, line_end));
            }''',
'''            if line_end > line_start {
                scratch.line_ranges.push((line_start, line_end));
            }'''
)

core = core.replace(
'''        if line_end > line_start {
            line_ranges.push((line_start, line_end));
        }''',
'''        if line_end > line_start {
            scratch.line_ranges.push((line_start, line_end));
        }'''
)

core = core.replace(
'''    let bitmasks = StoredBitmasks {
        real_quotes: &real_quotes,
        open_brace: &open_brace,
        close_brace: &close_brace,
        open_bracket: &open_bracket,
        close_bracket: &close_bracket,
    };''',
'''    let bitmasks = StoredBitmasks {
        real_quotes: &scratch.real_quotes,
        open_brace: &scratch.open_brace,
        close_brace: &scratch.close_brace,
        open_bracket: &scratch.open_bracket,
        close_bracket: &scratch.close_bracket,
    };'''
)

core = core.replace(
'''    for (start, end) in line_ranges {''',
'''    for &(start, end) in &scratch.line_ranges {'''
)

core = core.replace(
'''}

/// Decode a `\uXXXX` escape''',
'''}

/// Pre-allocated scratch buffers reused across batches.
#[derive(Debug)]
pub struct ScanScratch {
    /// Bitmask for real quotes.
    pub real_quotes: alloc::vec::Vec<u64>,
    /// Bitmask for open braces.
    pub open_brace: alloc::vec::Vec<u64>,
    /// Bitmask for close braces.
    pub close_brace: alloc::vec::Vec<u64>,
    /// Bitmask for open brackets.
    pub open_bracket: alloc::vec::Vec<u64>,
    /// Bitmask for close brackets.
    pub close_bracket: alloc::vec::Vec<u64>,
    /// Extracted line ranges within the block.
    pub line_ranges: alloc::vec::Vec<(usize, usize)>,
}

impl Default for ScanScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanScratch {
    /// Create a new ScanScratch with pre-allocated capacity.
    pub fn new() -> Self {
        ScanScratch {
            real_quotes: alloc::vec::Vec::with_capacity(32),
            open_brace: alloc::vec::Vec::with_capacity(32),
            close_brace: alloc::vec::Vec::with_capacity(32),
            open_bracket: alloc::vec::Vec::with_capacity(32),
            close_bracket: alloc::vec::Vec::with_capacity(32),
            line_ranges: alloc::vec::Vec::with_capacity(32),
        }
    }

    /// Clear all scratch buffers, preserving their capacity for the next batch.
    pub fn clear(&mut self) {
        self.real_quotes.clear();
        self.open_brace.clear();
        self.close_brace.clear();
        self.open_bracket.clear();
        self.close_bracket.clear();
        self.line_ranges.clear();
    }
}

/// Decode a `\uXXXX` escape'''
)

with open("crates/logfwd-core/src/json_scanner.rs", "w") as f:
    f.write(core)

run_cmd('''sed -i -e 's/scan_streaming(buf, &config, &mut builder)/scan_streaming(buf, \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(&buf, &config, &mut builder)/scan_streaming(\&buf, \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(input.as_bytes(), &config, &mut builder)/scan_streaming(input.as_bytes(), \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(buf.as_bytes(), &config, &mut builder)/scan_streaming(buf.as_bytes(), \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(lf_line.as_bytes(), &config, &mut lf_builder)/scan_streaming(lf_line.as_bytes(), \&config, \&mut lf_builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(crlf_line.as_bytes(), &config, &mut crlf_builder)/scan_streaming(crlf_line.as_bytes(), \&config, \&mut crlf_builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(lf_with_spaces.as_bytes(), &config, &mut lf_spaces_builder)/scan_streaming(lf_with_spaces.as_bytes(), \&config, \&mut lf_spaces_builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(json.as_bytes(), &config, &mut builder)/scan_streaming(json.as_bytes(), \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(int_json.as_bytes(), &config, &mut int_builder)/scan_streaming(int_json.as_bytes(), \&config, \&mut int_builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(float_json.as_bytes(), &config, &mut float_builder)/scan_streaming(float_json.as_bytes(), \&config, \&mut float_builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(input, &config, &mut builder)/scan_streaming(input, \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')
run_cmd('''sed -i -e 's/scan_streaming(line.as_bytes(), &config, &mut builder)/scan_streaming(line.as_bytes(), \&config, \&mut builder, \&mut ScanScratch::new())/g' crates/logfwd-core/src/json_scanner.rs''')

# 2. logfwd-core cri.rs
with open("crates/logfwd-core/src/cri.rs", "r") as f:
    cri = f.read()

cri = cri.replace(
'''pub fn json_escape_bytes(src: &[u8], dst: &mut Vec<u8>) {
    for &b in src {
        match b {
            b'"' => dst.extend_from_slice(b"\\\""),
            b'\\' => dst.extend_from_slice(b"\\\\"),
            0x08 => dst.extend_from_slice(b"\\b"),
            b'\t' => dst.extend_from_slice(b"\\t"),
            b'\n' => dst.extend_from_slice(b"\\n"),
            0x0C => dst.extend_from_slice(b"\\f"),
            b'\r' => dst.extend_from_slice(b"\\r"),
            0x00..=0x1F | 0x7F => {
                dst.extend_from_slice(b"\\u00");
                let hi = (b >> 4) & 0x0F;
                let lo = b & 0x0F;
                dst.push(if hi < 10 { b'0' + hi } else { b'a' + hi - 10 });
                dst.push(if lo < 10 { b'0' + lo } else { b'a' + lo - 10 });
            }
            _ => dst.push(b),
        }
    }
}''',
'''pub fn json_escape_bytes(src: &[u8], dst: &mut Vec<u8>) {
    let mut start = 0;
    for (i, &b) in src.iter().enumerate() {
        if matches!(b, b'"' | b'\\' | 0x00..=0x1F | 0x7F) {
            if i > start {
                dst.extend_from_slice(&src[start..i]);
            }
            start = i + 1;
            match b {
                b'"' => dst.extend_from_slice(b"\\\""),
                b'\\' => dst.extend_from_slice(b"\\\\"),
                0x08 => dst.extend_from_slice(b"\\b"),
                b'\t' => dst.extend_from_slice(b"\\t"),
                b'\n' => dst.extend_from_slice(b"\\n"),
                0x0C => dst.extend_from_slice(b"\\f"),
                b'\r' => dst.extend_from_slice(b"\\r"),
                0x00..=0x1F | 0x7F => {
                    dst.extend_from_slice(b"\\u00");
                    let hi = (b >> 4) & 0x0F;
                    let lo = b & 0x0F;
                    dst.push(if hi < 10 { b'0' + hi } else { b'a' + hi - 10 });
                    dst.push(if lo < 10 { b'0' + lo } else { b'a' + lo - 10 });
                }
                _ => unreachable!(),
            }
        }
    }
    if start < src.len() {
        dst.extend_from_slice(&src[start..]);
    }
}'''
)

with open("crates/logfwd-core/src/cri.rs", "w") as f:
    f.write(cri)

# 3. logfwd-arrow scanner.rs
with open("crates/logfwd-arrow/src/scanner.rs", "r") as f:
    sc = f.read()

sc = sc.replace(
'''    resource_attrs: Vec<(String, String)>,
}''',
'''    resource_attrs: Vec<(String, String)>,
    scratch: logfwd_core::json_scanner::ScanScratch,
}'''
)

sc = sc.replace(
'''            resource_attrs: Vec::new(),
        }''',
'''            resource_attrs: Vec::new(),
            scratch: logfwd_core::json_scanner::ScanScratch::new(),
        }'''
)

sc = sc.replace(
'''            resource_attrs,
        }''',
'''            resource_attrs,
            scratch: logfwd_core::json_scanner::ScanScratch::new(),
        }'''
)

sc = sc.replace(
'''scan_streaming(&buf, &self.config, &mut self.builder);''',
'''scan_streaming(&buf, &self.config, &mut self.builder, &mut self.scratch);'''
)

with open("crates/logfwd-arrow/src/scanner.rs", "w") as f:
    f.write(sc)


# 4. logfwd-arrow streaming_builder/mod.rs
with open("crates/logfwd-arrow/src/streaming_builder/mod.rs", "r") as f:
    mod = f.read()

# Change EmittedNameSet to HashMap
mod = mod.replace(
'''#[cfg(kani)]
type EmittedNameSet = std::collections::BTreeSet<String>;
#[cfg(not(kani))]
type EmittedNameSet = std::collections::HashSet<String>;''',
'''#[cfg(kani)]
type EmittedNameSet = std::collections::BTreeMap<String, usize>;
#[cfg(not(kani))]
type EmittedNameSet = HashMap<String, usize>;'''
)

mod = mod.replace(
'''    pub(crate) resource_attrs: Vec<(String, String)>,
}''',
'''    pub(crate) resource_attrs: Vec<(String, String)>,

    // Scratch buffers to avoid allocating vectors per column during finish_batch.
    pub(crate) scratch_i64: Vec<i64>,
    pub(crate) scratch_f64: Vec<f64>,
    pub(crate) scratch_valid: Vec<bool>,
    pub(crate) scratch_bool: Vec<bool>,

    pub(crate) emitted_names: EmittedNameSet,
    pub(crate) batch_index: usize,
}'''
)

mod = mod.replace(
'''            resource_attrs: Vec::new(),
        }''',
'''            resource_attrs: Vec::new(),
            scratch_i64: Vec::new(),
            scratch_f64: Vec::new(),
            scratch_valid: Vec::new(),
            scratch_bool: Vec::new(),
            emitted_names: new_emitted_name_set(),
            batch_index: 0,
        }'''
)

mod = mod.replace(
'''        // Discard all name→index mappings so resolve_field rebuilds them from
        // scratch.  Combined with resetting num_active, this prevents the
        // HashMap and the fields Vec from growing across batches with churning
        // field names (issue: field_index HashMap grows unboundedly).
        self.field_index.clear();
        self.num_active = 0;
        self.line_views.clear();''',
'''        self.batch_index = self.batch_index.wrapping_add(1);

        // Discard all name→index mappings so resolve_field rebuilds them from
        // scratch.  Combined with resetting num_active, this prevents the
        // HashMap and the fields Vec from growing across batches with churning
        // field names (issue: field_index HashMap grows unboundedly).
        self.field_index.clear();
        self.num_active = 0;
        self.line_views.clear();'''
)

with open("crates/logfwd-arrow/src/streaming_builder/mod.rs", "w") as f:
    f.write(mod)

# 5. logfwd-arrow streaming_builder/finish.rs
with open("crates/logfwd-arrow/src/streaming_builder/finish.rs", "r") as f:
    fin = f.read()

fin = fin.replace(
    "use arrow::buffer::{Buffer, NullBuffer};",
    "use arrow::buffer::{Buffer, NullBuffer, BooleanBuffer, ScalarBuffer};"
)

fin = fin.replace(
    "use super::{StreamingBuilder, append_string_view, new_emitted_name_set};",
    "use super::{StreamingBuilder, append_string_view};"
)

fin = fin.replace(
    "Some(Buffer::from(self.decoded_buf.clone()))",
    """let cap = self.decoded_buf.capacity();
            let buf = std::mem::replace(&mut self.decoded_buf, Vec::with_capacity(cap));
            Some(Buffer::from(buf))"""
)

fin = fin.replace(
'''        let mut emitted_names = new_emitted_name_set();
        if let Some(line_field_name) = self.line_field_name.as_ref() {
            emitted_names.insert(line_field_name.clone());
        }
        let mut reserve_name = |name: &str| -> Result<(), ArrowError> {
            if emitted_names.insert(name.to_string()) {
                Ok(())
            } else {
                Err(ArrowError::InvalidArgumentError(format!(
                    "duplicate output column name: {name}"
                )))
            }
        };''',
'''        let batch_idx = self.batch_index;
        if let Some(line_field_name) = self.line_field_name.as_ref() {
            self.emitted_names.insert(line_field_name.clone(), batch_idx);
        }'''
)

fin = fin.replace(
'''reserve_name(name.as_ref())?;''',
'''if self.emitted_names.insert(name.to_string(), batch_idx) == Some(batch_idx) {
                    return Err(ArrowError::InvalidArgumentError(format!(
                        "duplicate output column name: {name}"
                    )));
                }'''
)

fin = fin.replace(
'''            reserve_name(&col_name)?;''',
'''            if self.emitted_names.insert(col_name.clone(), batch_idx) == Some(batch_idx) {
                return Err(ArrowError::InvalidArgumentError(format!(
                    "duplicate output column name: {col_name}"
                )));
            }'''
)

def fix_int(t):
    return re.sub(
        r'let mut values = vec!\[0i64; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.int_values \{\s+let r = row as usize;\s+if r < num_rows \{\s+values\[r\] = v;\s+valid\[r\] = true;\s+\}\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = Int64Array::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_i64.clear();
                    self.scratch_i64.resize(num_rows, 0i64);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.int_values {
                        let r = row as usize;
                        if r < num_rows {
                            self.scratch_i64[r] = v;
                            self.scratch_valid[r] = true;
                        }
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = Int64Array::new(ScalarBuffer::new(Buffer::from_slice_ref(&self.scratch_i64), 0, num_rows), Some(nulls));''',
        t
    )

def fix_float(t):
    return re.sub(
        r'let mut values = vec!\[0\.0f64; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.float_values \{\s+let r = row as usize;\s+if r < num_rows \{\s+values\[r\] = v;\s+valid\[r\] = true;\s+\}\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = Float64Array::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_f64.clear();
                    self.scratch_f64.resize(num_rows, 0.0f64);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.float_values {
                        let r = row as usize;
                        if r < num_rows {
                            self.scratch_f64[r] = v;
                            self.scratch_valid[r] = true;
                        }
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = Float64Array::new(ScalarBuffer::new(Buffer::from_slice_ref(&self.scratch_f64), 0, num_rows), Some(nulls));''',
        t
    )

def fix_bool(t):
    return re.sub(
        r'let mut values = vec!\[false; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.bool_values \{\s+let r = row as usize;\s+if r < num_rows \{\s+values\[r\] = v;\s+valid\[r\] = true;\s+\}\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = BooleanArray::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_bool.clear();
                    self.scratch_bool.resize(num_rows, false);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.bool_values {
                        let r = row as usize;
                        if r < num_rows {
                            self.scratch_bool[r] = v;
                            self.scratch_valid[r] = true;
                        }
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = BooleanArray::new(BooleanBuffer::from_iter(self.scratch_bool.iter().copied()), Some(nulls));''',
        t
    )

def fix_detached_int(t):
    return re.sub(
        r'let mut values = vec!\[0i64; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.int_values \{\s+let row = row as usize;\s+if row >= num_rows \{\s+continue;\s+\}\s+values\[row\] = v;\s+valid\[row\] = true;\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = Int64Array::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_i64.clear();
                    self.scratch_i64.resize(num_rows, 0i64);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.int_values {
                        let r = row as usize;
                        if r >= num_rows {
                            continue;
                        }
                        self.scratch_i64[r] = v;
                        self.scratch_valid[r] = true;
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = Int64Array::new(ScalarBuffer::new(Buffer::from_slice_ref(&self.scratch_i64), 0, num_rows), Some(nulls));''',
        t
    )

def fix_detached_float(t):
    return re.sub(
        r'let mut values = vec!\[0\.0f64; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.float_values \{\s+let row = row as usize;\s+if row >= num_rows \{\s+continue;\s+\}\s+values\[row\] = v;\s+valid\[row\] = true;\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = Float64Array::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_f64.clear();
                    self.scratch_f64.resize(num_rows, 0.0f64);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.float_values {
                        let r = row as usize;
                        if r >= num_rows {
                            continue;
                        }
                        self.scratch_f64[r] = v;
                        self.scratch_valid[r] = true;
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = Float64Array::new(ScalarBuffer::new(Buffer::from_slice_ref(&self.scratch_f64), 0, num_rows), Some(nulls));''',
        t
    )

def fix_detached_bool(t):
    return re.sub(
        r'let mut values = vec!\[false; num_rows\];\s+let mut valid = vec!\[false; num_rows\];\s+for &\([^)]+\) in &fc\.bool_values \{\s+let row = row as usize;\s+if row >= num_rows \{\s+continue;\s+\}\s+values\[row\] = v;\s+valid\[row\] = true;\s+\}\s+let nulls = NullBuffer::from\(valid\);\s+let array = BooleanArray::new\(values\.into\(\), Some\(nulls\)\);',
        r'''self.scratch_bool.clear();
                    self.scratch_bool.resize(num_rows, false);
                    self.scratch_valid.clear();
                    self.scratch_valid.resize(num_rows, false);
                    for &(row, v) in &fc.bool_values {
                        let r = row as usize;
                        if r >= num_rows {
                            continue;
                        }
                        self.scratch_bool[r] = v;
                        self.scratch_valid[r] = true;
                    }
                    let nulls = NullBuffer::new(BooleanBuffer::from_iter(self.scratch_valid.iter().copied()));
                    let array = BooleanArray::new(BooleanBuffer::from_iter(self.scratch_bool.iter().copied()), Some(nulls));''',
        t
    )

fin = fix_int(fin)
fin = fix_float(fin)
fin = fix_bool(fin)
fin = fix_detached_int(fin)
fin = fix_detached_float(fin)
fin = fix_detached_bool(fin)

with open("crates/logfwd-arrow/src/streaming_builder/finish.rs", "w") as f:
    f.write(fin)


# Update benchmark to pass compiling!
with open("crates/logfwd-bench/benches/full_chain.rs", "r") as f:
    bench = f.read()

bench = bench.replace(
    '''logfwd_output::write_row_json(&result, row, &cols, &mut buf)''',
    '''logfwd_output::write_row_json(&result, row, &cols, &mut buf, true)'''
)

with open("crates/logfwd-bench/benches/full_chain.rs", "w") as f:
    f.write(bench)
