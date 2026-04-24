use arrow::array::Int64Array;
use arrow::buffer::{Buffer, ScalarBuffer, NullBuffer, BooleanBuffer};

fn main() {
    let mut scratch_i64: Vec<i64> = vec![1, 2, 3];
    let mut scratch_valid: Vec<bool> = vec![true, false, true];

    let buffer = Buffer::from_slice_ref(&scratch_i64);
    let scalar_buffer = ScalarBuffer::new(buffer, 0, 3);

    let boolean_buffer: BooleanBuffer = scratch_valid.iter().copied().collect();
    let null_buffer = NullBuffer::new(boolean_buffer);

    let array = Int64Array::new(scalar_buffer, Some(null_buffer));
}
