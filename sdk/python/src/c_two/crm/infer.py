from __future__ import annotations

import inspect
from collections.abc import Sequence
from typing import Any, get_type_hints

from .meta import _METHOD_ACCESS_ATTR, crm

_TRANSFER_ATTR = '__cc_transfer__'
_RPC_METHODS_ATTR = '__cc_rpc_methods__'


def infer_crm_from_resource(
    resource_cls: type,
    *,
    namespace: str,
    version: str,
    methods: Sequence[str],
    name: str | None = None,
) -> type:
    if not isinstance(resource_cls, type):
        raise TypeError('resource_cls must be a class.')
    method_names = _normalize_methods(methods)
    crm_name = name or f'{resource_cls.__name__}CRM'
    attrs: dict[str, Any] = {
        '__module__': resource_cls.__module__,
        '__qualname__': crm_name,
        '__doc__': f'CRM projection inferred from {resource_cls.__module__}.{resource_cls.__qualname__}.',
        '__cc_inferred_from__': f'{resource_cls.__module__}:{resource_cls.__qualname__}',
        _RPC_METHODS_ATTR: tuple(method_names),
    }
    for method_name in method_names:
        source = _resource_method(resource_cls, method_name)
        _validate_signature(source, method_name)
        attrs[method_name] = _stub_from_method(source, crm_name, method_name)
    return crm(namespace=namespace, version=version)(type(crm_name, (), attrs))


def _normalize_methods(methods: Sequence[str]) -> list[str]:
    if isinstance(methods, (str, bytes)) or methods is None:
        raise ValueError('methods must be a non-empty sequence of method names.')
    method_names = list(methods)
    if not method_names:
        raise ValueError('methods must contain at least one public method name.')
    seen: set[str] = set()
    for method_name in method_names:
        if not isinstance(method_name, str) or not method_name:
            raise ValueError('methods must contain non-empty method name strings.')
        if method_name.startswith('_'):
            raise ValueError(f'Cannot infer private resource method {method_name!r}.')
        if method_name in seen:
            raise ValueError(f'Duplicate resource method {method_name!r}.')
        seen.add(method_name)
    return method_names


def _resource_method(resource_cls: type, method_name: str) -> Any:
    try:
        raw = inspect.getattr_static(resource_cls, method_name)
    except AttributeError as exc:
        raise ValueError(
            f'{resource_cls.__name__} has no resource method {method_name!r}.',
        ) from exc
    if isinstance(raw, (staticmethod, classmethod)):
        raise TypeError(
            f'{resource_cls.__name__}.{method_name} must be an instance method.',
        )
    source = getattr(resource_cls, method_name)
    if not inspect.isfunction(source):
        raise TypeError(
            f'{resource_cls.__name__}.{method_name} must be a function method.',
        )
    return source


def _validate_signature(source: Any, method_name: str) -> None:
    sig = inspect.signature(source)
    parameters = list(sig.parameters.values())
    if not parameters or parameters[0].name not in {'self', 'cls'}:
        raise TypeError(f'{method_name} must be an instance method with self.')
    type_hints = _resolved_type_hints(source, method_name)
    for parameter in parameters[1:]:
        if parameter.kind in {
            inspect.Parameter.VAR_POSITIONAL,
            inspect.Parameter.VAR_KEYWORD,
        }:
            raise TypeError(
                f'{method_name} uses varargs/kwargs parameter {parameter.name!r}; '
                'inferred CRM methods must have explicit parameters.',
            )
        if parameter.kind is inspect.Parameter.KEYWORD_ONLY:
            raise TypeError(
                f'{method_name} uses keyword-only parameter {parameter.name!r}; '
                'inferred CRM methods must be positional-call compatible.',
            )
        if parameter.name not in type_hints:
            raise TypeError(f'{method_name}.{parameter.name} is missing a type annotation.')
    if 'return' not in type_hints:
        raise TypeError(f'{method_name} is missing a return annotation.')


def _resolved_type_hints(source: Any, method_name: str) -> dict[str, Any]:
    try:
        return get_type_hints(source, include_extras=True)
    except NameError as exc:
        raise ValueError(
            f'{method_name} contains an unresolved forward reference: {exc}',
        ) from exc
    except TypeError as exc:
        raise TypeError(f'{method_name} contains an unsupported annotation: {exc}') from exc


def _stub_from_method(source: Any, crm_name: str, method_name: str) -> Any:
    sig = inspect.signature(source)
    type_hints = _resolved_type_hints(source, method_name)

    def _stub(*_args: Any, **_kwargs: Any) -> Any:
        ...

    _stub.__name__ = method_name
    _stub.__qualname__ = f'{crm_name}.{method_name}'
    _stub.__module__ = source.__module__
    _stub.__doc__ = source.__doc__
    _stub.__annotations__ = dict(type_hints)
    _stub.__signature__ = sig
    if hasattr(source, _METHOD_ACCESS_ATTR):
        setattr(_stub, _METHOD_ACCESS_ATTR, getattr(source, _METHOD_ACCESS_ATTR))
    if hasattr(source, _TRANSFER_ATTR):
        setattr(_stub, _TRANSFER_ATTR, getattr(source, _TRANSFER_ATTR))
    return _stub
