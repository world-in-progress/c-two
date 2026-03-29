# Server Core Decoupling Design

**Date:** 2026-03-29
**Status:** Draft
**Scope:** `src/c_two/transport/server/core.py` — 1497 lines → ~550 lines

## Problem

`server/core.py` is a 1497-line monolith with 6 distinct responsibility clusters packed into one file. The `Server` class alone spans ~1090 lines, mixing protocol parsing, SHM management, chunk reassembly, worker dispatch, and reply encoding. This makes the file difficult to reason about, test in isolation, and evolve safely — especially under free-threading Python (3.14t) where thread-safety boundaries must be crystal-clear.

## Approach: Targeted Extraction

Extract 5 independent modules from `core.py`, keeping `Server` as a thin orchestrator. Each extracted module has a single responsibility, clear input/output boundary, and zero circular dependencies.

**Not in scope:**
- Scheduler (`scheduler.py`) — already extracted, no changes needed.
- Wire protocol (`wire.py`) — separate concern, no changes.
- IPC frame/buddy modules — separate concern, no changes.

## Current Responsibility Map

```
core.py (1497 lines)
├── _parse_call_inline()          [93–113]     Pure function: frame parsing
├── _Connection                   [120–173]    Per-client state + flight tracking
├── CRMSlot                       [179–205]    Per-CRM routing metadata
├── _ChunkAssembler               [211–277]    mmap-based chunk reassembly
├── _FastDispatcher                [282–401]    Worker pool + inline reply building
└── Server                        [407–1497]   Everything else
    ├── __init__                   [437–483]    Config, slots, lifecycle state
    ├── register/unregister_crm    [492–579]    CRM registration
    ├── ICRM helpers               [585–622]    create, discover, resolve
    ├── start/shutdown/_async_main [627–707]    Lifecycle
    ├── _invoke_shutdown           [712–732]    CRM shutdown callback
    ├── _handle_client             [738–925]    Frame loop (200 lines!)
    ├── _handle_handshake/v5/segs  [930–1031]   Handshake protocol
    ├── _process_chunked_frame     [1036–1175]  Chunk handling + GC
    ├── _handle_call/_resolve_buddy[1181–1315]  Call dispatch
    ├── _dispatch/_unpack/_wrap    [1321–1373]  CRM execution
    └── _send_reply/_send_chunked  [1379–1497]  Reply encoding + sending
```

## Target Module Structure

```
server/
├── __init__.py       # Re-exports: Server, CRMSlot (unchanged public API)
├── core.py           # Server class: lifecycle, CRM registration, frame loop (~550 lines)
├── connection.py     # _Connection, CRMSlot, _parse_call_inline() (~120 lines)
├── dispatcher.py     # _FastDispatcher worker pool (~130 lines)
├── chunk.py          # _ChunkAssembler + constants + GC helper (~110 lines)
├── handshake.py      # Handshake protocol: handle_v5, open_segments (~120 lines)
├── reply.py          # Reply encoding: send_reply, send_chunked, unpack, wrap_error (~170 lines)
└── scheduler.py      # (existing, untouched)
```

## Module Specifications

### 1. `connection.py` (~120 lines)

**Extracted from:** Lines 93–205 of core.py

**Contains:**
- `_parse_call_inline(payload) → (route_key, method_idx, args_start)` — pure parsing function
- `_Connection` dataclass — per-client state (writer, buddy_pool, seg_views, flight tracking)
- `CRMSlot` dataclass — per-CRM registration (name, icrm, method_table, scheduler, dispatch_table)

**Dependencies:** `asyncio`, `threading`, `dataclasses`, `wire.MethodTable`, `wire.U16_LE`, `scheduler.Scheduler`, `crm.meta.MethodAccess`

**Why extract:** These are data containers used by every other module. Putting them in their own file eliminates the need for other modules to import from core.py, breaking the hub pattern.

**Import stability:** `CRMSlot` is a public export — `__init__.py` will import from `connection.py` instead of `core.py`. All external imports via `transport.Server` / `transport.server.CRMSlot` remain unchanged.

### 2. `dispatcher.py` (~130 lines)

**Extracted from:** Lines 279–401 of core.py

**Contains:**
- `_SENTINEL` constant
- `_FastDispatcher` class — SimpleQueue-based worker pool with inline reply writing

**Dependencies:** `asyncio`, `queue`, `threading`, `reply.build_inline_reply_frame`, `reply.build_error_reply_frame`

**Key refactoring:** Currently `_FastDispatcher._write_inline_reply` duplicates ICRM result unpacking logic that also exists in `Server._unpack_icrm_result` and inline in `Server._handle_call`. After extraction:
- Result unpacking moves to `reply.unpack_icrm_result()`
- Frame building stays as `reply.build_inline_reply_frame()` / `reply.build_error_reply_frame()`
- Dispatcher imports from `reply` module, eliminating its direct dependency on `wire`

**Thread safety:** `_FastDispatcher` workers call `loop.call_soon_threadsafe()` — this is the only cross-thread boundary. No changes to the threading model.

### 3. `chunk.py` (~110 lines)

**Extracted from:** Lines 85–90 (constants) + 211–277 (_ChunkAssembler) + 1157–1175 (GC) of core.py

**Contains:**
- Chunk constants: `_CHUNK_THRESHOLD_RATIO`, `_CHUNK_ASSEMBLER_TIMEOUT`, `_CHUNK_GC_INTERVAL`, `_MAX_TOTAL_CHUNKS`, `_MAX_REASSEMBLY_BYTES`
- `_ChunkAssembler` dataclass — mmap-based reassembly buffer
- `gc_chunk_assemblers(assemblers, conn_id)` — stale assembler GC (promoted from static method to module function)

**Dependencies:** `mmap`, `time`, `logging`, `dataclasses`

**Why extract:** Fully self-contained — zero coupling to Server state. The only caller is `_process_chunked_frame` and the periodic GC in the frame loop. Extraction also allows independent unit testing (currently tested via `test_chunk_assembler.py` which imports `_ChunkAssembler` from `server.core`).

**Test impact:** `test_chunk_assembler.py` import changes: `from c_two.transport.server.core import _ChunkAssembler` → `from c_two.transport.server.chunk import _ChunkAssembler`

### 4. `handshake.py` (~120 lines)

**Extracted from:** Lines 930–1031 of core.py

**Contains:**
- `handle_handshake(conn, payload, writer, slots_snapshot_fn)` — version dispatch
- `handle_v5_handshake(conn, payload, writer, slots_snapshot_fn)` — v5 handshake protocol
- `open_segments(conn, segments, config)` — buddy SHM segment opening
- `_MAX_CLIENT_SEGMENTS = 16` constant
- `_SHM_NAME_RE` regex constant

**Dependencies:** `connection._Connection`, `protocol.{HANDSHAKE_V5, CAP_*, RouteInfo, MethodEntry, ...}`, `ipc.frame.{IPCConfig, encode_frame}`, `buddy.{BuddyPoolHandle, PoolConfig}` (lazy import)

**Interface:** These become standalone async functions. The handshake needs CRM route info — instead of passing the Server instance, we pass a `slots_snapshot: list[CRMSlot]` computed by the caller (`_handle_client` acquires `_slots_lock`, snapshots, then passes the list). This keeps handshake protocol logic pure and avoids a direct dependency on Server internals.

**Why extract:** Handshake is a discrete protocol concern. It runs once per connection, has no interaction with the dispatch hot path, and its complexity (segment validation, capability negotiation, route info building) deserves isolation.

### 5. `reply.py` (~170 lines)

**Extracted from:** Lines 1346–1497 of core.py + static methods from _FastDispatcher

**Contains:**
- `unpack_icrm_result(result) → (result_bytes, error_bytes)` — ICRM '<-' result tuple unpacking
- `wrap_error(exc) → (result_bytes, error_bytes)` — exception serialization
- `build_inline_reply_frame(request_id, result) → bytes` — inline reply frame from raw ICRM result
- `build_error_reply_frame(request_id, exc) → bytes` — error reply frame from exception
- `send_reply(conn, request_id, result_bytes, err_bytes, writer, config) → None` — async, chooses inline/buddy/chunked
- `send_chunked_reply(conn, request_id, result_bytes, writer, config) → None` — async, multi-frame reply

**Dependencies:** `connection._Connection`, `wire.{encode_buddy_reply_frame, encode_inline_reply_frame, encode_error_reply_frame, encode_*_chunked_reply_frame}`, `ipc.buddy.{...}`, `error`

**Key benefit:** Eliminates duplication. Currently, ICRM result unpacking is duplicated in:
1. `Server._unpack_icrm_result()` (lines 1346–1361)
2. `Server._handle_call()` inline (lines 1264–1276)
3. `_FastDispatcher._write_inline_reply()` (lines 370–382)

After extraction, all three paths use `reply.unpack_icrm_result()`.

### 6. `core.py` after extraction (~550 lines)

**Remains:**
- `Server.__init__` — config, slots, lifecycle state
- `Server.register_crm/unregister_crm/get_slot_info/names` — CRM registration
- `Server._create_icrm/_discover_methods/_extract_namespace/_resolve_slot` — ICRM helpers
- `Server.start/shutdown/_run_loop/_async_main` — lifecycle
- `Server._invoke_shutdown` — CRM shutdown callback
- `Server._handle_client` — frame loop (simplified: delegates to handshake/chunk/reply modules)
- `Server._handle_call` — call dispatch (simplified: uses reply.unpack_icrm_result)
- `Server._process_chunked_frame` — chunk routing (uses chunk module)
- `Server._dispatch` — CRM execution via scheduler

**The frame loop `_handle_client` becomes cleaner** because:
1. Handshake → `handshake.handle_handshake(conn, payload, writer, self._slots_snapshot)`
2. Chunked frames → `chunk` module for assembler management
3. Reply → `reply.send_reply(conn, ...)`
4. GC → `chunk.gc_chunk_assemblers(assemblers, conn.conn_id)`

The hot path (fast inline dispatch) stays in `_handle_client` — this is intentional. Moving it out would add function call overhead on every frame.

## Dependency Graph (Post-Extraction)

```
                ┌─────────────┐
                │ __init__.py │  Re-exports Server, CRMSlot
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │   core.py   │  Server class (orchestrator)
                └──┬──┬──┬──┬─┘
                   │  │  │  │
      ┌────────────┘  │  │  └────────────┐
      ▼               ▼  ▼               ▼
┌───────────┐  ┌─────────────┐  ┌──────────────┐
│handshake.py│  │dispatcher.py│  │   chunk.py   │
└─────┬─────┘  └──────┬──────┘  └──────────────┘
      │               │                  (stdlib only)
      │               ▼
      │        ┌────────────┐
      │        │  reply.py  │
      │        └────────────┘
                      │
               ┌──────▼──────┐
               │connection.py│  _Connection, CRMSlot
               └─────────────┘
                      │
               ┌──────▼──────┐
               │scheduler.py │  (existing, untouched)
               └─────────────┘
```

**No circular dependencies.** Each module depends only on modules below it in the graph.

## Import Changes

### Internal (src/)

| File | Before | After |
|------|--------|-------|
| `server/__init__.py` | `from .core import Server, CRMSlot` | `from .core import Server` + `from .connection import CRMSlot` |
| `registry.py` | `from .server.core import Server` | No change (Server stays in core.py) |

### Tests

| File | Before | After |
|------|--------|-------|
| `test_chunk_assembler.py` | `from c_two.transport.server.core import _ChunkAssembler` | `from c_two.transport.server.chunk import _ChunkAssembler` |

All other test imports (`Server`, `CRMSlot`, `Scheduler`, `ConcurrencyConfig`) remain unchanged — they go through `__init__.py` or import `Server` from `server.core` (which stays).

## Constraints

1. **Zero public API changes** — `Server`, `CRMSlot`, `Scheduler`, `ConcurrencyConfig` remain importable from the same paths.
2. **Hot path stays in core.py** — the fast inline dispatch path in `_handle_client` is not extracted. This is the performance-critical loop; function call overhead matters.
3. **No new abstractions** — extracted modules use plain functions, not classes or protocols. The Server passes explicit parameters or callbacks, not self-references.
4. **Free-threading safe** — all thread-safety invariants (slots_lock, flight tracking, call_soon_threadsafe) are preserved. Module boundaries make the threading model clearer, not weaker.
5. **Existing test coverage preserved** — all 675 tests must continue to pass after refactoring.

## Phases

1. **Extract `connection.py`** — Move `_Connection`, `CRMSlot`, `_parse_call_inline`. Update imports.
2. **Extract `chunk.py`** — Move `_ChunkAssembler`, constants, GC helper. Update imports.
3. **Extract `reply.py`** — Move result unpacking, error wrapping, reply sending. Eliminate duplication.
4. **Extract `dispatcher.py`** — Move `_FastDispatcher`. Wire it to `reply` module.
5. **Extract `handshake.py`** — Move handshake handlers, segment opening.
6. **Simplify `core.py`** — Update Server methods to delegate to extracted modules.
7. **Update `__init__.py` + test imports** — Fix any remaining import paths.
8. **Verify** — Run full test suite, check no regressions.
