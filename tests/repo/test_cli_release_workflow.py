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


def _filter_lines(ci_text: str, filter_name: str) -> list[str]:
    lines = ci_text.splitlines()
    header = f"            {filter_name}:"
    start = lines.index(header) + 1
    result: list[str] = []
    for line in lines[start:]:
        if line.startswith("            ") and not line.startswith("              - "):
            break
        if line.startswith("              - "):
            result.append(line.removeprefix("              - ").strip("'"))
    return result


def test_cli_release_builds_canonical_platform_artifacts():
    text = _workflow_text()

    assert "name: CLI Release" in text
    assert "workflow_dispatch:" in text
    assert "branches: [main]" in text
    assert "x86_64-unknown-linux-gnu" in text
    assert "aarch64-unknown-linux-gnu" in text
    assert "aarch64-apple-darwin" in text
    assert "x86_64-apple-darwin" in text
    assert 'cp "cli/target/${{ matrix.target }}/release/c3" "cli-dist/c3-${{ matrix.target }}"' in text
    assert '(cd cli-dist && shasum -a 256 "c3-${{ matrix.target }}" > "c3-${{ matrix.target }}.sha256")' in text


def test_cli_release_auto_detects_new_cli_versions_on_main_push():
    text = _workflow_text()

    assert "branches: [main]" in text
    assert "version-check:" in text
    assert "python .github/scripts/check_cli_release.py" in text
    assert "should_release: ${{ steps.check.outputs.should_release }}" in text
    assert "tag: ${{ steps.check.outputs.tag }}" in text
    assert "needs.version-check.outputs.should_release == 'true'" in text


def test_cli_release_publishes_github_release_assets_for_detected_tag():
    text = _workflow_text()

    assert "contents: write" in text
    assert "pattern: cli-*" in text
    assert "softprops/action-gh-release@" in text
    assert "tag_name: ${{ needs.version-check.outputs.tag }}" in text
    assert "target_commitish: ${{ github.sha }}" in text
    assert "name: c3 ${{ needs.version-check.outputs.version }}" in text
    assert "files: dist/c3-*" in text


def test_cli_release_publishes_installer_script_asset():
    text = _workflow_text()

    assert "installer:" in text
    assert "name: cli-installer" in text
    assert "c3-installer.sh" in text
    assert "path: cli-dist/c3-installer.sh" in text
    assert "needs: [version-check, build, installer]" in text
    assert "should_publish_installer: ${{ steps.check.outputs.should_publish_installer }}" in text
    assert "needs.version-check.outputs.should_publish_installer == 'true'" in text
    assert "files: dist/c3-*" in text


def test_cli_release_can_backfill_installer_when_current_release_exists():
    text = _workflow_text()

    assert "publish-installer:" in text
    assert "needs: [version-check, installer]" in text
    assert "tag_name: ${{ needs.version-check.outputs.tag }}" in text
    assert "files: dist/c3-installer.sh" in text


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
    assert "cli/install-c3.sh" in _filter_lines(ci_text, "workflow_policy")
    assert "tests/repo/**" in _filter_lines(ci_text, "workflow_policy")
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
    assert "uv run pytest sdk/python/tests -q --timeout=30" in ci_text


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
