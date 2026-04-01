# Phase 4: Python Transport Cleanup — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Delete ~2500 lines of superseded Python transport code, redirect all imports to surviving modules, and ensure 606+ tests pass with no regressions.

**Architecture:** The Rust transport layer (Phases 0-3) now handles IPC server, IPC client, HTTP client, wire codec, and memory pool. Python retains: NativeServerBridge (Rust bridge), scheduler (concurrency logic), registry (SOTA API), ICRMProxy, IPCConfig (config dataclass), MethodTable (method indexing), and `unpack_icrm_result` / `wrap_error` (result helpers). Everything else is dead code.

**Tech Stack:** Python 3.10+, pytest, uv

**Branch:** Create `phase4/cleanup` from `dev-feature`

---

## File Map

### Files to DELETE entirely (~2200 lines):
| File | Lines | Reason |
|------|-------|--------|
| `src/c_two/transport/server/core.py` | 954 | Old Python asyncio server — replaced by NativeServerBridge |
| `src/c_two/transport/server/connection.py` | 145 | CRMSlot/Connection for old server — native.py has its own CRMSlot |
| `src/c_two/transport/server/handshake.py` | 172 | Only used by old server core.py |
| `src/c_two/transport/server/dispatcher.py` | 102 | Only used by old server core.py |
| `src/c_two/transport/server/heartbeat.py` | 68 | Only used by old server core.py — Rust handles heartbeat |
| `src/c_two/transport/ipc/envelope.py` | 34 | Legacy event system — unused |
| `src/c_two/transport/ipc/msg_type.py` | 39 | Python MsgType enum — Rust has equivalent |
| `src/c_two/transport/ipc/shm_frame.py` | 192 | SHM frame codec — Rust handles this |
| `tests/unit/test_heartbeat.py` | 177 | Tests Python heartbeat — Rust has heartbeat.rs + tests |
| `tests/unit/test_ipc.py` | ~120 | Tests Python frame/shm_frame encoding — Rust handles this |

### Files to SLIM (keep business logic, delete transport code):
| File | Action |
|------|--------|
| `src/c_two/transport/server/reply.py` | Keep `unpack_icrm_result` + `wrap_error`, delete frame builders/senders |
| `src/c_two/transport/wire.py` | Keep `MethodTable` + `payload_total_size`, delete all encode/decode functions |
| `src/c_two/transport/ipc/frame.py` | Keep `IPCConfig` dataclass only, delete frame encoding/constants |

### Files to MODIFY (import redirects):
| File | Change |
|------|--------|
| `src/c_two/transport/server/__init__.py` | Export `NativeServerBridge as Server`, drop old imports |
| `src/c_two/transport/__init__.py` | Redirect `Server` → `.server.native`, `CRMSlot` → `.server.native` |
| `src/c_two/transport/ipc/__init__.py` | Remove dead exports (MsgType, Envelope) |
| `src/c_two/transport/server/native.py` | Add `icrm_class`/`crm_instance`/`name` constructor kwargs for compat |
| `tests/conftest.py` | Update Server import path |
| 8 other test files | Update Server/IPCConfig/wire import paths |

### Files that SURVIVE unchanged:
- `src/c_two/transport/server/scheduler.py` (264L) — business logic
- `src/c_two/transport/server/native.py` (315L) — Rust bridge (modified for compat)
- `src/c_two/transport/registry.py` — SOTA API
- `src/c_two/transport/client/proxy.py` — ICRMProxy
- `src/c_two/transport/client/http.py` — HTTP transport
- `src/c_two/transport/protocol.py` (128L) — kept for now (test_security uses it)
- `src/c_two/transport/config.py` — address scheme config

---

### Task 1: Create branch and add NativeServerBridge compat kwargs

**Files:**
- Modify: `src/c_two/transport/server/native.py:72-95`

- [ ] **Step 1: Create the cleanup branch**

```bash
git checkout dev-feature
git checkout -b phase4/cleanup
```

- [ ] **Step 2: Add `icrm_class`, `crm_instance`, `name` kwargs to `NativeServerBridge.__init__`**

The old `Server` constructor accepted `icrm_class=..., crm_instance=..., name=...` directly. Tests (especially `conftest.py`) rely on this signature. Add these kwargs to `NativeServerBridge.__init__` so it's a drop-in replacement.

In `src/c_two/transport/server/native.py`, modify the `__init__` method:

```python
def __init__(
    self,
    bind_address: str,
    icrm_class: type | None = None,
    crm_instance: object | None = None,
    ipc_config: IPCConfig | None = None,
    concurrency: ConcurrencyConfig | None = None,
    max_workers: int = 4,
    *,
    name: str | None = None,
) -> None:
    self._config = ipc_config or IPCConfig()
    self._address = bind_address
    self._default_concurrency = concurrency or ConcurrencyConfig(
        max_workers=max_workers,
    )

    self._slots: dict[str, CRMSlot] = {}
    self._slots_lock = threading.Lock()
    self._default_name: str | None = None
    self._started = False

    from c_two._native import RustServer

    chunked_threshold = int(
        self._config.max_payload_size * self._config.chunk_threshold_ratio,
    )
    self._rust_server = RustServer(
        bind_address=bind_address,
        segment_size=self._config.pool_segment_size,
        max_segments=self._config.max_pool_segments,
        chunked_threshold=chunked_threshold,
        heartbeat_interval_secs=self._config.heartbeat_interval,
        heartbeat_timeout_secs=self._config.heartbeat_timeout,
    )
    self._dispatch_executor = ThreadPoolExecutor(
        max_workers=max_workers, thread_name_prefix='crm-dispatch',
    )

    # Register initial CRM if provided (compat with old Server constructor).
    if icrm_class is not None and crm_instance is not None:
        self.register_crm(
            icrm_class, crm_instance, concurrency, name=name,
        )
```

- [ ] **Step 3: Run tests to verify compat**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/conftest.py tests/integration/test_server.py -q --timeout=30
```
Expected: All tests pass — NativeServerBridge now accepts the same constructor kwargs as old Server.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/server/native.py
git commit -m "feat(native): add icrm_class/crm_instance compat kwargs to NativeServerBridge"
```

---

### Task 2: Redirect Server export to NativeServerBridge

**Files:**
- Modify: `src/c_two/transport/server/__init__.py`
- Modify: `src/c_two/transport/__init__.py`

- [ ] **Step 1: Update `server/__init__.py`**

Replace the entire file with:

```python
from .native import NativeServerBridge as Server, CRMSlot
from .scheduler import Scheduler, ConcurrencyConfig, ConcurrencyMode
```

This drops the old `core.py` import entirely. `Server` is now an alias for `NativeServerBridge`. `CRMSlot` comes from native.py (which has its own independent copy).

- [ ] **Step 2: Update `transport/__init__.py` lazy imports**

Change the lazy import entries for `Server` and `CRMSlot`:

```python
'Server':          ('.server.native',       'NativeServerBridge'),
'CRMSlot':           ('.server.native',       'CRMSlot'),
```

(Previously they pointed to `.server.core`.)

- [ ] **Step 3: Run tests to verify redirect works**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: All 606+ tests pass — Server now resolves to NativeServerBridge everywhere.

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/server/__init__.py src/c_two/transport/__init__.py
git commit -m "refactor: redirect Server export to NativeServerBridge"
```

---

### Task 3: Update all test imports of Server

Tests import `Server` from three paths:
1. `from c_two.transport.server.core import Server` (6 files)
2. `from c_two.transport import Server` (3 files — these already work via lazy import)
3. `from c_two.transport.server import Server` (0 files currently)

Path (2) already works after Task 2. Path (1) must be changed.

**Files:**
- Modify: `tests/conftest.py:11`
- Modify: `tests/unit/test_ipc.py:25`
- Modify: `tests/unit/test_shared_client.py:12`
- Modify: `tests/integration/test_error_propagation.py:13`
- Modify: `tests/integration/test_concurrency_safety.py:21`
- Modify: `tests/integration/test_dynamic_pool.py:25`

- [ ] **Step 1: Replace all `from c_two.transport.server.core import Server`**

In each file above, change:
```python
from c_two.transport.server.core import Server
```
to:
```python
from c_two.transport.server import Server
```

This resolves to `NativeServerBridge` via the updated `server/__init__.py`.

- [ ] **Step 2: Run full test suite**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All 606+ tests pass with no import errors.

- [ ] **Step 3: Commit**

```bash
git add tests/conftest.py tests/unit/test_ipc.py tests/unit/test_shared_client.py \
  tests/integration/test_error_propagation.py tests/integration/test_concurrency_safety.py \
  tests/integration/test_dynamic_pool.py
git commit -m "refactor(tests): redirect Server imports from core.py to server package"
```

---

### Task 4: Slim reply.py — keep only `unpack_icrm_result` and `wrap_error`

**Files:**
- Modify: `src/c_two/transport/server/reply.py`

`native.py` only imports `unpack_icrm_result`. The frame-building and sending functions (`build_inline_reply`, `build_error_reply`, `send_reply`, `send_chunked_reply`) were used by the old Python server and can be deleted. Keep `wrap_error` as it's general-purpose error serialization.

- [ ] **Step 1: Replace reply.py with minimal version**

```python
"""ICRM result unpacking helpers.

Used by NativeServerBridge to convert raw ICRM return values
into (result_bytes, error_bytes) tuples.
"""
from __future__ import annotations

from typing import Any

from ... import error


def unpack_icrm_result(result: Any) -> tuple[bytes, bytes]:
    """Unpack ICRM ``'<-'`` result into ``(result_bytes, error_bytes)``."""
    if isinstance(result, tuple):
        err_part = result[0] if result[0] else b''
        res_part = result[1] if len(result) > 1 and result[1] else b''
        if isinstance(err_part, memoryview):
            err_part = bytes(err_part)
        if isinstance(res_part, memoryview):
            res_part = bytes(res_part)
        return res_part, err_part
    if result is None:
        return b'', b''
    if isinstance(result, memoryview):
        return bytes(result), b''
    return result, b''


def wrap_error(exc: Exception) -> tuple[bytes, bytes]:
    """Serialize an exception into ``(b'', error_bytes)``."""
    if isinstance(exc, error.CCBaseError):
        try:
            return b'', exc.serialize()
        except Exception:
            pass
    try:
        return b'', error.CRMExecuteFunction(str(exc)).serialize()
    except Exception:
        return b'', str(exc).encode('utf-8')
```

- [ ] **Step 2: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: All tests pass — only old server code used the deleted functions.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/transport/server/reply.py
git commit -m "refactor(reply): slim to unpack_icrm_result + wrap_error only"
```

---

### Task 5: Delete old server transport files

These files are only used by the old Python `Server` in `core.py`. After Tasks 2-4 redirected all imports, they are dead code.

**Files to delete:**
- `src/c_two/transport/server/core.py` (954 lines)
- `src/c_two/transport/server/connection.py` (145 lines)
- `src/c_two/transport/server/handshake.py` (172 lines)
- `src/c_two/transport/server/dispatcher.py` (102 lines)
- `src/c_two/transport/server/heartbeat.py` (68 lines)

- [ ] **Step 1: Delete the files**

```bash
rm src/c_two/transport/server/core.py \
   src/c_two/transport/server/connection.py \
   src/c_two/transport/server/handshake.py \
   src/c_two/transport/server/dispatcher.py \
   src/c_two/transport/server/heartbeat.py
```

- [ ] **Step 2: Run tests to verify no remaining imports**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: All tests pass. If any test fails with ImportError, it means we missed an import redirect in Task 3 — fix it before proceeding.

- [ ] **Step 3: Commit**

```bash
git add -A src/c_two/transport/server/
git commit -m "delete: remove 5 old Python server transport files (~1440 lines)"
```

---

### Task 6: Slim wire.py — keep MethodTable + payload_total_size only

`wire.py` (438 lines) contains the full Python wire codec. After the Rust transport sink, only two symbols are still used by surviving code:
- `MethodTable` class (lines 398-437) — used by native.py, test_security.py
- `payload_total_size` function (line 121) — used by test_transferable.py

Everything else (encode/decode functions, frame constants, struct objects) is dead code.

**Files:**
- Modify: `src/c_two/transport/wire.py`

- [ ] **Step 1: Replace wire.py with minimal version**

```python
"""Wire utilities — method indexing and payload size helpers.

The full wire codec has been moved to Rust (c2-wire crate).
This module retains only the Python-side business logic:
  - MethodTable: bidirectional method-name ↔ index mapping
  - payload_total_size: compute byte length of various payload types
"""
from __future__ import annotations

from typing import Any


def payload_total_size(payload: Any) -> int:
    """Return the total byte length of *payload*.

    Handles None, bytes, memoryview, and iterables of bytes/memoryview.
    """
    if payload is None:
        return 0
    if isinstance(payload, (bytes, memoryview)):
        return len(payload)
    return sum(len(p) for p in payload)


class MethodTable:
    """Bidirectional method-name ↔ uint16 index mapping.

    Used by the server to convert between method names and compact
    wire indices during RPC dispatch.
    """

    __slots__ = ('_name_to_idx', '_idx_to_name')

    def __init__(self) -> None:
        self._name_to_idx: dict[str, int] = {}
        self._idx_to_name: dict[int, str] = {}

    @classmethod
    def from_methods(cls, methods: list[str]) -> 'MethodTable':
        t = cls()
        for idx, name in enumerate(methods):
            t._name_to_idx[name] = idx
            t._idx_to_name[idx] = name
        return t

    def index_of(self, name: str) -> int | None:
        return self._name_to_idx.get(name)

    def name_of(self, idx: int) -> str | None:
        return self._idx_to_name.get(idx)

    def method_names(self) -> list[str]:
        return list(self._name_to_idx.keys())

    def __len__(self) -> int:
        return len(self._name_to_idx)
```

- [ ] **Step 2: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: All tests pass. `test_security.py::TestMethodTableSafety` and `test_transferable.py::TestPayloadTotalSize` should still work.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/transport/wire.py
git commit -m "refactor(wire): slim to MethodTable + payload_total_size (~370 lines removed)"
```

---

### Task 7: Slim frame.py — keep IPCConfig only

`frame.py` (310 lines) contains IPCConfig (config dataclass, ~50 lines) plus the entire frame encoding/decoding codec and constants. After Rust sink, only `IPCConfig` is still imported by surviving code (native.py, registry.py, tests).

**Files:**
- Modify: `src/c_two/transport/ipc/frame.py`

- [ ] **Step 1: Replace frame.py with minimal version**

```python
"""IPC configuration dataclass.

The frame encoding/decoding codec has been moved to Rust (c2-wire + c2-ipc).
This module retains only ``IPCConfig`` — the Python-side configuration
dataclass consumed by ``NativeServerBridge`` and ``registry.py``.
"""
from __future__ import annotations

from dataclasses import dataclass

# Default configuration values
DEFAULT_SHM_THRESHOLD = 4_096                       # 4 KB
DEFAULT_MAX_FRAME_SIZE = 2_147_483_648              # 2 GB
DEFAULT_MAX_PAYLOAD_SIZE = 17_179_869_184            # 16 GB
DEFAULT_MAX_PENDING_REQUESTS = 1024
DEFAULT_POOL_SEGMENT_SIZE = 268_435_456              # 256 MB
DEFAULT_MAX_POOL_SEGMENTS = 4
DEFAULT_MAX_POOL_MEMORY = DEFAULT_POOL_SEGMENT_SIZE * DEFAULT_MAX_POOL_SEGMENTS


@dataclass
class IPCConfig:
    """IPC transport configuration.

    Transport limits (affect correctness and compatibility):
        max_frame_size: Maximum inline frame size in bytes (default 2 GB).
        max_payload_size: Maximum SHM payload size (default 16 GB).
        max_pending_requests: Server-side concurrent request limit (default 1024).

    Performance tuning:
        shm_threshold: Payload size cutover from inline to SHM (default 4 KB).
        pool_segment_size: Size of each pool SHM segment (default 256 MB).
        max_pool_memory: Memory budget per pool direction (default 1 GB).

    Heartbeat (server probes client liveness):
        heartbeat_interval: Seconds between PING probes (default 15; ≤0 disables).
        heartbeat_timeout: Seconds with no activity before declaring dead (default 30).
    """
    # Transport limits
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS

    # Pool SHM settings
    pool_enabled: bool = True
    pool_decay_seconds: float = 60.0
    max_pool_memory: int = DEFAULT_MAX_POOL_MEMORY
    max_pool_segments: int = DEFAULT_MAX_POOL_SEGMENTS
    pool_segment_size: int = DEFAULT_POOL_SEGMENT_SIZE

    # Chunked transfer settings
    max_total_chunks: int = 512
    chunk_gc_interval: int = 100
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0
    max_reassembly_bytes: int = 8 * (1 << 30)  # 8 GB

    # Heartbeat settings
    heartbeat_interval: float = 15.0
    heartbeat_timeout: float = 30.0

    def __post_init__(self) -> None:
        if self.pool_segment_size > 0xFFFFFFFF:
            raise ValueError(
                f'pool_segment_size {self.pool_segment_size} exceeds uint32 max '
                f'(handshake wire format is uint32)'
            )
        if self.pool_segment_size <= 0:
            raise ValueError('pool_segment_size must be positive')
```

- [ ] **Step 2: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: Some tests that import frame constants (`FRAME_STRUCT`, `FLAG_RESPONSE`, `encode_frame`) may fail. These will be handled in Task 9.

- [ ] **Step 3: Commit**

```bash
git add src/c_two/transport/ipc/frame.py
git commit -m "refactor(frame): slim to IPCConfig dataclass only (~230 lines removed)"
```

---

### Task 8: Delete IPC helper files and clean ipc/__init__.py

**Files to delete:**
- `src/c_two/transport/ipc/envelope.py` (34 lines)
- `src/c_two/transport/ipc/msg_type.py` (39 lines)
- `src/c_two/transport/ipc/shm_frame.py` (192 lines)

**Files to modify:**
- `src/c_two/transport/ipc/__init__.py`

- [ ] **Step 1: Delete the files**

```bash
rm src/c_two/transport/ipc/envelope.py \
   src/c_two/transport/ipc/msg_type.py \
   src/c_two/transport/ipc/shm_frame.py
```

- [ ] **Step 2: Update ipc/__init__.py**

Replace with:

```python
from .frame import IPCConfig
```

- [ ] **Step 3: Run tests**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -x
```
Expected: Tests that imported MsgType, PING_BYTES, shm_frame, etc. will fail — these are handled in Task 9.

- [ ] **Step 4: Commit**

```bash
git add -A src/c_two/transport/ipc/
git commit -m "delete: remove 3 IPC helper files (~265 lines), clean ipc/__init__"
```

---

### Task 9: Fix or delete broken tests

After Tasks 5-8, some tests will have import errors because they tested deleted Python transport code. Strategy:
- Tests that ONLY test deleted Python code → delete entirely
- Tests that test surviving logic but import deleted symbols → fix imports
- Tests that test integration behavior (not Python transport internals) → keep and fix

**Analysis of affected test files:**

| File | Verdict | Reason |
|------|---------|--------|
| `tests/unit/test_heartbeat.py` (177L) | DELETE | Tests Python heartbeat — Rust has heartbeat.rs + tests |
| `tests/unit/test_ipc.py` (~120L) | DELETE or SLIM | Tests Python frame/shm_frame encoding — Rust handles this. Keep only IPCConfig validation tests if any. |
| `tests/unit/test_wire.py` (631L) | SLIM | Keep MethodTable tests, payload_total_size tests. Delete all encode/decode tests. |
| `tests/unit/test_security.py` (313L) | FIX IMPORTS | TestMethodTableSafety (lines 257-280) survives. Wire-manipulation tests that import deleted symbols need fixing or deletion. |
| `tests/integration/test_heartbeat.py` (~30L) | KEEP | Tests Rust heartbeat via NativeServerBridge — should still work |
| `tests/integration/test_concurrency_safety.py` | FIX | Lines 288, 332, 373 import `encode_frame` from deleted frame.py — these test raw frame injection which is no longer relevant. Delete those specific test methods. |

**Files:**
- Delete: `tests/unit/test_heartbeat.py`
- Delete or slim: `tests/unit/test_ipc.py`
- Modify: `tests/unit/test_wire.py`
- Modify: `tests/unit/test_security.py`
- Modify: `tests/integration/test_concurrency_safety.py`

- [ ] **Step 1: Delete test_heartbeat.py (unit)**

```bash
rm tests/unit/test_heartbeat.py
```

- [ ] **Step 2: Slim test_ipc.py**

Check what survives after removing shm_frame and frame encoding imports. If the file only tests frame encoding, delete it entirely:

```bash
rm tests/unit/test_ipc.py
```

If it has IPCConfig validation tests, keep only those.

- [ ] **Step 3: Slim test_wire.py**

Keep only the tests that exercise surviving code:
- `TestMethodTableSafety` (or similar) — tests MethodTable
- `TestPayloadTotalSize` — tests payload_total_size

Delete all test classes that test `encode_call`, `decode_call`, `encode_reply`, `encode_buddy_reply_frame`, etc. Remove the now-broken imports at the top of the file.

The surviving test file should look approximately like:

```python
"""Unit tests for wire utilities (MethodTable, payload_total_size)."""
from __future__ import annotations

import pytest

from c_two.transport.wire import MethodTable, payload_total_size


class TestMethodTable:
    def test_from_methods(self):
        t = MethodTable.from_methods(['a', 'b', 'c'])
        assert t.index_of('a') == 0
        assert t.index_of('b') == 1
        assert t.name_of(0) == 'a'
        assert len(t) == 3

    def test_empty(self):
        t = MethodTable()
        assert len(t) == 0
        assert t.index_of('x') is None
        assert t.name_of(0) is None

    def test_method_names(self):
        t = MethodTable.from_methods(['x', 'y'])
        assert t.method_names() == ['x', 'y']


class TestPayloadTotalSize:
    def test_none(self):
        assert payload_total_size(None) == 0

    def test_bytes(self):
        assert payload_total_size(b'hello') == 5

    def test_memoryview(self):
        assert payload_total_size(memoryview(b'hello')) == 5

    def test_tuple(self):
        assert payload_total_size((b'hel', b'lo')) == 5

    def test_list(self):
        assert payload_total_size([b'a', b'bc', b'def']) == 6
```

- [ ] **Step 4: Fix test_security.py imports**

Keep `TestMethodTableSafety` class (lines 257-280). It imports `MethodTable` from `c_two.transport.wire` — this still works.

Remove any test classes that import deleted symbols (e.g., `encode_frame`, `FLAG_RESPONSE`, protocol constants used for frame manipulation). Examine the file and remove only broken tests — keep everything that still imports correctly.

Check which imports from protocol.py are still used. If the test imports from `c_two.transport.protocol`, that file still exists — no change needed for those.

- [ ] **Step 5: Fix test_concurrency_safety.py**

Lines 288, 332, 373 contain `from c_two.transport.ipc.frame import encode_frame` inside test methods. These test raw malformed frame injection into the old Python server. Since we now use the Rust server, these specific test methods should be deleted:
- The method containing line 288
- The method containing line 332
- The method containing line 373

Keep all other test methods in the class that test concurrency behavior through the normal API.

- [ ] **Step 6: Run full test suite**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All surviving tests pass. Total count will be lower (we deleted ~200 lines of tests).

- [ ] **Step 7: Commit**

```bash
git add -A tests/
git commit -m "refactor(tests): delete/slim tests for removed Python transport code"
```

---

### Task 10: Final cleanup and verification

**Files:**
- Modify: `src/c_two/transport/__init__.py` (remove stale __all__ entries if needed)
- Verify: all imports resolve, no dangling references

- [ ] **Step 1: Verify no dangling imports to deleted modules**

```bash
cd /Users/soku/Desktop/codespace/WorldInProgress/c-two
grep -rn 'from.*server\.core import\|from.*server\.connection import\|from.*server\.handshake import\|from.*server\.dispatcher import\|from.*server\.heartbeat import' src/ tests/ --include='*.py' | grep -v __pycache__
```
Expected: Zero results.

```bash
grep -rn 'from.*ipc\.envelope import\|from.*ipc\.msg_type import\|from.*ipc\.shm_frame import' src/ tests/ --include='*.py' | grep -v __pycache__
```
Expected: Zero results.

- [ ] **Step 2: Verify wire.py old symbols not imported**

```bash
grep -rn 'encode_call\|decode_call\|encode_reply\|encode_buddy_reply\|encode_inline_reply\|encode_error_reply\|encode_buddy_chunked\|encode_inline_chunked' src/ tests/ --include='*.py' | grep -v __pycache__ | grep -v wire.py
```
Expected: Zero results (wire.py itself doesn't have them anymore either).

- [ ] **Step 3: Update transport/__init__.py __all__ list**

Verify `__all__` only lists symbols that actually exist. Remove `CRMSlot` from `__all__` if it's no longer a public API (it's an internal server detail). Or keep it if tests import it.

- [ ] **Step 4: Run full test suite one final time**

```bash
C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30
```
Expected: All tests pass. Record the final count.

- [ ] **Step 5: Count lines deleted**

```bash
git diff --stat dev-feature..HEAD
```
Expected: Net deletion of ~2000+ lines.

- [ ] **Step 6: Commit any final fixes**

```bash
git add -A
git commit -m "chore: final Phase 4 cleanup — verify no dangling imports"
```

---

## Summary

| Task | Description | Lines affected |
|------|-------------|---------------|
| 1 | NativeServerBridge compat kwargs | +15 |
| 2 | Redirect Server export | ~10 changed |
| 3 | Update test imports | ~12 changed |
| 4 | Slim reply.py | ~-120 |
| 5 | Delete 5 server files | ~-1440 |
| 6 | Slim wire.py | ~-370 |
| 7 | Slim frame.py | ~-230 |
| 8 | Delete 3 IPC files | ~-265 |
| 9 | Fix/delete broken tests | ~-400 |
| 10 | Final verification | ~0 |
| **Total** | | **~-2800 lines** |

---

## Post-Phase 4: Remaining Work

### Phase 5: SharedClient Migration (Blocked → Ready)

**Scope**: Delete `client/core.py` (85L) + `client/util.py` (91L) by migrating all 11 test files + `conftest.py` from `SharedClient` API to `cc.connect()` / `cc.close()` SOTA API.

**Files requiring migration**:

| File | SharedClient Uses | Migration Strategy |
|------|-------------------|-------------------|
| `tests/conftest.py` | `SharedClient.ping` in fixtures | Replace with `from c_two.transport.client.util import ping` |
| `tests/integration/test_server.py` | 20+ uses | Largest file — migrate to `cc.connect(ICRM, name=..., address=addr)` |
| `tests/integration/test_concurrency_safety.py` | 4 test classes | Migrate to SOTA API |
| `tests/integration/test_dynamic_pool.py` | Multiple | Migrate to SOTA API |
| `tests/integration/test_heartbeat.py` | Multiple | Migrate to SOTA API |
| `tests/integration/test_multi_crm_server.py` | Multiple | Migrate to SOTA API |
| `tests/integration/test_icrm_proxy.py` | Multiple | Migrate to SOTA API |
| `tests/integration/test_error_propagation.py` | Multiple | Migrate to SOTA API |
| `tests/integration/test_registry.py` | Multiple | Migrate to SOTA API |
| `tests/unit/test_icrm_proxy.py` | Multiple | Migrate to SOTA API |
| `tests/unit/test_shared_client.py` | Entire file | Delete or convert to RustClient tests |
| `src/c_two/transport/registry.py:302` | Comment only | Already uses RustClientPool (comment fixed) |

**Recommended approach**:
1. New branch `phase5/remove-shared-client` from `phase4/cleanup`
2. Start with `conftest.py` (fixture changes affect all tests)
3. Then `test_server.py` (most complex, 20+ uses)
4. Then remaining integration tests
5. Finally delete `client/core.py`, `client/util.py`, update `client/__init__.py`

### Other Future Work

- **RustAsyncClient** (spec §3.4) — `cc.connect_async()` + async ICRMProxy
- **IpcConfig unification** — merge c2-server and c2-ipc configs into c2-wire
- **`_MAX_HANDSHAKE_*` constants** — move to Rust (c2-wire) to allow protocol.py deletion
