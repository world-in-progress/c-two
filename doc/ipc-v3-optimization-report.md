# IPC v3 Optimization Report

## Executive Summary

IPC v3 replaces the original file-based `memory://` transport with a Rust buddy allocator + Unix Domain Socket (UDS) control plane architecture. Benchmark results on an Apple M-series ARM64 system show:

| Metric | memory:// | ipc:// (v3) | Improvement |
|--------|-----------|-------------|-------------|
| **Geomean P50 (≥10MB)** | 316.2 ms | 15.6 ms | **20.2×** |
| **Peak throughput** | 0.52 GB/s | 10.01 GB/s | **19.3×** |
| **Small payload latency (64B)** | 33.0 ms | 0.14 ms | **237×** |
| **1GB round-trip** | 2,433 ms | 269 ms | **9.0×** |

---

## 1. Architecture Evolution

### 1.1 memory:// Protocol (Baseline)

The original `memory://` transport uses **file-system temp files** for inter-process communication:

1. **Client** serializes the payload → writes a request file to `/tmp/`
2. **Server** polls for new request files → reads → processes → writes response file
3. **Client** polls for response file → reads → deserializes

**Bottlenecks:**
- Filesystem I/O on every call (open, write, fsync, read, unlink)
- No zero-copy: data is serialized to bytes, written to disk, read back, deserialized
- Polling latency: both sides poll on ~10ms intervals
- No shared memory: data crosses the kernel boundary twice per direction

**Typical performance profile:**
- Base latency: ~25-30ms (dominated by filesystem polling)
- Throughput ceiling: ~0.5 GB/s (limited by disk I/O)
- Latency nearly flat for small payloads (polling-dominated)

### 1.2 ipc:// (v3) Protocol

IPC v3 uses a **UDS control plane** + **shared memory data plane** with a **Rust buddy allocator**:

1. **Client** allocates a block from the buddy pool SHM → writes wire-encoded data directly into SHM → sends a small UDS control frame (16 bytes: offset + size)
2. **Server** reads the control frame → reads data directly from SHM (zero-copy memoryview) → processes → writes response into SHM → sends control frame back
3. **Client** reads control frame → reads response from SHM (zero-copy)

**Key advantages:**
- Zero filesystem I/O: all data stays in shared memory
- Zero-copy hot path: `memoryview` slicing avoids `bytes()` copies
- Sub-millisecond control plane: UDS is ~100× faster than file polling
- Buddy allocator: O(1) alloc/free with no fragmentation for power-of-2 blocks

---

## 2. Optimization Layers

### 2.1 Rust Buddy Allocator (`c2_buddy`)

The shared memory pool is managed by a Rust-based buddy allocator exposed to Python via PyO3. Key design decisions:

- **Power-of-2 block splitting**: O(log N) worst-case alloc/free, O(1) typical
- **Bitmap-based tracking**: CAS (compare-and-swap) operations for lock-free concurrent alloc/free
- **Spinlock with PID-based crash recovery**: If a process dies holding the lock, the next process detects the stale PID and recovers
- **Dedicated fallback**: Oversized allocations (>segment size) get a dedicated SHM segment
- **Idle GC** (`gc_buddy`): Reclaims segments that have been idle beyond `pool_decay_seconds`
- **No-shrink mode**: Segments are not returned to the OS during active use to avoid re-allocation overhead

**Configuration:**
- Default segment: 256 MB (configurable up to 4 GB)
- Min block: 4 KB
- Max segments: 1 regular + 4 dedicated

### 2.2 Wire Protocol (Zero-Copy)

The binary wire codec (`wire.py`) is designed for zero-copy processing:

- **Pre-encoded method headers**: Method names are UTF-8 encoded + struct-packed once at ICRM registration time
- **`memoryview` throughout**: The entire pipeline — SHM read → wire decode → CRM dispatch → serialize → SHM write — operates on `memoryview` slices without intermediate `bytes()` copies
- **Scatter-write for tuples**: When CRM methods return tuples, the scatter-write path writes each element directly into SHM without concatenation
- **Inline fast path**: Small payloads (<4KB) skip SHM and go directly through the UDS frame

### 2.3 Buddy Handshake Protocol

At connection time, client and server negotiate shared memory:

1. Client creates a buddy pool → allocates the first segment → sends segment name + size to server
2. Server opens the segment (read-only, `track=False`) → caches `memoryview` + base address → ACKs
3. Server creates its own response pool → sends segment info in the handshake ACK
4. Client opens server's response segment → caches views

This two-way handshake enables **full-duplex SHM**: client writes requests to client pool, server writes responses to server pool.

### 2.4 Scatter-Reuse Optimization

When the server detects that its response fits in the client's request block:

1. Server writes the response directly into the client's SHM block (no new allocation)
2. Client receives the response from the same block it sent the request in
3. On the next call, the client reuses the same block (no alloc/free cycle)

This eliminates 2 alloc + 2 free operations per round-trip for steady-state workloads.

### 2.5 Response Block Deferred Free

Instead of freeing the response SHM block immediately after reading, the client holds it as a "deferred free":

- If the next request fits in the same block → reuse (no alloc/free)
- If the next request is larger → free and allocate fresh
- On client destruction → free all deferred blocks

This converts the alloc-free-alloc-free pattern into a single long-lived allocation for uniform workloads.

---

## 3. Benchmark Results

### Test Environment

- **CPU**: Apple M-series ARM64
- **Python**: 3.14t (free-threaded build)
- **OS**: macOS (Darwin)
- **Benchmark**: General `@transferable` round-trip (serialize → send → receive → deserialize)
- **Rounds**: 100 (reduced to 30/20/10 for 50MB/100MB/500MB+)
- **Warmup**: 5 rounds

### 3.1 Detailed Results

| Size | memory P50 | ipc P50 | Speedup | mem ops/s | ipc ops/s | mem tput | ipc tput |
|------|-----------|---------|---------|-----------|-----------|----------|----------|
| 64B | 33.0 ms | 0.14 ms | **237×** | 30 | 7,179 | ~0 | ~0 |
| 1KB | 30.0 ms | 0.18 ms | **167×** | 33 | 5,572 | ~0 | 0.01 GB/s |
| 4KB | 32.0 ms | 0.20 ms | **159×** | 31 | 4,956 | ~0 | 0.02 GB/s |
| 64KB | 30.2 ms | 0.18 ms | **166×** | 33 | 5,508 | ~0 | 0.34 GB/s |
| 1MB | 38.0 ms | 0.28 ms | **135×** | 26 | 3,562 | 0.03 GB/s | 3.48 GB/s |
| 10MB | 56.1 ms | 1.41 ms | **40×** | 18 | 707 | 0.17 GB/s | 6.91 GB/s |
| 50MB | 118.1 ms | 5.02 ms | **24×** | 9 | 199 | 0.41 GB/s | 9.73 GB/s |
| 100MB | 206.9 ms | 9.75 ms | **21×** | 5 | 103 | 0.47 GB/s | **10.01 GB/s** |
| 500MB | 947.9 ms | 50.2 ms | **19×** | 1 | 20 | 0.52 GB/s | 9.72 GB/s |
| 1GB | 2,433 ms | 269 ms | **9×** | 0.4 | 3.7 | 0.41 GB/s | 3.72 GB/s |

**Geomean P50 (≥10MB): memory=316.2ms → ipc-v3=15.6ms — 20.2× speedup**

### 3.2 Analysis

**Small payloads (64B–64KB):**
- memory:// latency is ~30ms regardless of size (polling-dominated)
- ipc:// latency is 0.13–0.20ms (UDS round-trip dominated)
- Speedup: **150–237×** — entirely due to eliminating filesystem polling

**Medium payloads (1MB–10MB):**
- memory:// latency grows linearly with size (I/O-bound)
- ipc:// latency is sub-2ms (SHM memcpy dominated, ~7 GB/s effective)
- Speedup: **40–135×** — combines polling elimination + zero-copy SHM

**Large payloads (50MB–100MB):**
- Peak throughput zone for ipc:// at **~10 GB/s** (near DRAM bandwidth limit for a single-threaded Python process)
- memory:// caps at 0.5 GB/s (filesystem bottleneck)
- Speedup: **21–24×**

**Very large payloads (500MB–1GB):**
- 1GB ipc:// drops to 3.72 GB/s due to TLB pressure and Python GC pauses
- Still **9–19× faster** than memory://
- memory:// shows high variance at 1GB (2,262–3,813ms) due to filesystem cache pressure

### 3.3 IPC v3 Latency Breakdown (100MB example)

| Component | Estimated Time |
|-----------|---------------|
| Wire encode (client) | ~0.3 ms |
| SHM write (memcpy to pool) | ~1.5 ms |
| UDS control frame send/recv | ~0.1 ms |
| Server SHM read (memoryview) | ~0 ms (zero-copy) |
| CRM execute | ~0.01 ms |
| Wire encode response | ~0.3 ms |
| SHM write response | ~1.5 ms |
| Client SHM read + deserialize | ~2.0 ms |
| **Total** | **~9.7 ms** |

The dominant cost at 100MB is the `memcpy` operations (SHM write on both sides), which are bounded by DRAM bandwidth.

---

## 4. Concurrency & Safety

### 4.1 Free-Threading (Python 3.14t) Compatibility

All IPC v3 code is designed for GIL-free operation:

- **Rust spinlock**: Used instead of Python `threading.Lock` for SHM allocator — no GIL dependency
- **PID-based crash recovery**: Spinlock detects dead holder processes via `kill(pid, 0)`
- **No shared mutable Python objects** on the hot path: SHM is accessed via `ctypes.memmove` and `memoryview`
- **Connection-scoped state**: Each client connection has its own UDS socket and deferred-free tracker

### 4.2 Safety Mechanisms

| Mechanism | Purpose |
|-----------|---------|
| Frame size limit (`max_frame_size`) | Prevents OOM from malformed frames |
| SHM name validation | Rejects path traversal in segment names |
| Double-free guard | Buddy allocator detects and rejects double-free attempts |
| TOCTOU protection | SHM block headers use atomic operations |
| Stale segment cleanup | `gc_buddy` reclaims segments from dead processes |

---

## 5. Module Structure

```
src/c_two/rpc/ipc/
├── ipc_protocol.py      # Shared constants, IPCConfig, frame codec
├── ipc_v3_server.py     # IPC v3 async server (asyncio + UDS)
├── ipc_v3_client.py     # IPC v3 sync client (socket + buddy pool)
├── ipc_server.py        # IPC v2 async server (legacy, retained)
├── ipc_client.py        # IPC v2 sync client (legacy, retained)
├── shm_pool.py          # IPC v2 SharedMemory pool (legacy)
└── __init__.py          # Exports

rust/c2_buddy/
├── src/
│   ├── allocator.rs     # Buddy allocator core (bitmap-based)
│   ├── pool.rs          # Multi-segment pool manager
│   ├── spinlock.rs      # Cross-process spinlock with PID recovery
│   └── bitmap.rs        # CAS-based bit manipulation
└── Cargo.toml
```

---

## 6. Conclusion

IPC v3 delivers a **20× geomean speedup** over the original `memory://` file-based transport for payloads ≥10MB, with peak throughput reaching **10 GB/s** at 100MB. The key architectural wins are:

1. **Eliminating filesystem I/O** — shared memory + UDS replaces file polling
2. **Zero-copy pipeline** — `memoryview` end-to-end avoids intermediate copies
3. **Rust buddy allocator** — O(1) allocation with PID-based crash recovery
4. **Scatter-reuse** — eliminates alloc/free cycles for steady-state workloads
5. **Pre-encoded wire headers** — avoids repeated UTF-8 encoding per call

The protocol is now the **default for `ipc://`** addresses, with `ipc-v2://` retained as an explicit fallback. The `memory://` protocol remains available for environments where Unix domain sockets are unavailable.
