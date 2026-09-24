"""IPC utility functions backed by Rust c2-ipc control helpers."""
from __future__ import annotations


def _endpoint_name_from_address(server_address: str) -> str:
    from c_two._native import ipc_endpoint_name

    return ipc_endpoint_name(server_address)


def ping(server_address: str, timeout: float = 0.5) -> bool:
    """Ping a direct IPC server to check whether it is alive."""
    from c_two._native import ipc_ping

    try:
        return bool(ipc_ping(server_address, float(timeout)))
    except ValueError as exc:
        if 'timeout' in str(exc):
            raise
        return False


def shutdown(server_address: str, timeout: float = 0.5) -> dict[str, object]:
    """Send a direct IPC shutdown signal to a server."""
    from c_two._native import ipc_shutdown

    try:
        return dict(ipc_shutdown(server_address, float(timeout)))
    except ValueError as exc:
        if 'timeout' in str(exc):
            raise
        return {
            'acknowledged': False,
            'shutdown_started': False,
            'server_stopped': False,
            'route_outcomes': [],
        }
