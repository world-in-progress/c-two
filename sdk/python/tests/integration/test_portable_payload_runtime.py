from __future__ import annotations

import json
import os
import threading
import time

import pytest

import c_two as cc
from c_two import error
from c_two.transport import registry as registry_module
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload, PayloadError


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
def test_direct_ipc_open_copy_hold_borrowed_invalidation_and_no_payload() -> None:
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

        assert proxy.ping() is None

        with pytest.raises(error.ResourceSerializeOutput) as wrong_output_error:
            proxy.wrong_output()
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
