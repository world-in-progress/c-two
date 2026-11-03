# PR 181 Windows CI investigation and drain readiness repair

## Hosted evidence

The failing source is `05f4d869f0c4667208935b5fb222cdb9ebd0b113`, Windows Native [run 38111076176](https://github.com/world-in-progress/c-two/actions/runs/38111076176). The initial run passed Windows 2022 full and both local-platform jobs; Windows 2025 full failed. Only the failed job was rerun on the same source, without an intervening code change. Both attempts failed, with different failures.

| Attempt | Failure | Other relevant result |
| --- | --- | --- |
| 1 | Ordinary Python process exited with `3221226356` / `0xC0000374` (`STATUS_HEAP_CORRUPTION`) during `test_grid_python_payloads_work_explicit_http_relay`; the Python stack ends at native `RuntimeSession.register_route` | Windows 2022 ordinary Python passed 1166 tests, skipped 40; Windows 2025 portable and TypeScript stages passed |
| 2 | `pool::tests::concurrent_close_all_drains_serialize_and_acquires_reject_during_blocked_drain` failed with `acquire during active drain must reject`; c2-ipc library result was 201 passed, 1 failed | Windows 2025 ordinary Python passed 1166 tests, skipped 40; TypeScript stage passed |

The first stack locates corruption detection, not necessarily the operation that originally damaged the heap. No native crash dump was retained. The original heap failure remains unresolved; a passing Python rerun does not establish a fix. A recurrence needs a native crash dump from the hosted runner before choosing a runtime repair.

The diff from `916dacf1e0c91ec70bab496b45e0cf24d195e3ad` to the failing source has no changes under `sdk`, `core`, `cli`, `.github/workflows`, or `tools/ci`. The full CI job does not invoke `tools/dev/test_local.py`. This gives no direct evidence that the four preceding sidecar review corrections caused a runtime regression, and does not establish an environmental root cause either.

## Verified readiness defect and repair

`close_all` announces the coordinator's `active_generation` before setting `state.closing_generation`. Acquisitions reject based on the latter fence. The old test helper waited only for the announcement, so it could return while an acquisition was still allowed. The concurrent drain test then asserted that this acquisition must reject.

A new regression explicitly sets the announcement without the acquisition fence. It failed with the original helper and passed after the repair. The helper now waits for the same closing fence that acquisitions check. It never holds the coordinator lock while reading state, preserving the production lock order, and uses bounded condition-variable waits to handle missed notifications.

Both modified Rust regions are under `cfg(test)`. Production acquisition, close, timeout, and connection behavior are unchanged. No assertions were removed, no deadlines were increased, and no tests were newly skipped. Independent review found no actionable defects. The hosted log cannot prove the premature readiness window was the sole cause: the existing three-second drain deadline also bounds how long a delayed probe can observe the fence.

## Windows verification

Local tests ran on a Windows development host, not the hosted Windows Server 2025 image. Diagnostic processes used the sidecar's owned-process execution and cleanup. The Python environment matching CI used Python 3.12.10 and locked dependencies.

| Verification | Result | Local evidence under `D:/c-two-test/pr181-ci-investigation/` |
| --- | --- | --- |
| New readiness regression before repair | 0 passed, 1 failed as expected | `drain-readiness-red/rust-test/output.log` |
| c2-ipc library after repair, `cargo test --locked --manifest-path core/Cargo.toml -p c2-ipc --lib` | 203 passed, 0 failed; cleanup confirmed | `drain-library-final/run.json`, `rust-test/output.log` |
| Affected concurrent drain test, 20 separate process rounds from that test binary | 20/20 passed; all cleanup confirmed | `drain-repeat-20/run.json` |
| Three Grid tests alone | 3 passed | `grid-repro-1` |
| Ordered integration prefix through Grid, three separate process rounds | Each round 132 passed, 11 skipped | `prefix-rounds` |
| Full ordinary Python suite with Python 3.12.10 | 1166 passed, 40 skipped | `python31210-run-2` |
| Same suite with CI's `uv run --no-sync pytest` startup | 1166 passed, 40 skipped | `python31210-exact-run` |
| Three Grid scenarios repeated 30 times in one Python 3.12.10 process | 90 passed | `grid31210-stress` |

An earlier local Python attempt lacked the CoreSDK environment variable and ran no tests; the environment was corrected before both full-suite results. An earlier library execution passed all 203 Rust tests, but its wrapper failed process cleanup because the diagnostic script lost the selected compiler PATH through case-duplicate environment keys. The external script was corrected and the final library execution passed with cleanup confirmed. Neither diagnostic setup failure was suppressed or counted as a passing run.

No heap corruption reproduced locally. The code repair addresses the independently reproduced test readiness defect only. Hosted CI for the new repair commit still needs to run after pushing to the existing PR branch; this report does not claim hosted CI passes.

## Remaining work

Push the local repair to `codex/local-test-sidecar` and inspect the resulting Windows 2025 full run. If heap corruption recurs, retain a native crash dump with its exact source, runner, Python, and native dependency identities. If the drain assertion recurs, inspect probe scheduling relative to the drain deadline before changing production behavior or timeouts.
