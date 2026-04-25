"""Frozen IPC configuration dataclasses with validation."""
from __future__ import annotations

from dataclasses import dataclass, fields
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .settings import C2Settings


@dataclass(frozen=True)
class BaseIPCConfig:
    """Shared IPC configuration for both server and client."""

    pool_enabled: bool = True
    pool_segment_size: int = 268_435_456        # 256 MB
    max_pool_segments: int = 4
    max_pool_memory: int = 1_073_741_824        # 1 GB
    reassembly_segment_size: int = 268_435_456  # 256 MB
    reassembly_max_segments: int = 4
    max_total_chunks: int = 512
    chunk_gc_interval: float = 5.0
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0
    max_reassembly_bytes: int = 8_589_934_592   # 8 GB
    chunk_size: int = 131_072                   # 128 KB

    def __post_init__(self) -> None:
        if self.pool_segment_size <= 0 or self.pool_segment_size > 0xFFFFFFFF:
            raise ValueError(
                f'pool_segment_size must be in (0, {0xFFFFFFFF}], '
                f'got {self.pool_segment_size}'
            )
        if not 1 <= self.max_pool_segments <= 255:
            raise ValueError(
                f'max_pool_segments must be 1..255, got {self.max_pool_segments}'
            )
        if not (0 < self.chunk_threshold_ratio <= 1):
            raise ValueError(
                f'chunk_threshold_ratio must be in (0, 1], '
                f'got {self.chunk_threshold_ratio}'
            )
        if self.chunk_gc_interval <= 0:
            raise ValueError(
                f'chunk_gc_interval must be positive, got {self.chunk_gc_interval}'
            )
        if self.chunk_assembler_timeout <= 0:
            raise ValueError(
                f'chunk_assembler_timeout must be positive, '
                f'got {self.chunk_assembler_timeout}'
            )
        if not 1 <= self.reassembly_max_segments <= 255:
            raise ValueError(
                f'reassembly_max_segments must be 1..255, '
                f'got {self.reassembly_max_segments}'
            )
        if self.reassembly_segment_size <= 0:
            raise ValueError(
                f'reassembly_segment_size must be positive, '
                f'got {self.reassembly_segment_size}'
            )


@dataclass(frozen=True)
class ServerIPCConfig(BaseIPCConfig):
    """Server-side IPC configuration."""

    max_frame_size: int = 2_147_483_648         # 2 GB
    max_payload_size: int = 17_179_869_184      # 16 GB
    max_pending_requests: int = 1024
    pool_decay_seconds: float = 60.0
    heartbeat_interval: float = 15.0
    heartbeat_timeout: float = 30.0

    def __post_init__(self) -> None:
        super().__post_init__()
        if self.max_frame_size <= 16:
            raise ValueError(
                f'max_frame_size must be > 16, got {self.max_frame_size}'
            )
        if self.max_payload_size <= 0:
            raise ValueError(
                f'max_payload_size must be positive, got {self.max_payload_size}'
            )
        if self.heartbeat_interval < 0:
            raise ValueError(
                f'heartbeat_interval must be >= 0, got {self.heartbeat_interval}'
            )
        if self.heartbeat_interval > 0 and self.heartbeat_timeout <= self.heartbeat_interval:
            raise ValueError(
                f'heartbeat_timeout ({self.heartbeat_timeout}) must exceed '
                f'heartbeat_interval ({self.heartbeat_interval})'
            )
        if self.max_pool_memory < self.pool_segment_size:
            raise ValueError(
                f'max_pool_memory ({self.max_pool_memory}) must be >= '
                f'pool_segment_size ({self.pool_segment_size})'
            )


@dataclass(frozen=True)
class ClientIPCConfig(BaseIPCConfig):
    """Client-side IPC configuration."""

    reassembly_segment_size: int = 67_108_864   # 64 MB


def build_server_config(
    settings: C2Settings | None = None,
    **kwargs: object,
) -> ServerIPCConfig:
    """Build a ``ServerIPCConfig`` from kwargs > env vars > class defaults."""
    if settings is None:
        from .settings import settings as _settings
        settings = _settings

    merged: dict[str, object] = {}
    for f in fields(ServerIPCConfig):
        name = f.name
        env_name = f'ipc_{name}'
        if name in kwargs:
            merged[name] = kwargs[name]
        elif hasattr(settings, env_name) and getattr(settings, env_name) is not None:
            merged[name] = getattr(settings, env_name)

    # Clamping: pool_segment_size <= max_payload_size
    seg = merged.get('pool_segment_size', ServerIPCConfig.pool_segment_size)
    pay = merged.get('max_payload_size', ServerIPCConfig.max_payload_size)
    if seg > pay:  # type: ignore[operator]
        merged['pool_segment_size'] = pay

    # Auto-derive max_pool_memory if not explicitly set
    if 'max_pool_memory' not in merged:
        seg_final = merged.get('pool_segment_size', ServerIPCConfig.pool_segment_size)
        segs = merged.get('max_pool_segments', ServerIPCConfig.max_pool_segments)
        merged['max_pool_memory'] = seg_final * segs  # type: ignore[operator]

    return ServerIPCConfig(**merged)


def build_client_config(
    settings: C2Settings | None = None,
    **kwargs: object,
) -> ClientIPCConfig:
    """Build a ``ClientIPCConfig`` from kwargs > env vars > class defaults."""
    if settings is None:
        from .settings import settings as _settings
        settings = _settings

    merged: dict[str, object] = {}
    for f in fields(ClientIPCConfig):
        name = f.name
        env_name = f'ipc_{name}'
        if name in kwargs:
            merged[name] = kwargs[name]
        elif hasattr(settings, env_name) and getattr(settings, env_name) is not None:
            merged[name] = getattr(settings, env_name)

    return ClientIPCConfig(**merged)
