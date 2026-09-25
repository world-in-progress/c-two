from __future__ import annotations

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import ModuleType
from typing import Any

import pytest

import c_two as cc
from tests.fixtures.process_control import readline_with_timeout
from c_two.config.settings import settings
from c_two.error import ContractMismatch
from c_two.transport.registry import _ProcessRegistry
from fastdb4py.payload import Payload, PayloadError
from tests.fixtures.portable_interop import (
    GRAPH_SPEC,
    GRAPH_SPEC_PATH,
    RECORD_SPEC,
    RECORD_SPEC_PATH,
    PortableInterop,
    PortableInteropResource,
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
    REPOSITORY
    / "sdk/python/tests/fixtures/portable_interop_rust.rs"
)
TARGET_DIR = REPOSITORY / "core/target"

pytestmark = pytest.mark.timeout(300)

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


@pytest.fixture(autouse=True)
def _clean_python_runtime() -> Any:
    previous_relay = settings._relay_anchor_address  # noqa: SLF001
    previous_threshold = settings._shm_threshold  # noqa: SLF001
    _ProcessRegistry.reset()
    settings.relay_anchor_address = None
    settings.shm_threshold = None
    yield
    _ProcessRegistry.reset()
    settings._relay_anchor_address = previous_relay  # noqa: SLF001
    settings._shm_threshold = previous_threshold  # noqa: SLF001


def _write_artifacts(root: Path, artifact_set: Any) -> list[dict[str, Any]]:
    inventory: list[dict[str, Any]] = []
    for artifact in artifact_set.artifacts:
        actual = hashlib.sha256(artifact.bytes).hexdigest()
        assert actual == artifact.sha256
        destination = root / artifact.relative_path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(artifact.bytes)
        inventory.append(
            {
                "path": artifact.relative_path,
                "sha256": artifact.sha256,
                "size": len(artifact.bytes),
                "owner": artifact.owner,
            }
        )
    assert inventory == sorted(inventory, key=lambda item: item["path"])
    return inventory


def _import_generated_python(generated_root: Path) -> ModuleType:
    module_path = generated_root / "python/c_two_contract.py"
    module_name = "c_two_portable_interop_generated_contract"
    sys.modules.pop(module_name, None)
    spec = importlib.util.spec_from_file_location(module_name, module_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


def _path_dependency(path: Path) -> str:
    return json.dumps(str(path))


def _prepare_harness(work: Path) -> tuple[Path, ModuleType]:
    descriptor_text = cc.export_contract_descriptor(PortableInterop)
    descriptor = descriptor_text.encode("utf-8")
    descriptor_value = json.loads(descriptor)
    assert [method["name"] for method in descriptor_value["methods"]] == [
        "graph_roundtrip",
        "ping",
        "record_roundtrip",
    ]

    rust_artifacts = cc.compile_contract_artifacts(descriptor, target="rust")
    rust_artifacts_repeat = cc.compile_contract_artifacts(descriptor, target="rust")
    python_artifacts = cc.compile_contract_artifacts(descriptor, target="python")
    python_artifacts_repeat = cc.compile_contract_artifacts(
        descriptor,
        target="python",
    )
    assert rust_artifacts == rust_artifacts_repeat
    assert python_artifacts == python_artifacts_repeat

    rust_inventory = _write_artifacts(work / "generated", rust_artifacts)
    python_inventory = _write_artifacts(
        work / "generated-python",
        python_artifacts,
    )
    print(
        "ARTIFACT_RECEIPT "
        + json.dumps(
            {
                "descriptor_sha256": hashlib.sha256(descriptor).hexdigest(),
                "rust": rust_inventory,
                "python": python_inventory,
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )

    generated_python = _import_generated_python(work / "generated-python")
    assert [method.name for method in generated_python.METHODS] == [
        "graph_roundtrip",
        "ping",
        "record_roundtrip",
    ]

    fixtures = work / "fixtures"
    fixtures.mkdir()
    shutil.copyfile(RECORD_SPEC_PATH, fixtures / RECORD_SPEC_PATH.name)
    shutil.copyfile(GRAPH_SPEC_PATH, fixtures / GRAPH_SPEC_PATH.name)
    source = work / "src"
    source.mkdir()
    shutil.copyfile(RUST_HARNESS_SOURCE, source / "main.rs")

    manifest = f"""\
[package]
name = "c-two-portable-interop-harness"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
c-two = {{ version = "0.1.0", path = {_path_dependency(REPOSITORY / "sdk/rust")} }}
fastdb = "=0.2.1"
"""
    (work / "Cargo.toml").write_text(manifest, encoding="utf-8")

    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(TARGET_DIR)
    completed = subprocess.run(
        ["cargo", "build", "--quiet", "--manifest-path", str(work / "Cargo.toml")],
        cwd=work,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=180,
    )
    assert completed.returncode == 0, (
        "generated Rust host/client harness failed to compile\n"
        f"stdout:\n{completed.stdout}\n"
        f"stderr:\n{completed.stderr}"
    )
    binary = TARGET_DIR / "debug" / ("c-two-portable-interop-harness.exe" if os.name == "nt" else "c-two-portable-interop-harness")
    assert binary.is_file()
    return binary, generated_python



class _RustHost:
    def __init__(self, binary: Path, address: str, route_name: str) -> None:
        self._binary = binary
        self._address = address
        self._route_name = route_name
        self._process: subprocess.Popen[str] | None = None
        self._ready_line = ""

    def start(self) -> None:
        self._process = subprocess.Popen(
            [str(self._binary), "host", self._address, self._route_name],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        try:
            assert self._process.stdout is not None
            self._ready_line = readline_with_timeout(self._process.stdout, 20)
            if self._ready_line != f"READY {self._address}\n":
                stdout, stderr = self._process.communicate(timeout=5)
                raise AssertionError(
                    "Rust host did not become ready\n"
                    f"stdout:\n{self._ready_line}{stdout}\n"
                    f"stderr:\n{stderr}"
                )
        except BaseException:
            if self._process.poll() is None:
                self._process.kill()
                self._process.communicate()
            raise

    def stop(self) -> dict[str, int]:
        process = self._process
        assert process is not None
        try:
            stdout, stderr = process.communicate(input="\n", timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate()
            raise AssertionError(
                f"Rust host did not stop\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )
        complete_stdout = self._ready_line + stdout
        print(complete_stdout, end="")
        if stderr:
            print(stderr, file=sys.stderr, end="")
        assert process.returncode == 0
        assert "OK rust-host" in complete_stdout
        prefix = "RUST_HOST_RECEIPT "
        receipts = [
            json.loads(line.removeprefix(prefix))
            for line in complete_stdout.splitlines()
            if line.startswith(prefix)
        ]
        assert len(receipts) == 1
        return receipts[0]


def _run_rust_client(
    binary: Path,
    address: str,
    route_name: str,
    label: str,
) -> None:
    completed = subprocess.run(
        [str(binary), "client", address, route_name, label],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    print(completed.stdout, end="")
    if completed.stderr:
        print(completed.stderr, file=sys.stderr, end="")
    assert completed.returncode == 0
    assert f"OK rust-client {label}" in completed.stdout


def _configure_python_runtime(*, server: bool, client: bool) -> None:
    _ProcessRegistry.reset()
    cc.set_transport_policy(shm_threshold=1)
    if server:
        cc.set_server(ipc_overrides=IPC_OVERRIDES)
    if client:
        cc.set_client(ipc_overrides=IPC_OVERRIDES)


def _assert_python_response_lifetime(
    payload: Any,
    *,
    graph: bool,
) -> None:
    if graph:
        source_view, detached = inspect_graph_payload(payload)
    else:
        source_view, detached = inspect_record_payload(payload)
    try:
        payload.invalidate()
        assert_view_invalidated(source_view)
        if graph:
            assert_graph_detached(detached)
        else:
            assert_record_detached(detached)
    finally:
        source_view.close()
        detached.close()
        payload.close()


def test_generated_rust_python_runtime_composition_is_bidirectional() -> None:
    with tempfile.TemporaryDirectory(prefix="c-two-portable-interop-") as directory:
        work = Path(directory)
        binary, generated = _prepare_harness(work)

        python_route = f"portable-python-{os.getpid()}"
        resource = PortableInteropResource()
        _configure_python_runtime(server=True, client=False)
        cc.register(
            PortableInterop,
            resource,
            name=python_route,
            input_lifetime={
                "graph_roundtrip": cc.InputLifetime.BORROWED,
            },
        )
        python_address = cc.server_address()
        assert python_address is not None
        try:
            _run_rust_client(
                binary,
                python_address,
                python_route,
                "rust-to-python",
            )
            assert resource.graph_calls == 1
            assert resource.ping_calls == 1
            assert resource.record_calls == 1
            assert resource.borrowed_payload is not None
            assert resource.borrowed_view is not None
            assert resource.borrowed_detached is not None
            with pytest.raises(PayloadError) as invalidated_payload:
                resource.borrowed_payload.entry_view(0)
            assert invalidated_payload.value.symbol == "VIEW_INVALIDATED"
            assert_view_invalidated(resource.borrowed_view)
            assert_graph_detached(resource.borrowed_detached)
            print(
                'PYTHON_HOST_RECEIPT {"borrowed_invalidation":1,'
                '"graph":1,"ping":1,"record":1}'
            )
        finally:
            _ProcessRegistry.reset()
            resource.close()

        rust_route = f"portable-rust-{os.getpid()}"
        rust_address = f"ipc://portable_rust_{os.getpid()}"
        rust_host = _RustHost(binary, rust_address, rust_route)
        rust_host.start()
        host_receipt: dict[str, int] | None = None
        try:
            _configure_python_runtime(server=False, client=True)

            @cc.crm(namespace="test.portable-interop", version="0.1.0")
            class WrongPortableInterop:
                @cc.transfer(input=RECORD_SPEC, output=RECORD_SPEC)
                def record_roundtrip(self, payload: Payload) -> Payload:
                    ...

            with pytest.raises(ContractMismatch, match="CRM contract mismatch"):
                cc.connect(
                    WrongPortableInterop,
                    name=rust_route,
                    address=rust_address,
                )

            proxy = cc.connect(
                PortableInterop,
                name=rust_route,
                address=rust_address,
            )
            try:
                client = generated.ContractClient(proxy.client)
                assert client.method_1_ping() is None

                record = build_record_payload()
                try:
                    record_response = client.method_2_record_roundtrip(record)
                    _assert_python_response_lifetime(
                        record_response,
                        graph=False,
                    )
                finally:
                    record.close()

                graph = build_graph_payload()
                try:
                    graph_response = client.method_0_graph_roundtrip(graph)
                    _assert_python_response_lifetime(
                        graph_response,
                        graph=True,
                    )
                    with pytest.raises(PayloadError) as digest_mismatch:
                        client.method_2_record_roundtrip(graph)
                    assert digest_mismatch.value.symbol == "DIGEST_MISMATCH"
                    assert digest_mismatch.value.path == "/payload/spec_sha256"
                finally:
                    graph.close()

                held_source = build_record_payload()
                try:
                    with cc.hold(proxy.record_roundtrip)(held_source) as held:
                        retained = held.value
                        held_root, held_detached = inspect_record_payload(retained)
                    try:
                        assert_view_invalidated(held_root)
                        assert_record_detached(held_detached)
                    finally:
                        held_root.close()
                        held_detached.close()
                        retained.close()
                finally:
                    held_source.close()
                print(
                    'PYTHON_CLIENT_RECEIPT {"digest_mismatch":1,'
                    '"graph":1,"held_invalidation":1,"ping":1,'
                    '"record":1,"route_mismatch":1}'
                )
            finally:
                cc.close(proxy)

            _run_rust_client(
                binary,
                rust_address,
                rust_route,
                "rust-to-rust",
            )
        finally:
            _ProcessRegistry.reset()
            host_receipt = rust_host.stop()

        assert host_receipt == {"graph": 2, "ping": 2, "record": 3}
