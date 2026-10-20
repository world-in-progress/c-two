//! R02 baseline: a second peer mapping of the owner backing yields two
//! exclusive slices from two distinct MemPool objects. Compiled only.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn copy_of(source: &MemHandle) -> MemHandle {
    MemHandle {
        backing: source.backing,
    }
}

fn main() {
    let mut owner = MemPool::new(PoolConfig::default());
    let mut owner_handle = owner.alloc_handle(8192).expect("real allocation");
    let mut peer_handle = copy_of(&owner_handle);
    let allocation = owner_handle.shm_allocation().unwrap();
    let (seg_idx, generation) = (allocation.seg_idx, allocation.generation);
    let mut peer = MemPool::open_peer(PoolConfig::default(), owner.prefix().to_string());
    peer.ensure_peer_segment(seg_idx, generation, 8192)
        .expect("second mapping");
    let owner_writer = owner.handle_slice_mut(&mut owner_handle);
    let peer_writer = peer.handle_slice_mut(&mut peer_handle);
    owner_writer[0] = 1;
    peer_writer[0] = 2;
}
