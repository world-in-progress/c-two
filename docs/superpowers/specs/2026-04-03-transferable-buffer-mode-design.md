# Transferable Buffer Mode Design

## Problem

The server dispatch path in `native.py` materializes every SHM request to `bytes` before deserialization:

```python
mv = memoryview(request_buf)
payload = bytes(mv)       # ← unnecessary copy
mv.release()
```

This copy is redundant for `pickle.loads()` (which accepts `memoryview` natively) and blocks zero-deserialization use cases (e.g., ORM views over SHM).

The client response path in `com_to_crm` already passes `memoryview` but unconditionally releases the SHM buffer before returning, preventing hold-style zero-deserialization on the response side.

## Design

### Buffer Modes

Introduce a `buffer` parameter on `@cc.transferable` that declares the deserializer's relationship with the input buffer:

| Mode | SHM Lifetime | Use Case |
|------|-------------|----------|
| `'copy'` (default) | Shortest — freed before `deserialize` | Legacy / safe custom serializers |
| `'view'` | Medium — freed after `deserialize` returns | `pickle.loads`, Arrow, Protobuf |
| `'hold'` | Longest — freed by Python GC (RAII) | Zero-deserialization ORM views |

### API

```python
@cc.transferable                         # buffer='copy' (default)
class SafeData:
    value: int
    def serialize(data: 'SafeData') -> bytes: ...
    def deserialize(data: bytes) -> 'SafeData': ...

@cc.transferable(buffer='view')
class ArrowBatch:
    def serialize(batch: 'ArrowBatch') -> bytes: ...
    def deserialize(data: memoryview) -> 'ArrowBatch': ...

@cc.transferable(buffer='hold')
class FastDBView:
    def serialize(view: 'FastDBView') -> bytes: ...
    def deserialize(data: memoryview) -> 'FastDBView':
        # Returned object holds `data` (memoryview into SHM).
        # SHM stays alive via Python reference counting.
        ...
```

The internal default pickle transferable (created by `create_default_transferable()`) uses `'view'` mode — `pickle.loads(memoryview)` always creates independent objects.

### Execution Flow

#### Request Direction (`crm_to_com`, server-side)

```
copy:  bytes(mv) → release(mv, buf) → deserialize(bytes_copy) → CRM execute
view:  deserialize(mv) → release(mv, buf) → CRM execute
hold:  deserialize(mv) → CRM execute → [RAII: mv, buf freed when locals go out of scope]
```

For `hold` mode, the framework does NOT explicitly release. Python's reference counting frees the SHM when the deserialized args (and any memoryview references they hold) go out of scope at the end of `crm_to_com`.

#### Response Direction (`com_to_crm`, client-side)

```
copy:  bytes(mv) → release(mv, response) → deserialize(bytes_copy) → return result
view:  deserialize(mv) → release(mv, response) → return result
hold:  deserialize(mv) → return result → [RAII: mv, response freed when result is GC'd]
```

For `hold` mode, the deserialized result holds a reference to the memoryview (or sub-views), which holds a reference to the response `PyShmBuffer` via Python's buffer protocol. The SHM block stays alive as long as the result object exists. When the user's code discards the result, Python's reference counting chain frees everything:

```
result GC'd → memoryview GC'd → PyShmBuffer exports drops → Rust Drop frees SHM block
```

This follows the industry-standard RAII pattern used by iceoryx2 (`Sample<T>`), Cap'n Proto (`Response<T>::Reader`), and Arrow (`RecordBatch` with `Buffer.parent`).

### Dispatch Table Changes

The `_CRMSlot._dispatch_table` expands from `dict[str, tuple[method, MethodAccess]]` to include the input buffer mode:

```python
_dispatch_table: dict[str, tuple[method, MethodAccess, str]]
#                                                       ^^^ 'copy' | 'view' | 'hold'
```

The buffer mode is determined at wrapping time (in `auto_transfer`) and attached to the `transfer_wrapper` function as `_input_buffer_mode`. `build_dispatch_table` reads this attribute.

### Release Function Mechanism

For `copy` and `view` modes, the release function is passed to `crm_to_com` / `com_to_crm` as a `_release_fn` keyword argument. This allows the transferable layer to release at the precise moment (after materialization for `copy`, after deserialization for `view`).

For `hold` mode, no `_release_fn` is passed — RAII handles lifetime.

## Implementation

### File Changes

#### 1. `src/c_two/crm/transferable.py`

**`transferable()` decorator** — Accept optional `buffer` parameter:
```python
def transferable(cls=None, *, buffer: str = 'copy'):
    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls._buffer_mode = buffer
        ...
        return new_cls
    if cls is not None:
        return wrap(cls)
    return wrap
```

**`Transferable` base class** — Add `_buffer_mode = 'copy'` class attribute.

**`create_default_transferable()`** — Set `_buffer_mode = 'view'` on generated classes (pickle is memoryview-safe).

**`crm_to_com()`** — Accept `_release_fn=None` kwarg. Based on `input._buffer_mode`:
- `copy`: `request = bytes(request)` → `_release_fn()` → `deserialize(request)`
- `view`: `deserialize(request)` → `_release_fn()`
- `hold`: `deserialize(request)` → (no release, RAII)

**`com_to_crm()`** — Determine output buffer mode from `output._buffer_mode`. For SHM responses:
- `copy`: `bytes(mv)` → release → `deserialize(bytes_copy)`
- `view`: `deserialize(mv)` → release (current behavior, already correct)
- `hold`: `deserialize(mv)` → return result (no release)

**`transfer_wrapper()`** — Forward `**kwargs` (for `_release_fn`) to `crm_to_com`.

Attach `_input_buffer_mode` and `_output_buffer_mode` as function attributes on `transfer_wrapper` for dispatch table introspection.

#### 2. `src/c_two/transport/server/native.py`

**`_CRMSlot`** — Expand dispatch table tuple to include input buffer mode.

**`build_dispatch_table()`** — Read `_input_buffer_mode` from method.

**`_make_dispatch_closure()`** — Based on input buffer mode:
- `copy`/`view`: Create `_release_fn` closure, pass `memoryview` + `_release_fn` to method.
- `hold`: Pass `memoryview` only, do NOT release in dispatch; let RAII handle.

#### 3. Tests

- Unit test: Verify `_buffer_mode` attribute set correctly for all three modes.
- Unit test: Verify default transferable has `_buffer_mode = 'view'`.
- Integration test: Verify `view` mode eliminates copy (benchmark comparison).
- Integration test: Verify `hold` mode keeps SHM alive during CRM execution.
- Integration test: Verify `hold` mode SHM is freed after result GC (response direction).

### Unified Buffer Type Across All Sources

Rust wraps all request data into a single Python type (`PyShmBuffer`) regardless of origin:

| Source | `RequestData` Variant | `.release()` Behavior |
|--------|----------------------|----------------------|
| Inline (small, in UDS frame) | `Inline(Vec<u8>)` | No-op |
| Buddy SHM | `Shm{is_dedicated: false}` | `free_at()` on peer pool |
| Dedicated SHM | `Shm{is_dedicated: true}` | `free_at()` on peer pool |
| Chunked (reassembled) | `Handle{handle, pool}` | `release_handle()` on reassembly pool |

All four support `memoryview()` and `.release()`. Buffer mode logic applies uniformly — no source-specific branching needed. For inline data, `release()` is a no-op, making all three modes behaviorally identical.

### Safety Invariants

1. `copy` mode: SHM freed before user code touches the data. Unconditionally safe.
2. `view` mode: SHM freed after `deserialize` returns. Safe if deserializer creates independent objects (pickle, Arrow, Protobuf all do).
3. `hold` mode: SHM freed by GC. User's responsibility to not leak references. `PyShmBuffer`'s Rust `Drop` is the ultimate safety net.
4. For non-SHM transports (thread-local, inline), `_release_fn` is `None` — all modes behave identically (no SHM involved).
