from __future__ import annotations

import json
import re
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from types import UnionType
from typing import Any, Union, get_args, get_origin, get_type_hints

from fastdb4py import FdbViewOwner, core, materialize as _fastdb_materialize
from fastdb4py.column_engine import ColumnEngine, _get_default_table_build
from fastdb4py.decorator import feature
from fastdb4py.object_engine import LayerState, ObjectEngine
from fastdb4py.orm.table import Table
from fastdb4py.registry import (
    get_schema,
    is_feature,
    lookup_class,
    non_native_list_storage_diagnostics,
    raw_payload_storage_diagnostics,
)
from fastdb4py.schema import (
    columnar_capability,
    export_schema,
    feature_schema_dependencies,
    object_graph_capability,
    schema_sha256,
)
from fastdb4py.type import (
    Array,
    BYTES,
    Batch,
    BOOL,
    F64,
    I32,
    STR,
    WSTR,
    OriginFieldType,
    coerce_bool_scalar,
    get_origin_type,
)

CALL_DB_SCHEMA_VERSION = 'fastdb.call-db.schema.v1'
CALL_DB_CODEC_ID = 'org.fastdb.call-db'
CALL_DB_CODEC_VERSION = '1'
CALL_DB_COLUMNAR_PROFILE = 'fastdb.call.columnar.v1'
CALL_DB_OBJECT_GRAPH_PROFILE = 'fastdb.call.object-graph.v1'
_VALID_CALL_DB_PROFILES = {
    CALL_DB_COLUMNAR_PROFILE,
    CALL_DB_OBJECT_GRAPH_PROFILE,
}
_CRM_CONTEXT_KEYS = (
    'crm_namespace',
    'crm_name',
    'crm_version',
)


def _to_owned_fastdb_value(value: object) -> object:
    return _fastdb_materialize(value)


def _new_retained_owner() -> FdbViewOwner:
    return FdbViewOwner(checked=True, writeable=False)

_SCALAR_TABLE_NAME = '__c2_args'
_NONE_TYPE = type(None)
_SCALAR_KIND_BY_FIELD_TYPE = {
    OriginFieldType.u8: 'u8',
    OriginFieldType.u16: 'u16',
    OriginFieldType.u32: 'u32',
    OriginFieldType.i32: 'i32',
    OriginFieldType.u8n: 'u8n',
    OriginFieldType.u16n: 'u16n',
    OriginFieldType.f32: 'f32',
    OriginFieldType.f64: 'f64',
    OriginFieldType.str: 'str',
}
_FIELD_SCALAR_KIND_BY_FIELD_TYPE = {
    **_SCALAR_KIND_BY_FIELD_TYPE,
    OriginFieldType.wstr: 'wstr',
    OriginFieldType.bytes: 'bytes',
}
_VALID_CALL_DB_SCALAR_KINDS = {
    'bool',
    'bytes',
    'wstr',
    *_SCALAR_KIND_BY_FIELD_TYPE.values(),
}
_PYTHON_BUILTIN_SCALARS = {bool, int, float, str}


@dataclass(frozen=True)
class FastdbCallTableSpec:
    name: str
    kind: str
    cardinality: str
    feature_type: type | None = None
    feature_schema: dict[str, Any] | None = None
    feature_schema_dependencies: tuple[dict[str, Any], ...] = ()
    parameter: str | None = None
    return_index: int | None = None
    value_position: int | None = None
    scalar_fields: tuple[dict[str, str], ...] = ()
    scalar_positions: tuple[int, ...] = ()
    array_item: dict[str, str] | None = None

    def descriptor(self) -> dict[str, Any]:
        _validate_table_shape(self)
        if self.kind == 'scalars':
            fields = [dict(field) for field in self.scalar_fields]
            for field, position in zip(fields, self.scalar_positions):
                field['value_position'] = position
            return {
                'cardinality': self.cardinality,
                'fields': fields,
                'kind': 'scalars',
                'name': self.name,
            }
        if self.kind == 'array':
            if self.array_item is None:
                raise ValueError(f'array table {self.name!r} is missing item schema.')
            payload = {
                'cardinality': self.cardinality,
                'item': dict(self.array_item),
                'kind': 'array',
                'name': self.name,
            }
            if self.parameter is not None:
                payload['parameter'] = self.parameter
            if self.return_index is not None:
                payload['return_index'] = self.return_index
            if self.value_position is not None:
                payload['value_position'] = self.value_position
            return payload
        if self.feature_schema is None:
            raise ValueError(f'feature table {self.name!r} is missing feature schema.')
        payload: dict[str, Any] = {
            'cardinality': self.cardinality,
            'feature': self.feature_schema['feature'],
            'feature_schema_sha256': schema_sha256(self.feature_schema),
            'kind': 'feature',
            'name': self.name,
        }
        if self.feature_schema_dependencies:
            payload['feature_schema_dependencies'] = [
                {
                    'feature': dependency['feature'],
                    'feature_schema_sha256': schema_sha256(dependency),
                }
                for dependency in self.feature_schema_dependencies
            ]
        if self.parameter is not None:
            payload['parameter'] = self.parameter
        if self.return_index is not None:
            payload['return_index'] = self.return_index
        if self.value_position is not None:
            payload['value_position'] = self.value_position
        return payload


@dataclass(frozen=True)
class FastdbCallPlan:
    method_name: str
    direction: str
    profile: str
    tables: tuple[FastdbCallTableSpec, ...]
    crm_context: dict[str, str]
    scalar_feature_type: type | None = None

    @property
    def schema_descriptor(self) -> dict[str, Any]:
        self._validate_identity()
        _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        payload: dict[str, Any] = {
            'direction': self.direction,
            'method': self.method_name,
            'profile': self.profile,
            'schema': CALL_DB_SCHEMA_VERSION,
            'tables': [table.descriptor() for table in self.tables],
        }
        crm = _crm_descriptor(self.crm_context)
        if crm is not None:
            payload['crm'] = crm
        return payload

    @property
    def schema_text(self) -> str:
        return json.dumps(self.schema_descriptor, sort_keys=True, separators=(',', ':'))

    @property
    def schema_sha256(self) -> str:
        return schema_sha256(self.schema_descriptor)

    @property
    def supports_buffer_view(self) -> bool:
        self._validate_identity()
        _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        return self.profile == CALL_DB_COLUMNAR_PROFILE

    @property
    def payload_abi_ref(self) -> dict[str, Any]:
        return {
            'capabilities': (
                ['bytes', 'buffer-view']
                if self.supports_buffer_view
                else ['bytes']
            ),
            'id': CALL_DB_CODEC_ID,
            'kind': 'codec_ref',
            'portable': True,
            'schema': CALL_DB_SCHEMA_VERSION,
            'schema_sha256': self.schema_sha256,
            'version': CALL_DB_CODEC_VERSION,
        }

    def _validate_identity(self) -> None:
        if not isinstance(self.method_name, str) or not self.method_name:
            raise ValueError('fastdb call-db plan must include a non-empty method name.')
        if self.direction not in {'input', 'output'}:
            raise ValueError('fastdb call-db plan direction must be "input" or "output".')
        if self.profile not in _VALID_CALL_DB_PROFILES:
            raise ValueError(f'fastdb call-db plan has unsupported call-db profile {self.profile!r}.')
        _validate_crm_context(self.crm_context)

    def serialize_values(self, values: object) -> bytes:
        self._validate_identity()
        if self.profile == CALL_DB_OBJECT_GRAPH_PROFILE:
            return self._serialize_values_object_graph(values)
        normalized = self._normalize_values(values)
        engine = ColumnEngine.create()
        for table in self.tables:
            if table.kind == 'scalars':
                if self.scalar_feature_type is None:
                    raise ValueError('scalar table is present but scalar feature type is missing.')
                scalar_values = {
                    field['name']: _coerce_scalar_value(
                        field['kind'],
                        normalized[table.scalar_positions[field_offset]],
                    )
                    for field_offset, field in enumerate(table.scalar_fields)
                }
                engine.push(self.scalar_feature_type(**scalar_values), table_name=table.name)
                continue
            if table.kind == 'array':
                if table.feature_type is None:
                    raise ValueError(f'array table {table.name!r} is missing runtime feature type.')
                if table.value_position is None:
                    raise ValueError(f'array table {table.name!r} is missing value position.')
                items = _array_table_values(table, normalized[table.value_position])
                if items:
                    engine.push_many([
                        table.feature_type(value=item)
                        for item in items
                    ], table_name=table.name)
                else:
                    _create_empty_feature_table(engine, table)
                continue
            if table.value_position is None:
                raise ValueError(f'feature table {table.name!r} is missing value position.')
            value = normalized[table.value_position]
            if table.cardinality == 'many':
                rows = _feature_table_rows(table, value)
                if rows:
                    engine.push_many(rows, table_name=table.name)
                else:
                    _create_empty_feature_table(engine, table)
                continue
            engine.push(value, table_name=table.name)
        engine.combine()
        chunk = engine._origin.buffer()  # noqa: SLF001
        return chunk.to_bytes()

    def deserialize_values(self, data: bytes | bytearray | memoryview) -> object:
        self._validate_identity()
        _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        if self.profile == CALL_DB_OBJECT_GRAPH_PROFILE:
            engine = _object_engine_from_buffer(data, self.tables)
            return self._materialize_values_object_graph(engine)
        engine = _column_engine_from_buffer(data)
        return self._materialize_values(engine)

    def view_from_buffer(self, data: memoryview) -> object:
        self._validate_identity()
        if not self.supports_buffer_view:
            raise ValueError(f'{self.profile} does not support retained buffer views.')
        _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        view = FastdbCallView(
            plan=self,
            engine=_column_engine_from_buffer(data),
            buffer=data,
            owner=_new_retained_owner(),
        )
        logical_values = view.logical_values()
        if self.direction == 'input':
            return logical_values
        if len(logical_values) == 1:
            return logical_values[0]
        return logical_values

    def _materialize_values(self, engine: ColumnEngine) -> object:
        self._validate_identity()
        values: list[Any] = [None] * _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        for table in self.tables:
            if table.kind == 'scalars':
                if self.scalar_feature_type is None:
                    raise ValueError('scalar table is present but scalar feature type is missing.')
                row = engine.table(self.scalar_feature_type, name=table.name)[0]
                for field_offset, field in enumerate(table.scalar_fields):
                    values[table.scalar_positions[field_offset]] = _materialize_scalar_value(
                        field['kind'],
                        getattr(row, field['name']),
                    )
                continue
            if table.kind == 'array':
                if table.feature_type is None:
                    raise ValueError(f'array table {table.name!r} is missing feature type.')
                if table.value_position is None:
                    raise ValueError(f'array table {table.name!r} is missing value position.')
                fastdb_table = engine.table(table.feature_type, name=table.name)
                item_kind = _array_item_kind(table)
                values[table.value_position] = [
                    _materialize_scalar_value(
                        item_kind,
                        getattr(row, _ARRAY_VALUE_FIELD),
                    )
                    for row in fastdb_table
                ]
                continue
            if table.feature_type is None:
                raise ValueError(f'feature table {table.name!r} is missing feature type.')
            if table.value_position is None:
                raise ValueError(f'feature table {table.name!r} is missing value position.')
            fastdb_table = engine.table(table.feature_type, name=table.name)
            if table.cardinality == 'many':
                values[table.value_position] = _to_owned_fastdb_value(fastdb_table)
            else:
                values[table.value_position] = _to_owned_fastdb_value(fastdb_table[0])

        if self.direction == 'input':
            return tuple(values)
        if len(values) == 1:
            return values[0]
        return tuple(values)

    def _serialize_values_object_graph(self, values: object) -> bytes:
        self._validate_identity()
        normalized = self._normalize_values(values)
        engine = ObjectEngine.create()
        for table in self.tables:
            if table.kind == 'scalars':
                if self.scalar_feature_type is None:
                    raise ValueError('scalar table is present but scalar feature type is missing.')
                scalar_values = {
                    field['name']: _coerce_scalar_value(
                        field['kind'],
                        normalized[table.scalar_positions[field_offset]],
                    )
                    for field_offset, field in enumerate(table.scalar_fields)
                }
                engine.push(self.scalar_feature_type(**scalar_values))
                continue
            if table.value_position is None:
                raise ValueError(f'table {table.name!r} is missing value position.')
            value = normalized[table.value_position]
            if table.kind == 'array':
                if table.feature_type is None:
                    raise ValueError(f'array table {table.name!r} is missing runtime feature type.')
                items = _array_table_values(table, value)
                if not items:
                    engine._ensure_layer(table.feature_type)  # noqa: SLF001
                    continue
                for item in items:
                    engine.push(table.feature_type(value=item))
                continue
            if table.feature_type is None:
                raise ValueError(f'feature table {table.name!r} is missing feature type.')
            if table.cardinality == 'many':
                rows = _feature_table_rows(table, value)
                if not rows:
                    engine._ensure_layer(table.feature_type)  # noqa: SLF001
                    continue
                for item in rows:
                    engine.push(item)
                continue
            engine.push(value)
        engine.combine()
        return bytes(engine._buffer)  # noqa: SLF001

    def _materialize_values_object_graph(self, engine: ObjectEngine) -> object:
        self._validate_identity()
        values: list[Any] = [None] * _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        seen: dict[tuple[type, int], object] = {}
        for table in self.tables:
            if table.kind == 'scalars':
                if self.scalar_feature_type is None:
                    raise ValueError('scalar table is present but scalar feature type is missing.')
                row = engine.get(self.scalar_feature_type, 0, mode='copy')
                for field_offset, field in enumerate(table.scalar_fields):
                    values[table.scalar_positions[field_offset]] = _materialize_scalar_value(
                        field['kind'],
                        getattr(row, field['name']),
                    )
                continue
            if table.feature_type is None:
                raise ValueError(f'table {table.name!r} is missing feature type.')
            if table.value_position is None:
                raise ValueError(f'table {table.name!r} is missing value position.')
            if table.kind == 'array':
                item_kind = _array_item_kind(table)
                values[table.value_position] = [
                    _materialize_scalar_value(
                        item_kind,
                        getattr(row, _ARRAY_VALUE_FIELD),
                    )
                    for row in engine.iter(table.feature_type, mode='copy')
                ]
                continue
            if table.cardinality == 'many':
                values[table.value_position] = [
                    _copy_object_graph_feature(engine, table.feature_type, index, seen)
                    for index in range(engine.count(table.feature_type))
                ]
            else:
                values[table.value_position] = _copy_object_graph_feature(
                    engine,
                    table.feature_type,
                    0,
                    seen,
                )

        if self.direction == 'input':
            return tuple(values)
        if len(values) == 1:
            return values[0]
        return tuple(values)

    def _normalize_values(self, values: object) -> tuple[Any, ...]:
        self._validate_identity()
        if self.direction == 'input':
            if not isinstance(values, tuple):
                raise TypeError('input call-db serialization expects a tuple of parameter values.')
            expected = _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
            if len(values) != expected:
                raise ValueError(f'expected {expected} input values, got {len(values)}.')
            return values
        expected = _value_count(self.tables, scalar_feature_type=self.scalar_feature_type)
        if expected == 1:
            return (values,)
        if not isinstance(values, tuple):
            raise TypeError('multi-value output call-db serialization expects a tuple.')
        if len(values) != expected:
            raise ValueError(f'expected {expected} output values, got {len(values)}.')
        return values


@dataclass(frozen=True)
class FastdbCallView:
    plan: FastdbCallPlan
    engine: ColumnEngine
    buffer: memoryview
    owner: FdbViewOwner

    @property
    def _fdb_owner(self) -> FdbViewOwner:
        return self.owner

    def materialize(self) -> object:
        self._ensure_alive()
        return self.plan._materialize_values(self.engine)

    def to_owned(self) -> object:
        return self.materialize()

    def logical_values(self) -> tuple[object, ...]:
        self._ensure_alive()
        values: list[Any] = [None] * _value_count(
            self.plan.tables,
            scalar_feature_type=self.plan.scalar_feature_type,
        )
        for table in self.plan.tables:
            if table.kind == 'scalars':
                for field_offset, field in enumerate(table.scalar_fields):
                    values[table.scalar_positions[field_offset]] = self.scalar(field['name'])
                continue
            if table.value_position is None:
                raise ValueError(f'table {table.name!r} is missing value position.')
            if table.kind == 'feature':
                if table.cardinality == 'many':
                    values[table.value_position] = self.table(table.name)
                else:
                    values[table.value_position] = self.feature(table.name)
                continue
            if table.kind == 'array':
                values[table.value_position] = self.array(table.name)
                continue
            raise ValueError(f'unsupported call-db table kind {table.kind!r}.')
        return tuple(values)

    def table(self, name_or_index: str | int) -> 'FastdbCallTableView':
        self._ensure_alive()
        table = self._feature_table(name_or_index)
        return FastdbCallTableView(self, table)

    def feature(self, name_or_index: str | int) -> Any:
        self._ensure_alive()
        table_view = self.table(name_or_index)
        if table_view.spec.cardinality != 'one':
            raise TypeError(f'feature view {table_view.spec.name!r} has cardinality {table_view.spec.cardinality!r}; use table(...) for batch outputs.')
        if len(table_view) < 1:
            raise IndexError(f'feature table {table_view.spec.name!r} is empty.')
        return table_view[0]

    def array(self, name_or_index: str | int) -> 'FastdbCallArrayView':
        self._ensure_alive()
        table = self._array_table(name_or_index)
        return FastdbCallArrayView(self, table)

    def scalar(self, name_or_index: str | int) -> object:
        self._ensure_alive()
        table, field = self._scalar_field(name_or_index)
        if table.feature_type is None:
            raise ValueError(f'scalar table {table.name!r} is missing feature type.')
        rows = self.engine.table(
            table.feature_type,
            name=table.name,
            owner=self.owner,
            writeable=False,
        )
        if len(rows) < 1:
            raise IndexError(f'scalar table {table.name!r} is empty.')
        return _materialize_scalar_value(field['kind'], getattr(rows[0], field['name']))

    def _feature_table(self, name_or_index: str | int) -> FastdbCallTableSpec:
        feature_tables = [table for table in self.plan.tables if table.kind == 'feature']
        if isinstance(name_or_index, int):
            return feature_tables[name_or_index]
        for table in feature_tables:
            if table.name == name_or_index:
                return table
        raise KeyError(f'feature table {name_or_index!r} not found')

    def _array_table(self, name_or_index: str | int) -> FastdbCallTableSpec:
        array_tables = [table for table in self.plan.tables if table.kind == 'array']
        if isinstance(name_or_index, int):
            return array_tables[name_or_index]
        for table in array_tables:
            if table.name == name_or_index:
                return table
        raise KeyError(f'array table {name_or_index!r} not found')

    def _scalar_field(self, name_or_index: str | int) -> tuple[FastdbCallTableSpec, dict[str, str]]:
        scalar_fields = [
            (table, field)
            for table in self.plan.tables
            if table.kind == 'scalars'
            for field in table.scalar_fields
        ]
        if isinstance(name_or_index, int):
            return scalar_fields[name_or_index]
        for table, field in scalar_fields:
            if field['name'] == name_or_index:
                return table, field
        raise KeyError(f'scalar field {name_or_index!r} not found')

    def _single_logical_view(self) -> object:
        values = self.logical_values()
        if len(values) != 1:
            raise TypeError('direct view access is available only for single-value call-db payloads.')
        return values[0]

    @property
    def column(self) -> 'FastdbCallColumnView':
        value = self._single_logical_view()
        column = getattr(value, 'column', None)
        if column is None:
            raise TypeError('single call-db value does not expose columns.')
        return column

    def __len__(self) -> int:
        return len(self._single_logical_view())  # type: ignore[arg-type]

    def __getitem__(self, index: int) -> Any:
        return self._single_logical_view()[index]  # type: ignore[index]

    def __iter__(self):
        yield from self._single_logical_view()  # type: ignore[misc]

    def _ensure_alive(self) -> None:
        self.owner.assert_alive()
        try:
            _ = self.buffer.nbytes
        except ValueError as exc:
            raise RuntimeError('FastdbCallView buffer has been released.') from exc


@dataclass(frozen=True)
class FastdbCallTableView:
    call_view: FastdbCallView
    spec: FastdbCallTableSpec

    @property
    def _fdb_owner(self) -> FdbViewOwner:
        return self.call_view.owner

    def materialize(self) -> list[Any]:
        return _to_owned_fastdb_value(self._table())  # type: ignore[return-value]

    def to_owned(self) -> list[Any]:
        return self.materialize()

    @property
    def column(self) -> 'FastdbCallColumnView':
        return FastdbCallColumnView(self, self._table().column)

    def __len__(self) -> int:
        return len(self._table())

    def __getitem__(self, index: int) -> Any:
        return self._table()[index]

    def __iter__(self):
        table = self._table()
        for index in range(len(table)):
            self.call_view._ensure_alive()
            yield table[index]

    def _table(self) -> Table:
        self.call_view._ensure_alive()
        if self.spec.feature_type is None:
            raise ValueError(f'feature table {self.spec.name!r} is missing feature type.')
        return self.call_view.engine.table(
            self.spec.feature_type,
            name=self.spec.name,
            owner=self.call_view.owner,
            writeable=False,
        )


@dataclass(frozen=True)
class FastdbCallColumnView:
    table_view: FastdbCallTableView
    accessor: Any

    @property
    def _fdb_owner(self) -> FdbViewOwner:
        return self.table_view.call_view.owner

    def __getattr__(self, name: str) -> Any:
        self.table_view.call_view._ensure_alive()
        return getattr(self.accessor, name)


@dataclass(frozen=True)
class FastdbCallArrayView:
    call_view: FastdbCallView
    spec: FastdbCallTableSpec

    @property
    def _fdb_owner(self) -> FdbViewOwner:
        return self.call_view.owner

    def materialize(self) -> list[Any]:
        item_kind = _array_item_kind(self.spec)
        return [
            _materialize_scalar_value(item_kind, getattr(row, _ARRAY_VALUE_FIELD))
            for row in self._table()
        ]

    def to_owned(self) -> list[Any]:
        return self.materialize()

    def __len__(self) -> int:
        return len(self._table())

    def __getitem__(self, index: int) -> Any:
        return _materialize_scalar_value(
            _array_item_kind(self.spec),
            getattr(self._table()[index], _ARRAY_VALUE_FIELD),
        )

    def __iter__(self):
        table = self._table()
        item_kind = _array_item_kind(self.spec)
        for index in range(len(table)):
            self.call_view._ensure_alive()
            yield _materialize_scalar_value(
                item_kind,
                getattr(table[index], _ARRAY_VALUE_FIELD),
            )

    def _table(self) -> Table:
        self.call_view._ensure_alive()
        if self.spec.feature_type is None:
            raise ValueError(f'array table {self.spec.name!r} is missing feature type.')
        return self.call_view.engine.table(
            self.spec.feature_type,
            name=self.spec.name,
            owner=self.call_view.owner,
            writeable=False,
        )


def plan_call_db_input(
    *,
    method_name: str,
    parameters: list[tuple[str, object]] | tuple[tuple[str, object], ...],
    crm_context: dict[str, str] | None = None,
) -> FastdbCallPlan:
    scalar_fields: list[dict[str, str]] = []
    scalar_annotations: dict[str, object] = {}
    tables: list[FastdbCallTableSpec] = []
    scalar_positions: list[int] = []
    for position, (name, annotation) in enumerate(parameters):
        _reject_nullable_annotation(name, annotation)
        scalar_kind = _scalar_kind(annotation)
        if scalar_kind is not None:
            scalar_fields.append({
                'kind': scalar_kind,
                'name': name,
                'parameter': name,
            })
            scalar_positions.append(position)
            scalar_annotations[name] = _runtime_scalar_annotation(annotation)
            continue
        if _is_array_annotation(annotation):
            tables.append(_array_table_for_annotation(
                name,
                annotation,
                parameter=name,
                value_position=position,
                method_name=method_name,
                direction='input',
            ))
            continue
        tables.append(_feature_table_for_annotation(
            name,
            annotation,
            parameter=name,
            value_position=position,
        ))

    scalar_feature_type = None
    if scalar_fields:
        scalar_feature_type = _make_scalar_feature_type(
            method_name,
            'input',
            scalar_annotations,
            layer_name=_SCALAR_TABLE_NAME,
        )
        tables.insert(0, FastdbCallTableSpec(
            name=_SCALAR_TABLE_NAME,
            kind='scalars',
            cardinality='one',
            feature_type=scalar_feature_type,
            scalar_fields=tuple(scalar_fields),
            scalar_positions=tuple(scalar_positions),
        ))

    profile = _select_call_profile(tables)
    return FastdbCallPlan(
        method_name=method_name,
        direction='input',
        profile=profile,
        tables=tuple(tables),
        crm_context=dict(crm_context or {}),
        scalar_feature_type=scalar_feature_type,
    )


def plan_call_db_output(
    *,
    method_name: str,
    return_annotation: object,
    crm_context: dict[str, str] | None = None,
) -> FastdbCallPlan:
    annotations = _return_annotations(return_annotation)
    scalar_fields: list[dict[str, str]] = []
    scalar_annotations: dict[str, object] = {}
    tables: list[FastdbCallTableSpec] = []
    scalar_positions: list[int] = []
    for index, annotation in enumerate(annotations):
        _reject_nullable_annotation(f'return_{index}', annotation)
        scalar_kind = _scalar_kind(annotation)
        if scalar_kind is not None:
            field_name = f'return_{index}'
            scalar_fields.append({
                'kind': scalar_kind,
                'name': field_name,
            })
            scalar_positions.append(index)
            scalar_annotations[field_name] = _runtime_scalar_annotation(annotation)
            continue
        if _is_array_annotation(annotation):
            tables.append(_array_table_for_annotation(
                f'return_{index}',
                annotation,
                return_index=index,
                value_position=index,
                method_name=method_name,
                direction='output',
            ))
            continue
        tables.append(_feature_table_for_annotation(
            f'return_{index}',
            annotation,
            return_index=index,
            value_position=index,
        ))

    scalar_feature_type = None
    if scalar_fields:
        scalar_feature_type = _make_scalar_feature_type(
            method_name,
            'output',
            scalar_annotations,
            layer_name='__c2_return',
        )
        tables.insert(0, FastdbCallTableSpec(
            name='__c2_return',
            kind='scalars',
            cardinality='one',
            feature_type=scalar_feature_type,
            scalar_fields=tuple(scalar_fields),
            scalar_positions=tuple(scalar_positions),
        ))

    profile = _select_call_profile(tables)
    return FastdbCallPlan(
        method_name=method_name,
        direction='output',
        profile=profile,
        tables=tuple(tables),
        crm_context=dict(crm_context or {}),
        scalar_feature_type=scalar_feature_type,
    )


def _feature_table_for_annotation(
    table_name: str,
    annotation: object,
    *,
    parameter: str | None = None,
    return_index: int | None = None,
    value_position: int | None = None,
) -> FastdbCallTableSpec:
    origin = get_origin(annotation)
    args = get_args(annotation)
    cardinality = 'one'
    feature_type = annotation
    if origin is Batch:
        if len(args) != 1:
            raise TypeError(f'{table_name} uses a bare or unsupported Batch annotation.')
        cardinality = 'many'
        feature_type = args[0]
        _reject_nullable_annotation(table_name, feature_type, context='Batch item')
        if not is_feature(feature_type):
            raise TypeError(f'{table_name} uses Batch[Scalar]; use Batch[Feature] for feature tables or Array[Scalar] for homogeneous scalar arrays.')
    elif origin is list:
        if len(args) != 1:
            raise TypeError(f'{table_name} uses a bare or unsupported list annotation.')
        _raise_list_annotation_error(table_name, args[0])
    elif origin is Array:
        raise TypeError(f'{table_name} uses Array[Feature]; use Array[Scalar] for homogeneous scalar arrays or Batch[Feature] for feature tables.')
    if _is_python_builtin_scalar(feature_type):
        raise TypeError(
            f'{table_name} uses Python builtin scalar {feature_type.__name__}; '
            'use explicit fastdb scalar aliases such as I32, F64, STR, or BOOL for portable call-db.',
        )
    if not is_feature(feature_type):
        raise TypeError(f'{table_name} annotation {annotation!r} is not a fastdb @feature or list[@feature].')
    return FastdbCallTableSpec(
        name=table_name,
        kind='feature',
        cardinality=cardinality,
        feature_type=feature_type,
        feature_schema=export_schema(feature_type),
        feature_schema_dependencies=feature_schema_dependencies(feature_type),
        parameter=parameter,
        return_index=return_index,
        value_position=value_position,
    )


_ARRAY_VALUE_FIELD = 'value'


def _array_table_for_annotation(
    table_name: str,
    annotation: object,
    *,
    parameter: str | None = None,
    return_index: int | None = None,
    value_position: int | None = None,
    method_name: str,
    direction: str,
) -> FastdbCallTableSpec:
    args = get_args(annotation)
    if len(args) != 1:
        raise TypeError(f'{table_name} uses a bare or unsupported Array annotation.')
    item_annotation = args[0]
    _reject_nullable_annotation(table_name, item_annotation, context='Array item')
    item_kind = _scalar_kind(item_annotation)
    if item_kind is None:
        if _is_python_builtin_scalar(item_annotation):
            raise TypeError(
                f'{table_name} uses Array[{item_annotation.__name__}]; '
                'use explicit fastdb scalar aliases such as Array[I32], Array[F64], Array[STR], or Array[BOOL].',
            )
        raise TypeError(f'{table_name} uses Array[Feature]; use Array[Scalar] for homogeneous scalar arrays or Batch[Feature] for feature tables.')
    runtime_annotation = _runtime_scalar_annotation(item_annotation)
    feature_type = _make_array_feature_type(
        method_name,
        direction,
        table_name,
        runtime_annotation,
    )
    return FastdbCallTableSpec(
        name=table_name,
        kind='array',
        cardinality='many',
        feature_type=feature_type,
        parameter=parameter,
        return_index=return_index,
        value_position=value_position,
        array_item={
            'kind': item_kind,
            'name': _ARRAY_VALUE_FIELD,
        },
    )


def _return_annotations(return_annotation: object) -> tuple[object, ...]:
    origin = get_origin(return_annotation)
    if origin is tuple:
        args = get_args(return_annotation)
        if not args:
            raise TypeError('return annotation uses a bare tuple.')
        if len(args) == 2 and args[1] is Ellipsis:
            raise TypeError('variadic tuple returns are not supported by fastdb call-db.')
        if len(args) == 1:
            raise TypeError(
                'single-item tuple return annotations are not supported by fastdb call-db; '
                'use the item annotation directly.',
            )
        return tuple(args)
    return (return_annotation,)


def _select_call_profile(tables: list[FastdbCallTableSpec]) -> str:
    requires_object_graph = False
    for table in tables:
        if table.feature_type is None:
            continue
        columnar = columnar_capability(table.feature_type)
        if columnar['eligible']:
            continue
        graph = object_graph_capability(table.feature_type)
        if graph['eligible']:
            requires_object_graph = True
            continue
        raise TypeError(
            f'{table.name} is not eligible for fastdb call-db: '
            f'{graph["diagnostics"] or columnar["diagnostics"]}',
        )
    if not requires_object_graph:
        return CALL_DB_COLUMNAR_PROFILE
    _ensure_object_graph_call_supported(tables)
    return CALL_DB_OBJECT_GRAPH_PROFILE


def _ensure_object_graph_call_supported(tables: list[FastdbCallTableSpec]) -> None:
    seen_feature_types: dict[type, str] = {}
    seen_layer_names: dict[str, tuple[str, str]] = {}

    def claim_layer(layer_name: str, owner: str, kind: str) -> None:
        previous = seen_layer_names.get(layer_name)
        if previous is not None:
            previous_owner, previous_kind = previous
            if previous_kind == 'dependency' and kind == 'dependency' and previous_owner == owner:
                return
            raise TypeError(
                f'{CALL_DB_OBJECT_GRAPH_PROFILE} cannot encode both {previous_owner!r} '
                f'and {owner!r} as layer {layer_name!r}; use distinct feature wrapper '
                'types or table names.',
            )
        seen_layer_names[layer_name] = (owner, kind)

    for table in tables:
        if table.feature_type is None:
            continue
        previous = seen_feature_types.get(table.feature_type)
        if previous is not None:
            raise TypeError(
                f'{CALL_DB_OBJECT_GRAPH_PROFILE} cannot encode feature type '
                f'{table.feature_type.__name__} in both {previous!r} and {table.name!r}; '
                'use distinct wrapper feature types until named object-graph tables are supported.',
            )
        seen_feature_types[table.feature_type] = table.name
        layer_name = get_schema(table.feature_type).layer_name
        claim_layer(layer_name, f'table {table.name}', 'table')
        for dependency_layer_name, dependency_name in _feature_dependency_layer_names(table.feature_type):
            claim_layer(dependency_layer_name, f'dependency {dependency_name}', 'dependency')


def _feature_dependency_layer_names(feature_type: type) -> tuple[tuple[str, str], ...]:
    dependencies: dict[str, tuple[str, str]] = {}
    visiting: set[type] = set()

    def visit(current: type) -> None:
        if current in visiting:
            return
        visiting.add(current)
        schema = get_schema(current)
        for field in schema.ref_fields:
            _visit_target(field.ref_target)
        for field in schema.list_ref_fields:
            _visit_target(field.list_ref_target)
        visiting.remove(current)

    def _visit_target(target: type | None) -> None:
        if target is None or not is_feature(target):
            return
        schema = get_schema(target)
        if schema.layer_name not in dependencies:
            dependencies[schema.layer_name] = (schema.layer_name, target.__name__)
            visit(target)

    visit(feature_type)
    root_layer_name = get_schema(feature_type).layer_name
    dependencies.pop(root_layer_name, None)
    return tuple(
        dependencies[layer_name]
        for layer_name in sorted(dependencies)
    )


def _create_empty_feature_table(engine: ColumnEngine, table: FastdbCallTableSpec) -> None:
    if table.feature_type is None:
        raise ValueError(f'feature table {table.name!r} is missing feature type.')
    schema = get_schema(table.feature_type)
    diagnostics = [
        *raw_payload_storage_diagnostics(schema),
        *non_native_list_storage_diagnostics(schema),
    ]
    if diagnostics:
        raise TypeError(
            f"fastdb call-db cannot create a native table for "
            f"{table.feature_type.__name__}: {'; '.join(diagnostics)}"
        )
    origin = engine._origin
    mapped = Table.map_from(
        table.feature_type,
        _get_default_table_build(
            origin,
            table.name,
            raw_payload=bool(schema.bytes_plan),
        ),
        origin,
    )
    for field in schema.fields:
        if field.field_type == OriginFieldType.list:
            mapped._origin.add_list_field(field.name, field.cpp_type)  # noqa: SLF001
        else:
            mapped._origin.add_field(field.name, field.field_type.value)  # noqa: SLF001
    engine._table_map[table.name] = mapped  # noqa: SLF001
    engine._table_feature_types[table.name] = table.feature_type  # noqa: SLF001


def _array_table_values(table: FastdbCallTableSpec, value: Any) -> list[Any]:
    if table.feature_type is None:
        raise ValueError(f'array table {table.name!r} is missing feature type.')
    if isinstance(value, (str, bytes, bytearray, memoryview, Mapping)) or not isinstance(value, Iterable):
        raise TypeError(f'{table.name} expected an iterable Array[{_array_item_kind(table)}], got {type(value).__name__}.')
    item_kind = _array_item_kind(table)
    return [_coerce_scalar_value(item_kind, item) for item in value]


def _feature_table_rows(table: FastdbCallTableSpec, value: Any) -> list[Any]:
    if table.feature_type is None:
        raise ValueError(f'feature table {table.name!r} is missing feature type.')
    records = _table_like_records(value)
    source = records if records is not None else value
    if isinstance(source, (str, bytes, bytearray, memoryview, Mapping)) or not isinstance(source, Iterable):
        raise TypeError(f'{table.name} expected an iterable Batch[{table.feature_type.__name__}], got {type(value).__name__}.')
    return [_coerce_feature_row(table.feature_type, row) for row in source]


def _coerce_feature_row(feature_type: type, row: Any) -> Any:
    if isinstance(row, feature_type):
        return row
    values: dict[str, Any] = {}
    for field in get_schema(feature_type).fields:
        if isinstance(row, Mapping):
            if field.name not in row:
                raise KeyError(f'missing field {field.name!r} for fastdb feature {feature_type.__name__}.')
            value = row[field.name]
            kind = _FIELD_SCALAR_KIND_BY_FIELD_TYPE.get(field.field_type)
            values[field.name] = _coerce_scalar_value(kind, value) if kind is not None else value
            continue
        if not hasattr(row, field.name):
            raise KeyError(f'missing field {field.name!r} for fastdb feature {feature_type.__name__}.')
        value = getattr(row, field.name)
        kind = _FIELD_SCALAR_KIND_BY_FIELD_TYPE.get(field.field_type)
        values[field.name] = _coerce_scalar_value(kind, value) if kind is not None else value
    return feature_type(**values)


def _table_like_records(value: Any) -> Iterable[Any] | None:
    if isinstance(value, (str, bytes, bytearray, memoryview, Mapping)):
        return None
    to_pylist = getattr(value, 'to_pylist', None)
    if callable(to_pylist):
        return to_pylist()
    to_dicts = getattr(value, 'to_dicts', None)
    if callable(to_dicts):
        return to_dicts()
    to_dict = getattr(value, 'to_dict', None)
    if not callable(to_dict):
        return None
    try:
        return to_dict('records')
    except TypeError:
        try:
            return to_dict(orient='records')
        except TypeError:
            return None


def _column_engine_from_buffer(data: bytes | bytearray | memoryview) -> ColumnEngine:
    engine = ColumnEngine()
    engine._origin = core.WxDatabase.load_xbuffer(data)  # noqa: SLF001
    engine._origin._buffer = data  # noqa: SLF001
    return engine


def _object_engine_from_buffer(
    data: bytes | bytearray | memoryview,
    tables: tuple[FastdbCallTableSpec, ...] = (),
) -> ObjectEngine:
    engine = ObjectEngine()
    engine._db = core.WxDatabase.load_xbuffer(data)  # noqa: SLF001
    engine._db._buffer = data  # noqa: SLF001
    engine._buffer = data  # noqa: SLF001
    engine._built = True  # noqa: SLF001

    for index in range(engine._db.get_layer_count()):  # noqa: SLF001
        layer = engine._db.get_layer(index)  # noqa: SLF001
        registered_cls = lookup_class(layer.name())
        if registered_cls is None:
            continue
        schema = get_schema(registered_cls)
        state = LayerState(
            cls=registered_cls,
            schema=schema,
            layer_idx=index,
            row_count=layer.get_feature_count(),
        )
        engine._layers[registered_cls] = state  # noqa: SLF001
        engine._layer_order.append(registered_cls)  # noqa: SLF001
    _bind_call_plan_layers(engine, tables)
    return engine


def _bind_call_plan_layers(
    engine: ObjectEngine,
    tables: tuple[FastdbCallTableSpec, ...],
) -> None:
    if not tables:
        return
    layer_indices = {
        engine._db.get_layer(index).name(): index  # noqa: SLF001
        for index in range(engine._db.get_layer_count())  # noqa: SLF001
    }
    for table in tables:
        if table.feature_type is None:
            continue
        schema = get_schema(table.feature_type)
        layer_idx = layer_indices.get(schema.layer_name)
        if layer_idx is None:
            continue
        layer = engine._db.get_layer(layer_idx)  # noqa: SLF001
        engine._layers[table.feature_type] = LayerState(  # noqa: SLF001
            cls=table.feature_type,
            schema=schema,
            layer_idx=layer_idx,
            row_count=layer.get_feature_count(),
        )
        if table.feature_type not in engine._layer_order:  # noqa: SLF001
            engine._layer_order.append(table.feature_type)  # noqa: SLF001


def _copy_object_graph_feature(
    engine: ObjectEngine,
    feature_type: type,
    row_idx: int,
    seen: dict[tuple[type, int], object],
) -> object:
    key = (feature_type, row_idx)
    existing = seen.get(key)
    if existing is not None:
        return existing

    state = engine._layers[feature_type]  # noqa: SLF001
    layer = engine._db.get_layer(state.layer_idx)  # noqa: SLF001
    feature_value = layer.tryGetFeature(row_idx)
    schema = get_schema(feature_type)
    obj = feature_type.__new__(feature_type)
    seen[key] = obj

    from fastdb4py.reader import _read_field

    for field in schema.fields:
        if field.field_type == OriginFieldType.ref:
            ref = feature_value.get_field_as_ref(field.field_id)
            target_type = _target_type_from_ref(engine, ref, field.ref_target)
            if target_type is None:
                value = None
            else:
                value = _copy_object_graph_feature(
                    engine,
                    target_type,
                    _decode_ref_row(ref),
                    seen,
                )
        elif field.field_type == OriginFieldType.list and field.list_elem_type == OriginFieldType.ref:
            target_type = field.list_ref_target
            value = []
            for index in range(feature_value.get_field_list_size(field.field_id)):
                ref = feature_value.get_field_list_ref_at(field.field_id, index)
                item_type = _target_type_from_ref(engine, ref, target_type)
                if item_type is None:
                    value.append(None)
                else:
                    value.append(
                        _copy_object_graph_feature(
                            engine,
                            item_type,
                            _decode_ref_row(ref),
                            seen,
                        ),
                    )
        else:
            value = _read_field(feature_value, field)
        obj.__dict__[field.name] = value
    return obj


def _target_type_from_ref(
    engine: ObjectEngine,
    ref: object,
    declared_target: type | None,
) -> type | None:
    if ref is None:
        return None
    layer_idx = getattr(ref, 'ilayer', None)
    if layer_idx is None:
        return None
    if declared_target is not None:
        return declared_target
    if 0 <= layer_idx < len(engine._layer_order):  # noqa: SLF001
        return engine._layer_order[layer_idx]  # noqa: SLF001
    return None


def _decode_ref_row(ref: object) -> int:
    low = int(getattr(ref, 'ifeature', 0))
    high = int(getattr(ref, 'ifeatureH', 0))
    return low | (high << 8)


def _scalar_kind(annotation: object) -> str | None:
    if annotation is BOOL:
        return 'bool'
    if _is_python_builtin_scalar(annotation):
        return None
    field_type = get_origin_type(annotation)
    if field_type == OriginFieldType.unknown:
        return None
    return _FIELD_SCALAR_KIND_BY_FIELD_TYPE.get(field_type)


def _runtime_scalar_annotation(annotation: object) -> object:
    field_type = get_origin_type(annotation)
    if field_type == OriginFieldType.i32:
        return I32
    if field_type == OriginFieldType.f64:
        return F64
    if field_type == OriginFieldType.str:
        return STR
    if field_type == OriginFieldType.wstr:
        return WSTR
    if field_type == OriginFieldType.bytes:
        return BYTES
    return annotation


def _materialize_scalar_value(kind: str, value: object) -> object:
    if kind == 'bool':
        return coerce_bool_scalar(value)
    return value


def _coerce_scalar_value(kind: str, value: object) -> object:
    if kind == 'bool':
        return coerce_bool_scalar(value)
    if kind in {'u8', 'u16', 'u32', 'i32', 'u8n', 'u16n'}:
        return int(value)
    if kind in {'f32', 'f64'}:
        return float(value)
    if kind in {'str', 'wstr'}:
        return str(value)
    if kind == 'bytes':
        return bytes(value)
    return value


def _array_item_kind(table: FastdbCallTableSpec) -> str:
    if table.array_item is None:
        raise ValueError(f'array table {table.name!r} is missing item schema.')
    return table.array_item['kind']


def _is_python_builtin_scalar(annotation: object) -> bool:
    return annotation in _PYTHON_BUILTIN_SCALARS


def _raise_list_annotation_error(table_name: str, item_annotation: object) -> None:
    if is_feature(item_annotation):
        raise TypeError(
            f'{table_name} uses list[Feature]; use Batch[Feature] for portable fastdb call-db batches.',
        )
    if _scalar_kind(item_annotation) is not None:
        raise TypeError(
            f'{table_name} uses list[Scalar]; use Array[Scalar] for portable fastdb call-db arrays.',
        )
    if _is_python_builtin_scalar(item_annotation):
        raise TypeError(
            f'{table_name} uses list[{item_annotation.__name__}]; '
            'use explicit fastdb Array[...] or Batch[...] ABI markers for portable call-db.',
        )
    raise TypeError(
        f'{table_name} uses list[...] in a CRM call-db annotation; '
        'use Batch[Feature] for feature batches or Array[Scalar] for scalar arrays.',
    )


def _reject_nullable_annotation(
    table_name: str,
    annotation: object,
    *,
    context: str = 'annotation',
) -> None:
    if not _is_nullable_annotation(annotation):
        return
    raise TypeError(
        f'{table_name} {context} {annotation!r} is nullable; '
        'fastdb call-db does not support nullable CRM ABI values yet. '
        'Use non-null fastdb aliases/features or keep the method on Python-only fallback.',
    )


def _is_nullable_annotation(annotation: object) -> bool:
    if annotation is None or annotation is _NONE_TYPE:
        return True
    origin = get_origin(annotation)
    if origin not in (Union, UnionType):
        return False
    return any(arg is _NONE_TYPE for arg in get_args(annotation))


def _make_scalar_feature_type(
    method_name: str,
    direction: str,
    annotations: dict[str, object],
    *,
    layer_name: str,
) -> type:
    suffix = _identifier_suffix(method_name, direction, annotations)
    cls = type(
        f'_C2{direction.title()}{suffix}Scalars',
        (),
        {
            '__annotations__': dict(annotations),
            '__fastdb_layer_name__': layer_name,
            '__module__': __name__,
        },
    )
    return feature(cls)


def _make_array_feature_type(
    method_name: str,
    direction: str,
    table_name: str,
    annotation: object,
) -> type:
    suffix = _identifier_suffix(
        f'{method_name}_{table_name}',
        direction,
        {_ARRAY_VALUE_FIELD: annotation},
    )
    cls = type(
        f'_C2{direction.title()}{suffix}Array',
        (),
        {
            '__annotations__': {_ARRAY_VALUE_FIELD: annotation},
            '__fastdb_layer_name__': table_name,
            '__module__': __name__,
        },
    )
    return feature(cls)


def _identifier_suffix(method_name: str, direction: str, annotations: dict[str, object]) -> str:
    text = json.dumps(
        {
            'annotations': [
                [name, _scalar_kind(annotation)]
                for name, annotation in annotations.items()
            ],
            'direction': direction,
            'method': method_name,
        },
        sort_keys=True,
        separators=(',', ':'),
    )
    digest = schema_sha256({'schema': 'fastdb.call-db.scalar-class.v1', 'text': text})[:10]
    raw = re.sub(r'[^0-9A-Za-z]+', '_', method_name).strip('_') or 'Method'
    return f'{raw}_{digest}'


def _crm_descriptor(context: dict[str, str]) -> dict[str, str] | None:
    _validate_crm_context(context)
    namespace = context.get('crm_namespace')
    name = context.get('crm_name')
    version = context.get('crm_version')
    if namespace is None and name is None and version is None:
        return None
    return {
        'name': name or '',
        'namespace': namespace or '',
        'version': version or '',
    }


def _validate_crm_context(context: dict[str, str]) -> None:
    if not isinstance(context, dict):
        raise ValueError('fastdb call-db plan CRM context must be a dictionary.')
    values = [context.get(key) for key in _CRM_CONTEXT_KEYS]
    if all(value is None for value in values):
        return
    if any(not isinstance(value, str) or not value for value in values):
        raise ValueError(
            'fastdb call-db plan must include complete CRM context '
            '(crm_namespace, crm_name, crm_version) when any CRM context field is supplied.',
        )


def _value_count(
    tables: tuple[FastdbCallTableSpec, ...],
    *,
    scalar_feature_type: type | None = None,
) -> int:
    _validate_table_names(tables)
    _validate_table_shapes(tables, scalar_feature_type=scalar_feature_type)
    positions: list[int] = []
    for table in tables:
        if table.kind == 'scalars':
            positions.extend(table.scalar_positions)
            continue
        if table.value_position is None:
            raise ValueError(f'fastdb call-db {table.kind} table {table.name!r} is missing value_position.')
        positions.append(table.value_position)
    if not positions:
        return 0
    _validate_value_positions(positions)
    return len(positions)


def _validate_table_names(tables: tuple[FastdbCallTableSpec, ...]) -> None:
    seen: set[str] = set()
    duplicates: list[str] = []
    for table in tables:
        if not isinstance(table.name, str) or not table.name:
            raise ValueError('fastdb call-db table names must be non-empty strings.')
        if table.name in seen:
            duplicates.append(table.name)
            continue
        seen.add(table.name)
    if duplicates:
        raise ValueError(f'fastdb call-db duplicate table name values: {sorted(set(duplicates))!r}.')


def _validate_table_shapes(
    tables: tuple[FastdbCallTableSpec, ...],
    *,
    scalar_feature_type: type | None = None,
) -> None:
    for table in tables:
        _validate_table_shape(table, scalar_feature_type=scalar_feature_type)


def _validate_table_shape(
    table: FastdbCallTableSpec,
    *,
    scalar_feature_type: type | None = None,
) -> None:
    if table.kind == 'scalars':
        if table.cardinality != 'one':
            raise ValueError(
                f'fastdb call-db scalar table {table.name!r} must have cardinality "one".',
            )
        _validate_scalar_metadata_shape(table)
        if len(table.scalar_fields) != len(table.scalar_positions):
            raise ValueError(
                f'fastdb call-db scalar table {table.name!r} field/value_position metadata mismatch.',
            )
        _validate_value_position_items(list(table.scalar_positions))
        _validate_scalar_fields(table, scalar_feature_type=scalar_feature_type)
        return
    if table.kind == 'array':
        if table.cardinality != 'many':
            raise ValueError(
                f'fastdb call-db array table {table.name!r} must have cardinality "many".',
            )
        _validate_array_item(table)
        return
    if table.kind == 'feature':
        if table.cardinality not in {'one', 'many'}:
            raise ValueError(
                f'fastdb call-db feature table {table.name!r} must have cardinality "one" or "many".',
            )
        _validate_feature_table_matches_runtime_schema(table)
        return
    raise ValueError(f'Unsupported fastdb call-db table kind {table.kind!r}.')


def _validate_scalar_fields(
    table: FastdbCallTableSpec,
    *,
    scalar_feature_type: type | None = None,
) -> None:
    seen: set[str] = set()
    duplicates: list[str] = []
    for field in table.scalar_fields:
        if not isinstance(field, dict):
            raise ValueError(f'fastdb call-db scalar table {table.name!r} field entries must be objects.')
        name = field.get('name')
        if not isinstance(name, str) or not name:
            raise ValueError(f'fastdb call-db scalar field in table {table.name!r} must include a non-empty name.')
        if name in seen:
            duplicates.append(name)
            continue
        seen.add(name)
        _validate_scalar_kind(field.get('kind'), f'fastdb call-db scalar field {name!r}')
    if duplicates:
        raise ValueError(f'fastdb call-db duplicate scalar field name values: {sorted(set(duplicates))!r}.')
    runtime_feature_type = scalar_feature_type if scalar_feature_type is not None else table.feature_type
    if runtime_feature_type is not None:
        _validate_scalar_fields_match_runtime_feature(table, runtime_feature_type)


def _validate_scalar_metadata_shape(table: FastdbCallTableSpec) -> None:
    if not isinstance(table.scalar_fields, (list, tuple)):
        raise ValueError(f'fastdb call-db scalar table {table.name!r} must include fields metadata.')
    if not isinstance(table.scalar_positions, (list, tuple)):
        raise ValueError(f'fastdb call-db scalar table {table.name!r} must include value_position metadata.')


def _validate_array_item(table: FastdbCallTableSpec) -> None:
    if not isinstance(table.array_item, dict):
        raise ValueError(f'array table {table.name!r} is missing item schema.')
    item_name = table.array_item.get('name')
    if item_name != _ARRAY_VALUE_FIELD:
        raise ValueError(
            f'fastdb call-db array table {table.name!r} item name must be {_ARRAY_VALUE_FIELD!r}.',
        )
    _validate_scalar_kind(
        table.array_item.get('kind'),
        f'fastdb call-db array table {table.name!r} item',
    )
    _validate_array_item_matches_runtime_feature(table)


def _validate_scalar_kind(kind: object, context: str) -> None:
    if kind not in _VALID_CALL_DB_SCALAR_KINDS:
        raise ValueError(f'{context} uses unsupported fastdb scalar kind {kind!r}.')


def _validate_scalar_fields_match_runtime_feature(
    table: FastdbCallTableSpec,
    scalar_feature_type: type,
) -> None:
    annotations = _runtime_feature_annotations(
        scalar_feature_type,
        context=f'fastdb call-db scalar table {table.name!r}',
    )
    field_names = {field['name'] for field in table.scalar_fields}
    annotation_names = set(annotations)
    if field_names != annotation_names:
        raise ValueError(
            f'fastdb call-db scalar table {table.name!r} field metadata {sorted(field_names)!r} '
            f'does not match runtime feature fields {sorted(annotation_names)!r}.',
        )
    for field in table.scalar_fields:
        field_name = field['name']
        expected_kind = _scalar_kind(annotations[field_name])
        if expected_kind is None:
            raise ValueError(
                f'fastdb call-db scalar field {field_name!r} has unsupported runtime feature annotation '
                f'{annotations[field_name]!r}.',
            )
        if field.get('kind') != expected_kind:
            raise ValueError(
                f'fastdb call-db scalar field {field_name!r} metadata kind {field.get("kind")!r} '
                f'does not match runtime feature kind {expected_kind!r}.',
            )


def _validate_array_item_matches_runtime_feature(table: FastdbCallTableSpec) -> None:
    annotations = _runtime_feature_annotations(
        table.feature_type,
        context=f'fastdb call-db array table {table.name!r}',
    )
    if set(annotations) != {_ARRAY_VALUE_FIELD}:
        raise ValueError(
            f'fastdb call-db array table {table.name!r} runtime feature fields must be [{_ARRAY_VALUE_FIELD!r}].',
        )
    expected_kind = _scalar_kind(annotations[_ARRAY_VALUE_FIELD])
    if expected_kind is None:
        raise ValueError(
            f'fastdb call-db array table {table.name!r} has unsupported runtime feature annotation '
            f'{annotations[_ARRAY_VALUE_FIELD]!r}.',
        )
    if table.array_item is None:
        raise ValueError(f'array table {table.name!r} is missing item schema.')
    if table.array_item.get('kind') != expected_kind:
        raise ValueError(
            f'fastdb call-db array table {table.name!r} item metadata kind {table.array_item.get("kind")!r} '
            f'does not match runtime feature kind {expected_kind!r}.',
        )


def _validate_feature_table_matches_runtime_schema(table: FastdbCallTableSpec) -> None:
    if table.feature_type is None:
        raise ValueError(f'feature table {table.name!r} is missing feature type.')
    if not isinstance(table.feature_schema, dict):
        raise ValueError(f'feature table {table.name!r} is missing feature schema.')
    expected_schema = export_schema(table.feature_type)
    if (
        schema_sha256(table.feature_schema) != schema_sha256(expected_schema)
        or table.feature_schema.get('feature') != expected_schema.get('feature')
    ):
        raise ValueError(
            f'fastdb call-db feature table {table.name!r} feature schema '
            f'does not match runtime feature {table.feature_type.__name__}.',
        )
    dependencies = table.feature_schema_dependencies
    if not isinstance(dependencies, (tuple, list)):
        raise ValueError(f'fastdb call-db feature table {table.name!r} feature schema dependencies must be a sequence.')
    if any(not isinstance(dependency, dict) for dependency in dependencies):
        raise ValueError(f'fastdb call-db feature table {table.name!r} feature schema dependency entries must be objects.')
    expected_dependencies = feature_schema_dependencies(table.feature_type)
    actual_hashes = tuple(schema_sha256(dependency) for dependency in dependencies)
    expected_hashes = tuple(schema_sha256(dependency) for dependency in expected_dependencies)
    actual_features = tuple(dependency.get('feature') for dependency in dependencies)
    expected_features = tuple(dependency.get('feature') for dependency in expected_dependencies)
    if actual_hashes != expected_hashes or actual_features != expected_features:
        raise ValueError(
            f'fastdb call-db feature table {table.name!r} feature schema dependencies '
            f'does not match runtime feature {table.feature_type.__name__}.',
        )


def _runtime_feature_annotations(feature_type: type | None, *, context: str) -> dict[str, object]:
    if feature_type is None:
        raise ValueError(f'{context} is missing runtime feature type.')
    try:
        hints = get_type_hints(feature_type)
    except NameError:
        hints = dict(getattr(feature_type, '__annotations__', {}))
    return {
        name: annotation
        for name, annotation in hints.items()
        if isinstance(name, str) and not name.startswith('_')
    }


def _validate_value_positions(positions: list[int]) -> None:
    _validate_value_position_items(positions)
    actual = sorted(set(positions))
    expected = list(range(len(actual)))
    if actual != expected:
        raise ValueError(f'fastdb call-db value_position values must be contiguous from 0; got {actual!r}.')


def _validate_value_position_items(positions: list[int]) -> None:
    seen: set[int] = set()
    duplicates: list[int] = []
    for position in positions:
        if type(position) is not int or position < 0:
            raise ValueError('fastdb call-db value_position values must be non-negative integers.')
        if position in seen:
            duplicates.append(position)
            continue
        seen.add(position)
    if duplicates:
        raise ValueError(f'fastdb call-db duplicate value_position values: {sorted(set(duplicates))!r}.')


def _is_array_annotation(annotation: object) -> bool:
    return get_origin(annotation) is Array


_METHOD_PAYLOAD_BINDING_CACHE: dict[tuple[str, str, str, str], object] = {}


def resolve_method_payload_abi(shape: object, context: object | None = None) -> object | None:
    """Resolve a C-Two method shape to a FastDB call-db payload binding."""
    try:
        direction = str(getattr(shape, 'direction'))
        if direction == 'input':
            plan = plan_call_db_input(
                method_name=str(getattr(shape, 'method_name')),
                parameters=[
                    (str(getattr(parameter, 'name')), getattr(parameter, 'annotation'))
                    for parameter in getattr(shape, 'parameters')
                ],
                crm_context=_crm_context_from_shape(shape, context),
            )
        elif direction == 'output':
            plan = plan_call_db_output(
                method_name=str(getattr(shape, 'method_name')),
                return_annotation=getattr(shape, 'return_annotation'),
                crm_context=_crm_context_from_shape(shape, context),
            )
        else:
            return None
    except (TypeError, ValueError):
        return None

    return _method_payload_binding_for(plan)


def diagnostics_for_method_payload_abi(
    shape: object,
    context: object | None = None,
) -> list[dict[str, Any]]:
    try:
        direction = str(getattr(shape, 'direction'))
        if direction == 'input':
            plan_call_db_input(
                method_name=str(getattr(shape, 'method_name')),
                parameters=[
                    (str(getattr(parameter, 'name')), getattr(parameter, 'annotation'))
                    for parameter in getattr(shape, 'parameters')
                ],
                crm_context=_crm_context_from_shape(shape, context),
            )
        elif direction == 'output':
            plan_call_db_output(
                method_name=str(getattr(shape, 'method_name')),
                return_annotation=getattr(shape, 'return_annotation'),
                crm_context=_crm_context_from_shape(shape, context),
            )
        else:
            return []
    except (TypeError, ValueError) as exc:
        return [_call_db_diagnostic(shape, str(getattr(shape, 'direction', '<unknown>')), exc)]
    return []


def _method_payload_binding_for(plan: FastdbCallPlan) -> object:
    payload_abi_ref = plan.payload_abi_ref
    key = (
        plan.direction,
        plan.method_name,
        plan.profile,
        payload_abi_ref['schema_sha256'],
    )
    cached = _METHOD_PAYLOAD_BINDING_CACHE.get(key)
    if cached is not None:
        return cached

    from c_two.crm.payload_plan import PayloadBinding, PayloadPlanKind

    def serialize(*values, _plan=plan) -> bytes:
        if _plan.direction == 'input':
            return _plan.serialize_values(values)
        return _plan.serialize_values(values[0] if len(values) == 1 else values)

    def deserialize(data, _plan=plan):
        return _plan.deserialize_values(data)

    view_from_buffer = None
    if plan.supports_buffer_view:
        def view_from_buffer(data: memoryview, _plan=plan):
            return _plan.view_from_buffer(data)

    suffix = f'{plan.method_name}_{plan.direction}_{payload_abi_ref["schema_sha256"][:10]}'
    binding = PayloadBinding(
        kind=PayloadPlanKind.FDB,
        serialize=serialize,
        deserialize=deserialize,
        payload_abi_ref=payload_abi_ref,
        payload_abi_artifacts=_plan_payload_abi_artifacts(plan),
        view_from_buffer=view_from_buffer,
        label=f'FastdbC2Call{_safe_identifier(suffix)}',
    )
    _METHOD_PAYLOAD_BINDING_CACHE[key] = binding
    return binding


def _crm_context_from_shape(shape: object, context: object | None) -> dict[str, str]:
    source: dict[str, Any] = {}
    if isinstance(context, dict):
        source.update(context)
    for field in ('crm_namespace', 'crm_name', 'crm_version'):
        value = getattr(shape, field, None)
        if value is not None:
            source[field] = value
    return {
        field: str(source[field])
        for field in ('crm_namespace', 'crm_name', 'crm_version')
        if field in source and source[field] is not None
    }


def _plan_payload_abi_artifacts(plan: FastdbCallPlan) -> tuple[dict[str, Any], ...]:
    artifacts: list[dict[str, Any]] = [plan.schema_descriptor]
    for table in plan.tables:
        if table.feature_schema is not None:
            artifacts.append(table.feature_schema)
        artifacts.extend(table.feature_schema_dependencies)
    return _dedupe_payload_abi_artifacts(artifacts)


def _dedupe_payload_abi_artifacts(
    artifacts: list[dict[str, Any]] | tuple[dict[str, Any], ...],
) -> tuple[dict[str, Any], ...]:
    deduped: list[dict[str, Any]] = []
    seen: set[str] = set()
    for artifact in artifacts:
        key = _canonical_json(artifact)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(artifact)
    return tuple(deduped)


def _canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(',', ':'))


def _call_db_diagnostic(shape: object, position: str, exc: Exception) -> dict[str, Any]:
    method_name = str(getattr(shape, 'method_name', '<unknown>'))
    return {
        'code': 'fastdb_call_db_not_planned',
        'message': (
            f'{method_name}.{position} is not a portable fastdb call-db payload: {exc}'
        ),
        'position': position,
        'reason': str(exc),
        'severity': 'warning',
    }


def _safe_identifier(value: str) -> str:
    result = ''.join(ch if ch.isalnum() else '_' for ch in value)
    return result.strip('_') or 'Method'
