# Code Quality Remediation Design

**Date:** 2026-04-06
**Status:** Proposed
**Scope:** 3 code quality issues identified in post-restructure review

## Problem Statement

Post-restructure code review identified three categories of code quality issues:

1. **3× `#[allow(dead_code)]`** — fields kept for side effects (drop guards, metadata) using non-idiomatic suppression
2. **163× `.lock().unwrap()`** — lock poisoning risk across 6 crates; any panic in a critical section poisons all subsequent operations
3. **GC task leak** — `tokio::spawn`-ed GC loop in c2-server has no shutdown signal; runs until runtime is dropped

None are bugs introduced by the restructure — all are pre-existing. Fixing them improves robustness and idiomatic Rust usage.

---

## Issue 1: `#[allow(dead_code)]` → Underscore Prefix

### Rationale

Rust convention: prefix fields with `_` when they exist for side effects (Drop, reference counting) but are not read directly. This eliminates the warning without `#[allow]` and communicates intent to readers.

### Changes

| File | Field | New Name | Notes |
|------|-------|----------|-------|
| `transport/c2-ipc/src/shm.rs:33` | `fd: RawFd` | `_fd` | Used in `Drop::drop()` — update `self.fd` → `self._fd` |
| `transport/c2-http/src/relay/server.rs:46` | `pool: Arc<RwLock<UpstreamPool>>` | `_pool` | Holds Arc refcount; no Drop impl to update |
| `protocol/c2-wire/src/chunk/registry.rs:41` | `created_at: Instant` | `_created_at` | Metadata for future GC observability |

Each field: remove `#[allow(dead_code)]`, rename, update any references.

---

## Issue 2: Lock Poisoning → parking_lot Migration

### Rationale

`std::sync::Mutex` and `std::sync::RwLock` implement **lock poisoning**: if a thread panics while holding a lock, subsequent `.lock()` calls return `Err(PoisonError)`. The codebase handles this with `.unwrap()` everywhere (163 call sites), meaning a single panic cascades into panics on every subsequent lock acquisition.

`parking_lot::{Mutex, RwLock}` never poison — `.lock()` returns a guard directly. This eliminates the entire class of cascading-panic bugs while simplifying code.

### Approach

**Full direct replacement** — all 163 occurrences in one pass.

### Dependency Change

```toml
# workspace Cargo.toml [workspace.dependencies]
parking_lot = "0.12"

# Each crate's Cargo.toml
[dependencies]
parking_lot.workspace = true
```

Affected crates: `c2-wire`, `c2-ipc`, `c2-server`, `c2-http`, `c2-ffi` (5 crates; c2-mem has no std Mutex/RwLock usage)

### Code Transformation Pattern

```rust
// BEFORE
use std::sync::{Mutex, RwLock};
let guard = self.inner.lock().unwrap();
let guard = self.pool.read().unwrap();
let guard = self.pool.write().unwrap();

// AFTER
use parking_lot::{Mutex, RwLock};
let guard = self.inner.lock();
let guard = self.pool.read();
let guard = self.pool.write();
```

### Special Cases

1. **`Arc<RwLock<MemPool>>` cross-crate boundary** — `MemPool` is wrapped in `Arc<RwLock<>>` and passed between c2-server, c2-ipc, c2-ffi, and c2-wire. All consumers must switch to `parking_lot::RwLock` simultaneously. Since we're doing a full migration, this is handled naturally.

2. **Drop impls with `if let Ok(guard) = self.lock()`** — These defensive patterns (found in c2-ffi and c2-server/dispatcher.rs) exist specifically because of poisoning. With parking_lot, `lock()`/`write()` returns a guard directly — `if let Ok(...)` won't compile. Simplify to direct `let guard = self.lock()`:
   ```rust
   // BEFORE (c2-ffi + dispatcher.rs defensive pattern)
   if let Ok(guard) = self.inner.lock() {
       guard.cleanup();
   }
   // AFTER
   let guard = self.inner.lock();
   guard.cleanup();
   ```
   Affected locations (16 total): c2-ffi/client_ffi.rs (5), c2-ffi/shm_buffer.rs (5), c2-ffi/mem_ffi.rs (4), c2-server/dispatcher.rs (2).

3. **PyO3 compatibility** — `parking_lot` guards implement `Send`/`Sync` the same way as std. `#[pyclass(frozen)]` types with interior `parking_lot::Mutex` work identically.

4. **`std::sync::Arc`** — Not affected. Only `Mutex` and `RwLock` are replaced; `Arc` stays as `std::sync::Arc`.

### Scope by Crate

| Crate | Files | `.unwrap()` | `if let Ok` | Priority |
|-------|-------|-------------|-------------|----------|
| c2-ipc | client.rs(30), pool.rs(14), response.rs(3), sync_client.rs(2) | 49 | 0 | Critical (async blocking) |
| c2-ffi | mem_ffi.rs(17), client_ffi.rs(8), relay_ffi.rs(8), server_ffi.rs(5), shm_buffer.rs(5) | 43 | 14 | High (FFI boundary) |
| c2-server | connection.rs(18), server.rs(8) + dispatcher.rs | 26 | 2 | Critical (per-connection) |
| c2-http | relay/router.rs(12), relay/server.rs(6), client/pool.rs(5), relay/state.rs(1) | 24 | 0 | Medium |
| c2-wire | chunk/registry.rs(21) | 21 | 0 | High (sharded locks) |
| **Total** | **16 files** | **163** | **16** | |

---

## Issue 3: GC Task Graceful Shutdown

### Rationale

The chunk GC task is spawned via `tokio::spawn` in `Server::run()` as an infinite loop with no shutdown signal. It relies entirely on `rt.shutdown_background()` from the Python FFI layer to be aborted. This is:

- **Not graceful** — the GC sweep may be interrupted mid-operation
- **Inconsistent** — per-connection heartbeat tasks ARE properly managed (stored `JoinHandle` + `.abort()`)
- **Low actual risk** — the GC only touches `ChunkRegistry` (Arc-held), so interruption is safe

### Approach: Reuse existing `watch::channel<bool>`

The server already has `shutdown_tx: watch::Sender<bool>` / `shutdown_rx: watch::Receiver<bool>` for the accept loop. The GC task simply clones `shutdown_rx` and uses `tokio::select!` to listen for shutdown.

### Code Change (server.rs, ~10 lines)

```rust
// BEFORE (lines 200-213)
let gc_registry = self.chunk_registry.clone();
let gc_interval = self.chunk_registry.config().gc_interval;
tokio::spawn(async move {
    let mut interval = tokio::time::interval(gc_interval);
    interval.tick().await;
    loop {
        interval.tick().await;
        let stats = gc_registry.gc_sweep();
        if stats.expired > 0 {
            info!(expired = stats.expired, freed = stats.freed_bytes, "chunk GC sweep");
        }
    }
});

// AFTER
let gc_registry = self.chunk_registry.clone();
let gc_interval = self.chunk_registry.config().gc_interval;
let mut gc_shutdown = self.shutdown_rx.clone();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(gc_interval);
    interval.tick().await;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let stats = gc_registry.gc_sweep();
                if stats.expired > 0 {
                    info!(expired = stats.expired, freed = stats.freed_bytes, "chunk GC sweep");
                }
            }
            _ = gc_shutdown.changed() => {
                info!("chunk GC task shutting down");
                break;
            }
        }
    }
});
```

### Effect

- GC task exits promptly on `server.shutdown()`
- No new dependencies (watch channel already exists)
- `rt.shutdown_background()` in FFI layer still works as a safety net

---

## Testing Strategy

### Issue 1 (dead_code)

- `cargo check --workspace` — no warnings for these 3 fields
- Existing tests pass (pure rename, no behavior change)

### Issue 2 (parking_lot)

- `cargo test --workspace` — all Rust tests pass
- `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30` — all Python tests pass
- `cargo check --workspace 2>&1 | grep "unused import"` — no leftover std::sync imports
- Verify: `grep -r "std::sync::Mutex\|std::sync::RwLock" src/c_two/_native/ --include="*.rs"` returns zero results (except comments or documentation)

### Issue 3 (GC shutdown)

- Existing integration tests (server start/shutdown cycles) exercise the shutdown path
- `cargo test -p c2-server` — all server tests pass
- Manual verification: no "chunk GC sweep" log entries after shutdown signal

---

## Risk Assessment

| Issue | Risk | Mitigation |
|-------|------|------------|
| 1: underscore rename | None | Pure rename, compiler-verified |
| 2: parking_lot migration | Low | Mechanical replacement; API is nearly identical. parking_lot is a mature, widely-used crate (100M+ downloads). Main risk: missed `.unwrap()` → compiler will catch it (type mismatch). |
| 3: GC shutdown | None | Additive change; existing behavior preserved as fallback |

## Ordering

1. Issue 1 first (trivial, unblocks clean `cargo check`)
2. Issue 2 next (largest scope, independent of Issue 3)
3. Issue 3 last (touches server.rs which also gets parking_lot changes in Issue 2)

