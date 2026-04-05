# Config Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify Python↔Rust IPC config with Python as single source of truth, frozen dataclasses, and full FFI pass-through — eliminating config drift.

**Architecture:** Split monolithic `IpcConfig` into `Base/Server/Client` on both sides. Python frozen dataclasses define authoritative defaults; Rust mirrors the split but `Default` is test-only. FFI becomes all-mandatory (no `..default()`). Priority chain: `kwargs > .env > class defaults`.

**Tech Stack:** Python 3.10+ dataclasses (frozen), pydantic-settings, Rust structs, PyO3/maturin FFI

**Spec:** `docs/superpowers/specs/2026-04-05-config-architecture-design.md`

---

## File Structure

### New Files
- `src/c_two/config/__init__.py` — Package exports
- `src/c_two/config/settings.py` — `C2Settings` (migrated + extended)
- `src/c_two/config/ipc.py` — `BaseIPCConfig`, `ServerIPCConfig`, `ClientIPCConfig`, `build_server_config()`, `build_client_config()`

### Modified Files (Rust)
- `src/c_two/_native/c2-config/src/ipc.rs` — Split `IpcConfig` → `BaseIpcConfig` + `ServerIpcConfig` + `ClientIpcConfig`
- `src/c_two/_native/c2-config/src/lib.rs` — Update exports
- `src/c_two/_native/c2-server/src/config.rs` — Re-export `ServerIpcConfig`
- `src/c_two/_native/c2-server/src/server.rs` — Use `ServerIpcConfig`
- `src/c_two/_native/c2-server/src/heartbeat.rs` — Use `ServerIpcConfig`
- `src/c_two/_native/c2-server/src/lib.rs` — Re-export
- `src/c_two/_native/c2-ipc/src/client.rs` — Use `ClientIpcConfig`
- `src/c_two/_native/c2-ipc/src/pool.rs` — Use `ClientIpcConfig`
- `src/c_two/_native/c2-ipc/src/sync_client.rs` — Use `ClientIpcConfig`
- `src/c_two/_native/c2-ipc/src/lib.rs` — Re-export
- `src/c_two/_native/c2-chunk/src/config.rs` — `from_ipc()` → `from_base()`
- `src/c_two/_native/c2-ffi/src/server_ffi.rs` — All-mandatory FFI
- `src/c_two/_native/c2-ffi/src/client_ffi.rs` — All-mandatory FFI

### Modified Files (Python)
- `src/c_two/__init__.py` — Update exports
- `src/c_two/transport/__init__.py` — Update lazy imports
- `src/c_two/transport/registry.py` — New API + config builders
- `src/c_two/transport/server/native.py` — Full-field pass-through
- `src/c_two/transport/ipc/__init__.py` — Update imports

### Deleted Files
- `src/c_two/transport/config.py` — Replaced by `config/settings.py`
- `src/c_two/transport/ipc/frame.py` — Replaced by `config/ipc.py`

### Test Files
- `tests/unit/test_ipc_config.py` — NEW: Python config unit tests
- `tests/integration/test_heartbeat.py` — Update imports
- `tests/integration/test_server.py` — Update imports
- `tests/integration/test_concurrency_safety.py` — Update imports + API
- `tests/integration/test_dynamic_pool.py` — Update imports

---

## Phase 1: Rust Internal Split (backward-compatible FFI)

> FFI signatures stay unchanged — only internal Rust types change.
> All existing Python code continues to work after this phase.

### Task 1: Split c2-config IpcConfig into Base/Server/Client

**Files:**
- Modify: `src/c_two/_native/c2-config/src/ipc.rs` (full rewrite)
- Modify: `src/c_two/_native/c2-config/src/lib.rs`

- [ ] **Step 1: Rewrite `c2-config/src/ipc.rs`**

Replace the monolithic `IpcConfig` with three structs. Key design:
- `BaseIpcConfig`: pool + chunk + reassembly fields shared by both roles
- `ServerIpcConfig`: wraps `base: BaseIpcConfig` + server-only fields (frame limits, heartbeat, decay, pending requests, shm_threshold)
- `ClientIpcConfig`: wraps `base: BaseIpcConfig` + `shm_threshold`
- Both `ServerIpcConfig` and `ClientIpcConfig` impl `Deref<Target=BaseIpcConfig>` for ergonomic field access
- Type change: `chunk_size` from `usize` → `u64` (per spec: all sizes are `u64`)
- Rename time fields to `_secs` suffix: `chunk_gc_interval` → `chunk_gc_interval_secs`, `chunk_assembler_timeout` → `chunk_assembler_timeout_secs`, `heartbeat_interval` → `heartbeat_interval_secs`, `heartbeat_timeout` → `heartbeat_timeout_secs`

```rust
// c2-config/src/ipc.rs

//! IPC transport configuration — Base / Server / Client split.
//!
//! Python is the single source of truth for defaults.  Rust `Default` impls
//! exist for testing only and MUST match Python `config/ipc.py` defaults.

/// Configuration shared by both server and client IPC transport.
#[derive(Debug, Clone)]
pub struct BaseIpcConfig {
    // Pool SHM
    pub pool_enabled: bool,
    pub pool_segment_size: u64,
    pub max_pool_segments: u32,
    pub max_pool_memory: u64,
    // Reassembly pool
    pub reassembly_segment_size: u64,
    pub reassembly_max_segments: u32,
    // Chunked transfer
    pub max_total_chunks: u32,
    pub chunk_gc_interval_secs: f64,
    pub chunk_threshold_ratio: f64,
    pub chunk_assembler_timeout_secs: f64,
    pub max_reassembly_bytes: u64,
    pub chunk_size: u64,
}

impl Default for BaseIpcConfig {
    fn default() -> Self {
        Self {
            pool_enabled: true,
            pool_segment_size: 268_435_456,       // 256 MB
            max_pool_segments: 4,
            max_pool_memory: 1_073_741_824,       // 1 GB
            reassembly_segment_size: 268_435_456, // 256 MB
            reassembly_max_segments: 4,
            max_total_chunks: 512,
            chunk_gc_interval_secs: 5.0,
            chunk_threshold_ratio: 0.9,
            chunk_assembler_timeout_secs: 60.0,
            max_reassembly_bytes: 8_589_934_592,  // 8 GB
            chunk_size: 131_072,                  // 128 KB
        }
    }
}

impl BaseIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.pool_segment_size == 0 {
            return Err("pool_segment_size must be > 0".into());
        }
        if self.pool_segment_size > u32::MAX as u64 {
            return Err(format!(
                "pool_segment_size ({}) must be <= u32::MAX for wire compat",
                self.pool_segment_size,
            ));
        }
        if self.chunk_size == 0 {
            return Err("chunk_size must be > 0".into());
        }
        if !(0.0 < self.chunk_threshold_ratio && self.chunk_threshold_ratio <= 1.0) {
            return Err("chunk_threshold_ratio must be in (0, 1]".into());
        }
        Ok(())
    }
}

/// Server-side IPC configuration.
#[derive(Debug, Clone)]
pub struct ServerIpcConfig {
    pub base: BaseIpcConfig,
    pub shm_threshold: u64,
    pub max_frame_size: u64,
    pub max_payload_size: u64,
    pub max_pending_requests: u32,
    pub pool_decay_seconds: f64,
    pub heartbeat_interval_secs: f64,
    pub heartbeat_timeout_secs: f64,
}

impl std::ops::Deref for ServerIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}

impl Default for ServerIpcConfig {
    fn default() -> Self {
        Self {
            base: BaseIpcConfig::default(),
            shm_threshold: 4_096,
            max_frame_size: 2_147_483_648,       // 2 GB
            max_payload_size: 17_179_869_184,     // 16 GB
            max_pending_requests: 1024,
            pool_decay_seconds: 60.0,
            heartbeat_interval_secs: 15.0,
            heartbeat_timeout_secs: 30.0,
        }
    }
}

impl ServerIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.base.validate()?;
        if self.max_frame_size <= 16 {
            return Err(format!(
                "max_frame_size ({}) must be > 16 (header size)", self.max_frame_size,
            ));
        }
        if self.max_payload_size == 0 {
            return Err("max_payload_size must be > 0".into());
        }
        if self.shm_threshold > self.max_frame_size {
            return Err(format!(
                "shm_threshold ({}) must not exceed max_frame_size ({})",
                self.shm_threshold, self.max_frame_size,
            ));
        }
        Ok(())
    }
}

/// Client-side IPC configuration.
#[derive(Debug, Clone)]
pub struct ClientIpcConfig {
    pub base: BaseIpcConfig,
    pub shm_threshold: u64,
}

impl std::ops::Deref for ClientIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}

impl Default for ClientIpcConfig {
    fn default() -> Self {
        Self {
            base: BaseIpcConfig {
                reassembly_segment_size: 67_108_864,  // 64 MB (client override)
                ..BaseIpcConfig::default()
            },
            shm_threshold: 4_096,
        }
    }
}

impl ClientIpcConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.base.validate()
    }
}
```

Tests: update all existing tests to use new types. Server-related tests use `ServerIpcConfig`, validation tests split between `BaseIpcConfig::validate()` and `ServerIpcConfig::validate()`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_default_validates() {
        assert!(ServerIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn client_default_validates() {
        assert!(ClientIpcConfig::default().validate().is_ok());
    }

    #[test]
    fn client_reassembly_override() {
        let c = ClientIpcConfig::default();
        assert_eq!(c.reassembly_segment_size, 67_108_864); // 64 MB
        let s = ServerIpcConfig::default();
        assert_eq!(s.reassembly_segment_size, 268_435_456); // 256 MB
    }

    #[test]
    fn deref_access() {
        let cfg = ServerIpcConfig::default();
        let _: u64 = cfg.pool_segment_size; // via Deref
    }

    #[test]
    fn reject_zero_segment_size() {
        let cfg = ServerIpcConfig {
            base: BaseIpcConfig { pool_segment_size: 0, ..Default::default() },
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("pool_segment_size must be > 0"));
    }

    #[test]
    fn reject_oversized_segment() {
        let cfg = ServerIpcConfig {
            base: BaseIpcConfig {
                pool_segment_size: u64::from(u32::MAX) + 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("u32::MAX"));
    }

    #[test]
    fn reject_small_frame_size() {
        let cfg = ServerIpcConfig { max_frame_size: 16, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("max_frame_size"));
    }

    #[test]
    fn reject_zero_payload_size() {
        let cfg = ServerIpcConfig { max_payload_size: 0, ..Default::default() };
        assert!(cfg.validate().unwrap_err().contains("max_payload_size must be > 0"));
    }

    #[test]
    fn reject_threshold_exceeds_frame() {
        let cfg = ServerIpcConfig {
            shm_threshold: 1000,
            max_frame_size: 500,
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("shm_threshold"));
    }

    #[test]
    fn reject_zero_chunk_size() {
        let cfg = ServerIpcConfig {
            base: BaseIpcConfig { chunk_size: 0, ..Default::default() },
            ..Default::default()
        };
        assert!(cfg.validate().unwrap_err().contains("chunk_size must be > 0"));
    }

    #[test]
    fn chunk_gc_interval_is_f64() {
        let cfg = BaseIpcConfig::default();
        assert!((cfg.chunk_gc_interval_secs - 5.0).abs() < f64::EPSILON);
    }
}
```

- [ ] **Step 2: Update `c2-config/src/lib.rs`**

```rust
pub use ipc::{BaseIpcConfig, ServerIpcConfig, ClientIpcConfig};
```

Remove old `pub use ipc::IpcConfig;`.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/_native/c2-config/
git commit -m "refactor(c2-config): split IpcConfig into Base/Server/Client

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: Update Rust consumers (c2-server, c2-ipc, c2-chunk)

**Files:**
- Modify: `src/c_two/_native/c2-server/src/config.rs`
- Modify: `src/c_two/_native/c2-server/src/server.rs`
- Modify: `src/c_two/_native/c2-server/src/heartbeat.rs`
- Modify: `src/c_two/_native/c2-server/src/lib.rs`
- Modify: `src/c_two/_native/c2-ipc/src/client.rs`
- Modify: `src/c_two/_native/c2-ipc/src/pool.rs`
- Modify: `src/c_two/_native/c2-ipc/src/sync_client.rs`
- Modify: `src/c_two/_native/c2-ipc/src/lib.rs`
- Modify: `src/c_two/_native/c2-ipc/src/tests.rs` (if exists)
- Modify: `src/c_two/_native/c2-chunk/src/config.rs`

- [ ] **Step 1: Update c2-server**

`config.rs`:
```rust
pub use c2_config::ServerIpcConfig;
```

`server.rs` changes:
- `use crate::config::IpcConfig;` → `use crate::config::ServerIpcConfig;`
- `config: IpcConfig` field → `config: ServerIpcConfig`
- `pub fn new(address: &str, config: IpcConfig)` → `pub fn new(address: &str, config: ServerIpcConfig)`
- `config.heartbeat_interval` → `config.heartbeat_interval_secs`
- `config.heartbeat_timeout` → `config.heartbeat_timeout_secs`
- `config.chunk_assembler_timeout` → `config.chunk_assembler_timeout_secs` (if referenced)
- `ChunkConfig::from_ipc(&config)` → `ChunkConfig::from_base(&config)` (Deref coerces `&ServerIpcConfig` to `&BaseIpcConfig`)
- Test code: `IpcConfig::default()` → `ServerIpcConfig::default()`, override patterns use nested `base:` for base fields

`heartbeat.rs`:
- `use crate::config::IpcConfig;` → `use crate::config::ServerIpcConfig;`
- `config: &IpcConfig` → `config: &ServerIpcConfig`
- `config.heartbeat_interval` → `config.heartbeat_interval_secs`
- `config.heartbeat_timeout` → `config.heartbeat_timeout_secs`
- Test config construction: use `ServerIpcConfig { heartbeat_interval_secs: ..., ... }`

`lib.rs`:
- `pub use config::IpcConfig;` → `pub use config::ServerIpcConfig;`

- [ ] **Step 2: Update c2-ipc**

`client.rs`:
- `pub use c2_config::IpcConfig;` → `pub use c2_config::ClientIpcConfig;`
- `config: IpcConfig` field → `config: ClientIpcConfig`
- `IpcConfig::default()` → `ClientIpcConfig::default()`
- `fn make_chunk_registry(config: &IpcConfig)` → `fn make_chunk_registry(config: &ClientIpcConfig)`
- `ChunkConfig::from_ipc(config)` → `ChunkConfig::from_base(config)` (Deref coerces)
- `config.chunk_size` stays (via Deref to `BaseIpcConfig`)
- `config.shm_threshold` stays (directly on `ClientIpcConfig`)
- `with_pool(address, pool, config: IpcConfig)` → `config: ClientIpcConfig`
- `connect_with_config(config: IpcConfig, ...)` → `config: ClientIpcConfig`

`pool.rs`:
- `use crate::client::{IpcConfig, IpcError};` → `use crate::client::{ClientIpcConfig, IpcError};`
- `default_config: StdMutex<Option<IpcConfig>>` → `StdMutex<Option<ClientIpcConfig>>`
- `pub fn set_default_config(&self, config: IpcConfig)` → `config: ClientIpcConfig`
- `pub fn acquire(&self, address: &str, config: Option<&IpcConfig>)` → `Option<&ClientIpcConfig>`
- Config resolution: `IpcConfig::default()` → `ClientIpcConfig::default()`
- Test code: update accordingly

`sync_client.rs`:
- `use crate::client::{IpcClient, IpcConfig, ...}` → `ClientIpcConfig`
- `config: IpcConfig` field → `config: ClientIpcConfig`

`lib.rs`:
- `pub use client::{IpcClient, IpcConfig, ...}` → `ClientIpcConfig`

- [ ] **Step 3: Update c2-chunk**

`config.rs`:
- `pub fn from_ipc(cfg: &c2_config::IpcConfig)` → `pub fn from_base(cfg: &c2_config::BaseIpcConfig)`
- Field access: `cfg.chunk_assembler_timeout` → `cfg.chunk_assembler_timeout_secs`
- Field access: `cfg.chunk_gc_interval` → `cfg.chunk_gc_interval_secs`
- `cfg.chunk_size` stays but type changes from `usize` to `u64`, so cast: `cfg.chunk_size as usize` where needed (e.g., `max_chunks_per_request`)
- Test: `c2_config::IpcConfig::default()` → `c2_config::BaseIpcConfig::default()`
- Test function: `from_ipc_config` → `from_base_config`

```rust
impl ChunkConfig {
    pub fn from_base(cfg: &c2_config::BaseIpcConfig) -> Self {
        Self {
            assembler_timeout: Duration::from_secs_f64(cfg.chunk_assembler_timeout_secs),
            gc_interval: Duration::from_secs_f64(cfg.chunk_gc_interval_secs),
            soft_limit: cfg.max_total_chunks,
            max_reassembly_bytes: cfg.max_reassembly_bytes,
            max_chunks_per_request: cfg.max_total_chunks as usize,
            max_bytes_per_request: cfg.max_reassembly_bytes as usize,
        }
    }
}
```

- [ ] **Step 4: Update c2-ffi (internal only — keep FFI params unchanged)**

`server_ffi.rs`:
- `use c2_config::IpcConfig;` → `use c2_config::{BaseIpcConfig, ServerIpcConfig};`
- `RustServer::new()` body: construct `ServerIpcConfig` instead of `IpcConfig`

```rust
// Inside RustServer::new() body (params unchanged):
let config = ServerIpcConfig {
    base: BaseIpcConfig {
        pool_segment_size: segment_size,
        max_pool_segments,
        reassembly_segment_size,
        reassembly_max_segments,
        chunk_threshold_ratio: if max_payload_size > 0 {
            chunked_threshold as f64 / max_payload_size as f64
        } else {
            0.9
        },
        ..BaseIpcConfig::default()
    },
    shm_threshold,
    max_frame_size,
    max_payload_size,
    heartbeat_interval_secs: heartbeat_interval,
    heartbeat_timeout_secs: heartbeat_timeout,
    ..ServerIpcConfig::default()
};
```

`client_ffi.rs`:
- `use c2_config::IpcConfig;` → `use c2_config::{BaseIpcConfig, ClientIpcConfig};`
- `RustClient::new()` body: construct `ClientIpcConfig`
- `RustClientPool::acquire()` body: construct `ClientIpcConfig`
- `RustClientPool::set_default_config()` body: construct `ClientIpcConfig`

```rust
// Inside RustClient::new() body (params unchanged):
let config = ClientIpcConfig {
    base: BaseIpcConfig {
        chunk_size: chunk_size as u64,
        pool_segment_size: pool_segment_size
            .unwrap_or(BaseIpcConfig::default().pool_segment_size),
        reassembly_segment_size: reassembly_segment_size
            .unwrap_or(ClientIpcConfig::default().reassembly_segment_size),
        reassembly_max_segments: reassembly_max_segments
            .unwrap_or(ClientIpcConfig::default().reassembly_max_segments),
        ..BaseIpcConfig::default()
    },
    shm_threshold,
};
```

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/
git commit -m "refactor(rust): update all crates to use split IpcConfig types

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 3: Verify Rust compilation and tests

- [ ] **Step 1: Cargo check**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: compiles with no errors (warnings OK)

- [ ] **Step 2: Cargo test**

Run: `cd src/c_two/_native && cargo test -p c2-config -p c2-server -p c2-chunk --no-default-features`
Expected: all tests pass

- [ ] **Step 3: Quick Python rebuild test**

Run: `uv sync --reinstall-package c-two`
Expected: builds successfully (FFI params unchanged → Python still works)

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/ -q --timeout=30`
Expected: all unit tests pass

---

## Phase 2: Python Config Foundation

> New Python config classes created alongside old ones. No existing code modified yet.

### Task 4: Create Python config package

**Files:**
- Create: `src/c_two/config/__init__.py`
- Create: `src/c_two/config/settings.py`
- Create: `src/c_two/config/ipc.py`

- [ ] **Step 1: Create `src/c_two/config/__init__.py`**

```python
"""C-Two configuration."""
from __future__ import annotations

from .settings import C2Settings, settings
from .ipc import (
    BaseIPCConfig,
    ServerIPCConfig,
    ClientIPCConfig,
    build_server_config,
    build_client_config,
)

__all__ = [
    'C2Settings',
    'settings',
    'BaseIPCConfig',
    'ServerIPCConfig',
    'ClientIPCConfig',
    'build_server_config',
    'build_client_config',
]
```

- [ ] **Step 2: Create `src/c_two/config/settings.py`**

Migrate from `transport/config.py` and extend with all IPC env var fields.

```python
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
```

- [ ] **Step 3: Create `src/c_two/config/ipc.py` — dataclass skeleton**

Start with `BaseIPCConfig` and validation:

```python
"""IPC transport configuration — frozen dataclasses."""
from __future__ import annotations

from dataclasses import dataclass, fields
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .settings import C2Settings


@dataclass(frozen=True)
class BaseIPCConfig:
    """Configuration shared by both server and client IPC transport."""

    pool_enabled: bool = True
    pool_segment_size: int = 268_435_456           # 256 MB
    max_pool_segments: int = 4
    max_pool_memory: int = 1_073_741_824           # 1 GB

    reassembly_segment_size: int = 268_435_456     # 256 MB
    reassembly_max_segments: int = 4

    max_total_chunks: int = 512
    chunk_gc_interval: float = 5.0
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0
    max_reassembly_bytes: int = 8_589_934_592      # 8 GB
    chunk_size: int = 131_072                      # 128 KB

    def __post_init__(self) -> None:
        if self.pool_segment_size <= 0:
            raise ValueError('pool_segment_size must be positive')
        if self.pool_segment_size > 0xFFFFFFFF:
            raise ValueError(
                f'pool_segment_size {self.pool_segment_size} exceeds '
                'uint32 max (handshake wire format is uint32)'
            )
        if not (1 <= self.max_pool_segments <= 255):
            raise ValueError(
                f'max_pool_segments must be 1..255, got {self.max_pool_segments}'
            )
        if not (0.0 < self.chunk_threshold_ratio <= 1.0):
            raise ValueError('chunk_threshold_ratio must be in (0, 1]')
        if self.chunk_gc_interval <= 0:
            raise ValueError('chunk_gc_interval must be positive')
        if self.chunk_assembler_timeout <= 0:
            raise ValueError('chunk_assembler_timeout must be positive')
        if not (1 <= self.reassembly_max_segments <= 255):
            raise ValueError(
                f'reassembly_max_segments must be 1..255, '
                f'got {self.reassembly_max_segments}'
            )
        if self.reassembly_segment_size <= 0:
            raise ValueError('reassembly_segment_size must be positive')
```

- [ ] **Step 4: Add `ServerIPCConfig` and `ClientIPCConfig` to `ipc.py`**

```python
@dataclass(frozen=True)
class ServerIPCConfig(BaseIPCConfig):
    """Server-side IPC configuration."""

    max_frame_size: int = 2_147_483_648            # 2 GB
    max_payload_size: int = 17_179_869_184         # 16 GB
    max_pending_requests: int = 1024
    pool_decay_seconds: float = 60.0
    heartbeat_interval: float = 15.0
    heartbeat_timeout: float = 30.0

    def __post_init__(self) -> None:
        super().__post_init__()
        if self.max_frame_size <= 16:
            raise ValueError('max_frame_size must be > 16 (header size)')
        if self.max_payload_size <= 0:
            raise ValueError('max_payload_size must be positive')
        if self.shm_threshold_check:
            pass  # checked externally with shm_threshold
        if self.heartbeat_interval < 0:
            raise ValueError(
                'heartbeat_interval must be >= 0 (0 disables heartbeat)'
            )
        if (
            self.heartbeat_interval > 0
            and self.heartbeat_timeout <= self.heartbeat_interval
        ):
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
    """Client-side IPC configuration.

    Overrides reassembly_segment_size to 64 MB (vs Base's 256 MB)
    because a client process may connect to many servers.
    """
    reassembly_segment_size: int = 67_108_864      # 64 MB
```

**Note:** Remove the `self.shm_threshold_check` placeholder — `shm_threshold` is validated in `build_server_config()` where both values are known.

- [ ] **Step 5: Add build functions to `ipc.py`**

```python
def build_server_config(
    settings: C2Settings | None = None,
    **kwargs: object,
) -> ServerIPCConfig:
    """Build a frozen ServerIPCConfig.

    Priority: kwargs > env vars > class defaults.
    Clamping: pool_segment_size is clamped to max_payload_size.
    """
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
    if seg > pay:
        merged['pool_segment_size'] = pay

    # Auto-derive max_pool_memory if not explicitly set
    if 'max_pool_memory' not in merged:
        seg_final = merged.get('pool_segment_size', ServerIPCConfig.pool_segment_size)
        segs = merged.get('max_pool_segments', ServerIPCConfig.max_pool_segments)
        merged['max_pool_memory'] = seg_final * segs

    return ServerIPCConfig(**merged)


def build_client_config(
    settings: C2Settings | None = None,
    **kwargs: object,
) -> ClientIPCConfig:
    """Build a frozen ClientIPCConfig.

    Priority: kwargs > env vars > class defaults.
    """
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
```

- [ ] **Step 6: Commit**

```bash
git add src/c_two/config/
git commit -m "feat(config): add Python config package with frozen IPC dataclasses

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 5: Python config unit tests

**Files:**
- Create: `tests/unit/test_ipc_config.py`

- [ ] **Step 1: Write config construction + immutability tests**

```python
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
        with pytest.raises(ValueError, match='pool_segment_size must be positive'):
            BaseIPCConfig(pool_segment_size=0)

    def test_reject_oversized_segment(self):
        with pytest.raises(ValueError, match='uint32'):
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


class TestClientIPCConfig:
    def test_reassembly_override(self):
        cfg = ClientIPCConfig()
        assert cfg.reassembly_segment_size == 67_108_864  # 64 MB

    def test_base_defaults_preserved(self):
        cfg = ClientIPCConfig()
        assert cfg.pool_segment_size == 268_435_456  # 256 MB from Base
```

- [ ] **Step 2: Write build function + priority chain tests**

```python
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
```

- [ ] **Step 3: Run tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_ipc_config.py -v`
Expected: all tests pass

- [ ] **Step 4: Commit**

```bash
git add tests/unit/test_ipc_config.py
git commit -m "test: add unit tests for frozen IPC config classes

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---


## Phase 3: Integration Cut-over

> This phase atomically switches FFI + Python callers together.
> Between Tasks 6-8 the build may be broken — Task 9 rebuilds and verifies.

### Task 6: Server-side integration (registry + native.py + server FFI)

**Files:**
- Modify: `src/c_two/transport/registry.py`
- Modify: `src/c_two/transport/server/native.py`
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs`

- [ ] **Step 1: Update registry server-side state and methods**

In `registry.py`, update `_ProcessRegistry.__init__` to store new config types:

```python
# Replace these fields:
#   self._explicit_ipc_config: IPCConfig | None = None
#   self._shm_threshold: int | None = None
# With:
self._server_config: ServerIPCConfig | None = None
self._client_config: ClientIPCConfig | None = None
self._shm_threshold: int | None = None
self._server_kwargs: dict[str, object] = {}
self._client_kwargs: dict[str, object] = {}
```

Add import at top of registry.py:
```python
from c_two.config.ipc import (
    ServerIPCConfig, ClientIPCConfig,
    build_server_config, build_client_config,
)
from c_two.config.settings import settings
```

Replace `set_server_ipc_config()` method with `set_server()`:
```python
def set_server(self, **kwargs: object) -> None:
    """Configure IPC server. Must be called before register()."""
    with self._lock:
        if self._server is not None:
            import warnings
            warnings.warn(
                'Server already started, set_server() ignored. '
                'Call set_server() before register().',
                UserWarning,
                stacklevel=3,
            )
            return
        self._server_kwargs = kwargs
        self._server_config = None  # rebuilt lazily
```

Replace `_build_ipc_config()` with:
```python
def _build_server_config(self) -> ServerIPCConfig:
    if self._server_config is not None:
        return self._server_config
    self._server_config = build_server_config(settings, **self._server_kwargs)
    return self._server_config
```

- [ ] **Step 2: Update `NativeServerBridge` to pass ALL fields**

In `native.py`, replace `IPCConfig` import with new types. Update `__init__` to accept `ServerIPCConfig` and pass all fields:

```python
from c_two.config.ipc import ServerIPCConfig

# In __init__:
self._config = ipc_config  # ServerIPCConfig (frozen)

self._rust_server = RustServer(
    address=bind_address,
    shm_threshold=shm_threshold,
    pool_enabled=self._config.pool_enabled,
    pool_segment_size=self._config.pool_segment_size,
    max_pool_segments=self._config.max_pool_segments,
    max_pool_memory=self._config.max_pool_memory,
    reassembly_segment_size=self._config.reassembly_segment_size,
    reassembly_max_segments=self._config.reassembly_max_segments,
    max_frame_size=self._config.max_frame_size,
    max_payload_size=self._config.max_payload_size,
    max_pending_requests=self._config.max_pending_requests,
    pool_decay_seconds=self._config.pool_decay_seconds,
    heartbeat_interval=self._config.heartbeat_interval,
    heartbeat_timeout=self._config.heartbeat_timeout,
    max_total_chunks=self._config.max_total_chunks,
    chunk_gc_interval=self._config.chunk_gc_interval,
    chunk_threshold_ratio=self._config.chunk_threshold_ratio,
    chunk_assembler_timeout=self._config.chunk_assembler_timeout,
    max_reassembly_bytes=self._config.max_reassembly_bytes,
    chunk_size=self._config.chunk_size,
)
```

- [ ] **Step 3: Update `server_ffi.rs` to all-mandatory params**

Remove all default values from `#[pyo3(signature)]`. All params mandatory. Remove `chunked_threshold` param — `chunk_threshold_ratio` is passed directly. Body constructs `ServerIpcConfig` (already done in Task 2 Step 4, just remove the `..default()` fallbacks).

```rust
#[pyo3(signature = (
    address, shm_threshold, pool_enabled, pool_segment_size,
    max_pool_segments, max_pool_memory, reassembly_segment_size,
    reassembly_max_segments, max_frame_size, max_payload_size,
    max_pending_requests, pool_decay_seconds, heartbeat_interval,
    heartbeat_timeout, max_total_chunks, chunk_gc_interval,
    chunk_threshold_ratio, chunk_assembler_timeout,
    max_reassembly_bytes, chunk_size,
))]
fn new(
    address: &str,
    shm_threshold: u64, pool_enabled: bool,
    pool_segment_size: u64, max_pool_segments: u32, max_pool_memory: u64,
    reassembly_segment_size: u64, reassembly_max_segments: u32,
    max_frame_size: u64, max_payload_size: u64, max_pending_requests: u32,
    pool_decay_seconds: f64, heartbeat_interval: f64, heartbeat_timeout: f64,
    max_total_chunks: u32, chunk_gc_interval: f64, chunk_threshold_ratio: f64,
    chunk_assembler_timeout: f64, max_reassembly_bytes: u64, chunk_size: u64,
) -> PyResult<Self> {
    let config = ServerIpcConfig {
        base: BaseIpcConfig {
            pool_enabled, pool_segment_size, max_pool_segments, max_pool_memory,
            reassembly_segment_size, reassembly_max_segments, max_total_chunks,
            chunk_gc_interval_secs: chunk_gc_interval,
            chunk_threshold_ratio,
            chunk_assembler_timeout_secs: chunk_assembler_timeout,
            max_reassembly_bytes, chunk_size,
        },
        shm_threshold, max_frame_size, max_payload_size, max_pending_requests,
        pool_decay_seconds,
        heartbeat_interval_secs: heartbeat_interval,
        heartbeat_timeout_secs: heartbeat_timeout,
    };
    // ... Server::new(address, config) unchanged
}
```

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/registry.py src/c_two/transport/server/native.py \
  src/c_two/_native/c2-ffi/src/server_ffi.rs
git commit -m "refactor: server-side config integration (registry + native + FFI)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 7: Client-side integration (registry + client FFI)

**Files:**
- Modify: `src/c_two/transport/registry.py`
- Modify: `src/c_two/_native/c2-ffi/src/client_ffi.rs`

- [ ] **Step 1: Add `set_client()` to registry**

Replace `set_client_ipc_config()` with `set_client()`:
```python
def set_client(self, **kwargs: object) -> None:
    """Configure IPC client. Must be called before connect()."""
    with self._lock:
        # Warn if connections already exist
        if self._pool_config_applied:
            import warnings
            warnings.warn(
                'Client connections already exist, set_client() ignored. '
                'Call set_client() before connect().',
                UserWarning,
                stacklevel=3,
            )
            return
        self._client_kwargs = kwargs
        self._client_config = None  # rebuilt lazily
```

Add `_build_client_config()`:
```python
def _build_client_config(self) -> ClientIPCConfig:
    if self._client_config is not None:
        return self._client_config
    self._client_config = build_client_config(settings, **self._client_kwargs)
    return self._client_config
```

Update the `connect()` method to push resolved config into `RustClientPool`:
```python
# In connect(), before acquiring from pool:
cfg = self._build_client_config()
shm = self._shm_threshold or settings.shm_threshold or 4096
if not self._pool_config_applied:
    self._pool.set_default_config(
        shm_threshold=shm,
        pool_enabled=cfg.pool_enabled,
        pool_segment_size=cfg.pool_segment_size,
        max_pool_segments=cfg.max_pool_segments,
        max_pool_memory=cfg.max_pool_memory,
        reassembly_segment_size=cfg.reassembly_segment_size,
        reassembly_max_segments=cfg.reassembly_max_segments,
        max_total_chunks=cfg.max_total_chunks,
        chunk_gc_interval=cfg.chunk_gc_interval,
        chunk_threshold_ratio=cfg.chunk_threshold_ratio,
        chunk_assembler_timeout=cfg.chunk_assembler_timeout,
        max_reassembly_bytes=cfg.max_reassembly_bytes,
        chunk_size=cfg.chunk_size,
    )
    self._pool_config_applied = True
```

**Bug fix (critic finding):** Current `set_client_ipc_config()` accepts `max_segments` but never passes it to `_pool.set_default_config()`. The new code passes ALL fields — bug eliminated by design.

- [ ] **Step 2: Update `client_ffi.rs` — all-mandatory for `set_default_config` and simplified `acquire`**

`set_default_config()` — all mandatory, no `..default()`:
```rust
#[pyo3(signature = (
    shm_threshold, pool_enabled, pool_segment_size, max_pool_segments,
    max_pool_memory, reassembly_segment_size, reassembly_max_segments,
    max_total_chunks, chunk_gc_interval, chunk_threshold_ratio,
    chunk_assembler_timeout, max_reassembly_bytes, chunk_size,
))]
fn set_default_config(
    &self,
    shm_threshold: u64, pool_enabled: bool, pool_segment_size: u64,
    max_pool_segments: u32, max_pool_memory: u64,
    reassembly_segment_size: u64, reassembly_max_segments: u32,
    max_total_chunks: u32, chunk_gc_interval: f64,
    chunk_threshold_ratio: f64, chunk_assembler_timeout: f64,
    max_reassembly_bytes: u64, chunk_size: u64,
) {
    let config = ClientIpcConfig {
        base: BaseIpcConfig {
            pool_enabled, pool_segment_size, max_pool_segments, max_pool_memory,
            reassembly_segment_size, reassembly_max_segments, max_total_chunks,
            chunk_gc_interval_secs: chunk_gc_interval,
            chunk_threshold_ratio,
            chunk_assembler_timeout_secs: chunk_assembler_timeout,
            max_reassembly_bytes, chunk_size,
        },
        shm_threshold,
    };
    self.inner.set_default_config(config);
}
```

`acquire()` — simplified, no config overrides:
```rust
#[pyo3(signature = (address,))]
fn acquire(&self, py: Python<'_>, address: &str) -> PyResult<PyRustClient> {
    let addr = address.to_string();
    let pool = self.inner;
    let client = py
        .allow_threads(move || pool.acquire(&addr, None))
        .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
    Ok(PyRustClient { inner: client })
}
```

`RustClient::new()` — also all-mandatory:
```rust
#[pyo3(signature = (
    address, shm_threshold, pool_enabled, pool_segment_size,
    max_pool_segments, max_pool_memory, reassembly_segment_size,
    reassembly_max_segments, max_total_chunks, chunk_gc_interval,
    chunk_threshold_ratio, chunk_assembler_timeout,
    max_reassembly_bytes, chunk_size,
))]
fn new(
    py: Python<'_>, address: &str,
    shm_threshold: u64, pool_enabled: bool,
    pool_segment_size: u64, max_pool_segments: u32, max_pool_memory: u64,
    reassembly_segment_size: u64, reassembly_max_segments: u32,
    max_total_chunks: u32, chunk_gc_interval: f64,
    chunk_threshold_ratio: f64, chunk_assembler_timeout: f64,
    max_reassembly_bytes: u64, chunk_size: u64,
) -> PyResult<Self> {
    let config = ClientIpcConfig {
        base: BaseIpcConfig {
            pool_enabled, pool_segment_size, max_pool_segments, max_pool_memory,
            reassembly_segment_size, reassembly_max_segments, max_total_chunks,
            chunk_gc_interval_secs: chunk_gc_interval,
            chunk_threshold_ratio,
            chunk_assembler_timeout_secs: chunk_assembler_timeout,
            max_reassembly_bytes, chunk_size,
        },
        shm_threshold,
    };
    // ... SyncClient::connect with config
}
```

- [ ] **Step 3: Commit**

```bash
git add src/c_two/transport/registry.py src/c_two/_native/c2-ffi/src/client_ffi.rs
git commit -m "refactor: client-side config integration (registry + FFI)

Fixes: set_client max_segments bug (was accepted but never applied)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 8: API surface + exports

**Files:**
- Modify: `src/c_two/transport/registry.py` (module-level functions)
- Modify: `src/c_two/__init__.py`
- Modify: `src/c_two/transport/__init__.py`

- [ ] **Step 1: Add `set_config()` to registry**

```python
# In _ProcessRegistry:
def set_config(self, *, shm_threshold: int | None = None) -> None:
    """Set global config. Must be called before register()/connect()."""
    with self._lock:
        if self._server is not None or self._pool_config_applied:
            import warnings
            warnings.warn(
                'Active connections exist, set_config() ignored. '
                'Call set_config() before register()/connect().',
                UserWarning,
                stacklevel=3,
            )
            return
        if shm_threshold is not None:
            if shm_threshold <= 0:
                raise ValueError(f'shm_threshold must be > 0, got {shm_threshold}')
            self._shm_threshold = shm_threshold
```

- [ ] **Step 2: Update module-level functions in registry.py**

Replace `set_shm_threshold()`, `set_server_ipc_config()`, `set_client_ipc_config()` with:

```python
def set_config(*, shm_threshold: int | None = None) -> None:
    """Set global hardware config. Call before register()/connect()."""
    _ProcessRegistry.get().set_config(shm_threshold=shm_threshold)

def set_server(**kwargs: object) -> None:
    """Configure IPC server. Call before register()."""
    _ProcessRegistry.get().set_server(**kwargs)

def set_client(**kwargs: object) -> None:
    """Configure IPC client. Call before connect()."""
    _ProcessRegistry.get().set_client(**kwargs)
```

Keep old names as thin wrappers for one release cycle (optional — spec says breaking OK):
```python
# Deprecated aliases — remove in next version
set_shm_threshold = lambda threshold: set_config(shm_threshold=threshold)
set_server_ipc_config = lambda **kw: set_server(**kw)
set_client_ipc_config = lambda **kw: set_client(**kw)
```

- [ ] **Step 3: Update `src/c_two/__init__.py`**

```python
from .transport.registry import (
    set_address,
    set_config,
    set_server,
    set_client,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)
```

- [ ] **Step 4: Update `src/c_two/transport/__init__.py`**

Update `__all__` and `_LAZY_IMPORTS`:
```python
# In __all__:
'set_config', 'set_server', 'set_client',
# (remove 'set_shm_threshold', 'set_server_ipc_config', 'set_client_ipc_config')

# In _LAZY_IMPORTS:
'set_config':  ('.registry', 'set_config'),
'set_server':  ('.registry', 'set_server'),
'set_client':  ('.registry', 'set_client'),
```

- [ ] **Step 5: Commit**

```bash
git add src/c_two/__init__.py src/c_two/transport/__init__.py src/c_two/transport/registry.py
git commit -m "feat: new config API surface (set_config/set_server/set_client)

Breaking: removes set_shm_threshold, set_server_ipc_config, set_client_ipc_config

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 9: Rebuild and verify compilation

- [ ] **Step 1: Rebuild Rust + Python extension**

Run: `uv sync --reinstall-package c-two`
Expected: builds successfully

- [ ] **Step 2: Quick smoke test**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_ipc_config.py -v`
Expected: all new config tests pass

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_encoding.py -q --timeout=30`
Expected: existing unit tests still pass (no import breakage)

**Note:** Integration tests may fail at this point due to old `IPCConfig` imports — that is fixed in Phase 4.

---

## Phase 4: Cleanup and Verification

### Task 10: Update test imports

**Files:**
- Modify: `tests/integration/test_heartbeat.py`
- Modify: `tests/integration/test_server.py`
- Modify: `tests/integration/test_concurrency_safety.py`
- Modify: `tests/integration/test_dynamic_pool.py`

- [ ] **Step 1: Update `test_heartbeat.py`**

```python
# Replace:
from c_two.transport.ipc.frame import IPCConfig
# With:
from c_two.config.ipc import ServerIPCConfig
```

All `IPCConfig(...)` constructions become `ServerIPCConfig(...)`. Field names that changed:
- No field name changes for heartbeat tests (heartbeat_interval/heartbeat_timeout stay the same in Python)

- [ ] **Step 2: Update `test_server.py`**

```python
# Replace:
from c_two.transport.ipc.frame import IPCConfig
# With:
from c_two.config.ipc import ServerIPCConfig
```

`IPCConfig(shm_threshold=16)` → `ServerIPCConfig()` — note `shm_threshold` is no longer in `ServerIPCConfig`; it's passed separately to the server. Check how the test constructs the server and adapt accordingly.

- [ ] **Step 3: Update `test_concurrency_safety.py`**

```python
# Replace:
from c_two.transport.ipc.frame import IPCConfig
# With:
from c_two.config.ipc import ServerIPCConfig

# Replace:
cc.set_server_ipc_config(segment_size=1 << 20, max_segments=2)
# With:
cc.set_server(pool_segment_size=1 << 20, max_pool_segments=2)
```

- [ ] **Step 4: Update `test_dynamic_pool.py`**

```python
# Replace:
from c_two.transport.ipc.frame import IPCConfig
# With:
from c_two.config.ipc import ServerIPCConfig
```

Update `_pool_config()` and `cfg: IPCConfig` type hints.

- [ ] **Step 5: Commit**

```bash
git add tests/
git commit -m "test: migrate integration tests to new config types

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 11: Delete old config files

**Files:**
- Delete: `src/c_two/transport/config.py`
- Modify: `src/c_two/transport/ipc/frame.py` (delete IPCConfig, keep file if other content)
- Modify: `src/c_two/transport/ipc/__init__.py`
- Modify: `src/c_two/transport/registry.py` (remove old imports)

- [ ] **Step 1: Delete `transport/config.py`**

Remove the file. All imports now come from `c_two.config.settings`.

Update any remaining imports in `registry.py`:
```python
# Remove:
from c_two.transport.config import settings
# Already replaced with:
from c_two.config.settings import settings
```

- [ ] **Step 2: Clean `transport/ipc/frame.py`**

If `IPCConfig` is the only content (it is — verified), delete the file entirely.

- [ ] **Step 3: Update `transport/ipc/__init__.py`**

```python
# Remove:
from .frame import IPCConfig
# Replace with nothing, or re-export from new location for any external users:
from c_two.config.ipc import ServerIPCConfig, ClientIPCConfig, BaseIPCConfig
```

Or if no one imports from `c_two.transport.ipc` directly, leave empty:
```python
# IPC transport subpackage.
```

- [ ] **Step 4: Commit**

```bash
git add -A src/c_two/transport/
git commit -m "refactor: delete old config files (transport/config.py, ipc/frame.py)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 12: Full test suite pass

- [ ] **Step 1: Rebuild**

Run: `uv sync --reinstall-package c-two`
Expected: builds successfully

- [ ] **Step 2: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: all 520+ tests pass

- [ ] **Step 3: Run Rust tests**

Run: `cd src/c_two/_native && cargo test -p c2-config -p c2-server -p c2-chunk --no-default-features`
Expected: all tests pass

- [ ] **Step 4: Final commit if any fixups needed**

```bash
git add -A
git commit -m "fix: test suite fixups for config migration

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Summary

| Phase | Tasks | Description |
|-------|-------|-------------|
| 1: Rust Split | 1-3 | Split `IpcConfig` → Base/Server/Client, update consumers, FFI stays compat |
| 2: Python Config | 4-5 | New `config/` package + frozen dataclasses + unit tests |
| 3: Integration | 6-9 | Server FFI + client FFI + registry + exports, all cut over atomically |
| 4: Cleanup | 10-12 | Test imports, delete old files, full suite verification |

**Key risks mitigated:**
- FFI + Python callers changed together (no broken intermediate state)
- Clamping logic preserved in `build_server_config()` (not lost by frozen)
- Client `max_segments` bug fixed by design (all fields passed through)
- Heartbeat disable (interval=0) preserved
- Client reassembly 64 MB default kept distinct from Server 256 MB
