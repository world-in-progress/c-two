"""Private-endpoint fixtures and resource barriers for persistent Python clients."""

from __future__ import annotations

from collections import Counter
from collections.abc import Iterator
import json
from http.server import BaseHTTPRequestHandler
import threading
import time
import urllib.parse
import uuid

import c_two as cc
import c_two.observe
from c_two.crm.contract import crm_contract
import httpx
import pytest
from fastdb4py.payload import Payload
from tests.fixtures.http_server import LoopbackHTTPServer
from tests.fixtures.stream_python import BLOB_SPEC, blob_payload


Reply = dict[str, str | dict[str, int]]


@cc.crm(namespace="test.persistent-lifecycle", version="1.0.0")
class Lifecycle:
    @cc.read
    def call(self, value: str) -> Reply: ...

    @cc.read
    def blocked(self) -> Reply: ...

    @cc.read
    @cc.transfer(output=BLOB_SPEC)
    def held_blocked(self) -> Payload: ...

    @cc.read
    @cc.stream
    def steps(self) -> Iterator[Reply]: ...

    def commit_then_fail(self) -> None: ...


def wait_state(observe, predicate, *, timeout: float = 10):
    """Wait for an observed state, never use elapsed time as evidence."""
    deadline = time.monotonic() + timeout
    wake = threading.Event()
    while True:
        value = observe()
        if predicate(value):
            return value
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"state did not converge: {value!r}"
        wake.wait(min(0.01, remaining))


class Resource:
    def __init__(self, identity: dict, label: str):
        self.identity = identity
        self.label = label
        self.calls = Counter()
        self.lock = threading.Lock()
        self.entered = threading.Event()
        self.release = threading.Event()

    def snapshot(self) -> dict:
        with self.lock:
            return dict(self.calls)

    def reply(self, method: str, value: str) -> dict:
        with self.lock:
            self.calls[method] += 1
            calls = dict(self.calls)
        return {**self.identity, "resource": self.label, "value": value, "calls": calls}

    def call(self, value: str) -> Reply:
        return self.reply("call", value)

    def blocked(self) -> Reply:
        reply = self.reply("blocked", "x" * (256 * 1024))
        self.entered.set()
        assert self.release.wait(15), "test did not release resource callback"
        return reply

    def steps(self) -> Iterator[Reply]:
        yield self.reply("stream", "first")
        self.entered.set()
        assert self.release.wait(15), "test did not release stream step"
        yield self.reply("stream_step", "second")

    def held_blocked(self) -> Payload:
        return blob_payload(json.dumps(self.blocked()).encode())

    def commit_then_fail(self) -> None:
        self.reply("committed", "committed")
        raise RuntimeError("committed once")


class SnapshotResolver:
    """Expose a fixed real relay discovery snapshot, independent of cache TTL."""

    def __init__(self, routes: list[dict], *, namespace: str | None = None, pause_first: bool = False):
        self.routes = routes
        self.requests = 0
        self.lock = threading.Lock()
        self.entered = threading.Event()
        self.release = threading.Event()
        if not pause_first:
            self.release.set()
        resolver = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                assert self.path.startswith("/_resolve/"), self.path
                with resolver.lock:
                    resolver.requests += 1
                    first = resolver.requests == 1
                    data = json.dumps(resolver.routes).encode()
                if first:
                    resolver.entered.set()
                    assert resolver.release.wait(15), "test did not release discovery snapshot"
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                if namespace is not None:
                    self.send_header("x-c2-local-namespace", namespace)
                self.end_headers()
                self.wfile.write(data)

        self.server = LoopbackHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive()


class Events:
    def __init__(self, runtime: cc.Runtime):
        self.source = runtime.connection_events()
        self.records: list[dict] = []
        self.ended = False

    def take(self, timeout: float = 0) -> dict | None:
        if self.ended:
            return None
        try:
            event = self.source.recv(timeout)
        except StopIteration:
            self.ended = True
            return None
        if event is None:
            return None
        result = {"side": event.side, "kind": event.kind, "id": event.connection_id}
        if event.kind == "closed":
            result["reason"] = event.value.reason
        self.records.append(result)
        return result

    def drain(self) -> None:
        while self.take() is not None:
            pass

    def ids(self, kind: str, side: str) -> set[int]:
        self.drain()
        return {event["id"] for event in self.records
                if event["kind"] == kind and event["side"] == side}

    def active(self, side: str) -> set[int]:
        return self.ids("opened", side) - self.ids("closed", side)


class Lab:
    def __init__(self, path: str, root: str | None, start_relay):
        self.path = path
        self.server = cc.Runtime()
        self.client = cc.Runtime()
        self.resources: list[Resource] = []
        self.proxies = []
        self.refusals: list[dict] = []
        self.extra: dict = {}
        self.names: dict[str, str] = {}
        self.owner = None
        self.anchor = None
        if root is not None:
            self.server.set_local_endpoint(root=root)
            self.client.set_local_endpoint(root=root)
        self.outgoing = Events(self.client)
        if path != "direct_ipc":
            self.owner = start_relay(idle_timeout=1)
            self.server.set_relay_anchor(self.owner.url)
            self.anchor = self.owner
            if path == "relay_aware_relay":
                self.anchor = start_relay(seeds=[self.owner.url], idle_timeout=1)
            self.client.set_relay_anchor(self.anchor.url)
        self.accepted = Events(self.server)
        self.server.start()
        self.identity = dict(self.server._runtime_session.ensure_server())
        self.address = self.identity["ipc_address"]
        self.http = httpx.Client(trust_env=False, timeout=2)

    def name(self, label: str) -> str:
        if label not in self.names:
            self.names[label] = f"persistent-{label}-{uuid.uuid4().hex}"
        return self.names[label]

    def register(self, label: str) -> Resource:
        resource = Resource(self.identity, label)
        self.resources.append(resource)
        self.server.register(
            Lifecycle, resource, name=self.name(label),
            concurrency=cc.ConcurrencyConfig(mode="parallel", max_workers=4),
        )
        if self.anchor is not None:
            self.wait_route(label)
        return resource

    def routes(self, label: str) -> list[dict]:
        expected = crm_contract(Lifecycle)
        query = urllib.parse.urlencode({key: getattr(expected, key) for key in (
            "crm_ns", "crm_name", "crm_ver", "abi_hash", "signature_hash",
        )})
        response = self.http.get(
            f"{self.anchor.url}/_resolve/{self.name(label)}?{query}",
            headers={"x-c2-local-namespace": self.server._runtime_session.local_endpoint_context().namespace_id},
        )
        if response.status_code == 404:
            return []
        assert response.status_code == 200, response.text
        return response.json()

    def wait_route(self, label: str) -> list[dict]:
        return wait_state(lambda: self.routes(label), lambda routes: bool(routes))

    def connect(self, label: str):
        kwargs = {"address": self.address} if self.path == "direct_ipc" else {}
        try:
            proxy = self.client.connect(Lifecycle, name=self.name(label), timeout=3, **kwargs)
        except cc.error.CCBaseError as error:
            self.refusals.append({
                "route": self.name(label), "type": type(error).__name__,
                "details": dict(getattr(error, "details", {})),
                "cause": {key: getattr(error.__cause__, key, None) for key in (
                    "category", "name", "transport_kind", "transport_phase",
                    "io_kind", "raw_os_error", "details",
                )},
            })
            raise
        self.proxies.append(proxy)
        assert proxy.client.observed_path == self.path
        assert proxy.client.route_uid
        return proxy

    def assert_reply(self, reply: dict, resource: Resource, value: str) -> None:
        assert {key: reply[key] for key in self.identity} == self.identity
        assert reply["resource"] == resource.label
        assert reply["value"] == value

    def connection_id(self, proxy) -> int:
        return cc.observe.connection_observation(proxy).connection_id

    def assert_pre_dispatch(self, error) -> None:
        phase_key = "dispatch_phase" if self.path == "relay_aware_relay" else "transport_phase"
        assert error.details[phase_key] == "pre_dispatch"

    def closed_route_errors(self, *, acquiring: bool = False) -> tuple[type[Exception], ...]:
        """A relay may withdraw a closed route before lookup or HTTP admission."""
        refused = (cc.error.FallbackDenied if acquiring and self.path == "relay_aware_local_ipc"
                   else cc.error.ResourceClosed)
        if self.anchor is not None and (acquiring or self.path == "relay_aware_relay"):
            return refused, cc.error.ResourceNotFound
        return (refused,)

    def assert_acquire_closed(self, label: str, uid: str) -> None:
        """Keep the typed IPC cause whenever acquisition denies its fallback."""
        with pytest.raises(self.closed_route_errors(acquiring=True)) as caught:
            self.connect(label)
        if isinstance(caught.value, cc.error.FallbackDenied):
            details = caught.value.details
            assert details["direct_ipc_failure_kind"] == "semantic:ResourceClosed"
            assert details["direct_ipc_failure_phase"] == "pre_dispatch"
            cause = json.loads(details["direct_ipc_failure_envelope"])
            assert cause["version"] == 1
            assert cause["name"] == "ResourceClosed"
            assert cause["code"] == 707
            assert cause["details"]["route"] == self.name(label)
            assert cause["details"]["route_uid"] == uid
            assert details["server_id"] == self.identity["server_id"]
            assert details["server_instance_id"] == self.identity["server_instance_id"]
            assert details["route_uid"] == uid

    def cleanup(self) -> dict:
        for resource in self.resources:
            resource.release.set()
        for proxy in self.proxies:
            cc.close(proxy)
        client = self.client.shutdown(timeout=10)
        server = self.server.shutdown(timeout=10)
        assert client["completed"], client
        assert server["completed"], server
        self.outgoing.drain()
        self.accepted.drain()
        for events, side in ((self.outgoing, "client"), (self.accepted, "server")):
            assert events.ids("opened", side) == events.ids("closed", side), events.records
            events.source.close()
        self.http.close()
        return {"client": client, "server": server}

    def receipt(self, cleanup: dict) -> str:
        return json.dumps({
            "observed_path": self.path, "identity": self.identity,
            "route_uids": [proxy.client.route_uid for proxy in self.proxies],
            "business_calls": [resource.snapshot() for resource in self.resources],
            "acquire_refusals": self.refusals,
            "physical_client_connections": len(self.outgoing.ids("opened", "client")),
            "physical_server_connections": len(self.accepted.ids("opened", "server")),
            "connection_events": self.outgoing.records,
            "accepted_connection_events": self.accepted.records,
            "cleanup": cleanup,
            "external_peers": self.extra,
        }, sort_keys=True)
