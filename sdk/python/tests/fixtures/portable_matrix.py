from __future__ import annotations

from dataclasses import dataclass
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from types import ModuleType
from typing import Any
from urllib.parse import quote, urlencode
from urllib.request import ProxyHandler, Request, build_opener

import c_two as cc
from tests.fixtures.process_control import portable_command, readline_with_timeout
from c_two.config.settings import settings
from c_two.transport.registry import _ProcessRegistry
from fastdb4py.payload import Payload, PayloadError, View

from tests.fixtures.portable_interop import (
    GRAPH_SPEC_PATH,
    RECORD_SPEC_PATH,
    PortableNoPayload,
    PortableObjectGraph,
    PortableRecord,
    assert_graph_detached,
    assert_record_detached,
    assert_view_invalidated,
    build_graph_payload,
    build_record_payload,
    inspect_graph_payload,
    inspect_record_payload,
)


REPOSITORY = Path(__file__).resolve().parents[4]
RUST_HARNESS_SOURCE = (
    REPOSITORY / "sdk/python/tests/fixtures/portable_matrix_rust.rs"
)
TARGET_DIR = REPOSITORY / "core/target"
_CANDIDATE_CONTRACT_ROOT = os.environ.get(
    "C2_PORTABLE_MATRIX_CONTRACT_ROOT"
)
_contract_root = (
    Path(_CANDIDATE_CONTRACT_ROOT)
    if _CANDIDATE_CONTRACT_ROOT
    else REPOSITORY / "tests/fixtures/contracts"
)
DESCRIPTOR_PATHS = {
    "no-payload": _contract_root / "portable-no-payload.contract.json",
    "record-v1": _contract_root / "portable-record-v1.contract.json",
    "object-graph-v1": _contract_root
    / "portable-object-graph-v1.contract.json",
}
CRM_CLASSES = {
    "no-payload": PortableNoPayload,
    "record-v1": PortableRecord,
    "object-graph-v1": PortableObjectGraph,
}
IPC_OVERRIDES = {
    "pool_segment_size": 1024 * 1024,
    "max_pool_segments": 1,
    "reassembly_segment_size": 1024 * 1024,
    "reassembly_max_segments": 1,
    "max_total_chunks": 32,
    "chunk_gc_interval": 1.0,
    "chunk_assembler_timeout": 10.0,
    "max_reassembly_bytes": 16 * 1024 * 1024,
    "chunk_size": 64 * 1024,
}
_SHA256 = re.compile(r"^[0-9a-f]{64}$")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _package_tree_sha256(root: Path) -> str:
    files = sorted(
        path
        for path in root.rglob("*")
        if path.is_file()
        and "__pycache__" not in path.parts
        and path.suffix != ".pyc"
    )
    if not files:
        raise AssertionError(f"Python package tree is empty: {root}")
    digest = hashlib.sha256()
    for path in files:
        relative = path.relative_to(root).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        content_digest = bytes.fromhex(sha256_file(path))
        digest.update(len(content_digest).to_bytes(8, "big"))
        digest.update(content_digest)
    return digest.hexdigest()


def _configured_sha256(name: str, fallback: str) -> str:
    value = os.environ.get(name, fallback)
    if _SHA256.fullmatch(value) is None:
        raise AssertionError(f"{name} must be a lowercase SHA-256 digest")
    return value


def _path_dependency(path: Path) -> str:
    return json.dumps(str(path))


def _run_checked(
    command: list[str],
    *,
    cwd: Path,
    environment: dict[str, str] | None = None,
    timeout: float = 180,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        portable_command(command),
        cwd=cwd,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if completed.returncode != 0:
        raise AssertionError(
            f"command failed ({completed.returncode}): {command!r}\n"
            f"stdout:\n{completed.stdout}\n"
            f"stderr:\n{completed.stderr}"
        )
    return completed


def _c3_binary() -> Path:
    configured = os.environ.get("C2_PORTABLE_MATRIX_C3_BIN")
    if configured:
        candidate = Path(configured)
        if not candidate.is_file():
            raise AssertionError(
                f"C2_PORTABLE_MATRIX_C3_BIN is not a file: {candidate}"
            )
        return candidate
    name = "c3.exe" if os.name == "nt" else "c3"
    candidates = (
        REPOSITORY / "cli/target/debug" / name,
        REPOSITORY / "cli/target/release" / name,
    )
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    _run_checked(
        [
            "cargo",
            "build",
            "--manifest-path",
            str(REPOSITORY / "cli/Cargo.toml"),
            "--bin",
            "c3",
        ],
        cwd=REPOSITORY,
        timeout=300,
    )
    candidate = candidates[0]
    if not candidate.is_file():
        raise AssertionError(f"cargo did not produce the c3 binary at {candidate}")
    return candidate


def _import_generated_python(path: Path, payload: str) -> ModuleType:
    module_name = f"c_two_portable_matrix_{payload.replace('-', '_')}"
    sys.modules.pop(module_name, None)
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise AssertionError(f"cannot import generated Python module: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(module_name, None)
        raise
    return module


@dataclass(frozen=True)
class ContractFacts:
    descriptor_sha256: str
    release_ref: dict[str, Any]
    fastdb_spec_sha256: str | None


@dataclass
class MatrixArtifacts:
    work: Path
    c3_binary: Path
    rust_harness: Path
    rust_package_sha256: str
    python_package_sha256: str
    c3_sha256: str
    generated_python: dict[str, ModuleType]
    contracts: dict[str, ContractFacts]

    @classmethod
    def prepare(cls, work: Path) -> MatrixArtifacts:
        c3_binary = _c3_binary()
        generated_root = work / "generated"
        generated_python: dict[str, ModuleType] = {}
        contracts: dict[str, ContractFacts] = {}

        for payload, descriptor_path in DESCRIPTOR_PATHS.items():
            descriptor_bytes = descriptor_path.read_bytes()
            if not descriptor_bytes:
                raise AssertionError(f"empty matrix descriptor: {descriptor_path}")
            canonical_outputs: list[bytes] = []
            for target in ("rust", "python"):
                destination = generated_root / payload / target
                destination.parent.mkdir(parents=True, exist_ok=True)
                _run_checked(
                    [
                        str(c3_binary),
                        "contract",
                        "codegen",
                        target,
                        str(descriptor_path),
                        "--out-dir",
                        str(destination),
                    ],
                    cwd=REPOSITORY,
                )
                canonical_outputs.append(
                    (destination / "metadata/contract.json").read_bytes()
                )
            if canonical_outputs[0] != canonical_outputs[1]:
                raise AssertionError(
                    f"Rust/Python codegen admitted different descriptor bytes for {payload}"
                )

            python_root = generated_root / payload / "python"
            module = _import_generated_python(
                python_root / "python/c_two_contract.py",
                payload,
            )
            generated_python[payload] = module
            descriptor_sha256 = hashlib.sha256(canonical_outputs[0]).hexdigest()
            if descriptor_sha256 != module.DESCRIPTOR_SHA256:
                raise AssertionError(
                    f"generated descriptor digest mismatch for {payload}"
                )
            release_ref = json.loads(
                (
                    python_root
                    / "metadata/contract-release-ref.json"
                ).read_text(encoding="utf-8")
            )
            if release_ref["descriptor_sha256"] != descriptor_sha256:
                raise AssertionError(
                    f"release reference digest mismatch for {payload}"
                )
            input_digest = module.METHODS[0].input_sha256
            if payload == "no-payload":
                if input_digest is not None:
                    raise AssertionError("no-payload generated a FastDB binding")
            elif not isinstance(input_digest, str) or _SHA256.fullmatch(
                input_digest
            ) is None:
                raise AssertionError(
                    f"{payload} generated an invalid FastDB digest"
                )
            contracts[payload] = ContractFacts(
                descriptor_sha256=descriptor_sha256,
                release_ref=release_ref,
                fastdb_spec_sha256=input_digest,
            )

        fixtures = work / "fixtures"
        fixtures.mkdir()
        shutil.copyfile(RECORD_SPEC_PATH, fixtures / RECORD_SPEC_PATH.name)
        shutil.copyfile(GRAPH_SPEC_PATH, fixtures / GRAPH_SPEC_PATH.name)
        source = work / "src"
        source.mkdir()
        shutil.copyfile(RUST_HARNESS_SOURCE, source / "main.rs")
        candidate_cargo_home = os.environ.get(
            "C2_PORTABLE_MATRIX_CARGO_HOME"
        )
        if candidate_cargo_home:
            dependencies = """\
c-two = "=0.1.0"
fastdb = "=0.2.1"
"""
        else:
            dependencies = f"""\
c-two = {{ version = "0.1.0", path = {_path_dependency(REPOSITORY / "sdk/rust")} }}
fastdb = "=0.2.1"
"""
        manifest = f"""\
[package]
name = "c-two-portable-matrix-harness"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
{dependencies}\
"""
        (work / "Cargo.toml").write_text(manifest, encoding="utf-8")
        environment = os.environ.copy()
        target_dir = Path(
            os.environ.get(
                "C2_PORTABLE_MATRIX_CARGO_TARGET_DIR",
                str(TARGET_DIR),
            )
        )
        environment["CARGO_TARGET_DIR"] = str(target_dir)
        if candidate_cargo_home:
            environment["CARGO_HOME"] = candidate_cargo_home
        environment["C2_ENV_FILE"] = ""
        command = [
            "cargo",
            "build",
            "--manifest-path",
            str(work / "Cargo.toml"),
            "--message-format=json-render-diagnostics",
        ]
        if candidate_cargo_home:
            command.insert(2, "--offline")
        build = _run_checked(
            command,
            cwd=work,
            environment=environment,
            timeout=300,
        )
        c_two_rlibs: set[Path] = set()
        for line in build.stdout.splitlines():
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                message.get("reason") == "compiler-artifact"
                and message.get("target", {}).get("name") == "c_two"
            ):
                c_two_rlibs.update(
                    Path(filename)
                    for filename in message.get("filenames", ())
                    if filename.endswith(".rlib")
                )
        if len(c_two_rlibs) != 1:
            raise AssertionError(
                "matrix Cargo build did not identify exactly one linked c-two rlib: "
                f"{sorted(map(str, c_two_rlibs))}"
            )
        rust_harness = (
            target_dir
            / "debug"
            / (
                "c-two-portable-matrix-harness.exe"
                if os.name == "nt"
                else "c-two-portable-matrix-harness"
            )
        )
        if not rust_harness.is_file():
            raise AssertionError(f"missing Rust matrix harness: {rust_harness}")

        python_package_root = Path(cc.__file__).resolve().parent
        rust_package_sha256 = _configured_sha256(
            "C2_PORTABLE_MATRIX_RUST_PACKAGE_SHA256",
            sha256_file(next(iter(c_two_rlibs))),
        )
        python_package_sha256 = _configured_sha256(
            "C2_PORTABLE_MATRIX_PYTHON_PACKAGE_SHA256",
            _package_tree_sha256(python_package_root),
        )
        c3_sha256 = _configured_sha256(
            "C2_PORTABLE_MATRIX_C3_SHA256",
            sha256_file(c3_binary),
        )
        return cls(
            work=work,
            c3_binary=c3_binary,
            rust_harness=rust_harness,
            rust_package_sha256=rust_package_sha256,
            python_package_sha256=python_package_sha256,
            c3_sha256=c3_sha256,
            generated_python=generated_python,
            contracts=contracts,
        )

    def package_sha256(self, language: str) -> str:
        if language == "rust":
            return self.rust_package_sha256
        if language == "python":
            return self.python_package_sha256
        raise AssertionError(f"unknown SDK language: {language}")



def _parse_key_values(line: str, prefix: str) -> dict[str, str]:
    if not line.startswith(prefix):
        raise AssertionError(f"expected {prefix!r} line, got {line!r}")
    values: dict[str, str] = {}
    for item in line.removeprefix(prefix).strip().split():
        key, separator, value = item.partition("=")
        if not separator or not key or not value or key in values:
            raise AssertionError(f"malformed process receipt item: {item!r}")
        values[key] = value
    return values


class RustHost:
    def __init__(
        self,
        artifacts: MatrixArtifacts,
        *,
        payload: str,
        address: str,
        route_name: str,
        relay_url: str | None,
    ) -> None:
        self._process = subprocess.Popen(
            [
                str(artifacts.rust_harness),
                "host",
                payload,
                address,
                route_name,
                relay_url or "-",
            ],
            cwd=artifacts.work,
            env=_process_environment(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._ready_line = ""
        try:
            if self._process.stdout is None:
                raise AssertionError("Rust matrix host stdout is unavailable")
            self._ready_line = readline_with_timeout(self._process.stdout, 30)
            ready = _parse_key_values(self._ready_line, "READY ")
            if ready.get("address") != address:
                raise AssertionError(
                    f"Rust matrix host address mismatch: {ready!r}"
                )
            self.route_uid = ready["route_uid"]
            self.route_revision = int(ready["route_revision"])
        except BaseException:
            self._kill_and_collect()
            raise

    def _kill_and_collect(self) -> tuple[str, str]:
        if self._process.poll() is None:
            self._process.kill()
        return self._process.communicate()

    def stop(self) -> int:
        try:
            stdout, stderr = self._process.communicate(input="\n", timeout=30)
        except subprocess.TimeoutExpired:
            stdout, stderr = self._kill_and_collect()
            raise AssertionError(
                "Rust matrix host did not stop\n"
                f"stdout:\n{self._ready_line}{stdout}\n"
                f"stderr:\n{stderr}"
            )
        complete_stdout = self._ready_line + stdout
        if self._process.returncode != 0:
            raise AssertionError(
                f"Rust matrix host failed ({self._process.returncode})\n"
                f"stdout:\n{complete_stdout}\nstderr:\n{stderr}"
            )
        receipts = [
            _parse_key_values(line, "HOST_RECEIPT ")
            for line in complete_stdout.splitlines()
            if line.startswith("HOST_RECEIPT ")
        ]
        if len(receipts) != 1:
            raise AssertionError(
                f"Rust matrix host emitted {len(receipts)} receipts\n{complete_stdout}"
            )
        if self._process.poll() is None:
            raise AssertionError("Rust matrix host survived cleanup")
        return int(receipts[0]["calls"])


class MatrixRelay:
    def __init__(self, artifacts: MatrixArtifacts, row_id: str) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            port = int(probe.getsockname()[1])
        self.url = f"http://127.0.0.1:{port}"
        self._stdout = tempfile.TemporaryFile(mode="w+", encoding="utf-8")
        self._stderr = tempfile.TemporaryFile(mode="w+", encoding="utf-8")
        self._process = subprocess.Popen(
            [
                str(artifacts.c3_binary),
                "relay",
                "--bind",
                f"127.0.0.1:{port}",
                "--relay-id",
                f"matrix-relay-{hashlib.sha256(row_id.encode()).hexdigest()[:12]}",
                "--advertise-url",
                self.url,
                "--idle-timeout",
                "0",
            ],
            cwd=REPOSITORY,
            env=_process_environment(),
            stdout=self._stdout,
            stderr=self._stderr,
            text=True,
        )
        try:
            self._wait_ready()
        except BaseException:
            self.stop()
            raise

    def _logs(self) -> tuple[str, str]:
        self._stdout.flush()
        self._stderr.flush()
        self._stdout.seek(0)
        self._stderr.seek(0)
        return self._stdout.read(), self._stderr.read()

    def _wait_ready(self) -> None:
        opener = build_opener(ProxyHandler({}))
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self._process.poll() is not None:
                stdout, stderr = self._logs()
                raise AssertionError(
                    f"c3 relay exited before readiness\n"
                    f"stdout:\n{stdout}\nstderr:\n{stderr}"
                )
            try:
                with opener.open(f"{self.url}/health", timeout=0.5) as response:
                    if response.status == 200:
                        return
            except Exception:
                pass
            time.sleep(0.05)
        raise TimeoutError(f"c3 relay did not become ready: {self.url}")

    def stop(self) -> None:
        try:
            if self._process.poll() is None:
                if os.name == "nt":
                    self._process.terminate()
                else:
                    self._process.send_signal(signal.SIGINT)
                try:
                    self._process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self._process.kill()
                    self._process.wait(timeout=5)
            if self._process.poll() is None:
                raise AssertionError("c3 relay survived cleanup")
        finally:
            self._stdout.close()
            self._stderr.close()

    def __enter__(self) -> MatrixRelay:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.stop()


def _process_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["C2_ENV_FILE"] = ""
    environment["C2_RELAY_USE_PROXY"] = "false"
    environment["NO_PROXY"] = "127.0.0.1,localhost"
    environment["no_proxy"] = "127.0.0.1,localhost"
    return environment


def run_rust_client(
    artifacts: MatrixArtifacts,
    *,
    payload: str,
    transport: str,
    endpoint: str,
    route_name: str,
) -> dict[str, str]:
    completed = _run_checked(
        [
            str(artifacts.rust_harness),
            "client",
            payload,
            endpoint,
            route_name,
            transport,
        ],
        cwd=artifacts.work,
        environment=_process_environment(),
        timeout=60,
    )
    receipts = [
        _parse_key_values(line, "CLIENT_RECEIPT ")
        for line in completed.stdout.splitlines()
        if line.startswith("CLIENT_RECEIPT ")
    ]
    if len(receipts) != 1:
        raise AssertionError(
            f"Rust matrix client emitted {len(receipts)} receipts\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return receipts[0]


def wait_for_contract_resolution(
    relay_url: str,
    route_name: str,
    generated: ModuleType,
) -> dict[str, Any]:
    query = urlencode(
        {
            "crm_ns": generated.CRM_NAMESPACE,
            "crm_name": generated.CRM_NAME,
            "crm_ver": generated.CRM_VERSION,
            "abi_hash": generated.ABI_HASH,
            "signature_hash": generated.SIGNATURE_HASH,
        }
    )
    url = f"{relay_url}/_resolve/{quote(route_name, safe='')}?{query}"
    opener = build_opener(ProxyHandler({}))
    deadline = time.monotonic() + 10
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with opener.open(url, timeout=1) as response:
                routes = json.loads(response.read())
            if len(routes) == 1:
                route = routes[0]
                expected = generated.expected_route(route_name)
                for key, value in expected.items():
                    route_key = key.removeprefix("route_")
                    if key == "route_name":
                        route_key = "name"
                    if route.get(route_key) != value:
                        raise AssertionError(
                            f"relay resolution {route_key} mismatch: {route!r}"
                        )
                if not route.get("route_uid") or route.get("route_revision", 0) <= 0:
                    raise AssertionError(
                        f"relay resolution omitted route token: {route!r}"
                    )
                # Registration closes its attestation client before the relay
                # lazily acquires a data-plane client. Resolution therefore
                # proves catalog visibility, not yet upstream readiness. Poll
                # the same contract- and token-scoped probe used by an
                # explicit-relay client; the real matrix call itself is still
                # executed exactly once and is never replayed.
                probe = Request(
                    (
                        f"{route['relay_url'].rstrip('/')}/_probe/"
                        f"{quote(route_name, safe='')}"
                    ),
                    headers={
                        "x-c2-expected-crm-ns": expected["crm_ns"],
                        "x-c2-expected-crm-name": expected["crm_name"],
                        "x-c2-expected-crm-ver": expected["crm_ver"],
                        "x-c2-expected-abi-hash": expected["abi_hash"],
                        "x-c2-expected-signature-hash": expected[
                            "signature_hash"
                        ],
                        "x-c2-route-uid": route["route_uid"],
                        "x-c2-route-revision": str(route["route_revision"]),
                    },
                )
                with opener.open(probe, timeout=1) as response:
                    if response.status == 200:
                        return route
        except Exception as error:
            last_error = error
        time.sleep(0.05)
    raise TimeoutError(
        f"contract-scoped route did not resolve at {url}: {last_error}"
    )


def configure_python_runtime(
    *,
    server: bool,
    client: bool,
    relay_url: str | None,
) -> None:
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    cc.set_transport_policy(shm_threshold=1)
    if relay_url is not None:
        cc.set_relay_anchor(relay_url)
    if server:
        cc.set_server(ipc_overrides=IPC_OVERRIDES)
    if client:
        cc.set_client(ipc_overrides=IPC_OVERRIDES)


class NoPayloadResource:
    def __init__(self) -> None:
        self.calls = 0

    def ping(self) -> None:
        self.calls += 1
        return None

    def assert_lifetime(self) -> None:
        return None

    def close(self) -> None:
        return None


class RecordResource:
    def __init__(self) -> None:
        self.calls = 0
        self.borrowed_payload: Payload | None = None
        self.borrowed_view: View | None = None
        self.borrowed_detached: View | None = None
        self.outputs: list[Payload] = []

    def roundtrip(self, payload: Payload) -> Payload:
        self.calls += 1
        self.borrowed_payload = payload
        self.borrowed_view, self.borrowed_detached = inspect_record_payload(payload)
        output = build_record_payload()
        self.outputs.append(output)
        return output

    def assert_lifetime(self) -> None:
        if (
            self.borrowed_payload is None
            or self.borrowed_view is None
            or self.borrowed_detached is None
        ):
            raise AssertionError("record resource did not retain borrowed evidence")
        try:
            self.borrowed_payload.entry_view(0)
        except PayloadError as error:
            if error.symbol != "VIEW_INVALIDATED":
                raise
        else:
            raise AssertionError("borrowed record payload remained valid")
        assert_view_invalidated(self.borrowed_view)
        assert_record_detached(self.borrowed_detached)

    def close(self) -> None:
        _close_values(
            self.borrowed_view,
            self.borrowed_detached,
            self.borrowed_payload,
            *self.outputs,
        )


class ObjectGraphResource:
    def __init__(self) -> None:
        self.calls = 0
        self.borrowed_payload: Payload | None = None
        self.borrowed_view: View | None = None
        self.borrowed_detached: View | None = None
        self.outputs: list[Payload] = []

    def roundtrip(self, payload: Payload) -> Payload:
        self.calls += 1
        self.borrowed_payload = payload
        self.borrowed_view, self.borrowed_detached = inspect_graph_payload(payload)
        output = build_graph_payload()
        self.outputs.append(output)
        return output

    def assert_lifetime(self) -> None:
        if (
            self.borrowed_payload is None
            or self.borrowed_view is None
            or self.borrowed_detached is None
        ):
            raise AssertionError("graph resource did not retain borrowed evidence")
        try:
            self.borrowed_payload.entry_view(0)
        except PayloadError as error:
            if error.symbol != "VIEW_INVALIDATED":
                raise
        else:
            raise AssertionError("borrowed graph payload remained valid")
        assert_view_invalidated(self.borrowed_view)
        assert_graph_detached(self.borrowed_detached)

    def close(self) -> None:
        _close_values(
            self.borrowed_view,
            self.borrowed_detached,
            self.borrowed_payload,
            *self.outputs,
        )


def resource_for(payload: str) -> NoPayloadResource | RecordResource | ObjectGraphResource:
    if payload == "no-payload":
        return NoPayloadResource()
    if payload == "record-v1":
        return RecordResource()
    if payload == "object-graph-v1":
        return ObjectGraphResource()
    raise AssertionError(f"unknown payload profile: {payload}")


def input_lifetime_for(payload: str) -> dict[str, cc.InputLifetime]:
    if payload == "no-payload":
        return {}
    return {"roundtrip": cc.InputLifetime.BORROWED}


def invoke_generated_python(
    generated: ModuleType,
    proxy: Any,
    payload: str,
) -> None:
    client = generated.ContractClient(proxy.client)
    if payload == "no-payload":
        if client.method_0_ping() is not None:
            raise AssertionError("no-payload response must be None")
        return

    source = build_record_payload() if payload == "record-v1" else build_graph_payload()
    try:
        response = client.method_0_roundtrip(source)
        if payload == "record-v1":
            view, detached = inspect_record_payload(response)
        else:
            view, detached = inspect_graph_payload(response)
        try:
            response.invalidate()
            assert_view_invalidated(view)
            if payload == "record-v1":
                assert_record_detached(detached)
            else:
                assert_graph_detached(detached)
        finally:
            _close_values(view, detached, response)
    finally:
        source.close()


def _close_values(*values: object) -> None:
    for value in values:
        if value is None:
            continue
        close = getattr(value, "close", None)
        if callable(close):
            try:
                close()
            except Exception:
                pass
