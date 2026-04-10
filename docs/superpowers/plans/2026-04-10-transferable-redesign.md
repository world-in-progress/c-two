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

    def __del__(self, _is_finalizing=sys.is_finalizing, _warn=warnings.warn):
        # NOTE: _is_finalizing and _warn are bound as default args at class
        # definition time so they survive late interpreter teardown.
        if getattr(self, '_released', True):
            return
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
from c_two.crm.transferable import (
    Transferable, transfer, _build_transfer_wrapper, auto_transfer,
)
import c_two as cc


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
        @transfer(input=_FakeInput, output=_FakeOutput)
        def my_method(self, x: int) -> str:
            ...

        meta = getattr(my_method, '__cc_transfer__', None)
        assert meta is not None
        assert meta['input'] is _FakeInput
        assert meta['output'] is _FakeOutput
        assert meta['buffer'] == 'view'

    def test_buffer_hold(self):
        @transfer(buffer='hold')
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

        decorated = transfer(input=_FakeInput)(original)
        # The function itself is unchanged (no __wrapped__, same object)
        assert decorated is original

    def test_invalid_buffer_raises(self):
        with pytest.raises(ValueError, match='buffer'):
            @transfer(buffer='copy')
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

- [ ] **Step 5: Update `auto_transfer` to call `_build_transfer_wrapper()` instead of old `transfer()`**

The current `auto_transfer` at L529 calls `transfer(input=..., output=...)` which now only attaches metadata. It must call `_build_transfer_wrapper()` instead:

```python
        # --- Wrapping --- (L528-531)
        # Before: transfer_decorator = transfer(input=input_transferable, output=output_transferable)
        # Before: wrapped_func = transfer_decorator(func)
        # After:
        wrapped_func = _build_transfer_wrapper(func, input=input_transferable, output=output_transferable)
        return wrapped_func
```

Also update any direct `transfer()` callsites in tests:
- `tests/unit/test_transferable.py` lines that call `transfer(input=..., output=...)(func)` must now call `_build_transfer_wrapper(func, input=..., output=...)` or use `auto_transfer` instead.

- [ ] **Step 6: Verify no regressions**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 553+ passed (auto_transfer now calls _build_transfer_wrapper, existing behavior preserved)

- [ ] **Step 7: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transfer_decorator.py tests/unit/test_transferable.py
git commit -m "refactor(transferable): split transfer() into metadata + _build_transfer_wrapper

@cc.transfer is now metadata-only (attaches __cc_transfer__).
_build_transfer_wrapper() contains the com_to_crm/crm_to_com logic.
crm_to_com removes copy branch, uses buffer param directly.
com_to_crm accepts _c2_buffer for hold mode support.
transfer_wrapper pops _c2_buffer from kwargs on -> path.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 4: Add kwargs support to `auto_transfer` (keep Priority 2 for now)

**Files:**
- Modify: `src/c_two/crm/transferable.py:438-538` (`auto_transfer`)
- Modify: `tests/unit/test_transfer_decorator.py` (add config tests)

Priority 2 matching is NOT deleted yet — that happens atomically with grid example cleanup in Task 7. This task only adds `input`/`output`/`buffer` kwargs so `icrm()` can pass `@cc.transfer` config through.

- [ ] **Step 1: Write tests for the new `auto_transfer` kwargs behavior**

Add to `tests/unit/test_transfer_decorator.py`:

```python
class TestAutoTransferWithConfig:
    """auto_transfer accepts input/output/buffer kwargs from @cc.transfer."""

    def test_explicit_input_override(self):
        wrapped = auto_transfer(
            lambda self, x: ...,
            input=_FakeInput,
        )
        assert callable(wrapped)
        # The wrapper should use _FakeInput, not a default
        assert wrapped._input_buffer_mode == 'view'

    def test_explicit_buffer_hold(self):
        def my_method(self, x: int) -> int: ...
        wrapped = auto_transfer(my_method, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'

    def test_explicit_output_override(self):
        def my_method(self, x: int) -> str: ...
        wrapped = auto_transfer(my_method, output=_FakeOutput)
        assert callable(wrapped)
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transfer_decorator.py::TestAutoTransferWithConfig -v`
Expected: FAIL — `auto_transfer` doesn't accept kwargs yet

- [ ] **Step 3: Rewrite `auto_transfer` to accept kwargs (keep Priority 2 for now)**

Replace `auto_transfer` (L438–538) with:

```python
def auto_transfer(func=None, *, input=None, output=None, buffer='view'):
    """Auto-wrap a function with transfer logic.

    When called without kwargs, performs auto-matching:
    - Single-param direct match → registered @transferable
    - Multi-param field-name match (Priority 2) → registered @transferable
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

                    # Priority 2: field-name matching (kept for backward compat;
                    # will be removed with grid example migration in Task 7)
                    if input_transferable is None and len(input_model_fields) > 1:
                        for info in _get_all_transferable_info():
                            registered_fields = info.get('fields', {})
                            if registered_fields and set(input_model_fields.keys()) == set(registered_fields.keys()):
                                expected_input_types = {k: v.annotation for k, v in input_model_fields.items()}
                                if expected_input_types == registered_fields:
                                    input_transferable = get_transferable(info['name'])
                                    break

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

Key changes from the old version:
- Accepts `input`, `output`, `buffer` kwargs (explicit overrides)
- Priority 2 field-name matching is KEPT for now (removed in Task 7 with grid migration)
- Calls `_build_transfer_wrapper()` instead of old `transfer()`

- [ ] **Step 4: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass (Priority 2 still works, kwargs are additive)

- [ ] **Step 5: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transfer_decorator.py
git commit -m "refactor(transferable): add kwargs to auto_transfer for @cc.transfer config

auto_transfer now accepts input/output/buffer kwargs from @cc.transfer.
Priority 2 field-name matching preserved for backward compatibility.

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
            @transfer(input=_FakeInput, buffer='hold')
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

    def test_explicit_transferable_actually_runs(self):
        """Round-trip: explicit @transfer(input=..., output=...) uses the custom codec."""
        import pickle

        serialize_calls = []
        deserialize_calls = []

        @cc.transferable
        class TrackedInput(Transferable):
            val: int
            def serialize(d: 'TrackedInput') -> bytes:
                serialize_calls.append(d.val)
                return pickle.dumps(d.val)
            def deserialize(b: bytes) -> 'TrackedInput':
                v = pickle.loads(b)
                deserialize_calls.append(v)
                return TrackedInput(val=v)

        @cc.icrm(namespace='test.roundtrip', version='0.1.0')
        class IRoundTrip:
            @transfer(input=TrackedInput)
            def process(self, data: TrackedInput) -> int:
                ...

        # Simulate server-side call
        class CRM:
            def process(self, data):
                return data.val * 2

        server = IRoundTrip()
        server.crm = CRM()
        server.direction = '<-'

        # Call with serialized TrackedInput
        serialized = TrackedInput.serialize(TrackedInput(val=7))
        err_bytes, result_bytes = server.process(serialized)

        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        # Verify custom deserializer was called (not pickle fallback)
        assert 7 in deserialize_calls
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

### Task 6: Remove `_buffer_mode` from `Transferable` + update server dispatch (atomic)

**Files:**
- Modify: `src/c_two/crm/transferable.py:78-104` (`Transferable` class)
- Modify: `src/c_two/crm/transferable.py:245-268` (`transferable()` decorator)
- Modify: `src/c_two/transport/server/native.py:57-65` (`build_dispatch_table`)
- Modify: `src/c_two/transport/server/native.py:322-395` (dispatch function)
- Modify: `tests/unit/test_transferable.py` (replace type-level buffer tests with method-level tests)

This task is atomic: remove `_buffer_mode` from the type system AND update server dispatch default from `copy` to `view` in the same commit. Before deleting old buffer tests, write replacement tests for method-level `@cc.transfer(buffer=...)`.

- [ ] **Step 1: Write replacement buffer tests using method-level `@cc.transfer(buffer=...)`**

Add to `tests/unit/test_transfer_decorator.py`:

```python
class TestMethodLevelBufferBehavior:
    """Buffer lifecycle is controlled at method level, not type level."""

    def test_view_buffer_releases_after_deserialize(self):
        """With buffer='view' (default), input memoryview is released after CRM method returns."""
        @cc.icrm(namespace='test.buf.view', version='0.1.0')
        class IBufView:
            def process(self, data: bytes) -> str:
                ...

        class CRM:
            def process(self, data):
                return 'ok'

        server = IBufView()
        server.crm = CRM()
        server.direction = '<-'

        import pickle
        err, result = server.process(pickle.dumps(b'hello'))
        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err)) is None

    def test_hold_buffer_declared_at_method(self):
        """buffer='hold' is set via @cc.transfer, not on the type."""
        @cc.icrm(namespace='test.buf.hold', version='0.1.0')
        class IBufHold:
            @transfer(buffer='hold')
            def process(self, data: bytes) -> str:
                ...

        method = getattr(IBufHold, 'process')
        assert getattr(method, '_input_buffer_mode', None) == 'hold'
```

- [ ] **Step 2: Remove `_buffer_mode` from `Transferable` class**

In `Transferable` class (L89), remove:
```python
    _buffer_mode: str = 'copy'  # 'copy' | 'view' | 'hold'
```

- [ ] **Step 3: Remove `buffer` param from `@transferable` decorator**

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

- [ ] **Step 4: Remove `_buffer_mode` assignments in `create_default_transferable`**

In `create_default_transferable` (L192 and L227), delete:
```python
DynamicInputTransferable._buffer_mode = 'view'   # L192 — delete
DynamicOutputTransferable._buffer_mode = 'view'   # L227 — delete
```

- [ ] **Step 5: Update `build_dispatch_table` default**

Change L64 in `src/c_two/transport/server/native.py`:
```python
# Before:
buffer_mode = getattr(method, '_input_buffer_mode', 'copy')
# After:
buffer_mode = getattr(method, '_input_buffer_mode', 'view')
```

- [ ] **Step 6: Remove `copy` branch in dispatch**

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

- [ ] **Step 7: Update old buffer tests in `test_transferable.py`**

Remove or update tests that reference `_buffer_mode` on the type or `@cc.transferable(buffer=...)`.

- [ ] **Step 8: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 9: Commit**

```bash
git add src/c_two/crm/transferable.py src/c_two/transport/server/native.py tests/unit/test_transferable.py tests/unit/test_transfer_decorator.py
git commit -m "refactor: remove _buffer_mode from types, update server dispatch to view default

Buffer lifecycle is now controlled at method level (@cc.transfer) and call
site (cc.hold()), not on the data type. Server dispatch removes copy branch,
defaults to view.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 8: Delete Priority 2 matching + grid example cleanup (atomic)

**Files:**
- Modify: `src/c_two/crm/transferable.py` (delete Priority 2 block in `auto_transfer`)
- Modify: `examples/grid/transferables.py` (delete bundle classes)
- Modify: `examples/grid/igrid.py` (remove bundle imports)
- Modify: `tests/unit/test_transferable.py` (update Priority 2-dependent tests)

Priority 2 deletion and grid cleanup happen atomically — the grid example is the only consumer of Priority 2 matching.

- [ ] **Step 1: Delete Priority 2 matching block in `auto_transfer`**

In `auto_transfer`, remove the Priority 2 block:
```python
                    # Priority 2: field-name matching — DELETE THIS BLOCK
                    if input_transferable is None and len(input_model_fields) > 1:
                        for info in _get_all_transferable_info():
                            registered_fields = info.get('fields', {})
                            if registered_fields and set(input_model_fields.keys()) == set(registered_fields.keys()):
                                expected_input_types = {k: v.annotation for k, v in input_model_fields.items()}
                                if expected_input_types == registered_fields:
                                    input_transferable = get_transferable(info['name'])
                                    break
```

Also delete output Priority 2 matching (the similar block in the output section).

- [ ] **Step 2: Simplify `transferables.py`**

Keep only `GridSchema`, `GridAttribute`, and the helper functions. Delete `GridInfo`, `PeerGridInfos`, `GridInfos`, `GridAttributes`, `GridKeys`.

- [ ] **Step 3: Simplify `igrid.py`**

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

- [ ] **Step 4: Update Priority 2-dependent tests in `test_transferable.py`**

Update `test_matching_transferable_uses_existing` and any other tests that relied on Priority 2 field-name matching:

```python
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
git add src/c_two/crm/transferable.py examples/grid/transferables.py examples/grid/igrid.py tests/unit/test_transferable.py
git commit -m "refactor: delete Priority 2 matching + grid parameter bundles

Priority 2 field-name matching removed from auto_transfer.
Grid example simplified: only domain types (GridSchema, GridAttribute)
remain. All other methods auto-bundle via pickle.

Part of transferable redesign (spec: 2026-07-15)."
```

---

### Task 9: Wire up exports + end-to-end tests

**Files:**
- Modify: `src/c_two/crm/__init__.py`
- Modify: `src/c_two/__init__.py`
- Modify: `tests/unit/test_transfer_decorator.py` (add export + e2e tests)
- Create: `tests/integration/test_transfer_hold.py` (IPC integration)

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

- [ ] **Step 3: Write import verification + unit e2e tests**

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

        server_icrm = IE2E()
        server_icrm.crm = crm
        server_icrm.direction = '<-'

        greet = getattr(server_icrm, 'greet')
        serialized_input = pickle.dumps('World')
        err_bytes, result_bytes = greet(serialized_input)

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

        class FakeCRM:
            def compute(self, x: int) -> int:
                return x * 2

        from c_two.transport.client.proxy import ICRMProxy
        proxy = ICRMProxy.thread_local(FakeCRM())

        client_icrm = IHoldTest()
        client_icrm.client = proxy
        client_icrm.direction = '->'

        result = client_icrm.compute(5)
        assert result == 10

        held = cc.hold(client_icrm.compute)(5)
        assert isinstance(held, HeldResult)
        assert held.value == 10
        held.release()

    def test_c2_buffer_not_leaked_to_crm(self):
        """_c2_buffer is popped by transfer_wrapper, never reaches user CRM methods."""
        received_kwargs = {}

        @cc.icrm(namespace='test.noleak', version='0.1.0')
        class INoLeak:
            def process(self, x: int) -> int:
                ...

        class CRM:
            def process(self, x, **kwargs):
                received_kwargs.update(kwargs)
                return x

        server = INoLeak()
        server.crm = CRM()
        server.direction = '<-'

        import pickle
        err, result = server.process(pickle.dumps(42))
        assert '_c2_buffer' not in received_kwargs
```

- [ ] **Step 4: Write IPC integration test for `cc.hold()`**

Create `tests/integration/test_transfer_hold.py` following the pattern from existing `tests/integration/test_icrm_proxy.py`:

```python
# tests/integration/test_transfer_hold.py
"""IPC integration tests for buffer hold mode and HeldResult lifecycle."""
import pytest
import c_two as cc
from c_two.crm.transferable import HeldResult, transfer

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello


class TestIPCHoldMode:
    """Test cc.hold() through real IPC transport."""

    def test_hold_returns_held_result_ipc(self):
        """cc.hold() on IPC proxy returns HeldResult with valid data."""
        crm = Hello()
        cc.register(IHello, crm, name='hold_test')

        try:
            proxy = cc.connect(IHello, name='hold_test')
            try:
                # Normal call
                result = proxy.hello('World')
                assert result == 'Hello, World!'

                # Hold call
                held = cc.hold(proxy.hello)('World')
                assert isinstance(held, HeldResult)
                assert held.value == 'Hello, World!'
                held.release()

                # After release, value is inaccessible
                with pytest.raises(RuntimeError):
                    _ = held.value
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('hold_test')

    def test_hold_context_manager_ipc(self):
        """HeldResult works as context manager through IPC."""
        crm = Hello()
        cc.register(IHello, crm, name='hold_ctx')

        try:
            proxy = cc.connect(IHello, name='hold_ctx')
            try:
                with cc.hold(proxy.hello)('World') as held:
                    assert held.value == 'Hello, World!'
                # After context exit, released
                with pytest.raises(RuntimeError):
                    _ = held.value
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('hold_ctx')

    def test_view_default_releases_after_call(self):
        """Default view mode: data is accessible as normal return, no HeldResult."""
        crm = Hello()
        cc.register(IHello, crm, name='view_test')

        try:
            proxy = cc.connect(IHello, name='view_test')
            try:
                result = proxy.hello('World')
                assert result == 'Hello, World!'
                assert not isinstance(result, HeldResult)
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('view_test')
```

- [ ] **Step 5: Run all tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add src/c_two/crm/__init__.py src/c_two/__init__.py tests/unit/test_transfer_decorator.py tests/integration/test_transfer_hold.py
git commit -m "feat: wire exports + e2e tests for transferable redesign

Exports cc.transfer, cc.hold, cc.HeldResult to top-level namespace.
Unit e2e: auto-bundle round-trip, thread-local hold, _c2_buffer isolation.
IPC integration: hold/release lifecycle through real transport.

Part of transferable redesign (spec: 2026-07-15)."
```

---

## Notes

- **Task ordering rationale**: Tasks 3+4 merged to avoid breaking auto_transfer when transfer() becomes metadata-only. Tasks 6+7 merged so _buffer_mode removal and server default change happen atomically. Priority 2 deletion and grid cleanup happen together in Task 8 since grid is the only Priority 2 consumer.
- **Backward compatibility**: The grid example is the only consumer of Priority 2 matching and `_buffer_mode` on transferables. No external users are affected.
- **Thread safety**: All new code (HeldResult, hold()) is stateless per-instance. No new global state introduced.
- **`copy` mode removal**: The `copy` branch in both `_build_transfer_wrapper` and `native.py` dispatch is deleted. `view` becomes the universal default.
- **`HeldResult.__del__` safety**: `sys.is_finalizing` and `warnings.warn` are bound as default args at class definition time to survive late interpreter teardown.
- **Testing strategy**: Each task includes its own tests. Task 9 provides both unit e2e and real IPC integration tests. The full suite (553+ tests) is re-run after each task.
- **Early tests import `transfer` from `c_two.crm.transferable` directly** — the `cc.transfer` export is only wired in Task 9 (exports).
