from __future__ import annotations

import hashlib
import inspect
import re
from collections.abc import Iterable
from dataclasses import dataclass
from typing import Any

_IDENT_RE = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._:/+-]*$')
_CAPABILITY_RE = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._+-]*$')
_SHA256_RE = re.compile(r'^[0-9a-f]{64}$')


@dataclass(frozen=True)
class PayloadAbiRef:
    id: str
    version: str
    schema: str | None = None
    schema_sha256: str | None = None
    capabilities: tuple[str, ...] = ()
    media_type: str | None = None
    portable: bool = True

    def __post_init__(self) -> None:
        object.__setattr__(self, 'id', _nonempty_identity(self.id, 'id'))
        object.__setattr__(self, 'version', _nonempty_identity(self.version, 'version'))
        if self.schema is not None:
            object.__setattr__(self, 'schema', _nonempty_identity(self.schema, 'schema'))
        if self.media_type is not None:
            object.__setattr__(self, 'media_type', _nonempty_identity(self.media_type, 'media_type'))
        if self.schema_sha256 is not None:
            if not isinstance(self.schema_sha256, str) or not _SHA256_RE.match(self.schema_sha256):
                raise ValueError('schema_sha256 must be a lowercase 64-character hex digest.')
        capabilities = _normalize_capabilities(self.capabilities)
        object.__setattr__(self, 'capabilities', capabilities)
        if not isinstance(self.portable, bool):
            raise TypeError('portable must be a bool.')

    @classmethod
    def from_schema(
        cls,
        *,
        id: str,
        version: str,
        schema: str,
        schema_text: str,
        capabilities: Iterable[str] = (),
        media_type: str | None = None,
        portable: bool = True,
    ) -> 'PayloadAbiRef':
        if not isinstance(schema_text, str):
            raise TypeError('schema_text must be a string.')
        digest = hashlib.sha256(schema_text.encode()).hexdigest()
        return cls(
            id=id,
            version=version,
            schema=schema,
            schema_sha256=digest,
            capabilities=tuple(capabilities),
            media_type=media_type,
            portable=portable,
        )

    def to_wire_ref(self) -> dict[str, Any]:
        ref: dict[str, Any] = {
            'id': self.id,
            'kind': 'codec_ref',
            'portable': self.portable,
            'version': self.version,
        }
        if self.schema is not None:
            ref['schema'] = self.schema
        if self.schema_sha256 is not None:
            ref['schema_sha256'] = self.schema_sha256
        if self.capabilities:
            ref['capabilities'] = list(self.capabilities)
        if self.media_type is not None:
            ref['media_type'] = self.media_type
        return ref


@dataclass(frozen=True)
class MethodParameterShape:
    name: str
    annotation: object
    default: object = inspect.Signature.empty
    kind: str = inspect.Parameter.POSITIONAL_OR_KEYWORD.name


@dataclass(frozen=True)
class MethodPayloadAbiShape:
    method_name: str
    direction: str
    crm_namespace: str | None = None
    crm_name: str | None = None
    crm_version: str | None = None
    parameters: tuple[MethodParameterShape, ...] = ()
    return_annotation: object | None = None


def normalize_payload_abi_ref(value: PayloadAbiRef | dict[str, Any]) -> PayloadAbiRef:
    if isinstance(value, PayloadAbiRef):
        return value
    if not isinstance(value, dict):
        raise TypeError('payload_abi_ref must be a PayloadAbiRef or dict.')
    payload = dict(value)
    kind = payload.pop('kind', None)
    if kind is not None and kind != 'codec_ref':
        raise ValueError('PayloadAbiRef wire dict kind must be "codec_ref".')
    capabilities = payload.pop('capabilities', ())
    return PayloadAbiRef(capabilities=tuple(capabilities), **payload)


def _nonempty_identity(value: str, field: str) -> str:
    if not isinstance(value, str):
        raise TypeError(f'{field} must be a string.')
    stripped = value.strip()
    if not stripped:
        raise ValueError(f'{field} cannot be empty.')
    if not _IDENT_RE.match(stripped):
        raise ValueError(f'{field} contains unsupported characters.')
    return stripped


def _normalize_capabilities(values: Iterable[str]) -> tuple[str, ...]:
    if isinstance(values, str):
        raise TypeError('capabilities must be an iterable of strings.')
    seen: set[str] = set()
    normalized: list[str] = []
    for value in values:
        if not isinstance(value, str):
            raise TypeError('capabilities must contain strings.')
        stripped = value.strip()
        if not stripped:
            raise ValueError('capability cannot be empty.')
        if not _CAPABILITY_RE.match(stripped):
            raise ValueError('capability contains unsupported characters.')
        if stripped in seen:
            raise ValueError(f'duplicate capability {stripped!r}.')
        seen.add(stripped)
        normalized.append(stripped)
    return tuple(sorted(normalized))
