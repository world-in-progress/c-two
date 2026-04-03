# IPC Throughput Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce IPC round-trip copies from 4→2-3 (Part A) and accelerate remaining copies with adaptive parallel memcpy (Part B).

**Architecture:** Part A eliminates the server-side `bytes(memoryview)` forced copy by passing memoryview directly through the transferable deserialization chain, releasing the SHM buffer only after the response is written. Part B adds a rayon-based `adaptive_copy` function that parallelizes large memcpy operations when the system has spare capacity (inflight RPC count ≤ 1). Part C adds madvise hints for SHM page optimization.

**Tech Stack:** Python 3.14t (free-threading), Rust (PyO3/maturin), rayon (optional), POSIX SHM

**Spec:** `docs/superpowers/specs/2026-04-03-ipc-throughput-optimization-design.md`

---

## File Map

### Part A: Copy Elimination
| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/c_two/transport/server/native.py:312-361` | Dispatcher: pass memoryview, defer buffer release |
| Modify | `src/c_two/transport/server/reply.py:13-27` | Let memoryview pass through unpack_icrm_result |
| Verify | `src/c_two/crm/transferable.py:170-213` | Confirm deserializers accept memoryview (already done) |
| Verify | `src/c_two/crm/transferable.py:326-342` | Confirm client response path already uses memoryview |
| Test | `tests/unit/test_server_dispatch_memoryview.py` | New: memoryview passthrough tests |

### Part B: Adaptive Parallel memcpy
| Action | File | Responsibility |
|--------|------|----------------|
| Create | `src/c_two/_native/c2-mem/src/copy.rs` | adaptive_copy function with rayon |
| Modify | `src/c_two/_native/c2-mem/src/lib.rs` | Export copy module |
| Modify | `src/c_two/_native/c2-mem/Cargo.toml` | Add rayon optional dependency |
| Modify | `src/c_two/_native/c2-ipc/src/client.rs:36-54` | Add parallel_copy_threshold to IpcConfig |
| Modify | `src/c_two/_native/c2-ipc/src/sync_client.rs:92-107` | Use adaptive_copy + inflight counter |
| Modify | `src/c_two/_native/c2-server/src/server.rs:862-920` | Use adaptive_copy + inflight counter |
| Modify | `src/c_two/_native/c2-ffi/src/mem_ffi.rs:272-291` | Add write_adaptive with inflight param |
| Modify | `src/c_two/_native/c2-ffi/src/server_ffi.rs:31-85` | Add inflight counter to PyCrmCallback |
| Modify | `src/c_two/transport/registry.py:145-184` | Expose parallel_copy_threshold in set_ipc_config |
| Test | `tests/unit/test_adaptive_copy.py` | New: adaptive_copy behavior tests |

### Part C: Platform Optimizations
| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/c_two/_native/c2-mem/src/segment/shm.rs:54-61` | Add madvise after mmap |

---

### Task 1: Server Request Copy Elimination (native.py + reply.py)

**Files:**
- Modify: `src/c_two/transport/server/reply.py:13-27`
- Modify: `src/c_two/transport/server/native.py:312-361`
- Test: `tests/unit/test_reply.py` (existing), `tests/integration/test_server.py` (existing)

**Context:** Currently `native.py:319` does `payload = bytes(mv)` which copies the entire
request payload from SHM into a Python bytes object. Then `mv.release()` and
`request_buf.release()` free the SHM buffer. The CRM method receives bytes.

After this change: memoryview is passed directly to `method()` (the crm_to_com wrapper).
The transferable deserializer consumes the memoryview (pickle.loads or custom deserialize).
Buffer release is deferred to after `response_pool.write()` completes (important: for bytes
echo, the response write copies from request SHM → response SHM, so request SHM must stay
alive until then).

- [ ] **Step 1: Modify reply.py — let memoryview pass through**

Change `unpack_icrm_result` to not force `bytes()` on memoryview values. The caller
(native.py) will handle conversion when needed (inline response path only).

```python
# src/c_two/transport/server/reply.py — full function replacement
def unpack_icrm_result(result: Any) -> tuple[bytes | memoryview, bytes]:
    """Unpack ICRM ``'<-'`` result into ``(result_part, error_bytes)``.

    ``result_part`` may be bytes or memoryview — callers that need bytes
    must convert explicitly.  This avoids a forced copy when the result
    will be written directly to SHM.
    """
    if isinstance(result, tuple):
        err_part = result[0] if result[0] else b''
        res_part = result[1] if len(result) > 1 and result[1] else b''
        if isinstance(err_part, memoryview):
            err_part = bytes(err_part)
        # NOTE: res_part may be memoryview — intentionally NOT converted
        return res_part, err_part
    if result is None:
        return b'', b''
    # result may be bytes or memoryview — pass through as-is
    return result, b''
```

- [ ] **Step 2: Modify native.py dispatch — pass memoryview, defer release**

Replace the dispatch function body in `_make_dispatcher()`. Key changes:
1. Remove `bytes(mv)` — pass memoryview to `method()`
2. Move `mv.release()` and `request_buf.release()` to finally block at the end
3. For inline response path (small payloads), convert memoryview→bytes only there

```python
        # In _make_dispatcher(), replace the dispatch() closure (lines 312-361):
        def dispatch(
            _route_name: str, method_idx: int,
            request_buf: object, response_pool: object,
        ) -> object:
            mv = memoryview(request_buf)
            try:
                # 1. Resolve method
                method_name = idx_to_name.get(method_idx)
                if method_name is None:
                    raise RuntimeError(
                        f'Unknown method index {method_idx} for route {route_name}',
                    )
                entry = dispatch_table.get(method_name)
                if entry is None:
                    raise RuntimeError(f'Method not found: {method_name}')
                method, _access = entry

                # 2. Call CRM method — pass memoryview directly (zero-copy).
                #    The transferable deserializer consumes mv contents;
                #    mv remains valid because request_buf is not released
                #    until the finally block below.
                result = method(mv)

                # 3. Unpack result (res_part may be bytes or memoryview)
                res_part, err_part = unpack_icrm_result(result)
                if err_part:
                    raise CrmCallError(err_part)
                if not res_part:
                    return None

                # 4. Large responses → write to response pool SHM
                if response_pool is not None and len(res_part) > shm_threshold:
                    try:
                        alloc = response_pool.alloc(len(res_part))
                        response_pool.write(alloc, res_part)
                        seg_idx = int(alloc.seg_idx) & 0xFFFF
                        return (seg_idx, alloc.offset, len(res_part), alloc.is_dedicated)
                    except Exception:
                        pass  # SHM alloc failed — fall through to inline

                # 5. Inline response — must be bytes for Rust FFI
                if isinstance(res_part, memoryview):
                    return bytes(res_part)
                return res_part
            finally:
                mv.release()
                try:
                    request_buf.release()
                except Exception:
                    pass
```

- [ ] **Step 3: Run existing tests to verify no regression**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

Expected: All 503+ tests pass. The change is transparent — CRM methods receive
deserialized Python objects (not memoryview) because `crm_to_com` calls
`input_transferable.deserialize(mv)` which consumes the memoryview.

The bytes fast path is the exception: `deserialize(mv)` returns mv as-is, so the
CRM receives memoryview. For echo-type CRMs this works. For CRMs that need actual
bytes, `transferable.py:182-185` already returns data as-is — but if the CRM tries
to store it, they hold a reference to SHM that will be freed. This is acceptable:
CRM methods should not store input references beyond the method scope.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/server/native.py src/c_two/transport/server/reply.py
git commit -m "feat(server): zero-copy request dispatch via memoryview passthrough

- native.py: pass memoryview directly to crm_to_com instead of bytes(mv)
- reply.py: let memoryview pass through unpack_icrm_result (no forced bytes())
- Buffer release deferred to after response_pool.write (SHM stays valid)
- Inline response path still converts to bytes for Rust FFI
- Eliminates 1 data copy per request (server-side bytes materialization)"
```

---

### Task 2: Adaptive Copy Module in c2-mem (Rust)

**Files:**
- Create: `src/c_two/_native/c2-mem/src/copy.rs`
- Modify: `src/c_two/_native/c2-mem/src/lib.rs`
- Modify: `src/c_two/_native/c2-mem/Cargo.toml`

**Context:** All SHM writes currently use single-threaded `std::ptr::copy_nonoverlapping`.
This task adds an `adaptive_copy` function that uses rayon to parallelize large copies
when the system has spare capacity (inflight ≤ 1). rayon is an optional feature so the
module compiles without it.

- [ ] **Step 1: Add rayon dependency to c2-mem**

Edit `src/c_two/_native/c2-mem/Cargo.toml`:
```toml
[package]
name = "c2-mem"
edition.workspace = true
version.workspace = true

[features]
default = ["parallel-copy"]
parallel-copy = ["rayon"]

[dependencies]
libc = "0.2"
memmap2 = "0.9"
rayon = { version = "1.10", optional = true }
```

- [ ] **Step 2: Create copy.rs with adaptive_copy**

Create `src/c_two/_native/c2-mem/src/copy.rs`:
```rust
//! Adaptive parallel memory copy.
//!
//! Uses rayon to split large copies across threads when the system has
//! spare capacity (inflight RPC count ≤ 1).  Falls back to single-thread
//! `copy_nonoverlapping` when rayon is not available or the payload is
//! small or the system is busy.

use std::sync::atomic::{AtomicU32, Ordering};

/// Default threshold below which parallel copy is never used (8 MB).
pub const DEFAULT_PARALLEL_THRESHOLD: usize = 8 * 1024 * 1024;

/// Page size for chunk alignment (avoids false sharing at boundaries).
const PAGE_SIZE: usize = 4096;

/// Copy `len` bytes from `src` to `dst`, potentially using multiple threads.
///
/// # Decision logic
/// - `len < threshold` → single-thread (overhead not worth it)
/// - `inflight > 1` → single-thread (concurrent RPCs provide parallelism)
/// - Otherwise → rayon parallel copy with page-aligned chunks
///
/// # Safety
/// Same requirements as `std::ptr::copy_nonoverlapping`:
/// - `src` and `dst` must be valid for `len` bytes
/// - Regions must not overlap
/// - Both pointers must be properly aligned (byte-aligned is sufficient)
pub unsafe fn adaptive_copy(
    dst: *mut u8,
    src: *const u8,
    len: usize,
    inflight: &AtomicU32,
    threshold: usize,
) {
    if len < threshold || inflight.load(Ordering::Relaxed) > 1 {
        std::ptr::copy_nonoverlapping(src, dst, len);
        return;
    }

    parallel_copy(dst, src, len);
}

/// Single-threaded copy (always available).
#[inline(always)]
pub unsafe fn single_copy(dst: *mut u8, src: *const u8, len: usize) {
    std::ptr::copy_nonoverlapping(src, dst, len);
}

/// Multi-threaded copy via rayon.  Falls back to single-thread if rayon
/// feature is disabled.
#[cfg(feature = "parallel-copy")]
unsafe fn parallel_copy(dst: *mut u8, src: *const u8, len: usize) {
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // Use half the cores (capped at 8) to leave room for other work
    let max_threads = (ncpu / 2).max(2).min(8);

    // Page-aligned chunk size
    let raw_chunk = len / max_threads;
    let chunk_size = if raw_chunk < PAGE_SIZE {
        PAGE_SIZE
    } else {
        (raw_chunk + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    };

    // Pre-fault destination pages (advisory, non-fatal)
    #[cfg(unix)]
    {
        libc::madvise(dst as *mut libc::c_void, len, libc::MADV_WILLNEED);
    }

    rayon::scope(|s| {
        let mut offset = 0usize;
        while offset < len {
            let this_len = (len - offset).min(chunk_size);
            let d = dst;
            let sr = src;
            let o = offset;
            s.spawn(move |_| {
                std::ptr::copy_nonoverlapping(sr.add(o), d.add(o), this_len);
            });
            offset += this_len;
        }
    });
}

#[cfg(not(feature = "parallel-copy"))]
unsafe fn parallel_copy(dst: *mut u8, src: *const u8, len: usize) {
    // Fallback: no rayon available
    std::ptr::copy_nonoverlapping(src, dst, len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_single_copy_basic() {
        let src = vec![42u8; 1024];
        let mut dst = vec![0u8; 1024];
        unsafe { single_copy(dst.as_mut_ptr(), src.as_ptr(), 1024) };
        assert_eq!(src, dst);
    }

    #[test]
    fn test_adaptive_below_threshold() {
        let src = vec![99u8; 4096];
        let mut dst = vec![0u8; 4096];
        let inflight = AtomicU32::new(0);
        unsafe {
            adaptive_copy(
                dst.as_mut_ptr(), src.as_ptr(), 4096,
                &inflight, DEFAULT_PARALLEL_THRESHOLD,
            );
        }
        assert_eq!(src, dst);
    }

    #[test]
    fn test_adaptive_high_inflight_uses_single() {
        let size = 16 * 1024 * 1024; // 16MB > threshold
        let src = vec![7u8; size];
        let mut dst = vec![0u8; size];
        let inflight = AtomicU32::new(5); // busy
        unsafe {
            adaptive_copy(
                dst.as_mut_ptr(), src.as_ptr(), size,
                &inflight, DEFAULT_PARALLEL_THRESHOLD,
            );
        }
        assert_eq!(src, dst);
    }

    #[cfg(feature = "parallel-copy")]
    #[test]
    fn test_adaptive_parallel_large() {
        let size = 16 * 1024 * 1024; // 16MB
        let src: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let mut dst = vec![0u8; size];
        let inflight = AtomicU32::new(1); // single RPC → parallel path
        unsafe {
            adaptive_copy(
                dst.as_mut_ptr(), src.as_ptr(), size,
                &inflight, DEFAULT_PARALLEL_THRESHOLD,
            );
        }
        assert_eq!(src, dst);
    }
}
```

- [ ] **Step 3: Export copy module from lib.rs**

Add to `src/c_two/_native/c2-mem/src/lib.rs`:
```rust
pub mod copy;

pub use copy::{adaptive_copy, single_copy, DEFAULT_PARALLEL_THRESHOLD};
```

- [ ] **Step 4: Verify Rust compilation and tests**

```bash
cd src/c_two/_native && cargo test -p c2-mem --no-default-features
cd src/c_two/_native && cargo test -p c2-mem
```

Expected: All tests pass including the new copy module tests.

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/c2-mem/
git commit -m "feat(c2-mem): add adaptive parallel memcpy with rayon

- copy.rs: adaptive_copy() uses rayon when inflight ≤ 1 and payload > threshold
- Page-aligned chunks to avoid false sharing
- madvise(MADV_WILLNEED) pre-faults before parallel copy
- rayon is optional feature (default on), falls back to single-thread
- Unit tests for single, high-inflight, and parallel paths"
```

---

### Task 3: Inflight Counter + Adaptive Copy Integration (Rust)

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/server_ffi.rs:31-85`
- Modify: `src/c_two/_native/c2-server/src/server.rs:880-892`
- Modify: `src/c_two/_native/c2-ffi/src/mem_ffi.rs:272-291`
- Modify: `src/c_two/_native/c2-ipc/src/sync_client.rs:92-107`
- Modify: `src/c_two/transport/ipc/frame.py:15-54`
- Modify: `src/c_two/transport/registry.py:145-165,653-668`

**Context:** This task replaces all `copy_nonoverlapping` calls at the 3 SHM write sites
with `adaptive_copy` from Task 2. It adds an `Arc<AtomicU32>` inflight RPC counter to
PyCrmCallback and SyncClient. The counter increments before CRM invoke and decrements
after, so `adaptive_copy` knows when the system is busy. A `parallel_copy_threshold`
parameter is added to IPCConfig/set_ipc_config (default 8MB).

- [ ] **Step 1: Add inflight counter to PyCrmCallback (server_ffi.rs)**

The inflight counter is owned by the server and shared via `Arc`. When invoke() is
called, increment before calling Python, decrement in Drop guard after.

```rust
// server_ffi.rs — modify PyCrmCallback struct and invoke()
use std::sync::atomic::{AtomicU32, Ordering};

struct PyCrmCallback {
    py_callable: PyObject,
    response_pool_obj: PyObject,
    inflight: Arc<AtomicU32>,
    parallel_threshold: usize,
}

// ... Send/Sync impls unchanged ...

impl CrmCallback for PyCrmCallback {
    fn invoke(
        &self,
        route_name: &str,
        method_idx: u16,
        request: RequestData,
        _response_pool: Arc<std::sync::RwLock<MemPool>>,
    ) -> Result<ResponseMeta, CrmError> {
        // RAII guard for inflight count
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let _guard = InflightGuard(&self.inflight);

        Python::with_gil(|py| {
            // ... existing shm_buf + call logic unchanged ...
            // (see current code — no changes needed inside the GIL block)
        })
    }
}

struct InflightGuard<'a>(&'a AtomicU32);
impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
```

Also update the `register_route` PyO3 function that creates PyCrmCallback to pass
the inflight counter and threshold. Find where `PyCrmCallback` is constructed (in the
same file) and add:
```rust
// In register_route() — where PyCrmCallback is created
let inflight = Arc::new(AtomicU32::new(0));
let callback = PyCrmCallback {
    py_callable: dispatcher.into(),
    response_pool_obj: response_pool.into(),
    inflight,
    parallel_threshold: c2_mem::copy::DEFAULT_PARALLEL_THRESHOLD,
};
```

- [ ] **Step 2: Replace copy_nonoverlapping in server.rs**

In `write_buddy_reply_with_data`, the server writes CRM output to response SHM.
Import and use `adaptive_copy` instead of `copy_nonoverlapping`. The inflight counter
must be passed through the `CrmCallback` trait into this function.

First, modify the CrmCallback trait (in `c2-server/src/server.rs` or its trait file)
to expose inflight and threshold. Since PyCrmCallback is in c2-ffi and the copy
happens in c2-server, add methods to CrmCallback trait:

```rust
// In c2-server's CrmCallback trait (or add new trait):
pub trait CrmCallback: Send + Sync {
    fn invoke(...) -> Result<ResponseMeta, CrmError>;
    fn inflight_counter(&self) -> &AtomicU32;
    fn parallel_threshold(&self) -> usize;
}
```

Then in `write_buddy_reply_with_data`:
```rust
// server.rs — replace the copy block
use c2_mem::copy::adaptive_copy;

// 2. Write data to SHM (single lock scope).
let write_ok = {
    let pool = response_pool.read().unwrap();
    match pool.data_ptr(&alloc) {
        Ok(ptr) => {
            unsafe {
                adaptive_copy(
                    ptr, data.as_ptr(), data.len(),
                    callback.inflight_counter(),
                    callback.parallel_threshold(),
                );
            }
            true
        }
        Err(_) => false,
    }
};
```

- [ ] **Step 3: Replace copy_nonoverlapping in sync_client.rs**

Add inflight counter and threshold fields to `SyncClient`:

```rust
// sync_client.rs — add to SyncClient struct
pub struct SyncClient {
    inner: Arc<SyncClientInner>,
    inflight: Arc<AtomicU32>,
    parallel_threshold: usize,
}
```

Update `pool_alloc_and_write`:
```rust
pub fn pool_alloc_and_write(&self, data: &[u8]) -> Result<PoolAllocation, IpcError> {
    let pool_arc = self.inner.pool.as_ref()
        .ok_or_else(|| IpcError::Pool("no client pool".into()))?;
    let mut pool = pool_arc.lock().unwrap();
    let alloc = pool.alloc(data.len())
        .map_err(|e| IpcError::Pool(format!("alloc failed: {e}")))?;
    let ptr = pool.data_ptr(&alloc)
        .map_err(|e| {
            let _ = pool.free(&alloc);
            IpcError::Pool(format!("data_ptr failed: {e}"))
        })?;
    unsafe {
        c2_mem::copy::adaptive_copy(
            ptr, data.as_ptr(), data.len(),
            &self.inflight, self.parallel_threshold,
        );
    }
    Ok(alloc)
}
```

Wrap call/call_prealloc with inflight increment/decrement:
```rust
pub fn call(&self, data: &[u8]) -> Result<Vec<u8>, IpcError> {
    self.inflight.fetch_add(1, Ordering::Relaxed);
    let result = self.call_inner(data);
    self.inflight.fetch_sub(1, Ordering::Relaxed);
    result
}
```

- [ ] **Step 4: Add write_adaptive to MemPool FFI (mem_ffi.rs)**

The Python-side `response_pool.write(alloc, res_part)` goes through mem_ffi.rs.
Add the inflight counter and threshold as parameters:

```rust
// mem_ffi.rs — add new method alongside existing write()
/// Write with adaptive parallel copy (for hot-path server response).
fn write_adaptive(
    &self,
    alloc: &PyPoolAlloc,
    data: &[u8],
    inflight: u32,
    threshold: usize,
) -> PyResult<()> {
    let pool = self.pool.read().map_err(|e| {
        PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
    })?;
    let pa = PoolAllocation {
        seg_idx: alloc.seg_idx,
        offset: alloc.offset,
        actual_size: alloc.actual_size,
        level: alloc.level,
        is_dedicated: alloc.is_dedicated,
    };
    let ptr = pool.data_ptr(&pa).map_err(|e| PyRuntimeError::new_err(e))?;
    if data.len() > alloc.actual_size as usize {
        return Err(PyValueError::new_err("data exceeds allocation size"));
    }
    // Use a temporary AtomicU32 with the caller-provided value
    let counter = std::sync::atomic::AtomicU32::new(inflight);
    unsafe {
        c2_mem::copy::adaptive_copy(ptr, data.as_ptr(), data.len(), &counter, threshold);
    }
    Ok(())
}
```

- [ ] **Step 5: Add parallel_copy_threshold to IPCConfig**

```python
# frame.py — add field to IPCConfig dataclass
    parallel_copy_threshold: int = 8 * 1024 * 1024  # 8 MB
```

- [ ] **Step 6: Expose parallel_copy_threshold in set_ipc_config**

```python
# registry.py — update _ProcessRegistry.set_ipc_config()
def set_ipc_config(
    self,
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    parallel_copy_threshold: int | None = None,
) -> None:
    # ... existing body ...
    if parallel_copy_threshold is not None:
        self._ipc_config.parallel_copy_threshold = parallel_copy_threshold

# Also update module-level set_ipc_config():
def set_ipc_config(
    *,
    segment_size: int | None = None,
    max_segments: int | None = None,
    parallel_copy_threshold: int | None = None,
) -> None:
    _ProcessRegistry.get().set_ipc_config(
        segment_size=segment_size,
        max_segments=max_segments,
        parallel_copy_threshold=parallel_copy_threshold,
    )
```

- [ ] **Step 7: Rebuild and run tests**

```bash
cd src/c_two/_native && cargo check -p c2-mem -p c2-ipc -p c2-server -p c2-ffi
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

Expected: Rust compiles, all Python tests pass. Functional behavior unchanged —
adaptive_copy with inflight=0 and <8MB thresholds behaves identically to
copy_nonoverlapping for existing tests.

- [ ] **Step 8: Commit**

```bash
git add src/c_two/_native/ src/c_two/transport/
git commit -m "feat(transport): integrate adaptive parallel memcpy at all copy sites

- server_ffi.rs: add Arc<AtomicU32> inflight counter to PyCrmCallback
- server.rs: use adaptive_copy in write_buddy_reply_with_data
- sync_client.rs: use adaptive_copy in pool_alloc_and_write, track inflight
- mem_ffi.rs: add write_adaptive() with inflight/threshold params
- frame.py: add parallel_copy_threshold to IPCConfig (default 8MB)
- registry.py: expose parallel_copy_threshold in set_ipc_config()"
```

---

### Task 4: madvise Hints on SHM mmap (Part C)

**Files:**
- Modify: `src/c_two/_native/c2-mem/src/segment/shm.rs:54-61,114-121`

**Context:** After `mmap()`, add `MADV_SEQUENTIAL` hints to tell the kernel the SHM pages
will be accessed sequentially. This enables read-ahead prefetching and reduces TLB misses
for large payloads.

- [ ] **Step 1: Add madvise after owner mmap (create)**

After the mmap success check in `ShmRegion::create()` (~line 69), add:
```rust
// After: if ptr == libc::MAP_FAILED { ... }
// Before: Ok(ShmRegion { ... })
libc::madvise(ptr, size, libc::MADV_SEQUENTIAL);
```

- [ ] **Step 2: Add madvise after non-owner mmap (open)**

After the mmap success check in `ShmRegion::open()` (~line 129), add:
```rust
libc::madvise(ptr, map_size, libc::MADV_SEQUENTIAL);
```

- [ ] **Step 3: Rebuild and run tests**

```bash
cd src/c_two/_native && cargo check -p c2-mem
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

Expected: Builds clean, all tests pass. madvise is advisory-only — failures are
silently ignored by the kernel.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-mem/src/segment/shm.rs
git commit -m "perf(c2-mem): add MADV_SEQUENTIAL madvise hint after SHM mmap

Advisory hint enables kernel read-ahead for sequential SHM access patterns.
Applied to both owner (create) and non-owner (open) mmap paths."
```

---

### Task 5: Benchmark (64B–1GB, Before/After)

**Files:**
- Create: `benchmarks/throughput_benchmark.py`
- Existing reference: `benchmarks/segment_size_benchmark.py`

**Context:** Run the same CRM echo benchmark from the prior session
(`segment_size_benchmark.py`), comparing baseline (before Task 1 changes) vs
optimized (after all Tasks 1-4). The benchmark measures IPC round-trip latency
for bytes and dict payloads across sizes 64B to 1GB.

Since we cannot revert to baseline in the same run, we capture the "after" numbers
and compare them against the known baseline from the prior benchmark report at
`docs/superpowers/reports/2026-04-03-bidirectional-zero-copy-benchmark.md`.

- [ ] **Step 1: Create throughput benchmark script**

Create `benchmarks/throughput_benchmark.py` that:
1. Tests payload sizes: 64B, 1KB, 64KB, 1MB, 16MB, 64MB, 256MB, 500MB, 1GB
2. Tests both bytes and dict payloads
3. Tests with 256MB segment_size (uses dedicated for >256MB) and 2GB segment_size
4. Reports: mean latency, throughput (GB/s), copy count
5. Outputs TSV file for comparison with prior results

```python
"""IPC throughput benchmark — measures round-trip latency for echo CRM.

Usage:
    uv run python benchmarks/throughput_benchmark.py

Writes results to benchmarks/throughput_results.tsv.
"""

from __future__ import annotations

import gc
import os
import pickle
import sys
import time

import c_two as cc
from tests.fixtures.hello_icrm import IHello


class Echo:
    """Minimal echo CRM — returns input unchanged."""

    def hello(self, data: bytes) -> bytes:
        return data

    def hello_data(self, data):
        return data


SIZES = [
    ('64B', 64),
    ('1KB', 1024),
    ('64KB', 64 * 1024),
    ('1MB', 1 << 20),
    ('16MB', 16 << 20),
    ('64MB', 64 << 20),
    ('256MB', 256 << 20),
    ('500MB', 500 << 20),
    ('1GB', 1 << 30),
]

WARMUP = 2
ITERS = 5


def make_dict_payload(size: int) -> dict:
    """Create a dict that pickles to approximately `size` bytes."""
    chunk = b'x' * min(size, 1 << 20)
    n = max(1, size // len(chunk))
    return {'chunks': [chunk] * n}


def bench_echo(proxy, payload, iters: int) -> float:
    """Returns mean round-trip seconds."""
    times = []
    for _ in range(iters):
        gc.disable()
        t0 = time.perf_counter()
        proxy.hello(payload)
        t1 = time.perf_counter()
        gc.enable()
        times.append(t1 - t0)
    return sum(times) / len(times)


def bench_echo_dict(proxy, payload, iters: int) -> float:
    times = []
    for _ in range(iters):
        gc.disable()
        t0 = time.perf_counter()
        proxy.hello_data(payload)
        t1 = time.perf_counter()
        gc.enable()
        times.append(t1 - t0)
    return sum(times) / len(times)


def run_benchmark(segment_size: int) -> list[dict]:
    """Run full benchmark suite with given segment_size. Returns list of result dicts."""
    results = []
    seg_label = f'{segment_size // (1 << 20)}MB'

    os.environ.pop('C2_IPC_ADDRESS', None)
    os.environ.pop('C2_RELAY_ADDRESS', None)

    cc.set_ipc_config(segment_size=segment_size, max_segments=4)
    cc.set_address(f'ipc:///tmp/c2_bench_{seg_label}_{os.getpid()}')
    cc.register(IHello, Echo(), name='echo')

    proxy = cc.connect(IHello, name='echo', address=cc.get_address())

    try:
        for label, size in SIZES:
            if size > 4 * segment_size:
                continue  # skip impossibly large for this config

            # --- bytes echo ---
            payload = b'\x42' * size
            for _ in range(WARMUP):
                proxy.hello(payload)
            mean_s = bench_echo(proxy, payload, ITERS)
            throughput = (size / (1 << 30)) / mean_s if mean_s > 0 else 0
            results.append({
                'segment': seg_label,
                'size': label,
                'type': 'bytes',
                'latency_ms': f'{mean_s * 1000:.2f}',
                'throughput_gbps': f'{throughput:.3f}',
            })
            print(f'  {seg_label} {label:>6s} bytes  {mean_s*1000:8.2f} ms  {throughput:.3f} GB/s')
            del payload
            gc.collect()

            # --- dict echo ---
            d = make_dict_payload(size)
            for _ in range(WARMUP):
                proxy.hello_data(d)
            mean_s = bench_echo_dict(proxy, d, ITERS)
            throughput = (size / (1 << 30)) / mean_s if mean_s > 0 else 0
            results.append({
                'segment': seg_label,
                'size': label,
                'type': 'dict',
                'latency_ms': f'{mean_s * 1000:.2f}',
                'throughput_gbps': f'{throughput:.3f}',
            })
            print(f'  {seg_label} {label:>6s} dict   {mean_s*1000:8.2f} ms  {throughput:.3f} GB/s')
            del d
            gc.collect()

    finally:
        cc.close(proxy)
        cc.unregister('echo')
        cc.shutdown()

    return results


def main():
    print('=== IPC Throughput Benchmark ===\n')
    all_results = []

    print('--- Segment size: 256 MB ---')
    all_results.extend(run_benchmark(256 << 20))

    print('\n--- Segment size: 2 GB ---')
    all_results.extend(run_benchmark(2 << 30))

    # Write TSV
    tsv_path = 'benchmarks/throughput_results.tsv'
    with open(tsv_path, 'w') as f:
        f.write('segment\tsize\ttype\tlatency_ms\tthroughput_gbps\n')
        for r in all_results:
            f.write(f'{r["segment"]}\t{r["size"]}\t{r["type"]}\t'
                    f'{r["latency_ms"]}\t{r["throughput_gbps"]}\n')

    print(f'\nResults written to {tsv_path}')


if __name__ == '__main__':
    main()
```

- [ ] **Step 2: Run the benchmark**

```bash
uv sync --reinstall-package c-two
uv run python benchmarks/throughput_benchmark.py
```

Expected: Completes in ~5-10 minutes. Results show improvement at ≥16MB payloads
compared to prior baseline. Small payloads (<64KB) should be within noise.

- [ ] **Step 3: Commit benchmark script + results**

```bash
git add benchmarks/throughput_benchmark.py benchmarks/throughput_results.tsv
git commit -m "bench: add IPC throughput benchmark for optimization comparison"
```

---

### Task 6: Analysis Report

**Files:**
- Create: `docs/superpowers/reports/2026-04-03-ipc-throughput-optimization-report.md`

**Context:** Compare benchmark results from Task 5 against the baseline numbers from
`docs/superpowers/reports/2026-04-03-bidirectional-zero-copy-benchmark.md`. Analyze:
1. Copy count reduction (4→2-3)
2. Latency improvement per payload size
3. Memory pressure reduction (qualitative — no intermediate bytes allocation)
4. Part B (adaptive copy) effect — visible at ≥64MB on multi-core
5. Recommendations for further optimization

- [ ] **Step 1: Write report (split into multiple writes)**

The report should include:
- Executive summary (1 paragraph)
- Methodology section (benchmark config, hardware, Python version)
- Results table (side-by-side baseline vs optimized)
- Analysis per payload size band (small/medium/large)
- Copy flow diagrams (before/after)
- Conclusion + next steps

Use the actual benchmark numbers from Task 5 results.

- [ ] **Step 2: Commit report**

```bash
git add docs/superpowers/reports/
git commit -m "docs: add IPC throughput optimization benchmark report

Compares 4-copy baseline vs 2-3 copy optimized flow.
Part A: memoryview passthrough eliminates server-side bytes materialization.
Part B: adaptive parallel memcpy accelerates remaining copies for large payloads."
```
