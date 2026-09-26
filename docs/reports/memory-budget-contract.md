# Transport memory budget contract

This is the target contract for the approved [memory plan](../plans/2026-09-26-memory-policy.md). It incorporates the Buddy design review and Host corrections; it does not claim the implementation already exists. The source baseline is `8777e2dd157942fe6f3cc90389763af35f5056bd`.

## Scope and ownership

- A Core `Runtime` owns its outgoing IPC client cache and one shared memory budget. Clones of that Runtime share the same cache and budget. Independent Runtimes have independent limits. Every request and reassembly pool for connections in that cache receives the same budget object.
- A `Server` owns a separate budget shared by its response and reassembly pools. The server and client directions keep separate payload storage.
- Standalone lower-level IPC clients receive an explicit context or create a finite private one from their resolved config. Relay connection pools should share a context within their owning pool; no new process-global first-config-wins state is allowed.
- These limits cover C-Two-owned IPC backing and live reassembly. They do not limit Python/FastDB heap, HTTP buffers, receiver-opened peer mappings, or whole-process RSS. Statistics must name the scope and never present the three cells' sum as physical RAM usage.

`c2-config` owns canonical defaults and validation. `c2-mem` owns the reservation primitive. A budget/context constructor on `MemPool` accepts the resolved limits or shared context; do not put an `Arc<c2_mem::MemoryBudget>` inside `c2-config::PoolConfig`, which would introduce a dependency cycle.

## Three finite byte limits

Expose these base IPC override keys through Rust, native and typed SDK projection:

| Key | Default | Scope of charge |
| --- | --- | --- |
| `shm_backing_budget_bytes` | 8 GiB | Owner-created buddy and dedicated mapped backing, including header/alignment |
| `file_backing_budget_bytes` | 16 GiB | Owner-created file backing length |
| `live_reassembly_budget_bytes` | 8 GiB | Allocated capacity of incomplete and completed-but-retained chunk assemblies |

Zero means no positive allocation in that cell; it is not unlimited. Existing per-message, chunk-count and segment-count limits still apply. The finite defaults are a deliberate behavior change for workloads that previously exceeded aggregate limits. No universal claim that all scientific workloads fit these defaults is made. Role overrides/environment resolution use the existing Rust precedence and freeze rules.

The reassembly limit is separate from the existing single-message `max_reassembly_bytes`; it is not a limit on all resource values or inline heap allocations.

## Reservation primitive

Implement `MemoryBudget::new(shm_limit, file_limit, reassembly_limit)`, `reserve(BudgetKind, bytes)` and `snapshot()`. `BudgetKind` identifies `Shm`, `File`, or `Reassembly`.

Each cell tracks `limit_bytes`, `used_bytes`, `peak_bytes`, `rejected_allocations`, and `rejected_bytes`. A short mutex-protected update with checked arithmetic is sufficient; allocation, I/O and payload operations happen outside that lock. An overflow or capacity rejection occurs before allocation. Rejection counters may saturate; live usage must never wrap or silently underflow.

A non-cloneable `BudgetReservation` owns exactly one charge and returns it on Drop. Moving a guard transfers ownership. It holds only accounting state, not a pool, mapping, Runtime or callback, avoiding ownership cycles. There is no reset-usage API. The accounting Arc remains alive while any guard remains.

## Backing lifetime and fallbacks

- Reserve the actual buddy backing size before creating a segment, including allocator metadata. Reusing blocks in an already charged segment does not charge that segment again.
- Reserve aligned dedicated backing before creation and enforce representability before mapping. A dedicated entry waiting for peer `read_done`/GC remains charged until its owner mapping is actually dropped, even when logically freed.
- Reserve file backing before creating/mapping the file. The reservation is owned alongside the actual file mapping and must drop after mapping/file cleanup, including error and unwind paths.
- Peer-opened mappings do not acquire owner-creation charges. Retirement and exact generation validation remain unchanged.
- A failed buddy expansion can still try dedicated; SHM exhaustion can still use file backing for local reassembly when permitted. IPC falls back to existing checked chunk transport when it cannot send a shared reference. No alternate transport stack is introduced.
- File-backed reassembly remains file-backed after the earlier promotion-removal change. No speculative promotion policy is introduced.

`MemPool::free_at` / `release_handle` and backing owners remain release authorities. Accounting never copies or reads payload bytes and never independently frees memory. Reserve at the final creation seam so all allocation entry points share enforcement, including prewarm.

## Reassembly guard transfer

Reserve `total_chunks * chunk_size` with checked multiplication before allocating an assembly. Hold that full capacity charge until storage release, even when `finish()` trims logical length. The registry's active counters may fall at finish; the budget charge must not.

Use one guard carrier from assembler through finished result into `RequestData::Handle` or `ResponseData::Handle`. `RequestLease`/`ResponseLease` already own those values, and `HeldResponse` already owns its ResponseLease; do not add duplicate guards at every layer. Updating a Rust enum/owner field does not change the 15-byte wire BuddyPayload or C ABI. Audit all native transfers and exhaustive matches.

Budget-return ordering must be explicit where a handle is destructured: free/drop backing first, then return its charge. Cancellation, incomplete finish, duplicate insert, malformed chunks, timeout, copy failure and abandoned replies need the same ownership reasoning. FastDB checked-owner invalidation continues before transport lease release.

## Client-cache lifetime correction

The existing global `OnceLock<ClientPool>` is not destroyed at process exit; POSIX SHM names are not automatically unlinked by that fact. Move cache ownership to the Runtime. Avoid holding `RuntimeState`'s mutex across connection I/O, shutdown waits or callbacks: clone a cache/context Arc under the lock, then operate outside it.

Shutdown must detach and close this Runtime's cache entries through shared client ownership and a bounded receive-task barrier. It must not merely skip entries because an application Client still holds an Arc. Closing a connection and dropping a retained response backing are separate: existing ResponseLease pool Arcs keep held data valid, and their budget guards remain charged. Do not zero accounting, force-free held buffers, or reset a global cache that another Runtime uses.

Drop/eviction must not leave receive/maintenance tasks holding pool Arcs forever. Provide explicit bounded shutdown for the supported Runtime lifecycle; check Drop cleanup without claiming Rust statics run destructors at process exit. A restart/new acquire must preserve stale-client identity fencing and must not let a late old-client release decrement a replacement entry.

## Errors, control traffic and statistics

Use the existing structured capacity/unavailability conventions (`ResourceUnavailable`) with a useful budget cell/size message. Reject immediately rather than adding cancellation-sensitive waits in this slice. A receiver must actually return a correlated error when admission fails, not only log and leave the sender waiting.

Control frames must remain usable when data budgets are exhausted. Keep post-dispatch failure/retry semantics unchanged; only source-proven pre-dispatch failures may retain that classification.

Expose consistent scope-labelled snapshots through Core/Server and native SDK projection. The SDK must not own a second counter or resolver. Report backing/retained bytes separately from OS RSS and correct the existing misleading fragmentation metric name or definition.

## Acceptance

1. Concurrent reserve attempts never exceed finite limits; overflow, zero limits, guard movement, last-owner drop and unwind return accounting correctly.
2. Failed reserve creates no mapping/file. Buddy charge uses actual backing cost; reuse adds no backing charge; dedicated pending-GC entries stay charged.
3. Assembly finish/trim and held results keep capacity charged; every failure/cancellation/release path returns it exactly once after backing cleanup.
4. Two outgoing connections in one Runtime share limits; independent Runtimes do not silently share config. Shutting down one leaves the other callable. Held file-backed responses remain valid across connection shutdown and keep file/reassembly charges until release.
5. Small limits force the expected buddy/dedicated/chunk/file fallbacks. Exhausting all eligible tiers yields an explicit error while ping/shutdown still work.
6. Unix and Windows tests check actual owner mapping/file cleanup and existing stale-generation rejection. Do not infer OS cleanup solely from registry counters.

## Review provenance

Buddy design goal `64c91424-db52-448e-8b77-e3b018c0d399` first delivered `6ed3e20`, rejected by the Host for unlimited client budgets, process-global config coupling, incorrect static-drop claims and premature accounting release. Corrected source `c54182e` / artifact `ac98f8ba-0473-48cd-9368-9e9ee8fbf79b` was then reviewed. The Host further resolved active-client shutdown, avoided locking RuntimeState during I/O, made reassembly-only scope explicit, removed unproven workload guarantees and nonexistent config-type references, and specified a single guard carrier. This accepted design is not an implementation/test result.
