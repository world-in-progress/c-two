//! Sharded chunk registry.
//!
//! [`ChunkRegistry`] is a thread-safe, sharded manager for in-flight chunked
//! reassemblies. It wraps [`ChunkAssembler`] instances with tracking metadata
//! and provides `insert` / `feed` / `finish` / `abort` lifecycle operations.
//!
//! Admission is finite: `insert` reserves the checked assembly capacity from
//! the pool's canonical reassembly budget before any storage is allocated,
//! and the resulting charge stays live — through `finish` (including its
//! logical trim), queueing as a request/response backing, and held results —
//! until the [`ReassemblyBacking`] carrier actually releases the storage.

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use crate::assembler::ChunkAssembler;
use crate::chunk::backing::ReassemblyBacking;
use c2_mem::MemPool;
use tracing::warn;

use crate::chunk::config::ChunkConfig;

const SHARD_COUNT: usize = 16;

/// Statistics returned by [`ChunkRegistry::gc_sweep`].
#[derive(Debug, Default)]
pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}

/// Result of a successful [`ChunkRegistry::finish`] call.
///
/// `backing` is the single ownership carrier for the reassembled storage: it
/// keeps the pool, the handle, and the reassembly budget charge together
/// until it is released or dropped.
pub struct FinishedChunk {
    pub backing: ReassemblyBacking,
    pub route_name: Option<String>,
    pub method_idx: Option<u16>,
}

/// Identity of one admitted assembly generation.
///
/// The registry admits at most one assembly per `(conn_id, request_id)` key at
/// a time, but a key can be admitted again after its predecessor finished,
/// aborted, or timed out. The generation makes an entry addressable: teardown
/// code that only holds the coordinates of the generation it published can
/// release exactly that generation through [`ChunkRegistry::abort_id`] and
/// never a successor's same-key entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkAssemblyId {
    conn_id: u64,
    request_id: u64,
    generation: u64,
}

impl ChunkAssemblyId {
    /// Connection that owns this assembly.
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Request id this assembly was admitted for.
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// Monotonic per-registry admission generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Per-assembly tracking wrapper around [`ChunkAssembler`].
struct TrackedAssembler {
    inner: ChunkAssembler,
    generation: u64,
    _created_at: Instant,
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
    generations: AtomicU64,
}

impl ChunkRegistry {
    /// Create a new registry backed by `pool` with the given `config`.
    ///
    /// The pool's [`MemPool::budget`] reassembly cell is the canonical
    /// admission authority for this registry.
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            pool,
            config,
            active_count: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
            generations: AtomicU64::new(0),
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

    /// Whether an assembly exists for `(conn_id, request_id)`.
    pub fn contains(&self, conn_id: u64, request_id: u64) -> bool {
        self.shard(conn_id)
            .lock()
            .contains_key(&(conn_id, request_id))
    }

    /// Begin a new chunked reassembly for `(conn_id, request_id)`.
    ///
    /// Admission order: soft-limit GC sweep (advisory only), per-message
    /// limit and geometry checks, reassembly budget reservation for the
    /// checked `total_chunks × chunk_size` capacity, then allocation. A
    /// budget rejection returns the cell/size error and creates no mapping.
    ///
    /// On success the returned [`ChunkAssemblyId`] identifies this exact
    /// admission generation for identity-checked teardown
    /// ([`ChunkRegistry::abort_id`]).
    pub fn insert(
        &self,
        conn_id: u64,
        request_id: u64,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<ChunkAssemblyId, String> {
        // Soft-limit check: GC sweep first, then warn but never reject. The
        // hard admission bound is the reassembly budget cell.
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

        // Reserve + allocate inside the assembler constructor. Geometry,
        // per-message limits, and budget capacity all reject before any
        // mapping is created.
        let assembler = ChunkAssembler::new(
            self.pool.clone(),
            total_chunks,
            chunk_size,
            self.config.max_chunks_per_request,
            self.config.max_bytes_per_request,
        )?;
        let alloc_bytes = assembler.capacity_bytes();
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);

        let now = Instant::now();
        let tracked = TrackedAssembler {
            inner: assembler,
            generation,
            _created_at: now,
            last_activity: now,
            total_bytes: alloc_bytes,
        };

        // Lock shard, reject duplicates, publish the entry, and move the
        // counters together with it — all under the shard lock so no observer
        // sees an entry without its counters or vice versa.
        let mut shard = self.shard(conn_id).lock();
        if shard.contains_key(&(conn_id, request_id)) {
            drop(shard);
            // Drop the newly admitted assembler: storage is released through
            // the pool authority and the charge refunds exactly once.
            drop(tracked);
            return Err(format!("duplicate assembly for ({conn_id}, {request_id})"));
        }
        shard.insert((conn_id, request_id), tracked);
        self.active_count.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_add(alloc_bytes, Ordering::Relaxed);
        drop(shard);
        Ok(ChunkAssemblyId {
            conn_id,
            request_id,
            generation,
        })
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
        let mut shard = self.shard(conn_id).lock();
        let tracked = shard
            .get_mut(&(conn_id, request_id))
            .ok_or_else(|| format!("no assembly for ({conn_id}, {request_id})"))?;

        let complete = tracked.inner.feed_chunk(chunk_idx, data)?;
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
        let mut shard = self.shard(conn_id).lock();
        if let Some(tracked) = shard.get_mut(&(conn_id, request_id)) {
            tracked.inner.route_name = Some(route_name);
            tracked.inner.method_idx = Some(method_idx);
        }
    }

    /// Finish a completed assembly and return its owned backing carrier.
    ///
    /// The entry is removed from the registry regardless of success or
    /// failure. On success the returned [`ReassemblyBacking`] keeps the full
    /// capacity charged until it is released; on failure (including an
    /// incomplete assembly) the backing storage is released through the pool
    /// authority and the charge refunds exactly once.
    ///
    /// A file-spill backing is returned as-is. Consumers accept every
    /// [`c2_mem::MemHandle`] variant through the carrier, so completed
    /// reassembly storage stays where the memory-pressure decision put it
    /// instead of being copied into SHM.
    pub fn finish(&self, conn_id: u64, request_id: u64) -> Result<FinishedChunk, String> {
        // Remove from shard and move the counters while still holding the
        // shard lock; the pool work below happens after the lock is gone.
        let tracked = {
            let mut shard = self.shard(conn_id).lock();
            let Some(tracked) = shard.remove(&(conn_id, request_id)) else {
                return Err(format!("no assembly for ({conn_id}, {request_id})"));
            };
            self.active_count.fetch_sub(1, Ordering::Relaxed);
            self.total_bytes
                .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
            tracked
        };

        let mut inner = tracked.inner;
        let route_name = inner.route_name.take();
        let method_idx = inner.method_idx.take();
        // finish() releases storage and refunds the charge on its own error
        // paths; the registry counters above describe in-flight entries only
        // and intentionally stop here while the budget charge stays live.
        let backing = inner.finish()?;
        Ok(FinishedChunk {
            backing,
            route_name,
            method_idx,
        })
    }

    /// Abort an in-flight assembly, releasing all resources.
    ///
    /// Unconditional by key: this is the connection-teardown / timeout cleanup
    /// verb, used where the caller cannot or must not keep a generation
    /// identity. Use [`ChunkRegistry::abort_id`] for rollback of one exact
    /// admission generation.
    pub fn abort(&self, conn_id: u64, request_id: u64) {
        let removed = {
            let mut shard = self.shard(conn_id).lock();
            match shard.remove(&(conn_id, request_id)) {
                Some(tracked) => {
                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                    self.total_bytes
                        .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
                    Some(tracked)
                }
                None => None,
            }
        };
        // Backing release and charge refund happen on drop, outside the shard
        // lock.
        drop(removed);
    }

    /// Abort exactly the admission generation named by `id`.
    ///
    /// Identity-checked rollback: if the key has since been re-admitted (a
    /// replacement generation with its own charge), this is a no-op for that
    /// replacement. Storage release and charge refund happen on drop, outside
    /// the shard lock.
    pub fn abort_id(&self, id: ChunkAssemblyId) {
        let removed = {
            let mut shard = self.shard(id.conn_id).lock();
            let key = (id.conn_id, id.request_id);
            let matches = shard
                .get(&key)
                .is_some_and(|tracked| tracked.generation == id.generation);
            if !matches {
                None
            } else {
                shard.remove(&key).inspect(|tracked| {
                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                    self.total_bytes
                        .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
                })
            }
        };
        // Backing release and charge refund happen on drop, outside the shard
        // lock.
        drop(removed);
    }

    /// Sweep expired assemblies across all shards.
    pub fn gc_sweep(&self) -> GcStats {
        let mut stats = GcStats::default();
        let timeout = self.config.assembler_timeout;
        let now = Instant::now();

        for shard_mutex in &self.shards {
            let mut removed = Vec::new();
            let remaining;
            {
                let mut shard = shard_mutex.lock();
                let expired_keys: Vec<(u64, u64)> = shard
                    .iter()
                    .filter(|(_, tracked)| now.duration_since(tracked.last_activity) >= timeout)
                    .map(|(key, _)| *key)
                    .collect();
                for key in expired_keys {
                    if let Some(tracked) = shard.remove(&key) {
                        // Counters move with the entry removal, under the
                        // same shard lock.
                        self.active_count.fetch_sub(1, Ordering::Relaxed);
                        self.total_bytes
                            .fetch_sub(tracked.total_bytes, Ordering::Relaxed);
                        stats.expired += 1;
                        stats.freed_bytes += tracked.total_bytes;
                        removed.push(tracked);
                    }
                }
                remaining = shard.len();
            }
            stats.remaining += remaining;
            // Backing release and charge refund happen on drop, outside the
            // shard lock.
            drop(removed);
        }
        stats
    }

    /// Remove all in-flight assemblies for a specific connection.
    ///
    /// Called when a connection disconnects to prevent orphaned assemblies.
    /// Only accesses the single shard for `conn_id` (O(shard_size) scan).
    pub fn cleanup_connection(&self, conn_id: u64) {
        let mut removed = Vec::new();
        {
            let mut shard = self.shard(conn_id).lock();
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
                    removed.push(tracked);
                }
            }
        }
        // Backing release and charge refund happen on drop, outside the shard
        // lock.
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::MemoryBudget;
    use c2_mem::budget::BudgetKind;
    use c2_mem::config::PoolConfig;
    use std::sync::atomic::{AtomicU32, Ordering as TestOrdering};
    use std::thread;
    use std::time::Duration;

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn unique_prefix(tag: &str) -> String {
        let id = TEST_ID.fetch_add(1, TestOrdering::Relaxed);
        format!("/cc3g{:04x}{:04x}{}", std::process::id() as u16, id, tag)
    }

    fn base_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_reg_test"),
            ..PoolConfig::default()
        }
    }

    fn test_pool() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(MemPool::new_with_prefix(
            base_config(),
            unique_prefix(""),
        )))
    }

    fn budgeted_pool(reassembly_limit: u64) -> (Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        (
            Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
                base_config(),
                unique_prefix("b"),
                budget.clone(),
            ))),
            budget,
        )
    }

    fn reassembly_used(budget: &MemoryBudget) -> u64 {
        budget.snapshot().cell(BudgetKind::Reassembly).used_bytes
    }

    #[test]
    fn insert_feed_finish_happy_path() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let data = b"hello world!";
        reg.insert(1, 100, 1, data.len()).unwrap();
        assert!(reg.contains(1, 100));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), data.len() as u64);
        let complete = reg.feed(1, 100, 0, data).unwrap();
        assert!(complete);
        let mut finished = reg.finish(1, 100).unwrap();
        assert!(!reg.contains(1, 100));
        assert_eq!(finished.backing.len(), data.len());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(finished.backing.copy_bytes().unwrap(), data);
        finished.backing.release().unwrap();
    }

    #[test]
    fn abort_frees_resources_and_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 200, 3, 1024).unwrap();
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 3 * 1024);
        reg.abort(1, 200);
        // Idempotent: a missing entry aborts nothing.
        reg.abort(1, 200);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn abort_id_releases_only_its_own_generation() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        // First generation is admitted, then removed; a replacement generation
        // is admitted under the same key with its own charge.
        let first = reg.insert(1, 250, 1, 256).unwrap();
        reg.abort(1, 250);
        let replacement = reg.insert(1, 250, 2, 256).unwrap();
        assert_ne!(first.generation(), replacement.generation());
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 512);

        // A stale generation's rollback must not touch the replacement.
        reg.abort_id(first);
        assert!(reg.contains(1, 250));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 512);

        // The matching generation releases exactly its own charge.
        reg.abort_id(replacement);
        assert!(!reg.contains(1, 250));
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        // Repeated identity abort stays a no-op.
        reg.abort_id(replacement);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn finish_error_path_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 300, 3, 1024).unwrap();
        // Feed only 1 of 3 chunks — finish should fail and refund once.
        reg.feed(1, 300, 0, &[0u8; 1024]).unwrap();
        let result = reg.finish(1, 300);
        assert!(result.is_err());
        // Resources and charge freed exactly once; the pool stays usable.
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        reg.insert(1, 301, 1, 1024).unwrap();
        reg.abort(1, 301);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn duplicate_insert_rejects_and_returns_new_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 500, 2, 64).unwrap();
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 128);
        // Second insert for same key must fail and keep exactly one charge.
        let err = reg.insert(1, 500, 3, 128).unwrap_err();
        assert!(err.contains("duplicate"), "expected duplicate error: {err}");
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 128);
        // Original entry still intact and feedable to completion.
        assert!(!reg.feed(1, 500, 0, &[7u8; 64]).unwrap());
        assert!(reg.feed(1, 500, 1, &[6u8; 32]).unwrap());
        // Cleanup.
        reg.abort(1, 500);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn invalid_chunk_feed_aborts_entry_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 510, 2, 64).unwrap();
        assert_eq!(reassembly_used(&budget), 128);
        // Duplicate chunk index is an invalid chunk: caller aborts.
        reg.feed(1, 510, 0, &[1u8; 64]).unwrap();
        assert!(reg.feed(1, 510, 0, &[2u8; 64]).is_err());
        reg.abort(1, 510);
        assert_eq!(reassembly_used(&budget), 0);
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
        let mut finished = reg.finish(1, 400).unwrap();
        // Logical length must be trimmed to actual data (last chunk is 64, not 128).
        assert_eq!(finished.backing.len(), 128 * 3 + 64); // 448, not 512
        // Verify data integrity
        let slice = finished.backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..384], &[2u8; 128]);
        assert_eq!(&slice[384..384 + 64], &[3u8; 64]);
        finished.backing.release().unwrap();
    }

    #[test]
    fn gc_sweep_expires_stale_and_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let cfg = ChunkConfig {
            assembler_timeout: Duration::from_millis(50),
            ..ChunkConfig::default()
        };
        let reg = ChunkRegistry::new(pool, cfg);
        reg.insert(1, 1, 1, 1024).unwrap();
        assert_eq!(reassembly_used(&budget), 1024);
        std::thread::sleep(Duration::from_millis(80));
        let stats = reg.gc_sweep();
        assert_eq!(stats.expired, 1);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn disconnect_cleanup_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 100, 1, 1024).unwrap();
        reg.insert(1, 200, 2, 512).unwrap();
        reg.insert(2, 100, 1, 256).unwrap();
        assert_eq!(reassembly_used(&budget), 1024 + 1024 + 256);
        reg.cleanup_connection(1);
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 256);
        // Conn 2's assembly survives and completes.
        let complete = reg.feed(2, 100, 0, &[42u8; 256]).unwrap();
        assert!(complete);
        let mut finished = reg.finish(2, 100).unwrap();
        finished.backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);
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
    fn finished_and_trimmed_backing_stays_charged() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 600, 4, 128).unwrap();
        assert!(!reg.feed(1, 600, 0, &[0u8; 128]).unwrap());
        assert!(!reg.feed(1, 600, 1, &[1u8; 128]).unwrap());
        assert!(!reg.feed(1, 600, 2, &[2u8; 128]).unwrap());
        assert!(reg.feed(1, 600, 3, &[3u8; 64]).unwrap());
        let finished = reg.finish(1, 600).unwrap();
        // Registry-level accounting dropped at finish…
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        // …but the budget charge for the full capacity stays live through the
        // trim and until the carrier is released.
        assert_eq!(reassembly_used(&budget), 4 * 128);
        assert_eq!(finished.backing.len(), 3 * 128 + 64);
        drop(finished);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn held_backing_survives_registry_and_pool_handle_drop() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let registry_pool_view = pool.clone();
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 610, 2, 256).unwrap();
        reg.set_route_info(1, 610, "grid".into(), 3);
        assert!(!reg.feed(1, 610, 0, &[9u8; 256]).unwrap());
        assert!(reg.feed(1, 610, 1, &[8u8; 128]).unwrap());
        let finished = reg.finish(1, 610).unwrap();
        assert_eq!(finished.route_name.as_deref(), Some("grid"));
        assert_eq!(finished.method_idx, Some(3));

        // Registry and the outer pool Arc handle are gone (client/registry
        // shutdown). The carrier keeps the pool alive, the content readable,
        // and the charge held.
        drop(reg);
        drop(registry_pool_view);
        assert_eq!(reassembly_used(&budget), 512);
        let mut backing = finished.backing;
        assert_eq!(backing.len(), 384);
        assert_eq!(&backing.copy_bytes().unwrap()[0..256], &[9u8; 256]);
        assert_eq!(&backing.copy_bytes().unwrap()[256..384], &[8u8; 128]);

        backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn admission_rejects_before_mapping_with_cell_and_size_error() {
        let (pool, budget) = budgeted_pool(256);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        // 3 × 128 = 384 > 256: rejected with the cell and size named.
        let err = reg.insert(1, 620, 3, 128).unwrap_err();
        assert!(err.contains("'reassembly'"), "error must name the cell: {err}");
        assert!(err.contains("384"), "error must name the requested size: {err}");
        assert!(err.contains("256"), "error must name the limit: {err}");
        assert_eq!(reg.active_count(), 0);
        // No mapping was created and only rejection statistics moved.
        let stats = pool.read().stats();
        assert_eq!(stats.total_segments, 0);
        assert_eq!(stats.dedicated_segments, 0);
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert_eq!(snap.reassembly.rejected_allocations, 1);
        assert_eq!(snap.reassembly.rejected_bytes, 384);

        // Zero geometry is rejected without any budget interaction.
        let err = reg.insert(1, 621, 1, 0).unwrap_err();
        assert!(err.contains("chunk_size must be > 0"));
        let err = reg.insert(1, 622, 0, 128).unwrap_err();
        assert!(err.contains("total_chunks must be > 0"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);

        // After one admitted assembly fills the cell, a second is rejected.
        reg.insert(1, 623, 2, 128).unwrap();
        assert_eq!(reassembly_used(&budget), 256);
        let err = reg.insert(1, 624, 1, 128).unwrap_err();
        assert!(err.contains("'reassembly'"));
        assert_eq!(reg.active_count(), 1);
        reg.abort(1, 623);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn shared_budget_bounds_independent_registries() {
        // Two registries over two pools sharing one MemoryBudget: concurrent
        // admissions cannot exceed the shared live bytes.
        let budget = MemoryBudget::new(1 << 20, 1 << 20, 4 * 256);
        let mk_registry = |tag: char| {
            let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
                base_config(),
                unique_prefix(&tag.to_string()),
                budget.clone(),
            )));
            ChunkRegistry::new(pool, ChunkConfig::default())
        };
        let reg_a = mk_registry('a');
        let reg_b = mk_registry('b');

        // Exactly four 256-byte assemblies fit; the fifth is rejected no
        // matter which registry tries.
        reg_a.insert(1, 1, 2, 128).unwrap(); // 256
        reg_b.insert(2, 1, 2, 128).unwrap(); // 512
        reg_a.insert(1, 2, 2, 128).unwrap(); // 768
        reg_b.insert(2, 2, 2, 128).unwrap(); // 1024 = limit
        assert_eq!(reassembly_used(&budget), 1024);
        assert!(reg_a.insert(1, 3, 1, 1).is_err());
        assert!(reg_b.insert(2, 3, 1, 1).is_err());
        // Releasing one admission frees room for exactly one more.
        reg_a.abort(1, 1);
        assert_eq!(reassembly_used(&budget), 768);
        reg_b.insert(2, 3, 2, 128).unwrap();
        assert_eq!(reassembly_used(&budget), 1024);
        reg_a.abort(1, 2);
        reg_a.abort(1, 2);
        reg_b.abort(2, 1);
        reg_b.abort(2, 2);
        reg_b.abort(2, 3);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn concurrent_admissions_never_exceed_shared_live_bytes() {
        use std::sync::Barrier;

        const SEATS: usize = 4; // concurrent 512-byte assemblies that fit
        const ASSEMBLY_BYTES: usize = 512;
        const LIMIT: u64 = SEATS as u64 * ASSEMBLY_BYTES as u64;

        let budget = MemoryBudget::new(1 << 20, 1 << 20, LIMIT);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            PoolConfig {
                segment_size: 256 * 1024,
                min_block_size: 4096,
                max_segments: 4,
                max_dedicated_segments: 8,
                dedicated_crash_timeout_secs: 0.0,
                buddy_idle_decay_secs: 0.0,
                spill_threshold: 1.0,
                spill_dir: std::env::temp_dir().join("c2_reg_test"),
                ..PoolConfig::default()
            },
            unique_prefix("c"),
            budget.clone(),
        )));
        let reg = Arc::new(ChunkRegistry::new(pool, ChunkConfig::default()));

        // Phase 1: SEATS threads race to admit one assembly each and HOLD
        // it at a barrier. Exactly SEATS must succeed because the shared
        // cell fits exactly SEATS × 512 bytes.
        let admit_barrier = Arc::new(Barrier::new(SEATS));
        let mut handles = Vec::new();
        for t in 0..SEATS {
            let reg = reg.clone();
            let budget = budget.clone();
            let barrier = admit_barrier.clone();
            handles.push(thread::spawn(move || {
                reg.insert(t as u64, 1, 4, ASSEMBLY_BYTES / 4)
                    .expect("exactly the seats must fit");
                // The shared live-byte bound holds at every observation.
                assert!(reassembly_used(&budget) <= LIMIT);
                barrier.wait();
                t as u64
            }));
        }
        let held: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(reassembly_used(&budget), LIMIT);
        assert_eq!(reg.active_count(), SEATS);

        // Phase 2: with every byte charged and held, further concurrent
        // admissions must be rejected atomically with the budget cell named.
        let reject_barrier = Arc::new(Barrier::new(SEATS));
        let mut handles = Vec::new();
        for t in 0..SEATS {
            let reg = reg.clone();
            let barrier = reject_barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let err = reg
                    .insert((t + 100) as u64, 2, 4, ASSEMBLY_BYTES / 4)
                    .expect_err("a fully charged cell must reject");
                assert!(err.contains("'reassembly'"), "cell named: {err}");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reassembly_used(&budget), LIMIT);
        assert_eq!(reg.active_count(), SEATS);
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.rejected_allocations, SEATS as u64);
        assert_eq!(snap.reassembly.rejected_bytes, SEATS as u64 * ASSEMBLY_BYTES as u64);
        assert!(snap.reassembly.peak_bytes <= LIMIT && snap.reassembly.peak_bytes > 0);

        // Phase 3: releasing every held admission refunds exactly once and
        // the cell admits again.
        for conn_id in held {
            reg.abort(conn_id, 1);
        }
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        reg.insert(200, 1, 4, ASSEMBLY_BYTES / 4)
            .expect("freed budget must admit again");
        assert_eq!(reassembly_used(&budget), ASSEMBLY_BYTES as u64);
        reg.abort(200, 1);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn sharding_isolates_connections() {
        let pool = test_pool();
        let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
        let mut handles = vec![];
        for conn_id in 0u64..8 {
            let reg = reg.clone();
            let h = thread::spawn(move || {
                for req_id in 0u64..10 {
                    reg.insert(conn_id, req_id, 1, 64).unwrap();
                    reg.feed(conn_id, req_id, 0, &[conn_id as u8; 64]).unwrap();
                    let mut finished = reg.finish(conn_id, req_id).unwrap();
                    assert_eq!(
                        finished.backing.copy_bytes().unwrap(),
                        &[conn_id as u8; 64]
                    );
                    finished.backing.release().unwrap();
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
            let h = thread::spawn(move || {
                for i in 0u64..20 {
                    let req_id = thread_idx * 1000 + i;
                    reg.insert(conn_id, req_id, 1, 64).unwrap();
                    reg.feed(conn_id, req_id, 0, &[0u8; 64]).unwrap();
                    let mut finished = reg.finish(conn_id, req_id).unwrap();
                    finished.backing.release().unwrap();
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

    // ── File-backing retention (spill_threshold = 0 forces file spill) ──

    /// Pool where every mapping-backed allocation is forced to FileSpill:
    /// `should_spill` returns true for any size when the threshold is 0.0,
    /// and the pool starts with no pre-created buddy segments.
    fn spill_pool() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                spill_threshold: 0.0,
                ..base_config()
            },
            unique_prefix("s"),
        )))
    }

    fn budgeted_spill_pool(reassembly_limit: u64) -> (Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        (
            Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    spill_threshold: 0.0,
                    ..base_config()
                },
                unique_prefix("t"),
                budget.clone(),
            ))),
            budget,
        )
    }

    fn assert_no_shm_mappings(pool: &Arc<RwLock<MemPool>>) {
        let stats = pool.read().stats();
        assert_eq!(
            stats.total_segments, 0,
            "buddy segments must not be created"
        );
        assert_eq!(
            stats.dedicated_segments, 0,
            "dedicated SHM segments must not be created"
        );
    }

    #[test]
    fn finish_keeps_file_backing_without_shm_promotion() {
        let pool = spill_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let chunk_size = 128;
        reg.insert(1, 700, 3, chunk_size).unwrap();
        // The reassembly allocation itself must already be file-backed.
        assert_no_shm_mappings(&pool);

        // Feed out of order with a short final chunk: 2 (short), 0, 1.
        assert!(!reg.feed(1, 700, 2, &[2u8; 64]).unwrap());
        assert!(!reg.feed(1, 700, 0, &[0u8; 128]).unwrap());
        assert!(reg.feed(1, 700, 1, &[1u8; 128]).unwrap());

        let mut finished = reg.finish(1, 700).unwrap();

        // The completed reassembly must keep its file backing — no promotion.
        assert!(finished.backing.is_file_spill());
        // Logical length trimmed to actual data, not the full allocation.
        assert_eq!(finished.backing.len(), 128 + 128 + 64);
        // Data integrity across the file-backed mapping.
        let slice = finished.backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..320], &[2u8; 64]);

        // Finishing created no buddy or dedicated mappings either.
        assert_no_shm_mappings(&pool);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);

        finished.backing.release().unwrap();
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn file_backed_charge_and_content_across_failure_and_hold_paths() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());

        // Held result: charge survives registry drop and release refunds once.
        reg.insert(1, 710, 2, 512).unwrap();
        assert!(reg.contains(1, 710));
        assert!(!reg.feed(1, 710, 0, &[7u8; 512]).unwrap());
        assert!(reg.feed(1, 710, 1, &[6u8; 256]).unwrap());
        let held = reg.finish(1, 710).unwrap();
        assert!(held.backing.is_file_spill());
        assert_eq!(reassembly_used(&budget), 1024);
        drop(reg);
        let slice = held.backing.copy_bytes().unwrap();
        assert_eq!(&slice[0..512], &[7u8; 512]);
        assert_eq!(&slice[512..768], &[6u8; 256]);
        let mut held = held;
        held.backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);

        // Abort path on a fresh file-backed registry.
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 711, 2, 1024).unwrap();
        reg.feed(1, 711, 0, &[7u8; 1024]).unwrap();
        reg.abort(1, 711);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);

        // A fresh file-backed assembly still works with real content.
        reg.insert(1, 712, 1, 1024).unwrap();
        assert!(reg.feed(1, 712, 0, &[9u8; 1024]).unwrap());
        let mut finished = reg.finish(1, 712).unwrap();
        assert!(finished.backing.is_file_spill());
        assert_eq!(finished.backing.copy_bytes().unwrap(), &[9u8; 1024]);
        finished.backing.release().unwrap();
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn finish_error_path_releases_file_backed_assembly() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 720, 3, 512).unwrap();
        // Feed only 1 of 3 chunks — finish must fail without leaking storage.
        reg.feed(1, 720, 0, &[3u8; 512]).unwrap();
        assert!(reg.finish(1, 720).is_err());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);

        // A fresh assembly after the failed finish still works.
        reg.insert(1, 721, 1, 512).unwrap();
        assert!(reg.feed(1, 721, 0, &[4u8; 512]).unwrap());
        let mut finished = reg.finish(1, 721).unwrap();
        assert!(finished.backing.is_file_spill());
        finished.backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn gc_timeout_releases_file_backed_assembly() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let cfg = ChunkConfig {
            assembler_timeout: Duration::from_millis(50),
            ..ChunkConfig::default()
        };
        let reg = ChunkRegistry::new(pool.clone(), cfg);
        reg.insert(1, 730, 2, 512).unwrap();
        reg.feed(1, 730, 0, &[5u8; 512]).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let stats = reg.gc_sweep();
        assert_eq!(stats.expired, 1);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn file_backed_admission_rejection_creates_no_file_mapping() {
        // A dedicated spill dir isolates this test from parallel registry
        // tests so directory listings are deterministic.
        let spill_dir = std::env::temp_dir().join(format!(
            "c2_reg_reject_{}{:04x}",
            std::process::id(),
            TEST_ID.fetch_add(1, TestOrdering::Relaxed)
        ));
        std::fs::create_dir_all(&spill_dir).unwrap();
        let budget = MemoryBudget::new(1 << 20, 1 << 20, 100);
        let pool = Arc::new(RwLock::new(MemPool::new_with_prefix_and_budget(
            PoolConfig {
                spill_threshold: 0.0,
                spill_dir: spill_dir.clone(),
                ..base_config()
            },
            unique_prefix("r"),
            budget.clone(),
        )));
        let before: Vec<std::path::PathBuf> = std::fs::read_dir(&spill_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();

        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let err = reg.insert(1, 740, 2, 512).unwrap_err();
        assert!(err.contains("'reassembly'"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);

        // The rejection happened before any file mapping: the spill dir is
        // unchanged and no SHM mapping exists.
        let after: Vec<std::path::PathBuf> = std::fs::read_dir(&spill_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(before, after);
        assert_no_shm_mappings(&pool);
        let _ = std::fs::remove_dir_all(&spill_dir);
    }
}
