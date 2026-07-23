from __future__ import annotations

import enum
import json
import pickle
from collections.abc import Callable
from dataclasses import dataclass

DEFAULT_PICKLE_PROTOCOL = 4
SerializedPayload = bytes | bytearray | memoryview


@enum.unique
class PayloadPlanKind(enum.Enum):
    NO_PAYLOAD = "no_payload"
    FASTDB = "fastdb"
    PYTHON_PICKLE = "python_pickle"


@dataclass(frozen=True)
class PayloadBinding:
    kind: PayloadPlanKind
    serialize: Callable[..., SerializedPayload] | None = None
    deserialize: Callable[[bytes | bytearray | memoryview | None], object] | None = None
    invalidate: Callable[[object], None] | None = None
    spec: object | None = None
    spec_sha256: bytes | None = None
    compiled_spec: object | None = None
    label: str = ""

    def __post_init__(self) -> None:
        kind = self.kind
        if not isinstance(kind, PayloadPlanKind):
            kind = PayloadPlanKind(kind)
            object.__setattr__(self, "kind", kind)

        if kind is PayloadPlanKind.NO_PAYLOAD:
            if any(
                value is not None
                for value in (
                    self.serialize,
                    self.deserialize,
                    self.invalidate,
                    self.spec,
                    self.spec_sha256,
                    self.compiled_spec,
                )
            ):
                raise ValueError("NO_PAYLOAD bindings cannot carry payload behavior.")
            return

        if self.serialize is None or self.deserialize is None:
            raise ValueError(
                f"{kind.value} bindings must define serialize and deserialize hooks.",
            )

        if kind is PayloadPlanKind.FASTDB:
            if self.invalidate is None:
                raise ValueError("FASTDB bindings must define owner invalidation.")
            if self.spec is None or self.compiled_spec is None:
                raise ValueError("FASTDB bindings must carry a Core-compiled spec.")
            digest = self.spec_sha256
            if not isinstance(digest, bytes) or len(digest) != 32:
                raise ValueError(
                    "FASTDB bindings must carry the 32-byte Core spec digest.",
                )
            return

        if any(
            value is not None
            for value in (
                self.invalidate,
                self.spec,
                self.spec_sha256,
                self.compiled_spec,
            )
        ):
            raise ValueError(
                "PYTHON_PICKLE bindings cannot carry FastDB owner state.",
            )

    @property
    def supports_scoped_owner(self) -> bool:
        return (
            self.kind is PayloadPlanKind.FASTDB
            and self.deserialize is not None
            and self.invalidate is not None
        )


def no_payload_binding() -> PayloadBinding:
    return PayloadBinding(kind=PayloadPlanKind.NO_PAYLOAD)


def python_pickle_input_binding(func: Callable[..., object]) -> PayloadBinding:
    return PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=_pickle_serialize_values,
        deserialize=_pickle_deserialize_value,
        label=f"{func.__name__}.input.python_pickle",
    )


def python_pickle_output_binding(func: Callable[..., object]) -> PayloadBinding:
    return PayloadBinding(
        kind=PayloadPlanKind.PYTHON_PICKLE,
        serialize=_pickle_serialize_values,
        deserialize=_pickle_deserialize_value,
        label=f"{func.__name__}.output.python_pickle",
    )


def fastdb_payload_binding(
    spec_source: object,
    *,
    label: str,
) -> PayloadBinding:
    """Compile an opaque nested value through the official FastDB projection."""

    from fastdb4py.payload import CompiledSpec, Payload

    if isinstance(spec_source, CompiledSpec):
        compiled = spec_source.clone()
    else:
        compiled = CompiledSpec.compile(_spec_source_bytes(spec_source))

    try:
        canonical_spec = json.loads(compiled.canonical_json())
        digest = compiled.sha256()
    except BaseException:
        compiled.close()
        raise

    def serialize(value: object) -> bytes:
        if not isinstance(value, Payload):
            raise TypeError(
                f"{label} requires fastdb4py.payload.Payload, "
                f"got {type(value).__name__}.",
            )
        value.require_spec_sha256(digest)
        return value.binary_bytes()

    def deserialize(
        data: bytes | bytearray | memoryview | None,
    ) -> Payload:
        if data is None:
            raise TypeError(f"{label} requires payload bytes, got None.")
        owner = Payload.open_copy(compiled, bytes(data))
        try:
            owner.require_spec_sha256(digest)
        except BaseException:
            owner.close()
            raise
        return owner

    def invalidate(value: object) -> None:
        if not isinstance(value, Payload):
            raise TypeError(
                f"{label} can invalidate only fastdb4py.payload.Payload, "
                f"got {type(value).__name__}.",
            )
        value.invalidate()

    return PayloadBinding(
        kind=PayloadPlanKind.FASTDB,
        serialize=serialize,
        deserialize=deserialize,
        invalidate=invalidate,
        spec=canonical_spec,
        spec_sha256=digest,
        compiled_spec=compiled,
        label=label,
    )


def _spec_source_bytes(spec_source: object) -> bytes:
    if isinstance(spec_source, bytes):
        return spec_source
    if isinstance(spec_source, (bytearray, memoryview)):
        return bytes(spec_source)
    if isinstance(spec_source, str):
        return spec_source.encode()
    try:
        return json.dumps(
            spec_source,
            ensure_ascii=False,
            allow_nan=False,
            separators=(",", ":"),
        ).encode()
    except (TypeError, ValueError) as exc:
        raise TypeError(
            "FastDB payload specs must be JSON values, JSON text/bytes, "
            "or fastdb4py.payload.CompiledSpec.",
        ) from exc


def _pickle_serialize_values(*values: object) -> bytes:
    value = values[0] if len(values) == 1 else values
    return pickle.dumps(value, protocol=DEFAULT_PICKLE_PROTOCOL)


def _pickle_deserialize_value(
    data: bytes | bytearray | memoryview | None,
) -> object:
    if data is None or len(data) == 0:
        return None
    return pickle.loads(data)
