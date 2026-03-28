"""Shim — canonical location is now c_two.transport."""
from c_two.transport import __all__ as _all  # noqa: F401
from c_two.transport import __getattr__ as _transport_getattr  # noqa: F401

__all__ = _all


def __getattr__(name):
    return _transport_getattr(name)
