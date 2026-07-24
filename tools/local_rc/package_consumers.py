"""Validate and execute isolated consumers of the complete local candidate."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any, Mapping, Sequence

from tools.local_rc.artifact_manifest import (
    MANIFEST_FILENAME,
    verify_manifest,
)
from tools.local_rc.build_candidate import NUMPY_VERSIONS
from tools.local_rc.local_registry import (
    render_cargo_config,
    render_rust_consumer_manifest,
    validate_rust_consumer_manifest,
)
from tools.local_rc.portable_matrix_receipt import (
    load_and_validate_receipt as load_portable_receipt,
)
from tools.local_rc.typescript_receipt import (
    load_and_validate_receipt as load_typescript_receipt,
)


RECEIPT_SCHEMA = "c-two.package-consumer-receipt.v1"
TYPESCRIPT_VERSION = "5.9.3"
CONSUMER_ORDER = ("rust", "python-current", "python-3.10", "node")
_SHA256 = re.compile(r"^[0-9a-f]{64}$")
_TOP_FIELDS = {
    "schema",
    "evidence_stage",
    "candidate_manifest_sha256",
    "consumers",
    "cleanup",
}
_RUST_FIELDS = {
    "package_sha256",
    "version_only_manifest",
    "offline_local_registry",
    "real_call",
    "status",
}
_PYTHON_FIELDS = {
    "package_sha256",
    "no_index",
    "installed_only",
    "real_call",
    "lifetime",
    "status",
}
_NODE_FIELDS = {
    "package_sha256",
    "tarballs_only",
    "sibling_aliases",
    "real_call",
    "status",
}
_CLEANUP_FIELDS = {
    "host_processes_stopped",
    "relay_processes_stopped",
    "temporary_environments_removed",
}
_ENVIRONMENT_RESOLUTION_KEYS = {
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    "UV_PROJECT_ENVIRONMENT",
    "PIP_EDITABLE",
    "PIP_PREFIX",
    "CARGO_TARGET_DIR",
    "CARGO_MANIFEST_DIR",
    "NODE_PATH",
    "npm_config_prefix",
}


class ConsumerError(RuntimeError):
    """An isolated package consumer or its receipt is invalid."""


def canonical_json_bytes(document: Any) -> bytes:
    return (
        json.dumps(
            document,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def _object_from_pairs(
    pairs: list[tuple[str, Any]],
) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ConsumerError(f"receipt contains duplicate JSON key {key!r}")
        result[key] = value
    return result


def _exact_fields(
    value: Mapping[str, Any],
    expected: set[str],
    label: str,
) -> None:
    missing = sorted(expected - set(value))
    unknown = sorted(set(value) - expected)
    if missing:
        raise ConsumerError(f"{label} missing field(s): {missing}")
    if unknown:
        raise ConsumerError(f"{label} has unknown field(s): {unknown}")


def _sha256(value: object, label: str) -> str:
    if not isinstance(value, str) or _SHA256.fullmatch(value) is None:
        raise ConsumerError(f"{label} must be a lowercase SHA-256")
    return value


def _passed(value: Mapping[str, Any], field: str, label: str) -> None:
    if value[field] is not True:
        raise ConsumerError(f"{label}.{field} must be true")


def _package_hashes(
    value: object,
    *,
    expected: set[str],
    label: str,
) -> dict[str, str]:
    if not isinstance(value, dict):
        raise ConsumerError(f"{label} must be an object")
    _exact_fields(value, expected, label)
    return {
        name: _sha256(digest, f"{label}.{name}")
        for name, digest in value.items()
    }


def _validate_consumer(
    name: str,
    value: object,
) -> dict[str, Any]:
    label = f"$.consumers.{name}"
    if not isinstance(value, dict):
        raise ConsumerError(f"{label} must be an object")
    if name == "rust":
        _exact_fields(value, _RUST_FIELDS, label)
        _package_hashes(
            value["package_sha256"],
            expected={"c-two", "fastdb"},
            label=f"{label}.package_sha256",
        )
        for field in (
            "version_only_manifest",
            "offline_local_registry",
            "real_call",
        ):
            _passed(value, field, label)
    elif name in {"python-current", "python-3.10"}:
        _exact_fields(value, _PYTHON_FIELDS, label)
        _package_hashes(
            value["package_sha256"],
            expected={"c-two", "fastdb4py", "numpy"},
            label=f"{label}.package_sha256",
        )
        for field in ("no_index", "installed_only", "real_call", "lifetime"):
            _passed(value, field, label)
    elif name == "node":
        _exact_fields(value, _NODE_FIELDS, label)
        _package_hashes(
            value["package_sha256"],
            expected={"@c-two/c2-mem-ffi", "fastdb4ts", "typescript"},
            label=f"{label}.package_sha256",
        )
        _passed(value, "tarballs_only", label)
        if value["sibling_aliases"] is not False:
            raise ConsumerError(f"{label}.sibling_aliases must be false")
        _passed(value, "real_call", label)
    else:
        raise ConsumerError(f"unexpected consumer {name!r}")
    if value["status"] != "passed":
        raise ConsumerError(f"{label}.status must be 'passed'")
    return value


def validate_receipt(receipt: object) -> dict[str, Any]:
    if not isinstance(receipt, dict):
        raise ConsumerError("receipt must be an object")
    _exact_fields(receipt, _TOP_FIELDS, "$")
    if receipt["schema"] != RECEIPT_SCHEMA:
        raise ConsumerError(f"$.schema must be {RECEIPT_SCHEMA!r}")
    if receipt["evidence_stage"] != "candidate":
        raise ConsumerError("$.evidence_stage must be 'candidate'")
    _sha256(
        receipt["candidate_manifest_sha256"],
        "$.candidate_manifest_sha256",
    )
    consumers = receipt["consumers"]
    if not isinstance(consumers, dict):
        raise ConsumerError("$.consumers must be an object")
    _exact_fields(consumers, set(CONSUMER_ORDER), "$.consumers")
    for name in CONSUMER_ORDER:
        _validate_consumer(name, consumers[name])
    cleanup = receipt["cleanup"]
    if not isinstance(cleanup, dict):
        raise ConsumerError("$.cleanup must be an object")
    _exact_fields(cleanup, _CLEANUP_FIELDS, "$.cleanup")
    for field in sorted(_CLEANUP_FIELDS):
        _passed(cleanup, field, "$.cleanup")
    _reject_paths(receipt)
    return receipt


def _reject_paths(value: object, *, label: str = "$") -> None:
    if isinstance(value, dict):
        for key, member in value.items():
            _reject_paths(member, label=f"{label}.{key}")
    elif isinstance(value, list):
        for index, member in enumerate(value):
            _reject_paths(member, label=f"{label}[{index}]")
    elif isinstance(value, str) and (
        "/Users/" in value
        or "/private/" in value
        or "../c-two" in value
        or "../fastdb" in value
        or "../toodle" in value
    ):
        raise ConsumerError(f"{label} contains a retained local path")


def write_receipt(destination: Path, receipt: Mapping[str, Any]) -> None:
    validated = validate_receipt(dict(receipt))
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(canonical_json_bytes(validated))


def load_and_validate_receipt(path: Path) -> dict[str, Any]:
    try:
        source = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ConsumerError(f"could not read receipt {path}: {error}") from error
    try:
        document = json.loads(source, object_pairs_hook=_object_from_pairs)
    except json.JSONDecodeError as error:
        raise ConsumerError(f"receipt is invalid JSON: {error}") from error
    validated = validate_receipt(document)
    if canonical_json_bytes(validated) != source.encode("utf-8"):
        raise ConsumerError("receipt must use canonical JSON encoding")
    return validated


def clean_consumer_environment(
    environment: Mapping[str, str] | None = None,
) -> dict[str, str]:
    source = os.environ if environment is None else environment
    return {
        key: value
        for key, value in source.items()
        if key not in _ENVIRONMENT_RESOLUTION_KEYS
        and not key.startswith("PIP_EDITABLE_")
    }


def resolve_python_interpreter(
    environment_key: str,
    executable: str,
    *,
    fallback: str | None = None,
) -> Path:
    configured = os.environ.get(environment_key)
    discovered = (
        configured
        if configured is not None
        else shutil.which(executable) or fallback
    )
    if not discovered:
        raise ConsumerError(
            f"{executable} interpreter is required; set {environment_key}"
        )
    path = Path(discovered).expanduser()
    if not path.is_file():
        raise ConsumerError(
            f"{executable} interpreter does not exist at {path}; "
            f"set {environment_key}"
        )
    return path


def validate_installed_origin(
    origin: Path,
    consumer_root: Path,
    *,
    forbidden_roots: Sequence[Path],
) -> None:
    try:
        resolved = origin.resolve(strict=True)
    except OSError as error:
        raise ConsumerError(f"installed origin is missing: {origin}") from error
    try:
        resolved.relative_to(consumer_root.resolve(strict=True))
    except ValueError as error:
        raise ConsumerError(
            f"installed origin is outside the consumer root: {resolved}"
        ) from error
    for forbidden in forbidden_roots:
        try:
            resolved.relative_to(forbidden.resolve(strict=True))
        except ValueError:
            continue
        raise ConsumerError(
            f"installed origin resolves inside an active repository: {resolved}"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run_checked(
    command: Sequence[str],
    *,
    cwd: Path,
    environment: Mapping[str, str] | None = None,
    timeout: float = 1800,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        list(command),
        cwd=cwd,
        env=None if environment is None else dict(environment),
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="strict",
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise ConsumerError(
            f"command failed with exit {completed.returncode}: "
            f"{' '.join(command)}\n{completed.stdout}"
        )
    return completed


def _single_path(root: Path, pattern: str, label: str) -> Path:
    matches = tuple(root.glob(pattern))
    if len(matches) != 1:
        raise ConsumerError(
            f"{label} expected exactly one {pattern!r}, found {len(matches)}"
        )
    return matches[0]


def _artifact_map(
    manifest: Mapping[str, Any],
) -> dict[str, Mapping[str, Any]]:
    raw = manifest.get("artifacts")
    if not isinstance(raw, list):
        raise ConsumerError("candidate manifest has no artifact array")
    result: dict[str, Mapping[str, Any]] = {}
    for artifact in raw:
        if not isinstance(artifact, dict):
            raise ConsumerError("candidate artifact record is invalid")
        path = artifact.get("path")
        if not isinstance(path, str) or path in result:
            raise ConsumerError("candidate artifact paths are invalid")
        result[path] = artifact
    return result


def _artifact_digest(
    artifacts: Mapping[str, Mapping[str, Any]],
    path: Path,
    candidate: Path,
) -> str:
    try:
        relative = path.relative_to(candidate).as_posix()
    except ValueError as error:
        raise ConsumerError(
            f"package is outside the candidate: {path}"
        ) from error
    artifact = artifacts.get(relative)
    if artifact is None:
        raise ConsumerError(f"package is not in the candidate manifest: {relative}")
    digest = artifact.get("sha256")
    if not isinstance(digest, str) or digest != sha256_file(path):
        raise ConsumerError(f"package hash does not match manifest: {relative}")
    return digest


def _runtime_environment(
    candidate: Path,
    *,
    base: Mapping[str, str] | None = None,
) -> dict[str, str]:
    environment = clean_consumer_environment(base)
    library_dir = candidate / "fastdb/core/lib"
    environment.update(
        {
            "FASTDB_PAYLOAD_LINK_MODE": "system",
            "FASTDB_PAYLOAD_SYSTEM_LIB_DIR": str(library_dir),
            "C2_ENV_FILE": "",
            "C2_RELAY_ANCHOR_ADDRESS": "",
            "TZ": "UTC",
        }
    )
    if sys.platform == "darwin":
        environment["DYLD_LIBRARY_PATH"] = str(library_dir)
    elif os.name == "posix":
        environment["LD_LIBRARY_PATH"] = str(library_dir)
    elif os.name == "nt":
        environment["PATH"] = (
            str(library_dir)
            + os.pathsep
            + environment.get("PATH", "")
        )
    return environment


def _readline_with_timeout(
    stream,
    timeout: float,
) -> str:
    selector = selectors.DefaultSelector()
    selector.register(stream, selectors.EVENT_READ)
    try:
        if not selector.select(timeout):
            raise ConsumerError("timed out waiting for candidate host readiness")
        return stream.readline()
    finally:
        selector.close()


def _stop_host(
    process: subprocess.Popen[str],
) -> tuple[str, str]:
    try:
        stdout, stderr = process.communicate(input="\n", timeout=30)
    except subprocess.TimeoutExpired as error:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        stdout, stderr = process.communicate()
        raise ConsumerError(
            f"candidate Rust host did not stop\n{stdout}\n{stderr}"
        ) from error
    if process.returncode != 0:
        raise ConsumerError(
            f"candidate Rust host failed with {process.returncode}\n"
            f"{stdout}\n{stderr}"
        )
    return stdout, stderr


def run_rust_consumer(
    *,
    candidate: Path,
    registry: Path,
    artifacts: Mapping[str, Mapping[str, Any]],
    work: Path,
) -> dict[str, str]:
    consumer = work / "rust-consumer"
    (consumer / "src").mkdir(parents=True)
    shutil.copyfile(
        candidate / "harness/portable_matrix_rust.rs",
        consumer / "src/main.rs",
    )
    shutil.copytree(candidate / "generated", consumer / "generated")
    shutil.copytree(candidate / "fixtures/fastdb", consumer / "fixtures")
    manifest_source = render_rust_consumer_manifest()
    validate_rust_consumer_manifest(manifest_source)
    (consumer / "Cargo.toml").write_text(
        manifest_source,
        encoding="utf-8",
    )
    cargo_home = work / "rust-cargo-home"
    cargo_home.mkdir()
    (cargo_home / "config.toml").write_text(
        render_cargo_config(registry),
        encoding="utf-8",
    )
    environment = _runtime_environment(candidate)
    target_dir = work / "rust-target"
    environment.update(
        {
            "CARGO_HOME": str(cargo_home),
            "CARGO_TARGET_DIR": str(target_dir),
        }
    )
    build = run_checked(
        [
            "cargo",
            "build",
            "--release",
            "--offline",
            "--manifest-path",
            str(consumer / "Cargo.toml"),
            "--message-format=json-render-diagnostics",
        ],
        cwd=consumer,
        environment=environment,
    )
    repository = Path(__file__).resolve().parents[2]
    for forbidden in (
        str(repository),
        str(repository.parent / "fastdb"),
        str(repository.parent / "toodle"),
    ):
        if forbidden in build.stdout:
            raise ConsumerError(
                "Rust package consumer diagnostics reference an active repository"
            )
    binary = target_dir / "release/c-two-local-candidate-consumer"
    if os.name == "nt":
        binary = binary.with_suffix(".exe")
    if not binary.is_file():
        raise ConsumerError("Rust package consumer binary is missing")
    route = f"candidate-rust-{os.getpid()}"
    address = f"ipc://c2_candidate_rust_{os.getpid()}"
    host = subprocess.Popen(
        [
            str(binary),
            "host",
            "record-v1",
            address,
            route,
        ],
        cwd=consumer,
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        start_new_session=os.name == "posix",
    )
    ready = ""
    try:
        assert host.stdout is not None
        ready = _readline_with_timeout(host.stdout, 30)
        if not ready.startswith("READY "):
            raise ConsumerError(
                f"candidate Rust host did not become ready: {ready!r}"
            )
        client = run_checked(
            [
                str(binary),
                "client",
                "record-v1",
                address,
                route,
                "direct",
            ],
            cwd=consumer,
            environment=environment,
            timeout=120,
        )
        if "CLIENT_RECEIPT observed_path=DirectIpc" not in client.stdout:
            raise ConsumerError("candidate Rust client did not prove DirectIpc")
    finally:
        if host.poll() is None:
            host_stdout, host_stderr = _stop_host(host)
        else:
            host_stdout, host_stderr = host.communicate()
    complete = ready + host_stdout
    if "HOST_RECEIPT calls=1" not in complete:
        raise ConsumerError(
            f"candidate Rust host did not execute one call\n"
            f"{complete}\n{host_stderr}"
        )
    c_two = candidate / "rust/c-two-0.1.0.crate"
    fastdb = candidate / "fastdb/rust/fastdb-0.1.22.crate"
    return {
        "c-two": _artifact_digest(artifacts, c_two, candidate),
        "fastdb": _artifact_digest(artifacts, fastdb, candidate),
    }


PYTHON_CONSUMER = r'''
from __future__ import annotations

import json
from pathlib import Path
import sys
import time

import c_two as cc
import fastdb4py
import numpy
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload, PayloadError

SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [{
        "id": "value",
        "cardinality": "one",
        "type": {"kind": "u8", "nullable": False},
    }],
    "components": [],
}

@cc.crm(namespace="test.package-candidate", version="0.1.0")
class Echo:
    @cc.transfer(input=SPEC, output=SPEC)
    def echo(self, payload: Payload) -> Payload:
        ...

    @cc.transfer(input=SPEC, output=SPEC)
    def save_borrowed(self, payload: Payload) -> Payload:
        ...

class Resource:
    def __init__(self) -> None:
        self.saved = None

    def echo(self, payload: Payload) -> Payload:
        return payload

    def save_borrowed(self, payload: Payload) -> Payload:
        self.saved = payload
        return payload

def build_payload() -> Payload:
    encoded = json.dumps(SPEC, separators=(",", ":")).encode()
    spec = CompiledSpec.compile(encoded)
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(7)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()

prefix = Path(sys.prefix).resolve()
c_two_origin = Path(cc.__file__).resolve()
fastdb_origin = Path(fastdb4py.__file__).resolve()
numpy_origin = Path(numpy.__file__).resolve()
assert c_two_origin.is_relative_to(prefix), (c_two_origin, prefix)
assert fastdb_origin.is_relative_to(prefix), (fastdb_origin, prefix)
assert numpy_origin.is_relative_to(prefix), (numpy_origin, prefix)

resource = Resource()
source = build_payload()
ordinary = None
borrowed = None
retained = None
root = None
detached = None
proxy = None
try:
    cc.register(
        Echo,
        resource,
        name="package-candidate",
        input_lifetime={"save_borrowed": cc.InputLifetime.BORROWED},
    )
    time.sleep(0.2)
    address = cc.server_address()
    assert address is not None
    proxy = cc.connect(Echo, name="package-candidate", address=address)
    ordinary = proxy.echo(source)
    assert proxy.client._mode == "ipc"
    with ordinary.entry_view(0) as values:
        with values.at(0) as value:
            assert value.get_u8() == 7

    held = cc.hold(proxy.echo)(source)
    retained = held.value
    sequence = retained.entry_view(0)
    root = sequence.at(0)
    sequence.close()
    detached = root.materialize()
    assert root.get_u8() == 7
    held.release()
    try:
        root.get_u8()
    except PayloadError as error:
        assert error.symbol == "VIEW_INVALIDATED"
    else:
        raise AssertionError("held view remained valid after release")
    assert detached.get_u8() == 7

    borrowed = proxy.save_borrowed(source)
    assert resource.saved is not None
    try:
        resource.saved.entry_view(0)
    except PayloadError as error:
        assert error.symbol == "VIEW_INVALIDATED"
    else:
        raise AssertionError("borrowed input remained valid after callback")
finally:
    if proxy is not None:
        cc.close(proxy)
    for value in (detached, root, retained, borrowed, ordinary, source):
        if value is not None:
            value.close()
    cc.shutdown()

print(json.dumps({
    "c_two_origin": str(c_two_origin),
    "fastdb_origin": str(fastdb_origin),
    "numpy_origin": str(numpy_origin),
    "numpy_version": numpy.__version__,
    "host_call": True,
    "hold_invalidated": True,
    "materialized_survived": True,
    "borrowed_invalidated": True,
    "shutdown": True,
}, sort_keys=True))
'''


def install_python_consumer(
    *,
    candidate: Path,
    artifacts: Mapping[str, Mapping[str, Any]],
    work: Path,
    label: str,
    interpreter: Path,
    c_two_wheel: Path,
    fastdb_wheel: Path,
    numpy_wheel: Path,
    numpy_version: str,
) -> tuple[Path, dict[str, str]]:
    environment_root = work / f"venv-{label}"
    run_checked(
        [
            "uv",
            "venv",
            "--python",
            str(interpreter),
            str(environment_root),
        ],
        cwd=work,
        environment=clean_consumer_environment(),
    )
    python = (
        environment_root / "Scripts/python.exe"
        if os.name == "nt"
        else environment_root / "bin/python"
    )
    run_checked(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(python),
            "--no-index",
            "--find-links",
            str(c_two_wheel.parent),
            "--find-links",
            str(fastdb_wheel.parent),
            "--find-links",
            str(numpy_wheel.parent),
            f"numpy=={numpy_version}",
            "fastdb4py==0.1.22",
            "c-two==0.5.1",
        ],
        cwd=work,
        environment=clean_consumer_environment(),
    )
    script = work / f"python-consumer-{label}.py"
    script.write_text(PYTHON_CONSUMER, encoding="utf-8")
    environment = _runtime_environment(candidate)
    smoke = run_checked(
        [str(python), "-I", str(script)],
        cwd=work,
        environment=environment,
        timeout=180,
    )
    try:
        evidence = json.loads(smoke.stdout.splitlines()[-1])
    except (json.JSONDecodeError, IndexError) as error:
        raise ConsumerError(
            f"Python {label} consumer emitted no evidence\n{smoke.stdout}"
        ) from error
    for field in (
        "host_call",
        "hold_invalidated",
        "materialized_survived",
        "borrowed_invalidated",
        "shutdown",
    ):
        if evidence.get(field) is not True:
            raise ConsumerError(
                f"Python {label} consumer did not prove {field}"
            )
    validate_installed_origin(
        Path(evidence["c_two_origin"]),
        environment_root,
        forbidden_roots=(
            Path(__file__).resolve().parents[2],
            Path(__file__).resolve().parents[3] / "fastdb",
        ),
    )
    validate_installed_origin(
        Path(evidence["fastdb_origin"]),
        environment_root,
        forbidden_roots=(
            Path(__file__).resolve().parents[2],
            Path(__file__).resolve().parents[3] / "fastdb",
        ),
    )
    validate_installed_origin(
        Path(evidence["numpy_origin"]),
        environment_root,
        forbidden_roots=(
            Path(__file__).resolve().parents[2],
            Path(__file__).resolve().parents[3] / "fastdb",
        ),
    )
    if evidence.get("numpy_version") != numpy_version:
        raise ConsumerError(
            f"Python {label} installed unexpected numpy version"
        )
    return python, {
        "c-two": _artifact_digest(artifacts, c_two_wheel, candidate),
        "fastdb4py": _artifact_digest(artifacts, fastdb_wheel, candidate),
        "numpy": _artifact_digest(artifacts, numpy_wheel, candidate),
    }


def _install_pytest(python: Path, work: Path) -> None:
    run_checked(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(python),
            "pytest==8.4.2",
            "pytest-timeout==2.4.0",
        ],
        cwd=work,
        environment=clean_consumer_environment(),
    )


def run_candidate_suites(
    *,
    candidate: Path,
    registry: Path,
    artifacts: Mapping[str, Mapping[str, Any]],
    python: Path,
    work: Path,
) -> tuple[dict[str, Any], dict[str, Any]]:
    _install_pytest(python, work)
    cargo_home = work / "matrix-cargo-home"
    cargo_home.mkdir()
    (cargo_home / "config.toml").write_text(
        render_cargo_config(registry),
        encoding="utf-8",
    )
    c_two_crate = candidate / "rust/c-two-0.1.0.crate"
    c_two_wheel = _single_path(
        candidate / "python/current",
        "c_two-*.whl",
        "current C-Two wheel",
    )
    c3 = candidate / ("bin/c3.exe" if os.name == "nt" else "bin/c3")
    fastdb4ts = candidate / "fastdb/typescript/fastdb4ts-0.0.3.tgz"
    c2_mem = candidate / "typescript/c-two-c2-mem-ffi-0.1.0.tgz"
    typescript = candidate / f"typescript/typescript-{TYPESCRIPT_VERSION}.tgz"
    environment = _runtime_environment(candidate)
    environment.update(
        {
            "C2_PORTABLE_MATRIX_C3_BIN": str(c3),
            "C2_PORTABLE_MATRIX_CARGO_HOME": str(cargo_home),
            "C2_PORTABLE_MATRIX_CARGO_TARGET_DIR": str(
                work / "matrix-target"
            ),
            "C2_PORTABLE_MATRIX_FIXTURE_ROOT": str(
                candidate / "fixtures/fastdb"
            ),
            "C2_PORTABLE_MATRIX_CONTRACT_ROOT": str(
                candidate / "fixtures/contracts"
            ),
            "C2_PORTABLE_MATRIX_RUST_PACKAGE_SHA256": _artifact_digest(
                artifacts,
                c_two_crate,
                candidate,
            ),
            "C2_PORTABLE_MATRIX_PYTHON_PACKAGE_SHA256": _artifact_digest(
                artifacts,
                c_two_wheel,
                candidate,
            ),
            "C2_PORTABLE_MATRIX_C3_SHA256": _artifact_digest(
                artifacts,
                c3,
                candidate,
            ),
            "C2_PORTABLE_MATRIX_EVIDENCE_STAGE": "candidate",
            "C2_PORTABLE_MATRIX_RECEIPT": str(
                candidate / "portable-matrix-receipt.v1.json"
            ),
            "C2_TYPESCRIPT_FASTDB_PACKAGE": str(fastdb4ts),
            "C2_TYPESCRIPT_C2_MEM_PACKAGE": str(c2_mem),
            "C2_TYPESCRIPT_COMPILER_PACKAGE": str(typescript),
            "C2_TYPESCRIPT_EVIDENCE_STAGE": "candidate",
            "C2_TYPESCRIPT_RECEIPT": str(
                candidate / "typescript-real-call-receipt.v1.json"
            ),
        }
    )
    repository = Path(__file__).resolve().parents[2]
    run_checked(
        [
            str(python),
            "-m",
            "pytest",
            "tests/integration/test_portable_payload_matrix.py",
            "tests/integration/test_typescript_real_calls.py",
            "-q",
            "--timeout=600",
        ],
        cwd=repository / "sdk/python",
        environment=environment,
        timeout=1800,
    )
    portable = load_portable_receipt(
        candidate / "portable-matrix-receipt.v1.json",
        expected_stage="candidate",
    )
    typescript_receipt = load_typescript_receipt(
        candidate / "typescript-real-call-receipt.v1.json",
        expected_stage="candidate",
    )
    expected_language_hashes = {
        "rust": _artifact_digest(artifacts, c_two_crate, candidate),
        "python": _artifact_digest(artifacts, c_two_wheel, candidate),
    }
    expected_c3 = _artifact_digest(artifacts, c3, candidate)
    for row in portable["rows"]:
        client = row["client_language"]
        host = row["host_language"]
        if row["packages"] != {
            "client_sha256": expected_language_hashes[client],
            "host_sha256": expected_language_hashes[host],
        }:
            raise ConsumerError(
                f"portable row {row['id']} package hashes do not match "
                "the candidate manifest"
            )
        expected_row_c3 = expected_c3 if row["transport"] == "relay" else None
        if row["c3_sha256"] != expected_row_c3:
            raise ConsumerError(
                f"portable row {row['id']} c3 hash does not match"
            )
    if typescript_receipt["packages"] != {
        "fastdb4ts_sha256": _artifact_digest(
            artifacts,
            fastdb4ts,
            candidate,
        ),
        "c2_mem_ffi_sha256": _artifact_digest(
            artifacts,
            c2_mem,
            candidate,
        ),
        "c3_sha256": expected_c3,
    }:
        raise ConsumerError(
            "TypeScript receipt package hashes do not match the manifest"
        )
    return portable, typescript_receipt


def run_candidate_consumers(
    *,
    candidate: Path,
    receipt_path: Path,
) -> dict[str, Any]:
    manifest = verify_manifest(candidate, enforce_candidate_set=True)
    manifest_path = candidate / MANIFEST_FILENAME
    manifest_sha256 = sha256_file(manifest_path)
    artifacts = _artifact_map(manifest)
    registry = candidate.parent / "registry"
    if not registry.is_dir():
        raise ConsumerError(
            f"candidate sibling local registry is missing: {registry}"
        )
    current_python = resolve_python_interpreter(
        "C2_LRC_PYTHON_CURRENT",
        "python3",
        fallback=sys.executable,
    )
    python_310 = resolve_python_interpreter(
        "C2_LRC_PYTHON_310",
        "python3.10",
    )
    c2_current = _single_path(
        candidate / "python/current",
        "c_two-*.whl",
        "current C-Two wheel",
    )
    c2_cp310 = _single_path(
        candidate / "python/cp310",
        "c_two-*.whl",
        "CPython 3.10 C-Two wheel",
    )
    fastdb_current = _single_path(
        candidate / "fastdb/python/current",
        "fastdb4py-*.whl",
        "current FastDB wheel",
    )
    fastdb_cp310 = _single_path(
        candidate / "fastdb/python/cp310",
        "fastdb4py-*.whl",
        "CPython 3.10 FastDB wheel",
    )
    numpy_current = _single_path(
        candidate / "python/dependencies/current",
        f"numpy-{NUMPY_VERSIONS['current']}-*.whl",
        "current NumPy wheel",
    )
    numpy_cp310 = _single_path(
        candidate / "python/dependencies/cp310",
        f"numpy-{NUMPY_VERSIONS['cp310']}-*.whl",
        "CPython 3.10 NumPy wheel",
    )
    with tempfile.TemporaryDirectory(
        prefix="c-two-candidate-consumers-",
        dir=candidate.parent,
    ) as directory:
        work = Path(directory)
        rust_hashes = run_rust_consumer(
            candidate=candidate,
            registry=registry,
            artifacts=artifacts,
            work=work,
        )
        current_installed, current_hashes = install_python_consumer(
            candidate=candidate,
            artifacts=artifacts,
            work=work,
            label="current",
            interpreter=current_python,
            c_two_wheel=c2_current,
            fastdb_wheel=fastdb_current,
            numpy_wheel=numpy_current,
            numpy_version=NUMPY_VERSIONS["current"],
        )
        _, cp310_hashes = install_python_consumer(
            candidate=candidate,
            artifacts=artifacts,
            work=work,
            label="3.10",
            interpreter=python_310,
            c_two_wheel=c2_cp310,
            fastdb_wheel=fastdb_cp310,
            numpy_wheel=numpy_cp310,
            numpy_version=NUMPY_VERSIONS["cp310"],
        )
        _, typescript_receipt = run_candidate_suites(
            candidate=candidate,
            registry=registry,
            artifacts=artifacts,
            python=current_installed,
            work=work,
        )
        fastdb4ts = candidate / "fastdb/typescript/fastdb4ts-0.0.3.tgz"
        c2_mem = candidate / "typescript/c-two-c2-mem-ffi-0.1.0.tgz"
        typescript = candidate / f"typescript/typescript-{TYPESCRIPT_VERSION}.tgz"
        receipt = {
            "schema": RECEIPT_SCHEMA,
            "evidence_stage": "candidate",
            "candidate_manifest_sha256": manifest_sha256,
            "consumers": {
                "rust": {
                    "package_sha256": rust_hashes,
                    "version_only_manifest": True,
                    "offline_local_registry": True,
                    "real_call": True,
                    "status": "passed",
                },
                "python-current": {
                    "package_sha256": current_hashes,
                    "no_index": True,
                    "installed_only": True,
                    "real_call": True,
                    "lifetime": True,
                    "status": "passed",
                },
                "python-3.10": {
                    "package_sha256": cp310_hashes,
                    "no_index": True,
                    "installed_only": True,
                    "real_call": True,
                    "lifetime": True,
                    "status": "passed",
                },
                "node": {
                    "package_sha256": {
                        "@c-two/c2-mem-ffi": _artifact_digest(
                            artifacts,
                            c2_mem,
                            candidate,
                        ),
                        "fastdb4ts": _artifact_digest(
                            artifacts,
                            fastdb4ts,
                            candidate,
                        ),
                        "typescript": _artifact_digest(
                            artifacts,
                            typescript,
                            candidate,
                        ),
                    },
                    "tarballs_only": True,
                    "sibling_aliases": False,
                    "real_call": bool(typescript_receipt["rows"]),
                    "status": "passed",
                },
            },
            "cleanup": {
                "host_processes_stopped": True,
                "relay_processes_stopped": True,
                "temporary_environments_removed": True,
            },
        }
        write_receipt(receipt_path, receipt)
    validated = load_and_validate_receipt(receipt_path)
    verify_manifest(candidate, enforce_candidate_set=True)
    return validated


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run isolated consumers for a C-Two local candidate."
    )
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if not args.candidate.is_dir():
        raise ConsumerError(
            f"candidate directory does not exist: {args.candidate}"
        )
    receipt = run_candidate_consumers(
        candidate=args.candidate.resolve(),
        receipt_path=args.receipt.resolve(),
    )
    print(
        json.dumps(
            {
                "receipt": str(args.receipt.resolve()),
                "consumers": len(receipt["consumers"]),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ConsumerError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
