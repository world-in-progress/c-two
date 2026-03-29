# Wire v1 Removal — Implementation Plan

## Problem Statement

After the transport refactor (commits `1dbe72a`–`befaaef`), the codebase still carries **dual wire protocol paths**: wire v1 (`wire_v1.py`) and wire v2 (`wire.py`). This creates:

1. **Code duplication**: Server has parallel `_handle_v1_call()` / `_handle_v2_call()`, `_send_v1_reply()` / `_send_v2_reply()`, `_resolve_v1_buddy()` / `_resolve_v2_buddy()` — ~350 lines of v1-only server code.
2. **Signal coupling**: Even v2 clients send signals (PING/SHUTDOWN) as v1 wire payloads, processed through the v1 code path via `decode()`.
3. **Dead code**: `encode_call()`, `encode_reply()`, `encode_signal()`, `write_signal_into()` have zero callers.
4. **Wasted pre-computation**: `preregister_methods()` populates a v1 header cache that v2 never uses (v2 uses `MethodTable` with 2-byte indices instead).

## Proposed Approach

Remove wire v1 backward compatibility in 5 phases:

| Phase | Tasks | Commit | Key Changes |
|-------|-------|--------|-------------|
| 1 | 1–2 | ① | Lift signals + utility to protocol level |
| 2 | 3–5 | ② | Purge v1 from server (~350 lines) |
| 3 | 6–8 | ③ | Purge v1 from client, remove `try_v2` flag |
| 4 | 9–10 | ④ | Delete `wire_v1.py`, clean `crm/meta.py` |
| 5 | 11–12 | ⑤ | Update tests, final verification |

## Key Design Decisions

### Signal promotion strategy

Signals (PING, PONG, SHUTDOWN_CLIENT, SHUTDOWN_SERVER, SHUTDOWN_ACK) are currently 1-byte wire_v1 `MsgType` payloads inside inline frames. The v2 dispatch loop never sees them because they lack `FLAG_CALL_V2`.

**Decision**: Add a new frame flag `FLAG_SIGNAL = 1 << 11` to identify signal frames. The 1-byte payload remains the same (`MsgType` value), but detection moves from `wire_v1.decode()` to a frame-flag check in the dispatch loop. This is cleaner, faster (no decode call), and eliminates the last v2→v1 dependency.

### `payload_total_size()` relocation

This is a generic utility (handles `bytes | memoryview | tuple | list | None`) used by both v1 and v2 paths. Move it to `wire.py` alongside other wire utilities.

### `preregister_methods()` removal

This function populates `_call_header_cache` — a dict of `method_name → pre-encoded v1 wire header bytes`. The v2 path uses `MethodTable` (negotiated at handshake) instead. After removing v1, `preregister_methods()` and `get_call_header_cache()` serve no purpose. Remove the call in `crm/meta.py:149` and delete the functions.

### Client `try_v2` elimination

`SharedClient.__init__` has `try_v2: bool = False` which gates v5 handshake. With v1 removed, all clients are v2-only. Remove the flag, always perform v5 handshake, and remove the v4 fallback path.

---

## Phase 1 — Lift Signals & Utility to Protocol Level

### Task 1: Add `FLAG_SIGNAL` and move signal constants

**Files modified:**
- `src/c_two/transport/ipc/msg_type.py` — Add signal byte constants here (canonical home)
- `src/c_two/transport/protocol.py` — Add `FLAG_SIGNAL = 1 << 11`

**Changes:**

In `ipc/msg_type.py`, add after the `MsgType` enum:
```python
# Pre-encoded 1-byte signal payloads (used as inline frame body)
PING_BYTES = bytes([MsgType.PING])
PONG_BYTES = bytes([MsgType.PONG])
SHUTDOWN_CLIENT_BYTES = bytes([MsgType.SHUTDOWN_CLIENT])
SHUTDOWN_SERVER_BYTES = bytes([MsgType.SHUTDOWN_SERVER])
SHUTDOWN_ACK_BYTES = bytes([MsgType.SHUTDOWN_ACK])
SIGNAL_SIZE = 1
```

In `protocol.py`, add:
```python
FLAG_SIGNAL = 1 << 11  # Frame carries a 1-byte signal (PING, SHUTDOWN, etc.)
```

### Task 2: Move `payload_total_size()` to `wire.py`

**Files modified:**
- `src/c_two/transport/wire.py` — Add `payload_total_size()` function
- `src/c_two/transport/client/core.py` — Update import source
- `src/c_two/transport/wire_v1.py` — Add deprecation re-export shim (temporary)

The function (13 lines) is a pure utility with no v1 dependencies:
```python
def payload_total_size(payload) -> int:
    if payload is None:
        return 0
    if isinstance(payload, (bytes, memoryview)):
        return len(payload)
    return sum(len(seg) for seg in payload)
```

---

## Phase 2 — Purge v1 From Server (~350 lines deleted)

### Task 3: Refactor signal handling in dispatch loop

**File modified:** `src/c_two/transport/server/core.py`

In the main dispatch loop (around line 790–910), add signal detection **before** the v1/v2 call dispatch:

```python
# After reading frame header and payload:
if flags & FLAG_SIGNAL:
    # Direct signal handling — no wire decode needed
    if payload and payload[0] == MsgType.PING:
        writer.write(encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, PONG_BYTES))
    elif payload and payload[0] == MsgType.SHUTDOWN_CLIENT:
        if self._shutdown_event:
            self._shutdown_event.set()
        writer.write(encode_frame(request_id, FLAG_RESPONSE | FLAG_SIGNAL, SHUTDOWN_ACK_BYTES))
        return  # close connection
    continue
```

This replaces the signal handling currently buried inside `_handle_v1_call()` at lines 1211–1234.

### Task 4: Delete v1 server methods

**File modified:** `src/c_two/transport/server/core.py`

Delete these methods entirely:
- `_handle_v4_handshake()` (lines 950–965, ~16 lines)
- `_handle_v1_call()` (lines 1198–1245, ~48 lines)
- `_resolve_v1_buddy()` (lines 1247–1287, ~41 lines)
- `_check_signal()` (lines 1290–1298, ~9 lines)
- `_send_v1_reply()` (lines 1502–1548, ~47 lines)

Total: **~160 lines** of method bodies + related imports.

Modify `_handle_handshake()` (line 940–948) to reject non-v5 clients:
```python
async def _handle_handshake(self, conn, payload, writer):
    if payload and payload[0] == HANDSHAKE_V5:
        await self._handle_v5_handshake(conn, payload, writer)
    else:
        # Reject legacy v4 clients
        writer.close()
        await writer.wait_closed()
```

### Task 5: Remove v1 branch from dispatch loop

**File modified:** `src/c_two/transport/server/core.py`

In the fallback path (lines 889–903), remove the v1 branch:
```python
# BEFORE:
if is_v2_call:
    self._handle_v2_call(...)
else:
    self._handle_v1_call(...)

# AFTER:
if is_v2_call:
    self._handle_v2_call(...)
else:
    # Non-v2, non-signal frame — invalid, ignore
    continue
```

Remove wire_v1 imports from server: `decode`, `write_reply_into`, `reply_wire_size`.
Keep signal constant imports (will be sourced from `ipc/msg_type.py` after Task 1).

---

## Phase 3 — Purge v1 From Client

### Task 6: Remove `try_v2` flag from SharedClient

**File modified:** `src/c_two/transport/client/core.py`

- Remove `try_v2: bool = False` parameter from `__init__`
- Remove `self._try_v2` attribute
- Remove `self._v2_mode` attribute (always True now)
- All call-site checks `if self._v2_mode:` become unconditional

### Task 7: Delete v4 handshake fallback

**File modified:** `src/c_two/transport/client/core.py`

In `connect()` (around lines 325–336):
```python
# BEFORE:
if self._try_v2:
    v5_ok = self._try_v5_handshake(segments)
    if not v5_ok:
        self._sock = self._do_connect()
        self._do_v4_handshake(segments)
else:
    self._do_v4_handshake(segments)

# AFTER:
self._do_v5_handshake(segments)
```

Delete: `_do_v4_handshake()` method, `_try_v5_handshake()` wrapper (merge into direct call).

### Task 8: Delete v1 call/response paths in client

**File modified:** `src/c_two/transport/client/core.py`

- Delete the v1 branch of `call()` / `_send_request()` that uses `write_call_into()`, `call_wire_size()`, `get_call_header_cache()`
- Delete v1 response decoding via `wire_v1.decode()`
- Update `ping()` and `shutdown()` static methods to:
  - Send frames with `FLAG_SIGNAL` flag
  - Detect response by flag check, not `wire_v1.decode()`

```python
@staticmethod
def ping(server_address: str, timeout: float = 0.5) -> bool:
    frame = encode_frame(0, FLAG_SIGNAL, PING_BYTES)
    sock.sendall(frame)
    header = _recv_exact(sock, 16)
    total_len, _rid, flags = FRAME_STRUCT.unpack(header)
    payload = _recv_exact(sock, total_len - 12) if total_len > 12 else b''
    return bool(flags & FLAG_SIGNAL) and len(payload) == 1 and payload[0] == MsgType.PONG
```

Remove wire_v1 imports: `decode`, `write_call_into`, `call_wire_size`, `get_call_header_cache`, `CALL_HEADER_FIXED`.
Keep signal constants (sourced from `ipc/msg_type.py`).

---

## Phase 4 — Delete wire_v1.py

### Task 9: Remove `preregister_methods` from crm/meta.py

**File modified:** `src/c_two/crm/meta.py`

Delete line 5 (`from ..transport.wire_v1 import preregister_methods`) and line 149 (`preregister_methods(decorated_methods.keys())`).

The v2 path builds `MethodTable` at handshake time from the ICRM class's `__methods__` dict — no pre-registration needed.

### Task 10: Delete `wire_v1.py` and remove all imports

**Files modified:**
- Delete `src/c_two/transport/wire_v1.py` (322 lines)
- `src/c_two/transport/__init__.py` — Remove any wire_v1 re-exports
- Grep and fix any remaining `wire_v1` imports

---

## Phase 5 — Update Tests & Final Verification

### Task 11: Update test files

**Files modified:**
- `tests/unit/test_transferable.py` — Has wire_v1 tests (`test_payload_total_size_*`, `test_write_call_into_*`, `test_write_reply_into_*`). Delete v1-specific tests, update `payload_total_size` tests to import from `wire`.
- `tests/unit/test_wire_v2.py` — May reference wire_v1 for comparison; update.
- Any other test importing `wire_v1` — grep and fix.

### Task 12: Run full test suite + commit

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```

Expected: ~670+ tests pass. Commit all changes.

---

## Summary of Impact

| Metric | Before | After |
|--------|--------|-------|
| `wire_v1.py` | 322 lines | **Deleted** |
| Server v1 code | ~350 lines | **Deleted** |
| Client v1 code | ~150 lines | **Deleted** |
| `try_v2` flag | Exists (default False) | **Removed** (always v2) |
| Signal handling | Via `wire_v1.decode()` | Via `FLAG_SIGNAL` frame flag |
| `preregister_methods()` | Called at ICRM registration | **Removed** |
| Total lines removed | — | **~800+** |

## Risks & Mitigations

1. **Breaking external consumers**: Any code using `try_v2=False` (v4 handshake) will fail. Mitigation: this is an internal refactor; no external consumers exist.
2. **Signal backward compat**: Old clients sending signals without `FLAG_SIGNAL` will be unrecognized. Mitigation: no old clients exist after transport refactor.
3. **Pool `try_v2` propagation**: `ClientPool` passes `try_v2` to `SharedClient`. Must update pool too.
