"""Per-connection and per-CRM state containers.

Extracted from ``server.core`` to break the hub dependency — every other
server sub-module imports data types from here rather than from the
1 500-line ``core.py``.
"""
from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass, field
from typing import Any

from ...crm.meta import MethodAccess, get_method_access
from ..ipc.frame import IPCConfig
from ..wire import MethodTable, U16_LE
from .scheduler import Scheduler


# ---------------------------------------------------------------------------
# Inline call control parser (pure function)
# ---------------------------------------------------------------------------

def parse_call_inline(
    payload: bytes | memoryview,
) -> tuple[bytes, int, int]:
    """Parse call control from an inline (non-buddy) payload.

    Returns ``(route_key_bytes, method_idx, args_start_offset)``.

    Raises ``struct.error`` or ``IndexError`` on malformed data.
    This is the **single source of truth** for inline call control
    parsing — used by both the fast dispatch path and ``_handle_call``.
    """
    name_len = payload[0]
    if name_len:
        route_key = payload[1:1 + name_len]
        method_idx = U16_LE.unpack_from(payload, 1 + name_len)[0]
        args_start = 3 + name_len
    else:
        route_key = b''
        method_idx = U16_LE.unpack_from(payload, 1)[0]
        args_start = 3
    return route_key, method_idx, args_start


# ---------------------------------------------------------------------------
# Per-connection state
# ---------------------------------------------------------------------------

@dataclass
class Connection:
    """Per-client connection state."""

    conn_id: int
    writer: asyncio.StreamWriter
    config: IPCConfig
    buddy_pool: object = None          # MemPool
    seg_views: dict[int, memoryview] = field(default_factory=dict)
    remote_segment_names: list[str] = field(default_factory=list)
    remote_segment_sizes: list[int] = field(default_factory=list)
    peer_prefix: str = ''
    handshake_done: bool = False
    chunked_capable: bool = False
    last_activity: float = field(default_factory=time.monotonic)
    # In-flight submit_inline request counter.  Incremented on the event
    # loop thread before submitting; decremented by worker threads via
    # call_soon_threadsafe.  The _idle event is set when the count drops
    # to zero, allowing _handle_client's finally block to wait for all
    # pending replies before closing the writer.
    _inflight: int = field(default=0, repr=False)
    _idle: asyncio.Event | None = field(default=None, repr=False)

    def touch(self) -> None:
        """Update last_activity to current time."""
        self.last_activity = time.monotonic()

    def idle_seconds(self) -> float:
        """Seconds since last activity."""
        return time.monotonic() - self.last_activity

    def init_flight_tracking(self, loop: asyncio.AbstractEventLoop) -> None:
        self._idle = asyncio.Event()
        self._idle.set()  # starts idle
        self._loop = loop

    def flight_inc(self) -> None:
        """Mark a submit_inline request as in-flight (call from event loop)."""
        self._inflight += 1
        if self._idle is not None and self._idle.is_set():
            self._idle.clear()

    def flight_dec(self) -> None:
        """Mark a submit_inline request as completed (call from event loop via call_soon_threadsafe)."""
        self._inflight -= 1
        if self._inflight <= 0:
            self._inflight = 0
            if self._idle is not None:
                self._idle.set()

    async def wait_idle(self) -> None:
        """Wait until all in-flight submit_inline requests complete."""
        if self._idle is not None and not self._idle.is_set():
            await self._idle.wait()

    def cleanup(self) -> None:
        self.seg_views = {}
        if self.buddy_pool is not None:
            try:
                self.buddy_pool.destroy()
            except Exception:
                pass
            self.buddy_pool = None


# ---------------------------------------------------------------------------
# Per-CRM routing slot
# ---------------------------------------------------------------------------

@dataclass
class CRMSlot:
    """Per-CRM registration in the server, keyed by routing name."""

    name: str
    icrm: object
    method_table: MethodTable
    scheduler: Scheduler
    methods: list[str]
    shutdown_method: str | None = None
    # Pre-built dispatch table: method_name → (callable, MethodAccess).
    _dispatch_table: dict[str, tuple[Any, MethodAccess]] = field(
        default_factory=dict, repr=False,
    )

    def build_dispatch_table(self) -> None:
        """Populate the dispatch table from the ICRM instance.

        Skips the ``@cc.on_shutdown`` method (not RPC-callable).
        """
        for name in self.methods:
            if name == self.shutdown_method:
                continue
            method = getattr(self.icrm, name, None)
            if method is not None:
                self._dispatch_table[name] = (method, get_method_access(method))
