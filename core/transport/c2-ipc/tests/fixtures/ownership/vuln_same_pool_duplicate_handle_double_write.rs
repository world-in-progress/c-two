//! R02 baseline: a rebuilt same-coordinate MemHandle yields two exclusive
//! slices over one live allocation. Compiled only, never run.
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
    let mut forged = copy_of(&original);
    let left = pool.handle_slice_mut(&mut original);
    let right = pool.handle_slice_mut(&mut forged);
    left[0] = 1;
    right[0] = 2;
}
