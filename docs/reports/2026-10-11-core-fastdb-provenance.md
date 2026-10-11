# PR 181 Core sibling FastDB provenance repair

Review baseline: `efdc684730c72dc670ad9b90b84570d4efcb4d19`, [P2 comment 4240512004](https://github.com/world-in-progress/c-two/pull/181#discussion_r4240512004). Core code generation tests consume the actual sibling FastDB `ts/fastdb4ts/src/payload` tree in addition to its TypeScript compiler. Fingerprinting only the compiler did not verify the required source pair.

Core/all check, prepare and run now capture the actual sibling checkout identity and reject incorrect HEAD, dirty tracked or untracked source, a missing payload entrypoint, or a directory which is not the Git checkout root. Linked worktrees are accepted. `--fastdb-source` remains the separate WASM preparation source and cannot substitute for the Core sibling. The runner records the sibling path, HEAD, tracked diff/content hashes, status, index flags and payload entrypoint hash, then compares the final identity. It validates again under the sibling resource lease immediately before and after Core consumption, and before preparation execution.

Independent review identified another cleanliness boundary: `assume-unchanged` and `skip-worktree` can hide tracked payload edits from Git status. Two real Git regressions reproduced passing preflight with such edits before the additional fix. The runner now rejects those index flags. Final independent review reported no remaining actionable findings.

All implementation changes are in the development sidecar. No production transport, connection pool, payload implementation or CI workflow changed. No test skips, retries or relaxed assertions were added.

## Windows verification

These checks exercised the uncommitted repair on baseline `efdc684`, with source identities retained in the local reports; documentation was completed after the test runs. The actual FastDB sibling was `D:/c-two-test/fastdb` at `4f99f86a662b0e950a0dd29800c25a1c9fca4def`. Prepare and run both captured 750 source files, empty worktree status and no hidden index flags; their before/after identities matched. C-Two source identities also matched within each run. All final owned-process reports confirmed cleanup.

| Check | Result | Evidence under `D:/c-two-test/pr181-core-provenance/` |
| --- | --- | --- |
| Original regression set before implementation | 13 failed, exposing missing source rejection/records | Initial pytest console output |
| Focused sidecar regressions before adding the hidden-index cases | 70 passed | `focused-3.xml` |
| Hidden tracked edits before their rejection fix | 2 failed as expected | `hidden-red.xml` |
| Complete repository regressions, including all 26 new source-boundary cases | 499 passed, 2 expected skips | `repo-run/run.json`, `repo-run/repo/junit.xml`, `repo-run/repo/output.log` |
| Real Core preparation | Passed; sibling source unchanged | `core-prepare/run.json` |
| Real full Core execution | 1590 passed, 2 expected child-process fixture entrypoints ignored | `core-run/run.json`, `core-run/core/output.log` |
| Changed runner and two test files compiled by Python 3.10 | 3 files passed | Local Python 3.10 compile command |

An early focused invocation had a missing parent directory for pytest basetemp; another had an overly broad fake external-tool response for the unrelated Python preflight in suite all. Both diagnostic fixture setup issues were corrected before the passing focused and complete runs. They were not counted as passing verification. The first temporary-directory run also emitted an existing Windows pytest temporary-symlink cleanup warning; final runs used explicit new basetemp directories.

The regression set covers core/all preflight, preparation rejection, dirty/untracked and wrong-revision siblings, source mutations after preflight and during Core execution, prepare-time mutation with a failed JSON report, linked worktrees, and hidden Git index flags.

## Follow-up

Push the local commit to the existing `codex/local-test-sidecar` PR branch and inspect CI for that exact commit. This report establishes local verification, not hosted CI success. The earlier Windows native heap corruption remains a separate unresolved investigation documented in `2026-10-11-windows-ci-investigation.md`; this source-provenance repair does not claim to fix it.
