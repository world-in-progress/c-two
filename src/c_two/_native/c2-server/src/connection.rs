//! Per-client connection state.
//!
//! Mirrors the Python `Connection` dataclass from
//! `c_two.transport.server.connection` — tracks handshake status,
//! SHM segments, activity timestamps, and in-flight request counting.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::Notify;

/// Per-client connection state.
pub struct Connection {
    conn_id: u64,
    handshake_done: AtomicBool,
    chunked_capable: AtomicBool,

    /// Client's SHM pool prefix (for lazy segment naming).
    pub peer_prefix: String,

    /// Opened SHM segment names received from the client.
    pub remote_segment_names: Vec<String>,

    /// Sizes (bytes) of the client-side SHM segments.
    pub remote_segment_sizes: Vec<u64>,

    /// Monotonic timestamp of the last frame received/sent.
    last_activity: Mutex<Instant>,

    /// Number of requests currently being processed.
    inflight: AtomicI32,

    /// Notified whenever `inflight` drops to zero.
    idle_notify: Notify,
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
            peer_prefix: String::new(),
            remote_segment_names: Vec::new(),
            remote_segment_sizes: Vec::new(),
            last_activity: Mutex::new(Instant::now()),
            inflight: AtomicI32::new(0),
            idle_notify: Notify::new(),
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
        assert!(conn.peer_prefix.is_empty());
        assert!(conn.remote_segment_names.is_empty());
        assert!(conn.remote_segment_sizes.is_empty());
        assert_eq!(conn.inflight_count(), 0);
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
