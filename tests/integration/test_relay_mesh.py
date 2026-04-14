"""Integration tests for the relay mesh resource discovery system.

Tests cover:
- Single-relay register/resolve/unregister (backward compat)
- Two-relay gossip route propagation
- Route withdrawal propagation across peers
- Peer discovery and listing
"""
from __future__ import annotations

import json
import threading
import time
import urllib.error
import urllib.request

import pytest

import c_two as cc
from c_two._native import NativeRelay

pytestmark = pytest.mark.skipif(
    not hasattr(cc._native, "NativeRelay"),
    reason="relay feature not compiled",
)

# Port allocation — use 19200+ range to avoid conflicts with other tests.
_counter = 0
_lock = threading.Lock()


def _next_port() -> int:
    global _counter
    with _lock:
        _counter += 1
        return 19200 + _counter


def _http_post(url: str, body: dict) -> urllib.request.Request:
    return urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )


def _http_get_json(url: str):
    with urllib.request.urlopen(url, timeout=5) as resp:
        return json.loads(resp.read())


class TestSingleRelay:
    """Tests with one relay — backward compatibility."""

    def test_register_and_resolve(self):
        port = _next_port()
        relay = NativeRelay(f"127.0.0.1:{port}", skip_ipc_validation=True)
        relay.start()
        base = f"http://127.0.0.1:{port}"
        try:
            # Register a mock upstream.
            req = _http_post(f"{base}/_register", {"name": "grid", "address": "ipc://test_grid"})
            with urllib.request.urlopen(req, timeout=5) as resp:
                assert resp.status == 201

            # Resolve.
            routes = _http_get_json(f"{base}/_resolve/grid")
            assert len(routes) >= 1
            assert routes[0]["name"] == "grid"

            # Unregister.
            req = _http_post(f"{base}/_unregister", {"name": "grid"})
            with urllib.request.urlopen(req, timeout=5) as resp:
                assert resp.status == 200

            # Resolve should now 404.
            with pytest.raises(urllib.error.HTTPError, match="404"):
                urllib.request.urlopen(f"{base}/_resolve/grid", timeout=5)
        finally:
            relay.stop()

    def test_peers_empty(self):
        port = _next_port()
        relay = NativeRelay(f"127.0.0.1:{port}", skip_ipc_validation=True)
        relay.start()
        try:
            peers = _http_get_json(f"http://127.0.0.1:{port}/_peers")
            assert peers == []
        finally:
            relay.stop()


class TestTwoRelayMesh:
    """Tests with two relays in a mesh."""

    def test_gossip_route_propagation(self):
        port_a = _next_port()
        port_b = _next_port()
        url_a = f"http://127.0.0.1:{port_a}"
        url_b = f"http://127.0.0.1:{port_b}"

        relay_a = NativeRelay(
            f"127.0.0.1:{port_a}",
            relay_id="relay-a",
            skip_ipc_validation=True,
        )
        relay_a.start()

        relay_b = NativeRelay(
            f"127.0.0.1:{port_b}",
            relay_id="relay-b",
            seeds=[url_a],
            skip_ipc_validation=True,
        )
        relay_b.start()

        try:
            # Wait for join to complete.
            time.sleep(2)

            # Register on relay A.
            req = _http_post(f"{url_a}/_register", {"name": "grid", "address": "ipc://grid_a"})
            urllib.request.urlopen(req, timeout=5)

            # Wait for gossip propagation.
            time.sleep(3)

            # Resolve on relay B should find "grid".
            routes = _http_get_json(f"{url_b}/_resolve/grid")
            assert len(routes) >= 1
            assert routes[0]["name"] == "grid"

            # Both relays should see each other as peers.
            peers_a = _http_get_json(f"{url_a}/_peers")
            assert any(p["relay_id"] == "relay-b" for p in peers_a)
        finally:
            relay_b.stop()
            relay_a.stop()

    def test_route_withdraw_propagation(self):
        port_a = _next_port()
        port_b = _next_port()
        url_a = f"http://127.0.0.1:{port_a}"
        url_b = f"http://127.0.0.1:{port_b}"

        relay_a = NativeRelay(
            f"127.0.0.1:{port_a}",
            relay_id="relay-a",
            skip_ipc_validation=True,
        )
        relay_a.start()

        relay_b = NativeRelay(
            f"127.0.0.1:{port_b}",
            relay_id="relay-b",
            seeds=[url_a],
            skip_ipc_validation=True,
        )
        relay_b.start()

        try:
            time.sleep(2)

            # Register on A.
            req = _http_post(f"{url_a}/_register", {"name": "net", "address": "ipc://net_a"})
            urllib.request.urlopen(req, timeout=5)
            time.sleep(2)

            # Verify B can resolve.
            routes = _http_get_json(f"{url_b}/_resolve/net")
            assert len(routes) >= 1

            # Unregister on A.
            req = _http_post(f"{url_a}/_unregister", {"name": "net"})
            urllib.request.urlopen(req, timeout=5)
            time.sleep(2)

            # Resolve on B should now 404.
            with pytest.raises(urllib.error.HTTPError, match="404"):
                urllib.request.urlopen(f"{url_b}/_resolve/net", timeout=5)
        finally:
            relay_b.stop()
            relay_a.stop()

    def test_peer_discovery_bidirectional(self):
        """Both relays should discover each other after join."""
        port_a = _next_port()
        port_b = _next_port()
        url_a = f"http://127.0.0.1:{port_a}"
        url_b = f"http://127.0.0.1:{port_b}"

        relay_a = NativeRelay(
            f"127.0.0.1:{port_a}",
            relay_id="relay-a",
            skip_ipc_validation=True,
        )
        relay_a.start()

        relay_b = NativeRelay(
            f"127.0.0.1:{port_b}",
            relay_id="relay-b",
            seeds=[url_a],
            skip_ipc_validation=True,
        )
        relay_b.start()

        try:
            time.sleep(2)

            # A sees B.
            peers_a = _http_get_json(f"{url_a}/_peers")
            assert any(p["relay_id"] == "relay-b" for p in peers_a)

            # B sees A.
            peers_b = _http_get_json(f"{url_b}/_peers")
            assert any(p["relay_id"] == "relay-a" for p in peers_b)
        finally:
            relay_b.stop()
            relay_a.stop()

    def test_resolve_returns_relay_url(self):
        """/_resolve response includes the relay_url of the registering relay."""
        port_a = _next_port()
        port_b = _next_port()
        url_a = f"http://127.0.0.1:{port_a}"
        url_b = f"http://127.0.0.1:{port_b}"

        relay_a = NativeRelay(
            f"127.0.0.1:{port_a}",
            relay_id="relay-a",
            skip_ipc_validation=True,
        )
        relay_a.start()

        relay_b = NativeRelay(
            f"127.0.0.1:{port_b}",
            relay_id="relay-b",
            seeds=[url_a],
            skip_ipc_validation=True,
        )
        relay_b.start()

        try:
            time.sleep(2)

            # Register on A.
            req = _http_post(
                f"{url_a}/_register",
                {"name": "solver", "address": "ipc://solver_a"},
            )
            urllib.request.urlopen(req, timeout=5)
            time.sleep(2)

            # Resolve on B — relay_url should point to A.
            routes = _http_get_json(f"{url_b}/_resolve/solver")
            assert len(routes) == 1
            assert routes[0]["relay_url"] == url_a
        finally:
            relay_b.stop()
            relay_a.stop()
