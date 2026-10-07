# Owner-backing budget enforcement in c2-mem

This slice implements the owner-creation charge half of the accepted [memory budget contract](memory-budget-contract.md) on top of the already accepted `c2-mem` `MemoryBudget` primitive (source base `fb3a731`). It covers `c2-config` canonical limits, the shared-budget `MemPool` constructor, reject-before-map charges at the three backing-creation seams with move-only reservations attached to the actual backing owners, and checked creation geometry so unsupported layouts are rejected before any mapping. It does not cover IPC policy, transport stats wiring, or reassembly admission; see "Honest scope" below.

## Corrections from independent review (this revision)

The first revision was reviewed and three blockers were fixed:

1. Buddy creation geometry was previously computed with unchecked arithmetic (`next_power_of_two`, unchecked header/bitmap/alignment additions), so a huge `segment_size` could wrap, map a multi-GiB region, and only then hit `BuddyAllocator::init`'s `u32` assertion; `MemPool::validate_config` also multiplied `2 * min_block_size` unchecked. Both are fixed with checked preflight (next section). This property is implemented here, not deferred to the policy worker.
2. `with_budget_guard` was public, took an optional reservation, and allowed replacement (which would silently drop a live charge). It is now crate-private, takes a non-optional `BudgetReservation`, and is single-shot by explicit invariant.
3. The report previously presented source field-drop ordering as if tests had proven OS-level cleanup cross-platform. The test evidence primarily proves accounting counters and pool containers; drop ordering is a source-level invariant (guard declared after the region/mapping/file field), and the OS-level evidence is limited to focused name/file probes on the platform the suites ran on. The evidence sections below state exactly what is and is not proven, and new focused OS probes were added. Windows was not executed locally.

A separate policy worker fixes `DedicatedSegment` checked alignment under a `required_region_size` entry point; this tree keeps its own single checked helper (`DedicatedSegment::required_shm_size`) as the only dedicated geometry formula — `create` and `open` both derive their mapping size from it, and the previous unchecked `page_align` duplicate was deleted — so integration converges by renaming/merging one helper rather than choosing between two formulas.

## API surface added

- `c2_config::MemoryBudgetLimits` (new `core/foundation/c2-config/src/memory.rs`, exported from `lib.rs`): plain-data canonical limits with defaults 8 GiB `shm_backing_budget_bytes`, 16 GiB `file_backing_budget_bytes`, 8 GiB `live_reassembly_budget_bytes`, plus `zeroed()` (every positive charge rejected; zero is never unlimited). The type is plain data only — no `c2-mem` type appears in `c2-config`, so the existing one-way `c2-mem → c2-config` dependency is preserved and no cycle is created.
- `c2_mem::MemoryBudget::from_limits(&MemoryBudgetLimits)`: one-way conversion living in `c2-mem`, next to the reservation primitive.
- `MemPool::new_with_prefix_and_budget(config, prefix, MemoryBudget)`: owner pool whose buddy, dedicated, and file creation all charge the injected shared budget. Clones of the same `MemoryBudget` share cells across pools.
- `MemPool::budget() -> Option<&MemoryBudget>`: accessor for snapshots. Owner pools always carry a context; `open_peer` pools return `None` because peer-opened mappings never acquire owner-creation charges.
- Existing owner constructors `MemPool::new` / `new_with_prefix` now create finite private contexts from the canonical defaults (previously they had no limit at all). This is the contract's deliberate behavior change: standalone pools are no longer unlimited, and each remains independent — there is no process-global shared state.
- `PoolConfig` fields are unchanged; callers inject a resolved context through the new constructor, leaving IPC policy resolution to the policy worker's slice.
- `BuddyAllocator::checked_layout(min_data_capacity, min_block) -> Option<BuddyLayout>` replaces the previous unchecked `required_shm_size`: the single owner of the buddy segment-geometry formula, returning a validated `BuddyLayout { data_size, total_size }` or `None` for unsupported layouts. Exported from `c2-mem`.
- `DedicatedSegment::required_shm_size(data_size) -> Option<usize>`: the single checked dedicated geometry formula (`page_align(header + payload)`, `isize`-span-bounded); `create`/`open` consume it and no duplicate unchecked formula remains.
- `BuddySegment::with_budget_guard`, `DedicatedSegment::with_budget_guard`, `SpillMapping::with_budget_guard`: crate-private, single-shot attachment of one move-only `BudgetReservation` to the backing owner. `BuddySegment::create`, `DedicatedSegment::create/open`, and `spill::create_file_spill` remain raw OS creators, uncharged by themselves.

## Checked creation geometry (preflight, before reserve and before map)

Unsupported layouts must fail as errors before the budget charge and before any OS region exists — never as a wrapped value that maps first and panics in `BuddyAllocator::init`. `BuddyAllocator::checked_layout` validates, with checked arithmetic at every step: a checked next-power-of-two for the data capacity, `min_block` a positive power of two no larger than the data region, the data capacity within the wire's `u32` buddy coordinates (so a 4 GiB data region is rejected, largest supported capacity 2^31), the header + per-level bitmap bytes, page alignment of the data offset, the final total within one addressable `isize` mapping span. With a validated power-of-two capacity and dividing `min_block`, the level-bitmap byte sum is bounded (about `data_size / 4`) and cannot itself overflow; the additions are checked regardless. The formula is unchanged for every previously working layout (proven against an independent reconstruction of the legacy formula and against `init`'s data-region recovery), and the header ABI is untouched.

Enforcement points: `BuddySegment::create` validates and consumes `layout.total_size` for the mapping; `MemPool::create_segment` runs the same pure helper before reserving, so the charged bytes are the same validated total that creation maps; `MemPool::validate_config` now computes `2 * min_block_size` with `checked_mul` (an overflowing doubling is a configuration error instead of a wrapped small bound); `MemPool::max_buddy_block_size` derives the capacity from the checked layout, so unsupported geometry returns 0 and ordinary allocation can try dedicated/file backing. Explicit prewarm reports the buddy geometry error before mapping; ordinary fallback need not report that error. Dedicated `create`/`open` and the pool's dedicated precheck all use the one `required_shm_size` helper, which additionally bounds the padded total to `isize::MAX`.

## Where charges happen and how they are released

All owner creation entry points (`ensure_ready`/`ensure_buddy_segments` prewarm, `alloc`, `alloc_handle`, `try_alloc_shm`) funnel into three private seams in `pool.rs`, and each seam validates geometry, reserves, then maps:

| Seam | Cell | Charged bytes | Guard owner |
| --- | --- | --- | --- |
| `create_segment` (buddy) | `Shm` | `BuddyLayout::total_size` from `checked_layout` — header and bitmap metadata included, identical to what `BuddySegment::create` maps | `BuddySegment` |
| `alloc_dedicated` | `Shm` | `DedicatedSegment::required_shm_size(size)` — checked page-aligned header + payload | `DedicatedSegment` |
| `alloc_file_spill` | `File` | requested backing length `size` (the file's `set_len`) | `SpillMapping` inside `MemHandle::FileSpill` |

Guard attachment is crate-private and single-shot: each guard field is set exactly once, immediately after a budgeted creation succeeds and before the owner is published into the pool, taking a non-optional `BudgetReservation`; a second attachment trips an explicit panic ("a backing carries at most one owner-creation reservation") rather than silently dropping the live charge, and there is no public way to clear, replace, or reset a guard. The guard field is declared after the region/mapping/file field in each owner struct, so Rust's declaration-order field drop returns the charge after the region is unmapped (and after the spill file handle closes). That drop ordering is a source-level invariant of this crate, not something the test suite executes on every OS.

Behavioral consequences, with the nature of each proof stated honestly:

- A rejected reservation creates no mapping and no file; the rejection happens before any OS call. Proven by counter assertions (used bytes stay zero) plus, for geometry rejections, direct OS name probes (below).
- If mapping/creation fails after a successful reservation, the guard drops on the error path and the charge is released; the cell's persisted `peak_bytes` is the visible trace that the charge existed (file-failure test).
- The charge is taken exactly once, at the seam, attached to the backing owner — never duplicated in both the pool entry and the segment, and never replaceable while live.
- Reusing blocks inside an existing buddy segment never charges again; only a new mapped segment does (counter proof).
- A dedicated entry that is logically freed but still awaiting peer `read_done`/GC keeps its entry, keeps its segment, and therefore keeps its charge until `gc_dedicated` actually removes the owner mapping — even though its active allocation count is zero (counter proof).
- File guards ride inside `SpillMapping`, so they survive `MemHandle` movement, `set_len` trims, and whole-pool drop, releasing only when the mapping itself drops (counter proof plus mapping readability after pool drop).
- `MemPool::destroy`/`Drop`, `gc_buddy` reclamation, and dedicated GC return charges through ordinary owner drops; no counter is ever reset and no failure path is exempted.
- Peer pools (`open_peer`) carry no budget and never charge: `ensure_peer_segment`/`ensure_peer_dedicated`/`open_dedicated_at` open existing backings without owner-creation charges, and generation/stale-reference plus remote-free logic is untouched.
- Guards hold only the accounting `Arc`, kind, and byte count — never a pool, Runtime, or callback — so they cannot create ownership cycles and safely outlive their creating pool.

## OS-level cleanup evidence (and its limits)

The tests primarily assert budget counters and pool containers. Two focused checks provide direct OS evidence on the platform the suites ran on (macOS, POSIX shared memory and unlinked spill files):

- `owner_backing_names_disappear_after_pool_teardown` derives the exact bounded buddy and dedicated backing names, drops the owner pool, and proves neither name resolves via `BuddySegment::open` / `DedicatedSegment::open` — the creator's teardown actually unmapped and unlinked the objects, and the budget returned to zero.
- `unsupported_buddy_geometry_rejects_before_any_mapping` derives the name segment 0 would have used and proves nothing is registered under it after rejection.
- File spill already had a direct OS check before this slice (`spill::tests::test_create_file_spill_and_readback` asserts the spill directory is empty after the mapping drops on Unix), and the held-mapping test reads the mapping's bytes after the pool is dropped.

Not proven here: Windows behavior (`DELETE_ON_CLOSE`, named mappings) was not executed locally — the Windows field-ordering and file-cleanup properties remain source-level reasoning plus the existing Windows source documentation; Linux `/dev/shm` visibility was not enumerated directly (macOS has no enumerable POSIX SHM directory). Cross-platform execution remains with the integration gates.

## Single fallback order

`BackingError` (private to `pool.rs`) separates `Budget` rejections from other creation errors so the fallback chain stays single and predictable:

- `alloc` / `alloc_buddy`: a budget-rejected buddy expansion falls through to a smaller dedicated backing (which may still fit when a full segment does not); other creation errors such as generation exhaustion keep their original abort semantics and error text.
- `alloc_handle`: rejected or failed buddy expansion now tries dedicated first and only then falls to file spill when SHM budgets are exhausted and file spill is permitted. Previously a failed expansion jumped straight to file spill; the extra dedicated attempt is the contract's intended order and only adds a fallback hop, never removes one.
- `try_alloc_shm` (SHM-only): unchanged semantics — it returns `try_alloc_shm: no SHM capacity for N bytes` when neither buddy nor dedicated can be created, so transport keeps choosing its existing checked chunk fallback; it never silently spills to file.
- When every eligible tier is exhausted the terminal error is the last cell's `BudgetError` message (`memory budget cell '<shm|file>' rejected a N byte reservation: ...`), which names the cell, the requested bytes, and the limit. Geometry rejections surface as their own `buddy backing geometry unsupported` / `dedicated backing geometry unsupported` errors before any charge. Public `String` error surfaces are unchanged for non-budget, non-geometry errors.
- `ensure_ready`/`ensure_buddy_segments` do not fall back: prewarm fails with the budget or geometry rejection because it must produce the advertised buddy segments specifically.

## Merge overlaps with the policy worker (noted, not resolved here)

- The policy worker's dedicated checked alignment lands under a `required_region_size` entry point; this tree has exactly one dedicated geometry formula (`DedicatedSegment::required_shm_size`, consumed by `create`, `open`, and the pool's precheck, with the old unchecked `page_align` deleted), so integration should converge on a single helper rather than carry both.
- The previously late post-creation `seg.size() > u32::MAX` check in `alloc_dedicated` was moved to a pre-creation precheck (same error text, now before charge and mapping).
- `PoolConfig` still has no `buddy_enabled` / `min_retained` / budget fields; resolved-context injection through the constructor remains the agreed seam. `BuddyAllocator::checked_layout` now owns buddy geometry validation, which the policy worker's buddy work should reuse rather than duplicate.
- `alloc_handle`'s expansion-failure hop to dedicated (before file) is a deliberate order change; if the policy worker also touches fallback order, this slice's tests pin the budget-driven cases.

## Tests

All tests live in `core/foundation/c2-mem` (`pool.rs` `budget_tests` / `geometry_and_guard_tests` modules, `budget.rs`, `alloc/buddy.rs`) plus `c2-config` `memory.rs`, using tiny geometries (8 KiB buddy segments, 4 KiB blocks) and fault injection (a regular file occupying the spill directory path); nothing approaches real machine memory, and the near-`usize::MAX`/u32-bound cases are pure preflight arithmetic or pre-map rejections with OS name probes.

Budget enforcement (`pool::budget_tests`, unchanged from the first revision except the helper now derives expected bytes from `checked_layout`):

- `zero_budget_rejects_before_any_mapping` — `ensure_ready` and `alloc` reject with the cell-named budget error, `segment_count()==0`, no dedicated entries, no spill directory created, rejection counters/bytes recorded.
- `charges_match_exact_backing_geometry` — buddy charge equals the validated `total_size` (metadata included); dedicated charge equals the checked page-aligned header+payload; file charge equals the requested length and returns on handle drop.
- `reused_buddy_blocks_charge_no_new_backing` — two 4 KiB blocks in one charged segment; free/reuse keeps `used`/`peak` at exactly one segment cost.
- `shared_budget_spans_owner_pools_and_private_budgets_do_not` — two owner pools share one budget and exhaust it together; `new`/`new_with_prefix` pools keep independent private accounting; `open_peer` returns `None`.
- `owner_constructors_carry_canonical_private_limits` — private contexts are exactly 8/16/8 GiB.
- `dedicated_pending_gc_stays_charged_until_reclaim` — creator free keeps the charge while `freed_at` is set and GC declines; `read_done` + `gc_dedicated` returns it.
- `file_creation_failure_releases_reservation` — spill creation failure errors with `file spill failed`; `used_bytes==0`, `peak_bytes==4096` proving rollback.
- `file_guard_survives_move_trim_and_pool_drop` — charge and mapping data persist through `set_len`, handle move, and pool drop; readable until the handle drops, then the charge returns.
- `all_eligible_tiers_exhausted_returns_clear_capacity_error` — tiny shm+file budgets: buddy, dedicated, and file all rejected with no backing created anywhere (including no spill directory); `try_alloc_shm` reports the capacity error for chunk fallback.
- `buddy_budget_short_but_dedicated_fits` — SHM limit below the buddy segment cost but at the dedicated cost: allocation lands in dedicated with exactly that charge.
- `shm_exhaustion_falls_to_file_when_allowed` — zero SHM, generous file: `alloc_handle` yields a file-spill handle; two SHM rejections recorded; file charge returns on drop.
- `teardown_returns_all_charges_and_live_backings_are_not_retired_early` — live buddy block blocks GC and keeps its charge; free+GC returns the trailing segment's charge; `destroy` returns even the retained last segment; dedicated pending-GC and an out-of-pool file handle both keep charges until actual release; final snapshot has zero `used` in all cells with peaks persisted.

Geometry and guard ownership (new in this revision):

- `alloc::buddy::tests::checked_layout_preserves_previously_valid_geometries` — for supported layouts (including the 256 MB default and a 1 GiB capacity), `total_size` equals an independent reconstruction of the legacy formula and `init` recovers the same data region; header ABI unchanged.
- `alloc::buddy::tests::checked_layout_rejects_unsupported_geometries_pure_preflight` — pure arithmetic rejections: non-pow2/zero/oversized `min_block`, 4 GiB data capacity (u32 bound), next-power-of-two overflow near `usize::MAX`; 2^31 capacity still validates.
- `pool::geometry_and_guard_tests::validate_config_rejects_min_block_doubling_overflow` — `2 * min_block_size` overflow is a configuration error, not a wrapped bound.
- `pool::geometry_and_guard_tests::unsupported_buddy_geometry_rejects_before_any_mapping` — for `usize::MAX`, `2^63+1`, and `4 GiB+12345` segment sizes (validation accepts; the seam preflights): `ensure_ready` errors with `buddy backing geometry unsupported`, no segment exists, no charge and no budget rejection was recorded, and the derived backing name resolves to no SHM region.
- `pool::geometry_and_guard_tests::budget_guard_attachment_is_single_shot` — first attachment holds the charge; a second attachment panics with "at most one owner-creation reservation" instead of dropping it.
- `pool::geometry_and_guard_tests::owner_backing_names_disappear_after_pool_teardown` — direct OS probe: neither the buddy nor the dedicated backing name resolves after owner pool teardown, and the budget returns to zero.

Also: `budget::tests::from_limits_builds_the_canonical_cells`, and `c2-config memory::tests::canonical_defaults_are_the_accepted_finite_limits` / `zeroed_limits_are_finite_and_rejective_not_unlimited`.

Verification command (the mandated gate, run from the repository root):

```bash
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/c2-memory-backing-build cargo test --manifest-path core/Cargo.toml -p c2-config -p c2-mem
```

Result at the time of writing: `c2-config` 69 passed, `c2-mem` 127 passed (12 budget tests + 6 geometry/guard tests among them), 0 failed. `cargo check` over `c2-wire`, `c2-ipc`, `c2-server`, `c2-http`, `c2-local`, and `c2-core` also passes with the same target directory; a full `--workspace` check fails only in the pre-existing `fastdb-sys` build script, which requires the checkout-only `FASTDB_PAYLOAD_LINK_MODE=source` environment and is unrelated to this slice. Suites ran on macOS (darwin, arm64); Windows was not executed locally.

## Honest scope

Not implemented in this slice, by task assignment: reassembly admission control and guard transfer through `RequestData`/`ResponseData` (next slice), transport-level stats/SDK projections, IPC policy resolution and `PoolConfig` fields, Runtime client-cache ownership and shutdown wiring, and any relay/HTTP behavior. Budgets scope to C-Two-owned backing in controlled owner pools only: raw public OS creators (`BuddySegment::create`, `DedicatedSegment::create`, `create_file_spill`) remain uncharged by design (they do validate geometry), peer-opened mappings are uncharged by design, and the limits say nothing about SDK heaps, HTTP buffers, or process RSS. `MemPool`-level snapshots come from `budget().snapshot()`; no second Python/Rust counter exists. OS-cleanup evidence is macOS-local; Windows cleanup properties are source-level until integration gates run them.

## Host acceptance

The Host inspected the complete `fb3a731..116fc60` diff, obtained an independent second review after rejecting the first revision, and reran `c2-config` (69 passed) plus `c2-mem` (127 passed). The Host clarified unsupported-buddy fallback wording in code and this report. This accepts the backing layer only; the unimplemented integration work listed above remains pending.
