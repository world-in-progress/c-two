# C-Two Comprehensive Audit Report

**Date:** 2026-07-20  
**Branch:** `dev-feature` (base `2e842ae`)  
**Python:** 3.14.3 free-threading build (Clang 17.0.0)  
**Test Baseline:** 711 tests pass (`C2_RELAY_ADDRESS= uv run pytest tests/ -q --timeout=30`)

---

## Table of Contents

1. [Free-Threading Concurrency Safety Audit](#1-free-threading-concurrency-safety-audit)
2. [Architecture Quality Review](#2-architecture-quality-review)
3. [Three-Mode Benchmark Results](#3-three-mode-benchmark-results)
4. [Recommendations](#4-recommendations)

---

## 1. Free-Threading Concurrency Safety Audit

**Target runtime:** CPython 3.14.3t (free-threading / no-GIL)

Under GIL-free execution, every shared mutable state must be explicitly protected. The following analysis categorizes all shared state in the transport and CRM layers.

### 1.1 CRITICAL — Must Fix Before 3.14t Deployment

#### C1: `_conn_counter` unsynchronized increment
- **File:** `transport/server/core.py:423-424`
- **Issue:** `_conn_counter += 1` is a read-modify-write on a plain `int`. Under free-threading, concurrent `_on_connect` calls can produce duplicate connection IDs.
- **Impact:** Duplicate conn IDs → routing collisions → data corruption
- **Fix:** Wrap in existing `_lock`, or use `threading.Lock` for this specific counter.

#### C2: `_TRANSFERABLE_MAP` / `_TRANSFERABLE_INFOS` mutated without lock
- **File:** `crm/transferable.py:26-27, 58, 66, 271`
- **Issue:** `TransferableMeta.__new__()` and `register_transferable()` mutate these module-level dicts without any synchronization. Under free-threading, concurrent class definitions or dynamic imports can race.
- **Impact:** Lost registrations → serialization failures at runtime
- **Fix:** Add a module-level `threading.Lock` guarding all mutations. Read-only access after startup is safe.

### 1.2 HIGH — Should Fix

#### H1: `_client_tasks` list mutations
- **File:** `transport/server/core.py:342, 428, 640`
- **Issue:** `append()` and `remove()` on a plain `list` from different coroutine tasks. Although asyncio is single-threaded by default, the server uses `loop.call_soon_threadsafe()` from worker threads.
- **Fix:** Use `asyncio.Lock` or convert to a `set` protected by `_lock`.

#### H2: `_closed` / `_running` flag reads outside `_close_lock`
- **File:** `transport/client/core.py:172-173, 298-300, 314, 382`
- **Issue:** `_closed` and `_running` are read in hot paths without acquiring `_close_lock`. Under free-threading, the recv thread can see stale values → use-after-close.
- **Fix:** Use `threading.Event` for `_running` and `_closed`, or read under lock.

#### H3: `_seg_views` dict mutations without lock
- **File:** `transport/client/core.py:285-292, 348, 759`
- **Issue:** The recv thread adds entries to `_seg_views`; `terminate()` clears it. No lock protects these concurrent accesses.
- **Fix:** Protect with `_close_lock` or a dedicated `_seg_lock`.

#### H4: `_buddy_pool` TOCTOU race
- **File:** `transport/client/core.py:757-760`
- **Issue:** recv thread checks `_buddy_pool is not None`, then `terminate()` sets it to `None` → `AttributeError` or segfault in Rust FFI.
- **Fix:** Snapshot under lock: `pool = self._buddy_pool` under `_close_lock`, then use local ref.

### 1.3 MEDIUM — Low Probability but Worth Hardening

#### M1: Stale `_slots` dict reference
- **File:** `transport/server/core.py:437-441, 566-567`
- **Issue:** `_slots` is copied outside `_lock` for iteration. A concurrent registration can produce a stale snapshot with mismatched `_slots_generation`.
- **Fix:** Copy under lock or use `copy()` atomically.

#### M2: `_default_name` reads outside lock
- **File:** `transport/server/core.py:210-211, 225-226, 438, 543`
- **Issue:** `_default_name` is read without `_lock` in several paths.
- **Fix:** Read under `_lock`.

### 1.4 SAFE Components ✅

| Component | Why Safe |
|-----------|----------|
| `_ProcessRegistry` singleton | Double-checked locking with `_cls_lock` |
| `_pending` dict (client) | All mutations under `_pending_lock` |
| `_rid_counter` (client) | Incremented under `_pending_lock` |
| `ClientPool` ref-counting | All mutations under `_pool_lock` |
| `FastDispatcher` | Uses `queue.SimpleQueue` (thread-safe) + `loop.call_soon_threadsafe()` |
| `Scheduler` | `asyncio.Semaphore` + `asyncio.Lock` |
| Rust `BuddyPool` (c2-mem) | `RwLock<BuddyPool>` in Rust, GIL released via `py.allow_threads()` |

---

## 2. Architecture Quality Review

### 2.1 Zombie Code (Dead / Unreachable)

#### Z1: Unused error code constants
- **File:** `error.py:4-16`
- **Symbols:** `ERROR_AT_CRM_SERVER`, `ERROR_AT_CRM_DESERIALIZE_INPUT`, `ERROR_AT_CRM_SERIALIZE_OUTPUT`, `ERROR_AT_CRM_EXECUTE_FUNCTION`, `ERROR_AT_COMPO_CLIENT`, `ERROR_AT_COMPO_SERIALIZE_INPUT`, `ERROR_AT_COMPO_DESERIALIZE_OUTPUT`
- **Status:** These 7 integer constants are defined but never referenced in production code. Error types use the `CCError` subclass hierarchy with `ERROR_Code` class attributes instead.
- **Recommendation:** Delete. They add confusion about which error mechanism is canonical.

#### Z2: MCP integration exports (unused)
- **File:** `mcp/__init__.py`
- **Symbols:** `register_mcp_tool_from_compo_funcs`, `register_mcp_tools_from_compo_module`
- **Status:** Exported in `__init__.py` and `__all__`, but no code in the repository calls them. No tests exist.
- **Recommendation:** Keep if MCP integration is a planned feature; document as experimental. Otherwise, delete.

### 2.2 Dangling Logic (Incomplete Wiring)

#### D1: CLI `--max-segments` option broken — **HIGH BUG**
- **File:** `cli.py:193`
- **Issue:** `kwargs['pool_max_segments']` should be `kwargs['max_pool_segments']`. The CLI argument parser produces `max_pool_segments` (matching the argparse `dest`), but the code reads `pool_max_segments` → **`KeyError`** every time `--max-segments` is passed.
- **Impact:** `c3 build --max-segments 8` crashes with a traceback. The default path (no flag) works because it skips the kwarg entirely.
- **Fix:** Change to `kwargs['max_pool_segments']` or rename the argparse dest.

#### D2: Relay error serialization mismatch
- **File:** `transport/relay/core.py:483-500`
- **Issue:** When a non-`CCError` exception occurs in the relay's forwarding path, it is encoded as a raw UTF-8 string. However, the client deserializer expects binary `CCError` format (4-byte error code prefix). This causes a secondary deserialization error that masks the original exception.
- **Impact:** Generic Python exceptions from CRM code produce cryptic "failed to deserialize error" messages at the client instead of the actual error text.
- **Fix:** Wrap non-CCError exceptions in `CCError` before serialization, or introduce a generic wire error format.

#### D3: Relay shutdown leak on thread death
- **File:** `transport/relay/core.py:327-345`
- **Issue:** If `_serve_thread` dies unexpectedly (e.g., unhandled exception in uvicorn), `_pool.shutdown()` is never called. The buddy pool's SHM segments and UDS sockets are leaked until process exit.
- **Fix:** Add a `try/finally` in the serve thread wrapper, or a `threading.Thread.join()` with cleanup in `stop()`.

#### D4: Proxy `_closed` flag inconsistency
- **File:** `transport/client/proxy.py:167-214`
- **Issue:** `ICRMProxy.__call__()` checks `_closed` at entry, but the `__getattr__` path does not. A proxy obtained via attribute access (`proxy.method(...)`) can race with a concurrent `close()` call.
- **Fix:** Unify the check — either in `__getattr__` or in the underlying dispatch.

---

## 3. Three-Mode Benchmark Results

### 3.1 Environment

| Item | Value |
|------|-------|
| CPU | Apple Silicon (arm64) |
| Python | 3.14.3 free-threading build |
| Buddy segment | 2 GB × 8 segments |
| Relay | NativeRelay (axum, Rust) |
| Warmup | 3 calls per size |
| Rounds | 100 (≤1MB), 20 (10-100MB), 5 (500MB-1GB) |
| Metric | P50 (median) round-trip latency |

### 3.2 Latency Table — bytes payload (P50, milliseconds)

> **Note:** The bytes echo CRM uses the identity fast-path (`transferable.py`): single `bytes` parameter and `bytes` return type skip pickle entirely. See §3.5 for the dict payload comparison that reveals true serialization cost.

| Size | Rounds | Thread | IPC | Relay | IPC/Thread | Relay/IPC |
|-----:|-------:|-------:|----:|------:|-----------:|----------:|
| 64B | 100 | 0.003 | 0.129 | 0.864 | 40× | 6.7× |
| 256B | 100 | 0.003 | 0.195 | 1.041 | 58× | 5.3× |
| 1KB | 100 | 0.003 | 0.190 | 1.137 | 58× | 6.0× |
| 4KB | 100 | 0.003 | 0.201 | 0.980 | 62× | 4.9× |
| 64KB | 100 | 0.003 | 0.323 | 1.941 | 97× | 6.0× |
| 1MB | 100 | 0.004 | 0.500 | 14.51 | 134× | 29× |
| 10MB | 20 | 0.003 | 2.134 | 145.4 | 640× | 68× |
| 50MB | 20 | 0.003 | 9.017 | 751.5 | 2,689× | 83× |
| 100MB | 20 | 0.004 | 17.87 | 1,558 | 5,137× | 87× |
| 500MB | 5 | 0.004 | 139.3 | 8,687 | 33,105× | 62× |
| 1GB | 5 | 0.009 | 1,082 | 21,953 | 118,623× | 20× |

> Relay now works for all sizes after fixing axum body limit + IPC max frame size (see §3.4).

### 3.3 Analysis

**Thread-local mode** is essentially free (~3 µs) regardless of payload size. This confirms the zero-serialization design: `call_direct()` passes Python objects by reference with no copy.

**IPC mode** shows clear linear scaling with payload size:
- **Small payloads (≤4KB):** ~0.13-0.20 ms, dominated by UDS round-trip + buddy alloc/free overhead.
- **Medium payloads (64KB-1MB):** 0.3-0.5 ms, SHM memcpy cost becomes visible.
- **Large payloads (10-100MB):** 2-18 ms, approaching ~5.6 GB/s effective throughput (100MB in 18ms).
- **Huge payloads (500MB-1GB):** 139ms-1.1s. At 1GB, effective throughput is ~920 MB/s.

> ⚠️ **These IPC numbers use the bytes identity fast-path** — no pickle serialization. Real-world dict/object payloads are ~2-2.5× slower at ≥10MB (see §3.5).

**Relay (HTTP) mode** works end-to-end for all sizes after two fixes (see §3.4):
- **Small payloads (≤64KB):** ~1 ms, dominated by HTTP round-trip overhead (~5-6× IPC).
- **Medium payloads (1-10MB):** 14-145 ms, HTTP encode/decode + two extra memcpy (~29-68× IPC).
- **Large payloads (50-100MB):** 0.75-1.6 seconds (~83-87× IPC). The relay buffers the entire payload in memory (no SHM).
- **Huge payloads (500MB-1GB):** 8.7-22 seconds. At 1GB, the relay throughput is ~47 MB/s vs IPC's 920 MB/s.
- **Remaining optimization:** SHM-backed relay forwarding would eliminate the HTTP buffering overhead for large payloads.

**Throughput equivalents (IPC, bytes fast-path):**
- 64B @ 0.13ms → ~7,700 calls/sec (~480 KB/s)
- 1MB @ 0.50ms → ~2,000 calls/sec (~2.0 GB/s)
- 100MB @ 18ms → ~56 calls/sec (~5.6 GB/s)
- 1GB @ 1.08s → ~0.9 calls/sec (~920 MB/s)

### 3.4 Relay Payload Limit Fixes (Resolved)

Two limits prevented the relay from handling large payloads:

**1. Axum HTTP body limit (2MB → disabled)**
- `c2-relay/src/router.rs`: Added `.layer(DefaultBodyLimit::disable())` to the axum router.
- Previously, axum's implicit 2MB `Bytes` extractor limit caused HTTP 413 for any payload >2MB.

**2. IPC max frame size (16MB → 2GB)**
- `transport/ipc/frame.py`: `DEFAULT_MAX_FRAME_SIZE` raised from 16MB to 2GB.
- The relay sends payloads inline on UDS (no SHM), so the server's frame size check rejected frames >16MB with "Frame too large", causing broken pipe (HTTP 502).

### 3.5 Serialization Overhead: bytes vs dict (IPC mode)

The bytes echo benchmark uses the `@transferable` identity fast-path: when an ICRM method has a single `bytes` parameter and `bytes` return type, `transferable.py` skips pickle entirely (4 pickle ops saved per round-trip). To quantify this bias, we ran a parallel benchmark using `dict→dict` payloads (`{'payload': b'\xAB' * N}`) which go through the full pickle serialization path.

| Size | IPC-bytes (ms) | IPC-dict (ms) | dict/bytes | Pickle overhead |
|-----:|---------------:|--------------:|-----------:|----------------:|
| 64B | 0.129 | 0.207 | 1.6× | 38% |
| 256B | 0.195 | 0.186 | 1.0× | ~0% |
| 1KB | 0.190 | 0.215 | 1.1× | 10% |
| 4KB | 0.201 | 0.344 | 1.7× | 41% |
| 64KB | 0.323 | 0.353 | 1.1× | 9% |
| 1MB | 0.500 | 0.717 | 1.4× | 30% |
| 10MB | 2.134 | 4.374 | 2.0× | 51% |
| 50MB | 9.017 | 23.65 | 2.6× | 62% |
| 100MB | 17.87 | 45.36 | 2.5× | 61% |

> dict benchmarks capped at 100MB (pickle becomes prohibitively slow beyond this).

**Key findings:**
- **Small payloads (≤1KB):** Pickle overhead is negligible (~0-10%). UDS round-trip dominates.
- **Medium payloads (4KB-1MB):** Pickle contributes 30-41% of latency. Noticeable but not dominant.
- **Large payloads (≥10MB):** Pickle contributes **51-62% of total latency** — more than the SHM memcpy. The bytes fast-path benchmark understates real-world latency by ~2-2.5×.
- **Conclusion:** The bytes identity fast-path is a valid optimization for binary data (e.g., Arrow, Parquet, numpy). For structured Python objects, users should expect ~2× the IPC latency shown in §3.2.

---

## 4. Recommendations

### Priority 1 — Fix Before Production

| ID | Category | Severity | Effort |
|----|----------|----------|--------|
| C1 | Free-threading | CRITICAL | S (add lock) |
| C2 | Free-threading | CRITICAL | S (add lock) |
| D1 | CLI bug | HIGH | XS (rename key) |

### Priority 2 — Fix Before 3.14t Certification

| ID | Category | Severity | Effort |
|----|----------|----------|--------|
| H1 | Free-threading | HIGH | S |
| H2 | Free-threading | HIGH | M (refactor flag pattern) |
| H3 | Free-threading | HIGH | S |
| H4 | Free-threading | HIGH | S |
| D2 | Relay error | HIGH | M |

### Priority 3 — Hardening

| ID | Category | Severity | Effort |
|----|----------|----------|--------|
| M1 | Free-threading | MEDIUM | S |
| M2 | Free-threading | MEDIUM | S |
| D3 | Relay cleanup | MEDIUM | S |
| D4 | Proxy consistency | MEDIUM | S |
| Z1 | Dead code | LOW | XS (delete) |

### Priority 4 — Performance

| Item | Action |
|------|--------|
| Relay body limit | ~~Configure `DefaultBodyLimit::max()` in axum router~~ ✅ Fixed |
| Relay large payloads | ~~Implement chunked transfer or streaming body~~ ✅ Fixed (inline + raised frame limit) |
| IPC 1GB throughput | Consider `io_uring` or `splice()` for zero-copy SHM → UDS |

---

*Report generated by automated audit. Benchmark script: `benchmarks/three_mode_benchmark.py`. Raw data: `benchmark_results.tsv`.*

