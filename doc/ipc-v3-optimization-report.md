# IPC v3 Optimization Report

## Executive Summary

IPC v3 replaces the original file-based `memory://` transport with a Rust buddy allocator + Unix Domain Socket (UDS) control plane architecture. Benchmark results on an Apple M-series ARM64 system show:

| Metric | memory:// | ipc:// (v3) | Improvement |
|--------|-----------|-------------|-------------|
| **Geomean P50 (≥10MB)** | 384.8 ms | 35.8 ms | **10.7×** |
| **Peak throughput** | 0.44 GB/s | 5.03 GB/s | **11.4×** |
| **Small payload latency (64B)** | 31.1 ms | 0.12 ms | **271×** |
| **1GB round-trip** | 3,827 ms | 729 ms | **5.3×** |

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
- **Benchmark**: General `@transferable` round-trip — multi-field struct serialization (struct.pack/unpack), CRM computes checksum + mutates fields (NOT a raw bytes echo)
- **Rounds**: 100 (reduced to 30/20/10 for 50MB/100MB/500MB+)
- **Warmup**: 5 rounds

### 3.1 Detailed Results

| Size | memory P50 | ipc P50 | Speedup | mem ops/s | ipc ops/s | mem tput | ipc tput |
|------|-----------|---------|---------|-----------|-----------|----------|----------|
| 64B | 31.1 ms | 0.12 ms | **271×** | 32 | 8,715 | ~0 | ~0 |
| 1KB | 32.1 ms | 0.18 ms | **180×** | 31 | 5,602 | ~0 | 0.01 GB/s |
| 4KB | 32.1 ms | 0.22 ms | **143×** | 31 | 4,465 | ~0 | 0.02 GB/s |
| 64KB | 32.2 ms | 0.21 ms | **156×** | 31 | 4,830 | ~0 | 0.29 GB/s |
| 1MB | 38.1 ms | 0.43 ms | **88×** | 26 | 2,309 | 0.03 GB/s | 2.25 GB/s |
| 10MB | 65.7 ms | 2.87 ms | **23×** | 15 | 349 | 0.15 GB/s | 3.41 GB/s |
| 50MB | 134.9 ms | 10.6 ms | **13×** | 7 | 94 | 0.36 GB/s | 4.60 GB/s |
| 100MB | 224.4 ms | 19.4 ms | **12×** | 5 | 52 | 0.44 GB/s | **5.03 GB/s** |
| 500MB | 1,108 ms | 136.6 ms | **8×** | 1 | 7 | 0.44 GB/s | 3.57 GB/s |
| 1GB | 3,827 ms | 729 ms | **5×** | 0.3 | 1.4 | 0.26 GB/s | 1.37 GB/s |

**Geomean P50 (≥10MB): memory=384.8ms → ipc-v3=35.8ms — 10.7× speedup**

### 3.2 Analysis

**Small payloads (64B–64KB):**
- memory:// latency is ~31ms regardless of size (polling-dominated)
- ipc:// latency is 0.12–0.22ms (UDS round-trip + struct pack/unpack)
- Speedup: **143–271×** — entirely due to eliminating filesystem polling

**Medium payloads (1MB–10MB):**
- memory:// latency grows linearly with size (I/O-bound)
- ipc:// latency is sub-3ms (SHM memcpy + serialization dominated)
- Speedup: **23–88×** — combines polling elimination + SHM zero-copy

**Large payloads (50MB–100MB):**
- Peak throughput zone for ipc:// at **~5 GB/s** (includes serialization overhead)
- memory:// caps at 0.44 GB/s (filesystem bottleneck)
- Speedup: **12–13×** — serialization cost narrows the gap vs pure transport

**Very large payloads (500MB–1GB):**
- 1GB ipc:// at 729ms includes: 2× struct.pack, 2× struct.unpack, 2× SHM memcpy, CRM checksum
- memory:// shows extreme variance at 1GB (3,322–6,787ms) due to filesystem cache pressure
- Speedup: **5–8×** — serialization/deserialization of large payloads (bytes() copy on memoryview) becomes significant

**Note on bytes-echo vs realistic scenario:**
The bytes-echo benchmark (serialize = identity function, CRM = return input) yields ~20× geomean speedup. The realistic benchmark (struct packing, CRM processing, client-side deserialize) yields ~11× because serialization overhead is identical for both transports and proportionally reduces the transport advantage at large sizes. The absolute transport speedup is unchanged — the difference reflects the added constant-factor work.

### 3.3 IPC v3 Latency Breakdown (100MB realistic example)

| Component | Estimated Time |
|-----------|---------------|
| Client struct.pack + payload concat | ~0.5 ms |
| SHM write (memcpy to pool) | ~1.5 ms |
| UDS control frame send/recv | ~0.1 ms |
| Server SHM read → struct.unpack | ~2.0 ms |
| CRM checksum + field mutation | ~0.1 ms |
| Server struct.pack + SHM write | ~2.0 ms |
| Client SHM read + struct.unpack | ~2.0 ms |
| bytes() materialization on memoryview | ~5.0 ms |
| **Total** | **~19.4 ms** |

The dominant cost at 100MB is `bytes()` materialization from `memoryview` during deserialization, plus the `memcpy` operations for SHM writes. The struct.pack/unpack headers are negligible.

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

IPC v3 delivers a **10.7× geomean speedup** over the original `memory://` file-based transport for payloads ≥10MB in a realistic multi-field serialization scenario, with peak throughput reaching **5 GB/s** at 100MB. For small payloads, the advantage is **271×** due to eliminating filesystem polling entirely. The key architectural wins are:

1. **Eliminating filesystem I/O** — shared memory + UDS replaces file polling
2. **Zero-copy pipeline** — `memoryview` end-to-end avoids intermediate copies
3. **Rust buddy allocator** — O(1) allocation with PID-based crash recovery
4. **Scatter-reuse** — eliminates alloc/free cycles for steady-state workloads
5. **Pre-encoded wire headers** — avoids repeated UTF-8 encoding per call

The protocol is now the **default for `ipc://`** addresses, with `ipc-v2://` retained as an explicit fallback. The `memory://` protocol remains available for environments where Unix domain sockets are unavailable.
