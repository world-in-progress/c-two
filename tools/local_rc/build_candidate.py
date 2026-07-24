"""Build the complete FastDB/C-Two Phase 0B local release candidate."""

from __future__ import annotations

import argparse
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
from typing import Any, Callable, Iterable, Mapping, Sequence

from tools.local_rc.artifact_manifest import (
    MANIFEST_FILENAME,
    CandidateError,
    artifact_descriptor,
    canonical_json_bytes,
    construct_manifest,
    sha256_file,
    verify_manifest,
    write_manifest,
)
from tools.local_rc.local_registry import (
    C2_VERSION,
    FASTDB_VERSION,
    add_crate_archive,
    assert_no_forbidden_source_references,
    build_local_registry,
    cargo_local_registry_version,
    inspect_normalized_manifest,
    render_cargo_config,
    rewrite_path_dependencies_as_versions,
    scan_source_path_dependencies,
)


ROOT = Path(__file__).resolve().parents[2]
FASTDB_ROOT = ROOT.parent / "fastdb"
FASTDB_MANIFEST_FILENAME = "fastdb-local-candidate-manifest.v1.json"
FASTDB_MANIFEST_SCHEMA = "fastdb.local-candidate-manifest.v1"
C_TWO_PYTHON_VERSION = "0.5.1"
C_TWO_NODE_VERSION = "0.1.0"
TYPESCRIPT_VERSION = "5.9.3"
NUMPY_VERSIONS = {
    "cp310": "2.2.6",
    "current": "2.5.1",
}
EXPECTED_FASTDB_COMMIT = "7eb74734926bd8fe911229eee9744a6dd8172487"
RUST_PACKAGE_ORDER = (
    ("c2-config", "core/foundation/c2-config/Cargo.toml"),
    ("c2-contract", "core/foundation/c2-contract/Cargo.toml"),
    ("c2-error", "core/foundation/c2-error/Cargo.toml"),
    ("c2-mem", "core/foundation/c2-mem/Cargo.toml"),
    ("c2-codegen", "core/foundation/c2-codegen/Cargo.toml"),
    ("c2-mem-ffi", "core/foundation/c2-mem-ffi/Cargo.toml"),
    ("c2-wire", "core/protocol/c2-wire/Cargo.toml"),
    ("c2-server", "core/transport/c2-server/Cargo.toml"),
    ("c2-ipc", "core/transport/c2-ipc/Cargo.toml"),
    ("c2-http", "core/transport/c2-http/Cargo.toml"),
    ("c2-core", "core/runtime/c2-core/Cargo.toml"),
    ("c2-python-native", "sdk/python/native/Cargo.toml"),
    ("c2-cli", "cli/Cargo.toml"),
    ("c-two", "sdk/rust/Cargo.toml"),
)
CARGO_LOCK_MANIFESTS = (
    "core/Cargo.toml",
    "cli/Cargo.toml",
    "sdk/python/native/Cargo.toml",
    "sdk/rust/Cargo.toml",
)
RUST_PACKAGE_VERSIONS = {
    **{name: C2_VERSION for name, _ in RUST_PACKAGE_ORDER},
    "c2-cli": "0.1.4",
}
CONTRACT_FIXTURES = {
    "no-payload": "portable-no-payload.contract.json",
    "record-v1": "portable-record-v1.contract.json",
    "object-graph-v1": "portable-object-graph-v1.contract.json",
}
FASTDB_SPEC_SOURCES = {
    "record-all-types.source.json": (
        "tests/golden/payload/v1/spec/valid/record-all-types.source.json"
    ),
    "graph-all-values.source.json": (
        "tests/golden/payload/v1/binary/spec/graph-all-values.source.json"
    ),
}
EXPECTED_NODE_RUNTIME_MEMBERS = {
    "README.md",
    "package.json",
    "dist/index.js",
    "dist/index.d.ts",
    "dist/native/c2_mem_ffi_node.node",
}


class RunLog:
    """Non-retained command log; absolute execution paths never enter evidence."""

    def __init__(self, path: Path) -> None:
        self.path = path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("", encoding="utf-8")

    def append(self, command: Sequence[str], output: str) -> None:
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write("$ " + " ".join(command) + "\n")
            stream.write(output)
            if output and not output.endswith("\n"):
                stream.write("\n")


def require_absent_output(output: Path) -> None:
    if output.exists() or output.is_symlink():
        raise CandidateError(
            f"candidate output must be absent before construction: {output}"
        )


def run(
    command: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str] | None = None,
    log: RunLog | None = None,
    timeout: float = 1800,
) -> str:
    completed = subprocess.run(
        list(command),
        cwd=cwd,
        env=None if env is None else dict(env),
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="strict",
        timeout=timeout,
    )
    if log is not None:
        log.append(command, completed.stdout)
    if completed.returncode != 0:
        raise CandidateError(
            f"command failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{completed.stdout}"
        )
    return completed.stdout


def git(repository: Path, *arguments: str) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=repository,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="strict",
    )
    if completed.returncode != 0:
        raise CandidateError(
            f"git {' '.join(arguments)} failed in {repository}: "
            f"{completed.stdout}"
        )
    return completed.stdout


def source_record(repository: Path, name: str) -> dict[str, Any]:
    commit = git(repository, "rev-parse", "HEAD").strip()
    if re.fullmatch(r"[0-9a-f]{40}", commit) is None:
        raise CandidateError(f"{name} HEAD is not a full commit")
    status_bytes = subprocess.run(
        ["git", "status", "--porcelain=v1", "-z"],
        cwd=repository,
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    if status_bytes:
        raise CandidateError(
            f"{name} worktree must be clean before building the candidate"
        )
    epoch_text = git(repository, "show", "-s", "--format=%ct", commit).strip()
    try:
        epoch = int(epoch_text)
    except ValueError as error:
        raise CandidateError(
            f"{name} commit timestamp is invalid: {epoch_text!r}"
        ) from error
    import hashlib

    return {
        "repository": name,
        "commit": commit,
        "source_date_epoch": epoch,
        "worktree_clean": True,
        "worktree_state_sha256": hashlib.sha256(status_bytes).hexdigest(),
    }


def _safe_extract_git_archive(contents: bytes, destination: Path) -> None:
    destination.mkdir(parents=True)
    try:
        with tarfile.open(fileobj=io.BytesIO(contents), mode="r:") as archive:
            for member in archive.getmembers():
                path = Path(member.name)
                if (
                    path.is_absolute()
                    or ".." in path.parts
                    or member.issym()
                    or member.islnk()
                    or not (member.isfile() or member.isdir())
                ):
                    raise CandidateError(
                        f"git archive contains unsafe member {member.name!r}"
                    )
            archive.extractall(destination, filter="data")
    except tarfile.TarError as error:
        raise CandidateError(f"could not extract Git archive: {error}") from error


def copy_git_snapshot(
    repository: Path,
    commit: str,
    destination: Path,
) -> None:
    completed = subprocess.run(
        ["git", "archive", "--format=tar", commit],
        cwd=repository,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if completed.returncode != 0:
        raise CandidateError(
            f"git archive failed in {repository}: "
            f"{completed.stderr.decode(errors='replace')}"
        )
    _safe_extract_git_archive(completed.stdout, destination)


def prepare_package_snapshot(
    source: Path,
) -> None:
    findings = scan_source_path_dependencies(source)
    if findings:
        raise CandidateError(
            "committed source contains unversioned first-party paths:\n"
            + "\n".join(findings)
        )
    for manifest in sorted(source.rglob("Cargo.toml")):
        manifest.write_text(
            rewrite_path_dependencies_as_versions(
                manifest.read_text(encoding="utf-8")
            ),
            encoding="utf-8",
        )
    remaining = scan_source_path_dependencies(source)
    if remaining:
        raise CandidateError(
            "packaging snapshot retained a first-party dependency path:\n"
            + "\n".join(remaining)
        )


def prepare_packaging_snapshot(
    *,
    source_root: Path,
    registry: Path,
    bootstrap_registry: Path,
    bootstrap_cargo_home: Path,
    c2_commit: str,
    fastdb_commit: str,
    fastdb_archives: Iterable[Path],
    environment: Mapping[str, str],
    bootstrap_target: Path,
    log: RunLog,
) -> Path:
    """Build the external closure before removing source-only path edges.

    ``cargo local-registry sync`` must load the committed lockfiles together
    with their path-based workspace packages. Both repositories are therefore
    reconstructed from their approved commits. Only after the external
    registry closure exists do we rewrite the C-Two packaging snapshot to
    version-only dependencies.
    """

    snapshot = source_root / "c-two"
    fastdb_snapshot = source_root / "fastdb"
    copy_git_snapshot(ROOT, c2_commit, snapshot)
    copy_git_snapshot(FASTDB_ROOT, fastdb_commit, fastdb_snapshot)
    build_local_registry(
        closure_locks=(
            snapshot / "core/Cargo.lock",
            snapshot / "cli/Cargo.lock",
            snapshot / "sdk/python/native/Cargo.lock",
            snapshot / "sdk/rust/Cargo.lock",
        ),
        registry_dir=registry,
        crate_archives=fastdb_archives,
    )
    build_bootstrap_registry(
        snapshot=snapshot,
        base_registry=registry,
        bootstrap_registry=bootstrap_registry,
        cargo_home=bootstrap_cargo_home,
        environment=environment,
        package_target=bootstrap_target,
        log=log,
    )
    prepare_package_snapshot(snapshot)
    return snapshot


def _load_json_no_duplicates(path: Path) -> dict[str, Any]:
    def object_from_pairs(
        pairs: list[tuple[str, Any]],
    ) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise CandidateError(
                    f"{path} contains duplicate JSON key {key!r}"
                )
            result[key] = value
        return result

    try:
        value = json.loads(
            path.read_text(encoding="utf-8"),
            object_pairs_hook=object_from_pairs,
        )
    except (OSError, json.JSONDecodeError) as error:
        raise CandidateError(f"could not read {path}: {error}") from error
    if not isinstance(value, dict):
        raise CandidateError(f"{path} must contain a JSON object")
    return value


def validate_fastdb_candidate(
    candidate: Path,
    *,
    log: RunLog,
) -> tuple[dict[str, Any], str]:
    if not candidate.is_dir():
        raise CandidateError(f"FastDB candidate does not exist: {candidate}")
    run(
        [
            sys.executable,
            str(FASTDB_ROOT / "tests/ci/build_local_payload_candidate.py"),
            "--verify",
            str(candidate),
        ],
        cwd=FASTDB_ROOT,
        log=log,
    )
    manifest_path = candidate / FASTDB_MANIFEST_FILENAME
    manifest = _load_json_no_duplicates(manifest_path)
    if manifest.get("schema") != FASTDB_MANIFEST_SCHEMA:
        raise CandidateError("FastDB candidate manifest schema is not recognized")
    source = manifest.get("source")
    if not isinstance(source, dict):
        raise CandidateError("FastDB candidate has no source record")
    commit = source.get("commit")
    if commit != EXPECTED_FASTDB_COMMIT:
        raise CandidateError(
            "FastDB candidate is not the approved Task 10 source commit"
        )
    if git(FASTDB_ROOT, "rev-parse", "HEAD").strip() != commit:
        raise CandidateError(
            "FastDB repository HEAD does not match its candidate manifest"
        )
    if git(FASTDB_ROOT, "status", "--porcelain").strip():
        raise CandidateError("FastDB repository must remain clean")
    return manifest, sha256_file(manifest_path)


def current_platform() -> str:
    machine = platform.machine().lower()
    if sys.platform == "darwin":
        machine = "arm64" if machine in {"arm64", "aarch64"} else machine
        return f"darwin-{machine}"
    if sys.platform.startswith("linux"):
        machine = "aarch64" if machine in {"arm64", "aarch64"} else machine
        return f"linux-{machine}"
    return f"{sys.platform}-{machine}"


def rust_target(log: RunLog) -> str:
    output = run(["rustc", "-vV"], cwd=ROOT, log=log)
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise CandidateError("rustc -vV did not report a host target")


def tool_version(
    command: Sequence[str],
    *,
    cwd: Path,
    log: RunLog,
) -> str:
    return run(command, cwd=cwd, log=log).splitlines()[0].strip()


def pip_version(
    interpreter: Path,
    *,
    log: RunLog,
) -> str:
    output = tool_version(
        [str(interpreter), "-m", "pip", "--version"],
        cwd=ROOT,
        log=log,
    )
    fields = output.split()
    if len(fields) < 2 or fields[0] != "pip":
        raise CandidateError(f"pip --version output is invalid: {output!r}")
    return " ".join(fields[:2])


def build_evidence(
    *,
    commands: Sequence[str],
    target: str,
    toolchains: Mapping[str, str],
) -> dict[str, Any]:
    platform_name = current_platform()
    known = {
        "darwin-arm64",
        "linux-aarch64",
        "linux-x86_64",
        "win32-amd64",
    }
    unsupported = {"win32-amd64"}
    return {
        "commands": list(commands),
        "platform": platform_name,
        "target": target,
        "toolchains": dict(sorted(toolchains.items())),
        "verified_platforms": [platform_name],
        "unverified_platforms": sorted(
            known - {platform_name} - unsupported
        ),
        "unsupported_platforms": sorted(unsupported - {platform_name}),
    }


def _base_environment(
    *,
    source_date_epoch: int,
    cargo_home: Path,
    fastdb_candidate: Path,
) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "SOURCE_DATE_EPOCH": str(source_date_epoch),
            "TZ": "UTC",
            "CARGO_HOME": str(cargo_home),
            "FASTDB_PAYLOAD_LINK_MODE": "system",
            "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": str(
                fastdb_candidate / "core/lib"
            ),
            "C2_ENV_FILE": "",
        }
    )
    library_dir = str(fastdb_candidate / "core/lib")
    if sys.platform == "darwin":
        environment["DYLD_LIBRARY_PATH"] = library_dir
    elif os.name == "posix":
        environment["LD_LIBRARY_PATH"] = library_dir
    elif os.name == "nt":
        environment["PATH"] = (
            library_dir + os.pathsep + environment.get("PATH", "")
        )
    return environment


def _crate_archive_path(
    package_target: Path,
    name: str,
    version: str,
) -> Path:
    archive = package_target / "package" / f"{name}-{version}.crate"
    if not archive.is_file():
        raise CandidateError(f"cargo package did not produce {archive}")
    return archive


def package_rust_closure(
    *,
    snapshot: Path,
    environment: Mapping[str, str],
    package_target: Path,
    log: RunLog,
    on_archive: Callable[[Path], None] | None = None,
) -> list[Path]:
    archives: list[Path] = []
    for name, relative_manifest in RUST_PACKAGE_ORDER:
        version = RUST_PACKAGE_VERSIONS[name]
        manifest = snapshot / relative_manifest
        run(
            [
                "cargo",
                "package",
                "--manifest-path",
                str(manifest),
                "--allow-dirty",
                "--no-verify",
                "--offline",
                "--target-dir",
                str(package_target),
            ],
            cwd=snapshot,
            env=environment,
            log=log,
        )
        built = _crate_archive_path(package_target, name, version)
        inspect_normalized_manifest(built)
        archives.append(built)
        if on_archive is not None:
            on_archive(built)
    return archives


def build_bootstrap_registry(
    *,
    snapshot: Path,
    base_registry: Path,
    bootstrap_registry: Path,
    cargo_home: Path,
    environment: Mapping[str, str],
    package_target: Path,
    log: RunLog,
) -> None:
    """Seed a temporary registry from path-connected committed snapshots."""

    if bootstrap_registry.exists():
        raise CandidateError(
            f"bootstrap registry must be absent: {bootstrap_registry}"
        )
    shutil.copytree(base_registry, bootstrap_registry)
    cargo_home.mkdir()
    (cargo_home / "config.toml").write_text(
        render_cargo_config(bootstrap_registry),
        encoding="utf-8",
    )
    bootstrap_environment = dict(environment)
    bootstrap_environment["CARGO_HOME"] = str(cargo_home)
    package_rust_closure(
        snapshot=snapshot,
        environment=bootstrap_environment,
        package_target=package_target,
        log=log,
        on_archive=lambda archive: add_crate_archive(
            bootstrap_registry,
            archive,
        ),
    )


def build_rust_packages(
    *,
    snapshot: Path,
    output: Path,
    registry: Path,
    environment: Mapping[str, str],
    package_target: Path,
    log: RunLog,
) -> list[Path]:
    destination = output / "rust"
    destination.mkdir()
    archives: list[Path] = []
    for built in package_rust_closure(
        snapshot=snapshot,
        environment=environment,
        package_target=package_target,
        log=log,
    ):
        packaged = destination / built.name
        shutil.copyfile(built, packaged)
        inspect_normalized_manifest(packaged)
        assert_no_forbidden_source_references(
            [packaged],
            forbidden_roots=(ROOT, FASTDB_ROOT, ROOT.parent / "toodle"),
        )
        add_crate_archive(registry, packaged)
        archives.append(packaged)
    return archives


def regenerate_package_snapshot_locks(
    *,
    snapshot: Path,
    environment: Mapping[str, str],
    log: RunLog,
) -> None:
    """Lock the disposable version-only snapshot to final archive checksums."""

    for relative_manifest in CARGO_LOCK_MANIFESTS:
        manifest = snapshot / relative_manifest
        lockfile = manifest.parent / "Cargo.lock"
        if lockfile.exists():
            lockfile.unlink()
        run(
            [
                "cargo",
                "generate-lockfile",
                "--offline",
                "--manifest-path",
                str(manifest),
            ],
            cwd=snapshot,
            env=environment,
            log=log,
        )


def build_c3(
    *,
    snapshot: Path,
    output: Path,
    environment: Mapping[str, str],
    target_dir: Path,
    log: RunLog,
) -> Path:
    build_environment = dict(environment)
    build_environment["CARGO_TARGET_DIR"] = str(target_dir)
    run(
        [
            "cargo",
            "build",
            "--release",
            "--offline",
            "--manifest-path",
            str(snapshot / "cli/Cargo.toml"),
            "--bin",
            "c3",
        ],
        cwd=output.parent,
        env=build_environment,
        log=log,
    )
    name = "c3.exe" if os.name == "nt" else "c3"
    built = target_dir / "release" / name
    if not built.is_file():
        raise CandidateError(f"cargo did not build {built}")
    destination = output / "bin" / name
    destination.parent.mkdir()
    shutil.copyfile(built, destination)
    destination.chmod(0o755)
    assert_no_forbidden_source_references(
        [destination],
        forbidden_roots=(ROOT, FASTDB_ROOT, ROOT.parent / "toodle"),
    )
    return destination


def copy_candidate_fixtures(
    *,
    snapshot: Path,
    output: Path,
) -> None:
    contract_root = output / "fixtures/contracts"
    fastdb_root = output / "fixtures/fastdb"
    harness_root = output / "harness"
    contract_root.mkdir(parents=True)
    fastdb_root.mkdir(parents=True)
    harness_root.mkdir()
    for filename in CONTRACT_FIXTURES.values():
        shutil.copyfile(
            snapshot / "tests/fixtures/contracts" / filename,
            contract_root / filename,
        )
    for filename, source in FASTDB_SPEC_SOURCES.items():
        shutil.copyfile(FASTDB_ROOT / source, fastdb_root / filename)
    for relative in (
        "sdk/python/tests/fixtures/portable_matrix_rust.rs",
        "sdk/python/tests/fixtures/typescript_real_call.mjs",
    ):
        shutil.copyfile(
            snapshot / relative,
            harness_root / Path(relative).name,
        )


def generate_contract_trees(
    *,
    c3: Path,
    output: Path,
    environment: Mapping[str, str],
    log: RunLog,
) -> list[Path]:
    generated: list[Path] = []
    for profile, filename in CONTRACT_FIXTURES.items():
        descriptor = output / "fixtures/contracts" / filename
        for target in ("python", "rust", "typescript"):
            destination = output / "generated" / profile / target
            destination.parent.mkdir(parents=True, exist_ok=True)
            run(
                [
                    str(c3),
                    "contract",
                    "codegen",
                    target,
                    str(descriptor),
                    "--out-dir",
                    str(destination),
                ],
                cwd=output.parent,
                env=environment,
                log=log,
            )
            generated.append(destination)
    return generated


def build_python_packages(
    *,
    snapshot: Path,
    output: Path,
    python_current: Path,
    python_310: Path,
    environment: Mapping[str, str],
    log: RunLog,
) -> list[Path]:
    destination = output / "python"
    current = destination / "current"
    cp310 = destination / "cp310"
    current.mkdir(parents=True)
    cp310.mkdir()
    source_root = snapshot / "sdk/python"
    run(
        [
            "uv",
            "build",
            "--sdist",
            "--python",
            str(python_current),
            "--out-dir",
            str(destination),
            "--no-create-gitignore",
            str(source_root),
        ],
        cwd=output.parent,
        env=environment,
        log=log,
    )
    for interpreter, wheel_root in (
        (python_310, cp310),
        (python_current, current),
    ):
        run(
            [
                "uv",
                "build",
                "--wheel",
                "--python",
                str(interpreter),
                "--out-dir",
                str(wheel_root),
                "--no-create-gitignore",
                str(source_root),
            ],
            cwd=output.parent,
            env=environment,
            log=log,
        )
    sdists = tuple(destination.glob("c_two-*.tar.gz"))
    current_wheels = tuple(current.glob("c_two-*.whl"))
    cp310_wheels = tuple(cp310.glob("c_two-*.whl"))
    if not (
        len(sdists) == len(current_wheels) == len(cp310_wheels) == 1
    ):
        raise CandidateError(
            "C-Two Python build did not produce exactly one sdist and "
            "one wheel for each runtime"
        )
    for path in (*sdists, *current_wheels, *cp310_wheels):
        assert_no_forbidden_source_references(
            [path],
            forbidden_roots=(ROOT, FASTDB_ROOT, ROOT.parent / "toodle"),
        )
    return [*sdists, *cp310_wheels, *current_wheels]


def download_python_runtime_dependencies(
    *,
    output: Path,
    python_current: Path,
    python_310: Path,
    environment: Mapping[str, str],
    log: RunLog,
) -> list[Path]:
    dependencies = output / "python/dependencies"
    wheels: list[Path] = []
    for label, interpreter in (
        ("cp310", python_310),
        ("current", python_current),
    ):
        version = NUMPY_VERSIONS[label]
        destination = dependencies / label
        destination.mkdir(parents=True)
        run(
            [
                str(interpreter),
                "-m",
                "pip",
                "download",
                "--disable-pip-version-check",
                "--only-binary=:all:",
                "--no-deps",
                "--dest",
                str(destination),
                f"numpy=={version}",
            ],
            cwd=output.parent,
            env=environment,
            log=log,
        )
        matches = tuple(destination.glob(f"numpy-{version}-*.whl"))
        all_files = tuple(path for path in destination.iterdir() if path.is_file())
        if len(matches) != 1 or set(matches) != set(all_files):
            raise CandidateError(
                f"Python {label} dependency download must produce exactly "
                f"one numpy {version} wheel"
            )
        wheel = matches[0]
        assert_no_forbidden_source_references(
            [wheel],
            forbidden_roots=(ROOT, FASTDB_ROOT, ROOT.parent / "toodle"),
        )
        wheels.append(wheel)
    return wheels


def _required_npm_pack_file(
    destination: Path,
    filename: str,
    label: str,
) -> Path:
    package = destination / filename
    if not package.is_file():
        raise CandidateError(
            f"{label} npm pack did not produce exact file {filename!r}"
        )
    return package


def build_node_packages(
    *,
    snapshot: Path,
    output: Path,
    environment: Mapping[str, str],
    log: RunLog,
) -> list[Path]:
    package_root = (
        snapshot
        / "core/foundation/c2-mem-ffi/bindings/typescript"
    )
    destination = output / "typescript"
    destination.mkdir()
    run(
        [
            "npm",
            "ci",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ],
        cwd=package_root,
        env=environment,
        log=log,
    )
    run(
        [
            "npm",
            "pack",
            "--json",
            "--pack-destination",
            str(destination),
        ],
        cwd=package_root,
        env=environment,
        log=log,
    )
    c2_package = _required_npm_pack_file(
        destination,
        f"c-two-c2-mem-ffi-{C_TWO_NODE_VERSION}.tgz",
        "@c-two/c2-mem-ffi",
    )
    run(
        [
            "npm",
            "pack",
            f"typescript@{TYPESCRIPT_VERSION}",
            "--json",
            "--pack-destination",
            str(destination),
        ],
        cwd=output.parent,
        env=environment,
        log=log,
    )
    typescript_package = _required_npm_pack_file(
        destination,
        f"typescript-{TYPESCRIPT_VERSION}.tgz",
        "typescript",
    )
    from tools.local_rc.artifact_manifest import archive_inventory

    inventory = {
        item["path"]
        for item in archive_inventory(
            c2_package,
            label="@c-two/c2-mem-ffi",
        )
    }
    required = set(EXPECTED_NODE_RUNTIME_MEMBERS)
    required.add(
        "dist/native/libc2_mem_ffi.dylib"
        if sys.platform == "darwin"
        else "dist/native/libc2_mem_ffi.so"
    )
    if inventory != required:
        raise CandidateError(
            "C-Two npm package inventory is not the exact runtime set: "
            f"missing={sorted(required - inventory)}, "
            f"unexpected={sorted(inventory - required)}"
        )
    assert_no_forbidden_source_references(
        [c2_package, typescript_package],
        forbidden_roots=(ROOT, FASTDB_ROOT, ROOT.parent / "toodle"),
    )
    return [c2_package, typescript_package]


def _adapt_fastdb_build(value: object) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CandidateError("FastDB artifact build evidence is invalid")
    commands = value.get("commands")
    toolchains = value.get("toolchains")
    if not isinstance(commands, list) or not isinstance(toolchains, dict):
        raise CandidateError("FastDB artifact build evidence is incomplete")
    return {
        "commands": commands,
        "platform": value.get("platform"),
        "target": value.get("target"),
        "toolchains": toolchains,
        "verified_platforms": sorted(value.get("verified_platforms", [])),
        "unverified_platforms": sorted(value.get("unverified_platforms", [])),
        "unsupported_platforms": [],
    }


def _aggregate_fastdb_manifest_build(
    manifest: Mapping[str, Any],
) -> dict[str, Any]:
    raw_artifacts = manifest.get("artifacts")
    if not isinstance(raw_artifacts, list) or not raw_artifacts:
        raise CandidateError("FastDB manifest has no artifact build evidence")
    builds: list[dict[str, Any]] = []
    for raw in raw_artifacts:
        if not isinstance(raw, dict):
            raise CandidateError("FastDB artifact record is invalid")
        builds.append(_adapt_fastdb_build(raw.get("build")))

    platforms = {build["platform"] for build in builds}
    if len(platforms) != 1:
        raise CandidateError(
            "FastDB artifact build platforms are inconsistent"
        )
    targets = {build["target"] for build in builds}
    commands = sorted(
        {
            command
            for build in builds
            for command in build["commands"]
        }
    )
    toolchain_observations: dict[str, dict[str, str]] = {}
    for build in builds:
        for name, version in build["toolchains"].items():
            by_target = toolchain_observations.setdefault(name, {})
            target = build["target"]
            existing = by_target.get(target)
            if existing is not None and existing != version:
                raise CandidateError(
                    f"FastDB artifact toolchain {name!r} is inconsistent "
                    f"for target {target!r}"
                )
            by_target[target] = version
    toolchains: dict[str, str] = {}
    for name, by_target in sorted(toolchain_observations.items()):
        versions = set(by_target.values())
        if len(versions) == 1:
            toolchains[name] = next(iter(versions))
        else:
            for target, version in sorted(by_target.items()):
                toolchains[f"{name}@{target}"] = version
    return {
        "commands": commands,
        "platform": next(iter(platforms)),
        "target": next(iter(targets)) if len(targets) == 1 else "multi-target",
        "toolchains": toolchains,
        "verified_platforms": sorted(
            {
                platform_name
                for build in builds
                for platform_name in build["verified_platforms"]
            }
        ),
        "unverified_platforms": sorted(
            {
                platform_name
                for build in builds
                for platform_name in build["unverified_platforms"]
            }
        ),
        "unsupported_platforms": [],
    }


def describe_artifacts(
    *,
    output: Path,
    sources: Mapping[str, dict[str, Any]],
    fastdb_manifest: dict[str, Any],
    c2_rust_archives: Iterable[Path],
    c3: Path,
    python_packages: Iterable[Path],
    python_dependencies: Iterable[Path],
    node_packages: Iterable[Path],
    generated: Iterable[Path],
    rust_build: dict[str, Any],
    python_build: dict[str, Any],
    python_dependency_build: dict[str, Any],
    node_build: dict[str, Any],
    c3_build: dict[str, Any],
) -> list[dict[str, Any]]:
    c2_commit = sources["c-two"]["commit"]
    fastdb_commit = sources["fastdb"]["commit"]
    artifacts: list[dict[str, Any]] = []

    fastdb_manifest_path = f"fastdb/{FASTDB_MANIFEST_FILENAME}"
    artifacts.append(
        artifact_descriptor(
            output,
            path=fastdb_manifest_path,
            kind="fastdb-candidate-manifest",
            owner_repository="fastdb",
            source_commit=fastdb_commit,
            package={"name": "fastdb-candidate", "version": FASTDB_VERSION},
            build=_aggregate_fastdb_manifest_build(fastdb_manifest),
        )
    )
    raw_fastdb_artifacts = fastdb_manifest.get("artifacts")
    if not isinstance(raw_fastdb_artifacts, list):
        raise CandidateError("FastDB manifest artifacts are invalid")
    for raw in raw_fastdb_artifacts:
        if not isinstance(raw, dict):
            raise CandidateError("FastDB artifact record is invalid")
        package = raw.get("package")
        if not isinstance(package, dict):
            raise CandidateError("FastDB artifact package is invalid")
        artifacts.append(
            artifact_descriptor(
                output,
                path=f"fastdb/{raw['path']}",
                kind=f"fastdb-{raw['kind']}",
                owner_repository="fastdb",
                source_commit=fastdb_commit,
                package=package,
                build=_adapt_fastdb_build(raw["build"]),
            )
        )

    for archive in c2_rust_archives:
        matches = [
            (name, version)
            for name, version in RUST_PACKAGE_VERSIONS.items()
            if archive.name == f"{name}-{version}.crate"
        ]
        if len(matches) != 1:
            raise CandidateError(
                f"could not identify C-Two crate archive {archive.name!r}"
            )
        package_name, version = matches[0]
        artifacts.append(
            artifact_descriptor(
                output,
                path=archive.relative_to(output).as_posix(),
                kind="rust-crate",
                owner_repository="c-two",
                source_commit=c2_commit,
                package={"name": package_name, "version": version},
                build=rust_build,
            )
        )
    artifacts.append(
        artifact_descriptor(
            output,
            path=c3.relative_to(output).as_posix(),
            kind="c3-cli-binary",
            owner_repository="c-two",
            source_commit=c2_commit,
            package={"name": "c3", "version": "0.1.4"},
            build=c3_build,
        )
    )
    for package_path in python_packages:
        kind = (
            "python-sdist"
            if package_path.name.endswith(".tar.gz")
            else (
                "python-cp310-wheel"
                if "cp310" in package_path.name
                else "python-current-wheel"
            )
        )
        artifacts.append(
            artifact_descriptor(
                output,
                path=package_path.relative_to(output).as_posix(),
                kind=kind,
                owner_repository="c-two",
                source_commit=c2_commit,
                package={"name": "c-two", "version": C_TWO_PYTHON_VERSION},
                build=python_build,
            )
        )
    for dependency_path in python_dependencies:
        label = dependency_path.parent.name
        version = NUMPY_VERSIONS.get(label)
        if version is None or not dependency_path.name.startswith(
            f"numpy-{version}-"
        ):
            raise CandidateError(
                f"could not identify Python dependency {dependency_path.name!r}"
            )
        artifacts.append(
            artifact_descriptor(
                output,
                path=dependency_path.relative_to(output).as_posix(),
                kind="python-runtime-dependency-wheel",
                owner_repository="third-party",
                source_commit=c2_commit,
                package={"name": "numpy", "version": version},
                build=python_dependency_build,
            )
        )
    for package_path in node_packages:
        is_typescript = package_path.name.startswith("typescript-")
        artifacts.append(
            artifact_descriptor(
                output,
                path=package_path.relative_to(output).as_posix(),
                kind=(
                    "typescript-build-tool-tarball"
                    if is_typescript
                    else "typescript-runtime-tarball"
                ),
                owner_repository="build-tool" if is_typescript else "c-two",
                source_commit=c2_commit,
                package={
                    "name": (
                        "typescript"
                        if is_typescript
                        else "@c-two/c2-mem-ffi"
                    ),
                    "version": (
                        TYPESCRIPT_VERSION
                        if is_typescript
                        else C_TWO_NODE_VERSION
                    ),
                },
                build=node_build,
            )
        )
    for directory in generated:
        artifacts.append(
            artifact_descriptor(
                output,
                path=directory.relative_to(output).as_posix(),
                kind="generated-contract-tree",
                owner_repository="c-two",
                source_commit=c2_commit,
                package={"name": "c3-generated-contract", "version": "v2"},
                build=c3_build,
            )
        )
    for relative, owner, package_name in (
        ("fixtures/contracts", "c-two", "c-two-contract-fixtures"),
        ("fixtures/fastdb", "fastdb", "fastdb-payload-fixtures"),
        ("harness", "c-two", "c-two-candidate-harness"),
    ):
        artifacts.append(
            artifact_descriptor(
                output,
                path=relative,
                kind="candidate-proof-inputs",
                owner_repository=owner,
                source_commit=(
                    c2_commit if owner == "c-two" else fastdb_commit
                ),
                package={"name": package_name, "version": "v1"},
                build=c3_build,
            )
        )
    return artifacts


def build_candidate(
    *,
    output: Path,
    fastdb_candidate: Path,
    python_current: Path,
    python_310: Path,
) -> dict[str, Any]:
    require_absent_output(output)
    if not python_current.is_file() or not python_310.is_file():
        raise CandidateError("both Python interpreter paths must be files")
    run_root = output.parent
    run_root.mkdir(parents=True, exist_ok=True)
    log = RunLog(run_root / "c2-local-candidate-run.log")
    cargo_local_registry_version()
    c2_source = source_record(ROOT, "c-two")
    fastdb_source = source_record(FASTDB_ROOT, "fastdb")
    if fastdb_source["commit"] != EXPECTED_FASTDB_COMMIT:
        raise CandidateError("FastDB HEAD moved after the approved Task 10")
    fastdb_manifest, _ = validate_fastdb_candidate(
        fastdb_candidate,
        log=log,
    )

    source_root = run_root / "source"
    build_root = run_root / "build"
    registry = run_root / "registry"
    cargo_home = run_root / "cargo-home"
    for path in (source_root, build_root, registry, cargo_home):
        if path.exists():
            raise CandidateError(f"run path must be absent: {path}")
    output.mkdir()
    shutil.copytree(fastdb_candidate, output / "fastdb")
    build_root.mkdir()
    cargo_home.mkdir()
    (cargo_home / "config.toml").write_text(
        render_cargo_config(registry),
        encoding="utf-8",
    )
    environment = _base_environment(
        source_date_epoch=c2_source["source_date_epoch"],
        cargo_home=cargo_home,
        fastdb_candidate=output / "fastdb",
    )
    fastdb_archives = sorted((output / "fastdb/rust").glob("*.crate"))
    if len(fastdb_archives) != 2:
        raise CandidateError("FastDB candidate must contain two Rust crates")
    bootstrap_registry = build_root / "bootstrap-registry"
    bootstrap_cargo_home = build_root / "bootstrap-cargo-home"
    snapshot = prepare_packaging_snapshot(
        source_root=source_root,
        registry=registry,
        bootstrap_registry=bootstrap_registry,
        bootstrap_cargo_home=bootstrap_cargo_home,
        c2_commit=c2_source["commit"],
        fastdb_commit=fastdb_source["commit"],
        fastdb_archives=fastdb_archives,
        environment=environment,
        bootstrap_target=build_root / "bootstrap-package",
        log=log,
    )

    package_environment = dict(environment)
    package_environment["CARGO_HOME"] = str(bootstrap_cargo_home)
    c2_rust_archives = build_rust_packages(
        snapshot=snapshot,
        output=output,
        registry=registry,
        environment=package_environment,
        package_target=build_root / "rust-package",
        log=log,
    )
    regenerate_package_snapshot_locks(
        snapshot=snapshot,
        environment=environment,
        log=log,
    )
    c3 = build_c3(
        snapshot=snapshot,
        output=output,
        environment=environment,
        target_dir=build_root / "c3-target",
        log=log,
    )
    copy_candidate_fixtures(snapshot=snapshot, output=output)
    generated = generate_contract_trees(
        c3=c3,
        output=output,
        environment=environment,
        log=log,
    )
    python_packages = build_python_packages(
        snapshot=snapshot,
        output=output,
        python_current=python_current,
        python_310=python_310,
        environment=environment,
        log=log,
    )
    python_dependencies = download_python_runtime_dependencies(
        output=output,
        python_current=python_current,
        python_310=python_310,
        environment=environment,
        log=log,
    )
    node_environment = dict(environment)
    node_environment["CARGO_TARGET_DIR"] = str(
        build_root / "node-cargo-target"
    )
    node_packages = build_node_packages(
        snapshot=snapshot,
        output=output,
        environment=node_environment,
        log=log,
    )

    target = rust_target(log)
    cargo_version = tool_version(["cargo", "--version"], cwd=ROOT, log=log)
    rustc_version = tool_version(["rustc", "--version"], cwd=ROOT, log=log)
    uv_version = tool_version(["uv", "--version"], cwd=ROOT, log=log)
    node_version = tool_version(["node", "--version"], cwd=ROOT, log=log)
    npm_version = tool_version(["npm", "--version"], cwd=ROOT, log=log)
    rust_build = build_evidence(
        commands=(
            "cargo package --offline --manifest-path "
            "$SOURCE_SNAPSHOT/<crate>/Cargo.toml",
            "cargo local-registry sync $LOCK $RUN_ROOT/registry",
            "import locally built .crate checksum/index into "
            "$RUN_ROOT/registry",
        ),
        target=target,
        toolchains={"cargo": cargo_version, "rustc": rustc_version},
    )
    c3_build = build_evidence(
        commands=(
            "cargo build --release --offline --manifest-path "
            "$SOURCE_SNAPSHOT/cli/Cargo.toml --bin c3",
            "c3 contract codegen <target> $DESCRIPTOR --out-dir $OUTPUT",
        ),
        target=target,
        toolchains={"cargo": cargo_version, "rustc": rustc_version},
    )
    python_build = build_evidence(
        commands=(
            "uv build --sdist --python $PYTHON_CURRENT "
            "--out-dir $OUTPUT/python --no-create-gitignore "
            "$SOURCE_SNAPSHOT/sdk/python",
            "uv build --wheel --python $PYTHON_310 "
            "--out-dir $OUTPUT/python/cp310 --no-create-gitignore "
            "$SOURCE_SNAPSHOT/sdk/python",
            "uv build --wheel --python $PYTHON_CURRENT "
            "--out-dir $OUTPUT/python/current --no-create-gitignore "
            "$SOURCE_SNAPSHOT/sdk/python",
        ),
        target=target,
        toolchains={
            "python-current": tool_version(
                [str(python_current), "--version"],
                cwd=ROOT,
                log=log,
            ),
            "python-3.10": tool_version(
                [str(python_310), "--version"],
                cwd=ROOT,
                log=log,
            ),
            "uv": uv_version,
            "rustc": rustc_version,
        },
    )
    python_dependency_build = build_evidence(
        commands=(
            "python3.10 -m pip download --only-binary=:all: --no-deps "
            f"--dest $OUTPUT/python/dependencies/cp310 numpy=="
            f"{NUMPY_VERSIONS['cp310']}",
            "python3 -m pip download --only-binary=:all: --no-deps "
            f"--dest $OUTPUT/python/dependencies/current numpy=="
            f"{NUMPY_VERSIONS['current']}",
        ),
        target="python-multi-abi",
        toolchains={
            "pip@cp310": pip_version(python_310, log=log),
            "pip@current": pip_version(python_current, log=log),
            "python@cp310": tool_version(
                [str(python_310), "--version"],
                cwd=ROOT,
                log=log,
            ),
            "python@current": tool_version(
                [str(python_current), "--version"],
                cwd=ROOT,
                log=log,
            ),
        },
    )
    node_build = build_evidence(
        commands=(
            "npm ci --ignore-scripts --no-audit --no-fund",
            "npm pack --json --pack-destination $OUTPUT/typescript",
            f"npm pack typescript@{TYPESCRIPT_VERSION} --json "
            "--pack-destination $OUTPUT/typescript",
        ),
        target=target,
        toolchains={
            "node": node_version,
            "npm": npm_version,
            "cargo": cargo_version,
            "rustc": rustc_version,
        },
    )
    sources = {"c-two": c2_source, "fastdb": fastdb_source}
    artifacts = describe_artifacts(
        output=output,
        sources=sources,
        fastdb_manifest=fastdb_manifest,
        c2_rust_archives=c2_rust_archives,
        c3=c3,
        python_packages=python_packages,
        python_dependencies=python_dependencies,
        node_packages=node_packages,
        generated=generated,
        rust_build=rust_build,
        python_build=python_build,
        python_dependency_build=python_dependency_build,
        node_build=node_build,
        c3_build=c3_build,
    )
    top_build = build_evidence(
        commands=(
            "python -m tools.local_rc.build_candidate "
            "--output $OUTPUT --fastdb-candidate $FASTDB_CANDIDATE "
            "--python-current $PYTHON_CURRENT --python-310 $PYTHON_310",
        ),
        target=target,
        toolchains={
            "cargo": cargo_version,
            "rustc": rustc_version,
            "uv": uv_version,
            "node": node_version,
            "npm": npm_version,
            "cargo-local-registry": cargo_local_registry_version(),
        },
    )
    source_list = [c2_source, fastdb_source]
    manifest = construct_manifest(
        output,
        sources=source_list,
        artifacts=artifacts,
        build=top_build,
        enforce_candidate_set=False,
    )
    first = canonical_json_bytes(manifest)
    reconstructed = construct_manifest(
        output,
        sources=source_list,
        artifacts=artifacts,
        build=top_build,
        enforce_candidate_set=False,
    )
    if canonical_json_bytes(reconstructed) != first:
        raise CandidateError("candidate manifest reconstruction is not stable")
    write_manifest(
        output / MANIFEST_FILENAME,
        manifest,
        candidate_root=output,
        enforce_candidate_set=True,
    )
    verified = verify_manifest(
        output,
        expected_commits={
            "c-two": c2_source["commit"],
            "fastdb": fastdb_source["commit"],
        },
        enforce_candidate_set=True,
    )

    shutil.rmtree(source_root)
    shutil.rmtree(build_root)
    return verified


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Build the complete C-Two local release candidate."
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--fastdb-candidate", type=Path, required=True)
    parser.add_argument("--python-current", type=Path, required=True)
    parser.add_argument("--python-310", type=Path, required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    manifest = build_candidate(
        output=args.output.resolve(),
        fastdb_candidate=args.fastdb_candidate.resolve(),
        python_current=args.python_current.resolve(),
        python_310=args.python_310.resolve(),
    )
    print(
        json.dumps(
            {
                "manifest": str(
                    args.output.resolve() / MANIFEST_FILENAME
                ),
                "artifacts": len(manifest["artifacts"]),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CandidateError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
