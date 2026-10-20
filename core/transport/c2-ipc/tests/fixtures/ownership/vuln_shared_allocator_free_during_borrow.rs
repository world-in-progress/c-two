//! R02 baseline: the public segment allocator frees the block under a live
//! exclusive borrow; no rebuilt MemHandle is used for the free. Never run.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn main() {
    let mut pool = MemPool::new(PoolConfig::default());
    let mut handle = pool.alloc_handle(8192).expect("real allocation");
    let (seg_idx, offset, allocation_size, allocation_id) = match &handle {
        MemHandle::Buddy { seg_idx, offset, allocation_size, allocation_id, .. } =>
            (*seg_idx, *offset, *allocation_size, *allocation_id),
        _ => unreachable!("probe allocates from the buddy tier"),
    };
    let writer = pool.handle_slice_mut(&mut handle);
    let allocator = pool.segment(usize::from(seg_idx)).expect("mapped backing").allocator();
    let level = allocator.size_to_level(allocation_size as usize).expect("buddy level");
    allocator.free(offset, level as u16, allocation_id).expect("free under live borrow");
    writer[0] = 1;
}
