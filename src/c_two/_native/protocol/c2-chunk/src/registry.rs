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

/// Result of a successful [`ChunkRegistry::finish`] call.
pub struct FinishedChunk {
    pub handle: MemHandle,
    pub route_name: Option<String>,
    pub method_idx: Option<u16>,
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

        // Lock shard and insert — reject duplicates to prevent silent leaks.
        let mut shard = self.shard(conn_id).lock().unwrap();
        if shard.contains_key(&(conn_id, request_id)) {
            // Abort the newly allocated assembler before returning error.
            tracked
                .inner
                .abort(&mut self.pool.write().unwrap());
            return Err(format!(
                "duplicate assembly for ({conn_id}, {request_id})"
            ));
        }
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

    /// Store route metadata on an in-flight assembly (server-side use).
    pub fn set_route_info(
        &self,
        conn_id: u64,
        request_id: u64,
        route_name: String,
        method_idx: u16,
    ) {
        let mut shard = self.shard(conn_id).lock().unwrap();
        if let Some(tracked) = shard.get_mut(&(conn_id, request_id)) {
            tracked.inner.route_name = Some(route_name);
            tracked.inner.method_idx = Some(method_idx);
        }
    }

    /// Finish a completed assembly and return the reassembled data.
    ///
    /// The entry is removed from the registry regardless of success or failure.
    /// On any error path after `take_handle()`, the handle is released back to
    /// the pool to prevent leaks.
    pub fn finish(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Result<FinishedChunk, String> {
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

        // Trim logical length to actual written data (last chunk may be short).
        handle.set_len(inner.written_end());

        // Try SHM promotion if this is a FileSpill.
        if handle.is_file_spill() {
            let mut pool = self.pool.write().unwrap();
            match promote_to_shm(&mut pool, handle) {
                Ok(shm_handle) => handle = shm_handle,
                Err(original) => handle = original,
            }
        }

        Ok(FinishedChunk {
            handle,
            route_name: inner.route_name,
            method_idx: inner.method_idx,
        })
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

    /// Remove all in-flight assemblies for a specific connection.
    ///
    /// Called when a connection disconnects to prevent orphaned assemblies.
    /// Only accesses the single shard for `conn_id` (O(shard_size) scan).
    pub fn cleanup_connection(&self, conn_id: u64) {
        let mut shard = self.shard(conn_id).lock().unwrap();
        let conn_keys: Vec<(u64, u64)> = shard
            .keys()
            .filter(|(cid, _)| *cid == conn_id)
            .copied()
            .collect();

        for key in conn_keys {
            if let Some(tracked) = shard.remove(&key) {
                self.active_count.fetch_sub(1, Ordering::Relaxed);
                self.total_bytes
                    .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
                tracked
                    .inner
                    .abort(&mut self.pool.write().unwrap());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;
    use std::sync::atomic::{AtomicU32, Ordering as TestOrdering};
    use std::time::Duration;

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
        let finished = reg.finish(1, 100).unwrap();
        assert_eq!(finished.handle.len(), data.len());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        pool.write().unwrap().release_handle(finished.handle);
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
        let finished = reg.finish(1, 400).unwrap();
        // Logical length must be trimmed to actual data (last chunk is 64, not 128).
        assert_eq!(finished.handle.len(), 128 * 3 + 64); // 448, not 512
        // Verify data integrity
        let p = pool.read().unwrap();
        let slice = p.handle_slice(&finished.handle);
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..384], &[2u8; 128]);
        assert_eq!(&slice[384..384 + 64], &[3u8; 64]);
        drop(p);
        pool.write().unwrap().release_handle(finished.handle);
    }

    #[test]
    fn duplicate_insert_rejected() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 500, 2, 64).unwrap();
        assert_eq!(reg.active_count(), 1);
        // Second insert for same key must fail.
        let err = reg.insert(1, 500, 3, 128).unwrap_err();
        assert!(err.contains("duplicate"), "expected duplicate error: {err}");
        // Original entry still intact, counter unchanged.
        assert_eq!(reg.active_count(), 1);
        // Cleanup.
        reg.abort(1, 500);
    }

    #[test]
    fn gc_sweep_expires_stale() {
        let pool = test_pool();
        let cfg = ChunkConfig {
            assembler_timeout: Duration::from_millis(50),
            ..ChunkConfig::default()
        };
        let reg = ChunkRegistry::new(pool.clone(), cfg);
        reg.insert(1, 1, 1, 1024).unwrap();
        assert_eq!(reg.active_count(), 1);
        std::thread::sleep(Duration::from_millis(80));
        let stats = reg.gc_sweep();
        assert_eq!(stats.expired, 1);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
    }

    #[test]
    fn feed_updates_last_activity() {
        let pool = test_pool();
        let cfg = ChunkConfig {
            assembler_timeout: Duration::from_millis(100),
            ..ChunkConfig::default()
        };
        let reg = ChunkRegistry::new(pool.clone(), cfg);
        reg.insert(1, 1, 3, 1024).unwrap();
        // Wait 60ms, feed a chunk — resets timer.
        std::thread::sleep(Duration::from_millis(60));
        reg.feed(1, 1, 0, &[0u8; 1024]).unwrap();
        // Wait another 60ms (120ms from insert, but only 60ms from last feed).
        std::thread::sleep(Duration::from_millis(60));
        let stats = reg.gc_sweep();
        assert_eq!(stats.expired, 0);
        assert_eq!(reg.active_count(), 1);
        // Cleanup.
        reg.abort(1, 1);
    }

    #[test]
    fn soft_limit_triggers_gc() {
        let pool = test_pool();
        let cfg = ChunkConfig {
            soft_limit: 2,
            assembler_timeout: Duration::from_millis(10),
            ..ChunkConfig::default()
        };
        let reg = ChunkRegistry::new(pool.clone(), cfg);
        reg.insert(1, 1, 1, 64).unwrap();
        reg.insert(1, 2, 1, 64).unwrap();
        assert_eq!(reg.active_count(), 2);
        // Let both expire.
        std::thread::sleep(Duration::from_millis(30));
        // This insert exceeds soft_limit (2) → triggers gc_sweep internally.
        reg.insert(1, 3, 1, 64).unwrap();
        // GC should have cleaned up the 2 expired ones; only the new one remains.
        assert!(reg.active_count() <= 2);
        // Cleanup remaining.
        reg.abort(1, 3);
    }

    #[test]
    fn cleanup_connection_removes_only_target() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 100, 1, 1024).unwrap();
        reg.insert(1, 200, 1, 1024).unwrap();
        reg.insert(2, 100, 1, 1024).unwrap();
        assert_eq!(reg.active_count(), 3);
        // Cleanup conn 1 — should remove (1,100) and (1,200).
        reg.cleanup_connection(1);
        assert_eq!(reg.active_count(), 1);
        // Conn 2's assembly should survive.
        let complete = reg.feed(2, 100, 0, &[42u8; 1024]).unwrap();
        assert!(complete);
        let finished = reg.finish(2, 100).unwrap();
        pool.write().unwrap().release_handle(finished.handle);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn sharding_isolates_connections() {
        let pool = test_pool();
        let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
        let mut handles = vec![];
        for conn_id in 0u64..8 {
            let reg = reg.clone();
            let pool = pool.clone();
            let h = std::thread::spawn(move || {
                for req_id in 0u64..10 {
                    reg.insert(conn_id, req_id, 1, 64).unwrap();
                    reg.feed(conn_id, req_id, 0, &[conn_id as u8; 64]).unwrap();
                    let finished = reg.finish(conn_id, req_id).unwrap();
                    pool.write().unwrap().release_handle(finished.handle);
                }
            });
            handles.push(h);
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
    }

    #[test]
    fn concurrent_same_conn_no_corruption() {
        let pool = test_pool();
        let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
        let mut handles = vec![];
        let conn_id = 42u64;
        for thread_idx in 0u64..4 {
            let reg = reg.clone();
            let pool = pool.clone();
            let h = std::thread::spawn(move || {
                for i in 0u64..20 {
                    let req_id = thread_idx * 1000 + i;
                    reg.insert(conn_id, req_id, 1, 64).unwrap();
                    reg.feed(conn_id, req_id, 0, &[0u8; 64]).unwrap();
                    let finished = reg.finish(conn_id, req_id).unwrap();
                    pool.write().unwrap().release_handle(finished.handle);
                }
            });
            handles.push(h);
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
    }
}