"""Process-level configuration read from environment variables.

Uses ``pydantic-settings`` to load configuration at import time.

Recognised environment variables:

- ``C2_IPC_ADDRESS`` — override the auto-generated IPC server address.
  When set, :func:`cc.register` uses this address instead of generating
  a random ``ipc://cc_auto_…`` path.
- ``C2_IPC_SEGMENT_SIZE`` — buddy pool segment size in bytes
  (default 268 435 456 = 256 MB).  Determines the maximum single-call
  payload that can be transferred via SHM.
- ``C2_IPC_MAX_SEGMENTS`` — maximum number of buddy pool segments
  (default 4, range 1-255).
- ``C2_RELAY_ADDRESS`` — HTTP address of the relay server for service
  discovery.  When set, :func:`cc.register` automatically registers
  the CRM with the relay via ``POST /_register``.
- ``C2_ENV_FILE`` — path to the ``.env`` file. Set to empty string to
  disable ``.env`` loading entirely (used by the test suite).

Usage::

    # Shell
    export C2_IPC_ADDRESS=ipc://my_server
    export C2_IPC_SEGMENT_SIZE=2147483648   # 2 GB
    export C2_IPC_MAX_SEGMENTS=8
    export C2_RELAY_ADDRESS=http://127.0.0.1:8080

    # Disable .env loading (e.g. in CI or tests)
    export C2_ENV_FILE=

    # Python
    from c_two.transport.config import settings
    print(settings.ipc_address)        # 'ipc://my_server'
    print(settings.relay_address)      # 'http://127.0.0.1:8080'
"""
from __future__ import annotations

import os

from pydantic_settings import BaseSettings, SettingsConfigDict

# Resolve env_file at class-definition time so that tests can set
# C2_ENV_FILE='' *before* importing c_two to suppress .env loading.
_env_file = os.environ.get('C2_ENV_FILE', '.env') or None


class C2Settings(BaseSettings):
    """C-Two runtime settings sourced from environment variables."""

    model_config = SettingsConfigDict(
        env_file=_env_file,
        env_prefix='C2_',
        env_file_encoding='utf-8',
    )

    ipc_address: str | None = None
    ipc_segment_size: int | None = None
    ipc_max_segments: int | None = None
    relay_address: str | None = None


settings = C2Settings()
