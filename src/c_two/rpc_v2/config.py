"""Process-level configuration read from environment variables.

Uses ``pydantic-settings`` to load configuration at import time.

Recognised environment variables:

- ``C2_IPC_ADDRESS`` — override the auto-generated IPC server address.
  When set, :func:`cc.register` uses this address instead of generating
  a random ``ipc-v3://cc_auto_…`` path.

Usage::

    # Shell
    export C2_IPC_ADDRESS=ipc-v3://my_server

    # Python
    from c_two.rpc_v2.config import settings
    print(settings.ipc_address)   # 'ipc-v3://my_server'
"""
from __future__ import annotations

from pydantic_settings import BaseSettings, SettingsConfigDict


class C2Settings(BaseSettings):
    """C-Two runtime settings sourced from environment variables."""

    model_config = SettingsConfigDict(
        env_prefix='C2_',
    )

    ipc_address: str | None = None


settings = C2Settings()
