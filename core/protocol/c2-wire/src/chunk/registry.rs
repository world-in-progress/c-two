//! Sharded chunk registry.
//!
//! [`ChunkRegistry`] is a thread-safe, sharded manager for in-flight chunked
//! reassemblies and settled requests. It wraps [`ChunkAssembler`] instances
//! with tracking metadata and provides frame admission, one error-reply claim,
//! reassembly and connection/GC cleanup through the same sharded table.
//!
//! Admission is finite: `insert` reserves the checked assembly capacity from
//! the pool's canonical reassembly budget before any storage is allocated,
//! and the resulting charge stays live — through `finish` (including its
//! logical trim) and queueing as a request/response backing —
//! until the [`ReassemblyBacking`](crate::chunk::ReassemblyBacking) carrier
//! actually releases the storage.

use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use crate::assembler::ChunkAssembler;
use c2_mem::MemPool;
use tracing::warn;

use crate::chunk::ChunkAdmissionError;
use crate::chunk::config::ChunkConfig;

const SHARD_COUNT: usize = 16;

/// Statistics returned by [`ChunkRegistry::gc_sweep`].
#[derive(Debug, Default)]
pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}

#[repr(u8)]
enum StorageState {
    Pending,
    Assembling,
    Retired,
}

struct RequestState {
    settled: AtomicBool,
    last_received: AtomicBool,
    // Storage admission ends at take, GC, abort or settlement. Frame tokens
    // retain this exact fact even after removal from the shard.
    storage: AtomicU8,
}

impl Default for RequestState {
    fn default() -> Self {
        Self {
            settled: AtomicBool::new(false),
            last_received: AtomicBool::new(false),
            storage: AtomicU8::new(StorageState::Pending as u8),
        }
    }
}

/// One request's shared error-reply claim. Dispatched frames retain this exact
/// claim even after its last frame removes the registry entry.
pub struct ChunkRequest {
    conn_id: u64,
    request_id: u64,
    state: Arc<RequestState>,
}

impl ChunkRequest {
    pub fn is_settled(&self) -> bool {
        self.state.settled.load(Ordering::Acquire)
    }
}

struct RequestContents<C> {
    // Field order also releases storage before admission on final registry drop.
    inner: Option<ChunkAssembler>,
    context: Option<C>,
    last_activity: Instant,
    total_bytes: u64,
}

/// The shard owns a reference to this entry, never its payload borrow. All
/// storage transitions use `contents`; no caller waits for it under a shard.
struct TrackedRequest<C> {
    contents: Mutex<RequestContents<C>>,
    request: Arc<RequestState>,
}

impl<C> TrackedRequest<C> {
    fn pending() -> Self {
        Self {
            contents: Mutex::new(RequestContents {
                inner: None,
                context: None,
                last_activity: Instant::now(),
                total_bytes: 0,
            }),
            request: Arc::new(RequestState::default()),
        }
    }

    fn has_unsettled_frames(&self) -> bool {
        !self.request.settled.load(Ordering::Acquire) && Arc::strong_count(&self.request) > 1
    }
}

type Shard<C> = Mutex<HashMap<(u64, u64), Arc<TrackedRequest<C>>>>;

/// Sharded chunk reassembly lifecycle manager.
pub struct ChunkRegistry<C> {
    shards: [Shard<C>; SHARD_COUNT],
    pool: Arc<RwLock<MemPool>>,
    config: ChunkConfig,
    active_count: AtomicUsize,
    total_bytes: AtomicU64,
    // A sweep without skipped borrows proves no entry expires before this time.
    gc_not_before: Mutex<Option<Instant>>,
    #[cfg(test)]
    admission_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    admission_wait_probe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl<C> ChunkRegistry<C> {
    /// Create a registry using the pool's canonical reassembly budget.
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            pool,
            config,
            active_count: AtomicUsize::new(0),
            total_bytes: AtomicU64::new(0),
            gc_not_before: Mutex::new(None),
            #[cfg(test)]
            admission_probe: Mutex::new(None),
            #[cfg(test)]
            admission_wait_probe: Mutex::new(None),
        }
    }

    fn shard(&self, conn_id: u64) -> &Shard<C> {
        &self.shards[conn_id as usize % SHARD_COUNT]
    }

    fn lookup(&self, conn_id: u64, request_id: u64) -> Option<Arc<TrackedRequest<C>>> {
        self.shard(conn_id)
            .lock()
            .get(&(conn_id, request_id))
            .cloned()
    }

    fn admission_entry(
        &self,
        conn_id: u64,
        request_id: u64,
        request: Option<&ChunkRequest>,
    ) -> Option<Arc<TrackedRequest<C>>> {
        let mut shard = self.shard(conn_id).lock();
        if let Some(request) = request {
            // A frame token can only use the record that minted it. Check
            // its original outcome before any Pending record can be created.
            if request.is_settled()
                || request.state.storage.load(Ordering::Acquire) == StorageState::Retired as u8
            {
                return None;
            }
            shard
                .get(&(conn_id, request_id))
                .filter(|entry| Arc::ptr_eq(&request.state, &entry.request))
                .cloned()
        } else {
            Some(
                shard
                    .entry((conn_id, request_id))
                    .or_insert_with(|| Arc::new(TrackedRequest::pending()))
                    .clone(),
            )
        }
    }

    // Every remover owns `entry` while removing the shard's reference. Even
    // its final payload/context destructors therefore run after both guards.
    fn remove(&self, key: (u64, u64), entry: &Arc<TrackedRequest<C>>) {
        let mut shard = self.shard(key.0).lock();
        if shard
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            shard.remove(&key);
        }
    }

    // Frame-token publication and this decision share the same short shard
    // lock. A rejected admission without a token leaves no uncounted marker;
    // dispatched frames keep their original one-error claim through GC.
    fn remove_if_unobserved(&self, key: (u64, u64), entry: &Arc<TrackedRequest<C>>) {
        let mut shard = self.shard(key.0).lock();
        if !entry.has_unsettled_frames()
            && shard
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entry
                .request
                .storage
                .store(StorageState::Retired as u8, Ordering::Release);
            shard.remove(&key);
        }
    }

    fn detach(
        &self,
        contents: &mut RequestContents<C>,
    ) -> (Option<ChunkAssembler>, Option<C>, u64) {
        let inner = contents.inner.take();
        let context = contents.context.take();
        let bytes = std::mem::take(&mut contents.total_bytes);
        if inner.is_some() {
            self.active_count.fetch_sub(1, Ordering::Relaxed);
            self.total_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
        (inner, context, bytes)
    }

    /// Number of in-flight assemblies, excluding transferred finished owners.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }
    /// Payload and tracking capacity currently owned by registry entries.
    /// Finished backing reservations remain charged independently.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }
    /// Borrow the chunk configuration.
    pub fn config(&self) -> &ChunkConfig {
        &self.config
    }
    /// Borrow the admission pool; live backing owners are independent of it.
    pub fn pool(&self) -> &Arc<RwLock<MemPool>> {
        &self.pool
    }

    /// Whether this key still owns an in-flight assembly.
    pub fn contains(&self, conn_id: u64, request_id: u64) -> bool {
        self.lookup(conn_id, request_id).is_some_and(|entry| {
            entry.request.storage.load(Ordering::Acquire) == StorageState::Assembling as u8
        })
    }

    /// Observe a frame before dispatch. A last settled frame retires only its
    /// marker; already dispatched tasks retain the same error-reply claim.
    pub fn begin_frame(&self, conn_id: u64, request_id: u64, last: bool) -> ChunkRequest {
        let (entry, request) = {
            let mut shard = self.shard(conn_id).lock();
            let entry = shard
                .entry((conn_id, request_id))
                .or_insert_with(|| Arc::new(TrackedRequest::pending()))
                .clone();
            if last {
                entry.request.last_received.store(true, Ordering::Release);
            }
            let request = ChunkRequest {
                conn_id,
                request_id,
                state: entry.request.clone(),
            };
            (entry, request)
        };
        if request.is_settled() && last {
            self.remove((conn_id, request_id), &entry);
        }
        request
    }

    pub fn is_settled(&self, conn_id: u64, request_id: u64) -> bool {
        self.lookup(conn_id, request_id)
            .is_some_and(|entry| entry.request.settled.load(Ordering::Acquire))
    }

    /// Claim this exact request's one error reply, then retire its storage.
    /// The fence is acquired under the shard, but payload borrows and backing
    /// or context destruction always finish outside that shard.
    pub fn settle<F>(&self, request: &ChunkRequest, fence: impl FnOnce() -> F) -> bool {
        let entry = self
            .lookup(request.conn_id, request.request_id)
            .filter(|entry| Arc::ptr_eq(&entry.request, &request.state));
        // Taking the entry borrow before the shard makes storage terminal
        // transitions serialize with feed, take and GC without blocking a shard.
        let mut contents = entry.as_ref().map(|entry| entry.contents.lock());
        let fence = {
            let _shard = self.shard(request.conn_id).lock();
            if request
                .state
                .settled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return false;
            }
            fence()
        };
        request
            .state
            .storage
            .store(StorageState::Retired as u8, Ordering::Release);
        let removed = contents.as_mut().map(|contents| {
            let removed = self.detach(contents);
            contents.last_activity = Instant::now();
            removed
        });
        if request.state.last_received.load(Ordering::Acquire) {
            if let Some(entry) = &entry {
                self.remove((request.conn_id, request.request_id), entry);
            }
        }
        drop(contents);
        drop(removed);
        drop(fence);
        true
    }

    /// Admit storage and its context together. Geometry and the per-message
    /// limit precede reservation; the budget covers full payload capacity and
    /// tracking flags. Rejection returns the caller's context unconsumed.
    pub fn insert(
        &self,
        conn_id: u64,
        request_id: u64,
        total_chunks: usize,
        chunk_size: usize,
        context: C,
    ) -> Result<(), (ChunkAdmissionError, C)> {
        self.insert_tracked(conn_id, request_id, None, total_chunks, chunk_size, context)
    }

    /// Admit only through this frame's original, still-active request state.
    pub fn insert_request(
        &self,
        request: &ChunkRequest,
        total_chunks: usize,
        chunk_size: usize,
        context: C,
    ) -> Result<(), (ChunkAdmissionError, C)> {
        self.insert_tracked(
            request.conn_id,
            request.request_id,
            Some(request),
            total_chunks,
            chunk_size,
            context,
        )
    }

    fn insert_tracked(
        &self,
        conn_id: u64,
        request_id: u64,
        request: Option<&ChunkRequest>,
        total_chunks: usize,
        chunk_size: usize,
        context: C,
    ) -> Result<(), (ChunkAdmissionError, C)> {
        if self.active_count() >= self.config.soft_limit as usize
            || self.total_bytes() >= self.config.max_reassembly_bytes
        {
            let not_before = *self.gc_not_before.lock();
            if not_before.is_none_or(|bound| Instant::now() >= bound) {
                let stats = self.gc_sweep();
                warn!(
                    expired = stats.expired,
                    remaining = stats.remaining,
                    freed_bytes = stats.freed_bytes,
                    "chunk registry soft limit reached, swept expired entries"
                );
            }
        }
        loop {
            let Some(entry) = self.admission_entry(conn_id, request_id, request) else {
                return Err((
                    ChunkAdmissionError::Duplicate {
                        conn_id,
                        request_id,
                    },
                    context,
                ));
            };
            let mut contents = entry.contents.lock();
            if entry.request.settled.load(Ordering::Acquire)
                || entry.request.storage.load(Ordering::Acquire) != StorageState::Pending as u8
            {
                return Err((
                    ChunkAdmissionError::Duplicate {
                        conn_id,
                        request_id,
                    },
                    context,
                ));
            }
            #[cfg(test)]
            if let Some(probe) = self.admission_probe.lock().clone() {
                probe();
            }
            let geometry = match ChunkAssembler::check_geometry(
                total_chunks,
                chunk_size,
                self.config.max_reassembly_bytes,
            ) {
                Ok(geometry) => geometry,
                Err(error) => {
                    self.remove_if_unobserved((conn_id, request_id), &entry);
                    return Err((error, context));
                }
            };
            let Some(pool_guard) = self.pool.try_write() else {
                drop(contents);
                #[cfg(test)]
                if let Some(probe) = self.admission_wait_probe.lock().clone() {
                    probe();
                }
                // Neither entry nor shard is held while waiting for the pool.
                // Retry uses the current entry and its original outcome.
                drop(self.pool.write());
                continue;
            };
            let assembler = match ChunkAssembler::new_with_guard(
                Arc::clone(&self.pool),
                pool_guard,
                geometry,
            ) {
                Ok(assembler) => assembler,
                Err(error) => {
                    self.remove_if_unobserved((conn_id, request_id), &entry);
                    return Err((error, context));
                }
            };
            let bytes = assembler.capacity_bytes();
            contents.inner = Some(assembler);
            contents.context = Some(context);
            contents.total_bytes = bytes;
            contents.last_activity = Instant::now();
            self.active_count.fetch_add(1, Ordering::Relaxed);
            self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
            entry
                .request
                .storage
                .store(StorageState::Assembling as u8, Ordering::Release);
            return Ok(());
        }
    }

    /// Copy under this entry's exclusive borrow, after leaving the shard.
    pub fn feed(
        &self,
        conn_id: u64,
        request_id: u64,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String> {
        self.with_assembly(conn_id, request_id, |inner| {
            inner.feed_chunk(chunk_idx, data)
        })
    }

    fn with_assembly<R>(
        &self,
        conn_id: u64,
        request_id: u64,
        f: impl FnOnce(&mut ChunkAssembler) -> Result<R, String>,
    ) -> Result<R, String> {
        let missing = || format!("no assembly for ({conn_id}, {request_id})");
        let entry = self.lookup(conn_id, request_id).ok_or_else(missing)?;
        let mut contents = entry.contents.lock();
        if entry.request.settled.load(Ordering::Acquire) {
            return Err(missing());
        }
        let result = f(contents.inner.as_mut().ok_or_else(missing)?);
        contents.last_activity = Instant::now();
        result
    }

    /// Transfer storage and admission together. It waits only for this entry's
    /// current payload borrow, never under a shard or public pool lock.
    pub fn take(&self, conn_id: u64, request_id: u64) -> Result<(ChunkAssembler, C), String> {
        let missing = || format!("no assembly for ({conn_id}, {request_id})");
        let entry = self.lookup(conn_id, request_id).ok_or_else(missing)?;
        let mut contents = entry.contents.lock();
        if contents.inner.is_none() || entry.request.settled.load(Ordering::Acquire) {
            return Err(missing());
        }
        entry
            .request
            .storage
            .store(StorageState::Retired as u8, Ordering::Release);
        let (inner, context, _) = self.detach(&mut contents);
        self.remove((conn_id, request_id), &entry);
        drop(contents);
        Ok((inner.unwrap(), context.unwrap()))
    }

    /// Retire one entry, releasing backing before its context outside locks.
    pub fn abort(&self, conn_id: u64, request_id: u64) {
        if let Some(entry) = self.lookup(conn_id, request_id) {
            let removed = {
                let mut contents = entry.contents.lock();
                entry
                    .request
                    .storage
                    .store(StorageState::Retired as u8, Ordering::Release);
                let removed = self.detach(&mut contents);
                self.remove((conn_id, request_id), &entry);
                removed
            };
            drop(removed);
        }
    }

    /// Sweep expired storage without waiting for payload borrows. Frame tasks
    /// retain their original error claim; skipped borrows invalidate the expiry proof.
    pub fn gc_sweep(&self) -> GcStats {
        let mut stats = GcStats::default();
        let now = Instant::now();
        let mut oldest_live = now;
        let mut skipped_borrow = false;
        for shard in &self.shards {
            let entries: Vec<_> = shard
                .lock()
                .iter()
                .map(|(key, entry)| (*key, entry.clone()))
                .collect();
            for (key, entry) in entries {
                let removed = {
                    let Some(mut contents) = entry.contents.try_lock() else {
                        skipped_borrow = true;
                        continue;
                    };
                    if now.saturating_duration_since(contents.last_activity)
                        < self.config.assembler_timeout
                    {
                        oldest_live = oldest_live.min(contents.last_activity);
                        continue;
                    }
                    entry
                        .request
                        .storage
                        .store(StorageState::Retired as u8, Ordering::Release);
                    let removed = self.detach(&mut contents);
                    if removed.0.is_some() {
                        stats.expired += 1;
                        stats.freed_bytes += removed.2;
                    }
                    self.remove_if_unobserved(key, &entry);
                    removed
                };
                drop(removed);
            }
        }
        stats.remaining = self.active_count();
        *self.gc_not_before.lock() = if skipped_borrow {
            // Its last activity is unknown: the next pressure admission may
            // need to collect this entry as soon as the borrow ends.
            None
        } else {
            oldest_live.checked_add(self.config.assembler_timeout)
        };
        stats
    }

    /// Retire this connection's entries without touching other connections.
    pub fn cleanup_connection(&self, conn_id: u64) {
        let entries: Vec<_> = self
            .shard(conn_id)
            .lock()
            .iter()
            .filter(|(key, _)| key.0 == conn_id)
            .map(|(key, entry)| (*key, entry.clone()))
            .collect();
        for (key, entry) in entries {
            let removed = {
                let mut contents = entry.contents.lock();
                entry
                    .request
                    .storage
                    .store(StorageState::Retired as u8, Ordering::Release);
                let removed = self.detach(&mut contents);
                self.remove(key, &entry);
                removed
            };
            drop(removed);
        }
    }

    /// Skip busy entry borrows or shards, retaining their complete owner.
    pub fn try_cleanup_connection(&self, conn_id: u64) -> bool {
        self.try_drain_shard(
            self.shard(conn_id),
            |key| key.0 == conn_id,
            |_| true,
            None,
            false,
        )
        .0
    }

    /// Nonblocking teardown with the deadline checked before each release.
    pub fn try_cleanup_connection_until(&self, conn_id: u64, deadline: Instant) -> bool {
        self.try_drain_shard(
            self.shard(conn_id),
            |key| key.0 == conn_id,
            |_| true,
            Some(deadline),
            false,
        )
        .0
    }

    /// Nonblocking abort of one exact entry; unrelated busy entries are skipped.
    pub fn try_abort(&self, conn_id: u64, request_id: u64) -> bool {
        self.try_drain_shard(
            self.shard(conn_id),
            |key| *key == (conn_id, request_id),
            |_| true,
            None,
            false,
        )
        .0
    }

    /// Nonblocking GC; only successfully detached storage counts as expired.
    pub fn try_gc_sweep(&self) -> GcStats {
        let mut stats = GcStats::default();
        let now = Instant::now();
        for shard in &self.shards {
            let (_, released) = self.try_drain_shard(
                shard,
                |_| true,
                |contents| {
                    now.saturating_duration_since(contents.last_activity)
                        >= self.config.assembler_timeout
                },
                None,
                true,
            );
            stats.expired += released.expired;
            stats.freed_bytes += released.freed_bytes;
        }
        stats.remaining = self.active_count();
        stats
    }

    fn try_drain_shard(
        &self,
        shard: &Shard<C>,
        matches_key: impl Fn(&(u64, u64)) -> bool,
        matches_contents: impl Fn(&RequestContents<C>) -> bool,
        deadline: Option<Instant>,
        keep_frames: bool,
    ) -> (bool, GcStats) {
        let entries: Vec<_> = {
            let Some(shard) = shard.try_lock() else {
                return (false, GcStats::default());
            };
            shard
                .iter()
                .filter(|(key, _)| matches_key(key))
                .map(|(key, entry)| (*key, entry.clone()))
                .collect()
        };
        let mut complete = true;
        let mut stats = GcStats::default();
        for (key, entry) in entries {
            let Some(mut contents) = entry.contents.try_lock() else {
                complete = false;
                continue;
            };
            if !matches_contents(&contents) {
                continue;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                complete = false;
                continue;
            }
            // The second shard acquisition is also nonblocking. Do not retire
            // storage until this attempt can commit its exact entry removal.
            let Some(mut shard) = shard.try_lock() else {
                complete = false;
                continue;
            };
            entry
                .request
                .storage
                .store(StorageState::Retired as u8, Ordering::Release);
            let removed = self.detach(&mut contents);
            if removed.0.is_some() {
                stats.expired += 1;
                stats.freed_bytes += removed.2;
            }
            if (!keep_frames || !entry.has_unsettled_frames())
                && shard
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                shard.remove(&key);
            }
            drop(shard);
            drop(contents);
            drop(removed);
        }
        stats.remaining = self.active_count();
        (complete, stats)
    }
}

impl ChunkRegistry<()> {
    /// Admit the first reply chunk using its decoded wire metadata.
    /// Decoding alone accepts zero fields; validate them here before any
    /// reservation, then use the same atomic registry admission authority.
    pub fn insert_reply(
        &self,
        conn_id: u64,
        request_id: u64,
        total_size: u64,
        total_chunks: usize,
        first_data_len: usize,
    ) -> Result<(), ChunkAdmissionError> {
        if total_size == 0 {
            return Err(ChunkAdmissionError::Protocol(
                "total_size must be > 0".to_string(),
            ));
        }
        if first_data_len == 0 {
            return Err(ChunkAdmissionError::Protocol(
                "chunk_size must be > 0".to_string(),
            ));
        }
        if total_chunks == 1 && u64::try_from(first_data_len).map_or(true, |len| len > total_size) {
            return Err(ChunkAdmissionError::Protocol(format!(
                "first chunk data length {first_data_len} exceeds declared total_size {total_size}"
            )));
        }
        let chunk_size = if total_chunks > 1 {
            first_data_len
        } else {
            usize::try_from(total_size).map_err(|_| {
                ChunkAdmissionError::Protocol("total_size exceeds addressable capacity".to_string())
            })?
        };
        self.insert(conn_id, request_id, total_chunks, chunk_size, ())
            .map_err(|(error, ())| error)
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

            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_reg_test"),
            ..PoolConfig::default()
        }
    }

    fn test_pool() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(
            MemPool::new_with_prefix(base_config(), unique_prefix("")).unwrap(),
        ))
    }

    fn budgeted_pool(reassembly_limit: u64) -> (Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        (
            Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    base_config(),
                    unique_prefix("b"),
                    budget.clone(),
                )
                .unwrap(),
            )),
            budget,
        )
    }

    fn reassembly_used(budget: &MemoryBudget) -> u64 {
        budget.snapshot().cell(BudgetKind::Reassembly).used_bytes
    }

    #[test]
    fn failed_admission_retains_only_existing_frame_tokens() {
        for (reassembly_limit, file_limit) in [(0, 4096), (1024, 0)] {
            let (reg, pool, budget) = typed_file_registry(reassembly_limit, file_limit);
            for request_id in 1..=32 {
                assert!(reg.insert_reply(1, request_id, 128, 2, 64).is_err());
                assert!(
                    reg.shard(1).lock().is_empty(),
                    "rejected reply left uncounted metadata"
                );
                assert!(reg.insert(1, request_id, 0, 64, ()).is_err());
                assert!(
                    reg.shard(1).lock().is_empty(),
                    "bad geometry left uncounted metadata"
                );
            }
            let request = reg.begin_frame(1, 99, false);
            assert!(reg.insert_request(&request, 2, 64, ()).is_err());
            let continuation = reg.begin_frame(1, 99, true);
            assert!(Arc::ptr_eq(&request.state, &continuation.state));
            assert!(reg.settle(&request, || ()));
            assert!(!reg.settle(&continuation, || ()));
            assert!(reg.shard(1).lock().is_empty());
            assert_eq!(reg.active_count(), 0);
            assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
            assert_no_shm_mappings(&pool);
        }
    }

    #[test]
    fn settled_first_chunk_token_never_recreates_pending() {
        for settled in [true, false] {
            let (pool, budget) = budgeted_pool(128);
            let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
            let first = reg.begin_frame(1, 11, false);
            let entered = Arc::new(std::sync::Barrier::new(2));
            let resume = Arc::new(std::sync::Barrier::new(2));
            let inserting = reg.clone();
            let task_entered = entered.clone();
            let task_resume = resume.clone();
            let task = thread::spawn(move || {
                task_entered.wait();
                task_resume.wait();
                inserting.insert_request(&first, 1, 64, 42)
            });
            entered.wait();
            if settled {
                let last = reg.begin_frame(1, 11, true);
                assert!(reg.settle(&last, || ()));
            } else {
                reg.abort(1, 11);
            }
            assert!(!reg.shard(1).lock().contains_key(&(1, 11)));
            resume.wait();
            assert!(matches!(
                task.join().unwrap(),
                Err((
                    ChunkAdmissionError::Duplicate {
                        conn_id: 1,
                        request_id: 11
                    },
                    42
                ))
            ));
            assert!(
                !reg.shard(1).lock().contains_key(&(1, 11)),
                "the delayed first chunk recreated a retired request"
            );
            assert_eq!(reg.active_count(), 0);
            assert_eq!(reg.total_bytes(), 0);
            assert_eq!(budget.snapshot().reassembly.peak_bytes, 0);
            assert_no_shm_mappings(&pool);
        }
    }

    #[test]
    fn pressure_gc_admits_third_call_during_copy() {
        for file in [false, true] {
            for (soft_limit, byte_limit) in [(2, 4096), (512, 128)] {
                let (pool, budget) = if file {
                    budgeted_spill_pool(3 * (64 + 1))
                } else {
                    budgeted_pool(3 * (64 + 1))
                };
                let reg = Arc::new(ChunkRegistry::new(
                    pool,
                    ChunkConfig {
                        soft_limit,
                        max_reassembly_bytes: byte_limit,
                        ..ChunkConfig::default()
                    },
                ));
                reg.insert(1, 11, 1, 64, ()).unwrap();
                let other_conn = 1 + SHARD_COUNT as u64;
                reg.insert(other_conn, 22, 1, 64, ()).unwrap();
                reg.lookup(1, 11).unwrap().contents.lock().last_activity =
                    Instant::now() - reg.config.assembler_timeout;
                let entered = Arc::new(std::sync::Barrier::new(2));
                let resume = Arc::new(std::sync::Barrier::new(2));
                let copying = reg.clone();
                let copy_entered = entered.clone();
                let copy_resume = resume.clone();
                let worker = thread::spawn(move || {
                    copying.with_assembly(1, 11, |assembly| {
                        copy_entered.wait();
                        copy_resume.wait();
                        assembly.feed_chunk(0, &[0x11; 64])
                    })
                });
                entered.wait();
                let (done, observed) = std::sync::mpsc::channel();
                let advancing = reg.clone();
                let third = thread::spawn(move || {
                    done.send(advancing.insert(1, 33, 1, 64, ())).unwrap();
                });
                let progressed = observed.recv_timeout(Duration::from_secs(2));
                let charged = budget.snapshot();
                let bound = *reg.gc_not_before.lock();
                resume.wait();
                assert!(worker.join().unwrap().unwrap());
                third.join().unwrap();
                progressed
                    .expect("pressure GC waited for another call's copy")
                    .unwrap();
                assert_eq!(charged.reassembly.used_bytes, 3 * (64 + 1));
                assert_eq!(
                    bound, None,
                    "a skipped entry cannot prove a later expiry bound"
                );
                assert_eq!(reg.active_count(), 3);
                // The skipped entry may need collecting on the very next
                // pressure admission; no timer or delayed scan may be required.
                reg.lookup(1, 11).unwrap().contents.lock().last_activity =
                    Instant::now() - reg.config.assembler_timeout;
                reg.insert(1, 44, 1, 64, ()).unwrap();
                assert!(!reg.contains(1, 11));
                assert!(reg.contains(other_conn, 22));
                assert!(reg.contains(1, 33));
                assert!(reg.contains(1, 44));
                assert_eq!(reassembly_used(&budget), 3 * (64 + 1));
                assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
                reg.cleanup_connection(1);
                reg.cleanup_connection(other_conn);
                assert_eq!(reassembly_used(&budget), 0);
                assert_eq!(budget.snapshot().file.used_bytes, 0);
            }
        }
    }

    #[test]
    fn entry_copy_borrow_allows_same_shard_admission_release_and_cancellation() {
        for file in [false, true] {
            let (pool, budget) = if file {
                budgeted_spill_pool(1 << 20)
            } else {
                budgeted_pool(1 << 20)
            };
            let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
            reg.insert(1, 11, 1, 4096, ()).unwrap();
            let entered = Arc::new(std::sync::Barrier::new(2));
            let resume = Arc::new(std::sync::Barrier::new(2));
            let copying = reg.clone();
            let copy_entered = entered.clone();
            let copy_resume = resume.clone();
            let worker = thread::spawn(move || {
                copying.with_assembly(1, 11, |assembly| {
                    // Pause with exactly the exclusive entry borrow used by feed;
                    // the backing test separately pauses its actual slice borrow.
                    copy_entered.wait();
                    copy_resume.wait();
                    assembly.feed_chunk(0, &[0x11; 4096])
                })
            });
            entered.wait();
            let (done, observed) = std::sync::mpsc::channel();
            let advancing = reg.clone();
            let progress = thread::spawn(move || {
                assert!(advancing.shard(1).try_lock().is_some());
                assert!(advancing.pool.try_write().is_some());
                assert!(advancing.contains(1, 11));
                let request = advancing.begin_frame(1, 22, true);
                advancing.insert_request(&request, 1, 4096, ()).unwrap();
                advancing.feed(1, 22, 0, &[0x22; 4096]).unwrap();
                assert!(advancing.settle(&request, || ()));
                assert!(advancing.try_abort(1, 22));
                done.send(()).unwrap();
            });
            let progressed = observed.recv_timeout(Duration::from_secs(2));
            let charged = budget.snapshot();
            resume.wait();
            assert!(worker.join().unwrap().unwrap());
            progress.join().unwrap();
            progressed.expect("another call waited for the copy's shard or pool lock");
            assert_eq!(charged.reassembly.used_bytes, 4096 + 1);
            if file {
                assert_eq!(charged.file.used_bytes, 4096);
            }
            let (assembly, ()) = reg.take(1, 11).unwrap();
            assert_eq!(
                assembly.finish().unwrap().copy_bytes().unwrap(),
                &[0x11; 4096]
            );
            assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        }
    }

    #[test]
    fn terminal_paths_preserve_the_entry_borrow_and_refund_only_its_owner() {
        struct Context {
            request_id: u64,
            drops: Arc<Mutex<Vec<u64>>>,
            registry: std::sync::Weak<ChunkRegistry<Context>>,
            budget: MemoryBudget,
        }
        impl Drop for Context {
            fn drop(&mut self) {
                let registry = self.registry.upgrade().unwrap();
                assert!(registry.shard(1).try_lock().is_some());
                assert_eq!(
                    self.budget.snapshot().reassembly.used_bytes,
                    if self.request_id == 11 { 1024 + 1 } else { 0 }
                );
                self.drops.lock().push(self.request_id);
            }
        }
        for file in [false, true] {
            for action in ["gc", "take", "settle", "abort", "disconnect"] {
                let (pool, budget) = if file {
                    budgeted_spill_pool(1 << 20)
                } else {
                    budgeted_pool(1 << 20)
                };
                let reg = Arc::new(ChunkRegistry::new(
                    pool.clone(),
                    ChunkConfig {
                        assembler_timeout: Duration::from_secs(60),
                        ..ChunkConfig::default()
                    },
                ));
                let request = reg.begin_frame(1, 11, true);
                let drops = Arc::new(Mutex::new(Vec::new()));
                let context = |request_id| Context {
                    request_id,
                    drops: drops.clone(),
                    registry: Arc::downgrade(&reg),
                    budget: budget.clone(),
                };
                reg.insert_request(&request, 1, 4096, context(11))
                    .unwrap_or_else(|(e, _)| panic!("{e}"));
                // An unrelated connection shares the shard and the budget.
                reg.insert(1 + SHARD_COUNT as u64, 22, 1, 1024, context(22))
                    .unwrap_or_else(|(e, _)| panic!("{e}"));
                let entered = Arc::new(std::sync::Barrier::new(2));
                let resume = Arc::new(std::sync::Barrier::new(2));
                let copy_entered = entered.clone();
                let copy_resume = resume.clone();
                let copying = reg.clone();
                let worker = thread::spawn(move || {
                    copying.with_assembly(1, 11, |assembly| {
                        copy_entered.wait();
                        copy_resume.wait();
                        assembly.feed_chunk(0, &[0x11; 4096])
                    })
                });
                entered.wait();
                assert!(!reg.try_abort(1, 11));
                assert!(
                    drops.lock().is_empty(),
                    "copy borrow must retain its admission"
                );
                assert_eq!(
                    budget.snapshot().reassembly.used_bytes,
                    (4096 + 1) + (1024 + 1)
                );
                let (done, observed) = std::sync::mpsc::channel();
                let terminal = reg.clone();
                let terminal_started = Arc::new(std::sync::Barrier::new(2));
                let started = terminal_started.clone();
                let terminating = thread::spawn(move || {
                    started.wait();
                    let taken = match action {
                        "gc" => {
                            terminal.gc_sweep();
                            None
                        }
                        "take" => Some(terminal.take(1, 11).unwrap()),
                        "settle" => {
                            assert!(terminal.settle(&request, || ()));
                            None
                        }
                        "abort" => {
                            terminal.abort(1, 11);
                            None
                        }
                        "disconnect" => {
                            terminal.cleanup_connection(1);
                            None
                        }
                        _ => unreachable!(),
                    };
                    done.send(taken).unwrap();
                });
                terminal_started.wait();
                let before_resume = observed.recv_timeout(if action == "gc" {
                    Duration::from_secs(2)
                } else {
                    Duration::ZERO
                });
                // Skipping or waiting for this borrow cannot retain the shard.
                let (progressed, progress_observed) = std::sync::mpsc::channel();
                let checking = reg.clone();
                let progress = thread::spawn(move || {
                    let other = checking.begin_frame(1, 33, true);
                    assert!(checking.settle(&other, || ()));
                    progressed.send(()).unwrap();
                });
                let advanced = progress_observed.recv_timeout(Duration::from_secs(2));
                assert!(pool.try_write().is_some());
                assert_eq!(
                    budget.snapshot().reassembly.used_bytes,
                    (4096 + 1) + (1024 + 1)
                );
                resume.wait();
                progress.join().unwrap();
                advanced.expect("a terminal path retained the shard while waiting for a copy");
                assert!(worker.join().unwrap().unwrap());
                terminating.join().unwrap();
                let taken = if action == "gc" {
                    before_resume.expect("GC waited for a payload borrow")
                } else {
                    assert!(matches!(
                        before_resume,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ));
                    observed.recv_timeout(Duration::from_secs(2)).unwrap()
                };
                if let Some((assembly, admission)) = taken {
                    let backing = assembly.finish().unwrap();
                    assert_eq!(backing.copy_bytes().unwrap(), &[0x11; 4096]);
                    assert_eq!(backing.capacity_bytes(), 4096 + 1);
                    drop(backing);
                    drop(admission);
                }
                if action == "gc" {
                    // GC skipped the borrow and retained storage plus admission.
                    assert_eq!(
                        budget.snapshot().reassembly.used_bytes,
                        (4096 + 1) + (1024 + 1)
                    );
                    reg.lookup(1, 11).unwrap().contents.lock().last_activity =
                        Instant::now() - reg.config.assembler_timeout;
                    assert_eq!(reg.gc_sweep().expired, 1);
                }
                assert_eq!(budget.snapshot().reassembly.used_bytes, 1024 + 1);
                assert_eq!(*drops.lock(), vec![11]);
                assert!(reg.contains(1 + SHARD_COUNT as u64, 22));
                reg.abort(1 + SHARD_COUNT as u64, 22);
                assert_eq!(*drops.lock(), vec![11, 22]);
                assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
                assert_eq!(budget.snapshot().file.used_bytes, 0);
            }
        }
    }

    #[test]
    fn settled_request_releases_backing_and_last_frame_preserves_old_reply_claims() {
        let (pool, budget) = budgeted_pool(128);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        let first = reg.begin_frame(1, 7, false);
        let competing = reg.begin_frame(1, 7, false);
        reg.insert(1, 7, 2, 32, ()).unwrap();
        assert!(reassembly_used(&budget) > 0);
        assert!(reg.settle(&first, || ()));
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert!(reg.is_settled(1, 7));
        assert!(reg.begin_frame(1, 7, true).is_settled());
        assert!(!reg.shard(1).lock().contains_key(&(1, 7)));
        assert!(
            !reg.settle(&competing, || ()),
            "a delayed task cannot reply again after last-frame cleanup"
        );
    }

    #[test]
    fn settled_records_use_no_chunk_budget_and_gc_does_not_refresh_their_age() {
        for nonblocking in [false, true] {
            let (pool, budget) = budgeted_pool(128);
            let reg = ChunkRegistry::new(
                pool,
                ChunkConfig {
                    soft_limit: 1,
                    ..Default::default()
                },
            );
            for id in 1..=100 {
                let request = reg.begin_frame(1, id, false);
                assert!(reg.settle(&request, || ()));
            }
            assert_eq!(reassembly_used(&budget), 0);
            assert_eq!(reg.active_count(), 0);
            reg.insert(2, 1, 2, 32, ()).unwrap();
            reg.abort(2, 1);
            let expired = Instant::now() - reg.config.assembler_timeout;
            for entry in reg.shard(1).lock().values_mut() {
                entry.contents.lock().last_activity = expired;
            }
            assert!(reg.begin_frame(1, 1, false).is_settled());
            assert_eq!(
                reg.shard(1).lock()[&(1, 1)].contents.lock().last_activity,
                expired
            );
            let swept = if nonblocking {
                reg.try_gc_sweep()
            } else {
                reg.gc_sweep()
            };
            assert_eq!(swept.expired, 0, "settled entries own no assemblies");
            assert_eq!(swept.freed_bytes, 0);
            assert_eq!(swept.remaining, 0);
            assert!(reg.shard(1).lock().is_empty());
            assert_eq!(reassembly_used(&budget), 0);
        }
    }

    #[test]
    fn gc_releases_assembly_but_keeps_inflight_frames_on_one_error_claim() {
        let (pool, budget) = budgeted_pool(128);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        let queued = reg.begin_frame(1, 9, false);
        reg.insert(1, 9, 2, 32, ()).unwrap();
        reg.shard(1)
            .lock()
            .get_mut(&(1, 9))
            .unwrap()
            .contents
            .lock()
            .last_activity = Instant::now() - reg.config.assembler_timeout;
        assert_eq!(reg.gc_sweep().expired, 1);
        assert!(!reg.contains(1, 9));
        assert_eq!(reassembly_used(&budget), 0);
        let late = reg.begin_frame(1, 9, false);
        assert!(Arc::ptr_eq(&queued.state, &late.state));
        assert!(reg.settle(&queued, || ()));
        assert!(!reg.settle(&late, || ()));
        reg.cleanup_connection(1);
        assert!(reg.shard(1).lock().is_empty());
        assert!(!reg.settle(&queued, || ()));
    }

    #[test]
    fn forced_chunked_100_mib_request_reassembles_beyond_512_chunks() {
        const CHUNK_SIZE: usize = 128 * 1024;
        const TOTAL_CHUNKS: usize = 800;
        const TOTAL_SIZE: usize = CHUNK_SIZE * TOTAL_CHUNKS;

        let spill_dir = std::env::temp_dir().join(format!(
            "c2-wire-100mib-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, TestOrdering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&spill_dir).unwrap();

        let budget = MemoryBudget::new(0, TOTAL_SIZE as u64, (TOTAL_SIZE + TOTAL_CHUNKS) as u64);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    buddy_enabled: false,
                    spill_threshold: 0.0,
                    spill_dir: spill_dir.clone(),
                    ..base_config()
                },
                unique_prefix("large"),
                budget.clone(),
            )
            .unwrap(),
        ));
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 1, TOTAL_CHUNKS, CHUNK_SIZE, ()).unwrap();
        let data = vec![0xa5; CHUNK_SIZE];
        for index in 0..TOTAL_CHUNKS {
            assert_eq!(
                reg.feed(1, 1, index, &data).unwrap(),
                index + 1 == TOTAL_CHUNKS
            );
        }
        let (assembler, ()) = reg.take(1, 1).unwrap();
        let mut backing = assembler.finish().unwrap();
        assert_eq!(backing.len(), TOTAL_SIZE);
        assert_eq!(backing.capacity_bytes(), (TOTAL_SIZE + TOTAL_CHUNKS) as u64);
        assert!(
            backing
                .copy_bytes()
                .unwrap()
                .iter()
                .all(|byte| *byte == 0xa5)
        );
        backing.release();
        assert_eq!(reassembly_used(&budget), 0);
        std::fs::remove_dir(&spill_dir).unwrap();
    }

    #[test]
    fn insert_feed_finish_happy_path() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let data = b"hello world!";
        reg.insert(1, 100, 1, data.len(), ()).unwrap();
        assert!(reg.contains(1, 100));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), data.len() as u64 + 1);
        let complete = reg.feed(1, 100, 0, data).unwrap();
        assert!(complete);
        let (assembler, ()) = reg.take(1, 100).unwrap();
        let mut finished = assembler.finish().unwrap();
        assert!(!reg.contains(1, 100));
        assert_eq!(finished.len(), data.len());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(finished.copy_bytes().unwrap(), data);
        finished.release();
    }

    #[test]
    fn abort_frees_resources_and_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 200, 3, 1024, ()).unwrap();
        assert!(!reg.feed(1, 200, 0, &[7; 200]).unwrap());
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), 3 * 1024 + 3);
        assert_eq!(reassembly_used(&budget), 3 * 1024 + 3);
        reg.abort(1, 200);
        // Idempotent: a missing entry aborts nothing.
        reg.abort(1, 200);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn finish_error_path_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        let context = Arc::new(());
        reg.insert(1, 300, 3, 1024, context.clone()).unwrap();
        // Feed only 1 of 3 chunks — finish should fail and refund once.
        reg.feed(1, 300, 0, &[0u8; 1024]).unwrap();
        let (assembler, retained) = reg.take(1, 300).unwrap();
        assert!(assembler.finish().is_err());
        assert_eq!(Arc::strong_count(&context), 2);
        drop(retained);
        assert_eq!(Arc::strong_count(&context), 1);
        // Resources and charge freed exactly once; the pool stays usable.
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        reg.insert(1, 301, 1, 1024, context.clone()).unwrap();
        reg.abort(1, 301);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn duplicate_insert_rejects_and_returns_new_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 500, 2, 64, ()).unwrap();
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 128 + 2);
        // Second insert for same key must fail and keep exactly one charge.
        let err = reg.insert(1, 500, 3, 128, ()).unwrap_err().0.to_string();
        assert!(err.contains("duplicate"), "expected duplicate error: {err}");
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 128 + 2);
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
        reg.insert(1, 510, 2, 64, ()).unwrap();
        assert_eq!(reassembly_used(&budget), 128 + 2);
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
        reg.insert(1, 400, 4, chunk_size, ()).unwrap();
        // Feed out of order: 3, 1, 0, 2
        assert!(!reg.feed(1, 400, 3, &[3u8; 64]).unwrap());
        assert!(!reg.feed(1, 400, 1, &[1u8; 128]).unwrap());
        assert!(!reg.feed(1, 400, 0, &[0u8; 128]).unwrap());
        assert!(reg.feed(1, 400, 2, &[2u8; 128]).unwrap());
        let (assembler, ()) = reg.take(1, 400).unwrap();
        let mut finished = assembler.finish().unwrap();
        // Logical length must be trimmed to actual data (last chunk is 64, not 128).
        assert_eq!(finished.len(), 128 * 3 + 64); // 448, not 512
        // Verify data integrity
        let slice = finished.copy_bytes().unwrap();
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..384], &[2u8; 128]);
        assert_eq!(&slice[384..384 + 64], &[3u8; 64]);
        finished.release();
    }

    #[test]
    fn cleanup_releases_backing_before_context_outside_locks() {
        struct Context {
            registry: std::sync::Weak<ChunkRegistry<Context>>,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Context {
            fn drop(&mut self) {
                let registry = self.registry.upgrade().unwrap();
                assert!(registry.shard(1).try_lock().is_some());
                assert!(registry.gc_not_before.try_lock().is_some());
                assert_eq!(registry.pool.try_write().unwrap().stats().alloc_count, 0);
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (pool, budget) = budgeted_pool(1 << 20);
        let cfg = ChunkConfig {
            assembler_timeout: Duration::from_millis(50),
            ..ChunkConfig::default()
        };
        let reg = Arc::new(ChunkRegistry::new(pool.clone(), cfg));
        let dropped = Arc::new(AtomicUsize::new(0));
        for (index, action) in ["abort", "disconnect", "gc", "try_cleanup"]
            .iter()
            .enumerate()
        {
            reg.insert(
                1,
                1,
                1,
                1024,
                Context {
                    registry: Arc::downgrade(&reg),
                    dropped: dropped.clone(),
                },
            )
            .unwrap_or_else(|(error, _)| panic!("{error}"));
            assert_eq!(reassembly_used(&budget), 1024 + 1);
            match *action {
                "abort" => reg.abort(1, 1),
                "disconnect" => reg.cleanup_connection(1),
                "gc" => {
                    std::thread::sleep(Duration::from_millis(80));
                    assert_eq!(reg.gc_sweep().expired, 1);
                }
                "try_cleanup" => {
                    let entry = reg.lookup(1, 1).unwrap();
                    let held_entry = entry.contents.lock();
                    assert!(!reg.try_cleanup_connection(1));
                    assert_eq!(dropped.load(Ordering::SeqCst), index);
                    assert_eq!(reassembly_used(&budget), 1024 + 1);
                    drop(held_entry);
                    assert!(reg.try_cleanup_connection(1));
                }
                _ => unreachable!(),
            }
            assert_eq!(dropped.load(Ordering::SeqCst), index + 1);
            assert_eq!(reg.active_count(), 0);
            assert_eq!(reg.total_bytes(), 0);
            assert_eq!(reassembly_used(&budget), 0);
        }
    }

    #[test]
    fn disconnect_cleanup_returns_charge_once() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 100, 1, 1024, ()).unwrap();
        reg.insert(1, 200, 2, 512, ()).unwrap();
        reg.insert(2, 100, 1, 256, ()).unwrap();
        assert_eq!(
            reassembly_used(&budget),
            (1024 + 1) + (1024 + 2) + (256 + 1)
        );
        reg.cleanup_connection(1);
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 256 + 1);
        // Conn 2's assembly survives and completes.
        let complete = reg.feed(2, 100, 0, &[42u8; 256]).unwrap();
        assert!(complete);
        let (assembler, ()) = reg.take(2, 100).unwrap();
        let mut finished = assembler.finish().unwrap();
        finished.release();
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn gc_sweep_reentrant_feed_preserves_expiry_progress() {
        struct Context(Option<Box<dyn FnOnce() + Send>>);
        impl Drop for Context {
            fn drop(&mut self) {
                if let Some(action) = self.0.take() {
                    action();
                }
            }
        }
        let (pool, budget) = budgeted_pool(64 + 1);
        let reg = Arc::new(ChunkRegistry::new(
            pool,
            ChunkConfig {
                soft_limit: 1,
                assembler_timeout: Duration::from_secs(1),
                ..ChunkConfig::default()
            },
        ));
        let weak = Arc::downgrade(&reg);
        reg.insert(
            1,
            1,
            1,
            64,
            Context(Some(Box::new(move || {
                let reg = weak.upgrade().unwrap();
                // Shard 0 was already scanned. The removed backing is freed before
                // this context inserts and refreshes a new generation there.
                reg.insert(0, 2, 1, 64, Context(None))
                    .unwrap_or_else(|(e, _)| panic!("{e}"));
                reg.shard(0)
                    .lock()
                    .get_mut(&(0, 2))
                    .unwrap()
                    .contents
                    .lock()
                    .last_activity = Instant::now() - Duration::from_secs(2);
                let before = Instant::now();
                reg.feed(0, 2, 0, &[42; 64]).unwrap();
                let after = Instant::now();
                assert!(
                    (before..=after)
                        .contains(&reg.shard(0).lock()[&(0, 2)].contents.lock().last_activity)
                );
                assert_eq!(reg.gc_sweep().expired, 0); // nested scan publishes first
                thread::sleep(Duration::from_millis(1100)); // slow outer scan crosses expiry
            }))),
        )
        .unwrap_or_else(|(e, _)| panic!("{e}"));
        reg.shard(1)
            .lock()
            .get_mut(&(1, 1))
            .unwrap()
            .contents
            .lock()
            .last_activity = Instant::now() - Duration::from_secs(2);
        assert_eq!(reg.gc_sweep().expired, 1);
        assert!(reg.contains(0, 2));
        // No timer: the next pressure admission must collect the late entry,
        // despite the outer scan completing after the nested scan.
        reg.insert(15, 3, 1, 64, Context(None))
            .unwrap_or_else(|(e, _)| panic!("{e}"));
        assert!(!reg.contains(0, 2));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 64 + 1);
        drop(reg);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn pressure_gc_preserves_admission_without_timer() {
        for (soft_limit, byte_limit, expired, partial) in [
            (2, 4096, true, false),
            (512, 128, true, false),
            (2, 4096, true, true),
            (2, 4096, false, false),
            (512, 4096, true, false),
        ] {
            let (pool, budget) = budgeted_pool(2 * (64 + 1));
            let reg = ChunkRegistry::new(
                pool,
                ChunkConfig {
                    soft_limit,
                    max_reassembly_bytes: byte_limit,
                    ..ChunkConfig::default()
                },
            );
            let second = if partial { 17 } else { 15 };
            reg.insert(1, 1, 1, 64, ()).unwrap();
            reg.insert(second, 2, 1, 64, ()).unwrap();
            let activity = Instant::now() - Duration::from_secs(if expired { 61 } else { 30 });
            for (conn, rid) in [(1, 1), (second, 2)] {
                reg.shard(conn)
                    .lock()
                    .get_mut(&(conn, rid))
                    .unwrap()
                    .contents
                    .lock()
                    .last_activity = activity;
            }
            if partial {
                let _busy = reg.shard(1).lock();
                assert_eq!(reg.try_gc_sweep().expired, 0);
            }
            let result = reg.insert(7, 3, 1, 64, ());
            let reclaimed = expired && (soft_limit == 2 || byte_limit == 128);
            if !expired {
                assert_eq!(
                    *reg.gc_not_before.lock(),
                    activity.checked_add(reg.config.assembler_timeout)
                );
            }
            assert_eq!(result.is_ok(), reclaimed);
            if !reclaimed {
                assert!(matches!(
                    result,
                    Err((ChunkAdmissionError::Capacity(_), ()))
                ));
            }
            assert_eq!(reg.active_count(), if reclaimed { 1 } else { 2 });
            assert_eq!(
                reassembly_used(&budget),
                if reclaimed { 64 + 1 } else { 2 * (64 + 1) }
            );
            assert_eq!(
                budget.snapshot().reassembly.rejected_allocations,
                u64::from(!reclaimed)
            );
            drop(reg);
            assert_eq!(reassembly_used(&budget), 0);
        }
    }

    #[test]
    fn pressure_gc_skips_a_proven_fresh_unrelated_shard() {
        let (pool, budget) = budgeted_pool(2 * (64 + 1));
        let reg = Arc::new(ChunkRegistry::new(
            pool,
            ChunkConfig {
                soft_limit: 1,
                ..ChunkConfig::default()
            },
        ));
        reg.insert(1, 1, 1, 64, ()).unwrap();
        assert_eq!(reg.gc_sweep().expired, 0);
        let busy = reg.shard(0).lock();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_reg = reg.clone();
        let worker = thread::spawn(move || tx.send(worker_reg.insert(2, 2, 1, 64, ())).unwrap());
        let observed = rx.recv_timeout(Duration::from_secs(2));
        drop(busy); // even the baseline failure releases and joins its worker
        worker.join().unwrap();
        observed
            .expect("fresh pressure admission unnecessarily scanned a locked shard")
            .unwrap();
        assert_eq!(reg.active_count(), 2);
        drop(reg);
        assert_eq!(reassembly_used(&budget), 0);
        for timeout in [Duration::ZERO, Duration::MAX] {
            let reg = ChunkRegistry::<()>::new(
                test_pool(),
                ChunkConfig {
                    assembler_timeout: timeout,
                    ..ChunkConfig::default()
                },
            );
            reg.gc_sweep();
            assert!(
                reg.gc_not_before
                    .lock()
                    .is_none_or(|bound| bound <= Instant::now())
            );
        }
    }

    #[test]
    fn finished_and_trimmed_backing_stays_charged() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 600, 4, 128, ()).unwrap();
        assert!(!reg.feed(1, 600, 0, &[0u8; 128]).unwrap());
        assert!(!reg.feed(1, 600, 1, &[1u8; 128]).unwrap());
        assert!(!reg.feed(1, 600, 2, &[2u8; 128]).unwrap());
        assert!(reg.feed(1, 600, 3, &[3u8; 64]).unwrap());
        let (assembler, ()) = reg.take(1, 600).unwrap();
        let finished = assembler.finish().unwrap();
        // Registry-level accounting dropped at finish…
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        // …but the budget charge for the full capacity stays live through the
        // trim and until the carrier is released.
        assert_eq!(reassembly_used(&budget), 4 * (128 + 1));
        assert_eq!(finished.len(), 3 * 128 + 64);
        drop(finished);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn held_backing_survives_registry_and_pool_handle_drop() {
        let (pool, budget) = budgeted_pool(1 << 20);
        let registry_pool_view = pool.clone();
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 610, 2, 256, ()).unwrap();
        assert!(!reg.feed(1, 610, 0, &[9u8; 256]).unwrap());
        assert!(reg.feed(1, 610, 1, &[8u8; 128]).unwrap());
        let (assembler, ()) = reg.take(1, 610).unwrap();
        let finished = assembler.finish().unwrap();

        // Registry and the outer pool Arc handle are gone (client/registry
        // shutdown). The carrier keeps the pool alive, the content readable,
        // and the charge held.
        drop(reg);
        drop(registry_pool_view);
        assert_eq!(reassembly_used(&budget), 512 + 2);
        let mut backing = finished;
        assert_eq!(backing.len(), 384);
        assert_eq!(&backing.copy_bytes().unwrap()[0..256], &[9u8; 256]);
        assert_eq!(&backing.copy_bytes().unwrap()[256..384], &[8u8; 128]);

        backing.release();
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn admission_rejects_before_mapping_with_cell_and_size_error() {
        let (pool, budget) = budgeted_pool(2 * 128 + 2);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        // Three payload chunks and their flags exceed the two-chunk budget.
        let err = reg.insert(1, 620, 3, 128, ()).unwrap_err().0.to_string();
        assert!(
            err.contains("'reassembly'"),
            "error must name the cell: {err}"
        );
        assert!(
            err.contains("387"),
            "error must name the requested size: {err}"
        );
        assert!(err.contains("258"), "error must name the limit: {err}");
        assert_eq!(reg.active_count(), 0);
        // No mapping was created and only rejection statistics moved.
        let stats = pool.read().stats();
        assert_eq!(stats.total_segments, 0);
        assert_eq!(stats.dedicated_segments, 0);
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.used_bytes, 0);
        assert_eq!(snap.reassembly.rejected_allocations, 1);
        assert_eq!(snap.reassembly.rejected_bytes, 384 + 3);

        // Zero geometry is rejected without any budget interaction.
        let err = reg.insert(1, 621, 1, 0, ()).unwrap_err().0.to_string();
        assert!(err.contains("chunk_size must be > 0"));
        let err = reg.insert(1, 622, 0, 128, ()).unwrap_err().0.to_string();
        assert!(err.contains("total_chunks must be > 0"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);

        // After one admitted assembly fills the cell, a second is rejected.
        reg.insert(1, 623, 2, 128, ()).unwrap();
        assert_eq!(reassembly_used(&budget), 256 + 2);
        let err = reg.insert(1, 624, 1, 128, ()).unwrap_err().0.to_string();
        assert!(err.contains("'reassembly'"));
        assert_eq!(reg.active_count(), 1);
        reg.abort(1, 623);
        assert_eq!(reassembly_used(&budget), 0);
    }

    #[test]
    fn shared_budget_bounds_independent_registries() {
        // Two registries over two pools sharing one MemoryBudget: concurrent
        // admissions cannot exceed the shared live bytes.
        let budget = MemoryBudget::new(1 << 20, 1 << 20, 4 * (256 + 2));
        let mk_registry = |tag: char| {
            let pool = Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    base_config(),
                    unique_prefix(&tag.to_string()),
                    budget.clone(),
                )
                .unwrap(),
            ));
            ChunkRegistry::new(pool, ChunkConfig::default())
        };
        let reg_a = mk_registry('a');
        let reg_b = mk_registry('b');

        // Exactly four 258-byte assemblies fit; the fifth is rejected no
        // matter which registry tries.
        reg_a.insert(1, 1, 2, 128, ()).unwrap(); // 258
        reg_b.insert(2, 1, 2, 128, ()).unwrap(); // 516
        reg_a.insert(1, 2, 2, 128, ()).unwrap(); // 774
        reg_b.insert(2, 2, 2, 128, ()).unwrap(); // 1032 = limit
        assert_eq!(reassembly_used(&budget), 4 * (256 + 2));
        assert!(reg_a.insert(1, 3, 1, 1, ()).is_err());
        assert!(reg_b.insert(2, 3, 1, 1, ()).is_err());
        // Releasing one admission frees room for exactly one more.
        reg_a.abort(1, 1);
        assert_eq!(reassembly_used(&budget), 3 * (256 + 2));
        reg_b.insert(2, 3, 2, 128, ()).unwrap();
        assert_eq!(reassembly_used(&budget), 4 * (256 + 2));
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

        const SEATS: usize = 4; // concurrent 516-byte assemblies that fit
        const ASSEMBLY_BYTES: usize = 512;
        const CHUNKS_PER_ASSEMBLY: usize = 4;
        const CHARGED_BYTES: u64 = (ASSEMBLY_BYTES + CHUNKS_PER_ASSEMBLY) as u64;
        const LIMIT: u64 = SEATS as u64 * CHARGED_BYTES;

        let budget = MemoryBudget::new(1 << 20, 1 << 20, LIMIT);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    segment_size: 256 * 1024,
                    min_block_size: 4096,
                    max_segments: 4,
                    max_dedicated_segments: 8,

                    buddy_idle_decay_secs: 0.0,
                    spill_threshold: 1.0,
                    spill_dir: std::env::temp_dir().join("c2_reg_test"),
                    ..PoolConfig::default()
                },
                unique_prefix("c"),
                budget.clone(),
            )
            .unwrap(),
        ));
        let reg = Arc::new(ChunkRegistry::new(pool, ChunkConfig::default()));

        // Phase 1: SEATS threads race to admit one assembly each and HOLD
        // it at a barrier. Exactly SEATS must succeed because the shared
        // cell fits exactly SEATS × 516 bytes.
        let admit_barrier = Arc::new(Barrier::new(SEATS));
        let mut handles = Vec::new();
        for t in 0..SEATS {
            let reg = reg.clone();
            let budget = budget.clone();
            let barrier = admit_barrier.clone();
            handles.push(thread::spawn(move || {
                reg.insert(t as u64, 1, 4, ASSEMBLY_BYTES / 4, ())
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
                let (err, ()) = reg
                    .insert((t + 100) as u64, 2, 4, ASSEMBLY_BYTES / 4, ())
                    .expect_err("a fully charged cell must reject");
                assert!(
                    err.to_string().contains("'reassembly'"),
                    "cell named: {err}"
                );
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(reassembly_used(&budget), LIMIT);
        assert_eq!(reg.active_count(), SEATS);
        let snap = budget.snapshot();
        assert_eq!(snap.reassembly.rejected_allocations, SEATS as u64);
        assert_eq!(snap.reassembly.rejected_bytes, SEATS as u64 * CHARGED_BYTES);
        assert!(snap.reassembly.peak_bytes <= LIMIT && snap.reassembly.peak_bytes > 0);

        // Phase 3: releasing every held admission refunds exactly once and
        // the cell admits again.
        for conn_id in held {
            reg.abort(conn_id, 1);
        }
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        reg.insert(200, 1, 4, ASSEMBLY_BYTES / 4, ())
            .expect("freed budget must admit again");
        assert_eq!(reassembly_used(&budget), CHARGED_BYTES);
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
                    reg.insert(conn_id, req_id, 1, 64, ()).unwrap();
                    reg.feed(conn_id, req_id, 0, &[conn_id as u8; 64]).unwrap();
                    let (assembler, ()) = reg.take(conn_id, req_id).unwrap();
                    let mut finished = assembler.finish().unwrap();
                    assert_eq!(finished.copy_bytes().unwrap(), &[conn_id as u8; 64]);
                    finished.release();
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
                    reg.insert(conn_id, req_id, 1, 64, ()).unwrap();
                    reg.feed(conn_id, req_id, 0, &[0u8; 64]).unwrap();
                    let (assembler, ()) = reg.take(conn_id, req_id).unwrap();
                    let mut finished = assembler.finish().unwrap();
                    finished.release();
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
    fn interleaved_requests_keep_content_and_charges_independent() {
        let (pool, budget) = budgeted_pool(2 * (1024 + 2));
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        for request_id in [100, 200] {
            reg.insert(42, request_id, 2, 512, ()).unwrap();
        }
        assert_eq!(reg.active_count(), 2);
        assert_eq!(reg.total_bytes(), 2 * (1024 + 2));
        assert_eq!(reassembly_used(&budget), 2 * (1024 + 2));
        assert!(!reg.feed(42, 100, 0, &[1; 512]).unwrap());
        assert!(!reg.feed(42, 200, 0, &[2; 512]).unwrap());
        assert!(reg.feed(42, 100, 1, &[3; 100]).unwrap());

        let (first, ()) = reg.take(42, 100).unwrap();
        let mut first = first.finish().unwrap();
        assert!(!reg.contains(42, 100));
        assert!(reg.contains(42, 200));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), 1024 + 2);
        assert_eq!(reassembly_used(&budget), 2 * (1024 + 2));
        let expected_first = [vec![1; 512], vec![3; 100]].concat();
        assert_eq!(first.len(), 612);
        assert_eq!(first.copy_bytes().unwrap(), expected_first);
        first.release();
        assert_eq!(reassembly_used(&budget), 1024 + 2);
        assert_eq!(pool.read().stats().alloc_count, 1);

        assert!(reg.feed(42, 200, 1, &[4; 200]).unwrap());
        let (second, ()) = reg.take(42, 200).unwrap();
        let mut second = second.finish().unwrap();
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 1024 + 2);
        let expected_second = [vec![2; 512], vec![4; 200]].concat();
        assert_eq!(second.len(), 712);
        assert_eq!(second.copy_bytes().unwrap(), expected_second);
        second.release();
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(pool.read().stats().alloc_count, 0);
    }

    // ── File-backing retention (spill_threshold = 0 forces file spill) ──

    /// Pool where every mapping-backed allocation is forced to FileSpill:
    /// `should_spill` returns true for any size when the threshold is 0.0,
    /// and the pool starts with no pre-created buddy segments.
    fn spill_pool() -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(
            MemPool::new_with_prefix(
                PoolConfig {
                    spill_threshold: 0.0,
                    ..base_config()
                },
                unique_prefix("s"),
            )
            .unwrap(),
        ))
    }

    fn budgeted_spill_pool(reassembly_limit: u64) -> (Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        (
            Arc::new(RwLock::new(
                MemPool::new_with_prefix_and_budget(
                    PoolConfig {
                        spill_threshold: 0.0,
                        ..base_config()
                    },
                    unique_prefix("t"),
                    budget.clone(),
                )
                .unwrap(),
            )),
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
        reg.insert(1, 700, 3, chunk_size, ()).unwrap();
        // The reassembly allocation itself must already be file-backed.
        assert_no_shm_mappings(&pool);

        // Feed out of order with a short final chunk: 2 (short), 0, 1.
        assert!(!reg.feed(1, 700, 2, &[2u8; 64]).unwrap());
        assert!(!reg.feed(1, 700, 0, &[0u8; 128]).unwrap());
        assert!(reg.feed(1, 700, 1, &[1u8; 128]).unwrap());

        let (assembler, ()) = reg.take(1, 700).unwrap();

        let mut finished = assembler.finish().unwrap();

        // The completed reassembly must keep its file backing — no promotion.
        assert!(finished.is_file_spill());
        // Logical length trimmed to actual data, not the full allocation.
        assert_eq!(finished.len(), 128 + 128 + 64);
        // Data integrity across the file-backed mapping.
        let slice = finished.copy_bytes().unwrap();
        assert_eq!(&slice[0..128], &[0u8; 128]);
        assert_eq!(&slice[128..256], &[1u8; 128]);
        assert_eq!(&slice[256..320], &[2u8; 64]);

        // Finishing created no buddy or dedicated mappings either.
        assert_no_shm_mappings(&pool);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);

        finished.release();
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn file_backed_charge_and_content_across_failure_and_hold_paths() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());

        // Held result: charge survives registry drop and release refunds once.
        reg.insert(1, 710, 2, 512, ()).unwrap();
        assert!(reg.contains(1, 710));
        assert!(!reg.feed(1, 710, 0, &[7u8; 512]).unwrap());
        assert!(reg.feed(1, 710, 1, &[6u8; 256]).unwrap());
        let (assembler, ()) = reg.take(1, 710).unwrap();
        let held = assembler.finish().unwrap();
        assert!(held.is_file_spill());
        assert_eq!(reassembly_used(&budget), 1024 + 2);
        drop(reg);
        let slice = held.copy_bytes().unwrap();
        assert_eq!(&slice[0..512], &[7u8; 512]);
        assert_eq!(&slice[512..768], &[6u8; 256]);
        let mut held = held;
        held.release();
        assert_eq!(reassembly_used(&budget), 0);

        // Abort path on a fresh file-backed registry.
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        reg.insert(1, 711, 2, 1024, ()).unwrap();
        reg.feed(1, 711, 0, &[7u8; 1024]).unwrap();
        reg.abort(1, 711);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);

        // A fresh file-backed assembly still works with real content.
        reg.insert(1, 712, 1, 1024, ()).unwrap();
        assert!(reg.feed(1, 712, 0, &[9u8; 1024]).unwrap());
        let (assembler, ()) = reg.take(1, 712).unwrap();
        let mut finished = assembler.finish().unwrap();
        assert!(finished.is_file_spill());
        assert_eq!(finished.copy_bytes().unwrap(), &[9u8; 1024]);
        finished.release();
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn finish_error_path_releases_file_backed_assembly() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = ChunkRegistry::new(pool, ChunkConfig::default());
        reg.insert(1, 720, 3, 512, ()).unwrap();
        // Feed only 1 of 3 chunks — finish must fail without leaking storage.
        reg.feed(1, 720, 0, &[3u8; 512]).unwrap();
        assert!(
            reg.take(1, 720)
                .and_then(|(assembler, ())| assembler.finish())
                .is_err()
        );
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(reassembly_used(&budget), 0);

        // A fresh assembly after the failed finish still works.
        reg.insert(1, 721, 1, 512, ()).unwrap();
        assert!(reg.feed(1, 721, 0, &[4u8; 512]).unwrap());
        let (assembler, ()) = reg.take(1, 721).unwrap();
        let mut finished = assembler.finish().unwrap();
        assert!(finished.is_file_spill());
        finished.release();
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
        reg.insert(1, 730, 2, 512, ()).unwrap();
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
    fn try_cleanup_file_releases_only_its_connection_during_another_owner_borrow() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = Arc::new(ChunkRegistry::new(pool, ChunkConfig::default()));
        reg.insert(1, 11, 1, 512, ()).unwrap();
        reg.feed(1, 11, 0, &[11; 512]).unwrap();
        let (assembler, ()) = reg.take(1, 11).unwrap();
        let held = assembler.finish().unwrap();
        reg.insert(1, 22, 2, 1024, ()).unwrap();
        reg.feed(1, 22, 0, &[22; 1024]).unwrap();
        let other_conn = 1 + SHARD_COUNT as u64; // share a shard, not an owner
        reg.insert(other_conn, 22, 1, 1024, ()).unwrap();
        reg.feed(other_conn, 22, 0, &[33; 1024]).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = thread::spawn(move || {
            held.with_slice(|bytes| {
                assert_eq!(bytes, &[11; 512]);
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
            held
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!reg.try_cleanup_connection_until(1, Instant::now()));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_reg = Arc::clone(&reg);
        let worker = thread::spawn(move || {
            let closed = worker_reg.try_cleanup_connection(1);
            let aborted = worker_reg.try_abort(1, 22);
            done_tx.send((closed, aborted)).unwrap();
        });
        let completed = done_rx.recv_timeout(Duration::from_secs(2));
        let locked_phase = completed.as_ref().ok().map(|_| {
            (
                budget.snapshot(),
                reg.active_count(),
                reg.total_bytes(),
                reg.contains(1, 22),
            )
        });
        let _ = release_tx.send(());
        let mut held = holder.join().unwrap();
        worker.join().unwrap();
        let (closed, aborted) =
            completed.expect("try_cleanup_connection/try_abort blocked on live owner");
        let (live, count, bytes, assembly_retained) = locked_phase.unwrap();
        assert!(closed && aborted);
        assert_eq!(count, 1);
        assert_eq!(bytes, 1024 + 1);
        assert!(!assembly_retained);
        assert_eq!(live.file.used_bytes, 1024 + 512);
        assert_eq!(live.reassembly.used_bytes, (1024 + 1) + (512 + 1));
        assert!(reg.try_cleanup_connection(1));
        assert!(!reg.contains(1, 22));
        assert!(reg.contains(other_conn, 22));
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), 1024 + 1);
        assert_eq!(
            budget.snapshot().reassembly.used_bytes,
            (1024 + 1) + (512 + 1)
        );
        let (assembler, ()) = reg.take(other_conn, 22).unwrap();
        let mut other = assembler.finish().unwrap();
        assert_eq!(other.copy_bytes().unwrap(), &[33; 1024]);
        other.release();
        assert_eq!(held.copy_bytes().unwrap(), &[11; 512]);
        held.release();
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    }

    #[test]
    fn try_gc_skips_busy_shard_but_another_backing_borrow_does_not_block_it() {
        let (pool, budget) = budgeted_spill_pool(1 << 20);
        let reg = Arc::new(ChunkRegistry::new(
            Arc::clone(&pool),
            ChunkConfig {
                assembler_timeout: Duration::ZERO,
                ..ChunkConfig::default()
            },
        ));
        reg.insert(1, 11, 1, 512, ()).unwrap();
        reg.feed(1, 11, 0, &[11; 512]).unwrap();
        let (assembler, ()) = reg.take(1, 11).unwrap();
        let held = assembler.finish().unwrap();
        reg.insert(1, 22, 2, 1024, ()).unwrap();
        reg.insert(2, 33, 2, 1024, ()).unwrap();
        let (callback_ready_tx, callback_ready_rx) = std::sync::mpsc::channel();
        let (callback_release_tx, callback_release_rx) = std::sync::mpsc::channel();
        let callback_holder = thread::spawn(move || {
            held.with_slice(|bytes| {
                assert_eq!(bytes, &[11; 512]);
                callback_ready_tx.send(()).unwrap();
                callback_release_rx.recv().unwrap();
            })
            .unwrap();
            held
        });
        let (shard_ready_tx, shard_ready_rx) = std::sync::mpsc::channel();
        let (shard_release_tx, shard_release_rx) = std::sync::mpsc::channel();
        let holder_reg = Arc::clone(&reg);
        let shard_holder = thread::spawn(move || {
            let _shard = holder_reg.shard(2).lock();
            shard_ready_tx.send(()).unwrap();
            shard_release_rx.recv().unwrap();
        });
        callback_ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        shard_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let charged = budget.snapshot();
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        let first_reg = Arc::clone(&reg);
        let first_worker = thread::spawn(move || {
            first_tx.send(first_reg.try_gc_sweep()).unwrap();
        });
        let first_result = first_rx.recv_timeout(Duration::from_secs(2));
        let live = first_result.as_ref().ok().map(|_| budget.snapshot());
        if first_result.is_err() {
            let _ = callback_release_tx.send(());
            let _ = shard_release_tx.send(());
            callback_holder.join().unwrap();
            shard_holder.join().unwrap();
            first_worker.join().unwrap();
            panic!("first try_gc_sweep blocked on live callback or shard");
        }
        let skipped = first_result.expect("first try_gc_sweep blocked on live callback or shard");
        let live = live.unwrap();
        first_worker.join().unwrap();
        let _ = callback_release_tx.send(());
        let mut held = callback_holder.join().unwrap();
        // Shard 1 can now release, while shard 2 is still inaccessible.
        let (second_tx, second_rx) = std::sync::mpsc::channel();
        let second_reg = Arc::clone(&reg);
        let second_worker = thread::spawn(move || {
            second_tx.send(second_reg.try_gc_sweep()).unwrap();
        });
        let second_result = second_rx.recv_timeout(Duration::from_secs(2));
        if second_result.is_err() {
            let _ = shard_release_tx.send(());
            shard_holder.join().unwrap();
            second_worker.join().unwrap();
            panic!("second try_gc_sweep blocked on live shard");
        }
        let released_one = second_result.expect("second try_gc_sweep blocked on live shard");
        second_worker.join().unwrap();
        let _ = shard_release_tx.send(());
        shard_holder.join().unwrap();
        assert_eq!(skipped.expired, 1);
        assert_eq!(skipped.freed_bytes, 2048 + 2);
        assert_eq!(skipped.remaining, 1);
        assert_eq!(live.file.used_bytes, charged.file.used_bytes - 2048);
        assert_eq!(
            live.reassembly.used_bytes,
            charged.reassembly.used_bytes - (2048 + 2)
        );
        assert_eq!(released_one.expired, 0);
        assert_eq!(released_one.freed_bytes, 0);
        assert_eq!(released_one.remaining, 1);
        let released_other = reg.try_gc_sweep();
        assert_eq!(released_other.expired, 1);
        assert_eq!(released_other.remaining, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 512 + 1);
        assert_eq!(held.copy_bytes().unwrap(), &[11; 512]);
        held.release();
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
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
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    spill_threshold: 0.0,
                    spill_dir: spill_dir.clone(),
                    ..base_config()
                },
                unique_prefix("r"),
                budget.clone(),
            )
            .unwrap(),
        ));
        let before: Vec<std::path::PathBuf> = std::fs::read_dir(&spill_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();

        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let err = reg.insert(1, 740, 2, 512, ()).unwrap_err().0.to_string();
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

    fn typed_file_registry(
        live_limit: u64,
        file_limit: u64,
    ) -> (Arc<ChunkRegistry<()>>, Arc<RwLock<MemPool>>, MemoryBudget) {
        let budget = MemoryBudget::new(0, file_limit, live_limit);
        let pool = Arc::new(RwLock::new(
            MemPool::new_with_prefix_and_budget(
                PoolConfig {
                    buddy_enabled: false,
                    spill_threshold: 0.0,
                    ..base_config()
                },
                unique_prefix("typed"),
                budget.clone(),
            )
            .unwrap(),
        ));
        (
            Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default())),
            pool,
            budget,
        )
    }

    fn raw_reply(
        reg: &ChunkRegistry<()>,
        rid: u64,
        size: u64,
        chunks: u32,
        data_len: usize,
    ) -> Result<(), ChunkAdmissionError> {
        use crate::chunk::header::{decode_reply_chunk_meta, encode_reply_chunk_meta};
        let mut bytes = encode_reply_chunk_meta(size, chunks, 0).to_vec();
        bytes.resize(bytes.len() + data_len, 0x42);
        let (size, chunks, index, consumed) = decode_reply_chunk_meta(&bytes, 0).unwrap();
        assert_eq!(index, 0);
        reg.insert_reply(1, rid, size, chunks as usize, bytes.len() - consumed)
    }

    #[test]
    fn typed_raw_geometry_rejection_never_reserves() {
        let (reg, pool, budget) = typed_file_registry(0, 0);
        for (size, chunks, len) in [
            (128, 0, 64),
            (0, 1, 1),
            (0, 2, 64),
            (128, 2, 0),
            (128, 1, 0),
        ] {
            let error = raw_reply(&reg, 1, size, chunks, len).unwrap_err();
            assert!(
                matches!(error, ChunkAdmissionError::Protocol(_)),
                "{error:?}"
            );
        }
        let (overflow, ()) = reg.insert(1, 1, 2, usize::MAX, ()).unwrap_err();
        assert!(matches!(overflow, ChunkAdmissionError::Protocol(_)));
        assert!(overflow.to_string().contains("geometry overflow"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(reg.active_count(), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn reassembly_byte_limit_is_a_size_error_before_reservation() {
        let (_default_reg, pool, budget) = typed_file_registry(0, 0);
        let reg = ChunkRegistry::new(
            pool.clone(),
            ChunkConfig {
                max_reassembly_bytes: 100,
                ..ChunkConfig::default()
            },
        );
        let error = raw_reply(&reg, 1, 128, 2, 64).unwrap_err();
        assert!(matches!(error, ChunkAdmissionError::Capacity(_)));
        assert_eq!(error.to_string(), "reassembly size 128 exceeds limit 100");
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
        assert_eq!(reg.active_count(), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_budget_capacity_rejection_creates_no_mapping() {
        let (reg, pool, budget) = typed_file_registry(0, 4096);
        let error = raw_reply(&reg, 1, 128, 2, 64).unwrap_err();
        assert!(matches!(error, ChunkAdmissionError::Capacity(_)));
        assert!(error.to_string().contains("'reassembly'"));
        assert!(error.to_string().contains("130"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);
        assert_eq!(budget.snapshot().reassembly.rejected_bytes, 130);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(reg.active_count(), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn flags_are_included_in_the_admission_budget() {
        let (reg, pool, budget) = typed_file_registry(128, 4096);
        let error = raw_reply(&reg, 1, 128, 2, 64).unwrap_err();
        assert!(matches!(error, ChunkAdmissionError::Capacity(_)));
        assert!(error.to_string().contains("'reassembly'"));
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.reassembly.rejected_allocations, 1);
        assert_eq!(snapshot.reassembly.rejected_bytes, 130);
        assert_eq!(snapshot.reassembly.used_bytes, 0);
        assert_eq!(snapshot.file.rejected_allocations, 0);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn enormous_chunk_count_is_rejected_before_flags_or_backing_allocation() {
        let (_default_reg, pool, budget) = typed_file_registry(u32::MAX as u64, 0);
        let reg = ChunkRegistry::<()>::new(
            pool.clone(),
            ChunkConfig {
                max_reassembly_bytes: u64::MAX,
                ..ChunkConfig::default()
            },
        );
        let error = reg
            .insert_reply(1, 1, u64::MAX, u32::MAX as usize, 1)
            .unwrap_err();
        assert!(matches!(error, ChunkAdmissionError::Capacity(_)));
        assert!(error.to_string().contains("'reassembly'"));
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.reassembly.rejected_allocations, 1);
        assert_eq!(snapshot.reassembly.rejected_bytes, 2 * u32::MAX as u64);
        assert_eq!(snapshot.reassembly.used_bytes, 0);
        assert_eq!(snapshot.reassembly.peak_bytes, 0);
        assert_eq!(snapshot.file.rejected_allocations, 0);
        assert_eq!(snapshot.file.used_bytes, 0);
        assert_eq!(snapshot.file.peak_bytes, 0);
        assert_eq!(snapshot.shm.used_bytes, 0);
        assert_eq!(snapshot.shm.peak_bytes, 0);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_backing_capacity_failure_refunds_reservation() {
        let (reg, pool, budget) = typed_file_registry(1024, 0);
        let error = raw_reply(&reg, 1, 128, 2, 64).unwrap_err();
        assert!(matches!(error, ChunkAdmissionError::Capacity(_)));
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.reassembly.peak_bytes, 128 + 2);
        assert_eq!(snapshot.reassembly.used_bytes, 0);
        assert_eq!(snapshot.reassembly.rejected_allocations, 0);
        assert_eq!(snapshot.file.used_bytes, 0);
        assert_eq!(snapshot.file.rejected_allocations, 1);
        assert_eq!(reg.active_count(), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_duplicate_at_full_budget_is_protocol_without_reservation() {
        let (reg, pool, budget) = typed_file_registry(128 + 2, 4096);
        raw_reply(&reg, 11, 128, 2, 64).unwrap();
        reg.feed(1, 11, 0, &[0x11; 64]).unwrap();
        assert_eq!(reassembly_used(&budget), 128 + 2);
        let error = raw_reply(&reg, 11, 128, 2, 64).unwrap_err();
        assert!(matches!(
            error,
            ChunkAdmissionError::Duplicate {
                conn_id: 1,
                request_id: 11
            }
        ));
        assert!(error.to_string().contains("duplicate"));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reg.total_bytes(), 128 + 2);
        // An unrelated RID is a genuine capacity rejection, not a duplicate.
        assert!(matches!(
            raw_reply(&reg, 12, 128, 2, 64),
            Err(ChunkAdmissionError::Capacity(_))
        ));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);
        assert_eq!(reg.gc_sweep().expired, 0);
        assert!(matches!(
            raw_reply(&reg, 11, 128, 2, 64),
            Err(ChunkAdmissionError::Duplicate { .. })
        ));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);
        assert!(reg.feed(1, 11, 1, &[0x22; 64]).unwrap());
        let (assembler, ()) = reg.take(1, 11).unwrap();
        let mut finished = assembler.finish().unwrap();
        let bytes = finished.copy_bytes().unwrap();
        assert_eq!(&bytes[..64], &[0x11; 64]);
        assert_eq!(&bytes[64..], &[0x22; 64]);
        finished.release();
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_gc_removal_makes_same_key_a_fresh_capacity_decision() {
        let (reg, pool, budget) = typed_file_registry(2 * (64 + 1), 4096);
        raw_reply(&reg, 11, 64, 1, 64).unwrap();
        raw_reply(&reg, 22, 64, 1, 64).unwrap();
        assert!(reg.contains(1, 11)); // A caller's old snapshot is now deliberately stale.
        reg.shard(1)
            .lock()
            .get_mut(&(1, 11))
            .unwrap()
            .contents
            .lock()
            .last_activity = Instant::now() - Duration::from_secs(61);
        assert_eq!(reg.gc_sweep().expired, 1);
        raw_reply(&reg, 33, 64, 1, 64).unwrap(); // Another RID consumes the refunded space.
        let error = raw_reply(&reg, 11, 64, 1, 64).unwrap_err();
        assert!(
            matches!(error, ChunkAdmissionError::Capacity(_)),
            "{error:?}"
        );
        assert!(!reg.contains(1, 11));
        assert!(reg.contains(1, 22));
        assert!(reg.contains(1, 33));
        assert_eq!(reg.active_count(), 2);
        assert_eq!(reassembly_used(&budget), 2 * (64 + 1));
        reg.cleanup_connection(1);
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_gc_and_duplicate_are_rechecked_after_pool_contention() {
        let (reg, pool, budget) = typed_file_registry(2 * (64 + 1), 4096);
        raw_reply(&reg, 11, 64, 1, 64).unwrap();
        raw_reply(&reg, 22, 64, 1, 64).unwrap();
        reg.shard(1)
            .lock()
            .get_mut(&(1, 11))
            .unwrap()
            .contents
            .lock()
            .last_activity = Instant::now() - Duration::from_secs(61);
        // Gate only after production insert released its shard on contention.
        // Neither the GC result nor the eventual admission cause is mocked.
        let gate = Arc::new(std::sync::Barrier::new(2));
        let waiting_gate = gate.clone();
        let (entered, observed) = std::sync::mpsc::channel();
        let once = std::sync::atomic::AtomicBool::new(false);
        *reg.admission_wait_probe.lock() = Some(Arc::new(move || {
            if !once.swap(true, Ordering::Relaxed) {
                entered.send(()).unwrap();
                waiting_gate.wait();
            }
        }));
        let pool_guard = pool.write();
        let inserting = reg.clone();
        let insert_thread = std::thread::spawn(move || raw_reply(&inserting, 33, 64, 1, 64));
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        let sweeping = reg.clone();
        let gc_thread = std::thread::spawn(move || sweeping.gc_sweep());
        let deadline = Instant::now() + Duration::from_secs(2);
        while reg.contains(1, 11) {
            assert!(
                Instant::now() < deadline,
                "GC could not remove the expired entry during admission wait"
            );
            std::thread::yield_now();
        }
        assert_eq!(reg.active_count(), 1);
        // Final storage release uses its fixed owner and can finish while
        // admission is waiting for the unrelated public pool write guard.
        assert_eq!(gc_thread.join().unwrap().expired, 1);
        assert_eq!(reassembly_used(&budget), 64 + 1);
        drop(pool_guard);
        assert_eq!(reassembly_used(&budget), 64 + 1);
        // Another actual insert publishes the waiting key and fills the budget.
        // Retrying insert must now see Duplicate, without attempting a charge.
        raw_reply(&reg, 33, 64, 1, 64).unwrap();
        gate.wait();
        assert!(matches!(
            insert_thread.join().unwrap(),
            Err(ChunkAdmissionError::Duplicate {
                conn_id: 1,
                request_id: 33
            })
        ));
        assert!(reg.contains(1, 22));
        assert!(reg.contains(1, 33));
        assert_eq!(reg.active_count(), 2);
        assert_eq!(reg.total_bytes(), 2 * (64 + 1));
        assert_eq!(reassembly_used(&budget), 2 * (64 + 1));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
        reg.cleanup_connection(1);
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);
    }

    #[test]
    fn typed_review_oversized_first_chunk_is_protocol_before_capacity() {
        for (live, file) in [(0, 0), (1024, 4096)] {
            let (reg, pool, budget) = typed_file_registry(live, file);
            let error = raw_reply(&reg, 1, 128, 1, 129).unwrap_err();
            assert!(
                matches!(error, ChunkAdmissionError::Protocol(_)),
                "{error:?}"
            );
            assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
            assert_eq!(budget.snapshot().reassembly.peak_bytes, 0);
            assert_eq!(reg.active_count(), 0);
            assert_no_shm_mappings(&pool);
        }
        // The exact legal length is still a real capacity failure with zero budget.
        let (reg, _pool, budget) = typed_file_registry(0, 0);
        assert!(matches!(
            raw_reply(&reg, 1, 128, 1, 128),
            Err(ChunkAdmissionError::Capacity(_))
        ));
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 1);
    }

    #[test]
    fn file_carrier_borrow_allows_same_shard_admission_and_registry_reentry() {
        let (reg, pool, budget) = typed_file_registry(1024, 4096);
        raw_reply(&reg, 11, 64, 1, 64).unwrap();
        assert!(reg.feed(1, 11, 0, &[0x11; 64]).unwrap());
        let (assembler, ()) = reg.take(1, 11).unwrap();
        let carrier = assembler.finish().unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        let reader_entry = entered.clone();
        let reader_resume = resume.clone();
        let querying = reg.clone();
        let reader = thread::spawn(move || {
            let copied = carrier
                .with_slice(|bytes| {
                    reader_entry.wait();
                    reader_resume.wait();
                    assert!(querying.contains(1, 22));
                    bytes.to_vec()
                })
                .unwrap();
            assert_eq!(copied, &[0x11; 64]);
            carrier
        });
        entered.wait();
        let (done, observed) = std::sync::mpsc::channel();
        let inserting = reg.clone();
        let writer = thread::spawn(move || {
            inserting.insert(1, 22, 1, 64, ()).unwrap();
            done.send(()).unwrap();
        });
        let progressed = observed.recv_timeout(Duration::from_secs(2));
        resume.wait();
        let mut carrier = reader.join().unwrap();
        writer.join().unwrap();
        progressed.expect("admission blocked behind another carrier borrow");
        assert_eq!(reg.active_count(), 1);
        assert_eq!(reassembly_used(&budget), 2 * (64 + 1));
        reg.abort(1, 22);
        assert_eq!(reassembly_used(&budget), 64 + 1);
        carrier.release();
        assert_eq!(reassembly_used(&budget), 0);
        assert_no_shm_mappings(&pool);
    }
}
