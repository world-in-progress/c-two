# Goal: FastDB Borrowed Memory Semantics

## Objective
Complete the C-Two and FastDB memory ownership redesign for FDB-first CRM payloads. Defer all benchmark work. Focus only on functional semantics, public APIs, implementation, tests, and docs for materialized values, client held responses, and explicit server-side borrowed input.

## Repositories
- C-Two: `/Users/soku/Desktop/codespace/WorldInProgress/c-two`
- FastDB: `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`
- PyPI `fastdb4py` may be stale. Prefer the local FastDB checkout for development and C-Two tests.

## Design Decisions
- FastDB owns the FDB value model: `fdb.Batch[T]`, `fdb.Array[T]`, `fdb.feature`, scalar aliases, owned values, buffer-backed views, `materialize`, and `to_owned`.
- C-Two owns RPC and payload lifetime policy: `cc.hold(...)`, `cc.Held[T]`, transport leases, and `cc.InputLifetime`.
- C-Two must not introduce `cc.Batch` or `cc.Array`, and should not make FDB value types look like C-Two-owned types.
- Normal client and server calls are safe by default: they return or pass materialized owned FDB values.
- High-performance borrowed behavior is explicit only.
- Bridge owns shape adaptation only. It converts already-materialized FDB logical values into the Python/domain shape expected by a resource implementation. It does not own buffer lifetime.

## Target Public API
Client response borrowing:

```python
with cc.hold(proxy.method)(...) as held:
    value = held.value  # type follows the CRM return annotation, e.g. fdb.Batch[Point]
```

Server input lifetime:

```python
cc.register(
    CRMClass,
    resource,
    name="route",
    input_lifetime={
        "method_name": cc.InputLifetime.BORROWED,
    },
)
```

`cc.InputLifetime.MATERIALIZED` is the default. `BORROWED` is a server-side input payload lifetime strategy, not a FastDB value type and not a bridge hook.

## Required Semantics
- Client normal call: materialize the FDB response into an owned FDB value and release the transport response buffer immediately.
- Client `cc.hold`: return `cc.Held[R]`, where `R` is the CRM return annotation such as `fdb.Batch[GridAttribute]`; release invalidates the borrowed value and releases the response buffer.
- Server normal call: materialize FDB input before invoking the resource.
- Server `InputLifetime.BORROWED`: decode a call-scoped borrowed FDB view, call the resource, serialize output, then invalidate the borrowed input and release the request buffer in `finally`.
- If a resource wants to keep FDB input beyond the call, it must explicitly call `fdb.materialize(value)` or `value.to_owned()`.
- `BORROWED` v1 is only valid for FDB payloads, requires CRM/resource input signatures to match, and must fail at register time if combined with `bridge.input` for the same method.
- Output bridge may remain allowed only if output serialization still happens before borrowed input release.
- Thread-local direct calls must continue to pass Python objects directly and must not be forced through serialized FDB dispatch for symmetry.
- Direct IPC must remain relay-independent.

## FastDB Work
- Add or complete a public `fdb.materialize(value)` API.
- Add `to_owned()` on relevant borrowed/view value classes.
- Ensure `fdb.Batch`, `fdb.Array`, feature, scalar, tuple/multiple-return, and call-db view materialization are covered.
- Ensure materialized results are detached from the source buffer lifetime.
- Ensure borrowed/view values fail clearly after release or close.
- Audit child views such as columns, numpy arrays, string columns, and bytes columns. If FastDB cannot make escaped child views safely invalidate with their owner, either fix that owner-bound lifetime or require C-Two to expose the server policy as an unsafe variant until it is safe.

## C-Two Work
- Introduce `cc.InputLifetime` and thread it through `cc.register`, registry validation, native server registration, and the transfer wrapper.
- Publicly expose a typed `cc.Held[T]` lifecycle wrapper. A 0.x clean cut is acceptable; keep a compatibility alias only if it does not preserve confusing old semantics.
- Make `cc.hold` type semantics equivalent to `Callable[..., R] -> Callable[..., cc.Held[R]]`.
- Keep internal call-db views internal. User-facing held values should correspond to the CRM logical return type, not to an implementation-shaped `FastdbCallView`.
- Replace C-Two-owned scattered FDB materialization logic with FastDB-owned `materialize` / `to_owned` where possible.
- Validate `input_lifetime` at register time: unknown methods, non-FDB payloads, signature mismatch, and `bridge.input` conflicts must fail early with clear errors.
- Do not revive provider, neutral-provider, old codec, or legacy `@transferable` compatibility paths.
- Move examples and docs toward `import fastdb4py as fdb` for FDB types. C-Two may keep integration helpers, but FDB value types belong to FastDB.

## Tests
FastDB tests:
- `fdb.materialize` works for owned values and borrowed/view values.
- `to_owned()` results remain accessible after the source view is released.
- Borrowed values fail after release.
- Batch, Array, feature, scalar, tuple/multiple-return, string, bytes, and column access paths are covered where applicable.
- Child view escape behavior is explicitly tested.

C-Two tests:
- Normal FDB client calls return materialized owned values and release response buffers.
- `cc.hold` returns `cc.Held[fdb.Batch[T]]`, `cc.Held[fdb.Array[T]]`, or the correct logical return type; release invalidates access.
- Server default input lifetime is `MATERIALIZED` and does not leave active `resource_input` holds.
- `InputLifetime.BORROWED` passes borrowed FDB input to matching resource signatures and releases after output serialization.
- `InputLifetime.BORROWED` plus `bridge.input` fails.
- `InputLifetime.BORROWED` plus non-FDB payload fails.
- `InputLifetime.BORROWED` plus CRM/resource input signature mismatch fails.
- Resource-side `fdb.materialize(input)` allows data to be retained after the call.
- Cover direct IPC, relay HTTP, and thread-local behavior for the important semantics.

## Examples And Docs
- Update `examples/python/grid` to show normal materialized FDB calls, client `cc.hold`, server `input_lifetime=cc.InputLifetime.BORROWED`, and resource-side `fdb.materialize` for long-term retention.
- Document the three ownership boundaries: FastDB owns values/materialization, C-Two owns RPC/lifetimes, bridge owns shape adaptation.
- Do not spend time on benchmarks in this goal.

## Verification
- First run relevant FastDB Python and TypeScript tests, deriving exact commands from the FastDB repo.
- Then run targeted C-Two tests with the local FastDB dependency.
- Minimum C-Two verification:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=30
```

- If Rust/native code changes:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
uv sync --reinstall-package c-two
```

- Before final completion, try the full Python SDK suite:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
```

## Working Rules
- Read the current FastDB and C-Two code before editing.
- Write a short implementation plan before code changes.
- Implement incrementally and keep each stage testable.
- Do not commit, push, publish, or benchmark unless explicitly requested.
- Final response must include the API shape, changed files summary, verification results, and any remaining risks.
