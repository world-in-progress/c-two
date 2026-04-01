"""SharedClient — thin compatibility wrapper around Rust IPC client.

This module provides backward-compatible API for existing tests.
Production code uses RustClient/RustClientPool directly via proxy.py.
"""
from __future__ import annotations

from .util import ping, shutdown


class SharedClient:
    """Thin wrapper around Rust IPC client for backward compatibility.

    Production code should use RustClient via the registry/proxy.
    This wrapper exists solely to keep integration tests working during
    the migration period.

    Accepts both calling conventions:
    - Legacy: ``call(method_name, data, name=name)``
    - Rust:   ``call(route_name, method_name, data)``
    """

    def __init__(self, address: str, ipc_config=None, **kwargs):
        # ipc_config accepted for backward compat (ignored)
        self._address = address
        self._client = None
        self._default_name: str = ''

    def connect(self) -> None:
        from c_two._native import RustClient
        if self._client is None:
            self._client = RustClient(self._address)
            # Auto-discover default route name for backward compat
            names = self._client.route_names()
            if names:
                self._default_name = names[0]

    def call(self, *args, name: str = '') -> bytes:
        """Call a CRM method.

        Supports both calling conventions:
        - ``call(method_name, data, name=route)`` — legacy
        - ``call(route_name, method_name, data)``  — Rust client API
        """
        if self._client is None:
            self.connect()
        if len(args) == 3:
            route, method_name, data = args
        elif len(args) == 2:
            method_name, data = args
            route = name or self._default_name
        elif len(args) == 1:
            method_name = args[0]
            data = None
            route = name or self._default_name
        else:
            raise TypeError(
                f'call() takes 1-3 positional arguments but {len(args)} were given'
            )
        return self._client.call(route, method_name, data or b'')

    def route_names(self) -> list[str]:
        """Return route names advertised by the server."""
        if self._client is None:
            self.connect()
        return self._client.route_names()

    def terminate(self) -> None:
        self._client = None
        self._default_name = ''

    def close(self) -> None:
        self.terminate()

    @property
    def is_connected(self) -> bool:
        return self._client is not None and self._client.is_connected

    @staticmethod
    def ping(server_address: str, timeout: float = 0.5) -> bool:
        return ping(server_address, timeout)

    @staticmethod
    def shutdown(server_address: str, timeout: float = 0.5) -> bool:
        return shutdown(server_address, timeout)
