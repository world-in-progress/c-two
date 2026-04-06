# Code Quality Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 3 code quality issues: dead_code annotations, lock poisoning, and GC shutdown lifecycle.

**Architecture:** Mechanical replacements across Rust workspace — `#[allow(dead_code)]` → `_` prefix, `std::sync::{Mutex,RwLock}` → `parking_lot`, and GC task shutdown via existing `watch` channel.

**Tech Stack:** Rust 2024, parking_lot 0.12, tokio watch channel

**Spec:** `docs/superpowers/specs/2026-04-06-code-quality-remediation-design.md`

**Branch:** `refactor/code-quality-remediation` (from `dev`)

---

## File Map

| Task | Files | Action |
|------|-------|--------|
| 1 | 3 files | Rename fields: `_fd`, `_pool`, `_created_at` |
| 2 | 6 Cargo.toml | Add `parking_lot = "0.12"` dependency |
| 3 | 17 .rs files | Replace std Mutex/RwLock → parking_lot, remove `.unwrap()` |
| 4 | 1 file | GC task shutdown via `watch` channel |
| 5 | — | Final verification (cargo test + pytest) |

---

### Task 1: Dead Code Renames (Issue 1)

**Prerequisites:** Create branch first:
```bash
git checkout -b refactor/code-quality-remediation dev
```

**Files:** (all paths relative to `src/c_two/_native/`)
- Modify: `transport/c2-ipc/src/shm.rs` (lines 32-33, 120)
- Modify: `transport/c2-http/src/relay/server.rs` (lines 45-46, 88)
- Modify: `protocol/c2-wire/src/chunk/registry.rs` (lines 40-41, 128)

- [ ] **Step 1: Rename `fd` → `_fd` in shm.rs**

Remove `#[allow(dead_code)]` annotation and rename field + Drop usage:

```rust
// Lines 32-33: BEFORE
    #[allow(dead_code)]
    fd: RawFd,
// AFTER
    _fd: RawFd,

// Line 120 (Drop impl): BEFORE
                libc::close(self.fd);
// AFTER
                libc::close(self._fd);
```

- [ ] **Step 2: Rename `pool` → `_pool` in relay/server.rs**

```rust
// Lines 45-46: BEFORE
    #[allow(dead_code)]
    pool: Arc<RwLock<UpstreamPool>>,
// AFTER
    _pool: Arc<RwLock<UpstreamPool>>,

// Line 88 (struct construction): BEFORE
            pool,
// AFTER
            _pool: pool,
```

- [ ] **Step 3: Rename `created_at` → `_created_at` in chunk/registry.rs**

```rust
// Lines 40-41: BEFORE
    #[allow(dead_code)]
    created_at: Instant,
// AFTER
    _created_at: Instant,

// Line 128 (struct construction): BEFORE
            created_at: now,
// AFTER
            _created_at: now,
```

- [ ] **Step 4: Verify**

```bash
cd src/c_two/_native && cargo check --workspace 2>&1 | grep -E "dead_code|warning.*unused"
```
Expected: No dead_code warnings for these 3 fields.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor: replace #[allow(dead_code)] with _ prefix convention

Rename 3 fields kept for side effects (drop guards, metadata):
- shm.rs: fd → _fd (used in Drop for libc::close)
- relay/server.rs: pool → _pool (holds Arc refcount)
- chunk/registry.rs: created_at → _created_at (GC metadata)"
```

---

### Task 2: Add parking_lot Dependency

**Files:** (all paths relative to `src/c_two/_native/`)
- Modify: `Cargo.toml` (workspace root)
- Modify: `protocol/c2-wire/Cargo.toml`
- Modify: `transport/c2-ipc/Cargo.toml`
- Modify: `transport/c2-server/Cargo.toml`
- Modify: `transport/c2-http/Cargo.toml`
- Modify: `bridge/c2-ffi/Cargo.toml`

- [ ] **Step 1: Add workspace dependency**

In `src/c_two/_native/Cargo.toml`, add after `[workspace.package]` section:

```toml
[workspace.dependencies]
parking_lot = "0.12"
```

- [ ] **Step 2: Add to each crate's Cargo.toml**

Add `parking_lot.workspace = true` to the `[dependencies]` section of each crate:

**c2-wire** (`protocol/c2-wire/Cargo.toml`):
```toml
[dependencies]
parking_lot.workspace = true
c2-config = { path = "../../foundation/c2-config" }
...
```

**c2-ipc** (`transport/c2-ipc/Cargo.toml`):
```toml
[dependencies]
parking_lot.workspace = true
c2-config = { path = "../../foundation/c2-config" }
...
```

**c2-server** (`transport/c2-server/Cargo.toml`):
```toml
[dependencies]
parking_lot.workspace = true
c2-config = { path = "../../foundation/c2-config" }
...
```

**c2-http** (`transport/c2-http/Cargo.toml`) — add to always-available section:
```toml
[dependencies]
# Always available (client)
parking_lot.workspace = true
reqwest = { ... }
...
```

**c2-ffi** (`bridge/c2-ffi/Cargo.toml`):
```toml
[dependencies]
parking_lot.workspace = true
c2-config = { path = "../../foundation/c2-config" }
...
```

- [ ] **Step 3: Verify deps resolve**

```bash
cd src/c_two/_native && cargo check --workspace 2>&1 | head -5
```
Expected: Compiles (parking_lot downloaded but not yet used).

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "build: add parking_lot 0.12 workspace dependency

Add to 5 crates: c2-wire, c2-ipc, c2-server, c2-http, c2-ffi.
Preparing for std::sync → parking_lot migration."
```

---

### Task 3: parking_lot Migration (Issue 2)

**Scope:** 17 files, 163 `.unwrap()` removals + 16 `if let Ok` rewrites.

All paths relative to `src/c_two/_native/`. Every file follows the same mechanical pattern:

**Pattern A — import replacement:**
```rust
// BEFORE: use std::sync::{Arc, Mutex, RwLock};
// AFTER:  use std::sync::Arc;
//         use parking_lot::{Mutex, RwLock};
```
Keep `Arc` and `OnceLock` in `std::sync`. Move only `Mutex` and `RwLock` to `parking_lot`.

**Pattern B — unwrap removal:**
```rust
// BEFORE: self.pool.lock().unwrap()
// AFTER:  self.pool.lock()
// Same for .read().unwrap() and .write().unwrap()
```

**Pattern C — defensive Drop rewrite:**
```rust
// BEFORE: if let Ok(guard) = self.inner.lock() { ... }
// AFTER:  let guard = self.inner.lock(); ...
// (parking_lot never returns Result — if let Ok won't compile)
```

**Pattern D — StdMutex alias (c2-ipc/client.rs only):**
```rust
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, RwLock};
//         use tokio::sync::{oneshot, Mutex};
// AFTER:  use std::sync::Arc;
//         use parking_lot::{Mutex as StdMutex, RwLock};
//         use tokio::sync::{oneshot, Mutex};
// Keep StdMutex alias to avoid conflict with tokio::sync::Mutex.
```

**For files that use `Mutex as StdMutex` but have NO tokio::sync::Mutex conflict:**
```rust
// Files: c2-ipc/pool.rs, c2-ipc/sync_client.rs, c2-ffi/client_ffi.rs
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, ...};
// AFTER:  use std::sync::Arc;        (+ OnceLock if needed)
//         use parking_lot::Mutex;     (drop alias)
// Also rename all StdMutex → Mutex in the file body.
```

---

#### Task 3a: Migrate c2-wire (1 file, 21 sites)

- [ ] **Step 1: Migrate `protocol/c2-wire/src/chunk/registry.rs`**

Import change (line 9):
```rust
// BEFORE: use std::sync::{Arc, Mutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
```

Then remove `.unwrap()` from all 21 `.lock().unwrap()` / `.read().unwrap()` / `.write().unwrap()` calls. Exact lines:
- `.lock().unwrap()` — shard locks in insert/feed/finish/gc_sweep/cleanup_connection
- `.write().unwrap()` — pool write locks in abort calls
- `.read().unwrap()` — pool read locks in promote calls

---

#### Task 3b: Migrate c2-ipc (4 files, 49 sites)

- [ ] **Step 2: Migrate `transport/c2-ipc/src/client.rs`** (30 sites)

Import change (line 9):
```rust
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex as StdMutex, RwLock};
```
Keep `use tokio::sync::{oneshot, Mutex};` (line 13) unchanged — the `StdMutex` alias avoids conflict.

Remove `.unwrap()` from all 30 `.lock().unwrap()` / `.read().unwrap()` / `.write().unwrap()` calls.

- [ ] **Step 3: Migrate `transport/c2-ipc/src/pool.rs`** (14 sites)

Import change (line 9):
```rust
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, OnceLock};
// AFTER:
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;
```
Rename ALL `StdMutex` → `Mutex` in the file body (~8 occurrences: struct fields, `StdMutex::new(...)`, type annotations).

Remove `.unwrap()` from all 14 `.lock().unwrap()` calls.

- [ ] **Step 4: Migrate `transport/c2-ipc/src/response.rs`** (3 sites)

This file uses **fully-qualified paths** (no Mutex/RwLock import):
```rust
// Line 54: std::sync::Arc<std::sync::Mutex<...>>
// Line 55: std::sync::Arc<std::sync::RwLock<...>>
```
Change all `std::sync::Mutex` → `parking_lot::Mutex` and `std::sync::RwLock` → `parking_lot::RwLock` in the fully-qualified paths. Remove `.unwrap()` from 3 calls (lines 60, 66, 70).

- [ ] **Step 5: Migrate `transport/c2-ipc/src/sync_client.rs`** (2 sites)

Import change (line 6):
```rust
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};
// AFTER:
use std::sync::{Arc, OnceLock};
use parking_lot::{Mutex, RwLock};
```
Rename ALL `StdMutex` → `Mutex` in the file body. Remove `.unwrap()` from 2 calls.

---

#### Task 3c: Migrate c2-server (3 files, 26 unwrap + 2 if-let-Ok)

- [ ] **Step 6: Migrate `transport/c2-server/src/connection.rs`** (18 sites)

Import change (line 8):
```rust
// BEFORE: use std::sync::{Arc, Mutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
```
Remove `.unwrap()` from all 18 calls.

- [ ] **Step 7: Migrate `transport/c2-server/src/server.rs`** (8 sites)

**⚠️ This file uses BOTH tokio and std locks.** Line 14: `use tokio::sync::{watch, Mutex, RwLock};` — these are tokio locks for async context (dispatcher, writer). Do NOT change them.

The 8 `.unwrap()` sites are all on `response_pool: Arc<std::sync::RwLock<MemPool>>` using **fully-qualified paths**. Changes:
- Change all `std::sync::RwLock` → `parking_lot::RwLock` in fully-qualified type annotations (lines ~96, 125, 153, 173)
- Remove `.unwrap()` from 8 calls (lines 400, 888, 900, 913, 942, 1015, 1228, 1234)
- Line 1193 (test): `use std::sync::{Arc, RwLock};` → `use std::sync::Arc; use parking_lot::RwLock;`

- [ ] **Step 8: Migrate `transport/c2-server/src/dispatcher.rs`** (0 unwrap + 2 if-let-Ok)

This file uses **fully-qualified** `std::sync::RwLock` (no import). Change all `std::sync::RwLock` → `parking_lot::RwLock` in type annotations (lines ~55, 67, 115, 212, 237, 333, 339). Rewrite 2 `if let Ok` patterns at lines 76 and 81:

```rust
// BEFORE (line 76):
            if let Ok(mut p) = pool.write() {
                p.release_handle(handle);
            }
// AFTER:
            {
                let mut p = pool.write();
                p.release_handle(handle);
            }

// BEFORE (line 81):
            if let Ok(mut p) = pool.write() {
                p.release_handle(handle);
            }
// AFTER:
            {
                let mut p = pool.write();
                p.release_handle(handle);
            }
```

---

#### Task 3d: Migrate c2-http (4 files, 24 sites)

- [ ] **Step 9: Migrate `transport/c2-http/src/relay/router.rs`** (12 sites)

This file only imports `use std::sync::Arc;` (line 3) — no Mutex/RwLock import. The `RwLock` type flows from `RelayState.pool` (defined in state.rs). **No import change needed.** Just remove `.unwrap()` from 12 `.read().unwrap()` / `.write().unwrap()` calls.

- [ ] **Step 10: Migrate `transport/c2-http/src/relay/server.rs`** (6 sites)

Import change (line 13):
```rust
// BEFORE: use std::sync::{Arc, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::RwLock;
```
Remove `.unwrap()` from 6 calls.

- [ ] **Step 11: Migrate `transport/c2-http/src/relay/state.rs`** (1 site)

Import change (line 9):
```rust
// BEFORE: use std::sync::{Arc, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::RwLock;
```
Remove `.unwrap()` from 1 call.

- [ ] **Step 12: Migrate `transport/c2-http/src/client/pool.rs`** (5 sites)

Import change (line 4):
```rust
// BEFORE: use std::sync::{Arc, Mutex, OnceLock};
// AFTER:
use std::sync::{Arc, OnceLock};
use parking_lot::Mutex;
```
Remove `.unwrap()` from 5 calls.

---

#### Task 3e: Migrate c2-ffi (5 files, 43 unwrap + 14 if-let-Ok)

- [ ] **Step 13: Migrate `bridge/c2-ffi/src/mem_ffi.rs`** (17 unwrap + 4 if-let-Ok)

Import change (line 16):
```rust
// BEFORE: use std::sync::{Arc, Mutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
```
Remove `.unwrap()` from 17 calls. Rewrite 4 `if let Ok` patterns (in Drop impls at lines ~604, 606, 753, 755) to direct lock acquisition.

- [ ] **Step 14: Migrate `bridge/c2-ffi/src/client_ffi.rs`** (8 unwrap + 5 if-let-Ok)

Import change (line 11):
```rust
// BEFORE: use std::sync::{Arc, Mutex as StdMutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
```
Rename all `StdMutex` → `Mutex` in file body (no tokio::sync::Mutex conflict in this file).

Remove `.unwrap()` from 8 calls. Rewrite 5 `if let Ok` patterns (in Drop/release methods at lines ~143, 152, 234, 239, 249) to direct lock acquisition.

- [ ] **Step 15: Migrate `bridge/c2-ffi/src/shm_buffer.rs`** (5 unwrap + 5 if-let-Ok)

Import change (line 8):
```rust
// BEFORE: use std::sync::{Arc, Mutex, RwLock};
// AFTER:
use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
```
Remove `.unwrap()` from 5 calls. Rewrite 5 `if let Ok` patterns (in Drop/release methods at lines ~151, 157, 248, 254, 259) to direct lock acquisition.

- [ ] **Step 16: Migrate `bridge/c2-ffi/src/server_ffi.rs`** (5 unwrap)

Import change (line 12):
```rust
// BEFORE: use std::sync::{Arc, Mutex};
// AFTER:
use std::sync::Arc;
use parking_lot::Mutex;
```
Remove `.unwrap()` from 5 calls.

- [ ] **Step 17: Migrate `bridge/c2-ffi/src/relay_ffi.rs`** (8 unwrap)

Import change (line 11):
```rust
// BEFORE: use std::sync::Mutex;
// AFTER:
use parking_lot::Mutex;
```
Remove `.unwrap()` from 8 calls.

---

- [ ] **Step 18: Verify full compilation**

```bash
cd src/c_two/_native && cargo check --workspace
```
Expected: Clean compilation, no errors.

- [ ] **Step 19: Run Rust tests**

```bash
cd src/c_two/_native && cargo test --workspace
```
Expected: All tests pass.

- [ ] **Step 20: Verify no leftover std Mutex/RwLock**

```bash
grep -rn "std::sync::Mutex\|std::sync::RwLock" src/c_two/_native/ --include="*.rs" | grep -v "^.*:.*//.*std"
```
Expected: Zero results (no remaining std Mutex/RwLock except in comments).

- [ ] **Step 21: Commit**

```bash
git add -A && git commit -m "refactor: migrate std::sync → parking_lot (163 + 16 sites)

Replace std::sync::{Mutex, RwLock} with parking_lot equivalents across
all 5 Rust crates (17 files). Eliminates lock poisoning risk — parking_lot
locks never poison, removing 163 .unwrap() calls and simplifying 16
defensive if-let-Ok patterns in Drop impls.

Crate breakdown:
- c2-ipc: 49 sites (client.rs, pool.rs, response.rs, sync_client.rs)
- c2-ffi: 57 sites (mem_ffi.rs, client_ffi.rs, shm_buffer.rs, server_ffi.rs, relay_ffi.rs)
- c2-server: 28 sites (connection.rs, server.rs, dispatcher.rs)
- c2-http: 24 sites (relay/router.rs, relay/server.rs, relay/state.rs, client/pool.rs)
- c2-wire: 21 sites (chunk/registry.rs)"
```

---

### Task 4: GC Task Graceful Shutdown (Issue 3)

**Files:** (all paths relative to `src/c_two/_native/`)
- Modify: `transport/c2-server/src/server.rs` (lines 200-213)

- [ ] **Step 1: Add shutdown signal to GC task**

Replace the GC spawn block (lines 200-213) with a shutdown-aware version:

```rust
// BEFORE (lines 200-213):
        // Spawn periodic GC sweep for expired chunk assemblies.
        let gc_registry = self.chunk_registry.clone();
        let gc_interval = self.chunk_registry.config().gc_interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gc_interval);
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                let stats = gc_registry.gc_sweep();
                if stats.expired > 0 {
                    info!(expired = stats.expired, freed = stats.freed_bytes, "chunk GC sweep");
                }
            }
        });

// AFTER:
        // Spawn periodic GC sweep for expired chunk assemblies.
        let gc_registry = self.chunk_registry.clone();
        let gc_interval = self.chunk_registry.config().gc_interval;
        let mut gc_shutdown = self.shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gc_interval);
            interval.tick().await; // skip first immediate tick
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

- [ ] **Step 2: Verify compilation**

```bash
cd src/c_two/_native && cargo check -p c2-server
```
Expected: Clean compilation.

- [ ] **Step 3: Run server tests**

```bash
cd src/c_two/_native && cargo test -p c2-server
```
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "fix: add graceful shutdown to chunk GC task

GC task now listens to the existing watch::channel shutdown signal
via tokio::select!, exiting promptly on server.shutdown() instead of
running until the tokio runtime is dropped."
```

---

### Task 5: Final Verification

- [ ] **Step 1: Rebuild Python extension**

```bash
uv sync --reinstall-package c-two
```

- [ ] **Step 2: Run full Python test suite**

```bash
C2_RELAY_ADDRESS= C2_ENV_FILE= uv run pytest tests/ -q --timeout=30
```
Expected: All 553 tests pass.

- [ ] **Step 3: Verify grep cleanliness**

```bash
# No std::sync::Mutex or RwLock remaining (except comments)
grep -rn "std::sync::Mutex\|std::sync::RwLock" src/c_two/_native/ --include="*.rs"

# No #[allow(dead_code)] for the 3 fixed fields
grep -rn "allow(dead_code)" src/c_two/_native/ --include="*.rs"

# No remaining .lock().unwrap() / .read().unwrap() / .write().unwrap()
grep -rn "\.lock()\.unwrap()\|\.read()\.unwrap()\|\.write()\.unwrap()" src/c_two/_native/ --include="*.rs"
```
Expected: All three greps return zero results.

- [ ] **Step 4: Mark spec as Implemented**

In `docs/superpowers/specs/2026-04-06-code-quality-remediation-design.md`, change:
```
**Status:** Proposed
```
to:
```
**Status:** Implemented
```

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "docs: mark code quality remediation as implemented"
```
