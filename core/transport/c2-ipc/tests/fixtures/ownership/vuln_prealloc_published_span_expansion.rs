//! Compile only: publication must use the block's original requested length.
#![forbid(unsafe_code)]

use c2_ipc::{RouteBinding, SyncClient};

fn enlarge(client: &SyncClient, binding: &RouteBinding) {
    let block = client.pool_alloc_and_write(&[1; 4096]).unwrap();
    let _ = client.call_bound_prealloc(binding, "echo", &block, 8192);
}

fn main() {}
