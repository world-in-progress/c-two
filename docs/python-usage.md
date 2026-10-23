# Python SDK usage

C-Two 0.8.2 uses CRM contracts to expose stateful resources over same-process calls, local IPC and HTTP relay. Portable Python payloads use the optional FastDB 0.2.3 extra and the matching C3 0.4.1. For installation and a runnable introduction, see the [quickstart](../README.md#quickstart).

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

Unix processes sharing an IPC domain use the same configured root (`cc.set_local_endpoint(root=...)` or `C2_IPC_ROOT`). The application provisions custom roots; `0755` is accepted with verified ownership and protection from other-user mutation. The default is `0700`, and sockets are `0600`. Windows uses current-logon SID Named Pipes and rejects Unix root overrides. See [endpoint configuration](configuration.en.md#local-endpoint-directories) and [lifecycle management](local-endpoint-lifecycle.en.md).

## Low-level server bridge

`c_two.transport.Server` is a bridge bound to one native `RuntimeSession`. Applications normally use `cc.register()` and `cc.shutdown()`. Low-level hosts can obtain a bridge from their explicitly configured session:

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
- Registry: `cc.register`, `cc.connect`, `cc.close`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.set_transport_policy`, `cc.set_local_endpoint`
- Contracts: `cc.export_contract_descriptor`, `cc.export_contract_release_ref`, `cc.compile_contract_artifacts`, `cc.infer_crm_from_resource`
- Lifecycle and ownership: `cc.LifecycleConfig`, `cc.owner_control_pair`, `cc.spawn_owned_child`, `cc.adopt_owner_stdin`, `cc.OwnerControl`, `cc.OwnerRecoveryTicket`, `cc.prepare_owner_takeover`, `cc.PreparedOwnerTakeover`, `cc.OwnerStatus`, `cc.OwnerControlError`
- Endpoint maintenance: `cc.inspect_endpoint`, `cc.reap_endpoint`, `cc.sweep_endpoints`
- Connection observations: `cc.connection_observation`, `cc.connection_events`, `cc.ConnectionObservation`, `cc.ConnectionClosed`, `cc.ConnectionEvent`, `cc.ConnectionEvents`, `cc.ConnectionEventsLagged`
- Errors: `cc.error.CoreError` (structured local/Core errors), `cc.error.CCError` (semantic errors), with common `cc.error.CCBaseError`
- Monitoring: `cc.hold_stats`, `cc.memory_stats`, `cc.call_execution_snapshot`, `cc.native_lifecycle_snapshot`, `cc.native_terminal_outcome` (read-only)

Use `cc.set_server(startup_timeout=...)` before registration to choose a native startup wait. Startup expiry has `CoreError.lifecycle_kind == 'startup_timeout'`, `startup_stage` and `timeout_seconds`; see [startup configuration and errors](configuration.en.md#server-startup-deadlines).

Process-level IPC policy, call admission limits and relay settings are documented in the [configuration guide](configuration.en.md).

The [lifecycle API guide](local-endpoint-lifecycle.en.md) specifies observation return values, timeout/end/lag behavior, owner ticket issuance and revocation, the single-commit takeover sequence and error outcomes. Its [bind/connect section](local-endpoint-lifecycle.en.md#bind-and-connect-failures) documents the structured OS fields on public `cc.error.CoreError`. These observations do not prove process exit or authorize replay or cleanup.
