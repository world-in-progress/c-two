"""Unit tests for IPC v2 P0 enhancements — config validation & pool payload header."""

import struct
import pytest

from c_two.rpc.ipc.ipc_protocol import IPCConfig, POOL_PAYLOAD_HEADER_SIZE

pytestmark = pytest.mark.timeout(10)


# ---------------------------------------------------------------------------
# IPCConfig P0 field validation
# ---------------------------------------------------------------------------

class TestIPCConfigP0:
    """Validation of heartbeat and max_pool_segments fields."""

    def test_default_config_valid(self):
        cfg = IPCConfig()
        assert cfg.heartbeat_interval == 15.0
        assert cfg.heartbeat_timeout == 30.0
        assert cfg.max_pool_segments == 4

    def test_max_pool_segments_range(self):
        with pytest.raises(ValueError, match='max_pool_segments'):
            IPCConfig(max_pool_segments=0)
        with pytest.raises(ValueError, match='max_pool_segments'):
            IPCConfig(max_pool_segments=256)
        # boundary values must succeed
        cfg_min = IPCConfig(max_pool_segments=1)
        cfg_max = IPCConfig(max_pool_segments=255)
        assert cfg_min.max_pool_segments == 1
        assert cfg_max.max_pool_segments == 255

    def test_heartbeat_timeout_must_exceed_interval(self):
        with pytest.raises(ValueError, match='heartbeat_timeout'):
            IPCConfig(heartbeat_interval=10, heartbeat_timeout=5)
        with pytest.raises(ValueError, match='heartbeat_timeout'):
            IPCConfig(heartbeat_interval=10, heartbeat_timeout=10)

    def test_heartbeat_disabled_when_interval_zero(self):
        cfg = IPCConfig(heartbeat_interval=0, heartbeat_timeout=0)
        assert cfg.heartbeat_interval == 0

    def test_heartbeat_disabled_when_interval_negative(self):
        cfg = IPCConfig(heartbeat_interval=-1)
        assert cfg.heartbeat_interval == -1


# ---------------------------------------------------------------------------
# Pool payload header format
# ---------------------------------------------------------------------------

class TestPoolPayloadHeader:
    """Pool payload header: 1B segment_index + 8B data_size = 9 bytes."""

    def test_pool_payload_header_size_is_9(self):
        assert POOL_PAYLOAD_HEADER_SIZE == 9

    def test_pool_payload_header_encode_decode(self):
        idx, size = 7, 268_435_456
        buf = struct.pack('<BQ', idx, size)
        assert len(buf) == POOL_PAYLOAD_HEADER_SIZE
        dec_idx, dec_size = struct.unpack_from('<BQ', buf, 0)
        assert dec_idx == idx
        assert dec_size == size
