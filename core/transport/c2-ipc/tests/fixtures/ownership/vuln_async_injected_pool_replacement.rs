//! Compile only: an async client must own the pool it advertises.
#![forbid(unsafe_code)]

use c2_ipc::{ClientIpcConfig, IpcClient};
use c2_mem::{MemPool, PoolConfig};
use parking_lot::Mutex;
use std::sync::Arc;

fn main() {
    let pool = Arc::new(Mutex::new(MemPool::new(PoolConfig::default())));
    let _client = IpcClient::with_pool(
        "ipc://r02-compile-only",
        pool.clone(),
        ClientIpcConfig::default(),
    );
    let mut guard = pool.lock();
    let replacement = MemPool::new(guard.config().clone());
    let _advertised_pool = std::mem::replace(&mut *guard, replacement);
}
