# CI/CD Workflows Implementation Plan

> **Status: ✅ COMPLETE** — CI (`ci.yml`) and release (`release.yml`) workflows deployed. PR testing on Python 3.12 + 3.14t, OIDC Trusted Publishing active.

> **For agentic workers:** This plan is historical — all tasks have been completed.

**Goal:** Create PR testing (ci.yml) and version-gated release (release.yml) GitHub Actions workflows for world-in-progress/c-two.

**Architecture:** Two independent workflows — `ci.yml` tests PRs on Python 3.12 + 3.14t, `release.yml` compares pyproject.toml version with PyPI on push to main and triggers multi-platform wheel build + publish via OIDC Trusted Publishing. A standalone Python script `.github/scripts/check_version.py` handles version comparison logic.

**Tech Stack:** GitHub Actions, maturin-action, setup-python (3.14t), setup-uv, dtolnay/rust-toolchain, pypa/gh-action-pypi-publish (OIDC), packaging (PEP 440)

**Spec:** `docs/superpowers/specs/2025-07-26-cicd-workflows-design.md`

---

## File Structure

| File | Responsibility |
|------|----------------|
| `.github/scripts/check_version.py` | Compare pyproject.toml version vs PyPI, output GitHub Actions vars |
| `.github/workflows/ci.yml` | PR test workflow: Python 3.12 + 3.14t on ubuntu-latest |
| `.github/workflows/release.yml` | Release workflow: version-check → build-wheels → build-sdist → publish |
| `tests/unit/test_check_version.py` | Unit tests for version check script |

---

### Task 1: Version Check Script — Tests

**Files:**
- Create: `tests/unit/test_check_version.py`

- [ ] **Step 1: Write unit tests for version check logic**

```python
"""Tests for .github/scripts/check_version.py"""

from __future__ import annotations

import json
import sys
import urllib.error
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

# Add .github/scripts/ to sys.path so we can import check_version
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
```

- [ ] **Step 2: Run tests — expect failure (script doesn't exist yet)**

Run: `uv run pytest tests/unit/test_check_version.py -v`
Expected: FAIL — `.github/scripts/check_version.py` not found

- [ ] **Step 3: Commit test file**

```bash
git add tests/unit/test_check_version.py
git commit -m "test: add unit tests for version check script"
```

---

### Task 2: Version Check Script — Implementation

**Files:**
- Create: `.github/scripts/check_version.py`

- [ ] **Step 1: Create directory structure**

```bash
mkdir -p .github/scripts
```

- [ ] **Step 2: Write the version check script**

```python
"""Compare pyproject.toml version with PyPI to decide if a release is needed.

Outputs to $GITHUB_OUTPUT:
  should_release=true|false
  version=X.Y.Z
"""

from __future__ import annotations

import json
import os
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

from packaging.version import Version

PYPROJECT_PATH = str(Path(__file__).resolve().parents[2] / "pyproject.toml")
PYPI_URL = "https://pypi.org/pypi/c-two/json"
PYPI_TIMEOUT = 10


def _read_local_version() -> str:
    with open(PYPROJECT_PATH, "rb") as f:
        return tomllib.load(f)["project"]["version"]


def _read_pypi_version() -> str | None:
    """Return current PyPI version, or None on failure.

    Returns "0.0.0" on 404 (first publish).
    Returns None on network errors (safe degradation → skip release).
    """
    try:
        resp = urllib.request.urlopen(PYPI_URL, timeout=PYPI_TIMEOUT)
        data = json.loads(resp.read())
        return data["info"]["version"]
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return "0.0.0"
        return None
    except Exception:
        return None


def main() -> None:
    local = _read_local_version()
    pypi = _read_pypi_version()

    if pypi is None:
        should = False
    else:
        should = Version(local) > Version(pypi)

    with open(os.environ["GITHUB_OUTPUT"], "a") as f:
        f.write(f"should_release={'true' if should else 'false'}\n")
        f.write(f"version={local}\n")

    action = "RELEASING" if should else "SKIPPING"
    print(f"[check_version] local={local} pypi={pypi} → {action}")


if __name__ == "__main__":
    main()
```

- [ ] **Step 3: Run tests — expect all pass**

Run: `uv run pytest tests/unit/test_check_version.py -v`
Expected: 6 passed

- [ ] **Step 4: Commit**

```bash
git add .github/scripts/check_version.py tests/unit/test_check_version.py
git commit -m "feat: add version check script with tests"
```

---

### Task 3: CI Workflow

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Create the CI workflow file**

```yaml
name: CI

on:
  pull_request:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        python: ["3.12", "3.14t"]
    name: test (py${{ matrix.python }})
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-python@v5
        with:
          python-version: ${{ matrix.python }}

      - uses: dtolnay/rust-toolchain@stable

      - uses: astral-sh/setup-uv@v6

      - name: Install dependencies & build native extension
        run: uv sync

      - name: Run tests
        run: uv run pytest tests/ -q --timeout=30
        env:
          C2_RELAY_ADDRESS: ""
```

Key design decisions:
- `fail-fast: false` — let both Python versions complete even if one fails (shows full picture)
- `name: test (py${{ matrix.python }})` — clear job names in GitHub UI
- `C2_RELAY_ADDRESS: ""` — disables relay tests that require external server

- [ ] **Step 2: Validate YAML syntax**

Run: `python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"`
(If PyYAML not available, use: `python -c "import json, sys; __import__('yaml')" 2>/dev/null || echo "Skip YAML check"`)

Alternative: just verify the file can be parsed:
```bash
uv run python -c "
import pathlib, re
content = pathlib.Path('.github/workflows/ci.yml').read_text()
assert 'pull_request' in content
assert 'ubuntu-latest' in content
assert '3.14t' in content
assert '3.12' in content
print('ci.yml: structure OK')
"
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add PR testing workflow (Python 3.12 + 3.14t)"
```

---

### Task 4: Release Workflow

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Create the release workflow file**

```yaml
name: Release

on:
  push:
    branches: [main]

jobs:
  # ── Version comparison ──────────────────────────────────────
  version-check:
    runs-on: ubuntu-latest
    outputs:
      should_release: ${{ steps.check.outputs.should_release }}
      version: ${{ steps.check.outputs.version }}
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      - name: Install packaging
        run: pip install packaging

      - name: Check version against PyPI
        id: check
        run: python .github/scripts/check_version.py

  # ── Build wheels (4 platforms × 6 interpreters) ─────────────
  build-wheels:
    needs: version-check
    if: needs.version-check.outputs.should_release == 'true'
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - os: ubuntu-24.04-arm
            target: aarch64-unknown-linux-gnu
          - os: macos-latest
            target: aarch64-apple-darwin
          - os: macos-latest
            target: x86_64-apple-darwin
    name: wheel (${{ matrix.target }})
    steps:
      - uses: actions/checkout@v4

      - uses: PyO3/maturin-action@v1
        with:
          command: build
          target: ${{ matrix.target }}
          args: >-
            --release --out dist
            --interpreter python3.10 python3.11 python3.12
            python3.13 python3.14 python3.14t
          manylinux: auto

      - uses: actions/upload-artifact@v4
        with:
          name: wheels-${{ matrix.target }}
          path: dist/*.whl

  # ── Build source distribution ───────────────────────────────
  build-sdist:
    needs: version-check
    if: needs.version-check.outputs.should_release == 'true'
    runs-on: ubuntu-latest
    name: sdist
    steps:
      - uses: actions/checkout@v4

      - uses: PyO3/maturin-action@v1
        with:
          command: sdist
          args: --out dist

      - uses: actions/upload-artifact@v4
        with:
          name: sdist
          path: dist/*.tar.gz

  # ── Publish to PyPI via OIDC ────────────────────────────────
  publish:
    needs: [build-wheels, build-sdist]
    runs-on: ubuntu-latest
    environment: pypi
    permissions:
      id-token: write
    name: publish
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: dist
          merge-multiple: true

      - uses: pypa/gh-action-pypi-publish@release/v1
```

Key design decisions:
- `fail-fast: false` on build-wheels — one platform failure shouldn't block others from uploading artifacts (publish won't run anyway if any fail, because `needs` requires all to succeed)
- `environment: pypi` + `id-token: write` — OIDC Trusted Publishing, zero secrets
- `merge-multiple: true` — merges all wheel artifacts + sdist into single `dist/` directory
- macOS x86_64 cross-compilation: `macos-latest` (ARM) + `target: x86_64-apple-darwin`

- [ ] **Step 2: Validate YAML structure**

```bash
uv run python -c "
import pathlib
content = pathlib.Path('.github/workflows/release.yml').read_text()
assert 'version-check' in content
assert 'build-wheels' in content
assert 'build-sdist' in content
assert 'publish' in content
assert 'id-token: write' in content
assert 'pypa/gh-action-pypi-publish' in content
assert 'maturin-action' in content
print('release.yml: structure OK')
"
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: add version-gated release workflow with multi-platform wheels"
```

---

### Task 5: Full Validation & Final Commit

**Files:**
- Verify: all 3 new files + 1 test file

- [ ] **Step 1: Run version check tests to confirm nothing broke**

Run: `uv run pytest tests/unit/test_check_version.py -v`
Expected: 6 passed

- [ ] **Step 2: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All tests pass (689+ currently)

- [ ] **Step 3: Verify file tree**

```bash
ls -la .github/scripts/check_version.py
ls -la .github/workflows/ci.yml
ls -la .github/workflows/release.yml
ls -la tests/unit/test_check_version.py
```

Expected: All 4 files present.

- [ ] **Step 4: Review git log**

```bash
git --no-pager log --oneline -5
```

Expected: 3 commits from this plan (tests, ci.yml, release.yml).
