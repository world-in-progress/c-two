//! Compile only: the shared injected pool must not outlive ownership transfer.
#![forbid(unsafe_code)]

use c2_ipc::{ClientIpcConfig, SyncClient};
use c2_mem::{MemPool, PoolConfig};
use parking_lot::Mutex;
use std::sync::Arc;

fn main() {
    let pool = Arc::new(Mutex::new(MemPool::new(PoolConfig::default())));
    let client = SyncClient::connect(
        "ipc://r02-compile-only",
        Some(pool.clone()),
        ClientIpcConfig::default(),
    )
    .unwrap();
    let _ = client.pool_alloc_and_fill(4096, |destination| {
        pool.lock().destroy();
        destination[0] = 1;
        Ok(())
    });
}
