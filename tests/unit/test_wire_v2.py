"""Unit tests for rpc_v2 wire v2 codec and protocol v3.1."""
from __future__ import annotations

import pytest

from c_two.transport.protocol import (
    FLAG_CALL_V2,
    FLAG_REPLY_V2,
    FLAG_CHUNKED,
    FLAG_CHUNK_LAST,
    HANDSHAKE_V5,
    CAP_CALL_V2,
    CAP_METHOD_IDX,
    CAP_CHUNKED,
    STATUS_SUCCESS,
    STATUS_ERROR,
    MethodEntry,
    RouteInfo,
    HandshakeV5,
    encode_v5_client_handshake,
    encode_v5_server_handshake,
    decode_v5_handshake,
)
from c_two.transport.wire import (
    CHUNK_HEADER_SIZE,
    MethodTable,
    encode_call_control,
    decode_call_control,
    encode_reply_control,
    decode_reply_control,
    encode_chunk_header,
    decode_chunk_header,
    encode_v2_buddy_call_frame,
    encode_v2_inline_call_frame,
    encode_v2_buddy_reply_frame,
    encode_v2_inline_reply_frame,
    encode_v2_error_reply_frame,
    encode_v2_buddy_chunked_call_frame,
    encode_v2_inline_chunked_call_frame,
    encode_v2_buddy_chunked_reply_frame,
    encode_v2_inline_chunked_reply_frame,
)
from c_two.transport.ipc.frame import FRAME_STRUCT, FLAG_RESPONSE
from c_two.transport.ipc.buddy import FLAG_BUDDY, BUDDY_PAYLOAD_STRUCT


# ---------------------------------------------------------------------------
# Wire v2 call control round-trip
# ---------------------------------------------------------------------------

class TestCallControl:

    def test_encode_decode_with_name(self):
        name, idx = 'hello', 42
        encoded = encode_call_control(name, idx)
        dec_name, dec_idx, consumed = decode_call_control(encoded)
        assert dec_name == name
        assert dec_idx == idx
        assert consumed == len(encoded)

    def test_encode_decode_empty_name(self):
        encoded = encode_call_control('', 7)
        name, idx, consumed = decode_call_control(encoded)
        assert name == ''
        assert idx == 7
        assert consumed == 3  # 1B name_len=0 + 2B idx

    def test_decode_at_offset(self):
        prefix = b'\xff\xfe'
        ctrl = encode_call_control('ns', 99)
        data = prefix + ctrl + b'\x00\x00'
        name, idx, consumed = decode_call_control(data, offset=len(prefix))
        assert name == 'ns'
        assert idx == 99
        assert consumed == len(ctrl)

    def test_large_name(self):
        name = 'a' * 255  # max 1-byte length
        encoded = encode_call_control(name, 0)
        dec_name, dec_idx, consumed = decode_call_control(encoded)
        assert dec_name == name
        assert dec_idx == 0


# ---------------------------------------------------------------------------
# Wire v2 reply control round-trip
# ---------------------------------------------------------------------------

class TestReplyControl:

    def test_success_no_data(self):
        encoded = encode_reply_control(STATUS_SUCCESS)
        status, err, consumed = decode_reply_control(encoded)
        assert status == STATUS_SUCCESS
        assert err is None
        assert consumed == 1

    def test_error_with_data(self):
        err_data = b'something went wrong'
        encoded = encode_reply_control(STATUS_ERROR, err_data)
        status, dec_err, consumed = decode_reply_control(encoded)
        assert status == STATUS_ERROR
        assert dec_err == err_data
        assert consumed == 1 + 4 + len(err_data)

    def test_error_empty_data(self):
        encoded = encode_reply_control(STATUS_ERROR, b'')
        # STATUS_ERROR with empty error_data → just status byte
        assert encoded == bytes([STATUS_ERROR])


# ---------------------------------------------------------------------------
# V2 frame builders
# ---------------------------------------------------------------------------

class TestV2FrameBuilders:

    def test_inline_call_frame(self):
        frame = encode_v2_inline_call_frame(42, 'test.ns', 3, b'hello')
        total_len, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 42
        assert flags & FLAG_CALL_V2
        assert not (flags & FLAG_BUDDY)
        payload = frame[16:]
        ns, idx, consumed = decode_call_control(payload)
        assert ns == 'test.ns'
        assert idx == 3
        assert payload[consumed:] == b'hello'

    def test_buddy_call_frame(self):
        frame = encode_v2_buddy_call_frame(
            request_id=7, seg_idx=0, offset=1024, data_size=8192,
            is_dedicated=False, name='ns', method_idx=1,
        )
        total_len, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 7
        assert flags & FLAG_BUDDY
        assert flags & FLAG_CALL_V2
        payload = frame[16:]
        # First 11 bytes = buddy payload
        assert len(payload) >= BUDDY_PAYLOAD_STRUCT.size
        seg, off, dsize, bflags = BUDDY_PAYLOAD_STRUCT.unpack_from(payload)
        assert seg == 0
        assert off == 1024
        assert dsize == 8192
        # After buddy payload: call control
        ctrl_off = BUDDY_PAYLOAD_STRUCT.size
        ns, idx, _ = decode_call_control(payload, ctrl_off)
        assert ns == 'ns'
        assert idx == 1

    def test_inline_reply_frame(self):
        frame = encode_v2_inline_reply_frame(99, b'result')
        total_len, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 99
        assert flags & FLAG_RESPONSE
        assert flags & FLAG_REPLY_V2
        assert not (flags & FLAG_BUDDY)
        payload = frame[16:]
        status, err, consumed = decode_reply_control(payload)
        assert status == STATUS_SUCCESS
        assert payload[consumed:] == b'result'

    def test_buddy_reply_frame(self):
        frame = encode_v2_buddy_reply_frame(
            request_id=5, seg_idx=0, offset=0, data_size=4096,
            is_dedicated=False,
        )
        total_len, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 5
        assert flags & FLAG_RESPONSE
        assert flags & FLAG_BUDDY
        assert flags & FLAG_REPLY_V2
        payload = frame[16:]
        assert len(payload) >= BUDDY_PAYLOAD_STRUCT.size + 1
        status = payload[BUDDY_PAYLOAD_STRUCT.size]
        assert status == STATUS_SUCCESS

    def test_error_reply_frame(self):
        err = b'test error'
        frame = encode_v2_error_reply_frame(88, err)
        total_len, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 88
        assert flags & FLAG_RESPONSE
        assert flags & FLAG_REPLY_V2
        assert not (flags & FLAG_BUDDY)
        payload = frame[16:]
        status, dec_err, consumed = decode_reply_control(payload)
        assert status == STATUS_ERROR
        assert dec_err == err


# ---------------------------------------------------------------------------
# MethodTable
# ---------------------------------------------------------------------------

class TestMethodTable:

    def test_from_methods(self):
        table = MethodTable.from_methods(['add', 'greeting', 'get_items'])
        assert len(table) == 3
        assert table.index_of('add') == 0
        assert table.name_of(0) == 'add'
        assert table.index_of('greeting') == 1
        assert table.has_name('get_items')
        assert table.has_index(2)
        assert not table.has_name('missing')

    def test_add_manual(self):
        table = MethodTable()
        table.add('foo', 10)
        table.add('bar', 20)
        assert table.index_of('foo') == 10
        assert table.name_of(20) == 'bar'
        assert table.names() == ['foo', 'bar']


# ---------------------------------------------------------------------------
# Handshake v5 round-trip
# ---------------------------------------------------------------------------

class TestHandshakeV5:

    def test_client_handshake_roundtrip(self):
        segments = [('seg_abc', 268435456)]
        cap = CAP_CALL_V2 | CAP_METHOD_IDX
        encoded = encode_v5_client_handshake(segments, cap)
        assert encoded[0] == HANDSHAKE_V5
        hs = decode_v5_handshake(encoded)
        assert hs.segments == segments
        assert hs.capability_flags == cap
        assert hs.routes == []  # Client doesn't send routes

    def test_server_handshake_roundtrip(self):
        segments = [('srv_seg', 134217728)]
        cap = CAP_CALL_V2 | CAP_METHOD_IDX
        route = RouteInfo(
            name='hello',
            methods=[
                MethodEntry(name='add', index=0),
                MethodEntry(name='greeting', index=1),
            ],
        )
        encoded = encode_v5_server_handshake(segments, cap, [route])
        assert encoded[0] == HANDSHAKE_V5
        hs = decode_v5_handshake(encoded)
        assert hs.segments == segments
        assert hs.capability_flags == cap
        assert len(hs.routes) == 1
        r = hs.routes[0]
        assert r.name == 'hello'
        assert len(r.methods) == 2
        assert r.method_by_name('add') == 0
        assert r.method_by_name('greeting') == 1
        assert r.method_by_index(0) == 'add'

    def test_multi_segment_multi_route(self):
        segs = [('s1', 100), ('s2', 200)]
        r1 = RouteInfo('route_a', [MethodEntry('m1', 0)])
        r2 = RouteInfo('route_b', [MethodEntry('m2', 0), MethodEntry('m3', 1)])
        encoded = encode_v5_server_handshake(segs, CAP_CALL_V2, [r1, r2])
        hs = decode_v5_handshake(encoded)
        assert len(hs.segments) == 2
        assert len(hs.routes) == 2
        assert hs.routes[0].name == 'route_a'
        assert hs.routes[1].name == 'route_b'
        assert len(hs.routes[1].methods) == 2

    def test_empty_segments(self):
        encoded = encode_v5_client_handshake([], CAP_CALL_V2)
        hs = decode_v5_handshake(encoded)
        assert hs.segments == []

    def test_bad_version_raises(self):
        bad = bytes([4, 0, 0])  # version=4, not 5
        with pytest.raises(ValueError, match='Expected handshake v5'):
            decode_v5_handshake(bad)

    def test_truncated_raises(self):
        with pytest.raises(ValueError):
            decode_v5_handshake(b'\x05')


# ---------------------------------------------------------------------------
# Chunk header codec
# ---------------------------------------------------------------------------

class TestChunkHeader:

    def test_roundtrip(self):
        encoded = encode_chunk_header(0, 8)
        idx, total, consumed = decode_chunk_header(encoded)
        assert idx == 0
        assert total == 8
        assert consumed == CHUNK_HEADER_SIZE

    def test_max_values(self):
        encoded = encode_chunk_header(65535, 65535)
        idx, total, consumed = decode_chunk_header(encoded)
        assert idx == 65535
        assert total == 65535

    def test_decode_at_offset(self):
        prefix = b'\xaa\xbb\xcc'
        hdr = encode_chunk_header(3, 10)
        data = prefix + hdr + b'\x00'
        idx, total, consumed = decode_chunk_header(data, offset=len(prefix))
        assert idx == 3
        assert total == 10
        assert consumed == CHUNK_HEADER_SIZE

    def test_decode_truncated_raises(self):
        with pytest.raises(ValueError, match='chunk header'):
            decode_chunk_header(b'\x00\x01', offset=0)  # only 2 of 4 bytes

    def test_memoryview(self):
        raw = encode_chunk_header(7, 20)
        idx, total, _ = decode_chunk_header(memoryview(raw))
        assert idx == 7
        assert total == 20


# ---------------------------------------------------------------------------
# Chunked call frame builders
# ---------------------------------------------------------------------------

class TestChunkedCallFrames:

    def test_buddy_chunked_first_chunk(self):
        """First chunk carries call control (route + method_idx)."""
        frame = encode_v2_buddy_chunked_call_frame(
            request_id=42, seg_idx=0, offset=1024, data_size=4096,
            is_dedicated=False, chunk_idx=0, total_chunks=4,
            name='myroute', method_idx=3,
        )
        _, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 42
        assert flags & FLAG_BUDDY
        assert flags & FLAG_CALL_V2
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_CHUNK_LAST)
        payload = frame[16:]
        # 11B buddy
        seg, off, dsize, bflags = BUDDY_PAYLOAD_STRUCT.unpack_from(payload)
        assert seg == 0 and off == 1024 and dsize == 4096
        # 4B chunk header
        cidx, ctotal, cconsumed = decode_chunk_header(payload, BUDDY_PAYLOAD_STRUCT.size)
        assert cidx == 0 and ctotal == 4
        # call control after chunk header
        ctrl_off = BUDDY_PAYLOAD_STRUCT.size + CHUNK_HEADER_SIZE
        name, midx, _ = decode_call_control(payload, ctrl_off)
        assert name == 'myroute'
        assert midx == 3

    def test_buddy_chunked_middle_chunk(self):
        """Middle chunk has no call control."""
        frame = encode_v2_buddy_chunked_call_frame(
            request_id=42, seg_idx=1, offset=0, data_size=8192,
            is_dedicated=False, chunk_idx=2, total_chunks=4,
        )
        _, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_CHUNK_LAST)
        payload = frame[16:]
        # buddy + chunk header only (no call control)
        expected_len = BUDDY_PAYLOAD_STRUCT.size + CHUNK_HEADER_SIZE
        assert len(payload) == expected_len

    def test_buddy_chunked_last_chunk(self):
        """Last chunk sets FLAG_CHUNK_LAST."""
        frame = encode_v2_buddy_chunked_call_frame(
            request_id=42, seg_idx=0, offset=0, data_size=2048,
            is_dedicated=False, chunk_idx=3, total_chunks=4,
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNKED
        assert flags & FLAG_CHUNK_LAST

    def test_single_chunk_degeneracy(self):
        """Single chunk (total=1): both CHUNKED and CHUNK_LAST set."""
        frame = encode_v2_buddy_chunked_call_frame(
            request_id=1, seg_idx=0, offset=0, data_size=100,
            is_dedicated=False, chunk_idx=0, total_chunks=1,
            name='ns', method_idx=0,
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNKED
        assert flags & FLAG_CHUNK_LAST
        # Also has call control (chunk_idx==0)
        payload = frame[16:]
        ctrl_off = BUDDY_PAYLOAD_STRUCT.size + CHUNK_HEADER_SIZE
        name, _, _ = decode_call_control(payload, ctrl_off)
        assert name == 'ns'

    def test_inline_chunked_first_chunk(self):
        """Inline chunked: first chunk has call control + data."""
        data = b'hello world'
        frame = encode_v2_inline_chunked_call_frame(
            request_id=99, chunk_idx=0, total_chunks=3,
            data=data, name='route', method_idx=5,
        )
        _, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 99
        assert flags & FLAG_CALL_V2
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_BUDDY)
        assert not (flags & FLAG_CHUNK_LAST)
        payload = frame[16:]
        # chunk header first
        cidx, ctotal, cc = decode_chunk_header(payload, 0)
        assert cidx == 0 and ctotal == 3
        # call control after chunk header
        name, midx, ctrl_consumed = decode_call_control(payload, cc)
        assert name == 'route' and midx == 5
        # remaining is inline data
        assert payload[cc + ctrl_consumed:] == data

    def test_inline_chunked_subsequent_chunk(self):
        """Inline chunked: subsequent chunk has no call control."""
        data = b'\xff' * 100
        frame = encode_v2_inline_chunked_call_frame(
            request_id=99, chunk_idx=1, total_chunks=3, data=data,
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_CHUNK_LAST)
        payload = frame[16:]
        cidx, ctotal, cc = decode_chunk_header(payload, 0)
        assert cidx == 1 and ctotal == 3
        # remaining is inline data (no call control)
        assert payload[cc:] == data

    def test_inline_chunked_last_chunk(self):
        frame = encode_v2_inline_chunked_call_frame(
            request_id=99, chunk_idx=2, total_chunks=3, data=b'end',
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNKED
        assert flags & FLAG_CHUNK_LAST


# ---------------------------------------------------------------------------
# Chunked reply frame builders
# ---------------------------------------------------------------------------

class TestChunkedReplyFrames:

    def test_buddy_chunked_reply_first(self):
        """First reply chunk has status byte."""
        frame = encode_v2_buddy_chunked_reply_frame(
            request_id=10, seg_idx=0, offset=0, data_size=4096,
            is_dedicated=False, chunk_idx=0, total_chunks=2,
        )
        _, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 10
        assert flags & FLAG_RESPONSE
        assert flags & FLAG_BUDDY
        assert flags & FLAG_REPLY_V2
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_CHUNK_LAST)
        payload = frame[16:]
        # buddy + chunk header + status
        cidx, _, _ = decode_chunk_header(payload, BUDDY_PAYLOAD_STRUCT.size)
        assert cidx == 0
        status_off = BUDDY_PAYLOAD_STRUCT.size + CHUNK_HEADER_SIZE
        assert payload[status_off] == STATUS_SUCCESS

    def test_buddy_chunked_reply_subsequent(self):
        """Subsequent reply chunk has no status byte."""
        frame = encode_v2_buddy_chunked_reply_frame(
            request_id=10, seg_idx=0, offset=4096, data_size=2048,
            is_dedicated=False, chunk_idx=1, total_chunks=2,
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNK_LAST
        payload = frame[16:]
        # buddy + chunk header only (no status)
        expected = BUDDY_PAYLOAD_STRUCT.size + CHUNK_HEADER_SIZE
        assert len(payload) == expected

    def test_inline_chunked_reply_first(self):
        data = b'result part 1'
        frame = encode_v2_inline_chunked_reply_frame(
            request_id=20, chunk_idx=0, total_chunks=3, data=data,
        )
        _, rid, flags = FRAME_STRUCT.unpack(frame[:16])
        assert rid == 20
        assert flags & FLAG_RESPONSE
        assert flags & FLAG_REPLY_V2
        assert flags & FLAG_CHUNKED
        assert not (flags & FLAG_BUDDY)
        payload = frame[16:]
        cidx, ctotal, cc = decode_chunk_header(payload, 0)
        assert cidx == 0 and ctotal == 3
        # status byte after chunk header
        assert payload[cc] == STATUS_SUCCESS
        # inline data after status
        assert payload[cc + 1:] == data

    def test_inline_chunked_reply_subsequent(self):
        data = b'result part 2'
        frame = encode_v2_inline_chunked_reply_frame(
            request_id=20, chunk_idx=1, total_chunks=3, data=data,
        )
        payload = frame[16:]
        cidx, _, cc = decode_chunk_header(payload, 0)
        assert cidx == 1
        # no status byte — data immediately follows chunk header
        assert payload[cc:] == data

    def test_inline_chunked_reply_last(self):
        frame = encode_v2_inline_chunked_reply_frame(
            request_id=20, chunk_idx=2, total_chunks=3, data=b'fin',
        )
        _, _, flags = FRAME_STRUCT.unpack(frame[:16])
        assert flags & FLAG_CHUNK_LAST


# ---------------------------------------------------------------------------
# CAP_CHUNKED in handshake
# ---------------------------------------------------------------------------

class TestCapChunked:

    def test_cap_chunked_roundtrip(self):
        caps = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED
        encoded = encode_v5_client_handshake([('seg', 256)], caps)
        hs = decode_v5_handshake(encoded)
        assert hs.capability_flags & CAP_CHUNKED
        assert hs.capability_flags == caps

    def test_cap_without_chunked(self):
        """Capability without CAP_CHUNKED — older client."""
        caps = CAP_CALL_V2 | CAP_METHOD_IDX
        encoded = encode_v5_client_handshake([('seg', 256)], caps)
        hs = decode_v5_handshake(encoded)
        assert not (hs.capability_flags & CAP_CHUNKED)
        assert hs.capability_flags == caps


class TestChunkHeaderEdgeCases:
    """Edge cases for chunk header encoding."""

    def test_zero_chunk_idx(self):
        """chunk_idx=0 (first chunk) roundtrips correctly."""
        raw = encode_chunk_header(0, 10)
        idx, total = decode_chunk_header(raw)[:2]
        assert idx == 0
        assert total == 10

    def test_total_chunks_one(self):
        """Single-chunk transfer (degenerate)."""
        raw = encode_chunk_header(0, 1)
        idx, total = decode_chunk_header(raw)[:2]
        assert idx == 0
        assert total == 1

    def test_total_chunks_zero_reserved(self):
        """total_chunks=0 is reserved for future streaming — still encodes."""
        raw = encode_chunk_header(0, 0)
        idx, total = decode_chunk_header(raw)[:2]
        assert idx == 0
        assert total == 0

    def test_chunk_header_size_constant(self):
        """CHUNK_HEADER_SIZE must be 4 (2B idx + 2B total)."""
        assert CHUNK_HEADER_SIZE == 4
        raw = encode_chunk_header(1, 2)
        assert len(raw) == CHUNK_HEADER_SIZE


class TestChunkedFrameEdgeCases:
    """Edge cases for chunked frame builders."""

    def test_inline_chunked_call_empty_payload(self):
        """Inline chunked call with empty chunk data."""
        frame = encode_v2_inline_chunked_call_frame(
            request_id=1, chunk_idx=0, total_chunks=1,
            data=b'', name='r', method_idx=0,
        )
        assert len(frame) > 0  # header + chunk_header + call_control

    def test_inline_chunked_reply_empty_payload(self):
        """Inline chunked reply with empty chunk data."""
        frame = encode_v2_inline_chunked_reply_frame(
            request_id=1, chunk_idx=0, total_chunks=1, data=b'',
        )
        assert len(frame) > 0

    def test_large_chunk_idx_near_max(self):
        """chunk_idx near u16 max — 65534 (second-to-last of 65535 chunks)."""
        raw = encode_chunk_header(65534, 65535)
        idx, total = decode_chunk_header(raw)[:2]
        assert idx == 65534
        assert total == 65535
