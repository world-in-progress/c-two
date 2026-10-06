"""IPC utility functions backed by Rust c2-ipc control helpers."""
from __future__ import annotations


def _endpoint_name_from_address(
    server_address: str,
    *,
    endpoint_protocol: str | None = None,
) -> str:
    from c_two._native import ipc_endpoint_name

    return ipc_endpoint_name(server_address, endpoint_protocol)


def ping(
    server_address: str,
    timeout: float = 0.5,
    *,
    endpoint_protocol: str | None = None,
) -> bool:
    """Ping a direct IPC server to check whether it is alive.

    ``endpoint_protocol`` is a thin passthrough to the native resolver.
    ``None`` keeps the process client IPC policy (explicit overrides,
    environment / ``.env``, then Rust defaults); ``'legacy-v1'`` or
    ``'managed-v2'`` names exactly one OS endpoint. The probe never retries
    across endpoint namespaces to find a live server.
    """
    from c_two._native import ipc_ping

    try:
        return bool(ipc_ping(server_address, float(timeout), endpoint_protocol))
    except ValueError as exc:
        if 'timeout' in str(exc) or endpoint_protocol is not None:
            raise
        return False


def shutdown(
    server_address: str,
    timeout: float = 0.5,
    *,
    endpoint_protocol: str | None = None,
) -> dict[str, object]:
    """Send a direct IPC shutdown signal to a server.

    ``endpoint_protocol`` has the same strict, no-probing meaning as
    :func:`ping`: an admin probe only stops the server on the endpoint the
    selected protocol names. A non-canonical name raises rather than being
    reported as an absent server; ``None`` (the resolved process policy) and
    an invalid address keep their historical return shapes.
    """
    from c_two._native import ipc_shutdown

    try:
        return dict(ipc_shutdown(server_address, float(timeout), endpoint_protocol))
    except ValueError as exc:
        if 'timeout' in str(exc) or endpoint_protocol is not None:
            raise
        return {
            'acknowledged': False,
            'shutdown_started': False,
            'server_stopped': False,
            'route_outcomes': [],
        }
