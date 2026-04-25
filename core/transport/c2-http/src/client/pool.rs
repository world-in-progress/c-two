//! Reference-counted pool of [`HttpClient`] instances.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;
use std::time::{Duration, Instant};

use crate::client::{HttpClient, HttpError};

// ── Pool entry ──────────────────────────────────────────────────────────

struct PoolEntry {
    client: Arc<HttpClient>,
    ref_count: usize,
    /// Set to `Some(Instant::now())` when `ref_count` drops to 0.
    last_release: Option<Instant>,
}

// ── HttpClientPool ──────────────────────────────────────────────────────

/// Reference-counted pool of [`HttpClient`] instances.
///
/// Clients connecting to the same relay URL share a single
/// `HttpClient` (and its underlying connection pool).  When all
/// references are released, the client is kept for a grace period
/// before being destroyed.
pub struct HttpClientPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    grace_period: Duration,
    default_timeout: f64,
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
            default_timeout: 30.0,
            default_max_connections: 100,
        }
    }

    /// Acquire (or create) a client for `base_url`.
    pub fn acquire(&self, base_url: &str) -> Result<Arc<HttpClient>, HttpError> {
        self.sweep_expired();

        let mut entries = self.entries.lock();

        if let Some(entry) = entries.get_mut(base_url) {
            entry.ref_count += 1;
            entry.last_release = None;
            return Ok(Arc::clone(&entry.client));
        }

        // Create a new client (lock held — HttpClient::new is fast).
        let client = Arc::new(HttpClient::new(
            base_url,
            self.default_timeout,
            self.default_max_connections,
        )?);

        entries.insert(
            base_url.to_owned(),
            PoolEntry {
                client: Arc::clone(&client),
                ref_count: 1,
                last_release: None,
            },
        );

        Ok(client)
    }

    /// Decrement reference count; mark for grace-period cleanup at 0.
    pub fn release(&self, base_url: &str) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.get_mut(base_url) {
            if entry.ref_count == 0 {
                eprintln!(
                    "HttpClientPool::release: ref_count already 0 for {base_url}"
                );
                return;
            }
            entry.ref_count -= 1;
            if entry.ref_count == 0 {
                entry.last_release = Some(Instant::now());
            }
        }
    }

    /// Sweep entries past the grace period.
    pub fn sweep_expired(&self) {
        let mut entries = self.entries.lock();
        let grace = self.grace_period;
        entries.retain(|_url, entry| {
            if entry.ref_count == 0 {
                if let Some(released_at) = entry.last_release {
                    if released_at.elapsed() >= grace {
                        return false;
                    }
                }
            }
            true
        });
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
            .get(base_url)
            .map_or(0, |e| e.ref_count)
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
    fn test_pool_acquire_release() {
        let pool = HttpClientPool::new(60.0);
        let url = "http://localhost:9999";

        let _client = pool.acquire(url).unwrap();
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.refcount(url), 1);

        let _client2 = pool.acquire(url).unwrap();
        assert_eq!(pool.refcount(url), 2);

        pool.release(url);
        assert_eq!(pool.refcount(url), 1);

        pool.release(url);
        assert_eq!(pool.refcount(url), 0);
    }

    #[test]
    fn test_pool_sweep_expired() {
        let pool = HttpClientPool::new(0.0); // zero grace

        let url = "http://localhost:9998";
        let _client = pool.acquire(url).unwrap();
        pool.release(url);

        // After release with zero grace, sweep should remove it.
        std::thread::sleep(Duration::from_millis(10));
        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_shutdown_all() {
        let pool = HttpClientPool::new(60.0);
        let _c1 = pool.acquire("http://localhost:9997").unwrap();
        let _c2 = pool.acquire("http://localhost:9996").unwrap();
        assert_eq!(pool.active_count(), 2);

        pool.shutdown_all();
        assert_eq!(pool.active_count(), 0);
    }
}
