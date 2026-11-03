# Runtime-owned IPC client cache: ownership and lifecycle slice

Date: 2026-09-26 (second revision: detached-ownership atomicity, unconfirmed-record retention, Python facade surfacing)
Scope: client-cache ownership/lifecycle prerequisite from `docs/reports/memory-budget-contract.md` ("Move cache ownership to the Runtime" and the four independent-review lifecycle cases). This slice is deliberately **not** byte-budget wiring; budget primitives and buddy/allocator policy belong to the other workers.

## Second-review corrections (this revision)

The Host re-ran the corrected artifact's Core/IPC suites (all green) and found three narrow remaining problems; all are corrected here:

A. **Detachment and close-accounting were not one atomic step.** `discard_if_same` removed the entry under the pool-state lock and only registered the pending close afterwards (`close_accounted` incremented after unlock); the acquire sweep/stale batches had the same gap. A `close_all` scheduled in that window observed an empty map with zero pending barriers and could claim a false success. Correction: the coordinator now owns an **exact retired-record registry** (`CloseTxnState.retired: HashMap<ticket, RetiredClose>`); every detachment (discard, expiry sweep, stale replacement, race loser, straddled-connect rejection, drain children) registers its record via `retire_locked` **in the same critical section that removes the entry** (state → txn lock order, no I/O under either lock). Proof: the discard-vs-shutdown test's readiness hook now fires at registration — the deterministic detach→I/O window — and `close_all` started there reports an error instead of success.

B. **Unconfirmed closes disappeared with the counter.** The integer in-flight counter could not keep ownership of an unconfirmed client for retry. Correction: records are removed **only after a confirmed close**; an unconfirmed barrier flips its record to `UnconfirmedIdle`, keeping the client `Arc` reachable. `close_all` (i) waits for `Closing` records before starting, (ii) registers its drain children atomically with emptying the map, (iii) retries all `UnconfirmedIdle` records within the same aggregate absolute deadline, and (iv) at finalize reports every still-registered record as unconfirmed — a client is only claimed stopped once its exact record is removed after confirmation. Proof: new `prior_unconfirmed_drained_client_is_retried_and_reported_by_later_close_all` — a 200 ms drain against a blocked child reports it unconfirmed and leaves the record registered (`retired_records_for_test() == 1`); after the blocker clears, a later `close_all` retries the record, confirms it, and the registry empties. The discard/sweep/loser work is included in drain accounting through the same registry.

C. **Python facade now surfaces unconfirmed cleanup (scope granted).** `RuntimeRegistry.shutdown` in `sdk/python/src/c_two/transport/registry.py` logs a warning for `ipc_client_close_error` and `runtime_barrier_error` from the native shutdown outcome (same best-effort policy as the existing relay-error logging; no return-type change, no new public mutable inspection API). Added `test_shutdown_warns_when_native_cleanup_barrier_is_unconfirmed` in `sdk/python/tests/unit/test_runtime_session.py` (fake runtime session; asserts both warnings and silence on clean outcomes). Because `uv run` would rebuild the native extension and no installed native matches this checkout, the committed pytest case is marked **pending Host-rebuilt Python execution**; the facade logic itself was verified in this checkout with a one-off harness that loads the installed `_native` extension read-only and imports this tree's `c_two` source: both unconfirmed outcomes warn (`Shutdown could not confirm IPC client cache cleanup: …`), clean outcomes stay silent.

All prior fixes are retained unchanged: serialized drain generations, aggregate deadline, per-client close gate with restored receive-task handles, epoch fence and restart semantics, held-lease validity, and the env-isolation guards on config-reading `client_modes` tests.

## Independent-review corrections (first revision)

The Host re-ran the Core (26+15+9+5+6) and IPC (64) suites successfully and identified five defects in the uncovered concurrent paths. All five are corrected here with deterministic tests:

1. **Concurrent `close_all` callers could interleave fences.** The second empty drain cleared the closing flag and bumped the epoch while the first drain was still closing slow clients, so an acquire between them could insert a live client that neither drain covered. Correction: drain transactions are now serialized through a closing *generation* in a dedicated `CloseTxnState` coordinator (`close_txn: Arc<(Mutex<CloseTxnState>, Condvar)>`). A second `close_all` waits for the active generation to finish — bounded by its own absolute deadline — and then performs its own drain, so an entry raced in between two drains is covered by the second. A timed-out waiter returns `ClientCacheCloseReport::error` ("timed out waiting for a concurrent cache drain") without touching the fence. No entry/state lock is ever held during I/O. Proof: `concurrent_close_all_drains_serialize_and_acquires_reject_during_blocked_drain` (deterministic via the writer-slot guard, the coordinator condvar, and a readiness hook — the second caller's 300 ms deadline expires against a blocked first drain; acquires reject during it; the first drain then confirms and the cache reopens under epoch 1).
2. **Detached-close barriers were invisible to shutdown.** A discard/sweep/loser close running outside the lock was not accounted, so `close_all` could report drained while such a barrier was active. Correction: every detached close (drain children, discard, sweep, stale/loser replacement) runs through `ClientPool::close_accounted`, which registers itself in `in_flight_detached_closes` and notifies the coordinator condvar; `close_all` waits for these barriers (bounded by its deadline) before starting and again before finalizing, and withholds the drained claim with an error if any are still active. Proof: `close_all_does_not_claim_drained_while_discard_barrier_is_in_flight` (deterministic: the discard's barrier registration is awaited via a condvar hook before `close_all(300ms)` runs; it must report the in-flight error and must not start a drain generation; after the guard releases, a clean `close_all` follows).
3. **Per-child timeouts multiplied the shutdown budget.** Each drained client previously received the full `timeout`. Correction: one aggregate absolute deadline per `close_all`; each child close receives only `deadline - now` (possibly zero), and child-internal phases (including the post-abort join slice and the final writer clear) are all capped by that same absolute deadline. Proof: `close_all_deadline_is_aggregate_across_blocked_entries` — three cache entries with held writer slots drain inside one 1 s deadline (total < 2 s, independent of N), all three honestly unconfirmed.
4. **`IpcClient::close_shared_bounded` take-race.** Taking `recv_handle` let a concurrent close observe `None` and return `true` before the original join — treating a missing handle as terminal proof. Correction: a per-client `close_gate: tokio::sync::Mutex<()>` serializes close barriers (gate acquisition itself bounded by the caller's deadline; a gate timeout returns an honest unconfirmed). An aborted-but-unjoined receive task now has its handle **restored** to `recv_handle`, so unconfirmed tasks stay observable and retryable — a missing handle always means "no live receive task". The post-abort join slice is capped by `min(1 s, time remaining to the absolute deadline)`, so child phases never exceed the caller's budget. Proof: `two_concurrent_closes_of_one_blocked_receiver_serialize_and_stay_retryable` — two barrier-synchronized closes of one blocked receiver both stay bounded, the barrier that consumes its receive budget reports an honest unconfirmed, and a subsequent close retries the restored handle and confirms.
5. **Public `Runtime::ipc_client_pool` mutable escape.** The accessor bypassed config freeze/lifecycle and existed only for tests. Correction: removed (0.x clean cut). The inspector test now uses a standalone `c2_ipc::ClientPool` (`ClientPool::new` + public `acquire`/`release`) with identical assertions.

Additionally, `ClientCacheCloseReport` gained `error: Option<String>`, and Core `Runtime::shutdown` only claims `ipc_clients_drained` when every detached client confirmed **and** no error (serialized-drain wait or in-flight detached barriers) was reported; the message is propagated through `ShutdownOutcome.ipc_client_close_error`.

**Test env-isolation correction (Host inquiry):** the two new c2-core tests initially constructed Runtimes (whose client-config resolution reads the process environment) without the file-wide `ENV_LOCK` guard used by every other env-reading test in `client_modes.rs` — the exact Windows-CI reader/writer race class the Host flagged. Both tests now take `relay_env_lock()` before any `Runtime::new`/config resolution. The four new c2-ipc pool tests construct `ClientIpcConfig::default()` explicitly and never read the environment; the inspector-test change lives inside an already-guarded test. `cargo test -p c2-core --test client_modes` (default threading) re-verified: 15 passed, 0 failed.

**Propagation check (fix 5 verification):** the native `RuntimeSession.shutdown` dict includes `ipc_client_close_error`, and `Server.shutdown()` returns the outcome dict to its caller. However, Python's top-level `cc.shutdown()` (`sdk/python/src/c_two/transport/registry.py`) currently inspects only `relay_errors` and silently ignores all outcome error keys — including the pre-existing `runtime_barrier_error` and `route_close_error`, not just the new key. That facade file is outside this slice's write scope; a scope request is recorded in the turn outcome (one `log.warning` for `ipc_client_close_error`/`runtime_barrier_error` in `RuntimeRegistry.shutdown`).

## What changed

### Core runtime (`core/runtime/c2-core/src/session.rs`)

- `RuntimeState` now owns `client_pool: Arc<c2_ipc::ClientPool>` and `frozen_client_config: Option<ClientIpcConfig>`. Runtime clones share both; distinct `Runtime`s are isolated. The process-global `ClientPool::instance()` singleton is gone from Core.
- `acquire_ipc_client` resolves and freezes the client IPC config **atomically under the `RuntimeState` lock before any connect I/O**, then connects through the cache outside the lock. Documented freeze semantics: the **first valid connection attempt freezes the config, including attempts that later fail** (an unreachable address still freezes). A setter racing a stalled first connect observes `LifecycleError::ClientConfigFrozen` and cannot start a second config.
- `Runtime::shutdown` drains this Runtime's client cache (`close_all`) before touching the server, in both hosted and hostless lifecycles. New public `Runtime::shutdown_without_host(timeout)` gives Rust client-only lifecycles the same supported path; nothing process-global is reset.
- `Runtime::ipc_client_pool()` exposes the cache handle for inspection/test scaffolding only.
- `ShutdownOutcome` gains `ipc_client_close_error: Option<String>` naming addresses whose close barrier was unconfirmed (smallest consistent 0.x public-shape addition).
- The existing inspector test now uses a dedicated inspector `Runtime`'s own cache instead of the singleton; its assertions are unchanged.

### c2-ipc cache (`core/transport/c2-ipc/src/pool.rs`)

- `ClientPool` mutable state is now `CacheState { entries, epoch, closing }` behind one lock. Every connect and every close runs **outside** the lock; the lock only guards bookkeeping, the closing fence, and the epoch.
- `shutdown_all()` is replaced by `close_all(timeout) -> ClientCacheCloseReport { detached, unconfirmed, error }`: drain transactions serialize through a closing generation (see the revision section above); it sets the closing fence, detaches all entries, closes each through the bounded shared-ownership close outside the lock under **one aggregate absolute deadline**, accounts in-flight detached-close barriers, then bumps the epoch and reopens the cache. Acquisitions started before the drain observe the closing fence or the epoch mismatch, close their fresh connection explicitly, and reject; acquisitions during the closing window reject immediately; later acquires (restart/reacquire) proceed under the new epoch.
- Concurrent same-address race losers, stale-entry replacements, expiry sweeps, and `discard_if_same` all detach under the lock and close outside it through the bounded close (5 s detached-close deadline constant). Exact-client identity fencing on `release_if_same` / `discard_if_same` is unchanged.
- Singleton API (`instance()`, `reset_instance()`), its static, and its test are removed (0.x clean cut; no production caller remained after the Core/native/inspector updates). `HttpClientPool` in `c2-http` is a separate mechanism and is untouched.

### c2-ipc close barrier (`core/transport/c2-ipc/src/client.rs`, `sync_client.rs`)

- `IpcClient::close_shared_bounded(timeout)` bounds every phase from one deadline: writer-lock acquisition + disconnect exchange, receive-task join, and the final writer-slot clear. On writer timeout it aborts the stream so a blocked writer fails and releases the lock instead of pinning the barrier. On recv-join timeout it aborts the stream and task, then waits once more within a **fresh bounded join slice (≤ 1 s)** — fixed in this continuation: the previous code gave the post-abort join the already-exhausted deadline (zero budget), contradicting its own doc and making abort recovery ineffective. Worst-case barrier is now ≤ 2×timeout and always returns.
- `false` from the barrier is an honest "unconfirmed": tasks may still be draining; nothing is declared stopped, no accounting is zeroed, and no backing memory is force-released. An unconfirmed close does not poison the client: a later close confirms once the blocker clears (tested).
- New `SyncClient::close_shared(timeout)` projects the bounded barrier through `Arc<SyncClient>` shared ownership. `IpcClient::Drop` is non-blocking best-effort (abort only) so abandoned receive tasks cannot pin pool `Arc`s forever and no Drop blocks under cache/state locks.
- Held `ResponseLease` pool `Arc`s remain valid until released even after the connection closes (tested at the Runtime level with a 256 KiB SHM-backed held response).

### Native projection (`sdk/python/native/src/runtime_session_ffi.rs`)

- The hostless branch of `RuntimeSession.shutdown` now calls Core `shutdown_without_host(timeout)` instead of returning a default outcome, so client-only native sessions close their outgoing IPC clients through the same bounded barrier. Python projects the new `ipc_client_close_error` outcome key. Python owns no new mechanism.

## Deterministic acceptance tests (no timing-only sleeps)

All races use server-side observation channels or readiness oneshots; every wait is bounded.

c2-ipc (`core/transport/c2-ipc/src/pool.rs` tests):

- `close_shared_is_bounded_and_honest_when_writer_slot_is_held` — replaces the previously stalled `close_shared_is_bounded_and_honest_when_writer_is_blocked`. The blocked writer is modeled deterministically with a test-only writer-slot guard (`hold_writer_slot_for_test`, `#[cfg(test)]`) and a readiness oneshot, against the real `c2-server` protocol stack: no 32 MiB stack arrays, no kernel-pipe-pressure assumptions, no half-served control protocol. Asserts bounded close (< 10 s wall against a 1 s deadline), honest `false`, and honest recovery (a second close confirms after the holder releases).
- `close_shared_aborts_blocked_receiver_within_deadline` — a fake peer serves the real handshake, then stalls after two bytes of a frame length prefix; the receive task is blocked mid-frame. Close must abort the stalled stream, let the receive task finish in the bounded join slice, and confirm — without waiting for the peer.
- `close_all_rejects_in_flight_connect_and_reopens_under_new_epoch` — a stalled handshake peer holds one acquire deterministically mid-connect (handshake request observed server-side); a drain during the stall must fence the connect: when the handshake later succeeds, the fresh connection is closed explicitly (server observes disconnect/EOF), the acquire rejects with "cache closed while connecting", and a fresh acquire under the new epoch succeeds and drains cleanly.
- `concurrent_same_address_loser_is_explicitly_closed` (from the first attempt) — both same-address connects dial before either handshake is answered; the loser's connection is observed closed server-side while exactly one entry remains.
- `acquire_rejects_while_cache_is_closing` — acquisitions during the closing window reject.
- `test_pool_close_all_drains_and_reopens_under_new_epoch` — drain empties the cache, clears `closing`, bumps the epoch; `release_if_same`/`discard_if_same` identity-fencing tests unchanged.

c2-core (`core/runtime/c2-core/tests/client_modes.rs`):

- `hostless_shutdown_closes_own_clients_and_isolates_sibling_runtimes_and_held_leases` — one hosting Runtime, two hostless client Runtimes over direct IPC. A's `shutdown_without_host` confirms the drain and closes only A's client (A's calls fail, B stays callable, A's held nothing); B's own hostless shutdown closes B's connection while B's 256 KiB held response lease stays valid and releases cleanly afterwards.
- `first_connection_attempt_freezes_client_config_atomically_before_connect_io` — a peer reads the handshake and stalls; once the handshake bytes are observed the frozen flag is already set (happens-before, no polling), the racing setter gets `ClientConfigFrozen`, and the freeze survives the failed first attempt after the stall resolves as EOF.

## Commands and actual results

Environment: `CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/c2-memory-runtime-build FASTDB_PAYLOAD_LINK_MODE=system FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib DYLD_LIBRARY_PATH=/private/tmp/c2-memory-fastdb-sdk/lib`.

```bash
# Focused new/fixed tests (run first, all green before suites)
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib -- \
  close_shared_is_bounded close_shared_aborts_blocked_receiver close_all_rejects_in_flight
  → 3 passed

client_modes binary --test-threads=1 hostless_shutdown_closes_own_clients first_connection_attempt_freezes
  → 2 passed

# Full relevant suites
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib
cargo test --manifest-path core/Cargo.toml -p c2-core

# Native projection compile check
cd sdk/python/native && PYO3_PYTHON=/tmp/c2-memory-venv/bin/python cargo check
```

Suite and check results are recorded below in "Sealed results".

## Known limits

- The blocked-bulk-write close scenario is modeled by the deterministic writer-slot guard rather than a real multi-megabyte stalled pipe write. Rationale: with the default config large payloads travel via SHM (no blocked pipe write), and a fake server that also serves the route-lookup control exchange would duplicate fragile protocol internals. The guard exercises the exact close-barrier phase (writer slot unavailable) with readiness fencing; the real-protocol paths run against the real `c2-server`.
- The native projection is compile-verified (`cargo check` with `PYO3_PYTHON=/tmp/c2-memory-venv/bin/python`) and its hostless close behavior is covered by the c2-core tests of `shutdown_without_host`; Python-level pytest end-to-end runs are deferred because they require rebuilding the native extension (`uv sync --reinstall-package c-two`), which would touch shared environments outside this slice's authorization. The Host venv was not modified.
- `close_shared_bounded` keeps every phase, including the post-abort join slice and the final writer clear, inside the caller's single absolute deadline (the join slice is additionally capped at 1 s). Concurrent closers serialize through the close gate; a gate timeout or an exhausted budget returns an honest unconfirmed close that stays retryable through the restored receive-task handle.
- Freeze-on-first-attempt means a connect to a typo'd address freezes the Runtime's client config for its lifetime. This is the documented 0.x choice (prevents two configs racing a stalled connect); a future config-reset surface would need an explicit lifecycle decision.
- Pool decay sweeping remains lazy (inside `acquire` plus the public `sweep_expired`); no background maintenance timer was added in this slice.

## Lifecycle hooks for the forthcoming policy/budget maintenance task

Integration points the pool-policy worker should join (flagged per task instructions, not implemented here):

- `ClientPool::close_all(timeout) -> ClientCacheCloseReport` is the single drain/join hook for Runtime shutdown; it already tolerates unconfirmed closes without lying.
- `ClientPool::sweep_expired()` detaches expired entries under the lock and closes them outside it; a periodic policy maintenance task should call it without holding any cache/state lock across closes (the current shape).
- `SyncClient::close_shared(timeout)` / `IpcClient::close_shared_bounded(timeout)` are the bounded close barriers any policy-driven eviction must reuse; `Drop` is abort-only by design.
- `DETACHED_CLOSE_TIMEOUT` (5 s) is a const; the policy task may want it derived from configuration — that change belongs with the budget wiring, not here.
- The `CacheState.epoch`/`closing` fence must be preserved by any maintenance-induced drain so restart/reacquire semantics stay intact.
- Budget guards for held leases must stay charged across connection close (this slice keeps held `ResponseLease` pool Arcs valid until release; the budget task layers accounting on top).

## Sealed results

All commands run in the isolated managed checkout with the environment above (2026-09-26, second revision).

- Focused second-review tests (`cargo test -p c2-ipc --lib -- close_all_ close_shared_ prior_unconfirmed concurrent_close_all`, default threading): **8 passed** — including the new `prior_unconfirmed_drained_client_is_retried_and_reported_by_later_close_all` and the discard-vs-shutdown test whose readiness hook now fires exactly in the detach→I/O window.
- Full `cargo test -p c2-ipc --lib` (default threading): **69 passed; 0 failed; finished in 3.02 s**.
- Full `cargo test -p c2-core` (default threading): lib unittests **26**; `client_modes` **15**; `error_normalization` **9**; `host_routes` **5**; `lifetime_ordering` **6**; doc-tests 0 — **0 failed** (env-isolation guards retained).
- Native projection: `cd sdk/python/native && PYO3_PYTHON=/tmp/c2-memory-venv/bin/python cargo check` — **Finished `dev` profile in 1.76 s**, no errors or warnings.
- Python facade: `sdk/python/tests/unit/test_runtime_session.py::test_shutdown_warns_when_native_cleanup_barrier_is_unconfirmed` committed but **pending Host-rebuilt Python execution** (`uv run` would rebuild the native extension; no installed native matches this checkout). The facade logic was verified in-checkout with a one-off harness loading the installed `_native` extension read-only against this tree's `c_two` source: both unconfirmed outcome keys warn, clean outcomes stay silent (`FACADE-HARNESS-OK`).
- Diff stays within the granted scope: `core/runtime/c2-core`, `core/transport/c2-ipc`, `sdk/python/native/src/runtime_session_ffi.rs`, `sdk/python/src/c_two/transport/registry.py`, `sdk/python/tests/unit/test_runtime_session.py`, and this report. Host venv untouched; no byte budgets or policy/maintenance features added; no push, no release/version edits.

Prior-revision results (superseded but still valid evidence for those fixtures): first revision — suites 68 / 26+15+9+5+6, native check 1.72 s; initial slice — 64 / 26+15+9+5+6, native check 25.61 s.
