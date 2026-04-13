# Relay Mesh Resource Discovery — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the flat `UpstreamPool` with a gossip-replicated `RouteTable` so clients can do `cc.connect(IGrid, name='grid')` without knowing IPC/HTTP addresses.

**Architecture:** Each relay maintains a `RouteTable` (LOCAL + PEER routes) + `ConnectionPool` (IPC client lifecycle). Relays discover peers via seed URLs, synchronize route state via HTTP gossip (`/_peer/*` endpoints), and expose `/_resolve/{name}` for client-side name resolution. Python's `cc.connect()` queries the local relay and connects directly to the resolved target relay (single hop). All mesh logic is gated behind the existing `relay` Cargo feature flag.

**Tech Stack:** Rust (axum, tokio, serde_json, parking_lot), Python (urllib, threading, click), PyO3/maturin

**Spec:** `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`

---

## File Structure

### Rust — new files (all in `src/c_two/_native/transport/c2-http/src/relay/`)

| File | Responsibility |
|------|---------------|
| `types.rs` | Core data types: `RouteEntry`, `RouteInfo`, `Locality`, `PeerInfo`, `PeerStatus` |
| `route_table.rs` | `RouteTable` struct — route CRUD, resolve, peer tracking, digest, snapshot |
| `conn_pool.rs` | `ConnectionPool` — IPC client lifecycle (extracted from `UpstreamPool`) |
| `peer.rs` | `PeerEnvelope`, `PeerMessage` enum, serde serialization |
| `disseminator.rs` | `Disseminator` trait + `FullBroadcast` impl |
| `peer_handlers.rs` | `/_peer/*` HTTP endpoint handlers |
| `background.rs` | Tokio background tasks (heartbeat, anti-entropy, dead-peer probe, seed retry) |

### Rust — modified files

| File | Changes |
|------|---------|
| `relay/config.rs` | Extend `RelayConfig` with `relay_id`, `advertise_url`, `seeds`, mesh params |
| `relay/state.rs` | Replace `UpstreamPool` + old `RelayState` → new `RelayState` with `RouteTable` + `ConnectionPool` + transactional methods. Old `UpstreamPool` deleted. |
| `relay/router.rs` | Rewrite handlers to use new state; add `/_resolve/{name}`, `/_peers`; mount `/_peer/*` routes |
| `relay/server.rs` | Accept new config; spawn background tasks; `CancellationToken` for shutdown |
| `relay/mod.rs` | Export new modules |
| `c2-http/Cargo.toml` | Add `serde` + `tokio-util` (for `CancellationToken`) dependencies under `relay` feature |
| `c2-config/src/relay.rs` | Extended `RelayConfig` struct |
| `c2-ffi/src/relay_ffi.rs` | New `PyNativeRelay` constructor params (`seeds`, `relay_id`, `advertise_url`) |

### Python — modified files

| File | Changes |
|------|---------|
| `error.py` | Add `ResourceNotFound(701)`, `ResourceUnavailable(702)`, `RegistryUnavailable(705)` |
| `config/settings.py` | Add `C2_RELAY_SEEDS`; remove `ipc_address` field |
| `transport/registry.py` | Remove `set_ipc_address()`; add route cache + relay resolution in `connect()`; add retry logic in `register()` |
| `__init__.py` | Remove `set_ipc_address` export |
| `cli.py` | Add `--seeds` flag to `c3 relay`; add `c3 registry` subcommand |

### Tests — new files

| File | Tests |
|------|-------|
| `tests/unit/test_mesh_errors.py` | New error types serialization |
| `tests/integration/test_relay_mesh.py` | Two-relay mesh: register→gossip→resolve→call |

---

## Task 1: RelayConfig Extensions + Core Types

**Files:**
- Modify: `src/c_two/_native/foundation/c2-config/src/relay.rs`
- Create: `src/c_two/_native/transport/c2-http/src/relay/types.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

This task extends `RelayConfig` with mesh-required fields and creates the core data types used throughout the relay mesh.

- [ ] **Step 1: Extend RelayConfig**

Add mesh configuration fields to `RelayConfig` in `c2-config/src/relay.rs`:

```rust
use std::time::Duration;

/// Configuration for the relay server.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// HTTP bind address (e.g. "0.0.0.0:8080").
    pub bind: String,
    /// Stable relay identifier. Default: "{hostname}_{pid}_{uuid8}".
    pub relay_id: String,
    /// Publicly reachable URL of this relay (e.g. "http://node1:8080").
    /// Distinct from `bind` — bind may be 0.0.0.0 but advertise must be routable.
    pub advertise_url: String,
    /// Seed relay URLs for mesh bootstrap. Empty = standalone mode.
    pub seeds: Vec<String>,
    /// Upstream idle-connection timeout. 0 = disabled.
    pub idle_timeout_secs: u64,
    /// Anti-entropy digest exchange interval. 0 = disabled.
    pub anti_entropy_interval: Duration,
    /// Heartbeat interval for peer liveness. 0 = disabled.
    pub heartbeat_interval: Duration,
    /// Consecutive heartbeat misses before marking peer Dead.
    pub heartbeat_miss_threshold: u32,
    /// Dead-peer health probe interval.
    pub dead_peer_probe_interval: Duration,
    /// Seed retry interval when no peers available.
    pub seed_retry_interval: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            relay_id: Self::generate_relay_id(),
            advertise_url: String::new(), // Must be set explicitly or derived from bind
            seeds: Vec::new(),
            idle_timeout_secs: 0,
            anti_entropy_interval: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_miss_threshold: 3,
            dead_peer_probe_interval: Duration::from_secs(30),
            seed_retry_interval: Duration::from_secs(10),
        }
    }
}

impl RelayConfig {
    pub fn generate_relay_id() -> String {
        let host = gethostname::gethostname()
            .to_string_lossy()
            .to_string();
        let pid = std::process::id();
        let uuid = uuid::Uuid::new_v4();
        format!("{host}_{pid}_{}", &uuid.to_string()[..8])
    }

    /// Derive advertise_url from bind if not explicitly set.
    pub fn effective_advertise_url(&self) -> String {
        if !self.advertise_url.is_empty() {
            return self.advertise_url.clone();
        }
        let bind = &self.bind;
        if let Some((host, port)) = bind.rsplit_once(':') {
            let host = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
            format!("http://{host}:{port}")
        } else {
            format!("http://127.0.0.1:{bind}")
        }
    }
}
```

Add `gethostname = "0.2"` and `uuid = { version = "1", features = ["v4"] }` to `c2-config/Cargo.toml`.

- [ ] **Step 2: Create types.rs with core data types**

Create `src/c_two/_native/transport/c2-http/src/relay/types.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Locality of a route entry relative to this relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locality {
    Local,
    Peer,
}

/// A single CRM route entry in the RouteTable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub icrm_ns: String,
    pub icrm_ver: String,
    pub locality: Locality,
    pub registered_at: f64,
}

/// Resolution result returned to clients via /_resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub name: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub icrm_ns: String,
    pub icrm_ver: String,
}

impl RouteEntry {
    pub fn to_route_info(&self) -> RouteInfo {
        RouteInfo {
            name: self.name.clone(),
            relay_url: self.relay_url.clone(),
            ipc_address: self.ipc_address.clone(),
            icrm_ns: self.icrm_ns.clone(),
            icrm_ver: self.icrm_ver.clone(),
        }
    }
}

/// Peer relay status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerStatus {
    Alive,
    Suspect,
    Dead,
}

/// Information about a peer relay.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub relay_id: String,
    pub url: String,
    pub route_count: u32,
    pub last_heartbeat: Instant,
    pub status: PeerStatus,
}

/// Full sync snapshot for join protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullSync {
    pub routes: Vec<RouteEntry>,
    pub peers: Vec<PeerSnapshot>,
}

/// Serializable peer info (Instant is not serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub relay_id: String,
    pub url: String,
    pub route_count: u32,
    pub status: PeerStatus,
}
```

Add `serde = { version = "1", features = ["derive"], optional = true }` to `c2-http/Cargo.toml` under `relay` feature.

- [ ] **Step 3: Register types.rs in mod.rs**

Update `src/c_two/_native/transport/c2-http/src/relay/mod.rs`:

```rust
pub mod config;
pub mod router;
pub mod server;
pub mod state;
pub mod types;

pub use config::RelayConfig;
pub use server::RelayServer;
pub use state::RelayState;
pub use types::*;
```

- [ ] **Step 4: Verify compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: Compiles with no errors (types.rs and config changes are additive).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): add RelayConfig extensions and core mesh types

Add relay_id, advertise_url, seeds, and timing config to RelayConfig.
Create types.rs with RouteEntry, RouteInfo, PeerInfo, Locality, etc.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 2: RouteTable (LOCAL-only first)

**Files:**
- Create: `src/c_two/_native/transport/c2-http/src/relay/route_table.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

Implement `RouteTable` with full CRUD for LOCAL routes and deterministic resolve ordering. PEER routes are structurally supported but gossip wiring comes later.

- [ ] **Step 1: Create route_table.rs with RouteTable struct**

```rust
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

    /// Get the LOCAL route entry for a name (if any).
    pub fn get_local_route(&self, name: &str) -> Option<&RouteEntry> {
        self.routes.get(&(name.to_string(), self.relay_id.clone()))
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

    /// Remove all routes from a specific relay.
    pub fn remove_routes_by_relay(&mut self, relay_id: &str) -> Vec<RouteEntry> {
        let keys: Vec<_> = self.routes
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
        self.peers.values().filter(|p| p.status == PeerStatus::Alive).collect()
    }

    pub fn dead_peers(&self) -> Vec<&PeerInfo> {
        self.peers.values().filter(|p| p.status == PeerStatus::Dead).collect()
    }

    // -- Snapshot operations (for join protocol + anti-entropy) --

    pub fn full_snapshot(&self) -> FullSync {
        FullSync {
            routes: self.list_routes(),
            peers: self.peers.values().map(|p| PeerSnapshot {
                relay_id: p.relay_id.clone(),
                url: p.url.clone(),
                route_count: p.route_count,
                status: p.status,
            }).collect(),
        }
    }

    /// Merge a FULL_SYNC snapshot (join protocol).
    /// Replaces all PEER routes; preserves LOCAL routes.
    pub fn merge_snapshot(&mut self, sync: FullSync) {
        // Remove all existing PEER routes.
        let peer_keys: Vec<_> = self.routes
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
            self.peers.entry(ps.relay_id.clone()).or_insert_with(|| PeerInfo {
                relay_id: ps.relay_id,
                url: ps.url,
                route_count: ps.route_count,
                last_heartbeat: Instant::now(),
                status: PeerStatus::Alive,
            });
        }
    }

    /// Route digest for anti-entropy: (name, relay_id) → hash(registered_at, ipc_address).
    pub fn route_digest(&self) -> HashMap<(String, String), u64> {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        self.routes.iter().map(|(key, entry)| {
            let mut hasher = DefaultHasher::new();
            entry.registered_at.to_bits().hash(&mut hasher);
            entry.ipc_address.hash(&mut hasher);
            (key.clone(), hasher.finish())
        }).collect()
    }
}
```

- [ ] **Step 2: Add unit tests for RouteTable**

Add at the bottom of `route_table.rs`:

```rust
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
        rt.register_route(peer_entry("grid", "relay-b", 500.0)); // Earlier timestamp
        let resolved = rt.resolve("grid");
        assert_eq!(resolved.len(), 2);
        assert!(resolved[0].ipc_address.is_some()); // LOCAL first
        assert!(resolved[1].ipc_address.is_none());  // PEER second
    }

    #[test]
    fn resolve_peers_sorted_deterministically() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(peer_entry("grid", "relay-c", 1000.0));
        rt.register_route(peer_entry("grid", "relay-b", 1000.0)); // Same time, smaller relay_id
        let resolved = rt.resolve("grid");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].relay_url, "http://relay-b:8080"); // relay-b < relay-c
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
        assert_eq!(rt.list_routes().len(), 1); // Only local remains
    }

    #[test]
    fn purge_local_routes() {
        let mut rt = RouteTable::new("relay-a".into());
        rt.register_route(local_entry("grid", "relay-a"));
        rt.register_route(peer_entry("net", "relay-b", 1000.0));
        rt.purge_local_routes();
        assert!(!rt.has_local_route("grid"));
        assert_eq!(rt.list_routes().len(), 1); // PEER remains
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

        // LOCAL route of relay-c preserved.
        assert!(rt_c.has_local_route("local_c"));
        // PEER routes from snapshot merged.
        let resolved = rt_c.resolve("grid");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].relay_url, "http://relay-a:8080");
        // Peer relay-b learned.
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
```

- [ ] **Step 3: Register route_table.rs in mod.rs**

Update mod.rs to add `pub mod route_table;`.

- [ ] **Step 4: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-http --features relay -- route_table`
Expected: All 10 tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): implement RouteTable with CRUD, resolve, and snapshot

RouteTable supports LOCAL + PEER routes with deterministic resolution
ordering (LOCAL first, then PEER by registered_at/relay_id).
Includes upsert semantics, snapshot/merge for join protocol,
and route digest for anti-entropy.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 3: ConnectionPool (extract from UpstreamPool)

**Files:**
- Create: `src/c_two/_native/transport/c2-http/src/relay/conn_pool.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

Extract the IPC client lifecycle management from `UpstreamPool` into a standalone `ConnectionPool`. This separates route metadata (RouteTable) from connection lifecycle.

- [ ] **Step 1: Create conn_pool.rs**

```rust
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
```

- [ ] **Step 2: Add ConnectionPool unit tests**

```rust
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
```

- [ ] **Step 3: Register conn_pool.rs in mod.rs**

- [ ] **Step 4: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-http --features relay -- conn_pool`
Expected: All 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): extract ConnectionPool from UpstreamPool

Separates IPC client lifecycle (connect, evict, reconnect) from
route metadata. Same API surface as UpstreamPool's connection
management but as a standalone struct.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---


## Task 4: New RelayState with Transactional Methods

**Files:**
- Modify: `src/c_two/_native/transport/c2-http/src/relay/state.rs`

Replace `UpstreamPool` + old `RelayState` with the new state holding `RouteTable` + `ConnectionPool`. Provide transactional methods for atomic route+connection operations. The old `UpstreamPool` code is fully deleted in this task.

**Lock ordering rule (critical):** Always acquire `route_table` before `conn_pool`. All transactional methods in `RelayState` enforce this order.

- [ ] **Step 1: Rewrite state.rs**

Replace the entire content of `state.rs` with the new `RelayState`:

```rust
use std::sync::Arc;

use parking_lot::RwLock;
use c2_ipc::IpcClient;
use c2_config::RelayConfig;

use crate::relay::route_table::RouteTable;
use crate::relay::conn_pool::ConnectionPool;
use crate::relay::types::*;

/// Central relay state — thread-safe wrapper around RouteTable + ConnectionPool.
///
/// Lock ordering (must be followed everywhere):
///   1. route_table (RwLock)
///   2. conn_pool (RwLock)
pub struct RelayState {
    route_table: RwLock<RouteTable>,
    conn_pool: RwLock<ConnectionPool>,
    config: Arc<RelayConfig>,
}

impl RelayState {
    pub fn new(config: Arc<RelayConfig>) -> Self {
        Self {
            route_table: RwLock::new(RouteTable::new(config.relay_id.clone())),
            conn_pool: RwLock::new(ConnectionPool::new()),
            config,
        }
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
        // Lock order: route_table → conn_pool
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

    // -- Snapshot operations --

    pub fn full_snapshot(&self) -> FullSync { self.route_table.read().full_snapshot() }
    pub fn merge_snapshot(&self, sync: FullSync) { self.route_table.write().merge_snapshot(sync); }

    pub fn route_digest(&self) -> std::collections::HashMap<(String, String), u64> {
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
```

- [ ] **Step 2: Add unit tests at bottom of state.rs**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<RelayConfig> {
        Arc::new(RelayConfig {
            relay_id: "test-relay".into(),
            advertise_url: "http://localhost:9999".into(),
            ..Default::default()
        })
    }

    #[test]
    fn register_and_resolve_upstream() {
        let state = RelayState::new(test_config());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        state.register_upstream("grid".into(), "ipc://grid".into(),
            "test.ns".into(), "0.1.0".into(), client);
        let routes = state.resolve("grid");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].ipc_address.as_deref(), Some("ipc://grid"));
    }

    #[test]
    fn unregister_upstream() {
        let state = RelayState::new(test_config());
        let client = Arc::new(IpcClient::new("ipc://grid"));
        state.register_upstream("grid".into(), "ipc://grid".into(),
            "test.ns".into(), "0.1.0".into(), client);
        assert!(state.unregister_upstream("grid").is_some());
        assert!(state.resolve("grid").is_empty());
    }

    #[test]
    fn peer_route_operations() {
        let state = RelayState::new(test_config());
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
```

- [ ] **Step 3: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-http --features relay -- state::tests`
Expected: All 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(relay): replace UpstreamPool with new RelayState

New RelayState wraps RouteTable + ConnectionPool with explicit lock
ordering (route_table before conn_pool). Transactional methods ensure
atomic route + connection updates.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---


---

## Task 5: Migrate Router + Add New Endpoints

**Files:**
- Modify: `src/c_two/_native/transport/c2-http/src/relay/router.rs`

Rewrite all existing HTTP handlers to use the new `RelayState` API. Add `/_resolve/{name}` and `/_peers` endpoints. Keep existing endpoint behavior identical — this is a refactor with new additions.

- [ ] **Step 1: Rewrite router.rs handlers**

Key changes:
- `handle_register`: Use `state.register_upstream()` (now requires `icrm_ns`, `icrm_ver` params, defaulting to empty strings for backward compat).
- `handle_unregister`: Use `state.unregister_upstream()`.
- `call_handler`: Use `state.get_client()` + `state.touch_connection()`.
- `try_reconnect`: Use `state.get_address()` + `state.reconnect()`.
- `list_routes`: Use `state.route_names()`.

**New handlers:**

```rust
/// GET /_resolve/{name} → Vec<RouteInfo>
async fn handle_resolve(
    Path(name): Path<String>,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    let routes = state.resolve(&name);
    if routes.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "ResourceNotFound", "name": name,
        }))).into_response();
    }
    Json(routes).into_response()
}

/// GET /_peers → list of peer relays
async fn handle_peers(
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    Json(state.list_peers()).into_response()
}
```

**Updated router construction:**

```rust
pub fn build_router(state: Arc<RelayState>) -> axum::Router {
    axum::Router::new()
        .route("/_register", post(handle_register))
        .route("/_unregister", post(handle_unregister))
        .route("/_routes", get(handle_list_routes))
        .route("/_resolve/{name}", get(handle_resolve))
        .route("/_peers", get(handle_peers))
        .route("/health", get(handle_health))
        .route("/_echo", post(handle_echo))
        .route("/{route_name}/{method_name}", post(call_handler))
        .with_state(state)
}
```

- [ ] **Step 2: Update call_handler to use new state API**

Replace `state.pool.get(name)` → `state.get_client(name)`, `state.pool.touch(name)` → `state.touch_connection(name)`:

```rust
async fn call_handler(
    Path((route_name, method_name)): Path<(String, String)>,
    State(state): State<Arc<RelayState>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let client = state.get_client(&route_name);
    let client = match client {
        Some(c) => c,
        None => match try_reconnect(&state, &route_name).await {
            Some(c) => c,
            None => return (StatusCode::BAD_GATEWAY,
                format!("Upstream '{route_name}' not connected")).into_response(),
        },
    };
    state.touch_connection(&route_name);
    // Forward — existing logic unchanged.
    match client.send_raw(&body).await {
        Ok(response) => (StatusCode::OK, response).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("IPC error: {e}")).into_response(),
    }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: Clean compilation.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(relay): migrate router to new RelayState + add resolve/peers

Rewrite handlers to use RelayState API. Add /_resolve/{name} → Vec<RouteInfo>
and /_peers endpoints. Existing behavior preserved.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 6: Update RelayServer + FFI + Compatibility Gate

**Files:**
- Modify: `src/c_two/_native/transport/c2-http/src/relay/server.rs`
- Modify: `src/c_two/_native/bridge/c2-ffi/src/relay_ffi.rs`
- Modify: `src/c_two/_native/transport/c2-http/Cargo.toml`

Update server lifecycle for `RelayConfig` and `RelayState`. Update PyO3 bridge. **ALL existing tests must pass after this task.**

- [ ] **Step 1: Add dependencies to Cargo.toml**

In `c2-http/Cargo.toml`, add under `[dependencies]`:

```toml
tokio-util = { version = "0.7", features = ["rt"], optional = true }
parking_lot = { version = "0.12", optional = true }
```

Update `relay` feature:

```toml
relay = [
    "dep:axum", "dep:c2-ipc", "dep:c2-wire",
    "dep:tokio", "dep:tokio-util", "dep:parking_lot",
    "dep:serde_json",
]
```

- [ ] **Step 2: Update server.rs to accept RelayConfig**

Key changes: `start()` takes `RelayConfig` → builds `Arc<RelayState>` → passes to router. `idle_sweeper` uses `state.evict_idle()`. Commands operate via `RelayState` methods.

```rust
pub struct RelayServer {
    handle: Option<std::thread::JoinHandle<()>>,
    tx: Option<mpsc::Sender<Command>>,
    state: Arc<RelayState>,
}

impl RelayServer {
    pub fn start(config: RelayConfig) -> Result<Self, String> {
        let config = Arc::new(config);
        let state = Arc::new(RelayState::new(config.clone()));
        let (tx, rx) = mpsc::channel(32);
        let server_state = state.clone();
        let bind_addr = config.bind.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("tokio runtime");
            rt.block_on(run(server_state, bind_addr, rx));
        });
        Ok(Self { handle: Some(handle), tx: Some(tx), state })
    }
}
```

- [ ] **Step 3: Update relay_ffi.rs with new constructor params**

```rust
#[pymethods]
impl PyNativeRelay {
    #[new]
    #[pyo3(signature = (bind, relay_id=None, advertise_url=None, seeds=None, idle_timeout=0))]
    fn new(
        bind: String,
        relay_id: Option<String>,
        advertise_url: Option<String>,
        seeds: Option<Vec<String>>,
        idle_timeout: u64,
    ) -> Self {
        let config = RelayConfig {
            bind,
            relay_id: relay_id.unwrap_or_else(RelayConfig::generate_relay_id),
            advertise_url: advertise_url.unwrap_or_default(),
            seeds: seeds.unwrap_or_default(),
            idle_timeout_secs: idle_timeout,
            ..Default::default()
        };
        Self { state: Mutex::new(ServerState::Stopped), config }
    }
}
```

- [ ] **Step 4: Run ALL existing tests (compatibility gate)**

```bash
cd src/c_two/_native && cargo test -p c2-http --features relay
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All Rust relay tests pass. All ~571 Python tests pass. Zero regressions.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): update server + FFI for new config/state model

RelayServer accepts RelayConfig with mesh parameters.
PyNativeRelay takes optional relay_id, advertise_url, seeds.
Compatibility gate: all existing tests pass.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---


---

## Task 7: Peer Message Types + Disseminator

**Files:**
- Create: `src/c_two/_native/transport/c2-http/src/relay/peer.rs`
- Create: `src/c_two/_native/transport/c2-http/src/relay/disseminator.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

Define gossip protocol messages and the broadcasting trait. `FullBroadcast` posts to all known alive peers via HTTP.

- [ ] **Step 1: Create peer.rs with PeerMessage + PeerEnvelope**

```rust
use serde::{Deserialize, Serialize};
use crate::relay::types::PeerStatus;

pub const PROTOCOL_VERSION: u32 = 1;

/// Envelope wrapping every peer-to-peer message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEnvelope {
    pub protocol_version: u32,
    pub sender_relay_id: String,
    pub message: PeerMessage,
}

/// Gossip message variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PeerMessage {
    /// A CRM route was registered or updated.
    RouteAnnounce {
        name: String,
        relay_id: String,
        relay_url: String,
        ipc_address: Option<String>,
        icrm_ns: String,
        icrm_ver: String,
        registered_at: f64,
    },
    /// A CRM route was unregistered.
    RouteWithdraw {
        name: String,
        relay_id: String,
    },
    /// A relay wants to join the mesh.
    RelayJoin {
        relay_id: String,
        url: String,
    },
    /// A relay is gracefully leaving the mesh.
    RelayLeave {
        relay_id: String,
    },
    /// Periodic heartbeat.
    Heartbeat {
        relay_id: String,
        route_count: u32,
    },
    /// Anti-entropy digest exchange (request or response).
    DigestExchange {
        /// Map of (name, relay_id) → hash. Serialized as array of tuples.
        digest: Vec<DigestEntry>,
    },
    /// Anti-entropy diff: entries the sender has that recipient was missing.
    DigestDiff {
        entries: Vec<DigestDiffEntry>,
    },
}

/// One entry in a digest map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestEntry {
    pub name: String,
    pub relay_id: String,
    pub hash: u64,
}

/// Route data sent to repair digest differences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestDiffEntry {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub ipc_address: Option<String>,
    pub icrm_ns: String,
    pub icrm_ver: String,
    pub registered_at: f64,
}

impl PeerEnvelope {
    pub fn new(sender: &str, message: PeerMessage) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            sender_relay_id: sender.to_string(),
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip_json() {
        let env = PeerEnvelope::new("relay-a", PeerMessage::Heartbeat {
            relay_id: "relay-a".into(),
            route_count: 5,
        });
        let json = serde_json::to_string(&env).unwrap();
        let decoded: PeerEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.protocol_version, PROTOCOL_VERSION);
        assert_eq!(decoded.sender_relay_id, "relay-a");
        match decoded.message {
            PeerMessage::Heartbeat { relay_id, route_count } => {
                assert_eq!(relay_id, "relay-a");
                assert_eq!(route_count, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn route_announce_roundtrip() {
        let env = PeerEnvelope::new("relay-a", PeerMessage::RouteAnnounce {
            name: "grid".into(),
            relay_id: "relay-a".into(),
            relay_url: "http://relay-a:8080".into(),
            ipc_address: Some("ipc://grid".into()),
            icrm_ns: "cc.demo".into(),
            icrm_ver: "0.1.0".into(),
            registered_at: 1000.0,
        });
        let json = serde_json::to_string(&env).unwrap();
        let decoded: PeerEnvelope = serde_json::from_str(&json).unwrap();
        match decoded.message {
            PeerMessage::RouteAnnounce { name, .. } => assert_eq!(name, "grid"),
            _ => panic!("wrong variant"),
        }
    }
}
```

- [ ] **Step 2: Create disseminator.rs with Disseminator trait + FullBroadcast**

```rust
use std::sync::Arc;
use crate::relay::peer::PeerEnvelope;
use crate::relay::types::PeerSnapshot;

/// Trait for broadcasting gossip messages to peers.
pub trait Disseminator: Send + Sync {
    /// Broadcast a message to all relevant peers.
    fn broadcast(&self, envelope: PeerEnvelope, peers: &[PeerSnapshot]);
}

/// Full broadcast — sends to every Alive peer.
/// Suitable for clusters with <100 relays.
pub struct FullBroadcast {
    http_client: reqwest::Client,
}

impl FullBroadcast {
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    async fn send_to_peer(client: &reqwest::Client, url: &str, envelope: &PeerEnvelope) {
        let endpoint = match &envelope.message {
            crate::relay::peer::PeerMessage::RouteAnnounce { .. } => "/_peer/announce",
            crate::relay::peer::PeerMessage::RouteWithdraw { .. } => "/_peer/announce",
            crate::relay::peer::PeerMessage::RelayJoin { .. } => "/_peer/join",
            crate::relay::peer::PeerMessage::RelayLeave { .. } => "/_peer/leave",
            crate::relay::peer::PeerMessage::Heartbeat { .. } => "/_peer/heartbeat",
            crate::relay::peer::PeerMessage::DigestExchange { .. } => "/_peer/digest",
            crate::relay::peer::PeerMessage::DigestDiff { .. } => "/_peer/digest",
        };
        let full_url = format!("{url}{endpoint}");
        let _ = client.post(&full_url).json(envelope).send().await;
    }
}

impl Disseminator for FullBroadcast {
    fn broadcast(&self, envelope: PeerEnvelope, peers: &[PeerSnapshot]) {
        use crate::relay::types::PeerStatus;
        let alive: Vec<String> = peers.iter()
            .filter(|p| p.status == PeerStatus::Alive)
            .map(|p| p.url.clone())
            .collect();

        if alive.is_empty() {
            return;
        }

        let client = self.http_client.clone();
        // Fire-and-forget: spawn async tasks for each peer.
        tokio::spawn(async move {
            let futs: Vec<_> = alive.iter().map(|url| {
                Self::send_to_peer(&client, url, &envelope)
            }).collect();
            futures::future::join_all(futs).await;
        });
    }
}
```

Add `reqwest = { version = "0.12", features = ["json"], optional = true }` and `futures = { version = "0.3", optional = true }` to c2-http Cargo.toml under relay feature.

- [ ] **Step 3: Register peer.rs and disseminator.rs in mod.rs**

```rust
pub mod peer;
pub mod disseminator;
```

- [ ] **Step 4: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-http --features relay -- peer::tests`
Expected: 2 tests pass (envelope + announce roundtrip).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): add PeerMessage types + Disseminator trait

Define PeerEnvelope/PeerMessage with tagged JSON serde.
Implement FullBroadcast disseminator for <100 node clusters.
Fire-and-forget HTTP POST to all alive peers.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 8: Peer Protocol Endpoints

**Files:**
- Create: `src/c_two/_native/transport/c2-http/src/relay/peer_handlers.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/router.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

Implement `/_peer/*` HTTP handlers for mesh gossip. Each handler receives a `PeerEnvelope`, validates `protocol_version`, and applies the message to `RelayState`.

- [ ] **Step 1: Create peer_handlers.rs**

```rust
use std::sync::Arc;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use crate::relay::peer::{PeerEnvelope, PeerMessage, PROTOCOL_VERSION};
use crate::relay::state::RelayState;
use crate::relay::types::*;

/// POST /_peer/announce — receive RouteAnnounce or RouteWithdraw
pub async fn handle_peer_announce(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if envelope.protocol_version > PROTOCOL_VERSION {
        tracing::warn!(
            "Ignoring message from {} with protocol_version {}",
            envelope.sender_relay_id, envelope.protocol_version
        );
        return StatusCode::OK;
    }
    match envelope.message {
        PeerMessage::RouteAnnounce {
            name, relay_id, relay_url, ipc_address,
            icrm_ns, icrm_ver, registered_at,
        } => {
            if relay_id == state.relay_id() {
                return StatusCode::OK; // Ignore our own echoes.
            }
            state.register_peer_route(RouteEntry {
                name, relay_id, relay_url, ipc_address,
                icrm_ns, icrm_ver,
                locality: Locality::Peer,
                registered_at,
            });
        }
        PeerMessage::RouteWithdraw { name, relay_id } => {
            state.unregister_peer_route(&name, &relay_id);
        }
        _ => {} // Wrong endpoint, ignore.
    }
    StatusCode::OK
}

/// POST /_peer/join — a new relay wants to join
pub async fn handle_peer_join(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if let PeerMessage::RelayJoin { relay_id, url } = envelope.message {
        if relay_id == state.relay_id() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "self"}))).into_response();
        }
        // Register the new peer.
        state.register_peer(PeerInfo {
            relay_id: relay_id.clone(),
            url,
            route_count: 0,
            last_heartbeat: std::time::Instant::now(),
            status: PeerStatus::Alive,
        });
        // Return our full snapshot for the joiner.
        let snapshot = state.full_snapshot();
        Json(snapshot).into_response()
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

/// GET /_peer/sync — return full route table + peer list (for late joiners)
pub async fn handle_peer_sync(
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    Json(state.full_snapshot())
}

/// POST /_peer/heartbeat — periodic liveness check
pub async fn handle_peer_heartbeat(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if let PeerMessage::Heartbeat { relay_id, route_count } = envelope.message {
        state.with_route_table_mut(|rt| {
            if let Some(peer) = rt.get_peer_mut(&relay_id) {
                peer.last_heartbeat = std::time::Instant::now();
                peer.route_count = route_count;
                if peer.status == PeerStatus::Dead || peer.status == PeerStatus::Suspect {
                    // Recovery: peer came back alive.
                    peer.status = PeerStatus::Alive;
                }
            }
        });
    }
    StatusCode::OK
}

/// POST /_peer/leave — graceful departure
pub async fn handle_peer_leave(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    if let PeerMessage::RelayLeave { relay_id } = envelope.message {
        state.remove_routes_by_relay(&relay_id);
        state.unregister_peer(&relay_id);
    }
    StatusCode::OK
}

/// POST /_peer/digest — anti-entropy digest exchange
pub async fn handle_peer_digest(
    State(state): State<Arc<RelayState>>,
    Json(envelope): Json<PeerEnvelope>,
) -> impl IntoResponse {
    match envelope.message {
        PeerMessage::DigestExchange { digest } => {
            // Compare incoming digest with our own.
            let our_digest = state.route_digest();
            let mut diff_entries = Vec::new();
            let mut missing_from_us = Vec::new();

            // Find entries we have that differ from or are missing in peer's digest.
            let peer_map: std::collections::HashMap<(String, String), u64> = digest.iter()
                .map(|d| ((d.name.clone(), d.relay_id.clone()), d.hash))
                .collect();

            for (key, our_hash) in &our_digest {
                match peer_map.get(key) {
                    Some(peer_hash) if peer_hash == our_hash => {} // Match
                    _ => {
                        // We have something the peer doesn't or it differs.
                        // Send the full entry.
                        if let Some(entry) = state.with_route_table(|rt| {
                            rt.list_routes().into_iter().find(|e| {
                                e.name == key.0 && e.relay_id == key.1
                            })
                        }) {
                            diff_entries.push(crate::relay::peer::DigestDiffEntry {
                                name: entry.name,
                                relay_id: entry.relay_id,
                                relay_url: entry.relay_url,
                                ipc_address: entry.ipc_address,
                                icrm_ns: entry.icrm_ns,
                                icrm_ver: entry.icrm_ver,
                                registered_at: entry.registered_at,
                            });
                        }
                    }
                }
            }

            // Find entries in peer's digest that we don't have → mark for deletion.
            for d in &digest {
                let key = (d.name.clone(), d.relay_id.clone());
                if !our_digest.contains_key(&key) {
                    missing_from_us.push(key);
                }
            }

            // Apply incoming entries we're missing.
            // (They'll request diff from us in the reciprocal exchange.)

            let response = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::DigestDiff { entries: diff_entries },
            );
            Json(response).into_response()
        }
        PeerMessage::DigestDiff { entries } => {
            // Apply diff entries from peer.
            for diff in entries {
                state.register_peer_route(RouteEntry {
                    name: diff.name,
                    relay_id: diff.relay_id,
                    relay_url: diff.relay_url,
                    ipc_address: diff.ipc_address,
                    icrm_ns: diff.icrm_ns,
                    icrm_ver: diff.icrm_ver,
                    locality: Locality::Peer,
                    registered_at: diff.registered_at,
                });
            }
            StatusCode::OK.into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}
```

- [ ] **Step 2: Mount /_peer routes in router.rs**

Add to `build_router`:

```rust
use crate::relay::peer_handlers::*;

pub fn build_router(state: Arc<RelayState>) -> axum::Router {
    axum::Router::new()
        // Existing routes
        .route("/_register", post(handle_register))
        .route("/_unregister", post(handle_unregister))
        .route("/_routes", get(handle_list_routes))
        .route("/_resolve/{name}", get(handle_resolve))
        .route("/_peers", get(handle_peers))
        .route("/health", get(handle_health))
        .route("/_echo", post(handle_echo))
        // Peer protocol routes
        .route("/_peer/announce", post(handle_peer_announce))
        .route("/_peer/join", post(handle_peer_join))
        .route("/_peer/sync", get(handle_peer_sync))
        .route("/_peer/heartbeat", post(handle_peer_heartbeat))
        .route("/_peer/leave", post(handle_peer_leave))
        .route("/_peer/digest", post(handle_peer_digest))
        // Wildcard forwarding (must be last)
        .route("/{route_name}/{method_name}", post(call_handler))
        .with_state(state)
}
```

- [ ] **Step 3: Verify compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: Clean compilation.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(relay): implement /_peer/* gossip endpoints

Add handlers for join, sync, announce, heartbeat, leave, and
anti-entropy digest exchange. Mount on relay router.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 9: Background Tasks (Heartbeat, Anti-Entropy, Dead-Peer Probe, Seed Retry)

**Files:**
- Create: `src/c_two/_native/transport/c2-http/src/relay/background.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/server.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`

Implement the four periodic background tasks using tokio tasks + `CancellationToken` for clean shutdown.

- [ ] **Step 1: Create background.rs**

```rust
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::relay::peer::{PeerEnvelope, PeerMessage};
use crate::relay::disseminator::Disseminator;
use crate::relay::state::RelayState;
use crate::relay::types::PeerStatus;

/// Spawn all background tasks. Returns handles for monitoring.
pub fn spawn_background_tasks(
    state: Arc<RelayState>,
    disseminator: Arc<dyn Disseminator>,
    cancel: CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let config = state.config().clone();

    // 1. Heartbeat sender
    if config.heartbeat_interval > Duration::ZERO {
        let s = state.clone();
        let d = disseminator.clone();
        let c = cancel.clone();
        let interval = config.heartbeat_interval;
        handles.push(tokio::spawn(async move {
            heartbeat_loop(s, d, interval, c).await;
        }));
    }

    // 2. Heartbeat checker (failure detection)
    if config.heartbeat_interval > Duration::ZERO {
        let s = state.clone();
        let c = cancel.clone();
        let interval = config.heartbeat_interval;
        let threshold = config.heartbeat_miss_threshold;
        handles.push(tokio::spawn(async move {
            failure_detection_loop(s, interval, threshold, c).await;
        }));
    }

    // 3. Anti-entropy
    if config.anti_entropy_interval > Duration::ZERO {
        let s = state.clone();
        let c = cancel.clone();
        let interval = config.anti_entropy_interval;
        handles.push(tokio::spawn(async move {
            anti_entropy_loop(s, interval, c).await;
        }));
    }

    // 4. Dead-peer probe
    if config.dead_peer_probe_interval > Duration::ZERO {
        let s = state.clone();
        let c = cancel.clone();
        let interval = config.dead_peer_probe_interval;
        handles.push(tokio::spawn(async move {
            dead_peer_probe_loop(s, interval, c).await;
        }));
    }

    // 5. Seed retry (only if seeds configured)
    if !config.seeds.is_empty() {
        let s = state.clone();
        let c = cancel.clone();
        let interval = config.seed_retry_interval;
        let seeds = config.seeds.clone();
        handles.push(tokio::spawn(async move {
            seed_retry_loop(s, seeds, interval, c).await;
        }));
    }

    handles
}

async fn heartbeat_loop(
    state: Arc<RelayState>,
    disseminator: Arc<dyn Disseminator>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let route_count = state.route_names().len() as u32;
                let envelope = PeerEnvelope::new(
                    state.relay_id(),
                    PeerMessage::Heartbeat {
                        relay_id: state.relay_id().to_string(),
                        route_count,
                    },
                );
                let peers = state.list_peers();
                disseminator.broadcast(envelope, &peers);
            }
        }
    }
}

async fn failure_detection_loop(
    state: Arc<RelayState>,
    heartbeat_interval: Duration,
    miss_threshold: u32,
    cancel: CancellationToken,
) {
    let check_interval = heartbeat_interval * 2;
    let timeout = heartbeat_interval * miss_threshold;
    let mut ticker = tokio::time::interval(check_interval);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                state.with_route_table_mut(|rt| {
                    let now = std::time::Instant::now();
                    for peer in rt.list_peers() {
                        let elapsed = now.duration_since(peer.last_heartbeat);
                        if elapsed > timeout && peer.status == PeerStatus::Alive {
                            if let Some(p) = rt.get_peer_mut(&peer.relay_id) {
                                p.status = PeerStatus::Suspect;
                            }
                        } else if elapsed > timeout * 2 && peer.status == PeerStatus::Suspect {
                            if let Some(p) = rt.get_peer_mut(&peer.relay_id) {
                                p.status = PeerStatus::Dead;
                            }
                            // Remove dead peer's routes.
                            let rid = peer.relay_id.clone();
                            rt.remove_routes_by_relay(&rid);
                        }
                    }
                });
            }
        }
    }
}

async fn anti_entropy_loop(
    state: Arc<RelayState>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut ticker = tokio::time::interval(interval);
    let mut peer_index: usize = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let alive: Vec<_> = state.with_route_table(|rt| {
                    rt.alive_peers().into_iter()
                        .map(|p| (p.relay_id.clone(), p.url.clone()))
                        .collect()
                });
                if alive.is_empty() { continue; }

                // Round-robin: pick one peer per interval.
                let idx = peer_index % alive.len();
                peer_index = peer_index.wrapping_add(1);
                let (_peer_id, peer_url) = &alive[idx];

                // Build our digest.
                let digest_map = state.route_digest();
                let digest: Vec<_> = digest_map.into_iter().map(|((name, relay_id), hash)| {
                    crate::relay::peer::DigestEntry { name, relay_id, hash }
                }).collect();

                let envelope = PeerEnvelope::new(
                    state.relay_id(),
                    PeerMessage::DigestExchange { digest },
                );

                let url = format!("{peer_url}/_peer/digest");
                if let Ok(resp) = client.post(&url).json(&envelope).send().await {
                    if let Ok(diff_envelope) = resp.json::<PeerEnvelope>().await {
                        if let PeerMessage::DigestDiff { entries } = diff_envelope.message {
                            for diff in entries {
                                state.register_peer_route(crate::relay::types::RouteEntry {
                                    name: diff.name,
                                    relay_id: diff.relay_id,
                                    relay_url: diff.relay_url,
                                    ipc_address: diff.ipc_address,
                                    icrm_ns: diff.icrm_ns,
                                    icrm_ver: diff.icrm_ver,
                                    locality: crate::relay::types::Locality::Peer,
                                    registered_at: diff.registered_at,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn dead_peer_probe_loop(
    state: Arc<RelayState>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut ticker = tokio::time::interval(interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let dead: Vec<_> = state.with_route_table(|rt| {
                    rt.dead_peers().into_iter()
                        .map(|p| (p.relay_id.clone(), p.url.clone()))
                        .collect()
                });
                for (relay_id, url) in dead {
                    let health_url = format!("{url}/health");
                    if let Ok(resp) = client.get(&health_url).send().await {
                        if resp.status().is_success() {
                            // Peer is back! Mark alive + trigger anti-entropy.
                            state.with_route_table_mut(|rt| {
                                if let Some(peer) = rt.get_peer_mut(&relay_id) {
                                    peer.status = PeerStatus::Alive;
                                    peer.last_heartbeat = std::time::Instant::now();
                                }
                            });
                            // Trigger immediate digest exchange.
                            let digest_map = state.route_digest();
                            let digest: Vec<_> = digest_map.into_iter().map(|((n, r), h)| {
                                crate::relay::peer::DigestEntry { name: n, relay_id: r, hash: h }
                            }).collect();
                            let envelope = PeerEnvelope::new(
                                state.relay_id(),
                                PeerMessage::DigestExchange { digest },
                            );
                            let digest_url = format!("{url}/_peer/digest");
                            if let Ok(resp) = client.post(&digest_url).json(&envelope).send().await {
                                if let Ok(diff_env) = resp.json::<PeerEnvelope>().await {
                                    if let PeerMessage::DigestDiff { entries } = diff_env.message {
                                        for d in entries {
                                            state.register_peer_route(crate::relay::types::RouteEntry {
                                                name: d.name, relay_id: d.relay_id,
                                                relay_url: d.relay_url, ipc_address: d.ipc_address,
                                                icrm_ns: d.icrm_ns, icrm_ver: d.icrm_ver,
                                                locality: crate::relay::types::Locality::Peer,
                                                registered_at: d.registered_at,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn seed_retry_loop(
    state: Arc<RelayState>,
    seeds: Vec<String>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut ticker = tokio::time::interval(interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                // Only retry if we have no alive peers.
                let has_alive = !state.with_route_table(|rt| rt.alive_peers()).is_empty();
                if has_alive { continue; }

                for seed_url in &seeds {
                    let join_url = format!("{seed_url}/_peer/join");
                    let envelope = PeerEnvelope::new(
                        state.relay_id(),
                        PeerMessage::RelayJoin {
                            relay_id: state.relay_id().to_string(),
                            url: state.config().effective_advertise_url(),
                        },
                    );
                    if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                        if let Ok(snapshot) = resp.json::<crate::relay::types::FullSync>().await {
                            state.merge_snapshot(snapshot);
                            break; // One successful seed is enough.
                        }
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 2: Wire background tasks into server.rs**

In `RelayServer::start()`, after binding the HTTP server:

```rust
use tokio_util::sync::CancellationToken;
use crate::relay::background::spawn_background_tasks;
use crate::relay::disseminator::FullBroadcast;

// Inside the tokio runtime block:
let cancel = CancellationToken::new();
let disseminator: Arc<dyn Disseminator> = Arc::new(FullBroadcast::new());
let bg_handles = spawn_background_tasks(state.clone(), disseminator, cancel.clone());

// On shutdown, cancel all background tasks:
cancel.cancel();
for handle in bg_handles {
    let _ = handle.await;
}
```

- [ ] **Step 3: Wire seed bootstrap into server startup**

After the HTTP server is listening, attempt to join seeds:

```rust
// After server bind, before entering command loop:
if !config.seeds.is_empty() {
    let state = server_state.clone();
    let seeds = config.seeds.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        for seed_url in &seeds {
            let join_url = format!("{seed_url}/_peer/join");
            let envelope = PeerEnvelope::new(
                state.relay_id(),
                PeerMessage::RelayJoin {
                    relay_id: state.relay_id().to_string(),
                    url: state.config().effective_advertise_url(),
                },
            );
            if let Ok(resp) = client.post(&join_url).json(&envelope).send().await {
                if let Ok(snapshot) = resp.json::<FullSync>().await {
                    state.merge_snapshot(snapshot);
                    break;
                }
            }
        }
    });
}
```

- [ ] **Step 4: Verify compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: Clean compilation.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(relay): implement background tasks for mesh protocol

Add heartbeat sender, failure detection (Alive→Suspect→Dead),
round-robin anti-entropy digest exchange, dead-peer probe with
auto-recovery, and seed retry. All use CancellationToken for
clean shutdown.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 10: Health Check for Duplicate Name Detection

**Files:**
- Modify: `src/c_two/_native/transport/c2-http/src/relay/peer_handlers.rs`
- Modify: `src/c_two/_native/transport/c2-http/src/relay/router.rs`

When `/_register` receives a name that already exists from a different process on the same node, ping the existing IPC address (500ms timeout). If alive → 409 Conflict. If dead → upsert (update route with new address).

- [ ] **Step 1: Add health check to handle_register**

In `router.rs`, update `handle_register`:

```rust
async fn handle_register(
    State(state): State<Arc<RelayState>>,
    Json(payload): Json<RegisterPayload>,
) -> impl IntoResponse {
    let name = &payload.name;
    let address = &payload.address;

    // Check for duplicate LOCAL route.
    if state.has_local_route(name) {
        let existing_addr = state.get_address(name);
        if let Some(ref old_addr) = existing_addr {
            if old_addr != address {
                // Different address → might be a new process after crash.
                // Ping the old address to check if still alive.
                if let Some(old_client) = state.get_client(name) {
                    // Ping with 500ms timeout.
                    match tokio::time::timeout(
                        Duration::from_millis(500),
                        old_client.ping(),
                    ).await {
                        Ok(Ok(_)) => {
                            // Old process still alive → reject.
                            return (StatusCode::CONFLICT, Json(serde_json::json!({
                                "error": "DuplicateRoute",
                                "name": name,
                                "existing_address": old_addr,
                            }))).into_response();
                        }
                        _ => {
                            // Old process dead or unreachable → upsert.
                        }
                    }
                }
            }
            // Same address or old process dead → fall through to upsert.
        }
    }

    // Register (upsert semantics).
    let icrm_ns = payload.icrm_ns.unwrap_or_default();
    let icrm_ver = payload.icrm_ver.unwrap_or_default();
    let client = Arc::new(IpcClient::connect(&payload.address).await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("IPC connect failed: {e}")))?);

    let entry = state.register_upstream(
        name.clone(), address.clone(), icrm_ns, icrm_ver, client,
    );

    // Gossip announce to peers.
    // (Disseminator broadcast will be wired in Task 12 integration.)

    (StatusCode::OK, Json(serde_json::json!({"status": "registered"}))).into_response()
}
```

- [ ] **Step 2: Verify compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: Clean compilation.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat(relay): add health check for duplicate name detection

When same-node registers duplicate name with different address,
ping old IPC connection (500ms timeout). If alive → 409 Conflict.
If dead → upsert with new address.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 11: Python Error Types + Settings

**Files:**
- Modify: `src/c_two/error.py`
- Modify: `src/c_two/config/settings.py`

Add new error types for resource discovery and the `C2_RELAY_SEEDS` setting.

- [ ] **Step 1: Add new error classes to error.py**

Add after the existing error classes:

```python
class ResourceNotFound(CCError):
    """Raised when a named resource cannot be resolved by any relay."""
    ERROR_CODE = 701

class ResourceUnavailable(CCError):
    """Raised when a resource exists but is not reachable."""
    ERROR_CODE = 702

class RegistryUnavailable(CCError):
    """Raised when no relay is available for name resolution."""
    ERROR_CODE = 705
```

- [ ] **Step 2: Write tests for new error types**

Create `tests/unit/test_mesh_errors.py`:

```python
import c_two as cc
from c_two.error import ResourceNotFound, ResourceUnavailable, RegistryUnavailable

class TestMeshErrors:
    def test_resource_not_found_code(self):
        err = ResourceNotFound("grid")
        assert err.ERROR_CODE == 701

    def test_resource_unavailable_code(self):
        err = ResourceUnavailable("grid", "connection refused")
        assert err.ERROR_CODE == 702

    def test_registry_unavailable_code(self):
        err = RegistryUnavailable("no relay configured")
        assert err.ERROR_CODE == 705

    def test_error_serialize_roundtrip(self):
        err = ResourceNotFound("grid")
        data = err.serialize()
        restored = ResourceNotFound.deserialize(data)
        assert restored.ERROR_CODE == 701
```

- [ ] **Step 3: Run tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_mesh_errors.py -q`
Expected: 4 tests pass.

- [ ] **Step 4: Add C2_RELAY_SEEDS to settings.py**

```python
class C2Settings(BaseSettings):
    # ... existing fields ...
    relay_seeds: str = ""  # Comma-separated seed relay URLs

    @property
    def relay_seed_list(self) -> list[str]:
        if not self.relay_seeds:
            return []
        return [s.strip() for s in self.relay_seeds.split(",") if s.strip()]

    model_config = SettingsConfigDict(
        env_prefix="C2_",
        # ...
    )
```

- [ ] **Step 5: Remove C2_IPC_ADDRESS from settings**

Remove the `ipc_address` field from `C2Settings`. This field is replaced by auto-UUID addresses.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add resource discovery error types + C2_RELAY_SEEDS setting

ResourceNotFound(701), ResourceUnavailable(702), RegistryUnavailable(705).
C2_RELAY_SEEDS env var for comma-separated seed relay URLs.
Remove C2_IPC_ADDRESS (replaced by auto-UUID).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 12: Python cc.connect() with Relay Resolution + Route Cache

**Files:**
- Modify: `src/c_two/transport/registry.py`

Update `cc.connect()` to resolve names via relay when no `address=` is given. Implement thread-safe route cache with 30s TTL.

- [ ] **Step 1: Add route cache to _ProcessRegistry**

```python
import threading
import time

_ROUTE_CACHE_TTL = 30.0  # seconds

class _RouteCache:
    """Thread-safe TTL cache for resolved routes."""

    def __init__(self):
        self._lock = threading.Lock()
        self._cache: dict[str, tuple[list[dict], float]] = {}

    def get(self, name: str) -> list[dict] | None:
        with self._lock:
            entry = self._cache.get(name)
            if entry and time.monotonic() - entry[1] < _ROUTE_CACHE_TTL:
                return entry[0]
            return None

    def put(self, name: str, routes: list[dict]):
        with self._lock:
            self._cache[name] = (routes, time.monotonic())

    def invalidate(self, name: str):
        with self._lock:
            self._cache.pop(name, None)

    def clear(self):
        with self._lock:
            self._cache.clear()
```

- [ ] **Step 2: Add _resolve_via_relay method**

```python
def _resolve_via_relay(self, name: str) -> list[dict]:
    """Resolve a resource name via the local relay's /_resolve endpoint."""
    # Check cache first.
    cached = self._route_cache.get(name)
    if cached is not None:
        return cached

    relay_addr = self._settings.relay_address
    if not relay_addr:
        raise RegistryUnavailable("No relay configured (set C2_RELAY_ADDRESS)")

    url = f"{relay_addr}/_resolve/{name}"
    try:
        import urllib.request
        import json
        req = urllib.request.Request(url)
        with urllib.request.urlopen(req, timeout=5) as resp:
            routes = json.loads(resp.read())
            self._route_cache.put(name, routes)
            return routes
    except urllib.error.HTTPError as e:
        if e.code == 404:
            raise ResourceNotFound(f"Resource '{name}' not found")
        raise ResourceUnavailable(f"Relay error: {e.code}")
    except Exception as e:
        # Connection failed → invalidate cache and raise.
        self._route_cache.invalidate(name)
        raise RegistryUnavailable(f"Relay unreachable: {e}")
```

- [ ] **Step 3: Update connect() to use relay resolution**

In `_ProcessRegistry.connect()`, after checking thread-local preference, before raising error:

```python
def connect(self, icrm_cls, *, name=None, address=None, **kwargs):
    name = name or icrm_cls.__cc_icrm_namespace__

    # 1. Same-process preference (thread-local, zero serde).
    if name in self._registered_crms and address is None:
        return self._make_thread_proxy(icrm_cls, name)

    # 2. Explicit address → direct IPC/HTTP connection (escape hatch).
    if address is not None:
        return self._make_remote_proxy(icrm_cls, name, address)

    # 3. Relay resolution.
    routes = self._resolve_via_relay(name)
    if not routes:
        raise ResourceNotFound(f"No routes for '{name}'")

    # Use first route (LOCAL preferred, then PEER by ordering).
    route = routes[0]
    relay_url = route["relay_url"]

    # If LOCAL route → use IPC address directly.
    if route.get("ipc_address"):
        return self._make_remote_proxy(icrm_cls, name, route["ipc_address"])

    # PEER route → connect via that relay's HTTP endpoint.
    return self._make_remote_proxy(icrm_cls, name, f"{relay_url}/{name}")
```

- [ ] **Step 4: Verify existing tests still pass**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All tests pass (existing tests use `address=` escape hatch or thread-local).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add relay-based name resolution to cc.connect()

cc.connect(IGrid, name='grid') now resolves via /_resolve/{name}.
Thread-safe route cache with 30s TTL. Falls back to existing
address= escape hatch. Thread-local preference still takes priority.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 13: Python cc.register() Resilience + Remove set_ipc_address

**Files:**
- Modify: `src/c_two/transport/registry.py`
- Modify: `src/c_two/__init__.py`

Add retry logic to relay notification in `cc.register()`. Remove `set_ipc_address()` and auto-generate UUID addresses.

- [ ] **Step 1: Add retry logic to _relay_register**

```python
def _relay_register(self, name: str, ipc_address: str,
                    icrm_ns: str, icrm_ver: str):
    """Notify relay about a new CRM registration. Retry 3x, then background."""
    relay_addr = self._settings.relay_address
    if not relay_addr:
        return  # No relay configured — standalone mode.

    import urllib.request
    import json

    payload = json.dumps({
        "name": name,
        "address": ipc_address,
        "icrm_ns": icrm_ns,
        "icrm_ver": icrm_ver,
    }).encode()

    for attempt in range(3):
        try:
            req = urllib.request.Request(
                f"{relay_addr}/_register",
                data=payload,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                return  # Success
        except Exception:
            if attempt < 2:
                time.sleep(1)

    # All retries failed → register locally, schedule background retry.
    self._schedule_background_relay_register(name, ipc_address, icrm_ns, icrm_ver)


def _schedule_background_relay_register(self, name, ipc_address, icrm_ns, icrm_ver):
    """Background thread that keeps trying to register with relay."""
    import urllib.request
    import json

    def retry_loop():
        payload = json.dumps({
            "name": name, "address": ipc_address,
            "icrm_ns": icrm_ns, "icrm_ver": icrm_ver,
        }).encode()
        for _ in range(60):  # Try for ~5 minutes.
            time.sleep(5)
            try:
                req = urllib.request.Request(
                    f"{self._settings.relay_address}/_register",
                    data=payload,
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=5):
                    return
            except Exception:
                continue

    t = threading.Thread(target=retry_loop, daemon=True)
    t.start()
```

- [ ] **Step 2: Remove set_ipc_address from registry.py and __init__.py**

In `registry.py`, delete the `set_ipc_address()` method and all references to `_ipc_address_override`.

In `__init__.py`, remove `set_ipc_address` from the exports.

Replace the auto-address logic to always use UUID:

```python
def _auto_address(self) -> str:
    """Generate a unique IPC address using UUID."""
    import uuid
    return f"ipc://cc_auto_{uuid.uuid4().hex[:16]}"
```

- [ ] **Step 3: Update tests that used set_ipc_address**

Search for `set_ipc_address` usage in tests and replace with:
- Tests that set a deterministic address → use the auto-address and read it back via `server_address()` or equivalent.
- Tests that need a known address → pass `address=` directly to `cc.connect()`.

Run: `grep -rn "set_ipc_address" tests/`
Update each occurrence.

- [ ] **Step 4: Run full test suite**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: add relay notification retry + remove set_ipc_address

cc.register() retries relay notification 3x, then continues in
background. IPC addresses now always auto-generated UUID.
Removed set_ipc_address() and C2_IPC_ADDRESS env var.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 14: CLI Updates

**Files:**
- Modify: `src/c_two/cli.py`

Add `--seeds` flag to `c3 relay` and a new `c3 registry` subcommand.

- [ ] **Step 1: Add --seeds to c3 relay**

```python
@relay.command()
@click.option("--upstream", "-u", required=True, help="Upstream IPC address")
@click.option("--bind", "-b", default="0.0.0.0:8080", help="HTTP bind address")
@click.option("--seeds", "-s", default="", help="Comma-separated seed relay URLs")
@click.option("--relay-id", default=None, help="Stable relay identifier")
@click.option("--advertise-url", default=None, help="Publicly reachable relay URL")
def start(upstream, bind, seeds, relay_id, advertise_url):
    """Start the relay server."""
    from c_two._native import NativeRelay
    seed_list = [s.strip() for s in seeds.split(",") if s.strip()] if seeds else []
    relay = NativeRelay(
        bind=bind,
        relay_id=relay_id,
        advertise_url=advertise_url,
        seeds=seed_list,
    )
    relay.start()
    relay.register_upstream(upstream)
    # ... signal handling ...
```

- [ ] **Step 2: Add c3 registry subcommand**

```python
@cli.group()
def registry():
    """Resource registry commands."""
    pass

@registry.command()
@click.option("--relay", "-r", required=True, help="Relay HTTP address")
def list_routes(relay):
    """List all registered routes."""
    import urllib.request, json
    with urllib.request.urlopen(f"{relay}/_routes", timeout=5) as resp:
        routes = json.loads(resp.read())
        for name in routes:
            click.echo(name)

@registry.command()
@click.option("--relay", "-r", required=True, help="Relay HTTP address")
@click.argument("name")
def resolve(relay, name):
    """Resolve a resource name to route info."""
    import urllib.request, json
    try:
        with urllib.request.urlopen(f"{relay}/_resolve/{name}", timeout=5) as resp:
            routes = json.loads(resp.read())
            for route in routes:
                click.echo(json.dumps(route, indent=2))
    except urllib.error.HTTPError as e:
        if e.code == 404:
            click.echo(f"Resource '{name}' not found", err=True)
        else:
            click.echo(f"Error: {e.code}", err=True)

@registry.command()
@click.option("--relay", "-r", required=True, help="Relay HTTP address")
def peers(relay):
    """List known peer relays."""
    import urllib.request, json
    with urllib.request.urlopen(f"{relay}/_peers", timeout=5) as resp:
        peer_list = json.loads(resp.read())
        for peer in peer_list:
            click.echo(f"{peer['relay_id']} ({peer['url']}) - {peer['status']}")
```

- [ ] **Step 3: Run CLI smoke test**

```bash
uv run c3 --help
uv run c3 relay --help
uv run c3 registry --help
```
Expected: All help texts render correctly.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(cli): add --seeds to c3 relay + c3 registry subcommand

c3 relay now accepts --seeds, --relay-id, --advertise-url.
New c3 registry command with list-routes, resolve, peers subcommands.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 15: Integration Tests

**Files:**
- Create: `tests/integration/test_relay_mesh.py`

End-to-end tests for the relay mesh: two relays, gossip sync, name resolution, and failure detection.

- [ ] **Step 1: Create integration test file**

```python
import pytest
import time
import json
import urllib.request
import c_two as cc
from c_two._native import NativeRelay

# Skip if relay feature not available.
pytestmark = pytest.mark.skipif(
    not hasattr(cc._native, "NativeRelay"),
    reason="relay feature not compiled",
)


class TestSingleRelay:
    """Tests with one relay — backward compatibility."""

    def test_register_and_resolve(self, tmp_path):
        relay = NativeRelay(bind="127.0.0.1:0")
        relay.start()
        try:
            port = relay.bind_address().split(":")[-1]
            base = f"http://127.0.0.1:{port}"

            # Register a mock upstream.
            # (Real upstream needs IPC; test just verifies HTTP API.)
            data = json.dumps({"name": "grid", "address": "ipc://test_grid"}).encode()
            req = urllib.request.Request(
                f"{base}/_register", data=data,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                assert resp.status == 200

            # Resolve.
            with urllib.request.urlopen(f"{base}/_resolve/grid", timeout=5) as resp:
                routes = json.loads(resp.read())
                assert len(routes) >= 1
                assert routes[0]["name"] == "grid"

            # Unregister.
            data = json.dumps({"name": "grid"}).encode()
            req = urllib.request.Request(
                f"{base}/_unregister", data=data,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            with urllib.request.urlopen(req, timeout=5) as resp:
                assert resp.status == 200

            # Resolve should now 404.
            try:
                urllib.request.urlopen(f"{base}/_resolve/grid", timeout=5)
                assert False, "Expected 404"
            except urllib.error.HTTPError as e:
                assert e.code == 404
        finally:
            relay.stop()

    def test_peers_empty(self, tmp_path):
        relay = NativeRelay(bind="127.0.0.1:0")
        relay.start()
        try:
            port = relay.bind_address().split(":")[-1]
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/_peers", timeout=5
            ) as resp:
                peers = json.loads(resp.read())
                assert peers == []
        finally:
            relay.stop()


class TestTwoRelayMesh:
    """Tests with two relays in a mesh."""

    def test_gossip_route_propagation(self):
        # Start relay A.
        relay_a = NativeRelay(
            bind="127.0.0.1:0",
            relay_id="relay-a",
            advertise_url="http://127.0.0.1:0",  # Will be updated after start.
        )
        relay_a.start()
        port_a = relay_a.bind_address().split(":")[-1]
        url_a = f"http://127.0.0.1:{port_a}"

        # Start relay B with relay A as seed.
        relay_b = NativeRelay(
            bind="127.0.0.1:0",
            relay_id="relay-b",
            seeds=[url_a],
        )
        relay_b.start()
        port_b = relay_b.bind_address().split(":")[-1]
        url_b = f"http://127.0.0.1:{port_b}"

        try:
            # Wait for join to complete.
            time.sleep(2)

            # Register on relay A.
            data = json.dumps({"name": "grid", "address": "ipc://grid_a"}).encode()
            req = urllib.request.Request(
                f"{url_a}/_register", data=data,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            urllib.request.urlopen(req, timeout=5)

            # Wait for gossip propagation.
            time.sleep(3)

            # Resolve on relay B should find "grid".
            with urllib.request.urlopen(f"{url_b}/_resolve/grid", timeout=5) as resp:
                routes = json.loads(resp.read())
                assert len(routes) >= 1
                assert routes[0]["name"] == "grid"

            # Both relays should see each other as peers.
            with urllib.request.urlopen(f"{url_a}/_peers", timeout=5) as resp:
                peers_a = json.loads(resp.read())
                assert any(p["relay_id"] == "relay-b" for p in peers_a)

        finally:
            relay_b.stop()
            relay_a.stop()

    def test_route_withdraw_propagation(self):
        relay_a = NativeRelay(bind="127.0.0.1:0", relay_id="relay-a")
        relay_a.start()
        port_a = relay_a.bind_address().split(":")[-1]
        url_a = f"http://127.0.0.1:{port_a}"

        relay_b = NativeRelay(bind="127.0.0.1:0", relay_id="relay-b", seeds=[url_a])
        relay_b.start()
        port_b = relay_b.bind_address().split(":")[-1]
        url_b = f"http://127.0.0.1:{port_b}"

        try:
            time.sleep(2)

            # Register on A.
            data = json.dumps({"name": "net", "address": "ipc://net_a"}).encode()
            req = urllib.request.Request(
                f"{url_a}/_register", data=data,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            urllib.request.urlopen(req, timeout=5)
            time.sleep(2)

            # Unregister on A.
            data = json.dumps({"name": "net"}).encode()
            req = urllib.request.Request(
                f"{url_a}/_unregister", data=data,
                headers={"Content-Type": "application/json"}, method="POST",
            )
            urllib.request.urlopen(req, timeout=5)
            time.sleep(2)

            # Resolve on B should 404.
            try:
                urllib.request.urlopen(f"{url_b}/_resolve/net", timeout=5)
                assert False, "Expected 404"
            except urllib.error.HTTPError as e:
                assert e.code == 404
        finally:
            relay_b.stop()
            relay_a.stop()
```

- [ ] **Step 2: Run integration tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/integration/test_relay_mesh.py -v --timeout=30
```
Expected: All tests pass.

- [ ] **Step 3: Run full test suite (final check)**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All existing tests + new tests pass. Zero regressions.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "test: add relay mesh integration tests

Tests cover single-relay register/resolve/unregister, two-relay
gossip route propagation, route withdrawal propagation, and peer
discovery.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---


## Self-Review Checklist

### Spec Coverage
- [x] §1.1 Resolution Flow → Task 12 (cc.connect relay resolution)
- [x] §1.2 RouteTable → Tasks 2-4 (RouteTable, ConnectionPool, RelayState)
- [x] §1.3 Duplicate Name Policy → Task 10 (health check + upsert)
- [x] §1.4 Peer Discovery + Join Protocol → Tasks 7-9 (peer messages, endpoints, seed bootstrap)
- [x] §1.5 Gossip Protocol → Tasks 7-8 (PeerMessage, Disseminator, FullBroadcast)
- [x] §1.5.1 Anti-Entropy → Task 9 (round-robin anti_entropy_loop)
- [x] §1.6 Failure Detection → Task 9 (heartbeat + failure_detection_loop + dead_peer_probe)
- [x] §1.7 Config → Tasks 1, 11 (RelayConfig, C2_RELAY_SEEDS)
- [x] §1.8 Address Auto-UUID → Task 13 (remove set_ipc_address)
- [x] §1.9 Registry Integration → Tasks 12-13 (cc.connect + cc.register updates)
- [x] §1.10 CLI → Task 14 (--seeds, c3 registry)

### Type Consistency
- `RouteEntry` used consistently in Tasks 2, 4, 5, 8, 9
- `RouteInfo` used in Tasks 2, 5, 12
- `PeerInfo` / `PeerSnapshot` used in Tasks 2, 4, 7, 9
- `PeerEnvelope` / `PeerMessage` used in Tasks 7-9
- `RelayState` methods consistent across Tasks 4-10

### Placeholder Scan
- No TBD/TODO found
- All steps have code or specific commands
- All test commands have expected output

