"""C-Two HTTP Relay — native Rust implementation.

Wraps the Rust ``c2_relay`` native extension via ``c_two._native.NativeRelay``.

Typical usage::

    from c_two.relay import NativeRelay

    relay = NativeRelay("0.0.0.0:8080")
    relay.start()
    relay.register_upstream("grid", "ipc://my_server")
    relay.stop()
"""
from __future__ import annotations

from c_two._native import NativeRelay

__all__ = ["NativeRelay"]
