"""Compare cli/Cargo.toml version with GitHub Releases.

Outputs to $GITHUB_OUTPUT:
  should_release=true|false
  should_publish_installer=true|false
  version=X.Y.Z
  tag=c3-vX.Y.Z
"""

from __future__ import annotations

import json
import os
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

CLI_MANIFEST_PATH = str(Path(__file__).resolve().parents[2] / "cli" / "Cargo.toml")
GITHUB_API_TIMEOUT = 10
TAG_PREFIX = "c3-v"
INSTALLER_ASSET = "c3-installer.sh"


def _read_local_version() -> str:
    with open(CLI_MANIFEST_PATH, "rb") as f:
        return tomllib.load(f)["package"]["version"]


def _release_state(tag: str) -> tuple[bool | None, bool]:
    repository = os.environ.get("GITHUB_REPOSITORY")
    if not repository:
        return None, False

    url = f"https://api.github.com/repos/{repository}/releases/tags/{tag}"
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"

    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=GITHUB_API_TIMEOUT) as resp:
            data = json.loads(resp.read())
            exists = data.get("tag_name") == tag
            installer_exists = any(
                asset.get("name") == INSTALLER_ASSET
                for asset in data.get("assets", [])
                if isinstance(asset, dict)
            )
            return exists, installer_exists if exists else False
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return False, False
        return None, False
    except Exception:
        return None, False


def main() -> None:
    version = _read_local_version()
    tag = f"{TAG_PREFIX}{version}"
    exists, installer_exists = _release_state(tag)
    should = exists is False
    should_publish_installer = exists is True and not installer_exists

    with open(os.environ["GITHUB_OUTPUT"], "a") as f:
        f.write(f"should_release={'true' if should else 'false'}\n")
        f.write(
            f"should_publish_installer={'true' if should_publish_installer else 'false'}\n"
        )
        f.write(f"version={version}\n")
        f.write(f"tag={tag}\n")

    if should:
        action = "RELEASING"
    elif should_publish_installer:
        action = "BACKFILLING_INSTALLER"
    else:
        action = "SKIPPING"
    print(
        "[check_cli_release] "
        f"version={version} tag={tag} exists={exists} "
        f"installer_exists={installer_exists} -> {action}"
    )


if __name__ == "__main__":
    main()
