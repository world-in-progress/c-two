"""Generated ContractClient gates for route conflict and retained mesh ownership."""

from __future__ import annotations

from contextlib import ExitStack, contextmanager
import json
from pathlib import Path
from typing import Iterator

import pytest

import c_two as cc
from tests.fixtures.route_identity_mesh_support import wait_state
from tests.fixtures.typescript_idle_gates import AcceptedConnections, GateResource, IdleGate, node_gate_client
from tests.integration.test_route_identity_mesh import mesh
from tests.integration.test_typescript_persistent_ipc import idle_node_project, persistent_node_project


pytestmark = pytest.mark.timeout(180)


@contextmanager
def _host(root: str | None, relay_url: str, *, registered: bool = True) -> Iterator[tuple[cc.Runtime, GateResource, dict, dict]]:
    runtime = cc.Runtime()
    if root is not None:
        runtime.set_local_endpoint(root=root)
    runtime.set_relay_anchor(relay_url)
    resource = GateResource()
    cleanup = {}
    session = runtime._runtime_session
    connections = AcceptedConnections(runtime.connection_events())
    try:
        runtime.start()
        identity = dict(runtime._runtime_session.ensure_server())
        resource.identity = {**identity, "route": "manager"}
        if registered:
            runtime.register(IdleGate, resource, name="manager")
        yield runtime, resource, identity, cleanup
    finally:
        resource.release.set()
        receipt = runtime.shutdown(timeout=10)
        assert receipt["completed"], receipt
        assert session.call_execution_snapshot()["used_operations"] == 0
        memory = session.memory_stats()
        assert memory["holds"]["active_holds"] == 0
        opened = connections.snapshot()["opened"]
        if opened:
            connections.wait_closed(set(opened))
        cleanup.update(shutdown=receipt, memory=memory, connections=connections.snapshot())
        connections.events.close()
        resource.close()


def _node(project, work: Path, relay, root, mode):
    work.mkdir()
    return node_gate_client(project, work, relay.url, root, mode=mode, portable=True, record_cleanup=True)


def _identity(result: dict, identity: dict, path: str, count: int) -> None:
    assert result["ok"], result
    assert result["value"]["server_id"] == identity["server_id"], result
    assert result["value"]["server_instance_id"] == identity["server_instance_id"], result
    assert result["value"]["calls"] == count, result
    observation = result["observations"][-1]
    assert observation["path"] == path, result
    assert observation["routeName"] == "manager", result
    assert "routeUid" not in observation, result
    if path == "RelayAwareLocalIpc":
        assert observation["serverId"] == identity["server_id"], result
        assert observation["serverInstanceId"] == identity["server_instance_id"], result
        assert observation["connectionId"] in {item["connectionId"] for item in result["preparations"]}, result
    else:
        assert observation["relayUrl"].startswith("http://"), result


def _cleaned(work: Path) -> None:
    receipt = json.loads((work / "node-gates-cleanup.json").read_text(encoding="utf-8"))
    assert receipt["exit_code"] == 0, receipt
    assert receipt["stats"]["used_operations"] == "0", receipt
    assert (receipt["stats"].get("payload_buffer") or {}).get("used_bytes", "0") == "0", receipt
    assert all(word not in json.dumps(receipt) for word in ("routeUid", "route_uid", "revision")), receipt


def _holders(details: dict, expected: set[str], *, status: str | None = None) -> None:
    holders = json.loads(details["registrations"])
    assert {item["relay_id"] for item in holders} == expected, holders
    for holder in holders:
        assert holder["relay_url"].startswith("http://"), holder
        assert isinstance(holder["registered_at"], (int, float)), holder
        assert holder["relay_status"] in {"Alive", "Suspect", "Dead"}, holder
        if status is not None:
            assert holder["relay_status"] == status, holder
        assert "route_uid" not in holder and "routeUid" not in holder, holder


def _refusal(result: dict, code: int, canonical: str, *, conflict: bool = False) -> dict:
    assert not result["ok"], result
    assert "routeUid" not in json.dumps(result) and "route_uid" not in json.dumps(result), result
    error = result["error"]
    body = json.loads(error["body"]) if error.get("body") else error
    assert body["code"] == code, result
    assert body.get("canonicalName", body.get("name")) == canonical, result
    details = body.get("details", {})
    if error.get("body"):
        assert details["dispatch_phase"] == "pre_dispatch", result
    else:
        assert error["transportPhase"] == "pre_dispatch", result
    assert error["retrySafe"] is True, result
    if conflict:
        assert error["routeConflictInstance"] is True, result
        assert error["coreInstance"] is True, result
    elif code == 702:
        assert error["httpRelayInstance"] is True, result
    assert "routeUid" not in details and "route_uid" not in details, result
    return {**error, "details": details}


def _rows(mesh, relay, count: int, owner: str | None = None):
    return wait_state(
        lambda: mesh.routes(relay, "manager"),
        lambda rows: len(rows) == count and (owner is None or all(row["relay_id"] == owner for row in rows)),
    )


@pytest.mark.parametrize("mode,path", [("relay-local", "RelayAwareLocalIpc"), ("http", "RelayAwareHttp")])
def test_generated_node_mesh_conflict_retains_old_targets_and_follows(
    mesh, idle_node_project, tmp_path, mode, path,
):
    """Contracts and routing: "C-Two never chooses between them, and proxies that already hold one keep calling it."

    "The deployer ends the conflict by unregistering one copy, and that copy's proxies then follow the one that remains."
    """
    a = mesh.relay(f"node-g7-a-{mode}")
    b = mesh.relay(f"node-g7-b-{mode}")
    with ExitStack() as stack:
        host_a, resource_a, identity_a, cleanup_a = stack.enter_context(_host(mesh.root, a.url))
        _, resource_b, identity_b, cleanup_b = stack.enter_context(_host(mesh.root, b.url))
        node_a = stack.enter_context(_node(idle_node_project, tmp_path / "node-a", a, mesh.root, mode))
        node_b = stack.enter_context(_node(idle_node_project, tmp_path / "node-b", b, mesh.root, mode))

        first_a, first_b = node_a(), node_b()
        _identity(first_a, identity_a, path, 1)
        _identity(first_b, identity_b, path, 1)
        assert resource_a.calls == resource_b.calls == 1
        mesh.join(a, b)
        for relay in (a, b):
            rows = wait_state(lambda: mesh.routes(relay, "manager"),
                              lambda value: len(value) == 2 and all(row["conflict"] for row in value))
            assert all("route_uid" not in row and "routeUid" not in row for row in rows), rows

        for node in (node_a, node_b):
            failure = _refusal(node(client="fresh"), 719, "RouteConflict", conflict=True)
            _holders(failure["details"], {a.relay_id, b.relay_id})
        assert resource_a.calls == resource_b.calls == 1, "fresh acquisition dispatched a callback"
        during_a, during_b = node_a(), node_b()
        _identity(during_a, identity_a, path, 2)
        _identity(during_b, identity_b, path, 2)
        if mode == "relay-local":
            assert during_a["observations"][-1]["connectionId"] == first_a["observations"][-1]["connectionId"]
            assert during_b["observations"][-1]["connectionId"] == first_b["observations"][-1]["connectionId"]
        else:
            assert during_a["physicalConnections"] == during_b["physicalConnections"] == 0
        assert resource_a.calls == resource_b.calls == 2

        host_a.unregister("manager")
        for relay in (a, b):
            _rows(mesh, relay, 1, b.relay_id)
        # Both authorities must expose the surviving registration before the
        # old proxy is allowed to encounter its old target's refusal.
        refused = node_a()
        _refusal(refused, 701 if mode == "http" else 708,
                 "ResourceNotFound" if mode == "http" else "ResourceRemoved")
        assert resource_a.calls == resource_b.calls == 2, refused
        followed = node_a()
        _identity(followed, identity_b, "RelayAwareHttp", 3)
        assert followed["observations"][-1]["relayUrl"] == b.gate.url, followed
        resolutions = [item for item in followed["http"] if "/_resolve/" in item["url"]]
        assert resolutions[-1]["url"].startswith(f"{a.url}/_resolve/"), followed
        posts = [item for item in followed["http"] if item["method"] == "POST"]
        assert posts[-1]["url"].startswith(f"{b.gate.url}/"), followed
        old_b = node_b()
        _identity(old_b, identity_b, path, 4)
        if mode == "relay-local":
            assert old_b["observations"][-1]["connectionId"] == first_b["observations"][-1]["connectionId"]
            assert followed["physicalConnections"] == 1, followed
        assert resource_a.calls == 2 and resource_b.calls == 4
    _cleaned(tmp_path / "node-a")
    _cleaned(tmp_path / "node-b")
    (tmp_path / "generated-g7-receipt.json").write_text(json.dumps({
        "initial_path": path, "follow_path": followed["observations"][-1]["path"],
        "old_identity": identity_a, "remaining_identity": identity_b,
        "callbacks": {"a": resource_a.calls, "b": resource_b.calls},
        "physical_client_connections": {"a": followed["physicalConnections"], "b": old_b["physicalConnections"]},
        "ipc_reconnects": {"a": followed["physicalConnections"] - first_a["physicalConnections"],
                           "b": old_b["physicalConnections"] - first_b["physicalConnections"]},
        "host_cleanup": {"a": cleanup_a, "b": cleanup_b},
    }, indent=2), encoding="utf-8")


@pytest.mark.parametrize("mode", ["relay-local", "http"])
@pytest.mark.parametrize("release", ["owner-withdraws", "force-leave"])
def test_generated_node_mesh_unreachable_owner_retains_name_until_release(
    mesh, idle_node_project, tmp_path, mode, release,
):
    """Contracts and routing: "A relay that is Suspect or Dead keeps its registrations: they do not resolve".

    "the name cannot be registered elsewhere until that relay withdraws it after it comes back or a deployer force-leaves it"
    """
    a = mesh.relay(f"node-g8-a-{mode}-{release}")
    b = mesh.relay(f"node-g8-b-{mode}-{release}", seeds=[a.gate.url])
    mesh.join(a, b)
    with ExitStack() as stack:
        host_a, resource_a, identity_a, cleanup_a = stack.enter_context(_host(mesh.root, a.url))
        host_b, resource_b, identity_b, cleanup_b = stack.enter_context(_host(mesh.root, b.url, registered=False))
        node_b = stack.enter_context(_node(idle_node_project, tmp_path / "node-b", b, mesh.root, mode))
        _rows(mesh, b, 1, a.relay_id)
        a.gate.block()
        b.gate.block()

        for status in ("Suspect", "Dead"):
            wait_state(lambda: mesh.peer(b, a.relay_id),
                       lambda peer: peer is not None and peer["status"] == status, timeout=55)
            rows = _rows(mesh, b, 1, a.relay_id)
            assert rows[0]["relay_status"] == status and rows[0]["conflict"] is False, rows
            failure = _refusal(node_b(client=f"fresh-{status}"), 702, "ResourceUnavailable")
            _holders(failure["details"], {a.relay_id}, status=status)
            assert resource_a.calls == resource_b.calls == 0, failure
            with pytest.raises(cc.error.ResourceAlreadyRegistered) as caught:
                host_b.register(IdleGate, resource_b, name="manager")
            assert int(caught.value.code) == 703
            _holders(dict(caught.value.details), {a.relay_id}, status=status)
            assert resource_a.calls == resource_b.calls == 0

        if release == "owner-withdraws":
            a.gate.restore()
            b.gate.restore()
            mesh.join(a, b)
            wait_state(lambda: mesh.peer(b, a.relay_id),
                       lambda peer: peer is not None and peer["status"] == "Alive", timeout=55)
            host_a.unregister("manager")
            _rows(mesh, b, 0)
        else:
            response = mesh.http.post(f"{b.url}/_admin/force-leave", json={"relay_id": a.relay_id})
            assert response.status_code == 200, response.text
            _rows(mesh, b, 0)

        host_b.register(IdleGate, resource_b, name="manager")
        if release == "force-leave":
            # New registration is admitted before A withdraws; afterwards
            # restore the advertised data-plane URL for the successful HTTP
            # call without letting A rejoin with a conflicting registration.
            assert len(mesh.routes(a, "manager")) == 1
            host_a.unregister("manager")
            a.gate.restore()
            b.gate.restore()
            mesh.join(a, b)
            _rows(mesh, a, 1, b.relay_id)
            _rows(mesh, b, 1, b.relay_id)
        _rows(mesh, b, 1, b.relay_id)
        success = node_b(client="after-release")
        _identity(success, identity_b,
                  "RelayAwareLocalIpc" if mode == "relay-local" else "RelayAwareHttp", 1)
        assert resource_a.calls == 0 and resource_b.calls == 1

    _cleaned(tmp_path / "node-b")
    (tmp_path / "generated-g8-receipt.json").write_text(json.dumps({
        "release": release, "requested_mode": mode, "failed_dispatch_path": None,
        "success_path": success["observations"][-1]["path"],
        "old_identity": identity_a, "new_identity": identity_b,
        "callbacks": {"a": resource_a.calls, "b": resource_b.calls},
        "physical_client_connections": success["physicalConnections"], "ipc_reconnects": 0,
        "host_cleanup": {"a": cleanup_a, "b": cleanup_b},
    }, indent=2), encoding="utf-8")
