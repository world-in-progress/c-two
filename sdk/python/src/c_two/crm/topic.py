"""Resource-owned topic declarations on CRM contracts (ADR 0005).

A topic is declared with ``@cc.topic`` on a ``@cc.crm`` contract class. It
takes zero parameters, has no access category, and publishes exactly one
output value per frame. Topics are not RPC methods: resources never implement
them, and they are excluded from the method table and conformance checks.
"""
from __future__ import annotations

import inspect
from dataclasses import dataclass
from typing import Any, Callable

from .payload_plan import (
    PayloadBinding,
    PayloadPlanKind,
    _payload_type,
    fastdb_payload_binding,
    python_pickle_output_binding,
)

_TOPIC_ATTR = '__cc_topic__'


@dataclass(frozen=True, slots=True)
class TopicInfo:
    """One declared topic and its output payload binding."""

    name: str
    binding: PayloadBinding


def _topic_parameters(func: Callable) -> list[inspect.Parameter]:
    parameters = list(inspect.signature(func).parameters.values())
    if parameters and parameters[0].name in {'self', 'cls'}:
        parameters = parameters[1:]
    return parameters


def build_topic_output_binding(func: Callable) -> PayloadBinding:
    """Resolve one topic function's output binding and validate its shape.

    Runs at CRM class decoration, so an invalid topic declaration fails before
    any resource is registered. ``@cc.transfer(output=...)`` selects a portable
    FastDB payload; every other return annotation travels as Python pickle on
    the language-local contract path.
    """
    from .meta import _METHOD_ACCESS_ATTR, _SHUTDOWN_ATTR

    name = getattr(func, '__name__', '<topic>')
    if getattr(func, _SHUTDOWN_ATTR, False):
        raise TypeError(
            f'@cc.on_shutdown cannot be combined with @cc.topic on {name!r}.',
        )
    if getattr(func, _METHOD_ACCESS_ATTR, None) is not None:
        raise TypeError(
            f'@cc.topic declares no access category; remove @cc.read/@cc.write '
            f'from {name!r}.',
        )
    if getattr(func, '__cc_stream__', False):
        raise TypeError(
            f'@cc.stream cannot be combined with @cc.topic on {name!r}.',
        )
    if getattr(func, '_output_payload_binding', None) is not None:
        return func._output_payload_binding

    transfer_config = getattr(func, '__cc_transfer__', None) or {}
    if transfer_config.get('input') is not None:
        raise TypeError(
            f'@cc.topic takes no input; @cc.transfer(input=...) is invalid '
            f'on {name!r}.',
        )
    output_spec = transfer_config.get('output')

    try:
        hints = _resolved_hints(func)
    except (NameError, TypeError) as exc:
        raise TypeError(f'{name} contains unsupported annotations: {exc}') from exc
    return_annotation = hints.get('return', inspect.Signature.empty)

    if output_spec is not None:
        payload_type = _payload_type(required=True)
        if return_annotation is not payload_type:
            raise TypeError(
                f'{name} must return fastdb4py.payload.Payload for an explicit '
                'portable topic output binding.',
            )
        return fastdb_payload_binding(output_spec, label=f'{name}.topic.output')

    if return_annotation in (None, type(None), inspect.Signature.empty):
        raise TypeError(
            f'@cc.topic {name!r} requires a return annotation describing the '
            'published value.',
        )
    payload_type = _payload_type()
    if payload_type is not None and return_annotation is payload_type:
        raise TypeError(
            f'{name} returns fastdb4py.payload.Payload without '
            '@cc.transfer(output=...).',
        )
    return python_pickle_output_binding(func)


def _resolved_hints(func: Callable) -> dict[str, Any]:
    from typing import get_type_hints

    try:
        return get_type_hints(func, include_extras=True)
    except (NameError, TypeError, ValueError):
        return dict(getattr(func, '__annotations__', {}))


def topic_members(crm_class: type) -> dict[str, TopicInfo]:
    """All topics declared on a CRM contract, keyed by name.

    Requires the class to have been created by ``@cc.crm`` so every topic
    function already carries its validated output binding.
    """
    members: dict[str, TopicInfo] = {}
    for name, member in inspect.getmembers(crm_class, predicate=inspect.isfunction):
        if name.startswith('_') or not getattr(member, _TOPIC_ATTR, False):
            continue
        binding = getattr(member, '_output_payload_binding', None)
        if not isinstance(binding, PayloadBinding):
            raise TypeError(
                f'Topic {name!r} on {crm_class.__name__} was not declared '
                'inside a @cc.crm contract class.',
            )
        members[name] = TopicInfo(name, binding)
    return dict(sorted(members.items()))


def topic_names(crm_class: type) -> list[str]:
    """Declared topic names in ascending order (the canonical order)."""
    return list(topic_members(crm_class))


def _build_topic_wrapper(func: Callable, binding: PayloadBinding) -> Callable:
    """Build the client-facing subscription entry for one declared topic.

    Calling the wrapped member on a client-side CRM instance attaches one
    subscription to the topic (``telemetry.state()``); ``cc.hold(...)`` passes
    ``_c2_buffer='hold'`` through the same ``cc.hold`` used by unary calls and
    streams, yielding ``Delivery[HeldResult[T]]``. The wrapper keeps the
    declaration markers (``__cc_topic__`` and ``_output_payload_binding``) so
    ``topic_members()``, ``rpc_method_names()`` and resource conformance
    behave exactly like they did on the plain stub.
    """
    from functools import wraps

    method_name = func.__name__

    @wraps(func)
    def invoke(self, *args, start='latest', on_lag='skip', _c2_buffer=None):
        if args:
            raise TypeError(
                f'{method_name} takes no positional arguments, got {len(args)}.',
            )
        if not hasattr(self, 'direction'):
            raise AttributeError(
                'The CRM instance does not have a "direction" attribute.',
            )
        if self.direction == '<-':
            raise TypeError(
                f'Topic {method_name!r} has no resource-side call surface; '
                'publish through registration.publisher(...) and subscribe '
                'from a client CRM instance.',
            )
        if self.direction != '->':
            raise ValueError(
                f'Invalid direction value: {self.direction}. '
                "Expected '->' or '<-'.",
            )
        client = getattr(self, 'client', None)
        if client is None:
            raise AttributeError(
                f'{type(self).__name__} has no client connection; obtain it '
                'from cc.connect(...).',
            )
        return client.subscribe(
            method_name,
            start=start,
            on_lag=on_lag,
            hold=_c2_buffer == 'hold',
        )

    # wraps copies the declaration markers from the stub; set them explicitly
    # so they survive even if the stub was decorated in an unusual order.
    invoke.__cc_topic__ = True  # noqa: SLF001
    invoke._output_payload_binding = binding  # noqa: SLF001
    return invoke
