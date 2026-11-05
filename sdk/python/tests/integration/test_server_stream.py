from __future__ import annotations

import os
import multiprocessing
import threading
import uuid
from collections.abc import Iterator
from itertools import product

import pytest

import c_two as cc
from c_two import error
from fastdb4py.payload import Payload, PayloadError
from tests.fixtures.stream_python import (
    StreamPayloads,
    StreamValues,
    ValuesResource,
    blob_bytes,
    blob_payload,
)
from tests.fixtures.stream_matrix import (
    Portable as MatrixPortable,
    PythonHost as MatrixPythonHost,
    RustHarness,
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
    advancing.start()
    try:
        assert resource.step_entered.wait(5)
        ending.start()
        if end == "shutdown":
            ending.join(5)
            assert not ending.is_alive()
            assert "error" not in close
            assert close["receipt"]["completed"] is False
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


_MATRIX_ROWS = list(product(
    ("python", "rust"),  # host
    ("python", "rust"),  # client
    ("materialized", "hold"),
    (False, True),  # SHM disabled/enabled
    (False, True),  # inline/large
))


@pytest.mark.timeout(300)
@pytest.mark.parametrize(
    ("host_language", "client_language", "mode", "shm", "large"),
    _MATRIX_ROWS,
    ids=["-".join((host, client, mode, "shm" if shm else "no-shm", "large" if large else "inline"))
         for host, client, mode, shm, large in _MATRIX_ROWS],
)
def test_python_rust_direct_ipc_stream_matrix(
    rust_stream_harness, monkeypatch,
    host_language, client_language, mode, shm, large,
):
    monkeypatch.delenv("C2_RELAY_ANCHOR_ADDRESS", raising=False)
    route = f"stream-matrix-{uuid.uuid4().hex[:12]}"
    address = f"ipc://c2_stream_matrix_{os.getpid()}_{uuid.uuid4().hex[:12]}"
    threshold = 64 * 1024 if shm else 1 << 30
    server = None
    rust_host = None
    resource = None
    client = None
    proxy = None
    source = None
    try:
        if host_language == "python":
            server = cc.Runtime()
            server.set_transport_policy(shm_threshold=threshold)
            server.set_server(server_id=address.removeprefix("ipc://"))
            resource = MatrixPythonHost(large)
            server.register(MatrixPortable, resource, name=route)
            assert server.server_address() == address
        else:
            rust_host = rust_stream_harness.host(address, route, shm, large)

        if client_language == "rust":
            rust_stream_harness.client(address, route, shm, large, mode == "hold")
        else:
            client = cc.Runtime()
            client.set_transport_policy(shm_threshold=threshold)
            proxy = client.connect(MatrixPortable, name=route, address=address)
            assert proxy.client.observed_path == "direct_ipc"
            source = matrix_payload(large)
            stream = (
                cc.hold(proxy.echo)(source) if mode == "hold" else proxy.echo(source)
            )
            for _ in range(2):
                item = next(stream)
                if mode == "hold":
                    assert isinstance(item, cc.HeldResult)
                    retained = item.value
                    inspect_matrix_payload(retained, large)
                    item.release()
                    with pytest.raises(PayloadError) as released:
                        retained.entry_view(0)
                    assert released.value.symbol == "VIEW_INVALIDATED"
                else:
                    inspect_matrix_payload(item, large)
                    item.close()
            with pytest.raises(StopIteration):
                next(stream)
            assert stream.close() == {
                "completed": True, "error": None, "complete_items": 2,
            }
            assert client.memory_stats()["holds"]["active_holds"] == 0
            assert client.call_execution_snapshot()["used_operations"] == 0
            outgoing = client.memory_stats()["runtime_outgoing"]["cells"]
            assert outgoing["reassembly"]["used_bytes"] == 0
            if large and shm:
                assert outgoing["shm"]["peak_bytes"] > 0

        if resource is not None:
            assert resource.calls == 1
        if rust_host is not None:
            rust_host.stop()
            rust_host = None
    finally:
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
        if rust_host is not None:
            rust_host.abort()
