# Windows local test sidecar verification

Date: 2026-10-10. Source baseline: `8936c5c98745a87f3c8fbb1e08782228585a519b`. Branch: `codex/local-test-sidecar`. These are Windows development-machine results, not release certification or hosted CI evidence. macOS has not been executed on a real machine.

## Implementation

The shared entry is `tools/dev/test_local.py`, with explicit `check`, `prepare` and `run` actions. It reuses the existing strict Python executor and adds Cargo, Node and repository suites. Windows uses file leases and scoped Job Objects; Unix retains flock and process groups. Existing CI entrypoints, release receipt requirements and standard-user gates remain separate. Commands and setup are documented in `docs/local-development-testing.md`.

Preparation built matched FastDB JS/WASM from clean source commit `4f99f86a662b0e950a0dd29800c25a1c9fca4def`, packed native c2-mem and TypeScript inputs, and created an isolated repository-test environment through uv. Official FastDB target archives were retained using the existing manifest/pin verification. Generated packages, libraries and build outputs are outside Git.

## Retained evidence on the test machine

| Suite | Result | Evidence directory |
| --- | --- | --- |
| Core | 1588 passed, 2 declared HTTP child entrypoints ignored | `D:/c-two-test/sidecar-core-final-4/core` |
| CLI | 60 passed | `D:/c-two-test/sidecar-all-final-2/cli` |
| Rust SDK | 18 passed | `D:/c-two-test/sidecar-all-final-2/rust-sdk` |
| Native SDK | 7 passed | `D:/c-two-test/sidecar-all-final-2/native` |
| Node bindings | 57 passed, 3 exact platform skips | `D:/c-two-test/sidecar-all-final-2/node` |
| Complete Python SDK | 1205 passed, 40 exact platform skips | `D:/c-two-test/sidecar-all-final-2/python/evidence` |
| Repository tools | 460 passed, 2 exact platform skips | `D:/c-two-test/sidecar-repo-final-3/repo` |

Across the final per-suite results, 3395 tests passed, 45 declared platform cases skipped, and 2 parent-invoked Rust HTTP child entrypoints were ignored. These counts combine the retained all-suite stages with the corrected Core and repository reruns; they do not describe a single passing aggregate all-suite command.

Python collected 1245 nodeids and verified the partition and exactly-once execution. Ordinary tests passed 1166 cases, portable tests passed 25, and TypeScript real-call tests passed 14. The 18-row portable and 12-row TypeScript receipts both validated. All seven Python collection/execution stages confirmed process cleanup. This includes the existing direct IPC, thread, relay HTTP and supported payload-format tests; no unknown skips were accepted.

Additional focused verification: platform and strict-runner regressions passed 74 tests with one declared POSIX interpreter-symlink skip; sidecar behavior tests passed 17; installer, registry, promotion and Windows CI runner checks passed 151; persistent TypeScript IPC passed once in source prebuilt-pack mode and once with supplied candidate packages. Minimum Python 3.10 syntax checks executed successfully. The full repository suite includes these tool regressions.

## Failed runs retained and investigated

`D:/c-two-test/sidecar-all-final-2/run.json` remains failed. Its Core stage encountered a Windows pipe-closing error in the test peer, and its repository stage passed 459 cases but failed the negative source-checkout fixture (2 declared platform skips). The final Core rerun passed all 1588 cases with 2 declared child entrypoints ignored, and confirmed cleanup. The final repository rerun passed 460 cases with 2 declared platform skips and confirmed cleanup. Final focused suite reruns are recorded separately; the failed aggregate is not rewritten as passing evidence.

The repository negative test was reproduced under a temporary directory whose ancestor contained `cli`. Since the production locator searches ancestors, that location did not satisfy the fixture's claimed outside-checkout precondition. The fixture now uses a system temporary directory and verifies that precondition. The reproduced single test failed before the change; all five tests in that file passed after it.

The Rust test peer initially treated Windows error 232 at a new-frame boundary as an unexpected error. A scripted AsyncRead regression reproduced that classification failure while confirming that partial-header/body errors and unrelated errors remain failures. The repair is restricted to test code. A further real-pool run identified error 232 during ACK writing. The bounded client close may abort the pipe before the test peer ACK completes. The fixture now permits only Windows peer-closed errors 109/232 after a complete DISCONNECT and still requires an independent EOF read; a live next frame or partial next frame fails. Unrelated ACK errors and all PING write errors remain failures. Controlled tests cover those distinctions, and diagnostic failures name the operation and connection. Production pool/transport code is unchanged.

Earlier failed evidence is also retained: `sidecar-all-final` rejected residual MSBuild worker processes after a successful native test; Windows child environments now disable MSBuild node reuse, and the final native stage confirmed empty process ownership. `strict-windows-1` rejected a surviving compiler helper; exact scoped vctip cleanup was verified, with wrong-path, unknown-child and live-parent cases still rejected. No general process-name kill or blanket skip was added.

## Remaining platform validation

Actual macOS execution is still required before claiming macOS machine verification. Windows results and shared-interface regression coverage do not establish a macOS pass. Local output directories are intentionally machine-specific; they are neither packaged nor committed.

Subsequent PR 181 Windows 2025 CI execution exposed an ACK-write side effect not covered by the initial fixture repair. The failure, real-pipe regression and correction are recorded in [the CI follow-up](2026-10-11-pr181-windows-2025-ci-failure.md). The original dated local results above remain historical evidence.
