"""Integration tests for the relay mesh resource discovery system.

Tests cover:
- Single-relay register/resolve/unregister
- Two-relay gossip route propagation
- Route withdrawal propagation across peers
- Peer discovery and listing
"""
from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request

import pytest

import c_two as cc
from c_two.config.ipc import _resolve_server_ipc_config
from c_two.config.settings import settings
from c_two.crm.contract import crm_contract
from c_two.transport.registry import _ProcessRegistry


@cc.crm(namespace="test.mesh", version="0.1.0")
class MeshResource:
    def ping(self) -> str:
        ...


class MeshImpl:
    def ping(self) -> str:
        return "pong"


MESH_CONTRACT = crm_contract(MeshResource)
DEFAULT_MAX_PAYLOAD_SIZE = int(_resolve_server_ipc_config()['max_payload_size'])


@pytest.fixture(autouse=True)
def _clean_registry():
    previous_relay = settings.relay_anchor_address
    settings.relay_anchor_address = None
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()
    settings.relay_anchor_address = previous_relay


def _http_post(url: str, body: dict) -> urllib.request.Request:
    return urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )


def _server_instance_id_for(address: str) -> str:
    registry = _ProcessRegistry.get()
    identity = dict(registry._runtime_session.ensure_server())  # noqa: SLF001
    assert identity["ipc_address"] == address
    instance_id = identity["server_instance_id"]
    assert isinstance(instance_id, str)
    assert instance_id
    return instance_id


def _register_body(name: str, server_id: str) -> dict[str, object]:
    cc.set_server(server_id=server_id)
    cc.register(MeshResource, MeshImpl(), name=name)
    address = cc.server_address()
    actual_server_id = cc.server_id()
    assert address is not None
    assert actual_server_id == server_id
    server_instance_id = _server_instance_id_for(address)
    return {
        "name": name,
        "server_id": server_id,
        "server_instance_id": server_instance_id,
        "address": address,
        "max_payload_size": DEFAULT_MAX_PAYLOAD_SIZE,
    }


def _http_get_json(url: str):
    with urllib.request.urlopen(url, timeout=5) as resp:
        return json.loads(resp.read())


def _resolve_url(base_url: str, name: str) -> str:
    quoted_name = urllib.parse.quote(name, safe="")
    query = urllib.parse.urlencode(
        {
            "crm_ns": "test.mesh",
            "crm_name": "MeshResource",
            "crm_ver": "0.1.0",
            "abi_hash": MESH_CONTRACT.abi_hash,
            "signature_hash": MESH_CONTRACT.signature_hash,
        }
    )
    return f"{base_url}/_resolve/{quoted_name}?{query}"


def _wait_for_json(getter, predicate, *, timeout: float = 8.0, interval: float = 0.1):
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    last_value = None
    while time.monotonic() < deadline:
        try:
            value = getter()
            last_value = value
            if predicate(value):
                return value
        except Exception as exc:
            last_error = exc
        time.sleep(interval)
    if last_error is not None:
        raise AssertionError(f"Timed out waiting for state; last error: {last_error}") from last_error
    raise AssertionError(f"Timed out waiting for state; last value: {last_value!r}")


def _wait_for_route(url: str, name: str, *, timeout: float = 8.0):
    return _wait_for_json(
        lambda: _http_get_json(_resolve_url(url, name)),
        lambda routes: any(route["name"] == name for route in routes),
        timeout=timeout,
    )


def _wait_for_route_missing(url: str, name: str, *, timeout: float = 8.0) -> None:
    deadline = time.monotonic() + timeout
    last_routes = None
    while time.monotonic() < deadline:
        try:
            last_routes = _http_get_json(_resolve_url(url, name))
        except urllib.error.HTTPError as exc:
            if exc.code == 404:
                return
            raise
        if not any(route["name"] == name for route in last_routes):
            return
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {name!r} to disappear; last routes: {last_routes!r}")


def _wait_for_peer(url: str, relay_id: str, *, timeout: float = 8.0):
    return _wait_for_json(
        lambda: _http_get_json(f"{url}/_peers"),
        lambda peers: any(peer["relay_id"] == relay_id for peer in peers),
        timeout=timeout,
    )


class TestSingleRelay:
    """Tests with one relay."""

    def test_register_and_resolve(self, start_c3_relay):
        relay = start_c3_relay()
        base = relay.url

        # Register a mock upstream.
        req = _http_post(
            f"{base}/_register",
            _register_body("grid", "test-grid"),
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            assert resp.status == 201

        # Resolve.
        routes = _http_get_json(_resolve_url(base, "grid"))
        assert len(routes) >= 1
        assert routes[0]["name"] == "grid"

        # Unregister.
        req = _http_post(
            f"{base}/_unregister",
            {"name": "grid", "server_id": "test-grid"},
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            assert resp.status == 200

        # Resolve should now 404.
        with pytest.raises(urllib.error.HTTPError, match="404"):
            urllib.request.urlopen(_resolve_url(base, "grid"), timeout=5)

    def test_peers_empty(self, start_c3_relay):
        relay = start_c3_relay()

        peers = _http_get_json(f"{relay.url}/_peers")
        assert peers == []

    def test_register_rejects_incomplete_crm_tag_before_ipc_connect(self, start_c3_relay):
        relay = start_c3_relay()
        req = _http_post(
            f"{relay.url}/_register",
            {
                "name": "grid",
                "server_id": "grid-a",
                "server_instance_id": "grid-a-instance",
                "address": "ipc://grid_a",
                "crm_ns": "test.mesh",
                "crm_name": "",
                "crm_ver": "0.1.0",
                "abi_hash": MESH_CONTRACT.abi_hash,
                "signature_hash": MESH_CONTRACT.signature_hash,
                "max_payload_size": DEFAULT_MAX_PAYLOAD_SIZE,
            },
        )

        with pytest.raises(urllib.error.HTTPError) as exc:
            urllib.request.urlopen(req, timeout=5)

        assert exc.value.code == 400


class TestTwoRelayMesh:
    """Tests with two relays in a mesh."""

    def test_gossip_route_propagation(self, start_c3_relay):
        relay_a = start_c3_relay(
            relay_id="relay-a",
        )
        url_a = relay_a.url

        relay_b = start_c3_relay(
            relay_id="relay-b",
            seeds=[url_a],
        )
        url_b = relay_b.url
        _wait_for_peer(url_a, "relay-b")

        # Register on relay A.
        req = _http_post(
            f"{url_a}/_register",
            _register_body("grid", "grid-a"),
        )
        urllib.request.urlopen(req, timeout=5)

        # Resolve on relay B should find "grid".
        routes = _wait_for_route(url_b, "grid")
        assert len(routes) >= 1
        assert routes[0]["name"] == "grid"
        assert routes[0]["server_instance_id"] is None

        # Both relays should see each other as peers.
        peers_a = _http_get_json(f"{url_a}/_peers")
        assert any(p["relay_id"] == "relay-b" for p in peers_a)

    def test_route_withdraw_propagation(self, start_c3_relay):
        relay_a = start_c3_relay(
            relay_id="relay-a",
        )
        url_a = relay_a.url

        relay_b = start_c3_relay(
            relay_id="relay-b",
            seeds=[url_a],
        )
        url_b = relay_b.url
        _wait_for_peer(url_a, "relay-b")

        # Register on A.
        req = _http_post(
            f"{url_a}/_register",
            _register_body("net", "net-a"),
        )
        urllib.request.urlopen(req, timeout=5)

        # Verify B can resolve.
        routes = _wait_for_route(url_b, "net")
        assert len(routes) >= 1

        # Unregister on A.
        req = _http_post(
            f"{url_a}/_unregister",
            {"name": "net", "server_id": "net-a"},
        )
        urllib.request.urlopen(req, timeout=5)

        # Resolve on B should now 404.
        _wait_for_route_missing(url_b, "net")

    def test_peer_discovery_bidirectional(self, start_c3_relay):
        """Both relays should discover each other after join."""
        relay_a = start_c3_relay(
            relay_id="relay-a",
        )
        url_a = relay_a.url

        relay_b = start_c3_relay(
            relay_id="relay-b",
            seeds=[url_a],
        )
        url_b = relay_b.url

        # A sees B.
        peers_a = _wait_for_peer(url_a, "relay-b")
        assert any(p["relay_id"] == "relay-b" for p in peers_a)

        # B sees A.
        peers_b = _wait_for_peer(url_b, "relay-a")
        assert any(p["relay_id"] == "relay-a" for p in peers_b)

    def test_resolve_returns_relay_url(self, start_c3_relay):
        """/_resolve response includes the relay_url of the registering relay."""
        relay_a = start_c3_relay(
            relay_id="relay-a",
        )
        url_a = relay_a.url

        relay_b = start_c3_relay(
            relay_id="relay-b",
            seeds=[url_a],
        )
        url_b = relay_b.url
        _wait_for_peer(url_a, "relay-b")

        # Register on A.
        req = _http_post(
            f"{url_a}/_register",
            _register_body("solver", "solver-a"),
        )
        urllib.request.urlopen(req, timeout=5)

        # Resolve on B — relay_url should point to A.
        routes = _wait_for_route(url_b, "solver")
        assert len(routes) == 1
        assert routes[0]["relay_url"] == url_a
