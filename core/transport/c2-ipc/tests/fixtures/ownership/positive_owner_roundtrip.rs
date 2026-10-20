//! Legal owner round-trip: must compile before and after the R02 migration.
#![forbid(unsafe_code)]

use c2_mem::{MemPool, PoolConfig};

fn main() {
    let mut pool = MemPool::new(PoolConfig::default());
    let mut handle = pool.alloc_handle(8192).expect("real allocation");
    pool.handle_slice_mut(&mut handle)[0] = 0xAB;
    assert_eq!(pool.handle_slice(&handle)[0], 0xAB);
    let mut other = pool.alloc_handle(8192).expect("independent allocation");
    let first = pool.handle_slice_mut(&mut handle);
    let second = pool.handle_slice_mut(&mut other);
    first[0] = 0xCD;
    second[0] = first[0];
    assert_eq!(pool.handle_slice(&other)[0], 0xCD);
    let _ = pool.release_handle(other);
    let _ = pool.release_handle(handle);
    assert_eq!(pool.stats().alloc_count, 0);
}
