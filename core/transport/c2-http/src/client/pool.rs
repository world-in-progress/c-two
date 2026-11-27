//! Reference-counted pool of [`HttpClient`] instances.

use c2_config::ResolvedRelayClientConfig;
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
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
/// Clients connecting to the same relay URL and client policy share a single
/// `HttpClient` and its control and data pools.  When all references
/// are released, the client is kept for the idle grace period before being
/// destroyed.
pub struct HttpClientPool {
    entries: Mutex<HashMap<(String, ResolvedRelayClientConfig), PoolEntry>>,
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
    /// Create a new pool with the given idle grace period. Production pools
    /// recycle idle HTTP connections on [`c2_config::IDLE_CONNECTION_TIMEOUT`];
    /// tests may construct shorter periods.
    fn new(grace: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            grace_period: grace,
            default_max_connections: 100,
        }
    }

    pub(super) fn acquire_lease(
        &self,
        base_url: &str,
        policy: &ResolvedRelayClientConfig,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<HttpPoolLease, HttpError> {
        self.acquire_view(base_url, policy, deadline)
            .map(|(client, references)| HttpPoolLease { client, references })
    }

    fn acquire_view(
        &self,
        base_url: &str,
        policy: &ResolvedRelayClientConfig,
        deadline: c2_config::ConnectDeadline,
    ) -> Result<(Arc<HttpClient>, Arc<PoolReferences>), HttpError> {
        super::connect_deadline::check(deadline, "relay_pool_acquire")?;
        let key = (canonical_base_url(base_url), policy.clone());

        let mut entries =
            super::connect_deadline::lock(&self.entries, deadline, "relay_pool_acquire")?;
        entries.retain(|_, entry| !entry.references.expired(self.grace_period));

        if let Some(entry) = entries.get(&key) {
            entry.references.acquire();
            return Ok((Arc::clone(&entry.client), Arc::clone(&entry.references)));
        }

        // Publish both pools together after the trust policy has been loaded.
        let client = Arc::new(HttpClient::new(
            &key.0,
            self.default_max_connections,
            policy,
        )?);

        super::connect_deadline::check(deadline, "relay_pool_acquire")?;
        let references = Arc::new(PoolReferences::new());
        entries.insert(
            key,
            PoolEntry {
                client: Arc::clone(&client),
                references: Arc::clone(&references),
            },
        );

        Ok((client, references))
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
        let entries = self.entries.lock();
        let url = canonical_base_url(base_url);
        entries
            .iter()
            .filter_map(|((base, _), entry)| (base == &url).then_some(entry))
            .map(|entry| entry.references.count.load(Ordering::Acquire))
            .sum()
    }
}

// ── Singleton ───────────────────────────────────────────────────────────

static GLOBAL_HTTP_POOL: OnceLock<HttpClientPool> = OnceLock::new();

impl HttpClientPool {
    /// Return the process-level singleton.
    pub fn instance() -> &'static HttpClientPool {
        GLOBAL_HTTP_POOL.get_or_init(|| HttpClientPool::new(c2_config::IDLE_CONNECTION_TIMEOUT))
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(pool: &HttpClientPool, url: &str, use_proxy: bool) -> HttpPoolLease {
        pool.acquire_lease(
            url,
            &ResolvedRelayClientConfig {
                use_proxy,
                ..Default::default()
            },
            c2_config::ConnectDeadline::default(),
        )
        .unwrap()
    }

    #[test]
    fn connect_lease_release_cannot_decrement_replacement_client() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let url = "http://localhost:9986";
        let old = lease(&pool, url, false);
        pool.shutdown_all();
        let replacement = lease(&pool, url, false);
        assert!(!Arc::ptr_eq(&old.client, &replacement.client));
        drop(old);
        assert_eq!(pool.refcount(url), 1);
        drop(replacement);
        assert_eq!(pool.refcount(url), 0);
    }

    #[test]
    fn connect_lease_release_does_not_wait_for_pool_lock() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let url = "http://localhost:9987";
        let lease = lease(&pool, url, false);
        let held = pool.entries.lock();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let releaser = std::thread::spawn(move || {
            drop(lease);
            released_tx.send(()).unwrap();
        });
        let released_while_map_held = released_rx.recv_timeout(Duration::from_millis(500)).is_ok();
        let count = held[&(url.into(), Default::default())]
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
        let pool = Arc::new(HttpClientPool::new(Duration::from_secs(60)));
        let held = pool.entries.lock();
        let waiter_pool = Arc::clone(&pool);
        let waiter = std::thread::spawn(move || {
            let deadline = c2_config::ConnectDeadline::start(
                c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(30)),
            )
            .unwrap();
            let start = Instant::now();
            let error = waiter_pool
                .acquire_lease("http://localhost:9988", &Default::default(), deadline)
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
        let client = lease(&pool, "http://localhost:9988", false);
        assert_eq!(pool.refcount("http://localhost:9988"), 1);
        drop(client);
        assert_eq!(pool.refcount("http://localhost:9988"), 0);
    }

    #[test]
    fn test_pool_new() {
        let pool = HttpClientPool::new(Duration::from_secs(30));
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_singleton() {
        let p1 = HttpClientPool::instance() as *const HttpClientPool;
        let p2 = HttpClientPool::instance() as *const HttpClientPool;
        assert_eq!(p1, p2, "singleton must return the same instance");
    }

    #[test]
    fn singleton_recycles_idle_connections_on_the_shared_idle_timeout() {
        assert_eq!(
            HttpClientPool::instance().grace_period,
            c2_config::IDLE_CONNECTION_TIMEOUT
        );
    }

    #[test]
    fn trailing_slash_variants_share_one_pool_entry() {
        let pool = HttpClientPool::new(Duration::from_secs(60));

        let first = lease(&pool, "http://localhost:9998", false);
        let second = lease(&pool, "http://localhost:9998/", false);

        assert!(Arc::ptr_eq(&first.client, &second.client));
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.refcount("http://localhost:9998"), 2);
        assert_eq!(pool.refcount("http://localhost:9998/"), 2);

        drop(second);
        assert_eq!(pool.refcount("http://localhost:9998"), 1);
        drop(first);
        assert_eq!(pool.refcount("http://localhost:9998/"), 0);
    }

    #[test]
    fn proxy_policies_use_independent_pool_entries() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let url = "http://localhost:9995";

        let direct = lease(&pool, url, false);
        let proxied = lease(&pool, url, true);
        let same_policy = lease(&pool, url, true);
        assert!(!Arc::ptr_eq(&direct.client, &proxied.client));
        assert!(Arc::ptr_eq(&proxied.client, &same_policy.client));
        assert_eq!(pool.active_count(), 2);
        assert_eq!(pool.refcount(url), 3);
        drop((proxied, same_policy));
        assert_eq!(pool.refcount(url), 1);
    }

    #[test]
    fn tls_policies_use_independent_pool_entries() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let url = "https://localhost:9995";
        let pinned = |byte| ResolvedRelayClientConfig {
            tls_trust: c2_config::RelayTlsTrust::CertSha256([byte; 32]),
            ..Default::default()
        };
        let acquire = |policy: &ResolvedRelayClientConfig| {
            pool.acquire_lease(url, policy, c2_config::ConnectDeadline::default())
                .unwrap()
        };
        let default_client = acquire(&Default::default());
        let first_pin = acquire(&pinned(1));
        let second_pin = acquire(&pinned(2));
        let same_pin = acquire(&pinned(1));
        assert!(!Arc::ptr_eq(&default_client.client, &first_pin.client));
        assert!(!Arc::ptr_eq(&first_pin.client, &second_pin.client));
        assert!(Arc::ptr_eq(&first_pin.client, &same_pin.client));
        assert_eq!(pool.refcount(url), 4);
    }

    #[test]
    fn released_entry_is_reused_for_its_own_proxy_policy() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let url = "http://localhost:9994";

        let first = lease(&pool, url, false);
        let first_client = Arc::clone(&first.client);
        drop(first);

        let proxied = lease(&pool, url, true);
        let direct = lease(&pool, url, false);
        assert!(!Arc::ptr_eq(&first_client, &proxied.client));
        assert!(Arc::ptr_eq(&first_client, &direct.client));
        assert_eq!(pool.refcount(url), 2);
    }

    #[test]
    fn test_pool_sweep_expired() {
        let pool = HttpClientPool::new(Duration::ZERO); // zero grace

        let url = "http://localhost:9998";
        let client = lease(&pool, url, false);
        drop(client);

        // After release with zero grace, sweep should remove it.
        std::thread::sleep(Duration::from_millis(10));
        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_shutdown_all() {
        let pool = HttpClientPool::new(Duration::from_secs(60));
        let _c1 = lease(&pool, "http://localhost:9997", false);
        let _c2 = lease(&pool, "http://localhost:9996", false);
        assert_eq!(pool.active_count(), 2);

        pool.shutdown_all();
        assert_eq!(pool.active_count(), 0);
    }
}
