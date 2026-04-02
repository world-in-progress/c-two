# Zero-Copy SHM Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate 5 of 7 data copies in the server-side IPC path by separating control plane (Rust) from data plane (Python) via buffer protocol objects, reducing 1 GB echo from ~1320 ms to ~200-300 ms.

**Architecture:** Rust passes SHM metadata (segment index, offset, size) instead of payload bytes to Python. Python reads request data via `memoryview(ShmBuffer)` (zero-copy from client SHM) and writes response data via `MemPool.write_from_buffer()` (single memcpy). A new `ResponseMeta` enum replaces `Vec<u8>` return from CRM callbacks.

**Tech Stack:** Rust (PyO3 0.25, tokio), Python ≥ 3.10, `#[pyclass(frozen)]` for free-threading safety.

**Spec:** `docs/superpowers/specs/2026-04-02-zero-copy-shm-design.md`

---

## File Structure

### New files

| File | Responsibility |
|------|---------------|
| `c2-ffi/src/shm_buffer.rs` | `ShmBuffer` pyclass — unified zero-copy read buffer with `__getbuffer__` |

### Modified files

| File | Changes |
|------|---------|
| `c2-mem/src/pool.rs` | `FreeResult::DedicatedFreed` variant; `release_handle` returns `FreeResult` |
| `c2-server/src/dispatcher.rs` | `RequestData` + `ResponseMeta` enums; `CrmCallback::invoke` new signature |
| `c2-server/src/server.rs` | `dispatch_buddy/chunked_call` pass `RequestData`; `smart_reply_from_meta` accepts `ResponseMeta` |
| `c2-server/src/connection.rs` | `peer_pool_arc()` accessor; `ensure_peer_segment()` (lazy-open without data read) |
| `c2-ffi/src/server_ffi.rs` | `PyCrmCallback` converts `RequestData` → `PyShmBuffer`, parses Python return → `ResponseMeta` |
| `c2-ffi/src/lib.rs` | Register `shm_buffer` module |
| `c2-ffi/src/mem_ffi.rs` | `PyMemPool::from_arc` constructor; `MemHandle.coordinates()` |
| `src/c_two/transport/server/native.py` | `_make_dispatcher` accepts `ShmBuffer` + `MemPool` |
| `src/c_two/transport/server/reply.py` | Handle `memoryview` input in `unpack_icrm_result` |
| `tests/integration/test_zero_copy_ipc.py` | Zero-copy IPC integration tests |

---

## Phase 1: Rust Infrastructure

### Task 1: FreeResult::DedicatedFreed + release_handle returns FreeResult

**Files:**
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:19-24` (FreeResult enum)
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:396-399` (free_at dedicated branch)
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:580-594` (release_handle)

- [ ] **Step 1: Add DedicatedFreed variant to FreeResult**

In `c2-mem/src/pool.rs`, change the `FreeResult` enum (line 19-24):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeResult {
    /// Block freed normally, segment still has active allocations.
    Normal,
    /// Block freed and the segment is now completely idle (zero active allocations).
    SegmentIdle { seg_idx: u16 },
    /// Dedicated segment freed — caller may schedule delayed GC.
    DedicatedFreed { seg_idx: u16 },
}
```

- [ ] **Step 2: Fix free_at to return DedicatedFreed**

In `c2-mem/src/pool.rs`, change the `free_at` dedicated branch (line 397-399):

```rust
        if is_dedicated {
            self.free_dedicated(seg_idx);
            Ok(FreeResult::DedicatedFreed { seg_idx: seg_idx as u16 })
        } else if let Some(seg) = self.segments.get(seg_idx as usize) {
```

- [ ] **Step 3: Change release_handle to return FreeResult**

In `c2-mem/src/pool.rs`, change `release_handle` (line 580-594):

```rust
    /// Release resources held by a [`MemHandle`].
    ///
    /// Returns `FreeResult` so callers can trigger deferred GC.
    /// For `FileSpill`, always returns `Normal` (OS handles cleanup via Drop).
    pub fn release_handle(&mut self, handle: MemHandle) -> FreeResult {
        match handle {
            MemHandle::Buddy { seg_idx, offset, len } => {
                self.free_at(seg_idx as u32, offset, len as u32, false)
                    .unwrap_or(FreeResult::Normal)
            }
            MemHandle::Dedicated { seg_idx, .. } => {
                self.free_dedicated(seg_idx as u32);
                FreeResult::DedicatedFreed { seg_idx }
            }
            MemHandle::FileSpill { .. } => {
                // MmapMut dropped here → munmap; file already unlinked
                FreeResult::Normal
            }
        }
    }
```

- [ ] **Step 4: Fix all callers of release_handle**

The only caller that uses the return value is `c2-ffi/src/client_ffi.rs` line 144:

```rust
// In PyResponseBuffer::release() — Handle branch
Some(ResponseBufferInner::Handle { handle, pool }) => {
    if let Ok(mut p) = pool.lock() {
        let _ = p.release_handle(handle);  // was: p.release_handle(handle);
    }
    Ok(())
}
```

Also in the Drop impl (client_ffi.rs ~line 237), same pattern — add `let _ =` prefix.

- [ ] **Step 5: Run Rust checks**

```bash
cd src/c_two/_native && cargo check -p c2-mem -p c2-server -p c2-ffi
```

Expected: no errors. `FreeResult::DedicatedFreed` compiles; all match arms covered.

- [ ] **Step 6: Run existing Rust tests**

```bash
cd src/c_two/_native && cargo test -p c2-mem --no-default-features
```

Expected: all `c2-mem` tests pass. The `FreeResult` change is additive; existing tests match on `Normal` and `SegmentIdle` which are unchanged.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat(c2-mem): add FreeResult::DedicatedFreed, release_handle returns FreeResult

Fixes dedicated segment GC gap: free_at(is_dedicated=true) now returns
DedicatedFreed instead of Normal, enabling callers to schedule delayed GC.
release_handle() now returns FreeResult for all handle types.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 2: ResponseMeta + RequestData enums + CrmCallback new signature

**Files:**
- Modify: `src/c_two/_native/c2-server/src/dispatcher.rs:31-46` (CrmCallback trait)
- Modify: `src/c_two/_native/c2-server/src/dispatcher.rs:127-263` (tests)
- Modify: `src/c_two/_native/c2-server/src/lib.rs:10` (re-export)

- [ ] **Step 1: Add ResponseMeta and RequestData enums to dispatcher.rs**

Insert after line 24 (`impl std::error::Error for CrmError {}`) in `dispatcher.rs`:

```rust
/// Metadata describing how the CRM callback produced its response.
///
/// Returned by `CrmCallback::invoke()`. The server reads these coordinates
/// to build the reply frame — no payload data crosses the FFI boundary.
#[derive(Debug)]
pub enum ResponseMeta {
    /// CRM wrote result into response SHM (via pool.alloc + pool.write).
    ShmAlloc {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Small result returned as inline bytes (< shm_threshold).
    Inline(Vec<u8>),
    /// Method returned None / empty.
    Empty,
}

/// Input data for a CRM method call.
///
/// Pure Rust types — no PyO3 dependency. The `PyCrmCallback` impl in c2-ffi
/// converts these into Python objects (ShmBuffer/bytes) under GIL.
pub enum RequestData {
    /// SHM coordinates from peer (buddy or dedicated).
    /// ShmBuffer.release() frees via pool.free_at().
    Shm {
        pool: Arc<std::sync::Mutex<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Inline bytes from UDS frame.
    Inline(Vec<u8>),
    /// Reassembled MemHandle from chunked transfer.
    /// ShmBuffer.release() returns handle to pool.
    Handle {
        handle: MemHandle,
        pool: Arc<std::sync::Mutex<MemPool>>,
    },
}
```

Add required imports at top of dispatcher.rs:

```rust
use std::sync::Arc;
use c2_mem::{MemPool, MemHandle};
```

- [ ] **Step 2: Change CrmCallback trait signature**

Replace the existing `CrmCallback` trait (lines 31-47) with:

```rust
/// Trait for calling CRM methods from Rust.
///
/// `invoke()` acquires the GIL internally (in PyCrmCallback impl),
/// so callers must NOT hold the GIL when calling this.
///
/// Uses pure Rust types (RequestData, ResponseMeta) — no PyO3 types
/// in the trait interface. PyCrmCallback converts to/from Python objects.
pub trait CrmCallback: Send + Sync + 'static {
    /// Execute a CRM method call.
    ///
    /// # Arguments
    /// * `route_name` — CRM routing name (from cc.register(name=...))
    /// * `method_idx` — Method index (negotiated during handshake)
    /// * `request` — Request payload as RequestData enum
    /// * `response_pool` — Server's response SHM pool (for writing reply data)
    ///
    /// # Returns
    /// ResponseMeta describing where the result lives, or CrmError
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        response_pool: Arc<std::sync::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError>;
}
```

No PyO3 dependency needed in c2-server! All types are pure Rust.

- [ ] **Step 3: Update re-exports in lib.rs**

In `c2-server/src/lib.rs` line 10, add `ResponseMeta` and `RequestData`:

```rust
pub use dispatcher::{CrmCallback, CrmError, CrmRoute, Dispatcher, RequestData, ResponseMeta};
```

- [ ] **Step 4: Update MockCallback in dispatcher tests**

Replace the `MockCallback` impl and usage in tests:

```rust
    struct MockCallback;

    impl CrmCallback for MockCallback {
        fn invoke(
            &self,
            _route: &str,
            _method_idx: u16,
            _request: RequestData,
            _response_pool: Arc<std::sync::RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"echo".to_vec()))
        }
    }
```

Update the test that calls `invoke` directly:

```rust
        let pool = Arc::new(std::sync::RwLock::new(
            MemPool::new(PoolConfig::default()).unwrap()
        ));
        let result = route.callback.invoke(
            "grid", 0,
            RequestData::Inline(b"test".to_vec()),
            pool.clone(),
        ).unwrap();
        assert!(matches!(result, ResponseMeta::Inline(ref v) if v == b"echo"));
```

Also update the `FailCallback` test:

```rust
    fn mock_callback_error() {
        struct FailCallback;
        impl CrmCallback for FailCallback {
            fn invoke(
                &self,
                _route: &str,
                _method_idx: u16,
                _request: RequestData,
                _response_pool: Arc<std::sync::RwLock<MemPool>>,
            ) -> Result<ResponseMeta, CrmError> {
                Err(CrmError::InternalError("method not found".into()))
            }
        }

        let pool = Arc::new(std::sync::RwLock::new(
            MemPool::new(PoolConfig::default()).unwrap()
        ));
        let cb: Arc<dyn CrmCallback> = Arc::new(FailCallback);
        let err = cb.invoke(
            "grid", 99,
            RequestData::Inline(b"test".to_vec()),
            pool,
        ).unwrap_err();
        assert!(matches!(err, CrmError::InternalError(_)));
    }
```

- [ ] **Step 5: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-server
```

Expected: `c2-server` compiles with new trait. `c2-ffi` will have errors (PyCrmCallback not updated yet — that's Task 5). Check only `c2-server` first.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(c2-server): add ResponseMeta/RequestData enums, update CrmCallback

CrmCallback::invoke() now accepts RequestData (pure Rust) and
Arc<RwLock<MemPool>> instead of &[u8] payload, returning ResponseMeta.
No PyO3 dependency needed — PyCrmCallback in c2-ffi converts types.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 3: ShmBuffer pyclass

**Files:**
- Create: `src/c_two/_native/c2-ffi/src/shm_buffer.rs`
- Modify: `src/c_two/_native/c2-ffi/src/lib.rs:13` (add module)

This is the core zero-copy buffer object. It implements Python's buffer protocol so `memoryview(shm_buf)` returns a direct pointer into SHM memory.

- [ ] **Step 1: Create shm_buffer.rs — struct + constructors**

Create `src/c_two/_native/c2-ffi/src/shm_buffer.rs`:

```rust
//! `ShmBuffer` — unified zero-copy read buffer with Python buffer protocol.
//!
//! Wraps SHM coordinates or inline data and exposes them via `memoryview()`.
//! **Thread-safety:** `#[pyclass(frozen)]` + interior `Mutex` for
//! free-threading (Python 3.14t) compatibility.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyBufferError;
use pyo3::ffi;
use pyo3::prelude::*;

use c2_mem::{MemHandle, MemPool};

enum ShmBufferInner {
    /// Small payload inlined in UDS frame (no SHM).
    Inline(Vec<u8>),
    /// Buddy or dedicated SHM block from remote peer.
    PeerShm {
        pool: Arc<Mutex<MemPool>>,
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Chunked reassembly result held in local MemHandle.
    Handle {
        handle: MemHandle,
        pool: Arc<Mutex<MemPool>>,
    },
}

#[pyclass(name = "ShmBuffer", frozen)]
pub struct PyShmBuffer {
    inner: Mutex<Option<ShmBufferInner>>,
    data_len: usize,
    exports: AtomicU32,
}

impl PyShmBuffer {
    pub fn from_inline(data: Vec<u8>) -> Self {
        let len = data.len();
        Self {
            inner: Mutex::new(Some(ShmBufferInner::Inline(data))),
            data_len: len,
            exports: AtomicU32::new(0),
        }
    }

    pub fn from_peer_shm(
        pool: Arc<Mutex<MemPool>>,
        seg_idx: u16, offset: u32, data_size: u32, is_dedicated: bool,
    ) -> Self {
        Self {
            inner: Mutex::new(Some(ShmBufferInner::PeerShm {
                pool, seg_idx, offset, data_size, is_dedicated,
            })),
            data_len: data_size as usize,
            exports: AtomicU32::new(0),
        }
    }

    pub fn from_handle(handle: MemHandle, pool: Arc<Mutex<MemPool>>) -> Self {
        let len = handle.len();
        Self {
            inner: Mutex::new(Some(ShmBufferInner::Handle { handle, pool })),
            data_len: len,
            exports: AtomicU32::new(0),
        }
    }
}
```

- [ ] **Step 2: Add pymethods — release, __len__, __bool__, is_inline**

Append `#[pymethods]` block with `release()`, `__len__`, `__bool__`, `is_inline` getter. `release()` checks `exports > 0` and raises `BufferError` if active memoryviews exist. Frees PeerShm via `pool.free_at()`, Handle via `pool.release_handle()`, Inline drops the Vec. Idempotent on `None`.

- [ ] **Step 3: Add buffer protocol — __getbuffer__ / __releasebuffer__**

`__getbuffer__` acquires inner Mutex, matches variant to get `(ptr, len)`:
- `Inline`: `vec.as_ptr()`
- `PeerShm`: `pool.data_ptr_at(seg_idx, offset, is_dedicated)`
- `Handle`: `pool.handle_slice(&handle).as_ptr()`

Fills `Py_buffer` struct identically to existing `PyResponseBuffer.__getbuffer__`. Increments `exports` atomically. `__releasebuffer__` decrements.

- [ ] **Step 4: Add Drop impl + module registration**

Drop impl mirrors `release()` logic — auto-frees if Python forgot. `register_module` adds `PyShmBuffer` class.

- [ ] **Step 5: Register module in lib.rs**

In `c2-ffi/src/lib.rs`, add `mod shm_buffer;` and call `shm_buffer::register_module(m)?;`.

- [ ] **Step 6: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-ffi
```

Expected: compiles. `PyShmBuffer` is a new type with no callers yet.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "feat(c2-ffi): add ShmBuffer pyclass with zero-copy buffer protocol

ShmBuffer wraps SHM coordinates (PeerShm), reassembly handles (Handle),
or inline bytes (Inline) and exposes them via __getbuffer__ for
memoryview() zero-copy access. Thread-safe via frozen pyclass + Mutex.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 2: Server-Side Integration

### Task 4: Expose response_pool + peer_shm pool as Arc<Mutex<MemPool>>

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:81-92` (Server fields visibility)
- Modify: `src/c_two/_native/c2-server/src/connection.rs:78-98` (Connection peer pool accessor)

The server's `response_pool` and each connection's `peer_shm.pool` need to be accessible as `Arc<Mutex<MemPool>>` so they can be shared with `ShmBuffer` and `PyCrmCallback`.

- [ ] **Step 1: Expose response_pool as Arc**

In `server.rs`, change `response_pool` field from `std::sync::Mutex<MemPool>` to `Arc<std::sync::Mutex<MemPool>>`:

```rust
pub struct Server {
    // ... existing fields ...
    /// Server-side MemPool for chunked reassembly buffers.
    reassembly_pool: Arc<std::sync::Mutex<MemPool>>,
    /// Server-side MemPool for writing buddy SHM responses.
    response_pool: Arc<std::sync::Mutex<MemPool>>,
}
```

Wrap with `Arc::new(...)` in `Server::new()`. All existing callers use `&server.response_pool` which still works via `Deref`.

Add a public accessor:

```rust
    /// Get a shared reference to the response pool (for zero-copy dispatch).
    pub fn response_pool_arc(&self) -> Arc<std::sync::Mutex<MemPool>> {
        Arc::clone(&self.response_pool)
    }

    /// Get a shared reference to the reassembly pool.
    pub fn reassembly_pool_arc(&self) -> Arc<std::sync::Mutex<MemPool>> {
        Arc::clone(&self.reassembly_pool)
    }
```

- [ ] **Step 2: Expose connection peer pool as Arc**

In `connection.rs`, add a public method to get a cloneable reference to the peer pool:

```rust
    /// Get a shared reference to the peer's MemPool (for ShmBuffer construction).
    ///
    /// Returns None if handshake hasn't completed yet.
    pub fn peer_pool_arc(&self) -> Option<Arc<Mutex<MemPool>>> {
        let state = self.peer_shm.lock().unwrap();
        state.pool_arc.clone()
    }
```

This requires changing `PeerShmState.pool` from `Option<MemPool>` to storing an `Arc<Mutex<MemPool>>` alongside the owned pool. The simplest approach: add a `pool_arc` field that wraps the pool in an Arc after handshake initialization.

In `PeerShmState`:
```rust
struct PeerShmState {
    // ... existing fields ...
    pool: Option<MemPool>,
    pool_arc: Option<Arc<Mutex<MemPool>>>,
}
```

In `init_peer_shm`, after `state.pool = Some(pool);`, add:
```rust
    // Wrap in Arc for sharing with ShmBuffer instances.
    let pool_ref = state.pool.as_mut().unwrap() as *mut MemPool;
    // SAFETY: pool_arc lives inside PeerShmState alongside pool;
    // the Arc<Mutex> provides interior mutability for the same pool.
```

**Actually, simpler approach**: Change `pool` to be `Arc<Mutex<MemPool>>` directly. All callers already lock:

```rust
struct PeerShmState {
    prefix: String,
    segment_names: Vec<String>,
    segment_sizes: Vec<u32>,
    buddy_segment_size: usize,
    pool: Option<Arc<Mutex<MemPool>>>,
}
```

Then `ensure_buddy_segment` and `ensure_dedicated_segment` take `&self` (lock the inner Mutex), and `read_peer_data` / `free_peer_block` lock the inner Mutex instead of the outer one.

**Wait — this adds a nested lock** (outer `peer_shm: Mutex<PeerShmState>` + inner `Arc<Mutex<MemPool>>`). The existing code uses a single outer Mutex. To avoid restructuring all access patterns now, keep the existing pattern but add a separate `pool_arc` field that shares the same underlying data via `unsafe` aliasing.

**Cleanest approach for this task**: Just extract an `Arc<Mutex<MemPool>>` in the constructor and store it. The `PeerShmState.pool` becomes `Option<Arc<Mutex<MemPool>>>`:

In `PeerShmState`:
```rust
pool: Option<Arc<std::sync::Mutex<MemPool>>>,
```

In `ensure_buddy_segment`:
```rust
fn ensure_buddy_segment(&self, seg_idx: u32) -> Result<(), String> {
    let pool_arc = self.pool.as_ref().ok_or("peer pool not initialised")?;
    let mut pool = pool_arc.lock().unwrap();
    // ... rest unchanged, using `pool` instead of self.pool.as_mut()
}
```

In `init_peer_shm`:
```rust
state.pool = Some(Arc::new(std::sync::Mutex::new(pool)));
```

In `Connection::peer_pool_arc()`:
```rust
pub fn peer_pool_arc(&self) -> Option<Arc<std::sync::Mutex<MemPool>>> {
    let state = self.peer_shm.lock().unwrap();
    state.pool.clone()
}
```

Now `read_peer_data`, `free_peer_block`, `gc_peer_buddy` also change to lock the inner `pool_arc` Mutex (but the outer `peer_shm` Mutex is still needed for `ensure_*` which mutates pool).

**Important**: `ensure_buddy_segment` calls `pool.open_segment()` which requires `&mut MemPool`. So it must lock the inner Mutex for write. This is fine — the outer Mutex on `PeerShmState` serializes `ensure_*` calls, and the inner Mutex on the pool can be shared with ShmBuffer.

- [ ] **Step 3: Update all PeerShmState callers**

Update `read_peer_data`, `free_peer_block`, `gc_peer_buddy`, `ensure_buddy_segment`, `ensure_dedicated_segment` to use `pool_arc.lock().unwrap()` pattern.

- [ ] **Step 4: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-server
```

Expected: compiles. Existing behavior unchanged — only storage type changed from `MemPool` to `Arc<Mutex<MemPool>>`.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor(c2-server): expose response_pool and peer pool as Arc<Mutex<MemPool>>

Enables ShmBuffer instances to hold shared references to pools for
zero-copy buffer protocol access and deferred SHM freeing.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 5: Update PyCrmCallback + expose response_pool as PyMemPool

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs:27-69` (PyCrmCallback invoke)
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs:85-138` (PyServer fields)
- Modify: `src/c_two/_native/c2-ffi/src/mem_ffi.rs:189-191` (PyMemPool from_arc constructor)

- [ ] **Step 1: Add PyMemPool::from_arc constructor**

In `mem_ffi.rs`, add a public constructor for `PyMemPool` that wraps an existing `Arc<RwLock<MemPool>>`:

```rust
impl PyMemPool {
    /// Create from an existing shared pool (used by server_ffi to expose response_pool).
    pub fn from_arc(pool: Arc<RwLock<MemPool>>) -> Self {
        Self { pool }
    }
}
```

- [ ] **Step 2: Update PyCrmCallback::invoke**

Replace the existing `invoke` (lines 37-69) with the new signature that passes `PyObject` request_buffer and response_pool, and uses `parse_response_meta` to convert the Python return.

Key: No more `PyBytes::new(py, payload)` (eliminates copy C3). No more `bytes.as_bytes().to_vec()` (eliminates copy C4).

- [ ] **Step 3: Add parse_response_meta helper**

Python dispatcher returns:
- `None` → `ResponseMeta::Empty`
- `bytes` → `ResponseMeta::Inline(vec)`
- `(seg_idx, offset, data_size, is_dedicated)` tuple → `ResponseMeta::ShmAlloc`

- [ ] **Step 4: Store response_pool_obj on PyServer**

Add `response_pool_obj: Mutex<Option<PyObject>>` field. Populate in `start()` using `Server::response_pool_arc()` + `PyMemPool::from_arc()`.

- [ ] **Step 5: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-ffi
```

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat(c2-ffi): PyCrmCallback passes ShmBuffer + PyMemPool to Python

Eliminates copies C3 (PyBytes::new) and C4 (as_bytes().to_vec()).
parse_response_meta converts Python return into ResponseMeta.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 6: Update dispatch_buddy_call — pass RequestData instead of copying

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:495-598` (dispatch_buddy_call)
- Modify: `src/c_two/_native/c2-server/src/connection.rs` (add ensure_peer_segment)

This is where copy C2 (`.to_vec()` from peer SHM) is eliminated. Instead of reading data from peer SHM into a Vec, we construct `RequestData::Shm` with the coordinates and pass it to `callback.invoke()`. The `PyCrmCallback` impl (Task 10) creates a `PyShmBuffer` under GIL.

- [ ] **Step 1: Add ensure_peer_segment to Connection**

Expose the lazy-open as a separate step (no data read):

```rust
    /// Ensure the peer's SHM segment at `seg_idx` is mapped.
    pub fn ensure_peer_segment(&self, seg_idx: u16, data_size: u32, is_dedicated: bool) -> Result<(), String> {
        let mut state = self.peer_shm.lock().unwrap();
        if is_dedicated {
            state.ensure_dedicated_segment(seg_idx as u32, data_size as usize)
        } else {
            state.ensure_buddy_segment(seg_idx as u32)
        }
    }
```

- [ ] **Step 2: Update dispatch_buddy_call**

Replace `read_peer_data → callback.invoke(payload=&[u8])` with:

```rust
    // After decoding buddy pointer and call control:

    // 1. Ensure peer segment is mapped (lazy-open).
    if let Err(e) = conn.ensure_peer_segment(bp.seg_idx, bp.data_size, bp.is_dedicated) {
        warn!("ensure_peer_segment failed: {}", e);
        conn.flight_dec();
        return;
    }

    // 2. Build RequestData — NO data copy.
    let peer_pool = match conn.peer_pool_arc() {
        Some(p) => p,
        None => { warn!("no peer SHM state"); conn.flight_dec(); return; }
    };
    let request = RequestData::Shm {
        pool: peer_pool,
        seg_idx: bp.seg_idx,
        offset: bp.offset,
        data_size: bp.data_size,
        is_dedicated: bp.is_dedicated,
    };

    // 3. Route + execute via scheduler.
    let callback = Arc::clone(&route.callback);
    let name = ctrl.route_name;
    let idx = ctrl.method_idx;
    let response_pool = server.response_pool_arc();

    let result = route
        .scheduler
        .execute(idx, move || callback.invoke(&name, idx, request, response_pool))
        .await;

    // 4. Handle ResponseMeta (uses smart_reply_from_meta from Task 8).
    match result {
        Ok(meta) => smart_reply_from_meta(writer, request_id, meta).await,
        Err(e) => write_error_reply(writer, request_id, e).await,
    }

    conn.flight_dec();
```

Key: `conn.read_peer_data()` and `conn.free_peer_block()` calls are **removed**. ShmBuffer (constructed in PyCrmCallback::invoke) owns the SHM data lifecycle.

- [ ] **Step 3: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-server
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(c2-server): dispatch_buddy_call passes RequestData::Shm

Eliminates copy C2 (read_peer_data .to_vec()). RequestData::Shm wraps
peer SHM coordinates; PyCrmCallback creates ShmBuffer under GIL.
dispatch_buddy_call no longer reads or frees peer data — ShmBuffer owns it.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 7: Update dispatch_chunked_call — use RequestData::Handle

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:604-787` (dispatch_chunked_call)

Chunked transfers reassemble payload into server's `reassembly_pool` (a `MemPool`). Currently the assembled data is read via `.to_vec()` (line 740-742). With `RequestData::Handle`, we pass the `MemHandle` directly — the `PyCrmCallback` creates a `ShmBuffer` from it.

- [ ] **Step 1: Replace assembled data copy with RequestData::Handle**

After chunk reassembly completes, instead of:
```rust
let data = pool.handle_slice(&handle).to_vec();
pool.release_handle(handle);
```

Do:
```rust
let reassembly_pool_arc = server.reassembly_pool_arc();
let request = RequestData::Handle {
    handle,
    pool: reassembly_pool_arc,
};
```

The handle's lifetime is now owned by `RequestData` → `ShmBuffer` → Python memoryview. When Python calls `release()` or the ShmBuffer is dropped, it returns the handle to the pool.

- [ ] **Step 2: Verify chunk assembly integration**

`ChunkAssembler::push()` returns `Option<MemHandle>` when all chunks are received. The handle points to data in `reassembly_pool`. The pool must be `Arc<Mutex<MemPool>>` (same change as Task 4 for `response_pool`).

Key: `Server::reassembly_pool` also needs `Arc` wrapping and a `reassembly_pool_arc()` accessor. Add this alongside the `response_pool_arc()` accessor from Task 4.

- [ ] **Step 3: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-server
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(c2-server): dispatch_chunked_call passes RequestData::Handle

Eliminates copy of assembled chunked payload. MemHandle is owned by
ShmBuffer; Python reads via memoryview and releases when done.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 8: Update smart_reply_with_data for ResponseMeta

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:928-949` (smart_reply_with_data → smart_reply_from_meta)
- Modify: `src/c_two/_native/c2-server/src/server.rs:833-891` (write_buddy_reply_with_data)

Currently `smart_reply_with_data` accepts `&[u8]` and decides inline vs buddy. With the new flow, Python decides how to write the response (via `ResponseMeta`), so the server just needs to handle the framing.

- [ ] **Step 1: Create smart_reply_from_meta**

```rust
async fn smart_reply_from_meta(
    response_pool: &Arc<std::sync::RwLock<MemPool>>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    meta: ResponseMeta,
    shm_threshold: usize,
    chunk_size: usize,
) {
    match meta {
        ResponseMeta::Empty => {
            write_reply(writer, request_id, &ReplyControl::Ok { data: &[] }).await;
        }
        ResponseMeta::Inline(data) => {
            // Small data or SHM allocation failure — inline in UDS frame.
            write_reply(writer, request_id, &ReplyControl::Ok { data: &data }).await;
        }
        ResponseMeta::ShmAlloc { seg_idx, offset, data_size, is_dedicated } => {
            // Python wrote directly into response_pool SHM.
            // Send buddy/dedicated pointer frame (no data copy!).
            write_buddy_reply_meta(
                writer, request_id, seg_idx, offset, data_size, is_dedicated,
            ).await;
        }
    }
}
```

- [ ] **Step 2: Create write_buddy_reply_meta**

Unlike `write_buddy_reply_with_data` which copies data INTO SHM, this just sends a buddy pointer frame — the data is already in SHM (Python wrote it via `PyMemPool.alloc` + `memoryview`).

```rust
async fn write_buddy_reply_meta(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    seg_idx: u16,
    offset: u32,
    data_size: u32,
    is_dedicated: bool,
) {
    let ctrl = encode_reply_control_ok(0); // 0-byte inline body
    let bp = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated);
    let frame = encode_frame(
        request_id,
        FLAG_SHM_BUDDY | FLAG_REPLY,
        &ctrl,
        Some(&bp),
    );
    write_frame(writer, &frame).await;
}
```

- [ ] **Step 3: Remove old smart_reply_with_data (after all callers migrated)**

The old function can be removed once Tasks 6 and 7 are complete (all dispatch paths use `smart_reply_from_meta`).

- [ ] **Step 4: Run cargo check**

```bash
cd src/c_two/_native && cargo check -p c2-server
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(c2-server): smart_reply_from_meta handles ResponseMeta

Replaces smart_reply_with_data. ShmAlloc path sends buddy pointer
without copying — Python already wrote data to response_pool SHM.
Eliminates copy C5 (copy_nonoverlapping in old write_buddy_reply_with_data).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 3: Python Integration

### Task 9: Update Python dispatcher to use ShmBuffer + MemPool

**Files:**
- Modify: `src/c_two/transport/server/native.py:295-326` (_make_dispatcher)
- Modify: `src/c_two/transport/server/reply.py` (unpack_icrm_result)

The Python dispatcher signature changes from `(route_name, method_idx, payload_bytes) -> reply_bytes` to `(route_name, method_idx, shm_buffer, response_pool) -> ResponseMeta`.

- [ ] **Step 1: Update _make_dispatcher closure**

The closure now receives `ShmBuffer` (buffer-protocol object) and `MemPool` (for response allocation). It:
1. Reads request data via `memoryview(shm_buffer)` — zero-copy from SHM
2. Deserializes arguments from the memoryview
3. Calls CRM method
4. Serializes result
5. If result is small → return `bytes` (Rust treats as `ResponseMeta::Inline`)
6. If result is large → allocate from `response_pool`, write via `memoryview`, return `(seg_idx, offset, data_size, is_dedicated)` tuple
7. Releases request ShmBuffer after deserialization

```python
def _make_dispatcher(self):
    registry = self._registry
    method_tables = self._method_tables
    SHM_THRESHOLD = self._config.shm_threshold

    def dispatch(route_name: str, method_idx: int,
                 request_buf, response_pool) -> object:
        # 1. Read request payload via zero-copy memoryview
        mv = memoryview(request_buf)
        try:
            crm, method, is_read = registry.resolve(route_name, method_idx)
            args = method_tables[route_name].deserialize_args(method_idx, bytes(mv))
        finally:
            request_buf.release()  # Free SHM immediately after deser

        # 2. Execute CRM method
        result = method(crm, *args)

        # 3. Serialize result
        reply_data = method_tables[route_name].serialize_result(method_idx, result)
        if reply_data is None or len(reply_data) == 0:
            return None  # ResponseMeta::Empty

        # 4. Decide inline vs SHM based on threshold
        if len(reply_data) <= SHM_THRESHOLD:
            return reply_data  # ResponseMeta::Inline

        # 5. Allocate in response_pool, write via memoryview
        handle = response_pool.alloc(len(reply_data))
        if handle is None:
            return reply_data  # Fallback to inline

        mv_out = response_pool.handle_to_memoryview(handle)
        mv_out[:len(reply_data)] = reply_data
        seg_idx, offset, data_size, is_dedicated = handle.coordinates()
        response_pool.release_memoryview(mv_out)  # Unpin but keep allocated
        return (seg_idx, offset, data_size, is_dedicated)

    return dispatch
```

**Thread-safety notes:**
- `request_buf.release()` under `ShmBuffer`'s internal Mutex — safe with free-threading
- `response_pool.alloc()` acquires `RwLock<MemPool>` — safe with free-threading
- No shared mutable Python state touched — `crm`, `method`, `args` are all local

- [ ] **Step 2: Update unpack_icrm_result in reply.py**

The function currently handles `memoryview` → `bytes` conversion for the old path. Update to pass through result as-is (the dispatcher handles all conversion):

```python
def unpack_icrm_result(result):
    """Convert CRM method result for serialization.
    
    This function is now simplified — the dispatcher in _make_dispatcher
    handles memoryview/SHM allocation decisions. This just handles
    CRM method return value normalization.
    """
    if result is None:
        return None
    if isinstance(result, memoryview):
        return bytes(result)
    return result
```

- [ ] **Step 3: Add handle_to_memoryview and coordinates to PyMemPool/MemHandle**

If `PyMemPool.handle_to_memoryview` and `MemHandle.coordinates()` don't exist yet, add them in `mem_ffi.rs`:

```rust
// In PyMemPool impl
#[pyo3(name = "handle_to_memoryview")]
fn handle_to_memoryview<'py>(&self, py: Python<'py>, handle: &PyMemHandle) -> PyResult<Bound<'py, PyMemoryView>> {
    let pool = self.pool.read().map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("pool lock poisoned"))?;
    let slice = pool.handle_slice_mut(&handle.inner);
    // Safety: slice is valid while handle is alive and pool is locked
    // We return a memoryview — caller must release before handle is freed
    unsafe {
        PyMemoryView::from_memory(py, slice.as_mut_ptr(), slice.len())
    }
}

// In PyMemHandle impl
#[pyo3(name = "coordinates")]
fn coordinates(&self) -> (u16, u32, u32, bool) {
    match &self.inner {
        MemHandle::Buddy { seg_idx, offset, size, .. } => (*seg_idx as u16, *offset, *size, false),
        MemHandle::Dedicated { seg_idx, size, .. } => (*seg_idx as u16, 0, *size, true),
        MemHandle::FileSpill { .. } => panic!("FileSpill has no SHM coordinates"),
    }
}
```

Note: This is a simplified sketch. The actual memoryview implementation must ensure the GIL (or free-threading lock) keeps the memory valid. Use `PyBuffer` protocol on `PyMemHandle` instead of a separate `handle_to_memoryview` method — cleaner and matches existing `PyResponseBuffer` pattern.

- [ ] **Step 4: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(transport): Python dispatcher uses ShmBuffer + MemPool

Zero-copy request reading via memoryview(shm_buffer).
Response allocation via response_pool for large payloads.
Eliminates all remaining Python-side data copies.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 10: PyCrmCallback bridges RequestData → ShmBuffer/bytes for Python

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs` (PyCrmCallback::invoke)

This is the bridge between the Rust dispatcher (Task 6-7) and the Python dispatcher (Task 9). `PyCrmCallback::invoke` receives `RequestData` + `Arc<RwLock<MemPool>>` and must:
1. Convert `RequestData` to a Python-visible object under GIL
2. Call the Python dispatcher
3. Convert the Python return to `ResponseMeta`

- [ ] **Step 1: Implement PyCrmCallback::invoke for RequestData**

```rust
impl CrmCallback for PyCrmCallback {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        response_pool: Arc<std::sync::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        Python::with_gil(|py| {
            // 1. Convert RequestData to Python object
            let request_obj: PyObject = match request {
                RequestData::Shm { pool, seg_idx, offset, data_size, is_dedicated } => {
                    let buf = PyShmBuffer::from_peer_shm(pool, seg_idx, offset, data_size, is_dedicated);
                    Py::new(py, buf)
                        .map_err(|e| CrmError::InternalError(format!("ShmBuffer creation failed: {e}")))?
                        .into_any()
                        .unbind()
                }
                RequestData::Handle { handle, pool } => {
                    let buf = PyShmBuffer::from_handle(handle, pool);
                    Py::new(py, buf)
                        .map_err(|e| CrmError::InternalError(format!("ShmBuffer creation failed: {e}")))?
                        .into_any()
                        .unbind()
                }
                RequestData::Inline(data) => {
                    PyBytes::new(py, &data).unbind().into_any().unbind()
                }
            };

            // 2. Wrap response_pool as PyMemPool
            let pool_obj = Py::new(py, PyMemPool::from_arc(response_pool))
                .map_err(|e| CrmError::InternalError(format!("PyMemPool creation failed: {e}")))?
                .into_any()
                .unbind();

            // 3. Call Python dispatcher
            let result = self.callback.call1(
                py,
                (route_name, method_idx, request_obj, pool_obj),
            ).map_err(|e| {
                let msg = e.to_string();
                CrmError::InternalError(msg)
            })?;

            // 4. Parse return value into ResponseMeta
            parse_response_meta(py, result)
        })
    }
}
```

- [ ] **Step 2: Implement parse_response_meta**

```rust
fn parse_response_meta(py: Python<'_>, result: PyObject) -> Result<ResponseMeta, CrmError> {
    // None → Empty
    if result.is_none(py) {
        return Ok(ResponseMeta::Empty);
    }

    // bytes → Inline
    if let Ok(bytes) = result.downcast_bound::<PyBytes>(py) {
        return Ok(ResponseMeta::Inline(bytes.as_bytes().to_vec()));
    }

    // tuple(seg_idx, offset, data_size, is_dedicated) → ShmAlloc
    if let Ok(tuple) = result.downcast_bound::<PyTuple>(py) {
        if tuple.len() == 4 {
            let seg_idx: u16 = tuple.get_item(0)?.extract()?;
            let offset: u32 = tuple.get_item(1)?.extract()?;
            let data_size: u32 = tuple.get_item(2)?.extract()?;
            let is_dedicated: bool = tuple.get_item(3)?.extract()?;
            return Ok(ResponseMeta::ShmAlloc { seg_idx, offset, data_size, is_dedicated });
        }
    }

    Err(CrmError::InternalError(format!(
        "Unexpected dispatcher return type: {}",
        result.bind(py).get_type().name().unwrap_or("unknown")
    )))
}
```

- [ ] **Step 3: Run cargo check + tests**

```bash
cd src/c_two/_native && cargo check -p c2-ffi
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(c2-ffi): PyCrmCallback bridges RequestData to Python ShmBuffer

RequestData::Shm/Handle → PyShmBuffer (buffer protocol)
RequestData::Inline → PyBytes
Python return → ResponseMeta via parse_response_meta.
Completes the zero-copy server-side data path.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 4: Integration Tests + Benchmark

### Task 11: Full integration test suite

**Files:**
- Test: `tests/integration/test_zero_copy_ipc.py`

Verify the full zero-copy path works end-to-end with various payload sizes and edge cases.

- [ ] **Step 1: Write integration tests**

```python
"""Integration tests for zero-copy SHM IPC path."""
import c_two as cc
from tests.fixtures.hello import IHello, Hello

class TestZeroCopyIPC:
    """Test zero-copy data path for different payload sizes."""

    def test_inline_small(self):
        """Payloads below SHM threshold use inline path."""
        # 64-byte payload → inline (no SHM)

    def test_buddy_medium(self):
        """Payloads above threshold use buddy SHM (zero-copy)."""
        # 1KB, 1MB payloads → buddy SHM

    def test_dedicated_large(self):
        """Payloads exceeding buddy segment use dedicated SHM."""
        # 100MB payload → dedicated SHM

    def test_round_trip_data_integrity(self):
        """Data integrity check across all tiers."""
        # Send specific byte pattern, verify exact match on return

    def test_shm_buffer_release(self):
        """ShmBuffer is freed after memoryview is released."""
        # Verify no SHM leaks after multiple round-trips

    def test_concurrent_calls(self):
        """Multiple concurrent calls don't interfere."""
        # 10 parallel calls with different payload sizes
```

- [ ] **Step 2: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/integration/test_zero_copy_ipc.py -v --timeout=30
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "test: zero-copy IPC integration tests

Covers inline, buddy, dedicated, data integrity, SHM leak check,
and concurrent call paths.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 12: Benchmark (64B to 1GB)

**Files:**
- Run: `measure_v3.py` or `measure_v3_large.py`

- [ ] **Step 1: Run full benchmark**

```bash
uv run python measure_v3_large.py
```

Capture results for payload sizes: 64B, 1KB, 64KB, 1MB, 10MB, 100MB, 512MB, 1GB.

- [ ] **Step 2: Compare with baseline**

Expected improvement targets:
- 100MB: was 129.9ms (SHM response path), target < 50ms (eliminate request copies too)
- 1GB: was 1320ms, target < 300ms
- Small payloads (< 1KB): should remain unchanged (inline path unaffected)

- [ ] **Step 3: Record results**

Save benchmark comparison to `docs/superpowers/specs/2026-04-02-zero-copy-benchmark-results.md`.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "docs: zero-copy SHM benchmark results

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 13: Implementation Report

**Files:**
- Create: `docs/superpowers/specs/2026-04-02-zero-copy-implementation-report.md`

- [ ] **Step 1: Write report**

Document:
- Copies eliminated (C2, C3, C4, C5) with before/after
- Benchmark results table
- Any remaining copies (C0 serialization, C1 deserialization are inherent)
- Architecture decisions made during implementation
- Known limitations and future work

- [ ] **Step 2: Commit**

```bash
git add -A && git commit -m "docs: zero-copy SHM implementation report

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```
