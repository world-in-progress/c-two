# Finite reservation primitive

Implements the standalone accounting primitive in the [budget contract](memory-budget-contract.md). This commit does not yet enforce limits on real allocations; wiring and transport acceptance are separate steps.

`MemoryBudget` shares three fixed byte-limit cells through an Arc. A short standard-library mutex serializes checked admission and consistent snapshots. `BudgetReservation` is move-only and returns its charge on Drop, including unwind; a guard can outlive every MemoryBudget handle. Cells report limit/current/peak bytes and saturating rejection metrics. Zero rejects positive charges; there is no reset or implicit unlimited default.

Buddy artifact `a3187a32-47de-4f62-af38-006f773da04f`, source `8e5eee1f2162c3d4d8d62302382d7421a0d70402`, was independently reviewed. The Host corrected release arithmetic from debug-only assertion plus saturating subtraction to checked subtraction, removed silent mutex-poison recovery, marked the reservation `must_use`, and added the missing last-budget-owner lifecycle test. The crate keeps its existing dependency set; no extra mutex library is added.

Executed on the integrated source, using a target directory dedicated to the integration checkout:

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/c2-memory-integration-build cargo test --manifest-path core/Cargo.toml -p c2-mem budget::
# 8 passed, 0 failed
```

Tests cover independent cells, zero limits, overflow, move/drop, shared clones, guard-only state retention, unwind, and barrier-controlled concurrent overcommit. Live capacity and peak never exceed their caps in the concurrency test, and all successful guards return usage to zero. No payload is allocated or copied by this module, and no RSS improvement is claimed.

Validation environment note: sharing one Cargo target directory across concurrently active worktrees produced mismatched cached dependency metadata during Host checks. Integration checks now use `/tmp/c2-memory-integration-build`; Buddy worktrees keep separate target directories. The final passing command above uses that isolated directory.
