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
