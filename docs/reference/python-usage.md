# Python SDK usage

This guide describes C-Two 0.11.0 / c3 0.7.0 Python SDK usage, including CRM authoring, calls, server streams, portable FastDB 0.2.4 payloads, and Runtime ownership.

English · [简体中文](python-usage.zh-CN.md)

The [0.11.0 release notes](../releases/0.11.0.md) describe migration from 0.10.x, including IPC 16, SHM 4, server streams, route-name identity, role-domain capacity configuration and the converged error hierarchy. CRM contracts expose stateful resources over same-process calls, local IPC and HTTP relay; portable Python payloads use the optional FastDB 0.2.4 extra. For installation and a runnable introduction, see the [quickstart](../../README.md#quickstart).

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
    # Passing cc.server_address() routes through local IPC; omitting address uses same-process direct dispatch.
    with cc.connect(Echo, name='echo', address=cc.server_address()) as echo:
        result = echo.echo(source)
        result.close()
finally:
    source.close()
    cc.unregister('echo')
    cc.shutdown()
```

A portable method carries zero or one `Payload` envelope in each direction; C-Two embeds each nested specification as an opaque JSON value and never reinterprets it. Methods without an explicit binding can still use ordinary Python values for Python-scoped prototyping, but portable descriptor export and codegen diagnose and reject them.

Over IPC or relay HTTP, `cc.hold(echo.echo)(source)` retains the decoded response as `HeldResult`, which owns a local copy: the transport memory was released right after the copy, so holding never keeps the server's shared memory. Explicit `release()` runs FastDB invalidation once, then frees the local copy; invalidation does not replay and the value stays inaccessible afterwards. Release an additional exported memoryview before retrying an export rejection, which occurs before invalidation. On the same-process direct path (omitting `address` in the registering Runtime), `cc.hold()` returns `HeldResult(result)` wrapping the resource's original Python object without transport copying, FastDB invalidation, or `holds` accounting. Manually constructed `HeldResult(release_cb=...)` callbacks still run once. Use `with cc.hold(method)(args) as held` for normal scopes and materialize values that must outlive them.

`cc.observe.memory_stats()['holds']` counts transport-decoded local copies: `active_holds`, `total_held_bytes` (logical bytes, not SHM capacity), `oldest_hold_seconds` and `by_direction`. See [memory policy](memory-policy.md#statistics-and-payload-lifetime) for the role domains’ memory and file spill cells and their accounting limits.

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

Business-call waiting is independent. `cc.with_call_options` returns a view of an existing connection; its timeout does not become a resource method argument. A call deadline can end caller waiting after dispatch: the dispatched call keeps its `max_outstanding_calls` slot until its real terminal, whereas the request's outgoing storage and any IPC SHM loan can be released earlier once the transport has written or copied the request. Expiry does not cancel a running resource method or authorize replay. Direct IPC and relay HTTP wait indefinitely by default. Same-process synchronous calls accept inherited or unlimited waiting. See [configuration](configuration.md#call-deadlines-and-admission) for defaults and outstanding-call limits.

A connection follows its route name. When C-Two already knows that the registration it served is gone, because the server process ended or an earlier call was refused before dispatch for a missing, removed or closed route or another contract, the next call acquires the route again by name within its own deadline and goes to the current registration: direct IPC returns to the same address, a relay-aware connection resolves through its relay anchor and may change between local IPC and the relay, and an explicit relay connection stays on HTTP. A call that learns of the change only from its own refusal fails before dispatch and is safe to retry; the next call follows. A same-process connection does the same once its registration is unregistered: the next call goes to a new local registration of the name, or through the relay to a remote one, while calls go straight to the resource as long as the registration stays. The process that serves the name keeps its state continuous; use a different name when callers must notice a different state. Identity and protocol refusals do not start a new acquisition.

Unix processes sharing an IPC domain use the same configured root (`cc.endpoint.set_local_endpoint(root=...)` or `C2_IPC_ROOT`). The application provisions custom roots; `0755` is accepted with verified ownership and protection from other-user mutation. The default is `0700`, and sockets are `0600`. Windows uses TokenUser-scoped Named Pipes across logons and rejects Unix root overrides. Native connections require the same user and a server integrity of at least Medium; cross-session payloads automatically avoid inaccessible Local mappings. C-Two 0.11.0 uses RPC IPC handshake 16 behind the permanent fixed version preamble introduced in 0.10.0, so upgrade communicating peers together. Different versions that both implement the fixed preamble raise `CoreError` with `category='protocol_version_mismatch'` and nonzero `local_protocol_version` / `peer_protocol_version` (`None` on every other error); mixing with 0.9.x still yields a generic connection/protocol failure. See the [0.11.0 migration](../releases/0.11.0.md) and the [handshake spec](ipc-handshake.md). See [endpoint configuration](configuration.md#local-endpoint-directories) and [lifecycle management](local-endpoint-lifecycle.md).

## Low-level server bridge

`c_two.transport.Server` is a bridge bound to one native `RuntimeSession`. Applications normally use the module-level `cc.register()` / `cc.shutdown()` or an explicit `cc.Runtime()` instance. Low-level hosts can obtain a bridge from their explicitly configured session:

```python
from c_two._native import RuntimeSession

session = RuntimeSession(server_id='local-host', use_process_relay_anchor=False)
server = session.ensure_server_bridge()
```

`Server(runtime_session, ...)` uses the given session’s Host settings, including server identity, capacity and relay configuration. Registration, unregister and shutdown use that session. Configure independent sessions explicitly. `start()` takes no timeout argument. `shutdown(timeout=...)` observes the Core shutdown receipt. Call `shutdown()` when finished with the Host.

## Payload lifetimes

The proven remote portable receive path opens a **copy-backed** payload owner. Over remote transports, `cc.hold()` is a lifetime contract, not a zero-copy claim: it retains the response's local copy together with the payload owner and guarantees that release invalidates the payload owner and its checked views *before* freeing the local copy (on the same-process direct path, `cc.hold()` wraps the original Python object in `HeldResult(result)` without transport copies or FastDB invalidation).

```python
with cc.connect(Echo, name='echo', address=cc.server_address()) as echo:
    with cc.hold(echo.echo)(source) as held:
        payload = held.value                    # FastDB Payload owner; checked views stay valid
        ...
# leaving the block (or held.release()) invalidates the payload owner and checked views, then frees the local copy
```

Release is layered: explicit `.release()`, the `with` context manager, and a `__del__` fallback that warns if you forget both. `cc.observe.memory_stats()["holds"]` reports active holds for monitoring.

`held.unsafe_buffer` exposes the retained raw wire buffer as a `memoryview` escape hatch. Raw NumPy arrays or pointers derived from it bypass FastDB's payload owner and checked views and **cannot be revoked mechanically** — materialize values through FastDB before storing them beyond the hold scope.

On the server side, portable inputs are owned by default. Registering with `cc.register(..., input_lifetime={...: 'borrowed'})` opts specific methods into call-scoped borrowed inputs: the request lease is already released before the resource method runs. When the method returns or raises, C-Two invalidates the payload owner and its checked views, then frees the local input copy. Do not retain a borrowed payload or view after the method returns.

`cc.transfer` accepts only `input` and `output`. The former `buffer` keyword is removed, including `None` and `"view"`; passing it raises `TypeError`. Remove that keyword and select borrowed inputs through registration-level `input_lifetime`. The module-level `auto_transfer` follows the same rule.

## Server streams

A CRM method marked with `@cc.stream` returns zero or more items for one input. Annotate its return as `Iterator[T]` or `Generator[T, None, None]`, where `T` is one item; `@cc.transfer(output=...)` describes the encoding of one item, as it does for a unary return. `@cc.stream` combines with `@cc.read`, `@cc.write` and `@cc.transfer`. The resource implements the method as a generator, or returns an iterator:

```python
from collections.abc import Iterator

@cc.crm(namespace='science.sim', version='0.1.0')
class Simulation:
    @cc.stream
    @cc.transfer(input=PARAMETERS, output=STATE)
    def steps(self, parameters: Payload) -> Iterator[Payload]: ...

class BasinSimulation:
    def steps(self, parameters: Payload) -> Iterator[Payload]:
        model = initialize(parameters)
        try:
            while model.has_next():
                model.advance()
                yield model.export_owned_state()
        finally:
            model.finish()

cc.register(Simulation, BasinSimulation(), name='basin')

with cc.connect(Simulation, name='basin', address='ipc://resource-process') as sim:
    results = sim.steps(parameters)
    with results:
        for state in results:
            consume(state)
    outcome = results.close()
```

Each `next()` of the generator runs as one resource step: it holds the route permit and the GIL only while it runs, over IPC also an execution worker, and nothing between items. The stream copies the context once when it starts, and creating the generator, every `next()` and its `close()` all run in that one context, so `contextvars` and `decimal` contexts set inside the generator stay with it across workers and do not leak into them. Over IPC each item is encoded inside its step, so the generator may reuse or change an object after yielding it; within the process the consumer receives the yielded object itself. For transport-decoded calls registered with `cc.register(..., input_lifetime={'steps': 'borrowed'})`, the borrowed input stays valid until the generator has been closed, then is invalidated and released (same-process direct calls pass the caller's object directly without borrowed-input invalidation).

A call returns a `cc.Stream`, an iterator of decoded items. Leaving a `with` block or dropping the stream cancels it without waiting. `close(timeout=30.0)` cancels the stream if it is still running and then waits for the original call to finish: `None` waits without bound and `0` observes once. It returns a dict with `completed`, `error` and `complete_items`; while `completed` is false, the cleanup is still running and a later `close()` can observe it. `error` is the exception the stream ended with, or `None` after a natural end; a stream that the server or a relay ended reports `CallCancelled` naming shutdown, unregistration or relay shutdown. Over IPC or relay HTTP, `cc.hold(sim.steps)(parameters)` returns a stream of `HeldResult` items, each keeping its own local copy until released; several holds may coexist, and when they fill the client domain memory budget the next item waits until one is released (on the same-process direct path, each item is wrapped in `HeldResult(item)` without transport copies or `holds` accounting). A call deadline from `cc.with_call_options` covers the whole iteration and is not reset for each item; its expiry ends only the caller's wait. Every stream holds one `max_outstanding_calls` slot until its real terminal.

Within the registering process, a stream passes the yielded objects themselves, without encoding, and steps under the route's own concurrency limits; a finite call deadline is refused before dispatch. Over IPC, Python and Rust resources and clients stream to each other, and through a relay they stream over HTTP with the same `cc.Stream`, `close` and hold behavior; there a resource can run ahead of a consumer that stops reading by as many items as the HTTP and TCP buffers hold, which for small items can be thousands. [Runtime lifecycle](runtime-lifecycle.md#execution-admission) describes steps, cancellation and shutdown.

## Resident simulation pattern

CRM methods default to `@cc.write`. Under `ConcurrencyConfig(mode='read_parallel', max_workers=...)`, writes execute exclusively while concurrent reads share up to `max_workers` permits (still serialized around queued writes). For long-running stateful simulations that must remain responsive to state inspection, parameter interventions and live subscribers while advancing, do not block inside a single long-lived `@cc.write` step call. Instead:

- Run the simulation's time-stepping loop in a background thread owned by the resource, synchronizing internal state updates at step boundaries.
- Register the route with `ConcurrencyConfig(mode='read_parallel', max_workers=...)` so concurrent observers do not serialize behind one another.
- Keep contract methods short: expose `@cc.read` for snapshot queries, `@cc.write` for queueing interventions into the stepping loop, and `@cc.stream` combined with `@cc.read` for pushing published state frames. Ensure each stream `next()` step finishes promptly on its own or exits via an application-side signal before `unregister()` / `shutdown()` — Core drains any currently executing stream step before Python invokes `@cc.on_shutdown`, so `@cc.on_shutdown` performs final cleanup rather than waking an indefinitely blocked step.
- For what-if branching from live state, clone the resource state in application code, register the temporary branch under a new route name with `register()`, and call `unregister()` once the branch finishes; active streams and concurrent calls on the primary route continue unaffected.

See [resident_simulation.py](../../examples/python/resident_simulation.py) for a complete runnable example covering background stepping, live streaming, interventions, dynamic branch registration and clean shutdown.

### Deployment layering for web frontends

`c3 relay` is an internal routing fabric for C-Two peers, not a public browser-facing API; web browsers run inside a sandbox that cannot use local IPC or host a relay. When a web UI needs to inspect, steer or subscribe to a resident resource, place a Python backend gateway (BFF) on the application tier that connects to resources with standard `cc.connect(...)` over direct IPC or `c3 relay`, and expose HTTP or WebSocket endpoints from that gateway to browsers.

## Entry points

### Python SDK

The `c_two` root holds only the everyday API: contract decorators (`crm`, `read`, `write`, `stream`, `transfer`, `on_shutdown`), `Stream` / `StreamCloseReceipt`, `hold` / `HeldResult`, register / connect / close / unregister / serve / shutdown, `server_address`, `with_call_options`, setters, `Runtime`, `ConcurrencyConfig` and `error`. Endpoint maintenance, diagnostics and contract tooling live in `c_two.endpoint`, `c_two.observe` and `c_two.contract`; the Rust SDK uses the same three modules under `c_two::`. Do not add root names, aliases or second spellings for these capabilities; option values such as concurrency modes and input lifetimes are string literals.

Applications start with `import c_two as cc`. Import the specialized modules when needed:

```python
from c_two import endpoint, observe, contract
```

Rust users import the user-facing package as `c_two`, not by a Core or transport crate name:

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

Generated Rust modules may use the hidden `c_two::generated` seam. Ordinary application code must not assemble `c2-ipc`, `c2-http`, `c2-server`, `SyncClient`, or route bindings directly.

Contract export does not select a Runtime. Concurrency modes are `'parallel'`, `'exclusive'`, and `'read_parallel'`; registration input lifetimes are `'materialized'` and `'borrowed'`.

- Authoring: `@cc.crm`, `@cc.read`, `@cc.write`, `@cc.stream`, `@cc.on_shutdown`, `@cc.transfer`, `cc.hold`, `cc.HeldResult`, `cc.Stream`, `cc.StreamCloseReceipt`
- Calls: `cc.with_call_options` (per-call deadline views, see [remote connections and deadlines](#remote-connections-and-deadlines))
- Runtime: `cc.Runtime`, whose instance methods include `set_local_endpoint`, `set_server`, `set_client`, `set_relay_anchor`, `register`, `connect`, `unregister`, `start`, `server_id`, `server_address`, `server_credential`, `local_endpoint_context`, `shutdown`, `connection_events`, `lifecycle_snapshot`, `shutdown_outcome`, `memory_stats` and `call_execution_snapshot`
- Default Runtime: `cc.register`, `cc.connect`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.endpoint.server_credential`, `cc.endpoint.server_id`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.endpoint.set_local_endpoint`; `cc.close(proxy)` follows the proxy's own connection
- Contracts: `cc.contract.export_descriptor`, `cc.contract.release_ref`, `cc.contract.compile_artifacts`, `cc.contract.diagnostics`
- Endpoint maintenance: `cc.endpoint.inspect_endpoint`, `cc.endpoint.reap_endpoint`, `cc.endpoint.sweep_endpoints`, `cc.endpoint.local_endpoint_context`, `cc.endpoint.LocalEndpointContext`, `cc.endpoint.EndpointCredential` (immutable native value; explicit JSON import/export)
- Connection observations: `cc.observe.connection_observation`, `cc.observe.connection_events`, `cc.observe.ConnectionObservation`, `cc.observe.ConnectionClosed`, `cc.observe.ConnectionEvent`, `cc.observe.ConnectionEvents`, `cc.observe.ConnectionEventsLagged`
- Errors: `cc.error.ErrorCode`, `cc.error.CoreError` (structured local/Core errors with read-only `semantic`), `cc.error.CCError` (semantic errors with `side` and `stage`, including `ResourceExecutionError`, `SerializationError` and the route/protocol subclasses), and common `cc.error.CCBaseError` (read-only `transport_phase`)
- Monitoring: `cc.observe.memory_stats`, `cc.observe.call_execution_snapshot`, `cc.observe.lifecycle_snapshot`, `cc.observe.shutdown_outcome` (read-only)

Use `cc.set_server(startup_timeout=...)` before registration to choose a native startup wait. Startup expiry has `CoreError.lifecycle_kind == 'startup_timeout'`, `startup_stage` and `timeout_seconds`; see [startup configuration and errors](configuration.md#server-startup-timeout).

After a listener reaches Ready, `cc.endpoint.server_credential() -> cc.endpoint.EndpointCredential | None` returns its native credential from the existing lifecycle watch without inspection or filesystem I/O. It returns `None` before start and after terminal shutdown; a snapshot is not proof of liveness or cleanup authority. See the [lifecycle guide](local-endpoint-lifecycle.md#exact-cleanup-and-bounded-sweeps).

`set_client` sets the outgoing role's capacity. `set_server` sets the incoming role's capacity and Host options. Each call updates only the fields it supplies. The second call keeps the memory and slot settings from the first, and the third clears the slot override so the environment or derived default applies.

```python
cc.set_client(memory_budget_bytes=128 * 1024**2, max_outstanding_calls=128)
cc.set_client(file_spill_budget_bytes=0)
cc.set_client(max_outstanding_calls=None)

memory = cc.observe.memory_stats()
calls = cc.observe.call_execution_snapshot()
print(memory["client"]["memory"], calls["server"]["used_operations"])
```

Read the snapshots before starting or connecting to see an inactive role: its byte and call counters are `None`, and observation leaves the role inactive. After a role freezes, an update with the same effective values succeeds. An effective change raises `CoreError.lifecycle_kind == 'config_frozen'`. [Role configuration](configuration.md#role-domains-and-sdk-configuration) · [Diagnostic fields](memory-policy.md#statistics-and-payload-lifetime)

The [lifecycle API guide](local-endpoint-lifecycle.md) specifies observation return values, timeout/end/lag behavior and error outcomes. Its [bind/connect section](local-endpoint-lifecycle.md#bind-and-connect-failures) documents the structured OS fields on public `cc.error.CoreError`. These observations do not prove process exit or authorize replay or cleanup.

### CRM contracts, clients and names

CRM contract classes are interfaces with `...` method bodies. `@cc.crm()` requires `namespace` and `version`; contract class names have no `I` prefix. Resource implementations are plain classes named for the domain, such as `NestedGrid` or `PostgresVectorLayer`; `{ContractName}Impl` is acceptable for generated code. Python-only data types exchanged between Python peers use descriptive domain names such as `GridAttribute`.

```python
@cc.crm(namespace='cc.demo', version='0.1.0')
class Grid:
    def some_method(self, arg: int) -> str:
        ...
```

Any code calling `cc.connect(CRMClass, name='...', address='...')` is a client. Its typed proxy matches the CRM contract and supports context-manager close:

```python
with cc.connect(Grid, name='grid', address='ipc://server') as grid:
    result = grid.some_method(1)
```

There is no separate `compo/` module, `@cc.runtime.connect` decorator, or `Component` type. Routing names are user-chosen strings passed to `cc.register(name=...)` and `cc.connect(name=...)`, distinct from the CRM `namespace`. Different CRM contracts, or different instances of one contract, can coexist under distinct names. For remote IPC and relay paths, the native runtime also matches the expected CRM tag and contract hashes derived from the client's CRM class.

### Default Runtime API pattern

Module-level registration, connection, configuration and shutdown delegate to the default Python Runtime. `cc.close(proxy)` follows the Runtime that created the proxy. `cc.Runtime()` creates an application-owned instance with the same register / connect / configure / shutdown methods and an empty-Host `start()`:

```python
import c_two as cc

# Server side
cc.set_relay_anchor('http://relay-host:8080')
cc.set_server(server_id='my_server', memory_budget_bytes=256 * 1024**2)
cc.register(Grid, grid_instance, name='grid')
cc.register(Network, net_instance, name='network')
cc.serve()

# Client side, same process: thread preference
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side, direct IPC
cc.set_client(memory_budget_bytes=128 * 1024**2, max_outstanding_calls=128)
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

`runtime.start()` starts a Host with no registered CRM and waits for native readiness and the existing IPC PING/PONG. A later `runtime.register()` uses that Host. Without an explicit address, `runtime.connect()` first searches this instance's local routes and, if not found locally, resolves the name through this instance's configured relay anchor; use `address=other.server_address()` and the same Unix endpoint context for a direct cross-instance IPC call without a relay. `cc.close(proxy)`, its context manager, `cc.hold()` and `cc.with_call_options()` follow the Runtime that created the proxy. Explicit instances have no `serve()` method or automatic signal/atexit cleanup; `cc.serve()` handles the module-level default Runtime.

Calling `runtime.shutdown(timeout=...)` starts the Core Runtime's single permanent retirement scope for that SDK session; module-level `cc.shutdown()` does the same for the default Runtime. This scope seals the Core client pool and closes admission for new registration, start and remote connection work. A pending observation keeps the old session and its slots; later Core completion alone does not replace them. Call SDK shutdown again to consume the completed receipt and perform the session handoff. A zero or short wait budget limits only this caller's observation and does not cancel the driver. If the deadline expires before retirement is handed off, shutdown raises `CoreError` with `lifecycle_kind='shutdown_wait_timeout'`, `details.stage` and `timeout_seconds`; this is not a pending receipt. `shutdown()` acquires the SDK instance lock first to call native `begin_shutdown(timeout_seconds=...)` (where Python numeric validation and retirement handoff begin), then releases the lock before calling `observe()` so the observation wait, shutdown error projection and shutdown hooks run outside the Python binding lock; after handoff begins, Native Host, Core and pool waits share one absolute deadline. Hooks run after session replacement; if replacement construction fails, the original slots and journal remain available.

Shutdown never fails a unary call already sent: calls in flight on other threads finish and receive their results, and shutdown reports `completed=False` while they run. A server stream that this Runtime serves is the exception: shutdown ends it between its steps, delivers the items already produced, runs its cleanup and ends it with `CallCancelled` naming shutdown, and shutdown completes only after that cleanup. A stream that this Runtime consumes as a client is not ended by its shutdown; close or drop it. A thread waiting without a call deadline on a stopped server keeps waiting, and Python waits for non-daemon threads before it exits; give such calls a deadline with `cc.with_call_options(timeout=...)`.

`cc.serve(shutdown_timeout=30.0)` rejects an invalid budget before serving, blocks until SIGINT or SIGTERM on Unix, SIGINT or SIGBREAK on Windows, or the native Host stops (for example, after a direct IPC shutdown request), then calls `shutdown(timeout=shutdown_timeout)` once and returns that outcome. A pending outcome means cleanup continues in the background; a later `cc.shutdown()` observes its completion. With `blocking=False`, it returns `None` immediately.

The deadline passed to remote `connect()` also bounds the SDK's final publication check. If that original deadline expires before publication, connect raises the corresponding deadline error and releases only its own logical client; it does not retry. An otherwise successful connection continues to reuse its pooled client normally. A `runtime_retired` lifecycle error has no I/O cause. HTTP-only use does not create an IPC pool, and shutdown does not promise that every HTTP keepalive socket has physically closed. The Rust low-level Host shutdown/restart lifecycle remains separate and restartable; this permanent retirement applies to the Python SDK Runtime scope.

The session rotates only with `completed=True`. The handoff atomically transfers both roles’ explicit overrides, including `server_id` and `startup_timeout`, and preserves the real usage reported in `retired` and `holds`. An existing `connection_events()` subscription observes its original session only; subscribe again for the new session. The return value is the closing session's result, and held payloads retain their own lifetime. An application owns any custom endpoint directory it creates; the Runtime does not remove that directory. The [preflight example](../../examples/python/independent_runtime.py) checks the shutdown outcome before deleting its newly created private directory.
