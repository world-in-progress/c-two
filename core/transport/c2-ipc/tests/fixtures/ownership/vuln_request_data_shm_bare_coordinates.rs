//! R02 baseline: RequestData::Shm hand-built from bare coordinates drives a
//! safe RequestLease copy/release. Compiled only, never run.
#![forbid(unsafe_code)]

use std::sync::Arc;

use c2_mem::{MemPool, PoolConfig};
use c2_server::{RequestData, RequestLease};

fn main() {
    let pool = Arc::new(parking_lot::RwLock::new(MemPool::new(PoolConfig::default())));
    let mut lease = RequestLease::new(RequestData::Shm {
        pool: Arc::clone(&pool),
        seg_idx: 0,
        generation: 1,
        offset: 0,
        data_size: 4096,
        is_dedicated: false,
        allocation_id: 7,
    });
    let _ = lease.copy_bytes();
    let _ = lease.release();
}
