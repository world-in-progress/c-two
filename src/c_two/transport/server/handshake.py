"""Handshake protocol for the IPC server.

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
    HANDSHAKE_VERSION,
    CAP_CALL,
    CAP_METHOD_IDX,
    CAP_CHUNKED,
    RouteInfo,
    MethodEntry,
    encode_server_handshake,
    decode_handshake,
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
    if version == HANDSHAKE_VERSION:
        await _handle_handshake_impl(conn, payload, writer, slots_snapshot, config)
    else:
        # Reject legacy clients — only current version supported.
        writer.close()
        await writer.wait_closed()


async def _handle_handshake_impl(
    conn: Connection,
    payload: bytes,
    writer: asyncio.StreamWriter,
    slots_snapshot: list[CRMSlot],
    config: IPCConfig,
) -> None:
    """Process handshake: open segments, negotiate caps, send ACK."""
    try:
        hs = decode_handshake(payload)
    except Exception as e:
        logger.warning('Conn %d: bad handshake: %s', conn.conn_id, e)
        return
    open_segments(conn, hs.segments, hs.prefix, config)

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

    ack_payload = encode_server_handshake(
        segments=[],
        capability_flags=cap_flags,
        routes=route_infos,
    )
    writer.write(encode_frame(0, FLAG_HANDSHAKE, ack_payload))
    await writer.drain()


def open_segments(
    conn: Connection,
    segments: list[tuple[str, int]],
    peer_prefix: str,
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
        from c_two.mem import MemPool, PoolConfig

        conn.peer_prefix = peer_prefix
        conn.buddy_pool = MemPool(PoolConfig(
            segment_size=config.pool_segment_size,
            min_block_size=4096,
            max_segments=len(segments),
            max_dedicated_segments=4,
        ))
        for name, size in segments:
            conn.buddy_pool.open_segment(name, size)
            conn.remote_segment_names.append(name)
            conn.remote_segment_sizes.append(size)

        conn.seg_views = {}
        for seg_idx in range(conn.buddy_pool.segment_count()):
            base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            conn.seg_views[seg_idx] = mv

        conn.handshake_done = True
    except Exception as e:
        logger.error('Conn %d: segment open failed: %s', conn.conn_id, e)
        conn.cleanup()


def lazy_open_peer_seg(
    conn: Connection,
    seg_idx: int,
) -> memoryview | None:
    """Lazily open a client buddy segment not seen during handshake.

    When the client's pool expands dynamically, new segments appear with
    deterministic names derived from ``peer_prefix``.  The UDS FIFO
    guarantees the SHM segment exists before we receive the data frame
    referencing it.

    Returns the memoryview for the segment, or None on failure.
    """
    if conn.buddy_pool is None or not conn.peer_prefix:
        return None
    try:
        # Deterministic name: {peer_prefix}_b{seg_idx:04x}
        name = f'{conn.peer_prefix}_b{seg_idx:04x}'
        conn.buddy_pool.open_segment(name, conn.config.pool_segment_size)
        base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
        mv = memoryview(
            (ctypes.c_char * data_size).from_address(base_addr)
        ).cast('B')
        conn.seg_views[seg_idx] = mv
        logger.debug('Conn %d: lazily opened peer segment %d (%s)',
                     conn.conn_id, seg_idx, name)
        return mv
    except Exception as e:
        logger.warning('Conn %d: failed to lazy-open seg %d: %s',
                       conn.conn_id, seg_idx, e)
        return None
