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

def test_export_contract_descriptor_returns_canonical_portable_json():
    import fastdb4py as fdb

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
