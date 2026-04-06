# Chunk Lifecycle Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a new `c2-chunk` crate providing unified chunk reassembly lifecycle management (GC, soft limits, FileSpill promotion, connection cleanup) and integrate it into both `c2-server` and `c2-ipc`, replacing their independent assembler storage with a single shared `ChunkRegistry`.

**Architecture:** A new `c2-chunk` crate sits between `c2-wire`/`c2-mem` (dependencies) and `c2-server`/`c2-ipc` (consumers). It provides a sharded `ChunkRegistry` keyed by `(conn_id, request_id)` with 16 shards for per-connection lock isolation. Callers spawn their own GC timer tasks — the registry is runtime-agnostic (no tokio dependency). The server removes `Connection.assemblers` and the client removes its local `HashMap<u32, ChunkAssembler>`, both delegating to the shared registry.

**Tech Stack:** Rust (edition 2024), `c2-mem` (MemPool/MemHandle), `c2-wire` (ChunkAssembler), `c2-config` (IpcConfig), `std::sync::{Mutex, RwLock, atomic}`, `std::time::Instant`. No tokio in c2-chunk itself.

**Spec:** `docs/superpowers/specs/2026-04-04-chunk-lifecycle-design.md`

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `c2-chunk/Cargo.toml` | Crate manifest — depends on c2-config, c2-mem, c2-wire(std) |
| `c2-chunk/src/lib.rs` | Public re-exports: `ChunkRegistry`, `ChunkConfig`, `GcStats`, `promote_to_shm` |
| `c2-chunk/src/config.rs` | `ChunkConfig` struct extracted from IpcConfig chunk fields |
| `c2-chunk/src/promote.rs` | `promote_to_shm()` — FileSpill→SHM promotion |
| `c2-chunk/src/registry.rs` | `ChunkRegistry` — sharded lifecycle manager |

### Modified Files

| File | Changes |
|------|---------|
| `Cargo.toml` (workspace) | Add `c2-chunk` to members |
| `c2-config/src/ipc.rs` | `chunk_gc_interval: u32` → `f64` (seconds) |
| `c2-mem/src/pool.rs` | Add `try_alloc_shm()` — SHM-only alloc (no FileSpill fallback) |
| `c2-wire/src/assembler.rs` | Add `take_handle()`, parameterize limits in `new()` |
| `c2-server/Cargo.toml` | Add `c2-chunk` dependency |
| `c2-server/src/server.rs` | Replace `reassembly_pool` → `chunk_registry`, add GC timer |
| `c2-server/src/connection.rs` | Remove `assemblers` + methods, add `FlightGuard` |
| `c2-ipc/Cargo.toml` | Add `c2-chunk` dependency |
| `c2-ipc/src/client.rs` | Add `conn_id`, replace local assemblers → registry |
| `c2-ffi/Cargo.toml` | Add `c2-chunk` dependency |
| `c2-ffi/src/server_ffi.rs` | Pass chunk config when creating Server |
| `c2-ffi/src/client_ffi.rs` | Create/pass ChunkRegistry to client |

All file paths below are relative to `src/c_two/_native/`.

---

## Task 1: Update `c2-config` — change `chunk_gc_interval` type

**Files:**
- Modify: `c2-config/src/ipc.rs:42` (field type) and `:77` (default value)

- [ ] **Step 1: Write the failing test**

Add to `c2-config/src/ipc.rs` in the `#[cfg(test)]` module:

```rust
#[test]
fn chunk_gc_interval_is_f64() {
    let cfg = IpcConfig::default();
    let _secs: f64 = cfg.chunk_gc_interval; // compile-time type check
    assert!((cfg.chunk_gc_interval - 5.0).abs() < f64::EPSILON);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-config --no-default-features -- chunk_gc_interval_is_f64`
Expected: FAIL — `chunk_gc_interval` is currently `u32`, type mismatch.

- [ ] **Step 3: Change the field type and default**

In `c2-config/src/ipc.rs`:

Line 42 — change:
```rust
    /// GC sweep interval in seconds (default 5.0).
    pub chunk_gc_interval: f64,
```

Line 77 — change:
```rust
            chunk_gc_interval: 5.0,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd src/c_two/_native && cargo test -p c2-config --no-default-features`
Expected: ALL PASS (including existing tests — `default_validates()` still works).

- [ ] **Step 5: Run full workspace check for downstream breakage**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: PASS — no downstream crate currently reads `chunk_gc_interval`.

- [ ] **Step 6: Commit**

```bash
cd src/c_two/_native
git add c2-config/
git commit -m "refactor(c2-config): change chunk_gc_interval from u32 to f64 seconds

Default changes from 100 (request cycles) to 5.0 (seconds).
Part of chunk lifecycle management (c2-chunk crate).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 2: Add `MemPool::try_alloc_shm()` to `c2-mem`

**Files:**
- Modify: `c2-mem/src/pool.rs` (add method after `alloc_handle`)
- Test: `c2-mem/src/pool.rs` `#[cfg(test)]` module

> **Prerequisite check:** Verify `MemHandle` in `c2-mem/src/handle.rs` has **no** `Drop` impl.
> This is critical — callers (`promote_to_shm`, `finish()` error path) manually release handles.
> If a Drop impl exists, the double-free will corrupt the pool. Run `grep 'impl Drop for MemHandle'` to confirm.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `c2-mem/src/pool.rs`:

```rust
#[test]
fn try_alloc_shm_returns_buddy() {
    let mut pool = test_pool(small_config());
    let h = pool.try_alloc_shm(4096).unwrap();
    assert!(h.is_buddy());
    pool.release_handle(h);
}

#[test]
fn try_alloc_shm_never_file_spills() {
    // Create a pool with tiny segments that will fill up quickly
    let cfg = PoolConfig {
        segment_size: 8192,
        min_block_size: 4096,
        max_segments: 1,
        max_dedicated_segments: 0,
        dedicated_crash_timeout_secs: 0.0,
        spill_threshold: 0.0, // would normally always spill
        spill_dir: std::env::temp_dir().join("c2_try_shm_test"),
    };
    let mut pool = test_pool(cfg);
    // Fill the single buddy segment
    let _h1 = pool.alloc_handle(4096).unwrap();
    let _h2 = pool.alloc_handle(4096).unwrap();
    // Now try_alloc_shm should fail — NOT fall back to FileSpill
    let result = pool.try_alloc_shm(4096);
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-mem -- try_alloc_shm`
Expected: FAIL — method does not exist.

- [ ] **Step 3: Implement `try_alloc_shm`**

Add to `c2-mem/src/pool.rs`, after `alloc_handle()` (after line 571):

```rust
    /// Allocate from Buddy or Dedicated SHM only — no FileSpill fallback.
    ///
    /// Used by `promote_to_shm` when upgrading a FileSpill handle.
    /// Returns `Err` if neither buddy nor dedicated has capacity.
    pub fn try_alloc_shm(&mut self, size: usize) -> Result<MemHandle, String> {
        if size == 0 {
            return Err("cannot allocate 0 bytes".into());
        }
        let max_buddy = self.max_buddy_block_size();

        // Try existing buddy segments.
        if max_buddy > 0 && size <= max_buddy {
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

        // Try buddy expansion (no RAM check — caller already has data in RAM).
        if max_buddy > 0 && size <= max_buddy
            && self.segments.len() < self.config.max_segments
        {
            let reclaimed = self.gc_buddy();
            if reclaimed > 0 {
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
            if let Ok(seg) = self.create_segment() {
                let idx = self.segments.len();
                self.segments.push(seg);
                self.idle_since.push(None);
                if let Some(a) = self.segments[idx].allocator().alloc(size) {
                    return Ok(MemHandle::Buddy {
                        seg_idx: idx as u16, offset: a.offset, len: size,
                    });
                }
            }
        }

        // Try dedicated SHM — NO FileSpill fallback.
        match self.alloc_dedicated(size) {
            Ok(alloc) => Ok(MemHandle::Dedicated {
                seg_idx: alloc.seg_idx as u16, len: size,
            }),
            Err(_) => Err(format!(
                "try_alloc_shm: no SHM capacity for {size} bytes"
            )),
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src/c_two/_native && cargo test -p c2-mem`
Expected: ALL PASS (new + existing).

- [ ] **Step 5: Commit**

```bash
cd src/c_two/_native
git add c2-mem/
git commit -m "feat(c2-mem): add MemPool::try_alloc_shm() — SHM-only allocation

Allocates from buddy or dedicated SHM without FileSpill fallback.
Returns Err if neither has capacity. Used by promote_to_shm() in c2-chunk.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 3: Update `ChunkAssembler` in `c2-wire`

**Files:**
- Modify: `c2-wire/src/assembler.rs`
- Test: `c2-wire/src/assembler.rs` `#[cfg(test)]` module

### Part A: Add `take_handle()`

- [ ] **Step 1: Write the failing test for `take_handle`**

Add to `c2-wire/src/assembler.rs` `#[cfg(test)]` module:

```rust
#[test]
fn take_handle_extracts_and_clears() {
    let mut pool = test_pool();
    let mut asm = ChunkAssembler::new(&mut pool, 1, 4096, 512, 8 * (1 << 30)).unwrap();
    let h = asm.take_handle();
    assert!(h.is_some());
    let h2 = asm.take_handle();
    assert!(h2.is_none()); // second call returns None
    pool.release_handle(h.unwrap());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-wire -- take_handle`
Expected: FAIL — method and new signature don't exist.

- [ ] **Step 3: Implement `take_handle()` and update `new()` signature**

In `c2-wire/src/assembler.rs`:

**Delete the hardcoded constants** (lines 10-13):
```rust
// DELETE these two lines:
// const MAX_TOTAL_CHUNKS: usize = 512;
// const MAX_REASSEMBLY_BYTES: usize = 8 * (1 << 30);
```

**Change the `handle` field** from `MemHandle` to `Option<MemHandle>` (line 29):
```rust
    handle: Option<MemHandle>,
```

**Update `new()` signature** (lines 52-85) — add two params, use them for validation:
```rust
    pub fn new(
        pool: &mut MemPool,
        total_chunks: usize,
        chunk_size: usize,
        max_total_chunks: usize,
        max_reassembly_bytes: usize,
    ) -> Result<Self, String> {
        if total_chunks == 0 {
            return Err("total_chunks must be > 0".into());
        }
        if total_chunks > max_total_chunks {
            return Err(format!(
                "total_chunks {total_chunks} exceeds limit {max_total_chunks}"
            ));
        }
        let alloc_size = total_chunks.checked_mul(chunk_size)
            .ok_or_else(|| "chunk allocation overflow".to_string())?;
        if alloc_size > max_reassembly_bytes {
            return Err(format!(
                "reassembly size {alloc_size} exceeds limit {max_reassembly_bytes}"
            ));
        }
        let handle = pool.alloc_handle(alloc_size)?;
        Ok(Self {
            total_chunks,
            chunk_size,
            received: 0,
            written_end: 0,
            handle: Some(handle),
            received_flags: vec![false; total_chunks],
            route_name: None,
            method_idx: None,
        })
    }
```

**Update `feed_chunk()`** — unwrap the Option:
```rust
    pub fn feed_chunk(
        &mut self,
        pool: &MemPool,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String> {
        if chunk_idx >= self.total_chunks {
            return Err(format!(
                "chunk_idx {chunk_idx} >= total_chunks {}",
                self.total_chunks
            ));
        }
        if self.received_flags[chunk_idx] {
            return Err(format!("duplicate chunk_idx {chunk_idx}"));
        }
        if data.len() > self.chunk_size {
            return Err(format!(
                "data length {} exceeds chunk_size {}",
                data.len(), self.chunk_size
            ));
        }
        let handle = self.handle.as_mut()
            .ok_or("assembler handle already taken")?;
        let offset = chunk_idx * self.chunk_size;
        let slice = pool.handle_slice_mut(handle);
        slice[offset..offset + data.len()].copy_from_slice(data);
        self.received_flags[chunk_idx] = true;
        self.received += 1;
        let end = offset + data.len();
        if end > self.written_end {
            self.written_end = end;
        }
        Ok(self.received == self.total_chunks)
    }
```

**Update `finish()`** — take from Option:
```rust
    pub fn finish(mut self) -> Result<MemHandle, String> {
        if !self.is_complete() {
            return Err(format!(
                "incomplete: {}/{} chunks",
                self.received, self.total_chunks
            ));
        }
        let mut handle = self.handle.take()
            .ok_or("assembler handle already taken")?;
        handle.set_len(self.written_end);
        Ok(handle)
    }
```

**Update `abort()`** — take from Option:
```rust
    pub fn abort(mut self, pool: &mut MemPool) {
        if let Some(handle) = self.handle.take() {
            pool.release_handle(handle);
        }
    }
```

**Add `take_handle()`** method:
```rust
    /// Extract the underlying MemHandle without consuming self.
    ///
    /// Returns `None` if the handle was already taken (by a previous
    /// `take_handle`, `finish`, or `abort` call).
    /// Used by `ChunkRegistry` for safe error-path cleanup.
    pub fn take_handle(&mut self) -> Option<MemHandle> {
        self.handle.take()
    }
```

- [ ] **Step 4: Update all existing tests to use new `new()` signature**

Every existing test calling `ChunkAssembler::new(&mut pool, N, S)` must become `ChunkAssembler::new(&mut pool, N, S, 512, 8 * (1 << 30))`. Update all occurrences in the test module.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src/c_two/_native && cargo test -p c2-wire`
Expected: ALL PASS.

- [ ] **Step 6: Check downstream compilation**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: FAIL in `c2-server` and `c2-ipc` — they call `ChunkAssembler::new()` with the old 3-arg signature. Fix them:

In `c2-server/src/server.rs` (around line 700):
```rust
// Change from:
ChunkAssembler::new(&mut pool, total_chunks as usize, chunk_size)
// To:
ChunkAssembler::new(
    &mut pool, total_chunks as usize, chunk_size,
    server.config.max_total_chunks as usize,
    server.config.max_reassembly_bytes as usize,
)
```

In `c2-ipc/src/client.rs` (around line 960):
```rust
// Change from:
ChunkAssembler::new(&mut pool, total_chunks as usize, chunk_size)
// To:
ChunkAssembler::new(
    &mut pool, total_chunks as usize, chunk_size,
    512, // TODO: pass from config
    8 * (1 << 30), // TODO: pass from config
)
```

- [ ] **Step 7: Run full workspace check**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
cd src/c_two/_native
git add c2-wire/ c2-server/src/server.rs c2-ipc/src/client.rs
git commit -m "refactor(c2-wire): parameterize ChunkAssembler limits, add take_handle()

- ChunkAssembler::new() gains max_total_chunks and max_reassembly_bytes
  params. Hardcoded constants deleted.
- handle field becomes Option<MemHandle> for safe extraction.
- take_handle() allows ChunkRegistry to manage handle ownership on
  error paths without consuming the assembler.
- Updated c2-server and c2-ipc call sites.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 4: Create `c2-chunk` crate scaffold + `ChunkConfig`

**Files:**
- Create: `c2-chunk/Cargo.toml`
- Create: `c2-chunk/src/lib.rs`
- Create: `c2-chunk/src/config.rs`
- Modify: `Cargo.toml` (workspace)

- [ ] **Step 1: Add c2-chunk to workspace**

In `Cargo.toml` (workspace root), add `"c2-chunk"` to the members list:
```toml
members = [
    "c2-config",
    "c2-mem",
    "c2-wire",
    "c2-chunk",
    "c2-ipc",
    "c2-http",
    "c2-relay",
    "c2-server",
    "c2-ffi",
]
```

- [ ] **Step 2: Create `c2-chunk/Cargo.toml`**

```toml
[package]
name = "c2-chunk"
edition.workspace = true
version.workspace = true
description = "Chunk reassembly lifecycle management for C-Two IPC transport"

[dependencies]
c2-config = { path = "../c2-config" }
c2-mem = { path = "../c2-mem" }
c2-wire = { path = "../c2-wire", features = ["std"] }
tracing = "0.1"
```

- [ ] **Step 3: Create `c2-chunk/src/config.rs`**

```rust
//! Chunk lifecycle configuration.

use std::time::Duration;

/// Configuration for the [`ChunkRegistry`](super::ChunkRegistry).
///
/// Typically derived from [`c2_config::IpcConfig`] chunk-related fields.
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Timeout for incomplete assemblies (default 60s).
    pub assembler_timeout: Duration,
    /// GC sweep interval (default 5s). Callers use this to drive their timer.
    pub gc_interval: Duration,
    /// Soft limit on concurrent in-flight assemblies (default 512).
    pub soft_limit: u32,
    /// Soft limit on total reassembly bytes (default 8 GB).
    pub max_reassembly_bytes: u64,
    /// Per-assembler: max chunks allowed (passed to ChunkAssembler::new).
    pub max_chunks_per_request: usize,
    /// Per-assembler: max bytes allowed (passed to ChunkAssembler::new).
    pub max_bytes_per_request: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            assembler_timeout: Duration::from_secs(60),
            gc_interval: Duration::from_secs(5),
            soft_limit: 512,
            max_reassembly_bytes: 8_589_934_592, // 8 GB
            max_chunks_per_request: 512,
            max_bytes_per_request: 8 * (1 << 30), // 8 GB
        }
    }
}

impl ChunkConfig {
    /// Build a `ChunkConfig` from an [`IpcConfig`](c2_config::IpcConfig).
    pub fn from_ipc(cfg: &c2_config::IpcConfig) -> Self {
        Self {
            assembler_timeout: Duration::from_secs_f64(cfg.chunk_assembler_timeout),
            gc_interval: Duration::from_secs_f64(cfg.chunk_gc_interval),
            soft_limit: cfg.max_total_chunks,
            max_reassembly_bytes: cfg.max_reassembly_bytes,
            max_chunks_per_request: cfg.max_total_chunks as usize,
            max_bytes_per_request: cfg.max_reassembly_bytes as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_sane() {
        let cfg = ChunkConfig::default();
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
    }

    #[test]
    fn from_ipc_config() {
        let ipc = c2_config::IpcConfig::default();
        let cfg = ChunkConfig::from_ipc(&ipc);
        assert_eq!(cfg.assembler_timeout, Duration::from_secs(60));
        assert_eq!(cfg.gc_interval, Duration::from_secs(5));
        assert_eq!(cfg.soft_limit, 512);
        assert_eq!(cfg.max_reassembly_bytes, 8_589_934_592);
    }
}
```

- [ ] **Step 4: Create `c2-chunk/src/lib.rs`**

```rust
//! Chunk reassembly lifecycle management.
//!
//! Provides [`ChunkRegistry`] — a sharded, thread-safe manager for in-flight
//! chunked transfers. Used identically by both `c2-server` and `c2-ipc`.

pub mod config;
pub mod promote;
pub mod registry;

pub use config::ChunkConfig;
pub use promote::promote_to_shm;
pub use registry::{ChunkRegistry, GcStats};
```

Note: `promote` and `registry` modules will be created in subsequent tasks. For now, create placeholder files so it compiles:

Create `c2-chunk/src/promote.rs`:
```rust
//! FileSpill → SHM promotion.
//! Full implementation in Task 5.

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;

/// Try to move a FileSpill handle into SHM.
/// Returns Ok(shm_handle) on success, Err(original_handle) if SHM unavailable.
pub fn promote_to_shm(
    pool: &mut MemPool,
    file_handle: MemHandle,
) -> Result<MemHandle, MemHandle> {
    // Placeholder — returns original for now.
    Err(file_handle)
}
```

Create `c2-chunk/src/registry.rs`:
```rust
//! Sharded chunk registry.
//! Full implementation in Task 6-7.

use crate::config::ChunkConfig;
use c2_mem::MemPool;
use c2_mem::handle::MemHandle;
use std::sync::{Arc, RwLock};

/// Statistics returned by [`ChunkRegistry::gc_sweep`].
#[derive(Debug, Default)]
pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}

/// Sharded chunk reassembly lifecycle manager.
pub struct ChunkRegistry {
    _pool: Arc<RwLock<MemPool>>,
    _config: ChunkConfig,
}

impl ChunkRegistry {
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self {
        Self { _pool: pool, _config: config }
    }
}
```

- [ ] **Step 5: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-chunk`
Expected: PASS.

- [ ] **Step 6: Run config tests**

Run: `cd src/c_two/_native && cargo test -p c2-chunk`
Expected: 2 tests PASS (default_config_is_sane, from_ipc_config).

- [ ] **Step 7: Commit**

```bash
cd src/c_two/_native
git add Cargo.toml c2-chunk/
git commit -m "feat(c2-chunk): create crate scaffold with ChunkConfig

New c2-chunk crate with:
- ChunkConfig: extracted from IpcConfig chunk fields
- Placeholder registry and promote modules (implemented next)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 5: Implement `promote_to_shm()` in `c2-chunk`

**Files:**
- Modify: `c2-chunk/src/promote.rs`
- Test: `c2-chunk/src/promote.rs` `#[cfg(test)]` module

- [ ] **Step 1: Write the failing tests**

Add to `c2-chunk/src/promote.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn test_pool(cfg: PoolConfig) -> MemPool {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("/cc3p{:04x}{:04x}", std::process::id() as u16, id);
        MemPool::new_with_prefix(cfg, prefix)
    }

    fn spill_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            spill_threshold: 0.0, // force FileSpill on slow path
            spill_dir: std::env::temp_dir().join("c2_promote_test"),
        }
    }

    #[test]
    fn promote_file_spill_to_shm() {
        let mut pool = test_pool(PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            spill_threshold: 1.0, // no spill for SHM allocs
            spill_dir: std::env::temp_dir().join("c2_promote_test"),
        });
        // Manually create a FileSpill handle
        let data = b"hello promote";
        let mut file_handle = pool.alloc_handle(4096).unwrap();
        // Write test data
        pool.handle_slice_mut(&mut file_handle)[..data.len()].copy_from_slice(data);
        file_handle.set_len(data.len());

        // If it's already SHM, force a FileSpill for the test
        if !file_handle.is_file_spill() {
            // This handle is already SHM — promote should return Err (not FileSpill)
            let result = promote_to_shm(&mut pool, file_handle);
            assert!(result.is_err()); // not a FileSpill, nothing to promote
            pool.release_handle(result.unwrap_err());
            return;
        }
        let result = promote_to_shm(&mut pool, file_handle);
        assert!(result.is_ok());
        let shm = result.unwrap();
        assert!(!shm.is_file_spill());
        assert_eq!(shm.len(), data.len());
        assert_eq!(&pool.handle_slice(&shm)[..data.len()], data);
        pool.release_handle(shm);
    }

    #[test]
    fn promote_returns_original_when_shm_full() {
        let mut pool = test_pool(PoolConfig {
            segment_size: 8192,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 0,
            dedicated_crash_timeout_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c2_promote_full_test"),
        });
        // Fill SHM completely
        let _h1 = pool.alloc_handle(4096).unwrap();
        let _h2 = pool.alloc_handle(4096).unwrap();
        // Force a FileSpill alloc
        let file_handle = pool.alloc_file_spill_for_test(1024);
        let result = promote_to_shm(&mut pool, file_handle);
        // SHM full → should return Err with original handle intact
        assert!(result.is_err());
        let original = result.unwrap_err();
        assert!(original.is_file_spill());
        pool.release_handle(original);
    }
}
```

Note: We may need a test helper `alloc_file_spill_for_test` — or we can use spill directly. Adapt based on what MemPool exposes. If `alloc_file_spill` is private, create a FileSpill handle via `c2_mem::spill::create_file_spill()` directly.

- [ ] **Step 2: Implement `promote_to_shm()`**

Replace the placeholder in `c2-chunk/src/promote.rs`:

```rust
//! FileSpill → SHM promotion.

use c2_mem::handle::MemHandle;
use c2_mem::MemPool;

/// Try to move a FileSpill handle into SHM (Buddy or Dedicated).
///
/// Returns `Ok(shm_handle)` on success — original handle is consumed.
/// Returns `Err(original_handle)` if SHM has no capacity — no data loss.
///
/// Only call on FileSpill handles. Non-FileSpill handles return Err unchanged.
pub fn promote_to_shm(
    pool: &mut MemPool,
    file_handle: MemHandle,
) -> Result<MemHandle, MemHandle> {
    if !file_handle.is_file_spill() {
        return Err(file_handle);
    }
    let len = file_handle.len();

    // Phase 1: Allocate SHM destination.
    let mut shm_handle = match pool.try_alloc_shm(len) {
        Ok(h) => h,
        Err(_) => return Err(file_handle),
    };

    // Phase 2: Copy data via temp buffer.
    // We use .to_vec() to avoid potential borrow checker issues with
    // simultaneous handle_slice + handle_slice_mut through the same pool.
    let data = pool.handle_slice(&file_handle)[..len].to_vec();
    pool.handle_slice_mut(&mut shm_handle)[..len].copy_from_slice(&data);

    // Phase 3: Set length and release old handle.
    shm_handle.set_len(len);
    pool.release_handle(file_handle);
    Ok(shm_handle)
}
```

- [ ] **Step 3: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- promote`
Expected: Tests PASS (adjust test setup if `alloc_file_spill` access needs changes).

- [ ] **Step 4: Commit**

```bash
cd src/c_two/_native
git add c2-chunk/src/promote.rs
git commit -m "feat(c2-chunk): implement promote_to_shm()

FileSpill→SHM promotion using try_alloc_shm + temp buffer copy.
Returns original handle on failure (no data loss).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 6: Implement `ChunkRegistry` core lifecycle (insert/feed/finish/abort)

**Files:**
- Modify: `c2-chunk/src/registry.rs`
- Test: `c2-chunk/src/registry.rs` `#[cfg(test)]` module

- [ ] **Step 1: Write the failing test — happy path**

Add to `c2-chunk/src/registry.rs` a test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use c2_mem::config::PoolConfig;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_ID: AtomicU32 = AtomicU32::new(0);

    fn test_pool() -> Arc<RwLock<MemPool>> {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("/cc3g{:04x}{:04x}", std::process::id() as u16, id);
        Arc::new(RwLock::new(MemPool::new_with_prefix(
            PoolConfig {
                segment_size: 64 * 1024,
                min_block_size: 4096,
                max_segments: 2,
                max_dedicated_segments: 2,
                dedicated_crash_timeout_secs: 0.0,
                spill_threshold: 1.0,
                spill_dir: std::env::temp_dir().join("c2_reg_test"),
            },
            prefix,
        )))
    }

    #[test]
    fn insert_feed_finish_happy_path() {
        let pool = test_pool();
        let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
        let data = b"hello world!";
        reg.insert(1, 100, 1, data.len()).unwrap();
        assert_eq!(reg.active_count(), 1);
        let complete = reg.feed(1, 100, 0, data).unwrap();
        assert!(complete);
        let handle = reg.finish(1, 100).unwrap();
        assert_eq!(handle.len(), data.len());
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.total_bytes(), 0);
        pool.write().unwrap().release_handle(handle);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- insert_feed_finish`
Expected: FAIL — `ChunkRegistry` has no `insert`/`feed`/`finish` methods.

- [ ] **Step 3: Implement the full `ChunkRegistry` struct with sharded storage**

Replace the placeholder in `c2-chunk/src/registry.rs` with the complete struct definition including fields: `shards` (16 `Mutex<HashMap>`), `pool`, `config`, `active_count` (AtomicUsize), `total_bytes` (AtomicU64). Implement the `shard()` helper, `new()`, `active_count()`, `total_bytes()` query methods, `config()` getter (`&ChunkConfig`), `pool()` getter (`&Arc<RwLock<MemPool>>`), and `insert()`.

Key implementation details:

- `SHARD_COUNT = 16` (power of two)
- `shard(&self, conn_id) -> &Mutex<HashMap<(u64,u64), TrackedAssembler>>` uses `conn_id as usize % SHARD_COUNT`
- `insert()`: checks soft limits (atomic reads), triggers `gc_sweep()` if exceeded, allocates via `pool.write().unwrap().alloc_handle()`, wraps in `TrackedAssembler`, inserts into shard, increments atomics
- `TrackedAssembler` stores: `inner: ChunkAssembler`, `created_at: Instant`, `last_activity: Instant`, `total_bytes: u64`

- [ ] **Step 4: Implement `feed()`**

`feed()` locks the correct shard, gets mutable ref to `TrackedAssembler`, calls `inner.feed_chunk()` with pool read lock, updates `last_activity`. Returns `Ok(complete)`.

- [ ] **Step 5: Implement `finish()`**

`finish()` removes from shard, decrements atomics. Calls `inner.take_handle()` to extract handle, then verifies completeness. If the assembler reports complete, sets handle length and optionally promotes FileSpill. On any error, explicitly calls `pool.write().unwrap().release_handle()` on the extracted handle before returning Err.

- [ ] **Step 6: Implement `abort()`**

`abort()` removes from shard, decrements atomics, calls `inner.abort(&mut pool.write().unwrap())`.

- [ ] **Step 7: Run test to verify happy path passes**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- insert_feed_finish`
Expected: PASS.

- [ ] **Step 8: Add more lifecycle tests**

Add tests:
```rust
#[test]
fn abort_frees_resources() {
    let pool = test_pool();
    let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
    reg.insert(1, 200, 3, 1024).unwrap();
    assert_eq!(reg.active_count(), 1);
    reg.abort(1, 200);
    assert_eq!(reg.active_count(), 0);
    assert_eq!(reg.total_bytes(), 0);
}

#[test]
fn finish_error_path_does_not_leak() {
    let pool = test_pool();
    let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
    reg.insert(1, 300, 3, 1024).unwrap();
    // Feed only 1 of 3 chunks — finish should fail
    reg.feed(1, 300, 0, &[0u8; 1024]).unwrap();
    let result = reg.finish(1, 300);
    assert!(result.is_err());
    // Resources must still be freed
    assert_eq!(reg.active_count(), 0);
    assert_eq!(reg.total_bytes(), 0);
}

#[test]
fn multi_chunk_out_of_order() {
    let pool = test_pool();
    let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
    let chunk_size = 128;
    reg.insert(1, 400, 4, chunk_size).unwrap();
    // Feed out of order: 3, 1, 0, 2
    assert!(!reg.feed(1, 400, 3, &[3u8; 64]).unwrap());
    assert!(!reg.feed(1, 400, 1, &[1u8; 128]).unwrap());
    assert!(!reg.feed(1, 400, 0, &[0u8; 128]).unwrap());
    assert!(reg.feed(1, 400, 2, &[2u8; 128]).unwrap());
    let handle = reg.finish(1, 400).unwrap();
    // Verify data integrity
    let p = pool.read().unwrap();
    let slice = p.handle_slice(&handle);
    assert_eq!(&slice[0..128], &[0u8; 128]);
    assert_eq!(&slice[128..256], &[1u8; 128]);
    assert_eq!(&slice[256..384], &[2u8; 128]);
    assert_eq!(&slice[384..384+64], &[3u8; 64]);
    drop(p);
    pool.write().unwrap().release_handle(handle);
}
```

- [ ] **Step 9: Run all tests**

Run: `cd src/c_two/_native && cargo test -p c2-chunk`
Expected: ALL PASS.

- [ ] **Step 10: Commit**

```bash
cd src/c_two/_native
git add c2-chunk/src/registry.rs
git commit -m "feat(c2-chunk): implement ChunkRegistry core lifecycle

Sharded (16 shards) insert/feed/finish/abort with:
- Lock-free atomic counters for active_count/total_bytes
- Safe finish() error path — handle extracted via take_handle(),
  explicitly released on error
- FileSpill promotion on finish()

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 7: Implement GC sweep + cleanup_connection

**Files:**
- Modify: `c2-chunk/src/registry.rs`

- [ ] **Step 1: Write the failing test — GC expires stale assemblies**

```rust
#[test]
fn gc_sweep_expires_stale() {
    let pool = test_pool();
    let cfg = ChunkConfig {
        assembler_timeout: Duration::from_millis(50),
        ..ChunkConfig::default()
    };
    let reg = ChunkRegistry::new(pool.clone(), cfg);
    reg.insert(1, 1, 1, 1024).unwrap();
    assert_eq!(reg.active_count(), 1);
    // Wait for timeout
    std::thread::sleep(Duration::from_millis(80));
    let stats = reg.gc_sweep();
    assert_eq!(stats.expired, 1);
    assert_eq!(reg.active_count(), 0);
    assert_eq!(reg.total_bytes(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- gc_sweep_expires`
Expected: FAIL — `gc_sweep` not yet implemented (or returns default).

- [ ] **Step 3: Implement `gc_sweep()`**

`gc_sweep()` iterates all 16 shards sequentially. For each shard: lock, collect expired keys (where `now - last_activity > assembler_timeout`), for each expired: remove from map, call `inner.abort(&mut pool.write().unwrap())`, decrement atomics, log warning. Return `GcStats { expired, remaining: active_count(), freed_bytes }`.

- [ ] **Step 4: Run GC test**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- gc_sweep`
Expected: PASS.

- [ ] **Step 5: Write test — feed updates last_activity (prevents GC)**

```rust
#[test]
fn feed_updates_last_activity() {
    let pool = test_pool();
    let cfg = ChunkConfig {
        assembler_timeout: Duration::from_millis(100),
        ..ChunkConfig::default()
    };
    let reg = ChunkRegistry::new(pool.clone(), cfg);
    reg.insert(1, 1, 3, 1024).unwrap();
    // Wait 60ms, feed a chunk — resets timer
    std::thread::sleep(Duration::from_millis(60));
    reg.feed(1, 1, 0, &[0u8; 1024]).unwrap();
    // Wait another 60ms (total 120ms from insert, but only 60ms from last feed)
    std::thread::sleep(Duration::from_millis(60));
    let stats = reg.gc_sweep();
    assert_eq!(stats.expired, 0); // should NOT be expired
    assert_eq!(reg.active_count(), 1);
}
```

- [ ] **Step 6: Run test**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- feed_updates`
Expected: PASS (feed already updates `last_activity` in Task 6).

- [ ] **Step 7: Write test — soft limit triggers GC**

```rust
#[test]
fn soft_limit_triggers_gc() {
    let pool = test_pool();
    let cfg = ChunkConfig {
        soft_limit: 2,
        assembler_timeout: Duration::from_millis(10),
        ..ChunkConfig::default()
    };
    let reg = ChunkRegistry::new(pool.clone(), cfg);
    reg.insert(1, 1, 1, 64).unwrap();
    reg.insert(1, 2, 1, 64).unwrap();
    assert_eq!(reg.active_count(), 2);
    // Let both expire
    std::thread::sleep(Duration::from_millis(30));
    // This insert exceeds soft_limit (2) → should trigger gc_sweep internally
    reg.insert(1, 3, 1, 64).unwrap();
    // GC should have cleaned up the 2 expired ones
    assert!(reg.active_count() <= 2); // at most: the 1 new + maybe 1 straggler
}
```

- [ ] **Step 8: Write test — cleanup_connection**

```rust
#[test]
fn cleanup_connection_removes_only_target() {
    let pool = test_pool();
    let reg = ChunkRegistry::new(pool.clone(), ChunkConfig::default());
    // Insert for conn 1 and conn 2
    reg.insert(1, 100, 1, 1024).unwrap();
    reg.insert(1, 200, 1, 1024).unwrap();
    reg.insert(2, 100, 1, 1024).unwrap();
    assert_eq!(reg.active_count(), 3);
    // Cleanup conn 1
    reg.cleanup_connection(1);
    assert_eq!(reg.active_count(), 1);
    // Conn 2's assembly should survive
    let complete = reg.feed(2, 100, 0, &[42u8; 1024]).unwrap();
    assert!(complete);
    let handle = reg.finish(2, 100).unwrap();
    pool.write().unwrap().release_handle(handle);
    assert_eq!(reg.active_count(), 0);
}
```

- [ ] **Step 9: Implement `cleanup_connection()`**

`cleanup_connection(conn_id)` locks `self.shard(conn_id)`, collects all keys where `key.0 == conn_id`, removes each, calls `inner.abort()`, decrements atomics.

- [ ] **Step 10: Run all tests**

Run: `cd src/c_two/_native && cargo test -p c2-chunk`
Expected: ALL PASS.

- [ ] **Step 11: Commit**

```bash
cd src/c_two/_native
git add c2-chunk/src/registry.rs
git commit -m "feat(c2-chunk): implement gc_sweep() and cleanup_connection()

- gc_sweep: iterates all shards sequentially, aborts expired assemblies
- cleanup_connection: O(1) shard access, removes all entries for conn_id
- Soft limit check in insert() triggers gc_sweep when exceeded
- feed() updates last_activity to prevent premature GC

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 8: ChunkRegistry concurrency tests

**Files:**
- Modify: `c2-chunk/src/registry.rs` (test module)

- [ ] **Step 1: Write sharding isolation test**

```rust
#[test]
fn sharding_isolates_connections() {
    let pool = test_pool();
    let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
    let mut handles = vec![];
    // Spawn 8 threads, each using a different conn_id
    for conn_id in 0u64..8 {
        let reg = reg.clone();
        let pool = pool.clone();
        let h = std::thread::spawn(move || {
            for req_id in 0u64..10 {
                reg.insert(conn_id, req_id, 1, 64).unwrap();
                reg.feed(conn_id, req_id, 0, &[conn_id as u8; 64]).unwrap();
                let handle = reg.finish(conn_id, req_id).unwrap();
                pool.write().unwrap().release_handle(handle);
            }
        });
        handles.push(h);
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(reg.active_count(), 0);
    assert_eq!(reg.total_bytes(), 0);
}
```

- [ ] **Step 2: Write concurrent insert/feed/finish on same conn_id test**

```rust
#[test]
fn concurrent_same_conn_no_corruption() {
    let pool = test_pool();
    let reg = Arc::new(ChunkRegistry::new(pool.clone(), ChunkConfig::default()));
    let mut handles = vec![];
    let conn_id = 42u64;
    // 4 threads, each doing 20 insert/feed/finish cycles with unique req_ids
    for thread_idx in 0u64..4 {
        let reg = reg.clone();
        let pool = pool.clone();
        let h = std::thread::spawn(move || {
            for i in 0u64..20 {
                let req_id = thread_idx * 1000 + i;
                reg.insert(conn_id, req_id, 1, 64).unwrap();
                reg.feed(conn_id, req_id, 0, &[0u8; 64]).unwrap();
                let handle = reg.finish(conn_id, req_id).unwrap();
                pool.write().unwrap().release_handle(handle);
            }
        });
        handles.push(h);
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(reg.active_count(), 0);
    assert_eq!(reg.total_bytes(), 0);
}
```

- [ ] **Step 3: Run concurrency tests**

Run: `cd src/c_two/_native && cargo test -p c2-chunk -- concurrent`
Expected: ALL PASS.

- [ ] **Step 4: Commit**

```bash
cd src/c_two/_native
git add c2-chunk/src/registry.rs
git commit -m "test(c2-chunk): add concurrency tests for ChunkRegistry

- sharding_isolates_connections: 8 threads × 10 requests
- concurrent_same_conn_no_corruption: 4 threads × 20 requests on same conn

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 9: Integrate `c2-server` with `ChunkRegistry`

**Files:**
- Modify: `c2-server/Cargo.toml` (add c2-chunk dep)
- Modify: `c2-server/src/server.rs` (replace reassembly_pool, add GC timer)
- Modify: `c2-server/src/connection.rs` (remove assemblers, add FlightGuard)

### Part A: FlightGuard + remove assemblers from Connection

- [ ] **Step 1: Add c2-chunk dependency to c2-server**

In `c2-server/Cargo.toml` `[dependencies]`:
```toml
c2-chunk = { path = "../c2-chunk" }
```

- [ ] **Step 2: Add FlightGuard to connection.rs**

Add before the `Connection` impl block:

```rust
/// RAII guard that increments flight count on creation and decrements on drop.
/// Ensures wait_idle() blocks until all chunk tasks complete.
pub(crate) struct FlightGuard<'a> {
    conn: &'a Connection,
}

impl<'a> FlightGuard<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        conn.flight_inc();
        Self { conn }
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.conn.flight_dec();
    }
}
```

- [ ] **Step 3: Remove assembler fields and methods from Connection**

Delete from `Connection` struct:
```rust
    // DELETE this field:
    // assemblers: Mutex<HashMap<u64, ChunkAssembler>>,
```

Delete from `Connection::new()` the `assemblers: Mutex::new(HashMap::new())` field init.

Delete these three methods entirely:
- `insert_assembler(&self, request_id: u64, asm: ChunkAssembler)`
- `feed_chunk(&self, request_id: u64, pool: &MemPool, chunk_idx: usize, data: &[u8]) -> Result<bool, String>`
- `take_assembler(&self, request_id: u64) -> Option<ChunkAssembler>`

Remove the `use c2_wire::assembler::ChunkAssembler;` import if it becomes unused.

- [ ] **Step 4: Verify connection.rs compiles**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: FAIL — server.rs still references the deleted methods. That's expected; we fix server.rs next.

### Part B: Replace reassembly_pool with chunk_registry in Server

- [ ] **Step 5: Update Server struct**

In `c2-server/src/server.rs`, change the `Server` struct:

```rust
// REMOVE:
//     reassembly_pool: Arc<std::sync::RwLock<MemPool>>,
// ADD:
    chunk_registry: Arc<c2_chunk::ChunkRegistry>,
```

- [ ] **Step 6: Update Server::new()**

In `Server::new()`, after creating `reassembly_pool` (lines 108-122), wrap it in a `ChunkRegistry`:

```rust
let chunk_config = c2_chunk::ChunkConfig::from_ipc(&config);
let chunk_registry = Arc::new(c2_chunk::ChunkRegistry::new(
    Arc::new(std::sync::RwLock::new(reassembly_pool)),
    chunk_config,
));
```

Replace `reassembly_pool: Arc::new(...)` with `chunk_registry` in the `Ok(Self { ... })` block.

- [ ] **Step 7: Add GC timer spawn**

In `Server::run()` (or wherever the server starts its accept loop), before entering the main loop, spawn the GC timer:

```rust
let gc_registry = self.chunk_registry.clone();
let gc_interval = self.chunk_registry.config().gc_interval;
tokio::spawn(async move {
    let mut interval = tokio::time::interval(gc_interval);
    interval.tick().await; // skip first immediate tick
    loop {
        interval.tick().await;
        let stats = gc_registry.gc_sweep();
        if stats.expired > 0 {
            tracing::info!(expired = stats.expired, freed = stats.freed_bytes, "chunk GC");
        }
    }
});
```

Note: `ChunkRegistry` needs a `pub fn config(&self) -> &ChunkConfig` getter for this. Add it in `c2-chunk/src/registry.rs`.

- [ ] **Step 8: Add cleanup_connection call in handle_connection**

In `handle_connection()`, after `conn.wait_idle().await` (line ~349):

```rust
conn.wait_idle().await;
server.chunk_registry.cleanup_connection(conn.conn_id());
```

Note: `Connection` must expose `pub fn conn_id(&self) -> u64`. Add a getter if not present.

- [ ] **Step 9: Update dispatch_chunked_call()**

Replace the assembler interactions in `dispatch_chunked_call()`:

At the function entry (inside the spawned task), add FlightGuard:
```rust
let _flight = FlightGuard::new(&conn);
```

Replace first-chunk assembler creation:
```rust
// OLD: conn.insert_assembler(request_id, asm);
// NEW:
if let Err(e) = server.chunk_registry.insert(conn.conn_id(), request_id, total_chunks as usize, chunk_size) {
    warn!(conn_id, request_id, error = %e, "chunk insert failed");
    return;
}
```

Replace chunk feeding:
```rust
// OLD: conn.feed_chunk(request_id, &pool, chunk_idx, chunk_data)
// NEW:
server.chunk_registry.feed(conn.conn_id(), request_id, chunk_idx as usize, chunk_data)
```

Replace assembler finish:
```rust
// OLD: conn.take_assembler(request_id) + asm.finish()
// NEW:
match server.chunk_registry.finish(conn.conn_id(), request_id) {
    Ok(handle) => { /* use handle for dispatch */ }
    Err(e) => { warn!(...); return; }
}
```

Remove the old `flight_inc()` call that was only at line 787 (now handled by FlightGuard at task entry).

- [ ] **Step 10: Remove reassembly_pool_arc() helper if present**

If `Server` had a method returning `Arc<RwLock<MemPool>>` for reassembly, update callers to go through `chunk_registry` instead. The registry's pool is used for `RequestData::Handle`.

For `RequestData::Handle { handle, pool }`, the pool arc is now inside the registry. Add a public getter:
```rust
// In ChunkRegistry:
pub fn pool(&self) -> &Arc<RwLock<MemPool>> { &self.pool }
```

- [ ] **Step 11: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-server`
Expected: PASS (may need iterative fixes for method signatures).

- [ ] **Step 12: Run existing server tests**

Run: `cd src/c_two/_native && cargo test -p c2-server --no-default-features`
Expected: ALL PASS.

- [ ] **Step 13: Commit**

```bash
cd src/c_two/_native
git add c2-server/ c2-chunk/src/registry.rs
git commit -m "feat(c2-server): integrate ChunkRegistry, add FlightGuard

- Replace reassembly_pool with chunk_registry on Server struct
- Add FlightGuard RAII: flight_inc at chunk task entry, flight_dec on drop
- dispatch_chunked_call uses registry.insert/feed/finish
- handle_connection calls cleanup_connection after wait_idle
- Remove Connection.assemblers and its 3 methods
- Spawn GC timer task in server run loop

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 10: Integrate `c2-ipc` client with `ChunkRegistry`

**Files:**
- Modify: `c2-ipc/Cargo.toml` (add c2-chunk dep)
- Modify: `c2-ipc/src/client.rs` (conn_id, replace local assemblers)

- [ ] **Step 1: Add c2-chunk dependency to c2-ipc**

In `c2-ipc/Cargo.toml` `[dependencies]`:
```toml
c2-chunk = { path = "../c2-chunk" }
```

- [ ] **Step 2: Add CLIENT_CONN_COUNTER and conn_id field**

At the top of `c2-ipc/src/client.rs`:
```rust
use std::sync::atomic::AtomicU64;
static CLIENT_CONN_COUNTER: AtomicU64 = AtomicU64::new(1);
```

Add `conn_id: u64` field to `IpcClient`:
```rust
pub struct IpcClient {
    conn_id: u64,
    // ... existing fields
}
```

In the constructor(s) (`connect()`, `with_pool()`, etc.):
```rust
conn_id: CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed),
```

- [ ] **Step 3: Add chunk_registry field to IpcClient**

```rust
pub struct IpcClient {
    conn_id: u64,
    chunk_registry: Arc<c2_chunk::ChunkRegistry>,
    // ... existing fields (remove reassembly_pool if solely used for chunks)
}
```

In `make_reassembly_pool()` or the constructor, create the registry:
```rust
let chunk_config = c2_chunk::ChunkConfig::from_ipc(&config);
let reassembly_pool = Arc::new(std::sync::RwLock::new(/* existing pool creation */));
let chunk_registry = Arc::new(c2_chunk::ChunkRegistry::new(reassembly_pool, chunk_config));
```

Note: If `reassembly_pool` is also used for non-chunk purposes, keep it as a separate field too. But per the codebase analysis, it's only used in `recv_loop` for chunk reassembly, so it can live solely inside the registry.

- [ ] **Step 4: Update recv_loop signature**

Change `recv_loop` to accept `Arc<ChunkRegistry>` instead of `Arc<StdMutex<MemPool>>`:

```rust
async fn recv_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    pending: Arc<StdMutex<PendingMap>>,
    _server_pool: Arc<StdMutex<Option<ServerPoolState>>>,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    chunk_registry: Arc<c2_chunk::ChunkRegistry>,
    conn_id: u64,
) {
```

- [ ] **Step 5: Replace local assemblers HashMap with registry calls**

Delete: `let mut assemblers: HashMap<u32, ChunkAssembler> = HashMap::new();`

Replace first-chunk creation:
```rust
// OLD: ChunkAssembler::new(&mut pool, total_chunks, chunk_size) + assemblers.insert(rid, asm)
// NEW:
if let Err(e) = chunk_registry.insert(conn_id, rid as u64, total_chunks as usize, chunk_size) {
    // send error to caller
    continue;
}
```

Replace chunk feed:
```rust
// OLD: asm.feed_chunk(&pool, chunk_idx, chunk_data)
// NEW:
match chunk_registry.feed(conn_id, rid as u64, chunk_idx as usize, chunk_data) {
    Ok(true) => { /* complete — call finish */ }
    Ok(false) => { /* more chunks needed */ }
    Err(e) => { /* abort + send error */ chunk_registry.abort(conn_id, rid as u64); }
}
```

Replace finish:
```rust
// OLD: assemblers.remove(&rid).unwrap().finish()
// NEW:
match chunk_registry.finish(conn_id, rid as u64) {
    Ok(handle) => { /* send ResponseData::Handle(handle) to caller */ }
    Err(e) => { /* send error to caller */ }
}
```

Replace disconnect cleanup:
```rust
// OLD: for (_, asm) in assemblers.drain() { asm.abort(&mut pool); }
// NEW:
chunk_registry.cleanup_connection(conn_id);
```

- [ ] **Step 6: Update recv_loop spawn site**

Where `recv_loop` is spawned (in `IpcClient::connect()` or similar), pass the new params:
```rust
let chunk_reg = self.chunk_registry.clone();
let cid = self.conn_id;
tokio::spawn(async move {
    recv_loop(reader, pending, server_pool, writer, chunk_reg, cid).await;
    // ...
});
```

- [ ] **Step 7: Spawn GC timer**

In `IpcClient::connect()`, after spawning `recv_loop`:
```rust
let gc_reg = self.chunk_registry.clone();
let gc_interval = self.chunk_registry.config().gc_interval;
tokio::spawn(async move {
    let mut interval = tokio::time::interval(gc_interval);
    interval.tick().await;
    loop {
        interval.tick().await;
        gc_reg.gc_sweep();
    }
});
```

- [ ] **Step 8: Add TODO comment at request_id truncation**

At the line `let rid = hdr.request_id as u32;`:
```rust
let rid = hdr.request_id as u32; // TODO: migrate to u64 when dispatch table supports it
```

- [ ] **Step 9: Verify it compiles**

Run: `cd src/c_two/_native && cargo check -p c2-ipc`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
cd src/c_two/_native
git add c2-ipc/
git commit -m "feat(c2-ipc): integrate ChunkRegistry in client recv_loop

- Add conn_id field (CLIENT_CONN_COUNTER AtomicU64)
- Replace local assemblers HashMap with ChunkRegistry
- recv_loop delegates insert/feed/finish/abort to registry
- Disconnect calls cleanup_connection
- Spawn GC timer alongside recv_loop

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 11: Update `c2-ffi` bindings

**Files:**
- Modify: `c2-ffi/Cargo.toml` (add c2-chunk dep)
- Modify: `c2-ffi/src/server_ffi.rs` (pass chunk config)
- Modify: `c2-ffi/src/client_ffi.rs` (pass chunk config)

- [ ] **Step 1: Add c2-chunk dependency**

In `c2-ffi/Cargo.toml`:
```toml
c2-chunk = { path = "../c2-chunk" }
```

- [ ] **Step 2: Update RustServer construction in server_ffi.rs**

The `RustServer::new()` constructor creates `Server::new(address, config)`. Ensure that the `IpcConfig` passed includes the updated `chunk_gc_interval: f64` field. Since `RustServer::new()` already builds an `IpcConfig`, verify that all chunk-related Python parameters are correctly mapped.

If the Python `NativeServerBridge` doesn't currently pass chunk-specific params (timeout, gc_interval), this is acceptable — they use defaults from `IpcConfig::default()`. The change is transparent.

- [ ] **Step 3: Update RustClient construction in client_ffi.rs**

Similarly, `PyRustClient::new()` builds an `IpcConfig` and passes it to `SyncClient::connect()`. The chunk config flows through `IpcConfig → ChunkConfig::from_ipc()` inside the client. No FFI-visible changes needed.

- [ ] **Step 4: Handle RequestData::Handle pool arc**

If `PyCrmCallback::invoke()` receives `RequestData::Handle { handle, pool }` and needs the pool arc, verify the server passes `chunk_registry.pool().clone()`. Update `dispatch_chunked_call` in server.rs if needed.

- [ ] **Step 5: Verify full workspace compiles**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd src/c_two/_native
git add c2-ffi/
git commit -m "chore(c2-ffi): add c2-chunk dependency, verify FFI compatibility

No Python-visible API changes. chunk config flows through IpcConfig.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Task 12: End-to-end verification — Python tests

**Files:**
- No new files — run existing tests to verify nothing is broken.

- [ ] **Step 1: Rebuild the Python extension**

Run: `uv sync --reinstall-package c-two`
Expected: Build succeeds (maturin compiles all Rust crates including c2-chunk).

- [ ] **Step 2: Run full Rust test suite**

Run: `cd src/c_two/_native && cargo test --workspace --no-default-features`
Expected: ALL PASS.

- [ ] **Step 3: Run full Python test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: ALL PASS — specifically:
- `tests/unit/test_chunk_assembler.py` (ChunkAssembler Python bindings)
- `tests/unit/test_mem_pool.py` (MemPool with new try_alloc_shm)
- `tests/integration/test_zero_copy_ipc.py` (end-to-end chunked transfer)
- `tests/integration/test_server.py` (server routing)

- [ ] **Step 4: Run specific chunked transfer tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_chunk_assembler.py tests/integration/test_zero_copy_ipc.py -v --timeout=30`
Expected: ALL PASS — chunked transfer still works end-to-end.

- [ ] **Step 5: Final commit + tag**

```bash
git add -A
git commit -m "test: verify chunk lifecycle integration end-to-end

All Rust and Python tests pass after c2-chunk integration.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Verification Checklist

After all tasks complete, verify:

- [ ] `cargo check --workspace` passes
- [ ] `cargo test --workspace --no-default-features` passes
- [ ] `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30` passes
- [ ] `ChunkRegistry.active_count() == 0` after every test (no leaks)
- [ ] `ChunkRegistry.total_bytes() == 0` after every test (no leaks)
- [ ] No `Connection.assemblers` field exists in `c2-server/src/connection.rs`
- [ ] No local `assemblers: HashMap` exists in `c2-ipc/src/client.rs` recv_loop
- [ ] `FlightGuard` used in every spawned chunk task in server
- [ ] GC timer spawned in both server and client
