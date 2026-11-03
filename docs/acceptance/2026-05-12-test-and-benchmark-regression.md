# Test And Benchmark Regression Report - 2026-05-12

## Scope

This run validates the current worktree after the thin SDK / Rust core boundary
documentation update and benchmark-script compatibility fixes. Benchmarks were
run on the local machine with Python 3.14.3 free-threading and NumPy 2.4.4
unless noted otherwise.

After this report was first created, the root `pyproject.toml` gained a
`benchmark` dependency group with NumPy and Ray. Ray is guarded with
`python_version < '3.14'` because `uv sync --group benchmark` against the active
CPython 3.14.3t environment reported that `ray==2.55.1` has no source
distribution or wheel for the current platform. With that marker in place,
`env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv sync --group benchmark`
succeeds on 3.14.3t and installs the non-Ray benchmark dependencies.

## Test Results

| Command | Result |
| --- | --- |
| `cargo test --manifest-path core/Cargo.toml --workspace` | Passed. All workspace unit tests and doctests exited 0. |
| `env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30 -rs` | `820 passed, 2 skipped in 52.76s` |
| `env C2_RELAY_ANCHOR_ADDRESS= .venv-py312/bin/python -m pytest sdk/python/tests/unit/test_benchmark_relay_helpers.py -q --timeout=30 -rs` | `9 passed in 0.15s` |
| `git diff --check` | Passed. No whitespace errors. |

Skipped Python tests were the two grid example integration cases requiring
example dependencies.

## Benchmark Harness Fixes Required Before Measurement

Two benchmark scripts had fallen behind current runtime guardrails:

- `relay_qps_benchmark.py` and `three_mode_benchmark.py` posted old relay
  `/_register` bodies with only `name`, `server_id`, and `address`. Current
  relay registration requires a server instance identity and CRM contract
  attestation fields.
- `relay_qps_benchmark.py` used direct `hey` HTTP calls without the required
  expected-contract headers.
- `three_mode_benchmark.py` and `segment_size_benchmark.py` used bare `dict`
  CRM annotations, which current descriptor guardrails correctly reject as an
  unstable RPC ABI.

The benchmark scripts were updated to pass `server_instance_id`, CRM contract
fields, expected-contract HTTP headers, and parameterized dict annotations.
`sdk/python/tests/unit/test_benchmark_relay_helpers.py` now covers these
benchmark guardrails.

## Benchmark Results

### Hold vs View

Command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/hold_vs_view_benchmark.py --max-mb 100
```

| Size | Rows | Rounds | Copy P50 ms | View P50 ms | Speedup |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 64B | 1 | 200 | 0.0033 | 0.0014 | 2.4x |
| 256B | 5 | 200 | 0.0032 | 0.0013 | 2.5x |
| 1KB | 21 | 200 | 0.0035 | 0.0013 | 2.7x |
| 4KB | 87 | 200 | 0.0049 | 0.0013 | 3.7x |
| 64KB | 1,394 | 200 | 0.0320 | 0.0020 | 16.3x |
| 1MB | 22,310 | 200 | 0.4709 | 0.0174 | 27.1x |
| 10MB | 223,101 | 30 | 4.80 | 0.2106 | 22.8x |
| 50MB | 1,115,506 | 30 | 23.84 | 1.08 | 22.1x |
| 100MB | 2,231,012 | 30 | 50.63 | 2.33 | 21.7x |

Analysis: hold mode keeps the buffer alive and avoids the `np.frombuffer(...).copy()`
cost. The advantage becomes material at 64KB and stabilizes around 22x for
10-100MB payloads. This is consistent with the intended zero-copy value
proposition.

### Chunked Transfer

Command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/chunked_benchmark.py --max-mb 512
```

This benchmark had to be run outside the sandbox because default sandboxing
denied POSIX `shm_open`.

| Payload | Mode | Min ms | Median ms | Max ms | Throughput |
| ---: | --- | ---: | ---: | ---: | ---: |
| 4KB | normal | 0.31 | 0.32 | 0.40 | 24.4 MB/s |
| 64KB | normal | 0.34 | 0.36 | 0.38 | 350.5 MB/s |
| 256KB | normal | 0.36 | 0.36 | 0.40 | 1371.7 MB/s |
| 1MB | normal | 0.55 | 0.55 | 0.62 | 3618.3 MB/s |
| 4MB | normal | 1.29 | 1.40 | 1.74 | 5716.3 MB/s |
| 16MB | normal | 4.30 | 4.71 | 5.73 | 6787.3 MB/s |
| 64MB | normal | 15.10 | 20.01 | 24.39 | 6398.0 MB/s |
| 230MB | normal | 60.41 | 78.55 | 117.95 | 5866.2 MB/s |
| 230MB | chunked | 52.30 | 68.85 | 95.54 | 6692.5 MB/s |
| 256MB | chunked | 171.52 | 173.39 | 277.01 | 2952.9 MB/s |
| 512MB | chunked | 401.96 | 436.02 | 576.35 | 2348.5 MB/s |

Analysis: both normal and chunked paths completed integrity-checked echo
round trips. The 230MB boundary case shows chunked path is not inherently
broken; larger 256MB/512MB payloads show expected lower throughput due to
multi-chunk assembly and memory pressure.

### Thread-Local vs Direct IPC

Command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/thread_vs_ipc_benchmark.py
```

| Size | Thread P50 ms | IPC P50 ms | IPC/Thread |
| ---: | ---: | ---: | ---: |
| 64B | 0.001 | 0.168 | 121.8x |
| 256B | 0.001 | 0.150 | 109.3x |
| 1KB | 0.001 | 0.148 | 110.8x |
| 4KB | 0.001 | 0.150 | 109.0x |
| 16KB | 0.001 | 0.161 | 120.9x |
| 64KB | 0.001 | 0.170 | 127.4x |
| 256KB | 0.001 | 0.182 | 136.5x |
| 1MB | 0.001 | 0.283 | 205.7x |
| 4MB | 0.001 | 0.659 | 465.1x |
| 10MB | 0.001 | 1.385 | 1038.9x |
| 50MB | 0.001 | 4.903 | 3675.6x |
| 100MB | 0.001 | 9.733 | 7301.9x |
| 500MB | 0.001 | 47.474 | 35614.3x |
| 1GB | 0.001 | 420.826 | 288632.1x |
| GeoMean | 0.001 | 0.973 | 715.8x |

Analysis: same-process dispatch remains effectively zero-serialization and
orders of magnitude faster than cross-process IPC, as expected. Direct IPC
scales smoothly through 500MB. The 1GB measurement is much higher than the
500MB linear trend and should be treated as memory-pressure sensitive rather
than a precise steady-state throughput number.

### Relay QPS

Command: `relay_qps_benchmark.py` with a locally started `c3 relay` and `hey`.

| Metric | Value |
| --- | ---: |
| Requests | 3000 |
| Concurrency | 32 |
| QPS | 2942.0 |
| Fastest | 0.0005 s |
| Average | 0.0104 s |
| P50 | 0.0077 s |
| P99 | 0.0437 s |
| Slowest | 0.0880 s |

Analysis: after the benchmark was updated to include current contract headers,
the relay path returned 2xx responses and produced a stable small-payload QPS
measurement. This validates the HTTP relay path under the current route
contract guardrails.

### Three-Mode Benchmark

Command: `three_mode_benchmark.py` with a locally started `c3 relay`.

| Size | Rounds | Thread ms | IPC bytes ms | IPC dict ms | Dict/Bytes | Relay ms | IPC/Thread |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 64B | 100 | 0.0014 | 0.2846 | 0.3149 | 1.1x | 0.5648 | 201.0x |
| 256B | 100 | 0.0013 | 0.3102 | 0.3006 | 1.0x | 0.5941 | 232.7x |
| 1KB | 100 | 0.0014 | 0.3140 | 0.3034 | 1.0x | 0.6077 | 221.6x |
| 4KB | 100 | 0.0014 | 0.3279 | 0.3138 | 1.0x | 0.5992 | 238.5x |
| 64KB | 100 | 0.0013 | 0.2996 | 0.3214 | 1.1x | 0.7778 | 224.8x |
| 1MB | 100 | 0.0013 | 0.5010 | 0.5049 | 1.0x | 3.722 | 375.9x |
| 10MB | 20 | 0.0013 | 2.251 | 2.228 | 1.0x | 32.801 | 1688.7x |
| 50MB | 20 | 0.0014 | 7.354 | 7.244 | 1.0x | 152.8 | 5348.7x |
| 100MB | 20 | 0.0015 | 14.622 | 14.362 | 1.0x | 322.4 | 10028.5x |
| 500MB | 5 | 0.0015 | 81.462 | 71.952 | 0.9x | N/A | 55872.1x |
| 1GB | 5 | 0.0015 | 556.8 | 474.1 | 0.9x | N/A | 371182.6x |
| GeoMean | | 0.0014 | 2.397 | 2.341 | | 4.362 | |

Analysis: direct IPC and relay both run under current CRM contract guardrails.
Relay is not measured above 100MB by this script. Dict and bytes are very close
because this benchmark's dict payload is dominated by one large bytes field;
it should not be interpreted as a general Python object serialization result.

### Segment Size Benchmark

Command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
```

| Size | 256M bytes ms | 2G bytes ms | Speedup | 256M dict ms | 2G dict ms | Speedup | 256M path |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 64B | 0.290 | 0.278 | 1.04x | 0.297 | 0.255 | 1.17x | buddy |
| 256B | 0.314 | 0.260 | 1.21x | 0.303 | 0.264 | 1.15x | buddy |
| 1KB | 0.304 | 0.253 | 1.20x | 0.298 | 0.253 | 1.18x | buddy |
| 4KB | 0.305 | 0.274 | 1.11x | 0.309 | 0.273 | 1.13x | buddy |
| 64KB | 0.306 | 0.263 | 1.17x | 0.318 | 0.269 | 1.18x | buddy |
| 1MB | 0.498 | 0.460 | 1.08x | 0.507 | 0.465 | 1.09x | buddy |
| 10MB | 2.276 | 2.234 | 1.02x | 2.203 | 2.183 | 1.01x | buddy |
| 50MB | 7.287 | 7.175 | 1.02x | 7.618 | 7.389 | 1.03x | buddy |
| 100MB | 14.796 | 14.551 | 1.02x | 14.464 | 14.251 | 1.01x | buddy |
| 500MB | 231.994 | 71.994 | 3.22x | 216.190 | 70.767 | 3.05x | DEDICATED |
| 1GB | 876.851 | 568.547 | 1.54x | 887.943 | 476.016 | 1.87x | DEDICATED |

Analysis: 2GB segments have small advantages for small/medium payloads and a
large advantage once 256MB segments force dedicated allocation. This supports
the existing benchmark premise that large scientific payloads benefit from a
pool configuration sized to the workload.

### Kostya-Style C-Two Coordinate Benchmark

Commands:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/kostya_ctwo_benchmark.py --variant pickle-records --n 100000 --iters 10 --warmup 2
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/kostya_ctwo_benchmark.py --variant pickle-arrays --n 100000 --iters 10 --warmup 2
```

| Variant | N | Iters | P10 ms | P50 ms | P90 ms | Mean ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| pickle-records | 100,000 | 10 | 63.54 | 65.01 | 73.48 | 66.63 |
| pickle-arrays | 100,000 | 10 | 9.52 | 9.78 | 10.39 | 9.84 |

Analysis: array-oriented payload layout is about 6.6x faster than record-list
pickle for the same coordinate count. This is expected and supports the
project's scientific workload direction: columnar representation dominates.

### Unified NumPy IPC Benchmark - Python 3.12 Ray Rerun

Setup command:

```bash
env UV_CACHE_DIR=.uv-cache UV_PROJECT_ENVIRONMENT=.venv-py312 uv sync --python 3.12 --group benchmark
```

The 3.12 benchmark environment installed `ray==2.55.1`, `numpy==2.4.4`, and
the local `c-two==0.4.10` editable package. Running the benchmark through
`uv run --python 3.12 --group benchmark` caused Ray workers to inherit a
temporary runtime environment and repeatedly rebuild dependencies under
`/private/tmp/ray/...`; workers did not register before Ray's timeout. The
completed measurement therefore used the already-synced 3.12 virtualenv
directly:

```bash
env C2_RELAY_ANCHOR_ADDRESS= .venv-py312/bin/python sdk/python/benchmarks/unified_numpy_benchmark.py
```

| N | Rounds | C2 pickle ms | C2 hold ms | Ray ms | Ray / C2 hold |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1K | 100 | 0.1555 | 0.0914 | 6.8839 | 75.3x |
| 10K | 100 | 0.9233 | 0.1166 | 7.2432 | 62.1x |
| 100K | 30 | 8.1963 | 0.3985 | 9.6708 | 24.3x |
| 1M | 15 | 140.1915 | 3.7482 | 45.7995 | 12.2x |
| 3M | 5 | 577.6699 | 11.0482 | 151.1746 | 13.7x |

Analysis: the previously unrun Ray comparison now has local evidence on
Python 3.12. C-Two hold mode is faster than Ray for these same-host NumPy
round trips in this benchmark, with the largest ratio on smaller arrays where
Ray's per-call overhead dominates. The ratio narrows on larger arrays but
remains above 12x in this run. C-Two pickle mode becomes expensive for
multi-million element arrays, which reinforces the hold/zero-copy path as the
relevant performance contract for scientific array payloads.

## Not Run

`sdk/python/benchmarks/unified_numpy_benchmark.py` still does not run in the
active 3.14.3t environment. The benchmark group is present, and
`uv run --group benchmark python -VV` confirms the active interpreter is:

```text
Python 3.14.3 free-threading build (main, Feb  3 2026, 15:32:20) [Clang 17.0.0 (clang-1700.6.3.2)]
```

However, Ray is not importable there because the package has no installable
artifact for this platform/Python ABI. The rerun command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv run --group benchmark python sdk/python/benchmarks/unified_numpy_benchmark.py
```

still exits at module import with:

```text
ModuleNotFoundError: No module named 'ray'
```

The Python 3.12 Ray benchmark has now been run in `.venv-py312`; the remaining
unrun scope is only the 3.14.3t Ray comparison, which is blocked by package
availability for the free-threading interpreter ABI rather than by C-Two code.

## Regression Assessment

No functional regression was found in the refreshed full test suites. Benchmark
execution did reveal stale benchmark harness code, not a runtime regression:
the harness had not been updated for current relay and CRM contract guardrails.
After fixing the benchmark harnesses, direct IPC, chunked transfer, hold/view,
relay QPS, three-mode transport, segment-size, and C-Two Kostya benchmark paths
all completed successfully.

The benchmark numbers should be treated as local-run evidence, not a
statistically controlled performance signoff: there is no pinned historical
baseline in this run and no CPU isolation. Ray comparison is available for
Python 3.12 only; it remains unavailable for the active 3.14.3t free-threading
environment because Ray has no installable artifact for that ABI. The most
meaningful regression signal here is that the current runtime paths complete
successfully under the same guardrails enforced by the test suite.
