from __future__ import annotations

import pickle

import pytest

import c_two as cc
from c_two.crm.transferable import HeldResult
from c_two.transport.registry import _ProcessRegistry
from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


@pytest.fixture(autouse=True)
def _clean_registry():
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()


def test_held_response_delegates_invalidation_before_release_to_core():
    cc.register(Hello, HelloImpl(), name="core-held")
    proxy = cc.connect(Hello, name="core-held", address=cc.server_address())
    try:
        response = proxy.client._client.call(  # noqa: SLF001
            "greeting",
            pickle.dumps("Lifetime", protocol=4),
        )
        assert hasattr(response, "invalidate_then_release")
        assert pickle.loads(bytes(response)) == "Hello, Lifetime!"
        events: list[str] = []
        response.invalidate_then_release(
            lambda value: events.append(f"invalidate:{value}"),
            "owner",
        )
        assert events == ["invalidate:owner"]
        assert response.is_released
    finally:
        cc.close(proxy)


def test_invalidation_failure_still_runs_core_release_once():
    events: list[str] = []

    class CoreOrderedResponse:
        def invalidate_then_release(self, invalidator, value):
            events.append("core")
            invalidator(value)

    def invalidator(_value):
        events.append("invalidate")
        raise RuntimeError("invalidate failed")

    response = CoreOrderedResponse()
    held = HeldResult(
        "value",
        release_cb=lambda: response.invalidate_then_release(
            invalidator,
            "value",
        ),
    )
    with pytest.raises(RuntimeError, match="invalidate failed"):
        held.release()
    assert events == ["core", "invalidate"]
