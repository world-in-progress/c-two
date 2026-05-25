# C-Two FastDB 0.1.18 Lifetime Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move C-Two's FDB-first CRM payload lifetime model onto the released FastDB 0.1.18 owner, invalidation, and materialization APIs.

**Architecture:** FastDB owns backed value semantics through `FdbViewOwner`, `fdb.invalidate(...)`, `fdb.materialize(...)`, and owner-bound table, row, column, string, bytes, and numeric views. C-Two owns transport buffer leases and the policy switches `cc.hold(...)` and `cc.InputLifetime.BORROWED`; when a C-Two lease ends, it invalidates the FastDB owner before releasing the transport memory. The call-db envelope stays internal: user-facing held values follow the CRM logical return annotation instead of exposing `FastdbCallView.table("return_0")` as the public API.

**Tech Stack:** Python SDK, FastDB `fastdb4py>=0.1.18`, Rust native transport only if existing Python/native boundaries require it, `uv`, `pytest`.

---

## Scope

This plan is C-Two-side integration work after FastDB 0.1.18 has been published. It must not add new FastDB storage features, benchmarks, neutral authoring schema, or TypeScript SDK parity work. It also must not revive provider, neutral-provider, old codec-provider, or legacy `@transferable` compatibility paths.

The immediate target is a functional lifetime closure:

- normal FDB calls materialize and release transport buffers immediately;
- `cc.hold(...)` returns `cc.Held[R]` where `R` is the CRM logical return shape;
- server `InputLifetime.BORROWED` passes owner-bound FastDB views and invalidates them in `finally`;
- escaped rows, columns, single features, arrays, string columns, bytes columns, and checked numeric columns fail after release;
- `fdb.materialize(...)` or `.to_owned()` produces retained owned data that remains valid after release;
- legacy method metadata such as `@cc.transfer(buffer="hold")` cannot bypass `input_lifetime`.

## Current Evidence

- `sdk/python/pyproject.toml` still declares `fastdb4py>=0.1.17`, and `uv.lock` currently resolves `fastdb4py` to `0.1.17`.
- A local probe under the current C-Two environment reports `fastdb4py==0.1.17` and no `materialize`, `FdbViewOwner`, or `invalidate` exports.
- `sdk/python/src/c_two/fastdb/call_db.py` currently checks `memoryview.nbytes` in `FastdbCallView._ensure_alive()`, but rows and columns returned from `engine.table(...)` are not bound to a FastDB owner.
- `sdk/python/src/c_two/crm/transferable.py` still allows `@cc.transfer(buffer="hold")` metadata and can set `_input_buffer_mode="hold"` without `cc.register(..., input_lifetime=...)`.
- Existing tests cover top-level release after `held.release()`, but do not fully cover escaped held child rows/columns or borrowed single-feature inputs after release.

## File Map

- Modify `sdk/python/pyproject.toml`: raise dependency to `fastdb4py>=0.1.18`.
- Modify `uv.lock`: refresh resolved FastDB artifacts.
- Modify `sdk/python/tests/unit/test_fastdb_abi.py`: assert the new minimum dependency.
- Modify `sdk/python/src/c_two/fastdb/call_db.py`: create and propagate `FdbViewOwner`; return logical held values; use FastDB `materialize`.
- Modify `sdk/python/src/c_two/crm/payload_plan.py`: add the smallest internal hook needed for owner-aware retained views if the current single-argument `view_from_buffer(memoryview)` shape is insufficient.
- Modify `sdk/python/src/c_two/crm/transferable.py`: invalidate held values before release; invalidate borrowed input values in the server wrapper; remove or hard-fail legacy `buffer="hold"` input policy.
- Modify `sdk/python/src/c_two/transport/server/native.py`: keep transport release authority but route server borrowed cleanup through the transfer wrapper's invalidation path; keep Rust buffer release idempotent.
- Modify `sdk/python/src/c_two/fastdb/annotations.py`, examples, and docs: move authored FDB value types to `import fastdb4py as fdb`; keep `c_two.fastdb` for C-Two integration helpers such as bridge/codegen.
- Modify focused tests under `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`, `sdk/python/tests/integration/test_fastdb_crm_smoke.py`, and unit tests near `test_held_result.py` or a new `test_fastdb_lifetime.py`.
- Modify `README.md` and `examples/python/grid/README.md` to describe the final public API.

## Task 1: Bump FastDB Dependency And Establish Baseline

**Files:**
- Modify: `sdk/python/pyproject.toml`
- Modify: `uv.lock`
- Modify: `sdk/python/tests/unit/test_fastdb_abi.py`

- [x] **Step 1: Update the dependency floor**

Change the SDK dependency from `fastdb4py>=0.1.17` to `fastdb4py>=0.1.18`.

- [x] **Step 2: Refresh the lockfile**

Run:

```bash
uv lock
```

Expected: `uv.lock` resolves `fastdb4py` to `0.1.18` or a newer compatible release.

- [x] **Step 3: Update the dependency assertion**

Change `test_c_two_python_package_declares_fastdb_dependency` to assert `fastdb4py>=0.1.18`.

- [x] **Step 4: Verify the runtime FastDB API is present**

Run:

```bash
uv run python - <<'PY'
import importlib.metadata
import fastdb4py as fdb
print(importlib.metadata.version("fastdb4py"))
assert hasattr(fdb, "FdbViewOwner")
assert hasattr(fdb, "invalidate")
assert hasattr(fdb, "materialize")
PY
```

Expected: version is `0.1.18` or newer and all assertions pass.

## Task 2: Add Failing Lifetime Regression Tests

**Files:**
- Modify: `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`
- Modify: `sdk/python/tests/integration/test_fastdb_crm_smoke.py`
- Modify or create: `sdk/python/tests/unit/test_fastdb_lifetime.py`

- [x] **Step 1: Held batch child row escape must fail after release**

Add a direct IPC test using an FDB `Batch[Feature]` return. The test must store `row = held.value[0]`, release the held result, and assert `row.some_field` raises FastDB's invalidated-view error or a C-Two error that wraps it.

- [x] **Step 2: Held column escape must fail after release**

Add a direct IPC test using `column = held.value.column.global_id`, release the held result, and assert `column[0]` or `column.to_numpy()` raises after release. If the test intentionally calls `unsafe_numpy_view()`, assert that this is documented as an explicit unsafe escape and do not use it as the normal held path.

- [x] **Step 3: Held single feature escape must fail after release**

Add a direct IPC test where `held.value` is a single FDB feature. Store `point = held.value`, release the held result, and assert `point.x` raises.

- [x] **Step 4: Borrowed server input escape must fail after call return**

Add a direct IPC test using `cc.register(..., input_lifetime={"method": cc.InputLifetime.BORROWED})`. The resource stores the borrowed input row, array, or batch on `self`. After the client call returns, accessing the stored value must raise.

- [x] **Step 5: Borrowed server input can be retained through FastDB materialization**

Add the paired positive test where the resource stores `fastdb4py.materialize(values)` or `values.to_owned()`. After the call returns, the stored owned value remains readable.

- [x] **Step 6: Legacy `buffer="hold"` cannot enable server input borrowing**

Add a test that attempts to use `@transfer(buffer="hold")` from `c_two.crm.transferable` and proves it either raises during decoration/CRM construction or is ignored without producing retained resource-input holds. The preferred assertion is a clear `TypeError` or `ValueError` saying server input borrowing is controlled by `input_lifetime`.

## Task 3: Bind Call-DB Views To FastDB Owners

**Files:**
- Modify: `sdk/python/src/c_two/fastdb/call_db.py`
- Modify if needed: `sdk/python/src/c_two/crm/payload_plan.py`

- [x] **Step 1: Import FastDB lifetime APIs without fallback**

Since C-Two now depends on `fastdb4py>=0.1.18`, import `FdbViewOwner`, `invalidate`, and `materialize` directly. Remove fallback behavior that silently treats missing `materialize` as acceptable.

- [x] **Step 2: Add owner state to retained call-db views**

Create a checked read-only owner for retained response and borrowed request views:

```python
owner = fdb.FdbViewOwner(checked=True, writeable=False)
```

Attach this owner to the internal call-db retained view and expose it through `_fdb_owner` or an equivalent attribute that `fastdb4py.invalidate(...)` can discover.

- [x] **Step 3: Pass owner into every backed FastDB table**

When calling `engine.table(feature_type, name=..., ...)` for retained or borrowed views, pass `owner=owner` and `writeable=False`. This ensures rows, checked numeric columns, `StringColumn`, and `BytesColumn` are FastDB owner-bound.

- [x] **Step 4: Return logical values from retained output**

Change retained output construction so `cc.hold(proxy.method)().value` is the logical CRM return shape:

- `fdb.Batch[T]` returns a batch/table-like FastDB value with list and column access;
- `fdb.Array[T]` returns an array-like value;
- a single feature returns the feature view;
- a scalar returns the scalar;
- multiple return values return a tuple of logical values.

The envelope methods `table("return_0")`, `array("return_0")`, `feature("return_0")`, and `scalar("return_0")` must remain internal debugging/adapter details rather than the public held-value API.

- [x] **Step 5: Keep materialized paths detached**

Use `fastdb4py.materialize(...)` for normal call results and default server inputs. Remove C-Two-owned ad hoc materialization where FastDB can do the job.

## Task 4: Make C-Two Release Invalidate FastDB Values

**Files:**
- Modify: `sdk/python/src/c_two/crm/transferable.py`
- Modify if needed: `sdk/python/src/c_two/transport/server/native.py`

- [x] **Step 1: Invalidate held values before transport release**

In `HeldResult.release()` or in the held response release callback, call `fastdb4py.invalidate(self._value)` before releasing the retained `memoryview` and native response buffer. The release path must remain idempotent.

- [x] **Step 2: Preserve transport release authority**

Keep native `response.release()` and `request_buf.release()` as the transport memory release authority. FastDB invalidation only marks Python views stale; it must not replace Rust/C-Two buffer lease release.

- [x] **Step 3: Invalidate borrowed input in `finally`**

For `input_lifetime=cc.InputLifetime.BORROWED`, invalidate the deserialized FDB input values after output serialization has completed or after any resource/serialization error is converted. Then release the request memoryview/native request buffer.

- [x] **Step 4: Avoid invalidating materialized inputs**

Only call `fastdb4py.invalidate(...)` for retained or borrowed FDB views. Materialized default inputs and Python pickle fallback values must keep current semantics.

## Task 5: Remove Legacy Input Hold Bypass

**Files:**
- Modify: `sdk/python/src/c_two/crm/transferable.py`
- Modify: `sdk/python/src/c_two/crm/meta.py` only if needed
- Modify: stale tests and docs that still preserve the old behavior

- [x] **Step 1: Reject `@cc.transfer(buffer="hold")` as an input lifetime policy**

Make `transfer(buffer="hold")` fail with a message such as:

```text
server-side borrowed input is controlled by cc.register(..., input_lifetime=...)
```

- [x] **Step 2: Remove `_input_buffer_mode="hold"` as a server dispatch mode**

After this cleanup, server dispatch buffer modes should be `view` for materialized/default paths and `borrowed` for explicit `InputLifetime.BORROWED`. There should be no production path where method metadata alone causes retained resource input.

- [x] **Step 3: Keep `cc.hold(...)` client-side only**

Ensure `cc.hold(proxy.method)` still injects the client response hold request and does not imply anything about server input lifetime.

## Task 6: Move Authored FDB Types To FastDB Namespace

**Files:**
- Modify: `examples/python/grid/grid_fdb_crm.py`
- Modify: `examples/python/fastdb_relay_client.py`
- Modify: `examples/python/grid/README.md`
- Modify: `README.md`
- Modify tests that author CRMs with `c_two.fastdb` value aliases
- Modify: `sdk/python/src/c_two/fastdb/annotations.py` and `sdk/python/src/c_two/fastdb/__init__.py`

- [x] **Step 1: Update examples to import FastDB value types directly**

Use:

```python
import fastdb4py as fdb
```

for `@fdb.feature`, `fdb.I32`, `fdb.Array[...]`, and `fdb.Batch[...]`.

- [x] **Step 2: Keep C-Two FastDB module integration-focused**

Keep `c_two.fastdb` for C-Two-owned bridge/codegen/call-db integration helpers. Do not present it as the home of FDB value types.

- [x] **Step 3: Update public docs**

Describe this ownership split:

- FastDB owns value types, backed views, materialization, and invalidation;
- C-Two owns `cc.hold`, `cc.Held`, transport leases, route identity, relay, IPC, and `cc.InputLifetime`;
- bridge helpers adapt shapes but do not own buffer lifetime.

## Task 7: Verification

**Files:**
- No new files required unless failures reveal missing tests.

- [x] **Step 1: Run focused dependency and lifetime tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_held_result.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=30
```

- [x] **Step 2: Run broad FastDB CRM integration tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_crm_smoke.py -q --timeout=30
```

- [x] **Step 3: Run syntax/import verification**

Run:

```bash
uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests examples/python
```

- [x] **Step 4: Run full Python SDK suite if focused tests pass**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
```

- [x] **Step 5: Run Rust tests only if Rust/native files changed**

Run:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
```

- [x] **Step 6: Run final hygiene scans**

Run:

```bash
git diff --check
rg -n "buffer=['\"]hold['\"]|@cc\\.transfer\\(|@transfer\\(" sdk/python/src/c_two examples/python sdk/python/tests README.md docs/plans
rg -n "from c_two import fastdb as fdb|import c_two\\.fastdb as fdb|from c_two\\.fastdb import .*Array|from c_two\\.fastdb import .*Batch" examples/python sdk/python/tests README.md
```

Expected: no production or public-example use remains that contradicts the new model. Historical docs under `docs/` may still contain archival references if they are clearly historical and not public current guidance.

Verification result: focused dependency/lifetime tests, broad FastDB CRM integration tests, compileall, and the full Python SDK suite passed. Rust tests were not run because this implementation did not touch Rust source. `git diff --check` passed. The FDB authoring import scan returned no current example/test/README use of `c_two.fastdb` value aliases; the transfer/hold scan only reported historical plan text, explicit negative tests, `cc.hold` client-side `_c2_buffer="hold"` injection, and error messages rejecting `buffer="hold"`.

## Acceptance Criteria

- C-Two resolves FastDB 0.1.18 or newer in the active lockfile and runtime.
- `cc.hold(...)` returns `Held[R]` where `.value` is the CRM logical return shape, not the call-db envelope.
- `held.release()` invalidates escaped held rows, columns, string/bytes columns, single features, and checked numeric views before transport memory is released.
- Default server FDB inputs are materialized owned values.
- `InputLifetime.BORROWED` server inputs are owner-bound views and are invalidated after the call.
- `fdb.materialize(...)` or `.to_owned()` lets resources and clients retain data safely after release.
- `@cc.transfer(buffer="hold")` cannot enable server input borrowing.
- Examples and public docs use `fastdb4py as fdb` for FDB value types.
- No benchmark work, PR creation, commit, push, or publishing happens in this goal unless the user explicitly asks.

## Goal Prompt

Use this prompt for the next Codex goal:

```text
Follow docs/plans/2026-05-24-c-two-fastdb-0-1-18-lifetime-integration.md to implement C-Two's FastDB 0.1.18 lifetime integration. Bump C-Two to fastdb4py>=0.1.18, use FastDB FdbViewOwner/invalidate/materialize for held responses and borrowed inputs, make cc.hold return logical CRM values instead of the call-db envelope, block legacy @cc.transfer(buffer="hold") input borrowing, update examples/docs to import FDB value types from fastdb4py, add escape/materialization regression tests, and run the plan's verification commands. Do not benchmark, commit, push, publish, or start neutral authoring schema work.
```
