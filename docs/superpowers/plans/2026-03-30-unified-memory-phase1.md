# Unified Memory Fallback — Phase 1: Dynamic Pool Expansion

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable the buddy pool to dynamically expand from 1 to N segments, with both client and server lazily opening new segments by deriving SHM names from a shared prefix — eliminating per-request SHM degradation under load.

**Architecture:** The client's Rust buddy pool creates segments on demand (up to `max_pool_segments`). Segment names are deterministic: `{prefix}_b{vec_index:04x}` for buddy, `{prefix}_d{dedicated_idx:04x}` for dedicated. The pool prefix is exchanged once during handshake. When either side encounters an unknown `seg_idx` in a wire frame, it lazily opens the segment using the derived name. No announce protocol needed — UDS FIFO ordering guarantees the segment exists before the data frame arrives.

**Tech Stack:** Rust (c2-buddy crate), Python 3.10+, PyO3/maturin, pytest

**Design spec:** `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md`

**Test baseline:** 717 passed, 2 flaky (backpressure + serve subprocess). Run: `uv run pytest tests/ -q --timeout=30`

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/c_two/_native/c2-buddy/src/pool.rs` | Deterministic naming, expose prefix |
| Modify | `src/c_two/_native/c2-ffi/src/buddy_ffi.rs` | FFI: prefix(), derive_segment_name() |
| Modify | `src/c_two/transport/protocol.py` | Handshake v6: prefix exchange |
| Modify | `src/c_two/transport/client/core.py` | max_segments, dict seg_views, lazy open |
| Modify | `src/c_two/transport/server/connection.py` | seg_views → dict, peer_prefix field |
| Modify | `src/c_two/transport/server/handshake.py` | Store peer_prefix, fix max_segments |
| Modify | `src/c_two/transport/server/core.py` | Lazy open peer segments |
| Modify | `src/c_two/transport/server/reply.py` | Dict seg_views access |
| Modify | `tests/unit/test_buddy_pool.py` | Rust pool deterministic naming tests |
| Modify | `tests/unit/test_wire.py` | Handshake prefix roundtrip tests |
| Create | `tests/integration/test_dynamic_pool.py` | Multi-segment E2E tests |

---

### Task 1: Rust — Deterministic Buddy Segment Naming

Currently `create_segment()` (pool.rs:596) calls `next_name("b")` which uses a shared atomic counter. Dedicated segments also use this counter via `next_name("d")`. When buddy and dedicated interleave, the counter diverges from the Vec index — making names unpredictable from the outside.

**Fix:** Buddy segments use `segments.len()` as name suffix. Dedicated segments use `next_dedicated_idx` as name suffix. Remove the shared `name_counter`.

**Files:**
- Modify: `src/c_two/_native/c2-buddy/src/pool.rs:75-112` (struct + constructor)
- Modify: `src/c_two/_native/c2-buddy/src/pool.rs:596-604` (create_segment + next_name)
- Modify: `src/c_two/_native/c2-buddy/src/pool.rs:544` (alloc_dedicated naming)
- Test: `src/c_two/_native/c2-buddy/src/pool.rs` (inline Rust tests)

- [ ] **Step 1: Write Rust test for deterministic naming**

Add to the `mod tests` block at the bottom of `pool.rs`:

```rust
#[test]
fn test_deterministic_buddy_naming() {
    let mut pool = test_pool(PoolConfig {
        max_segments: 4,
        max_dedicated_segments: 2,
        ..test_config()
    });

    // Allocate to force segment 0 creation.
    let a0 = pool.alloc(4096).unwrap();
    assert_eq!(a0.seg_idx, 0);
    let name0 = pool.segment_name(0).unwrap();
    // Name must end with _b0000.
    assert!(name0.ends_with("_b0000"), "got: {}", name0);

    // Fill segment 0 to force segment 1 creation.
    // Allocate a large block to exhaust the segment quickly.
    let big = pool.config.segment_size; // fills entire segment
    pool.free(&a0).unwrap();
    let a_big = pool.alloc(big).unwrap();
    assert_eq!(a_big.seg_idx, 0);
    // Now segment 0 is full — next alloc creates segment 1.
    let a1 = pool.alloc(4096).unwrap();
    assert_eq!(a1.seg_idx, 1);
    let name1 = pool.segment_name(1).unwrap();
    assert!(name1.ends_with("_b0001"), "got: {}", name1);

    // Interleave a dedicated allocation — should NOT affect buddy naming.
    let a_ded = pool.alloc(big * 2).unwrap(); // too big for buddy → dedicated
    assert!(a_ded.is_dedicated);

    // Allocate buddy segment 2 — fill segment 1 first.
    pool.free(&a1).unwrap();
    let a1_big = pool.alloc(big).unwrap();
    assert_eq!(a1_big.seg_idx, 1);
    let a2 = pool.alloc(4096).unwrap();
    assert_eq!(a2.seg_idx, 2);
    let name2 = pool.segment_name(2).unwrap();
    assert!(name2.ends_with("_b0002"), "got: {}", name2);

    pool.free(&a_ded).unwrap();
    pool.free(&a_big).unwrap();
    pool.free(&a1_big).unwrap();
    pool.free(&a2).unwrap();
}

#[test]
fn test_deterministic_dedicated_naming() {
    let mut pool = test_pool(PoolConfig {
        max_segments: 1,
        max_dedicated_segments: 4,
        ..test_config()
    });

    // Force segment 0 creation.
    let a0 = pool.alloc(4096).unwrap();
    pool.free(&a0).unwrap();

    // Allocate dedicated segments.
    let big = pool.config.segment_size * 2; // too big for buddy
    let d0 = pool.alloc(big).unwrap();
    assert!(d0.is_dedicated);
    assert_eq!(d0.seg_idx, 256); // next_dedicated_idx starts at 256
    let dname0 = pool.dedicated_name(256).unwrap();
    assert!(dname0.ends_with("_d0100"), "got: {}", dname0);

    let d1 = pool.alloc(big).unwrap();
    assert!(d1.is_dedicated);
    assert_eq!(d1.seg_idx, 257);
    let dname1 = pool.dedicated_name(257).unwrap();
    assert!(dname1.ends_with("_d0101"), "got: {}", dname1);

    pool.free(&d0).unwrap();
    pool.free(&d1).unwrap();
}

#[test]
fn test_prefix_accessor() {
    let pool = test_pool(test_config());
    let prefix = pool.prefix();
    assert!(prefix.starts_with("/cc3t"), "got: {}", prefix);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src/c_two/_native/c2-buddy && cargo test --no-default-features -- test_deterministic 2>&1 | tail -20`
Expected: compile error or test failure (prefix() method doesn't exist yet, naming is non-deterministic).

- [ ] **Step 3: Implement deterministic naming**

In `pool.rs`, make these changes:

**3a.** Remove `name_counter` from `BuddyPool` struct and constructor:

```rust
// In struct BuddyPool (line ~82):
// DELETE: name_counter: AtomicU32,

// In new_with_prefix() (line ~103):
// DELETE: name_counter: AtomicU32::new(0),
```

**3b.** Replace `create_segment()` and remove `next_name()`:

```rust
fn create_segment(&self) -> Result<ShmSegment, String> {
    let idx = self.segments.len();
    let name = format!("{}_{}{:04x}", self.name_prefix, "b", idx);
    ShmSegment::create(&name, self.config.segment_size, self.config.min_block_size)
}

// DELETE the next_name() method entirely.
```

**3c.** Update `alloc_dedicated()` naming (line ~544):

```rust
// Replace: let name = self.next_name("d");
// With:
let name = format!("{}_{}{:04x}", self.name_prefix, "d", idx);
```

Note: `idx` is already `self.next_dedicated_idx` which is defined on the line above.

**3d.** Add `prefix()` accessor:

```rust
/// Get the pool name prefix (for handshake exchange).
pub fn prefix(&self) -> &str {
    &self.name_prefix
}
```

- [ ] **Step 4: Run Rust tests**

Run: `cd src/c_two/_native/c2-buddy && cargo test --no-default-features 2>&1 | tail -10`
Expected: all tests pass, including the new deterministic naming tests.

- [ ] **Step 5: Commit**

```bash
git add src/c_two/_native/c2-buddy/src/pool.rs
git commit -m "feat(buddy): deterministic segment naming + prefix accessor

Buddy segments now use Vec index as name suffix (_b{idx:04x}),
dedicated segments use next_dedicated_idx (_d{idx:04x}).
Removes shared name_counter — names are derivable from
(prefix, seg_idx, is_dedicated).

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 2: Rust FFI — Expose Prefix and Derive Name

**Files:**
- Modify: `src/c_two/_native/c2-ffi/src/buddy_ffi.rs:175-184` (PyBuddyPoolHandle methods)

- [ ] **Step 1: Add `prefix()` method to PyBuddyPoolHandle**

In `buddy_ffi.rs`, add inside the `#[pymethods] impl PyBuddyPoolHandle` block (after `segment_name` method around line 425):

```rust
/// Get the pool name prefix (for handshake exchange / name derivation).
fn prefix(&self) -> PyResult<String> {
    let pool = self.pool.read().map_err(|e| {
        PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
    })?;
    Ok(pool.prefix().to_string())
}

/// Derive the SHM name for a segment given its index and tier.
///
/// Buddy segments: `{prefix}_b{seg_idx:04x}`
/// Dedicated segments: `{prefix}_d{seg_idx:04x}`
#[pyo3(signature = (seg_idx, is_dedicated = false))]
fn derive_segment_name(&self, seg_idx: u32, is_dedicated: bool) -> PyResult<String> {
    let pool = self.pool.read().map_err(|e| {
        PyRuntimeError::new_err(format!("pool lock poisoned: {e}"))
    })?;
    let tag = if is_dedicated { "d" } else { "b" };
    Ok(format!("{}_{}{:04x}", pool.prefix(), tag, seg_idx))
}
```

- [ ] **Step 2: Build and verify**

Run: `cd src/c_two/_native && cargo check && cd ../../../.. && uv sync --reinstall-package c-two 2>&1 | tail -5`
Expected: compiles and installs cleanly.

- [ ] **Step 3: Quick Python smoke test**

```bash
uv run python -c "
from c_two.buddy import BuddyPoolHandle, PoolConfig
p = BuddyPoolHandle(PoolConfig(segment_size=64*1024, min_block_size=4096, max_segments=2))
a = p.alloc(4096)
print('prefix:', p.prefix())
print('derive b0:', p.derive_segment_name(0))
print('derive d256:', p.derive_segment_name(256, True))
print('actual name:', p.segment_name(0))
assert p.derive_segment_name(0) == p.segment_name(0), 'name mismatch!'
print('OK: derived name matches actual name')
p.free(a)
p.destroy()
"
```

- [ ] **Step 4: Commit**

```bash
git add src/c_two/_native/c2-ffi/src/buddy_ffi.rs
git commit -m "feat(ffi): expose prefix() and derive_segment_name() to Python

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 3: Python — Handshake Prefix Exchange

Bump handshake version from 5 to 6. Add `prefix` field after `version` byte. Both client and server send their pool prefix so each side can derive the other's segment names.

**Files:**
- Modify: `src/c_two/transport/protocol.py:44,108-113,120-141,148-190,197-277`
- Test: `tests/unit/test_wire.py`

- [ ] **Step 1: Write test for handshake prefix roundtrip**

Add to `tests/unit/test_wire.py` — a new test class:

```python
class TestHandshakePrefixExchange:
    """Handshake v6: prefix field."""

    def test_client_handshake_with_prefix(self):
        """Client handshake encodes and decodes prefix."""
        segments = [('/cc3b00001234_b0000', 262144)]
        prefix = '/cc3b00001234'
        encoded = encode_client_handshake(segments, CAP_CALL | CAP_METHOD_IDX, prefix=prefix)
        assert encoded[0] == HANDSHAKE_VERSION
        hs = decode_handshake(encoded)
        assert hs.prefix == prefix
        assert hs.segments == segments
        assert hs.capability_flags & CAP_CALL

    def test_server_handshake_with_prefix(self):
        """Server handshake encodes and decodes prefix."""
        segments = [('/cc3bABCD0000_b0000', 262144)]
        prefix = '/cc3bABCD0000'
        routes = [RouteInfo(name='grid', methods=[MethodEntry(name='get', index=0)])]
        encoded = encode_server_handshake(segments, CAP_CALL, routes, prefix=prefix)
        hs = decode_handshake(encoded)
        assert hs.prefix == prefix
        assert len(hs.routes) == 1
        assert hs.routes[0].name == 'grid'

    def test_empty_prefix_defaults(self):
        """When prefix is empty string, roundtrip still works."""
        segments = [('/seg0', 65536)]
        encoded = encode_client_handshake(segments, CAP_CALL, prefix='')
        hs = decode_handshake(encoded)
        assert hs.prefix == ''

    def test_prefix_none_uses_empty(self):
        """When prefix is not provided, defaults to empty string."""
        segments = [('/seg0', 65536)]
        encoded = encode_client_handshake(segments, CAP_CALL)
        hs = decode_handshake(encoded)
        assert hs.prefix == ''
```

- [ ] **Step 2: Run test to verify it fails**

Run: `uv run pytest tests/unit/test_wire.py::TestHandshakePrefixExchange -v 2>&1 | tail -10`
Expected: FAIL — `encode_client_handshake` doesn't accept `prefix` kwarg yet.

- [ ] **Step 3: Implement handshake changes**

**3a.** In `protocol.py`, bump version (line 44):

```python
# Change:
HANDSHAKE_VERSION = 5
# To:
HANDSHAKE_VERSION = 6
```

**3b.** Add `prefix` field to `Handshake` dataclass (line ~108):

```python
@dataclass
class Handshake:
    """Parsed handshake payload."""
    segments: list[tuple[str, int]]   # [(shm_name, segment_size), ...]
    capability_flags: int = 0
    routes: list[RouteInfo] = field(default_factory=list)
    prefix: str = ''                  # pool name prefix for segment name derivation
```

**3c.** Update `encode_client_handshake()` — add prefix parameter and encode after version:

```python
def encode_client_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int = CAP_CALL | CAP_METHOD_IDX,
    *,
    prefix: str = '',
) -> bytes:
    """Encode client->server handshake.

    Format (v6)::

        [1B version=6]
        [1B prefix_len][prefix UTF-8]
        [2B seg_count LE]
        [per-segment: [4B size LE][1B name_len][name UTF-8]]
        [2B capability_flags LE]
    """
    parts: list[bytes] = [bytes([HANDSHAKE_VERSION])]
    prefix_b = prefix.encode('utf-8')
    parts.append(bytes([len(prefix_b)]))
    parts.append(prefix_b)
    parts.append(_U16.pack(len(segments)))
    for name, size in segments:
        name_b = name.encode('utf-8')
        parts.append(_U32.pack(size))
        parts.append(bytes([len(name_b)]))
        parts.append(name_b)
    parts.append(_U16.pack(capability_flags))
    return b''.join(parts)
```

**3d.** Update `encode_server_handshake()` — add prefix parameter:

```python
def encode_server_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int,
    routes: list[RouteInfo],
    *,
    prefix: str = '',
) -> bytes:
```

Add after `parts = [bytes([HANDSHAKE_VERSION])]`:

```python
    prefix_b = prefix.encode('utf-8')
    parts.append(bytes([len(prefix_b)]))
    parts.append(prefix_b)
```

**3e.** Update `decode_handshake()` — parse prefix after version byte:

After `off = 1` (line ~217), insert:

```python
    # Parse prefix (v6).
    if off + 1 > buf_len:
        raise ValueError('Handshake truncated: missing prefix length')
    prefix_len = buf[off]; off += 1
    if off + prefix_len > buf_len:
        raise ValueError('Handshake truncated in prefix')
    prefix = bytes(buf[off:off + prefix_len]).decode('utf-8'); off += prefix_len
```

And update the return statement:

```python
    return Handshake(
        segments=segments,
        capability_flags=cap_flags,
        routes=routes,
        prefix=prefix,
    )
```

- [ ] **Step 4: Run tests**

Run: `uv run pytest tests/unit/test_wire.py -v 2>&1 | tail -20`
Expected: new prefix tests pass. Some existing handshake tests may need version byte updates.

- [ ] **Step 5: Fix existing handshake tests for v6**

Update any test that checks `encoded[0] == 5` to `encoded[0] == HANDSHAKE_VERSION` (or `6`). Ensure all wire tests pass.

Run: `uv run pytest tests/unit/test_wire.py -v 2>&1 | tail -20`
Expected: ALL wire tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/c_two/transport/protocol.py tests/unit/test_wire.py
git commit -m "feat(protocol): handshake v6 with pool prefix exchange

Add prefix field to Handshake. encode/decode functions support prefix
kwarg. Prefix is encoded after version byte: [1B len][UTF-8 prefix].
Bump HANDSHAKE_VERSION 5->6.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 4: Python Client — Dynamic Pool Expansion

Three changes: (a) increase `max_segments` from 1 to `config.max_pool_segments`, (b) change `_seg_views` from list to dict with lazy local segment opening, (c) send pool prefix in handshake and store server's prefix.

**Files:**
- Modify: `src/c_two/transport/client/core.py:286-350,534-561,825-900`

- [ ] **Step 1: Change PoolConfig max_segments**

In `_do_buddy_handshake()` (line ~286), change:

```python
# Old:
self._buddy_pool = BuddyPoolHandle(PoolConfig(
    segment_size=self._config.pool_segment_size,
    min_block_size=4096,
    max_segments=1,
    max_dedicated_segments=4,
))

# New:
self._buddy_pool = BuddyPoolHandle(PoolConfig(
    segment_size=self._config.pool_segment_size,
    min_block_size=4096,
    max_segments=self._config.max_pool_segments,
    max_dedicated_segments=4,
))
```

- [ ] **Step 2: Send prefix in client handshake**

In `_do_buddy_handshake()` (line ~306), add prefix kwarg:

```python
# Old:
handshake_payload = encode_client_handshake(
    segments, CAP_CALL | CAP_METHOD_IDX | CAP_CHUNKED,
)

# New:
pool_prefix = self._buddy_pool.prefix()
handshake_payload = encode_client_handshake(
    segments, CAP_CALL | CAP_METHOD_IDX | CAP_CHUNKED,
    prefix=pool_prefix,
)
```

- [ ] **Step 3: Store server prefix from handshake response**

After `hs = decode_handshake(payload)` (line ~323), store the server prefix:

```python
self._peer_prefix = hs.prefix  # server pool prefix (empty if server has no pool)
```

Also add `self._peer_prefix: str = ''` in `__init__`.

- [ ] **Step 4: Change _seg_views from list to dict**

In `_do_buddy_handshake()` (line ~340), change:

```python
# Old:
self._seg_views = []
self._seg_base_addrs = []
for seg_idx in range(self._buddy_pool.segment_count()):
    base_addr, data_size = self._buddy_pool.seg_data_info(seg_idx)
    mv = memoryview(
        (ctypes.c_char * data_size).from_address(base_addr)
    ).cast('B')
    self._seg_views.append(mv)
    self._seg_base_addrs.append(base_addr)

# New:
self._seg_views: dict[int, memoryview] = {}
self._seg_base_addrs: dict[int, int] = {}
for seg_idx in range(self._buddy_pool.segment_count()):
    self._cache_local_seg(seg_idx)
```

And add a helper method to `SharedClient`:

```python
def _cache_local_seg(self, seg_idx: int) -> memoryview:
    """Cache memoryview for a local buddy segment (lazy expansion)."""
    if seg_idx in self._seg_views:
        return self._seg_views[seg_idx]
    base_addr, data_size = self._buddy_pool.seg_data_info(seg_idx)
    mv = memoryview(
        (ctypes.c_char * data_size).from_address(base_addr)
    ).cast('B')
    self._seg_views[seg_idx] = mv
    self._seg_base_addrs[seg_idx] = base_addr
    return mv
```

- [ ] **Step 5: Update _try_buddy_alloc — lazy local expansion + keep dedicated**

In `_try_buddy_alloc()` (line ~534), replace the seg_views access:

```python
# Old:
            if alloc.is_dedicated:
                self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, size, True)
                if size <= self._config.max_frame_size:
                    logger.debug('Buddy alloc dedicated for %d bytes, inline fallback%s', size, f' ({label})' if label else '')
                    return None, None
                raise error.MemoryPressureError(
                    f'Buddy pool exhausted (dedicated segment): cannot allocate {size} bytes; '
                    f'payload exceeds inline limit ({self._config.max_frame_size})',
                )

            seg_mv = self._seg_views[alloc.seg_idx]

# New (remove dedicated rejection, add lazy local expansion):
            if alloc.is_dedicated:
                # Dedicated segments are not SHM-shared with the peer.
                # Fall back to inline for small payloads; error for large.
                self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, size, True)
                if size <= self._config.max_frame_size:
                    logger.debug('Buddy alloc dedicated for %d bytes, inline fallback%s', size, f' ({label})' if label else '')
                    return None, None
                raise error.MemoryPressureError(
                    f'Buddy pool exhausted (dedicated segment): cannot allocate {size} bytes; '
                    f'payload exceeds inline limit ({self._config.max_frame_size})',
                )

            # Lazy-open local segment if pool expanded.
            seg_mv = self._cache_local_seg(alloc.seg_idx)
```

> Note: We keep dedicated rejection for now (dedicated segments aren't shared with the peer). The key fix is `_cache_local_seg` for dynamically created buddy segments.

- [ ] **Step 6: Update recv loop — lazy peer segment opening**

In the recv reply handler (line ~842), change:

```python
# Old:
            seg_mv = self._seg_views[seg_idx]

# New:
            seg_mv = self._seg_views.get(seg_idx)
            if seg_mv is None:
                # Server created a new segment — should not happen in current
                # design (server uses client pool), but guard gracefully.
                return None, error.CompoClientError(f'Unknown reply seg_idx {seg_idx}')
```

And similarly in `_handle_chunked_reply` (line ~890):

```python
# Old:
            if self._buddy_pool is None or seg_idx >= len(self._seg_views):

# New:
            if self._buddy_pool is None or seg_idx not in self._seg_views:
```

- [ ] **Step 7: Run tests**

Run: `uv run pytest tests/ -q --timeout=30 2>&1 | tail -5`
Expected: all tests pass (no regressions from max_segments or dict change).

- [ ] **Step 8: Commit**

```bash
git add src/c_two/transport/client/core.py
git commit -m "feat(client): dynamic pool expansion with lazy seg_views

- PoolConfig max_segments from 1 -> config.max_pool_segments (4)
- _seg_views/base_addrs changed from list to dict
- _cache_local_seg() lazily caches memoryviews for new segments
- Send pool prefix in handshake, store peer prefix
- Recv guards use dict lookup instead of index bounds

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 5: Python Server — Lazy Peer Segment Opening

When the server receives a frame with `seg_idx` not in `conn.seg_views`, it derives the SHM name from `conn.peer_prefix` and opens the segment lazily.

**Files:**
- Modify: `src/c_two/transport/server/connection.py:52-66`
- Modify: `src/c_two/transport/server/handshake.py:54-95,98-138`
- Modify: `src/c_two/transport/server/core.py:640-680,855-880`
- Modify: `src/c_two/transport/server/reply.py:110-136,163-182`

- [ ] **Step 1: Add peer_prefix and change seg_views type in Connection**

In `connection.py`, update the `Connection` dataclass (line ~52):

```python
# Change:
    seg_views: list[memoryview] = field(default_factory=list)
# To:
    seg_views: dict[int, memoryview] = field(default_factory=dict)
```

Add field after `remote_segment_sizes`:

```python
    peer_prefix: str = ''              # client pool prefix for segment name derivation
```

Update `cleanup()`:

```python
    def cleanup(self) -> None:
        self.seg_views = {}  # was self.seg_views = []
        # ... rest unchanged
```

- [ ] **Step 2: Store peer_prefix in handshake + fix max_segments**

In `handshake.py`, `_handle_handshake_impl()` (line ~62), after `hs = decode_handshake(payload)`:

```python
    conn.peer_prefix = hs.prefix
```

In `open_segments()`, change PoolConfig (line ~115):

```python
# Old:
        conn.buddy_pool = BuddyPoolHandle(PoolConfig(
            segment_size=config.pool_segment_size,
            min_block_size=4096,
            max_segments=len(segments),
            max_dedicated_segments=4,
        ))
# New:
        conn.buddy_pool = BuddyPoolHandle(PoolConfig(
            segment_size=config.pool_segment_size,
            min_block_size=4096,
            max_segments=config.max_pool_segments,
            max_dedicated_segments=4,
        ))
```

Change seg_views from list to dict (line ~126):

```python
# Old:
        conn.seg_views = []
        for seg_idx in range(conn.buddy_pool.segment_count()):
            base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            conn.seg_views.append(mv)
# New:
        conn.seg_views = {}
        for seg_idx in range(conn.buddy_pool.segment_count()):
            base_addr, data_size = conn.buddy_pool.seg_data_info(seg_idx)
            mv = memoryview(
                (ctypes.c_char * data_size).from_address(base_addr)
            ).cast('B')
            conn.seg_views[seg_idx] = mv
```

- [ ] **Step 3: Add lazy_open_peer_seg helper in handshake.py**

Add after `open_segments()`:

```python
def lazy_open_peer_seg(conn, seg_idx: int) -> memoryview | None:
    """Lazily open a peer buddy segment by deriving name from conn.peer_prefix."""
    if conn.buddy_pool is None or not conn.peer_prefix:
        return None
    name = f'{conn.peer_prefix}_b{seg_idx:04x}'
    try:
        opened_idx = conn.buddy_pool.open_segment(name, conn.config.pool_segment_size)
        base_addr, data_size = conn.buddy_pool.seg_data_info(opened_idx)
        import ctypes
        mv = memoryview(
            (ctypes.c_char * data_size).from_address(base_addr)
        ).cast('B')
        conn.seg_views[seg_idx] = mv
        logger.debug('Conn %d: lazily opened peer segment %s (idx=%d)',
                     conn.conn_id, name, seg_idx)
        return mv
    except Exception as exc:
        logger.warning('Conn %d: failed to lazy-open peer seg %d (%s): %s',
                       conn.conn_id, seg_idx, name, exc)
        return None
```

- [ ] **Step 4: Update core.py — use lazy open**

Import at top of `core.py`:

```python
from .handshake import lazy_open_peer_seg
```

In `_process_chunked_call` (line ~653):

```python
# Old:
            if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
                return
            seg_mv = conn.seg_views[seg_idx]
# New:
            if conn.buddy_pool is None:
                return
            seg_mv = conn.seg_views.get(seg_idx)
            if seg_mv is None:
                seg_mv = lazy_open_peer_seg(conn, seg_idx)
                if seg_mv is None:
                    return
```

In `_parse_buddy_call` (line ~858):

```python
# Old:
        if conn.buddy_pool is None or seg_idx >= len(conn.seg_views):
            return '', -1, None
        # ... call control parsing ...
        seg_mv = conn.seg_views[seg_idx]
# New:
        if conn.buddy_pool is None:
            return '', -1, None
        seg_mv = conn.seg_views.get(seg_idx)
        if seg_mv is None:
            seg_mv = lazy_open_peer_seg(conn, seg_idx)
            if seg_mv is None:
                return '', -1, None
```

Move the seg_mv access AFTER call control parsing, just before the OOB check. The call control parsing doesn't need seg_mv.

- [ ] **Step 5: Update reply.py — dict seg_views access**

In `send_result_reply()` (line ~120):

```python
# Old:
            seg_mv = conn.seg_views[alloc.seg_idx]
# New:
            seg_mv = conn.seg_views.get(alloc.seg_idx)
            if seg_mv is None:
                conn.buddy_pool.free_at(
                    alloc.seg_idx, alloc.offset, data_size, alloc.is_dedicated,
                )
                raise MemoryError('seg_views missing for reply segment')
```

In `send_chunked_reply()` (line ~173):

```python
# Old:
                seg_mv = conn.seg_views[alloc.seg_idx]
# New:
                seg_mv = conn.seg_views.get(alloc.seg_idx)
                if seg_mv is None:
                    conn.buddy_pool.free_at(
                        alloc.seg_idx, alloc.offset, chunk_len, alloc.is_dedicated,
                    )
                    raise MemoryError('seg_views missing for reply segment')
```

- [ ] **Step 6: Send prefix in server handshake ACK**

In `handshake.py`, `_handle_handshake_impl()`:

```python
# Old:
    ack_payload = encode_server_handshake(
        segments=[],
        capability_flags=cap_flags,
        routes=route_infos,
    )
# New:
    ack_payload = encode_server_handshake(
        segments=[],
        capability_flags=cap_flags,
        routes=route_infos,
        prefix='',  # Server uses client pool, no own prefix
    )
```

- [ ] **Step 7: Run full test suite**

Run: `uv run pytest tests/ -q --timeout=30 2>&1 | tail -5`
Expected: all existing tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/c_two/transport/server/
git commit -m "feat(server): lazy peer segment opening + dict seg_views

- Connection.seg_views: list -> dict[int, memoryview]
- Store peer_prefix from client handshake
- lazy_open_peer_seg() derives name from prefix + seg_idx
- Server pool max_segments uses config (not handshake count)
- All seg_views access uses .get() + lazy open fallback

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

### Task 6: Integration Tests — Multi-Segment Dynamic Expansion

Verify the full E2E flow: client pool expands, server lazily opens new segments, requests and replies work across multiple buddy segments.

**Files:**
- Create: `tests/integration/test_dynamic_pool.py`

- [ ] **Step 1: Write multi-segment integration test**

Create `tests/integration/test_dynamic_pool.py`:

```python
"""Integration tests for dynamic buddy pool expansion.

Verifies that when the client's buddy pool fills segment 0 and
auto-expands to segment 1, the server lazily opens the new segment
and round-trips succeed without per-request SHM degradation.
"""
from __future__ import annotations

import threading
import time

import pytest

import c_two as cc
from c_two.transport.ipc.frame import IPCConfig


# Use small segments so we can fill them quickly in tests.
_SMALL_CONFIG = IPCConfig(
    pool_segment_size=64 * 1024,    # 64 KB segments
    max_pool_segments=4,
    shm_threshold=256,              # Low SHM threshold
    max_frame_size=16 * 1024 * 1024,
)


@cc.icrm(namespace='cc.test.dynpool', version='0.1.0')
class IEchoLarge:
    def echo(self, data: bytes) -> bytes:
        ...

    def alloc_pressure(self, size: int) -> int:
        """Allocate and return `size` bytes of data to create memory pressure."""
        ...


class EchoLarge:
    def echo(self, data: bytes) -> bytes:
        return data

    def alloc_pressure(self, size: int) -> int:
        return len(b'x' * size)


@pytest.fixture
def dynpool_server():
    """Start a server with small pool segments."""
    address = f'ipc://test_dynpool_{threading.get_ident()}'
    cc.register(IEchoLarge, EchoLarge(), name='echo', ipc_config=_SMALL_CONFIG)
    cc.set_address(address)
    yield address
    cc.unregister('echo')
    cc.shutdown()


class TestDynamicPoolExpansion:
    """Verify multi-segment buddy pool expansion."""

    def test_large_payloads_span_segments(self, dynpool_server):
        """Payloads that exceed segment 0 should auto-expand to segment 1."""
        proxy = cc.connect(
            IEchoLarge, name='echo',
            address=dynpool_server,
            ipc_config=_SMALL_CONFIG,
        )
        try:
            # Each call sends ~16KB. With 64KB segments, ~4 concurrent
            # allocs would fill segment 0. Send enough to force expansion.
            results = []
            for i in range(10):
                data = bytes([i % 256]) * 16_000
                result = proxy.echo(data)
                assert result == data
                results.append(len(result))
            # All 10 calls succeeded — pool expanded as needed.
            assert len(results) == 10
        finally:
            cc.close(proxy)

    def test_alloc_pressure_forces_expansion(self, dynpool_server):
        """Call that returns a large response — server pool reply path works."""
        proxy = cc.connect(
            IEchoLarge, name='echo',
            address=dynpool_server,
            ipc_config=_SMALL_CONFIG,
        )
        try:
            result = proxy.alloc_pressure(32_000)
            assert result == 32_000
        finally:
            cc.close(proxy)
```

- [ ] **Step 2: Run integration test**

Run: `uv run pytest tests/integration/test_dynamic_pool.py -v --timeout=30 2>&1 | tail -15`
Expected: all tests pass.

- [ ] **Step 3: Run full regression suite**

Run: `uv run pytest tests/ -q --timeout=30 2>&1 | tail -5`
Expected: 717+ passed, no new failures.

- [ ] **Step 4: Commit**

```bash
git add tests/integration/test_dynamic_pool.py
git commit -m "test: integration tests for dynamic buddy pool expansion

Verify multi-segment expansion: large payloads span segments,
server lazily opens new segments, round-trips succeed.

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

---

## Notes

- **Dedicated segments** remain rejected by client and server reply paths (Phase 2 concern). With 4 buddy segments (1 GB), dedicated fallback is rare.
- **Wire format** stays at u16 seg_idx + `is_dedicated` flag. Expansion to u32 + `tier` enum deferred to Phase 2 (disk spillover).
- **gc_buddy()** is safe: it only pops trailing empty segments. If segment 3 is empty, gc pops it. If segment 2 was later recreated, Rust would use Vec index = segments.len() for naming, which may produce a different name. To avoid this, do NOT call gc_buddy while segments are actively expanding. Phase 3 addresses this with HashMap refactor.
- **No backward compat needed** — 0.x.x version, no production users.
- **Server pool vs client pool**: The server ONLY opens client's segments (never creates its own). `max_segments` in server pool only prevents unwanted Layer-2 creation. Layer-1 iterates ALL opened segments regardless of max_segments.
- **UDS FIFO guarantee**: When client sends a frame referencing seg_idx=1, the segment was created BEFORE the frame was sent. UDS preserves ordering, so the server receives the frame AFTER the SHM is globally visible.
