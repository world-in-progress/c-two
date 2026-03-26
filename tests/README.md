# Tests & Benchmarks

## Running Tests

```bash
# Full test suite (616 tests, ~3 min)
uv run pytest -q

# Single test file
uv run pytest tests/unit/test_encoding.py -q

# Single test function
uv run pytest tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Run with verbose output
uv run pytest -v --timeout=30
```

All tests use a **30-second per-test timeout**. Tests are parametrized across 7 transport protocols: `thread`, `memory`, `tcp`, `http`, `ipc` (v3, default), `ipc-v2`, `ipc-v3`.

---

## Unit Tests (`tests/unit/`)

| File | Description |
|------|-------------|
| `test_icrm_decorator.py` | `@cc.icrm()` decorator, namespace/version validation, method registration |
| `test_server_config.py` | `ServerConfig` validation — ICRM-CRM method matching, missing method detection |
| `test_crm_template.py` | CRM template generation from ICRM interface classes |
| `test_encoding.py` | Wire protocol encoding/decoding — `Event`, `EventTag`, message serialization |
| `test_event.py` | `Event` / `EventQueue` lifecycle — round-trip serialization, queue mechanics |
| `test_error.py` | `CCError` hierarchy — serialization/deserialization across wire |
| `test_transferable.py` | `@transferable` decorator — serialize/deserialize, auto-dataclass, scatter-write tuples, memoryview-aware deserialize, bytes fast-path |
| `test_concurrency.py` | Read/write lock semantics for ICRM method scheduling |
| `test_adaptive_buffer.py` | `AdaptiveBuffer` — grow/shrink, idle decay (used by IPC v2 client) |
| `test_buddy_pool.py` | Rust buddy allocator (`c2_buddy`) — alloc/free, pool stats, dedicated fallback, FFI bindings, segment lifecycle, stale cleanup |
| `test_ipc_v3.py` | IPC v3 transport — buddy handshake, wire frames, inline/buddy paths, reuse path, boundary checks, shutdown safety |
| `test_ipc_security.py` | IPC handshake security — SHM name validation, segment count DoS limits, malformed frame handling |
| `test_op2_safety.py` | Safety regression tests from op2 audit — deferred free, TOCTOU, err_len bounds, scatter-write, double-free guard |

## Integration Tests (`tests/integration/`)

| File | Description |
|------|-------------|
| `test_server_client.py` | End-to-end server↔client across all protocols — call, relay, ping, shutdown |
| `test_grid_operations.py` | Grid CRM domain logic — subdivide, merge, attribute queries |
| `test_component_runtime.py` | Component `@cc.runtime.connect` — injection, type matching, address passing |
| `test_concurrency_scheduler.py` | Read/write concurrency scheduling under concurrent client load |
| `test_error_propagation.py` | CRM-side exceptions propagate to client as typed `CCError` |
| `test_ipc_v2.py` | IPC v2 specific tests — legacy protocol validation (retained for backwards compat) |
| `test_router.py` | HTTP Router — worker registration, relay, health endpoint |

## Shared Fixtures (`tests/fixtures/`)

| File | Description |
|------|-------------|
| `ihello.py` | `IHello` ICRM interface + `HelloData` / `HelloItems` transferable types |
| `hello.py` | `Hello` CRM implementation (stateful greeting service) |

## Protocol Address Fixtures (`tests/conftest.py`)

Parametrized `protocol_address` fixture provides unique addresses for each protocol:

| Protocol | Prefix | Transport |
|----------|--------|-----------|
| `thread` | `thread://` | In-process, zero-copy (skips serialization) |
| `memory` | `memory://` | Shared memory file-based, cross-process |
| `tcp` | `tcp://` | ZeroMQ TCP socket |
| `http` | `http://` | HTTP/REST via Starlette/uvicorn |
| `ipc-v2` | `ipc-v2://` | UDS control + Python SharedMemory pool (legacy) |
| `ipc-v3` | `ipc-v3://` | UDS control + Rust buddy allocator SHM (explicit) |
| `ipc` | `ipc://` | **Default IPC** — routes to v3 (recommended) |

---

## Benchmarks (`benchmarks/`)

Run benchmarks with `uv run python benchmarks/<script>.py`.

| File | Description |
|------|-------------|
| `memory_benchmark.py` | Memory transport latency/throughput across payload sizes |
| `ipc_v3_detailed_bench.py` | IPC v3 detailed latency breakdown (P50/P95/P99, throughput, ops) |
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

## Rust Tests (`rust/c2_buddy/`)

The Rust buddy allocator has its own test suite:

```bash
# Run Rust tests (must use --no-default-features on macOS to avoid PyO3 linker errors)
cd rust/c2_buddy && cargo test --no-default-features
```

Currently **36 Rust tests** covering:
- `allocator::tests` — alloc/free, buddy merge, fragmentation, double-free detection, invalid input rejection
- `pool::tests` — multi-segment pool, dedicated fallback, gc_buddy idle reclamation, config validation
- `spinlock::tests` — lock/unlock, RAII guard, concurrent correctness, PID-based crash recovery
- `bitmap::tests` — bit manipulation, CAS alloc/free
