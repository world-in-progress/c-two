from __future__ import annotations

import contextvars
import decimal
import gc
import threading
from collections.abc import Iterator

import pytest

import c_two as cc
from c_two.transport.client.proxy import CRMProxy
from tests.fixtures.stream_python import StreamObjects, StreamValues, ValuesResource


@pytest.fixture
def local_values(monkeypatch):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    runtime = cc.Runtime()
    resource = ValuesResource()
    route = "local-stream-values"
    runtime.register(StreamValues, resource, name=route)
    proxy = runtime.connect(StreamValues, name=route)
    assert proxy.client._mode == "thread"
    try:
        yield runtime, proxy, resource
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        cc.close(proxy)
        assert runtime.shutdown(timeout=5)["completed"]


@pytest.mark.parametrize("count", [0, 1, 4])
def test_same_process_natural_completion_counts_each_item(local_values, count):
    _, proxy, resource = local_values
    stream = proxy.steps(count)
    assert isinstance(stream, cc.Stream)
    assert list(stream) == list(range(count))
    receipt = stream.close()
    assert receipt == {"completed": True, "error": None, "complete_items": count}
    assert resource.events == [
        "factory",
        *(f"yield:{index}" for index in range(count)),
        "finally",
    ]


def test_same_process_preserves_item_identity_and_does_not_eagerly_advance(monkeypatch):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    runtime = cc.Runtime()
    first, second = object(), object()

    class Resource:
        def __init__(self):
            self.events: list[str] = []

        def steps(self) -> Iterator[object]:
            self.events.append("factory")
            yield first
            self.events.append("second")
            yield second
            self.events.append("finally")

    resource = Resource()
    runtime.register(StreamObjects, resource, name="objects")
    proxy = runtime.connect(StreamObjects, name="objects")
    stream = None
    try:
        stream = proxy.steps()
        assert next(stream) is first
        assert next(stream) is second
        with pytest.raises(StopIteration):
            next(stream)
        assert resource.events == ["factory", "second", "finally"]
        assert stream.close()["complete_items"] == 2
    finally:
        if stream is not None:
            stream.close()
        cc.close(proxy)
        assert runtime.shutdown(timeout=5)["completed"]


@pytest.mark.parametrize("timeout", [0, 0.01, 3.0])
def test_same_process_finite_deadline_rejected_before_factory(local_values, timeout):
    _, proxy, resource = local_values
    view = cc.with_call_options(proxy, timeout=timeout)
    try:
        with pytest.raises((ValueError, cc.error.UnsupportedCallMode)):
            view.steps(2)
        assert resource.events == []
    finally:
        cc.close(view)


def test_captured_local_stream_target_stays_local_until_native_admission(monkeypatch):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    runtime = cc.Runtime()
    remote_runtime = cc.Runtime()
    old_resource = ValuesResource()
    remote_resource = ValuesResource()
    runtime.register(StreamValues, old_resource, name='stream-follow')
    remote_runtime.register(StreamValues, remote_resource, name='stream-follow')
    proxy = runtime.connect(StreamValues, name='stream-follow')
    monkeypatch.setattr(
        runtime, '_connect',
        lambda _crm_class, *, name, address, attempt: remote_runtime.connect(
            StreamValues, name=name, address=address or remote_runtime.server_address(),
        ),
    )

    captured = threading.Event()
    proceed = threading.Event()
    paused = False
    getter = CRMProxy.supports_direct_call.fget

    def gated_direct_call(target):
        nonlocal paused
        if threading.current_thread().name == 'captured-local-stream' and not paused:
            paused = True
            assert target._mode == 'thread'
            assert target._client is None
            captured.set()
            assert proceed.wait(5)
        return getter(target)

    monkeypatch.setattr(CRMProxy, 'supports_direct_call', property(gated_direct_call))
    result = {}

    def call_captured():
        try:
            stream = proxy.steps(1)
            try:
                result['value'] = list(stream)
            finally:
                stream.close()
        except BaseException as exc:
            result['error'] = exc

    worker = threading.Thread(target=call_captured, name='captured-local-stream')
    try:
        worker.start()
        assert captured.wait(5)
        runtime.unregister('stream-follow')
        followed = proxy.steps(1)
        try:
            assert list(followed) == [0]
        finally:
            followed.close()
        proceed.set()
        worker.join(5)
        assert not worker.is_alive()
        assert result == {'value': [0]}
        assert old_resource.events == []
        assert remote_resource.events.count('factory') == 2
    finally:
        proceed.set()
        worker.join(5)
        cc.close(proxy)
        assert runtime.shutdown(timeout=5)['completed']
        assert remote_runtime.shutdown(timeout=5)['completed']


def test_context_and_decimal_state_survive_yield_without_leaking_to_caller(monkeypatch):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    runtime = cc.Runtime()
    marker = contextvars.ContextVar("stream-marker", default="unset")
    observed: list[tuple[str, int]] = []

    class Resource:
        def steps(self, count: int) -> Iterator[int]:
            token = marker.set("stream")
            try:
                with decimal.localcontext() as context:
                    context.prec = 13
                    observed.append((marker.get(), decimal.getcontext().prec))
                    for index in range(count):
                        assert marker.get() == "stream"
                        assert decimal.getcontext().prec == 13
                        yield index
            finally:
                observed.append((marker.get(), decimal.getcontext().prec))
                marker.reset(token)

        def gated(self):
            raise AssertionError("unused")

        def failed_step(self):
            raise AssertionError("unused")

        def failed_cleanup(self):
            raise AssertionError("unused")

        def ping(self):
            return "pong"

    runtime.register(StreamValues, Resource(), name="context")
    proxy = runtime.connect(StreamValues, name="context")
    original_precision = decimal.getcontext().prec
    try:
        stream = proxy.steps(2)
        assert list(stream) == [0, 1]
        assert stream.close()["complete_items"] == 2
        assert observed == [("stream", 13), ("stream", original_precision)]
        assert marker.get() == "unset"
        assert decimal.getcontext().prec == original_precision
    finally:
        cc.close(proxy)
        assert runtime.shutdown(timeout=5)["completed"]


def test_early_context_exit_submits_cancel_without_waiting_for_current_step(local_values):
    _, proxy, resource = local_values
    stream = proxy.gated()
    assert next(stream) == 0
    outcome: dict[str, object] = {}

    def advance() -> None:
        try:
            outcome["item"] = next(stream)
        except BaseException as exc:
            outcome["error"] = exc

    thread = threading.Thread(target=advance, daemon=True)
    try:
        with stream:
            thread.start()
            assert resource.step_entered.wait(5)
        assert thread.is_alive()
        resource.release_step.set()
        assert resource.cleanup_entered.wait(5)
        pending = stream.close(timeout=0)
        assert pending == {"completed": False, "error": None, "complete_items": None}
        resource.release_cleanup.set()
        thread.join(5)
        assert not thread.is_alive()
        final = stream.close(timeout=None)
        assert final["completed"]
        assert isinstance(final["error"], cc.error.CallCancelled)
        assert final["complete_items"] in (1, 2)
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        thread.join(5)


def test_last_stream_handle_submits_cancel_without_explicit_close(monkeypatch):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    runtime = cc.Runtime()

    class Resource(ValuesResource):
        def __init__(self):
            super().__init__()
            self.finalized = threading.Event()

        def steps(self, count: int) -> Iterator[int]:
            try:
                yield from range(count)
            finally:
                self.finalized.set()

    resource = Resource()
    runtime.register(StreamValues, resource, name="finalizer")
    proxy = runtime.connect(StreamValues, name="finalizer")
    try:
        stream = proxy.steps(100)
        assert next(stream) == 0
        del stream
        gc.collect()
        assert resource.finalized.wait(5)
    finally:
        cc.close(proxy)
        assert runtime.shutdown(timeout=5)["completed"]
