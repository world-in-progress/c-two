"""Generated Node stream clients against real Python and Rust IPC hosts."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

import c_two as cc
from tests.fixtures.stream_matrix import Portable, PythonHost, RustHarness, build_payload, inspect
from tests.fixtures.typescript_project import write_node_tsconfig

REPOSITORY = Path(__file__).resolve().parents[4]
NODE_FIXTURE = REPOSITORY / "sdk/python/tests/fixtures/typescript_server_stream.mjs"
TS_SOURCE = REPOSITORY / "core/foundation/c2-codegen/assets/typescript_transport.ts"


def _checked(command: list[str], root: Path) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=300, check=False)
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
