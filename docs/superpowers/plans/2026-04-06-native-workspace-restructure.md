# Native Workspace Restructure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize the `src/c_two/_native/` Cargo workspace from 9 flat crates to 7 layered crates with directory grouping.

**Architecture:** Move all crates into 4-layer directories (foundation/protocol/transport/bridge). Merge c2-chunk into c2-wire (protocol affinity). Merge c2-relay into c2-http with feature gate (HTTP transport layer). All changes are purely structural — no logic changes, no new features.

**Tech Stack:** Rust (Cargo workspace), PyO3/maturin, Python (pytest)

**Design Spec:** `docs/superpowers/specs/2026-04-05-native-workspace-restructure-design.md`

---

## File Structure

After this plan, the workspace looks like:

```
src/c_two/_native/
├── Cargo.toml                          # workspace manifest (updated members)
├── foundation/
│   ├── c2-config/Cargo.toml            # path deps updated
│   └── c2-mem/Cargo.toml               # path deps updated
├── protocol/
│   └── c2-wire/
│       ├── Cargo.toml                  # gains c2-config dep, loses features/no_std
│       └── src/
│           ├── lib.rs                  # updated: chunk becomes subdir module
│           └── chunk/                  # NEW directory
│               ├── mod.rs              # re-exports header + registry types
│               ├── header.rs           # RENAMED from chunk.rs
│               ├── config.rs           # MOVED from c2-chunk
│               ├── registry.rs         # MOVED from c2-chunk (imports updated)
│               └── promote.rs          # MOVED from c2-chunk (imports updated)
├── transport/
│   ├── c2-ipc/Cargo.toml              # path deps updated, c2-chunk → c2-wire
│   ├── c2-server/Cargo.toml           # path deps updated, c2-chunk → c2-wire
│   └── c2-http/
│       ├── Cargo.toml                 # merged deps, feature gate for relay
│       └── src/
│           ├── lib.rs                 # NEW: pub mod client + #[cfg] pub mod relay
│           ├── main.rs                # MOVED from c2-relay (imports updated)
│           ├── client/
│           │   ├── mod.rs             # re-exports HttpClient, HttpClientPool, HttpError
│           │   ├── client.rs          # MOVED from c2-http/src/client.rs
│           │   └── pool.rs            # MOVED from c2-http/src/pool.rs
│           └── relay/
│               ├── mod.rs             # re-exports RelayConfig, RelayServer, RelayState
│               ├── server.rs          # MOVED from c2-relay (imports updated)
│               ├── router.rs          # MOVED from c2-relay (imports updated)
│               ├── state.rs           # MOVED from c2-relay (imports updated)
│               └── config.rs          # MOVED from c2-relay
└── bridge/
    └── c2-ffi/
        ├── Cargo.toml                 # path deps updated, c2-relay → c2-http[relay]
        └── src/
            ├── http_ffi.rs            # import path updated
            └── relay_ffi.rs           # import path updated

DELETED: c2-chunk/, c2-relay/ (original flat directories)
```

**Files modified but not moved:** `pyproject.toml`, `.github/copilot-instructions.md`

---

### Task 1: Create layer directories and move crates

**Files:**
- Modify: `src/c_two/_native/Cargo.toml`
- Move: all 9 crate directories into layer subdirectories

- [ ] **Step 1: Create layer directories and git-mv crates**

```bash
cd src/c_two/_native

# Create layer directories
mkdir -p foundation protocol transport bridge

# Move crates into layers
git mv c2-config foundation/
git mv c2-mem    foundation/
git mv c2-wire   protocol/
git mv c2-chunk  protocol/       # temporary — will be merged in Task 3
git mv c2-ipc    transport/
git mv c2-http   transport/
git mv c2-server transport/
git mv c2-relay  transport/      # temporary — will be merged in Task 4
git mv c2-ffi   bridge/
```

- [ ] **Step 2: Update workspace Cargo.toml members**

Replace `src/c_two/_native/Cargo.toml` with:

```toml
[workspace]
resolver = "2"
members = [
    "foundation/c2-config",
    "foundation/c2-mem",
    "protocol/c2-wire",
    "protocol/c2-chunk",
    "transport/c2-ipc",
    "transport/c2-http",
    "transport/c2-server",
    "transport/c2-relay",
    "bridge/c2-ffi",
]

[workspace.package]
edition = "2024"
version = "0.1.0"
```

Note: c2-chunk and c2-relay are still listed — they get removed in Tasks 3 and 4.

- [ ] **Step 3: Update all inter-crate path dependencies**

Every `Cargo.toml` that references another crate via `path = "../c2-xxx"` needs updating. The new relative paths depend on the layer each crate is in.

**foundation/c2-config/Cargo.toml** — no deps, no changes needed.

**foundation/c2-mem/Cargo.toml** — update:
```toml
c2-config = { path = "../c2-config" }
```
(Same as before — both in `foundation/`, relative path unchanged.)

**protocol/c2-wire/Cargo.toml** — update:
```toml
c2-mem = { path = "../../foundation/c2-mem", optional = true }
```

**protocol/c2-chunk/Cargo.toml** — update:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-mem = { path = "../../foundation/c2-mem" }
c2-wire = { path = "../c2-wire", features = ["std"] }
```

**transport/c2-ipc/Cargo.toml** — update:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-chunk = { path = "../../protocol/c2-chunk" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

**transport/c2-server/Cargo.toml** — update:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-chunk = { path = "../../protocol/c2-chunk" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

**transport/c2-http/Cargo.toml** — no c2 deps, no changes needed.

**transport/c2-relay/Cargo.toml** — update:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-ipc = { path = "../c2-ipc" }
```

**bridge/c2-ffi/Cargo.toml** — update:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-mem = { path = "../../foundation/c2-mem" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-relay = { path = "../../transport/c2-relay" }
c2-ipc = { path = "../../transport/c2-ipc" }
c2-http = { path = "../../transport/c2-http" }
c2-server = { path = "../../transport/c2-server" }
```

- [ ] **Step 4: Verify compilation**

```bash
cd src/c_two/_native && cargo check --workspace
```

Expected: compiles with no errors (warnings OK).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: move native crates into layered directories

foundation/ : c2-config, c2-mem
protocol/   : c2-wire, c2-chunk
transport/  : c2-ipc, c2-http, c2-server, c2-relay
bridge/     : c2-ffi

All inter-crate path dependencies updated.
No logic changes — purely structural."
```

### Task 2: Remove c2-wire no_std feature gate (prep for merge)

c2-wire currently has `features = { default = ["std"], std = ["c2-mem"] }` making c2-mem optional. After chunk merge, c2-mem and c2-config become hard deps. Clean this up first.

**Files:**
- Modify: `src/c_two/_native/protocol/c2-wire/Cargo.toml`
- Modify: `src/c_two/_native/protocol/c2-wire/src/lib.rs`
- Modify: `src/c_two/_native/protocol/c2-wire/src/control.rs`
- Modify: `src/c_two/_native/protocol/c2-wire/src/handshake.rs`
- Modify: `src/c_two/_native/protocol/c2-wire/src/ctrl.rs`
- Modify: `src/c_two/_native/protocol/c2-wire/src/frame.rs`
- Modify: `src/c_two/_native/protocol/c2-wire/src/chunk.rs`
- Modify: `src/c_two/_native/protocol/c2-chunk/Cargo.toml` (temporary fix until Task 3 deletes it)

- [ ] **Step 1: Update c2-wire/Cargo.toml — make c2-mem mandatory, remove features**

Replace the full content with:

```toml
[package]
name = "c2-wire"
edition.workspace = true
version.workspace = true
description = "Wire protocol codec for C-Two IPC frames"

[dependencies]
c2-config = { path = "../../foundation/c2-config" }
c2-mem = { path = "../../foundation/c2-mem" }
```

Key changes:
- c2-mem is now mandatory (not optional)
- c2-config added (needed by chunk after merge)
- `[features]` section removed entirely
- `tracing` will be added in Task 3 when chunk is merged

- [ ] **Step 2: Update c2-wire/src/lib.rs — remove no_std conditionals**

Replace the full content with:

```rust
//! C-Two IPC wire protocol codec.
//!
//! Implements the binary frame format used by IPC transports:
//!
//! - **Frame header** (16 bytes): `[4B total_len LE][8B request_id LE][4B flags LE]`
//! - **Buddy payload** (11 bytes): `[2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]`
//! - **V2 call control**: `[1B name_len][route_name UTF-8][2B method_idx LE]`
//! - **V2 reply control**: `[1B status][optional: 4B error_len LE + error_bytes]`
//! - **Chunk header** (4 bytes): `[2B chunk_idx LE][2B total_chunks LE]`
//! - **Control messages**: Segment announce, consumed, buddy announce
//! - **Handshake**: Segments + capabilities + routes + method tables
//! - **MsgType**: Signal/message type enum
//!
//! All integers are little-endian.

pub mod flags;
pub mod frame;
pub mod buddy;
pub mod control;
pub mod chunk;
pub mod ctrl;
pub mod msg_type;
pub mod handshake;
pub mod assembler;

#[cfg(test)]
mod tests;
```

Key changes:
- Removed `#![cfg_attr(not(feature = "std"), no_std)]`
- Removed `extern crate alloc` and `use alloc::` imports
- Removed `#[cfg(feature = "std")]` guard on `pub mod assembler`
- `pub mod chunk` stays (will become dir module in Task 3)

- [ ] **Step 3: Update c2-chunk Cargo.toml to remove `features = ["std"]`**

c2-chunk currently depends on `c2-wire = { path = "../c2-wire", features = ["std"] }`. The `std` feature no longer exists after Step 1. Update to plain path dep:

In `protocol/c2-chunk/Cargo.toml`, change:
```toml
c2-wire = { path = "../c2-wire", features = ["std"] }
```
to:
```toml
c2-wire = { path = "../c2-wire" }
```

This is a temporary fix — c2-chunk is fully deleted in Task 3.

- [ ] **Step 4: Fix alloc:: references in all 6 affected source files**

The following files have `#[cfg(not(feature = "std"))]` guarded alloc imports or `#[cfg(feature = "std")]` guards that must be removed:

| File | What to remove |
|------|---------------|
| `lib.rs` | `#![cfg_attr(not(feature = "std"), no_std)]`, `extern crate alloc`, `use alloc::...`, `#[cfg(feature = "std")]` on `pub mod assembler` |
| `control.rs:22-23` | `#[cfg(not(feature = "std"))] use alloc::{string::String, vec::Vec};` |
| `handshake.rs:26-27` | `#[cfg(not(feature = "std"))] use alloc::{string::String, vec, vec::Vec};` |
| `ctrl.rs:18-19` | `#[cfg(not(feature = "std"))] use alloc::{string::String, vec::Vec};` |
| `frame.rs:12-13` | `#[cfg(not(feature = "std"))] use alloc::vec::Vec;` |
| `frame.rs:116,134` | `#[cfg(feature = "std")]` guards on `Display`/`Error` impl blocks |
| `chunk.rs:83-84` | `#[cfg(not(feature = "std"))] use alloc::vec;` |

Verify with:
```bash
cd src/c_two/_native/protocol/c2-wire/src
grep -rn 'cfg.*feature.*std\|extern crate alloc\|use alloc::' . --include='*.rs'
```

Expected: zero matches after cleanup.

- [ ] **Step 5: Verify compilation**

```bash
cd src/c_two/_native && cargo check --workspace
```

Expected: compiles cleanly. The `--no-default-features` path is intentionally broken.

- [ ] **Step 6: Run c2-wire tests**

```bash
cd src/c_two/_native && cargo test -p c2-wire
```

Expected: all existing tests pass.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor(c2-wire): remove no_std feature gate

Make c2-mem and c2-config hard dependencies. Remove #![no_std],
extern crate alloc, and all cfg(feature = \"std\") guards.

The no_std path was already broken (vec! macro error in control.rs)
and had no consumers. This prepares c2-wire for the c2-chunk merge."
```

### Task 3: Merge c2-chunk into c2-wire

**Files:**
- Create: `src/c_two/_native/protocol/c2-wire/src/chunk/mod.rs`
- Move+Rename: `chunk.rs` → `chunk/header.rs`
- Move: c2-chunk source files → `chunk/` subdir
- Modify: `src/c_two/_native/protocol/c2-wire/Cargo.toml` (add tracing dep)
- Modify: `src/c_two/_native/protocol/c2-wire/src/lib.rs` (no change needed — already has `pub mod chunk`)
- Modify: `src/c_two/_native/transport/c2-ipc/Cargo.toml` (remove c2-chunk dep)
- Modify: `src/c_two/_native/transport/c2-ipc/src/client.rs` (update imports)
- Modify: `src/c_two/_native/transport/c2-server/Cargo.toml` (remove c2-chunk dep)
- Modify: `src/c_two/_native/transport/c2-server/src/server.rs` (update imports)
- Delete: `src/c_two/_native/protocol/c2-chunk/` directory
- Modify: `src/c_two/_native/Cargo.toml` (remove c2-chunk from members)

- [ ] **Step 1: Convert chunk.rs to chunk/ directory**

```bash
cd src/c_two/_native/protocol/c2-wire/src

# Create chunk directory and move existing chunk.rs into it as header.rs
mkdir -p chunk
git mv chunk.rs chunk/header.rs
```

- [ ] **Step 2: Move c2-chunk source files into chunk/ directory**

```bash
cd src/c_two/_native

# Copy c2-chunk source files into c2-wire/src/chunk/
# (git mv doesn't work across crate boundaries cleanly, so cp + rm)
cp protocol/c2-chunk/src/config.rs   protocol/c2-wire/src/chunk/config.rs
cp protocol/c2-chunk/src/registry.rs protocol/c2-wire/src/chunk/registry.rs
cp protocol/c2-chunk/src/promote.rs  protocol/c2-wire/src/chunk/promote.rs
```

- [ ] **Step 3: Create chunk/mod.rs**

Create `src/c_two/_native/protocol/c2-wire/src/chunk/mod.rs`:

```rust
//! Chunk codec and lifecycle management.
//!
//! - `header`: chunk header encode/decode (4-byte wire format)
//! - `config`: chunk reassembly configuration
//! - `registry`: sharded lifecycle manager for in-flight chunked transfers
//! - `promote`: heap-to-SHM promotion for finished chunks

pub mod header;
pub mod config;
pub mod promote;
pub mod registry;

// Re-export header codec at chunk:: level for backward compatibility.
// Existing code uses c2_wire::chunk::encode_chunk_header etc.
pub use header::*;

// Re-export key types at chunk:: level.
pub use config::ChunkConfig;
pub use promote::promote_to_shm;
pub use registry::{ChunkRegistry, FinishedChunk, GcStats};
```

- [ ] **Step 4: Update imports in moved files**

**chunk/registry.rs** — update crate references from `c2_mem`/`c2_wire` to `crate`/`c2_mem`:

Line 12-14 currently:
```rust
use c2_mem::handle::MemHandle;
use c2_mem::MemPool;
use c2_wire::assembler::ChunkAssembler;
```

Change `c2_wire::assembler` → `crate::assembler` (now same crate):
```rust
use c2_mem::handle::MemHandle;
use c2_mem::MemPool;
use crate::assembler::ChunkAssembler;
```

Line 17-18 currently:
```rust
use crate::config::ChunkConfig;
use crate::promote::promote_to_shm;
```

Change to use full chunk submodule path:
```rust
use crate::chunk::config::ChunkConfig;
use crate::chunk::promote::promote_to_shm;
```

In test section (around line 319):
```rust
use c2_mem::config::PoolConfig;
```
This stays the same — c2_mem is still an external dep.

**chunk/promote.rs** — lines 3-4 currently:
```rust
use c2_mem::handle::MemHandle;
use c2_mem::MemPool;
```
These stay the same — c2_mem is still external.

In test section (around line 37-38):
```rust
use c2_mem::config::PoolConfig;
use c2_mem::spill::create_file_spill;
```
These stay the same.

**chunk/config.rs** — line with `c2_config::BaseIpcConfig`:
```rust
pub fn from_base(cfg: &c2_config::BaseIpcConfig) -> Self {
```
This stays the same — c2_config is still external.

- [ ] **Step 5: Add tracing dep to c2-wire/Cargo.toml**

c2-chunk used `tracing`. Add it to c2-wire:

```toml
[dependencies]
c2-config = { path = "../../foundation/c2-config" }
c2-mem = { path = "../../foundation/c2-mem" }
tracing = "0.1"
```

- [ ] **Step 6: Update downstream — c2-ipc**

**transport/c2-ipc/Cargo.toml** — remove c2-chunk dependency:

Before:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-chunk = { path = "../../protocol/c2-chunk" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

After:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

**transport/c2-ipc/src/client.rs** — line 15:

Before: `use c2_chunk::{ChunkConfig, ChunkRegistry};`
After: `use c2_wire::chunk::{ChunkConfig, ChunkRegistry};`

- [ ] **Step 7: Update downstream — c2-server**

**transport/c2-server/Cargo.toml** — remove c2-chunk dependency:

Before:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-chunk = { path = "../../protocol/c2-chunk" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

After:
```toml
c2-config = { path = "../../foundation/c2-config" }
c2-wire = { path = "../../protocol/c2-wire" }
c2-mem = { path = "../../foundation/c2-mem" }
```

**transport/c2-server/src/server.rs** — update all `c2_chunk::` references:

Find: `c2_chunk::ChunkRegistry` → Replace: `c2_wire::chunk::ChunkRegistry`
Find: `c2_chunk::ChunkConfig` → Replace: `c2_wire::chunk::ChunkConfig`

Exact lines to change (6 occurrences in server.rs):
```
chunk_registry: Arc<c2_chunk::ChunkRegistry>,
→ chunk_registry: Arc<c2_wire::chunk::ChunkRegistry>,

let chunk_config = c2_chunk::ChunkConfig::from_base(&config);
→ let chunk_config = c2_wire::chunk::ChunkConfig::from_base(&config);

let chunk_registry = Arc::new(c2_chunk::ChunkRegistry::new(
→ let chunk_registry = Arc::new(c2_wire::chunk::ChunkRegistry::new(

pub fn chunk_registry(&self) -> &Arc<c2_chunk::ChunkRegistry> {
→ pub fn chunk_registry(&self) -> &Arc<c2_wire::chunk::ChunkRegistry> {

let registry = c2_chunk::ChunkRegistry::new(
→ let registry = c2_wire::chunk::ChunkRegistry::new(

c2_chunk::ChunkConfig::default(),
→ c2_wire::chunk::ChunkConfig::default(),
```

- [ ] **Step 8: Delete c2-chunk crate and remove from workspace**

```bash
cd src/c_two/_native
rm -rf protocol/c2-chunk
```

Update `Cargo.toml` — remove `"protocol/c2-chunk"` from members list:

```toml
[workspace]
resolver = "2"
members = [
    "foundation/c2-config",
    "foundation/c2-mem",
    "protocol/c2-wire",
    "transport/c2-ipc",
    "transport/c2-http",
    "transport/c2-server",
    "transport/c2-relay",
    "bridge/c2-ffi",
]

[workspace.package]
edition = "2024"
version = "0.1.0"
```

- [ ] **Step 9: Verify compilation**

```bash
cd src/c_two/_native && cargo check --workspace
```

Expected: compiles with no errors.

- [ ] **Step 10: Run tests**

```bash
cd src/c_two/_native && cargo test -p c2-wire && cargo test -p c2-server --no-default-features && cargo test -p c2-ipc --no-default-features
```

Note: `--no-default-features` on server/ipc avoids PyO3 link issues. c2-wire tests should include the migrated chunk tests.

Expected: all tests pass.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -m "refactor: merge c2-chunk into c2-wire

- chunk.rs → chunk/header.rs (naming conflict resolution)
- c2-chunk/src/{config,registry,promote}.rs → c2-wire/src/chunk/
- New chunk/mod.rs re-exports all types at c2_wire::chunk:: level
- c2-ipc, c2-server: c2_chunk:: → c2_wire::chunk::
- c2-wire: removed no_std feature gate, c2-config + tracing added
- Deleted protocol/c2-chunk/ crate"
```

### Task 4: Merge c2-relay into c2-http (with feature gate)

**Files:**
- Modify: `src/c_two/_native/transport/c2-http/Cargo.toml` (merged deps + features)
- Create: `src/c_two/_native/transport/c2-http/src/client/mod.rs`
- Move: `c2-http/src/client.rs` → `c2-http/src/client/client.rs`
- Move: `c2-http/src/pool.rs` → `c2-http/src/client/pool.rs`
- Create: `src/c_two/_native/transport/c2-http/src/relay/mod.rs`
- Move: c2-relay source files → `c2-http/src/relay/`
- Move: `c2-relay/src/main.rs` → `c2-http/src/main.rs`
- Rewrite: `src/c_two/_native/transport/c2-http/src/lib.rs`
- Modify: `src/c_two/_native/bridge/c2-ffi/Cargo.toml`
- Modify: `src/c_two/_native/bridge/c2-ffi/src/relay_ffi.rs`
- Modify: `src/c_two/_native/bridge/c2-ffi/src/http_ffi.rs`
- Delete: `src/c_two/_native/transport/c2-relay/`
- Modify: `src/c_two/_native/Cargo.toml` (remove c2-relay from members)

- [ ] **Step 1: Restructure c2-http/src/ — create client/ submodule**

```bash
cd src/c_two/_native/transport/c2-http/src

# Create client/ directory and move existing files
mkdir -p client
git mv client.rs client/client.rs
git mv pool.rs client/pool.rs
```

Create `client/mod.rs`:

```rust
//! HTTP client for C-Two relay transport.
//!
//! Provides [`HttpClient`] for making CRM calls through an HTTP relay
//! server, and [`HttpClientPool`] for reference-counted connection pooling.

mod client;
mod pool;

pub use client::{HttpClient, HttpError};
pub use pool::HttpClientPool;
```

- [ ] **Step 2: Copy c2-relay source files into relay/ submodule**

```bash
cd src/c_two/_native

# Create relay/ directory inside c2-http
mkdir -p transport/c2-http/src/relay

# Copy relay source files
cp transport/c2-relay/src/server.rs  transport/c2-http/src/relay/server.rs
cp transport/c2-relay/src/router.rs  transport/c2-http/src/relay/router.rs
cp transport/c2-relay/src/state.rs   transport/c2-http/src/relay/state.rs
cp transport/c2-relay/src/config.rs  transport/c2-http/src/relay/config.rs

# Copy main.rs to c2-http root
cp transport/c2-relay/src/main.rs    transport/c2-http/src/main.rs
```

- [ ] **Step 3: Create relay/mod.rs**

Create `src/c_two/_native/transport/c2-http/src/relay/mod.rs`:

```rust
//! C-Two HTTP relay server — bridges HTTP requests to IPC.
//!
//! This module is behind the `relay` feature gate. Enable it with:
//! ```toml
//! c2-http = { path = "...", features = ["relay"] }
//! ```

pub mod config;
pub mod router;
pub mod server;
pub mod state;

pub use config::RelayConfig;
pub use server::RelayServer;
pub use state::RelayState;
```

- [ ] **Step 4: Update imports in moved relay files**

**relay/server.rs** — update `crate::` references:

Line 19-20 currently:
```rust
use crate::router;
use crate::state::{RelayState, UpstreamPool};
```

Change to:
```rust
use crate::relay::router;
use crate::relay::state::{RelayState, UpstreamPool};
```

**relay/router.rs** — line 15 currently:
```rust
use crate::state::RelayState;
```

Change to:
```rust
use crate::relay::state::RelayState;
```

**relay/state.rs** — no `crate::` imports, no changes needed.

**relay/config.rs** — currently just:
```rust
pub use c2_config::RelayConfig;
```
No changes needed.

- [ ] **Step 5: Update main.rs imports**

`src/c_two/_native/transport/c2-http/src/main.rs` — line 15 currently:
```rust
use c2_relay::{router, RelayState};
```

Change to:
```rust
use c2_http::{relay::router, relay::RelayState};
```

(The crate name is `c2_http` since that's the lib name.)

Wait — actually, in `main.rs` within the same crate, you should use the crate name from Cargo.toml. Since we're inside c2-http, the lib name defaults to `c2_http`. But `main.rs` binaries in the same crate access the lib via the crate name. Let me check:

The Cargo.toml has `name = "c2-http"`, so the lib is `c2_http`. The `main.rs` binary accesses it as:
```rust
use c2_http::relay::{router, RelayState};
```

Full updated `main.rs`:

```rust
//! C-Two HTTP relay binary — multi-upstream mode.
//!
//! Starts with zero upstreams. CRM processes register themselves
//! dynamically via `POST /_register`.
//!
//! Usage:
//!
//! ```bash
//! c2-relay --bind 0.0.0.0:8080
//! ```

use clap::Parser;
use tracing_subscriber::EnvFilter;

use c2_http::relay::{router, RelayState};

#[derive(Parser, Debug)]
#[command(name = "c2-relay", about = "C-Two HTTP relay server (multi-upstream)")]
struct Cli {
    /// HTTP bind address (e.g. 0.0.0.0:8080).
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(bind = %cli.bind, "starting c2-relay (multi-upstream, no initial upstreams)");

    let state = RelayState::new();
    let app = router::build_router(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("listening on {}", cli.bind);

    axum::serve(listener, app).await?;

    Ok(())
}
```

- [ ] **Step 6: Rewrite c2-http/src/lib.rs**

Replace with:

```rust
//! C-Two HTTP transport layer.
//!
//! - `client`: HTTP client for connecting to relay servers (always available)
//! - `relay`: HTTP relay server bridging HTTP→IPC (requires `relay` feature)

pub mod client;

#[cfg(feature = "relay")]
pub mod relay;
```

- [ ] **Step 7: Rewrite c2-http/Cargo.toml**

Replace the full content with:

```toml
[package]
name = "c2-http"
edition.workspace = true
version.workspace = true
description = "C-Two HTTP transport layer (client + relay server)"

[features]
default = []
relay = [
    "dep:axum",
    "dep:tower-http",
    "dep:c2-config",
    "dep:c2-wire",
    "dep:c2-ipc",
    "dep:clap",
    "dep:serde_json",
    "dep:tracing",
    "dep:tracing-subscriber",
    "dep:anyhow",
]

[dependencies]
# Always available (client)
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
tokio = { version = "1", features = ["full"] }
thiserror = "2"

# Relay-only dependencies (behind feature gate)
axum = { version = "0.8", optional = true }
tower-http = { version = "0.6", features = ["cors"], optional = true }
c2-config = { path = "../../foundation/c2-config", optional = true }
c2-wire = { path = "../../protocol/c2-wire", optional = true }
c2-ipc = { path = "../c2-ipc", optional = true }
clap = { version = "4", features = ["derive"], optional = true }
serde_json = { version = "1", optional = true }
tracing = { version = "0.1", optional = true }
tracing-subscriber = { version = "0.3", features = ["env-filter"], optional = true }
anyhow = { version = "1", optional = true }

[[bin]]
name = "c2-relay"
required-features = ["relay"]
```

- [ ] **Step 8: Update c2-ffi — Cargo.toml**

In `bridge/c2-ffi/Cargo.toml`, replace `c2-relay` dep with `c2-http` features:

Before:
```toml
c2-relay = { path = "../../transport/c2-relay" }
c2-http = { path = "../../transport/c2-http" }
```

After:
```toml
c2-http = { path = "../../transport/c2-http", features = ["relay"] }
```

(Single dep with relay feature enabled — c2-ffi needs both client and relay.)

- [ ] **Step 9: Update c2-ffi — relay_ffi.rs import**

In `bridge/c2-ffi/src/relay_ffi.rs`, line 16:

Before: `use c2_relay::server::RelayServer;`
After: `use c2_http::relay::server::RelayServer;`

- [ ] **Step 10: Update c2-ffi — http_ffi.rs import**

In `bridge/c2-ffi/src/http_ffi.rs`, line 12:

Before: `use c2_http::{HttpClient, HttpClientPool, HttpError};`
After: `use c2_http::client::{HttpClient, HttpClientPool, HttpError};`

- [ ] **Step 11: Delete c2-relay crate and remove from workspace**

```bash
cd src/c_two/_native
rm -rf transport/c2-relay
```

Update `Cargo.toml` — remove `"transport/c2-relay"` from members:

```toml
[workspace]
resolver = "2"
members = [
    "foundation/c2-config",
    "foundation/c2-mem",
    "protocol/c2-wire",
    "transport/c2-ipc",
    "transport/c2-http",
    "transport/c2-server",
    "bridge/c2-ffi",
]

[workspace.package]
edition = "2024"
version = "0.1.0"
```

- [ ] **Step 12: Verify compilation**

```bash
cd src/c_two/_native && cargo check --workspace
```

Expected: compiles with no errors.

- [ ] **Step 13: Run tests**

```bash
cd src/c_two/_native && cargo test -p c2-http --features relay
```

Expected: all tests pass (relay tests are behind feature gate).

- [ ] **Step 14: Commit**

```bash
git add -A
git commit -m "refactor: merge c2-relay into c2-http with feature gate

- c2-http/src/client.rs,pool.rs → c2-http/src/client/ submodule
- c2-relay/src/{server,router,state,config}.rs → c2-http/src/relay/
- c2-relay/src/main.rs → c2-http/src/main.rs
- Feature gate: relay module behind features = [\"relay\"]
- c2-ffi: c2_relay:: → c2_http::relay::, c2_http:: → c2_http::client::
- [[bin]] name = \"c2-relay\" with required-features = [\"relay\"]
- Deleted transport/c2-relay/ crate"
```

### Task 5: Update external configs and verify Python build

**Files:**
- Modify: `pyproject.toml` (maturin manifest-path)
- Verify: full Python test suite

- [ ] **Step 1: Update pyproject.toml**

In `pyproject.toml`, update the maturin manifest-path:

Before: `manifest-path = "src/c_two/_native/c2-ffi/Cargo.toml"`
After: `manifest-path = "src/c_two/_native/bridge/c2-ffi/Cargo.toml"`

- [ ] **Step 2: Rebuild native extension**

```bash
uv sync --reinstall-package c-two
```

Expected: maturin finds the Cargo.toml at the new path, builds successfully.

- [ ] **Step 3: Run full Python test suite**

```bash
C2_RELAY_ADDRESS= C2_ENV_FILE= uv run pytest tests/ -q --timeout=30
```

Expected: 553 tests pass (same as before restructure). Python imports are unchanged — `from c_two._native import ...` still works because c2-ffi exports the same pyclass names.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "build: update pyproject.toml manifest-path for restructured workspace

maturin manifest-path: c2-ffi → bridge/c2-ffi
All 553 Python tests pass."
```

### Task 6: Update documentation

**Files:**
- Modify: `.github/copilot-instructions.md`
- Modify: `README.md`
- Modify: `README.zh-CN.md`

- [ ] **Step 1: Update copilot-instructions.md — Rust crate table**

Find the section with "A Cargo workspace of 9 crates" and replace with:

```markdown
A Cargo workspace of 7 crates organized in 4 layers, compiled into a single `c_two._native` Python extension module:

| Layer | Crate | Purpose |
|-------|-------|---------|
| foundation | `c2-config` | Unified IPC configuration structs (Base/Server/Client), shared by all Rust crates |
| foundation | `c2-mem` | Buddy allocator, SHM regions, unified MemPool (buddy/dedicated/file-spill) |
| protocol | `c2-wire` | Wire protocol codec, frame encoding, ChunkAssembler, ChunkRegistry |
| transport | `c2-ipc` | Async IPC client (UDS + SHM), chunked transfer |
| transport | `c2-server` | Tokio-based UDS server with per-connection state and peer SHM lazy-open |
| transport | `c2-http` | HTTP client for relay transport + HTTP relay server (axum → IPC, behind `relay` feature) |
| bridge | `c2-ffi` | PyO3 bindings: `mem_ffi`, `wire_ffi`, `ipc_ffi`, `server_ffi`, `client_ffi`, `relay_ffi`, `http_ffi` |
```

- [ ] **Step 2: Update copilot-instructions.md — Rust check/test commands**

Find the Rust check/test commands section and update paths:

```bash
# Rust type-check (no PyO3 link needed)
cd src/c_two/_native && cargo check --workspace

# Rust unit tests (pure Rust, no Python linkage)
cd src/c_two/_native && cargo test -p c2-mem -p c2-wire --no-default-features
```

- [ ] **Step 3: Update README.md — Rust layer description**

Find the section describing Rust native layer and update the crate count from 9 to 7, mention the 4-layer structure.

- [ ] **Step 4: Update README.zh-CN.md — same changes as English**

Mirror the same Rust layer description changes.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: update documentation for 7-crate workspace structure

- copilot-instructions: 9→7 crates, layered table, updated commands
- README/README.zh-CN: updated Rust layer description"
```

### Task 7: Final cleanup

- [ ] **Step 1: Verify clean git state**

```bash
git status
git --no-pager log --oneline -10
```

Expected: clean working tree, 6 new commits (Tasks 1-6).

- [ ] **Step 2: Final verification**

```bash
# Rust workspace compiles
cd src/c_two/_native && cargo check --workspace

# Rust tests pass
cargo test -p c2-wire -p c2-mem

# Python tests pass (run from repo root)
cd ../../.. && C2_RELAY_ADDRESS= C2_ENV_FILE= uv run pytest tests/ -q --timeout=30
```

Expected: all green.

- [ ] **Step 3: Mark design spec as implemented** (only after Step 2 passes)

In `docs/superpowers/specs/2026-04-05-native-workspace-restructure-design.md`, change:

`**Status**: Draft` → `**Status**: Implemented`

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore: mark workspace restructure design as implemented"
```
