# C-Two FastDB Direct SHM Sink Integration Plan

> **For agentic workers:** This is a C-Two integration plan. Do not implement FastDB binary layout parsing, layer import, or DB build logic in C-Two. Those capabilities belong in the sibling FastDB repository.

**Date:** 2026-05-26
**Status:** Initial implementation landed in the current worktree. Phases 0-5
are implemented for direct IPC FastDB call-db payloads through FastDB's
transport-neutral `prepare_call_db(...)` / `FastdbPreparedCallDb.write_into(...)`
surface. HTTP/relay remains materialized-copy by design. Arena/stub-supported
long-lived shared memory remains deferred.
**Scope:** C-Two-side integration plan for consuming future FastDB planned call-db build APIs so direct IPC request and response payloads can be built into one C-Two SHM allocation instead of building a temporary FastDB backing buffer and then copying it into SHM.

## Context

C-Two FastDB-first CRM methods describe logical payloads such as:

```python
def solve(step: fdb.I32, cells: fdb.Batch[Cell], weights: fdb.Array[fdb.F32]) -> fdb.Batch[Cell]:
    ...
```

For remote IPC, the transport still sends one serialized request payload and one serialized response payload. Today the Python payload binding asks FastDB to serialize the logical values, receives `bytes` or `memoryview`, and passes that completed payload to native C-Two. Native C-Two then copies the completed bytes into request or response SHM when the payload crosses the SHM threshold.

For multi-slot FastDB call-db payloads, this creates two avoidable costs:

1. FastDB builds the final call-db backing buffer in temporary memory.
2. C-Two copies that final backing buffer into IPC SHM.

The target improvement is not per-value SHM allocation. `Batch.allocate(...)` should not normally map each batch to a separate C-Two SHM block for ordinary CRM calls. The target improvement is a single final-payload direct sink: after the FastDB binding knows all arguments or return values, FastDB plans the complete call-db backing size, C-Two allocates one SHM block, and FastDB builds the final call-db DB directly in that block.

## Decision

C-Two should add a prepared payload path that can consume a FastDB-owned write plan:

```python
plan = binding.prepare_write(values)
plan.nbytes
plan.write_into(writable_buffer)
```

C-Two owns the downstream destination and transport lifecycle. FastDB owns the payload layout and bytes written to the destination.

The integration boundary is:

- FastDB receives a generic writable buffer and writes FastDB call-db bytes.
- C-Two allocates the buffer from its request or response transport pool.
- C-Two sends the normal single-payload IPC coordinates after a successful write.
- C-Two releases or rolls back the transport allocation on any planning, writing, route, or send failure.
- HTTP/relay paths continue to materialize ordinary request/response bodies, because relay/network transport is not a shared-memory sink.

The writable destination is call-scoped. It is not a public user object and must not escape the serializer/write-plan call. If the writer needs Python execution, native C-Two must hold the GIL while invoking it, then release the GIL only after the destination has been written and the transport call can proceed without Python callbacks.

The implementation should prefer a one-shot `WritablePayloadSink` or equivalent wrapper over handing out a long-lived raw pointer. The sink should expose a writable buffer only during `plan.write_into(...)`, mark itself closed immediately after the callback, and reject new buffer exports after close. This does not revoke a `memoryview`, NumPy array, or raw pointer deliberately retained by the writer; those are treated as unsafe escapes, matching the FastDB view-lifetime boundary. The narrow guarantee is that C-Two never exposes this sink as a public user API and normal writers cannot reacquire it after the call-scoped write returns.

## Non-Goals

- Do not introduce multi-buffer request or response protocol support for this work.
- Do not make every `Batch.allocate(...)` use C-Two SHM.
- Do not expose C-Two pool objects in FastDB public APIs.
- Do not parse FastDB DB headers, layer headers, field descriptors, string sections, or list sections in C-Two.
- Do not use this plan to design arena/stub-supported long-lived shared-memory RPC. That is a later design.

## Ownership Boundary

### C-Two Owns

- CRM method inspection and payload binding selection.
- `PayloadAbiRef`, route identity, method identity, bridge policy, and error mapping.
- Request pool and response pool allocation.
- IPC payload size limits, SHM threshold decisions, buddy/dedicated/file-spill policy, and wire metadata limits.
- Native response/request buffer release, retained lease tracking, `cc.hold(...)`, and `InputLifetime.BORROWED`.
- Fallback from prepared SHM path to existing serialized path when the prepared path is unavailable or below threshold.

### FastDB Owns

- `prepare_call_db(...)`, `encode_call_db_into(...)`, or equivalent generic build-plan API.
- Final call-db backing byte layout, DB headers, layer headers, slot binding, layer import, direct layer writers, object-graph rules, and schema validation.
- `Batch`, `Array`, `Table`, scalar arrays, owner invalidation, materialization, and unsafe raw export semantics.
- Any exact export, encoded snapshot cache, layer import, or partial paste decision.

## Target Data Flow

### Direct IPC Request

For a client call with large FastDB input payloads:

1. The C-Two client wrapper asks the input `PayloadBinding` for a prepared write plan.
2. If no plan exists, C-Two uses the current `serialize -> client.call(data)` path.
3. If the plan exists, C-Two checks `plan.nbytes` against route `max_payload_size`, SHM threshold, and buddy wire metadata limits.
4. Native C-Two allocates one request SHM block of `plan.nbytes`.
5. C-Two exposes a call-scoped writable view to the plan.
6. FastDB writes the complete call-db backing into that view.
7. C-Two sends the existing preallocated buddy request frame with the allocation coordinates.
8. On send failure or write failure, C-Two frees the allocation and raises the existing C-Two error type.

The server still receives a normal SHM-backed request payload and maps it through FastDB `view_call_db(...)` or `decode_call_db(...)` according to the method input lifetime policy.

### Direct IPC Response

For a resource result with a large FastDB output payload:

1. The server wrapper runs the resource method and obtains the logical result.
2. The output `PayloadBinding` asks FastDB for a prepared write plan.
3. If no plan exists, the wrapper returns the current serialized bytes or buffer.
4. If the plan exists, native C-Two uses the existing response-pool allocation model to allocate one response SHM block.
5. Native C-Two calls the plan writer with the response destination slice.
6. The existing response wire metadata carries the response SHM coordinates.
7. On write failure, native C-Two frees the allocation and returns a resource output serialization error.

Held client responses continue to use `cc.Held[R]`; normal calls continue to materialize or decode and release the transport buffer immediately.

## Required FastDB Prerequisites

C-Two must not implement this integration before FastDB provides a generic, transport-neutral API with these capabilities:

- compute one byte length for the full call-db payload, including all scalar, array, feature, and object-graph slots;
- write the final call-db DB into a caller-provided writable buffer;
- avoid intermediate Python `bytes` on the success path;
- expose deterministic unsupported/fallback results when a value shape cannot be directly built;
- import compatible backed layers without per-row or per-column repack when FastDB can validate the layout;
- keep slot binding distinct from physical layer names;
- reject or materialize object-graph direct builds until root-slot and dependency rules are fully specified.

The sibling tracking document is `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/docs/opt/batch-array-call-db-fast-path-design.md`.

## C-Two Implementation Phases

### Phase 0: Documentation And Boundary Gate

Document this split before implementation. Add scans that catch accidental FastDB binary layout parsing in C-Two transport, native IPC, relay, scheduler, config, error, or CLI layers.

Status: implemented as this plan plus the sibling FastDB plan. Boundary scans
remain part of completion verification.

Exit criteria:

- this plan and the sibling FastDB plan describe the same single-buffer direct sink model;
- docs explicitly reject per-`Batch` transport allocation for ordinary CRM calls;
- docs explicitly defer arena/stub-supported RPC.

### Phase 1: Prepared Payload Binding Surface

Extend C-Two's internal `PayloadBinding` with an optional prepared-write hook. The hook should be internal at first and should not change public CRM authoring.

Expected shape:

```python
@dataclass(frozen=True)
class PayloadWritePlan:
    nbytes: int
    write_into: Callable[[memoryview], None]

@dataclass(frozen=True)
class PayloadBinding:
    prepare_write: Callable[..., PayloadWritePlan | None] | None = None
```

Exit criteria:

- pickle fallback and no-payload bindings do not expose direct SHM write plans;
- FastDB bindings may expose a write plan only when FastDB supports it;
- existing `serialize` and `deserialize` behavior remains the fallback path;
- tests prove a dummy prepared binding writes to a caller-supplied bytearray without changing CRM authoring;
- tests or source guards prove a one-shot writable sink is closed after `write_into(...)` returns and cannot be reacquired as a fresh buffer export.

Status: implemented. `PayloadBinding.prepare_write` is internal, FDB bindings
provide FastDB write plans, and pickle/no-payload bindings keep the serialized
fallback surface.

### Phase 2: Direct IPC Request Prepared Write

Add a native/PyO3 request path that allocates request SHM before serialized bytes exist. The Python wrapper should pass a prepared write plan to native code only for IPC clients, not for thread-local or HTTP clients.

Requirements:

- validate route payload limits before allocation when the method metadata is available;
- allocate exactly one request pool block for the full payload;
- provide a call-scoped writable destination to the plan and prevent it from escaping as a retained user object;
- send the existing `call_prealloc` request after successful write;
- free the allocation on planning, writing, validation, or send failure;
- preserve the current inline/chunked fallback path for small payloads, disabled pools, or unsupported plans.

Exit criteria:

- focused tests prove a prepared input payload is written directly into the request pool and reaches the resource;
- failure tests prove allocation is freed if the writer raises;
- stale-sink tests prove a retained request writable view cannot be used after the write callback returns;
- explicit `ipc://` direct calls still work without relay configuration.

Status: implemented for IPC through native `RustClient.call_prepared(...)` and
request-pool `pool_alloc_and_fill(...)`. Small and non-IPC calls fall back to
materialized request bodies.

### Phase 3: Direct IPC Response Prepared Write

Add a server output path that returns a prepared response plan to native response allocation instead of a finished `bytes` object.

Requirements:

- keep resource execution and bridge application in Python;
- convert FastDB write-plan failures into `ResourceSerializeOutput`;
- use the existing response-pool `try_prepare_shm_response` allocation authority;
- keep the response destination mutable only during the write callback and never expose it as `HeldResult.unsafe_buffer` before the write commits;
- free response allocations on writer failure;
- keep small-payload inline behavior unchanged;
- keep HTTP/relay behavior on materialized body bytes.

Exit criteria:

- focused tests prove a prepared output payload is written into response SHM and can be consumed by normal and held clients;
- held response lease accounting and `fdb.invalidate(...)` still work after release;
- stale-sink tests prove a retained response writable view cannot be used after the write callback returns;
- writer failure does not leave retained native response leases.

Status: implemented for server responses through native response-pool prepared
write handling. Existing held response and borrowed input lifetime mechanisms
remain responsible for FastDB view invalidation after transport buffers are
released.

### Phase 4: Consume FastDB Planned Direct DB Build

After FastDB publishes or is consumed as a sibling dependency with the required API, wire C-Two FastDB payload bindings to FastDB's call-db build plan.

Exit criteria:

- multi-slot inputs such as `(fdb.I32, fdb.Batch[Feature], fdb.Array[fdb.F32])` build into one request SHM payload;
- multi-return outputs build into one response SHM payload;
- C-Two does not inspect FastDB binary layout to decide slot offsets or layer import;
- unsupported FastDB shapes fall back to existing serialize/copy behavior or raise existing C-Two boundary errors.

Status: implemented against the sibling FastDB dependency. C-Two FastDB payload
bindings call FastDB `prepare_call_db(...)`; C-Two only observes `nbytes` /
`byte_length`, `write_into(...)`, and fallback `to_bytes()`.

### Phase 5: Benchmark And Regression Guard

Update the Kostya-style benchmark to report the distinct performance tiers:

- normal FastDB copied path;
- held FastDB copied path;
- prepared direct-SHM sink path;
- prepared direct-SHM sink plus FastDB layer import, when available;
- historical raw structured ndarray transfer, clearly labeled as non-equivalent;
- Ray baseline.

Exit criteria:

- benchmark tables state row count, payload schema, payload size, transport mode, whether layer import was used, and whether the result is held or materialized;
- README claims do not compare historical raw ndarray transfer as if it were semantically equivalent to full FastDB CRM;
- regressions can be attributed to FastDB build planning, C-Two transport copy, client materialization, or safe view overhead.

Status: implemented for the current benchmark surface. The README reports
Kostya-style C-Two FastDB normal/hold tiers, pickle-array fallback, and Ray
arrays/records baseline. Future benchmark expansion should add a dedicated
direct-layer-writer tier once FastDB stops using intermediate DB fallback for
remaining shapes.

### Phase 6: Arena/Stub Follow-Up

Keep arena/stub-supported RPC out of this implementation. It may later provide a stronger model where user code writes directly into a long-lived output arena. That future path needs separate protocol, lifetime, commit/rollback, and safety design.

## Acceptance Criteria

- C-Two can consume a FastDB prepared call-db write plan without exposing C-Two SHM pools in FastDB.
- Large direct IPC request payloads can be built into one request SHM allocation.
- Large direct IPC response payloads can be built into one response SHM allocation.
- Multi-slot FastDB payloads remain one contiguous call-db backing buffer.
- C-Two does not parse or patch FastDB DB/layer binary metadata.
- HTTP/relay paths keep explicit copy/materialized body semantics.
- `cc.hold(...)`, `cc.Held`, native lease tracking, and `InputLifetime.BORROWED` keep their existing lifetime guarantees.
- Prepared writable destinations are call-scoped internals and cannot be retained by resource or client user code.
- Unsupported FastDB direct-build shapes fall back or fail with existing C-Two boundary errors.
- Benchmarks separate direct sink, layer import, materialization, and unsafe held-view effects.

## Verification

Run focused checks after the C-Two prepared payload surface is implemented:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py sdk/python/tests/unit/test_input_lifetime.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=120
```

Run broad checks before claiming implementation completion:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs
uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests examples/python
cargo test --manifest-path core/Cargo.toml --workspace
git diff --check
```

Run boundary scans:

```bash
rg -n "FastVectorDb|layer_header|field_desc|offset_table|offset_strings|offset_wstrings|FASTVectorDB" sdk/python/src/c_two core cli
rg -n "from fastdb4py|import fastdb4py" sdk/python/src/c_two/transport sdk/python/src/c_two/config sdk/python/src/c_two/error.py core cli
rg -n "Batch\\.allocate.*(SHM|shm)|per-.*Batch.*(SHM|shm)|multi-buffer.*FastDB" README.md docs sdk/python/src examples/python --glob '!docs/plans/2026-05-26-fastdb-direct-shm-sink-integration.md'
```

Expected scan results:

- no FastDB binary layout parsing in C-Two transport, Rust core, relay, config, error, lease, scheduler, lifecycle, or CLI layers;
- FastDB imports remain limited to C-Two FastDB contract/ABI/bridge/helper layers;
- active docs do not describe per-`Batch` C-Two SHM allocation as the ordinary CRM path.
