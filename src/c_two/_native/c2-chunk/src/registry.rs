//! Sharded chunk registry.
//! Full implementation in Task 6-7.

use crate::config::ChunkConfig;
use c2_mem::MemPool;
use std::sync::{Arc, RwLock};

/// Statistics returned by [`ChunkRegistry::gc_sweep`].
#[derive(Debug, Default)]
pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}

/// Sharded chunk reassembly lifecycle manager.
pub struct ChunkRegistry {
    _pool: Arc<RwLock<MemPool>>,
    _config: ChunkConfig,
}

impl ChunkRegistry {
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self {
        Self { _pool: pool, _config: config }
    }
}
