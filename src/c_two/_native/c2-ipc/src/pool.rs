//! Reference-counted pool of [`SyncClient`] instances.
//!
//! Clients connecting to the same server address share a single
//! `SyncClient`. When all references are released the client is
//! kept alive for a grace period before being destroyed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use c2_mem::{MemPool, PoolConfig};

/// Monotonic counter so each client MemPool gets a unique SHM prefix
/// within the same process.  Format: `/cc3c{pid:08x}{counter:08x}`.
///
/// Uses 32-bit range (~4 billion unique prefixes per process).
/// Combined with `_b{idx:04x}` suffix, max SHM name length is 27 chars
/// (within POSIX 31-char limit on macOS).
static CLIENT_POOL_GEN: AtomicU64 = AtomicU64::new(0);

use crate::client::{IpcConfig, IpcError};
use crate::sync_client::SyncClient;

// ── Pool entry ───────────────────────────────────────────────────────────

struct PoolEntry {
    client: Arc<SyncClient>,
    ref_count: usize,
    /// Set to `Some(Instant::now())` when `ref_count` drops to 0.
    last_release: Option<Instant>,
}

// ── ClientPool ───────────────────────────────────────────────────────────

/// Reference-counted pool of `SyncClient` instances.
///
/// Clients connecting to the same server address share a single
/// `SyncClient`. When all references are released, the client is
/// kept alive for a grace period before being destroyed.
pub struct ClientPool {
    entries: StdMutex<HashMap<String, PoolEntry>>,
    grace_period: Duration,
    default_config: StdMutex<Option<IpcConfig>>,
}

// Compile-time assertion: ClientPool must be Send + Sync.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _assert_sync<T: Sync>() {}
    fn _assertions() {
        _assert_send::<ClientPool>();
        _assert_sync::<ClientPool>();
    }
};

impl ClientPool {
    /// Create a new pool with the given grace period.
    pub fn new(grace_period: Duration) -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
            grace_period,
            default_config: StdMutex::new(None),
        }
    }

    /// Set the default IPC config for newly created clients.
    pub fn set_default_config(&self, config: IpcConfig) {
        *self.default_config.lock().unwrap() = Some(config);
    }

    /// Acquire a client for `address`. Creates and connects if needed.
    /// Increments reference count.
    pub fn acquire(
        &self,
        address: &str,
        config: Option<&IpcConfig>,
    ) -> Result<Arc<SyncClient>, IpcError> {
        // Sweep stale entries before potentially creating a new one.
        self.sweep_expired();

        let mut entries = self.entries.lock().unwrap();

        // Fast path: existing connected client.
        if let Some(entry) = entries.get_mut(address) {
            if entry.client.is_connected() {
                entry.ref_count += 1;
                entry.last_release = None;
                return Ok(Arc::clone(&entry.client));
            }
            // Stale — remove and fall through to create a new one.
            entries.remove(address);
        }

        // Resolve config: explicit > default > IpcConfig::default().
        let cfg = match config {
            Some(c) => c.clone(),
            None => self
                .default_config
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default(),
        };

        // Create a fresh MemPool for the client, respecting pool_segment_size.
        let mut pc = PoolConfig::default();
        pc.segment_size = cfg.pool_segment_size as usize;
        let counter = CLIENT_POOL_GEN.fetch_add(1, Ordering::Relaxed) as u32;
        let prefix = format!("/cc3c{:08x}{:08x}", std::process::id(), counter);
        let pool = Arc::new(StdMutex::new(MemPool::new_with_prefix(pc, prefix)));

        // Drop the entries lock before connecting (connect may block).
        drop(entries);

        let client = SyncClient::connect(address, Some(pool), cfg)?;
        let client = Arc::new(client);

        let mut entries = self.entries.lock().unwrap();

        // Another thread may have raced and inserted the same address.
        if let Some(entry) = entries.get_mut(address) {
            if entry.client.is_connected() {
                entry.ref_count += 1;
                entry.last_release = None;
                return Ok(Arc::clone(&entry.client));
            }
            // Stale racing entry — replace below.
        }

        entries.insert(
            address.to_owned(),
            PoolEntry {
                client: Arc::clone(&client),
                ref_count: 1,
                last_release: None,
            },
        );

        Ok(client)
    }

    /// Decrement reference count. When it reaches 0, mark for grace-period
    /// cleanup.
    pub fn release(&self, address: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(address) {
            if entry.ref_count == 0 {
                eprintln!("ClientPool::release: ref_count already 0 for {address}");
                return;
            }
            entry.ref_count -= 1;
            if entry.ref_count == 0 {
                entry.last_release = Some(Instant::now());
            }
        } else {
            eprintln!("ClientPool::release: no entry for {address}");
        }
    }

    /// Sweep expired entries that have been unreferenced longer than
    /// `grace_period`. Call this periodically (e.g., from Python on a
    /// timer or before acquire).
    pub fn sweep_expired(&self) {
        let mut entries = self.entries.lock().unwrap();
        let grace = self.grace_period;
        entries.retain(|_addr, entry| {
            if entry.ref_count == 0 {
                if let Some(released_at) = entry.last_release {
                    if released_at.elapsed() >= grace {
                        // Drop the Arc — connection closes when last ref is gone.
                        return false;
                    }
                }
            }
            true
        });
    }

    /// Destroy all clients immediately (for shutdown / testing).
    pub fn shutdown_all(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.clear(); // Arcs are dropped → connections close.
    }

    /// Number of active entries (for testing).
    pub fn active_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Reference count for an address (for testing).
    pub fn refcount(&self, address: &str) -> usize {
        self.entries
            .lock()
            .unwrap()
            .get(address)
            .map_or(0, |e| e.ref_count)
    }

    /// Check if a client exists for the address (for testing).
    pub fn has_client(&self, address: &str) -> bool {
        self.entries.lock().unwrap().contains_key(address)
    }
}

// ── Singleton ────────────────────────────────────────────────────────────

static GLOBAL_POOL: OnceLock<ClientPool> = OnceLock::new();

impl ClientPool {
    /// Return the process-level singleton.
    pub fn instance() -> &'static ClientPool {
        GLOBAL_POOL.get_or_init(|| ClientPool::new(Duration::from_secs(60)))
    }

    /// Reset the singleton (testing only).
    /// `OnceLock` cannot be truly reset, so this shuts down all entries.
    pub fn reset_instance() {
        if let Some(pool) = GLOBAL_POOL.get() {
            pool.shutdown_all();
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_new() {
        let pool = ClientPool::new(Duration::from_secs(30));
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_singleton() {
        let p1 = ClientPool::instance() as *const ClientPool;
        let p2 = ClientPool::instance() as *const ClientPool;
        assert_eq!(p1, p2, "singleton must return the same instance");
    }

    #[test]
    fn test_pool_grace_period_sweep() {
        let pool = ClientPool::new(Duration::from_millis(50));

        // Manually insert a fake entry with ref_count=0 and old release time.
        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock().unwrap();
            entries.insert(
                "ipc://fake".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now() - Duration::from_millis(200)),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(pool.active_count(), 0, "expired entry should be swept");
    }

    #[test]
    fn test_pool_grace_period_not_expired() {
        let pool = ClientPool::new(Duration::from_secs(60));

        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock().unwrap();
            entries.insert(
                "ipc://recent".to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }
        assert_eq!(pool.active_count(), 1);

        pool.sweep_expired();
        assert_eq!(pool.active_count(), 1, "recently released entry should survive");
    }

    #[test]
    fn test_pool_refcount_tracking() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://reftest";

        // Insert a fake connected-looking entry for refcount math.
        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock().unwrap();
            entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 2,
                    last_release: None,
                },
            );
        }

        assert_eq!(pool.refcount(addr), 2);

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 1);
        assert!(pool.has_client(addr));

        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
        // last_release should now be set; entry still present.
        assert!(pool.has_client(addr));
    }

    #[test]
    fn test_pool_release_unknown_address() {
        // Should not panic — just print a warning.
        let pool = ClientPool::new(Duration::from_secs(30));
        pool.release("ipc://nonexistent");
    }

    #[test]
    fn test_pool_release_already_zero() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let addr = "ipc://zero";

        {
            let client = make_disconnected_client();
            let mut entries = pool.entries.lock().unwrap();
            entries.insert(
                addr.to_owned(),
                PoolEntry {
                    client: Arc::new(client),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }

        // Should not panic or underflow.
        pool.release(addr);
        assert_eq!(pool.refcount(addr), 0);
    }

    #[test]
    fn test_pool_shutdown_all() {
        let pool = ClientPool::new(Duration::from_secs(30));

        {
            let c1 = make_disconnected_client();
            let c2 = make_disconnected_client();
            let mut entries = pool.entries.lock().unwrap();
            entries.insert(
                "ipc://a".to_owned(),
                PoolEntry {
                    client: Arc::new(c1),
                    ref_count: 1,
                    last_release: None,
                },
            );
            entries.insert(
                "ipc://b".to_owned(),
                PoolEntry {
                    client: Arc::new(c2),
                    ref_count: 0,
                    last_release: Some(Instant::now()),
                },
            );
        }
        assert_eq!(pool.active_count(), 2);

        pool.shutdown_all();
        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_pool_acquire_no_server() {
        // acquire() should fail gracefully when no server is listening.
        let pool = ClientPool::new(Duration::from_secs(30));
        let result = pool.acquire("ipc:///nonexistent_socket_path", None);
        assert!(result.is_err(), "acquire without server should fail");
    }

    #[test]
    fn test_pool_set_default_config() {
        let pool = ClientPool::new(Duration::from_secs(30));
        let cfg = IpcConfig {
            shm_threshold: 1024,
            chunk_size: 65536,
            ..IpcConfig::default()
        };
        pool.set_default_config(cfg);
        // Verify the config is stored (indirectly — acquire would use it).
        let stored = pool.default_config.lock().unwrap();
        assert!(stored.is_some());
        let c = stored.as_ref().unwrap();
        assert_eq!(c.shm_threshold, 1024);
        assert_eq!(c.chunk_size, 65536);
    }

    // ── Helper ───────────────────────────────────────────────────────────

    /// Create a disconnected SyncClient for testing pool bookkeeping.
    fn make_disconnected_client() -> SyncClient {
        SyncClient::new_unconnected("ipc:///pool_test_fake")
    }
}
