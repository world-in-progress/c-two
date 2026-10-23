# Thin SDK / Rust Core Boundary Reference

**Date:** 2026-05-04
**Status:** Architecture reference / implemented downshift ledger with future
hardening notes
**Scope:** Identify Python SDK runtime state and mechanisms that should move
down into the Rust core so future language SDKs can stay thin.

## 0.x Constraint: Prefer Clean Cuts Over Compatibility Debt

C-Two is currently in the 0.x line. Implementation work from this document
should not preserve backwards compatibility for incorrect, experimental, or
superseded internal APIs unless a compatibility window is explicitly requested.
Do not leave dangling fallback behavior, compatibility shims, or zombie modules
after moving behavior into Rust. Remove obsolete code paths, tests, docs, and
module exports in the same change that replaces them.

## Non-Negotiable Runtime Constraint: IPC Must Not Depend On Relay

Direct single-node cross-process IPC must remain a complete standalone mode.
Relay is a discovery/forwarding layer, not the lead authority for IPC.

Required invariants:

- `cc.connect(..., address="ipc://...")` must bypass relay and connect through
  `c2-ipc` directly.
- `cc.register(...)` without `C2_RELAY_ANCHOR_ADDRESS` must still start a local IPC
  server and expose `cc.server_address()`.
- A bad or unavailable relay must not break explicit direct IPC connections.
- Rust-side runtime work must build a standalone IPC session first; relay
  registration/name resolution is only an optional projection above that
  session.
- Relay-backed name resolution uses `C2_RELAY_ANCHOR_ADDRESS` as the SDK
  process's control-plane relay anchor. Remote HTTP data-plane calls must go
  directly to the selected route's `relay_url`, and direct IPC selection from
  relay resolution is allowed only for loopback/local anchors.
- Relay-discovered direct IPC is accepted only after native IPC handshake
  identity matches the relay route's `server_id` and `server_instance_id`;
  route-name-only acceptance is forbidden.
- Thread-local same-process dispatch is an SDK/language optimization, not a
  replacement for cross-process IPC.

## Current Boundary Summary

C-Two already has the main language-neutral layers in Rust:

- `core/foundation/c2-config`: environment/default/config resolution.
- `core/foundation/c2-error`: canonical error code registry and wire bytes.
- `core/foundation/c2-mem`: SHM pool, buddy/dedicated/file-spill handles.
- `core/protocol/c2-wire`: wire frame, handshake, control, signal, chunk codec.
- `core/transport/c2-ipc`: direct IPC client, client pool, SHM/chunk transfer.
- `core/transport/c2-server`: direct IPC UDS server, dispatcher, scheduler.
- `core/transport/c2-http`: relay HTTP client, relay-aware route fallback,
  relay server behind the `relay` feature.
- `core/runtime/c2-runtime`: process runtime session, route transactions,
  client pools, relay projection, and lifecycle outcomes.

The Python SDK is already thinner than earlier versions. The downshift ledger
below records the generic runtime mechanisms that were identified here and have
now moved to Rust ownership, while keeping Python domain logic, CRM decorators,
and transferable serialization semantics in the SDK.

### Current encoded request and response boundary

The current remote request path is `RequestData -> RequestLease.copy_bytes() -> EncodedService.invoke(Vec<u8>) -> PyCoreRequestBuffer -> Python memoryview`. Core retains the original transport lease until `invoke` returns or unwinds. The owned snapshot remains intentional; PyO3 moves that same `Vec` into its buffer projection without a second copy. Response receive follows `ResponseLease.copy_bytes() -> HeldResponse -> PyCoreResponse`; Core owns transport lease release ordering, while `c2-mem` remains the allocation-release authority. Same-process thread-local calls continue to pass Python objects directly.

`6e6519a8` removed all `PyShmBuffer` construction from `server_ffi.rs`; `e21e79c4` retained the snapshot while removing the second request copy. Retirement removes only the unused `sdk/python/native/src/shm_buffer.rs`, its module registration and the low-level `c_two._native.ShmBuffer` export, without compatibility aliases. It does not change these data paths, R06 active buffer state, C/FFI ABI 5, IPC 12, SHM 3 or the Rust API.

The ledger's verified coverage below records the implementation stage named in each section. Earlier `ShmBuffer` and direct Python-buffer-to-response-SHM observations remain historical evidence; they do not establish zero-copy behavior for the current copy-backed Core adapters.

## Keep In Python SDKs

These are language-specific and should not be forced into the Rust core:

- CRM decorators/metaclasses and Python method discovery:
  `sdk/python/src/c_two/crm/`.
- Python resource object invocation and Python exception presentation.
- Transferable dataclass conversion, `serialize`, `deserialize`, and
  `from_buffer` orchestration.
- Python proxy object construction and attaching `crm.client`.
- Thread-local same-process fast path that passes Python objects directly and
  skips serialization.
- SDK UX such as `cc.serve()` banner/signals, provided it does not own core
  transport semantics.

## Downshift Candidates

### Downshift Issue Status Ledger

This ledger tracks downshift issues in this document against the current
`dev-feature` branch. Do not mark an issue as implemented here unless the
corresponding Python-owned authority has actually been removed or reduced in
code, not merely planned in a separate implementation document.

| Issue | Candidate | Current status | Evidence |
| --- | --- | --- | --- |
| 1 | Remote IPC scheduler config and execution semantics | Implemented | See `docs/plans/2026-05-04-remote-ipc-scheduler-rust-config.md`; this section below records Rust `c2-server` ownership and verified IPC/thread-local coverage. |
| 2 | Direct IPC probe/control helpers | Implemented | See `docs/plans/2026-05-04-direct-ipc-control-helpers-rust.md`; `c2-ipc` owns socket-path derivation plus ping/shutdown control helpers, and Python `client/util.py` is a thin native facade. |
| 3 | Process runtime/session state | Implemented | See `docs/plans/2026-05-05-runtime-session-rust-authority.md`; `core/runtime/c2-runtime` owns identity, route transactions, client pools, relay-anchor projection/name resolution, and shutdown/unregister outcomes. |
| 4 | Canonical error registry and wire bytes | Implemented | See `docs/plans/2026-05-06-canonical-error-rust-authority.md`; Rust `c2-error` owns registry/wire bytes and Python delegates through native codec bindings. |
| 5 | SDK-visible buffer lease lifecycle | Implemented | See `docs/plans/2026-05-06-unified-buffer-lease-rust-authority.md`; `c2-mem` owns retained buffer lease accounting and Python exposes only hold/result facades. |
| 6 | Server route slot lifecycle and readiness | Implemented | See `docs/plans/2026-05-07-server-readiness-rust-authority.md`; `c2-server` owns readiness, lifecycle transitions, socket unlink authority, and shutdown barriers. |
| 7 | Response allocation decision | Implemented | See `docs/plans/2026-05-07-response-allocation-rust-authority.md`; Python response-pool allocation was removed and low-copy response buffer preparation now lives in Rust native/core. |
| 8 | IPC config validation duplication | Implemented | See `docs/plans/2026-05-07-ipc-config-validation-rust-authority.md`; Python now forwards IPC override mappings and Rust/native config parsing owns allowed-key, forbidden-key, and shape validation. |
| 9 | CRM contract descriptor and route validation | Implemented | See `docs/plans/2026-05-08-crm-contract-rust-authority.md`; SDKs extract CRM descriptors, while Rust owns language-neutral CRM identity matching, route metadata projection, direct IPC/relay contract checks, and method-index validity checks. |

### 2026-05-08 Post-Review Addendum

Session `019dec60-8de6-7492-9152-9ad8b61ebe49` was interrupted during a strict
review of relay registration behavior. The durable conclusion belongs in this
boundary reference because it reinforces the same rule: language SDKs must not
own generic runtime, relay, IPC, wire, or chunk lifecycle correctness.

Settled findings from that session:

- The relay command-loop / `c3 relay --upstream` registration path had a real
  ordering bug: it could dial a new candidate IPC endpoint before rejecting a
  duplicate live owner.
- That specific bug is now handled in Rust `c2-http`: command-loop registration
  is split into candidate eligibility, candidate IPC attestation, and final
  commit. Duplicate live owners are rejected before candidate connect.
- Non-skip IPC validation now requires the candidate handshake to provide a real
  `server_instance_id`; only skip-validation tests may synthesize an instance.
- Final relay route replacement remains a Rust authority decision guarded by
  relay-local route state, `OwnerToken`, candidate IPC attestation, and an
  opaque final owner proof created after candidate attestation.
- The stale pre-attestation evidence reuse gap is closed in Rust relay authority:
  preliminary owner probes may only allow a candidate IPC attempt, and final
  commit can consume only a proof produced by the post-attestation final owner
  probe. This is still a relay-local proof boundary, not a cross-process owner
  fence; a stronger guarantee that an external owner cannot become live after
  final proof would require a future IPC owner-fence protocol.

This relay robustness note is not a downshift-ledger candidate. Treat it as a
separate Rust relay/IPC hardening concern unless it becomes release-blocking.

### 2026-05-08 Issue 8 Verification And Issue 9 Decision

Issue 8 is implemented and re-reviewed. Verification from the strict review
pass:

- `uv sync --reinstall-package c-two`
- `cargo test --manifest-path core/Cargo.toml -p c2-config`
- `cargo check --manifest-path sdk/python/native/Cargo.toml -q`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_ipc_config.py sdk/python/tests/unit/test_runtime_session.py sdk/python/tests/unit/test_sdk_boundary.py -q --timeout=30 -rs`
- `cargo test --manifest-path core/Cargo.toml --workspace`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs`
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs`
- direct native `RuntimeSession(UserDict(...))` mapping snapshot probe
- `git diff --check`

All passed, with the full Python suite reporting `741 passed, 2 skipped`; the
skips are examples-dependency skips, not IPC config failures.

Issue 9 was initially reviewed too narrowly as a native method-table facade.
That conclusion is superseded by the cross-language CRM review: Python should
still discover Python CRM methods and map `method_idx -> Python method` inside
the callback, but CRM namespace/version identity, route metadata projection,
client-side CRM matching, and method-index validity are language-neutral
runtime contract concerns. They should live in Rust core so future Rust, Go,
Fortran, and C++ SDKs do not each reimplement the same checks.

The repair is tracked in
`docs/plans/2026-05-08-crm-contract-rust-authority.md` and the fingerprint
closure in
`docs/superpowers/plans/2026-05-10-thin-sdk-boundary-gap-remediation.md`.
Issue 9 no longer blocks the thin-SDK/Rust-core boundary: Python extracts a CRM
descriptor from decorated classes and passes the descriptor hashes to native
runtime entry points, while Rust `c2-contract`, `c2-wire`, `c2-server`,
`c2-ipc`, `c2-http`, and `c2-runtime` own route metadata validation,
handshake projection, direct IPC route contract validation, relay-aware contract
filtering, explicit HTTP relay contract probing, relay registration
attestation, and O(1) invalid-method-index rejection before scheduler
acquisition or SDK callback invocation.

### 2026-05-10 Coverage Audit And Remaining Gaps

The current implementation covers all nine downshift candidates in this
reference. The remaining items are not evidence that Python still owns generic
runtime authority; they are either future protocol-safety work, relay hardening,
or cleanup of narrow compatibility facades.

Execution plan:
`docs/superpowers/plans/2026-05-10-thin-sdk-boundary-gap-remediation.md`.

1. **Strict cross-language ABI/IDL compatibility remains future work.** The
   current implementation generates route `abi_hash` and `signature_hash` from
   Python CRM descriptors and carries those hashes through registration,
   handshakes, direct IPC, relay resolve/probe/call, and explicit HTTP relay
   paths. It does not yet define a language-neutral annotation grammar,
   required ABI declarations for custom transferable byte formats, or a
   cross-SDK IDL/codegen story. That future work must not move Python-specific
   `serialize`, `deserialize`, or `from_buffer` orchestration into Rust merely
   for symmetry.

2. **Relay stale-owner replacement is Rust-owned and now uses final proof.** The
   relay no longer trusts route names alone: candidate upstreams must pass IPC
   identity and route contract attestation, and relay-local replacement commits
   use owner-token, owner-generation, owner-lease-epoch, route-identity, and
   final-proof checks. The fixed gap was stale pre-attestation evidence reuse:
   a dead or route-missing observation collected before candidate attestation can
   no longer authorize final commit. This remains relay-local hardening; future
   cross-process owner fencing, if needed, must live in Rust relay/IPC state and
   must not add SDK-side route replay, retry, or route-name trust.

3. **Narrow Python compatibility facades still exist but do not own runtime
   authority.** `NativeServerBridge` still accepts constructor-time CRM
   registration for old low-level call sites, and `transport/protocol.py` keeps
   Python-compatible wrapper arguments around Rust codec calls. These facades
   should be cleaned up under the 0.x clean-cut rule when their remaining tests
   or internal users are migrated, but they are not boundary regressions: route
   registration, wire validation, lifecycle, response allocation, and config
   validation still execute through Rust/native authority.

### P1. Remote IPC scheduler config and execution semantics

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-04-remote-ipc-scheduler-rust-config.md`.

Route concurrency is owned by Rust `c2-server`. Python passes the full typed
`ConcurrencyConfig` during route registration; PyO3 parses `mode`,
`max_pending`, and `max_workers` into the native route handle; Rust acquires
the per-route guard around callback invocation and exposes a read-only
snapshot to the SDK. Python keeps `ConcurrencyConfig` as the typed facade and
keeps `Scheduler.execution_guard()` only as a thin adapter for thread-local
direct calls.

Historical verified coverage from that scheduler implementation includes remote IPC `exclusive`, `parallel`, and `read_parallel` behavior; large SHM-backed hold input arriving as `memoryview` over `ShmBuffer`; explicit direct `ipc://` bypassing a bad relay environment; and thread-local calls continuing to pass Python objects directly. Route-level `max_pending` and `max_workers` are now enforced by the same Rust handle for remote IPC and same-process thread-local calls.

**Why this is core behavior**

Read/write/exclusive/parallel dispatch semantics are not Python-specific. Every
language SDK needs identical behavior once calls cross IPC. If this remains in
Python, future SDKs must reimplement scheduler semantics and risk diverging.

**Current snapshot and lease constraint**

The original scheduler downshift preserved the then-current `RequestData -> PyShmBuffer -> memoryview` path and rejected an extra Python `bytes` materialization. That describes the scheduler implementation stage, before the Core encoded-service boundary replaced it. Current scheduler changes must preserve the owned snapshot and transport lease ordering described above: `RequestLease.copy_bytes()` creates the snapshot, `EncodedService.invoke(Vec<u8>)` receives it, and PyO3 moves it into `PyCoreRequestBuffer` for Python `memoryview` access without a second copy.

**Thread-local caveat**

Thread-local dispatch is different from remote IPC. It passes Python objects
directly and intentionally skips serialization. Do not route thread-local calls
through Rust bytes dispatch for symmetry. The implemented shape projects a
small native route-concurrency handle into Python so same-process calls share
mode and capacity state with remote calls without serializing Python objects.
Each future SDK can use its own same-process fast path, but the shared
concurrency state must remain Rust-owned.

**Implementation notes**

1. Native `RustServer.register_route(...)` accepts a concurrency mode string
   from Python.
2. PyO3 parses Python mode strings into
   `c2_server::scheduler::ConcurrencyMode`.
3. `access_map` remains method-index-to-read/write metadata.
4. Rust applies the mode when constructing the per-route scheduler.
5. Python scheduler state is not an authority; it only forwards to the native
   handle for thread-local direct calls and snapshot/debug projection.
6. `max_pending` / `max_workers` are enforced by Rust for both local and
   remote execution paths.

**Required tests**

- Remote IPC `ConcurrencyMode::Exclusive` serializes all calls.
- Remote IPC `ConcurrencyMode::Parallel` permits concurrent writes.
- Remote IPC `ConcurrencyMode::ReadParallel` permits concurrent reads and
  serializes writes.
- Remote requests expose the Core-owned snapshot through `PyCoreRequestBuffer` / Python `memoryview`, with no second PyO3 copy and the original transport lease retained through invocation.
- Thread-local calls still skip serialization and still pass Python objects.
- Mixed thread-local/direct-IPC calls share `max_workers` and `max_pending`
  capacity limits through the same native route handle.

### P1. Direct IPC probe/control helpers

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-04-direct-ipc-control-helpers-rust.md`.

Rust `core/transport/c2-ipc` now owns socket-path derivation and direct IPC
control helpers for `ping()` and `shutdown()`. The PyO3 native layer exposes
`ipc_socket_path()`, `ipc_ping()`, and `ipc_shutdown()`. Python
`sdk/python/src/c_two/transport/client/util.py` delegates to those functions
and no longer owns raw UDS socket probing, copied frame structs, signal
constants, or socket-path derivation.

Malformed IPC addresses are rejected by Rust validation. Missing sockets return
the same boolean probe behavior through the native helper without making relay
part of the direct IPC control path.

`shutdown("ipc://...")` is an admin/control-plane helper, not a normal CRM
business-client method and not top-level `cc.shutdown()`. It intentionally
allows a same-host high-privilege supervisor process to stop a child resource
server by direct IPC address. Future name-based or cross-node shutdown should
extend this as an explicit authenticated admin control plane, with separate
route-level and server-level semantics, rather than hiding it behind ordinary
CRM calls.

**Why this is core behavior**

Signal frames, UDS path derivation, and IPC liveness probes are wire/IPC
control-plane behavior. They are language-neutral and already partially defined
in Rust through `c2-wire` constants and `c2-server` signal handling.

**IPC/no-relay impact**

This strengthens direct IPC independence. These helpers must live under
`c2-ipc`/native bindings, not `c2-http` or relay code.

**Verified coverage**

- `ping("ipc://...")` returns true against a direct IPC server with no relay.
- `shutdown("ipc://...")` stops a direct IPC server with no relay.
- Invalid IPC addresses are rejected through Rust validation.
- Owner-side bridge cleanup remains idempotent after an external direct IPC
  shutdown signal; native runtime draining must not depend on lifecycle
  readiness still being `running`.

### P1. Process runtime/session state

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-05-runtime-session-rust-authority.md`.

Rust `core/runtime/c2-runtime` now owns process-level runtime/session state.
The Python registry is a facade around native `RuntimeSession` plus
language-specific CRM binding and proxy construction.

Rust-owned pieces include:

- local server identity and canonical `ipc://` address;
- server/client IPC override storage and resolved client config projection;
- client config freeze after first connection acquisition;
- direct IPC client acquire/release/refcount/shutdown through the native
  session pool;
- low-level HTTP client acquire/release/refcount/shutdown through the native
  session projection, while explicit SOTA HTTP connects use the native
  relay-aware contract-validation path;
- lazy server bridge creation through `ensure_server_bridge()`;
- route registration transactions through `register_route()`;
- local unregister and shutdown transaction outcomes;
- relay address projection, relay-backed name resolution, and relay cleanup
  error reporting.

Python still owns Python CRM local bindings, thread-local proxy construction,
and invoking `@on_shutdown` callbacks from native structured outcomes.

**Why this is core behavior**

Most of this is not Python-specific. A future SDK should not have to reproduce
server identity generation, standalone IPC session lifecycle, client pool
configuration, and registration transaction rules.

**Correct target shape**

```text
Rust RuntimeSession
  - standalone local IPC server identity/address
  - direct IPC server lifecycle
  - direct IPC client pool and default config
  - route registration handles
  - local unregister/shutdown transaction rules
  - optional RelayProjection for relay register/unregister/resolve
```

Relay projection must not own the base session. The base IPC session exists and
works when relay is absent.

**Verified coverage**

- No relay: `register -> server_address -> connect(address)` works across
  processes.
- Bad relay env: explicit direct IPC address still works.
- Relay register failure does not leave split-brain local/Rust route state.
- Explicit unregister preserves local correctness even if relay cleanup fails.
- Name-only relay connections go through native `RuntimeSession.connect_via_relay()`.
- Relay anchor config uses the clean-cut `C2_RELAY_ANCHOR_ADDRESS` /
  `cc.set_relay_anchor(...)` surface. The retired `C2_RELAY_ADDRESS` /
  `cc.set_relay(...)` names are not read as aliases.
- Name-only relay connections may select direct IPC only when the configured
  relay anchor is loopback/local. Nonlocal anchor responses are forced through
  HTTP routing even if an `ipc_address` is present.
- Relay-discovered direct IPC is accepted only after native IPC handshake
  identity matches the relay route's `server_id` and `server_instance_id`;
  route-name-only acceptance is forbidden.
- Relay `/_resolve` exposes `ipc_address`, `server_id`, and
  `server_instance_id` only to loopback callers for local routes. Non-loopback
  callers and peer routes receive HTTP route targets without IPC-only fields.
- Remote HTTP data-plane calls use the resolved route's `relay_url` directly,
  avoiding a client -> anchor relay -> remote relay forwarding hop.
- Relay `/_resolve` strips `ipc_address` for non-loopback HTTP callers as
  defense-in-depth; peer routes already omit IPC addresses.
- Shutdown drains native IPC/HTTP clients and consumes native relay cleanup
  outcomes before clearing Python local bindings.

### P1. Canonical error registry and wire bytes

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-06-canonical-error-rust-authority.md`.

Rust `core/foundation/c2-error` owns the canonical error code registry and the
canonical `code:message` error wire bytes. The format is named `wire`, not
`legacy`, because it is still the active C-Two error payload format. Python
keeps only SDK presentation: `ERROR_Code` as a public enum facade generated
from `_native.error_registry()`, Python exception subclasses, and subclass
mapping for `CCError.deserialize()`.

**Copy constraints**

- Python `CCError.deserialize(...)` passes the input buffer directly to native
  decode and does not call `memoryview.tobytes()` or `bytes(data)` first.
- Native decode returns only `(code, message)` for the SDK hot path. It does
  not allocate a dict or return the error name for normal Python exception
  construction.
- Native encode writes `code:message` directly into the Python bytes allocation
  and avoids an intermediate Rust `Vec<u8>` where practical.
- Python malformed-payload presentation remains SDK-owned: native parser errors
  become `CCError(ERROR_UNKNOWN, "Malformed error payload: ...")` instead of
  leaking native `ValueError` through user-facing call paths.

**Verified coverage**

- All Python `ERROR_Code` values match the Rust registry.
- `CCError.serialize()` uses native `encode_error_wire()`.
- `CCError.deserialize()` uses native `decode_error_wire_parts()` without a
  Python pre-copy.
- Unknown numeric codes map to `ERROR_UNKNOWN` with canonical Rust context.
- Malformed wire payloads keep Python public behavior.
- Boundary tests prevent Python from reimplementing the `code:message` parser.
- Transfer hook errors distinguish bytes deserialization from buffer-backed
  `from_buffer()` construction on both resource input and client output. Rust
  `c2-error` owns `ResourceInputFromBuffer = 4` and
  `ClientOutputFromBuffer = 8`; SDKs project those codes and choose the
  precise hook-specific presentation error without re-owning the wire codec.
- `c2-server` emits canonical `c2-error` wire bytes for route-capacity,
  route-closed, and server-side internal execution errors instead of raw
  UTF-8 status strings, so issue1/3 route outcomes remain compatible with
  the issue4 decoder.

### P2. SDK-visible buffer lease lifecycle

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-06-unified-buffer-lease-rust-authority.md`.
Post-review repair also landed for deserialize-only client output hold: any
successful `cc.hold()` response that keeps a native response owner alive is
counted as a retained client-response lease until `HeldResult.release()`, even
when the output type falls back to `deserialize()` rather than `from_buffer()`.

Rust `core/foundation/c2-mem` owns SDK-visible buffer lease accounting for
runtime-owned buffers exposed to language SDKs. Inline, SHM, handle, and
file-spill buffers are storage tiers; transient view mode and retained hold
mode are retention policies. Python keeps `cc.hold()`, `HeldResult`,
`from_buffer()` orchestration, and warning presentation, but retained entry
state, byte totals, ages, storage/direction aggregation, and stale snapshots
come from native accounting.

Client-side `cc.hold()` is the primary retained lease path. At the issue5 implementation stage, resource-side `@cc.transfer(..., buffer="hold")` input retention used the same native tracker as a diagnostic path; that decorator form is now retired. Inline retained buffers are counted because they can back zero-copy `memoryview` outputs through native-owned `Vec<u8>`; they are reported separately from SHM-backed retained buffers so memory pressure diagnostics stay accurate.

**Historical verified coverage (issue5 implementation stage)**

- Inline client-response `cc.hold()` counts as a retained lease and releases on
  `HeldResult.release()` / context manager / GC fallback.
- SHM or handle client-response `cc.hold()` uses the same API and releases
  through the existing `ResponseBuffer.release()` memory path.
- Resource-side input hold uses the same native retained tracker without
  Python weakref accounting.
- `hold_stats()` works without a Python server object and reports storage and
  direction breakdowns.
- Direct IPC retained buffers remain relay-independent.
- Client output `from_buffer()` and resource input `from_buffer()` failure
  paths release their memoryview/native buffer owner before propagating a
  `CCError`, so traceback-local response/request objects do not pin stale
  retained lease stats.
- Deserialize-only outputs wrapped with `cc.hold()` are counted as retained
  client-response leases because `HeldResult` still keeps the native response
  owner and memoryview alive until explicit release.
- Default view mode still releases immediately and does not materialize payloads
  or add retained accounting overhead.

**Non-blocking future hardening notes**

The issue5 ledger records the earlier `ResponseBuffer.release()` / `ShmBuffer.release()` seam. Current work must keep `c2-mem` as the allocation-release authority and Core `RequestLease` / `ResponseLease` / `HeldResponse` as the transport lifetime boundary; Python buffer projections must not acquire independent transport release authority. Further work should improve only the SDK/native seam. A future hardening plan can focus on (1) locking down successful-hold tracking semantics for both `from_buffer()` and deserialize-only outputs, (2) making native retained tracking calls idempotent and non-leaking under exceptional paths, and (3) extracting cross-SDK conformance tests that Go, Rust, Fortran, and Python can share without forcing payload copies or SHM promotion for inline results.

### P2. Server route slot lifecycle and readiness

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-07-server-readiness-rust-authority.md`.

Rust `c2-server` owns IPC server lifecycle state and readiness notification.
Native startup exposes `start_and_wait(timeout)` and state projection through
`is_running` / `is_ready`. Python `NativeServerBridge` keeps CRM slot objects
and Python callback dispatch, but it no longer polls socket files, maintains a
separate `_started` flag, or infers readiness from filesystem state.

Rust also tracks whether a concrete `Server` instance actually bound its socket
path before it is allowed to unlink that path during shutdown. This prevents a
failed second server startup from removing the active socket owned by an
already-running server.

Each native start attempt is explicitly fenced by Rust before PyO3 begins
waiting for readiness. The fence resets stale terminal lifecycle state
(`Stopped` / `Failed`) and the one-shot shutdown signal so normal restart after
shutdown, and retry after a failed bind once the socket is released, cannot be
poisoned by a previous attempt.

Native shutdown is also a lifecycle barrier. PyO3 waits for native stop
notification before dropping the runtime and terminalizes runtime-owned
`Starting` / `Ready` / `Stopping` states after forced runtime cleanup while
preserving `Failed(_)` diagnostics. This prevents immediate restart from seeing
transient `Stopping` after `RustServer.shutdown()` returns.

**Verified coverage**

- `Server.start(timeout=...)` returns only after direct IPC ping/connect can
  succeed.
- Starting a second server on an active IPC socket fails without unlinking the
  first server's socket path.
- Calling shutdown on a server that failed to bind does not unlink another
  server's active socket path.
- The same native server bridge can start again after normal shutdown.
- Repeated tight start/shutdown cycles do not expose stale `Stopping` through
  `is_started()` or poison the next start.
- A duplicate server whose first start failed can retry successfully after the
  active socket owner shuts down.
- Runtime shutdown outcomes use native lifecycle state instead of
  `socket_path().exists()`.
- Direct IPC readiness remains relay-independent, including with a bad relay
  environment variable.
- Boundary tests prevent Python socket polling and Python-owned `_started`
  lifecycle state from being reintroduced.

### P2. Response allocation decision

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-07-response-allocation-rust-authority.md`.

Python dispatcher now returns only `None` or serialized bytes-like data after
CRM method execution. It no longer receives a native response pool object,
compares `len(res_part)` against `shm_threshold`, writes response SHM, returns
SHM coordinate tuples, or converts `memoryview` outputs with `bytes(...)` for
transport.

Rust `c2-server::response::try_prepare_shm_response()` owns threshold-based large response SHM preparation and post-allocation cleanup. Current `EncodedService` returns owned response bytes through Core; `c2-server` enforces payload limits and selects inline / buddy SHM / chunked transport, including checked frame length, buddy metadata and chunk-count limits. Payloads valid for the configured response path but too large for buddy wire metadata use chunked fallback.

At the response-allocation downshift stage, PyO3 `parse_response_meta()` accepted `bytes` and generic `PyBuffer<u8>` exporters, attempted direct Python-buffer-to-response-SHM writes for large outputs, and materialized owned bytes for small outputs or SHM allocation fallback. Its pre-allocation payload-limit checks and the coverage below describe that earlier implementation; they are not a requirement to restore PyO3-owned response allocation or evidence of current direct SHM construction.

**Historical verified coverage (response-allocation downshift stage)**

- Rust helper tests cover below-threshold no-allocation behavior, large
  direct SHM writes, allocation-failure fallback, and cleanup after copy
  failure.
- Rust reply-selection tests cover oversized buddy wire fallback, inline frame
  length limits, chunk-count overflow rejection, and response allocation
  cleanup when reply writes fail.
- Direct IPC integration tests cover large `bytes` responses, large
  `memoryview` responses, small inline responses through `cc.hold()` retained
  storage accounting, and `max_payload_size` rejection as a resource output
  serialization error.
- Boundary tests prevent Python response-pool allocation and PyO3 SHM
  coordinate tuple parsing from being reintroduced.
- Direct IPC response allocation remains relay-independent, including with a
  bad relay environment variable.

### P2. IPC config validation duplication

**Implemented ownership**

Status: implemented on `dev-feature` by
`docs/plans/2026-05-07-ipc-config-validation-rust-authority.md`.

Python `sdk/python/src/c_two/config/ipc.py` now keeps only `TypedDict` schemas,
native resolver forwarding, and global SHM override projection. It no longer
keeps `_normalize_*`, `_clean_ipc_overrides`, allowed-key tables, forbidden-key
tables, or user-facing IPC override key error text. `registry.py` passes caller
override mappings directly into native `RuntimeSession` parsing.

The canonical IPC override key catalog now lives in `core/foundation/c2-config`
and is exported to PyO3 bindings. `sdk/python/native/src/config_ffi.rs` accepts
arbitrary Python mappings, snapshots them into an owned native `dict`, validates
key names, rejects global-only fields such as `shm_threshold`, rejects unknown
fields such as derived `max_pool_memory`, and then builds Rust override structs.
Runtime-session storage remains Rust-owned, so mutating a caller mapping after
`cc.set_server()` or `cc.set_client()` cannot mutate stored overrides.

**Verified coverage**

- Unknown server/client IPC override errors still point to the offending key
  and come from native validation.
- `shm_threshold` remains a global transport policy override, not an IPC role
  override.
- Mapping subclasses are accepted and snapshotted by native parsing.
- Non-mapping override inputs and non-string override keys fail with precise
  native `TypeError` messages.
- Non-string key shape errors take precedence over semantic forbidden-field
  errors.
- Boundary tests prevent Python allowed-key, forbidden-key, and `_normalize_*`
  validation helpers from being reintroduced.

### P3. CRM contract descriptor and route validation

**Implemented ownership**

Status: implemented and verified by
`docs/plans/2026-05-08-crm-contract-rust-authority.md`, with follow-up relay
attestation hardening recorded in
`docs/plans/2026-05-08-relay-crm-attestation-hardening.md`.

`sdk/python/src/c_two/transport/wire.py` may retain a Python `MethodTable` only
as a Python callback projection. It is not the language-neutral contract
authority for route metadata, method-index validity, or client CRM matching.

Python method discovery stays in Python, but it now feeds a Rust-owned route
contract descriptor and expected-route validator:

- SDKs extract `crm_ns`, `crm_name`, `crm_ver`, RPC method membership, access
  metadata, Python signature metadata, and transfer metadata from the
  language-native CRM definition, then ask native `c2-contract` to hash the
  canonical descriptor JSON.
- Rust stores `abi_hash` and `signature_hash` beside route CRM identity and
  validates that native route metadata agrees with the runtime route spec at
  registration time.
- Direct IPC handshakes project CRM identity and descriptor hashes next to route
  method entries.
- Rust IPC clients validate a complete `ExpectedRouteContract` before the SDK
  binds a proxy to a direct IPC route.
- Relay-aware route selection filters candidates by complete expected route
  contract before choosing local IPC or HTTP targets.
- Explicit HTTP relay connects share the relay-aware contract-validation path and
  probe/resolve with expected route hashes before returning a proxy.
- Relay registration derives published contract metadata from IPC handshake
  attestation. The old `skip_ipc_validation` path is removed rather than kept as
  a production or test registration bypass.
- Rust server dispatch rejects invalid method indexes before scheduler
  acquisition and before invoking any SDK callback on inline, SHM, and chunked
  request paths.

**Verified coverage**

- `RouteInfo` carries `crm_ns`, `crm_name`, `crm_ver`, `abi_hash`, and
  `signature_hash` in the Rust handshake codec and PyO3 projection.
- Native server registration rejects invalid CRM tags and mismatched
  route/spec metadata, including malformed or mismatched descriptor hashes.
- Direct IPC `cc.connect(..., address="ipc://...")` validates the expected CRM
  contract through the Rust IPC client, returns a route-bound native client, and
  remains relay-independent.
- Relay-discovered local IPC validates both relay-advertised endpoint identity
  and the complete route contract before use; contract mismatch is a hard error,
  not an HTTP fallback trigger.
- Relay `/_resolve`, `_probe`, and data-plane calls can carry expected CRM
  identity plus descriptor hashes so stale or incompatible routes are filtered or
  rejected before a CRM method call is sent.
- Relay control registration publishes attested IPC handshake metadata in normal
  mode; claim-only metadata is accepted only for explicit skip-validation test
  paths and must still contain valid hashes.
- Thread-local same-process connects still pass Python objects directly, but
  compare the requested CRM contract against the registered local slot before
  returning a zero-serialization proxy.

Strict cross-language annotation grammar and custom transferable ABI declarations
remain future work. The current Python descriptor hash is sufficient to prevent
same-SDK half-upgrades from binding a route with different method/signature
shape, but it is not a cross-language IDL contract.

## Historical Execution Order

1. Remote IPC scheduler config, with explicit zero-copy guardrails.
2. Direct IPC probe/control helpers in `c2-ipc`.
3. Native runtime/session design for standalone IPC plus optional relay
   projection.
4. Canonical Python error facade over `c2-error`.
5. Hold lease tracking and server readiness cleanup.
6. Response allocation decision without payload-copy regression.
7. Config validation deduplication.
8. CRM contract descriptor and route validation, without moving
   language-specific method discovery into Rust.

## Regression Matrix For Future Work

Every implementation spawned from this reference should preserve:

- No-relay direct IPC cross-process success.
- Bad relay does not affect explicit `ipc://` connection.
- Name-only connect without relay fails clearly when the resource is not local.
- Relay-aware name resolution uses Rust relay-aware HTTP client when configured.
- Relay anchor resolution cannot trick a client configured with a nonlocal
  anchor into dialing arbitrary local IPC.
- Relay-discovered IPC cannot be accepted from route names alone; the IPC
  handshake must prove the exact relay-advertised server instance before any
  CRM call is sent over that IPC client.
- Named route clients returned by `cc.connect(...)` must remain bound to the
  validated route contract; a validated direct IPC or relay client cannot reuse
  the same native object to call a different named route without a fresh
  expected contract.
- Route contract hashes (`abi_hash`, `signature_hash`) must be present in named
  route registration, IPC handshake metadata, relay route state, resolve/probe
  expectations, and direct IPC validation. Empty or missing hashes are valid only
  for route-less admin probes, not for named CRM routes.
- Remote relay HTTP calls use the resolved route's `relay_url` directly rather
  than adding an anchor-relay forwarding hop.
- Thread-local same-process calls do not serialize Python objects.
- Remote IPC preserves the Core-owned request snapshot, moves the same `Vec` into `PyCoreRequestBuffer` for Python `memoryview` access, and retains the original transport lease through invocation.
- Large result replies keep using Rust transport selection and avoid avoidable
  copies.
- Chunked request/response lifecycle cleanup remains Rust-owned: no client or
  server chunk assembly should rely on Python SDK cleanup, receive-loop natural
  exit, or process teardown as the only release path.
- Relay stale-owner replacement remains Rust-owned and identity-attested:
  preliminary owner probes cannot authorize commit, final proof is created after
  candidate IPC identity and route-contract attestation, and any future stronger
  cross-process owner fencing must live in relay/IPC state rather than SDK-side
  retry, replay, or route-name trust.

## Notes For Future SDK Authors

The intended SDK shape is:

```text
Language SDK
  - language contract/decorator/interface model
  - language object invocation
  - language serialization hooks
  - thin typed config facade
  - thin proxy facade

Rust core/native
  - config/default/env resolution
  - direct IPC server/client/session lifecycle
  - wire protocol
  - SHM/chunk/file-spill memory mechanisms
  - remote IPC scheduling
  - relay-aware route fallback
  - canonical error registry/encoding
```

Do not move language-specific transferable behavior into Rust merely for
symmetry. Do move any behavior that a second SDK would otherwise have to copy
verbatim.
