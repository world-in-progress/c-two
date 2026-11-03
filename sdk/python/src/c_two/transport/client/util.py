"""Direct IPC admin helpers projected through the current Core Runtime."""
from __future__ import annotations

from typing import Literal

from ..endpoint import LocalEndpointContext, _selected_context, _selection


def _session():
    from ..registry import _get_default_runtime

    return _get_default_runtime()._runtime_session  # noqa: SLF001


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
) -> Literal['initiated', 'absent', 'unconfirmed']:
    """Send a direct IPC shutdown signal to a server.

    Defaults to the current Runtime, including its code override and frozen
    context. Independent supervisors may supply a Unix root or captured context.
    Only 'initiated' confirms that shutdown was initiated; completion is
    observed through the original Host shutdown receipt. 'absent' means this
    endpoint has no listener. 'unconfirmed' means initiation was not confirmed.
    Configuration, target and timeout errors propagate to the caller.
    """
    from c_two._native import ipc_shutdown

    _selection(root, context)
    selected = (
        _selected_context(root, context)
        if root is not None or context is not None else None
    )
    if selected is not None:
        return ipc_shutdown(server_address, float(timeout), context=selected)
    return _session().shutdown_direct_ipc(server_address, float(timeout))
