//! R02 baseline: a rebuilt same-coordinate MemHandle yields a read and a
//! write alias over one live allocation. Compiled only, never run.
#![forbid(unsafe_code)]

use c2_mem::{MemHandle, MemPool, PoolConfig};

fn copy_of(source: &MemHandle) -> MemHandle {
    MemHandle {
        backing: source.backing,
    }
}

fn main() {
    let mut pool = MemPool::new(PoolConfig::default()).unwrap();
    let mut original = pool.alloc_handle(8192).expect("real allocation");
    let forged = copy_of(&original);
    let writer = pool.handle_slice_mut(&mut original);
    let reader = pool.handle_slice(&forged);
    writer[0] ^= reader[0];
}
