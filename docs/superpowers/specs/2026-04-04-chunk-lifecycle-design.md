# Chunk Lifecycle Management — Design Spec

**Date:** 2026-04-04
**Status:** Approved
**Resolves:** `docs/issues/chunk-management-gaps.md`

## Problem

Chunked transfer in c-two has no lifecycle management:

1. **Server**: `Connection.assemblers` HashMap leaks on disconnect — no cleanup, no GC, no timeout.
2. **Client**: Has basic disconnect cleanup but no GC/timeout for long-running incomplete assemblies.
3. **Config fields** `chunk_gc_interval`, `chunk_assembler_timeout`, `max_total_chunks`, `max_reassembly_bytes` are defined in `IpcConfig` but **never used**. The constants in `c2-wire/assembler.rs` are hardcoded.
4. No memory pressure mitigation — when SHM is exhausted during chunked reassembly, allocation fails hard instead of spilling to disk.

## Design Goals

- **Never reject normal requests** due to soft resource limits. Physical exhaustion (disk + RAM) is the only hard limit.
- **Symmetric architecture** — server and client share the exact same lifecycle module.
- **Transparent FileSpill** — when SHM is full, chunk reassembly spills to disk automatically; on completion, attempt promotion back to SHM.
- **Predictable cleanup** — time-based GC with configurable interval and timeout.

## Solution: New `c2-chunk` Crate

A new Cargo crate `c2-chunk` provides a unified `ChunkRegistry` that both `c2-server` and `c2-ipc` instantiate identically.

### Dependency Position

```
c2-config ← c2-mem ← c2-wire (std) ← c2-chunk
                                         ↑
                              c2-server ──┘
                              c2-ipc ─────┘
```

`c2-chunk` depends on `c2-wire` (for `ChunkAssembler`) and `c2-mem` (for `MemPool`, `MemHandle`). Both `c2-server` and `c2-ipc` add `c2-chunk` as a dependency.

### Crate Structure

```
c2-chunk/
  Cargo.toml
  src/
    lib.rs            # pub mod registry; pub mod promote;
    registry.rs       # ChunkRegistry
    promote.rs        # promote_to_shm()
```

---

## Core Data Types

### ChunkConfig

Extracted from `IpcConfig` chunk-related fields:

```rust
pub struct ChunkConfig {
    /// Timeout for incomplete assemblies. Default 60s.
    pub assembler_timeout: Duration,
    /// GC sweep interval. Default 5s.
    pub gc_interval: Duration,
    /// Soft limit on concurrent in-flight assemblies. Default 512.
    pub soft_limit: u32,
    /// Soft limit on total reassembly bytes. Default 8 GB.
    pub max_reassembly_bytes: u64,
    /// Per-assembler validation limits (passed to ChunkAssembler::new).
    pub max_chunks_per_request: usize,
    pub max_bytes_per_request: usize,
}
```

### TrackedAssembler (internal)

```rust
struct TrackedAssembler {
    inner: ChunkAssembler,
    created_at: Instant,
    last_activity: Instant,
    total_bytes: usize,
}
```

### ChunkRegistry

```rust
pub struct ChunkRegistry {
    assemblers: Mutex<HashMap<(u64, u64), TrackedAssembler>>,
    pool: Arc<RwLock<MemPool>>,
    config: ChunkConfig,
}
```

Key: `(conn_id, request_id)`. Server uses actual connection IDs; client uses its own connection ID (constant per `recv_loop` lifetime).

---

## Public API

```rust
impl ChunkRegistry {
    /// Create a new registry with a shared MemPool and config.
    pub fn new(pool: Arc<RwLock<MemPool>>, config: ChunkConfig) -> Self;

    // ── Lifecycle ────────────────────────────────────────────────

    /// Start tracking a new chunked transfer.
    /// Allocates MemHandle from pool (may FileSpill if SHM full).
    /// Triggers GC if soft limits exceeded (but never rejects).
    /// Returns Err only on physical allocation failure.
    pub fn insert(
        &self,
        conn_id: u64,
        request_id: u64,
        total_chunks: usize,
        chunk_size: usize,
    ) -> Result<(), String>;

    /// Feed a chunk into an in-progress assembly.
    /// Updates last_activity timestamp.
    /// Returns Ok(true) when all chunks received.
    pub fn feed(
        &self,
        conn_id: u64,
        request_id: u64,
        chunk_idx: usize,
        data: &[u8],
    ) -> Result<bool, String>;

    /// Extract completed assembly as MemHandle.
    /// If the handle is FileSpill and SHM has space, promotes to SHM.
    /// Never fails due to promotion — returns FileSpill handle on failure.
    pub fn finish(
        &self,
        conn_id: u64,
        request_id: u64,
    ) -> Result<MemHandle, String>;

    /// Abort a specific in-progress assembly and free its MemHandle.
    pub fn abort(&self, conn_id: u64, request_id: u64);

    // ── Cleanup ──────────────────────────────────────────────────

    /// Sweep all assemblies, abort those exceeding assembler_timeout.
    /// Returns statistics about what was cleaned.
    pub fn gc_sweep(&self) -> GcStats;

    /// Remove and abort all assemblies belonging to a connection.
    /// Called on connection close (both server and client).
    pub fn cleanup_connection(&self, conn_id: u64);

    // ── Query ────────────────────────────────────────────────────

    /// Number of in-flight assemblies.
    pub fn active_count(&self) -> usize;

    /// Total bytes allocated by all in-flight assemblies.
    pub fn total_bytes(&self) -> u64;
}

pub struct GcStats {
    pub expired: usize,
    pub remaining: usize,
    pub freed_bytes: u64,
}
```

---

## Behavioral Rules

### Soft Limit Logic in `insert()`

```
insert(conn_id, req_id, total_chunks, chunk_size):
  1. alloc_size = total_chunks × chunk_size
  2. if active_count() >= soft_limit:
       stats = gc_sweep()           // immediate GC attempt
       if active_count() >= soft_limit:
           warn!("chunk registry: soft limit {soft_limit} still exceeded after GC")
       // continue regardless — soft limit, not hard
  3. if total_bytes() + alloc_size > max_reassembly_bytes:
       gc_sweep()                   // immediate GC attempt
       if total_bytes() + alloc_size > max_reassembly_bytes:
           warn!("chunk registry: reassembly bytes exceed {max_reassembly_bytes}")
       // continue regardless
  4. pool.write().alloc_handle(alloc_size)
       → buddy/dedicated/FileSpill (transparent fallback)
       → Err only on physical exhaustion (true hard limit)
  5. store TrackedAssembler { created_at: now, last_activity: now, total_bytes: alloc_size }
```

### GC Sweep

```
gc_sweep():
  now = Instant::now()
  for each (key, tracked) in assemblers:
    if now - tracked.last_activity > assembler_timeout:
      tracked.inner.abort(&mut pool.write())
      remove from map
      warn!("chunk GC: expired assembly conn={} req={} age={:.1}s",
            key.0, key.1, (now - tracked.created_at).as_secs_f64())
  return GcStats { expired, remaining, freed_bytes }
```

The GC timer is **not** owned by `ChunkRegistry`. Callers (`c2-server` / `c2-ipc`) spawn their own `tokio::interval` task that calls `gc_sweep()` periodically. This keeps `ChunkRegistry` runtime-agnostic (no tokio dependency in c2-chunk).

### FileSpill Promotion in `finish()`

```
finish(conn_id, req_id):
  1. remove TrackedAssembler from map
  2. handle = tracked.inner.finish()?
  3. if handle.is_file_spill():
       match promote_to_shm(&mut pool.write(), handle):
         Ok(shm_handle) => debug!("promoted FileSpill → SHM"); return Ok(shm_handle)
         Err(original)  => return Ok(original)   // silent degradation
  4. return Ok(handle)
```

### Connection Cleanup

```
cleanup_connection(conn_id):
  let to_remove: Vec<_> = map.keys()
      .filter(|(cid, _)| *cid == conn_id)
      .copied().collect()
  for key in to_remove:
      tracked = map.remove(&key).unwrap()
      tracked.inner.abort(&mut pool)
      warn!("cleanup: aborted assembly conn={} req={}", key.0, key.1)
```

---

## FileSpill Promotion (`promote.rs`)

```rust
/// Try to move a FileSpill handle into SHM (Buddy or Dedicated).
/// Returns Ok(shm_handle) on success, Err(original_handle) if SHM unavailable.
/// The original handle is consumed on success, returned on failure (no data loss).
pub fn promote_to_shm(
    pool: &mut MemPool,
    file_handle: MemHandle,
) -> Result<MemHandle, MemHandle> {
    debug_assert!(file_handle.is_file_spill());
    let len = file_handle.len();

    // Phase 1: Allocate SHM destination (needs &mut self).
    let mut shm_handle = match pool.try_alloc_shm(len) {
        Ok(h) => h,
        Err(_) => return Err(file_handle),
    };

    // Phase 2: Copy data.
    // handle_slice / handle_slice_mut both take &self on pool,
    // so concurrent immutable reborrows are fine.
    {
        let src = pool.handle_slice(&file_handle);
        let dst = pool.handle_slice_mut(&mut shm_handle);
        dst[..len].copy_from_slice(&src[..len]);
    }
    // Slices dropped here.

    // Phase 3: Release old handle (needs &mut self).
    shm_handle.set_len(len);
    pool.release_handle(file_handle);
    Ok(shm_handle)
}
```

**Requires new `MemPool` method:**

```rust
/// Allocate from Buddy or Dedicated SHM only — no FileSpill fallback.
/// Returns Err if neither has capacity.
pub fn try_alloc_shm(&mut self, size: usize) -> Result<MemHandle, String>;
```

Implementation: same logic as `alloc_handle()` lines 513–570, but the final FileSpill fallbacks at lines 560 and 569 return `Err` instead.

---

## Integration Changes

### c2-server

**`Server` struct changes:**
```rust
// REMOVE:
//   reassembly_pool: Arc<RwLock<MemPool>>   (absorbed into ChunkRegistry)
// ADD:
    chunk_registry: Arc<ChunkRegistry>,
```

**`Server::new()` changes:**
- Create MemPool for reassembly (same as today, with `/cc3s` prefix)
- Wrap in `ChunkRegistry::new(pool, config)`
- Spawn GC timer task: `tokio::interval(config.gc_interval)` → `registry.gc_sweep()`

**`handle_connection()` changes (lines 349–352):**
```rust
// AFTER wait_idle():
registry.cleanup_connection(conn_id);   // NEW — abort orphaned assemblers
```

**`dispatch_chunked_call()` changes:**
- Replace `conn.insert_assembler()` → `registry.insert(conn_id, req_id, ...)`
- Replace `conn.feed_chunk()` → `registry.feed(conn_id, req_id, ...)`
- Replace `conn.take_assembler() + asm.finish()` → `registry.finish(conn_id, req_id)`

**`Connection` struct changes:**
- Remove `assemblers: Mutex<HashMap<u64, ChunkAssembler>>`
- Remove `insert_assembler()`, `feed_chunk()`, `take_assembler()` methods

### c2-ipc (client)

**`recv_loop()` changes:**
- Remove local `let mut assemblers: HashMap<u32, ChunkAssembler>`
- Accept `Arc<ChunkRegistry>` parameter (passed from Client/ClientPool)
- Replace manual assembler HashMap operations → `registry.insert/feed/finish`
- Replace manual abort loop on disconnect → `registry.cleanup_connection(conn_id)`

**GC timer:**
- Spawned by Client or ClientPool, same pattern as server

### c2-wire

**`ChunkAssembler::new()` signature change:**
```rust
// OLD:
pub fn new(pool: &mut MemPool, total_chunks: usize, chunk_size: usize) -> Result<Self, String>

// NEW:
pub fn new(
    pool: &mut MemPool,
    total_chunks: usize,
    chunk_size: usize,
    max_total_chunks: usize,     // from config
    max_reassembly_bytes: usize, // from config
) -> Result<Self, String>
```

Remove hardcoded constants:
- `const MAX_TOTAL_CHUNKS: usize = 512;` → deleted
- `const MAX_REASSEMBLY_BYTES: usize = 8 * (1 << 30);` → deleted

### c2-config

**`IpcConfig` field change:**
```rust
// OLD:
pub chunk_gc_interval: u32,      // "completed-request cycles"

// NEW:
pub chunk_gc_interval: f64,      // seconds (default 5.0)
```

Other chunk fields unchanged — they're already correct types.

---

## Log Levels

| Event | Level |
|-------|-------|
| GC cleans expired assembler | `warn!` |
| Soft limit exceeded after GC | `warn!` |
| Connection cleanup aborts assembler | `warn!` |
| FileSpill allocation (transparent) | `info!` |
| Promote FileSpill → SHM success | `debug!` |
| Promote FileSpill → SHM failed (degraded) | `debug!` |
| Physical alloc failure | `error!` |

---

## Testing Strategy

### Unit Tests (c2-chunk)

1. **insert + feed + finish happy path** — basic lifecycle
2. **GC sweep expires stale assemblers** — insert, advance time, gc_sweep, verify freed
3. **Soft limit triggers GC** — fill to soft_limit, insert more, verify GC runs and logs
4. **cleanup_connection** — insert for 2 conn_ids, cleanup one, verify other survives
5. **finish promotes FileSpill** — force FileSpill alloc, then finish with SHM available
6. **finish keeps FileSpill when SHM full** — force FileSpill, keep SHM full, verify FileSpill handle returned
7. **feed updates last_activity** — insert, wait, feed, verify GC does not expire

### Integration Tests

8. **Server: connection disconnect cleanup** — start chunked transfer, kill client, verify assembler cleaned up
9. **Client: connection disconnect cleanup** — start chunked response, kill server, verify assembler cleaned up
10. **End-to-end large payload with FileSpill** — send payload larger than SHM, verify correct reassembly

### Property: No Assembler/MemHandle Leak

Every test must verify: after cleanup/gc, `active_count() == 0` and `total_bytes() == 0`.

---

## Migration Notes

- `Connection.assemblers` field and its 3 methods are deleted — breaking change within c2-server (internal only, no public API).
- `ChunkAssembler::new()` gains 2 parameters — breaking change within c2-wire (internal crate API).
- `IpcConfig.chunk_gc_interval` type changes from `u32` to `f64` — breaking change in c2-config. FFI layer (`PyIpcConfig` if any) must update.
- No Python-visible API changes. The `ChunkRegistry` is purely Rust-internal.

---

## Implementation Notes

### Locking Discipline

`ChunkRegistry` holds two locks: `assemblers: Mutex` and `pool: Arc<RwLock<MemPool>>`.

**Lock order:** Always `assemblers` → `pool`. Never reverse.

**Soft limit checks in `insert()`:** The active count / total bytes checks must happen **outside** the assemblers lock. `gc_sweep()` acquires the assemblers lock internally, so calling it while already holding the lock would deadlock. The check-then-insert has a benign TOCTOU race (another thread may insert between check and lock), which is acceptable for a soft advisory limit.

### `ChunkAssembler::abort()` Ownership

`abort(self, &mut MemPool)` consumes the assembler. This aligns with `HashMap::remove()` which returns the owned value — remove then abort is safe and clean.

