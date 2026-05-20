from __future__ import annotations

import inspect
from collections.abc import Callable, Iterable, Iterator, Mapping, MutableMapping, MutableSequence, Sequence
from typing import Any, get_args, get_origin, get_type_hints

from fastdb4py.registry import get_schema, is_feature
from fastdb4py.type import (
    Array,
    Batch,
    BYTES,
    BOOL,
    F32,
    F64,
    I32,
    STR,
    U16,
    U16N,
    U32,
    U8,
    U8N,
    WSTR,
    coerce_bool_scalar,
)

_MISSING = object()
_BOOL_TYPES = {BOOL}
_INT_TYPES = {I32, U8, U16, U32, U8N, U16N}
_FLOAT_TYPES = {F32, F64}
_STR_TYPES = {STR, WSTR}
_BYTES_TYPES = {BYTES}
_BATCH_SCALAR_TYPES = (str, bytes, bytearray, memoryview)


def coerce_scalar(value: Any, converter: Callable[[Any], Any] | None = None) -> Any:
    if converter is None:
        return value
    return converter(value)


def coerce_array(
    values: Any,
    converter: Callable[[Any], Any] | None = None,
) -> list[Any]:
    if isinstance(values, (*_BATCH_SCALAR_TYPES, Mapping)) or not isinstance(values, Iterable):
        raise TypeError('fastdb bridge array values must be an iterable of items, not a scalar, bytes-like value, or mapping.')
    if converter is None:
        return list(values)
    return [converter(value) for value in values]


def object_feature_mapper(
    feature_type: type,
    *,
    field_map: Mapping[str, str | Callable[[Any], Any]] | None = None,
    converters: Mapping[str, Callable[[Any], Any]] | None = None,
    defaults: Mapping[str, Any] | None = None,
) -> Callable[[Any], Any]:
    _ensure_feature(feature_type)
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    fields = dict(field_map or {})
    field_converters = dict(converters or {})
    field_defaults = dict(defaults or {})
    _validate_field_mapping(feature_type, field_names, fields, 'field_map')
    _validate_field_mapping(feature_type, field_names, field_converters, 'converters')
    _validate_field_mapping(feature_type, field_names, field_defaults, 'defaults')
    for field_name, selector in fields.items():
        if not isinstance(selector, str) and not callable(selector):
            raise TypeError(f'field_map[{field_name!r}] must be a source field name or callable.')
    for field_name, converter in field_converters.items():
        if not callable(converter):
            raise TypeError(f'converters[{field_name!r}] must be callable.')

    def map_one(source: Any) -> Any:
        values: dict[str, Any] = {}
        for field_name in field_names:
            value = _read_field(source, fields.get(field_name, field_name), field_name)
            if value is _MISSING:
                value = _default_value(field_defaults, field_name)
            if value is _MISSING:
                raise KeyError(f'missing field {field_name!r} for fastdb feature {feature_type.__name__}.')
            converter = field_converters.get(field_name)
            if converter is not None:
                value = converter(value)
            values[field_name] = value
        return feature_type(**values)

    return map_one


def feature_from_object(
    feature_type: type,
    source: Any,
    *,
    field_map: Mapping[str, str | Callable[[Any], Any]] | None = None,
    converters: Mapping[str, Callable[[Any], Any]] | None = None,
    defaults: Mapping[str, Any] | None = None,
) -> Any:
    return object_feature_mapper(
        feature_type,
        field_map=field_map,
        converters=converters,
        defaults=defaults,
    )(source)


def object_batch_mapper(
    feature_type: type,
    *,
    field_map: Mapping[str, str | Callable[[Any], Any]] | None = None,
    converters: Mapping[str, Callable[[Any], Any]] | None = None,
    defaults: Mapping[str, Any] | None = None,
) -> Callable[[Any], list[Any]]:
    map_one = object_feature_mapper(
        feature_type,
        field_map=field_map,
        converters=converters,
        defaults=defaults,
    )

    def map_many(sources: Iterable[Any]) -> list[Any]:
        records = _table_like_records(sources)
        if records is not None:
            if _is_malformed_batch_source(records):
                raise TypeError('fastdb bridge object batch sources must be an iterable of row objects, not a scalar string/bytes value or a single mapping.')
            return [map_one(source) for source in records]
        if _is_malformed_batch_source(sources):
            raise TypeError('fastdb bridge object batch sources must be an iterable of row objects, not a scalar string/bytes value or a single mapping.')
        return [map_one(source) for source in sources]

    return map_many


def batch_from_objects(
    feature_type: type,
    sources: Any,
    *,
    field_map: Mapping[str, str | Callable[[Any], Any]] | None = None,
    converters: Mapping[str, Callable[[Any], Any]] | None = None,
    defaults: Mapping[str, Any] | None = None,
) -> list[Any]:
    return object_batch_mapper(
        feature_type,
        field_map=field_map,
        converters=converters,
        defaults=defaults,
    )(sources)


def batch_from_columns(
    feature_type: type,
    *,
    converters: Mapping[str, Callable[[Any], Any]] | None = None,
    defaults: Mapping[str, Any] | None = None,
    **columns: Iterable[Any],
) -> list[Any]:
    _ensure_feature(feature_type)
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    field_converters = dict(converters or {})
    field_defaults = dict(defaults or {})
    _validate_field_mapping(feature_type, field_names, columns, 'columns')
    _validate_field_mapping(feature_type, field_names, field_converters, 'converters')
    _validate_field_mapping(feature_type, field_names, field_defaults, 'defaults')
    for field_name, converter in field_converters.items():
        if not callable(converter):
            raise TypeError(f'converters[{field_name!r}] must be callable.')
    materialized: dict[str, list[Any]] = {}
    missing_with_defaults: dict[str, Any] = {}

    for field_name in field_names:
        if field_name in columns:
            materialized[field_name] = _materialize_column_values(field_name, columns[field_name])
            continue
        default = _default_value(field_defaults, field_name)
        if default is _MISSING:
            raise KeyError(f'missing column {field_name!r} for fastdb feature {feature_type.__name__}.')
        missing_with_defaults[field_name] = default

    lengths = {field_name: len(values) for field_name, values in materialized.items()}
    unique_lengths = set(lengths.values())
    if len(unique_lengths) > 1:
        detail = ', '.join(f'{name}={length}' for name, length in sorted(lengths.items()))
        raise ValueError(f'fastdb bridge columns must have the same length: {detail}.')
    row_count = next(iter(unique_lengths), 1 if missing_with_defaults else 0)
    for field_name, default in missing_with_defaults.items():
        materialized[field_name] = [default for _ in range(row_count)]

    rows: list[Any] = []
    for index in range(row_count):
        values = {}
        for field_name in field_names:
            value = materialized[field_name][index]
            converter = field_converters.get(field_name)
            if converter is not None:
                value = converter(value)
            values[field_name] = value
        rows.append(feature_type(**values))
    return rows


def _materialize_column_values(field_name: str, values: Iterable[Any]) -> list[Any]:
    if isinstance(values, (*_BATCH_SCALAR_TYPES, Mapping)) or not isinstance(values, Iterable):
        raise TypeError(f'fastdb bridge column values for {field_name!r} must be an iterable of items, not a scalar or mapping value.')
    return list(values)


def _is_malformed_batch_source(value: Any) -> bool:
    return isinstance(value, (*_BATCH_SCALAR_TYPES, Mapping)) or not isinstance(value, Iterable)


def _table_like_records(value: Any) -> Iterable[Any] | None:
    if isinstance(value, (*_BATCH_SCALAR_TYPES, Mapping)):
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


def _batch_input_records(value: Any, method_name: str, feature_type: type) -> Iterable[Any]:
    records = _table_like_records(value)
    if records is not None:
        if _is_malformed_batch_source(records):
            raise TypeError(f'{method_name} bridge expected an iterable Batch[{feature_type.__name__}] input, got table-like {type(value).__name__} records.')
        return records
    if _is_malformed_batch_source(value):
        raise TypeError(f'{method_name} bridge expected an iterable Batch[{feature_type.__name__}] input, got {type(value).__name__}.')
    return value


def derive_bridge_hooks(
    crm_class: type,
    resource: object,
    *,
    methods: Iterable[str] | None = None,
) -> dict[str, dict[str, Callable[..., Any]]]:
    hooks: dict[str, dict[str, Callable[..., Any]]] = {}
    for method_name in _rpc_method_names(crm_class, methods):
        if not hasattr(resource, method_name):
            continue
        crm_method = inspect.unwrap(getattr(crm_class, method_name))
        resource_method = inspect.unwrap(getattr(resource, method_name))
        method_hooks: dict[str, Callable[..., Any]] = {}
        input_hook = _derive_input_hook(method_name, crm_method, resource_method)
        if input_hook is not None:
            method_hooks['input'] = input_hook
        output_hook = _derive_output_hook(method_name, crm_method, resource_method)
        if output_hook is not None:
            method_hooks['output'] = output_hook
        if method_hooks:
            hooks[method_name] = method_hooks
    return hooks


def build_c_two_bridge(cc_module: object, hooks: Mapping[str, Mapping[str, Callable[..., Any]]]) -> dict[str, Any]:
    bridge_factory = getattr(cc_module, 'bridge')
    return {
        method_name: bridge_factory(
            input=method_hooks.get('input'),
            output=method_hooks.get('output'),
        )
        for method_name, method_hooks in hooks.items()
    }


def derive_c_two_bridge(
    cc_module: object,
    crm_class: type,
    resource: object,
    *,
    methods: Iterable[str] | None = None,
) -> dict[str, Any]:
    return build_c_two_bridge(
        cc_module,
        derive_bridge_hooks(crm_class, resource, methods=methods),
    )


def _ensure_feature(feature_type: type) -> None:
    if not is_feature(feature_type):
        raise TypeError(f'{feature_type!r} is not a fastdb @feature class.')


def _validate_field_mapping(
    feature_type: type,
    field_names: tuple[str, ...],
    values: Mapping[str, Any],
    label: str,
) -> None:
    unknown = set(values) - set(field_names)
    if unknown:
        names = ', '.join(sorted(unknown))
        raise KeyError(f'{label} references unknown field(s) for {feature_type.__name__}: {names}.')


def _read_field(source: Any, selector: str | Callable[[Any], Any], field_name: str) -> Any:
    if callable(selector):
        return selector(source)
    if isinstance(source, Mapping):
        return source.get(selector, _MISSING)
    return getattr(source, selector, _MISSING)


def _default_value(defaults: Mapping[str, Any], field_name: str) -> Any:
    if field_name not in defaults:
        return _MISSING
    value = defaults[field_name]
    if callable(value):
        return value()
    return value


def _rpc_method_names(crm_class: type, methods: Iterable[str] | None) -> tuple[str, ...]:
    if methods is not None:
        return tuple(methods)
    explicit = getattr(crm_class, '__cc_rpc_methods__', None)
    if explicit is not None:
        return tuple(explicit)
    shutdown_method = _shutdown_method_name(crm_class)
    return tuple(sorted(
        name
        for name, value in inspect.getmembers(crm_class, predicate=inspect.isfunction)
        if not name.startswith('_') and name != shutdown_method
    ))


def _shutdown_method_name(crm_class: type) -> str | None:
    for name, value in inspect.getmembers(crm_class, predicate=inspect.isfunction):
        if getattr(value, '__cc_on_shutdown__', False):
            return name
    return None


def _derive_input_hook(
    method_name: str,
    crm_method: Callable[..., Any],
    resource_method: Callable[..., Any],
) -> Callable[..., tuple[Any, ...]] | None:
    crm_params, crm_hints = _method_parameters_and_hints(crm_method, method_name)
    resource_params, resource_hints = _method_parameters_and_hints(resource_method, method_name)
    if len(crm_params) != len(resource_params):
        expanded_hook = _derive_expanded_input_hook(
            method_name,
            crm_params,
            crm_hints,
            resource_params,
            resource_hints,
        )
        if expanded_hook is not None:
            return expanded_hook
        raise TypeError(
            f'{method_name} cannot derive fastdb bridge input: CRM has {len(crm_params)} parameters but resource has {len(resource_params)}.',
        )
    converters: list[Callable[[Any], Any] | None] = []
    for crm_param, resource_param in zip(crm_params, resource_params):
        crm_annotation = crm_hints[crm_param.name]
        resource_annotation = resource_hints.get(resource_param.name, inspect.Signature.empty)
        converters.append(_input_converter(method_name, crm_annotation, resource_annotation))
    if all(converter is None for converter in converters):
        return None

    def input_hook(*values: Any) -> tuple[Any, ...]:
        if len(values) != len(converters):
            raise TypeError(f'{method_name} bridge expected {len(converters)} input values, got {len(values)}.')
        converted = [
            converter(value) if converter is not None else value
            for converter, value in zip(converters, values)
        ]
        return tuple(converted)

    return input_hook


def _derive_expanded_input_hook(
    method_name: str,
    crm_params: list[inspect.Parameter],
    crm_hints: Mapping[str, Any],
    resource_params: list[inspect.Parameter],
    resource_hints: Mapping[str, Any],
) -> Callable[..., tuple[Any, ...]] | None:
    if len(crm_params) != 1:
        return None
    crm_annotation = crm_hints[crm_params[0].name]
    if is_feature(crm_annotation):
        return _feature_split_fields_input_mapper(
            method_name,
            crm_annotation,
            resource_params,
            resource_hints,
        )
    origin = get_origin(crm_annotation)
    args = get_args(crm_annotation)
    if origin is not Batch or len(args) != 1 or not is_feature(args[0]):
        return None
    return _batch_split_columns_input_mapper(
        method_name,
        args[0],
        resource_params,
        resource_hints,
    )


def _derive_output_hook(
    method_name: str,
    crm_method: Callable[..., Any],
    resource_method: Callable[..., Any],
) -> Callable[[Any], Any] | None:
    crm_return = _return_annotation(crm_method, method_name)
    resource_return = _return_annotation(resource_method, method_name, required=False)
    return _output_converter(method_name, crm_return, resource_return)


def _method_parameters_and_hints(
    method: Callable[..., Any],
    method_name: str,
) -> tuple[list[inspect.Parameter], dict[str, Any]]:
    sig = inspect.signature(method)
    hints = get_type_hints(method)
    params = [
        param
        for index, param in enumerate(sig.parameters.values())
        if not (index == 0 and param.name in {'self', 'cls'})
    ]
    for param in params:
        if param.name not in hints:
            raise TypeError(f'{method_name}.{param.name} is missing a type annotation for automatic fastdb bridge derivation.')
    return params, hints


def _return_annotation(
    method: Callable[..., Any],
    method_name: str,
    *,
    required: bool = True,
) -> Any:
    hints = get_type_hints(method)
    if 'return' not in hints:
        if required:
            raise TypeError(f'{method_name} is missing a return annotation for automatic fastdb bridge derivation.')
        return inspect.Signature.empty
    return hints['return']


def _input_converter(method_name: str, crm_annotation: Any, resource_annotation: Any) -> Callable[[Any], Any] | None:
    if _same_fastdb_abi_annotation(crm_annotation, resource_annotation):
        return None
    origin = get_origin(crm_annotation)
    if origin is Array:
        return _array_input_mapper(method_name, crm_annotation, resource_annotation)
    if origin is Batch:
        args = get_args(crm_annotation)
        if len(args) != 1 or not is_feature(args[0]):
            raise TypeError(f'{method_name} cannot derive input bridge for unsupported Batch annotation {crm_annotation!r}.')
        target_origin = get_origin(resource_annotation)
        if target_origin is tuple:
            variadic_item = _variadic_tuple_item_annotation(resource_annotation)
            if variadic_item is not inspect.Signature.empty:
                return _batch_variadic_tuple_input_mapper(method_name, args[0], variadic_item)
            return _batch_tuple_columns_input_mapper(method_name, args[0], resource_annotation)
        if _is_mapping_annotation(resource_annotation):
            if not _is_dict_column_annotation(resource_annotation):
                raise TypeError(
                    f'{method_name} cannot derive Batch[{args[0].__name__}] input to {resource_annotation!r}; use dict[str, list[...]] or Mapping[str, Sequence[...]] column inputs, or write an explicit bridge.',
                )
            _ensure_dict_column_scalar_items(method_name, args[0], resource_annotation, direction='input to')
            return _batch_dict_columns_input_mapper(method_name, args[0], resource_annotation)
        if _is_sequence_annotation(resource_annotation):
            return _batch_sequence_input_mapper(method_name, args[0], resource_annotation)
        table_mapper = _batch_table_object_input_mapper(method_name, args[0], resource_annotation)
        if table_mapper is not None:
            return table_mapper
        return None
    if is_feature(crm_annotation):
        if _is_mapping_annotation(resource_annotation):
            _ensure_mapping_string_keys(method_name, resource_annotation, direction=f'{crm_annotation.__name__} input to')
            return _feature_dict_input_mapper(crm_annotation)
        if _is_tuple_annotation(resource_annotation):
            return _feature_tuple_input_mapper(method_name, crm_annotation, resource_annotation)
        return _feature_object_input_mapper(method_name, crm_annotation, resource_annotation)
    if _python_converter_for_annotation(crm_annotation) is not None:
        _ensure_scalar_resource_annotation(method_name, resource_annotation, direction='input to')
        converter = _python_converter_for_annotation(resource_annotation) or _python_converter_for_annotation(crm_annotation)
        return lambda value: coerce_scalar(value, converter)
    return None


def _array_input_mapper(
    method_name: str,
    crm_annotation: Any,
    resource_annotation: Any,
) -> Callable[[Any], Any]:
    args = get_args(crm_annotation)
    variadic_item = _variadic_tuple_item_annotation(resource_annotation)
    if variadic_item is not inspect.Signature.empty:
        item_converter = _python_converter_for_annotation(variadic_item)
        if item_converter is None and variadic_item not in {Any, object}:
            raise TypeError(
                f'{method_name} cannot derive Array input to tuple item {variadic_item!r}; '
                'use scalar tuple item annotations or write an explicit bridge.',
            )
        if item_converter is None and args:
            item_converter = _python_converter_for_annotation(args[0])
        return lambda value: tuple(coerce_array(value, item_converter))

    if _is_bare_tuple_annotation(resource_annotation):
        item_converter = _python_converter_for_annotation(args[0]) if args else None
        return lambda value: tuple(coerce_array(value, item_converter))

    if get_origin(resource_annotation) is tuple:
        raise TypeError(
            f'{method_name} cannot derive Array input to fixed tuple resource annotation {resource_annotation!r}; '
            'use tuple[Scalar, ...], bare tuple, list[...] or Sequence[...] values, or write an explicit bridge.',
        )

    if not _array_accepts_sequence_resource(resource_annotation):
        raise TypeError(
            f'{method_name} cannot derive Array input to resource annotation {resource_annotation!r}; '
            'use list[Scalar], Sequence[Scalar], Iterable[Scalar], Iterator[Scalar], tuple[Scalar, ...], bare tuple, or write an explicit bridge.',
        )
    target_annotation = _list_item_annotation(resource_annotation)
    item_converter = None
    if target_annotation not in {inspect.Signature.empty, Any, object}:
        item_converter = _python_converter_for_annotation(target_annotation)
        if item_converter is None:
            raise TypeError(
                f'{method_name} cannot derive Array input to sequence item {target_annotation!r}; '
                'use scalar item annotations or write an explicit bridge.',
            )
    if item_converter is None and args:
        item_converter = _python_converter_for_annotation(args[0])
    if _is_iterator_annotation(resource_annotation):
        return lambda value: iter(coerce_array(value, item_converter))
    return lambda value: coerce_array(value, item_converter)


def _feature_dict_input_mapper(
    feature_type: type,
) -> Callable[[Any], dict[str, Any]]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    converters = _feature_converters(feature_type)

    def input_hook(value: Any) -> dict[str, Any]:
        mapped: dict[str, Any] = {}
        for field_name in field_names:
            item = _read_field(value, field_name, field_name)
            if item is _MISSING:
                raise KeyError(f'missing field {field_name!r} for fastdb feature {feature_type.__name__} input.')
            converter = converters.get(field_name)
            if converter is not None:
                item = converter(item)
            mapped[field_name] = item
        return mapped

    return input_hook


def _feature_tuple_input_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], tuple[Any, ...]]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    feature_converters = _feature_converters(feature_type)
    converters: list[Callable[[Any], Any] | None] = []
    if _is_bare_tuple_annotation(resource_annotation):
        converters = [feature_converters.get(field_name) for field_name in field_names]
    else:
        args = get_args(resource_annotation)
        if len(args) == 2 and args[1] is Ellipsis:
            raise TypeError(f'{method_name} cannot derive {feature_type.__name__} input to variadic tuple resource annotation.')
        if len(args) != len(field_names):
            raise TypeError(
                f'{method_name} cannot derive {feature_type.__name__} input to tuple with {len(args)} items; expected {len(field_names)} feature fields.',
            )
        for field_name, item_annotation in zip(field_names, args):
            converter = _python_converter_for_annotation(item_annotation)
            if converter is None:
                raise TypeError(
                    f'{method_name} cannot derive {feature_type.__name__} input to tuple item {field_name!r} annotated as {item_annotation!r}; use scalar tuple item annotations or write an explicit bridge.',
                )
            converters.append(converter)

    def input_hook(value: Any) -> tuple[Any, ...]:
        items: list[Any] = []
        for field_name, converter in zip(field_names, converters):
            item = _read_field(value, field_name, field_name)
            if item is _MISSING:
                raise KeyError(f'missing field {field_name!r} for fastdb feature {feature_type.__name__} input.')
            if converter is not None:
                item = converter(item)
            items.append(item)
        return tuple(items)

    return input_hook


def _batch_dict_list_input_mapper(
    method_name: str,
    feature_type: type,
) -> Callable[[Any], list[dict[str, Any]]]:
    map_one = _feature_dict_input_mapper(feature_type)

    def input_hook(value: Any) -> list[dict[str, Any]]:
        return [map_one(row) for row in _batch_input_records(value, method_name, feature_type)]

    return input_hook


def _batch_tuple_row_input_mapper(
    method_name: str,
    feature_type: type,
    row_annotation: Any,
) -> Callable[[Any], list[tuple[Any, ...]]]:
    field_names, converters = _tuple_row_item_converters(
        method_name,
        feature_type,
        row_annotation,
        direction='input to',
    )

    def input_hook(value: Any) -> list[tuple[Any, ...]]:
        rows: list[tuple[Any, ...]] = []
        for source in _batch_input_records(value, method_name, feature_type):
            items: list[Any] = []
            for field_name, converter in zip(field_names, converters):
                item = _read_field(source, field_name, field_name)
                if item is _MISSING:
                    raise KeyError(f'missing field {field_name!r} for fastdb Batch[{feature_type.__name__}] input.')
                items.append(converter(item))
            rows.append(tuple(items))
        return rows

    return input_hook


def _batch_variadic_tuple_input_mapper(
    method_name: str,
    feature_type: type,
    item_annotation: Any,
) -> Callable[[Any], tuple[Any, ...]]:
    if _is_mapping_annotation(item_annotation):
        _ensure_mapping_string_keys(method_name, item_annotation, direction=f'Batch[{feature_type.__name__}] input to tuple row')
        list_mapper = _batch_dict_list_input_mapper(method_name, feature_type)
    elif get_origin(item_annotation) is tuple:
        list_mapper = _batch_tuple_row_input_mapper(method_name, feature_type, item_annotation)
    else:
        object_mapper = _feature_object_input_mapper(method_name, feature_type, item_annotation)
        if object_mapper is not None:
            list_mapper = _batch_object_list_input_mapper(method_name, feature_type, object_mapper)
        elif _same_fastdb_abi_annotation(feature_type, item_annotation):
            list_mapper = _batch_feature_list_input_mapper(method_name, feature_type)
        else:
            raise TypeError(
                f'{method_name} cannot derive Batch[{feature_type.__name__}] input to tuple[{item_annotation!r}, ...]; use tuple row, mapping row, domain object row, fastdb feature row, or write an explicit bridge.',
            )

    def input_hook(value: Any) -> tuple[Any, ...]:
        return tuple(list_mapper(value))

    return input_hook


def _batch_sequence_input_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], Any] | None:
    item_annotation = _list_item_annotation(resource_annotation)
    if item_annotation is inspect.Signature.empty:
        return None
    if item_annotation in {Any, object}:
        return _sequence_input_container_mapper(resource_annotation, list)
    if _is_mapping_annotation(item_annotation):
        _ensure_mapping_string_keys(method_name, item_annotation, direction=f'Batch[{feature_type.__name__}] input to sequence row')
        list_mapper = _batch_dict_list_input_mapper(method_name, feature_type)
        return _sequence_input_container_mapper(resource_annotation, list_mapper)
    if get_origin(item_annotation) is tuple:
        list_mapper = _batch_tuple_row_input_mapper(method_name, feature_type, item_annotation)
        return _sequence_input_container_mapper(resource_annotation, list_mapper)
    object_mapper = _feature_object_input_mapper(method_name, feature_type, item_annotation)
    if object_mapper is not None:
        list_mapper = _batch_object_list_input_mapper(method_name, feature_type, object_mapper)
        return _sequence_input_container_mapper(resource_annotation, list_mapper)
    if _same_fastdb_abi_annotation(feature_type, item_annotation):
        list_mapper = _batch_feature_list_input_mapper(method_name, feature_type)
        return _sequence_input_container_mapper(resource_annotation, list_mapper)
    raise TypeError(
        f'{method_name} cannot derive Batch[{feature_type.__name__}] input to sequence item {item_annotation!r}; use tuple rows, mapping rows, domain object rows, fastdb feature rows, or write an explicit bridge.',
    )


def _sequence_input_container_mapper(
    resource_annotation: Any,
    list_mapper: Callable[[Any], list[Any]],
) -> Callable[[Any], Any]:
    if not _is_iterator_annotation(resource_annotation):
        return list_mapper

    def input_hook(value: Any) -> Iterator[Any]:
        return iter(list_mapper(value))

    return input_hook


def _feature_object_input_mapper(
    method_name: str,
    feature_type: type,
    target_type: Any,
) -> Callable[[Any], Any] | None:
    target_class = _domain_object_type(target_type)
    if target_class is None:
        return None
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    _ensure_keyword_constructible(method_name, feature_type, target_class, field_names)
    feature_converters = _feature_converters(feature_type)
    target_hints = _safe_type_hints(target_class)
    field_converters = {
        field_name: _python_converter_for_annotation(target_hints.get(field_name)) or feature_converters.get(field_name)
        for field_name in field_names
    }

    def input_hook(value: Any) -> Any:
        mapped: dict[str, Any] = {}
        for field_name in field_names:
            item = _read_field(value, field_name, field_name)
            if item is _MISSING:
                raise KeyError(f'missing field {field_name!r} for fastdb feature {feature_type.__name__} input.')
            converter = field_converters.get(field_name)
            if converter is not None:
                item = converter(item)
            mapped[field_name] = item
        try:
            return target_class(**mapped)
        except TypeError as exc:
            raise TypeError(
                f'{method_name} bridge could not construct {target_class.__name__} from fastdb feature fields.',
            ) from exc

    return input_hook


def _feature_split_fields_input_mapper(
    method_name: str,
    feature_type: type,
    resource_params: list[inspect.Parameter],
    resource_hints: Mapping[str, Any],
) -> Callable[[Any], tuple[Any, ...]]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if len(resource_params) != len(field_names):
        raise TypeError(
            f'{method_name} cannot derive {feature_type.__name__} input to {len(resource_params)} resource parameters; expected {len(field_names)} scalar parameters.',
        )
    resource_param_names = tuple(param.name for param in resource_params)
    if set(resource_param_names) != set(field_names):
        expected = ', '.join(field_names)
        actual = ', '.join(resource_param_names)
        raise TypeError(
            f'{method_name} cannot derive {feature_type.__name__} split-field input; resource parameters must match fastdb fields. Expected {expected}; got {actual}.',
        )
    field_converters: dict[str, Callable[[Any], Any]] = {}
    for resource_param in resource_params:
        field_name = resource_param.name
        param_annotation = resource_hints.get(field_name, inspect.Signature.empty)
        converter = _python_converter_for_annotation(param_annotation)
        if converter is None:
            raise TypeError(
                f'{method_name} cannot derive {feature_type.__name__} input to resource parameter {field_name!r} annotated as {param_annotation!r}; use scalar parameters or write an explicit bridge.',
            )
        field_converters[field_name] = converter

    def input_hook(value: Any) -> tuple[Any, ...]:
        fields: dict[str, Any] = {}
        for field_name in field_names:
            item = _read_field(value, field_name, field_name)
            if item is _MISSING:
                raise KeyError(f'missing field {field_name!r} for fastdb feature {feature_type.__name__} input.')
            converter = field_converters[field_name]
            fields[field_name] = converter(item)
        return tuple(fields[param.name] for param in resource_params)

    return input_hook


def _batch_object_list_input_mapper(
    method_name: str,
    feature_type: type,
    map_one: Callable[[Any], Any],
) -> Callable[[Any], list[Any]]:
    def input_hook(value: Any) -> list[Any]:
        return [map_one(row) for row in _batch_input_records(value, method_name, feature_type)]

    return input_hook


def _batch_feature_list_input_mapper(
    method_name: str,
    feature_type: type,
) -> Callable[[Any], list[Any]]:
    map_one = object_feature_mapper(feature_type, converters=_feature_converters(feature_type))

    def input_hook(value: Any) -> list[Any]:
        rows: list[Any] = []
        for row in _batch_input_records(value, method_name, feature_type):
            if isinstance(row, feature_type):
                rows.append(row)
                continue
            rows.append(map_one(row))
        return rows

    return input_hook


def _batch_table_object_input_mapper(
    method_name: str,
    feature_type: type,
    target_type: Any,
) -> Callable[[Any], Any] | None:
    target_class = _domain_object_type(target_type)
    if target_class is None:
        return None
    _ensure_table_constructible(method_name, feature_type, target_class)
    map_one = _feature_dict_input_mapper(feature_type)

    def input_hook(value: Any) -> Any:
        records = [map_one(row) for row in _batch_input_records(value, method_name, feature_type)]
        try:
            return _construct_table_object(target_class, records)
        except TypeError as exc:
            raise TypeError(
                f'{method_name} bridge could not construct {target_class.__name__} from fastdb Batch[{feature_type.__name__}] row records.',
            ) from exc

    return input_hook


def _sequence_column_container(column_annotation: Any, values: list[Any]) -> Any:
    return iter(values) if _is_iterator_annotation(column_annotation) else values


def _batch_tuple_columns_input_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], tuple[Any, ...]]:
    args = get_args(resource_annotation)
    if len(args) == 2 and args[1] is Ellipsis:
        raise TypeError(f'{method_name} cannot derive Batch input to variadic tuple resource annotation.')
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if len(args) != len(field_names):
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] input to tuple with {len(args)} items; expected {len(field_names)} columns.',
    )
    feature_converters = _feature_converters(feature_type)
    column_converters: dict[str, Callable[[Any], Any] | None] = {}
    column_annotations: dict[str, Any] = {}
    for field_name, column_annotation in zip(field_names, args):
        if not _is_sequence_annotation(column_annotation):
            raise TypeError(
                f'{method_name} cannot derive Batch[{feature_type.__name__}] input to tuple column {field_name!r} annotated as {column_annotation!r}; use list[...] columns, Sequence[...] columns, Iterator[...] columns, or write an explicit bridge.',
            )
        _ensure_scalar_column_item_annotation(
            method_name,
            feature_type,
            field_name,
            column_annotation,
            direction='input to',
        )
        item_converter = _python_converter_for_annotation(_list_item_annotation(column_annotation))
        column_converters[field_name] = item_converter or feature_converters.get(field_name)
        column_annotations[field_name] = column_annotation

    def input_hook(value: Any) -> tuple[Any, ...]:
        columns: dict[str, list[Any]] = {field_name: [] for field_name in field_names}
        for row in _batch_input_records(value, method_name, feature_type):
            for field_name in field_names:
                item = _read_field(row, field_name, field_name)
                if item is _MISSING:
                    raise KeyError(f'missing field {field_name!r} for fastdb Batch[{feature_type.__name__}] input.')
                converter = column_converters[field_name]
                if converter is not None:
                    item = converter(item)
                columns[field_name].append(item)
        return tuple(
            _sequence_column_container(column_annotations[field_name], columns[field_name])
            for field_name in field_names
        )

    return input_hook


def _batch_dict_columns_input_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], dict[str, Any]]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    feature_converters = _feature_converters(feature_type)
    args = get_args(resource_annotation)
    value_annotation = args[1] if len(args) == 2 else inspect.Signature.empty

    def input_hook(value: Any) -> dict[str, Any]:
        columns: dict[str, list[Any]] = {field_name: [] for field_name in field_names}
        for row in _batch_input_records(value, method_name, feature_type):
            for field_name in field_names:
                item = _read_field(row, field_name, field_name)
                if item is _MISSING:
                    raise KeyError(f'missing field {field_name!r} for fastdb Batch[{feature_type.__name__}] input.')
                converter = feature_converters.get(field_name)
                if converter is not None:
                    item = converter(item)
                columns[field_name].append(item)
        return {
            field_name: _sequence_column_container(value_annotation, columns[field_name])
            for field_name in field_names
        }

    return input_hook


def _batch_split_columns_input_mapper(
    method_name: str,
    feature_type: type,
    resource_params: list[inspect.Parameter],
    resource_hints: Mapping[str, Any],
) -> Callable[[Any], tuple[Any, ...]]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if len(resource_params) != len(field_names):
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] input to {len(resource_params)} resource parameters; expected {len(field_names)} list[...] parameters.',
        )
    resource_param_names = tuple(param.name for param in resource_params)
    if set(resource_param_names) != set(field_names):
        expected = ', '.join(field_names)
        actual = ', '.join(resource_param_names)
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] split-column input; resource parameters must match fastdb fields. Expected {expected}; got {actual}.',
        )
    feature_converters = _feature_converters(feature_type)
    column_converters: dict[str, Callable[[Any], Any] | None] = {}
    for resource_param in resource_params:
        field_name = resource_param.name
        column_annotation = resource_hints.get(resource_param.name, inspect.Signature.empty)
        if not _is_sequence_annotation(column_annotation):
            raise TypeError(
                f'{method_name} cannot derive Batch[{feature_type.__name__}] input to resource parameter {resource_param.name!r} annotated as {column_annotation!r}; use list[...] parameters, Sequence[...] parameters, Iterator[...] parameters, or write an explicit bridge.',
            )
        _ensure_scalar_column_item_annotation(
            method_name,
            feature_type,
            field_name,
            column_annotation,
            direction='input to',
        )
        item_converter = _python_converter_for_annotation(_list_item_annotation(column_annotation))
        column_converters[field_name] = item_converter or feature_converters.get(field_name)

    def input_hook(value: Any) -> tuple[Any, ...]:
        columns: dict[str, list[Any]] = {field_name: [] for field_name in field_names}
        for row in _batch_input_records(value, method_name, feature_type):
            for field_name in field_names:
                item = _read_field(row, field_name, field_name)
                if item is _MISSING:
                    raise KeyError(f'missing field {field_name!r} for fastdb Batch[{feature_type.__name__}] input.')
                converter = column_converters[field_name]
                if converter is not None:
                    item = converter(item)
                columns[field_name].append(item)
        return tuple(
            _sequence_column_container(resource_hints[param.name], columns[param.name])
            for param in resource_params
        )

    return input_hook


def _output_converter(method_name: str, crm_annotation: Any, resource_annotation: Any) -> Callable[[Any], Any] | None:
    if _same_fastdb_abi_annotation(crm_annotation, resource_annotation):
        return None
    origin = get_origin(crm_annotation)
    if origin is tuple:
        return _tuple_return_output_mapper(method_name, crm_annotation, resource_annotation)
    if origin is Batch:
        args = get_args(crm_annotation)
        if len(args) != 1 or not is_feature(args[0]):
            raise TypeError(f'{method_name} cannot derive output bridge for unsupported Batch annotation {crm_annotation!r}.')
        feature_type = args[0]
        converters = _feature_converters(feature_type)
        if get_origin(resource_annotation) is tuple:
            variadic_item = _variadic_tuple_item_annotation(resource_annotation)
            if variadic_item is not inspect.Signature.empty:
                return _batch_variadic_tuple_output_mapper(method_name, feature_type, variadic_item, converters)
            return _tuple_columns_output_mapper(method_name, feature_type, resource_annotation, converters)
        if _is_mapping_annotation(resource_annotation):
            if not _is_dict_column_annotation(resource_annotation):
                raise TypeError(
                    f'{method_name} cannot derive Batch[{feature_type.__name__}] output from {resource_annotation!r}; use dict[str, list[...]] or Mapping[str, Sequence[...]] column outputs, or write an explicit bridge.',
                )
            _ensure_dict_column_scalar_items(method_name, feature_type, resource_annotation, direction='output from')
            return _dict_columns_output_mapper(method_name, feature_type, converters)
        if _is_sequence_annotation(resource_annotation):
            item_annotation = _list_item_annotation(resource_annotation)
            return _batch_sequence_output_mapper(method_name, feature_type, item_annotation, converters)
        return _batch_object_output_mapper(method_name, feature_type, converters)
    if origin is Array:
        return _array_output_mapper(method_name, crm_annotation, resource_annotation)
    if is_feature(crm_annotation):
        return _feature_output_mapper(method_name, crm_annotation, resource_annotation)
    scalar_converter = _python_converter_for_annotation(crm_annotation)
    if scalar_converter is not None:
        _ensure_scalar_resource_annotation(method_name, resource_annotation, direction='output from')
        return lambda value: coerce_scalar(value, scalar_converter)
    return None


def _array_output_mapper(
    method_name: str,
    crm_annotation: Any,
    resource_annotation: Any,
) -> Callable[[Any], Any]:
    args = get_args(crm_annotation)
    item_converter = _python_converter_for_annotation(args[0]) if args else None
    variadic_item = _variadic_tuple_item_annotation(resource_annotation)
    if variadic_item is not inspect.Signature.empty:
        if _python_converter_for_annotation(variadic_item) is None and variadic_item not in {Any, object}:
            raise TypeError(
                f'{method_name} cannot derive Array output from tuple item {variadic_item!r}; '
                'use scalar tuple item annotations or write an explicit bridge.',
            )
        return lambda value: coerce_array(value, item_converter)

    if _is_bare_tuple_annotation(resource_annotation):
        return lambda value: coerce_array(value, item_converter)

    if get_origin(resource_annotation) is tuple:
        raise TypeError(
            f'{method_name} cannot derive Array output from fixed tuple resource annotation {resource_annotation!r}; '
            'use tuple[Scalar, ...], bare tuple, list[...] or Sequence[...] values, or write an explicit bridge.',
        )

    if resource_annotation in {inspect.Signature.empty, Any, object}:
        return lambda value: coerce_array(value, item_converter)

    if not _array_accepts_sequence_resource(resource_annotation):
        raise TypeError(
            f'{method_name} cannot derive Array output from resource annotation {resource_annotation!r}; '
            'use list[Scalar], Sequence[Scalar], Iterable[Scalar], Iterator[Scalar], tuple[Scalar, ...], bare tuple, or write an explicit bridge.',
        )
    item_annotation = _list_item_annotation(resource_annotation)
    if item_annotation not in {inspect.Signature.empty, Any, object} and _python_converter_for_annotation(item_annotation) is None:
        raise TypeError(
            f'{method_name} cannot derive Array output from sequence item {item_annotation!r}; '
            'use scalar item annotations or write an explicit bridge.',
        )
    return lambda value: coerce_array(value, item_converter)


def _tuple_return_output_mapper(
    method_name: str,
    crm_annotation: Any,
    resource_annotation: Any,
) -> Callable[[Any], tuple[Any, ...]] | None:
    crm_args = get_args(crm_annotation)
    if len(crm_args) == 2 and crm_args[1] is Ellipsis:
        raise TypeError(f'{method_name} cannot derive output bridge for variadic tuple CRM return annotation.')
    if len(crm_args) == 1:
        raise TypeError(
            f'{method_name} cannot derive output bridge for single-item tuple return annotation; '
            'use the item annotation directly.',
        )

    resource_origin = get_origin(resource_annotation)
    if resource_origin is tuple:
        resource_args = get_args(resource_annotation)
        if len(resource_args) == 2 and resource_args[1] is Ellipsis:
            raise TypeError(f'{method_name} cannot derive output bridge from variadic tuple resource return annotation.')
        if len(resource_args) != len(crm_args):
            raise TypeError(
                f'{method_name} cannot derive tuple output bridge from {len(resource_args)} resource items; expected {len(crm_args)} CRM return items.',
            )
    elif resource_annotation in {inspect.Signature.empty, tuple}:
        resource_args = (inspect.Signature.empty,) * len(crm_args)
    else:
        raise TypeError(
            f'{method_name} cannot derive tuple output bridge from {resource_annotation!r}; use tuple[...] returns or write an explicit bridge.',
        )

    converters: list[Callable[[Any], Any] | None] = []
    has_converter = False
    for index, (crm_item, resource_item) in enumerate(zip(crm_args, resource_args)):
        converter = _output_converter(f'{method_name}.return_{index}', crm_item, resource_item)
        converters.append(converter)
        has_converter = has_converter or converter is not None
    if not has_converter:
        return None

    def output_hook(value: Any) -> tuple[Any, ...]:
        if not isinstance(value, tuple):
            raise TypeError(f'{method_name} bridge expected tuple output, got {type(value).__name__}.')
        if len(value) != len(converters):
            raise ValueError(f'{method_name} bridge expected {len(converters)} return values, got {len(value)}.')
        return tuple(
            item if converter is None else converter(item)
            for item, converter in zip(value, converters)
        )

    return output_hook


def _feature_output_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], Any]:
    if _is_tuple_annotation(resource_annotation):
        return _feature_tuple_output_mapper(method_name, feature_type, resource_annotation)
    if _is_mapping_annotation(resource_annotation):
        _ensure_mapping_string_keys(method_name, resource_annotation, direction=f'{feature_type.__name__} output from')
    if (
        resource_annotation is not inspect.Signature.empty
        and not _is_mapping_annotation(resource_annotation)
        and _domain_object_type(resource_annotation) is None
    ):
        raise TypeError(
            f'{method_name} cannot derive {feature_type.__name__} output from {resource_annotation!r}; use dict[...], tuple[...] scalar items, or a domain object return, or write an explicit bridge.',
        )
    return object_feature_mapper(feature_type, converters=_feature_converters(feature_type))


def _feature_tuple_output_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
) -> Callable[[Any], Any]:
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if not _is_bare_tuple_annotation(resource_annotation):
        args = get_args(resource_annotation)
        if len(args) == 2 and args[1] is Ellipsis:
            raise TypeError(f'{method_name} cannot derive {feature_type.__name__} output from variadic tuple return annotation.')
        if len(args) != len(field_names):
            raise TypeError(
                f'{method_name} cannot derive {feature_type.__name__} output from tuple with {len(args)} items; expected {len(field_names)} feature fields.',
            )
        for field_name, item_annotation in zip(field_names, args):
            if _python_converter_for_annotation(item_annotation) is None:
                raise TypeError(
                    f'{method_name} cannot derive {feature_type.__name__} output from tuple item {field_name!r} annotated as {item_annotation!r}; use scalar tuple item annotations or write an explicit bridge.',
                )
    converters = _feature_converters(feature_type)

    def output_hook(value: Any) -> Any:
        if not isinstance(value, tuple):
            raise TypeError(f'{method_name} bridge expected tuple output, got {type(value).__name__}.')
        if len(value) != len(field_names):
            raise ValueError(f'{method_name} bridge expected {len(field_names)} feature fields, got {len(value)}.')
        mapped: dict[str, Any] = {}
        for field_name, item in zip(field_names, value):
            converter = converters.get(field_name)
            if converter is not None:
                item = converter(item)
            mapped[field_name] = item
        return feature_type(**mapped)

    return output_hook


def _batch_object_output_mapper(
    method_name: str,
    feature_type: type,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    map_many = object_batch_mapper(feature_type, converters=converters)

    def output_hook(value: Any) -> list[Any]:
        records = _table_like_records(value)
        if records is not None:
            return map_many(records)
        if _is_malformed_batch_source(value):
            raise TypeError(f'{method_name} bridge expected an iterable Batch[{feature_type.__name__}] output, got {type(value).__name__}.')
        return map_many(value)

    return output_hook


def _batch_tuple_row_output_mapper(
    method_name: str,
    feature_type: type,
    row_annotation: Any,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    field_names, _ = _tuple_row_item_converters(
        method_name,
        feature_type,
        row_annotation,
        direction='output from',
    )

    def output_hook(value: Any) -> list[Any]:
        records = _table_like_records(value)
        if records is not None:
            if _is_malformed_batch_source(records):
                raise TypeError(f'{method_name} bridge expected an iterable Batch[{feature_type.__name__}] output, got table-like {type(value).__name__} records.')
            source = records
        else:
            if _is_malformed_batch_source(value):
                raise TypeError(f'{method_name} bridge expected an iterable Batch[{feature_type.__name__}] output, got {type(value).__name__}.')
            source = value
        rows: list[Any] = []
        for row in source:
            if not isinstance(row, tuple):
                raise TypeError(f'{method_name} bridge expected tuple rows for Batch[{feature_type.__name__}] output, got {type(row).__name__}.')
            if len(row) != len(field_names):
                raise ValueError(f'{method_name} bridge expected tuple rows with {len(field_names)} feature fields, got {len(row)}.')
            mapped: dict[str, Any] = {}
            for field_name, item in zip(field_names, row):
                converter = converters.get(field_name)
                if converter is not None:
                    item = converter(item)
                mapped[field_name] = item
            rows.append(feature_type(**mapped))
        return rows

    return output_hook


def _batch_variadic_tuple_output_mapper(
    method_name: str,
    feature_type: type,
    item_annotation: Any,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    if get_origin(item_annotation) is tuple:
        return _batch_tuple_row_output_mapper(method_name, feature_type, item_annotation, converters)
    if _is_mapping_annotation(item_annotation):
        _ensure_mapping_string_keys(method_name, item_annotation, direction=f'Batch[{feature_type.__name__}] output from tuple row')
    if (
        _is_mapping_annotation(item_annotation)
        or _domain_object_type(item_annotation) is not None
        or _same_fastdb_abi_annotation(feature_type, item_annotation)
    ):
        return _batch_object_output_mapper(method_name, feature_type, converters)
    raise TypeError(
        f'{method_name} cannot derive Batch[{feature_type.__name__}] output from tuple[{item_annotation!r}, ...]; use tuple row, mapping row, domain object row, fastdb feature row, or write an explicit bridge.',
    )


def _batch_sequence_output_mapper(
    method_name: str,
    feature_type: type,
    item_annotation: Any,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    if item_annotation in {inspect.Signature.empty, Any, object}:
        return _batch_object_output_mapper(method_name, feature_type, converters)
    if get_origin(item_annotation) is tuple:
        return _batch_tuple_row_output_mapper(method_name, feature_type, item_annotation, converters)
    if _is_mapping_annotation(item_annotation):
        _ensure_mapping_string_keys(method_name, item_annotation, direction=f'Batch[{feature_type.__name__}] output from sequence row')
    if (
        _is_mapping_annotation(item_annotation)
        or _domain_object_type(item_annotation) is not None
        or _same_fastdb_abi_annotation(feature_type, item_annotation)
    ):
        return _batch_object_output_mapper(method_name, feature_type, converters)
    raise TypeError(
        f'{method_name} cannot derive Batch[{feature_type.__name__}] output from sequence item {item_annotation!r}; use tuple rows, mapping rows, domain object rows, fastdb feature rows, or write an explicit bridge.',
    )


def _tuple_columns_output_mapper(
    method_name: str,
    feature_type: type,
    resource_annotation: Any,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    args = get_args(resource_annotation)
    if len(args) == 2 and args[1] is Ellipsis:
        raise TypeError(f'{method_name} cannot derive Batch output from variadic tuple return annotation.')
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if len(args) != len(field_names):
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] output from tuple with {len(args)} items; expected {len(field_names)} columns.',
        )
    for field_name, column_annotation in zip(field_names, args):
        if not _is_sequence_annotation(column_annotation):
            raise TypeError(
                f'{method_name} cannot derive Batch[{feature_type.__name__}] output from tuple column {field_name!r} annotated as {column_annotation!r}; use list[...] columns or Sequence[...] columns, or write an explicit bridge.',
            )
        _ensure_scalar_column_item_annotation(
            method_name,
            feature_type,
            field_name,
            column_annotation,
            direction='output from',
        )

    def output_hook(value: Any) -> list[Any]:
        if not isinstance(value, tuple):
            raise TypeError(f'{method_name} bridge expected tuple output, got {type(value).__name__}.')
        if len(value) != len(field_names):
            raise ValueError(f'{method_name} bridge expected {len(field_names)} output columns, got {len(value)}.')
        columns = {
            field_name: column
            for field_name, column in zip(field_names, value)
        }
        return batch_from_columns(feature_type, converters=converters, **columns)

    return output_hook


def _dict_columns_output_mapper(
    method_name: str,
    feature_type: type,
    converters: Mapping[str, Callable[[Any], Any]],
) -> Callable[[Any], list[Any]]:
    def output_hook(value: Any) -> list[Any]:
        if not isinstance(value, Mapping):
            raise TypeError(f'{method_name} bridge expected mapping output, got {type(value).__name__}.')
        if not all(isinstance(key, str) for key in value):
            raise TypeError(f'{method_name} bridge expected string column names in mapping output.')
        return batch_from_columns(feature_type, converters=converters, **dict(value))

    return output_hook


def _list_item_annotation(annotation: Any) -> Any:
    if not _is_sequence_annotation(annotation):
        return inspect.Signature.empty
    args = get_args(annotation)
    return args[0] if args else inspect.Signature.empty


def _ensure_dict_column_scalar_items(method_name: str, feature_type: type, annotation: Any, *, direction: str) -> None:
    args = get_args(annotation)
    if len(args) != 2:
        return
    _ensure_scalar_column_item_annotation(
        method_name,
        feature_type,
        '<mapping-value>',
        args[1],
        direction=direction,
    )


def _ensure_scalar_column_item_annotation(
    method_name: str,
    feature_type: type,
    field_name: str,
    column_annotation: Any,
    *,
    direction: str,
) -> None:
    item_annotation = _list_item_annotation(column_annotation)
    if item_annotation in {inspect.Signature.empty, Any, object}:
        return
    if _python_converter_for_annotation(item_annotation) is None:
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] {direction} column {field_name!r} '
            f'with non-scalar column item {item_annotation!r}; use scalar column item annotations or write an explicit bridge.',
        )


def _ensure_scalar_resource_annotation(method_name: str, resource_annotation: Any, *, direction: str) -> None:
    if resource_annotation in {inspect.Signature.empty, Any, object}:
        return
    if _python_converter_for_annotation(resource_annotation) is not None:
        return
    raise TypeError(
        f'{method_name} cannot derive scalar {direction} resource annotation {resource_annotation!r}; use scalar resource annotations or write an explicit bridge.',
    )


def _variadic_tuple_item_annotation(annotation: Any) -> Any:
    if get_origin(annotation) is not tuple:
        return inspect.Signature.empty
    args = get_args(annotation)
    if len(args) == 2 and args[1] is Ellipsis:
        return args[0]
    return inspect.Signature.empty


def _is_bare_tuple_annotation(annotation: Any) -> bool:
    return annotation is tuple or (get_origin(annotation) is tuple and not get_args(annotation))


def _is_tuple_annotation(annotation: Any) -> bool:
    return annotation is tuple or get_origin(annotation) is tuple


def _tuple_row_item_converters(
    method_name: str,
    feature_type: type,
    row_annotation: Any,
    *,
    direction: str,
) -> tuple[tuple[str, ...], list[Callable[[Any], Any]]]:
    args = get_args(row_annotation)
    if len(args) == 2 and args[1] is Ellipsis:
        raise TypeError(f'{method_name} cannot derive Batch[{feature_type.__name__}] {direction} variadic tuple rows.')
    field_names = tuple(field.name for field in get_schema(feature_type).fields)
    if len(args) != len(field_names):
        raise TypeError(
            f'{method_name} cannot derive Batch[{feature_type.__name__}] {direction} tuple with {len(args)} items; expected {len(field_names)} feature fields.',
        )
    converters: list[Callable[[Any], Any]] = []
    for field_name, item_annotation in zip(field_names, args):
        converter = _python_converter_for_annotation(item_annotation)
        if converter is None:
            raise TypeError(
                f'{method_name} cannot derive Batch[{feature_type.__name__}] {direction} tuple row item {field_name!r} annotated as {item_annotation!r}; use scalar tuple item annotations or write an explicit bridge.',
            )
        converters.append(converter)
    return field_names, converters


def _is_mapping_annotation(annotation: Any) -> bool:
    origin = get_origin(annotation)
    return (
        annotation is dict
        or annotation is Mapping
        or annotation is MutableMapping
        or origin is dict
        or origin is Mapping
        or origin is MutableMapping
    )


def _is_dict_column_annotation(annotation: Any) -> bool:
    args = get_args(annotation)
    return (
        _is_mapping_annotation(annotation)
        and len(args) == 2
        and args[0] is str
        and _is_sequence_annotation(args[1])
    )


def _ensure_mapping_string_keys(method_name: str, annotation: Any, *, direction: str) -> None:
    args = get_args(annotation)
    if args and (len(args) != 2 or args[0] is not str):
        raise TypeError(
            f'{method_name} cannot derive {direction} mapping annotation {annotation!r}; mapping keys must be str.',
        )


def _is_sequence_annotation(annotation: Any) -> bool:
    origin = get_origin(annotation)
    return (
        annotation is list
        or annotation is Sequence
        or annotation is Iterable
        or annotation is Iterator
        or annotation is MutableSequence
        or origin is list
        or origin is Sequence
        or origin is Iterable
        or origin is Iterator
        or origin is MutableSequence
    )


def _is_iterator_annotation(annotation: Any) -> bool:
    origin = get_origin(annotation)
    return annotation is Iterator or origin is Iterator


def _array_accepts_sequence_resource(annotation: Any) -> bool:
    return annotation in {Any, object, list, Sequence, Iterable, Iterator, MutableSequence} or _is_sequence_annotation(annotation)


def _domain_object_type(annotation: Any) -> type | None:
    if annotation in {
        inspect.Signature.empty,
        Any,
        object,
        bool,
        int,
        float,
        str,
        bytes,
        bytearray,
        dict,
        list,
        tuple,
        set,
        Mapping,
        MutableMapping,
        Sequence,
        Iterator,
        MutableSequence,
        Iterable,
    }:
        return None
    if get_origin(annotation) is not None:
        return None
    if not isinstance(annotation, type):
        return None
    if _is_fastdb_abi_annotation(annotation):
        return None
    return annotation


def _ensure_keyword_constructible(
    method_name: str,
    feature_type: type,
    target_class: type,
    field_names: tuple[str, ...],
) -> None:
    try:
        signature = inspect.signature(target_class)
    except (TypeError, ValueError):
        return
    params = tuple(signature.parameters.values())
    accepts_kwargs = any(param.kind is inspect.Parameter.VAR_KEYWORD for param in params)
    keyword_params = {
        param.name: param
        for param in params
        if param.kind in {inspect.Parameter.POSITIONAL_OR_KEYWORD, inspect.Parameter.KEYWORD_ONLY}
    }
    missing_fields = [
        field_name
        for field_name in field_names
        if field_name not in keyword_params and not accepts_kwargs
    ]
    extra_required = [
        param.name
        for param in params
        if param.default is inspect.Signature.empty
        and param.kind in {inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD, inspect.Parameter.KEYWORD_ONLY}
        and param.name not in field_names
    ]
    if missing_fields or extra_required:
        missing = ', '.join(missing_fields) or '<none>'
        extra = ', '.join(extra_required) or '<none>'
        raise TypeError(
            f'{method_name} cannot derive fastdb {feature_type.__name__} input to {target_class.__name__}: '
            f'constructor must accept fastdb fields; missing={missing}; extra_required={extra}.',
        )


def _ensure_table_constructible(method_name: str, feature_type: type, target_class: type) -> None:
    if _table_object_factory(target_class) is not None:
        return
    try:
        signature = inspect.signature(target_class)
    except (TypeError, ValueError):
        return
    params = tuple(signature.parameters.values())
    accepts_args = any(param.kind is inspect.Parameter.VAR_POSITIONAL for param in params)
    positional_params = [
        param
        for param in params
        if param.kind in {inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD}
    ]
    required_after_first = [
        param.name
        for param in positional_params[1:]
        if param.default is inspect.Signature.empty
    ]
    required_keyword_only = [
        param.name
        for param in params
        if param.kind is inspect.Parameter.KEYWORD_ONLY
        and param.default is inspect.Signature.empty
    ]
    if (accepts_args or positional_params) and not required_after_first and not required_keyword_only:
        return
    raise TypeError(
        f'{method_name} cannot derive Batch[{feature_type.__name__}] input to {target_class.__name__}: '
        'constructor must accept one row-record iterable or define from_records/from_pylist/from_dicts.',
    )


def _construct_table_object(target_class: type, records: list[dict[str, Any]]) -> Any:
    factory = _table_object_factory(target_class)
    if factory is not None:
        return factory(records)
    return target_class(records)


def _table_object_factory(target_class: type) -> Callable[[list[dict[str, Any]]], Any] | None:
    for factory_name in ('from_records', 'from_pylist', 'from_dicts'):
        factory = getattr(target_class, factory_name, None)
        if callable(factory) and _callable_accepts_single_positional_argument(factory):
            return factory
    return None


def _callable_accepts_single_positional_argument(value: Callable[..., Any]) -> bool:
    try:
        signature = inspect.signature(value)
    except (TypeError, ValueError):
        return True
    params = tuple(signature.parameters.values())
    accepts_args = any(param.kind is inspect.Parameter.VAR_POSITIONAL for param in params)
    positional_params = [
        param
        for param in params
        if param.kind in {inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD}
    ]
    required_positional = [
        param
        for param in positional_params
        if param.default is inspect.Signature.empty
    ]
    required_keyword_only = [
        param
        for param in params
        if param.kind is inspect.Parameter.KEYWORD_ONLY
        and param.default is inspect.Signature.empty
    ]
    if required_keyword_only:
        return False
    if accepts_args:
        return True
    return bool(positional_params) and len(required_positional) <= 1


def _safe_type_hints(target_type: type) -> dict[str, Any]:
    try:
        return get_type_hints(target_type)
    except Exception:
        return {}


def _feature_converters(feature_type: type) -> dict[str, Callable[[Any], Any]]:
    hints = get_type_hints(feature_type)
    converters: dict[str, Callable[[Any], Any]] = {}
    for field_name in (field.name for field in get_schema(feature_type).fields):
        converter = _python_converter_for_annotation(hints.get(field_name))
        if converter is not None:
            converters[field_name] = converter
    return converters


def _same_fastdb_abi_annotation(left: Any, right: Any) -> bool:
    return left == right and _is_fastdb_abi_annotation(left)


def _is_fastdb_abi_annotation(annotation: Any) -> bool:
    if annotation in _BOOL_TYPES or annotation in _INT_TYPES or annotation in _FLOAT_TYPES:
        return True
    if annotation in _STR_TYPES or annotation in _BYTES_TYPES:
        return True
    if is_feature(annotation):
        return True
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin is Array:
        return len(args) == 1 and _is_fastdb_scalar_annotation(args[0])
    if origin is Batch:
        return len(args) == 1 and is_feature(args[0])
    return False


def _is_fastdb_scalar_annotation(annotation: Any) -> bool:
    return (
        annotation in _BOOL_TYPES
        or annotation in _INT_TYPES
        or annotation in _FLOAT_TYPES
        or annotation in _STR_TYPES
        or annotation in _BYTES_TYPES
    )


def _python_converter_for_annotation(annotation: Any) -> Callable[[Any], Any] | None:
    if annotation in _BOOL_TYPES or annotation is bool:
        return coerce_bool_scalar
    if annotation in _INT_TYPES or annotation is int:
        return int
    if annotation in _FLOAT_TYPES or annotation is float:
        return float
    if annotation in _STR_TYPES or annotation is str:
        return str
    if annotation in _BYTES_TYPES or annotation is bytes:
        return bytes
    return None
