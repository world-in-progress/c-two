"""Unit tests for rpc_v2 wire v2 codec and protocol v3.1."""
from __future__ import annotations

import pytest

from c_two.rpc_v2.protocol import (
    FLAG_CALL_V2,
    FLAG_REPLY_V2,
    HANDSHAKE_V5,
    CAP_CALL_V2,
    CAP_METHOD_IDX,
    STATUS_SUCCESS,
    STATUS_ERROR,
    MethodEntry,
    NamespaceInfo,
    HandshakeV5,
    encode_v5_client_handshake,
    encode_v5_server_handshake,
    decode_v5_handshake,
)
from c_two.rpc_v2.wire import (
    MethodTable,
    encode_call_control,
    decode_call_control,
    encode_reply_control,
    decode_reply_control,
    encode_v2_buddy_call_frame,
    encode_v2_inline_call_frame,
    encode_v2_buddy_reply_frame,
    encode_v2_inline_reply_frame,
    encode_v2_error_reply_frame,
)
from c_two.rpc.ipc.ipc_protocol import FRAME_STRUCT, FLAG_RESPONSE
from c_two.rpc.ipc.ipc_v3_protocol import FLAG_BUDDY, BUDDY_PAYLOAD_STRUCT


# ---------------------------------------------------------------------------
# Wire v2 call control round-trip
# ---------------------------------------------------------------------------

class TestCallControl:

    def test_encode_decode_with_namespace(self):
        ns, idx = 'test.hello', 42
        encoded = encode_call_control(ns, idx)
        dec_ns, dec_idx, consumed = decode_call_control(encoded)
        assert dec_ns == ns
        assert dec_idx == idx
        assert consumed == len(encoded)

    def test_encode_decode_empty_namespace(self):
        encoded = encode_call_control('', 7)
        ns, idx, consumed = decode_call_control(encoded)
        assert ns == ''
        assert idx == 7
        assert consumed == 3  # 1B ns_len=0 + 2B idx

    def test_decode_at_offset(self):
        prefix = b'\xff\xfe'
        ctrl = encode_call_control('ns', 99)
        data = prefix + ctrl + b'\x00\x00'
        ns, idx, consumed = decode_call_control(data, offset=len(prefix))
        assert ns == 'ns'
        assert idx == 99
        assert consumed == len(ctrl)

    def test_large_namespace(self):
        ns = 'a' * 255  # max 1-byte length
        encoded = encode_call_control(ns, 0)
        dec_ns, dec_idx, consumed = decode_call_control(encoded)
        assert dec_ns == ns
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
            is_dedicated=False, namespace='ns', method_idx=1,
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
        assert hs.namespaces == []  # Client doesn't send namespaces

    def test_server_handshake_roundtrip(self):
        segments = [('srv_seg', 134217728)]
        cap = CAP_CALL_V2 | CAP_METHOD_IDX
        ns_info = NamespaceInfo(
            namespace='test.hello',
            methods=[
                MethodEntry(name='add', index=0),
                MethodEntry(name='greeting', index=1),
            ],
        )
        encoded = encode_v5_server_handshake(segments, cap, [ns_info])
        assert encoded[0] == HANDSHAKE_V5
        hs = decode_v5_handshake(encoded)
        assert hs.segments == segments
        assert hs.capability_flags == cap
        assert len(hs.namespaces) == 1
        ns = hs.namespaces[0]
        assert ns.namespace == 'test.hello'
        assert len(ns.methods) == 2
        assert ns.method_by_name('add') == 0
        assert ns.method_by_name('greeting') == 1
        assert ns.method_by_index(0) == 'add'

    def test_multi_segment_multi_namespace(self):
        segs = [('s1', 100), ('s2', 200)]
        ns1 = NamespaceInfo('ns.a', [MethodEntry('m1', 0)])
        ns2 = NamespaceInfo('ns.b', [MethodEntry('m2', 0), MethodEntry('m3', 1)])
        encoded = encode_v5_server_handshake(segs, CAP_CALL_V2, [ns1, ns2])
        hs = decode_v5_handshake(encoded)
        assert len(hs.segments) == 2
        assert len(hs.namespaces) == 2
        assert hs.namespaces[0].namespace == 'ns.a'
        assert hs.namespaces[1].namespace == 'ns.b'
        assert len(hs.namespaces[1].methods) == 2

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
