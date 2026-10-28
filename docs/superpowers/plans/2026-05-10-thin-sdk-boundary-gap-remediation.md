# Thin SDK Boundary Gap Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the thin-SDK/Rust-core boundary gaps as a mechanically verifiable loop: route contract identity, relay stale-owner replacement, validation ownership, peer route-state ingress, and legacy Python/native bypass removal must all have source-to-sink coverage, tests, and regression guardrails.

**Architecture:** Keep language-neutral authority in Rust. Python may discover Python CRM and transferable metadata, but Rust owns route metadata validation, relay owner freshness, IPC/HTTP contract checks, peer route-state validation, and wire codec shape. Treat the closure tracks below as one coordinated boundary repair with shared final regression coverage.

**Tech Stack:** Rust workspace crates (`c2-contract`, `c2-wire`, `c2-ipc`, `c2-server`, `c2-http`, `c2-runtime`, `c2-python-native`), Python SDK, PyO3/maturin, pytest, cargo tests.

---

## Current Status

Status as of 2026-05-11: Task 1 and Task 2 are implemented. Task 3's
route-contract implementation is in place for named CRM routes and for the
current Python SDK descriptor producer:

- Python registration and connect use one CRM-layer descriptor helper.
- Python descriptor building now uses the shared CRM-layer method discovery
  helper, excludes `@cc.on_shutdown`, rejects incomplete or non-canonical
  remote signatures, rejects custom-hook transferables without exactly one
  explicit ABI declaration, rejects plain no-hook dataclass-field transferables
  until a real field codec exists, and ties built-in pickle fallback descriptors
  to `DEFAULT_PICKLE_PROTOCOL = 4`.
- Default pickle input/output transferables now call
  `pickle.dumps(..., protocol=DEFAULT_PICKLE_PROTOCOL)` instead of relying on
  interpreter defaults.
- Existing custom-hook transferables used by remotely callable test fixtures,
  integration tests, examples, and Python benchmarks now declare `abi_id` or
  `abi_schema`; descriptor guardrails scan those remote surfaces for implicit
  ABI decorators.
- Native registration carries `abi_hash` and `signature_hash` through
  `PyRuntimeSession`, `PyServer`, `RuntimeRouteSpec`, `CrmRoute`, IPC handshake
  projection, relay route state, relay control registration, and HTTP/IPC
  client projections.
- `c2-contract` owns the route-name, CRM-tag, contract-hash, descriptor-hash,
  and `ExpectedRouteContract` validation primitives used by transport crates.
- Direct IPC, relay-discovered IPC, relay HTTP resolve/probe/call, and explicit
  HTTP relay clients validate complete route contracts, not CRM triplets alone.
- Native clients returned by normal `cc.connect(...)` are route-bound at the
  PyO3 boundary, so a client validated for one route cannot call another named
  route without another validation.
- Python-visible native CRM data-plane `call()` methods no longer accept an
  independent `route_name` argument. Route-bound IPC and relay-aware clients
  derive the route from the validated contract; raw pooled IPC/HTTP clients
  reject CRM calls and remain diagnostics/health surfaces only.
- The Rust raw HTTP client no longer exposes public named-route
  `call(route_name, ...)`, `call_async(route_name, ...)`, or name-only
  `probe_route_async(route_name)` methods. Relay-aware clients remain the
  public named-route HTTP data-plane surface and always carry a validated
  `ExpectedRouteContract`.
- Relay registration normally publishes the contract derived from the IPC
  handshake. HTTP or command-loop claims are only additional expected values;
  the old `skip_ipc_validation` path is removed instead of carried as a
  compatibility branch.

This does **not** implement a generated dataclass-field wire ABI, so plain
no-hook transferables remain intentionally rejected when they appear in remote
signatures. Task 3 Step 10's relay peer route-state/full-sync closure is now
implemented for `c2-http`: peer route-state protocol version is bumped, full
sync uses `FullSyncEnvelope`, digest and digest-diff hashes are key-bound
lowercase SHA-256 hex, raw digest repair cannot convert directly into
`RouteEntry`, and production full-sync merge accepts only `ValidatedFullSync`.
Task 3 Step 11's direct IPC mismatch coverage is now implemented: same-tag CRM
contracts with different parameter types, return types, method sets, access
modes, transferable ABI ids, transferable ABI schemas, and default annotation
shapes are rejected for thread-local and explicit direct IPC connects before
dispatch. Direct IPC remains relay-independent when the relay anchor points at
an unavailable endpoint, and Python guardrails reject old route-key
`.call(...)` shapes.
Task 3 Step 12's relay mismatch coverage is now implemented for the relay
client/router/runtime choke points: typed relay resolve rejects malformed
expected contracts before network I/O, relay-aware construction rejects
malformed expected contracts without panic, `/_resolve` and data-plane expected
identity paths reject missing or mismatched hash material, runtime relay
connectors propagate construction errors instead of panicking, and Python relay
mismatch integration tests still pass through the SDK.
The later clean-cut pass also removed runtime name-only relay resolve:
`RelayControlClient::resolve(name)`, `resolve_async(name)`, and Python
`RustRelayControlClient.resolve(name)` are gone; `/_resolve/{route}` requires
the full CRM tag plus both hashes and returns `404` for complete-contract
mismatches instead of exposing a name-only diagnostic. Future name-based mesh
lookup must be a separate catalog/discovery endpoint and must not feed dispatch
clients.
Task 3 Step 13 and Task 4 final closure verification have now been run against
the working tree. A later 2026-05-11 closed-loop review found one remaining
compatibility-shaped relay HTTP data-plane path: missing expected-contract
headers were still accepted as `None` in `/_probe/{route}` and
`/{route}/{method}`. That path is tracked in
`docs/reviews/2026-05-11-relay-http-contract-clean-cut.md` and is now closed by
requiring complete `ExpectedRouteContract` headers at the relay data-plane
choke point, removing `Option<&ExpectedRouteContract>` from crate-internal
HTTP call/probe helpers, and adding source guards against reintroducing the old
shape. The clean-cut pass additionally closes name-only runtime resolve. The
final route-table guard pass removed the last compatibility-shaped private
helper by deleting production `resolve_filtered(name, Option<&ExpectedRouteContract>)`;
test-only name resolution now lives only under `#[cfg(test)]`, while production
`resolve_matching(...)` directly filters by the required full contract. The
remaining unexecuted item is the optional commit step; there are no known open
implementation gaps in the plan after this clean-cut pass.

## Scope

This plan remediates the gaps recorded in `docs/plans/2026-05-04-thin-sdk-rust-core-boundary.md` under "2026-05-10 Coverage Audit And Remaining Gaps" and the subsequent closed-loop reviews that found remaining bypasses in registration, call, peer sync, digest repair, validation ownership, and stale-owner replacement.

The work is intentionally split into five closure tracks:

1. Remove narrow Python compatibility facades and public per-call named-route bypasses that remain from the old Python transport.
2. Add relay-side freshness/fencing semantics for stale-owner replacement.
3. Move route/CRM/hash validation ownership into `c2-contract` and make Python produce canonical CRM descriptors only.
4. Add contract ABI and full method-signature compatibility metadata to registration, connect, call, resolve, relay, IPC, and HTTP negotiation.
5. Version and validate peer route-state ingress, full sync, digest exchange, and digest repair before any route-table mutation.

Each track must preserve the current boundary: the Python SDK remains glue around Rust mechanisms. Do not add SDK-side route retries, route replay, relay authority, duplicate wire validation, or Python-owned generic runtime state.

## File Structure

- Modify `sdk/python/src/c_two/transport/server/native.py`
  - Remove constructor-time CRM registration compatibility.
- Modify `sdk/python/src/c_two/transport/protocol.py`
  - Remove ignored Python-compatible decode limit arguments.
- Modify `sdk/python/tests/unit/test_sdk_boundary.py`
  - Add clean-cut guards for removed Python facades.
- Modify `sdk/python/tests/unit/test_wire.py`
  - Update handshake wrapper expectations.
- Modify direct low-level tests that instantiate `NativeServerBridge`
  - Use explicit `register_crm(...)` calls.
- Modify `core/transport/c2-http/src/relay/conn_pool.rs`
  - Add owner generation and lease freshness to cached upstream slots.
- Modify `core/transport/c2-http/src/relay/route_table.rs`
  - Preserve the invariant that peer snapshots and digests never carry owner-private freshness.
- Modify `core/transport/c2-http/src/relay/types.rs`
  - Keep route serialization free of owner lease metadata; Task 3 adds only peer-safe contract fingerprint fields.
- Modify `core/transport/c2-http/src/relay/authority.rs`
  - Validate replacement freshness before final route replacement, attest IPC route fingerprint metadata, and carry hash-bearing `AttestedRouteContract` values into `RegisterLocal`.
- Modify `core/transport/c2-http/src/relay/state.rs`
  - Carry local-only freshness tokens plus replacement evidence through registration commit and tests.
- Modify `core/transport/c2-http/src/relay/router.rs`
  - Use the refreshed replacement token on HTTP `/_register`.
- Modify `core/transport/c2-http/src/relay/server.rs`
  - Use the refreshed replacement token in command-loop upstream registration.
- Modify `docs/issues/relay-stale-owner-revival-race.md`
  - Mark the issue resolved only after freshness tests pass.
- Modify `core/protocol/c2-wire/src/control.rs`
  - Delegate call-control and reply route-not-found route-key wire limit and validation to `c2-contract`; make route-not-found reply encoding fallible so invalid route text cannot panic or be silently accepted.
- Modify `core/protocol/c2-wire/src/handshake.rs`
  - Extend route metadata with ABI and signature hashes, and delegate CRM-tag, handshake text-field, and hash validation to `c2-contract`.
- Modify `core/Cargo.toml`
  - Add the new lightweight `foundation/c2-contract` crate to the workspace.
- Add `core/foundation/c2-contract/Cargo.toml`
  - Declare only lightweight validation/hash dependencies such as `serde_json`, `sha2`, and `thiserror`; do not depend on transport, relay, Tokio, reqwest, Axum, or `c2-wire`.
- Add `core/foundation/c2-contract/src/lib.rs`
  - Own `ExpectedRouteContract`, named-route validation, call-control route-key validation, reply route-not-found route-text validation, CRM-tag validation, shared wire text-length constants, contract-hash validation, and canonical descriptor JSON normalization/SHA-256 hashing for every transport.
- Modify `core/protocol/c2-wire/Cargo.toml`
  - Depend on `c2-contract` for route-name, CRM-tag, and route contract hash validation; do not own duplicate validators or the expected-contract type.
- Modify `core/protocol/c2-wire/src/tests.rs`
  - Cover route hash encode/decode and malformed hash rejection.
- Modify `sdk/python/native/src/wire_ffi.rs`
  - Expose new `RouteInfo` hash fields to Python.
- Modify `sdk/python/native/src/server_ffi.rs`
  - Accept and validate route hash fields during native route registration.
- Modify `core/transport/c2-server/Cargo.toml`
  - Depend on `c2-contract` for route metadata validation at registration and handshake projection.
- Modify `core/transport/c2-server/src/dispatcher.rs`
  - Store route ABI and signature hashes in `CrmRoute` beside CRM namespace/name/version.
- Modify `core/transport/c2-server/src/server.rs`
  - Validate route hashes through `c2-contract`, include them in handshake metadata, and propagate fallible reply-control encoding through reply writers without panics.
- Modify `core/transport/c2-ipc/src/client.rs`
  - Preserve route hash metadata in `MethodTable`, expose expected-contract checks, and make `route_contract(...)` project CRM identity plus ABI/signature hashes.
- Modify `core/transport/c2-ipc/src/tests.rs`
  - Update reply-control encode call sites for the fallible reply encoder and keep IPC frame tests explicit about error propagation.
- Modify `core/transport/c2-ipc/Cargo.toml`
  - Depend on `c2-contract` for expected-route validation.
- Modify `core/runtime/c2-runtime/src/session.rs`
  - Pass expected route fingerprints through direct IPC, relay-discovered IPC, and explicit HTTP relay connects.
- Modify `core/runtime/c2-runtime/Cargo.toml`
  - Depend on `c2-contract` for runtime-session expected-contract APIs.
- Modify `core/transport/c2-ipc/src/sync_client.rs`
  - Mirror async-client `MethodTable`, route-contract validation, and projection changes so PyO3/runtime-session synchronous IPC paths cannot remain CRM-triplet-only.
- Modify `core/transport/c2-http/src/client/control.rs`
  - Carry expected fingerprint fields in relay control-plane requests, registration expectation payloads, and cache keys.
- Modify `core/transport/c2-http/src/client/client.rs`
  - Carry expected fingerprint headers on relay probe and data-plane calls; remove public named-route call/probe entry points that synthesize no expected contract.
- Modify `core/transport/c2-http/src/client/relay_aware.rs`
  - Require a full expected route contract for named relay-aware clients and filter/reject route candidates whose fingerprints do not match the requested CRM.
- Modify `core/transport/c2-http/src/relay/types.rs`, `route_table.rs`, `router.rs`, `state.rs`, and `server.rs`
  - Store, validate, resolve, probe, and command-loop-register fingerprint metadata.
- Modify `core/transport/c2-http/src/relay/test_support.rs`
  - Update relay live-server fixtures and route-contract tuples to include ABI and signature hash values.
- Modify `core/transport/c2-http/Cargo.toml`
  - Add `c2-contract` as a normal client-safe dependency for expected-contract validation. Add `sha2` and `hex` only as relay-feature-gated optional dependencies for stable relay anti-entropy digest hashes.
- Modify `core/transport/c2-http/src/relay/peer.rs`, a new or existing peer-validation helper module, `gossip.rs`, and peer handlers
  - Carry fingerprint metadata through route announcements, stable digest entries, and digest repair entries, and expose one shared route-state envelope validator used by both HTTP peer handlers and background anti-entropy response paths.
- Modify `core/transport/c2-http/src/relay/peer_handlers.rs`
  - Return versioned full-sync envelopes from both `/_peer/join` and `/_peer/sync`.
- Modify `core/transport/c2-http/src/relay/background.rs` and seed bootstrap code in `server.rs`
  - Parse and validate versioned full-sync envelopes from seed join responses before `merge_snapshot(...)`, and validate every anti-entropy / dead-peer-probe `PeerEnvelope` response before any `DigestDiff` entry can reach `apply_digest_diff(...)`.
- Modify `sdk/python/native/src/client_ffi.rs` and `sdk/python/native/src/runtime_session_ffi.rs`
  - Expose and pass expected fingerprint parameters through Python-facing native connection helpers, return route-bound clients for named routes, and project IPC route contracts with both hashes.
- Modify `sdk/python/native/src/http_ffi.rs`
  - Project relay route fingerprints to Python diagnostics and tests; remove or harden Python-facing named-route calls and relay registration helpers that lack expected fingerprints.
- Modify `sdk/python/native/src/wire_ffi.rs`
  - Map fallible reply-control encoding errors into Python exceptions through a `try_encode_reply_control(...)` Rust helper instead of assuming reply encoding is infallible.
- Modify `sdk/python/native/Cargo.toml`
  - Depend on `c2-contract` for PyO3 expected-contract wrappers and descriptor hashing exports.
- Modify `sdk/python/tests/unit/test_sdk_boundary.py`
  - Update boundary assertions so relay validation points at `c2-contract`, not `c2_wire::handshake` public helpers.
- Modify `sdk/python/tests/unit/test_crm_proxy.py`
  - Migrate mock clients to route-bound `call(method_name, data)` signatures so tests cannot preserve the old public native call shape.
- Modify `sdk/python/tests/integration/test_ipc_buddy_reply.py`
  - Replace direct low-level `rust_client.call(route_name, ...)` checks with route-bound clients or explicit expected-contract helpers.
- Modify `sdk/python/tests/integration/test_http_relay.py`
  - Replace direct `native_client.call(route_name, ...)` close/probe checks with route-bound calls or dedicated closed-state probes.
- Modify `sdk/python/src/c_two/transport/client/proxy.py`
  - Use route-bound native clients without supplying mutable per-call route names after `cc.connect(...)`.
- Modify `sdk/python/src/c_two/crm/transferable.py`
  - Add an explicit ABI declaration surface for custom transferable byte formats.
- Add `sdk/python/src/c_two/crm/methods.py`
  - Own transport-independent CRM RPC method discovery and shutdown filtering helpers used by both descriptor building and `NativeServerBridge`.
- Add `sdk/python/src/c_two/crm/descriptor.py`
  - Build canonical Python CRM contract and transferable descriptors.
- Modify existing custom transferable fixtures and examples
  - Add explicit ABI declarations for every custom-hook transferable that appears in a remotely callable CRM signature.
  - Known files from the current scan: `sdk/python/tests/fixtures/ihello.py`, `sdk/python/tests/integration/test_buffer_lease_ipc.py`, `sdk/python/tests/integration/test_response_allocation_ipc.py`, `sdk/python/tests/integration/test_remote_scheduler_config.py`, and `examples/python/grid/transferables.py`.
- Modify `sdk/python/src/c_two/transport/server/native.py`
  - Compute descriptors for registered CRM classes and pass their hashes into Rust.
- Modify `sdk/python/src/c_two/transport/registry.py`
  - Compute expected descriptors for `cc.connect(...)`.
- Modify `sdk/python/tests/unit/test_crm_descriptor.py`
  - Cover descriptor canonicalization, signature mismatches, and transferable ABI mismatches.
- Modify `sdk/python/tests/integration/`
  - Add direct IPC and relay mismatch rejection tests.

## Design Decisions

- The compatibility cleanup track is a 0.x clean cut. Remove obsolete Python surfaces instead of keeping hidden shims.
- `NativeServerBridge.__init__()` must only construct the server bridge. CRM route registration must happen through `register_crm(...)` or top-level `cc.register(...)`.
- `decode_handshake()` must expose the Rust codec contract directly. Ignored Python keyword limits create false authority and must be removed.
- Relay owner replacement must remain Rust-owned. Python must not retry, replay, or adjudicate relay route ownership.
- Relay owner freshness has one mutable authority: the connection slot owned by `ConnectionPool`. A state-level replacement token may bundle the captured route identity with the slot token, but there must not be a second mutable owner-freshness map that can drift from the slot. Do not serialize freshness into peer snapshots, anti-entropy digests, HTTP resolve responses, or normal route info.
- Relay owner lease duration is derived from `RelayConfig.idle_timeout_secs`. Do not introduce a new environment variable or SDK setting for owner lease duration. The lease is a relay-local freshness fence, not a liveness proof and not an ownership-forfeiture rule: lease expiry may force a candidate to repeat the final owner probe and may invalidate captured tokens, but it must never replace a currently connected owner by itself. When `idle_timeout_secs == 0`, time-based lease expiry is disabled; stale-owner replacement still proceeds only through explicit final-probe evidence.
- A candidate may replace a stale owner only when the captured replacement token still matches the current route, connection slot, owner generation, owner lease epoch, and exactly one final-probe replacement evidence at final commit. Valid replacement evidence is only `ConfirmedDead` or `ConfirmedRouteMissing` from the final owner probe. There is no `LeaseExpired` replacement evidence. If the old owner is reachable and still exposes the route during the final probe, replacement fails with duplicate-owner semantics even when the relay-local lease deadline has passed.
- Time does not bypass connection state. Different-owner replacement may proceed only after the final commit holds route authority, the captured slot still matches, `active_requests == 0`, and the locked slot is already replaceable because the final probe proved `ConfirmedDead` or `ConfirmedRouteMissing`. Route-table write authority alone is not a connection-slot fence: an acquire may already have cloned the old slot after an earlier route-table read. The final replacement must therefore be a connection-pool atomic operation that marks the captured old slot `Retired` under the slot lock before installing the candidate slot and before the new route becomes readable. Any acquire that already cloned the old slot must either increment `active_requests` before the replacement check, causing the candidate commit to fail, or observe `Retired` and fail without dispatching.
- If the old owner re-registers, renews its relay-local lease, or otherwise changes the relay-local generation or lease epoch before candidate commit, the candidate commit must fail with duplicate-owner semantics even if the old owner later becomes disconnected before the candidate reaches commit.
- If an old owner recovers externally before candidate commit and the final probe can reach the old endpoint and observe the route, the relay must preserve that owner even if no relay-local lease renewal occurred. Silent recovery is not a durable freshness signal for captured tokens, but final-probe liveness remains authoritative for preventing replacement.
- Contract fingerprinting is a route-compatibility mechanism, not a move of Python serialization hooks into Rust.
- Python discovers language-specific CRM shape and transferable declarations. Rust owns the canonical hash function, hash validation, route metadata carriage, and all IPC/HTTP mismatch rejection.
- Arbitrary custom transferable byte formats cannot be inferred from Python code. Transferables with custom `serialize()` / `deserialize()` must declare an explicit ABI id or ABI schema string before they can participate in strict cross-language compatibility.
- Plain `@cc.transferable` dataclass-field classes with no real SDK-owned serialization codec must be rejected by descriptor building when they appear in a remotely callable signature. Do not claim a generated dataclass-field ABI until this plan also defines and tests a concrete wire codec for that ABI.
- Python module names and qualified class names are SDK diagnostics and lookup hints, not cross-language route compatibility. They must not participate in `abi_hash` or `signature_hash`; hash material for a transferable must use a stable ABI reference such as a built-in serializer family/version, a declared `abi_id`, or a declared `abi_schema`.
- Dynamically generated `Default*Transferable` classes are an SDK-owned built-in pickle serialization family, not user custom byte formats. They must not require user ABI declarations. Their descriptor entry must include the built-in serializer family name, serializer version, Python minimum compatibility scope, and the CRM method annotations that determine the value shape. The serializer version must be backed by an explicit `DEFAULT_PICKLE_PROTOCOL` constant used by the actual `pickle.dumps(..., protocol=...)` calls; do not describe an implicit interpreter default as a stable ABI version.
- Missing CRM parameter or return annotations are not "compatible by omission". The strict fingerprint path must reject incomplete remotely callable signatures with a clear SDK error during registration or connect.
- Descriptor method membership must match the actual wire method table, not the raw class method list. `@cc.on_shutdown` callbacks are excluded from RPC dispatch and must not enter ABI/signature hash material or strict annotation checks.
- CRM descriptor building must not import `c_two.transport.server.native`. Put shared CRM method discovery in a CRM-layer helper and let both `NativeServerBridge` and descriptor building call that helper, or pass the already-filtered method list from registration into the descriptor builder.
- Full method signatures include `inspect.Parameter.kind` and a stable default-value policy. Reject `*args` and `**kwargs` on remotely callable CRM methods. Encode defaults only when they are JSON-stable scalar values (`None`, `bool`, `int`, finite `float`, or `str`); reject object defaults, callable defaults, non-finite floats, bytes, and mutable containers until a cross-language default-value schema exists.
- Type annotations must be converted into a documented canonical descriptor grammar, not `repr()` or `str()` output. The grammar must cover primitives, `None`, containers, tuples, unions, and transferable references; it must reject unresolved forward references, incomplete annotations, and implicit `Any` on remotely callable signatures.
- The wire hash format is lowercase SHA-256 hex. Empty or absent hash fields are invalid for newly registered routes after this remediation, because the current project is still in 0.x.
- `c2-contract` is the single owner of route-name, call-control route-key, reply route-not-found route-text, CRM-tag, contract-hash, and expected-route validation. Both `validate_named_route_name(...)` and `validate_call_route_key(...)` reject empty strings: wire call control must carry an explicit route key, and `name_len=0` is invalid rather than a connection-default route. Update call sites to import those constants and validators from `c2-contract`; `c2-wire` may use private codec helpers that delegate to `c2-contract`, but it must not retain public compatibility re-exports, independent `MAX_CALL_ROUTE_NAME_BYTES`, independent `MAX_HANDSHAKE_NAME_BYTES`, standalone `validate_crm_tag(...)`, or hash validators with separate literal rules. If a wire field limit remains visible outside `c2-contract`, the plan has failed.
- Relay registration must attest ABI and signature hashes from the connected IPC server handshake. HTTP `/_register` or command-loop claims may provide expected values, but relay route metadata must not trust claim-only hash fields that differ from the actual IPC route.
- Route registration is not exempt from contract-fingerprint closure. `NativeServerBridge.register_crm(...)` is the Python producer of CRM descriptors for registered routes; `PyRuntimeSession.register_route(...)`, `PyServer::build_route(...)`, `RuntimeRouteSpec`, `CrmRoute`, `RuntimeSession::register_route(...)`, and relay control registration are the first Rust sinks. Every one of these signatures and constructors must carry `abi_hash` and `signature_hash`. A plan or implementation that only hardens connect/call/resolve paths while leaving registration producers CRM-triplet-only is still a bypass.
- Expected fingerprints are part of route identity for relay clients. Relay resolve query parameters, data-plane/probe headers, `ResolveCacheKey`, route filtering, and stale-cache invalidation tests must include both `abi_hash` and `signature_hash`; CRM namespace/name/version alone is not a sufficient cache key after this remediation. Guardrails must scan `control.rs`, relay `router.rs`, runtime-session FFI, `c2-runtime`, and IPC validation methods as well as data-plane `.call(...)` methods; otherwise acquisition and resolve paths can remain vulnerable after `.call(route_name, ...)` is removed.
- Peer mesh messages that carry routes must be treated as a protocol shape change. Bump the relay peer protocol version, update route announce and digest repair payloads in `peer.rs` and `gossip.rs`, and reject missing or malformed hash fields before route-table mutation.
- After the peer route shape changes, route-state peer messages from older protocol versions must be rejected instead of accepted by the current "only reject versions greater than ours" check. This applies to `RouteAnnounce`, `RouteWithdraw`, `RelayLeave` / internal `RemovePeerRoutes`, `DigestExchange`, `DigestDiffEntry::Active`, `DigestDiffEntry::Deleted`, and all versioned full snapshots, including snapshots with empty `routes` and empty `tombstones`. Treat `DigestExchange` as route-state affecting even when it looks like a request, because the current handler can create authoritative-missing tombstones while computing the response.
- Peer route-state ingress must have one shared validator for both `peer_handlers.rs` request handlers and `background.rs` anti-entropy / dead-peer-probe response consumers. No production path may mutate route state from a raw `PeerEnvelope`, raw `DigestDiff`, or raw digest entry after the route-shape bump; background response loops must validate the envelope and digest payload before calling any route-table command helper.
- Full-sync snapshots are not currently wrapped in `PeerEnvelope`, so they must get an explicit protocol/schema version before they can participate in version rejection. Seed bootstrap, seed retry, `/_peer/join` responses, and `/_peer/sync` must all use the same versioned full-sync shape.
- Full-sync merge is route-state mutation even when the incoming snapshot has no `routes` and no `tombstones`: current `merge_snapshot(...)` replaces all existing peer routes before rebuilding from the snapshot, and then merges peer URL/status metadata. Therefore old-version and future-version `FullSyncEnvelope` payloads must be rejected unconditionally before `RelayState::merge_snapshot(...)`; do not infer safety from an empty snapshot.
- Relay anti-entropy digests must use a deterministic protocol hash, not `std::collections::hash_map::DefaultHasher`. Use lowercase SHA-256 hex over canonical route/tombstone digest material so different relay binaries observe the same digest for the same route metadata. The canonical digest material must bind the route key (`name`, `relay_id`, and `state`) as well as peer-visible compatibility fields; `DigestDiffEntry.hash` and full-sync tombstone hashes are standalone wire fields and must not be reusable for a different route key that happens to share the same metadata or timestamp.
- Relay digest hash dependencies must not leak into client-only `c2-http` builds. The SHA-256 route-digest helper belongs under the `c2-http` relay implementation because it hashes relay route-state material, not generic contract descriptors. Therefore `sha2` and `hex` are optional dependencies enabled only by the `relay` feature, and `cargo check -p c2-http --no-default-features` must not compile them.
- Low-level admin probes that do not bind to a CRM route, such as direct IPC `ping()` or same-host `shutdown("ipc://...")`, may omit expected fingerprints only with an empty route name. Any low-level API that sends, probes, resolves, or validates a named CRM route must require an `ExpectedRouteContract` containing route name, CRM identity, ABI hash, and signature hash.
- Relay-aware HTTP clients are named-route clients. Public constructors and call paths for `RelayAwareHttpClient` must require an `ExpectedRouteContract`; do not keep a public `RelayAwareHttpClient::new(relay_url, route_name, ...)` path that leaves expected identity unset.
- Native Python client objects returned after route validation must be bound to that validated route. Do not leave `PyRustClient.call(route_name, ...)`, `PyRelayConnectedClient.call(route_name, ...)`, or any other low-level per-call route-name API able to call a different named route without a fresh `ExpectedRouteContract` validation.
- Public Python/PyO3 `.call(...)` methods must never accept `route_name` after this remediation. If a low-level test or diagnostic path still needs an arbitrary named route, it must be named `call_route_with_expected_contract(...)`, validate a complete expected contract in the same call, and remain out of the normal CRM proxy path.

## Closed-Loop Invariants

These invariants are the closure criteria for the contract-fingerprint, relay route-state, validation ownership, named-route, descriptor-producer, and stale-owner work. A task is not complete if any one of these has an uncovered ingress path.

- **Named CRM route call invariant:** every named call/probe/resolve/acquire path is either route-bound at construction or accepts one complete `ExpectedRouteContract`; no public API may accept a mutable `route_name` without the expected CRM identity plus both hashes.
- **Registration producer invariant:** every named registered route gets `abi_hash` and `signature_hash` from the same canonical Python CRM descriptor used by connect-side expected contracts, and those hashes are passed through Python `register_crm`, PyO3 `register_route`, `PyServer::build_route`, `RuntimeRouteSpec`, `CrmRoute`, IPC handshake projection, and relay registration. There is no empty-hash fallback and no registration helper is exempt from this invariant.
- **Descriptor producer invariant:** registration and connect use the same CRM-layer method discovery, annotation grammar, transferable ABI references, and Rust-owned canonical hash function. `@cc.on_shutdown` callbacks are excluded from RPC descriptors, custom hook transferables require explicit ABI identity, and built-in pickle descriptors are tied to `DEFAULT_PICKLE_PROTOCOL`.
- **Stale-owner replacement invariant:** different-owner replacement is authorized only by one final-probe `OwnerReplacementEvidence` consumed by `ConnectionPool::replace_if_owner_token(...)` while the captured old slot is locked. Lease expiry alone is not evidence, and no token-only `can_replace_owner_token(...)` pre-check may survive.
- **Peer route-state ingress invariant:** every raw `PeerEnvelope` that can lead to route-table reads for repair or to route-table mutation must pass through `validate_route_state_envelope(...)`. The validator owns protocol-version gates, sender checks, and hash-bearing payload validation.
- **Full-sync invariant:** `RelayState::merge_snapshot(...)` accepts only `ValidatedFullSync`; every production HTTP decode path creates that newtype from `FullSyncEnvelope`. Old or future protocol versions never enter merge, including empty snapshots, because merge itself is a destructive peer-route replacement operation.
- **Digest repair invariant:** raw `DigestDiffEntry` cannot convert directly into `RouteEntry`. Only validated hash-bearing active diff entries can produce peer routes, and deleted diff entries must validate route name, relay id, removed time, and tombstone digest before creating tombstones.
- **Digest dependency invariant:** relay digest hashing is implemented under the `c2-http` relay feature, and client-only `c2-http` builds must not pull relay-only hashing dependencies.
- **Validation ownership invariant:** named-route, call-control route-key, CRM-tag, hash, and expected-contract validators live in `c2-contract`; transport crates import those validators directly or call private delegating helpers, but they do not expose compatibility re-exports, duplicate numeric limits, or ad hoc validation tables. Empty route keys are invalid for call control, registered route metadata, and `ExpectedRouteContract`.

## Original Implementation Gap Ledger

This ledger freezes the repository evidence that made earlier repair passes
incomplete before the 2026-05-11 route fingerprint hardening. Rows marked
closed below have implementation changes, focused tests, and guardrails in the
current worktree. Rows not marked closed remain plan work and must not be
described as implemented elsewhere.

- **Registration fingerprint gap - closed for named CRM routes on 2026-05-11.**
  `NativeServerBridge.register_crm(...)` now computes descriptor hashes,
  `PyRuntimeSession.register_route(...)`, `PyServer::build_route(...)`,
  `RuntimeRouteSpec`, `CrmRoute`, IPC `RouteInfo`, relay `RouteEntry`, and
  relay control registration all carry `abi_hash` and `signature_hash`.
  Relay registration normally derives published metadata from IPC handshake
  attestation; skip-validation paths require explicit valid hashes.
- **Named-route call bypass gap - closed for normal `cc.connect(...)` clients on
  2026-05-11.** `PyRustClient` and `PyRelayConnectedClient` returned by
  validated connect paths are route-bound and reject a different per-call route
  name before dispatch. Low-level unbound diagnostic clients still exist for
  explicit raw constructors/pools and remain outside the normal CRM proxy path.
- **Peer full-sync and digest raw-ingress gap - closed for `c2-http` relay on
  2026-05-11.** Raw `FullSync`, `RouteEntry`, and `RouteTombstone` are now
  internal non-serde structs; `FullSyncEnvelope`, `FullSyncRoute`, and
  `FullSyncTombstone` are the only peer-sync JSON shape. Seed bootstrap and
  seed retry parse `FullSyncEnvelope`, convert to `ValidatedFullSync`, and only
  then call `RelayState::merge_snapshot(...)`, whose signature no longer accepts
  raw `FullSync`. `DigestEntry` and `DigestDiffEntry` carry key-bound
  `RouteDigestHash` strings, route digests use relay-feature-gated SHA-256 hex
  instead of `DefaultHasher` / `u64`, and raw `DigestDiffEntry` no longer
  converts directly into `RouteEntry`. `validate_route_state_envelope(...)`
  gates peer handlers plus background anti-entropy/dead-peer repair before any
  route-table mutation.
- **Stale-owner replacement gap - closed on 2026-05-11.** Different-owner
  replacement now consumes final-probe evidence through
  `replace_if_owner_token(...)` while holding the captured old slot lock and
  compares route identity, slot pointer/address, owner generation, lease epoch,
  replaceable state, and `active_requests == 0`. The old token-only
  `can_replace_owner_token(...)` pre-check is no longer a production
  replacement authority, and owner freshness remains local to the relay
  connection slot rather than peer-visible route state.
- **Validation ownership gap - closed for route/CRM/hash validation on
  2026-05-11.** `c2-contract` exists and owns route-name, call route-key,
  CRM-tag, contract-hash, expected-contract, and descriptor-hash validation.
  Transport crates delegate to it. Remaining validation work is the peer
  full-sync/digest newtype gate listed above, not CRM route contract matching.
- **Python descriptor producer gap - closed for the current Python SDK on
  2026-05-11.** Registration and connect now share
  `sdk/python/src/c_two/crm/descriptor.py`, which uses
  `sdk/python/src/c_two/crm/methods.py` for the same method membership as
  `NativeServerBridge`. The descriptor excludes `@cc.on_shutdown`, rejects
  missing annotations, `Any`, bare containers, unresolved forward references,
  varargs, unsupported defaults, custom hooks without explicit ABI identity,
  and no-hook transferable pseudo-ABIs. Built-in default pickle serialization is
  pinned to `DEFAULT_PICKLE_PROTOCOL = 4`, and descriptor tests plus remote
  surface guardrails cover fixtures, integration tests, examples, and
  benchmarks. This closure deliberately does not add a generated
  dataclass-field wire codec; such transferables remain rejected in remote
  signatures until a future plan defines and tests that codec.

## Source-To-Sink Closure Matrix

| Invariant | Producer | Consumers and Storage | Network / Wire | FFI / SDK Boundary | Test and Support Paths | Legacy / Bypass Closure |
| --- | --- | --- | --- | --- | --- | --- |
| Registration producer | Python CRM descriptor builder computes `abi_hash` and `signature_hash` during `NativeServerBridge.register_crm(...)` and `cc.connect(...)`. | `RuntimeRouteSpec`, thread-local `CRMSlot`, `CrmRoute`, relay `RouteEntry`, IPC `MethodTable`, HTTP resolve cache, and relay route table store both hashes. | IPC handshake `RouteInfo`, relay `/_register`, `/_resolve`, `/_probe`, peer announce, digest diff, and full-sync wrappers carry both hashes. | `PyRuntimeSession.register_route(...)`, `PyServer::build_route(...)`, `PyRustClient.route_contract(...)`, `PyRelayConnectedClient`, and HTTP FFI project the full expected contract. | Descriptor unit tests, mismatch integration tests, relay fixture tests, and static carrier scans prove every route fixture is hash-bearing. | Remove CRM-triplet-only defaults, empty-hash fallbacks, and tests that construct `CrmRoute` / `RuntimeRouteSpec` without hashes. |
| Descriptor producer | `c_two.crm.methods.rpc_method_names(...)` defines RPC method membership; `c_two.crm.descriptor` builds canonical descriptor JSON and sends it to native hashing. | Registration, connect, thread-local slots, and expected-contract construction consume the same descriptor output. | Only descriptor hashes cross IPC/HTTP/relay; Python diagnostics may include module/qualified names but hash material does not. | PyO3 exposes descriptor hashing/validation from `c2-contract`; Python does not own the canonical SHA-256 normalization. | Descriptor tests cover method order, shutdown exclusion, missing annotations, default policy, custom ABI declarations, and built-in pickle protocol stability. | Remove transport-local method discovery, implicit `pickle.dumps(...)` protocol use, `repr(annotation)`/`str(annotation)` descriptor material, and no-hook transferable pseudo-ABI claims. |
| Named CRM route call | `cc.connect(...)` builds one `ExpectedRouteContract`; native client construction binds the validated route. | Route-bound IPC/HTTP/relay-aware clients store the validated route internally; diagnostics expose route name read-only. | Data-plane calls and probes derive route name from the bound expected contract or from a same-call `ExpectedRouteContract` helper. | Python-visible `.call(...)` signatures are `call(method_name, data)` for route-bound clients; arbitrary-route helpers are explicitly named `*_with_expected_contract`. | Unit test doubles and integration tests use the route-bound signature; negative tests prove a client acquired for `grid` cannot call `other`. | Delete `call(route_name, method_name, data)`, `route_name=""`, `expected_crm_*=""`, and route-name plus expected-contract dual arguments. |
| Peer route-state ingress | Local relay registration and validated peer messages produce hash-bearing route-state wire wrappers. | `RelayState` and `RouteTable` mutate only from validated active-route/tombstone newtypes; raw wrappers remain outside storage. | `PeerEnvelope`, route announce/withdraw, digest exchange/diff, `FullSyncEnvelope`, seed join, seed retry, and `/_peer/sync` are version-gated before mutation. | No Python SDK authority exists for peer route state; PyO3/HTTP diagnostics project already-validated route fingerprints only. | Peer handler tests, background anti-entropy tests, seed-bootstrap tests, old-version deletion tests, and full-sync empty-snapshot rejection tests run against the same validator. | Remove raw `TryFrom<DigestDiffEntry> for RouteEntry`, raw `json::<FullSync>()`, raw `Serialize`/`Deserialize` on internal route structs, and `DefaultHasher` digests. |
| Stale-owner replacement | Final owner probe produces exactly one `OwnerReplacementEvidence`; connection slot produces owner generation and lease epoch. | `ConnectionPool` is the single mutable freshness authority; `OwnerReplacementToken` captures route identity plus slot token/generation/epoch. | Owner freshness is local-only and never appears in resolve responses, peer snapshots, digest material, or route gossip. | Python does not retry, replay, or authorize relay owner replacement. | Race tests cover renew-then-disconnect, active request during replacement, recovered owner, disabled idle timeout, and concurrent acquire/replace. | Delete token-only `can_replace_owner_token(...)`; replacement must be an atomic `replace_if_owner_token(...)` mutation under the captured slot lock. |
| Validation ownership | `c2-contract` defines route-name, call-route-key, reply route text, CRM-tag, hash, and expected-contract validators. | `c2-wire`, `c2-ipc`, `c2-server`, `c2-http`, `c2-runtime`, and PyO3 import or privately delegate to `c2-contract`. | Wire codecs reject empty call route keys through `validate_call_route_key(...)`; named route metadata uses `validate_named_route_name(...)`. | Python exposes typed facades and descriptor inputs but does not own language-neutral validators or wire limits. | SDK boundary tests scan for stale public `c2-wire` validation constants and relay-local validation logic. | Remove public `MAX_CALL_ROUTE_NAME_BYTES`, `MAX_HANDSHAKE_NAME_BYTES`, standalone public wire validators, and relay/Python duplicate validation tables. |

Every matrix row is all-or-nothing. For example, adding hashes to `ExpectedRouteContract` without registration producer hashes, route-bound native clients, relay resolve cache keys, and peer sync wrappers is still an open vulnerability, not a partial success.

## Task 1: Remove Python Compatibility Facades

**Files:**
- Modify `sdk/python/src/c_two/transport/server/native.py`
- Modify `sdk/python/src/c_two/transport/protocol.py`
- Modify `sdk/python/tests/unit/test_sdk_boundary.py`
- Modify `sdk/python/tests/unit/test_wire.py`
- Modify any tests found by `rg "NativeServerBridge\\(" sdk/python/tests`

- [x] **Step 1: Add failing clean-cut tests**

Add SDK boundary tests that assert `NativeServerBridge` no longer accepts constructor-time CRM registration:

```python
def test_native_server_bridge_constructor_has_no_crm_registration_compat():
    import inspect
    from c_two.transport.server.native import NativeServerBridge

    signature = inspect.signature(NativeServerBridge)
    for obsolete in ("crm_class", "crm_instance", "concurrency", "name"):
        assert obsolete not in signature.parameters

    source = inspect.getsource(NativeServerBridge.__init__)
    forbidden = [
        "Register initial CRM",
        "if crm_class is not None",
        "self.register_crm(",
        "_default_concurrency",
        "_default_name",
    ]
    offenders = [needle for needle in forbidden if needle in source]
    assert offenders == []
```

Add a wire test proving ignored decode keyword limits are gone:

```python
def test_decode_handshake_does_not_accept_ignored_limit_keywords():
    import inspect
    from c_two.transport.protocol import decode_handshake

    signature = inspect.signature(decode_handshake)
    assert list(signature.parameters) == ["payload"]

    encoded = encode_client_handshake([], CAP_CALL | CAP_METHOD_IDX)
    with pytest.raises(TypeError):
        decode_handshake(encoded, max_segments=1)
```

- [x] **Step 2: Run the focused failing tests**

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_sdk_boundary.py::test_native_server_bridge_constructor_has_no_crm_registration_compat sdk/python/tests/unit/test_wire.py::test_decode_handshake_does_not_accept_ignored_limit_keywords -q --timeout=30 -rs
```

Expected: tests fail because the compatibility signatures and wrapper arguments still exist.

- [x] **Step 3: Remove constructor-time CRM registration**

Change `NativeServerBridge.__init__()` to accept only server construction arguments:

```python
def __init__(
    self,
    bind_address: str,
    *,
    ipc_overrides: ServerIPCOverrides | Mapping[str, object] | None = None,
    server_id: str | None = None,
    server_instance_id: str | None = None,
    hold_warn_seconds: float = 60.0,
    lease_tracker: object | None = None,
) -> None:
    ...
```

Remove:

- `crm_class`
- `crm_instance`
- constructor `concurrency`
- constructor `name`
- `_default_concurrency`
- `_default_name`
- the `if crm_class is not None and crm_instance is not None` branch

Keep `register_crm(..., concurrency=...)` as the explicit registration path. Its fallback must be `concurrency or ConcurrencyConfig()`.

- [x] **Step 4: Migrate direct test users**

Find direct constructor users:

```bash
rg "NativeServerBridge\\(|Server\\(" sdk/python/tests sdk/python/src examples/python
```

For any low-level user that relied on constructor registration, rewrite it to:

```python
from c_two.transport.server.scheduler import ConcurrencyConfig

bridge = NativeServerBridge("ipc://test_server", ipc_overrides={})
bridge.register_crm(
    Grid,
    grid_instance,
    concurrency=ConcurrencyConfig(max_workers=1),
    name="grid",
)
```

For unit tests that manually construct `NativeServerBridge` with `object.__new__`, remove any `_default_concurrency` or `_default_name` fixture setup. Pass the desired `ConcurrencyConfig` directly into `register_crm(...)` instead.

Do not add a compatibility helper.

- [x] **Step 5: Remove ignored handshake wrapper arguments**

In `sdk/python/src/c_two/transport/protocol.py`:

- delete `_MAX_HANDSHAKE_SEGMENTS`, `_MAX_HANDSHAKE_ROUTES`, and `_MAX_HANDSHAKE_METHODS` unless another test imports them;
- change `decode_handshake(payload: bytes | memoryview) -> Handshake`;
- remove doc text saying keyword arguments are ignored for API compatibility.

- [x] **Step 6: Verify compatibility cleanup**

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_sdk_boundary.py sdk/python/tests/unit/test_wire.py sdk/python/tests/unit/test_runtime_session.py -q --timeout=30 -rs
```

Expected: focused Python tests pass, and no constructor-time registration path remains.

- [x] **Step 7: Commit Task 1**

```bash
git add sdk/python/src/c_two/transport/server/native.py sdk/python/src/c_two/transport/protocol.py sdk/python/tests/unit/test_sdk_boundary.py sdk/python/tests/unit/test_wire.py sdk/python/tests/unit/test_runtime_session.py
git commit -m "refactor: remove obsolete python transport compat facades"
```

## Task 2: Add Relay Owner Freshness And Fencing

**Files:**
- Modify `core/transport/c2-http/src/relay/conn_pool.rs`
- Modify `core/transport/c2-http/src/relay/route_table.rs`
- Modify `core/transport/c2-http/src/relay/types.rs`
- Modify `core/transport/c2-http/src/relay/authority.rs`
- Modify `core/transport/c2-http/src/relay/state.rs`
- Modify `core/transport/c2-http/src/relay/router.rs`
- Modify `core/transport/c2-http/src/relay/server.rs`
- Modify `docs/issues/relay-stale-owner-revival-race.md`

- [x] **Step 1: Add failing owner freshness tests**

Add state/authority tests that cover these cases:

- stale candidate commit is rejected after the old owner renews the relay-local lease;
- stale candidate commit is rejected after the old owner renews the relay-local lease and then disconnects before the candidate reaches final commit;
- stale candidate commit is rejected after the old owner re-registers and receives a new owner generation;
- stale candidate commit is rejected after the old owner lease expires if the final owner probe observes the old owner alive and still exporting the route;
- stale candidate commit is allowed after the old owner lease expires only when the candidate passes IPC identity plus route contract attestation and the final owner probe proves `ConfirmedDead` or `ConfirmedRouteMissing`;
- time-based owner lease expiry uses `RelayConfig.idle_timeout_secs` as a monotonic freshness deadline; a test relay with `idle_timeout_secs = 0` does not create a lease deadline and does not allow replacement merely because time advanced;
- system wall-clock jumps do not expire or extend owner leases; owner lease tests must use an injected monotonic clock or `Instant`-based test hooks;
- a test relay with `idle_timeout_secs = 0` still allows replacement when the final owner probe proves `ConfirmedDead` or `ConfirmedRouteMissing`;
- a stale candidate whose first probe saw `Dead` is rejected if a final probe sees the owner recover before commit;
- live owner replacement remains rejected before candidate IPC connect;
- owner-token pointer/address checks still reject relay-local replacement races.
- full snapshots, peer route announces, and anti-entropy digests do not change when only the local owner lease deadline is renewed;
- malformed peer snapshots that include owner-private freshness fields are rejected or scrubbed before route table mutation.

Use existing tests around `commit_register_upstream(...)` and `prepare_register(...)` in `core/transport/c2-http/src/relay/state.rs` as the fixture style. The new assertions must inspect `RegisterCommitResult::Duplicate` or `RegisterCommitResult::ConflictingOwner` rather than string-matching logs.

- [x] **Step 2: Run the failing relay tests**

```bash
cargo test --manifest-path core/Cargo.toml -p c2-http relay::state::tests::stale_owner --features relay -- --nocapture
```

Expected: at least the lease-renewal and generation-change cases fail because `OwnerToken` currently captures only slot pointer and address.

- [x] **Step 3: Add owner generation and lease metadata**

In `conn_pool.rs`:

- add a monotonic owner generation counter to `ConnectionPool`;
- add `owner_generation: u64`, `owner_lease_epoch: u64`, and `owner_lease_deadline: Option<std::time::Instant>` to `SlotInner`;
- include `owner_generation` and `owner_lease_epoch` in a pool-owned `OwnerSlotToken`; carry replacement evidence as a separate final-commit argument produced by `authority.rs`;
- assemble the public relay replacement token in `state.rs` by combining the captured `RouteEntry` owner identity (`route_name`, `server_id`, `server_instance_id`, `ipc_address`) with the `OwnerSlotToken`; this state-level token is the only object final commit may consume;
- increment generation when inserting a replacement slot, reconnecting an owner as authoritative, or re-registering a same route under a different `server_instance_id`;
- initialize or increment lease epoch whenever a slot becomes the authoritative owner for a route;
- add a method that renews the current owner lease only when the route and owner token still match; successful renewal must increment `owner_lease_epoch` and extend `owner_lease_deadline` with a monotonic `Instant`.

Derive the monotonic lease duration from `state.config().idle_timeout_secs`:

```rust
fn owner_lease_duration(config: &RelayConfig) -> Option<std::time::Duration> {
    match config.idle_timeout_secs {
        0 => None,
        seconds => Some(std::time::Duration::from_secs(seconds)),
    }
}
```

Do not add `C2_RELAY_OWNER_LEASE_*` or any SDK-level owner lease setting. The relay server already owns `C2_RELAY_IDLE_TIMEOUT`; reusing that Rust config keeps ownership in the relay runtime and avoids a second control-plane lifetime.

Define replacement evidence explicitly so the final commit does not conflate time-based expiry with proven owner loss. Keep the module boundary clean: `conn_pool.rs` must not import or mention `OwnerProbe`, because `OwnerProbe` is authority/probing policy. `authority.rs` performs the final probe and maps it to a neutral replacement evidence value before asking the pool to validate slot state.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerReplacementEvidence {
    ConfirmedDead,
    ConfirmedRouteMissing,
}

#[derive(Debug, Clone)]
pub struct OwnerSlotToken {
    slot: ConnectionSlot,
    address: String,
    owner_generation: u64,
    owner_lease_epoch: u64,
}
```

In `state.rs`, define the commit token that carries both route-table identity and pool freshness. This avoids keeping two mutable owner-freshness authorities:

```rust
#[derive(Debug, Clone)]
pub struct OwnerReplacementToken {
    route_name: String,
    server_id: String,
    server_instance_id: String,
    ipc_address: String,
    slot_token: OwnerSlotToken,
}
```

The slot-token check in `conn_pool.rs` must preserve the existing slot-state replaceability guard and must never treat lease expiry as replacement evidence. `ConfirmedDead` and `ConfirmedRouteMissing` are already-authorized facts from `authority.rs`; the pool only verifies that the captured slot is still the current owner slot, no request is active, and the locked slot is replaceable. Do not evict the old client before the candidate is fully attested and ready to commit. Do not keep a public or state-level `can_replace_owner_token(...)` API after this task: a boolean pre-check is not a commit fence and can be reused without final-probe evidence. The only production mutation API for different-owner replacement is the atomic `replace_if_owner_token(...)` operation in Step 5.

```rust
fn replacement_evidence_allows_locked(
    inner: &SlotInner,
    token: &OwnerSlotToken,
    evidence: OwnerReplacementEvidence,
) -> bool {
    if inner.address != token.address
        || inner.owner_generation != token.owner_generation
        || inner.owner_lease_epoch != token.owner_lease_epoch
    {
        return false;
    }

    match evidence {
        OwnerReplacementEvidence::ConfirmedDead
        | OwnerReplacementEvidence::ConfirmedRouteMissing => {
            inner.active_requests == 0 && inner.state.is_replaceable()
        }
    }
}
```

`lease_is_expired(..., None)` must return `false` for renewal/introspection tests, but no replacement predicate may call it as authorization. The exact method names may follow the surrounding code, but the slot predicate must include pointer, address, generation, lease epoch, `active_requests == 0`, locked-slot replaceability, and exactly one final-probe replacement evidence. The state-level commit predicate must also compare the current `RouteEntry` against `route_name`, `server_id`, `server_instance_id`, and `ipc_address` captured in `OwnerReplacementToken`.

- [x] **Step 4: Keep freshness out of peer-serialized route state**

Do not add `owner_generation` or `owner_lease_deadline` as serde-visible fields on `RouteEntry`. `RouteEntry` is used by full snapshots and peer repair paths, so owner freshness must stay outside the peer-serialized route payload.

Do not add a mutable `LocalOwnerFreshness` map in `state.rs`. `RelayState` may expose helper methods to capture `OwnerReplacementToken` and to renew the owner lease, but those helpers must update and validate the connection slot as the single freshness authority while holding the existing lock order.

Rules:

- Capture `OwnerReplacementToken` only while holding relay authority according to the existing lock order: route table first, then connection pool.
- `RouteEntry` must remain limited to fields that are valid for route selection and peer propagation.
- `RouteInfo::to_route_info()`, full snapshot generation, route announce messages, and anti-entropy digest entries must not include lease internals.
- `route_table::merge_snapshot(...)` must continue to scrub owner-private fields from peer entries. If the serde shape is extended in the future, add a regression test that proves peer inputs cannot poison local owner freshness.
- Lease renewal may update only the connection slot by incrementing `owner_lease_epoch` and extending `owner_lease_deadline` with a monotonic `Instant`; it must not mutate peer-visible route timestamps, digest material, or any separate freshness cache.
- A same-owner re-register or lease renewal before candidate commit must invalidate an already captured replacement token through either `owner_generation` or `owner_lease_epoch`, even when the old owner disconnects before the candidate commits.

- [x] **Step 5: Fence final replacement commit**

In `authority.rs`, replacement commits must not rely on a route-table write lock as the only fence. The current `acquire_upstream(...)` shape reads the route under a short route-table read lock, releases it, and only then enters `ConnectionPool::acquire_with(...)`. A request can therefore already hold an `Arc<UpstreamSlot>` before final commit obtains route-table write authority. The fix is a pool-owned atomic replacement API that retires the captured slot under the slot lock.

Add a replacement operation in `conn_pool.rs` instead of using `can_replace_owner_token(...)` followed by the existing `insert(...)` path:

```rust
pub enum OwnerReplaceError {
    StaleToken,
    NotReplaceable,
}

pub fn replace_if_owner_token(
    &self,
    name: &str,
    token: &OwnerSlotToken,
    new_address: String,
    new_client: Arc<IpcClient>,
    evidence: OwnerReplacementEvidence,
) -> Result<Option<Arc<IpcClient>>, OwnerReplaceError> {
    let mut entries = self.entries.lock();
    let Some(current) = entries.get(name).cloned() else {
        return Err(OwnerReplaceError::StaleToken);
    };
    if !Arc::ptr_eq(&current, &token.slot) {
        return Err(OwnerReplaceError::StaleToken);
    }

    let old_client = {
        let mut inner = current.inner.lock();
        if inner.address != token.address
            || inner.owner_generation != token.owner_generation
            || inner.owner_lease_epoch != token.owner_lease_epoch
        {
            return Err(OwnerReplaceError::StaleToken);
        }
        if !replacement_evidence_allows_locked(&inner, evidence) {
            return Err(OwnerReplaceError::NotReplaceable);
        }
        inner.state = SlotState::Retired;
        inner.client.take()
    };

    entries.insert(
        name.to_string(),
        Arc::new(UpstreamSlot::new(new_address, Some(new_client), SlotState::Ready)),
    );
    current.notify.notify_waiters();
    Ok(old_client)
}
```

`replacement_evidence_allows_locked(...)` must check evidence while holding the old slot lock:

- There is no `LeaseExpired` branch. Lease expiry may invalidate stale tokens or trigger a new probe, but it is not replacement authorization.
- For `ConfirmedDead`, repeat or consume a just-completed owner probe result that failed to connect to the existing owner endpoint, then require the locked slot to satisfy `active_requests == 0` plus the normal replaceability guard.
- For `ConfirmedRouteMissing`, repeat or consume a just-completed owner probe result that proved the existing owner endpoint responded but did not expose the route, then require the locked slot to satisfy `active_requests == 0` plus the normal replaceability guard.
- For all evidence types, compare `route_name`, `server_id`, `server_instance_id`, `ipc_address`, `owner_generation`, and `owner_lease_epoch` from the captured `OwnerReplacementToken` against the current `RouteEntry` and locked current slot immediately before mutation.
- If a final probe observes `Alive`, `Stale`, or any route with a changed owner generation or lease epoch, return duplicate-owner semantics.
- After the final probe selects `ConfirmedDead` or `ConfirmedRouteMissing`, pass that neutral evidence into `conn_pool.rs`; do not pass `OwnerProbe` itself across the module boundary.

The final route-table mutation and pool replacement must be ordered so a rejected candidate cannot strand the old owner:

1. Hold the existing route-table write lock.
2. Build the candidate `RouteEntry` and validate that the exact entry can replace the current route, including tombstone timestamp ordering, before touching the connection slot.
3. Call `replace_if_owner_token(...)` while still holding the route-table write lock. This retires the old slot under its slot lock and installs the candidate slot in the pool. An acquire that cloned the old slot before this point either already incremented `active_requests` and makes the replacement fail, or observes `Retired` and cannot dispatch.
4. Insert the prevalidated route entry with an infallible or debug-asserted route-table update path. Do not call the current boolean `register_route(...)` after retiring the old slot unless a failure leaves the old slot intact.
5. Drop locks, then asynchronously close the returned old client. Never await close while holding the route-table lock, connection-map lock, or slot lock.

If the route-table update cannot be made infallible for a prevalidated entry, use a guard-based two-phase pool API instead: `begin_replace_if_owner_token(...)` marks the old slot `Replacing` under the slot lock and returns a rollback guard; route-table registration runs while the guard is held; `guard.commit(new_address, new_client)` swaps the map and retires the old client only after route-table mutation succeeds; dropping the guard before commit restores the old slot state and wakes waiters. Do not ship an implementation where route-table failure after pool replacement can leave no valid owner for the route.

If the current route owner is different from the candidate owner and the replacement token is stale, return `ControlError::DuplicateRoute { existing_address }`.

Do not close, evict, or retire the old cached client during preflight or during failed candidate attestation. Do not make this a retry loop inside Python or the SDK.

- [x] **Step 6: Renew lease on relay-local owner activity**

Renew the owner lease when:

- the owner successfully registers as `SameOwner`;
- the relay successfully acquires or uses the current upstream client for a CRM call and the lease still matches the current local route identity;
- a command-loop registration confirms the same `server_id`, `server_instance_id`, and address;
- an explicit unregister removes the active route, connection slot, and local owner freshness.

Do not renew lease for failed IPC probes, failed candidate attestation, peer route gossip, or unregister. Unregister must preserve the route tombstone until the existing tombstone GC policy removes it; deleting the tombstone during unregister would allow stale peer announcements to resurrect deleted routes.

- [x] **Step 7: Update HTTP and command-loop registration paths**

Ensure both paths carry the same `OwnerReplacementToken` data:

- HTTP `/_register` in `router.rs`
- command-loop upstream registration in `server.rs`

Both paths must preserve the ordering:

```text
prepare with current route owner
  -> attest candidate IPC identity and CRM contract
  -> refresh replacement freshness
  -> final commit under route authority
  -> broadcast route announce only after successful commit
```

When Task 3 adds route fingerprints, the attestation step must be upgraded in place to attest the candidate route's IPC handshake ABI and signature hashes before commit. Do not let HTTP body claims or command-loop arguments become the authority for route fingerprint metadata.

- [x] **Step 8: Update stale-owner issue document**

In `docs/issues/relay-stale-owner-revival-race.md`, add a resolution section describing the chosen relay-local owner contract:

- relay ownership is relay-local generation plus lease;
- owner freshness is not part of peer snapshot, anti-entropy, or normal route resolution state;
- silent external recovery preserves relay ownership when the final owner probe reaches the old endpoint and observes the route; lease expiry alone is never an ownership-forfeiture event;
- re-registration or lease renewal before candidate commit prevents replacement.

- [x] **Step 9: Verify relay owner freshness**

```bash
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay relay:: -- --nocapture
```

Expected: all relay state, authority, and router tests pass.

Also run the freshness-shape guardrail so the implementation cannot regress to a time-only replacement proof:

```bash
python - <<'PY'
import ast
import re
from pathlib import Path

def rust_fn_body(text: str, name: str) -> str:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return ""
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

paths = [
    Path("core/transport/c2-http/src/relay/conn_pool.rs"),
    Path("core/transport/c2-http/src/relay/authority.rs"),
    Path("core/transport/c2-http/src/relay/state.rs"),
]
combined = "\n".join(path.read_text() for path in paths)
for forbidden in ("LeaseExpired", "lease_deadline_ms", "owner_lease_timeout_ms"):
    if forbidden in combined:
        raise SystemExit(f"relay owner freshness must not use {forbidden}")

conn_pool = paths[0].read_text()
if "owner_lease_deadline" not in conn_pool or "Instant" not in conn_pool:
    raise SystemExit("conn_pool owner lease deadline must be monotonic Instant-based")
if re.search(r"owner_lease_deadline[^;\n]*(now_millis|SystemTime)", conn_pool):
    raise SystemExit("owner lease deadline must not be derived from wall-clock time")
if "OwnerReplacementEvidence" not in combined:
    raise SystemExit("relay stale-owner commit must use explicit OwnerReplacementEvidence")
if "ConfirmedDead" not in combined or "ConfirmedRouteMissing" not in combined:
    raise SystemExit("replacement evidence must be limited to final-probe ConfirmedDead or ConfirmedRouteMissing")

if re.search(r"pub\s+fn\s+can_replace_owner_token\s*\(", conn_pool):
    raise SystemExit("conn_pool must not expose a public token-only replacement pre-check; use atomic replace_if_owner_token")
if ".can_replace_owner_token(" in combined or "can_replace_owner_token(" in paths[1].read_text() or "can_replace_owner_token(" in paths[2].read_text():
    raise SystemExit("authority/state must not authorize replacement through token-only can_replace_owner_token")

replace_body = rust_fn_body(conn_pool, "replace_if_owner_token")
for required in (
    "OwnerReplacementEvidence",
    "owner_generation",
    "owner_lease_epoch",
    "active_requests",
    "Retired",
):
    if required not in replace_body:
        raise SystemExit(f"replace_if_owner_token() must check/use {required}")
if "Arc::ptr_eq" not in replace_body and "ptr_eq" not in replace_body:
    raise SystemExit("replace_if_owner_token() must compare the captured slot pointer under the map lock")
if "replacement_evidence_allows_locked" not in replace_body:
    raise SystemExit("replace_if_owner_token() must consume final-probe evidence while holding the old slot lock")

authority = paths[1].read_text()
prepare_body = rust_fn_body(authority, "prepare_register")
register_local_body = rust_fn_body(authority, "register_local")
for required in ("OwnerProbe::Dead", "ConfirmedDead", "OwnerProbe::RouteMissing", "ConfirmedRouteMissing"):
    if required not in prepare_body and required not in register_local_body:
        raise SystemExit(f"authority final replacement path must bind {required} to replacement evidence")
if "replace_if_owner_token" not in register_local_body:
    raise SystemExit("authority register_local() must commit replacement through conn_pool::replace_if_owner_token")
if "OwnerReplacementEvidence" not in register_local_body:
    raise SystemExit("authority register_local() must consume explicit final-probe OwnerReplacementEvidence")
PY
```

- [x] **Step 10: Commit Task 2**

```bash
git add core/transport/c2-http/src/relay docs/issues/relay-stale-owner-revival-race.md
git commit -m "fix(relay): fence stale owner replacement with freshness"
```

## Task 3: Add Contract ABI/Signature Fingerprints And Relay Route-State Validation

**Files:**
- Modify `core/protocol/c2-wire/src/control.rs`
- Modify `core/protocol/c2-wire/src/handshake.rs`
- Modify `core/Cargo.toml`
- Add `core/foundation/c2-contract/Cargo.toml`
- Add `core/foundation/c2-contract/src/lib.rs`
- Modify `core/protocol/c2-wire/Cargo.toml`
- Modify `core/protocol/c2-wire/src/tests.rs`
- Modify `sdk/python/native/src/wire_ffi.rs`
- Modify `sdk/python/native/src/server_ffi.rs`
- Modify `core/transport/c2-server/Cargo.toml`
- Modify `core/transport/c2-server/src/dispatcher.rs`
- Modify `core/transport/c2-server/src/server.rs`
- Modify `core/transport/c2-ipc/src/client.rs`
- Modify `core/transport/c2-ipc/src/sync_client.rs`
- Modify `core/transport/c2-ipc/src/tests.rs`
- Modify `core/transport/c2-ipc/Cargo.toml`
- Modify `core/runtime/c2-runtime/src/session.rs`
- Modify `core/runtime/c2-runtime/Cargo.toml`
- Modify `core/transport/c2-http/src/client/control.rs`
- Modify `core/transport/c2-http/src/client/client.rs`
- Modify `core/transport/c2-http/src/client/relay_aware.rs`
- Modify `core/transport/c2-http/Cargo.toml`
- Modify `core/transport/c2-http/src/relay/types.rs`
- Modify `core/transport/c2-http/src/relay/authority.rs`
- Modify `core/transport/c2-http/src/relay/route_table.rs`
- Modify `core/transport/c2-http/src/relay/router.rs`
- Modify `core/transport/c2-http/src/relay/state.rs`
- Modify `core/transport/c2-http/src/relay/server.rs`
- Modify `core/transport/c2-http/src/relay/test_support.rs`
- Modify `core/transport/c2-http/src/relay/peer.rs`
- Modify `core/transport/c2-http/src/relay/gossip.rs`
- Modify `core/transport/c2-http/src/relay/peer_handlers.rs`
- Modify `core/transport/c2-http/src/relay/disseminator.rs`
- Modify `core/transport/c2-http/src/relay/background.rs`
- Modify `sdk/python/native/src/client_ffi.rs`
- Modify `sdk/python/native/src/runtime_session_ffi.rs`
- Modify `sdk/python/native/src/http_ffi.rs`
- Modify `sdk/python/native/Cargo.toml`
- Modify `sdk/python/tests/unit/test_sdk_boundary.py`
- Modify `sdk/python/tests/unit/test_crm_proxy.py`
- Modify `sdk/python/src/c_two/crm/transferable.py`
- Modify `sdk/python/tests/fixtures/ihello.py`
- Modify `examples/python/grid/transferables.py`
- Add `sdk/python/src/c_two/crm/methods.py`
- Add `sdk/python/src/c_two/crm/descriptor.py`
- Modify `sdk/python/src/c_two/transport/server/native.py`
- Modify `sdk/python/src/c_two/transport/registry.py`
- Add `sdk/python/tests/unit/test_crm_descriptor.py`
- Modify `sdk/python/tests/unit/test_wire.py`
- Modify `sdk/python/tests/integration/test_ipc_buddy_reply.py`
- Modify `sdk/python/tests/integration/test_http_relay.py`
- Modify `sdk/python/tests/integration/test_direct_ipc_contract_validation.py`
- Modify or add relay integration coverage under `sdk/python/tests/integration/`

- [x] **Step 1: Add failing wire hash tests**

Extend `RouteInfo` tests in `c2-wire` and Python to require:

- `abi_hash` and `signature_hash` are present on server route metadata;
- hashes round-trip through the server handshake;
- malformed hash strings are rejected on encode and decode;
- `HANDSHAKE_VERSION` is bumped from `9` to `10` and a payload encoded with the old route layout/version is rejected by the strict decoder;
- a current-version route cannot be encoded or decoded when either hash field is missing or malformed;
- `encode_call_control("", method_idx)`, `encode_call_control_into(..., "", method_idx)`, and `decode_call_control(...)` reject the empty route key; `validate_call_route_key(...)` and `validate_named_route_name(...)` both reject empty strings;
- mismatched expected hashes cause route validation failure before method dispatch;
- low-level unnamed admin probes may omit expected hashes, but named route validation rejects missing expected hashes once a route name is supplied.

Use lowercase SHA-256 hex fixtures:

```rust
const ABI_HASH: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SIG_HASH: &str =
    "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
```

- [x] **Step 2: Run failing wire tests**

```bash
cargo test --manifest-path core/Cargo.toml -p c2-wire route_hash -- --nocapture
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_wire.py -q --timeout=30 -rs
```

Expected: failures because `RouteInfo` does not yet carry hash fields.

- [x] **Step 3: Extend Rust handshake route metadata**

In `c2-wire`, extend `RouteInfo`:

```rust
pub struct RouteInfo {
    pub name: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub methods: Vec<MethodEntry>,
}
```

Add `validate_contract_hash(label, value)`:

- non-empty;
- exactly 64 bytes;
- lowercase ASCII hex;
- no whitespace or control characters.

Bump `HANDSHAKE_VERSION` in `core/protocol/c2-wire/src/handshake.rs` from `9` to `10` because the route entry binary layout changes. If another branch has already consumed version `10` before implementation, use the next unused version and update every assertion in the same task. Keep encode/decode symmetric, and add tests that:

- a v9 server-handshake payload using the old route layout is rejected by the strict version check;
- a current-version route with missing hash fields cannot be encoded;
- a current-version route payload with malformed hash fields cannot be decoded into `RouteInfo`.

Because C-Two is in 0.x, do not add dual-layout compatibility unless a test explicitly proves mixed-version support is still required. `c2-ipc`, `c2-server`, `c2-runtime`, and the Python native bindings must compile against the same `HANDSHAKE_VERSION` constant; do not duplicate the version number in SDK code.

- [x] **Step 4: Expose hash fields through PyO3**

In `wire_ffi.rs`, update the Python `RouteInfo` pyclass constructor and fields. The constructor must require `abi_hash` and `signature_hash` explicitly and delegate validation to Rust.

Update `sdk/python/tests/unit/test_wire.py` helpers so all routes include explicit hashes.

- [x] **Step 5: Add canonical contract hashing and expected-contract validation in Rust core**

Create a new foundation crate, `core/foundation/c2-contract`, and add it to
the workspace in `core/Cargo.toml`. This crate owns the route-contract type and
hashing helpers because `c2-http` client code is available with default features
disabled and must not need a relay-only or wire-codec dependency just to validate
expected route identity.

`c2-contract` must expose:

- `ExpectedRouteContract`, with route name, CRM namespace/name/version, ABI hash,
  and signature hash;
- shared text-size constants for route names and contract text fields. The
  current wire representation is one-byte length-prefixed, so these constants
  are `u8::MAX as usize` unless Task 3 intentionally changes the wire layout;
- `validate_named_route_name(label, value)` for registered CRM routes, relay
  route state, and `ExpectedRouteContract`; this rejects empty names;
- `validate_call_route_key(label, value)` for call-control route fields; this
  rejects the empty string because `name_len=0` is invalid, and otherwise
  applies the same length/control-character rules as named routes;
- `validate_contract_text_field(label, value)` and `validate_crm_tag(...)` for
  CRM namespace/name/version fields;
- `validate_contract_hash(label, value)`;
- `validate_expected_route_contract(expected)`;
- `contract_descriptor_sha256_hex(json_bytes)`.

Add only lightweight dependencies such as `serde_json`, `sha2`, and `thiserror`
to `c2-contract`. It must not depend on `c2-wire`, `c2-http`, `c2-ipc`, Tokio,
reqwest, Axum, or relay modules. `c2-wire`, `c2-ipc`, `c2-http`, `c2-runtime`,
and `sdk/python/native` must depend on this single crate instead of duplicating
equivalent validation structs or literal field-limit constants.

Update `core/protocol/c2-wire/src/control.rs` and
`core/protocol/c2-wire/src/handshake.rs` in the same step. Remove public wire
compatibility constants and validation wrappers from the API surface; update
all call sites to use `c2-contract` directly. `c2-wire` may keep private codec
helpers only when they call `c2-contract` internally. It must not keep
definitions such as `pub const MAX_CALL_ROUTE_NAME_BYTES: usize = u8::MAX as
usize`, independent `MAX_HANDSHAKE_NAME_BYTES` literals, public
`validate_name_len(...)`, `validate_crm_tag(...)`,
`validate_crm_tag_field(...)` wrappers, or route hash validators. `control.rs`
must call `validate_call_route_key(...)` from `encode_call_control(...)`,
`encode_call_control_into(...)`, and `decode_call_control(...)` so empty route
keys and invalid incoming route text are rejected before dispatch. It must also
validate the route text carried by
`ReplyControl::RouteNotFound` in `try_encode_reply_control(...)` and
`decode_reply_control(...)`; reply metadata is not a dispatch authority, but it
is still wire-visible route text and must not keep a weaker length or
control-character policy. Replace the old infallible Rust reply encoder on all
production paths with `try_encode_reply_control(...) -> Result<Vec<u8>,
EncodeError>`. `core/transport/c2-server/src/server.rs` reply writers must call
the fallible helper directly and propagate the error. `core/transport/c2-ipc/src/tests.rs`
must assert the invalid-route error path. `sdk/python/native/src/wire_ffi.rs`
may keep a Python-visible facade named `encode_reply_control`, but that facade
must call `try_encode_reply_control(...)` and convert the error to `PyResult`.
Do not use `unwrap()`, `expect()`, or ignored `let _ = ...` validation on an
attacker-controlled route string. The server, relay authority, IPC clients, and
runtime session must all call the same validators through `c2-contract`, so a
later rule change cannot diverge by crate.

Update relay helpers in `route_table.rs` and `authority.rs` at the same time:
`valid_route_name(...)`, `valid_crm_tag(...)`, and any server-id wire length
checks should be removed in favor of direct `c2-contract` / `c2-config` calls
where the call site remains readable. If a private helper remains because it
reduces repeated error mapping, its body must be a thin wrapper over those
crates. Relay route-table validation must use `validate_named_route_name(...)`,
not `validate_call_route_key(...)`, because relay route metadata must never
publish an empty route name. Do not leave relay-local CRM tag validation copied
from the old wire crate.

Update `sdk/python/tests/unit/test_sdk_boundary.py` in the same step. Replace
`test_relay_does_not_keep_second_crm_tag_validator()` so it asserts
`route_table.rs` / `authority.rs` reference `c2_contract` and do not reference
`c2_wire::handshake::validate_crm_tag`, `MAX_HANDSHAKE_NAME_BYTES`, or
`MAX_CALL_ROUTE_NAME_BYTES`.

```rust
pub fn contract_descriptor_sha256_hex(json_bytes: &[u8]) -> Result<String, ContractError>
```

The function must parse JSON, canonicalize object key order recursively, serialize without insignificant whitespace, and return lowercase SHA-256 hex.

Expose this through `sdk/python/native/src/wire_ffi.rs` as a PyO3 function. Use
Rust for the hash so future SDKs can reuse the same canonical function. Python
must not use a separate ad hoc hash implementation as the authority. The PyO3 function must accept bytes-like JSON input and may accept `str` by UTF-8 encoding it before calling the Rust function. It must not accept arbitrary Python `dict` objects directly, because that would either fail at runtime or move canonical JSON decisions into Python.

Keep `cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features`
green after this step. If adding `c2-contract` to `c2-http` breaks a
client-only build, the crate is too heavy or has the wrong dependency direction
and must be corrected before moving on.

- [x] **Step 6: Add explicit transferable ABI declarations**

Update `sdk/python/src/c_two/crm/transferable.py` so custom byte formats can declare stable compatibility metadata without changing serialization behavior:

First pin the built-in pickle fallback to an explicit protocol. This is part of the ABI for generated `Default*Transferable` classes and must be used by both input and output default serializers:

```python
DEFAULT_PICKLE_PROTOCOL = 4


def _pickle_dumps_default(value: object) -> bytes:
    return pickle.dumps(value, protocol=DEFAULT_PICKLE_PROTOCOL)
```

Replace default fallback calls such as `pickle.dumps(args[0])` and `pickle.dumps(args)` with `_pickle_dumps_default(...)`. Do not use `pickle.DEFAULT_PROTOCOL` or `pickle.HIGHEST_PROTOCOL` in the descriptor path, because those values can drift across Python releases while the route hash remains unchanged.

```python
def transferable(
    cls=None,
    *,
    abi_id: str | None = None,
    abi_schema: str | None = None,
):
    """Decorator for transferable classes.

    `abi_id` is a stable human-assigned identifier for custom byte formats.
    `abi_schema` is a stable schema string when a custom format needs richer
    compatibility metadata.
    """
    if abi_id is not None and not isinstance(abi_id, str):
        raise TypeError("transferable abi_id must be a string")
    if abi_schema is not None and not isinstance(abi_schema, str):
        raise TypeError("transferable abi_schema must be a string")
    if abi_id is not None and abi_schema is not None:
        raise ValueError(
            "transferable abi_id and abi_schema are mutually exclusive"
        )
    if abi_id is not None and abi_id.strip() == "":
        raise ValueError("transferable abi_id must not be empty")
    if abi_schema is not None and abi_schema.strip() == "":
        raise ValueError("transferable abi_schema must not be empty")

    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls.__module__ = cls.__module__
        new_cls.__qualname__ = cls.__qualname__
        new_cls.__cc_transferable_abi__ = {
            "abi_id": abi_id,
            "abi_schema": abi_schema,
        }
        return new_cls

    if cls is not None:
        return wrap(cls)
    return wrap
```

Preserve these decorator forms:

```python
import json

# This form remains valid as Python class decoration, but descriptor building
# must reject PlainData when it appears in a remotely callable CRM signature
# until a concrete generated dataclass-field wire codec exists.
@cc.transferable
class PlainData:
    value: int

@cc.transferable()
class PlainDataWithCall:
    value: int

@cc.transferable(abi_id="c-two.tests.HelloData.v1")
class HelloData:
    name: str
    value: int

    def serialize(data: "HelloData") -> bytes:
        return json.dumps({"name": data.name, "value": data.value}).encode()

    def deserialize(data_bytes: bytes) -> "HelloData":
        fields = json.loads(data_bytes.decode())
        return HelloData(name=fields["name"], value=fields["value"])
```

Add tests in `sdk/python/tests/unit/test_crm_descriptor.py` proving:

- plain dataclass-field transferables with no concrete SDK-owned serialization codec are rejected when used in a remotely callable CRM signature, with an error that says generated dataclass-field transferables are not yet a wire ABI;
- dynamically generated `Default*Transferable` pickle fallbacks need no user ABI id and produce a descriptor entry with `serializer_family="python-pickle-default"` and `serializer_version="pickle-protocol-4"`;
- default fallback serialization calls `pickle.dumps(..., protocol=DEFAULT_PICKLE_PROTOCOL)` for both one-argument and multi-argument paths;
- custom hooks without `abi_id` or `abi_schema` are rejected by descriptor building;
- custom hooks with `abi_id` are accepted and the id participates in the ABI hash;
- custom hooks with `abi_schema` are accepted and the schema participates in the ABI hash;
- empty or non-string ABI declarations fail at decoration time;
- a declaration with both `abi_id` and `abi_schema` fails at decoration time so every custom transferable has exactly one ABI identity source.

Migrate existing custom hook users. This is not limited to fixtures; run the scan before and after migration so no remote CRM signature still references a custom-hook transferable without an ABI declaration:

- `sdk/python/tests/fixtures/ihello.py` must use stable test ABI ids for `HelloData` and `HelloItems`;
- `sdk/python/tests/integration/test_buffer_lease_ipc.py` must use stable test ABI ids for `BytesView`, `BytesDeserializeOnly`, `BadInputFromBuffer`, and `BadOutputFromBuffer`;
- `sdk/python/tests/integration/test_response_allocation_ipc.py` must use stable test ABI ids for `BytesResponse` and `MemoryViewResponse`;
- `sdk/python/tests/integration/test_remote_scheduler_config.py` must use stable test ABI ids for `Delay`, `Window`, `Count`, and `LargePayload`;
- `examples/python/grid/transferables.py` must declare ABI schema strings for the Arrow table layouts used by `GridSchema` and `GridAttribute`;
- `sdk/python/benchmarks/thread_vs_ipc_benchmark.py`, `sdk/python/benchmarks/unified_numpy_benchmark.py`, and `sdk/python/benchmarks/kostya_ctwo_benchmark.py` are runnable remote CRM surfaces, not disposable test-only code. Add benchmark ABI declarations for `Payload`, `NpPayload`, `NpStructured`, `CoordRecords`, `CoordArrays`, and conditional `CoordFastdb`. For performance-sensitive benchmark pickle hooks, do not silently change the measured byte format; either keep the current protocol and include the evaluated protocol number in `abi_schema`, or pin a benchmark-local protocol constant and include that constant in the ABI schema. Raw NumPy / fastdb buffer schemas must include dtype, byte order, row layout, and any shape assumptions.
- Unit tests that define `@cc.crm` contracts with custom-hook transferables must not silently bypass the remote-surface gate. Add stable test ABI ids for positive tests in `sdk/python/tests/unit/test_transfer_decorator.py` and `sdk/python/tests/unit/test_transferable.py`, or move the no-ABI variants into `sdk/python/tests/unit/test_crm_descriptor.py` as explicit negative descriptor tests that assert registration/descriptor construction fails before route publication.

Use this repository scan as the guardrail:

```bash
python - <<'PY'
import ast
from pathlib import Path

roots = [
    Path("sdk/python/tests"),
    Path("sdk/python/benchmarks"),
    Path("examples/python"),
]
for root in roots:
    for path in root.rglob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for cls in [n for n in ast.walk(tree) if isinstance(n, ast.ClassDef)]:
            decorators = [ast.unparse(d) for d in cls.decorator_list]
            if not any("transferable" in d for d in decorators):
                continue
            own_methods = {n.name for n in cls.body if isinstance(n, ast.FunctionDef)}
            has_custom_hook = bool({"serialize", "deserialize"} & own_methods)
            has_abi_id = any("abi_id" in d for d in decorators)
            has_abi_schema = any("abi_schema" in d for d in decorators)
            if has_abi_id and has_abi_schema:
                print(f"{path}:{cls.lineno}:{cls.name}: both abi_id and abi_schema")
                continue
            has_abi = has_abi_id or has_abi_schema
            if has_custom_hook and not has_abi:
                print(f"{path}:{cls.lineno}:{cls.name}")
PY
```

Expected after migration: output may still include unit-only decorator fixtures, but it must not include `sdk/python/tests/fixtures/ihello.py`, `sdk/python/tests/integration/test_buffer_lease_ipc.py`, `sdk/python/tests/integration/test_response_allocation_ipc.py`, `sdk/python/tests/integration/test_remote_scheduler_config.py`, `sdk/python/benchmarks/thread_vs_ipc_benchmark.py`, `sdk/python/benchmarks/unified_numpy_benchmark.py`, `sdk/python/benchmarks/kostya_ctwo_benchmark.py`, or `examples/python/grid/transferables.py`. Unit-only transferables that intentionally test decorator behavior may remain without ABI only if descriptor tests prove they are rejected when used in remote signatures.

The class-level scan above is not sufficient on its own, because it cannot prove whether a leftover no-ABI transferable is reachable from a remote CRM signature. Add a second, path-aware scan that walks CRM methods and `@cc.transfer(input=..., output=...)` metadata, then fails on reachable custom-hook transferables without ABI declarations. The scan must avoid simple class-name overwrites, because tests and examples can reuse class names across files:

```bash
python - <<'PY'
import ast
from pathlib import Path

roots = [
    Path("sdk/python/tests"),
    Path("sdk/python/benchmarks"),
    Path("examples/python"),
]
negative_descriptor_fixture_paths = {
    Path("sdk/python/tests/unit/test_crm_descriptor.py"),
}
classes: list[tuple[Path, ast.ClassDef]] = []
custom_without_abi: dict[str, list[tuple[Path, ast.ClassDef]]] = {}
custom_without_abi_by_file: dict[tuple[Path, str], list[tuple[Path, ast.ClassDef]]] = {}


def decorator_text(node: ast.AST) -> str:
    return ast.unparse(node)


def annotation_names(node: ast.AST | None) -> set[str]:
    if node is None:
        return set()
    if isinstance(node, ast.Name):
        return {node.id}
    if isinstance(node, ast.Attribute):
        return {node.attr}
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        try:
            return annotation_names(ast.parse(node.value, mode="eval").body)
        except SyntaxError:
            return set()
    if isinstance(node, ast.Subscript):
        return annotation_names(node.value) | annotation_names(node.slice)
    if isinstance(node, ast.Tuple):
        out = set()
        for item in node.elts:
            out |= annotation_names(item)
        return out
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        return annotation_names(node.left) | annotation_names(node.right)
    return set()


def transfer_kw_names(decorator: ast.AST) -> set[str]:
    if not isinstance(decorator, ast.Call):
        return set()
    text = decorator_text(decorator.func)
    if not text.endswith("transfer"):
        return set()
    out = set()
    for keyword in decorator.keywords:
        if keyword.arg in {"input", "output"}:
            out |= annotation_names(keyword.value)
    return out


def is_crm_class(cls: ast.ClassDef) -> bool:
    return any(
        decorator_text(decorator).startswith(("cc.crm", "c_two.crm", "crm"))
        for decorator in cls.decorator_list
    )


for root in roots:
    for path in root.rglob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for cls in [n for n in ast.walk(tree) if isinstance(n, ast.ClassDef)]:
            classes.append((path, cls))
            decorators = [decorator_text(d) for d in cls.decorator_list]
            if not any("transferable" in d for d in decorators):
                continue
            own_methods = {n.name for n in cls.body if isinstance(n, ast.FunctionDef)}
            has_custom_hook = bool({"serialize", "deserialize"} & own_methods)
            has_abi_id = any("abi_id" in d for d in decorators)
            has_abi_schema = any("abi_schema" in d for d in decorators)
            if has_abi_id and has_abi_schema:
                custom_without_abi.setdefault(cls.name, []).append((path, cls))
                custom_without_abi_by_file.setdefault((path, cls.name), []).append((path, cls))
                continue
            has_abi = has_abi_id or has_abi_schema
            if has_custom_hook and not has_abi:
                custom_without_abi.setdefault(cls.name, []).append((path, cls))
                custom_without_abi_by_file.setdefault((path, cls.name), []).append((path, cls))

violations = []
for path, cls in classes:
    if not is_crm_class(cls):
        continue
    if path in negative_descriptor_fixture_paths:
        continue
    for item in cls.body:
        if not isinstance(item, ast.FunctionDef) or item.name.startswith("_"):
            continue
        if any("on_shutdown" in decorator_text(d) for d in item.decorator_list):
            continue
        referenced = set()
        for arg in item.args.posonlyargs + item.args.args + item.args.kwonlyargs:
            if arg.arg in {"self", "cls"}:
                continue
            referenced |= annotation_names(arg.annotation)
        referenced |= annotation_names(item.returns)
        for decorator in item.decorator_list:
            referenced |= transfer_kw_names(decorator)
        for name in sorted(referenced & custom_without_abi.keys()):
            local_candidates = custom_without_abi_by_file.get((path, name), [])
            candidates = local_candidates or custom_without_abi[name]
            for t_path, t_cls in candidates:
                if t_path in negative_descriptor_fixture_paths:
                    continue
                t_decorators = [decorator_text(d) for d in t_cls.decorator_list]
                if any("abi_id" in d for d in t_decorators) and any(
                    "abi_schema" in d for d in t_decorators
                ):
                    violations.append(
                        f"{path}:{item.lineno}:{cls.name}.{item.name} references "
                        f"{t_path}:{t_cls.lineno}:{name} with both abi_id and abi_schema"
                    )
                    continue
                violations.append(
                    f"{path}:{item.lineno}:{cls.name}.{item.name} references "
                    f"{t_path}:{t_cls.lineno}:{name} without abi_id or abi_schema"
                )

for violation in violations:
    print(violation)
raise SystemExit(1 if violations else 0)
PY
```

Expected after migration: no output and exit code 0. The only allowed no-ABI remote-signature fixtures live in `sdk/python/tests/unit/test_crm_descriptor.py`, and those tests must assert descriptor construction rejects them. This second scan is the gate for remote-surface safety; the broader class-level scan is only an inventory aid.

- [x] **Step 7: Build Python CRM descriptors**

Add `sdk/python/src/c_two/crm/descriptor.py` with:

```python
def build_contract_descriptor(crm_class: type) -> dict[str, object]:
    """Return the validated canonical descriptor object described below."""

def build_contract_fingerprints(crm_class: type) -> tuple[str, str]:
    descriptor = build_contract_descriptor(crm_class)
    abi_descriptor = _abi_descriptor_from(descriptor)
    signature_descriptor = _signature_descriptor_from(descriptor)
    abi_json = json.dumps(abi_descriptor, sort_keys=True, separators=(",", ":")).encode("utf-8")
    signature_json = json.dumps(signature_descriptor, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return (
        _native.contract_descriptor_sha256_hex(abi_json),
        _native.contract_descriptor_sha256_hex(signature_json),
    )
```

Define descriptor construction, annotation canonicalization, default-value
validation, transferable ABI reference resolution, the `json` import, and the
`_native` import in the same file. Python may serialize the derived ABI and
signature descriptor objects to JSON bytes, but Rust still parses,
canonicalizes, and hashes those bytes. `build_contract_descriptor(...)` must
construct exactly the descriptor fields listed below and raise the explicit
errors shown in this step. Hash material is limited to the derived ABI and
signature descriptor objects; Python module names and qualified names are not
part of those objects.

Descriptor requirements:

- include CRM namespace, class name, and version;
- include method names in the exact wire method-table order used by `NativeServerBridge.register_crm(...)` after `@cc.on_shutdown` filtering;
- include read/write access metadata;
- include every positional/keyword parameter name, `inspect.Parameter.kind`, annotation, and default-value metadata;
- include return annotation;
- include transfer metadata from `@cc.transfer(...)`;
- include stable transferable ABI references in hash material: built-in serializer family/version for `Default*Transferable`, declared custom ABI id/schema for custom hooks, field annotations only for a future generated dataclass codec that this plan does not implement;
- reject custom transferables whose decorator metadata contains both `abi_id` and `abi_schema`; descriptor hash material must have exactly one custom ABI identity source, never an implementation-defined precedence rule;
- do not include transferable Python module names or qualified names in
  `abi_hash` or `signature_hash` material. If diagnostics are added later, they
  must remain outside the derived hash descriptors.
- obtain method names by calling `c_two.crm.methods.rpc_method_names(crm_class)`. `NativeServerBridge.register_crm(...)` must call the same helper before constructing `MethodTable`; either remove `NativeServerBridge._discover_methods(...)` or make it a thin delegate to the CRM-layer helper. Descriptor building must not import `c_two.transport.server.native`, and it must not traverse `crm_class.__dict__` order independently.
- add a descriptor test with a CRM that has an unannotated `@cc.on_shutdown def cleanup(self): ...`; descriptor building must ignore `cleanup`, must not require a return annotation for it, and the resulting method list must match the native `MethodTable`.

Implement the shared CRM helper before descriptor imports it:

```python
# sdk/python/src/c_two/crm/methods.py
import inspect

from .meta import get_shutdown_method


def rpc_method_names(crm_class: type) -> list[str]:
    shutdown_name = get_shutdown_method(crm_class)
    names = [
        name
        for name, value in inspect.getmembers(crm_class, predicate=inspect.isfunction)
        if not name.startswith("_") and name != shutdown_name
    ]
    return sorted(names)
```

Canonical annotation grammar:

```text
{"kind": "primitive", "name": "int"}
{"kind": "none"}
{"kind": "list", "item": <type>}
{"kind": "dict", "key": <type>, "value": <type>}
{"kind": "tuple", "items": [<type>, ...]}
{"kind": "union", "items": [<type>, ...]}
{"kind": "transferable", "abi_ref": {"kind": "custom_id", "value": "c-two.tests.GridBlock.v1"}}
{"kind": "transferable", "abi_ref": {"kind": "custom_schema", "sha256": "<64 lowercase hex>"}}
{"kind": "transferable", "abi_ref": {"kind": "builtin", "family": "python-pickle-default", "version": "pickle-protocol-4"}}
```

Rules:

- normalize `str | None` and `typing.Optional[str]` to the same sorted union representation;
- sort union variants by their canonical JSON string and remove duplicates;
- reject `Any`, unresolved `ForwardRef`, bare containers such as `list` without an item type, and unknown typing constructs;
- do not describe dataclass-field annotations as a supported ABI until the same change also adds a generated dataclass-field wire codec and tests for that codec;
- reject plain dataclass-field transferables in remote signatures because this remediation does not add that generated dataclass-field wire codec;
- exclude Python module and qualified names from the ABI or signature JSON that Rust hashes;
- never use `repr(annotation)` or `str(annotation)` as descriptor material.

Default-value policy:

- Encode missing defaults as `{"kind": "missing"}`.
- Encode supported defaults as `{"kind": "json_scalar", "value": <value>}` for `None`, `bool`, `int`, finite `float`, and `str`.
- Reject `inspect.Parameter.VAR_POSITIONAL` and `inspect.Parameter.VAR_KEYWORD`.
- Reject unsupported defaults with a registration-time error until a cross-language default schema is designed.

```python
raise ValueError(
    "CRM method Grid.get uses unsupported default value for parameter 'limit'; "
    "only None, bool, int, finite float, and str defaults are ABI-stable"
)
```

Reject incomplete remotely callable signatures:

```python
raise ValueError(
    "CRM method Grid.get must annotate every parameter and return value "
    "before it can be exported across IPC or relay"
)
```

Reject custom hook transferables without an explicit ABI declaration:

```python
raise ValueError(
    "Transferable GridBlock uses custom serialize/deserialize hooks; "
    "declare an ABI id or ABI schema so route compatibility can be checked"
)
```

Reject plain no-hook transferables until a generated dataclass-field wire codec exists:

```python
raise ValueError(
    "Transferable PlainData has no concrete wire ABI; declare custom "
    "serialize/deserialize hooks with an ABI id or ABI schema, or use a "
    "built-in Default*Transferable path"
)
```

- [x] **Step 8: Pass fingerprints during server registration**

In `NativeServerBridge.register_crm(...)`, compute `(abi_hash, signature_hash)` from the CRM class and pass both into native route registration. This is the producer-side choke point for registered route identity; do not leave it as a CRM-triplet-only call that relies on later connect-side checks.

The registration source-to-sink chain must be updated as one commit:

- `sdk/python/src/c_two/transport/server/native.py::NativeServerBridge.register_crm(...)` calls the shared CRM descriptor builder after shutdown-method filtering and before `MethodTable.from_methods(...)`; it passes `abi_hash` and `signature_hash` into `runtime_session.register_route(...)`.
- `sdk/python/native/src/runtime_session_ffi.rs::PyRuntimeSession.register_route(...)` accepts the two hash values with no empty defaults, validates them through `c2-contract`, and stores them in `RuntimeRouteSpec`.
- `sdk/python/native/src/server_ffi.rs::PyServer::build_route(...)` accepts the same two hash values and constructs a `CrmRoute` with them. `build_route(...)` must not recompute hashes from partial Rust metadata and must not synthesize empty hashes for tests.
- `core/runtime/c2-runtime/src/outcome.rs::RuntimeRouteSpec` stores `abi_hash` and `signature_hash`; `RuntimeSession::register_route(...)` compares route/spec hashes before registering with `c2-server`, and relay registration receives the same validated hash pair.
- `core/transport/c2-server/src/dispatcher.rs::CrmRoute` stores `abi_hash` and `signature_hash`; `core/transport/c2-server/src/server.rs` projects those values into the IPC handshake route table.

The Python local/thread route slot must also store the same descriptor hashes. Same-process `cc.connect(...)` must compare the expected descriptor hashes against the stored slot before returning a thread-local proxy. Thread-local is a zero-serialization fast path, not a compatibility bypass.

In `server_ffi.rs`, `c2-server`, and `c2-ipc`, store the fingerprints with the route and publish them in the server handshake. Add `c2-contract` to `core/transport/c2-server/Cargo.toml`, add `abi_hash` and `signature_hash` to `core/transport/c2-server/src/dispatcher.rs::CrmRoute`, validate them in `server.rs::validate_route_for_wire(...)`, and project them into `c2_wire::handshake::RouteInfo`. `CrmRoute`, `RouteInfo`, `c2_ipc::MethodTable`, and the Python `RustIpcClient` FFI projection must all expose the same route fingerprint values.

The IPC projection is part of the security boundary, not a diagnostic-only
detail. Update `core/transport/c2-ipc/src/client.rs::MethodTable` to store
`abi_hash` and `signature_hash` beside the CRM namespace/name/version; update
`MethodTable::from_route(...)`, `route_contract(...)`,
`validate_route_contract(...)`, and the `SyncClient` delegating methods so the
comparison fails when either hash differs. Update
`sdk/python/native/src/client_ffi.rs::PyRustClient.route_contract(...)` to return
the same five-field contract payload or an `ExpectedRouteContract` projection;
do not leave a Python-visible CRM-triplet-only route-contract helper after this
task.

Update relay test support in the same step so route-hash fields are not bolted
onto production structs while tests keep CRM-triplet-only fixtures:

- Add this fixture shape and constants to `test_support.rs`:

```rust
pub(crate) const TEST_ABI_HASH: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(crate) const TEST_SIGNATURE_HASH: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub(crate) struct TestRouteContract<'a> {
    pub route_name: &'a str,
    pub crm_ns: &'a str,
    pub crm_name: &'a str,
    pub crm_ver: &'a str,
    pub abi_hash: &'a str,
    pub signature_hash: &'a str,
}
```

- `register_echo_route_with_contract(...)` must accept
  `TestRouteContract<'_>` and pass all six metadata fields into `CrmRoute`.
- `start_live_server_with_routes(...)` must construct `TestRouteContract`
  values with `TEST_ABI_HASH` and `TEST_SIGNATURE_HASH`.
- `start_live_server_with_identity_and_contracts(...)` must accept
  `&[TestRouteContract<'_>]`; no relay live-server helper may keep
  `(&str, &str, &str, &str)` CRM-triplet-only route tuples.

Update relay IPC route attestation so the actual connected IPC handshake is the authority:

```rust
pub(crate) struct AttestedRouteContract {
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
}
```

Update `core/transport/c2-http/src/relay/authority.rs` so
`attest_ipc_route_contract(...)` compares claimed values, when supplied, against
the handshake route table and returns the hash-bearing `AttestedRouteContract`.
The `RouteCommand::RegisterLocal` payload and every call site in `router.rs`,
`state.rs`, and command-loop `server.rs` must carry the attested ABI and
signature hashes, not separate claim-only strings. HTTP `/_register` body fields
and command-loop registration arguments are expectations only; a relay must
reject the registration if the connected IPC route advertises different hashes.
The normal SDK registration path must send a complete expected route contract
for named CRM routes: route name, CRM namespace/name/version, ABI hash, and
signature hash. Low-level relay upstream registration may omit caller claims
only when it is explicitly named as an attestation-only path and cannot publish
claim-derived metadata; in that path the relay must derive every published CRM
and hash field from the IPC handshake. `RelayControlClient::register(...)`,
`RegisterRequest`, `PyRustRelayControlClient.register(...)`, and any command-loop
registration helper must be split or renamed if needed so an empty-default
CRM-triplet-only registration API cannot survive as the production SDK path.
Do not keep a test-only `skip_ipc_validation` branch. Tests that need relay
registration must use real IPC attestation or explicitly named fixtures that
cannot enter production configuration, CLI parsing, HTTP registration, or route
state publication without a hash-bearing IPC contract.

Thread-local local slots must store the same fingerprints so same-process `cc.connect(...)` can compare the requested CRM against the registered CRM without serialization.

- [x] **Step 9: Pass expected fingerprints during connect**

In `registry.py`, compute expected fingerprints from the requested CRM class before choosing thread-local, direct IPC, explicit HTTP, or relay-aware connection paths.

In `sdk/python/native/src/runtime_session_ffi.rs`, `sdk/python/native/src/client_ffi.rs`, and `c2-runtime`, include expected fingerprints in:

- direct IPC route validation;
- relay-discovered local IPC validation;
- explicit HTTP relay contract probing;
- relay-aware route acquisition.

Mismatch must fail before a CRM method call is sent.

- Add one Rust-owned expected-contract shape and thread it through every named-route path. Define this once in the new foundation crate `c2-contract` so `c2-wire`, `c2-ipc`, `c2-http`, `c2-runtime`, and `sdk/python/native` all use the same type and validators without making `c2-http` client-only builds depend on the wire codec or relay feature. Do not duplicate an equivalent struct in `c2-http`, `c2-runtime`, or PyO3 glue; duplicated shapes are a plan failure because they can drift on hash validation, empty-field handling, or route-name validation.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedRouteContract {
    pub route_name: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
}
```

Validate `ExpectedRouteContract` with the same route-name, CRM-tag, and hash validators used for registered route metadata. Empty strings are invalid in this struct.

- Low-level admin helpers that do not target a CRM route may pass no expected contract only when `route_name` is empty.
- PyO3 named-route production APIs must use `expected: ExpectedRouteContract` or a Python-visible `expected_contract` dict/pyclass over six parallel strings. If a diagnostic/test helper exposes scalar fields, those six fields are the contract payload: it must expose route name, CRM namespace/name/version, ABI hash, and signature hash with no empty defaults, convert them into `ExpectedRouteContract`, and derive the dispatched route from that object. It must not also accept a second independent `route_name` argument. The old shape `route_name="", expected_crm_ns="", expected_crm_name="", expected_crm_ver=""` must disappear from `acquire_ipc_client(...)`, `connect_explicit_relay_http(...)`, and `connect_via_relay(...)`.
- `RuntimeSession::resolve_relay_connection(...)`, `connect_relay_http_client(...)`, and `connect_explicit_relay_http_client(...)` must accept `ExpectedRouteContract`, not `(route_name, expected_crm_ns, expected_crm_name, expected_crm_ver)`. Derive the route name from `expected.route_name`; do not accept a second independent route-name string that can diverge from the validated contract.
- `c2_ipc::Client::validate_route_contract(...)` and `SyncClient::validate_route_contract(...)` must accept `&ExpectedRouteContract` for named routes and compare CRM identity plus `abi_hash` and `signature_hash`. The only allowed no-contract validation path is an explicit unnamed/admin helper whose route name is empty and which cannot dispatch a CRM method.
- `core/transport/c2-http/src/client/relay_aware.rs` must not expose any public named-route constructor that can leave expected identity unset. Replace `RelayAwareHttpClient::new(relay_url, route_name, use_proxy, config)` and `RelayAwareHttpClient::new_with_control(...)` with constructor shapes that require the full expected contract at construction time, for example `RelayAwareHttpClient::new(relay_url, expected: ExpectedRouteContract, use_proxy, config)` and `RelayAwareHttpClient::new_with_control(control, expected: ExpectedRouteContract, config)`. Derive the route name from `expected.route_name`; do not accept a second independent route-name string.
- Remove or privatize optional expected-identity setters such as `with_expected_crm(...)` once constructor-time `ExpectedRouteContract` exists. There must be no public `expected_crm: None` state for named relay-aware clients after this task.
- Update `c2-runtime` relay paths (`resolve_relay_connection`, `connect_explicit_relay_http_client`, and the runtime-session helpers exposed through PyO3) so every named relay-aware client is built from the Python-computed `ExpectedRouteContract` containing route name, CRM identity, `abi_hash`, and `signature_hash`.
- Update all `core/transport/c2-http/src/client/relay_aware.rs` tests that currently call `RelayAwareHttpClient::new(...)` directly. Each test must build an `ExpectedRouteContract` fixture with lowercase 64-hex `abi_hash` and `signature_hash`; tests must not rely on `expected_crm: None` as a valid named-route state.
- Add one compile-time or construction-time regression test proving a named relay-aware client cannot be created without an `ExpectedRouteContract`. If Rust type signatures make the old call impossible, keep a runtime test that `new_with_control(...)` rejects an empty or malformed expected contract before any route acquisition.
- `core/transport/c2-http/src/client/client.rs` must remove or privatize public named-route call/probe methods that synthesize `None` expected contract. Replace them with methods such as `call_route_async(&self, expected: &ExpectedRouteContract, method_name: &str, data: &[u8])` and `probe_route_async(&self, expected: &ExpectedRouteContract)`. These methods must derive the route name from `expected.route_name`; do not keep a signature that accepts both `route_name` and `ExpectedRouteContract`.
- `sdk/python/native/src/http_ffi.rs` must remove or route-bind `PyRustHttpClient.call(route_name, ...)`. Do not keep a public Python-visible method named `call(...)` that accepts `route_name`, even if it also accepts expected fields. If a low-level diagnostic or test path still needs arbitrary named HTTP calls, expose it as `call_route_with_expected_contract(...)`; that helper must accept exactly one complete expected contract object or scalar contract payload and must derive the dispatched route from it.
- `sdk/python/native/src/runtime_session_ffi.rs::acquire_ipc_client(...)`, `connect_explicit_relay_http(...)`, and `connect_via_relay(...)` must reject a non-empty `route_name` unless expected CRM namespace/name/version plus both hashes are supplied and valid. Do not keep default empty expected strings for named route acquisition. Use route-bound signatures such as `acquire_ipc_client(address, expected_contract=None)`, `connect_explicit_relay_http(address, expected_contract)`, and `connect_via_relay(expected_contract)`, where `expected_contract=None` is accepted only for unnamed admin/probe use and never for CRM dispatch.
- If `route_name` is non-empty, IPC and HTTP validation must require expected identity plus both expected hashes.
- `sdk/python/native/src/client_ffi.rs::PyRustClient.call(...)` must not remain a public named-route bypass. For clients acquired for a named route, return a route-bound wrapper whose Python-visible `call(...)` signature is `call(method_name, data)` and whose internal route name is the validated `ExpectedRouteContract.route_name`. If a raw low-level IPC helper remains for tests, it must be renamed to `call_route_with_expected_contract(...)`, accept a full `ExpectedRouteContract` in the same call, derive the route name from that contract, and must not be the method used by `CRMProxy`.
- `sdk/python/native/src/runtime_session_ffi.rs::PyRelayConnectedClient.call(...)` must not accept an arbitrary per-call `route_name`. Its IPC branch currently forwards the caller-provided route to `call_sync_client(...)`; replace that with a route-bound field captured from the validated `ExpectedRouteContract`, or require a full expected contract on every per-call named route. `CRMProxy.ipc(...)` and `CRMProxy.http(...)` must call bound clients without being able to swap route names after connect.
- Migrate every existing direct low-level named-call user in the same task so the API change does not leave test-only bypasses behind:
  - `sdk/python/src/c_two/transport/client/proxy.py::CRMProxy.call(...)` must call route-bound clients as `self._client.call(method_name, data or b"")`; keep `_name` only as diagnostic `route_name` metadata and for thread-local route reporting.
  - `sdk/python/tests/unit/test_crm_proxy.py::_MockClient.call(...)` must be migrated to the same route-bound signature: `call(self, method_name: str, data: bytes = b"")`. Its recorded calls must become `(method_name, data)` tuples. These unit tests are part of the public-shape guardrail; do not leave a test double that still accepts `call(route_name, method_name, data)`.
  - `sdk/python/tests/integration/test_ipc_buddy_reply.py` direct `rust_client.call(route_name, "echo", ...)` checks must either use the route-bound client returned by `CRMProxy` or a raw test helper that supplies a full `ExpectedRouteContract` in the same call.
  - `sdk/python/tests/integration/test_http_relay.py::test_connect_http_close_closes_relay_aware_client` must call the route-bound relay client as `native_client.call("greeting", b"")` after `cc.close(...)`, or use a dedicated closed-state probe. It must not preserve a three-argument named-route call just to test close behavior.
  - Any direct `PyRustHttpClient.call(route_name, ...)` tests must move to a new explicit `call_route_with_expected_contract(...)` test-only/helper API that requires `ExpectedRouteContract`, or be deleted if runtime-session relay clients are the only supported named HTTP callers.
- Add multiline `rg`, call-expression AST, and method-definition AST guardrails to Task 13 or the relevant Python test file so future code cannot reintroduce the old public shape. A single-line `rg` is not enough because the current `CRMProxy.call(...)` form can split `_client.call(` and `self._name` across lines, and call-expression scans do not catch stale test doubles such as `def call(self, route_name, method_name, data)`.
- Registration guardrails must recognize hash-bearing typed carriers as first-class evidence. A Rust function such as `RuntimeSession::register_route(route: CrmRoute, spec: RuntimeRouteSpec, ...)` is valid only when the referenced carrier structs themselves contain `abi_hash` and `signature_hash`, and the function body compares or relays those fields. Do not force scalar hash parameters when an existing route/spec type is the cleaner boundary. Conversely, do not treat the mere type name as sufficient: the guardrail must inspect the carrier struct body and the registration function body.

```bash
rg -n -U "\\.call\\(" sdk/python/src sdk/python/tests

python - <<'PY'
import ast
from pathlib import Path

roots = [Path("sdk/python/src"), Path("sdk/python/tests")]
violations = []

def expr_text(node: ast.AST) -> str:
    try:
        return ast.unparse(node)
    except Exception:
        return "<expr>"

for root in roots:
    for path in root.rglob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            if not isinstance(node.func, ast.Attribute) or node.func.attr != "call":
                continue
            receiver = expr_text(node.func.value)
            three_positional_call = len(node.args) >= 3
            keyword_names = {kw.arg for kw in node.keywords if kw.arg is not None}
            keyword_route_like = bool(keyword_names & {"route_name", "name"})
            if three_positional_call or keyword_route_like:
                violations.append(
                    f"{path}:{node.lineno}: public/native .call() keeps route-name-shaped call surface on {receiver}"
                )

for violation in violations:
    print(violation)
raise SystemExit(1 if violations else 0)
PY

python - <<'PY'
import ast
from pathlib import Path

roots = [Path("sdk/python/src"), Path("sdk/python/tests")]
violations = []

for root in roots:
    for path in root.rglob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if node.name != "call":
                continue
            positional = [arg.arg for arg in node.args.posonlyargs + node.args.args]
            user_args = positional[1:] if positional and positional[0] in {"self", "cls"} else positional
            if len(user_args) >= 3 and user_args[0] in {"route_name", "name"}:
                if "with_expected_contract" not in path.name and "with_expected_contract" not in node.name:
                    violations.append(
                        f"{path}:{node.lineno}: public call() still accepts a route-name first argument"
                    )

for violation in violations:
    print(violation)
raise SystemExit(1 if violations else 0)
PY

# Python AST coverage is necessary but not sufficient: the dangerous public
# route-name bypasses live in PyO3, runtime-session acquisition, IPC route
# validation, HTTP resolve/cache, and Rust HTTP client entry points.
rg -n "route_name|expected_crm|ExpectedRouteContract|ResolveCacheKey|x-c2-expected|resolve_matching" \
  sdk/python/native/src/client_ffi.rs \
  sdk/python/native/src/http_ffi.rs \
  sdk/python/native/src/runtime_session_ffi.rs \
  core/runtime/c2-runtime/src/session.rs \
  core/transport/c2-ipc/src/client.rs \
  core/transport/c2-ipc/src/sync_client.rs \
  core/transport/c2-http/src/client/client.rs \
  core/transport/c2-http/src/client/control.rs \
  core/transport/c2-http/src/client/relay_aware.rs \
  core/transport/c2-http/src/relay/router.rs

python - <<'PY'
import ast
import re
from pathlib import Path

ffi_paths = [
    Path("sdk/python/native/src/client_ffi.rs"),
    Path("sdk/python/native/src/http_ffi.rs"),
    Path("sdk/python/native/src/runtime_session_ffi.rs"),
]
rust_paths = [
    Path("core/runtime/c2-runtime/src/session.rs"),
    Path("core/transport/c2-ipc/src/client.rs"),
    Path("core/transport/c2-ipc/src/sync_client.rs"),
    Path("core/transport/c2-http/src/client/client.rs"),
    Path("core/transport/c2-http/src/client/control.rs"),
    Path("core/transport/c2-http/src/client/relay_aware.rs"),
    Path("core/transport/c2-http/src/relay/router.rs"),
]
violations = []

fn_sig = re.compile(
    r"(?:#\[pyo3\(signature\s*=\s*\((?P<pysig>.*?)\)\)\]\s*)?"
    r"(?P<vis>pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+"
    r"(?P<name>\w+)(?:<[^>]+>)?\s*\((?P<args>.*?)\)\s*(?:->|\{)",
    re.S,
)
required_expected_fields = {
    "expected_crm_ns",
    "expected_crm_name",
    "expected_crm_ver",
    "expected_abi_hash",
    "expected_signature_hash",
}
route_sensitive_names = (
    "call",
    "probe",
    "resolve",
    "acquire",
    "connect",
    "register",
    "register_route",
    "build_route",
    "commit_register",
    "validate_route_contract",
    "new",
    "new_with_control",
)
route_arg_names = ("route_name", "name", "route")
route_management_exempt_names = {"unregister", "invalidate"}
registration_sensitive_names = ("register", "register_route", "build_route", "commit_register")
hash_bearing_carriers = {
    "ExpectedRouteContract": Path("core/foundation/c2-contract/src/lib.rs"),
    "RuntimeRouteSpec": Path("core/runtime/c2-runtime/src/outcome.rs"),
    "CrmRoute": Path("core/transport/c2-server/src/dispatcher.rs"),
    "AttestedRouteContract": Path("core/transport/c2-http/src/relay/authority.rs"),
    "TestRouteContract": Path("core/transport/c2-http/src/relay/test_support.rs"),
    "RegisterRequest": Path("core/transport/c2-http/src/client/control.rs"),
}

def compact(value: str | None) -> str:
    return " ".join((value or "").split())

def has_symbol(text: str, name: str) -> bool:
    return re.search(
        rf"(?<![A-Za-z0-9_]){re.escape(name)}(?![A-Za-z0-9_])",
        text,
    ) is not None

def route_arg_name(text: str) -> str | None:
    for candidate in route_arg_names:
        if has_symbol(text, candidate):
            return candidate
    return None

def is_route_sensitive_fn(name: str) -> bool:
    if name in route_management_exempt_names:
        return False
    return any(token in name for token in route_sensitive_names)

def is_registration_sensitive_fn(name: str) -> bool:
    if name in route_management_exempt_names or name.startswith("unregister"):
        return False
    return any(token in name for token in registration_sensitive_names)

def hash_bearing_carrier_names(text: str) -> list[str]:
    names = []
    for type_name, path in hash_bearing_carriers.items():
        if not has_symbol(text, type_name):
            continue
        if not path.exists():
            continue
        carrier_body = rust_struct_body(path.read_text(), type_name) or ""
        if "abi_hash" in carrier_body and "signature_hash" in carrier_body:
            names.append(type_name)
    return names

def expected_route_contract_definitions() -> list[tuple[Path, int]]:
    definitions: list[tuple[Path, int]] = []
    for root in (Path("core"), Path("sdk/python/native/src"), Path("sdk/python/src")):
        if not root.exists():
            continue
        for path in root.rglob("*.rs" if root != Path("sdk/python/src") else "*.py"):
            text = path.read_text()
            patterns = [r"\bstruct\s+ExpectedRouteContract\b"]
            if path.suffix == ".py":
                patterns.append(r"\bclass\s+ExpectedRouteContract\b")
            for pattern in patterns:
                for match in re.finditer(pattern, text):
                    definitions.append((path, text[: match.start()].count("\n") + 1))
    return definitions

def accepts_valid_expected_contract(text: str, name: str, combined: str) -> bool:
    carrier_names = hash_bearing_carrier_names(combined)
    body = rust_fn_body(text, name) or ""
    if "ExpectedRouteContract" in carrier_names:
        return "abi_hash" in body and "signature_hash" in body
    if not has_symbol(combined, "expected_contract"):
        return False
    return (
        has_symbol(body, "ExpectedRouteContract")
        and "abi_hash" in body
        and "signature_hash" in body
    )

def rust_fn_body(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return None
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

def rust_fn_signature(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{;]*",
        text,
        re.S,
    )
    return None if match is None else " ".join(match.group(0).split())

def rust_struct_body(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?struct\s+{re.escape(name)}(?:<[^>]+>)?\s*\{{(?P<body>.*?)\n\}}",
        text,
        re.S,
    )
    return None if match is None else match.group("body")

def rust_item_body(text: str, keyword: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?{re.escape(keyword)}\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return None
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

def rust_enum_variant_body(text: str, enum_name: str, variant_name: str) -> str | None:
    enum_body = rust_item_body(text, "enum", enum_name)
    if enum_body is None:
        return None
    match = re.search(rf"\b{re.escape(variant_name)}\s*\{{", enum_body)
    if match is None:
        return None
    depth = 1
    i = match.end()
    while i < len(enum_body) and depth:
        if enum_body[i] == "{":
            depth += 1
        elif enum_body[i] == "}":
            depth -= 1
        i += 1
    return enum_body[match.end(): i - 1]

def python_method_source(path: Path, class_name: str, method_name: str) -> str:
    text = path.read_text()
    tree = ast.parse(text)
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)) and item.name == method_name:
                    return ast.get_source_segment(text, item) or ""
    return ""

expected_contract_defs = expected_route_contract_definitions()
expected_contract_paths = [path.as_posix() for path, _line in expected_contract_defs]
if expected_contract_paths != ["core/foundation/c2-contract/src/lib.rs"]:
    rendered = ", ".join(f"{path}:{line}" for path, line in expected_contract_defs) or "<none>"
    violations.append(
        "ExpectedRouteContract struct must be defined exactly once in "
        f"core/foundation/c2-contract/src/lib.rs; found {rendered}"
    )

native_server = Path("sdk/python/src/c_two/transport/server/native.py")
native_server_text = native_server.read_text()
register_crm_body = python_method_source(native_server, "NativeServerBridge", "register_crm")
for required in ("abi_hash", "signature_hash", "runtime_session.register_route"):
    if required not in register_crm_body:
        violations.append(
            f"{native_server}: NativeServerBridge.register_crm() must produce and pass {required}"
        )
if "_discover_methods(" in register_crm_body and "rpc_method_names" not in native_server_text:
    violations.append(
        f"{native_server}: register_crm() must use the shared CRM method-order helper used by descriptor hashing"
    )
if "class CRMSlot" in native_server_text:
    crm_slot_match = re.search(r"class\s+CRMSlot\b[\s\S]*?(?=\nclass\s+|\ndef\s+|\Z)", native_server_text)
    crm_slot_body = "" if crm_slot_match is None else crm_slot_match.group(0)
    if "abi_hash" not in crm_slot_body or "signature_hash" not in crm_slot_body:
        violations.append(f"{native_server}: CRMSlot must store abi_hash and signature_hash for thread-local validation")

runtime_ffi = Path("sdk/python/native/src/runtime_session_ffi.rs").read_text()
register_route_body = rust_fn_body(runtime_ffi, "register_route") or ""
for required in ("abi_hash", "signature_hash"):
    if required not in register_route_body:
        violations.append(f"sdk/python/native/src/runtime_session_ffi.rs: register_route() must accept/pass {required}")
if re.search(r"register_route[\s\S]{0,700}crm_ns\s*=\s*\"\"", runtime_ffi):
    violations.append("sdk/python/native/src/runtime_session_ffi.rs: register_route() must not keep empty CRM defaults for named route registration")
runtime_spec = rust_struct_body(Path("core/runtime/c2-runtime/src/outcome.rs").read_text(), "RuntimeRouteSpec") or ""
if "abi_hash" not in runtime_spec or "signature_hash" not in runtime_spec:
    violations.append("core/runtime/c2-runtime/src/outcome.rs: RuntimeRouteSpec must store abi_hash and signature_hash")
server_ffi = Path("sdk/python/native/src/server_ffi.rs").read_text()
build_route_body = rust_fn_body(server_ffi, "build_route") or ""
if "abi_hash" not in build_route_body or "signature_hash" not in build_route_body:
    violations.append("sdk/python/native/src/server_ffi.rs: PyServer::build_route() must construct CrmRoute with route hashes")
runtime_session = Path("core/runtime/c2-runtime/src/session.rs").read_text()
runtime_register_body = rust_fn_body(runtime_session, "register_route") or ""
if "abi_hash" not in runtime_register_body or "signature_hash" not in runtime_register_body:
    violations.append("core/runtime/c2-runtime/src/session.rs: RuntimeSession::register_route() must compare and relay route hashes")

for path in ffi_paths:
    text = path.read_text()
    for match in fn_sig.finditer(text):
        name = match.group("name")
        args = compact(match.group("args"))
        pysig = compact(match.group("pysig"))
        combined = f"{args} {pysig}"
        route_arg = route_arg_name(combined)
        registration_sensitive = is_registration_sensitive_fn(name)
        route_sensitive = is_route_sensitive_fn(name)
        mentions_expected_contract = has_symbol(combined, "ExpectedRouteContract") or has_symbol(combined, "expected_contract")
        if registration_sensitive and route_arg is not None:
            carrier_names = hash_bearing_carrier_names(combined)
            accepts_expected_contract = accepts_valid_expected_contract(text, name, combined)
            has_hash_pair = (
                (has_symbol(combined, "abi_hash") and has_symbol(combined, "signature_hash"))
                or (has_symbol(combined, "expected_abi_hash") and has_symbol(combined, "expected_signature_hash"))
                or accepts_expected_contract
                or bool(carrier_names)
            )
            if not has_hash_pair:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: registration helper {name}() accepts route argument {route_arg} without route hashes"
                )
            if carrier_names:
                body = rust_fn_body(text, name) or ""
                if "abi_hash" not in body or "signature_hash" not in body:
                    line = text[: match.start()].count("\n") + 1
                    violations.append(
                        f"{path}:{line}: registration helper {name}() accepts hash-bearing {carrier_names} but does not compare or relay route hashes in the function body"
                    )
            if has_symbol(combined, "expected_contract") and not accepts_expected_contract:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: registration helper {name}() mentions expected_contract but does not convert it into canonical ExpectedRouteContract with route hashes"
                )
            for field in ("crm_ns", "crm_name", "crm_ver", "abi_hash", "signature_hash", "expected_abi_hash", "expected_signature_hash"):
                if re.search(rf"(?<![A-Za-z0-9_]){re.escape(field)}\s*=\s*\"\"", pysig):
                    line = text[: match.start()].count("\n") + 1
                    violations.append(
                        f"{path}:{line}: registration helper {name}() defaults {field} to an empty string"
                    )
            continue
        if name == "call" and route_arg is not None:
            line = text[: match.start()].count("\n") + 1
            violations.append(
                f"{path}:{line}: PyO3 call() still accepts route argument {route_arg}"
            )
        if route_sensitive and route_arg is not None and mentions_expected_contract:
            line = text[: match.start()].count("\n") + 1
            violations.append(
                f"{path}:{line}: {name}() accepts route argument {route_arg} alongside expected_contract; derive the route from the contract"
            )
        elif route_sensitive and route_arg is not None:
            missing = sorted(field for field in required_expected_fields if not has_symbol(args, field))
            scalar_helper = "with_expected_contract" in name and not missing
            if missing or name in {"acquire_ipc_client", "connect_explicit_relay_http", "connect_via_relay"}:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: {name}() accepts route argument {route_arg} without complete ExpectedRouteContract"
                )
            if scalar_helper and any(re.search(rf"(?<![A-Za-z0-9_]){re.escape(field)}\s*=\s*\"\"", pysig) for field in required_expected_fields | {"route_name"}):
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: {name}() defaults expected contract fields to empty strings"
                )
            if name != "call" and not scalar_helper:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: route-name PyO3 helper must be route-bound or named *_with_expected_contract"
                )

for path in rust_paths:
    text = path.read_text()
    for match in fn_sig.finditer(text):
        name = match.group("name")
        args = compact(match.group("args"))
        visible = bool(match.group("vis"))
        registration_sensitive = is_registration_sensitive_fn(name)
        route_sensitive = is_route_sensitive_fn(name)
        route_arg = route_arg_name(args)
        if visible and registration_sensitive and route_arg is not None:
            carrier_names = hash_bearing_carrier_names(args)
            has_hash_pair = (
                (has_symbol(args, "abi_hash") and has_symbol(args, "signature_hash"))
                or (has_symbol(args, "expected_abi_hash") and has_symbol(args, "expected_signature_hash"))
                or bool(carrier_names)
            )
            if not has_hash_pair:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: public registration helper {name}() accepts route argument {route_arg} without route hashes"
                )
            if carrier_names:
                body = rust_fn_body(text, name) or ""
                if "abi_hash" not in body or "signature_hash" not in body:
                    line = text[: match.start()].count("\n") + 1
                    violations.append(
                        f"{path}:{line}: public registration helper {name}() accepts hash-bearing {carrier_names} but does not compare or relay route hashes in the function body"
                    )
            continue
        if visible and route_sensitive and route_arg is not None and has_symbol(args, "ExpectedRouteContract"):
            line = text[: match.start()].count("\n") + 1
            violations.append(
                f"{path}:{line}: public {name}() accepts route argument {route_arg} alongside ExpectedRouteContract; derive the route from the contract"
            )
        elif visible and route_sensitive and route_arg is not None:
            line = text[: match.start()].count("\n") + 1
            violations.append(
                f"{path}:{line}: public {name}() accepts independent route argument {route_arg} without ExpectedRouteContract ownership"
            )
        if name == "validate_route_contract" and not has_symbol(args, "ExpectedRouteContract"):
            line = text[: match.start()].count("\n") + 1
            violations.append(
                f"{path}:{line}: validate_route_contract() must validate the full ExpectedRouteContract"
            )

control = Path("core/transport/c2-http/src/client/control.rs").read_text()
if "abi_hash" not in control or "signature_hash" not in control:
    violations.append(
        "core/transport/c2-http/src/client/control.rs: ResolveCacheKey and resolve_matching_async() must include expected hashes"
    )
register_struct = re.search(
    r"struct\s+RegisterRequest(?:<[^>]+>)?\s*\{(?P<body>.*?)\n\}",
    control,
    re.S,
)
if register_struct is not None:
    register_fields = register_struct.group("body")
    register_body = rust_fn_body(control, "register")
    register_has_hashes = (
        ("expected_abi_hash" in register_fields and "expected_signature_hash" in register_fields)
        or ("abi_hash" in register_fields and "signature_hash" in register_fields)
    )
    if not register_has_hashes:
        violations.append(
            "core/transport/c2-http/src/client/control.rs: RegisterRequest/register() must carry expected route hashes or be split into an explicitly attestation-only helper"
        )
    if register_body is not None and any(
        stale_default in register_body
        for stale_default in ('crm_ns: ""', 'crm_name: ""', 'crm_ver: ""')
    ):
        violations.append(
            "core/transport/c2-http/src/client/control.rs: register() must not synthesize empty CRM expectation defaults for named SDK registration"
        )
http_ffi = Path("sdk/python/native/src/http_ffi.rs").read_text()
for match in fn_sig.finditer(http_ffi):
    if match.group("name") != "register":
        continue
    args = compact(match.group("args"))
    pysig = compact(match.group("pysig"))
    combined = f"{args} {pysig}"
    register_has_hash_scalars = (
        (has_symbol(combined, "expected_abi_hash") and has_symbol(combined, "expected_signature_hash"))
        or (has_symbol(combined, "abi_hash") and has_symbol(combined, "signature_hash"))
    )
    register_accepts_expected_contract = accepts_valid_expected_contract(http_ffi, "register", combined)
    if not (register_accepts_expected_contract or register_has_hash_scalars):
        violations.append(
            "sdk/python/native/src/http_ffi.rs: PyRustRelayControlClient.register() must expose expected hashes or be renamed to an explicit attestation-only helper"
        )
    if has_symbol(combined, "expected_contract") and not register_accepts_expected_contract:
        violations.append(
            "sdk/python/native/src/http_ffi.rs: PyRustRelayControlClient.register() mentions expected_contract but does not convert it into canonical ExpectedRouteContract with route hashes"
        )
    for field in ("crm_ns", "crm_name", "crm_ver", "expected_abi_hash", "expected_signature_hash"):
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(field)}\s*=\s*\"\"", pysig):
            violations.append(
                f"sdk/python/native/src/http_ffi.rs: PyRustRelayControlClient.register() must not default {field} to an empty string"
            )
router = Path("core/transport/c2-http/src/relay/router.rs").read_text()
for needle in ("x-c2-expected-abi-hash", "x-c2-expected-signature-hash", "abi_hash", "signature_hash"):
    if needle not in router:
        violations.append(
            f"core/transport/c2-http/src/relay/router.rs: missing expected route fingerprint field/header {needle}"
        )

wire_control = Path("core/protocol/c2-wire/src/control.rs").read_text()
wire_handshake = Path("core/protocol/c2-wire/src/handshake.rs").read_text()

def public_validate_functions(text: str) -> list[str]:
    return sorted(
        set(re.findall(r"pub(?:\([^)]*\))?\s+fn\s+(validate_\w+)\b", text))
    )

if re.search(r"pub(?:\([^)]*\))?\s+(?:const|use)[^\n]*MAX_CALL_ROUTE_NAME_BYTES", wire_control):
    violations.append(
        "core/protocol/c2-wire/src/control.rs: remove public MAX_CALL_ROUTE_NAME_BYTES compatibility surface; call c2-contract directly"
    )
if re.search(r"pub(?:\([^)]*\))?\s+(?:const|use)[^\n]*MAX_HANDSHAKE_NAME_BYTES", wire_handshake):
    violations.append(
        "core/protocol/c2-wire/src/handshake.rs: remove public MAX_HANDSHAKE_NAME_BYTES compatibility surface; call c2-contract directly"
    )
if "c2_contract" not in wire_control:
    violations.append(
        "core/protocol/c2-wire/src/control.rs: route-name validation must use c2-contract"
    )
if "c2_contract" not in wire_handshake:
    violations.append(
        "core/protocol/c2-wire/src/handshake.rs: CRM tag and hash validation must use c2-contract"
    )
public_wire_validators = {
    "core/protocol/c2-wire/src/control.rs": public_validate_functions(wire_control),
    "core/protocol/c2-wire/src/handshake.rs": public_validate_functions(wire_handshake),
}
for validator_path, names in public_wire_validators.items():
    if names:
        violations.append(
            f"{validator_path}: remove public validation helpers {names}; call c2-contract directly"
        )
if re.search(r"pub(?:\([^)]*\))?\s+use\s+c2_contract", wire_control) or re.search(r"pub(?:\([^)]*\))?\s+use\s+c2_contract", wire_handshake):
    violations.append(
        "c2-wire: remove public c2-contract compatibility re-exports; call c2-contract directly"
    )
for fn_name in ("encode_call_control", "encode_call_control_into", "decode_call_control"):
    call_control_body = rust_fn_body(wire_control, fn_name)
    if call_control_body is None or "validate_call_route_key" not in call_control_body:
        violations.append(
            f"core/protocol/c2-wire/src/control.rs: {fn_name}() must validate with validate_call_route_key() so empty route keys and invalid route text are rejected"
        )
for fn_name in ("try_encode_reply_control", "decode_reply_control"):
    reply_control_body = rust_fn_body(wire_control, fn_name)
    if reply_control_body is None or "validate_call_route_key" not in reply_control_body:
        violations.append(
            f"core/protocol/c2-wire/src/control.rs: {fn_name}() must validate ReplyControl::RouteNotFound route text with validate_call_route_key()"
        )
    if reply_control_body and any(bad in reply_control_body for bad in ("unwrap(", "expect(", "let _ =")):
        violations.append(
            f"core/protocol/c2-wire/src/control.rs: {fn_name}() must propagate validation errors, not unwrap/expect/ignore them"
        )
reply_encode_sig = rust_fn_signature(wire_control, "try_encode_reply_control") or ""
if "Result" not in reply_encode_sig:
    violations.append(
        "core/protocol/c2-wire/src/control.rs: reply-control route validation requires try_encode_reply_control(...) -> Result<...>"
    )
legacy_reply_encode_sig = rust_fn_signature(wire_control, "encode_reply_control") or ""
if legacy_reply_encode_sig and "Result" not in legacy_reply_encode_sig:
    violations.append(
        "core/protocol/c2-wire/src/control.rs: remove the old infallible encode_reply_control(...) Rust API"
    )
legacy_reply_encode_body = rust_fn_body(wire_control, "encode_reply_control") or ""
if legacy_reply_encode_sig and "try_encode_reply_control" not in legacy_reply_encode_body:
    violations.append(
        "core/protocol/c2-wire/src/control.rs: any retained encode_reply_control(...) compatibility wrapper must delegate to try_encode_reply_control(...)"
    )
server_rs = Path("core/transport/c2-server/src/server.rs").read_text()
for match in re.finditer(r"(?<!try_)\bencode_reply_control\s*\(", server_rs):
    line = server_rs[: match.start()].count("\n") + 1
    violations.append(
        f"core/transport/c2-server/src/server.rs:{line}: production reply writers must use a fallible reply-control encoder; do not call legacy infallible encode_reply_control(...) directly"
    )
wire_ffi_rs = Path("sdk/python/native/src/wire_ffi.rs").read_text()
wire_ffi_reply_body = rust_fn_body(wire_ffi_rs, "encode_reply_control") or ""
if "try_encode_reply_control" not in wire_ffi_reply_body or ("?" not in wire_ffi_reply_body and "map_err" not in wire_ffi_reply_body):
    violations.append(
        "sdk/python/native/src/wire_ffi.rs: Python-visible encode_reply_control wrapper must call try_encode_reply_control() and map errors into PyResult"
    )

server_manifest = Path("core/transport/c2-server/Cargo.toml").read_text()
if "c2-contract" not in server_manifest:
    violations.append(
        "core/transport/c2-server/Cargo.toml: c2-server must depend on c2-contract for route fingerprint validation"
    )
server_dispatcher = Path("core/transport/c2-server/src/dispatcher.rs").read_text()
if "abi_hash" not in server_dispatcher or "signature_hash" not in server_dispatcher:
    violations.append(
        "core/transport/c2-server/src/dispatcher.rs: CrmRoute must store abi_hash and signature_hash"
    )
ipc_client = Path("core/transport/c2-ipc/src/client.rs").read_text()
ipc_sync_client = Path("core/transport/c2-ipc/src/sync_client.rs").read_text()
method_table_body = rust_struct_body(ipc_client, "MethodTable") or ""
if "abi_hash" not in method_table_body or "signature_hash" not in method_table_body:
    violations.append(
        "core/transport/c2-ipc/src/client.rs: MethodTable must store abi_hash and signature_hash"
    )
for fn_name in ("validate_route_contract", "route_contract"):
    body = rust_fn_body(ipc_client, fn_name) or ""
    if "abi_hash" not in body or "signature_hash" not in body:
        violations.append(
            f"core/transport/c2-ipc/src/client.rs: {fn_name}() must compare/project route hashes, not only CRM tags"
        )
sync_validate_sig = rust_fn_signature(ipc_sync_client, "validate_route_contract") or ""
sync_validate_body = rust_fn_body(ipc_sync_client, "validate_route_contract") or ""
sync_project_sig = rust_fn_signature(ipc_sync_client, "route_contract") or ""
sync_project_body = rust_fn_body(ipc_sync_client, "route_contract") or ""
if (
    "ExpectedRouteContract" not in sync_validate_sig
    or "inner.validate_route_contract(expected)" not in sync_validate_body
):
    violations.append(
        "core/transport/c2-ipc/src/sync_client.rs: SyncClient::validate_route_contract() must accept the canonical ExpectedRouteContract and delegate to IpcClient validation"
    )
if (
    "ExpectedRouteContract" not in sync_project_sig
    or "inner.route_contract(route_name)" not in sync_project_body
):
    violations.append(
        "core/transport/c2-ipc/src/sync_client.rs: SyncClient::route_contract() must project the canonical hash-bearing ExpectedRouteContract from IpcClient"
    )
client_ffi = Path("sdk/python/native/src/client_ffi.rs").read_text()
ffi_route_contract_body = rust_fn_body(client_ffi, "route_contract") or ""
if "abi_hash" not in ffi_route_contract_body or "signature_hash" not in ffi_route_contract_body:
    violations.append(
        "sdk/python/native/src/client_ffi.rs: PyRustClient.route_contract() must expose ABI and signature hashes"
    )
authority = Path("core/transport/c2-http/src/relay/authority.rs").read_text()
if "AttestedRouteContract" not in authority or "abi_hash" not in authority or "signature_hash" not in authority:
    violations.append(
        "core/transport/c2-http/src/relay/authority.rs: relay attestation must carry abi_hash and signature_hash from IPC handshake"
    )
register_local_variant_body = rust_enum_variant_body(authority, "RouteCommand", "RegisterLocal")
if register_local_variant_body is None:
    violations.append("core/transport/c2-http/src/relay/authority.rs: missing RouteCommand::RegisterLocal variant")
elif "abi_hash" not in register_local_variant_body or "signature_hash" not in register_local_variant_body:
    violations.append(
        "core/transport/c2-http/src/relay/authority.rs: RouteCommand::RegisterLocal must carry attested abi_hash and signature_hash to the commit path"
    )
register_local_body = rust_fn_body(authority, "register_local") or ""
if "abi_hash" not in register_local_body or "signature_hash" not in register_local_body:
    violations.append(
        "core/transport/c2-http/src/relay/authority.rs: register_local() must build hash-bearing RouteEntry from attested contract metadata"
    )
state_rs = Path("core/transport/c2-http/src/relay/state.rs").read_text()
commit_sig = rust_fn_signature(state_rs, "commit_register_upstream") or ""
commit_body = rust_fn_body(state_rs, "commit_register_upstream") or ""
if "abi_hash" not in commit_sig or "signature_hash" not in commit_sig:
    violations.append(
        "core/transport/c2-http/src/relay/state.rs: commit_register_upstream() signature must require abi_hash and signature_hash"
    )
if "RouteCommand::RegisterLocal" in commit_body and (
    "abi_hash" not in commit_body or "signature_hash" not in commit_body
):
    violations.append(
        "core/transport/c2-http/src/relay/state.rs: commit_register_upstream() must pass route hashes into RouteCommand::RegisterLocal"
    )
handle_register_body = rust_fn_body(router, "handle_register") or ""
if "commit_register_upstream" in handle_register_body and (
    "abi_hash" not in handle_register_body or "signature_hash" not in handle_register_body
):
    violations.append(
        "core/transport/c2-http/src/relay/router.rs: handle_register() must commit only attested hash-bearing route metadata"
    )
relay_server = Path("core/transport/c2-http/src/relay/server.rs").read_text()
command_register_body = rust_enum_variant_body(relay_server, "Command", "RegisterUpstream")
if "enum Command" in relay_server and command_register_body is None:
    violations.append(
        "core/transport/c2-http/src/relay/server.rs: Command::RegisterUpstream must exist or command-loop registration must be removed"
    )
elif command_register_body and (
    any(field in command_register_body for field in ("crm_ns", "crm_name", "crm_ver"))
    and ("abi_hash" not in command_register_body or "signature_hash" not in command_register_body)
):
    violations.append(
        "core/transport/c2-http/src/relay/server.rs: Command::RegisterUpstream cannot carry CRM triplet fields without route hashes"
    )
for fn_name in ("validate_register_command", "register_upstream"):
    body = rust_fn_body(relay_server, fn_name) or ""
    if (
        body
        and any(field in body for field in ("crm_ns", "crm_name", "crm_ver"))
        and ("abi_hash" not in body or "signature_hash" not in body)
    ):
        violations.append(
            f"core/transport/c2-http/src/relay/server.rs: {fn_name}() cannot carry CRM triplet fields without route hashes"
        )
if (
    "contract.abi_hash" not in relay_server
    or "contract.signature_hash" not in relay_server
):
    violations.append(
        "core/transport/c2-http/src/relay/server.rs: command-loop registration must derive attested hashes from IPC before commit"
    )
route_table = Path("core/transport/c2-http/src/relay/route_table.rs").read_text()
for helper_name in ("valid_route_name", "valid_crm_tag"):
    body = rust_fn_body(route_table, helper_name)
    if body is None:
        continue
    if "c2_contract" not in body:
        violations.append(
            f"core/transport/c2-http/src/relay/route_table.rs: {helper_name}() must delegate to c2-contract"
        )
    for stale in ("MAX_CALL_ROUTE_NAME_BYTES", "c2_wire::handshake", "trim()", "is_control"):
        if stale in body:
            violations.append(
                f"core/transport/c2-http/src/relay/route_table.rs: {helper_name}() keeps stale local validation detail {stale}"
            )
for path, text in {
    "core/transport/c2-http/src/relay/route_table.rs": route_table,
    "core/transport/c2-http/src/relay/authority.rs": authority,
}.items():
    for stale in (
        "c2_wire::handshake::validate_crm_tag",
        "MAX_HANDSHAKE_NAME_BYTES",
        "MAX_CALL_ROUTE_NAME_BYTES",
    ):
        if stale in text:
            violations.append(f"{path}: stale relay validation dependency {stale}")
test_support = Path("core/transport/c2-http/src/relay/test_support.rs").read_text()
if "abi_hash" not in test_support or "signature_hash" not in test_support:
    violations.append(
        "core/transport/c2-http/src/relay/test_support.rs: relay live-server fixtures must include abi_hash and signature_hash"
    )
sdk_boundary = Path("sdk/python/tests/unit/test_sdk_boundary.py").read_text()
if (
    '"c2_contract::validate_crm_tag" in source' not in sdk_boundary
    or '"c2_wire::handshake::validate_crm_tag" not in source' not in sdk_boundary
):
    violations.append(
        "sdk/python/tests/unit/test_sdk_boundary.py: boundary test must require c2_contract and reject c2_wire CRM-tag validation"
    )

for violation in violations:
    print(violation)
raise SystemExit(1 if violations else 0)
PY
```

Expected after migration: no output from the AST and Rust/PyO3 guardrails except the first diagnostic `rg` listing, which is for human inspection and must show only route-bound APIs. Python-visible methods named `call(...)` must not accept a route key positionally or by keyword; any three-positional-argument `.call(...)` in `sdk/python/src` or `sdk/python/tests` is treated as the old native route-name shape unless it is renamed out of `call(...)`. Rust/PyO3 named-route APIs must treat parameters named `route_name`, `name`, or `route` as route keys when the function is a call/probe/resolve/acquire/connect/validation entry point. No public named-route API may accept both an independent route key and `ExpectedRouteContract`; the route key must be derived from the expected contract. If a raw low-level test helper remains, it must have a name that includes `with_expected_contract`, must accept a complete expected contract or scalar contract payload in one call, and must not match the old public `call(route_name, method_name, data)` shape.
- Add native/Python tests with one IPC server exporting two routes: acquire a client for route `grid` with `ExpectedRouteContract(grid, ...)`, then attempt to call route `other` through the returned low-level object. The call must fail before wire dispatch, and server-side dispatch counters for `other` must remain zero.
- `core/transport/c2-http/src/client/control.rs` must include `abi_hash` and `signature_hash` in `ResolveCacheKey`, `resolve_matching_async(...)`, and the `/_resolve` query string. There must be no `ResolveCacheKey::Name`, public `resolve(name)`, or `resolve_async(name)` runtime surface. A cached response for one full `ExpectedRouteContract` must never be reused for another contract under the same route name.
- `core/transport/c2-http/src/client/client.rs` and `core/transport/c2-http/src/relay/router.rs` must add expected hash headers beside the existing expected CRM headers:

```text
x-c2-expected-abi-hash
x-c2-expected-signature-hash
```

Header parsing must reject a partial expected identity: if a route name is present and any expected CRM or hash field is supplied, all five expected fields must be supplied and valid.

- [x] **Step 10: Carry fingerprints through relay control plane and mesh**

Implementation status as of 2026-05-11:

- `core/transport/c2-http/src/relay/types.rs` defines the only peer full-sync
  wire shape as `FullSyncEnvelope { protocol_version, snapshot }`, with
  hash-bearing `FullSyncRoute` and `FullSyncTombstone` wrappers. Internal
  `RouteEntry`, `RouteTombstone`, and raw `FullSync` no longer derive
  `Serialize` or `Deserialize`.
- `ValidatedFullSync::try_from(FullSyncEnvelope)` rejects old and future
  versions, malformed peer snapshots, malformed route/tombstone fields,
  malformed hashes, and key-replayed route/tombstone hashes before any merge
  mutation. Empty old-version snapshots are rejected before merge.
- `RelayState::merge_snapshot(...)` accepts `ValidatedFullSync`; the raw
  `RouteTable` merge implementation is private to `route_table.rs`. Seed
  bootstrap and seed retry parse `FullSyncEnvelope`, convert to
  `ValidatedFullSync`, and only then call `merge_snapshot(...)`.
- `peer_handlers.rs::current_full_sync_envelope(...)` is the shared producer
  for `/_peer/join` and `/_peer/sync`, injects the relay's self
  `PeerSnapshot`, and serializes only the hash-bearing full-sync wrappers.
- `peer.rs::validate_route_state_envelope(...)` is the shared ingress gate for
  route-state peer messages. It gates `RouteAnnounce`, `RouteWithdraw`,
  `RelayLeave`, `DigestExchange`, and `DigestDiff` on the route-hash protocol
  version and validates sender identity plus hash-bearing digest payloads.
- `background.rs` validates peer `DigestDiff` response envelopes from both
  anti-entropy and dead-peer-probe repair before applying any route/tombstone
  mutation; the repair helper accepts only `ValidatedDigestDiffEntry`.
- `DigestEntry.hash`, `DigestDiffEntry::Active.hash`, and
  `DigestDiffEntry::Deleted.hash` use `RouteDigestHash` rather than `u64`.
  The digest binds state, `name`, `relay_id`, ABI/signature hashes, and the
  peer-visible compatibility fields while excluding local-only
  `server_id`, `server_instance_id`, and `ipc_address`.
- `sha2` and `hex` are optional `c2-http` dependencies enabled only by the
  `relay` feature. `cargo check --manifest-path core/Cargo.toml -p c2-http
  --no-default-features` passed after this migration.
- Focused verification after this step: `cargo test --manifest-path
  core/Cargo.toml -p c2-http --features relay -- --nocapture` passed with
  219 tests; `cargo check --manifest-path core/Cargo.toml -p c2-http
  --no-default-features` passed; guardrail scans found no raw
  `json::<FullSync>`, no raw `DigestDiffEntry` conversion into `RouteEntry`,
  no `DefaultHasher`/`u64` peer digest, no public raw `FullSync` merge
  signature, and no serde derive on internal `RouteEntry`, `RouteTombstone`,
  or raw `FullSync`.

Extend relay route types and request/response bodies:

- `RouteEntry`
- `RouteInfo`
- HTTP `/_register`
- HTTP `/_resolve` with required CRM tag and hash query fields
- HTTP `/_probe`
- relay data-plane expected headers
- relay control-client `ResolveCacheKey`
- command-loop upstream registration
- peer `RouteAnnounce`
- peer `RouteWithdraw`
- peer `DigestExchange`
- peer `DigestDiffEntry::Active`
- peer `DigestDiffEntry::Deleted`
- peer `RelayLeave` / internal `RemovePeerRoutes`
- versioned `FullSync` response payloads for `/_peer/join` and `/_peer/sync`
- seed bootstrap and seed retry full-sync merge paths
- anti-entropy route digest, digest exchange, and diff repair entries
- `gossip.rs` route announce construction
- Python `RustRelayControlClient` must not expose `resolve(name)`; any future
  route catalog projection must be explicitly separate from runtime resolve

Make full-sync version validation a type-level gate, not a convention at individual parse sites:

```rust
pub struct FullSyncEnvelope {
    pub protocol_version: u32,
    pub snapshot: FullSyncSnapshot,
}

pub struct FullSyncSnapshot {
    pub routes: Vec<FullSyncRoute>,
    #[serde(default)]
    pub tombstones: Vec<FullSyncTombstone>,
    pub peers: Vec<PeerSnapshot>,
}

pub struct FullSyncRoute {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub registered_at: f64,
    pub hash: RouteDigestHash,
}

pub struct FullSyncTombstone {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
    pub hash: RouteDigestHash,
}

pub struct ValidatedFullSync(FullSync);

pub enum FullSyncValidationError {
    RouteSnapshotTooOld { got: u32, need: u32 },
    UnsupportedRouteSnapshotVersion { got: u32, expected: u32 },
    InvalidRouteHash { name: String, relay_id: String },
    InvalidTombstoneHash { name: String, relay_id: String },
    InvalidPeerSnapshot { relay_id: String },
}

impl TryFrom<FullSyncEnvelope> for ValidatedFullSync {
    type Error = FullSyncValidationError;

    fn try_from(envelope: FullSyncEnvelope) -> Result<Self, Self::Error> {
        if envelope.protocol_version < ROUTE_HASH_FULL_SYNC_VERSION {
            return Err(FullSyncValidationError::RouteSnapshotTooOld {
                got: envelope.protocol_version,
                need: ROUTE_HASH_FULL_SYNC_VERSION,
            });
        }
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(FullSyncValidationError::UnsupportedRouteSnapshotVersion {
                got: envelope.protocol_version,
                expected: PROTOCOL_VERSION,
            });
        }
        let sync = validate_full_sync_hash_payload(envelope.snapshot)?;
        Ok(Self(sync))
    }
}
```

The versioned `FullSync` wire payload has exactly one public JSON shape: `FullSyncEnvelope { protocol_version, snapshot: FullSyncSnapshot }`. Do not serialize raw `FullSync`, raw `RouteEntry`, or raw `RouteTombstone` as peer-sync wire data after this task. `FullSyncRoute` and `FullSyncTombstone` are wire-only hash-bearing wrappers; `validate_full_sync_hash_payload(...)` validates and converts them into the internal `FullSync` used by the route-table merge. The route table does not need to store digest hashes after validation, but `validate_full_sync_hash_payload(...)` must reject malformed route hashes, malformed tombstone hashes, non-finite timestamps, invalid names/relay ids, invalid peer URLs, invalid CRM tags, and any route/tombstone whose `hash` does not match the canonical key-bound digest before any merge state changes. Full-sync version validation is strict: old versions and future versions both reject before merge unless a future plan explicitly defines a compatible version window with tests for every accepted version.

Do not special-case "empty" snapshots as safe. Current `RouteTable::merge_snapshot(...)` removes all existing PEER routes before applying the replacement set, and then merges peer URL/status metadata. That means `FullSyncEnvelope { protocol_version: old, snapshot: FullSyncSnapshot { routes: [], tombstones: [], peers: [...] } }` is still route-state affecting. If future relay mesh needs old-version peer-list-only compatibility, add a separate peer-list endpoint or payload type that cannot call `merge_snapshot(...)`.

Add one helper in `peer_handlers.rs` or `state.rs` for producing outbound snapshots so `/_peer/join` and `/_peer/sync` cannot drift. This helper must accept `&RelayState`, not a prebuilt raw `FullSync`, because the current relay must inject its own peer identity before hashing and serializing the snapshot:

```rust
pub fn current_full_sync_envelope(state: &RelayState) -> FullSyncEnvelope {
    let mut snapshot = state.full_snapshot();
    snapshot.ensure_self_peer(
        state.relay_id(),
        state.advertise_url(),
        state.local_route_count(),
    );

    FullSyncEnvelope {
        protocol_version: PROTOCOL_VERSION,
        snapshot: FullSyncSnapshot::from_internal(snapshot),
    }
}
```

The exact method names may follow the existing `RelayState` API, but the helper must prove this invariant in tests: a relay with only self-owned routes still emits a current-version full-sync envelope containing a valid self `PeerSnapshot`, and a receiving peer validates and merges that envelope. Do not rely on callers to remember self-peer injection before calling `FullSyncSnapshot::from_internal(...)`.

`FullSyncSnapshot::from_internal(...)` must compute `hash` for every active route and tombstone using the same key-bound digest helper used by `route_digest()` and `DigestDiffEntry`. It must scrub local-only active-route fields before serializing: `server_id`, `server_instance_id`, `ipc_address`, and `locality` are internal route-table state and must not appear in full-sync JSON.

Remove `Serialize` and `Deserialize` derives from internal `RouteEntry`, `RouteTombstone`, and raw `FullSync` once `FullSyncRoute`, `FullSyncTombstone`, and `FullSyncEnvelope` exist. If tests need to inspect JSON, serialize the wire wrappers instead. Keeping serde derives on the raw internal structs would leave a second supported peer-sync shape and would let future code bypass the hash-bearing wrapper by accident. If an internal type still needs clone/debug/equality derives, keep only those non-wire derives.

Add a shared route-state envelope validator in `peer.rs`, `types.rs`, or a small `peer_validation.rs` module so no route-state message can bypass the route-hash protocol bump. Do not implement this as handler-local checks only: `background.rs` also consumes peer `DigestDiff` responses and currently applies them outside `peer_handlers.rs`. The validator must consume the raw envelope and return a validated newtype so downstream mutation helpers cannot accidentally accept unvalidated peer wire data.

```rust
pub struct ValidatedRouteStateEnvelope(PeerEnvelope);

impl ValidatedRouteStateEnvelope {
    pub fn into_message(self) -> PeerMessage {
        self.0.message
    }
}

fn route_state_message_min_version(message: &PeerMessage) -> Option<u32> {
    match message {
        PeerMessage::RouteAnnounce { .. }
        | PeerMessage::RouteWithdraw { .. }
        | PeerMessage::RelayLeave { .. }
        | PeerMessage::DigestExchange { .. }
        | PeerMessage::DigestDiff { .. } => Some(ROUTE_HASH_PEER_VERSION),
        _ => None,
    }
}

pub fn validate_route_state_envelope(
    envelope: PeerEnvelope,
    expected_sender: Option<&str>,
) -> Result<ValidatedRouteStateEnvelope, PeerRouteStateError> {
    check_protocol_version(envelope.protocol_version)?;

    if let Some(expected_sender) = expected_sender {
        if envelope.sender_relay_id != expected_sender {
            return Err(PeerRouteStateError::UnexpectedSender);
        }
    }

    if let Some(min_version) = route_state_message_min_version(&envelope.message) {
        if envelope.protocol_version < min_version {
            return Err(PeerRouteStateError::RouteStateProtocolTooOld {
                got: envelope.protocol_version,
                need: min_version,
            });
        }
        validate_route_state_payload_hashes(&envelope.message)?;
    }

    Ok(ValidatedRouteStateEnvelope(envelope))
}
```

Every ingress point that can read route-state wire material, create repair responses, or call `RouteAuthority::execute(...)` with `AnnouncePeer`, `WithdrawPeer`, `RemovePeerRoutes`, or digest-diff repair commands must pass through `validate_route_state_envelope(...)` before route-table read/compare/write logic. This includes `handle_peer_announce(...)`, `handle_peer_digest(...)`, `handle_peer_leave(...)`, the anti-entropy response path in `background.rs`, and the dead-peer probe response path in `background.rs`. This is separate from rejecting future protocol versions.

`apply_digest_diff(...)` must not remain callable from background loops with raw `DigestDiff` entries. Either make it accept only validated entry types, or rename/scope it so the only public helper is:

```rust
fn apply_validated_digest_diff(
    state: &RelayState,
    sender_relay_id: &str,
    envelope: ValidatedRouteStateEnvelope,
) -> Result<(), PeerRouteStateError> {
    match envelope.into_message() {
        PeerMessage::DigestDiff { entries } => {
            for entry in validate_digest_diff_entries(entries)? {
                apply_validated_digest_diff_entry(state, sender_relay_id, entry)?;
            }
            Ok(())
        }
        _ => Err(PeerRouteStateError::UnexpectedMessage),
    }
}
```

The background anti-entropy loop and dead-peer probe loop must parse `PeerEnvelope`, call `validate_route_state_envelope(envelope, Some(peer_id))`, and then call only the validated diff helper. They must not call `apply_digest_diff(...)` on raw `resp.json::<PeerEnvelope>()` output.

Replace the existing `u64` digest hash with a deterministic protocol hash. Do not use `DefaultHasher` for peer-visible route state. Add a small route digest module or helper in `route_table.rs`:

Keep the SHA-256 implementation inside the `c2-http` relay feature; do not move
relay route-state digest code into `c2-contract`, because `c2-contract` owns
contract descriptors and expected-route validation, not relay tombstone or
anti-entropy semantics:

```toml
[features]
relay = [
    # existing relay feature members...
    "dep:sha2",
    "dep:hex",
]

[dependencies]
sha2 = { version = "0.10", optional = true }
hex = { version = "0.4", optional = true }
```

`cargo check -p c2-http --no-default-features` must not require relay-only
digest crates.

```rust
use sha2::Digest as _;

pub type RouteDigestHash = String; // lowercase 64-hex SHA-256

#[derive(serde::Serialize)]
struct CanonicalActiveRouteDigest<'a> {
    state: &'static str,
    name: &'a str,
    relay_id: &'a str,
    relay_url: &'a str,
    crm_ns: &'a str,
    crm_name: &'a str,
    crm_ver: &'a str,
    abi_hash: &'a str,
    signature_hash: &'a str,
    registered_at_bits: u64,
}

#[derive(serde::Serialize)]
struct CanonicalDeletedRouteDigest<'a> {
    state: &'static str,
    name: &'a str,
    relay_id: &'a str,
    removed_at_bits: u64,
}

fn stable_digest_hash<T: serde::Serialize>(value: &T) -> RouteDigestHash {
    let bytes = serde_json::to_vec(value).expect("canonical route digest serializes");
    let digest = sha2::Sha256::digest(bytes);
    hex::encode(digest)
}
```

`route_digest()` must return `HashMap<(String, String, bool), RouteDigestHash>`. Active route digest material must include only peer-visible route key, state, and compatibility fields: `state`, `name`, `relay_id`, `relay_url`, `crm_ns`, `crm_name`, `crm_ver`, `abi_hash`, `signature_hash`, and `registered_at_bits`. It must continue to exclude local-only `server_id`, `server_instance_id`, and `ipc_address`. Tombstone digest material must include `state`, `name`, `relay_id`, and `removed_at_bits`, with no local-only owner fields. Add golden-vector tests for at least one active route and one tombstone, plus regression tests proving the digest changes when `name`, `relay_id`, `abi_hash`, or `signature_hash` changes and stays stable when only IPC-only identity fields change.

Update `DigestEntry` and `DigestDiffEntry` to carry concrete hash-bearing wire fields. Do not leave this as a prose-only "preserve hashes" rule:

```rust
pub struct DigestEntry {
    pub name: String,
    pub relay_id: String,
    #[serde(default)]
    pub deleted: bool,
    pub hash: RouteDigestHash,
}

#[serde(tag = "state")]
pub enum DigestDiffEntry {
    Active {
        name: String,
        relay_id: String,
        relay_url: String,
        crm_ns: String,
        crm_name: String,
        crm_ver: String,
        abi_hash: String,
        signature_hash: String,
        registered_at: f64,
        hash: RouteDigestHash,
    },
    Deleted {
        name: String,
        relay_id: String,
        removed_at: f64,
        hash: RouteDigestHash,
    },
}
```

`DigestDiffEntry::Active.hash` is the stable digest of the same active-route material returned by `route_digest()`, including the entry's `name` and `relay_id`. `DigestDiffEntry::Deleted.hash` is the stable key-bound tombstone digest, including the entry's `name` and `relay_id`. The route table does not need to store these digest hashes, but validation must recompute or otherwise verify them before mutation so a malformed repair response cannot downgrade route metadata, replay a valid hash onto a different route key, or install an inconsistent tombstone.

Peer handlers must validate every current-version digest entry before comparing it: route name, relay id, and `hash` must be valid; invalid current-version digest messages are ignored without partial route-state mutation. Old-version `DigestExchange` is rejected before digest validation or tombstone repair. Current-version `DigestExchange` may still produce `DigestDiffEntry::Deleted` for authoritative-missing routes, but only after all digest entries pass validation.

Remove raw conversion paths from peer wire data to route entries. The current `impl TryFrom<DigestDiffEntry> for RouteEntry` must be deleted or made private to validated types; it is too easy for a caller to convert unvalidated, hashless peer wire data directly into route state. Replace it with a validated newtype:

```rust
pub enum ValidatedDigestDiffEntry {
    Active(ValidatedDigestDiffActive),
    Deleted(ValidatedDigestDiffDeleted),
}

pub struct ValidatedDigestDiffActive {
    pub name: String,
    pub relay_id: String,
    pub relay_url: String,
    pub crm_ns: String,
    pub crm_name: String,
    pub crm_ver: String,
    pub abi_hash: String,
    pub signature_hash: String,
    pub registered_at: f64,
}

pub struct ValidatedDigestDiffDeleted {
    pub name: String,
    pub relay_id: String,
    pub removed_at: f64,
}

impl TryFrom<ValidatedDigestDiffActive> for RouteEntry {
    type Error = RouteValidationError;

    fn try_from(active: ValidatedDigestDiffActive) -> Result<Self, Self::Error> {
        Ok(RouteEntry {
            name: active.name,
            relay_id: active.relay_id,
            relay_url: active.relay_url,
            server_id: None,
            server_instance_id: None,
            ipc_address: None,
            crm_ns: active.crm_ns,
            crm_name: active.crm_name,
            crm_ver: active.crm_ver,
            abi_hash: active.abi_hash,
            signature_hash: active.signature_hash,
            locality: Locality::Peer,
            registered_at: active.registered_at,
        })
    }
}
```

`validate_digest_diff_entries(...)` must reject any current-version `Active` that lacks valid `abi_hash`, valid `signature_hash`, valid route digest hash, finite `registered_at`, valid route name, valid relay id, valid relay URL, or valid CRM tag. It must reject any current-version `Deleted` that lacks valid route name, valid relay id, finite `removed_at`, or valid tombstone digest hash. It must validate the whole list before applying any entry so a single malformed entry cannot cause partial mutation.

`RelayState::merge_snapshot(...)` must accept `ValidatedFullSync`, not raw `FullSync`. The raw route-table implementation is `RouteTable::merge_snapshot_inner(FullSync)` and remains private to `route_table.rs`, so new seed/bootstrap code cannot accidentally bypass the version gate. Every production HTTP decode path must parse `FullSyncEnvelope`, convert to `ValidatedFullSync`, and only then call `RelayState::merge_snapshot(...)`. Unit tests inside `route_table.rs` may exercise the inner merge helper directly, but production code outside the route-table module must not.

The compile-time choke point is the primary guardrail: `RelayState::merge_snapshot(...)` accepts `ValidatedFullSync`, and any raw route-table merge helper remains private to `route_table.rs`. The repository scan below is a secondary regression fence and must not be weakened into file-wide string checks. For every production `.merge_snapshot(...)` call outside `state.rs` and the private route-table inner helper, the scan must inspect the enclosing Rust function body, prove the exact argument is either a direct `ValidatedFullSync::try_from(...)` expression or a single immutable local binding initialized by that conversion, and reject any shadowing or reassignment between validation and merge.

Validation rules:

- locally registered routes must have non-empty valid hashes;
- peer routes must have non-empty valid hashes before entering the table;
- resolve/probe filters must reject contract identity or hash mismatch;
- route table internal mutation must reject malformed hashes as defense-in-depth;
- peer gossip handlers must reject malformed or missing hashes before route table mutation;
- peer digest repair must use `ValidatedDigestDiffEntry`; raw `DigestDiffEntry` must not implement a public conversion into `RouteEntry`;
- `DigestDiffEntry::Active` must carry `abi_hash`, `signature_hash`, and a stable key-bound route digest hash; `DigestDiffEntry::Deleted` must carry a stable key-bound tombstone digest hash. Missing or malformed fields reject the entire diff batch before any mutation;
- the shared peer route-state validator must be the only entry point from raw `PeerEnvelope` route-state wire data to route mutation helpers. This applies equally to HTTP peer handlers and to `background.rs` response consumers for anti-entropy and dead-peer probes;
- `apply_digest_diff(...)` or its replacement must accept only validated digest entries / validated envelopes. Background loops must not parse `resp.json::<PeerEnvelope>()` and then directly execute `RouteCommand::AnnouncePeer` or `RouteCommand::WithdrawPeer` from raw `DigestDiff` entries;
- relay route digests must include ABI and signature hashes because every relay must observe identical fingerprint metadata for a given route;
- relay route digests must bind `name`, `relay_id`, and route `state` so a valid hash cannot be replayed onto a different route key;
- relay route digests must be lowercase SHA-256 hex over canonical digest material. `DefaultHasher` / `u64` hash output is not a wire protocol and must not remain in `DigestEntry`;
- relay peer `PROTOCOL_VERSION` must be bumped and peer handlers must reject route-state messages from older protocol versions, including `RouteAnnounce`, `RouteWithdraw`, `RelayLeave` / internal `RemovePeerRoutes`, full snapshots that contain routes or tombstones, `DigestExchange`, `DigestDiffEntry::Active`, and `DigestDiffEntry::Deleted`;
- the existing `check_protocol_version(...)` behavior that accepts lower peer versions must be replaced or supplemented with a route-message minimum-version check before deserializing into route-table mutation commands or computing digest repairs. The helper must treat deletion-only messages and `DigestExchange` as route-state messages; old-version tombstones are not safe just because they do not carry route hashes;
- `FullSync` must be wrapped in `FullSyncEnvelope { protocol_version, snapshot: FullSyncSnapshot }`; do not keep a public raw `FullSync` JSON response shape for either `/_peer/join` or `/_peer/sync`. Because current seed bootstrap and seed retry parse `FullSync` directly from join responses, update every direct parse in `server.rs` and `background.rs` to parse `FullSyncEnvelope` and require `ValidatedFullSync` before calling `merge_snapshot(...)`;
- current-version full sync payloads must validate active route ABI/signature hashes and key-bound route/tombstone digest hashes before merge; malformed full-sync routes or tombstones reject the whole snapshot before any peer route replacement or peer metadata merge;
- current-version full sync payloads must use `FullSyncRoute` / `FullSyncTombstone` wire wrappers. Raw `RouteEntry` and raw `RouteTombstone` are internal route-table types and must not be serialized as peer-sync JSON. This prevents local-only fields and missing hash fields from becoming a second supported wire shape;
- internal `RouteEntry`, `RouteTombstone`, and raw `FullSync` must not derive `Serialize` or `Deserialize` after the wrapper migration. JSON tests and production peer handlers must serialize only `FullSyncEnvelope`, `FullSyncRoute`, and `FullSyncTombstone`;
- full snapshots whose protocol version is lower than the route-hash protocol version must always be rejected before `merge_snapshot(...)`, even when `routes` and `tombstones` are both empty. They must not clear existing peer routes, merge peers, install tombstones, rewrite peer route URLs, or partially mutate route state;
- outbound `/_peer/join` and `/_peer/sync` snapshots must be produced through the shared `current_full_sync_envelope(&RelayState)` helper so self-peer injection, protocol version, and hash-bearing wrappers cannot drift between endpoints;
- full snapshots and validated digest repair conversion must preserve ABI/signature hashes while continuing to scrub local-only `server_id`, `server_instance_id`, and `ipc_address` fields from peer routes;
- Python relay route dictionaries must include `abi_hash` and `signature_hash` for diagnostics and tests.

- [x] **Step 11: Add direct IPC mismatch tests**

Add integration tests where two CRM contracts intentionally share `namespace`, class name, and version but differ in:

- method parameter type;
- return type;
- method set;
- read/write access;
- transferable field schema;
- declared transferable ABI id;
- declared transferable ABI schema;
- `Default*Transferable` annotation shape while keeping the same CRM namespace/name/version.

Expected behavior:

- thread-local connect rejects mismatches;
- direct IPC connect rejects mismatches with relay unset;
- explicit `ipc://` connect still does not consult relay settings;
- direct IPC client validation rejects a named route when expected hashes are missing or malformed;
- direct IPC client validation rejects a named route when the expected CRM tag matches but either expected hash differs;
- `c2_ipc::MethodTable`, `Client::route_contract(...)`, `SyncClient::route_contract(...)`, and `PyRustClient.route_contract(...)` all project ABI/signature hashes, and `Client::validate_route_contract(...)` cannot pass by comparing only CRM namespace/name/version;
- `RuntimeSession.acquire_ipc_client(...)` cannot be called for a non-empty route with only CRM namespace/name/version; it must receive `ExpectedRouteContract` or fail before returning a client;
- direct IPC and low-level HTTP client APIs reject named-route calls that do not provide a full `ExpectedRouteContract`;
- a low-level native IPC client acquired for one validated route cannot call a different route by passing another `route_name` to `call(...)`;
- `CRMProxy.call(...)` no longer passes `self._name` into native `.call(...)`; route identity is bound at native client construction or validated by an explicit `ExpectedRouteContract` helper;
- Python guardrails reject both positional and keyword forms of route-name calls, including `_client.call(self._name, ...)`, `_client.call(route_name=self._name, ...)`, `_client.call(name=self._name, ...)`, any three-positional-argument `.call(...)`, and stale test doubles such as `def call(self, route_name, method_name, data)`;
- no mismatched route reaches method dispatch.

Implementation status as of 2026-05-11:

- Added `sdk/python/tests/integration/test_direct_ipc_contract_validation.py`.
  It covers thread-local and explicit direct IPC mismatch rejection for the
  same CRM namespace/name/version across parameter type, return type, method
  set, read/write access, transferable ABI id, transferable ABI schema, and
  default annotation shape differences.
- The same integration file proves explicit `ipc://` connect remains
  relay-independent when `cc.set_relay_anchor("http://127.0.0.1:9")` points at
  an unavailable relay, and it asserts no mismatch path reaches resource method
  dispatch.
- The integration file checks direct IPC validation rejects missing or malformed
  expected hashes, and that raw plus route-bound `PyRustClient.route_contract`
  projections include both ABI and signature hashes.
- The route-bound native-client test verifies a client acquired for `grid`
  cannot call `other` using the old three-argument route-key shape.
- Added `sdk/python/tests/unit/test_sdk_boundary.py` guardrails for Python
  `.call(route_name=...)`, `.call(name=...)`, three-positional `.call(...)`,
  `def call(self, route_name, ...)`, and `CRMProxy.call(...)` passing
  `self._name` into native call surfaces.
- Verification:
  `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_direct_ipc_contract_validation.py sdk/python/tests/unit/test_sdk_boundary.py::test_python_crm_call_surfaces_do_not_accept_route_key_arguments sdk/python/tests/unit/test_sdk_boundary.py::test_crm_proxy_does_not_pass_route_name_into_native_call -q --timeout=30 -rs`
  passed with `20 passed`.
- Broader guardrail verification:
  `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_sdk_boundary.py -q --timeout=30 -rs`
  passed with `20 passed`.

- [x] **Step 12: Add relay mismatch tests**

Add relay integration coverage:

- relay `/_resolve` does not select a route whose identity matches but hashes differ, and wrong complete contracts return `404` rather than a name-only mismatch diagnostic;
- SDK relay registration sends complete expected route fingerprints, and a relay rejects registration when the connected IPC handshake advertises different ABI or signature hashes;
- any attestation-only upstream registration helper publishes only IPC-handshake-derived CRM/hash metadata and cannot accept empty default CRM-triplet claims as production SDK input;
- relay resolve cache does not reuse routes across different hash pairs for the same route and CRM tag;
- `ResolveCacheKey`, `resolve_matching_async(...)`, and relay `/_resolve` query parsing include both hashes; `ResolveCacheKey::Name`, `resolve(name)`, `resolve_async(name)`, Python `RustRelayControlClient.resolve(name)`, and missing-query `/_resolve` are rejected by guardrails/tests;
- relay-discovered local IPC rejects hash mismatch after IPC handshake;
- explicit HTTP relay connect rejects hash mismatch before data-plane call;
- `RuntimeSession::resolve_relay_connection(...)`, `connect_relay_http_client(...)`, and `connect_explicit_relay_http_client(...)` reject any call path that passes route name plus CRM triplet without `ExpectedRouteContract`;
- named `RelayAwareHttpClient` construction or route acquisition rejects missing, empty, or malformed `ExpectedRouteContract` fields before any resolve/data-plane request;
- `PyRelayConnectedClient` returned from relay name resolution is route-bound; its IPC and HTTP branches cannot accept a caller-supplied alternate route name after connect;
- every `relay_aware.rs` unit fixture uses explicit ABI/signature hash values, so no relay-aware client test exercises an unset expected contract for a named route;
- peer route gossip with malformed hashes is rejected before route table mutation;
- peer route gossip that omits hash fields is rejected after the protocol version bump;
- peer route gossip with a lower protocol version is rejected for route-mutating messages even if the message would otherwise deserialize;
- route digest golden-vector tests prove anti-entropy uses stable lowercase SHA-256 hex material and not `DefaultHasher`;
- route digest tests prove `name`, `relay_id`, `abi_hash`, and `signature_hash` change the digest while local-only `server_id`, `server_instance_id`, and `ipc_address` do not;
- digest diff validation rejects a replay attempt where a valid active or deleted `hash` is copied onto the same metadata under a different `name` or `relay_id`;
- current-version `DigestExchange` with malformed digest hashes is ignored without partial tombstone or route mutation;
- current-version `DigestDiffEntry::Active` missing `abi_hash`, missing `signature_hash`, malformed hashes, mismatched active digest hash, invalid route name, invalid relay id, invalid relay URL, invalid CRM tag, or non-finite `registered_at` is rejected before any route mutation;
- current-version `DigestDiffEntry::Deleted` missing or malformed tombstone digest hash, invalid route name, invalid relay id, or non-finite `removed_at` is rejected before any tombstone mutation;
- a malformed mixed `DigestDiff` batch with one valid entry and one invalid entry leaves route and tombstone state unchanged, proving validation is all-or-nothing;
- compile-time/API tests or `rg` guardrails prove there is no public `impl TryFrom<DigestDiffEntry> for RouteEntry`; production conversion must use `ValidatedDigestDiffEntry` / `ValidatedDigestDiffActive`;
- old-version `RouteWithdraw`, `DigestExchange`, `DigestDiffEntry::Deleted`, and `RelayLeave` / `RemovePeerRoutes` paths cannot remove a current hash-bearing route or install a tombstone;
- old-version `DigestExchange` specifically cannot trigger `authoritative_missing_tombstone(...)`, even when its digest claims an active route for this relay id that is missing locally;
- old-version `DigestDiff` responses received by `background.rs` anti-entropy cannot announce a route, withdraw a route, or install a tombstone, even if the peer HTTP response body otherwise deserializes successfully;
- old-version `DigestDiff` responses received by `background.rs` dead-peer probe repair cannot announce a route, withdraw a route, or install a tombstone;
- current-version `DigestDiff` responses received by background loops with malformed, missing, or non-lowercase hash fields are ignored without partial mutation;
- an `rg` or unit guardrail proves background loops no longer call raw `apply_digest_diff(...)` directly from parsed response envelopes; the only allowed background mutation path is through `validate_route_state_envelope(...)` plus the validated diff helper;
- versioned `/_peer/sync` responses with an old or future protocol version and route entries are rejected by seed bootstrap and seed retry before `merge_snapshot(...)`;
- versioned `/_peer/sync` and `/_peer/join` responses with old or future protocol version and tombstones are rejected before `merge_snapshot(...)`, even when `routes` is empty;
- versioned `/_peer/sync` and `/_peer/join` responses with old or future protocol version, empty `routes`, empty `tombstones`, and non-empty `peers` are rejected before `merge_snapshot(...)`; they must not clear existing peer routes, rewrite peer route URLs, or merge peer status;
- versioned old-protocol empty snapshots leave an existing peer route table byte-for-byte equivalent, including route count, relay URL, ABI hash, signature hash, and tombstone set;
- current-version full-sync snapshots with malformed route ABI/signature hashes, malformed route digest hashes, malformed tombstone digest hashes, key-replayed active/tombstone hashes, or one invalid entry in an otherwise valid snapshot reject the entire snapshot before peer routes are replaced;
- versioned `/_peer/sync` responses with current protocol version preserve ABI/signature hashes and still scrub local-only owner identity;
- versioned `/_peer/join` responses use the same `FullSyncEnvelope` shape as `/_peer/sync`; seed bootstrap and seed retry reject old-version join snapshots before `merge_snapshot(...)`;
- a relay with only self-owned routes emits `/_peer/join` and `/_peer/sync` envelopes that contain a valid self `PeerSnapshot`, and a receiving peer validates and merges that envelope without requiring callers to pre-inject self metadata;
- `handle_peer_join(...)`, `handle_peer_sync(...)`, and `current_full_sync_envelope(...)` tests assert the public JSON shape contains `protocol_version` plus `snapshot`, and does not expose raw top-level `routes` / `tombstones` / `peers`;
- `FullSyncEnvelope.snapshot.routes[*]` and `snapshot.tombstones[*]` tests assert the public JSON shape uses `FullSyncRoute` and `FullSyncTombstone` with `hash`, and does not serialize raw `RouteEntry` fields such as `server_id`, `server_instance_id`, `ipc_address`, or `locality`;
- compile-time/API tests or `rg` guardrails prove internal `RouteEntry`, `RouteTombstone`, and raw `FullSync` no longer derive `Serialize` or `Deserialize`; only the versioned wire wrappers are serde-visible;
- compile-time/API tests or `rg` guardrails prove no production code outside `route_table.rs` can call `merge_snapshot` with raw `FullSync`; all callers must provide `ValidatedFullSync`;
- peer digest repair preserves hashes and does not downgrade a route to CRM-triplet-only metadata;
- relay probe and data-plane calls reject partial expected headers, including cases where CRM headers are present without both hashes or one hash is missing.

Implementation status as of 2026-05-11:

- Added `core/transport/c2-http/src/client/control.rs`
  `resolve_matching_rejects_malformed_expected_contract_before_request`.
  The RED run showed the malformed ABI hash was encoded into the `/_resolve`
  request and attempted against `127.0.0.1:9`; the fix validates
  `ExpectedRouteContract` at `resolve_with_query_async(...)` before cache lookup
  or HTTP request construction.
- Added `core/transport/c2-http/src/client/relay_aware.rs`
  `construction_rejects_malformed_expected_contract_without_panic` and
  `new_with_control_rejects_malformed_expected_contract_without_panic`.
  `RelayAwareHttpClient::{new,new_with_control}` now return
  `HttpError::InvalidInput(...)` for malformed expected contracts instead of
  panicking through `expect(...)`.
- Updated `core/runtime/c2-runtime/src/session.rs` relay connectors to map
  relay-aware construction errors through `runtime_http_error(...)`, so
  `resolve_relay_connection(...)`, `connect_relay_http_client(...)`, and
  `connect_explicit_relay_http_client(...)` fail as structured runtime errors.
- Added relay router coverage for hash-aware resolve and partial data-plane
  headers:
  `resolve_with_expected_contract_hides_name_match_with_wrong_hash`,
  `resolve_requires_expected_contract_query`, and
  `data_plane_rejects_partial_expected_hash_headers_before_acquire`.
- Added `core/transport/c2-http/src/client/control.rs`
  `relay_control_client_does_not_expose_name_only_resolve` and
  `sdk/python/tests/unit/test_sdk_boundary.py`
  `test_relay_control_client_does_not_expose_name_only_resolve_to_python` so
  Rust and PyO3 cannot reintroduce runtime `resolve(name)`.
- Added `core/transport/c2-http/src/relay/route_table.rs`
  `name_only_resolve_helpers_are_test_only`; `RouteTable::resolve(name)` and
  `RelayState::resolve(name)` are available only under `#[cfg(test)]` for
  internal assertions, not as production runtime APIs.
- Existing relay-aware fixtures use explicit `TEST_ABI_HASH` and
  `TEST_SIGNATURE_HASH`; the focused review found no named relay-aware fixture
  constructing an unset expected contract.
- Verification:
  `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay -- --nocapture`
  passed with `233 passed`.
- Verification:
  `cargo test --manifest-path core/Cargo.toml -p c2-runtime`
  passed with `13 passed`.
- Verification:
  `cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features`
  passed.
- Verification:
  `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_http_relay.py::TestCcConnectHttp::test_connect_http_rejects_crm_contract_mismatch_before_call sdk/python/tests/integration/test_http_relay.py::TestCcConnectHttp::test_relay_call_rejects_invalid_expected_crm_header sdk/python/tests/integration/test_http_relay.py::TestCcConnectHttp::test_relay_local_ipc_contract_mismatch_does_not_fallback_to_http -q --timeout=30 -rs`
  passed with `3 passed`.

Add this raw-wire-shape guardrail beside the relay full-sync tests:

```bash
python - <<'PY'
import ast
import re
from pathlib import Path

paths = [
    Path("core/transport/c2-http/src/relay/types.rs"),
    Path("core/transport/c2-http/src/relay/route_table.rs"),
]
internal_wire_bypass = {"RouteEntry", "RouteTombstone", "FullSync"}
violations = []

for path in paths:
    text = path.read_text()
    for struct_name in internal_wire_bypass:
        for match in re.finditer(rf"(#\[derive\([^)]*\)\]\s*)+(?:pub(?:\(crate\))?\s+)?struct\s+{struct_name}\b", text, re.S):
            derive_block = match.group(0)
            if "Serialize" in derive_block or "Deserialize" in derive_block:
                line = text[: match.start()].count("\n") + 1
                violations.append(
                    f"{path}:{line}: {struct_name} must not derive Serialize/Deserialize; use FullSync* wire wrappers"
                )

if violations:
    print("\n".join(violations))
raise SystemExit(1 if violations else 0)
PY
```

Add this production-path full-sync guardrail beside the raw-wire-shape guardrail. The serde guard above only proves raw internal types are not public JSON shapes; this guard proves old raw producer/consumer paths cannot remain wired into the relay runtime:

```bash
python - <<'PY'
import re
from pathlib import Path

def rust_fn_signature(text: str, name: str) -> str:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{;]*",
        text,
        re.S,
    )
    return "" if match is None else " ".join(match.group(0).split())

def rust_fn_body(text: str, name: str) -> str:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return ""
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

relay_paths = {
    "peer_handlers": Path("core/transport/c2-http/src/relay/peer_handlers.rs"),
    "server": Path("core/transport/c2-http/src/relay/server.rs"),
    "background": Path("core/transport/c2-http/src/relay/background.rs"),
    "state": Path("core/transport/c2-http/src/relay/state.rs"),
    "route_table": Path("core/transport/c2-http/src/relay/route_table.rs"),
}
texts = {name: path.read_text() for name, path in relay_paths.items()}
relay_dir = Path("core/transport/c2-http/src/relay")

def compact_arg(value: str) -> str:
    return " ".join(value.strip().split())

def rust_function_ranges(text: str):
    for match in re.finditer(
        r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(?P<name>\w+)\b[^\{]*\{",
        text,
        re.S,
    ):
        depth = 1
        i = match.end()
        while i < len(text) and depth:
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
            i += 1
        yield {
            "name": match.group("name"),
            "item_start": match.start(),
            "body_start": match.end(),
            "item_end": i,
            "body": text[match.end(): i - 1],
        }

def enclosing_rust_fn(text: str, pos: int):
    candidates = [
        fn for fn in rust_function_ranges(text)
        if fn["item_start"] <= pos < fn["item_end"]
    ]
    if not candidates:
        return None
    return max(candidates, key=lambda fn: fn["item_start"])

def validated_merge_arg(text: str, call_start: int, arg: str) -> bool:
    arg = compact_arg(arg)
    if "ValidatedFullSync::try_from" in arg:
        return True
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", arg):
        return False
    fn = enclosing_rust_fn(text, call_start)
    if fn is None:
        return False
    prefix = text[fn["body_start"]:call_start]
    plain_bindings = list(re.finditer(
        rf"\blet\s+(?P<mutable>mut\s+)?{re.escape(arg)}\s*=\s*(?P<init>.*?);",
        prefix,
        re.S,
    ))
    result_let_else_bindings = list(re.finditer(
        rf"\blet\s+Ok\s*\(\s*(?P<mutable>mut\s+)?{re.escape(arg)}\s*\)\s*=\s*(?P<init>.*?);",
        prefix,
        re.S,
    ))
    bindings = plain_bindings + result_let_else_bindings
    if len(bindings) != 1:
        return False
    binding = bindings[0]
    if binding.group("mutable"):
        return False
    if "ValidatedFullSync::try_from" not in binding.group("init"):
        return False
    between_validation_and_merge = prefix[binding.end():]
    forbidden_patterns = [
        rf"\blet\s+(?:mut\s+)?{re.escape(arg)}\s*=",
        rf"(?<![A-Za-z0-9_]){re.escape(arg)}(?![A-Za-z0-9_])\s*(?:=|\+=|-=|\*=|/=|%=)",
        rf"&mut\s+{re.escape(arg)}\b",
        rf"std::mem::(?:replace|take)\s*\(\s*&mut\s+{re.escape(arg)}\b",
    ]
    return not any(
        re.search(pattern, between_validation_and_merge, re.S)
        for pattern in forbidden_patterns
    )

if re.search(r"fn\s+canonical_peer_snapshot\b", texts["peer_handlers"]):
    raise SystemExit("legacy canonical_peer_snapshot() helper must not reappear; use current_full_sync_envelope(...)")
if "Json(canonical_peer_snapshot" in texts["peer_handlers"]:
    raise SystemExit("peer handlers must return current_full_sync_envelope(...), not raw canonical_peer_snapshot JSON")
for path in relay_dir.glob("*.rs"):
    text = path.read_text()
    if path.name != "route_table.rs" and (
        "json::<FullSync>" in text
        or "json::<crate::relay::types::FullSync>" in text
    ):
        raise SystemExit(f"{path} parses raw FullSync from HTTP")
    if path.name in {"state.rs", "route_table.rs"}:
        continue
    for match in re.finditer(r"\.merge_snapshot\s*\((?P<arg>[^)]*)\)", text, re.S):
        arg = match.group("arg")
        if not validated_merge_arg(text, match.start(), arg):
            line = text[: match.start()].count("\n") + 1
            raise SystemExit(
                f"{path}:{line}: merge_snapshot({compact_arg(arg)}) is not mechanically tied to ValidatedFullSync::try_from(...)"
            )

state_merge_sig = rust_fn_signature(texts["state"], "merge_snapshot")
if "ValidatedFullSync" not in state_merge_sig:
    raise SystemExit("RelayState::merge_snapshot() must accept ValidatedFullSync, not raw FullSync")
if re.search(r"pub\s+fn\s+merge_snapshot\b[^\n{]*FullSync", texts["route_table"]):
    raise SystemExit("RouteTable raw FullSync merge helper must not be public; production callers must use RelayState::merge_snapshot(ValidatedFullSync)")
for name in ("handle_peer_join", "handle_peer_sync"):
    body = rust_fn_body(texts["peer_handlers"], name)
    if "FullSyncEnvelope" not in body and "current_full_sync_envelope" not in body:
        raise SystemExit(f"{name}() must publish the versioned full-sync envelope shape")
PY
```

- [x] **Step 13: Verify contract fingerprints**

```bash
cargo test --manifest-path core/Cargo.toml -p c2-contract -p c2-wire -p c2-ipc -p c2-server -p c2-runtime
cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features
cargo test --manifest-path core/Cargo.toml -p c2-http --features relay
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv sync --reinstall-package c-two
python tools/dev/c3_tool.py --build --link
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_wire.py sdk/python/tests/unit/test_crm_descriptor.py sdk/python/tests/unit/test_sdk_boundary.py sdk/python/tests/unit/test_crm_proxy.py sdk/python/tests/integration/test_direct_ipc_contract_validation.py sdk/python/tests/integration/ -q --timeout=30 -rs
```

Expected: Rust route metadata tests and Python direct IPC/relay mismatch tests pass.

Also verify the new foundation crate did not grow transport or async runtime
dependencies, and that the transport crates use it instead of retaining partial
validation authority:

```bash
python - <<'PY'
import ast
import re
from pathlib import Path

manifest = Path("core/foundation/c2-contract/Cargo.toml").read_text()
for forbidden in ("c2-wire", "c2-http", "c2-ipc", "tokio", "reqwest", "axum"):
    if forbidden in manifest:
        raise SystemExit(f"c2-contract must not depend on {forbidden}")

control = Path("core/protocol/c2-wire/src/control.rs").read_text()
handshake = Path("core/protocol/c2-wire/src/handshake.rs").read_text()

def rust_fn_body(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return None
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

def rust_fn_signature(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+{re.escape(name)}\b[^\{{;]*",
        text,
        re.S,
    )
    return None if match is None else " ".join(match.group(0).split())

def rust_struct_body(text: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?struct\s+{re.escape(name)}(?:<[^>]+>)?\s*\{{(?P<body>.*?)\n\}}",
        text,
        re.S,
    )
    return None if match is None else match.group("body")

def rust_item_body(text: str, keyword: str, name: str) -> str | None:
    match = re.search(
        rf"(?:pub(?:\([^)]*\))?\s+)?{re.escape(keyword)}\s+{re.escape(name)}\b[^\{{]*\{{",
        text,
    )
    if not match:
        return None
    depth = 1
    i = match.end()
    while i < len(text) and depth:
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
        i += 1
    return text[match.end(): i - 1]

def rust_enum_variant_body(text: str, enum_name: str, variant_name: str) -> str | None:
    enum_body = rust_item_body(text, "enum", enum_name)
    if enum_body is None:
        return None
    match = re.search(rf"\b{re.escape(variant_name)}\s*\{{", enum_body)
    if match is None:
        return None
    depth = 1
    i = match.end()
    while i < len(enum_body) and depth:
        if enum_body[i] == "{":
            depth += 1
        elif enum_body[i] == "}":
            depth -= 1
        i += 1
    return enum_body[match.end(): i - 1]

def public_validate_functions(text: str) -> list[str]:
    return sorted(
        set(re.findall(r"pub(?:\([^)]*\))?\s+fn\s+(validate_\w+)\b", text))
    )

def python_method_source(path: Path, class_name: str, method_name: str) -> str:
    text = path.read_text()
    tree = ast.parse(text)
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef) and node.name == class_name:
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)) and item.name == method_name:
                    return ast.get_source_segment(text, item) or ""
    return ""

def has_symbol(text: str, name: str) -> bool:
    return re.search(
        rf"(?<![A-Za-z0-9_]){re.escape(name)}(?![A-Za-z0-9_])",
        text,
    ) is not None

hash_bearing_carriers = {
    "ExpectedRouteContract": Path("core/foundation/c2-contract/src/lib.rs"),
    "RuntimeRouteSpec": Path("core/runtime/c2-runtime/src/outcome.rs"),
    "CrmRoute": Path("core/transport/c2-server/src/dispatcher.rs"),
    "AttestedRouteContract": Path("core/transport/c2-http/src/relay/authority.rs"),
    "TestRouteContract": Path("core/transport/c2-http/src/relay/test_support.rs"),
    "RegisterRequest": Path("core/transport/c2-http/src/client/control.rs"),
}

def hash_bearing_carrier_names(text: str) -> list[str]:
    names = []
    for type_name, path in hash_bearing_carriers.items():
        if not has_symbol(text, type_name) or not path.exists():
            continue
        carrier_body = rust_struct_body(path.read_text(), type_name) or ""
        if "abi_hash" in carrier_body and "signature_hash" in carrier_body:
            names.append(type_name)
    return names

def expected_route_contract_definitions() -> list[tuple[Path, int]]:
    definitions: list[tuple[Path, int]] = []
    for root in (Path("core"), Path("sdk/python/native/src"), Path("sdk/python/src")):
        if not root.exists():
            continue
        suffix = "*.py" if root == Path("sdk/python/src") else "*.rs"
        for path in root.rglob(suffix):
            text = path.read_text()
            patterns = [r"\bstruct\s+ExpectedRouteContract\b"]
            if path.suffix == ".py":
                patterns.append(r"\bclass\s+ExpectedRouteContract\b")
            for pattern in patterns:
                for match in re.finditer(pattern, text):
                    definitions.append((path, text[: match.start()].count("\n") + 1))
    return definitions

def accepts_valid_expected_contract(text: str, name: str, combined: str) -> bool:
    carrier_names = hash_bearing_carrier_names(combined)
    body = rust_fn_body(text, name) or ""
    if "ExpectedRouteContract" in carrier_names:
        return "abi_hash" in body and "signature_hash" in body
    if not has_symbol(combined, "expected_contract"):
        return False
    return (
        has_symbol(body, "ExpectedRouteContract")
        and "abi_hash" in body
        and "signature_hash" in body
    )

expected_contract_defs = expected_route_contract_definitions()
expected_contract_paths = [path.as_posix() for path, _line in expected_contract_defs]
if expected_contract_paths != ["core/foundation/c2-contract/src/lib.rs"]:
    rendered = ", ".join(f"{path}:{line}" for path, line in expected_contract_defs) or "<none>"
    raise SystemExit(
        "ExpectedRouteContract struct must be defined exactly once in "
        f"core/foundation/c2-contract/src/lib.rs; found {rendered}"
    )

native_server = Path("sdk/python/src/c_two/transport/server/native.py")
native_server_text = native_server.read_text()
register_crm_body = python_method_source(native_server, "NativeServerBridge", "register_crm")
for required in ("abi_hash", "signature_hash", "runtime_session.register_route"):
    if required not in register_crm_body:
        raise SystemExit(f"NativeServerBridge.register_crm() must produce and pass {required}")
if "_discover_methods(" in register_crm_body and "rpc_method_names" not in native_server_text:
    raise SystemExit("NativeServerBridge.register_crm() must use shared CRM method discovery used by descriptor hashing")
if "class CRMSlot" in native_server_text:
    crm_slot_match = re.search(r"class\s+CRMSlot\b[\s\S]*?(?=\nclass\s+|\ndef\s+|\Z)", native_server_text)
    crm_slot_body = "" if crm_slot_match is None else crm_slot_match.group(0)
    if "abi_hash" not in crm_slot_body or "signature_hash" not in crm_slot_body:
        raise SystemExit("CRMSlot must store abi_hash and signature_hash for thread-local validation")

runtime_ffi = Path("sdk/python/native/src/runtime_session_ffi.rs").read_text()
register_route_body = rust_fn_body(runtime_ffi, "register_route") or ""
for required in ("abi_hash", "signature_hash"):
    if required not in register_route_body:
        raise SystemExit(f"PyRuntimeSession.register_route() must accept/pass {required}")
if re.search(r"register_route[\s\S]{0,700}crm_ns\s*=\s*\"\"", runtime_ffi):
    raise SystemExit("PyRuntimeSession.register_route() must not keep empty CRM defaults for named registration")
runtime_spec = rust_struct_body(Path("core/runtime/c2-runtime/src/outcome.rs").read_text(), "RuntimeRouteSpec") or ""
if "abi_hash" not in runtime_spec or "signature_hash" not in runtime_spec:
    raise SystemExit("RuntimeRouteSpec must store abi_hash and signature_hash")
server_ffi = Path("sdk/python/native/src/server_ffi.rs").read_text()
build_route_body = rust_fn_body(server_ffi, "build_route") or ""
if "abi_hash" not in build_route_body or "signature_hash" not in build_route_body:
    raise SystemExit("PyServer::build_route() must construct CrmRoute with route hashes")
runtime_session = Path("core/runtime/c2-runtime/src/session.rs").read_text()
runtime_register_body = rust_fn_body(runtime_session, "register_route") or ""
if "abi_hash" not in runtime_register_body or "signature_hash" not in runtime_register_body:
    raise SystemExit("RuntimeSession::register_route() must compare and relay route hashes")

if re.search(r"pub(?:\([^)]*\))?\s+(?:const|use)[^\n]*MAX_CALL_ROUTE_NAME_BYTES", control):
    raise SystemExit("c2-wire control.rs still exposes MAX_CALL_ROUTE_NAME_BYTES compatibility surface")
if re.search(r"pub(?:\([^)]*\))?\s+(?:const|use)[^\n]*MAX_HANDSHAKE_NAME_BYTES", handshake):
    raise SystemExit("c2-wire handshake.rs still exposes MAX_HANDSHAKE_NAME_BYTES compatibility surface")
if "c2_contract" not in control or "c2_contract" not in handshake:
    raise SystemExit("c2-wire must delegate route-name, CRM-tag, and hash validation to c2-contract")
public_wire_validators = {
    "core/protocol/c2-wire/src/control.rs": public_validate_functions(control),
    "core/protocol/c2-wire/src/handshake.rs": public_validate_functions(handshake),
}
for validator_path, names in public_wire_validators.items():
    if names:
        raise SystemExit(
            f"{validator_path} must not expose public validation helpers {names}"
        )
if re.search(r"pub(?:\([^)]*\))?\s+use\s+c2_contract", control) or re.search(r"pub(?:\([^)]*\))?\s+use\s+c2_contract", handshake):
    raise SystemExit("c2-wire must not expose public c2-contract compatibility re-exports")
for fn_name in ("encode_call_control", "encode_call_control_into", "decode_call_control"):
    call_control_body = rust_fn_body(control, fn_name)
    if call_control_body is None or "validate_call_route_key" not in call_control_body:
        raise SystemExit(f"{fn_name}() must use validate_call_route_key()")
for fn_name in ("try_encode_reply_control", "decode_reply_control"):
    reply_control_body = rust_fn_body(control, fn_name)
    if reply_control_body is None or "validate_call_route_key" not in reply_control_body:
        raise SystemExit(f"{fn_name}() must validate RouteNotFound route text with validate_call_route_key()")
    if reply_control_body and any(bad in reply_control_body for bad in ("unwrap(", "expect(", "let _ =")):
        raise SystemExit(f"{fn_name}() must propagate validation errors, not unwrap/expect/ignore them")
reply_encode_sig = rust_fn_signature(control, "try_encode_reply_control") or ""
if "Result" not in reply_encode_sig:
    raise SystemExit("reply-control route validation requires try_encode_reply_control(...) -> Result<...>")
legacy_reply_encode_sig = rust_fn_signature(control, "encode_reply_control") or ""
if legacy_reply_encode_sig and "Result" not in legacy_reply_encode_sig:
    raise SystemExit("old infallible encode_reply_control(...) Rust API still exists")
legacy_reply_encode_body = rust_fn_body(control, "encode_reply_control") or ""
if legacy_reply_encode_sig and "try_encode_reply_control" not in legacy_reply_encode_body:
    raise SystemExit("retained encode_reply_control(...) wrapper must delegate to try_encode_reply_control(...)")
server_rs = Path("core/transport/c2-server/src/server.rs").read_text()
for match in re.finditer(r"(?<!try_)\bencode_reply_control\s*\(", server_rs):
    line = server_rs[: match.start()].count("\n") + 1
    raise SystemExit(
        f"c2-server reply writers must use a fallible reply-control encoder; legacy encode_reply_control(...) call remains at line {line}"
    )
wire_ffi_rs = Path("sdk/python/native/src/wire_ffi.rs").read_text()
wire_ffi_reply_body = rust_fn_body(wire_ffi_rs, "encode_reply_control") or ""
if "try_encode_reply_control" not in wire_ffi_reply_body or ("?" not in wire_ffi_reply_body and "map_err" not in wire_ffi_reply_body):
    raise SystemExit("wire_ffi encode_reply_control wrapper must call try_encode_reply_control() and map fallible Rust errors into PyResult")

http_control = Path("core/transport/c2-http/src/client/control.rs").read_text()
register_struct = re.search(
    r"struct\s+RegisterRequest(?:<[^>]+>)?\s*\{(?P<body>.*?)\n\}",
    http_control,
    re.S,
)
if register_struct is not None:
    register_fields = register_struct.group("body")
    register_body = rust_fn_body(http_control, "register")
    register_has_hashes = (
        ("expected_abi_hash" in register_fields and "expected_signature_hash" in register_fields)
        or ("abi_hash" in register_fields and "signature_hash" in register_fields)
    )
    if not register_has_hashes:
        raise SystemExit("RelayControlClient RegisterRequest/register() must carry expected route hashes or be split into an explicitly attestation-only helper")
    if register_body is not None and any(
        stale_default in register_body
        for stale_default in ('crm_ns: ""', 'crm_name: ""', 'crm_ver: ""')
    ):
        raise SystemExit("RelayControlClient register() must not synthesize empty CRM expectation defaults for named SDK registration")
http_ffi = Path("sdk/python/native/src/http_ffi.rs").read_text()
for match in re.finditer(
    r"(?:#\[pyo3\(signature\s*=\s*\((?P<pysig>.*?)\)\)\]\s*)?"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+"
    r"(?P<name>\w+)(?:<[^>]+>)?\s*\((?P<args>.*?)\)\s*(?:->|\{)",
    http_ffi,
    re.S,
):
    if match.group("name") != "register":
        continue
    args = " ".join((match.group("args") or "").split())
    pysig = " ".join((match.group("pysig") or "").split())
    combined = f"{args} {pysig}"
    register_has_hash_scalars = (
        has_symbol(combined, "expected_abi_hash")
        and has_symbol(combined, "expected_signature_hash")
    ) or (
        has_symbol(combined, "abi_hash")
        and has_symbol(combined, "signature_hash")
    )
    register_accepts_expected_contract = accepts_valid_expected_contract(http_ffi, "register", combined)
    if not (register_accepts_expected_contract or register_has_hash_scalars):
        raise SystemExit("PyRustRelayControlClient.register() must expose expected hashes or be renamed to an explicit attestation-only helper")
    if has_symbol(combined, "expected_contract") and not register_accepts_expected_contract:
        raise SystemExit("PyRustRelayControlClient.register() mentions expected_contract but does not convert it into canonical ExpectedRouteContract with route hashes")
    for field in ("crm_ns", "crm_name", "crm_ver", "expected_abi_hash", "expected_signature_hash"):
        if re.search(rf"(?<![A-Za-z0-9_]){re.escape(field)}\s*=\s*\"\"", pysig):
            raise SystemExit(f"PyRustRelayControlClient.register() must not default {field} to an empty string")

server_manifest = Path("core/transport/c2-server/Cargo.toml").read_text()
if "c2-contract" not in server_manifest:
    raise SystemExit("c2-server must depend on c2-contract")
server_dispatcher = Path("core/transport/c2-server/src/dispatcher.rs").read_text()
if "abi_hash" not in server_dispatcher or "signature_hash" not in server_dispatcher:
    raise SystemExit("c2-server CrmRoute must store route hashes")
ipc_client = Path("core/transport/c2-ipc/src/client.rs").read_text()
ipc_sync_client = Path("core/transport/c2-ipc/src/sync_client.rs").read_text()
method_table_body = rust_struct_body(ipc_client, "MethodTable") or ""
if "abi_hash" not in method_table_body or "signature_hash" not in method_table_body:
    raise SystemExit("c2-ipc MethodTable must store abi_hash and signature_hash")
for fn_name in ("validate_route_contract", "route_contract"):
    body = rust_fn_body(ipc_client, fn_name) or ""
    if "abi_hash" not in body or "signature_hash" not in body:
        raise SystemExit(f"c2-ipc Client::{fn_name}() must compare/project route hashes, not only CRM tags")
sync_validate_sig = rust_fn_signature(ipc_sync_client, "validate_route_contract") or ""
sync_validate_body = rust_fn_body(ipc_sync_client, "validate_route_contract") or ""
sync_project_sig = rust_fn_signature(ipc_sync_client, "route_contract") or ""
sync_project_body = rust_fn_body(ipc_sync_client, "route_contract") or ""
if (
    "ExpectedRouteContract" not in sync_validate_sig
    or "inner.validate_route_contract(expected)" not in sync_validate_body
):
    raise SystemExit("c2-ipc SyncClient::validate_route_contract() must accept the canonical ExpectedRouteContract and delegate to IpcClient validation")
if (
    "ExpectedRouteContract" not in sync_project_sig
    or "inner.route_contract(route_name)" not in sync_project_body
):
    raise SystemExit("c2-ipc SyncClient::route_contract() must project the canonical hash-bearing ExpectedRouteContract from IpcClient")
client_ffi = Path("sdk/python/native/src/client_ffi.rs").read_text()
ffi_route_contract_body = rust_fn_body(client_ffi, "route_contract") or ""
if "abi_hash" not in ffi_route_contract_body or "signature_hash" not in ffi_route_contract_body:
    raise SystemExit("PyRustClient.route_contract() must expose ABI and signature hashes")
authority = Path("core/transport/c2-http/src/relay/authority.rs").read_text()
if "AttestedRouteContract" not in authority or "abi_hash" not in authority or "signature_hash" not in authority:
    raise SystemExit("relay authority must attest and carry route hashes")
register_local_variant_body = rust_enum_variant_body(authority, "RouteCommand", "RegisterLocal")
if register_local_variant_body is None:
    raise SystemExit("relay authority must define RouteCommand::RegisterLocal")
if "abi_hash" not in register_local_variant_body or "signature_hash" not in register_local_variant_body:
    raise SystemExit("RouteCommand::RegisterLocal must carry attested abi_hash and signature_hash")
register_local_body = rust_fn_body(authority, "register_local") or ""
if "abi_hash" not in register_local_body or "signature_hash" not in register_local_body:
    raise SystemExit("relay authority register_local() must build hash-bearing RouteEntry")
state_rs = Path("core/transport/c2-http/src/relay/state.rs").read_text()
commit_sig = rust_fn_signature(state_rs, "commit_register_upstream") or ""
commit_body = rust_fn_body(state_rs, "commit_register_upstream") or ""
if "abi_hash" not in commit_sig or "signature_hash" not in commit_sig:
    raise SystemExit("RelayState::commit_register_upstream() must require abi_hash and signature_hash")
if "RouteCommand::RegisterLocal" in commit_body and ("abi_hash" not in commit_body or "signature_hash" not in commit_body):
    raise SystemExit("RelayState::commit_register_upstream() must pass route hashes into RouteCommand::RegisterLocal")
router = Path("core/transport/c2-http/src/relay/router.rs").read_text()
handle_register_body = rust_fn_body(router, "handle_register") or ""
if "commit_register_upstream" in handle_register_body and ("abi_hash" not in handle_register_body or "signature_hash" not in handle_register_body):
    raise SystemExit("relay handle_register() must commit only attested hash-bearing route metadata")
relay_server = Path("core/transport/c2-http/src/relay/server.rs").read_text()
command_register_body = rust_enum_variant_body(relay_server, "Command", "RegisterUpstream")
if "enum Command" in relay_server and command_register_body is None:
    raise SystemExit("Command::RegisterUpstream must exist or command-loop registration must be removed")
if command_register_body and (
    any(field in command_register_body for field in ("crm_ns", "crm_name", "crm_ver"))
    and ("abi_hash" not in command_register_body or "signature_hash" not in command_register_body)
):
    raise SystemExit("Command::RegisterUpstream cannot carry CRM triplet fields without route hashes")
for fn_name in ("validate_register_command", "register_upstream"):
    body = rust_fn_body(relay_server, fn_name) or ""
    if (
        body
        and any(field in body for field in ("crm_ns", "crm_name", "crm_ver"))
        and ("abi_hash" not in body or "signature_hash" not in body)
    ):
        raise SystemExit(f"relay server {fn_name}() cannot carry CRM triplet fields without route hashes")
if (
    "contract.abi_hash" not in relay_server
    or "contract.signature_hash" not in relay_server
):
    raise SystemExit("relay server command-loop registration must derive attested hashes from IPC before commit")
route_table = Path("core/transport/c2-http/src/relay/route_table.rs").read_text()
for helper_name in ("valid_route_name", "valid_crm_tag"):
    body = rust_fn_body(route_table, helper_name)
    if body is None:
        continue
    if "c2_contract" not in body:
        raise SystemExit(f"relay route_table {helper_name}() must delegate to c2-contract")
    for stale in ("MAX_CALL_ROUTE_NAME_BYTES", "c2_wire::handshake", "trim()", "is_control"):
        if stale in body:
            raise SystemExit(f"relay route_table {helper_name}() keeps stale local validation detail {stale}")
for path, text in {
    "core/transport/c2-http/src/relay/route_table.rs": route_table,
    "core/transport/c2-http/src/relay/authority.rs": authority,
}.items():
    for stale in (
        "c2_wire::handshake::validate_crm_tag",
        "MAX_HANDSHAKE_NAME_BYTES",
        "MAX_CALL_ROUTE_NAME_BYTES",
    ):
        if stale in text:
            raise SystemExit(f"{path} keeps stale relay validation dependency {stale}")
test_support = Path("core/transport/c2-http/src/relay/test_support.rs").read_text()
if "abi_hash" not in test_support or "signature_hash" not in test_support:
    raise SystemExit("relay test_support live-server fixtures must include route hashes")
sdk_boundary = Path("sdk/python/tests/unit/test_sdk_boundary.py").read_text()
if (
    '"c2_contract::validate_crm_tag" in source' not in sdk_boundary
    or '"c2_wire::handshake::validate_crm_tag" not in source' not in sdk_boundary
):
    raise SystemExit("sdk boundary test must require c2_contract and reject c2_wire CRM-tag validation")
PY
```

Implementation status as of 2026-05-11:

- `c2-contract`, `c2-wire`, `c2-ipc`, `c2-server`, and `c2-runtime` route
  fingerprint tests pass together. `c2-wire` now keeps route-key/CRM/hash
  validators private and delegated to `c2-contract`; reply route-not-found
  encoding is fallible through `try_encode_reply_control(...)`.
- `c2-http` relay tests pass with the hash-aware relay resolve, required
  expected-contract data-plane/probe headers, peer route-state, stale-owner, and
  raw HTTP public-surface guardrails. The raw `HttpClient` no longer exposes
  public named-route CRM call/probe methods without an `ExpectedRouteContract`,
  and crate-internal HTTP call/probe helpers no longer accept optional expected
  contracts or independent route names.
- The static foundation/validation guard above was extracted from this document
  and executed successfully after the implementation. It proves the canonical
  `ExpectedRouteContract` definition is unique, registration producers and
  sinks carry both hashes, route-table and authority validation use
  `c2-contract`, `SyncClient` projections are hash-bearing, relay registration
  derives or explicitly supplies hashes, and SDK boundary tests reject stale
  `c2_wire::handshake::validate_crm_tag` dependencies.
- Verification evidence:
  - `cargo test --manifest-path core/Cargo.toml -p c2-contract -p c2-wire -p c2-ipc -p c2-server -p c2-runtime` passed.
  - `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay -- --nocapture` passed with `233 passed`.
  - `cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features` passed.
  - `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv sync --reinstall-package c-two` rebuilt the native extension.
  - `python tools/dev/c3_tool.py --build --link` rebuilt and linked local `c3`.
  - `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs` passed with `808 passed, 2 skipped`.

- [ ] **Step 14: Commit Task 3 (manual integration step, not executed here)**

```bash
git add core sdk/python docs
git commit -m "feat: enforce route ABI and signature fingerprints"
```

This step is intentionally left unchecked in this working-tree remediation pass
because committing requires an explicit user request. It is not an implementation
gap: the code, tests, and guardrails above have been verified against the dirty
worktree.

## Task 4: Final Boundary Regression Pass

- [x] **Step 1: Run the implementation-reality closure guard**

Run this before the normal test suite. It is intentionally biased toward the old bypass shapes observed in the current repository; if it fails, do not mark the remediation complete even if unit tests pass.

```bash
python - <<'PY'
from pathlib import Path
import ast
import re

violations: list[str] = []

def read(path: str) -> str:
    return Path(path).read_text()

def require_file(path: str, message: str) -> None:
    if not Path(path).exists():
        violations.append(message)

require_file(
    "core/foundation/c2-contract/Cargo.toml",
    "missing c2-contract foundation crate; validation ownership is not closed",
)

for path, struct_name in [
    ("core/runtime/c2-runtime/src/outcome.rs", "RuntimeRouteSpec"),
    ("core/transport/c2-server/src/dispatcher.rs", "CrmRoute"),
    ("core/protocol/c2-wire/src/handshake.rs", "RouteInfo"),
    ("core/transport/c2-ipc/src/client.rs", "MethodTable"),
]:
    text = read(path)
    if "abi_hash" not in text or "signature_hash" not in text:
        violations.append(f"{path}: {struct_name} must carry abi_hash and signature_hash")

runtime_ffi = read("sdk/python/native/src/runtime_session_ffi.rs")
for stale in [
    'route_name: Option<&str>',
    'route_name="",',
    'expected_crm_ns=""',
    'expected_crm_name=""',
    'expected_crm_ver=""',
]:
    if stale in runtime_ffi:
        violations.append(f"sdk/python/native/src/runtime_session_ffi.rs: remove stale named-route default/bypass shape {stale}")

client_ffi = read("sdk/python/native/src/client_ffi.rs")
if re.search(r"fn\s+call\b[\s\S]{0,300}\broute_name\b", client_ffi):
    violations.append("sdk/python/native/src/client_ffi.rs: PyRustClient.call(...) must not accept route_name")

proxy = read("sdk/python/src/c_two/transport/client/proxy.py")
if re.search(r"\._client\.call\s*\(\s*self\._name\b", proxy, re.S):
    violations.append("sdk/python/src/c_two/transport/client/proxy.py: CRMProxy.call(...) still passes self._name into native call")

native_server = read("sdk/python/src/c_two/transport/server/native.py")
discover = re.search(r"def\s+_discover_methods\b[\s\S]{0,800}", native_server)
if discover and "rpc_method_names" not in discover.group(0):
    violations.append("sdk/python/src/c_two/transport/server/native.py: _discover_methods must delegate to c_two.crm.methods.rpc_method_names")

transferable = read("sdk/python/src/c_two/crm/transferable.py")
if "DEFAULT_PICKLE_PROTOCOL" not in transferable:
    violations.append("sdk/python/src/c_two/crm/transferable.py: missing DEFAULT_PICKLE_PROTOCOL for built-in pickle ABI")
transferable_tree = ast.parse(transferable)
for node in ast.walk(transferable_tree):
    if not isinstance(node, ast.Call):
        continue
    func = node.func
    is_pickle_dumps = (
        isinstance(func, ast.Attribute)
        and func.attr == "dumps"
        and isinstance(func.value, ast.Name)
        and func.value.id == "pickle"
    )
    if not is_pickle_dumps:
        continue
    protocol_kw = next((kw for kw in node.keywords if kw.arg == "protocol"), None)
    if protocol_kw is None:
        violations.append(
            "sdk/python/src/c_two/crm/transferable.py: pickle.dumps(...) must pass protocol=DEFAULT_PICKLE_PROTOCOL"
        )
        continue
    if not (
        isinstance(protocol_kw.value, ast.Name)
        and protocol_kw.value.id == "DEFAULT_PICKLE_PROTOCOL"
    ):
        violations.append(
            "sdk/python/src/c_two/crm/transferable.py: pickle.dumps(...) must use protocol=DEFAULT_PICKLE_PROTOCOL"
        )

relay_types = read("core/transport/c2-http/src/relay/types.rs")
if re.search(r"impl\s+TryFrom\s*<\s*DigestDiffEntry\s*>\s+for\s+RouteEntry", relay_types):
    violations.append("core/transport/c2-http/src/relay/types.rs: raw DigestDiffEntry must not convert directly into RouteEntry")
for struct_name in ("RouteEntry", "RouteTombstone", "FullSync"):
    match = re.search(r"#\[derive\((?P<derives>[^)]*)\)\]\s*pub\s+struct\s+" + struct_name + r"\b", relay_types, re.S)
    if match and ("Serialize" in match.group("derives") or "Deserialize" in match.group("derives")):
        violations.append(f"core/transport/c2-http/src/relay/types.rs: internal {struct_name} must not derive Serialize/Deserialize")

for path in [
    "core/transport/c2-http/src/relay/background.rs",
    "core/transport/c2-http/src/relay/server.rs",
    "core/transport/c2-http/src/relay/peer_handlers.rs",
]:
    if "json::<FullSync>" in read(path):
        violations.append(f"{path}: production code must parse FullSyncEnvelope, not raw FullSync")

state = read("core/transport/c2-http/src/relay/state.rs")
if re.search(r"fn\s+merge_snapshot\b[^\n{]*(?<!Validated)\bFullSync\b", state):
    violations.append("core/transport/c2-http/src/relay/state.rs: RelayState::merge_snapshot(...) must accept ValidatedFullSync")

route_table = read("core/transport/c2-http/src/relay/route_table.rs")
if "DefaultHasher" in route_table:
    violations.append("core/transport/c2-http/src/relay/route_table.rs: relay digests must not use DefaultHasher")
if "c2_wire::control::MAX_CALL_ROUTE_NAME_BYTES" in route_table:
    violations.append("core/transport/c2-http/src/relay/route_table.rs: route validation must use c2-contract, not c2-wire limits")

http_client = read("core/transport/c2-http/src/client/client.rs")
http_client_production = http_client.split("#[cfg(test)]", 1)[0]
for forbidden in [
    "pub fn call(\n        &self,\n        route_name: &str,",
    "pub async fn call_async(\n        &self,\n        route_name: &str,",
    "pub async fn probe_route_async(&self, route_name: &str)",
    "expected: Option<&ExpectedRouteContract>",
    "call_with_expected_crm_async(\n        &self,\n        route_name: &str,",
    "probe_route_with_expected_crm_async(\n        &self,\n        route_name: &str,",
]:
    if forbidden in http_client_production:
        violations.append(
            "core/transport/c2-http/src/client/client.rs: HTTP data-plane helpers must derive route from a required ExpectedRouteContract"
        )

relay_aware = read("core/transport/c2-http/src/client/relay_aware.rs")
relay_aware_production = relay_aware.split("#[cfg(test)]", 1)[0]
if "resolve_matching_async(&self.expected)" not in relay_aware_production:
    violations.append("core/transport/c2-http/src/client/relay_aware.rs: dispatch clients must use typed relay resolve")
for stale in [
    ".resolve_async(self.route_name())",
    ".resolve_async(&self.route_name",
    ".probe_route_with_expected_crm_async(self.route_name()",
    ".call_with_expected_crm_async(self.route_name()",
]:
    if stale in relay_aware_production:
        violations.append(f"core/transport/c2-http/src/client/relay_aware.rs: remove name-only dispatch path {stale}")

relay_control = read("core/transport/c2-http/src/client/control.rs")
relay_control_production = relay_control.split("#[cfg(test)]", 1)[0]
for forbidden in [
    "pub fn resolve(&self, name: &str)",
    "pub async fn resolve_async(&self, name: &str)",
    "ResolveCacheKey::Name",
    "expected: Option<&ExpectedRouteContract>",
    "None => format!(\"/_resolve/{}\"",
]:
    if forbidden in relay_control_production:
        violations.append(
            "core/transport/c2-http/src/client/control.rs: relay runtime resolve must require a complete ExpectedRouteContract"
        )

relay_router = read("core/transport/c2-http/src/relay/router.rs")
resolve_handler = re.search(
    r"async\s+fn\s+handle_resolve\b[\s\S]*?\n}\n\n/// `GET /_peers`",
    relay_router,
)
if not resolve_handler:
    violations.append("core/transport/c2-http/src/relay/router.rs: handle_resolve(...) guard could not locate function body")
else:
    body = resolve_handler.group(0)
    for required in ("query.crm_ns", "query.crm_name", "query.crm_ver", "query.abi_hash", "query.signature_hash"):
        if required not in body:
            violations.append("core/transport/c2-http/src/relay/router.rs: /_resolve must require complete CRM tag and hash query fields")
    if "state.resolve(&name)" in body or "StatusCode::CONFLICT" in body:
        violations.append("core/transport/c2-http/src/relay/router.rs: /_resolve must not keep a name-only mismatch diagnostic fallback")
expected_headers = re.search(
    r"fn\s+expected_crm_from_headers\b[\s\S]*?\n}\n\nfn\s+crm_contract_mismatch_response\b",
    relay_router,
)
if not expected_headers:
    violations.append("core/transport/c2-http/src/relay/router.rs: expected_crm_from_headers(...) guard could not locate function body")
else:
    body = expected_headers.group(0)
    if "Result<Option<c2_contract::ExpectedRouteContract>" in body or "Ok(None)" in body:
        violations.append("core/transport/c2-http/src/relay/router.rs: relay data-plane expected headers must be required, not optional")
    if "expected CRM headers and hash headers are required" not in body:
        violations.append("core/transport/c2-http/src/relay/router.rs: missing expected headers must be rejected before route lookup")
for fn_name in ("validate_expected_crm_for_route", "validate_expected_crm_for_acquired_route"):
    match = re.search(r"fn\s+" + fn_name + r"\b[\s\S]*?\n}\n", relay_router)
    if not match:
        violations.append(f"core/transport/c2-http/src/relay/router.rs: missing {fn_name}(...)")
        continue
    body = match.group(0)
    if "Option<c2_contract::ExpectedRouteContract>" in body or "let Some(expected)" in body:
        violations.append(f"core/transport/c2-http/src/relay/router.rs: {fn_name}(...) must not treat missing expected contract as success")

combined_relay = "\n".join(
    read(path)
    for path in [
        "core/transport/c2-http/src/relay/authority.rs",
        "core/transport/c2-http/src/relay/state.rs",
        "core/transport/c2-http/src/relay/conn_pool.rs",
    ]
)
if "can_replace_owner_token" in combined_relay:
    violations.append("relay stale-owner replacement still exposes token-only can_replace_owner_token(...)")
for field in ("owner_generation", "owner_lease_epoch", "OwnerReplacementEvidence", "replace_if_owner_token"):
    if field not in combined_relay:
        violations.append(f"relay stale-owner replacement missing required fence field/API {field}")

wire_control = read("core/protocol/c2-wire/src/control.rs")
wire_handshake = read("core/protocol/c2-wire/src/handshake.rs")
for path, text, stale in [
    ("core/protocol/c2-wire/src/control.rs", wire_control, "MAX_CALL_ROUTE_NAME_BYTES"),
    ("core/protocol/c2-wire/src/handshake.rs", wire_handshake, "MAX_HANDSHAKE_NAME_BYTES"),
]:
    if re.search(r"pub(?:\([^)]*\))?\s+(?:const|use)[^\n]*" + stale, text):
        violations.append(f"{path}: remove public {stale} compatibility surface")
if re.search(r"pub(?:\([^)]*\))?\s+fn\s+validate_crm_tag\b", wire_handshake):
    violations.append("core/protocol/c2-wire/src/handshake.rs: public validate_crm_tag must move to c2-contract")

for violation in violations:
    print(violation)
raise SystemExit(1 if violations else 0)
PY
```

Expected: after the remediation is implemented, this prints no violations. On the current pre-remediation repository it is expected to fail, and those failures are the exact closure work this plan tracks.

Status as of 2026-05-11: passed with no output after extraction from this
markdown file and direct execution. The guard itself was tightened to avoid
false positives from Python comments and Rust `ValidatedFullSync` substrings
while still rejecting old public raw HTTP named-route CRM/probe methods,
crate-internal `Option<&ExpectedRouteContract>` HTTP data-plane helpers,
independent `route_name + expected` helper signatures, name-only relay-aware
dispatch resolve, Rust/PyO3 name-only runtime `resolve(name)`, `/_resolve`
missing-query or name-only mismatch fallback, and relay router handlers that
treat missing expected headers as success.

- [x] **Step 2: Run Rust workspace tests**

```bash
cargo test --manifest-path core/Cargo.toml --workspace
```

Expected: all Rust workspace tests pass.

Status as of 2026-05-11: passed. The run covered `c2-config`, `c2-contract`,
`c2-error`, `c2-http`, `c2-ipc`, `c2-mem`, `c2-runtime`, `c2-server`,
`c2-wire`, and doctests.

- [x] **Step 3: Reinstall native extension**

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv sync --reinstall-package c-two
```

Expected: maturin rebuilds `c_two._native` without PyO3 compile errors.

Status as of 2026-05-11: passed; `c-two==0.4.10` was rebuilt from the local
checkout and reinstalled.

- [x] **Step 4: Rebuild local c3**

```bash
python tools/dev/c3_tool.py --build --link
```

Expected: local `c3 relay` binary is rebuilt from current Rust code.

Status as of 2026-05-11: passed; `~/.cargo/bin/c3` was linked to the
current `cli/target/debug/c3`.

- [x] **Step 5: Run Python suite**

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs
```

Expected: full Python suite passes.

Status as of 2026-05-11: passed with `807 passed, 2 skipped`.

- [x] **Step 6: Run direct IPC no-relay guard**

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS=http://127.0.0.1:9 uv run pytest sdk/python/tests/integration/test_direct_ipc_control.py sdk/python/tests/integration/test_buffer_lease_ipc.py::test_client_hold_shm_response_counts_and_releases_without_relay -q --timeout=30 -rs
```

Expected: explicit `ipc://` paths remain relay-independent.

Status as of 2026-05-11: passed with `12 passed` when run outside the filesystem
sandbox. The sandboxed run failed earlier because Python `shm_open` was denied
with `Operation not permitted`; a standard-library `multiprocessing.shared_memory`
probe reproduced the same sandbox permission failure, so the elevated rerun is
the valid implementation evidence.

- [x] **Step 7: Run formatting and diff checks**

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
git diff --check
```

Expected: formatting and whitespace checks pass.

Status as of 2026-05-11: passed for both `cargo fmt --manifest-path
core/Cargo.toml --all -- --check` and `git diff --check`.

## Self-Review Checklist

This checklist is a closure gate, not an advisory appendix. A remediation pass
is not complete until every applicable item is checked against the repository
state. Any unchecked implementation item remains a blocking unimplemented item
with owner, scope, and reason; the only unchecked non-implementation item in
this pass is the explicit manual commit step above. Do not mark the plan
complete after fixing only one
edge of an issue; for example, route contract hashing is closed only when the
wire, IPC, relay, runtime-session, PyO3, Python facade, tests, and guardrails all
carry the same full contract. A narrower patch is not complete unless it removes
the old bypass path or adds a mechanical guardrail that fails when the bypass is
reintroduced.

- [x] Python no longer accepts constructor-time CRM registration in `NativeServerBridge`.
- [x] Python no longer keeps constructor-level `_default_concurrency` or `_default_name` fallbacks in `NativeServerBridge`.
- [x] Python `decode_handshake()` no longer accepts ignored limit keywords.
- [x] No SDK code owns relay route replacement, retry, or replay semantics.
- [x] The implementation-reality closure guard in Task 4 Step 1 passes before any final success claim; it is not replaced by a narrower `rg` spot-check or by unit tests that do not inspect old public bypass shapes.
- [x] Relay stale-owner replacement checks pointer, address, generation, lease epoch, and exactly one valid replacement evidence before commit.
- [x] No `LeaseExpired` replacement evidence exists. Different-owner replacement requires final-probe `ConfirmedDead` or `ConfirmedRouteMissing`, `active_requests == 0`, and a connection-pool atomic replacement operation that retires the captured old slot under the slot lock; route-table write authority is not treated as the only acquire fence.
- [x] No production `can_replace_owner_token(...)` token-only pre-check remains in `authority.rs` or `state.rs`; `conn_pool.rs` replacement authority is the atomic `replace_if_owner_token(...)` path that consumes `OwnerReplacementEvidence` while holding the captured old slot lock.
- [x] Relay owner lease renewal increments `owner_lease_epoch`, and a renew-then-disconnect race invalidates stale replacement tokens before final commit.
- [x] Connection slot state is the single mutable owner-freshness authority; no separate `LocalOwnerFreshness` map can drift from `owner_generation`, `owner_lease_epoch`, or monotonic `owner_lease_deadline`.
- [x] Final stale-owner replacement commit compares captured route name, server id, server instance id, IPC address, owner generation, and owner lease epoch before mutating route or connection state.
- [x] `idle_timeout_secs == 0` disables time-based lease deadlines but still permits final-probe-proven `ConfirmedDead` or `ConfirmedRouteMissing` replacement.
- [x] Relay owner lease duration is derived from `RelayConfig.idle_timeout_secs`, with no new owner-lease env var or SDK setting.
- [x] Relay owner freshness remains local-only and is absent from peer snapshots, route announces, HTTP resolve responses, and anti-entropy digest churn.
- [x] Silent external recovery after lease expiry has a documented ownership outcome.
- [x] Default pickle fallback serialization uses an explicit `DEFAULT_PICKLE_PROTOCOL`, and the descriptor serializer version matches that constant.
- [x] CRM annotation descriptors use the documented canonical grammar and reject unresolved forward references, implicit `Any`, bare containers, and missing return annotations.
- [x] Descriptor method ordering comes from the shared `c_two.crm.methods.rpc_method_names(...)` helper used by `NativeServerBridge.register_crm(...)`, and shutdown callbacks do not require RPC annotations.
- [x] Plain no-hook transferables without a concrete generated dataclass-field codec are rejected when used in remote CRM signatures.
- [x] Custom Python transferables with custom byte hooks have exactly one explicit ABI declaration path (`abi_id` xor `abi_schema`), and all remote-signature custom hook users found by the repository scan use it.
- [x] The remote CRM signature scan walks `@cc.crm` methods and `@cc.transfer(input/output=...)` across tests, examples, and benchmarks; tracks duplicate class names by file path; and allows no-ABI remote-signature fixtures only in explicit negative descriptor tests.
- [x] Python module and qualified names are diagnostics only and do not participate in route ABI or signature hashes.
- [x] Route ABI and signature hashes are generated for registration and connect.
- [x] Registration producer closure is mechanically proven: `NativeServerBridge.register_crm(...)`, `PyRuntimeSession.register_route(...)`, `PyServer::build_route(...)`, `RuntimeRouteSpec`, `CrmRoute`, `RuntimeSession::register_route(...)`, IPC handshake projection, thread-local `CRMSlot`, and relay registration all carry the same descriptor-derived `abi_hash` and `signature_hash`; no named registration path synthesizes empty hashes or CRM-triplet-only metadata.
- [x] Registration guardrails distinguish legitimate hash-bearing typed carriers from unsafe route-key-only signatures: `CrmRoute`, `RuntimeRouteSpec`, `ExpectedRouteContract`, `AttestedRouteContract`, `RegisterRequest`, and test carrier structs are accepted only when their struct bodies contain `abi_hash` and `signature_hash`, and the registration function body compares or relays those fields.
- [x] No guardrail branch treats the string `ExpectedRouteContract` or `expected_contract` as sufficient proof by itself. A registration API passes the guardrail only when it exposes scalar hash fields, accepts the unique canonical `ExpectedRouteContract` whose struct body contains `abi_hash` and `signature_hash`, or converts a Python-visible `expected_contract` payload into that canonical carrier and uses both hashes in the function body.
- [x] `ExpectedRouteContract` is defined once in the foundation crate `c2-contract` and reused by wire, IPC, HTTP, runtime session, and PyO3/native glue; no duplicate expected-contract structs or CRM-triplet-only validation helpers remain for named routes.
- [x] The guardrail script fails when a second `struct ExpectedRouteContract` or Python `class ExpectedRouteContract` appears outside `core/foundation/c2-contract/src/lib.rs`; PyO3 wrappers may expose Python payloads only as thin conversion facades, not as duplicate validation authorities.
- [x] `c2-contract` owns named-route limits, call-control route-key validation, reply route-not-found route-text validation, CRM-tag validation, contract-hash validation, and expected-route validation; `validate_named_route_name(...)` and `validate_call_route_key(...)` reject empty route names, so `encode_call_control("", method_idx)`, `encode_call_control_into(..., "", method_idx)`, and `decode_call_control(...)` do not allow empty-route dispatch.
- [x] Reply route-not-found encoding uses `try_encode_reply_control(...) -> Result<...>` in Rust production paths; `c2-server` reply writers and `wire_ffi` propagate/map invalid route-text errors without `unwrap()`, `expect()`, ignored validation results, or direct calls to the old infallible encoder.
- [x] `c2-wire` imports or privately delegates to `c2-contract` and does not expose compatibility re-exports, independent `MAX_CALL_ROUTE_NAME_BYTES`, independent `MAX_HANDSHAKE_NAME_BYTES`, or standalone public `validate_*` helpers such as `validate_name_len`, `validate_crm_tag`, or `validate_crm_tag_field`.
- [x] Relay `route_table.rs` / `authority.rs` route-name and CRM-tag validation either calls `c2-contract` directly or keeps only private thin helper bodies that delegate to `c2-contract`; no relay-local trim/control-character logic or `c2_wire::handshake` validation calls remain.
- [x] `c2-contract` has only lightweight validation/hash dependencies and no dependency on `c2-wire`, `c2-http`, `c2-ipc`, Tokio, reqwest, Axum, or relay modules.
- [x] `core/transport/c2-server/Cargo.toml` depends on `c2-contract`, `CrmRoute` stores `abi_hash` and `signature_hash`, and `server.rs::validate_route_for_wire(...)` validates those hashes before handshake projection.
- [x] `core/transport/c2-ipc/src/client.rs::MethodTable`, `Client::validate_route_contract(...)`, `Client::route_contract(...)`, `SyncClient` projections, and `sdk/python/native/src/client_ffi.rs::PyRustClient.route_contract(...)` all carry and compare/project ABI and signature hashes; no IPC validation path remains CRM-triplet-only for named routes.
- [x] `core/transport/c2-http/src/relay/test_support.rs` constructs hash-bearing `CrmRoute` fixtures and no relay live-server helper still models route contracts as CRM-triplet-only tuples.
- [x] `sdk/python/tests/unit/test_sdk_boundary.py` asserts relay validation uses `c2_contract` and rejects stale `c2_wire::handshake::validate_crm_tag` dependencies.
- [x] `core/transport/c2-http/src/relay/authority.rs::AttestedRouteContract`, `RouteCommand::RegisterLocal`, HTTP `/_register`, command-loop registration, `RelayControlClient::register(...)`, `RegisterRequest`, and `PyRustRelayControlClient.register(...)` carry route hashes from IPC handshake attestation or are explicitly split into attestation-only helpers; the old `skip_ipc_validation` branch is removed rather than retained as a test bypass.
- [x] Relay registration derives published hash metadata from IPC handshake attestation, not from claim-only HTTP or command-loop fields.
- [x] Hash validation is Rust-owned and applied at wire, IPC, relay, and runtime-session boundaries.
- [x] Relay resolve query parameters, expected CRM headers, `ResolveCacheKey`, and stale-cache invalidation include both ABI and signature hashes.
- [x] Relay runtime resolve has no name-only Rust or Python surface: `RelayControlClient::resolve(name)`, `resolve_async(name)`, Python `RustRelayControlClient.resolve(name)`, `ResolveCacheKey::Name`, and the `handle_resolve` `state.resolve(&name)` mismatch fallback are removed. `/_resolve/{route}` requires complete CRM tag/hash query fields and wrong complete contracts return `404`.
- [x] Relay route-table/state name-only `resolve(name)` helpers are `#[cfg(test)]` only and source-guarded; production runtime code uses `resolve_matching(expected)` directly and has no `resolve_filtered(name, None)` / `Option<&ExpectedRouteContract>` private bypass.
- [x] Relay HTTP data-plane calls and probes reject missing expected CRM/hash headers before route lookup or upstream acquire; missing headers cannot downgrade to name-only dispatch.
- [x] Crate-internal HTTP data-plane helpers take a required `ExpectedRouteContract`, derive the URL route segment from `expected.route_name`, and cannot be called with `None` or a second independent route key.
- [x] Python-visible low-level raw pooled IPC/HTTP clients cannot issue named CRM data-plane calls; route-bound IPC and relay-aware clients are acquired only after a full `ExpectedRouteContract` validation.
- [x] Native clients returned from normal `cc.connect(...)` paths are bound to the validated route or revalidate a full `ExpectedRouteContract` on every named call; callers cannot swap `route_name` after connect. Low-level raw pool/constructor clients remain explicit diagnostic/health surfaces and reject CRM calls.
- [x] `CRMProxy.call(...)`, `sdk/python/tests/unit/test_crm_proxy.py` test doubles, IPC response-buffer tests, and HTTP relay close tests no longer rely on public `call(route_name, method_name, data)` native APIs; `test_sdk_boundary.py` rejects empty expected-contract defaults and the old PyO3 `call(..., route_name, ...)` signature.
- [x] `CRMProxy.relay(...)` is removed rather than left as a raw-wire compatibility method; `test_crm_proxy.py` and `test_sdk_boundary.py` assert the Python proxy does not expose or delegate `_client.relay(...)`.
- [x] Relay data-plane response materialization errors are not converted into empty successful HTTP responses. `router.rs::call_handler` maps `ResponseData::into_bytes_with_pool(...)` failures to `502 UpstreamResponseUnavailable`, and source guardrails reject `unwrap_or_default()` in that response path.
- [x] Python guardrails catch keyword route-key call forms such as `.call(route_name=...)` and `.call(name=...)`, plus any three-positional-argument `.call(...)` under SDK source/tests, not only hard-coded variable names such as `rust_client` or `native_client`.
- [x] Rust/PyO3 route-key guardrails use exact identifier matching for `route_name`, `name`, and `route` on route-sensitive functions so legitimate cleanup/admin parameters such as `route_names` are not flagged, but `resolve(name)` and `register(name, ...)` cannot slip through because they avoided the literal `route_name`.
- [x] Rust/PyO3 guardrails cover `sdk/python/native/src/client_ffi.rs`, `http_ffi.rs`, `runtime_session_ffi.rs`, `core/runtime/c2-runtime/src/session.rs`, `core/transport/c2-ipc/src/{client,sync_client}.rs`, `core/transport/c2-http/src/client/{client,control,relay_aware}.rs`, and `core/transport/c2-http/src/relay/router.rs`, so public named-route `call`, `probe`, `resolve`, `register`, `acquire`, `connect`, validation, cache, header, query, or relay-aware constructor APIs cannot accept independent route-key arguments named `route_name`, `name`, or `route` without deriving them from `ExpectedRouteContract` or using the explicitly attestation-only registration path.
- [x] No public named-route API accepts both `route_name` and `ExpectedRouteContract`; route-bound clients derive the route from the validated contract and arbitrary-route test helpers accept one complete contract payload.
- [x] Python-visible PyO3 methods named `call(...)` do not accept `route_name` at all; route-bound clients derive the route internally, and raw low-level clients reject CRM data-plane calls instead of accepting arbitrary route keys.
- [x] `RelayAwareHttpClient` has no public named-route constructor or setter path that leaves the expected route contract unset; every named relay-aware client is constructed with route name, CRM identity, ABI hash, and signature hash.
- [x] `HANDSHAKE_VERSION` is bumped for the route layout change, and old route-layout payloads are rejected instead of being decoded under the new hash-bearing schema.
- [x] Peer route protocol version is bumped, peer route announcements carry ABI/signature hashes, `DigestExchange` is gated as route-state wire traffic, and old-version, omitted-hash, or malformed route gossip cannot mutate the route table.
- [x] The shared peer route-state validator is used by both peer handlers and `background.rs`; anti-entropy and dead-peer-probe `DigestDiff` responses cannot mutate routes or tombstones unless their `PeerEnvelope` and digest entries passed the route-state version/hash validator.
- [x] `DigestDiffEntry::Active` carries and validates `abi_hash`, `signature_hash`, and active route digest hash; `DigestDiffEntry::Deleted` carries and validates tombstone digest hash; both hashes bind `name`, `relay_id`, and state; no raw `DigestDiffEntry` can convert directly into `RouteEntry`.
- [x] Relay anti-entropy route digests use stable lowercase SHA-256 hex over canonical key-bound digest material, include route key plus ABI/signature hashes, exclude local-only owner identity, and have active-route plus tombstone golden-vector tests.
- [x] `c2-http` client-only builds still pass `cargo check --manifest-path core/Cargo.toml -p c2-http --no-default-features`; relay digest dependencies are optional relay-feature dependencies and are not pulled into client-only builds.
- [x] Old-version deletion paths, including `RouteWithdraw`, `DigestExchange`, `DigestDiffEntry::Deleted`, `RelayLeave` / `RemovePeerRoutes`, and tombstone-bearing full sync snapshots, cannot remove current hash-bearing routes or install tombstones.
- [x] Full-sync snapshots carry an explicit protocol/schema version and exactly one hash-bearing wire shape, `FullSyncEnvelope { protocol_version, snapshot: FullSyncSnapshot }`; `/_peer/join`, `/_peer/sync`, seed bootstrap, and seed retry reject old-version route snapshots before merging.
- [x] Old-version and future-version `FullSyncEnvelope` payloads are rejected before merge even when the snapshot has empty routes and tombstones, because `merge_snapshot(...)` is destructive peer-route replacement and peer metadata mutation.
- [x] Current-version full-sync validation rejects malformed route ABI/signature hashes, key-replayed active route digest hashes, and tombstone digest hashes all-or-nothing before any route replacement or peer metadata merge.
- [x] Current-version full-sync JSON uses `FullSyncRoute` and `FullSyncTombstone` wrappers with key-bound `hash`; raw `RouteEntry`, raw `RouteTombstone`, `server_id`, `server_instance_id`, `ipc_address`, and `locality` do not appear in peer-sync wire payloads.
- [x] Internal `RouteEntry`, `RouteTombstone`, and raw `FullSync` do not derive `Serialize` or `Deserialize`; only versioned wire wrappers are serde-visible.
- [x] `/_peer/join` and `/_peer/sync` use one state-owned full-sync envelope helper that injects the relay's self `PeerSnapshot` before creating hash-bearing wire wrappers.
- [x] `RelayState::merge_snapshot(...)` accepts only `ValidatedFullSync`; raw `FullSync` cannot be merged by production seed, retry, background, or peer-handler code.
- [x] Production full-sync guardrails scan `peer_handlers.rs`, `server.rs`, `background.rs`, `state.rs`, and `route_table.rs` for raw `FullSync` JSON parsing, revived legacy `canonical_peer_snapshot(...)` producers, public raw merge signatures, and unvalidated `.merge_snapshot(snapshot)` calls; serde-only checks are not treated as sufficient proof.
- [x] Production full-sync guardrails validate every `.merge_snapshot(...)` call outside `state.rs` / the private route-table inner helper by call argument, not by variable name or file-wide `ValidatedFullSync` mentions; the argument must be directly built by `ValidatedFullSync::try_from(...)` or by a local variable mechanically tied to that conversion.
- [x] The full-sync guardrail inspects the enclosing Rust function body for each `.merge_snapshot(...)` call and rejects local variables that are mutable, shadowed, assigned more than once, reassigned, or passed through `&mut` / `std::mem::{replace,take}` after `ValidatedFullSync::try_from(...)` and before merge.
- [x] The full-sync type signature remains the primary compile-time fence: `RelayState::merge_snapshot(...)` accepts `ValidatedFullSync`, raw route-table merge is private to `route_table.rs`, and the scan is only a regression detector for production callers rather than the sole proof of validation.

The 2026-05-11 closure pass extracted and executed the raw full-sync wire-shape
and production-path guard scripts from this plan. They found no raw
`json::<FullSync>`, no public raw `FullSync` merge signature, no `DefaultHasher`
/ `u64` digest, no raw `DigestDiffEntry` conversion into `RouteEntry`, no serde
derive on internal `RouteEntry`, `RouteTombstone`, or raw `FullSync`, and no
unvalidated production `.merge_snapshot(...)` call.
- [x] Relay peer gossip, anti-entropy repair, and Python HTTP FFI route projection carry and validate route fingerprints.
- [x] Thread-local, direct IPC, relay-discovered IPC, and explicit HTTP relay all reject hash mismatches before method dispatch.
- [x] Low-level admin probes without a CRM route remain usable without expected fingerprints, while named routes still require valid route fingerprint metadata.
- [x] Direct IPC still works when relay is unset or points at an unavailable address.
- [x] SHM-backed request and response paths still avoid Python `bytes` materialization.
- [x] Docs reference the remediation plan and no longer describe these gaps as unplanned work.
