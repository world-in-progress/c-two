# C-Two FastDB Call-DB Runtime Boundary Remediation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans or subagent-driven-development to implement this plan task by task. This document is a remediation plan, not an implementation patch.

**Date:** 2026-05-25
**Status:** Implemented locally; C-Two lockfile update is blocked until `fastdb4py==0.1.19` is published
**Scope:** C-Two-side remediation after FastDB 0.1.18 lifetime integration, focused on removing C-Two-owned FastDB value runtime wrappers and consuming FastDB-owned generic call-db runtime APIs instead. The implementation target is `fastdb4py>=0.1.19`, because `0.1.19` adds the Python generic call-db runtime prerequisite.

## Implementation Status

Implemented locally on 2026-05-25 across the sibling FastDB and C-Two checkouts. FastDB now provides the generic Python call-db runtime prerequisite (`encode_call_db`, `decode_call_db`, `view_call_db`, binding/table/scalar/array/view descriptors), including owner-bound retained views for columnar payloads and deterministic retained-view rejection for object-graph payloads. C-Two `FastdbCallPlan` now builds a generic FastDB binding and delegates encode/decode/view runtime behavior to FastDB; the C-Two-owned Python `FastdbCallView`, `FastdbCallTableView`, `FastdbCallColumnView`, and `FastdbCallArrayView` wrappers have been removed from `sdk/python/src/c_two/fastdb/call_db.py`.

Local verification passed with the sibling FastDB checkout on `PYTHONPATH` and C-Two `uv run --no-sync`, because PyPI does not yet have `fastdb4py>=0.1.19`. Evidence: FastDB `uv run pytest tests/python -q` passed with 347 tests; FastDB `npm --prefix ts/fastdb4ts run test:ts` passed with 76 tests; C-Two `sdk/python/tests/integration/test_fastdb_crm_smoke.py` passed with 115 tests; C-Two `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py` passed with 23 tests; C-Two full Python suite passed with 904 tests; `cargo test --manifest-path core/Cargo.toml --workspace`, compileall, `git diff --check`, and boundary import/wrapper scans passed. `uv lock` in C-Two still fails until the FastDB 0.1.19 package is published, so the lockfile must be refreshed after that release.

## Goal

Move C-Two's FastDB-first CRM payload implementation to the corrected boundary: C-Two owns CRM method planning, route/contract identity, bridge application, transport leases, `cc.hold(...)`, and `InputLifetime.BORROWED`; FastDB owns generic call-db encode/decode/view runtime, backed value semantics, owner invalidation, materialization, array/table/feature/column views, and direct read/write behavior.

## Why This Exists

The 2026-05-24 lifetime integration closed the immediate stale-view gap by making C-Two create FastDB checked owners and call `fdb.invalidate(...)` when held responses or borrowed inputs end. That was necessary but not sufficient. Before this remediation, C-Two still contained `FastdbCallView`, `FastdbCallTableView`, `FastdbCallColumnView`, and `FastdbCallArrayView` in `sdk/python/src/c_two/fastdb/call_db.py`. Those wrappers delegated enough to FastDB to be mostly functionally safe, but they kept C-Two responsible for FastDB runtime value behavior that should be generic FastDB behavior.

FastDB 0.1.18 already changed the Python value model. `FdbViewOwner` owns liveness and writeability, `fdb.invalidate(...)` invalidates owner-bound views, `fdb.materialize(...)` detaches backed values, `Table.__getitem__` returns mapped feature views instead of copies for new `@feature` classes, and mapped feature field writes dispatch back to the backing table when the owner is writeable. Therefore C-Two should not keep row, table, column, or array guard wrappers as its long-term public or internal value model.

The remaining architectural split is that FastDB TypeScript already has generic call-db runtime entrypoints such as `encodeFastdbCallDb(...)`, `decodeFastdbCallDb(...)`, `viewFastdbCallDb(...)`, and `FastdbCallDbView`, while FastDB Python does not yet expose an equivalent generic runtime surface. C-Two Python currently fills that gap in `call_db.py`. This plan exists to close that asymmetry without moving C-Two route, relay, IPC, or bridge policy into FastDB.

## Relationship To Existing Documents

This document extends `docs/plans/2026-05-24-c-two-fastdb-0-1-18-lifetime-integration.md`. That plan closed the owner/invalidation lifetime loop using FastDB 0.1.18 and should remain the authority for current held/borrowed safety behavior until this remediation is implemented.

This document narrows and corrects the runtime-value portion of `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md` and `docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md`. Those documents remain useful for product-level FastDB-first contract direction, but any wording that implies C-Two should permanently own generic FastDB call-db value/view runtime is superseded by this plan.

This document also supersedes active implementation guidance in older FastDB-first plans that says C-Two production code should have no FastDB imports. The corrected rule is narrower: C-Two contract and ABI layers may depend on FastDB because FastDB is the portable CRM payload ABI; C-Two route, relay, IPC, scheduler, lease, config, error, and lifecycle layers must not parse FastDB storage internals.

## Corrected Ownership Boundary

### C-Two Owns

- CRM method inspection and lowering from C-Two CRM annotations into a FastDB call-db binding shape.
- CRM context fields such as namespace, name, version, method name, direction, route name, ABI hash, signature hash, and descriptor artifacts.
- `PayloadAbiRef` construction and portable contract policy: portable method payloads are FastDB call-db refs or no-payload refs; Python pickle remains Python-only fallback.
- Bridge derivation and bridge application when the FastDB CRM signature differs from the Python resource implementation signature.
- Transport memory ownership and lifecycle: native response/request release, lease tracking, `cc.hold(...)`, `HeldResult`, and `InputLifetime.BORROWED`.
- C-Two TypeScript client transport generation and FastDB-backed typed facade orchestration for C-Two contracts.
- End-to-end C-Two tests across thread-local, direct IPC, relay HTTP, held calls, and borrowed inputs.

### FastDB Owns

- Generic `fastdb.call-db.schema.v1` runtime validation independent of C-Two route identity.
- Python and TypeScript call-db encode/decode/view APIs from a generic binding plus payload bytes.
- `Table`, mapped feature, scalar array, column, string, bytes, numeric, object-graph, and materialized value behavior.
- `FdbViewOwner`, checked owner invalidation, writeability checks, generation checks, `fdb.invalidate(...)`, and `fdb.materialize(...)`.
- Direct read and direct write behavior for backed features and columns.
- Unsafe raw array/export APIs and their documented limits.
- FastDB generic tests proving storage/runtime behavior without C-Two transports.

### Forbidden After This Remediation

- C-Two must not expose or depend on `FastdbCallTableView`, `FastdbCallColumnView`, or `FastdbCallArrayView` as the runtime representation of `fdb.Batch[...]`, `fdb.Array[...]`, or feature returns.
- C-Two must not implement FastDB row, column, feature, or array stale-view checks outside the FastDB owner API.
- C-Two runtime route/relay/IPC/scheduler/lease layers must not parse FastDB storage internals or table layouts.
- FastDB must not import C-Two, know C-Two route identity, or own C-Two bridge application across transports.
- FastDB generic Python call-db runtime must not require C-Two CRM metadata to encode, decode, or view a payload.

## Pre-Implementation Evidence From This Worktree

- Before implementation, `sdk/python/src/c_two/fastdb/call_db.py` defined C-Two-owned call-db planning and runtime value wrappers. `FastdbCallPlan.view_from_buffer(...)` constructed `FastdbCallView` and returned `view.logical_values()`, while `FastdbCallView` then exposed `table(...)`, `array(...)`, `feature(...)`, `scalar(...)`, direct list-like access, and `FastdbCallTableView`/`FastdbCallArrayView` child wrappers.
- `FastdbCallTableView._table()` already called `engine.table(feature_type, name=..., owner=..., writeable=False)`, so child rows and columns got FastDB owner binding. This meant the wrapper was no longer the owner of stale-view safety; it was mostly a C-Two envelope and adapter around FastDB.
- `FastdbCallArrayView.__getitem__` and `__iter__` still materialized scalar items one by one through C-Two helper logic. That was the clearest remaining place where C-Two owned FastDB array runtime behavior.
- `sdk/python/src/c_two/crm/transferable.py` currently owns the transport lifetime adapter. `HeldResult.release()` invokes an explicit FastDB invalidator callback for retained remote responses, then releases the memoryview/native response. Server borrowed input cleanup invalidates deserialized FastDB values before calling the request release function. This ownership should remain in C-Two.
- FastDB Python currently exposes `FdbViewOwner`, `fdb.invalidate(...)`, and `fdb.materialize(...)`. Its `Table.__getitem__` binds rows through `bind_feature(... owner=..., writeable=...)`, and mapped feature read/write goes through `FeatureBacking.read_field(...)` and `FeatureBacking.write_field(...)`.
- FastDB TypeScript currently has generic call-db runtime entrypoints and view classes in `ts/fastdb4ts/src/call-db.ts`, proving the intended generic runtime shape already exists on one language side.

## Target Public Semantics

Normal remote calls keep materialized semantics. If the caller does not use `cc.hold(...)`, C-Two decodes or materializes the FastDB payload and releases the transport buffer before returning.

Held remote calls return `cc.Held[R]`, where `R` is the logical CRM return annotation. For `fdb.Batch[Point]`, `held.value` should be a FastDB table or FastDB generic batch view, not a C-Two `FastdbCallTableView`. For a single `Point`, `held.value` should be a FastDB mapped feature view. For `fdb.Array[fdb.I32]`, `held.value` should be a FastDB generic scalar array view once FastDB provides one. For tuple returns, `held.value` should be a tuple of those logical FastDB values.

Server borrowed inputs remain explicit. `cc.register(..., input_lifetime={"method": cc.InputLifetime.BORROWED})` passes FastDB owner-bound views to the resource only when CRM and resource signatures match and no bridge input hook is configured. After the call and output serialization complete, C-Two invalidates the FastDB value graph and releases the request buffer.

The explicit retention escape remains `fdb.materialize(...)` or `.to_owned()`. The explicit unsafe raw escape remains `HeldResult.unsafe_buffer` and FastDB unsafe raw export APIs. C-Two must not promise revocation of raw NumPy or raw pointer aliases after user code has taken them.

## Required FastDB Prerequisite

Before C-Two can remove its Python call-db runtime wrappers, FastDB Python needs a generic call-db runtime surface equivalent in role to the current fastdb4ts runtime:

```python
import fastdb4py as fdb

payload = fdb.encode_call_db(binding, values)
owned = fdb.decode_call_db(binding, payload)
view = fdb.view_call_db(binding, payload, owner=owner)
```

The exact names can differ, but the capabilities must be present:

- accept a generic binding descriptor with codec id, profile, schema hash, method, direction, table descriptors, scalar field positions, array item metadata, feature classes, and dependency schema hashes;
- encode columnar and object-graph call-db payloads without C-Two CRM route knowledge;
- decode to materialized Python values for normal calls;
- create owner-bound retained views for columnar payloads;
- expose a generic scalar array view for `Array[Scalar]` so C-Two no longer needs `FastdbCallArrayView`;
- expose feature batch views as FastDB table/batch views with column access, row access, iteration, materialization, and owner invalidation;
- expose single feature views as mapped FastDB feature views;
- accept a caller-supplied `FdbViewOwner` with optional release hook;
- fail deterministically for object-graph retained views until FastDB can represent them safely, while still supporting materialized object-graph decode;
- keep generic schema/runtime errors in FastDB exception types or generic `ValueError`/`TypeError` messages, without importing C-Two errors.

This prerequisite belongs in the FastDB repository because it is generic value/runtime behavior and should serve Python, TypeScript parity, and any future non-C-Two consumer.

## C-Two Remediation Phases

### Phase 0: Freeze Current Working Lifetime Model

Do not remove the existing C-Two wrappers before FastDB Python has a replacement generic runtime. Keep the 2026-05-24 lifetime closure tests green. Treat current `FastdbCallView` wrappers as temporary compatibility internals, not as a public model to polish.

Exit criteria:

- Current hold and borrowed tests still pass.
- Docs no longer recommend adding performance features to C-Two wrappers.
- A future FastDB prerequisite issue or plan exists and is referenced by this C-Two plan.

### Phase 1: Add FastDB Python Generic Call-DB Runtime Prerequisite

Implement or consume FastDB Python runtime APIs outside C-Two. C-Two should not start this phase by moving `FastdbCallView` code verbatim into another C-Two module. If some code is copied as a bootstrap into FastDB, remove CRM context, route identity, C-Two error mapping, bridge derivation, and C-Two-specific labels from that generic runtime.

FastDB-side acceptance criteria that C-Two depends on:

- FastDB tests prove `Table.__getitem__` retained views are direct mapped views, not copied rows.
- FastDB tests prove scalar array views are owner-bound and can materialize to owned Python lists or arrays.
- FastDB tests prove `view_call_db(..., owner=owner)` child row, column, single feature, string column, bytes column, numeric column, and scalar array views raise after `fdb.invalidate(owner)`.
- FastDB tests prove `fdb.materialize(view)` survives owner invalidation.
- FastDB tests prove unsafe raw exports are explicit and documented as non-revocable.
- FastDB TypeScript and Python call-db schema/binding fields remain semantically aligned.

### Phase 2: Add C-Two Adapter To FastDB Generic Runtime

Change C-Two `FastdbCallPlan` from a runtime value owner into a C-Two CRM-to-FastDB binding producer. C-Two may keep C-Two-specific planning classes if they are needed to derive method payload ABI refs, but retained view construction should call FastDB generic runtime.

Expected shape:

```python
owner = fdb.FdbViewOwner(checked=True, writeable=False)
value = fdb.view_call_db(binding, memoryview_response, owner=owner).logical_value()
```

The binding may still be assembled by C-Two from CRM annotations and route context. The returned value must be a FastDB-owned value/view graph, not a C-Two `FastdbCall*View`.

Implementation notes:

- Keep `payload_abi_ref` and sidecar artifact generation in C-Two because those are contract artifacts.
- Keep C-Two error mapping around the call to FastDB runtime, translating FastDB runtime failures into `ClientOutputFromBuffer`, `ClientDeserializeOutput`, `ResourceInputFromBuffer`, or `ResourceDeserializeInput` at the boundary.
- Keep C-Two lease tracking and native response/request release callbacks unchanged unless FastDB owner release hooks make the code simpler.
- Prefer `FdbViewOwner(release=release_cb)` once it removes duplicated invalidator and release callback ordering from `HeldResult`, but do not move native lease authority into FastDB.

Exit criteria:

- `cc.hold(...).value` for `Batch[Feature]` is a FastDB table/batch view.
- `cc.hold(...).value` for `Array[Scalar]` is a FastDB scalar array view.
- `cc.hold(...).value` for single `Feature` is a FastDB mapped feature view.
- C-Two tests do not assert C-Two `FastdbCall*View` type names.
- Release after child escape raises FastDB invalidation errors through FastDB views.

### Phase 3: Remove C-Two-Owned FastDB Runtime Wrappers

After Phase 2 tests are green, delete or shrink these C-Two runtime classes:

- `FastdbCallView`
- `FastdbCallTableView`
- `FastdbCallColumnView`
- `FastdbCallArrayView`

Any surviving class in `sdk/python/src/c_two/fastdb/call_db.py` should be a C-Two planning, descriptor, diagnostics, or binding adapter type, not a value runtime type.

Exit criteria:

- `rg -n "FastdbCall(Table|Column|Array)?View" sdk/python/src/c_two sdk/python/tests examples/python` returns no production or user-facing dependency except historical docs or explicit migration notes.
- `sdk/python/src/c_two/fastdb/call_db.py` no longer constructs FastDB tables only to wrap them in C-Two list-like classes.
- C-Two normal calls still use materialized decode and immediate release.
- C-Two held calls still retain native buffers until `HeldResult.release()` or context manager exit.

### Phase 4: Update Tests, Docs, And Type Surfaces

Rewrite tests and examples so they assert behavior and FastDB-owned value types, not C-Two wrapper names. Update public docs to explain that `fdb.Batch`, `fdb.Array`, `@fdb.feature`, `fdb.materialize(...)`, and `fdb.invalidate(...)` are FastDB-owned, while `cc.hold(...)`, `cc.Held`, bridge registration, transport leases, route identity, and `InputLifetime` are C-Two-owned.

Test migration requirements:

- Replace type-name assertions such as `type(values).__name__ == "FastdbCallArrayView"` with FastDB public API assertions.
- Keep direct IPC and relay held-response tests for row, column, scalar array, single feature, WSTR, BYTES, and tuple returns.
- Keep borrowed server input tests proving escaped values fail after call return and materialized copies survive.
- Keep thread-local tests proving `cc.hold(...)` does not invalidate resource-owned FastDB values.
- Keep normal-call tests proving no retained buffer-backed view escapes without `cc.hold(...)`.
- Keep Python pickle fallback tests proving fallback is skipped or warned during portable export and not treated as FastDB.

Documentation requirements:

- Mark older docs that claim C-Two production should have no FastDB imports as superseded. The correct rule is that C-Two contract/ABI layers may depend on FastDB, while C-Two runtime transport/route/lease layers must not parse FastDB internals.
- Mark older docs that describe C-Two wrapper views as the user-facing held API as superseded by FastDB-owned views.
- Keep the 0.x clean-cut rule explicit: remove zombie provider and wrapper docs rather than carrying compatibility language after the migration lands.

## Verification Matrix

Run focused checks after each implementation slice:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py sdk/python/tests/unit/test_input_lifetime.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=120
```

Run broad C-Two verification before claiming completion:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs
uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests examples/python
cargo test --manifest-path core/Cargo.toml --workspace
git diff --check
```

Run FastDB prerequisite verification in the FastDB repository before removing C-Two wrappers:

```bash
uv run pytest tests/python/test_view_owner_lifetime.py tests/python/test_materialize.py -q
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
npm --prefix ts/fastdb4ts run test:ts
git diff --check
```

Run boundary scans before completion:

```bash
rg -n "FastdbCall(Table|Column|Array)?View" sdk/python/src/c_two sdk/python/tests examples/python
rg -n "C-Two production.*no FastDB imports|fastdb-neutral|provider-neutral|install_c_two_provider|@cc\\.transfer\\(buffer=['\"]hold" README.md docs sdk/python/src sdk/python/tests examples/python
rg -n "from fastdb4py|import fastdb4py" sdk/python/src/c_two/transport sdk/python/src/c_two/config sdk/python/src/c_two/error.py core cli
```

Expected scan results:

- No remaining C-Two production or example dependency on C-Two `FastdbCall*View` classes after Phase 3.
- Provider-neutral or FastDB-neutral wording may remain only in superseded historical context or explicit migration notes that say the wording is obsolete. Any active README, example, test, or current plan guidance that presents provider-neutral codecs as a peer portable path must be changed.
- No FastDB imports in C-Two transport/config/error/Rust/CLI route, relay, IPC, scheduler, lease, or lifecycle layers.

## Risks And Mitigations

| Risk | Severity | Mitigation |
| --- | --- | --- |
| Removing C-Two wrappers before FastDB Python has generic runtime parity | High | Keep wrappers as temporary internals through Phase 0 and Phase 1; require FastDB prerequisite tests before Phase 3 deletion. |
| Accidentally moving C-Two CRM route identity into FastDB | High | FastDB runtime accepts generic binding descriptors only; CRM namespace, route, ABI hash, signature hash, and C-Two diagnostics stay in C-Two. |
| Leaving `Array[Scalar]` as a C-Two-only runtime view | High | Make FastDB scalar array view an explicit prerequisite and block wrapper deletion until C-Two can consume it. |
| Breaking current lifetime safety while refactoring | High | Keep existing held and borrowed stale-view tests green; add type-surface migration tests before deletion. |
| Optimizing C-Two wrappers instead of removing them | Medium | Treat wrapper caching and wrapper API polish as out of scope unless needed to keep the temporary bridge stable before FastDB prerequisite lands. |
| Overstating raw pointer safety | Medium | Keep `unsafe_buffer` and FastDB unsafe raw exports documented as explicit non-revocable escapes. |
| Confusing old boundary docs with current architecture | Medium | Mark old docs superseded and add boundary scans to the completion gate. |

## Non-Goals

- Do not implement neutral authoring schema or schema-to-CRM codegen in this remediation.
- Do not work on TypeScript item 91 parity gaps unless required to keep current tests green.
- Do not create a separate `fastdb-c-two` integration package.
- Do not restore provider-neutral public codec flows or legacy `@cc.transferable` portability.
- Do not make FastDB a hostile multi-tenant sandbox.
- Do not promise revocation of already-exported raw NumPy arrays or raw pointer aliases.

## Review Log

### Iteration 1 Checklist

- Correctness: The document must not claim C-Two can delete wrappers before FastDB Python provides generic call-db runtime parity.
- Architecture: The document must keep C-Two CRM route identity out of FastDB and FastDB storage runtime out of C-Two transport layers.
- Performance: The document must reject spending effort on wrapper caching as a final solution.
- Test coverage: The document must include child escape, borrowed input, materialization, scalar array, WSTR/BYTES, normal-call, direct IPC, relay, and thread-local guardrails.
- Migration hygiene: The document must include scans for old provider-neutral/FastDB-neutral guidance and C-Two wrapper type dependencies.

### Iteration 1 Result

Findings found and remediated in the document:

- P1: The status claimed strict self-review was complete while this review log still said pending. This could mislead the next executor into treating an unreviewed draft as accepted. Fixed by changing status to "Under strict self-review" until the final review passes.
- P1: The boundary scan expected no provider-neutral or FastDB-neutral wording anywhere under `docs`, but this repository intentionally retains historical plans and this plan itself discusses obsolete language. Fixed by changing the completion expectation to allow only superseded historical context or explicit migration notes and to require active guidance cleanup.

No implementation code was reviewed or changed in this iteration.

### Iteration 2 Checklist

- Re-run document hygiene checks.
- Verify the plan has no pending placeholders.
- Verify the plan preserves the FastDB prerequisite before C-Two wrapper deletion.
- Verify the verification matrix has realistic completion expectations.

### Iteration 2 Result

Passed after review.

Evidence:

- `git diff --check -- docs/plans/2026-05-25-fastdb-call-db-runtime-boundary-remediation.md` passed.
- Direct scans for trailing whitespace and merge-conflict markers passed, which matters because this plan file is new and not yet tracked by git.
- Placeholder scan found only this review-result marker before the fix, and no unresolved design placeholder was present in the plan content.
- Boundary scan confirmed the plan states the FastDB Python generic call-db runtime prerequisite before C-Two wrapper deletion.
- Boundary scan confirmed the plan keeps C-Two route identity and transport lease authority out of FastDB while keeping FastDB storage/view/runtime behavior out of C-Two route, relay, IPC, scheduler, lease, config, error, and lifecycle layers.
- Verification matrix covers FastDB prerequisite tests, focused C-Two hold/borrowed tests, direct IPC, relay, thread-local behavior, normal materialized calls, Python example syntax, Rust core tests, and boundary text/import scans.

No open findings remain for this documentation goal. This document is ready to serve as the implementation plan for the next C-Two/FastDB boundary remediation goal.
