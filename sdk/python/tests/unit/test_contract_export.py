import json

import pytest

import c_two as cc


def test_export_contract_descriptor_uses_c_two_contract_schema_and_rejects_pickle_for_unsupported_bytes():
    @cc.crm(namespace='test.contract-export', version='0.1.0')
    class PickleOnly:
        def echo(self, value: bytes) -> bytes:
            ...

    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(PickleOnly)


def test_export_contract_descriptor_rejects_python_native_primitives_as_nonportable():
    @cc.crm(namespace='test.contract-export', version='0.1.0')
    class PythonNative:
        def echo(self, values: list[int], label: str | None = None) -> list[str | None]:
            ...

    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(PythonNative)


def test_contract_descriptor_diagnostics_report_python_only_and_non_fastdb_paths():
    @cc.crm(namespace='test.contract-diagnostics', version='0.1.0')
    class PickleOnly:
        def echo(self, value: bytes) -> bytes:
            ...

    pickle_diagnostics = cc.contract_descriptor_diagnostics(PickleOnly)

    assert [
        (item['method'], item['position'], item['code'])
        for item in pickle_diagnostics
    ] == [
        ('echo', 'input', 'fastdb_call_db_not_planned'),
        ('echo', 'output', 'fastdb_call_db_not_planned'),
        ('echo', 'input', 'python_only_pickle'),
        ('echo', 'output', 'python_only_pickle'),
    ]
    assert all(item['severity'] == 'warning' for item in pickle_diagnostics)

    @cc.crm(namespace='test.contract-diagnostics', version='0.1.0')
    class PythonNative:
        def echo(self, value: int) -> str:
            ...

    control_diagnostics = cc.contract_descriptor_diagnostics(PythonNative)

    assert [
        (item['method'], item['position'], item['code'])
        for item in control_diagnostics
    ] == [
        ('echo', 'input', 'fastdb_call_db_not_planned'),
        ('echo', 'output', 'fastdb_call_db_not_planned'),
        ('echo', 'input', 'python_only_pickle'),
        ('echo', 'output', 'python_only_pickle'),
    ]


def test_contract_descriptor_diagnostics_warn_for_non_fastdb_portable_payload_abi_refs():
    from c_two.crm._payload_abi import PayloadAbiRef
    from c_two.crm.transferable import _payload_abi_ref_transferable

    payload_ref = PayloadAbiRef(id='org.example.custom', version='1', schema_sha256='c' * 64)

    @_payload_abi_ref_transferable(payload_abi_ref=payload_ref)
    class PayloadWire:
        value: int

        def serialize(value: 'PayloadWire') -> bytes:
            return str(value.value).encode()

        def deserialize(data: bytes) -> 'PayloadWire':
            return PayloadWire(int(bytes(data)))

    @cc.crm(namespace='test.contract-diagnostics', version='0.1.0')
    class CustomPayloadAbi:
        @cc.transfer(input=PayloadWire, output=PayloadWire)
        def echo(self, value: PayloadWire) -> PayloadWire:
            ...

    diagnostics = cc.contract_descriptor_diagnostics(CustomPayloadAbi)

    assert [
        (item['method'], item['position'], item['code'], item['payload_abi_id'])
        for item in diagnostics
    ] == [
        ('echo', 'input', 'non_fastdb_portable_payload_abi', 'org.example.custom'),
        ('echo', 'output', 'non_fastdb_portable_payload_abi', 'org.example.custom'),
    ]
    assert all(item['severity'] == 'warning' for item in diagnostics)


def test_export_contract_descriptor_returns_canonical_portable_json():
    from c_two import fastdb as fdb

    @cc.crm(namespace='test.contract-export', version='0.1.0')
    class Portable:
        def echo(self, value: fdb.I32) -> fdb.I32:
            ...

    exported = cc.export_contract_descriptor(Portable)
    descriptor = json.loads(exported)
    from c_two.crm.contract import crm_contract

    contract = crm_contract(Portable)

    assert exported == json.dumps(descriptor, sort_keys=True, separators=(',', ':'))
    assert descriptor['schema'] == 'c-two.contract.v1'
    assert descriptor['fingerprints'] == {
        'abi_hash': contract.abi_hash,
        'signature_hash': contract.signature_hash,
    }
    assert descriptor['methods'][0]['wire']['input']['id'] == 'org.fastdb.call-db'
    assert descriptor['methods'][0]['wire']['output']['id'] == 'org.fastdb.call-db'
    assert 'python-pickle-default' not in exported


def test_export_contract_payload_abi_artifacts_dedupes_transferable_artifacts():
    from c_two.crm._payload_abi import PayloadAbiRef
    from c_two.crm.transferable import _payload_abi_ref_transferable

    payload_ref = PayloadAbiRef(
        id='org.example.artifact',
        version='1',
        schema='example.schema.v1',
        schema_sha256='b' * 64,
    )
    artifact = {
        'fields': [{'name': 'value', 'type': 'i32'}],
        'schema': 'example.schema.v1',
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

    @cc.crm(namespace='test.contract-artifacts', version='0.1.0')
    class Portable:
        @cc.transfer(input=PayloadWire, output=PayloadWire)
        def echo(self, value: PayloadWire) -> PayloadWire:
            ...

    artifacts = json.loads(cc.export_contract_payload_abi_artifacts(Portable))

    assert artifacts == [artifact]


def test_export_contract_descriptor_rejects_legacy_custom_abi_after_native_validation():
    @cc.transferable(abi_id='org.example.legacy.raw.v1')
    class LegacyPayload:
        value: int

        def serialize(value: 'LegacyPayload') -> bytes:
            return b''

        def deserialize(data: bytes) -> 'LegacyPayload':
            return LegacyPayload(1)

    @cc.crm(namespace='test.contract-export', version='0.1.0')
    class LegacyPortable:
        @cc.transfer(input=LegacyPayload, output=LegacyPayload)
        def echo(self, value: LegacyPayload) -> LegacyPayload:
            ...

    with pytest.raises(ValueError, match='PayloadAbiRef'):
        cc.export_contract_descriptor(LegacyPortable)


def test_contract_export_cli_writes_descriptor(tmp_path, monkeypatch):
    module_path = tmp_path / 'portable_contract_module.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            '@cc.crm(namespace="test.contract-export-cli", version="0.1.0")',
            'class Ping:',
            '    def ping(self) -> None:',
            '        ...',
            '',
        ]),
    )
    out_path = tmp_path / 'contract.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(['export', 'portable_contract_module:Ping', '--out', str(out_path)]) == 0
    descriptor = json.loads(out_path.read_text())

    assert descriptor['schema'] == 'c-two.contract.v1'
    assert descriptor['crm']['name'] == 'Ping'
    assert descriptor['methods'][0]['name'] == 'ping'


def test_contract_artifacts_cli_writes_payload_abi_artifacts(tmp_path, monkeypatch):
    module_path = tmp_path / 'artifact_contract_module.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            'from c_two.crm._payload_abi import PayloadAbiRef',
            'from c_two.crm.transferable import _payload_abi_ref_transferable',
            'payload_ref = PayloadAbiRef(id="org.example.artifact", version="1", schema="example.schema.v1", schema_sha256="c" * 64)',
            '@_payload_abi_ref_transferable(payload_abi_ref=payload_ref)',
            'class PayloadWire:',
            '    def serialize(value: "PayloadWire") -> bytes:',
            '        return b""',
            '    def deserialize(data: bytes) -> "PayloadWire":',
            '        return PayloadWire()',
            'PayloadWire.__cc_payload_abi_artifacts__ = ({"schema": "example.schema.v1", "type": "Payload"},)',
            '@cc.crm(namespace="test.contract-artifacts-cli", version="0.1.0")',
            'class Ping:',
            '    @cc.transfer(input=PayloadWire, output=PayloadWire)',
            '    def ping(self) -> PayloadWire:',
            '        ...',
            '',
        ]),
    )
    out_path = tmp_path / 'artifacts.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(['artifacts', 'artifact_contract_module:Ping', '--out', str(out_path)]) == 0
    artifacts = json.loads(out_path.read_text())

    assert artifacts == [{'schema': 'example.schema.v1', 'type': 'Payload'}]


def test_contract_diagnose_cli_writes_fastdb_first_diagnostics(tmp_path, monkeypatch):
    module_path = tmp_path / 'diagnostic_contract_module.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            '@cc.crm(namespace="test.contract-diagnose-cli", version="0.1.0")',
            'class PythonNative:',
            '    def echo(self, value: int) -> str:',
            '        ...',
            '',
        ]),
    )
    out_path = tmp_path / 'diagnostics.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(['diagnose', 'diagnostic_contract_module:PythonNative', '--out', str(out_path)]) == 0
    diagnostics = json.loads(out_path.read_text())

    assert [
        (item['method'], item['position'], item['code'])
        for item in diagnostics
    ] == [
        ('echo', 'input', 'fastdb_call_db_not_planned'),
        ('echo', 'output', 'fastdb_call_db_not_planned'),
        ('echo', 'input', 'python_only_pickle'),
        ('echo', 'output', 'python_only_pickle'),
    ]


def test_contract_diagnose_cli_writes_non_fastdb_payload_abi_diagnostics(tmp_path, monkeypatch):
    module_path = tmp_path / 'non_fastdb_payload_abi_diagnostic_module.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            'from c_two.crm._payload_abi import PayloadAbiRef',
            'from c_two.crm.transferable import _payload_abi_ref_transferable',
            'payload_ref = PayloadAbiRef(id="org.example.cli-custom", version="1", schema_sha256="d" * 64)',
            '@_payload_abi_ref_transferable(payload_abi_ref=payload_ref)',
            'class PayloadWire:',
            '    def serialize(value: "PayloadWire") -> bytes:',
            '        return b""',
            '    def deserialize(data: bytes) -> "PayloadWire":',
            '        return PayloadWire()',
            '@cc.crm(namespace="test.contract-diagnose-cli", version="0.1.0")',
            'class CustomPayloadAbi:',
            '    @cc.transfer(input=PayloadWire, output=PayloadWire)',
            '    def echo(self, value: PayloadWire) -> PayloadWire:',
            '        ...',
            '',
        ]),
    )
    out_path = tmp_path / 'diagnostics.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main(['diagnose', 'non_fastdb_payload_abi_diagnostic_module:CustomPayloadAbi', '--out', str(out_path)]) == 0
    diagnostics = json.loads(out_path.read_text())

    assert [
        (item['method'], item['position'], item['code'], item['payload_abi_id'])
        for item in diagnostics
    ] == [
        ('echo', 'input', 'non_fastdb_portable_payload_abi', 'org.example.cli-custom'),
        ('echo', 'output', 'non_fastdb_portable_payload_abi', 'org.example.cli-custom'),
    ]
