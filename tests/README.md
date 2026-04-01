# Tests & Benchmarks

## Running Tests

```bash
# Full test suite (855 tests, ~80s on Apple Silicon)
uv run pytest -q

# Single test file
uv run pytest tests/unit/test_encoding.py -q

# Single test function
uv run pytest tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Run with verbose output
uv run pytest -v --timeout=30
```

All tests use a **30-second per-test timeout**. Verified on Python 3.14t (free-threaded).

---

## Unit Tests (`tests/unit/`)

### CRM Core

| File | Description |
|------|-------------|
| `test_icrm_decorator.py` | `@cc.icrm()` decorator, namespace/version validation, method registration |
| `test_crm_template.py` | CRM template generation from ICRM interface classes |
| `test_encoding.py` | Wire protocol encoding/decoding, message serialization |
| `test_error.py` | `CCError` hierarchy — serialization/deserialization across wire |
| `test_transferable.py` | `@transferable` decorator — serialize/deserialize, auto-dataclass, scatter-write tuples, memoryview-aware deserialize, bytes fast-path |
| `test_shutdown_decorator.py` | `@cc.on_shutdown` CRM lifecycle cleanup |

### rpc_v2 Transport

| File | Description |
|------|-------------|
| `test_wire_v2.py` | Wire v2 codec — call/reply control encoding, default-route cache |
| `test_scheduler.py` | Read/write concurrency scheduler for CRM method dispatch |
| `test_concurrency.py` | Read/write lock semantics for ICRM method scheduling |
| `test_client_pool.py` | `ClientPool` — ref-counted IPC client management, grace period |
| `test_icrm_proxy.py` | `ICRMProxy` — thread-local and IPC modes, method routing |
| `test_proxy_concurrency.py` | ICRMProxy concurrency under read/write access control |
| `test_chunk_assembler.py` | Chunk assembler — OOM validation, reassembly, boundary checks |
| `test_serve.py` | `cc.serve()` API — server start/stop lifecycle |
| `test_security_v2.py` | v2 handshake security, frame validation |
| `test_name_collision.py` | Multi-CRM name collision detection |
| `test_adaptive_buffer.py` | `AdaptiveBuffer` — grow/shrink, idle decay |
| `test_op2_safety.py` | Safety regressions — deferred free, TOCTOU, err_len bounds, scatter-write, double-free guard |

### IPC & Buddy Allocator

| File | Description |
|------|-------------|
| `test_buddy_pool.py` | Rust buddy allocator — alloc/free, pool stats, dedicated fallback, FFI, segment lifecycle, stale cleanup |
| `test_ipc.py` | IPC transport — buddy handshake, wire frames, inline/buddy paths, boundary checks, shutdown safety |
| `test_ipc_security.py` | IPC handshake security — SHM name validation, segment count DoS limits, malformed frame handling |

### Relay

| File | Description |
|------|-------------|
| `test_cli_relay.py` | CLI relay commands — start/stop, config |
| `test_http_client.py` | HTTP client transport — request/response, error handling |
| `test_native_relay.py` | `NativeRelay` (Rust/axum) — start/stop, upstream registration |
| `test_relay_graceful_shutdown.py` | Relay graceful shutdown — drain, timeout |

## Integration Tests (`tests/integration/`)

| File | Description |
|------|-------------|
| `test_rpc_v2_server.py` | ServerV2 end-to-end — multi-CRM hosting, handshake, method dispatch |
| `test_rpc_v2_basic.py` | IPC client backward compatibility with legacy IPC server |
| `test_registry.py` | `cc.register()` / `cc.connect()` / `cc.close()` SOTA API lifecycle |
| `test_multi_crm_server.py` | Multi-CRM routing — name-based dispatch, concurrent access |
| `test_icrm_proxy.py` | ICRMProxy integration — thread-local + IPC modes end-to-end |
| `test_chunked_transfer.py` | Large payload chunked transfer across transports |
| `test_backpressure.py` | Buddy pool OOM backpressure — L0/L1/L2 protection |
| `test_concurrency_safety.py` | Concurrent client safety under load |
| `test_error_propagation.py` | CRM-side exceptions propagate to client as typed `CCError` |
| `test_p0_fixes.py` | P0 regression tests — scheduler, proxy, lifecycle fixes |
| `test_serve.py` | `cc.serve()` integration — multi-protocol serving |
| `test_component_runtime.py` | Component `@cc.runtime.connect` — injection, type matching, address passing |
| `test_http_relay.py` | HTTP relay end-to-end — POST routing, error forwarding |

## Shared Fixtures (`tests/fixtures/`)

| File | Description |
|------|-------------|
| `ihello.py` | `IHello` ICRM interface + `HelloData` / `HelloItems` transferable types |
| `hello.py` | `Hello` CRM implementation (stateful greeting service) |
| `counter.py` | `ICounter` / `Counter` — minimal read/write CRM for concurrency tests |

## Protocol Address Fixtures (`tests/conftest.py`)

Parametrized `protocol_address` fixture provides unique addresses for each protocol:

| Protocol | Prefix | Transport |
|----------|--------|-----------|
| `thread` | `thread://` | In-process, zero-copy (skips serialization) |
| `memory` | `memory://` | Shared memory file-based, cross-process |
| `tcp` | `tcp://` | ZeroMQ TCP socket |
| `http` | `http://` | HTTP/REST via Starlette/uvicorn |
| `ipc-v2` | `ipc-v2://` | UDS control + Python SharedMemory pool (legacy) |
| `ipc` | `ipc://` | UDS control + Rust buddy allocator SHM (explicit) |
| `ipc` | `ipc://` | **Default IPC** — UDS control + Rust buddy allocator SHM |

---

## Benchmarks (`benchmarks/`)

Run benchmarks with `uv run python benchmarks/<script>.py`.

| File | Description |
|------|-------------|
| `memory_benchmark.py` | Memory transport latency/throughput across payload sizes |
| `ipc_detailed_bench.py` | IPC detailed latency breakdown (P50/P95/P99, throughput, ops) |
| `ipc_v2_vs_v3_benchmark.py` | Side-by-side comparison: IPC v2 vs v3 across 64B–1GB |
| `concurrency_benchmark.py` | Concurrent client load — throughput scaling, contention |
| `wire_preencoding_benchmark.py` | Wire protocol encoding micro-benchmark |
| `adaptive_buffer_benchmark.py` | AdaptiveBuffer grow/shrink performance (IPC v2 component) |

### Key Benchmark Notes

- Benchmarks use **realistic `@transferable` paths** (not echo-optimized)
- Default round count: 100 per size tier
- Payload sizes typically span: 64B, 1KB, 4KB, 64KB, 1MB, 10MB, 50MB, 100MB, 500MB, 1GB
- Metrics: P50 latency, throughput (GB/s), ops/sec, min/max latency

---

## Rust Tests (`src/c_two/buddy/_buddy_core/`)

The Rust buddy allocator has its own test suite:

```bash
# Run Rust tests (must use --no-default-features on macOS to avoid PyO3 linker errors)
cd src/c_two/buddy/_buddy_core && cargo test --no-default-features
```

Currently **36 Rust tests** covering:
- `allocator::tests` — alloc/free, buddy merge, fragmentation, double-free detection, invalid input rejection
- `pool::tests` — multi-segment pool, dedicated fallback, gc_buddy idle reclamation, config validation
- `spinlock::tests` — lock/unlock, RAII guard, concurrent correctness, PID-based crash recovery
- `bitmap::tests` — bit manipulation, CAS alloc/free
