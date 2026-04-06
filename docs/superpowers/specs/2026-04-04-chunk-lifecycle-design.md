# Chunk Lifecycle Management — Design Spec

**Date:** 2026-04-04
**Revised:** 2026-04-04 (post-audit — fixed 6 issues from concurrency/safety review)
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
/// Number of shards — must be power of two.
const SHARD_COUNT: usize = 16;

pub struct ChunkRegistry {
    /// Sharded by conn_id to minimize cross-connection lock contention.
    /// Each shard owns an independent Mutex<HashMap>.
    /// Shard index = conn_id % SHARD_COUNT.
    shards: [Mutex<HashMap<(u64, u64), TrackedAssembler>>; SHARD_COUNT],
    pool: Arc<RwLock<MemPool>>,
    config: ChunkConfig,
    /// Global counters (atomic, lock-free) for soft-limit checks.
    active_count: AtomicUsize,
    total_bytes: AtomicU64,
}
```

Key: `(conn_id, request_id)`. Server uses actual connection IDs; client uses a fabricated unique conn_id (from an atomic counter, see Client Integration below).

**Why sharded?** The server spawns a separate tokio task per chunk frame (`tokio::spawn` at line 321 of server.rs). Even chunks for the same request run as independent tasks. A single global Mutex would serialize ALL connections — unacceptable under load. Sharding by `conn_id` restores per-connection isolation: tasks for the same connection contend only with each other (same as the current per-connection Mutex), while cross-connection contention is eliminated.

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

    /// Number of in-flight assemblies (lock-free atomic read).
    pub fn active_count(&self) -> usize;

    /// Total bytes allocated by all in-flight assemblies (lock-free atomic read).
    pub fn total_bytes(&self) -> u64;

    // ── Internal ─────────────────────────────────────────────────

    /// Shard selection: &shards[conn_id as usize % SHARD_COUNT]
    fn shard(&self, conn_id: u64) -> &Mutex<HashMap<(u64, u64), TrackedAssembler>>;
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
  2. if self.active_count.load(Relaxed) >= soft_limit:   // atomic, no lock
       stats = gc_sweep()                                // locks each shard briefly
       if self.active_count.load(Relaxed) >= soft_limit:
           warn!("chunk registry: soft limit {soft_limit} still exceeded after GC")
       // continue regardless — soft limit, not hard
  3. if self.total_bytes.load(Relaxed) + alloc_size > max_reassembly_bytes:
       gc_sweep()
       if self.total_bytes.load(Relaxed) + alloc_size > max_reassembly_bytes:
           warn!("chunk registry: reassembly bytes exceed {max_reassembly_bytes}")
       // continue regardless
  4. pool.write().alloc_handle(alloc_size)
       → buddy/dedicated/FileSpill (transparent fallback)
       → Err only on physical exhaustion (true hard limit)
  5. let shard = self.shard(conn_id)
     shard.lock().insert((conn_id, req_id), TrackedAssembler { ... })
     self.active_count.fetch_add(1, Relaxed)
     self.total_bytes.fetch_add(alloc_size, Relaxed)
```

Note: soft limit checks are lock-free (atomic reads). The check-then-insert has a benign TOCTOU race, acceptable for advisory limits.

### GC Sweep

```
gc_sweep():
  now = Instant::now()
  total_expired = 0
  total_freed = 0
  for shard in &self.shards:             // iterate ALL shards
    let mut map = shard.lock()
    let expired_keys: Vec<_> = map.iter()
        .filter(|(_, t)| now - t.last_activity > assembler_timeout)
        .map(|(k, _)| *k).collect()
    for key in expired_keys:
      let tracked = map.remove(&key).unwrap()
      tracked.inner.abort(&mut pool.write())   // free MemHandle
      self.active_count.fetch_sub(1, Relaxed)
      self.total_bytes.fetch_sub(tracked.total_bytes, Relaxed)
      warn!("chunk GC: expired conn={} req={} age={:.1}s", ...)
      total_expired += 1
      total_freed += tracked.total_bytes
  return GcStats { expired: total_expired, remaining: active_count(), freed_bytes: total_freed }
```

The GC timer is **not** owned by `ChunkRegistry`. Callers (`c2-server` / `c2-ipc`) spawn their own `tokio::interval` task that calls `gc_sweep()` periodically. This keeps `ChunkRegistry` runtime-agnostic (no tokio dependency in c2-chunk).

### FileSpill Promotion in `finish()`

```
finish(conn_id, req_id):
  1. let shard = self.shard(conn_id)
     let tracked = shard.lock().remove((conn_id, req_id))
         .ok_or("unknown assembly")?
     self.active_count.fetch_sub(1, Relaxed)
     self.total_bytes.fetch_sub(tracked.total_bytes, Relaxed)
  2. match tracked.inner.finish():
       Ok(handle) => proceed to step 3
       Err(e) =>
         // CRITICAL: ChunkAssembler::finish(self) consumes self.
         // On error, the inner MemHandle is dropped — but MemHandle has
         // NO Drop impl, so Buddy/Dedicated SHM segments would leak.
         // Solution: ChunkAssembler MUST implement Drop that calls abort()
         // on the inner MemHandle. See "ChunkAssembler Drop Safety" below.
         return Err(e)
  3. if handle.is_file_spill():
       match promote_to_shm(&mut pool.write(), handle):
         Ok(shm_handle) => debug!("promoted FileSpill → SHM"); return Ok(shm_handle)
         Err(original)  => return Ok(original)   // silent degradation
  4. return Ok(handle)
```

#### ChunkAssembler Drop Safety

**Problem:** `ChunkAssembler::finish(self)` consumes `self`. If it returns `Err`, the inner `MemHandle` is dropped. Neither `ChunkAssembler` nor `MemHandle` implements `Drop` — Buddy/Dedicated SHM segments leak permanently (FileSpill is safe because `MmapMut` auto-unmaps).

**Solution (implemented in c2-chunk, not c2-wire):** `TrackedAssembler` implements `Drop`:

```rust
impl Drop for TrackedAssembler {
    fn drop(&mut self) {
        // If this TrackedAssembler is being dropped without finish() or abort()
        // having been called, the inner ChunkAssembler still holds a MemHandle.
        // We must free it. Since we need &mut MemPool, we go through the
        // Arc<RwLock<MemPool>> stored in the registry.
        //
        // In practice, this path is only hit on panic or logic error.
        // Normal paths call finish() or abort() which consume the inner.
        if self.inner_not_consumed {
            warn!("TrackedAssembler dropped without finish/abort — freeing MemHandle");
            // self.inner.abort(pool) — but we don't have pool here.
            // Instead: ChunkAssembler gains a take_handle() -> Option<MemHandle>
            // and TrackedAssembler stores Arc<RwLock<MemPool>> for emergency cleanup.
        }
    }
}
```

**Practical implementation:** Rather than a complex Drop, we use a simpler approach:
1. `ChunkAssembler` gains `pub fn take_handle(&mut self) -> Option<MemHandle>` — extracts the handle without consuming self.
2. `ChunkRegistry::finish()` calls `take_handle()` first, then `finish_with_handle()` (a new method that operates on the extracted handle). If any step fails, the handle is still owned by the registry and can be freed via `pool.release_handle()`.
3. All error paths in `finish()` explicitly call `pool.write().release_handle(handle)` before returning `Err`.

### Connection Cleanup

```
cleanup_connection(conn_id):
  let shard = self.shard(conn_id)          // O(1) — all entries for this conn in one shard
  let mut map = shard.lock()
  let conn_keys: Vec<_> = map.keys()
      .filter(|(cid, _)| *cid == conn_id)
      .copied().collect()
  for key in conn_keys:
    let tracked = map.remove(&key).unwrap()
    tracked.inner.abort(&mut pool.write())
    self.active_count.fetch_sub(1, Relaxed)
    self.total_bytes.fetch_sub(tracked.total_bytes, Relaxed)
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
    //
    // BORROW SAFETY NOTE (Audit Issue #6):
    // handle_slice(&self, &MemHandle) -> &[u8] and
    // handle_slice_mut(&self, &mut MemHandle) -> &mut [u8]
    // both take &self on pool. The borrow checker may reject simultaneous
    // calls because &file_handle and &mut shm_handle create conflicting
    // borrows through pool's &self.
    //
    // Fallback: if borrow checker rejects, use a temp buffer:
    //   let data = pool.handle_slice(&file_handle).to_vec();
    //   pool.handle_slice_mut(&mut shm_handle)[..len].copy_from_slice(&data);
    //
    // The to_vec() adds one allocation + copy. For typical chunk sizes
    // (hundreds of KB to a few MB), this is acceptable for the rare
    // FileSpill→SHM promotion path.
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

**CRITICAL: `wait_idle()` vs chunk task race (Audit Issue #2)**

Current problem: `dispatch_chunked_call()` is spawned per chunk frame (`tokio::spawn` at line 321). `flight_inc()` is only called at line 787 when ALL chunks are assembled — NOT per chunk task. So after the connection read loop exits:
1. `wait_idle()` sees inflight=0 and returns immediately
2. `cleanup_connection()` removes assemblers
3. But spawned chunk tasks may still be running and try `registry.feed()` on removed assemblers

**Solution:** Each spawned chunk task must call `conn.flight_inc()` at entry and `conn.flight_dec()` on exit (via a guard or explicit finally). This ensures `wait_idle()` blocks until ALL chunk tasks complete — including partial ones that haven't finished reassembly.

```rust
// In dispatch_chunked_call() (spawned task body):
conn.flight_inc();                           // NEW — at task entry
let _guard = FlightGuard::new(&conn);        // RAII: calls flight_dec on drop
// ... existing chunk processing ...
// flight_dec called automatically when _guard drops
```

`FlightGuard` is a simple RAII struct:
```rust
struct FlightGuard<'a> { conn: &'a Connection }
impl<'a> FlightGuard<'a> {
    fn new(conn: &'a Connection) -> Self {
        conn.flight_inc();
        Self { conn }
    }
}
impl Drop for FlightGuard<'_> {
    fn drop(&mut self) { self.conn.flight_dec(); }
}
```

This guarantees: `wait_idle()` returns ONLY when all chunk tasks (including partial ones) have completed → `cleanup_connection()` can safely remove all entries.

**`dispatch_chunked_call()` changes:**
- Replace `conn.insert_assembler()` → `registry.insert(conn_id, req_id, ...)`
- Replace `conn.feed_chunk()` → `registry.feed(conn_id, req_id, ...)`
- Replace `conn.take_assembler() + asm.finish()` → `registry.finish(conn_id, req_id)`

**`Connection` struct changes:**
- Remove `assemblers: Mutex<HashMap<u64, ChunkAssembler>>`
- Remove `insert_assembler()`, `feed_chunk()`, `take_assembler()` methods

### c2-ipc (client)

**Client `conn_id` generation (Audit Issue #5):**

`IpcClient` does not have a `conn_id` field. We fabricate one using an atomic counter:

```rust
// In c2-ipc/src/client.rs or c2-ipc/src/pool.rs:
static CLIENT_CONN_COUNTER: AtomicU64 = AtomicU64::new(1);

impl IpcClient {
    pub fn new(...) -> Self {
        let conn_id = CLIENT_CONN_COUNTER.fetch_add(1, Relaxed);
        // ...
    }
}
```

Each `IpcClient` instance gets a unique `conn_id` at construction time. This ID is used as the first element of the `(conn_id, request_id)` registry key. Since a single `IpcClient` runs one `recv_loop()`, all chunks for that client share the same shard — preserving the current zero-contention property.

**Client `request_id` type (Audit Issue #4):**

The wire protocol uses `u64` request IDs, but `IpcClient` uses `AtomicU32` (`rid_counter`) and truncates at line 938: `let rid = hdr.request_id as u32`.

**Decision:** Keep `u32` in the registry key for now. The truncation is safe as long as fewer than 2³² concurrent requests exist per client (practically impossible). Add a `TODO` comment at the truncation site. The registry key type for client is `(u64, u64)` — the request_id is widened to u64 via `as u64` when calling registry methods. This avoids breaking the wire protocol or client dispatch table (which uses `u32` keys).

**`recv_loop()` changes:**
- Remove local `let mut assemblers: HashMap<u32, ChunkAssembler>`
- Accept `Arc<ChunkRegistry>` parameter (passed from Client/ClientPool)
- Replace manual assembler HashMap operations → `registry.insert/feed/finish`
  (with `request_id as u64` widening for registry calls)
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
8. **finish error path does not leak** — trigger finish error, verify MemHandle freed (active_count=0, total_bytes=0)
9. **sharding isolation** — insert for N different conn_ids in parallel threads, verify no cross-shard interference
10. **cleanup_connection touches only target shard** — insert for conn 1 and conn 2 (different shards), cleanup conn 1, verify conn 2 untouched
11. **concurrent insert/feed/finish** — spawn M threads doing insert+feed+finish on same conn_id, verify no data corruption

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

`ChunkRegistry` uses `SHARD_COUNT` (16) independent `Mutex<HashMap>` shards, plus `pool: Arc<RwLock<MemPool>>`.

**Lock order:** Always `shard[i]` → `pool`. Never reverse.

**Cross-shard:** `gc_sweep()` iterates shards sequentially (lock shard 0, process, unlock, lock shard 1, ...). This means GC can never deadlock with insert/feed/finish (which only lock one shard). GC holds at most one shard lock + pool lock at a time.

**Soft limit checks in `insert()`:** The `active_count` / `total_bytes` checks use lock-free atomic reads. `gc_sweep()` acquires shard locks internally, so it's safe to call before locking a shard. The check-then-insert has a benign TOCTOU race (another thread may insert between check and shard lock), which is acceptable for a soft advisory limit.

**Atomic ordering:** All counter updates use `Relaxed` ordering. Exact counts don't affect correctness (soft limits are advisory). Worst case: a GC sweep is triggered one insert too late or too early.

### `ChunkAssembler::abort()` Ownership

`abort(self, &mut MemPool)` consumes the assembler. This aligns with `HashMap::remove()` which returns the owned value — remove then abort is safe and clean.

---

## Audit Findings Summary (2026-04-04)

This section records the issues found during the concurrency/safety audit and their resolutions in this spec revision.

| # | Severity | Issue | Resolution | Section |
|---|----------|-------|------------|---------|
| 1 | 🔴 Critical | Global Mutex contention — single lock serializes all connections | Sharded by conn_id (16 shards), restoring per-connection isolation | ChunkRegistry struct, Locking Discipline |
| 2 | 🔴 Critical | `wait_idle()` returns before chunk tasks complete → use-after-cleanup race | FlightGuard RAII — each chunk task does flight_inc/dec, wait_idle blocks until all tasks done | Server Integration: wait_idle section |
| 3 | 🔴 Critical | `finish()` error path leaks MemHandle (no Drop impl) | ChunkAssembler gains `take_handle()`, finish uses explicit handle ownership, all error paths call `release_handle()` | ChunkAssembler Drop Safety |
| 4 | 🟠 High | Client request_id is u32 (truncated from wire u64) | Accept u32→u64 widening at registry call site, add TODO for full u64 migration | Client Integration |
| 5 | 🟠 High | Client has no conn_id field | Fabricate via `CLIENT_CONN_COUNTER: AtomicU64` at IpcClient construction | Client Integration |
| 6 | 🟡 Medium | `promote_to_shm` Phase 2 borrow safety uncertain | Added `.to_vec()` fallback strategy if borrow checker rejects direct slice access | promote_to_shm code comment |

