use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use c2_ipc::IpcClient;

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A managed IPC connection with activity tracking.
struct ConnectionEntry {
    address: String,
    client: Option<Arc<IpcClient>>,
    last_activity: AtomicU64,
}

/// Pool of IPC connections keyed by route name.
///
/// Separated from RouteTable to keep route metadata independent of
/// connection lifecycle. Supports lazy reconnection and idle eviction.
pub struct ConnectionPool {
    entries: HashMap<String, ConnectionEntry>,
}

impl ConnectionPool {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Insert a pre-connected client for a route name.
    pub fn insert(&mut self, name: String, address: String, client: Arc<IpcClient>) {
        self.entries.insert(name, ConnectionEntry {
            address,
            client: Some(client),
            last_activity: AtomicU64::new(now_millis()),
        });
    }

    /// Get a connected client for a route name.
    /// Returns None if not registered, evicted, or disconnected.
    pub fn get(&self, name: &str) -> Option<Arc<IpcClient>> {
        let entry = self.entries.get(name)?;
        let client = entry.client.as_ref()?;
        if client.is_connected() { Some(client.clone()) } else { None }
    }

    /// Check if an entry exists (even if evicted).
    pub fn has_entry(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Get stored address for reconnection.
    pub fn get_address(&self, name: &str) -> Option<String> {
        self.entries.get(name).map(|e| e.address.clone())
    }

    /// Touch activity timestamp (lock-free, Relaxed).
    pub fn touch(&self, name: &str) {
        if let Some(entry) = self.entries.get(name) {
            entry.last_activity.store(now_millis(), Ordering::Relaxed);
        }
    }

    /// Names of entries to evict (dead or idle beyond timeout_ms).
    pub fn idle_entries(&self, idle_timeout_ms: u64) -> Vec<String> {
        let cutoff = now_millis().saturating_sub(idle_timeout_ms);
        self.entries.iter()
            .filter(|(_, e)| {
                if let Some(ref client) = e.client {
                    !client.is_connected() || e.last_activity.load(Ordering::Relaxed) < cutoff
                } else {
                    false
                }
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Evict a client — returns old Arc for async close.
    pub fn evict(&mut self, name: &str) -> Option<Arc<IpcClient>> {
        self.entries.get_mut(name).and_then(|e| e.client.take())
    }

    /// Re-attach a freshly connected client.
    pub fn reconnect(&mut self, name: &str, client: Arc<IpcClient>) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.client = Some(client);
            entry.last_activity.store(now_millis(), Ordering::Relaxed);
        }
    }

    /// Remove entry entirely.
    pub fn remove(&mut self, name: &str) -> Option<Arc<IpcClient>> {
        self.entries.remove(name).and_then(|e| e.client)
    }

    /// List route names with addresses.
    pub fn list_connections(&self) -> Vec<(String, String)> {
        self.entries.iter().map(|(n, e)| (n.clone(), e.address.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pool_is_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.list_connections().is_empty());
    }

    #[test]
    fn insert_and_get() {
        let mut pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        client.force_connected(true);
        pool.insert("grid".into(), "ipc://test".into(), client);
        assert!(pool.get("grid").is_some());
    }

    #[test]
    fn get_returns_none_for_disconnected() {
        let mut pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        // Client starts disconnected.
        pool.insert("grid".into(), "ipc://test".into(), client);
        assert!(pool.get("grid").is_none());
    }

    #[test]
    fn evict_and_reconnect() {
        let mut pool = ConnectionPool::new();
        let c1 = Arc::new(IpcClient::new("ipc://test"));
        c1.force_connected(true);
        pool.insert("grid".into(), "ipc://test".into(), c1);
        pool.evict("grid");
        assert!(pool.get("grid").is_none());
        assert!(pool.has_entry("grid"));

        let c2 = Arc::new(IpcClient::new("ipc://test"));
        c2.force_connected(true);
        pool.reconnect("grid", c2);
        assert!(pool.get("grid").is_some());
    }

    #[test]
    fn remove_deletes_entry() {
        let mut pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://test"));
        pool.insert("grid".into(), "ipc://test".into(), client);
        pool.remove("grid");
        assert!(!pool.has_entry("grid"));
    }

    #[test]
    fn idle_entries_detects_disconnected() {
        let mut pool = ConnectionPool::new();
        let client = Arc::new(IpcClient::new("ipc://dead"));
        pool.insert("d".into(), "ipc://dead".into(), client);
        assert_eq!(pool.idle_entries(u64::MAX).len(), 1);
    }
}
