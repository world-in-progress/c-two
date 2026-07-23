from __future__ import annotations

import json
import time

import pytest

import c_two as cc
from c_two import error
from c_two.transport.registry import _ProcessRegistry
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


@pytest.fixture(autouse=True)
def cleanup_runtime():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


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
def test_direct_ipc_open_copy_hold_borrowed_invalidation_and_no_payload() -> None:
    resource = EchoResource()
    cc.register(
        Echo,
        resource,
        name="portable-owner",
        input_lifetime={"save_borrowed": cc.InputLifetime.BORROWED},
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
