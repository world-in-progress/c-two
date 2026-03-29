"""Reply encoding and sending for the IPC v3 server.

Centralises ICRM result unpacking, error wrapping, and reply frame
construction — eliminating the triple-duplication that existed when
this logic lived in both ``Server`` methods and ``_FastDispatcher``
static helpers.
"""
from __future__ import annotations

import logging
import math
from typing import Any

from ... import error
from ..wire import (
    encode_buddy_reply_frame,
    encode_inline_reply_frame,
    encode_error_reply_frame,
    encode_buddy_chunked_reply_frame,
    encode_inline_chunked_reply_frame,
)
from .chunk import CHUNK_THRESHOLD_RATIO

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# ICRM result helpers (shared by async and fire-and-forget paths)
# ---------------------------------------------------------------------------

def unpack_icrm_result(result: Any) -> tuple[bytes, bytes]:
    """Unpack ICRM ``'<-'`` result into ``(result_bytes, error_bytes)``."""
    if isinstance(result, tuple):
        err_part = result[0] if result[0] else b''
        res_part = result[1] if len(result) > 1 and result[1] else b''
        if isinstance(err_part, memoryview):
            err_part = bytes(err_part)
        if isinstance(res_part, memoryview):
            res_part = bytes(res_part)
        return res_part, err_part
    if result is None:
        return b'', b''
    if isinstance(result, memoryview):
        return bytes(result), b''
    return result, b''


def wrap_error(exc: Exception) -> tuple[bytes, bytes]:
    """Serialize an exception into ``(b'', error_bytes)``."""
    if isinstance(exc, error.CCBaseError):
        try:
            return b'', exc.serialize()
        except Exception:
            pass
    try:
        return b'', error.CRMExecuteFunction(str(exc)).serialize()
    except Exception:
        return b'', str(exc).encode('utf-8')


# ---------------------------------------------------------------------------
# Fire-and-forget reply builders (called from worker threads)
# ---------------------------------------------------------------------------

def build_inline_reply(request_id: int, result: Any) -> bytes:
    """Build an inline reply frame from a raw ICRM result."""
    res_part, err_part = unpack_icrm_result(result)
    if err_part:
        return encode_error_reply_frame(request_id, err_part)
    return encode_inline_reply_frame(request_id, res_part)


def build_error_reply(request_id: int, exc: BaseException) -> bytes:
    """Build an error reply frame from an exception."""
    try:
        if hasattr(exc, 'serialize'):
            err_bytes = exc.serialize()
        else:
            err_bytes = str(exc).encode('utf-8')
    except Exception:
        err_bytes = b'Internal server error'
    return encode_error_reply_frame(request_id, err_bytes)


# ---------------------------------------------------------------------------
# Async reply sending (buddy / inline / chunked selection)
# ---------------------------------------------------------------------------

async def send_reply(
    conn,  # Connection
    request_id: int,
    result_bytes: bytes,
    err_bytes: bytes,
    writer,
    config,  # IPCConfig
) -> None:
    """Choose the best reply path and send the response frame(s)."""
    if err_bytes:
        frame = encode_error_reply_frame(request_id, err_bytes)
        writer.write(frame)
        await writer.drain()
        return

    data_size = len(result_bytes)

    # Chunked reply for large results.
    chunk_threshold = int(config.pool_segment_size * CHUNK_THRESHOLD_RATIO)
    if data_size > chunk_threshold and conn.chunked_capable:
        await send_chunked_reply(conn, request_id, result_bytes, writer, config)
        return

    if data_size > config.shm_threshold and conn.buddy_pool is not None:
        try:
            alloc = conn.buddy_pool.alloc(data_size)
            if alloc.is_dedicated:
                conn.buddy_pool.free_at(
                    alloc.seg_idx, alloc.offset, data_size, True,
                )
                raise MemoryError('dedicated segment, fall back to inline')

            seg_mv = conn.seg_views[alloc.seg_idx]
            if alloc.offset + data_size > len(seg_mv):
                conn.buddy_pool.free_at(
                    alloc.seg_idx, alloc.offset, data_size, alloc.is_dedicated,
                )
                raise MemoryError('reply offset OOB, fall back to inline')
            seg_mv[alloc.offset : alloc.offset + data_size] = result_bytes
            frame = encode_buddy_reply_frame(
                request_id, alloc.seg_idx, alloc.offset,
                data_size, alloc.is_dedicated,
            )
            writer.write(frame)
            if data_size > 65536:
                await writer.drain()
            return
        except Exception:
            pass  # Fall through to inline.

    frame = encode_inline_reply_frame(request_id, result_bytes)
    writer.write(frame)
    if data_size > 65536:
        await writer.drain()


async def send_chunked_reply(
    conn,  # Connection
    request_id: int,
    result_bytes: bytes,
    writer,
    config,  # IPCConfig
) -> None:
    """Send a large result as multiple chunked reply frames."""
    chunk_size = config.pool_segment_size // 2
    total_size = len(result_bytes)
    n_chunks = math.ceil(total_size / chunk_size)

    for i in range(n_chunks):
        start = i * chunk_size
        end = min(start + chunk_size, total_size)
        chunk_data = result_bytes[start:end]
        chunk_len = end - start
        is_last = (i == n_chunks - 1)

        # Try buddy allocation for each chunk.
        sent = False
        if conn.buddy_pool is not None and chunk_len > config.shm_threshold:
            try:
                alloc = conn.buddy_pool.alloc(chunk_len)
                if alloc.is_dedicated:
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, chunk_len, True,
                    )
                    raise MemoryError('dedicated')
                seg_mv = conn.seg_views[alloc.seg_idx]
                seg_mv[alloc.offset:alloc.offset + chunk_len] = chunk_data
                frame = encode_buddy_chunked_reply_frame(
                    request_id, alloc.seg_idx, alloc.offset,
                    chunk_len, alloc.is_dedicated, i, n_chunks,
                )
                writer.write(frame)
                sent = True
            except Exception:
                pass  # Fall through to inline.

        if not sent:
            if chunk_len > config.max_frame_size:
                logger.error(
                    'Conn %d: chunk %d/%d size %d exceeds max_frame_size %d '
                    'and buddy alloc failed — aborting chunked reply',
                    conn.conn_id, i, n_chunks, chunk_len,
                    config.max_frame_size,
                )
                writer.write(encode_error_reply_frame(
                    request_id,
                    b'Chunked reply failed: chunk exceeds inline limit',
                ))
                await writer.drain()
                return
            frame = encode_inline_chunked_reply_frame(
                request_id, i, n_chunks, chunk_data,
            )
            writer.write(frame)

        # Batch drain: flush every 4 chunks or on the final chunk.
        if is_last or (i + 1) % 4 == 0:
            await writer.drain()
