//! Multi-upstream relay state.
//!
//! Manages a pool of IPC connections, each keyed by a user-chosen route
//! name. Supports dynamic registration, removal, idle eviction, and
//! lazy reconnection of upstreams.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use c2_ipc::IpcClient;

/// Current wall-clock time in milliseconds since UNIX epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A single upstream connection entry.
///
/// `client` is `None` when the connection has been evicted due to
/// idleness. The address is retained so that a lazy reconnect can
/// re-establish the connection on the next request.
struct UpstreamEntry {
    address: String,
    client: Option<Arc<IpcClient>>,
    last_activity: AtomicU64,
}

/// Thread-safe pool of upstream IPC connections.
pub struct UpstreamPool {
    entries: HashMap<String, UpstreamEntry>,
}

impl UpstreamPool {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Insert a pre-connected upstream.
    pub fn insert(&mut self, name: String, address: String, client: Arc<IpcClient>) -> Result<(), String> {
        if self.entries.contains_key(&name) {
            return Err(format!("Route name already registered: '{name}'"));
        }
        self.entries.insert(
            name,
            UpstreamEntry {
                address,
                client: Some(client),
                last_activity: AtomicU64::new(now_millis()),
            },
        );
        Ok(())
    }

    /// Check if a route name is already registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Unregister an upstream by name. Returns the old client (if any)
    /// so the caller can close it outside the lock.
    pub fn remove(&mut self, name: &str) -> Result<Option<Arc<IpcClient>>, String> {
        match self.entries.remove(name) {
            Some(entry) => Ok(entry.client),
            None => Err(format!("Route name not registered: '{name}'")),
        }
    }

    /// Get a cloned Arc to an upstream's IpcClient.
    ///
    /// Returns `None` if the route is not registered, if the client
    /// was evicted, **or** if the client is disconnected (the caller
    /// should trigger lazy reconnect via [`try_reconnect`]).
    pub fn get(&self, name: &str) -> Option<Arc<IpcClient>> {
        let entry = self.entries.get(name)?;
        let client = entry.client.as_ref()?;
        if client.is_connected() {
            Some(client.clone())
        } else {
            None
        }
    }

    /// Check whether an entry exists (even if evicted).
    pub fn has_entry(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Get the stored address for a route (for reconnection).
    pub fn get_address(&self, name: &str) -> Option<String> {
        self.entries.get(name).map(|e| e.address.clone())
    }

    /// Update `last_activity` to now. Lock-free — only an `AtomicU64`
    /// store with `Relaxed` ordering, safe to call on every request.
    pub fn touch(&self, name: &str) {
        if let Some(entry) = self.entries.get(name) {
            entry.last_activity.store(now_millis(), Ordering::Relaxed);
        }
    }

    /// Return the names of entries whose client should be evicted.
    /// An entry is evictable when it has a live client reference AND
    /// either (a) the connection is dead (`!is_connected()`), or
    /// (b) last activity is older than `idle_timeout_ms` milliseconds.
    pub fn idle_entries(&self, idle_timeout_ms: u64) -> Vec<String> {
        let cutoff = now_millis().saturating_sub(idle_timeout_ms);
        self.entries
            .iter()
            .filter(|(_, e)| {
                if let Some(ref client) = e.client {
                    !client.is_connected()
                        || e.last_activity.load(Ordering::Relaxed) < cutoff
                } else {
                    false
                }
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Evict an idle upstream — sets `client` to `None` and returns
    /// the old `Arc<IpcClient>` so the caller can close it outside
    /// any lock.
    pub fn evict(&mut self, name: &str) -> Option<Arc<IpcClient>> {
        self.entries.get_mut(name).and_then(|e| e.client.take())
    }

    /// Re-attach a freshly connected client to an existing entry.
    pub fn reconnect(&mut self, name: &str, client: Arc<IpcClient>) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.client = Some(client);
            entry.last_activity.store(now_millis(), Ordering::Relaxed);
        }
    }

    /// List all registered routes with their addresses.
    pub fn list_routes(&self) -> Vec<RouteEntry> {
        self.entries
            .iter()
            .map(|(name, entry)| RouteEntry {
                name: name.clone(),
                address: entry.address.clone(),
            })
            .collect()
    }

    /// List registered route names.
    pub fn route_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

/// Serializable route info.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub name: String,
    pub address: String,
}

/// Shared relay state, held in axum's state extractor.
#[derive(Clone)]
pub struct RelayState {
    pub pool: Arc<RwLock<UpstreamPool>>,
}

impl RelayState {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(RwLock::new(UpstreamPool::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_pool_new_is_empty() {
        let pool = UpstreamPool::new();
        assert!(pool.list_routes().is_empty());
        assert!(pool.route_names().is_empty());
    }

    #[test]
    fn upstream_pool_get_missing_returns_none() {
        let pool = UpstreamPool::new();
        assert!(pool.get("nonexistent").is_none());
    }

    #[test]
    fn upstream_pool_remove_missing_returns_err() {
        let mut pool = UpstreamPool::new();
        let result = pool.remove("nonexistent");
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("not registered"));
    }

    #[test]
    fn upstream_pool_insert_duplicate_returns_err() {
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://nonexistent"));
        pool.insert("test".into(), "ipc://nonexistent".into(), client.clone()).unwrap();
        let result = pool.insert("test".into(), "ipc://nonexistent".into(), client);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn relay_state_new_has_empty_pool() {
        let state = RelayState::new();
        let pool = state.pool.read();
        assert!(pool.list_routes().is_empty());
    }

    #[test]
    fn touch_updates_last_activity() {
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        client.force_connected(true);
        pool.insert("r".into(), "ipc://test".into(), client).unwrap();

        // Manually set activity far in the past.
        pool.entries.get("r").unwrap().last_activity.store(0, Ordering::Relaxed);
        assert_eq!(pool.idle_entries(1000).len(), 1);

        pool.touch("r");
        // After touch, the entry should no longer be idle.
        assert!(pool.idle_entries(1000).is_empty());
    }

    #[test]
    fn evict_removes_client_and_keeps_address() {
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://addr"));
        pool.insert("x".into(), "ipc://addr".into(), client).unwrap();

        let old = pool.evict("x");
        assert!(old.is_some());
        // Entry still exists but get() returns None (no live client).
        assert!(pool.has_entry("x"));
        assert!(pool.get("x").is_none());
        assert_eq!(pool.get_address("x").unwrap(), "ipc://addr");
    }

    #[test]
    fn reconnect_restores_client() {
        let mut pool = UpstreamPool::new();
        let c1 = Arc::new(IpcClient::new("ipc://a"));
        c1.force_connected(true);
        pool.insert("y".into(), "ipc://a".into(), c1).unwrap();
        pool.evict("y");
        assert!(pool.get("y").is_none());

        let c2 = Arc::new(IpcClient::new("ipc://a"));
        c2.force_connected(true);
        pool.reconnect("y", c2);
        assert!(pool.get("y").is_some());
    }

    #[test]
    fn idle_entries_skips_evicted() {
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://z"));
        pool.insert("z".into(), "ipc://z".into(), client).unwrap();
        pool.entries.get("z").unwrap().last_activity.store(0, Ordering::Relaxed);

        // Before eviction, entry shows up as idle.
        assert_eq!(pool.idle_entries(1).len(), 1);

        pool.evict("z");
        // After eviction, entry is no longer reported as idle.
        assert!(pool.idle_entries(1).is_empty());
    }

    #[test]
    fn remove_returns_old_client() {
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://rm"));
        pool.insert("rm".into(), "ipc://rm".into(), client).unwrap();

        let old = pool.remove("rm").unwrap();
        assert!(old.is_some());
        assert!(!pool.has_entry("rm"));
    }

    #[test]
    fn idle_entries_returns_disconnected_client() {
        // A client that is NOT idle by time but IS disconnected should
        // still be returned by idle_entries (dead-connection detection).
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://dead"));
        // Client starts disconnected (is_connected() == false).
        assert!(!client.is_connected());
        pool.insert("d".into(), "ipc://dead".into(), client).unwrap();

        // last_activity was just set to now by insert(), so it is NOT
        // idle by time (use a very large timeout to make sure).
        let idle = pool.idle_entries(999_999_999);
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0], "d");
    }

    #[test]
    fn idle_entries_ignores_connected_and_active_client() {
        // A connected client with recent activity must NOT appear idle.
        let mut pool = UpstreamPool::new();
        let client = Arc::new(IpcClient::new("ipc://alive"));
        client.force_connected(true);
        pool.insert("a".into(), "ipc://alive".into(), client).unwrap();

        assert!(pool.idle_entries(999_999_999).is_empty());
    }
}
