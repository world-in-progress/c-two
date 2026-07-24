from __future__ import annotations

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.transport.registry import _ProcessRegistry
from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


@pytest.fixture(autouse=True)
def _clean_registry():
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()


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
        assert proxy.client.route_revision == 1
        counters = _ProcessRegistry.get()._runtime_session.path_counters()  # noqa: SLF001
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

        client_registry = _ProcessRegistry()
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
                client_registry.close(proxy)
        if client_registry is not None:
            client_registry.shutdown()
        _ProcessRegistry.reset()
        settings._relay_anchor_address = previous_anchor  # noqa: SLF001
