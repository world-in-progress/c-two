# Transferable Redesign

## Problem

The current `@cc.transferable` mechanism has two fundamental design issues:

### 1. Type Conflation

`@cc.transferable` conflates two orthogonal concepts:

- **Domain types** — data structures with meaningful schemas (e.g., `GridSchema`, `GridAttribute`)
- **Parameter bundles** — ad-hoc wrappers that group a method's parameters for wire serialization (e.g., `GridInfo`, `PeerGridInfos`, `GridKeys`)

In the grid example, users must write 7 transferable classes for one ICRM — but only 2 are genuine domain types. The other 5 exist solely to bundle method parameters. This creates proliferation, confusion, and fragile implicit matching via `auto_transfer`'s Priority 2 field-name comparison logic (lines 464–483 of `transferable.py`).

### 2. Buffer Lifecycle Misplacement

The `_buffer_mode` attribute lives on the `Transferable` class (line 89 of `transferable.py`), coupling SHM lifetime policy to the data type. In practice:

- **Input buffer** is a CRM-side concern — the server controls when SHM backing a request is released.
- **Output buffer** is a client-side concern — the caller decides how long to hold a response in SHM.

These are independent decisions that should not be conflated on a single type.

## Design

### Overview

The redesign separates three orthogonal concerns:

| Concern | Where | API |
|---------|-------|-----|
| Domain type codec | Type definition | `@cc.transferable` (unchanged) |
| Parameter bundling | Framework auto-generates | Implicit — zero annotation |
| Input buffer (CRM-side) | ICRM method declaration | `@cc.transfer(buffer='hold')` |
| Output buffer (Client-side) | Call site | `cc.hold(proxy.method)(args)` → `HeldResult[R]` |

### 1. Transferable Separation

`@cc.transferable` is exclusively for **domain types** with custom serialization logic. Users never write parameter bundles.

```python
# ✅ Domain type — user writes this
@cc.transferable
class GridSchema:
    epsg: int
    bounds: list[float]
    first_size: list[float]
    subdivide_rules: list[list[int]]

    def serialize(schema: 'GridSchema') -> bytes: ...
    def deserialize(data: bytes) -> 'GridSchema': ...

# ✅ Domain type — user writes this
@cc.transferable
class GridAttribute:
    level: int
    type: int
    activate: bool
    # ... fields ...

    def serialize(data: 'GridAttribute') -> bytes: ...
    def deserialize(data: bytes) -> 'GridAttribute': ...

# ❌ DELETED — framework auto-generates these
# class GridInfo: ...        (was: bundle for level + global_id)
# class PeerGridInfos: ...   (was: bundle for level + global_ids)
# class GridInfos: ...       (was: bundle for levels + global_ids)
# class GridAttributes: ...  (was: bundle for list[GridAttribute])
# class GridKeys: ...        (was: bundle for list[str | None])
```

The grid example drops from 7 user-written transferables to 2.

### 2. ICRM Methods — Zero Annotation Default

ICRM methods require no annotation by default. The framework auto-generates parameter bundles using pickle:

```python
@cc.icrm(namespace='icrm', version='0.1.0')
class IGrid:
    # No annotation — framework auto-bundles (levels, global_ids) via pickle
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    # Single param is a registered @transferable → matched automatically
    def get_schema(self) -> GridSchema:
        ...

    # Multiple params → auto-bundle via pickle; list[GridAttribute] also pickle (not a direct transferable)
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        ...
```

**Matching rules** (simplified `auto_transfer`):

1. **Single-param direct match**: If a method has exactly one non-self parameter whose type is a registered `@cc.transferable`, use it directly.
2. **Return type direct match**: If the return type **itself** (not a container of it) is a registered `@cc.transferable`, use it directly. `list[GridAttribute]` does **not** match — it falls back to pickle.
3. **Fallback**: Generate `DynamicInputTransferable` / `DynamicOutputTransferable` using pickle.

Container types like `list[T]`, `dict[K, V]`, `tuple[...]` are never auto-lifted to a domain transferable. If you need custom serialization for `list[GridAttribute]`, use `@cc.transfer(output=GridAttributes)` explicitly.

Priority 2 field-name matching (lines 464–483) is **deleted**. The fragile name+type comparison across modules is replaced by the explicit `@cc.transfer` override (see below).

### 3. Explicit Method-Level Override: `@cc.transfer`

When the auto-bundle default is insufficient, users can provide explicit transferables per method:

```python
@cc.icrm(namespace='icrm', version='0.1.0')
class IGrid:
    # Explicit input+output transferable override
    @cc.transfer(input=GridInfos, output=GridKeys)
    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    # Explicit output only — input uses auto-bundle
    @cc.transfer(output=GridAttributes)
    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        ...
```

Note: parameter bundle classes like `GridInfos`, `GridKeys`, `GridAttributes` are **not required by default** — the framework auto-bundles via pickle. However, users may still author them for explicit method overrides when custom serialization (e.g., Arrow, fastdb) is desired.

`@cc.transfer` parameters:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `input` | `type[Transferable] \| None` | `None` | Custom input transferable. `None` = auto-bundle. |
| `output` | `type[Transferable] \| None` | `None` | Custom output transferable. `None` = auto-bundle. |
| `buffer` | `'view' \| 'hold'` | `'view'` | Input buffer mode (CRM-side). See §5. |

#### Composition with `@cc.icrm`

`@cc.transfer` is a **metadata-only** decorator. It does **not** wrap the function itself. Instead, it attaches a `__cc_transfer__` attribute:

```python
def transfer(*, input=None, output=None, buffer='view'):
    def decorator(func):
        func.__cc_transfer__ = {
            'input': input,
            'output': output,
            'buffer': buffer,
        }
        return func  # NO wrapping — just attach metadata
    return decorator
```

The `icrm()` decorator's method loop detects `__cc_transfer__` and passes it to `auto_transfer`:

```python
# Inside icrm() — meta.py
for name, value in cls.__dict__.items():
    if isfunction(value) and not getattr(value, _SHUTDOWN_ATTR, False):
        transfer_config = getattr(value, '__cc_transfer__', None)
        if transfer_config:
            # Explicit: use provided transferables, fill gaps with auto-bundle
            decorated_methods[name] = auto_transfer(value, **transfer_config)
        else:
            # Implicit: full auto-bundle
            decorated_methods[name] = auto_transfer(value)
```

`auto_transfer` gains optional keyword parameters `input`, `output`, `buffer` that override the matching logic when provided.

### 4. `auto_transfer` Simplification

The redesigned `auto_transfer` has a clean two-step flow:

```
Input:
  1. Single-param type is registered @transferable? → use it
  2. Otherwise → DynamicInputTransferable (pickle)

Output:
  1. Return type is registered @transferable? → use it
  2. Otherwise → DynamicOutputTransferable (pickle)
```

The deleted Priority 2 block (lines 464–483) relied on matching field names and types across module boundaries. This was fragile — two methods with slightly different parameter lists needed entirely separate transferable classes. The new design eliminates this by making `@cc.transfer` the explicit escape hatch.

### 5. Input Buffer Mode (CRM-Side)

Input buffer mode controls when the server releases SHM backing a request. This is a **CRM-side** concern declared on the ICRM method:

```python
@cc.icrm(namespace='icrm', version='0.1.0')
class IGrid:
    # Default: view — SHM released after deserialization
    def get_schema(self) -> GridSchema:
        ...

    # Hold: SHM stays alive — CRM deserializer returns a view into SHM
    @cc.transfer(buffer='hold')
    def compute(self, data: GridData) -> GridResult:
        ...
```

| Mode | SHM Lifetime | Use Case |
|------|-------------|----------|
| `'view'` (default) | Freed after `deserialize` returns | pickle, Arrow, Protobuf — all create independent objects |
| `'hold'` | Freed by Python GC (RAII) | Zero-deserialization ORM views (fastdb) |

**`copy` is removed as a user concept.** The previous `copy` mode (materialize to `bytes` before deserialize) provided no benefit over `view` for any known deserializer. Transport-level fallback (e.g., HTTP body is always bytes) happens automatically.

**Server dispatch** (in `native.py`):

```python
# view (default): pass memoryview, release after deserialize
mv = memoryview(request_buf)
def release_fn():
    mv.release()
    request_buf.release()
result = method(mv, _release_fn=release_fn)
release_fn()  # safe to release — deserialize returned independent objects

# hold: pass memoryview, SHM lifetime tied to Python object
mv = memoryview(request_buf)
result = method(mv)
# request_buf NOT released — Python GC handles it via RAII
```

### 6. Output Buffer Mode (Client-Side): `cc.hold()`

Output buffer mode controls when the client releases SHM backing a response. This is a **client-side** concern, expressed at the call site — not in the ICRM declaration.

**Default is `view`** — SHM is released immediately after deserialization. The caller gets an independent Python object.

For zero-copy scenarios, `cc.hold()` wraps a single method call to keep SHM alive:

```python
grid = cc.connect(IGrid, name='grid', address='ipc://server')

# Default: view — returns GridResult directly
result = grid.compute(data)

# Hold: returns HeldResult[GridResult] — SHM stays alive
result = cc.hold(grid.compute)(data)
process(result.value)    # .value is GridResult with full type hints
result.release()         # explicit SHM release

# Context manager (recommended)
with cc.hold(grid.compute)(data) as held:
    process(held.value)
# SHM auto-released on exit
```

#### `HeldResult[R]`

```python
from typing import Generic, TypeVar
import sys
import warnings

R = TypeVar('R')

class HeldResult(Generic[R]):
    """Wraps a method return value with explicit SHM lifecycle control.

    Three-layer safety net:
    1. Explicit .release() — preferred
    2. Context manager (__enter__/__exit__) — recommended
    3. __del__ fallback — last resort with warning

    **Alias warning**: After .release(), any previously obtained reference
    from .value may point to freed SHM and must not be accessed. Treat
    .value as borrowing — do not let references escape the hold scope.
    """

    __slots__ = ('_value', '_release_cb', '_released')

    def __init__(self, value: R, release_cb: object | None) -> None:
        self._value = value
        self._release_cb = release_cb  # callable that frees SHM resources
        self._released = False

    @property
    def value(self) -> R:
        """Access the return value. Raises after release."""
        if self._released:
            raise CCError("SHM released — value no longer accessible")
        return self._value

    def release(self) -> None:
        """Explicitly release the underlying SHM buffer.

        After this call, .value raises and any aliases to the
        returned data must not be accessed (they may reference freed SHM).
        """
        if not self._released:
            self._released = True
            cb = self._release_cb
            self._release_cb = None
            self._value = None
            if cb is not None:
                try:
                    cb()
                except Exception:
                    pass

    def __enter__(self) -> 'HeldResult[R]':
        return self

    def __exit__(self, *args) -> None:
        self.release()

    def __del__(self) -> None:
        if getattr(self, '_released', True):
            return
        # Guard against interpreter shutdown — globals may be torn down
        if sys.is_finalizing():
            self.release()
            return
        warnings.warn(
            "HeldResult was garbage-collected without release() — "
            "potential SHM leak. Use 'with cc.hold(...)' or call .release().",
            ResourceWarning,
            stacklevel=2,
        )
        self.release()
```

The `release_cb` is a closure that captures both the `memoryview` and the response buffer, ensuring correct release order. See §6.3 for how `com_to_crm` constructs it.

#### `cc.hold()` Implementation

```python
from typing import ParamSpec, TypeVar, Callable
from functools import wraps

P = ParamSpec('P')
R = TypeVar('R')

def hold(method: Callable[P, R]) -> Callable[P, HeldResult[R]]:
    """Wrap an ICRM bound method to hold SHM on the response.

    Only accepts bound methods on ICRM instances (i.e., proxy.some_method).
    Returns a callable with the same parameter signature.
    The return type changes from R to HeldResult[R].

    Intended usage: cc.hold(proxy.method)(args) — single-shot pattern.
    Storing the wrapper is allowed but discouraged.
    """
    # Validate: must be a bound method on an ICRM instance
    self_obj = getattr(method, '__self__', None)
    name = getattr(method, '__name__', None)
    if self_obj is None or name is None:
        raise TypeError(
            "cc.hold() requires a bound ICRM method, e.g. cc.hold(grid.compute)"
        )

    @wraps(method)
    def wrapper(*args: P.args, **kwargs: P.kwargs) -> HeldResult[R]:
        kwargs['_c2_buffer'] = 'hold'
        return getattr(self_obj, name)(*args, **kwargs)

    return wrapper
```

**Design choice — strong reference**: `cc.hold()` keeps a strong reference to the ICRM proxy (via `self_obj`), matching Python's standard bound-method semantics where `f = obj.method` also keeps `obj` alive. Since the intended pattern is single-shot `cc.hold(grid.compute)(data)`, the reference is ephemeral. If stored, it prevents proxy GC — same as storing any bound method.

#### `_c2_buffer` Propagation Path

The `_c2_buffer` kwarg must flow through the entire call chain. Currently `transfer_wrapper` drops kwargs on the `->` (client→CRM) path. The fix requires changes at two levels:

**1. `transfer_wrapper`** (in `transferable.py`): Pop `_c2_buffer` from kwargs and forward it to `com_to_crm`:

```python
@wraps(func)
def transfer_wrapper(*args, **kwargs):
    icrm = args[0]
    _c2_buffer = kwargs.pop('_c2_buffer', None)

    if icrm.direction == '->':
        return com_to_crm(*args, _c2_buffer=_c2_buffer)
    elif icrm.direction == '<-':
        return crm_to_com(*args, **kwargs)
    else:
        raise ValueError(f'Invalid direction: {icrm.direction}')
```

**2. `com_to_crm`** (in `transferable.py`): Accept `_c2_buffer` parameter and use it for response handling:

```python
def com_to_crm(*args, _c2_buffer=None):
    output_transferable = output.deserialize if output else None

    # ... input serialization + RPC call (unchanged) ...

    # Deserialize output — buffer mode affects SHM lifecycle
    if hasattr(response, 'release'):
        mv = memoryview(response)
        if _c2_buffer == 'hold':
            # Hold: deserialize over SHM, return HeldResult with release callback
            result = output_transferable(mv)
            def release_cb():
                mv.release()
                try:
                    response.release()
                except Exception:
                    pass
            return HeldResult(result, release_cb)
        else:
            # View (default): deserialize, then release SHM immediately
            try:
                result = output_transferable(mv)
                if isinstance(result, memoryview):
                    result = bytes(result)
            finally:
                mv.release()
                response.release()
            return result
    else:
        # No SHM (thread-local or HTTP) — return directly
        result = output_transferable(response) if output_transferable else None
        if _c2_buffer == 'hold':
            return HeldResult(result, None)  # no-op release
        return result
```

**3. Thread-local fast path** (in `com_to_crm`): When `supports_direct_call` is true, `_c2_buffer` wrapping still applies:

```python
if getattr(client, 'supports_direct_call', False):
    result = client.call_direct(method_name, request or ())
    if _c2_buffer == 'hold':
        return HeldResult(result, None)  # no SHM, no-op release
    return result
```

This ensures `cc.hold()` works uniformly across all three transports.

#### Transport Degradation

| Transport | `cc.hold()` Behavior |
|-----------|---------------------|
| IPC (SHM) | Full hold — SHM stays allocated until `.release()` |
| HTTP | Graceful no-op — `HeldResult` wraps value, `.release()` is no-op |
| Thread-local | Graceful no-op — no serialization at all, value is direct |

### 7. Transport-Level Codec Selection (Out of Scope — Future Work)

> **This section describes future direction only.** It is NOT part of this redesign's implementation scope. It is included for context so that the current design does not preclude future codec negotiation.

Codec is determined by transport, not by method annotation:

| Transport | Current Codec | Future Codec |
|-----------|--------------|--------------|
| Thread-local | None (direct call) | None |
| IPC | pickle | fastdb (planned) |
| HTTP | pickle | JSON (for cross-language) |

The framework auto-selects the codec based on transport type and type capabilities. Users write zero codec annotations on methods. This follows the industrial pattern established by ConnectRPC (Content-Type header) and Dubbo Triple (HTTP/2 metadata negotiation).

When fastdb is integrated, the framework will probe each type at registration time: if a type has a fastdb schema, IPC uses fastdb; otherwise, pickle. HTTP transport will use JSON for cross-language compatibility. This negotiation is transparent to the ICRM author.

## Migration

### Breaking Changes

1. **`_buffer_mode` removed from `Transferable` class** — replaced by `@cc.transfer(buffer=...)` on methods.
2. **`copy` buffer mode removed** — `view` is the new default everywhere.
3. **Priority 2 field-name matching deleted** — methods that relied on implicit name+type matching must add `@cc.transfer(input=X)` explicitly.

### Migration Path

1. **Parameter bundle transferables** (like `GridInfo`, `PeerGridInfos`): Delete them. The framework auto-bundles via pickle. If custom serialization was needed, use `@cc.transfer(input=X)` on the method.
2. **Domain type transferables** (like `GridSchema`, `GridAttribute`): No change.
3. **`buffer='copy'` on transferables**: Remove the parameter. Default is now `view`.
4. **`buffer='view'` on transferables**: Move to `@cc.transfer(buffer='view')` on the ICRM method (or omit — it's the default).
5. **`buffer='hold'` on transferables**: Move to `@cc.transfer(buffer='hold')` on the ICRM method for input; use `cc.hold()` at call sites for output.

### Grid Example After Migration

```python
# transferables.py — only domain types remain
@cc.transferable
class GridSchema:
    epsg: int
    bounds: list[float]
    first_size: list[float]
    subdivide_rules: list[list[int]]
    def serialize(schema: 'GridSchema') -> bytes: ...
    def deserialize(data: bytes) -> 'GridSchema': ...

@cc.transferable
class GridAttribute:
    level: int
    type: int
    activate: bool
    # ... remaining fields ...
    def serialize(data: 'GridAttribute') -> bytes: ...
    def deserialize(data: bytes) -> 'GridAttribute': ...


# igrid.py — zero annotation needed for most methods
@cc.icrm(namespace='icrm', version='0.1.0')
class IGrid:
    def get_schema(self) -> GridSchema:          # auto-matched
        ...

    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...                                      # auto-bundle via pickle

    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        ...                                      # auto-bundle input, pickle for output (list[T] not direct-matched)

    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        ...                                      # auto-bundle via pickle

    def hello(self, name: str) -> str:
        ...                                      # auto-bundle via pickle


# client.py — hold when needed
grid = cc.connect(IGrid, name='grid', address='ipc://server')

schema = grid.get_schema()                       # view (default)

with cc.hold(grid.get_grid_infos)(1, [0, 1]) as held:
    infos = held.value                           # list[GridAttribute], held in SHM
    process(infos)
# SHM released
```

## Thread Safety

All components are designed for free-threading (Python 3.14+):

- **`cc.hold()`**: Returns a new closure per call. No shared mutable state.
- **`HeldResult`**: Each instance is independent. No global state.
- **`auto_transfer` cache**: Protected by existing `_TRANSFERABLE_LOCK`.
- **`com_to_crm` / `crm_to_com`**: Stateless closures bound at ICRM decoration time.
- **`ICRMProxy`**: Already uses `threading.Lock` for close operations.

## Files Affected

| File | Change |
|------|--------|
| `src/c_two/crm/transferable.py` | Remove `_buffer_mode` from `Transferable`; delete Priority 2 matching (L464-483); add `HeldResult` class; add `hold()` function; modify `transfer_wrapper` to pop and forward `_c2_buffer`; modify `com_to_crm` to accept `_c2_buffer` and return `HeldResult` in hold mode; thread-local fast path wraps in `HeldResult` when hold |
| `src/c_two/crm/meta.py` | Detect `__cc_transfer__` metadata on methods; pass `input`/`output`/`buffer` kwargs to `auto_transfer` |
| `src/c_two/crm/__init__.py` | Export `hold`, `HeldResult`, `transfer` |
| `src/c_two/transport/server/native.py` | Remove `copy` branch from dispatch; default `buffer_mode` to `'view'` |
| `tests/unit/test_transferable.py` | Update tests for new matching rules; add `_c2_buffer` propagation tests |
| `tests/unit/test_held_result.py` | New: `HeldResult` lifecycle (release, context manager, double-release, `__del__` warning) |
| `examples/grid/transferables.py` | Delete parameter bundles (GridInfo, PeerGridInfos, GridInfos, GridAttributes, GridKeys) |
| `examples/grid/igrid.py` | Remove transferable imports; clean up |
