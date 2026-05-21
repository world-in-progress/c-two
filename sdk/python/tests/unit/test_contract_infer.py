import json
import sys
from pathlib import Path

import pytest

import c_two as cc
from c_two.crm.descriptor import build_contract_descriptor


def test_infer_crm_from_resource_builds_python_only_descriptor():
    class Resource:
        @cc.read
        def total(self, values: list[int]) -> int:
            return sum(values)

        def label(self, name: str, suffix: str | None = None) -> str | None:
            return name if suffix is None else f'{name}-{suffix}'

        def _helper(self) -> None:
            ...

    crm_class = cc.infer_crm_from_resource(
        Resource,
        namespace='test.infer',
        version='0.1.0',
        name='ResourceCRM',
        methods=['total', 'label'],
    )
    descriptor = build_contract_descriptor(crm_class)
    methods = {method['name']: method for method in descriptor['methods']}

    assert descriptor['crm'] == {
        'namespace': 'test.infer',
        'name': 'ResourceCRM',
        'version': '0.1.0',
    }
    assert list(methods) == ['total', 'label']
    assert methods['total']['access'] == 'read'
    assert methods['total']['wire']['input']['family'] == 'python-pickle-default'
    assert methods['total']['wire']['output']['family'] == 'python-pickle-default'
    assert methods['label']['parameters'][1]['default'] == {
        'kind': 'json_scalar',
        'value': None,
    }
    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(crm_class)


def test_infer_crm_requires_explicit_public_methods():
    class Resource:
        def public(self) -> None:
            ...

    with pytest.raises(ValueError, match='methods'):
        cc.infer_crm_from_resource(
            Resource,
            namespace='test.infer',
            version='0.1.0',
            methods=[],
        )
    with pytest.raises(ValueError, match='private'):
        cc.infer_crm_from_resource(
            Resource,
            namespace='test.infer',
            version='0.1.0',
            methods=['_hidden'],
        )


def test_infer_crm_preserves_method_transfer_metadata():
    from c_two.crm._payload_abi import PayloadAbiRef
    from c_two.crm.transferable import _payload_abi_ref_transferable

    payload_abi_ref = PayloadAbiRef(
        id='org.example.payload',
        version='1',
        schema='example.payload.v1',
    )

    @_payload_abi_ref_transferable(payload_abi_ref=payload_abi_ref)
    class Payload:
        value: str

        def serialize(data: 'Payload') -> bytes:
            return data.value.encode()

        def deserialize(data: bytes) -> 'Payload':
            return Payload(data.decode())

    class Resource:
        @cc.transfer(input=Payload, output=Payload)
        def echo(self, payload: Payload) -> Payload:
            return payload

    crm_class = cc.infer_crm_from_resource(
        Resource,
        namespace='test.infer',
        version='0.1.0',
        name='PayloadCRM',
        methods=['echo'],
    )
    descriptor = build_contract_descriptor(crm_class)
    method = descriptor['methods'][0]

    assert method['wire']['input'] == payload_abi_ref.to_wire_ref()
    assert method['wire']['output'] == payload_abi_ref.to_wire_ref()
    with pytest.raises(ValueError, match='non-FastDB payload ABI org.example.payload'):
        cc.export_contract_descriptor(crm_class)
    assert [
        (item['method'], item['position'], item['code'], item['payload_abi_id'])
        for item in cc.contract_descriptor_diagnostics(crm_class)
    ] == [
        ('echo', 'input', 'non_fastdb_portable_payload_abi', 'org.example.payload'),
        ('echo', 'output', 'non_fastdb_portable_payload_abi', 'org.example.payload'),
    ]


def test_infer_crm_rejects_ambiguous_resource_signatures():
    class Resource:
        def missing_param_annotation(self, value) -> int:
            return 1

        def missing_return_annotation(self, value: int):
            return value

        def varargs(self, *values: int) -> int:
            return sum(values)

        def kwargs(self, **values: int) -> int:
            return len(values)

        def keyword_only(self, *, value: int) -> int:
            return value

    cases = [
        ('missing_param_annotation', 'missing a type annotation'),
        ('missing_return_annotation', 'missing a return annotation'),
        ('varargs', 'varargs'),
        ('kwargs', 'varargs'),
        ('keyword_only', 'keyword-only'),
    ]
    for method_name, message in cases:
        with pytest.raises(TypeError, match=message):
            cc.infer_crm_from_resource(
                Resource,
                namespace='test.infer',
                version='0.1.0',
                methods=[method_name],
            )


def test_grid_resource_infer_smoke_uses_python_fallback_shapes(monkeypatch):
    pytest.importorskip('pyarrow', reason='grid examples require pyarrow file support')
    root = Path(__file__).resolve().parents[4]
    monkeypatch.syspath_prepend(str(root / 'examples/python'))
    for module_name in (
        'grid.nested_grid',
        'grid.grid_py_crm',
    ):
        sys.modules.pop(module_name, None)
    from grid.nested_grid import NestedGrid

    methods = [
        'get_schema',
        'get_grid_infos',
        'subdivide_grids',
        'get_active_grid_infos',
        'hello',
        'none_hello',
    ]
    crm_class = cc.infer_crm_from_resource(
        NestedGrid,
        namespace='demo.grid.inferred',
        version='0.1.0',
        name='InferredGrid',
        methods=methods,
    )
    descriptor = build_contract_descriptor(crm_class)
    by_name = {method['name']: method for method in descriptor['methods']}
    encoded = json.dumps(descriptor, sort_keys=True)

    assert [method['name'] for method in descriptor['methods']] == methods
    assert by_name['get_schema']['wire']['output']['family'] == 'python-pickle-default'
    assert by_name['get_grid_infos']['wire']['output']['family'] == 'python-pickle-default'
    assert by_name['get_grid_infos']['return']['kind'] == 'list'
    assert by_name['get_grid_infos']['return']['item']['kind'] == 'python_type'
    assert by_name['subdivide_grids']['wire']['input']['family'] == 'python-pickle-default'
    assert by_name['subdivide_grids']['wire']['output']['family'] == 'python-pickle-default'
    assert by_name['get_active_grid_infos']['wire']['output']['family'] == 'python-pickle-default'
    assert 'python-pickle-default' in encoded

    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(crm_class, methods=['get_schema'])
    with pytest.raises(ValueError, match='python-pickle-default'):
        cc.export_contract_descriptor(crm_class)


def test_contract_infer_cli_writes_descriptor(tmp_path, monkeypatch):
    module_path = tmp_path / 'resource_module.py'
    module_path.write_text(
        '\n'.join([
            'class Resource:',
            '    def ping(self) -> None:',
            '        return None',
            '',
        ]),
    )
    out_path = tmp_path / 'contract.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main([
        'infer',
        'resource_module:Resource',
        '--namespace',
        'test.infer-cli',
        '--version',
        '0.1.0',
        '--name',
        'ResourceCRM',
        '--method',
        'ping',
        '--out',
        str(out_path),
    ]) == 0
    descriptor = json.loads(out_path.read_text())

    assert descriptor['schema'] == 'c-two.contract.v1'
    assert descriptor['crm']['name'] == 'ResourceCRM'
    assert descriptor['methods'][0]['name'] == 'ping'


def test_contract_infer_cli_writes_python_only_diagnostics(tmp_path, monkeypatch):
    module_path = tmp_path / 'diagnostic_resource_module.py'
    module_path.write_text(
        '\n'.join([
            'class Resource:',
            '    def echo(self, value: int) -> str:',
            '        return str(value)',
            '',
        ]),
    )
    out_path = tmp_path / 'diagnostics.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main([
        'infer',
        'diagnostic_resource_module:Resource',
        '--namespace',
        'test.infer-cli',
        '--version',
        '0.1.0',
        '--name',
        'ResourceCRM',
        '--method',
        'echo',
        '--diagnose',
        '--out',
        str(out_path),
    ]) == 0
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


def test_contract_infer_cli_writes_non_fastdb_payload_abi_diagnostics(tmp_path, monkeypatch):
    module_path = tmp_path / 'non_fastdb_payload_abi_diagnostic_resource.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            'from c_two.crm._payload_abi import PayloadAbiRef',
            'from c_two.crm.transferable import _payload_abi_ref_transferable',
            'payload_ref = PayloadAbiRef(id="org.example.infer-custom", version="1", schema_sha256="e" * 64)',
            '@_payload_abi_ref_transferable(payload_abi_ref=payload_ref)',
            'class PayloadWire:',
            '    def serialize(value: "PayloadWire") -> bytes:',
            '        return b""',
            '    def deserialize(data: bytes) -> "PayloadWire":',
            '        return PayloadWire()',
            'class Resource:',
            '    @cc.transfer(input=PayloadWire, output=PayloadWire)',
            '    def echo(self, value: PayloadWire) -> PayloadWire:',
            '        return value',
            '',
        ]),
    )
    out_path = tmp_path / 'diagnostics.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main([
        'infer',
        'non_fastdb_payload_abi_diagnostic_resource:Resource',
        '--namespace',
        'test.infer-cli',
        '--version',
        '0.1.0',
        '--name',
        'ResourceCRM',
        '--method',
        'echo',
        '--diagnose',
        '--out',
        str(out_path),
    ]) == 0
    diagnostics = json.loads(out_path.read_text())

    assert [
        (item['method'], item['position'], item['code'], item['payload_abi_id'])
        for item in diagnostics
    ] == [
        ('echo', 'input', 'non_fastdb_portable_payload_abi', 'org.example.infer-custom'),
        ('echo', 'output', 'non_fastdb_portable_payload_abi', 'org.example.infer-custom'),
    ]


def test_contract_infer_cli_writes_payload_abi_artifacts(tmp_path, monkeypatch):
    module_path = tmp_path / 'artifact_resource_module.py'
    module_path.write_text(
        '\n'.join([
            'import c_two as cc',
            'from c_two.crm._payload_abi import PayloadAbiRef',
            'from c_two.crm.transferable import _payload_abi_ref_transferable',
            'payload_ref = PayloadAbiRef(',
            '    id="org.example.infer-artifact",',
            '    version="1",',
            '    schema="example.infer-artifact.v1",',
            '    schema_sha256="c" * 64,',
            ')',
            '@_payload_abi_ref_transferable(payload_abi_ref=payload_ref)',
            'class PayloadWire:',
            '    value: int',
            '    def serialize(value: "PayloadWire") -> bytes:',
            '        return str(value.value).encode()',
            '    def deserialize(data: bytes) -> "PayloadWire":',
            '        return PayloadWire(int(bytes(data)))',
            'PayloadWire.__cc_payload_abi_artifacts__ = ({"schema": "example.infer-artifact.v1", "type": "Payload"},)',
            'class Resource:',
            '    @cc.transfer(input=PayloadWire, output=PayloadWire)',
            '    def echo(self, value: PayloadWire) -> PayloadWire:',
            '        return value',
            '',
        ]),
    )
    out_path = tmp_path / 'artifacts.json'
    monkeypatch.syspath_prepend(str(tmp_path))

    from c_two.cli.contract import main

    assert main([
        'infer',
        'artifact_resource_module:Resource',
        '--namespace',
        'test.infer-cli',
        '--version',
        '0.1.0',
        '--name',
        'ResourceCRM',
        '--method',
        'echo',
        '--artifacts',
        '--out',
        str(out_path),
    ]) == 0
    artifacts = json.loads(out_path.read_text())

    assert artifacts == [{'schema': 'example.infer-artifact.v1', 'type': 'Payload'}]
