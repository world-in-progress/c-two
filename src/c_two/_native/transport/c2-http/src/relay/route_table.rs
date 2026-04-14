use std::collections::HashMap;
use std::time::Instant;

use crate::relay::types::*;

/// Route table — owns all route entries and peer info.
///
/// Primary key for routes: (name, relay_id).
/// Resolve returns LOCAL first, then PEER sorted by (registered_at, relay_id).
pub struct RouteTable {
    /// Routes keyed by (name, relay_id) for O(1) upsert/delete.
    routes: HashMap<(String, String), RouteEntry>,
    /// Known peer relays keyed by relay_id.
    peers: HashMap<String, PeerInfo>,
    /// This relay's ID (for distinguishing LOCAL vs PEER).
    relay_id: String,
}

impl RouteTable {
    pub fn new(relay_id: String) -> Self {
        Self {
            routes: HashMap::new(),
            peers: HashMap::new(),
            relay_id,
        }
    }

    pub fn relay_id(&self) -> &str {
        &self.relay_id
    }

    // -- Route operations --

    /// Register or update a route (upsert semantics).
    pub fn register_route(&mut self, entry: RouteEntry) {
        let key = (entry.name.clone(), entry.relay_id.clone());
        self.routes.insert(key, entry);
    }

    /// Remove a specific route by (name, relay_id).
    pub fn unregister_route(&mut self, name: &str, relay_id: &str) -> Option<RouteEntry> {
        self.routes.remove(&(name.to_string(), relay_id.to_string()))
    }

    /// Check if a LOCAL route with this name exists.
    pub fn has_local_route(&self, name: &str) -> bool {
        self.routes.contains_key(&(name.to_string(), self.relay_id.clone()))
    }

    /// Resolve a name → ordered list of RouteInfo.
    /// LOCAL first, then PEER sorted by (registered_at, relay_id).
    pub fn resolve(&self, name: &str) -> Vec<RouteInfo> {
        let mut local = Vec::new();
        let mut peers = Vec::new();

        for ((n, _), entry) in &self.routes {
            if n != name {
                continue;
            }
            match entry.locality {
                Locality::Local => local.push(entry.to_route_info()),
                Locality::Peer => peers.push(entry),
            }
        }

        // Deterministic sort: (registered_at, relay_id) ascending.
        peers.sort_by(|a, b| {
            a.registered_at
                .partial_cmp(&b.registered_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.relay_id.cmp(&b.relay_id))
        });

        let mut result = local;
        result.extend(peers.into_iter().map(|e| e.to_route_info()));
        result
    }

    /// List all routes.
    pub fn list_routes(&self) -> Vec<RouteEntry> {
        self.routes.values().cloned().collect()
    }

    /// List route names (unique).
    pub fn route_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.routes.values().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// Count of LOCAL routes only (for heartbeat reporting).
    pub fn local_route_count(&self) -> u32 {
        self.routes
            .values()
            .filter(|e| e.locality == Locality::Local)
            .count() as u32
    }

    /// Remove all routes from a specific relay.
    pub fn remove_routes_by_relay(&mut self, relay_id: &str) -> Vec<RouteEntry> {
        let keys: Vec<_> = self
            .routes
            .keys()
            .filter(|(_, rid)| rid == relay_id)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| self.routes.remove(&k))
            .collect()
    }

    /// Purge all LOCAL routes (crash recovery on startup).
    pub fn purge_local_routes(&mut self) {
        let relay_id = self.relay_id.clone();
        self.remove_routes_by_relay(&relay_id);
    }

    // -- Peer operations --

    pub fn register_peer(&mut self, info: PeerInfo) {
        self.peers.insert(info.relay_id.clone(), info);
    }

    pub fn unregister_peer(&mut self, relay_id: &str) -> Option<PeerInfo> {
        self.peers.remove(relay_id)
    }

    pub fn get_peer(&self, relay_id: &str) -> Option<&PeerInfo> {
        self.peers.get(relay_id)
    }

    pub fn get_peer_mut(&mut self, relay_id: &str) -> Option<&mut PeerInfo> {
        self.peers.get_mut(relay_id)
    }

    pub fn list_peers(&self) -> Vec<&PeerInfo> {
        self.peers.values().collect()
    }

    pub fn alive_peers(&self) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.status == PeerStatus::Alive)
            .collect()
    }

    pub fn dead_peers(&self) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.status == PeerStatus::Dead)
            .collect()
    }

    // -- Snapshot operations (for join protocol + anti-entropy) --

    pub fn full_snapshot(&self) -> FullSync {
        FullSync {
            routes: self.list_routes(),
            peers: self
                .peers
                .values()
                .map(|p| PeerSnapshot {
                    relay_id: p.relay_id.clone(),
                    url: p.url.clone(),
                    route_count: p.route_count,
                    status: p.status,
                })
                .collect(),
        }
    }

    /// Merge a FULL_SYNC snapshot (join protocol).
    /// Replaces all PEER routes; preserves LOCAL routes.
    pub fn merge_snapshot(&mut self, sync: FullSync) {
        // Remove all existing PEER routes.
        let peer_keys: Vec<_> = self
            .routes
            .iter()
            .filter(|(_, e)| e.locality == Locality::Peer)
            .map(|(k, _)| k.clone())
            .collect();
        for key in peer_keys {
            self.routes.remove(&key);
        }

        // Insert snapshot routes (mark non-local as Peer).
        for mut entry in sync.routes {
            if entry.relay_id == self.relay_id {
                continue; // Don't overwrite our own LOCAL routes.
            }
            entry.locality = Locality::Peer;
            let key = (entry.name.clone(), entry.relay_id.clone());
            self.routes.insert(key, entry);
        }

        // Merge peers.
        for ps in sync.peers {
            if ps.relay_id == self.relay_id {
                continue; // Don't add ourselves.
            }
            self.peers.entry(ps.relay_id.clone())
                .and_modify(|existing| {
                    existing.url = ps.url.clone();
                    existing.route_count = ps.route_count;
                })
                .or_insert_with(|| {
                    PeerInfo {
                        relay_id: ps.relay_id,
                        url: ps.url,
                        route_count: ps.route_count,
                        last_heartbeat: Instant::now(),
                        status: PeerStatus::Alive,
                    }
                });
        }
    }

    /// Route digest for anti-entropy: (name, relay_id) → hash(registered_at, ipc_address).
    pub fn route_digest(&self) -> HashMap<(String, String), u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        self.routes
            .iter()
            .map(|(key, entry)| {
                let mut hasher = DefaultHasher::new();
                entry.registered_at.to_bits().hash(&mut hasher);
                entry.ipc_address.hash(&mut hasher);
                (key.clone(), hasher.finish())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn local_entry(name: &str, relay_id: &str) -> RouteEntry {
        RouteEntry {
            name: name.into(),
            relay_id: relay_id.into(),
            relay_url: format!("http://{relay_id}:8080"),
            ipc_address: Some(format!("ipc://{name}_{relay_id}")),
            icrm_ns: "test.ns".into(),
            icrm_ver: "0.1.0".into(),
            locality: Locality::Local,
            registered_at: 1000.0,
        }
    }

    fn peer_entry(name: &str, relay_id: &str, registered_at: f64) -> RouteEntry {
        RouteEntry {
            name: name.into(),
            relay_id: relay_id.into(),
            relay_url: format!("http://{relay_id}:8080"),
            ipc_address: None,
            icrm_ns: "test.ns".into(),
            icrm_ver: "0.1.0".into(),
            locality: Locality::Peer,
            registered_at,
        }
    }

    #[test]
    fn new_table_is_empty() {
        let rt = RouteTable::new("relay-a".into());
        assert!(rt.list_routes().is_empty());
        assert!(rt.list_peers().is_empty());
    }

    #[test]
    fn register_and_resolve_local() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        let resolved = rt.resolve("grid");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "grid");
        assert!(resolved[0].ipc_address.is_some());
    }

    #[test]
    fn resolve_local_before_peer() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        rt.register_route(peer_entry("grid", "relay-b", 500.0));
        let resolved = rt.resolve("grid");
        assert_eq!(resolved.len(), 2);
        assert!(resolved[0].ipc_address.is_some()); // LOCAL first
        assert!(resolved[1].ipc_address.is_none()); // PEER second
    }

    #[test]
    fn resolve_peers_sorted_deterministically() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(peer_entry("grid", "relay-c", 1000.0));
        rt.register_route(peer_entry("grid", "relay-b", 1000.0));
        let resolved = rt.resolve("grid");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].relay_url, "http://relay-b:8080");
        assert_eq!(resolved[1].relay_url, "http://relay-c:8080");
    }

    #[test]
    fn upsert_semantics() {
        let mut rt = RouteTable::new("relay-a".into());
        let mut entry = local_entry("grid", "relay-a");
        rt.register_route(entry.clone());
        entry.ipc_address = Some("ipc://new_addr".into());
        entry.registered_at = 2000.0;
        rt.register_route(entry);
        let routes = rt.list_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://new_addr"));
    }

    #[test]
    fn unregister_route() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        assert!(rt.has_local_route("grid"));
        let removed = rt.unregister_route("grid", "relay-a");
        assert!(removed.is_some());
        assert!(!rt.has_local_route("grid"));
    }

    #[test]
    fn remove_routes_by_relay() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(peer_entry("grid", "relay-b", 1000.0));
        rt.register_route(peer_entry("net", "relay-b", 1001.0));
        rt.register_route(local_entry("local", "relay-a"));
        let removed = rt.remove_routes_by_relay("relay-b");
        assert_eq!(removed.len(), 2);
        assert_eq!(rt.list_routes().len(), 1);
    }

    #[test]
    fn purge_local_routes() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        rt.register_route(peer_entry("net", "relay-b", 1000.0));
        rt.purge_local_routes();
        assert!(!rt.has_local_route("grid"));
        assert_eq!(rt.list_routes().len(), 1);
    }

    #[test]
    fn full_snapshot_and_merge() {
        let mut rt_a = RouteTable::new("relay-a".into());
        rt_a.register_route(local_entry("grid", "relay-a"));
        rt_a.register_route(peer_entry("net", "relay-b", 1000.0));
        rt_a.register_peer(PeerInfo {
            relay_id: "relay-b".into(),
            url: "http://relay-b:8080".into(),
            route_count: 1,
            last_heartbeat: Instant::now(),
            status: PeerStatus::Alive,
        });

        let snapshot = rt_a.full_snapshot();

        let mut rt_c = RouteTable::new("relay-c".into());
        rt_c.register_route(local_entry("local_c", "relay-c"));
        rt_c.merge_snapshot(snapshot);

        assert!(rt_c.has_local_route("local_c"));
        let resolved = rt_c.resolve("grid");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].relay_url, "http://relay-a:8080");
        assert!(rt_c.get_peer("relay-b").is_some());
    }

    #[test]
    fn route_digest_changes_on_update() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        let d1 = rt.route_digest();
        let mut entry = local_entry("grid", "relay-a");
        entry.ipc_address = Some("ipc://changed".into());
        rt.register_route(entry);
        let d2 = rt.route_digest();
        let key = ("grid".to_string(), "relay-a".to_string());
        assert_ne!(d1[&key], d2[&key]);
    }

    #[test]
    fn resolve_missing_name_returns_empty() {
        let rt = RouteTable::new("relay-a".into());
        assert!(rt.resolve("nonexistent").is_empty());
    }
}
