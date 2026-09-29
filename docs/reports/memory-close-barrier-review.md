# Windows 2025 blocked receive close review

Source: Buddy checkout based on `0075bf9c54ba3a536c5dc1ae2c9bb65be1155648`. Scope: `c2-ipc` close barrier and its focused fixtures. No `c2-local` change.

## Known evidence and cause

`/tmp/c2-win2025-local-tests-0075.log` records `pool::tests::close_shared_aborts_blocked_receiver_within_deadline` failing at `pool.rs:1818` (`confirmed=false`), with 127 passed and 1 failed. Other hosted jobs passing does not settle this failure. The previous fixture waited for the fake peer to write two prefix bytes, but never established that the client's receive task had consumed them or even been polled. That was a fixture readiness gap.

`SyncClient::close_shared` forwards to `IpcClient::close_shared_bounded` on the shared two-worker runtime. `do_handshake` spawns `recv_loop` and stores its handle. The receive loop owns the `LocalReadHalf` while reading a frame header. `c2-local` uses Tokio's split stream and a short-held stream mutex per poll; a pending read does not hold the mutex between polls. The close gate serializes close barriers. The old receive join waited until the *entire* absolute deadline, then aborted the stream and task, and attempted a second join using only the deadline's remaining time. At that point the remaining time can be zero, so `confirmed` depends on whether the aborted task has already been scheduled to finish. This is a production barrier race, independent of the fixture readiness gap.

The fix reserves up to one second of the *existing* deadline for receiver abort and join, limited by the time left after maintenance and disconnect writing. It does not extend the caller's total timeout. If prior phases consume the budget or the task still cannot join, close remains unconfirmed and keeps the handle for retry. The owner-pool detach gate, pending request release, lease authority, and budget accounting are unchanged.

The real half-frame fixture now arms a test-only probe before connect. It reports ready only after the same receive task reads the two prefix bytes and polls the remaining header read to `Pending`. A 25 ms test-only cancellation drop delay makes a zero-budget post-abort join fail consistently; it is not used to infer receive readiness. A separate pure test injects a pending receiver handle with finite cancellation latency to isolate the close budget rule without SHM.

## Local evidence

All local commands set `CARGO_BUILD_JOBS=2`, `CARGO_TARGET_DIR=/private/tmp/c2-close-barrier-32e9`, `C2_ENV_FILE=''`, and `C2_RELAY_ANCHOR_ADDRESS=''`.

| Check | Result |
| --- | --- |
| `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib --no-run` | Compiled; exit 0 |
| `cargo check --manifest-path core/Cargo.toml -p c2-ipc` | Production configuration compiled without warnings; exit 0 |
| `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib client::tests::close_reserves_time_to_join_an_aborted_receiver -- --exact` | 1 passed; exit 0 |
| Same pure test with only the old receiver wait line restored temporarily | 1 failed, expected `confirmed=false`; exit 101. Fix restored and same test passed again. |
| `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib pool::tests::test_pool_grace_period_not_expired -- --exact` | 1 passed; exit 0 |
| `rustfmt --check --edition 2024` on `client.rs` and `sync_client.rs`; `git diff --check` | Exit 0 |

`cargo fmt --manifest-path core/transport/c2-ipc/Cargo.toml --check` reports pre-existing formatting differences in `pool.rs` outside this edit; the file was not wholesale reformatted. The SHM/local IPC test was deliberately not run in this sandbox, where those facilities are denied.

## Host continuation and concurrent-close correction

The Host's `/tmp/c2-close-host-validation-0929/affected-rust.log` shows the repaired half-frame test and pure close-budget test passing. It also shows `two_concurrent_closes_of_one_blocked_receiver_serialize_and_stay_retryable` failing because both closes legitimately confirmed in about one second. Its old `false` assertion depended on consuming the whole two-second deadline and no longer tested an actually nonterminal receiver. The Host reports the other SDK, CLI, Python, portable, TypeScript, and memory-matrix gates passed; those unrelated gates were not rerun here.

The concurrent fixture now uses the same connection-scoped half-header Pending probe, plus a connection-scoped test-only cancellation Drop gate. The guard checks that this connection's stream abort handle was actually aborted before it signals entry or blocks; a natural receive exit cannot satisfy the gate. Until the test releases it, that task cannot finish its JoinHandle, so **both** bounded closes must report `false` even if one caller temporarily owns the handle under the close gate. Releasing the gate permits a subsequent close to join the restored handle and confirm. Dropping the release sender also opens the gate on test failure; a failed connection attempt never installs a blocking Drop guard. The normal half-frame test installs no gate and still expects `confirmed=true`. No production close, lease, owner, budget, or `c2-local` code changed in this continuation.

This continuation used `CARGO_TARGET_DIR=/private/tmp/c2-close-barrier-33ae` with two build jobs and empty `C2_ENV_FILE` / `C2_RELAY_ANCHOR_ADDRESS`. `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib --no-run` compiled, the pure `client::tests::close_reserves_time_to_join_an_aborted_receiver` passed (1/1), and `rustfmt --check` on `client.rs` and `sync_client.rs` plus `git diff --check` passed. The actual concurrent IPC test remains for Host validation with SHM/native pipe access.

## Host validation needed

Run against this exact checkout contents on the Windows 2025 runner with SHM and native pipe access. Use an independent temporary Cargo target, two build jobs, and empty `C2_ENV_FILE` / `C2_RELAY_ANCHOR_ADDRESS`:

```powershell
$env:CARGO_BUILD_JOBS = '2'
$env:CARGO_TARGET_DIR = Join-Path $env:TEMP 'c2-close-barrier-33ae'
$env:C2_ENV_FILE = ''
$env:C2_RELAY_ANCHOR_ADDRESS = ''
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib pool::tests::two_concurrent_closes_of_one_blocked_receiver_serialize_and_stay_retryable -- --exact --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib pool::tests::close_shared_aborts_blocked_receiver_within_deadline -- --exact --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib pool::tests::close_shared_is_bounded_and_honest_when_writer_slot_is_held -- --exact --nocapture
```

Acceptance: the three focused tests pass from the patched source, with exit codes and full logs retained. The concurrent test must see the same receive task's Pending and cancellation-Drop events, both closes must return bounded `false` before gate release, and retry must confirm after release. The normal half-frame test must still confirm; the writer-slot test must remain unconfirmed while held and confirm after release. If any fails, return its full log and the exact source fingerprint; unrelated green jobs cannot substitute for this gate.

Host 本机集成验证：生产关闭修复经过 Rust runtime/transport、Rust SDK、CLI、native 重建、Python suite、18/12 行严格跨语言矩阵与9行内存矩阵。首次仅旧并发夹具的至少一个 false 断言失败；按事件化改写后，完整 c2-ipc 130项全部通过，其中真实半帧、writer-slot、并发 gate 测试均通过。原始记录为 `/tmp/c2-close-host-validation-0929/` 和 `/tmp/c2-close-concurrent-host-0929/`。新的 Windows 源码验证待本提交后的 CI，不把本机结果当作 Windows 通过。
