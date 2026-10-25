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


@pytest.mark.parametrize("fail_invalidation", [False, True], ids=["success", "failure"])
def test_held_response_delegates_invalidation_before_release_to_core(fail_invalidation):
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
        failure = RuntimeError("invalidate failed")

        def invalidator(value):
            events.append(f"invalidate:{value}")
            if fail_invalidation:
                raise failure

        def release_response():
            events.append("core")
            response.invalidate_then_release(invalidator, "owner")

        held = HeldResult("owner", release_cb=release_response)
        if fail_invalidation:
            with pytest.raises(RuntimeError, match="invalidate failed") as caught:
                held.release()
            assert caught.value is failure
        else:
            held.release()
        assert events == ["core", "invalidate:owner"]
        assert response.is_released
        with pytest.raises(RuntimeError, match="value no longer accessible"):
            _ = held.value
        held.release()
        response.invalidate_then_release(invalidator, "owner")
        response.release()
        assert response.is_released
        assert events == ["core", "invalidate:owner"]
    finally:
        cc.close(proxy)
