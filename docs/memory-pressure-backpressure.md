# IPC Memory Pressure And Backpressure

This guide describes Python 0.7.0 / c3 0.3.0. See [release preparation](releases/0.7.0.md) for version availability, upgrades and validation progress, and [memory policy](memory-policy.md) for configuration and retained-owner accounting.

## Current Model

C-Two's active IPC stack lives in Rust:

- `core/foundation/c2-config` resolves defaults, environment inputs, and code-level overrides.
- `core/foundation/c2-mem` owns shared-memory pools and allocation policy.
- `core/protocol/c2-wire` handles chunking and reassembly for large payloads.
- `core/transport/c2-ipc` and `core/transport/c2-server` move requests over Unix UDS or Windows Named Pipes, with native shared payload memory.

Python does not implement an IPC config resolver and does not expose memory-pool defaults. Python SDK code can only provide typed code-level overrides through `cc.set_server(ipc_overrides=...)`, `cc.set_client(ipc_overrides=...)`, and the global `cc.set_transport_policy(shm_threshold=...)`.

## Backpressure Boundaries

The runtime protects memory use at three points:

| Boundary | Mechanism |
| --- | --- |
| Config resolution | Rust validates sizes, counts, finite durations, and derived values before runtime use. |
| Backing allocation | Rust atomically reserves finite SHM/file budgets before creating backing, alongside segment size/count and OS-pressure checks. |
| Chunk reassembly | Chunk registries cap segment counts, total chunks, stale assembly time and message size, and reserve full capacity against the shared live-reassembly budget. |

`max_pool_memory` is not a user setting. It is derived from:

```text
pool_segment_size * max_pool_segments
```

The three finite budgets are `shm_backing_budget_bytes`,
`file_backing_budget_bytes` and `live_reassembly_budget_bytes`; zero refuses
positive reservations. Backing and reassembly are separate accounting units,
not additive RSS measurements. Budget charges survive queueing, execution and
hold until the actual storage is released, including retained owners after
runtime shutdown.

Buddy reuse/expansion, dedicated SHM and checked chunked IPC remain the transport
fallback model, with file spill where receiving-side storage supports it.
Default buddy allocation is lazy; `pool_enabled=False` skips buddy while
keeping dedicated mappings and the other fallback paths. File spill is not a
guarantee for every resource or call path. Chunking completes a request before
resource invocation; it does not expose streaming business methods.

## Failure Semantics

Current high-level callers should expect transport failures to surface through the active error types, such as `ResourceUnavailable`, `RegistryUnavailable`, or underlying native IPC errors depending on the call path. The old Python `rpc_v2` documents and `MemoryPressureError` examples are obsolete and should not be used as implementation guidance.

When memory pressure is observed in tests or production:

1. Lower request concurrency or payload size.
2. Inspect the read-only `cc.memory_stats()` snapshot, then adjust the relevant backing budget or pool size/count through typed IPC overrides.
3. Increase reassembly limits only when the receiving side is expected to accept larger chunked payloads.
4. Keep `shm_threshold` as a process-wide transport policy, set before creating servers or clients.

The standalone relay resolves its own upstream IPC policy; Python client overrides do not propagate to it. See the [CLI guide](../cli/README.md) for budget flags, shared accounting and idle reconnect behavior. FastDB checked views must be invalidated before their lease is released; unsafe raw aliases cannot be mechanically revoked.

## Test Focus

Memory-pressure tests should exercise the current Rust-backed paths:

- config resolver rejection for invalid or non-finite values;
- pool allocation limits and release behavior;
- chunk assembly timeout and cleanup;
- large payload transfer across IPC and relay paths;
- failure propagation without process aborts or stale SHM leaks.
