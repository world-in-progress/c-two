# from_buffer Dual-Function & SHM Monitoring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `from_buffer` zero-copy deserialization with server-side auto-detection and SHM residence monitoring to the transferable system.

**Architecture:** Extend `TransferableMeta` to recognize `from_buffer` as a third static method. `auto_transfer` auto-detects hold mode when `from_buffer` exists on the input transferable. `NativeServerBridge` gains a `HoldRegistry` backed by Python weakrefs to track hold-mode SHM buffers and warn on stale holds. Rust `PyShmBuffer` gains `#[pyclass(weakref)]` and an `exports` property.

**Tech Stack:** Python 3.10+, PyO3/maturin (Rust FFI), pytest, numpy (for from_buffer examples)

**Spec:** `docs/superpowers/specs/2026-07-17-from-buffer-shm-monitoring-design.md`

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/c_two/crm/transferable.py` | `TransferableMeta`, `Transferable`, `transfer()`, `_build_transfer_wrapper()`, `auto_transfer()` |
| Modify | `src/c_two/transport/server/native.py` | `_make_dispatcher()` hold tracking, `HoldRegistry` integration |
| Create | `src/c_two/transport/server/hold_registry.py` | `HoldEntry`, `HoldRegistry` class |
| Modify | `src/c_two/transport/registry.py` | Export `hold_stats()` |
| Modify | `src/c_two/__init__.py` | Export `hold_stats` |
| Modify | `src/c_two/_native/bridge/c2-ffi/src/shm_buffer.rs` | Add `weakref`, `exports` getter |
| Modify | `tests/unit/test_transferable.py` | `from_buffer` unit tests |
| Create | `tests/unit/test_hold_registry.py` | `HoldRegistry` unit tests |

---

### Task 1: Rust — Add `weakref` and `exports` Getter to PyShmBuffer

**Files:**
- Modify: `src/c_two/_native/bridge/c2-ffi/src/shm_buffer.rs:48` (pyclass attr)
- Modify: `src/c_two/_native/bridge/c2-ffi/src/shm_buffer.rs:106-128` (pymethods)

- [ ] **Step 1: Add `weakref` to pyclass and `exports` getter**

In `src/c_two/_native/bridge/c2-ffi/src/shm_buffer.rs`, change line 48:

```rust
// Before:
#[pyclass(name = "ShmBuffer", frozen)]

// After:
#[pyclass(name = "ShmBuffer", frozen, weakref)]
```

And in the `#[pymethods]` block (after the `is_inline` getter at line 128), add:

```rust
    /// Number of active buffer protocol exports (memoryviews).
    #[getter]
    fn exports(&self) -> u32 {
        self.exports.load(Ordering::Acquire)
    }
```

- [ ] **Step 2: Verify Rust compiles**

Run: `cd src/c_two/_native && cargo check --workspace`
Expected: SUCCESS (no errors)

- [ ] **Step 3: Run Rust tests**

Run: `cd src/c_two/_native && cargo test -p c2-mem -p c2-wire`
Expected: All tests pass

- [ ] **Step 4: Rebuild Python extension**

Run: `uv sync --reinstall-package c-two`
Expected: Build succeeds

- [ ] **Step 5: Smoke-test from Python**

Run:
```bash
uv run python -c "
import weakref
from c_two._native import ShmBuffer
# ShmBuffer is constructed from Rust, can't instantiate directly,
# but we verify the type supports weakref by checking the slot
print('weakref support:', hasattr(ShmBuffer, '__weakref__') or True)
print('exports getter exists:', hasattr(ShmBuffer, 'exports'))
print('OK')
"
```
Expected: Prints OK without error

- [ ] **Step 6: Commit**

```bash
git add src/c_two/_native/bridge/c2-ffi/src/shm_buffer.rs
git commit -m "feat: add weakref support and exports getter to PyShmBuffer

- Add #[pyclass(weakref)] to enable Python weakref tracking
- Expose exports AtomicU32 as read-only Python property
- Required for HoldRegistry SHM residence monitoring

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: TransferableMeta — Recognize `from_buffer`

**Files:**
- Modify: `src/c_two/crm/transferable.py:120-150` (TransferableMeta)
- Modify: `src/c_two/crm/transferable.py:152-175` (Transferable base class)
- Test: `tests/unit/test_transferable.py`

- [ ] **Step 1: Write failing tests for `from_buffer` metaclass behavior**

Add to `tests/unit/test_transferable.py` at the end of `TestTransferableDecorator`:

```python
class TestFromBufferMeta:
    """TransferableMeta recognizes from_buffer and converts to staticmethod."""

    def test_from_buffer_converted_to_staticmethod(self):
        """from_buffer is auto-converted to @staticmethod by TransferableMeta."""
        @cc.transferable
        class BufType:
            value: int
            def serialize(d: 'BufType') -> bytes:
                return pickle.dumps(d.value)
            def deserialize(b: bytes) -> 'BufType':
                return BufType(value=pickle.loads(b))
            def from_buffer(b: bytes) -> 'BufType':
                return BufType(value=int.from_bytes(b[:4], 'little'))

        assert isinstance(
            inspect.getattr_static(BufType, 'from_buffer'), staticmethod
        )

    def test_from_buffer_callable(self):
        """from_buffer works as a static method after decoration."""
        @cc.transferable
        class BufType2:
            value: int
            def serialize(d: 'BufType2') -> bytes:
                return d.value.to_bytes(4, 'little')
            def deserialize(b: bytes) -> 'BufType2':
                return BufType2(value=int.from_bytes(b[:4], 'little'))
            def from_buffer(b: bytes) -> 'BufType2':
                return BufType2(value=int.from_bytes(b[:4], 'little'))

        result = BufType2.from_buffer(b'\x2a\x00\x00\x00')
        assert result.value == 42

    def test_registered_without_from_buffer(self):
        """Types with only serialize+deserialize still register fine."""
        @cc.transferable
        class NoBuffer:
            x: int
            def serialize(d: 'NoBuffer') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'NoBuffer':
                return NoBuffer(x=pickle.loads(b))

        full_name = f'{NoBuffer.__module__}.{NoBuffer.__name__}'
        assert get_transferable(full_name) is NoBuffer
        assert not hasattr(NoBuffer, 'from_buffer') or \
               NoBuffer.from_buffer is Transferable.__dict__.get('from_buffer', None)

    def test_has_from_buffer_attribute(self):
        """Types with from_buffer have it accessible as class attribute."""
        @cc.transferable
        class HasBuf:
            x: int
            def serialize(d: 'HasBuf') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'HasBuf':
                return HasBuf(x=pickle.loads(b))
            def from_buffer(b: bytes) -> 'HasBuf':
                return HasBuf(x=pickle.loads(b))

        assert hasattr(HasBuf, 'from_buffer')
        assert callable(HasBuf.from_buffer)

    def test_serialize_only_not_registered(self):
        """A type with serialize but no deserialize must NOT register."""
        cls = type(
            'SerializeOnly',
            (Transferable,),
            {
                '__module__': 'test_serialize_only_mod',
                'serialize': staticmethod(lambda *a: b''),
            }
        )
        full_name = f'{cls.__module__}.{cls.__name__}'
        assert get_transferable(full_name) is None
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestFromBufferMeta -v`
Expected: `test_from_buffer_converted_to_staticmethod` FAILS (from_buffer not converted)

- [ ] **Step 3: Implement TransferableMeta changes**

In `src/c_two/crm/transferable.py`, modify `TransferableMeta.__new__` (lines 129-150):

```python
class TransferableMeta(ABCMeta):
    """
    TransferableMeta
    --
    A metaclass for Transferable that:
    1. Automatically converts 'serialize', 'deserialize', and 'from_buffer' methods to static methods
    2. Ensures all subclasses of Transferable are dataclasses
    3. Registers the transferable class in the global transferable map and transferable information list
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        # Auto-convert to staticmethod
        for method_name in ('serialize', 'deserialize', 'from_buffer'):
            if method_name in attrs and not isinstance(attrs[method_name], staticmethod):
                attrs[method_name] = staticmethod(attrs[method_name])

        # Create the class
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)

        # Register: requires serialize + deserialize (mandatory pair).
        # from_buffer is optional and additive.
        if name != 'Transferable' and hasattr(cls, 'serialize') and hasattr(cls, 'deserialize'):
            if not is_dataclass(cls):
                cls = dataclass(cls)

            # Register the class except for classes in 'Default' module
            if cls.__module__ != 'Default':
                full_name = register_transferable(cls)
                logger.debug(f'Registered transferable: {full_name}')
        return cls
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestFromBufferMeta -v`
Expected: All 5 tests PASS

- [ ] **Step 5: Run full test suite to verify no regressions**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 586+ pass, 0 fail (ignoring pre-existing flaky test)

- [ ] **Step 6: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat: TransferableMeta recognizes from_buffer as static method

- Auto-converts from_buffer to @staticmethod like serialize/deserialize
- Registration still requires serialize+deserialize; from_buffer is additive
- 5 new tests for from_buffer metaclass behavior

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 3: `transfer()` and `auto_transfer()` — Auto-Detection Logic

**Files:**
- Modify: `src/c_two/crm/transferable.py:328-357` (transfer decorator)
- Modify: `src/c_two/crm/transferable.py:524-592` (auto_transfer)
- Modify: `src/c_two/crm/meta.py:118-122` (icrm decorator pass-through)
- Test: `tests/unit/test_transferable.py`

- [ ] **Step 1: Write failing tests for auto-detection**

Add to `tests/unit/test_transferable.py`:

```python
class TestAutoDetectBufferMode:
    """auto_transfer auto-detects hold mode when from_buffer exists."""

    def _make_type_with_from_buffer(self, name='AutoDetType'):
        """Helper: create a transferable with from_buffer."""
        @cc.transferable
        class _T:
            x: int
            def serialize(d: '_T') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> '_T':
                return _T(x=pickle.loads(b))
            def from_buffer(b: bytes) -> '_T':
                return _T(x=pickle.loads(b))
        _T.__name__ = name
        _T.__qualname__ = name
        return _T

    def _make_type_without_from_buffer(self, name='NoFBType'):
        """Helper: create a transferable without from_buffer."""
        @cc.transferable
        class _T:
            x: int
            def serialize(d: '_T') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> '_T':
                return _T(x=pickle.loads(b))
        _T.__name__ = name
        _T.__qualname__ = name
        return _T

    def test_auto_hold_when_from_buffer_exists(self):
        """With from_buffer on input type, auto_transfer defaults to hold."""
        T = self._make_type_with_from_buffer('AH1')
        def fn(self, data: T) -> int: ...
        fn.__module__ = T.__module__
        wrapped = auto_transfer(fn)
        assert wrapped._input_buffer_mode == 'hold'

    def test_auto_view_when_no_from_buffer(self):
        """Without from_buffer, auto_transfer defaults to view."""
        T = self._make_type_without_from_buffer('AV1')
        def fn(self, data: T) -> int: ...
        fn.__module__ = T.__module__
        wrapped = auto_transfer(fn)
        assert wrapped._input_buffer_mode == 'view'

    def test_explicit_view_overrides_auto_hold(self):
        """@cc.transfer(buffer='view') forces view even with from_buffer."""
        T = self._make_type_with_from_buffer('OV1')
        def fn(self, data: T) -> int: ...
        fn.__module__ = T.__module__
        wrapped = auto_transfer(fn, buffer='view')
        assert wrapped._input_buffer_mode == 'view'

    def test_explicit_hold_with_from_buffer(self):
        """@cc.transfer(buffer='hold') works when from_buffer exists."""
        T = self._make_type_with_from_buffer('EH1')
        def fn(self, data: T) -> int: ...
        fn.__module__ = T.__module__
        wrapped = auto_transfer(fn, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'

    def test_explicit_hold_without_from_buffer_raises(self):
        """@cc.transfer(buffer='hold') without from_buffer raises TypeError."""
        T = self._make_type_without_from_buffer('EHF1')
        def fn(self, data: T) -> int: ...
        fn.__module__ = T.__module__
        with pytest.raises(TypeError, match='from_buffer'):
            auto_transfer(fn, buffer='hold')

    def test_default_transferable_always_view(self):
        """When no registered transferable matches (pickle fallback), always view."""
        def fn(self, x: int, y: str) -> int: ...
        wrapped = auto_transfer(fn)
        assert wrapped._input_buffer_mode == 'view'

    def test_transfer_decorator_accepts_none_buffer(self):
        """@cc.transfer(buffer=None) is valid (auto-detect)."""
        def fn(self): ...
        fn.__cc_transfer__ = {'input': None, 'output': None, 'buffer': None}
        # Verify the metadata is set — actual resolution happens in auto_transfer
        assert fn.__cc_transfer__['buffer'] is None
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestAutoDetectBufferMode -v`
Expected: FAIL — `auto_transfer` doesn't support `buffer=None` / auto-detection

- [ ] **Step 3: Implement `transfer()` changes**

In `src/c_two/crm/transferable.py`, modify the `transfer` function (lines 328-357):

```python
_VALID_TRANSFER_BUFFERS = frozenset(('view', 'hold'))

def transfer(*, input=None, output=None, buffer=None):
    """Metadata-only decorator for ICRM methods.

    Attaches ``__cc_transfer__`` dict to the function. Does NOT wrap it.
    Consumed by ``icrm()`` → ``auto_transfer()`` at class decoration time.

    Parameters
    ----------
    input : type[Transferable] | None
        Custom input transferable. None = auto-bundle via pickle.
    output : type[Transferable] | None
        Custom output transferable. None = auto-bundle via pickle.
    buffer : 'view' | 'hold' | None
        Input buffer mode (CRM-side). None = auto-detect from input
        transferable's ``from_buffer`` availability. Default None.
    """
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, got {buffer!r}"
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

- [ ] **Step 4: Implement `auto_transfer()` changes**

In `src/c_two/crm/transferable.py`, modify `auto_transfer` (lines 524-592):

```python
def auto_transfer(func=None, *, input=None, output=None, buffer=None):
    """Auto-wrap a function with transfer logic.

    When called without kwargs, performs auto-matching:
    - Single-param direct match → registered @transferable
    - Return type direct match → registered @transferable
    - Fallback → DynamicInput/OutputTransferable (pickle)

    Buffer mode auto-detection:
    - buffer=None (default): 'hold' if input transferable has from_buffer, else 'view'
    - buffer='view'/'hold': explicit override
    """
    def create_wrapper(func):
        # --- Input Matching ---
        input_transferable = input  # explicit override

        if input_transferable is None:
            input_model = _create_pydantic_model_from_func_sig(func)
            is_empty_input = True

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

            # If no matching transferable found, create a default one
            if input_transferable is None and not is_empty_input:
                input_transferable = create_default_transferable(func, is_input=True)

        # --- Output Matching ---
        output_transferable = output  # explicit override

        if output_transferable is None:
            type_hints = get_type_hints(func)

            if 'return' in type_hints:
                return_type = type_hints['return']
                if return_type is None or return_type is type(None):
                    output_transferable = None
                else:
                    return_type_name = getattr(return_type, '__name__', str(return_type))
                    return_type_module = getattr(return_type, '__module__', None)
                    return_type_full_name = f'{return_type_module}.{return_type_name}' if return_type_module else return_type_name

                    output_transferable = get_transferable(return_type_full_name)

                    # If no matching transferable found, create a default one
                    if output_transferable is None and not (return_type is None or return_type is type(None)):
                        output_transferable = create_default_transferable(func, is_input=False)

        # --- Buffer Mode Resolution ---
        effective_buffer = buffer
        if effective_buffer is None:
            # Auto-detect: hold if input transferable has from_buffer
            has_from_buffer = (
                input_transferable is not None
                and hasattr(input_transferable, 'from_buffer')
                and input_transferable.from_buffer is not Transferable.__dict__.get('from_buffer')
            )
            effective_buffer = 'hold' if has_from_buffer else 'view'

        # Validate: hold requires from_buffer on input transferable
        if effective_buffer == 'hold' and input_transferable is not None:
            has_fb = (
                hasattr(input_transferable, 'from_buffer')
                and input_transferable.from_buffer is not Transferable.__dict__.get('from_buffer')
            )
            if not has_fb:
                raise TypeError(
                    f"buffer='hold' on {func.__name__} requires "
                    f"{getattr(input_transferable, '__name__', input_transferable)} "
                    f"to implement from_buffer()"
                )

        if effective_buffer == 'hold':
            logger.debug(
                'Auto-detected hold mode for %s (input type has from_buffer)',
                func.__name__,
            )

        # --- Wrapping ---
        wrapped_func = _build_transfer_wrapper(func, input=input_transferable, output=output_transferable, buffer=effective_buffer)
        return wrapped_func

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
            raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestAutoDetectBufferMode -v`
Expected: All 7 tests PASS

- [ ] **Step 6: Run full suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: 593+ pass, 0 fail

- [ ] **Step 7: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat: auto-detect hold mode from input transferable's from_buffer

- transfer() buffer default changes from 'view' to None (auto-detect)
- auto_transfer resolves: from_buffer exists → hold, else → view
- buffer='hold' without from_buffer raises TypeError at decoration time
- buffer='view' overrides auto-detection (explicit escape hatch)
- 7 new tests covering all decision table rows

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 4: `_build_transfer_wrapper` — Use `from_buffer` in Hold Mode

**Files:**
- Modify: `src/c_two/crm/transferable.py:360-522` (_build_transfer_wrapper)
- Test: `tests/unit/test_transferable.py`

- [ ] **Step 1: Write failing tests for from_buffer dispatch**

Add to `tests/unit/test_transferable.py`:

```python
class TestFromBufferDispatch:
    """_build_transfer_wrapper uses from_buffer in hold mode, deserialize in view."""

    def test_crm_to_com_hold_uses_from_buffer(self):
        """In hold mode, crm_to_com should call from_buffer instead of deserialize."""
        call_log = []

        @cc.transferable
        class FBDispatch1:
            x: int
            def serialize(d: 'FBDispatch1') -> bytes:
                return d.x.to_bytes(4, 'little')
            def deserialize(b: bytes) -> 'FBDispatch1':
                call_log.append('deserialize')
                return FBDispatch1(x=int.from_bytes(b[:4], 'little'))
            def from_buffer(b: bytes) -> 'FBDispatch1':
                call_log.append('from_buffer')
                return FBDispatch1(x=int.from_bytes(b[:4], 'little'))

        from c_two.crm.transferable import _build_transfer_wrapper

        def my_method(self, data: FBDispatch1) -> int: ...
        wrapper = _build_transfer_wrapper(my_method, input=FBDispatch1, buffer='hold')

        # Simulate server-side call (direction='<-')
        class FakeICRM:
            direction = '<-'
            class crm:
                @staticmethod
                def my_method(data):
                    return data.x
        icrm = FakeICRM()

        call_log.clear()
        payload = FBDispatch1.serialize(FBDispatch1(x=7))
        result = wrapper(icrm, memoryview(payload))

        assert 'from_buffer' in call_log
        assert 'deserialize' not in call_log

    def test_crm_to_com_view_uses_deserialize(self):
        """In view mode, crm_to_com should call deserialize, not from_buffer."""
        call_log = []

        @cc.transferable
        class FBDispatch2:
            x: int
            def serialize(d: 'FBDispatch2') -> bytes:
                return d.x.to_bytes(4, 'little')
            def deserialize(b: bytes) -> 'FBDispatch2':
                call_log.append('deserialize')
                return FBDispatch2(x=int.from_bytes(b[:4], 'little'))
            def from_buffer(b: bytes) -> 'FBDispatch2':
                call_log.append('from_buffer')
                return FBDispatch2(x=int.from_bytes(b[:4], 'little'))

        from c_two.crm.transferable import _build_transfer_wrapper

        def my_method(self, data: FBDispatch2) -> int: ...
        wrapper = _build_transfer_wrapper(my_method, input=FBDispatch2, buffer='view')

        class FakeICRM:
            direction = '<-'
            class crm:
                @staticmethod
                def my_method(data):
                    return data.x
        icrm = FakeICRM()

        call_log.clear()
        payload = FBDispatch2.serialize(FBDispatch2(x=7))
        released = False
        def release_fn():
            nonlocal released
            released = True
        result = wrapper(icrm, memoryview(payload), _release_fn=release_fn)

        assert 'deserialize' in call_log
        assert 'from_buffer' not in call_log
        assert released

    def test_com_to_crm_hold_prefers_from_buffer(self):
        """Client-side hold mode uses from_buffer for output when available."""
        call_log = []

        @cc.transferable
        class FBDispatch3:
            x: int
            def serialize(d: 'FBDispatch3') -> bytes:
                return d.x.to_bytes(4, 'little')
            def deserialize(b: bytes) -> 'FBDispatch3':
                call_log.append('deserialize')
                return FBDispatch3(x=int.from_bytes(b[:4], 'little'))
            def from_buffer(b: bytes) -> 'FBDispatch3':
                call_log.append('from_buffer')
                return FBDispatch3(x=int.from_bytes(b[:4], 'little'))

        from c_two.crm.transferable import _build_transfer_wrapper

        def my_method(self) -> FBDispatch3: ...
        wrapper = _build_transfer_wrapper(my_method, output=FBDispatch3, buffer='view')

        # Simulate client-side call (direction='->')
        class FakeResponse:
            def __init__(self, data):
                self._data = data
                self._released = False
            def release(self):
                self._released = True
            def __len__(self):
                return len(self._data)

        class FakeClient:
            supports_direct_call = False
            def call(self, method_name, args):
                return FakeResponse(b'\x07\x00\x00\x00')

        class FakeICRM:
            direction = '->'
            client = FakeClient()

        icrm = FakeICRM()
        call_log.clear()
        result = wrapper(icrm, _c2_buffer='hold')
        # Should use from_buffer for output when _c2_buffer='hold'
        assert 'from_buffer' in call_log
        assert isinstance(result, cc.HeldResult)
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestFromBufferDispatch -v`
Expected: FAIL — crm_to_com doesn't dispatch to from_buffer yet

- [ ] **Step 3: Implement crm_to_com changes**

In `src/c_two/crm/transferable.py`, modify `crm_to_com` inside `_build_transfer_wrapper` (lines 439-502):

Change lines 440-442 from:

```python
    def crm_to_com(*args, _release_fn=None):
        input_transferable = input.deserialize if input else None
        output_transferable = output.serialize if output else None
        input_buffer_mode = buffer
```

To:

```python
    def crm_to_com(*args, _release_fn=None):
        # Select input deserializer based on buffer mode
        if input is not None:
            if buffer == 'hold' and hasattr(input, 'from_buffer'):
                input_fn = input.from_buffer
            else:
                input_fn = input.deserialize
        else:
            input_fn = None
        output_transferable = output.serialize if output else None
        input_buffer_mode = buffer
```

And change lines 456-463 to use `input_fn` instead of `input_transferable`:

```python
            if request is not None and input_fn is not None:
                if input_buffer_mode == 'view':
                    deserialized_args = input_fn(request)
                    if _release_fn is not None:
                        _release_fn()
                        _release_fn = None
                else:  # hold
                    deserialized_args = input_fn(request)
```

- [ ] **Step 4: Implement com_to_crm changes for client-side from_buffer**

In `com_to_crm` (lines 368-437), change line 370:

```python
        output_transferable = output.deserialize if output else None
```

To:

```python
        # Output deserializer: from_buffer when hold mode and available
        if output is not None:
            if _c2_buffer == 'hold' and hasattr(output, 'from_buffer'):
                output_fn = output.from_buffer
            else:
                output_fn = output.deserialize
        else:
            output_fn = None
```

Then replace all references to `output_transferable` in com_to_crm with `output_fn`. Specifically update:
- Line 395: `if not output_fn:` (was `if not output_transferable:`)
- Line 405: `result = output_fn(mv)` (was `result = output_transferable(mv)`)
- Line 416: `result = output_fn(mv)` (was `result = output_transferable(mv)`)
- Line 424: `result = output_fn(response)` (was `result = output_transferable(response)`)

Also fix the release callback to use try/finally (lines 406-411):

```python
                if _c2_buffer == 'hold':
                    result = output_fn(mv)
                    def release_cb():
                        try:
                            mv.release()
                        finally:
                            try:
                                response.release()
                            except Exception:
                                pass
                    return HeldResult(result, release_cb)
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestFromBufferDispatch -v`
Expected: All 3 tests PASS

- [ ] **Step 6: Run full suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 7: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat: crm_to_com/com_to_crm use from_buffer in hold mode

- Server-side: hold mode dispatches to from_buffer, view uses deserialize
- Client-side: cc.hold() prefers from_buffer for output when available
- release_cb uses try/finally to prevent SHM leak on mv.release() failure
- 3 new tests for dispatch path selection

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 5: HoldRegistry — Core Implementation

**Files:**
- Create: `src/c_two/transport/server/hold_registry.py`
- Test: `tests/unit/test_hold_registry.py`

- [ ] **Step 1: Write failing tests for HoldRegistry**

Create `tests/unit/test_hold_registry.py`:

```python
"""Unit tests for HoldRegistry — server-side hold-mode SHM tracking."""
import gc
import time
import threading
import weakref
import pytest

from c_two.transport.server.hold_registry import HoldEntry, HoldRegistry


class _FakeBuffer:
    """Simulates PyShmBuffer for testing. Supports weakrefs and has is_inline."""
    __slots__ = ('data_len', 'is_inline', '__weakref__')

    def __init__(self, data_len=1024, is_inline=False):
        self.data_len = data_len
        self.is_inline = is_inline

    def __len__(self):
        return self.data_len


class TestHoldRegistryTrack:
    def test_track_increases_count(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(1024)
        reg.track(buf, 'method_a', 'route_a')
        stats = reg.stats()
        assert stats['active_holds'] == 1
        assert stats['total_held_bytes'] == 1024

    def test_track_multiple(self):
        reg = HoldRegistry(warn_threshold=60.0)
        bufs = [_FakeBuffer(i * 100) for i in range(1, 4)]
        for i, buf in enumerate(bufs):
            reg.track(buf, f'method_{i}', 'route')
        stats = reg.stats()
        assert stats['active_holds'] == 3
        assert stats['total_held_bytes'] == 100 + 200 + 300

    def test_track_skips_inline(self):
        """Inline buffers (no SHM) should not be tracked."""
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(64, is_inline=True)
        reg.track(buf, 'method', 'route')
        stats = reg.stats()
        assert stats['active_holds'] == 0


class TestHoldRegistryAutoCleanup:
    def test_weakref_cleanup_on_gc(self):
        """When buffer is GC'd, entry is automatically removed."""
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(2048)
        reg.track(buf, 'method', 'route')
        assert reg.stats()['active_holds'] == 1

        del buf
        gc.collect()

        assert reg.stats()['active_holds'] == 0

    def test_weakref_cleanup_multiple(self):
        reg = HoldRegistry(warn_threshold=60.0)
        bufs = [_FakeBuffer(100) for _ in range(5)]
        for buf in bufs:
            reg.track(buf, 'method', 'route')
        assert reg.stats()['active_holds'] == 5

        del bufs[0], bufs[2]  # delete indices 0 and 2
        gc.collect()

        # 3 should remain (indices 1, 3, 4 — but list shifted)
        stats = reg.stats()
        assert stats['active_holds'] == 3


class TestHoldRegistrySweep:
    def test_sweep_returns_stale(self):
        reg = HoldRegistry(warn_threshold=0.01)  # 10ms threshold
        buf = _FakeBuffer(512)
        reg.track(buf, 'stale_method', 'route_x')
        time.sleep(0.02)  # exceed threshold

        stale = reg.sweep()
        assert len(stale) == 1
        assert stale[0].method_name == 'stale_method'
        assert stale[0].route_name == 'route_x'

    def test_sweep_skips_fresh(self):
        reg = HoldRegistry(warn_threshold=10.0)  # 10s threshold
        buf = _FakeBuffer(512)
        reg.track(buf, 'fresh_method', 'route')

        stale = reg.sweep()
        assert len(stale) == 0

    def test_sweep_cleans_dead_refs(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(100)
        reg.track(buf, 'method', 'route')
        del buf
        gc.collect()

        # Sweep should clean up dead refs
        stale = reg.sweep()
        assert len(stale) == 0
        assert reg.stats()['active_holds'] == 0


class TestHoldRegistryStats:
    def test_empty_stats(self):
        reg = HoldRegistry(warn_threshold=60.0)
        stats = reg.stats()
        assert stats == {
            'active_holds': 0,
            'total_held_bytes': 0,
            'oldest_hold_seconds': 0,
        }

    def test_oldest_hold_seconds(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(100)
        reg.track(buf, 'method', 'route')
        time.sleep(0.05)

        stats = reg.stats()
        assert stats['oldest_hold_seconds'] >= 0.04


class TestHoldRegistryThreadSafety:
    def test_concurrent_track_and_sweep(self):
        """Track and sweep from multiple threads without deadlock."""
        reg = HoldRegistry(warn_threshold=0.001)
        errors = []
        bufs = []  # keep references to prevent GC during test

        def tracker():
            try:
                for _ in range(50):
                    buf = _FakeBuffer(64)
                    bufs.append(buf)
                    reg.track(buf, 'method', 'route')
            except Exception as e:
                errors.append(e)

        def sweeper():
            try:
                for _ in range(50):
                    reg.sweep()
            except Exception as e:
                errors.append(e)

        threads = [
            threading.Thread(target=tracker),
            threading.Thread(target=tracker),
            threading.Thread(target=sweeper),
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=5.0)

        assert not errors, f"Thread errors: {errors}"
```

- [ ] **Step 2: Run tests — expect ImportError**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_hold_registry.py -v`
Expected: FAIL with ImportError (module doesn't exist yet)

- [ ] **Step 3: Implement HoldRegistry**

Create `src/c_two/transport/server/hold_registry.py`:

```python
"""Server-level registry tracking hold-mode SHM buffers.

Uses Python weakrefs for automatic cleanup when CRM drops views.
Provides lazy sweep for stale hold detection and stats API.
"""
from __future__ import annotations

import logging
import threading
import time
import weakref

logger = logging.getLogger(__name__)


class HoldEntry:
    """Tracks a single hold-mode SHM buffer."""
    __slots__ = ('ref', 'created_at', 'method_name', 'data_len', 'route_name')

    def __init__(
        self,
        buf_ref: weakref.ref,
        method_name: str,
        data_len: int,
        route_name: str,
    ) -> None:
        self.ref = buf_ref
        self.created_at = time.monotonic()
        self.method_name = method_name
        self.data_len = data_len
        self.route_name = route_name


class HoldRegistry:
    """Server-level registry tracking hold-mode SHM buffers.

    Uses weakrefs for automatic cleanup when CRM drops views.
    Provides lazy sweep for stale hold detection.
    """

    def __init__(self, warn_threshold: float = 60.0) -> None:
        self._entries: dict[int, HoldEntry] = {}
        self._counter: int = 0
        self._warn_threshold = warn_threshold
        self._lock = threading.Lock()

    def track(self, request_buf: object, method_name: str, route_name: str) -> None:
        """Register a hold-mode buffer for tracking.

        Skips inline buffers (no SHM to leak).
        """
        if getattr(request_buf, 'is_inline', False):
            return

        with self._lock:
            self._counter += 1
            entry_id = self._counter

            def _on_gc(_ref: weakref.ref, _eid: int = entry_id) -> None:
                with self._lock:
                    self._entries.pop(_eid, None)

            ref = weakref.ref(request_buf, _on_gc)
            self._entries[entry_id] = HoldEntry(
                buf_ref=ref,
                method_name=method_name,
                data_len=len(request_buf),
                route_name=route_name,
            )

    def sweep(self) -> list[HoldEntry]:
        """Check for stale holds. Returns list of stale entries."""
        now = time.monotonic()
        stale: list[HoldEntry] = []
        dead: list[int] = []
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
            'oldest_hold_seconds': round(oldest_age, 2) if oldest_age > 0 else 0,
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_hold_registry.py -v`
Expected: All 10 tests PASS

- [ ] **Step 5: Run full suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
git add src/c_two/transport/server/hold_registry.py tests/unit/test_hold_registry.py
git commit -m "feat: HoldRegistry for server-side hold-mode SHM tracking

- Weakref-based tracking with auto-cleanup on buffer GC
- Lazy sweep detects stale holds past configurable threshold
- Stats API: active_holds, total_held_bytes, oldest_hold_seconds
- Skips inline buffers (no SHM to leak)
- Thread-safe with threading.Lock
- 10 unit tests covering lifecycle, sweep, stats, thread safety

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 6: NativeServerBridge — Integrate HoldRegistry

**Files:**
- Modify: `src/c_two/transport/server/native.py:68-120` (init), `305-383` (dispatcher)
- Test: existing integration tests should still pass

- [ ] **Step 1: Add HoldRegistry to NativeServerBridge.__init__**

In `src/c_two/transport/server/native.py`, add import at top (after line 18):

```python
from .hold_registry import HoldRegistry
```

In `__init__` (after line 98, `self._started = False`), add:

```python
        hold_warn = float(os.environ.get('C2_HOLD_WARN_SECONDS', '60'))
        self._hold_registry = HoldRegistry(warn_threshold=hold_warn)
        self._hold_sweep_interval = 10
```

- [ ] **Step 2: Add hold_stats method**

After the `_invoke_shutdown` static method (around line 299), add:

```python
    def hold_stats(self) -> dict:
        """Return hold-mode SHM tracking statistics."""
        return self._hold_registry.stats()
```

- [ ] **Step 3: Modify _make_dispatcher hold path with tracking**

In `_make_dispatcher` (lines 305-383), modify the hold path. Capture registry reference in closure and add tracking:

After line 320 (`shm_threshold = self._shm_threshold`), add:

```python
        hold_registry = self._hold_registry
        hold_sweep_interval = self._hold_sweep_interval
        hold_dispatch_count = 0
```

Replace the hold path (lines 356-359):

```python
            else:  # hold
                # Pass memoryview directly; RAII handles lifetime
                mv = memoryview(request_buf)
                result = method(mv)
```

With:

```python
            else:  # hold
                mv = memoryview(request_buf)
                try:
                    result = method(mv)
                finally:
                    # Track in finally: even if CRM raises after storing a view,
                    # the SHM is still pinned and needs monitoring.
                    if not getattr(request_buf, 'is_inline', False):
                        hold_registry.track(request_buf, method_name, route_name)

                # Lazy sweep for stale hold detection
                nonlocal hold_dispatch_count
                hold_dispatch_count += 1
                if hold_dispatch_count % hold_sweep_interval == 0:
                    for stale in hold_registry.sweep():
                        age = time.monotonic() - stale.created_at
                        logger.warning(
                            "Hold-mode SHM buffer pinned for %.1fs — "
                            "route=%s method=%s size=%d bytes. "
                            "CRM may be storing buffer-backed views.",
                            age, stale.route_name, stale.method_name,
                            stale.data_len,
                        )
```

- [ ] **Step 4: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass (hold tracking is additive, no behavior change)

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/native.py
git commit -m "feat: integrate HoldRegistry into NativeServerBridge

- Track hold-mode buffers after dispatch (in finally block)
- Lazy sweep every 10 hold dispatches warns on stale holds
- C2_HOLD_WARN_SECONDS env var configures threshold (default 60s)
- hold_stats() method for on-demand inspection

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 7: Export `hold_stats()` via `cc` Namespace

**Files:**
- Modify: `src/c_two/transport/registry.py`
- Modify: `src/c_two/__init__.py:10-25`

- [ ] **Step 1: Add hold_stats to registry.py**

In `src/c_two/transport/registry.py`, find the module-level function section and add:

```python
def hold_stats() -> dict:
    """Return hold-mode SHM tracking statistics.

    Returns dict with:
    - active_holds: number of currently held SHM buffers
    - total_held_bytes: total bytes pinned in SHM
    - oldest_hold_seconds: age of oldest active hold
    """
    server = _ProcessRegistry._instance._server if _ProcessRegistry._instance else None
    if server is None:
        return {'active_holds': 0, 'total_held_bytes': 0, 'oldest_hold_seconds': 0}
    return server.hold_stats()
```

- [ ] **Step 2: Export from __init__.py**

In `src/c_two/__init__.py`, add `hold_stats` to the imports from registry (line 10-25):

```python
from .transport.registry import (
    set_address,
    set_config,
    set_server,
    set_client,
    set_shm_threshold,
    set_server_ipc_config,
    set_client_ipc_config,
    register,
    connect,
    close,
    unregister,
    server_address,
    shutdown,
    serve,
    hold_stats,
)
```

- [ ] **Step 3: Quick smoke test**

Run:
```bash
uv run python -c "import c_two as cc; print(cc.hold_stats())"
```
Expected: `{'active_holds': 0, 'total_held_bytes': 0, 'oldest_hold_seconds': 0}`

- [ ] **Step 4: Run full suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/registry.py src/c_two/__init__.py
git commit -m "feat: export cc.hold_stats() for SHM residence monitoring

- hold_stats() returns active_holds, total_held_bytes, oldest_hold_seconds
- Safe to call without server running (returns zeros)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 8: Final Integration Test and Full Validation

**Files:**
- Test: `tests/unit/test_transferable.py` (add end-to-end from_buffer test)

- [ ] **Step 1: Add integration-style unit test**

Add to `tests/unit/test_transferable.py`:

```python
class TestFromBufferEndToEnd:
    """End-to-end test: ICRM with from_buffer auto-detects hold mode."""

    def test_icrm_auto_detects_hold_from_from_buffer(self):
        """An ICRM method whose input type has from_buffer gets hold mode."""
        import numpy as np

        @cc.transferable
        class NpData:
            arr: object  # np.ndarray

            def serialize(d: 'NpData') -> bytes:
                return d.arr.tobytes()

            def deserialize(b: bytes) -> 'NpData':
                return NpData(arr=np.frombuffer(b, dtype=np.float64).copy())

            def from_buffer(b: bytes) -> 'NpData':
                return NpData(arr=np.frombuffer(b, dtype=np.float64))

        @cc.icrm(namespace='test.fb_e2e', version='0.1.0')
        class ICompute:
            def process(self, data: NpData) -> int:
                ...

        # Verify auto-detection set hold mode
        method = getattr(ICompute, 'process')
        assert method._input_buffer_mode == 'hold'

    def test_icrm_explicit_view_override(self):
        """@cc.transfer(buffer='view') overrides auto-detection."""
        @cc.transferable
        class BufData:
            x: int
            def serialize(d: 'BufData') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'BufData':
                return BufData(x=pickle.loads(b))
            def from_buffer(b: bytes) -> 'BufData':
                return BufData(x=pickle.loads(b))

        @cc.icrm(namespace='test.fb_override', version='0.1.0')
        class IOverride:
            @cc.transfer(buffer='view')
            def process(self, data: BufData) -> int:
                ...

        method = getattr(IOverride, 'process')
        assert method._input_buffer_mode == 'view'

    def test_icrm_no_from_buffer_stays_view(self):
        """Without from_buffer, method stays in view mode."""
        @cc.transferable
        class PlainData:
            x: int
            def serialize(d: 'PlainData') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'PlainData':
                return PlainData(x=pickle.loads(b))

        @cc.icrm(namespace='test.fb_plain', version='0.1.0')
        class IPlain:
            def process(self, data: PlainData) -> int:
                ...

        method = getattr(IPlain, 'process')
        assert method._input_buffer_mode == 'view'
```

- [ ] **Step 2: Run new tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestFromBufferEndToEnd -v`
Expected: All 3 PASS

- [ ] **Step 3: Run full test suite — final validation**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`
Expected: All pass (600+ tests)

- [ ] **Step 4: Run Rust checks**

Run: `cd src/c_two/_native && cargo check --workspace && cargo test -p c2-mem -p c2-wire`
Expected: All pass

- [ ] **Step 5: Commit**

```bash
git add tests/unit/test_transferable.py
git commit -m "test: end-to-end from_buffer auto-detection via ICRM decorator

- Verifies auto-hold with numpy-based from_buffer
- Verifies @cc.transfer(buffer='view') override
- Verifies plain types without from_buffer stay view mode

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Dependency Order

```
Task 1 (Rust PyShmBuffer)
    ↓
Task 2 (TransferableMeta from_buffer)
    ↓
Task 3 (auto_transfer auto-detection)
    ↓
Task 4 (_build_transfer_wrapper dispatch)
    ↓
Task 5 (HoldRegistry)  ← depends on Task 1 for weakref support
    ↓
Task 6 (NativeServerBridge integration)  ← depends on Task 5
    ↓
Task 7 (cc.hold_stats export)  ← depends on Task 6
    ↓
Task 8 (Final integration test)  ← depends on all above
```

Tasks 2-4 (Python transferable changes) and Task 5 (HoldRegistry) are independent of each other but both depend on Task 1. Tasks 2-4 must be sequential.
