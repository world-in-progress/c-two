from __future__ import annotations

import hashlib
import os
import stat
import subprocess
from pathlib import Path


def _repo_root() -> Path:
    return next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / "cli").is_dir()
    )


def _installer() -> Path:
    return _repo_root() / "cli" / "install-c3.sh"


def _run_installer(
    *args: str,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    return subprocess.run(
        ["sh", str(_installer()), *args],
        check=check,
        env=merged_env,
        text=True,
        capture_output=True,
    )


def test_installer_help_documents_short_install_options():
    result = _run_installer("--help")

    assert "Install the c3 CLI" in result.stdout
    assert "-b, --bin-dir DIR" in result.stdout
    assert "--target TARGET" in result.stdout


def test_installer_reports_missing_option_values_cleanly():
    result = _run_installer("--version", check=False)

    assert result.returncode != 0
    assert "--version requires a value" in result.stderr


def test_installer_detects_linux_targets_from_uname_overrides():
    amd64 = _run_installer(
        "--print-target",
        env={"C3_INSTALLER_OS": "linux", "C3_INSTALLER_ARCH": "x86_64"},
    )
    arm64 = _run_installer(
        "--print-target",
        env={"C3_INSTALLER_OS": "linux", "C3_INSTALLER_ARCH": "aarch64"},
    )

    assert amd64.stdout.strip() == "x86_64-unknown-linux-gnu"
    assert arm64.stdout.strip() == "aarch64-unknown-linux-gnu"


def test_installer_detects_macos_targets_from_uname_overrides():
    apple_silicon = _run_installer(
        "--print-target",
        env={"C3_INSTALLER_OS": "darwin", "C3_INSTALLER_ARCH": "arm64"},
    )
    intel = _run_installer(
        "--print-target",
        env={"C3_INSTALLER_OS": "darwin", "C3_INSTALLER_ARCH": "amd64"},
    )

    assert apple_silicon.stdout.strip() == "aarch64-apple-darwin"
    assert intel.stdout.strip() == "x86_64-apple-darwin"


def test_installer_downloads_verifies_and_installs_from_release_base(tmp_path):
    target = "x86_64-unknown-linux-gnu"
    asset_name = f"c3-{target}"
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    asset = release_dir / asset_name
    asset.write_text("#!/bin/sh\necho c3 test binary\n", encoding="utf-8")
    asset.chmod(asset.stat().st_mode | stat.S_IXUSR)
    digest = hashlib.sha256(asset.read_bytes()).hexdigest()
    (release_dir / f"{asset_name}.sha256").write_text(
        f"{digest}  {asset_name}\n",
        encoding="utf-8",
    )
    bin_dir = tmp_path / "bin"

    result = _run_installer(
        "--version",
        "0.0.0-test",
        "--bin-dir",
        str(bin_dir),
        env={
            "C3_INSTALLER_OS": "linux",
            "C3_INSTALLER_ARCH": "x86_64",
            "C3_RELEASE_BASE_URL": release_dir.as_uri(),
        },
    )

    installed = bin_dir / "c3"
    assert installed.read_text(encoding="utf-8") == asset.read_text(encoding="utf-8")
    assert os.access(installed, os.X_OK)
    assert "Installed c3 to" in result.stdout


def test_installer_fails_before_installing_when_checksum_mismatches(tmp_path):
    target = "x86_64-unknown-linux-gnu"
    asset_name = f"c3-{target}"
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    (release_dir / asset_name).write_text("#!/bin/sh\necho c3 test binary\n", encoding="utf-8")
    (release_dir / f"{asset_name}.sha256").write_text(
        f"{'0' * 64}  {asset_name}\n",
        encoding="utf-8",
    )
    bin_dir = tmp_path / "bin"

    result = _run_installer(
        "--version",
        "0.0.0-test",
        "--bin-dir",
        str(bin_dir),
        check=False,
        env={
            "C3_INSTALLER_OS": "linux",
            "C3_INSTALLER_ARCH": "x86_64",
            "C3_RELEASE_BASE_URL": release_dir.as_uri(),
        },
    )

    assert result.returncode != 0
    assert not (bin_dir / "c3").exists()


def test_installer_fails_before_installing_when_downloaded_binary_cannot_run(tmp_path):
    target = "x86_64-unknown-linux-gnu"
    asset_name = f"c3-{target}"
    release_dir = tmp_path / "release"
    release_dir.mkdir()
    asset = release_dir / asset_name
    asset.write_text("not a runnable binary\n", encoding="utf-8")
    digest = hashlib.sha256(asset.read_bytes()).hexdigest()
    (release_dir / f"{asset_name}.sha256").write_text(
        f"{digest}  {asset_name}\n",
        encoding="utf-8",
    )
    bin_dir = tmp_path / "bin"

    result = _run_installer(
        "--version",
        "0.0.0-test",
        "--bin-dir",
        str(bin_dir),
        check=False,
        env={
            "C3_INSTALLER_OS": "linux",
            "C3_INSTALLER_ARCH": "x86_64",
            "C3_RELEASE_BASE_URL": release_dir.as_uri(),
        },
    )

    assert result.returncode != 0
    assert "downloaded c3 binary failed to run" in result.stderr
    assert not (bin_dir / "c3").exists()
