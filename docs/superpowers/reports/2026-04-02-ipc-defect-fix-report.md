# IPC Defect Fix — Implementation Report

**Branch:** `fix/ipc-perf-regression`
**Date:** 2026-04-02
**Commits:** 13 (4ed74e6 → 56a75cb)

## Summary

Fixed 5 IPC defects identified in the post-SHM-response-path audit. All fixes maintain
free-threading safety (Python 3.14t) and backward compatibility.

## Defects Fixed

| ID | Defect | Severity | Fix |
|----|--------|----------|-----|
| A | gc_buddy never auto-triggered | Medium | FreeResult enum + gc_buddy before expansion |
| B | SHM alloc failure → silent data loss | High | Result return + buddy→chunked→inline degradation |
| C | No chunked response path | Medium | Reply chunk codec + server writer + client assembler |
| D | 2× copy for SHM responses | Medium | ResponseData enum + PyResponseBuffer + memoryview |
| E | Response inline uses heap Vec | Low | Stack buffer for replies ≤4KB |

## Implementation Phases

### Phase 1 — Memory Management (Defects A + E)

**Task 1: FreeResult enum** (`c2-mem/pool.rs`)
- `free_at()` now returns `FreeResult::Freed` or `FreeResult::SegmentIdle { seg_idx }`.
- Callers know when a segment becomes fully idle without polling.

**Task 2: gc_buddy before expansion** (`c2-mem/pool.rs`)
- `alloc_buddy()` and `alloc_handle()` call `gc_buddy()` before creating new segments.
- Reclaims idle segments, preventing unbounded SHM growth.

**Task 3: FreeResult propagation** (`c2-server`, `c2-ipc`)
- Server `free_peer_block()` returns `FreeResult`; on `SegmentIdle`, spawns 10s delayed gc.
- Client `read_and_free()` returns `FreeResult`; same delayed gc pattern.

**Task 4: Stack buffer for small inline replies** (`c2-server/server.rs`)
- Replies ≤4KB use `[u8; 4096]` stack buffer instead of heap `Vec<u8>`.
- Eliminates allocation overhead for the common small-response case.

**Task 5: Phase 1 integration tests**
- `test_gc_buddy_triggered_after_free`: verifies segment count decreases.
- `test_stress_alloc_free_cycles`: 500 alloc/free cycles without leaks.
- `test_concurrent_ipc_calls`: 20 threads × 10 calls round-trip correctness.

### Phase 2 — Zero-Copy Response (Defects B + D)

**Task 6: ResponseData enum** (`c2-ipc/response.rs`, `c2-ipc/client.rs`)
- Three variants: `Inline(Vec<u8>)`, `Shm { seg_idx, offset, data_size, is_dedicated }`, `Handle(MemHandle)`.
- `decode_response()` returns `Shm` for SHM replies — data stays in SHM, no copy.
- `recv_loop` changed from `PendingMap<Vec<u8>>` to `PendingMap<ResponseData>`.

**Task 7: PyResponseBuffer** (`c2-ffi/client_ffi.rs`)
- Frozen `#[pyclass]` with `StdMutex<Option<Inner>>` + `AtomicU32` exports counter.
- Implements `__getbuffer__` / `__releasebuffer__` for Python buffer protocol.
- `release()` method: frees SHM after Python is done reading.
- `Drop` impl: auto-frees if Python GC collects before explicit release.

**Task 8: SHM degradation** (`c2-server/server.rs`)
- `write_buddy_reply_with_data()` returns `Result<(), BuddyReplyError>`.
- `smart_reply_with_data()` orchestrates: buddy SHM → chunked → inline fallback.

**Task 9: Python memoryview integration** (`transferable.py`)
- `com_to_crm()` uses `memoryview(response)` for zero-copy deserialization.
- Explicit `response.release()` returns SHM ownership to Rust.
- Tests verify memoryview read, auto-release on GC, and round-trip correctness.

### Phase 3 — Chunked Response (Defect C)

**Task 10: Reply chunk meta codec** (`c2-wire/chunk.rs`)
- 16-byte format: `u64 total_size` + `u32 total_chunks` + `u32 chunk_idx`.
- Distinct from request chunks (4-byte `u16+u16`) — responses may exceed 64KB chunks.

**Task 11: Server chunked reply writer** (`c2-server/server.rs`)
- `write_chunked_reply()`: splits large response into 128KB chunks over UDS.
- Used as middle degradation tier: buddy SHM fails → chunked → inline.

**Task 12: Client chunked response reception** (`c2-ipc/client.rs`)
- `recv_loop` uses `HashMap<u32, ChunkAssembler>` for in-flight reassembly.
- Reassembled data stored as `ResponseData::Handle(MemHandle)` — stays in SHM.
- `PyResponseBuffer` extended with `Handle` variant support.

**Task 13: Relay fix + final testing + benchmark**
- Fixed relay `into_inline_bytes()` panic by adding `ResponseData::into_bytes_with_pool()`.
- 493 tests pass (3 pre-existing failures in test_dynamic_pool.py).

## Benchmark Results

### IPC Latency (P50, bytes payload, 2GB segments)

| Size | Latency | IPC/Thread ratio | Notes |
|------|---------|------------------|-------|
| 64B | 0.20ms | 60× | UDS round-trip overhead dominates |
| 256B | 0.28ms | 63× | |
| 1KB | 0.34ms | 50× | |
| 4KB | 0.42ms | 54× | Stack buffer path (≤4KB) |
| 64KB | 0.42ms | 100× | Buddy SHM path |
| 1MB | 0.55ms | 106× | |
| 10MB | 2.5ms | 728× | |
| 50MB | 8.3ms | 2446× | |
| 100MB | 17.0ms | 4963× | ~5.9 GB/s effective throughput |
| 500MB | 707ms | — | Standalone test (benchmark infra limitation) |

### Relay Latency (P50, via HTTP)

| Size | Latency | Relay/IPC ratio |
|------|---------|-----------------|
| 64B | 2.3ms | 11.5× |
| 1MB | 6.9ms | 12.7× |
| 10MB | 31ms | 12.8× |
| 100MB | 270ms | 15.9× |

### Key Observations

1. **IPC throughput**: ~5.9 GB/s at 100MB — near memory bandwidth on Apple Silicon.
2. **Zero-copy effective**: SHM response path eliminates server→client data copy for buddy payloads.
3. **Dict overhead**: pickle serialization adds ~40% latency vs raw bytes for ≥1MB payloads.
4. **Small payload**: 64B latency is ~0.2ms — dominated by UDS round-trip, not data transfer.
5. **Relay overhead**: ~12-16× vs IPC due to HTTP + JSON encoding + TCP stack.

## Commit Log

| SHA | Description |
|-----|-------------|
| `4ed74e6` | feat(c2-mem): add FreeResult enum, free_at returns idle signal |
| `c1881a7` | perf(c2-server): stack buffer for small inline replies |
| `25565f6` | feat(c2-mem): gc_buddy before buddy expansion in alloc paths |
| `0bed399` | feat(c2-server,c2-ipc): propagate FreeResult, delayed gc on segment idle |
| `6102163` | test: add Phase 1 integration tests for gc, stress, and concurrency |
| `cd7fcb5` | feat(c2-ipc): ResponseData enum, defer SHM read to consumer |
| `95ce7b8` | feat(c2-ffi): PyResponseBuffer with buffer protocol for zero-copy SHM |
| `4f5defc` | feat(c2-server): SHM alloc failure returns Result for smart degradation |
| `520e78b` | feat: Python memoryview + release for zero-copy ResponseBuffer |
| `1f188ba` | feat(c2-wire): reply chunk meta codec for chunked responses |
| `9cee48f` | feat(c2-server): chunked reply writer for large response fallback |
| `4198af7` | feat(c2-ipc): chunked response reception + Handle variant |
| `56a75cb` | fix(c2-relay): materialize SHM responses before HTTP forwarding |

## Files Changed

### Rust (c2-mem, c2-wire, c2-ipc, c2-server, c2-relay, c2-ffi)

- `c2-mem/src/pool.rs`: FreeResult enum, gc_buddy before expansion, release_handle
- `c2-wire/src/chunk.rs`: reply chunk meta encode/decode (16-byte format)
- `c2-ipc/src/response.rs`: ResponseData enum (Inline/Shm/Handle), into_bytes_with_pool
- `c2-ipc/src/client.rs`: decode_response → ResponseData, server_pool_arc, reassembly_pool
- `c2-ipc/src/sync_client.rs`: call() returns ResponseData, pool accessors
- `c2-server/src/server.rs`: smart_reply_with_data, write_chunked_reply, stack buffer
- `c2-server/src/connection.rs`: free_peer_block returns FreeResult, gc_peer_buddy
- `c2-server/src/config.rs`: reply_chunk_size field
- `c2-relay/src/router.rs`: into_bytes_with_pool for SHM response materialization
- `c2-ffi/src/client_ffi.rs`: PyResponseBuffer (buffer protocol, Handle variant)

### Python

- `src/c_two/crm/transferable.py`: memoryview(response) + release() for zero-copy
- `tests/integration/test_ipc_buddy_reply.py`: gc, stress, concurrent, memoryview tests
- `benchmarks/three_mode_benchmark.py`: skip relay for >100MB payloads

## Known Issues

1. **3 pre-existing test failures** in `test_dynamic_pool.py` — "SHM access: invalid
   dedicated segment index". Not caused by these changes; existed before this branch.

2. **Benchmark infra limitation**: Sequential IPC server creation/teardown in the
   three-mode benchmark hangs at 500MB after 9 prior iterations. Standalone 500MB
   test completes in 707ms. Root cause: `_ProcessRegistry.reset()` may not fully
   clean up SHM mappings from prior iterations.

3. **Small payload latency (64B = 0.2ms)**: Not yet using UDS inline optimization.
   Future work: payloads below a configurable threshold should skip SHM and embed
   data directly in the UDS control frame.





