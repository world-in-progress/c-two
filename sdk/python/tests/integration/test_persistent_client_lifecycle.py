"""Late registration and route authority through persistent Python SDK clients."""

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
    assert builder_proxy.client.observed_path == manager_proxy.client.observed_path
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
    scheduler = lab.server._server.get_local_slot_info(lab.name("target"))[1]
    with ThreadPoolExecutor(max_workers=2) as threads:
        running = threads.submit(proxy.blocked)
        assert resource.entered.wait(5)
        removing = threads.submit(lab.server.unregister, lab.name("target"))
        try:
            wait_state(scheduler.snapshot, lambda state: state.closed)
            lab.assert_acquire_closed("target")
            with pytest.raises(lab.closed_route_errors()) as closed:
                proxy.call("closed")
            lab.extra["closed_error"] = {
                "details": closed.value.details,
                "cause_details": getattr(closed.value.__cause__, "details", None),
                "cause_phase": getattr(closed.value.__cause__, "transport_phase", None),
            }
            lab.assert_pre_dispatch(closed.value)
            assert "route_uid" not in closed.value.details
            if lab.anchor is not None:
                # A closed route can disappear from any relay's resolution.
                wait_state(lambda: lab.routes("target"), lambda routes: not routes)
                lab.assert_acquire_closed("target")
            assert resource.snapshot() == {"call": 1, "blocked": 1}
        finally:
            resource.release.set()
        lab.assert_reply(running.result(5), resource, "x" * (256 * 1024))
        removing.result(5)
    # A closed target is re-acquired on its next call. Relay discovery then
    # reports the absent route before the proxy enters a callback.
    acquire_removed = cc.error.ResourceRemoved
    call_removed = cc.error.ResourceRemoved
    if lab.anchor is not None:
        wait_state(lambda: lab.routes("target"), lambda routes: not routes)
        acquire_removed = cc.error.ResourceNotFound
        call_removed = cc.error.ResourceNotFound
    with pytest.raises(acquire_removed):
        lab.connect("target")
    with pytest.raises(call_removed) as absent:
        proxy.call("removed")
    lab.assert_pre_dispatch(absent.value)
    assert "route_uid" not in absent.value.details
    assert resource.snapshot() == {"call": 1, "blocked": 1}
    replacement = lab.register("target")
    new_proxy = lab.connect("target")
    # Each new acquisition reads the current relay registration.
    lab.assert_reply(proxy.call("after-observed-removal"), replacement, "after-observed-removal")
    lab.assert_reply(new_proxy.call("new"), replacement, "new")
    assert resource.snapshot() == {"call": 1, "blocked": 1}
    assert replacement.snapshot() == {"call": 2}
    assert not hasattr(proxy.client, "route_uid")
    assert not hasattr(new_proxy.client._client, "route_uid")


@pytest.mark.parametrize("lab", ["direct_ipc", "relay_aware_local_ipc"], indirect=True)
def test_real_process_same_endpoint_reregistration_reaches_current_resource(lab, private_root, tmp_path):
    """Gate 2: command acknowledgements bracket the real peer's empty interval."""
    name = lab.name("process-target")
    peer = Peer(name, f"persistent-{uuid.uuid4().hex}", lab.owner.url if lab.owner else "",
                tmp_path / "same-endpoint.calls", private_root)
    try:
        lab.address = peer.identity["ipc_address"]
        lab.identity = peer.identity
        if lab.anchor is not None:
            lab.wait_route("process-target")
        old_proxy = lab.connect("process-target")
        before = old_proxy.call("before")
        assert before["resource"] == name and before["calls"] == {"call": 1}
        assert peer.command("unregister")["unregistered"]
        with pytest.raises(cc.error.ResourceRemoved) as gap:
            old_proxy.call("gap")
        lab.assert_pre_dispatch(gap.value)
        assert [entry["method"] for entry in peer.journal_records()] == ["call"]
        assert peer.command("register")["registered"]
        if lab.anchor is not None:
            lab.wait_route("process-target")
        current = lab.connect("process-target")
        from_old = old_proxy.call("from-old-proxy")
        from_new = current.call("from-new-proxy")
        assert from_old["resource"] == from_new["resource"] == f"{name}:replacement"
        assert from_old["server_instance_id"] == from_new["server_instance_id"] == peer.identity["server_instance_id"]
        assert from_old["calls"] == {"call": 1}
        assert from_new["calls"] == {"call": 2}
        assert [entry["resource"] for entry in peer.journal_records()] == [name, f"{name}:replacement", f"{name}:replacement"]
    finally:
        lab.extra["same_endpoint_cleanup"] = peer.close()


@pytest.mark.parametrize("lab", ["relay_aware_relay"], indirect=True)
def test_http_old_proxy_uses_current_registration_after_reacquisition(lab, private_root):
    """Gate 2: old HTTP proxies reach each new registration exactly once."""
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
        lab.assert_reply(proxy.call("old"), old, "old")
        lab.server.unregister(lab.name("stale"))
        new = lab.register("stale")
        requests_before = resolver.requests
        lab.assert_reply(proxy.call("old-current"), new, "old-current")
        assert resolver.requests == requests_before
        assert new.snapshot() == {"call": 1}
        assert old.snapshot() == {"call": 1}
        lab.server.unregister(lab.name("stale"))
        # The real relay's pre-dispatch refusal prompts a subsequent acquire.
        with pytest.raises(cc.error.ResourceNotFound) as missing:
            proxy.call("missing-before-replacement")
        lab.assert_pre_dispatch(missing.value)
        assert new.snapshot() == {"call": 1}
        latest = lab.register("stale")
        resolver.routes = lab.routes("stale")
        other = client.connect(Lifecycle, name=lab.name("stale"), timeout=3)
        try:
            assert other.client.observed_path == "relay_aware_relay"
            lab.assert_reply(proxy.call("old-after-acquire"), latest, "old-after-acquire")
            lab.assert_reply(other.call("other-after-acquire"), latest, "other-after-acquire")
            assert latest.snapshot() == {"call": 2}
            assert resolver.requests > requests_before
        finally:
            cc.close(other)
        lab.extra["current_http"] = {"observed_path": proxy.client.observed_path,
                                     "resolve_requests": resolver.requests,
                                     "current_calls": latest.snapshot()}
    finally:
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        resolver.close()


@pytest.mark.parametrize("lab", ["relay_aware_relay"], indirect=True)
def test_explicit_http_proxy_follows_across_real_c3_relays_after_one_refusal(lab, private_root, tmp_path):
    """The old A data target refuses once; the next call resolves B through A."""
    name = lab.name("migration")
    old = Peer(name, f"persistent-a-{uuid.uuid4().hex}", lab.owner.url,
               tmp_path / "migration-a.calls", private_root)
    new = None
    proxy = None
    try:
        routes = wait_state(lambda: lab.routes("migration", relay_url=lab.owner.url),
                            lambda routes: bool(routes) and routes[0]["relay_url"] == lab.owner.url)
        assert routes[0]["server_instance_id"] == old.identity["server_instance_id"]
        proxy = lab.client.connect(Lifecycle, name=name, address=lab.owner.url, timeout=5)
        assert proxy.client.observed_path == "explicit_relay"
        settled = proxy.call("on-a")
        assert settled["server_instance_id"] == old.identity["server_instance_id"]
        assert [entry["method"] for entry in old.journal_records()] == ["call"]
        assert old.command("unregister")["unregistered"]
        wait_state(lambda: lab.routes("migration"), lambda routes: not routes)
        new = Peer(name, f"persistent-b-{uuid.uuid4().hex}", lab.anchor.url,
                   tmp_path / "migration-b.calls", private_root)
        wait_state(lambda: lab.routes("migration", relay_url=lab.owner.url),
                   lambda routes: bool(routes) and routes[0]["relay_url"] == lab.anchor.url)
        before = lab.client._runtime_session.path_counters()["explicit_relay"]  # noqa: SLF001
        with pytest.raises(cc.error.ResourceNotFound) as refused:
            proxy.call("refused-on-a")
        assert refused.value.details["dispatch_phase"] == "pre_dispatch"
        assert old.journal_records() == [{"method": "call", "resource": name, "calls": {"call": 1}}]
        assert new.journal_records() == []
        assert lab.client._runtime_session.path_counters()["explicit_relay"] == before  # noqa: SLF001
        followed = proxy.call("on-b")
        assert followed["server_instance_id"] == new.identity["server_instance_id"]
        assert followed["value"] == "on-b"
        assert new.command("snapshot")["calls"] == {"call": 1}
        assert lab.client._runtime_session.path_counters()["explicit_relay"] == before + 1  # noqa: SLF001
        assert proxy.client.observed_path == "explicit_relay"
        lab.extra["explicit_migration"] = {
            "from": old.identity, "to": new.identity,
            "old_journal": old.journal_records(), "new_journal": new.journal_records(),
        }
    finally:
        if proxy is not None:
            cc.close(proxy)
        if new is not None:
            lab.extra["migration_b_cleanup"] = new.close()
        lab.extra["migration_a_cleanup"] = old.close()


@pytest.mark.parametrize("lab", ["relay_aware_local_ipc"], indirect=True)
def test_relay_aware_proxy_switches_local_http_local_in_real_c3_mesh(
    lab, private_root, tmp_path, start_c3_relay,
):
    """One proxy selects A locally, B over HTTP, then A locally again."""
    relay_b = start_c3_relay(seeds=[lab.owner.url])
    name = lab.name("path-switch")
    owner_a = Peer(name, f"persistent-a-{uuid.uuid4().hex}", lab.owner.url,
                   tmp_path / "switch-a.calls", private_root)
    owner_b = None
    try:
        lab.address = owner_a.identity["ipc_address"]
        lab.identity = owner_a.identity
        lab.wait_route("path-switch")
        proxy = lab.connect("path-switch")
        assert proxy.client.observed_path == "relay_aware_local_ipc"
        assert proxy.call("on-a")["server_instance_id"] == owner_a.identity["server_instance_id"]
        assert owner_a.command("unregister")["unregistered"]
        with pytest.raises(cc.error.ResourceRemoved) as missing:
            proxy.call("missing-on-a")
        lab.assert_pre_dispatch(missing.value)
        wait_state(lambda: lab.routes("path-switch", relay_url=relay_b.url),
                   lambda routes: not routes)
        owner_b = Peer(name, f"persistent-b-{uuid.uuid4().hex}", relay_b.url,
                       tmp_path / "switch-b.calls", private_root)
        wait_state(lambda: lab.routes("path-switch"),
                   lambda routes: bool(routes) and routes[0]["relay_url"] == relay_b.url)
        assert proxy.call("on-b")["server_instance_id"] == owner_b.identity["server_instance_id"]
        assert proxy.client.observed_path == "relay_aware_relay"
        with pytest.raises(cc.error.UnsupportedCallMode):
            cc.observe.connection_observation(proxy)
        assert owner_b.command("unregister")["unregistered"]
        wait_state(lambda: lab.routes("path-switch"), lambda routes: not routes)
        assert owner_a.command("register")["registered"]
        wait_state(lambda: lab.routes("path-switch"),
                   lambda routes: bool(routes) and routes[0]["relay_url"] == lab.owner.url
                   and routes[0]["server_instance_id"] == owner_a.identity["server_instance_id"])
        with pytest.raises(cc.error.ResourceNotFound) as missing:
            proxy.call("missing-on-b")
        assert missing.value.details["dispatch_phase"] == "pre_dispatch"
        assert proxy.call("back-on-a")["server_instance_id"] == owner_a.identity["server_instance_id"]
        assert proxy.client.observed_path == "relay_aware_local_ipc"
        assert cc.observe.connection_observation(proxy).connection_id is not None
        counters = lab.client._runtime_session.path_counters()  # noqa: SLF001
        assert counters["relay_aware_local_ipc"] == 2
        assert counters["relay_aware_relay"] == 1
        assert [entry["method"] for entry in owner_a.journal_records()] == ["call", "call"]
        assert [entry["method"] for entry in owner_b.journal_records()] == ["call"]
    finally:
        if owner_b is not None:
            lab.extra["switch_b_cleanup"] = owner_b.close()
        lab.extra["switch_a_cleanup"] = owner_a.close()


def test_stateful_resource_failure_is_not_replayed(lab):
    """Gate 6: a failed non-idempotent callback commits once on a persistent path."""
    resource = lab.register("stateful")
    proxy = lab.connect("stateful")
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
    assert proxy.client.observed_path == lab.path
    assert lab.outgoing.ids("opened", "client") == connections


@pytest.mark.parametrize("lab", ["relay_aware_local_ipc", "relay_aware_relay"], indirect=True)
def test_late_candidate_cannot_withdraw_or_close_current_owner(lab, private_root):
    """Gate 5: a captured old discovery response resumes during new-owner work.

    An old discovery response cannot withdraw the current owner's route. Its
    in-flight response and physical carrier survive the candidate.
    """
    # Keep the endpoint's independent control connection alive while this one
    # route is replaced, so its business reconnections can be counted exactly.
    lab.register("manager")
    old = lab.register("candidate")
    old_proxy = lab.connect("candidate")
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
                lab.assert_reply(current.call("new"), replacement, "new")
                running = threads.submit(current.blocked)
                assert replacement.entered.wait(5)
                carrier = lab.outgoing.ids("opened", "client")
                accepted = lab.accepted.ids("opened", "server")
                resolver.routes = lab.routes("candidate")
                resolver.release.set()
                candidate = acquiring.result(5)
                assert candidate.client.observed_path == lab.path
                if lab.path == "relay_aware_local_ipc":
                    lab.assert_reply(old_proxy.call("old-proxy"), replacement, "old-proxy")
                assert not running.done()
                assert old.snapshot() == {"call": 1}
                assert replacement.snapshot() == {
                    "call": 2 if lab.path == "relay_aware_local_ipc" else 1,
                    "blocked": 1,
                }
                lab.assert_reply(candidate.call("candidate"), replacement, "candidate")
                lab.assert_reply(current.call("owner-live"), replacement, "owner-live")
                assert not (carrier & lab.outgoing.ids("closed", "client"))
                assert len(lab.accepted.ids("opened", "server") - accepted) == (1 if lab.path == "relay_aware_local_ipc" else 0)
                assert len(events.ids("opened", "client")) == (1 if lab.path == "relay_aware_local_ipc" else 0)
                assert len(lab.outgoing.ids("opened", "client")) == len(carrier)
                # Same-relay replacement is accepted by name on both paths.
                assert resolver.requests == 1
                routes = lab.routes("candidate")
                assert routes[0]["relay_url"] == lab.owner.url
                if lab.path == "relay_aware_local_ipc":
                    assert routes[0]["server_instance_id"] == lab.identity["server_instance_id"]
                lab.extra["late_candidate"] = {
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
        assert replacement.snapshot() == {
            "call": 4 if lab.path == "relay_aware_local_ipc" else 3,
            "blocked": 1,
        }
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
    original_instance = lab.routes("idle")[0]["server_instance_id"] if lab.anchor is not None else None
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
        assert routes[0]["server_instance_id"] == original_instance
        assert routes[0]["relay_url"] == lab.owner.url
    barrier = threading.Barrier(4)

    def invoke(index):
        barrier.wait(timeout=5)
        new = lab.connect("idle")
        assert new.client.observed_path == lab.path
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


@pytest.mark.parametrize("transition", ["closed", "removed", "replaced"])
def test_other_route_refusal_preserves_inflight_held_response(lab, transition):
    """Gate 4: B's transition preserves A's shared carrier and local hold."""
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
                expected = lab.closed_route_errors()
            else:
                lab.server.unregister(lab.name("b"))
                expected = (cc.error.ResourceNotFound if lab.path == "relay_aware_relay"
                            else cc.error.ResourceRemoved)
                if transition == "replaced":
                    replacement = lab.register("b")
            if transition == "replaced":
                lab.assert_reply(b.call("replaced"), replacement, "replaced")
            else:
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
            if transition == "replaced":
                assert replacement.snapshot() == {"call": 1}
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
    if transition == "replaced":
        assert replacement.snapshot() == {"call": 1}


def test_peer_eof_is_uncertain_and_existing_proxy_follows_new_registration(lab, private_root, tmp_path):
    """Gates 3/4/5/6: real stream EOF, no replay, then the old proxy follows.

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
                lambda routes: bool(routes) and routes[0]["server_instance_id"] != old.identity["server_instance_id"],
            )
        assert new.command("snapshot")["calls"] == {}
        paths_before = lab.client._runtime_session.path_counters()[lab.path]  # noqa: SLF001
        view = cc.with_call_options(proxy, timeout=5)
        barrier = threading.Barrier(8)

        def follow(index):
            barrier.wait(timeout=5)
            return (view if index % 2 else proxy).call(f"follow-{index}")

        try:
            with ThreadPoolExecutor(max_workers=8) as threads:
                replies = list(threads.map(follow, range(8)))
        finally:
            cc.close(view)
        assert {reply["value"] for reply in replies} == {f"follow-{index}" for index in range(8)}
        assert all(reply["server_instance_id"] == new.identity["server_instance_id"] for reply in replies)
        assert not hasattr(proxy.client, "route_uid")
        assert new.command("snapshot")["calls"] == {"call": 8}
        assert lab.client._runtime_session.path_counters()[lab.path] == paths_before + (lab.path != "relay_aware_relay")  # noqa: SLF001
        if lab.path == "relay_aware_relay":
            assert not lab.outgoing.ids("opened", "client")
        else:
            assert len(lab.outgoing.ids("opened", "client") - before) == 1
        # The old control observer's disconnect and the old bound call must
        # not withdraw the replacement owner's route.
        reply = proxy.call("owner-still-live")
        assert reply["server_instance_id"] == new.identity["server_instance_id"]
        assert reply["calls"] == {"call": 9}
        lab.extra["new_snapshot"] = new.command("snapshot")
    finally:
        if stream is not None:
            stream.close(timeout=0)
        if new is not None:
            lab.extra["new_cleanup"] = new.close()
        lab.extra["old_cleanup"] = old.close()


@pytest.mark.parametrize("lab", ["direct_ipc"], indirect=True)
def test_late_old_response_cannot_close_restarted_direct_target(lab, private_root, tmp_path):
    """contracts-and-routing: "Calls already sent keep their target and finish there."

    ADR 0004: "注册结束时正在进行的调用照旧报完成或派发不确定，从不重放".
    Pause a real native reply before the SDK finishes its original prepared
    call, as the generated client's response barrier does; no I/O is mocked.
    """
    name = lab.name("late-response")
    server_id = f"persistent-late-{uuid.uuid4().hex}"
    old = Peer(name, server_id, "", tmp_path / "late-old.calls", private_root)
    new = None
    arrived = threading.Event()
    release = threading.Event()

    class PausedCall:
        def __init__(self, native):
            self.native = native

        def __getattr__(self, attribute):
            return getattr(self.native, attribute)

        def call(self, data):
            result = self.native.call(data)
            arrived.set()
            release.wait()
            return result

    class PausedClient:
        def __init__(self, native):
            self.native = native
            self.pause_next = True
            self.lock = threading.Lock()

        def __getattr__(self, attribute):
            return getattr(self.native, attribute)

        def begin_call(self, method):
            prepared = self.native.begin_call(method)
            with self.lock:
                pause = self.pause_next
                self.pause_next = False
            return PausedCall(prepared) if pause else prepared

    try:
        lab.address, lab.identity = old.identity["ipc_address"], old.identity
        proxy = lab.connect("late-response")
        assert proxy.call("before")["server_instance_id"] == old.identity["server_instance_id"]
        old_connection = cc.observe.connection_observation(proxy)
        proxy.client._client = PausedClient(proxy.client._client)
        with ThreadPoolExecutor(max_workers=1) as threads:
            late = threads.submit(proxy.call, "old-late")
            try:
                assert arrived.wait(5), "old native reply did not arrive"
                assert old.command("snapshot")["calls"] == {"call": 2}
                lab.extra["old_peer_end"] = old.kill()
                wait_state(old_connection.snapshot, lambda state: state is not None)
                new = Peer(name, server_id, "", tmp_path / "late-new.calls", private_root)
                assert new.identity["server_id"] == old.identity["server_id"]
                assert new.identity["server_instance_id"] != old.identity["server_instance_id"]
                followed = proxy.call("new-target")
                assert followed["server_instance_id"] == new.identity["server_instance_id"]
                assert followed["calls"] == {"call": 1}
                current = cc.observe.connection_observation(proxy)
                assert current.connection_id != old_connection.connection_id
                assert current.snapshot() is None
            finally:
                release.set()
            delivered = late.result(5)
        assert delivered["server_instance_id"] == old.identity["server_instance_id"]
        assert delivered["value"] == "old-late" and delivered["calls"] == {"call": 2}
        assert new.command("snapshot")["calls"] == {"call": 1}
        assert current.snapshot() is None
        after = proxy.call("after-old-completion")
        assert after["server_instance_id"] == new.identity["server_instance_id"]
        assert after["calls"] == {"call": 2}
        assert cc.observe.connection_observation(proxy).connection_id == current.connection_id
        assert len(lab.outgoing.ids("opened", "client")) == 2
        assert len(old.journal_records()) == len(new.journal_records()) == 2
        lab.extra["late_response"] = {
            "actual_path": proxy.client.observed_path,
            "from": old.identity, "to": new.identity,
            "old_result": delivered, "current_result": after,
            "reconnects": 1, "old_calls": old.journal_records(), "new_calls": new.journal_records(),
        }
    finally:
        release.set()
        if new is not None:
            lab.extra["late_new_cleanup"] = new.close()
        lab.extra["late_old_cleanup"] = old.close()
