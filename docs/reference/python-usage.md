# Python SDK usage

This guide describes C-Two 0.10.1 / c3 0.6.0 Python SDK usage, including CRM authoring, calls, portable FastDB 0.2.4 payloads, and Runtime ownership.

English · [简体中文](python-usage.zh-CN.md)

The [0.10.0 release notes](../releases/0.10.0.md) describe migration from 0.9.x, including `cc.Runtime()`, `cc.endpoint.server_credential()`, Unix schema 4 and the additional binding reason. CRM contracts expose stateful resources over same-process calls, local IPC and HTTP relay; portable Python payloads use the optional FastDB 0.2.4 extra. For installation and a runnable introduction, see the [quickstart](../../README.md#quickstart).

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

`cc.hold(echo.echo)(source)` retains a response as `HeldResult`, which owns a local copy: the transport memory was released right after the copy, so holding never keeps the server's shared memory. Explicit `release()` runs FastDB invalidation once, then frees the local copy; invalidation does not replay and the value stays inaccessible afterwards. Release an additional exported memoryview before retrying an export rejection, which occurs before invalidation. Manually constructed `HeldResult(release_cb=...)` callbacks still run once. Use `with cc.hold(...) as held` for normal scopes and materialize values that must outlive them.

`cc.observe.memory_stats()['holds']` counts these local copies: `active_holds`, `total_held_bytes` (logical bytes, not SHM capacity), `oldest_hold_seconds` and `by_direction`. See [memory policy](memory-policy.md#statistics-and-payload-lifetime) for the backing and reassembly cells and their accounting limits.

## Remote connections and deadlines

Use the resource process's `cc.server_address()` value for direct IPC. Direct IPC works without a relay. For discovery by route name, set `cc.set_relay_anchor(...)` before registration or connection and run `c3 relay` separately. Calls in the registering process prefer same-process dispatch when no explicit address is supplied.

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

Connection `timeout` is one budget in seconds across pool waiting, OS connection, handshake, current route lookup and relay acquisition. Omission or `None` adds no caller deadline; zero expires immediately. A timeout replaces the fixed five-second guards for the OS connection and the handshake, and the namespace gate or pipe wait also runs until it; without one, those guards remain. Negative and non-finite values are rejected. Expiration raises `CallDeadlineExceeded` with `operation=connect`, `transport_phase=pre_dispatch` and the failed `stage`; no resource method has run.

Business-call waiting is independent. `cc.with_call_options` returns a view of an existing connection; its timeout does not become a resource method argument. A call deadline can end caller waiting after dispatch while native work still holds the input until completion. It does not cancel a running resource method or authorize replay. Direct IPC waits indefinitely by default; HTTP uses the configured logical-call budget. Same-process synchronous calls accept inherited or unlimited waiting. See [configuration](configuration.md#call-deadlines-and-admission) for defaults and finite-call admission limits.

Unix processes sharing an IPC domain use the same configured root (`cc.endpoint.set_local_endpoint(root=...)` or `C2_IPC_ROOT`). The application provisions custom roots; `0755` is accepted with verified ownership and protection from other-user mutation. The default is `0700`, and sockets are `0600`. Windows uses TokenUser-scoped Named Pipes across logons and rejects Unix root overrides. Native connections require the same user and a server integrity of at least Medium; cross-session payloads automatically avoid inaccessible Local mappings. C-Two 0.10.0 moves the RPC handshake from 13 to 14 behind a permanent fixed version preamble, so upgrade communicating peers together. Different versions that both implement the fixed preamble raise `CoreError` with `category='protocol_version_mismatch'` and nonzero `local_protocol_version` / `peer_protocol_version` (`None` on every other error); mixing with 0.9.x still yields a generic connection/protocol failure. See the [0.10.0 migration](../releases/0.10.0.md) and the [handshake spec](ipc-handshake.md). See [endpoint configuration](configuration.md#local-endpoint-directories) and [lifecycle management](local-endpoint-lifecycle.md).

## Low-level server bridge

`c_two.transport.Server` is a bridge bound to one native `RuntimeSession`. Applications normally use the module-level `cc.register()` / `cc.shutdown()` or an explicit `cc.Runtime()` instance. Low-level hosts can obtain a bridge from their explicitly configured session:

```python
from c_two._native import RuntimeSession

session = RuntimeSession(server_id='local-host', use_process_relay_anchor=False)
server = session.ensure_server_bridge()
```

The session owns identity, IPC overrides and relay configuration. `Server(runtime_session, ...)` no longer accepts `bind_address`, `ipc_overrides`, `server_id` or `server_instance_id`; registration, unregister and shutdown no longer accept a replacement session or per-operation relay anchor. Configure independent sessions explicitly rather than inheriting Python process settings. `start()` takes no timeout argument; `shutdown(timeout=...)` retains its Core-owned deadline and completion result. Close the bridge with `shutdown()` when its host is finished.

## Payload lifetimes

The proven portable receive path opens a **copy-backed** payload owner. `cc.hold()` is a lifetime contract, not a zero-copy claim: it retains the C-Two response lease together with the payload owner and guarantees that release invalidates the payload owner and its checked views *before* the lease goes back.

```python
with cc.hold(echo.echo)(source) as held:
    payload = held.value                    # FastDB Payload owner; checked views stay valid
    ...
# leaving the block (or held.release()) invalidates owner and views, then frees the lease
```

Release is layered: explicit `.release()`, the `with` context manager, and a `__del__` fallback that warns if you forget both. `cc.observe.memory_stats()["holds"]` reports active holds for monitoring.

`held.unsafe_buffer` exposes the retained raw wire buffer as a `memoryview` escape hatch. Raw NumPy arrays or pointers derived from it bypass FastDB's payload owner and checked views and **cannot be revoked mechanically** — materialize values through FastDB before storing them beyond the hold scope.

On the server side, portable inputs are owned by default. Registering with `cc.register(..., input_lifetime={...: 'borrowed'})` opts specific methods into call-scoped borrowed inputs: C-Two invalidates the payload and its checked views before releasing the request lease when the call returns or raises. Do not retain a borrowed payload or view after the method returns.

`cc.transfer` accepts only `input` and `output`. The former `buffer` keyword is removed, including `None` and `"view"`; passing it raises `TypeError`. Remove that keyword and select borrowed inputs through registration-level `input_lifetime`. The module-level `auto_transfer` follows the same rule.

## Entry points

### Python SDK

The `c_two` root holds only the everyday API: contract decorators, `hold` / `HeldResult`, register / connect / close / unregister / serve / shutdown, `server_address`, `with_call_options`, setters, `Runtime`, `ConcurrencyConfig` and `error`. Endpoint maintenance, diagnostics and contract tooling live in `c_two.endpoint`, `c_two.observe` and `c_two.contract`; the Rust SDK uses the same three modules under `c_two::`. Do not add root names, aliases or second spellings for these capabilities; option values such as concurrency modes and input lifetimes are string literals.

Applications start with `import c_two as cc`. Import the specialized modules when needed:

```python
from c_two import owner, endpoint, observe, contract
```

Rust users import the user-facing package as `c_two`, not by a Core or transport crate name:

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

Generated Rust modules may use the hidden `c_two::generated` seam. Ordinary application code must not assemble `c2-ipc`, `c2-http`, `c2-server`, `SyncClient`, or route bindings directly.

Contract export does not select a Runtime. Concurrency modes are `'parallel'`, `'exclusive'`, and `'read_parallel'`; registration input lifetimes are `'materialized'` and `'borrowed'`.

- Authoring: `@cc.crm`, `@cc.read`, `@cc.write`, `@cc.on_shutdown`, `@cc.transfer`, `cc.hold`
- Calls: `cc.with_call_options` (per-call deadline views, see [remote connections and deadlines](#remote-connections-and-deadlines))
- Runtime: `cc.Runtime`, whose instance methods include `set_local_endpoint`, `set_server`, `set_client`, `set_transport_policy`, `set_relay_anchor`, `set_call_execution_limits`, `register`, `connect`, `unregister`, `start`, `server_id`, `server_address`, `server_credential`, `local_endpoint_context`, `shutdown`, `connection_events`, `lifecycle_snapshot`, `shutdown_outcome`, `memory_stats` and `call_execution_snapshot`
- Default Runtime: `cc.register`, `cc.connect`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.endpoint.server_credential`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.set_transport_policy`, `cc.endpoint.set_local_endpoint`; `cc.close(proxy)` follows the proxy's own connection
- Contracts: `cc.contract.export_descriptor`, `cc.contract.release_ref`, `cc.contract.compile_artifacts`, `cc.contract.diagnostics`
- Endpoint maintenance: `cc.endpoint.inspect_endpoint`, `cc.endpoint.reap_endpoint`, `cc.endpoint.sweep_endpoints`, `cc.endpoint.EndpointCredential` (immutable native value; explicit JSON import/export)
- Connection observations: `cc.observe.connection_observation`, `cc.observe.connection_events`, `cc.observe.ConnectionObservation`, `cc.observe.ConnectionClosed`, `cc.observe.ConnectionEvent`, `cc.observe.ConnectionEvents`, `cc.observe.ConnectionEventsLagged`
- Errors: `cc.error.CoreError` (structured local/Core errors), `cc.error.CCError` (semantic errors), with common `cc.error.CCBaseError`
- Monitoring: `cc.observe.memory_stats`, `cc.observe.call_execution_snapshot`, `cc.observe.lifecycle_snapshot`, `cc.observe.shutdown_outcome` (read-only)

Use `cc.set_server(startup_timeout=...)` before registration to choose a native startup wait. Startup expiry has `CoreError.lifecycle_kind == 'startup_timeout'`, `startup_stage` and `timeout_seconds`; see [startup configuration and errors](configuration.md#server-startup-timeout).

After a listener reaches Ready, `cc.endpoint.server_credential() -> cc.endpoint.EndpointCredential | None` returns its native credential from the existing lifecycle watch without inspection or filesystem I/O. It returns `None` before start and after terminal shutdown; a snapshot is not proof of liveness or cleanup authority. See the [lifecycle guide](local-endpoint-lifecycle.md#exact-cleanup-and-bounded-sweeps).

Runtime-scoped IPC policy, call admission limits and relay settings are documented in the [configuration guide](configuration.md).

The [lifecycle API guide](local-endpoint-lifecycle.md) specifies observation return values, timeout/end/lag behavior and error outcomes. Its [bind/connect section](local-endpoint-lifecycle.md#bind-and-connect-failures) documents the structured OS fields on public `cc.error.CoreError`. These observations do not prove process exit or authorize replay or cleanup.

### CRM contracts, clients and names

CRM contract classes are interfaces with `...` method bodies. `@cc.crm()` requires `namespace` and `version`; contract class names have no `I` prefix. Resource implementations are plain classes named for the domain, such as `NestedGrid` or `PostgresVectorLayer`; `{ContractName}Impl` is acceptable for generated code. Transferable classes use descriptive data names such as `GridAttribute`.

```python
@cc.crm(namespace='cc.demo', version='0.1.0')
class Grid:
    def some_method(self, arg: int) -> str:
        ...
```

Any code calling `cc.connect(CRMClass, name='...', address='...')` is a client. Its typed proxy matches the CRM contract and supports context-manager close:

```python
with cc.connect(Grid, name='grid', address='ipc://server') as grid:
    result = grid.subdivide_grids([1], [0])
```

There is no separate `compo/` module, `@cc.runtime.connect` decorator, or `Component` type. Routing names are user-chosen strings passed to `cc.register(name=...)` and `cc.connect(name=...)`, distinct from the CRM `namespace`. Different CRM contracts, or different instances of one contract, can coexist under distinct names. For remote IPC and relay paths, the native runtime also matches the expected CRM tag and contract hashes derived from the client's CRM class.

### Default Runtime API pattern

Module-level registration, connection, configuration and shutdown delegate to the default Python Runtime. `cc.close(proxy)` follows the Runtime that created the proxy. `cc.Runtime()` creates an application-owned instance with the same register / connect / configure / shutdown methods and an empty-Host `start()`:

```python
import c_two as cc

# Server side
cc.set_relay_anchor('http://relay-host:8080')
cc.set_transport_policy(shm_threshold=64 * 1024)
cc.set_server(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
cc.register(Grid, grid_instance, name='grid')
cc.register(Network, net_instance, name='network')
cc.serve()

# Client side, same process: thread preference
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side, direct IPC
cc.set_client(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
grid = cc.connect(Grid, name='grid', address='ipc://my_server')
grid.some_method(arg)
cc.close(grid)

# Client side, relay-based name resolution and routing
cc.set_relay_anchor('http://relay-host:8080')
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Cleanup
cc.unregister('grid')
cc.shutdown()
```

### Independent Python Runtime instances

`cc.Runtime()` creates independent Python bindings over one native session. Its `set_*`, `register`, `connect`, `unregister` and `shutdown` methods have the module-level signatures. Module functions use a separate default Runtime. Each explicit instance starts with empty code overrides; process environment and `.env` remain shared configuration sources, resolved by Core at the usual freeze points. Use instance setters when different Runtime instances need different settings. Independence means separate route, connection, observation and configuration ownership within one process.

`runtime.start()` starts a Host with no registered CRM and waits for native readiness and the existing IPC PING/PONG. A later `runtime.register()` uses that Host. Without an explicit address, `runtime.connect()` searches only that instance's local routes; use `address=other.server_address()` and the same Unix endpoint context for a cross-instance IPC call. `cc.close(proxy)`, its context manager, `cc.hold()` and `cc.with_call_options()` follow the Runtime that created the proxy. Explicit instances have no `serve()` method or automatic signal/atexit cleanup; `cc.serve()` handles the module-level default Runtime.

Calling `runtime.shutdown(timeout=...)` starts the Core Runtime's single permanent retirement scope for that SDK session; module-level `cc.shutdown()` does the same for the default Runtime. This scope seals the Core client pool and closes admission for new registration, start and remote connection work. A pending observation keeps the old session and its slots; later Core completion alone does not replace them. Call SDK shutdown again to consume the completed receipt and perform the session handoff. A zero or short wait budget limits only this caller's observation and does not cancel the driver. If the deadline expires before retirement is handed off, shutdown raises `CoreError` with `lifecycle_kind='shutdown_wait_timeout'`, `details.stage` and `timeout_seconds`; this is not a pending receipt. Python's numeric conversion happens before the SDK instance lock. Waiting to acquire that lock remains outside the shutdown deadline; after acquiring it, Native Host, Core and pool waits share one absolute deadline. Shutdown error projection and shutdown hooks run outside the Python binding lock. Hooks run after session replacement; if replacement construction fails, the original slots and journal remain available.

`cc.serve(shutdown_timeout=30.0)` rejects an invalid budget before serving, blocks until SIGINT or SIGTERM, then calls `shutdown(timeout=shutdown_timeout)` once and returns that outcome. A pending outcome means cleanup continues in the background; a later `cc.shutdown()` observes its completion.

The deadline passed to remote `connect()` also bounds the SDK's final publication check. If that original deadline expires before publication, connect raises the corresponding deadline error and releases only its own logical client; it does not retry. An otherwise successful connection continues to reuse its pooled client normally. A `runtime_retired` lifecycle error has no I/O cause. HTTP-only use does not create an IPC pool, and shutdown does not promise that every HTTP keepalive socket has physically closed. The Rust low-level Host shutdown/restart lifecycle remains separate and restartable; this permanent retirement applies to the Python SDK Runtime scope. IPC handshake 14, SHM 3 and C/FFI ABI 6 are unchanged. R07 final Drop remains separate work.

After completed handoff, configured endpoint selection, transport policy, call limits and relay override retain their existing handoff behavior, while server identity, IPC overrides and startup timeout reset. An existing `connection_events()` subscription observes its original session only; subscribe again for the new session. The return value is the closing session's result, and held payloads retain their own lifetime. An application owns any custom endpoint directory it creates; the Runtime does not remove that directory. The [preflight example](../../examples/python/independent_runtime.py) checks the shutdown outcome before deleting its newly created private directory.
