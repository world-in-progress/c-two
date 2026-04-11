# from_buffer Dual-Function & SHM Residence Monitoring

**Addendum to**: `2026-07-15-transferable-redesign-design.md`

## Problem

The transferable redesign (§5) introduced `buffer='hold'` on `@cc.transfer` to keep SHM alive through CRM method execution. However, two problems remain:

### 1. Deserialize Cannot Serve Both Modes

A `deserialize(data: bytes) → T` function either returns an independent copy (safe for view mode) or a zero-copy view backed by the buffer (required for hold mode). It cannot do both:

- **View mode**: `deserialize` must return an *independent* object. After `_release_fn()` frees SHM, the object remains valid.
- **Hold mode**: The whole point is zero-copy — `deserialize` should return a buffer-backed view (e.g., `numpy.frombuffer`). But if the same type is used in view mode elsewhere, the returned view becomes a dangling reference after SHM release.

The current design requires users to mentally track which mode a method uses and write `deserialize` accordingly. This is error-prone and prevents a single transferable type from being used in both modes.

### 2. No Server-Side SHM Leak Detection

In hold mode, SHM stays alive as long as the CRM holds a reference (via `numpy → memoryview → PyShmBuffer` chain). If a CRM accidentally stores these views in persistent state (e.g., `self.cache[key] = view`), SHM is pinned indefinitely — a silent memory leak. There is currently no mechanism to detect or warn about this.

## Design

### Part 1: `from_buffer` Dual-Function

#### Overview

Each transferable type may provide two deserialization functions:

| Function | Returns | Use Case |
|----------|---------|----------|
| `deserialize(data) → T` | Independent copy | View mode (default), output deserialization |
| `from_buffer(data) → T` | Zero-copy view backed by `data` | Hold mode (server-side input) |

Both are optional but at least one must exist for a type used in deserialization contexts.

#### Transferable Definition

```python
@cc.transferable
class GridData:
    values: np.ndarray

    def serialize(data: 'GridData') -> bytes:
        return data.values.tobytes()

    def deserialize(data: bytes) -> 'GridData':
        # Returns independent copy — safe after buffer release
        return GridData(values=np.frombuffer(data, dtype=np.float64).copy())

    def from_buffer(data: bytes) -> 'GridData':
        # Returns zero-copy view — buffer must stay alive
        return GridData(values=np.frombuffer(data, dtype=np.float64))
```

#### `TransferableMeta` Changes

The metaclass auto-converts `from_buffer` to `@staticmethod`, same as `serialize`/`deserialize`:

```python
class TransferableMeta(ABCMeta):
    def __new__(mcs, name, bases, attrs, **kwargs):
        for method_name in ('serialize', 'deserialize', 'from_buffer'):
            if method_name in attrs and not isinstance(attrs[method_name], staticmethod):
                attrs[method_name] = staticmethod(attrs[method_name])

        cls = super().__new__(mcs, name, bases, attrs, **kwargs)

        # Registration requires serialize + deserialize (mandatory pair).
        # from_buffer is optional and additive — it does NOT replace deserialize.
        # deserialize is always needed for: view mode, client output, fallback paths.
        if name != 'Transferable':
            has_serialize = hasattr(cls, 'serialize')
            has_deserialize = hasattr(cls, 'deserialize')

            if has_serialize and has_deserialize:
                if not is_dataclass(cls):
                    cls = dataclass(cls)
                if cls.__module__ != 'Default':
                    register_transferable(cls)

        return cls
```

#### Server-Side Auto-Detection

The server automatically selects the buffer mode based on the transferable's capabilities:

```
                 ┌─────────────────────┐
                 │ Input transferable   │
                 │ has from_buffer()?   │
                 └──────┬──────────────┘
                        │
               ┌────────┴────────┐
               │yes              │no
               ▼                 ▼
     ┌─────────────────┐  ┌─────────────────┐
     │ @cc.transfer     │  │ Always view mode │
     │ buffer='view'?   │  │ use deserialize  │
     └───────┬─────────┘  └─────────────────┘
             │
     ┌───────┴────────┐
     │yes (explicit)  │no (default)
     ▼                ▼
  ┌────────────┐  ┌────────────────┐
  │ View mode  │  │ Hold mode      │
  │ deserialize│  │ from_buffer    │
  └────────────┘  └────────────────┘
```

Decision table:

| `from_buffer` exists | `@cc.transfer(buffer=...)` | Effective mode | Function used |
|---------------------|---------------------------|----------------|---------------|
| ✅ | not specified | **hold** (auto) | `from_buffer` |
| ✅ | `buffer='view'` | view (override) | `deserialize` |
| ✅ | `buffer='hold'` | hold (explicit) | `from_buffer` |
| ❌ | not specified | view (default) | `deserialize` |
| ❌ | `buffer='view'` | view (explicit) | `deserialize` |
| ❌ | `buffer='hold'` | **error** | — |

The last row is a configuration error caught at ICRM decoration time — requesting hold mode without providing `from_buffer`.

> **⚠️ Design trade-off: auto-detection scope.** Adding `from_buffer` to a transferable type changes the default buffer mode for ALL server methods using that type as input. This is intentional — providing `from_buffer` is a declaration of zero-copy intent. However, if a type is used across methods with mixed requirements, specific methods can opt out via `@cc.transfer(buffer='view')`. Users should be aware that adding `from_buffer` has global effect. The framework logs at DEBUG level when auto-detection activates hold mode.

#### Usage-Site Capability Validation

The framework validates at ICRM decoration time that each method's effective buffer mode is compatible with its input transferable:

| Effective mode | Requires on input transferable |
|---|---|
| `view` | `deserialize` (always present — required for registration) |
| `hold` | `from_buffer` |

For output (client-side): normal client path always uses `deserialize`; `cc.hold()` prefers `from_buffer` if available, falls back to `deserialize`.

#### Implementation in `auto_transfer`

`auto_transfer` gains `from_buffer`-aware buffer mode resolution:

```python
def auto_transfer(func=None, *, input=None, output=None, buffer=None):
    # buffer=None means "auto-detect" (new default)
    # buffer='view'/'hold' means explicit override

    def create_wrapper(func):
        input_transferable = _resolve_input(func, input)
        output_transferable = _resolve_output(func, output)

        # Auto-detect buffer mode from input transferable capabilities
        effective_buffer = buffer
        if effective_buffer is None:
            has_from_buffer = (
                input_transferable is not None
                and hasattr(input_transferable, 'from_buffer')
            )
            effective_buffer = 'hold' if has_from_buffer else 'view'

        # Validate: hold requires from_buffer
        if effective_buffer == 'hold' and input_transferable is not None:
            if not hasattr(input_transferable, 'from_buffer'):
                raise TypeError(
                    f"buffer='hold' on {func.__name__} requires "
                    f"{input_transferable.__name__} to implement from_buffer()"
                )

        return _build_transfer_wrapper(
            func,
            input=input_transferable,
            output=output_transferable,
            buffer=effective_buffer,
        )

    # ... existing func/None dispatch ...
```

#### Implementation in `_build_transfer_wrapper`

`crm_to_com` (server-side) selects the deserialization function based on buffer mode:

```python
def crm_to_com(*args, _release_fn=None):
    # Choose deserializer based on buffer mode
    if input_buffer_mode == 'hold':
        input_fn = getattr(input, 'from_buffer', None) or input.deserialize
    else:
        input_fn = input.deserialize if input else None

    # ... rest unchanged — the dispatch in native.py already handles
    # view vs hold buffer lifecycle correctly ...
```

`com_to_crm` (client-side) also supports `from_buffer` for `cc.hold()` responses:

```python
def com_to_crm(*args, _c2_buffer=None):
    # Choose output deserializer
    if output:
        if _c2_buffer == 'hold' and hasattr(output, 'from_buffer'):
            output_fn = output.from_buffer
        else:
            output_fn = output.deserialize
    else:
        output_fn = None

    # ... rest unchanged ...
```

#### `@cc.transfer` Changes

The `buffer` parameter default changes from `'view'` to `None` (auto-detect):

```python
def transfer(*, input=None, output=None, buffer=None):
    # buffer=None → auto-detect from input transferable's from_buffer
    # buffer='view' → force view (explicit override)
    # buffer='hold' → force hold (explicit, validated)
    ...
```

#### Correctness Argument

In `crm_to_com` (transferable.py L457-461), view mode calls `_release_fn()` AFTER deserialization but BEFORE `crm_method()`. This means:

- `from_buffer` in view mode → returns buffer-backed view → buffer released → CRM gets **dangling reference** → **BUG**
- `deserialize` in view mode → returns independent copy → safe to release buffer → **CORRECT**
- `from_buffer` in hold mode → returns buffer-backed view → buffer NOT released → CRM uses live data → **CORRECT**

Auto-detection ensures `from_buffer` is only used in hold mode. The `buffer='view'` override forces `deserialize` even when `from_buffer` exists, preventing the dangling reference scenario.

#### Exception Safety in Hold Mode

If `from_buffer` raises during hold-mode dispatch, SHM is still freed correctly:
- `mv` and `request_buf` are local variables in the dispatch closure
- When the exception propagates, both fall out of scope → Python GC collects them
- `PyShmBuffer.Drop` auto-frees the SHM allocation (shm_buffer.rs L243-261)
- The `finally` block in dispatch still registers tracking (for leak detection even in error paths)

This must be covered by a regression test.

### Part 2: SHM Residence Monitoring

#### Problem

In hold mode, the SHM buffer chain is:
```
numpy_array.base → memoryview → PyShmBuffer → SHM segment
```

When the CRM drops all references, Python GC unwinds the chain and `PyShmBuffer.Drop` auto-frees SHM. But if the CRM stores references in persistent state (e.g., `self.state[key] = array`), SHM is pinned indefinitely — a silent leak that exhausts the shared memory pool.

#### Design: Server-Level Registry with Weakref Tracking

##### Rust Change: Add `weakref` Support to PyShmBuffer

```rust
// shm_buffer.rs — single attribute addition
#[pyclass(name = "ShmBuffer", frozen, weakref)]
pub struct PyShmBuffer {
    inner: Mutex<Option<ShmBufferInner>>,
    data_len: usize,
    exports: AtomicU32,
}
```

This adds a weak reference list pointer to the Python type object. No behavioral change — just enables `weakref.ref(shm_buf)` from Python. The overhead is one pointer per instance (negligible for SHM-sized buffers).

##### Rust Change: Expose `exports` Property

```rust
#[pymethods]
impl PyShmBuffer {
    /// Number of active buffer protocol exports (memoryviews).
    #[getter]
    fn exports(&self) -> u32 {
        self.exports.load(Ordering::Acquire)
    }
}
```

##### Python: `HoldEntry` and `HoldRegistry`

```python
import weakref
import time
import logging

logger = logging.getLogger(__name__)

class HoldEntry:
    """Tracks a single hold-mode SHM buffer."""
    __slots__ = ('ref', 'created_at', 'method_name', 'data_len', 'route_name')

    def __init__(self, buf_ref, method_name, data_len, route_name):
        self.ref = buf_ref          # weakref.ref to PyShmBuffer
        self.created_at = time.monotonic()
        self.method_name = method_name
        self.data_len = data_len
        self.route_name = route_name


class HoldRegistry:
    """Server-level registry tracking hold-mode SHM buffers.

    Uses weakrefs for automatic cleanup when CRM drops views.
    Provides lazy sweep for stale hold detection.
    """

    def __init__(self, warn_threshold: float = 60.0):
        self._entries: dict[int, HoldEntry] = {}
        self._counter: int = 0
        self._warn_threshold = warn_threshold
        self._lock = threading.Lock()

    def track(self, request_buf, method_name: str, route_name: str) -> None:
        """Register a hold-mode buffer for tracking."""
        with self._lock:
            self._counter += 1
            entry_id = self._counter

            def on_gc(_ref, _eid=entry_id):
                with self._lock:
                    self._entries.pop(_eid, None)

            ref = weakref.ref(request_buf, on_gc)
            self._entries[entry_id] = HoldEntry(
                buf_ref=ref,
                method_name=method_name,
                data_len=len(request_buf),
                route_name=route_name,
            )

    def sweep(self) -> list[HoldEntry]:
        """Check for stale holds. Returns list of stale entries (for logging)."""
        now = time.monotonic()
        stale = []
        dead = []
        with self._lock:
            for eid, entry in self._entries.items():
                buf = entry.ref()
                if buf is None:
                    dead.append(eid)
                elif now - entry.created_at > self._warn_threshold:
                    stale.append(entry)
            for eid in dead:
                del self._entries[eid]
        return stale

    def stats(self) -> dict:
        """Return hold tracking statistics."""
        active = 0
        total_bytes = 0
        oldest_age = 0.0
        now = time.monotonic()
        with self._lock:
            for entry in self._entries.values():
                if entry.ref() is not None:
                    active += 1
                    total_bytes += entry.data_len
                    age = now - entry.created_at
                    if age > oldest_age:
                        oldest_age = age
        return {
            'active_holds': active,
            'total_held_bytes': total_bytes,
            'oldest_hold_seconds': round(oldest_age, 2),
        }
```

##### Integration with `NativeServerBridge`

```python
class NativeServerBridge:
    def __init__(self, ...):
        # ... existing init ...
        threshold = float(os.environ.get('C2_HOLD_WARN_SECONDS', '60'))
        self._hold_registry = HoldRegistry(warn_threshold=threshold)
        self._hold_sweep_interval = 10  # sweep every N hold-mode dispatches
        self._hold_dispatch_count = 0
```

In `_make_dispatcher`, the hold path gains tracking:

```python
def dispatch(...):
    # ... resolve method ...

    if buffer_mode == 'view':
        # ... existing view logic (unchanged) ...
    else:  # hold
        mv = memoryview(request_buf)
        try:
            result = method(mv)
        finally:
            # Track in finally: if CRM stored a view and then raised,
            # SHM is still pinned — we must track it for leak detection.
            # Skip inline buffers (no SHM to leak).
            if not request_buf.is_inline:
                hold_registry.track(request_buf, method_name, route_name)

        # Lazy sweep: check for stale holds periodically
        nonlocal hold_dispatch_count
        hold_dispatch_count += 1
        if hold_dispatch_count % hold_sweep_interval == 0:
            for stale in hold_registry.sweep():
                age = time.monotonic() - stale.created_at
                logger.warning(
                    "Hold-mode SHM buffer pinned for %.1fs — "
                    "route=%s method=%s size=%d bytes. "
                    "CRM may be storing buffer-backed views in persistent state.",
                    age, stale.route_name, stale.method_name, stale.data_len,
                )

    # ... unpack result, respond ...
```

##### Public API: `cc.hold_stats()`

Exposed via the registry module:

```python
# In registry.py
def hold_stats() -> dict:
    """Return hold-mode SHM tracking statistics.

    Returns dict with:
    - active_holds: number of currently held SHM buffers
    - total_held_bytes: total bytes pinned in SHM
    - oldest_hold_seconds: age of oldest active hold
    """
    if _server is None:
        return {'active_holds': 0, 'total_held_bytes': 0, 'oldest_hold_seconds': 0}
    return _server._hold_registry.stats()
```

##### Lifecycle & Conflict Analysis

| Existing Mechanism | Interaction with HoldRegistry | Conflict? |
|---|---|---|
| `PyShmBuffer.Drop` | Weakref callback fires BEFORE Drop. Entry removed, then SHM freed. | ✅ No conflict |
| `gc_buddy()` / `gc_dedicated()` | Pool-level GC. Held buffers keep segments non-idle (exports > 0 → alloc_count > 0). GC skips them. | ✅ No conflict |
| `ChunkRegistry gc_sweep` (5s Rust timer) | Tracks in-flight reassembly, not dispatched requests. Completely orthogonal. | ✅ No conflict |
| `HeldResult.__del__` | Client-side only. HoldRegistry is server-side. | ✅ No conflict |
| `Server response pool` | Response pool is for outbound data. HoldRegistry tracks inbound request buffers. | ✅ No conflict |
| `cleanup_connection()` | Cleans up per-connection assemblers on disconnect. Does not touch dispatched buffers. | ✅ No conflict |

##### Why NOT a Python Timer Thread

The Python side currently has zero periodic timers — all timing is delegated to Rust. Introducing a `threading.Timer` would:
1. Add a background thread that complicates shutdown ordering
2. Risk `ResourceWarning` during interpreter teardown
3. Be unnecessary — hold-mode dispatches are the natural sweep trigger

Lazy sweep (every N hold dispatches) is simpler and sufficient. For servers with very infrequent hold calls, `cc.hold_stats()` provides on-demand inspection.

##### Configuration

| Env Variable | Default | Description |
|---|---|---|
| `C2_HOLD_WARN_SECONDS` | `60` | Warn threshold for stale holds |

### Part 3: End-to-End Data Flow

#### Server-Side Hold Mode (Input)

```
Client sends request → Rust server receives frame
    → SHM-backed: PyShmBuffer.from_peer_shm(pool, seg, offset, size)
    → Inline: PyShmBuffer.from_inline(data)

Python dispatch (hold path):
    1. mv = memoryview(request_buf)           # PyShmBuffer.__getbuffer__: exports += 1
    2. try:
           result = crm_method(mv, _release_fn=None)
           └─ crm_to_com calls from_buffer(mv):
              └─ e.g. np.frombuffer(mv, dtype=np.float64)
              └─ numpy.base → mv → PyShmBuffer   # reference chain keeps SHM alive
           3. CRM method executes with zero-copy view
       finally:
           4. hold_registry.track(request_buf, ...)  # weakref-based tracking
              (skipped for inline buffers — no SHM to leak)
    5. Return result (serialized output)

CRM drops numpy view later:
    numpy GC → mv.__del__ → PyShmBuffer.__releasebuffer__: exports -= 1
    → PyShmBuffer.__del__ (if refcount=0) → Drop → SHM freed
    → Weakref callback → entry removed from HoldRegistry
```

#### Server-Side View Mode (Input, Default)

```
Python dispatch (view path):
    1. mv = memoryview(request_buf)
    2. result = crm_method(mv, _release_fn=release_fn)
       └─ crm_to_com calls deserialize(mv):
          └─ e.g. np.frombuffer(mv).copy()  # independent copy
       └─ release_fn() called:
          └─ mv.release() → PyShmBuffer.__releasebuffer__: exports -= 1
          └─ request_buf.release() → SHM freed immediately
    3. CRM method executes with independent copy
    4. No hold tracking needed
```

#### Client-Side Hold Mode (Output)

```
Client calls cc.hold(proxy.method)(args):
    1. _c2_buffer='hold' injected into kwargs
    2. com_to_crm receives response (PyShmBuffer)
    3. mv = memoryview(response)
    4. result = output.from_buffer(mv)     # zero-copy if from_buffer exists
                                           # else output.deserialize(mv)
    5. release_cb:                          # try/finally ensures response freed
           try: mv.release()
           finally: response.release()     # safe: exports=0 after mv.release()
    6. return HeldResult(result, release_cb)

User code:
    with cc.hold(grid.compute)(data) as held:
        process(held.value)  # SHM alive
    # __exit__ → release_cb() → SHM freed
```

## Backward Compatibility

1. **No `from_buffer`**: If a transferable only has `deserialize`, behavior is identical to current. Auto-detection defaults to `view`.
2. **Explicit `buffer='hold'`**: The existing `@cc.transfer(buffer='hold')` works as before but now uses `from_buffer` when available.
3. **`buffer` default change**: `@cc.transfer(buffer=...)` default changes from `'view'` to `None` (auto-detect). Functionally equivalent for types without `from_buffer`.
4. **Default transferables (pickle)**: `DynamicInputTransferable` and `DynamicOutputTransferable` do NOT get `from_buffer` — they always use `deserialize` (pickle) in view mode.

## Testing Strategy

### Unit Tests

1. **`TransferableMeta` from_buffer conversion**: Verify `from_buffer` is auto-converted to `@staticmethod`
2. **Auto-detection logic**: Test all 6 rows of the decision table
3. **Validation**: `buffer='hold'` without `from_buffer` raises `TypeError` at decoration time
4. **`HoldRegistry` lifecycle**: Track, sweep, auto-cleanup via weakref callback
5. **`HoldRegistry` stats**: Accurate counts after track/release cycles
6. **`HoldRegistry` thread safety**: Concurrent track/sweep from multiple threads

### Integration Tests

7. **IPC hold mode with `from_buffer`**: Server auto-detects hold, uses `from_buffer`, client receives result
8. **IPC view override**: `@cc.transfer(buffer='view')` forces `deserialize` even with `from_buffer` available
9. **Stale hold warning**: CRM stores view → sweep detects → warning logged
10. **`cc.hold_stats()`**: Returns accurate metrics during active holds

