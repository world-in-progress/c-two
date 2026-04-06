# Dedicated Segment Lifecycle — SHM Header Flag Design

**Date:** 2026-04-02
**Branch:** `fix/ipc-perf-regression`
**Status:** Approved

## Problem

Dedicated SHM segments use a time-based GC delay (`freed_at + 5s`) to avoid
reclaiming segments before the remote peer finishes reading. This is fragile:
too short → SIGBUS; too long → memory waste. No industrial IPC framework uses
time assumptions for lifecycle management.

## Solution: SHM Header Flag (iceoryx2-inspired)

Place an `AtomicU32 read_done` flag in the first 64 bytes of every dedicated
segment. The reader sets `read_done = 1` after consuming the data. The creator's
GC checks this flag instead of a timer.

## Research Basis

| Framework   | Signal Mechanism        | Time Assumptions |
|-------------|------------------------|-----------------|
| iceoryx2    | AtomicU32 in SHM       | None            |
| DPDK        | uint16_t refcnt        | None            |
| Chromium    | FD refcount (kernel)   | None            |
| Cap'n Proto | Status flag in SHM     | None            |
| **C-Two**   | `freed_at` + timer     | **5 seconds**   |

All 6 researched frameworks use shared-visible state, not timers.

## Memory Layout

```
+----------+------+----------------------------------------------+
| Offset   | Size | Description                                  |
+----------+------+----------------------------------------------+
| 0        | 4B   | read_done: AtomicU32 (0 = unread, 1 = done)  |
| 4        | 60B  | _reserved (cache-line padding to 64B)         |
| 64       | N    | payload data                                  |
+----------+------+----------------------------------------------+
```

Constant: `DEDICATED_HEADER_SIZE = 64`.

`data_ptr()` returns `base_ptr + 64`. `size()` returns data capacity
(region size − 64). The header is transparent to all callers.

## Lifecycle Flow

### Request Direction (Client → Server)

```
Client (creator)                     Server (reader)
────────────────                     ───────────────
1. alloc_dedicated()
   create(size) → header read_done=0
2. Write data at data_ptr()
3. Send frame (BuddyPayload)
                                     4. ensure_dedicated_segment()
                                        → open(size)
                                     5. Read data at data_ptr()
                                     6. free_peer_block()
                                        → mark_read_done()  [AtomicU32=1]
                                        → freed_at = now     [reader local]
                                     7. Reader GC → munmap (no shm_unlink)
8. Receive response
9. free_dedicated()
   → freed_at = now [creator local]
10. gc_dedicated()
    → is_read_done()? ✓ → drop → shm_unlink
```

### Response Direction (Server → Client)

Symmetric: Server is creator, Client is reader.

## DedicatedEntry Changes

```rust
struct DedicatedEntry {
    segment: DedicatedSegment,
    freed_at: Option<Instant>,  // When local side marked as freed
    is_creator: bool,           // NEW: true = created, false = opened
}
```

- `alloc_dedicated()`: sets `is_creator: true`
- `open_dedicated_at()`: sets `is_creator: false`

## GC Logic

```rust
fn gc_dedicated(&mut self) {
    let crash_timeout = Duration::from_secs_f64(
        self.config.dedicated_crash_timeout_secs  // default 60.0
    );
    let now = Instant::now();
    let to_remove: Vec<u32> = self.dedicated.iter()
        .filter_map(|(&idx, entry)| {
            if let Some(freed_at) = entry.freed_at {
                if entry.is_creator {
                    // Primary: peer set read_done in SHM
                    if entry.segment.is_read_done() {
                        return Some(idx);
                    }
                    // Fallback: peer likely crashed
                    if now.duration_since(freed_at) >= crash_timeout {
                        return Some(idx);
                    }
                } else {
                    // Reader: just munmap, no shm_unlink needed
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

## GC Trigger Points

`gc_dedicated()` runs at the start of:

| Method      | Why                                        |
|-------------|--------------------------------------------|
| `alloc()`   | Every allocation scans for reclaimable     |
| `free()`    | Every local free scans                     |
| `free_at()` | Every remote free scans                    |

Cost: iterate HashMap (0–4 entries) + AtomicU32::load per entry = nanoseconds.

## DedicatedSegment API Changes

```rust
// dedicated.rs

pub const DEDICATED_HEADER_SIZE: usize = 64;

pub struct DedicatedSegment { region: ShmRegion }

impl DedicatedSegment {
    pub fn create(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::create(name, aligned)?;
        // Initialize header
        let hdr = region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(0, Ordering::Release); }
        Ok(Self { region })
    }

    pub fn open(name: &str, data_size: usize) -> Result<Self, String> {
        let total = data_size + DEDICATED_HEADER_SIZE;
        let aligned = page_align(total);
        let region = ShmRegion::open(name, aligned)?;
        Ok(Self { region })
    }

    pub fn data_ptr(&self) -> *mut u8 {
        unsafe { self.region.base_ptr().add(DEDICATED_HEADER_SIZE) }
    }

    pub fn data_size(&self) -> usize {
        self.region.size() - DEDICATED_HEADER_SIZE
    }

    pub fn mark_read_done(&self) {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).store(1, Ordering::Release); }
    }

    pub fn is_read_done(&self) -> bool {
        let hdr = self.region.base_ptr() as *const AtomicU32;
        unsafe { (*hdr).load(Ordering::Acquire) == 1 }
    }
}
```

## Config Changes

```rust
// In PoolConfig:
// REMOVE: dedicated_gc_delay_secs: f64    (was 5.0)
// ADD:    dedicated_crash_timeout_secs: f64  (default 60.0)
```

The crash timeout is a safety net for peer process crashes, not a
correctness mechanism. Normal GC is driven entirely by the SHM flag.

## Safety Analysis

| Scenario         | Behavior                                              |
|------------------|-------------------------------------------------------|
| Normal           | Reader sets flag → creator GC reclaims immediately    |
| Reader crash     | Flag stays 0 → 60s crash timeout reclaims            |
| Creator crash    | Reader munmap is safe; orphan cleaned by PID scan     |
| Concurrent R/W   | AtomicU32 Release/Acquire provides cross-process fence|
| ShmRegion drop   | Owner: shm_unlink + munmap; Non-owner: munmap only   |

## Files Modified

| File                         | Change                                       |
|------------------------------|----------------------------------------------|
| `c2-mem/src/dedicated.rs`    | Header layout, data_ptr offset, flag methods |
| `c2-mem/src/pool.rs`         | DedicatedEntry.is_creator, GC logic, triggers|
| `c2-mem/src/config.rs`       | Rename config field                          |
| `c2-ffi/src/mem_ffi.rs`      | Update config FFI if field renamed           |
| `c2-server/src/connection.rs`| No change (free_peer_block already calls free_at) |
| `c2-ipc/src/client.rs`       | No change (read_and_free already calls free_at)   |

## What Does NOT Change

- Wire protocol (BuddyPayload fields unchanged)
- Buddy allocator (already uses SHM atomics)
- Python layer (header transparent to buffer protocol)
- Frame encoding/decoding
- Handshake protocol
