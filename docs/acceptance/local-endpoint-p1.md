# Local endpoint P1 native slice

Status: the `c2-local` native endpoint layer exposes v1 inspection, exact credentials, identity-checked reap, bind-time rollback, and an explicit bounded sweep. This is a native-layer slice only; runtime shutdown and higher-level SDK integration remain incomplete, so it is not evidence that the full P1 phase is complete.

Baseline: source commit `0f72d072e3f4f5aa3c51c8251c15d306fc1cafd0`. Two execution environments were used, and their results are reported separately. In the earlier sandboxed attempt, `cargo test --manifest-path core/Cargo.toml -p c2-local --lib` failed with 14 failures at `EPERM` while existing tests attempted local endpoint operations under the shared `/tmp/c_two_ipc` namespace; that environment also rejected a direct Unix-socket bind probe under `/private/tmp` and under the allocated checkout. That failure record is kept below because it is the reason the earlier revision could not be runtime-verified. The current repair round ran on the DSH command-capable route, where local AF_UNIX bind is permitted, and the results in the "Validation results" table supersede the blocked attempt for the listed commands.

## API and behavior

| API | Behavior |
| --- | --- |
| `inspect_endpoint(&LocalEndpoint)` | Read-only filesystem observation; returns `Absent`, `Present(EndpointCredential)`, `Unverified`, `KernelManaged`, or `IoError`. It does not create ownership files or connect to the service. |
| `reap_endpoint(&LocalEndpoint, &EndpointCredential)` | Nonblocking reap limited to the credential's logical endpoint and recorded v1 device/inode/ctime identity; returns `Reaped`, `AlreadyAbsent`, `Busy`, `StaleTarget`, `Unverified`, `NotApplicable`, or `IoError`. A credential for another endpoint is `StaleTarget` on every platform. |
| `LocalListener::credential()` | Exposes the exact endpoint credential while the listener is owned. |
| `LocalListener::close(self)` | Explicitly closes with a structured result; Drop keeps its existing best-effort cleanup ordering. |
| `EndpointSweep::open()` | Opens the default managed namespace derived from `LocalEndpoint` authority (the parent directory of a `LocalEndpoint` OS name). A missing namespace is an error and is never created by a sweep. |
| `EndpointSweep::for_endpoint(&LocalEndpoint)` | Opens the managed namespace that owns an explicit endpoint. Replaces the previous test-only way to target a real namespace. |
| `next_batch(SweepBudget)` | Explicitly driven, stateful sweep with a retained directory iterator, one active sweep per process, per-batch entry and time budgets, bounded counters, and a bounded last OS-error sample. A new sweep starts an incomplete round. |

Unix inspection and cleanup pin the namespace directory and perform entry lookup, ownership-file opens, and socket unlink relative to its directory fd. Namespace, socket, and ownership entries are checked for current-user ownership and expected object type; directory and ownership opens use `O_NOFOLLOW`, and entry checks use `AT_SYMLINK_NOFOLLOW`. Reaping takes a nonblocking exclusive `flock`, rechecks both recorded socket identity and the current directory entries, and unlinks only the socket. The existing v1 `.lock` inode is never removed. Unknown, malformed, or mismatched records are retained as `Unverified` or `StaleTarget`; age, PID, and failed connection probes do not authorize reaping. A real unlink failure is returned as `IoError(io::Error)` and summarized in sweep batches by kind and raw OS error.

Bind-time stale replacement and explicit reap share the existing v1 record, ownership lock, and identity-checked unlink implementation. Advisory connect probes remain denial-only. The listener still cleans the socket before its native listener field drops, and releases its lock after the listener closes.

A successful `bind` is now followed by a rollback guard that runs while the same instance holds the v1 ownership lock and before the listening descriptor closes. If socket permission setting, the following stat, or the owner-record write fails, the guard withdraws the entry it created. The guard compares the directory-entry device/inode/type/owner captured through the verified dirfd (`fstatat`), not the listening descriptor's inode and not the full record identity, because `fchmodat` rewrites ctime before the record is written. It never reads the owner record it may have failed to write, and a foreign replacement object is left untouched (`StaleTarget`). This closes the regression where a failed initialization left a socket pinned at a fixed address.

New namespaces are created with an explicit `0700` mode through `DirBuilder`, so a permissive process umask (`002`/`000`) cannot produce a group- or world-writable directory that the same module then refuses to open. Pre-existing directories are never chmod-ed: an unsafe existing directory is still an explicit `UnsafeDirectory`/`PermissionDenied` error.

Sweep rounds that end because the verified namespace directory was replaced now report `round_interrupted: true` and `round_complete: false` on that batch and on every later batch of the same sweep. A caller that only checks `round_complete` cannot mistake an interrupted round for full coverage; `namespace_changed` stays true on those terminal batches.

Windows reports `KernelManaged` inspection and `NotApplicable` reap/sweep after checking credential/endpoint mismatch; `KernelManaged` describes platform ownership and is explicitly not evidence that a live instance exists. Windows creates no Unix-style files and does not claim to collect live kernel handles.

## Incomplete integration and risks

- `c2-core`, `c2-server`, Rust SDK, and Python/PyO3 do not yet consume listener credentials or explicit close outcomes. Their lifecycle integration is outside this slice and remains required before describing P1 as complete.
- The sweep is synchronous and explicitly invoked. Its caller must schedule it on an appropriate blocking worker; it is not an automatic startup or request-path task.
- Sweep and reap still act on the v1 namespace (`/tmp/c_two_ipc` on Unix). Versioned namespace isolation and lock-file retirement are P2 and are not implemented here.
- The rollback guard cannot act when the identity capture itself fails (for example a hard filesystem error immediately after `bind`); in that case the entry is intentionally left in place and remains visible as `Unverified` rather than being removed without proof.
- Windows received a cross-target compile check only; named-pipe lifecycle behavior was not executed on Windows.
- All new files still need to be staged in the Host-managed Git index: the worktree index lives outside this execution workspace (`…/c-two/.git/worktrees/checkout2/index.lock`), and the sandbox denied both the direct `git add` and the escalation request. The workspace snapshot still records the new files.

## Validation results

Current repair round, macOS 26.6.2 (Darwin 25.6.0, arm64), rustc/cargo 1.91.0, `CARGO_BUILD_JOBS=2`, normal parallel test harness (no `--test-threads=1`):

| Command | Result |
| --- | --- |
| `cargo test --locked --manifest-path core/Cargo.toml -p c2-local --lib` | Passed: 29 passed, 0 failed, 0 ignored. Repeated three times with the same result. The `platform::tests` fixtures now use short random `0700` roots under `/tmp` (`tempfile`), UUID-bounded endpoint names, and assert the final socket path is below the `SUN_LEN` budget; `/tmp` had no leftover `c2l1-*` roots afterwards. The pre-existing `tests::*` listener fixtures keep their original behavior of creating only their own uniquely named sockets and cleaning them up. |
| `platform::tests::active_reap_is_busy_and_normal_disconnect_keeps_listener_available` | Passed: live listener is `Busy`; two connect/disconnect cycles leave the service reachable. |
| `platform::tests::crashed_owner_can_be_reaped_twice_without_removing_v1_lock` | Passed: no-destructor child exit, exact `Reaped`, then `AlreadyAbsent`; the `.lock` file remains. |
| `platform::tests::old_credential_cannot_remove_a_new_listener_instance` | Passed: stale credential returns `StaleTarget` and the new socket survives. |
| `platform::tests::inspect_and_reap_leave_unknown_and_corrupt_records_untouched` | Passed: `MissingOwnership` and `InvalidRecord` are reported and never reaped; reap creates no lock file. |
| `platform::tests::symlink_endpoint_is_refused_without_following_its_target` | Passed: `Symlink`, target file content unchanged. |
| `platform::tests::unknown_stale_socket_requires_explicit_cleanup` | Passed (restored original name and semantics): unregistered raw socket is refused with `AddrInUse` and its device/inode/ctime identity is unchanged. |
| `platform::tests::probing_a_full_backlog_never_waits_for_accept` | Passed: the negative backlog case still returns `AddrInUse` within the 1 s probe deadline. |
| `platform::tests::failed_initialization_withdraws_its_socket_and_allows_rebinding` | Passed: injected socket-permission and record-write failures withdraw the socket, keep the `.lock` file, and allow the same fixed address to bind again. |
| `platform::tests::rollback_never_removes_a_replacement_object` | Passed: injected failure after a foreign replacement leaves the replacement file intact, and bind keeps refusing with `AddrInUse`. |
| `platform::tests::new_namespace_directory_ignores_permissive_process_umask` | Passed: child processes with umask `002` and `000` create a `0700` namespace. The child owns the umask; the parallel harness never changes its own. |
| `platform::tests::budgeted_sweep_advances_failures_and_finishes_a_stable_round` | Passed: one-entry batches keep advancing past `Busy`/`Unverified` entries, later valid candidates are reaped, and the stable directory finishes a round. |
| `platform::tests::interrupted_sweep_never_reports_a_completed_round` | Passed: after the namespace directory is replaced, the round stays `round_interrupted` and never reports `round_complete`. |
| `platform::tests::default_sweep_namespace_comes_from_the_local_endpoint_mapping` | Passed: the default sweep namespace is the `LocalEndpoint` parent directory, not a second hardcoded path. |
| Mutation check (temporary, reverted) | Disabling the rollback guard makes `failed_initialization_withdraws_its_socket_and_allows_rebinding` fail; restoring `create_dir_all` makes the umask test fail with `mode 775`. The new tests are not vacuous. |
| `cargo check --locked --manifest-path core/Cargo.toml -p c2-local --target x86_64-pc-windows-msvc --tests` | Passed: Windows target and its `cfg(windows)` assertions compile. |
| `rustfmt --edition 2024 core/transport/c2-local/src/{lib,unix,unix_endpoint,windows}.rs` | Passed. |

Historical blocked attempt (same baseline, restricted sandbox), kept as the original failure record:

| Command | Result |
| --- | --- |
| `cargo test --manifest-path core/Cargo.toml -p c2-local --lib` (pre-change baseline, restricted sandbox) | Failed: 14 failed, 2 passed; endpoint operations returned `EPERM` in that sandbox. A direct Unix-socket bind probe also returned `EPERM` under both `/private/tmp` and the allocated checkout. |
| `cargo test --manifest-path core/Cargo.toml -p c2-local --lib` (previous revision, relative checkout target root) | Failed: 13 passed, 9 failed; every failure was `path must be shorter than SUN_LEN` because the relative target path plus UUID endpoint names exceeded the macOS sockaddr bound. |
| `cargo check --manifest-path core/Cargo.toml -p c2-local --target x86_64-pc-windows-msvc` | Passed in that environment as well. |

Remaining host steps: stage the listed files in the Host-managed Git index, and review the diff. No `c2-core`/`c2-server`/SDK integration is included or claimed.
