//! Compile only: copying by raw coordinates must not bypass an exclusive borrow.
#![forbid(unsafe_code)]
use c2_mem::{MemHandle, MemPool, PoolAllocation};

#[allow(dead_code)]
fn copy_during_write(pool: &MemPool, handle: &mut MemHandle, raw: PoolAllocation) {
    let writer = pool.handle_slice_mut(handle);
    let copy = pool
        .copy_data_at(
            raw.seg_idx,
            raw.generation,
            raw.offset,
            raw.actual_size,
            raw.is_dedicated,
        )
        .unwrap();
    std::hint::black_box((writer, copy));
}

fn main() {}
