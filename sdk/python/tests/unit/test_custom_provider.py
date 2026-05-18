import dataclasses
import json

import pytest

import c_two as cc
from c_two.crm.descriptor import build_contract_descriptor


@pytest.fixture(autouse=True)
def clear_codec_registry():
    from c_two.crm.codec import _clear_codec_registry_for_tests

    _clear_codec_registry_for_tests()
    try:
        yield
    finally:
        _clear_codec_registry_for_tests()


def test_custom_record_provider_generates_codec_and_round_trips():
    from c_two.providers import custom

    @custom.record(
        id='org.example.point.custom',
        version='1',
        schema_text='{"fields":["x","y"],"kind":"point"}',
        media_type='application/vnd.example.point',
    )
    class Point:
        x: int
        y: int

        def serialize(value: 'Point') -> bytes:
            return f'{value.x},{value.y}'.encode()

        def deserialize(data: bytes | memoryview) -> 'Point':
            x, y = bytes(data).decode().split(',')
            return Point(int(x), int(y))

    assert dataclasses.is_dataclass(Point)

    provider = custom.CustomCodecProvider()
    binding = provider.candidates_for_type(Point)

    assert binding is not None
    assert binding.codec_ref.id == 'org.example.point.custom'
    assert binding.codec_ref.version == '1'
    assert binding.codec_ref.schema == custom.CUSTOM_SCHEMA
    assert binding.codec_ref.media_type == 'application/vnd.example.point'
    assert binding.codec_ref.capabilities == ('bytes',)
    assert binding.transferable.__module__ == 'c_two.providers.custom.generated'
    assert binding.transferable.deserialize(binding.transferable.serialize(Point(1, 2))) == Point(1, 2)


def test_custom_record_resolves_in_crm_without_manual_transfer_and_exports_portably():
    from c_two.providers import custom

    @custom.record(
        id='org.example.payload.custom',
        version='1',
        schema_text='{"fields":["value"],"kind":"payload"}',
    )
    class Payload:
        value: int

        def serialize(value: 'Payload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes | memoryview) -> 'Payload':
            return Payload(int(bytes(data)))

    @cc.crm(namespace='test.custom-provider', version='0.1.0')
    class PayloadStore:
        def echo(self, value: Payload) -> Payload:
            ...

    descriptor = build_contract_descriptor(PayloadStore, portable=True)
    method = descriptor['methods'][0]

    assert method['wire']['input']['id'] == 'org.example.payload.custom'
    assert method['wire']['output']['id'] == 'org.example.payload.custom'
    assert method['parameters'][0]['annotation']['kind'] == 'codec'
    assert method['return']['kind'] == 'codec'
    assert 'python-pickle-default' not in json.dumps(descriptor)
    assert PayloadStore.echo._input_transferable.__module__ == 'c_two.providers.custom.generated'
    assert PayloadStore.echo._output_transferable.__module__ == 'c_two.providers.custom.generated'


def test_custom_record_from_buffer_declares_buffer_view_capability():
    from c_two.providers import custom

    calls: list[str] = []

    @custom.record(
        id='org.example.buffered.custom',
        version='1',
        schema_sha256='a' * 64,
    )
    class Buffered:
        value: bytes

        def serialize(value: 'Buffered') -> bytes:
            return value.value

        def deserialize(data: bytes | memoryview) -> 'Buffered':
            calls.append('deserialize')
            return Buffered(bytes(data))

        def from_buffer(data: memoryview) -> 'Buffered':
            calls.append('from_buffer')
            return Buffered(bytes(data))

    binding = custom.CustomCodecProvider().candidates_for_type(Buffered)

    assert binding is not None
    assert set(binding.codec_ref.capabilities) == {'bytes', 'buffer-view'}
    assert binding.transferable.from_buffer(memoryview(b'abc')) == Buffered(b'abc')
    assert calls == ['from_buffer']


def test_custom_record_rejects_missing_or_invalid_contract_parts():
    from c_two.providers import custom

    with pytest.raises(ValueError, match='schema_text or schema_sha256'):

        @custom.record(id='org.example.missing', version='1')
        class MissingSchema:
            value: int

            def serialize(value: 'MissingSchema') -> bytes:
                return b''

            def deserialize(data: bytes) -> 'MissingSchema':
                return MissingSchema(0)

    with pytest.raises(TypeError, match='deserialize'):

        @custom.record(id='org.example.no-deserialize', version='1', schema_sha256='b' * 64)
        class NoDeserialize:
            value: int

            def serialize(value: 'NoDeserialize') -> bytes:
                return b''

    with pytest.raises(ValueError, match='schema_sha256'):

        @custom.record(id='org.example.bad-hash', version='1', schema_sha256='bad')
        class BadHash:
            value: int

            def serialize(value: 'BadHash') -> bytes:
                return b''

            def deserialize(data: bytes) -> 'BadHash':
                return BadHash(0)

    with pytest.raises(ValueError, match='buffer-view capability requires'):

        @custom.record(
            id='org.example.bad-capability',
            version='1',
            schema_sha256='c' * 64,
            capabilities=('buffer-view',),
        )
        class BadCapability:
            value: int

            def serialize(value: 'BadCapability') -> bytes:
                return b''

            def deserialize(data: bytes) -> 'BadCapability':
                return BadCapability(0)


def test_custom_record_runtime_rejects_non_bytes_serializer_output():
    from c_two.providers import custom

    @custom.record(id='org.example.bad-serializer', version='1', schema_sha256='d' * 64)
    class BadSerializer:
        value: int

        def serialize(value: 'BadSerializer') -> object:
            return {'value': value.value}

        def deserialize(data: bytes) -> 'BadSerializer':
            return BadSerializer(0)

    binding = custom.CustomCodecProvider().candidates_for_type(BadSerializer)

    assert binding is not None
    with pytest.raises(TypeError, match='must return bytes-like data'):
        binding.transferable.serialize(BadSerializer(1))


def test_custom_record_conflicting_provider_candidates_are_rejected():
    from c_two.crm.codec import CodecBinding, CodecRef, resolve_codec, use_codec
    from c_two.crm.transferable import Transferable
    from c_two.providers import custom

    @custom.record(id='org.example.conflict.custom', version='1', schema_sha256='e' * 64)
    class Payload:
        value: int

        def serialize(value: 'Payload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'Payload':
            return Payload(int(bytes(data)))

    class AlternatePayloadTransferable(Transferable):
        def serialize(value: Payload) -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> Payload:
            return Payload(int(bytes(data)))

    def alternate_provider(annotation: object, context: object | None = None):
        if annotation is Payload:
            return CodecBinding(
                transferable=AlternatePayloadTransferable,
                codec_ref=CodecRef(
                    id='org.example.conflict.alternate',
                    version='1',
                    schema_sha256='f' * 64,
                ),
            )
        return None

    use_codec(alternate_provider)

    with pytest.raises(ValueError, match='Multiple codec candidates'):
        resolve_codec(Payload, {'position': 'output'})
