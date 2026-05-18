from __future__ import annotations

import dataclasses
from dataclasses import dataclass
from typing import Any

from c_two.crm.codec import CodecBinding, CodecRef, use_codec
from c_two.crm.transferable import Transferable

CUSTOM_SCHEMA = 'c-two.custom.schema.v1'
CUSTOM_CAPABILITY_BYTES = 'bytes'
CUSTOM_CAPABILITY_BUFFER_VIEW = 'buffer-view'

_RECORD_MARKER = '__c_two_custom_record__'
_GENERATED_MODULE = 'c_two.providers.custom.generated'


@dataclass(frozen=True)
class CustomRecordOptions:
    codec_ref: CodecRef
    schema_text: str | None = None


class CustomCodecProvider:
    """C-Two codec provider for application-owned custom payload codecs."""

    def __init__(self) -> None:
        self._cache: dict[tuple[type, CodecRef], CodecBinding] = {}

    def candidates_for_type(
        self,
        annotation: object,
        context: object | None = None,
    ) -> CodecBinding | None:
        if not _is_custom_record(annotation):
            return None
        options = getattr(annotation, _RECORD_MARKER)
        cached = self._cache.get((annotation, options.codec_ref))
        if cached is not None:
            return cached
        transferable = _make_transferable(annotation, options)
        binding = CodecBinding(transferable=transferable, codec_ref=options.codec_ref)
        self._cache[(annotation, options.codec_ref)] = binding
        return binding


_DEFAULT_PROVIDER = CustomCodecProvider()


def record(
    cls: type | None = None,
    *,
    id: str,
    version: str,
    schema: str = CUSTOM_SCHEMA,
    schema_text: str | None = None,
    schema_sha256: str | None = None,
    media_type: str | None = None,
    capabilities: tuple[str, ...] = (),
):
    """Mark a payload class as using an application-owned custom codec."""

    if not isinstance(schema_text, (str, type(None))):
        raise TypeError('schema_text must be a string when provided.')
    if schema_text is not None and not schema_text:
        raise ValueError('schema_text cannot be empty.')
    if schema_text is not None and schema_sha256 is not None:
        raise ValueError('Provide schema_text or schema_sha256, not both.')
    if schema_text is None and schema_sha256 is None:
        raise ValueError('custom.record requires schema_text or schema_sha256.')

    def wrap(target: type) -> type:
        if not isinstance(target, type):
            raise TypeError('@custom.record must decorate a class.')
        _ensure_hook(target, 'serialize')
        _ensure_hook(target, 'deserialize')
        record_cls = target if dataclasses.is_dataclass(target) else dataclasses.dataclass(target)
        codec_ref = _codec_ref_for_record(
            record_cls,
            id=id,
            version=version,
            schema=schema,
            schema_text=schema_text,
            schema_sha256=schema_sha256,
            media_type=media_type,
            capabilities=capabilities,
        )
        setattr(
            record_cls,
            _RECORD_MARKER,
            CustomRecordOptions(codec_ref=codec_ref, schema_text=schema_text),
        )
        use_codec(_DEFAULT_PROVIDER)
        return record_cls

    if cls is not None:
        return wrap(cls)
    return wrap


def _ensure_hook(target: type, name: str) -> None:
    hook = getattr(target, name, None)
    if hook is None or not callable(hook):
        raise TypeError(f'@custom.record class {target.__name__} must define {name}().')


def _codec_ref_for_record(
    record_type: type,
    *,
    id: str,
    version: str,
    schema: str,
    schema_text: str | None,
    schema_sha256: str | None,
    media_type: str | None,
    capabilities: tuple[str, ...],
) -> CodecRef:
    normalized_capabilities = _capabilities_for_record(record_type, capabilities)
    if schema_text is not None:
        return CodecRef.from_schema(
            id=id,
            version=version,
            schema=schema,
            schema_text=schema_text,
            capabilities=normalized_capabilities,
            media_type=media_type,
            portable=True,
        )
    return CodecRef(
        id=id,
        version=version,
        schema=schema,
        schema_sha256=schema_sha256,
        capabilities=normalized_capabilities,
        media_type=media_type,
        portable=True,
    )


def _capabilities_for_record(record_type: type, requested: tuple[str, ...]) -> tuple[str, ...]:
    values = [CUSTOM_CAPABILITY_BYTES]
    has_from_buffer = hasattr(record_type, 'from_buffer') and callable(
        getattr(record_type, 'from_buffer'),
    )
    for value in requested:
        if value == CUSTOM_CAPABILITY_BUFFER_VIEW and not has_from_buffer:
            raise ValueError(
                'buffer-view capability requires a callable from_buffer() hook.',
            )
        if value not in values:
            values.append(value)
    if has_from_buffer:
        if CUSTOM_CAPABILITY_BUFFER_VIEW not in values:
            values.append(CUSTOM_CAPABILITY_BUFFER_VIEW)
    return tuple(values)


def _make_transferable(record_type: type, options: CustomRecordOptions) -> type:
    codec_ref = options.codec_ref

    def serialize(value: object) -> bytes:
        if not isinstance(value, record_type):
            raise TypeError(
                f'expected {record_type.__name__}, got {type(value).__name__}',
            )
        result = record_type.serialize(value)
        if not isinstance(result, (bytes, bytearray, memoryview)):
            raise TypeError(
                f'{record_type.__name__}.serialize() must return bytes-like data, '
                f'got {type(result).__name__}',
            )
        return bytes(result)

    def deserialize(data: memoryview | bytes) -> object:
        return record_type.deserialize(data)

    attrs: dict[str, Any] = {
        '__module__': _GENERATED_MODULE,
        '__cc_codec_ref__': codec_ref,
        '_c2_schema_text': options.schema_text,
        'serialize': serialize,
        'deserialize': deserialize,
    }
    from_buffer = getattr(record_type, 'from_buffer', None)
    if callable(from_buffer):

        def from_buffer_adapter(data: memoryview) -> object:
            return record_type.from_buffer(data)

        attrs['from_buffer'] = from_buffer_adapter
    return type(_generated_class_name(record_type, codec_ref), (Transferable,), attrs)


def _generated_class_name(record_type: type, codec_ref: CodecRef) -> str:
    digest = codec_ref.schema_sha256[:12] if codec_ref.schema_sha256 is not None else 'unspecified'
    return f'Custom{record_type.__name__}Transferable_{digest}'


def _is_custom_record(value: object) -> bool:
    return isinstance(value, type) and isinstance(
        getattr(value, _RECORD_MARKER, None),
        CustomRecordOptions,
    )


__all__ = [
    'CUSTOM_CAPABILITY_BUFFER_VIEW',
    'CUSTOM_CAPABILITY_BYTES',
    'CUSTOM_SCHEMA',
    'CustomCodecProvider',
    'CustomRecordOptions',
    'record',
]
