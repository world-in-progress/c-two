//! Compile only: copying a peer read during an owner write requires unsafe.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PeerMappingCache, PoolAllocation};

#[allow(dead_code)]
fn copy_during_write(
    pool: &MemPool,
    cache: &PeerMappingCache,
    handle: &mut MemHandle,
    raw: PoolAllocation,
) {
    let read = cache
        .admit(
            u16::try_from(raw.seg_idx).unwrap(),
            raw.generation,
            raw.offset,
            raw.actual_size,
            raw.is_dedicated,
        )
        .unwrap();
    let writer = pool.handle_slice_mut(handle);
    let copy = read.copy().unwrap();
    let mut destination = vec![0; read.len()];
    read.copy_into(&mut destination).unwrap();
    std::hint::black_box((writer, copy, destination));
}

fn main() {}
