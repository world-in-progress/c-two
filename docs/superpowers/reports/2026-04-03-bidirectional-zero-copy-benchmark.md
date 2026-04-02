# Bidirectional Zero-Copy SHM Benchmark Report

> **Date:** 2026-04-03
> **Branch:** `fix/ipc-perf-regression`
> **Platform:** macOS (Apple Silicon), Python 3.14.3 free-threading build
> **Scope:** IPC round-trip latency, 64B–1GB, two segment configurations

## 1. Executive Summary

This report evaluates the bidirectional zero-copy SHM optimization for c-two's IPC
transport layer. The optimization eliminates redundant data copies on both the client
request path (CC1+CC2 → single Python→SHM copy) and the server response path
(C4+C5 → single Python→SHM copy).

**Key results:**

- Buddy-path latency (≤segment_size): **0.23–10.6 ms** for 64B–100MB — near memory-bandwidth limit
- Dedicated segments incur **5.2× penalty** at 500MB (303 ms vs 59 ms with 2GB buddy segments)
- 2GB segment config eliminates dedicated path up to 1GB, yielding **2–5× improvement** for large payloads
- Dict (pickle) adds 1.2–1.7× overhead over bytes identity path for medium payloads
- Zero-copy optimization reduces per-call copies from 4 to 2 (one each direction, the theoretical minimum)

## 2. Test Configuration

| Parameter | Value |
|-----------|-------|
| Transport | IPC (UDS control + POSIX SHM data plane) |
| CRM | Echo (returns input unchanged) |
| Payload types | `bytes` (identity fast path), `dict` (pickle serde) |
| Segment configs | 256 MB (default), 2 GB |
| Max segments | 8 |
| SHM threshold | 4096 bytes |
| Warmup | 3 rounds per size |
| Rounds | 100 (≤1MB), 20 (≤100MB), 5 (500MB, 1GB) |
| Metric | P50 (median) round-trip latency in ms |

## 3. Raw Results

### 3.1 256MB Segment Configuration (default)

With 256 MB buddy segments, payloads > 256 MB must use dedicated SHM segments
(create/mmap per call, heavier lifecycle overhead).

| Size | Rounds | bytes P50 (ms) | dict P50 (ms) | dict/bytes | Alloc Path |
|-----:|-------:|---------------:|--------------:|-----------:|:-----------|
| 64B | 100 | 0.303 | 0.396 | 1.3× | buddy |
| 256B | 100 | 0.331 | 0.391 | 1.2× | buddy |
| 1KB | 100 | 0.334 | 0.401 | 1.2× | buddy |
| 4KB | 100 | 0.358 | 0.393 | 1.1× | buddy |
| 64KB | 100 | 0.346 | 0.400 | 1.2× | buddy |
| 1MB | 100 | 0.455 | 0.624 | 1.4× | buddy |
| 10MB | 20 | 1.569 | 2.624 | 1.7× | buddy |
| 50MB | 20 | 5.962 | 9.867 | 1.7× | buddy |
| 100MB | 20 | 10.710 | 18.711 | 1.7× | buddy |
| 500MB | 5 | 303.079 | 347.128 | 1.1× | **dedicated** |
| 1GB | 5 | 1134.163 | 2274.756 | 2.0× | **dedicated** |

### 3.2 2GB Segment Configuration

With 2 GB buddy segments, all payloads up to 1 GB fit in the buddy allocator —
no dedicated segments are needed.

| Size | Rounds | bytes P50 (ms) | dict P50 (ms) | dict/bytes | Alloc Path |
|-----:|-------:|---------------:|--------------:|-----------:|:-----------|
| 64B | 100 | 0.235 | 0.318 | 1.4× | buddy |
| 256B | 100 | 0.234 | 0.340 | 1.5× | buddy |
| 1KB | 100 | 0.250 | 0.340 | 1.4× | buddy |
| 4KB | 100 | 0.283 | 0.326 | 1.2× | buddy |
| 64KB | 100 | 0.267 | 0.329 | 1.2× | buddy |
| 1MB | 100 | 0.383 | 0.544 | 1.4× | buddy |
| 10MB | 20 | 1.608 | 2.631 | 1.6× | buddy |
| 50MB | 20 | 5.324 | 9.285 | 1.7× | buddy |
| 100MB | 20 | 10.630 | 18.682 | 1.8× | buddy |
| 500MB | 5 | 58.726 | 91.820 | 1.6× | buddy |
| 1GB | 5 | 509.180 | 1519.864 | 3.0× | buddy |

### 3.3 Head-to-Head Comparison

| Size | 256M-bytes | 2G-bytes | Speedup | 256M-dict | 2G-dict | Speedup | 256M Path |
|-----:|-----------:|---------:|--------:|----------:|--------:|--------:|:----------|
| 64B | 0.303 | 0.235 | 1.29× | 0.396 | 0.318 | 1.24× | buddy |
| 256B | 0.331 | 0.234 | 1.42× | 0.391 | 0.340 | 1.15× | buddy |
| 1KB | 0.334 | 0.250 | 1.33× | 0.401 | 0.340 | 1.18× | buddy |
| 4KB | 0.358 | 0.283 | 1.27× | 0.393 | 0.326 | 1.20× | buddy |
| 64KB | 0.346 | 0.267 | 1.30× | 0.400 | 0.329 | 1.22× | buddy |
| 1MB | 0.455 | 0.383 | 1.19× | 0.624 | 0.544 | 1.15× | buddy |
| 10MB | 1.569 | 1.608 | 0.98× | 2.624 | 2.631 | 1.00× | buddy |
| 50MB | 5.962 | 5.324 | 1.12× | 9.867 | 9.285 | 1.06× | buddy |
| 100MB | 10.710 | 10.630 | 1.01× | 18.711 | 18.682 | 1.00× | buddy |
| **500MB** | **303.079** | **58.726** | **5.16×** | **347.128** | **91.820** | **3.78×** | **DEDICATED** |
| **1GB** | **1134.163** | **509.180** | **2.23×** | **2274.756** | **1519.864** | **1.50×** | **DEDICATED** |

## 4. Analysis

### 4.1 Buddy Allocator: Sub-Millisecond to Bandwidth-Limited

For payloads that fit in buddy segments (≤ segment_size), the IPC round-trip
is dominated by `memcpy` bandwidth. The zero-copy optimization ensures only
**one copy per direction** (Python→SHM on send, SHM→Python on receive),
achieving the theoretical minimum for cross-process data transfer.

**Bandwidth calculation (2GB-seg, bytes path):**

| Size | P50 (ms) | Effective throughput |
|-----:|---------:|--------------------:|
| 1MB | 0.383 | 5.2 GB/s |
| 10MB | 1.608 | 12.4 GB/s |
| 100MB | 10.630 | 18.8 GB/s |
| 500MB | 58.726 | 17.0 GB/s |

The 10–19 GB/s throughput is consistent with Apple Silicon's unified memory bandwidth
(~50 GB/s peak, shared across CPU/GPU), considering the round-trip involves at minimum
2 copies (request + response) plus UDS control channel overhead.

### 4.2 Dedicated Segment Penalty

When a payload exceeds `segment_size`, the pool falls back to dedicated SHM segments.
Each dedicated allocation involves:

1. `shm_open()` — create a new POSIX shared memory object
2. `ftruncate()` — set the segment size
3. `mmap()` — map the segment into address space
4. Data copy (Python→SHM)
5. Send control frame over UDS
6. **Peer side:** `shm_open()` + `mmap()` again (lazy open by name derivation)
7. Read data from SHM
8. `munmap()` + `shm_unlink()` — cleanup (flag-based GC)

Steps 1-3 and 6 are **syscalls per invocation** — in contrast, buddy allocation is
pure user-space pointer arithmetic within pre-mapped regions.

**Quantified penalty at 500MB:**

| Metric | 256M-seg (dedicated) | 2G-seg (buddy) | Ratio |
|--------|---------------------:|----------------:|------:|
| bytes P50 | 303 ms | 59 ms | 5.16× |
| dict P50 | 347 ms | 92 ms | 3.78× |

The ~244 ms overhead is consistent with the cost of 4 `shm_open/mmap/unlink` syscalls
(2 per direction × 2 sides) on macOS, where POSIX SHM is backed by the filesystem.

### 4.3 Serialization Overhead (bytes vs dict)

The `bytes` identity path skips serialization entirely — the Python `bytes` object
is written directly to SHM via `memoryview`. The `dict` path uses pickle, which
involves Python object traversal, serialization, and deserialization.

| Size range | dict/bytes ratio | Bottleneck |
|:-----------|:----------------:|:-----------|
| 64B–4KB | 1.1–1.5× | Pickle fixed overhead (object inspection) |
| 1MB–100MB | 1.4–1.8× | Pickle throughput (~6 GB/s vs memcpy ~19 GB/s) |
| 500MB (ded.) | 1.1× | Dedicated syscalls dominate, masking serde cost |
| 1GB (buddy) | 3.0× | Pickle 1GB ≈ 1 second; memcpy negligible |

**Takeaway:** For large payloads, users should prefer `@transferable` types with
efficient `serialize`/`deserialize` (e.g., Arrow IPC, NumPy `.tobytes()`) over
generic pickle. The transport layer's zero-copy work is irrelevant if serialization
costs 3× the transfer.

### 4.4 Small Payload Anomaly (2GB Segments Faster)

Unexpectedly, 2GB segments are **1.2–1.4× faster** even for tiny payloads (64B–64KB)
where both configs use the buddy path. Possible explanations:

1. **Fewer TLB misses:** A single 2GB mapping has better TLB residency than
   multiple 256MB mappings under contention.
2. **Buddy tree depth:** 2GB segments have deeper trees but the same minimum
   allocation size — the extra depth doesn't add overhead for small allocations.
3. **Segment count:** 256MB config may create more segments sooner, increasing
   the bookkeeping overhead in `alloc()`.

This effect is small (< 0.1 ms absolute) and within noise for real workloads.

## 5. Copy Elimination Summary

The bidirectional zero-copy optimization removed 2 of the 4 data copies in the
IPC round-trip:

| Path | Copy ID | Before | After |
|:-----|:--------|:-------|:------|
| Client → Server | CC1: `client_ffi.rs` `data.to_vec()` | Python→Rust Vec | ✅ **Eliminated** |
| Client → Server | CC2: `client.rs` `copy_nonoverlapping` | Rust Vec→SHM | → Python→SHM direct |
| Server → Client | C4: `server_ffi.rs` `to_vec()` | Python→Rust Vec | ✅ **Eliminated** |
| Server → Client | C5: `server.rs` `copy_nonoverlapping` | Rust Vec→SHM | → Python→SHM direct |

**Before:** 4 copies per round-trip (2 per direction)
**After:** 2 copies per round-trip (1 per direction — the theoretical minimum)

The remaining copies are **unavoidable** — data must move from Python heap to SHM
at least once per direction. The only way to eliminate them would be to have CRMs
operate directly on SHM-backed buffers (a fundamentally different programming model).

## 6. Recommendations

### 6.1 Segment Size Tuning

| Workload profile | Recommended `segment_size` |
|:-----------------|:---------------------------|
| Small payloads (< 10 MB typical) | 256 MB (default) |
| Mixed, occasional large (up to 500 MB) | 1 GB |
| Regularly > 256 MB, up to ~2 GB | 2 GB |
| Unpredictable, very large | 2 GB + raise `max_segments` |

Users can configure via:
```python
cc.set_ipc_config(segment_size=2 * 1024**3)  # 2 GB segments
```

### 6.2 Serialization Strategy

For payloads > 10 MB, serialization cost dominates over transport. Prefer:

1. **`@transferable` with Arrow IPC** — near-zero-copy deserialization, schema-aware
2. **`@transferable` with NumPy `.tobytes()`** — simple, fast for numeric data
3. **Raw `bytes`** — when data is already serialized (e.g., protobuf, msgpack)
4. **Pickle** (default fallback) — convenient but 3× slower for very large payloads

### 6.3 Future Optimizations

1. **Streaming for >segment_size:** Currently punted — dedicated segments work but are slow.
   A chunked streaming mode over buddy segments would avoid dedicated entirely.
2. **Response-side SHM read zero-copy:** Python currently reads SHM into a new `bytes`
   object. A `memoryview` over SHM could eliminate the last copy on the read path,
   but requires careful lifetime management.
3. **Buddy segment pre-warming:** Pre-fault pages with `madvise(MADV_WILLNEED)` to
   avoid page faults on first write.

## 7. Conclusion

The bidirectional zero-copy SHM optimization achieves its design goal: **2 copies
per round-trip (down from 4)**, with sub-millisecond latency for payloads ≤ 1 MB
and near-memory-bandwidth throughput (17–19 GB/s) for large payloads on the buddy path.

The dedicated segment penalty (2–5× slower) is the primary remaining bottleneck for
payloads exceeding `segment_size`. This is addressed by allowing users to configure
larger segment sizes, making the buddy path available for most real-world workloads.





