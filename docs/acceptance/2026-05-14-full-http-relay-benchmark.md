# 2026-05-14 Full HTTP Relay Large Payload Benchmark

## Scope

This report records a local full three-mode benchmark run after the relay large
payload request-path remediation. The run includes HTTP relay rows for every
declared payload size, including 500MB and 1GB.

The first draft of this report used one full benchmark matrix. That was not a
strong enough benchmark protocol for regression discussion. This version uses
three independent full-matrix runs and reports the median, plus min/max ranges,
for each payload size.

The relay path label is:

```text
relay_http_request_stream_to_shm_response_materialized
```

That label is intentional. The client-side HTTP request source is still
materialized, the relay-to-upstream IPC request path is streamed into upstream
IPC/SHM, and the HTTP response path is still materialized before being batched
into the response body.

## Environment

- Machine: Apple M1 Max, 34GB RAM
- OS: Darwin 25.4.0 arm64
- Python: 3.14.3 free-threading build
- c-two: 0.4.10 from current checkout
- NumPy: 2.4.4
- Relay: local `c3 relay`, rebuilt and relinked from current checkout
- IPC pool: 2GB segment size, 8 max segments
- Remote payload chunk size: `C2_REMOTE_PAYLOAD_CHUNK_SIZE=1048576`
- Relay HTTP timeout: `C2_RELAY_CALL_TIMEOUT=0`
- Benchmark protocol: 3 independent full-matrix runs; each script run uses
  internal warmup `3` plus adaptive per-size rounds `100/20/5`.

## Commands

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= uv sync --group benchmark
python tools/dev/c3_tool.py --build --link
env C2_RELAY_ANCHOR_ADDRESS= C2_ENV_FILE= c3 relay --bind 127.0.0.1:18080
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_RELAY_CALL_TIMEOUT=0 C2_REMOTE_PAYLOAD_CHUNK_SIZE=1048576 \
  uv run python -c "import sys; sys.path.insert(0, 'sdk/python/benchmarks'); import three_mode_benchmark as b; b._relay_port = 18080; b.main()" \
  --output sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run1.tsv
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_RELAY_CALL_TIMEOUT=0 C2_REMOTE_PAYLOAD_CHUNK_SIZE=1048576 \
  uv run python -c "import sys; sys.path.insert(0, 'sdk/python/benchmarks'); import three_mode_benchmark as b; b._relay_port = 18080; b.main()" \
  --output sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run2.tsv
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_RELAY_CALL_TIMEOUT=0 C2_REMOTE_PAYLOAD_CHUNK_SIZE=1048576 \
  uv run python -c "import sys; sys.path.insert(0, 'sdk/python/benchmarks'); import three_mode_benchmark as b; b._relay_port = 18080; b.main()" \
  --output sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run3.tsv
```

The raw TSV files and aggregate TSV were written to:

```text
sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run1.tsv
sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run2.tsv
sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_run3.tsv
sdk/python/benchmarks/results/benchmark_2gb_full_http_relay_2026-05-14_3run_median.tsv
```

`sdk/python/benchmarks/results/` is git-ignored except for `.gitkeep`, so the
table below is the tracked benchmark record.

## Results

| Size | Rounds | IPC bytes median ms | IPC bytes range | IPC delta vs 2026-05-12 | Relay median ms | Relay range | Relay vs IPC | Relay path |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 64B | 100 | 0.2968 | 0.2922-0.3071 | +4.3% | 0.5658 | 0.5606-0.5684 | 1.91x | request_stream_to_shm_response_materialized |
| 256B | 100 | 0.3226 | 0.3065-0.3326 | +4.0% | 0.5984 | 0.5734-0.6028 | 1.85x | request_stream_to_shm_response_materialized |
| 1KB | 100 | 0.3121 | 0.3083-0.3121 | -0.6% | 0.5641 | 0.5611-0.5991 | 1.81x | request_stream_to_shm_response_materialized |
| 4KB | 100 | 0.3296 | 0.3231-0.3524 | +0.5% | 0.5998 | 0.5983-0.6081 | 1.82x | request_stream_to_shm_response_materialized |
| 64KB | 100 | 0.3443 | 0.3346-0.3761 | +14.9% | 0.6798 | 0.6658-0.6991 | 1.97x | request_stream_to_shm_response_materialized |
| 1MB | 100 | 0.5203 | 0.5148-0.5460 | +3.9% | 1.4961 | 1.4614-1.5886 | 2.88x | request_stream_to_shm_response_materialized |
| 10MB | 20 | 2.2696 | 2.2371-2.2738 | +0.8% | 9.6694 | 9.6259-9.8584 | 4.26x | request_stream_to_shm_response_materialized |
| 50MB | 20 | 7.8811 | 7.4369-8.5384 | +7.2% | 35.0053 | 34.9033-35.5530 | 4.44x | request_stream_to_shm_response_materialized |
| 100MB | 20 | 14.5168 | 14.5113-14.9106 | -0.7% | 69.5591 | 68.8972-69.5910 | 4.79x | request_stream_to_shm_response_materialized |
| 500MB | 5 | 114.9148 | 102.0005-209.7837 | +41.1% | 987.9362 | 905.8466-1022.4985 | 8.60x | request_stream_to_shm_response_materialized |
| 1GB | 5 | 1038.1022 | 964.2578-1090.6924 | +86.4% | 4689.4054 | 4621.9016-4827.7477 | 4.52x | request_stream_to_shm_response_materialized |
| GeoMean | | 2.6958 | | +12.5% | 8.2900 | | 3.07x | request_stream_to_shm_response_materialized |

## Observations

- The full HTTP relay path completes at 500MB and 1GB instead of reporting
  `N/A` for those rows.
- The benchmark confirms relay large-payload execution under the current label:
  request-side relay-to-IPC streaming is active, while the HTTP response path is
  still materialized.
- Small and medium IPC bytes rows are mostly within low single-digit variance
  compared with the 2026-05-12 report. The 64KB row is slower by 14.9%, which is
  worth watching but not enough alone to prove a mechanism-level regression.
- Large IPC bytes rows show a real regression signal versus the 2026-05-12
  report: 500MB is +41.1% at the 3-run median, and 1GB is +86.4%. The 500MB run
  also has high spread, with one outlier at 209.7837 ms.
- Because this benchmark runs thread-local, IPC, dict IPC, and full HTTP relay
  sequentially in the same process, it is useful evidence but not final root
  cause. The large IPC signal should be followed by an IPC-only benchmark or a
  repeated `segment_size_benchmark.py` run to separate IPC transport regression
  from full-matrix memory-pressure and run-order effects.
- These are local workstation numbers, not a cross-machine production latency
  guarantee.

## Buddy Segment Diagnostic

After the initial three-run full matrix, we also ran three independent
`segment_size_benchmark.py` passes with:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= \
  C2_IPC_POOL_SEGMENT_SIZE=2147483648 C2_ENV_FILE= \
  uv run python sdk/python/benchmarks/segment_size_benchmark.py
```

The env resolver was verified separately:

```text
C2_IPC_POOL_SEGMENT_SIZE=2147483648 -> pool_segment_size 2147483648
```

Important caveat: `three_mode_benchmark.py` already uses explicit
`ipc_overrides={'pool_segment_size': 2147483648, 'max_pool_segments': 8}` for
IPC and relay rows. Because explicit overrides outrank environment values, the
full-matrix numbers above were already using a 2GiB buddy segment. The segment
diagnostic below compares 256MiB and 2GiB directly to isolate allocator-path
effects.

| Size | 256MiB bytes median ms | 2GiB bytes median ms | bytes speedup | 256MiB dict median ms | 2GiB dict median ms | dict speedup |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 64B | 0.3206 | 0.2873 | 1.12x | 0.3106 | 0.2786 | 1.11x |
| 256B | 0.3310 | 0.2907 | 1.14x | 0.3272 | 0.3215 | 1.02x |
| 1KB | 0.3197 | 0.2985 | 1.07x | 0.3395 | 0.2943 | 1.15x |
| 4KB | 0.3250 | 0.3085 | 1.05x | 0.3461 | 0.2984 | 1.16x |
| 64KB | 0.3396 | 0.3293 | 1.03x | 0.3466 | 0.3137 | 1.10x |
| 1MB | 0.5304 | 0.5074 | 1.05x | 0.5434 | 0.5221 | 1.04x |
| 10MB | 2.3130 | 2.4387 | 0.95x | 2.2997 | 2.4144 | 0.95x |
| 50MB | 8.3890 | 8.2306 | 1.02x | 8.0377 | 8.1708 | 0.98x |
| 100MB | 15.8420 | 16.3263 | 0.97x | 15.3989 | 15.6917 | 0.98x |
| 500MB | 304.7193 | 77.8531 | 3.91x | 255.2023 | 76.0820 | 3.35x |
| 1GB | 1757.2782 | 1198.5178 | 1.47x | 1883.0012 | 1195.8573 | 1.57x |

Large-payload ranges across the three diagnostic runs:

| Size | 256MiB bytes range | 2GiB bytes range | 256MiB dict range | 2GiB dict range |
| ---: | ---: | ---: | ---: | ---: |
| 500MB | 280.3222-308.4548 | 73.8169-77.9546 | 238.7864-298.9886 | 74.3880-78.0333 |
| 1GB | 1713.4700-2586.0780 | 1128.4810-1332.9597 | 1720.7347-2714.4396 | 1121.0331-1198.3945 |

Diagnostic conclusion:

- Raising the buddy segment from 256MiB to 2GiB materially improves payloads
  that exceed the 256MiB segment boundary. The 500MB row is the clearest case:
  2GiB segment avoids the heavy 256MiB dedicated path and improves median bytes
  latency by 3.91x.
- For payloads at or below 100MB, 2GiB segment is mostly noise-level or slightly
  slower around 10MB/100MB, so it should not be treated as a universal latency
  improvement.
- Because the full three-mode matrix was already run with 2GiB explicit
  overrides, the remaining 500MB/1GB gap versus the 2026-05-12 report is not
  explained by an unconfigured buddy segment. The next isolation step should
  compare this same 2GiB segment benchmark against the exact 2026-05-12 commit
  or rerun an IPC-only benchmark on both revisions with identical env and
  interpreter.
