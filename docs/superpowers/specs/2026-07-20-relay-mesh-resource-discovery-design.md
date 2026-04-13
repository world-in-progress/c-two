# Relay Mesh Resource Discovery — Design Spec

**Date:** 2026-07-20
**Status:** Draft
**Scope:** Full architecture (Phase 1–3) with Phase 1 implementation detail

## Problem Statement

C-Two currently requires clients to know the exact IPC or HTTP address of the CRM process they want to connect to. This creates three pain points:

1. **Address coupling** — `cc.connect(IGrid, name='grid', address='ipc://...')` forces clients to track process addresses manually.
2. **No multi-node discovery** — No mechanism to find CRM resources across machines.
3. **No replica awareness** — When the same CRM resource exists on multiple nodes (e.g., read replicas), there is no coordination for write consistency or staleness tracking.

## Proposed Solution

A **decentralized Relay Mesh** where every node runs a Relay that:

1. Maintains a **Registry CRM** — a fully-replicated route table stored as a regular CRM resource.
2. Synchronizes route state with peer relays via **gossip** (HTTP-based, reusing the existing axum stack).
3. Enables `cc.connect(IGrid, name='grid')` to resolve the target relay automatically and connect directly (single hop).

The architecture is **self-bootstrapping**: the registry is itself a CRM, consuming C-Two's own transport, concurrency, and serialization infrastructure.

## Architecture Overview

```
┌──────────┐    gossip     ┌──────────┐    gossip     ┌──────────┐
│ Relay-A  │◄────────────► │ Relay-B  │◄────────────► │ Relay-C  │
│ Node-1   │               │ Node-2   │               │ Node-3   │
│          │               │          │               │          │
│ Registry │  full replica  │ Registry │  full replica  │ Registry │
│ CRM      │◄────────────► │ CRM      │◄────────────► │ CRM      │
│          │               │          │               │          │
│ app CRMs │               │ app CRMs │               │ app CRMs │
└──────────┘               └──────────┘               └──────────┘
     ▲                          ▲                          ▲
     │ IPC                      │ IPC                      │ IPC
  Client-1                   Client-2                   Client-3
```

**Key properties:**

- **No single point of failure.** Every relay holds a full copy of the route table.
- **Zero extra hops for local resources.** Same-node CRM → IPC direct.
- **Single hop for remote resources.** Resolve locally → HTTP direct to target relay.
- **Self-bootstrapping.** Registry CRM is a standard CRM, validated by C-Two's own test suite.

## Phase 1: Naming + Discovery

### 1.1 Name Resolution Flow

```python
grid = cc.connect(IGrid, name='grid')
```

Resolution chain (evaluated in order, first match wins):

| Priority | Check | Transport | Latency |
|----------|-------|-----------|---------|
| 1 | Same process (`_registrations[name]`) | thread-local proxy | ~0 |
| 2 | Local relay has `name` as LOCAL route | IPC direct to CRM | ~0.3ms |
| 3 | Local relay has `name` as PEER route | HTTP direct to owning relay | ~1-5ms |
| 4 | None found | raise `ResourceNotFound` | — |

For priority 3 (cross-node), the client resolves the target relay URL from the local Registry CRM, then establishes a direct HTTP connection to that relay. Subsequent RPC calls go directly to the target relay — the resolution happens once at `cc.connect()` time.

Explicit `address=` parameter is still supported and bypasses the resolution chain entirely (backward compatible).

### 1.2 Registry CRM

Each relay embeds a Registry CRM with the following ICRM interface:

```python
@cc.icrm(namespace='cc.internal', version='0.1.0')
class IRelayRegistry:
    @cc.read
    def resolve(self, name: str) -> RouteInfo | None:
        """Resolve a CRM name to its hosting relay's URL.

        Returns None if the name is not registered anywhere.
        """
        ...

    @cc.read
    def list_routes(self) -> list[RouteEntry]:
        """List all known routes across the mesh."""
        ...

    @cc.read
    def list_relays(self) -> list[RelayInfo]:
        """List all known peer relays."""
        ...

    @cc.write
    def register_route(self, name: str, relay_id: str, address: str,
                       icrm_namespace: str, icrm_version: str) -> None:
        """Register a CRM route (called by local relay on cc.register)."""
        ...

    @cc.write
    def unregister_route(self, name: str, relay_id: str) -> None:
        """Unregister a CRM route (called by local relay on cc.unregister)."""
        ...

    @cc.write
    def register_relay(self, relay_id: str, url: str) -> None:
        """Register a peer relay (called during peer join)."""
        ...

    @cc.write
    def unregister_relay(self, relay_id: str) -> None:
        """Unregister a peer relay (called during peer leave or failure)."""
        ...
```

#### Data Model

```python
@dataclass
class RouteEntry:
    name: str                # CRM routing name
    relay_id: str            # Which relay hosts it
    relay_url: str           # HTTP URL of the hosting relay
    icrm_namespace: str      # ICRM namespace (e.g., 'cc.demo')
    icrm_version: str        # ICRM version (e.g., '0.1.0')
    locality: str            # 'local' or 'peer'
    registered_at: float     # Unix timestamp

@dataclass
class RelayInfo:
    relay_id: str            # Unique relay identifier
    url: str                 # HTTP URL
    route_count: int         # Number of CRMs on this relay
    last_heartbeat: float    # Unix timestamp of last heartbeat
    status: str              # 'alive' | 'suspect' | 'dead'

@dataclass
class RouteInfo:
    name: str
    relay_url: str           # Where to connect
    icrm_namespace: str
    icrm_version: str
```

#### Storage

Each Registry CRM persists its state to local SQLite (path: `{C2_DATA_DIR}/registry.db` or in-memory for single-node).

Tables:
- `routes(name TEXT, relay_id TEXT, relay_url TEXT, icrm_ns TEXT, icrm_ver TEXT, registered_at REAL, PRIMARY KEY (name, relay_id))`
- `relays(relay_id TEXT PRIMARY KEY, url TEXT, last_heartbeat REAL, status TEXT)`

### 1.3 Duplicate Name Policy

- **Same node:** Rejected (409 Conflict). A single machine gains nothing from running duplicate CRM instances of the same resource — the existing `READ_PARALLEL` scheduler already handles concurrent reads via thread pool, and duplicate processes waste CPU/GPU/memory.
- **Cross node:** Allowed. Multiple relays can register the same `name`. The route table stores all entries. Resolution returns the LOCAL entry first (if available), otherwise picks the nearest peer (Phase 1: first registered; Phase 2: version-aware selection).

### 1.4 Peer Discovery (Seed Bootstrap)

Relays discover each other via **seed addresses** — a list of known relay URLs provided at startup:

```bash
# Environment variable
C2_RELAY_SEEDS=http://node1:8080,http://node2:8080

# CLI argument
c3 relay --bind 0.0.0.0:8080 --seeds http://node1:8080,http://node2:8080

# Cluster environments
# K8s: headless service
C2_RELAY_SEEDS=http://c2-relay.default.svc:8080
# Slurm: derive from SLURM_NODELIST
```

No designated "head node." Any relay can be a seed. Seeds are only used for initial connection; once joined, a relay learns about all peers from the mesh.

#### Join Protocol

```
Relay-C starts up:
  1. Read C2_RELAY_SEEDS → [http://node1:8080, http://node2:8080]
  2. Try POST /_peer/join to each seed (first success wins)
     Body: {"relay_id": "relay-c", "url": "http://node3:8080"}
  3. Seed responds with full route table + peer list (FULL_SYNC)
  4. Relay-C merges into local Registry CRM
  5. Relay-C announces itself to ALL peers learned from sync
     POST /_peer/announce to each peer
  6. Relay-C registers its local CRMs as routes
     → Each triggers gossip to all peers
```

If all seeds are unreachable (e.g., this relay starts first):
- Relay operates in standalone mode
- Retries seed connection periodically (every 10s)
- Once a seed becomes reachable, completes the join protocol

### 1.5 Gossip Protocol (HTTP-based)

All peer communication uses HTTP endpoints on the relay (new `/_peer/*` routes), reusing the existing axum stack.

#### Endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/_peer/join` | POST | New relay announces itself to a seed |
| `/_peer/sync` | GET | Request full route table + peer list |
| `/_peer/announce` | POST | Broadcast a route or relay state change |
| `/_peer/heartbeat` | POST | Periodic liveness signal |
| `/_peer/leave` | POST | Graceful shutdown notification |

#### Message Types

```rust
enum PeerMessage {
    RouteAnnounce { name: String, relay_id: String, relay_url: String,
                    icrm_ns: String, icrm_ver: String },
    RouteWithdraw { name: String, relay_id: String },
    RelayJoin    { relay_id: String, url: String },
    RelayLeave   { relay_id: String },
    Heartbeat    { relay_id: String, route_count: u32 },
    // Phase 2 (reserved):
    // StaleNotify  { name: String, version: u64 },
    // WriteIntent  { name: String, writer_relay_id: String },
    // WriteComplete { name: String, version: u64 },
}
```

#### Dissemination Strategy

**Phase 1: Full broadcast** (adequate for <100 nodes).

On route change: send `/_peer/announce` to ALL known peers.

The dissemination layer is abstracted behind a `Disseminator` trait:

```rust
trait Disseminator: Send + Sync {
    async fn broadcast(&self, msg: PeerMessage, peers: &[RelayInfo]);
}

struct FullBroadcast;  // Phase 1: send to all
// struct GossipFanout { fan_out: usize };  // Future: probabilistic
```

This allows swapping to probabilistic gossip (SWIM-style) if C-Two is deployed on 1000+ node clusters, without changing the rest of the architecture.

### 1.6 Failure Detection

**Dual-mode detection:**

| Mode | Mechanism | Detection Latency |
|------|-----------|-------------------|
| Active | Heartbeat every 5s; 3 consecutive misses → dead | 15s worst case |
| Passive | RPC forwarding failure → immediate mark dead | 0s (on traffic) |

When a peer is marked dead:

1. Set peer status to `dead` in Registry CRM.
2. Remove all routes from that peer.
3. If a removed route has replicas on other peers, traffic auto-routes to surviving replicas.
4. Log warning: `[relay] Peer relay-b marked dead; removed N routes`.
5. If the dead peer recovers, it re-joins via seed protocol and FULL_SYNC restores its routes.

### 1.7 Client Configuration

Clients need to know their local relay address for name resolution. Two mechanisms:

```python
# Option 1: Environment variable (recommended for deployments)
# C2_RELAY_ADDRESS=http://localhost:8080

# Option 2: Programmatic
cc.set_relay('http://localhost:8080')
```

`C2_RELAY_ADDRESS` semantics remain unchanged: it points to the relay this process should use for remote access. The relay now also serves as the name resolution endpoint.

For same-node scenarios where only IPC is needed (no relay), name resolution falls back to local-only (priorities 1–2 in the resolution chain). The relay is only needed for cross-node discovery.

### 1.8 Registry Integration with `cc.register()` and `cc.connect()`

#### Server side (`cc.register`)

```python
def register(icrm_class, crm_instance, name, ...):
    # ... existing logic (create server, register CRM) ...

    # NEW: if relay is configured, notify registry
    if relay_address:
        registry = _get_or_connect_registry(relay_address)
        registry.register_route(
            name=name,
            relay_id=_local_relay_id(),
            address=server_address,
            icrm_namespace=icrm_class.__cc_namespace__,
            icrm_version=icrm_class.__cc_version__,
        )
        # Relay's gossip will propagate to all peers
```

#### Client side (`cc.connect`)

```python
def connect(icrm_class, name, address=None):
    # Priority 1: explicit address (backward compatible)
    if address:
        return _connect_explicit(icrm_class, name, address)

    # Priority 2: same-process
    if name in _registrations:
        return ICRMProxy.thread_local(...)

    # Priority 3: resolve via relay
    relay_address = _get_relay_address()
    if relay_address:
        route_info = _resolve_via_relay(relay_address, name)
        if route_info:
            if route_info.relay_url == relay_address:
                # Local relay → IPC direct
                return _connect_ipc(icrm_class, name, route_info)
            else:
                # Remote relay → HTTP direct to target relay
                return _connect_http(icrm_class, name, route_info.relay_url)

    raise ResourceNotFound(f"CRM '{name}' not found")
```

### 1.9 Relay CLI Changes

```bash
# Start relay with peer discovery
c3 relay --bind 0.0.0.0:8080 --seeds http://node1:8080,http://node2:8080

# Start relay in standalone mode (no peers)
c3 relay --bind 0.0.0.0:8080

# New: query the registry
c3 registry list-routes
c3 registry list-relays
c3 registry resolve grid
```

---

## Phase 2: Write Consistency

### 2.1 Problem

When the same CRM name exists on multiple nodes (replicas), a `@cc.write` method call on one node mutates state without notifying others. Replicas become stale.

### 2.2 Design Principle

**C-Two owns the consistency protocol; applications own the consistency implementation.**

C-Two provides:
- Write detection (leveraging existing `@cc.read`/`@cc.write` annotations)
- Version tracking per CRM name
- Stale notification via gossip (`STALE_NOTIFY`)
- Configurable read-when-stale behavior

Applications provide:
- Data refresh implementation (how to sync from primary)
- Data transfer mechanism (RustFS, MinIO, direct RPC, etc.)

### 2.3 Version Tracking

Each registered CRM route gains a `version: u64` field, starting at 0.

When a `@cc.write` method executes successfully on a CRM:
1. The local relay increments the route's version.
2. The relay gossips `STALE_NOTIFY(name, new_version)` to all peers.
3. Peer relays compare versions: if their local replica's version < received version, mark it as **stale**.

### 2.4 Write Coordination

To prevent concurrent writes to the same logical resource across nodes:

```
Write request arrives at Relay-A for 'grid':
  1. Relay-A checks: does 'grid' have replicas on other nodes?
  2. If no replicas → execute locally (no coordination needed)
  3. If replicas exist:
     a. Broadcast WRITE_INTENT(name='grid', writer='relay-a') to all peers
     b. Peers respond:
        - ACK if no local write in progress for 'grid'
        - NACK if a write is already in progress
     c. If all ACK → execute write → broadcast WRITE_COMPLETE + STALE_NOTIFY
     d. If any NACK → reject with WriteConflict error (fast fail)
```

This is a **distributed writer-priority lock** — the same pattern as C-Two's `_WriterPriorityReadWriteLock` in `scheduler.py`, lifted to the relay mesh level.

**Timeout:** If a peer doesn't respond to WRITE_INTENT within 5 seconds, treat as NACK (fail-safe).

### 2.5 Stale-on-Read Behavior

When a client reads from a stale replica, the relay can respond in three configurable modes:

| Mode | Behavior | Use Case |
|------|----------|----------|
| `serve_stale` | Return data with `X-C2-Stale: true` header | Latency-sensitive reads that tolerate staleness |
| `refuse` | Return `StaleResource` error; client retries on primary | Strong consistency requirement |
| `callback` | Trigger `on_stale` callback before serving | Application-controlled refresh |

Default: `serve_stale` (most permissive; applications opt into stronger guarantees).

Configuration:

```python
cc.register(IGrid, grid, name='grid', stale_policy='callback')
```

### 2.6 On-Stale Callback

```python
class Grid:
    @cc.on_stale
    def refresh(self, primary_relay_url: str, current_version: int) -> None:
        """Called when this replica is detected as stale.

        The implementation should fetch fresh state from the primary.
        """
        # Application-specific: could use RustFS, MinIO, direct RPC, etc.
        fresh_data = fetch_from_primary(primary_relay_url)
        self._state = fresh_data
```

After `on_stale` completes, the local relay updates the replica's version to match the primary.

---

## Phase 3: Data Refresh Integration

### 3.1 Problem

Phase 2 detects staleness and notifies replicas. Phase 3 provides standard mechanisms for actually refreshing stale data.

### 3.2 Built-in Refresh Strategies

C-Two provides optional built-in refresh strategies that applications can use instead of writing custom `on_stale` callbacks:

| Strategy | Mechanism | When to Use |
|----------|-----------|-------------|
| `rpc_sync` | Call `@cc.read` methods on primary via RPC; replay results locally | Small/medium state; CRM exposes full state via read methods |
| `snapshot` | Primary serializes full state via `@cc.transfer`; replica deserializes | Medium state; transferable types already defined |
| `external` | Application provides custom `on_stale` callback | Large state; external storage (RustFS, MinIO) |

Configuration:

```python
cc.register(IGrid, grid, name='grid',
            stale_policy='callback',
            refresh_strategy='snapshot')
```

### 3.3 Consistency Levels

Different CRM resources may require different consistency guarantees. C-Two provides named consistency levels:

| Level | Write Coordination | Read Behavior | Description |
|-------|-------------------|---------------|-------------|
| `none` | No coordination | Serve independently | Replicas are fully independent (no staleness tracking) |
| `notify` | No coordination | Serve with stale flag | Writes update version; replicas notified but no blocking |
| `coordinate` | Distributed write lock | Configurable (serve_stale / refuse / callback) | Full write coordination across replicas |

Default: `none` (backward compatible). Applications opt into higher levels:

```python
cc.register(IGrid, grid, name='grid', consistency='coordinate')
```

---

## Error Handling

### New Error Types

| Error | Code | Raised When |
|-------|------|-------------|
| `ResourceNotFound` | 701 | `cc.connect(name)` finds no route in registry |
| `ResourceUnavailable` | 702 | Route exists but target relay is unreachable |
| `WriteConflict` | 703 | Concurrent write to replicated resource (Phase 2) |
| `StaleResource` | 704 | Read from stale replica with `refuse` policy (Phase 2) |
| `RegistryUnavailable` | 705 | No relay configured and no local match |

All errors follow fast-fail semantics (no automatic retry at the C-Two level). Clients handle retries explicitly.

---

## Backward Compatibility

- `cc.connect(IGrid, name='grid', address='ipc://...')` continues to work unchanged. Explicit `address` bypasses the registry entirely.
- `C2_RELAY_ADDRESS` continues to work. The relay it points to now also serves as the name resolution endpoint.
- Existing single-node deployments (no relay) are unaffected. Name resolution falls back to local-only.
- `cc.register()` gains no new required parameters. All new parameters (`consistency`, `stale_policy`, `refresh_strategy`) have backward-compatible defaults.

---

## Scope Boundaries

### C-Two Owns

- Name registration and resolution (Registry CRM)
- Relay mesh gossip protocol (peer discovery, route sync, failure detection)
- Write detection and version tracking (`@cc.read`/`@cc.write` awareness)
- Stale notification gossip (`STALE_NOTIFY`)
- Configurable stale-on-read behavior
- `on_stale` callback trigger point
- Built-in refresh strategies (optional)

### C-Two Does NOT Own

- CRM process lifecycle management (start/stop/migrate processes)
- Data replication/synchronization implementation (RustFS, MinIO, etc.)
- Application-level conflict resolution
- Cluster orchestration (node provisioning, job scheduling)
- Resource migration decisions (when/where to move a CRM)

---

## Implementation Layers

### Rust (relay extensions)

| Component | Crate | Changes |
|-----------|-------|---------|
| Peer protocol endpoints | `c2-http` (relay module) | New `/_peer/*` routes in `router.rs` |
| Route table state | `c2-http` (relay module) | Extend `UpstreamPool` or new `RegistryState` |
| Gossip dissemination | `c2-http` (relay module) | `Disseminator` trait + `FullBroadcast` impl |
| Heartbeat sweeper | `c2-http` (relay module) | Extend existing idle sweeper |
| Peer message types | `c2-wire` | New message enum (or JSON via serde) |

### Python (client + server integration)

| Component | File | Changes |
|-----------|------|---------|
| Name resolution | `transport/registry.py` | Extend `connect()` with relay-based resolution |
| Registry notification | `transport/registry.py` | Extend `register()` / `unregister()` to notify relay |
| Relay configuration | `config/settings.py` | New `C2_RELAY_SEEDS` setting |
| CLI extensions | `cli.py` | New `c3 relay --seeds` flag; `c3 registry` subcommand |
| Error types | `errors.py` | New error classes |

---

## Testing Strategy

### Phase 1

- **Unit tests:** Registry CRM CRUD operations; route resolution priority chain; peer message serialization.
- **Integration tests:** Two-relay mesh with CRM registration and cross-relay resolution; seed bootstrap with delayed startup; failure detection and route cleanup.
- **Single-node regression:** Existing tests pass unchanged (no relay configured → local-only behavior).

### Phase 2

- **Unit tests:** Version tracking; WRITE_INTENT/ACK/NACK protocol; stale marking.
- **Integration tests:** Concurrent writes to replicated CRM across relays; stale-on-read behavior modes; `on_stale` callback invocation.

---

## Open Questions (for implementation planning)

1. **Registry CRM storage:** In-memory dict vs. SQLite for persistence across relay restarts. Recommendation: in-memory for Phase 1 (simplicity); SQLite for production (persistence after restart).
2. **Relay ID generation:** UUID vs. hostname-based. Recommendation: `{hostname}_{pid}_{short_uuid}` for debuggability.
3. **Heartbeat protocol:** Full broadcast vs. SWIM-style probabilistic probing. Recommendation: full broadcast for Phase 1 (< 100 nodes); abstract `Disseminator` trait for future optimization.
4. **Peer message format:** JSON (simple, human-readable) vs. binary (efficient). Recommendation: JSON for Phase 1 (peer messages are infrequent and small); binary optimization later if needed.
