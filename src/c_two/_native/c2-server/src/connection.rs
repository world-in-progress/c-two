//! Per-client connection state.
//!
//! Mirrors the Python `Connection` dataclass from
//! `c_two.transport.server.connection` — tracks handshake status,
//! SHM segments, activity timestamps, and in-flight request counting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use tokio::sync::Notify;
use tracing::warn;

use c2_mem::config::PoolConfig;
use c2_mem::{FreeResult, MemPool};
use c2_wire::assembler::ChunkAssembler;

// ---------------------------------------------------------------------------
// Peer SHM state (set once during handshake, read concurrently)
// ---------------------------------------------------------------------------

struct PeerShmState {
    prefix: String,
    segment_names: Vec<String>,
    segment_sizes: Vec<u32>,
    /// Buddy segment size for lazy-open derivation.
    buddy_segment_size: usize,
    /// MemPool opened over the client's SHM segments for data access + free.
    pool: Option<Arc<RwLock<MemPool>>>,
}

impl PeerShmState {
    fn new() -> Self {
        Self {
            prefix: String::new(),
            segment_names: Vec::new(),
            segment_sizes: Vec::new(),
            buddy_segment_size: 256 * 1024 * 1024,
            pool: None,
        }
    }

    /// Derive buddy segment SHM name from prefix and index.
    fn buddy_segment_name(prefix: &str, idx: usize) -> String {
        format!("{}_{}{:04x}", prefix, "b", idx)
    }

    /// Derive dedicated segment SHM name from prefix and index.
    fn dedicated_segment_name(prefix: &str, idx: u32) -> String {
        format!("{}_{}{:04x}", prefix, "d", idx)
    }

    /// Ensure the pool has the buddy segment at `seg_idx` open.
    /// If not yet open, derive the name from prefix and lazy-open it.
    fn ensure_buddy_segment(&self, seg_idx: u32) -> Result<(), String> {
        let pool_arc = self.pool.as_ref().ok_or("peer pool not initialised")?;
        // Fast path: check segment count under read lock
        {
            let pool = pool_arc.read().unwrap();
            if (seg_idx as usize) < pool.segment_count() {
                return Ok(());
            }
        }
        // Slow path: open missing segments under write lock
        let mut pool = pool_arc.write().unwrap();
        // Re-check after upgrading (another thread may have opened it)
        let idx = seg_idx as usize;
        if idx < pool.segment_count() {
            return Ok(());
        }
        for i in pool.segment_count()..=idx {
            let name = Self::buddy_segment_name(&self.prefix, i);
            pool.open_segment(&name, self.buddy_segment_size)?;
        }
        Ok(())
    }

    /// Ensure a dedicated segment is open at the specific producer index.
    // TODO: add read-check-first optimisation once MemPool exposes a
    // `has_dedicated(seg_idx)` predicate to avoid unconditional write-lock.
    fn ensure_dedicated_segment(&self, seg_idx: u32, min_size: usize) -> Result<(), String> {
        let pool_arc = self.pool.as_ref().ok_or("peer pool not initialised")?;
        let mut pool = pool_arc.write().unwrap();
        let name = Self::dedicated_segment_name(&self.prefix, seg_idx);
        pool.open_dedicated_at(seg_idx, &name, min_size)
    }
}

/// Per-client connection state.
pub struct Connection {
    conn_id: u64,
    handshake_done: AtomicBool,
    chunked_capable: AtomicBool,

    /// Client's SHM pool state — prefix, segment names/sizes, opened MemPool.
    peer_shm: Mutex<PeerShmState>,

    /// Monotonic timestamp of the last frame received/sent.
    last_activity: Mutex<Instant>,

    /// Number of requests currently being processed.
    inflight: AtomicI32,

    /// Notified whenever `inflight` drops to zero.
    idle_notify: Notify,

    /// In-progress chunked reassembly state, keyed by request_id.
    assemblers: Mutex<HashMap<u64, ChunkAssembler>>,
}

impl Connection {
    /// Create a new connection with the given `conn_id`.
    ///
    /// All fields start at their default/empty state; `last_activity` is set
    /// to the current instant.
    pub fn new(conn_id: u64) -> Self {
        Self {
            conn_id,
            handshake_done: AtomicBool::new(false),
            chunked_capable: AtomicBool::new(false),
            peer_shm: Mutex::new(PeerShmState::new()),
            last_activity: Mutex::new(Instant::now()),
            inflight: AtomicI32::new(0),
            idle_notify: Notify::new(),
            assemblers: Mutex::new(HashMap::new()),
        }
    }

    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    pub fn handshake_done(&self) -> bool {
        self.handshake_done.load(Ordering::Relaxed)
    }

    pub fn set_handshake_done(&self, v: bool) {
        self.handshake_done.store(v, Ordering::Relaxed);
    }

    pub fn chunked_capable(&self) -> bool {
        self.chunked_capable.load(Ordering::Relaxed)
    }

    pub fn set_chunked_capable(&self, v: bool) {
        self.chunked_capable.store(v, Ordering::Relaxed);
    }

    // -- peer SHM accessors -------------------------------------------------

    /// Get a shared reference to the peer's MemPool (for ShmBuffer construction).
    /// Returns None if handshake hasn't completed yet.
    pub fn peer_pool_arc(&self) -> Option<Arc<RwLock<MemPool>>> {
        let state = self.peer_shm.lock().unwrap();
        state.pool.clone()
    }

    /// Return the peer's SHM pool prefix (empty if handshake not done).
    pub fn peer_prefix(&self) -> String {
        self.peer_shm.lock().unwrap().prefix.clone()
    }

    /// Return cloned list of remote segment names.
    pub fn remote_segment_names(&self) -> Vec<String> {
        self.peer_shm.lock().unwrap().segment_names.clone()
    }

    /// Return cloned list of remote segment sizes.
    pub fn remote_segment_sizes(&self) -> Vec<u32> {
        self.peer_shm.lock().unwrap().segment_sizes.clone()
    }

    /// Initialise peer SHM state from handshake data.
    ///
    /// Always creates a `MemPool` with the peer's prefix so that later
    /// `read_peer_data` / `free_peer_block` can lazy-open segments by
    /// deriving their names from `{prefix}_b{idx:04x}`.
    pub fn init_peer_shm(&self, prefix: String, segments: Vec<(String, u32)>) {
        let mut state = self.peer_shm.lock().unwrap();
        state.prefix = prefix;
        state.segment_names = segments.iter().map(|(n, _)| n.clone()).collect();
        state.segment_sizes = segments.iter().map(|(_, s)| *s).collect();

        // Derive buddy segment size from first segment, or keep default.
        if let Some(&(_, size)) = segments.first() {
            state.buddy_segment_size = size as usize;
        }

        let cfg = PoolConfig {
            segment_size: state.buddy_segment_size,
            min_block_size: 4096,
            max_segments: 16,
            max_dedicated_segments: 4,
            dedicated_gc_delay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::path::PathBuf::from("/tmp/c_two_spill_srv"),
        };
        let peer_prefix = state.prefix.clone();
        let mut pool = MemPool::new_with_prefix(cfg, peer_prefix);

        // Eagerly open any segments declared in the handshake.
        for (name, size) in &segments {
            if let Err(e) = pool.open_segment(name, *size as usize) {
                warn!(
                    conn_id = self.conn_id,
                    segment = name.as_str(),
                    "failed to open peer segment: {e}"
                );
            }
        }
        state.pool = Some(Arc::new(RwLock::new(pool)));
    }

    /// Read `data_size` bytes from the peer's SHM at `(seg_idx, offset)`.
    ///
    /// Lazy-opens the segment if it hasn't been seen before — the name is
    /// derived deterministically from `{prefix}_b{idx:04x}`.
    pub fn read_peer_data(
        &self,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<Vec<u8>, String> {
        let state = self.peer_shm.lock().unwrap();
        // Lazy-open the segment if the pool hasn't mapped it yet.
        if is_dedicated {
            state.ensure_dedicated_segment(seg_idx as u32, data_size as usize)?;
        } else {
            state.ensure_buddy_segment(seg_idx as u32)?;
        }
        let pool_arc = state.pool.as_ref().ok_or("peer pool not initialised")?;
        let pool = pool_arc.read().unwrap();
        let ptr = pool.data_ptr_at(seg_idx as u32, offset, is_dedicated)?;
        let slice = unsafe { std::slice::from_raw_parts(ptr, data_size as usize) };
        Ok(slice.to_vec())
    }

    /// Free a buddy block in the peer's SHM pool.
    ///
    /// Lazy-opens the segment if not yet mapped.
    /// Returns `FreeResult` so callers can trigger deferred GC on idle segments.
    pub fn free_peer_block(
        &self,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> FreeResult {
        let state = self.peer_shm.lock().unwrap();
        // Lazy-open the segment before freeing.
        let lazy_res = if is_dedicated {
            state.ensure_dedicated_segment(seg_idx as u32, data_size as usize)
        } else {
            state.ensure_buddy_segment(seg_idx as u32)
        };
        if let Err(e) = lazy_res {
            warn!(
                conn_id = self.conn_id,
                seg_idx,
                "lazy-open for free failed: {e}"
            );
            return FreeResult::Normal;
        }
        if let Some(pool_arc) = state.pool.as_ref() {
            let mut pool = pool_arc.write().unwrap();
            match pool.free_at(seg_idx as u32, offset, data_size, is_dedicated) {
                Ok(result) => return result,
                Err(e) => {
                    warn!(
                        conn_id = self.conn_id,
                        seg_idx,
                        offset,
                        "peer free_at failed: {e}"
                    );
                }
            }
        }
        FreeResult::Normal
    }

    /// Run buddy GC on the peer's SHM pool — reclaim idle trailing segments.
    pub fn gc_peer_buddy(&self) {
        let state = self.peer_shm.lock().unwrap();
        if let Some(pool_arc) = state.pool.as_ref() {
            let mut pool = pool_arc.write().unwrap();
            pool.gc_buddy();
        }
    }

    // -- chunk assembler management -----------------------------------------

    /// Insert a new chunk assembler for the given `request_id`.
    pub fn insert_assembler(&self, request_id: u64, asm: ChunkAssembler) {
        self.assemblers.lock().unwrap().insert(request_id, asm);
    }

    /// Feed a chunk into the assembler for `request_id`.
    ///
    /// Returns `Ok(true)` when all chunks are received, `Ok(false)` otherwise.
    /// Returns `Err` if no assembler exists for this request or on data error.
    pub fn feed_chunk(
        &self,
        request_id: u64,
        pool: &MemPool,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String> {
        let mut map = self.assemblers.lock().unwrap();
        let asm = map
            .get_mut(&request_id)
            .ok_or_else(|| format!("no assembler for request {request_id}"))?;
        asm.feed_chunk(pool, chunk_idx, data)
    }

    /// Remove and return the completed assembler for `request_id`.
    pub fn take_assembler(&self, request_id: u64) -> Option<ChunkAssembler> {
        self.assemblers.lock().unwrap().remove(&request_id)
    }

    /// Record activity — updates the last-activity timestamp to *now*.
    pub fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    /// Seconds elapsed since the last `touch()`.
    pub fn idle_seconds(&self) -> f64 {
        self.last_activity
            .lock()
            .unwrap()
            .elapsed()
            .as_secs_f64()
    }

    /// Mark a new request as in-flight.
    pub fn flight_inc(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a request as completed.  If the in-flight counter reaches zero,
    /// all waiters on [`wait_idle`](Self::wait_idle) are notified.
    pub fn flight_dec(&self) {
        let prev = self.inflight.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Counter went 1 → 0: connection is idle.
            self.idle_notify.notify_waiters();
        }
    }

    /// Current number of in-flight requests.
    pub fn inflight_count(&self) -> i32 {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Wait until all in-flight requests have completed (counter == 0).
    ///
    /// Returns immediately if already idle.
    pub async fn wait_idle(&self) {
        // Fast path: already idle.
        if self.inflight.load(Ordering::Acquire) == 0 {
            return;
        }
        // Slow path: wait for notification from `flight_dec`.
        loop {
            let notified = self.idle_notify.notified();
            // Re-check after registering the future to avoid missed wakeups.
            if self.inflight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
            if self.inflight.load(Ordering::Acquire) == 0 {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn new_connection_defaults() {
        let conn = Connection::new(42);
        assert_eq!(conn.conn_id(), 42);
        assert!(!conn.handshake_done());
        assert!(!conn.chunked_capable());
        assert!(conn.peer_prefix().is_empty());
        assert!(conn.remote_segment_names().is_empty());
        assert!(conn.remote_segment_sizes().is_empty());
        assert_eq!(conn.inflight_count(), 0);
    }

    #[test]
    fn init_peer_shm_stores_state() {
        let conn = Connection::new(1);
        conn.init_peer_shm(
            "/cc3b_test".into(),
            vec![("seg0".into(), 4096), ("seg1".into(), 8192)],
        );
        assert_eq!(conn.peer_prefix(), "/cc3b_test");
        assert_eq!(conn.remote_segment_names(), vec!["seg0", "seg1"]);
        assert_eq!(conn.remote_segment_sizes(), vec![4096, 8192]);
    }

    #[test]
    fn init_peer_shm_empty_segments() {
        let conn = Connection::new(2);
        conn.init_peer_shm("prefix".into(), vec![]);
        assert_eq!(conn.peer_prefix(), "prefix");
        assert!(conn.remote_segment_names().is_empty());
    }

    #[test]
    fn touch_updates_activity() {
        let conn = Connection::new(1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let idle_before = conn.idle_seconds();
        assert!(idle_before >= 0.015, "idle_before={idle_before}");
        conn.touch();
        let idle_after = conn.idle_seconds();
        assert!(
            idle_after < idle_before,
            "touch should reset idle: before={idle_before}, after={idle_after}"
        );
    }

    #[test]
    fn idle_seconds_increases() {
        let conn = Connection::new(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let idle = conn.idle_seconds();
        assert!(idle >= 0.04, "idle={idle}");
    }

    #[test]
    fn flight_inc_dec_tracking() {
        let conn = Connection::new(1);
        assert_eq!(conn.inflight_count(), 0);
        conn.flight_inc();
        assert_eq!(conn.inflight_count(), 1);
        conn.flight_inc();
        assert_eq!(conn.inflight_count(), 2);
        conn.flight_dec();
        assert_eq!(conn.inflight_count(), 1);
        conn.flight_dec();
        assert_eq!(conn.inflight_count(), 0);
    }

    #[tokio::test]
    async fn wait_idle_returns_immediately_when_idle() {
        let conn = Connection::new(1);
        // No in-flight requests — should return instantly.
        conn.wait_idle().await;
    }

    #[tokio::test]
    async fn wait_idle_resolves_on_flight_dec() {
        let conn = Arc::new(Connection::new(1));
        conn.flight_inc();

        let conn2 = Arc::clone(&conn);
        let handle = tokio::spawn(async move {
            conn2.wait_idle().await;
        });

        // Give the waiter a moment to register.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        conn.flight_dec();

        // Should resolve quickly.
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("timed out waiting for wait_idle")
            .expect("task panicked");
    }
}
