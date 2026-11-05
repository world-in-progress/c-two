"""Python server streams over a real one-hop c3 relay and explicit HTTP client."""

from __future__ import annotations

import threading
import uuid
from collections.abc import Iterator

import pytest

import c_two as cc
from c_two import error


@cc.crm(namespace="test.relay-server-stream", version="0.1.0")
class RelayValues:
    @cc.stream
    def steps(self, count: int) -> Iterator[int]: ...

    @cc.stream
    def failed_step(self) -> Iterator[int]: ...

    @cc.stream
    def failed_cleanup(self) -> Iterator[int]: ...

    @cc.stream
    def gated(self) -> Iterator[int]: ...

    @cc.stream
    def large(self, count: int) -> Iterator[bytes]: ...

    @cc.read
    def ping(self) -> str: ...


class RelayResource:
    def __init__(self) -> None:
        self.events: list[str] = []
        self.step_entered = threading.Event()
        self.release_step = threading.Event()
        self.cleanup_entered = threading.Event()
        self.release_cleanup = threading.Event()
        self.large_second_produced = threading.Event()
        self.gated_yields = 0

    def steps(self, count: int) -> Iterator[int]:
        try:
            for index in range(count):
                yield index
        finally:
            self.events.append("steps-cleanup")

    def failed_step(self) -> Iterator[int]:
        try:
            yield 0
            raise ValueError("relay step failed")
        finally:
            self.events.append("step-cleanup")

    def failed_cleanup(self) -> Iterator[int]:
        try:
            yield 0
        finally:
            self.events.append("cleanup-start")
            raise ValueError("relay cleanup failed")

    def gated(self) -> Iterator[int]:
        try:
            self.gated_yields += 1
            yield 0
            self.step_entered.set()
            if not self.release_step.wait(10):
                raise AssertionError("test did not release stream step")
            index = 1
            while True:
                self.gated_yields += 1
                yield index
                index += 1
        finally:
            self.cleanup_entered.set()
            if not self.release_cleanup.wait(10):
                raise AssertionError("test did not release stream cleanup")
            self.events.append("gated-cleanup")

    def large(self, count: int) -> Iterator[bytes]:
        for index in range(count):
            if index == 1:
                self.large_second_produced.set()
            yield b"x" * 20_000

    def ping(self) -> str:
        return "pong"


@pytest.fixture
def relay_values(start_c3_relay, monkeypatch, request):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    relay = start_c3_relay()
    server = cc.Runtime()
    client = cc.Runtime()
    if hasattr(request, "param"):
        client.set_call_execution_limits(
            payload_buffer_budget_bytes=request.param,
        )
    server.set_relay_anchor(relay.url)
    resource = RelayResource()
    proxy = None
    route_name = f"relay-stream-{uuid.uuid4().hex}"
    try:
        server.register(
            RelayValues, resource, name=route_name,
            concurrency=cc.ConcurrencyConfig(mode="parallel", max_workers=2),
        )
        # An explicit HTTP address forces the relay forwarder, even on one host.
        proxy = client.connect(
            RelayValues, name=route_name, address=relay.url,
        )
        assert proxy.client.observed_path == "explicit_relay"
        yield relay, server, client, proxy, resource
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


@pytest.mark.parametrize("count", [0, 1, 5])
def test_natural_items_and_complete_count(relay_values, count):
    _, _, client, proxy, resource = relay_values
    stream = proxy.steps(count)
    assert list(stream) == list(range(count))
    assert stream.close() == {
        "completed": True, "error": None, "complete_items": count,
    }
    assert stream.close(timeout=0)["complete_items"] == count
    assert resource.events == ["steps-cleanup"]
    assert client.call_execution_snapshot()["used_operations"] == 0


@pytest.mark.parametrize("method", ["failed_step", "failed_cleanup"])
def test_resource_failure_preserves_completed_item(relay_values, method):
    _, _, _, proxy, resource = relay_values
    stream = getattr(proxy, method)()
    assert next(stream) == 0
    with pytest.raises(error.ResourceExecuteFunction, match="failed"):
        next(stream)
    receipt = stream.close()
    assert receipt["completed"] is True
    assert isinstance(receipt["error"], error.ResourceExecuteFunction)
    assert receipt["error"].details.get("transport_phase") is None
    assert receipt["complete_items"] == 1
    assert resource.events == [
        "step-cleanup" if method == "failed_step" else "cleanup-start",
    ]


def test_held_items_release_their_independent_local_copies(relay_values):
    _, _, client, proxy, _ = relay_values
    stream = cc.hold(proxy.steps)(2)
    first = second = None
    try:
        first = next(stream)
        second = next(stream)
        assert (first.value, second.value) == (0, 1)
        assert client.memory_stats()["holds"]["active_holds"] == 2
        first.release()
        assert second.value == 1
        assert client.memory_stats()["holds"]["active_holds"] == 1
        second.release()
        assert list(stream) == []
        assert stream.close() == {
            "completed": True, "error": None, "complete_items": 2,
        }
        assert client.memory_stats()["holds"]["active_holds"] == 0
    finally:
        if first is not None:
            first.release()
        if second is not None:
            second.release()
        stream.close(timeout=0)


@pytest.mark.parametrize("relay_values", [30_000], indirect=True)
def test_held_item_budget_release_allows_continued_delivery(relay_values):
    _, _, client, proxy, resource = relay_values
    # Each pickle item is about 20 KiB. One fits, but two retained copies do not.
    stream = cc.hold(proxy.large)(3)
    first = second = third = None
    result: dict[str, object] = {}
    started = threading.Event()
    finished = threading.Event()

    def receive_second() -> None:
        started.set()
        try:
            result["value"] = next(stream)
        except BaseException as exc:
            result["error"] = exc
        finally:
            finished.set()

    worker = threading.Thread(target=receive_second, daemon=True)
    try:
        first = next(stream)
        assert first.value == b"x" * 20_000
        assert client.memory_stats()["holds"]["active_holds"] == 1
        worker.start()
        assert started.wait(5)
        first.release()
        worker.join(5)
        assert not worker.is_alive()
        assert finished.is_set()
        assert "error" not in result
        assert resource.large_second_produced.is_set()
        second = result["value"]
        assert second.value == b"x" * 20_000
        assert client.memory_stats()["holds"]["active_holds"] == 1
        second.release()
        third = next(stream)
        assert third.value == b"x" * 20_000
        third.release()
        assert list(stream) == []
        assert stream.close(timeout=None)["complete_items"] == 3
        assert client.memory_stats()["holds"]["active_holds"] == 0
        assert client.memory_stats()["payload_buffer"]["peak_bytes"] <= 30_000
    finally:
        if first is not None:
            first.release()
        if second is not None:
            second.release()
        if third is not None:
            third.release()
        stream.close(timeout=0)
        if worker.ident is not None:
            worker.join(5)


def test_deadline_covers_whole_iteration_and_close_waits_for_cleanup(relay_values):
    _, _, _, proxy, resource = relay_values
    bounded = cc.with_call_options(proxy, timeout=0.2)
    stream = bounded.gated()
    try:
        assert next(stream) == 0
        with pytest.raises(error.CallDeadlineExceeded) as raised:
            next(stream)
        assert raised.value.transport_phase == "dispatch_uncertain"
        assert resource.step_entered.is_set()
        assert not resource.cleanup_entered.is_set()
        assert stream.close(timeout=0)["completed"] is False
        resource.release_step.set()
        assert resource.cleanup_entered.wait(5)
        resource.release_cleanup.set()
        receipt = stream.close(timeout=None)
        assert receipt["completed"] is True
        assert isinstance(receipt["error"], error.CallCancelled)
        # Cancellation can discard the single item in progress. The generator
        # cannot exhaust naturally and turn this test into a late-cancel race.
        assert max(1, resource.gated_yields - 1) <= receipt["complete_items"] <= resource.gated_yields
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        stream.close(timeout=0)
        cc.close(bounded)


def test_close_during_running_step_cancels_only_stream(relay_values):
    _, _, _, proxy, resource = relay_values
    stream = proxy.gated()
    outcome: dict[str, object] = {}

    def advance() -> None:
        try:
            outcome["value"] = next(stream)
        except BaseException as exc:
            outcome["error"] = exc

    assert next(stream) == 0
    worker = threading.Thread(target=advance, daemon=True)
    worker.start()
    try:
        assert resource.step_entered.wait(5)
        initial = stream.close(timeout=0)
        assert initial["completed"] is False
        assert proxy.ping() == "pong"
        resource.release_step.set()
        assert resource.cleanup_entered.wait(5)
        resource.release_cleanup.set()
        worker.join(5)
        assert not worker.is_alive()
        if "value" in outcome:
            assert outcome == {"value": 1}
        else:
            assert isinstance(outcome.get("error"), error.CallCancelled)
        receipt = stream.close(timeout=None)
        assert receipt["completed"] is True
        assert isinstance(receipt["error"], error.CallCancelled)
        assert max(1, resource.gated_yields - 1) <= receipt["complete_items"] <= resource.gated_yields
        assert proxy.ping() == "pong"
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        if worker.ident is not None:
            worker.join(5)
        stream.close(timeout=0)
