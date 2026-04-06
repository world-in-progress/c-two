# Config Architecture Design

**Date:** 2026-04-05
**Status:** Draft
**Scope:** IPC config unification — Python single source of truth, Rust mirror, FFI full-field pass-through

## Breaking Changes (0.x.x)

Since the project is pre-1.0, these are documented breaking changes with no backward-compat shims.

### Environment Variables

| Before | After | Reason |
|--------|-------|--------|
| `C2_IPC_SEGMENT_SIZE` | `C2_IPC_POOL_SEGMENT_SIZE` | Disambiguate pool vs reassembly segment size |
| `C2_IPC_MAX_SEGMENTS` | `C2_IPC_MAX_POOL_SEGMENTS` | Disambiguate pool vs reassembly segment count |

### Python API

| Before | After | Reason |
|--------|-------|--------|
| `cc.set_server_ipc_config(segment_size=...)` | `cc.set_server(pool_segment_size=...)` | Shorter name; kwargs use field names directly |
| `cc.set_client_ipc_config(segment_size=...)` | `cc.set_client(pool_segment_size=...)` | Shorter name; kwargs use field names directly |
| `cc.set_shm_threshold(value)` | `cc.set_config(shm_threshold=value)` | Unified global config entry point |

Parameter names also change: `segment_size` → `pool_segment_size`, `max_segments` → `max_pool_segments` (matching field names directly, no more remapping).

## Problem

Configuration is scattered and drifting:

| Issue | Detail |
|-------|--------|
| **Duplicate defaults** | Python `IPCConfig` (in `transport/ipc/frame.py`) and Rust `IpcConfig` (in `c2-config/src/ipc.rs`) each define their own defaults; 6+ fields disagree |
| **Partial FFI pass-through** | Python passes ~9 of 18 fields; Rust fills the rest with `..IpcConfig::default()`, causing silent config drift |
| **Wrong module location** | Python `IPCConfig` lives in `transport/ipc/frame.py` — a transport-internal module, not discoverable as config |
| **No .env support for IPC fields** | Only `C2_IPC_ADDRESS`, `C2_RELAY_ADDRESS`, `C2_ENV_FILE` are env-configurable; pool sizes, heartbeat, chunk params are not |
| **Mutable config** | Python `IPCConfig` is a regular dataclass — can be mutated after construction, risking runtime inconsistency |
| **Type inconsistency** | Some Rust fields are `u64`, others `usize`; Python uses `int` for everything |
| **No server/client split** | One monolithic config struct serves both roles despite different field sets |

### Measured Drift (Current State)

| Field | Python default | Rust default | FFI default | Ratio |
|-------|---------------|-------------|-------------|-------|
| `reassembly_segment_size` | 256 MB | 64 MB | (not passed) | 4× |
| `max_frame_size` | 2 GB | (not in Rust) | 256 MB | 8× |
| `max_payload_size` | 16 GB | (not in Rust) | 128 MB | 128× |
| `heartbeat_interval` | 15.0 s | 10.0 s | 10.0 s | 1.5× |
| `chunk_gc_interval` | 100 (int!) | 5.0 (f64) | (not passed) | type + 20× |

## Design Principles

1. **Python is the single source of truth** — all authoritative defaults live in Python
2. **Frozen after construction** — config objects are immutable once built
3. **Full FFI pass-through** — every field crosses the bridge; no `..default()` in production Rust
4. **Priority chain** — `kwargs > .env > code defaults`
5. **Separation of concerns** — global hardware config vs per-role IPC config

## Python Module Organization

### New Structure

```
src/c_two/config/
├── __init__.py          # Exports: C2Settings, ServerIPCConfig, ClientIPCConfig, BaseIPCConfig
├── settings.py          # C2Settings (pydantic-settings, env vars, global params)
└── ipc.py               # BaseIPCConfig, ServerIPCConfig, ClientIPCConfig (frozen dataclasses)
```

### Migration

| Before | After | Action |
|--------|-------|--------|
| `transport/config.py` → `C2Settings` | `config/settings.py` → `C2Settings` | Move + extend |
| `transport/ipc/frame.py` → `IPCConfig` | `config/ipc.py` → `ServerIPCConfig` / `ClientIPCConfig` | Replace |
| `transport/ipc/frame.py` → `DEFAULT_SHM_THRESHOLD` etc. | `config/ipc.py` (as class defaults) | Absorb |
| `transport/config.py` | Delete (or empty re-export shim) | Remove |
| `transport/ipc/frame.py` `IPCConfig` class | Delete | Remove |

`transport/ipc/frame.py` may retain non-config content (if any); `IPCConfig` is the only thing removed.

## Python Config Classes

### C2Settings (Global)

```python
# config/settings.py
from pydantic_settings import BaseSettings

class C2Settings(BaseSettings):
    """Global settings loaded from env vars / .env file."""
    model_config = SettingsConfigDict(env_prefix='C2_', env_file='.env')

    # Transport addresses
    ipc_address: str | None = None          # C2_IPC_ADDRESS
    relay_address: str | None = None        # C2_RELAY_ADDRESS
    env_file: str = '.env'                  # C2_ENV_FILE

    # Hardware-related global param
    shm_threshold: int = 4_096              # C2_SHM_THRESHOLD

    # IPC field overrides (all optional — None means "use class default")
    ipc_pool_segment_size: int | None = None         # C2_IPC_POOL_SEGMENT_SIZE
    ipc_max_pool_segments: int | None = None         # C2_IPC_MAX_POOL_SEGMENTS
    ipc_max_pool_memory: int | None = None           # C2_IPC_MAX_POOL_MEMORY
    ipc_heartbeat_interval: float | None = None      # C2_IPC_HEARTBEAT_INTERVAL
    ipc_heartbeat_timeout: float | None = None       # C2_IPC_HEARTBEAT_TIMEOUT
    ipc_max_frame_size: int | None = None            # C2_IPC_MAX_FRAME_SIZE
    ipc_max_payload_size: int | None = None          # C2_IPC_MAX_PAYLOAD_SIZE
    # All remaining BaseIPCConfig + ServerIPCConfig fields follow the same pattern:
    # ipc_{field_name}: type | None = None
    # Full list: see Appendix "Full Field Inventory"
```

`shm_threshold` is a first-class field (not `ipc_`-prefixed) because it is a hardware property independent of IPC role.

### BaseIPCConfig (Shared)

```python
# config/ipc.py
from dataclasses import dataclass

@dataclass(frozen=True)
class BaseIPCConfig:
    """Configuration shared by both server and client IPC transport."""

    # Pool settings
    pool_enabled: bool = True
    pool_segment_size: int = 268_435_456           # 256 MB
    max_pool_segments: int = 4
    max_pool_memory: int = 1_073_741_824           # 1 GB

    # Reassembly pool settings
    reassembly_segment_size: int = 268_435_456     # 256 MB
    reassembly_max_segments: int = 4

    # Chunk settings
    max_total_chunks: int = 512
    chunk_gc_interval: float = 5.0                 # seconds
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0          # seconds
    max_reassembly_bytes: int = 8_589_934_592      # 8 GB
    chunk_size: int = 131_072                      # 128 KB

    def __post_init__(self):
        if self.pool_segment_size <= 0:
            raise ValueError("pool_segment_size must be positive")
        if not (1 <= self.max_pool_segments <= 255):
            raise ValueError("max_pool_segments must be 1..255")
        if not (0.0 < self.chunk_threshold_ratio <= 1.0):
            raise ValueError("chunk_threshold_ratio must be in (0, 1]")
        if self.chunk_gc_interval <= 0:
            raise ValueError("chunk_gc_interval must be positive")
        if self.chunk_assembler_timeout <= 0:
            raise ValueError("chunk_assembler_timeout must be positive")
```

> **Note on clamping:** The current `IPCConfig.__post_init__` mutates
> `pool_segment_size` to clamp it to `max_payload_size`. With `frozen=True`,
> mutation is impossible. Clamping logic moves to `build_server_config()` /
> `build_client_config()` — applied **before** constructing the frozen
> dataclass. See "Config Priority Chain → Implementation" below.

### ServerIPCConfig

```python
@dataclass(frozen=True)
class ServerIPCConfig(BaseIPCConfig):
    """Server-side IPC configuration. Extends BaseIPCConfig with server-specific fields."""

    max_frame_size: int = 2_147_483_648            # 2 GB
    max_payload_size: int = 17_179_869_184         # 16 GB
    max_pending_requests: int = 1024
    pool_decay_seconds: float = 60.0
    heartbeat_interval: float = 15.0               # seconds
    heartbeat_timeout: float = 30.0                # seconds

    def __post_init__(self):
        super().__post_init__()
        if self.max_frame_size <= 0:
            raise ValueError("max_frame_size must be positive")
        if self.max_payload_size <= 0:
            raise ValueError("max_payload_size must be positive")
        if self.heartbeat_interval < 0:
            raise ValueError("heartbeat_interval must be >= 0 (0 disables heartbeat)")
        if self.heartbeat_interval > 0 and self.heartbeat_timeout <= self.heartbeat_interval:
            raise ValueError("heartbeat_timeout must exceed heartbeat_interval when heartbeat is enabled")
```

### ClientIPCConfig

```python
@dataclass(frozen=True)
class ClientIPCConfig(BaseIPCConfig):
    """Client-side IPC configuration. Overrides reassembly defaults for lower memory footprint."""
    reassembly_segment_size: int = 67_108_864      # 64 MB (vs server 256 MB)
    reassembly_max_segments: int = 4
```

Client uses a smaller `reassembly_segment_size` default (64 MB vs server's 256 MB) because a single process may connect to many servers — 10 connections × 256 MB × 4 segments = 10 GB would be excessive. At 64 MB × 4 = 256 MB per connection, 10 connections = 2.5 GB, which is reasonable.

## Config Priority Chain

### Resolution Order

```
kwargs (cc.set_server / cc.set_client / cc.set_config)
  ↓ (override)
.env / environment variables (via C2Settings)
  ↓ (override)
Python class defaults (frozen dataclass field defaults)
```

### Implementation

```python
# config/ipc.py

def build_server_config(settings: C2Settings, **kwargs) -> ServerIPCConfig:
    """Build a frozen ServerIPCConfig with priority: kwargs > env > defaults."""
    merged = {}
    for field in fields(ServerIPCConfig):
        name = field.name
        env_name = f'ipc_{name}'
        if name in kwargs:
            merged[name] = kwargs[name]
        elif hasattr(settings, env_name) and getattr(settings, env_name) is not None:
            merged[name] = getattr(settings, env_name)
        # else: omit — dataclass default applies

    # Clamping (formerly in IPCConfig.__post_init__ mutation):
    # pool_segment_size must not exceed max_payload_size
    seg = merged.get('pool_segment_size', ServerIPCConfig.pool_segment_size)
    pay = merged.get('max_payload_size', ServerIPCConfig.max_payload_size)
    if seg > pay:
        merged['pool_segment_size'] = pay

    return ServerIPCConfig(**merged)


def build_client_config(settings: C2Settings, **kwargs) -> ClientIPCConfig:
    """Build a frozen ClientIPCConfig with priority: kwargs > env > defaults."""
    merged = {}
    for field in fields(ClientIPCConfig):
        name = field.name
        env_name = f'ipc_{name}'
        if name in kwargs:
            merged[name] = kwargs[name]
        elif hasattr(settings, env_name) and getattr(settings, env_name) is not None:
            merged[name] = getattr(settings, env_name)
    return ClientIPCConfig(**merged)
```

Unknown kwargs raise `TypeError` immediately (standard Python behavior for frozen dataclass).

## User-Facing API

### API Surface

```python
import c_two as cc

# Global hardware config
cc.set_config(shm_threshold=8192)
# set_config() accepts ONLY global fields (currently: shm_threshold).
# IPC-specific fields go through set_server() / set_client().

# Server-side IPC config (call BEFORE cc.register)
cc.set_server(
    pool_segment_size=512 * 1024 * 1024,
    heartbeat_interval=10.0,
    max_payload_size=32 * 1024 * 1024 * 1024,
)

# Client-side IPC config (call BEFORE cc.connect)
cc.set_client(
    chunk_size=256 * 1024,
)

# Normal usage
cc.register(IGrid, grid, name='grid')         # uses frozen ServerIPCConfig
proxy = cc.connect(IGrid, name='grid')         # uses frozen ClientIPCConfig
```

### Runtime Protection

```python
cc.register(IGrid, grid, name='grid')          # server started

cc.set_server(pool_segment_size=1024)
# ⚠️ WARNING: "Server already started, set_server() ignored.
#              Call set_server() before cc.register()."

cc.set_config(shm_threshold=2048)
# ⚠️ WARNING: "Active connections exist, set_config() ignored.
#              Call set_config() before cc.register() / cc.connect()."
```

- `warnings.warn(msg, UserWarning, stacklevel=2)` — not an exception
- Config call is silently ignored (no partial application)
- Rationale: In interactive/notebook environments, hard errors on config timing break workflow

### Internal Flow

```
cc.set_server(**kwargs)
  → build_server_config(C2Settings(), **kwargs)
  → ServerIPCConfig (frozen)
  → stored in registry._server_config

cc.register(icrm, crm, name=...)
  → reads registry._server_config + registry._global_config.shm_threshold
  → passes ALL fields to NativeServerBridge → RustServer::new()
  → marks registry._server_started = True

cc.set_client(**kwargs)
  → build_client_config(C2Settings(), **kwargs)
  → ClientIPCConfig (frozen)
  → stored in registry._client_config

cc.connect(icrm, name=..., address=...)
  → reads registry._client_config + registry._global_config.shm_threshold
  → passes ALL fields to RustClient::new() / RustHttpClient::new()
```

## Rust Config Structs

### Crate: `c2-config`

The existing `c2-config` crate already holds `IpcConfig`, `PoolConfig`, and `RelayConfig`. This design replaces the monolithic `IpcConfig` with three structs:

```rust
// c2-config/src/ipc.rs

/// Shared IPC transport configuration (both server and client)
#[derive(Debug, Clone)]
pub struct BaseIpcConfig {
    pub pool_enabled: bool,
    pub pool_segment_size: u64,
    pub max_pool_segments: u32,
    pub max_pool_memory: u64,
    pub reassembly_segment_size: u64,
    pub reassembly_max_segments: u32,
    pub max_total_chunks: u32,
    pub chunk_gc_interval_secs: f64,
    pub chunk_threshold_ratio: f64,
    pub chunk_assembler_timeout_secs: f64,
    pub max_reassembly_bytes: u64,
    pub chunk_size: u64,
}

/// Server-side IPC configuration
#[derive(Debug, Clone)]
pub struct ServerIpcConfig {
    pub base: BaseIpcConfig,
    pub max_frame_size: u64,
    pub max_payload_size: u64,
    pub max_pending_requests: u32,
    pub pool_decay_seconds: f64,
    pub heartbeat_interval_secs: f64,
    pub heartbeat_timeout_secs: f64,
    pub shm_threshold: u64,
}

impl std::ops::Deref for ServerIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}

/// Client-side IPC configuration
#[derive(Debug, Clone)]
pub struct ClientIpcConfig {
    pub base: BaseIpcConfig,
    pub shm_threshold: u64,
}

impl std::ops::Deref for ClientIpcConfig {
    type Target = BaseIpcConfig;
    fn deref(&self) -> &BaseIpcConfig { &self.base }
}
```

### Design Decisions

- **`shm_threshold` in both Server/Client**: Python stores it globally in `C2Settings`, but Rust receives it per-config (no global Rust state)
- **`Deref` to `BaseIpcConfig`**: Allows `config.pool_segment_size` without `.base.` — ergonomic for downstream crates
- **`Default` impl**: Retained for Rust-only testing. Values match Python defaults but are NOT authoritative in production
- **Types**: `u64` for all sizes, `f64` for all durations, `u32` for all counts — no more `usize` ambiguity
- **Client `reassembly_segment_size`**: Python `ClientIPCConfig` overrides to 64 MB; Rust `Default` for `ClientIpcConfig` should also use 64 MB. The `BaseIpcConfig::Default` uses 256 MB (server-oriented), but `ClientIpcConfig::Default` overrides `base.reassembly_segment_size` to 64 MB

### Downstream Crate Usage

| Crate | Uses | Notes |
|-------|------|-------|
| `c2-server` | `ServerIpcConfig` | Pool setup, heartbeat, frame limits |
| `c2-ipc` | `ClientIpcConfig` | Pool setup, chunk config |
| `c2-chunk` | `BaseIpcConfig` (via `ChunkConfig::from_base()`) | Rename from current `from_ipc()` |
| `c2-mem` | `PoolConfig` (unchanged) | Not affected by IPC split |
| `c2-relay` | `RelayConfig` (unchanged) | Not affected |

## FFI Bridge

### Core Change: No Default Values in FFI

**Before (current — problematic):**
```rust
#[pyo3(signature = (
    callback, path, shm_prefix,
    pool_segment_size = 268_435_456,    // hardcoded default — drifts from Python
    max_pool_segments = 4,
    // ... 9 more with hardcoded defaults ...
    // ... 9 fields NOT passed at all — always use Rust default ...
))]
fn new(/* ... */) -> PyResult<Self> {
    let config = IpcConfig {
        pool_segment_size,
        // ...
        ..IpcConfig::default()   // ← fills unpassed fields with Rust defaults
    };
}
```

**After (this design):**
```rust
#[pyo3(signature = (
    callback, path, shm_prefix,
    // ALL fields mandatory — Python has already resolved defaults
    shm_threshold,
    pool_enabled,
    pool_segment_size,
    max_pool_segments,
    max_pool_memory,
    reassembly_segment_size,
    reassembly_max_segments,
    max_frame_size,
    max_payload_size,
    max_pending_requests,
    pool_decay_seconds,
    heartbeat_interval,
    heartbeat_timeout,
    max_total_chunks,
    chunk_gc_interval,
    chunk_threshold_ratio,
    chunk_assembler_timeout,
    max_reassembly_bytes,
    chunk_size,
))]
fn new(
    callback: PyObject,
    path: String,
    shm_prefix: String,
    shm_threshold: u64,
    pool_enabled: bool,
    pool_segment_size: u64,
    max_pool_segments: u32,
    max_pool_memory: u64,
    reassembly_segment_size: u64,
    reassembly_max_segments: u32,
    max_frame_size: u64,
    max_payload_size: u64,
    max_pending_requests: u32,
    pool_decay_seconds: f64,
    heartbeat_interval: f64,
    heartbeat_timeout: f64,
    max_total_chunks: u32,
    chunk_gc_interval: f64,
    chunk_threshold_ratio: f64,
    chunk_assembler_timeout: f64,
    max_reassembly_bytes: u64,
    chunk_size: u64,
) -> PyResult<Self> {
    let config = ServerIpcConfig {
        base: BaseIpcConfig {
            pool_enabled,
            pool_segment_size,
            max_pool_segments,
            max_pool_memory,
            reassembly_segment_size,
            reassembly_max_segments,
            max_total_chunks,
            chunk_gc_interval_secs: chunk_gc_interval,
            chunk_threshold_ratio,
            chunk_assembler_timeout_secs: chunk_assembler_timeout,
            max_reassembly_bytes,
            chunk_size,
        },
        max_frame_size,
        max_payload_size,
        max_pending_requests,
        pool_decay_seconds,
        heartbeat_interval_secs: heartbeat_interval,
        heartbeat_timeout_secs: heartbeat_timeout,
        shm_threshold,
    };
    // ...
}
```

### Python-Side Caller

```python
# transport/server/native.py — NativeServerBridge.__init__()
def __init__(self, ...):
    cfg = registry._server_config    # frozen ServerIPCConfig
    shm = registry._global_config.shm_threshold

    self._server = RustServer(
        callback=self._callback,
        path=str(self._path),
        shm_prefix=self._shm_prefix,
        shm_threshold=shm,
        pool_enabled=cfg.pool_enabled,
        pool_segment_size=cfg.pool_segment_size,
        max_pool_segments=cfg.max_pool_segments,
        max_pool_memory=cfg.max_pool_memory,
        reassembly_segment_size=cfg.reassembly_segment_size,
        reassembly_max_segments=cfg.reassembly_max_segments,
        max_frame_size=cfg.max_frame_size,
        max_payload_size=cfg.max_payload_size,
        max_pending_requests=cfg.max_pending_requests,
        pool_decay_seconds=cfg.pool_decay_seconds,
        heartbeat_interval=cfg.heartbeat_interval,
        heartbeat_timeout=cfg.heartbeat_timeout,
        max_total_chunks=cfg.max_total_chunks,
        chunk_gc_interval=cfg.chunk_gc_interval,
        chunk_threshold_ratio=cfg.chunk_threshold_ratio,
        chunk_assembler_timeout=cfg.chunk_assembler_timeout,
        max_reassembly_bytes=cfg.max_reassembly_bytes,
        chunk_size=cfg.chunk_size,
    )
```

**Key invariant:** Every field in `ServerIpcConfig` / `ClientIpcConfig` has a 1:1 mapping in the FFI signature. No field is omitted, no default is assumed.

### RustClientPool FFI

The current `RustClientPool.acquire()` and `set_default_config()` in `client_ffi.rs` also have hardcoded defaults and `..IpcConfig::default()` fallback. These must be converted to full-field pass-through.

**Before (current — problematic):**
```rust
// set_default_config() — hardcoded defaults drift from Python
#[pyo3(signature = (shm_threshold=4096, chunk_size=131072, pool_segment_size=None, ...))]
fn set_default_config(&self, ...) {
    let defaults = IpcConfig::default();
    self.inner.set_default_config(IpcConfig {
        shm_threshold,
        chunk_size,
        pool_segment_size: pool_segment_size.unwrap_or(defaults.pool_segment_size),
        ..defaults    // ← fills unpassed fields with Rust defaults
    });
}

// acquire() — optional overrides with IpcConfig::default() fallback
#[pyo3(signature = (address, shm_threshold=None, chunk_size=None, ...))]
fn acquire(&self, ...) -> PyResult<PyRustClient> {
    let defaults = IpcConfig::default();
    let config = Some(IpcConfig { ..defaults });  // ← Rust defaults, not Python
    ...
}
```

**After (this design):**

`RustClientPool.set_default_config()` receives a frozen `ClientIPCConfig` worth of fields — all mandatory:

```rust
#[pyo3(signature = (
    shm_threshold,
    pool_enabled,
    pool_segment_size,
    max_pool_segments,
    max_pool_memory,
    reassembly_segment_size,
    reassembly_max_segments,
    max_total_chunks,
    chunk_gc_interval,
    chunk_threshold_ratio,
    chunk_assembler_timeout,
    max_reassembly_bytes,
    chunk_size,
))]
fn set_default_config(
    &self,
    shm_threshold: u64,
    pool_enabled: bool,
    pool_segment_size: u64,
    max_pool_segments: u32,
    max_pool_memory: u64,
    reassembly_segment_size: u64,
    reassembly_max_segments: u32,
    max_total_chunks: u32,
    chunk_gc_interval: f64,
    chunk_threshold_ratio: f64,
    chunk_assembler_timeout: f64,
    max_reassembly_bytes: u64,
    chunk_size: u64,
) {
    let config = ClientIpcConfig {
        base: BaseIpcConfig { /* all fields from params */ },
        shm_threshold,
    };
    self.inner.set_default_config(config);
}
```

`RustClientPool.acquire()` no longer accepts config overrides — it uses the pool's default config set via `set_default_config()`. Per-connection config overrides are eliminated (they were a source of inconsistency):

```rust
#[pyo3(signature = (address,))]
fn acquire(&self, py: Python<'_>, address: &str) -> PyResult<PyRustClient> {
    // Uses the default config set by set_default_config()
    let client = py.allow_threads(move || self.inner.acquire(address, None))
        .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
    Ok(PyRustClient { inner: client })
}
```

**Python-side caller:**
```python
# registry.py — during init or set_client()
pool = RustClientPool(max_connections=...)
cfg = registry._client_config    # frozen ClientIPCConfig
shm = registry._global_config.shm_threshold
pool.set_default_config(
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
```

### RustClient::new() FFI

`RustClient::new()` currently has the same hardcoded defaults problem (`shm_threshold=4096`, `chunk_size=131072`, `..IpcConfig::default()`). It is converted to all-mandatory, matching the `ClientIpcConfig` field set:

```rust
#[pyo3(signature = (
    address,
    shm_threshold,
    pool_enabled,
    pool_segment_size,
    max_pool_segments,
    max_pool_memory,
    reassembly_segment_size,
    reassembly_max_segments,
    max_total_chunks,
    chunk_gc_interval,
    chunk_threshold_ratio,
    chunk_assembler_timeout,
    max_reassembly_bytes,
    chunk_size,
))]
fn new(
    py: Python<'_>,
    address: &str,
    shm_threshold: u64,
    pool_enabled: bool,
    pool_segment_size: u64,
    max_pool_segments: u32,
    max_pool_memory: u64,
    reassembly_segment_size: u64,
    reassembly_max_segments: u32,
    max_total_chunks: u32,
    chunk_gc_interval: f64,
    chunk_threshold_ratio: f64,
    chunk_assembler_timeout: f64,
    max_reassembly_bytes: u64,
    chunk_size: u64,
) -> PyResult<Self> {
    let config = ClientIpcConfig {
        base: BaseIpcConfig { /* all fields from params */ },
        shm_threshold,
    };
    // ...
}
```

`RustClient::new()` is still needed for standalone (non-pooled) connections. Both `RustClient::new()` and `RustClientPool.set_default_config()` receive the same `ClientIpcConfig` field set.

### In Scope

- Python `config/` package: `settings.py`, `ipc.py`, `__init__.py`
- Delete `transport/config.py` and `IPCConfig` from `transport/ipc/frame.py`
- Update all Python import sites
- Rust `c2-config/src/ipc.rs`: replace `IpcConfig` with `BaseIpcConfig` + `ServerIpcConfig` + `ClientIpcConfig`
- Update all Rust crate consumers (`c2-server`, `c2-ipc`, `c2-chunk`, `c2-ffi`)
- FFI: full-field mandatory signatures for `RustServer::new()`, `RustClient::new()`, `RustClientPool.set_default_config()`, and simplified `RustClientPool.acquire()`
- `cc.set_server()`, `cc.set_client()`, `cc.set_config()` API on `cc` namespace
- `.env` support for all IPC config fields via `C2Settings`
- Runtime protection (warning on post-start config changes)

### Out of Scope

- Relay config refactoring (stays as-is)
- Transport logic changes (wire protocol, SHM, chunk behavior unchanged)
- Serialization changes
- New CLI commands
- Performance optimization

## Testing Strategy

### Python Unit Tests

1. **Config construction**: `BaseIPCConfig()`, `ServerIPCConfig()`, `ClientIPCConfig()` with defaults
2. **Frozen enforcement**: Attempt attribute mutation → `FrozenInstanceError`
3. **Validation**: Invalid values → `ValueError` (negative sizes, out-of-range ratios)
4. **Clamping**: `build_server_config(pool_segment_size > max_payload_size)` → clamped to `max_payload_size`
5. **Priority chain**: kwargs override env, env override defaults
6. **Runtime protection**: `cc.set_server()` after `cc.register()` → `UserWarning` emitted, config unchanged
7. **Build functions**: `build_server_config()` / `build_client_config()` merge correctly
8. **Client defaults**: `ClientIPCConfig().reassembly_segment_size == 64 MB` (differs from Base's 256 MB)

### Rust Unit Tests

1. **Struct construction**: `ServerIpcConfig` / `ClientIpcConfig` with explicit values
2. **Deref**: `server_config.pool_segment_size` resolves to `base.pool_segment_size`
3. **Default**: `Default` impl values match Python defaults (cross-check)

### Integration Tests

1. **All existing 520+ pytest tests pass** — no regression
2. **Benchmark**: No performance regression (same benchmark suite)

## Appendix: Full Field Inventory

### BaseIPCConfig Fields

| Field | Type (Python) | Type (Rust) | Default | Env Var |
|-------|--------------|-------------|---------|---------|
| `pool_enabled` | `bool` | `bool` | `True` | `C2_IPC_POOL_ENABLED` |
| `pool_segment_size` | `int` | `u64` | 268,435,456 (256 MB) | `C2_IPC_POOL_SEGMENT_SIZE` |
| `max_pool_segments` | `int` | `u32` | 4 | `C2_IPC_MAX_POOL_SEGMENTS` |
| `max_pool_memory` | `int` | `u64` | 1,073,741,824 (1 GB) | `C2_IPC_MAX_POOL_MEMORY` |
| `reassembly_segment_size` | `int` | `u64` | 268,435,456 (256 MB) | `C2_IPC_REASSEMBLY_SEGMENT_SIZE` |
| `reassembly_max_segments` | `int` | `u32` | 4 | `C2_IPC_REASSEMBLY_MAX_SEGMENTS` |
| `max_total_chunks` | `int` | `u32` | 512 | `C2_IPC_MAX_TOTAL_CHUNKS` |
| `chunk_gc_interval` | `float` | `f64` | 5.0 | `C2_IPC_CHUNK_GC_INTERVAL` |
| `chunk_threshold_ratio` | `float` | `f64` | 0.9 | `C2_IPC_CHUNK_THRESHOLD_RATIO` |
| `chunk_assembler_timeout` | `float` | `f64` | 60.0 | `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` |
| `max_reassembly_bytes` | `int` | `u64` | 8,589,934,592 (8 GB) | `C2_IPC_MAX_REASSEMBLY_BYTES` |
| `chunk_size` | `int` | `u64` | 131,072 (128 KB) | `C2_IPC_CHUNK_SIZE` |

### ServerIPCConfig Additional Fields

| Field | Type (Python) | Type (Rust) | Default | Env Var |
|-------|--------------|-------------|---------|---------|
| `max_frame_size` | `int` | `u64` | 2,147,483,648 (2 GB) | `C2_IPC_MAX_FRAME_SIZE` |
| `max_payload_size` | `int` | `u64` | 17,179,869,184 (16 GB) | `C2_IPC_MAX_PAYLOAD_SIZE` |
| `max_pending_requests` | `int` | `u32` | 1024 | `C2_IPC_MAX_PENDING_REQUESTS` |
| `pool_decay_seconds` | `float` | `f64` | 60.0 | `C2_IPC_POOL_DECAY_SECONDS` |
| `heartbeat_interval` | `float` | `f64` | 15.0 (0 = disabled) | `C2_IPC_HEARTBEAT_INTERVAL` |
| `heartbeat_timeout` | `float` | `f64` | 30.0 | `C2_IPC_HEARTBEAT_TIMEOUT` |

### ClientIPCConfig Overrides

`ClientIPCConfig` inherits all `BaseIPCConfig` fields but overrides these defaults:

| Field | Base Default | Client Default | Reason |
|-------|-------------|---------------|--------|
| `reassembly_segment_size` | 268,435,456 (256 MB) | 67,108,864 (64 MB) | Client may connect to many servers; 10 × 256 MB × 4 = 10 GB is excessive |

All other fields use the `BaseIPCConfig` defaults.

### Global Settings (C2Settings)

| Field | Type | Default | Env Var |
|-------|------|---------|---------|
| `ipc_address` | `str \| None` | `None` | `C2_IPC_ADDRESS` |
| `relay_address` | `str \| None` | `None` | `C2_RELAY_ADDRESS` |
| `shm_threshold` | `int` | 4,096 | `C2_SHM_THRESHOLD` |
| `env_file` | `str` | `.env` | `C2_ENV_FILE` |
