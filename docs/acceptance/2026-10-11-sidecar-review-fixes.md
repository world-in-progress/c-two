# Local sidecar PR review corrections — Windows verification

Reviewed baseline: `916dacf1e0c91ec70bab496b45e0cf24d195e3ad`. This verification used the modified working tree on `codex/local-test-sidecar`; the report's baseline HEAD plus diff/file hashes identify those inputs. The changes affect the local development entry, regressions and documentation. CI workflows and production runtime files are outside this patch.

## Corrections

1. Capture checkout HEAD, tracked diff and tracked/untracked file inventory before preflight. Retain starting source identity and compare at completion. Changes fail the aggregate; it stays running until final guards finish. Output and preparation staging directories are excluded from the inventory. The regression mutates tracked source, creates untracked source and advances HEAD using real child processes in temporary Git repositories.
2. Resolve tool commands to absolute paths from the child environment before both probes and process launches. Native Windows tests put a tool only on the child PATH, and select it over a different executable on the parent PATH. Reports record actual resolved commands; npm continues to use the selected Node's JavaScript CLI.
3. Core/all preparation installs the sibling FastDB locked npm dependencies with scripts disabled. Core preflight checks TypeScript 5.9.3 and hashes the whole compiler package, including its implementation behind bin/tsc. Sibling leases protect preflight and actual Core execution, with input comparisons before/after consumption and at final validation. The lease is released between stages so the existing strict Python entry can acquire its own leases.
4. Reserve each new evidence directory before preparation planning. Validation, probe, planning and cancellation failures retain JSON status, phase, error and finished time. Existing evidence is never overwritten.

## Verification

| Verification | Result | Retained evidence |
| --- | --- | --- |
| New boundary tests before corrections | 10 failed, 1 passed | `D:/c-two-test/sidecar-review-red` |
| Runner and review boundary regressions | 30 passed | `tests/repo/test_local_test_runner.py` and `tests/repo/test_local_test_review.py` |
| Clean pinned sibling preparation and compiler consumption | Passed; TypeScript 5.9.3 installed, sample compiled and executed | `D:/c-two-test/sidecar-review-clean/evidence/run.json` |
| Actual `prepare --suite core` | Passed; npm and locked Cargo dependencies prepared | `D:/c-two-test/sidecar-review-core-prepare/run.json` |
| Actual `run --suite core` | 1589 passed, 0 failed, 2 expected child entrypoints ignored | `D:/c-two-test/sidecar-review-core-run/run.json` |
| Actual `run --suite repo` | 473 passed, 2 declared Windows skips | `D:/c-two-test/sidecar-review-repo-run/run.json` |
| Python 3.10 syntax | Passed for entry and changed tests | Source compiled with Python 3.10 without writing bytecode |

The repository run used the prepared repo interpreter, verified official SDK fixtures and the MSVC development environment. Core used Node 22.16 with Node-API 10 and `D:/c-two-test/fastdb-core-sdk/lib`. The reproducible local invocation wrapper is `D:/c-two-test/sidecar-review-entry-proof.py`: run with `repo` or `core`; it loads vcvars64 and invokes the same sidecar main entry. All three main-entry reports confirm equal before/after source identities.

An initial direct pytest invocation bypassed the sidecar's UTF-8 environment and hit a GBK decode failure while reading a workflow. The subsequent full run through the sidecar passed without changing that workflow test or adding a skip. The two accepted repository skips are the non-PE installer fixture and POSIX virtualenv symlink semantics.

## Limits

Source provenance uses beginning/end snapshots rather than a filesystem freeze; an external edit reverted before the end cannot be detected by this comparison. Tests ran on Windows. Unix command variants have regression code, but native macOS execution and hosted CI were not rerun here. The fresh sibling proof exercises the actual npm preparation stage and compiler; the complete Core prepare/run proof uses the development checkout. These remain development evidence, separate from official release certification.
