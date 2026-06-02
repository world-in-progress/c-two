# FastDB-First CRM Surface Consolidation

## Status

Implemented and verified.

## Goal

Collapse the many observable FastDB-first CRM usage shapes into a small public model:

1. two CRM authoring modes: fully portable FastDB call-db or Python-only pickle fallback;
2. two resource binding modes: direct FDB signatures or explicit bridge;
3. one recommended C-Two-facing FastDB construction entry for resource-created outputs: `fdb.require(...)`;
4. three lifetime policies: default materialized call, client `cc.hold(...)`, and server `InputLifetime.BORROWED`;
5. internal-only performance strategies with diagnostics, not public authoring modes.

Each phase must finish with a review/remediation loop before the next phase starts.

## Non-Goals

- Do not move FastDB storage, backing, allocator, or direct-build semantics into C-Two.
- Do not reintroduce `@cc.transferable`, provider packages, or `@cc.transfer(buffer="hold")`.
- Do not claim resource-time direct SHM allocation unless tests prove allocation happens on C-Two-provided backing during the resource method.

## Phase Plan

### Phase 1: Contract Authoring

Public model: a method is either fully FastDB portable or Python-only. Mixed FastDB/Python payload methods can still run in Python, but diagnostics must call them out as outside the two public authoring modes.

Exit criteria:

- [x] strict export still accepts fully FastDB portable contracts;
- [x] strict export still rejects Python-only pickle fallback;
- [x] diagnostics explicitly warn for mixed FastDB/Python payload methods;
- [x] README and README.zh-CN present the two-mode authoring decision clearly;
- [x] phase review finds no required fixes.

### Phase 2: Resource Binding

Public model: resource implementations either use the same FDB signatures as the CRM or register an explicit bridge. Bridge usage is a binding concern, not a separate FastDB payload mode.

Exit criteria:

- [x] examples and docs describe direct FDB resource signatures and explicit bridges as the only resource binding choices;
- [x] borrowed input validation remains incompatible with `bridge.input`;
- [x] phase review finds no required fixes.

### Phase 3: Value Construction

Public model: C-Two-facing resource-created FastDB outputs should use `fdb.require(...)`. Plain `Batch.allocate(...)` / `Array.allocate(...)` remains a FastDB API but is not the C-Two fast-path authoring recommendation.

Exit criteria:

- [x] examples prefer `fdb.require(...)` for resource-created FastDB outputs where the size is known;
- [x] docs explain why plain allocate may repack or fallback in C-Two call-db output;
- [x] tests preserve expected fallback behavior for plain allocate without treating it as the recommended path;
- [x] phase review finds no required fixes.

### Phase 4: Lifetime Policy

Public model: default materialized call, client `cc.hold(...)`, and server `InputLifetime.BORROWED`. Unsafe NumPy/raw buffer access is an escape hatch inside these policies, not a separate application mode.

Exit criteria:

- [x] docs and examples use the three-policy model consistently;
- [x] held values remain logical CRM values, not call-db envelopes;
- [x] borrowed inputs remain opt-in and owner-invalidated after the call;
- [x] unsafe buffer/NumPy wording stays narrow and accurate;
- [x] phase review finds no required fixes.

### Phase 5: Performance Strategy

Public model: performance mechanisms are internal plans with observable diagnostics. Users should not pick between `direct-layer-splice`, exact export, final-backing direct build, prepared sink, or fallback repack as API modes.

Exit criteria:

- [x] benchmark names and descriptions distinguish workload categories instead of exposing internal strategies as user modes;
- [x] diagnostics expose direct/fallback reason where useful;
- [x] docs avoid claiming allocator/direct-SHM behavior that current tests do not prove;
- [x] phase review finds no required fixes.

## Verification Checklist

Run focused tests after each phase, then broader checks before completion:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_contract_export.py sdk/python/tests/unit/test_fastdb_abi.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/integration/test_fastdb_prepared_payload.py -q --timeout=30
cargo check --manifest-path sdk/python/native/Cargo.toml
git diff --check
```

Use broader Python SDK and benchmark checks before marking the overall goal complete.
