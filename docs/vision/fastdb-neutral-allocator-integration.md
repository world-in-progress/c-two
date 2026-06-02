# FastDB Neutral Allocator Integration Vision

**Date:** 2026-05-28
**Status:** P0 architectural direction
**Weight:** This document is higher priority than existing direct-sink or prepared-payload plans. If a C-Two plan conflicts with this document, update that plan or explicitly mark the conflict.

## Purpose

C-Two is FDB-first for portable CRM payloads, but FastDB must own FastDB memory layout and allocator semantics. C-Two should integrate with FastDB's neutral C++ memory interfaces by providing request and response final backing resources backed by C-Two's transport memory pools.

The target is not a C-Two-specific FastDB package surface or public payload
extension model. The target is:

```text
C-Two call context
    -> FastDB neutral final backing adapter
    -> FastDB direct call-db build
    -> normal C-Two wire metadata and lease lifecycle
```

## Current Boundary

Current C-Two prepared payload support can allocate a request or response SHM block and call a Python plan's `write_into(destination)`. That removes some materialized `bytes` paths, but it does not prove FastDB built the value in C-Two memory during resource execution.

If FastDB has already built a `Batch` through its own backing and C-Two later copies or imports that backing into response SHM, the path is still a final sink optimization. It must not be described as resource-time direct shared-memory construction.

## C-Two Ownership

C-Two owns:

- route identity and CRM contract validation;
- `PayloadAbiRef` derivation from CRM annotations;
- request and response memory pools;
- buddy, dedicated, inline, chunked, and file-spill transport choices;
- wire metadata for allocated payloads;
- commit, rollback, and release of C-Two transport allocations;
- `cc.hold(...)`, retained lease accounting, and borrowed input release timing;
- mapping FastDB failures to C-Two error surfaces.

C-Two does not own:

- FastDB DB headers;
- FastDB layer headers;
- FastDB field descriptors;
- FastDB table names;
- FastDB row/string/list/object-graph section layouts;
- `fdb.require(...)` semantics;
- FastDB object graph planning.

No C-Two transport, scheduler, registry, or native Rust code should parse FastDB binary layout for allocation decisions.

## Required Integration Shape

When FastDB exposes a C++ neutral final backing interface, C-Two should implement a native adapter that allocates from the existing request or response pool.

The adapter must support:

- allocate one contiguous block for the final FastDB call-db payload;
- expose a writable region to FastDB only while the allocation is active;
- commit exactly once after FastDB reports success;
- rollback/free on FastDB error, Python exception, route-limit rejection, send failure, or response preparation failure;
- convert committed request allocations into existing IPC request metadata;
- convert committed response allocations into existing IPC response metadata;
- preserve existing fallback paths for HTTP/relay and unsupported FastDB shapes.

The final-backing adapter is internal glue. It must remain hidden from the
public C-Two namespace and must not become a user-visible payload registry.

C-Two should not initially implement FastDB build scratch memory. Scratch memory covers dynamic builder vectors, string/list packing, object graph analysis, dependency discovery, and reference fixup maps. Those are FastDB construction concerns and should remain FastDB-owned heap/default memory unless a separate future design proves that external scratch control is useful and safe.

The C-Two direct path starts only after FastDB can prove the final byte layout. Known row count alone is insufficient; C-Two needs FastDB to report a complete final backing size and shape before C-Two allocates transport memory.

## Resource-Time Output Context

For response optimization, C-Two eventually needs a call-scoped output allocation context before the resource method constructs returned FastDB values.

The intended flow is:

```text
server dispatch enters resource call
    -> C-Two installs FastDB call allocator context
    -> resource calls fdb.require(...) for a supported planned fixed-columnar shape
    -> FastDB plans final byte layout
    -> FastDB allocates final backing through C-Two memory resource
    -> resource fills Batch/Array views
    -> resource returns logical FastDB values
    -> C-Two validates returned shape
    -> C-Two commits allocation and sends normal response metadata
```

If the resource returns an ordinary pre-existing `Batch`, `Array`, Python object, or object graph value that was not built in the context, C-Two should use the prepared writer or materialized fallback. It must not claim direct allocator behavior.

If the resource shape requires dynamic object analysis, dynamic push, REF/list[REF] fixups, or unknown-size variable payload planning, FastDB may use its own scratch build path and C-Two must treat the result as fallback unless FastDB later provides a complete final backing plan before transport allocation.

## Client-Time Input Context

For request optimization, C-Two can provide a request-pool-backed allocator to FastDB while serializing CRM arguments. This is less ambiguous than output because the proxy already controls serialization before sending.

The intended flow is:

```text
proxy validates CRM input binding
    -> FastDB prepares final call-db byte size and shape
    -> C-Two allocates request pool block
    -> FastDB writes directly into that block
    -> C-Two sends existing request SHM metadata
```

If FastDB reports fallback, C-Two can still use the current `write_into(destination)` path or inline bytes path.

## Fallback Boundary

C-Two must preserve correct fallback behavior for:

- FastDB versions without a C++ neutral allocator;
- HTTP and relay paths, where network/relay copy is expected;
- small payloads below the SHM threshold;
- any payload where row count is known but final byte layout is not known;
- object-engine dynamic push outputs;
- object graphs that require dependency discovery or reference fixup;
- variable-size strings/lists/geometries without a complete FastDB size plan;
- resource returns not created through the call-scoped FastDB context;
- Python fallback CRM payloads using pickle.

Fallback paths should be explicit in metrics and benchmark labels. A benchmark must not call a copied prepared-sink path "direct allocator" or "zero-copy output build".

The primary direct target is planned fixed columnar data from FastDB `ColumnEngine`/columnar profile. ObjectEngine is fallback by default. A future predeclared object-graph plan may be considered later, but C-Two should not drive that design and should not parse FastDB object graph internals.

## Public API Boundary

C-Two should not add:

- public `cc.layout(...)`;
- public `cc.require(...)`;
- public FastDB payload or allocator registration;
- public `return_0` or argument-table naming requirements;
- C-Two-owned FastDB schema/layout specs;
- public CRM signatures that expose C-Two buffer wrappers.

The public user model stays:

```python
@cc.crm(namespace="demo.geometry", version="0.1.0")
class Geometry:
    def vertices(self, n: fdb.I32) -> fdb.Batch[Vertex]:
        ...
```

and resource authors use FastDB-owned APIs:

```python
vertices = fdb.require(fdb.batch(Vertex, rows=n))
vertices.fill(...)
return vertices
```

## Lifetime And Safety

FastDB checked view invalidation remains the safety boundary for FastDB-managed views. C-Two must invalidate held responses and borrowed inputs before releasing the corresponding transport allocation.

C-Two cannot mechanically invalidate every raw NumPy pointer or memoryview a user intentionally extracts. This is an unsafe escape hatch. The C-Two safety requirement is to keep checked FastDB views owner-bound and to make transport pool directionality and relay copies limit cross-channel exposure.

## Tests Required Before Claiming Success

C-Two must not claim direct FastDB allocator integration until tests prove:

- direct IPC response path installs a FastDB allocator context before resource code runs;
- `fdb.require(...)` inside the resource allocates in the C-Two response pool;
- C-Two is not used for FastDB dynamic scratch allocations in the initial design;
- no `to_bytes()` or post-resource layer copy is needed for the supported direct fixed-columnar case;
- unsupported object-engine dynamic push falls back and is labeled as fallback;
- known-row-count but unknown-final-byte-layout payloads fall back and are labeled as fallback;
- request direct allocation uses one request pool block for multi-slot call-db payloads;
- failure before commit frees the transport allocation;
- held FastDB response values remain logical `fdb.Batch[T]` / `fdb.Array[T]` values;
- checked FastDB views fail after `held.release()`;
- explicit `ipc://` direct mode remains relay-independent;
- HTTP/relay paths remain honest copy paths.

## Migration Stance

C-Two is in the 0.x line. Clean cuts are preferred over compatibility shims for incorrect internal APIs. Once FastDB exposes the C++ neutral allocator contract, C-Two should remove or downgrade misleading Python-only allocator surfaces and keep the current prepared sink as a clearly named fallback.

The desired end state is smaller C-Two FastDB glue, not larger glue:

- C-Two supplies memory;
- FastDB builds FastDB payloads;
- C-Two transports committed payloads;
- user code remains typed in terms of CRM, `Batch`, `Array`, and `Feature`.
