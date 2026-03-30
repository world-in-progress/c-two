"""Server-side heartbeat probe.

Spawned as a per-connection asyncio task.  Periodically checks
``conn.idle_seconds()`` and sends a PING frame if the connection
has been idle longer than ``heartbeat_interval``.  If no activity
is observed within ``heartbeat_timeout``, the writer is closed to
signal the ``_handle_client`` frame loop to exit.

Lifecycle:
- Created by ``_handle_client`` after handshake completes.
- Cancelled in ``_handle_client``'s ``finally`` block on disconnect.
- Self-terminates (returns) when timeout triggers writer close.
"""
from __future__ import annotations

import asyncio
import logging

from ..ipc.frame import encode_frame
from ..ipc.msg_type import PING_BYTES
from ..protocol import FLAG_SIGNAL

from .connection import Connection

logger = logging.getLogger(__name__)


async def run_heartbeat(conn: Connection) -> None:
    """Probe client liveness via periodic PING frames.

    Returns (does not raise) when:
    - ``heartbeat_interval <= 0`` (heartbeat disabled)
    - timeout detected → writer closed
    - writer already closing

    Expected to be wrapped in ``asyncio.create_task`` and
    cancelled externally on normal disconnect.
    """
    interval = conn.config.heartbeat_interval
    timeout = conn.config.heartbeat_timeout
    if interval <= 0:
        return

    writer = conn.writer
    ping_frame = encode_frame(0, FLAG_SIGNAL, PING_BYTES)

    while True:
        await asyncio.sleep(interval)

        if writer.is_closing():
            return

        idle = conn.idle_seconds()

        if idle >= timeout:
            logger.warning(
                'Conn %d: heartbeat timeout (%.1fs idle, timeout=%.1fs)',
                conn.conn_id, idle, timeout,
            )
            writer.close()
            return

        if idle >= interval:
            writer.write(ping_frame)
            try:
                await writer.drain()
            except (ConnectionResetError, BrokenPipeError, OSError):
                return
