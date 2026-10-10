# Local test sidecar implementation plan

> Execution: use subagent-driven-development for independent platform and fixture work; retain task review and final verification. This document is the authoritative progress record.

**Goal:** Share macOS/Unix local testing with a native Windows developer sidecar while preserving CI gates and evidence semantics.

**Architecture:** Keep `tools/dev/test_python.py` as the shared strict Python executor and compatible pytest plugin. Extract OS operations into `tools/dev/test_platform.py`; add `tools/dev/test_local.py` for explicit check/prepare/run and wider suite scheduling. CI remains independent.

**Tech stack:** Python 3.10+, uv, pytest/xdist, ctypes Windows Job Objects, Cargo, npm, official FastDB CoreSDK preparation.

## Global constraints

- No unknown skip, missing test, failed preparation, timeout or residual process may count as passed.
- Node 22 with Node-API >=10; FastDB source commit `4f99f86a662b0e950a0dd29800c25a1c9fca4def`; paired JS/WASM artifacts.
- Source linking for CLI and Python builds; system linking for Rust portable harnesses; session-only loader paths.
- Preserve existing macOS CLI and plugin identity; preserve CI scopes, release receipts, standard-user and required-gate checks.
- No generated packages or native build artifacts in Git. Windows tests run here; macOS real-machine evidence remains explicitly pending.

## Task 1: platform backend and strict executor integration

Files: create `tools/dev/test_platform.py`, `tests/repo/test_local_test_platform.py`; modify `tools/dev/test_python.py`, `tests/repo/test_python_test_runner.py`.

Interface: `resource_lease(resource)` coordinates stable resources across runs; `ProcessTree(command, cwd, env, stdout)` owns launch, `.process`, `.alive()`, `.stop()`, `.close()`; `run_step` retains existing record schema. Windows launches suspended, assigns a kill-on-close Job Object, then resumes before executing user code; Unix keeps session/process groups. `core_library_name(platform)` and `validate_node_runtime(version, napi)` are shared checks.

- [x] Add real subprocess tests for normal exit, orphan descendants, finite timeout, competing locks and release. Run `.venv/Scripts/python.exe -m pytest --confcutdir=tests/repo tests/repo/test_local_test_platform.py -q`; observe missing behavior fail before implementation.
- [x] Implement lazy platform imports and integrate the backend; replace blanket POSIX marker with markers on Unix-specific cases. Pure runner tests must execute on Windows.
- [x] Verify strict collection/partition, unknown skip rejection, receipts and synthetic real pytest execution. Exit: focused backend and runner regression tests pass, with no weakening of validation.

## Task 2: shared Windows fixture repairs

Files: `tools/local_rc/local_registry.py`, corresponding repo tests, POSIX installer/promotion tests and TypeScript persistent IPC fixture.

- [x] Reproduce Windows separator rejection and CRLF/shebang failures with focused tests; use `python -m pytest --confcutdir=tests/repo tests/repo -q` or exact failing files for red/green verification.
- [x] Fix slash variants, force LF for generated POSIX artifacts, locate Git Bash correctly and provide shell-readable Python/PATH; install npm tarball dependencies when directory symlink permissions are unavailable, preserving candidate mode.
- [x] Verify formerly failing cases and existing CI runner tests. Exit: these compatibility failures are fixed and original assertions remain covered.

## Task 3: shared sidecar entry and preparation

Files: create `tools/dev/test_local.py`, `tests/repo/test_local_test_runner.py`, `docs/local-development-testing.md`.

Interface: `python tools/dev/test_local.py {check,prepare,run} --suite {core,cli,rust-sdk,native,python,typescript,node,repo,all} --output PATH`, optional Python/c3/CoreSDK/tool/package paths. `check` runs capability probes and fails clearly on missing prerequisites. `prepare` invokes existing validated SDK preparation and project build tools in ordered stages, creates matched npm tarballs in a temporary FastDB source copy. `run` uses the strict Python executor for the complete SDK; Cargo/npm/repo suites use scoped process trees and separate step reports. Outputs are exclusively development evidence.

- [x] Add tests asserting Node-API 9 rejection, selected-suite prerequisites, suite commands/link modes, immutable/new output directories, failed preparation propagation and all-suite scheduling; run them red before implementation.
- [x] Implement explicit preparation with resource leases and bounded subprocesses, no implicit installs in run, and UTF-8/loader environment isolated to children. Retain source provenance and hashes in reports.
- [x] Document macOS and Windows commands, tool selection, preparation, full and focused suites, evidence limitations and cleanup semantics. Exit: check/prepare/run behavior tests and actual Windows targeted suites pass.

## Final verification and delivery

- [x] Run platform/local runner/CI regression tests, complete repo tests, TypeScript real-call integration, persistent IPC and strict Python suite with finite timeouts; record actual counts and unresolved failures.
- [x] Review complete diff for CI behavior drift, process leaks, mutable artifacts and misleading evidence. Update this plan with commands and outcomes. Deliver branch/path, commands and actual verification scope; do not claim macOS execution.

## Progress

2026-10-10: existing clean clone fetched; upstream remains `8936c5c`. Branch `codex/local-test-sidecar` created. Design approved; implementation starting.

Task 1 implemented: shared platform backend and strict executor, real Windows Job Object/locks, Windows relay fixture serialization, selected Node/npm/CoreSDK handling, frozen exact platform skip lists. Focused platform/strict regression after compiler-helper policy: 74 passed, 1 POSIX interpreter-symlink skip. Python 3.10 syntax checks passed. The original Windows baseline skipped all 42 runner tests.

Task 2 implemented and reviewed: 151 installer/registry/promotion/Windows CI runner checks passed, with one expected duplicate-ZIP warning. Persistent TypeScript IPC passed in source prebuilt-pack and supplied-candidate modes, one test each. Added explicit `C2_DEV_FASTDB_RELEASE_FIXTURES` configuration for the two real official release tests; synthetic CI tests retain their independent inputs.

Task 3 implemented and reviewed: 17 sidecar behavior tests passed after observed red cases for absent entry, unknown JUnit/TAP skips, unknown Rust ignores, missing supplied tarballs, fixture preparation and foreign Cargo target inheritance. Preparation successfully built paired FastDB JS/WASM in an independent pinned clone, compiled c2-mem native runtime and packed all three npm inputs. Repository preparation created an isolated uv environment; run directly uses `--repo-python` and performs no dependency installation. Official three-target release archives were downloaded and verified through the existing public-pin helper.

Evidence-driven lifecycle clarification: a complete ordinary SDK execution produced 1166 passed and 40 declared Unix-only skips, but remained failed because its owned MSVC vctip helper stayed alive. Scoped compiler probes reproduced this helper; environment suppression guesses were rejected by actual behavior. The platform backend now explicitly terminates the helper only after its parent exits, if every remaining Job-owned process is the exact vctip executable beside the selected cl compiler; it confirms empty ownership and records the cleanup. Wrong-path, mixed unknown child and live-parent regressions remain rejected. Unknown process leakage and unconfirmed cleanup still fail.

Strict Windows SDK final verification passed in `D:/c-two-test/sidecar-all-final-2/python/evidence`: 1245 collected nodeids, 1205 passed calls and 40 exact platform skips; ordinary 1166, portable 25, TypeScript 14. Both 18-row portable and 12-row TypeScript receipts passed, with all collection/execution stages confirming cleanup. CLI 60, Rust SDK 18, native 7 and Node 57 passed (3 exact Windows platform skips). The aggregate all-suite report remains failed because Core encountered Windows pipe-closing error 232 in its test peer and the repository negative fixture was nested below an ancestor `cli` directory. These failures are retained; they cannot be relabeled passed. The repository fixture was reproduced red and corrected using an actual system temporary directory; its five tests passed. A frame-boundary-only Windows EOF regression is being verified for the Rust test peer. Both final reruns and complete diff review are now recorded below. No actual macOS run is claimed.

Final Core rerun: `D:/c-two-test/sidecar-core-final-4/run.json` passed, 1588 cases and 2 declared HTTP child entrypoints, no filters and confirmed cleanup. Five new scripted peer tests cover frame-boundary closure, partial frames, unrelated errors, ACK peer closure requiring independent EOF, and ACK error handling. A subsequent real-pool red run exposed ACK-write error 232; the test peer now follows bounded-close semantics without treating ACK delivery as guaranteed. Only Windows 109/232 after complete DISCONNECT qualify, and the next read must prove EOF; live/partial frames and PING errors still fail. All Rust edits are confined to the test module. Final repository rerun `D:/c-two-test/sidecar-repo-final-3/run.json` passed 460 cases, 2 declared skips and confirmed cleanup. Final read-only review approved the strict EOF and bounded ACK distinction; no blocking findings remain.

Delivery verification completed: the seven final suite results total 3395 passed, 45 exact platform skips and 2 parent-invoked HTTP child entrypoints ignored. The initially failed aggregate remains failed; corrected Core/repository results are separate development evidence. The retained machine paths, source pair, failure investigations and macOS limitation are summarized in `docs/reports/2026-10-10-local-test-sidecar-windows.md`. Generated artifacts are excluded from Git, and the branch is retained locally.
