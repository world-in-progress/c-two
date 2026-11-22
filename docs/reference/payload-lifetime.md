# Payload and buffer lifetime

This page defines FastDB CRM binding, retained and borrowed payload lifetimes, and the request and response ownership paths through Core and the Python SDK.

## FastDB CRM binding

Author the nested payload specification with FastDB's `fastdb.payload.v1` schema and bind it explicitly with `@cc.transfer(...)`. The method surface carries one `fastdb4py.payload.Payload` envelope; C-Two must not infer payload structure from annotations or add a second FastDB helper module.

```python
import c_two as cc
from fastdb4py.payload import Payload

CELL_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u8", "nullable": False},
        },
    ],
    "components": [],
}

@cc.crm(namespace='demo.grid', version='0.1.0')
class Grid:
    @cc.transfer(input=CELL_SPEC, output=CELL_SPEC)
    def echo(self, payload: Payload) -> Payload:
        ...
```

FastDB Core is the sole authority for nested parsing, canonical identity, digest, binary layout, build, open, view, materialize, invalidate behavior, and payload-only codegen. C-Two validates only the outer contract and binding relationship. Server-side borrowed input is explicit registration policy through `cc.register(..., input_lifetime={...})`; method metadata must not bypass that gate.

The old Python `CRMProxy.call_prepared`, `CoreClient.call_prepared`, and `NativePreparedCall.call_prepared` write-plan entry points are retired. Client requests retain `begin_call` / `charge_input` / `call` / `close` and the Python-only pickle fallback; do not restore arbitrary client encoding plans as a formal alternative to FastDB. Core opaque-byte interfaces and the existing custom client/scheduler adaptation protocols remain unchanged; the producerless server-side writable sinks and the Rust IPC preallocation API are removed.

## Held results

Over remote transports (IPC and relay HTTP), `cc.hold()` wraps a CRM proxy bound method so that its decoded response stays available as a local copy in a `HeldResult` with `.value`, `.unsafe_buffer`, and `.release()`. Safety layers are explicit release, a context manager, and a `__del__` fallback. On the same-process direct path, `cc.hold()` returns `HeldResult(result)` wrapping the original Python object directly, with no transport copy, FastDB invalidation on release, or `holds` accounting.

`cc.hold()` composes with topic subscriptions the same way: a held subscription yields `Delivery[HeldResult[T]]`, and each delivery's `.value` owns its own retained local copy until released. Closing the subscription never invalidates an already-delivered value; a delivered hold dies only by its own release. On the same-process topic path every delivery decodes from the immutable snapshot the publisher sealed at `publish()` time, so no reader observes another reader's release or the publisher's later mutation.

Hold accounting is Rust-owned: Core `LocalCopy*` records each transport-decoded held local copy, and `HeldResult` is a Python facade over it. The transport memory behind a response, whether inline, SHM, chunked or file reassembly, is released right after the copy into a warm-recycled local buffer (`RecycledBytes`), so a long hold keeps no peer shared memory, connection or mapping; do not reintroduce Python weakref registries for held buffers. For a portable method over remote transports, `held.value` is a FastDB `Payload` owner opened directly over that retained local buffer via `Payload.open_external` / `ExternalBytes` (zero extra decode copy after the ADR 0001 receive copy). Releasing the hold invalidates that owner and its checked views, then returns or frees the local copy; non-held remote calls detach their transport budget charge immediately upon return while keeping the backing alive until the returned `Payload` closes. `held.unsafe_buffer` is a raw `memoryview` escape hatch for advanced users and cannot mechanically invalidate raw NumPy or pointer aliases created from it; normal code should use FastDB checked views and materialize values before storing them beyond the hold scope.

```python
with cc.hold(proxy.method)(args) as held:
    data = held.value

a = cc.hold(proxy.method)(args_a)
b = cc.hold(proxy.method)(args_b)
try:
    process(a.value, b.value)
finally:
    a.release()
    b.release()
```

## FastDB view lifetime

FastDB can enforce stale-use detection only through its public `Payload` owner and checked-view contract. For transport-decoded payloads, C-Two invalidates held responses and borrowed inputs before freeing their local copy. A held response first stops accepting new views and buffer exports under its own lock, then runs the Python invalidator after that lock is released, because an invalidator can give up the GIL and another thread may touch the same response meanwhile; only after the invalidator returns is the local copy freed. A transport-decoded server stream's borrowed input stays valid until its generator or iterator has been closed in the stream's cleanup, and each held stream item keeps its own local copy (same-process direct calls pass original Python objects without transport copies or invalidation). Values that must outlive the hold or the method call must be materialized through the official FastDB API.

FastDB cannot reliably invalidate every raw pointer a user deliberately extracts from a view. In particular, unsafe NumPy access can produce an ndarray or pointer that no longer consults the payload owner on later reads. Treat such APIs as explicit unsafe escapes: tests should assert normal FastDB view invalidation after release, but do not claim that a leaked raw NumPy pointer can be made mechanically impossible in Python.

IPC request and response memory pools are directional rather than one full-duplex shared pool, so leaked or malicious stale access in one direction should not expose the opposite channel. HTTP/relay paths necessarily copy at the relay/network boundary, breaking the shared-memory eavesdropping path. These mitigations do not replace FastDB view invalidation or explicit materialization; C-Two treats stale raw-pointer exposure as a developer-safety/performance footgun rather than a primary cross-channel security model.

## Dispatch and transport ownership

Wire codec and transport code in Rust (`c2-wire`, `c2-ipc`, `c2-mem`) prioritize zero-copy and single-allocation patterns. Python `wire.py` retains only `MethodTable`; wire codecs and handshake objects are not exposed by the Python SDK. The five native contract projection functions remain available under their existing names. The thread-local transport skips serialization entirely. SHM segment names are deterministic for lazy peer-side opening. Each pool's owner allocates and frees under its own local lock; a peer only reads its read-only mapping.

Remote dispatch follows `RequestData -> RecycledBytes (copy, then release) -> CrmCallback::invoke -> EncodedService.invoke_into(RecycledBytes, ResponseSink) -> PyCoreRequestBuffer -> FastDB Payload.open_external`. The server charges the request copy to its domain memory (reusing a bounded warm local buffer pool when eligible). It copies any SHM or reassembled request and queues its release before the resource method runs, so a slow method never holds the client's shared memory. PyO3 moves the same `RecycledBytes` owner into the request buffer without a second copy, and FastDB opens it directly via `ExternalBytes`; `open_external` automatically lifts FastDB's default `OpenOptions.max_total_bytes` to match transport-admitted payloads larger than 256 MiB. Preserve this copy, release and move ordering when changing dispatch or scheduler code. In the client direction, a call's owned input (or direct `WritableSlab` SHM allocation for a FastDB `BuildPlan`) is charged once to the [client domain's memory budget](memory-policy.md#role-domain-budgets-and-call-slots) and released as soon as the transport has written, copied or sent it, before the reply arrives; no copy is kept for a possible re-send. The unused `PyShmBuffer` projection and low-level `c_two._native.ShmBuffer` export are retired without compatibility aliases.

Responses follow `ResponseData -> RecycledBytes (copy, then release) -> HeldResponse -> PyCoreResponse -> FastDB Payload.open_external`: an inline `Vec` moves without a copy, while SHM and reassembled carriers copy once into a warm-recycled local buffer and release their loan before decoding or delivering a hold. Do not add an inline clone or a second snapshot, and do not keep a carrier after its copy.

For portable FastDB outputs (unary responses, server stream items, topic publications, and client requests), Python resources and callers may supply either a prebuilt `Payload` (read via zero-copy `Payload.memoryview()`) or a deferred `fastdb4py.payload.BuildPlan` (accepted in resource return annotations as `BuildPlan` or `Payload | BuildPlan`, and `Iterator[BuildPlan]` for streams). When the plan is direct-build eligible and fits in SHM, C-Two executes it straight into a `WritableSlab` SHM allocation (`BuildPolicy.REQUIRE_DIRECT` + `WritableBufferBacking`) without staging or intermediate `bytes` copies, while still preserving `c2-server` payload-limit checks, inline/SHM/chunked fallback, and ADR 0001 single-owner release. Thread-local same-process calls pass Python objects directly (materializing a returned `BuildPlan` into a `Payload`) instead of routing through Rust serialized dispatch.

See the [memory policy](memory-policy.md) for allocator tiers, budgets, loans and release records.
