# Retired-memory observation lifetime

Implementation source: `ac4ca86fa1d298e11baf9ee5337e5e6084dcd82c`.

A completed transport shutdown does not mean every SDK result has finished becoming a held lease. A response can already be delivered and pause in FastDB decoding before Python calls `track_retained`. Old supported proxies can also remain producers. Consequently, zero usage at shutdown is not a sufficient reason to discard a tracker.

Retired observation follows actual owner lifetime. `BufferLeaseGuard` strongly owns its Rust metadata, so an outstanding lease remains observable after the session wrapper disappears. The tracker stores metadata only and contains no references back to guards or payloads. Retired bundles hold weak budget and tracker observations, which cannot keep a Runtime, cache, pool, callback or payload alive. They discard records only when the corresponding owners disappear, and deduplicate accounting domains and trackers by identity across bundles. A referenced old proxy or session may therefore leave a zero-valued record; returning physical charges and removing the last metadata record are different events.

The public observation API has no allocation or tracking authority. A budget observer exposes copied limits and snapshots without `reserve`. A lease observer exposes copied `stats()` and `sweep_retained()` values; upgrading it to the producer is private. A compile-fail test enforces that boundary. Python receives an opaque handoff and copied statistics, not native trackers or budgets.

Session replacement captures current own domains and earlier retired observations without consuming them. It constructs and adopts into the replacement before publishing the replacement session under the registry lock. A failed construction or adoption leaves the installed session intact. A retry captures current domains again, including ones created after the failure. There is no `retired_once` latch and no Python close-confirmation fence for observation pruning.

## Acceptance evidence

Host review rejected the earlier close-confirmed pruning and destructive capture designs. Real public API regressions demonstrated a readable late-held result missing from `hold_stats`, and a retained file backing disappearing from observation after a failed replacement followed by retry. Both now pass in `sdk/python/tests/integration/test_memory_budget_lifetime.py`.

Host additionally made `BufferLeaseObserver::upgrade` private and changed Core composition to consume copied snapshots. The external-access compile-fail test first failed because the public upgrade compiled, then passed after the fix. The final affected Rust checks passed 222 runtime tests and that interface test. Fourteen focused Python tests and all 622 Python unit tests passed against the rebuilt native extension. Duplicate Worker integration tests were removed in favor of the stronger Host-owned cases that also check complete reassembly capacity and actual backing release.

The observation-only read of a weak handle briefly retains metadata while copying a snapshot. It never reads payload bytes or takes over `MemPool` release authority. The receive path remains copy-backed; this work does not establish direct resource-time construction in SHM or zero-copy decoding. Complete Windows and cross-language acceptance remains part of the final task gate.
