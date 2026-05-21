# C-Two FastDB-First Boundary Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the FastDB-backed portable CRM contract integration into C-Two, make FastDB call-db the first-class C-Two portable payload ABI, and clean FastDB back to generic schema/storage/serialization/runtime ownership.

**Architecture:** C-Two owns CRM contract planning, bridge application, descriptor/artifact/codegen orchestration, generated TypeScript client/facade behavior, and cross-transport verification. FastDB owns generic schema descriptors, call-db storage/runtime primitives, serializers, views, owned bytes, and WASM/runtime capabilities. The migration is a clean-cut 0.x refactor: provider-neutral public language and optional install flows are removed or demoted, while C-Two runtime route/relay/IPC/scheduler/lease layers still do not inspect FastDB storage internals.

**Tech Stack:** Python 3.10+ SDK with `uv`/maturin/PyO3, Rust core crates and `c3` CLI, FastDB Python package `fastdb4py`, FastDB TypeScript runtime `fastdb4ts`, generated TypeScript clients, Node test runner, pytest, Cargo.

---

## Source Documents

- Design spec: `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md`
- Prior implementation plan: `docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`
- Closure review packet: `docs/plans/2026-05-20-fastdb-first-closure-review-packet.md`
- TypeScript IPC/runtime requirement notes: `docs/plans/2026-05-20-typescript-ipc-shm-runtime-adapter-requirements.md`
- FastDB sibling plan material: `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/docs/superpowers/plans/2026-05-18-c-two-call-db-codec.md`

## Boundary Inventory From Current Worktree

### C-Two surfaces to migrate or rewrite

- Docs with provider-neutral or FastDB-neutral wording: `README.md`, `README.zh-CN.md`, `docs/roadmap.md`, `docs/roadmap.zh-CN.md`, `docs/vision/cross-language-contract-codec-architecture.md`, `docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`, `docs/plans/2026-05-20-fastdb-first-closure-review-packet.md`, and `sdk/python/src/c_two/providers/README.md`.
- Public/provider surfaces to demote or hide: `sdk/python/src/c_two/crm/codec.py`, `sdk/python/src/c_two/providers/arrow.py`, `sdk/python/src/c_two/providers/custom.py`, `sdk/python/src/c_two/providers/README.md`, `sdk/python/src/c_two/__init__.py` exports for `use_codec`, `bind_codec`, `PayloadAbiBinding`, and method provider shapes.
- FastDB examples/tests that currently depend on FastDB-owned C-Two glue: `examples/python/grid/grid_fdb_crm.py`, `examples/python/grid/fastdb_bridge.py`, `sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py`, and `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`.
- Optional Arrow/custom provider tests to demote: `sdk/python/tests/unit/test_arrow_provider.py`, `sdk/python/tests/unit/test_custom_provider.py`, `sdk/python/tests/integration/test_arrow_view_provider_smoke.py`, `sdk/python/tests/integration/test_custom_codec_provider_smoke.py`, and `sdk/python/tests/unit/test_grid_arrow_codec.py`.
- C-Two bridge/runtime pieces to keep but re-anchor to FastDB ABI: `sdk/python/src/c_two/crm/bridge.py`, `sdk/python/src/c_two/crm/conformance.py`, `sdk/python/src/c_two/crm/descriptor.py`, `sdk/python/src/c_two/crm/transferable.py`, `sdk/python/src/c_two/transport/registry.py`, and `sdk/python/src/c_two/transport/server/native.py`.
- Rust/codegen pieces to preserve and retarget: `core/foundation/c2-contract`, `core/foundation/c2-codegen`, `core/foundation/c2-mem-ffi`, and `cli/src/contract.rs`.

### FastDB sibling surfaces to migrate out of FastDB

- Python C-Two glue modules: `python/fastdb4py/c_two_provider.py`, `python/fastdb4py/c_two_call.py`, `python/fastdb4py/c_two_bridge.py`, and `python/fastdb4py/codegen/c_two_ts.py`.
- Python exports and CLI hooks: `python/fastdb4py/__init__.py`, `python/fastdb4py/codegen/__init__.py`, and `python/fastdb4py/cli.py`.
- TypeScript C-Two runtime facade: `ts/fastdb4ts/src/c-two.ts` and the export from `ts/fastdb4ts/src/index.ts`.
- FastDB C-Two tests to move or split: `tests/python/test_c_two_schema_provider.py`, `tests/python/test_c_two_call_db.py`, `tests/python/test_c_two_bridge.py`, `tests/python/test_c_two_codegen.py`, and `tests/ts/test_c_two_runtime.mjs`.
- FastDB docs to shrink: `README.md`, `docs/c-two-provider-architecture.md`, and `docs/superpowers/plans/2026-05-18-c-two-call-db-codec.md`.

### Current facts that change the old closure packet

- The closure packet's FastDB-first PR order is superseded by this redesign. FastDB should no longer receive a broad C-Two-specific PR first.
- C-Two should add a direct FastDB ABI dependency in the contract layer. The target is not "C-Two production has no FastDB imports"; the target is "C-Two runtime route/relay/IPC/scheduler/lease layers do not parse FastDB storage internals."
- The old `install_c_two_provider(cc_module=cc)` flow is a migration source, not the desired user API.
- Item 91 remains out of scope until this boundary redesign is landed and verified.

## Target File Structure

### C-Two Python

- Create `sdk/python/src/c_two/fastdb/__init__.py`: public FastDB ABI module and ergonomic exports.
- Create `sdk/python/src/c_two/fastdb/annotations.py`: C-Two-facing FastDB aliases and annotation exports backed by `fastdb4py.type`.
- Create `sdk/python/src/c_two/fastdb/call_db.py`: C-Two-owned call-db planning, codec refs, method input/output envelope transferables, and diagnostics migrated from FastDB.
- Create `sdk/python/src/c_two/fastdb/bridge.py`: C-Two-owned bridge derivation and bridge helpers migrated from FastDB-specific glue.
- Create `sdk/python/src/c_two/fastdb/artifacts.py`: FastDB sidecar artifact collection for `c3 contract artifacts` and resource-first infer artifact paths.
- Create `sdk/python/src/c_two/fastdb/typescript.py`: Python helper surface for FastDB-backed TypeScript facade generation if the generator keeps Python-side schema assembly.
- Modify `sdk/python/src/c_two/__init__.py`: expose `fastdb` and only a minimal approved subset of FastDB ABI names if the examples prove top-level re-exports improve readability.
- Modify `sdk/python/pyproject.toml`: add the direct `fastdb4py` dependency once the C-Two-owned module is the official portable path.

### C-Two Rust and CLI

- Modify `cli/src/contract.rs`: make C-Two codegen/artifacts own the FastDB-backed TypeScript workflow instead of delegating the main path to `fdb codegen --c-two-ts`.
- Modify `cli/tests/contract_commands.rs`: assert the generated FastDB-backed TypeScript flow runs from `c3 contract ...`.
- Modify `core/foundation/c2-codegen/src/lib.rs`: keep transport generation in Rust and add FastDB-backed typed facade emission or a deterministic handoff to the C-Two FastDB ABI generator.
- Modify `core/foundation/c2-codegen/tests/typescript.rs`: cover FastDB strict path, held calls, typed facade shape, allocator integration, and rejection of non-FastDB codecs in portable mode.
- Keep `core/foundation/c2-contract` as the descriptor validator and hash authority; extend only if descriptor rules need to encode FastDB-only portable policy more explicitly.

### FastDB sibling

- Modify `python/fastdb4py/__init__.py`: stop exporting C-Two-specific modules as core package surface.
- Modify `python/fastdb4py/cli.py`: remove `--c-two-ts` or convert it into a deprecated error that points to the C-Two `c3 contract` workflow during the migration window.
- Modify `python/fastdb4py/codegen/__init__.py`: remove C-Two codegen exports after C-Two owns them.
- Remove or reduce `python/fastdb4py/c_two_provider.py`, `python/fastdb4py/c_two_call.py`, `python/fastdb4py/c_two_bridge.py`, and `python/fastdb4py/codegen/c_two_ts.py` after their generic pieces are either moved to C-Two or re-expressed as FastDB-generic utilities.
- Modify `ts/fastdb4ts/src/index.ts`: stop exporting C-Two-specific runtime facade as a core FastDB export.
- Remove or reduce `ts/fastdb4ts/src/c-two.ts` after C-Two owns generated/runtime facade composition.
- Keep generic FastDB runtime files such as `python/fastdb4py/schema.py`, `python/fastdb4py/type.py`, `python/fastdb4py/column_engine.py`, `python/fastdb4py/object_engine.py`, `ts/fastdb4ts/src/feature.ts`, `ts/fastdb4ts/src/orm.ts`, `ts/fastdb4ts/src/database-buffer.ts`, and WSTR/BYTES runtime support.

## Phase 0: Boundary Inventory And Freeze

### Task 0.1: Record current boundary inventory

**Files:**
- Modify: `docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md`
- Read-only evidence: C-Two and FastDB paths listed in "Boundary Inventory From Current Worktree"

- [x] **Step 1: Refresh the C-Two inventory scan**

Run:

```bash
rg -l "provider-neutral|fastdb-neutral|C-Two core remains FastDB-neutral|C-Two production code must remain fastdb-neutral|Payload codecs are enabled through providers|c_two\\.providers|py-arrow provider|custom provider|install_c_two_provider|fastdb4py\\.c_two|fdb codegen --c-two-ts" README.md README.zh-CN.md docs sdk/python/src sdk/python/tests examples cli core --glob '!**/.venv/**' | sort
```

Expected: the output includes the C-Two files listed in this plan's inventory section and no unreviewed new file categories.

- [x] **Step 2: Refresh the FastDB inventory scan**

Run from `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`:

```bash
rg -l "fastdb4py\\.c_two|fastdb4ts/src/c-two|codegen/c_two|c_two_provider|c_two_bridge|c_two_call|fdb codegen --c-two-ts|C-Two provider|C-Two codec|C-Two call-db|createFastdbC2|FastdbC2" README.md docs python/fastdb4py ts/fastdb4ts/src tests/python tests/ts --glob '!**/.venv/**' | sort
```

Expected: the output includes the FastDB sibling files listed in this plan's inventory section and no unreviewed new file categories.

- [x] **Step 3: Freeze item 91 and old PR order**

Edit `docs/plans/2026-05-20-fastdb-first-closure-review-packet.md` so the old "FastDB PR first, C-Two PR second" order is marked superseded by `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md` and this plan. Keep the old verification evidence intact.

- [x] **Step 4: Verify Phase 0 documentation hygiene**

Run:

```bash
git diff --check -- docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md docs/plans/2026-05-20-fastdb-first-closure-review-packet.md
rg -n "T[B]D|TO[D]O|PLACE[H]OLDER|FIXM[E]|<{7}|={7}|>{7}" docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md docs/plans/2026-05-20-fastdb-first-closure-review-packet.md
```

Expected: `git diff --check` exits 0; the `rg` command exits 1 with no matches.

### Checkpoint: Phase 0

- [x] User confirms the inventory categories and migration order before implementation starts.
- [x] No implementation code changed during Phase 0; the existing prior fastdb-first implementation diff remains untouched as migration material.
- [x] The plan still states that item 91 is out of scope.

## Phase 1: C-Two FastDB ABI Module Skeleton

### Task 1.1: Add the public `c_two.fastdb` module with direct FastDB ABI dependency

**Files:**
- Create: `sdk/python/src/c_two/fastdb/__init__.py`
- Create: `sdk/python/src/c_two/fastdb/annotations.py`
- Modify: `sdk/python/src/c_two/__init__.py`
- Modify: `sdk/python/pyproject.toml`
- Test: `sdk/python/tests/unit/test_fastdb_abi.py`

- [x] **Step 1: Write the failing import and dependency tests**

Create `sdk/python/tests/unit/test_fastdb_abi.py` with assertions that `import c_two.fastdb as fdb` exposes FastDB aliases and that top-level `c_two` exposes the `fastdb` submodule without requiring `install_c_two_provider(...)`.

The initial test should fail because `c_two.fastdb` does not exist yet.

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
```

Expected before implementation: import failure for `c_two.fastdb`.

- [x] **Step 2: Add the dependency**

Modify `sdk/python/pyproject.toml` so `[project].dependencies` includes `fastdb4py`. If the published dependency version is available in the development index, use a lower bound matching the sibling package version currently observed in `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/pyproject.toml`:

```toml
dependencies = [
    "fastdb4py>=0.1.16",
]
```

Local development now resolves `fastdb4py` from the sibling checkout via root `pyproject.toml` source override because the PyPI `fastdb4py==0.1.16` package is older than the current FastDB ABI surface needed by C-Two:

```toml
[tool.uv.sources]
fastdb4py = { path = "../fastdb", editable = true }
```

Before merge/release, either publish the current FastDB ABI surface or keep release instructions explicit that source override is a development-only workspace dependency.

- [x] **Step 3: Add `c_two.fastdb.annotations`**

Create `sdk/python/src/c_two/fastdb/annotations.py` to import and re-export FastDB ABI annotation names from `fastdb4py.type`, including the scalar aliases and `Array` / `Batch`. Keep this file as C-Two-facing API glue only; it must not import C-Two transport modules.

- [x] **Step 4: Add `c_two.fastdb.__init__`**

Create `sdk/python/src/c_two/fastdb/__init__.py` to expose the annotation names and a module docstring that says FastDB call-db is the C-Two portable CRM payload ABI.

- [x] **Step 5: Expose the submodule from top-level `c_two`**

Modify `sdk/python/src/c_two/__init__.py` so `from c_two import fastdb` works. Do not yet re-export every scalar alias from top-level `c_two`; keep the first slice explicit.

- [x] **Step 6: Verify focused tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
uv run python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/tests/unit/test_fastdb_abi.py
git diff --check
```

Expected: tests pass, compileall passes, and `git diff --check` exits 0.

Task 1.1 verification on 2026-05-20 used the sibling FastDB checkout through the root `uv` source override:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/tests/unit/test_fastdb_abi.py
uv run --locked python -c "import fastdb4py, c_two, c_two.fastdb as fdb; print(fastdb4py.__file__); print(c_two.fastdb is fdb, fdb.Array, fdb.Batch, fdb.feature)"
C2_RELAY_ANCHOR_ADDRESS= uv run --locked python - <<'PY'
import sys
import c_two
print('after import c_two', 'fastdb4py.c_two_provider' in sys.modules, 'fastdb4py.c_two_bridge' in sys.modules)
from c_two import fastdb
print('after from import fastdb', fastdb.__name__, c_two.fastdb is fastdb)
PY
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_sdk_boundary.py sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
git diff --check
```

Observed evidence: focused ABI tests reported `4 passed in 0.19s`; compileall exited 0; import smoke printed `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/python/fastdb4py/__init__.py` and `True <class 'fastdb4py.type.Array'> <class 'fastdb4py.type.Batch'> <function feature ...>`; lazy import smoke printed `after import c_two False False` and `after from import fastdb c_two.fastdb True`; adjacent SDK boundary run reported `38 passed in 0.49s`; `git diff --check` exited 0.

### Task 1.2: Make official FastDB ABI planning not require provider installation

**Files:**
- Create: `sdk/python/src/c_two/fastdb/call_db.py`
- Modify: `sdk/python/src/c_two/crm/transferable.py`
- Modify: `sdk/python/src/c_two/crm/descriptor.py`
- Test: `sdk/python/tests/unit/test_fastdb_abi.py`
- Test: `sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py`

- [x] **Step 1: Write RED tests for no-install FastDB CRM resolution**

Add tests where a CRM method uses `c_two.fastdb` aliases and resolves to `org.fastdb.call-db` wire refs without calling `fastdb4py.c_two_provider.install_c_two_provider(cc_module=cc)`.

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
```

Expected before implementation: the descriptor still falls back to pickle or requires the old provider install.

- [x] **Step 2: Move call-db planning into C-Two**

Port the method input/output planning code from FastDB's `python/fastdb4py/c_two_call.py` into `sdk/python/src/c_two/fastdb/call_db.py`. Keep direct calls into FastDB generic schema/type/runtime APIs, but place the C-Two method envelope, CRM context, codec-ref construction, and transferable class generation in C-Two.

- [x] **Step 3: Wire FastDB ABI planning before pickle fallback**

Modify `sdk/python/src/c_two/crm/transferable.py` so `auto_transfer()` checks the C-Two FastDB ABI planner for method input/output shapes before default pickle creation. Preserve Python-only pickle fallback only for shapes the FastDB planner rejects and only outside portable export.

- [x] **Step 4: Keep descriptor diagnostics FastDB-first**

Modify `sdk/python/src/c_two/crm/descriptor.py` so diagnostics for rejected FastDB ABI shapes mention FastDB call-db requirements directly, not a generic provider ecosystem.

- [x] **Step 5: Verify focused and adjacent tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_contract_infer.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=120
uv run python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/src/c_two/crm sdk/python/tests/unit/test_fastdb_abi.py
git diff --check
```

Expected: FastDB-backed CRM descriptors no longer require provider installation; old tests are either migrated to the new official path or intentionally marked legacy/prototype.

Task 1.2 verification on 2026-05-20:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_fastdb_abi.py sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_contract_infer.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_sdk_boundary.py sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=120
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/src/c_two/crm/transferable.py sdk/python/src/c_two/crm/descriptor.py sdk/python/tests/unit/test_fastdb_abi.py sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_contract_infer.py
git diff --check
```

Observed evidence: no-install ABI tests reported `6 passed in 0.14s`; focused contract export/infer adjacency reported `27 passed in 0.77s`; SDK boundary adjacency reported `40 passed in 0.48s`; full FastDB provider integration smoke reported `116 passed in 310.50s`; compileall and `git diff --check` exited 0.

### Checkpoint: Phase 1

- [x] `from c_two import fastdb as fdb` or `import c_two.fastdb as fdb` is the documented authoring path.
- [x] FastDB-backed portable CRM descriptors work without `install_c_two_provider(...)`.
- [x] Python pickle fallback still works for Python-only non-portable local flows.

## Phase 2: Bridge Ownership Migration

### Task 2.1: Move bridge derivation into C-Two

**Files:**
- Create: `sdk/python/src/c_two/fastdb/bridge.py`
- Modify: `sdk/python/src/c_two/crm/bridge.py`
- Modify: `examples/python/grid/fastdb_bridge.py`
- Test: `sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py`
- Test: `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`

- [x] **Step 1: Write or update RED tests for C-Two-owned bridge derivation**

Change the relevant C-Two tests to import `derive_bridge_hooks` / `derive_c_two_bridge` from `c_two.fastdb.bridge` instead of `fastdb4py.c_two_bridge`.

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py::test_fastdb_derived_scalar_bridge_across_transports -q --timeout=120
```

Expected before implementation: import failure for `c_two.fastdb.bridge`.

- [x] **Step 2: Port bridge helpers**

Move C-Two-specific bridge derivation from FastDB's `python/fastdb4py/c_two_bridge.py` into `sdk/python/src/c_two/fastdb/bridge.py`. Keep helper functions that convert resource values into FastDB generic values inside C-Two only if they exist solely to build C-Two `cc.bridge(...)` hooks.

- [x] **Step 3: Keep C-Two bridge execution unchanged**

Do not move execution authority from `sdk/python/src/c_two/crm/bridge.py`, `sdk/python/src/c_two/transport/registry.py`, or `sdk/python/src/c_two/transport/server/native.py` into FastDB. The C-Two FastDB bridge module should produce ordinary `ResourceBridge` mappings consumed by existing C-Two runtime paths.

- [x] **Step 4: Verify cross-transport bridge behavior**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
git diff --check
```

Expected: thread-local, direct IPC, explicit relay, and held bridge smokes still pass with imports rooted in C-Two.

Task 2.1 verification on 2026-05-20:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py::test_fastdb_derived_scalar_bridge_across_transports -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py examples/python/grid/fastdb_bridge.py
rg -n "fastdb4py\\.c_two_bridge|from fastdb4py import c_two_bridge" sdk/python/src sdk/python/tests examples/python/grid
git diff --check
```

Observed evidence: the focused RED failed with `ModuleNotFoundError: No module named 'c_two.fastdb.bridge'`; after porting, the focused scalar bridge test reported `1 passed in 4.25s`; full FastDB provider smoke reported `116 passed in 311.85s`; grid bridge smoke reported `12 passed in 25.45s`; compileall and `git diff --check` exited 0. The source scan found no direct C-Two source/test/example imports from `fastdb4py.c_two_bridge`; only plan text and the intentional `test_fastdb_abi.py` lazy-import sys.modules guard still mention the old module. Current sibling FastDB still eager-exports its old C-Two bridge from `fastdb4py/__init__.py`, so removing that indirect package-level exposure remains a Phase 4 FastDB cleanup item.

### Task 2.2: Preserve FastDB-native no-op bridge behavior

**Files:**
- Modify: `sdk/python/src/c_two/fastdb/bridge.py`
- Test: `sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py`

- [x] **Step 1: Keep the no-op identity regression**

Preserve the existing behavior from the prior plan: if CRM and resource annotations are already the same FastDB ABI, C-Two bridge derivation returns an empty bridge map and C-Two still carries the FastDB call-db payload through ordinary and held calls.

- [x] **Step 2: Verify the focused no-op test**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py::test_fastdb_native_resource_uses_empty_bridge_across_transports -q --timeout=120
```

Expected: the test passes without importing `fastdb4py.c_two_bridge`.

Task 2.2 verification on 2026-05-20:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py::test_fastdb_native_resource_uses_empty_bridge_across_transports -q --timeout=120
```

Observed evidence: the focused no-op identity regression reported `1 passed in 4.11s` with the test importing `derive_c_two_bridge` from `c_two.fastdb.bridge`.

### Checkpoint: Phase 2

- [x] C-Two owns bridge derivation for C-Two registration.
- [x] FastDB still owns generic type/schema/runtime semantics only.
- [x] All existing scalar, `Array`, `Batch`, WSTR, BYTES, and held-view bridge smokes are either passing or intentionally moved with matching coverage.

## Phase 3: TypeScript Codegen Ownership Migration

### Task 3.1: Move FastDB-backed TypeScript helper generation to C-Two

**Files:**
- Modify: `core/foundation/c2-codegen/src/lib.rs`
- Modify: `core/foundation/c2-codegen/tests/typescript.rs`
- Modify: `cli/src/contract.rs`
- Modify: `cli/tests/contract_commands.rs`
- Create or modify: `sdk/python/src/c_two/fastdb/typescript.py` if Python-side schema helper assembly remains necessary
- Test: `sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py`

- [x] **Step 1: Write RED tests for single C-Two codegen command**

Add a CLI test proving a FastDB-backed contract can generate the C-Two transport plus FastDB typed facade through `c3 contract codegen typescript` without invoking `fdb codegen --c-two-ts`.

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test contract_commands contract_codegen_typescript_strict_fastdb_output_compiles_with_tsc -- --nocapture
```

Expected before implementation: the test still depends on `fdb codegen --c-two-ts` or lacks the C-Two-owned typed facade output.

- [x] **Step 2: Move TypeScript facade emission**

Port the C-Two-specific generator logic from FastDB's `python/fastdb4py/codegen/c_two_ts.py` into C-Two. Keep FastDB schema validation and runtime primitive use generic; C-Two owns CRM method mapping, route/method identity, typed client facade, codec registry assembly, and held-call TypeScript shape.

- [x] **Step 3: Preserve generated transport authority in C-Two**

Keep HTTP relay, relay-aware resolve, direct IPC, SHM reader/writer, c2-mem-ffi facade, and held-result transport code in `core/foundation/c2-codegen`. FastDB runtime helpers should not own route selection, frame parsing, SHM lifecycle, or timeout/retry policy.

- [x] **Step 4: Verify generated TypeScript path**

Run:

```bash
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml --test typescript
cargo test --manifest-path cli/Cargo.toml --test contract_commands
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_export_feeds_fastdb_typescript_codegen -q --timeout=120
git diff --check
```

Expected: the generated FastDB TypeScript smoke compiles/runs from the C-Two workflow.

Task 3.1 verification on 2026-05-20:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_export_feeds_fastdb_typescript_codegen -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_c3_artifacts_cli_feeds_fastdb_codegen -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_resource_infer_c3_artifacts_cli_feeds_fastdb_codegen -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py examples/python/grid/grid_fdb_crm.py
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml --test typescript
cargo test --manifest-path cli/Cargo.toml --test contract_commands
rg -n "fastdb4py\\.codegen\\.c_two_ts|python -m fastdb4py\\.cli|--c-two-ts|fdb codegen --c-two-ts" sdk/python/src sdk/python/tests examples/python cli core
git diff --check -- cli/src/contract.rs cli/tests/contract_commands.rs sdk/python/src/c_two/fastdb/__init__.py sdk/python/src/c_two/fastdb/typescript.py sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py examples/python/grid/grid_fdb_crm.py
```

Observed evidence: the RED pass failed first with `ModuleNotFoundError: No module named 'c_two.fastdb.typescript'` and then with `unexpected argument '--fastdb-schema'`; after implementation, `c_two.fastdb.typescript` owns the copied C-Two FastDB helper generator, `c3 contract codegen typescript` accepts `--fastdb-schema`, `--fastdb-out`, and `--python`, and the grid/infer C3 smokes generate C-Two transport plus typed FastDB codec helpers without invoking `fastdb4py.cli` or `fdb codegen --c-two-ts`. The focused three-test integration group reported `3 passed in 15.40s`; the full grid FastDB bridge smoke reported `12 passed in 25.94s`; `c2-codegen` TypeScript tests reported `11 passed`; CLI contract command tests reported `23 passed`; compileall and the scoped `git diff --check` exited 0; the old codegen entry scan returned no matches in C-Two source, tests, examples, CLI, or core.

### Task 3.2: Keep FastDB TypeScript runtime generic

**Files in FastDB sibling:**
- Modify: `ts/fastdb4ts/src/index.ts`
- Modify or remove: `ts/fastdb4ts/src/c-two.ts`
- Modify: `tests/ts/test_c_two_runtime.mjs`

- [x] **Step 1: Split generic runtime tests from C-Two facade tests**

Move FastDB tests that validate generic WSTR/BYTES/list/object-graph runtime behavior to names that do not imply C-Two ownership. Keep C-Two generated facade tests in C-Two.

- [x] **Step 2: Remove core FastDB export of C-Two facade**

Stop exporting C-Two-specific symbols such as `createFastdbC2CodecRegistry`, `FastdbC2CallDbView`, and typed C-Two facade helpers from the main `fastdb4ts` entrypoint unless they have been renamed into generic FastDB view/runtime primitives.

- [x] **Step 3: Verify FastDB TypeScript runtime**

Run from `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`:

```bash
npm --prefix ts/fastdb4ts run build
npm --prefix ts/fastdb4ts run test:ts
git diff --check
```

Expected: generic FastDB runtime tests pass without main-package C-Two facade exports.

Task 3.2 verification on 2026-05-20:

```bash
npm --prefix ts/fastdb4ts run build && node --test tests/ts/test_public_exports.mjs
npm --prefix ts/fastdb4ts run build && node --test tests/ts/test_public_exports.mjs tests/ts/test_call_db_runtime.mjs
npm --prefix ts/fastdb4ts run test:ts
find ts/fastdb4ts/dist -maxdepth 1 -type f \( -name '*c-two*' -o -name '*call-db*' \) | sort
rg -n "FastdbC2|C-Two|C2|CTwo|c-two|c_two" ts/fastdb4ts/src tests/ts/test_call_db_runtime.mjs tests/ts/test_public_exports.mjs
git diff --check -- ts/fastdb4ts/package.json ts/fastdb4ts/src/index.ts ts/fastdb4ts/src/call-db.ts tests/ts/test_call_db_runtime.mjs tests/ts/test_public_exports.mjs
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_export_feeds_fastdb_typescript_codegen -q --timeout=180
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_c3_artifacts_cli_feeds_fastdb_codegen -q --timeout=120
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb
git diff --check -- sdk/python/src/c_two/fastdb/typescript.py docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md
```

Observed evidence: the RED public-export test first failed because `encodeFastdbCallDb` was `undefined`; after the runtime rename, `fastdb4ts` exports generic `encodeFastdbCallDb`, `decodeFastdbCallDb`, `viewFastdbCallDb`, `encodeFastdbFeature`, and `decodeFastdbFeature` from `src/call-db.ts`, while `src/index.ts` no longer exports `FastdbC2*` runtime names. The `fastdb4ts` build script now removes stale `dist` before compiling, and the post-build dist scan shows only `dist/call-db.js` / `dist/call-db.d.ts`, not stale `dist/c-two.*` artifacts. The focused FastDB build plus public/call-db tests reported `48` passing tests; the full `npm --prefix ts/fastdb4ts run test:ts` reported `76` passing tests after the clean-dist build change. The C-Two generator now imports the generic fastdb4ts runtime names and locally aliases `FastdbC2CallDbView` for its generated C-Two typed facade; focused C-Two grid codegen smokes reported `1 passed` each, the full grid FastDB bridge smoke reported `12 passed in 25.74s`, a post-clean-build focused C-Two grid codegen smoke reported `1 passed in 5.10s`, compileall passed, and scoped `git diff --check` passed in both repositories. The only remaining `C2` match under `ts/fastdb4ts/src` and the renamed generic TS runtime test is the deliberate negative public-export regex in `tests/ts/test_public_exports.mjs`.

### Checkpoint: Phase 3

- [x] `c3 contract codegen typescript` is the primary C-Two FastDB-backed client workflow.
- [x] `fdb codegen --c-two-ts` is no longer the main path.
- [x] FastDB TypeScript runtime remains useful without C-Two route/transport concepts.

## Phase 4: FastDB Repository Cleanup

### Task 4.1: Remove C-Two-specific Python surfaces from FastDB core package

**Files in FastDB sibling:**
- Modify: `python/fastdb4py/__init__.py`
- Modify: `python/fastdb4py/cli.py`
- Modify: `python/fastdb4py/codegen/__init__.py`
- Remove or reduce: `python/fastdb4py/c_two_provider.py`
- Remove or reduce: `python/fastdb4py/c_two_call.py`
- Remove or reduce: `python/fastdb4py/c_two_bridge.py`
- Remove or reduce: `python/fastdb4py/codegen/c_two_ts.py`
- Test: `tests/python/test_c_two_schema_provider.py`
- Test: `tests/python/test_c_two_call_db.py`
- Test: `tests/python/test_c_two_bridge.py`
- Test: `tests/python/test_c_two_codegen.py`

- [x] **Step 1: Move C-Two-specific test coverage to C-Two**

For each FastDB test file listed above, decide whether the assertion is generic FastDB runtime behavior or C-Two contract glue. Move C-Two contract glue coverage to C-Two tests. Keep generic FastDB runtime assertions in renamed FastDB tests.

- [x] **Step 2: Remove C-Two exports from `fastdb4py.__init__`**

Delete exports for `CTwoFastdbCodecProvider`, `FastdbCodecProvider` if it exists only as a C-Two candidate provider, `install_c_two_provider`, `derive_c_two_bridge`, and C-Two bridge helper names. Keep generic FastDB names such as scalar aliases, `feature`, schema export, engine classes, serializers, and runtime views.

- [x] **Step 3: Remove or deprecate `fdb codegen --c-two-ts`**

Update `python/fastdb4py/cli.py` so the FastDB CLI no longer presents C-Two codegen as a FastDB-owned command. During the transition, if removing the flag breaks too much at once, make the flag fail with a message directing users to `c3 contract codegen typescript`.

- [x] **Step 4: Verify FastDB Python suite**

Run from `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`:

```bash
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
git diff --check
```

Expected: FastDB generic tests pass; removed C-Two-specific tests have equivalent C-Two coverage.

### Task 4.2: Shrink FastDB C-Two docs

**Files in FastDB sibling:**
- Modify: `README.md`
- Modify or remove: `docs/c-two-provider-architecture.md`
- Modify: `docs/superpowers/plans/2026-05-18-c-two-call-db-codec.md`

- [x] **Step 1: Rewrite README C-Two sections**

Keep at most a short note that FastDB provides schema/storage/runtime primitives consumed by C-Two. Remove text that says FastDB owns C-Two provider installation, C-Two codegen, or C-Two typed client facades.

- [x] **Step 2: Retire or narrow C-Two provider architecture doc**

If `docs/c-two-provider-architecture.md` remains, mark it historical or superseded by C-Two's boundary redesign spec. Do not present it as current architecture.

- [x] **Step 3: Verify FastDB doc hygiene**

Run from `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`:

```bash
rg -n "install_c_two_provider|fdb codegen --c-two-ts|fastdb4py\\.c_two|C-Two provider|provider-owned portable payload ABI" README.md docs python/fastdb4py ts/fastdb4ts/src
git diff --check
```

Expected: any remaining matches are historical/superseded notes or generic "consumed by C-Two" references, not current core surfaces.

Task 4 verification on 2026-05-20:

```bash
uv run --locked python - <<'PY'
import fastdb4py, pathlib
print(pathlib.Path(fastdb4py.__file__).resolve())
PY
uv run pytest tests/python/test_import_boundary.py -q
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
rg -n "install_c_two_provider|fdb codegen --c-two-ts|fastdb4py\\.c_two|C-Two provider|provider-owned portable payload ABI" README.md docs python/fastdb4py ts/fastdb4ts/src
git diff --check
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=60
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
C2_RELAY_ANCHOR_ADDRESS= uv run --locked pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=180
uv run --locked python -m compileall -q sdk/python/src/c_two/fastdb sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py
git diff --check
```

Observed evidence: C-Two's uv environment resolves `fastdb4py` to `/Users/soku/Desktop/codespace/WorldInProgress/fastdb/python/fastdb4py/__init__.py`, confirming the local editable sibling repository is the test input instead of the stale PyPI package. The RED FastDB import-boundary test first failed because `fastdb4py` exposed and eagerly loaded `fastdb4py.c_two_provider`, `fastdb4py.c_two_call`, `fastdb4py.c_two_bridge`, `fastdb4py.codegen.c_two_ts`, and `--c-two-ts`; after cleanup, focused boundary tests reported `5 passed`, full FastDB Python tests reported `318 passed`, FastDB compileall passed, the focused FastDB doc hygiene scan returned no matches, and FastDB `git diff --check` passed. The old FastDB C-Two modules and tests were deleted or moved into C-Two-owned surfaces, `fastdb4py.__init__` now exports only generic FastDB names, `fastdb4py.codegen` only exports generic TypeScript schema codegen, and `fdb codegen` no longer advertises C-Two codegen. C-Two smoke verification reported `6 passed` for `test_fastdb_abi.py`, `12 passed in 27.41s` for grid FastDB bridge smoke, and `114 passed in 314.26s` for the full FastDB codec provider smoke after removing obsolete provider-installation idempotence tests. C-Two compileall and `git diff --check` passed. The C-Two source/test scan no longer finds direct imports from `fastdb4py.c_two*`; remaining matches are C-Two docs/spec/plan text that belongs to Phase 5 cleanup, plus the intentional negative boundary assertion in `test_fastdb_abi.py`.

### Checkpoint: Phase 4

- [x] FastDB no longer exposes C-Two-specific modules as core package surfaces.
- [x] FastDB generic Python and TypeScript tests pass.
- [x] C-Two owns all moved C-Two-specific test coverage.

## Phase 5: C-Two Docs And API Cleanup

### Task 5.1: Rewrite C-Two public docs around FastDB-first

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `docs/roadmap.md`
- Modify: `docs/roadmap.zh-CN.md`
- Modify: `docs/vision/cross-language-contract-codec-architecture.md`
- Modify: `sdk/python/src/c_two/providers/README.md`
- Modify: `docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`
- Modify: `docs/plans/2026-05-20-fastdb-first-closure-review-packet.md`

- [x] **Step 1: Replace provider-neutral portable language**

Rewrite public docs so the official statement is: FastDB call-db is the C-Two portable CRM payload ABI; Python pickle fallback is Python-only; Arrow/custom are examples, tests, or internal experiments.

- [x] **Step 2: Replace old workflow examples**

Change examples that say `fdb codegen --c-two-ts` is the main path to the C-Two-owned `c3 contract codegen typescript` workflow.

- [x] **Step 3: Keep runtime boundary precise**

Use this wording consistently: "C-Two contract layers depend on FastDB ABI; C-Two runtime route/relay/IPC/scheduler/lease layers do not parse FastDB storage internals."

- [x] **Step 4: Verify documentation scans**

Run:

```bash
rg -n "provider-neutral|fastdb-neutral|C-Two core remains FastDB-neutral|C-Two production code must remain fastdb-neutral|Payload codecs are enabled through providers|fdb codegen --c-two-ts|install_c_two_provider" README.md README.zh-CN.md docs sdk/python/src/c_two/providers/README.md
git diff --check
```

Expected: any remaining matches are in historical sections explicitly marked superseded, not current guidance.

Task 5.1 verification on 2026-05-20:

```bash
rg -n "provider-neutral|fastdb-neutral|C-Two core remains FastDB-neutral|C-Two production code must remain fastdb-neutral|Payload codecs are enabled through providers|fdb codegen --c-two-ts|install_c_two_provider" README.md README.zh-CN.md docs/roadmap.md docs/roadmap.zh-CN.md docs/vision/cross-language-contract-codec-architecture.md sdk/python/src/c_two/providers/README.md sdk/python/src/c_two/providers/arrow.py sdk/python/src/c_two/providers/custom.py
rg -n "from fastdb4py\\.c_two|import fastdb4py\\.c_two|install_c_two_provider|fdb codegen --c-two-ts|provider-neutral|fastdb-neutral" README.md README.zh-CN.md docs sdk/python/src sdk/python/tests examples --glob '!docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md' --glob '!docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md' --glob '!docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md'
rg -n "fastdb4py|from fastdb|import fastdb" core cli sdk/python/src/c_two/transport sdk/python/src/c_two/config --glob '*.rs' --glob '*.py'
git diff --check
```

Observed evidence: README and README.zh-CN now use `c3 contract codegen typescript --fastdb-schema/--fastdb-out` as the C-Two-owned FastDB helper workflow instead of `fdb codegen --c-two-ts`, and describe FastDB call-db as the portable CRM payload ABI while keeping Python pickle, Arrow, and custom providers out of strict portable export/codegen. `docs/roadmap.md` and `docs/roadmap.zh-CN.md` now summarize the current C-Two-owned FastDB ABI/codegen/bridge state instead of the old fastdb-neutral/provider-install story. `docs/vision/cross-language-contract-codec-architecture.md` is reduced to a superseded pointer to the boundary redesign. `docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md` is explicitly marked as historical migration evidence where it conflicts with the new boundary. The focused current-guidance scan over README, roadmap, vision, and providers docs returned no matches for the old provider-neutral/fastdb-neutral/install/codegen terms; the broader scan only found the intentional negative assertion in `test_fastdb_abi.py` plus explicitly excluded historical spec/plan text. The runtime-layer FastDB import scan over `core`, `cli`, `c_two.transport`, and `c_two.config` returned no matches. `git diff --check` passed.

### Task 5.2: Demote Arrow/custom providers

**Files:**
- Modify: `sdk/python/src/c_two/providers/README.md`
- Modify: `sdk/python/src/c_two/providers/arrow.py`
- Modify: `sdk/python/src/c_two/providers/custom.py`
- Modify: `sdk/python/tests/unit/test_arrow_provider.py`
- Modify: `sdk/python/tests/unit/test_custom_provider.py`
- Modify: `sdk/python/tests/integration/test_arrow_view_provider_smoke.py`
- Modify: `sdk/python/tests/integration/test_custom_codec_provider_smoke.py`

- [x] **Step 1: Clarify module status**

Update docstrings and README prose to state that Arrow/custom providers are examples or Python-only experiments, not the portable C-Two ABI.

- [x] **Step 2: Tighten portable export tests**

Ensure strict portable tests reject Arrow/custom as official cross-language ABI unless the test is explicitly checking legacy/example behavior.

- [x] **Step 3: Verify provider tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_arrow_provider.py sdk/python/tests/unit/test_custom_provider.py sdk/python/tests/integration/test_arrow_view_provider_smoke.py sdk/python/tests/integration/test_custom_codec_provider_smoke.py -q --timeout=60
git diff --check
```

Expected: tests pass with revised expectations that keep those providers out of the official portable path.

Task 5.2 verification on 2026-05-20:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_arrow_provider.py sdk/python/tests/unit/test_custom_provider.py sdk/python/tests/unit/test_codec_provider.py -q --timeout=60
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_arrow_view_provider_smoke.py sdk/python/tests/integration/test_custom_codec_provider_smoke.py -q --timeout=60
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=60
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_arrow_provider.py sdk/python/tests/unit/test_custom_provider.py sdk/python/tests/integration/test_arrow_view_provider_smoke.py sdk/python/tests/integration/test_custom_codec_provider_smoke.py -q --timeout=60
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py::test_grid_fastdb_export_feeds_fastdb_typescript_codegen -q --timeout=180
uv run python -m compileall -q sdk/python/src/c_two/crm/descriptor.py sdk/python/src/c_two/providers/arrow.py sdk/python/src/c_two/providers/custom.py sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_arrow_provider.py sdk/python/tests/unit/test_custom_provider.py sdk/python/tests/unit/test_codec_provider.py
git diff --check
```

Observed evidence: strict portable descriptor construction now rejects non-FastDB codec refs, while ordinary descriptors still allow Arrow/custom example providers and surface `non_fastdb_portable_codec` diagnostics. The focused unit provider suite reported `22 passed`; the integration Arrow/custom provider suite reported `3 passed`; the Phase 5 provider command reported `18 passed`; contract export plus FastDB ABI tests reported `18 passed`; the focused grid FastDB TypeScript codegen smoke reported `1 passed`; compileall and `git diff --check` passed.

### Checkpoint: Phase 5

- [x] C-Two public docs/API make FastDB-first explicit.
- [x] Arrow/custom/provider-neutral portable narratives are removed or downgraded.
- [x] Old closure packet is clearly superseded where it conflicts with the new boundary decision.

## Phase 6: Cross-Repo Verification And Review Loop

### Task 6.1: Full verification gate

**Files:**
- No planned source edits unless verification exposes failures.

- [x] **Step 1: Run focused C-Two FastDB ABI tests**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_fastdb_abi.py sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180
```

Expected: all focused FastDB ABI and bridge tests pass.

Observed evidence on 2026-05-20: the focused C-Two FastDB ABI and bridge group reported `132 passed in 339.50s (0:05:39)`.

- [x] **Step 2: Run full C-Two Python suite**

Run:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
```

Expected: full SDK suite passes.

Observed evidence on 2026-05-20: the first full Python run exposed three stale tests that still expected custom/Arrow codec refs to pass strict portable export. The affected tests were updated to assert ordinary descriptors keep custom/Arrow metadata while `export_contract_descriptor()` rejects non-FastDB codec refs, the focused remediation command reported `3 passed in 0.35s`, and the repeated full suite reported `1055 passed in 420.35s (0:07:00)`.

- [x] **Step 3: Run C-Two Rust suite**

Run:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
cargo test --manifest-path cli/Cargo.toml --tests
```

Expected: Rust core and CLI tests pass.

Observed evidence on 2026-05-20: `cargo test --manifest-path core/Cargo.toml --workspace` exited 0, including the strict FastDB TypeScript codegen tests in `c2-codegen`; `cargo test --manifest-path cli/Cargo.toml --tests` exited 0 with CLI contract command tests reporting `23 passed`.

- [x] **Step 4: Run c2-mem-ffi gates sequentially**

Run:

```bash
cargo test --manifest-path core/foundation/c2-mem-ffi/Cargo.toml
NODE=/Users/soku/.nvm/versions/node/v22.17.0/bin/node npm test --prefix core/foundation/c2-mem-ffi/bindings/typescript
NODE=/Users/soku/.nvm/versions/node/v22.17.0/bin/node npm run pack:check --prefix core/foundation/c2-mem-ffi/bindings/typescript
```

Expected: all pass; `pack:check` is not run concurrently with tests.

Observed evidence on 2026-05-20: `cargo test --manifest-path core/foundation/c2-mem-ffi/Cargo.toml` reported `19 passed`; TypeScript binding `npm test` reported `27` passing Node tests; `npm run pack:check` reported that the dry-run pack contained 6 runtime files and that a clean consumer install typechecked and exercised native request/response pool and handle lifecycle paths.

- [x] **Step 5: Run FastDB sibling suites**

Run from `/Users/soku/Desktop/codespace/WorldInProgress/fastdb`:

```bash
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
npm --prefix ts/fastdb4ts run build
npm --prefix ts/fastdb4ts run test:ts
git diff --check
```

Expected: FastDB generic suites pass after C-Two-specific cleanup.

Observed evidence on 2026-05-20: in the FastDB sibling repository, `uv run pytest tests/python -q` reported `318 passed in 1.47s`; `uv run python -m compileall -q python/fastdb4py tests/python` exited 0; `npm --prefix ts/fastdb4ts run build` exited 0; `npm --prefix ts/fastdb4ts run test:ts` reported `76` passing tests, including generic call-db public export tests; `git diff --check` exited 0.

- [x] **Step 6: Run boundary scans**

Run in C-Two:

```bash
rg -n "from fastdb4py\\.c_two|import fastdb4py\\.c_two|install_c_two_provider|fdb codegen --c-two-ts|provider-neutral|fastdb-neutral" README.md README.zh-CN.md docs sdk/python/src sdk/python/tests examples --glob '!docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md'
rg -n "fastdb4py|from fastdb|import fastdb" core cli sdk/python/src/c_two/transport sdk/python/src/c_two/config --glob '*.rs' --glob '*.py'
```

Expected: first scan has no current-guidance matches outside superseded historical notes; second scan returns no runtime-layer imports.

Run in FastDB:

```bash
rg -n "fastdb4py\\.c_two|install_c_two_provider|fdb codegen --c-two-ts|createFastdbC2|FastdbC2" README.md docs python/fastdb4py ts/fastdb4ts/src tests --glob '!docs/superpowers/plans/2026-05-18-c-two-call-db-codec.md'
```

Expected: no core package surfaces remain; historical plan matches are allowed only in explicitly historical files.

Observed evidence on 2026-05-20: the broad C-Two scan found only the intentional negative assertion in `sdk/python/tests/unit/test_fastdb_abi.py` plus current spec/plan records documenting the migration; the C-Two runtime-layer import scan over `core`, `cli`, `c_two.transport`, and `c_two.config` returned no matches; the broad FastDB scan found only negative import-boundary tests. Refined current-guidance scans excluding migration spec/plan files and negative import-boundary tests returned no matches in both repositories. C-Two `git diff --check` also exited 0.

### Task 6.2: Strict review and remediation checkpoint

**Files:**
- Modify only files implicated by review findings.

- [x] **Step 1: Review by requirement**

Audit the final work against every acceptance criterion in `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md`. Record each criterion as proven, incomplete, or contradicted in the final PR description or review note.

- [x] **Step 2: Remediate findings**

For each finding, add or update a focused test before changing implementation unless the finding is documentation-only.

- [x] **Step 3: Re-run affected and final gates**

Repeat the focused gate that exposed the finding and then repeat the Phase 6 full verification gate.

Requirement audit on 2026-05-20:

| Acceptance criterion | Status | Evidence |
| --- | --- | --- |
| C-Two public docs state that FastDB call-db is the portable CRM payload ABI. | Proven | README, README.zh-CN, roadmap, vision, and provider docs were rewritten in Phase 5; current-guidance scans for provider-neutral, fastdb-neutral, provider-install, and old `fdb codegen --c-two-ts` wording returned no matches. |
| Portable export and strict TypeScript codegen reject pickle and non-FastDB codec refs, except no-payload refs. | Proven | Descriptor tests, Arrow/custom/provider tests, C-Two full Python suite, `c2-codegen` strict codec tests, and CLI contract codegen tests passed after the stale Arrow/custom portable-export tests were corrected. |
| C-Two can generate and run a FastDB-backed TypeScript client path without requiring `fdb codegen --c-two-ts` as a separate primary workflow. | Proven | C-Two grid FastDB bridge/codegen smokes passed, CLI contract codegen tests passed, and boundary scans found no current workflow dependence on `fdb codegen --c-two-ts`. |
| C-Two Python users can author FastDB-backed CRM contracts without calling a provider installation function. | Proven | `test_fastdb_abi.py` verifies `c_two.fastdb` aliases and absence of `install_c_two_provider`; focused ABI tests and the full Python suite passed. |
| C-Two runtime layers still do not parse FastDB storage internals for route, relay, IPC, scheduler, lease, or shutdown decisions. | Proven | Runtime-layer FastDB import scan over `core`, `cli`, `sdk/python/src/c_two/transport`, and `sdk/python/src/c_two/config` returned no matches; all Rust runtime/transport tests passed. |
| FastDB repository no longer exposes C-Two-specific modules as core package surfaces. | Proven | FastDB import-boundary tests passed, refined FastDB scans found no C-Two-specific public runtime/codegen symbols outside negative tests, and FastDB Python/TypeScript suites passed. |
| FastDB generic runtime tests and C-Two end-to-end contract/transport tests are separated by responsibility. | Proven | FastDB repository runs generic Python/TS suites; C-Two repository owns FastDB ABI, bridge, C3 codegen, direct IPC, explicit relay, and held-view integration tests. |
| Existing verified behavior for scalar, `Array`, `Batch`, WSTR, BYTES, held views, bridge derivation, direct IPC, relay, and TypeScript allocator-backed paths remains covered after migration. | Proven | Focused C-Two FastDB ABI/bridge tests, the full C-Two Python suite, FastDB TS call-db tests covering WSTR/BYTES/retained views, and c2-mem-ffi Rust/TS/pack gates all passed. |

### Checkpoint: Complete

- [x] C-Two public docs/API clearly state FastDB call-db is the portable CRM payload ABI.
- [x] C-Two repository owns FastDB-backed CRM contract integration.
- [x] FastDB repository no longer exposes C-Two-specific modules as core package surfaces.
- [x] Python pickle fallback is only Python-only local/prototype behavior.
- [x] Arrow/custom/provider-neutral portable narrative is removed or downgraded.
- [x] Existing verified behaviors remain covered: scalar, `Array`, `Batch`, WSTR, BYTES, held views, bridge derivation, direct IPC, relay, and TypeScript allocator-backed path.
- [x] Full cross-repo verification passed.
- [x] Goal is not marked complete until this checklist is proven from current-state evidence.
