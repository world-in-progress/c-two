"""ICRM result unpacking helpers.

Used by NativeServerBridge to convert raw ICRM return values
into (result_bytes, error_bytes) tuples.
"""
from __future__ import annotations

from typing import Any

from ... import error


def unpack_icrm_result(result: Any) -> tuple[bytes | memoryview, bytes]:
    """Unpack ICRM ``'<-'`` result into ``(result_data, error_bytes)``.

    ``result_data`` may be ``bytes`` or ``memoryview`` — the caller
    decides whether to materialise.  Error bytes are always ``bytes``.
    """
    if isinstance(result, tuple):
        err_part = result[0] if result[0] else b''
        res_part = result[1] if len(result) > 1 and result[1] else b''
        if isinstance(err_part, memoryview):
            err_part = bytes(err_part)
        return res_part, err_part
    if result is None:
        return b'', b''
    return result, b''


def wrap_error(exc: Exception) -> tuple[bytes, bytes]:
    """Serialize an exception into ``(b'', error_bytes)``."""
    if isinstance(exc, error.CCBaseError):
        try:
            return b'', exc.serialize()
        except Exception:
            pass
    try:
        return b'', error.CRMExecuteFunction(str(exc)).serialize()
    except Exception:
        return b'', str(exc).encode('utf-8')
