"""Direct IPC admin helpers projected through the current Core Runtime."""
from __future__ import annotations

from ..endpoint import LocalEndpointContext, _selected_context, _selection


def _session():
    from ..registry import _ProcessRegistry

    return _ProcessRegistry.get()._runtime_session  # noqa: SLF001


def _endpoint_name_from_address(
    server_address: str,
    *,
    root: str | None = None,
    context: LocalEndpointContext | None = None,
) -> str:
    return _selected_context(root, context).endpoint_name(server_address)


def ping(
    server_address: str,
    timeout: float = 0.5,
    *,
    root: str | None = None,
    context: LocalEndpointContext | None = None,
) -> bool:
    """Ping a direct IPC server to check whether it is alive.

    Defaults to the current Runtime, including its code override and frozen
    context. Independent supervisors may supply a Unix root or captured context.
    Core returns False for malformed targets; configuration and timeout errors
    propagate to the caller.
    """
    from c_two._native import ipc_ping

    _selection(root, context)
    selected = (
        _selected_context(root, context)
        if root is not None or context is not None else None
    )
    if selected is not None:
        return bool(ipc_ping(server_address, float(timeout), context=selected))
    return bool(_session().ping_direct_ipc(server_address, float(timeout)))


def shutdown(
    server_address: str,
    timeout: float = 0.5,
    *,
    root: str | None = None,
    context: LocalEndpointContext | None = None,
) -> dict[str, object]:
    """Send a direct IPC shutdown signal to a server.

    Defaults to the current Runtime, including its code override and frozen
    context. Independent supervisors may supply a Unix root or captured context.
    The acknowledgement proves initiation; observe the owner-side native
    shutdown barrier before running hooks or considering work drained.
    Core returns an unconfirmed result for malformed targets; configuration and
    timeout errors propagate to the caller.
    """
    from c_two._native import ipc_shutdown

    _selection(root, context)
    selected = (
        _selected_context(root, context)
        if root is not None or context is not None else None
    )
    if selected is not None:
        return dict(ipc_shutdown(server_address, float(timeout), context=selected))
    return dict(_session().shutdown_direct_ipc(server_address, float(timeout)))
