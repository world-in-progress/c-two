# FastDB-First Closure Review Packet

> **Status:** Superseded for integration-order purposes by the FastDB-first contract boundary redesign
> **Date:** 2026-05-20
> **Related plan:** `docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`

## Superseded Integration Direction

The verification evidence and residual-risk notes in this packet remain useful as a snapshot of the bounded fastdb-first implementation. The proposed integration order below is superseded by `docs/superpowers/specs/2026-05-20-c-two-fastdb-first-contract-boundary-redesign.md` and `docs/superpowers/plans/2026-05-20-c-two-fastdb-first-boundary-redesign.md`.

The new direction is not "FastDB PR first, then C-Two PR." FastDB call-db is now treated as C-Two's first-class portable CRM payload ABI, so C-Two must own the FastDB-backed contract glue, planner, bridge, codegen, typed facade, and cross-transport verification. FastDB should keep or expose only generic schema, storage, serialization, view, owned-buffer, and runtime capabilities. The old FastDB-owned C-Two provider/codegen/bridge work is migration material, not a merge-ready endpoint.

## Closure Decision

The current goal should close at the bounded fastdb-first CRM ABI and bridge scope instead of continuing the micro-phase loop into Implementation Order item 91. The completed scope is large enough to review and integrate as two coordinated repository changes: FastDB first, then C-Two against that FastDB sidecar contract.

Item 91 remains real follow-up work, but it is no longer a blocker for this closure packet. It should be discussed as a separate implementation stream with its own acceptance criteria after the FastDB and C-Two integration order is agreed.

## Bounded Scope

- FastDB owns call-db schema planning, call-db encode/decode, bridge derivation, Python provider surfaces, TypeScript generated helper/runtime surfaces, and FastDB-owned payload allocation.
- C-Two owns opaque payload ABI descriptor/export behavior around FastDB call-db `PayloadAbiRef` values, portable diagnostics, CRM conformance guardrails, bridged resource registration/execution, generated TypeScript transport surfaces, Rust-owned memory/SHM substrate, and grid-level integration examples/tests.
- C-Two production code remains FastDB-neutral. FastDB appears only in tests, examples, docs, and provider-owned sibling-repo code.
- The bridge is intentionally applied before resource execution and before response serialization so thread-local, direct IPC, and explicit HTTP relay flows share the same semantic path.
- Normal calls still return materialized ergonomic values, while held calls can expose retained FastDB views when the provider can safely build them from retained payloads.

## Review Findings

- **Blocking findings:** None remain from the fresh verification gate in this packet.
- **High residual risk:** The diff is too large for a single mixed-repository PR. It should be split FastDB-first, then C-Two, because C-Two TypeScript/client proof code depends on FastDB sidecar schemas and runtime helpers, while C-Two must stay FastDB-neutral.
- **Medium residual risk:** Several C-Two files are currently untracked, including the new `c2-mem-ffi` package and grid bridge example/test files. Those files must be intentionally staged in the C-Two PR; otherwise the diff will look incomplete even though local tests pass.
- **Medium residual risk:** The first `npm run pack:check` run in `core/foundation/c2-mem-ffi/bindings/typescript` failed only when it raced a concurrent `npm test` that intentionally creates `dist/native/unexpected-debug.map` as a negative pack-check fixture. Sequential rerun passed; keep pack verification sequential in CI or isolate test scratch output.
- **Low residual risk:** The first FastDB TypeScript test run failed on a WSTR/bytes retained-output case against stale generated/build output. After explicit `npm --prefix ts/fastdb4ts run build`, the focused test and full TypeScript suite passed. Keep the build step explicit in the FastDB PR checklist.

## Verification Evidence

### C-Two

- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_contract_infer.py sdk/python/tests/unit/test_codec_provider.py sdk/python/tests/unit/test_transferable.py sdk/python/tests/unit/test_resource_conformance.py sdk/python/tests/unit/test_custom_provider.py sdk/python/tests/unit/test_arrow_provider.py -q --timeout=30` passed: 145 tests.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py -q --timeout=120` passed: 116 tests.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py -q --timeout=180` passed: 12 tests.
- `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30` passed: 1051 tests.
- `cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml --test typescript` passed: 11 tests.
- `cargo test --manifest-path core/foundation/c2-mem-ffi/Cargo.toml` passed: 19 tests plus doc-test collection.
- `cargo test --manifest-path core/Cargo.toml --workspace` passed across `c2-codegen`, `c2-config`, `c2-contract`, `c2-error`, `c2-http`, `c2-ipc`, `c2-mem`, `c2-mem-ffi`, `c2-runtime`, `c2-server`, and `c2-wire`.
- `uv run python -m compileall -q sdk/python/src/c_two sdk/python/tests/integration/test_fastdb_codec_provider_smoke.py sdk/python/tests/integration/test_grid_fastdb_bridge_smoke.py sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_contract_infer.py` passed.
- `NODE=/Users/soku/.nvm/versions/node/v22.17.0/bin/node npm test` under `core/foundation/c2-mem-ffi/bindings/typescript` passed: 27 tests.
- `NODE=/Users/soku/.nvm/versions/node/v22.17.0/bin/node npm run pack:check` under `core/foundation/c2-mem-ffi/bindings/typescript` passed sequentially.
- `rg -n "fastdb4py|from fastdb|import fastdb" sdk/python/src core --glob '*.py' --glob '*.rs'` returned no production C-Two imports.
- `git diff --check` passed.

### FastDB

- `uv run pytest tests/python -q` passed: 654 tests.
- `uv run python -m compileall -q python/fastdb4py tests/python` passed.
- `npm --prefix ts/fastdb4ts run build` passed before the final TypeScript rerun.
- `node --test --test-name-pattern "WSTR and bytes batch" tests/ts/test_c_two_runtime.mjs` passed: 1 focused test.
- `npm --prefix ts/fastdb4ts run test:ts` passed: 74 tests.
- `git diff --check` passed.

## Integration Order

This section is historical. It records the old bounded-closure proposal and must not be used as the current migration order after the boundary redesign.

1. Land the FastDB PR first with the provider, call-db planner/schema/runtime, bridge derivation, TypeScript helper/runtime changes, docs, and FastDB tests. Proposed title: `Add C-Two call-db provider and bridge runtime`.
2. Land the C-Two PR second with FastDB-first payload ABI descriptors/diagnostics, bridge registration/execution, generated TypeScript transports, `c2-mem-ffi`, grid examples, docs, and C-Two tests. Proposed title: `Add fastdb-first CRM ABI bridge and TypeScript transport proof`.
3. The C-Two PR should reference the FastDB PR or exact FastDB commit used for verification. Do not merge C-Two first unless CI pins or vendors the exact FastDB commit that provides the sidecar schemas and runtime helpers.
4. The cross-repo final gate should run C-Two with the sibling FastDB checkout at the reviewed FastDB commit, then rerun the grid FastDB bridge smoke and FastDB TypeScript runtime tests.

## PR Readiness

### FastDB PR Scope

- Include `fastdb4py.c_two_provider`, `fastdb4py.c_two_call`, `fastdb4py.c_two_bridge`, schema/codegen/runtime updates, import-boundary tests, Python call-db/bridge tests, TypeScript runtime tests, and docs.
- Keep the FastDB PR responsible for all FastDB type, table, object-graph, array, batch, WSTR, bytes, and payload allocation semantics.
- Required gates: `uv run pytest tests/python -q`, `uv run python -m compileall -q python/fastdb4py tests/python`, `npm --prefix ts/fastdb4ts run build`, `npm --prefix ts/fastdb4ts run test:ts`, and `git diff --check`.

### C-Two PR Scope

- Include FastDB-first payload ABI descriptor/export behavior, bridge/conformance/registry/server integration, Rust `c2-codegen`, Rust `c2-mem-ffi`, TypeScript generated transport tests, grid FastDB examples/tests, roadmap/vision/docs, and the plan/review packet.
- Keep C-Two runtime route, relay, IPC, scheduler, lease, and lifecycle code free of FastDB storage parsing, while C-Two owns the FastDB-backed payload ABI facade.
- Required gates: the focused C-Two CRM/provider suite, `test_fastdb_codec_provider_smoke.py`, `test_grid_fastdb_bridge_smoke.py`, full `sdk/python/tests/`, `cargo test --manifest-path core/Cargo.toml --workspace`, `cargo test --manifest-path core/foundation/c2-mem-ffi/Cargo.toml`, `cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml --test typescript`, c2-mem-ffi TypeScript `npm test`, c2-mem-ffi `npm run pack:check`, production FastDB import scan, compileall, and `git diff --check`.

## Item 91 Backlog

- Broader real IPC TypeScript coverage and publishable TypeScript SDK packaging beyond the current initial Node/POSIX direct IPC proof.
- Bun, Deno, or equivalent native/FFI loaders over the same Rust-owned memory substrate.
- TypeScript runtime coverage for non-native variable-length list layouts after FastDB defines concrete storage/runtime semantics.
- Further automatic bridge synthesis driven by real resource shapes, not speculative aliases for already-covered bridge families.
- Lower-level fixed-table or heap-backed writes only if benchmark evidence beats the current mutable bulk append path for C-Two call-db shapes.

## Final Handling Notes

- No staging, commit, or push has been performed as part of this packet.
- The next discussion should start with whether to prepare FastDB PR slicing or to scope item 91 as a new plan, not by continuing the current micro-phase loop.
