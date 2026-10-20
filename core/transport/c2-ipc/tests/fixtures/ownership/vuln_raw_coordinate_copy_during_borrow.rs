//! R02 baseline: copy_data_at reads bare coordinates under a live exclusive
//! borrow; no rebuilt MemHandle is involved. Compiled only, never run.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn main() {
    let mut pool = MemPool::new(PoolConfig::default());
    let mut handle = pool.alloc_handle(8192).expect("real allocation");
    let (seg_idx, generation, offset, len) = match &handle {
        MemHandle::Buddy { seg_idx, generation, offset, len, .. } =>
            (u32::from(*seg_idx), *generation, *offset, *len as u32),
        _ => unreachable!("probe allocates from the buddy tier"),
    };
    let writer = pool.handle_slice_mut(&mut handle);
    let stolen = pool
        .copy_data_at(seg_idx, generation, offset, len, false)
        .expect("raw-coordinate read under live borrow");
    writer[0] ^= stolen[0];
}
