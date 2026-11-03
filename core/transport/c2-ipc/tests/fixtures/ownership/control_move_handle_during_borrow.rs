//! Control: moving the *same* handle into the release authority while its
//! slice is live is rejected by Rust's move/borrow rules today and after
//! the migration.
#![forbid(unsafe_code)]

use c2_mem::{MemPool, PoolConfig};

fn main() {
    let mut pool = MemPool::new(PoolConfig::default()).unwrap();
    let mut handle = pool.alloc_handle(8192).expect("real allocation");
    let writer = pool.handle_slice_mut(&mut handle);
    let release = pool.release_handle(handle);
    writer[0] = 1;
    let _ = release;
}
