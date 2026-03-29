"""Tests for .github/scripts/check_version.py"""

from __future__ import annotations

import json
import sys
import urllib.error
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

_SCRIPT_DIR = str(Path(__file__).resolve().parents[2] / ".github" / "scripts")


@pytest.fixture(scope="module")
def cv():
    """Import check_version.py as a module."""
    sys.path.insert(0, _SCRIPT_DIR)
    sys.modules.pop("check_version", None)
    import check_version

    yield check_version
    sys.path.remove(_SCRIPT_DIR)
    sys.modules.pop("check_version", None)


def _mock_pypi(version: str) -> MagicMock:
    """Create a mock urllib response returning the given PyPI version."""
    resp = MagicMock()
    resp.read.return_value = json.dumps({"info": {"version": version}}).encode()
    resp.__enter__ = lambda s: s
    resp.__exit__ = MagicMock(return_value=False)
    return resp


def _parse_output(path: Path) -> dict[str, str]:
    """Parse GITHUB_OUTPUT file into a dict."""
    return dict(
        line.split("=", 1) for line in path.read_text().strip().split("\n") if "=" in line
    )


class TestCheckVersion:
    def _run(self, cv, tmp_path, monkeypatch, local_ver, *, pypi_mock=None, pypi_err=None):
        """Helper: write temp pyproject.toml, mock PyPI, run main(), return outputs."""
        toml = tmp_path / "pyproject.toml"
        toml.write_text(f'[project]\nversion = "{local_ver}"\n')
        output = tmp_path / "output.txt"
        output.touch()

        monkeypatch.setattr(cv, "PYPROJECT_PATH", str(toml))
        monkeypatch.setenv("GITHUB_OUTPUT", str(output))

        if pypi_err is not None:
            ctx = patch("urllib.request.urlopen", side_effect=pypi_err)
        else:
            ctx = patch("urllib.request.urlopen", return_value=pypi_mock)

        with ctx:
            cv.main()

        return _parse_output(output)

    def test_local_higher_than_pypi(self, cv, tmp_path, monkeypatch):
        """Local 0.3.0 > PyPI 0.2.7 → should release."""
        result = self._run(cv, tmp_path, monkeypatch, "0.3.0", pypi_mock=_mock_pypi("0.2.7"))
        assert result["should_release"] == "true"
        assert result["version"] == "0.3.0"

    def test_local_equal_to_pypi(self, cv, tmp_path, monkeypatch):
        """Local 0.2.7 == PyPI 0.2.7 → should NOT release."""
        result = self._run(cv, tmp_path, monkeypatch, "0.2.7", pypi_mock=_mock_pypi("0.2.7"))
        assert result["should_release"] == "false"

    def test_local_lower_than_pypi(self, cv, tmp_path, monkeypatch):
        """Local 0.2.6 < PyPI 0.2.7 → should NOT release."""
        result = self._run(cv, tmp_path, monkeypatch, "0.2.6", pypi_mock=_mock_pypi("0.2.7"))
        assert result["should_release"] == "false"

    def test_pypi_404_first_publish(self, cv, tmp_path, monkeypatch):
        """PyPI 404 (package doesn't exist yet) → should release."""
        err = urllib.error.HTTPError(
            "https://pypi.org/pypi/c-two/json", 404, "Not Found", {}, None
        )
        result = self._run(cv, tmp_path, monkeypatch, "0.1.0", pypi_err=err)
        assert result["should_release"] == "true"

    def test_pypi_unreachable(self, cv, tmp_path, monkeypatch):
        """Network error → safe degradation, should NOT release."""
        err = urllib.error.URLError("Connection refused")
        result = self._run(cv, tmp_path, monkeypatch, "0.3.0", pypi_err=err)
        assert result["should_release"] == "false"

    def test_pypi_500_error(self, cv, tmp_path, monkeypatch):
        """PyPI 500 server error → safe degradation, should NOT release."""
        err = urllib.error.HTTPError(
            "https://pypi.org/pypi/c-two/json", 500, "Server Error", {}, None
        )
        result = self._run(cv, tmp_path, monkeypatch, "0.3.0", pypi_err=err)
        assert result["should_release"] == "false"
