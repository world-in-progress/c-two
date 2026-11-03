//! Compile only: an admitted peer read cannot become a second mutable owner.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PeerRead};

#[allow(dead_code)]
fn write_during_borrow(pool: &MemPool, handle: &mut MemHandle, mut read: PeerRead) {
    let writer = pool.handle_slice_mut(handle);
    let peer_writer = pool.handle_slice_mut(&mut read);
    writer[0] = 1;
    peer_writer[0] = 2;
}

fn main() {}
