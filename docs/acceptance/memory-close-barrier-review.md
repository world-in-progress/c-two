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

This continuation used `CARGO_TARGET_DIR=/private/tmp/c2-close-barrier-33ae` with two build jobs and empty `C2_ENV_FILE` / `C2_RELAY_ANCHOR_ADDRESS`. `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib --no-run` compiled, the pure `client::tests::close_reserves_time_to_join_an_aborted_receiver` passed (1/1), and `rustfmt --check` on `client.rs` and `sync_client.rs` plus `git diff --check` passed. The concurrent IPC test was then handed to the Host; its completed results are below.

## 最终验证与边界

Host 集成环境的 `/tmp/c2-close-concurrent-host-0929/result.json` 记录 `ipc-full` 退出码 0；相应日志显示 c2-ipc 130/130 通过，三个具名关闭测试均为 `ok`。这份本机结果的来源记录是基线加运行时补丁，须与下列 Windows 源码提交分开表述。

GitHub Windows Native `36552609987` 的四份 `run-evidence.json` 均标记 C-Two 实际源码为 `abfeaf71deaa9ff4428d0c043ab80e3210746f47` 且状态为 `passed`。Windows 2022 与 2025 的 `local-platform-tests.log` 分别显示 707 项通过，其中 c2-ipc 为 129/129；两份日志均明确记录半帧关闭、writer-slot 关闭、并发关闭三个具名测试为 `ok`。两个 `full` scope 各有 23/23 个成功步骤。

`/tmp/c2-memory-windows-abfe/verification.txt` 以 `PASS: 0 failure(s)` 收尾，并记录严格 18 行 Rust/Python、12 行 TypeScript、9 行 IPC memory 矩阵，以及普通 wheel 消费者、测试 XML 和 c3 字节一致性核验。该文件还核对了解包内容的内部哈希及所给 ZIP 摘要；Host 另行报告 ZIP 摘要与 GitHub API 一致、非管理员 wheel 消费及进程账号目录清理通过。以上是 Windows Server 2022/2025 的该提交证据；Windows 11 未执行，RC 未发布。
