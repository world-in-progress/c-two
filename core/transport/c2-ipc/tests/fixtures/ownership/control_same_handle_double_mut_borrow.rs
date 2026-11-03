//! Control: a second exclusive borrow of the *same* handle value is
//! rejected by Rust's own borrow rules today and after the migration.
#![forbid(unsafe_code)]

use c2_mem::{MemPool, PoolConfig};

fn main() {
    let mut pool = MemPool::new(PoolConfig::default()).unwrap();
    let mut handle = pool.alloc_handle(8192).expect("real allocation");
    let first = pool.handle_slice_mut(&mut handle);
    let second = pool.handle_slice_mut(&mut handle);
    first[0] = 1;
    second[0] = 2;
}
