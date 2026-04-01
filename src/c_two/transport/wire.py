"""Wire codec — MethodTable for method name→index dispatch.

As of Phase 4, all encoding/decoding primitives live in the Rust c2-wire
crate, exposed via c_two._native.  This module retains only MethodTable
(used by NativeServerBridge for dispatch), payload_total_size (used by
proxy.py for size checks), and thin Python wrappers that preserve the
offset=0 / error_data=None defaults from the original Python API.
"""
from __future__ import annotations

from c_two._native import (  # noqa: F401 — re-export
    encode_call_control,
)
from c_two._native import (
    decode_call_control as _ffi_decode_call_control,
    encode_reply_control as _ffi_encode_reply_control,
    decode_reply_control as _ffi_decode_reply_control,
)


def decode_call_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[str, int, int]:
    """Decode call control -> (route_name, method_idx, bytes_consumed)."""
    return _ffi_decode_call_control(bytes(data), offset)


def encode_reply_control(status: int, error_data: bytes | None = None) -> bytes:
    """Encode reply control payload."""
    return _ffi_encode_reply_control(status, error_data)


def decode_reply_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[int, bytes | None, int]:
    """Decode reply control -> (status, error_data_or_none, bytes_consumed)."""
    return _ffi_decode_reply_control(bytes(data), offset)


def payload_total_size(payload) -> int:
    """Return total byte length of payload.

    payload may be bytes, memoryview, a tuple/list of such segments
    (scatter-write), or None.
    """
    if payload is None:
        return 0
    if isinstance(payload, (list, tuple)):
        return sum(len(s) for s in payload)
    return len(payload)


class MethodTable:
    """Maps method names to 2-byte indices (and back).

    Built during handshake and used per-request to replace method name
    strings with compact indices.
    """

    def __init__(self) -> None:
        self._name_to_idx: dict[str, int] = {}
        self._idx_to_name: dict[int, str] = {}

    @classmethod
    def from_methods(cls, method_names: list[str]) -> MethodTable:
        """Create a table assigning sequential indices to method names."""
        table = cls()
        for i, name in enumerate(method_names):
            table._name_to_idx[name] = i
            table._idx_to_name[i] = name
        return table

    def add(self, name: str, idx: int) -> None:
        self._name_to_idx[name] = idx
        self._idx_to_name[idx] = name

    def index_of(self, name: str) -> int:
        return self._name_to_idx[name]

    def name_of(self, idx: int) -> str:
        return self._idx_to_name[idx]

    def has_name(self, name: str) -> bool:
        return name in self._name_to_idx

    def has_index(self, idx: int) -> bool:
        return idx in self._idx_to_name

    def __len__(self) -> int:
        return len(self._name_to_idx)

    def names(self) -> list[str]:
        return list(self._name_to_idx.keys())
