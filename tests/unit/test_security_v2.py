"""Security and bounds-checking tests for rpc_v2 wire codec and protocol.

Validates that malformed / malicious inputs are rejected with clear errors
rather than causing buffer overflows, silent data corruption, or DoS.
"""
from __future__ import annotations

import struct

import pytest

from c_two.transport.protocol import (
    HANDSHAKE_V5,
    CAP_CALL_V2,
    CAP_METHOD_IDX,
    STATUS_SUCCESS,
    STATUS_ERROR,
    MethodEntry,
    RouteInfo,
    encode_v5_client_handshake,
    encode_v5_server_handshake,
    decode_v5_handshake,
    _MAX_HANDSHAKE_SEGMENTS,
    _MAX_HANDSHAKE_ROUTES,
    _MAX_HANDSHAKE_METHODS,
)
from c_two.transport.wire import (
    encode_call_control,
    decode_call_control,
    encode_reply_control,
    decode_reply_control,
)


# ---------------------------------------------------------------------------
# decode_v5_handshake bounds checking
# ---------------------------------------------------------------------------

class TestHandshakeV5BoundsChecking:
    """Validate that decode_v5_handshake rejects malformed payloads."""

    def test_too_short_payload(self):
        with pytest.raises(ValueError, match='too short'):
            decode_v5_handshake(b'\x05')

    def test_empty_payload(self):
        with pytest.raises(ValueError, match='too short'):
            decode_v5_handshake(b'')

    def test_wrong_version(self):
        with pytest.raises(ValueError, match='Expected handshake v5'):
            decode_v5_handshake(bytes([4, 0, 0]))

    def test_truncated_segment_entry(self):
        """seg_count=1 but no segment data follows."""
        buf = bytes([HANDSHAKE_V5]) + struct.pack('<H', 1)  # version + seg_count=1
        with pytest.raises(ValueError, match='truncated'):
            decode_v5_handshake(buf)

    def test_truncated_segment_name(self):
        """Segment name_len claims 10 bytes but only 2 available."""
        buf = bytearray()
        buf.append(HANDSHAKE_V5)
        buf += struct.pack('<H', 1)    # seg_count=1
        buf += struct.pack('<I', 256)  # segment size
        buf.append(10)                 # name_len = 10
        buf += b'ab'                   # only 2 bytes of name
        with pytest.raises(ValueError, match='truncated'):
            decode_v5_handshake(bytes(buf))

    def test_excessive_segment_count(self):
        """seg_count exceeds _MAX_HANDSHAKE_SEGMENTS."""
        buf = bytearray()
        buf.append(HANDSHAKE_V5)
        buf += struct.pack('<H', _MAX_HANDSHAKE_SEGMENTS + 1)
        with pytest.raises(ValueError, match='exceeds limit'):
            decode_v5_handshake(bytes(buf))

    def test_max_segment_count_ok(self):
        """Exactly _MAX_HANDSHAKE_SEGMENTS is allowed."""
        segs = [(f's{i}', 100) for i in range(_MAX_HANDSHAKE_SEGMENTS)]
        encoded = encode_v5_client_handshake(segs, CAP_CALL_V2)
        hs = decode_v5_handshake(encoded)
        assert len(hs.segments) == _MAX_HANDSHAKE_SEGMENTS

    def test_missing_capability_flags(self):
        """Payload has segment data but no capability_flags."""
        buf = bytearray()
        buf.append(HANDSHAKE_V5)
        buf += struct.pack('<H', 1)     # seg_count=1
        buf += struct.pack('<I', 256)   # segment size
        buf.append(2)                   # name_len=2
        buf += b'ab'                    # name
        # Missing 2-byte capability_flags
        with pytest.raises(ValueError, match='missing capability_flags'):
            decode_v5_handshake(bytes(buf))

    def test_truncated_route_entry(self):
        """Route section present but truncated."""
        segs = [('s1', 100)]
        routes = [RouteInfo('r1', [MethodEntry('m1', 0)])]
        full = encode_v5_server_handshake(segs, CAP_CALL_V2, routes)
        # Truncate inside route section
        truncated = full[:len(full) - 3]
        with pytest.raises(ValueError, match='truncated'):
            decode_v5_handshake(truncated)

    def test_excessive_route_count(self):
        """Craft payload with route_count exceeding limit."""
        segs = [('s1', 100)]
        encoded = encode_v5_client_handshake(segs, CAP_CALL_V2)
        # Append a fake route section with excessive count
        buf = bytearray(encoded)
        buf += struct.pack('<H', _MAX_HANDSHAKE_ROUTES + 1)
        with pytest.raises(ValueError, match='exceeds limit'):
            decode_v5_handshake(bytes(buf))

    def test_excessive_method_count(self):
        """Craft payload with method_count exceeding limit."""
        segs = [('s1', 100)]
        encoded = encode_v5_client_handshake(segs, CAP_CALL_V2)
        # Append route section: 1 route, with excessive method count
        buf = bytearray(encoded)
        buf += struct.pack('<H', 1)     # route_count=1
        buf.append(2)                   # route_name_len=2
        buf += b'r1'                    # route_name
        buf += struct.pack('<H', _MAX_HANDSHAKE_METHODS + 1)  # method count
        with pytest.raises(ValueError, match='exceeds limit'):
            decode_v5_handshake(bytes(buf))

    def test_valid_roundtrip_after_hardening(self):
        """Ensure normal encode/decode still works after bounds checks."""
        segs = [('seg_a', 256), ('seg_b', 512)]
        routes = [
            RouteInfo('ns1', [MethodEntry('add', 0), MethodEntry('mul', 1)]),
            RouteInfo('ns2', [MethodEntry('get', 0)]),
        ]
        encoded = encode_v5_server_handshake(segs, CAP_CALL_V2 | CAP_METHOD_IDX, routes)
        hs = decode_v5_handshake(encoded)
        assert len(hs.segments) == 2
        assert len(hs.routes) == 2
        assert hs.routes[0].methods[1].name == 'mul'


# ---------------------------------------------------------------------------
# decode_call_control bounds checking
# ---------------------------------------------------------------------------

class TestCallControlBoundsChecking:
    """Validate decode_call_control rejects truncated/malformed buffers."""

    def test_empty_buffer(self):
        with pytest.raises((ValueError, IndexError)):
            decode_call_control(b'', 0)

    def test_offset_beyond_buffer(self):
        with pytest.raises(ValueError, match='offset beyond buffer'):
            decode_call_control(b'\x00\x00\x00', 5)

    def test_name_len_exceeds_buffer(self):
        """name_len=10 but only 3 total bytes in buffer."""
        buf = bytes([10, 0x41, 0x42])  # name_len=10, only 2 bytes of name
        with pytest.raises(ValueError, match='buffer too short'):
            decode_call_control(buf, 0)

    def test_missing_method_idx_after_name(self):
        """Name is complete but no room for 2-byte method_idx."""
        buf = bytes([2, 0x41, 0x42])  # name_len=2, name='AB', but no method_idx
        with pytest.raises(ValueError, match='buffer too short'):
            decode_call_control(buf, 0)

    def test_zero_name_insufficient_idx(self):
        """name_len=0 but only 1 byte remains (need 2 for method_idx)."""
        buf = bytes([0, 0x01])  # name_len=0, only 1 byte for idx (need 2)
        with pytest.raises(ValueError, match='buffer too short'):
            decode_call_control(buf, 0)

    def test_valid_after_hardening(self):
        """Normal round-trip still works."""
        encoded = encode_call_control('hello', 42)
        name, idx, consumed = decode_call_control(encoded)
        assert name == 'hello'
        assert idx == 42

    def test_max_name_length(self):
        """255-byte name (max for 1-byte length)."""
        long_name = 'x' * 255
        encoded = encode_call_control(long_name, 100)
        name, idx, consumed = decode_call_control(encoded)
        assert name == long_name
        assert idx == 100


# ---------------------------------------------------------------------------
# decode_reply_control bounds checking
# ---------------------------------------------------------------------------

class TestReplyControlBoundsChecking:
    """Validate decode_reply_control rejects truncated/malformed buffers."""

    def test_empty_buffer(self):
        with pytest.raises((ValueError, IndexError)):
            decode_reply_control(b'', 0)

    def test_offset_beyond_buffer(self):
        with pytest.raises(ValueError, match='offset beyond buffer'):
            decode_reply_control(b'\x00', 5)

    def test_error_missing_length(self):
        """STATUS_ERROR but no error_len follows."""
        buf = bytes([STATUS_ERROR])  # status=error, no length
        with pytest.raises(ValueError, match='buffer too short for error length'):
            decode_reply_control(buf, 0)

    def test_error_truncated_length(self):
        """STATUS_ERROR with partial error_len (only 2 of 4 bytes)."""
        buf = bytes([STATUS_ERROR, 0x00, 0x00])
        with pytest.raises(ValueError, match='buffer too short for error length'):
            decode_reply_control(buf, 0)

    def test_error_len_exceeds_buffer(self):
        """error_len claims 1000 bytes but only 5 available."""
        buf = bytearray([STATUS_ERROR])
        buf += struct.pack('<I', 1000)  # err_len = 1000
        buf += b'hello'                 # only 5 bytes of error data
        with pytest.raises(ValueError, match='error data claims'):
            decode_reply_control(bytes(buf), 0)

    def test_error_len_zero(self):
        """error_len=0 is valid (empty error)."""
        buf = bytearray([STATUS_ERROR])
        buf += struct.pack('<I', 0)
        status, err, consumed = decode_reply_control(bytes(buf), 0)
        assert status == STATUS_ERROR
        assert err == b''

    def test_success_after_hardening(self):
        """Normal success round-trip."""
        encoded = encode_reply_control(STATUS_SUCCESS)
        status, err, consumed = decode_reply_control(encoded)
        assert status == STATUS_SUCCESS
        assert err is None

    def test_error_with_data_roundtrip(self):
        """Normal error round-trip."""
        err_data = b'something went wrong'
        encoded = encode_reply_control(STATUS_ERROR, err_data)
        status, dec_err, consumed = decode_reply_control(encoded)
        assert status == STATUS_ERROR
        assert dec_err == err_data


# ---------------------------------------------------------------------------
# PendingCall thread safety
# ---------------------------------------------------------------------------

class TestPendingCallSafety:
    """Validate PendingCall behavior under edge conditions."""

    def test_double_set_result(self):
        """Setting result twice is harmless (first wins on event)."""
        from c_two.transport.client.core import PendingCall
        p = PendingCall(rid=0)
        p.set_result(b'first')
        p.set_result(b'second')
        # Event is already set, wait returns immediately.
        # Result is the last one set (Python assignment is atomic).
        result = p.wait(timeout=1.0)
        assert result in (b'first', b'second')

    def test_set_error_then_result(self):
        """Error takes precedence even if result is also set."""
        from c_two.transport.client.core import PendingCall
        p = PendingCall(rid=0)
        p.set_error(ValueError('test'))
        p.set_result(b'data')
        with pytest.raises(ValueError, match='test'):
            p.wait(timeout=1.0)

    def test_timeout_raises(self):
        """wait() with short timeout raises TimeoutError."""
        from c_two.transport.client.core import PendingCall
        p = PendingCall(rid=0)
        with pytest.raises(TimeoutError):
            p.wait(timeout=0.01)

    def test_concurrent_wait_and_set(self):
        """One thread waits, another sets result concurrently."""
        import threading
        from c_two.transport.client.core import PendingCall
        p = PendingCall(rid=42)
        results = []

        def waiter():
            try:
                r = p.wait(timeout=5.0)
                results.append(r)
            except Exception as e:
                results.append(e)

        def setter():
            import time
            time.sleep(0.05)
            p.set_result(b'concurrent')

        t1 = threading.Thread(target=waiter)
        t2 = threading.Thread(target=setter)
        t1.start()
        t2.start()
        t1.join(timeout=3.0)
        t2.join(timeout=1.0)
        assert results == [b'concurrent']


# ---------------------------------------------------------------------------
# MethodTable safety
# ---------------------------------------------------------------------------

class TestMethodTableSafety:
    """Edge cases for MethodTable lookups."""

    def test_index_of_missing_raises(self):
        from c_two.transport.wire import MethodTable
        t = MethodTable.from_methods(['a', 'b'])
        with pytest.raises(KeyError):
            t.index_of('nonexistent')

    def test_name_of_missing_raises(self):
        from c_two.transport.wire import MethodTable
        t = MethodTable.from_methods(['a', 'b'])
        with pytest.raises(KeyError):
            t.name_of(999)

    def test_empty_table(self):
        from c_two.transport.wire import MethodTable
        t = MethodTable()
        assert len(t) == 0
        assert t.names() == []
        assert not t.has_name('x')
        assert not t.has_index(0)


class TestSchedulerSafety:
    """Validate scheduler edge cases."""

    def test_begin_after_shutdown_raises(self):
        from c_two.transport.server.scheduler import ConcurrencyConfig, Scheduler
        sched = Scheduler(ConcurrencyConfig())
        sched.shutdown()
        with pytest.raises(RuntimeError, match='shut down'):
            sched.begin()

    def test_begin_at_capacity_raises(self):
        from c_two.transport.server.scheduler import ConcurrencyConfig, Scheduler
        sched = Scheduler(ConcurrencyConfig(max_pending=1))
        sched.begin()  # fills to capacity
        with pytest.raises(RuntimeError, match='capacity'):
            sched.begin()
        # Clean up: decrement so shutdown doesn't wait forever
        with sched._state_lock:
            sched._pending_count = 0
            sched._drain_event.set()
        sched.shutdown()

    def test_execute_decrements_pending(self):
        from c_two.transport.server.scheduler import ConcurrencyConfig, Scheduler
        sched = Scheduler(ConcurrencyConfig(max_pending=2))
        sched.begin()
        assert sched._pending_count == 1
        sched.execute(lambda b: b, b'test')
        assert sched._pending_count == 0
        sched.shutdown()
