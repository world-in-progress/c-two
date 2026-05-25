from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def test_c_two_fastdb_module_exposes_c_two_integration_helpers_only():
    import c_two as cc
    from c_two import fastdb as top_level_fastdb
    import c_two.fastdb as c2_fastdb
    import fastdb4py as fdb

    assert top_level_fastdb is c2_fastdb
    assert cc.fastdb is c2_fastdb
    assert 'C-Two integration helpers' in (c2_fastdb.__doc__ or '')

    for name in (
        'derive_bridge_hooks',
        'derive_c_two_bridge',
        'CTwoCodegenError',
        'generate_c_two_typescript_helpers',
        'run_codegen_c_two_ts',
    ):
        assert hasattr(c2_fastdb, name), name

    for name in (
        'BOOL',
        'U8',
        'U16',
        'U32',
        'I32',
        'U8N',
        'U16N',
        'F32',
        'F64',
        'STR',
        'WSTR',
        'REF',
        'BYTES',
        'Array',
        'Batch',
        'feature',
    ):
        assert hasattr(fdb, name), name
        assert not hasattr(c2_fastdb, name), name

    assert not any(name.startswith('install_c_two') for name in dir(c2_fastdb))


def test_import_c_two_does_not_eagerly_load_legacy_fastdb_glue():
    script = """
import sys
import c_two
print('fastdb4py.c_two_bridge' in sys.modules)
"""
    result = subprocess.run(
        [sys.executable, '-c', script],
        check=True,
        capture_output=True,
        text=True,
    )

    assert result.stdout.splitlines() == ['False']


def test_c_two_python_package_declares_fastdb_dependency():
    pyproject = tomllib.loads((_repo_root() / 'sdk/python/pyproject.toml').read_text())

    dependencies = pyproject['project']['dependencies']

    assert 'fastdb4py>=0.1.20' in dependencies


def test_c_two_workspace_uses_sibling_fastdb_for_local_integration():
    pyproject = tomllib.loads((_repo_root() / 'pyproject.toml').read_text())

    sources = pyproject.get('tool', {}).get('uv', {}).get('sources', {})

    assert sources['fastdb4py'] == {'path': '../fastdb', 'editable': True}


def test_fastdb_abi_plans_portable_descriptor_without_extra_setup():
    import c_two as cc
    import fastdb4py as fdb

    namespace = {'cc': cc, 'fdb': fdb}
    source = """
@fdb.feature
class Point:
    x: fdb.F64
    y: fdb.F64

@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class FastdbPortable:
    def project(
        self,
        values: fdb.Array[fdb.F64],
        label: fdb.WSTR,
        raw: fdb.BYTES,
    ) -> fdb.Batch[Point]:
        ...
"""
    exec(compile(source, '<fastdb_abi_no_install>', 'exec', dont_inherit=True), namespace)
    FastdbPortable = namespace['FastdbPortable']

    descriptor = json.loads(cc.export_contract_descriptor(FastdbPortable))
    method = descriptor['methods'][0]

    assert method['wire']['input']['id'] == 'org.fastdb.call-db'
    assert method['wire']['output']['id'] == 'org.fastdb.call-db'
    assert method['wire']['input']['portable'] is True
    assert method['wire']['output']['portable'] is True


def test_fastdb_abi_diagnostics_explain_rejected_shapes_without_extra_setup():
    import c_two as cc
    import fastdb4py as fdb

    namespace = {'cc': cc, 'fdb': fdb}
    source = """
@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class UnsupportedFastdbShape:
    def echo(self, values: list[int]) -> fdb.Array[fdb.I32]:
        ...
"""
    exec(compile(source, '<fastdb_abi_diagnostics>', 'exec', dont_inherit=True), namespace)
    UnsupportedFastdbShape = namespace['UnsupportedFastdbShape']

    diagnostics = cc.contract_descriptor_diagnostics(UnsupportedFastdbShape)

    fastdb_items = [
        item
        for item in diagnostics
        if item['code'] == 'fastdb_call_db_not_planned'
    ]
    assert len(fastdb_items) == 1
    assert fastdb_items[0]['method'] == 'echo'
    assert fastdb_items[0]['position'] == 'input'
    assert 'portable fastdb call-db payload' in fastdb_items[0]['message']
    assert 'Array[...]' in fastdb_items[0]['reason']


def test_fastdb_call_plan_uses_fastdb_exporter_before_encoder(monkeypatch):
    import c_two.fastdb.call_db as c2_call_db
    import fastdb4py as fdb

    namespace = {'fdb': fdb}
    exec(compile("""
@fdb.feature
class ExportPoint:
    x: fdb.F64
""", '<fastdb_export_point>', 'exec', dont_inherit=True), namespace)
    ExportPoint = namespace['ExportPoint']

    plan = c2_call_db.plan_call_db_output(
        method_name='points',
        return_annotation=fdb.Batch[ExportPoint],
        crm_context={
            'crm_namespace': 'test.fastdb-abi',
            'crm_name': 'ExportContract',
            'crm_version': '0.1.0',
        },
    )
    exported = memoryview(b'exact-fastdb-buffer')

    def fake_export(binding, value):
        assert binding.method == 'points'
        assert value == []
        return exported

    def fail_encode(*args, **kwargs):
        raise AssertionError('FastDB exact export must be tried before encode_call_db')

    monkeypatch.setattr(c2_call_db, 'try_export_call_db', fake_export)
    monkeypatch.setattr(c2_call_db, 'encode_call_db', fail_encode)

    assert plan.serialize_values([]) is exported


def test_fastdb_call_plan_falls_back_to_encoder_when_exporter_misses(monkeypatch):
    import c_two.fastdb.call_db as c2_call_db
    import fastdb4py as fdb

    namespace = {'fdb': fdb}
    exec(compile("""
@fdb.feature
class FallbackPoint:
    x: fdb.F64
""", '<fastdb_fallback_point>', 'exec', dont_inherit=True), namespace)
    FallbackPoint = namespace['FallbackPoint']

    plan = c2_call_db.plan_call_db_output(
        method_name='points',
        return_annotation=fdb.Batch[FallbackPoint],
        crm_context={
            'crm_namespace': 'test.fastdb-abi',
            'crm_name': 'FallbackContract',
            'crm_version': '0.1.0',
        },
    )

    monkeypatch.setattr(c2_call_db, 'try_export_call_db', lambda binding, value: None)
    monkeypatch.setattr(c2_call_db, 'encode_call_db', lambda binding, value: b'encoded')

    assert plan.serialize_values([]) == b'encoded'


def test_fastdb_call_plan_returns_memoryview_for_exact_named_batch():
    import c_two.fastdb.call_db as c2_call_db
    import fastdb4py as fdb
    import numpy as np

    namespace = {'fdb': fdb}
    exec(compile("""
@fdb.feature
class ExactPoint:
    x: fdb.F64
""", '<fastdb_exact_point>', 'exec', dont_inherit=True), namespace)
    ExactPoint = namespace['ExactPoint']

    plan = c2_call_db.plan_call_db_output(
        method_name='points',
        return_annotation=fdb.Batch[ExactPoint],
        crm_context={
            'crm_namespace': 'test.fastdb-abi',
            'crm_name': 'ExactContract',
            'crm_version': '0.1.0',
        },
    )
    engine = fdb.ColumnEngine.truncate([fdb.Layout(ExactPoint, 3, name='return_0')])
    table = engine.table(ExactPoint, name='return_0')
    table.fill(x=np.array([1.0, 2.0, 3.0], dtype=np.float64))

    serialized = plan.serialize_values(table)

    assert isinstance(serialized, memoryview)
    assert serialized.obj is table._db._buffer  # noqa: SLF001
