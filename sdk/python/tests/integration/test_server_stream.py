from __future__ import annotations

import gc
import os
import weakref
import multiprocessing
from pathlib import Path
import tempfile
import threading
import time
import uuid
from collections.abc import Iterator
from itertools import product

import pytest

import c_two as cc
import c_two.endpoint
from c_two import error
from fastdb4py.payload import Payload, PayloadError
from tests.fixtures.stream_python import (
    BLOB_SPEC,
    StreamPayloads,
    StreamValues,
    ValuesResource,
    blob_bytes,
    blob_payload,
)
from tests.fixtures.stream_matrix import (
    Portable as MatrixPortable,
    PythonHost as MatrixPythonHost,
    PythonHostProcess,
    RustHarness,
    TRANSPORT_CASES,
    assert_transport_stats,
    build_payload as matrix_payload,
    inspect as inspect_matrix_payload,
)


def _decode_failure():
    raise ValueError('stream item decode failed')


class _Undecodable:
    def __reduce__(self):
        return _decode_failure, ()


def test_one_tuple_argument_keeps_its_shape_over_ipc(monkeypatch, tmp_path):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)

    @cc.crm(namespace='test.stream.tuple_input', version='0.1.0')
    class Pairs:
        @cc.stream
        def steps(self, pair: tuple[int, int]) -> Iterator[int]: ...

    class Resource:
        def steps(self, pair):
            yield sum(pair)

    runtime = cc.Runtime()
    if os.name != 'nt':
        runtime.set_local_endpoint(root=str(tmp_path))
    proxy = None
    try:
        runtime.register(Pairs, Resource(), name='pairs')
        proxy = runtime.connect(Pairs, name='pairs', address=runtime.server_address())
        stream = proxy.steps((3, 4))
        try:
            assert list(stream) == [7]
        finally:
            stream.close()
    finally:
        if proxy is not None:
            cc.close(proxy)
        assert runtime.shutdown()['completed']


def test_decode_failure_cancels_while_caller_keeps_stream(monkeypatch, tmp_path):
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)

    @cc.crm(namespace='test.stream.decode_failure', version='0.1.0')
    class Values:
        @cc.stream
        def steps(self) -> Iterator[object]: ...

    cleaned = threading.Event()

    class Resource:
        def steps(self):
            try:
                while True:
                    yield _Undecodable()
            finally:
                cleaned.set()

    runtime = cc.Runtime()
    if os.name != 'nt':
        runtime.set_local_endpoint(root=str(tmp_path))
    proxy = stream = None
    try:
        runtime.register(Values, Resource(), name='decode')
        proxy = runtime.connect(Values, name='decode', address=runtime.server_address())
        stream = proxy.steps()
        with pytest.raises(error.ClientDeserializeOutput, match='decode failed'):
            next(stream)
        assert cleaned.wait(5), 'decode failure must submit cancellation without relying on GC'
        receipt = stream.close(timeout=None)
        assert receipt['completed']
        assert isinstance(receipt['error'], error.CallCancelled)
        assert runtime.call_execution_snapshot()['used_operations'] == 0
    finally:
        if stream is not None:
            stream.close()
        if proxy is not None:
            cc.close(proxy)
        assert runtime.shutdown()['completed']


@pytest.mark.parametrize('with_options', [False, True], ids=['original', 'call-options'])
def test_generated_python_stream_client_uses_real_native_ipc(monkeypatch, tmp_path, with_options):
    import importlib.util

    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    generated = cc.contract.compile_artifacts(
        cc.contract.export_descriptor(MatrixPortable).encode(), target='python',
    )
    for artifact in generated.artifacts:
        output = tmp_path / artifact.relative_path
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(artifact.bytes)
    location = tmp_path / 'python/c_two_contract.py'
    spec = importlib.util.spec_from_file_location('generated_stream_test', location)
    module = importlib.util.module_from_spec(spec)
    import sys
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
    finally:
        sys.modules.pop(spec.name)
    runtime = cc.Runtime()
    if os.name != 'nt':
        runtime.set_local_endpoint(root=str(tmp_path))
    source = matrix_payload(False)
    # One receive item fits B; a second server-side B reservation would self-wait.
    runtime.set_call_execution_limits(payload_buffer_budget_bytes=len(source.binary_bytes()) + 1)
    resource = MatrixPythonHost(False)
    proxy = None
    held_stream = None
    try:
        runtime.register(MatrixPortable, resource, name='generated-stream')
        proxy = runtime.connect(MatrixPortable, name='generated-stream', address=runtime.server_address())
        native = proxy.client._client
        if with_options:
            native = native.with_call_options(timeout=None)
        client = module.ContractClient(native)
        client.method_0_ping()
        stream = client.method_1_echo(source)
        for value in stream:
            try:
                inspect_matrix_payload(value, False)
            finally:
                value.close()
        assert stream.close() == {'completed': True, 'error': None, 'complete_items': 2}
        held_stream = client.hold_method_1_echo(source)
        for held in held_stream:
            owner = held.value
            try:
                inspect_matrix_payload(owner, False)
                held_bytes = len(held.unsafe_buffer)
                holds = runtime.memory_stats()['holds']
                assert holds['active_holds'] == 1
                assert holds['total_held_bytes'] == held_bytes
            finally:
                held.release()
            holds = runtime.memory_stats()['holds']
            assert holds['active_holds'] == 0
            assert holds['total_held_bytes'] == 0
            with pytest.raises(PayloadError):
                owner.entry_view(0)
        assert held_stream.close()['complete_items'] == 2
    finally:
        if held_stream is not None:
            held_stream.close(timeout=None)
        source.close()
        if proxy is not None:
            cc.close(proxy)
        assert runtime.shutdown()['completed']
        for value in resource.outputs:
            value.close()


@pytest.fixture(params=[None, "http://127.0.0.1:9"], ids=["relay-unset", "relay-unavailable"])
def direct_values(request, monkeypatch, tmp_path):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server = cc.Runtime()
    client = cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    if request.param is not None:
        client.set_relay_anchor(request.param)
    server.set_server(ipc_overrides={"max_execution_workers": 1})
    resource = ValuesResource()
    server.register(
        StreamValues, resource, name="stream-values",
        concurrency=cc.ConcurrencyConfig(max_workers=1, max_pending=16),
    )
    address = server.server_address()
    assert address is not None and address.startswith("ipc://")
    proxy = client.connect(StreamValues, name="stream-values", address=address)
    assert proxy.client._mode == "ipc"
    try:
        yield server, client, proxy, resource
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


@pytest.mark.parametrize("count", [0, 1, 5])
def test_direct_ipc_natural_items_and_single_terminal(direct_values, count):
    _, client, proxy, resource = direct_values
    stream = proxy.steps(count)
    assert list(stream) == list(range(count))
    assert stream.close() == {
        "completed": True,
        "error": None,
        "complete_items": count,
    }
    assert stream.close(timeout=0)["complete_items"] == count
    assert resource.events == [
        "factory",
        *(f"yield:{index}" for index in range(count)),
        "finally",
    ]
    assert client.call_execution_snapshot()["used_operations"] == 0


@pytest.mark.parametrize("method", ["failed_step", "failed_cleanup"])
def test_direct_ipc_error_after_complete_item_preserves_count(direct_values, method):
    _, _, proxy, resource = direct_values
    stream = getattr(proxy, method)()
    assert next(stream) == 0
    with pytest.raises(error.ResourceExecuteFunction) as raised:
        next(stream)
    assert "failed" in str(raised.value)
    receipt = stream.close()
    assert receipt["completed"] is True
    assert isinstance(receipt["error"], error.ResourceExecuteFunction)
    assert receipt["complete_items"] == 1
    assert resource.events[-1] in ("step-finally", "cleanup-start")


def test_yielded_stream_does_not_occupy_route_worker_or_block_unary(direct_values):
    _, client, proxy, _ = direct_values
    streams = [proxy.steps(100) for _ in range(5)]
    try:
        assert [next(stream) for stream in streams] == [0] * len(streams)
        assert proxy.ping() == "pong"
        assert client.call_execution_snapshot()["used_operations"] >= len(streams)
    finally:
        for stream in streams:
            assert stream.close(timeout=None)["completed"]
    assert client.call_execution_snapshot()["used_operations"] == 0


def test_direct_ipc_stopped_consumer_has_one_queued_and_one_authorized_item(monkeypatch, tmp_path):
    """Section 9, clarified delivery boundary: taking k items permits k + 2 steps."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)

    @cc.crm(namespace="test.stream.stopped-consumer", version="0.1.0")
    class Progress:
        @cc.stream
        @cc.transfer(input=BLOB_SPEC, output=BLOB_SPEC)
        def steps(self, payload: Payload) -> Iterator[Payload]: ...

        @cc.read
        def ping(self) -> None: ...

    taken = 1
    queued_step = threading.Event()
    release_queued_step = threading.Event()
    authorized_step = threading.Event()
    release_authorized_step = threading.Event()
    forbidden_step = threading.Event()
    cleaned = threading.Event()
    steps: list[int] = []
    outputs = [blob_payload(b"x" * size) for size in (16_384, 32_768, 262_144)]
    lengths = [len(output.binary_bytes()) for output in outputs]
    source = blob_payload(b"input")

    class Resource:
        def ping(self) -> None:
            return None

        def steps(self, payload: Payload) -> Iterator[Payload]:
            try:
                for index in range(100):
                    steps.append(index)
                    if index == taken:
                        queued_step.set()
                        assert release_queued_step.wait(5)
                    if index == taken + 1:
                        authorized_step.set()
                        assert release_authorized_step.wait(5)
                    if index == taken + 2:
                        forbidden_step.set()
                    yield outputs[min(index, 2)]
            finally:
                cleaned.set()

    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    server.set_transport_policy(shm_threshold=1024)
    server.set_server(ipc_overrides={"pool_enabled": False, "spill_threshold": 1.0})
    proxy = stream = None
    try:
        server.register(Progress, Resource(), name="stopped-consumer")
        proxy = client.connect(Progress, name="stopped-consumer", address=server.server_address())
        assert proxy.client.observed_path == "direct_ipc"
        stream = proxy.steps(source)
        first = next(stream)
        assert blob_bytes(first) == b"x" * 16_384
        first.close()
        assert queued_step.wait(5)
        release_queued_step.set()
        assert authorized_step.wait(5)
        # Starting step k+2 proves item k+1 entered the capacity-one consumer
        # interface. The third step is still blocked inside the resource.
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == lengths[1]
        assert steps == list(range(taken + 2))
        assert server.memory_stats()["server"]["cells"]["shm"]["peak_bytes"] < lengths[2]
        release_authorized_step.set()
        deadline = time.monotonic() + 5
        while True:
            shm = server.memory_stats()["server"]["cells"]["shm"]
            buffered = client.memory_stats()["payload_buffer"]["used_bytes"]
            if shm["peak_bytes"] >= lengths[2] and shm["used_bytes"] == 0 and buffered == sum(lengths[1:]):
                break
            assert time.monotonic() < deadline, "authorized item was not copied and released"
            proxy.ping()
        # The larger SHM allocation has been copied and its loan released;
        # this is a receive-completion barrier, not just ITEM_BEGIN admission.
        # Item k+1 stays queued, item k+2 waits for that delivery slot, and
        # neither consumption nor a grant for step k+3 has occurred.
        proxy.ping()
        assert not forbidden_step.is_set()
        receipt = stream.close(timeout=None)
        assert receipt["completed"]
        assert isinstance(receipt["error"], error.CallCancelled)
        assert cleaned.is_set()
        assert steps == list(range(taken + 2))
        assert not forbidden_step.is_set()
        assert client.call_execution_snapshot()["used_operations"] == 0
        stream = None  # Drop the unread item already delivered to this handle.
        deadline = time.monotonic() + 5
        while client.memory_stats()["payload_buffer"]["used_bytes"]:
            assert time.monotonic() < deadline
            proxy.ping()
    finally:
        release_queued_step.set()
        release_authorized_step.set()
        if stream is not None:
            stream.close(timeout=0)
        source.close()
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]
        for output in outputs:
            output.close()


@pytest.mark.parametrize("abandon", ["with", "gc"])
def test_direct_ipc_implicit_cancel_does_not_wait_for_step_or_cleanup(monkeypatch, tmp_path, abandon):
    """Section 9: with exit and last-handle GC submit CANCEL without waiting."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    cleaned = threading.Event()

    class InfiniteValues(ValuesResource):
        def gated(self) -> Iterator[int]:
            try:
                yield 0
                self.step_entered.set()
                assert self.release_step.wait(5)
                while True:
                    yield 1
            finally:
                self.cleanup_entered.set()
                assert self.release_cleanup.wait(5)
                cleaned.set()

    resource = InfiniteValues()
    proxy = None
    owners = []
    abandoned = threading.Event()
    failures: list[BaseException] = []

    def discard() -> None:
        try:
            owned = owners.pop()
            if abandon == "with":
                with owned:
                    pass
                owners.append(owned)
            else:
                del owned
                gc.collect()
        except BaseException as exc:
            failures.append(exc)
        finally:
            abandoned.set()

    thread = threading.Thread(target=discard, daemon=True)
    try:
        server.register(StreamValues, resource, name="implicit-cancel")
        proxy = client.connect(StreamValues, name="implicit-cancel", address=server.server_address())
        assert proxy.client.observed_path == "direct_ipc"
        owners.append(proxy.gated())
        reference = weakref.ref(owners[0])
        assert next(owners[0]) == 0
        assert resource.step_entered.wait(5)
        thread.start()
        assert abandoned.wait(5), "implicit cancellation waited for the running resource step"
        thread.join(5)
        assert not thread.is_alive() and not failures
        if abandon == "gc":
            assert reference() is None
        assert not cleaned.is_set()
        assert client.call_execution_snapshot()["used_operations"] == 1
        resource.release_step.set()
        assert resource.cleanup_entered.wait(5), "infinite stream did not receive CANCEL"
        assert client.call_execution_snapshot()["used_operations"] == 1
        assert not cleaned.is_set()
        if owners:
            assert owners[0].close(timeout=0)["completed"] is False
        resource.release_cleanup.set()
        assert cleaned.wait(5)
        if owners:
            receipt = owners[0].close(timeout=None)
            assert receipt["completed"] and isinstance(receipt["error"], error.CallCancelled)
        deadline = time.monotonic() + 5
        while client.call_execution_snapshot()["used_operations"]:
            assert time.monotonic() < deadline
            proxy.ping()
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == 0
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        for stream in owners:
            stream.close(timeout=0)
        if thread.ident is not None:
            thread.join(5)
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


def test_direct_ipc_multiple_holds_deadline_and_cancel_cross_full_budget(monkeypatch, tmp_path):
    """Section 9: full B waits for hold release or deadline; CANCEL/terminal bypass B."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    output = blob_payload(b"b" * 20_000)
    source = blob_payload(b"input")
    encoded_len = len(output.binary_bytes())
    client.set_call_execution_limits(payload_buffer_budget_bytes=2 * encoded_len)
    waiting_item = threading.Event()
    waiting_again = threading.Event()
    cleaned = threading.Event()

    class Resource:
        def steps(self, payload: Payload) -> Iterator[Payload]:
            try:
                for index in range(100):
                    if index == 2:
                        waiting_item.set()
                    if index == 3:
                        waiting_again.set()
                    yield output
            finally:
                cleaned.set()

    proxy = view = stream = None
    holds = []
    delivered = {}
    started = threading.Event()
    finished = threading.Event()

    def take_waiting_item():
        started.set()
        try:
            delivered["value"] = next(stream)
        except BaseException as exc:
            delivered["error"] = exc
        finally:
            finished.set()

    thread = threading.Thread(target=take_waiting_item, daemon=True)
    try:
        server.register(StreamPayloads, Resource(), name="deadline-holds")
        proxy = client.connect(StreamPayloads, name="deadline-holds", address=server.server_address())
        assert proxy.client.observed_path == "direct_ipc"
        view = cc.with_call_options(proxy, timeout=2)
        stream = cc.hold(view.steps)(source)
        holds.extend((next(stream), next(stream)))
        assert waiting_item.wait(5)
        budget = client.memory_stats()["payload_buffer"]
        assert budget["used_bytes"] == budget["limit_bytes"] == 2 * encoded_len
        assert client.memory_stats()["holds"]["active_holds"] == 2
        thread.start()
        assert started.wait(5)
        assert not finished.is_set(), "next crossed B while both holds still owned it"
        holds.pop(0).release()
        thread.join(5)
        assert not thread.is_alive() and "error" not in delivered
        holds.append(delivered["value"])
        assert waiting_again.wait(5)
        assert client.memory_stats()["holds"]["active_holds"] == 2
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == 2 * encoded_len
        with pytest.raises(error.CallDeadlineExceeded) as raised:
            next(stream)
        assert raised.value.transport_phase == "dispatch_uncertain"
        assert not cleaned.is_set(), "caller deadline cancelled the resource"
        assert client.call_execution_snapshot()["used_operations"] == 1
        assert client.memory_stats()["payload_buffer"]["rejected_allocations"] == 0
        receipt = stream.close(timeout=5)
        assert receipt["completed"] and isinstance(receipt["error"], error.CallCancelled)
        assert cleaned.is_set()
        assert client.call_execution_snapshot()["used_operations"] == 0
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == 2 * encoded_len
        for held in holds:
            assert blob_bytes(held.value) == b"b" * 20_000
            held.release()
        assert client.memory_stats()["holds"]["active_holds"] == 0
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == 0
    finally:
        for held in holds:
            held.release()
        if stream is not None:
            stream.close(timeout=0)
        if thread.ident is not None:
            thread.join(5)
        source.close()
        output.close()
        if view is not None:
            cc.close(view)
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


@pytest.mark.parametrize("end", ["shutdown", "unregister"])
def test_server_end_preserves_current_step_then_cancels_with_cause(direct_values, end):
    server, _, proxy, resource = direct_values
    stream = proxy.gated()
    assert next(stream) == 0
    item: dict[str, object] = {}
    close: dict[str, object] = {}

    def advance() -> None:
        try:
            item["value"] = next(stream)
        except BaseException as exc:
            item["error"] = exc

    def end_route() -> None:
        try:
            close["receipt"] = (
                server.shutdown(timeout=0) if end == "shutdown"
                else server.unregister("stream-values")
            )
        except BaseException as exc:
            close["error"] = exc

    advancing = threading.Thread(target=advance, daemon=True)
    ending = threading.Thread(target=end_route, daemon=True)
    _, scheduler, _ = server._ensure_server_bridge().get_local_slot_info("stream-values")
    advancing.start()
    try:
        assert resource.step_entered.wait(5)
        ending.start()
        if end == "shutdown":
            ending.join(5)
            assert not ending.is_alive()
            assert "error" not in close
            assert close["receipt"]["completed"] is False
        # The server records the cancellation asynchronously: shutdown(timeout=0)
        # returns after initiating it, and unregister blocks until the stream
        # ends. Both cancel the route's streams before closing its scheduler, so
        # release the running step only after the route has closed; otherwise
        # the generator may finish on its own first, which is a natural end.
        deadline = time.monotonic() + 5
        while not scheduler.snapshot().closed:
            assert time.monotonic() < deadline, f"{end} did not close the route"
            time.sleep(0.001)
        resource.release_step.set()
        advancing.join(5)
        assert not advancing.is_alive()
        assert item == {"value": 1}
        assert resource.cleanup_entered.wait(5)
        resource.release_cleanup.set()
        with pytest.raises(error.CallCancelled) as raised:
            next(stream)
        assert end in str(raised.value).lower()
        receipt = stream.close(timeout=None)
        assert receipt["completed"] is True
        assert isinstance(receipt["error"], error.CallCancelled)
        assert receipt["complete_items"] == 2
        ending.join(5)
        assert not ending.is_alive()
        assert "error" not in close
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        advancing.join(5)
        if ending.ident is not None:
            ending.join(5)


def test_direct_ipc_borrowed_input_survives_yield_and_cleanup(monkeypatch, tmp_path):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    observed: list[bytes] = []
    saved: list[Payload] = []

    class Resource:
        def steps(self, payload: Payload) -> Iterator[Payload]:
            saved.append(payload)
            try:
                observed.append(blob_bytes(payload))
                yield blob_payload(b"first")
                observed.append(blob_bytes(payload))
                yield blob_payload(b"second")
            finally:
                observed.append(blob_bytes(payload))

    server.register(
        StreamPayloads,
        Resource(),
        name="borrowed-stream",
        input_lifetime={"steps": "borrowed"},
    )
    proxy = client.connect(
        StreamPayloads,
        name="borrowed-stream",
        address=server.server_address(),
    )
    source = blob_payload(b"input")
    stream = None
    outputs: list[Payload] = []
    try:
        stream = proxy.steps(source)
        outputs.append(next(stream))
        assert blob_bytes(outputs[-1]) == b"first"
        assert observed and all(value == b"input" for value in observed)
        outputs.append(next(stream))
        assert blob_bytes(outputs[-1]) == b"second"
        assert len(observed) >= 2
        assert stream.close(timeout=None)["completed"]
        assert observed == [b"input"] * 3
        with pytest.raises(PayloadError) as raised:
            saved[0].entry_view(0)
        assert raised.value.symbol == "VIEW_INVALIDATED"
    finally:
        if stream is not None:
            stream.close(timeout=None)
        for output in outputs:
            output.close()
        source.close()
        cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


def test_hold_saturates_receive_budget_until_item_release(monkeypatch, tmp_path):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    output_bytes = b"x" * 20_000
    sample = blob_payload(output_bytes)
    encoded_len = len(sample.binary_bytes())
    sample.close()
    client.set_call_execution_limits(payload_buffer_budget_bytes=encoded_len * 3 // 2)
    second_entered = threading.Event()

    class Resource:
        def steps(self, payload: Payload) -> Iterator[Payload]:
            for index in range(3):
                if index == 1:
                    second_entered.set()
                yield blob_payload(output_bytes)

    server.register(StreamPayloads, Resource(), name="hold-budget")
    proxy = client.connect(
        StreamPayloads, name="hold-budget", address=server.server_address(),
    )
    source = blob_payload(b"input")
    stream = cc.hold(proxy.steps)(source)
    first = second = third = None
    delivered: dict[str, object] = {}
    finished = threading.Event()

    def take_second() -> None:
        try:
            delivered["value"] = next(stream)
        except BaseException as exc:
            delivered["error"] = exc
        finally:
            finished.set()

    thread = threading.Thread(target=take_second, daemon=True)
    try:
        first = next(stream)
        assert blob_bytes(first.value) == output_bytes
        assert client.memory_stats()["holds"]["active_holds"] == 1
        thread.start()
        assert second_entered.wait(5)
        assert not finished.wait(0.1), "second held item crossed the full B budget"
        first.release()
        thread.join(5)
        assert not thread.is_alive()
        assert "error" not in delivered
        second = delivered["value"]
        assert blob_bytes(second.value) == output_bytes
        second.release()
        third = next(stream)
        assert blob_bytes(third.value) == output_bytes
        third.release()
        with pytest.raises(StopIteration):
            next(stream)
        assert stream.close(timeout=None)["complete_items"] == 3
        assert client.memory_stats()["holds"]["active_holds"] == 0
    finally:
        if first is not None:
            first.release()
        if second is not None:
            second.release()
        if third is not None:
            third.release()
        stream.close(timeout=0)
        thread.join(5) if thread.ident is not None else None
        source.close()
        cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


def test_stream_item_larger_than_receive_budget_fails_without_stalling(monkeypatch, tmp_path):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    server, client = cc.Runtime(), cc.Runtime()
    if os.name != "nt":
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    output = blob_payload(b"x" * 20_000)
    client.set_call_execution_limits(
        payload_buffer_budget_bytes=len(output.binary_bytes()) - 1,
    )

    class Resource:
        def steps(self, payload: Payload) -> Iterator[Payload]:
            yield output

    server.register(StreamPayloads, Resource(), name="oversized-item")
    proxy = client.connect(
        StreamPayloads, name="oversized-item", address=server.server_address(),
    )
    source = blob_payload(b"input")
    stream = None
    try:
        with pytest.raises(error.CallCapacityExceeded):
            stream = proxy.steps(source)
            next(stream)
        assert client.memory_stats()["payload_buffer"]["used_bytes"] == 0
    finally:
        if stream is not None:
            stream.close(timeout=None)
        source.close()
        output.close()
        cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]
        assert server.shutdown(timeout=5)["completed"]


def test_call_deadline_covers_whole_iteration_without_cancelling_resource(direct_values):
    _, _, proxy, resource = direct_values
    view = cc.with_call_options(proxy, timeout=0.2)
    stream = view.gated()
    try:
        assert next(stream) == 0
        assert resource.step_entered.wait(5)
        with pytest.raises(error.CallDeadlineExceeded) as raised:
            next(stream)
        assert raised.value.transport_phase == "dispatch_uncertain"
        assert "finally" not in resource.events
        resource.release_step.set()
        resource.release_cleanup.set()
        receipt = stream.close(timeout=None)
        assert receipt["completed"]
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        stream.close(timeout=0)
        cc.close(view)


def _gil_wait_child(result_queue) -> None:
    """Run native wait/cleanup handshakes outside pytest's own process."""
    server, client = cc.Runtime(), cc.Runtime()
    resource = ValuesResource()
    server.register(StreamValues, resource, name="gil-wait")
    proxy = client.connect(
        StreamValues, name="gil-wait", address=server.server_address(),
    )
    stream = proxy.gated()
    assert next(stream) == 0
    items: list[int] = []
    close: list[dict] = []

    def advance() -> None:
        items.append(next(stream))

    def finish() -> None:
        close.append(stream.close(timeout=None))

    advancing = threading.Thread(target=advance, daemon=True)
    closing = threading.Thread(target=finish, daemon=True)
    try:
        advancing.start()
        assert resource.step_entered.wait(5)
        resource.release_step.set()
        advancing.join(5)
        assert items == [1]
        closing.start()
        assert resource.cleanup_entered.wait(5)
        resource.release_cleanup.set()
        closing.join(5)
        assert not closing.is_alive()
        assert close[0]["completed"]
        assert close[0]["error"] is None or isinstance(
            close[0]["error"], error.CallCancelled,
        )
        result_queue.put("ok")
    finally:
        resource.release_step.set()
        resource.release_cleanup.set()
        cc.close(proxy)
        client.shutdown(timeout=5)
        server.shutdown(timeout=5)


@pytest.mark.timeout(20)
def test_native_iteration_and_close_wait_release_gil_in_subprocess():
    context = multiprocessing.get_context("spawn")
    result_queue = context.Queue()
    process = context.Process(target=_gil_wait_child, args=(result_queue,))
    process.start()
    process.join(15)
    if process.is_alive():
        process.terminate()
        process.join(5)
        pytest.fail("stream native wait kept the GIL or blocked cleanup")
    assert process.exitcode == 0
    assert result_queue.get(timeout=1) == "ok"


@pytest.fixture(scope="module")
def rust_stream_harness(tmp_path_factory):
    return RustHarness.prepare(tmp_path_factory.mktemp("c-two-stream-matrix"))


@pytest.fixture
def private_stream_ipc_root():
    """Contain abrupt listener exits; Windows pipes/leases are kernel-owned."""
    with tempfile.TemporaryDirectory(
        prefix="c2-stream-", dir="/tmp" if os.name != "nt" else None,
    ) as directory:
        root = Path(directory)
        yield root
        if os.name != "nt":
            # Test teardown has waited for each child and completed Runtime
            # shutdown. Reap only this private namespace, never the default.
            with cc.endpoint.sweep_endpoints(root=str(root)) as sweep:
                while True:
                    batch = sweep.next_batch()
                    assert not batch["round_interrupted"], batch
                    assert not batch["io_errors"], batch
                    if batch["round_complete"]:
                        break
        assert not list(root.rglob("*.lease"))
        assert not any(path.is_socket() for path in root.rglob("*"))
    assert not root.exists()


@pytest.mark.timeout(300)
@pytest.mark.parametrize("host_language", ["python", "rust"])
def test_direct_ipc_peer_process_end_preserves_item_and_never_replays(
    rust_stream_harness, private_stream_ipc_root, monkeypatch, host_language,
):
    """Section 9: real peer end is dispatch-uncertain, keeps delivered holds and never replays."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    route = f"stream-peer-end-{uuid.uuid4().hex[:12]}"
    address = f"ipc://c2_stream_peer_end_{os.getpid()}_{uuid.uuid4().hex[:12]}"
    transport = next(case for case in TRANSPORT_CASES if case.name == "shm")
    client = cc.Runtime()
    if os.name != "nt":
        client.set_local_endpoint(root=str(private_stream_ipc_root))
    client.set_transport_policy(shm_threshold=64 * 1024)
    client.set_client(ipc_overrides=transport.overrides())
    host = proxy = source = stream = held = None
    try:
        if host_language == "python":
            host = PythonHostProcess(address, route, private_stream_ipc_root, transport.overrides())
        else:
            host = rust_stream_harness.host(
                address, route, True, True,
                ipc_overrides=transport.overrides(), gated=True, ipc_root=private_stream_ipc_root,
            )
        if os.name != "nt":
            context = cc.endpoint.local_endpoint_context(root=str(private_stream_ipc_root))
            endpoint = Path(context.endpoint_name(address))
            assert endpoint.is_socket()
            assert endpoint.with_suffix(".lease").is_file()
        proxy = client.connect(MatrixPortable, name=route, address=address)
        assert proxy.client.observed_path == "direct_ipc"
        source = matrix_payload(True)
        stream = cc.hold(proxy.echo)(source)
        held = next(stream)
        owner = held.value
        inspect_matrix_payload(owner, True)
        host.wait_blocked()
        # kill()/Process.kill() ends the native peer on Windows and POSIX;
        # no signal suspension or platform-specific substitute is involved.
        remaining = host.abort()
        if host_language == "rust":
            assert host.process.returncode != 0
            assert "CALL calls=" not in remaining, "resource was entered again"
        else:
            assert host.process.exitcode != 0
            assert host.calls.value == 1
        inspect_matrix_payload(owner, True)
        assert client.memory_stats()["holds"]["active_holds"] == 1
        with pytest.raises(error.CCBaseError) as raised:
            next(stream)
        assert raised.value.transport_phase == "dispatch_uncertain"
        receipt = stream.close(timeout=5)
        assert receipt["completed"]
        assert receipt["complete_items"] is None
        assert receipt["error"].transport_phase == "dispatch_uncertain"
        assert client.call_execution_snapshot()["used_operations"] == 0
        inspect_matrix_payload(owner, True)
        held.release()
        with pytest.raises(PayloadError) as released:
            owner.entry_view(0)
        assert released.value.symbol == "VIEW_INVALIDATED"
        stats = client.memory_stats()
        assert stats["holds"]["active_holds"] == 0
        assert stats["holds"]["total_held_bytes"] == 0
        assert stats["payload_buffer"]["used_bytes"] == 0
        for kind in ("shm", "file", "reassembly"):
            assert stats["runtime_outgoing"]["cells"][kind]["used_bytes"] == 0
    finally:
        if host is not None:
            host.abort()
        if held is not None:
            held.release()
        if stream is not None:
            stream.close(timeout=0)
        if source is not None:
            source.close()
        if proxy is not None:
            cc.close(proxy)
        assert client.shutdown(timeout=5)["completed"]


# Carrier choices depend on both language adapters; retaining an item must refund
# the same carriers. Five distinct profiles avoid the old redundant size/threshold
# combinations while checking every adapter and consumption mode (40 rows).
_MATRIX_ROWS = list(product(
    ("python", "rust"),
    ("python", "rust"),
    ("materialized", "hold"),
    TRANSPORT_CASES,
))


@pytest.mark.timeout(300)
@pytest.mark.parametrize(
    ("host_language", "client_language", "mode", "transport"),
    _MATRIX_ROWS,
    ids=["-".join((host, client, mode, case.name))
         for host, client, mode, case in _MATRIX_ROWS],
)
def test_python_rust_direct_ipc_stream_matrix(
    rust_stream_harness, private_stream_ipc_root, monkeypatch,
    host_language, client_language, mode, transport,
):
    """Section 9: both carrier directions and retained owners in all four languages pairs."""
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    route = f"stream-matrix-{uuid.uuid4().hex[:12]}"
    address = f"ipc://c2_stream_matrix_{os.getpid()}_{uuid.uuid4().hex[:12]}"
    threshold = 64 * 1024 if transport.prefer_shm else 1 << 30
    overrides = transport.overrides()
    server = rust_host = resource = client = proxy = source = stream = None
    try:
        if host_language == "python":
            server = cc.Runtime()
            if os.name != "nt":
                server.set_local_endpoint(root=str(private_stream_ipc_root))
            server.set_transport_policy(shm_threshold=threshold)
            server.set_server(
                server_id=address.removeprefix("ipc://"), ipc_overrides=overrides,
            )
            resource = MatrixPythonHost(transport.large)
            resource.observe_input = lambda: server.memory_stats()["server"]
            server.register(MatrixPortable, resource, name=route)
            assert server.server_address() == address
        else:
            rust_host = rust_stream_harness.host(
                address, route, transport.prefer_shm, transport.large,
                ipc_overrides=overrides, ipc_root=private_stream_ipc_root,
            )

        if client_language == "rust":
            receipt = rust_stream_harness.client(
                address, route, transport.prefer_shm, transport.large, mode == "hold",
                ipc_overrides=overrides, ipc_root=private_stream_ipc_root,
            )
            client_stats = receipt["memory"]
            encoded_len = receipt["encoded_len"]
        else:
            client = cc.Runtime()
            if os.name != "nt":
                client.set_local_endpoint(root=str(private_stream_ipc_root))
            client.set_transport_policy(shm_threshold=threshold)
            client.set_client(ipc_overrides=overrides)
            proxy = client.connect(MatrixPortable, name=route, address=address)
            assert proxy.client.observed_path == "direct_ipc"
            source = matrix_payload(transport.large)
            encoded_len = len(source.binary_bytes())
            stream = cc.hold(proxy.echo)(source) if mode == "hold" else proxy.echo(source)
            for _ in range(2):
                item = next(stream)
                if mode == "hold":
                    assert isinstance(item, cc.HeldResult)
                    assert len(item.unsafe_buffer) == encoded_len
                    retained = item.value
                    inspect_matrix_payload(retained, transport.large)
                    item.release()
                    with pytest.raises(PayloadError) as released:
                        retained.entry_view(0)
                    assert released.value.symbol == "VIEW_INVALIDATED"
                else:
                    assert len(item.binary_bytes()) == encoded_len
                    inspect_matrix_payload(item, transport.large)
                    item.close()
            with pytest.raises(StopIteration):
                next(stream)
            assert stream.close() == {
                "completed": True, "error": None, "complete_items": 2,
            }
            # This same-connection round trip follows the queued input/item
            # RELEASE records, so backing refunds are observed without a sleep.
            proxy.ping()
            client_stats = client.memory_stats()
            assert client_stats["holds"]["active_holds"] == 0
            assert client_stats["holds"]["total_held_bytes"] == 0
            assert client.call_execution_snapshot()["used_operations"] == 0

        if resource is not None:
            assert resource.calls == 1
            input_stats = resource.input_stats
            server_stats = server.memory_stats()
        else:
            receipt = rust_host.stats()
            input_stats = receipt["input"]
            server_stats = receipt["memory"]
        assert_transport_stats(transport, encoded_len, input_stats, server_stats, client_stats)
        if rust_host is not None:
            rust_host.stop()
            rust_host = None
    finally:
        if rust_host is not None:
            rust_host.abort()
        if stream is not None:
            stream.close(timeout=0)
        if source is not None:
            source.close()
        if proxy is not None:
            cc.close(proxy)
        if client is not None:
            assert client.shutdown(timeout=5)["completed"]
        if resource is not None:
            resource.close()
        if server is not None:
            assert server.shutdown(timeout=5)["completed"]
