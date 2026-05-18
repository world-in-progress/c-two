from __future__ import annotations

import dataclasses
import json
import types
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any, ForwardRef, Generic, TypeVar, Union, get_args, get_origin, get_type_hints

from c_two.crm.codec import CodecBinding, CodecRef, use_codec
from c_two.crm.transferable import Transferable

try:
    import pyarrow as pa
except ModuleNotFoundError as exc:  # pragma: no cover - exercised only without optional dependency
    raise ModuleNotFoundError(
        'c_two.providers.arrow requires pyarrow. Install the c-two examples/dev '
        'dependencies or add pyarrow to your project environment.',
    ) from exc

ARROW_IPC_CODEC_ID = 'org.apache.arrow.ipc'
ARROW_IPC_CODEC_VERSION = '1'
ARROW_IPC_SCHEMA = 'arrow.ipc.schema.v1'
ARROW_IPC_MEDIA_TYPE = 'application/vnd.apache.arrow.stream'
ARROW_IPC_BYTES_CAPABILITIES = ('bytes',)
ARROW_IPC_BUFFER_VIEW_CAPABILITIES = ('bytes', 'buffer-view')
ARROW_IPC_CAPABILITIES = ARROW_IPC_BYTES_CAPABILITIES

_RECORD_MARKER = '__c_two_arrow_record__'
_GENERATED_MODULE = 'c_two.providers.arrow.generated'
T = TypeVar('T')


class Batch(Generic[T]):
    """Explicit annotation for methods that should return an Arrow view normally."""


class ArrowBatchView(Generic[T]):
    """Arrow table view returned by ``arrow.Batch[T]`` and held ``list[T]`` calls."""

    __slots__ = ('_owner', '_record_type', '_table')

    def __init__(self, record_type: type[T], table: Any, owner: object | None = None) -> None:
        self._record_type = record_type
        self._table = table
        self._owner = owner

    @property
    def record_type(self) -> type[T]:
        return self._record_type

    @property
    def table(self) -> Any:
        return self._table

    def __len__(self) -> int:
        return int(self._table.num_rows)

    def __iter__(self):
        return iter(self.to_records())

    def to_table(self) -> Any:
        return self._table

    def to_records(self) -> list[T]:
        return [
            self._record_type(**row)
            for row in self._table.to_pylist()
        ]


@dataclass(frozen=True)
class ArrowRecordOptions:
    name: str | None = None
    schema_id: str | None = None


@dataclass(frozen=True)
class _ArrowFieldSpec:
    name: str
    arrow_type: Any
    type_text: str
    nullable: bool


@dataclass(frozen=True)
class _ArrowRecordSpec:
    record_type: type
    record_name: str
    schema_id: str
    crm_namespace: str | None
    crm_name: str | None
    crm_version: str | None
    fields: tuple[_ArrowFieldSpec, ...]

    def schema_text(self, mode: str) -> str:
        if mode not in {'single', 'batch'}:
            raise ValueError(f'unsupported Arrow schema mode: {mode!r}')
        payload: dict[str, object] = {
            'codec': ARROW_IPC_CODEC_ID,
            'codec_version': ARROW_IPC_CODEC_VERSION,
            'fields': [
                {
                    'name': field.name,
                    'nullable': field.nullable,
                    'type': field.type_text,
                }
                for field in self.fields
            ],
            'mode': mode,
            'record': self.record_name,
            'schema': 'c-two.arrow.record.v1',
            'schema_id': self.schema_id,
        }
        if self.crm_namespace is not None:
            payload['crm'] = {
                'name': self.crm_name,
                'namespace': self.crm_namespace,
                'version': self.crm_version,
            }
        return json.dumps(payload, sort_keys=True, separators=(',', ':'))

    def arrow_schema(self) -> Any:
        return pa.schema([
            pa.field(field.name, field.arrow_type, nullable=field.nullable)
            for field in self.fields
        ])


def record(
    cls: type | None = None,
    *,
    name: str | None = None,
    schema_id: str | None = None,
):
    """Mark a dataclass-like record as an Arrow IPC payload type."""

    def wrap(target: type) -> type:
        if not isinstance(target, type):
            raise TypeError('@arrow.record must decorate a class.')
        if name is not None and (not isinstance(name, str) or not name.strip()):
            raise ValueError('name must be a non-empty string.')
        if schema_id is not None and (not isinstance(schema_id, str) or not schema_id.strip()):
            raise ValueError('schema_id must be a non-empty string.')
        record_cls = target if dataclasses.is_dataclass(target) else dataclasses.dataclass(target)
        setattr(
            record_cls,
            _RECORD_MARKER,
            ArrowRecordOptions(
                name=name.strip() if name is not None else None,
                schema_id=schema_id.strip() if schema_id is not None else None,
            ),
        )
        use_codec(_DEFAULT_PROVIDER)
        return record_cls

    if cls is not None:
        if not isinstance(cls, type):
            raise TypeError('@arrow.record must decorate a class.')
        return wrap(cls)
    return wrap


class ArrowCodecProvider:
    """C-Two codec provider for dataclass records encoded as Arrow IPC streams."""

    def __init__(self) -> None:
        self._single_cache: dict[tuple[type, str, str], CodecBinding] = {}
        self._batch_cache: dict[tuple[type, str, str], CodecBinding] = {}
        self._batch_view_cache: dict[tuple[type, str, str], CodecBinding] = {}

    def candidates_for_type(
        self,
        annotation: object,
        context: object | None = None,
    ) -> CodecBinding | None:
        if _is_arrow_record(annotation):
            return self._single_binding(annotation, context)
        origin = get_origin(annotation)
        args = get_args(annotation)
        if origin is list and len(args) == 1 and _is_arrow_record(args[0]):
            return self._batch_binding(args[0], context)
        if origin is Batch and len(args) == 1 and _is_arrow_record(args[0]):
            return self._batch_view_binding(args[0], context)
        return None

    def _single_binding(self, record_type: type, context: object | None) -> CodecBinding:
        spec = _record_spec(record_type, context)
        schema_text = spec.schema_text('single')
        cached = self._single_cache.get((record_type, 'single', schema_text))
        if cached is not None:
            return cached
        schema = spec.arrow_schema()
        codec_ref = _codec_ref_for_schema(schema_text)

        def serialize(value: object) -> bytes:
            if not isinstance(value, record_type):
                raise TypeError(
                    f'expected {record_type.__name__}, got {type(value).__name__}',
                )
            table = pa.Table.from_pylist([_record_to_row(value)], schema=schema)
            return _table_to_ipc(table)

        def deserialize(data: memoryview | bytes) -> object:
            rows = _ipc_to_rows(data)
            if len(rows) != 1:
                raise ValueError(
                    f'expected one Arrow IPC row for {record_type.__name__}, got {len(rows)}',
                )
            return record_type(**rows[0])

        transferable = _make_transferable(
            _generated_class_name(record_type, 'Transferable', codec_ref),
            codec_ref,
            serialize,
            deserialize,
            schema_text=schema_text,
        )
        binding = CodecBinding(transferable=transferable, codec_ref=codec_ref)
        self._single_cache[(record_type, 'single', schema_text)] = binding
        return binding

    def _batch_binding(self, record_type: type, context: object | None) -> CodecBinding:
        spec = _record_spec(record_type, context)
        schema_text = spec.schema_text('batch')
        cached = self._batch_cache.get((record_type, 'batch', schema_text))
        if cached is not None:
            return cached
        schema = spec.arrow_schema()
        item_binding = self._single_binding(record_type, context)
        codec_ref = _codec_ref_for_schema(schema_text, buffer_view=True)

        def serialize(values: list[object]) -> bytes:
            if not isinstance(values, list):
                raise TypeError(
                    f'expected list[{record_type.__name__}], got {type(values).__name__}',
                )
            for value in values:
                if not isinstance(value, record_type):
                    raise TypeError(
                        f'expected list[{record_type.__name__}], got item '
                        f'{type(value).__name__}',
                    )
            table = pa.Table.from_pylist([_record_to_row(value) for value in values], schema=schema)
            return _table_to_ipc(table)

        def deserialize(data: memoryview | bytes) -> list[object]:
            return [
                record_type(**row)
                for row in _ipc_to_rows(data)
            ]

        def from_buffer(data: memoryview) -> ArrowBatchView:
            return ArrowBatchView(record_type, _ipc_to_table(data), data)

        transferable = _make_transferable(
            _generated_class_name(record_type, 'BatchTransferable', codec_ref),
            codec_ref,
            serialize,
            deserialize,
            from_buffer=from_buffer,
            item_codec_ref=item_binding.codec_ref,
            schema_text=schema_text,
            auto_input_hold=False,
        )
        binding = CodecBinding(transferable=transferable, codec_ref=codec_ref)
        self._batch_cache[(record_type, 'batch', schema_text)] = binding
        return binding

    def _batch_view_binding(self, record_type: type, context: object | None) -> CodecBinding:
        spec = _record_spec(record_type, context)
        schema_text = spec.schema_text('batch')
        cached = self._batch_view_cache.get((record_type, 'batch', schema_text))
        if cached is not None:
            return cached
        schema = spec.arrow_schema()
        codec_ref = _codec_ref_for_schema(schema_text, buffer_view=True)

        def serialize(values: object) -> bytes:
            if isinstance(values, ArrowBatchView):
                if values.record_type is not record_type:
                    raise TypeError(
                        f'expected ArrowBatchView[{record_type.__name__}], got '
                        f'ArrowBatchView[{values.record_type.__name__}]',
                    )
                return _table_to_ipc(values.to_table())
            if not isinstance(values, list):
                raise TypeError(
                    f'expected list[{record_type.__name__}] or ArrowBatchView, '
                    f'got {type(values).__name__}',
                )
            for value in values:
                if not isinstance(value, record_type):
                    raise TypeError(
                        f'expected list[{record_type.__name__}], got item '
                        f'{type(value).__name__}',
                    )
            table = pa.Table.from_pylist([_record_to_row(value) for value in values], schema=schema)
            return _table_to_ipc(table)

        def deserialize(data: memoryview | bytes) -> ArrowBatchView:
            owned = bytes(data)
            return ArrowBatchView(record_type, _ipc_to_table(owned), owned)

        def from_buffer(data: memoryview) -> ArrowBatchView:
            return ArrowBatchView(record_type, _ipc_to_table(data), data)

        transferable = _make_transferable(
            _generated_class_name(record_type, 'BatchViewTransferable', codec_ref),
            codec_ref,
            serialize,
            deserialize,
            from_buffer=from_buffer,
            schema_text=schema_text,
            auto_input_hold=False,
        )
        binding = CodecBinding(transferable=transferable, codec_ref=codec_ref)
        self._batch_view_cache[(record_type, 'batch', schema_text)] = binding
        return binding


_DEFAULT_PROVIDER = ArrowCodecProvider()


def _make_transferable(
    class_name: str,
    codec_ref: CodecRef,
    serialize,
    deserialize,
    *,
    from_buffer=None,
    item_codec_ref: CodecRef | None = None,
    schema_text: str,
    auto_input_hold: bool = True,
) -> type:
    attrs: dict[str, object] = {
        '__module__': _GENERATED_MODULE,
        '__cc_codec_ref__': codec_ref,
        '_c2_schema_text': schema_text,
        '_c2_auto_input_hold': auto_input_hold,
        'serialize': serialize,
        'deserialize': deserialize,
    }
    if from_buffer is not None:
        attrs['from_buffer'] = from_buffer
    if item_codec_ref is not None:
        attrs['_c2_item_codec_ref'] = item_codec_ref
    return type(class_name, (Transferable,), attrs)


def _generated_class_name(record_type: type, suffix: str, codec_ref: CodecRef) -> str:
    digest = codec_ref.schema_sha256[:12] if codec_ref.schema_sha256 is not None else 'unspecified'
    return f'Arrow{record_type.__name__}{suffix}_{digest}'


def _codec_ref_for_schema(schema_text: str, *, buffer_view: bool = False) -> CodecRef:
    return CodecRef.from_schema(
        id=ARROW_IPC_CODEC_ID,
        version=ARROW_IPC_CODEC_VERSION,
        schema=ARROW_IPC_SCHEMA,
        schema_text=schema_text,
        capabilities=(
            ARROW_IPC_BUFFER_VIEW_CAPABILITIES
            if buffer_view
            else ARROW_IPC_BYTES_CAPABILITIES
        ),
        media_type=ARROW_IPC_MEDIA_TYPE,
    )


def _is_arrow_record(value: object) -> bool:
    return isinstance(value, type) and hasattr(value, _RECORD_MARKER)


def _record_spec(record_type: type, context: object | None = None) -> _ArrowRecordSpec:
    if not dataclasses.is_dataclass(record_type):
        raise TypeError(f'{record_type.__name__} must be a dataclass.')
    options = getattr(record_type, _RECORD_MARKER, None)
    if not isinstance(options, ArrowRecordOptions):
        raise TypeError(f'{record_type.__name__} is not marked with @arrow.record.')
    record_name = options.name or record_type.__name__
    crm_namespace = _context_str(context, 'crm_namespace')
    crm_name = _context_str(context, 'crm_name')
    crm_version = _context_str(context, 'crm_version')
    crm_values = (crm_namespace, crm_name, crm_version)
    if any(value is not None for value in crm_values) and not all(
        value is not None for value in crm_values
    ):
        raise TypeError(
            'Arrow codec context must include crm_namespace, crm_name, and '
            'crm_version together.',
        )
    if options.schema_id is not None:
        schema_id = options.schema_id
    else:
        missing = [
            label
            for label, value in (
                ('crm_namespace', crm_namespace),
                ('crm_name', crm_name),
                ('crm_version', crm_version),
            )
            if value is None
        ]
        if missing:
            joined = ', '.join(missing)
            raise TypeError(
                f'@arrow.record type {record_type.__name__} requires CRM codec context '
                f'to derive a default schema identity; missing {joined}. '
                'Use it from a @cc.crm method or pass schema_id=... for an explicit '
                'shared payload identity.',
            )
        schema_id = f'{crm_namespace}.{crm_name}.{record_name}.arrow-ipc.v{crm_version}'
    try:
        type_hints = get_type_hints(record_type, include_extras=True)
    except (NameError, TypeError) as exc:
        raise TypeError(
            f'Failed to resolve Arrow record annotations for {record_type.__name__}: {exc}',
        ) from exc
    fields = []
    for field in dataclasses.fields(record_type):
        annotation = type_hints.get(field.name)
        if annotation is None:
            raise TypeError(
                f'Arrow record field {record_type.__name__}.{field.name} '
                'is missing a type annotation.',
            )
        arrow_type, type_text, nullable = _arrow_type_for_annotation(
            annotation,
            f'{record_type.__name__}.{field.name}',
        )
        fields.append(_ArrowFieldSpec(field.name, arrow_type, type_text, nullable))
    return _ArrowRecordSpec(
        record_type=record_type,
        record_name=record_name,
        schema_id=schema_id,
        crm_namespace=crm_namespace,
        crm_name=crm_name,
        crm_version=crm_version,
        fields=tuple(fields),
    )


def _context_str(context: object | None, key: str) -> str | None:
    if not isinstance(context, Mapping):
        return None
    value = context.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value:
        raise TypeError(f'Arrow codec context {key} must be a non-empty string.')
    return value


def _arrow_type_for_annotation(annotation: object, path: str) -> tuple[object, str, bool]:
    if isinstance(annotation, (str, ForwardRef)):
        raise TypeError(f'Unsupported Arrow record annotation at {path}: unresolved reference.')
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin is dataclasses.InitVar:
        raise TypeError(f'Unsupported Arrow record annotation at {path}: InitVar.')
    if origin is Union or origin is types.UnionType:
        non_none = [arg for arg in args if arg is not type(None)]
        if len(non_none) == 1 and len(non_none) != len(args):
            arrow_type, type_text, _nullable = _arrow_type_for_annotation(non_none[0], path)
            return arrow_type, type_text, True
        raise TypeError(f'Unsupported Arrow record annotation at {path}: union.')
    if annotation is bool:
        return pa.bool_(), 'bool', False
    if annotation is int:
        return pa.int64(), 'int64', False
    if annotation is float:
        return pa.float64(), 'float64', False
    if annotation is str:
        return pa.string(), 'string', False
    if annotation is bytes:
        return pa.binary(), 'binary', False
    if origin is list:
        if len(args) != 1:
            raise TypeError(f'Unsupported Arrow record annotation at {path}: bare list.')
        item_type, item_text, item_nullable = _arrow_type_for_annotation(args[0], f'{path}[]')
        if item_nullable:
            item_field = pa.field('item', item_type, nullable=True)
            return pa.list_(item_field), f'list<{item_text}?>', False
        return pa.list_(item_type), f'list<{item_text}>', False
    name = getattr(annotation, '__name__', repr(annotation))
    raise TypeError(f'Unsupported Arrow record annotation at {path}: {name}.')


def _record_to_row(value: object) -> dict[str, object]:
    return {
        field.name: getattr(value, field.name)
        for field in dataclasses.fields(value)
    }


def _table_to_ipc(table: object) -> bytes:
    sink = pa.BufferOutputStream()
    with pa.ipc.new_stream(sink, table.schema) as writer:
        writer.write_table(table)
    return sink.getvalue().to_pybytes()


def _ipc_to_rows(data: memoryview | bytes) -> list[dict[str, object]]:
    return _ipc_to_table(data).to_pylist()


def _ipc_to_table(data: memoryview | bytes) -> object:
    buffer = pa.py_buffer(data)
    with pa.ipc.open_stream(buffer) as reader:
        return reader.read_all()


__all__ = [
    'ARROW_IPC_CAPABILITIES',
    'ARROW_IPC_BUFFER_VIEW_CAPABILITIES',
    'ARROW_IPC_BYTES_CAPABILITIES',
    'ARROW_IPC_CODEC_ID',
    'ARROW_IPC_CODEC_VERSION',
    'ARROW_IPC_MEDIA_TYPE',
    'ARROW_IPC_SCHEMA',
    'ArrowBatchView',
    'ArrowCodecProvider',
    'ArrowRecordOptions',
    'Batch',
    'record',
]
