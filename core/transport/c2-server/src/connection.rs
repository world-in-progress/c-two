//! Per-client connection state.
//!
//! Tracks one physical IPC peer and projects its shared connection core.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use tokio::sync::Notify;

use c2_config::MAX_IPC_POOL_SEGMENTS;
use c2_local::{ConnectionEndReason, ConnectionObservation, ConnectionPublisher, RemoteReadGuard};
use c2_mem::PeerMappingCache;
use c2_mem::config::PoolConfig;
use c2_wire::connection::{ConnectionCore, ConnectionError, ConnectionSide};

// ---------------------------------------------------------------------------
// Peer SHM state (set once during handshake, read concurrently)
// ---------------------------------------------------------------------------

/// Valid initialization geometry only; the OS mapping supplies the actual
/// span. This does not allocate or size a peer mapping.
const MIN_BUDDY_SEGMENT_SIZE: usize = 2 * 4096;

struct PeerShmState {
    cache: Option<Arc<PeerMappingCache>>,
}

impl PeerShmState {
    fn new() -> Self {
        Self { cache: None }
    }
}

#[derive(Clone, Copy)]
enum PendingEnd {
    Explicit {
        reason: ConnectionEndReason,
        operation: &'static str,
    },
    Read {
        operation: &'static str,
        kind: std::io::ErrorKind,
        os_error: Option<i32>,
    },
    Io {
        operation: &'static str,
        kind: std::io::ErrorKind,
        os_error: Option<i32>,
    },
}

fn restore_error(kind: std::io::ErrorKind, os_error: Option<i32>) -> std::io::Error {
    match os_error {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(kind, "connection I/O ended"),
    }
}

fn publish_end(publisher: &ConnectionPublisher, first_end: &Mutex<Option<PendingEnd>>) {
    let end = *first_end.lock();
    match end {
        Some(PendingEnd::Explicit { reason, operation }) => {
            publisher.close(reason, operation, None);
        }
        Some(PendingEnd::Read {
            operation,
            kind,
            os_error,
        }) => {
            publisher.close_read_error(operation, &restore_error(kind, os_error));
        }
        Some(PendingEnd::Io {
            operation,
            kind,
            os_error,
        }) => {
            publisher.close_io(operation, &restore_error(kind, os_error));
        }
        None => {
            publisher.close(ConnectionEndReason::LocalClose, "stream_retired", None);
        }
    }
}

/// Publishes only after the last driver or remote reader drops this owner.
pub(crate) struct PhysicalRetirement {
    guard: Option<RemoteReadGuard>,
    publisher: ConnectionPublisher,
    first_end: Arc<Mutex<Option<PendingEnd>>>,
}

impl Drop for PhysicalRetirement {
    fn drop(&mut self) {
        drop(self.guard.take());
        publish_end(&self.publisher, &self.first_end);
    }
}

/// RAII guard that increments flight count on creation and decrements on drop.
/// Owns the reservation before task submission, through the response flush.
pub(crate) struct FlightGuard {
    conn: Arc<Connection>,
}

impl FlightGuard {
    pub fn new(conn: &Arc<Connection>) -> Self {
        conn.flight_inc();
        Self {
            conn: Arc::clone(conn),
        }
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.conn.flight_dec();
    }
}

/// A submitted write outlives its waiter's timeout. Keep business drain open
/// until the driver completes or discards the original submission.
pub(crate) struct WriteGuard {
    conn: Arc<Connection>,
}

impl WriteGuard {
    pub(crate) fn new(conn: &Arc<Connection>) -> Self {
        conn.pending_writes.fetch_add(1, Ordering::AcqRel);
        Self { conn: conn.clone() }
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        if self.conn.pending_writes.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.conn.notify_idle();
        }
    }
}

/// Per-client connection state.
pub struct Connection {
    pub(crate) core: ConnectionCore,
    /// Serialize response-loan publication with local deactivation. The writer
    /// holds this only through preparation and begin_io, never through I/O.
    pub(crate) publication_gate: Mutex<()>,
    pub(crate) writer_wake: Arc<Notify>,
    conn_id: u64,
    ordinary_protocol: AtomicBool,
    handshake_done: AtomicBool,
    chunked_capable: AtomicBool,
    /// Native shared-memory reachability observed on this connection's
    /// accepted stream (same TokenSessionId as the peer on Windows; always
    /// true on Unix). Captured before the handshake so the capability ACK is
    /// the intersection of the client's offer with actual reachability.
    /// Defaults to `false`: a connection whose stream was never observed
    /// never negotiates SHM directions.
    native_shm_reachable: AtomicBool,
    /// Whether the handshake negotiated the client→server SHM request
    /// direction (client offered `CAP_SHM_REQUEST` and the native
    /// reachability above agreed). Inbound buddy request frames are rejected
    /// before any admission, lazy-open, or late-discard cleanup while this
    /// is false.
    shm_request_capable: AtomicBool,
    /// Whether the handshake negotiated the server→client SHM response
    /// direction. While false, response transport selection never publishes
    /// response coordinates on this connection.
    shm_response_capable: AtomicBool,

    /// Client's SHM pool incarnation prefix and opened peer cache.
    peer_shm: Mutex<PeerShmState>,

    /// Retain the accepted OS stream until the last admitted remote read ends.
    remote_read_guard: Mutex<Option<Arc<PhysicalRetirement>>>,
    /// Once installed, only PhysicalRetirement may publish the physical close.
    remote_read_guard_installed: AtomicBool,
    /// The first requested end is retained until the physical stream retires.
    first_end: Arc<Mutex<Option<PendingEnd>>>,
    /// The receive driver cannot obtain any more release or peer-end facts.
    read_stopped: AtomicBool,
    peer_ended: AtomicBool,
    /// The reader lost the peer or a trustworthy next frame boundary.
    work_cancelled: AtomicBool,
    #[cfg(test)]
    pub(crate) fail_next_write_allocation: AtomicBool,

    /// Number of requests currently being processed.
    inflight: AtomicI32,
    pending_writes: AtomicUsize,
    #[cfg(test)]
    pub(crate) signal_reply_pause: parking_lot::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    /// Route admission is closed; accepted calls retain response write access.
    shutdown_requested: AtomicBool,

    /// Notified whenever `inflight` drops to zero.
    idle_notify: Notify,

    /// Terminal transport authority; queued work cancellation derives from it.
    publisher: ConnectionPublisher,
}

impl Connection {
    /// Create a new connection with the given `conn_id`.
    ///
    /// All fields start at their default/empty state.
    pub fn new(conn_id: u64) -> Self {
        let connection = Self::with_publisher(
            conn_id,
            ConnectionPublisher::new(c2_local::ConnectionSide::Server),
        );
        connection.select_ordinary_protocol();
        connection
    }

    pub(crate) fn observed(publisher: ConnectionPublisher) -> Self {
        Self::with_publisher(publisher.observation().connection_id(), publisher)
    }

    fn with_publisher(conn_id: u64, publisher: ConnectionPublisher) -> Self {
        let writer_wake = Arc::new(Notify::new());
        let notify = writer_wake.clone();
        Self {
            core: ConnectionCore::new(ConnectionSide::Server, move || {
                notify.notify_waiters();
                notify.notify_one();
            }),
            publication_gate: Mutex::new(()),
            writer_wake,
            conn_id,
            ordinary_protocol: AtomicBool::new(false),
            handshake_done: AtomicBool::new(false),
            chunked_capable: AtomicBool::new(false),
            native_shm_reachable: AtomicBool::new(false),
            shm_request_capable: AtomicBool::new(false),
            shm_response_capable: AtomicBool::new(false),
            peer_shm: Mutex::new(PeerShmState::new()),
            remote_read_guard: Mutex::new(None),
            remote_read_guard_installed: AtomicBool::new(false),
            first_end: Arc::new(Mutex::new(None)),
            read_stopped: AtomicBool::new(false),
            peer_ended: AtomicBool::new(false),
            work_cancelled: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_write_allocation: AtomicBool::new(false),
            inflight: AtomicI32::new(0),
            pending_writes: AtomicUsize::new(0),
            #[cfg(test)]
            signal_reply_pause: parking_lot::Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
            idle_notify: Notify::new(),
            publisher,
        }
    }

    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    pub(crate) fn ordinary_protocol(&self) -> bool {
        self.ordinary_protocol.load(Ordering::Acquire)
    }

    pub(crate) fn select_ordinary_protocol(&self) {
        self.ordinary_protocol.store(true, Ordering::Release);
    }

    pub fn connection_observation(&self) -> ConnectionObservation {
        self.publisher.observation()
    }

    pub(crate) fn end(&self, reason: ConnectionEndReason, operation: &'static str) {
        let _publication = self.publication_gate.lock();
        self.record_end(PendingEnd::Explicit { reason, operation });
        drop(self.core.deactivate());
    }

    pub(crate) fn begin_shutdown(&self) {
        let _publication = self.publication_gate.lock();
        self.record_end(PendingEnd::Explicit {
            reason: ConnectionEndReason::LocalClose,
            operation: "server_shutdown",
        });
        self.shutdown_requested.store(true, Ordering::Release);
        if self.is_idle() {
            drop(self.core.deactivate());
        }
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    pub(crate) fn end_read(&self, operation: &'static str, error: &std::io::Error) {
        let _publication = self.publication_gate.lock();
        self.record_end(PendingEnd::Read {
            operation,
            kind: error.kind(),
            os_error: error.raw_os_error(),
        });
        drop(self.core.deactivate());
    }

    pub(crate) fn end_io(&self, operation: &'static str, error: &std::io::Error) {
        let _publication = self.publication_gate.lock();
        self.record_end(PendingEnd::Io {
            operation,
            kind: error.kind(),
            os_error: error.raw_os_error(),
        });
        drop(self.core.deactivate());
    }

    fn record_end(&self, end: PendingEnd) {
        self.first_end.lock().get_or_insert(end);
    }

    pub(crate) fn note_peer_close_message(&self) {
        let _publication = self.publication_gate.lock();
        self.record_end(PendingEnd::Explicit {
            reason: ConnectionEndReason::PeerCloseMessage,
            operation: "disconnect",
        });
    }

    /// Publish for a connection that ended before a retirement guard was made.
    /// The caller must already have dropped its unsplit stream.
    pub(crate) fn publish_physical_close(&self) {
        publish_end(&self.publisher, &self.first_end);
    }

    pub(crate) fn stop_reading(&self) {
        self.read_stopped.store(true, Ordering::Release);
        self.writer_wake.notify_waiters();
        self.writer_wake.notify_one();
    }

    pub(crate) fn reading_stopped(&self) -> bool {
        self.read_stopped.load(Ordering::Acquire)
    }

    pub(crate) fn install_remote_read_guard(&self, guard: RemoteReadGuard) {
        *self.remote_read_guard.lock() = Some(Arc::new(PhysicalRetirement {
            guard: Some(guard),
            publisher: self.publisher.clone(),
            first_end: Arc::clone(&self.first_end),
        }));
        self.remote_read_guard_installed
            .store(true, Ordering::Release);
    }

    pub(crate) fn remote_read_guard(&self) -> Arc<PhysicalRetirement> {
        self.remote_read_guard
            .lock()
            .as_ref()
            .expect("accepted stream guard installed")
            .clone()
    }

    pub(crate) fn has_ever_installed_remote_read_guard(&self) -> bool {
        self.remote_read_guard_installed.load(Ordering::Acquire)
    }

    pub(crate) fn has_remote_read_guard(&self) -> bool {
        self.remote_read_guard.lock().is_some()
    }

    pub(crate) fn clear_remote_read_guard(&self) {
        self.remote_read_guard.lock().take();
    }

    pub(crate) fn try_close(&self) -> bool {
        if !self.is_idle() && !self.reading_stopped() && !self.peer_ended.load(Ordering::Acquire) {
            return false;
        }
        self.core.try_close()
    }

    pub(crate) fn peer_end(&self) {
        self.peer_ended.store(true, Ordering::Release);
        drop(self.core.peer_end());
    }

    pub fn handshake_done(&self) -> bool {
        self.handshake_done.load(Ordering::Relaxed)
    }

    pub fn set_handshake_done(&self, v: bool) -> Result<(), ConnectionError> {
        if !v {
            return Err(ConnectionError::ProtocolViolation(
                "handshake completion cannot be undone".into(),
            ));
        }
        self.core.finish_handshake()?;
        self.handshake_done.store(true, Ordering::Release);
        Ok(())
    }

    pub fn chunked_capable(&self) -> bool {
        self.chunked_capable.load(Ordering::Relaxed)
    }

    pub fn set_chunked_capable(&self, v: bool) {
        self.chunked_capable.store(v, Ordering::Relaxed);
    }

    /// Native shared-memory reachability of this connection's accepted
    /// stream; set once by the accept path before the handshake runs.
    pub(crate) fn native_shm_reachable(&self) -> bool {
        self.native_shm_reachable.load(Ordering::Acquire)
    }

    pub(crate) fn set_native_shm_reachable(&self, v: bool) {
        self.native_shm_reachable.store(v, Ordering::Release);
    }

    /// Whether the handshake negotiated the client→server SHM request
    /// direction for this connection.
    pub(crate) fn shm_request_capable(&self) -> bool {
        self.shm_request_capable.load(Ordering::Acquire)
    }

    pub(crate) fn set_shm_request_capable(&self, v: bool) {
        self.shm_request_capable.store(v, Ordering::Release);
    }

    /// Whether the handshake negotiated the server→client SHM response
    /// direction for this connection.
    pub(crate) fn shm_response_capable(&self) -> bool {
        self.shm_response_capable.load(Ordering::Acquire)
    }

    pub(crate) fn set_shm_response_capable(&self, v: bool) {
        self.shm_response_capable.store(v, Ordering::Release);
    }

    // -- peer SHM accessors -------------------------------------------------

    /// Get the read-only mapping cache for the peer's pool incarnation.
    /// Returns None if handshake hasn't completed yet.
    pub(crate) fn peer_mappings(&self) -> Option<Arc<PeerMappingCache>> {
        let state = self.peer_shm.lock();
        state.cache.clone()
    }

    /// Initialise the peer SHM cache from its handshake pool incarnation.
    ///
    /// Dedicated views retire at the last admitted read; buddy views start
    /// their configured idle window then and retire in the maintenance task.
    pub(crate) fn init_peer_shm(&self, prefix: String, decay_seconds: f64) -> Result<(), String> {
        let cfg = PoolConfig {
            segment_size: MIN_BUDDY_SEGMENT_SIZE,
            min_block_size: 4096,
            max_segments: MAX_IPC_POOL_SEGMENTS as usize,
            max_dedicated_segments: usize::from(u16::MAX) + 1,
            buddy_idle_decay_secs: decay_seconds,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c_two_request_cache"),
            buddy_enabled: true,
            min_retained_segments: 0,
        };
        let cache = PeerMappingCache::new(cfg, prefix)?;
        self.peer_shm.lock().cache = Some(Arc::new(cache));
        Ok(())
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
            self.notify_idle();
        }
    }

    fn is_idle(&self) -> bool {
        self.inflight.load(Ordering::Acquire) == 0
            && self.pending_writes.load(Ordering::Acquire) == 0
    }

    fn notify_idle(&self) {
        if self.is_idle() {
            if self.shutdown_requested() {
                let _publication = self.publication_gate.lock();
                drop(self.core.deactivate());
            }
            self.idle_notify.notify_waiters();
            self.writer_wake.notify_one();
        }
    }

    pub(crate) fn cancel_queued_work(&self) {
        self.work_cancelled.store(true, Ordering::Release);
        self.end(ConnectionEndReason::LocalClose, "connection_cancel");
    }

    pub(crate) fn is_deactivated(&self) -> bool {
        self.core.is_deactivated()
    }

    pub(crate) fn is_work_cancelled(&self) -> bool {
        self.work_cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_work_cancelled(&self) {
        loop {
            let notified = self.writer_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_work_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Current number of in-flight requests.
    pub fn inflight_count(&self) -> i32 {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Wait until requests and their original queued writes have completed.
    ///
    /// Returns immediately if already idle.
    pub async fn wait_idle(&self) {
        // Fast path: already idle.
        if self.is_idle() {
            return;
        }
        // Slow path: wait for notification from `flight_dec`.
        loop {
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // Re-check after registering the future to avoid missed wakeups.
            if self.is_idle() {
                return;
            }
            notified.await;
            if self.is_idle() {
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
        assert_eq!(
            conn.connection_observation().side(),
            c2_local::ConnectionSide::Server
        );
        assert_eq!(conn.conn_id(), 42);
        assert!(!conn.handshake_done());
        assert!(!conn.chunked_capable());
        // SHM directions fail closed until the accept path observes the
        // stream and the handshake negotiates them.
        assert!(!conn.native_shm_reachable());
        assert!(!conn.shm_request_capable());
        assert!(!conn.shm_response_capable());
        assert!(conn.peer_mappings().is_none());
        assert_eq!(conn.inflight_count(), 0);
    }

    #[test]
    fn init_peer_shm_initializes_unmapped_peer_cache() {
        let conn = Connection::new(1);
        conn.init_peer_shm("prefix".into(), 7.5).unwrap();
        assert_eq!(conn.peer_mappings().unwrap().mapped_segments(), 0);
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

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use c2_wire::connection::WriteRequest;

    #[test]
    fn unpolled_response_task_keeps_shutdown_writable_and_cancellation_retires_its_flight() {
        let conn = Arc::new(Connection::new(1));
        conn.set_handshake_done(true).unwrap();
        let flight = FlightGuard::new(&conn);
        let task = async move {
            let _flight = flight;
            std::future::pending::<()>().await;
        };
        conn.begin_shutdown();
        assert_eq!(conn.inflight_count(), 1);
        assert!(!conn.is_deactivated());
        drop(
            conn.core
                .begin_write(WriteRequest::Business(Vec::new()))
                .unwrap(),
        );
        drop(task);
        assert_eq!(conn.inflight_count(), 0);
        assert!(conn.is_deactivated());
        assert!(conn.try_close());
    }
}
