"""Generated Node stream clients against real Python and Rust IPC hosts."""
from __future__ import annotations

import json
from itertools import product
import os
from pathlib import Path
import shutil
import subprocess
import threading

import pytest

import c_two as cc
from tests.fixtures.process_control import portable_command, readline_with_timeout
from tests.fixtures.stream_matrix import (
    CHUNK_SIZE, TRANSPORT_CASES, Portable, PythonHost, RustHarness,
    build_payload, inspect, process_environment,
)
from tests.fixtures.typescript_project import write_node_tsconfig

REPOSITORY = Path(__file__).resolve().parents[4]
NODE_FIXTURE = REPOSITORY / "sdk/python/tests/fixtures/typescript_server_stream.mjs"
TS_SOURCE = REPOSITORY / "core/foundation/c2-codegen/assets/typescript_transport.ts"


def _checked(command: list[str], root: Path) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        portable_command(command), cwd=root, capture_output=True, text=True,
        encoding="utf-8", timeout=300, check=False,
    )
    assert result.returncode == 0, f"{command!r}\n{result.stdout}\n{result.stderr}"
    return result


@pytest.fixture(scope="module")
def node_stream_project(tmp_path_factory):
    root = tmp_path_factory.mktemp("c-two-node-stream")
    (root / "package.json").write_text('{"type":"module"}', encoding="utf-8")
    package_vars = (
        "C2_TYPESCRIPT_FASTDB_PACKAGE", "C2_TYPESCRIPT_C2_MEM_PACKAGE",
        "C2_TYPESCRIPT_COMPILER_PACKAGE",
    )
    supplied = [os.environ.get(name) for name in package_vars]
    assert not any(supplied) or all(supplied), "provide all three TypeScript package tarballs together"
    if all(supplied):
        archives = [str(Path(path).resolve()) for path in supplied if path]
    else:
        fastdb = REPOSITORY.parent / "fastdb/ts/fastdb4ts"
        dependencies = (
            fastdb,
            REPOSITORY / "core/foundation/c2-mem-ffi/bindings/typescript",
            fastdb / "node_modules/typescript",
        )
        archives = []
        for index, source in enumerate(dependencies):
            assert (source / "package.json").is_file(), f"Node dependency missing: {source}"
            if index < 2:
                assert (source / "dist/index.js").is_file(), f"build Node dependency before this suite: {source}"
            packed = root / "packages" / str(index)
            packed.mkdir(parents=True)
            _checked(["npm", "pack", str(source), "--offline", "--ignore-scripts",
                      "--pack-destination", str(packed)], root)
            files = list(packed.glob("*.tgz"))
            assert len(files) == 1
            archives.append(str(files[0]))
    _checked(["npm", "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", *archives], root)
    artifacts = cc.contract.compile_artifacts(
        cc.contract.export_descriptor(Portable).encode(), target="typescript",
    )
    for artifact in artifacts.artifacts:
        destination = root / "generated" / artifact.relative_path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(artifact.bytes)
    generated = root / "generated/typescript/c_two_contract.ts"
    assert TS_SOURCE.read_text(encoding="utf-8") in generated.read_text(encoding="utf-8"), "rebuild C-Two native codegen from this checkout"
    payload_sources = list((root / "generated/typescript/payloads").glob("*/*.ts"))
    assert len(payload_sources) == 1
    write_node_tsconfig(root)
    node = shutil.which("node")
    assert node is not None
    _checked([node, str(root / "node_modules/typescript/bin/tsc"), "--project", str(root / "tsconfig.json")], root)
    shutil.copyfile(NODE_FIXTURE, root / NODE_FIXTURE.name)
    try:
        yield root, node, payload_sources[0].relative_to(root / "generated").with_suffix(".js")
    finally:
        shutil.rmtree(root)


@pytest.fixture(scope="module")
def rust_stream_harness(tmp_path_factory):
    return RustHarness.prepare(tmp_path_factory.mktemp("c-two-node-rust-stream"))


class NodePythonHost(PythonHost):
    def __init__(self, large: bool, count: int, fail: bool) -> None:
        super().__init__(large)
        self.count = count
        self.fail = fail

    def echo(self, payload):
        inspect(payload, self.large)
        self.calls += 1
        for _ in range(self.count):
            output = build_payload(self.large)
            self.outputs.append(output)
            yield output
        if self.fail:
            raise ValueError("node-python-stream-error")


@pytest.mark.timeout(180)
@pytest.mark.parametrize("host_language,count,fail", [
    ("python", 0, False), ("python", 1, False),
    ("python", 5, False), ("python", 3, True), ("rust", 2, False),
], ids=["python-zero", "python-one", "python-many", "python-error", "rust-many"])
@pytest.mark.parametrize("held", [False, True], ids=["materialized", "held"])
@pytest.mark.parametrize("large", [False, True], ids=["inline", "large"])
def test_generated_node_streams_from_real_hosts(
    node_stream_project, rust_stream_harness, monkeypatch, tmp_path,
    host_language: str, count: int, fail: bool, held: bool, large: bool,
):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    root, node, payload_relative = node_stream_project
    route = f"node-stream-{os.getpid()}-{tmp_path.name}"
    address = f"ipc://c2_node_stream_{os.getpid()}_{tmp_path.name}"
    source = build_payload(large)
    input_path = tmp_path / "input.bin"
    input_path.write_bytes(source.binary_bytes())
    source.close()
    server = rust_host = resource = None
    try:
        if host_language == "python":
            server = cc.Runtime()
            server.set_server(server_id=address.removeprefix("ipc://"))
            resource = NodePythonHost(large, count, fail)
            server.register(Portable, resource, name=route)
            assert server.server_address() == address
        else:
            rust_host = rust_stream_harness.host(address, route, True, large)
        config = tmp_path / "node-config.json"
        config.write_text(json.dumps({
            "address": address, "route": route, "hold": held, "inputPath": str(input_path),
            "count": count, "fail": fail,
            "contractModule": str(root / "dist/typescript/c_two_contract.js"),
            "payloadModule": str(root / "dist" / payload_relative),
        }), encoding="utf-8")
        run = subprocess.run(
            [node, str(root / NODE_FIXTURE.name), str(config)], cwd=root,
            capture_output=True, text=True, timeout=60, check=False,
        )
        assert run.returncode == 0, f"{run.stdout}\n{run.stderr}"
        receipt = json.loads(run.stdout.strip().splitlines()[-1])
        assert receipt == {"count": count, "completeItems": str(count), "held": held, "failed": fail}
        if resource is not None:
            assert resource.calls == 1
        if rust_host is not None:
            rust_host.stop()
            rust_host = None
    finally:
        if resource is not None:
            resource.close()
        if server is not None:
            assert server.shutdown(timeout=5)["completed"]
        if rust_host is not None:
            rust_host.abort()


class ObservedNodePythonHost(NodePythonHost):
    def echo(self, payload):
        self.input_stats = self.observe_input()
        yield from super().echo(payload)


def _node_config(root, payload_relative, tmp_path, address, route, **options):
    source = build_payload(options.pop("large", False))
    try:
        input_path = tmp_path / "input.bin"
        input_path.write_bytes(source.binary_bytes())
    finally:
        source.close()
    config = {
        "address": address, "route": route, "inputPath": str(input_path),
        "contractModule": str(root / "dist/typescript/c_two_contract.js"),
        "payloadModule": str(root / "dist" / payload_relative),
        **options,
    }
    destination = tmp_path / "node-config.json"
    destination.write_text(json.dumps(config), encoding="utf-8")
    return destination, input_path.stat().st_size


def _assert_node_carriers(case, encoded_len, input_stats, server_stats, node_stats, count):
    """Matrix B: observe resource backing; ABI 8 exposes B and slots, not client IPC cells."""
    at_input = input_stats["cells"]
    server = server_stats["server"]["cells"]
    for cells in (at_input, server):
        for kind in ("shm", "file", "reassembly"):
            assert cells[kind]["used_bytes"] == 0, (case.name, kind, cells)
            assert cells[kind]["peak_bytes"] <= cells[kind]["limit_bytes"]
        assert cells["shm"]["limit_bytes"] == case.shm_budget
    if case.large:
        assert encoded_len > CHUNK_SIZE
    for cells in (at_input, server):
        if case.reassembly != "none":
            assert cells["reassembly"]["peak_bytes"] >= encoded_len
        else:
            assert cells["reassembly"]["peak_bytes"] == 0
        if case.reassembly == "file":
            assert cells["file"]["peak_bytes"] >= encoded_len
        else:
            assert cells["file"]["peak_bytes"] == 0
    if case.reassembly == "shm":
        assert at_input["shm"]["peak_bytes"] >= encoded_len
    else:
        assert at_input["shm"]["peak_bytes"] == 0
    if case.reassembly == "shm" or (case.name == "shm" and count > 0):
        assert server["shm"]["peak_bytes"] >= encoded_len
    else:
        assert server["shm"]["peak_bytes"] == 0
    assert int(node_stats["payload_buffer"]["used_bytes"]) == 0
    assert int(node_stats["payload_buffer"]["peak_bytes"]) >= encoded_len
    assert node_stats["used_operations"] == "0"


_NODE_MATRIX_ROWS = list(product(
    [("python", 0, False), ("python", 1, False), ("python", 5, False),
     ("python", 3, True), ("rust", 2, False)],
    [False, True], TRANSPORT_CASES,
))


@pytest.mark.timeout(180)
@pytest.mark.parametrize(
    "host_case,held,transport", _NODE_MATRIX_ROWS,
    ids=[f"{host}-{count}-{'error' if fail else 'ok'}-{'hold' if held else 'materialized'}-{case.name}"
         for (host, count, fail), held, case in _NODE_MATRIX_ROWS],
)
def test_generated_node_stream_transport_matrix(
    node_stream_project, rust_stream_harness, monkeypatch, tmp_path,
    host_case, held, transport,
):
    """Requirement 1: generated clients verify actual carriers and terminals in matrix B."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    root, node, payload_relative = node_stream_project
    ipc_root = tmp_path / "ipc"
    ipc_root.mkdir(mode=0o700)
    host_language, count, fail = host_case
    route = f"node-carrier-{os.getpid()}-{tmp_path.name}"
    address = f"ipc://c2_node_carrier_{os.getpid()}_{tmp_path.name}"
    threshold = 64 * 1024 if transport.prefer_shm else 1 << 30
    overrides = transport.overrides()
    node_options = {"requestShmThreshold": threshold, "requestChunkSize": CHUNK_SIZE}
    if os.name != "nt":
        node_options["ipcRoot"] = str(ipc_root)
    config, encoded_len = _node_config(
        root, payload_relative, tmp_path, address, route, large=transport.large,
        hold=held, count=count, fail=fail, collectStats=True, connectorOptions=node_options,
    )
    server = resource = rust_host = None
    try:
        if host_language == "python":
            server = cc.Runtime()
            if os.name != "nt":
                server.set_local_endpoint(root=str(ipc_root))
            server.set_transport_policy(shm_threshold=threshold)
            server.set_server(server_id=address.removeprefix("ipc://"), ipc_overrides=overrides)
            resource = ObservedNodePythonHost(transport.large, count, fail)
            resource.observe_input = lambda: server.memory_stats()["server"]
            server.register(Portable, resource, name=route)
        else:
            rust_host = rust_stream_harness.host(
                address, route, transport.prefer_shm, transport.large,
                ipc_overrides=overrides, ipc_root=ipc_root,
            )
        run = subprocess.run(
            [node, str(root / NODE_FIXTURE.name), str(config)], cwd=root,
            env=process_environment(overrides, ipc_root), capture_output=True, text=True,
            timeout=60, check=False,
        )
        assert run.returncode == 0, f"{run.stdout}\n{run.stderr}"
        receipt = json.loads(run.stdout.strip().splitlines()[-1])
        assert {key: receipt[key] for key in ("count", "completeItems", "held", "failed")} == {
            "count": count, "completeItems": str(count), "held": held, "failed": fail,
        }
        if resource is not None:
            assert resource.calls == 1
            input_stats, server_stats = resource.input_stats, server.memory_stats()
        else:
            stats = rust_host.stats()
            input_stats, server_stats = stats["input"], stats["memory"]
        _assert_node_carriers(transport, encoded_len, input_stats, server_stats, receipt["stats"], count)
        if rust_host is not None:
            rust_host.stop()
            rust_host = None
    finally:
        if server is not None:
            assert server.shutdown(timeout=5)["completed"]
        if resource is not None:
            resource.close()
        if rust_host is not None:
            rust_host.abort()


class InterleavedNodeResource(PythonHost):
    def __init__(self):
        super().__init__(False)
        self.blocked = [threading.Event(), threading.Event()]
        self.release = threading.Event()
        self.stream_calls = [0, 0]
        self.unary_calls = 0

    def echo(self, payload):
        large = len(payload.binary_bytes()) > CHUNK_SIZE
        inspect(payload, large)
        index = int(large)
        self.stream_calls[index] += 1
        for step in range(index + 3):
            if step == 1:
                self.blocked[index].set()
                assert self.release.wait(30), "test did not release the stream step"
            output = build_payload(large)
            self.outputs.append(output)
            yield output

    def ping(self):
        assert all(event.is_set() for event in self.blocked)
        assert not self.release.is_set()
        self.unary_calls += 1


class ConcurrentUnaryNodeResource(PythonHost):
    def __init__(self):
        super().__init__(False)
        self.blocked = threading.Event()
        self.second_completed = threading.Event()
        self.release = threading.Event()
        self.unary_calls = 0

    def ping(self):
        self.unary_calls += 1
        if self.unary_calls == 1:
            self.blocked.set()
            assert self.release.wait(30), "test did not release the first unary"
        else:
            assert self.blocked.is_set()
            assert not self.release.is_set()
            self.second_completed.set()


@pytest.mark.timeout(90)
@pytest.mark.parametrize("scenario", ["interleave", "unary"])
def test_generated_contract_client_connection_concurrency(
    node_stream_project, monkeypatch, tmp_path, scenario,
):
    """Requirement 2: real ContractClient interleaves two streams/unary or overlaps unary on one connection."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    root, node, payload_relative = node_stream_project
    ipc_root = tmp_path / "ipc"
    ipc_root.mkdir(mode=0o700)
    route = f"node-concurrent-{os.getpid()}-{tmp_path.name}"
    address = f"ipc://c2_node_concurrent_{os.getpid()}_{tmp_path.name}"
    options = {} if os.name == "nt" else {"ipcRoot": str(ipc_root)}
    second_input = tmp_path / "second.bin"
    second = build_payload(True)
    try:
        second_input.write_bytes(second.binary_bytes())
    finally:
        second.close()
    config, _ = _node_config(
        root, payload_relative, tmp_path, address, route, scenario=scenario,
        secondInputPath=str(second_input), connectorOptions=options,
    )
    server = cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(ipc_root))
    server.set_server(server_id=address.removeprefix("ipc://"), ipc_overrides={"max_execution_workers": 4})
    resource = InterleavedNodeResource() if scenario == "interleave" else ConcurrentUnaryNodeResource()
    child = None
    try:
        server.register(
            Portable, resource, name=route,
            concurrency=cc.ConcurrencyConfig(mode="parallel", max_workers=4),
        )
        child = subprocess.Popen(
            [node, str(root / NODE_FIXTURE.name), str(config)], cwd=root,
            env=process_environment(ipc_root=ipc_root), stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        if scenario == "unary":
            assert resource.blocked.wait(30), "first unary did not enter the resource"
            child.stdin.write("first_unary_blocked\n")
            child.stdin.flush()
        expected = "unary_interleaved" if scenario == "interleave" else "second_unary_completed"
        event = json.loads(readline_with_timeout(child.stdout, 30))
        assert event == {"kind": expected}
        if scenario == "interleave":
            assert all(event.is_set() for event in resource.blocked)
            assert resource.stream_calls == [1, 1]
            assert resource.unary_calls == 1
        else:
            assert resource.blocked.is_set() and resource.second_completed.is_set()
            assert resource.unary_calls == 2
        resource.release.set()
        stdout, stderr = child.communicate(timeout=30)
        assert child.returncode == 0, f"{stdout}\n{stderr}"
        receipt = json.loads(stdout.strip().splitlines()[-1])
        assert receipt["scenario"] == scenario
        assert receipt["connections"] == 1
        assert receipt["connectionId"]
    finally:
        resource.release.set()
        if child is not None and child.poll() is None:
            child.kill()
            child.communicate(timeout=10)
        assert server.shutdown(timeout=5)["completed"]
        resource.close()
