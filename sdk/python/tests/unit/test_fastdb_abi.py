from __future__ import annotations

import json
import subprocess
import sys
import tomllib
from pathlib import Path


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def test_c_two_fastdb_module_exposes_fastdb_abi_aliases():
    import c_two as cc
    from c_two import fastdb as top_level_fastdb
    import c_two.fastdb as fdb

    assert top_level_fastdb is fdb
    assert cc.fastdb is fdb
    assert 'FastDB call-db' in (fdb.__doc__ or '')
    assert 'portable CRM payload ABI' in (fdb.__doc__ or '')

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

    assert not any(name.startswith('install_c_two') for name in dir(fdb))


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

    assert 'fastdb4py>=0.1.17' in dependencies


def test_c_two_workspace_does_not_override_fastdb_release_dependency():
    pyproject = tomllib.loads((_repo_root() / 'pyproject.toml').read_text())

    sources = pyproject.get('tool', {}).get('uv', {}).get('sources', {})

    assert 'fastdb4py' not in sources


def test_fastdb_abi_plans_portable_descriptor_without_extra_setup():
    import c_two as cc
    from c_two import fastdb as fdb

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
    from c_two import fastdb as fdb

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
