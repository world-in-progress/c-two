"""Process-level configuration read from environment variables."""
from __future__ import annotations

import os

from pydantic_settings import BaseSettings, SettingsConfigDict

_env_file = os.environ.get('C2_ENV_FILE', '.env') or None


class C2Settings(BaseSettings):
    """C-Two runtime settings sourced from environment variables."""

    model_config = SettingsConfigDict(
        env_file=_env_file,
        env_prefix='C2_',
        env_file_encoding='utf-8',
    )

    # Transport addresses
    ipc_address: str | None = None
    relay_address: str | None = None

    # Global hardware param
    shm_threshold: int | None = None            # C2_SHM_THRESHOLD

    # IPC pool overrides (None = use class default)
    ipc_pool_segment_size: int | None = None    # C2_IPC_POOL_SEGMENT_SIZE
    ipc_max_pool_segments: int | None = None    # C2_IPC_MAX_POOL_SEGMENTS
    ipc_max_pool_memory: int | None = None      # C2_IPC_MAX_POOL_MEMORY
    ipc_pool_decay_seconds: float | None = None
    ipc_pool_enabled: bool | None = None

    # IPC frame/payload limits
    ipc_max_frame_size: int | None = None
    ipc_max_payload_size: int | None = None
    ipc_max_pending_requests: int | None = None

    # IPC heartbeat
    ipc_heartbeat_interval: float | None = None
    ipc_heartbeat_timeout: float | None = None

    # IPC chunk
    ipc_max_total_chunks: int | None = None
    ipc_chunk_gc_interval: float | None = None
    ipc_chunk_threshold_ratio: float | None = None
    ipc_chunk_assembler_timeout: float | None = None
    ipc_max_reassembly_bytes: int | None = None
    ipc_chunk_size: int | None = None

    # IPC reassembly pool
    ipc_reassembly_segment_size: int | None = None
    ipc_reassembly_max_segments: int | None = None


settings = C2Settings()
