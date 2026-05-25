# C-Two FastDB Batch/Array Payload Semantics And Fast Path Plan

> **For agentic workers:** This is a design and sequencing document. Do not implement C-Two-local FastDB storage hacks from this document; first check the sibling FastDB plan and keep the ownership boundary intact.

**Date:** 2026-05-26
**Status:** Proposed
**Scope:** C-Two-side repair plan for FastDB-first CRM payload semantics after the Batch/Array/Table boundary discussion. This document complements `docs/plans/2026-05-25-fastdb-call-db-runtime-boundary-remediation.md` and the sibling FastDB document `docs/opt/batch-array-call-db-fast-path-design.md`.

## Context

C-Two CRM methods currently express portable FastDB payloads with logical annotations such as `fdb.Batch[Coord]`, `fdb.Array[fdb.I32]`, `Coord`, or scalar FastDB aliases. Those annotations are correct for users: a resource method should describe the domain payload it accepts or returns, not a raw byte buffer. The runtime reality is more complex because remote IPC transports bytes, and FastDB values may be materialized objects, table-backed views, retained buffer views, or cached encoded buffers.

The recent Kostya-style benchmark investigation showed that current FastDB call-db performance is dominated by server-side FastDB repacking, not by `cc.hold(...)` or client-side held views. For a 3M-row numeric batch, FastDB call-db bulk encoding still creates a new DB/table and copies columns into it before returning bytes, while the old custom transferable benchmark used a raw structured ndarray whose `arr.tobytes()` path was much cheaper. This means the current C-Two/FastDB boundary is functionally correct but not yet aligned with the intended payload-granularity model.

The core semantic issue is that C-Two signatures say `Batch` or `Array`, but the current fastest implementation path depends on whether the Python resource returns a physical `fdb.Table`. `Table` is a FastDB storage concept; `Batch` and `Array` are the CRM payload concepts C-Two should expose.

## Decision

C-Two should keep CRM method signatures logical and FastDB-first:

```python
@cc.crm(namespace='bench.grid', version='0.1.0')
class Grid:
    def coords(self, count: fdb.I32) -> fdb.Batch[Coord]:
        ...
```

C-Two should not ask users to write CRM signatures that expose `bytes`, `memoryview`, or C-Two-specific buffer wrappers just to get performance. Instead, C-Two should lower logical `Batch`/`Array` annotations into FastDB call-db bindings and let FastDB decide whether a returned value can be exported by exact buffer pass-through, encoded snapshot cache, table-level bulk repack, or row-oriented fallback.

## Ownership Boundary

### C-Two Owns

- CRM method inspection and lowering from annotations to FastDB call-db binding descriptors.
- `PayloadAbiRef` construction and C-Two contract artifacts, including route/CRM namespace/name/version/method context.
- Bridge application when a Python resource implementation intentionally uses a different signature from the FastDB CRM contract.
- Transport lifecycle: IPC/HTTP response buffers, request buffers, `cc.hold(...)`, `cc.Held`, `InputLifetime.BORROWED`, native lease tracking, release ordering, and route-level error mapping.
- Tests and examples that prove FastDB payloads work across thread-local, direct IPC, and relay HTTP paths.

### FastDB Owns

- The runtime meaning of `Batch`, `Array`, `Table`, backed rows, columns, and scalar arrays.
- Generic call-db encode/decode/view/export APIs independent of C-Two route identity.
- Backed view owner checks, invalidation, writeability, generation tracking, materialization, and unsafe raw exports.
- Exact-match buffer export, named table construction, encoded snapshot caches, and encode-into-writer APIs.

### C-Two Must Not Own

- FastDB table binary layout parsing in transport, relay, scheduler, config, or lease code.
- C-Two-local wrappers that become the public runtime model for FastDB row/table/array behavior.
- C-Two-specific optimizations that assume FastDB internal table offsets, row layout, or string-column layout.
- A public CRM authoring model that says "return bytes for speed" when the logical return type is really `Batch[T]` or `Array[T]`.

## Target Semantics

Normal resource authors should think in logical FastDB values:

| CRM annotation | Resource may return or receive | Runtime interpretation |
| --- | --- | --- |
| `fdb.Batch[Coord]` | FastDB batch/table/view or iterable rows | Logical batch of `Coord`; FastDB chooses export or encode path |
| `fdb.Array[fdb.I32]` | FastDB scalar array view, NumPy-like vector, or iterable scalars | Logical scalar vector; FastDB owns array runtime behavior |
| `Coord` | Owned or backed FastDB feature | Single logical feature |
| FastDB scalar alias | Python scalar compatible with FastDB alias | Scalar field in a synthetic scalar payload table |

`fdb.Table[T]` remains an acceptable resource implementation object because it is a natural FastDB storage-backed implementation of `Batch[T]`, but C-Two should not require user signatures to mention `Table[T]` for portable CRM contracts. If a resource returns a `Table[T]`, C-Two should pass it to FastDB's generic exporter/encoder through the logical `Batch[T]` binding.

Held calls should preserve the logical return type. `cc.hold(proxy.coords)()` should return `cc.Held[fdb.Batch[Coord]]` conceptually, and `held.value` should be a FastDB-owned batch/table view or equivalent logical batch view. It should not be a C-Two-owned `FastdbCall*View` public model.

Normal calls may return owned FastDB views over copied bytes when that is the safest efficient default. Data that must outlive a hold or borrowed input scope must still be detached with `fdb.materialize(...)` or `.to_owned()`.

## C-Two Implementation Implications

### PayloadBinding Should Accept Buffer-Like Results

`PayloadBinding.serialize` is currently typed as returning `bytes`, while the native server already accepts Python `bytes` or generic buffer-protocol objects and copies large payloads directly into response SHM. C-Two should update the Python-side type contract and tests so FastDB exporters can return `bytes`, `memoryview`, or other buffer-protocol values without pretending everything is a newly allocated `bytes`.

This is a C-Two API cleanup around the payload binding boundary, not a FastDB storage optimization.

### FastDB Export Before Encode

Once FastDB provides a generic API such as `try_export_call_db(binding, value)`, C-Two's FastDB payload serializer should ask FastDB for a fast export before falling back to `encode_call_db(...)`. The fallback order should be FastDB-owned:

1. Exact call-db-compatible buffer export.
2. Encoded snapshot cache.
3. Table-level bulk repack.
4. Row-oriented encode for convenience and small payloads.

C-Two should not reimplement this order by inspecting FastDB internals. It should call the FastDB generic runtime and map errors into C-Two error classes at the boundary.

### Bridge Policy Remains Explicit

If the FastDB CRM signature and Python resource signature match, C-Two can pass FastDB logical values directly according to normal or borrowed input lifetime policy. If a registered bridge converts FastDB CRM values into Python-native resource values, that bridge is the place where materialization or domain conversion happens. The performance model should be explicit: bridge conversion may trade zero-copy performance for resource implementation convenience.

### Hold And Borrowed Lifetimes Stay Transport-Owned

FastDB can detect stale FastDB-managed views through `FdbViewOwner`, but C-Two still owns when request/response transport buffers are released. `cc.hold(...)` and `InputLifetime.BORROWED` remain C-Two APIs because they are transport-lifetime decisions. C-Two should continue to call `fdb.invalidate(...)` when a held response or borrowed input lease ends.

## Cross-Repository Dependency

C-Two should not attempt the main fast path until FastDB lands the generic prerequisites:

- named table/layout construction for call-db target table names;
- logical batch/array runtime metadata distinct from storage table metadata;
- exact-match call-db buffer export for at least single fixed `Batch[T]`;
- generation-based encoded snapshot invalidation;
- scalar array runtime/view cleanup;
- optional encode-into-writer API after the buffer export path is stable.

The sibling FastDB tracking document is `docs/opt/batch-array-call-db-fast-path-design.md`.

## Phases

### Phase 1: Document And Type Boundary

Update C-Two docs and examples so `Batch`/`Array` are described as logical payload concepts and `Table` is described as an acceptable FastDB-backed implementation value. Do not document raw bytes as the normal high-performance CRM authoring model.

### Phase 2: Payload Binding Buffer Protocol Cleanup

Update `PayloadBinding.serialize` typing and focused tests so serializers may return buffer-protocol objects. Confirm native server response handling still copies such buffers into SHM without materializing an extra Python `bytes` object at the C-Two layer.

### Phase 3: Consume FastDB Export Capability

After FastDB exposes exact-match export, change C-Two FastDB call-db serialization to call the FastDB exporter first and fall back to `encode_call_db(...)`. Keep C-Two-specific planning, `PayloadAbiRef`, bridge policy, and error mapping in C-Two.

### Phase 4: Re-Benchmark And Document Performance Tiers

Re-run the Kostya-style benchmark with at least these variants:

- current FastDB normal and hold paths;
- FastDB exact-export hold path;
- FastDB exact-export normal path;
- old structured numeric-only custom-transferable baseline, clearly labeled as non-equivalent historical baseline;
- Ray arrays and records.

The README should avoid claiming equivalence between historical raw ndarray hold and full FastDB CRM unless the payload schema and wire layout are actually equivalent.

## Acceptance Criteria

- C-Two docs clearly say `Batch` and `Array` are logical FastDB payload types, not raw bytes and not necessarily physical `Table`.
- C-Two public CRM examples keep `fdb.Batch[...]` and `fdb.Array[...]` signatures.
- C-Two does not add FastDB binary-layout parsing to transport, relay, config, scheduler, error, lease, Rust core, or CLI layers.
- FastDB exact export can be consumed through a generic API without C-Two-specific code in FastDB.
- Benchmarks report payload shape and semantic equivalence clearly enough that historical raw ndarray results are not compared as if they were full FastDB CRM results.

## Verification

Run focused C-Two checks after implementation slices:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py sdk/python/tests/unit/test_input_lifetime.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=120
```

Run full C-Two checks before claiming completion:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests examples/python
cargo test --manifest-path core/Cargo.toml --workspace
git diff --check
```

Run boundary scans before completion:

```bash
rg -n "from fastdb4py|import fastdb4py" sdk/python/src/c_two/transport sdk/python/src/c_two/config sdk/python/src/c_two/error.py core cli
rg -n "return bytes|memoryview.*CRM|raw bytes.*Batch|Table\\[.*\\].*CRM" README.md docs examples/python sdk/python/src
```
