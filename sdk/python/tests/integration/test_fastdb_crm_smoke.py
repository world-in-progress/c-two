from __future__ import annotations

from collections.abc import Iterable, Iterator, Mapping, MutableMapping, MutableSequence, Sequence
from typing import (
    Mapping as TypingMapping,
    MutableMapping as TypingMutableMapping,
    MutableSequence as TypingMutableSequence,
    Sequence as TypingSequence,
    Tuple as TypingTuple,
)

import pytest

import c_two as cc
from c_two.config.settings import settings
from c_two.crm.descriptor import build_contract_descriptor
from c_two.transport.registry import _ProcessRegistry


@pytest.fixture(autouse=True)
def clean_runtime():
    previous_relay = settings.relay_anchor_address
    _ProcessRegistry.reset()
    try:
        yield
    finally:
        _ProcessRegistry.reset()
        settings.relay_anchor_address = previous_relay


def _make_fastdb_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    from fastdb4py import F64, STR, feature

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': F64, 'y': F64, 'name': STR}
    RpcPoint = feature(RpcPoint)

    class FastdbPoint:
        def echo(self, point):
            ...

    FastdbPoint.echo.__annotations__ = {'point': RpcPoint, 'return': RpcPoint}
    FastdbPoint = cc.crm(namespace=f'test.fastdb.{namespace}', version='0.1.0')(FastdbPoint)

    class FastdbPointResource:
        def echo(self, point):
            return RpcPoint(x=point.x + 1.0, y=point.y + 2.0, name=f'{point.name}:ok')

    return RpcPoint, FastdbPoint, FastdbPointResource


def _make_fastdb_native_resource_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbNativeGrid:
        def summarize(self, ids):
            ...

        def active(self):
            ...

    FastdbNativeGrid.summarize.__annotations__ = {
        'ids': fdb.Array[fdb.I32],
        'return': fdb.I32,
    }
    FastdbNativeGrid.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbNativeGrid = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbNativeGrid)

    class FastdbNativeGridResource:
        def __init__(self):
            self.seen = []
            self.active_calls = 0

        def summarize(self, ids):
            values = list(ids)
            self.seen.append(('summarize', values))
            return sum(values)

        def active(self):
            self.active_calls += 1
            self.seen.append(('active', self.active_calls))
            return [
                RpcGridId(level=self.active_calls, global_id=self.active_calls * 10),
                RpcGridId(level=self.active_calls + 1, global_id=self.active_calls * 10 + 10),
            ]

    FastdbNativeGridResource.summarize.__annotations__ = {
        'ids': fdb.Array[fdb.I32],
        'return': fdb.I32,
    }
    FastdbNativeGridResource.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }

    resource = FastdbNativeGridResource()
    return (
        RpcGridId,
        FastdbNativeGrid,
        resource,
        derive_c_two_bridge(cc, FastdbNativeGrid, resource),
    )


def _make_fastdb_batch_input_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbBatchInput:
        def summarize(self, ids):
            ...

    FastdbBatchInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbBatchInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbBatchInput)

    class TupleColumnResource:
        def __init__(self):
            self.seen = []

        def summarize(self, ids: tuple[list[int], list[int]]) -> int:
            levels, global_ids = ids
            self.seen.append((levels, global_ids))
            return sum(levels) + sum(global_ids)

    resource = TupleColumnResource()
    return (
        RpcGridId,
        FastdbBatchInput,
        resource,
        derive_c_two_bridge(cc, FastdbBatchInput, resource),
    )


def _make_fastdb_column_container_batch_input_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbColumnContainerBatchInput:
        def summarize(self, ids):
            ...

    FastdbColumnContainerBatchInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbColumnContainerBatchInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbColumnContainerBatchInput)

    class ColumnContainerResource:
        def __init__(self):
            self.seen = []

        def summarize(self, ids) -> int:
            if isinstance(ids, Mapping):
                levels_source = ids['level']
                global_ids_source = ids['global_id']
            else:
                levels_source, global_ids_source = ids
            iterator_flags = (
                iter(levels_source) is levels_source,
                iter(global_ids_source) is global_ids_source,
            )
            levels = list(levels_source)
            global_ids = list(global_ids_source)
            self.seen.append((iterator_flags, levels, global_ids))
            return sum(levels) + sum(global_ids)

    if resource_shape == 'tuple_sequence_columns':
        ids_annotation = tuple[Sequence[int], Sequence[int]]
    elif resource_shape == 'tuple_iterator_columns':
        ids_annotation = tuple[Iterator[int], Iterator[int]]
    elif resource_shape == 'mapping_sequence_columns':
        ids_annotation = Mapping[str, Sequence[int]]
    elif resource_shape == 'mapping_iterator_columns':
        ids_annotation = Mapping[str, Iterator[int]]
    elif resource_shape == 'mutable_mapping_columns':
        ids_annotation = MutableMapping[str, MutableSequence[int]]
    elif resource_shape == 'typing_mapping_columns':
        ids_annotation = TypingMapping[str, TypingSequence[int]]
    elif resource_shape == 'typing_mutable_mapping_columns':
        ids_annotation = TypingMutableMapping[str, TypingMutableSequence[int]]
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')
    ColumnContainerResource.summarize.__annotations__ = {
        'ids': ids_annotation,
        'return': int,
    }

    resource = ColumnContainerResource()
    return (
        RpcGridId,
        FastdbColumnContainerBatchInput,
        resource,
        derive_c_two_bridge(cc, FastdbColumnContainerBatchInput, resource),
    )


def _make_fastdb_variadic_tuple_batch_bridge_contract(namespace: str, *, output_shape: str = 'domain_object'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbVariadicTupleBatch:
        def score_many(self, points):
            ...

        def active_points(self):
            ...

    FastdbVariadicTupleBatch.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbVariadicTupleBatch.active_points.__annotations__ = {
        'return': fdb.Batch[RpcPoint],
    }
    FastdbVariadicTupleBatch = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbVariadicTupleBatch)

    class DomainPoint:
        def __init__(self, x: float, y: float, label: str):
            self.x = x
            self.y = y
            self.label = label

    class VariadicTupleBatchResource:
        def __init__(self):
            self.seen = []

        def score_many(self, points):
            self.seen.append((type(points), [(point.x, point.y, point.label) for point in points]))
            return sum(point.x + point.y for point in points)

        def active_points(self):
            if output_shape == 'domain_object':
                return (
                    DomainPoint(1.5, 2.5, 'a'),
                    DomainPoint(3.5, 4.5, 'b'),
                )
            if output_shape == 'mapping':
                return (
                    {'x': '1.5', 'y': '2.5', 'label': 123},
                    {'x': '3.5', 'y': '4.5', 'label': 'edge'},
                )
            if output_shape == 'feature':
                return (
                    RpcPoint(x='1.5', y='2.5', label=123),
                    RpcPoint(x='3.5', y='4.5', label='edge'),
                )
            raise AssertionError(f'unhandled output shape: {output_shape}')

    VariadicTupleBatchResource.score_many.__annotations__ = {
        'points': tuple[DomainPoint, ...],
        'return': float,
    }
    if output_shape == 'domain_object':
        active_return = tuple[DomainPoint, ...]
    elif output_shape == 'mapping':
        active_return = tuple[Mapping[str, object], ...]
    elif output_shape == 'feature':
        active_return = tuple[RpcPoint, ...]
    else:
        raise AssertionError(f'unhandled output shape: {output_shape}')
    VariadicTupleBatchResource.active_points.__annotations__ = {
        'return': active_return,
    }

    resource = VariadicTupleBatchResource()
    return (
        RpcPoint,
        FastdbVariadicTupleBatch,
        resource,
        derive_c_two_bridge(cc, FastdbVariadicTupleBatch, resource),
    )


def _make_fastdb_split_batch_input_bridge_contract(namespace: str, *, resource_shape: str = 'list'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbSplitBatchInput:
        def summarize(self, ids):
            ...

    FastdbSplitBatchInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbSplitBatchInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbSplitBatchInput)

    class SplitColumnResource:
        def __init__(self):
            self.seen = []

        def summarize(self, global_id, level) -> int:
            global_is_iterator = iter(global_id) is global_id
            level_is_iterator = iter(level) is level
            global_values = list(global_id)
            level_values = list(level)
            if resource_shape == 'iterator':
                self.seen.append((global_is_iterator, global_values, level_is_iterator, level_values))
            else:
                self.seen.append((global_values, level_values))
            return sum(level_values) + sum(global_values)

    if resource_shape == 'list':
        global_annotation = list[int]
        level_annotation = list[int]
    elif resource_shape == 'sequence':
        global_annotation = Sequence[int]
        level_annotation = Sequence[int]
    elif resource_shape == 'mutable_sequence':
        global_annotation = MutableSequence[int]
        level_annotation = MutableSequence[int]
    elif resource_shape == 'iterator':
        global_annotation = Iterator[int]
        level_annotation = Iterator[int]
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')
    SplitColumnResource.summarize.__annotations__ = {
        'global_id': global_annotation,
        'level': level_annotation,
        'return': int,
    }

    resource = SplitColumnResource()
    return (
        RpcGridId,
        FastdbSplitBatchInput,
        resource,
        derive_c_two_bridge(cc, FastdbSplitBatchInput, resource),
    )


def _make_fastdb_dict_column_input_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbDictColumnInput:
        def summarize(self, ids):
            ...

    FastdbDictColumnInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbDictColumnInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbDictColumnInput)

    class DictColumnResource:
        def __init__(self):
            self.seen = []

        def summarize(self, columns: dict[str, list[int]]) -> int:
            self.seen.append(columns)
            return sum(columns['level']) + sum(columns['global_id'])

    resource = DictColumnResource()
    return (
        RpcGridId,
        FastdbDictColumnInput,
        resource,
        derive_c_two_bridge(cc, FastdbDictColumnInput, resource),
    )


def _make_fastdb_table_object_input_bridge_contract(namespace: str, *, resource_shape: str = 'constructor'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbTableObjectInput:
        def summarize(self, ids):
            ...

    FastdbTableObjectInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbTableObjectInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbTableObjectInput)

    class RecordsTable:
        def __init__(self, records):
            self.records = [dict(record) for record in records]

    class FromRecordsTable:
        def __init__(self, records):
            self.records = [dict(record) for record in records]

        @classmethod
        def from_records(cls, records):
            return cls(records)

    class ConstructorTableWithInstanceFactory:
        def __init__(self, records):
            self.records = [dict(record) for record in records]

        def from_records(self, records):
            raise AssertionError('instance from_records should not be used as a table factory')

    if resource_shape == 'constructor':
        table_cls = RecordsTable
    elif resource_shape == 'from_records':
        table_cls = FromRecordsTable
    elif resource_shape == 'constructor_instance_factory':
        table_cls = ConstructorTableWithInstanceFactory
    else:
        raise AssertionError(f'unknown table object input resource shape {resource_shape!r}')

    class TableObjectResource:
        def __init__(self):
            self.seen = []

        def summarize(self, ids):
            if resource_shape == 'constructor':
                self.seen.append(ids.records)
            else:
                self.seen.append((type(ids).__name__, ids.records))
            return sum(row['level'] for row in ids.records) + sum(row['global_id'] for row in ids.records)

    TableObjectResource.summarize.__annotations__ = {
        'ids': table_cls,
        'return': int,
    }

    resource = TableObjectResource()
    return (
        RpcGridId,
        FastdbTableObjectInput,
        resource,
        derive_c_two_bridge(cc, FastdbTableObjectInput, resource),
    )


def _make_fastdb_dict_column_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbDictColumnOutput:
        def active(self):
            ...

    FastdbDictColumnOutput.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbDictColumnOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbDictColumnOutput)

    class DictColumnResource:
        def __init__(self):
            self.calls = 0

        def active(self) -> dict[str, list[int]]:
            self.calls += 1
            return {'global_id': [10, 20], 'level': [1, 2]}

    resource = DictColumnResource()
    return (
        RpcGridId,
        FastdbDictColumnOutput,
        resource,
        derive_c_two_bridge(cc, FastdbDictColumnOutput, resource),
    )


def _make_fastdb_column_container_output_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbColumnContainerOutput:
        def active(self):
            ...

    FastdbColumnContainerOutput.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbColumnContainerOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbColumnContainerOutput)

    class ColumnContainerOutputResource:
        def __init__(self):
            self.calls = 0

        def active(self):
            self.calls += 1
            levels = [str(self.calls), str(self.calls + 1)]
            global_ids = [str(self.calls * 10), str(self.calls * 10 + 10)]
            if resource_shape == 'tuple_sequence_columns':
                return (tuple(levels), tuple(global_ids))
            if resource_shape == 'tuple_iterator_columns':
                return (iter(levels), iter(global_ids))
            if resource_shape == 'mapping_sequence_columns':
                return {'global_id': tuple(global_ids), 'level': tuple(levels)}
            if resource_shape == 'mapping_iterator_columns':
                return {'global_id': iter(global_ids), 'level': iter(levels)}
            if resource_shape == 'mutable_mapping_columns':
                return {'global_id': global_ids, 'level': levels}
            if resource_shape == 'typing_mutable_mapping_columns':
                return {'global_id': global_ids, 'level': levels}
            raise AssertionError(f'unhandled resource shape: {resource_shape}')

    if resource_shape == 'tuple_sequence_columns':
        return_annotation = tuple[Sequence[int], Sequence[int]]
    elif resource_shape == 'tuple_iterator_columns':
        return_annotation = tuple[Iterator[int], Iterator[int]]
    elif resource_shape == 'mapping_sequence_columns':
        return_annotation = Mapping[str, Sequence[int]]
    elif resource_shape == 'mapping_iterator_columns':
        return_annotation = Mapping[str, Iterator[int]]
    elif resource_shape == 'mutable_mapping_columns':
        return_annotation = MutableMapping[str, MutableSequence[int]]
    elif resource_shape == 'typing_mutable_mapping_columns':
        return_annotation = TypingMutableMapping[str, TypingMutableSequence[int]]
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')
    ColumnContainerOutputResource.active.__annotations__ = {
        'return': return_annotation,
    }

    resource = ColumnContainerOutputResource()
    return (
        RpcGridId,
        FastdbColumnContainerOutput,
        resource,
        derive_c_two_bridge(cc, FastdbColumnContainerOutput, resource),
    )


def _make_fastdb_table_like_output_bridge_contract(namespace: str, *, iterable_table: bool = True):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbTableLikeOutput:
        def active(self):
            ...

    FastdbTableLikeOutput.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbTableLikeOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbTableLikeOutput)

    class IterableTableLikeRows:
        def __init__(self, records):
            self._records = list(records)

        def __iter__(self):
            return iter(['level', 'global_id'])

        def to_dict(self, orient='dict'):
            if orient == 'records':
                return list(self._records)
            return {
                name: [record[name] for record in self._records]
                for name in ['level', 'global_id']
            }

    class NonIterableTableLikeRows:
        def __init__(self, records):
            self._records = list(records)

        def to_pylist(self):
            return list(self._records)

    class TableLikeResource:
        def __init__(self):
            self.calls = 0

        def active(self) -> object:
            self.calls += 1
            table_type = IterableTableLikeRows if iterable_table else NonIterableTableLikeRows
            return table_type([
                {'global_id': '10', 'level': '1'},
                {'global_id': '20', 'level': '2'},
            ])

    resource = TableLikeResource()
    return (
        RpcGridId,
        FastdbTableLikeOutput,
        resource,
        derive_c_two_bridge(cc, FastdbTableLikeOutput, resource),
    )


def _make_fastdb_table_like_tuple_row_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbTableLikeTupleRowOutput:
        def active_points(self):
            ...

    FastdbTableLikeTupleRowOutput.active_points.__annotations__ = {
        'return': fdb.Batch[RpcPoint],
    }
    FastdbTableLikeTupleRowOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbTableLikeTupleRowOutput)

    class NonIterableTupleRowTableLike:
        def __init__(self, rows):
            self._rows = list(rows)

        def to_pylist(self):
            return list(self._rows)

    class TableLikeTupleRowResource:
        def __init__(self):
            self.calls = 0

        def active_points(self) -> list[tuple[float, float, str]]:
            self.calls += 1
            return NonIterableTupleRowTableLike([
                ('1.5', '2.5', 123),
                ('3.5', '4.5', 'edge'),
            ])

    resource = TableLikeTupleRowResource()
    return (
        RpcPoint,
        FastdbTableLikeTupleRowOutput,
        resource,
        derive_c_two_bridge(cc, FastdbTableLikeTupleRowOutput, resource),
    )


def _make_fastdb_iterator_tuple_row_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbIteratorTupleRowOutput:
        def active_points(self):
            ...

    FastdbIteratorTupleRowOutput.active_points.__annotations__ = {
        'return': fdb.Batch[RpcPoint],
    }
    FastdbIteratorTupleRowOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbIteratorTupleRowOutput)

    class IteratorTupleRowResource:
        def __init__(self):
            self.calls = 0

        def active_points(self) -> Iterator[tuple[float, float, str]]:
            self.calls += 1
            rows = [
                ('1.5', '2.5', 123),
                ('3.5', '4.5', 'edge'),
            ]
            return iter(rows)

    resource = IteratorTupleRowResource()
    return (
        RpcPoint,
        FastdbIteratorTupleRowOutput,
        resource,
        derive_c_two_bridge(cc, FastdbIteratorTupleRowOutput, resource),
    )


def _make_fastdb_mapping_row_output_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbMappingRowOutput:
        def active_points(self):
            ...

    FastdbMappingRowOutput.active_points.__annotations__ = {
        'return': fdb.Batch[RpcPoint],
    }
    FastdbMappingRowOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbMappingRowOutput)

    rows = [
        {'x': '1.5', 'y': '2.5', 'label': 123},
        {'x': '3.5', 'y': '4.5', 'label': 'edge'},
    ]

    if resource_shape == 'sequence':
        class SequenceMappingRowResource:
            def __init__(self):
                self.calls = 0

            def active_points(self) -> Sequence[Mapping[str, object]]:
                self.calls += 1
                return list(rows)

        resource = SequenceMappingRowResource()
    elif resource_shape == 'iterator':
        class IteratorMappingRowResource:
            def __init__(self):
                self.calls = 0

            def active_points(self) -> Iterator[Mapping[str, object]]:
                self.calls += 1
                return iter(rows)

        resource = IteratorMappingRowResource()
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')

    return (
        RpcPoint,
        FastdbMappingRowOutput,
        resource,
        derive_c_two_bridge(cc, FastdbMappingRowOutput, resource),
    )


def _make_fastdb_abstract_container_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbAbstractContainers:
        def summarize(self, ids):
            ...

        def summarize_mutable(self, ids):
            ...

        def active(self):
            ...

        def active_mutable(self):
            ...

        def score_many(self, points):
            ...

    FastdbAbstractContainers.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbAbstractContainers.active.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbAbstractContainers.summarize_mutable.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbAbstractContainers.active_mutable.__annotations__ = {
        'return': fdb.Batch[RpcGridId],
    }
    FastdbAbstractContainers.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbAbstractContainers = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbAbstractContainers)

    class AbstractContainerResource:
        def __init__(self):
            self.seen = []

        def summarize(self, columns: Mapping[str, Sequence[int]]) -> int:
            self.seen.append(('summarize', dict(columns)))
            return sum(columns['level']) + sum(columns['global_id'])

        def summarize_mutable(self, columns: MutableMapping[str, MutableSequence[int]]) -> int:
            self.seen.append(('summarize_mutable', dict(columns)))
            return sum(columns['level']) + sum(columns['global_id'])

        def active(self) -> Mapping[str, Sequence[int]]:
            self.seen.append(('active', None))
            return {'global_id': (10, 20), 'level': (1, 2)}

        def active_mutable(self) -> MutableMapping[str, MutableSequence[int]]:
            self.seen.append(('active_mutable', None))
            return {'global_id': [10, 20], 'level': [1, 2]}

        def score_many(self, points: Sequence[Mapping[str, object]]) -> float:
            materialized = [dict(point) for point in points]
            self.seen.append(('score_many', materialized))
            return sum(float(point['x']) + float(point['y']) for point in materialized)

    resource = AbstractContainerResource()
    return (
        RpcGridId,
        RpcPoint,
        FastdbAbstractContainers,
        resource,
        derive_c_two_bridge(cc, FastdbAbstractContainers, resource),
    )


def _make_fastdb_tuple_return_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbTupleReturn:
        def active_pair(self):
            ...

    FastdbTupleReturn.active_pair.__annotations__ = {
        'return': tuple[fdb.Array[fdb.I32], fdb.Batch[RpcGridId]],
    }
    FastdbTupleReturn = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbTupleReturn)

    class TupleReturnResource:
        def __init__(self):
            self.calls = 0

        def active_pair(self) -> tuple[list[int], Mapping[str, Sequence[int]]]:
            self.calls += 1
            return [1, 2], {'global_id': (10, 20), 'level': (1, 2)}

    resource = TupleReturnResource()
    return (
        RpcGridId,
        FastdbTupleReturn,
        resource,
        derive_c_two_bridge(cc, FastdbTupleReturn, resource),
    )


def _make_fastdb_mapping_array_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    class MappingArrayOutputResource:
        def __init__(self):
            self.calls = 0

        def active_levels(self) -> Mapping[int, int]:
            self.calls += 1
            return {1: 10, 2: 20}

    resource = MappingArrayOutputResource()
    return (
        FastdbArrayOutput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayOutput, resource),
    )


def _make_fastdb_mutable_sequence_array_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    class MutableSequenceArrayOutputResource:
        def __init__(self):
            self.calls = 0

        def active_levels(self) -> MutableSequence[int]:
            self.calls += 1
            return [1, 2, 3]

    resource = MutableSequenceArrayOutputResource()
    return (
        FastdbArrayOutput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayOutput, resource),
    )


def _make_fastdb_sequence_array_output_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    if resource_shape == 'list':
        class ListArrayOutputResource:
            def __init__(self):
                self.calls = 0

            def active_levels(self) -> list[int]:
                self.calls += 1
                return ['1', '2', '3']

        resource = ListArrayOutputResource()
    elif resource_shape == 'sequence':
        class SequenceArrayOutputResource:
            def __init__(self):
                self.calls = 0

            def active_levels(self) -> Sequence[int]:
                self.calls += 1
                return ('1', '2', '3')

        resource = SequenceArrayOutputResource()
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')

    return (
        FastdbArrayOutput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayOutput, resource),
    )


def _make_fastdb_iterable_array_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    class IterableArrayOutputResource:
        def __init__(self):
            self.calls = 0

        def active_levels(self) -> Iterable[int]:
            self.calls += 1
            return (str(item) for item in (1, 2, 3))

    resource = IterableArrayOutputResource()
    return (
        FastdbArrayOutput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayOutput, resource),
    )


def _make_fastdb_iterator_array_output_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    class IteratorArrayOutputResource:
        def __init__(self):
            self.calls = 0

        def active_levels(self) -> Iterator[int]:
            self.calls += 1
            return iter(['1', '2', '3'])

    resource = IteratorArrayOutputResource()
    return (
        FastdbArrayOutput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayOutput, resource),
    )


def _make_fastdb_array_output_iterable_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb

    class FastdbArrayOutput:
        def active_levels(self):
            ...

    FastdbArrayOutput.active_levels.__annotations__ = {
        'return': fdb.Array[fdb.I32],
    }
    FastdbArrayOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayOutput)

    class TupleArrayOutputResource:
        def __init__(self):
            self.calls = 0

        def active_levels(self):
            self.calls += 1
            return (1, 2, 3)

    resource = TupleArrayOutputResource()
    return FastdbArrayOutput, resource


def _make_fastdb_array_input_bridge_contract(namespace: str, *, resource_shape: str = 'list'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbArrayInput:
        def summarize(self, ids):
            ...

    FastdbArrayInput.summarize.__annotations__ = {
        'ids': fdb.Array[fdb.I32],
        'return': fdb.I32,
    }
    FastdbArrayInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbArrayInput)

    if resource_shape == 'list':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: list[int]) -> int:
                self.seen.append(ids)
                return sum(ids)
    elif resource_shape == 'sequence':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: Sequence[int]) -> int:
                self.seen.append(ids)
                return sum(ids)
    elif resource_shape == 'iterable':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: Iterable[int]) -> int:
                self.seen.append((type(ids), ids))
                return sum(ids)
    elif resource_shape == 'mutable_sequence':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: MutableSequence[int]) -> int:
                self.seen.append((type(ids), ids))
                return sum(ids)
    elif resource_shape == 'iterator':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: Iterator[int]) -> int:
                self.seen.append((iter(ids) is ids, list(ids)))
                return sum(self.seen[-1][1])
    elif resource_shape == 'variadic_tuple':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: tuple[int, ...]) -> int:
                self.seen.append((type(ids), ids))
                return sum(ids)
    elif resource_shape == 'bare_tuple':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: tuple) -> int:
                self.seen.append((type(ids), ids))
                return sum(ids)
    elif resource_shape == 'typing_bare_tuple':
        class ArrayInputResource:
            def __init__(self):
                self.seen = []

            def summarize(self, ids: TypingTuple) -> int:
                self.seen.append((type(ids), ids))
                return sum(ids)
    elif resource_shape == 'mapping':
        class ArrayInputResource:
            def summarize(self, ids: Mapping[int, int]) -> int:
                return sum(ids.values())
    elif resource_shape == 'set':
        class ArrayInputResource:
            def summarize(self, ids: set[int]) -> int:
                return sum(ids)
    elif resource_shape == 'nested_sequence':
        class ArrayInputResource:
            def summarize(self, ids: list[list[int]]) -> int:
                return sum(sum(row) for row in ids)
    else:
        raise ValueError(f'unknown array input resource shape {resource_shape!r}')

    resource = ArrayInputResource()
    return (
        FastdbArrayInput,
        resource,
        derive_c_two_bridge(cc, FastdbArrayInput, resource),
    )


def _make_fastdb_scalar_coercion_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb

    class FastdbScalarCoercion:
        def increment(self, level):
            ...

        def invert(self, enabled):
            ...

        def current(self):
            ...

    FastdbScalarCoercion.increment.__annotations__ = {
        'level': fdb.I32,
        'return': fdb.I32,
    }
    FastdbScalarCoercion.invert.__annotations__ = {
        'enabled': fdb.BOOL,
        'return': fdb.BOOL,
    }
    FastdbScalarCoercion.current.__annotations__ = {'return': fdb.I32}
    FastdbScalarCoercion = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbScalarCoercion)

    class ScalarCoercionResource:
        def __init__(self):
            self.seen = []
            self.seen_bool = []

        def increment(self, level):
            self.seen.append((type(level), level))
            return str(level + 1)

        def invert(self, enabled):
            self.seen_bool.append((type(enabled), enabled))
            return 'false' if enabled else 'true'

        def current(self):
            return '42'

    return FastdbScalarCoercion, ScalarCoercionResource()


def _make_fastdb_scalar_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class FastdbScalarBridge:
        def increment(self, level):
            ...

        def invert(self, enabled):
            ...

        def current(self):
            ...

    FastdbScalarBridge.increment.__annotations__ = {
        'level': fdb.I32,
        'return': fdb.I32,
    }
    FastdbScalarBridge.invert.__annotations__ = {
        'enabled': fdb.BOOL,
        'return': fdb.BOOL,
    }
    FastdbScalarBridge.current.__annotations__ = {'return': fdb.I32}
    FastdbScalarBridge = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbScalarBridge)

    class ScalarBridgeResource:
        def __init__(self):
            self.seen = []

        def increment(self, level: int) -> str:
            self.seen.append(('increment', type(level), level))
            return str(level + 1)

        def invert(self, enabled: bool) -> str:
            self.seen.append(('invert', type(enabled), enabled))
            return 'false' if enabled else 'true'

        def current(self) -> str:
            self.seen.append(('current', None, None))
            return '42'

    resource = ScalarBridgeResource()
    return (
        FastdbScalarBridge,
        resource,
        derive_c_two_bridge(cc, FastdbScalarBridge, resource),
    )


def _make_fastdb_blob_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcBlob:
        pass

    RpcBlob.__annotations__ = {'name': fdb.WSTR, 'payload': fdb.BYTES}
    RpcBlob = fdb.feature(RpcBlob)

    class FastdbBlobOutput:
        def blobs(self):
            ...

    FastdbBlobOutput.blobs.__annotations__ = {'return': fdb.Batch[RpcBlob]}
    FastdbBlobOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbBlobOutput)

    class BlobOutputResource:
        def __init__(self):
            self.calls = 0

        def blobs(self) -> list[dict]:
            self.calls += 1
            return [
                {'name': 123, 'payload': bytearray(b'abc')},
                {'name': 'tail', 'payload': memoryview(b'xyz')},
            ]

    resource = BlobOutputResource()
    return (
        RpcBlob,
        FastdbBlobOutput,
        resource,
        derive_c_two_bridge(cc, FastdbBlobOutput, resource),
    )


def _make_fastdb_feature_input_bridge_contract(namespace: str, *, resource_shape: str = 'dict'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbFeatureInput:
        def score(self, point):
            ...

    FastdbFeatureInput.score.__annotations__ = {
        'point': RpcPoint,
        'return': fdb.F64,
    }
    FastdbFeatureInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbFeatureInput)

    if resource_shape == 'dict':
        class DictResource:
            def __init__(self):
                self.seen = []

            def score(self, point: dict[str, object]) -> float:
                self.seen.append(point)
                return float(point['x']) + float(point['y'])

        resource = DictResource()
    elif resource_shape == 'mapping':
        class MappingResource:
            def __init__(self):
                self.seen = []

            def score(self, point: Mapping[str, object]) -> float:
                self.seen.append((type(point), dict(point)))
                return float(point['x']) + float(point['y'])

        resource = MappingResource()
    elif resource_shape == 'mutable_mapping':
        class MutableMappingResource:
            def __init__(self):
                self.seen = []

            def score(self, point: MutableMapping[str, object]) -> float:
                self.seen.append((type(point), dict(point)))
                return float(point['x']) + float(point['y'])

        resource = MutableMappingResource()
    elif resource_shape == 'tuple':
        class TupleResource:
            def __init__(self):
                self.seen = []

            def score(self, point: tuple[float, float, str]) -> float:
                self.seen.append(point)
                return float(point[0]) + float(point[1])

        resource = TupleResource()
    elif resource_shape == 'bare_tuple':
        class BareTupleResource:
            def __init__(self):
                self.seen = []

            def score(self, point: tuple) -> float:
                self.seen.append((type(point), point))
                return float(point[0]) + float(point[1])

        resource = BareTupleResource()
    elif resource_shape == 'typing_bare_tuple':
        class TypingBareTupleResource:
            def __init__(self):
                self.seen = []

            def score(self, point: TypingTuple) -> float:
                self.seen.append((type(point), point))
                return float(point[0]) + float(point[1])

        resource = TypingBareTupleResource()
    else:
        raise AssertionError(f'unknown feature input resource shape {resource_shape!r}')
    return (
        RpcPoint,
        FastdbFeatureInput,
        resource,
        derive_c_two_bridge(cc, FastdbFeatureInput, resource),
    )


def _make_fastdb_split_feature_input_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbSplitFeatureInput:
        def score(self, point):
            ...

    FastdbSplitFeatureInput.score.__annotations__ = {
        'point': RpcPoint,
        'return': fdb.F64,
    }
    FastdbSplitFeatureInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbSplitFeatureInput)

    class SplitParameterResource:
        def __init__(self):
            self.seen = []

        def score(self, label: str, x: float, y: float) -> float:
            self.seen.append((label, x, y))
            return x + y

    resource = SplitParameterResource()
    return (
        RpcPoint,
        FastdbSplitFeatureInput,
        resource,
        derive_c_two_bridge(cc, FastdbSplitFeatureInput, resource),
    )


def _make_fastdb_feature_output_bridge_contract(namespace: str, *, resource_shape: str = 'dict'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbFeatureOutput:
        def origin(self):
            ...

    FastdbFeatureOutput.origin.__annotations__ = {
        'return': RpcPoint,
    }
    FastdbFeatureOutput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbFeatureOutput)

    if resource_shape == 'dict':
        class DictOutputResource:
            def __init__(self):
                self.calls = 0

            def origin(self) -> dict[str, object]:
                self.calls += 1
                return {'x': '1.5', 'y': '2.5', 'label': 123}

        resource = DictOutputResource()
    elif resource_shape == 'tuple':
        class TupleOutputResource:
            def __init__(self):
                self.calls = 0

            def origin(self) -> tuple[float, float, str]:
                self.calls += 1
                return ('1.5', '2.5', 123)

        resource = TupleOutputResource()
    elif resource_shape == 'object':
        class DomainPoint:
            def __init__(self, x: float, y: float, label: str):
                self.x = x
                self.y = y
                self.label = label

        class ObjectOutputResource:
            def __init__(self):
                self.calls = 0

            def origin(self):
                self.calls += 1
                return DomainPoint(x='1.5', y='2.5', label=123)

        ObjectOutputResource.origin.__annotations__ = {'return': DomainPoint}
        resource = ObjectOutputResource()
    elif resource_shape == 'bare_tuple':
        class BareTupleOutputResource:
            def __init__(self):
                self.calls = 0

            def origin(self) -> tuple:
                self.calls += 1
                return ('1.5', '2.5', 123)

        resource = BareTupleOutputResource()
    elif resource_shape == 'typing_bare_tuple':
        class TypingBareTupleOutputResource:
            def __init__(self):
                self.calls = 0

            def origin(self) -> TypingTuple:
                self.calls += 1
                return ('1.5', '2.5', 123)

        resource = TypingBareTupleOutputResource()
    else:
        raise AssertionError(f'unknown feature output resource shape {resource_shape!r}')
    return (
        RpcPoint,
        FastdbFeatureOutput,
        resource,
        derive_c_two_bridge(cc, FastdbFeatureOutput, resource),
    )


def _make_fastdb_batch_dict_input_bridge_contract(namespace: str, *, resource_shape: str = 'dict_list'):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbBatchDictInput:
        def score_many(self, points):
            ...

    FastdbBatchDictInput.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbBatchDictInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbBatchDictInput)

    if resource_shape == 'dict_list':
        class BatchDictInputResource:
            def __init__(self):
                self.seen = []

            def score_many(self, points: list[dict[str, object]]) -> float:
                self.seen.append(points)
                return sum(float(point['x']) + float(point['y']) for point in points)
    elif resource_shape == 'sequence_mapping':
        class BatchDictInputResource:
            def __init__(self):
                self.seen = []

            def score_many(self, points: Sequence[Mapping[str, object]]) -> float:
                rows = [dict(point) for point in points]
                self.seen.append((type(points), rows))
                return sum(point['x'] + point['y'] for point in rows)
    elif resource_shape == 'iterable_mapping':
        class BatchDictInputResource:
            def __init__(self):
                self.seen = []

            def score_many(self, points: Iterable[Mapping[str, object]]) -> float:
                rows = [dict(point) for point in points]
                self.seen.append((type(points), rows))
                return sum(point['x'] + point['y'] for point in rows)
    elif resource_shape == 'iterator_mapping':
        class BatchDictInputResource:
            def __init__(self):
                self.seen = []

            def score_many(self, points: Iterator[Mapping[str, object]]) -> float:
                rows = [dict(point) for point in points]
                self.seen.append((iter(points) is points, rows))
                return sum(point['x'] + point['y'] for point in rows)
    elif resource_shape == 'mutable_sequence_mapping':
        class BatchDictInputResource:
            def __init__(self):
                self.seen = []

            def score_many(self, points: MutableSequence[MutableMapping[str, object]]) -> float:
                rows = [dict(point) for point in points]
                self.seen.append((type(points), rows))
                return sum(point['x'] + point['y'] for point in rows)
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')

    resource = BatchDictInputResource()
    return (
        RpcPoint,
        FastdbBatchDictInput,
        resource,
        derive_c_two_bridge(cc, FastdbBatchDictInput, resource),
    )


def _make_fastdb_variadic_tuple_batch_input_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbVariadicTupleBatchInput:
        def score_many(self, points):
            ...

    FastdbVariadicTupleBatchInput.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbVariadicTupleBatchInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbVariadicTupleBatchInput)

    class DomainPoint:
        def __init__(self, x: float, y: float, label: str):
            self.x = x
            self.y = y
            self.label = label

    class VariadicTupleBatchInputResource:
        def __init__(self):
            self.seen = []

        def score_many(self, points) -> float:
            if resource_shape == 'tuple_row':
                rows = list(points)
                self.seen.append((type(points), rows))
                return sum(row[0] + row[1] for row in rows)
            if resource_shape == 'mapping':
                rows = [dict(point) for point in points]
                self.seen.append((type(points), rows))
                return sum(point['x'] + point['y'] for point in rows)
            rows = [
                (type(point).__name__, point.x, point.y, point.label)
                for point in points
            ]
            self.seen.append((type(points), rows))
            return sum(row[1] + row[2] for row in rows)

    if resource_shape == 'domain_object':
        points_annotation = tuple[DomainPoint, ...]
    elif resource_shape == 'tuple_row':
        points_annotation = tuple[tuple[float, float, str], ...]
    elif resource_shape == 'mapping':
        points_annotation = tuple[Mapping[str, object], ...]
    elif resource_shape == 'feature':
        points_annotation = tuple[RpcPoint, ...]
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')
    VariadicTupleBatchInputResource.score_many.__annotations__ = {
        'points': points_annotation,
        'return': float,
    }

    resource = VariadicTupleBatchInputResource()
    return (
        RpcPoint,
        FastdbVariadicTupleBatchInput,
        resource,
        derive_c_two_bridge(cc, FastdbVariadicTupleBatchInput, resource),
    )


def _make_fastdb_tuple_row_batch_input_bridge_contract(namespace: str, *, resource_shape: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbTupleRowBatchInput:
        def summarize(self, rows):
            ...

    FastdbTupleRowBatchInput.summarize.__annotations__ = {
        'rows': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbTupleRowBatchInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbTupleRowBatchInput)

    class TupleRowBatchInputResource:
        def __init__(self):
            self.seen = []

        def summarize(self, rows) -> int:
            is_iterator = iter(rows) is rows
            materialized = list(rows)
            self.seen.append((is_iterator, materialized))
            return sum(level + global_id for level, global_id in materialized)

    if resource_shape == 'list_tuple_row':
        row_annotation = list[tuple[int, int]]
    elif resource_shape == 'sequence_tuple_row':
        row_annotation = Sequence[tuple[int, int]]
    elif resource_shape == 'iterator_tuple_row':
        row_annotation = Iterator[tuple[int, int]]
    elif resource_shape == 'typing_tuple_row':
        row_annotation = list[TypingTuple[int, int]]
    else:
        raise AssertionError(f'unhandled resource shape: {resource_shape}')
    TupleRowBatchInputResource.summarize.__annotations__ = {
        'rows': row_annotation,
        'return': int,
    }

    resource = TupleRowBatchInputResource()
    return (
        RpcGridId,
        FastdbTupleRowBatchInput,
        resource,
        derive_c_two_bridge(cc, FastdbTupleRowBatchInput, resource),
    )


def _make_fastdb_feature_list_input_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class FastdbFeatureListInput:
        def score_many(self, points):
            ...

    FastdbFeatureListInput.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbFeatureListInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbFeatureListInput)

    class FeatureListResource:
        def __init__(self):
            self.seen = []

        def score_many(self, points) -> float:
            self.seen.append([
                (type(point).__name__, point.x, point.y, point.label)
                for point in points
            ])
            return sum(point.x + point.y for point in points)

    FeatureListResource.score_many.__annotations__ = {
        'points': list[RpcPoint],
        'return': float,
    }

    resource = FeatureListResource()
    return (
        RpcPoint,
        FastdbFeatureListInput,
        resource,
        derive_c_two_bridge(cc, FastdbFeatureListInput, resource),
    )


def _make_fastdb_object_input_bridge_contract(namespace: str):
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcPoint:
        pass

    RpcPoint.__annotations__ = {'x': fdb.F64, 'y': fdb.F64, 'label': fdb.STR}
    RpcPoint = fdb.feature(RpcPoint)

    class DomainPoint:
        def __init__(self, x: float, y: float, label: str):
            self.x = x
            self.y = y
            self.label = label

    class FastdbObjectInput:
        def score(self, point):
            ...

        def score_many(self, points):
            ...

    FastdbObjectInput.score.__annotations__ = {
        'point': RpcPoint,
        'return': fdb.F64,
    }
    FastdbObjectInput.score_many.__annotations__ = {
        'points': fdb.Batch[RpcPoint],
        'return': fdb.F64,
    }
    FastdbObjectInput = cc.crm(
        namespace=f'test.fastdb.{namespace}',
        version='0.1.0',
    )(FastdbObjectInput)

    class ObjectResource:
        def __init__(self):
            self.seen = []

        def score(self, point) -> float:
            self.seen.append(('one', type(point).__name__, point.x, point.y, point.label))
            return point.x + point.y

        def score_many(self, points) -> float:
            self.seen.append(('many', [(type(point).__name__, point.x, point.y, point.label) for point in points]))
            return sum(point.x + point.y for point in points)

    ObjectResource.score.__annotations__ = {'point': DomainPoint, 'return': float}
    ObjectResource.score_many.__annotations__ = {'points': list[DomainPoint], 'return': float}

    resource = ObjectResource()
    return (
        RpcPoint,
        FastdbObjectInput,
        resource,
        derive_c_two_bridge(cc, FastdbObjectInput, resource),
    )


def _assert_fastdb_echo(crm, Point) -> None:
    result = crm.echo(Point(x=1.0, y=2.0, name='rpc'))

    assert result.x == pytest.approx(2.0)
    assert result.y == pytest.approx(4.0)
    assert result.name == 'rpc:ok'


def _assert_fastdb_held_echo(crm, Point) -> None:
    with cc.hold(crm.echo)(Point(x=1.0, y=2.0, name='held')) as held:
        result = held.value.feature('return_0')
        assert isinstance(result, Point)
        assert result.x == pytest.approx(2.0)
        assert result.y == pytest.approx(4.0)
        assert result.name == 'held:ok'


def test_fastdb_crm_payloads_work_thread_local_and_direct_ipc():
    Point, FastdbPoint, FastdbPointResource = _make_fastdb_contract('thread-ipc')
    cc.register(FastdbPoint, FastdbPointResource(), name='fastdb-point-thread-ipc')
    descriptor = build_contract_descriptor(FastdbPoint)
    echo_wire = descriptor['methods'][0]['wire']
    assert 'python-pickle-default' not in repr(descriptor)
    assert echo_wire['input']['id'] == 'org.fastdb.call-db'
    assert echo_wire['output']['id'] == 'org.fastdb.call-db'

    thread_crm = cc.connect(FastdbPoint, name='fastdb-point-thread-ipc')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_fastdb_echo(thread_crm, Point)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbPoint, name='fastdb-point-thread-ipc', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_fastdb_echo(ipc_crm, Point)
        _assert_fastdb_held_echo(ipc_crm, Point)
    finally:
        cc.close(ipc_crm)


def test_fastdb_native_resource_uses_empty_bridge_across_transports(start_c3_relay):
    def expected_rows(call_number: int) -> list[tuple[int, int]]:
        return [
            (call_number, call_number * 10),
            (call_number + 1, call_number * 10 + 10),
        ]

    def assert_rows(rows, call_number: int):
        assert [(row.level, row.global_id) for row in rows] == expected_rows(call_number)

    _GridId, FastdbNativeGrid, resource, bridge = _make_fastdb_native_resource_contract('native-noop-bridge')
    assert bridge == {}
    descriptor = build_contract_descriptor(FastdbNativeGrid)
    method_wire = {
        method['name']: method['wire']
        for method in descriptor['methods']
    }
    assert 'python-pickle-default' not in repr(descriptor)
    assert method_wire['summarize']['input']['id'] == 'org.fastdb.call-db'
    assert method_wire['summarize']['output']['id'] == 'org.fastdb.call-db'
    assert method_wire['active']['input'] is None
    assert method_wire['active']['output']['id'] == 'org.fastdb.call-db'
    cc.register(
        FastdbNativeGrid,
        resource,
        name='fastdb-native-noop-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbNativeGrid, name='fastdb-native-noop-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize([1, 2, 3]) == 6
        assert_rows(thread_crm.active(), 1)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbNativeGrid, name='fastdb-native-noop-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize([4, 5, 6]) == 15
        assert_rows(ipc_crm.active(), 2)
        with cc.hold(ipc_crm.active)() as held:
            assert_rows(held.value.table('return_0'), 3)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-native-noop-bridge-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbNativeGrid,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbNativeGrid, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize([7, 8, 9]) == 24
        assert_rows(relay_crm.active(), 4)
        with cc.hold(relay_crm.active)() as held:
            assert_rows(held.value.table('return_0'), 5)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        ('summarize', [1, 2, 3]),
        ('active', 1),
        ('summarize', [4, 5, 6]),
        ('active', 2),
        ('active', 3),
        ('summarize', [7, 8, 9]),
        ('active', 4),
        ('active', 5),
    ]

def test_fastdb_derived_bridge_maps_batch_input_to_tuple_columns_thread_local_and_direct_ipc():
    GridId, FastdbBatchInput, resource, bridge = _make_fastdb_batch_input_bridge_contract('batch-input-bridge')
    cc.register(
        FastdbBatchInput,
        resource,
        name='fastdb-batch-input-bridge',
        bridge=bridge,
    )

    rows = [
        GridId(level=1, global_id=10),
        GridId(level=2, global_id=20),
    ]
    thread_crm = cc.connect(FastdbBatchInput, name='fastdb-batch-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbBatchInput, name='fastdb-batch-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows) == 33
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        ([1, 2], [10, 20]),
        ([1, 2], [10, 20]),
    ]


def test_fastdb_batch_column_bridge_rejects_nested_column_items_before_registration():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb
    from c_two.fastdb.bridge import derive_c_two_bridge

    class RpcGridId:
        pass

    RpcGridId.__annotations__ = {'level': fdb.I32, 'global_id': fdb.I32}
    RpcGridId = fdb.feature(RpcGridId)

    class FastdbBatchInput:
        def summarize(self, ids):
            ...

    FastdbBatchInput.summarize.__annotations__ = {
        'ids': fdb.Batch[RpcGridId],
        'return': fdb.I32,
    }
    FastdbBatchInput = cc.crm(
        namespace='test.fastdb.batch-column-item-guardrail',
        version='0.1.0',
    )(FastdbBatchInput)

    class NestedColumnResource:
        def summarize(self, ids: tuple[list[list[int]], list[int]]) -> int:
            return 0

    with pytest.raises(TypeError, match='scalar column item'):
        derive_c_two_bridge(cc, FastdbBatchInput, NestedColumnResource())


def test_fastdb_derived_bridge_maps_batch_input_to_tuple_columns_explicit_http_relay(start_c3_relay):
    GridId, FastdbBatchInput, resource, bridge = _make_fastdb_batch_input_bridge_contract('batch-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbBatchInput,
        resource,
        name='fastdb-batch-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbBatchInput, name='fastdb-batch-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.summarize([
            GridId(level=3, global_id=30),
            GridId(level=4, global_id=40),
        ]) == 77
    finally:
        cc.close(crm)

    assert resource.seen == [([3, 4], [30, 40])]


@pytest.mark.parametrize(
    ('resource_shape', 'expected_iterator_flags'),
    [
        ('tuple_sequence_columns', (False, False)),
        ('tuple_iterator_columns', (True, True)),
        ('mapping_sequence_columns', (False, False)),
        ('mapping_iterator_columns', (True, True)),
        ('mutable_mapping_columns', (False, False)),
        ('typing_mapping_columns', (False, False)),
        ('typing_mutable_mapping_columns', (False, False)),
    ],
)
def test_fastdb_derived_bridge_maps_batch_input_to_column_containers_across_transports(
    start_c3_relay,
    resource_shape,
    expected_iterator_flags,
):
    namespace = f'batch-column-container-input-{resource_shape.replace("_", "-")}'
    route_name = f'fastdb-batch-column-container-input-{resource_shape.replace("_", "-")}'
    GridId, FastdbColumnContainerBatchInput, resource, bridge = _make_fastdb_column_container_batch_input_bridge_contract(
        namespace,
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbColumnContainerBatchInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(level_a: int, id_a: int, level_b: int, id_b: int):
        return [
            GridId(level=level_a, global_id=id_a),
            GridId(level=level_b, global_id=id_b),
        ]

    thread_crm = cc.connect(FastdbColumnContainerBatchInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows(1, 10, 2, 20)) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbColumnContainerBatchInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows(3, 30, 4, 40)) == 77
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbColumnContainerBatchInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbColumnContainerBatchInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize(rows(5, 50, 6, 60)) == 121
    finally:
        cc.close(relay_crm)

    def expected_entry(level_a: int, id_a: int, level_b: int, id_b: int):
        return (expected_iterator_flags, [level_a, level_b], [id_a, id_b])

    assert resource.seen == [
        expected_entry(1, 10, 2, 20),
        expected_entry(3, 30, 4, 40),
        expected_entry(5, 50, 6, 60),
    ]


def test_fastdb_derived_bridge_maps_batch_input_to_split_column_parameters_thread_local_and_direct_ipc():
    GridId, FastdbSplitBatchInput, resource, bridge = _make_fastdb_split_batch_input_bridge_contract('batch-split-input-bridge')
    cc.register(
        FastdbSplitBatchInput,
        resource,
        name='fastdb-batch-split-input-bridge',
        bridge=bridge,
    )

    rows = [
        GridId(level=1, global_id=10),
        GridId(level=2, global_id=20),
    ]
    thread_crm = cc.connect(FastdbSplitBatchInput, name='fastdb-batch-split-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbSplitBatchInput, name='fastdb-batch-split-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows) == 33
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        ([10, 20], [1, 2]),
        ([10, 20], [1, 2]),
    ]


def test_fastdb_derived_bridge_maps_batch_input_to_split_column_parameters_explicit_http_relay(start_c3_relay):
    GridId, FastdbSplitBatchInput, resource, bridge = _make_fastdb_split_batch_input_bridge_contract('batch-split-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbSplitBatchInput,
        resource,
        name='fastdb-batch-split-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbSplitBatchInput, name='fastdb-batch-split-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.summarize([
            GridId(level=3, global_id=30),
            GridId(level=4, global_id=40),
        ]) == 77
    finally:
        cc.close(crm)

    assert resource.seen == [([30, 40], [3, 4])]


@pytest.mark.parametrize('resource_shape', ['sequence', 'mutable_sequence', 'iterator'])
def test_fastdb_derived_bridge_maps_batch_input_to_split_column_parameter_containers_across_transports(
    start_c3_relay,
    resource_shape,
):
    namespace_shape = resource_shape.replace('_', '-')
    route_name = f'fastdb-batch-split-input-{namespace_shape}-bridge'
    GridId, FastdbSplitBatchInput, resource, bridge = _make_fastdb_split_batch_input_bridge_contract(
        f'batch-split-input-{namespace_shape}-bridge',
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbSplitBatchInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(level_a: int, id_a: int, level_b: int, id_b: int):
        return [
            GridId(level=level_a, global_id=id_a),
            GridId(level=level_b, global_id=id_b),
        ]

    thread_crm = cc.connect(FastdbSplitBatchInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows(1, 10, 2, 20)) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbSplitBatchInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows(3, 30, 4, 40)) == 77
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbSplitBatchInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbSplitBatchInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize(rows(5, 50, 6, 60)) == 121
    finally:
        cc.close(relay_crm)

    def expected_seen(level_a: int, id_a: int, level_b: int, id_b: int):
        if resource_shape == 'iterator':
            return (True, [id_a, id_b], True, [level_a, level_b])
        return ([id_a, id_b], [level_a, level_b])

    assert resource.seen == [
        expected_seen(1, 10, 2, 20),
        expected_seen(3, 30, 4, 40),
        expected_seen(5, 50, 6, 60),
    ]


def test_fastdb_derived_bridge_maps_batch_input_to_dict_columns_thread_local_and_direct_ipc():
    GridId, FastdbDictColumnInput, resource, bridge = _make_fastdb_dict_column_input_bridge_contract('batch-dict-column-input-bridge')
    cc.register(
        FastdbDictColumnInput,
        resource,
        name='fastdb-batch-dict-column-input-bridge',
        bridge=bridge,
    )

    rows = [
        GridId(level=1, global_id=10),
        GridId(level=2, global_id=20),
    ]
    thread_crm = cc.connect(FastdbDictColumnInput, name='fastdb-batch-dict-column-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbDictColumnInput, name='fastdb-batch-dict-column-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows) == 33
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        {'level': [1, 2], 'global_id': [10, 20]},
        {'level': [1, 2], 'global_id': [10, 20]},
    ]


def test_fastdb_derived_bridge_maps_batch_input_to_dict_columns_explicit_http_relay(start_c3_relay):
    GridId, FastdbDictColumnInput, resource, bridge = _make_fastdb_dict_column_input_bridge_contract('batch-dict-column-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbDictColumnInput,
        resource,
        name='fastdb-batch-dict-column-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbDictColumnInput, name='fastdb-batch-dict-column-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.summarize([
            GridId(level=3, global_id=30),
            GridId(level=4, global_id=40),
        ]) == 77
    finally:
        cc.close(crm)

    assert resource.seen == [{'level': [3, 4], 'global_id': [30, 40]}]


def test_fastdb_derived_bridge_maps_batch_input_to_table_object_thread_local_and_direct_ipc():
    GridId, FastdbTableObjectInput, resource, bridge = _make_fastdb_table_object_input_bridge_contract('batch-table-object-input-bridge')
    cc.register(
        FastdbTableObjectInput,
        resource,
        name='fastdb-batch-table-object-input-bridge',
        bridge=bridge,
    )

    rows = [
        GridId(level=1, global_id=10),
        GridId(level=2, global_id=20),
    ]
    thread_crm = cc.connect(FastdbTableObjectInput, name='fastdb-batch-table-object-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTableObjectInput, name='fastdb-batch-table-object-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows) == 33
    finally:
        cc.close(ipc_crm)

    expected = [{'level': 1, 'global_id': 10}, {'level': 2, 'global_id': 20}]
    assert resource.seen == [expected, expected]


def test_fastdb_derived_bridge_maps_batch_input_to_table_object_explicit_http_relay(start_c3_relay):
    GridId, FastdbTableObjectInput, resource, bridge = _make_fastdb_table_object_input_bridge_contract('batch-table-object-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTableObjectInput,
        resource,
        name='fastdb-batch-table-object-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbTableObjectInput, name='fastdb-batch-table-object-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.summarize([
            GridId(level=3, global_id=30),
            GridId(level=4, global_id=40),
        ]) == 77
    finally:
        cc.close(crm)

    assert resource.seen == [[{'level': 3, 'global_id': 30}, {'level': 4, 'global_id': 40}]]


@pytest.mark.parametrize(
    ('resource_shape', 'expected_table_type'),
    [
        ('from_records', 'FromRecordsTable'),
        ('constructor_instance_factory', 'ConstructorTableWithInstanceFactory'),
    ],
)
def test_fastdb_derived_bridge_maps_batch_input_to_table_object_factories_across_transports(
    start_c3_relay,
    resource_shape,
    expected_table_type,
):
    namespace_shape = resource_shape.replace('_', '-')
    route_name = f'fastdb-batch-table-object-input-{namespace_shape}-bridge'
    GridId, FastdbTableObjectInput, resource, bridge = _make_fastdb_table_object_input_bridge_contract(
        f'batch-table-object-input-{namespace_shape}-bridge',
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbTableObjectInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(level_a, id_a, level_b, id_b):
        return [
            GridId(level=level_a, global_id=id_a),
            GridId(level=level_b, global_id=id_b),
        ]

    def expected_seen(level_a, id_a, level_b, id_b):
        return (
            expected_table_type,
            [
                {'level': level_a, 'global_id': id_a},
                {'level': level_b, 'global_id': id_b},
            ],
        )

    thread_crm = cc.connect(FastdbTableObjectInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows(1, 10, 2, 20)) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTableObjectInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows(3, 30, 4, 40)) == 77
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTableObjectInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbTableObjectInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize(rows(5, 50, 6, 60)) == 121
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        expected_seen(1, 10, 2, 20),
        expected_seen(3, 30, 4, 40),
        expected_seen(5, 50, 6, 60),
    ]


def _assert_grid_ids(rows, expected: Sequence[tuple[int, int]] = ((1, 10), (2, 20))) -> None:
    assert [(row.level, row.global_id) for row in rows] == list(expected)


def _assert_held_grid_ids(held, expected: Sequence[tuple[int, int]] = ((1, 10), (2, 20))) -> None:
    table = held.value.table('return_0')
    _assert_grid_ids(table, expected)
    assert table.column.global_id[1] == expected[1][1]


def _assert_held_levels(held) -> None:
    array = held.value.array('return_0')
    assert array.materialize() == [1, 2, 3]
    assert list(array) == [1, 2, 3]
    assert array[1] == 2


def _assert_origin_point(result, Point) -> None:
    assert isinstance(result, Point)
    assert (result.x, result.y, result.label) == (1.5, 2.5, '123')


def _assert_held_origin_point(held, Point) -> None:
    _assert_origin_point(held.value.feature('return_0'), Point)
    assert held.value.feature(0).label == '123'


def _assert_blob_rows(rows) -> None:
    materialized = [(row.name, bytes(row.payload)) for row in rows]
    assert materialized == [('123', b'abc'), ('tail', b'xyz')]


def _assert_held_blob_rows(held) -> None:
    table = held.value.table('return_0')
    _assert_blob_rows(table)
    assert table.column.name[0] == '123'
    assert bytes(table.column.payload[1]) == b'xyz'


def test_fastdb_derived_bridge_maps_dict_columns_to_batch_output_thread_local_and_direct_ipc():
    GridId, FastdbDictColumnOutput, resource, bridge = _make_fastdb_dict_column_output_bridge_contract('dict-column-output-bridge')
    cc.register(
        FastdbDictColumnOutput,
        resource,
        name='fastdb-dict-column-output-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbDictColumnOutput, name='fastdb-dict-column-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_grid_ids(thread_crm.active())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbDictColumnOutput, name='fastdb-dict-column-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_grid_ids(ipc_crm.active())
        with cc.hold(ipc_crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 3


def test_fastdb_derived_bridge_accepts_abstract_container_annotations_across_transports(start_c3_relay):
    GridId, Point, FastdbAbstractContainers, resource, bridge = _make_fastdb_abstract_container_bridge_contract('abstract-container-bridge')
    cc.register(
        FastdbAbstractContainers,
        resource,
        name='fastdb-abstract-container-bridge',
        bridge=bridge,
    )

    grid_rows = [
        GridId(level=1, global_id=10),
        GridId(level=2, global_id=20),
    ]
    point_rows = [
        Point(x=1.5, y=2.5, label='center'),
        Point(x=3.5, y=4.5, label='edge'),
    ]
    thread_crm = cc.connect(FastdbAbstractContainers, name='fastdb-abstract-container-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(grid_rows) == 33
        assert thread_crm.summarize_mutable(grid_rows) == 33
        _assert_grid_ids(thread_crm.active())
        _assert_grid_ids(thread_crm.active_mutable())
        assert thread_crm.score_many(point_rows) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbAbstractContainers, name='fastdb-abstract-container-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(grid_rows) == 33
        assert ipc_crm.summarize_mutable(grid_rows) == 33
        _assert_grid_ids(ipc_crm.active())
        _assert_grid_ids(ipc_crm.active_mutable())
        assert ipc_crm.score_many(point_rows) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-abstract-container-bridge-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbAbstractContainers,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbAbstractContainers, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize(grid_rows) == 33
        assert relay_crm.summarize_mutable(grid_rows) == 33
        _assert_grid_ids(relay_crm.active())
        _assert_grid_ids(relay_crm.active_mutable())
        assert relay_crm.score_many(point_rows) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    expected_segment = [
        ('summarize', {'level': [1, 2], 'global_id': [10, 20]}),
        ('summarize_mutable', {'level': [1, 2], 'global_id': [10, 20]}),
        ('active', None),
        ('active_mutable', None),
        ('score_many', [
            {'x': 1.5, 'y': 2.5, 'label': 'center'},
            {'x': 3.5, 'y': 4.5, 'label': 'edge'},
        ]),
    ]
    assert resource.seen == [
        *expected_segment,
        *expected_segment,
        *expected_segment,
    ]


def test_fastdb_derived_bridge_maps_tuple_return_thread_local_and_direct_ipc():
    GridId, FastdbTupleReturn, resource, bridge = _make_fastdb_tuple_return_bridge_contract('tuple-return-bridge')
    cc.register(
        FastdbTupleReturn,
        resource,
        name='fastdb-tuple-return-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbTupleReturn, name='fastdb-tuple-return-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        levels, rows = thread_crm.active_pair()
        assert levels == [1, 2]
        _assert_grid_ids(rows)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTupleReturn, name='fastdb-tuple-return-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        levels, rows = ipc_crm.active_pair()
        assert levels == [1, 2]
        _assert_grid_ids(rows)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 2


def test_fastdb_derived_bridge_tuple_return_hold_across_remote_transports(start_c3_relay):
    def assert_held_tuple_view(held):
        view = held.value
        raw_buffer = held.buffer
        assert view.array('return_0').materialize() == [1, 2]
        _assert_grid_ids(view.table('return_1'))
        assert view.table('return_1').column.global_id[1] == 20
        held.release()
        with pytest.raises(RuntimeError, match='released'):
            _ = held.value
        with pytest.raises(ValueError):
            _ = raw_buffer.nbytes
        with pytest.raises(RuntimeError, match='released'):
            view.array('return_0')

    _GridId, FastdbTupleReturn, ipc_resource, ipc_bridge = _make_fastdb_tuple_return_bridge_contract('tuple-return-hold-ipc')
    cc.register(
        FastdbTupleReturn,
        ipc_resource,
        name='fastdb-tuple-return-hold-ipc',
        bridge=ipc_bridge,
    )
    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTupleReturn, name='fastdb-tuple-return-hold-ipc', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert_held_tuple_view(cc.hold(ipc_crm.active_pair)())
    finally:
        cc.close(ipc_crm)

    _RelayGridId, RelayTupleReturn, relay_resource, relay_bridge = _make_fastdb_tuple_return_bridge_contract('tuple-return-hold-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        RelayTupleReturn,
        relay_resource,
        name='fastdb-tuple-return-hold-relay',
        bridge=relay_bridge,
    )
    relay_crm = cc.connect(RelayTupleReturn, name='fastdb-tuple-return-hold-relay', address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert_held_tuple_view(cc.hold(relay_crm.active_pair)())
    finally:
        cc.close(relay_crm)

    assert ipc_resource.calls == 1
    assert relay_resource.calls == 1


def test_fastdb_derived_bridge_rejects_mapping_array_output_annotation_before_registration():
    with pytest.raises(TypeError, match='cannot derive Array output from'):
        _make_fastdb_mapping_array_output_bridge_contract('array-output-mapping-guardrail')


def test_fastdb_crm_array_output_accepts_tuple_direct_ipc():
    FastdbArrayOutput, resource = _make_fastdb_array_output_iterable_contract('array-output-iterable')
    cc.register(FastdbArrayOutput, resource, name='fastdb-array-output-iterable')

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterable', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.active_levels() == [1, 2, 3]
        with cc.hold(ipc_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 2


def test_fastdb_crm_array_output_accepts_tuple_explicit_http_relay(start_c3_relay):
    FastdbArrayOutput, resource = _make_fastdb_array_output_iterable_contract('array-output-iterable-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(FastdbArrayOutput, resource, name='fastdb-array-output-iterable-relay')

    crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterable-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.active_levels() == [1, 2, 3]
        with cc.hold(crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(crm)

    assert resource.calls == 2


def test_fastdb_array_output_bridge_accepts_mutable_sequence_across_transports(start_c3_relay):
    FastdbArrayOutput, resource, bridge = _make_fastdb_mutable_sequence_array_output_bridge_contract(
        'array-output-mutable-sequence',
    )
    cc.register(
        FastdbArrayOutput,
        resource,
        name='fastdb-array-output-mutable-sequence',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-mutable-sequence')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.active_levels() == [1, 2, 3]
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-mutable-sequence', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.active_levels() == [1, 2, 3]
        with cc.hold(ipc_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-array-output-mutable-sequence-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.active_levels() == [1, 2, 3]
        with cc.hold(relay_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


@pytest.mark.parametrize('resource_shape', ['list', 'sequence'])
def test_fastdb_array_output_bridge_accepts_list_and_sequence_across_transports(start_c3_relay, resource_shape):
    FastdbArrayOutput, resource, bridge = _make_fastdb_sequence_array_output_bridge_contract(
        f'array-output-{resource_shape}-annotation',
        resource_shape=resource_shape,
    )
    local_name = f'fastdb-array-output-{resource_shape}-annotation'
    cc.register(
        FastdbArrayOutput,
        resource,
        name=local_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayOutput, name=local_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.active_levels() == [1, 2, 3]
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayOutput, name=local_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.active_levels() == [1, 2, 3]
        with cc.hold(ipc_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{local_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.active_levels() == [1, 2, 3]
        with cc.hold(relay_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_array_output_bridge_accepts_iterable_across_transports(start_c3_relay):
    FastdbArrayOutput, resource, bridge = _make_fastdb_iterable_array_output_bridge_contract(
        'array-output-iterable-annotation',
    )
    cc.register(
        FastdbArrayOutput,
        resource,
        name='fastdb-array-output-iterable-annotation',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterable-annotation')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.active_levels() == [1, 2, 3]
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterable-annotation', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.active_levels() == [1, 2, 3]
        with cc.hold(ipc_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-array-output-iterable-annotation-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.active_levels() == [1, 2, 3]
        with cc.hold(relay_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_array_output_bridge_accepts_iterator_across_transports(start_c3_relay):
    FastdbArrayOutput, resource, bridge = _make_fastdb_iterator_array_output_bridge_contract(
        'array-output-iterator-annotation',
    )
    cc.register(
        FastdbArrayOutput,
        resource,
        name='fastdb-array-output-iterator-annotation',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterator-annotation')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.active_levels() == [1, 2, 3]
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayOutput, name='fastdb-array-output-iterator-annotation', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.active_levels() == [1, 2, 3]
        with cc.hold(ipc_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-array-output-iterator-annotation-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.active_levels() == [1, 2, 3]
        with cc.hold(relay_crm.active_levels)() as held:
            _assert_held_levels(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_array_input_accepts_tuple_thread_local_and_direct_ipc():
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract('array-input-iterable')
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-iterable',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterable')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize((1, 2, 3)) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterable', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [[1, 2, 3], [4, 5, 6]]


def test_fastdb_array_input_accepts_sequence_thread_local_and_direct_ipc():
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-sequence',
        resource_shape='sequence',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-sequence',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-sequence')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize((1, 2, 3)) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-sequence', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [[1, 2, 3], [4, 5, 6]]


def test_fastdb_array_input_accepts_iterable_thread_local_and_direct_ipc():
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-iterable-annotation',
        resource_shape='iterable',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-iterable-annotation',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterable-annotation')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(('1', '2', '3')) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterable-annotation', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        (list, [1, 2, 3]),
        (list, [4, 5, 6]),
    ]


def test_fastdb_array_input_accepts_mutable_sequence_thread_local_and_direct_ipc():
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-mutable-sequence',
        resource_shape='mutable_sequence',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-mutable-sequence',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-mutable-sequence')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(('1', '2', '3')) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-mutable-sequence', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        (list, [1, 2, 3]),
        (list, [4, 5, 6]),
    ]


def test_fastdb_array_input_accepts_iterator_across_transports(start_c3_relay):
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-iterator',
        resource_shape='iterator',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-iterator',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterator')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(('1', '2', '3')) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterator', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-array-input-iterator-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize([7, 8, 9]) == 24
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        (True, [1, 2, 3]),
        (True, [4, 5, 6]),
        (True, [7, 8, 9]),
    ]


def test_fastdb_array_input_accepts_typing_bare_tuple_thread_local_and_direct_ipc():
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-typing-bare-tuple',
        resource_shape='typing_bare_tuple',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-typing-bare-tuple',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-typing-bare-tuple')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize((1, 2, 3)) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-typing-bare-tuple', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        (tuple, (1, 2, 3)),
        (tuple, (4, 5, 6)),
    ]


@pytest.mark.parametrize(
    ('resource_shape', 'expected_seen'),
    [
        ('sequence', [[1, 2, 3], [4, 5, 6], [7, 8, 9]]),
        ('iterable', [(list, [1, 2, 3]), (list, [4, 5, 6]), (list, [7, 8, 9])]),
        ('mutable_sequence', [(list, [1, 2, 3]), (list, [4, 5, 6]), (list, [7, 8, 9])]),
        ('bare_tuple', [(tuple, (1, 2, 3)), (tuple, (4, 5, 6)), (tuple, (7, 8, 9))]),
        ('typing_bare_tuple', [(tuple, (1, 2, 3)), (tuple, (4, 5, 6)), (tuple, (7, 8, 9))]),
    ],
)
def test_fastdb_array_input_accepts_container_annotations_across_transports(
    start_c3_relay,
    resource_shape,
    expected_seen,
):
    namespace_shape = resource_shape.replace('_', '-')
    route_name = f'fastdb-array-input-{namespace_shape}-container'
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        f'array-input-{namespace_shape}-container',
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(('1', '2', '3')) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize((4, 5, 6)) == 15
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize([7, 8, 9]) == 24
    finally:
        cc.close(relay_crm)

    assert resource.seen == expected_seen


def test_fastdb_array_input_bridge_rejects_unsupported_resource_annotations():
    for resource_shape in ('mapping', 'set', 'nested_sequence'):
        with pytest.raises(TypeError, match='cannot derive Array input'):
            _make_fastdb_array_input_bridge_contract(
                f'array-input-bad-{resource_shape}',
                resource_shape=resource_shape,
            )


def test_fastdb_array_input_accepts_tuple_explicit_http_relay(start_c3_relay):
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract('array-input-iterable-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-iterable-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-iterable-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.summarize((7, 8, 9)) == 24
    finally:
        cc.close(crm)

    assert resource.seen == [[7, 8, 9]]


def test_fastdb_array_input_maps_to_variadic_tuple_resource_across_transports(start_c3_relay):
    FastdbArrayInput, resource, bridge = _make_fastdb_array_input_bridge_contract(
        'array-input-variadic-tuple',
        resource_shape='variadic_tuple',
    )
    cc.register(
        FastdbArrayInput,
        resource,
        name='fastdb-array-input-variadic-tuple',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-variadic-tuple')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(['1', '2', '3']) == 6
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbArrayInput, name='fastdb-array-input-variadic-tuple', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize([4, 5, 6]) == 15
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-array-input-variadic-tuple-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbArrayInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbArrayInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize([7, 8, 9]) == 24
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        (tuple, (1, 2, 3)),
        (tuple, (4, 5, 6)),
        (tuple, (7, 8, 9)),
    ]


def test_fastdb_scalar_call_db_coercion_direct_ipc_and_explicit_http_relay(start_c3_relay):
    FastdbScalarCoercion, resource = _make_fastdb_scalar_coercion_contract('scalar-coercion')
    cc.register(FastdbScalarCoercion, resource, name='fastdb-scalar-coercion')

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbScalarCoercion, name='fastdb-scalar-coercion', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.increment('6') == 7
        assert ipc_crm.invert('false') is True
        assert ipc_crm.current() == 42
        with cc.hold(ipc_crm.increment)('10') as held:
            assert held.value.scalar('return_0') == 11
        with cc.hold(ipc_crm.invert)('true') as held:
            assert held.value.scalar('return_0') is False
        with cc.hold(ipc_crm.current)() as held:
            assert held.value.scalar('return_0') == 42
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-scalar-coercion-relay'
    FastdbRelayScalarCoercion, relay_resource = _make_fastdb_scalar_coercion_contract('scalar-coercion-relay')
    cc.set_relay_anchor(relay.url)
    cc.register(FastdbRelayScalarCoercion, relay_resource, name=relay_name)

    relay_crm = cc.connect(FastdbRelayScalarCoercion, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.increment('8') == 9
        assert relay_crm.invert('true') is False
        assert relay_crm.current() == 42
        with cc.hold(relay_crm.increment)('12') as held:
            assert held.value.scalar('return_0') == 13
        with cc.hold(relay_crm.invert)('false') as held:
            assert held.value.scalar('return_0') is True
        with cc.hold(relay_crm.current)() as held:
            assert held.value.scalar('return_0') == 42
    finally:
        cc.close(relay_crm)

    assert resource.seen == [(int, 6), (int, 10)]
    assert resource.seen_bool == [(bool, False), (bool, True)]
    assert relay_resource.seen == [(int, 8), (int, 12)]
    assert relay_resource.seen_bool == [(bool, True), (bool, False)]


def test_fastdb_derived_scalar_bridge_across_transports(start_c3_relay):
    FastdbScalarBridge, resource, bridge = _make_fastdb_scalar_bridge_contract('scalar-bridge')
    cc.register(
        FastdbScalarBridge,
        resource,
        name='fastdb-scalar-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbScalarBridge, name='fastdb-scalar-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.increment('6') == 7
        assert thread_crm.invert('false') is True
        assert thread_crm.current() == 42
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbScalarBridge, name='fastdb-scalar-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.increment('10') == 11
        assert ipc_crm.invert('true') is False
        with cc.hold(ipc_crm.current)() as held:
            assert held.value.scalar('return_0') == 42
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-scalar-bridge-relay'
    FastdbRelayScalarBridge, relay_resource, relay_bridge = _make_fastdb_scalar_bridge_contract('scalar-bridge-relay')
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbRelayScalarBridge,
        relay_resource,
        name=relay_name,
        bridge=relay_bridge,
    )

    relay_crm = cc.connect(FastdbRelayScalarBridge, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.increment('12') == 13
        assert relay_crm.invert('false') is True
        with cc.hold(relay_crm.current)() as held:
            assert held.value.scalar('return_0') == 42
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        ('increment', int, 6),
        ('invert', bool, False),
        ('current', None, None),
        ('increment', int, 10),
        ('invert', bool, True),
        ('current', None, None),
    ]
    assert relay_resource.seen == [
        ('increment', int, 12),
        ('invert', bool, False),
        ('current', None, None),
    ]


def test_fastdb_derived_bridge_maps_wstr_and_bytes_feature_output_across_transports(start_c3_relay):
    _, FastdbBlobOutput, resource, bridge = _make_fastdb_blob_bridge_contract('blob-output-bridge')
    cc.register(
        FastdbBlobOutput,
        resource,
        name='fastdb-blob-output-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbBlobOutput, name='fastdb-blob-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_blob_rows(thread_crm.blobs())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbBlobOutput, name='fastdb-blob-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_blob_rows(ipc_crm.blobs())
        with cc.hold(ipc_crm.blobs)() as held:
            _assert_held_blob_rows(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-blob-output-bridge-relay'
    _, FastdbRelayBlobOutput, relay_resource, relay_bridge = _make_fastdb_blob_bridge_contract('blob-output-bridge-relay')
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbRelayBlobOutput,
        relay_resource,
        name=relay_name,
        bridge=relay_bridge,
    )

    relay_crm = cc.connect(FastdbRelayBlobOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        _assert_blob_rows(relay_crm.blobs())
        with cc.hold(relay_crm.blobs)() as held:
            _assert_held_blob_rows(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 3
    assert relay_resource.calls == 2


def test_fastdb_derived_bridge_maps_table_like_batch_output_thread_local_and_direct_ipc():
    GridId, FastdbTableLikeOutput, resource, bridge = _make_fastdb_table_like_output_bridge_contract('table-like-output-bridge')
    cc.register(
        FastdbTableLikeOutput,
        resource,
        name='fastdb-table-like-output-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbTableLikeOutput, name='fastdb-table-like-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_grid_ids(thread_crm.active())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTableLikeOutput, name='fastdb-table-like-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_grid_ids(ipc_crm.active())
        with cc.hold(ipc_crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 3


def test_fastdb_derived_bridge_maps_table_like_batch_output_explicit_http_relay(start_c3_relay):
    GridId, FastdbTableLikeOutput, resource, bridge = _make_fastdb_table_like_output_bridge_contract(
        'table-like-output-bridge-relay',
    )
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTableLikeOutput,
        resource,
        name='fastdb-table-like-output-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbTableLikeOutput, name='fastdb-table-like-output-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_grid_ids(crm.active())
        with cc.hold(crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(crm)

    assert resource.calls == 2


def test_fastdb_derived_bridge_maps_non_iterable_table_like_batch_output_thread_local_and_direct_ipc():
    GridId, FastdbTableLikeOutput, resource, bridge = _make_fastdb_table_like_output_bridge_contract(
        'non-iterable-table-like-output-bridge',
        iterable_table=False,
    )
    cc.register(
        FastdbTableLikeOutput,
        resource,
        name='fastdb-non-iterable-table-like-output-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbTableLikeOutput, name='fastdb-non-iterable-table-like-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_grid_ids(thread_crm.active())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTableLikeOutput, name='fastdb-non-iterable-table-like-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_grid_ids(ipc_crm.active())
        with cc.hold(ipc_crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 3


def test_fastdb_derived_bridge_maps_non_iterable_table_like_batch_output_explicit_http_relay(start_c3_relay):
    GridId, FastdbTableLikeOutput, resource, bridge = _make_fastdb_table_like_output_bridge_contract(
        'non-iterable-table-like-output-bridge-relay',
        iterable_table=False,
    )
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTableLikeOutput,
        resource,
        name='fastdb-non-iterable-table-like-output-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbTableLikeOutput, name='fastdb-non-iterable-table-like-output-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_grid_ids(crm.active())
        with cc.hold(crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(crm)

    assert resource.calls == 2


def test_fastdb_derived_bridge_maps_table_like_tuple_row_batch_output_across_transports(start_c3_relay):
    Point, FastdbTableLikeTupleRowOutput, resource, bridge = _make_fastdb_table_like_tuple_row_output_bridge_contract(
        'table-like-tuple-row-output-bridge',
    )
    cc.register(
        FastdbTableLikeTupleRowOutput,
        resource,
        name='fastdb-table-like-tuple-row-output-bridge',
        bridge=bridge,
    )

    def assert_points(result):
        assert len(result) == 2
        assert isinstance(result[0], Point)
        assert [(point.x, point.y, point.label) for point in result] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]

    def assert_held_points(held):
        table = held.value.table('return_0')
        assert [(row.x, row.y, row.label) for row in table] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]
        assert table.column.x[0] == pytest.approx(1.5)
        assert table.column.label.to_pylist()[1] == 'edge'

    thread_crm = cc.connect(FastdbTableLikeTupleRowOutput, name='fastdb-table-like-tuple-row-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert_points(thread_crm.active_points())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTableLikeTupleRowOutput, name='fastdb-table-like-tuple-row-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert_points(ipc_crm.active_points())
        with cc.hold(ipc_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-table-like-tuple-row-output-bridge-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTableLikeTupleRowOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbTableLikeTupleRowOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert_points(relay_crm.active_points())
        with cc.hold(relay_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_derived_bridge_maps_iterator_tuple_row_batch_output_across_transports(start_c3_relay):
    Point, FastdbIteratorTupleRowOutput, resource, bridge = _make_fastdb_iterator_tuple_row_output_bridge_contract(
        'iterator-tuple-row-output-bridge',
    )
    assert bridge
    cc.register(
        FastdbIteratorTupleRowOutput,
        resource,
        name='fastdb-iterator-tuple-row-output-bridge',
        bridge=bridge,
    )

    def assert_points(result):
        assert len(result) == 2
        assert isinstance(result[0], Point)
        assert [(point.x, point.y, point.label) for point in result] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]

    def assert_held_points(held):
        table = held.value.table('return_0')
        assert [(row.x, row.y, row.label) for row in table] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]
        assert table.column.x[0] == pytest.approx(1.5)
        assert table.column.label.to_pylist()[1] == 'edge'

    thread_crm = cc.connect(FastdbIteratorTupleRowOutput, name='fastdb-iterator-tuple-row-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert_points(thread_crm.active_points())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbIteratorTupleRowOutput, name='fastdb-iterator-tuple-row-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert_points(ipc_crm.active_points())
        with cc.hold(ipc_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-iterator-tuple-row-output-bridge-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbIteratorTupleRowOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbIteratorTupleRowOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert_points(relay_crm.active_points())
        with cc.hold(relay_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


@pytest.mark.parametrize('resource_shape', ['sequence', 'iterator'])
def test_fastdb_derived_bridge_maps_mapping_row_batch_output_across_transports(start_c3_relay, resource_shape):
    Point, FastdbMappingRowOutput, resource, bridge = _make_fastdb_mapping_row_output_bridge_contract(
        f'{resource_shape}-mapping-row-output-bridge',
        resource_shape=resource_shape,
    )
    assert bridge
    route_name = f'fastdb-{resource_shape}-mapping-row-output-bridge'
    cc.register(
        FastdbMappingRowOutput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def assert_points(result):
        assert len(result) == 2
        assert isinstance(result[0], Point)
        assert [(point.x, point.y, point.label) for point in result] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]

    def assert_held_points(held):
        table = held.value.table('return_0')
        assert [(row.x, row.y, row.label) for row in table] == [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]
        assert table.column.x[0] == pytest.approx(1.5)
        assert table.column.label.to_pylist()[1] == 'edge'

    thread_crm = cc.connect(FastdbMappingRowOutput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert_points(thread_crm.active_points())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbMappingRowOutput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert_points(ipc_crm.active_points())
        with cc.hold(ipc_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbMappingRowOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbMappingRowOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert_points(relay_crm.active_points())
        with cc.hold(relay_crm.active_points)() as held:
            assert_held_points(held)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_derived_bridge_maps_dict_columns_to_batch_output_explicit_http_relay(start_c3_relay):
    GridId, FastdbDictColumnOutput, resource, bridge = _make_fastdb_dict_column_output_bridge_contract('dict-column-output-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbDictColumnOutput,
        resource,
        name='fastdb-dict-column-output-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbDictColumnOutput, name='fastdb-dict-column-output-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_grid_ids(crm.active())
        with cc.hold(crm.active)() as held:
            _assert_held_grid_ids(held)
    finally:
        cc.close(crm)

    assert resource.calls == 2


@pytest.mark.parametrize('resource_shape', [
    'tuple_sequence_columns',
    'tuple_iterator_columns',
    'mapping_sequence_columns',
    'mapping_iterator_columns',
    'mutable_mapping_columns',
    'typing_mutable_mapping_columns',
])
def test_fastdb_derived_bridge_maps_column_container_batch_output_across_transports(start_c3_relay, resource_shape):
    GridId, FastdbColumnContainerOutput, resource, bridge = _make_fastdb_column_container_output_bridge_contract(
        f'column-container-output-{resource_shape.replace("_", "-")}',
        resource_shape=resource_shape,
    )
    route_name = f'fastdb-column-container-output-{resource_shape.replace("_", "-")}'

    def expected_rows(call_number: int) -> list[tuple[int, int]]:
        return [
            (call_number, call_number * 10),
            (call_number + 1, call_number * 10 + 10),
        ]

    cc.register(
        FastdbColumnContainerOutput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbColumnContainerOutput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_grid_ids(thread_crm.active(), expected_rows(1))
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbColumnContainerOutput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_grid_ids(ipc_crm.active(), expected_rows(2))
        with cc.hold(ipc_crm.active)() as held:
            _assert_held_grid_ids(held, expected_rows(3))
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbColumnContainerOutput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbColumnContainerOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        _assert_grid_ids(relay_crm.active(), expected_rows(4))
        with cc.hold(relay_crm.active)() as held:
            _assert_held_grid_ids(held, expected_rows(5))
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_derived_bridge_maps_feature_input_to_dict_thread_local_and_direct_ipc():
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract('feature-input-bridge')
    cc.register(
        FastdbFeatureInput,
        resource,
        name='fastdb-feature-input-bridge',
        bridge=bridge,
    )

    point = Point(x=1.5, y=2.5, label='center')
    thread_crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        {'x': 1.5, 'y': 2.5, 'label': 'center'},
        {'x': 1.5, 'y': 2.5, 'label': 'center'},
    ]


def test_fastdb_derived_bridge_maps_feature_input_to_dict_explicit_http_relay(start_c3_relay):
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract('feature-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbFeatureInput,
        resource,
        name='fastdb-feature-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.score(Point(x=3.5, y=4.5, label='relay')) == pytest.approx(8.0)
    finally:
        cc.close(crm)

    assert resource.seen == [{'x': 3.5, 'y': 4.5, 'label': 'relay'}]


@pytest.mark.parametrize('resource_shape', ['mapping', 'mutable_mapping'])
def test_fastdb_derived_bridge_maps_feature_input_to_mapping_across_transports(start_c3_relay, resource_shape):
    namespace_shape = resource_shape.replace('_', '-')
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract(
        f'feature-{namespace_shape}-input-bridge',
        resource_shape=resource_shape,
    )
    route_name = f'fastdb-feature-{namespace_shape}-input-bridge'
    cc.register(
        FastdbFeatureInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbFeatureInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(Point(x=1.5, y=2.5, label='thread')) == pytest.approx(4.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(Point(x=3.5, y=4.5, label='ipc')) == pytest.approx(8.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    relay_name = f'{route_name}-relay'
    cc.register(FastdbFeatureInput, resource, name=relay_name, bridge=bridge)
    relay_crm = cc.connect(FastdbFeatureInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score(Point(x=5.5, y=6.5, label='relay')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        (dict, {'x': 1.5, 'y': 2.5, 'label': 'thread'}),
        (dict, {'x': 3.5, 'y': 4.5, 'label': 'ipc'}),
        (dict, {'x': 5.5, 'y': 6.5, 'label': 'relay'}),
    ]


def test_fastdb_derived_bridge_maps_feature_input_to_tuple_thread_local_and_direct_ipc():
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract(
        'feature-tuple-input-bridge',
        resource_shape='tuple',
    )
    cc.register(
        FastdbFeatureInput,
        resource,
        name='fastdb-feature-tuple-input-bridge',
        bridge=bridge,
    )

    point = Point(x=1.5, y=2.5, label='center')
    thread_crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-tuple-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-tuple-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        (1.5, 2.5, 'center'),
        (1.5, 2.5, 'center'),
    ]


def test_fastdb_derived_bridge_maps_feature_input_to_tuple_explicit_http_relay(start_c3_relay):
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract(
        'feature-tuple-input-bridge-relay',
        resource_shape='tuple',
    )
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbFeatureInput,
        resource,
        name='fastdb-feature-tuple-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbFeatureInput, name='fastdb-feature-tuple-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.score(Point(x=3.5, y=4.5, label='relay')) == pytest.approx(8.0)
    finally:
        cc.close(crm)

    assert resource.seen == [(3.5, 4.5, 'relay')]


@pytest.mark.parametrize('resource_shape', ['bare_tuple', 'typing_bare_tuple'])
def test_fastdb_derived_bridge_maps_feature_input_to_bare_tuple_across_transports(start_c3_relay, resource_shape):
    namespace_shape = resource_shape.replace('_', '-')
    Point, FastdbFeatureInput, resource, bridge = _make_fastdb_feature_input_bridge_contract(
        f'feature-{namespace_shape}-input-bridge',
        resource_shape=resource_shape,
    )
    route_name = f'fastdb-feature-{namespace_shape}-input-bridge'
    cc.register(
        FastdbFeatureInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbFeatureInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(Point(x=1.5, y=2.5, label='thread')) == pytest.approx(4.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(Point(x=3.5, y=4.5, label='ipc')) == pytest.approx(8.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    relay_name = f'{route_name}-relay'
    cc.register(FastdbFeatureInput, resource, name=relay_name, bridge=bridge)
    relay_crm = cc.connect(FastdbFeatureInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score(Point(x=5.5, y=6.5, label='relay')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        (tuple, (1.5, 2.5, 'thread')),
        (tuple, (3.5, 4.5, 'ipc')),
        (tuple, (5.5, 6.5, 'relay')),
    ]


def test_fastdb_derived_bridge_maps_feature_input_to_split_parameters_thread_local_and_direct_ipc():
    Point, FastdbSplitFeatureInput, resource, bridge = _make_fastdb_split_feature_input_bridge_contract('feature-split-input-bridge')
    cc.register(
        FastdbSplitFeatureInput,
        resource,
        name='fastdb-feature-split-input-bridge',
        bridge=bridge,
    )

    point = Point(x=1.5, y=2.5, label='center')
    thread_crm = cc.connect(FastdbSplitFeatureInput, name='fastdb-feature-split-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbSplitFeatureInput, name='fastdb-feature-split-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(point) == pytest.approx(4.0)
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        ('center', 1.5, 2.5),
        ('center', 1.5, 2.5),
    ]


def test_fastdb_derived_bridge_maps_feature_input_to_split_parameters_explicit_http_relay(start_c3_relay):
    Point, FastdbSplitFeatureInput, resource, bridge = _make_fastdb_split_feature_input_bridge_contract('feature-split-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbSplitFeatureInput,
        resource,
        name='fastdb-feature-split-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbSplitFeatureInput, name='fastdb-feature-split-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.score(Point(x=3.5, y=4.5, label='relay')) == pytest.approx(8.0)
    finally:
        cc.close(crm)

    assert resource.seen == [('relay', 3.5, 4.5)]


def test_fastdb_derived_bridge_maps_dict_output_to_single_feature_thread_local_and_direct_ipc():
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract('feature-output-bridge')
    cc.register(
        FastdbFeatureOutput,
        resource,
        name='fastdb-feature-output-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_origin_point(thread_crm.origin(), Point)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_origin_point(ipc_crm.origin(), Point)
        with cc.hold(ipc_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 3


def test_fastdb_derived_bridge_maps_dict_output_to_single_feature_explicit_http_relay(start_c3_relay):
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract('feature-output-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbFeatureOutput,
        resource,
        name='fastdb-feature-output-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_origin_point(crm.origin(), Point)
        with cc.hold(crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(crm)

    assert resource.calls == 2


def test_fastdb_derived_bridge_maps_tuple_output_to_single_feature_thread_local_and_direct_ipc():
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract(
        'feature-output-tuple-bridge',
        resource_shape='tuple',
    )
    cc.register(
        FastdbFeatureOutput,
        resource,
        name='fastdb-feature-output-tuple-bridge',
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-tuple-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_origin_point(thread_crm.origin(), Point)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-tuple-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_origin_point(ipc_crm.origin(), Point)
        with cc.hold(ipc_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(ipc_crm)

    assert resource.calls == 3


def test_fastdb_derived_bridge_maps_tuple_output_to_single_feature_explicit_http_relay(start_c3_relay):
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract(
        'feature-output-tuple-bridge-relay',
        resource_shape='tuple',
    )
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbFeatureOutput,
        resource,
        name='fastdb-feature-output-tuple-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-tuple-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_origin_point(crm.origin(), Point)
        with cc.hold(crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(crm)

    assert resource.calls == 2


def test_fastdb_derived_bridge_maps_object_output_to_single_feature_across_transports(start_c3_relay):
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract(
        'feature-output-object-bridge',
        resource_shape='object',
    )
    cc.register(
        FastdbFeatureOutput,
        resource,
        name='fastdb-feature-output-object-bridge',
        bridge=bridge,
    )

    def assert_origin(crm):
        _assert_origin_point(crm.origin(), Point)

    thread_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-object-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert_origin(thread_crm)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureOutput, name='fastdb-feature-output-object-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert_origin(ipc_crm)
        with cc.hold(ipc_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    relay_name = 'fastdb-feature-output-object-bridge-relay'
    cc.register(FastdbFeatureOutput, resource, name=relay_name, bridge=bridge)
    relay_crm = cc.connect(FastdbFeatureOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert_origin(relay_crm)
        with cc.hold(relay_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


@pytest.mark.parametrize('resource_shape', ['bare_tuple', 'typing_bare_tuple'])
def test_fastdb_derived_bridge_maps_bare_tuple_output_to_single_feature_across_transports(start_c3_relay, resource_shape):
    namespace_shape = resource_shape.replace('_', '-')
    Point, FastdbFeatureOutput, resource, bridge = _make_fastdb_feature_output_bridge_contract(
        f'feature-output-{namespace_shape}-bridge',
        resource_shape=resource_shape,
    )
    route_name = f'fastdb-feature-output-{namespace_shape}-bridge'
    cc.register(
        FastdbFeatureOutput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    thread_crm = cc.connect(FastdbFeatureOutput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        _assert_origin_point(thread_crm.origin(), Point)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureOutput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        _assert_origin_point(ipc_crm.origin(), Point)
        with cc.hold(ipc_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    relay_name = f'{route_name}-relay'
    cc.register(FastdbFeatureOutput, resource, name=relay_name, bridge=bridge)
    relay_crm = cc.connect(FastdbFeatureOutput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        _assert_origin_point(relay_crm.origin(), Point)
        with cc.hold(relay_crm.origin)() as held:
            _assert_held_origin_point(held, Point)
    finally:
        cc.close(relay_crm)

    assert resource.calls == 5


def test_fastdb_derived_bridge_maps_batch_input_to_dict_list_thread_local_and_direct_ipc():
    Point, FastdbBatchDictInput, resource, bridge = _make_fastdb_batch_dict_input_bridge_contract('batch-dict-input-bridge')
    cc.register(
        FastdbBatchDictInput,
        resource,
        name='fastdb-batch-dict-input-bridge',
        bridge=bridge,
    )

    points = [
        Point(x=1.5, y=2.5, label='center'),
        Point(x=3.5, y=4.5, label='edge'),
    ]
    thread_crm = cc.connect(FastdbBatchDictInput, name='fastdb-batch-dict-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(points) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbBatchDictInput, name='fastdb-batch-dict-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(points) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        [
            {'x': 1.5, 'y': 2.5, 'label': 'center'},
            {'x': 3.5, 'y': 4.5, 'label': 'edge'},
        ],
        [
            {'x': 1.5, 'y': 2.5, 'label': 'center'},
            {'x': 3.5, 'y': 4.5, 'label': 'edge'},
        ],
    ]


def test_fastdb_derived_bridge_maps_batch_input_to_dict_list_explicit_http_relay(start_c3_relay):
    Point, FastdbBatchDictInput, resource, bridge = _make_fastdb_batch_dict_input_bridge_contract('batch-dict-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbBatchDictInput,
        resource,
        name='fastdb-batch-dict-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbBatchDictInput, name='fastdb-batch-dict-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.score_many([
            Point(x=5.5, y=6.5, label='relay-a'),
            Point(x=7.5, y=8.5, label='relay-b'),
        ]) == pytest.approx(28.0)
    finally:
        cc.close(crm)

    assert resource.seen == [[
        {'x': 5.5, 'y': 6.5, 'label': 'relay-a'},
        {'x': 7.5, 'y': 8.5, 'label': 'relay-b'},
    ]]


def test_fastdb_derived_bridge_maps_table_like_batch_input_to_dict_list_across_transports(start_c3_relay):
    class TableLikeRows:
        def __init__(self, records):
            self._records = list(records)

        def to_pylist(self):
            return list(self._records)

    _, FastdbBatchDictInput, resource, bridge = _make_fastdb_batch_dict_input_bridge_contract('batch-dict-input-table-like-bridge')
    cc.register(
        FastdbBatchDictInput,
        resource,
        name='fastdb-batch-dict-input-table-like-bridge',
        bridge=bridge,
    )

    def rows(label_a, label_b):
        return TableLikeRows([
            {'x': 1.5, 'y': 2.5, 'label': label_a},
            {'x': 3.5, 'y': 4.5, 'label': label_b},
        ])

    thread_crm = cc.connect(FastdbBatchDictInput, name='fastdb-batch-dict-input-table-like-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(rows('thread-a', 'thread-b')) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbBatchDictInput, name='fastdb-batch-dict-input-table-like-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(rows('ipc-a', 'ipc-b')) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = 'fastdb-batch-dict-input-table-like-bridge-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbBatchDictInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbBatchDictInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score_many(rows('relay-a', 'relay-b')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        [
            {'x': 1.5, 'y': 2.5, 'label': 'thread-a'},
            {'x': 3.5, 'y': 4.5, 'label': 'thread-b'},
        ],
        [
            {'x': 1.5, 'y': 2.5, 'label': 'ipc-a'},
            {'x': 3.5, 'y': 4.5, 'label': 'ipc-b'},
        ],
        [
            {'x': 1.5, 'y': 2.5, 'label': 'relay-a'},
            {'x': 3.5, 'y': 4.5, 'label': 'relay-b'},
        ],
    ]


@pytest.mark.parametrize(
    ('resource_shape', 'expected_marker'),
    [
        ('sequence_mapping', list),
        ('iterable_mapping', list),
        ('iterator_mapping', True),
        ('mutable_sequence_mapping', list),
    ],
)
def test_fastdb_derived_bridge_maps_batch_input_to_mapping_containers_across_transports(
    start_c3_relay,
    resource_shape,
    expected_marker,
):
    namespace = f'batch-input-{resource_shape.replace("_", "-")}'
    route_name = f'fastdb-batch-input-{resource_shape.replace("_", "-")}'
    Point, FastdbBatchDictInput, resource, bridge = _make_fastdb_batch_dict_input_bridge_contract(
        namespace,
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbBatchDictInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(label_a, label_b):
        return [
            Point(x=1.5, y=2.5, label=label_a),
            Point(x=3.5, y=4.5, label=label_b),
        ]

    def expected_seen(label_a, label_b):
        return (
            expected_marker,
            [
                {'x': 1.5, 'y': 2.5, 'label': label_a},
                {'x': 3.5, 'y': 4.5, 'label': label_b},
            ],
        )

    thread_crm = cc.connect(FastdbBatchDictInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(rows('thread-a', 'thread-b')) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbBatchDictInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(rows('ipc-a', 'ipc-b')) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbBatchDictInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbBatchDictInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score_many(rows('relay-a', 'relay-b')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        expected_seen('thread-a', 'thread-b'),
        expected_seen('ipc-a', 'ipc-b'),
        expected_seen('relay-a', 'relay-b'),
    ]


@pytest.mark.parametrize(
    ('resource_shape', 'client_shape'),
    [
        ('domain_object', 'feature'),
        ('tuple_row', 'feature'),
        ('mapping', 'feature'),
        ('feature', 'feature'),
        ('feature', 'mapping'),
    ],
)
def test_fastdb_derived_bridge_maps_batch_input_to_variadic_tuple_shapes_across_transports(
    start_c3_relay,
    resource_shape,
    client_shape,
):
    namespace = f'batch-input-variadic-{resource_shape}-{client_shape}'
    route_name = f'fastdb-batch-input-variadic-{resource_shape}-{client_shape}'
    Point, FastdbVariadicTupleBatchInput, resource, bridge = _make_fastdb_variadic_tuple_batch_input_bridge_contract(
        namespace,
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbVariadicTupleBatchInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(label_a, label_b):
        if client_shape == 'mapping':
            return [
                {'x': '1.5', 'y': '2.5', 'label': label_a},
                {'x': '3.5', 'y': '4.5', 'label': label_b},
            ]
        return [
            Point(x=1.5, y=2.5, label=label_a),
            Point(x=3.5, y=4.5, label=label_b),
        ]

    def expected_seen(label_a, label_b):
        if resource_shape == 'tuple_row':
            payload = [(1.5, 2.5, label_a), (3.5, 4.5, label_b)]
        elif resource_shape == 'mapping':
            payload = [
                {'x': 1.5, 'y': 2.5, 'label': label_a},
                {'x': 3.5, 'y': 4.5, 'label': label_b},
            ]
        elif resource_shape == 'feature':
            payload = [
                ('RpcPoint', 1.5, 2.5, label_a),
                ('RpcPoint', 3.5, 4.5, label_b),
            ]
        else:
            payload = [
                ('DomainPoint', 1.5, 2.5, label_a),
                ('DomainPoint', 3.5, 4.5, label_b),
            ]
        return (tuple, payload)

    thread_crm = cc.connect(FastdbVariadicTupleBatchInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(rows('thread-a', 'thread-b')) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbVariadicTupleBatchInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(rows('ipc-a', 'ipc-b')) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbVariadicTupleBatchInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbVariadicTupleBatchInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score_many(rows('relay-a', 'relay-b')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        expected_seen('thread-a', 'thread-b'),
        expected_seen('ipc-a', 'ipc-b'),
        expected_seen('relay-a', 'relay-b'),
    ]


@pytest.mark.parametrize('resource_shape', [
    'list_tuple_row',
    'sequence_tuple_row',
    'iterator_tuple_row',
    'typing_tuple_row',
])
def test_fastdb_derived_bridge_maps_batch_input_to_tuple_row_containers_across_transports(start_c3_relay, resource_shape):
    namespace = f'batch-input-tuple-row-{resource_shape.replace("_", "-")}'
    route_name = f'fastdb-batch-input-tuple-row-{resource_shape.replace("_", "-")}'
    GridId, FastdbTupleRowBatchInput, resource, bridge = _make_fastdb_tuple_row_batch_input_bridge_contract(
        namespace,
        resource_shape=resource_shape,
    )
    cc.register(
        FastdbTupleRowBatchInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(level_a: int, id_a: int, level_b: int, id_b: int):
        return [
            GridId(level=level_a, global_id=id_a),
            GridId(level=level_b, global_id=id_b),
        ]

    thread_crm = cc.connect(FastdbTupleRowBatchInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.summarize(rows(1, 10, 2, 20)) == 33
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbTupleRowBatchInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.summarize(rows(3, 30, 4, 40)) == 77
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbTupleRowBatchInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbTupleRowBatchInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.summarize(rows(5, 50, 6, 60)) == 121
    finally:
        cc.close(relay_crm)

    def expected_entry(level_a: int, id_a: int, level_b: int, id_b: int):
        return (resource_shape == 'iterator_tuple_row', [(level_a, id_a), (level_b, id_b)])

    assert resource.seen == [
        expected_entry(1, 10, 2, 20),
        expected_entry(3, 30, 4, 40),
        expected_entry(5, 50, 6, 60),
    ]


@pytest.mark.parametrize('output_shape', ['domain_object', 'mapping', 'feature'])
def test_fastdb_derived_bridge_maps_variadic_tuple_batch_rows_across_transports(start_c3_relay, output_shape):
    namespace = f'variadic-tuple-batch-bridge-{output_shape.replace("_", "-")}'
    route_name = f'fastdb-variadic-tuple-batch-bridge-{output_shape.replace("_", "-")}'
    Point, FastdbVariadicTupleBatch, resource, bridge = _make_fastdb_variadic_tuple_batch_bridge_contract(
        namespace,
        output_shape=output_shape,
    )
    cc.register(
        FastdbVariadicTupleBatch,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def points(label_a, label_b):
        return [
            Point(x=1.5, y=2.5, label=label_a),
            Point(x=3.5, y=4.5, label=label_b),
        ]

    if output_shape == 'domain_object':
        expected_active = [
            (1.5, 2.5, 'a'),
            (3.5, 4.5, 'b'),
        ]
    else:
        expected_active = [
            (1.5, 2.5, '123'),
            (3.5, 4.5, 'edge'),
        ]

    def assert_active(result):
        assert [(point.x, point.y, point.label) for point in result] == expected_active

    def assert_held_active(held):
        table = held.value.table('return_0')
        assert [(row.x, row.y, row.label) for row in table] == expected_active
        assert table.column.x[0] == pytest.approx(1.5)
        assert table.column.label.to_pylist()[1] == expected_active[1][2]

    thread_crm = cc.connect(FastdbVariadicTupleBatch, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(points('thread-a', 'thread-b')) == pytest.approx(12.0)
        assert_active(thread_crm.active_points())
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbVariadicTupleBatch, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(points('ipc-a', 'ipc-b')) == pytest.approx(12.0)
        assert_active(ipc_crm.active_points())
        with cc.hold(ipc_crm.active_points)() as held:
            assert_held_active(held)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbVariadicTupleBatch,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbVariadicTupleBatch, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score_many(points('relay-a', 'relay-b')) == pytest.approx(12.0)
        assert_active(relay_crm.active_points())
        with cc.hold(relay_crm.active_points)() as held:
            assert_held_active(held)
    finally:
        cc.close(relay_crm)

    assert resource.seen == [
        (tuple, [(1.5, 2.5, 'thread-a'), (3.5, 4.5, 'thread-b')]),
        (tuple, [(1.5, 2.5, 'ipc-a'), (3.5, 4.5, 'ipc-b')]),
        (tuple, [(1.5, 2.5, 'relay-a'), (3.5, 4.5, 'relay-b')]),
    ]


def test_fastdb_derived_bridge_maps_feature_and_batch_inputs_to_domain_objects_thread_local_and_direct_ipc():
    Point, FastdbObjectInput, resource, bridge = _make_fastdb_object_input_bridge_contract('object-input-bridge')
    cc.register(
        FastdbObjectInput,
        resource,
        name='fastdb-object-input-bridge',
        bridge=bridge,
    )

    point = Point(x=1.5, y=2.5, label='center')
    points = [
        Point(x=1.5, y=2.5, label='center'),
        Point(x=3.5, y=4.5, label='edge'),
    ]
    thread_crm = cc.connect(FastdbObjectInput, name='fastdb-object-input-bridge')
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score(point) == pytest.approx(4.0)
        assert thread_crm.score_many(points) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbObjectInput, name='fastdb-object-input-bridge', address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score(point) == pytest.approx(4.0)
        assert ipc_crm.score_many(points) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    assert resource.seen == [
        ('one', 'DomainPoint', 1.5, 2.5, 'center'),
        ('many', [('DomainPoint', 1.5, 2.5, 'center'), ('DomainPoint', 3.5, 4.5, 'edge')]),
        ('one', 'DomainPoint', 1.5, 2.5, 'center'),
        ('many', [('DomainPoint', 1.5, 2.5, 'center'), ('DomainPoint', 3.5, 4.5, 'edge')]),
    ]


@pytest.mark.parametrize('input_shape', ['feature_rows', 'mapping_rows', 'table_like_rows'])
def test_fastdb_derived_bridge_maps_batch_input_to_feature_list_resource_across_transports(start_c3_relay, input_shape):
    class TableLikeRows:
        def __init__(self, records):
            self._records = list(records)

        def to_pylist(self):
            return list(self._records)

    namespace = f'feature-list-input-{input_shape.replace("_", "-")}'
    route_name = f'fastdb-feature-list-input-{input_shape.replace("_", "-")}'
    Point, FastdbFeatureListInput, resource, bridge = _make_fastdb_feature_list_input_bridge_contract(namespace)
    cc.register(
        FastdbFeatureListInput,
        resource,
        name=route_name,
        bridge=bridge,
    )

    def rows(label_a: str, label_b: str):
        records = [
            {'x': '1.5', 'y': '2.5', 'label': label_a},
            {'x': '3.5', 'y': '4.5', 'label': label_b},
        ]
        if input_shape == 'feature_rows':
            return [
                Point(x=1.5, y=2.5, label=label_a),
                Point(x=3.5, y=4.5, label=label_b),
            ]
        if input_shape == 'mapping_rows':
            return records
        if input_shape == 'table_like_rows':
            return TableLikeRows(records)
        raise AssertionError(f'unhandled input shape: {input_shape}')

    thread_crm = cc.connect(FastdbFeatureListInput, name=route_name)
    try:
        assert thread_crm.client._mode == 'thread'  # noqa: SLF001
        assert thread_crm.score_many(rows('thread-a', 'thread-b')) == pytest.approx(12.0)
    finally:
        cc.close(thread_crm)

    address = cc.server_address()
    assert address is not None
    ipc_crm = cc.connect(FastdbFeatureListInput, name=route_name, address=address)
    try:
        assert ipc_crm.client._mode == 'ipc'  # noqa: SLF001
        assert ipc_crm.score_many(rows('ipc-a', 'ipc-b')) == pytest.approx(12.0)
    finally:
        cc.close(ipc_crm)

    relay = start_c3_relay()
    relay_name = f'{route_name}-relay'
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbFeatureListInput,
        resource,
        name=relay_name,
        bridge=bridge,
    )
    relay_crm = cc.connect(FastdbFeatureListInput, name=relay_name, address=relay.url)
    try:
        assert relay_crm.client._mode == 'http'  # noqa: SLF001
        assert relay_crm.score_many(rows('relay-a', 'relay-b')) == pytest.approx(12.0)
    finally:
        cc.close(relay_crm)

    def expected_seen(label_a: str, label_b: str):
        return [
            ('RpcPoint', 1.5, 2.5, label_a),
            ('RpcPoint', 3.5, 4.5, label_b),
        ]

    assert resource.seen == [
        expected_seen('thread-a', 'thread-b'),
        expected_seen('ipc-a', 'ipc-b'),
        expected_seen('relay-a', 'relay-b'),
    ]
    assert Point.__name__ == 'RpcPoint'


def test_fastdb_derived_bridge_maps_feature_and_batch_inputs_to_domain_objects_explicit_http_relay(start_c3_relay):
    Point, FastdbObjectInput, resource, bridge = _make_fastdb_object_input_bridge_contract('object-input-bridge-relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(
        FastdbObjectInput,
        resource,
        name='fastdb-object-input-bridge-relay',
        bridge=bridge,
    )

    crm = cc.connect(FastdbObjectInput, name='fastdb-object-input-bridge-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        assert crm.score(Point(x=3.5, y=4.5, label='relay')) == pytest.approx(8.0)
        assert crm.score_many([
            Point(x=5.5, y=6.5, label='relay-a'),
            Point(x=7.5, y=8.5, label='relay-b'),
        ]) == pytest.approx(28.0)
    finally:
        cc.close(crm)

    assert resource.seen == [
        ('one', 'DomainPoint', 3.5, 4.5, 'relay'),
        ('many', [('DomainPoint', 5.5, 6.5, 'relay-a'), ('DomainPoint', 7.5, 8.5, 'relay-b')]),
    ]


def test_fastdb_crm_payloads_work_explicit_http_relay(start_c3_relay):
    Point, FastdbPoint, FastdbPointResource = _make_fastdb_contract('relay')
    relay = start_c3_relay()
    cc.set_relay_anchor(relay.url)
    cc.register(FastdbPoint, FastdbPointResource(), name='fastdb-point-relay')

    crm = cc.connect(FastdbPoint, name='fastdb-point-relay', address=relay.url)
    try:
        assert crm.client._mode == 'http'  # noqa: SLF001
        _assert_fastdb_echo(crm, Point)
        _assert_fastdb_held_echo(crm, Point)
    finally:
        cc.close(crm)


def test_fastdb_crm_does_not_claim_python_builtin_scalar_methods():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')

    @cc.crm(namespace='test.fastdb.scalar-policy', version='0.1.0')
    class Counter:
        def increment(self, amount: int) -> int:
            ...

    descriptor = build_contract_descriptor(Counter)
    wire = descriptor['methods'][0]['wire']

    assert wire['input']['family'] == 'python-pickle-default'
    assert wire['output']['family'] == 'python-pickle-default'
    assert wire['input'].get('id') != 'org.fastdb.call-db'
    assert wire['output'].get('id') != 'org.fastdb.call-db'

    diagnostics = cc.contract_descriptor_diagnostics(Counter)
    fastdb_items = [
        item for item in diagnostics
        if item['code'] == 'fastdb_call_db_not_planned'
    ]
    assert [
        (item['method'], item['position'])
        for item in fastdb_items
    ] == [('increment', 'input'), ('increment', 'output')]
    assert all('Python builtin scalar int' in item['reason'] for item in fastdb_items)


def test_fastdb_crm_does_not_claim_python_nullable_builtin_methods():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')

    @cc.crm(namespace='test.fastdb.nullable-policy', version='0.1.0')
    class Counter:
        def maybe_increment(self, amount: int | None) -> int | None:
            ...

    descriptor = build_contract_descriptor(Counter)
    wire = descriptor['methods'][0]['wire']

    assert wire['input']['family'] == 'python-pickle-default'
    assert wire['output']['family'] == 'python-pickle-default'
    assert wire['input'].get('id') != 'org.fastdb.call-db'
    assert wire['output'].get('id') != 'org.fastdb.call-db'


def test_fastdb_crm_diagnoses_list_feature_crm_spelling():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    from fastdb4py import F64, feature

    class Point:
        pass

    Point.__annotations__ = {'x': F64}
    Point = feature(Point)

    class Grid:
        def query(self, points):
            ...

    Grid.query.__annotations__ = {'points': list[Point], 'return': None}
    Grid = cc.crm(namespace='test.fastdb.list-feature-policy', version='0.1.0')(Grid)

    diagnostics = cc.contract_descriptor_diagnostics(Grid)
    fastdb_items = [
        item for item in diagnostics
        if item['code'] == 'fastdb_call_db_not_planned'
    ]

    assert len(fastdb_items) == 1
    assert fastdb_items[0]['position'] == 'input'
    assert 'list[Feature]' in fastdb_items[0]['reason']
    assert 'Batch[Feature]' in fastdb_items[0]['reason']


def test_fastdb_crm_diagnoses_nested_list_feature_fields_at_c_two_entrypoint():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb

    class NestedValues:
        pass

    NestedValues.__annotations__ = {'values': list[list[fdb.I32]]}
    NestedValues = fdb.feature(NestedValues)

    class Grid:
        def active(self):
            ...

    Grid.active.__annotations__ = {'return': fdb.Batch[NestedValues]}
    Grid = cc.crm(namespace='test.fastdb.nested-list-policy', version='0.1.0')(Grid)

    diagnostics = cc.contract_descriptor_diagnostics(Grid)
    fastdb_items = [
        item for item in diagnostics
        if item['code'] == 'fastdb_call_db_not_planned'
    ]

    assert len(fastdb_items) == 1
    assert fastdb_items[0]['method'] == 'active'
    assert fastdb_items[0]['position'] == 'output'
    assert 'values: list[list]' in fastdb_items[0]['reason']
    assert 'native fixed-width list storage' in fastdb_items[0]['reason']


def test_fastdb_crm_diagnoses_single_item_tuple_return_at_c_two_entrypoint():
    pytest.importorskip('fastdb4py', reason='fastdb C-Two smoke requires fastdb4py')
    import fastdb4py as fdb

    class Grid:
        def active_ids(self):
            ...

    Grid.active_ids.__annotations__ = {'return': tuple[fdb.Array[fdb.I32]]}
    Grid = cc.crm(namespace='test.fastdb.single-tuple-policy', version='0.1.0')(Grid)

    with pytest.raises(TypeError, match='unsupported annotation'):
        build_contract_descriptor(Grid)

    diagnostics = cc.contract_descriptor_diagnostics(Grid)
    fastdb_items = [
        item for item in diagnostics
        if item['code'] == 'fastdb_call_db_not_planned'
    ]

    assert len(fastdb_items) == 1
    assert fastdb_items[0]['method'] == 'active_ids'
    assert fastdb_items[0]['position'] == 'output'
    assert 'single-item tuple return' in fastdb_items[0]['reason']
    assert 'use the item annotation directly' in fastdb_items[0]['reason']
