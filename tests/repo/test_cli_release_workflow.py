from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest
import yaml

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


def test_cli_release_requires_one_explicit_candidate():
    workflow = yaml.load(_workflow_text(), Loader=yaml.BaseLoader)
    assert set(workflow["on"]) == {"workflow_dispatch"}
    inputs = workflow["on"]["workflow_dispatch"]["inputs"]
    assert set(inputs) == {"candidate_run_id"}
    assert inputs["candidate_run_id"]["required"] == "true"
    assert "GITHUB_STEP_SUMMARY" in _workflow_text()
    assert "workflow_run" not in _workflow_text()


@pytest.mark.parametrize("changed", ["version", "executable", "LIBDIR", "LDLIBRARY", "LIBRARY", "SOABI"])
def test_cargo_cache_identity_tracks_the_actual_python(changed, tmp_path, monkeypatch):
    import subprocess
    import sys
    import sysconfig
    from types import SimpleNamespace

    workflow = yaml.load((_repo_root() / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
    steps = workflow["jobs"]["source"]["steps"]
    script = next(step["run"] for step in steps if step.get("id") == "cargo_identity")
    script = script.split("<<'PYTHON'\n", 1)[1].rsplit("\nPYTHON", 1)[0]
    cache = next(step["with"] for step in steps if step.get("id") == "cargo_cache")
    assert "steps.cargo_identity.outputs.key" in cache["key"]
    assert all("steps.cargo_identity.outputs.key" in key for key in cache["restore-keys"].splitlines())
    output = tmp_path / "identity"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setattr(subprocess, "check_output", lambda *a, **kw: b"fixed rustc")
    monkeypatch.setattr(subprocess, "run", lambda *a, **kw: SimpleNamespace(stdout=b"fixed cc"))
    configuration = {name: "original" for name in ("LIBDIR", "LDLIBRARY", "LIBRARY", "SOABI")}
    monkeypatch.setattr(sysconfig, "get_config_var", configuration.get)

    def fingerprint():
        exec(script, {})
        return output.read_text().splitlines()[-1]

    original = fingerprint()
    assert fingerprint() == original
    if changed in ("version", "executable"):
        monkeypatch.setattr(sys, changed, getattr(sys, changed) + "-changed")
    else:
        configuration[changed] = "changed"
    assert fingerprint() != original


def test_cli_release_accepts_explicit_manual_candidate_inputs():
    text = _workflow_text()

    assert "workflow_dispatch:" in text
    assert "candidate_run_id:" in text
    assert "required: true" in text
    assert "GITHUB_STEP_SUMMARY" in text


def test_cli_release_fails_closed_outside_the_canonical_repository():
    """Fork runs, off-main dispatches, and pull_request candidate runs never
    gain publish authority."""
    text = _workflow_text()

    assert "github.repository == 'world-in-progress/c-two'" in text
    assert "github.ref == 'refs/heads/main'" in text
    assert "github.event_name == 'workflow_dispatch'" in text


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
    assert "needs.prepare.outputs.action == 'upload'" in publish
    assert "environment: github-release" in publish
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


def test_cli_release_body_links_provenance_and_arm64_coverage():
    script = (
        _repo_root() / ".github" / "scripts" / "promote_release_candidate.py"
    ).read_text(encoding="utf-8")

    assert "Windows 11 ARM64" in script
    assert "Source Validation tested commit" in script


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
                               ("core-test", "core/Cargo.toml")):
            assert commands[name][:2] == ["cargo", "test"]
            assert commands[name][commands[name].index("--manifest-path") + 1] == manifest
            assert "--exclude" not in commands[name]
        assert commands['python-native-test'][1:] == ['tools/dev/test_native.py']


def test_ci_validates_source_on_pr_merge_group_and_main_push():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert ci_text.startswith("name: Source Validation\n")
    for event in ("pull_request:", "merge_group:", "push:"):
        assert event in ci_text
    assert "branches: [main]" in ci_text
    assert "tools/ci/source_validation.py" in ci_text
    # Every lane that release promotion requires must exist in the source matrix.
    spec = importlib.util.spec_from_file_location(
        "source_evidence", _repo_root() / ".github" / "scripts" / "source_evidence.py")
    assert spec is not None and spec.loader is not None
    source_evidence = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(source_evidence)
    for label in source_evidence.FULL_LANES:
        assert f"os: {label}," in ci_text
    assert "3.12" in ci_text
    assert "3.14t" in ci_text


def test_ci_keeps_workflow_policy_tests_lightweight():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert "workflow-policy:" in ci_text
    assert "          tests/repo/\n" in ci_text
    assert "sdk/python/tests/unit/test_cli_release_workflow.py" not in ci_text
    assert "uv run --locked --only-group ci-policy" in ci_text
    assert "--confcutdir=tests/repo" in ci_text
    assert "--confcutdir=sdk/python/tests/unit" not in ci_text
    assert "sdk/python/tests/unit/test_c3_tool.py" not in ci_text
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
