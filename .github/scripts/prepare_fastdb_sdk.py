"""Download and verify the official FastDB CoreSDK for system link mode.

Fetches the release manifest and the matching fastdb-core-<version>-<target>
archive from the official FastDB GitHub release, verifies the public manifest
pin, the expected source revision, the archive hash and every internal member
hash, rejects unsafe archive entries and missing Windows DLL/import-library
members, and extracts the verified bundle under an explicit output directory.
With --emit-github it appends FASTDB_PAYLOAD_LINK_MODE=system, the absolute
FASTDB_PAYLOAD_SYSTEM_LIB_DIR, and the platform loader path to the GitHub
environment files. It never mutates the current process or user environment.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import sys
import tarfile
import urllib.request

RELEASE_VERSION = "0.2.1"
EXPECTED_SOURCE_SHA = "4f99f86a662b0e950a0dd29800c25a1c9fca4def"
RELEASE_MANIFEST_NAME = "release-manifest.json"
RELEASE_MANIFEST_SHA256 = (
    "a741c33ef2caa9925f7aa496a4ec81dc16c642f02a29bcef471e95264a1e7ded"
)
RELEASE_REPOSITORY = "world-in-progress/fastdb"
RELEASE_MANIFEST_SCHEMA = "fastdb.release-manifest.v1"
CORE_BUNDLE_KIND = "core-bundle"
CORE_BUNDLE_SCHEMA = "fastdb.core-bundle.v1"
BUNDLE_MANIFEST_MEMBER = "manifest.json"
EXPECTED_ABI = "fdb_payload_v1"
EXPECTED_LINK_MODE = "system"
DOWNLOAD_TIMEOUT_SECONDS = 120

LINK_MODE_ENV = "FASTDB_PAYLOAD_LINK_MODE"
SYSTEM_LIB_DIR_ENV = "FASTDB_PAYLOAD_SYSTEM_LIB_DIR"

LIBRARY_BY_TARGET = {
    "x86_64-unknown-linux-gnu": "libfastdb.so",
    "aarch64-apple-darwin": "libfastdb.dylib",
    "x86_64-pc-windows-msvc": "fastdb.dll",
}
WINDOWS_RUNTIME_DLL_MEMBER = "lib/fastdb.dll"
WINDOWS_IMPORT_LIBRARY_MEMBER = "lib/fastdb.lib"
LOADER_VARIABLE_BY_TARGET = {
    "x86_64-unknown-linux-gnu": "LD_LIBRARY_PATH",
    "aarch64-apple-darwin": "DYLD_LIBRARY_PATH",
}


class PrepareError(RuntimeError):
    """A verification or preparation failure that must fail the CI step."""


def detect_host_target() -> str:
    system = platform.system().lower()
    machine = platform.machine().lower()
    if system == "linux" and machine in {"amd64", "x86_64"}:
        return "x86_64-unknown-linux-gnu"
    if system == "darwin" and machine in {"aarch64", "arm64"}:
        return "aarch64-apple-darwin"
    if system in {"cygwin", "msys", "windows"} and machine in {"amd64", "x86_64"}:
        return "x86_64-pc-windows-msvc"
    raise PrepareError(
        f"no published FastDB CoreSDK matches this host ({system}/{machine}); "
        f"supported targets are: {', '.join(sorted(LIBRARY_BY_TARGET))}"
    )


def release_url(asset_name: str) -> str:
    return (
        f"https://github.com/{RELEASE_REPOSITORY}/releases/download/"
        f"v{RELEASE_VERSION}/{asset_name}"
    )


def fetch_release_manifest(from_dir: Path | None) -> bytes:
    if from_dir is not None:
        manifest_path = from_dir / RELEASE_MANIFEST_NAME
        if not manifest_path.is_file():
            raise PrepareError(f"release manifest is missing: {manifest_path}")
        return manifest_path.read_bytes()
    with urllib.request.urlopen(
        release_url(RELEASE_MANIFEST_NAME), timeout=DOWNLOAD_TIMEOUT_SECONDS
    ) as response:
        return response.read()


def fetch_core_bundle(entry: dict, from_dir: Path | None, output: Path) -> Path:
    name = entry["name"]
    if from_dir is not None:
        bundle_path = from_dir / name
        if not bundle_path.is_file():
            raise PrepareError(f"core bundle is missing: {bundle_path}")
        verify_core_bundle_file(bundle_path, entry)
        return bundle_path
    bundle_path = output.parent / f".{name}.download"
    bundle_path.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(
        release_url(name), timeout=DOWNLOAD_TIMEOUT_SECONDS
    ) as response, bundle_path.open("wb") as target:
        copy_stream(response, target)
    try:
        verify_core_bundle_file(bundle_path, entry)
    except PrepareError:
        bundle_path.unlink(missing_ok=True)
        raise
    return bundle_path


def copy_stream(source: io.IOBase, target: io.IOBase) -> None:
    while True:
        chunk = source.read(1 << 20)
        if not chunk:
            break
        target.write(chunk)


def verify_core_bundle_file(path: Path, entry: dict) -> None:
    actual_bytes = path.stat().st_size
    if actual_bytes != entry["bytes"]:
        raise PrepareError(
            f"core bundle {path.name} is {actual_bytes} bytes, "
            f"expected {entry['bytes']}"
        )
    digest = sha256_file(path)
    if digest != entry["sha256"]:
        raise PrepareError(
            f"core bundle {path.name} has sha256 {digest}, "
            f"the release manifest pins {entry['sha256']}"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1 << 20)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def verify_release_manifest(data: bytes) -> dict:
    digest = hashlib.sha256(data).hexdigest()
    if digest != RELEASE_MANIFEST_SHA256:
        raise PrepareError(
            f"release manifest sha256 is {digest}, the public pin is "
            f"{RELEASE_MANIFEST_SHA256}"
        )
    try:
        manifest = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PrepareError(f"release manifest is not valid JSON: {error}") from error
    if manifest.get("schema") != RELEASE_MANIFEST_SCHEMA:
        raise PrepareError(
            f"release manifest schema is {manifest.get('schema')!r}, "
            f"expected {RELEASE_MANIFEST_SCHEMA!r}"
        )
    if manifest.get("version") != RELEASE_VERSION:
        raise PrepareError(
            f"release manifest version is {manifest.get('version')!r}, "
            f"expected {RELEASE_VERSION!r}"
        )
    if manifest.get("source_sha") != EXPECTED_SOURCE_SHA:
        raise PrepareError(
            f"release manifest source_sha is {manifest.get('source_sha')!r}, "
            f"expected the official source {EXPECTED_SOURCE_SHA!r}"
        )
    return manifest


def core_bundle_entry(manifest: dict, target: str) -> dict:
    expected_name = f"fastdb-core-{RELEASE_VERSION}-{target}.tar.gz"
    entries = [
        entry
        for entry in manifest.get("artifacts", [])
        if entry.get("kind") == CORE_BUNDLE_KIND
    ]
    for entry in entries:
        if entry.get("target") == target:
            if entry.get("name") != expected_name:
                raise PrepareError(
                    f"target {target} is served by {entry.get('name')!r}, "
                    f"expected {expected_name!r}"
                )
            for field in ("bytes", "sha256"):
                if not isinstance(entry.get(field), (int, str)):
                    raise PrepareError(
                        f"core bundle entry for {target} lacks {field}"
                    )
            return entry
    raise PrepareError(
        f"release v{RELEASE_VERSION} publishes no core bundle for target "
        f"{target}; published targets are: "
        f"{', '.join(sorted(entry['target'] for entry in entries))}"
    )


def validate_member_name(name: str) -> None:
    if name.startswith(("/", "\\")):
        raise PrepareError(f"archive member has an absolute path: {name!r}")
    if re.match(r"^[A-Za-z]:", name):
        raise PrepareError(f"archive member has a drive-letter path: {name!r}")
    if "\\" in name:
        raise PrepareError(f"archive member uses backslashes: {name!r}")
    parts = PurePosixPath(name).parts
    if not parts or any(part in {"", ".."} for part in parts):
        raise PrepareError(f"archive member has an unsafe path: {name!r}")


def collect_regular_members(archive: tarfile.TarFile) -> dict[str, tarfile.TarInfo]:
    members: dict[str, tarfile.TarInfo] = {}
    for info in archive:
        validate_member_name(info.name)
        if info.issym() or info.islnk():
            raise PrepareError(
                f"archive member {info.name!r} is a link; the official bundle "
                "must contain only plain files"
            )
        if info.isdir():
            continue
        if not info.isfile():
            raise PrepareError(
                f"archive member {info.name!r} is not a regular file or directory"
            )
        if info.name in members:
            raise PrepareError(f"archive contains a duplicate member: {info.name!r}")
        members[info.name] = info
    if not members:
        raise PrepareError("archive contains no files")
    return members


def read_bundle_manifest(archive: tarfile.TarFile, members: dict[str, tarfile.TarInfo]) -> dict:
    info = members.get(BUNDLE_MANIFEST_MEMBER)
    if info is None:
        raise PrepareError(
            f"archive is missing its internal {BUNDLE_MANIFEST_MEMBER}"
        )
    extracted = archive.extractfile(info)
    if extracted is None:
        raise PrepareError(f"archive member {BUNDLE_MANIFEST_MEMBER} is unreadable")
    try:
        bundle = json.loads(extracted.read())
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise PrepareError(
            f"internal bundle manifest is not valid JSON: {error}"
        ) from error
    if not isinstance(bundle, dict):
        raise PrepareError("internal bundle manifest is not a JSON object")
    return bundle


def verify_bundle_manifest(bundle: dict, target: str) -> list[dict]:
    expectations = (
        ("schema", CORE_BUNDLE_SCHEMA),
        ("version", RELEASE_VERSION),
        ("target", target),
        ("abi", EXPECTED_ABI),
        ("abi_symbol_count", 117),
        ("link_mode", EXPECTED_LINK_MODE),
        ("source_sha", EXPECTED_SOURCE_SHA),
    )
    for field, expected in expectations:
        actual = bundle.get(field)
        if actual != expected:
            raise PrepareError(
                f"internal bundle manifest {field} is {actual!r}, "
                f"expected {expected!r}"
            )
    files = bundle.get("files")
    if not isinstance(files, list) or not files:
        raise PrepareError("internal bundle manifest lists no member files")
    for entry in files:
        if not isinstance(entry, dict):
            raise PrepareError("internal bundle manifest file entry is not an object")
        if set(entry) != {"path", "bytes", "sha256"}:
            raise PrepareError(
                f"internal bundle manifest entry has unexpected fields: {sorted(entry)}"
            )
        validate_member_name(str(entry["path"]))
        if not isinstance(entry["bytes"], int) or not isinstance(entry["sha256"], str):
            raise PrepareError(
                f"internal bundle manifest entry {entry.get('path')!r} has "
                "invalid bytes/sha256 fields"
            )
    declared = {entry["path"] for entry in files}
    library_member = f"lib/{LIBRARY_BY_TARGET[target]}"
    if library_member not in declared:
        raise PrepareError(
            f"internal bundle manifest is missing the shared library member "
            f"{library_member!r} for target {target}"
        )
    if target == "x86_64-pc-windows-msvc":
        for field, expected_member in (
            ("windows_runtime_dll", WINDOWS_RUNTIME_DLL_MEMBER),
            ("windows_import_library", WINDOWS_IMPORT_LIBRARY_MEMBER),
        ):
            actual = bundle.get(field)
            if actual != expected_member:
                raise PrepareError(
                    f"internal bundle manifest {field} is {actual!r}, expected "
                    f"{expected_member!r}; the Windows SDK must ship both the "
                    "runtime DLL and the import library"
                )
            if actual not in declared:
                raise PrepareError(
                    f"internal bundle manifest declares {field} {actual!r} but "
                    "lists no verified member for it"
                )
    return files


def extract_verified_bundle(
    archive: tarfile.TarFile,
    members: dict[str, tarfile.TarInfo],
    declared_files: list[dict],
    target: str,
    output: Path,
) -> Path:
    declared = {entry["path"]: entry for entry in declared_files}
    undeclared = sorted(set(members) - set(declared) - {BUNDLE_MANIFEST_MEMBER})
    if undeclared:
        raise PrepareError(
            f"archive contains members the internal manifest does not declare: "
            f"{undeclared}"
        )
    unextracted = sorted(set(declared) - set(members))
    if unextracted:
        raise PrepareError(
            f"internal manifest declares members the archive does not contain: "
            f"{unextracted}"
        )
    if output.exists() and any(output.iterdir()):
        raise PrepareError(f"output directory is not empty: {output}")
    output.mkdir(parents=True, exist_ok=True)
    for name in sorted(members):
        info = members[name]
        extracted = archive.extractfile(info)
        if extracted is None:
            raise PrepareError(f"archive member {name!r} is unreadable")
        data = extracted.read()
        entry = declared.get(name)
        if entry is not None:
            if len(data) != entry["bytes"]:
                raise PrepareError(
                    f"archive member {name!r} is {len(data)} bytes, the internal "
                    f"manifest pins {entry['bytes']}"
                )
            digest = hashlib.sha256(data).hexdigest()
            if digest != entry["sha256"]:
                raise PrepareError(
                    f"archive member {name!r} has sha256 {digest}, the internal "
                    f"manifest pins {entry['sha256']}"
                )
        destination = output.joinpath(*PurePosixPath(name).parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
        destination.chmod(0o755 if info.mode & 0o111 else 0o644)
    lib_dir = (output / "lib").resolve()
    library = lib_dir / LIBRARY_BY_TARGET[target]
    if not library.is_file():
        raise PrepareError(
            f"extracted SDK is missing its shared library: {library}"
        )
    return lib_dir


def append_lines(path: Path, lines: list[str]) -> None:
    with path.open("a", encoding="utf-8") as handle:
        for line in lines:
            handle.write(line + "\n")


def emit_github_setup(
    target: str,
    lib_dir: Path,
    *,
    env_file: Path | None,
    path_file: Path | None,
    output_file: Path | None,
) -> dict[str, str]:
    if env_file is None:
        raise PrepareError(
            "--emit-github requires --github-env-file or the GITHUB_ENV file"
        )
    environment_lines = [
        f"{LINK_MODE_ENV}=system",
        f"{SYSTEM_LIB_DIR_ENV}={lib_dir}",
    ]
    loader_variable = LOADER_VARIABLE_BY_TARGET.get(target)
    if loader_variable is not None:
        existing = os.environ.get(loader_variable)
        value = f"{lib_dir}{os.pathsep}{existing}" if existing else str(lib_dir)
        environment_lines.append(f"{loader_variable}={value}")
    append_lines(env_file, environment_lines)
    if target == "x86_64-pc-windows-msvc":
        if path_file is None:
            raise PrepareError(
                "--emit-github for Windows requires --github-path-file or the "
                "GITHUB_PATH file"
            )
        append_lines(path_file, [str(lib_dir)])
    outputs = {
        "lib-dir": str(lib_dir),
        "target": target,
        "version": RELEASE_VERSION,
        "source-sha": EXPECTED_SOURCE_SHA,
    }
    if output_file is not None:
        append_lines(
            output_file, [f"{name}={value}" for name, value in outputs.items()]
        )
    return outputs


def prepare(
    *,
    target: str,
    output: Path,
    from_dir: Path | None,
    emit_github: bool,
    github_env_file: Path | None,
    github_path_file: Path | None,
    github_output_file: Path | None,
) -> dict:
    if target not in LIBRARY_BY_TARGET:
        raise PrepareError(
            f"target {target!r} has no published FastDB CoreSDK; supported "
            f"targets are: {', '.join(sorted(LIBRARY_BY_TARGET))}"
        )
    manifest = verify_release_manifest(fetch_release_manifest(from_dir))
    entry = core_bundle_entry(manifest, target)
    bundle_path = fetch_core_bundle(entry, from_dir, output)
    with tarfile.open(bundle_path, "r:gz") as archive:
        members = collect_regular_members(archive)
        bundle_manifest = read_bundle_manifest(archive, members)
        declared_files = verify_bundle_manifest(bundle_manifest, target)
        lib_dir = extract_verified_bundle(
            archive, members, declared_files, target, output
        )
    summary = {
        "target": target,
        "version": RELEASE_VERSION,
        "source_sha": EXPECTED_SOURCE_SHA,
        "bundle": entry["name"],
        "bundle_sha256": entry["sha256"],
        "lib_dir": str(lib_dir),
        "link_mode": "system",
    }
    if emit_github:
        outputs = emit_github_setup(
            target,
            lib_dir,
            env_file=github_env_file,
            path_file=github_path_file,
            output_file=github_output_file,
        )
        summary["github_outputs"] = outputs
    elif github_output_file is not None:
        # A caller may need step outputs without changing the job-wide link
        # mode (for example while building a statically linked Python app).
        outputs = {
            "lib-dir": str(lib_dir),
            "target": target,
            "version": RELEASE_VERSION,
            "source-sha": EXPECTED_SOURCE_SHA,
        }
        append_lines(github_output_file, [f"{key}={value}" for key, value in outputs.items()])
        summary["github_outputs"] = outputs
    print("FASTDB_SDK_PREPARED " + json.dumps(summary, sort_keys=True))
    return summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        choices=sorted(LIBRARY_BY_TARGET),
        default=None,
        help="CoreSDK target triple; defaults to the published target for this host",
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="explicit (empty or nonexistent) directory to extract the SDK into",
    )
    parser.add_argument(
        "--from-dir",
        type=Path,
        default=None,
        help="read the release manifest and bundle from this directory instead "
        "of downloading them; verification is unchanged",
    )
    parser.add_argument(
        "--emit-github",
        action="store_true",
        help="append system-mode environment, loader path, and step outputs to "
        "the GitHub environment files",
    )
    parser.add_argument(
        "--github-env-file",
        type=Path,
        default=None,
        help="GITHUB_ENV file to append environment lines to "
        "(defaults to the GITHUB_ENV variable)",
    )
    parser.add_argument(
        "--github-path-file",
        type=Path,
        default=None,
        help="GITHUB_PATH file to append loader path lines to "
        "(defaults to the GITHUB_PATH variable)",
    )
    parser.add_argument(
        "--github-output-file",
        type=Path,
        default=None,
        help="step output file to append lib-dir/target/version to "
        "(defaults to the GITHUB_OUTPUT variable)",
    )
    options = parser.parse_args(argv)
    github_env_file = options.github_env_file or (
        Path(os.environ["GITHUB_ENV"])
        if "GITHUB_ENV" in os.environ
        else None
    )
    github_path_file = options.github_path_file or (
        Path(os.environ["GITHUB_PATH"])
        if "GITHUB_PATH" in os.environ
        else None
    )
    github_output_file = options.github_output_file or (
        Path(os.environ["GITHUB_OUTPUT"])
        if "GITHUB_OUTPUT" in os.environ
        else None
    )
    try:
        target = options.target or detect_host_target()
        prepare(
            target=target,
            output=options.output,
            from_dir=options.from_dir,
            emit_github=options.emit_github,
            github_env_file=github_env_file,
            github_path_file=github_path_file,
            github_output_file=github_output_file,
        )
    except PrepareError as error:
        print(f"prepare_fastdb_sdk.py: {error}", file=sys.stderr)
        return 1
    except (OSError, tarfile.TarError) as error:
        print(f"prepare_fastdb_sdk.py: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
