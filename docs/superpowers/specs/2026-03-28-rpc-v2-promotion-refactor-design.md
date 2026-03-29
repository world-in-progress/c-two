# rpc_v2 Promotion Refactor: `rpc/` → `transport/`

**Date**: 2026-03-28
**Status**: Approved
**Scope**: Promote `rpc_v2/` as sole RPC implementation, remove all v1 legacy, restructure into `transport/` package

---

## Problem Statement

The C-Two codebase carries two parallel RPC implementations:

- **`rpc/`** (v1) — 40 files, ~7600 LOC. Full transport stack (thread, memory, tcp/zmq, http, ipc-v2, ipc-v3) with legacy event system, v1 Server/Client/Router.
- **`rpc_v2/`** — 12 files, ~5900 LOC. SOTA API (`cc.register`/`cc.connect`), multi-CRM hosting, wire v2 codec, SharedClient, IPC-v3 + thread + HTTP relay.

`rpc_v2/` depends on 5 shared modules that still live inside `rpc/`. This coupling prevents clean deletion of v1 code and creates confusing import paths.

**Goal**: Consolidate into a single `transport/` package with clean internal organization, remove ~30 v1-only files, and establish a clear three-tier architecture (crm → transport → compo).

---

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Target module name | `transport/` | Semantic: reflects its role as the transmission layer |
| Shared infra location | `transport/ipc/` subpackage | Unified under transport layer |
| `transferable.py` | → `crm/transferable.py` | Serialization is CRM domain logic |
| `compo/` layer | Rewrite to use `cc.connect()` SOTA API | Remove v1 Client dependency entirely |
| Old examples | Delete v1 + Grid examples | v2 examples already exist, no reference value |
| Old benchmarks | Delete v1-only benchmarks | Keep only rpc_v2 and universal benchmarks |
| Legacy protocols | Remove memory://, tcp://(ZMQ), ipc-v2:// | Only IPC-v3 + thread + HTTP relay needed |
| Dependencies | Remove `pyzmq`, `watchdog` | v1-only, no longer used |
| Migration strategy | Phased with atomic commits | Bisectable, tests green at every step |

---

## Target Package Structure

```
src/c_two/
├── __init__.py              # Public API: cc.register, cc.connect, cc.close, ...
├── error.py                 # CCError hierarchy
├── cli.py                   # c3 CLI
│
├── crm/                     # CRM Layer (domain logic)
│   ├── __init__.py
│   ├── meta.py              # @cc.icrm, @cc.read, @cc.write, @cc.on_shutdown
│   ├── template.py          # CRM template generation
│   └── transferable.py      # @cc.transferable (migrated from rpc/)
│
├── compo/                   # Component Layer (rewritten to use SOTA API)
│   ├── __init__.py
│   └── runtime_connect.py   # @cc.runtime.connect → uses cc.connect() internally
│
├── transport/               # Transport Layer (promoted from rpc_v2/)
│   ├── __init__.py          # Module exports
│   ├── registry.py          # cc.register/connect/close/shutdown
│   ├── config.py            # C2Settings (pydantic, C2_IPC_ADDRESS env)
│   ├── protocol.py          # HandshakeV5, RouteInfo, capability negotiation
│   ├── wire.py              # Wire v2 codec (control-plane routing)
│   ├── wire_v1.py           # Wire v1 codec (v1-compat fallback, preregister_methods)
│   │
│   ├── server/              # Server-side
│   │   ├── __init__.py
│   │   ├── core.py          # ServerV2 (asyncio multi-CRM hosting)
│   │   └── scheduler.py     # Read/write concurrency scheduler
│   │
│   ├── client/              # Client-side
│   │   ├── __init__.py
│   │   ├── core.py          # SharedClient (multiplexed IPC v3)
│   │   ├── pool.py          # ClientPool (ref-counted lifecycle)
│   │   ├── proxy.py         # ICRMProxy (thread-local + IPC modes)
│   │   └── http.py          # HTTP client transport
│   │
│   ├── relay/               # HTTP relay
│   │   ├── __init__.py
│   │   └── core.py          # RelayV2 (Starlette + uvicorn bridge)
│   │
│   └── ipc/                 # IPC v3 protocol infrastructure
│       ├── __init__.py
│       ├── frame.py          # Frame codec, SHM utils, IPCConfig (from ipc_protocol.py)
│       ├── buddy.py          # Buddy allocator protocol (from ipc_v3_protocol.py)
│       └── msg_type.py       # MsgType enum (from rpc/event/msg_type.py)
│
├── buddy/                   # Rust buddy allocator (unchanged)
├── relay/                   # NativeRelay Rust wrapper (unchanged)
├── mcp/                     # MCP integration (unchanged)
├── seed/                    # Docker builder (unchanged)
└── _native/                 # Rust FFI workspace (unchanged)
```

### Three-Tier Architecture

```
┌─────────────────────────────────────────────────────────┐
│  compo/                                                 │
│  "WHO initiates calls"                                  │
│  @cc.runtime.connect → cc.connect() internally          │
├─────────────────────────────────────────────────────────┤
│  transport/                                             │
│  "HOW data is transmitted"                              │
│  registry, server, client, wire, IPC, relay             │
├─────────────────────────────────────────────────────────┤
│  crm/                                                   │
│  "WHAT is being called"                                 │
│  @cc.icrm, @cc.transferable, meta, template             │
└─────────────────────────────────────────────────────────┘
```

Dependencies flow downward: `compo/ → transport/ → crm/`. No upward or circular dependencies.

---

## Public API (Post-Refactor)

### Preserved (source changes, behavior identical)

```python
import c_two as cc

# CRM definition
@cc.icrm(namespace='...', version='...')
@cc.read / @cc.write
@cc.transferable
@cc.on_shutdown

# SOTA API (source: transport/registry.py)
cc.register(ICRMClass, crm, name='...')
cc.connect(ICRMClass, name='...', address='...')
cc.close(proxy)
cc.unregister(name='...')
cc.set_address('ipc-v3://...')
cc.set_ipc_config(...)
cc.server_address()
cc.shutdown()
cc.serve(...)

# Component layer (rewritten internals, same interface)
@cc.runtime.connect
cc.runtime.connect_crm()
```

### Removed

| Symbol | Replacement |
|--------|-------------|
| `cc.rpc` | Deleted — use `cc.register` + `cc.serve` |
| `cc.rpc.Server` | `cc.register()` + `cc.serve()` |
| `cc.rpc.Client` | `cc.connect()` |
| `cc.rpc.ServerConfig` | `cc.register()` params |
| `cc.rpc.Router` / `RouterConfig` | `RelayV2` / `NativeRelay` |

---

## Migration Phases

### Phase 1: Create `transport/` skeleton, migrate shared infrastructure

**Files created:**
- `transport/__init__.py`
- `transport/ipc/__init__.py`
- `transport/ipc/frame.py` ← copy from `rpc/ipc/ipc_protocol.py`
- `transport/ipc/buddy.py` ← copy from `rpc/ipc/ipc_v3_protocol.py`
- `transport/ipc/msg_type.py` ← copy from `rpc/event/msg_type.py`
- `transport/wire_v1.py` ← copy from `rpc/util/wire.py`
- `rpc/util/wait.py` — inline the single `wait()` function into `transport/relay/core.py` (only consumer); do not create a separate file
- `transport/server/__init__.py`, `transport/client/__init__.py`, `transport/relay/__init__.py` (empty stubs)

**Files modified:**
- Original `rpc/` locations get re-export shims (e.g., `rpc/ipc/ipc_protocol.py` → `from c_two.transport.ipc.frame import *`)
- This ensures `rpc_v2/` and all other consumers continue working without changes

**Validation:** All 855 tests pass. No external behavior change.

### Phase 2: Migrate `rpc_v2/` contents into `transport/`

**File moves:**
| Source | Destination |
|--------|-------------|
| `rpc_v2/server.py` | `transport/server/core.py` |
| `rpc_v2/scheduler.py` | `transport/server/scheduler.py` |
| `rpc_v2/client.py` | `transport/client/core.py` |
| `rpc_v2/pool.py` | `transport/client/pool.py` |
| `rpc_v2/proxy.py` | `transport/client/proxy.py` |
| `rpc_v2/http_client.py` | `transport/client/http.py` |
| `rpc_v2/relay.py` | `transport/relay/core.py` |
| `rpc_v2/wire.py` | `transport/wire.py` |
| `rpc_v2/protocol.py` | `transport/protocol.py` |
| `rpc_v2/registry.py` | `transport/registry.py` |
| `rpc_v2/config.py` | `transport/config.py` |

**Import updates:**
- All internal imports within moved files: `from ..rpc.ipc.ipc_protocol import X` → `from .ipc.frame import X`
- All internal imports within moved files: `from ..rpc.event.msg_type import X` → `from .ipc.msg_type import X`
- All internal imports within moved files: `from ..rpc.util.wire import X` → `from .wire_v1 import X`
- Cross-references between moved files update to new subpackage paths

**`rpc_v2/` shim:** Replace contents with re-exports from `transport/` for one phase of compatibility.

**Validation:** All 855 tests pass.

### Phase 3: Migrate `transferable.py`, rewrite `compo/`

**`transferable.py` migration:**
- Copy `rpc/transferable.py` → `crm/transferable.py`
- Update `crm/meta.py`: `from .transferable import auto_transfer` (was `from ..rpc.transferable`)
- Update `crm/meta.py`: `from ..transport.wire_v1 import preregister_methods` (was `from ..rpc.util.wire`)
- Update `c_two/__init__.py`: `from .crm.transferable import transferable` (was `from .rpc`)

**`compo/runtime_connect.py` rewrite:**
- Remove `from ..rpc import Client` dependency
- `connect_crm()` context manager: internally call `cc.connect()` / `cc.close()`
- `@connect` decorator: inject `cc.connect()` result as first arg, `cc.close()` on exit
- Preserve exact public interface and behavior

**Validation:** All tests pass, especially `test_component_runtime.py` (14 tests).

### Phase 4: Delete `rpc/`, clean peripherals

**Delete:**
- `src/c_two/rpc/` — entire directory (~40 files)
- `src/c_two/rpc_v2/` — shim directory (if still present)
- `examples/server.py`, `examples/client.py`, `examples/crm.py`, `examples/icrm.py`, `examples/example_addresses.py`
- `examples/grid/` — entire Grid domain example directory
- v1-only benchmarks: `memory_benchmark.py`, `ipc_v2_vs_v3_benchmark.py`, `memory_vs_ipc_v3_bench.py`, `adaptive_buffer_benchmark.py`, `thread_vs_ipc_benchmark.py`, `concurrency_benchmark.py`

**`pyproject.toml` changes:**
- Remove `pyzmq` from dependencies
- Remove `watchdog` from dependencies

**`tests/conftest.py` changes:**
- Remove `memory://` protocol from `protocol_address` fixture
- Remove `tcp://` protocol from `protocol_address` fixture
- Remove `ipc-v2://` protocol from `protocol_address` fixture
- Keep: `thread://`, `ipc://`, `ipc-v3://`, `http://`

**`c_two/__init__.py` changes:**
- Remove `from . import rpc` line
- All transport symbols now sourced from `transport/`

**`cli.py` changes:**
- `from .rpc.ipc.ipc_protocol import IPCConfig` → `from .transport.ipc.frame import IPCConfig`

**Rename v2 examples:**
- `v2_server.py` → `server.py`
- `v2_client.py` → `client.py`
- `v2_local.py` → `local.py`
- `v2_relay_server.py` → `relay_server.py`
- `v2_relay_client.py` → `relay_client.py`

**Validation:** Full test suite passes. Test count may decrease slightly (fewer protocol parameterizations).

---

## Dependency Changes

### Before (rpc_v2 → rpc dependency graph)

```
rpc_v2/client.py ──→ rpc/event/msg_type.py
                 ──→ rpc/util/wire.py
                 ──→ rpc/ipc/ipc_protocol.py
                 ──→ rpc/ipc/ipc_v3_protocol.py

rpc_v2/server.py ──→ rpc/event/msg_type.py
                 ──→ rpc/util/wire.py
                 ──→ rpc/ipc/ipc_protocol.py
                 ──→ rpc/ipc/ipc_v3_protocol.py

rpc_v2/wire.py   ──→ rpc/ipc/ipc_protocol.py
                 ──→ rpc/ipc/ipc_v3_protocol.py

rpc_v2/pool.py     ──→ rpc/ipc/ipc_protocol.py
rpc_v2/registry.py ──→ rpc/ipc/ipc_protocol.py
rpc_v2/relay.py    ──→ rpc/util/wait.py
                   ──→ rpc/ipc/ipc_protocol.py
```

### After (all internal to transport/)

```
transport/server/core.py ──→ transport/ipc/msg_type.py
                         ──→ transport/wire_v1.py
                         ──→ transport/ipc/frame.py
                         ──→ transport/ipc/buddy.py

transport/client/core.py ──→ transport/ipc/msg_type.py
                         ──→ transport/wire_v1.py
                         ──→ transport/ipc/frame.py
                         ──→ transport/ipc/buddy.py

transport/wire.py        ──→ transport/ipc/frame.py
                         ──→ transport/ipc/buddy.py

(All imports are now package-internal: from .ipc.frame import ...)
```

Zero cross-package dependencies for protocol code.

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Import path breakage | Re-export shims in Phase 1-2; deleted only in Phase 4 |
| `compo/` rewrite breaks 14 tests | `test_component_runtime.py` validates exact behavior |
| `preregister_methods` hot-path | Import path change only, no logic change |
| Test count drop | Expected — fewer protocol parameterizations, not lost coverage |
| Phase rollback needed | Each phase is one atomic commit; `git revert` safe |
| Python 3.14t compat | All thread-safety fixes from prior review (a402a9e) preserved |

---

## Files Deleted Summary

| Category | Count | Examples |
|----------|-------|---------|
| v1 transport implementations | ~20 | zmq/, memory/, thread/, http/, ipc servers/clients |
| v1 core | ~6 | server.py, client.py, router.py, routing.py, base/ |
| v1 event system | ~4 | event.py, event_queue.py, envelope.py |
| v1 utilities | ~3 | encoding.py, adaptive_buffer.py, wait.py |
| v1 examples | ~5 | server.py, client.py, crm.py, icrm.py, grid/ |
| v1 benchmarks | ~6 | memory_*, ipc_v2_*, thread_*, concurrency_*, adaptive_* |
| **Total** | **~44 files** | |

## pyproject.toml Dependency Removals

| Package | Reason |
|---------|--------|
| `pyzmq` | ZMQ tcp:// transport removed |
| `watchdog` | memory:// file-watching transport removed |
