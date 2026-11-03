//! Compile only: an admitted peer read must not reveal its raw OS mapping.
#![forbid(unsafe_code)]

use c2_mem::{MemPool, PeerMappingCache, PoolConfig};

fn main() {
    let mut owner = MemPool::new(PoolConfig::default()).unwrap();
    let mut handle = owner.alloc_shm_handle(8192).unwrap();
    let allocation = handle.shm_allocation().unwrap();
    let cache = PeerMappingCache::new(PoolConfig::default(), owner.prefix().into()).unwrap();
    let read = cache
        .admit(
            allocation.seg_idx as u16,
            allocation.generation,
            allocation.offset,
            8192,
            allocation.is_dedicated,
        )
        .unwrap();
    let writer = owner.handle_slice_mut(&mut handle);
    let mapping = &read.mapping;
    std::hint::black_box((writer, mapping));
}
