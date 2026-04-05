"""Unit tests for c_two.config.ipc frozen dataclasses."""
from __future__ import annotations

import pytest
from c_two.config.ipc import (
    BaseIPCConfig,
    ServerIPCConfig,
    ClientIPCConfig,
    build_server_config,
    build_client_config,
)


class TestBaseIPCConfig:
    def test_defaults(self):
        cfg = BaseIPCConfig()
        assert cfg.pool_segment_size == 268_435_456
        assert cfg.max_pool_segments == 4
        assert cfg.chunk_size == 131_072

    def test_frozen(self):
        cfg = BaseIPCConfig()
        with pytest.raises(AttributeError):
            cfg.pool_segment_size = 0  # type: ignore[misc]

    def test_reject_zero_segment_size(self):
        with pytest.raises(ValueError, match='pool_segment_size'):
            BaseIPCConfig(pool_segment_size=0)

    def test_reject_oversized_segment(self):
        with pytest.raises(ValueError):
            BaseIPCConfig(pool_segment_size=0xFFFFFFFF + 1)

    def test_reject_bad_pool_segments(self):
        with pytest.raises(ValueError, match='max_pool_segments'):
            BaseIPCConfig(max_pool_segments=0)
        with pytest.raises(ValueError, match='max_pool_segments'):
            BaseIPCConfig(max_pool_segments=256)

    def test_reject_bad_threshold_ratio(self):
        with pytest.raises(ValueError, match='chunk_threshold_ratio'):
            BaseIPCConfig(chunk_threshold_ratio=0.0)
        with pytest.raises(ValueError, match='chunk_threshold_ratio'):
            BaseIPCConfig(chunk_threshold_ratio=1.5)

    def test_reject_bad_gc_interval(self):
        with pytest.raises(ValueError, match='chunk_gc_interval'):
            BaseIPCConfig(chunk_gc_interval=0.0)

    def test_reject_bad_assembler_timeout(self):
        with pytest.raises(ValueError, match='chunk_assembler_timeout'):
            BaseIPCConfig(chunk_assembler_timeout=-1.0)

    def test_reject_bad_reassembly_segments(self):
        with pytest.raises(ValueError, match='reassembly_max_segments'):
            BaseIPCConfig(reassembly_max_segments=0)
        with pytest.raises(ValueError, match='reassembly_max_segments'):
            BaseIPCConfig(reassembly_max_segments=256)

    def test_reject_bad_reassembly_size(self):
        with pytest.raises(ValueError, match='reassembly_segment_size'):
            BaseIPCConfig(reassembly_segment_size=0)


class TestServerIPCConfig:
    def test_defaults(self):
        cfg = ServerIPCConfig()
        assert cfg.max_frame_size == 2_147_483_648
        assert cfg.max_payload_size == 17_179_869_184
        assert cfg.heartbeat_interval == 15.0
        assert cfg.heartbeat_timeout == 30.0

    def test_inherits_base(self):
        cfg = ServerIPCConfig()
        assert cfg.pool_segment_size == 268_435_456
        assert cfg.reassembly_segment_size == 268_435_456  # NOT 64 MB

    def test_heartbeat_disabled(self):
        cfg = ServerIPCConfig(heartbeat_interval=0.0)
        assert cfg.heartbeat_interval == 0.0  # 0 = disabled, no error

    def test_heartbeat_negative_rejected(self):
        with pytest.raises(ValueError, match='heartbeat_interval'):
            ServerIPCConfig(heartbeat_interval=-1.0)

    def test_heartbeat_timeout_must_exceed_interval(self):
        with pytest.raises(ValueError, match='heartbeat_timeout'):
            ServerIPCConfig(heartbeat_interval=10.0, heartbeat_timeout=10.0)

    def test_max_pool_memory_check(self):
        with pytest.raises(ValueError, match='max_pool_memory'):
            ServerIPCConfig(max_pool_memory=1, pool_segment_size=1024)

    def test_max_frame_size_check(self):
        with pytest.raises(ValueError, match='max_frame_size'):
            ServerIPCConfig(max_frame_size=16)

    def test_max_payload_size_check(self):
        with pytest.raises(ValueError, match='max_payload_size'):
            ServerIPCConfig(max_payload_size=0)

    def test_custom_values(self):
        cfg = ServerIPCConfig(
            pool_segment_size=1 << 20,
            max_pool_segments=2,
            max_pool_memory=2 << 20,
            max_frame_size=1 << 20,
            max_payload_size=1 << 30,
            heartbeat_interval=5.0,
            heartbeat_timeout=10.0,
        )
        assert cfg.pool_segment_size == 1 << 20
        assert cfg.max_pool_segments == 2
        assert cfg.heartbeat_interval == 5.0


class TestClientIPCConfig:
    def test_reassembly_override(self):
        cfg = ClientIPCConfig()
        assert cfg.reassembly_segment_size == 67_108_864  # 64 MB

    def test_base_defaults_preserved(self):
        cfg = ClientIPCConfig()
        assert cfg.pool_segment_size == 268_435_456  # 256 MB from Base

    def test_custom_reassembly(self):
        cfg = ClientIPCConfig(reassembly_segment_size=1 << 28)
        assert cfg.reassembly_segment_size == 1 << 28


class TestBuildServerConfig:
    def test_defaults(self):
        cfg = build_server_config()
        assert cfg.pool_segment_size == 268_435_456
        assert cfg.heartbeat_interval == 15.0

    def test_kwargs_override(self):
        cfg = build_server_config(pool_segment_size=1 << 30)
        assert cfg.pool_segment_size == 1 << 30

    def test_env_override(self, monkeypatch):
        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', '536870912')
        from c_two.config.settings import C2Settings
        s = C2Settings()
        cfg = build_server_config(settings=s)
        assert cfg.pool_segment_size == 536_870_912

    def test_kwargs_beat_env(self, monkeypatch):
        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.setenv('C2_IPC_POOL_SEGMENT_SIZE', '536870912')
        from c_two.config.settings import C2Settings
        s = C2Settings()
        cfg = build_server_config(settings=s, pool_segment_size=1 << 20)
        assert cfg.pool_segment_size == 1 << 20

    def test_clamping(self):
        cfg = build_server_config(
            pool_segment_size=100 * (1 << 30),  # 100 GB
            max_payload_size=1 * (1 << 30),      # 1 GB
        )
        assert cfg.pool_segment_size == 1 * (1 << 30)  # clamped

    def test_auto_max_pool_memory(self):
        cfg = build_server_config(pool_segment_size=1 << 20, max_pool_segments=2)
        assert cfg.max_pool_memory == 2 * (1 << 20)


class TestBuildClientConfig:
    def test_defaults(self):
        cfg = build_client_config()
        assert cfg.reassembly_segment_size == 67_108_864  # 64 MB

    def test_kwargs_override(self):
        cfg = build_client_config(reassembly_segment_size=1 << 28)
        assert cfg.reassembly_segment_size == 1 << 28

    def test_env_override(self, monkeypatch):
        monkeypatch.setenv('C2_ENV_FILE', '')
        monkeypatch.setenv('C2_IPC_REASSEMBLY_SEGMENT_SIZE', '33554432')
        from c_two.config.settings import C2Settings
        s = C2Settings()
        cfg = build_client_config(settings=s)
        assert cfg.reassembly_segment_size == 33_554_432
