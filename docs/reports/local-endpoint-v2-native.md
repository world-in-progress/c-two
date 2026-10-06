# Managed-v2 Unix endpoint ownership and retirement (c2-local native slice)

Status: the `c2-local` native layer now implements the managed-v2 ownership and retirement protocol for explicitly selected `managed-v2` endpoints. Legacy `legacy-v1` listeners, records, reap, and sweep behavior are unchanged, and remain the default through `LocalEndpoint::from_address`. This is a native-layer slice only: `c2-core`, `c2-server`, the Rust SDK, and Python/PyO3 still do not select `managed-v2`, and no Runtime/SDK routing behavior was touched.

Baseline: `fbc2f7a387572b3f924cc4d5f160f03d5ff8dbb3`.

## Scope and ownership boundary

`c2-config::LocalEndpoint` remains the single authority for the logical-to-OS mapping. `LocalListener::bind` dispatches on `endpoint.protocol()`: `ManagedV2` binds through the new `unix_managed` module, everything else keeps the existing v1 path in `unix_endpoint`/`unix`. Windows is untouched: it adds no files, `c2-config` still rejects `managed-v2` derivation there as `Unsupported`, and named-pipe lifecycle remains kernel-managed.

Changed files in this slice:

- `core/transport/c2-local/src/unix_managed.rs` (new) and `unix_managed/tests.rs` (new).
- `core/transport/c2-local/src/{lib.rs, unix.rs, unix_endpoint.rs}`: dispatch, shared credential/result fields, and P1 helper exposure.
- `core/transport/c2-local/Cargo.toml`: `uuid` (v4) added to the Unix target dependencies for fresh listener incarnations.
- `core/Cargo.lock` needed no edit; `cargo test --locked` passes against the existing lock.

## Namespace and protocol

Namespace root is the canonical managed derivation `/tmp/c2-<effective-uid-hex>/v2`. Both the parent directory and the versioned directory are opened with `O_DIRECTORY | O_NOFOLLOW`, verified as owned by the effective UID with exactly `0700`, and re-checked against the path before every mutation. Creation uses the existing explicit-mode `DirBuilder` helper, so a permissive process umask cannot create a directory the module then refuses; pre-existing unsafe directories are never chmod-ed.

| Object | Name | Rules |
| --- | --- | --- |
| Coordinator gate | `.gate` | Opened `O_RDWR|O_CREAT|O_EXCL` (0600, no-follow) only while initializing; otherwise opened without `O_CREAT`. Held with `flock(LOCK_EX)` for every lease open/create/delete/retire. Long-lived constant: never unlinked by the protocol and never reused under a different identity. |
| Gate marker | `.gate.marker` | 25-byte bounded record: magic, version, gate device, gate inode. Written once, exclusively, while holding the gate. Validated on every open. |
| Endpoint lease | `<sha256(server-id)>.lease` | One per endpoint. `flock(LOCK_EX|LOCK_NB)` held by its listener for the listener's entire life. Stores the bounded owner record. |
| Endpoint socket | `<sha256(server-id)>.sock` | Bound after the lease is locked; `0600`; identity recorded in the lease. |

Initialization identity boundaries:

- First initialization: gate created, marker written with the gate device/inode, before any endpoint exists.
- Crash between gate creation and marker write with no endpoint objects: the next participant opens the *same* gate inode (creation is not forced) and completes initialization. No second lock is possible.
- Endpoint objects present without a marker: `Unverified(InitializationIncomplete)`. The fixed gate is **never unlinked online**, not even by a refused first initialization that created it: a drop-then-unlink window would let a third party bind a *different* coordinator inode at the same name. The namespace stays visibly Unverified and every later open keeps using the same inode. First-initialization scanning is bounded to 64 entries and fails closed beyond that instead of traversing an unbounded directory.
- Marker present but gate missing: `Unverified(CoordinatorMissing)`; no replacement gate is created.
- Marker present but the gate entry names a different inode, or the marker is malformed: `Unverified(CoordinatorReplaced)`; the marker is never rewritten to adopt a replacement.
- Crash after `bind` but before the owner record is durable leaves an unregistered socket. Bind refuses it (`AddrInUse`) and the reaper keeps it `Unverified`; this is the explicit "identity incomplete" boundary that requires bounded manual maintenance, matching the P2 plan's refusal to delete by age or guess.

## Descriptor-bound socket creation (P1 blocker repair)

A socket address is resolved against the **verified directory descriptor**, never the absolute path that descriptor was opened from. Holding the gate is an unbounded window: a concurrent `rename` of `/tmp/c2-<uid>/v2` can redirect the path after verification, and binding through the path would leave this process's socket inside a replacement directory it does not own.

- After the gate is acquired, both the private parent and the versioned root are re-checked through their open descriptors (`path_still_names_open_directory`), and the namespace is refused unless both still name those exact inodes.
- The gate entry is re-verified immediately before bind, and again inside `gate_still_named()` for every retire/reap/inspect operation.
- Binding itself goes through `bind_in_verified_directory`. On Linux the descriptor is named as `/proc/self/fd/<fd>/<name>`. macOS has no equivalent path (`/dev/fd/<dirfd>/<name>` returns `ENOENT` for a socket bind, verified directly), so it binds on a **dedicated short-lived thread**: that thread switches its *own* working directory with `pthread_fchdir_np(dirfd)`, binds the bare relative name, and the thread exits immediately afterwards. The caller's thread directory is never read, written, or restored, so an existing per-thread cwd is untouched and the process-wide directory never moves. Any other Unix target has no descriptor-anchored bind primitive and fails closed as `Unsupported`; there is deliberately no fallback to the stale absolute path.
- The switch is thread-local and owned by a thread that exists only for one synchronous bind, so no other thread, and never the caller, observes it. A regression negative runs a real bind in an independent subprocess and asserts that the default follow-process cwd, an already-established thread-local cwd, and the process working directory are all unchanged.

## Owner record and credentials

The lease record is bounded (`RECORD_FIXED_LEN + address <= 1084` bytes): magic, version, protocol byte `2`, address length, full logical address, fresh 16-byte incarnation (`uuid::Uuid::new_v4`), and the socket identity (device, inode, ctime seconds/nanos). It is opened `O_NOFOLLOW`, verified as a private regular file owned by the current UID, and rejected if it does not decode or exceeds the bound.

`record_matches_endpoint` re-derives the endpoint through `LocalEndpoint::from_address_with_protocol(record.address, ManagedV2)` and compares the resulting OS name with the slot. The hash basename is therefore never treated as a logical identity, and the digest mapping is not reimplemented.

`EndpointCredential` gained an optional managed incarnation (Unix only). A managed credential proves both the exact socket identity and the listener incarnation, so an old credential returns `StaleTarget` for a rebound instance even when the OS reuses the same inode or ctime.

## Listener, Drop, reap, sweep

- Bind takes the gate with a **bounded** acquisition (50 attempts at 2 ms), then opens/locks the lease, replaces a stale socket only when the probe does not veto and the previous record matches the exact current inode, binds, sets permissions, writes and re-reads the record, re-verifies that the recorded identity still names the bound entry, and only then releases the gate. A busy foreign coordinator is never waited on unboundedly: startup reaches this path, so it surfaces as `AddrInUse` and the upper start attempt retries with its own timeout. A bound-socket rollback guard (the P1 guard, reused) withdraws exactly the object this call created on failure and never removes a replacement object.
- The listener keeps its lease flock for life. No `connect` probe, PID, or age is part of listener ownership.
- Explicit `close()` closes the listening descriptor first, then acquires the gate with a **bounded** retry (50 attempts at 2 ms) instead of waiting forever, and plays the documented order: verify its own record/entries, unlink its socket, unlink its lease. The listener descriptor is closed before the gate attempt, so a busy coordinator can never keep the listening socket open. `Drop` uses a single nonblocking gate attempt: it closes the listening descriptor first, retires when the gate is available, and otherwise releases the lease handle and leaves the complete metadata for the reaper. There is no unbounded background task.
- Lease release is explicit: the listener issues `flock(LOCK_UN)` on its lease open file description and then closes its own descriptor, in both `close()` and the bounded `Drop` path. `flock` locks belong to the *open file description*, so a `fork`/`dup` duplicate would keep the lock held if the listener merely closed its descriptor; the explicit `LOCK_UN` releases ownership deterministically even while an inherited duplicate is still open, and never extends it to an unrelated duplicate.
- `reap_endpoint`, `reap_slot`, and `inspect_endpoint` with a managed credential take the gate **nonblockingly** (`LOCK_EX|LOCK_NB`) and report `Busy` / a `WouldBlock` inspection when another process holds it. A foreign holder can therefore never stall maintenance: the caller keeps its budget, the round keeps advancing through other entries, and a later pass retries the slot. `reap_endpoint` requires a matching endpoint and managed credential, holds the gate, and only tries the lease lock nonblockingly (`Busy` when a listener owns it, never blocks). It deletes only when the locked record, the current socket identity, and the expected credential all agree; a valid record whose socket is already gone may still retire its lease. Unknown, malformed, symlinked, foreign, or initialization-incomplete objects stay `Unverified`, and no age/PID/connect-failure rule can authorize a delete.
- `EndpointSweep::for_endpoint` dispatches on the endpoint's real protocol. The managed sweep enumerates the versioned namespace with a retained iterator and bounded counters, acquires the gate per candidate (never across the traversal), rebuilds the logical endpoint from each verified record and compares the slot, counts `Busy`/`Unverified`/IO failures against the budget, and can retire socket-less lease slots (reported through the new `SweepBatch::leases_retired`). Interrupted rounds keep `round_interrupted` and never report `round_complete`. The legacy `/tmp/c_two_ipc` history is never enumerated or deleted by a managed sweep.
- All directory mutations are performed relative to the verified namespace dirfd; sweep batches keep the P1 bounded `EndpointIoError` shape.

## API changes

- `LocalEndpoint` is unchanged; `managed-v2` selection continues to come only from `LocalEndpoint::from_address_with_protocol`.
- New `EndpointUnverifiedReason` variants: `CoordinatorMissing`, `CoordinatorReplaced`, `InitializationIncomplete`.
- New `SweepBatch` field: `leases_retired`.
- `EndpointCredential` carries a managed incarnation internally; no public constructor or method was removed.
- `LocalListener::{bind, credential, close, accept}`, `inspect_endpoint`, `reap_endpoint`, and `EndpointSweep::{open, for_endpoint, next_batch}` keep their signatures.

## Tests

27 managed tests were added; the 29 pre-existing legacy tests are unchanged and still pass (56 total). The new tests use the real public paths for the real namespace (`/tmp/c2-<uid>/v2` derived by `c2-config`), real child processes, and isolated two-level `0700` roots for fault injection. They run under the normal parallel harness (no `--test-threads=1`); a single small mutex is taken only by the four tests that assert on slots in the *shared* derived namespace while another test may legitimately sweep that same namespace, so no test weakens its assertions and the suite as a whole is not serialized.

| Test | Coverage |
| --- | --- |
| `managed_public_namespace_lifecycle_converges_over_repeated_runs` | 120 real bind/accept/close lifecycles on the derived namespace; socket and lease return to gate + marker only; duplicate bind is `AddrInUse`. |
| `managed_concurrent_same_address_bind_has_exactly_one_winner` | 8 threads race the same managed address; exactly one winner, live reap is `Busy`. |
| `managed_two_process_competition_and_registered_kill_converge` | Real second process owns the address; duplicate bind refused in a second child; SIGKILL of the registered holder leaves socket + lease that reap converges (`Reaped`, then `AlreadyAbsent`). |
| `managed_registered_exit_without_destructors_is_reapable` | Registered child exits without destructors; record is `Present`; reap removes socket and lease. |
| `managed_live_listener_is_busy_and_disconnects_keep_service_available` | Live endpoint reap is `Busy`; connect/disconnect cycles keep the service reachable. |
| `managed_old_credential_never_removes_a_new_incarnation` | Old credential is `Busy` while the new listener lives and `StaleTarget` after the new instance is abandoned; the current credential removes it. |
| `managed_missing_or_replaced_gate_never_creates_a_second_lock` | Deleted gate → `CoordinatorMissing` with no new gate entry; replacement inode → `CoordinatorReplaced` with the marker byte-identical. |
| `managed_corrupt_record_and_symlink_are_unverified_and_untouched` | Corrupt lease → `InvalidRecord`; symlinked socket → `Symlink`; both objects and the symlink target unchanged. |
| `managed_budgeted_sweep_advances_past_busy_and_corrupt_slots` | One-entry batches advance past live, crashed, socket-less, and corrupt slots; crashed socket and orphan lease converge, corrupt and live slots remain. |
| `managed_public_sweep_targets_the_versioned_namespace_only` | `EndpointSweep::for_endpoint` on a managed endpoint converges a registered leftover, keeps the live slot, and leaves a legacy `/tmp/c_two_ipc` socket untouched. |
| `managed_failed_initialization_withdraws_its_socket_and_rebinds` | Injected socket-permission failure withdraws only the socket this bind created; the fixed address rebinds and then retires cleanly. |
| `managed_drop_without_gate_leaves_metadata_for_the_reaper` | While the gate is provably held elsewhere, Drop stays bounded, leaves socket and lease metadata, and a later reap converges them after the gate is released. |
| `managed_interrupted_sweep_never_reports_a_completed_round` | Replacing the verified namespace makes the managed round `round_interrupted` with `namespace_changed`, and every later batch stays incomplete. |
| `managed_initialization_window_stays_unverified_without_creating_locks` | Endpoint object without a first-initialization identity → `InitializationIncomplete`, no gate and no marker created (create and read-only opens). |
| `managed_directory_rename_inside_the_gate_window_never_binds_into_the_replacement` | P1 negative case: a controlled barrier parks an opener inside the gate-hold window while `v2` is renamed away and a fresh `v2` takes its name; the opener fails, no socket is created in either directory, and a foreign object in the replacement directory is untouched. |
| `managed_parallel_first_initialization_shares_one_gate_inode` | Six threads race a brand-new namespace; every opener succeeds, all bind under the same coordinator inode, and exactly one `.gate` entry exists. |
| `managed_busy_gate_never_blocks_maintenance_and_budget_still_advances` | A foreign holder owns the gate: `reap_managed`, `reap_slot`, and `inspect` all return `Busy`/`WouldBlock` promptly, and a budgeted sweep still reaches EOF, counts the blocked slot, and converges it after the holder leaves. |
| `managed_record_failures_roll_back_conservatively` | Injected owner-record write failure, partial write, and foreign replacement after bind: the socket is withdrawn or the foreign object preserved, the slot stays `Unverified`, and the address is reusable. |
| `managed_duplicate_lease_cannot_extend_ownership_across_retirement` | A true duplicate of the listener's own lease open file description (fork-style inheritance): reap is `Busy` while the listener lives; the listener's `close()` and its bounded no-gate `Drop` both release ownership via explicit `LOCK_UN` so a re-bind and a post-Drop reap succeed **while the duplicate is still open**; the retired lease entry never comes back. Verified to fail if `LOCK_UN` is removed. |
| `managed_bind_never_changes_the_calling_thread_directory` | Holder threads carry their own thread-local cwd across two real public binds; the calling thread's directory is unchanged mid-call and after, and the process-wide cwd never moves. |
| `managed_bind_does_not_change_cwd_in_an_independent_subprocess` | Separate-process negative: a real managed bind leaves the default follow-process cwd, a pre-existing thread-local cwd, and the process working directory all unchanged. |
| `managed_bind_is_bounded_when_the_gate_is_held` | A foreign holder owns the gate: `bind_managed_at` returns `AddrInUse` within the bounded window, creates nothing, and a retry after the holder leaves binds. Verified to hang if the unbounded blocking gate is restored. |
| `managed_descriptor_bind_support_matches_the_platform` | Non-Linux/non-macOS targets fail closed as `Unsupported` for the descriptor-anchored bind; supported targets are covered by the real bind tests. |
| `managed_bind_target_is_descriptor_relative_not_the_absolute_path` | The bind name is descriptor-relative (`/proc/self/fd` on Linux, dedicated-thread bare relative name on macOS), a real bind lands in the verified root and serves a client, and the derived production slot is never created. |
| `managed_first_initialization_scan_is_bounded_and_fails_closed` | A namespace exceeding the 64-entry first-init scan bound is refused and never adopted. |
| `managed_record_decode_rejects_hash_only_and_oversized_payloads` | Record round-trip, bounded size, address/hash mapping via `LocalEndpoint`, and rejection of truncated/oversized/wrong-protocol records. |
| `managed_process_fixture` | Child-process fixture used by the real-process tests (no-op without its environment). |
| `legacy_derivation_remains_the_default_and_untouched` | `from_address` still selects `legacy-v1` at `/tmp/c_two_ipc`. |

## Validation results

Environment: macOS 26.6.2 (Darwin 25.6.0, arm64), rustc/cargo 1.91.0, `CARGO_BUILD_JOBS=2`, default parallel test harness.

| Command | Result |
| --- | --- |
| `cargo test --locked --manifest-path core/Cargo.toml -p c2-local --lib` | Passed 8 consecutive default-parallel runs: 57 passed, 0 failed, 0 ignored (29 legacy + 28 managed). The derived namespace is left to gate + marker only after every run. |
| `cargo fmt --manifest-path core/Cargo.toml -p c2-local -- --check` | Clean; the changed files were also formatted directly with `rustfmt --edition 2024`. |
| `cargo check --manifest-path core/Cargo.toml -p c2-local --target x86_64-pc-windows-msvc --tests` | Passed, exit 0. Windows layout is unchanged and `managed-v2` remains explicitly unsupported there. |
| `git diff --check` | Clean. |
| Cargo.lock | Unchanged; `--locked` succeeds. |
| `cargo check --locked -p c2-ipc -p c2-server -p c2-core` | Passed, exit 0; dependent native crates are unaffected. |
| `git diff --cached --name-only` | Empty: no Git index operation was performed. |

## Failure record kept for the round

Round 1 exposed three real defects before delivery, all fixed and covered by the tests above:

1. The record/slot mapping originally hashed the full `ipc://…` string instead of the canonical server ID, so no record matched its slot. The fix re-derives the endpoint through `LocalEndpoint` authority instead of duplicating a digest.
2. A managed listener did not retain its namespace root, so explicit close and bounded Drop retried retirement against the default UID namespace instead of the listener's own namespace. The root is retained again and used by both paths.
3. A socket slot without a lease was counted as `AlreadyAbsent` by the sweep; it is now `Unverified(MissingOwnership)`, consistent with the public reap result.

**Round 2 (this narrow repair)**, driven by the Host's fixed-source review of the rejected candidate:

4. **P1 open_root directory identity.** After the gate wait, the parent and root were not re-checked, and the bind used the derived absolute path. A concurrent `rename` plus a fresh `v2` could make `UnixListener::bind` land in the replacement directory. Fixed by the descriptor re-checks and `bind_in_verified_directory` above, with the rename-window negative test.
5. **P2 blocking maintenance.** `reap_slot`, `reap_slot`'s `open_root`, and `inspect_managed_at` used a blocking `LOCK_EX`, and explicit `close()` waited unboundedly. A foreign holder could stall a sweep round forever. Fixed with nonblocking gate attempts (bounded retry for `close`), covered by the busy-gate test.
6. **Gate unlink race.** A refused first initialization dropped the gate and then unlinked it if the call had created it, opening a window where a third party could bind a second coordinator inode. The gate is now never unlinked online.
7. **Unbounded first-init scan.** The "any endpoint entries?" walk is now bounded to 64 entries and fails closed, and the refusal leaves the fixed gate in place rather than deleting it.

Two test-authoring mistakes in this round were corrected rather than papered over: an over-strict assertion expected a socket-less lease to be deleted (it stays `Unverified` when its record is unreadable, which is the conservative behavior), and the shared-namespace tests raced a legitimate concurrent sweep (now guarded by a single small mutex used only by those tests). The intermediate failing runs are not presented as validation; only the final clean runs are.

**Round 3 (this repair)**, driven by the Host's fixed-source probe of the `380bee` candidate:

8. **macOS caller-thread cwd.** The previous repair bound through a caller-side `ThreadDirGuard` that restored a saved descriptor. The Host showed directly that this changed ambient behavior: a thread that had been following the process cwd became pinned to its own directory after a guard restore. The guard production path is deleted. macOS now binds on a dedicated short-lived thread that switches only its own directory and exits with the bind, so the caller's thread directory is never touched at all. Non-macOS/non-Linux targets fail closed as `Unsupported`. Stale `ThreadDirGuard` code, helpers, and docs were removed (no zombie helper left behind).
9. **Lease release was Drop-only.** The managed lease was released only by dropping its `File`. Because `flock` locks belong to the open file description, a fork-style duplicate kept the lock after the listener closed, blocking a fresh bind or reap. `Lease::release` now issues an explicit `LOCK_UN` and closes the listener's own descriptor, in both `close()` and the bounded `Drop` path. The test now proves re-bind and reap succeed **while the duplicate is still open** (verified to fail without `LOCK_UN`).
10. **Unbounded bind gate.** `bind_managed_at` opened the gate with an unbounded blocking `LOCK_EX`, reachable from `Server::start`; a foreign holder could stall accept startup indefinitely. It now uses the bounded gate acquisition and surfaces `AddrInUse`, leaving retry-with-timeout to the upper start attempt (verified to hang under the old behavior).

The existing maintained `Busy` / namespace-identity / fault-injection tests were kept and were not weakened.

## Not executed / not claimed

- Linux (GNU) execution of the new tests: the tests are Unix-generic, but the Linux branch uses `/proc/self/fd/<dirfd>` for the descriptor bind and the `pthread_fchdir_np` dedicated-thread branch is `cfg`-excluded there. Both branches compile, but only macOS (the dedicated-thread branch) was executed in this round; the Linux branch was not run.
- Windows runtime behavior: compile-only check. No Unix-style files are created on Windows by this slice.
- The 10,000-lifecycle stress campaign: intentionally left to the Host's independent command; this slice ran the small-scale (120 lifecycle) convergence test.
- `c2-core`/`c2-server`/SDK lifecycle integration and endpoint-protocol selection are out of scope and unchanged.
- No `git add`/index operation was attempted: the worktree index lives outside this execution workspace and this slice does not touch it. The new lines still need staging by the Host when sealing the patch.

## Integration notes

Legacy behavior is the default and no fallback between namespaces was introduced. A deployment that switches to `managed-v2` must switch every participant (server, client native library, relay upstream IPC) together; logical addresses are not interchangeable across the two protocol namespaces.
