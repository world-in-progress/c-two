"""Handshake v5 protocol for the IPC v3 server.

Handles capability negotiation, method-table exchange, and
client SHM segment opening.  All functions are standalone —
the ``Server`` class passes connection state and a CRM slot
snapshot rather than exposing its internals.
"""
from __future__ import annotations

import asyncio
import ctypes
import logging
import re

from ..ipc.frame import IPCConfig, encode_frame
from ..protocol import (
    HANDSHAKE_V5,
    CAP_CALL,
    CAP_METHOD_IDX,
    CAP_CHUNKED,
    RouteInfo,
    MethodEntry,
    encode_v5_server_handshake,
    decode_v5_handshake,
)
from .connection import Connection, CRMSlot

logger = logging.getLogger(__name__)

_SHM_NAME_RE = re.compile(r'^/?[A-Za-z0-9_.\-]{1,255}$')
FLAG_HANDSHAKE = 1 << 2
_MAX_CLIENT_SEGMENTS = 16


async def handle_handshake(
    conn: Connection,
    payload: bytes,
    writer: asyncio.StreamWriter,
    slots_snapshot: list[CRMSlot],
    config: IPCConfig,
) -> None:
    """Dispatch handshake by version byte."""
    if len(payload) < 1:
        return
    version = payload[0]
    if version == HANDSHAKE_V5:
        await handle_v5_handshake(conn, payload, writer, slots_snapshot, config)
    else:
        # Reject legacy v4 clients — only v5 supported.
        writer.close()
        await writer.wait_closed()


async def handle_v5_handshake(
    conn: Connection,
    payload: bytes,
    writer: asyncio.StreamWriter,
    slots_snapshot: list[CRMSlot],
    config: IPCConfig,
) -> None:
    """Process a v5 handshake: open segments, negotiate caps, send ACK."""
    try:
        hs = decode_v5_handshake(payload)
    except Exception as e:
        logger.warning('Conn %d: bad v5 handshake: %s', conn.conn_id, e)
        return
    open_segments(conn, hs.segments, config)

    # Build route / method table ACK for *all* registered CRMs.
    route_infos: list[RouteInfo] = []
    for slot in slots_snapshot:
        route_infos.append(RouteInfo(
            name=slot.name,
            methods=[
                MethodEntry(name=n, index=slot.method_table.index_of(n))
                for n in slot.method_table.names()
            ],
        ))

    cap_flags = 0
    if hs.capability_flags & CAP_CALL:
        cap_flags |= CAP_CALL
    if hs.capability_flags & CAP_METHOD_IDX:
        cap_flags |= CAP_METHOD_IDX
    if hs.capability_flags & CAP_CHUNKED:
        cap_flags |= CAP_CHUNKED
        conn.chunked_capable = True

    ack_payload = encode_v5_server_handshake(
        segments=[],
        capability_flags=cap_flags,
        routes=route_infos,
    )
    writer.write(encode_frame(0, FLAG_HANDSHAKE, ack_payload))
    await writer.drain()


def open_segments(
    conn: Connection,
    segments: list[tuple[str, int]],
    config: IPCConfig,
) -> None:
    """Open the client's buddy SHM segments and cache memoryviews."""
    if len(segments) > _MAX_CLIENT_SEGMENTS:
        logger.warning('Conn %d: too many segments (%d > %d), rejecting handshake',
                       conn.conn_id, len(segments), _MAX_CLIENT_SEGMENTS)
        return
    for name, _ in segments:
        if not _SHM_NAME_RE.match(name):
            logger.warning('Conn %d: invalid segment name: %r', conn.conn_id, name)
            return
    try:
        from c_two.buddy import BuddyPoolHandle, PoolConfig

        conn.buddy_pool = BuddyPoolHandle(PoolConfig(
            segment_size=config.pool_segment_size,
            min_block_size=4096,
            max_segments=len(segments),
            max_dedicated_segments=4,
        ))
        for name, size in segments:
            conn.buddy_pool.open_segment(name, size)
            conn.remote_segment_names.append(name)
            conn.remote_segment_sizes.append(size)

        conn.seg_views = []
        for seg_idx in range(conn.buddy_pool.segment_count()):
            base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            conn.seg_views.append(mv)

        conn.handshake_done = True
    except Exception as e:
        logger.error('Conn %d: segment open failed: %s', conn.conn_id, e)
        conn.cleanup()
