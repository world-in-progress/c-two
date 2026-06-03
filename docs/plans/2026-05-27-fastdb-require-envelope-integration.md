# C-Two FastDB Require Envelope Integration Plan

> **For agentic workers:** This is a C-Two integration plan. Do not implement FastDB binary layout parsing, direct DB section writers, or `fdb.require(...)` semantics in C-Two. Those belong in the sibling FastDB repository. Use this plan together with `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/docs/superpowers/plans/2026-05-27-require-envelope-neutral-allocator.md`.

**Date:** 2026-05-27
**Status:** Proposed follow-up plan. It extends the existing direct SHM prepared payload path after FastDB adds `fdb.require(...)` and a neutral allocator/direct-envelope writer.
**Goal:** Let C-Two consume FastDB's typed `fdb.require(...)` call-envelope direct build path through a neutral allocator so fixed-size columnar FastDB CRM inputs and outputs can be built into one C-Two IPC payload allocation without exposing C-Two pools, call slot names, physical FastDB tables, or raw bytes to resource authors.

## Context

C-Two already has an initial FastDB prepared payload path:

- Python payload planning can call FastDB `prepare_call_db(...)`.
- Native C-Two can allocate request/response memory before serialized bytes exist.
- `WritablePayloadSink` exposes a call-scoped writable buffer to Python write plans.
- Held responses still return logical FastDB values and are invalidated through FastDB when released.

The remaining performance issue is that resource code typically creates a normal FastDB value first, then FastDB copies or imports that value into the final call-db payload. This is correct, but it still leaves a large server-side response write for cached or newly built batches. The next step is not a C-Two-specific FastDB storage optimization. The next step is for FastDB to expose a transport-neutral `fdb.require(...)` envelope and direct build plan, while C-Two supplies an allocator backed by its existing request/response pools.

## Public User Model

C-Two CRM signatures remain logical and FastDB-first:

```python
@cc.crm(namespace="bench.grid", version="0.1.0")
class Grid:
    def coords(self, count: fdb.I32) -> fdb.Batch[Coord]:
        ...
```

Resource code may opt into fixed-size direct envelope allocation through FastDB:

```python
class CoordResource:
    def coords(self, count: fdb.I32) -> fdb.Batch[Coord]:
        n = int(count)
        cells = fdb.require(fdb.batch(Coord, rows=n))
        cells.fill(row_id=ids, x=x, y=y, z=z, name=names)
        return cells
```

Multi-return resource code stays typed:

```python
class SolverResource:
    def solve(self, step: fdb.I32) -> tuple[fdb.Batch[Cell], fdb.Array[fdb.F32]]:
        cells, residual = fdb.require(
            fdb.batch(Cell, rows=n),
            fdb.array(fdb.F32, rows=m),
        )
        cells.fill(...)
        residual.fill(...)
        return cells, residual
```

Rules:

- No `cc.layout(...)` public authoring API in the first version.
- No C-Two public `return_0`, parameter-name, alias, or table-name requirement.
- No `out.batch(0)` builder object as the main user surface.
- The full call-db binding remains positional: each `fdb.require(...)` aggregate maps to its CRM input/output slot, and ordinary scalar slots keep their CRM position.
- Ordinary fixed-size scalar CRM values such as `fdb.I32` can share the same final call-db payload with envelope-backed `Batch` and `Array` values; C-Two should let FastDB decide the byte layout and compatibility.
- C-Two validates the returned value shape through the existing FastDB call-db binding and maps failures to C-Two error types.

## Ownership Boundary

### C-Two Owns

- CRM method inspection, route identity, `PayloadAbiRef`, contract artifacts, bridge policy, and error mapping.
- Request and response pool allocation, SHM threshold, payload limits, buddy/dedicated/file-spill policy, and wire metadata.
- The C-Two implementation of a generic writable allocator/lease that FastDB can call through a transport-neutral protocol.
- `cc.hold(...)`, `HeldResult`, retained lease accounting, borrowed input release, and transport buffer invalidation timing.
- Integration tests and benchmarks that prove the direct envelope path works over direct IPC and falls back over HTTP/relay.

### FastDB Owns

- `fdb.require(...)`, `fdb.batch(...)`, `fdb.array(...)`, typed requirement specs, and envelope owner behavior.
- Generic neutral allocator protocol shape.
- Call-db size planning, final DB layout, direct section writers, layer import, object-graph rejection/fallback, and byte-for-byte encoding parity.
- FastDB view invalidation and materialization semantics.

### C-Two Must Not Own

- FastDB DB header, layer header, field descriptor, row table, string section, or list section parsing.
- A C-Two-specific FastDB API surface.
- Per-`Batch` or per-`Array` transport allocation for ordinary CRM calls.
- Public CRM signatures that return `bytes`, `memoryview`, `DB`, `Table`, or C-Two buffer wrappers for performance.

## Integration Design

### Neutral allocator adapter

C-Two should expose an internal object that satisfies FastDB's neutral allocator protocol. It may be implemented in native Rust/PyO3 or Python, but it must look generic from FastDB's perspective:

```python
allocation = allocator.allocate(nbytes)
view = allocation.buffer
allocation.commit()
allocation.rollback()
```

The concrete C-Two adapter owns:

- the selected request or response pool;
- allocation coordinates;
- rollback on planning/write/send failure;
- conversion to existing C-Two wire metadata after commit;
- release through existing transport lease logic.

FastDB should only see a writable allocation. It must not see C-Two route names, SHM segment names, pool types, or transport handles.

### Output path

For direct IPC response payloads:

1. The resource method executes normally and may return values created by `fdb.require(...)`.
2. The C-Two output `PayloadBinding.prepare_write` asks FastDB to prepare a call-db write plan.
3. If the plan can use a direct-envelope allocator, C-Two passes a response-pool allocator adapter.
4. FastDB computes one payload byte length, C-Two allocates one response block, and FastDB writes the final call-db backing into that block.
5. C-Two commits the allocation and sends normal response metadata.
6. On any failure, C-Two rolls back the allocation and maps the error to `ResourceSerializeOutput`.

The output path must remain compatible with normal returned `Batch.allocate(...)` values, exact exports, layer import, and materialized fallback.

### Input path

For direct IPC request payloads:

1. The client wrapper serializes CRM arguments through the FastDB input binding.
2. If aggregate arguments are `fdb.require(...)` envelope-backed, ordinary scalar slots are fixed-size, and direct build is possible, C-Two passes a request-pool allocator adapter to FastDB.
3. Otherwise the current prepared writer or materialized fallback remains in use.
4. The server receives the existing single FastDB call-db payload and applies normal borrowed/materialized input lifetime policy.

Input-side `fdb.require(...)` is an advanced client optimization, not required for ordinary clients.

### HTTP/relay behavior

HTTP and relay paths remain materialized-copy paths. They may still use `fdb.require(...)` for FastDB-side efficient local construction, but C-Two must not pretend that relay/network transport is SHM zero-copy. The writer plan can fall back to `to_bytes()` or FastDB-owned materialized body construction.

## Implementation Tasks

### Task 1: Pin the integration contract and boundary tests

**Files:**

- Modify: `sdk/python/tests/unit/test_fastdb_abi.py`
- Modify: `sdk/python/tests/unit/test_sdk_boundary.py`
- Modify: `docs/plans/2026-05-26-fastdb-direct-shm-sink-integration.md` to add a supersession note pointing to this follow-up plan for the `fdb.require(...)` direct-envelope design.

- [ ] Add assertions that C-Two source does not expose public `cc.layout`, public `cc.require`, public `return_0` requirement helpers, or C-Two-owned FastDB layout specs.
- [ ] Add source guards that `sdk/python/src/c_two/transport`, `sdk/python/src/c_two/config`, `sdk/python/src/c_two/error.py`, `core`, and `cli` do not import `fastdb4py`.
- [ ] Add documentation text that the public user opt-in is `fdb.require(...)`, not a C-Two API.
- [ ] Add a source guard that no C-Two transport/native Rust code parses FastDB DB or layer field names for routing or allocation decisions.

### Task 2: Add FastDB neutral allocator feature detection

**Files:**

- Modify: `sdk/python/src/c_two/fastdb/call_db.py`
- Modify: `sdk/python/src/c_two/crm/payload_plan.py`
- Test: `sdk/python/tests/unit/test_fastdb_abi.py`

- [ ] Extend the FastDB payload binding glue to detect whether a FastDB write plan supports neutral allocator direct build.
- [ ] Detect support by checking for the exact allocator-build method or capability flag on the FastDB plan object, not by version string alone.
- [ ] Keep the current `plan.nbytes` plus `plan.write_into(writable_buffer)` path as the fallback for FastDB versions without allocator support.
- [ ] Do not require users to install a provider or configure a C-Two FastDB plugin.
- [ ] Tests should monkeypatch a fake FastDB plan with allocator support and verify C-Two chooses the allocator path only for IPC-capable clients/servers.

### Task 3: Implement request allocator adapter

**Files:**

- Modify: `sdk/python/native/src/client_ffi.rs`
- Modify: `sdk/python/native/src/writable_sink.rs`
- Modify: `core/transport/c2-ipc/src/sync_client.rs`
- Test: `sdk/python/tests/integration/test_fastdb_prepared_payload.py`

- [ ] Add or extend native request allocation so C-Two can provide a one-shot neutral writable allocation object to Python/FastDB.
- [ ] The allocation object must expose a writable buffer while active and reject fresh buffer exports after close/commit/rollback.
- [ ] The request path must allocate exactly one IPC payload block for a multi-slot FastDB call-db payload.
- [ ] On writer failure, send failure, route limit failure, or Python exception, the request allocation must be freed.
- [ ] Direct `ipc://` must remain relay-independent.

### Task 4: Implement response allocator adapter

**Files:**

- Modify: `sdk/python/native/src/server_ffi.rs`
- Modify: `sdk/python/native/src/writable_sink.rs`
- Test: `sdk/python/tests/integration/test_fastdb_prepared_payload.py`
- Test: `sdk/python/tests/unit/test_held_result.py`

- [ ] Add or extend native response allocation so FastDB can build the final response call-db backing directly in the response pool.
- [ ] The response allocation should commit only after FastDB write success.
- [ ] On writer failure, return `ResourceSerializeOutput` and release the allocation.
- [ ] Held response lease accounting must continue to be Rust-owned.
- [ ] `cc.hold(...)` must still expose `Held[fdb.Batch[T]]` or `Held[fdb.Array[T]]` logical values, not C-Two call-db internals.
- [ ] `fdb.invalidate(...)` must still run before C-Two releases a retained FastDB response buffer.

### Task 5: Add direct-envelope output integration tests

**Files:**

- Create: `sdk/python/tests/integration/test_fastdb_require_envelope_ipc.py`

- [ ] Add a test CRM whose resource returns:

```python
cells = fdb.require(fdb.batch(Coord, rows=n))
cells.fill(...)
return cells
```

- [ ] Verify direct IPC normal call materializes the expected values.
- [ ] Verify direct IPC held call reads columns from `held.value` and fails after `held.release()`.
- [ ] Add a multi-return test:

```python
cells, residual = fdb.require(fdb.batch(Coord, rows=n), fdb.array(fdb.F32, rows=m))
return cells, residual
```

- [ ] Verify tuple order and types on the client side.
- [ ] Add a wrong-order resource test and assert C-Two reports a serialization/type error instead of producing swapped data.

### Task 6: Add direct-envelope input integration tests

**Files:**

- Modify: `sdk/python/tests/integration/test_fastdb_require_envelope_ipc.py`
- Modify: `sdk/python/tests/unit/test_input_lifetime.py`

- [ ] Add a client-side direct IPC call that passes values created by `fdb.require(...)`.
- [ ] Include a fixed-size scalar argument in the same call so the test covers scalar-plus-envelope payload planning.
- [ ] Verify server-side default input behavior materializes or safely owns values according to existing policy.
- [ ] Verify `cc.register(..., input_lifetime={...: cc.InputLifetime.BORROWED})` still gates borrowed FastDB views.
- [ ] Verify retained borrowed inputs invalidate after the call and cannot be used as normal FastDB views after release.

### Task 7: Preserve bridge and fallback behavior

**Files:**

- Modify: `sdk/python/src/c_two/crm/transferable.py`
- Modify: `sdk/python/src/c_two/fastdb/bridge.py` to keep bridge conversion on its explicit materialized path and prevent `fdb.require(...)` envelopes from being silently selected by bridge-only code.
- Test: `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`
- Test: `sdk/python/tests/integration/test_fastdb_crm_smoke.py`

- [ ] If a resource uses a bridge that materializes FastDB CRM values into Python-native domain objects, keep that behavior explicit and do not silently use direct-envelope allocation.
- [ ] If `fdb.require(...)` is unavailable or an unsupported shape is returned, fall back to the existing FastDB prepared writer or materialized encode path.
- [ ] Do not let Python pickle fallback enter strict portable FastDB contract export.
- [ ] Keep thread-local calls as direct Python object calls without forced serialization symmetry.

### Task 8: Update examples and docs

**Files:**

- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `docs/plans/2026-05-26-fastdb-batch-array-payload-semantics.md`
- Modify: `docs/plans/2026-05-26-fastdb-direct-shm-sink-integration.md`
- Add: `examples/python/fastdb_require_resource.py`

- [ ] Document `fdb.require(...)` as the high-performance, fixed-size, columnar call-envelope opt-in for resource implementations.
- [ ] State that C-Two does not expose `cc.layout(...)` or `cc.require(...)` as the public API.
- [ ] State that public mapping is positional, not by alias/name.
- [ ] State that object graph, nested objects, unknown-size outputs, and variable-size direct writers are deferred or fallback.
- [ ] State that HTTP/relay remains copy-based even when the resource uses `fdb.require(...)`.

### Task 9: Benchmark and compare performance tiers

**Files:**

- Modify: `sdk/python/benchmarks/kostya_ctwo_benchmark.py`
- Modify: `sdk/python/benchmarks/run_kostya_sweep.sh`
- Modify: `README.md` only after real measurements exist.

- [ ] Add variants that isolate:
  - current FastDB hold with normal `Batch.allocate(...)`;
  - FastDB hold with `fdb.require(...)` direct-envelope output;
  - hold call plus view only;
  - hold call plus unsafe sum;
  - local unsafe sum only.
- [ ] Keep Ray arrays and pickle arrays comparison, but label semantic differences clearly.
- [ ] Report p10, p50, p90, mean, payload size, and whether direct-envelope path was used.
- [ ] Do not claim equivalence with old raw ndarray transferable unless schema and wire layout are equivalent.

## Cross-Repository Sequencing

1. FastDB lands spec builders and `fdb.require(...)` runtime values without C-Two integration.
2. FastDB lands direct-envelope planning into bytearray and its own neutral in-memory allocator tests.
3. C-Two consumes the new FastDB version through the sibling local dependency before any PyPI release dependency bump.
4. C-Two implements allocator adapters over existing request and response pools.
5. C-Two adds direct IPC integration tests for normal, held, input, output, multi-return, and wrong-order cases.
6. C-Two refreshes benchmarks and docs.
7. FastDB and C-Two run their own full verification gates before release/PR.

## Verification

Focused C-Two checks:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py sdk/python/tests/unit/test_input_lifetime.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_require_envelope_ipc.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_prepared_payload.py -q --timeout=120
```

Full C-Two checks before completion:

```bash
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests examples/python
C2_RELAY_ANCHOR_ADDRESS= cargo test --manifest-path core/Cargo.toml --workspace
git diff --check
```

Boundary scans:

```bash
rg -n "from fastdb4py|import fastdb4py" sdk/python/src/c_two/transport sdk/python/src/c_two/config sdk/python/src/c_two/error.py core cli
rg -n "cc\\.layout|cc\\.require|return_0.*require|alias\\(|with_call_slot|bind_call_slot" README.md README.zh-CN.md docs examples/python sdk/python/src sdk/python/tests
rg -n "FastVectorDb|layer_header|field_desc|offset_table|offset_strings|offset_wstrings|FASTVectorDB" sdk/python/src/c_two core cli
```

Expected:

- FastDB imports stay confined to C-Two FastDB integration/planning layers, not transport/config/error/Rust core/CLI.
- No current guidance presents public `cc.layout`, public `cc.require`, aliases, or call-slot names as the direct-envelope API.
- No C-Two runtime layer parses FastDB binary layout.

## Review Checklist

- [ ] C-Two remains FastDB-first at the CRM contract layer but does not move FastDB storage behavior into C-Two.
- [ ] The public high-performance authoring API is `fdb.require(...)`, not a C-Two API.
- [ ] The integration uses one contiguous call-db payload allocation per request or response.
- [ ] HTTP/relay copy boundaries are documented honestly.
- [ ] Normal calls, held calls, borrowed inputs, bridge conversions, and pickle fallback retain their existing semantics.
- [ ] Unsupported FastDB shapes fail or fall back with clear errors and no leaked allocations.
- [ ] Benchmarks distinguish transport-only, view-only, safe consume, and unsafe consume costs.
