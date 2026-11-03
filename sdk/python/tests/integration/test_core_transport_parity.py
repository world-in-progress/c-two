from __future__ import annotations

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.transport.registry import Runtime, _get_default_runtime, _reset_default_runtime
from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


@pytest.fixture(autouse=True)
def _clean_registry():
    _reset_default_runtime()
    yield
    _reset_default_runtime()


def test_direct_ipc_is_selected_by_the_single_core_client():
    cc.register(Hello, HelloImpl(), name="core-direct")
    address = cc.server_address()
    assert address is not None

    proxy = cc.connect(Hello, name="core-direct", address=address)
    try:
        assert proxy.greeting("Core") == "Hello, Core!"
        assert proxy.client._client.mode == "ipc"  # noqa: SLF001
        assert proxy.client.observed_path == "direct_ipc"
        assert proxy.client.route_uid
        counters = _get_default_runtime()._runtime_session.path_counters()  # noqa: SLF001
        assert counters == {
            "direct_ipc": 1,
            "explicit_relay": 0,
            "relay_aware_local_ipc": 0,
            "relay_aware_relay": 0,
        }
    finally:
        cc.close(proxy)


def test_semantic_error_fields_are_projected_from_core():
    from c_two.error import ResourceExecuteFunction

    @cc.crm(namespace="test.core-error", version="0.1.0")
    class Failure:
        def fail(self) -> str:
            ...

    class FailureResource:
        def fail(self) -> str:
            raise RuntimeError("portable Python fixture failure")

    cc.register(Failure, FailureResource(), name="core-error")
    proxy = cc.connect(
        Failure,
        name="core-error",
        address=cc.server_address(),
    )
    try:
        with pytest.raises(ResourceExecuteFunction) as caught:
            proxy.fail()
        assert caught.value.message.endswith("portable Python fixture failure")
        assert isinstance(caught.value.details, dict)
    finally:
        cc.close(proxy)


def test_semantic_error_fields_match_direct_explicit_and_relay_aware(
    start_c3_relay,
):
    from c_two.error import ResourceExecuteFunction

    @cc.crm(namespace="test.core-error-parity", version="0.1.0")
    class Failure:
        def fail(self) -> str:
            ...

    class FailureResource:
        def fail(self) -> str:
            raise RuntimeError("portable Python fixture failure")

    previous_anchor = settings._relay_anchor_address  # noqa: SLF001
    relay = start_c3_relay()
    proxies = []
    client_registry = None
    try:
        cc.set_relay_anchor(relay.url)
        cc.register(Failure, FailureResource(), name="core-error-parity")
        address = cc.server_address()
        assert address is not None

        client_registry = Runtime()
        client_registry.set_relay_anchor(relay.url)
        proxies = [
            client_registry.connect(
                Failure,
                name="core-error-parity",
                address=address,
            ),
            client_registry.connect(
                Failure,
                name="core-error-parity",
                address=relay.url,
            ),
            client_registry.connect(Failure, name="core-error-parity"),
        ]
        receipts = []
        for proxy in proxies:
            with pytest.raises(ResourceExecuteFunction) as caught:
                proxy.fail()
            receipts.append(
                (
                    caught.value.code,
                    caught.value.message,
                    caught.value.details,
                ),
            )

        assert receipts[0] == receipts[1] == receipts[2]
        assert client_registry._runtime_session.path_counters() == {  # noqa: SLF001
            "direct_ipc": 1,
            "explicit_relay": 1,
            "relay_aware_local_ipc": 1,
            "relay_aware_relay": 0,
        }
    finally:
        for proxy in proxies:
            if client_registry is not None:
                cc.close(proxy)
        if client_registry is not None:
            client_registry.shutdown()
        _reset_default_runtime()
        settings._relay_anchor_address = previous_anchor  # noqa: SLF001


@pytest.mark.parametrize('relay_anchor', [None, 'http://127.0.0.1:1'])
def test_low_level_host_shutdown_keeps_outgoing_calls_until_runtime_retirement(
    relay_anchor, monkeypatch, tmp_path,
):
    """A Host receipt cannot close the Runtime's client execution domain."""
    import os
    from c_two._native import RuntimeSession

    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    remote = RuntimeSession(use_process_relay_anchor=False)
    if os.name != 'nt':
        remote.set_local_endpoint(root=str(tmp_path))
    remote_bridge = remote.ensure_server_bridge()
    remote_bridge.register_crm(Hello, HelloImpl(), name='outgoing-target')
    remote_bridge.start()
    client = Runtime()
    if os.name != 'nt':
        client.set_local_endpoint(root=str(tmp_path))
    if relay_anchor is not None:
        client.set_relay_anchor(relay_anchor)
    client.set_call_execution_limits(max_outstanding_calls=4)
    client.start()  # An empty local Host still has a real shutdown receipt.
    old_session = client._runtime_session
    proxy = None
    try:
        proxy = client.connect(Hello, name='outgoing-target', address=remote.server_address)
        bounded = cc.with_call_options(proxy, timeout=2.0)
        try:
            assert bounded.add(1, 2) == 3
            bridge = client._server
            assert bridge.shutdown()['completed']
            assert old_session.native_lifecycle_snapshot() == {
                'finished': True, 'listener_closed': True, 'work_drained': True,
            }
            assert not client.call_execution_snapshot()['closed']
            assert bounded.add(3, 4) == 7
            bridge.start()
            assert bridge.is_started()
            assert client.shutdown()['completed']
            assert old_session.retired
            with pytest.raises(cc.error.CallCapacityExceeded) as refused:
                bounded.add(5, 6)
            assert refused.value.details['admission_reason'] == 'Closed'
            with pytest.raises(cc.error.CoreError) as retired:
                old_session.check_active()
            assert retired.value.lifecycle_kind == 'runtime_retired'
        finally:
            cc.close(bounded)
    finally:
        if proxy is not None:
            cc.close(proxy)
        client.shutdown()
        remote_bridge.shutdown()
