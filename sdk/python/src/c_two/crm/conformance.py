from __future__ import annotations

import inspect
from typing import Any, get_type_hints

from .bridge import ResourceBridge, normalize_bridge_map
from .contract import crm_contract_identity
from .methods import rpc_method_names


def validate_resource_conformance(
    crm_class: type,
    resource: object,
    *,
    bridge: dict[str, ResourceBridge] | None = None,
) -> None:
    crm_contract_identity(crm_class)
    methods = rpc_method_names(crm_class)
    bridge_map = normalize_bridge_map(bridge, method_names=methods)
    for method_name in methods:
        _validate_method(
            crm_class,
            resource,
            method_name,
            bridge=bridge_map.get(method_name),
        )


def _validate_method(
    crm_class: type,
    resource: object,
    method_name: str,
    *,
    bridge: ResourceBridge | None,
) -> None:
    resource_method = getattr(resource, method_name, None)
    if resource_method is None or not callable(resource_method):
        raise TypeError(
            f'Resource {type(resource).__name__} is missing method {method_name!r} '
            f'required by CRM {crm_class.__name__}.',
        )

    crm_method = inspect.unwrap(getattr(crm_class, method_name))
    crm_signature = inspect.signature(crm_method)
    resource_signature = inspect.signature(inspect.unwrap(resource_method))
    crm_params = _callable_params(crm_signature, skip_self=True)
    resource_params = _callable_params(resource_signature, skip_self=False)
    validate_input = bridge is None or bridge.input is None
    validate_output = bridge is None or bridge.output is None
    if validate_input:
        _validate_parameter_shape(method_name, crm_params, resource_params)
    if validate_input or validate_output:
        _validate_annotations(
            method_name,
            crm_method,
            resource_method,
            crm_params,
            resource_params,
            validate_parameters=validate_input,
            validate_return=validate_output,
        )


def _callable_params(
    signature: inspect.Signature,
    *,
    skip_self: bool,
) -> list[inspect.Parameter]:
    params = list(signature.parameters.values())
    if skip_self and params and params[0].name in {'self', 'cls'}:
        params = params[1:]
    return params


def _validate_parameter_shape(
    method_name: str,
    crm_params: list[inspect.Parameter],
    resource_params: list[inspect.Parameter],
) -> None:
    resource_positional = [
        param
        for param in resource_params
        if param.kind in {
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        }
    ]
    has_varargs = any(
        param.kind is inspect.Parameter.VAR_POSITIONAL
        for param in resource_params
    )
    required_positional = [
        param
        for param in resource_positional
        if param.default is inspect.Parameter.empty
    ]
    required_keyword_only = [
        param
        for param in resource_params
        if (
            param.kind is inspect.Parameter.KEYWORD_ONLY
            and param.default is inspect.Parameter.empty
        )
    ]

    if len(required_positional) > len(crm_params):
        raise TypeError(
            f'{method_name} parameter count mismatch: resource requires '
            f'{len(required_positional)} positional parameters but CRM exposes '
            f'{len(crm_params)}.',
        )
    if len(resource_positional) < len(crm_params) and not has_varargs:
        raise TypeError(
            f'{method_name} parameter count mismatch: resource accepts '
            f'{len(resource_positional)} positional parameters but CRM exposes '
            f'{len(crm_params)}.',
        )
    if required_keyword_only:
        names = ', '.join(param.name for param in required_keyword_only)
        raise TypeError(
            f'{method_name} has required resource keyword-only parameters '
            f'not exposed by CRM: {names}.',
        )

    for crm_param, resource_param in zip(crm_params, resource_positional):
        if (
            resource_param.kind is not inspect.Parameter.POSITIONAL_ONLY
            and crm_param.name != resource_param.name
        ):
            raise TypeError(
                f'{method_name} parameter name mismatch: CRM exposes '
                f'{crm_param.name!r} but resource uses {resource_param.name!r}.',
            )


def _validate_annotations(
    method_name: str,
    crm_method: object,
    resource_method: object,
    crm_params: list[inspect.Parameter],
    resource_params: list[inspect.Parameter],
    *,
    validate_parameters: bool,
    validate_return: bool,
) -> None:
    crm_hints = _type_hints(crm_method, method_name, 'CRM')
    resource_hints = _type_hints(resource_method, method_name, 'resource')
    resource_positional = [
        param
        for param in resource_params
        if param.kind in {
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        }
    ]

    if validate_parameters:
        for crm_param, resource_param in zip(crm_params, resource_positional):
            if resource_param.name not in resource_hints:
                continue
            crm_annotation = _normalize_annotation(crm_hints.get(crm_param.name))
            resource_annotation = _normalize_annotation(resource_hints[resource_param.name])
            if crm_annotation != resource_annotation:
                raise TypeError(
                    f'{method_name}.{crm_param.name} annotation mismatch: CRM '
                    f'{_annotation_name(crm_annotation)}, resource '
                    f'{_annotation_name(resource_annotation)}.',
                )

    if validate_return and 'return' in resource_hints:
        crm_return = _normalize_annotation(crm_hints.get('return'))
        resource_return = _normalize_annotation(resource_hints['return'])
        if crm_return != resource_return:
            raise TypeError(
                f'{method_name} return annotation mismatch: CRM '
                f'{_annotation_name(crm_return)}, resource '
                f'{_annotation_name(resource_return)}.',
            )


def _type_hints(method: object, method_name: str, owner: str) -> dict[str, Any]:
    try:
        return get_type_hints(method, include_extras=True)
    except (NameError, TypeError) as exc:
        raise TypeError(
            f'{method_name} has unresolved or unsupported {owner} annotations: {exc}',
        ) from exc


def _normalize_annotation(annotation: object) -> object:
    if annotation is None:
        return type(None)
    return annotation


def _annotation_name(annotation: object) -> str:
    if annotation is None:
        return 'missing'
    name = getattr(annotation, '__name__', None)
    if isinstance(name, str):
        return name
    return str(annotation)
