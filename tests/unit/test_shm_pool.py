"""Unit tests for the SHM pool module."""

import os
import struct
import pytest
from multiprocessing import shared_memory

from c_two.rpc.ipc.shm_pool import (
    cleanup_stale_shm,
    close_pool_shm,
    create_pool_shm,
    decode_handshake,
    encode_handshake,
    pool_shm_name,
)
from c_two.rpc.ipc.ipc_protocol import (
    shm_name,
    extract_pid_from_shm_name,
)
from c_two.rpc.ipc.ipc_client import _client_pool_shm_name

pytestmark = pytest.mark.timeout(10)


# ---------------------------------------------------------------------------
# SHM naming
# ---------------------------------------------------------------------------

class TestPoolSHMName:
    """pool_shm_name produces valid, deterministic POSIX SHM names."""

    def test_format(self):
        name = pool_shm_name('region1', 0, 'resp')
        assert name.startswith('ccpr')
        assert name[3] == 'r'
        assert '_' in name[4:]
        assert len(name) == 20  # ccp + d + pid_hex + _ + hash_hex

    def test_direction_initial(self):
        req_name = pool_shm_name('r', 1, 'req')
        resp_name = pool_shm_name('r', 1, 'resp')
        assert req_name[3] == 'r'
        assert resp_name[3] == 'r'

    def test_deterministic(self):
        a = pool_shm_name('region', 42, 'req')
        b = pool_shm_name('region', 42, 'req')
        assert a == b

    def test_unique_across_connections(self):
        a = pool_shm_name('region', 0, 'req')
        b = pool_shm_name('region', 1, 'req')
        assert a != b

    def test_within_macos_limit(self):
        """macOS POSIX SHM names are limited to 31 chars."""
        name = pool_shm_name('very_long_region_id_here', 999999, 'resp')
        assert len(name) <= 31


# ---------------------------------------------------------------------------
# Handshake encode/decode
# ---------------------------------------------------------------------------

class TestHandshakeCodec:
    """Round-trip encoding/decoding of pool handshake payloads."""

    def test_round_trip(self):
        name = 'ccpr_abcdef123456'
        size = 268_435_456  # 256 MB
        payload = encode_handshake(name, size)
        decoded_name, decoded_size, decoded_index = decode_handshake(payload)
        assert decoded_name == name
        assert decoded_size == size
        assert decoded_index == 0

    def test_small_segment(self):
        payload = encode_handshake('test', 1024)
        name, size, index = decode_handshake(payload)
        assert name == 'test'
        assert size == 1024
        assert index == 0

    def test_memoryview_input(self):
        payload = encode_handshake('test_name', 4096)
        mv = memoryview(payload)
        name, size, index = decode_handshake(mv)
        assert name == 'test_name'
        assert size == 4096
        assert index == 0

    def test_too_short_raises(self):
        with pytest.raises(ValueError, match='too short'):
            decode_handshake(b'\x01\x00')

    def test_wrong_version_raises(self):
        buf = bytearray(10)
        buf[0] = 99  # invalid version
        struct.pack_into('<I', buf, 1, 1024)
        with pytest.raises(ValueError, match='Unsupported handshake version'):
            decode_handshake(bytes(buf))


# ---------------------------------------------------------------------------
# SHM lifecycle helpers
# ---------------------------------------------------------------------------

class TestPoolSHMLifecycle:
    """create_pool_shm / close_pool_shm / cleanup_stale_shm."""

    def test_create_and_close(self):
        name = f'cc_test_pool_{os.getpid()}'
        shm = create_pool_shm(name, 4096)
        assert shm.size >= 4096
        close_pool_shm(shm, unlink=True)
        # Verify it was unlinked
        with pytest.raises(FileNotFoundError):
            shared_memory.SharedMemory(name=name, create=False)

    def test_create_cleans_stale(self):
        """create_pool_shm removes a stale segment with the same name."""
        name = f'cc_test_stale_{os.getpid()}'
        # Create a stale segment
        stale = shared_memory.SharedMemory(name=name, create=True, size=1024)
        stale.close()
        # create_pool_shm should replace it
        shm = create_pool_shm(name, 2048)
        assert shm.size >= 2048
        close_pool_shm(shm, unlink=True)

    def test_cleanup_stale_nonexistent_is_noop(self):
        """cleanup_stale_shm on a nonexistent segment should not raise."""
        cleanup_stale_shm('cc_nonexistent_12345')

    def test_close_none_is_safe(self):
        """close_pool_shm(None) should not raise."""
        close_pool_shm(None)
        close_pool_shm(None, unlink=True)

    def test_close_without_unlink(self):
        """close_pool_shm without unlink leaves segment accessible."""
        name = f'cc_test_nounlink_{os.getpid()}'
        shm = create_pool_shm(name, 4096)
        close_pool_shm(shm, unlink=False)
        # Should still be accessible
        shm2 = shared_memory.SharedMemory(name=name, create=False)
        shm2.close()
        shm2.unlink()

    def test_write_and_read_through_pool_shm(self):
        """Data written to pool SHM can be read back through a second handle."""
        name = f'cc_test_rw_{os.getpid()}'
        writer = create_pool_shm(name, 1024)
        try:
            test_data = b'hello pool world'
            writer.buf[:len(test_data)] = test_data

            # Open as reader (simulates the other process)
            reader = shared_memory.SharedMemory(name=name, create=False)
            try:
                assert bytes(reader.buf[:len(test_data)]) == test_data
            finally:
                reader.close()
        finally:
            close_pool_shm(writer, unlink=True)


# ---------------------------------------------------------------------------
# Handshake v2 encode/decode (segment chain)
# ---------------------------------------------------------------------------

class TestHandshakeV2:
    """Handshake v2 wire format with segment_index."""

    def test_v2_round_trip_with_index(self):
        payload = encode_handshake('pool_seg_3', 131072, segment_index=3)
        name, size, idx = decode_handshake(payload)
        assert name == 'pool_seg_3'
        assert size == 131072
        assert idx == 3

    def test_v2_index_zero_equivalent_to_default(self):
        explicit = encode_handshake('test', 4096, segment_index=0)
        default = encode_handshake('test', 4096)
        assert explicit == default

    def test_v1_decode_backward_compat(self):
        """Manually build a v1 payload and verify decode returns segment_index=0."""
        shm_name_str = 'ccpr_oldformat'
        name_bytes = shm_name_str.encode('utf-8')
        buf = bytearray(5 + len(name_bytes))
        buf[0] = 1  # v1
        struct.pack_into('<I', buf, 1, 8192)
        buf[5:] = name_bytes
        name, size, idx = decode_handshake(bytes(buf))
        assert name == shm_name_str
        assert size == 8192
        assert idx == 0

    def test_v2_short_payload_raises(self):
        """v2 header needs 6 bytes; 3 bytes should raise."""
        payload = bytes([2, 0, 0])  # version=2 but only 3 bytes total
        with pytest.raises(ValueError, match='too short'):
            decode_handshake(payload)


# ---------------------------------------------------------------------------
# PID-embedded SHM naming
# ---------------------------------------------------------------------------

class TestPIDEmbeddedNaming:
    """PID extraction from per-request, pool, and client-pool SHM names."""

    def test_extract_from_per_request_name(self):
        name = shm_name('region1', 'req42', 'req')
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_extract_from_pool_name(self):
        name = pool_shm_name('region1', 0, 'resp')
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_extract_from_client_pool_name(self):
        name = _client_pool_shm_name('region1')
        pid = extract_pid_from_shm_name(name)
        assert pid == os.getpid()

    def test_extract_returns_none_for_unknown_prefix(self):
        assert extract_pid_from_shm_name('xyz_test') is None

    def test_extract_returns_none_for_no_underscore(self):
        assert extract_pid_from_shm_name('ccr1234') is None

    def test_all_names_within_macos_limit(self):
        """macOS POSIX SHM names must be ≤ 31 chars."""
        names = [
            shm_name('region', 'req1', 'req'),
            pool_shm_name('region', 0, 'resp'),
            _client_pool_shm_name('region'),
        ]
        for n in names:
            assert len(n) <= 31, f'{n!r} is {len(n)} chars (> 31)'
