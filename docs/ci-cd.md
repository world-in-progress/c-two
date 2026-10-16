# CI and release qualification

Daily validation, release qualification and publication are separate stages.
A normal pull request or main-branch push does not build the 30-wheel release
matrix and cannot publish anything. Publishing promotes verified bytes; it
does not compile them again.

| Stage | Trigger | Required evidence | Can publish? |
| --- | --- | --- | --- |
| Source Validation | PR, merge queue, main push, explicit full run | Workflow policy; full source matrix when runtime inputs change | No |
| Release Candidate | Explicit dispatch on canonical main | Successful full source run for the exact commit, then all package and installed-consumer receipts | No |
| CLI Release | Explicit candidate run ID | Candidate and pinned source receipts, asset inventory and hashes, remote-state checks | c3 assets only |
| Python Package Release | Explicit candidate run ID | The same source and candidate proof, all 31 Python distributions and hashes | PyPI only |

Both publication workflows default to a dry run. They have no `push`, tag or
`workflow_run` trigger. A source-test completion cannot start publication, and
the two validation completions cannot create duplicate promotion runs.

## Daily checks

The scope selector reads the complete changed-file list: PR API results,
push `before`/`after`, or merge-group base/head. Runtime/build inputs and
unclassified new paths receive the full matrix. Documentation, workflow
definitions, release tooling and their repository tests receive workflow
policy checks without native compilation. Mixed changes select full coverage;
an incomplete PR inventory also selects full coverage.

Workflow policy always runs the pinned, hash-checked actionlint binary and
automatically discovers all `tests/repo/` tests. Python tools come from the
locked `ci-policy` dependency group, which installs neither C-Two nor its
runtime dependencies. The development group includes that same tool group.
The source matrix starts only after policy succeeds, so a tooling or workflow
failure is rejected before expensive native jobs begin.
The protected check names remain
`test (py3.12)` and `test (py3.14t)`. Their always-running aggregation requires
policy success and either successful source checks or an intentional scope
decision to omit them. A failed scope decision cannot produce a green gate.

Full source coverage retains Linux, macOS, Windows 2022, Windows 2025 and
free-threaded Python, including relay, cross-language and endpoint-cleanup
checks. A policy-only green run is never sufficient for a release candidate.
To qualify a commit whose daily checks did not need native compilation:

```bash
gh workflow run ci.yml --ref main -F force_full=true
```

Manual source qualification and daily checks use separate concurrency groups;
a new push cannot cancel the explicitly requested qualification. Superseded
daily runs on the same ref are cancelled. Cargo cache recovery is restricted
by OS, architecture, runner, Python and toolchain. Lockfile/version changes may
reuse unchanged dependencies, but Cargo revalidates fingerprints and all
applicable tests still execute. Cache hits are never test evidence.

Validation commands share one 45-minute aggregate budget. Each child command
is bounded by the lesser of its phase limit and the remaining budget. A
timeout first interrupts the driver and allows up to 40 seconds for its
existing child cleanup before forcing the owned process group to stop. The
50-minute validation-step and 60-minute job limits leave time for cleanup
and evidence upload. A failure retains its real exit code and full log. Cargo
panics and Python assertions are shown in the console and step summary even
when later successful commands would otherwise push them out of the log tail.

## Build one release candidate

After full source validation succeeds, dispatch:

```bash
gh workflow run release-candidate.yml --ref main
```

The first job checks canonical repository/main provenance and all required
full source jobs before any package compilation. Its exact source-validation
run ID is written into `rc-context.json`. Every build and receipt uses the
dispatch commit, not a later moving branch head. The existing Linux/native
reusable workflows produce and verify the complete 30-wheel, 5-c3 and sdist
inventory, installed-wheel and cross-language checks, and 47 receipts.

Only a successful candidate is eligible for promotion. Keep its run ID;
do not start another candidate build to publish it. Explicit candidate builds
do not cancel each other. Publication requires retained, unexpired artifacts;
an expired or failed candidate needs a new qualification, not a bypass.

## Promote the retained bytes

Preview both destinations using the same successful candidate ID:

```bash
gh workflow run cli-release.yml --ref main -f candidate_run_id=RUN_ID -F dry_run=true
gh workflow run python-package-release.yml --ref main -f candidate_run_id=RUN_ID -F dry_run=true
```

The trusted main-branch verifier binds the selected candidate, its dispatch
commit, its pinned full source run, receipt inventory and SHA-256 digests.
Fork, PR and merge-group runs cannot supply release evidence. The publishing
jobs recheck the staged bytes immediately before upload. Same-byte retries
can finish an interrupted publication; different bytes or source identities
cannot overwrite an existing version, tag or asset.

Publication uses the same commands with `dry_run=false` after release
authorization. CLI and PyPI remain separate destinations so each publishing
job has only its required permission. PyPI keeps the existing
`python-package-release.yml` Trusted Publisher identity and `pypi` environment;
only its publishing job receives `id-token: write`. CLI's publishing job alone
receives `contents: write`. Preparation has read-only permissions.

The flow follows GitHub's [explicit dispatch](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch),
[reusable workflow](https://docs.github.com/en/actions/concepts/workflows-and-actions/reusing-workflow-configurations)
and [concurrency](https://docs.github.com/en/actions/concepts/workflows-and-actions/concurrency)
mechanisms, and PyPI's [Trusted Publisher](https://docs.pypi.org/trusted-publishers/using-a-publisher/)
permission boundary. No external release service or additional orchestration
framework is required.
