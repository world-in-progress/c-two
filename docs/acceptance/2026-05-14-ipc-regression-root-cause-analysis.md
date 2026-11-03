# 2026-05-14 IPC Regression Root Cause Analysis

## Question

The 2026-05-14 full HTTP relay benchmark appeared to show IPC regression versus
the 2026-05-12 report, especially at 500MB and 1GB. This report investigates
whether that is a real IPC implementation regression or a benchmark protocol
artifact.

## Frozen Invariants

- The benchmark entry must be comparable before using it as regression
  evidence. Same script, same native extension rebuild discipline, same payload
  type, same IPC pool configuration, and same warmup/round policy are required.
- Direct IPC hot path health must be separated from Python SDK output
  deserialization and benchmark run-order effects.
- Large-payload rows must not be judged from a single full-matrix run, because
  500MB and 1GB calls are sensitive to memory page state and allocator history.

## Evidence

### Current Full `thread_vs_ipc_benchmark.py`

Command:

```bash
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_ENV_FILE= \
  uv run python sdk/python/benchmarks/thread_vs_ipc_benchmark.py
```

After rebuilding the native extension at current HEAD `61f6da1`, the full
script reported:

| Size | 05-12 IPC ms | Current IPC ms | Delta |
| ---: | ---: | ---: | ---: |
| 64B | 0.168 | 0.164 | -2.4% |
| 256B | 0.150 | 0.170 | +13.3% |
| 1KB | 0.148 | 0.169 | +14.2% |
| 4KB | 0.150 | 0.173 | +15.3% |
| 16KB | 0.161 | 0.183 | +13.7% |
| 64KB | 0.170 | 0.176 | +3.5% |
| 256KB | 0.182 | 0.190 | +4.4% |
| 1MB | 0.283 | 0.321 | +13.4% |
| 4MB | 0.659 | 0.681 | +3.3% |
| 10MB | 1.385 | 1.463 | +5.6% |
| 50MB | 4.903 | 5.346 | +9.0% |
| 100MB | 9.733 | 10.411 | +7.0% |
| 500MB | 47.474 | 87.593 | +84.5% |
| 1GB | 420.826 | 297.896 | -29.2% |

The 500MB row is the only strong apparent regression in this comparable
single-run table. The 1GB row does not reproduce as a stable regression.

### Native Rebuild Discipline

Commit switching was tested with explicit native rebuilds:

```bash
git switch --detach 190c3c3
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_ENV_FILE= \
  uv sync --reinstall-package c-two
```

A 500MB-only microbenchmark at `190c3c3` produced:

```text
commit=190c3c3 payload=500MB rounds=100 warmup=5 median_ms=86.802 min_ms=50.629 max_ms=134.176
```

This did not reproduce the 05-12 report's `47.474ms` 500MB number even on the
05-12 report commit. That weakens the claim that the 05-12 value is a stable
baseline for code regression comparison.

After switching back and rebuilding current HEAD:

```bash
git switch dev-feature
env UV_CACHE_DIR=.uv-cache C2_RELAY_ANCHOR_ADDRESS= C2_ENV_FILE= \
  uv sync --reinstall-package c-two
```

The same 500MB-only microbenchmark at `61f6da1` produced:

```text
commit=61f6da1 payload=500MB rounds=100 warmup=5 median_ms=49.050 min_ms=46.642 max_ms=120.333
```

This shows current IPC can still reproduce the 05-12-scale 500MB latency when
the large row is isolated.

### Full-Matrix Run-Order Effect

Immediately after the full `thread_vs_ipc_benchmark.py` run, the same isolated
500MB microbenchmark changed to:

```text
commit=61f6da1 after_full_script payload=500MB rounds=100 warmup=5 median_ms=88.207 min_ms=47.963 max_ms=161.993
```

The distribution showed a cold/hot split:

```text
median=87.422 min=48.992 p10=82.768 p25=85.313 p75=89.501 p90=96.055 max=159.463
first10=49.0,49.0,101.1,83.6,77.5,89.3,86.6,82.4,84.2,85.9
fast_lt60=5 slow_gt80=93
```

The slow state crosses Python process boundaries and is triggered by prior
large-payload benchmark activity. It is not explained by stale C-Two SHM
objects: `cleanup_stale_shm()` returned `0`.

### IPC/SHM Versus Client Output Copy

A raw route-bound client call that receives `ResponseBuffer`, takes a
`memoryview`, and releases it without client-side `Payload.deserialize()`
reported:

```text
raw_response_no_client_deserialize median=39.573 min=34.159 p10=36.104 p90=72.858 max=77.140
```

That isolates the IPC/SHM path plus server resource execution from the client
output-copy path. It is substantially faster than the full
`crm.echo(Payload)` path, so the direct IPC transport itself is not the primary
source of the apparent 500MB regression.

Manually adding only the client-side `bytes(memoryview(response))` copy:

```text
call_no_copy median=40.870 min=35.732 p90=72.167 max=100.003
client_bytes_copy median=12.612 min=10.990 p90=13.400 max=16.840
total median=53.715 min=48.031 p90=85.510 max=112.619
```

This places the stable transport floor around 40ms and a typical 500MB client
copy around 12-14ms on this machine.

### Server-Side Input Deserialize Split

Instrumenting `Payload.deserialize()` by thread showed:

```text
total median=52.963 min=46.712 max=95.022
MainThread count=50 median=12.452 min=10.974 p90=13.233 max=14.000
```

The `MainThread` records are client output deserialization and are stable. The
server-side `Dummy-*` records include repeated 46-49ms input copies from
request SHM to Python bytes. That points to server-side
`bytes(memoryview(request_buf))` sensitivity to memory page state / allocator
offset history rather than an IPC frame or scheduler regression.

### Warmup Sensitivity

The default benchmark warmup is 5. Raising only the 500MB warmup count improves
the median materially:

```text
warmup=20 median=51.462 min=46.193 p10=47.478 p90=84.943 max=122.843
slow_gt80=28 fast_lt60=72

warmup=40 median=52.407 min=45.416 p10=48.482 p90=86.289 max=122.397
slow_gt80=28 fast_lt60=71
```

The improvement from warmup 5 to warmup 20 is enough to move the current 500MB
median back near the 05-12 report's 47ms range. Warmup 40 does not improve much
further, suggesting the main issue is insufficient large-payload warmup plus
remaining memory-state variance.

## Diagnosis

Current evidence does not support a stable IPC implementation regression.

The apparent regression is primarily a benchmark protocol artifact:

- The 05-12 500MB value is not reproducible as a stable baseline even when
  checking out and rebuilding the 05-12 report commit.
- Current HEAD can reproduce a 05-12-scale isolated 500MB latency after a fresh
  native rebuild and isolated large-row run.
- Running the full matrix first pushes the system into a slower 500MB state that
  persists across Python processes.
- Raw `ResponseBuffer` measurements show the IPC/SHM path is materially faster
  than the full auto-transfer result path.
- The expensive and variable part is dominated by large Python
  `bytes(memoryview(...))` construction, especially server-side input
  deserialization from request SHM, not by HTTP relay changes or direct IPC
  control-plane changes.

## Implications

- The full three-mode report should not use the 05-12 single-run 500MB/1GB rows
  as proof of direct IPC code regression.
- Large-payload benchmark rows need size-aware warmup and distribution reporting
  (`min`, `p10`, `p50`, `p90`, `max`), not only a single P50.
- IPC transport and Python auto-transfer should be benchmarked separately:
  `ResponseBuffer` no-copy/view path, Python output-copy path, and full
  CRM-transfer path answer different performance questions.
- If product-level latency matters for 500MB+ Python `Payload` objects, the real
  optimization target is avoiding Python `bytes` reconstruction by using hold /
  from-buffer style transferables or explicit view-mode benchmark variants.

## Recommended Next Changes

1. Update large-payload benchmark scripts to use size-aware warmup, for example
   keep 5 warmups for small rows but use at least 20 warmups for 500MB and 1GB
   rows when measuring steady-state P50.
2. Add an IPC-only benchmark variant that reports:
   `raw_response_no_client_deserialize`, `client_bytes_copy`, and
   `full_auto_transfer`.
3. Stop comparing full relay matrix IPC rows against older single-run report
   rows unless both runs use the same warmup policy and distribution output.
4. Keep the full HTTP relay benchmark for relay functionality and relay
   transport-cost tracking, but do not use it as the primary direct-IPC
   regression detector.

