# Core sibling FastDB provenance repair

Review: PR 181 comment 4240512004 at efdc684. Core reads sibling FastDB payload source as well as its TypeScript compiler. Existing compiler hashes do not prove the pinned source pair.

Design: capture the actual sibling Git checkout identity under its resource lease for core/all check, prepare and run. Reject incorrect HEAD, dirty source, missing payload entrypoint, or a directory which is not the checkout root. Record the before/after identities and revalidate before and after Core consumption. Preserve the fixed sibling convention and separate --fastdb-source WASM selection. No production mechanism or CI workflow changes.

- [x] Add real temporary Git repository regressions for wrong HEAD, dirty sibling, linked worktree, and source changes between preflight and consumption or during execution; prove the original 13 cases fail before implementation.
- [x] Implement identity capture, prepare/run guards and retained failure reports. Independent review identified hidden Git index flags; reproduce both cases before rejecting those flags. Final review has no actionable findings.
- [x] Run focused regressions (70 passed before the two additional flag cases), then the complete repository suite including all 26 new cases (499 passed, 2 expected skips) on Windows. Verify actual pinned sibling prepare then full Core run (1590 passed, 2 expected fixture entrypoints ignored), with unchanged source identities and cleanup confirmed. All three changed Python files pass real Python 3.10 syntax checks.
- [x] Update development documentation and verification report; prepare the local commit for the existing PR. Push and hosted CI validation remain follow-up steps.

Exit criteria: source violations cannot produce passing development evidence; both identities are retained where capture succeeds. Existing CI and the separate unresolved native heap corruption investigation remain outside this repair.
