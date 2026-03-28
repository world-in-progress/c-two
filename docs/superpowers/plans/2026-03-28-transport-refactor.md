# Transport Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Promote `rpc_v2/` as the sole RPC implementation under a new `transport/` package, remove all v1 legacy code from `rpc/`, and establish a clean three-tier architecture.

**Architecture:** Three-tier design: `compo/` (WHO initiates) → `transport/` (HOW transmitted) → `crm/` (WHAT is called). Dependencies flow downward only. Migration is phased with atomic commits — tests green at every step.

**Tech Stack:** Python ≥3.10 (targeting 3.14t free-threaded), pytest, maturin/PyO3 (Rust buddy allocator unchanged), pydantic, starlette+uvicorn (relay)

**Design Spec:** `docs/superpowers/specs/2026-03-28-rpc-v2-promotion-refactor-design.md`

---

## File Structure

### New files to create

```
src/c_two/transport/
├── __init__.py              # Module exports (SharedClient, ServerV2, etc.)
├── registry.py              # ← rpc_v2/registry.py
├── config.py                # ← rpc_v2/config.py
├── protocol.py              # ← rpc_v2/protocol.py
├── wire.py                  # ← rpc_v2/wire.py
├── wire_v1.py               # ← rpc/util/wire.py (v1 codec, preregister_methods)
│
├── server/
│   ├── __init__.py           # Re-exports: ServerV2, CRMSlot
│   ├── core.py               # ← rpc_v2/server.py
│   └── scheduler.py          # ← rpc_v2/scheduler.py
│
├── client/
│   ├── __init__.py           # Re-exports: SharedClient, ClientPool, ICRMProxy
│   ├── core.py               # ← rpc_v2/client.py
│   ├── pool.py               # ← rpc_v2/pool.py
│   ├── proxy.py              # ← rpc_v2/proxy.py
│   └── http.py               # ← rpc_v2/http_client.py
│
├── relay/
│   ├── __init__.py           # Re-exports: RelayV2, UpstreamPool
│   └── core.py               # ← rpc_v2/relay.py
│
└── ipc/
    ├── __init__.py            # Re-exports: IPCConfig, MsgType, frame/buddy symbols
    ├── frame.py               # ← rpc/ipc/ipc_protocol.py (minus AdaptiveBuffer)
    ├── buddy.py               # ← rpc/ipc/ipc_v3_protocol.py
    ├── msg_type.py            # ← rpc/event/msg_type.py
    └── envelope.py            # ← rpc/event/envelope.py
```

### Files to move (crm layer)

```
src/c_two/crm/transferable.py   # ← rpc/transferable.py (inline add_length_prefix)
```

### Files to delete (Phase 4)

```
src/c_two/rpc/                   # Entire directory (~40 files)
src/c_two/rpc_v2/                # Shim directory (after Phase 2)
examples/server.py, client.py, crm.py, icrm.py, example_addresses.py
examples/grid/                   # Entire Grid example
benchmarks/adaptive_buffer_benchmark.py
benchmarks/concurrency_benchmark.py
benchmarks/ipc_v2_vs_v3_benchmark.py
benchmarks/ipc_v3_detailed_bench.py
benchmarks/memory_benchmark.py
benchmarks/memory_vs_ipc_v3_bench.py
benchmarks/wire_preencoding_benchmark.py
tests/unit/test_ipc_security.py
tests/unit/test_op2_safety.py
tests/unit/test_adaptive_buffer.py
tests/unit/test_encoding.py
tests/unit/test_concurrency.py
tests/integration/test_rpc_v2_basic.py
```

### Import mapping reference

| Old import | New import |
|------------|-----------|
| `c_two.rpc.event.msg_type.MsgType` | `c_two.transport.ipc.msg_type.MsgType` |
| `c_two.rpc.event.envelope.Envelope` | `c_two.transport.ipc.envelope.Envelope` |
| `c_two.rpc.ipc.ipc_protocol.*` | `c_two.transport.ipc.frame.*` |
| `c_two.rpc.ipc.ipc_v3_protocol.*` | `c_two.transport.ipc.buddy.*` |
| `c_two.rpc.util.wire.*` | `c_two.transport.wire_v1.*` |
| `c_two.rpc.transferable.*` | `c_two.crm.transferable.*` |
| `c_two.rpc_v2.server.ServerV2` | `c_two.transport.server.core.ServerV2` |
| `c_two.rpc_v2.client.SharedClient` | `c_two.transport.client.core.SharedClient` |
| `c_two.rpc_v2.pool.ClientPool` | `c_two.transport.client.pool.ClientPool` |
| `c_two.rpc_v2.proxy.ICRMProxy` | `c_two.transport.client.proxy.ICRMProxy` |
| `c_two.rpc_v2.http_client.*` | `c_two.transport.client.http.*` |
| `c_two.rpc_v2.relay.*` | `c_two.transport.relay.core.*` |
| `c_two.rpc_v2.wire.*` | `c_two.transport.wire.*` |
| `c_two.rpc_v2.protocol.*` | `c_two.transport.protocol.*` |
| `c_two.rpc_v2.registry.*` | `c_two.transport.registry.*` |
| `c_two.rpc_v2.config.*` | `c_two.transport.config.*` |
| `c_two.rpc_v2.scheduler.*` | `c_two.transport.server.scheduler.*` |

---

## Phase 1: Create `transport/` with shared infrastructure

### Task 1: Create `transport/ipc/` package (shared protocol infrastructure)

**Files:**
- Create: `src/c_two/transport/__init__.py` (empty initially)
- Create: `src/c_two/transport/ipc/__init__.py`
- Create: `src/c_two/transport/ipc/msg_type.py` ← copy from `src/c_two/rpc/event/msg_type.py`
- Create: `src/c_two/transport/ipc/envelope.py` ← copy from `src/c_two/rpc/event/envelope.py`
- Create: `src/c_two/transport/ipc/frame.py` ← copy from `src/c_two/rpc/ipc/ipc_protocol.py` (remove AdaptiveBuffer)
- Create: `src/c_two/transport/ipc/buddy.py` ← copy from `src/c_two/rpc/ipc/ipc_v3_protocol.py`
- Create: `src/c_two/transport/wire_v1.py` ← copy from `src/c_two/rpc/util/wire.py`
- Create: `src/c_two/transport/server/__init__.py` (empty stub)
- Create: `src/c_two/transport/client/__init__.py` (empty stub)
- Create: `src/c_two/transport/relay/__init__.py` (empty stub)

- [ ] **Step 1: Create directory structure and empty `__init__.py` files**

```bash
mkdir -p src/c_two/transport/{ipc,server,client,relay}
touch src/c_two/transport/__init__.py
touch src/c_two/transport/server/__init__.py
touch src/c_two/transport/client/__init__.py
touch src/c_two/transport/relay/__init__.py
touch src/c_two/transport/ipc/__init__.py
```

- [ ] **Step 2: Copy `msg_type.py` (no changes needed)**

Copy `src/c_two/rpc/event/msg_type.py` → `src/c_two/transport/ipc/msg_type.py` verbatim. The file is self-contained (no internal imports).

```bash
cp src/c_two/rpc/event/msg_type.py src/c_two/transport/ipc/msg_type.py
```

- [ ] **Step 3: Copy `envelope.py` and update its import**

Copy `src/c_two/rpc/event/envelope.py` → `src/c_two/transport/ipc/envelope.py`.

Change line 4:
```python
# OLD:
from .msg_type import MsgType
# NEW (same — relative import still works within ipc/):
from .msg_type import MsgType
```

No change needed — the relative import `from .msg_type import MsgType` already resolves correctly within `transport/ipc/`.

```bash
cp src/c_two/rpc/event/envelope.py src/c_two/transport/ipc/envelope.py
```

- [ ] **Step 4: Copy `ipc_protocol.py` → `frame.py` and remove AdaptiveBuffer**

Copy `src/c_two/rpc/ipc/ipc_protocol.py` → `src/c_two/transport/ipc/frame.py`.

Then make these edits in `frame.py`:

1. **Remove** the AdaptiveBuffer import (line 17):
```python
# DELETE this line:
from ..util.adaptive_buffer import AdaptiveBuffer
```

2. **Remove** the two functions that use AdaptiveBuffer: `read_shm_data()` and `read_pool_shm_data()` (they are only used by v1 server/client, NOT by rpc_v2).

3. **Update** any `from ..` imports. The original `ipc_protocol.py` is at `rpc/ipc/ipc_protocol.py`. The new `frame.py` is at `transport/ipc/frame.py`. Check if there are other relative imports and update them. Specifically, if the file imports from `..error` or similar, update to `...error` (parent of transport is c_two).

Verify that `frame.py` exports at minimum: `IPCConfig`, `FRAME_STRUCT`, `FRAME_HEADER_SIZE`, `FLAG_RESPONSE`, `FLAG_SHM`, `FLAG_BUDDY`, `encode_frame`, `decode_frame_header`, `MAX_INLINE_SIZE`, `SHM_NAME_RE`. Check the exact exports by examining what rpc_v2 files import from ipc_protocol.

- [ ] **Step 5: Copy `ipc_v3_protocol.py` → `buddy.py` and update imports**

Copy `src/c_two/rpc/ipc/ipc_v3_protocol.py` → `src/c_two/transport/ipc/buddy.py`.

Update internal imports:
```python
# OLD (line ~16):
from .ipc_protocol import (...)
# NEW:
from .frame import (...)
```

If the file imports from `..` paths (e.g., `from ..error import ...`), update to `...error` (c_two package level).

- [ ] **Step 6: Copy `wire.py` → `wire_v1.py` and update imports**

Copy `src/c_two/rpc/util/wire.py` → `src/c_two/transport/wire_v1.py`.

Update internal imports:
```python
# OLD:
from ..rpc.event.msg_type import MsgType
from ..rpc.event.envelope import Envelope, CompletionType
# NEW:
from .ipc.msg_type import MsgType
from .ipc.envelope import Envelope, CompletionType
```

The original `wire.py` is at `rpc/util/wire.py` and uses relative imports like `from ..event.msg_type import MsgType`. In the new location (`transport/wire_v1.py`), the relative path is `from .ipc.msg_type import MsgType`.

Check ALL imports in the file and update accordingly. Likely imports to fix:
- `from ..event.msg_type import MsgType` → `from .ipc.msg_type import MsgType`
- `from ..event.envelope import Envelope, CompletionType` → `from .ipc.envelope import Envelope, CompletionType`

- [ ] **Step 7: Verify new package imports**

```bash
uv run python -c "
from c_two.transport.ipc.msg_type import MsgType
from c_two.transport.ipc.envelope import Envelope
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport.ipc.buddy import BuddyPoolHandle
from c_two.transport.wire_v1 import encode_call, decode
print('All transport/ipc imports OK')
"
```

Expected: `All transport/ipc imports OK`

- [ ] **Step 8: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All 855± tests pass. No behavior change — old code still uses old paths.

---

### Task 2: Add re-export shims in `rpc/` and commit Phase 1

**Files:**
- Modify: `src/c_two/rpc/event/msg_type.py`
- Modify: `src/c_two/rpc/event/envelope.py`
- Modify: `src/c_two/rpc/ipc/ipc_protocol.py`
- Modify: `src/c_two/rpc/ipc/ipc_v3_protocol.py`
- Modify: `src/c_two/rpc/util/wire.py`

- [ ] **Step 1: Add re-export shim to `rpc/event/msg_type.py`**

Replace the entire file content with:
```python
"""Shim — canonical location is now c_two.transport.ipc.msg_type."""
from c_two.transport.ipc.msg_type import *  # noqa: F401,F403
from c_two.transport.ipc.msg_type import MsgType  # noqa: F401  — explicit for type checkers
```

- [ ] **Step 2: Add re-export shim to `rpc/event/envelope.py`**

Replace the entire file content with:
```python
"""Shim — canonical location is now c_two.transport.ipc.envelope."""
from c_two.transport.ipc.envelope import *  # noqa: F401,F403
from c_two.transport.ipc.envelope import Envelope, CompletionType  # noqa: F401
```

- [ ] **Step 3: Add re-export shim to `rpc/ipc/ipc_protocol.py`**

**Do NOT replace the entire file.** The v1 code still uses `read_shm_data()` and `read_pool_shm_data()` which depend on `AdaptiveBuffer`. Instead, add a forwarding header that re-exports everything from `frame.py`, while keeping the AdaptiveBuffer-dependent functions in place.

At the top of the file, add:
```python
# Re-export all symbols from the canonical location.
from c_two.transport.ipc.frame import *  # noqa: F401,F403
```

This way: `frame.py` symbols are available, AND the file's own AdaptiveBuffer functions still work for v1 consumers. When v1 is deleted in Phase 4, this entire file goes away.

- [ ] **Step 4: Add re-export shim to `rpc/ipc/ipc_v3_protocol.py`**

At the top of the file, add:
```python
# Re-export all symbols from the canonical location.
from c_two.transport.ipc.buddy import *  # noqa: F401,F403
```

- [ ] **Step 5: Add re-export shim to `rpc/util/wire.py`**

At the top of the file, add:
```python
# Re-export all symbols from the canonical location.
from c_two.transport.wire_v1 import *  # noqa: F401,F403
```

- [ ] **Step 6: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All 855± tests pass. Shims are transparent.

- [ ] **Step 7: Commit Phase 1**

```bash
git add src/c_two/transport/ src/c_two/rpc/event/msg_type.py src/c_two/rpc/event/envelope.py \
        src/c_two/rpc/ipc/ipc_protocol.py src/c_two/rpc/ipc/ipc_v3_protocol.py src/c_two/rpc/util/wire.py
git commit -m "refactor: create transport/ skeleton with shared IPC infrastructure

Phase 1 of rpc_v2 promotion refactor:
- Copy msg_type, envelope, ipc_protocol, ipc_v3_protocol, wire to transport/
- Remove AdaptiveBuffer dependency from transport/ipc/frame.py
- Add re-export shims in rpc/ for backward compatibility
- All tests pass unchanged

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```


---

## Phase 2: Move `rpc_v2/` contents into `transport/`

### Task 3: Move rpc_v2 files into transport/ subpackages

**Files:**
- Create: `src/c_two/transport/server/core.py` ← `rpc_v2/server.py`
- Create: `src/c_two/transport/server/scheduler.py` ← `rpc_v2/scheduler.py`
- Create: `src/c_two/transport/client/core.py` ← `rpc_v2/client.py`
- Create: `src/c_two/transport/client/pool.py` ← `rpc_v2/pool.py`
- Create: `src/c_two/transport/client/proxy.py` ← `rpc_v2/proxy.py`
- Create: `src/c_two/transport/client/http.py` ← `rpc_v2/http_client.py`
- Create: `src/c_two/transport/relay/core.py` ← `rpc_v2/relay.py`
- Create: `src/c_two/transport/wire.py` ← `rpc_v2/wire.py`
- Create: `src/c_two/transport/protocol.py` ← `rpc_v2/protocol.py`
- Create: `src/c_two/transport/registry.py` ← `rpc_v2/registry.py`
- Create: `src/c_two/transport/config.py` ← `rpc_v2/config.py`

- [ ] **Step 1: Copy all rpc_v2 files to transport/ locations**

```bash
cp src/c_two/rpc_v2/server.py     src/c_two/transport/server/core.py
cp src/c_two/rpc_v2/scheduler.py  src/c_two/transport/server/scheduler.py
cp src/c_two/rpc_v2/client.py     src/c_two/transport/client/core.py
cp src/c_two/rpc_v2/pool.py       src/c_two/transport/client/pool.py
cp src/c_two/rpc_v2/proxy.py      src/c_two/transport/client/proxy.py
cp src/c_two/rpc_v2/http_client.py src/c_two/transport/client/http.py
cp src/c_two/rpc_v2/relay.py      src/c_two/transport/relay/core.py
cp src/c_two/rpc_v2/wire.py       src/c_two/transport/wire.py
cp src/c_two/rpc_v2/protocol.py   src/c_two/transport/protocol.py
cp src/c_two/rpc_v2/registry.py   src/c_two/transport/registry.py
cp src/c_two/rpc_v2/config.py     src/c_two/transport/config.py
```

- [ ] **Step 2: Update imports in `transport/ipc/` — ensure no circular deps**

Verify `transport/ipc/frame.py`, `transport/ipc/buddy.py`, `transport/ipc/msg_type.py`, `transport/ipc/envelope.py` have NO imports from `rpc/` or `rpc_v2/`. They should only use standard library, `...error`, and `.` (sibling) imports.

---

### Task 4: Update all internal imports within transport/

This is the largest mechanical task. Each moved file has imports from `..rpc.*` (cross-package) and `from .` (sibling within rpc_v2/). Both must be updated.

**Import update rules for files now inside `transport/server/`:**
- `from ..rpc.event.msg_type import X` → `from ..ipc.msg_type import X`
- `from ..rpc.util.wire import X` → `from ..wire_v1 import X`
- `from ..rpc.ipc.ipc_protocol import X` → `from ..ipc.frame import X`
- `from ..rpc.ipc.ipc_v3_protocol import X` → `from ..ipc.buddy import X`
- `from ..crm.meta import X` → `from ...crm.meta import X` (up to c_two, down to crm)
- `from .wire import X` → `from ..wire import X` (was sibling in rpc_v2, now parent)
- `from .protocol import X` → `from ..protocol import X`
- `from .scheduler import X` → `from .scheduler import X` (same subpackage, no change)
- `from .client import X` → `from ..client.core import X`
- `from .config import X` → `from ..config import X`

**Import update rules for files now inside `transport/client/`:**
- Same `..rpc.*` → `..ipc.*`/`..wire_v1` pattern as above
- `from ..crm.meta import X` → `from ...crm.meta import X`
- `from .pool import X` → `from .pool import X` (same subpackage)
- `from .client import SharedClient` → `from .core import SharedClient` (renamed file)

**Import update rules for files now at `transport/` top level (`wire.py`, `protocol.py`, `registry.py`, `config.py`):**
- `from ..rpc.ipc.ipc_protocol import X` → `from .ipc.frame import X`
- `from ..rpc.ipc.ipc_v3_protocol import X` → `from .ipc.buddy import X`
- `from ..crm.meta import X` → `from ..crm.meta import X` (still one level up to c_two)
- `from .server import ServerV2` → `from .server.core import ServerV2`
- `from .client import SharedClient` → `from .client.core import SharedClient`
- `from .pool import ClientPool` → `from .client.pool import ClientPool`
- `from .proxy import ICRMProxy` → `from .client.proxy import ICRMProxy`
- `from .http_client import X` → `from .client.http import X`
- `from .relay import X` → `from .relay.core import X`
- `from .scheduler import X` → `from .server.scheduler import X`

**Files to update (in order):**

- [ ] **Step 1: Update `transport/ipc/frame.py`**

Fix any `..` imports. If `ipc_protocol.py` had `from ...error import X`, check: from `transport/ipc/frame.py`, `...error` goes up 3 levels (frame→ipc→transport→c_two) then into error. This is correct.

Remove any `from ..util.adaptive_buffer import AdaptiveBuffer` line (should already be done in Task 1 Step 4 — verify).

- [ ] **Step 2: Update `transport/ipc/buddy.py`**

```python
# OLD:
from .ipc_protocol import (...)
# NEW:
from .frame import (...)
```

- [ ] **Step 3: Update `transport/server/core.py` (was server.py)**

This is the largest file (~1650 lines). Update ALL imports:
```python
# OLD:
from ..crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ..rpc.event.msg_type import MsgType
from ..rpc.util.wire import (
    encode_reply, decode, preregister_methods,
    payload_total_size, write_reply_into,
    REPLY_HEADER_SIZE,
)
from ..rpc.ipc.ipc_protocol import (
    IPCConfig, FRAME_STRUCT, FRAME_HEADER_SIZE,
    FLAG_RESPONSE, FLAG_SHM, encode_frame,
    decode_frame_header, MAX_INLINE_SIZE, SHM_NAME_RE,
)
from ..rpc.ipc.ipc_v3_protocol import (
    BuddyPoolHandle, PoolConfig, buddy_write_to_pool,
    buddy_read_from_pool, BUDDY_PAYLOAD_STRUCT, FLAG_BUDDY,
)

# NEW:
from ...crm.meta import MethodAccess, get_method_access, get_shutdown_method
from ..ipc.msg_type import MsgType
from ..wire_v1 import (
    encode_reply, decode, preregister_methods,
    payload_total_size, write_reply_into,
    REPLY_HEADER_SIZE,
)
from ..ipc.frame import (
    IPCConfig, FRAME_STRUCT, FRAME_HEADER_SIZE,
    FLAG_RESPONSE, FLAG_SHM, encode_frame,
    decode_frame_header, MAX_INLINE_SIZE, SHM_NAME_RE,
)
from ..ipc.buddy import (
    BuddyPoolHandle, PoolConfig, buddy_write_to_pool,
    buddy_read_from_pool, BUDDY_PAYLOAD_STRUCT, FLAG_BUDDY,
)
```

Also update intra-rpc_v2 imports:
```python
# OLD:
from .wire import (...)
from .protocol import (...)
from .scheduler import (...)
# NEW:
from ..wire import (...)
from ..protocol import (...)
from .scheduler import (...)  # same subpackage, unchanged
```

Check for any `from .config import` or `from .client import` references and update similarly.

- [ ] **Step 4: Update `transport/server/scheduler.py`**

```python
# OLD:
from ..crm.meta import MethodAccess, get_method_access
# NEW:
from ...crm.meta import MethodAccess, get_method_access
```

- [ ] **Step 5: Update `transport/client/core.py` (was client.py)**

Update ALL imports following the same patterns:
```python
# OLD:
from ..rpc.event.msg_type import MsgType
from ..rpc.util.wire import (...)
from ..rpc.ipc.ipc_protocol import (...)
from ..rpc.ipc.ipc_v3_protocol import (...)
# NEW:
from ..ipc.msg_type import MsgType
from ..wire_v1 import (...)
from ..ipc.frame import (...)
from ..ipc.buddy import (...)
```

Also update intra-rpc_v2 references:
```python
# OLD:
from .wire import (...)
from .protocol import (...)
# NEW:
from ..wire import (...)
from ..protocol import (...)
```

Check for any lazy/inline imports (e.g., `from ..rpc.ipc.ipc_v3_protocol import BUDDY_PAYLOAD_STRUCT` at lines ~898, ~1036) and update those too.

- [ ] **Step 6: Update `transport/client/pool.py`**

```python
# OLD:
from ..rpc.ipc.ipc_protocol import IPCConfig
from .client import SharedClient
# NEW:
from ..ipc.frame import IPCConfig
from .core import SharedClient
```

- [ ] **Step 7: Update `transport/client/proxy.py`**

```python
# OLD:
from ..crm.meta import MethodAccess
from .scheduler import (...)
# NEW:
from ...crm.meta import MethodAccess
from ..server.scheduler import (...)
```

Check for other imports referencing sibling files that moved to subpackages.

- [ ] **Step 8: Update `transport/client/http.py` (was http_client.py)**

Check imports — likely references to `.protocol`, `.client`, `.pool`. Update:
```python
# from .protocol import ... → from ..protocol import ...
# from .client import SharedClient → from .core import SharedClient
# from .pool import ClientPool → from .pool import ClientPool  # same subpackage
```

- [ ] **Step 9: Update `transport/relay/core.py` (was relay.py)**

```python
# OLD:
from ..rpc.util.wait import wait
from ..rpc.ipc.ipc_protocol import IPCConfig
from .client import SharedClient
from .pool import ClientPool
# NEW (inline wait function):
from ..ipc.frame import IPCConfig
from ..client.core import SharedClient
from ..client.pool import ClientPool
```

For the `wait` function (from `rpc/util/wait.py`): **inline it** directly in `relay/core.py` as a module-level `_wait()` helper. It is 15 lines and only used in this one file. This avoids creating a utility module in transport/ just for one consumer.

```python
import time as _time

_MAX_WAIT_TIMEOUT = 0.1

def _wait(wait_fn, wait_complete_fn, timeout, spin_cb=None):
    """Spin-wait with timeout (inlined from rpc/util/wait.py)."""
    if timeout is None:
        while not wait_complete_fn():
            wait_fn(timeout=_MAX_WAIT_TIMEOUT)
            if spin_cb is not None:
                spin_cb()
        return True
    end = _time.time() + timeout
    while not wait_complete_fn():
        remaining = min(end - _time.time(), _MAX_WAIT_TIMEOUT)
        if remaining < 0:
            return True
        wait_fn(timeout=remaining)
        if spin_cb is not None:
            spin_cb()
    return False
```

- [ ] **Step 10: Update `transport/wire.py` (was rpc_v2/wire.py)**

```python
# OLD:
from ..rpc.ipc.ipc_protocol import (...)
from ..rpc.ipc.ipc_v3_protocol import (...)
# NEW:
from .ipc.frame import (...)
from .ipc.buddy import (...)
```

- [ ] **Step 11: Update `transport/protocol.py`**

Check for any rpc/ or rpc_v2/ imports and update. Likely minimal — protocol.py is mostly self-contained with dataclass definitions.

- [ ] **Step 12: Update `transport/registry.py`**

This is the second-largest file. Update:
```python
# OLD:
from ..crm.meta import MethodAccess
from ..rpc.ipc.ipc_protocol import IPCConfig
from .server import ServerV2
from .client import SharedClient
from .pool import ClientPool
from .proxy import ICRMProxy
from .http_client import HttpClient, HttpClientPool
from .relay import RelayV2
from .scheduler import Scheduler, ConcurrencyConfig
from .config import C2Settings
# NEW:
from ..crm.meta import MethodAccess
from .ipc.frame import IPCConfig
from .server.core import ServerV2
from .client.core import SharedClient
from .client.pool import ClientPool
from .client.proxy import ICRMProxy
from .client.http import HttpClient, HttpClientPool
from .relay.core import RelayV2
from .server.scheduler import Scheduler, ConcurrencyConfig
from .config import C2Settings
```

- [ ] **Step 13: Update `transport/config.py`**

Likely self-contained (pydantic BaseSettings). Check for any rpc_v2/ imports.

- [ ] **Step 14: Verify all transport/ imports resolve**

```bash
uv run python -c "
from c_two.transport.server.core import ServerV2
from c_two.transport.client.core import SharedClient
from c_two.transport.client.pool import ClientPool
from c_two.transport.client.proxy import ICRMProxy
from c_two.transport.client.http import HttpClient
from c_two.transport.relay.core import RelayV2
from c_two.transport.wire import encode_v2_call
from c_two.transport.wire_v1 import encode_call
from c_two.transport.protocol import HandshakeV5
from c_two.transport.registry import register, connect, close, shutdown
from c_two.transport.config import C2Settings
from c_two.transport.server.scheduler import Scheduler, ConcurrencyConfig
print('All transport imports OK')
"
```

Expected: `All transport imports OK`

---

### Task 5: Write `transport/__init__.py`, add `rpc_v2/` shim, update `c_two/__init__.py`

**Files:**
- Modify: `src/c_two/transport/__init__.py`
- Modify: `src/c_two/transport/server/__init__.py`
- Modify: `src/c_two/transport/client/__init__.py`
- Modify: `src/c_two/transport/relay/__init__.py`
- Modify: `src/c_two/transport/ipc/__init__.py`
- Modify: `src/c_two/rpc_v2/__init__.py` (replace with shim)
- Modify: `src/c_two/__init__.py`

- [ ] **Step 1: Write subpackage `__init__.py` files**

`transport/server/__init__.py`:
```python
from .core import ServerV2, CRMSlot
from .scheduler import Scheduler, ConcurrencyConfig, ConcurrencyMode
```

`transport/client/__init__.py`:
```python
from .core import SharedClient
from .pool import ClientPool
from .proxy import ICRMProxy
from .http import HttpClient, HttpClientPool
```

`transport/relay/__init__.py`:
```python
from .core import RelayV2, UpstreamPool
```

`transport/ipc/__init__.py`:
```python
from .frame import IPCConfig
from .msg_type import MsgType
from .envelope import Envelope, CompletionType
```

- [ ] **Step 2: Write `transport/__init__.py`**

```python
"""Transport layer — promoted from rpc_v2/.

Provides the complete transport stack for C-Two:
- ServerV2: asyncio-based multi-CRM IPC server
- SharedClient: concurrent multiplexed IPC client
- ClientPool: reference-counted client lifecycle
- ICRMProxy: unified proxy (thread-local / IPC / HTTP)
- RelayV2: HTTP → IPC bridge (Starlette + uvicorn)
- Registry: cc.register/connect/close/shutdown SOTA API
"""
from __future__ import annotations

from .client import SharedClient, ClientPool, ICRMProxy, HttpClient, HttpClientPool
from .server import ServerV2, CRMSlot, Scheduler, ConcurrencyConfig, ConcurrencyMode
from .relay import RelayV2, UpstreamPool
from .registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)

__all__ = [
    'SharedClient', 'ClientPool', 'ICRMProxy',
    'HttpClient', 'HttpClientPool',
    'ServerV2', 'CRMSlot',
    'Scheduler', 'ConcurrencyConfig', 'ConcurrencyMode',
    'RelayV2', 'UpstreamPool',
    'set_address', 'set_ipc_config', 'register', 'connect', 'close',
    'unregister', 'server_address', 'shutdown', 'serve',
]
```

- [ ] **Step 3: Replace `rpc_v2/__init__.py` with re-export shim**

Replace the entire content of `src/c_two/rpc_v2/__init__.py`:
```python
"""Shim — canonical location is now c_two.transport."""
from c_two.transport import *  # noqa: F401,F403
from c_two.transport import (  # noqa: F401  — explicit for type checkers
    SharedClient, ClientPool, ICRMProxy,
    HttpClient, HttpClientPool,
    ServerV2, CRMSlot,
    Scheduler, ConcurrencyConfig, ConcurrencyMode,
    RelayV2, UpstreamPool,
    set_address, set_ipc_config, register, connect, close,
    unregister, server_address, shutdown,
)
```

Similarly, replace each `rpc_v2/*.py` file with a one-line shim redirecting to `transport/`:

`rpc_v2/server.py`:
```python
"""Shim."""
from c_two.transport.server.core import *  # noqa: F401,F403
from c_two.transport.server.core import ServerV2, CRMSlot  # noqa: F401
```

`rpc_v2/client.py`:
```python
"""Shim."""
from c_two.transport.client.core import *  # noqa: F401,F403
from c_two.transport.client.core import SharedClient  # noqa: F401
```

Apply the same pattern for: `pool.py`, `proxy.py`, `http_client.py`, `relay.py`, `wire.py`, `protocol.py`, `registry.py`, `config.py`, `scheduler.py`.

- [ ] **Step 4: Update `c_two/__init__.py`**

```python
# OLD (line 11-21):
from .rpc_v2.registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)

# NEW:
from .transport.registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)
```

Keep `from . import rpc` for now — deleted in Phase 4.

- [ ] **Step 5: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All 855± tests pass. Both old (`rpc_v2.X`) and new (`transport.X`) import paths work.

- [ ] **Step 6: Commit Phase 2**

```bash
git add src/c_two/transport/ src/c_two/rpc_v2/ src/c_two/__init__.py
git commit -m "refactor: move rpc_v2/ contents into transport/ package

Phase 2 of rpc_v2 promotion refactor:
- Move 11 rpc_v2 files into transport/{server,client,relay}/
- Update all internal imports to new package structure
- Write transport/__init__.py with full re-exports
- Replace rpc_v2/ files with re-export shims
- Update c_two/__init__.py to source from transport/
- All tests pass unchanged

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 3: Migrate `transferable.py`, rewrite `compo/`, update `conftest.py`

### Task 6: Move `transferable.py` to `crm/` and update dependent imports

**Files:**
- Create: `src/c_two/crm/transferable.py` ← `rpc/transferable.py`
- Modify: `src/c_two/crm/meta.py` (lines 4-5)
- Modify: `src/c_two/__init__.py` (line 10)
- Modify: `src/c_two/rpc/transferable.py` (replace with shim)

- [ ] **Step 1: Copy `rpc/transferable.py` → `crm/transferable.py`**

```bash
cp src/c_two/rpc/transferable.py src/c_two/crm/transferable.py
```

- [ ] **Step 2: Update imports in `crm/transferable.py`**

The file imports:
```python
# OLD (line 11-12):
from .. import error
from .util.encoding import add_length_prefix
```

Since the file is now at `crm/transferable.py`, `..` goes to `c_two/`, so `from .. import error` is still correct.

For `add_length_prefix` — **inline it** (6 lines). Remove the import and add the function body:

```python
# DELETE:
from .util.encoding import add_length_prefix

# ADD (at module level, after other imports):
import struct as _struct

def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = _struct.pack('>Q', length)
    return prefix + message_bytes
```

Then find all usages of `add_length_prefix` in the file and rename to `_add_length_prefix`.

- [ ] **Step 3: Update `crm/meta.py` imports**

```python
# OLD (lines 4-5):
from ..rpc.transferable import auto_transfer
from ..rpc.util.wire import preregister_methods

# NEW:
from .transferable import auto_transfer
from ..transport.wire_v1 import preregister_methods
```

- [ ] **Step 4: Update `c_two/__init__.py`**

```python
# OLD (line 10):
from .rpc.transferable import transferable

# NEW:
from .crm.transferable import transferable
```

- [ ] **Step 5: Add shim to `rpc/transferable.py`**

Replace the entire file content with:
```python
"""Shim — canonical location is now c_two.crm.transferable."""
from c_two.crm.transferable import *  # noqa: F401,F403
from c_two.crm.transferable import (  # noqa: F401
    transferable, auto_transfer, TransferableMeta,
    create_default_transferable, Transferable,
)
```

- [ ] **Step 6: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All tests pass. `@cc.transferable` works via new path.

- [ ] **Step 7: Commit**

```bash
git add src/c_two/crm/transferable.py src/c_two/crm/meta.py \
        src/c_two/__init__.py src/c_two/rpc/transferable.py
git commit -m "refactor: move transferable.py to crm/ layer

- Copy rpc/transferable.py to crm/transferable.py
- Inline add_length_prefix (6 lines, single consumer)
- Update crm/meta.py to import from .transferable and ..transport.wire_v1
- Update c_two/__init__.py to source @cc.transferable from crm/
- Add re-export shim in rpc/transferable.py

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 7: Rewrite `tests/conftest.py` — replace v1 server fixtures

**Files:**
- Modify: `tests/conftest.py`

The current `conftest.py` has:
- 7 protocol address fixtures (thread, memory, tcp, http, ipc-v2, ipc-v3, ipc)
- `protocol_address` parametrized fixture (all 7 protocols)
- `hello_server` fixture using `cc.rpc.ServerConfig`, `cc.rpc.Server`, `_start()`

After refactor: only `thread`, `ipc-v3`, and `http` protocols remain. Server setup uses `ServerV2`.

- [ ] **Step 1: Rewrite `tests/conftest.py`**

Replace the entire file with:

```python
import os
import pytest
import threading
import time
import c_two as cc

from c_two.transport.server.core import ServerV2
from c_two.transport.client.core import SharedClient
from c_two.transport.ipc.frame import IPCConfig

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello

# Disable proxy for localhost to avoid HTTP test failures
os.environ.setdefault('NO_PROXY', '127.0.0.1,localhost')
os.environ.setdefault('no_proxy', '127.0.0.1,localhost')


# Unique address factory to avoid conflicts between tests
_address_counter = 0
_address_lock = threading.Lock()

def _next_id():
    global _address_counter
    with _address_lock:
        _address_counter += 1
        return _address_counter


@pytest.fixture
def unique_ipc_v3_address():
    return f'ipc-v3://test_hello_{_next_id()}'


@pytest.fixture(params=['ipc-v3'])
def protocol_address(request, unique_ipc_v3_address):
    """Parametrized fixture providing a unique address for each supported protocol."""
    addresses = {
        'ipc-v3': unique_ipc_v3_address,
    }
    return addresses[request.param]


@pytest.fixture
def hello_crm():
    """Create a Hello CRM instance."""
    return Hello()


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    """Poll until the server responds to ping."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


@pytest.fixture
def hello_server(protocol_address, hello_crm):
    """Start a Hello CRM server on the given protocol, yield the address, then shut down."""
    server = ServerV2(
        bind_address=protocol_address,
        icrm_class=IHello,
        crm_instance=hello_crm,
    )
    server.start()
    _wait_for_server(protocol_address)

    yield protocol_address

    server.shutdown()
```

- [ ] **Step 2: Run conftest-dependent tests**

```bash
uv run pytest tests/integration/test_component_runtime.py -q --timeout=30
```

Expected: Tests pass (reduced from 7 protocol params to 1, so test count drops).

- [ ] **Step 3: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All remaining tests pass.

---

### Task 8: Rewrite `compo/runtime_connect.py` — remove v1 Client dependency

**Files:**
- Modify: `src/c_two/compo/runtime_connect.py`

The current file uses `from ..rpc import Client` (v1). Rewrite to use `cc.connect()` / `cc.close()` SOTA API.

**Key design decision:** The `cc.connect()` API requires a `name` parameter. The current `connect_crm(address, icrm_class)` API does not have one. Solution: add an optional `crm_name` parameter defaulting to `'default'`.

- [ ] **Step 1: Rewrite `compo/runtime_connect.py`**

```python
import inspect
import logging
import threading
from functools import wraps
from contextlib import contextmanager
from typing import ContextManager, Generator, TypeVar, overload

from ..transport.registry import connect as _transport_connect
from ..transport.registry import close as _transport_close

_local = threading.local()

ICRM = TypeVar('ICRM')

logger = logging.getLogger(__name__)


@overload
def connect_crm(address: str, icrm_class: type[ICRM], *, crm_name: str = 'default') -> ContextManager[ICRM]:
    ...


@contextmanager
def connect_crm(
    address: str,
    icrm_class: type[ICRM] = None,
    *,
    crm_name: str = 'default',
    **kwargs,
) -> Generator[ICRM, None, None]:
    """Context manager to connect to a CRM server.

    Args:
        address: The server address string (e.g., 'ipc-v3://region')
        icrm_class: The ICRM class to connect as
        crm_name: The CRM routing name (default: 'default')
        **kwargs: Reserved for future use
    """
    if icrm_class is None:
        raise ValueError('icrm_class is required for connect_crm')

    proxy = _transport_connect(icrm_class, name=crm_name, address=address)
    old_proxy = getattr(_local, 'current_proxy', None)
    _local.current_proxy = proxy
    try:
        yield proxy
    finally:
        if old_proxy is not None:
            _local.current_proxy = old_proxy
        elif hasattr(_local, 'current_proxy'):
            delattr(_local, 'current_proxy')
        _transport_close(proxy)


def get_current_client():
    """Get the current proxy from the thread-local storage."""
    return getattr(_local, 'current_proxy', None)


def connect(func=None, icrm_class=None) -> callable:
    """Decorator: inject a connected ICRM proxy as the first argument.

    Supports both @connect and @connect() usage patterns.
    If icrm_class is None, it is inferred from the first parameter's
    type annotation.

    The decorated function accepts additional kwargs:
    - crm_address: str — server address
    - crm_name: str — CRM routing name (default: 'default')
    - crm_connection: pre-existing proxy (for testing)
    """
    def create_wrapper(func):
        nonlocal icrm_class
        if icrm_class is None:
            signature = inspect.signature(func)
            params = list(signature.parameters.values())
            if params and params[0].annotation != inspect.Parameter.empty:
                icrm_class = params[0].annotation
            else:
                raise ValueError(
                    f"Could not infer ICRM class from {func.__name__}'s first parameter. "
                    "Either provide icrm_class explicitly or add type annotations."
                )

        @wraps(func)
        def wrapper(
            *args,
            crm_address: str = '',
            crm_name: str = 'default',
            crm_connection=None,
        ):
            # Priority: runtime context > provided connection > new from address
            current = get_current_client()
            if current is not None:
                proxy = current
                close_proxy = False
            elif crm_connection is not None:
                proxy = crm_connection
                close_proxy = False
            elif crm_address:
                proxy = _transport_connect(
                    icrm_class, name=crm_name, address=crm_address,
                )
                close_proxy = True
            else:
                raise ValueError(
                    "No client available: use 'with connect_crm()', "
                    "or provide 'crm_address' or 'crm_connection'"
                )

            try:
                return func(proxy, *args)
            finally:
                if close_proxy:
                    _transport_close(proxy)

        return wrapper

    if func is not None:
        return create_wrapper(func)

    if icrm_class is not None and not isinstance(icrm_class, type) and callable(icrm_class):
        return create_wrapper(icrm_class)

    return create_wrapper
```

- [ ] **Step 2: Run component runtime tests**

```bash
uv run pytest tests/integration/test_component_runtime.py -v --timeout=30
```

Expected: Tests pass. The component decorator injects ICRM proxies via transport API.

Note: `test_component_runtime.py` will also need import updates (done in Task 9). If it fails here due to the `hello_server` fixture using v1 Server, that was already fixed in Task 7. If it fails due to `from c_two.rpc import Client` in the test file itself, update that import too.

- [ ] **Step 3: Commit Phase 3**

```bash
git add src/c_two/crm/ src/c_two/compo/ src/c_two/__init__.py \
        src/c_two/rpc/transferable.py tests/conftest.py
git commit -m "refactor: migrate transferable to crm/, rewrite compo/ to use SOTA API

Phase 3 of rpc_v2 promotion refactor:
- Move transferable.py to crm/ (inline add_length_prefix)
- Rewrite compo/runtime_connect.py to use cc.connect()/cc.close()
- Remove all v1 Client dependency from compo layer
- Rewrite tests/conftest.py hello_server to use ServerV2
- Reduce protocol_address to ipc-v3 only

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Phase 4: Delete `rpc/`, clean peripherals, update all test imports

### Task 9: Update all test imports from `rpc_v2` → `transport` and `rpc` → `transport`/`crm`

**Files to modify (import path updates only — no logic changes):**

Unit tests:
- `tests/unit/test_wire_v2.py`
- `tests/unit/test_transferable.py`
- `tests/unit/test_shared_client.py`
- `tests/unit/test_client_pool.py`
- `tests/unit/test_ipc_v3.py`
- `tests/unit/test_icrm_proxy.py`
- `tests/unit/test_proxy_concurrency.py`
- `tests/unit/test_scheduler.py`
- `tests/unit/test_security_v2.py`
- `tests/unit/test_serve.py`
- `tests/unit/test_http_client.py`
- `tests/unit/test_name_collision.py`
- `tests/unit/test_shutdown_decorator.py`
- `tests/unit/test_chunk_assembler.py`
- `tests/unit/test_relay_graceful_shutdown.py`
- `tests/unit/test_native_relay.py`
- `tests/unit/test_buddy_pool.py`

Integration tests:
- `tests/integration/test_rpc_v2_server.py`
- `tests/integration/test_error_propagation.py`
- `tests/integration/test_backpressure.py`
- `tests/integration/test_component_runtime.py`
- `tests/integration/test_icrm_proxy.py`
- `tests/integration/test_multi_crm_server.py`
- `tests/integration/test_http_relay.py`
- `tests/integration/test_registry.py`
- `tests/integration/test_concurrency_safety.py`
- `tests/integration/test_chunked_transfer.py`
- `tests/integration/test_p0_fixes.py`
- `tests/integration/test_serve.py`

Apply these mechanical replacements across ALL files:

```
from c_two.rpc_v2 import X         → from c_two.transport import X
from c_two.rpc_v2.server import X  → from c_two.transport.server.core import X
from c_two.rpc_v2.client import X  → from c_two.transport.client.core import X
from c_two.rpc_v2.pool import X    → from c_two.transport.client.pool import X
from c_two.rpc_v2.proxy import X   → from c_two.transport.client.proxy import X
from c_two.rpc_v2.http_client import X → from c_two.transport.client.http import X
from c_two.rpc_v2.relay import X   → from c_two.transport.relay.core import X
from c_two.rpc_v2.wire import X    → from c_two.transport.wire import X
from c_two.rpc_v2.protocol import X → from c_two.transport.protocol import X
from c_two.rpc_v2.registry import X → from c_two.transport.registry import X
from c_two.rpc_v2.config import X  → from c_two.transport.config import X
from c_two.rpc_v2.scheduler import X → from c_two.transport.server.scheduler import X

from c_two.rpc.ipc.ipc_protocol import X → from c_two.transport.ipc.frame import X
from c_two.rpc.ipc.ipc_v3_protocol import X → from c_two.transport.ipc.buddy import X
from c_two.rpc.event.msg_type import X → from c_two.transport.ipc.msg_type import X
from c_two.rpc.event.envelope import X → from c_two.transport.ipc.envelope import X
from c_two.rpc.util.wire import X → from c_two.transport.wire_v1 import X
from c_two.rpc.transferable import X → from c_two.crm.transferable import X
from c_two.rpc import ConcurrencyConfig → from c_two.transport import ConcurrencyConfig
from c_two.rpc import ServerConfig  → DELETE (v1-only, no equivalent)
from c_two.rpc import Client        → DELETE (v1-only, use cc.connect())
```

- [ ] **Step 1: Run mechanical replacement**

Use a script or manual find-and-replace across all test files. Process each file:
1. Read its imports
2. Apply the mapping above
3. Remove any `from c_two.rpc.server import _start` lines
4. Remove any `from c_two.rpc import Client, ServerConfig` lines

- [ ] **Step 2: Run full test suite to verify import changes**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: Most tests pass. Some tests using `_start()` will fail — those are handled in Task 10.

---

### Task 10: Rewrite test server setup — replace `_start()` with ServerV2

**Files to modify (server setup rewrite):**
- `tests/unit/test_ipc_v3.py`
- `tests/unit/test_shared_client.py`
- `tests/unit/test_client_pool.py`
- `tests/integration/test_rpc_v2_server.py`
- `tests/integration/test_error_propagation.py`
- `tests/integration/test_backpressure.py`
- `tests/integration/test_component_runtime.py`

Each of these files uses the pattern:
```python
from c_two.rpc.server import _start

config = cc.rpc.ServerConfig(name='X', crm=crm, icrm=ICRM, bind_address=addr)
server = cc.rpc.Server(config)
_start(server._state)
```

Replace with:
```python
from c_two.transport.server.core import ServerV2

server = ServerV2(bind_address=addr, icrm_class=ICRM, crm_instance=crm)
server.start()
# ... tests ...
server.shutdown()
```

And for ping/shutdown:
```python
# OLD:
cc.rpc.Client.ping(addr)
cc.rpc.Client.shutdown(addr)

# NEW:
from c_two.transport.client.core import SharedClient
SharedClient.ping(addr)
# For shutdown: just call server.shutdown() directly
```

- [ ] **Step 1: Rewrite `tests/unit/test_ipc_v3.py`**

This file has 6 `_start()` calls. Each test function creates a v1 Server, calls `_start()`, runs assertions, then shuts down. Replace each with ServerV2 pattern.

The file tests low-level IPC v3 behavior (SHM segment handling, buddy protocol). All this functionality is preserved in ServerV2. The test logic stays the same — only the server setup changes.

Common helper to add at top of file:
```python
from c_two.transport.server.core import ServerV2
from c_two.transport.client.core import SharedClient
from c_two.transport.ipc.frame import IPCConfig

def _start_server(addr, icrm_class, crm_instance, ipc_config=None):
    server = ServerV2(
        bind_address=addr,
        icrm_class=icrm_class,
        crm_instance=crm_instance,
        ipc_config=ipc_config,
    )
    server.start()
    # Wait for ready
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(addr, timeout=0.5):
                return server
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server not ready at {addr}')
```

Then replace each `_start(server._state)` block with:
```python
server = _start_server(addr, IHello, Hello(), config)
# ... test code ...
server.shutdown()
```

- [ ] **Step 2: Rewrite remaining test files**

Apply the same pattern to: `test_shared_client.py`, `test_client_pool.py`, `test_rpc_v2_server.py`, `test_error_propagation.py`, `test_backpressure.py`, `test_component_runtime.py`.

For `test_error_propagation.py`: this file uses `cc.rpc.ServerConfig` to configure a server. Replace with `ServerV2(bind_address=addr, icrm_class=ICRM, crm_instance=crm)`.

For `test_backpressure.py`: has 2 `_start()` calls in separate test methods. Replace each independently.

For `test_component_runtime.py`: uses `hello_server` fixture (already rewritten in Task 7) AND has direct `from c_two.rpc import Client, ServerConfig`. Remove those imports. The tests should use the fixture.

- [ ] **Step 3: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All tests pass (except files slated for deletion in Task 11).

---

### Task 11: Delete v1-coupled tests, delete `rpc/` and `rpc_v2/`

**Files to delete:**

v1-coupled test files (cannot work without rpc/):
- `tests/unit/test_ipc_security.py` — tests v1 IPC internals (Event, EventQueue)
- `tests/unit/test_op2_safety.py` — tests v1 IPC v3 client/server raw ops
- `tests/unit/test_adaptive_buffer.py` — tests v1-only AdaptiveBuffer
- `tests/unit/test_encoding.py` — tests v1-only encoding utils
- `tests/unit/test_concurrency.py` — tests v1 ServerConfig concurrency
- `tests/integration/test_rpc_v2_basic.py` — tests v1↔v2 backward compat (meaningless without v1)

Source directories:
- `src/c_two/rpc/` — entire v1 directory (~40 files)
- `src/c_two/rpc_v2/` — shim directory (11 files)

- [ ] **Step 1: Delete v1-coupled test files**

```bash
rm tests/unit/test_ipc_security.py
rm tests/unit/test_op2_safety.py
rm tests/unit/test_adaptive_buffer.py
rm tests/unit/test_encoding.py
rm tests/unit/test_concurrency.py
rm tests/integration/test_rpc_v2_basic.py
```

- [ ] **Step 2: Delete `rpc/` directory**

```bash
rm -rf src/c_two/rpc/
```

- [ ] **Step 3: Delete `rpc_v2/` shim directory**

```bash
rm -rf src/c_two/rpc_v2/
```

- [ ] **Step 4: Update `c_two/__init__.py` — remove `from . import rpc`**

```python
# DELETE these lines:
from . import rpc

# The file should now contain:
from dotenv import load_dotenv
load_dotenv()

from . import mcp
from . import compo
from . import error
from .compo import runtime
from .crm.meta import icrm, read, write, on_shutdown
from .crm.transferable import transferable
from .transport.registry import (
    set_address,
    set_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
)

__version__ = '0.2.7'
# ... LOGO_ASCII ...
```

- [ ] **Step 5: Update `cli.py` imports**

```python
# OLD (line 173):
from .rpc.ipc.ipc_protocol import IPCConfig
# NEW:
from .transport.ipc.frame import IPCConfig

# OLD (line 233):
from .rpc_v2.relay import RelayV2
# NEW:
from .transport.relay.core import RelayV2
```

- [ ] **Step 6: Run full test suite**

```bash
uv run pytest tests/ -q --timeout=30
```

Expected: All tests pass. Test count decreases (deleted 6 test files + fewer protocol parameterizations).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: delete rpc/ and rpc_v2/, remove v1-coupled tests

Phase 4a:
- Delete entire rpc/ directory (~40 files, ~7600 LOC)
- Delete rpc_v2/ shim directory (11 files)
- Delete 6 v1-coupled test files
- Update c_two/__init__.py (remove 'from . import rpc')
- Update cli.py imports to transport/

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 12: Clean examples, benchmarks, dependencies, docs

**Files to delete:**

Examples (v1-only):
- `examples/server.py`
- `examples/client.py`
- `examples/crm.py`
- `examples/icrm.py`
- `examples/example_addresses.py`
- `examples/grid/` (entire directory)

Benchmarks (v1-only):
- `benchmarks/adaptive_buffer_benchmark.py`
- `benchmarks/concurrency_benchmark.py`
- `benchmarks/ipc_v2_vs_v3_benchmark.py`
- `benchmarks/ipc_v3_detailed_bench.py`
- `benchmarks/memory_benchmark.py`
- `benchmarks/memory_vs_ipc_v3_bench.py`
- `benchmarks/wire_preencoding_benchmark.py`

**Files to rename (examples):**
- `examples/v2_server.py` → `examples/server.py`
- `examples/v2_client.py` → `examples/client.py`
- `examples/v2_local.py` → `examples/local.py`
- `examples/v2_relay_server.py` → `examples/relay_server.py`
- `examples/v2_relay_client.py` → `examples/relay_client.py`
- `examples/v2_relay/` → `examples/relay/` (if directory)

**Files to modify:**
- `pyproject.toml` — remove `pyzmq`, `watchdog` from dependencies
- `benchmarks/rpc_v2_vs_v1_benchmark.py` — remove v1 comparison paths or rename
- `benchmarks/chunked_benchmark.py` — update imports if needed

- [ ] **Step 1: Delete v1-only examples**

```bash
rm examples/server.py examples/client.py examples/crm.py examples/icrm.py examples/example_addresses.py
rm -rf examples/grid/
```

- [ ] **Step 2: Rename v2 examples**

```bash
mv examples/v2_server.py examples/server.py
mv examples/v2_client.py examples/client.py
mv examples/v2_local.py examples/local.py
mv examples/v2_relay_server.py examples/relay_server.py
mv examples/v2_relay_client.py examples/relay_client.py
```

If `examples/v2_relay/` exists as a directory:
```bash
mv examples/v2_relay examples/relay
```

- [ ] **Step 3: Update imports in renamed example files**

Check each renamed example for `from c_two.rpc_v2 import` and update to `from c_two.transport import` (or simply use `import c_two as cc` with the SOTA API).

- [ ] **Step 4: Delete v1-only benchmarks**

```bash
rm benchmarks/adaptive_buffer_benchmark.py
rm benchmarks/concurrency_benchmark.py
rm benchmarks/ipc_v2_vs_v3_benchmark.py
rm benchmarks/ipc_v3_detailed_bench.py
rm benchmarks/memory_benchmark.py
rm benchmarks/memory_vs_ipc_v3_bench.py
rm benchmarks/wire_preencoding_benchmark.py
```

- [ ] **Step 5: Update mixed benchmarks**

For `benchmarks/rpc_v2_vs_v1_benchmark.py`: either delete the v1 comparison code (keeping only v2 benchmark) or delete the entire file (v2 performance is validated by `thread_vs_ipc_benchmark.py`). Decision: rename to `benchmarks/transport_benchmark.py` and remove v1 paths.

For `benchmarks/chunked_benchmark.py`: update any `rpc_v2` imports to `transport`.

- [ ] **Step 6: Update `pyproject.toml` — remove v1 dependencies**

Remove `pyzmq` and `watchdog` from `[project.dependencies]`:

```toml
# DELETE these lines from dependencies:
# "pyzmq>=26.0",
# "watchdog>=4.0",
```

Run `uv sync` to verify the lock file updates cleanly.

- [ ] **Step 7: Update `tests/README.md`**

Update the test README to reflect the new structure: no more v1 protocols, transport/ is the sole RPC layer.

- [ ] **Step 8: Run full test suite**

```bash
uv sync && uv run pytest tests/ -q --timeout=30
```

Expected: All tests pass. Dependency count reduced.

- [ ] **Step 9: Final commit**

```bash
git add -A
git commit -m "refactor: clean examples, benchmarks, dependencies

Phase 4b — final cleanup:
- Delete v1-only examples (5 files) and grid/ directory
- Rename v2 examples (v2_server → server, etc.)
- Delete v1-only benchmarks (7 files)
- Remove pyzmq and watchdog dependencies
- Update test README

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Self-Review

### 1. Spec Coverage

| Spec Requirement | Task |
|-----------------|------|
| Create `transport/` skeleton | Task 1 |
| Shared infra (frame, buddy, msg_type) | Task 1 |
| Re-export shims for backward compat | Task 2 |
| Move rpc_v2 files to transport/ subpackages | Task 3 |
| Update all internal imports | Task 4 |
| Write transport/__init__.py + rpc_v2 shim | Task 5 |
| Move transferable.py to crm/ | Task 6 |
| Rewrite conftest.py hello_server | Task 7 |
| Rewrite compo/ to use SOTA API | Task 8 |
| Update test imports | Task 9 |
| Rewrite test server setup (_start → ServerV2) | Task 10 |
| Delete rpc/ + rpc_v2/ + v1 tests | Task 11 |
| Clean examples, benchmarks, deps | Task 12 |
| Remove pyzmq, watchdog | Task 12 Step 6 |
| Remove memory://, tcp://, ipc-v2:// | Task 7 (conftest), Task 11 |
| Rename v2 examples | Task 12 Step 2 |

**Coverage: 100%** — all spec requirements have corresponding tasks.

### 2. Placeholder Scan

- No "TBD", "TODO", "implement later" found
- All code blocks contain complete implementations
- All commands include expected output
- ✅ Clean

### 3. Type Consistency

- `ServerV2` — consistent across all tasks
- `SharedClient` — consistent
- `IPCConfig` — sourced from `transport.ipc.frame` everywhere
- `ConcurrencyConfig` — sourced from `transport.server.scheduler`
- `_transport_connect` / `_transport_close` — used in compo rewrite (Task 8), matches registry API
- `_start_server` helper — defined in Task 10 Step 1, used in Step 2
- ✅ Clean

### 4. Risk Notes

- **Envelope omission in spec**: The spec's target structure didn't list `transport/ipc/envelope.py`, but `wire_v1.py` requires `Envelope` and `CompletionType`. Task 1 includes it.
- **`rpc/ipc/ipc_protocol.py` shim strategy**: Rather than full replacement (which would break AdaptiveBuffer-dependent v1 code), Task 2 adds `from transport.ipc.frame import *` at the top — both old and new code works.
- **`compo/` breaking change**: Adding `crm_name` parameter is backward-compatible (defaults to `'default'`). Removing `Client` return type from `connect_crm` overloads IS breaking — but the old API (`connect_crm(address)` without icrm_class returning raw Client) has no users outside tests.
- **Test count reduction**: Expected. Fewer protocol parameterizations (7→1 in conftest) + 6 deleted test files.

---

## Execution Summary

| Phase | Tasks | Commits | Key Risk |
|-------|-------|---------|----------|
| 1 | 1-2 | 1 | None — pure additions |
| 2 | 3-5 | 1 | Import errors in 11 moved files |
| 3 | 6-8 | 1 | compo/ rewrite may break component tests |
| 4 | 9-12 | 2 | Missing import updates; test setup rewrites |

**Total: 12 tasks, 5 atomic commits, ~44 files deleted, ~7600 LOC removed**
