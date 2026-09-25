from __future__ import annotations

from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - Python < 3.11
    import tomli as tomllib


def _repo_root() -> Path:
    return next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / ".github" / "workflows").is_dir()
    )


def _workflow_text() -> str:
    return (_repo_root() / ".github" / "workflows" / "cli-release.yml").read_text(
        encoding="utf-8"
    )


def test_cli_release_promotes_only_after_both_candidate_gates_complete():
    """The promotion triggers on either gate's completion, main pushes only."""
    text = _workflow_text()

    assert "workflow_run:" in text
    assert "workflows: [Release Candidate, Windows Native]" in text
    assert "types: [completed]" in text
    assert "branches: [main]" in text


def test_cli_release_accepts_explicit_manual_candidate_inputs():
    text = _workflow_text()

    assert "workflow_dispatch:" in text
    assert "candidate_run_id:" in text
    assert "source_sha:" in text
    assert "dry_run:" in text


def test_cli_release_fails_closed_outside_the_canonical_repository():
    """Fork runs, off-main dispatches, and pull_request candidate runs never
    gain publish authority."""
    text = _workflow_text()

    assert "github.repository == 'world-in-progress/c-two'" in text
    assert "github.ref == 'refs/heads/main'" in text
    assert "github.event.workflow_run.event == 'push'" in text
    assert "github.event.workflow_run.head_branch == 'main'" in text


def test_cli_release_never_rebuilds_the_candidate_bytes():
    """No build matrix, no cargo/maturin/docker: only verified bytes are promoted."""
    text = _workflow_text()

    assert "cargo build" not in text
    assert "maturin" not in text
    assert "docker run" not in text
    assert "runs-on: ${{ matrix.os }}" not in text
    assert "python .github/scripts/promote_release_candidate.py prepare" in text
    assert "--target github-release" in text


def test_cli_release_publishes_only_verified_staged_bytes():
    publish = _workflow_text().split("  publish:", 1)[1]

    assert "needs: prepare" in publish
    assert "needs.prepare.outputs.deferred == 'false'" in publish
    assert "needs.prepare.outputs.action == 'upload'" in publish
    assert "inputs.dry_run != true" in publish
    assert "permissions:" in publish
    assert "contents: write" in publish
    # The staging artifact is downloaded back into ./staging so the plan's
    # staged_path layout is restored exactly.
    assert "name: promotion-staging" in publish
    assert "path: staging" in publish
    assert "dist/c3-*" not in publish
    # The upload list comes from upload-files.txt; gh release upload fails on
    # an asset name that already exists, so nothing is ever clobbered.
    assert 'xargs gh release upload "$TAG" --repo "$REPO" < upload-files.txt' in publish


def test_cli_release_creates_the_release_only_when_the_plan_says_it_is_missing():
    publish = _workflow_text().split("  publish:", 1)[1]

    # Reruns against a partial release must complete it, never recreate it,
    # and every gh command names the canonical repository explicitly because
    # the publish job has no checkout.
    assert "if: needs.prepare.outputs.release_exists == 'false'" in publish
    assert 'gh release create "$TAG" --repo "$REPO"' in publish
    assert 'extra+=(--verify-tag)' in publish
    assert 'extra+=(--target "$SOURCE_SHA")' in publish
    assert "REPO: world-in-progress/c-two" in _workflow_text()


def test_cli_release_defers_when_only_one_gate_has_completed():
    """Exit 75 (deferred) is a neutral skip; the late event re-triggers promotion."""
    text = _workflow_text()

    assert 'if [ "$code" -eq 75 ]' in text
    assert "deferred=true" in text
    assert "action=deferred" in text


def test_cli_release_prepare_job_reads_actions_with_least_privilege():
    text = _workflow_text().split("  publish:", 1)[0]

    assert "actions: read" in text
    assert "contents: read" in text
    assert "id-token: write" not in text


def test_cli_release_publishes_installer_assets_under_stable_names():
    """The promotion stages installers and FastDB license evidence by name."""
    script = (
        _repo_root() / ".github" / "scripts" / "promote_release_candidate.py"
    ).read_text(encoding="utf-8")

    assert 'EXPECTED_INSTALLER_NAMES = ("c3-installer.sh", "c3-installer.ps1")' in script
    assert '"fastdb-LICENSE"' in script
    assert '"fastdb-THIRD_PARTY_NOTICES.txt"' in script


def test_cli_release_body_links_provenance_without_desktop_claims():
    script = (
        _repo_root() / ".github" / "scripts" / "promote_release_candidate.py"
    ).read_text(encoding="utf-8")

    assert "Windows 11 desktop is not verified" in script
    assert "Windows Native full-scope run" in script


def test_readmes_document_the_c3_installer_asset():
    root = _repo_root()

    for path in [
        root / "README.md",
        root / "README.zh-CN.md",
        root / "examples" / "README.md",
        root / "cli" / "README.md",
    ]:
        text = path.read_text(encoding="utf-8")
        assert "releases/latest/download/c3-installer.sh" in text


def test_ci_runs_cli_rust_tests():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert "cargo test --manifest-path cli/Cargo.toml" in ci_text
    assert "cargo test --manifest-path core/Cargo.toml --workspace" in ci_text
    assert "cargo test --manifest-path sdk/python/native/Cargo.toml" in ci_text
    assert "--exclude " + "c2-ffi" not in ci_text


def test_ci_routes_tests_by_changed_domain():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert "changes:" in ci_text
    assert "dorny/paths-filter@" in ci_text
    assert "pull-requests: read" in ci_text
    assert "sdk/python/**" in ci_text
    assert "core/**" in ci_text
    assert "cli/**" in ci_text
    assert "tools/dev/**" in ci_text
    assert ".github/dependabot.yml" in ci_text
    assert ".github/workflows/cli-release.yml" in ci_text
    assert "github.event_name == 'merge_group'" in ci_text
    assert "needs.changes.outputs.sdk == 'true'" in ci_text
    assert "needs.changes.outputs.core == 'true'" in ci_text
    assert "needs.changes.outputs.ci == 'true'" in ci_text
    assert "needs.changes.outputs.cli == 'true'" in ci_text
    assert "Skip full Python tests for unrelated changes" in ci_text
    assert "this change does not touch sdk/python, examples/python, core, or CI" in ci_text


def test_ci_keeps_workflow_policy_tests_lightweight():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert "workflow-policy:" in ci_text
    assert "tests/repo/test_cli_release_workflow.py" in ci_text
    assert "sdk/python/tests/unit/test_cli_release_workflow.py" not in ci_text
    assert "uv run --no-project" in ci_text
    assert "--with pytest" in ci_text
    assert "--confcutdir=tests/repo" in ci_text
    assert "--confcutdir=sdk/python/tests/unit" not in ci_text
    assert "tests/repo/test_check_version.py::TestCheckVersion" in ci_text
    assert "sdk/python/tests/unit/test_check_version.py::TestCheckVersion" not in ci_text
    assert "tests/repo/test_check_cli_release.py" in ci_text
    assert "sdk/python/tests/unit/test_check_cli_release.py" not in ci_text
    assert "tests/repo/test_c3_tool.py" in ci_text
    assert "tests/repo/test_c3_installer.py" in ci_text
    assert "sdk/python/tests/unit/test_c3_tool.py" not in ci_text
    assert "tests/repo/test_python_package_release_workflow.py" in ci_text
    assert "sdk/python/tests/unit/test_python_package_release_workflow.py" not in ci_text
    # The native extension is built in source mode first; --no-sync keeps the
    # system-mode consumer environment from relinking it against the SDK.
    assert "uv run --no-sync pytest sdk/python/tests -q --timeout=30" in ci_text


def test_cli_enables_graceful_termination_signal_handling():
    cargo_toml = (_repo_root() / "cli" / "Cargo.toml").read_text(encoding="utf-8")
    parsed = tomllib.loads(cargo_toml)

    ctrlc_dep = parsed["dependencies"]["ctrlc"]
    assert isinstance(ctrlc_dep, dict)
    assert "termination" in ctrlc_dep["features"]


def test_registry_commands_have_bounded_http_timeout():
    registry_rs = (_repo_root() / "cli" / "src" / "registry.rs").read_text(
        encoding="utf-8"
    )

    assert ".timeout(std::time::Duration::from_secs(5))" in registry_rs
