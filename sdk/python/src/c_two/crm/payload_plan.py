from __future__ import annotations

import enum
import json
import pickle
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from typing import Any

from ._payload_abi import PayloadAbiRef, normalize_payload_abi_ref

DEFAULT_PICKLE_PROTOCOL = 4
SerializedPayload = bytes | bytearray | memoryview


@enum.unique
class PayloadPlanKind(enum.Enum):
    NO_PAYLOAD = 'no_payload'
    FDB = 'fdb'
    PYTHON_PICKLE = 'python_pickle'


@dataclass(frozen=True)
class PayloadBinding:
    kind: PayloadPlanKind
    serialize: Callable[..., SerializedPayload] | None = None
    deserialize: Callable[[bytes | bytearray | memoryview | None], object] | None = None
    payload_abi_ref: PayloadAbiRef | dict[str, Any] | None = None
    payload_abi_artifacts: Iterable[dict[str, Any]] = ()
    view_from_buffer: Callable[[memoryview], object] | None = None
    label: str = ''

    def __post_init__(self) -> None:
        kind = self.kind
        if not isinstance(kind, PayloadPlanKind):
            kind = PayloadPlanKind(kind)
            object.__setattr__(self, 'kind', kind)

        ref = self.payload_abi_ref
        if ref is not None:
            object.__setattr__(self, 'payload_abi_ref', normalize_payload_abi_ref(ref))

        artifacts = tuple(self.payload_abi_artifacts or ())
        for artifact in artifacts:
            if not isinstance(artifact, dict):
                raise TypeError('payload ABI artifacts must be JSON objects.')
            json.dumps(artifact, sort_keys=True, separators=(',', ':'))
        object.__setattr__(self, 'payload_abi_artifacts', artifacts)

        if kind is PayloadPlanKind.NO_PAYLOAD:
            if self.serialize is not None or self.deserialize is not None:
                raise ValueError('NO_PAYLOAD bindings cannot define payload encode/decode hooks.')
            if self.payload_abi_ref is not None:
                raise ValueError('NO_PAYLOAD bindings cannot carry a PayloadAbiRef.')
            return

        if self.serialize is None or self.deserialize is None:
            raise ValueError(f'{kind.value} bindings must define serialize and deserialize hooks.')
        if kind is PayloadPlanKind.FDB and self.payload_abi_ref is None:
            raise ValueError('FDB bindings must carry a PayloadAbiRef.')
        if kind is PayloadPlanKind.PYTHON_PICKLE and self.payload_abi_ref is not None:
            raise ValueError('PYTHON_PICKLE bindings cannot carry a PayloadAbiRef.')

    @property
    def supports_retained_view(self) -> bool:
        return self.view_from_buffer is not None


def no_payload_binding() -> PayloadBinding:
    return PayloadBinding(kind=PayloadPlanKind.NO_PAYLOAD)


def python_pickle_input_binding(func: Callable[..., object]) -> PayloadBinding:
    return PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=_pickle_serialize_values,
        deserialize=_pickle_deserialize_value,
        label=f'{func.__name__}.input.python_pickle',
    )


def python_pickle_output_binding(func: Callable[..., object]) -> PayloadBinding:
    return PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=_pickle_serialize_values,
        deserialize=_pickle_deserialize_value,
        label=f'{func.__name__}.output.python_pickle',
    )


def _pickle_serialize_values(*values: object) -> bytes:
    value = values[0] if len(values) == 1 else values
    return pickle.dumps(value, protocol=DEFAULT_PICKLE_PROTOCOL)


def _pickle_deserialize_value(data: bytes | bytearray | memoryview | None) -> object:
    if data is None or len(data) == 0:
        return None
    return pickle.loads(data)
