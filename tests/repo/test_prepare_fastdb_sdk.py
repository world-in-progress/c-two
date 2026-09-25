from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile

import pytest


REPOSITORY = Path(__file__).resolve().parents[2]
HELPER_PATH = REPOSITORY / ".github" / "scripts" / "prepare_fastdb_sdk.py"
# The Host provided the official v0.2.1 release assets read-only under /tmp for
# local verification. They are not present on CI runners; the hermetic tests
# below build synthetic releases instead of downloading anything.
PROVIDED_RELEASE_FIXTURES = Path("/tmp/fastdb-021-main-candidate/release")

SYNTHETIC_VERSION = "9.9.9"
SYNTHETIC_SOURCE_SHA = "f" * 40


def load_helper():
    spec = importlib.util.spec_from_file_location("prepare_fastdb_sdk", HELPER_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def default_members(target: str) -> dict[str, bytes]:
    members = {
        "LICENSE": b"synthetic license\n",
        "include/fastdb_payload.h": b"/* synthetic header */\n",
    }
    if target == "x86_64-pc-windows-msvc":
        members["lib/fastdb.dll"] = b"MZ" + bytes(64)
        members["lib/fastdb.lib"] = b"synthetic import library" * 4
    elif target == "aarch64-apple-darwin":
        members["lib/libfastdb.dylib"] = b"synthetic dylib" * 16
    else:
        members["lib/libfastdb.so"] = b"synthetic elf shared object" * 16
    return members


def bundle_manifest_for(
    target: str,
    members: dict[str, bytes],
    *,
    version: str = SYNTHETIC_VERSION,
    source_sha: str = SYNTHETIC_SOURCE_SHA,
) -> dict:
    manifest = {
        "schema": "fastdb.core-bundle.v1",
        "version": version,
        "target": target,
        "abi": "fdb_payload_v1",
        "abi_symbol_count": 117,
        "link_mode": "system",
        "source_sha": source_sha,
        "files": [
            {
                "path": path,
                "bytes": len(data),
                "sha256": sha256_bytes(data),
            }
            for path, data in sorted(members.items())
        ],
    }
    if target == "x86_64-pc-windows-msvc":
        manifest["windows_runtime_dll"] = "lib/fastdb.dll"
        manifest["windows_import_library"] = "lib/fastdb.lib"
    return manifest


def tar_gz(members: dict[str, bytes]) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, data in sorted(members.items()):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def build_release_dir(
    directory: Path,
    *,
    target: str = "x86_64-unknown-linux-gnu",
    members: dict[str, bytes] | None = None,
    omit_members: tuple[str, ...] = (),
    bundle_manifest: dict | None = None,
    pinned_bundle_sha256: str | None = None,
    pinned_bundle_bytes: int | None = None,
    release_manifest_mutator=None,
) -> Path:
    """Write a synthetic official release directory (manifest plus one bundle)."""
    selected = {
        path: data
        for path, data in (members if members is not None else default_members(target)).items()
        if path not in omit_members
    }
    manifest = bundle_manifest if bundle_manifest is not None else bundle_manifest_for(target, selected)
    archive_bytes = tar_gz({**selected, "manifest.json": json.dumps(manifest).encode()})
    entry = {
        "kind": "core-bundle",
        "name": f"fastdb-core-{SYNTHETIC_VERSION}-{target}.tar.gz",
        "bytes": pinned_bundle_bytes if pinned_bundle_bytes is not None else len(archive_bytes),
        "sha256": pinned_bundle_sha256 or sha256_bytes(archive_bytes),
        "target": target,
    }
    release_manifest = {
        "schema": "fastdb.release-manifest.v1",
        "version": SYNTHETIC_VERSION,
        "source_sha": SYNTHETIC_SOURCE_SHA,
        "artifacts": [entry],
    }
    if release_manifest_mutator is not None:
        release_manifest_mutator(release_manifest)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "release-manifest.json").write_text(json.dumps(release_manifest))
    (directory / entry["name"]).write_bytes(archive_bytes)
    return directory


@pytest.fixture
def synthetic_release(tmp_path, monkeypatch):
    """Build a synthetic release directory and pin the helper to its constants."""

    def make(**kwargs) -> tuple[object, Path]:
        module = load_helper()
        release_dir = build_release_dir(tmp_path / "release", **kwargs)
        manifest_bytes = (release_dir / "release-manifest.json").read_bytes()
        monkeypatch.setattr(module, "RELEASE_VERSION", SYNTHETIC_VERSION)
        monkeypatch.setattr(module, "EXPECTED_SOURCE_SHA", SYNTHETIC_SOURCE_SHA)
        monkeypatch.setattr(module, "RELEASE_MANIFEST_SHA256", sha256_bytes(manifest_bytes))
        return module, release_dir

    return make


def run_prepare(module, *arguments: str, target: str = "x86_64-unknown-linux-gnu") -> int:
    return module.main(["--target", target, *arguments])


def failure_message(module, capsys) -> str:
    captured = capsys.readouterr()
    assert captured.err, "expected the helper to explain the failure on stderr"
    return captured.err


def test_real_release_fixtures_prepare_every_published_target(tmp_path):
    if not PROVIDED_RELEASE_FIXTURES.is_dir():
        pytest.skip(f"provided release fixtures are absent: {PROVIDED_RELEASE_FIXTURES}")
    module = load_helper()
    for target, library in (
        ("x86_64-unknown-linux-gnu", "libfastdb.so"),
        ("aarch64-apple-darwin", "libfastdb.dylib"),
        ("x86_64-pc-windows-msvc", "fastdb.dll"),
    ):
        output = tmp_path / target
        assert module.main([
            "--from-dir", str(PROVIDED_RELEASE_FIXTURES),
            "--target", target,
            "--output", str(output),
        ]) == 0
        lib_dir = output / "lib"
        assert (lib_dir / library).is_file()
        assert (output / "manifest.json").is_file()
        if target == "x86_64-pc-windows-msvc":
            assert (lib_dir / "fastdb.lib").is_file()
        # The fixture directory must be consumed read-only.
        assert not any(path.suffix == ".download" for path in PROVIDED_RELEASE_FIXTURES.iterdir())


def test_real_release_fixture_tampering_is_rejected(tmp_path):
    if not PROVIDED_RELEASE_FIXTURES.is_dir():
        pytest.skip(f"provided release fixtures are absent: {PROVIDED_RELEASE_FIXTURES}")
    module = load_helper()
    manifest = json.loads((PROVIDED_RELEASE_FIXTURES / "release-manifest.json").read_bytes())
    manifest["source_sha"] = "0" * 40
    tampered = tmp_path / "release"
    tampered.mkdir()
    (tampered / "release-manifest.json").write_text(json.dumps(manifest))
    assert run_prepare(
        module, "--from-dir", str(tampered), "--output", str(tmp_path / "sdk")
    ) == 1
    assert not (tmp_path / "sdk").exists()


def test_release_manifest_pin_mismatch_is_rejected(synthetic_release, tmp_path, capsys):
    module, release_dir = synthetic_release()
    module.RELEASE_MANIFEST_SHA256 = "0" * 64
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk")
    ) == 1
    assert "public pin" in failure_message(module, capsys)


def test_release_manifest_identity_mismatches_are_rejected(synthetic_release, tmp_path, capsys):
    cases = {
        "schema": lambda manifest: manifest.update(schema="fastdb.release-manifest.v0"),
        "version": lambda manifest: manifest.update(version="0.0.1"),
        "source_sha": lambda manifest: manifest.update(source_sha="e" * 40),
    }
    for field, mutator in cases.items():
        module, release_dir = synthetic_release(release_manifest_mutator=mutator)
        assert run_prepare(
            module, "--from-dir", str(release_dir), "--output", str(tmp_path / f"sdk-{field}")
        ) == 1, field
        assert field in failure_message(module, capsys)


def test_missing_core_bundle_for_target_fails_honestly(synthetic_release, tmp_path, capsys):
    module, release_dir = synthetic_release(target="x86_64-unknown-linux-gnu")
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk"),
        target="x86_64-pc-windows-msvc",
    ) == 1
    message = failure_message(module, capsys)
    assert "no core bundle" in message and "x86_64-unknown-linux-gnu" in message


def test_archive_hash_and_size_mismatches_are_rejected(synthetic_release, tmp_path, capsys):
    module, release_dir = synthetic_release(pinned_bundle_sha256="a" * 64)
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-hash")
    ) == 1
    assert "release manifest pins" in failure_message(module, capsys)

    module, release_dir = synthetic_release(pinned_bundle_bytes=1)
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-bytes")
    ) == 1
    assert "bytes" in failure_message(module, capsys)


def test_member_hash_and_size_mismatches_are_rejected(synthetic_release, tmp_path, capsys):
    def corrupt_sha(manifest):
        manifest["files"][0]["sha256"] = "b" * 64

    def corrupt_size(manifest):
        manifest["files"][0]["bytes"] += 1

    for name, mutator in (("sha256", corrupt_sha), ("bytes", corrupt_size)):
        members = default_members("x86_64-unknown-linux-gnu")
        manifest = bundle_manifest_for("x86_64-unknown-linux-gnu", members)
        mutator(manifest)
        module, release_dir = synthetic_release(bundle_manifest=manifest)
        assert run_prepare(
            module, "--from-dir", str(release_dir), "--output", str(tmp_path / f"sdk-{name}")
        ) == 1, name
        assert name in failure_message(module, capsys)


def test_unlisted_and_missing_archive_members_are_rejected(synthetic_release, tmp_path, capsys):
    target = "x86_64-unknown-linux-gnu"
    smuggled = {**default_members(target), "evil.txt": b"undeclared"}
    module, release_dir = synthetic_release(
        members=smuggled,
        bundle_manifest=bundle_manifest_for(target, default_members(target)),
    )
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-extra")
    ) == 1
    assert "does not declare" in failure_message(module, capsys)
    assert not (tmp_path / "sdk-extra" / "evil.txt").exists()

    declared_only = default_members("x86_64-unknown-linux-gnu")
    manifest = bundle_manifest_for("x86_64-unknown-linux-gnu", declared_only)
    manifest["files"].append(
        {"path": "lib/ghost.so", "bytes": 1, "sha256": "c" * 64}
    )
    module, release_dir = synthetic_release(bundle_manifest=manifest)
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-ghost")
    ) == 1
    assert "does not contain" in failure_message(module, capsys)


def test_unsafe_archive_members_are_rejected_before_extraction(synthetic_release, tmp_path, capsys):
    members = default_members("x86_64-unknown-linux-gnu")
    manifest = bundle_manifest_for("x86_64-unknown-linux-gnu", members)

    def archive_with_raw_member(raw_name: str, raw_type: bytes, linkname: str = "") -> Path:
        selected = {**members, "manifest.json": json.dumps(manifest).encode()}
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
            for name, data in sorted(selected.items()):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
            evil = tarfile.TarInfo(raw_name)
            evil.size = len(raw_type)
            if linkname:
                evil.type = tarfile.SYMTYPE
                evil.linkname = linkname
            archive.addfile(evil, io.BytesIO(raw_type))
        release_dir = tmp_path / f"release-{abs(hash(raw_name))}"
        release_dir.mkdir()
        archive_bytes = buffer.getvalue()
        (release_dir / f"fastdb-core-{SYNTHETIC_VERSION}-x86_64-unknown-linux-gnu.tar.gz").write_bytes(archive_bytes)
        (release_dir / "release-manifest.json").write_text(json.dumps({
            "schema": "fastdb.release-manifest.v1",
            "version": SYNTHETIC_VERSION,
            "source_sha": SYNTHETIC_SOURCE_SHA,
            "artifacts": [{
                "kind": "core-bundle",
                "name": f"fastdb-core-{SYNTHETIC_VERSION}-x86_64-unknown-linux-gnu.tar.gz",
                "bytes": len(archive_bytes),
                "sha256": sha256_bytes(archive_bytes),
                "target": "x86_64-unknown-linux-gnu",
            }],
        }))
        return release_dir

    module = load_helper()
    module.RELEASE_VERSION = SYNTHETIC_VERSION
    module.EXPECTED_SOURCE_SHA = SYNTHETIC_SOURCE_SHA
    for raw_name, linkname in (
        ("/etc/passwd", ""),
        ("../escapee", ""),
        ("C:/Windows/system32/fastdb.dll", ""),
        ("lib\\libfastdb.so", ""),
        ("lib/evil-link", "../../etc/passwd"),
    ):
        release_dir = archive_with_raw_member(raw_name, b"payload", linkname)
        module.RELEASE_MANIFEST_SHA256 = sha256_bytes((release_dir / "release-manifest.json").read_bytes())
        output = tmp_path / f"sdk-{abs(hash(raw_name))}"
        assert run_prepare(
            module, "--from-dir", str(release_dir), "--output", str(output)
        ) == 1, raw_name
        message = failure_message(module, capsys)
        assert any(
            marker in message
            for marker in ("unsafe path", "link", "backslashes", "absolute path", "drive-letter")
        )
        assert not (tmp_path / "escapee").exists()
        assert not output.exists() or not any(output.iterdir())


def test_windows_bundle_requires_dll_and_import_library(synthetic_release, tmp_path, capsys):
    module, release_dir = synthetic_release(
        target="x86_64-pc-windows-msvc", omit_members=("lib/fastdb.dll",)
    )
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-dll"),
        target="x86_64-pc-windows-msvc",
    ) == 1
    assert "lib/fastdb.dll" in failure_message(module, capsys)

    module, release_dir = synthetic_release(
        target="x86_64-pc-windows-msvc", omit_members=("lib/fastdb.lib",)
    )
    assert run_prepare(
        module, "--from-dir", str(release_dir), "--output", str(tmp_path / "sdk-lib"),
        target="x86_64-pc-windows-msvc",
    ) == 1
    message = failure_message(module, capsys)
    assert "windows_import_library" in message and "fastdb.lib" in message


def test_bundle_manifest_field_mismatches_are_rejected(synthetic_release, tmp_path, capsys):
    members = default_members("x86_64-unknown-linux-gnu")
    cases = {
        "schema": {"schema": "fastdb.core-bundle.v0"},
        "version": {"version": "0.2.0"},
        "target": {"target": "aarch64-apple-darwin"},
        "abi": {"abi": "fdb_payload_v0"},
        "abi_symbol_count": {"abi_symbol_count": 116},
        "link_mode": {"link_mode": "source"},
        "source_sha": {"source_sha": "e" * 40},
    }
    for name, override in cases.items():
        manifest = bundle_manifest_for("x86_64-unknown-linux-gnu", members)
        manifest.update(override)
        module, release_dir = synthetic_release(bundle_manifest=manifest)
        assert run_prepare(
            module, "--from-dir", str(release_dir), "--output", str(tmp_path / f"sdk-{name}")
        ) == 1, name
        assert name in failure_message(module, capsys)


def test_unsupported_target_argument_is_a_usage_error(tmp_path):
    module = load_helper()
    with pytest.raises(SystemExit) as exit_info:
        run_prepare(module, "--output", str(tmp_path / "sdk"), target="aarch64-unknown-linux-gnu")
    assert exit_info.value.code == 2


def test_unsupported_host_fails_honestly(monkeypatch, tmp_path, capsys):
    module = load_helper()
    monkeypatch.setattr(module.platform, "system", lambda: "Linux")
    monkeypatch.setattr(module.platform, "machine", lambda: "aarch64")
    assert module.main(["--output", str(tmp_path / "sdk")]) == 1
    message = failure_message(module, capsys)
    assert "no published FastDB CoreSDK" in message
    assert "x86_64-unknown-linux-gnu" in message and "aarch64-apple-darwin" in message


def test_host_target_detection_mappings(monkeypatch):
    module = load_helper()
    def host(system, machine):
        monkeypatch.setattr(module.platform, "system", lambda: system)
        monkeypatch.setattr(module.platform, "machine", lambda: machine)
        return module.detect_host_target()

    assert host("Linux", "x86_64") == "x86_64-unknown-linux-gnu"
    assert host("Darwin", "arm64") == "aarch64-apple-darwin"
    assert host("Windows", "AMD64") == "x86_64-pc-windows-msvc"
    with pytest.raises(module.PrepareError):
        host("Darwin", "x86_64")


def test_step_outputs_do_not_require_job_wide_environment(synthetic_release, tmp_path, monkeypatch):
    module, release_dir = synthetic_release()
    output_file = tmp_path / "github-output"
    env_file = tmp_path / "github-env"
    path_file = tmp_path / "github-path"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output_file))
    monkeypatch.setenv("GITHUB_ENV", str(env_file))
    monkeypatch.setenv("GITHUB_PATH", str(path_file))
    sdk = tmp_path / "sdk"
    assert run_prepare(module, "--from-dir", str(release_dir), "--output", str(sdk)) == 0
    assert f"lib-dir={(sdk / 'lib').resolve()}" in output_file.read_text()
    assert not env_file.exists()
    assert not path_file.exists()


def test_emit_github_environment_for_linux_composes_loader_path(
    synthetic_release, tmp_path, monkeypatch, capsys
):
    module, release_dir = synthetic_release(target="x86_64-unknown-linux-gnu")
    env_file = tmp_path / "github-env"
    path_file = tmp_path / "github-path"
    output_file = tmp_path / "github-output"
    monkeypatch.setenv("LD_LIBRARY_PATH", "/opt/other-libs")
    assert run_prepare(
        module,
        "--from-dir", str(release_dir),
        "--output", str(tmp_path / "sdk"),
        "--emit-github",
        "--github-env-file", str(env_file),
        "--github-path-file", str(path_file),
        "--github-output-file", str(output_file),
    ) == 0
    environment = env_file.read_text().splitlines()
    lib_dir = (tmp_path / "sdk" / "lib").resolve()
    assert "FASTDB_PAYLOAD_LINK_MODE=system" in environment
    assert f"FASTDB_PAYLOAD_SYSTEM_LIB_DIR={lib_dir}" in environment
    assert f"LD_LIBRARY_PATH={lib_dir}{os.pathsep}/opt/other-libs" in environment
    assert str(lib_dir).startswith(str(tmp_path))
    # The loader PATH file is a Windows-only concern.
    assert not path_file.exists()
    outputs = dict(
        line.split("=", 1) for line in output_file.read_text().splitlines()
    )
    assert outputs["lib-dir"] == str(lib_dir)
    assert outputs["target"] == "x86_64-unknown-linux-gnu"
    assert outputs["version"] == SYNTHETIC_VERSION


def test_emit_github_environment_for_windows_appends_loader_path(
    synthetic_release, tmp_path
):
    module, release_dir = synthetic_release(target="x86_64-pc-windows-msvc")
    env_file = tmp_path / "github-env"
    path_file = tmp_path / "github-path"
    assert run_prepare(
        module,
        "--from-dir", str(release_dir),
        "--output", str(tmp_path / "sdk"),
        "--emit-github",
        "--github-env-file", str(env_file),
        "--github-path-file", str(path_file),
        "--github-output-file", str(tmp_path / "github-output"),
        target="x86_64-pc-windows-msvc",
    ) == 0
    environment = env_file.read_text().splitlines()
    assert "FASTDB_PAYLOAD_LINK_MODE=system" in environment
    assert any(line.startswith("FASTDB_PAYLOAD_SYSTEM_LIB_DIR=") for line in environment)
    assert not any(line.startswith("LD_LIBRARY_PATH") for line in environment)
    lib_dir = (tmp_path / "sdk" / "lib").resolve()
    assert path_file.read_text().splitlines() == [str(lib_dir)]


def test_emit_github_environment_for_macos_uses_dyld_fallback(
    synthetic_release, tmp_path
):
    module, release_dir = synthetic_release(target="aarch64-apple-darwin")
    env_file = tmp_path / "github-env"
    assert run_prepare(
        module,
        "--from-dir", str(release_dir),
        "--output", str(tmp_path / "sdk"),
        "--emit-github",
        "--github-env-file", str(env_file),
        target="aarch64-apple-darwin",
    ) == 0
    environment = env_file.read_text().splitlines()
    assert any(line.startswith("DYLD_LIBRARY_PATH=") for line in environment)
    assert not any(line.startswith("LD_LIBRARY_PATH") for line in environment)


def test_emission_never_mutates_the_current_process(synthetic_release, tmp_path):
    module, release_dir = synthetic_release()
    before = dict(os.environ)
    assert run_prepare(
        module,
        "--from-dir", str(release_dir),
        "--output", str(tmp_path / "sdk"),
        "--emit-github",
        "--github-env-file", str(tmp_path / "github-env"),
        "--github-output-file", str(tmp_path / "github-output"),
    ) == 0
    assert dict(os.environ) == before


def test_non_empty_output_directory_is_rejected(synthetic_release, tmp_path, capsys):
    module, release_dir = synthetic_release()
    output = tmp_path / "sdk"
    output.mkdir()
    (output / "stale.txt").write_text("stale")
    assert run_prepare(module, "--from-dir", str(release_dir), "--output", str(output)) == 1
    assert "not empty" in failure_message(module, capsys)
    assert (output / "stale.txt").read_text() == "stale"
