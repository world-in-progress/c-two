# Local sidecar review corrections

Baseline: `916dacf1e0c91ec70bab496b45e0cf24d195e3ad`. Goal: fix the four reviewed developer-runner gaps while preserving CI and production communication behavior.

- [x] Add boundary regressions first: actual temporary Git checkout changes during execution; native Windows child-only and conflicting tool paths; clean FastDB sibling compiler preparation; planning/probe exceptions retaining failed JSON.
- [x] Capture source identity before execution, retain it in every intermediate/final report, compare after execution, and fail on HEAD, tracked content or non-ignored source changes. Exclude new evidence/staging output directories from source inventories.
- [x] Resolve executables from the selected child environment before launching and record the absolute command; preserve Node/npm selection and test actual Windows process creation.
- [x] Prepare the locked npm dependencies at the sibling TypeScript path consumed by Core codegen tests, include Node/npm prerequisites, and verify the compiler before Core execution.
- [x] Reserve a new evidence directory before planning; retain phase/error/status on preparation exceptions without overwriting existing evidence.
- [x] Run focused regressions, actual clean-sibling preparation/consumer proof, full repository checks, Python 3.10 syntax and final diff review. Record commands, results and limits, then commit locally.

The accepted review supplies the design and scope; implement these corrections directly. Hosted CI results at the reviewed commit and reviewer macOS results are reported by the reviewer, not reruns performed on this Windows machine.


## Exit evidence

The initial boundary regressions produced 10 failures and one pass against the reviewed runner. The corrected runner passes all 30 focused runner/review tests on Windows. Native execution verifies both child-only tools and conflicting parent PATH tools; both preflight probes and owned process launches use the selected executable. Independent review caught premature aggregate passing evidence and incomplete compiler implementation hashing; both were corrected and re-reviewed without remaining actionable findings.

A fresh pinned FastDB sibling without node_modules was prepared using the actual core npm stage. TypeScript 5.9.3 then compiled a sample whose JavaScript output executed successfully. The actual Core prepare-to-run sequence on the development checkout passed 1589 Rust tests with two parent-invoked child entrypoints ignored. The sidecar repository run passed 473 tests with exactly two declared Windows skips. Each retained report has matching beginning/end source identity. Python 3.10 compilation checks and git diff whitespace checks passed.

Detailed evidence and boundaries are recorded in [the review-fix report](../../reports/2026-10-11-sidecar-review-fixes.md). No implementation follow-ups remain for these four findings. Hosted CI and native macOS are not rerun locally; pushing the local commit to the existing PR branch triggers its CI.
