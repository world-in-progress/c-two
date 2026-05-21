"""Tests for .github/scripts/check_cli_release.py."""

from __future__ import annotations

import json
import sys
import urllib.error
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

_ROOT = next(
    parent
    for parent in Path(__file__).resolve().parents
    if (parent / ".github" / "scripts").is_dir()
)
_SCRIPT_DIR = str(_ROOT / ".github" / "scripts")


@pytest.fixture(scope="module")
def cli_release():
    sys.path.insert(0, _SCRIPT_DIR)
    sys.modules.pop("check_cli_release", None)
    import check_cli_release

    yield check_cli_release
    sys.path.remove(_SCRIPT_DIR)
    sys.modules.pop("check_cli_release", None)


def _mock_github_release(*, assets: list[dict[str, str]] | None = None) -> MagicMock:
    resp = MagicMock()
    resp.read.return_value = json.dumps(
        {
            "tag_name": "c3-v0.1.0",
            "assets": assets if assets is not None else [{"name": "c3-installer.sh"}],
        }
    ).encode()
    resp.__enter__ = lambda s: s
    resp.__exit__ = MagicMock(return_value=False)
    return resp


def _parse_output(path: Path) -> dict[str, str]:
    return dict(
        line.split("=", 1) for line in path.read_text().splitlines() if "=" in line
    )


def _run(cli_release, tmp_path, monkeypatch, version: str, *, github_result):
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(f'[package]\nname = "c2-cli"\nversion = "{version}"\n')
    output = tmp_path / "github-output.txt"
    output.touch()

    monkeypatch.setattr(cli_release, "CLI_MANIFEST_PATH", str(manifest))
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_REPOSITORY", "world-in-progress/c-two")
    monkeypatch.setenv("GITHUB_TOKEN", "test-token")

    if isinstance(github_result, BaseException):
        ctx = patch("urllib.request.urlopen", side_effect=github_result)
    else:
        ctx = patch("urllib.request.urlopen", return_value=github_result)

    with ctx:
        cli_release.main()

    return _parse_output(output)


def test_default_cli_manifest_path_points_to_root_cli(cli_release):
    expected = _ROOT / "cli" / "Cargo.toml"
    assert Path(cli_release.CLI_MANIFEST_PATH) == expected
    assert expected.is_file()


def test_release_missing_triggers_publish(cli_release, tmp_path, monkeypatch):
    err = urllib.error.HTTPError(
        "https://api.github.com/repos/world-in-progress/c-two/releases/tags/c3-v0.2.0",
        404,
        "Not Found",
        {},
        None,
    )

    result = _run(cli_release, tmp_path, monkeypatch, "0.2.0", github_result=err)

    assert result["should_release"] == "true"
    assert result["should_publish_installer"] == "false"
    assert result["version"] == "0.2.0"
    assert result["tag"] == "c3-v0.2.0"


def test_existing_release_skips_publish(cli_release, tmp_path, monkeypatch):
    result = _run(
        cli_release,
        tmp_path,
        monkeypatch,
        "0.1.0",
        github_result=_mock_github_release(),
    )

    assert result["should_release"] == "false"
    assert result["should_publish_installer"] == "false"
    assert result["version"] == "0.1.0"
    assert result["tag"] == "c3-v0.1.0"


def test_existing_release_missing_installer_triggers_installer_publish(
    cli_release, tmp_path, monkeypatch
):
    result = _run(
        cli_release,
        tmp_path,
        monkeypatch,
        "0.1.0",
        github_result=_mock_github_release(assets=[{"name": "c3-x86_64-unknown-linux-gnu"}]),
    )

    assert result["should_release"] == "false"
    assert result["should_publish_installer"] == "true"
    assert result["version"] == "0.1.0"
    assert result["tag"] == "c3-v0.1.0"


def test_github_api_failure_skips_publish(cli_release, tmp_path, monkeypatch):
    err = urllib.error.URLError("Connection refused")

    result = _run(cli_release, tmp_path, monkeypatch, "0.3.0", github_result=err)

    assert result["should_release"] == "false"
    assert result["should_publish_installer"] == "false"
    assert result["version"] == "0.3.0"
    assert result["tag"] == "c3-v0.3.0"
