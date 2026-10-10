# Python SDK usage

C-Two 0.7.4 uses CRM contracts to expose stateful resources over same-process calls, local IPC and HTTP relay. Python uses FastDB 0.2.1 and the matching C3 0.3.3. For installation and a runnable introduction, see the [quickstart](../README.md#quickstart).

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

## Entry points

### Python SDK

The main user surface, shown throughout the guides. The top-level `cc` namespace groups:

- Authoring: `@cc.crm`, `@cc.read`, `@cc.write`, `@cc.on_shutdown`, `@cc.transfer`, `cc.hold`, `cc.InputLifetime`
- Calls: `cc.with_call_options` (per-call deadline views, see [remote connections and deadlines](#remote-connections-and-deadlines))
- Registry: `cc.register`, `cc.connect`, `cc.close`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.set_transport_policy`, `cc.set_local_endpoint`
- Contracts: `cc.export_contract_descriptor`, `cc.export_contract_release_ref`, `cc.compile_contract_artifacts`, `cc.infer_crm_from_resource`
- Lifecycle and maintenance: `cc.LifecycleConfig`, `cc.owner_control_pair`, `cc.spawn_owned_child`, `cc.adopt_owner_stdin`, `cc.inspect_endpoint`, `cc.reap_endpoint`, `cc.sweep_endpoints`
- Monitoring: `cc.hold_stats`, `cc.memory_stats`, `cc.call_execution_snapshot` (read-only)

Process-level IPC policy, call admission limits and relay settings are documented in the [configuration guide](configuration.en.md).
