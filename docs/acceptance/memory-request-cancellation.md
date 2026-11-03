# Request ownership and bounded cancellation cleanup

C-Two now keeps the exact allocating pool and one release authority with each request block. This closes the ownership gaps found while integrating the shared memory-budget context: dropping a preallocated block, replacing a client pool, cancelling a send, or abandoning a reply wait must not free through a different pool, strand a pending waiter, or refund memory while a peer still uses it.

## Ownership and publication

`RequestReleaseState` owns the allocation coordinates, its pool, an atomic `Armed → Dispatched → Released` state, and an optional dedicated-retention permit. An unpublished block can be freed locally. Dispatch rejects a released or already-dispatched block before writing any bytes, and attaches the release authority to the pending entry before publication.

Host review found an additional race between the dispatch CAS and permit installation. The final implementation holds the permit mutex across the phase transition and installation; release takes that same mutex before changing the phase. Every published dedicated request therefore has exactly one retention permit when any terminal observer settles it. There is no job without a permit.

After dispatch, buddy allocations remain subject to the peer's allocator release. A failed or partial write does not prove that the peer never observed the coordinates. Dedicated allocations are locally marked freed but retain their backing and budget charge until peer `read_done` or the existing configured crash-timeout policy allows retirement.

`SendGuard` covers both sending and waiting for a reply. Cancelling before publication removes the pending entry and releases the armed allocation. Cancelling a partial write aborts the connection so another writer cannot append to an incomplete frame. Cancelling after a complete send removes that call's waiter while leaving the connection usable. A late or unclaimed response is released through its exact `ResponseLease` owner.

## Dedicated retirement

One shared executor serves at most two native threads and admits at most 256 dedicated-retention permits per process. Permits cover published requests and their later queued or in-flight retirement; admission occurs before publication. Capacity exhaustion or worker-creation failure rejects an unpublished call without dropping already-retained backing.

A retirement job keeps the actual owner pool and its charge alive independently of the request block, client slot, or client itself. The permit is returned after retirement. Idle workers exit only when no permit remains, so an admitted request is not handed to an abandoned queue.

The executor uses the existing dedicated crash-timeout policy. Its observation latency depends on polling and acquiring the pool lock; a pathological lock holder can delay cleanup. This is a bound on retained work and allocation, not a hard real-time shutdown deadline.

## Verification

The implementation and its tests were reviewed independently of Worker completion. Host corrected the dispatch/permit handoff race before integration. Linux and Windows CI then exposed two fixture defects: fault-injection guards named different static locks, and cancellation scenarios inherited a machine-dependent worker count despite stalling six or four callbacks. Those fixtures now share the intended injection lock and declare the required execution capacity while ordinary scenarios remain parallel.

Host also rejected a pool-scoped observer that counted only queued jobs and a later test that read the return flag and count separately. The final test-only observer covers queued and worker-popped jobs through weak references; GPT-6 Sol corrected the observation race with a snapshot under the executor lock and an assertion at the entry-removal transition. See [the focused review](memory-linux-cancellation-tests.md).

Host verification in the integrated tree recorded:

- The deterministic in-flight retention regression passed; the full `c2-ipc` target passed all 129 tests, with no ignored tests.
- Rebuilt c3 passed the Rust/Python portable tests (25 tests) and generated TypeScript real-call tests (14 tests). Strict validators accepted the exact 18 and 12 receipt rows.
- Both matrices reference the same c3 bytes: SHA-256 `6cdc59752012aced8ff4faecce4c09f44db479d613b17f80d1ea2029250f4938`.

Commands, exit codes, input diff and raw logs are retained under `/tmp/c2-combined-final-0929/`. The native permission errors inside the Sol Buddy sandbox are recorded separately and are not counted as successful runtime verification. Windows and Linux hosted results must be verified on the pushed source before claiming overall completion.

## Remaining boundaries

A dispatched buddy block with an uncertain delivery outcome is not locally freed; it remains charged until peer release or owner destruction. An explicit request-consumption acknowledgement could narrow that interval in future work.

Dedicated retention continues to use the existing configurable crash-timeout compromise. Partial-write cancellation closes the whole affected connection. Neither behavior is a new guarantee of retry after dispatch.

An earlier local investigation observed a chunked request failing to dispatch promptly when it shared a connection with a stalled inline call. That observation was not root-caused and is not claimed as fixed here. The cancellation tests exercise each sender on its own connection; cross-request chunk scheduling remains a separate follow-up. A separate loaded-run close deadline timeout is recorded in the focused review without an unproven cause or fix.
