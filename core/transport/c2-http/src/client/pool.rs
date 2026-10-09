//! Reference-counted pool of [`HttpClient`] instances.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, HttpError};

// ── Pool entry ──────────────────────────────────────────────────────────

fn canonical_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_owned()
}

struct PoolEntry {
    client: Arc<HttpClient>,
    references: Arc<PoolReferences>,
    use_proxy: bool,
    timeout_secs: f64,
    remote_payload_chunk_size: u64,
}

/// A lease can release its exact entry without waiting for the pool map lock.
/// This is especially important when a connect/probe future is cancelled.
struct PoolReferences {
    count: AtomicUsize,
    origin: Instant,
    // Zero means active; otherwise monotonic nanoseconds since origin + 1.
    // A cancelled probe can release its exact lease without acquiring a lock.
    released_tick: AtomicU64,
}

impl PoolReferences {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(1),
            origin: Instant::now(),
            released_tick: AtomicU64::new(0),
        }
    }

    fn acquire(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.released_tick.store(0, Ordering::Release);
    }

    fn release(&self) {
        if self.count.load(Ordering::Acquire) == 0 {
            return;
        }
        // Publish before exposing an idle count. Acquire's reset is protected
        // by its counted reference; older stamps cannot backdate idle GC.
        self.released_tick.fetch_max(self.tick(), Ordering::AcqRel);
        let _ = self
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            });
    }

    fn tick(&self) -> u64 {
        self.origin.elapsed().as_nanos().min((u64::MAX - 1) as u128) as u64 + 1
    }

    fn expired(&self, grace: Duration) -> bool {
        if self.count.load(Ordering::Acquire) != 0 {
            return false;
        }
        let released = self.released_tick.load(Ordering::Acquire);
        released != 0
            && self.count.load(Ordering::Acquire) == 0
            && u128::from(self.tick().saturating_sub(released)) >= grace.as_nanos()
    }
}

pub(super) struct HttpPoolLease {
    pub(super) client: Arc<HttpClient>,
    references: Arc<PoolReferences>,
}

impl Drop for HttpPoolLease {
    fn drop(&mut self) {
        self.references.release();
    }
}

// ── HttpClientPool ──────────────────────────────────────────────────────

/// Reference-counted pool of [`HttpClient`] instances.
///
/// Clients connecting to the same relay URL share a single
/// `HttpClient` (and its underlying connection pool).  When all
/// references are released, the client is kept for a grace period
/// before being destroyed.
pub struct HttpClientPool {
    // Controlled calls have a separate no-total-timeout view. Legacy clients
    // retain their configured policy, including when both views are live.
    entries: Mutex<HashMap<(String, bool), PoolEntry>>,
    grace_period: Duration,
    default_max_connections: usize,
}

// Compile-time assertion: HttpClientPool must be Send + Sync.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<HttpClientPool>();
        _assert_sync::<HttpClientPool>();
    }
};

impl HttpClientPool {
    /// Create a new pool with the given grace period.
    pub fn new(grace_secs: f64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            grace_period: Duration::from_secs_f64(grace_secs),
            default_max_connections: 100,
        }
    }

    #[cfg(test)]
    fn acquire_with_proxy_policy(
        &self,
        base_url: &str,
        use_proxy: bool,
    ) -> Result<Arc<HttpClient>, HttpError> {
        self.acquire_with_options(
            base_url,
            use_proxy,
            300.0,
            c2_config::DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE,
        )
    }

    /// Acquire (or create) a client for `base_url` with explicit proxy, timeout,
    /// and remote payload batching policy.
    pub fn acquire_with_options(
        &self,
        base_url: &str,
        use_proxy: bool,
        timeout_secs: f64,
        remote_payload_chunk_size: u64,
    ) -> Result<Arc<HttpClient>, HttpError> {
        self.acquire_view(
            base_url,
            use_proxy,
            timeout_secs,
            remote_payload_chunk_size,
            false,
            c2_config::ConnectDeadline::default(),
        )
        .map(|(client, _)| client)
    }

    pub(super) fn acquire_lease(
        &self,
        base_url: &str,
        use_proxy: bool,
        timeout_secs: f64,
        remote_payload_chunk_size: u64,
        controlled: bool,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<HttpPoolLease, HttpError> {
        self.acquire_view(
            base_url,
            use_proxy,
            if controlled { 0.0 } else { timeout_secs },
            remote_payload_chunk_size,
            controlled,
            deadline,
        )
        .map(|(client, references)| HttpPoolLease { client, references })
    }

    fn acquire_view(
        &self,
        base_url: &str,
        use_proxy: bool,
        timeout_secs: f64,
        remote_payload_chunk_size: u64,
        controlled: bool,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<(Arc<HttpClient>, Arc<PoolReferences>), HttpError> {
        super::connect_deadline::check(deadline, "relay_pool_acquire")?;
        crate::payload::validate_remote_payload_chunk_size(remote_payload_chunk_size)?;
        let key = (canonical_base_url(base_url), controlled);

        let mut entries =
            super::connect_deadline::lock(&self.entries, deadline, "relay_pool_acquire")?;
        entries.retain(|_, entry| !entry.references.expired(self.grace_period));

        if let Some(entry) = entries.get_mut(&key) {
            if entry.use_proxy == use_proxy
                && entry.timeout_secs == timeout_secs
                && entry.remote_payload_chunk_size == remote_payload_chunk_size
            {
                entry.references.acquire();
                return Ok((Arc::clone(&entry.client), Arc::clone(&entry.references)));
            }
            if entry.references.count.load(Ordering::Acquire) > 0 && entry.use_proxy != use_proxy {
                return Err(HttpError::Transport(format!(
                    "active pooled HTTP client for {base_url} has proxy policy mismatch"
                )));
            }
            if entry.references.count.load(Ordering::Acquire) > 0
                && entry.timeout_secs != timeout_secs
            {
                return Err(HttpError::Transport(format!(
                    "active pooled HTTP client for {base_url} has timeout policy mismatch"
                )));
            }
            if entry.references.count.load(Ordering::Acquire) > 0
                && entry.remote_payload_chunk_size != remote_payload_chunk_size
            {
                return Err(HttpError::Transport(format!(
                    "active pooled HTTP client for {base_url} has remote payload chunk policy mismatch"
                )));
            }
        }

        // Create a new client (lock held — HttpClient::new is fast).
        let client = Arc::new(if controlled {
            HttpClient::new_controlled(
                &key.0,
                self.default_max_connections,
                use_proxy,
                remote_payload_chunk_size,
            )?
        } else {
            HttpClient::new_with_transport_policy(
                &key.0,
                timeout_secs,
                self.default_max_connections,
                use_proxy,
                remote_payload_chunk_size,
            )?
        });

        super::connect_deadline::check(deadline, "relay_pool_acquire")?;
        let references = Arc::new(PoolReferences::new());
        entries.insert(
            key,
            PoolEntry {
                client: Arc::clone(&client),
                references: Arc::clone(&references),
                use_proxy,
                timeout_secs,
                remote_payload_chunk_size,
            },
        );

        Ok((client, references))
    }

    /// Decrement reference count; mark for grace-period cleanup at 0.
    pub fn release(&self, base_url: &str) {
        self.release_view(base_url, false);
    }

    pub(crate) fn release_view(&self, base_url: &str, controlled: bool) {
        let key = (canonical_base_url(base_url), controlled);
        let entries = self.entries.lock();
        if let Some(entry) = entries.get(&key) {
            entry.references.release();
        }
    }

    /// Sweep entries past the grace period.
    pub fn sweep_expired(&self) {
        self.entries
            .lock()
            .retain(|_, entry| !entry.references.expired(self.grace_period));
    }

    /// Destroy all clients immediately.
    pub fn shutdown_all(&self) {
        let mut entries = self.entries.lock();
        entries.clear();
    }

    /// Number of active entries.
    pub fn active_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// Reference count for a specific URL.
    pub fn refcount(&self, base_url: &str) -> usize {
        self.entries
            .lock()
            .get(&(canonical_base_url(base_url), false))
            .map_or(0, |e| e.references.count.load(Ordering::Acquire))
    }
}

// ── Singleton ───────────────────────────────────────────────────────────

static GLOBAL_HTTP_POOL: OnceLock<HttpClientPool> = OnceLock::new();

impl HttpClientPool {
    /// Return the process-level singleton.
    pub fn instance() -> &'static HttpClientPool {
        GLOBAL_HTTP_POOL.get_or_init(|| HttpClientPool::new(60.0))
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_lease_release_cannot_decrement_replacement_client() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9986";
        let old = pool
            .acquire_lease(
                url,
                false,
                300.0,
                1024,
                false,
                c2_config::ConnectDeadline::default(),
            )
            .unwrap();
        pool.shutdown_all();
        let replacement = pool
            .acquire_lease(
                url,
                false,
                300.0,
                1024,
                false,
                c2_config::ConnectDeadline::default(),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&old.client, &replacement.client));
        drop(old);
        assert_eq!(pool.refcount(url), 1);
        drop(replacement);
        assert_eq!(pool.refcount(url), 0);
    }

    #[test]
    fn connect_lease_release_does_not_wait_for_pool_lock() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9987";
        let lease = pool
            .acquire_lease(
                url,
                false,
                300.0,
                1024,
                false,
                c2_config::ConnectDeadline::default(),
            )
            .unwrap();
        let held = pool.entries.lock();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let releaser = std::thread::spawn(move || {
            drop(lease);
            released_tx.send(()).unwrap();
        });
        let released_while_map_held = released_rx.recv_timeout(Duration::from_millis(500)).is_ok();
        let count = held[&(url.into(), false)]
            .references
            .count
            .load(Ordering::Acquire);
        drop(held);
        releaser.join().unwrap();
        assert!(
            released_while_map_held,
            "cancel cleanup must not wait for another pool acquisition"
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn connect_pool_wait_is_bounded_and_does_not_publish() {
        let pool = Arc::new(HttpClientPool::new(60.0));
        let held = pool.entries.lock();
        let waiter_pool = Arc::clone(&pool);
        let waiter = std::thread::spawn(move || {
            let deadline = c2_config::ConnectDeadline::start(
                c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(30)),
            )
            .unwrap();
            let start = Instant::now();
            let error = waiter_pool
                .acquire_lease("http://localhost:9988", false, 300.0, 1024, false, deadline)
                .err()
                .expect("pool lock must expire");
            assert!(start.elapsed() < Duration::from_millis(500));
            let HttpError::LocalCallRejected(error) = error else {
                panic!("canonical error required")
            };
            assert_eq!(error.code, c2_error::ErrorCode::CallDeadlineExceeded);
            assert_eq!(error.details["operation"], "connect");
            assert_eq!(error.details["transport_phase"], "pre_dispatch");
            assert_eq!(error.details["stage"], "relay_pool_acquire");
        });
        waiter.join().unwrap();
        assert!(held.is_empty());
        drop(held);
        let client = pool
            .acquire_with_options("http://localhost:9988", false, 300.0, 1024)
            .unwrap();
        assert_eq!(pool.refcount("http://localhost:9988"), 1);
        pool.release("http://localhost:9988");
        drop(client);
    }

    #[test]
    fn controlled_view_coexists_with_live_legacy_timeout_policy() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9989";
        let legacy = pool.acquire_with_options(url, false, 300.0, 1024).unwrap();
        let controlled = pool
            .acquire_view(
                url,
                false,
                0.0,
                1024,
                true,
                c2_config::ConnectDeadline::default(),
            )
            .unwrap()
            .0;
        let controlled_again = pool
            .acquire_view(
                url,
                false,
                0.0,
                1024,
                true,
                c2_config::ConnectDeadline::default(),
            )
            .unwrap()
            .0;
        let legacy_again = pool.acquire_with_options(url, false, 300.0, 1024).unwrap();

        assert!(!Arc::ptr_eq(&legacy, &controlled));
        assert!(Arc::ptr_eq(&controlled, &controlled_again));
        assert!(Arc::ptr_eq(&legacy, &legacy_again));
        assert_eq!(pool.refcount(url), 2);
        assert_eq!(
            pool.entries.lock()[&(url.to_string(), true)]
                .references
                .count
                .load(Ordering::Acquire),
            2
        );

        pool.release_view(url, true);
        pool.release_view(url, true);
        assert_eq!(
            pool.refcount(url),
            2,
            "controlled release must not release legacy leases"
        );
        assert_eq!(
            pool.entries.lock()[&(url.to_string(), true)]
                .references
                .count
                .load(Ordering::Acquire),
            0
        );
        assert_eq!(
            pool.entries.lock()[&(url.to_string(), false)].timeout_secs,
            300.0
        );
        pool.release(url);
        pool.release(url);
    }

    #[test]
    fn test_pool_new() {
        let pool = HttpClientPool::new(30.0);
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_singleton() {
        let p1 = HttpClientPool::instance() as *const HttpClientPool;
        let p2 = HttpClientPool::instance() as *const HttpClientPool;
        assert_eq!(p1, p2, "singleton must return the same instance");
    }

    #[test]
    fn test_pool_acquire_with_proxy_policy_release() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9999";

        let _client = pool.acquire_with_proxy_policy(url, false).unwrap();
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.refcount(url), 1);

        let _client2 = pool.acquire_with_proxy_policy(url, false).unwrap();
        assert_eq!(pool.refcount(url), 2);

        pool.release(url);
        assert_eq!(pool.refcount(url), 1);

        pool.release(url);
        assert_eq!(pool.refcount(url), 0);
    }

    #[test]
    fn trailing_slash_variants_share_one_pool_entry() {
        let pool = HttpClientPool::new(60.0);

        let first = pool
            .acquire_with_proxy_policy("http://localhost:9998", false)
            .unwrap();
        let second = pool
            .acquire_with_proxy_policy("http://localhost:9998/", false)
            .unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.refcount("http://localhost:9998"), 2);
        assert_eq!(pool.refcount("http://localhost:9998/"), 2);

        pool.release("http://localhost:9998/");
        assert_eq!(pool.refcount("http://localhost:9998"), 1);
        pool.release("http://localhost:9998");
        assert_eq!(pool.refcount("http://localhost:9998/"), 0);
    }

    #[test]
    fn active_proxy_policy_mismatch_is_rejected_without_refcount_change() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9995";

        let _client = pool.acquire_with_proxy_policy(url, false).unwrap();
        let err = match pool.acquire_with_proxy_policy(url, true) {
            Ok(_) => panic!("active client with different proxy policy must be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("proxy policy mismatch"),
            "unexpected error: {err}"
        );
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn released_proxy_policy_mismatch_replaces_idle_entry() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9994";

        let first = pool.acquire_with_proxy_policy(url, false).unwrap();
        pool.release(url);

        let second = pool.acquire_with_proxy_policy(url, true).unwrap();

        assert!(
            !Arc::ptr_eq(&first, &second),
            "idle entry with stale proxy policy should be replaced"
        );
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn active_timeout_policy_mismatch_is_rejected_without_refcount_change() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9993";

        let _client = pool
            .acquire_with_options(url, false, 300.0, 1_048_576)
            .unwrap();
        let err = match pool.acquire_with_options(url, false, 900.0, 1_048_576) {
            Ok(_) => panic!("active client with different timeout policy must be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("timeout policy mismatch"),
            "unexpected error: {err}"
        );
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn active_remote_payload_chunk_policy_mismatch_is_rejected_without_refcount_change() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9994";

        let _client = pool
            .acquire_with_options(url, false, 300.0, 1_048_576)
            .unwrap();
        let err = match pool.acquire_with_options(url, false, 300.0, 2_097_152) {
            Ok(_) => panic!("active client with different remote chunk policy must be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("remote payload chunk policy mismatch"),
            "unexpected error: {err}"
        );
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn released_timeout_policy_mismatch_replaces_idle_entry() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9992";

        let first = pool
            .acquire_with_options(url, false, 300.0, 1_048_576)
            .unwrap();
        pool.release(url);

        let second = pool
            .acquire_with_options(url, false, 900.0, 1_048_576)
            .unwrap();

        assert!(
            !Arc::ptr_eq(&first, &second),
            "idle entry with stale timeout policy should be replaced"
        );
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn test_pool_sweep_expired() {
        let pool = HttpClientPool::new(0.0); // zero grace

        let url = "http://localhost:9998";
        let _client = pool.acquire_with_proxy_policy(url, false).unwrap();
        pool.release(url);

        // After release with zero grace, sweep should remove it.
        std::thread::sleep(Duration::from_millis(10));
        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_shutdown_all() {
        let pool = HttpClientPool::new(60.0);
        let _c1 = pool
            .acquire_with_proxy_policy("http://localhost:9997", false)
            .unwrap();
        let _c2 = pool
            .acquire_with_proxy_policy("http://localhost:9996", false)
            .unwrap();
        assert_eq!(pool.active_count(), 2);

        pool.shutdown_all();
        assert_eq!(pool.active_count(), 0);
    }
}
