# Windows CI current failures — diagnostic probe 2026-09-25

Historical probe for run 36017573157. The later fixes and successful run 36034972057 are recorded in [final validation](windows-native-final-validation.md).

Probe only: no fixes, builds, downloads, or reruns were attempted in this investigation. Windows adaptation was not complete at this snapshot.

Observed HEAD: `b39bc20f12e8603ec38fa594ce7109343f072761` (`git rev-parse HEAD` ran; `git status --short` ran and returned empty — clean tree at the frozen input commit).

Source log inspected: `/tmp/c-two-windows-36017573157-failed-console.log` (1,095,380 bytes, mtime 2026-09-25 00:32 local). It exists and is nonempty; it contains exactly two GitHub Actions jobs, both `full / MSVC x64`, run 2026-09-24 15:05–15:38 UTC.

## windows-2022 / full / MSVC x64 — 1 of 22 gates failed

- FAILED gate: `core-test` (exit 101). All other 21 gates passed.
- Failing binary: `cargo test -p c2-core --test client_modes` (12 passed, 1 failed).
- Failing test: `a_cached_stale_route_is_refreshed_once_and_a_second_stale_result_is_terminal`.
- Immediate error: panicked at `runtime\c2-core\tests\client_modes.rs:959:10` with `relay-aware client: Lifecycle(Configuration("C2_RELAY_USE_PROXY must be a boolean (1/0, true/false, yes/no)"))` — `Runtime::connect(..., Connect::RelayAware).expect("relay-aware client")` got a configuration lifecycle error instead of a client.
- Job end: `Run evidence: D:\a\c-two\c-two\c-two\artifacts\windows-native\full\run-evidence.json (failed)`; `##[error]Process completed with exit code 1.`

## windows-2025 / full / MSVC x64 — 1 of 22 gates failed

- FAILED gate: `core-test` (exit 101). All other 21 gates passed.
- Failing binary: `cargo test -p c2-http --lib` (292 passed, 2 failed).
- Failing test 1: `relay::router::tests::call_missing_upstream_route_removes_stale_local_route` — panicked at `transport\c2-http\src\relay\router.rs:4896:9`, assertion `left == right` failed, left: `410`, right: `404`.
- Failing test 2: `relay::router::tests::probe_missing_upstream_route_removes_stale_local_route` — panicked at `transport\c2-http\src\relay\router.rs:4781:9`, same assertion failure: left: `410`, right: `404`.
- Both tests first log the expected stale-route removal (`Removed unreachable route ... reason=route-removed`) before the status-code assertion mismatch.
- Job end: same `run-evidence.json (failed)` + `##[error]Process completed with exit code 1.`

## Remains unknown

- The effective `C2_RELAY_USE_PROXY` value on the windows-2022 runner is not visible anywhere in the log (only the parse-failure message appears), so why it parsed as non-boolean is unconfirmed.
- Cargo test fail-fast stopped each job's `core-test` at its first failing binary: whether `c2-http --lib` also fails on windows-2022, and whether `c2-core client_modes` also fails on windows-2025, is unobserved.
- Whether the two job-level failures share one root cause (the 410-vs-404 relay status and the proxy-flag parse rejection are different surfaces) is not established by this log alone.

Host review: gate counts above were corrected against the 22 declared gate IDs; nested diagnostic output must not be counted as additional gates. Failure signatures were checked against the retained console log. This is diagnosis, not final artifact acceptance.
