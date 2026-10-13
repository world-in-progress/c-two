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
    assert "workflows: [Release Candidate, Source Validation]" in text
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


def test_cli_release_skips_explicitly_when_version_already_published():
    """Automatic workflow_run promotions whose c3 version is already fully
    released from the tagged source commit finish as a no-publication skip
    validated against the published, manifest-bound release; manual
    dispatches keep failing closed on that collision."""
    text = _workflow_text()
    prepare = text.split("  publish:", 1)[0]

    # The skip allowance exists only on the automatic workflow_run path.
    assert "AUTOMATIC: ${{ github.event_name == 'workflow_run' }}" in prepare
    assert (
        'if [ "$AUTOMATIC" = "true" ]; then args+=(--skip-already-published); fi'
        in prepare
    )
    # The prepare parser accepts the skip plan and reports it explicitly,
    # reading the reason from the selected group so a late race skip stored
    # under github_release cannot raise.
    assert 'plan.get("github_release") or plan["skip"]' in prepare
    assert "No publication: {release['reason']}" in prepare
    assert "plan['skip']['reason']" not in prepare
    assert '"skip-already-published"' in prepare
    # A skip retains no staged bytes or plan artifact and never publishes:
    # both retention steps are gated, and publish still requires 'upload'.
    assert prepare.count("action != 'skip-already-published'") == 2
    publish = text.split("  publish:", 1)[1]
    assert "needs.prepare.outputs.action == 'upload'" in publish


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
    assert "Source Validation full-scope run" in script


def test_readmes_document_the_c3_installer_asset():
    root = _repo_root()
    version = tomllib.loads((root / "cli" / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    releases = "https://github.com/world-in-progress/c-two/releases"
    installers = (
        f"{releases}/latest/download/c3-installer.sh",
        f"{releases}/download/c3-v{version}/c3-installer.sh",
    )

    for path in [
        root / "README.md",
        root / "README.zh-CN.md",
        root / "examples" / "README.md",
        root / "cli" / "README.md",
    ]:
        text = path.read_text(encoding="utf-8")
        assert any(url in text for url in installers), path
        if path != root / "examples" / "README.md":
            assert f"--version {version}" in text, path


def test_ci_runs_cli_rust_tests():
    from tools.ci.source_validation import gates

    for platform in ("linux", "macos", "windows"):
        commands = {name: command for name, command, _ in gates(
            "python", Path("."), "full", target_platform=platform)}
        for name, manifest in (("cli-test", "cli/Cargo.toml"),
                               ("core-test", "core/Cargo.toml"),
                               ("python-native-test", "sdk/python/native/Cargo.toml")):
            assert commands[name][:2] == ["cargo", "test"]
            assert commands[name][commands[name].index("--manifest-path") + 1] == manifest
            assert "--exclude" not in commands[name]


def test_ci_validates_source_on_pr_merge_group_and_main_push():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert ci_text.startswith("name: Source Validation\n")
    for event in ("pull_request:", "merge_group:", "push:"):
        assert event in ci_text
    assert "branches: [main]" in ci_text
    assert "tools/ci/source_validation.py" in ci_text
    for label in ("ubuntu-24.04", "macos-15", "windows-2022", "windows-2025"):
        assert label in ci_text
    assert "3.12" in ci_text
    assert "3.14t" in ci_text


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
    assert "tests/repo/test_c3_tool.py" in ci_text
    assert "tests/repo/test_c3_installer.py" in ci_text
    assert "sdk/python/tests/unit/test_c3_tool.py" not in ci_text
    assert "tests/repo/test_python_package_release_workflow.py" in ci_text
    assert "sdk/python/tests/unit/test_python_package_release_workflow.py" not in ci_text
    # The source runner delegates the complete Python partition and receipts
    # to the prepared test entry point rather than an inline pytest invocation.
    from tools.ci.source_validation import gates

    commands = {name: command for name, command, _ in gates(
        "python", Path("."), "full", target_platform="linux")}
    assert any(argument.endswith("tools/dev/test_python.py")
               for argument in commands["python-suite"])


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
