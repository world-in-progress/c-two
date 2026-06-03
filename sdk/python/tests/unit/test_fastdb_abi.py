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

    assert 'fastdb4py>=0.1.22' in dependencies


def test_c_two_workspace_uses_published_fastdb_dependency():
    pyproject = tomllib.loads((_repo_root() / 'pyproject.toml').read_text())

    sources = pyproject.get('tool', {}).get('uv', {}).get('sources', {})

    assert 'fastdb4py' not in sources


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


def test_fastdb_abi_distinguishes_columnar_vertices_from_object_graph_nodes():
    import c_two as cc
    import fastdb4py as fdb

    namespace = {'cc': cc, 'fdb': fdb}
    source = """
@fdb.feature
class Vertex:
    vertex_id: fdb.U32
    x: fdb.F64
    y: fdb.F64
    z: fdb.F64

@fdb.feature
class Node:
    node_id: fdb.U32
    weight: fdb.F64
    anchor: Vertex
    neighbors: list[Vertex]

@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class Mesh:
    def vertices(self) -> fdb.Batch[Vertex]:
        ...

    def nodes(self) -> fdb.Batch[Node]:
        ...
"""
    exec(compile(source, '<fastdb_vertex_node_abi>', 'exec', dont_inherit=True), namespace)
    Mesh = namespace['Mesh']

    descriptor = json.loads(cc.export_contract_descriptor(Mesh))
    methods = {method['name']: method for method in descriptor['methods']}

    vertex_output = methods['vertices']['wire']['output']
    node_output = methods['nodes']['wire']['output']

    assert vertex_output['id'] == 'org.fastdb.call-db'
    assert 'buffer-view' in vertex_output['capabilities']
    assert node_output['id'] == 'org.fastdb.call-db'
    assert node_output['capabilities'] == ['bytes']

    vertex_binding = Mesh.vertices._output_payload_binding
    node_binding = Mesh.nodes._output_payload_binding

    assert vertex_binding.supports_retained_view is True
    assert node_binding.supports_retained_view is False

    node_artifacts = list(node_binding.payload_abi_artifacts)
    call_schema = next(
        artifact for artifact in node_artifacts
        if artifact.get('schema') == 'fastdb.call-db.schema.v1'
    )
    assert call_schema['profile'] == 'fastdb.call.object-graph.v1'
    node_table = call_schema['tables'][0]
    assert node_table['feature']['name'] == 'Node'
    assert node_table['feature_schema_dependencies']
    assert node_table['feature_schema_dependencies'][0]['feature']['name'] == 'Vertex'


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


def test_fastdb_call_plan_serializes_allocated_batch_without_slot_name():
    import c_two.fastdb.call_db as c2_call_db
    import fastdb4py as fdb
    import numpy as np

    namespace = {'fdb': fdb}
    exec(compile("""
@fdb.feature
class AllocPoint:
    x: fdb.F64
""", '<fastdb_alloc_point>', 'exec', dont_inherit=True), namespace)
    AllocPoint = namespace['AllocPoint']

    plan = c2_call_db.plan_call_db_output(
        method_name='points',
        return_annotation=fdb.Batch[AllocPoint],
        crm_context={
            'crm_namespace': 'test.fastdb-abi',
            'crm_name': 'AllocContract',
            'crm_version': '0.1.0',
        },
    )
    batch = fdb.Batch.allocate(AllocPoint, 3)
    batch.fill(x=np.array([1.0, 2.0, 3.0], dtype=np.float64))

    serialized = plan.serialize_values(batch)
    view = plan.view_from_buffer(serialized)

    assert isinstance(serialized, bytes)
    assert isinstance(view, fdb.Batch)
    assert list(view.column.x) == [1.0, 2.0, 3.0]


def test_fastdb_payload_binding_exposes_prepared_write_plan_for_call_db():
    import c_two as cc
    import fastdb4py as fdb
    import numpy as np

    namespace = {'cc': cc, 'fdb': fdb}
    exec(compile("""
@fdb.feature
class PreparedPoint:
    x: fdb.F64

@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class PreparedContract:
    def pairs(self) -> tuple[fdb.Batch[PreparedPoint], fdb.Batch[PreparedPoint]]:
        ...
""", '<fastdb_prepared_binding>', 'exec', dont_inherit=True), namespace)
    PreparedContract = namespace['PreparedContract']
    PreparedPoint = namespace['PreparedPoint']
    binding = PreparedContract.pairs._output_payload_binding

    left = fdb.Batch.allocate(PreparedPoint, 2)
    left.fill(x=np.array([1.0, 2.0], dtype=np.float64))
    right = fdb.Batch.allocate(PreparedPoint, 3)
    right.fill(x=np.array([3.0, 4.0, 5.0], dtype=np.float64))

    write_plan = binding.prepare_write(left, right)
    destination = bytearray(write_plan.nbytes)

    write_plan.write_into(destination)
    decoded_left, decoded_right = binding.deserialize(destination)

    assert binding.prepare_write is not None
    assert write_plan.byte_length == write_plan.nbytes
    assert list(decoded_left.column.x) == [1.0, 2.0]
    assert list(decoded_right.column.x) == [3.0, 4.0, 5.0]


def test_fastdb_payload_binding_direct_writer_uses_final_backing_allocator(monkeypatch):
    import c_two as cc
    import fastdb4py as fdb
    from fastdb4py import call_db as fdb_call_db
    import numpy as np

    namespace = {'cc': cc, 'fdb': fdb}
    exec(compile("""
@fdb.feature
class DirectAllocatorPoint:
    x: fdb.F64

@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class DirectAllocatorContract:
    def points(self) -> fdb.Batch[DirectAllocatorPoint]:
        ...
""", '<fastdb_direct_allocator_binding>', 'exec', dont_inherit=True), namespace)
    DirectAllocatorContract = namespace['DirectAllocatorContract']
    DirectAllocatorPoint = namespace['DirectAllocatorPoint']
    binding = DirectAllocatorContract.points._output_payload_binding

    points = fdb.Batch.allocate(DirectAllocatorPoint, 3)
    points.fill(x=np.array([1.0, 2.0, 3.0], dtype=np.float64))

    def fail_prepared_write(self, destination):
        raise AssertionError('C-Two direct allocator path must not use FastdbPreparedCallDb.write_into')

    monkeypatch.setattr(fdb_call_db.FastdbPreparedCallDb, 'write_into', fail_prepared_write)

    write_plan = binding.prepare_write(points)
    destination = bytearray(write_plan.nbytes)

    write_plan.write_into(destination)
    decoded = binding.deserialize(destination)

    assert getattr(write_plan, 'direct', False) is True
    assert getattr(write_plan, 'build_mode', '') != 'fallback'
    assert binding.build_context is not None
    assert list(decoded.column.x) == [1.0, 2.0, 3.0]


def test_fastdb_payload_binding_keeps_unsupported_direct_as_fallback():
    import c_two as cc
    import fastdb4py as fdb

    namespace = {'cc': cc, 'fdb': fdb}
    exec(compile("""
@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class FallbackStringArrayContract:
    def values(self) -> fdb.Array[fdb.STR]:
        ...
""", '<fastdb_direct_allocator_fallback_binding>', 'exec', dont_inherit=True), namespace)
    FallbackStringArrayContract = namespace['FallbackStringArrayContract']
    binding = FallbackStringArrayContract.values._output_payload_binding

    values = fdb.Array.allocate(fdb.STR, 2)
    values.fill(['left', 'right'])

    write_plan = binding.prepare_write(values)
    destination = bytearray(write_plan.nbytes)

    write_plan.write_into(destination)
    decoded = binding.deserialize(destination)

    assert getattr(write_plan, 'direct', True) is False
    assert getattr(write_plan, 'build_mode', '') == 'fallback'
    assert isinstance(getattr(write_plan, 'fallback_reason', None), str)
    assert getattr(write_plan, 'fallback_reason')
    assert binding.build_context is None
    assert list(decoded) == ['left', 'right']


def test_fastdb_payload_binding_prepares_require_envelope_values():
    import c_two as cc
    import fastdb4py as fdb
    import numpy as np

    namespace = {'cc': cc, 'fdb': fdb}
    exec(compile("""
@fdb.feature
class RequirePreparedPoint:
    x: fdb.F64

@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class RequirePreparedContract:
    def points(self) -> fdb.Batch[RequirePreparedPoint]:
        ...
""", '<fastdb_require_prepared_binding>', 'exec', dont_inherit=True), namespace)
    RequirePreparedContract = namespace['RequirePreparedContract']
    RequirePreparedPoint = namespace['RequirePreparedPoint']
    binding = RequirePreparedContract.points._output_payload_binding

    points = fdb.require(fdb.batch(RequirePreparedPoint, rows=3))
    points.fill(x=np.array([10.0, 20.0, 30.0], dtype=np.float64))

    write_plan = binding.prepare_write(points)
    destination = bytearray(write_plan.nbytes)

    write_plan.write_into(destination)
    decoded = binding.deserialize(destination)

    assert binding.prepare_write is not None
    assert getattr(write_plan, 'direct', False) is True
    assert getattr(write_plan, 'build_mode', '') == 'direct-layer-splice'
    assert list(decoded.column.x) == [10.0, 20.0, 30.0]


def test_fastdb_input_calls_use_prepared_payload_when_client_supports_it():
    import c_two as cc
    import fastdb4py as fdb

    namespace = {'cc': cc, 'fdb': fdb}
    exec(compile("""
@cc.crm(namespace='test.fastdb-abi', version='0.1.0')
class PreparedInputContract:
    def send(self, values: fdb.Array[fdb.I32]) -> None:
        ...
""", '<fastdb_prepared_input>', 'exec', dont_inherit=True), namespace)
    PreparedInputContract = namespace['PreparedInputContract']

    class PreparedClient:
        supports_direct_call = False

        def __init__(self):
            self.calls = []

        def call_prepared(self, method_name, write_plan):
            self.calls.append((method_name, write_plan))
            return b''

        def call(self, method_name, payload):
            raise AssertionError('prepared FastDB input should not use serialized call fallback')

    values = fdb.Array.allocate(fdb.I32, 3)
    values.fill([1, 2, 3])
    client = PreparedClient()
    contract = PreparedInputContract()
    contract.client = client

    assert contract.send(values) is None

    assert len(client.calls) == 1
    method_name, write_plan = client.calls[0]
    assert method_name == 'send'
    assert write_plan.nbytes > 0
