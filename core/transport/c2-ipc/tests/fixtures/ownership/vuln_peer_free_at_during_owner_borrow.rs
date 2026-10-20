//! R02 baseline: a peer pool's free_at releases the owner's block under the
//! owner's live borrow; distinct objects share no borrow state. Never run.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn main() {
    let mut owner = MemPool::new(PoolConfig::default());
    let mut handle = owner.alloc_handle(8192).expect("real allocation");
    let (seg_idx, generation, offset, allocation_size, allocation_id) = match &handle {
        MemHandle::Buddy { seg_idx, generation, offset, allocation_size, allocation_id, .. } =>
            (u32::from(*seg_idx), *generation, *offset, *allocation_size, *allocation_id),
        _ => unreachable!("probe allocates from the buddy tier"),
    };
    let mut peer = MemPool::open_peer(PoolConfig::default(), owner.prefix().to_string());
    peer.ensure_peer_segment(seg_idx, generation, 8192).expect("second mapping");
    let writer = owner.handle_slice_mut(&mut handle);
    let freed = peer.free_at(seg_idx, generation, offset, allocation_size, false, allocation_id);
    writer[0] = 1;
    let _ = freed;
}
