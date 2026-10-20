//! R02 baseline: a second peer mapping of the owner backing yields two
//! exclusive slices from two distinct MemPool objects. Compiled only.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn copy_of(source: &MemHandle) -> MemHandle {
    match source {
        MemHandle::Buddy { seg_idx, generation, allocation_id, offset, allocation_size, len } =>
            MemHandle::Buddy { seg_idx: *seg_idx, generation: *generation, allocation_id: *allocation_id, offset: *offset, allocation_size: *allocation_size, len: *len },
        _ => unreachable!("probe allocates from the buddy tier"),
    }
}

fn main() {
    let mut owner = MemPool::new(PoolConfig::default());
    let mut owner_handle = owner.alloc_handle(8192).expect("real allocation");
    let mut peer_handle = copy_of(&owner_handle);
    let (seg_idx, generation) = match &peer_handle {
        MemHandle::Buddy { seg_idx, generation, .. } => (u32::from(*seg_idx), *generation),
        _ => unreachable!("probe allocates from the buddy tier"),
    };
    let mut peer = MemPool::open_peer(PoolConfig::default(), owner.prefix().to_string());
    peer.ensure_peer_segment(seg_idx, generation, 8192).expect("second mapping");
    let owner_writer = owner.handle_slice_mut(&mut owner_handle);
    let peer_writer = peer.handle_slice_mut(&mut peer_handle);
    owner_writer[0] = 1;
    peer_writer[0] = 2;
}
