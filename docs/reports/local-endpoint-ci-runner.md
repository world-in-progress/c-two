# Local endpoint validation CI trigger (scaffold only)

Date: 2026-10-06. Base: `da5ca3bec0a26a36bc50c62dd81b413a77654da5`. This change adds a small mechanical trigger for the local endpoint lifecycle work: `windows-native.yml` gains `socu/local-endpoint-validation` in its push branches, and the new `.github/workflows/local-endpoint-validation.yml` runs on Ubuntu 24.04 for that branch.

Status: **scaffold only. Nothing here has been executed.** No GitHub Actions run was started, no job output, log, JUnit file, or receipt exists yet, and no push or PR was made. The previous 15-minute CI attempt was cancelled with a clean tree and no retained evidence; this document does not supersede that record with new claims.

Design: three independent jobs (`core-test`, `focused-local`, `python-suite`), each doing its own checkout, toolchain, and dependency setup. No venv, cargo target directory, or c3 binary is shared or cached across them. Each job has a finite `timeout-minutes`, uploads `artifacts/local-endpoint-validation/` with `if: always()`, and lets a real step failure fail the job: no `continue-on-error`, no filtered exit codes, no missing-binary skip.

Coverage: `core-test` runs `cargo test --locked --manifest-path core/Cargo.toml --workspace --all-features`, which already includes the new integration tests. `focused-local` reruns the touched crates (`c2-config`, `c2-local`, `c2-local-security`, `c2-mem`, `c2-server`) with their lib and integration targets to narrow the failure surface; it deliberately does not reuse the Windows `--lib`-only `local-platform` subset. `python-suite` runs the existing complete entry point `tools/dev/test_python.py`, which enforces the strict 18-row Rust/Python matrix, the 12-row TypeScript matrix, and the ordinary suite as an exact partition of one full collection.

Inputs mirrored from existing workflows: both FastDB and C-Two are checked out as siblings at a fixed SHA (`4f99f86a662b0e950a0dd29800c25a1c9fca4def`), Python 3.12 plus an installed 3.10 for the minimum-syntax gate, Node 22, stable Rust, the official `prepare_fastdb_sdk.py` CoreSDK, `npm ci` for the FastDB and `c2-mem-ffi` TypeScript trees, and Emscripten 5.0.2 following the `release-candidate`/`ci.yml` setup. The app workspaces link FastDB from source through their own manifest patches; the runner is invoked with an explicit system CoreSDK lib directory.

Unverified until a real run: every step above is a hypothesis about the Ubuntu 24.04 runner, including the FastDB WASM build inside the TypeScript matrix and the exact Python 3.10 resolution path. The toolchain is unaware of whether `socu/local-endpoint-validation` exists remotely, so the trigger is unproven until the branch and a run both exist. Verification of the running workflow remains the Host's, through the Actions UI or `gh run`; this change does not claim an executed gate, and the workflow itself is the artifact under review, not evidence.

Locally verified: YAML parses for both workflows, `actionlint` reports no findings on either, both match the published GitHub workflow schema, and the 27 existing `tests/repo` workflow policy tests still pass. These are structural checks only.

Host 审核修正：Core/Rust SDK 使用 prepare_fastdb_sdk.py --emit-github 的真实系统库配置；Python 作业也独立准备该 SDK，原生 Python/CLI 构建显式 source 模式；c3 指向 cli/target/debug/c3。Linux 增加 Rust SDK 与 CLI 完整门禁。仍未执行 Actions，不能作为通过证据。
