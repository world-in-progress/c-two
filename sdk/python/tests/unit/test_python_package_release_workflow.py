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


def test_publish_downloads_only_python_distribution_artifacts():
    publish_job = _workflow_text().split("  publish:", 1)[1]

    assert "pattern: wheels-*" in publish_job
    assert "name: sdist" in publish_job
    assert "path: dist" in publish_job
    assert (
        "actions/download-artifact@v4\n"
        "        with:\n"
        "          path: dist\n"
        "          merge-multiple: true"
    ) not in publish_job


def test_python_release_does_not_build_or_package_cli():
    build_wheels_job = _workflow_text().split("  build-wheels:", 1)[1].split(
        "  build-sdist:", 1
    )[0]

    assert "Build c3 CLI" not in build_wheels_job
    assert "Inject c3 CLI into Python package" not in build_wheels_job
    assert 'sdk/python/src/c_two/_bin' not in build_wheels_job
    assert "cli-dist" not in build_wheels_job
    assert "name: c3-${{ matrix.target }}" not in build_wheels_job


def test_python_pyproject_has_no_cli_entrypoint_or_packaged_binary():
    pyproject = (_repo_root() / "sdk" / "python" / "pyproject.toml").read_text(
        encoding="utf-8"
    )

    assert "[project.scripts]" not in pyproject
    assert "c3 =" not in pyproject
    assert "src/c_two/_bin" not in pyproject
    assert "build-backend = \"maturin\"" in pyproject
