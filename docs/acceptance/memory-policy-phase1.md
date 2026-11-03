# Memory Policy Phase 1A: Buddy Policy Enforcement, Lazy Startup, Safe Idle-to-Zero

Date: 2026-09-26 · Base commit: `8777e2dd157942fe6f3cc90389763af35f5056bd` · Scope: reviewed memory optimization plan `/tmp/c-two-memory-0.6.0-review.md`, Phase 1A only.

This report covers the complete two-attempt patch: the Rust core work from the first attempt (verified by the Host against this checkout) plus the continuation work (real IPC round-trip regressions, Python typed/native projections, native compile proof).

## Policy Summary

One fallback system is preserved end to end: buddy reuse → buddy expansion → dedicated SHM → chunked local transport, with receiver file-backed fallback where an API has one. Disabling `pool_enabled` skips only the buddy tiers; dedicated SHM, chunked transfer, inline frames, and file spill remain. Chunking stays a transport mechanism, not a streaming resource API. Payload checks, wire generation safety, request/response directionality, lease accounting, and FastDB checked invalidation are untouched.

## Source Changes (19 files)

### c2-config — policy fields, validation, centralized projection

- `BaseIpcConfig` gains `pool_prewarm_segments` (default 0) and `pool_min_retained_segments` (default 0). `pool_enabled` is explicitly documented as a buddy-only switch. New env keys `C2_IPC_POOL_PREWARM_SEGMENTS` and `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` resolve through the Rust resolver (`resolver.rs`), and both new keys are legal server/client code-level overrides.
- Validation rejects: prewarm above `max_pool_segments`, prewarm with buddy disabled, min-retained above `max_pool_segments`, and min-retained above `reassembly_max_segments`.
- `PoolConfig` gains `buddy_enabled` (default true) and `min_retained_segments` (default 1 for direct native users; IPC transports project their own setting, default 0).
- New `PoolRoleTuning` plus `BaseIpcConfig::primary_pool_config()` / `reassembly_pool_config()` and `ServerIpcConfig::response_pool_tuning()` / `reassembly_pool_tuning()` centralize config-to-PoolConfig projection. A disabled buddy still yields a real pool with dedicated capacity — never `None`, which would lose dedicated and `CAP_CHUNKED`.

### c2-mem — policy-gated allocation, lazy prewarm, retire-to-zero

- `buddy_block_limit()` returns 0 when buddy is disabled, so `alloc`, `alloc_handle`, and `try_alloc_shm` skip the buddy tiers — including reuse of already-cached segments — and go straight to dedicated SHM (or the API's file-spill fallback).
- `ensure_ready()` is a policy-gated no-op for disabled pools; `ensure_buddy_segments(count)` is the only prewarm entry point and is rejected outright when buddy is disabled. No unconditional eager mapping remains in any transport.
- `gc_buddy()` retires trailing idle segments down to `min_retained_segments` (which may be 0) while preserving generation counters; a re-created slot gets a fresh generation, and stale coordinates naming a retired backing are rejected. Live allocations are never retired.
- Buddy creation failure (for example an SHM name collision) falls through to dedicated storage instead of failing the allocation or jumping to file spill; buddy generation exhaustion likewise falls through to dedicated.
- Dedicated size representability (`required_region_size ≤ u32::MAX`) is checked before `DedicatedSegment::create`, with the post-create guard kept defensively. `MemPool::config()` exposes the effective policy for diagnostics.

### c2-ipc — lazy handshake, policy-projected pools, capability independence

- `IpcClient::own_pool_from_config` always builds a config-owned pool through `primary_pool_config`; `with_config` no longer drops the pool when `pool_enabled=false` (dedicated requests and the wire prefix stay alive).
- `make_chunk_registry` projects `reassembly_pool_config`, so chunked reception reassembles into dedicated SHM when buddy is disabled.
- Connect-time prewarm is explicit only: `pool_prewarm_segments > 0` maps buddy memory before the handshake; the default 0 announces an empty segment list and stays fully lazy. The pooled-client acquire path (`pool.rs`) follows the same projection and prewarm rule.
- `CAP_CHUNKED` is advertised independently of pool state — chunked response reassembly always exists.
- `ServerPoolState::ensure_segment` and the server's `PeerShmState::ensure_peer_segment` open peer backings with the frame's advertised data span; no local segment-size default is substituted for actual peer geometry. Empty announcements and differing client/server segment sizes are supported on the current generation protocol (no wire protocol bump).
- `c2-wire` changes are exactly two one-line test-fixture additions (`..PoolConfig::default()`) for the new `PoolConfig` fields; no production or promotion logic was touched.

### c2-server — lazy response pool, projected roles, maintenance-driven GC

- Response and reassembly pools are built through the centralized projections (`response_pool_tuning` carries `pool_decay_seconds`). The response pool starts unmapped; only explicit `pool_prewarm_segments` maps buddy memory at construction.
- The existing periodic GC sweep task (the chunk-registry maintenance loop) additionally drives `response_pool.gc_buddy()` and the reassembly pool's `gc_buddy()` — no new thread or per-allocation task.
- `init_peer_shm` treats the first announced segment size as diagnostic only; backings open with the frame's data span and the SHM header supplies real geometry.

### Python SDK — typed facades and native projection (forward typed config only)

- `sdk/python/src/c_two/config/ipc.py`: `BaseIPCOverrides` gains `pool_prewarm_segments` and `pool_min_retained_segments` with buddy-only semantics documented. No Python-side validation tables; Rust owns key validation.
- `sdk/python/native/src/config_ffi.rs`: the two fields round-trip through server/client override parsing, override serialization, and the resolved config dict (`base_ipc_to_dict`).
- `sdk/python/native/src/mem_ffi.rs`: one-line fixture — the low-level `PyPoolConfig` conversion takes pool policy from `PoolConfig::default()` since IPC policy is owned by the config resolver.
- `sdk/python/tests/unit/test_ipc_config.py`: focused tests for lazy defaults, override/env resolution, and Rust-side rejection of invalid combinations.

## Regression Tests

### Rust — c2-mem / c2-config unit level (first attempt)

Config: defaults are lazy and retire-to-zero; prewarm/min-retained boundary and disabled-combination rejections; projection of policy into primary and reassembly pool configs. Memory: disabled buddy skips buddy tiers in every allocation API while keeping file spill; buddy backing-name collisions fall through to dedicated; retire-to-zero with fresh generations on re-create and stale-coordinate rejection; retire only down to configured floor; live allocations survive idle GC; dedicated 4 GiB representability rejection before mapping.

### Rust — c2-ipc real round-trip level (continuation, new)

A new `lazy_policy_roundtrip_tests` module drives a real `c2-server` (real route registration, real run loop, real local stream) from a real `IpcClient` with contract-checked route acquisition. The echo callback records which request transport it actually observed, so the tests assert real wire/transport behavior rather than config fields:

- `lazy_startup_maps_no_buddy_memory_and_serves_one_byte_ipc` — with default policy the handshake announces zero segments, neither side maps buddy memory, and a one-byte inline call changes nothing.
- `disabled_buddy_serves_dedicated_large_request_and_response` — 64 KiB round trip with buddy disabled: the callback receives dedicated SHM coordinates, the reply returns as `ResponseData::Shm { is_dedicated: true }`, no buddy segments are mapped on either side, and the server's dedicated tier is provably used.
- `disabled_buddy_falls_back_to_chunked_reply_when_dedicated_exhausts` — the callback holds every dedicated response segment; the reply falls back to chunked transfer and the client reassembles it into its own policy-disabled (dedicated-backed) reassembly pool.
- `disabled_buddy_chunked_request_reassembles_into_dedicated_shm` — a chunked request (below SHM threshold, above chunk size) with buddy disabled arrives at the callback as a reassembled `RequestData::Handle`.
- `lazy_buddy_created_after_empty_handshake_with_unequal_segment_sizes` — client 256 KiB vs server 64 KiB geometry, empty announcement; the first large call lazily creates the client's first buddy segment, the server lazy-opens it from the frame's coordinates without trusting its own 64 KiB default, and the buddy reply round-trips with one lazily created segment on each side.

## Test Results (real commands, this checkout)

```bash
CARGO_TARGET_DIR=/tmp/c2-memory-host-build CARGO_BUILD_JOBS=2 \
  cargo test --manifest-path core/Cargo.toml -p c2-ipc -p c2-config -p c2-mem -p c2-wire
```

- c2-config: `76 passed; 0 failed`
- c2-ipc: `64 passed; 0 failed` (59 prior + 5 new round-trip tests)
- c2-mem: `108 passed; 0 failed`
- c2-wire: `117 passed; 0 failed` (verifies the two fixture lines)

Full log: `/tmp/c2-memory-phase1a-continuation-tests.log`.

c2-server is unchanged in the continuation and was verified by the Host against this exact checkout: 141 passed (see `/tmp/c2-memory-lazy-policy-partial-tests.log`, which also contains the first attempt's c2-config 76 / c2-mem 108 / c2-ipc 59 results).

Native extension compile proof:

```bash
PYO3_PYTHON=/tmp/c2-memory-venv/bin/python \
FASTDB_PAYLOAD_LINK_MODE=system \
FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib \
DYLD_LIBRARY_PATH=/private/tmp/c2-memory-fastdb-sdk/lib \
CARGO_TARGET_DIR=/tmp/c2-memory-host-build CARGO_BUILD_JOBS=2 \
  cargo check --manifest-path sdk/python/native/Cargo.toml
```

Result: `Finished 'dev' profile [unoptimized + debuginfo] target(s)` — the PyO3 projection including the new fields compiles against the official FastDB SDK.

Python-side: `sdk/python/tests/unit/test_ipc_config.py` and `sdk/python/src/c_two/config/ipc.py` pass `py_compile` under `/tmp/c2-memory-venv/bin/python`.

## Integration Boundaries

- The Python behavioral tests (`test_ipc_config.py` additions: lazy defaults, override/env resolution, Rust-side rejection of invalid combinations) require the rebuilt integrated `c_two._native` extension, which the Host owns; they were syntax-checked but not executed here.
- `AGENTS.md` (environment-variable table) and `.env.example` are outside this task's write scope; the two new env vars `C2_IPC_POOL_PREWARM_SEGMENTS` and `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` should be added there at integration time.
- The two c2-wire fixture lines will be merged by the Host alongside the separately accepted removal of automatic promotion; this patch does not touch promotion production logic.
- Same-process direct calls are untouched: the thread-local transport never enters the Rust dispatch path.
- Byte-budget design is explicitly deferred to the Host's next slice, as are client-cache refactors.

## Corrective review and Host verification

The initial artifact was rejected for an injected-pool buddy-policy bypass, missing periodic client GC, unchecked dedicated page alignment, and a receiver-side 16-segment bound below the canonical maximum. The corrected code rejects mismatched injected policy before connection I/O, runs one weakly owned cancellable client maintenance task (buddy + dedicated GC and stale chunk sweep), uses checked dedicated sizing, and tests index 16 in both directions with the canonical 255 bound. Client `pool_decay_seconds` now resolves through Rust/native/Python with nonnegative, finite, representable duration validation. Server maintenance also sweeps dedicated storage.

Two Host corrections were needed after the final attempt timed out: move the maintenance Option out of its synchronous lock before awaiting shutdown, and use RequestLease in echo/held-request test callbacks so request SHM is actually released. A temporary diagnostic loop that re-locked its own mutex was removed. Assertions for cleanup, held data, and peer index 16 remain.

Host reran the corrected checkout with CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/tmp/c2-memory-policy-build cargo test --manifest-path core/Cargo.toml -p c2-config -p c2-mem -p c2-wire -p c2-ipc -p c2-server: config 81, mem 110, wire 117, IPC 73, server 141 passed (522 total). Log: /tmp/c2-memory-policy-host-acceptance-tests.log. This supersedes earlier test counts and paths. Windows remains pending.

Integration must retain accepted backing-budget guards and its canonical dedicated size helper (including isize bound), and compose maintenance shutdown with the independently reviewed Runtime cache close barrier. These independently passing slices still need combined tests.

### Integrated source validation

The Host merged this policy with accepted backing-budget commit `72b23e4` and Runtime-cache commit `6b017f2`. The single checked dedicated geometry helper and all backing guards are retained; creation failures continue to the next eligible tier. Maintenance shutdown is composed into the cache close gate and one deadline, with timed-out task handles retained for retry. A new deterministic close/maintenance retry test verifies this boundary.

Combined macOS tests passed: config 83, mem 137, wire 119, IPC 85, server 142, Core 61 (627 total), plus the focused maintenance test. Core workspace `--all-targets` and Python native `cargo check` passed on the combined tree. Python behavioral execution, transport shared-budget wiring and Windows CI remain pending.

A separate composition review caught cancellation bypassing the receive loop footer. The receive task now owns a scoped cleanup guard for incomplete connection assemblies. A fake peer sends a partial response then stays silent; the regression verifies cleanup after forced abort, reconnect of the same client, and Drop while external registry/pool Arcs remain. Completed response owners are not registry entries and remain untouched. The corrected IPC suite passed 85 tests.
