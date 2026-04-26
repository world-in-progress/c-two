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


def test_cli_release_builds_canonical_platform_artifacts():
    text = _workflow_text()

    assert "name: CLI Release" in text
    assert "workflow_dispatch:" in text
    assert "tags:" in text
    assert "x86_64-unknown-linux-gnu" in text
    assert "aarch64-unknown-linux-gnu" in text
    assert "aarch64-apple-darwin" in text
    assert "x86_64-apple-darwin" in text
    assert 'cp "cli/target/${{ matrix.target }}/release/c3" "cli-dist/c3-${{ matrix.target }}"' in text
    assert '(cd cli-dist && shasum -a 256 "c3-${{ matrix.target }}" > "c3-${{ matrix.target }}.sha256")' in text


def test_cli_release_publishes_github_release_assets_on_tags():
    text = _workflow_text()

    assert "contents: write" in text
    assert "pattern: cli-*" in text
    assert "softprops/action-gh-release@v2" in text
    assert "files: dist/c3-*" in text


def test_ci_runs_cli_rust_tests():
    ci_text = (_repo_root() / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert "cargo test --manifest-path cli/Cargo.toml" in ci_text


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
