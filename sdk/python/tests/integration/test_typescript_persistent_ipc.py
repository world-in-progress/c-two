"""T3: actual generated Node clients retain route/owner bindings across IPC streams.

This focused fixture does not build or modify sibling repositories. It consumes
already built packages (or the same three supplied tarballs as the TS matrix),
uses current native codegen, and retains command/Node logs under /tmp.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from typing import Any, Iterator

import pytest

import c_two as cc
from c_two.transport.server import Server
from tests.fixtures.process_control import portable_command, readline_with_timeout


REPOSITORY = Path(__file__).resolve().parents[4]
NODE_FIXTURE = REPOSITORY / "sdk/python/tests/fixtures/typescript_persistent_ipc.mjs"
TS_SOURCE = REPOSITORY / "core/foundation/c2-codegen/assets/typescript_transport.ts"
pytestmark = pytest.mark.timeout(90)


@cc.crm(namespace="test.typescript-persistent.manager", version="0.1.0")
class Manager:
    def ping(self) -> None:
        ...


@cc.crm(namespace="test.typescript-persistent.builder", version="0.1.0")
class Builder:
    def ping(self) -> None:
        ...


class CountingResource:
    def __init__(self) -> None:
        self.calls = 0

    def ping(self) -> None:
        self.calls += 1


class PublishingManager(CountingResource):
    def __init__(self, server: Server, builder: CountingResource) -> None:
        super().__init__()
        self.server = server
        self.builder = builder

    def ping(self) -> None:
        super().ping()
        if self.calls == 1:
            self.server.register_crm(Builder, self.builder, name="builder")


def _run(command: list[str], root: Path, label: str) -> None:
    completed = subprocess.run(
        portable_command(command), cwd=root, text=True, capture_output=True,
        timeout=120, check=False,
    )
    log = root / f"{label}.log"
    log.write_text(
        f"command={command!r}\nexit={completed.returncode}\n"
        f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        encoding="utf-8",
    )
    assert completed.returncode == 0, f"{label} failed: {log}\n{completed.stderr}"


@pytest.fixture(scope="module")
def persistent_node_project() -> Iterator[tuple[Path, str]]:
    log_root = tempfile.gettempdir() if os.name == "nt" else "/tmp"
    root = Path(tempfile.mkdtemp(prefix="c2-t3-persistent-", dir=log_root))
    node = shutil.which("node")
    assert node is not None, "real persistent IPC regression requires Node"
    (root / "package.json").write_text('{"type":"module"}', encoding="utf-8")
    package_vars = (
        "C2_TYPESCRIPT_FASTDB_PACKAGE", "C2_TYPESCRIPT_C2_MEM_PACKAGE",
        "C2_TYPESCRIPT_COMPILER_PACKAGE",
    )
    packages = [os.environ.get(name) for name in package_vars]
    assert not any(packages) or all(packages), "provide all three matrix package tarballs together"
    if all(packages):
        archives = [str(Path(package).resolve()) for package in packages if package]
        assert all(Path(archive).is_file() for archive in archives), "supplied TS package tarball missing"
        _run([
            "npm", "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", *archives,
        ], root, "install")
    else:
        fastdb = REPOSITORY.parent / "fastdb/ts/fastdb4ts"
        dependencies = {
            "fastdb4ts": fastdb,
            "typescript": fastdb / "node_modules/typescript",
            "@c-two/c2-mem-ffi": REPOSITORY / "core/foundation/c2-mem-ffi/bindings/typescript",
        }
        for name, source in dependencies.items():
            assert (source / "package.json").is_file(), f"prebuilt Node dependency missing: {source}"
            destination = root / "node_modules" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.symlink_to(source, target_is_directory=True)
    compiler = root / "node_modules/typescript"
    assert json.loads((compiler / "package.json").read_text())["version"] == "5.9.3"
    tsc = compiler / "bin/tsc"
    assert tsc.is_file(), f"pinned TS compiler missing: {tsc}"
    for name, crm in (("manager", Manager), ("builder", Builder)):
        artifacts = cc.compile_contract_artifacts(
            cc.export_contract_descriptor(crm).encode(), target="typescript",
        )
        for artifact in artifacts.artifacts:
            destination = root / "generated" / name / artifact.relative_path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(artifact.bytes)
        generated = root / "generated" / name / "typescript/c_two_contract.ts"
        assert TS_SOURCE.read_text() in generated.read_text(), "native codegen must be rebuilt from this checkout"
    (root / "tsconfig.json").write_text(json.dumps({
        "compilerOptions": {
            "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext",
            "lib": ["ES2022", "DOM"], "strict": True, "skipLibCheck": False,
            "rootDir": str(root / "generated"), "outDir": str(root / "dist"),
        },
        "include": [str(root / "generated/**/*.ts")],
    }), encoding="utf-8")
    _run([node, str(tsc), "--project", str(root / "tsconfig.json")], root, "compile")
    shutil.copyfile(NODE_FIXTURE, root / NODE_FIXTURE.name)
    yield root, node


class PersistentNode:
    def __init__(self, project: tuple[Path, str], address: str) -> None:
        root, node = project
        self.root = root
        config = root / "persistent.json"
        config.write_text(json.dumps({
            "address": address,
            "managerModule": str(root / "dist/manager/typescript/c_two_contract.js"),
            "builderModule": str(root / "dist/builder/typescript/c_two_contract.js"),
        }), encoding="utf-8")
        self.stderr = (root / "node-stderr.log").open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [node, str(root / NODE_FIXTURE.name), str(config)], cwd=root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, bufsize=1,
        )
        assert self.process.stdout is not None
        try:
            ready = readline_with_timeout(self.process.stdout, 20)
            assert json.loads(ready) == {"ready": True}, f"Node startup failed: {root}/node-stderr.log"
        except BaseException:
            self.stop()
            raise

    def request(self, op: str, route: str = "manager", **kwargs: Any) -> dict[str, Any]:
        assert self.process.stdin is not None and self.process.stdout is not None
        command = {"op": op, "route": route, **kwargs}
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()
        line = readline_with_timeout(self.process.stdout, 20)
        with (self.root / "node-commands.log").open("a", encoding="utf-8") as log:
            log.write(json.dumps(command) + "\n" + line)
        assert line, f"Node exited: {self.root}/node-stderr.log"
        return json.loads(line)

    def stop(self) -> None:
        primary_error = sys.exc_info()[1]
        try:
            try:
                if self.process.poll() is None:
                    try:
                        acknowledgement = self.request("exit")
                        assert acknowledgement == {"ok": True}, acknowledgement
                        assert self.process.stdin is not None
                        self.process.stdin.close()
                        self.process.wait(timeout=10)
                    except (BrokenPipeError, TimeoutError, subprocess.TimeoutExpired):
                        self.process.kill()
                        self.process.wait(timeout=10)
                assert self.process.returncode == 0, f"Node cleanup failed: {self.root}/node-stderr.log"
            finally:
                self.stderr.close()
                if self.process.stdin is not None:
                    self.process.stdin.close()
                if self.process.stdout is not None:
                    self.process.stdout.close()
        except Exception as cleanup_error:
            if primary_error is None:
                raise
            print(
                f"Node cleanup also failed while preserving the original exception: {cleanup_error}",
                file=sys.stderr,
            )


def _ok(result: dict[str, Any]) -> dict[str, Any]:
    assert result["ok"], result
    return result


def _reject(result: dict[str, Any], text: str | None = None) -> None:
    assert not result["ok"], result
    if text:
        assert text in result["error"]["message"], result


def test_generated_node_persistent_dynamic_acquire_and_identity_fences(
    persistent_node_project: tuple[Path, str], unique_ipc_address: str,
) -> None:
    server = Server(bind_address=unique_ipc_address)
    builder = CountingResource()
    manager = PublishingManager(server, builder)
    server.register_crm(Manager, manager, name="manager")
    server.start()
    native_identity = dict(server._runtime_session.ensure_server())
    node: PersistentNode | None = None
    try:
        node = PersistentNode(persistent_node_project, unique_ipc_address)
        first = _ok(node.request("call"))
        assert manager.calls == 1
        assert first["connects"] == 1
        late = _ok(node.request("call", "builder"))
        assert late["connects"] == 1 and builder.calls == 1
        bound = late["observations"][-1]
        assert bound["serverId"] == native_identity["server_id"]
        assert bound["serverInstanceId"] == native_identity["server_instance_id"]
        assert bound["routeUid"] != first["observations"][-1]["routeUid"]

        uncertain = node.request("call", "builder", uncertain=True)
        _reject(uncertain, "actual business reply header")
        assert builder.calls == 2 and uncertain["businessWrites"] == 3
        resumed = _ok(node.request("call", "builder"))
        assert builder.calls == 3 and resumed["connects"] == 2
        assert resumed["observations"][-1]["routeUid"] == bound["routeUid"]
        assert resumed["observations"][-1]["routeRevision"] == bound["routeRevision"]
        _ok(node.request("prepare", "builder"))

        removed = server.unregister_crm("builder")
        assert removed["local_removed"]
        _reject(node.request("prepare", "builder"))
        _reject(node.request("call", "builder"))
        healthy = _ok(node.request("call"))
        assert healthy["connects"] == 2 and manager.calls == 2
        assert builder.calls == 3

        replacement = CountingResource()
        server.register_crm(Builder, replacement, name="builder")
        _reject(node.request("prepare", "builder"), "route token mismatch")
        _reject(node.request("call", "builder"), "route token mismatch")
        assert replacement.calls == 0
        fresh = _ok(node.request("call", "builder", transport="fresh"))
        assert replacement.calls == 1
        assert fresh["observations"][-1]["routeUid"] != bound["routeUid"]
        _ok(node.request("close"))
        _reject(node.request("call", "builder"), "route token mismatch")
        assert replacement.calls == 1
        healthy = _ok(node.request("call"))
        assert healthy["connects"] == 3 and manager.calls == 3

        _ok(node.request("close"))
        _ok(node.request("close", transport="fresh"))
        assert server.shutdown()["completed"]
        server = Server(bind_address=unique_ipc_address)
        restarted_manager, restarted_builder = CountingResource(), CountingResource()
        server.register_crm(Manager, restarted_manager, name="manager")
        server.register_crm(Builder, restarted_builder, name="builder")
        server.start()
        new_identity = dict(server._runtime_session.ensure_server())
        assert new_identity["server_id"] == native_identity["server_id"]
        assert new_identity["server_instance_id"] != native_identity["server_instance_id"]
        _reject(node.request("call"), "server identity mismatch")
        _reject(node.request("prepare", "builder", transport="fresh"), "server identity mismatch")
        assert restarted_manager.calls == 0 and restarted_builder.calls == 0
        restarted = _ok(node.request("call", "builder", transport="restarted"))
        assert restarted_builder.calls == 1
        assert restarted["observations"][-1]["serverInstanceId"] == new_identity["server_instance_id"]
    finally:
        try:
            if node is not None:
                node.stop()
        finally:
            assert server.shutdown()["completed"]
    inspection = cc.inspect_endpoint(unique_ipc_address)
    assert inspection["status"] in ("absent", "not-applicable"), inspection
