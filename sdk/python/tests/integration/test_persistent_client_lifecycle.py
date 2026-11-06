"""Late registration and route authority through persistent Python SDK clients.

Exact-token acquisition stays internal: relay/state.rs's
exact_token_ready_to_{closed,removed,reregistered}_uses_current_authority
tests cover it over real IPC. Here acquire means the SDK's name-based connect.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import tempfile
import threading
import uuid

import pytest

import c_two as cc
import c_two.endpoint
from tests.fixtures.persistent_lifecycle_support import Events, Lab, Lifecycle, SnapshotResolver, wait_state
from tests.fixtures.persistent_lifecycle_peer import Peer
from tests.fixtures.stream_python import blob_bytes


@pytest.fixture
def private_root(monkeypatch):
    monkeypatch.setenv("C2_ENV_FILE", "")
    monkeypatch.setenv("C2_RELAY_ANCHOR_ADDRESS", "")
    if os.name == "nt":
        monkeypatch.delenv("C2_IPC_ROOT", raising=False)
        yield None
        return
    with tempfile.TemporaryDirectory(prefix="c2-pl-", dir="/tmp") as directory:
        monkeypatch.setenv("C2_IPC_ROOT", directory)
        yield directory
        root = Path(directory)
        assert not list(root.glob("*.lease"))
        assert not any(path.is_socket() for path in root.iterdir())


@pytest.fixture(params=["direct_ipc", "relay_aware_local_ipc", "relay_aware_relay"])
def lab(request, private_root, start_c3_relay, record_property):
    scenario = Lab(request.param, private_root, start_c3_relay)
    try:
        yield scenario
    finally:
        cleanup = scenario.cleanup()
        record_property("persistent_lifecycle", scenario.receipt(cleanup))


def test_late_builder_uses_persistent_manager_endpoint(lab):
    """Gate 1: a later route is acquired on the existing endpoint and path."""
    manager = lab.register("manager")
    manager_proxy = lab.connect("manager")
    lab.assert_reply(manager_proxy.call("before"), manager, "before")
    client_connections = lab.outgoing.ids("opened", "client")
    server_connections = lab.accepted.active("server")
    with pytest.raises(cc.error.ResourceNotFound):
        lab.connect("builder")
    builder = lab.register("builder")
    builder_proxy = lab.connect("builder")
    lab.assert_reply(builder_proxy.call("built"), builder, "built")
    lab.assert_reply(manager_proxy.call("after"), manager, "after")
    assert builder_proxy.client.route_uid != manager_proxy.client.route_uid
    if lab.path != "relay_aware_relay":
        assert lab.connection_id(manager_proxy) == lab.connection_id(builder_proxy)
        assert len(client_connections) == 1
    assert lab.outgoing.ids("opened", "client") == client_connections
    # Relay registration performs short attestation probes. Count them in the
    # receipt, while checking that no additional persistent carrier survives.
    wait_state(lambda: lab.accepted.active("server"), lambda ids: ids == server_connections)
    assert manager.snapshot() == {"call": 2}
    assert builder.snapshot() == {"call": 1}


def test_route_close_remove_and_reregister_use_current_authority(lab):
    """Gate 2: close/remove affect name acquire and already acquired clients."""
    resource = lab.register("target")
    proxy = lab.connect("target")
    lab.assert_reply(proxy.call("ready"), resource, "ready")
    old_uid = proxy.client.route_uid
    scheduler = lab.server._server.get_local_slot_info(lab.name("target"))[1]
    with ThreadPoolExecutor(max_workers=2) as threads:
        running = threads.submit(proxy.blocked)
        assert resource.entered.wait(5)
        removing = threads.submit(lab.server.unregister, lab.name("target"))
        try:
            wait_state(scheduler.snapshot, lambda state: state.closed)
            if lab.path != "relay_aware_relay":
                lab.assert_acquire_closed("target", old_uid)
            closed_errors = cc.error.ResourceClosed
            if lab.path == "relay_aware_relay":
                closed_errors = (cc.error.ResourceClosed, cc.error.ResourceNotFound)
            with pytest.raises(closed_errors) as closed:
                proxy.call("closed")
            lab.extra["closed_error"] = {
                "details": closed.value.details,
                "cause_details": getattr(closed.value.__cause__, "details", None),
                "cause_phase": getattr(closed.value.__cause__, "transport_phase", None),
            }
            if lab.path != "relay_aware_relay":
                lab.assert_pre_dispatch(closed.value)
            if lab.path == "relay_aware_relay":
                # Either the periodic attestation or this call withdraws the
                # closed route; the next name acquisition sees no resource.
                wait_state(lambda: lab.routes("target"), lambda routes: not routes)
                lab.assert_acquire_closed("target", old_uid)
            assert resource.snapshot() == {"call": 1, "blocked": 1}
        finally:
            resource.release.set()
        lab.assert_reply(running.result(5), resource, "x" * (256 * 1024))
        removing.result(5)
    # After relay withdrawal a new name lookup has no matching resource.
    # The already-bound IPC proxy still carries the observed removed UID.
    acquire_removed = cc.error.ResourceRemoved
    call_removed = cc.error.ResourceRemoved
    if lab.anchor is not None:
        wait_state(lambda: lab.routes("target"), lambda routes: not routes)
        acquire_removed = cc.error.ResourceNotFound
        if lab.path == "relay_aware_relay":
            call_removed = cc.error.ResourceNotFound
    with pytest.raises(acquire_removed):
        lab.connect("target")
    with pytest.raises(call_removed):
        proxy.call("removed")
    replacement = lab.register("target")
    new_proxy = lab.connect("target")
    assert new_proxy.client.route_uid != old_uid
    if lab.path == "relay_aware_relay":
        # The preceding authoritative 404 invalidated this HTTP client's
        # discovery cache. A new logical call now resolves before dispatch.
        lab.assert_reply(proxy.call("after-observed-removal"), replacement, "after-observed-removal")
    else:
        with pytest.raises(cc.error.RouteStale) as stale:
            proxy.call("stale")
        lab.assert_pre_dispatch(stale.value)
        assert replacement.snapshot() == {}
    lab.assert_reply(new_proxy.call("new"), replacement, "new")
    assert resource.snapshot() == {"call": 1, "blocked": 1}
    assert replacement.snapshot() == {"call": 2 if lab.path == "relay_aware_relay" else 1}


@pytest.mark.parametrize("lab", ["relay_aware_relay"], indirect=True)
def test_http_stale_post_is_not_replayed_and_next_call_resolves(lab, private_root):
    """Gates 2/6: a stale business POST fails once, then the caller may retry."""
    old = lab.register("stale")
    resolver = SnapshotResolver(lab.routes("stale"))
    client = cc.Runtime()
    if private_root is not None:
        client.set_local_endpoint(root=private_root)
    client.set_relay_anchor(resolver.url)
    proxy = None
    try:
        proxy = client.connect(Lifecycle, name=lab.name("stale"), timeout=3)
        assert proxy.client.observed_path == "relay_aware_relay"
        old_uid = proxy.client.route_uid
        lab.assert_reply(proxy.call("old"), old, "old")
        lab.server.unregister(lab.name("stale"))
        new = lab.register("stale")
        current = lab.routes("stale")
        assert current[0]["route_uid"] != old_uid
        with pytest.raises(cc.error.RouteStale):
            proxy.call("must-not-replay")
        assert new.snapshot() == {}
        requests_before = resolver.requests
        resolver.routes = current
        lab.assert_reply(proxy.call("caller-retries"), new, "caller-retries")
        assert resolver.requests == requests_before + 1
        assert new.snapshot() == {"call": 1}
        assert old.snapshot() == {"call": 1}
        lab.extra["stale_http"] = {"observed_path": proxy.client.observed_path,
                                  "old_uid": old_uid, "new_uid": current[0]["route_uid"],
                                  "resolve_requests": resolver.requests}
    finally:
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        resolver.close()


def test_stateful_resource_failure_is_not_replayed(lab):
    """Gate 6: a failed non-idempotent callback commits once on a persistent path."""
    resource = lab.register("stateful")
    proxy = lab.connect("stateful")
    uid = proxy.client.route_uid
    lab.assert_reply(proxy.call("before"), resource, "before")
    connections = lab.outgoing.ids("opened", "client")
    with pytest.raises(cc.error.ResourceExecuteFunction, match="committed once") as failed:
        proxy.commit_then_fail()
    assert "transport_phase" not in failed.value.details
    assert "dispatch_phase" not in failed.value.details
    assert resource.snapshot() == {"call": 1, "committed": 1}
    reply = proxy.call("after")
    lab.assert_reply(reply, resource, "after")
    assert reply["calls"] == {"call": 2, "committed": 1}
    assert proxy.client.route_uid == uid
    assert lab.outgoing.ids("opened", "client") == connections


@pytest.mark.parametrize("lab", ["relay_aware_local_ipc", "relay_aware_relay"], indirect=True)
def test_late_candidate_cannot_withdraw_or_close_current_owner(lab, private_root):
    """Gate 5: a captured old discovery response resumes during new-owner work.

    RouteStale is refreshed before any business dispatch; the current owner's
    in-flight response and physical carrier survive the old candidate.
    """
    # Keep the endpoint's independent control connection alive while this one
    # route is replaced, so its business reconnections can be counted exactly.
    lab.register("manager")
    old = lab.register("candidate")
    old_proxy = lab.connect("candidate")
    old_uid = old_proxy.client.route_uid
    lab.assert_reply(old_proxy.call("old"), old, "old")
    namespace = cc.endpoint.local_endpoint_context(root=private_root).namespace_id
    resolver = SnapshotResolver(lab.routes("candidate"), namespace=namespace, pause_first=True)
    client = cc.Runtime()
    if private_root is not None:
        client.set_local_endpoint(root=private_root)
    client.set_relay_anchor(resolver.url)
    events = Events(client)
    candidate = None
    replacement = None
    try:
        with ThreadPoolExecutor(max_workers=2) as threads:
            acquiring = threads.submit(client.connect, Lifecycle, name=lab.name("candidate"), timeout=10)
            assert resolver.entered.wait(5)
            try:
                lab.server.unregister(lab.name("candidate"))
                replacement = lab.register("candidate")
                current = lab.connect("candidate")
                new_uid = current.client.route_uid
                assert new_uid != old_uid
                lab.assert_reply(current.call("new"), replacement, "new")
                running = threads.submit(current.blocked)
                assert replacement.entered.wait(5)
                carrier = lab.outgoing.ids("opened", "client")
                accepted = lab.accepted.ids("opened", "server")
                resolver.routes = lab.routes("candidate")
                resolver.release.set()
                candidate = acquiring.result(5)
                assert candidate.client.observed_path == lab.path
                assert candidate.client.route_uid == new_uid
                assert old_proxy.client.route_uid == old_uid
                assert not running.done()
                assert old.snapshot() == {"call": 1}
                assert replacement.snapshot() == {"call": 1, "blocked": 1}
                lab.assert_reply(candidate.call("candidate"), replacement, "candidate")
                lab.assert_reply(current.call("owner-live"), replacement, "owner-live")
                assert not (carrier & lab.outgoing.ids("closed", "client"))
                assert len(lab.accepted.ids("opened", "server") - accepted) == (1 if lab.path == "relay_aware_local_ipc" else 0)
                assert len(events.ids("opened", "client")) == (1 if lab.path == "relay_aware_local_ipc" else 0)
                assert len(lab.outgoing.ids("opened", "client")) == len(carrier)
                assert resolver.requests == 2
                routes = lab.routes("candidate")
                assert routes[0]["route_uid"] == new_uid
                lab.extra["late_candidate"] = {
                    "old_uid": old_uid, "new_uid": new_uid,
                    "observed_path": candidate.client.observed_path,
                    "identity": lab.identity, "resolves": resolver.requests,
                    "calls": replacement.snapshot(),
                    "client_events": events.records,
                }
            finally:
                resolver.release.set()
                if replacement is not None:
                    replacement.release.set()
            lab.assert_reply(running.result(5), replacement, "x" * (256 * 1024))
        assert replacement.snapshot() == {"call": 3, "blocked": 1}
    finally:
        if candidate is not None:
            cc.close(candidate)
        outcome = client.shutdown(timeout=5)
        assert outcome["completed"], outcome
        events.drain()
        assert events.ids("opened", "client") == events.ids("closed", "client")
        events.source.close()
        lab.extra["candidate_cleanup"] = outcome
        resolver.close()


def test_background_idle_eviction_then_concurrent_reconnect(lab):
    """Gate 3: background closure precedes four callers sharing one reconnect."""
    lab.client.set_client(ipc_overrides={"client_idle_timeout": 0.1})
    resource = lab.register("idle")
    proxy = lab.connect("idle")
    uid = proxy.client.route_uid
    lab.assert_reply(proxy.call("before-idle"), resource, "before-idle")
    outgoing_before = lab.outgoing.ids("opened", "client")
    accepted_before = lab.accepted.ids("opened", "server")
    active_before = lab.accepted.active("server")
    cc.close(proxy)
    if lab.path == "relay_aware_relay":
        # The relay's --idle-timeout=1 evicts the business carrier; its
        # independent upstream control connection retains route authority.
        wait_state(lambda: lab.accepted.active("server"),
                   lambda ids: len(active_before - ids) == 1)
    else:
        wait_state(lambda: lab.outgoing.ids("closed", "client"),
                   lambda ids: outgoing_before <= ids)
    assert resource.snapshot() == {"call": 1}
    if lab.anchor is not None:
        routes = lab.wait_route("idle")
        assert routes[0]["route_uid"] == uid
        assert routes[0]["relay_url"] == lab.owner.url
    barrier = threading.Barrier(4)

    def invoke(index):
        barrier.wait(timeout=5)
        new = lab.connect("idle")
        assert new.client.route_uid == uid
        lab.assert_reply(new.call(str(index)), resource, str(index))
        return new

    with ThreadPoolExecutor(max_workers=4) as threads:
        proxies = list(threads.map(invoke, range(4)))
    lab.extra["idle_reconnect"] = {
        "before": sorted(outgoing_before),
        "after": sorted(lab.outgoing.ids("opened", "client")),
        "proxy_connections": ([lab.connection_id(proxy) for proxy in proxies]
                              if lab.path != "relay_aware_relay" else []),
        "business_calls": resource.snapshot(),
    }
    if lab.path == "relay_aware_relay":
        assert not lab.outgoing.ids("opened", "client")
    else:
        assert len({lab.connection_id(proxy) for proxy in proxies}) == 1
        assert len(lab.outgoing.ids("opened", "client") - outgoing_before) == 1
    assert len(lab.accepted.ids("opened", "server") - accepted_before) == 1
    assert resource.snapshot() == {"call": 5}


@pytest.mark.parametrize("transition", ["closed", "removed", "stale"])
def test_other_route_refusal_preserves_inflight_held_response(lab, transition):
    """Gate 4: B's route refusal preserves A's shared carrier and local hold."""
    first = lab.register("a")
    second = lab.register("b")
    a = lab.connect("a")
    b = lab.connect("b")
    lab.assert_reply(b.call("ready"), second, "ready")
    if lab.path != "relay_aware_relay":
        assert lab.connection_id(a) == lab.connection_id(b)
    client_connections = lab.outgoing.ids("opened", "client")
    scheduler = lab.server._server.get_local_slot_info(lab.name("b"))[1]
    with ThreadPoolExecutor(max_workers=3) as threads:
        # Pickle replies are already materialized and retain no raw buffer.
        # A portable reply exercises Core's counted local-copy owner.
        running = threads.submit(cc.hold(a.held_blocked))
        assert first.entered.wait(5)
        try:
            if transition == "closed":
                b_running = threads.submit(b.blocked)
                assert second.entered.wait(5)
                removing = threads.submit(lab.server.unregister, lab.name("b"))
                wait_state(scheduler.snapshot, lambda state: state.closed)
                expected = cc.error.ResourceClosed
                if lab.path == "relay_aware_relay":
                    expected = (cc.error.ResourceClosed, cc.error.ResourceNotFound)
            else:
                lab.server.unregister(lab.name("b"))
                expected = (cc.error.ResourceNotFound if lab.path == "relay_aware_relay"
                            else cc.error.ResourceRemoved)
                if transition == "stale":
                    replacement = lab.register("b")
                    expected = cc.error.RouteStale
            with pytest.raises(expected) as refused:
                b.call("refused")
            if lab.path != "relay_aware_relay":
                lab.assert_pre_dispatch(refused.value)
            assert not running.done()
            assert lab.outgoing.ids("opened", "client") == client_connections
            assert not (lab.outgoing.ids("closed", "client") & client_connections)
            if transition == "closed":
                second.release.set()
                lab.assert_reply(b_running.result(5), second, "x" * (256 * 1024))
                removing.result(5)
            if transition == "stale":
                assert replacement.snapshot() == {}
        finally:
            first.release.set()
            second.release.set()
        held = running.result(5)
    try:
        lab.assert_reply(json.loads(blob_bytes(held.value)), first, "x" * (256 * 1024))
        assert lab.client.memory_stats()["holds"]["active_holds"] == 1
        lab.assert_reply(a.call("still-live"), first, "still-live")
        lab.assert_reply(json.loads(blob_bytes(held.value)), first, "x" * (256 * 1024))
    finally:
        held.release()
    assert lab.client.memory_stats()["holds"]["active_holds"] == 0
    assert first.snapshot() == {"blocked": 1, "call": 1}
    assert second.snapshot() == ({"call": 1, "blocked": 1} if transition == "closed" else {"call": 1})


def test_peer_eof_is_uncertain_and_new_acquire_uses_new_instance(lab, private_root, tmp_path):
    """Gates 3/4/5/6: real stream EOF, no replay, then acquire the restarted owner.

    Both Unix sockets and Windows Named Pipes use a killed resource process.
    Its durable journal proves that the dispatched stream was entered once.
    Restart also interrupts the relay's old upstream control connection; the
    replacement registers itself through the anchor, without --upstream.
    """
    name = lab.name("crash")
    server_id = f"persistent-{uuid.uuid4().hex}"
    anchor = lab.owner.url if lab.owner is not None else ""
    old = Peer(name, server_id, anchor, tmp_path / "old.calls", private_root)
    new = None
    stream = None
    try:
        lab.address = old.identity["ipc_address"]
        lab.identity = old.identity
        if lab.anchor is not None:
            lab.extra["routes_before_restart"] = lab.wait_route("crash")
        proxy = lab.connect("crash")
        old_uid = proxy.client.route_uid
        completed = proxy.call("settled")
        assert completed["server_instance_id"] == old.identity["server_instance_id"]
        stream = proxy.steps()
        delivered = next(stream)
        assert delivered["value"] == "first"
        assert delivered["server_instance_id"] == old.identity["server_instance_id"]
        assert old.command("snapshot")["calls"] == {"call": 1, "stream": 1}
        before = lab.outgoing.ids("opened", "client")
        lab.extra["old"] = old.kill()
        expected = (cc.error.ResourceUnavailable if lab.path == "relay_aware_relay"
                    else cc.error.CoreError)
        with pytest.raises(expected) as uncertain:
            next(stream)
        phase = (uncertain.value.details["dispatch_phase"] if lab.path == "relay_aware_relay"
                 else uncertain.value.transport_phase)
        assert phase == "dispatch_uncertain"
        lab.extra["stream_failure"] = {
            "type": type(uncertain.value).__name__, "phase": phase,
            "details": uncertain.value.details,
        }
        terminal = stream.close(timeout=5)
        assert terminal["completed"], terminal
        assert isinstance(terminal["error"], expected)
        assert delivered["value"] == "first"
        assert completed["value"] == "settled"
        assert [record["method"] for record in old.journal_records()] == ["call", "stream"]
        if lab.path != "relay_aware_relay":
            wait_state(lambda: lab.outgoing.ids("closed", "client"), lambda ids: before <= ids)
        # The old OS process and its namespace lease are gone before the
        # replacement claims the same server ID and endpoint.
        new = Peer(name, server_id, anchor, tmp_path / "new.calls", private_root)
        assert new.identity["server_id"] == old.identity["server_id"]
        assert new.identity["server_instance_id"] != old.identity["server_instance_id"]
        if lab.anchor is not None:
            lab.extra["routes_after_restart"] = wait_state(
                lambda: lab.routes("crash"),
                lambda routes: bool(routes) and routes[0]["route_uid"] != old_uid,
            )
        replacement = lab.connect("crash")
        assert replacement.client.route_uid != old_uid
        assert new.command("snapshot")["calls"] == {}
        reply = replacement.call("recovered-before-dispatch")
        assert reply["server_instance_id"] == new.identity["server_instance_id"]
        assert reply["calls"] == {"call": 1}
        if lab.path != "relay_aware_relay":
            with pytest.raises(cc.error.CCBaseError):
                proxy.call("old-binding")
        assert proxy.client.route_uid == old_uid
        assert new.command("snapshot")["calls"] == {"call": 1}
        if lab.path == "relay_aware_relay":
            assert not lab.outgoing.ids("opened", "client")
        else:
            assert len(lab.outgoing.ids("opened", "client") - before) == 1
        # The old control observer's disconnect and the old bound call must
        # not withdraw the replacement owner's route.
        reply = replacement.call("owner-still-live")
        assert reply["server_instance_id"] == new.identity["server_instance_id"]
        assert reply["calls"] == {"call": 2}
        lab.extra["new_snapshot"] = new.command("snapshot")
    finally:
        if stream is not None:
            stream.close(timeout=0)
        if new is not None:
            lab.extra["new_cleanup"] = new.close()
        lab.extra["old_cleanup"] = old.close()
