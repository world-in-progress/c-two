# C-Two Hold/Borrowed Lifetime Closed-Loop Review

Date: 2026-05-25
Scope: Python SDK `cc.hold()` retained responses, FastDB held views, server-side `InputLifetime.BORROWED`, legacy hold-buffer bypasses, and active docs/test guardrails.

## Frozen Invariants

1. **Client retained-response ownership is explicit and call-site scoped.** `cc.hold(proxy.method)(...)` may retain a transport response lease only for that call, and `HeldResult.release()` must release the lease and invalidate only FastDB views whose owner was created by the retained-response path. This prevents stale view use, leaked transport leases, and accidental invalidation of resource/user-owned FastDB views in thread-local calls.
2. **Normal client calls materialize or decode before release.** Without `cc.hold()`, remote FastDB outputs must not expose buffer-backed views after the response buffer is released. This prevents hidden zero-copy aliases from escaping ordinary calls.
3. **Server borrowed inputs are opt-in and FDB-only.** A resource receives borrowed FastDB input views only when `cc.register(..., input_lifetime={method: cc.InputLifetime.BORROWED})` is set, the payload binding is FastDB with a retained view hook, no `bridge.input` is present, and CRM/resource input signatures match exactly. This prevents old server-side hold semantics, Python pickle fallback borrowing, and mismatched resource APIs.
4. **Borrowed server inputs are call-scoped.** Borrowed input views and their children must become invalid after the resource call returns or raises, while `fdb.materialize(...)` remains the explicit way to retain data. This prevents resource-side stale view retention without disabling high-performance borrowed reads.
5. **Raw buffer escape is visibly unsafe.** `HeldResult.unsafe_buffer` may expose a raw `memoryview`, but the normal API is `held.value`; C-Two can release the lease but cannot invalidate NumPy/raw pointer aliases derived by user code. This prevents docs and type surfaces from overstating safety guarantees.
6. **Legacy/custom-codec hold paths stay locked.** `@cc.transfer(input=..., output=...)`, `@cc.transferable`, and `@cc.transfer(buffer='hold')` must not select portable codecs or server-side borrowed lifetimes. This prevents half-migrated FDB-first code from regressing into the old transferable model.

## Findings

### Critical

None open after the remediation in this change.

### High

None open after the remediation in this change.

Resolved High finding: `HeldResult.release()` used to invalidate any FastDB owner reachable from `.value`, including thread-local/resource-owned views. The current implementation makes invalidation explicit via `_invalidate_cb` and leaves thread-local `HeldResult(result, None)` without an invalidator ([transferable.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/crm/transferable.py:36), [transferable.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/crm/transferable.py:63), [transferable.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/crm/transferable.py:221)). Regression coverage is in [test_held_result.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/unit/test_held_result.py:93) and [test_fastdb_crm_smoke.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/integration/test_fastdb_crm_smoke.py:120).

Resolved High finding: server-side borrowed input could be partially protected only at registration while the internal transfer wrapper still trusted `_c2_input_buffer_mode='borrowed'` when a non-FDB binding exposed `view_from_buffer`. The current code gates both dispatch-table construction and wrapper execution on `PayloadPlanKind.FDB` plus `view_from_buffer` ([native.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/transport/server/native.py:85), [transferable.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/crm/transferable.py:331)). Regression coverage is in [test_input_lifetime.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/unit/test_input_lifetime.py:10) and [test_input_lifetime.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/unit/test_input_lifetime.py:42).

### Medium

None open after the remediation in this change.

Resolved Medium finding: `InputLifetime.BORROWED` signature validation previously compared only zipped positional parameters, so extra resource parameters could pass registration and fail later. It now rejects parameter-count mismatches, non-positional parameters, missing CRM annotations, missing resource annotations, and annotation mismatches ([input_lifetime.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/transport/input_lifetime.py:78), [input_lifetime.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/transport/input_lifetime.py:127)). Regression coverage is in [test_grid_fastdb_bridge_smoke.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py:1581).

Resolved Medium finding: borrowed dispatch called lease accounting before establishing the release closure, leaving a possible accounting-failure path without the same Python-side release fence. The current dispatch creates `memoryview` and `release_fn` first, then does `track_retained(...)` inside the protected block ([native.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/src/c_two/transport/server/native.py:566)).

Resolved Medium finding: HTTP relay held FastDB response tests proved in-scope access but not saved child row/column invalidation after release. The relay test now stores row/column views outside the context and asserts `FdbViewInvalidatedError` after release ([test_grid_fastdb_bridge_smoke.py](/Users/soku/Desktop/codespace/WorldInProgress/c-two/sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py:1635)).

### Low

None open after the remediation in this change.

Resolved Low finding: active docs still described hold as `transferable from_buffer` zero-copy and hid the raw pointer boundary behind `.buffer`. README, roadmap, tests README, and AGENTS now describe FastDB held response views, `PayloadAbiRef`, `InputLifetime.BORROWED`, and `HeldResult.unsafe_buffer` as the unsafe raw escape hatch ([README.md](/Users/soku/Desktop/codespace/WorldInProgress/c-two/README.md:252), [AGENTS.md](/Users/soku/Desktop/codespace/WorldInProgress/c-two/AGENTS.md:87)).

## Source-To-Sink Matrix Gap Summary

| Invariant | Producer | Consumer | Storage/cache | Network/wire | FFI/SDK | Test/support | Legacy/bypass status |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Client held response ownership | `cc.hold()` injects `_c2_buffer='hold'`; remote call path builds `HeldResult` with `release_cb` and explicit FastDB invalidator | User reads `held.value`; `HeldResult.release()` invalidates owned FastDB views and releases response buffer | Native lease tracker records `client_response`; `HeldResult` stores value, raw buffer, release callback | IPC response buffer, HTTP copied response payload, direct IPC SHM/file-spill handles | Python wrapper over native response buffers; FastDB `FdbViewOwner` invalidated through `fastdb4py.invalidate` | Unit test for external owner non-invalidation; integration tests for direct IPC and relay child escape invalidation | Thread-local hold has no invalidator; non-retained/materialized hold has no invalidator; legacy server hold rejected |
| Normal materialized calls | Payload planner selects `deserialize` when no hold is requested | Client receives owned/materialized Python/FastDB values | No retained `HeldResult` lease | IPC/HTTP response buffer released immediately after decode | `FastdbCallPlan.deserialize_values()` materializes via FastDB | FastDB CRM smoke tests cover ordinary calls before held calls | `_c2_buffer` is only injected by `cc.hold()` wrapper |
| Server borrowed input gate | `cc.register(..., input_lifetime=...)` normalizes per-method lifetime | `CRMSlot.build_dispatch_table()` selects `buffer_mode='borrowed'` only for FDB retained-view bindings | Dispatch table stores method/access/buffer mode | IPC request buffer enters Python as `memoryview`; relay path copies before reaching upstream IPC | `crm_to_com` receives `_c2_input_buffer_mode='borrowed'`; wrapper also rejects non-FDB direct bypass | Unit test constructs non-FDB binding with `view_from_buffer`; integration rejects pickle fallback, bridge input, missing annotation, extra parameter | `@cc.transfer(buffer='hold')` and stale `_input_buffer_mode='hold'` are rejected |
| Borrowed input call scope | FastDB call-db `view_from_buffer` creates checked owner | Resource method receives FastDB view; may call `fdb.materialize(...)` to retain | Native lease tracker records `resource_input` and active holds return to zero | IPC request buffer released by `release_fn` after call/error | `_invalidate_fastdb_value(deserialized_args)` runs before release; FastDB child views share owner | Tests assert borrowed array/single feature invalidates after call and materialized copies survive | Internal wrapper rejects non-FDB borrowed bypass and releases `_release_fn` on rejected input |
| Raw buffer escape clarity | `HeldResult.unsafe_buffer` exposes retained `memoryview`; `.buffer` remains an alias for compatibility | Advanced user code may derive NumPy/raw pointer aliases | `HeldResult` only clears its own raw buffer reference and releases memoryview; raw aliases are user responsibility | Raw bytes may be IPC SHM/handle/file-spill or HTTP-owned bytes | No FastDB owner can guard raw pointer aliases | Tests assert raw `memoryview` becomes closed after `HeldResult.release()` | Docs label raw buffer unsafe and recommend `held.value`/`fdb.materialize` |

## Next Remediation Checklist

All items from this review cycle are implemented in the current diff:

1. Add `HeldResult` explicit invalidator and stop default FastDB invalidation for thread-local/materialized values.
2. Pass FastDB invalidator only from retained remote response paths.
3. Add `.unsafe_buffer` and update active docs to describe raw buffer escape semantics.
4. Reject non-FDB borrowed input both at dispatch-table build and internal wrapper execution.
5. Tighten borrowed CRM/resource signature validation.
6. Move borrowed request release closure setup before retained accounting.
7. Add direct IPC, HTTP relay, thread-local, wrapper-bypass, and doc guardrail tests.

## Read Scope And Verification

Read production code: `sdk/python/src/c_two/crm/transferable.py`, `sdk/python/src/c_two/transport/input_lifetime.py`, `sdk/python/src/c_two/transport/server/native.py`, `sdk/python/src/c_two/transport/registry.py`, `sdk/python/src/c_two/transport/client/proxy.py`, and `sdk/python/src/c_two/fastdb/call_db.py`.

Read tests/docs: `sdk/python/tests/unit/test_held_result.py`, `sdk/python/tests/unit/test_input_lifetime.py`, `sdk/python/tests/unit/test_crm_descriptor.py`, `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`, `sdk/python/tests/integration/test_fastdb_crm_smoke.py`, `sdk/python/tests/integration/test_transfer_hold.py`, `README.md`, `AGENTS.md`, `docs/roadmap.md`, `docs/roadmap.zh-CN.md`, and `sdk/python/tests/README.md`.

Verification completed so far:

- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py sdk/python/tests/unit/test_input_lifetime.py sdk/python/tests/unit/test_crm_descriptor.py sdk/python/tests/integration/test_transfer_hold.py -q --timeout=30` -> 34 passed.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=120` -> 23 passed.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=120` -> 115 passed.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30` -> 904 passed.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs` -> 1 passed.
- `uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests/unit/test_input_lifetime.py sdk/python/tests/unit/test_held_result.py sdk/python/tests/integration/test_fastdb_crm_smoke.py sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py` -> passed with no output.
- `git diff --check` -> passed with no output.
- Conflict-marker scan over touched source/test/doc files -> no matches.
- Active-doc legacy scan for obsolete transferable/`buffer='hold'` guidance -> only AGENTS guardrail statements remain.

Unverified: no Rust tests were rerun because this change is Python SDK and documentation only.
