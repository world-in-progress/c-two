# Rust Native Module Redesign: Three-Tier Memory Architecture

> **Status: ✅ SUPERSEDED** — Original 3-crate design (c2-alloc/c2-segment/c2-mempool) was consolidated into unified `c2-mem` crate. See `disk-spill-memhandle-design.md` for the final architecture.

**Date:** 2026-03-31
**Status:** Draft
**Scope:** Restructure `c2-buddy` into three Rust crates + rename Python interfaces

## 1. Problem Statement

The current `c2-buddy` crate conflates three distinct responsibilities:
- **Allocation algorithms** (buddy allocator, bitmap math)
- **OS resource lifecycle** (POSIX SHM create/open/unlink, cross-process spinlock)
- **Pool composition** (multi-segment pool, fallback chain, GC)

This makes it impossible to:
1. Add alternative allocators (slab, disk spill) without touching SHM code
2. Add alternative backing stores (file-mapped) without touching allocator code
3. Test algorithms in isolation from OS resources

On the Python side, `c_two.buddy` and `BuddyPoolHandle` imply the module is "just a buddy allocator" when it actually manages the entire shared memory subsystem (T1 buddy alloc → T2 pool expansion → T3 dedicated buffers → T4 future disk spill).

## 2. Design Goals

- **Separation of concerns**: algorithm ≠ OS resource ≠ composition
- **Extensibility**: T4 disk spill can be added by implementing a new segment type in `c2-segment` and a new strategy in `c2-mempool` — no changes to `c2-alloc`
- **Minimal public surface**: `c2-mempool` is the only crate Python talks to (via FFI)
- **Zero runtime cost**: no additional indirection or allocation vs. current monolith
- **Python clarity**: `c_two.mem.MemPool` replaces `c_two.buddy.BuddyPoolHandle`

## 3. Dependency Graph

```
c2-alloc          c2-segment
(pure algorithm)  (OS resources: SHM, spinlock)
    │                  │
    └────────┬─────────┘
             ▼
         c2-mempool
    (composition: T1–T4 strategy)
             │
             ▼
         c2-ffi (PyO3 bindings)
            │
            ▼
    c_two._native (Python extension)
        ├── c_two.mem      (MemPool, PoolConfig, PoolAlloc, PoolStats)
        └── c_two.relay    (NativeRelay — unchanged)

Separate concern chain (unchanged):
c2-wire → c2-ipc → c2-relay → c2-ffi
```

## 4. Rust Crate Details

### 4.1 `c2-alloc` — Pure Allocation Algorithms

**Purpose:** Buddy allocator math operating on raw memory pointers. Zero awareness of SHM, files, or any OS resource.

**Dependencies:** `libc` (for `AtomicU32`, `AtomicU64` types only — no syscalls)

**Source files (migrated from c2-buddy):**

| File | Origin | Content |
|------|--------|---------|
| `src/buddy.rs` | `allocator.rs` | `BuddyAllocator`, `Allocation`, `SegmentHeader`, constants |
| `src/bitmap.rs` | `bitmap.rs` | `LevelBitmap`, `total_bitmap_bytes()`, `num_levels()` |
| `src/lib.rs` | new | Re-exports |

**Public API:**

```rust
// Core allocator — operates on raw *mut u8, no SHM knowledge
pub struct BuddyAllocator { /* ... */ }

impl BuddyAllocator {
    pub unsafe fn init(base: *mut u8, total_size: usize, min_block: usize) -> Self;
    pub unsafe fn attach(base: *mut u8, total_size: usize) -> Result<Self, &'static str>;
    pub fn alloc(&self, size: usize) -> Option<Allocation>;
    pub fn free(&self, offset: u32, level: u16) -> Result<(), String>;
    pub fn data_ptr(&self, offset: u32) -> *mut u8;
    pub fn data_size(&self) -> usize;
    pub fn min_block(&self) -> usize;
    pub fn alloc_count(&self) -> u32;
    pub fn free_bytes(&self) -> u64;
    pub fn size_to_level(&self, actual_size: usize) -> Option<usize>;
    pub fn required_shm_size(min_data_capacity: usize, min_block: usize) -> usize;
}

pub struct Allocation {
    pub offset: u32,
    pub actual_size: u32,
    pub level: u16,
}

// Header layout for buddy-managed memory region
#[repr(C)]
pub struct SegmentHeader { /* magic, version, sizes, spinlock, counters */ }
```

**Design notes:**
- `SegmentHeader` stays here because it defines the buddy algorithm's on-disk/on-memory format
- `spinlock` field in header is a raw `u32` — the spinlock *implementation* lives in `c2-segment`
- Future allocators (slab, ring buffer) would be added as new files in this crate

### 4.2 `c2-segment` — OS Resource Lifecycle

**Purpose:** Manage POSIX shared memory regions and cross-process synchronization primitives. No allocation logic — just create/open/unlink raw memory regions.

**Dependencies:** `libc`

**Source files (migrated from c2-buddy):**

| File | Origin | Content |
|------|--------|---------|
| `src/shm.rs` | `segment.rs` | `ShmRegion` — POSIX SHM lifecycle |
| `src/spinlock.rs` | `spinlock.rs` | `ShmSpinlock`, `is_process_alive()` |
| `src/lib.rs` | new | Re-exports |

**Public API:**

```rust
/// Raw POSIX SHM region — no allocator attached.
/// Owner creates and unlinks; non-owner opens and detaches.
pub struct ShmRegion {
    name: String,
    base: *mut u8,
    size: usize,
    is_owner: bool,
}

impl ShmRegion {
    /// Create a new SHM region (shm_open + ftruncate + mmap)
    pub fn create(name: &str, size: usize) -> Result<Self, String>;
    /// Open an existing SHM region (shm_open + mmap)
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String>;
    /// Accessors
    pub fn name(&self) -> &str;
    pub fn base_ptr(&self) -> *mut u8;
    pub fn size(&self) -> usize;
    pub fn is_owner(&self) -> bool;
    pub unsafe fn as_slice(&self) -> &[u8];
    pub unsafe fn as_slice_mut(&self) -> &mut [u8];
}

impl Drop for ShmRegion {
    // Owner: munmap + shm_unlink
    // Non-owner: munmap only
}

/// Cross-process spinlock stored in shared memory
pub struct ShmSpinlock { /* unchanged API */ }
pub fn is_process_alive(pid: u32) -> bool;
```

**Key difference from current `ShmSegment`:**
- Current `ShmSegment` = SHM lifecycle + `BuddyAllocator` instance (tightly coupled)
- New `ShmRegion` = **only** SHM lifecycle — returns raw `(*mut u8, size)` to caller
- Composition layer (`c2-mempool`) wraps `ShmRegion` + `BuddyAllocator` into `BuddySegment`

**Future extensibility:**
- `file.rs` — `FileRegion`: mmap a file for T4 disk spill (same interface as `ShmRegion`)
- `Region` trait: unify `ShmRegion` and `FileRegion` behind common interface

### 4.3 `c2-mempool` — Composition Layer (T1–T4 Strategy)

**Purpose:** Combine allocators and OS resources into a unified memory pool with tiered fallback. This is the **only** crate exposed to Python via FFI.

**Dependencies:** `c2-alloc`, `c2-segment`, `libc`

**Source files (migrated + restructured from c2-buddy):**

| File | Origin | Content |
|------|--------|---------|
| `src/buddy_segment.rs` | `segment.rs` (partial) | `BuddySegment = ShmRegion + BuddyAllocator` |
| `src/dedicated.rs` | `segment.rs` (partial) | `DedicatedSegment = ShmRegion` (no allocator) |
| `src/pool.rs` | `pool.rs` | `MemPool` (renamed from `BuddyPool`) |
| `src/config.rs` | `pool.rs` (inline) | `PoolConfig`, `PoolAllocation`, `PoolStats` |
| `src/lib.rs` | new | Re-exports limited public surface |

**Public API (limited surface):**

```rust
/// Buddy-backed SHM segment (T1 allocations)
pub struct BuddySegment {
    region: ShmRegion,
    allocator: BuddyAllocator,
}

impl BuddySegment {
    pub fn create(name: &str, size: usize, min_block: usize) -> Result<Self, String>;
    pub fn open(name: &str, expected_size: usize) -> Result<Self, String>;
    pub fn allocator(&self) -> &BuddyAllocator;
    pub fn region(&self) -> &ShmRegion;
}

/// Dedicated SHM segment (T3 large allocations)
pub struct DedicatedSegment {
    region: ShmRegion,
}

impl DedicatedSegment {
    pub fn create(name: &str, size: usize) -> Result<Self, String>;
    pub fn open(name: &str, size: usize) -> Result<Self, String>;
    pub fn data_ptr(&self) -> *mut u8;
    pub fn size(&self) -> usize;
}

/// Unified memory pool — the main entry point
pub struct MemPool { /* replaces BuddyPool */ }

impl MemPool {
    pub fn new(config: PoolConfig) -> Self;
    pub fn new_with_prefix(config: PoolConfig, prefix: String) -> Self;
    pub fn alloc(&mut self, size: usize) -> Result<PoolAllocation, String>;
    pub fn free(&mut self, alloc: &PoolAllocation) -> Result<(), String>;
    pub fn free_at(&mut self, seg_idx: u32, offset: u32, size: u32, is_dedicated: bool) -> Result<(), String>;
    pub fn data_ptr(&self, alloc: &PoolAllocation) -> Result<*mut u8, String>;
    pub fn data_ptr_at(&self, seg_idx: u32, offset: u32, is_dedicated: bool) -> Result<*mut u8, String>;
    pub fn seg_data_info(&self, seg_idx: u32) -> Result<(*mut u8, usize), String>;
    pub fn stats(&self) -> PoolStats;
    pub fn gc_buddy(&mut self) -> usize;
    pub fn gc_dedicated(&mut self);
    pub fn segment_count(&self) -> usize;
    pub fn segment_name(&self, idx: usize) -> Option<&str>;
    pub fn dedicated_name(&self, idx: u32) -> Option<&str>;
    pub fn prefix(&self) -> &str;
    pub fn derive_segment_name(&self, seg_idx: u32, is_dedicated: bool) -> String;
    pub fn open_segment(&mut self, name: &str, size: usize) -> Result<usize, String>;
    pub fn open_dedicated(&mut self, name: &str, size: usize) -> Result<u32, String>;
    pub fn destroy(&mut self);
    pub fn validate_config(config: &PoolConfig) -> Result<(), String>;
    pub fn cleanup_stale_segments(prefix: &str) -> usize;
}

// Data types (moved from pool.rs inline definitions to config.rs)
pub struct PoolConfig { /* unchanged fields */ }
pub struct PoolAllocation { /* unchanged fields */ }
pub struct PoolStats { /* unchanged fields */ }
```

**Internal allocation flow (unchanged behavior):**
1. **T1 buddy**: Try `alloc_buddy()` in existing segments
2. **T2 expand**: If T1 fails and `segment_count < max_segments`, create new `BuddySegment`
3. **T3 dedicated**: If size > segment_size/2, create `DedicatedSegment`
4. **T4 disk spill**: (future) Create `FileRegion`-backed segment

## 5. FFI Layer Changes (`c2-ffi`)

### 5.1 File Renames

| Old | New |
|-----|-----|
| `buddy_ffi.rs` | `mem_ffi.rs` |
| `PyBuddyPoolHandle` | `PyMemPool` (Python name: `"MemPool"`) |

### 5.2 Dependency Update

```toml
# c2-ffi/Cargo.toml
[dependencies]
c2-mempool = { path = "../c2-mempool" }  # was c2-buddy
c2-relay = { path = "../c2-relay" }
c2-ipc = { path = "../c2-ipc" }
```

### 5.3 Class Rename

```rust
// mem_ffi.rs
#[pyclass(name = "MemPool")]
pub struct PyMemPool {
    pool: Arc<RwLock<MemPool>>,  // was BuddyPool
}
```

Other Python-facing types (`PoolConfig`, `PoolAlloc`, `PoolStats`) keep their names — they are already generic enough.

## 6. Python Layer Changes

### 6.1 Module Rename

| Old | New |
|-----|-----|
| `c_two/buddy/__init__.py` | `c_two/mem/__init__.py` |
| `c_two.buddy.BuddyPoolHandle` | `c_two.mem.MemPool` |
| `c_two/transport/ipc/buddy.py` | `c_two/transport/ipc/shm_frame.py` |

### 6.2 `c_two/mem/__init__.py`

```python
from c_two._native import (
    MemPool,           # was BuddyPoolHandle
    PoolAlloc,
    PoolConfig,
    PoolStats,
    cleanup_stale_shm,
)

__all__ = ["MemPool", "PoolAlloc", "PoolConfig", "PoolStats", "cleanup_stale_shm"]
```

### 6.3 Backward Compatibility Shim

`c_two/buddy/__init__.py` becomes a deprecation shim:

```python
"""Deprecated: use c_two.mem instead."""
import warnings
from c_two.mem import MemPool as BuddyPoolHandle, PoolAlloc, PoolConfig, PoolStats, cleanup_stale_shm

warnings.warn(
    "c_two.buddy is deprecated, use c_two.mem instead",
    DeprecationWarning,
    stacklevel=2,
)

__all__ = ["BuddyPoolHandle", "PoolAlloc", "PoolConfig", "PoolStats", "cleanup_stale_shm"]
```

### 6.4 Transport Wire Codec Rename

`c_two/transport/ipc/buddy.py` → `c_two/transport/ipc/shm_frame.py`

All function names stay the same (they already use `buddy_` prefix which still makes sense as the frame format name). Internal imports across the transport package are updated.

### 6.5 Internal Import Updates

All files importing from the old paths must be updated:

| Pattern | Files affected |
|---------|---------------|
| `from c_two.buddy import` | `transport/client/core.py`, `transport/server/handshake.py`, `transport/ipc/buddy.py` |
| `from .buddy import` | `transport/ipc/__init__.py` |
| `BuddyPoolHandle` references | `transport/client/core.py`, `transport/server/handshake.py`, `transport/server/connection.py` |

## 7. Migration Strategy

### Phase 1: Rust Crate Split (no Python-visible changes)

1. Create `c2-alloc/` with `buddy.rs` + `bitmap.rs` extracted from `c2-buddy/`
2. Create `c2-segment/` with `shm.rs` + `spinlock.rs` extracted from `c2-buddy/`
3. Create `c2-mempool/` with `pool.rs` + `buddy_segment.rs` + `dedicated.rs` + `config.rs`
4. Update `c2-mempool` to depend on `c2-alloc` + `c2-segment`
5. Update `c2-ffi` to depend on `c2-mempool` instead of `c2-buddy`
6. Run `cargo test` in workspace — all existing Rust tests must pass
7. Delete `c2-buddy/` crate

### Phase 2: FFI + Python Rename

1. Rename `buddy_ffi.rs` → `mem_ffi.rs`, `PyBuddyPoolHandle` → `PyMemPool`
2. Create `c_two/mem/__init__.py`, convert `c_two/buddy/` to deprecation shim
3. Rename `transport/ipc/buddy.py` → `transport/ipc/shm_frame.py`
4. Update all internal imports
5. Run full Python test suite — 727+ tests must pass
6. Run examples to verify

### Phase 3: Cleanup (separate commit)

1. Update documentation references
2. Update `pyproject.toml` if needed
3. Update roadmap

## 8. Risk Assessment

| Risk | Mitigation |
|------|------------|
| Rust test breakage during split | Each file moves 1:1 — code stays identical, only module paths change |
| Python import breakage | `c_two.buddy` shim preserves backward compat during transition |
| `uv sync` rebuild issues | cache-keys already configured for `.rs` file monitoring |
| FFI type name change (`MemPool`) | Internal only — no public API users yet (pre-0.3.0) |
| Circular deps in new crate graph | c2-alloc and c2-segment are independent; only c2-mempool depends on both — no cycles |

## 9. Files Affected Summary

**New Rust crates:**
- `src/c_two/_native/c2-alloc/` (2 source files, ~910 lines)
- `src/c_two/_native/c2-segment/` (2 source files, ~640 lines)
- `src/c_two/_native/c2-mempool/` (4 source files, ~925 lines)

**Deleted:**
- `src/c_two/_native/c2-buddy/` (entire crate)

**Modified Rust:**
- `src/c_two/_native/Cargo.toml` (workspace members)
- `src/c_two/_native/c2-ffi/Cargo.toml` (dependency)
- `src/c_two/_native/c2-ffi/src/lib.rs` (module name)
- `src/c_two/_native/c2-ffi/src/buddy_ffi.rs` → `mem_ffi.rs`

**New Python:**
- `src/c_two/mem/__init__.py`
- `src/c_two/transport/ipc/shm_frame.py`

**Modified Python:**
- `src/c_two/buddy/__init__.py` (deprecation shim)
- `src/c_two/transport/ipc/__init__.py` (import path)
- `src/c_two/transport/client/core.py` (import path + type name)
- `src/c_two/transport/server/handshake.py` (import path + type name)
- `src/c_two/transport/server/connection.py` (type annotation)
- Any other files referencing `BuddyPoolHandle` or `from c_two.buddy`

