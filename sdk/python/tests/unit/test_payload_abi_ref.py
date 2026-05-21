import pytest

import c_two as cc
from c_two.crm._payload_abi import PayloadAbiRef
from c_two.crm.descriptor import build_contract_descriptor
from c_two.crm.transferable import _payload_abi_ref_transferable


def test_payload_abi_ref_serializes_stably_and_validates_fields():
    ref = PayloadAbiRef(
        id='org.example.payload',
        version='1',
        schema='example.schema.v1',
        schema_sha256='a' * 64,
        capabilities=('buffer-view', 'bytes'),
        media_type='application/vnd.example.payload',
    )

    assert ref.to_wire_ref() == {
        'capabilities': ['buffer-view', 'bytes'],
        'id': 'org.example.payload',
        'kind': 'codec_ref',
        'media_type': 'application/vnd.example.payload',
        'portable': True,
        'schema': 'example.schema.v1',
        'schema_sha256': 'a' * 64,
        'version': '1',
    }

    with pytest.raises(ValueError, match='id'):
        PayloadAbiRef(id=' ', version='1')
    with pytest.raises(ValueError, match='schema_sha256'):
        PayloadAbiRef(id='org.example.payload', version='1', schema_sha256='bad')
    with pytest.raises(ValueError, match='duplicate capability'):
        PayloadAbiRef(id='org.example.payload', version='1', capabilities=('bytes', 'bytes'))


def test_internal_payload_abi_ref_transferable_can_carry_wire_identity_and_artifacts():
    payload_ref = PayloadAbiRef(
        id='org.example.internal',
        version='1',
        schema='example.internal.v1',
        schema_sha256='b' * 64,
    )
    artifact = {
        'schema': 'example.internal.v1',
        'type': 'Payload',
    }

    @_payload_abi_ref_transferable(payload_abi_ref=payload_ref)
    class PayloadWire:
        value: int

        def serialize(value: 'PayloadWire') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'PayloadWire':
            return PayloadWire(int(bytes(data)))

    PayloadWire.__cc_payload_abi_artifacts__ = (artifact,)

    @cc.crm(namespace='test.payload-abi-ref', version='0.1.0')
    class PayloadContract:
        @cc.transfer(input=PayloadWire, output=PayloadWire)
        def echo(self, value: PayloadWire) -> PayloadWire:
            ...

    descriptor = build_contract_descriptor(PayloadContract)
    method = descriptor['methods'][0]

    assert method['wire']['input'] == payload_ref.to_wire_ref()
    assert method['wire']['output'] == payload_ref.to_wire_ref()
    assert method['parameters'][0]['annotation'] == {
        'codec': payload_ref.to_wire_ref(),
        'kind': 'codec',
    }
    assert method['return'] == {
        'codec': payload_ref.to_wire_ref(),
        'kind': 'codec',
    }
    assert cc.export_contract_payload_abi_artifacts(PayloadContract) == '[{"schema":"example.internal.v1","type":"Payload"}]'
    with pytest.raises(ValueError, match='portable contract.*non-FastDB payload ABI'):
        build_contract_descriptor(PayloadContract, portable=True)


def test_portable_descriptor_rejects_pickle_only_wire_refs():
    @cc.crm(namespace='test.payload-abi-ref', version='0.1.0')
    class PickleOnlyContract:
        def echo(self, value: bytes) -> bytes:
            ...

    nonportable = build_contract_descriptor(PickleOnlyContract)
    assert nonportable['methods'][0]['wire']['input']['family'] == 'python-pickle-default'

    with pytest.raises(ValueError, match='portable contract.*python-pickle-default'):
        build_contract_descriptor(PickleOnlyContract, portable=True)
