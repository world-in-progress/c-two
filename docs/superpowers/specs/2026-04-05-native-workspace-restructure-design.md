# Native Workspace Restructure Design

**Date**: 2026-04-05  
**Status**: Draft  
**Scope**: Reorganize `src/c_two/_native/` Cargo workspace — directory grouping + crate merges

## Problem

The `_native` workspace has 9 Rust crates in a flat directory layout. This makes it hard to see the architectural layers at a glance, and several crates are too small to justify independence (c2-chunk: 764 LOC, c2-http: 361 LOC).

## Decision

**Approach A (modified)**: 9 → 7 crates with 4-layer directory hierarchy.

### Merges

1. **c2-chunk → c2-wire** (protocol affinity: chunk lifecycle is part of wire protocol)
2. **c2-relay → c2-http** (HTTP transport layer: client + relay server under one crate)

### Crates Kept Independent

- **c2-config** — zero-dependency config types, depended on by 6 crates
- **c2-mem** — SHM + buddy allocator (3366 LOC, fundamental infrastructure)
- **c2-ipc** — UDS+SHM async client (distinct transport from HTTP)
- **c2-server** — tokio UDS server (distinct from client)
- **c2-ffi** — PyO3 bridge (must be separate)

## Target Structure

```
_native/
├── Cargo.toml                    # workspace manifest
├── foundation/
│   ├── c2-config/                # (381 LOC) config types
│   └── c2-mem/                   # (3366 LOC) SHM + buddy allocator
├── protocol/
│   └── c2-wire/                  # (2747 LOC) wire codec + chunk lifecycle
│       └── src/
│           ├── chunk/            # ← merged from c2-chunk
│           │   ├── mod.rs
│           │   ├── config.rs
│           │   ├── registry.rs
│           │   └── promote.rs
│           ├── assembler.rs
│           ├── frame.rs
│           └── ...
├── transport/
│   ├── c2-ipc/                   # (2385 LOC) UDS+SHM async client
│   ├── c2-server/                # (2645 LOC) tokio UDS server
│   └── c2-http/                  # (1327 LOC) HTTP transport layer
│       └── src/
│           ├── client/           # ← original c2-http (reqwest)
│           │   ├── mod.rs
│           │   ├── client.rs
│           │   └── pool.rs
│           ├── relay/            # ← merged from c2-relay
│           │   ├── mod.rs
│           │   ├── server.rs
│           │   ├── router.rs
│           │   ├── state.rs
│           │   └── config.rs
│           ├── main.rs           # relay binary entry point
│           └── lib.rs
└── bridge/
    └── c2-ffi/                   # (2887 LOC) PyO3 bindings
```

## Dependency Graph (After)

```
Layer 0 (foundation):  c2-config (no c2 deps)
Layer 1 (foundation):  c2-mem → c2-config
Layer 2 (protocol):    c2-wire → c2-mem, c2-config
Layer 3 (transport):   c2-ipc → c2-config, c2-wire, c2-mem
                       c2-server → c2-config, c2-wire, c2-mem
                       c2-http → c2-config, c2-wire, c2-ipc (relay needs IPC upstream)
Layer 4 (bridge):      c2-ffi → all above
```

No dependency cycles. Each layer only depends on layers below it.

## Merge Details

### 1. c2-chunk → c2-wire

**Rationale**: ChunkRegistry manages wire-level chunk reassembly lifecycle. It's tightly coupled to wire framing concepts.

**File mapping**:
| Source (c2-chunk) | Destination (c2-wire) |
|---|---|
| `src/config.rs` | `src/chunk/config.rs` |
| `src/registry.rs` | `src/chunk/registry.rs` |
| `src/promote.rs` | `src/chunk/promote.rs` |
| `src/lib.rs` | `src/chunk/mod.rs` (re-exports) |

**Dependency change**: c2-wire gains `c2-config` dependency (inherited from c2-chunk).

**Import updates**:
- `c2-ipc`: `c2_chunk::` → `c2_wire::chunk::`
- `c2-server`: `c2_chunk::` → `c2_wire::chunk::`
- `c2-ffi`: no direct c2-chunk usage (goes through server/ipc)

### 2. c2-relay → c2-http

**Rationale**: Both are the HTTP transport layer — one is client-side (connecting TO relay), the other is server-side (the relay process itself). The combined crate represents "HTTP transport".

**File mapping**:
| Source (c2-relay) | Destination (c2-http) |
|---|---|
| `src/server.rs` | `src/relay/server.rs` |
| `src/router.rs` | `src/relay/router.rs` |
| `src/state.rs` | `src/relay/state.rs` |
| `src/config.rs` | `src/relay/config.rs` |
| `src/lib.rs` | `src/relay/mod.rs` (re-exports) |
| `src/main.rs` | `src/main.rs` (stays at crate root) |

**Original c2-http files** move into `src/client/` submodule:
| Source (c2-http) | Destination |
|---|---|
| `src/client.rs` | `src/client/client.rs` |
| `src/pool.rs` | `src/client/pool.rs` |
| `src/lib.rs` | `src/client/mod.rs` |

**Dependency change**: c2-http gains c2-config, c2-wire, c2-ipc, axum, tower (from relay).

**Import updates**:
- `c2-ffi/relay_ffi.rs`: `c2_relay::` → `c2_http::relay::`
- `c2-ffi/http_ffi.rs`: `c2_http::HttpClient` → `c2_http::client::HttpClient` (etc.)
- `c2-ffi/Cargo.toml`: remove `c2-relay` dep, keep `c2-http`

## Workspace Cargo.toml

```toml
[workspace]
resolver = "2"
members = [
    "foundation/c2-config",
    "foundation/c2-mem",
    "protocol/c2-wire",
    "transport/c2-ipc",
    "transport/c2-server",
    "transport/c2-http",
    "bridge/c2-ffi",
]

[workspace.package]
edition = "2024"
version = "0.1.0"
```

## External Config Updates

### pyproject.toml
```toml
[tool.maturin]
manifest-path = "src/c_two/_native/bridge/c2-ffi/Cargo.toml"
```

### Python Side
No Python code changes needed — `from c_two._native import ...` stays the same. The FFI module continues to export the same pyclass names to a flat namespace.

## Implementation Order

Phased approach to avoid intermediate broken states:

1. **Phase 1: Directory moves** — `git mv` all crates into layer directories, update workspace `Cargo.toml` members + all `path = "..."` references. Verify: `cargo check --workspace`.

2. **Phase 2: Merge c2-chunk → c2-wire** — Move chunk source files into `c2-wire/src/chunk/`, update c2-wire's `Cargo.toml` and `lib.rs`, update downstream imports (c2-ipc, c2-server). Remove c2-chunk directory. Verify: `cargo check --workspace && cargo test -p c2-wire`.

3. **Phase 3: Merge c2-relay → c2-http** — Move relay source into `c2-http/src/relay/`, move original http files into `c2-http/src/client/`, merge Cargo.toml dependencies, update c2-ffi imports. Remove c2-relay directory. Verify: `cargo check --workspace && cargo test -p c2-http`.

4. **Phase 4: Update external configs** — Update `pyproject.toml` manifest-path, rebuild with `uv sync --reinstall-package c-two`, run full Python test suite.

5. **Phase 5: Documentation** — Update copilot-instructions.md crate table, README Rust layer description.

## Testing Strategy

- After each phase: `cargo check --workspace` (fast compilation check)
- After Phase 2: `cargo test -p c2-wire --no-default-features` (chunk tests now in wire)
- After Phase 3: `cargo test -p c2-http` (relay + client tests)
- After Phase 4: `C2_RELAY_ADDRESS= C2_ENV_FILE= uv run pytest tests/ -q --timeout=30` (full Python suite)
- No new tests needed — existing tests cover all functionality, only import paths change

## Risks

1. **Cargo.lock churn**: Moving crates changes paths but not versions. Lock file will regenerate cleanly.
2. **maturin path**: Must update `pyproject.toml` or build will fail. Caught in Phase 4.
3. **CI paths**: If CI references specific crate paths, those need updating. Check `.github/workflows/`.
4. **Relay binary**: `main.rs` must stay at crate root (`c2-http/src/main.rs`). Currently the binary is named `c2-relay` (from `name = "c2-relay"` in Cargo.toml). After merge, add explicit `[[bin]] name = "c2-relay"` in c2-http's Cargo.toml to preserve the binary name. The library name remains `c2_http`.
