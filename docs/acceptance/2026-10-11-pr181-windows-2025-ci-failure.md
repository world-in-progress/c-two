# PR 181 Windows 2025 local-platform CI failure

The [Windows Native run](https://github.com/world-in-progress/c-two/actions/runs/38105268428) for [PR 181](https://github.com/world-in-progress/c-two/pull/181) failed only in [Windows 2025 local-platform job 114369342868](https://github.com/world-in-progress/c-two/actions/runs/38105268428/job/114369342868). Both Windows full jobs, Windows 2022 local-platform, regular CI and Release Candidate checks passed. The failed artifact verified C-Two source `ec60e7dc974dfae6fa2ad24531707ea17bd4dfc2` and FastDB source `4f99f86a662b0e950a0dd29800c25a1c9fca4def`; compilation completed and the failure was a test assertion.

## Root cause

`pool::tests::real_pool_close_all_closes_active_and_idle_connections_and_reopens` failed with `peer errors: ["connection 1: new frame read: local connection aborted (raw=None)"]`. The c2-ipc unit target reported 200 passed and 1 failed.

The test peer acknowledges DISCONNECT and then independently reads the stream to prove remote EOF. Its ACK path used the inherent `LocalWriteHalf::write_all`, which invokes `write_complete`. The production `AbortOnDrop` guard intentionally aborts both local halves after any failed write, including Windows peer-closed error 109/232 during a bounded client close. The prior test repair recognized those ACK errors but then tried to read an already locally aborted observer. This is a missing case in the test fixture repair; the CI runner did not lack dependencies.

## Correction and regression

The test peer ACK helper now explicitly uses `AsyncWriteExt::write_all` and `flush`, preserving its read half after a remote-close write failure. Flush remains necessary to surface Windows overlapped-write errors. `confirm_peer_disconnect` still accepts only Windows 109/232 after a complete DISCONNECT and independently requires EOF; partial frames, a live next frame, unrelated ACK errors and PING write errors still fail. All Rust edits are inside the test module; the production abort policy and CI workflow are unchanged.

A new real Windows named-pipe regression first closes the remote endpoint and proves EOF, then attempts ACK writing, verifies the write failed without aborting the local observer, and independently confirms EOF again. Before the writer correction, it failed with `ACK failure aborted the EOF observer`; after the correction it passed. This exercises the transport wrapper side effect that the previous scripted AsyncRead checks did not cover.

## Validation

Evidence is retained outside Git at `D:/c-two-test/pr181-ci-investigation`: downloaded job log, official uploaded artifact, red/green regression logs, `real-pool-repeat` and `ci-local-platform`. Original real-pool and real-idle tests passed 20 rounds of four tests (80 test executions), with default Cargo concurrency and confirmed owned-process cleanup in every round. The real-pipe regression passed with confirmed cleanup. The complete local-platform Cargo gate passed 1511 tests with 0 failures, 0 filters and 2 declared HTTP child entrypoints ignored, in 141.625 seconds; the existing strict Cargo-summary policy verified the ignores, and process cleanup was confirmed. The tested pool.rs SHA-256 is `a3c50bc874d588dd1854f1d946beed88880884fa66e5c29e3a69d53c4fb11d3d`.

This local verification uses the same Cargo gate options as the failed CI task, including `c2-http/relay`, `--no-fail-fast` and its eleven selected packages. It is development-machine evidence. A hosted Windows 2025 rerun is required to claim that the GitHub job passes after this patch; macOS execution has not been performed here.
