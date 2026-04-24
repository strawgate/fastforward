## Plan
1. **Reduce bitmask `Vec` allocations in `json_scanner.rs`**:
    - The issue mentions "5 bitmask Vecs + line_ranges — use caller-held scratch buffers".
    - `scan_streaming` allocates `real_quotes`, `open_brace`, `close_brace`, `open_bracket`, `close_bracket`, and `line_ranges` on every call.
    - We should move these out into a reusable struct or use an existing one that is held across batches, like adding to `Scanner` or creating a `ScanScratch` passed from `Scanner`. Wait, `scan_streaming` takes a `B: ScanBuilder`.
    - Wait, does `ScanBuilder` have a method for getting scratch buffers? No. But wait! We can add a struct `ScannerScratch` (or similar) that is held by `Scanner` and passed to `scan_streaming`. Wait, `scan_streaming` is a standalone function. We can either pass a new scratch parameter to it, or store it in `ScanBuilder`, or create a `Scanner` type in core. But `Scanner` is in `logfwd-arrow`. So we change `scan_streaming` signature to take an explicit scratch buffer, and `Scanner` will own it.

2. **Pre-size `line_ranges` `Vec`**:
    - "Vec::new() with no capacity -> Pre-size based on buffer length"
    - The `line_ranges` should be pre-sized if not already using a scratch buffer. Since we'll use a scratch buffer, we will `.clear()` it, but initially we should give it a decent capacity. Or pre-size based on `num_blocks`.

3. **`json_escape_bytes` byte-by-byte push**:
    - "memchr-based bulk copy of unescaped runs".
    - Update `json_escape_bytes` in `crates/logfwd-core/src/cri.rs` to use `memchr` (or similar logic) to copy runs of unescaped characters using `extend_from_slice`, instead of pushing byte-by-byte.

4. **`decoded_buf.clone()` in `finish_batch`**:
    - "Use `std::mem::take` + capacity preservation"
    - In `crates/logfwd-arrow/src/streaming_builder/finish.rs`, `let decoded_arrow_buf = if self.decoded_buf.is_empty() { None } else { Some(Buffer::from(self.decoded_buf.clone())) };`
    - Wait, `Buffer::from(std::mem::take(&mut self.decoded_buf))` might clear the vec, losing capacity. Better to use `std::mem::replace` with a new `Vec` of the same capacity, or similar. `Vec::clone()` clones all elements and the capacity. If we use `std::mem::take(&mut self.decoded_buf)`, the new vec has 0 capacity. We can then do `self.decoded_buf = Vec::with_capacity(cap)`.

5. **`vec![0i64; num_rows]` per column per batch**:
    - "Reusable scratch buffers in StreamingBuilder".
    - In `finish.rs` it does `let mut values = vec![0i64; num_rows];` and `let mut valid = vec![false; num_rows];` per column.
    - We can add scratch vecs to `StreamingBuilder` or `FieldColumns` or reuse some buffer. Or maybe one reusable `values_i64` scratch buffer in `StreamingBuilder` that we `clear()` and resize. But wait, `Int64Array::new` takes a `ScalarBuffer<i64>` (which takes a `Vec`). If we pass it, we give away ownership of the `Vec`. So how do we avoid allocation? If we give away the `Vec` to Arrow, we *must* allocate a new one. Wait, the issue says: "Reusable scratch buffers in StreamingBuilder". Wait, does `Int64Array::new` take `Vec` and keep it? Yes. We can't reuse it if we give it to Arrow. BUT wait. `arrow::buffer::BufferBuilder` or `Vec::with_capacity`? Oh, the issue says "vec![0i64; num_rows] per column per batch".
    - Ah, it says "use caller-held scratch buffers" for the scanner, but for `StreamingBuilder` it says "Reusable scratch buffers in StreamingBuilder" for the `field_index` HashMap and the `vec![0i64; num_rows]`? No, for `vec![...]` maybe we can't reuse because Arrow takes ownership? Arrow `Buffer` takes ownership. But wait! The issue specifically states: `vec![0i64; num_rows]` per column per batch -> "Reusable scratch buffers in StreamingBuilder". Maybe we don't *pre-fill* it with `vec![0i64; num_rows]`? Wait, `vec![0; num_rows]` allocates and fills. If we do `let mut values = Vec::with_capacity(num_rows); values.resize(num_rows, 0);` it's the same. Wait, if we use Arrow's builders? `arrow::array::Int64Builder`. Arrow builders have a `values_slice` or we can just append. But `Int64Builder` handles nulls better. Let's look closer at `finish.rs`.

6. **`field_index` HashMap cleared + rebuilt per batch**:
    - "Keep across batches, only evict stale entries".
    - Currently `begin_batch` calls `self.field_index.clear()`.
    - We can instead keep it and only evict fields that are no longer active, or just keep all of them across batches (up to some limit), or use a generation counter? Wait, if we keep `field_index`, it maps `name` to `idx`. The `fields` vector also is kept across batches but `num_active` is reset to `0`. If we don't reset `num_active` to 0 but instead keep the fields and append new ones, the `fields` vector grows. But `begin_batch` clears `num_active`. Wait, if we keep `num_active` and just clear the data arrays (`str_views`, `int_values`, etc.) inside each field, we can reuse the field indices! So `self.fields.iter_mut().for_each(|f| f.clear());` and we don't clear `field_index` and don't reset `num_active` to 0. But we might need a way to avoid unbounded growth if new field names keep coming. "Keep across batches, only evict stale entries" implies we should evict.

7. **`new_emitted_name_set()` + `name.to_string()` per field per batch**:
    - "Reusable set with clear()".
    - We can add `emitted_names: EmittedNameSet` to `StreamingBuilder`, and in `finish.rs` just `self.emitted_names.clear();` instead of creating a new one. `name.to_string()` can also be replaced, wait, `name` is a `&str`, we can use a hash set of `&str` or similar? Wait, `HashSet<String>` requires `String`. Maybe we can make it `HashSet<usize>` storing the field index, or `HashSet<&'a str>`? Wait, we can't easily store `&'a str` across batches. But for *within* a single batch, `emitted_names` is just used to detect duplicate names. We can clear it at the beginning of `finish_batch`, and since it's only used for one batch, we can use `HashSet<&str>` or keep `HashSet<String>` but `clear()` it and reuse the capacity. "Reusable set with clear()"

Let's do a deep dive to refine the plan.
