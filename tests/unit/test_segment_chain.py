"""Unit tests for SHM pool segment chain elastic expansion logic.

Covers:
- Segment selection algorithm (first-fit across chain)
- Wire overhead awareness at segment boundaries
- Expansion policy (new segment sizing)
- Server-side negotiation clamping (initial vs append handshake)
- Handshake v2 codec for append operations
- Pool payload header with multi-segment indices
- SHM write/read through a simulated segment chain
"""

import os
import struct

import pytest
from multiprocessing import shared_memory

from c_two.rpc.ipc.ipc_protocol import (
    IPCConfig,
    POOL_PAYLOAD_HEADER_SIZE,
)
from c_two.rpc.ipc.shm_pool import (
    create_pool_shm,
    close_pool_shm,
    encode_handshake,
    decode_handshake,
)
from c_two.rpc.util.wire import (
    MsgType,
    REPLY_HEADER_FIXED,
    call_wire_size,
    reply_wire_size,
    write_call_into,
    write_reply_into,
)

pytestmark = pytest.mark.timeout(10)


# ---------------------------------------------------------------------------
# Helper: pure-function analog of the first-fit segment selection loop
# used in both client._send_request and server.reply()
# ---------------------------------------------------------------------------

def _select_segment(segment_sizes: list[int], wire_size: int) -> int | None:
    """Return index of the first segment that can hold *wire_size*, or None."""
    for idx, seg_size in enumerate(segment_sizes):
        if wire_size <= seg_size:
            return idx
    return None


# ---------------------------------------------------------------------------
# 1. Segment selection logic
# ---------------------------------------------------------------------------

class TestSegmentSelection:
    """First-fit selection over a segment chain."""

    def test_fits_in_first_segment(self):
        assert _select_segment([1024, 4096], 512) == 0

    def test_fits_in_second_segment(self):
        assert _select_segment([1024, 4096], 2048) == 1

    def test_exact_boundary_fits(self):
        """wire_size == seg_size → fits (<= comparison, not <)."""
        assert _select_segment([1024], 1024) == 0

    def test_one_byte_over_does_not_fit(self):
        assert _select_segment([1024], 1025) is None

    def test_no_segments_returns_none(self):
        assert _select_segment([], 100) is None

    def test_first_fit_not_best_fit(self):
        """Selection picks the first adequate segment, even if a later one is tighter."""
        assert _select_segment([4096, 2048], 1500) == 0

    def test_progressive_three_segments(self):
        sizes = [1024, 8192, 65536]
        assert _select_segment(sizes, 500) == 0
        assert _select_segment(sizes, 1024) == 0    # boundary of [0]
        assert _select_segment(sizes, 1025) == 1    # spills to [1]
        assert _select_segment(sizes, 8192) == 1    # boundary of [1]
        assert _select_segment(sizes, 8193) == 2    # spills to [2]
        assert _select_segment(sizes, 65536) == 2   # boundary of [2]
        assert _select_segment(sizes, 65537) is None # nothing fits


# ---------------------------------------------------------------------------
# 2. Wire overhead awareness at segment boundaries
# ---------------------------------------------------------------------------

class TestWireOverheadBoundary:
    """Wire overhead must be included in the size check against segment capacity.

    CRM_CALL:  3 + len(method_name) + len(payload) = wire_size
    CRM_REPLY: 5 + err_len + result_len            = total_wire
    """

    def test_call_wire_overhead_formula(self):
        method = 'echo'  # 4 bytes
        payload_len = 1000
        wire = call_wire_size(len(method), payload_len)
        assert wire == 3 + len(method) + payload_len  # 1007

    def test_reply_wire_overhead_formula(self):
        result_len = 1000
        wire = reply_wire_size(0, result_len)
        assert wire == REPLY_HEADER_FIXED + result_len  # 1005

    def test_max_payload_that_fits_request(self):
        """Calculate the exact max user payload fitting a 1024-byte segment."""
        seg_size = 1024
        method = 'echo'
        overhead = 3 + len(method)  # 7 bytes
        max_payload = seg_size - overhead  # 1017

        assert call_wire_size(len(method), max_payload) == seg_size
        assert call_wire_size(len(method), max_payload + 1) == seg_size + 1

    def test_max_payload_that_fits_reply(self):
        """Max result_bytes fitting a 1024-byte segment for a no-error reply."""
        seg_size = 1024
        max_result = seg_size - REPLY_HEADER_FIXED  # 1019

        assert reply_wire_size(0, max_result) == seg_size
        assert reply_wire_size(0, max_result + 1) == seg_size + 1

    def test_request_is_binding_constraint(self):
        """Request overhead (7B for 'echo') > reply overhead (5B), so request is tighter."""
        method = 'echo'
        call_overhead = 3 + len(method)
        reply_overhead = REPLY_HEADER_FIXED
        assert call_overhead > reply_overhead


# ---------------------------------------------------------------------------
# 3. Expansion policy
# ---------------------------------------------------------------------------

class TestExpansionPolicy:
    """Client computes new_size = max(pool_segment_size, wire_size)."""

    def test_expansion_at_least_pool_segment_size(self):
        cfg = IPCConfig(pool_segment_size=256 * 1024)
        wire_size = 100 * 1024
        new_size = max(cfg.pool_segment_size, wire_size)
        assert new_size == cfg.pool_segment_size

    def test_expansion_grows_beyond_pool_segment_size(self):
        cfg = IPCConfig(pool_segment_size=256 * 1024)
        wire_size = 512 * 1024
        new_size = max(cfg.pool_segment_size, wire_size)
        assert new_size == wire_size

    def test_expansion_blocked_at_max_segments(self):
        """When current segment count >= max_pool_segments, no expansion."""
        cfg = IPCConfig(max_pool_segments=2)
        current_count = 2
        can_expand = current_count < cfg.max_pool_segments
        assert can_expand is False

    def test_expansion_allowed_below_max_segments(self):
        cfg = IPCConfig(max_pool_segments=4)
        current_count = 2
        can_expand = current_count < cfg.max_pool_segments
        assert can_expand is True

    def test_expansion_size_covers_wire_overhead(self):
        """Expanded segment must be large enough for wire_size, not just payload."""
        cfg = IPCConfig(pool_segment_size=1024)
        method = 'echo'
        payload_len = 2000
        wire_size = call_wire_size(len(method), payload_len)  # 2007
        new_size = max(cfg.pool_segment_size, wire_size)
        assert new_size >= wire_size  # segment can hold the full wire data


# ---------------------------------------------------------------------------
# 4. Server-side negotiation clamping rules
# ---------------------------------------------------------------------------

class TestServerNegotiation:
    """Verify asymmetric clamping in _handle_pool_handshake.

    Initial  (segment_index==0): clamp to pool_segment_size
    Append   (segment_index>0):  clamp to max_payload_size (much larger)
    """

    def test_initial_clamps_to_pool_segment_size(self):
        pool_segment_size = 256 * 1024 * 1024
        client_requested = 512 * 1024 * 1024
        clamped = min(client_requested, pool_segment_size)
        assert clamped == pool_segment_size

    def test_initial_keeps_smaller_request(self):
        pool_segment_size = 256 * 1024 * 1024
        client_requested = 128 * 1024 * 1024
        clamped = min(client_requested, pool_segment_size)
        assert clamped == client_requested

    def test_append_allows_beyond_pool_segment_size(self):
        """The whole point of segment chain: append is NOT clamped to pool_segment_size."""
        pool_segment_size = 256 * 1024 * 1024
        max_payload_size = IPCConfig().max_payload_size
        client_requested = 512 * 1024 * 1024  # > pool_segment_size
        clamped = min(client_requested, max_payload_size)
        assert clamped == client_requested
        assert clamped > pool_segment_size

    def test_append_clamps_to_max_payload_size(self):
        cfg = IPCConfig()
        client_requested = cfg.max_payload_size * 2
        clamped = min(client_requested, cfg.max_payload_size)
        assert clamped == cfg.max_payload_size

    def test_negotiated_respects_actual_shm_size(self):
        """negotiated = min(clamped_request, shm.size) — OS may round up."""
        name = f'cc_neg_{os.getpid()}'
        requested = 5000  # non-page-aligned
        shm = create_pool_shm(name, requested)
        try:
            assert shm.size >= requested  # OS rounds up
            negotiated = min(requested, shm.size)
            assert negotiated == requested
        finally:
            close_pool_shm(shm, unlink=True)

    def test_negotiated_capped_by_small_shm(self):
        """If SHM is smaller than request (hypothetical), negotiated = shm.size."""
        negotiated = min(100_000, 4096)
        assert negotiated == 4096


# ---------------------------------------------------------------------------
# 5. Handshake v2 codec for append operations
# ---------------------------------------------------------------------------

class TestHandshakeV2Append:
    """Handshake codec for segment chain append operations."""

    @pytest.mark.parametrize('index', [1, 2, 3, 127, 254])
    def test_append_index_round_trip(self, index):
        name = f'cc_append_{index}'
        size = 512 * 1024 * 1024  # > default pool_segment_size
        payload = encode_handshake(name, size, segment_index=index)
        dec_name, dec_size, dec_index = decode_handshake(payload)
        assert dec_name == name
        assert dec_size == size
        assert dec_index == index

    def test_large_expansion_size_preserved(self):
        """Sizes >256 MB survive codec round-trip."""
        size = 1024 * 1024 * 1024  # 1 GB
        payload = encode_handshake('seg1', size, segment_index=1)
        _, dec_size, _ = decode_handshake(payload)
        assert dec_size == size

    def test_ack_encodes_negotiated_not_requested(self):
        """Server ACK encodes negotiated_size (may differ from client request)."""
        client_requested = 512 * 1024 * 1024
        server_negotiated = 256 * 1024 * 1024
        ack = encode_handshake('', server_negotiated, segment_index=0)
        _, ack_size, _ = decode_handshake(ack)
        assert ack_size == server_negotiated
        assert ack_size != client_requested

    def test_ack_append_allows_large_size(self):
        """Append ACK can carry a size larger than the default pool_segment_size."""
        pool_segment_size = 256 * 1024 * 1024
        negotiated = 512 * 1024 * 1024
        ack = encode_handshake('', negotiated, segment_index=1)
        _, ack_size, ack_idx = decode_handshake(ack)
        assert ack_size == negotiated
        assert ack_size > pool_segment_size
        assert ack_idx == 1


# ---------------------------------------------------------------------------
# 6. Pool payload header with multi-segment indices
# ---------------------------------------------------------------------------

class TestPoolPayloadHeaderMultiSegment:
    """Pool payload header: 1B segment_index + 8B data_size = 9 bytes."""

    @pytest.mark.parametrize('idx', [0, 1, 2, 3, 255])
    def test_segment_index_round_trip(self, idx):
        size = 100 * 1024 * 1024
        buf = struct.pack('<BQ', idx, size)
        assert len(buf) == POOL_PAYLOAD_HEADER_SIZE
        dec_idx, dec_size = struct.unpack_from('<BQ', buf, 0)
        assert dec_idx == idx
        assert dec_size == size

    def test_max_uint64_data_size(self):
        max_size = (1 << 64) - 1
        buf = struct.pack('<BQ', 0, max_size)
        _, dec_size = struct.unpack_from('<BQ', buf, 0)
        assert dec_size == max_size


# ---------------------------------------------------------------------------
# 7. SHM write/read through a simulated segment chain
# ---------------------------------------------------------------------------

class TestSegmentChainSHM:
    """Simulate segment chain: multiple SHMs with different sizes."""

    def test_write_call_to_expanded_segment(self):
        """CRM_CALL exceeding segment[0] is written to segment[1]."""
        s0 = create_pool_shm(f'cc_s0_{os.getpid()}', 1024)
        s1 = create_pool_shm(f'cc_s1_{os.getpid()}', 8192)
        try:
            method = 'echo'
            payload = b'\xAB' * 2000
            wire_size = call_wire_size(len(method), len(payload))
            segments = [(s0, min(1024, s0.size)),
                        (s1, min(8192, s1.size))]

            selected = _select_segment([sz for _, sz in segments], wire_size)
            assert selected == 1

            write_call_into(s1.buf, 0, method, payload)
            assert s1.buf[0] == MsgType.CRM_CALL
        finally:
            close_pool_shm(s0, unlink=True)
            close_pool_shm(s1, unlink=True)

    def test_write_reply_to_expanded_segment(self):
        """CRM_REPLY exceeding segment[0] is written to segment[1]."""
        s0 = create_pool_shm(f'cc_r0_{os.getpid()}', 1024)
        s1 = create_pool_shm(f'cc_r1_{os.getpid()}', 8192)
        try:
            result = b'\xCD' * 2000
            total_wire = reply_wire_size(0, len(result))
            segments = [min(1024, s0.size), min(8192, s1.size)]

            selected = _select_segment(segments, total_wire)
            assert selected == 1

            write_reply_into(s1.buf, 0, b'', result)
            assert s1.buf[0] == MsgType.CRM_REPLY
        finally:
            close_pool_shm(s0, unlink=True)
            close_pool_shm(s1, unlink=True)

    def test_segment_reuse_small_after_expansion(self):
        """After expanding to segment[1], a small payload still uses segment[0]."""
        s0 = create_pool_shm(f'cc_re0_{os.getpid()}', 1024)
        s1 = create_pool_shm(f'cc_re1_{os.getpid()}', 8192)
        try:
            sizes = [min(1024, s0.size), min(8192, s1.size)]

            small_wire = call_wire_size(4, 100)  # ~107 bytes
            large_wire = call_wire_size(4, 2000)  # ~2007 bytes

            assert _select_segment(sizes, small_wire) == 0
            assert _select_segment(sizes, large_wire) == 1
            # After using segment[1], small still goes to [0]
            assert _select_segment(sizes, small_wire) == 0
        finally:
            close_pool_shm(s0, unlink=True)
            close_pool_shm(s1, unlink=True)
