from __future__ import annotations

import inspect
from collections.abc import Mapping
from enum import Enum
from typing import Any, get_type_hints

from c_two.crm.bridge import ResourceBridge


class InputLifetime(str, Enum):
    MATERIALIZED = 'materialized'
    BORROWED = 'borrowed'


InputLifetimeLike = InputLifetime | str


def normalize_input_lifetime_map(
    input_lifetime: Mapping[str, InputLifetimeLike] | None,
    *,
    method_names: list[str] | tuple[str, ...],
) -> dict[str, InputLifetime]:
    if input_lifetime is None:
        return {}
    if not isinstance(input_lifetime, Mapping):
        raise TypeError('input_lifetime must be a mapping from method name to InputLifetime')

    known = set(method_names)
    normalized: dict[str, InputLifetime] = {}
    for method_name, lifetime in input_lifetime.items():
        if not isinstance(method_name, str):
            raise TypeError('input_lifetime keys must be method names')
        if method_name not in known:
            raise ValueError(f'input_lifetime references unknown CRM method {method_name!r}')
        try:
            normalized[method_name] = (
                lifetime if isinstance(lifetime, InputLifetime) else InputLifetime(str(lifetime))
            )
        except ValueError as exc:
            allowed = ', '.join(item.value for item in InputLifetime)
            raise ValueError(
                f'input_lifetime for {method_name!r} must be one of: {allowed}',
            ) from exc
    return normalized


def validate_input_lifetime_resource_contract(
    crm_class: type,
    resource: object,
    input_lifetime: Mapping[str, InputLifetime],
    *,
    bridge: Mapping[str, ResourceBridge],
) -> None:
    for method_name, lifetime in input_lifetime.items():
        if lifetime is not InputLifetime.BORROWED:
            continue
        bridge_plan = bridge.get(method_name)
        if bridge_plan is not None and bridge_plan.input is not None:
            raise ValueError(
                f'input_lifetime BORROWED for {method_name!r} cannot be combined with bridge.input',
            )
        _validate_borrowed_signature(crm_class, resource, method_name)


def _validate_borrowed_signature(
    crm_class: type,
    resource: object,
    method_name: str,
) -> None:
    crm_method = inspect.unwrap(getattr(crm_class, method_name))
    resource_method = inspect.unwrap(getattr(resource, method_name))
    crm_params = _callable_params(inspect.signature(crm_method), skip_self=True)
    resource_params = _callable_params(
        inspect.signature(resource_method),
        skip_self=False,
    )
    crm_positional = _borrowed_positional_params(crm_params, method_name, 'CRM')
    resource_positional = _borrowed_positional_params(
        resource_params,
        method_name,
        'resource',
    )
    if len(crm_positional) != len(resource_positional):
        raise TypeError(
            f'input_lifetime BORROWED for {method_name!r} requires CRM and '
            f'resource methods to have the same positional parameters; '
            f'got {len(crm_positional)} CRM parameters and '
            f'{len(resource_positional)} resource parameters',
        )
    crm_hints = _type_hints(crm_method, method_name, 'CRM')
    resource_hints = _type_hints(resource_method, method_name, 'resource')

    for crm_param, resource_param in zip(crm_positional, resource_positional):
        if crm_param.name not in crm_hints:
            raise TypeError(
                f'input_lifetime BORROWED for {method_name!r} requires CRM parameter '
                f'{crm_param.name!r} to be annotated',
            )
        if resource_param.name not in resource_hints:
            raise TypeError(
                f'input_lifetime BORROWED for {method_name!r} requires resource parameter '
                f'{resource_param.name!r} to use the same annotation as CRM parameter '
                f'{crm_param.name!r}',
            )
        crm_annotation = crm_hints.get(crm_param.name)
        resource_annotation = resource_hints[resource_param.name]
        if crm_annotation != resource_annotation:
            raise TypeError(
                f'input_lifetime BORROWED for {method_name!r} requires resource parameter '
                f'{resource_param.name!r} to match CRM annotation {crm_annotation!r}; '
                f'got {resource_annotation!r}',
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


def _borrowed_positional_params(
    params: list[inspect.Parameter],
    method_name: str,
    owner: str,
) -> list[inspect.Parameter]:
    allowed = {
        inspect.Parameter.POSITIONAL_ONLY,
        inspect.Parameter.POSITIONAL_OR_KEYWORD,
    }
    invalid = [param for param in params if param.kind not in allowed]
    if invalid:
        names = ', '.join(param.name for param in invalid)
        raise TypeError(
            f'input_lifetime BORROWED for {method_name!r} requires {owner} '
            f'parameters to be positional-only or positional-or-keyword; '
            f'unsupported parameters: {names}',
        )
    return params


def _type_hints(method: object, method_name: str, owner: str) -> dict[str, Any]:
    try:
        return get_type_hints(method, include_extras=True)
    except (NameError, TypeError) as exc:
        raise TypeError(
            f'{method_name} has unresolved or unsupported {owner} annotations: {exc}',
        ) from exc
