# IPC Config Split: Server/Client Separation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single `cc.set_ipc_config()` with `cc.set_server_ipc_config()`, `cc.set_client_ipc_config()`, and `cc.set_shm_threshold()` — making reassembly pool sizes configurable on both sides.

**Architecture:** Three-layer change: (1) Rust config structs + FFI gain reassembly fields, (2) Python IPCConfig dataclass + native.py pass them through, (3) Python registry.py exposes new API and removes old one. No backward compatibility shim — clean break.

**Tech Stack:** Rust (c2-ipc, c2-server, c2-ffi), Python (registry.py, frame.py, native.py, __init__.py)

---

## Current State

```
cc.set_ipc_config(segment_size, max_segments)
  ├── Server: IPCConfig → RustServer → response_pool (configurable), reassembly_pool (hardcoded 64MB×4)
  └── Client: RustClientPool.set_default_config() → request_pool (configurable), reassembly_pool (hardcoded 256MB×4)
```

## Target State

```
cc.set_shm_threshold(threshold)          # global, hardware-dependent
cc.set_server_ipc_config(                # server response + reassembly pools
    segment_size, max_segments,
    reassembly_segment_size, reassembly_max_segments)
cc.set_client_ipc_config(                # client request + reassembly pools
    segment_size, max_segments,
    reassembly_segment_size, reassembly_max_segments)
```

## Files Overview

| File | Action | Purpose |
|------|--------|---------|
| `src/c_two/_native/c2-ipc/src/client.rs` | Modify | Add reassembly fields to client `IpcConfig`, use in `default_reassembly_pool` |
| `src/c_two/_native/c2-ipc/src/pool.rs` | Modify | Pass config to reassembly pool creation in `ClientPool::acquire` |
| `src/c_two/_native/c2-server/src/config.rs` | Modify | Add reassembly fields to server `IpcConfig` |
| `src/c_two/_native/c2-server/src/server.rs` | Modify | Use config fields for reassembly pool |
| `src/c_two/_native/c2-ffi/src/client_ffi.rs` | Modify | Add reassembly params to `set_default_config` and `acquire` |
| `src/c_two/_native/c2-ffi/src/server_ffi.rs` | Modify | Add reassembly params to `RustServer::new` |
| `src/c_two/transport/ipc/frame.py` | Modify | Add reassembly fields to Python `IPCConfig` |
| `src/c_two/transport/server/native.py` | Modify | Pass reassembly config to `RustServer()` |
| `src/c_two/transport/registry.py` | Modify | Replace `set_ipc_config` with 3 new functions |
| `src/c_two/__init__.py` | Modify | Update exports |
| `src/c_two/transport/__init__.py` | Modify | Update lazy imports |
| `tests/integration/test_concurrency_safety.py` | Modify | Update `set_ipc_config` call |
| `benchmarks/segment_size_benchmark.py` | Modify | Update `set_ipc_config` calls |
| `benchmarks/three_mode_benchmark.py` | Modify | Update `set_ipc_config` calls |
| `benchmarks/thread_vs_ipc_benchmark.py` | Modify | Update `set_ipc_config` calls |

---

### Task 1: Rust layer — extend IpcConfig structs and FFI

**Files:**
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:34-54` (client IpcConfig struct + default_reassembly_pool)
- Modify: `src/c_two/_native/c2-ipc/src/pool.rs:55-107` (ClientPool acquire + set_default_config)
- Modify: `src/c_two/_native/c2-server/src/config.rs:6-62` (server IpcConfig struct + Default)
- Modify: `src/c_two/_native/c2-server/src/server.rs:99-139` (Server::new reassembly pool)
- Modify: `src/c_two/_native/c2-ffi/src/client_ffi.rs:444-485` (PyRustClientPool acquire + set_default_config)
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs:112-154` (RustServer::new params)

This task adds `reassembly_segment_size` and `reassembly_max_segments` to both Rust IpcConfig structs and threads them through FFI. All new params have defaults matching current hardcoded values (backward safe at Rust level).

- [ ] **Step 1: Extend client-side `IpcConfig` in c2-ipc**

In `src/c_two/_native/c2-ipc/src/client.rs`, add two new fields:

```rust
#[derive(Debug, Clone)]
pub struct IpcConfig {
    pub shm_threshold: usize,
    pub chunk_size: usize,
    pub pool_segment_size: Option<usize>,
    /// Reassembly pool segment size (default 256 MB).
    pub reassembly_segment_size: Option<usize>,
    /// Reassembly pool max segments (default 4).
    pub reassembly_max_segments: Option<usize>,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            shm_threshold: 4096,
            chunk_size: 131072,
            pool_segment_size: None,
            reassembly_segment_size: None,
            reassembly_max_segments: None,
        }
    }
}
```

- [ ] **Step 2: Make `default_reassembly_pool` config-aware**

In `src/c_two/_native/c2-ipc/src/client.rs`, replace `default_reassembly_pool()` with a config-accepting version:

```rust
fn make_reassembly_pool(config: &IpcConfig) -> Arc<StdMutex<MemPool>> {
    let seg_size = config.reassembly_segment_size.unwrap_or(256 * 1024 * 1024);
    let max_segs = config.reassembly_max_segments.unwrap_or(4);
    Arc::new(StdMutex::new(MemPool::new(PoolConfig {
        segment_size: seg_size,
        max_segments: max_segs,
        ..PoolConfig::default()
    })))
}
```

Update `IpcClient::new()` (line 270-290) to use `Self::make_reassembly_pool(&IpcConfig::default())` instead of `Self::default_reassembly_pool()`.

Update `IpcClient::with_pool()` (line 296-320) to use `Self::make_reassembly_pool(&config)` instead of `Self::default_reassembly_pool()`.

- [ ] **Step 3: Extend server-side `IpcConfig` in c2-server**

In `src/c_two/_native/c2-server/src/config.rs`, add two new fields to the struct:

```rust
pub struct IpcConfig {
    // ... existing fields ...

    // Reassembly pool settings (for chunked request reassembly)
    pub reassembly_segment_size: u64,
    pub reassembly_max_segments: u32,

    // ... heartbeat fields ...
}
```

And add defaults in `impl Default`:

```rust
// Reassembly pool
reassembly_segment_size: 64 * 1024 * 1024,  // 64 MB
reassembly_max_segments: 4,
```

- [ ] **Step 4: Use config for server reassembly pool**

In `src/c_two/_native/c2-server/src/server.rs`, replace the hardcoded reassembly config (lines 103-111):

```rust
let reassembly_cfg = PoolConfig {
    segment_size: config.reassembly_segment_size as usize,
    min_block_size: 4096,
    max_segments: config.reassembly_max_segments as usize,
    max_dedicated_segments: 4,
    dedicated_crash_timeout_secs: 5.0,
    spill_threshold: 0.8,
    spill_dir: PathBuf::from("/tmp/c_two_reassembly"),
};
```

- [ ] **Step 5: Update client FFI**

In `src/c_two/_native/c2-ffi/src/client_ffi.rs`, update `set_default_config` (lines 477-485):

```rust
#[pyo3(signature = (shm_threshold=4096, chunk_size=131072, pool_segment_size=None, reassembly_segment_size=None, reassembly_max_segments=None))]
fn set_default_config(
    &self,
    shm_threshold: usize,
    chunk_size: usize,
    pool_segment_size: Option<usize>,
    reassembly_segment_size: Option<usize>,
    reassembly_max_segments: Option<usize>,
) {
    self.inner.set_default_config(IpcConfig {
        shm_threshold,
        chunk_size,
        pool_segment_size,
        reassembly_segment_size,
        reassembly_max_segments,
    });
}
```

Also update `acquire` (lines 444-470) — add same two optional params and pass them through to `IpcConfig`:

```rust
#[pyo3(signature = (address, shm_threshold=None, chunk_size=None, pool_segment_size=None, reassembly_segment_size=None, reassembly_max_segments=None))]
fn acquire(
    &self,
    py: Python<'_>,
    address: &str,
    shm_threshold: Option<usize>,
    chunk_size: Option<usize>,
    pool_segment_size: Option<usize>,
    reassembly_segment_size: Option<usize>,
    reassembly_max_segments: Option<usize>,
) -> PyResult<PyRustClient> {
    let config = if shm_threshold.is_some() || chunk_size.is_some()
        || pool_segment_size.is_some() || reassembly_segment_size.is_some()
        || reassembly_max_segments.is_some()
    {
        Some(IpcConfig {
            shm_threshold: shm_threshold.unwrap_or(4096),
            chunk_size: chunk_size.unwrap_or(131072),
            pool_segment_size,
            reassembly_segment_size,
            reassembly_max_segments,
        })
    } else {
        None
    };
    // ... rest unchanged ...
}
```

- [ ] **Step 6: Update server FFI**

In `src/c_two/_native/c2-ffi/src/server_ffi.rs`, add two new params to `RustServer::new()` (lines 112-154):

```rust
#[pyo3(signature = (
    address,
    max_frame_size = 268_435_456,
    max_payload_size = 134_217_728,
    max_pool_segments = 4,
    segment_size = 268_435_456,
    chunked_threshold = 67_108_864,
    heartbeat_interval = 10.0,
    heartbeat_timeout = 30.0,
    shm_threshold = 4096,
    reassembly_segment_size = 67_108_864,
    reassembly_max_segments = 4,
))]
fn new(
    address: &str,
    max_frame_size: u64,
    max_payload_size: u64,
    max_pool_segments: u32,
    segment_size: u64,
    chunked_threshold: u64,
    heartbeat_interval: f64,
    heartbeat_timeout: f64,
    shm_threshold: u64,
    reassembly_segment_size: u64,
    reassembly_max_segments: u32,
) -> PyResult<Self> {
    let config = IpcConfig {
        max_frame_size,
        max_payload_size,
        max_pool_segments,
        pool_segment_size: segment_size,
        heartbeat_interval,
        heartbeat_timeout,
        shm_threshold,
        reassembly_segment_size,
        reassembly_max_segments,
        chunk_threshold_ratio: if max_payload_size > 0 {
            chunked_threshold as f64 / max_payload_size as f64
        } else {
            0.9
        },
        ..IpcConfig::default()
    };
    // ... rest unchanged ...
}
```

- [ ] **Step 7: Build and verify Rust compiles**

Run from `src/c_two/_native/`:
```bash
cargo check -p c2-mem -p c2-wire -p c2-ipc -p c2-server
```
Expected: No errors.

- [ ] **Step 8: Build Python extension**

```bash
uv sync --reinstall-package c-two
```
Expected: Build success.

- [ ] **Step 9: Run tests (existing behavior unchanged)**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"
```
Expected: All pass — new fields have defaults matching old hardcoded values.

- [ ] **Step 10: Commit**

```bash
git add src/c_two/_native/
git commit -m "feat(rust): add reassembly pool config to IpcConfig structs and FFI

Both client and server IpcConfig now accept reassembly_segment_size
and reassembly_max_segments. Defaults match previous hardcoded values
(client: 256MB×4, server: 64MB×4). No Python API change yet.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 2: Python IPCConfig + native.py — add reassembly fields

**Files:**
- Modify: `src/c_two/transport/ipc/frame.py:15-88` (IPCConfig dataclass)
- Modify: `src/c_two/transport/server/native.py:96-111` (RustServer creation)

- [ ] **Step 1: Add reassembly fields to Python `IPCConfig`**

In `src/c_two/transport/ipc/frame.py`, add after the existing pool fields (after line 44):

```python
# Reassembly pool settings
reassembly_segment_size: int = 64 * 1024 * 1024  # 64 MB (server default)
reassembly_max_segments: int = 4
```

Add validation in `__post_init__` after the `max_pool_segments` check:

```python
if not (1 <= self.reassembly_max_segments <= 255):
    raise ValueError(
        f'reassembly_max_segments must be >= 1 and <= 255, got {self.reassembly_max_segments}'
    )
if self.reassembly_segment_size <= 0:
    raise ValueError('reassembly_segment_size must be > 0')
```

- [ ] **Step 2: Pass reassembly config to `RustServer`**

In `src/c_two/transport/server/native.py`, update the `RustServer()` call (lines 101-111) to pass the new params:

```python
self._rust_server = RustServer(
    address=bind_address,
    max_frame_size=self._config.max_frame_size,
    max_payload_size=self._config.max_payload_size,
    max_pool_segments=self._config.max_pool_segments,
    segment_size=self._config.pool_segment_size,
    chunked_threshold=chunked_threshold,
    heartbeat_interval=self._config.heartbeat_interval,
    heartbeat_timeout=self._config.heartbeat_timeout,
    shm_threshold=self._config.shm_threshold,
    reassembly_segment_size=self._config.reassembly_segment_size,
    reassembly_max_segments=self._config.reassembly_max_segments,
)
```

- [ ] **Step 3: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"
```
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/ipc/frame.py src/c_two/transport/server/native.py
git commit -m "feat(config): add reassembly pool fields to IPCConfig and pass to RustServer

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 3: Python registry.py — new API functions

**Files:**
- Modify: `src/c_two/transport/registry.py:109-183,579-668` (instance + module-level API)

Replace `set_ipc_config` with three new functions: `set_shm_threshold`, `set_server_ipc_config`, `set_client_ipc_config`.

- [ ] **Step 1: Add `_shm_threshold` to `_ProcessRegistry.__init__`**

In `src/c_two/transport/registry.py`, add to `__init__` (after line 115):

```python
self._shm_threshold: int | None = None
```

- [ ] **Step 2: Replace `set_ipc_config` instance method with three new methods**

Remove the `set_ipc_config` method (lines 145-183). Replace with:

```python
def set_shm_threshold(self, threshold: int) -> None:
    """Set the SHM threshold globally (payload size cutover from inline to SHM).

    This is hardware-dependent and applies to both server and client.
    Must be called **before** any :func:`register` call.

    Parameters
    ----------
    threshold:
        Payload size in bytes above which SHM is used (default 4096).
    """
    with self._lock:
        if self._server is not None:
            raise RuntimeError(
                'Cannot set SHM threshold after CRMs have been registered. '
                'Call set_shm_threshold() before register().',
            )
        if threshold < 0:
            raise ValueError(f'shm_threshold must be >= 0, got {threshold}')
        self._shm_threshold = threshold

def set_server_ipc_config(
    self,
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    reassembly_segment_size: int | None = None,
    reassembly_max_segments: int | None = None,
) -> None:
    """Configure the IPC server pools (response + reassembly).

    Must be called **before** any :func:`register` call.

    Parameters
    ----------
    segment_size:
        Response pool segment size in bytes (default 256 MB).
    max_segments:
        Response pool max segments, 1–255 (default 4).
    reassembly_segment_size:
        Server reassembly pool segment size in bytes (default 64 MB).
    reassembly_max_segments:
        Server reassembly pool max segments, 1–255 (default 4).
    """
    with self._lock:
        if self._server is not None:
            raise RuntimeError(
                'Cannot set server IPC config after CRMs have been registered. '
                'Call set_server_ipc_config() before register().',
            )
        overrides: dict[str, int] = {}
        if segment_size is not None:
            overrides['pool_segment_size'] = segment_size
        if max_segments is not None:
            overrides['max_pool_segments'] = max_segments
        if reassembly_segment_size is not None:
            overrides['reassembly_segment_size'] = reassembly_segment_size
        if reassembly_max_segments is not None:
            overrides['reassembly_max_segments'] = reassembly_max_segments
        if overrides:
            self._explicit_ipc_config = self._make_ipc_config(**overrides)

def set_client_ipc_config(
    self,
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    reassembly_segment_size: int | None = None,
    reassembly_max_segments: int | None = None,
) -> None:
    """Configure the IPC client pools (request + reassembly).

    Must be called **before** any :func:`connect` call.

    Parameters
    ----------
    segment_size:
        Request pool segment size in bytes (default 256 MB).
    max_segments:
        Request pool max segments (default 4).
    reassembly_segment_size:
        Client reassembly pool segment size in bytes (default 256 MB).
    reassembly_max_segments:
        Client reassembly pool max segments (default 4).
    """
    with self._lock:
        self._pool.set_default_config(
            pool_segment_size=segment_size,
            reassembly_segment_size=reassembly_segment_size,
            reassembly_max_segments=reassembly_max_segments,
        )
```

- [ ] **Step 3: Update `_build_ipc_config` to incorporate `_shm_threshold`**

In `_build_ipc_config` (line 579-595), add shm_threshold injection:

```python
def _build_ipc_config(self) -> IPCConfig:
    """Build an :class:`IPCConfig` from the three-level priority chain."""
    if self._explicit_ipc_config is not None:
        cfg = self._explicit_ipc_config
    else:
        overrides: dict[str, int] = {}
        if settings.ipc_segment_size is not None:
            overrides['pool_segment_size'] = settings.ipc_segment_size
        if settings.ipc_max_segments is not None:
            overrides['max_pool_segments'] = settings.ipc_max_segments
        cfg = self._make_ipc_config(**overrides) if overrides else IPCConfig()

    # Apply global shm_threshold override.
    if self._shm_threshold is not None:
        cfg.shm_threshold = self._shm_threshold
    return cfg
```

- [ ] **Step 4: Replace module-level `set_ipc_config` with three new functions**

Remove the module-level `set_ipc_config` function (lines 653-668). Replace with:

```python
def set_shm_threshold(threshold: int = 4096) -> None:
    """Set the global SHM threshold (payload size cutover).

    See :meth:`_ProcessRegistry.set_shm_threshold`.
    """
    _ProcessRegistry.get().set_shm_threshold(threshold)


def set_server_ipc_config(
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    reassembly_segment_size: int | None = None,
    reassembly_max_segments: int | None = None,
) -> None:
    """Configure the IPC server pools.

    See :meth:`_ProcessRegistry.set_server_ipc_config`.
    """
    _ProcessRegistry.get().set_server_ipc_config(
        segment_size=segment_size,
        max_segments=max_segments,
        reassembly_segment_size=reassembly_segment_size,
        reassembly_max_segments=reassembly_max_segments,
    )


def set_client_ipc_config(
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    reassembly_segment_size: int | None = None,
    reassembly_max_segments: int | None = None,
) -> None:
    """Configure the IPC client pools.

    See :meth:`_ProcessRegistry.set_client_ipc_config`.
    """
    _ProcessRegistry.get().set_client_ipc_config(
        segment_size=segment_size,
        max_segments=max_segments,
        reassembly_segment_size=reassembly_segment_size,
        reassembly_max_segments=reassembly_max_segments,
    )
```

- [ ] **Step 5: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"
```
Expected: 1 failure in `test_concurrency_safety.py` (still uses old `set_ipc_config`). All others pass.

- [ ] **Step 6: Commit**

```bash
git add src/c_two/transport/registry.py
git commit -m "feat(api): replace set_ipc_config with set_server_ipc_config, set_client_ipc_config, set_shm_threshold

- set_shm_threshold(threshold): global, hardware-dependent
- set_server_ipc_config(...): server response + reassembly pools
- set_client_ipc_config(...): client request + reassembly pools

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 4: Update exports and all callers

**Files:**
- Modify: `src/c_two/__init__.py:9-19`
- Modify: `src/c_two/transport/__init__.py:10-34`
- Modify: `tests/integration/test_concurrency_safety.py:200`
- Modify: `benchmarks/segment_size_benchmark.py:99,140`
- Modify: `benchmarks/three_mode_benchmark.py:161,189,237`
- Modify: `benchmarks/thread_vs_ipc_benchmark.py:129`

- [ ] **Step 1: Update `__init__.py` exports**

In `src/c_two/__init__.py`, replace `set_ipc_config` import (line 11):

```python
from .transport.registry import (
    set_address,
    set_shm_threshold,
    set_server_ipc_config,
    set_client_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)
```

- [ ] **Step 2: Update `transport/__init__.py` lazy imports**

In `src/c_two/transport/__init__.py`, replace `set_ipc_config` in `__all__` and `_LAZY_IMPORTS`:

`__all__`:
```python
__all__ = [
    'ICRMProxy',
    'Server', 'CRMSlot',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'set_address', 'set_shm_threshold', 'set_server_ipc_config',
    'set_client_ipc_config', 'register', 'connect', 'close',
    'unregister', 'server_address', 'shutdown', 'serve',
]
```

`_LAZY_IMPORTS` — remove `set_ipc_config`, add three new entries:
```python
'set_shm_threshold':       ('.registry',   'set_shm_threshold'),
'set_server_ipc_config':   ('.registry',   'set_server_ipc_config'),
'set_client_ipc_config':   ('.registry',   'set_client_ipc_config'),
```

- [ ] **Step 3: Update `test_concurrency_safety.py`**

In `tests/integration/test_concurrency_safety.py`, line 200, replace:

```python
cc.set_ipc_config(segment_size=1 << 20, max_segments=2)
```

With:

```python
cc.set_server_ipc_config(segment_size=1 << 20, max_segments=2)
cc.set_client_ipc_config(segment_size=1 << 20, max_segments=2)
```

- [ ] **Step 4: Update `segment_size_benchmark.py`**

In `benchmarks/segment_size_benchmark.py`, replace both `cc.set_ipc_config` calls (lines 99 and 140):

```python
# Line 99 (bench_ipc_bytes):
cc.set_server_ipc_config(segment_size=seg_size, max_segments=8)
cc.set_client_ipc_config(segment_size=seg_size, max_segments=8)

# Line 140 (bench_ipc_dict):
cc.set_server_ipc_config(segment_size=seg_size, max_segments=8)
cc.set_client_ipc_config(segment_size=seg_size, max_segments=8)
```

- [ ] **Step 5: Update `three_mode_benchmark.py`**

In `benchmarks/three_mode_benchmark.py`, replace all three `cc.set_ipc_config` calls (lines 161, 189, 237):

Each occurrence of:
```python
cc.set_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
```
Becomes:
```python
cc.set_server_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
cc.set_client_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
```

- [ ] **Step 6: Update `thread_vs_ipc_benchmark.py`**

In `benchmarks/thread_vs_ipc_benchmark.py`, line 129, replace:

```python
cc.set_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
```

With:

```python
cc.set_server_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
cc.set_client_ipc_config(segment_size=2 * 1024 * 1024 * 1024, max_segments=8)
```

- [ ] **Step 7: Run full test suite**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"
```
Expected: All pass.

- [ ] **Step 8: Commit**

```bash
git add src/c_two/__init__.py src/c_two/transport/__init__.py \
       tests/integration/test_concurrency_safety.py \
       benchmarks/segment_size_benchmark.py \
       benchmarks/three_mode_benchmark.py \
       benchmarks/thread_vs_ipc_benchmark.py
git commit -m "refactor: update all callers from set_ipc_config to new split API

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 5: Verification benchmark — 1GB with matching reassembly pool

**Files:**
- Use: `benchmarks/segment_size_benchmark.py`

Run benchmark with client reassembly pool sized to match request pool, eliminating the 1GB chunked-transfer bottleneck.

- [ ] **Step 1: Run benchmark with 2GB reassembly pool**

Update `segment_size_benchmark.py` temporarily (or add config) so that `set_client_ipc_config` also sets `reassembly_segment_size=seg_size`:

```bash
# In bench_ipc_bytes and bench_ipc_dict, change to:
cc.set_client_ipc_config(segment_size=seg_size, max_segments=8,
                         reassembly_segment_size=seg_size, reassembly_max_segments=8)
```

Run:
```bash
C2_RELAY_ADDRESS= uv run python benchmarks/segment_size_benchmark.py
```

- [ ] **Step 2: Compare 1GB results**

Before (2GB-seg, reassembly 256MB×4):
- 1GB bytes: ~597ms
- 1GB dict: ~652ms

After (2GB-seg, reassembly 2GB×8):
- Expected: 1GB should be ~2× the 500MB time (~140-150ms), not 8×

- [ ] **Step 3: Revert benchmark to use default reassembly (no hardcoded override)**

The benchmark should use the new API to configure both pools symmetrically. Update both `bench_ipc_bytes` and `bench_ipc_dict` to their final form:

```python
cc.set_server_ipc_config(segment_size=seg_size, max_segments=8,
                         reassembly_segment_size=seg_size, reassembly_max_segments=8)
cc.set_client_ipc_config(segment_size=seg_size, max_segments=8,
                         reassembly_segment_size=seg_size, reassembly_max_segments=8)
```

- [ ] **Step 4: Commit benchmark update**

```bash
git add benchmarks/segment_size_benchmark.py
git commit -m "bench: configure reassembly pools symmetrically in segment benchmark

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```
