# IPC Defect Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 5 IPC defects (gc_buddy auto-trigger, response stack buffer, SHM degradation, ResponseBuffer zero-copy, chunked response) identified in the implementation audit.

**Architecture:** Three-phase approach ordered by risk. Phase 1 (low risk): Rust-only changes in c2-mem and c2-server for buddy GC auto-trigger and response stack buffer. Phase 2 (medium risk): Rust+Python changes for SHM failure degradation and ResponseBuffer zero-copy. Phase 3 (high risk): Wire protocol extension for chunked response path.

**Tech Stack:** Rust (c2-mem, c2-wire, c2-ipc, c2-server, c2-ffi), Python (transferable.py, proxy.py), PyO3, maturin, pytest

**Design Spec:** `docs/superpowers/specs/2026-04-02-ipc-defect-fix-design.md`

**Build/Test Commands:**
```bash
# Rust type-check (fast iteration)
cd src/c_two/_native && cargo check -p c2-mem -p c2-server -p c2-ipc -p c2-wire

# Rust unit tests (pure Rust, no Python)
cd src/c_two/_native && cargo test -p c2-mem -p c2-wire --no-default-features

# Force rebuild Python extension after Rust changes
uv sync --reinstall-package c-two

# Full Python test suite
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30

# Single test file
C2_RELAY_ADDRESS= uv run pytest tests/integration/test_ipc_buddy_reply.py -q
```

---

## File Map

### Phase 1 — Modify
| File | Responsibility |
|------|---------------|
| `src/c_two/_native/c2-mem/src/pool.rs` | FreeResult enum, alloc_buddy gc-before-expand, free_at return FreeResult |
| `src/c_two/_native/c2-server/src/server.rs` | write_reply_with_data stack buffer, dispatch FreeResult |
| `src/c_two/_native/c2-server/src/connection.rs` | free_peer_block return FreeResult |
| `src/c_two/_native/c2-ipc/src/client.rs` | read_and_free return FreeResult |
| `src/c_two/_native/c2-ffi/src/mem_ffi.rs` | free_at FFI adapt to FreeResult |
| `tests/integration/test_ipc_buddy_reply.py` | GC integration tests |

### Phase 2 — Create + Modify
| File | Responsibility |
|------|---------------|
| `src/c_two/_native/c2-ipc/src/response.rs` | **NEW** — ResponseData enum |
| `src/c_two/_native/c2-ipc/src/client.rs` | decode_response → ResponseData, call_full → ResponseData |
| `src/c_two/_native/c2-ipc/src/sync_client.rs` | call() → ResponseData |
| `src/c_two/_native/c2-server/src/server.rs` | write_buddy_reply → Result, smart_reply degradation |
| `src/c_two/_native/c2-ffi/src/client_ffi.rs` | PyResponseBuffer pyclass, call() returns ResponseBuffer |
| `src/c_two/crm/transferable.py` | @auto_transfer memoryview + release |
| `src/c_two/transport/client/proxy.py` | call() return type |

### Phase 3 — Modify
| File | Responsibility |
|------|---------------|
| `src/c_two/_native/c2-wire/src/chunk.rs` | reply_chunk_meta encode/decode |
| `src/c_two/_native/c2-server/src/server.rs` | write_chunked_reply() |
| `src/c_two/_native/c2-ipc/src/client.rs` | chunked response reception + assembler |
| `src/c_two/_native/c2-ipc/src/response.rs` | ResponseData::Handle variant |
| `src/c_two/_native/c2-ffi/src/client_ffi.rs` | ResponseBufferInner::Handle variant |

---

## Phase 1: Memory Lifecycle + Small Optimization

### Task 1: FreeResult enum + free_at returns FreeResult

**Files:**
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:387-411`

- [ ] **Step 1: Define FreeResult enum**

Add `FreeResult` enum at the top of `pool.rs` (after line 15, before `DedicatedEntry`):

```rust
/// Result of a free operation — signals whether the segment became fully idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeResult {
    /// Block freed normally, segment still has active allocations.
    Normal,
    /// Block freed and the segment is now completely idle (zero active allocations).
    SegmentIdle { seg_idx: u16 },
}
```

- [ ] **Step 2: Change free_at() return type to FreeResult**

In `pool.rs`, modify `free_at()` (line 387) to return `Result<FreeResult, String>`:

```rust
pub fn free_at(&mut self, seg_idx: u32, offset: u32, data_size: u32, is_dedicated: bool) -> Result<FreeResult, String> {
    if is_dedicated {
        self.free_dedicated(seg_idx);
        Ok(FreeResult::Normal)
    } else if let Some(seg) = self.segments.get(seg_idx as usize) {
        let actual_size = (data_size as usize)
            .next_power_of_two()
            .max(self.config.min_block_size);
        if let Some(level) = seg.allocator().size_to_level(actual_size) {
            seg.allocator().free(offset, level as u16)?;
            let idx = seg_idx as usize;
            if seg.allocator().alloc_count() == 0 {
                if idx < self.idle_since.len() && self.idle_since[idx].is_none() {
                    self.idle_since[idx] = Some(Instant::now());
                }
                return Ok(FreeResult::SegmentIdle { seg_idx: seg_idx as u16 });
            }
            Ok(FreeResult::Normal)
        } else {
            Err("could not determine buddy level for free_at".into())
        }
    } else {
        Err(format!("invalid segment index {}", seg_idx))
    }
}
```

- [ ] **Step 3: Update internal callers of free_at**

Search for all `free_at` calls within `pool.rs` itself (line ~559). These call `self.free_at(...)` and discard the result — change from `let _ = self.free_at(...)` to keep discarding with the new return type (no behavior change needed since they're internal cleanup).

- [ ] **Step 4: Export FreeResult from c2-mem**

In `src/c_two/_native/c2-mem/src/lib.rs`, add `FreeResult` to the `pub use pool::...` re-export.

- [ ] **Step 5: Cargo check**

Run: `cd src/c_two/_native && cargo check -p c2-mem`
Expected: Compiles with warnings about unused FreeResult variants (callers not yet updated)

- [ ] **Step 6: Commit**

```bash
git add src/c_two/_native/c2-mem/
git commit -m "feat(c2-mem): add FreeResult enum, free_at returns idle signal

free_at() now returns FreeResult::SegmentIdle when a buddy segment
becomes fully idle after freeing, enabling callers to trigger delayed
GC. Part of defect A fix.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: alloc_buddy gc-before-expand + alloc_handle gc

**Files:**
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:587-628` (alloc_buddy)
- Modify: `src/c_two/_native/c2-mem/src/pool.rs:461-511` (alloc_handle)

- [ ] **Step 1: Add gc_buddy + retry in alloc_buddy**

In `alloc_buddy()` (line 587), insert a gc+retry block between Layer 1 (line 607) and Layer 2 (line 609):

```rust
fn alloc_buddy(&mut self, size: usize) -> Result<PoolAllocation, String> {
    // Layer 1: Try existing segments (unchanged, lines 590-607)
    for (idx, seg) in self.segments.iter().enumerate() {
        if (seg.allocator().free_bytes() as usize) < size {
            continue;
        }
        if let Some(a) = seg.allocator().alloc(size) {
            if idx < self.idle_since.len() {
                self.idle_since[idx] = None;
            }
            return Ok(PoolAllocation {
                seg_idx: idx as u32,
                offset: a.offset,
                actual_size: a.actual_size,
                level: a.level,
                is_dedicated: false,
            });
        }
    }

    // Layer 1.5 (NEW): GC before expansion — reclaim idle trailing segments
    let reclaimed = self.gc_buddy();
    if reclaimed > 0 {
        for (idx, seg) in self.segments.iter().enumerate() {
            if (seg.allocator().free_bytes() as usize) < size {
                continue;
            }
            if let Some(a) = seg.allocator().alloc(size) {
                if idx < self.idle_since.len() {
                    self.idle_since[idx] = None;
                }
                return Ok(PoolAllocation {
                    seg_idx: idx as u32,
                    offset: a.offset,
                    actual_size: a.actual_size,
                    level: a.level,
                    is_dedicated: false,
                });
            }
        }
    }

    // Layer 2: Create new segment (unchanged)
    if self.segments.len() < self.config.max_segments {
        let seg = self.create_segment()?;
        let idx = self.segments.len();
        self.segments.push(seg);
        self.idle_since.push(None);
        if let Some(a) = self.segments[idx].allocator().alloc(size) {
            return Ok(PoolAllocation {
                seg_idx: idx as u32,
                offset: a.offset,
                actual_size: a.actual_size,
                level: a.level,
                is_dedicated: false,
            });
        }
    }

    // Layer 3: Dedicated segment fallback.
    self.alloc_dedicated(size)
}
```

- [ ] **Step 2: Add gc_buddy + retry in alloc_handle buddy expansion**

In `alloc_handle()` (line 486-502), insert gc_buddy before create_segment:

```rust
    // RAM fine: try buddy expansion.
    if max_buddy > 0 && size <= max_buddy
        && self.segments.len() < self.config.max_segments
    {
        // NEW: GC before expanding — may free trailing idle segments
        let reclaimed = self.gc_buddy();
        if reclaimed > 0 {
            // Retry existing segments after GC
            for (idx, seg) in self.segments.iter().enumerate() {
                if (seg.allocator().free_bytes() as usize) < size { continue; }
                if let Some(a) = seg.allocator().alloc(size) {
                    if idx < self.idle_since.len() { self.idle_since[idx] = None; }
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16, offset: a.offset, len: size,
                    });
                }
            }
        }

        match self.create_segment() {
            Ok(seg) => {
                let idx = self.segments.len();
                self.segments.push(seg);
                self.idle_since.push(None);
                if let Some(a) = self.segments[idx].allocator().alloc(size) {
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16, offset: a.offset, len: size,
                    });
                }
            }
            Err(_) => return self.alloc_file_spill(size),
        }
    }
```

- [ ] **Step 3: Cargo check + test**

Run: `cd src/c_two/_native && cargo check -p c2-mem && cargo test -p c2-mem --no-default-features`
Expected: Compiles and existing tests pass

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-mem/
git commit -m "feat(c2-mem): gc_buddy before buddy expansion in alloc paths

alloc_buddy() and alloc_handle() now call gc_buddy() to reclaim idle
trailing segments before creating new ones. This prevents unbounded
segment growth when allocation patterns are bursty.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 3: Propagate FreeResult through server + client callers

**Files:**
- Modify: `src/c_two/_native/c2-server/src/connection.rs:223-255`
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:86-109`
- Modify: `src/c_two/_native/c2-ffi/src/mem_ffi.rs:393-407`
- Modify: `src/c_two/_native/c2-server/src/server.rs` (dispatch_buddy_call)

- [ ] **Step 1: connection.rs — free_peer_block returns FreeResult**

Import `FreeResult` from `c2_mem`. Change `free_peer_block()` return type from `()` to `FreeResult`:

```rust
use c2_mem::FreeResult;

pub fn free_peer_block(
    &self, seg_idx: u16, offset: u32, data_size: u32, is_dedicated: bool,
) -> FreeResult {
    let mut state = self.peer_shm.lock().unwrap();
    let lazy_res = if is_dedicated {
        state.ensure_dedicated_segment(seg_idx as u32, data_size as usize)
    } else {
        state.ensure_buddy_segment(seg_idx as u32)
    };
    if let Err(e) = lazy_res {
        warn!(conn_id = self.conn_id, seg_idx, "lazy-open for free failed: {e}");
        return FreeResult::Normal;
    }
    if let Some(pool) = state.pool.as_mut() {
        match pool.free_at(seg_idx as u32, offset, data_size, is_dedicated) {
            Ok(result) => result,
            Err(e) => {
                warn!(conn_id = self.conn_id, seg_idx, offset, "peer free_at failed: {e}");
                FreeResult::Normal
            }
        }
    } else {
        FreeResult::Normal
    }
}
```

- [ ] **Step 2: client.rs — read_and_free returns FreeResult**

In `ServerPoolState::read_and_free()`, change return to `Result<(Vec<u8>, FreeResult), String>`. Update the free_at call to capture FreeResult:

```rust
use c2_mem::FreeResult;

// In read_and_free, replace the free_at call:
let free_result = match self.pool.free_at(seg_idx as u32, offset, data_size, is_dedicated) {
    Ok(fr) => fr,
    Err(e) => {
        eprintln!("Warning: server SHM free_at failed: {e}");
        FreeResult::Normal
    }
};
Ok((data, free_result))
```

Update `decode_response()` buddy branch to discard FreeResult for now:

```rust
state.read_and_free(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated)
    .map(|(data, _free_result)| data)
    .map_err(|e| IpcError::Handshake(format!("buddy read: {e}")))
```

- [ ] **Step 3: mem_ffi.rs — FFI discards FreeResult**

In `free_at` FFI wrapper, map FreeResult to `()`:

```rust
pool.free_at(seg_idx, offset, data_size, is_dedicated)
    .map(|_| ())
    .map_err(|e| PyRuntimeError::new_err(e))?;
```

- [ ] **Step 4: server.rs — delayed gc on SegmentIdle**

In `dispatch_buddy_call()`, capture the FreeResult from `conn.free_peer_block()`. On `SegmentIdle`, spawn delayed gc:

```rust
let free_result = conn.free_peer_block(bp.seg_idx, bp.offset, bp.data_size, bp.is_dedicated);
if let FreeResult::SegmentIdle { .. } = free_result {
    let peer_shm = conn.peer_shm_arc();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let mut state = peer_shm.lock().unwrap();
        if let Some(pool) = state.pool.as_mut() {
            pool.gc_buddy();
        }
    });
}
```

Note: If `Connection` doesn't expose `peer_shm` as an Arc, add accessor `pub fn peer_shm_arc(&self) -> Arc<Mutex<PeerShmState>>`.

- [ ] **Step 5: Cargo check all crates**

Run: `cd src/c_two/_native && cargo check -p c2-mem -p c2-server -p c2-ipc`
Expected: Clean compilation

- [ ] **Step 6: Commit**

```bash
git add src/c_two/_native/
git commit -m "feat: propagate FreeResult, delayed gc on segment idle

connection.free_peer_block() and client.read_and_free() now return
FreeResult. Server spawns delayed gc_buddy() on SegmentIdle signal.
FFI layer maps FreeResult away (Python unaffected).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 4: Response inline stack buffer (Defect E)

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:788-795`

- [ ] **Step 1: Rewrite write_reply_with_data with stack buffer**

Replace current `write_reply_with_data()` with stack-buffer optimization. Mirror `call_inline()` pattern from `client.rs:493-515`:

```rust
async fn write_reply_with_data(writer: &Arc<Mutex<OwnedWriteHalf>>, request_id: u64, data: &[u8]) {
    let ctrl_bytes = encode_reply_control(&ReplyControl::Success);
    let payload_len = ctrl_bytes.len() + data.len();
    let total_len = (12 + payload_len) as u32;
    let frame_size = frame::HEADER_SIZE + payload_len;

    if frame_size <= 1024 {
        let mut buf = [0u8; 1024];
        buf[0..4].copy_from_slice(&total_len.to_le_bytes());
        buf[4..12].copy_from_slice(&request_id.to_le_bytes());
        buf[12..16].copy_from_slice(&REPLY_FLAGS.to_le_bytes());
        let mut off = frame::HEADER_SIZE;
        buf[off..off + ctrl_bytes.len()].copy_from_slice(&ctrl_bytes);
        off += ctrl_bytes.len();
        buf[off..off + data.len()].copy_from_slice(data);
        off += data.len();
        let _ = writer.lock().await.write_all(&buf[..off]).await;
    } else {
        let mut payload = Vec::with_capacity(ctrl_bytes.len() + data.len());
        payload.extend_from_slice(&ctrl_bytes);
        payload.extend_from_slice(data);
        let frame = encode_frame(request_id, REPLY_FLAGS, &payload);
        let _ = writer.lock().await.write_all(&frame).await;
    }
}
```

- [ ] **Step 2: Cargo check**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: Clean compilation

- [ ] **Step 3: Build + test**

Run: `uv sync --reinstall-package c-two && C2_RELAY_ADDRESS= uv run pytest tests/integration/test_ipc_buddy_reply.py -q --timeout=30`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-server/
git commit -m "perf(c2-server): stack buffer for small inline replies

write_reply_with_data() uses 1024B stack buffer for small responses,
matching call_inline() optimization. Eliminates heap alloc for typical
small responses.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 5: Phase 1 integration tests + full verification

**Files:**
- Modify: `tests/integration/test_ipc_buddy_reply.py`

- [ ] **Step 1: Add GC integration test**

```python
def test_gc_buddy_auto_trigger(self):
    """Verify buddy segments are reclaimed after going idle."""
    proxy = self._setup_ipc('ipc://test_buddy_reply_gc')
    for _ in range(5):
        data = os.urandom(10 * 1024 * 1024)  # 10 MB
        result = proxy.echo(data)
        assert len(result) == len(data)
    small = b'after-gc'
    result = proxy.echo(small)
    assert result == small
    cc.close(proxy)
```

- [ ] **Step 2: Add small payload stress test**

```python
def test_small_payload_stress(self):
    """Many small calls use stack buffer path (no regression)."""
    proxy = self._setup_ipc('ipc://test_buddy_reply_small_stress')
    for i in range(100):
        data = f'msg-{i}'.encode()
        result = proxy.echo(data)
        assert result == data
    cc.close(proxy)
```

- [ ] **Step 3: Run full test suite**

Run: `uv sync --reinstall-package c-two && C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All tests pass (491+)

- [ ] **Step 4: Commit**

```bash
git add tests/
git commit -m "test: add GC auto-trigger and small payload stress tests

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 2: SHM Degradation + ResponseBuffer Zero-Copy

### Task 6: ResponseData enum + decode_response refactor

**Files:**
- Create: `src/c_two/_native/c2-ipc/src/response.rs`
- Modify: `src/c_two/_native/c2-ipc/src/lib.rs`
- Modify: `src/c_two/_native/c2-ipc/src/client.rs:826-868`
- Modify: `src/c_two/_native/c2-ipc/src/sync_client.rs:72-80`

- [ ] **Step 1: Create response.rs**

```rust
// src/c_two/_native/c2-ipc/src/response.rs
#[derive(Debug)]
pub enum ResponseData {
    Inline(Vec<u8>),
    Shm {
        seg_idx: u16,
        offset: u32,
        data_size: u32,
        is_dedicated: bool,
    },
}

impl ResponseData {
    pub fn len(&self) -> usize {
        match self {
            ResponseData::Inline(v) => v.len(),
            ResponseData::Shm { data_size, .. } => *data_size as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
```

- [ ] **Step 2: Register module in c2-ipc/src/lib.rs**

Add `pub mod response;` and `pub use response::ResponseData;`.

- [ ] **Step 3: Refactor decode_response → ResponseData**

SHM branch returns coordinates (no copy). Inline branch wraps in `ResponseData::Inline`. See design spec §2.2 for exact code.

- [ ] **Step 4: Update call_full, call_inline, call_buddy, call_chunked**

Return types change from `Result<Vec<u8>, IpcError>` to `Result<ResponseData, IpcError>`. Pending map type also changes.

- [ ] **Step 5: Update SyncClient::call() return type**

- [ ] **Step 6: Cargo check c2-ipc**

Run: `cd src/c_two/_native && cargo check -p c2-ipc`

- [ ] **Step 7: Commit**

```bash
git add src/c_two/_native/c2-ipc/
git commit -m "feat(c2-ipc): ResponseData enum, defer SHM read

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 7: PyResponseBuffer pyclass (Defect D core)

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/client_ffi.rs`
- Modify: `src/c_two/_native/c2-ffi/src/lib.rs`

- [ ] **Step 1: Add ResponseBufferInner enum + PyResponseBuffer struct**

In `client_ffi.rs`, add `ResponseBufferInner` (Inline/Shm variants) and `PyResponseBuffer` (frozen pyclass with `Mutex<Option<Inner>>` + `AtomicU32` exports counter). See design spec section 2.2.

- [ ] **Step 2: Implement __len__, __bytes__, release pymethods**

- `release()` checks `exports > 0` before allowing release
- `__bytes__` copies data for backward compatibility
- `__len__` returns data size

- [ ] **Step 3: Implement __getbuffer__ / __releasebuffer__**

Buffer protocol for zero-copy memoryview. SHM path returns direct pointer. exports counter tracks active memoryviews. See design spec section 2.2.

- [ ] **Step 4: Implement Drop for GC safety**

Auto-release SHM on garbage collection if Python code forgot `release()`.

- [ ] **Step 5: Update PyRustClient.call() to return PyResponseBuffer**

Return `PyResponseBuffer` instead of `PyBytes`. Construct from `ResponseData`. SyncClient must expose `server_pool_arc()` accessor.

- [ ] **Step 6: Register in c2-ffi/src/lib.rs**

Add `m.add_class::<client_ffi::PyResponseBuffer>()?;`

- [ ] **Step 7: Cargo check**

Run: `cd src/c_two/_native && cargo check -p c2-ffi`

- [ ] **Step 8: Commit**

```bash
git add src/c_two/_native/c2-ffi/
git commit -m "feat(c2-ffi): PyResponseBuffer with buffer protocol

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 8: SHM failure degradation (Defect B)

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs:799-873`

- [ ] **Step 1: write_buddy_reply_with_data returns Result**

Return `Result<(), String>` instead of `()`. On alloc/write failure, return `Err` instead of silently falling back inline.

- [ ] **Step 2: smart_reply_with_data tries buddy then degrades**

On buddy `Err`, fall back to inline (Phase 3 adds chunked here).

- [ ] **Step 3: Cargo check + test**

Run: `cd src/c_two/_native && cargo check -p c2-server`

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-server/
git commit -m "feat(c2-server): SHM alloc failure degrades to inline

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 9: Python integration -- memoryview + release

**Files:**
- Modify: `src/c_two/crm/transferable.py:319-329`
- Modify: `src/c_two/transport/client/proxy.py:179-203`
- Modify: `tests/integration/test_ipc_buddy_reply.py`

- [ ] **Step 1: Update transferable.py response handling**

In `com_to_crm()`, wrap response with memoryview + release:

```python
response = client.call(method_name, serialized_args)
stage = 'deserialize_output'
if not output_transferable:
    if hasattr(response, 'release'):
        response.release()
    return None
try:
    mv = memoryview(response) if hasattr(response, '__getbuffer__') else response
    result = output_transferable(mv)
finally:
    if hasattr(response, 'release'):
        response.release()
return result
```

- [ ] **Step 2: Build + full test suite**

Run: `uv sync --reinstall-package c-two && C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`

- [ ] **Step 3: Add Phase 2 integration tests**

Add tests for ResponseBuffer memoryview and SHM degradation.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/ tests/
git commit -m "feat: Python memoryview + release for ResponseBuffer

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```


---

## Phase 3: Chunked Response Path

### Task 10: Reply chunk meta codec

**Files:**
- Modify: `src/c_two/_native/c2-wire/src/chunk.rs`

- [ ] **Step 1: Check FLAG_CHUNKED reuse for replies**

Chunked response frames use `FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_CHUNKED`. The existing `FLAG_CHUNKED` (1 << 9) and `FLAG_CHUNK_LAST` (1 << 10) are reused. No new flag constants needed -- `is_response() && is_chunked()` distinguishes direction.

- [ ] **Step 2: Add reply chunk meta encode/decode**

In `chunk.rs`, add extended reply chunk metadata (includes `total_size: u64`):

```rust
pub const REPLY_CHUNK_META_SIZE: usize = 16;

pub fn encode_reply_chunk_meta(total_size: u64, total_chunks: u32, chunk_idx: u32) -> [u8; REPLY_CHUNK_META_SIZE] {
    let mut buf = [0u8; REPLY_CHUNK_META_SIZE];
    buf[0..8].copy_from_slice(&total_size.to_le_bytes());
    buf[8..12].copy_from_slice(&total_chunks.to_le_bytes());
    buf[12..16].copy_from_slice(&chunk_idx.to_le_bytes());
    buf
}

pub fn decode_reply_chunk_meta(buf: &[u8], offset: usize) -> Result<(u64, u32, u32, usize), String> {
    if buf.len() < offset + REPLY_CHUNK_META_SIZE {
        return Err("buffer too short for reply chunk meta".into());
    }
    let total_size = u64::from_le_bytes(buf[offset..offset+8].try_into().unwrap());
    let total_chunks = u32::from_le_bytes(buf[offset+8..offset+12].try_into().unwrap());
    let chunk_idx = u32::from_le_bytes(buf[offset+12..offset+16].try_into().unwrap());
    Ok((total_size, total_chunks, chunk_idx, REPLY_CHUNK_META_SIZE))
}
```

- [ ] **Step 3: Add round-trip unit test**

- [ ] **Step 4: Cargo test c2-wire**

Run: `cd src/c_two/_native && cargo test -p c2-wire --no-default-features`

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/c2-wire/
git commit -m "feat(c2-wire): reply chunk meta codec

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 11: Server chunked reply writer

**Files:**
- Modify: `src/c_two/_native/c2-server/src/server.rs`

- [ ] **Step 1: Add write_chunked_reply function**

Splits data into chunks, sends each with reply chunk meta. Uses `FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_CHUNKED` flags. Last chunk also sets `FLAG_CHUNK_LAST`. See design spec section 3.1 for exact code pattern.

- [ ] **Step 2: Wire into smart_reply_with_data degradation**

Update `smart_reply_with_data` to add `chunk_size` parameter. When SHM fails and data > chunk_size, use `write_chunked_reply`. Pass `chunk_size` from `IPCConfig` through to the function call.

- [ ] **Step 3: Cargo check + test**

Run: `cd src/c_two/_native && cargo check -p c2-server`

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-server/
git commit -m "feat(c2-server): chunked reply writer for large responses

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 12: Client chunked response reception

**Files:**
- Modify: `src/c_two/_native/c2-ipc/src/client.rs` (recv_loop)
- Modify: `src/c_two/_native/c2-ipc/src/response.rs`
- Modify: `src/c_two/_native/c2-ffi/src/client_ffi.rs`

- [ ] **Step 1: Add ResponseData::Handle variant**

```rust
use c2_mem::MemHandle;

pub enum ResponseData {
    Inline(Vec<u8>),
    Shm { seg_idx: u16, offset: u32, data_size: u32, is_dedicated: bool },
    Handle(MemHandle),
}
```

- [ ] **Step 2: Add chunked response handling in recv_loop**

In recv_loop, detect `is_response() && is_chunked()`:
- First chunk: create ChunkAssembler using client reassembly pool
- Feed chunks via feed_chunk()
- On FLAG_CHUNK_LAST or is_complete(): finish() -> MemHandle -> ResponseData::Handle
- Send through pending oneshot channel

- [ ] **Step 3: Add Handle variant to PyResponseBuffer**

`ResponseBufferInner::Handle(MemHandle, Arc<Mutex<MemPool>>)` -- provides buffer protocol over the MemHandle data.

- [ ] **Step 4: Cargo check + build + test**

```bash
cd src/c_two/_native && cargo check -p c2-ipc -p c2-ffi
uv sync --reinstall-package c-two
C2_RELAY_ADDRESS= uv run pytest tests/integration/test_ipc_buddy_reply.py -q --timeout=30
```

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/
git commit -m "feat(c2-ipc): chunked response reception + reassembly

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 13: Phase 3 integration tests + final verification

**Files:**
- Modify: `tests/integration/test_ipc_buddy_reply.py`

- [ ] **Step 1: Add chunked response test**

```python
def test_chunked_response_path(self):
    """Response larger than SHM pool triggers chunked transfer."""
    proxy = self._setup_ipc('ipc://test_chunked_reply')
    data = os.urandom(300 * 1024 * 1024)  # 300 MB
    result = proxy.echo(data)
    assert len(result) == len(data)
    assert result == data
    cc.close(proxy)
```

- [ ] **Step 2: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=60`
Expected: All tests pass

- [ ] **Step 3: Benchmark**

Run: `C2_RELAY_ADDRESS= uv run python benchmarks/three_mode_benchmark.py`
Compare: 100MB IPC target <= 25ms, 64B IPC target <= 0.15ms

- [ ] **Step 4: Commit**

```bash
git add tests/
git commit -m "test: chunked response + full benchmark verification

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```
