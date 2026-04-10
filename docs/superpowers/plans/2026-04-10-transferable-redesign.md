# Transferable Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Separate domain-type codecs from parameter bundling, move buffer lifecycle from types to method/call-site, add `cc.hold()` output hold API.

**Architecture:** Remove `_buffer_mode` from `Transferable`, delete Priority 2 field-name matching, add metadata-only `@cc.transfer` decorator consumed by `auto_transfer`, add `HeldResult[R]` and `cc.hold()` for client-side output buffer control, update server dispatch to remove `copy` branch.

**Tech Stack:** Python 3.10+, pytest, pickle (default codec)

**Spec:** `docs/superpowers/specs/2026-07-15-transferable-redesign-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/c_two/crm/transferable.py` | Core: `Transferable`, `@transferable`, `@transfer`, `auto_transfer`, `transfer()`, `HeldResult`, `hold()` |
| `src/c_two/crm/meta.py` | `icrm()` decorator: detect `__cc_transfer__` and pass config to `auto_transfer` |
| `src/c_two/crm/__init__.py` | Re-export `transfer`, `hold`, `HeldResult` |
| `src/c_two/__init__.py` | Top-level export of `transfer`, `hold`, `HeldResult` |
| `src/c_two/transport/server/native.py` | Server dispatch: remove `copy` branch, default to `view` |
| `tests/unit/test_held_result.py` | New: `HeldResult` lifecycle tests |
| `tests/unit/test_transfer_decorator.py` | New: `@cc.transfer` metadata + `auto_transfer` integration tests |
| `tests/unit/test_transferable.py` | Update: remove Priority 2 expectations, add new matching rule tests |
| `examples/grid/transferables.py` | Cleanup: delete parameter bundle classes |
| `examples/grid/igrid.py` | Cleanup: remove bundle imports, add `@cc.transfer` where needed |

---

### Task 1: Add `HeldResult[R]` class

**Files:**
- Create: `tests/unit/test_held_result.py`
- Modify: `src/c_two/crm/transferable.py:1-15` (imports)

This task adds `HeldResult` as a standalone class in `transferable.py` with full lifecycle management, then tests it in isolation.

- [ ] **Step 1: Write failing tests for `HeldResult`**

```python
# tests/unit/test_held_result.py
import warnings
import pytest


class TestHeldResultBasic:
    """HeldResult lifecycle: access, release, double-release."""

    def test_access_value(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult(42, release_cb=None)
        assert hr.value == 42

    def test_release_clears_value(self):
        from c_two.crm.transferable import HeldResult
        released = []
        hr = HeldResult('data', release_cb=lambda: released.append(True))
        hr.release()
        assert released == [True]
        with pytest.raises(Exception):
            _ = hr.value

    def test_double_release_is_noop(self):
        from c_two.crm.transferable import HeldResult
        count = []
        hr = HeldResult(99, release_cb=lambda: count.append(1))
        hr.release()
        hr.release()
        assert len(count) == 1

    def test_context_manager(self):
        from c_two.crm.transferable import HeldResult
        released = []
        with HeldResult('ctx', release_cb=lambda: released.append(True)) as held:
            assert held.value == 'ctx'
        assert released == [True]
        with pytest.raises(Exception):
            _ = held.value

    def test_none_release_cb(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('no-shm', release_cb=None)
        hr.release()
        with pytest.raises(Exception):
            _ = hr.value

    def test_del_warns_if_not_released(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('leak', release_cb=lambda: None)
        with warnings.catch_warnings(record=True) as w:
            warnings.simplefilter('always')
            hr.__del__()
            assert len(w) == 1
            assert 'HeldResult' in str(w[0].message)
            assert issubclass(w[0].category, ResourceWarning)

    def test_del_no_warn_if_already_released(self):
        from c_two.crm.transferable import HeldResult
        hr = HeldResult('ok', release_cb=lambda: None)
        hr.release()
        with warnings.catch_warnings(record=True) as w:
            warnings.simplefilter('always')
            hr.__del__()
            assert len(w) == 0

    def test_release_cb_exception_swallowed(self):
        from c_two.crm.transferable import HeldResult
        def bad_cb():
            raise RuntimeError('boom')
        hr = HeldResult('err', release_cb=bad_cb)
        hr.release()  # should not raise
        with pytest.raises(Exception):
            _ = hr.value
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_held_result.py -v`
Expected: FAIL — `ImportError: cannot import name 'HeldResult'`

- [ ] **Step 3: Implement `HeldResult` in `transferable.py`**

Add to `src/c_two/crm/transferable.py` after the imports section (before `_add_length_prefix`), adding `sys` and `warnings` to imports:

```python
import sys
import warnings

# ... existing imports ...

R = TypeVar('R')

class HeldResult:
    """Wraps a method return value with explicit SHM lifecycle control.

    Three-layer safety net:
    1. Explicit .release() — preferred
    2. Context manager (__enter__/__exit__) — recommended
    3. __del__ fallback — last resort with warning
    """

    __slots__ = ('_value', '_release_cb', '_released')

    def __init__(self, value, release_cb=None):
        self._value = value
        self._release_cb = release_cb
        self._released = False

    @property
    def value(self):
        if self._released:
            raise RuntimeError("SHM released — value no longer accessible")
        return self._value

    def release(self):
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

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.release()

    def __del__(self):
        if getattr(self, '_released', True):
            return
        _is_finalizing = sys.is_finalizing
        _warn = warnings.warn
        if _is_finalizing():
            self.release()
            return
        _warn(
            "HeldResult was garbage-collected without release() — "
            "potential SHM leak. Use 'with cc.hold(...)' or call .release().",
            ResourceWarning,
            stacklevel=2,
        )
        self.release()
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_held_result.py -v`
Expected: All PASS

- [ ] **Step 5: Verify no regressions**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 553+ passed

- [ ] **Step 6: Commit**

```bash
git add tests/unit/test_held_result.py src/c_two/crm/transferable.py
git commit -m "feat(transferable): add HeldResult class for SHM lifecycle control

Implements HeldResult[R] with three-layer safety: explicit .release(),
context manager, and __del__ fallback with ResourceWarning.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 2: Add `cc.hold()` function

**Files:**
- Modify: `src/c_two/crm/transferable.py` (add `hold()` function)
- Modify: `tests/unit/test_held_result.py` (add `hold()` tests)

- [ ] **Step 1: Write failing tests for `cc.hold()`**

Append to `tests/unit/test_held_result.py`:

```python
class TestHoldFunction:
    """cc.hold() wraps a bound ICRM method to inject _c2_buffer='hold'."""

    def test_hold_injects_c2_buffer(self):
        from c_two.crm.transferable import hold

        class FakeProxy:
            def compute(self, x, **kwargs):
                return kwargs

        proxy = FakeProxy()
        wrapped = hold(proxy.compute)
        result = wrapped(42)
        assert result['_c2_buffer'] == 'hold'

    def test_hold_rejects_unbound_function(self):
        from c_two.crm.transferable import hold

        def standalone(x):
            return x

        with pytest.raises(TypeError, match='bound'):
            hold(standalone)

    def test_hold_rejects_non_callable(self):
        from c_two.crm.transferable import hold

        with pytest.raises(TypeError):
            hold(42)

    def test_hold_preserves_args(self):
        from c_two.crm.transferable import hold

        class FakeProxy:
            def compute(self, a, b, **kwargs):
                return (a, b, kwargs.get('_c2_buffer'))

        proxy = FakeProxy()
        wrapped = hold(proxy.compute)
        result = wrapped(1, 2)
        assert result == (1, 2, 'hold')
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_held_result.py::TestHoldFunction -v`
Expected: FAIL — `ImportError: cannot import name 'hold'`

- [ ] **Step 3: Implement `hold()` in `transferable.py`**

Add after the `HeldResult` class:

```python
def hold(method):
    """Wrap an ICRM bound method to hold SHM on the response.

    Usage: ``cc.hold(proxy.method)(args)`` — single-shot pattern.
    Returns a callable that injects ``_c2_buffer='hold'`` into kwargs.
    """
    if not callable(method):
        raise TypeError(
            f"cc.hold() requires a callable, got {type(method).__name__}"
        )
    self_obj = getattr(method, '__self__', None)
    name = getattr(method, '__name__', None)
    if self_obj is None or name is None:
        raise TypeError(
            "cc.hold() requires a bound ICRM method, "
            "e.g. cc.hold(grid.compute)"
        )

    @wraps(method)
    def wrapper(*args, **kwargs):
        kwargs['_c2_buffer'] = 'hold'
        return getattr(self_obj, name)(*args, **kwargs)

    return wrapper
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_held_result.py -v`
Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_held_result.py
git commit -m "feat(transferable): add cc.hold() for output buffer control

cc.hold(proxy.method)(args) injects _c2_buffer='hold' kwarg.
Validates input is a bound method. Single-shot pattern.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 3: Add metadata-only `@cc.transfer` decorator

**Files:**
- Modify: `src/c_two/crm/transferable.py:270` (rewrite `transfer()`)
- Create: `tests/unit/test_transfer_decorator.py`

The current `transfer()` function (L270–436) is overloaded — it both stores metadata and creates the full `com_to_crm`/`crm_to_com`/`transfer_wrapper` closure. The redesign splits this: `transfer()` becomes metadata-only, and the wrapping logic moves into a new internal `_build_transfer_wrapper()` function.

- [ ] **Step 1: Write failing tests for metadata-only `@cc.transfer`**

```python
# tests/unit/test_transfer_decorator.py
import pickle
import pytest
import c_two as cc
from c_two.crm.transferable import Transferable


@cc.transferable
class _FakeInput(Transferable):
    x: int
    def serialize(d: '_FakeInput') -> bytes:
        return pickle.dumps(d.x)
    def deserialize(b: bytes) -> '_FakeInput':
        return _FakeInput(x=pickle.loads(b))


@cc.transferable
class _FakeOutput(Transferable):
    y: str
    def serialize(d: '_FakeOutput') -> bytes:
        return pickle.dumps(d.y)
    def deserialize(b: bytes) -> '_FakeOutput':
        return _FakeOutput(y=pickle.loads(b))


class TestTransferDecorator:
    """@cc.transfer attaches __cc_transfer__ metadata without wrapping."""

    def test_attaches_metadata(self):
        @cc.transfer(input=_FakeInput, output=_FakeOutput)
        def my_method(self, x: int) -> str:
            ...

        meta = getattr(my_method, '__cc_transfer__', None)
        assert meta is not None
        assert meta['input'] is _FakeInput
        assert meta['output'] is _FakeOutput
        assert meta['buffer'] == 'view'

    def test_buffer_hold(self):
        @cc.transfer(buffer='hold')
        def my_method(self, data: bytes) -> bytes:
            ...

        meta = my_method.__cc_transfer__
        assert meta['buffer'] == 'hold'
        assert meta['input'] is None
        assert meta['output'] is None

    def test_does_not_wrap_function(self):
        """The decorated function should be the original — no wrapper layer."""
        def original(self, x: int) -> int:
            return x

        decorated = cc.transfer(input=_FakeInput)(original)
        # The function itself is unchanged (no __wrapped__, same object)
        assert decorated is original

    def test_invalid_buffer_raises(self):
        with pytest.raises(ValueError, match='buffer'):
            @cc.transfer(buffer='copy')
            def bad(self) -> None:
                ...
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transfer_decorator.py -v`
Expected: FAIL — current `transfer()` returns a wrapping closure, not the original function

- [ ] **Step 3: Rewrite `transfer()` as metadata-only, extract `_build_transfer_wrapper()`**

Replace the existing `transfer()` function (L270–436) in `transferable.py`. The new `transfer()` only attaches `__cc_transfer__`. The closure-building logic moves to `_build_transfer_wrapper(func, input, output, buffer)`:

New `transfer()`:
```python
_VALID_TRANSFER_BUFFERS = frozenset(('view', 'hold'))

def transfer(*, input=None, output=None, buffer='view'):
    """Metadata-only decorator for ICRM methods.

    Attaches ``__cc_transfer__`` dict to the function. Does NOT wrap it.
    Consumed by ``icrm()`` → ``auto_transfer()`` at class decoration time.

    Parameters
    ----------
    input : type[Transferable] | None
        Custom input transferable. None = auto-bundle via pickle.
    output : type[Transferable] | None
        Custom output transferable. None = auto-bundle via pickle.
    buffer : 'view' | 'hold'
        Input buffer mode (CRM-side). Default 'view'.
    """
    if buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be one of {sorted(_VALID_TRANSFER_BUFFERS)}, got {buffer!r}"
        )

    def decorator(func):
        func.__cc_transfer__ = {
            'input': input,
            'output': output,
            'buffer': buffer,
        }
        return func  # NO wrapping
    return decorator
```

New `_build_transfer_wrapper(func, input, output, buffer)` — contains the existing `com_to_crm`, `crm_to_com`, `transfer_wrapper` closure logic, with modifications for `_c2_buffer` propagation:

```python
def _build_transfer_wrapper(func, input=None, output=None, buffer='view'):
    """Build the com_to_crm / crm_to_com / transfer_wrapper closure.

    This is the internal implementation that was previously inside transfer().
    Called by auto_transfer() after resolving input/output transferables.
    """
    method_name = func.__name__

    def com_to_crm(*args, _c2_buffer=None):
        input_transferable = input.serialize if input else None
        output_transferable = output.deserialize if output else None

        try:
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')

            icrm = args[0]
            client = icrm.client
            request = args[1:] if len(args) > 1 else None

            # Thread fast path
            if getattr(client, 'supports_direct_call', False):
                result = client.call_direct(method_name, request or ())
                if _c2_buffer == 'hold':
                    return HeldResult(result, None)
                return result

            # Standard cross-process path
            stage = 'serialize_input'
            serialized_args = input_transferable(*request) if (request is not None and input_transferable is not None) else None

            stage = 'call_crm'
            response = client.call(method_name, serialized_args)

            stage = 'deserialize_output'
            if not output_transferable:
                if hasattr(response, 'release'):
                    response.release()
                if _c2_buffer == 'hold':
                    return HeldResult(None, None)
                return None

            if hasattr(response, 'release'):
                mv = memoryview(response)
                if _c2_buffer == 'hold':
                    result = output_transferable(mv)
                    def release_cb():
                        mv.release()
                        try:
                            response.release()
                        except Exception:
                            pass
                    return HeldResult(result, release_cb)
                else:
                    # view (default)
                    try:
                        result = output_transferable(mv)
                        if isinstance(result, memoryview):
                            result = bytes(result)
                    finally:
                        mv.release()
                        response.release()
                    return result
            else:
                result = output_transferable(response)
                if _c2_buffer == 'hold':
                    return HeldResult(result, None)
                return result

        except error.CCBaseError:
            raise
        except Exception as e:
            if stage == 'serialize_input':
                raise error.CompoSerializeInput(str(e)) from e
            elif stage == 'call_crm':
                raise error.CompoCRMCalling(str(e)) from e
            else:
                raise error.CompoDeserializeOutput(str(e)) from e

    def crm_to_com(*args, _release_fn=None):
        input_transferable = input.deserialize if input else None
        output_transferable = output.serialize if output else None
        input_buffer_mode = buffer

        err = None
        result = None
        stage = 'deserialize_input'

        try:
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')

            iicrm = args[0]
            crm = iicrm.crm
            request = args[1] if len(args) > 1 else None

            if request is not None and input_transferable is not None:
                if input_buffer_mode == 'view':
                    deserialized_args = input_transferable(request)
                    if _release_fn is not None:
                        _release_fn()
                        _release_fn = None
                else:  # hold
                    deserialized_args = input_transferable(request)
            else:
                deserialized_args = tuple()
                if _release_fn is not None:
                    _release_fn()
                    _release_fn = None

            if not isinstance(deserialized_args, tuple):
                deserialized_args = (deserialized_args,)

            crm_method = getattr(crm, method_name, None)
            if crm_method is None:
                raise ValueError(f'Method "{method_name}" not found on CRM class.')

            stage = 'execute_function'
            result = crm_method(*deserialized_args)
            err = None

        except Exception as e:
            result = None
            if _release_fn is not None:
                try:
                    _release_fn()
                except Exception:
                    pass
            if stage == 'deserialize_input':
                err = error.CRMDeserializeInput(str(e))
            elif stage == 'execute_function':
                err = error.CRMExecuteFunction(str(e))
            else:
                err = error.CRMSerializeOutput(str(e))

        serialized_error = error.CCError.serialize(err)
        serialized_result = b''
        if output_transferable is not None and result is not None:
            serialized_result = (
                output_transferable(*result) if isinstance(result, tuple)
                else output_transferable(result)
            )
        return (serialized_error, serialized_result)

    @wraps(func)
    def transfer_wrapper(*args, **kwargs):
        if not args:
            raise ValueError('No arguments provided to determine direction.')

        icrm = args[0]
        if not hasattr(icrm, 'direction'):
            raise AttributeError('The ICRM instance does not have a "direction" attribute.')

        if icrm.direction == '->':
            _c2_buffer = kwargs.pop('_c2_buffer', None)
            return com_to_crm(*args, _c2_buffer=_c2_buffer)
        elif icrm.direction == '<-':
            return crm_to_com(*args, **kwargs)
        else:
            raise ValueError(f'Invalid direction value: {icrm.direction}. Expected "->" or "<-".')

    transfer_wrapper._input_buffer_mode = buffer
    return transfer_wrapper
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transfer_decorator.py tests/unit/test_held_result.py -v`
Expected: All PASS

- [ ] **Step 5: Verify no regressions**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 553+ passed (existing tests still pass because `auto_transfer` still calls the build function internally)

- [ ] **Step 6: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transfer_decorator.py
git commit -m "refactor(transferable): split transfer() into metadata + _build_transfer_wrapper

@cc.transfer is now metadata-only (attaches __cc_transfer__).
_build_transfer_wrapper() contains the com_to_crm/crm_to_com logic.
crm_to_com removes copy branch, uses buffer param directly.
com_to_crm accepts _c2_buffer for hold mode support.
transfer_wrapper pops _c2_buffer from kwargs on -> path.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 4: Simplify `auto_transfer` — delete Priority 2, accept config kwargs

**Files:**
- Modify: `src/c_two/crm/transferable.py:438-538` (`auto_transfer`)
- Modify: `tests/unit/test_transferable.py` (update expectations)

- [ ] **Step 1: Write tests for the new `auto_transfer` behavior**

Add to `tests/unit/test_transfer_decorator.py`:

```python
class TestAutoTransferWithConfig:
    """auto_transfer accepts input/output/buffer kwargs from @cc.transfer."""

    def test_explicit_input_override(self):
        from c_two.crm.transferable import auto_transfer
        wrapped = auto_transfer(
            lambda self, x: ...,
            input=_FakeInput,
        )
        assert callable(wrapped)
        # The wrapper should use _FakeInput, not a default
        assert wrapped._input_buffer_mode == 'view'

    def test_explicit_buffer_hold(self):
        from c_two.crm.transferable import auto_transfer
        def my_method(self, x: int) -> int: ...
        wrapped = auto_transfer(my_method, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'

    def test_explicit_output_override(self):
        from c_two.crm.transferable import auto_transfer
        def my_method(self, x: int) -> str: ...
        wrapped = auto_transfer(my_method, output=_FakeOutput)
        assert callable(wrapped)
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transfer_decorator.py::TestAutoTransferWithConfig -v`
Expected: FAIL — `auto_transfer` doesn't accept kwargs yet

- [ ] **Step 3: Rewrite `auto_transfer` to accept kwargs and delete Priority 2**

Replace `auto_transfer` (L438–538) with:

```python
def auto_transfer(func=None, *, input=None, output=None, buffer='view'):
    """Auto-wrap a function with transfer logic.

    When called without kwargs, performs auto-matching:
    - Single-param direct match → registered @transferable
    - Return type direct match → registered @transferable
    - Fallback → DynamicInput/OutputTransferable (pickle)

    When called with kwargs (from @cc.transfer metadata), uses provided
    input/output transferables and fills gaps with auto-matching.
    """
    def create_wrapper(func):
        # --- Input Matching ---
        input_transferable = input  # explicit override

        if input_transferable is None:
            input_model = _create_pydantic_model_from_func_sig(func)
            if input_model is not None:
                input_model_fields = getattr(input_model, 'model_fields', {})
                is_empty_input = not bool(input_model_fields)

                if not is_empty_input:
                    # Priority 1: single param whose type is a registered Transferable
                    if len(input_model_fields) == 1:
                        field_info = next(iter(input_model_fields.values()))
                        param_type = field_info.annotation
                        param_module = getattr(param_type, '__module__', None)
                        param_name = getattr(param_type, '__name__', str(param_type))
                        full_name = f'{param_module}.{param_name}' if param_module else param_name
                        input_transferable = get_transferable(full_name)

                    # Priority 2 (field-name matching) — DELETED

                    if input_transferable is None and not is_empty_input:
                        input_transferable = create_default_transferable(func, is_input=True)

        # --- Output Matching ---
        output_transferable = output  # explicit override

        if output_transferable is None:
            type_hints = get_type_hints(func)
            if 'return' in type_hints:
                return_type = type_hints['return']
                if return_type is not None and return_type is not type(None):
                    return_type_name = getattr(return_type, '__name__', str(return_type))
                    return_type_module = getattr(return_type, '__module__', None)
                    full_name = f'{return_type_module}.{return_type_name}' if return_type_module else return_type_name
                    output_transferable = get_transferable(full_name)

                    if output_transferable is None and not (return_type is None or return_type is type(None)):
                        output_transferable = create_default_transferable(func, is_input=False)

        # --- Build wrapper ---
        return _build_transfer_wrapper(func, input=input_transferable, output=output_transferable, buffer=buffer)

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
            raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)
```

Key changes:
- Accepts `input`, `output`, `buffer` kwargs
- Deletes Priority 2 field-name matching block (L463–483)
- Deletes output Priority 2 matching (L512–522)
- Calls `_build_transfer_wrapper()` instead of old `transfer()`

- [ ] **Step 4: Update existing tests in `test_transferable.py`**

The `test_matching_transferable_uses_existing` test relied on Priority 2 field-name matching. Update it:

```python
# In TestAutoTransfer class, update test_matching_transferable_uses_existing:
def test_matching_transferable_uses_existing(self):
    """When a function has a single param typed as a registered Transferable,
    auto_transfer should use it (Priority 1 direct lookup)."""
    def process(self, data: HelloData) -> HelloData: ...
    process.__module__ = HelloData.__module__
    wrapped = auto_transfer(process)
    assert callable(wrapped)
```

- [ ] **Step 5: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transfer_decorator.py tests/unit/test_transferable.py
git commit -m "refactor(transferable): simplify auto_transfer, delete Priority 2 matching

auto_transfer now accepts input/output/buffer kwargs from @cc.transfer.
Priority 2 field-name matching (L464-483) deleted — replaced by explicit
@cc.transfer override. Output Priority 2 also deleted.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 5: Update `icrm()` to detect `__cc_transfer__` metadata

**Files:**
- Modify: `src/c_two/crm/meta.py:113-118`
- Modify: `tests/unit/test_transfer_decorator.py` (add integration test)

- [ ] **Step 1: Write failing integration test**

Add to `tests/unit/test_transfer_decorator.py`:

```python
class TestIcrmTransferIntegration:
    """@cc.transfer metadata is consumed by icrm() via auto_transfer."""

    def test_icrm_picks_up_transfer_metadata(self):
        @cc.icrm(namespace='test.transfer', version='0.1.0')
        class ITest:
            @cc.transfer(input=_FakeInput, buffer='hold')
            def process(self, x: int) -> str:
                ...

            def plain(self, y: str) -> int:
                ...

        # process should have buffer='hold'
        process_method = getattr(ITest, 'process', None)
        assert process_method is not None
        assert getattr(process_method, '_input_buffer_mode', None) == 'hold'

        # plain should have default buffer='view'
        plain_method = getattr(ITest, 'plain', None)
        assert plain_method is not None
        assert getattr(plain_method, '_input_buffer_mode', None) == 'view'
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transfer_decorator.py::TestIcrmTransferIntegration -v`
Expected: FAIL — `icrm()` doesn't detect `__cc_transfer__`

- [ ] **Step 3: Update `icrm()` in `meta.py`**

Modify the method loop in `icrm()` (L113–118):

```python
        decorated_methods = {}
        for name, value in cls.__dict__.items():
            if isfunction(value) and name not in ('__dict__', '__weakref__', '__module__', '__qualname__', '__init__', '__tag__'):
                if getattr(value, _SHUTDOWN_ATTR, False):
                    continue  # @cc.on_shutdown methods are not RPC-callable
                transfer_config = getattr(value, '__cc_transfer__', None)
                if transfer_config:
                    decorated_methods[name] = auto_transfer(value, **transfer_config)
                else:
                    decorated_methods[name] = auto_transfer(value)
```

- [ ] **Step 4: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add src/c_two/crm/meta.py tests/unit/test_transfer_decorator.py
git commit -m "feat(meta): icrm() detects __cc_transfer__ metadata

The icrm() decorator now checks for __cc_transfer__ on methods and passes
input/output/buffer kwargs through to auto_transfer.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 6: Remove `_buffer_mode` from `Transferable` class and `@transferable` decorator

**Files:**
- Modify: `src/c_two/crm/transferable.py:78-104` (`Transferable` class)
- Modify: `src/c_two/crm/transferable.py:245-268` (`transferable()` decorator)
- Modify: `tests/unit/test_transferable.py` (update any `_buffer_mode` references)

- [ ] **Step 1: Remove `_buffer_mode` from `Transferable` class**

In `Transferable` class (L89), remove:
```python
    _buffer_mode: str = 'copy'  # 'copy' | 'view' | 'hold'
```

- [ ] **Step 2: Remove `buffer` param from `@transferable` decorator**

Simplify the `transferable()` decorator — remove `buffer` parameter:
```python
_VALID_BUFFER_MODES = frozenset(('copy', 'view', 'hold'))  # DELETE this line

def transferable(cls=None):
    """Decorator to make a class inherit from Transferable.

    Supports both ``@cc.transferable`` and ``@cc.transferable()``.
    """
    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls.__module__ = cls.__module__
        new_cls.__qualname__ = cls.__qualname__
        return new_cls

    if cls is not None:
        return wrap(cls)
    return wrap
```

- [ ] **Step 3: Remove `_buffer_mode` assignments in `create_default_transferable`**

In `create_default_transferable` (L192 and L227), delete:
```python
DynamicInputTransferable._buffer_mode = 'view'   # L192 — delete
DynamicOutputTransferable._buffer_mode = 'view'   # L227 — delete
```

- [ ] **Step 4: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add src/c_two/crm/transferable.py
git commit -m "refactor(transferable): remove _buffer_mode from Transferable class

Buffer lifecycle is now controlled at method level (@cc.transfer) and
call site (cc.hold()), not on the data type.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 7: Update server dispatch — remove `copy` branch, default to `view`

**Files:**
- Modify: `src/c_two/transport/server/native.py:57-65` (`build_dispatch_table`)
- Modify: `src/c_two/transport/server/native.py:322-395` (dispatch function)

- [ ] **Step 1: Update `build_dispatch_table` default**

Change L64:
```python
# Before:
buffer_mode = getattr(method, '_input_buffer_mode', 'copy')
# After:
buffer_mode = getattr(method, '_input_buffer_mode', 'view')
```

- [ ] **Step 2: Remove `copy` branch in dispatch**

In the dispatch function (L338–349), remove the `copy` block and make `view` the first branch:

```python
            # 2. Buffer-mode-aware request handling
            if buffer_mode == 'view':
                # Pass memoryview; _release_fn frees SHM after deserialize
                mv = memoryview(request_buf)
                released = False
                def release_fn():
                    nonlocal released
                    if not released:
                        released = True
                        mv.release()
                        try:
                            request_buf.release()
                        except Exception:
                            pass
                try:
                    result = method(mv, _release_fn=release_fn)
                finally:
                    if not released:
                        release_fn()
            else:  # hold
                # Pass memoryview directly; RAII handles lifetime
                mv = memoryview(request_buf)
                result = method(mv)
```

- [ ] **Step 3: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 4: Commit**

```bash
git add src/c_two/transport/server/native.py
git commit -m "refactor(server): remove copy buffer branch, default to view

Server dispatch now only handles 'view' and 'hold' buffer modes.
Default changed from 'copy' to 'view'.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 8: Wire up exports — `cc.transfer`, `cc.hold`, `cc.HeldResult`

**Files:**
- Modify: `src/c_two/crm/__init__.py`
- Modify: `src/c_two/__init__.py`

- [ ] **Step 1: Update `crm/__init__.py`**

```python
from .meta import ICRMMeta, MethodAccess, get_method_access, read, write
from .template import generate_crm_template
from .transferable import transfer, hold, HeldResult
```

- [ ] **Step 2: Update `src/c_two/__init__.py`**

Add imports:
```python
from .crm.transferable import transfer, hold, HeldResult
```

- [ ] **Step 3: Write import verification test**

Add to `tests/unit/test_transfer_decorator.py`:

```python
class TestExports:
    def test_cc_transfer_importable(self):
        assert hasattr(cc, 'transfer')
        assert callable(cc.transfer)

    def test_cc_hold_importable(self):
        assert hasattr(cc, 'hold')
        assert callable(cc.hold)

    def test_cc_held_result_importable(self):
        assert hasattr(cc, 'HeldResult')
```

- [ ] **Step 4: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add src/c_two/crm/__init__.py src/c_two/__init__.py tests/unit/test_transfer_decorator.py
git commit -m "feat: export cc.transfer, cc.hold, cc.HeldResult

Makes the new transferable redesign API accessible from the top-level
cc namespace.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 9: Update grid example — delete parameter bundles

**Files:**
- Modify: `examples/grid/transferables.py`
- Modify: `examples/grid/igrid.py`

- [ ] **Step 1: Simplify `transferables.py`**

Keep only `GridSchema`, `GridAttribute`, and the helper functions. Delete `GridInfo`, `PeerGridInfos`, `GridInfos`, `GridAttributes`, `GridKeys`.

- [ ] **Step 2: Simplify `igrid.py`**

Remove unused imports (`GridInfo`, `GridInfos`, `PeerGridInfos`). The ICRM methods that previously relied on Priority 2 matching now auto-bundle via pickle by default:

```python
import c_two as cc

from grid.transferables import GridSchema, GridAttribute

@cc.icrm(namespace='icrm', version='0.1.0')
class IGrid:
    def get_schema(self) -> GridSchema:
        ...

    def subdivide_grids(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    def get_parent_keys(self, levels: list[int], global_ids: list[int]) -> list[str | None]:
        ...

    def get_grid_infos(self, level: int, global_ids: list[int]) -> list[GridAttribute]:
        ...

    def get_active_grid_infos(self) -> tuple[list[int], list[int]]:
        ...

    def hello(self, name: str) -> str:
        ...

    def none_hello(self, message: str) -> str | None:
        ...

    @cc.on_shutdown
    def terminate(self) -> None:
        ...
```

- [ ] **Step 3: Commit**

```bash
git add examples/grid/transferables.py examples/grid/igrid.py
git commit -m "refactor(examples): delete parameter bundle transferables from grid

Only domain types (GridSchema, GridAttribute) remain. All other
methods auto-bundle via pickle.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 10: End-to-end integration test

**Files:**
- Modify: `tests/unit/test_transfer_decorator.py` (add e2e test)

- [ ] **Step 1: Write an end-to-end test verifying the full flow**

```python
class TestEndToEnd:
    """Full round-trip: ICRM with @cc.transfer, proxy call, hold mode."""

    def test_auto_bundle_round_trip(self):
        """Methods without @cc.transfer auto-bundle via pickle."""
        @cc.icrm(namespace='test.e2e', version='0.1.0')
        class IE2E:
            def greet(self, name: str) -> str:
                ...

        from tests.fixtures.hello import Hello
        crm = Hello()

        # Server-side ICRM
        server_icrm = IE2E()
        server_icrm.crm = crm
        server_icrm.direction = '<-'

        # Simulate crm_to_com: serialize input, call CRM, serialize output
        greet = getattr(server_icrm, 'greet')
        serialized_input = pickle.dumps('World')
        err_bytes, result_bytes = greet(serialized_input)

        # Verify no error
        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert pickle.loads(result_bytes) == 'Hello, World!'

    def test_hold_mode_thread_local(self):
        """cc.hold() on thread-local proxy returns HeldResult."""
        from c_two.crm.transferable import HeldResult

        @cc.icrm(namespace='test.hold', version='0.1.0')
        class IHoldTest:
            def compute(self, x: int) -> int:
                ...

        # Create thread-local proxy
        class FakeCRM:
            def compute(self, x: int) -> int:
                return x * 2

        from c_two.transport.client.proxy import ICRMProxy
        proxy = ICRMProxy.thread_local(FakeCRM())

        client_icrm = IHoldTest()
        client_icrm.client = proxy
        client_icrm.direction = '->'

        # Normal call
        result = client_icrm.compute(5)
        assert result == 10

        # Hold call
        held = cc.hold(client_icrm.compute)(5)
        assert isinstance(held, HeldResult)
        assert held.value == 10
        held.release()
```

- [ ] **Step 2: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 3: Commit**

```bash
git add tests/unit/test_transfer_decorator.py
git commit -m "test: add end-to-end integration tests for transferable redesign

Tests auto-bundle round-trip and cc.hold() on thread-local proxy.

Part of transferable redesign (spec: 2026-07-15)."
```

---

## Notes

- **Backward compatibility**: The grid example is the only consumer of Priority 2 matching and `_buffer_mode` on transferables. No external users are affected.
- **Thread safety**: All new code (HeldResult, hold()) is stateless per-instance. No new global state introduced.
- **`copy` mode removal**: The `copy` branch in both `_build_transfer_wrapper` and `native.py` dispatch is deleted. `view` becomes the universal default.
- **Testing strategy**: Each task includes its own tests. Task 10 provides end-to-end verification. The full suite (553+ tests) is re-run after each task.
