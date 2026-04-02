# IPC Performance Regression Fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore IPC large-payload performance by adding server-side SHM response path, eliminating UDS transit for responses > 4KB.

**Architecture:** The server gets its own MemPool for writing CRM responses to SHM. During handshake, the server announces its SHM segments so the client can mmap them. For large responses (> `shm_threshold`), the server writes to its buddy pool and sends an 11-byte pointer via UDS. The client reads from server SHM and frees the block via the cross-process buddy allocator embedded in the SHM region. For small responses, the existing inline UDS path is preserved.

**Tech Stack:** Rust (c2-server, c2-ipc, c2-mem, c2-ffi), Python (native.py)

---

## Root Cause Summary

| Factor | Old (Python) | New (Rust) | Impact |
|--------|-------------|------------|--------|
| Response transport | SHM pointer (11B on UDS) | Full data inline on UDS | ~4-5× slower |
| Data copies per round-trip | ~2 | ~6 | ~1.5-2× slower |
| Combined | 17.9ms / 100MB | 129.9ms / 100MB | **7.3× regression** |

## File Structure

**Rust crate modifications:**

| File | Action | Responsibility |
|------|--------|---------------|
| `src/c_two/_native/c2-server/src/server.rs` | Modify | Add `response_pool`, announce in handshake, add `write_buddy_reply_with_data` |
| `src/c_two/_native/c2-ipc/src/client.rs` | Modify | Replace `SegmentCache` with `MemPool` for server segments; free after read |
| `src/c_two/_native/c2-ipc/src/shm.rs` | No change | `SegmentCache` stays (used by relay); client uses MemPool instead |
| `src/c_two/_native/c2-ffi/src/server_ffi.rs` | Modify | Pass `shm_threshold` to Server config |

**Python modifications:**

| File | Action | Responsibility |
|------|--------|---------------|
| `src/c_two/transport/server/native.py` | Modify | Pass `shm_threshold` from IPCConfig to RustServer |

**Test files:**

| File | Action | Responsibility |
|------|--------|---------------|
| `tests/integration/test_ipc_buddy_reply.py` | Create | Verify buddy response path works end-to-end |
| `benchmarks/three_mode_benchmark.py` | No change | Used for verification |

---

## Tasks

### Task 1: Add response MemPool to Server

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:76-114`

The Server struct needs a dedicated MemPool for writing CRM response data to SHM. This pool uses the same segment_size and max_segments from the existing IpcConfig.

- [ ] **Step 1: Add `response_pool` field to Server struct**

In `server.rs`, add a new field and initialize it in `Server::new()`:

```rust
pub struct Server {
    config: IpcConfig,
    socket_path: PathBuf,
    dispatcher: RwLock<Dispatcher>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    conn_counter: AtomicU64,
    reassembly_pool: std::sync::Mutex<MemPool>,
    /// Server-side MemPool for writing buddy SHM responses.
    response_pool: std::sync::Mutex<MemPool>,
}
```

In `Server::new()`, after `reassembly_pool` creation, add:

```rust
let response_cfg = PoolConfig {
    segment_size: config.pool_segment_size as usize,
    min_block_size: 4096,
    max_segments: config.max_pool_segments as usize,
    max_dedicated_segments: 4,
    dedicated_gc_delay_secs: 5.0,
    spill_threshold: 0.8,
    spill_dir: PathBuf::from("/tmp/c_two_response_spill"),
};
let mut response_pool = MemPool::new(response_cfg);
response_pool.ensure_ready()
    .map_err(|e| ServerError::Config(format!("response pool init: {e}")))?;
```

Add `response_pool: std::sync::Mutex::new(response_pool)` to the struct initializer.

- [ ] **Step 2: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: Compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/_native/c2-server/src/server.rs
git commit -m "feat(server): add response MemPool for SHM buddy replies"
```

---

### Task 2: Announce server SHM segments in handshake

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:315-357` (handle_handshake)

Currently `handle_handshake` passes `&[]` (empty segment list) to `encode_server_handshake`. We must pass the response pool's segment info so clients can mmap them.

- [ ] **Step 1: Read response pool segments in handshake handler**

In `handle_handshake()`, before building the handshake response, read the server's response pool segments:

```rust
// Collect response pool segment info for handshake.
let (server_segments, server_prefix) = {
    let pool = server.response_pool.lock().unwrap();
    let count = pool.segment_count();
    let mut segs = Vec::with_capacity(count);
    for i in 0..count {
        if let (Some(name), Some(seg)) = (pool.segment_name(i), pool.segment(i)) {
            segs.push((name.to_string(), seg.allocator().data_size() as u32));
        }
    }
    let prefix = pool.prefix().to_string();
    (segs, prefix)
};
```

- [ ] **Step 2: Pass segments to encode_server_handshake**

Replace the existing handshake encoding line:

```rust
// OLD:
let hs_bytes = encode_server_handshake(&[], cap, &routes, "server");

// NEW:
let cap = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED;
let hs_bytes = encode_server_handshake(&server_segments, cap, &routes, &server_prefix);
```

Note: Added `CAP_CHUNKED` to server capabilities (client uses this to know chunked is supported).

- [ ] **Step 3: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: Compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-server/src/server.rs
git commit -m "feat(server): announce response pool segments in handshake"
```

---

### Task 3: Add buddy reply write path to server

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:725-745` (reply helpers)
- Modify: `src/c_two/_native/c2-server/src/server.rs:395-544` (dispatch functions)

The server needs a new reply function that writes response data to the response pool's SHM and sends an 11-byte buddy pointer frame, and the dispatch functions need to choose buddy vs inline based on `shm_threshold`.

- [ ] **Step 1: Add `write_buddy_reply_with_data` function**

Add after the existing `write_reply_with_data` function:

```rust
/// Write a success reply via buddy SHM: allocate from response pool, write
/// data, send 11-byte pointer frame.  Falls back to inline on alloc failure.
async fn write_buddy_reply_with_data(
    response_pool: &std::sync::Mutex<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
) {
    // 1. Allocate from response pool.
    let alloc = {
        let mut pool = response_pool.lock().unwrap();
        match pool.alloc(data.len()) {
            Ok(a) => a,
            Err(_) => {
                // Fallback: send inline.
                write_reply_with_data(writer, request_id, data).await;
                return;
            }
        }
    };

    // 2. Write data to SHM.
    {
        let pool = response_pool.lock().unwrap();
        match pool.data_ptr(&alloc) {
            Ok(ptr) => unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            },
            Err(_) => {
                let _ = response_pool.lock().unwrap().free(&alloc);
                write_reply_with_data(writer, request_id, data).await;
                return;
            }
        }
    }

    // 3. Encode buddy payload + reply control.
    let bp = BuddyPayload {
        seg_idx: alloc.seg_idx as u16,
        offset: alloc.offset,
        data_size: data.len() as u32,
        is_dedicated: alloc.is_dedicated,
    };
    let buddy_bytes = encode_buddy_payload(&bp);
    let ctrl_bytes = encode_reply_control(&ReplyControl::Success);

    let mut payload = Vec::with_capacity(BUDDY_PAYLOAD_SIZE + ctrl_bytes.len());
    payload.extend_from_slice(&buddy_bytes);
    payload.extend_from_slice(&ctrl_bytes);

    // 4. Send frame with FLAG_BUDDY.
    let flags = FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY;
    let frame = encode_frame(request_id, flags, &payload);
    let _ = writer.lock().await.write_all(&frame).await;
}
```

- [ ] **Step 2: Add smart reply helper that chooses buddy vs inline**

```rust
/// Choose buddy SHM or inline reply based on data size and threshold.
async fn smart_reply_with_data(
    response_pool: &std::sync::Mutex<MemPool>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    request_id: u64,
    data: &[u8],
    shm_threshold: u64,
) {
    if data.len() as u64 > shm_threshold {
        write_buddy_reply_with_data(response_pool, writer, request_id, data).await;
    } else {
        write_reply_with_data(writer, request_id, data).await;
    }
}
```

- [ ] **Step 3: Update dispatch_call to use smart_reply**

In `dispatch_call`, change:

```rust
// OLD:
Ok(data) => write_reply_with_data(writer, request_id, &data).await,

// NEW:
Ok(data) => smart_reply_with_data(
    &server.response_pool, writer, request_id, &data,
    server.config.shm_threshold,
).await,
```

- [ ] **Step 4: Update dispatch_buddy_call to use smart_reply**

Same pattern — change the `Ok(data) =>` branch in `dispatch_buddy_call`.

- [ ] **Step 5: Update dispatch_chunked_call to use smart_reply**

Same pattern — change the `Ok(data) =>` branch in `dispatch_chunked_call`.

- [ ] **Step 6: Add required imports**

At the top of server.rs, ensure these are imported:

```rust
use c2_wire::buddy::{encode_buddy_payload, BuddyPayload, BUDDY_PAYLOAD_SIZE};
use c2_wire::flags::FLAG_BUDDY;
```

- [ ] **Step 7: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: Compiles with no errors.

- [ ] **Step 8: Commit**

```bash
git add src/c_two/_native/c2-server/src/server.rs
git commit -m "feat(server): add buddy SHM reply path for large responses"
```

---

### Task 4: Client reads and frees server SHM responses

**Files:**
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:133-145` (IpcClient struct)
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:238-252` (handshake segment opening)
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:318-329` (recv_loop spawn)
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:700-752` (recv_loop function)
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:754-804` (decode_response)

The client must replace `SegmentCache` (read-only mmap) with `MemPool` for server segments. This is necessary because:
1. SegmentCache uses raw mmap offsets, but buddy payloads use data-region-relative offsets
2. MemPool supports `free_at()` to release server blocks via the cross-process buddy allocator embedded in SHM
3. `BuddySegment::open()` attaches to the existing allocator metadata in shared memory, enabling cross-process alloc/free

- [ ] **Step 1: Replace `seg_cache` field with `server_pool` in IpcClient**

```rust
pub struct IpcClient {
    socket_path: PathBuf,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    pending: Arc<StdMutex<PendingMap>>,
    rid_counter: AtomicU32,
    route_tables: HashMap<String, MethodTable>,
    server_segments: Vec<(String, u32)>,
    /// MemPool opened over the server's SHM segments (for buddy reply reads + frees).
    server_pool: Arc<StdMutex<Option<MemPool>>>,
    recv_handle: Option<tokio::task::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    pool: Option<Arc<StdMutex<MemPool>>>,
    config: IpcConfig,
}
```

Update all constructors (`new`, `with_pool`) to initialize `server_pool: Arc::new(StdMutex::new(None))`.

- [ ] **Step 2: Open server segments as MemPool during handshake**

Replace the SegmentCache opening block in `do_handshake` (after `self.server_segments = hs.segments.clone()`) with:

```rust
// Open server SHM segments into a MemPool for buddy response reads + frees.
if !hs.segments.is_empty() {
    let cfg = c2_mem::PoolConfig {
        // Server pool is read-only from our side; config doesn't matter for open_segment.
        segment_size: hs.segments[0].1 as usize,
        min_block_size: 4096,
        max_segments: hs.segments.len(),
        max_dedicated_segments: 4,
        dedicated_gc_delay_secs: 60.0,
        spill_threshold: 1.0,
        spill_dir: std::path::PathBuf::from("/tmp"),
    };
    let mut pool = MemPool::new_with_prefix(cfg, hs.prefix.clone());
    for (idx, (name, size)) in hs.segments.iter().enumerate() {
        let shm_name = if name.starts_with('/') {
            name.clone()
        } else {
            format!("/{name}")
        };
        if let Err(e) = pool.open_segment(&shm_name, *size as usize) {
            eprintln!("Warning: failed to open server SHM segment {idx} ({name}): {e}");
        }
    }
    *self.server_pool.lock().unwrap() = Some(pool);
}
```

- [ ] **Step 3: Pass `server_pool` to recv_loop instead of `seg_cache`**

```rust
let server_pool = self.server_pool.clone();
tokio::spawn(async move {
    recv_loop(reader, pending, server_pool, writer_clone).await;
    connected.store(false, Ordering::Release);
});
```

- [ ] **Step 4: Update `recv_loop` signature**

```rust
async fn recv_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    pending: Arc<StdMutex<PendingMap>>,
    server_pool: Arc<StdMutex<Option<MemPool>>>,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
) {
```

Update the `decode_response` call inside recv_loop accordingly.

- [ ] **Step 5: Update `decode_response` to use MemPool**

```rust
async fn decode_response(
    hdr: &FrameHeader,
    payload: &[u8],
    server_pool: &Arc<StdMutex<Option<MemPool>>>,
) -> Result<Vec<u8>, IpcError> {
    let is_v2 = hdr.is_reply_v2();
    let is_buddy = hdr.is_buddy();

    if !is_v2 {
        return Ok(payload.to_vec());
    }

    if is_buddy {
        if payload.len() < BUDDY_PAYLOAD_SIZE + 1 {
            return Err(IpcError::Decode(DecodeError::BufferTooShort {
                need: BUDDY_PAYLOAD_SIZE + 1,
                have: payload.len(),
            }));
        }
        let (bp, _) = decode_buddy_payload(payload)?;
        let ctrl_start = BUDDY_PAYLOAD_SIZE;
        let (ctrl, _) = decode_reply_control(payload, ctrl_start)?;

        match ctrl {
            ReplyControl::Success => {
                let mut pool_guard = server_pool.lock().unwrap();
                let pool = pool_guard.as_mut().ok_or_else(|| {
                    IpcError::Handshake("server pool not initialised for buddy reply".into())
                })?;

                // Read data from server SHM.
                let ptr = pool.data_ptr_at(
                    bp.seg_idx as u32, bp.offset, bp.is_dedicated,
                ).map_err(|e| IpcError::Handshake(format!("buddy read: {e}")))?;

                let data = unsafe {
                    std::slice::from_raw_parts(ptr, bp.data_size as usize)
                }.to_vec();

                // Free the server's allocation (cross-process via SHM-embedded allocator).
                let _ = pool.free_at(
                    bp.seg_idx as u32, bp.offset, bp.data_size, bp.is_dedicated,
                );

                Ok(data)
            }
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    } else {
        let (ctrl, consumed) = decode_reply_control(payload, 0)?;
        match ctrl {
            ReplyControl::Success => Ok(payload[consumed..].to_vec()),
            ReplyControl::Error(err_data) => Err(IpcError::CrmError(err_data)),
        }
    }
}
```

- [ ] **Step 6: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-ipc`
Expected: Compiles with no errors.

- [ ] **Step 7: Commit**

```bash
git add src/c_two/_native/c2-ipc/src/client.rs
git commit -m "feat(client): open server SHM pool for buddy reply reads + frees"
```

---

### Task 5: Pass `shm_threshold` through FFI to Server

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs:94-136` (PyServer::new)
- Modify: `src/c_two/transport/server/native.py:72-114` (NativeServerBridge.__init__)

The `shm_threshold` config is already in `IpcConfig` on the Rust side (default 4096). It's also already passed through the Python `IPCConfig` to `RustServer`, but the FFI constructor uses `IpcConfig::default()` for the shm_threshold. We need to ensure it's propagated.

- [ ] **Step 1: Add `shm_threshold` parameter to PyServer::new**

In `server_ffi.rs`, add `shm_threshold` to the constructor signature:

```rust
#[new]
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
) -> PyResult<Self> {
    let config = IpcConfig {
        max_frame_size,
        max_payload_size,
        max_pool_segments,
        pool_segment_size: segment_size,
        heartbeat_interval,
        heartbeat_timeout,
        shm_threshold,
        chunk_threshold_ratio: if max_payload_size > 0 {
            chunked_threshold as f64 / max_payload_size as f64
        } else {
            0.9
        },
        ..IpcConfig::default()
    };
    // ...rest unchanged
}
```

- [ ] **Step 2: Pass `shm_threshold` from Python NativeServerBridge**

In `native.py`, in `__init__`, add `shm_threshold` to the `RustServer(...)` constructor call:

```python
self._rust_server = RustServer(
    address=bind_address,
    max_frame_size=self._config.max_frame_size,
    max_payload_size=self._config.max_payload_size,
    max_pool_segments=self._config.max_pool_segments,
    segment_size=self._config.pool_segment_size,
    chunked_threshold=chunked_threshold,
    shm_threshold=self._config.shm_threshold,
)
```

- [ ] **Step 3: Verify Rust compilation**

Run: `cd src/c_two/_native && cargo check -p c2-ffi`
Expected: Compiles with no errors.

- [ ] **Step 4: Verify Python import**

Run: `uv sync --reinstall-package c-two && uv run python -c "from c_two._native import RustServer; print('OK')"`
Expected: Prints "OK".

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/c2-ffi/src/server_ffi.rs src/c_two/transport/server/native.py
git commit -m "feat(ffi): propagate shm_threshold from Python to Rust server"
```

---

### Task 6: Integration test for buddy SHM replies

**Files:**
- Create: `tests/integration/test_ipc_buddy_reply.py`

Write an integration test that verifies large payloads are returned via buddy SHM responses (and arrive correctly).

- [ ] **Step 1: Write the test**

```python
"""Test that large IPC responses use buddy SHM path."""
from __future__ import annotations

import os
import sys
import time

import pytest

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), '../../src/')))

import c_two as cc
from c_two.transport.registry import _ProcessRegistry


@cc.icrm(namespace='test.buddy_reply', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...


class Echo:
    def echo(self, data: bytes) -> bytes:
        return data


@pytest.fixture(autouse=True)
def _cleanup():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


@pytest.mark.timeout(30)
class TestBuddyReply:
    """Verify large responses transit via buddy SHM (not inline UDS)."""

    def _setup_ipc(self, address: str) -> cc.ICRMProxy:
        cc.set_address(address)
        cc.register(IEcho, Echo(), name='echo')
        time.sleep(0.3)
        return cc.connect(IEcho, name='echo', address=address)

    def test_small_payload_roundtrip(self):
        """Small payloads still work (inline path)."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_small')
        data = b'hello'
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_large_payload_roundtrip(self):
        """Large payload (1MB) goes through buddy SHM response path."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_large')
        data = os.urandom(1024 * 1024)  # 1 MB — above 4KB shm_threshold
        result = proxy.echo(data)
        assert result == data
        cc.close(proxy)

    def test_very_large_payload_roundtrip(self):
        """Very large payload (50MB) round-trips correctly."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_vlarge')
        data = os.urandom(50 * 1024 * 1024)  # 50 MB
        result = proxy.echo(data)
        assert len(result) == len(data)
        assert result == data
        cc.close(proxy)

    def test_multiple_large_calls(self):
        """Multiple sequential large calls don't leak memory."""
        proxy = self._setup_ipc('ipc://test_buddy_reply_multi')
        for _ in range(10):
            data = os.urandom(1024 * 1024)  # 1 MB each
            result = proxy.echo(data)
            assert result == data
        cc.close(proxy)
```

- [ ] **Step 2: Run the test**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/integration/test_ipc_buddy_reply.py -v --timeout=30`
Expected: All 4 tests pass.

- [ ] **Step 3: Run full test suite for regression check**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All existing tests still pass (487+).

- [ ] **Step 4: Commit**

```bash
git add tests/integration/test_ipc_buddy_reply.py
git commit -m "test: add integration tests for buddy SHM reply path"
```

---

### Task 7: Benchmark verification

**Files:**
- No file changes — verification only

- [ ] **Step 1: Rebuild with all changes**

```bash
uv sync --reinstall-package c-two
```

- [ ] **Step 2: Run benchmark**

```bash
C2_RELAY_ADDRESS= uv run python benchmarks/three_mode_benchmark.py
```

- [ ] **Step 3: Compare results**

Expected improvements for IPC-bytes mode:

| Payload | Old (pre-v0.4.0) | v0.4.0 (regressed) | Expected (with fix) |
|---------|-------------------|---------------------|---------------------|
| 100MB | 17.9ms | 129.9ms | 40-60ms |
| 50MB | 9.0ms | 64.7ms | 20-30ms |
| 10MB | 2.1ms | 13.8ms | 5-8ms |

The fix eliminates UDS transit for large responses but retains some copies at FFI boundaries. Full zero-copy (Phase 2) would bring numbers closer to the old baseline.

- [ ] **Step 4: Update benchmark_results.tsv with new results**

Save the new numbers as the v0.4.1 baseline.

---

## Phase 2: Copy Reduction (Follow-up)

These are **not part of this plan** but documented as the logical next step:

1. **Server: avoid SHM→Vec copy in `read_peer_data`** — return `&[u8]` slice into SHM, hold lock during callback. Requires lifetime changes to `CrmCallback::invoke`.

2. **Server FFI: avoid PyBytes copy** — use `PyBuffer` or Python buffer protocol to pass SHM data to Python without intermediate PyBytes allocation.

3. **Client: return memoryview into server SHM** — instead of `to_vec() + PyBytes::new()`, implement PyO3 buffer protocol on a `ShmSlice` wrapper so Python gets zero-copy access.

4. **Client FFI: avoid request `to_vec()`** — pass `&[u8]` directly into buddy alloc path by restructuring the `allow_threads` boundary.

These changes would reduce copies from ~4 to ~1, bringing performance back to or below the old Python baseline.
