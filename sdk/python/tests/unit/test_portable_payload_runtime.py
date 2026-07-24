from __future__ import annotations

import json
from pathlib import Path

import pytest

import c_two as cc
from c_two import error
from c_two.crm.payload_plan import PayloadBinding, PayloadPlanKind, no_payload_binding
from c_two.crm.transferable import _build_transfer_wrapper
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload, PayloadError


RECORD_SPEC = {
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

FASTDB_DIGEST_MISMATCH_CAUSE = json.loads(
    (
        Path(__file__).resolve().parents[4]
        / "tests/fixtures/fastdb-digest-mismatch-cause.json"
    ).read_text(),
)


def build_payload(spec_value: object, value: int = 7) -> Payload:
    spec = CompiledSpec.compile(
        json.dumps(spec_value, separators=(",", ":")).encode(),
    )
    builder = Builder.create(spec)
    if spec_value == OTHER_SPEC:
        builder.entry_begin(0, 1).value_u16(value)
    else:
        builder.entry_begin(0, 1).value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


def payload_value(payload: Payload) -> int:
    with payload.entry_view(0) as values:
        with values.at(0) as value:
            return value.get_u8()


def test_explicit_payload_binding_exports_rust_validated_v2_descriptor() -> None:
    @cc.crm(namespace="test.portable-payload", version="0.1.0")
    class Echo:
        @cc.transfer(input=RECORD_SPEC, output=RECORD_SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    descriptor = json.loads(cc.export_contract_descriptor(Echo))
    reference = json.loads(cc.export_contract_release_ref(Echo))
    from c_two.crm.contract import crm_contract

    route_contract = crm_contract(Echo)
    method = descriptor["methods"][0]

    assert descriptor["schema"] == "c-two.contract.v2"
    assert method["parameters"] == [
        {
            "name": "payload",
            "kind": "POSITIONAL_OR_KEYWORD",
            "default": {"kind": "missing"},
            "type": {"kind": "payload"},
        }
    ]
    assert method["return"] == {"kind": "payload"}
    assert method["bindings"]["input"] == {
        "kind": "fastdb",
        "spec": RECORD_SPEC,
    }
    assert method["bindings"]["output"] == method["bindings"]["input"]
    assert descriptor["fingerprints"] == {
        "abi_hash": route_contract.abi_hash,
        "signature_hash": route_contract.signature_hash,
    }
    assert reference["contract_schema"] == "c-two.contract.v2"
    assert "call-db" not in json.dumps(descriptor)
    assert "PayloadAbiRef" not in json.dumps(descriptor)


def test_explicit_binding_uses_core_digest_send_and_open_copy() -> None:
    @cc.crm(namespace="test.portable-owner", version="0.1.0")
    class Echo:
        @cc.transfer(input=RECORD_SPEC, output=RECORD_SPEC)
        def echo(self, payload: Payload) -> Payload:
            ...

    method = Echo.echo
    binding = method._input_payload_binding
    original = build_payload(RECORD_SPEC)
    binary = binding.serialize(original)
    opened = binding.deserialize(memoryview(binary))

    try:
        assert isinstance(opened, Payload)
        assert opened is not original
        assert opened.binary_bytes() == binary
        assert payload_value(opened) == 7
    finally:
        opened.close()
        original.close()

    wrong = build_payload(OTHER_SPEC)
    try:
        with pytest.raises(PayloadError) as raised:
            binding.serialize(wrong)
        assert raised.value.symbol == "DIGEST_MISMATCH"
        assert raised.value.path == "/payload/spec_sha256"
    finally:
        wrong.close()


def test_explicit_binding_requires_one_payload_annotation_per_bound_direction() -> None:
    with pytest.raises(TypeError, match="exactly one.*Payload"):
        @cc.crm(namespace="test.bad-input-shape", version="0.1.0")
        class BadInputShape:
            @cc.transfer(input=RECORD_SPEC)
            def echo(self, left: Payload, right: Payload) -> None:
                ...

    with pytest.raises(TypeError, match="fastdb4py.payload.Payload"):
        @cc.crm(namespace="test.bad-input-type", version="0.1.0")
        class BadInputType:
            @cc.transfer(input=RECORD_SPEC)
            def echo(self, payload: bytes) -> None:
                ...

    with pytest.raises(TypeError, match="fastdb4py.payload.Payload"):
        @cc.crm(namespace="test.bad-output-type", version="0.1.0")
        class BadOutputType:
            @cc.transfer(output=RECORD_SPEC)
            def echo(self) -> bytes:
                ...

    with pytest.raises(TypeError, match="must be positional"):
        @cc.crm(namespace="test.bad-keyword-only-input", version="0.1.0")
        class BadKeywordOnlyInput:
            @cc.transfer(input=RECORD_SPEC)
            def echo(self, *, payload: Payload) -> None:
                ...


def test_python_pickle_remains_runtime_only_and_portable_export_rejects_it() -> None:
    @cc.crm(namespace="test.python-only", version="0.1.0")
    class PythonOnly:
        def echo(self, value: int) -> int:
            ...

    with pytest.raises(ValueError, match="python-pickle-default"):
        cc.export_contract_descriptor(PythonOnly)


def test_malformed_nested_spec_is_rejected_by_fastdb_core() -> None:
    with pytest.raises(PayloadError) as raised:
        @cc.crm(namespace="test.bad-fastdb-spec", version="0.1.0")
        class BadSpec:
            @cc.transfer(input={"schema": "not-fastdb"})
            def echo(self, payload: Payload) -> None:
                ...

    assert raised.value.symbol
    assert raised.value.path


def test_binding_clones_compiled_spec_and_accepts_json_text_and_bytes() -> None:
    spec_json = json.dumps(RECORD_SPEC, separators=(",", ":"))
    source_spec = CompiledSpec.compile(spec_json.encode())

    @cc.crm(namespace="test.portable-spec-sources", version="0.1.0")
    class Echo:
        @cc.transfer(input=source_spec, output=spec_json.encode())
        def echo(self, payload: Payload) -> Payload:
            ...

    source_spec.close()
    descriptor = json.loads(cc.export_contract_descriptor(Echo))
    binding = Echo.echo._input_payload_binding
    original = build_payload(RECORD_SPEC)
    opened = binding.deserialize(binding.serialize(original))
    try:
        assert descriptor["methods"][0]["bindings"]["input"]["spec"] == RECORD_SPEC
        assert descriptor["methods"][0]["bindings"]["output"]["spec"] == RECORD_SPEC
        assert payload_value(opened) == 7
    finally:
        opened.close()
        original.close()


def test_exported_descriptor_and_authoring_source_do_not_alias_binding_spec() -> None:
    authoring_spec = json.loads(json.dumps(RECORD_SPEC))

    @cc.crm(namespace="test.portable-spec-copy", version="0.1.0")
    class Echo:
        @cc.transfer(input=authoring_spec)
        def echo(self, payload: Payload) -> None:
            ...

    authoring_spec["entries"][0]["id"] = "mutated-authoring-source"
    exported = json.loads(cc.export_contract_descriptor(Echo))
    exported["methods"][0]["bindings"]["input"]["spec"]["entries"][0][
        "id"
    ] = "mutated-export"
    exported_again = json.loads(cc.export_contract_descriptor(Echo))

    assert exported_again["methods"][0]["bindings"]["input"]["spec"] == RECORD_SPEC


class _FailingReleaseResponse(bytearray):
    def __init__(self, source: bytes) -> None:
        super().__init__(source)
        self.release_calls = 0

    def release(self) -> None:
        self.release_calls += 1
        raise RuntimeError("transport release failed")


def _fake_fastdb_binding(
    *,
    deserialize,
    invalidate=lambda value: None,
) -> PayloadBinding:
    return PayloadBinding(
        kind=PayloadPlanKind.FASTDB,
        serialize=lambda value: bytes(value),
        deserialize=deserialize,
        invalidate=invalidate,
        spec={"schema": "test-only"},
        spec_sha256=b"\x00" * 32,
        compiled_spec=object(),
        label="test.fake-fastdb",
    )


@pytest.mark.parametrize(
    ("held", "expected_error"),
    [
        (False, error.ClientDeserializeOutput),
        (True, error.ClientOutputFromBuffer),
    ],
)
def test_decode_error_remains_primary_when_response_cleanup_also_fails(
    held: bool,
    expected_error: type[error.CCError],
) -> None:
    response = _FailingReleaseResponse(b"payload")

    class Client:
        supports_direct_call = False

        def call(self, method_name: str, payload: object) -> object:
            assert method_name == "fetch"
            assert payload is None
            return response

    class Contract:
        direction = "->"
        client = Client()

    def fetch(self) -> object:
        ...

    def fail_decode(_source: object) -> object:
        raise RuntimeError("decode failed")

    wrapped = _build_transfer_wrapper(
        fetch,
        input=no_payload_binding(),
        output=_fake_fastdb_binding(deserialize=fail_decode),
    )

    kwargs = {"_c2_buffer": "hold"} if held else {}
    with pytest.raises(expected_error) as raised:
        wrapped(Contract(), **kwargs)

    assert "decode failed" in raised.value.message
    assert "transport release failed" not in raised.value.message
    assert response.release_calls == 1


def test_post_decode_release_failure_invalidates_unreturned_owner() -> None:
    response = _FailingReleaseResponse(b"payload")
    owner = object()
    invalidated: list[object] = []

    class Client:
        supports_direct_call = False

        def call(self, method_name: str, payload: object) -> object:
            return response

    class Contract:
        direction = "->"
        client = Client()

    def fetch(self) -> object:
        ...

    wrapped = _build_transfer_wrapper(
        fetch,
        input=no_payload_binding(),
        output=_fake_fastdb_binding(
            deserialize=lambda source: owner,
            invalidate=invalidated.append,
        ),
    )

    with pytest.raises(error.ClientDeserializeOutput, match="transport release failed"):
        wrapped(Contract())

    assert invalidated == [owner]
    assert response.release_calls == 1


def test_retained_tracking_failure_invalidates_owner_then_releases_response() -> None:
    events: list[str] = []
    owner = object()

    class Response(bytearray):
        def track_retained(self, *args: object) -> None:
            events.append("track")
            raise RuntimeError("tracking failed")

        def release(self) -> None:
            events.append("release")

    response = Response(b"payload")

    class Client:
        supports_direct_call = False
        lease_tracker = object()
        route_name = "route"

        def call(self, method_name: str, payload: object) -> object:
            return response

    class Contract:
        direction = "->"
        client = Client()

    def fetch(self) -> object:
        ...

    wrapped = _build_transfer_wrapper(
        fetch,
        input=no_payload_binding(),
        output=_fake_fastdb_binding(
            deserialize=lambda source: owner,
            invalidate=lambda value: events.append("invalidate"),
        ),
    )

    with pytest.raises(error.ClientOutputFromBuffer, match="tracking failed"):
        wrapped(Contract(), _c2_buffer="hold")

    assert events == ["track", "invalidate", "release"]


def test_client_payload_keyword_is_normalized_instead_of_discarded() -> None:
    sent: list[object] = []

    class Client:
        supports_direct_call = False

        def call(self, method_name: str, payload: object) -> bytes:
            assert method_name == "send"
            sent.append(payload)
            return b""

    class Contract:
        direction = "->"
        client = Client()

    def send(self, payload: object) -> None:
        ...

    wrapped = _build_transfer_wrapper(
        send,
        input=_fake_fastdb_binding(deserialize=lambda source: source),
        output=no_payload_binding(),
    )

    assert wrapped(Contract(), payload=b"keyword") is None
    assert sent == [b"keyword"]


def test_fastdb_cause_fields_survive_client_input_error_wrapping() -> None:
    @cc.crm(namespace="test.portable-cause", version="0.1.0")
    class Echo:
        @cc.transfer(input=RECORD_SPEC)
        def echo(self, payload: Payload) -> None:
            ...

    class Client:
        supports_direct_call = False

        def call(self, method_name: str, payload: object) -> bytes:
            raise AssertionError("digest mismatch must fail before transport")

    class Contract:
        direction = "->"
        client = Client()

    wrong = build_payload(OTHER_SPEC)
    try:
        with pytest.raises(error.ClientSerializeInput) as raised:
            Echo.echo(Contract(), wrong)
    finally:
        wrong.close()

    assert raised.value.details == FASTDB_DIGEST_MISMATCH_CAUSE


def test_missing_payload_is_classified_at_resource_deserialize_boundary() -> None:
    decoded_sources: list[object] = []

    def deserialize(source: object) -> object:
        decoded_sources.append(source)
        raise TypeError("portable payload bytes are missing")

    class Resource:
        def echo(self, payload: object) -> None:
            raise AssertionError("resource method must not run")

    class Contract:
        direction = "<-"
        resource = Resource()

    def echo(self, payload: object) -> None:
        ...

    wrapped = _build_transfer_wrapper(
        echo,
        input=_fake_fastdb_binding(deserialize=deserialize),
        output=no_payload_binding(),
    )

    error_bytes, result = wrapped(Contract())
    restored = error.CCError.deserialize(error_bytes)

    assert decoded_sources == [None]
    assert result == b""
    assert isinstance(restored, error.ResourceDeserializeInput)
    assert "portable payload bytes are missing" in restored.message


def test_non_fastdb_error_fields_are_not_misattributed_to_fastdb() -> None:
    class DomainError(RuntimeError):
        def __init__(self) -> None:
            super().__init__("domain rejected input")
            self.code = 41
            self.symbol = "DOMAIN_REJECTED"
            self.path = "/domain/input"
            self.message = "domain rejected input"
            self.details_json = '{"owner":"domain"}'

    def serialize(_value: object) -> bytes:
        raise DomainError()

    binding = PayloadBinding(
        kind=PayloadPlanKind.FASTDB,
        serialize=serialize,
        deserialize=lambda source: source,
        invalidate=lambda value: None,
        spec={"schema": "test-only"},
        spec_sha256=b"\x00" * 32,
        compiled_spec=object(),
        label="test.non-fastdb-error",
    )

    class Client:
        supports_direct_call = False

        def call(self, method_name: str, payload: object) -> bytes:
            raise AssertionError("domain rejection must fail before transport")

    class Contract:
        direction = "->"
        client = Client()

    def send(self, payload: object) -> None:
        ...

    wrapped = _build_transfer_wrapper(
        send,
        input=binding,
        output=no_payload_binding(),
    )

    with pytest.raises(error.ClientSerializeInput) as raised:
        wrapped(Contract(), object())

    assert raised.value.details == {}
