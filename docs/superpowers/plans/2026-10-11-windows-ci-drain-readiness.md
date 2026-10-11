# Windows CI drain test readiness repair

Evidence: Windows Native run 38111076176 at 05f4d86 failed twice with different failures. Attempt 1 had ordinary Python heap corruption at Grid relay registration. Attempt 2 passed ordinary Python but failed the concurrent drain Rust test. The SDK/Core/CI runtime code did not change between 916dacf and 05f4d86. Native heap corruption remains an unresolved separate investigation; local Python 3.12.10 ordinary runs and 90 Grid stress tests passed.

Design: align the cfg(test) drain readiness predicate with the closing_generation state that acquire actually checks. The coordinator announces active_generation before this fence, so the old readiness helper can return before acquisitions should reject. Keep all production close/acquire logic intact, maintain lock order, and keep the helper deadline bounded. Do not increase deadlines or remove assertions.

- [x] Add a regression that announces the coordinator without the acquire fence; prove it fails before fixing the helper.
- [x] Fix only the cfg(test) readiness helper and verify the regression and actual concurrent drain test.
- [x] Run all c2-ipc library tests (203 passed), repeat the affected concurrent test under real Windows (20/20 passed with cleanup confirmed), and obtain independent review (no actionable defects).
- [x] Retain failure and verification evidence and update the investigation boundaries in `docs/reports/2026-10-11-windows-ci-investigation.md`.
- [x] Prepare the verified repair and report for the local commit on the existing PR branch; pushing and hosted verification remain separate from the local verification.

Exit criteria cover the concrete readiness defect only. The original heap corruption has no native dump and is not claimed fixed. Hosted CI for the resulting commit must be recorded separately after push.
