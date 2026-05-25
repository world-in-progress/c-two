from __future__ import annotations

import inspect
import json
import math
import types
from typing import Any, ForwardRef, Union, get_args, get_origin, get_type_hints

from .contract import crm_contract_identity
from ._payload_abi import (
    PayloadAbiRef,
    MethodPayloadAbiShape,
    MethodParameterShape,
)
from .meta import MethodAccess, get_method_access
from .methods import rpc_method_names
from .payload_plan import DEFAULT_PICKLE_PROTOCOL, PayloadBinding, PayloadPlanKind

_DESCRIPTOR_SCHEMA = 'c-two.python.crm.descriptor.v2'
_PORTABLE_CONTRACT_SCHEMA = 'c-two.contract.v1'
_ABI_SCHEMA = 'c-two.python.crm.abi.v2'
_SIGNATURE_SCHEMA = 'c-two.python.crm.signature.v2'
_PICKLE_DEFAULT_REF = {
    'family': 'python-pickle-default',
    'kind': 'builtin',
    'python_min': '3.10',
    'portable': False,
    'version': f'pickle-protocol-{DEFAULT_PICKLE_PROTOCOL}',
}
_FASTDB_FIRST_PORTABLE_PAYLOAD_ABI_IDS = {
    'org.fastdb.call-db',
}
_PRIMITIVES = {
    bool: 'bool',
    int: 'int',
    float: 'float',
    str: 'str',
    bytes: 'bytes',
    memoryview: 'memoryview',
    bytearray: 'bytearray',
}
_BARE_CONTAINERS = {list, dict, tuple, set, frozenset}


def build_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    portable: bool = False,
) -> dict[str, Any]:
    crm_ns, crm_name, crm_ver = crm_contract_identity(crm_class)
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    return {
        'crm': {
            'name': crm_name,
            'namespace': crm_ns,
            'version': crm_ver,
        },
        'methods': [
            _method_descriptor(crm_class, method_name, portable=portable)
            for method_name in method_names
        ],
        'schema': _DESCRIPTOR_SCHEMA,
    }


def build_contract_fingerprints(
    crm_class: type,
    methods: list[str] | None = None,
) -> tuple[str, str]:
    descriptor = build_contract_descriptor(crm_class, methods)
    return _fingerprints_from_descriptor(descriptor)


def _fingerprints_from_descriptor(descriptor: dict[str, Any]) -> tuple[str, str]:
    abi_descriptor = {
        'crm': descriptor['crm'],
        'methods': [
            {
                'buffer': method['buffer'],
                'input': method['wire']['input'],
                'name': method['name'],
                'output': method['wire']['output'],
            }
            for method in descriptor['methods']
        ],
        'schema': _ABI_SCHEMA,
    }
    signature_descriptor = {
        'crm': descriptor['crm'],
        'methods': [
            {
                'access': method['access'],
                'buffer': method['buffer'],
                'name': method['name'],
                'parameters': method['parameters'],
                'return': method['return'],
                'transfer': method['transfer'],
            }
            for method in descriptor['methods']
        ],
        'schema': _SIGNATURE_SCHEMA,
    }
    return (_hash_descriptor(abi_descriptor), _hash_descriptor(signature_descriptor))


def build_portable_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
) -> dict[str, Any]:
    descriptor = build_contract_descriptor(crm_class, methods, portable=True)
    runtime_descriptor = build_contract_descriptor(crm_class, methods)
    abi_hash, signature_hash = _fingerprints_from_descriptor(runtime_descriptor)
    return {
        'crm': descriptor['crm'],
        'fingerprints': {
            'abi_hash': abi_hash,
            'signature_hash': signature_hash,
        },
        'methods': [
            {
                'access': method['access'],
                'buffer': method['buffer'],
                'name': method['name'],
                'parameters': [
                    {
                        'default': param['default'],
                        'kind': param['kind'],
                        'name': param['name'],
                        'type': param['annotation'],
                    }
                    for param in method['parameters']
                ],
                'return': method['return'],
                'wire': method['wire'],
            }
            for method in descriptor['methods']
        ],
        'schema': _PORTABLE_CONTRACT_SCHEMA,
    }


def build_contract_payload_abi_artifacts(
    crm_class: type,
    methods: list[str] | None = None,
) -> list[dict[str, Any]]:
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    artifacts: list[dict[str, Any]] = []
    seen: set[str] = set()
    for method_name in method_names:
        method = getattr(crm_class, method_name)
        for binding in (
            getattr(method, '_input_payload_binding', None),
            getattr(method, '_output_payload_binding', None),
        ):
            for artifact in _payload_abi_artifacts_for_binding(binding):
                key = _canonical_json(artifact)
                if key in seen:
                    continue
                seen.add(key)
                artifacts.append(artifact)
    return artifacts


def export_contract_payload_abi_artifacts(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str:
    artifacts = build_contract_payload_abi_artifacts(crm_class, methods)
    if pretty:
        return json.dumps(artifacts, sort_keys=True, indent=2) + '\n'
    return json.dumps(artifacts, sort_keys=True, separators=(',', ':'))


def contract_descriptor_diagnostics(
    crm_class: type,
    methods: list[str] | None = None,
) -> list[dict[str, Any]]:
    diagnostics: list[dict[str, Any]] = []
    method_names = rpc_method_names(crm_class) if methods is None else list(methods)
    diagnostics.extend(_fastdb_method_payload_abi_diagnostics_for_contract(crm_class, method_names))
    try:
        descriptor = build_contract_descriptor(crm_class, methods)
    except Exception as exc:
        diagnostics.append({
            'code': 'descriptor_build_failed',
            'message': f'Contract descriptor could not be built: {exc}',
            'severity': 'error',
        })
        return _dedupe_diagnostics(diagnostics)
    for method in descriptor['methods']:
        method_name = method['name']
        wire = method['wire']
        for position in ('input', 'output'):
            diagnostic = _fastdb_first_wire_diagnostic(
                method_name,
                position,
                wire.get(position),
            )
            if diagnostic is not None:
                diagnostics.append(diagnostic)
    return _dedupe_diagnostics(diagnostics)


def _fastdb_method_payload_abi_diagnostics_for_contract(
    crm_class: type,
    method_names: list[str],
) -> list[dict[str, Any]]:
    crm_ns, crm_name, crm_ver = crm_contract_identity(crm_class)
    diagnostics: list[dict[str, Any]] = []
    for method_name in method_names:
        try:
            method = getattr(crm_class, method_name)
            signature_target = inspect.unwrap(method)
            sig = inspect.signature(signature_target)
            type_hints = _resolved_type_hints(signature_target, method_name)
        except Exception as exc:
            diagnostics.append({
                'code': 'fastdb_diagnostics_unavailable',
                'message': f'FastDB diagnostics for {method_name} could not inspect annotations: {exc}',
                'method': method_name,
                'severity': 'error',
            })
            continue
        payload_abi_context = dict(getattr(method, '_payload_abi_context', None) or {})
        payload_abi_context.setdefault('crm_namespace', crm_ns)
        payload_abi_context.setdefault('crm_name', crm_name)
        payload_abi_context.setdefault('crm_version', crm_ver)
        payload_abi_context.setdefault('method_name', method_name)
        signature_params = [
            param
            for index, param in enumerate(sig.parameters.values())
            if not (index == 0 and param.name in {'self', 'cls'})
        ]
        parameters = tuple(
            MethodParameterShape(
                name=param.name,
                annotation=type_hints[param.name],
                default=param.default,
                kind=param.kind.name,
            )
            for param in signature_params
            if param.name in type_hints
        )
        input_binding = getattr(method, '_input_payload_binding', None)
        output_binding = getattr(method, '_output_payload_binding', None)
        if parameters:
            shape = MethodPayloadAbiShape(
                method_name=method_name,
                direction='input',
                crm_namespace=crm_ns,
                crm_name=crm_name,
                crm_version=crm_ver,
                parameters=parameters,
            )
            context = dict(payload_abi_context, position='input')
            if _payload_abi_ref_for_binding(input_binding) is None:
                diagnostics.extend(_fastdb_method_payload_abi_diagnostics(shape, context))
        if 'return' in type_hints and type_hints['return'] not in (None, type(None)):
            shape = MethodPayloadAbiShape(
                method_name=method_name,
                direction='output',
                crm_namespace=crm_ns,
                crm_name=crm_name,
                crm_version=crm_ver,
                return_annotation=type_hints['return'],
            )
            context = dict(payload_abi_context, position='output')
            if _payload_abi_ref_for_binding(output_binding) is None:
                diagnostics.extend(_fastdb_method_payload_abi_diagnostics(shape, context))
    return diagnostics


def _fastdb_method_payload_abi_diagnostics(
    shape: MethodPayloadAbiShape,
    context: dict[str, Any],
) -> list[dict[str, Any]]:
    try:
        from c_two.fastdb.call_db import diagnostics_for_method_payload_abi
    except ImportError:
        return []
    result = diagnostics_for_method_payload_abi(shape, context)
    diagnostics: list[dict[str, Any]] = []
    for item in result:
        if not isinstance(item, dict):
            raise TypeError('FastDB method payload ABI diagnostics must be dicts.')
        payload = dict(item)
        payload.setdefault('method', shape.method_name)
        payload.setdefault('position', shape.direction)
        payload.setdefault('planner', 'c_two.fastdb.call_db')
        payload.setdefault('severity', 'warning')
        message = payload.get('message')
        if not isinstance(message, str) or not message:
            payload['message'] = (
                f'c_two.fastdb.call_db reported a payload ABI diagnostic for '
                f'{shape.method_name}.{shape.direction}.'
            )
        diagnostics.append(payload)
    return diagnostics


def _dedupe_diagnostics(diagnostics: list[dict[str, Any]]) -> list[dict[str, Any]]:
    deduped: list[dict[str, Any]] = []
    seen: set[tuple[object, ...]] = set()
    for diagnostic in diagnostics:
        key = (
            diagnostic.get('code'),
            diagnostic.get('method'),
            diagnostic.get('position'),
            diagnostic.get('reason'),
            diagnostic.get('message'),
        )
        if key in seen:
            continue
        seen.add(key)
        deduped.append(diagnostic)
    return deduped


def export_contract_descriptor(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str:
    descriptor = build_portable_contract_descriptor(crm_class, methods)
    compact = json.dumps(descriptor, sort_keys=True, separators=(',', ':'))
    from c_two._native import validate_portable_contract_descriptor

    validate_portable_contract_descriptor(compact.encode())
    if pretty:
        return json.dumps(descriptor, sort_keys=True, indent=2) + '\n'
    return compact


def _hash_descriptor(descriptor: dict[str, Any]) -> str:
    from c_two._native import contract_descriptor_sha256_hex

    payload = json.dumps(
        descriptor,
        sort_keys=True,
        separators=(',', ':'),
    ).encode()
    return contract_descriptor_sha256_hex(payload)


def _method_descriptor(
    crm_class: type,
    method_name: str,
    *,
    portable: bool,
) -> dict[str, Any]:
    method = getattr(crm_class, method_name)
    signature_target = inspect.unwrap(method)
    sig = inspect.signature(signature_target)
    type_hints = _resolved_type_hints(signature_target, method_name)
    payload_abi_context = getattr(method, '_payload_abi_context', None) or {}
    input_binding = getattr(method, '_input_payload_binding', None)
    output_binding = getattr(method, '_output_payload_binding', None)
    input_method_payload_abi_aggregate = bool(
        getattr(method, '_input_method_payload_abi_aggregate', False),
    )
    output_method_payload_abi_aggregate = bool(
        getattr(method, '_output_method_payload_abi_aggregate', False),
    )
    buffer_mode = getattr(method, '_input_buffer_mode', 'view')

    signature_params = [
        param
        for index, param in enumerate(sig.parameters.values())
        if not (index == 0 and param.name in {'self', 'cls'})
    ]
    input_annotation_ref = _payload_abi_ref_for_binding(input_binding)
    input_annotation_binding = input_binding
    if len(signature_params) != 1 and not input_method_payload_abi_aggregate:
        input_annotation_ref = None
        input_annotation_binding = None

    params = []
    for param in signature_params:
        if param.kind in {
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        }:
            raise TypeError(
                f'{method_name} uses varargs parameter {param.name!r}; '
                'CRM RPC methods must have explicit parameters.',
            )
        if param.name not in type_hints:
            raise TypeError(
                f'{method_name}.{param.name} is missing a type annotation.',
            )
        annotation = type_hints[param.name]
        params.append({
            'annotation': _annotation_descriptor(
                annotation,
                method_name,
                payload_abi_ref=input_annotation_ref,
                payload_abi_context=payload_abi_context,
                portable=portable,
                payload_binding=input_annotation_binding,
                aggregate_payload_abi=input_method_payload_abi_aggregate,
            ),
            'default': _default_descriptor(param.default, method_name, param.name),
            'kind': param.kind.name,
            'name': param.name,
        })

    if 'return' not in type_hints:
        raise TypeError(f'{method_name} is missing a return annotation.')
    return_annotation = type_hints['return']

    access = get_method_access(method)
    input_wire = _wire_ref_for_input(input_binding, params)
    output_wire = _wire_ref_for_output(output_binding, return_annotation)
    if portable:
        _ensure_portable_wire_ref(input_wire, method_name, 'input')
        _ensure_portable_wire_ref(output_wire, method_name, 'output')
    return {
        'access': 'read' if access is MethodAccess.READ else 'write',
        'buffer': buffer_mode,
        'name': method_name,
        'parameters': params,
        'return': _annotation_descriptor(
            return_annotation,
            method_name,
            payload_abi_ref=_payload_abi_ref_for_binding(output_binding),
            payload_abi_context=payload_abi_context,
            portable=portable,
            payload_binding=output_binding,
            aggregate_payload_abi=output_method_payload_abi_aggregate,
        ),
        'transfer': {
            'input': _payload_binding_descriptor(input_binding),
            'output': _payload_binding_descriptor(output_binding),
        },
        'wire': {
            'input': input_wire,
            'output': output_wire,
        },
    }


def _resolved_type_hints(func: object, method_name: str) -> dict[str, Any]:
    try:
        return get_type_hints(func, include_extras=True)
    except NameError as exc:
        raise ValueError(
            f'{method_name} contains an unresolved forward reference: {exc}',
        ) from exc
    except TypeError as exc:
        raise TypeError(
            f'{method_name} contains an unsupported annotation: {exc}',
        ) from exc


def _annotation_descriptor(
    annotation: Any,
    method_name: str,
    *,
    payload_abi_ref: PayloadAbiRef | None = None,
    payload_abi_context: dict[str, Any] | None = None,
    portable: bool = False,
    payload_binding: PayloadBinding | None = None,
    aggregate_payload_abi: bool = False,
) -> dict[str, Any]:
    if annotation is inspect.Signature.empty:
        raise TypeError(f'{method_name} contains a missing annotation.')
    if annotation is Any:
        raise TypeError(f'{method_name} uses Any, which is not a stable RPC ABI.')
    if isinstance(annotation, (str, ForwardRef)):
        raise ValueError(
            f'{method_name} contains an unresolved forward reference.',
        )
    if annotation is None or annotation is type(None):
        return {'kind': 'none'}
    if annotation in _PRIMITIVES:
        return {'kind': 'primitive', 'name': _PRIMITIVES[annotation]}
    if annotation in _BARE_CONTAINERS:
        raise TypeError(f'{method_name} uses a bare container annotation.')
    if aggregate_payload_abi and payload_abi_ref is not None:
        return {
            'codec': payload_abi_ref.to_wire_ref(),
            'kind': 'codec',
        }
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in {Union, types.UnionType}:
        items = [
            _annotation_descriptor(
                arg,
                method_name,
                payload_abi_context=payload_abi_context,
                portable=portable,
                payload_binding=payload_binding,
            )
            for arg in args
        ]
        items.sort(key=_canonical_json)
        return {'items': items, 'kind': 'union'}
    if origin is list:
        if not args:
            raise TypeError(f'{method_name} uses a bare container annotation.')
        item_payload_abi_ref = _item_payload_abi_ref_for_binding(payload_binding)
        return {
            'item': _annotation_descriptor(
                args[0],
                method_name,
                payload_abi_ref=item_payload_abi_ref,
                payload_abi_context=payload_abi_context,
                portable=portable,
                payload_binding=payload_binding,
            ),
            'kind': 'list',
        }
    if origin is dict:
        if len(args) != 2:
            raise TypeError(f'{method_name} uses a bare container annotation.')
        return {
            'key': _annotation_descriptor(
                args[0],
                method_name,
                payload_abi_context=payload_abi_context,
                portable=portable,
                payload_binding=payload_binding,
            ),
            'kind': 'dict',
            'value': _annotation_descriptor(
                args[1],
                method_name,
                payload_abi_context=payload_abi_context,
                portable=portable,
                payload_binding=payload_binding,
            ),
        }
    if origin is tuple:
        if not args:
            raise TypeError(f'{method_name} uses a bare container annotation.')
        if len(args) == 2 and args[1] is Ellipsis:
            return {
                'item': _annotation_descriptor(
                    args[0],
                    method_name,
                    payload_abi_context=payload_abi_context,
                    portable=portable,
                    payload_binding=payload_binding,
                ),
                'kind': 'tuple_variadic',
            }
        return {
            'items': [
                    _annotation_descriptor(
                        arg,
                        method_name,
                        payload_abi_context=payload_abi_context,
                        portable=portable,
                        payload_binding=payload_binding,
                    )
                    for arg in args
                ],
                'kind': 'tuple',
            }

    if payload_abi_ref is not None:
        return {
            'codec': payload_abi_ref.to_wire_ref(),
            'kind': 'codec',
        }
    if (
        isinstance(annotation, type)
        and (
            not portable
            or (
                payload_binding is not None
                and payload_binding.kind is PayloadPlanKind.PYTHON_PICKLE
            )
        )
    ):
        return _python_type_descriptor(annotation)

    raise TypeError(
        f'{method_name} contains unsupported annotation {annotation!r}.',
    )


def _python_type_descriptor(annotation: type) -> dict[str, str]:
    module = getattr(annotation, '__module__', None)
    name = getattr(annotation, '__name__', None)
    if not isinstance(module, str) or not module:
        module = '<unknown>'
    if not isinstance(name, str) or not name:
        name = '<anonymous>'
    return {
        'kind': 'python_type',
        'module': module,
        'name': name,
    }


def _item_payload_abi_ref_for_binding(binding: PayloadBinding | None) -> PayloadAbiRef | None:
    if binding is None:
        return None
    return None


def _default_descriptor(value: object, method_name: str, param_name: str) -> dict[str, Any]:
    if value is inspect.Parameter.empty:
        return {'kind': 'missing'}
    if value is None:
        return {'kind': 'json_scalar', 'value': None}
    if isinstance(value, bool):
        return {'kind': 'json_scalar', 'value': value}
    if isinstance(value, int) and not isinstance(value, bool):
        return {'kind': 'json_scalar', 'value': value}
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError(
                f'{method_name}.{param_name} has a non-finite default value.',
            )
        return {'kind': 'json_scalar', 'value': value}
    if isinstance(value, str):
        return {'kind': 'json_scalar', 'value': value}
    raise TypeError(
        f'{method_name}.{param_name} has unsupported default value '
        f'{value!r}; only JSON scalar defaults are allowed.',
    )


def _wire_ref_for_input(
    input_binding: PayloadBinding | None,
    params: list[dict[str, Any]],
) -> dict[str, Any] | None:
    if input_binding is not None and input_binding.kind is not PayloadPlanKind.NO_PAYLOAD:
        return _payload_binding_wire_ref(input_binding)
    if not params:
        return None
    return dict(_PICKLE_DEFAULT_REF)


def _wire_ref_for_output(
    output_binding: PayloadBinding | None,
    return_annotation: Any,
) -> dict[str, Any] | None:
    if output_binding is not None and output_binding.kind is not PayloadPlanKind.NO_PAYLOAD:
        return _payload_binding_wire_ref(output_binding)
    if return_annotation is None or return_annotation is type(None):
        return None
    return dict(_PICKLE_DEFAULT_REF)


def _payload_binding_descriptor(binding: PayloadBinding | None) -> dict[str, Any] | None:
    if binding is None or binding.kind is PayloadPlanKind.NO_PAYLOAD:
        return None
    return _payload_binding_wire_ref(binding)


def _payload_abi_ref_for_binding(binding: PayloadBinding | None) -> PayloadAbiRef | None:
    if binding is None or binding.kind is not PayloadPlanKind.FDB:
        return None
    ref = binding.payload_abi_ref
    return ref if isinstance(ref, PayloadAbiRef) else None


def _payload_binding_wire_ref(binding: PayloadBinding) -> dict[str, Any]:
    if binding.kind is PayloadPlanKind.PYTHON_PICKLE:
        return dict(_PICKLE_DEFAULT_REF)
    if binding.kind is PayloadPlanKind.FDB:
        ref = binding.payload_abi_ref
        if not isinstance(ref, PayloadAbiRef):
            raise TypeError('FDB payload bindings must carry a PayloadAbiRef.')
        return ref.to_wire_ref()
    if binding.kind is PayloadPlanKind.NO_PAYLOAD:
        raise ValueError('NO_PAYLOAD bindings do not have a wire ref.')
    raise ValueError(f'Unsupported payload binding kind {binding.kind!r}.')


def _payload_abi_artifacts_for_binding(binding: PayloadBinding | None) -> tuple[dict[str, Any], ...]:
    if binding is None:
        return ()
    raw_artifacts = binding.payload_abi_artifacts
    if raw_artifacts is None:
        return ()
    if isinstance(raw_artifacts, dict):
        raw_items = (raw_artifacts,)
    else:
        try:
            raw_items = tuple(raw_artifacts)
        except TypeError as exc:
            raise TypeError(
                f'{binding.label or binding.kind.value} payload_abi_artifacts must be a JSON object or iterable of JSON objects.',
            ) from exc
    artifacts: list[dict[str, Any]] = []
    for item in raw_items:
        if not isinstance(item, dict):
            raise TypeError(
                f'{binding.label or binding.kind.value} payload_abi_artifacts entries must be JSON objects.',
            )
        _canonical_json(item)
        artifacts.append(item)
    return tuple(artifacts)


def _ensure_portable_wire_ref(
    wire_ref: dict[str, Any] | None,
    method_name: str,
    position: str,
) -> None:
    if wire_ref is None:
        return
    if wire_ref.get('family') == 'python-pickle-default' or wire_ref.get('portable') is False:
        raise ValueError(
            f'{method_name} cannot be exported as a portable contract: '
            f'{position} uses python-pickle-default.',
        )
    payload_abi_id = wire_ref.get('id')
    if wire_ref.get('kind') != 'codec_ref' or not isinstance(payload_abi_id, str):
        raise ValueError(
            f'{method_name} cannot be exported as a portable contract: '
            f'{position} does not use a PayloadAbiRef wire identity.',
        )
    if payload_abi_id not in _FASTDB_FIRST_PORTABLE_PAYLOAD_ABI_IDS:
        raise ValueError(
            f'{method_name} cannot be exported as a portable contract: '
            f'{position} uses non-FastDB payload ABI {payload_abi_id}.',
        )


def _fastdb_first_wire_diagnostic(
    method_name: str,
    position: str,
    wire_ref: dict[str, Any] | None,
) -> dict[str, Any] | None:
    if wire_ref is None:
        return None
    base = {
        'method': method_name,
        'position': position,
        'severity': 'warning',
    }
    if wire_ref.get('family') == 'python-pickle-default' or wire_ref.get('portable') is False:
        return {
            **base,
            'code': 'python_only_pickle',
            'message': (
                f'{method_name}.{position} uses python-pickle-default; this method can run in Python-only mode but is not a fastdb-first portable CRM payload.'
            ),
        }
    payload_abi_id = wire_ref.get('id')
    if wire_ref.get('kind') != 'codec_ref' or not isinstance(payload_abi_id, str):
        return {
            **base,
            'code': 'non_codec_wire_ref',
            'message': (
                f'{method_name}.{position} does not use a PayloadAbiRef wire identity and cannot be treated as a fastdb-first portable CRM payload.'
            ),
        }
    if payload_abi_id not in _FASTDB_FIRST_PORTABLE_PAYLOAD_ABI_IDS:
        return {
            **base,
            'code': 'non_fastdb_portable_payload_abi',
            'payload_abi_id': payload_abi_id,
            'message': (
                f'{method_name}.{position} uses payload ABI {payload_abi_id}; strict fastdb-first CRM workflows require FastDB call-db PayloadAbiRef values or no payload.'
            ),
        }
    return None


def _canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(',', ':'))
