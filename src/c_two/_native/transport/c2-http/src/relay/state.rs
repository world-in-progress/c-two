//! Central relay state — thread-safe wrapper around RouteTable + ConnectionPool.
//!
//! Lock ordering (must be followed everywhere):
//!   1. route_table (RwLock)
//!   2. conn_pool (RwLock)

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use c2_ipc::IpcClient;
use c2_config::RelayConfig;

use crate::relay::route_table::RouteTable;
use crate::relay::conn_pool::ConnectionPool;
use crate::relay::types::*;

pub struct RelayState {
    route_table: RwLock<RouteTable>,
    conn_pool: RwLock<ConnectionPool>,
    config: Arc<RelayConfig>,
    disseminator: Arc<dyn crate::relay::disseminator::Disseminator>,
}

impl RelayState {
    pub fn new(config: Arc<RelayConfig>, disseminator: Arc<dyn crate::relay::disseminator::Disseminator>) -> Self {
        Self {
            route_table: RwLock::new(RouteTable::new(config.relay_id.clone())),
            conn_pool: RwLock::new(ConnectionPool::new()),
            disseminator,
            config,
        }
    }

    pub fn disseminator(&self) -> &Arc<dyn crate::relay::disseminator::Disseminator> {
        &self.disseminator
    }

    pub fn config(&self) -> &RelayConfig { &self.config }
    pub fn relay_id(&self) -> &str { &self.config.relay_id }

    // -- Transactional: route + connection together --

    /// Register a LOCAL upstream CRM.
    pub fn register_upstream(
        &self, name: String, address: String,
        icrm_ns: String, icrm_ver: String,
        client: Arc<IpcClient>,
    ) -> RouteEntry {
        let entry = RouteEntry {
            name: name.clone(),
            relay_id: self.config.relay_id.clone(),
            relay_url: self.config.effective_advertise_url(),
            ipc_address: Some(address.clone()),
            icrm_ns, icrm_ver,
            locality: Locality::Local,
            registered_at: now_secs(),
        };
        // Lock order: route_table then conn_pool
        self.route_table.write().register_route(entry.clone());
        self.conn_pool.write().insert(name, address, client);
        entry
    }

    /// Unregister a LOCAL upstream CRM.
    pub fn unregister_upstream(&self, name: &str) -> Option<(RouteEntry, Option<Arc<IpcClient>>)> {
        let relay_id = self.config.relay_id.clone();
        let entry = self.route_table.write().unregister_route(name, &relay_id);
        let client = self.conn_pool.write().remove(name);
        entry.map(|e| (e, client))
    }

    // -- Route-only operations --

    pub fn resolve(&self, name: &str) -> Vec<RouteInfo> {
        self.route_table.read().resolve(name)
    }

    pub fn has_local_route(&self, name: &str) -> bool {
        self.route_table.read().has_local_route(name)
    }

    pub fn route_names(&self) -> Vec<String> {
        self.route_table.read().route_names()
    }

    pub fn list_routes(&self) -> Vec<RouteEntry> {
        self.route_table.read().list_routes()
    }

    // -- Connection-only operations --

    pub fn get_client(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.conn_pool.read().get(name)
    }

    pub fn touch_connection(&self, name: &str) {
        self.conn_pool.read().touch(name);
    }

    pub fn get_address(&self, name: &str) -> Option<String> {
        self.conn_pool.read().get_address(name)
    }

    pub fn evict_idle(&self, idle_timeout_ms: u64) -> Vec<(String, Option<Arc<IpcClient>>)> {
        let mut cp = self.conn_pool.write();
        let names = cp.idle_entries(idle_timeout_ms);
        names.into_iter().map(|n| { let c = cp.evict(&n); (n, c) }).collect()
    }

    pub fn evict_connection(&self, name: &str) -> Option<Arc<IpcClient>> {
        self.conn_pool.write().evict(name)
    }

    pub fn reconnect(&self, name: &str, client: Arc<IpcClient>) {
        self.conn_pool.write().reconnect(name, client);
    }

    pub fn has_connection(&self, name: &str) -> bool {
        self.conn_pool.read().has_entry(name)
    }

    // -- PEER route operations (gossip) --

    pub fn register_peer_route(&self, entry: RouteEntry) {
        self.route_table.write().register_route(entry);
    }

    pub fn unregister_peer_route(&self, name: &str, relay_id: &str) {
        self.route_table.write().unregister_route(name, relay_id);
    }

    pub fn remove_routes_by_relay(&self, relay_id: &str) -> Vec<RouteEntry> {
        self.route_table.write().remove_routes_by_relay(relay_id)
    }

    // -- Peer management --

    pub fn register_peer(&self, info: PeerInfo) {
        self.route_table.write().register_peer(info);
    }

    pub fn unregister_peer(&self, relay_id: &str) -> Option<PeerInfo> {
        self.route_table.write().unregister_peer(relay_id)
    }

    pub fn list_peers(&self) -> Vec<PeerSnapshot> {
        self.route_table.read().list_peers().into_iter().map(|p| PeerSnapshot {
            relay_id: p.relay_id.clone(), url: p.url.clone(),
            route_count: p.route_count, status: p.status,
        }).collect()
    }

    pub fn local_route_count(&self) -> u32 {
        self.route_table.read().local_route_count()
    }

    // -- Snapshot operations --

    pub fn full_snapshot(&self) -> FullSync { self.route_table.read().full_snapshot() }

    pub fn merge_snapshot(&self, sync: FullSync) { self.route_table.write().merge_snapshot(sync); }

    pub fn route_digest(&self) -> HashMap<(String, String), u64> {
        self.route_table.read().route_digest()
    }

    pub fn with_route_table<F, R>(&self, f: F) -> R where F: FnOnce(&RouteTable) -> R {
        f(&self.route_table.read())
    }

    pub fn with_route_table_mut<F, R>(&self, f: F) -> R where F: FnOnce(&mut RouteTable) -> R {
        f(&mut self.route_table.write())
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullDisseminator;
    impl crate::relay::disseminator::Disseminator for NullDisseminator {
        fn broadcast(&self, _envelope: crate::relay::peer::PeerEnvelope, _peers: &[PeerSnapshot]) -> Option<tokio::task::JoinHandle<()>> {
            None
        }
    }

    fn test_config() -> Arc<RelayConfig> {
        Arc::new(RelayConfig {
            relay_id: "test-relay".into(),
            advertise_url: "http://localhost:9999".into(),
            ..Default::default()
        })
    }

    fn null_disseminator() -> Arc<dyn crate::relay::disseminator::Disseminator> {
        Arc::new(NullDisseminator)
    }

    #[test]
    fn register_and_resolve_upstream() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        state.register_upstream("grid".into(), "ipc://grid".into(),
            "test.ns".into(), "0.1.0".into(), client);
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
    }

    #[test]
    fn unregister_upstream() {
        let state = RelayState::new(test_config(), null_disseminator());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        state.register_upstream("grid".into(), "ipc://grid".into(),
            "test.ns".into(), "0.1.0".into(), client);
        assert!(state.unregister_upstream("grid").is_some());
        assert!(state.resolve("grid").is_empty());
    }

    #[test]
    fn peer_route_operations() {
        let state = RelayState::new(test_config(), null_disseminator());
        state.register_peer_route(RouteEntry {
            name: "remote".into(), relay_id: "peer-1".into(),
            relay_url: "http://peer-1:8080".into(), ipc_address: None,
            icrm_ns: "ns".into(), icrm_ver: "0.1.0".into(),
            locality: Locality::Peer, registered_at: 1000.0,
        });
        assert_eq!(state.resolve("remote").len(), 1);
        state.unregister_peer_route("remote", "peer-1");
        assert!(state.resolve("remote").is_empty());
    }
}
