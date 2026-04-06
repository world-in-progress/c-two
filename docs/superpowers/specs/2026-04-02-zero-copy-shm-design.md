# Zero-Copy SHM Architecture — Control/Data Plane Separation

**Date:** 2026-04-02
**Status:** Design
**Branch:** `fix/ipc-perf-regression`
**Prerequisite:** `2026-04-02-ipc-defect-fix-design.md` (13 tasks completed)

## 1. Problem Statement

Current IPC echo for 1 GB payload takes ~1320 ms. Theoretical memcpy time is ~45 ms. Root cause: **7 unnecessary data copies** across the PyO3 FFI boundary.

### Copy Chain (Request → CRM → Response)

| # | Location | Description |
|---|----------|-------------|
| C0 | `client_ffi.rs:312` | `data.to_vec()` — Python bytes → Rust Vec (GIL release) |
| C1 | `client.rs:615` | `copy_nonoverlapping` — Rust Vec → client SHM |
| C2 | `connection.rs:217` | `.to_vec()` — client SHM → Rust Vec (server read) |
| C3 | `server_ffi.rs:45` | `PyBytes::new(py, payload)` — Rust Vec → Python PyBytes |
| C4 | `server_ffi.rs:53` | `.as_bytes().to_vec()` — Python result bytes → Rust Vec |
| C5 | `server.rs:857` | `copy_nonoverlapping` — Rust Vec → response SHM |
| C6 | `transferable.py:336` | `bytes(result)` — memoryview → Python bytes |

**Cost estimate:** 7 × 45 ms memcpy + ~400 ms malloc = ~715 ms in copies alone.

### Target

Reduce to **2 copies** (one client→SHM write, one SHM→client read), achieving ~200-300 ms for 1 GB echo.

## 2. Architecture Overview

**Core principle:** Rust owns the **control plane** (framing, UDS transport, routing, SHM lifecycle). Python owns the **data plane** (reading/writing SHM directly via buffer protocol objects).

No payload data crosses the PyO3 FFI boundary. Rust passes only **metadata** (segment index, offset, size, type) to Python. Python creates `memoryview` objects pointing directly at SHM memory.

```
Client Process                          Server Process
┌──────────────┐                      ┌──────────────────────────┐
│  Python      │                      │  Python (data plane)     │
│  serialize   │                      │  memoryview(req_buf)     │
│     ↓        │                      │  → CRM method            │
│  pool.alloc  │                      │  → pool.alloc + write    │
│  pool.write  │  ── UDS control ──→  │                          │
│  (1 memcpy)  │  (frame header only) │  (1 memcpy for response) │
└──────────────┘                      └──────────────────────────┘
       ↓                                        ↓
┌──────────────┐                      ┌──────────────────────────┐
│  Rust        │                      │  Rust (control plane)    │
│  (control)   │                      │  parse frame header      │
│  send frame  │                      │  create ShmBuffer        │
│  header      │                      │  → pass metadata to Py   │
│              │                      │  read ResponseMeta       │
│              │  ←── UDS control ──  │  send reply frame header │
└──────────────┘                      └──────────────────────────┘
```

## 3. Component Design

### 3.1 ShmBuffer (Rust → Python, unified read buffer)

Replaces both `PyResponseBuffer` (client reads server response) and the implicit server-side request read. A single `#[pyclass(frozen)]` that implements `__getbuffer__` for zero-copy memoryview access.

```rust
#[pyclass(frozen)]
pub struct ShmBuffer {
    inner: Mutex<Option<ShmBufferInner>>,
}

enum ShmBufferInner {
    /// Small payload inlined in UDS frame (no SHM).
    Inline(Vec<u8>),

    /// Buddy or dedicated SHM block from remote peer.
    PeerShm {
        remote_pool: Arc<Mutex<RemotePool>>,  // for free_block + GC
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
```

**Python interface:**

| Method | Behavior |
|--------|----------|
| `__getbuffer__` | Returns read-only `Py_buffer` pointing at SHM/inline memory. For PeerShm: computes `base + offset`. For Handle: `pool.handle_slice()`. For Inline: `vec.as_ptr()`. |
| `release()` | Frees the underlying resource. PeerShm → `remote_pool.free_block()`. Handle → `pool.release_handle()`. Inline → drops Vec. Sets inner to None. |
| `__len__` | Returns data size. |
| `__bool__` | True if not yet released. |
| `is_inline` | Property. True for inline payloads. |

**Lifecycle:** Rust creates ShmBuffer → Python calls `memoryview(buf)` → reads data → `buf.release()` → Rust frees SHM.

**Safety:** `__getbuffer__` acquires the Mutex, checks inner is Some, and holds a reference. `release()` acquires the Mutex and replaces with None. The `frozen` pyclass + Mutex ensure thread safety under free-threading.

### 3.2 RemotePool (unified remote SHM mirror)

Replaces both `PeerShmState` (server mirrors client pool) and `ServerPoolState` (client mirrors server pool). Both are structurally identical: "a local view of a remote process's SHM pool."

```rust
pub struct RemotePool {
    pool: MemPool,          // opened in read-only (peer segments)
    prefix: String,         // SHM name prefix for lazy-open
    gc_state: GcState,      // per-segment idle tracking
}

struct GcState {
    buddy_idle_since: Vec<Option<Instant>>,  // per-segment idle timestamp
    gc_delay: Duration,                       // default 10s
}
```

**Unified behaviors:**

| Operation | Description |
|-----------|-------------|
| `ensure_segment(idx)` | Lazy-open buddy/dedicated segment by index (deterministic name) |
| `read_data(seg_idx, offset, len)` | Returns `(*const u8, len)` pointer into SHM — no copy |
| `free_block(seg_idx, offset, len, is_dedicated)` | Calls `pool.free_at()`, processes FreeResult |
| `gc_tick()` | Checks idle timestamps, unmaps segments past `gc_delay` |

**GC logic (identical for both directions):**
- On `FreeResult::SegmentIdle { seg_idx }`: record `buddy_idle_since[seg_idx] = now()`
- On `FreeResult::DedicatedFreed { seg_idx }`: schedule `gc_dedicated()` after delay
- On allocation into a segment: clear its idle timestamp
- `gc_tick()`: for each segment with `idle_since + gc_delay < now()`, call `pool.unmap_segment(seg_idx)`

### 3.3 FreeResult Enhancement

Current `FreeResult` only covers buddy. Add dedicated variant:

```rust
pub enum FreeResult {
    /// Block freed but segment still has allocations.
    Normal,
    /// Buddy segment is now fully idle — caller may schedule delayed GC.
    SegmentIdle { seg_idx: u16 },
    /// Dedicated segment freed — caller may schedule delayed GC.
    DedicatedFreed { seg_idx: u16 },
}
```

**Changes to `pool.rs`:**
- `free_at(is_dedicated=true)` → return `FreeResult::DedicatedFreed` instead of `FreeResult::Normal`
- `release_handle()` for dedicated variant → also return `DedicatedFreed`
- No change needed for FileSpill (Drop-based, no GC required)

### 3.4 CRM Callback Interface

The Rust→Python callback signature changes to pass metadata instead of payload bytes.

**Old:**
```rust
trait CrmCallback: Send + Sync {
    fn invoke(&self, route: &str, method_idx: u16, payload: &[u8]) -> Result<Vec<u8>, CrmError>;
}
```

**New:**
```rust
trait CrmCallback: Send + Sync {
    fn invoke(
        &self,
        route: &str,
        method_idx: u16,
        request_buffer: Py<ShmBuffer>,
        response_pool: Py<PyMemPool>,
    ) -> Result<ResponseMeta, CrmError>;
}
```

**ResponseMeta** (returned by Python dispatcher):
```rust
pub enum ResponseMeta {
    /// CRM wrote result into response SHM via pool.alloc() + pool.write().
    ShmAlloc {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
    /// Small result returned as inline bytes (< inline threshold).
    Inline(Vec<u8>),
    /// Method returned None / empty.
    Empty,
    /// CRM raised an error.
    Error(Vec<u8>),
}
```

**Python dispatcher flow:**
```python
def dispatch(route_name, method_idx, request_buffer, response_pool):
    mv = memoryview(request_buffer)
    args = deserialize(mv)
    result = crm_method(*args)
    mv.release()
    request_buffer.release()  # free peer SHM

    if result is None:
        return ResponseMeta.Empty

    data = serialize(result)
    if len(data) < INLINE_THRESHOLD:
        return ResponseMeta.Inline(data)

    handle = response_pool.alloc(len(data))
    response_pool.write_from_buffer(data, handle)
    return ResponseMeta.ShmAlloc(handle.seg_idx, handle.offset, len(data), handle.is_dedicated)
```

## 4. Data Flow by Frame Type

### 4.1 Inline (small payloads, < inline threshold)

```
Client → [UDS: inline frame with payload bytes] → Server
Server: ShmBuffer::Inline(vec) → memoryview → CRM → serialize → ResponseMeta::Inline(vec)
Server → [UDS: inline reply with result bytes] → Client
```

**Copies:** 0 SHM copies. Payload travels in UDS frame directly. Inline→Inline round trip has no SHM involvement.

### 4.2 Buddy (medium payloads, fits in buddy allocator)

```
Client: alloc buddy → write payload → send frame header (seg_idx, offset, size)
Server: parse header → ShmBuffer::PeerShm{seg_idx, offset, size}
        Python: memoryview(buf) → zero-copy read from client SHM
        → CRM method → serialize result
        → response_pool.alloc() → response_pool.write() (1 memcpy)
        → ResponseMeta::ShmAlloc{...}
        buf.release() → free client buddy block
Server: send reply header (seg_idx, offset, size) over UDS
Client: parse header → ShmBuffer::PeerShm → memoryview → zero-copy read
        → buf.release() → free server buddy block
```

**Copies:** 2 total (client write + server write into respective SHM pools).

### 4.3 Dedicated (large payloads, > buddy max block size)

Identical to buddy flow, except:
- `alloc()` returns a dedicated segment instead of buddy block
- `ShmBuffer::PeerShm { is_dedicated: true }`
- `release()` calls `free_dedicated()` → `FreeResult::DedicatedFreed` → delayed GC

**Copies:** 2 total (same as buddy).

### 4.4 Chunked / Streaming (SHM alloc failed, or relay mode)

```
Client: payload → chunk into multiple UDS frames → send sequentially
Server: receive chunks → reassemble into local MemHandle (reassembly_pool)
        → ShmBuffer::Handle{handle, pool}
        Python: memoryview(buf) → zero-copy read from reassembly pool
        → CRM method → serialize → response_pool.alloc + write
        → ResponseMeta::ShmAlloc{...}
        buf.release() → pool.release_handle(handle)
Server: send reply as buddy/dedicated frame (normal path)
        OR: if response_pool.alloc fails → chunk response into UDS frames
Client: receive chunked reply → reassemble → ShmBuffer::Handle → memoryview
```

**Copies:** 2 total for request+response data (chunks reassembled via `copy_nonoverlapping` into reassembly pool, then zero-copy read).

### 4.5 FileSpill (RAM scarce, alloc cascades to disk)

Transparent to the data flow. `MemPool.alloc()` returns `MemHandle::FileSpill` when RAM is scarce. ShmBuffer::Handle wraps it identically. The mmap is anonymous (unlink-on-create), so:
- Read/write via `handle_slice`/`handle_slice_mut` works identically
- `release()` drops MmapMut → munmap → OS reclaims
- No GC needed (no persistent resource)

## 5. Error Handling

### 5.1 SHM Allocation Failure

When `response_pool.alloc()` fails on the server side:

1. Python catches the alloc error
2. Returns `ResponseMeta::Inline(data)` for small results, or `ResponseMeta::Error` with a serialized error
3. Rust falls back to chunked UDS transfer for large payloads
4. Client receives the response via chunked path → ShmBuffer::Handle

This mirrors the existing degradation logic but is now explicit in ResponseMeta.

### 5.2 ShmBuffer Access After Release

`__getbuffer__` checks `inner.is_some()`. If already released, raises `BufferError("ShmBuffer already released")`.

### 5.3 CRM Method Exception

Python dispatcher catches exceptions, serializes the error, and returns `ResponseMeta::Error(serialized_bytes)`. Rust sends the error bytes as an inline error frame. No SHM involved in error path.

### 5.4 Segment Lazy-Open Failure

`RemotePool.ensure_segment(idx)` may fail if the peer's SHM segment doesn't exist yet. This is handled by:
- Retry with backoff (segment may be in-flight creation by peer)
- If persistent failure, return error to Python → error frame to client

## 6. GC Summary

| Resource | Trigger | Action | Delay |
|----------|---------|--------|-------|
| Buddy segment idle | `FreeResult::SegmentIdle` | Record `idle_since` → `gc_tick()` unmaps | 10s default |
| Dedicated segment freed | `FreeResult::DedicatedFreed` | Record `freed_at` → `gc_tick()` unmaps + `shm_unlink` | 10s default |
| FileSpill | Drop(MmapMut) | munmap → OS reclaims (file pre-unlinked) | Immediate |
| Inline Vec | Drop | Rust allocator frees | Immediate |

Both server-side `RemotePool` (mirrors client pool) and client-side `RemotePool` (mirrors server pool) use the **same** GC implementation.

## 7. Migration Strategy

### Phase 1: Rust Infrastructure (no Python changes)

- Add `FreeResult::DedicatedFreed` variant
- Implement `RemotePool` struct (replacing both `PeerShmState` and `ServerPoolState`)
- Implement `ShmBuffer` pyclass with `__getbuffer__` and `release()`
- Add `ResponseMeta` enum
- All behind feature flag or new API surface — existing paths unchanged

### Phase 2: Server-Side Integration

- Change `CrmCallback::invoke()` signature to pass `ShmBuffer` + `PyMemPool`
- Update `NativeServerBridge._make_dispatcher()` in Python to use new callback
- `dispatch_buddy_call` creates ShmBuffer instead of reading data
- `smart_reply_with_data` reads ResponseMeta instead of receiving bytes
- Remove copies C2, C3, C4, C5

### Phase 3: Client-Side Integration

- Replace `ServerPoolState` with `RemotePool`
- Replace `PyResponseBuffer` with `ShmBuffer`
- Client `call_buddy` writes directly from Python buffer → SHM (1 copy)
- Remove copies C0, C1
- Unified GC for client and server

### Phase 4: Cleanup

- Remove old `PeerShmState`, `ServerPoolState`, `PyResponseBuffer`
- Remove deprecated copy paths
- Update all tests

### Backward Compatibility

- `ShmBuffer` is a new type; old `PyResponseBuffer` remains until Phase 4
- CRM callback interface change is internal (no public API impact)
- Python user code (`@cc.icrm`, `@cc.transferable`, `cc.connect()`) unchanged

## 8. Performance Expectations

| Size | Current | Expected | Improvement |
|------|---------|----------|-------------|
| 64B | 0.20 ms | ~0.15 ms | Marginal (inline, no SHM) |
| 1 KB | 0.34 ms | ~0.25 ms | Marginal |
| 1 MB | 0.55 ms | ~0.35 ms | ~1.6× |
| 100 MB | 17 ms | ~10 ms | ~1.7× |
| 1 GB | 1320 ms | ~200-300 ms | **4-7×** |

The improvement scales with payload size because copy overhead is proportional to data volume. Small payloads (inline path) see minimal improvement since they don't involve SHM.

## 9. Glossary — Pool Roles

To avoid confusion, the two pool types on each side:

| Term | Owner | Contains | Used For |
|------|-------|----------|----------|
| **Local pool** | Own process's `MemPool` | Segments created by this process | Writing data that the peer will read |
| **Remote pool** | `RemotePool` wrapping peer's segments | Segments lazy-opened from peer | Reading data the peer wrote; freeing after read |

On the **server**, the local pool is `response_pool` (writes response data for client to read), and the remote pool mirrors the **client's** SHM (reads request data).

On the **client**, the local pool writes request data for the server to read, and the remote pool mirrors the **server's** SHM (reads response data).

`ShmBuffer::PeerShm` always references a `RemotePool`. `ShmBuffer::Handle` references a local `MemPool` (the reassembly pool). The `response_pool` passed to Python's dispatcher is the server's local `MemPool`.
