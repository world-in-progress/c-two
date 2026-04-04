//! Sharded chunk registry.
//!
//! [`ChunkRegistry`] is a thread-safe, sharded manager for in-flight chunked
//! reassemblies. It wraps [`ChunkAssembler`] instances with tracking metadata
//! and provides `insert` / `feed` / `finish` / `abort` lifecycle operations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;
use c2_wire::assembler::ChunkAssembler;
use tracing::warn;

use crate::config::ChunkConfig;
use crate::promote::promote_to_shm;

const SHARD_COUNT: usize = 16;

/// Statistics returned by [`ChunkRegistry::gc_sweep`].
#[derive(Debug, Default)]
pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}

/// Per-assembly tracking wrapper around [`ChunkAssembler`].
struct TrackedAssembler {
    inner: ChunkAssembler,
    #[allow(dead_code)]
    created_at: Instant,
    last_activity: Instant,
    total_bytes: u64,
}

/// Sharded chunk reassembly lifecycle manager.
pub struct ChunkRegistry {
    shards: [Mutex<HashMap<(u64, u64), TrackedAssembler>>; SHARD_COUNT],
    pool: Arc<RwLock<MemPool>>,
    config: ChunkConfig,
    active_count: AtomicUsize,
    total_bytes: AtomicU64,
}

impl ChunkRegistry {
    /// Create a new registry backed by `pool` with the given `config`.
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            pool,
            config,
            active_count: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
        }
    }

    fn shard(&self, conn_id: u64) -> &Mutex<HashMap<(u64, u64), TrackedAssembler>> {
        &self.shards[conn_id as usize % SHARD_COUNT]
    }

    /// Number of in-flight assemblies.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Total bytes allocated for in-flight assemblies.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Borrow the chunk configuration.
    pub fn config(&self) -> &ChunkConfig {
        &self.config
    }

    /// Borrow the shared pool.
    pub fn pool(&self) -> &Arc<RwLock<MemPool>> {
        &self.pool
    }

    /// Begin a new chunked reassembly for `(conn_id, request_id)`.
    pub fn insert(
        &self,
        conn_id: u64,
        request_id: u64,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<(), String> {
        // Soft-limit check: GC sweep first, then warn but never reject.
        if self.active_count() >= self.config.soft_limit as usize
            || self.total_bytes() >= self.config.max_reassembly_bytes
        {
            let stats = self.gc_sweep();
            warn!(
                expired = stats.expired,
                remaining = stats.remaining,
                freed_bytes = stats.freed_bytes,
                "chunk registry soft limit reached, swept expired entries"
            );
        }

        // Allocate the reassembly buffer (needs write lock on pool).
        let assembler = {
            let mut pool = self.pool.write().unwrap();
            ChunkAssembler::new(
                &mut pool,
                total_chunks,
                chunk_size,
                self.config.max_chunks_per_request,
                self.config.max_bytes_per_request,
            )?
        };

        let alloc_bytes = (total_chunks * chunk_size) as u64;
        let now = Instant::now();
        let tracked = TrackedAssembler {
            inner: assembler,
            created_at: now,
            last_activity: now,
            total_bytes: alloc_bytes,
        };

        // Lock shard and insert.
        let mut shard = self.shard(conn_id).lock().unwrap();
        shard.insert((conn_id, request_id), tracked);
        drop(shard);

        self.active_count.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_add(alloc_bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Feed a data chunk into an existing assembly.
    ///
    /// Returns `Ok(true)` when all chunks have been received.
    pub fn feed(
        &self,
        conn_id: u64,
        request_id: u64,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String> {
        let mut shard = self.shard(conn_id).lock().unwrap();
        let tracked = shard
            .get_mut(&(conn_id, request_id))
            .ok_or_else(|| format!("no assembly for ({conn_id}, {request_id})"))?;

        let pool = self.pool.read().unwrap();
        let complete = tracked.inner.feed_chunk(&pool, chunk_idx, data)?;
        drop(pool);

        tracked.last_activity = Instant::now();
        Ok(complete)
    }

    /// Finish a completed assembly and return the reassembled [`MemHandle`].
    ///
    /// The entry is removed from the registry regardless of success or failure.
    /// On any error path after `take_handle()`, the handle is released back to
    /// the pool to prevent leaks.
    pub fn finish(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Result<MemHandle, String> {
        // Remove from shard and decrement counters first.
        let tracked = {
            let mut shard = self.shard(conn_id).lock().unwrap();
            let t = shard
                .remove(&(conn_id, request_id))
                .ok_or_else(|| format!("no assembly for ({conn_id}, {request_id})"))?;
            self.active_count.fetch_sub(1, Ordering::Relaxed);
            self.total_bytes.fetch_sub(t.total_bytes, Ordering::Relaxed);
            t
        };

        let mut inner = tracked.inner;

        // Extract handle — must release on any error after this point.
        let mut handle = match inner.take_handle() {
            Some(h) => h,
            None => return Err("assembler handle already taken".into()),
        };

        if !inner.is_complete() {
            // Incomplete: release handle and report error.
            self.pool.write().unwrap().release_handle(handle);
            return Err(format!(
                "incomplete assembly for ({conn_id}, {request_id})"
            ));
        }

        // Try SHM promotion if this is a FileSpill.
        if handle.is_file_spill() {
            let mut pool = self.pool.write().unwrap();
            match promote_to_shm(&mut pool, handle) {
                Ok(shm_handle) => handle = shm_handle,
                Err(original) => handle = original,
            }
        }

        Ok(handle)
    }

    /// Abort an in-flight assembly, releasing all resources.
    pub fn abort(&self, conn_id: u64, request_id: u64) {
        let mut shard = self.shard(conn_id).lock().unwrap();
        if let Some(tracked) = shard.remove(&(conn_id, request_id)) {
            drop(shard);
            self.active_count.fetch_sub(1, Ordering::Relaxed);
            self.total_bytes
                .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
            tracked
                .inner
                .abort(&mut self.pool.write().unwrap());
        }
    }

    /// Sweep expired assemblies across all shards.
    pub fn gc_sweep(&self) -> GcStats {
        let mut stats = GcStats::default();
        let timeout = self.config.assembler_timeout;
        let now = Instant::now();

        for shard_mutex in &self.shards {
            let mut shard = shard_mutex.lock().unwrap();
            let expired_keys: Vec<(u64, u64)> = shard
                .iter()
                .filter(|(_, t)| now.duration_since(t.last_activity) >= timeout)
                .map(|(k, _)| *k)
                .collect();

            for key in expired_keys {
                if let Some(tracked) = shard.remove(&key) {
                    stats.expired += 1;
                    stats.freed_bytes += tracked.total_bytes;
                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                    self.total_bytes
                        .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
                    tracked
                        .inner
                        .abort(&mut self.pool.write().unwrap());
                }
            }
            stats.remaining += shard.len();
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;
    use std::sync::atomic::{AtomicU32, Ordering as TestOrdering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn test_pool() -> Arc<RwLock<MemPool>> {
        let id = TEST_ID.fetch_add(1, TestOrdering::Relaxed);
        let prefix = format!("/cc3g{:04x}{:04x}", std::process::id() as u16, id);
        Arc::new(RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 2,
                dedicated_crash_timeout_secs: 0.0,
                spill_threshold: 1.0,
                spill_dir: std::env::temp_dir().join("c2_reg_test"),
            },
            prefix,
        )))
    }

    #[test]
    fn insert_feed_finish_happy_path() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let data = b"hello world!";
        reg.insert(1, 100, 1, data.len()).unwrap();
        assert_eq!(reg.active_count(), 1);
        let complete = reg.feed(1, 100, 0, data).unwrap();
        assert!(complete);
        let handle = reg.finish(1, 100).unwrap();
        assert_eq!(handle.len(), data.len());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        pool.write().unwrap().release_handle(handle);
    }

    #[test]
    fn abort_frees_resources() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 200, 3, 1024).unwrap();
        assert_eq!(reg.active_count(), 1);
        reg.abort(1, 200);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
    }

    #[test]
    fn finish_error_path_does_not_leak() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 300, 3, 1024).unwrap();
        // Feed only 1 of 3 chunks — finish should fail
        reg.feed(1, 300, 0, &[0u8; 1024]).unwrap();
        let result = reg.finish(1, 300);
        assert!(result.is_err());
        // Resources must still be freed
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
    }

    #[test]
    fn multi_chunk_out_of_order() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let chunk_size = 128;
        reg.insert(1, 400, 4, chunk_size).unwrap();
        // Feed out of order: 3, 1, 0, 2
        assert!(!reg.feed(1, 400, 3, &[3u8; 64]).unwrap());
        assert!(!reg.feed(1, 400, 1, &[1u8; 128]).unwrap());
        assert!(!reg.feed(1, 400, 0, &[0u8; 128]).unwrap());
        assert!(reg.feed(1, 400, 2, &[2u8; 128]).unwrap());
        let handle = reg.finish(1, 400).unwrap();
        // Verify data integrity
        let p = pool.read().unwrap();
        let slice = p.handle_slice(&handle);
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..384], &[2u8; 128]);
        assert_eq!(&slice[384..384 + 64], &[3u8; 64]);
        drop(p);
        pool.write().unwrap().release_handle(handle);
    }
}
