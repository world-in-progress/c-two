"""Real c3/Python coverage for ADR 0004's mesh route-name authority."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.parse
import uuid

import httpx
import pytest

import c_two as cc
import c_two.endpoint
from c_two._native import resolve_server_ipc_config
from c_two.crm.contract import crm_contract
from tests.fixtures.persistent_lifecycle_support import Lifecycle
from tests.fixtures.route_identity_host import Host
from tests.fixtures.route_identity_mesh_support import GatedRelay, wait_state
from tests.conftest import c3_binary


pytestmark = pytest.mark.timeout(180)


@pytest.fixture
def mesh(monkeypatch, request, tmp_path):
    """Keep logs in pytest's temporary path and Unix IPC paths short."""
    run = tmp_path
    root = None if os.name == "nt" else tempfile.mkdtemp(prefix="c2-mesh-", dir="/tmp")
    monkeypatch.setenv("C2_ENV_FILE", "")
    monkeypatch.setenv("C2_RELAY_ANCHOR_ADDRESS", "")
    if root is not None:
        monkeypatch.setenv("C2_IPC_ROOT", root)
    else:
        monkeypatch.delenv("C2_IPC_ROOT", raising=False)
    relays = []
    hosts = []
    clients = []
    proxies = []

    class Scenario:
        def __init__(self):
            self.root = root
            self.run = run
            self.http = httpx.Client(trust_env=False, timeout=4)

        def relay(self, relay_id: str, seeds: list[str] = ()) -> GatedRelay:
            relay = GatedRelay(relay_id, run, seeds)
            relays.append(relay)
            return relay

        def host(self, name: str, relay: GatedRelay, label: str) -> Host:
            host = Host(name, f"{label}-{uuid.uuid4().hex}", relay.url, root,
                        run / f"{label}.calls")
            hosts.append(host)
            return host

        def client(self, relay: GatedRelay) -> cc.Runtime:
            client = cc.Runtime()
            if root is not None:
                client.set_local_endpoint(root=root)
            client.set_relay_anchor(relay.url)
            clients.append(client)
            return client

        def track(self, proxy):
            proxies.append(proxy)
            return proxy

        def routes(self, relay: GatedRelay, name: str) -> list[dict]:
            response = self.http.get(f"{relay.url}/_routes")
            response.raise_for_status()
            return [row for row in response.json()["routes"] if row["name"] == name]

        def peer(self, relay: GatedRelay, peer_id: str) -> dict | None:
            response = self.http.get(f"{relay.url}/_peers")
            response.raise_for_status()
            return next((row for row in response.json() if row["relay_id"] == peer_id), None)

        def resolve(self, relay: GatedRelay, name: str) -> httpx.Response:
            contract = crm_contract(Lifecycle)
            query = urllib.parse.urlencode({key: getattr(contract, key) for key in (
                "crm_ns", "crm_name", "crm_ver", "abi_hash", "signature_hash",
            )})
            return self.http.get(f"{relay.url}/_resolve/{urllib.parse.quote(name, safe='')}?{query}")

        def registry_text(self, relay: GatedRelay) -> str:
            result = subprocess.run(
                [str(c3_binary()), "registry", "list-routes", "--relay", relay.url],
                capture_output=True, text=True, timeout=10,
                env={**os.environ, "C2_ENV_FILE": "", "C2_RELAY_USE_PROXY": "0"},
                check=True,
            )
            return result.stdout

        def register_http(self, relay: GatedRelay, host: Host, name: str) -> httpx.Response:
            contract = crm_contract(Lifecycle)
            body = {"name": name, "server_id": host.identity["server_id"],
                    "server_instance_id": host.identity["server_instance_id"],
                    "address": host.identity["ipc_address"],
                    "max_payload_size": int(resolve_server_ipc_config(None, None)["max_payload_size"]),
                    **{key: getattr(contract, key) for key in (
                        "crm_ns", "crm_name", "crm_ver", "abi_hash", "signature_hash",
                    )}}
            namespace = cc.endpoint.local_endpoint_context(root=root).namespace_id
            return self.http.post(f"{relay.url}/_register", json=body,
                                  headers={"x-c2-local-namespace": namespace})

        def join(self, left: GatedRelay, right: GatedRelay) -> None:
            for sender, recipient in ((left, right), (right, left)):
                envelope = {"protocol_version": 7, "sender_relay_id": sender.relay_id,
                            "message": {"type": "RelayJoin", "relay_id": sender.relay_id,
                                        "url": sender.gate.url}}
                response = self.http.post(f"{recipient.gate.url}/_peer/join", json=envelope)
                assert response.status_code == 200, response.text
            wait_state(lambda: self.peer(left, right.relay_id),
                       lambda peer: peer is not None and peer["status"] == "Alive")
            wait_state(lambda: self.peer(right, left.relay_id),
                       lambda peer: peer is not None and peer["status"] == "Alive")
            # A manually submitted Join does not let either relay consume the
            # returned full snapshot. Drive the normal digest/diff exchange so
            # two formerly independent local registrations converge promptly.
            for source, target in ((left, right), (right, left)):
                digest = {"protocol_version": 7, "sender_relay_id": target.relay_id,
                          "message": {"type": "DigestExchange", "digest": []}}
                response = self.http.post(f"{source.gate.url}/_peer/digest", json=digest)
                assert response.status_code == 200, response.text
                diff = response.json()
                assert diff["message"]["type"] == "DigestDiff"
                applied = self.http.post(f"{target.gate.url}/_peer/digest", json=diff)
                assert applied.status_code == 200, applied.text

    scenario = Scenario()
    try:
        yield scenario
    finally:
        cleanup_errors = []
        for proxy in proxies:
            try:
                cc.close(proxy)
            except Exception as error:
                cleanup_errors.append(error)
        for client in clients:
            try:
                assert client.shutdown(timeout=10)["completed"]
            except Exception as error:
                cleanup_errors.append(error)
        for host in reversed(hosts):
            try:
                host.close()
            except Exception as error:
                cleanup_errors.append(error)
        for relay in reversed(relays):
            try:
                relay.close()
            except Exception as error:
                cleanup_errors.append(error)
        scenario.http.close()
        assert not cleanup_errors, cleanup_errors
        if root is not None:
            assert not list(Path(root).glob("*.lease"))
            assert not any(path.is_socket() for path in Path(root).iterdir())
            shutil.rmtree(root)
        new_variant = (request.node.name.endswith(("[relay_aware_local_ipc]", "[force_leave]"))
                       or request.node.path.name == "test_typescript_route_identity_mesh.py")
        if new_variant:
            request.node.user_properties.append(("route_identity_mesh_cleanup", json.dumps({
                "host_process_count": len(hosts), "relay_process_count": len(relays),
                "hosts_closed": all(host.process.poll() is not None for host in hosts),
                "relays_closed": all(relay.process.poll() is not None for relay in relays),
                "ipc_root_removed": root is None or not Path(root).exists(),
                "errors": [repr(error) for error in cleanup_errors],
            })))
        assert not cleanup_errors, cleanup_errors


def _registered(host: Host) -> None:
    assert host.command("register") == {"registered": True}, host.stderr()


def _holders(details: dict, expected_ids: set[str]) -> list[dict]:
    registrations = json.loads(details["registrations"])
    assert {row["relay_id"] for row in registrations} == expected_ids
    for row in registrations:
        assert row["relay_url"].startswith("http://")
        assert isinstance(row["registered_at"], (int, float))
        assert row["relay_status"] in {"Alive", "Suspect", "Dead"}
    return registrations


def _error(response: httpx.Response, status: int, code: int, name: str, route: str) -> dict:
    assert response.status_code == status, response.text
    body = response.json()
    assert body["code"] == code
    assert body["name"] == name
    assert body["details"]["route"] == route
    assert body["details"]["dispatch_phase"] == "pre_dispatch"
    return body["details"]


def test_peer_registration_rejects_second_host(mesh):
    name = f"unique-{uuid.uuid4().hex}"
    a = mesh.relay("unique-a")
    b = mesh.relay("unique-b", seeds=[a.gate.url])
    mesh.join(a, b)
    host_a = mesh.host(name, a, "unique-a")
    host_b = mesh.host(name, b, "unique-b")
    _registered(host_a)
    wait_state(lambda: mesh.routes(b, name), lambda rows: len(rows) == 1)
    rejected = host_b.command("register")
    assert rejected["code"] == 703
    assert rejected["error"] == "ResourceAlreadyRegistered"
    assert rejected["details"], "canonical relay registration details must survive the Core receipt"
    _holders(rejected["details"], {a.relay_id})
    assert rejected["message"] == f"Route name already registered with relay: {name!r}"
    assert rejected["native"]["status_code"] == 409
    assert rejected["native"]["failure_source"] == "relay_prepare"
    rollback = rejected["native"]["rollback"]
    assert rollback["closed_reason"] == "registration_rollback"
    assert rollback["active_drained"] is True
    assert rollback["local_removed"] is False
    raw = mesh.register_http(b, host_b, name)
    assert raw.status_code == 409, raw.text
    assert raw.json()["code"] == 703
    (mesh.run / "registration-diagnostics.json").write_text(
        json.dumps({"sdk_exception": rejected, "http_response": raw.json()}, indent=2), encoding="utf-8",
    )
    _holders(raw.json()["details"], {a.relay_id})
    assert rejected["details"] == raw.json()["details"]
    assert rejected["native"]["error_message"] == raw.text
    assert all(row["relay_id"] != b.relay_id for row in mesh.routes(b, name))


@pytest.mark.parametrize("release", ["owner_withdrawal", "force_leave"])
def test_unreachable_owner_keeps_name_until_host_withdraws(mesh, release, record_property):
    name = f"unreachable-{uuid.uuid4().hex}"
    a = mesh.relay("unreachable-a")
    b = mesh.relay("unreachable-b", seeds=[a.gate.url])
    mesh.join(a, b)
    host_a = mesh.host(name, a, "unreachable-a")
    host_b = mesh.host(name, b, "unreachable-b")
    client = mesh.client(b)
    acquisition_failures = []
    _registered(host_a)
    wait_state(lambda: mesh.routes(b, name), lambda rows: len(rows) == 1)
    a.gate.block()
    b.gate.block()
    for status in ("Suspect", "Dead"):
        wait_state(lambda: mesh.peer(b, a.relay_id),
                   lambda peer: peer is not None and peer["status"] == status, timeout=55)
        rows = mesh.routes(b, name)
        assert len(rows) == 1 and rows[0]["relay_status"] == status
        assert rows[0]["conflict"] is False
        assert f"{name} ({a.relay_id}, {a.gate.url})" in mesh.registry_text(b)
        assert f" - {status}\n" in mesh.registry_text(b)
        rejected = host_b.command("register")
        assert rejected["code"] == 703
        assert rejected["error"] == "ResourceAlreadyRegistered"
        assert _holders(rejected["details"], {a.relay_id})[0]["relay_status"] == status
        raw = mesh.register_http(b, host_b, name)
        assert raw.status_code == 409, raw.text
        assert raw.json()["code"] == 703
        assert _holders(raw.json()["details"], {a.relay_id})[0]["relay_status"] == status
        details = _error(mesh.resolve(b, name), 503, 702, "ResourceUnavailable", name)
        assert _holders(details, {a.relay_id})[0]["relay_status"] == status
        # ADR 0004 note: "名字只归失联的 relay 时，解析报 `ResourceUnavailable`（702），在别处注册报
        # `ResourceAlreadyRegistered`（703）". Reference: "A relay that is Suspect or Dead
        # keeps its registrations: they do not resolve".
        for address in (None, b.url):
            with pytest.raises(cc.error.ResourceUnavailable) as unavailable:
                client.connect(Lifecycle, name=name, address=address, timeout=5)
            assert unavailable.value.code == 702
            assert unavailable.value.details["dispatch_phase"] == "pre_dispatch"
            assert _holders(unavailable.value.details, {a.relay_id})[0]["relay_status"] == status
            acquisition_failures.append({
                "state": status, "requested_mode": "relay_aware" if address is None else "explicit_http",
                "actual_path": None, "error": type(unavailable.value).__name__,
            })
    if release == "owner_withdrawal":
        a.gate.restore()
        b.gate.restore()
        wait_state(lambda: mesh.peer(b, a.relay_id),
                   lambda peer: peer is not None and peer["status"] == "Alive", timeout=55)
        assert host_a.command("unregister")["unregistered"]
    else:
        response = mesh.http.post(f"{b.url}/_admin/force-leave", json={"relay_id": a.relay_id})
        assert response.status_code == 200, response.text
    wait_state(lambda: mesh.routes(b, name), lambda rows: not rows)
    _registered(host_b)
    if release == "force_leave":
        # The registration succeeded while A still had its original route:
        # force-leave, rather than A's withdrawal, freed B's authority.
        assert len(mesh.routes(a, name)) == 1
        assert host_a.command("unregister")["unregistered"]
        a.gate.restore()
        b.gate.restore()
        mesh.join(a, b)
        for relay in (a, b):
            wait_state(lambda: mesh.routes(relay, name),
                       lambda rows: len(rows) == 1 and rows[0]["relay_id"] == b.relay_id)
    wait_state(lambda: mesh.routes(b, name),
               lambda rows: len(rows) == 1 and rows[0]["relay_id"] == b.relay_id)
    paths = []
    for address, expected_path in ((None, "relay_aware_local_ipc"),
                                   (b.url, "explicit_relay")):
        proxy = mesh.track(client.connect(Lifecycle, name=name, address=address, timeout=5))
        assert proxy.client.observed_path == expected_path
        reply = proxy.call(expected_path)
        assert reply["resource"] == host_b.identity["server_id"]
        assert reply["server_instance_id"] == host_b.identity["server_instance_id"]
        paths.append(proxy.client.observed_path)
    assert host_a.records() == []
    assert [row["method"] for row in host_b.records()] == ["call", "call"]
    if release == "force_leave":
        record_property("route_identity_g8", json.dumps({
            "release": release, "holder": host_a.identity, "new_owner": host_b.identity,
            "failed_acquisitions": acquisition_failures,
            "paths": paths, "callbacks": host_b.records(),
        }))


@pytest.mark.parametrize("path", ["relay_aware_local_ipc", "explicit_relay"])
def test_independent_registrations_join_conflict_and_existing_proxies_follow(
    mesh, path, record_property,
):
    name = f"conflict-{uuid.uuid4().hex}"
    a = mesh.relay("conflict-a")
    b = mesh.relay("conflict-b")
    host_a = mesh.host(name, a, "conflict-a")
    host_b = mesh.host(name, b, "conflict-b")
    _registered(host_a)
    _registered(host_b)
    client_a = mesh.client(a)
    client_b = mesh.client(b)
    address_a = a.url if path == "explicit_relay" else None
    address_b = b.url if path == "explicit_relay" else None
    old_a = mesh.track(client_a.connect(Lifecycle, name=name, address=address_a, timeout=5))
    old_b = mesh.track(client_b.connect(Lifecycle, name=name, address=address_b, timeout=5))
    assert old_a.client.observed_path == path
    assert old_b.client.observed_path == path
    original_a = (cc.observe.connection_observation(old_a).connection_id
                  if path == "relay_aware_local_ipc" else None)
    assert old_a.call("before-join-a")["resource"] == host_a.identity["server_id"]
    assert old_b.call("before-join-b")["resource"] == host_b.identity["server_id"]
    mesh.join(a, b)
    for relay in (a, b):
        wait_state(lambda: mesh.routes(relay, name),
                   lambda rows: len(rows) == 2 and all(row["conflict"] for row in rows))
        lines = [line for line in mesh.registry_text(relay).splitlines() if line.startswith(name)]
        assert len(lines) == 2
        assert all(line.endswith(" - Alive - route conflict") for line in lines)
        details = _error(mesh.resolve(relay, name), 409, 719, "RouteConflict", name)
        assert {row["relay_status"] for row in _holders(details, {a.relay_id, b.relay_id})} == {"Alive"}
    for client, relay in ((client_a, a), (client_b, b)):
        with pytest.raises(cc.error.RouteConflict) as conflict:
            client.connect(Lifecycle, name=name,
                           address=relay.url if path == "explicit_relay" else None,
                           timeout=5)
        assert conflict.value.details["dispatch_phase"] == "pre_dispatch"
        _holders(conflict.value.details, {a.relay_id, b.relay_id})
    assert old_a.call("still-a")["resource"] == host_a.identity["server_id"]
    assert old_b.call("still-b")["resource"] == host_b.identity["server_id"]
    assert host_a.command("unregister")["unregistered"]
    for relay in (a, b):
        wait_state(lambda: mesh.routes(relay, name),
                   lambda rows: len(rows) == 1 and rows[0]["relay_id"] == b.relay_id
                   and rows[0]["conflict"] is False)
    # ADR 0004 note: "只能从这次调用得知时（端点或 relay 仍在线，名字已换到别处），这次调用在派发前失败".
    # Errors reference: "including a route a relay has withdrawn (`ResourceNotFound`, 701)"
    # and "removed (`ResourceRemoved`, 708)". The next call follows the live name.
    refusal_type = (cc.error.ResourceRemoved if path == "relay_aware_local_ipc"
                    else cc.error.ResourceNotFound)
    with pytest.raises(refusal_type) as refused:
        old_a.call("refused-on-a")
    phase_key = ("transport_phase" if path == "relay_aware_local_ipc"
                 else "dispatch_phase")
    assert refused.value.details[phase_key] == "pre_dispatch"
    assert old_a.call("followed-b")["resource"] == host_b.identity["server_id"]
    expected_follow_path = ("relay_aware_relay" if path == "relay_aware_local_ipc"
                            else "explicit_relay")
    assert old_a.client.observed_path == expected_follow_path
    if original_a is not None:
        with pytest.raises(cc.error.UnsupportedCallMode):
            cc.observe.connection_observation(old_a)
    assert old_b.call("still-b-after")["resource"] == host_b.identity["server_id"]
    assert [row["method"] for row in host_a.records()] == ["call", "call"]
    assert [row["method"] for row in host_b.records()] == ["call"] * 4
    if path == "relay_aware_local_ipc":
        record_property("route_identity_g7", json.dumps({
            "starting_path": path, "follow_path": old_a.client.observed_path,
            "original_connection": original_a, "host_a": host_a.identity,
            "host_b": host_b.identity, "host_a_callbacks": host_a.records(),
            "host_b_callbacks": host_b.records(), "refusal": type(refused.value).__name__,
        }))
    for relay in (a, b):
        detected = f"route conflict detected: route={name}"
        resolved = f"route conflict resolved: route={name}"
        log = wait_state(relay.log, lambda value: detected in value and resolved in value)
        assert log.count(detected) == 1
        assert log.count(resolved) == 1


def test_crashed_host_admin_withdrawal_propagates(mesh):
    name = f"crashed-{uuid.uuid4().hex}"
    a = mesh.relay("crashed-a")
    b = mesh.relay("crashed-b", seeds=[a.gate.url])
    mesh.join(a, b)
    host_a = mesh.host(name, a, "crashed-a")
    host_b = mesh.host(name, b, "crashed-b")
    _registered(host_a)
    wait_state(lambda: mesh.routes(b, name), lambda rows: len(rows) == 1)
    host_a.kill()
    assert host_a.process.poll() is not None
    assert len(mesh.routes(a, name)) == 1
    response = mesh.http.post(f"{a.url}/_admin/unregister",
                              json={"name": name, "server_id": host_a.identity["server_id"]})
    assert response.status_code == 200, response.text
    wait_state(lambda: mesh.routes(b, name), lambda rows: not rows)
    _registered(host_b)


def test_force_leave_allows_registration_then_restored_owner_conflicts(mesh):
    name = f"force-{uuid.uuid4().hex}"
    a = mesh.relay("force-a")
    b = mesh.relay("force-b", seeds=[a.gate.url])
    mesh.join(a, b)
    host_a = mesh.host(name, a, "force-a")
    host_b = mesh.host(name, b, "force-b")
    _registered(host_a)
    wait_state(lambda: mesh.routes(b, name), lambda rows: len(rows) == 1)
    a.gate.block()
    b.gate.block()
    wait_state(lambda: mesh.peer(b, a.relay_id),
               lambda peer: peer is not None and peer["status"] == "Dead", timeout=65)
    response = mesh.http.post(f"{b.url}/_admin/force-leave", json={"relay_id": a.relay_id})
    assert response.status_code == 200, response.text
    wait_state(lambda: mesh.routes(b, name), lambda rows: not rows)
    _registered(host_b)
    a.gate.restore()
    b.gate.restore()
    mesh.join(a, b)
    wait_state(lambda: mesh.routes(b, name),
               lambda rows: len(rows) == 2 and all(row["conflict"] for row in rows), timeout=60)
    details = _error(mesh.resolve(b, name), 409, 719, "RouteConflict", name)
    _holders(details, {a.relay_id, b.relay_id})


def test_relay_crash_restart_reregisters_after_peer_recovery(mesh):
    name = f"restart-{uuid.uuid4().hex}"
    a = mesh.relay("restart-a")
    b = mesh.relay("restart-b", seeds=[a.gate.url])
    mesh.join(a, b)
    host_a = mesh.host(name, a, "restart-a")
    _registered(host_a)
    wait_state(lambda: mesh.routes(b, name), lambda rows: len(rows) == 1)
    a.process.kill()
    assert a.process.wait(timeout=5) != 0
    host_a.kill()
    assert host_a.process.poll() is not None
    wait_state(lambda: mesh.peer(b, a.relay_id),
               lambda peer: peer is not None and peer["status"] == "Dead", timeout=65)
    # Restart at the same address and identity, then exchange the owner's
    # authoritative absence before a new Host claims the route.
    a.restart()
    mesh.join(a, b)
    stale = {"protocol_version": 7, "sender_relay_id": b.relay_id,
             "message": {"type": "DigestExchange", "digest": [
                 {"name": name, "relay_id": a.relay_id, "deleted": False, "hash": "0" * 64}
             ]}}
    response = mesh.http.post(f"{a.gate.url}/_peer/digest", json=stale)
    assert response.status_code == 200, response.text
    diff = response.json()
    assert any(entry["name"] == name and entry["state"] == "Deleted"
               for entry in diff["message"]["entries"])
    applied = mesh.http.post(f"{b.gate.url}/_peer/digest", json=diff)
    assert applied.status_code == 200, applied.text
    wait_state(lambda: mesh.routes(b, name), lambda rows: not rows)
    replacement = mesh.host(name, a, "restart-new")
    _registered(replacement)
    wait_state(lambda: mesh.routes(b, name),
               lambda rows: len(rows) == 1 and rows[0]["relay_status"] == "Alive",
               timeout=65)
    assert mesh.resolve(b, name).status_code == 200
    assert a.process.poll() is None
