"""Unit tests for the Phase 1 split pool architecture.

Covers:
- CTRL message codec (SEGMENT_ANNOUNCE, CONSUMED)
- Handshake v3 codec (client→server and server ACK)
- v2 backward compatibility
- IPCConfig.max_pool_memory validation
- SegmentState enum
- Split pool SHM naming (ccpo/ccps prefixes)
- PID extraction from new naming formats
- Memory budget enforcement logic
"""

import os
import struct

import pytest

from c_two.rpc.ipc.ipc_protocol import (
    FLAG_CTRL,
    FLAG_DISK_SPILL,
    IPCConfig,
    SegmentState,
    CTRL_SEGMENT_ANNOUNCE,
    CTRL_CONSUMED,
    POOL_DIR_OUTBOUND,
    POOL_DIR_RESPONSE,
    encode_ctrl_segment_announce,
    decode_ctrl_segment_announce,
    encode_ctrl_consumed,
    decode_ctrl_consumed,
    extract_pid_from_shm_name,
)
from c_two.rpc.ipc.shm_pool import (
    encode_handshake,
    encode_handshake_ack,
    decode_handshake,
    decode_handshake_ack,
    pool_shm_name_outbound,
    pool_shm_name_response,
    pool_shm_name,
)
from c_two.rpc.ipc.ipc_client import _client_pool_shm_name

pytestmark = pytest.mark.timeout(10)


# ---------------------------------------------------------------------------
# 1. CTRL message codec
# ---------------------------------------------------------------------------

class TestCtrlSegmentAnnounce:
    """Round-trip encoding/decoding of CTRL_SEGMENT_ANNOUNCE."""

    def test_round_trip_outbound(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 1, 268_435_456, 'ccpo1234_abcdef')
        direction, index, size, name = decode_ctrl_segment_announce(payload)
        assert direction == POOL_DIR_OUTBOUND
        assert index == 1
        assert size == 268_435_456
        assert name == 'ccpo1234_abcdef'

    def test_round_trip_response(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_RESPONSE, 0, 512_000_000, 'ccps5678_fedcba')
        direction, index, size, name = decode_ctrl_segment_announce(payload)
        assert direction == POOL_DIR_RESPONSE
        assert index == 0
        assert size == 512_000_000
        assert name == 'ccps5678_fedcba'

    def test_first_byte_is_ctrl_type(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 0, 1024, 'test')
        assert payload[0] == CTRL_SEGMENT_ANNOUNCE

    def test_max_segment_index(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 255, 1024, 'x')
        _, index, _, _ = decode_ctrl_segment_announce(payload)
        assert index == 255

    def test_max_uint32_size(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_RESPONSE, 0, 0xFFFFFFFF, 'big')
        _, _, size, _ = decode_ctrl_segment_announce(payload)
        assert size == 0xFFFFFFFF

    def test_empty_name(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 0, 1024, '')
        _, _, _, name = decode_ctrl_segment_announce(payload)
        assert name == ''

    def test_too_short_raises(self):
        with pytest.raises(ValueError, match='too short'):
            decode_ctrl_segment_announce(b'\x01\x00')

    def test_memoryview_input(self):
        payload = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 2, 8192, 'mvtest')
        mv = memoryview(payload)
        direction, index, size, name = decode_ctrl_segment_announce(mv)
        assert direction == POOL_DIR_OUTBOUND
        assert index == 2
        assert name == 'mvtest'


class TestCtrlConsumed:
    """Round-trip encoding/decoding of CTRL_CONSUMED."""

    def test_round_trip_outbound(self):
        payload = encode_ctrl_consumed(POOL_DIR_OUTBOUND, 0)
        direction, index = decode_ctrl_consumed(payload)
        assert direction == POOL_DIR_OUTBOUND
        assert index == 0

    def test_round_trip_response(self):
        payload = encode_ctrl_consumed(POOL_DIR_RESPONSE, 3)
        direction, index = decode_ctrl_consumed(payload)
        assert direction == POOL_DIR_RESPONSE
        assert index == 3

    def test_first_byte_is_ctrl_type(self):
        payload = encode_ctrl_consumed(POOL_DIR_OUTBOUND, 0)
        assert payload[0] == CTRL_CONSUMED

    def test_exact_length(self):
        """CONSUMED is exactly 3 bytes: type + direction + index."""
        payload = encode_ctrl_consumed(POOL_DIR_RESPONSE, 5)
        assert len(payload) == 3

    def test_too_short_raises(self):
        with pytest.raises(ValueError, match='too short'):
            decode_ctrl_consumed(b'\x02\x00')

    def test_memoryview_input(self):
        payload = encode_ctrl_consumed(POOL_DIR_RESPONSE, 7)
        direction, index = decode_ctrl_consumed(memoryview(payload))
        assert direction == POOL_DIR_RESPONSE
        assert index == 7


# ---------------------------------------------------------------------------
# 2. Handshake v3 codec
# ---------------------------------------------------------------------------

class TestHandshakeV3:
    """Handshake v3 client→server and server ACK codec."""

    def test_client_to_server_round_trip(self):
        """Client→server handshake uses same layout as v2 (version=3)."""
        payload = encode_handshake('ccpo1234_abcdef', 268_435_456, 0)
        name, size, index = decode_handshake(payload)
        assert name == 'ccpo1234_abcdef'
        assert size == 268_435_456
        assert index == 0

    def test_client_version_byte_is_3(self):
        payload = encode_handshake('test', 1024)
        assert payload[0] == 3

    def test_server_ack_round_trip(self):
        ack = encode_handshake_ack(256_000_000, 256_000_000, 'ccps5678_fedcba')
        idx, client_neg, server_seg, server_name = decode_handshake_ack(ack)
        assert idx == 0
        assert client_neg == 256_000_000
        assert server_seg == 256_000_000
        assert server_name == 'ccps5678_fedcba'

    def test_server_ack_with_index(self):
        ack = encode_handshake_ack(100, 200, 'name', segment_index=3)
        idx, client_neg, server_seg, server_name = decode_handshake_ack(ack)
        assert idx == 3
        assert client_neg == 100
        assert server_seg == 200
        assert server_name == 'name'

    def test_v2_ack_backward_compat(self):
        """v2 ACK (version=2, no server pool info) decodes gracefully."""
        # Manually build a v2 ACK: [version=2][index=0][4B size][empty name]
        buf = bytearray(6)
        buf[0] = 2
        buf[1] = 0
        struct.pack_into('<I', buf, 2, 128_000_000)
        idx, client_neg, server_seg, server_name = decode_handshake_ack(bytes(buf))
        assert idx == 0
        assert client_neg == 128_000_000
        assert server_seg == 0
        assert server_name == ''

    def test_v3_ack_too_short_raises(self):
        # v3 ACK needs at least 10 bytes header
        buf = bytearray(8)
        buf[0] = 3
        with pytest.raises(ValueError, match='too short'):
            decode_handshake_ack(bytes(buf))

    def test_v1_decode_still_works(self):
        """v1 handshake still decodes correctly (backward compat)."""
        name_bytes = b'test_v1'
        buf = bytearray(5 + len(name_bytes))
        buf[0] = 1
        struct.pack_into('<I', buf, 1, 4096)
        buf[5:] = name_bytes
        name, size, idx = decode_handshake(bytes(buf))
        assert name == 'test_v1'
        assert size == 4096
        assert idx == 0


# ---------------------------------------------------------------------------
# 3. Flag constants
# ---------------------------------------------------------------------------

class TestFlagConstants:
    """Verify new flag bits don't collide with existing ones."""

    def test_flag_ctrl_value(self):
        assert FLAG_CTRL == 0x10

    def test_flag_disk_spill_value(self):
        assert FLAG_DISK_SPILL == 0x20

    def test_no_flag_collision(self):
        from c_two.rpc.ipc.ipc_protocol import FLAG_SHM, FLAG_RESPONSE, FLAG_HANDSHAKE, FLAG_POOL
        all_flags = [FLAG_SHM, FLAG_RESPONSE, FLAG_HANDSHAKE, FLAG_POOL, FLAG_CTRL, FLAG_DISK_SPILL]
        # Each should be a distinct power of 2
        for f in all_flags:
            assert f & (f - 1) == 0, f'Flag {f:#x} is not a power of 2'
        assert len(set(all_flags)) == len(all_flags), 'Duplicate flag values'


# ---------------------------------------------------------------------------
# 4. SegmentState enum
# ---------------------------------------------------------------------------

class TestSegmentState:
    """SegmentState enum correctness."""

    def test_values(self):
        assert SegmentState.FREE == 0
        assert SegmentState.IN_USE == 1

    def test_state_transition(self):
        """Simulate FREE → IN_USE → FREE."""
        state = SegmentState.FREE
        state = SegmentState.IN_USE
        assert state == SegmentState.IN_USE
        state = SegmentState.FREE
        assert state == SegmentState.FREE


# ---------------------------------------------------------------------------
# 5. IPCConfig.max_pool_memory validation
# ---------------------------------------------------------------------------

class TestIPCConfigMaxPoolMemory:
    """max_pool_memory field validation in IPCConfig."""

    def test_default_value(self):
        cfg = IPCConfig()
        assert cfg.max_pool_memory == cfg.pool_segment_size * cfg.max_pool_segments

    def test_custom_value(self):
        cfg = IPCConfig(pool_segment_size=1024, max_pool_memory=4096)
        assert cfg.max_pool_memory == 4096

    def test_too_small_raises(self):
        with pytest.raises(ValueError, match='max_pool_memory'):
            IPCConfig(pool_segment_size=1024, max_pool_memory=512)

    def test_equal_to_segment_size_ok(self):
        cfg = IPCConfig(pool_segment_size=1024, max_pool_memory=1024)
        assert cfg.max_pool_memory == 1024

    def test_budget_logic_allows_expansion(self):
        """Budget check: sum of segment sizes <= max_pool_memory."""
        cfg = IPCConfig(pool_segment_size=256, max_pool_memory=1024, max_pool_segments=4)
        segments = [256]  # initial segment
        for _ in range(3):
            budget_used = sum(segments)
            new_size = cfg.pool_segment_size
            if budget_used + new_size <= cfg.max_pool_memory:
                segments.append(new_size)
        assert len(segments) == 4
        assert sum(segments) == 1024

    def test_budget_logic_blocks_expansion(self):
        """Budget exhausted → can't add more segments."""
        cfg = IPCConfig(pool_segment_size=512, max_pool_memory=1024, max_pool_segments=4)
        segments = [512, 512]  # two segments, budget full
        budget_used = sum(segments)
        can_expand = budget_used + cfg.pool_segment_size <= cfg.max_pool_memory
        assert not can_expand


# ---------------------------------------------------------------------------
# 6. Split pool SHM naming
# ---------------------------------------------------------------------------

class TestSplitPoolNaming:
    """New ccpo_/ccps_ naming for split pool segments."""

    def test_outbound_prefix(self):
        name = pool_shm_name_outbound('region1', 0)
        assert name.startswith('ccpo')

    def test_response_prefix(self):
        name = pool_shm_name_response('region1', 0)
        assert name.startswith('ccps')

    def test_outbound_length(self):
        name = pool_shm_name_outbound('test', 42)
        assert len(name) == 20

    def test_response_length(self):
        name = pool_shm_name_response('test', 42)
        assert len(name) == 20

    def test_outbound_deterministic(self):
        a = pool_shm_name_outbound('r', 5)
        b = pool_shm_name_outbound('r', 5)
        assert a == b

    def test_response_deterministic(self):
        a = pool_shm_name_response('r', 5)
        b = pool_shm_name_response('r', 5)
        assert a == b

    def test_outbound_vs_response_differ(self):
        out = pool_shm_name_outbound('r', 0)
        resp = pool_shm_name_response('r', 0)
        assert out != resp

    def test_different_conn_ids(self):
        a = pool_shm_name_outbound('r', 0)
        b = pool_shm_name_outbound('r', 1)
        assert a != b

    def test_within_macos_limit(self):
        for fn in (pool_shm_name_outbound, pool_shm_name_response):
            name = fn('very_long_region_id_here', 999999)
            assert len(name) <= 31

    def test_client_pool_name_prefix(self):
        """_client_pool_shm_name should now produce ccpo prefix."""
        name = _client_pool_shm_name('test')
        assert name.startswith('ccpo')


# ---------------------------------------------------------------------------
# 7. PID extraction from new naming formats
# ---------------------------------------------------------------------------

class TestPIDExtractionSplitPool:
    """extract_pid_from_shm_name supports ccpo/ccps prefixes."""

    def test_extract_from_outbound_pool(self):
        name = pool_shm_name_outbound('region', 0)
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_extract_from_response_pool(self):
        name = pool_shm_name_response('region', 0)
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_extract_from_client_pool(self):
        name = _client_pool_shm_name('region')
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_legacy_pool_still_works(self):
        """Old ccp{d} format still extracts PID."""
        name = pool_shm_name('region', 0, 'resp')
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_all_new_names_within_limit(self):
        names = [
            pool_shm_name_outbound('r', 0),
            pool_shm_name_response('r', 0),
            _client_pool_shm_name('r'),
        ]
        for n in names:
            assert len(n) <= 31


# ---------------------------------------------------------------------------
# 8. Pool direction constants
# ---------------------------------------------------------------------------

class TestPoolDirection:
    """Direction constants used in CTRL messages and handshake."""

    def test_outbound_is_0(self):
        assert POOL_DIR_OUTBOUND == 0

    def test_response_is_1(self):
        assert POOL_DIR_RESPONSE == 1

    def test_distinct(self):
        assert POOL_DIR_OUTBOUND != POOL_DIR_RESPONSE
