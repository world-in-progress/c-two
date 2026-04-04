# Transferable Buffer Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate unnecessary `bytes()` materialization in IPC dispatch by introducing `buffer='copy|view|hold'` on `@cc.transferable`, cutting one memcpy per request on the server hot path.

**Architecture:** The `transferable()` decorator gains a `buffer` parameter that controls SHM lifetime during deserialization. The dispatch closure in `native.py` reads this mode from the dispatch table and decides whether to materialize (`copy`), pass memoryview then release (`view`), or let RAII handle release (`hold`). Default pickle transferables use `view` mode (pickle.loads accepts memoryview). Custom `@cc.transferable` defaults to `copy` for backward safety.

**Tech Stack:** Python 3.10+, pytest, c_two framework (Python + Rust via PyO3)

**Spec:** `docs/superpowers/specs/2026-04-03-transferable-buffer-mode-design.md`

---

## File Map

| File | Role | Changes |
|------|------|---------|
| `src/c_two/crm/transferable.py` | Core transferable system | Decorator, base class, crm_to_com, com_to_crm, transfer_wrapper |
| `src/c_two/transport/server/native.py` | Server dispatch | Dispatch table expansion, dispatch closure rewrite |
| `tests/unit/test_transferable.py` | Unit tests | Buffer mode attribute tests, crm_to_com mode tests |
| `tests/integration/test_ipc_buddy_reply.py` | Integration tests | End-to-end view/hold mode tests |
| `tests/fixtures/ihello.py` | Test fixtures | New view/hold mode test transferables |

---

### Task 1: `transferable()` decorator accepts `buffer` parameter

**Files:**
- Modify: `src/c_two/crm/transferable.py:78` (Transferable base class)
- Modify: `src/c_two/crm/transferable.py:241-249` (transferable decorator)
- Test: `tests/unit/test_transferable.py`

- [ ] **Step 1: Write failing tests for `_buffer_mode` attribute**

Add a new test class after the existing `TestTransferableDecorator` class:

```python
# In tests/unit/test_transferable.py

class TestBufferMode:
    """Tests for the buffer='copy|view|hold' parameter on @cc.transferable."""

    def test_default_buffer_mode_is_copy(self):
        """@cc.transferable without buffer= defaults to 'copy'."""
        @cc.transferable
        class CopyData:
            x: int
            def serialize(d: 'CopyData') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'CopyData':
                return CopyData(x=pickle.loads(b))

        assert CopyData._buffer_mode == 'copy'

    def test_explicit_copy_mode(self):
        """@cc.transferable(buffer='copy') sets _buffer_mode='copy'."""
        @cc.transferable(buffer='copy')
        class ExplicitCopy:
            v: int
            def serialize(d: 'ExplicitCopy') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(b: bytes) -> 'ExplicitCopy':
                return ExplicitCopy(v=pickle.loads(b))

        assert ExplicitCopy._buffer_mode == 'copy'

    def test_view_mode(self):
        """@cc.transferable(buffer='view') sets _buffer_mode='view'."""
        @cc.transferable(buffer='view')
        class ViewData:
            v: int
            def serialize(d: 'ViewData') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(data: memoryview) -> 'ViewData':
                return ViewData(v=pickle.loads(data))

        assert ViewData._buffer_mode == 'view'

    def test_hold_mode(self):
        """@cc.transferable(buffer='hold') sets _buffer_mode='hold'."""
        @cc.transferable(buffer='hold')
        class HoldData:
            v: int
            def serialize(d: 'HoldData') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(data: memoryview) -> 'HoldData':
                return HoldData(v=pickle.loads(data))

        assert HoldData._buffer_mode == 'hold'

    def test_invalid_buffer_mode_raises(self):
        """Invalid buffer= value raises ValueError."""
        with pytest.raises(ValueError, match='buffer'):
            @cc.transferable(buffer='invalid')
            class Bad:
                x: int
                def serialize(d: 'Bad') -> bytes:
                    return b''
                def deserialize(b: bytes) -> 'Bad':
                    return Bad(x=0)

    def test_existing_hello_data_has_copy_mode(self):
        """Pre-existing @cc.transferable classes default to 'copy'."""
        assert HelloData._buffer_mode == 'copy'

    def test_default_transferable_has_view_mode(self):
        """Default pickle transferable (auto-generated) uses 'view' mode."""
        def fn(self, x: int) -> int: ...
        t_in = create_default_transferable(fn, is_input=True)
        t_out = create_default_transferable(fn, is_input=False)
        assert t_in._buffer_mode == 'view'
        assert t_out._buffer_mode == 'view'
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestBufferMode -v --timeout=30`
Expected: FAIL — `_buffer_mode` attribute does not exist.

- [ ] **Step 3: Add `_buffer_mode` to Transferable base class**

In `src/c_two/crm/transferable.py`, add a class attribute to `Transferable`:

```python
class Transferable(metaclass=TransferableMeta):
    """..."""

    _buffer_mode: str = 'copy'  # 'copy' | 'view' | 'hold'

    def serialize(*args: any) -> bytes:
        ...

    def deserialize(bytes: any) -> any:
        ...
```

- [ ] **Step 4: Modify `transferable()` decorator to accept optional `buffer` parameter**

Replace the current `transferable()` function (line 241-249) with:

```python
_VALID_BUFFER_MODES = frozenset(('copy', 'view', 'hold'))

def transferable(cls=None, *, buffer: str = 'copy'):
    """Decorator to make a class inherit from Transferable.

    Supports both ``@cc.transferable`` and ``@cc.transferable(buffer='view')``.
    """
    if buffer not in _VALID_BUFFER_MODES:
        raise ValueError(
            f"buffer must be one of {sorted(_VALID_BUFFER_MODES)}, got {buffer!r}"
        )

    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls.__module__ = cls.__module__
        new_cls.__qualname__ = cls.__qualname__
        new_cls._buffer_mode = buffer
        return new_cls

    if cls is not None:
        # Called as @cc.transferable (no parentheses)
        return wrap(cls)
    # Called as @cc.transferable(buffer='view')
    return wrap
```

- [ ] **Step 5: Set `_buffer_mode = 'view'` on default transferables**

In `create_default_transferable()`, after creating each class, set the attribute:

For `DynamicInputTransferable` (after line 190):
```python
DynamicInputTransferable._buffer_mode = 'view'
```

For `DynamicOutputTransferable` (after line 222):
```python
DynamicOutputTransferable._buffer_mode = 'view'
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestBufferMode -v --timeout=30`
Expected: All 8 tests PASS.

- [ ] **Step 7: Run full test suite to verify no regressions**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests pass (502+).

- [ ] **Step 8: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat(transferable): add buffer='copy|view|hold' parameter to @cc.transferable

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 2: `crm_to_com()` buffer-mode-aware deserialization (server request path)

**Files:**
- Modify: `src/c_two/crm/transferable.py:311-365` (crm_to_com function)
- Modify: `src/c_two/crm/transferable.py:367-383` (transfer_wrapper function)
- Test: `tests/unit/test_transferable.py`

The `crm_to_com` function is the **server-side** path: it deserializes the request, calls the CRM method, and serializes the response. The buffer mode controls how the request data is deserialized. The `transfer_wrapper` must forward `**kwargs` (specifically `_release_fn`) from the dispatch closure to `crm_to_com`.

- [ ] **Step 1: Write failing tests for `crm_to_com` buffer modes**

Add a new test class in `tests/unit/test_transferable.py`:

```python
class TestCrmToComBufferModes:
    """Test that crm_to_com handles _release_fn based on input buffer mode."""

    def _setup(self, input_trans):
        """Create a transfer-wrapped echo function with given input transferable."""
        from c_two.crm.transferable import transfer
        decorator = transfer(input=input_trans, output=None)
        def echo(self, x):
            return x
        return decorator(echo)

    def _make_icrm(self):
        class MockCRM:
            def echo(self, x):
                return x
        class MockICRM:
            direction = '<-'
            crm = MockCRM()
        return MockICRM()

    def test_copy_mode_calls_release(self):
        """In copy mode, _release_fn is called (before deserialize)."""
        released = []

        @cc.transferable(buffer='copy')
        class CopyIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                return pickle.loads(data)

        wrapped = self._setup(CopyIn)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn was not called in copy mode'

    def test_view_mode_calls_release(self):
        """In view mode, _release_fn is called (after deserialize)."""
        released = []
        def fn(self, x: int) -> int: ...
        input_trans = create_default_transferable(fn, is_input=True)
        assert input_trans._buffer_mode == 'view'

        wrapped = self._setup(input_trans)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn was not called in view mode'

    def test_hold_mode_skips_release(self):
        """In hold mode, _release_fn is NOT called (RAII)."""
        released = []

        @cc.transferable(buffer='hold')
        class HoldIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(data) if isinstance(data, (bytes, memoryview)) else data

        wrapped = self._setup(HoldIn)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert not released, '_release_fn should NOT be called in hold mode'

    def test_no_release_fn_works(self):
        """When _release_fn is None (thread-local), all modes work fine."""
        def fn(self, x: int) -> int: ...
        input_trans = create_default_transferable(fn, is_input=True)
        wrapped = self._setup(input_trans)
        icrm = self._make_icrm()
        # No _release_fn — must not crash
        result = wrapped(icrm, pickle.dumps(42))
        # result is (error_bytes, result_bytes) tuple
        assert isinstance(result, tuple)

    def test_release_called_on_exception(self):
        """_release_fn must be called even if deserialize raises."""
        released = []

        @cc.transferable(buffer='copy')
        class BadDeser:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                raise RuntimeError('boom')

        wrapped = self._setup(BadDeser)
        icrm = self._make_icrm()
        # Should not raise — crm_to_com catches exceptions
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn must be called even on error'

    def test_transfer_wrapper_has_buffer_mode_attrs(self):
        """transfer_wrapper should expose _input_buffer_mode and _output_buffer_mode."""
        @cc.transferable(buffer='view')
        class ViewIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(data)

        wrapped = self._setup(ViewIn)
        assert hasattr(wrapped, '_input_buffer_mode')
        assert wrapped._input_buffer_mode == 'view'
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestCrmToComBufferModes -v --timeout=30`
Expected: FAIL — `crm_to_com` does not accept `_release_fn` kwarg.

- [ ] **Step 3: Modify `crm_to_com` to handle buffer modes**

In `src/c_two/crm/transferable.py`, replace the `crm_to_com` function body (inside `transfer()`) with buffer-mode-aware logic:

```python
def crm_to_com(*args: any, _release_fn=None) -> tuple[any, any]:
    input_transferable = input.deserialize if input else None
    output_transferable = output.serialize if output else None
    input_buffer_mode = getattr(input, '_buffer_mode', 'copy') if input else 'copy'

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
            if input_buffer_mode == 'copy':
                # Materialize to bytes, release SHM, then deserialize
                request_copy = bytes(request) if not isinstance(request, bytes) else request
                if _release_fn is not None:
                    _release_fn()
                    _release_fn = None
                deserialized_args = input_transferable(request_copy)
            elif input_buffer_mode == 'view':
                # Deserialize directly (memoryview-safe), then release
                deserialized_args = input_transferable(request)
                if _release_fn is not None:
                    _release_fn()
                    _release_fn = None
            else:  # hold
                # Deserialize directly, do NOT release (RAII)
                deserialized_args = input_transferable(request)
        else:
            deserialized_args = tuple()
            # Release even when no input to deserialize
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
        # Safety net: release on error if not already released
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

    serialized_error: bytes = error.CCError.serialize(err)
    serialized_result = b''
    if output_transferable is not None and result is not None:
        serialized_result = (
            output_transferable(*result) if isinstance(result, tuple)
            else output_transferable(result)
        )
    return (serialized_error, serialized_result)
```

- [ ] **Step 4: Modify `transfer_wrapper` to forward kwargs and expose buffer mode**

In `src/c_two/crm/transferable.py`, replace `transfer_wrapper`:

```python
@wraps(func)
def transfer_wrapper(*args: any, **kwargs: any) -> any:
    if not args:
        raise ValueError('No arguments provided to determine direction.')

    icrm = args[0]
    if not hasattr(icrm, 'direction'):
        raise AttributeError('The ICRM instance does not have a "direction" attribute.')

    if icrm.direction == '->':
        return com_to_crm(*args)
    elif icrm.direction == '<-':
        return crm_to_com(*args, **kwargs)
    else:
        raise ValueError(f'Invalid direction value: {icrm.direction}.')

# Expose buffer mode attributes for dispatch table introspection
transfer_wrapper._input_buffer_mode = getattr(input, '_buffer_mode', 'copy') if input else 'copy'
transfer_wrapper._output_buffer_mode = getattr(output, '_buffer_mode', 'copy') if output else 'copy'

return transfer_wrapper
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestCrmToComBufferModes -v --timeout=30`
Expected: All 7 tests PASS.

- [ ] **Step 6: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat(transferable): buffer-mode-aware crm_to_com and transfer_wrapper

- crm_to_com accepts _release_fn kwarg for SHM lifecycle control
- copy: materialize → release → deserialize
- view: deserialize → release
- hold: deserialize only (RAII)
- transfer_wrapper forwards **kwargs to crm_to_com
- transfer_wrapper exposes _input/_output_buffer_mode attributes

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 3: `com_to_crm()` buffer-mode-aware response deserialization (client path)

**Files:**
- Modify: `src/c_two/crm/transferable.py:256-309` (com_to_crm function)
- Test: `tests/unit/test_transferable.py`

The `com_to_crm` function is the **client-side** path: it serializes the request, calls `client.call()`, and deserializes the response. The output buffer mode controls how the response is handled. Currently lines 288-296 always materialize + release — this needs to respect the output transferable's buffer mode.

- [ ] **Step 1: Write failing tests for `com_to_crm` output buffer modes**

Add a new test class in `tests/unit/test_transferable.py`:

```python
class TestComToCrmBufferModes:
    """Test that com_to_crm handles response based on output buffer mode."""

    def _make_mock_response(self, data: bytes):
        """Create a mock PyShmBuffer-like response."""
        class MockResponse:
            def __init__(self, data):
                self._data = data
                self.released = False
            def release(self):
                self.released = True
            def __buffer__(self, flags):
                return memoryview(self._data)
        return MockResponse(data)

    def _make_icrm(self, response_data):
        mock_resp = self._make_mock_response(response_data)
        class MockClient:
            supports_direct_call = False
            def __init__(self, resp):
                self.response = resp
            def call(self, method, data):
                return self.response
        class MockICRM:
            direction = '->'
            def __init__(self, client):
                self.client = client
        client = MockClient(mock_resp)
        return MockICRM(client), mock_resp

    def test_copy_mode_releases_response(self):
        """copy mode: response is materialized and released."""
        @cc.transferable(buffer='copy')
        class CopyOut:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                assert isinstance(data, bytes)
                return pickle.loads(data)

        from c_two.crm.transferable import transfer
        icrm, mock_resp = self._make_icrm(pickle.dumps(42))
        decorator = transfer(input=None, output=CopyOut)
        def fn(self) -> int: ...
        wrapped = decorator(fn)
        result = wrapped(icrm)
        assert result == 42
        assert mock_resp.released

    def test_hold_mode_skips_release(self):
        """hold mode: response is NOT released (RAII)."""
        @cc.transferable(buffer='hold')
        class HoldOut:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(bytes(data)) if isinstance(data, memoryview) else pickle.loads(data)

        from c_two.crm.transferable import transfer
        icrm, mock_resp = self._make_icrm(pickle.dumps(42))
        decorator = transfer(input=None, output=HoldOut)
        def fn(self) -> int: ...
        wrapped = decorator(fn)
        result = wrapped(icrm)
        assert result == 42
        assert not mock_resp.released
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestComToCrmBufferModes -v --timeout=30`
Expected: FAIL — current `com_to_crm` always releases.

- [ ] **Step 3: Modify `com_to_crm` to handle output buffer modes**

In `src/c_two/crm/transferable.py`, modify the response handling in `com_to_crm`. Add `output_buffer_mode` at the top, then replace the `hasattr(response, 'release')` block:

```python
def com_to_crm(*args: any) -> any:
    input_transferable = input.serialize if input else None
    output_transferable = output.deserialize if output else None
    output_buffer_mode = getattr(output, '_buffer_mode', 'copy') if output else 'copy'

    # ... (serialize input, call CRM unchanged) ...

    # Deserialize output — buffer-mode-aware
    stage = 'deserialize_output'
    if not output_transferable:
        if hasattr(response, 'release'):
            response.release()
        return None

    if hasattr(response, 'release'):
        mv = memoryview(response)
        if output_buffer_mode == 'copy':
            try:
                data = bytes(mv)
            finally:
                mv.release()
                response.release()
            result = output_transferable(data)
        elif output_buffer_mode == 'view':
            try:
                result = output_transferable(mv)
                if isinstance(result, memoryview):
                    result = bytes(result)
            finally:
                mv.release()
                response.release()
        else:  # hold — RAII, no explicit release
            result = output_transferable(mv)
    else:
        result = output_transferable(response)
    return result
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/unit/test_transferable.py::TestComToCrmBufferModes -v --timeout=30`
Expected: All tests PASS.

- [ ] **Step 5: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/c_two/crm/transferable.py tests/unit/test_transferable.py
git commit -m "feat(transferable): buffer-mode-aware com_to_crm (client response path)

- copy: bytes(mv) → release → deserialize
- view: deserialize(mv) → release (current behavior)
- hold: deserialize(mv) → no release (RAII)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 4: Dispatch table expansion and `native.py` dispatch closure rewrite

**Files:**
- Modify: `src/c_two/transport/server/native.py:42-61` (CRMSlot, build_dispatch_table)
- Modify: `src/c_two/transport/server/native.py:295-361` (_make_dispatcher, dispatch closure)

This is the critical task — it connects the buffer mode to the actual IPC hot path. The dispatch closure in `native.py` currently does `bytes(mv)` for every request. After this task, it reads buffer mode from the dispatch table and either:
- `copy`: `bytes(mv)` → release buf → call method (already released, no `_release_fn`)
- `view`: call method with `_release_fn=<closure>` that releases mv+buf
- `hold`: call method WITHOUT release — RAII handles lifetime

- [ ] **Step 1: Expand `CRMSlot._dispatch_table` type**

In `src/c_two/transport/server/native.py`, change the type annotation on line 51:

```python
_dispatch_table: dict[str, tuple[Any, MethodAccess, str]] = field(
    default_factory=dict, repr=False,
)
```

- [ ] **Step 2: Update `build_dispatch_table` to read buffer mode**

```python
def build_dispatch_table(self) -> None:
    for name in self.methods:
        if name == self.shutdown_method:
            continue
        method = getattr(self.icrm, name, None)
        if method is not None:
            access = get_method_access(method)
            buffer_mode = getattr(method, '_input_buffer_mode', 'copy')
            self._dispatch_table[name] = (method, access, buffer_mode)
```

- [ ] **Step 3: Rewrite dispatch closure in `_make_dispatcher`**

Replace the dispatch closure body (lines 312-361):

```python
def dispatch(
    _route_name: str, method_idx: int,
    request_buf: object, response_pool: object,
) -> object:
    # 1. Resolve method
    method_name = idx_to_name.get(method_idx)
    if method_name is None:
        raise RuntimeError(
            f'Unknown method index {method_idx} for route {route_name}',
        )
    entry = dispatch_table.get(method_name)
    if entry is None:
        raise RuntimeError(f'Method not found: {method_name}')
    method, _access, buffer_mode = entry

    # 2. Buffer-mode-aware request handling
    if buffer_mode == 'copy':
        # Materialize to bytes, release SHM immediately
        try:
            mv = memoryview(request_buf)
            payload = bytes(mv)
            mv.release()
        finally:
            try:
                request_buf.release()
            except Exception:
                pass
        result = method(payload)
    elif buffer_mode == 'view':
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

    # 3. Unpack result
    res_part, err_part = unpack_icrm_result(result)
    if err_part:
        raise CrmCallError(err_part)
    if not res_part:
        return None

    # 4. For large responses, write to response pool SHM
    if response_pool is not None and len(res_part) > shm_threshold:
        try:
            alloc = response_pool.alloc(len(res_part))
            response_pool.write(alloc, res_part)
            seg_idx = int(alloc.seg_idx) & 0xFFFF
            return (seg_idx, alloc.offset, len(res_part), alloc.is_dedicated)
        except Exception:
            pass

    return res_part
```

- [ ] **Step 4: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests pass. Default pickle path now uses `view` mode — zero-copy on server request!

- [ ] **Step 5: Commit**

```bash
git add src/c_two/transport/server/native.py
git commit -m "feat(native): buffer-mode-aware dispatch eliminates server request copy

- Dispatch table expanded: (method, access, buffer_mode)
- copy: bytes(mv) → release → call (backward compat)
- view: pass mv + _release_fn (zero-copy for default pickle)
- hold: pass mv, RAII (zero-copy for ORM views)

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 5: Integration tests for IPC buffer modes

**Files:**
- Modify: `tests/integration/test_ipc_buddy_reply.py`

End-to-end tests verifying that all three buffer modes work through the full IPC stack (client → Rust UDS → server dispatch → CRM → response).

- [ ] **Step 1: Add view-mode IPC integration test**

Add to `tests/integration/test_ipc_buddy_reply.py`:

```python
def test_view_mode_large_payload(self):
    """View mode (default pickle) works for large payloads through IPC."""
    proxy = self._setup_ipc('ipc://test_buddy_reply_view')
    data = b'V' * (2 * 1024 * 1024)  # 2MB
    result = proxy.echo(data)
    assert result == data
    cc.close(proxy)
```

- [ ] **Step 2: Add copy-mode backward-compat test**

```python
def test_copy_mode_custom_transferable(self):
    """Custom @cc.transferable (copy mode default) works through IPC."""
    proxy = self._setup_ipc('ipc://test_buddy_reply_copy_compat')
    data = b'C' * (1024 * 1024)  # 1MB
    result = proxy.echo(data)
    assert result == data
    cc.close(proxy)
```

- [ ] **Step 3: Run integration tests**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/integration/test_ipc_buddy_reply.py -v --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests PASS.

- [ ] **Step 4: Run full test suite**

Run: `C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30 -k "not test_concurrent_large_calls"`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add tests/integration/test_ipc_buddy_reply.py
git commit -m "test: add integration tests for buffer mode IPC paths

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

### Task 6: Benchmark verification (64B–1GB)

**Files:**
- Use: `benchmarks/segment_size_benchmark.py`

Run the existing benchmark to verify performance improvement. Compare against baseline:

**Baseline (pre-optimization):**

| Size | Latency | Throughput |
|------|---------|------------|
| 64B  | 0.17ms  | —          |
| 1MB  | 0.33ms  | 3.0 GB/s   |
| 100MB| 9.80ms  | 10.0 GB/s  |
| 1GB  | 311ms   | 3.2 GB/s   |

- [ ] **Step 1: Run bytes echo benchmark (256MB segment)**

```bash
C2_RELAY_ADDRESS= uv run python benchmarks/segment_size_benchmark.py
```

Record results for 64B, 1KB, 64KB, 1MB, 10MB, 100MB, 500MB, 1GB.

- [ ] **Step 2: Compare against baseline**

Expected: ~5-15% latency reduction for medium-large payloads (1MB–100MB) where the eliminated `bytes()` copy was significant. Small payloads (64B, 1KB): negligible change. Very large (500MB+): moderate improvement.

- [ ] **Step 3: Document results**

Record benchmark comparison in commit message or brief analysis.