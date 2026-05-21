"""C-Two-facing FastDB ABI annotation exports."""

from __future__ import annotations

from collections.abc import Sequence
from typing import TYPE_CHECKING, TypeAlias, TypeVar

if TYPE_CHECKING:
    _T = TypeVar('_T')

    Array: TypeAlias = Sequence[_T]
    Batch: TypeAlias = Sequence[_T]
    BOOL: TypeAlias = bool
    BYTES: TypeAlias = bytes
    F32: TypeAlias = float
    F64: TypeAlias = float
    I32: TypeAlias = int
    REF: TypeAlias = object
    STR: TypeAlias = str
    U8: TypeAlias = int
    U8N: TypeAlias = int
    U16: TypeAlias = int
    U16N: TypeAlias = int
    U32: TypeAlias = int
    WSTR: TypeAlias = str

    def feature(cls: type[_T]) -> type[_T]: ...

else:
    from fastdb4py.decorator import feature
    from fastdb4py.type import (
        Array,
        BOOL,
        BYTES,
        Batch,
        F32,
        F64,
        I32,
        REF,
        STR,
        U8,
        U8N,
        U16,
        U16N,
        U32,
        WSTR,
    )

__all__ = [
    'Array',
    'BOOL',
    'BYTES',
    'Batch',
    'F32',
    'F64',
    'I32',
    'REF',
    'STR',
    'U8',
    'U8N',
    'U16',
    'U16N',
    'U32',
    'WSTR',
    'feature',
]
