from __future__ import annotations

import enum
import importlib
import json
import pickle
import sys
from collections.abc import Callable
from dataclasses import dataclass
from types import ModuleType
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from fastdb4py.payload import BuildPlan, Payload

DEFAULT_PICKLE_PROTOCOL = 4
# FDB_PAYLOAD_DIRECT_BUILD_ELIGIBLE in the FastDB 0.4.0 C ABI.
_FASTDB_DIRECT_BUILD_ELIGIBLE = 1
# Lift FastDB's default OpenOptions total-byte cap when C-Two has already
# admitted a >256 MiB payload within max_payload_size and the role budget.
_FASTDB_OPEN_OPTIONS_THRESHOLD_BYTES = 256 * 1024 * 1024

# Marker attribute distinguishing which payload hook actually failed: opening
# the FastDB payload buffer (``from_buffer``) versus verifying/decoding the
# payload content (``deserialize``). Wrappers read it to pick the precise
# error code instead of guessing from the retention mode.
DECODE_STAGE_ATTRIBUTE = '_c2_decode_stage'
DECODE_STAGE_FROM_BUFFER = 'from_buffer'
DECODE_STAGE_DESERIALIZE = 'deserialize'


def _tag_decode_stage(exc: BaseException, stage: str) -> None:
    try:
        setattr(exc, DECODE_STAGE_ATTRIBUTE, stage)
    except (AttributeError, TypeError):  # pragma: no cover - exotic builtins
        pass


def decode_stage_of(exc: BaseException) -> str:
    """The failing payload hook for one decode exception.

    Defaults to ``'deserialize'``: bindings without a buffer-opening step
    (plain pickle) always fail while decoding content.
    """
    return getattr(exc, DECODE_STAGE_ATTRIBUTE, DECODE_STAGE_DESERIALIZE)


def _fastdb_payload_module() -> ModuleType:
    try:
        return importlib.import_module("fastdb4py.payload")
    except ModuleNotFoundError as exc:
        if (exc.name or "").split(".")[0] not in {"fastdb4py", "numpy"}:
            raise
        raise ImportError(
            "FastDB payloads require the optional FastDB dependencies. "
            "Install them with: uv pip install 'c-two[fastdb]'",
        ) from exc


def _payload_type(*, required: bool = False) -> type | None:
    # A real Payload annotation already has its defining module loaded. Ordinary
    # Python annotations must not import FastDB just to test their identity.
    if required:
        return _fastdb_payload_module().Payload
    return getattr(sys.modules.get("fastdb4py.payload"), "Payload", None)


def _build_plan_type(*, required: bool = False) -> type | None:
    if required:
        return _fastdb_payload_module().BuildPlan
    return getattr(sys.modules.get("fastdb4py.payload"), "BuildPlan", None)


@dataclass(frozen=True)
class _DeferredFastDbPlan:
    plan: BuildPlan
    spec_sha256: bytes
    label: str

    def _c2_total_bytes(self) -> int:
        return self.plan.info().total_bytes

    def materialize_payload(self) -> Payload:
        module = _fastdb_payload_module()
        owner = self.plan.execute(module.BuildPolicy.ALLOW_STAGING).payload
        try:
            owner.require_spec_sha256(self.spec_sha256)
        except BaseException:
            owner.close()
            raise
        return owner

    def _c2_write_slab(self, slab) -> None:
        module = _fastdb_payload_module()
        if self.plan.info().direct_build_status == _FASTDB_DIRECT_BUILD_ELIGIBLE:
            owner = self.plan.execute(
                module.BuildPolicy.REQUIRE_DIRECT,
                backing=module.WritableBufferBacking(slab),
            ).payload
            try:
                owner.require_spec_sha256(self.spec_sha256)
            finally:
                owner.close()
            return

        with self.materialize_payload() as owner, owner.memoryview() as source:
            with memoryview(slab) as destination:
                destination[:] = source

    def _c2_materialize_bytes(self) -> bytes:
        with self.materialize_payload() as owner, owner.memoryview() as source:
            return bytes(source)

    def to_bytes(self) -> bytes:
        return self._c2_materialize_bytes()


SerializedPayload = bytes | bytearray | memoryview | _DeferredFastDbPlan


def _materialize_direct_plan(value, binding):
    if binding is None or binding.kind is not PayloadPlanKind.FASTDB:
        return value
    if isinstance(value, _DeferredFastDbPlan):
        value = value.plan
    plan_type = _build_plan_type()
    if plan_type is not None and isinstance(value, plan_type):
        return _DeferredFastDbPlan(
            value, binding.spec_sha256, binding.label,
        ).materialize_payload()
    return value


def _fastdb_input_nbytes(value) -> int | None:
    plan_type = _build_plan_type()
    if plan_type is not None and isinstance(value, plan_type):
        return value.info().total_bytes
    payload_type = _payload_type()
    if payload_type is not None and isinstance(value, payload_type):
        return value.execution_report().used_bytes
    return None


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
        return self.kind is PayloadPlanKind.FASTDB


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

    module = _fastdb_payload_module()
    CompiledSpec, Payload = module.CompiledSpec, module.Payload

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

    def serialize(value: object) -> SerializedPayload:
        if isinstance(value, Payload):
            value.require_spec_sha256(digest)
            return value.memoryview()
        if isinstance(value, module.BuildPlan):
            return _DeferredFastDbPlan(value, digest, label)
        raise TypeError(
            f"{label} requires fastdb4py.payload.Payload or BuildPlan, "
            f"got {type(value).__name__}.",
        )

    def deserialize(
        data: bytes | bytearray | memoryview | None,
    ) -> Payload:
        if data is None:
            raise TypeError(f"{label} requires payload bytes, got None.")
        try:
            # Give ExternalBytes its own memoryview when the caller has a
            # temporary one to release. Both reference the same buffer export.
            ext = module.ExternalBytes(memoryview(data) if isinstance(data, memoryview) else data)
            try:
                data_len = len(data)
                if data_len > _FASTDB_OPEN_OPTIONS_THRESHOLD_BYTES:
                    owner = Payload.open_external(
                        compiled,
                        ext,
                        module.OpenOptions(max_total_bytes=data_len),
                    )
                else:
                    owner = Payload.open_external(compiled, ext)
            finally:
                ext.close()
        except BaseException as exc:
            _tag_decode_stage(exc, DECODE_STAGE_FROM_BUFFER)
            raise
        try:
            owner.require_spec_sha256(digest)
        except BaseException as exc:
            from .transferable import _cleanup_preserving_primary

            _tag_decode_stage(exc, DECODE_STAGE_DESERIALIZE)
            _cleanup_preserving_primary(exc, owner.close)
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
