from __future__ import annotations

import json
import os
import threading
import time
from collections.abc import Iterator

import pytest

import c_two as cc
from c_two import error
from c_two.transport import registry as registry_module
from fastdb4py.payload import BuildPlan, BuildPolicy, Builder, CompiledSpec, Payload, PayloadError

from ..fixtures.stream_python import BLOB_SPEC, blob_bytes


SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u8", "nullable": False},
        }
    ],
    "components": [],
}

OTHER_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {
            "id": "value",
            "cardinality": "one",
            "type": {"kind": "u16", "nullable": False},
        }
    ],
    "components": [],
}


@cc.crm(namespace="test.portable-runtime", version="0.1.0")
class Echo:
    @cc.transfer(input=SPEC, output=SPEC)
    def echo(self, payload: Payload) -> Payload:
        ...

    @cc.transfer(input=SPEC, output=SPEC)
    def save_borrowed(self, payload: Payload) -> Payload:
        ...

    def ping(self) -> None:
        ...

    @cc.transfer(output=SPEC)
    def wrong_output(self) -> Payload:
        ...


class EchoResource:
    def __init__(self) -> None:
        self.saved: Payload | None = None
        self.outputs: list[Payload] = []

    def echo(self, payload: Payload) -> Payload:
        return payload

    def save_borrowed(self, payload: Payload) -> Payload:
        self.saved = payload
        return payload

    def ping(self) -> None:
        return None

    def wrong_output(self) -> Payload:
        payload = build_payload(9, spec_value=OTHER_SPEC)
        self.outputs.append(payload)
        return payload


@cc.crm(namespace='test.portable-runtime-retirement', version='0.1.0')
class RetiringEcho:
    @cc.transfer(input=SPEC)
    def consume(self, payload: Payload) -> None: ...

    @cc.transfer(input=SPEC, output=SPEC)
    def echo(self, payload: Payload) -> Payload: ...

    @cc.on_shutdown
    def stopped(self) -> None: ...


class RetiringEchoResource:
    def __init__(self) -> None:
        self.entered = threading.Event()
        self.verify_after_pending = threading.Event()
        self.verified_after_pending = threading.Event()
        self.proceed = threading.Event()
        self.finished = threading.Event()
        self.saved_input: Payload | None = None
        self.value_while_pending: int | None = None
        self.value_after_pending_shutdown: int | None = None
        self.events: list[str] = []

    def consume(self, payload: Payload) -> None:
        self.saved_input = payload
        self.entered.set()
        try:
            if not self.verify_after_pending.wait(timeout=10):
                raise RuntimeError('borrowed input was not verified after shutdown began')
            self.value_while_pending = read_value(payload)
            self.verified_after_pending.set()
            if not self.proceed.wait(timeout=10):
                raise RuntimeError('borrowed callback was not released')
            self.value_after_pending_shutdown = read_value(payload)
        finally:
            self.events.append('callback_finished')
            self.finished.set()

    def echo(self, payload: Payload) -> Payload:
        return payload

    def stopped(self) -> None:
        assert self.saved_input is not None
        with pytest.raises(PayloadError) as borrowed_error:
            self.saved_input.entry_view(0)
        assert borrowed_error.value.symbol == 'VIEW_INVALIDATED'
        self.events.append('shutdown_hook')


@pytest.fixture(autouse=True)
def cleanup_runtime():
    yield
    cc.shutdown()
    registry_module._default_runtime = None


def build_payload(value: int = 7, *, spec_value: object = SPEC) -> Payload:
    spec = CompiledSpec.compile(
        json.dumps(spec_value, separators=(",", ":")).encode(),
    )
    builder = Builder.create(spec)
    value_builder = builder.entry_begin(0, 1)
    if spec_value == OTHER_SPEC:
        value_builder.value_u16(value)
    else:
        value_builder.value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


def read_value(payload: Payload) -> int:
    with payload.entry_view(0) as values:
        with values.at(0) as value:
            return value.get_u8()


@pytest.mark.timeout(30)
def test_runtime_retirement_preserves_borrowed_callback_and_obtained_hold(
    tmp_path, monkeypatch,
) -> None:
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    server = cc.Runtime()
    client = cc.Runtime()
    if os.name != 'nt':
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    resource = RetiringEchoResource()
    source = build_payload()
    proxy = None
    held = None
    retained = None
    call_thread = None
    call_result: dict[str, object] = {}
    try:
        server.register(
            RetiringEcho, resource, name='retiring-echo',
            input_lifetime={'consume': 'borrowed'},
        )
        proxy = client.connect(
            RetiringEcho, name='retiring-echo', address=server.server_address(),
        )
        held = cc.hold(proxy.echo)(source)
        retained = held.value
        assert read_value(retained) == 7

        def call_borrowed() -> None:
            try:
                call_result['value'] = proxy.consume(source)
            except BaseException as exc:
                call_result['error'] = exc

        call_thread = threading.Thread(target=call_borrowed, daemon=True)
        call_thread.start()
        assert resource.entered.wait(timeout=5), 'borrowed callback did not start'
        assert resource.saved_input is not None

        pending = server.shutdown(timeout=0.05)
        assert pending['completed'] is False, pending
        assert resource.events == []
        resource.verify_after_pending.set()
        assert resource.verified_after_pending.wait(timeout=5)
        assert resource.value_while_pending == 7
        assert resource.events == []
        assert read_value(retained) == 7

        resource.proceed.set()
        call_thread.join(timeout=5)
        assert not call_thread.is_alive(), 'borrowed call did not finish'
        assert resource.finished.wait(timeout=5), 'borrowed callback did not finish'
        assert call_result == {'value': None}, call_result
        assert resource.value_after_pending_shutdown == 7
        assert resource.events == ['callback_finished']
        with pytest.raises(PayloadError) as borrowed_error:
            resource.saved_input.entry_view(0)
        assert borrowed_error.value.symbol == 'VIEW_INVALIDATED'

        retired_client = client.shutdown(timeout=5)
        assert retired_client['completed'] is True, retired_client
        assert read_value(retained) == 7

        completed = server.shutdown(timeout=5)
        assert completed['completed'] is True, completed
        assert resource.events == ['callback_finished', 'shutdown_hook']
        assert read_value(retained) == 7
        held.release()
        with pytest.raises(PayloadError) as held_error:
            retained.entry_view(0)
        assert held_error.value.symbol == 'VIEW_INVALIDATED'
    finally:
        resource.verify_after_pending.set()
        resource.proceed.set()
        if call_thread is not None and call_thread.ident is not None:
            call_thread.join(timeout=5)
        if held is not None:
            held.release()
        if proxy is not None:
            cc.close(proxy)
        client.shutdown(timeout=5)
        server.shutdown(timeout=5)
        source.close()


@pytest.mark.timeout(30)
def test_direct_ipc_external_payload_hold_borrowed_invalidation_and_no_payload() -> None:
    resource = EchoResource()
    cc.register(
        Echo,
        resource,
        name="portable-owner",
        input_lifetime={"save_borrowed": 'borrowed'},
    )
    time.sleep(0.2)
    address = cc.server_address()
    assert address is not None

    proxy = cc.connect(Echo, name="portable-owner", address=address)
    source = build_payload()
    try:
        ordinary = proxy.echo(source)
        assert proxy.client._mode == "ipc"  # noqa: SLF001
        assert isinstance(ordinary, Payload)
        assert ordinary is not source
        assert read_value(ordinary) == 7

        held = cc.hold(proxy.echo)(source)
        retained = held.value
        assert isinstance(retained, Payload)
        assert read_value(retained) == 7
        held.release()
        with pytest.raises(PayloadError) as held_error:
            retained.entry_view(0)
        assert held_error.value.symbol == "VIEW_INVALIDATED"

        borrowed_response = proxy.save_borrowed(source)
        assert read_value(borrowed_response) == 7
        assert resource.saved is not None
        with pytest.raises(PayloadError) as borrowed_error:
            resource.saved.entry_view(0)
        assert borrowed_error.value.symbol == "VIEW_INVALIDATED"

        source.invalidate()
        assert cc.observe.memory_stats()['client']['memory']['used_bytes'] == 0
        assert read_value(ordinary) == 7
        assert proxy.ping() is None

        with pytest.raises(error.SerializationError) as wrong_output_error:
            proxy.wrong_output()
        assert wrong_output_error.value.code == error.ErrorCode.ResourceOutputSerializing
        assert wrong_output_error.value.stage == 'serialize'
        assert wrong_output_error.value.details["cause_owner"] == "fastdb"
        assert wrong_output_error.value.details["fastdb_code"] == "3006"
        assert wrong_output_error.value.details["fastdb_symbol"] == "DIGEST_MISMATCH"
        assert wrong_output_error.value.details["fastdb_path"] == (
            "/payload/spec_sha256"
        )
        assert json.loads(
            wrong_output_error.value.details["fastdb_details_json"],
        )["reason"] == "spec_digest_mismatch"

        ordinary.close()
        borrowed_response.close()
    finally:
        source.close()
        cc.close(proxy)
        for output in resource.outputs:
            output.close()


@pytest.mark.parametrize('stage', ['resource', 'serialize'])
def test_ipc_borrowed_input_invalidates_once_on_cancellation(stage, monkeypatch):
    from asyncio import CancelledError

    class CancellingEcho(EchoResource):
        def save_borrowed(self, payload: Payload) -> Payload:
            self.saved = payload
            if stage == 'resource':
                raise CancelledError('resource cancelled')
            return payload

    resource = CancellingEcho()
    invalidated = []
    memoryview = Payload.memoryview
    invalidate = Payload.invalidate

    def cancel_serialization(payload):
        if payload is resource.saved:
            assert read_value(payload) == 7
            raise CancelledError('serialization cancelled')
        return memoryview(payload)

    def record_invalidation(payload):
        if payload is resource.saved:
            invalidated.append(payload)
        invalidate(payload)

    monkeypatch.setattr(Payload, 'memoryview', cancel_serialization)
    monkeypatch.setattr(Payload, 'invalidate', record_invalidation)
    cc.register(Echo, resource, name='borrowed-cancellation',
                input_lifetime={'save_borrowed': 'borrowed'})
    proxy = cc.connect(Echo, name='borrowed-cancellation', address=cc.server_address())
    source = build_payload()
    try:
        failure_type, expected_code = (
            (error.ResourceExecutionError, error.ErrorCode.ResourceFunctionExecuting)
            if stage == 'resource'
            else (error.SerializationError, error.ErrorCode.ResourceOutputSerializing)
        )
        with pytest.raises(failure_type, match='cancelled') as caught:
            proxy.save_borrowed(source)
        assert caught.value.code == expected_code
        assert len(invalidated) == 1 and invalidated[0] is resource.saved
        with pytest.raises(PayloadError) as failure:
            resource.saved.entry_view(0)
        assert failure.value.symbol == 'VIEW_INVALIDATED'
        assert cc.observe.memory_stats()['holds']['active_holds'] == 0
        assert read_value(source) == 7
    finally:
        source.close()
        cc.close(proxy)
        if resource.saved is not None:
            resource.saved.close()


def test_held_native_invalidation_failure_has_output_buffer_stage(monkeypatch):
    primary = error.ResourceClosed('invalidation refused', transport_phase='pre_dispatch')

    def fail(_payload):
        raise primary

    cc.register(Echo, EchoResource(), name='held-invalidation')
    proxy = cc.connect(Echo, name='held-invalidation', address=cc.server_address())
    source = build_payload()
    held = None
    try:
        held = cc.hold(proxy.echo)(source)
        retained = held.value
        monkeypatch.setattr(Payload, 'invalidate', fail)
        with pytest.raises(error.SerializationError) as caught:
            held.release()
        assert caught.value.code == error.ErrorCode.ClientOutputFromBuffer
        assert caught.value.transport_phase is None
        assert caught.value.__cause__ is primary
        assert cc.observe.memory_stats()['holds']['active_holds'] == 0
        with pytest.raises(RuntimeError, match='no longer accessible'):
            _ = held.value
        held.release()
        retained.close()
    finally:
        if held is not None:
            held.release()
        source.close()
        cc.close(proxy)


@cc.crm(namespace='test.deferred-portable-runtime', version='0.1.0')
class BlobExchange:
    @cc.transfer(input=BLOB_SPEC, output=BLOB_SPEC)
    def echo(self, payload: Payload) -> Payload: ...

    @cc.transfer(output=BLOB_SPEC)
    def build(self, wrong: bool) -> Payload: ...

    @cc.stream
    @cc.transfer(input=BLOB_SPEC, output=BLOB_SPEC)
    def frames(self, payload: Payload) -> Iterator[Payload]: ...

    @cc.topic
    @cc.transfer(output=BLOB_SPEC)
    def state(self) -> Payload: ...


def blob_plan(data: bytes, *, wrong=False) -> BuildPlan:
    field = BLOB_SPEC['entries'][0]
    spec = BLOB_SPEC if not wrong else {**BLOB_SPEC, 'entries': [{**field, 'id': 'other'}]}
    with CompiledSpec.compile(json.dumps(spec).encode()) as compiled:
        with Builder.create(compiled) as builder:
            builder.entry_begin(0, 1).value_bytes(data)
            return builder.freeze()


class BlobExchangeResource:
    def __init__(self, data):
        self.data = data

    def echo(self, payload):
        return payload

    def build(self, wrong):
        return blob_plan(self.data, wrong=wrong)

    def frames(self, payload):
        yield payload
        yield self.build(False)


def test_ipc_deferred_plan_writes_requests_responses_streams_and_topics(tmp_path, monkeypatch):
    from c_two.crm.payload_plan import _DeferredFastDbPlan

    data = b'c' * (64 * 1024)
    server = cc.Runtime()
    client = cc.Runtime()
    if os.name != 'nt':
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    writes = []
    write_slab = _DeferredFastDbPlan._c2_write_slab

    def record_write(plan, target):
        writes.append((plan.label, type(target).__name__))
        return write_slab(plan, target)

    monkeypatch.setattr(_DeferredFastDbPlan, '_c2_write_slab', record_write)
    proxy = None
    try:
        registration = server.register(BlobExchange, BlobExchangeResource(data), name='blob')
        proxy = client.connect(BlobExchange, name='blob', address=server.server_address())
        with blob_plan(data) as plan, proxy.echo(plan) as received:
            assert blob_bytes(received) == data
        assert ('echo.input', 'PyWritableSlab') in writes

        with pytest.raises(error.SerializationError) as mismatch:
            proxy.build(True)
        assert mismatch.value.code == error.ErrorCode.ResourceOutputSerializing
        assert mismatch.value.details['fastdb_symbol'] == 'DIGEST_MISMATCH'
        with proxy.build(False) as received:
            assert blob_bytes(received) == data
        assert writes.count(('build.output', 'PyWritableSlab')) == 2

        with blob_plan(data) as plan, proxy.frames(plan) as stream:
            with next(stream) as original, next(stream) as from_plan:
                assert blob_bytes(original) == blob_bytes(from_plan) == data
            assert next(stream, None) is None
        assert ('frames.output', 'PyScopedWritableBuffer') in writes

        publisher = registration.publisher('state')
        subscription = proxy.client.subscribe('state')
        try:
            with blob_plan(data, wrong=True) as plan:
                with pytest.raises(error.SerializationError) as mismatch:
                    publisher.publish(plan)
                assert mismatch.value.details['fastdb_symbol'] == 'DIGEST_MISMATCH'
            with blob_plan(data) as plan:
                seq = publisher.publish(plan)
            assert seq == 1
            delivery = next(subscription)
            with delivery.value as received:
                assert delivery.seq == seq and blob_bytes(received) == data
        finally:
            subscription.close(timeout=5)
        assert writes.count(('state.topic.output', 'PyWritableSlab')) == 2
    finally:
        if proxy is not None:
            cc.close(proxy)
        client.shutdown(timeout=5)
        server.shutdown(timeout=5)


@pytest.mark.parametrize('held', [False, True])
def test_ipc_fastdb_stream_and_topic_release_pins_and_accounting(tmp_path, held):
    data = b'payload'
    server = cc.Runtime()
    client = cc.Runtime()
    if os.name != 'nt':
        server.set_local_endpoint(root=str(tmp_path))
        client.set_local_endpoint(root=str(tmp_path))
    proxy = None

    def verify(delivered):
        memory = client.memory_stats()
        if held:
            assert memory['holds']['active_holds'] == 1
            payload = delivered.value
            assert blob_bytes(payload) == data
            delivered.release()
            with pytest.raises(PayloadError) as invalidated:
                payload.entry_view(0)
            assert invalidated.value.symbol == 'VIEW_INVALIDATED'
        else:
            assert memory['holds']['active_holds'] == 0
            assert memory['client']['memory']['used_bytes'] == 0
            with delivered:
                assert blob_bytes(delivered) == data

    try:
        class EchoStream(BlobExchangeResource):
            def frames(self, payload: Payload):
                yield payload

        registration = server.register(
            BlobExchange, EchoStream(data), name='pins',
            input_lifetime={'frames': 'borrowed'},
        )
        proxy = client.connect(BlobExchange, name='pins', address=server.server_address())
        with blob_plan(data) as plan, plan.execute(BuildPolicy.ALLOW_STAGING).payload as source:
            frames = cc.hold(proxy.frames) if held else proxy.frames
            with frames(source) as stream:
                source.invalidate()
                verify(next(stream))
                assert next(stream, None) is None

        publisher = registration.publisher('state')
        sub = proxy.client.subscribe('state', hold=held)
        try:
            with blob_plan(data) as plan, plan.execute(BuildPolicy.ALLOW_STAGING).payload as source:
                publisher.publish(source)
                source.invalidate()
                verify(next(sub).value)
        finally:
            sub.close(timeout=5)
        assert client.memory_stats()['holds']['active_holds'] == 0
        assert client.memory_stats()['client']['memory']['used_bytes'] == 0
    finally:
        if proxy is not None:
            cc.close(proxy)
        client.shutdown(timeout=5)
        server.shutdown(timeout=5)
