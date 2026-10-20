//! Per-client connection state.
//!
//! Tracks handshake status, the peer SHM cache, activity timestamps, and
//! in-flight request counting for one IPC peer.

use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

use tokio::sync::Notify;
use tracing::warn;

use c2_config::MAX_IPC_POOL_SEGMENTS;
use c2_mem::config::PoolConfig;
use c2_mem::{FreeResult, MemPool};

// ---------------------------------------------------------------------------
// Peer SHM state (set once during handshake, read concurrently)
// ---------------------------------------------------------------------------

/// Valid initialization geometry only; mapped backing headers supply the
/// peer's actual geometry. This does not allocate or size a peer mapping.
const MIN_BUDDY_SEGMENT_SIZE: usize = 2 * 4096;

struct PeerShmState {
    prefix: String,
    /// MemPool opened over the client's SHM segments for data access + free.
    pool: Option<Arc<RwLock<MemPool>>>,
}

impl PeerShmState {
    fn new() -> Self {
        Self {
            prefix: String::new(),
            pool: None,
        }
    }

    fn ensure_segment(
        &self,
        seg_idx: u32,
        generation: u32,
        data_size: usize,
        dedicated: bool,
    ) -> Result<(), String> {
        let pool_arc = self.pool.as_ref().ok_or("peer pool not initialised")?;
        let mut pool = pool_arc.write();
        if dedicated {
            if generation != 0 {
                return Err("dedicated generation must be zero".into());
            }
            let name = MemPool::dedicated_segment_name(&self.prefix, seg_idx);
            // SAFETY: the private peer cache and canonical name come from one handshake.
            unsafe { pool.open_dedicated_at(seg_idx, &name, data_size) }
        } else {
            // Open with exactly the frame's data span: the mapped region must
            // hold the advertised payload, and the allocator header supplies
            // the real segment geometry. Never clamp up to a local guess.
            pool.ensure_peer_segment(seg_idx, generation, data_size)
        }
    }
}

/// RAII guard that increments flight count on creation and decrements on drop.
/// Ensures [`Connection::wait_idle`] blocks until all guarded tasks complete.
pub(crate) struct FlightGuard<'a> {
    conn: &'a Connection,
}

impl<'a> FlightGuard<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        conn.flight_inc();
        Self { conn }
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.conn.flight_dec();
    }
}

/// Per-client connection state.
pub struct Connection {
    conn_id: u64,
    handshake_done: AtomicBool,
    chunked_capable: AtomicBool,

    /// Client's SHM pool incarnation prefix and opened peer cache.
    peer_shm: Mutex<PeerShmState>,

    /// Monotonic timestamp of the last frame received/sent.
    last_activity: Mutex<Instant>,

    /// Number of requests currently being processed.
    inflight: AtomicI32,

    /// Notified whenever `inflight` drops to zero.
    idle_notify: Notify,

    /// Set when the read loop has closed and queued not-started work should stop.
    cancelled: AtomicBool,
    cancel_notify: Notify,
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
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
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
    #[cfg(test)]
    pub(crate) fn peer_pool_arc(&self) -> Option<Arc<RwLock<MemPool>>> {
        let state = self.peer_shm.lock();
        state.pool.clone()
    }

    /// Return the peer's SHM pool prefix (empty if handshake not done).
    pub fn peer_prefix(&self) -> String {
        self.peer_shm.lock().prefix.clone()
    }

    /// Initialise the peer SHM cache from its handshake pool incarnation.
    ///
    /// Always creates a `MemPool` with the peer's prefix so that later
    /// `read_peer_data` / `free_peer_block` can lazy-open segments by
    /// resolving names through MemPool's prefix, index, and generation authority.
    pub fn init_peer_shm(&self, prefix: String) {
        let mut state = self.peer_shm.lock();
        state.prefix = prefix;

        // Receive-side cache: `buddy_enabled` is irrelevant (peer pools never
        // allocate) and `min_retained_segments` is 0 so cached views retire as
        // soon as their peer frees — matching MemPool's peer-cache behavior.
        // The segment bound is the canonical IPC buddy segment bound, so any
        // index a legitimately configured peer may reference (config caps
        // `max_pool_segments` at the same constant) is accepted here while
        // arbitrary counts stay rejected by `ensure_peer_segment`.
        let cfg = PoolConfig {
            segment_size: MIN_BUDDY_SEGMENT_SIZE,
            min_block_size: 4096,
            max_segments: MAX_IPC_POOL_SEGMENTS as usize,
            max_dedicated_segments: 4,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 60.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c_two_request_cache"),
            buddy_enabled: true,
            min_retained_segments: 0,
        };
        let peer_prefix = state.prefix.clone();
        let pool = MemPool::open_peer(cfg, peer_prefix);
        state.pool = Some(Arc::new(RwLock::new(pool)));
    }

    /// Ensure the peer's SHM segment at `seg_idx` is mapped (lazy-open).
    /// Does NOT read data — just ensures the segment is available for ShmBuffer access.
    pub fn ensure_peer_segment(
        &self,
        seg_idx: u16,
        generation: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<(), String> {
        let state = self.peer_shm.lock();
        state.ensure_segment(seg_idx as u32, generation, data_size as usize, is_dedicated)
    }

    /// Ensure the peer's SHM segment is mapped and return the pool Arc.
    /// Combines `ensure_peer_segment` + `peer_pool_arc` under one lock.
    pub(crate) fn ensure_and_get_peer_pool(
        &self,
        seg_idx: u16,
        generation: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<Arc<RwLock<MemPool>>, String> {
        let state = self.peer_shm.lock();
        state.ensure_segment(seg_idx as u32, generation, data_size as usize, is_dedicated)?;
        state
            .pool
            .clone()
            .ok_or_else(|| "peer pool not initialised".to_string())
    }

    /// Read `data_size` bytes from the peer's SHM at `(seg_idx, generation, offset)`.
    ///
    /// Lazy-opens the segment if it hasn't been seen before — the name is
    /// derived by MemPool from its prefix, index, and generation.
    ///
    /// Crate-private: only the server's frame-receive paths may pass peer
    /// coordinates through this raw seam; every admission funnels into a
    /// [`crate::PeerShmRequest`] carrier that owns the read/release
    /// authority for its block.
    pub(crate) fn read_peer_data(
        &self,
        seg_idx: u16,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    ) -> Result<Vec<u8>, String> {
        let state = self.peer_shm.lock();
        // Lazy-open the segment if the pool hasn't mapped it yet.
        state.ensure_segment(seg_idx as u32, generation, data_size as usize, is_dedicated)?;
        let pool_arc = state.pool.as_ref().ok_or("peer pool not initialised")?;
        let pool = pool_arc.read();
        // SAFETY: the coordinates come from a frame decoded on this exact
        // connection; the peer completed and published the write
        // (write-then-publish protocol) and no other local access to the
        // block exists.
        unsafe { pool.copy_data_at(seg_idx as u32, generation, offset, data_size, is_dedicated) }
    }

    /// Free a buddy block in the peer's SHM pool.
    ///
    /// Lazy-opens the segment if not yet mapped.
    /// Returns `FreeResult` so callers can trigger deferred GC on idle segments.
    ///
    /// Crate-private: same frame-admission boundary as
    /// [`Connection::read_peer_data`].
    pub(crate) fn free_peer_block(
        &self,
        seg_idx: u16,
        generation: u32,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
        allocation_id: u64,
    ) -> FreeResult {
        let state = self.peer_shm.lock();
        // Lazy-open the segment before freeing.
        let lazy_res =
            state.ensure_segment(seg_idx as u32, generation, data_size as usize, is_dedicated);
        if let Err(e) = lazy_res {
            warn!(
                conn_id = self.conn_id,
                seg_idx, "lazy-open for free failed: {e}"
            );
            return FreeResult::Normal;
        }
        if let Some(pool_arc) = state.pool.as_ref() {
            let mut pool = pool_arc.write();
            match pool
                .validate_data_at(seg_idx as u32, generation, offset, data_size, is_dedicated)
                .and_then(|_| {
                    // SAFETY: the coordinates come from a frame received on
                    // this connection and the server is the sole releaser
                    // for the peer's block (read-then-release protocol).
                    unsafe {
                        pool.free_at(
                            seg_idx as u32,
                            generation,
                            offset,
                            data_size,
                            is_dedicated,
                            allocation_id,
                        )
                    }
                }) {
                Ok(result) => return result,
                Err(e) => {
                    warn!(
                        conn_id = self.conn_id,
                        seg_idx, offset, "peer free_at failed: {e}"
                    );
                }
            }
        }
        FreeResult::Normal
    }

    /// Run buddy GC on the peer's SHM pool — reclaim idle trailing segments.
    pub fn gc_peer_buddy(&self) {
        let state = self.peer_shm.lock();
        if let Some(pool_arc) = state.pool.as_ref() {
            let mut pool = pool_arc.write();
            pool.gc_buddy();
        }
    }

    /// Record activity — updates the last-activity timestamp to *now*.
    pub fn touch(&self) {
        *self.last_activity.lock() = Instant::now();
    }

    /// Seconds elapsed since the last `touch()`.
    pub fn idle_seconds(&self) -> f64 {
        self.last_activity.lock().elapsed().as_secs_f64()
    }

    /// Mark a new request as in-flight.
    pub(crate) fn flight_inc(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a request as completed.  If the in-flight counter reaches zero,
    /// all waiters on [`wait_idle`](Self::wait_idle) are notified.
    pub(crate) fn flight_dec(&self) {
        let prev = self.inflight.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Counter went 1 → 0: connection is idle.
            self.idle_notify.notify_waiters();
        }
    }

    pub(crate) fn cancel_queued_work(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        loop {
            let notified = self.cancel_notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
            if self.is_cancelled() {
                return;
            }
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
        assert!(conn.peer_pool_arc().is_none());
        assert_eq!(conn.inflight_count(), 0);
    }

    #[test]
    fn init_peer_shm_initializes_unmapped_peer_pool() {
        let conn = Connection::new(1);
        conn.init_peer_shm("prefix".into());
        assert_eq!(conn.peer_prefix(), "prefix");
        assert_eq!(conn.peer_pool_arc().unwrap().read().segment_count(), 0);
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

    #[test]
    fn manual_flight_counter_methods_are_not_public_surface() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/connection.rs"))
                .unwrap();
        assert!(
            !source
                .lines()
                .any(|line| line.trim_start().starts_with("pub fn flight_inc(")),
            "manual flight counter increment must not be part of the public API"
        );
        assert!(
            !source
                .lines()
                .any(|line| line.trim_start().starts_with("pub fn flight_dec(")),
            "manual flight counter decrement must not be part of the public API"
        );
        assert!(source.contains("pub(crate) fn flight_inc("));
        assert!(source.contains("pub(crate) fn flight_dec("));
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
