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

`cc.hold()` wraps a CRM proxy bound method so that its result stays available as a local copy. It returns `HeldResult` with `.value`, `.unsafe_buffer`, and `.release()`. Safety layers are explicit release, a context manager, and a `__del__` fallback.

Hold accounting is Rust-owned: Core `LocalCopy*` records each held local copy, and `HeldResult` is a Python facade over it. The transport memory behind a response, whether inline, SHM, chunked or file reassembly, is released right after the copy, so a long hold keeps no peer shared memory, connection or mapping; do not reintroduce Python weakref registries for held buffers. For a portable method, `held.value` is a FastDB `Payload` owner. Releasing the hold invalidates that owner and its checked views, then frees the local copy. The proven receive path is copy-backed; hold is a lifetime contract, not proof of direct response-SHM construction or zero-copy decoding. `held.unsafe_buffer` is a raw `memoryview` escape hatch for advanced users and cannot mechanically invalidate raw NumPy or pointer aliases created from it; normal code should use FastDB checked views and materialize values before storing them beyond the hold scope.

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

FastDB can enforce stale-use detection only through its public `Payload` owner and checked-view contract. C-Two must invalidate held responses and borrowed inputs before freeing their local copy. A held response first stops accepting new views and buffer exports under its own lock, then runs the Python invalidator after that lock is released, because an invalidator can give up the GIL and another thread may touch the same response meanwhile; only after the invalidator returns is the local copy freed. A server stream's borrowed input stays valid until its generator or iterator has been closed in the stream's cleanup, and each held stream item keeps its own local copy. Values that must outlive the hold or the method call must be materialized through the official FastDB API.

FastDB cannot reliably invalidate every raw pointer a user deliberately extracts from a view. In particular, unsafe NumPy access can produce an ndarray or pointer that no longer consults the payload owner on later reads. Treat such APIs as explicit unsafe escapes: tests should assert normal FastDB view invalidation after release, but do not claim that a leaked raw NumPy pointer can be made mechanically impossible in Python.

IPC request and response memory pools are directional rather than one full-duplex shared pool, so leaked or malicious stale access in one direction should not expose the opposite channel. HTTP/relay paths necessarily copy at the relay/network boundary, breaking the shared-memory eavesdropping path. These mitigations do not replace FastDB view invalidation or explicit materialization; C-Two treats stale raw-pointer exposure as a developer-safety/performance footgun rather than a primary cross-channel security model.

## Dispatch and transport ownership

Wire codec and transport code in Rust (`c2-wire`, `c2-ipc`, `c2-mem`) prioritize zero-copy and single-allocation patterns. Python `wire.py` retains only `MethodTable`; wire codecs and handshake objects are not exposed by the Python SDK. The five native contract projection functions remain available under their existing names. The thread-local transport skips serialization entirely. SHM segment names are deterministic for lazy peer-side opening. Each pool's owner allocates and frees under its own local lock; a peer only reads its read-only mapping.

Remote dispatch follows `RequestData -> owned Vec<u8> (copy, then release) -> CrmCallback::invoke -> EncodedService.invoke(Vec<u8>) -> PyCoreRequestBuffer -> Python memoryview`. The server copies an SHM or reassembled request and queues its release before the resource method runs, so a slow method never holds the client's shared memory; PyO3 moves the same owned `Vec` into the request buffer without a second copy. Preserve this copy, release and move ordering when changing dispatch or scheduler code. In the client direction, a call's owned input is charged to the [payload buffer budget](memory-policy.md#payload-buffer-budget) and released as soon as the transport has written, copied or sent it, before the reply arrives; no copy is kept for a possible re-send. The unused `PyShmBuffer` projection and low-level `c_two._native.ShmBuffer` export are retired without compatibility aliases.

Responses follow `ResponseData -> owned Vec<u8> (copy, then release) -> HeldResponse -> PyCoreResponse`: an inline `Vec` moves without a copy, while SHM and reassembled carriers are copied once and released before decoding or delivering a hold. Do not add an inline clone or a second snapshot, and do not keep a carrier after its copy.

Python server dispatch may return serialized `bytes` or buffer-protocol data, but must not receive native response pools, compare result length against `shm_threshold`, return SHM coordinate tuples, or call `bytes(memoryview)` for transport. Core receives owned response bytes; `c2-server` owns payload-limit checks, response SHM allocation and inline/SHM/chunked transport selection. Response allocation enforces native IPC payload limits before allocating owned fallback buffers, skips buddy SHM when buddy wire metadata cannot represent the payload, and uses checked chunked fallback rather than truncating frame or chunk metadata. Thread-local same-process calls pass Python objects directly instead of routing them through Rust serialized dispatch for symmetry.

See the [memory policy](memory-policy.md) for allocator tiers, budgets, loans and release records.
