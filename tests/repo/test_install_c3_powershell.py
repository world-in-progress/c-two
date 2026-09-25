"""Behavioral tests for cli/install-c3.ps1 (requires PowerShell; skipped elsewhere)."""

from __future__ import annotations

import hashlib
import http.server
import os
import shutil
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

import pytest

pwsh = shutil.which("pwsh") or shutil.which("powershell")

pytestmark = pytest.mark.skipif(pwsh is None, reason="PowerShell is not installed")

_ASSET_NAME = "c3-x86_64-pc-windows-msvc.exe"


def _repo_root() -> Path:
    return next(
        parent
        for parent in Path(__file__).resolve().parents
        if (parent / "cli").is_dir()
    )


def _installer() -> Path:
    return _repo_root() / "cli" / "install-c3.ps1"


def _run_installer(
    *args: str,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    merged_env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("C3_")
    }
    if env:
        merged_env.update(env)
    return subprocess.run(
        [pwsh, "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(_installer()), *args],
        check=check,
        env=merged_env,
        text=True,
        capture_output=True,
    )


def _run_pwsh(command: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [pwsh, "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
        check=False,
        text=True,
        capture_output=True,
    )


def _sidecar(payload: bytes, name: str = _ASSET_NAME) -> bytes:
    return f"{hashlib.sha256(payload).hexdigest()}  {name}\n".encode()


def test_target_detection_without_powershell_core_platform_key():
    script = str(_installer()).replace("'", "''")
    result = _run_pwsh(
        "$PSVersionTable.Remove('Platform'); "
        "$env:C3_INSTALLER_OS='windows'; $env:C3_INSTALLER_ARCH='AMD64'; "
        f"& '{script}' -PrintTarget"
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "x86_64-pc-windows-msvc"


def _leftover_temp_dirs() -> list[str]:
    return sorted(path.name for path in Path(tempfile.gettempdir()).glob("c3-install-*"))


class _ReleaseMirror:
    """Local stand-in for GitHub release asset downloads; unknown paths 404."""

    def __init__(self, files: dict[str, bytes]) -> None:
        self.requested: list[str] = []
        mirror = self

        class _Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                mirror.requested.append(self.path)
                body = files.get(self.path)
                if body is None:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                return

        self._server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def base_url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def test_installer_has_no_powershell_syntax_errors():
    result = _run_pwsh(
        "$tokens = $null; $errors = $null; "
        f"[void][System.Management.Automation.Language.Parser]::ParseFile("
        f"'{_installer()}', [ref]$tokens, [ref]$errors); "
        "if ($errors.Count -gt 0) { $errors | ForEach-Object { $_.Message }; exit 1 } "
        "else { 'parse-ok' }"
    )

    assert result.returncode == 0
    assert "parse-ok" in result.stdout


def test_installer_help_uses_valid_keywords_and_documents_options():
    result = _run_pwsh(f"Get-Help -Name '{_installer()}' -Full")

    assert result.returncode == 0
    text = result.stdout + result.stderr
    assert "Install the c3 CLI from GitHub Releases (Windows)" in text
    assert "-BinDir" in text
    assert "-PrintTarget" in text
    # The .NOTES section must carry the environment documentation; the
    # invalid ".Environment" help keyword must stay out of the file.
    assert "C3_RELEASE_BASE_URL" in text
    installer_text = _installer().read_text(encoding="utf-8")
    assert ".NOTES" in installer_text
    assert ".Environment" not in installer_text


def test_installer_resolves_windows_x64_target_from_overrides():
    x86_64 = _run_installer(
        "-PrintTarget",
        env={"C3_INSTALLER_OS": "windows", "C3_INSTALLER_ARCH": "x86_64"},
    )
    amd64 = _run_installer(
        "-PrintTarget",
        env={"C3_INSTALLER_OS": "windows", "C3_INSTALLER_ARCH": "AMD64"},
    )
    from_env = _run_installer("-PrintTarget", env={"C3_TARGET": "x86_64-pc-windows-msvc"})

    assert x86_64.stdout.strip() == "x86_64-pc-windows-msvc"
    assert amd64.stdout.strip() == "x86_64-pc-windows-msvc"
    assert from_env.stdout.strip() == "x86_64-pc-windows-msvc"


def test_installer_rejects_unsupported_target_overrides():
    bad_arch = _run_installer(
        "-PrintTarget",
        env={"C3_INSTALLER_OS": "windows", "C3_INSTALLER_ARCH": "aarch64"},
        check=False,
    )
    bad_os = _run_installer(
        "-PrintTarget",
        env={"C3_INSTALLER_OS": "linux", "C3_INSTALLER_ARCH": "x86_64"},
        check=False,
    )

    assert bad_arch.returncode != 0
    assert "unsupported CPU architecture" in bad_arch.stderr
    assert bad_os.returncode != 0
    assert "unsupported operating system" in bad_os.stderr


@pytest.mark.skipif(sys.platform == "win32", reason="fixture is not a runnable Windows PE")
def test_installer_downloads_verifies_and_installs_from_release_base(tmp_path):
    payload = b"MZ fake windows c3 binary"
    mirror = _ReleaseMirror(
        {
            f"/{_ASSET_NAME}": payload,
            f"/{_ASSET_NAME}.sha256": _sidecar(payload),
        }
    )
    bin_dir = tmp_path / "bin"
    try:
        result = _run_installer(
            "-Version",
            "0.0.0-test",
            "-BinDir",
            str(bin_dir),
            env={
                "C3_TARGET": "x86_64-pc-windows-msvc",
                "C3_RELEASE_BASE_URL": mirror.base_url,
            },
        )
        requested = list(mirror.requested)
    finally:
        mirror.close()

    assert requested == [f"/{_ASSET_NAME}", f"/{_ASSET_NAME}.sha256"]
    assert (bin_dir / "c3.exe").read_bytes() == payload
    assert "Installed c3 to" in result.stdout
    assert result.stdout.strip().endswith("Installed c3 to " + str(bin_dir / "c3.exe"))
    assert _leftover_temp_dirs() == []


def test_installer_rejects_checksum_mismatch_and_cleans_up(tmp_path):
    payload = b"MZ fake windows c3 binary"
    mirror = _ReleaseMirror(
        {
            f"/{_ASSET_NAME}": payload,
            f"/{_ASSET_NAME}.sha256": _sidecar(b"tampered bytes"),
        }
    )
    bin_dir = tmp_path / "bin"
    try:
        result = _run_installer(
            "-Version",
            "0.0.0-test",
            "-BinDir",
            str(bin_dir),
            env={
                "C3_TARGET": "x86_64-pc-windows-msvc",
                "C3_RELEASE_BASE_URL": mirror.base_url,
            },
            check=False,
        )
    finally:
        mirror.close()

    assert result.returncode != 0
    assert "checksum mismatch" in result.stderr
    assert not (bin_dir / "c3.exe").exists()
    assert _leftover_temp_dirs() == []


def test_installer_rejects_missing_checksum_sidecar_and_cleans_up(tmp_path):
    payload = b"MZ fake windows c3 binary"
    mirror = _ReleaseMirror({f"/{_ASSET_NAME}": payload})
    bin_dir = tmp_path / "bin"
    try:
        result = _run_installer(
            "-Version",
            "0.0.0-test",
            "-BinDir",
            str(bin_dir),
            env={
                "C3_TARGET": "x86_64-pc-windows-msvc",
                "C3_RELEASE_BASE_URL": mirror.base_url,
            },
            check=False,
        )
    finally:
        mirror.close()

    assert result.returncode != 0
    assert "c3 installer:" in result.stderr
    assert ".sha256" in result.stderr
    assert not (bin_dir / "c3.exe").exists()
    assert _leftover_temp_dirs() == []


def test_installer_rejects_corrupt_checksum_sidecar_and_cleans_up(tmp_path):
    payload = b"MZ fake windows c3 binary"
    mirror = _ReleaseMirror(
        {
            f"/{_ASSET_NAME}": payload,
            f"/{_ASSET_NAME}.sha256": b"<html>mirror error page</html>\n",
        }
    )
    bin_dir = tmp_path / "bin"
    try:
        result = _run_installer(
            "-Version",
            "0.0.0-test",
            "-BinDir",
            str(bin_dir),
            env={
                "C3_TARGET": "x86_64-pc-windows-msvc",
                "C3_RELEASE_BASE_URL": mirror.base_url,
            },
            check=False,
        )
    finally:
        mirror.close()

    assert result.returncode != 0
    assert "does not contain a sha256 digest" in result.stderr
    assert not (bin_dir / "c3.exe").exists()
    assert _leftover_temp_dirs() == []
