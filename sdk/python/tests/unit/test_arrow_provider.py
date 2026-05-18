import dataclasses
import json

import pytest

import c_two as cc
from c_two.crm.descriptor import build_contract_descriptor

pytest.importorskip('pyarrow', reason='Arrow provider tests require pyarrow')


@pytest.fixture(autouse=True)
def clear_codec_registry():
    from c_two.crm.codec import _clear_codec_registry_for_tests

    _clear_codec_registry_for_tests()
    try:
        yield
    finally:
        _clear_codec_registry_for_tests()


def test_arrow_record_provider_generates_deterministic_codec_and_round_trips():
    from c_two.providers import arrow

    assert not hasattr(arrow, 'use_arrow')

    @arrow.record(schema_id='test.point.arrow-ipc.v1')
    class Point:
        x: float
        y: float
        name: str

    assert dataclasses.is_dataclass(Point)

    provider_a = arrow.ArrowCodecProvider()
    provider_b = arrow.ArrowCodecProvider()
    binding_a = provider_a.candidates_for_type(Point, {'position': 'output'})
    binding_b = provider_b.candidates_for_type(Point, {'position': 'output'})

    assert binding_a is not None
    assert binding_b is not None
    assert binding_a.codec_ref == binding_b.codec_ref
    assert binding_a.codec_ref.id == arrow.ARROW_IPC_CODEC_ID
    assert binding_a.codec_ref.schema == arrow.ARROW_IPC_SCHEMA
    assert binding_a.codec_ref.media_type == arrow.ARROW_IPC_MEDIA_TYPE

    payload = Point(x=1.25, y=2.5, name='origin')
    restored = binding_a.transferable.deserialize(binding_a.transferable.serialize(payload))

    assert restored == payload


def test_arrow_provider_rejects_unsupported_record_annotations():
    from c_two.providers import arrow

    @arrow.record(schema_id='test.bad.arrow-ipc.v1')
    class BadRecord:
        values: set[int]

    provider = arrow.ArrowCodecProvider()

    with pytest.raises(TypeError, match='Unsupported Arrow record annotation.*set'):
        provider.candidates_for_type(BadRecord, {'position': 'output'})


def test_arrow_record_without_schema_id_requires_crm_context():
    from c_two.providers import arrow

    @arrow.record
    class Detached:
        value: int

    provider = arrow.ArrowCodecProvider()

    with pytest.raises(TypeError, match='requires CRM codec context'):
        provider.candidates_for_type(Detached, {'position': 'output'})


def test_arrow_provider_supports_list_record_wire_without_manual_transfer():
    from c_two.providers import arrow

    @arrow.record
    class Cell:
        level: int
        global_id: int
        active: bool

    @cc.crm(namespace='test.arrow-provider', version='0.1.0')
    class CellStore:
        def get_cell(self, global_id: int) -> Cell:
            ...

        def get_cell_again(self, global_id: int) -> Cell:
            ...

        def list_cells(self, level: int) -> list[Cell]:
            ...

        def list_cells_again(self, level: int) -> list[Cell]:
            ...

        def list_ids(self) -> list[int]:
            ...

    descriptor = json.loads(cc.export_contract_descriptor(CellStore))
    methods = {method['name']: method for method in descriptor['methods']}

    assert methods['get_cell']['wire']['output']['id'] == arrow.ARROW_IPC_CODEC_ID
    assert methods['get_cell']['return']['kind'] == 'codec'
    assert methods['list_cells']['wire']['output']['id'] == arrow.ARROW_IPC_CODEC_ID
    assert methods['list_cells']['return']['kind'] == 'list'
    assert methods['list_cells']['return']['item']['kind'] == 'codec'
    assert methods['list_cells']['return']['item']['codec']['id'] == arrow.ARROW_IPC_CODEC_ID
    assert methods['list_ids']['wire']['output']['id'] == 'c-two.control.json'
    assert getattr(CellStore.list_cells, '_output_transferable').__module__ == 'c_two.providers.arrow.generated'
    assert 'test.arrow-provider.CellStore.Cell.arrow-ipc.v0.1.0' in (
        CellStore.list_cells._output_transferable._c2_schema_text
    )
    assert '"mode":"batch"' in CellStore.list_cells._output_transferable._c2_schema_text
    assert 'test.arrow-provider.CellStore.Cell.arrow-ipc.v0.1.0' in (
        CellStore.get_cell._output_transferable._c2_schema_text
    )
    assert '"mode":"single"' in CellStore.get_cell._output_transferable._c2_schema_text
    assert (
        CellStore.get_cell._output_transferable.__cc_codec_ref__
        == CellStore.get_cell_again._output_transferable.__cc_codec_ref__
    )
    assert (
        CellStore.list_cells._output_transferable.__cc_codec_ref__
        == CellStore.list_cells_again._output_transferable.__cc_codec_ref__
    )

    single_schema = json.loads(CellStore.get_cell._output_transferable._c2_schema_text)
    batch_schema = json.loads(CellStore.list_cells._output_transferable._c2_schema_text)
    assert single_schema['crm'] == {
        'name': 'CellStore',
        'namespace': 'test.arrow-provider',
        'version': '0.1.0',
    }
    assert single_schema['fields'] == [
        {'name': 'level', 'nullable': False, 'type': 'int64'},
        {'name': 'global_id', 'nullable': False, 'type': 'int64'},
        {'name': 'active', 'nullable': False, 'type': 'bool'},
    ]
    assert single_schema['mode'] == 'single'
    assert single_schema['record'] == 'Cell'
    assert single_schema['schema_id'] == 'test.arrow-provider.CellStore.Cell.arrow-ipc.v0.1.0'
    assert batch_schema['crm'] == single_schema['crm']
    assert batch_schema['fields'] == single_schema['fields']
    assert batch_schema['mode'] == 'batch'
    assert batch_schema['record'] == 'Cell'
    assert batch_schema['schema_id'] == 'test.arrow-provider.CellStore.Cell.arrow-ipc.v0.1.0'

    from c_two.crm.codec import _clear_codec_registry_for_tests

    _clear_codec_registry_for_tests()
    rebuilt = build_contract_descriptor(CellStore, methods=['list_cells'], portable=True)
    assert rebuilt['methods'][0]['return']['item']['codec']['id'] == arrow.ARROW_IPC_CODEC_ID

    restored = CellStore.list_cells._output_transferable.deserialize(
        CellStore.list_cells._output_transferable.serialize([
            Cell(level=1, global_id=10, active=True),
            Cell(level=1, global_id=11, active=False),
        ]),
    )

    assert restored == [
        Cell(level=1, global_id=10, active=True),
        Cell(level=1, global_id=11, active=False),
    ]


def test_arrow_provider_descriptor_helper_keeps_plain_dataclass_portable():
    from c_two.providers import arrow

    @arrow.record
    class Sample:
        value: int

    @cc.crm(namespace='test.arrow-provider', version='0.1.0')
    class SampleStore:
        def echo(self, sample: Sample) -> Sample:
            ...

    descriptor = build_contract_descriptor(SampleStore, portable=True)
    method = descriptor['methods'][0]

    assert method['parameters'][0]['annotation']['kind'] == 'codec'
    assert method['wire']['input']['id'] == arrow.ARROW_IPC_CODEC_ID
    assert method['wire']['output']['id'] == arrow.ARROW_IPC_CODEC_ID


def test_arrow_default_schema_identity_uses_crm_version():
    from c_two.providers import arrow

    @arrow.record
    class Versioned:
        value: int

    def make_store(version: str):
        @cc.crm(namespace='test.arrow-version', version=version)
        class Store:
            def get(self) -> Versioned:
                ...

        return Store

    store_v1 = make_store('0.1.0')
    store_v2 = make_store('0.2.0')
    ref_v1 = store_v1.get._output_transferable.__cc_codec_ref__
    ref_v2 = store_v2.get._output_transferable.__cc_codec_ref__

    assert ref_v1.schema_sha256 != ref_v2.schema_sha256
    assert 'test.arrow-version.Store.Versioned.arrow-ipc.v0.1.0' in (
        store_v1.get._output_transferable._c2_schema_text
    )
    assert 'test.arrow-version.Store.Versioned.arrow-ipc.v0.2.0' in (
        store_v2.get._output_transferable._c2_schema_text
    )


def test_arrow_record_name_and_schema_id_overrides():
    from c_two.providers import arrow

    @arrow.record(name='PublicCell')
    class InternalCell:
        value: int

    @arrow.record(schema_id='org.example.shared-cell.arrow-ipc')
    class ExplicitCell:
        value: int

    @cc.crm(namespace='test.arrow-overrides', version='1.2.3')
    class OverrideStore:
        def named(self) -> InternalCell:
            ...

        def explicit(self) -> ExplicitCell:
            ...

    assert 'test.arrow-overrides.OverrideStore.PublicCell.arrow-ipc.v1.2.3' in (
        OverrideStore.named._output_transferable._c2_schema_text
    )
    assert 'org.example.shared-cell.arrow-ipc' in (
        OverrideStore.explicit._output_transferable._c2_schema_text
    )


def test_arrow_explicit_batch_annotation_returns_arrow_view_without_changing_list_behavior():
    from c_two.providers import arrow

    @arrow.record
    class Cell:
        level: int
        global_id: int
        active: bool

    @cc.crm(namespace='test.arrow-view', version='0.1.0')
    class CellStore:
        def list_cells(self) -> list[Cell]:
            ...

        def view_cells(self) -> arrow.Batch[Cell]:
            ...

    descriptor = build_contract_descriptor(CellStore, portable=True)
    methods = {method['name']: method for method in descriptor['methods']}

    assert methods['list_cells']['return']['kind'] == 'list'
    assert methods['list_cells']['return']['item']['kind'] == 'codec'
    assert hasattr(CellStore.list_cells._output_transferable, 'from_buffer')
    assert 'buffer-view' in CellStore.list_cells._output_transferable.__cc_codec_ref__.capabilities
    assert '"mode":"batch"' in CellStore.list_cells._output_transferable._c2_schema_text

    assert methods['view_cells']['return']['kind'] == 'codec'
    assert methods['view_cells']['wire']['output']['id'] == arrow.ARROW_IPC_CODEC_ID
    assert hasattr(CellStore.view_cells._output_transferable, 'from_buffer')
    assert 'buffer-view' in CellStore.view_cells._output_transferable.__cc_codec_ref__.capabilities
    assert '"mode":"batch"' in CellStore.view_cells._output_transferable._c2_schema_text
    assert (
        CellStore.list_cells._output_transferable.__cc_codec_ref__
        == CellStore.view_cells._output_transferable.__cc_codec_ref__
    )

    payload = [
        Cell(level=1, global_id=10, active=True),
        Cell(level=1, global_id=11, active=False),
    ]
    data = CellStore.list_cells._output_transferable.serialize(payload)

    materialized = CellStore.list_cells._output_transferable.deserialize(data)
    assert materialized == payload

    view = CellStore.list_cells._output_transferable.from_buffer(memoryview(data))

    assert isinstance(view, arrow.ArrowBatchView)
    assert len(view) == 2
    assert view.to_records() == payload

    held_view = CellStore.view_cells._output_transferable.from_buffer(memoryview(data))
    assert isinstance(held_view, arrow.ArrowBatchView)
    assert held_view.to_records() == payload
