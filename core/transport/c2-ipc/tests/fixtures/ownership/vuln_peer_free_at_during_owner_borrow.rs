//! Compile only: a peer free must not bypass a genuine owner's live borrow.
#![forbid(unsafe_code)]
use c2_mem::{MemHandle, MemPool, PoolAllocation};

#[allow(dead_code)]
fn peer_free(owner: &MemPool, peer: &mut MemPool, handle: &mut MemHandle, raw: PoolAllocation) {
    let writer = owner.handle_slice_mut(handle);
    peer.free_at(
        raw.seg_idx,
        raw.generation,
        raw.offset,
        raw.actual_size,
        raw.is_dedicated,
        raw.allocation_id,
    )
    .unwrap();
    std::hint::black_box(writer);
}

fn main() {}
