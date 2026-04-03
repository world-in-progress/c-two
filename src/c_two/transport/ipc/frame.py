"""IPC transport configuration dataclass."""
from __future__ import annotations

from dataclasses import dataclass

DEFAULT_SHM_THRESHOLD = 4_096
DEFAULT_MAX_FRAME_SIZE = 2_147_483_648
DEFAULT_MAX_PAYLOAD_SIZE = 17_179_869_184
DEFAULT_MAX_PENDING_REQUESTS = 1024
DEFAULT_POOL_SEGMENT_SIZE = 268_435_456
DEFAULT_MAX_POOL_SEGMENTS = 4
DEFAULT_MAX_POOL_MEMORY = DEFAULT_POOL_SEGMENT_SIZE * DEFAULT_MAX_POOL_SEGMENTS
DEFAULT_REASSEMBLY_SEGMENT_SIZE = 268_435_456  # 256 MB (client default)
DEFAULT_REASSEMBLY_MAX_SEGMENTS = 4


@dataclass
class IPCConfig:
    """IPC transport configuration.

    Transport limits:
        max_frame_size: Maximum inline frame size in bytes (default 2 GB).
        max_payload_size: Maximum SHM payload size (default 16 GB).
        max_pending_requests: Server-side concurrent request limit (default 1024).

    Performance tuning:
        shm_threshold: Payload size cutover from inline to SHM (default 4 KB).
        pool_enabled: Whether to use pre-allocated pool SHM (default True).
        pool_segment_size: Size of each pool SHM segment (default 256 MB).
        pool_decay_seconds: Idle seconds before pool teardown (default 60).
        max_pool_memory: Memory budget per pool direction (default 1 GB).
        reassembly_segment_size: Size of each reassembly pool segment (default 256 MB).
        reassembly_max_segments: Max segments for reassembly pool (default 4).

    Heartbeat:
        heartbeat_interval: Seconds between PING probes (default 15; <=0 disables).
        heartbeat_timeout: Seconds with no activity before declaring dead (default 30).
    """
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS

    pool_enabled: bool = True
    pool_decay_seconds: float = 60.0
    max_pool_memory: int = DEFAULT_MAX_POOL_MEMORY
    max_pool_segments: int = DEFAULT_MAX_POOL_SEGMENTS
    pool_segment_size: int = DEFAULT_POOL_SEGMENT_SIZE
    reassembly_segment_size: int = DEFAULT_REASSEMBLY_SEGMENT_SIZE
    reassembly_max_segments: int = DEFAULT_REASSEMBLY_MAX_SEGMENTS

    max_total_chunks: int = 512
    chunk_gc_interval: int = 100
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0
    max_reassembly_bytes: int = 8 * (1 << 30)

    heartbeat_interval: float = 15.0
    heartbeat_timeout: float = 30.0

    def __post_init__(self) -> None:
        if self.pool_segment_size > 0xFFFFFFFF:
            raise ValueError(
                f'pool_segment_size {self.pool_segment_size} exceeds uint32 max '
                '(handshake wire format is uint32)'
            )
        if self.pool_segment_size <= 0:
            raise ValueError('pool_segment_size must be > 0')
        if self.max_frame_size <= 16:
            raise ValueError('max_frame_size must be > 16 (header size)')
        if self.max_payload_size <= 0:
            raise ValueError('max_payload_size must be > 0')
        if self.shm_threshold > self.max_frame_size:
            raise ValueError(
                f'shm_threshold ({self.shm_threshold}) must not exceed '
                f'max_frame_size ({self.max_frame_size})'
            )
        if self.pool_segment_size > self.max_payload_size:
            self.pool_segment_size = self.max_payload_size
        if not (1 <= self.max_pool_segments <= 255):
            raise ValueError(
                f'max_pool_segments must be >= 1 and <= 255, got {self.max_pool_segments}'
            )
        if not (1 <= self.reassembly_max_segments <= 255):
            raise ValueError(
                f'reassembly_max_segments must be >= 1 and <= 255, got {self.reassembly_max_segments}'
            )
        if self.reassembly_segment_size <= 0:
            raise ValueError('reassembly_segment_size must be > 0')
        if self.max_pool_memory < self.pool_segment_size:
            raise ValueError(
                f'max_pool_memory ({self.max_pool_memory}) must be >= '
                f'pool_segment_size ({self.pool_segment_size})'
            )
        if self.heartbeat_interval > 0 and self.heartbeat_timeout <= self.heartbeat_interval:
            raise ValueError(
                f'heartbeat_timeout ({self.heartbeat_timeout}) must be > '
                f'heartbeat_interval ({self.heartbeat_interval})'
            )
