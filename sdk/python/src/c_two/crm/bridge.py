from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from functools import wraps
from typing import Any


@dataclass(frozen=True)
class ResourceBridge:
    input: Callable[..., Any] | None = None
    output: Callable[[Any], Any] | None = None

    def __post_init__(self) -> None:
        if self.input is not None and not callable(self.input):
            raise TypeError('bridge input must be callable.')
        if self.output is not None and not callable(self.output):
            raise TypeError('bridge output must be callable.')

    def input_args(self, args: tuple[Any, ...]) -> tuple[Any, ...]:
        if self.input is None:
            return args
        return _as_args(self.input(*args))

    def output_value(self, value: Any) -> Any:
        if self.output is None:
            return value
        return self.output(value)


def bridge(
    *,
    input: Callable[..., Any] | None = None,
    output: Callable[[Any], Any] | None = None,
) -> ResourceBridge:
    if input is not None and not callable(input):
        raise TypeError('bridge input must be callable.')
    if output is not None and not callable(output):
        raise TypeError('bridge output must be callable.')
    return ResourceBridge(input=input, output=output)


def normalize_bridge_map(
    bridge_map: Mapping[str, ResourceBridge | Mapping[str, Any]] | None,
    *,
    method_names: list[str] | tuple[str, ...],
) -> dict[str, ResourceBridge]:
    if bridge_map is None:
        return {}
    if not isinstance(bridge_map, Mapping):
        raise TypeError('bridge must be a mapping from CRM method name to cc.bridge(...).')
    valid_methods = set(method_names)
    normalized: dict[str, ResourceBridge] = {}
    for method_name, value in bridge_map.items():
        if not isinstance(method_name, str):
            raise TypeError('bridge method names must be strings.')
        if method_name not in valid_methods:
            raise TypeError(f'unknown bridge method {method_name!r}.')
        normalized[method_name] = _coerce_bridge(value)
    return normalized


def wrap_resource(resource: object, bridges: Mapping[str, ResourceBridge]) -> object:
    if not bridges:
        return resource
    return _BridgedResource(resource, dict(bridges))


class _BridgedResource:
    def __init__(self, resource: object, bridges: dict[str, ResourceBridge]) -> None:
        self._resource = resource
        self._bridges = bridges

    def __getattr__(self, name: str) -> Any:
        target = getattr(self._resource, name)
        bridge_plan = self._bridges.get(name)
        if bridge_plan is None or not callable(target):
            return target

        @wraps(target)
        def bridged_method(*args: Any, **kwargs: Any) -> Any:
            if kwargs:
                raise TypeError('bridged resource methods currently accept positional CRM arguments only.')
            resource_args = bridge_plan.input_args(tuple(args))
            result = target(*resource_args)
            return bridge_plan.output_value(result)

        return bridged_method


def _coerce_bridge(value: ResourceBridge | Mapping[str, Any]) -> ResourceBridge:
    if isinstance(value, ResourceBridge):
        return value
    if isinstance(value, Mapping):
        return bridge(input=value.get('input'), output=value.get('output'))
    raise TypeError('bridge entries must be created by cc.bridge(...).')


def _as_args(value: Any) -> tuple[Any, ...]:
    if value is None:
        return ()
    if isinstance(value, tuple):
        return value
    return (value,)
