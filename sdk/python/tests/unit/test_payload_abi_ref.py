import pytest

import c_two as cc
from c_two.crm._payload_abi import PayloadAbiRef
from c_two.crm.descriptor import build_contract_descriptor
from c_two.crm.payload_plan import PayloadBinding, PayloadPlanKind


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


def test_payload_binding_carries_fdb_wire_identity_and_artifacts():
    payload_ref = PayloadAbiRef(
        id='org.fastdb.call-db',
        version='1',
        schema='fastdb.call-db.schema.v1',
        schema_sha256='b' * 64,
    )
    artifact = {
        'schema': 'fastdb.call-db.schema.v1',
        'type': 'Payload',
    }

    binding = PayloadBinding(
        kind=PayloadPlanKind.FDB,
        serialize=lambda value: bytes(value),
        deserialize=lambda data: data,
        payload_abi_ref=payload_ref,
        payload_abi_artifacts=(artifact,),
    )

    assert binding.payload_abi_ref == payload_ref
    assert binding.payload_abi_artifacts == (artifact,)
    assert binding.kind is PayloadPlanKind.FDB
    assert not binding.supports_retained_view


def test_portable_descriptor_rejects_pickle_only_wire_refs():
    @cc.crm(namespace='test.payload-abi-ref', version='0.1.0')
    class PickleOnlyContract:
        def echo(self, value: bytes) -> bytes:
            ...

    nonportable = build_contract_descriptor(PickleOnlyContract)
    assert nonportable['methods'][0]['wire']['input']['family'] == 'python-pickle-default'

    with pytest.raises(ValueError, match='portable contract.*python-pickle-default'):
        build_contract_descriptor(PickleOnlyContract, portable=True)
