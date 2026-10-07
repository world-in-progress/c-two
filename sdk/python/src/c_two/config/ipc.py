"""Typed IPC override schemas backed by the Rust config resolver."""
from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypedDict

from .settings import C2Settings

__all__ = [
    'BaseIPCOverrides',
    'ServerIPCOverrides',
    'ClientIPCOverrides',
]


class BaseIPCOverrides(TypedDict, total=False):
    """Shared IPC code-level overrides for server and client resolution."""

    # Buddy-pool policy switch. Disabling it skips only the buddy tiers;
    # dedicated SHM, chunked transfer, and file spill stay available.
    pool_enabled: bool
    # Byte size of each SHM pool segment allocated by the buddy pool.
    pool_segment_size: int
    # Maximum number of SHM pool segments the process may create/open.
    max_pool_segments: int
    # Buddy segments mapped eagerly at connect/register (default 0 = fully
    # lazy; requires pool_enabled).
    pool_prewarm_segments: int
    # Minimum idle buddy segments retained after garbage-collection sweeps
    # (default 0 = idle pools may retire back to zero mappings).
    pool_min_retained_segments: int
    # Byte size of each segment used when reassembling chunked transfers.
    reassembly_segment_size: int
    # Maximum number of reassembly segments retained for a transfer.
    reassembly_max_segments: int
    # Maximum number of chunks tracked across active reassembly operations.
    max_total_chunks: int
    # Seconds between garbage-collection sweeps for stale chunks.
    chunk_gc_interval: float
    # Fraction of a segment at which payloads switch to chunked transfer.
    chunk_threshold_ratio: float
    # Seconds before an incomplete chunk assembly is considered stale.
    chunk_assembler_timeout: float
    # Maximum bytes accepted for one reassembled payload.
    max_reassembly_bytes: int
    # Byte size of each chunk when chunked transfer is used.
    chunk_size: int
    # Finite budget for owner-created SHM backing (buddy and dedicated),
    # including header and alignment overhead. Zero rejects positive
    # reservations in this cell; it is not unlimited.
    shm_backing_budget_bytes: int
    # Finite budget for owner-created file backing length. Zero rejects
    # positive reservations in this cell; it is not unlimited.
    file_backing_budget_bytes: int
    # Finite budget for allocated capacity of live chunk reassembly storage.
    # Zero rejects positive reservations in this cell; it is not unlimited.
    live_reassembly_budget_bytes: int


class ServerIPCOverrides(BaseIPCOverrides, total=False):
    """Server-side IPC code-level overrides."""

    # Maximum byte size of one decoded IPC frame accepted by the server.
    max_frame_size: int
    # Maximum byte size of one logical request/response payload.
    max_payload_size: int
    # Maximum number of concurrent pending server requests.
    max_pending_requests: int
    # Maximum number of actively executing remote resource callbacks.
    max_execution_workers: int
    # Seconds before unused server-side pool resources may decay.
    pool_decay_seconds: float
    # Seconds between heartbeat probes; zero disables heartbeat probes.
    heartbeat_interval: float
    # Seconds to wait for a heartbeat response before treating a peer as dead.
    heartbeat_timeout: float


class ClientIPCOverrides(BaseIPCOverrides, total=False):
    """Client-side IPC code-level overrides."""

    # Idle window before the client's own request/reassembly buddy segments
    # may be retired while the connection stays open. Zero retires idle
    # segments at the next maintenance tick; negative values are rejected.
    pool_decay_seconds: float


def _resolve_server_ipc_config(
    ipc_overrides: ServerIPCOverrides | Mapping[str, object] | None = None,
    settings: C2Settings | None = None,
) -> dict[str, Any]:
    """Resolve server IPC config through Rust from explicit overrides only."""
    native = _native_resolver()
    return dict(native.resolve_server_ipc_config(
        ipc_overrides,
        _shm_overrides(settings),
    ))


def _resolve_client_ipc_config(
    ipc_overrides: ClientIPCOverrides | Mapping[str, object] | None = None,
    settings: C2Settings | None = None,
) -> dict[str, Any]:
    """Resolve client IPC config through Rust from explicit overrides only."""
    native = _native_resolver()
    return dict(native.resolve_client_ipc_config(
        ipc_overrides,
        _shm_overrides(settings),
    ))


def _native_resolver() -> Any:
    from c_two import _native

    return _native


def _shm_overrides(settings: C2Settings | None) -> dict[str, object]:
    if settings is None:
        from .settings import settings as _settings
        settings = _settings
    if not isinstance(settings, C2Settings):
        raise TypeError('settings must be a C2Settings instance')
    return settings._shm_overrides()  # noqa: SLF001
