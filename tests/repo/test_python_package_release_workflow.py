from __future__ import annotations

from pathlib import Path


def _repo_root() -> Path:
    return next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / ".github" / "workflows" / "python-package-release.yml").is_file()
    )


def _workflow_text() -> str:
    return (_repo_root() / ".github" / "workflows" / "python-package-release.yml").read_text(
        encoding="utf-8"
    )


def test_python_release_promotes_only_after_both_candidate_gates_complete():
    text = _workflow_text()

    assert "workflow_run:" in text
    assert "workflows: [Release Candidate, Windows Native]" in text
    assert "types: [completed]" in text
    assert "branches: [main]" in text
    assert "candidate_run_id:" in text
    assert "source_sha:" in text
    assert "dry_run:" in text


def test_python_release_fails_closed_outside_the_canonical_repository():
    text = _workflow_text()

    assert "github.repository == 'world-in-progress/c-two'" in text
    assert "github.ref == 'refs/heads/main'" in text
    assert "github.event.workflow_run.event == 'push'" in text
    assert "github.event.workflow_run.head_branch == 'main'" in text


def test_python_release_never_rebuilds_wheels_or_sdist():
    """No maturin/cargo/build jobs remain: only verified candidate bytes ship."""
    text = _workflow_text()

    assert "maturin-action" not in text
    assert "cargo" not in text
    assert "python -m build" not in text
    assert "runs-on: ${{ matrix.os }}" not in text
    assert "python .github/scripts/promote_release_candidate.py prepare" in text
    assert "--target pypi" in text


def test_python_release_prepare_job_reads_actions_with_least_privilege():
    text = _workflow_text().split("  publish:", 1)[0]

    assert "actions: read" in text
    assert "contents: read" in text
    assert "id-token: write" not in text


def test_publish_uses_pypi_oidc_trust_identity_and_environment():
    publish = _workflow_text().split("  publish:", 1)[1]

    assert "environment: pypi" in publish
    assert "id-token: write" in publish
    assert "pypa/gh-action-pypi-publish@release/v1" in publish
    # The OIDC identity is granted only to the publishing job.
    prepare = _workflow_text().split("  publish:", 1)[0]
    assert "id-token: write" not in prepare


def test_publish_uploads_only_the_plan_staged_distribution():
    publish = _workflow_text().split("  publish:", 1)[1]

    assert "needs: prepare" in publish
    assert "needs.prepare.outputs.deferred == 'false'" in publish
    assert "needs.prepare.outputs.action == 'upload'" in publish
    assert "inputs.dry_run != true" in publish
    assert "packages-dir: staging/dist" in publish
    # The staging artifact is downloaded back into ./staging so the plan's
    # staging/dist layout is restored exactly; the directory holds only the
    # registry-missing c_two files the prepare job verified byte-by-byte.
    assert "path: staging" in publish
    assert "pattern: wheels-*" not in publish
    assert "name: sdist" not in publish
    # A registry collision must fail loudly, never silently skip a differing file.
    assert "skip-existing: true" not in publish
    assert "skip-existing: false" not in publish


def test_promotion_stages_only_c_two_packages_never_fastdb_proof():
    script = (
        _repo_root() / ".github" / "scripts" / "promote_release_candidate.py"
    ).read_text(encoding="utf-8")

    assert 'refusing to publish {entry[\'name\']} as a C-Two package' in script
    assert "expected 31 PyPI files" in script


def test_python_pyproject_has_no_cli_entrypoint_or_packaged_binary():
    pyproject = (_repo_root() / "sdk" / "python" / "pyproject.toml").read_text(
        encoding="utf-8"
    )

    assert "[project.scripts]" not in pyproject
    assert "c3 =" not in pyproject
    assert "src/c_two/_bin" not in pyproject
    assert "build-backend = \"maturin\"" in pyproject


def test_python_pyproject_owns_native_binding_manifest():
    pyproject = (_repo_root() / "sdk" / "python" / "pyproject.toml").read_text(
        encoding="utf-8"
    )
    old_manifest_path = "/".join(("core", "bridge", "c2-ffi"))

    assert 'manifest-path = "native/Cargo.toml"' in pyproject
    assert old_manifest_path not in pyproject
    assert 'module-name = "c_two._native"' in pyproject
