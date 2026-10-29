# Python SDK usage

This guide describes the Python APIs of C-Two 0.10.0 / c3 0.6.0; the [release notes](releases/0.10.0.md) describe the migration from 0.9.x, including `cc.Runtime()`, `cc.server_credential()`, Unix schema 4 and the additional binding reason. CRM contracts expose stateful resources over same-process calls, local IPC and HTTP relay; portable Python payloads use the optional FastDB 0.2.3 extra. For installation and a runnable introduction, see the [quickstart](../README.md#quickstart).

## Portable payloads (FastDB)

Portable methods use the explicit FastDB binding below.

Methods that must move structured data across language boundaries bind an explicit FastDB specification. FastDB Core owns the nested payload semantics — validation, canonical identity, binary layout, builders, views, invalidation, and payload-only codegen — while C-Two owns the outer contract, routing, transport, and lifetimes:

```python
import json

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

VALUE_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one",
         "type": {"kind": "u8", "nullable": False}},
    ],
    "components": [],
}


@cc.crm(namespace='demo.payload', version='0.1.0')
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload: ...


def build_value(value: int) -> Payload:
    spec = CompiledSpec.compile(json.dumps(VALUE_SPEC).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


class EchoResource:
    def echo(self, payload: Payload) -> Payload:
        return payload


cc.register(Echo, EchoResource(), name='echo')
source = build_value(7)
try:
    with cc.connect(Echo, name='echo') as echo:
        result = echo.echo(source)
        result.close()
finally:
    source.close()
    cc.unregister('echo')
    cc.shutdown()
```

A portable method carries zero or one `Payload` envelope in each direction; C-Two embeds each nested specification as an opaque JSON value and never reinterprets it. Methods without an explicit binding can still use ordinary Python values for Python-scoped prototyping, but portable descriptor export and codegen diagnose and reject them.

`cc.hold(echo.echo)(source)` retains a response as `HeldResult`. Explicit `release()` first attempts FastDB invalidation once, then releases native storage. When native cleanup fails, the original response stays with the result for another `release()` call; invalidation does not replay and the value stays inaccessible. Release an additional exported memoryview before retrying an export rejection, which occurs before invalidation. Manually constructed `HeldResult(release_cb=...)` callbacks still run once. Use `with cc.hold(...) as held` for normal scopes and materialize values that must outlive them.

`cc.memory_stats()` includes `abandoned_releases` in each native memory scope. Final consuming cleanup failures retain unreturned charges and count one abandoned obligation, with the error returned or logged by its native carrier. No background recovery runs. The counter includes uncertain published SHM outcomes that a peer may later release. An abnormal same-user SHM peer can assert invalid, replayed or stale allocation coordinates; unreleaseable asserted references also count and can make this counter exceed actual unreleased allocations. See [memory policy](memory-policy.en.md#statistics-and-payload-lifetime) for its accounting limits.

## Remote connections and deadlines

Use the resource process's `cc.server_address()` value for direct IPC. Direct IPC works without a relay. For discovery by resource name, set `cc.set_relay_anchor(...)` before registration or connection and run `c3 relay` separately. Calls in the registering process prefer same-process dispatch when no explicit address is supplied.

```python
source = build_value(7)
try:
    # Replace the address with the resource process's cc.server_address().
    with cc.connect(Echo, name='echo', address='ipc://resource-process', timeout=1.0) as echo:
        reply = cc.with_call_options(echo, timeout=5.0).echo(source)
        reply.close()
finally:
    source.close()
```

Connection `timeout` is one budget in seconds across pool waiting, OS connection, handshake, current route lookup and relay acquisition. Omission or `None` adds no caller deadline; zero expires immediately. Negative and non-finite values are rejected. Expiration raises `CallDeadlineExceeded` with `operation=connect`, `transport_phase=pre_dispatch` and the failed `stage`; no resource method has run.

Business-call waiting is independent. `cc.with_call_options` returns a view of an existing connection; its timeout does not become a resource method argument. A call deadline can end caller waiting after dispatch while native work still holds the input until completion. It does not cancel a running resource method or authorize replay. Direct IPC waits indefinitely by default; HTTP uses the configured logical-call budget. Same-process synchronous calls accept inherited or unlimited waiting. See [configuration](configuration.en.md#call-deadlines-and-admission) for defaults and finite-call admission limits.

Unix processes sharing an IPC domain use the same configured root (`cc.set_local_endpoint(root=...)` or `C2_IPC_ROOT`). The application provisions custom roots; `0755` is accepted with verified ownership and protection from other-user mutation. The default is `0700`, and sockets are `0600`. Windows uses TokenUser-scoped Named Pipes across logons and rejects Unix root overrides. Native connections require the same user and a server integrity of at least Medium; cross-session payloads automatically avoid inaccessible Local mappings. C-Two 0.10.0 moves the RPC handshake from 13 to 14 behind a permanent fixed version preamble, so upgrade communicating peers together. Different versions that both implement the fixed preamble raise `CoreError` with `category='protocol_version_mismatch'` and nonzero `local_protocol_version` / `peer_protocol_version` (`None` on every other error); mixing with 0.9.x still yields a generic connection/protocol failure. See the [0.10.0 migration](releases/0.10.0.md) and the [handshake spec](ipc-handshake.md). See [endpoint configuration](configuration.en.md#local-endpoint-directories) and [lifecycle management](local-endpoint-lifecycle.en.md).

## Low-level server bridge

`c_two.transport.Server` is a bridge bound to one native `RuntimeSession`. Applications normally use the module-level `cc.register()` / `cc.shutdown()` or an explicit `cc.Runtime()` instance. Low-level hosts can obtain a bridge from their explicitly configured session:

```python
from c_two._native import RuntimeSession

session = RuntimeSession(server_id='local-host', use_process_relay_anchor=False)
server = session.ensure_server_bridge()
```

The session owns identity, IPC overrides and relay configuration. `Server(runtime_session, ...)` no longer accepts `bind_address`, `ipc_overrides`, `server_id` or `server_instance_id`; registration, unregister and shutdown no longer accept a replacement session or per-operation relay anchor. Configure independent sessions explicitly rather than inheriting Python process settings. `start()` takes no timeout argument; `shutdown(timeout=...)` retains its Core-owned deadline and completion result. Close the bridge with `shutdown()` when its host is finished.

## Payload lifetimes

The proven portable receive path opens a **copy-backed** FastDB owner. `cc.hold()` is a lifetime contract, not a zero-copy claim: it retains the C-Two response lease together with the payload owner and guarantees that release invalidates the FastDB owner and its checked views *before* the lease goes back.

```python
with cc.hold(echo.echo)(source) as held:
    payload = held.value                    # FastDB Payload owner; checked views stay valid
    ...
# leaving the block (or held.release()) invalidates owner and views, then frees the lease
```

Release is layered: explicit `.release()`, the `with` context manager, and a `__del__` fallback that warns if you forget both. `cc.hold_stats()` reports active holds for monitoring.

`held.unsafe_buffer` exposes the retained raw wire buffer as a `memoryview` escape hatch. Raw NumPy arrays or pointers derived from it bypass FastDB's checked owner/view model and **cannot be revoked mechanically** — materialize values through FastDB before storing them beyond the hold scope.

On the server side, portable inputs are owned by default. Registering with `cc.register(..., input_lifetime={...: cc.InputLifetime.BORROWED})` opts specific methods into call-scoped borrowed inputs: C-Two invalidates the payload and its checked views before releasing the request lease when the call returns or raises. Do not retain a borrowed payload or view after the method returns.

`cc.transfer` accepts only `input` and `output`. The former `buffer` keyword is removed, including `None` and `"view"`; passing it raises `TypeError`. Remove that keyword and select borrowed inputs through registration-level `input_lifetime`. The module-level `auto_transfer` follows the same rule.

## Entry points

### Python SDK

The main user surface, shown throughout the guides. The top-level `cc` namespace groups:

- Authoring: `@cc.crm`, `@cc.read`, `@cc.write`, `@cc.on_shutdown`, `@cc.transfer`, `cc.hold`, `cc.InputLifetime`
- Calls: `cc.with_call_options` (per-call deadline views, see [remote connections and deadlines](#remote-connections-and-deadlines))
- Runtime: `cc.Runtime`, whose instance methods include `set_local_endpoint`, `set_server`, `set_client`, `set_transport_policy`, `set_relay_anchor`, `set_call_execution_limits`, `register`, `connect`, `unregister`, `start`, `server_id`, `server_address`, `server_credential`, `local_endpoint_context`, `shutdown`, `connection_events`, `native_lifecycle_snapshot`, `native_terminal_outcome`, `memory_stats`, `hold_stats`, `call_execution_snapshot` and `prepare_owner_takeover`
- Default Runtime: `cc.register`, `cc.connect`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.server_credential`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.set_transport_policy`, `cc.set_local_endpoint`; `cc.close(proxy)` follows the proxy's own connection
- Contracts: `cc.export_contract_descriptor`, `cc.export_contract_release_ref`, `cc.compile_contract_artifacts`, `cc.infer_crm_from_resource`
- Lifecycle and ownership: `cc.LifecycleConfig`, `cc.owner_control_pair`, `cc.spawn_owned_child`, `cc.adopt_owner_stdin`, `cc.OwnerControl`, `cc.OwnerRecoveryTicket`, `cc.prepare_owner_takeover`, `cc.PreparedOwnerTakeover`, `cc.OwnerStatus`, `cc.OwnerControlError`
- Endpoint maintenance: `cc.inspect_endpoint`, `cc.reap_endpoint`, `cc.sweep_endpoints`, `cc.EndpointCredential` (immutable native value; explicit JSON import/export)
- Connection observations: `cc.connection_observation`, `cc.connection_events`, `cc.ConnectionObservation`, `cc.ConnectionClosed`, `cc.ConnectionEvent`, `cc.ConnectionEvents`, `cc.ConnectionEventsLagged`
- Errors: `cc.error.CoreError` (structured local/Core errors), `cc.error.CCError` (semantic errors), with common `cc.error.CCBaseError`
- Monitoring: `cc.hold_stats`, `cc.memory_stats`, `cc.call_execution_snapshot`, `cc.native_lifecycle_snapshot`, `cc.native_terminal_outcome` (read-only)

Use `cc.set_server(startup_timeout=...)` before registration to choose a native startup wait. Startup expiry has `CoreError.lifecycle_kind == 'startup_timeout'`, `startup_stage` and `timeout_seconds`; see [startup configuration and errors](configuration.en.md#server-startup-deadlines).

After a listener reaches Ready, `cc.server_credential() -> cc.EndpointCredential | None` returns its native credential from the existing lifecycle watch without inspection or filesystem I/O. It returns `None` before start and after terminal shutdown; a snapshot is not proof of liveness or cleanup authority. See the [lifecycle guide](local-endpoint-lifecycle.en.md#exact-cleanup-and-bounded-sweeps).

Runtime-scoped IPC policy, call admission limits and relay settings are documented in the [configuration guide](configuration.en.md).

The [lifecycle API guide](local-endpoint-lifecycle.en.md) specifies observation return values, timeout/end/lag behavior, owner ticket issuance and revocation, the single-commit takeover sequence and error outcomes. Its [bind/connect section](local-endpoint-lifecycle.en.md#bind-and-connect-failures) documents the structured OS fields on public `cc.error.CoreError`. These observations do not prove process exit or authorize replay or cleanup.

### Independent Python runtimes

`cc.Runtime()` creates an independent Python binding owner and native session. Its `set_*`, `register`, `connect`, `unregister` and `shutdown` methods have the module-level signatures. Module functions use a separate default Runtime. Each explicit instance starts with empty code overrides; process environment and `.env` remain shared configuration sources, resolved by Core at the usual freeze points. Use instance setters when different runtimes need different settings. Independence means separate route, connection, observation and configuration ownership within one process.

`runtime.start()` starts a Host with no registered CRM and waits for native readiness and the existing IPC PING/PONG. A later `runtime.register()` uses that Host. Without an explicit address, `runtime.connect()` searches only that instance's local routes; use `address=other.server_address()` and the same Unix endpoint context for a cross-instance IPC call. `cc.close(proxy)`, its context manager, `cc.hold()` and `cc.with_call_options()` follow the proxy's original owner. Explicit instances have no `serve()` method or automatic signal/atexit cleanup; `cc.serve()` handles the module-level default Runtime.

Calling `runtime.shutdown(timeout=...)` starts the Core Runtime's single permanent retirement scope for that SDK session; module-level `cc.shutdown()` does the same for the default Runtime. This scope seals the Core client pool and closes admission for new registration, start and remote connection work. A pending observation keeps the old session and its slots; later Core completion alone does not replace them. Call SDK shutdown again to consume the completed receipt and perform the session handoff. A zero or short wait budget limits only this caller's observation and does not cancel the driver. If the deadline expires before retirement is handed off, shutdown raises `CoreError` with `lifecycle_kind='shutdown_wait_timeout'`, `details.stage` and `timeout_seconds`; this is not a pending receipt. Python's numeric conversion happens before the SDK instance lock. Waiting to acquire that lock remains outside the shutdown deadline; after acquiring it, Native Host, Core and pool waits share one absolute deadline. Shutdown error projection and shutdown hooks run outside the Python binding lock. Hooks run after session replacement; if replacement construction fails, the original slots and journal remain available.

The deadline passed to remote `connect()` also bounds the SDK's final publication check. If that original deadline expires before publication, connect raises the corresponding deadline error and releases only its own logical client; it does not retry. An otherwise successful connection continues to reuse its pooled client normally. A `runtime_retired` lifecycle error has no I/O cause. HTTP-only use does not create an IPC pool, and shutdown does not promise that every HTTP keepalive socket has physically closed. The Rust low-level Host shutdown/restart lifecycle remains separate and restartable; this permanent retirement applies to the Python SDK Runtime scope. IPC handshake 14, SHM 3 and C/FFI ABI 6 are unchanged. R07 final Drop remains separate work.

After completed handoff, configured endpoint selection, transport policy, call limits and relay override retain their existing handoff behavior, while server identity, IPC overrides, startup timeout and one-use owner capability reset. An existing `connection_events()` subscription observes its original session only; subscribe again for the new session. The return value is the closing session's result, and held payloads retain their own lifetime. An application owns any custom endpoint directory it creates; the Runtime does not remove that directory. The [preflight example](../examples/python/independent_runtime.py) checks the shutdown outcome before deleting its newly created private directory.
