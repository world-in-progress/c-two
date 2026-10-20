# Route Token And Endpoint Connection Architecture

> **Status:** Accepted clean-cut direction for the relay/IPC recovery refactor
> **Date:** 2026-06-07
> **Scope:** Direct IPC acquisition, route-bound clients, relay-local IPC fast path,
> relay HTTP data plane, upstream connection pooling, route lifecycle errors
> **Supersedes:** The watch-first correctness framing in
> `docs/plans/2026-06-05-route-catalog-watch-redesign.md`. That older plan
> remains useful historical evidence, but this document is the active design
> contract for the clean 0.x refactor.

## Objective

Hydro exposed a production failure class where a long-lived server registered a
new route after an IPC client connection already existed. The old connection had
learned only the handshake route snapshot, so later `cc.connect(..., name=...)`
for the new route could report `route missing`. In relay-aware paths, that direct
IPC failure could then be disguised as HTTP relay fallback or relay upstream
reconnect trouble.

The clean design separates three concepts that were previously easy to conflate:

- an IPC connection is an endpoint-scoped transport between a client process and
  one server instance;
- a route binding is a route-scoped immutable token acquired from the current
  route authority;
- relay is discovery, route publication, mesh authority, and HTTP bridging. It
  does not own direct IPC connection semantics.

Success means the hydro class and its follow-on failure modes are mechanically
impossible:

1. A reused IPC endpoint connection can acquire routes registered after the
   connection handshake.
2. A route-bound proxy cannot silently retarget itself to a same-name
   replacement route.
3. Relay idle eviction can close only endpoint data-plane connections; it cannot
   imply route ownership loss.
4. Same local relay HTTP fallback is denied when loopback relay resolution
   already selected the failing local IPC endpoint.
5. Generic IPC I/O failures never withdraw routes. Only semantic lifecycle or
   identity evidence can change route authority.

## Non-Negotiable Invariants

1. **Endpoint connection is not route ownership.** One IPC connection may carry
   calls for many routes hosted by the same `server_instance_id`. A route must
   never require its own transport connection.
2. **Route binding is immutable.** `cc.connect(CRM, name=...)` produces a binding
   to one observed `route_uid` and `route_revision`. Existing proxies keep that
   binding until closed.
3. **Acquire is authoritative.** Route-bound proxy creation cannot rely on the
   initial handshake route table or on a client-side watch cache. It must query
   the server route authority when the binding is created.
4. **Calls are token-gated.** Every remote CRM call carries `RouteCallIdentity`
   and the server validates the route token before invoking resource code.
5. **Watch is optional optimization.** Watch/list can improve freshness,
   diagnostics, relay control-plane promptness, or mesh monitoring. It is not the
   direct IPC correctness contract.
6. **Relay registry is not a connection pool.** Register/unregister/lease
   maintain route metadata. Relay-created IPC clients are disposable data-plane
   endpoint connections.
7. **Semantic errors drive semantic state.** `RouteRemoved`, `ResourceClosed`,
   `RouteStale`, `IdentityMismatch`, and `ContractMismatch` can affect route
   authority. `ConnectionReset`, EOF, refused, timeout, and transient connect
   errors affect only the current endpoint connection.
8. **Python stays thin.** Shared route acquisition, fallback, and lifecycle
   decisions belong to Rust core. Python projects the native API and canonical
   errors.

## Runtime Model

```text
Client process
  |
  | endpoint pool key: ipc_address + server_id + server_instance_id
  v
EndpointConnection
  - UDS framing
  - SHM pools
  - heartbeat / pending requests
  - reconnect / idle close
  - no route ownership authority

RouteBinding
  - route_name
  - route_uid
  - route_revision
  - ExpectedRouteContract
  - method table
  - max_payload_size
  - owner server identity

CRM call
  - reuses EndpointConnection
  - carries RouteCallIdentity from RouteBinding
  - server admission validates token before callback
```

This model keeps the high-throughput path cheap without collapsing route
lifecycle into transport lifecycle.

## Direct IPC Contract

Direct IPC acquisition has two explicit operations:

```rust
acquire_route(expected: ExpectedRouteContract) -> RouteBinding
acquire_route_token(
    expected: ExpectedRouteContract,
    route_uid: RouteUid,
    route_revision: u64,
) -> RouteBinding
```

`acquire_route(...)` is used by explicit direct IPC and by local name resolution
when the caller wants the current matching route. It performs a live lookup
against the server route authority before returning a binding.

`acquire_route_token(...)` is used when a caller already selected a specific
route snapshot, such as relay HTTP precheck or relay-local IPC resolution. It
must return `RouteStale` rather than binding a same-name replacement route.

The production call APIs are route-bound:

```rust
call_bound(binding: &RouteBinding, method: &str, payload: &[u8])
call_bound_prealloc(binding: &RouteBinding, method: &str, block: &RequestBlock)
call_bound_sized_stream(binding: &RouteBinding, method: &str, len, stream)
```

Name-only call APIs are not production APIs in the clean model. They must be
removed, made private, or quarantined to tests/diagnostics with names that make
the bypass explicit.

## Relay Contract

Relay has three separate responsibilities.

### Route Authority

The relay route table stores route metadata:

- `route_name`
- `route_uid`
- `route_revision`
- full CRM contract
- `server_id`
- `server_instance_id`
- `ipc_address`
- owner lease metadata
- relay URL / peer metadata

It does not store a long-lived "registration client" as the proof of route
liveness.

### HTTP Data Plane

HTTP clients resolve a route and receive a route token. Every probe/call sends
the expected CRM contract and route token headers. The relay:

1. validates the token against its current route table;
2. acquires or reuses an endpoint-scoped upstream IPC connection for the owner
   server instance;
3. performs exact-token IPC acquire on that endpoint connection;
4. forwards the call through `call_bound*`.

If the route is replaced between HTTP precheck and upstream acquire, the relay
returns `RouteStale` instead of forwarding to the replacement.

### Local IPC Fast Path

When loopback relay resolution selects a local IPC target, the candidate must
carry:

- `ipc_address`
- `server_id`
- `server_instance_id`
- `route_uid`
- `route_revision`
- full expected CRM contract

Native runtime must exact-token acquire that route on the local endpoint. If the
candidate is stale or unavailable and no genuinely distinct candidate remains,
the caller sees canonical `FallbackDenied` with the direct IPC failure details.
The runtime must not retry the same failed local route through the same local
relay HTTP data plane.

## Watch And List Role

Watch/list are demoted from correctness primitives to control-plane utilities.

Allowed uses:

- diagnostics and route visibility;
- relay owner health hints;
- prompt local route-table cleanup;
- mesh anti-entropy or future admin surfaces.

Disallowed uses:

- proving a route exists for direct IPC proxy creation;
- deciding that a cached route can accept a call without server admission;
- withdrawing a route solely because a watch stream disconnected;
- forcing one route per IPC connection to avoid cache drift.

If watch history is compacted, direct IPC acquire remains correct because it can
perform a fresh lookup. Relay control-plane surfaces can relist or report
watch-unavailable diagnostics, but data-plane correctness still comes from exact
route token acquire and call-time admission.

## Error Semantics

Canonical route and endpoint errors must be distinguishable across Rust and SDKs.

Route lifecycle / semantic errors:

- `RouteStale`
- `ResourceClosed`
- `ResourceRemoved`
- `ResourceNotFound`
- `ContractMismatch`
- `IdentityMismatch`
- `ProtocolViolation`

Endpoint / transient transport errors:

- connection reset
- EOF
- refused
- timeout
- endpoint unavailable
- pool acquire failure

Only semantic errors can mutate route authority. Endpoint errors may evict or
mark an endpoint connection unhealthy, then callers can retry or reconnect
according to the routing policy.

## Implementation Phases

Every phase must finish with:

1. focused tests for the phase behavior;
2. broader relevant regression checks;
3. a strict review pass across correctness, architecture, security, and
   performance;
4. any review findings fixed and reverified;
5. one atomic commit.

### Phase 0: Documentation Contract

Record this architecture, update historical issue references, and make the
implementation sequence reviewable.

Acceptance:

- The active design states endpoint connections are server-instance scoped, not
  route scoped.
- The active design states watch is not the correctness contract.
- The hydro failure sequence is tied to authoritative acquire and call-time
  token admission.
- The implementation phases and verification gates are explicit.

Verification:

- `git diff --check`
- source review of this document against the current code and historical issue.

### Phase 1: IPC API Clean Cut

Refactor `c2-ipc` and `c2-python-native` so production callers use explicit
route acquire plus bound calls.

Acceptance:

- Public production IPC API exposes `acquire_route` and
  `acquire_route_token` semantics.
- Route-name call APIs are removed, private, or diagnostic-only.
- Python native clients require a `RouteBinding` for CRM calls.
- Same endpoint connection can acquire routes registered after handshake.
- Old binding to same-name replacement route fails with `RouteStale`.

Verification:

- `cargo test --manifest-path core/Cargo.toml -p c2-ipc`
- `cargo test --manifest-path core/Cargo.toml -p c2-runtime`
- focused Python direct IPC tests through `RuntimeSession.acquire_ipc_client`.

### Phase 2: Direct IPC Watch Demotion

Remove watch dependence from ordinary direct IPC correctness. Retain only
diagnostic or explicitly control-plane use if it earns its complexity.

Acceptance:

- Direct IPC acquire works with watch disabled or unavailable.
- Watch failure cannot turn a valid direct route acquire into
  `RouteWatchUnavailable`.
- `CatalogCompacted` does not affect direct IPC acquire correctness.
- Tests no longer assert watch-driven direct route correctness.

Verification:

- `cargo test --manifest-path core/Cargo.toml -p c2-ipc`
- direct IPC hydro mini reproduction.

### Phase 3: Relay-Local IPC Tokenization

Make local relay-selected IPC candidates exact-token candidates.

Acceptance:

- `RelayLocalIpcCandidate` carries `route_uid` and `route_revision`.
- Native `connect_via_relay` uses exact-token acquire for local IPC candidates.
- Same-name same-contract replacement returns `RouteStale` / `FallbackDenied`;
  it does not bind the replacement.
- Same local relay HTTP fallback remains denied unless a truly distinct route
  candidate exists.

Verification:

- `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30`
  after native rebuild.

### Phase 4: Relay Endpoint Pool

Refactor relay upstream pooling from route-keyed ownership to endpoint-keyed
transport reuse.

Acceptance:

- Pool key is owner endpoint identity:
  `ipc_address + server_id + server_instance_id`.
- Multiple routes on the same server instance can share one upstream endpoint
  connection.
- Idle eviction logs endpoint identity and never implies route withdrawal.
- Register attestation uses temporary clients and does not install a
  registration data-plane client.

Verification:

- relay tests prove two routes on one owner share endpoint connection semantics.
- idle eviction test proves registry state is unchanged.
- transient reconnect failure does not withdraw route.

### Phase 5: Semantic Withdrawal Rules

Constrain route authority mutation to semantic proof.

Acceptance:

- Generic I/O errors evict or mark endpoint unhealthy only.
- Semantic route missing/removed/closed, identity mismatch, and contract
  mismatch produce canonical route authority decisions.
- Relay tombstone and GC logs name affected routes and reasons.

Verification:

- transient reset/refused/timeout tests preserve route table.
- semantic route missing tests withdraw or mark stale as specified.

### Phase 6: SDK And Error Facade Cleanup

Keep Python as a thin native facade and align public errors.

Acceptance:

- `registry.py` does not manage route state, fallback policy, or IPC endpoint
  authority.
- Python exposes canonical `CCError` subclasses for route and fallback errors.
- Explicit `cc.connect(..., address="ipc://...")` bypasses relay even when relay
  env vars are set or relay is unavailable.

Verification:

- `uv sync --reinstall-package c-two`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30`

### Phase 7: Final Review And Performance Guard

Audit the whole stack against this document.

Acceptance:

- No production route-name IPC call path remains.
- No one-route-one-connection assumption remains.
- Watch is optional/control-plane only.
- Relay local IPC, relay HTTP, explicit direct IPC, and thread-local paths all
  enforce route lifecycle correctly.
- No obvious connection explosion, duplicate SHM pool, or extra HTTP fallback
  path is introduced.

Verification:

- `cargo test --manifest-path core/Cargo.toml --workspace`
- `uv sync --reinstall-package c-two`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30`
- targeted hydro-style reproduction with long-lived server, late route
  registration, idle endpoint eviction, and reconnect.

## Review Checklist

Use this checklist at every phase boundary.

- Does this phase move the code closer to endpoint-scoped connections and
  route-scoped immutable bindings?
- Did any new path treat a route-name lookup as sufficient authority?
- Did any relay path use an endpoint I/O error as route withdrawal proof?
- Does the change preserve direct IPC as relay-independent?
- Does the change avoid adding Python-owned generic runtime logic?
- Could it create one IPC connection per route in a multi-route server?
- Are tests proving behavior rather than source-shape only?
- Are errors canonical and diagnosable enough for production logs?
- Are idle eviction, reconnect, and same-name replacement covered?
- Is the phase small enough to commit atomically and review on its own?

## Explicit Non-Goals

- No compatibility shim for name-only production dispatch.
- No Python-side route synchronization engine.
- No route-scoped IPC connection pool.
- No relay-owned direct IPC lifecycle.
- No fallback from a failed loopback local IPC candidate to the same local relay
  HTTP data plane.
- No broad I/O-error route withdrawal.
