//! Compile only: raw free must require authority even with a genuine owner.
#![forbid(unsafe_code)]
use c2_mem::{MemHandle, MemPool, PoolAllocation};

#[allow(dead_code)]
fn free_under_borrow(pool: &MemPool, handle: &mut MemHandle, raw: PoolAllocation) {
    let writer = pool.handle_slice_mut(handle);
    let allocator = pool.segment(raw.seg_idx as usize).unwrap().allocator();
    allocator
        .free(raw.offset, raw.level, raw.allocation_id)
        .unwrap();
    std::hint::black_box(writer);
}

fn main() {}
