# CI and release qualification

Daily validation, release qualification and publication are separate stages. A normal pull request or main-branch push does not build the 35-wheel release matrix and cannot publish anything. Publishing promotes verified bytes; it does not compile them again.

| Stage | Trigger | Required evidence | Can publish? |
| --- | --- | --- | --- |
| Source Validation | PR, merge queue, main push, explicit full run | Workflow policy; full source matrix when required | No |
| Release Candidate | Explicit dispatch on canonical main | All package and installed-consumer receipts for the dispatch commit | No |
| CLI Release | Explicit candidate run ID | Same-tree source validation, candidate inventory and hashes, remote-state checks, environment approval | c3 assets only |
| Python Package Release | Explicit candidate run ID | The same source and candidate proof, all 36 Python distributions and hashes, environment approval | PyPI only |

Both publication workflows use one explicit dispatch followed by environment approval. They have no `push`, tag or `workflow_run` trigger. A source-test completion cannot start publication, and the two validation completions cannot create duplicate promotion runs.

## Daily checks

The scope selector reads the complete changed-file list: PR API results, push `before`/`after`, or merge-group base/head. Runtime/build inputs and unclassified new paths receive the full matrix. Documentation, workflow definitions, release tooling and their repository tests receive workflow policy checks without native compilation. Mixed changes select full coverage; an incomplete PR inventory also selects full coverage.

On a main push, the selector first applies the changed-path rules. Policy-only changes, including documentation, remain policy-only without an evidence lookup. When the paths require full source validation, it searches for successful full Source Validation of an associated merged PR head with the same Git tree as the pushed commit. If such evidence exists, it prints the selected run and runs only workflow policy; otherwise it keeps full source validation and prints the lookup result or error. Missing workflow environment variables for the evidence lookup fail scope selection. An explicit full dispatch always runs the source matrix.

Workflow policy always runs the pinned, hash-checked actionlint binary and automatically discovers all `tests/repo/` tests. Python tools come from the locked `ci-policy` dependency group, which installs neither C-Two nor its runtime dependencies. The development group includes that same tool group. Source jobs and workflow policy run in parallel after scope selection. The protected check names remain `test (py3.12)` and `test (py3.14t)`. Their always-running aggregation requires successful scope selection, policy success and either successful source checks or an intentional scope decision to omit them. A failed scope decision cannot produce a green gate.

Full source coverage includes Linux, macOS, Windows Server 2025 x64, Windows 11 ARM64 and free-threaded Python, including relay, cross-language and endpoint-cleanup checks. ARM64 uses native Rust, Python and Node consumers. A policy-only green run cannot supply release source evidence. To qualify a commit without retained full source evidence:

```bash
gh workflow run ci.yml --ref main -F force_full=true
```

Source evidence binds to a Git tree: the release commit itself and the head SHAs of merged PRs whose merge commit is that release commit are the candidate tested commits. The shared selector accepts runs in `world-in-progress/c-two` using `.github/workflows/ci.yml`, triggered by `push`, `workflow_dispatch` or `pull_request`, completed successfully, and testing the identical Git tree. A PR run in the canonical repository can qualify even when its head repository is a fork. Files, including the workflow, must match exactly through the Git tree hash.

Every required job's latest attempt must succeed: the five platform `source (<label>, py3.12)` jobs, `source (ubuntu-24.04, py3.14t)` and `workflow policy`. Source artifact names end with `-a<run_attempt>`. Each full platform lane must retain exactly one unexpired artifact for that job's latest attempt and tested commit. Failed source lanes may be rerun within the same workflow run; successful lanes keep their own attempt numbers and evidence. Earlier attempts' artifacts do not invalidate the run. Promotion selects the newest qualifying run and reports rejection reasons when none qualifies. It checks this source metadata boundary without downloading and revalidating internal source receipts.

Manual source qualification and daily checks use separate concurrency groups; a new push cannot cancel the explicitly requested qualification. Superseded daily runs on the same ref are cancelled. Cargo cache recovery is restricted by OS, architecture, runner, Python and toolchain. Lockfile/version changes may reuse unchanged dependencies, but Cargo revalidates fingerprints and all applicable tests still execute. Cache hits are never test evidence.

Validation commands share one 45-minute aggregate budget. Each child command is bounded by the lesser of its phase limit and the remaining budget. A timeout first interrupts the driver and allows up to 40 seconds for its existing child cleanup before forcing the owned process group to stop. The validation step has a 50-minute limit; the job has a 90-minute limit to include native prerequisites, cleanup and evidence upload. A failure retains its real exit code and full log. Cargo panics and Python assertions are shown in the console and step summary even when later successful commands would otherwise push them out of the log tail.

## Build one release candidate

Dispatch immediately after merging the release commit; packaging does not wait for Source Validation:

```bash
gh workflow run release-candidate.yml --ref main
```

Every build and receipt uses the dispatch commit, not a later moving branch head. `rc-context.json` uses `c-two.release-candidate.context.v2` and records source identity and versions; source validation is selected later during promotion. The existing Linux/native reusable workflows produce and verify 35 wheels, 6 c3 binaries, 1 sdist, 2 installers and 56 receipts. Windows ARM64 covers Python 3.11–3.14 and 3.14t; the other five targets also cover 3.10. Installed consumers validate native architecture, cross-language calls and cleanup, including standard-user checks on both Windows architectures.

Only a successful candidate is eligible for promotion. Keep its run ID and use it for both publication destinations. Explicit candidate builds do not cancel each other. Publication requires retained, unexpired artifacts; an expired or failed candidate needs a new candidate build. Candidate artifact handling is separate from source-lane reruns: rebuild the complete candidate if candidate qualification fails.

## Promote the retained bytes

Trigger each destination once using the same successful candidate ID:

```bash
gh workflow run cli-release.yml --ref main -f candidate_run_id=RUN_ID
gh workflow run python-package-release.yml --ref main -f candidate_run_id=RUN_ID
```

The prepare job verifies the canonical-main dispatch candidate, API artifact digests, manifest, receipt inventory and SHA-256 bindings, then selects same-tree source evidence for its source commit. Candidate publication bytes still come exclusively from the selected canonical-main Release Candidate run. PR source runs supply validation evidence only. Remote-state checks ensure same-byte retries can finish an interrupted publication and different bytes or source identities cannot overwrite an existing version, tag or asset.

Prepare writes the plan to its step summary before approval: candidate URL, release source and versions, selected Source Validation run URL and event, tested commit and tree, required job attempt numbers, publication action, and each file's disposition and SHA-256. The retained JSON plan includes the selected source run ID and all this source provenance. A publish job runs only when the plan requires uploads, waits for its environment approval, and rechecks the staged bytes immediately before upload. Already-published identical bytes require no publishing job.

Before enabling CLI publication, create the `github-release` environment in the repository settings, restrict it to `main` and configure required reviewers. The CLI publish job references that environment and alone receives `contents: write`. PyPI keeps the existing `python-package-release.yml` Trusted Publisher identity and `pypi` environment with required reviewers; only its publishing job receives `id-token: write`. Preparation has read-only permissions. Environment protection is configured in repository settings, so the environment and required reviewers must exist before relying on the approval boundary.

The flow follows GitHub's [explicit dispatch](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch), [reusable workflow](https://docs.github.com/en/actions/concepts/workflows-and-actions/reusing-workflow-configurations) and [concurrency](https://docs.github.com/en/actions/concepts/workflows-and-actions/concurrency) mechanisms, and PyPI's [Trusted Publisher](https://docs.pypi.org/trusted-publishers/using-a-publisher/) permission boundary. No external release service or additional orchestration framework is required.
