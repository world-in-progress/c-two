"""T3: actual generated Node clients retain endpoint and owner identity across IPC streams.

This focused fixture does not build or modify sibling repositories. It consumes
already built packages (or the same three supplied tarballs as the TS matrix),
uses current native codegen, and retains command/Node logs under the platform temporary directory.
"""
from __future__ import annotations

from contextlib import contextmanager
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any, Iterator

import pytest

from tests.fixtures import local_server

import c_two as cc
from tests.fixtures.typescript_project import require_c2_mem_node_build, require_fastdb_node_build, write_node_tsconfig

import c_two.endpoint

import c_two.contract
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
        portable_command(command), cwd=root, text=True, encoding="utf-8", capture_output=True,
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
def persistent_node_project(tmp_path_factory: pytest.TempPathFactory) -> Iterator[tuple[Path, str]]:
    root = tmp_path_factory.mktemp("c2-t3-persistent-")
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
    else:
        fastdb = REPOSITORY.parent / "fastdb/ts/fastdb4ts"
        require_c2_mem_node_build(REPOSITORY)
        require_fastdb_node_build(fastdb)
        dependencies = {
            "fastdb4ts": fastdb,
            "typescript": fastdb / "node_modules/typescript",
            "@c-two/c2-mem-ffi": REPOSITORY / "core/foundation/c2-mem-ffi/bindings/typescript",
        }
        archives = []
        for index, (name, source) in enumerate(dependencies.items()):
            assert (source / "package.json").is_file(), f"prebuilt Node dependency missing: {source}"
            destination = root / "packages" / str(index)
            destination.mkdir(parents=True)
            # Consume the same packaged layout as candidate mode. Do not run
            # prepack/build scripts or modify the already built source trees.
            _run([
                "npm", "pack", str(source), "--offline", "--ignore-scripts",
                "--pack-destination", str(destination),
            ], root, f"pack-{index}")
            packed = list(destination.glob("*.tgz"))
            assert len(packed) == 1, f"expected one prebuilt package archive: {destination}"
            archives.append(str(packed[0]))
    _run([
        "npm", "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", *archives,
    ], root, "install")
    compiler = root / "node_modules/typescript"
    assert json.loads((compiler / "package.json").read_text(encoding="utf-8"))["version"] == "5.9.3"
    tsc = compiler / "bin/tsc"
    assert tsc.is_file(), f"pinned TS compiler missing: {tsc}"
    for name, crm in (("manager", Manager), ("builder", Builder)):
        artifacts = cc.contract.compile_artifacts(
            cc.contract.export_descriptor(crm).encode(), target="typescript",
        )
        for artifact in artifacts.artifacts:
            destination = root / "generated" / name / artifact.relative_path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(artifact.bytes)
        generated = root / "generated" / name / "typescript/c_two_contract.ts"
        assert TS_SOURCE.read_text(encoding="utf-8") in generated.read_text(encoding="utf-8"), "native codegen must be rebuilt from this checkout"
    write_node_tsconfig(root)
    _run([node, str(tsc), "--project", str(root / "tsconfig.json")], root, "compile")
    shutil.copyfile(NODE_FIXTURE, root / NODE_FIXTURE.name)
    yield root, node


class PersistentNode:
    def __init__(
        self, project: tuple[Path, str], address: str, *, ipc_root: str | None = None,
    ) -> None:
        root, node = project
        self.root = root
        config = root / "persistent.json"
        config.write_text(json.dumps({
            "address": address,
            "ipcRoot": ipc_root,
            "managerModule": str(root / "dist/manager/typescript/c_two_contract.js"),
            "builderModule": str(root / "dist/builder/typescript/c_two_contract.js"),
        }), encoding="utf-8")
        self.stderr = (root / "node-stderr.log").open("w", encoding="utf-8")
        self.process = subprocess.Popen(
            [node, str(root / NODE_FIXTURE.name), str(config)], cwd=root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, encoding="utf-8", bufsize=1,
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
    assert result["ok"], json.dumps(result, ensure_ascii=False)
    return result


def _reject(result: dict[str, Any], text: str | None = None) -> None:
    assert not result["ok"], result
    if text:
        assert text in result["error"]["message"], result


def test_generated_node_persistent_dynamic_acquire_and_identity_fences(
    persistent_node_project: tuple[Path, str], unique_ipc_address: str,
) -> None:
    server = local_server(unique_ipc_address)
    builder = CountingResource()
    manager = PublishingManager(server, builder)
    server.register_crm(Manager, manager, name="manager")
    server.start()
    native_identity = dict(server._runtime_session.ensure_server())
    node: PersistentNode | None = None
    try:
        node = PersistentNode(persistent_node_project, unique_ipc_address)
        first = _ok(node.request("call"))
        assert manager.calls == 1 and first["connects"] == 1
        late = _ok(node.request("call", "builder"))
        assert late["connects"] == 1 and builder.calls == 1
        bound = late["observations"][-1]
        assert bound["serverId"] == native_identity["server_id"]
        assert bound["serverInstanceId"] == native_identity["server_instance_id"]
        assert bound["routeName"] == "builder"
        assert first["observations"][-1]["routeName"] == "manager"
        _ok(node.request("prepare", "builder"))

        removed = server.unregister_crm("builder")
        assert removed["local_removed"]
        _reject(node.request("prepare", "builder"))
        _reject(node.request("call", "builder"))
        healthy = _ok(node.request("call"))
        assert manager.calls == 2 and healthy["connects"] == 1
        assert builder.calls == 1

        replacement = CountingResource()
        server.register_crm(Builder, replacement, name="builder")
        _ok(node.request("prepare", "builder"))
        _ok(node.request("call", "builder"))
        assert replacement.calls == 1
        fresh = _ok(node.request("call", "builder", transport="fresh"))
        assert replacement.calls == 2
        assert fresh["observations"][-1]["routeName"] == "builder"
        healthy = _ok(node.request("call"))
        assert healthy["connects"] == 1 and manager.calls == 3

        _ok(node.request("close"))
        _ok(node.request("close", transport="fresh"))
        assert server.shutdown()["completed"]
        server = local_server(unique_ipc_address)
        restarted_manager, restarted_builder = CountingResource(), CountingResource()
        server.register_crm(Manager, restarted_manager, name="manager")
        server.register_crm(Builder, restarted_builder, name="builder")
        server.start()
        new_identity = dict(server._runtime_session.ensure_server())
        assert new_identity["server_id"] == native_identity["server_id"]
        assert new_identity["server_instance_id"] != native_identity["server_instance_id"]
        _reject(node.request("call"), "deactivated")
        _reject(node.request("prepare", "builder", transport="fresh"), "deactivated")
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
    inspection = cc.endpoint.inspect_endpoint(unique_ipc_address)
    assert inspection["status"] in ("absent", "not-applicable"), inspection


def test_generated_node_relay_local_late_route_reuses_physical_connection(
    persistent_node_project, start_c3_relay, monkeypatch, tmp_path,
) -> None:
    """Gates 1/6: late builder acquisition keeps one relay-local IPC carrier.

    The direct counterpart is the existing persistent dynamic-acquire test.
    Native prepare connection IDs count physical connections, independently
    of how often the generated transport invokes its endpoint connector.
    """
    import tempfile

    from tests.fixtures.typescript_idle_gates import node_gate_client

    with tempfile.TemporaryDirectory(
        prefix="c2-n9-", dir="/tmp" if os.name != "nt" else None,
    ) as ipc_root:
        local_root = ipc_root if os.name != "nt" else None
        if local_root is not None:
            monkeypatch.setenv("C2_IPC_ROOT", local_root)
        else:
            monkeypatch.delenv("C2_IPC_ROOT", raising=False)
        monkeypatch.setenv("C2_RELAY_ANCHOR_ADDRESS", "")
        relay = start_c3_relay(idle_timeout=0)
        server = cc.Runtime()
        server.set_local_endpoint(root=local_root)
        server.set_relay_anchor(relay.url)
        builder = CountingResource()

        class LateManager(CountingResource):
            def ping(self) -> None:
                super().ping()
                if self.calls == 1:
                    server.register(Builder, builder, name="builder")

        manager = LateManager()
        try:
            server.register(Manager, manager, name="manager")
            identity = dict(server._runtime_session.ensure_server())
            with node_gate_client(persistent_node_project, tmp_path, relay.url, local_root) as node:
                first = _ok(node())
                late = _ok(node(route="builder"))
                assert manager.calls == builder.calls == 1
                observed = late["observations"]
                assert [item["routeName"] for item in observed] == ["manager", "builder"]
                assert all(item["path"] == "RelayAwareLocalIpc" for item in observed)
                assert all(item["serverId"] == identity["server_id"] for item in observed)
                assert all(item["serverInstanceId"] == identity["server_instance_id"] for item in observed)
                assert [item["routeName"] for item in observed] == ["manager", "builder"]
                assert all(item["method"] == "GET" for item in late["http"]), late
                assert len({item["connectionId"] for item in first["preparations"]}) == 1
                connections = {item["connectionId"] for item in late["preparations"]}
                assert len(connections) == 1, f"late acquisition replaced its physical IPC connection: {late}"
        finally:
            assert server.shutdown(timeout=5)["completed"]
            relay.stop()
            assert not list(Path(ipc_root).glob("*.lease"))
            assert not any(path.is_socket() for path in Path(ipc_root).iterdir())


def test_generated_node_relay_local_rebuilds_after_endpoint_deactivation(
    persistent_node_project, start_c3_relay, monkeypatch, tmp_path,
) -> None:
    """A failed call retires its endpoint; the following call gets a new carrier."""
    import tempfile

    from tests.fixtures.typescript_idle_gates import node_gate_client

    with tempfile.TemporaryDirectory(
        prefix="c2-n9-rebuild-", dir="/tmp" if os.name != "nt" else None,
    ) as ipc_root:
        local_root = ipc_root if os.name != "nt" else None
        if local_root is not None:
            monkeypatch.setenv("C2_IPC_ROOT", local_root)
        else:
            monkeypatch.delenv("C2_IPC_ROOT", raising=False)
        monkeypatch.setenv("C2_RELAY_ANCHOR_ADDRESS", "")
        relay = start_c3_relay(idle_timeout=0)
        server = cc.Runtime()
        server.set_local_endpoint(root=local_root)
        server.set_relay_anchor(relay.url)
        manager = CountingResource()
        try:
            server.register(Manager, manager, name="manager")
            with node_gate_client(persistent_node_project, tmp_path, relay.url, local_root) as node:
                first = _ok(node())
                old_connection = first["preparations"][-1]["connectionId"]
                _ok(node("deactivate-next-call"))
                failed = node()
                _reject(failed)
                assert failed["error"]["code"] == 707, failed
                assert failed["error"]["transportPhase"] == "pre_dispatch", failed
                assert manager.calls == 1
                rebuilt = _ok(node())
                assert manager.calls == 2
                assert rebuilt["preparations"][-1]["connectionId"] != old_connection
                assert all(item["path"] == "RelayAwareLocalIpc" for item in rebuilt["observations"])
                assert all(item["method"] == "GET" for item in rebuilt["http"]), rebuilt
        finally:
            assert server.shutdown(timeout=5)["completed"]
            relay.stop()
            assert not list(Path(ipc_root).glob("*.lease"))
            assert not any(path.is_socket() for path in Path(ipc_root).iterdir())


@pytest.fixture(scope="module")
def idle_node_project(persistent_node_project):
    """Generate the resource's instance evidence as an ordinary portable result."""
    from tests.fixtures.typescript_idle_gates import IdleGate

    root, node = persistent_node_project
    artifacts = cc.contract.compile_artifacts(
        cc.contract.export_descriptor(IdleGate).encode(), target="typescript",
    )
    for artifact in artifacts.artifacts:
        destination = root / "generated/gates" / artifact.relative_path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(artifact.bytes)
    _run([node, str(root / "node_modules/typescript/bin/tsc"), "--project", str(root / "tsconfig.json")], root, "compile-idle-gates")
    return root, node


@pytest.fixture
def node_gate_root(monkeypatch):
    import tempfile

    with tempfile.TemporaryDirectory(prefix="c2-n9b-", dir="/tmp" if os.name != "nt" else None) as root:
        ipc_root = root if os.name != "nt" else None
        if ipc_root is None:
            monkeypatch.delenv("C2_IPC_ROOT", raising=False)
        else:
            monkeypatch.setenv("C2_IPC_ROOT", ipc_root)
        monkeypatch.setenv("C2_ENV_FILE", "")
        monkeypatch.setenv("C2_RELAY_ANCHOR_ADDRESS", "")
        monkeypatch.setenv("C2_IPC_CLIENT_IDLE_TIMEOUT", "0")
        yield ipc_root
        assert not list(Path(root).glob("*.lease"))
        assert not any(path.is_socket() for path in Path(root).iterdir())


def _gate_wait(predicate, message: str):
    import threading
    import time

    deadline = time.monotonic() + 8
    wake = threading.Event()
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        wake.wait(0.005)
    pytest.fail(message)


def _gate_error(result):
    _reject(result)
    error = result["error"]
    return json.loads(error["body"]) if "body" in error else error


def _gate_identity(result, identity, path):
    _ok(result)
    assert result["value"]["server_id"] == identity["server_id"], result
    assert result["value"]["server_instance_id"] == identity["server_instance_id"], result
    observation = result["observations"][-1]
    assert observation["path"] == path, result
    assert "routeUid" not in observation, result
    if path != "RelayAwareHttp":
        assert observation["serverId"] == identity["server_id"], result
        assert observation["serverInstanceId"] == identity["server_instance_id"], result


@pytest.mark.parametrize("mode", ["direct", "relay-local", "http"])
def test_generated_node_closed_route_keeps_dispatched_sibling_and_first_cause(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path, mode,
):
    """Gates 2/4/6: closed lookup is current, A drains, and IPC cause survives.

    Unregistration while B is inside its resource supplies the deterministic
    closed interval. A is already inside its own resource when B closes.
    HTTP uses the relay's current withdrawal authority. Direct IPC also checks
    the generated transport's public name-based prepare separately from call.
    """
    from tests.fixtures.typescript_idle_gates import GateResource, IdleGate, node_gate_client
    from concurrent.futures import ThreadPoolExecutor

    relay = start_c3_relay(idle_timeout=0) if mode != "direct" else None
    server = cc.Runtime()
    server.set_local_endpoint(root=node_gate_root)
    if relay:
        server.set_relay_anchor(relay.url)
    manager, builder = GateResource(), GateResource()
    executor = ThreadPoolExecutor(max_workers=1)
    path = {"direct": "DirectIpc", "relay-local": "RelayAwareLocalIpc", "http": "RelayAwareHttp"}[mode]
    try:
        server.register(IdleGate, manager, name="manager")
        server.register(IdleGate, builder, name="builder")
        identity = dict(server._runtime_session.ensure_server())
        manager.identity = builder.identity = identity
        bridge = server._runtime_session.ensure_server_bridge()
        scheduler = bridge.get_local_slot_info("builder")[1]
        address = server.server_address() if mode == "direct" else relay.url
        with node_gate_client(idle_node_project, tmp_path, address, node_gate_root, mode=mode, portable=True) as node:
            _gate_identity(node(), identity, path)
            ready = node(route="builder")
            _gate_identity(ready, identity, path)
            assert manager.calls == builder.calls == 1
            if mode != "http":
                assert ready["physicalConnections"] == 1, ready
            manager.block = True
            _ok(node("start-call", id="a"))
            assert manager.entered.wait(5), "A did not enter the resource"
            builder.block = True
            _ok(node("start-call", route="builder", id="b"))
            assert builder.entered.wait(5), "B did not enter the resource"
            try:
                closing = executor.submit(bridge.unregister_crm, "builder")
                _gate_wait(lambda: scheduler.snapshot().closed, "route admission did not close")
                acquired = node("prepare", route="builder") if mode == "direct" else None
                if mode == "http":
                    def withdrawn():
                        result = node(route="builder")
                        assert builder.calls == 2, result
                        return result if _gate_error(result)["code"] == 701 else None
                    refused = _gate_wait(withdrawn, "relay did not withdraw its closed route")
                else:
                    refused = node(route="builder")
                assert builder.calls == 2, refused
                assert manager.calls == 2
            finally:
                manager.release.set()
                builder.release.set()
            completed = node("finish-call", id="a")
            _gate_identity(completed, identity, path)
            assert completed["value"]["calls"] == 2
            assert any(item["routeName"] == "manager" for item in completed["observations"]), completed
            assert all("routeUid" not in item for item in completed["observations"]), completed
            drained_b = node("finish-call", id="b")
            _gate_identity(drained_b, identity, path)
            assert drained_b["value"]["calls"] == 2
            cleanup = closing.result(timeout=5)
            assert cleanup["local_removed"] and cleanup["close"]["active_drained"], cleanup
            (tmp_path / "closed-gate-evidence.json").write_text(json.dumps({
                "mode": mode, "identity": identity, "manager_calls": manager.calls,
                "builder_calls": builder.calls, "acquired": acquired,
                "refused": refused, "completed": completed, "cleanup": cleanup,
            }, indent=2), encoding="utf-8")
            if mode == "http":
                assert _gate_error(refused)["name"] == "ResourceNotFound"
                assert refused["physicalConnections"] == 0
            else:
                if mode == "direct":
                    assert acquired is not None
                    error = _gate_error(acquired)
                    call_error = _gate_error(refused)
                    assert call_error["code"] == 707, refused
                    assert call_error["transportPhase"] == "pre_dispatch", refused
                    assert call_error["retrySafe"] is True, refused
                else:
                    assert _gate_error(refused)["name"] == "FallbackDenied", refused
                    error = refused["error"]["cause"]
                    assert all(item["method"] == "GET" for item in refused["http"]), refused
                assert error["code"] == 707, refused
                assert error["transportPhase"] == "pre_dispatch", refused
                assert error["retrySafe"] is True, refused
                assert refused["physicalConnections"] == 1, refused
    finally:
        manager.release.set()
        builder.release.set()
        executor.shutdown(wait=True)
        assert server.shutdown(timeout=5)["completed"]
        manager.close()
        builder.close()
        if relay:
            relay.stop()


_GATE_PATHS = {"direct": "DirectIpc", "relay-local": "RelayAwareLocalIpc", "http": "RelayAwareHttp"}


def _save_gate(work, name, **evidence):
    (work / f"{name}-evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")


@contextmanager
def _gate_host(root, work, relay=None, *, late_builder=False):
    from tests.fixtures.typescript_idle_gates import AcceptedConnections, GateResource, IdleGate

    server = cc.Runtime()
    server.set_local_endpoint(root=root)
    if relay:
        server.set_relay_anchor(relay.url)
    connections = AcceptedConnections(server.connection_events())
    manager, builder = GateResource(), GateResource()
    if late_builder:
        class LateManager(GateResource):
            def ping(self):
                result = super().ping()
                if self.calls == 1:
                    server.register(IdleGate, builder, name="builder")
                return result
        manager = LateManager()
    try:
        server.start()
        for connection_id in connections.snapshot()["active"]:
            connections.wait_closed({connection_id})
        server.register(IdleGate, manager, name="manager")
        if not late_builder:
            server.register(IdleGate, builder, name="builder")
        identity = dict(server._runtime_session.ensure_server())
        manager.identity = builder.identity = identity
        yield server, manager, builder, identity, connections
    finally:
        manager.release.set()
        builder.release.set()
        cleanup = server.shutdown(timeout=5)
        assert cleanup["completed"], cleanup
        manager.close()
        builder.close()
        assert server.call_execution_snapshot()["used_operations"] == 0
        memory = server.memory_stats()
        assert memory["holds"]["active_holds"] == 0
        _save_gate(work, "host-cleanup", shutdown=cleanup, memory=memory, connections=connections.snapshot())
        connections.events.close()


def test_generated_node_http_late_route_reuses_upstream_connection(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path,
):
    """Gate 1 HTTP: persistent manager sees late builder on the same upstream."""
    from tests.fixtures.typescript_idle_gates import node_gate_client

    relay = start_c3_relay(idle_timeout=0)
    try:
        with _gate_host(node_gate_root, tmp_path, relay, late_builder=True) as (server, manager, builder, identity, connections):
            with node_gate_client(idle_node_project, tmp_path, relay.url, node_gate_root, mode="http", portable=True) as node:
                first = node()
                _gate_identity(first, identity, "RelayAwareHttp")
                initial = connections.snapshot()
                late = node(route="builder")
                _gate_identity(late, identity, "RelayAwareHttp")
                final = node()
                _gate_identity(final, identity, "RelayAwareHttp")
                assert manager.calls == 2 and builder.calls == 1
                assert [item["routeName"] for item in final["observations"]] == ["manager", "builder", "manager"]
                assert first["observations"][-1]["routeName"] == final["observations"][-1]["routeName"] == "manager"
                assert late["observations"][-1]["routeName"] == "builder"
                assert final["physicalConnections"] == 0
                assert connections.snapshot()["opened"] == initial["opened"]
                missing = node(route="never-registered")
                assert _gate_error(missing)["code"] == 701, missing
                assert manager.calls == 2 and builder.calls == 1
                assert sum(item["method"] == "POST" for item in missing["http"]) == 3
                _save_gate(tmp_path, "late-http", identity=identity, result=final, absent=missing,
                           connections=connections.snapshot(), manager_calls=manager.calls, builder_calls=builder.calls)
    finally:
        relay.stop()


@pytest.mark.parametrize("mode", ["direct", "relay-local"])
def test_generated_node_background_idle_eviction_and_concurrent_reconnect(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path, monkeypatch, mode,
):
    """Gate 3: an unreferenced cached carrier is evicted without another acquire.

    A refused first acquire leaves no bound proxy, as the documented idle rule
    requires. The persistent generated transport survives. Server events prove
    eviction before eight simultaneous manager calls share one reconnect.
    """
    from tests.fixtures.typescript_idle_gates import node_gate_client

    monkeypatch.setenv("C2_IPC_CLIENT_IDLE_TIMEOUT", "0.05")
    relay = start_c3_relay(idle_timeout=0) if mode != "direct" else None
    try:
        with _gate_host(node_gate_root, tmp_path, relay) as (server, manager, builder, identity, connections):
            scheduler = server._runtime_session.ensure_server_bridge().get_local_slot_info("manager")[1]
            address = server.server_address() if mode == "direct" else relay.url
            baseline = connections.snapshot()
            with node_gate_client(idle_node_project, tmp_path, address, node_gate_root, mode=mode, portable=True) as node:
                if mode == "relay-local":
                    _ok(node("pause", target="resolve"))
                    _ok(node("start-call", route="builder", id="lookup"))
                    _ok(node("wait-paused"))
                removed = server._runtime_session.ensure_server_bridge().unregister_crm("builder")
                assert removed["local_removed"]
                if mode == "relay-local":
                    _ok(node("resume"))
                    refused = node("finish-call", id="lookup")
                    assert _gate_error(refused)["code"] == 714, refused
                    assert refused["error"]["cause"]["code"] == 708, refused
                else:
                    refused = node(route="builder")
                    assert _gate_error(refused)["code"] == 708, refused
                before_idle = connections.snapshot()
                old = set(before_idle["opened"]) - set(baseline["opened"])
                assert len(old) == 1, (baseline, before_idle, refused)
                closed_id = connections.wait_closed(old)
                assert manager.calls == builder.calls == 0
                assert not scheduler.snapshot().closed
                after_idle = connections.snapshot()
                rebuilt = _ok(node("call-many", count=8))
                assert manager.calls == 8 and builder.calls == 0
                assert all(item["value"]["server_id"] == identity["server_id"] for item in rebuilt["outcomes"])
                assert all(item["value"]["server_instance_id"] == identity["server_instance_id"] for item in rebuilt["outcomes"])
                assert {item["path"] for item in rebuilt["observations"]} == {_GATE_PATHS[mode]}
                assert {item["routeName"] for item in rebuilt["observations"]} == {"manager"}
                assert rebuilt["physicalConnections"] == 1, rebuilt
                after_reconnect = connections.snapshot()
                assert len(set(after_reconnect["opened"]) - set(after_idle["opened"])) == 1, after_reconnect
                assert all(item["method"] == "GET" for item in rebuilt["http"]), rebuilt
                _save_gate(tmp_path, "idle", mode=mode, identity=identity, refused=refused,
                           idle_connection=closed_id, idle=after_idle, reconnected=after_reconnect,
                           result=rebuilt, business_calls=manager.calls)
    finally:
        if relay:
            relay.stop()


def test_generated_node_http_relay_idle_eviction_preserves_route(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path,
):
    """Gate 3 HTTP: relay idle eviction preserves registration and route UID."""
    from tests.fixtures.typescript_idle_gates import node_gate_client

    relay = start_c3_relay(idle_timeout=1)
    try:
        with _gate_host(node_gate_root, tmp_path, relay) as (server, manager, builder, identity, connections):
            with node_gate_client(idle_node_project, tmp_path, relay.url, node_gate_root, mode="http", portable=True) as node:
                first = node()
                _gate_identity(first, identity, "RelayAwareHttp")
                initial = connections.snapshot()
                closed = connections.wait_closed(set(initial["active"]))
                idle = connections.snapshot()
                assert manager.calls == 1 and builder.calls == 0
                rebuilt = _ok(node("call-many", count=8))
                assert manager.calls == 9 and builder.calls == 0
                assert rebuilt["physicalConnections"] == 0
                assert {item["routeName"] for item in rebuilt["observations"]} == {first["observations"][-1]["routeName"]}
                assert all(item["path"] == "RelayAwareHttp" for item in rebuilt["observations"])
                assert all(item["value"]["server_instance_id"] == identity["server_instance_id"] for item in rebuilt["outcomes"])
                after = connections.snapshot()
                assert len(set(after["opened"]) - set(idle["opened"])) == 1, (idle, after)
                _save_gate(tmp_path, "http-idle", identity=identity, first=first, closed=closed,
                           idle=idle, reconnected=after, result=rebuilt, business_calls=manager.calls)
    finally:
        relay.stop()


@pytest.mark.parametrize("mode", ["direct", "relay-local", "http"])
@pytest.mark.parametrize("transition", ["removed", "reregistered"])
def test_generated_node_call_time_route_change_does_not_end_sibling(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path, mode, transition,
):
    """Gates 2/4/6: change B after acquire; A and its carrier still finish.

    The test wrapper pauses native dispatch (HTTP: the real business POST)
    after lookup/probe, exercising current registration admission.
    A stale HTTP POST is sent once; the next explicit call resolves again.
    """
    from tests.fixtures.typescript_idle_gates import GateResource, IdleGate, node_gate_client

    relay = start_c3_relay(idle_timeout=0) if mode != "direct" else None
    replacement = GateResource()
    try:
        with _gate_host(node_gate_root, tmp_path, relay) as (server, manager, builder, identity, connections):
            replacement.identity = identity
            address = server.server_address() if mode == "direct" else relay.url
            with node_gate_client(idle_node_project, tmp_path, address, node_gate_root, mode=mode, portable=True) as node:
                _gate_identity(node(), identity, _GATE_PATHS[mode])
                old_b = node(route="builder")
                _gate_identity(old_b, identity, _GATE_PATHS[mode])
                manager.block = True
                _ok(node("start-call", id="a"))
                assert manager.entered.wait(5)
                _ok(node("pause", target="post" if mode == "http" else "dispatch"))
                _ok(node("start-call", route="builder", id="b"))
                acquired = _ok(node("wait-paused"))
                try:
                    assert server._runtime_session.ensure_server_bridge().unregister_crm("builder")["local_removed"]
                    if transition == "reregistered":
                        server.register(IdleGate, replacement, name="builder")
                    _ok(node("resume"))
                    changed = node("finish-call", id="b")
                    if transition == "reregistered" and mode != "http":
                        _gate_identity(changed, identity, _GATE_PATHS[mode])
                        assert changed["value"]["calls"] == 1, changed
                        assert builder.calls == replacement.calls == 1
                    else:
                        error = _gate_error(changed)
                        assert error["code"] == (704 if transition == "reregistered" else 701 if mode == "http" else 708), changed
                        if mode == "http":
                            assert changed["error"]["retrySafe"] is True, changed
                            assert sum(item["method"] == "POST" for item in changed["http"]) == sum(item["method"] == "POST" for item in acquired["http"]), changed
                        else:
                            assert error["transportPhase"] == "pre_dispatch", changed
                            assert changed["physicalConnections"] == 1, changed
                        assert builder.calls == 1 and replacement.calls == 0
                finally:
                    manager.release.set()
                    _ok(node("resume"))
                completed = node("finish-call", id="a")
                _gate_identity(completed, identity, _GATE_PATHS[mode])
                assert completed["value"]["calls"] == 2
                if transition == "reregistered":
                    fresh = node(route="builder", client="main" if mode == "http" else "fresh")
                    _gate_identity(fresh, identity, _GATE_PATHS[mode])
                    assert fresh["observations"][-1]["routeName"] == old_b["observations"][-1]["routeName"] == "builder"
                    assert replacement.calls == (1 if mode == "http" else 2)
                else:
                    absent = node(route="builder", client="fresh")
                    assert _gate_error(absent)["code"] == (701 if mode != "direct" else 708), absent
                    assert builder.calls == 1
                _save_gate(tmp_path, "call-time", mode=mode, transition=transition, identity=identity,
                           changed=changed, completed=completed, connections=connections.snapshot(),
                           old_business_calls=builder.calls, replacement_calls=replacement.calls)
    finally:
        replacement.close()
        if relay:
            relay.stop()


@pytest.mark.parametrize("mode", ["direct", "relay-local", "http"])
def test_generated_node_real_peer_end_is_uncertain_without_replay(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path, mode,
):
    """Gates 3/4/6: real peer end closes the carrier without replaying a call.

    Direct and relay-local break an entered server stream; HTTP breaks an
    entered unary. A completed result remains completed after EOF. A fresh
    acquisition recovers against the restarted resource, before dispatch.
    """
    import uuid
    from tests.fixtures.typescript_idle_gates import GateProcess, node_gate_client

    relay = start_c3_relay(idle_timeout=0) if mode != "direct" else None
    server_id = f"node-cut-{uuid.uuid4().hex}"
    try:
        with GateProcess(tmp_path, node_gate_root, server_id, relay.url if relay else None, "old") as old:
            identity = old.ready["identity"]
            address = old.ready["address"] if mode == "direct" else relay.url
            with node_gate_client(idle_node_project, tmp_path, address, node_gate_root, mode=mode, portable=True) as node:
                _gate_identity(node(), identity, _GATE_PATHS[mode])
                old_b = node(route="builder")
                _gate_identity(old_b, identity, _GATE_PATHS[mode])
                _ok(node("pause", target="http-response" if mode == "http" else "response"))
                _ok(node("start-call", id="settled"))
                settled_ready = _ok(node("wait-paused"))
                if mode == "http":
                    assert settled_ready["http"][-1]["completeBodyBytes"] > 0
                if mode == "http":
                    _ok(old.request("arm"))
                    _ok(node("start-call", id="broken"))
                    _ok(old.request("entered"))
                else:
                    _ok(node("stream-open", id="broken"))
                    first_item = _ok(node("stream-next", id="broken", retain=True))
                    assert not first_item["done"] and first_item["value"]["steps"] == 1
                    assert first_item["value"]["server_instance_id"] == identity["server_instance_id"]
                    _ok(old.request("entered", stream=True))
                    _ok(node("start-next", id="broken"))
                before_end = _ok(node("snapshot"))
                old_stats = old.request()
                old.kill()
                with GateProcess(tmp_path, node_gate_root, server_id, relay.url if relay else None, "new") as new:
                    new_identity = new.ready["identity"]
                    assert new_identity["server_id"] == identity["server_id"]
                    assert new_identity["server_instance_id"] != identity["server_instance_id"]
                    uncertain = node("finish-call", id="broken")
                    error = _gate_error(uncertain)
                    assert uncertain["error"]["retrySafe"] is False, uncertain
                    if mode == "http":
                        assert error["code"] == 702, uncertain
                        assert error["details"]["dispatch_phase"] == "dispatch_uncertain", uncertain
                        assert sum(item["method"] == "POST" for item in uncertain["http"]) == sum(item["method"] == "POST" for item in before_end["http"]), uncertain
                    else:
                        assert error["transportPhase"] == "dispatch_uncertain", uncertain
                        assert len(uncertain["dispatches"]) == len(before_end["dispatches"]), uncertain
                        receipt = _ok(node("stream-close", id="broken"))["receipt"]
                        # EOF carries no CALL_END item count. The one item
                        # actually delivered stays valid independently.
                        assert receipt["completed"] and receipt["completeItems"] is None, receipt
                        assert receipt["error"]["transportPhase"] == "dispatch_uncertain", receipt
                        assert _ok(node("retained-item", id="broken"))["value"] == first_item["value"]
                        _ok(node("release-item", id="broken"))
                    _ok(node("resume"))
                    still_settled = node("finish-call", id="settled")
                    _gate_identity(still_settled, identity, _GATE_PATHS[mode])
                    assert still_settled["value"]["calls"] == 2, still_settled
                    if mode != "http":
                        old_proxy = node()
                        assert _gate_error(old_proxy)["code"] == 710, old_proxy
                        assert old_proxy["error"]["transportPhase"] == "pre_dispatch", old_proxy
                    recovered = node(route="builder", client="fresh")
                    _gate_identity(recovered, new_identity, _GATE_PATHS[mode])
                    assert recovered["value"]["server_instance_id"] != old_b["value"]["server_instance_id"]
                    new_stats = new.request()
                    assert new_stats["resources"]["manager"]["calls"] == 0, new_stats
                    assert new_stats["resources"]["manager"]["stream_calls"] == 0, new_stats
                    assert new_stats["resources"]["builder"]["calls"] == 1, new_stats
                    ledger = [json.loads(line) for line in (tmp_path / "resources.jsonl").read_text(encoding="utf-8").splitlines()]
                    old_manager = [entry for entry in ledger if entry["server_instance_id"] == identity["server_instance_id"] and entry["route"] == "manager"]
                    assert sum(entry["method"] == "ping" for entry in old_manager) == (3 if mode == "http" else 2), ledger
                    assert sum(entry["method"] == "items" for entry in old_manager) == (0 if mode == "http" else 1), ledger
                    _save_gate(tmp_path, "peer-end", mode=mode, old_identity=identity, new_identity=new_identity,
                               old=old_stats, uncertain=uncertain, settled=still_settled, recovered=recovered,
                               new=new_stats, ledger=ledger)
    finally:
        if relay:
            relay.stop()


@pytest.mark.parametrize("mode", ["relay-local", "http"])
def test_generated_node_old_candidate_cannot_discard_restarted_relay_owner(
    idle_node_project, node_gate_root, start_c3_relay, tmp_path, mode,
):
    """Gate 5: interrupt real upstream control, register a new instance, then
    finish an old candidate. Current authority and the new carrier survive.

    HTTP also proves a stale business POST fails once; the next explicit call
    resolves the current UID. Relay recovery has no watch history cursor.
    """
    import uuid
    from tests.fixtures.typescript_idle_gates import GateProcess, node_gate_client

    relay = start_c3_relay(idle_timeout=0)
    server_id = f"node-owner-{uuid.uuid4().hex}"
    try:
        with GateProcess(tmp_path, node_gate_root, server_id, relay.url, "old") as old:
            identity = old.ready["identity"]
            with node_gate_client(idle_node_project, tmp_path, relay.url, node_gate_root, mode=mode, portable=True) as node:
                first = node()
                _gate_identity(first, identity, _GATE_PATHS[mode])
                _ok(node("pause", target="post" if mode == "http" else "dispatch"))
                _ok(node("start-call", id="candidate"))
                _ok(node("wait-paused"))
                assert old.request()["resources"]["manager"]["calls"] == 1
                old.kill()
                with GateProcess(tmp_path, node_gate_root, server_id, relay.url, "new") as new:
                    new_identity = new.ready["identity"]
                    assert new_identity["server_id"] == identity["server_id"]
                    assert new_identity["server_instance_id"] != identity["server_instance_id"]
                    fresh = node(route="builder")
                    _gate_identity(fresh, new_identity, _GATE_PATHS[mode])
                    alive = new.request()
                    _ok(node("resume"))
                    refused = node("finish-call", id="candidate")
                    _reject(refused)
                    if mode == "http":
                        error = _gate_error(refused)
                        assert error["code"] == 704 and refused["error"]["retrySafe"] is True, refused
                        assert sum(item["method"] == "POST" for item in refused["http"]) == sum(item["method"] == "POST" for item in fresh["http"]), refused
                    after_old = new.request()
                    assert after_old["resources"]["manager"]["calls"] == 0, after_old
                    kept = node(route="builder")
                    _gate_identity(kept, new_identity, _GATE_PATHS[mode])
                    assert kept["observations"][-1]["routeName"] == fresh["observations"][-1]["routeName"] == "builder"
                    if mode == "relay-local":
                        assert kept["preparations"][-1]["connectionId"] == fresh["preparations"][-1]["connectionId"]
                    kept_stats = new.request()
                    assert kept_stats["connections"]["opened"] == alive["connections"]["opened"], (alive, kept_stats)
                    assert kept_stats["resources"]["builder"]["calls"] == 2
                    current = node(client="main" if mode == "http" else "fresh")
                    _gate_identity(current, new_identity, _GATE_PATHS[mode])
                    assert current["value"]["server_instance_id"] != first["value"]["server_instance_id"]
                    assert new.request()["resources"]["manager"]["calls"] == 1
                    _save_gate(tmp_path, "restart", mode=mode, old_identity=identity, new_identity=new_identity,
                               refused=refused, fresh=fresh, kept=kept, current=current,
                               before_old_result=alive, after_old_result=after_old, after_second_call=kept_stats)
    finally:
        relay.stop()
