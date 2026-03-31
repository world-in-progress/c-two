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

### 3.2 Latency Table (P50, milliseconds)

| Size | Rounds | Thread | IPC | Relay | IPC/Thread | Relay/Thread |
|-----:|-------:|-------:|----:|------:|-----------:|-------------:|
| 64B | 100 | 0.0096 | 0.354 | 3.379 | 36.7× | 350.3× |
| 256B | 100 | 0.0100 | 0.459 | 1.750 | 46.1× | 175.7× |
| 1KB | 100 | 0.0093 | 0.337 | 2.060 | 36.1× | 220.7× |
| 4KB | 100 | 0.0100 | 0.426 | 2.079 | 42.6× | 207.9× |
| 64KB | 100 | 0.0097 | 0.592 | 5.965 | 61.0× | 614.4× |
| 1MB | 100 | 0.0098 | 1.367 | 59.915 | 139.0× | 6,093× |
| 10MB | 20 | 0.0096 | 9.271 | — | 969.6× | — |
| 50MB | 20 | 0.0094 | 39.851 | — | 4,260× | — |
| 100MB | 20 | 0.0094 | 85.047 | — | 9,011× | — |
| 500MB | 5 | 0.0132 | 473.4 | — | 35,951× | — |
| 1GB | 5 | 0.0164 | 2,215.1 | — | 135,276× | — |
| **GeoMean** | | **0.0104** | **6.301** | **4.565** | | |

> **"—"** = Relay returned HTTP 413 (axum body size limit exceeded).

### 3.3 Analysis

**Thread-local mode** is essentially free (~10 µs) regardless of payload size. This confirms the zero-serialization design: `call_direct()` passes Python objects by reference with no copy.

**IPC mode** shows clear linear scaling with payload size:
- **Small payloads (≤4KB):** ~0.3-0.5 ms, dominated by UDS round-trip + buddy alloc/free overhead.
- **Medium payloads (64KB-1MB):** 0.6-1.4 ms, serialization cost becomes visible.
- **Large payloads (10-100MB):** 9-85 ms, approaching memcpy throughput limits (~1.2 GB/s effective for 100MB in 85ms).
- **Huge payloads (500MB-1GB):** 0.5-2.2 seconds. At 1GB, the effective throughput is ~450 MB/s (serialize + SHM copy + deserialize).

**Relay (HTTP) mode** works for payloads ≤1MB but fails for ≥10MB:
- At 1MB, latency is ~60ms (44× IPC overhead from HTTP encode/decode + two extra copies).
- At ≥10MB, axum's default request body limit (likely 2MB or configurable) triggers HTTP 413 rejection.
- **Action required:** Configure `axum::extract::DefaultBodyLimit` in `c2-relay` to support larger payloads, or implement chunked transfer encoding.

**Throughput equivalents (IPC):**
- 64B @ 0.35ms → ~2,857 calls/sec (~178 KB/s)
- 1MB @ 1.37ms → ~730 calls/sec (~730 MB/s)
- 100MB @ 85ms → ~11.8 calls/sec (~1.18 GB/s)
- 1GB @ 2.2s → ~0.45 calls/sec (~450 MB/s, limited by 2× memcpy)

### 3.4 Relay Body Limit Root Cause

The NativeRelay uses axum with the default body size limit. In `c2-relay/src/handler.rs` (or equivalent), the route handlers do not override `DefaultBodyLimit`. Axum's default is 2MB for JSON extractors and unlimited for `Body` — but the relay likely uses `Bytes` extractor which has a 2MB default.

**Fix:** Add `.layer(DefaultBodyLimit::max(2 * 1024 * 1024 * 1024))` to the axum router, or use streaming body extraction.

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
| Relay body limit | Configure `DefaultBodyLimit::max()` in axum router |
| Relay large payloads | Implement chunked transfer or streaming body |
| IPC 1GB throughput | Consider `io_uring` or `splice()` for zero-copy SHM → UDS |

---

*Report generated by automated audit. Benchmark script: `benchmarks/three_mode_benchmark.py`. Raw data: `benchmark_results.tsv`.*

