import pytest

import c_two as cc
from c_two.crm.codec import _clear_codec_registry_for_tests


pytestmark = pytest.mark.timeout(30)


@pytest.fixture(autouse=True)
def cleanup_registry():
    cc.shutdown()
    _clear_codec_registry_for_tests()
    yield
    cc.shutdown()
    _clear_codec_registry_for_tests()


def test_custom_provider_ipc_round_trip_and_input_from_buffer():
    from c_two.providers import custom

    from_buffer_calls: list[bytes] = []

    @custom.record(
        id='org.example.custom-ipc.payload',
        version='1',
        schema_text='{"fields":["value"],"kind":"custom-ipc-payload"}',
    )
    class Payload:
        value: int

        def serialize(value: 'Payload') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes | memoryview) -> 'Payload':
            return Payload(int(bytes(data)))

        def from_buffer(data: memoryview) -> 'Payload':
            from_buffer_calls.append(bytes(data))
            return Payload(int(bytes(data)))

    @cc.crm(namespace='test.custom-ipc', version='0.1.0')
    class CustomResource:
        def echo(self, payload: Payload) -> Payload:
            ...

    class CustomResourceImpl:
        def echo(self, payload: Payload) -> Payload:
            return Payload(payload.value + 1)

    cc.register(CustomResource, CustomResourceImpl(), name='custom-ipc')
    address = cc.server_address()
    assert address is not None

    client = cc.connect(CustomResource, name='custom-ipc', address=address)
    try:
        assert client.echo(Payload(41)) == Payload(42)
        held = cc.hold(client.echo)(Payload(9))
        try:
            assert held.value == Payload(10)
        finally:
            held.release()
    finally:
        cc.close(client)

    assert from_buffer_calls
