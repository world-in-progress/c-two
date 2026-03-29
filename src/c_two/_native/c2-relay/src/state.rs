//! Multi-upstream relay state.
//!
//! Manages a pool of IPC connections, each keyed by a user-chosen route
//! name. Supports dynamic registration and removal of upstreams.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use c2_ipc::IpcClient;

/// A single upstream connection entry.
struct UpstreamEntry {
    address: String,
    client: Arc<IpcClient>,
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
        self.entries.insert(name, UpstreamEntry { address, client });
        Ok(())
    }

    /// Check if a route name is already registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Unregister an upstream by name.
    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        match self.entries.remove(name) {
            Some(_entry) => Ok(()),
            None => Err(format!("Route name not registered: '{name}'")),
        }
    }

    /// Get a cloned Arc to an upstream's IpcClient.
    ///
    /// The caller gets an owned Arc — no need to hold the pool lock
    /// while making IPC calls.
    pub fn get(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.entries.get(name).map(|e| e.client.clone())
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
        assert!(result.unwrap_err().contains("not registered"));
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
        let pool = state.pool.read().unwrap();
        assert!(pool.list_routes().is_empty());
    }
}
