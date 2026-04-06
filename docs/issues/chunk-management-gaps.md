# Chunk Management Gaps Report

**Date**: 2026-04-04
**Severity**: Medium (memory leak under abnormal conditions)
**Scope**: `c2-server`, `c2-ipc`, `c2-wire` (ChunkAssembler)

## Summary

Chunked transfer — the mechanism for sending payloads too large for a single SHM region —
has critical gaps in lifecycle management on **both** server and client sides.
Configuration fields for GC and timeout exist in `IpcConfig` but are **never used**.
Incomplete chunked transfers leak `ChunkAssembler` + `MemHandle` indefinitely.

## Affected Configuration Fields

| Field | Default | Defined In | Actually Used? |
|-------|---------|-----------|----------------|
| `chunk_gc_interval` | 100 | `c2-server` IpcConfig | ❌ Never read |
| `chunk_assembler_timeout` | 60.0s | `c2-server` IpcConfig | ❌ Never read |
| `max_total_chunks` | 512 | `c2-server` IpcConfig | ❌ Hardcoded in `ChunkAssembler` |

## Detailed Findings

### 1. Server-Side: No GC or Timeout for Incoming Chunked Requests

**Location**: `c2-server/src/connection.rs` — `assemblers: Mutex<HashMap<u64, ChunkAssembler>>`

- When a client sends a chunked request and disconnects mid-transfer, the `ChunkAssembler`
  remains in the per-connection HashMap.
- The `chunk_assembler_timeout` (60s) config value is **defined but never checked**.
- No background task scans for stale assemblers.
- Memory (MemHandle in reassembly pool) is held until the `Connection` is dropped,
  which only happens when the TCP connection fully closes.

**Impact**: A misbehaving or crashed client that sent partial chunks will hold reassembly
memory until the OS-level socket close is detected.

### 2. Server-Side: No Enforced Concurrent Chunk Limit

- `max_total_chunks` (512) is in config but **not enforced**.
- `ChunkAssembler::new()` in `c2-wire` has a hardcoded `MAX_TOTAL_CHUNKS = 512` constant.
- These two values are not connected — changing the config has no effect.

### 3. Client-Side: Zero Chunk Management

**Location**: `c2-ipc/src/client.rs` — `read_responses_loop()` local HashMap

- Client stores in-progress `ChunkAssembler`s in a local `HashMap<u32, ChunkAssembler>`.
- **No timeout**: If the server sends some chunks of a response but never finishes,
  the assembler sits in the HashMap forever.
- **No GC**: No background task to sweep stale entries.
- **No concurrency limit**: HashMap can grow without bound.
- **No explicit cleanup on disconnect**: HashMap is dropped when the read loop exits,
  but in-flight MemHandles may not be properly freed (depends on Drop impl).

**Impact**: A crashed server or network partition during a chunked response will leak
reassembly memory on the client side.

### 4. ChunkAssembler Has No Built-in Lifecycle Management

**Location**: `c2-wire/src/assembler.rs`

- `ChunkAssembler` is a pure data assembler with no timer or expiry.
- It provides `abort()` for explicit cleanup but never calls it internally.
- The caller (server/client) is responsible for lifecycle management — **but neither does it**.

## Reproduction Scenario

1. Client sends a 1 GB payload as chunked request (e.g., 8192 chunks × 128 KB)
2. After sending 4096 chunks, client process crashes
3. Server has a `ChunkAssembler` holding ~512 MB of reassembly pool memory
4. This memory is never reclaimed until the TCP connection times out (OS-level)
5. Under repeated crashes, reassembly pool exhaustion is possible

## Recommended Fix

### Phase 1: Implement Chunk GC (Both Sides)

Add a background `tokio::spawn` task in both `c2-server` (per-connection) and
`c2-ipc` (per-client) that:

1. Runs every `chunk_gc_interval` completed request cycles (or convert to a time interval)
2. Scans the assembler HashMap for entries older than `chunk_assembler_timeout`
3. Calls `assembler.abort()` on stale entries to free MemHandles
4. Logs a warning when aborting (tracing)

### Phase 2: Enforce `max_total_chunks` from Config

- Pass `max_total_chunks` from `IpcConfig` to `ChunkAssembler::new()` instead of
  using the hardcoded constant in `c2-wire`.
- Both server and client should reject chunked transfers exceeding this limit.

### Phase 3: Connection Close Cleanup

- On connection close (both sides), explicitly iterate remaining assemblers and call `abort()`.
- Server: in the connection close handler after `conn.wait_idle()`.
- Client: in `read_responses_loop()` exit path.

## Related Files

- `src/c_two/_native/c2-wire/src/assembler.rs` — ChunkAssembler definition
- `src/c_two/_native/c2-server/src/server.rs:632-816` — Server chunked dispatch
- `src/c_two/_native/c2-server/src/connection.rs:108-109,320-345` — Server assembler storage
- `src/c_two/_native/c2-ipc/src/client.rs:897-1050` — Client chunked response handling
- `src/c_two/_native/c2-server/src/config.rs:25-30` — Unused config fields
