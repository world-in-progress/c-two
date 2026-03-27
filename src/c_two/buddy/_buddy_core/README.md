# c2_buddy — Rust Core

Cross-process buddy allocator over POSIX shared memory for C-Two IPC v3.

> **This is the Rust implementation.** Python access is via `from c_two.buddy import BuddyPoolHandle, PoolConfig`.
> The Rust code is compiled automatically by `uv sync` (maturin build backend).

## Overview

`c2_buddy` provides zero-syscall dynamic memory allocation within pre-mapped POSIX shared memory segments. It uses a hierarchical power-of-two buddy system with atomic bitmap operations for cross-process safety. The allocator is designed for the C-Two framework's IPC v3 transport, where a server and client share SHM segments for high-throughput, low-latency data transfer.

Key features:

- **Buddy allocation**: O(log N) alloc/free with automatic splitting and merging
- **Cross-process safe**: Atomic bitmap CAS + SHM spinlock with crash recovery
- **Multi-segment pool**: Auto-expansion up to configurable limits, with idle GC
- **Dedicated segments**: Fallback for oversized allocations exceeding segment capacity
- **Python bindings**: Full PyO3 API (`BuddyPoolHandle`) with GIL-free operation
- **Zero-copy support**: Direct pointer access (`alloc_ptr`, `data_addr`) for `ctypes.memmove` / `memoryview` integration

## Architecture

```
BuddyPool (pool.rs)
├── BuddySegment[] (segment.rs)
│   ├── ShmSegment (POSIX shm_open + mmap)
│   │   ├── SegmentHeader (allocator.rs) — 4KB page-aligned
│   │   │   ├── magic, version, sizes
│   │   │   ├── ShmSpinlock (spinlock.rs) — atomic u32
│   │   │   └── LevelBitmap[] (bitmap.rs) — AtomicU64 arrays
│   │   └── Data Region — power-of-2 sized
│   └── BuddyAllocator — alloc/free with split/merge
└── DedicatedSegment[] (segment.rs)
    └── One SHM per oversized allocation
```

## Module Reference

### `allocator.rs` — Core Buddy Allocator

The buddy allocator manages a power-of-two data region within a single SHM segment.

**Layout** (SHM segment):

```
[SegmentHeader (4KB)] [Bitmap data...] [Data region (power-of-2)]
```

Key types:

- `SegmentHeader` — `#[repr(C)]` header at offset 0: magic, version, sizes, spinlock, stats, followed by bitmap data
- `BuddyAllocator` — main allocator: `init()` for new segments, `attach()` for existing
- `Allocation` — result: `{ offset, actual_size, level }`

Key methods:

- `alloc(size) -> Option<Allocation>` — search upward from target level, split larger blocks
- `free(offset, level)` — mark free, recursively merge with buddy
- `data_ptr(offset) -> *mut u8` — raw pointer for zero-copy access
- `required_shm_size(min_data_capacity, min_block) -> usize` — compute SHM size to guarantee data capacity (avoids the round-down-to-power-of-2 trap where 1GB SHM → 512MB data)

### `bitmap.rs` — Level-wise Atomic Bitmaps

Each buddy level has a `LevelBitmap` — an array of `AtomicU64` words stored in SHM.

Bit semantics: **1 = FREE**, **0 = USED**.

- `alloc_one() -> Option<usize>` — atomic CAS scan for free block
- `free_one(block_idx)` — set bit (mark free)
- `is_free(block_idx) -> bool` — check buddy state for merge decisions

### `pool.rs` — Multi-Segment Pool

`BuddyPool` manages multiple SHM segments with a three-layer allocation fallback:

1. **Existing buddy segment** with sufficient free space
2. **New buddy segment** (if under `max_segments` limit)
3. **Dedicated segment** (one SHM per large allocation)

Configuration (`PoolConfig`):

| Field | Default | Description |
|---|---|---|
| `segment_size` | 256 MB | Size of each buddy segment |
| `min_block_size` | 4 KB | Minimum allocation granularity |
| `max_segments` | 8 | Maximum buddy segments |
| `max_dedicated_segments` | 4 | Maximum dedicated segments |
| `dedicated_gc_delay_secs` | 5.0 | GC delay before reclaiming dedicated segments |

Cross-process operations:

- `open_segment(name, size)` — attach to a remote process's buddy segment
- `free_at(seg_idx, offset, data_size, is_dedicated)` — free a remote allocation by coordinates
- `seg_data_info(seg_idx) -> (base_addr, data_size)` — get segment data region info

GC:

- `gc_buddy()` — reclaim idle buddy segments after inactivity
- `gc_dedicated()` — reclaim freed dedicated segments after delay

### `segment.rs` — SHM Segment Lifecycle

`ShmSegment` wraps POSIX `shm_open` + `mmap`:

- `create(name, size, min_block)` — create + initialize buddy allocator
- `open(name, size)` — attach to existing (validates size via `fstat`)
- Drop: `munmap` + `shm_unlink` (owner only)

`DedicatedSegment` — simplified SHM for oversized allocations (no buddy metadata).

SHM naming convention: `cc3b{pid_hex}_{counter}` (embeds creator PID for stale cleanup).

### `spinlock.rs` — Cross-Process Spinlock

`ShmSpinlock` stores the lock holder's PID in an `AtomicU32` in SHM:

- Spin with exponential backoff (spin → yield → sleep)
- **Crash recovery**: after 5M spins, check if holder process is alive (`kill(pid, 0)`); if dead, take over via CAS
- Deadlock detection: panic after 10M spins

## Python API

The crate exposes a PyO3 module (installed as `c_two.buddy._buddy_core`) with `gil_used = false`:

```python
from c_two.buddy import BuddyPoolHandle, PoolConfig, PoolAlloc, PoolStats, cleanup_stale_shm

# Create pool with custom config
config = PoolConfig(segment_size=256 * 1024 * 1024, min_block_size=4096)
pool = BuddyPoolHandle(config)

# Allocate
alloc = pool.alloc(1024 * 1024)  # 1 MB
print(alloc.seg_idx, alloc.offset, alloc.actual_size)

# Write data
pool.write(alloc, b'hello world')

# Read data
data = pool.read(alloc, 11)  # -> bytes

# Zero-copy: get raw address for ctypes.memmove
alloc, addr = pool.alloc_ptr(size)
# ctypes.memmove(addr, source_ptr, size)

# Remote process: open segment by name
seg_idx = pool.open_segment('/cc3b_deadbeef_01', 256 * 1024 * 1024)
base_addr, data_size = pool.seg_data_info(seg_idx)

# Free
pool.free(alloc)
# Or by coordinates (remote side):
pool.free_at(seg_idx=0, offset=4096, data_size=1048576, is_dedicated=False)

# Stats
stats = pool.stats()
print(stats.total_segments, stats.free_bytes, stats.fragmentation_ratio)

# Cleanup stale segments from dead processes
cleaned = cleanup_stale_shm('cc3b')

# Destroy all segments
pool.destroy()
```

### Python Classes

| Class | Description |
|---|---|
| `PoolConfig` | Pool configuration (segment_size, min_block_size, max_segments, etc.) |
| `BuddyPoolHandle` | Main pool object. Thread-safe via internal `RwLock`. |
| `PoolAlloc` | Allocation result (seg_idx, offset, actual_size, level, is_dedicated) |
| `PoolStats` | Statistics snapshot (total_segments, free_bytes, alloc_count, fragmentation_ratio) |

## Building

The Rust code is compiled automatically when running `uv sync` from the project root (maturin build backend).

```bash
# Auto-build via uv (from project root)
uv sync

# Manual build for development iteration
cd src/c_two/buddy/_buddy_core
cargo build --release

# Run Rust-only tests (no PyO3 / no Python)
cargo test --no-default-features

# Build with Python bindings manually (via maturin)
maturin develop --release
```

## Thread Safety

- `BuddyAllocator`: cross-process safe via `ShmSpinlock` (atomic CAS in SHM)
- `BuddyPool`: NOT internally synchronized — wrapped in `RwLock` by `BuddyPoolHandle`
- `BuddyPoolHandle` (Python): `RwLock<BuddyPool>` — read operations (stats, read_at) take shared lock; write operations (alloc, free) take exclusive lock
- All PyO3 methods release the GIL (`gil_used = false`)

## Performance Characteristics

- **Alloc/Free**: O(log N) where N = data_region_size / min_block_size
- **Bitmap scan**: O(1) amortized via `trailing_zeros` on `AtomicU64`
- **Cross-process sync**: Spinlock with adaptive backoff (spin → yield → sleep)
- **Typical latency**: Sub-microsecond for small allocations; SHM memoryview passthrough enables 24-37 GB/s throughput for 10MB-1GB payloads in IPC v3

## License

See repository root LICENSE file.
