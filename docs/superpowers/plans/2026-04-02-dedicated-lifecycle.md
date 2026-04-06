# Dedicated Segment SHM Header Flag — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the fragile time-based dedicated segment GC (`freed_at + 5s`) with an `AtomicU32 read_done` flag in SHM, enabling deterministic cross-process lifecycle management.

**Architecture:** Add a 64-byte header to every dedicated SHM segment containing an `AtomicU32 read_done` flag. Reader sets flag after consuming data; creator GC checks flag instead of timer. Crash timeout (60s) as safety net.

**Tech Stack:** Rust (c2-mem crate), PyO3 (c2-ffi crate)

**Spec:** `docs/superpowers/specs/2026-04-02-dedicated-lifecycle-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `c2-mem/src/dedicated.rs` | Modify | Add 64B header, shift data_ptr, add flag methods |
| `c2-mem/src/config.rs` | Modify | Rename config field |
| `c2-mem/src/pool.rs` | Modify | DedicatedEntry.is_creator, new GC logic, expanded triggers |
| `c2-ffi/src/mem_ffi.rs` | Modify | Update PyPoolConfig field name |

All paths relative to `src/c_two/_native/`.

---

### Task 1: Add SHM header to DedicatedSegment

**Files:**
- Modify: `c2-mem/src/dedicated.rs` (entire file, 43 lines)

- [ ] **Step 1: Write the test for header flag round-trip**

Add to the bottom of `c2-mem/src/dedicated.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_flag_lifecycle() {
        let name = format!("/c2ded_test_{}", std::process::id());
        let data_size = 4096;

        // Creator: flag starts at 0
        let creator = DedicatedSegment::create(&name, data_size).unwrap();
        assert!(!creator.is_read_done());

        // Write data through data_ptr (offset by header)
        unsafe { *creator.data_ptr() = 0xAB; }

        // Reader: open same segment, read data
        let reader = DedicatedSegment::open(&name, data_size).unwrap();
        assert_eq!(unsafe { *reader.data_ptr() }, 0xAB);
        assert!(!reader.is_read_done());

        // Reader signals done
        reader.mark_read_done();
        assert!(reader.is_read_done());

        // Creator sees the flag (same SHM pages)
        assert!(creator.is_read_done());

        // Verify data_size excludes header
        assert!(creator.data_size() >= data_size);

        drop(reader);
        drop(creator); // owner unlinks
    }

    #[test]
    fn test_data_ptr_offset() {
        let name = format!("/c2ded_off_{}", std::process::id());
        let seg = DedicatedSegment::create(&name, 8192).unwrap();

        // data_ptr must be HEADER_SIZE bytes after base
        let base = seg.region.base_ptr() as usize;
        let data = seg.data_ptr() as usize;
        assert_eq!(data - base, DEDICATED_HEADER_SIZE);

        drop(seg);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src/c_two/_native && cargo test -p c2-mem -- dedicated::tests --nocapture 2>&1 | head -30`

Expected: compilation error — `is_read_done`, `mark_read_done`, `data_size`, `DEDICATED_HEADER_SIZE` not found.

- [ ] **Step 3: Implement the header in DedicatedSegment**

Replace the entire `c2-mem/src/dedicated.rs` with:

```rust
//! Dedicated SHM segment for oversized allocations (T3).
//!
//! These bypass the buddy allocator — the entire region serves
//! one allocation.  A 64-byte header at the start of the SHM region
//! carries an `AtomicU32 read_done` flag for cross-process lifecycle
//! coordination (iceoryx2-inspired).

use crate::segment::ShmRegion;
use std::sync::atomic::{AtomicU32, Ordering};

/// Header size in bytes (cache-line aligned).
pub const DEDICATED_HEADER_SIZE: usize = 64;

const PAGE_SIZE: usize = 4096;

fn page_align(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// A dedicated SHM segment (no buddy allocator).
///
/// Layout: `[64B header | payload data]`
/// Header byte 0..4: `read_done: AtomicU32` (0 = unread, 1 = done)
/// Header byte 4..64: reserved padding
pub struct DedicatedSegment {
    region: ShmRegion,
}

unsafe impl Send for DedicatedSegment {}
unsafe impl Sync for DedicatedSegment {}

impl DedicatedSegment {
    /// Create a dedicated segment for a single large allocation.
    ///
    /// `data_size` is the usable payload capacity.  The actual SHM region
    /// is `page_align(data_size + DEDICATED_HEADER_SIZE)`.
    pub fn create(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::create(name, aligned)?;

        // Initialize header: read_done = 0
        let hdr = region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(0, Ordering::Release); }

        Ok(Self { region })
    }

    /// Open an existing dedicated segment.
    ///
    /// `data_size` is the expected payload capacity (used for size validation).
    pub fn open(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::open(name, aligned)?;
        Ok(Self { region })
    }

    /// Pointer to the start of the payload data region (past the header).
    pub fn data_ptr(&self) -> *mut u8 {
        unsafe { self.region.base_ptr().add(DEDICATED_HEADER_SIZE) }
    }

    /// Usable data capacity in bytes (excludes the 64-byte header).
    pub fn data_size(&self) -> usize {
        self.region.size() - DEDICATED_HEADER_SIZE
    }

    /// Total SHM region size including header.
    pub fn size(&self) -> usize {
        self.region.size()
    }

    pub fn name(&self) -> &str {
        self.region.name()
    }

    /// Signal that the reader has finished consuming the payload.
    ///
    /// Called by the non-owner (reader) side after reading is complete.
    /// The owner (creator) polls `is_read_done()` during GC to decide
    /// when it is safe to `shm_unlink`.
    pub fn mark_read_done(&self) {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(1, Ordering::Release); }
    }

    /// Check whether the reader has signalled completion.
    pub fn is_read_done(&self) -> bool {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).load(Ordering::Acquire) == 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_flag_lifecycle() {
        let name = format!("/c2ded_test_{}", std::process::id());
        let data_size = 4096;

        let creator = DedicatedSegment::create(&name, data_size).unwrap();
        assert!(!creator.is_read_done());

        unsafe { *creator.data_ptr() = 0xAB; }

        let reader = DedicatedSegment::open(&name, data_size).unwrap();
        assert_eq!(unsafe { *reader.data_ptr() }, 0xAB);
        assert!(!reader.is_read_done());

        reader.mark_read_done();
        assert!(reader.is_read_done());
        assert!(creator.is_read_done());

        assert!(creator.data_size() >= data_size);

        drop(reader);
        drop(creator);
    }

    #[test]
    fn test_data_ptr_offset() {
        let name = format!("/c2ded_off_{}", std::process::id());
        let seg = DedicatedSegment::create(&name, 8192).unwrap();

        let base = seg.region.base_ptr() as usize;
        let data = seg.data_ptr() as usize;
        assert_eq!(data - base, DEDICATED_HEADER_SIZE);

        drop(seg);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src/c_two/_native && cargo test -p c2-mem -- dedicated::tests --nocapture 2>&1 | tail -10`

Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/c2-mem/src/dedicated.rs
git commit -m "feat(c2-mem): add 64B SHM header to DedicatedSegment

Add AtomicU32 read_done flag at offset 0 of dedicated segments.
data_ptr() now returns base_ptr + 64. Reader calls mark_read_done()
after consuming data; creator polls is_read_done() for GC.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: Rename config field + update FFI

**Files:**
- Modify: `c2-mem/src/config.rs:15` — rename field
- Modify: `c2-mem/src/pool.rs:275-280` — update GC reference
- Modify: `c2-ffi/src/mem_ffi.rs:31,46,55,70-71,81,103` — rename PyPoolConfig field

- [ ] **Step 1: Rename in config.rs**

In `c2-mem/src/config.rs`, replace:

```rust
    /// Delay before reclaiming empty dedicated segments (seconds).
    pub dedicated_gc_delay_secs: f64,
```

with:

```rust
    /// Crash-recovery timeout for dedicated segments (seconds).
    /// Normal GC uses SHM read_done flag; this is a safety net for peer crashes.
    pub dedicated_crash_timeout_secs: f64,
```

In the `Default` impl, replace:

```rust
            dedicated_gc_delay_secs: 5.0,
```

with:

```rust
            dedicated_crash_timeout_secs: 60.0,
```

- [ ] **Step 2: Update pool.rs reference**

In `c2-mem/src/pool.rs`, replace the GC delay read (line ~275):

```rust
        let secs = self.config.dedicated_gc_delay_secs;
```

with:

```rust
        let secs = self.config.dedicated_crash_timeout_secs;
```

- [ ] **Step 3: Update FFI in mem_ffi.rs**

In `c2-ffi/src/mem_ffi.rs`, make 4 replacements:

1. Field declaration (line 31): `dedicated_gc_delay_secs` → `dedicated_crash_timeout_secs`
2. Default value (line 46): `dedicated_gc_delay_secs = 5.0` → `dedicated_crash_timeout_secs = 60.0`
3. Constructor param (line 55): `dedicated_gc_delay_secs: f64` → `dedicated_crash_timeout_secs: f64`
4. Validation (lines 70-71):

Replace:
```rust
        if dedicated_gc_delay_secs.is_nan() {
            return Err(PyValueError::new_err("dedicated_gc_delay_secs must not be NaN"));
        }
```

with:
```rust
        if dedicated_crash_timeout_secs.is_nan() {
            return Err(PyValueError::new_err("dedicated_crash_timeout_secs must not be NaN"));
        }
```

5. Struct init (line 81): `dedicated_gc_delay_secs,` → `dedicated_crash_timeout_secs,`
6. From impl (line 103): `dedicated_gc_delay_secs: py.dedicated_gc_delay_secs,` → `dedicated_crash_timeout_secs: py.dedicated_crash_timeout_secs,`

- [ ] **Step 4: Update test configs in pool.rs**

In `c2-mem/src/pool.rs`, tests use `dedicated_gc_delay_secs: 0.0`. Replace all occurrences:

```rust
            dedicated_gc_delay_secs: 0.0,
```

with:

```rust
            dedicated_crash_timeout_secs: 0.0,
```

There are 2 occurrences: `small_config()` (line ~785) and `test_segment_expansion()` (line ~825).

- [ ] **Step 5: Verify compilation**

Run: `cd src/c_two/_native && cargo check -p c2-mem -p c2-ffi 2>&1 | tail -10`

Expected: compiles cleanly (warnings OK, no errors).

- [ ] **Step 6: Run tests**

Run: `cd src/c_two/_native && cargo test -p c2-mem --no-default-features 2>&1 | tail -10`

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/c_two/_native/c2-mem/src/config.rs \
        src/c_two/_native/c2-mem/src/pool.rs \
        src/c_two/_native/c2-ffi/src/mem_ffi.rs
git commit -m "refactor(c2-mem): rename dedicated_gc_delay_secs → dedicated_crash_timeout_secs

Default changes from 5.0s to 60.0s. The crash timeout is a safety net
for peer crashes; normal GC is driven by the SHM read_done flag.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 3: Rewrite pool GC and add is_creator tracking

**Files:**
- Modify: `c2-mem/src/pool.rs:28-32` — DedicatedEntry struct
- Modify: `c2-mem/src/pool.rs:273-298` — gc_dedicated()
- Modify: `c2-mem/src/pool.rs:374-392` — open_dedicated_at()
- Modify: `c2-mem/src/pool.rs:683-729` — alloc_dedicated()
- Modify: `c2-mem/src/pool.rs:747-751` — free_dedicated()
- Modify: `c2-mem/src/pool.rs:176-183` — free()
- Modify: `c2-mem/src/pool.rs:394-401` — free_at()

- [ ] **Step 1: Write tests for new GC behavior**

Add at the end of the `mod tests` block in `c2-mem/src/pool.rs` (before the closing `}`):

```rust
    #[test]
    fn test_dedicated_gc_flag_based() {
        // Creator allocates dedicated, then frees — GC should NOT
        // remove it until read_done is set.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 60.0,
            ..PoolConfig::default()
        };
        let mut pool = test_pool(config);

        let a = pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);
        let seg_idx = a.seg_idx;

        // Creator frees → freed_at set, but read_done still 0
        pool.free(&a).unwrap();

        // GC should NOT remove (read_done == 0, timeout not reached)
        pool.gc_dedicated();
        assert!(pool.dedicated.contains_key(&seg_idx));

        // Simulate reader: set read_done via the segment
        pool.dedicated.get(&seg_idx).unwrap().segment.mark_read_done();

        // Now GC should remove
        pool.gc_dedicated();
        assert!(!pool.dedicated.contains_key(&seg_idx));
    }

    #[test]
    fn test_dedicated_reader_gc_immediate() {
        // Reader side (opened, not created) should be GC'd immediately.
        let config = PoolConfig {
            segment_size: 32 * 1024,
            min_block_size: 4096,
            max_segments: 1,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 60.0,
            ..PoolConfig::default()
        };
        let mut creator_pool = test_pool(config.clone());
        let a = creator_pool.alloc(64 * 1024).unwrap();
        assert!(a.is_dedicated);
        let seg_name = creator_pool.dedicated.get(&a.seg_idx).unwrap()
            .segment.name().to_string();
        let data_size = 64 * 1024;

        // Open in a second pool (simulates reader/peer side)
        let mut reader_pool = test_pool(config);
        reader_pool.open_dedicated_at(a.seg_idx, &seg_name, data_size).unwrap();
        assert!(reader_pool.dedicated.contains_key(&a.seg_idx));

        // Reader frees → should mark read_done + be immediately removable
        reader_pool.free_at(a.seg_idx, 0, data_size as u32, true).unwrap();
        reader_pool.gc_dedicated();
        assert!(!reader_pool.dedicated.contains_key(&a.seg_idx));

        // Creator sees read_done
        assert!(creator_pool.dedicated.get(&a.seg_idx).unwrap()
            .segment.is_read_done());

        creator_pool.free(&a).unwrap();
        creator_pool.gc_dedicated();
        assert!(!creator_pool.dedicated.contains_key(&a.seg_idx));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src/c_two/_native && cargo test -p c2-mem -- test_dedicated_gc_flag 2>&1 | tail -15`

Expected: FAIL — `is_creator` field missing, new GC logic not yet implemented.

- [ ] **Step 3: Update DedicatedEntry struct**

In `c2-mem/src/pool.rs`, replace:

```rust
/// Tracking info for a dedicated segment.
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,
}
```

with:

```rust
/// Tracking info for a dedicated segment.
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,
    /// `true` if this process created the segment (owns shm_unlink).
    /// `false` if opened from a peer (reader side, munmap-only on drop).
    is_creator: bool,
}
```

- [ ] **Step 4: Update alloc_dedicated() to set is_creator: true**

In `c2-mem/src/pool.rs`, in `alloc_dedicated()`, replace:

```rust
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
            },
        );
```

with:

```rust
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
                is_creator: true,
            },
        );
```

- [ ] **Step 5: Update open_dedicated_at() to set is_creator: false**

In `c2-mem/src/pool.rs`, in `open_dedicated_at()`, replace:

```rust
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
            },
        );
```

with:

```rust
        self.dedicated.insert(
            idx,
            DedicatedEntry {
                segment: seg,
                freed_at: None,
                is_creator: false,
            },
        );
```

- [ ] **Step 6: Update free_dedicated() to mark read_done for readers**

In `c2-mem/src/pool.rs`, replace:

```rust
    fn free_dedicated(&mut self, seg_idx: u32) {
        if let Some(entry) = self.dedicated.get_mut(&seg_idx) {
            entry.freed_at = Some(Instant::now());
        }
    }
```

with:

```rust
    fn free_dedicated(&mut self, seg_idx: u32) {
        if let Some(entry) = self.dedicated.get_mut(&seg_idx) {
            if !entry.is_creator {
                entry.segment.mark_read_done();
            }
            entry.freed_at = Some(Instant::now());
        }
    }
```

- [ ] **Step 7: Rewrite gc_dedicated() with flag-based logic**

In `c2-mem/src/pool.rs`, replace the entire `gc_dedicated()` method:

```rust
    pub fn gc_dedicated(&mut self) {
        let secs = self.config.dedicated_crash_timeout_secs;
        let delay = if secs < 0.0 {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_secs_f64(secs)
        };
        let now = Instant::now();
        let to_remove: Vec<u32> = self
            .dedicated
            .iter()
            .filter_map(|(&idx, entry)| {
                if let Some(freed_at) = entry.freed_at {
                    if now.duration_since(freed_at) >= delay {
                        return Some(idx);
                    }
                }
                None
            })
            .collect();

        for idx in to_remove {
            self.dedicated.remove(&idx); // Drop triggers munmap + shm_unlink.
        }
    }
```

with:

```rust
    /// Run garbage collection on freed dedicated segments.
    ///
    /// For creator entries: reclaim when the reader has set `read_done = 1`
    /// in the SHM header, or after a crash-recovery timeout.
    /// For reader entries: reclaim immediately (munmap only, no shm_unlink).
    pub fn gc_dedicated(&mut self) {
        let crash_timeout = {
            let secs = self.config.dedicated_crash_timeout_secs;
            if secs < 0.0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs_f64(secs)
            }
        };
        let now = Instant::now();
        let to_remove: Vec<u32> = self
            .dedicated
            .iter()
            .filter_map(|(&idx, entry)| {
                if let Some(freed_at) = entry.freed_at {
                    if entry.is_creator {
                        // Primary: peer set read_done in SHM header
                        if entry.segment.is_read_done() {
                            return Some(idx);
                        }
                        // Fallback: peer likely crashed — safety net
                        if now.duration_since(freed_at) >= crash_timeout {
                            return Some(idx);
                        }
                    } else {
                        // Reader side: can drop immediately (munmap only)
                        return Some(idx);
                    }
                }
                None
            })
            .collect();

        for idx in to_remove {
            self.dedicated.remove(&idx);
        }
    }
```

- [ ] **Step 8: Add gc_dedicated() calls to alloc(), free(), and free_at()**

In `c2-mem/src/pool.rs`, add `self.gc_dedicated();` at the very start of three methods:

**`alloc()`** — add as first line of method body (before `if size == 0`):

```rust
    pub fn alloc(&mut self, size: usize) -> Result<PoolAllocation, String> {
        self.gc_dedicated();
        if size == 0 {
```

**`free()`** — add as first line (before `if alloc.is_dedicated`):

```rust
    pub fn free(&mut self, alloc: &PoolAllocation) -> Result<(), String> {
        self.gc_dedicated();
        if alloc.is_dedicated {
```

**`free_at()`** — add as first line (before `if is_dedicated`):

```rust
    pub fn free_at(&mut self, seg_idx: u32, offset: u32, data_size: u32, is_dedicated: bool) -> Result<FreeResult, String> {
        self.gc_dedicated();
        if is_dedicated {
```

(Keep all other code in these methods unchanged.)

- [ ] **Step 9: Run all c2-mem tests**

Run: `cd src/c_two/_native && cargo test -p c2-mem --no-default-features 2>&1 | tail -15`

Expected: all tests pass including the 2 new flag-based tests.

- [ ] **Step 10: Commit**

```bash
git add src/c_two/_native/c2-mem/src/pool.rs
git commit -m "feat(c2-mem): flag-based dedicated GC with is_creator tracking

- DedicatedEntry gains is_creator field
- gc_dedicated() checks SHM read_done flag (creator) or drops
  immediately (reader)
- free_dedicated() calls mark_read_done() for reader entries
- gc_dedicated() now runs on alloc(), free(), and free_at()
- Crash-recovery timeout (60s) as safety net

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 4: Full integration test + Python rebuild

**Files:**
- No new files — verify end-to-end

- [ ] **Step 1: Run complete Rust test suite**

Run: `cd src/c_two/_native && cargo test -p c2-mem -p c2-wire --no-default-features 2>&1 | tail -15`

Expected: all tests pass.

- [ ] **Step 2: Cargo check all crates**

Run: `cd src/c_two/_native && cargo check -p c2-mem -p c2-wire -p c2-ipc -p c2-server -p c2-ffi 2>&1 | tail -15`

Expected: compiles with no errors.

- [ ] **Step 3: Rebuild Python extension**

Run: `cd /Users/soku/Desktop/codespace/WorldInProgress/c-two && uv sync --reinstall-package c-two 2>&1 | tail -10`

Expected: maturin rebuilds successfully.

- [ ] **Step 4: Run Python test suite**

Run: `cd /Users/soku/Desktop/codespace/WorldInProgress/c-two && C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 2>&1 | tail -20`

Expected: all tests pass.

- [ ] **Step 5: Commit (if any fixups needed)**

If Python tests revealed issues requiring small fixups, commit them:

```bash
git add -u
git commit -m "fix(c2-mem): fixups from integration testing

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```
