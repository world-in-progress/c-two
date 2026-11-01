//! Compile only: importing a backing must not be a safe owner-rebinding operation.
#![forbid(unsafe_code)]
use c2_mem::{MemHandle, MemPool};
fn rebind(pool: &mut MemPool, old: &MemHandle, index: u32, name: &str) {
    pool.open_dedicated_at(index, 1, name, 131072).unwrap();
    let _ = pool.handle_slice(old);
}
fn main() {}
