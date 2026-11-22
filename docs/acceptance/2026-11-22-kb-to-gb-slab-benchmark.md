# KB-to-GB Unified Slab & FastDB 0.3.0 Benchmark vs Ray 2.56 (Planned 0.12.0)

**Date:** 2026-11-22  
**Scope:** Reproducible headless evaluation of C-Two's Workstream 2 unified `WritableSlab` direct-SHM channel, FastDB `0.3.0` (`ABI-118`) zero-copy `ExternalBytes` / `memoryview()` / `BuildPlan` integration, buddy geometry & warm local buffer recycling (including unary IPC response recycling), and KB-scale inline framing trimming under [ADR 0001](../decisions/0001-single-owner-release.md) single-owner copy-then-release semantics. Covers `1 KiB–1 GiB` single-frame payloads, `4 GiB` bounded-memory streamed migration inside a strict `1 GiB` role RAM budget, multi-subscriber fan-out (`@cc.stream` and `@cc.topic`, `R = 1, 2, 4`), and 1M-row structured coordinates (Kostya), paired on the identical Python 3.13.3 interpreter against `Ray 2.56.0` (`detached named actor`, `Ray Compiled Graphs`, and Ray actor streaming generator).

- **Driver:** [portable_payload_benchmark.py](../../sdk/python/benchmarks/portable_payload_benchmark.py) (public APIs only, guarded by [test_portable_payload_benchmark.py](../../sdk/python/tests/integration/test_portable_payload_benchmark.py))
- **Frozen JSON evidence:** [2026-11-22-kb-to-gb-slab-benchmark.json](evidence/2026-11-22-kb-to-gb-slab-benchmark.json)
- **Prior baselines:** [2026-11-20-portable-payload-benchmark.md](2026-11-20-portable-payload-benchmark.md) (`fastdb4py 0.2.4`, `fb1cc978`), [2026-11-21-pubsub-fanout-benchmark.md](2026-11-21-pubsub-fanout-benchmark.md) (`fastdb4py 0.2.4`, `a84211d0`)

## Environment & Configuration

| Item | Value |
| --- | --- |
| OS / Arch / CPUs | Darwin 27.2.0, `arm64`, 10 logical CPUs, headless |
| Python | CPython 3.13.3 (`python_gil_enabled = true`) — identical interpreter for C-Two and Ray |
| Rust / C-Two / CLI / FastDB / NumPy | Rust 1.99.1, `c-two` source candidate (release native extension), `c3` source candidate (release static build), `fastdb4py 0.3.0` (`ABI-118`), `numpy 2.4.4` |
| Ray | `ray 2.56.0` (`object_store_memory = 1.5 GiB`, `automatic_object_spilling_enabled = false`, default inline threshold `100 KiB`; isolated Ray session restarted for `>= 256 MiB` arms and before `migration` so failed Compiled Graph channels cannot starve subsequent Ray arms) |
| C-Two role memory budget | Strict `1 GiB` per role (`client`, `server`, `c3 relay`) for all `<= 512 MiB` frames and `4 GiB` streamed migration; `1,280 MiB` (`1 GiB + 256 MiB`) only for `1 GiB` monolithic single-frame calls whose encoded image exceeds `1 GiB` |
| Source commit (exact benchmark run) | `562293c4` |

## Key Findings (Summary)

1. **One-way SHM copy lower bound cut from `8 / 7` copies to `2` copies (ADR 0001 theoretical floor):**
   - Eliminated all `5–6` Python/`fastdb4py` glue copies across unary, server streams, pub/sub topics, and client requests: fresh `BuildPlan` (over read-only frozen NumPy arrays) executes directly into C-Two's `WritableSlab` SHM (`0` staging), published `Payload` copies once via `Payload.memoryview()` into SHM, receivers copy once from peer SHM into warm-recycled `RecycledBytes` before `RELEASE`, and `Payload.open_external(ExternalBytes(response))` + `acc.memoryview()` decode and reduce with **0 copies**.
   - Single-process `open_external(memoryview)` at `32 MiB` is **0.0200 ms** (**175× faster** than `0.2.4` `open_copy` `3.49 ms`, **103× faster** than `0.3.0` `open_copy` `2.07 ms`), and `build_external_plan` over a read-only `32 MiB` ndarray takes **0.0527 ms** (`0` snapshot copy).
2. **KB-scale bidirectional coupling (`1 KiB` & `16 KiB`):**
   - At `16 KiB`, portable `ctwo-fastdb-unary` hits **0.503 ms P50 / 0.771 ms P95** (`1,989 req/s`), beating `ray-named-actor` (`0.688 / 1.461 ms`, **1.37× faster P50 / 1.89× tighter P95**), and Python-only `ctwo-pickle-arrays` hits **0.202 ms P50 / 0.242 ms P95** (`4,958 req/s`), beating even static `ray-compiled-graph` (`0.227 / 0.625 ms`, **2.58× tighter P95**).
   - At `1 KiB`, `ctwo-fastdb-unary` (`0.526 ms P50 / 0.725 ms P95 / 0.750 ms P99`) delivers tighter tail latency than `ray-named-actor` (`0.478 / 0.786 / 1.011 ms`), and `ctwo-pickle-arrays` (`0.179 ms P50 / 0.880 ms P99`) avoids `ray-compiled-graph`'s `4.115 ms` P99 spike (`0.170 ms P50`).
3. **Mid-to-large frames (`4 MiB` & `32 MiB`) — C-Two beats both Ray Named Actor and Ray Compiled Graphs:**
   - **4 MiB `published`:** `ctwo-fastdb-stream` hits **0.414 ms P50 / 0.586 ms P95 (`9.43 GiB/s`)** — **9.05× faster than `ray-named-actor`** (`3.746 ms`), and **3.02× faster P50 / 20.5× tighter P95** than `ray-compiled-graph` (`1.250 / 12.037 ms`).
   - **32 MiB `published`:** `ctwo-fastdb-stream` hits **2.497 ms P50 (`12.52 GiB/s`)** and `ctwo-fastdb-unary` hits **3.122 ms P50 / 3.427 ms P95 (`10.01 GiB/s`)** — **5.24× faster than `ray-named-actor`** (`13.09 ms`) and **5.59× faster than `ray-compiled-graph`** (`13.96 ms`). Even 1-hop HTTP `c3 relay` stream (`11.10 ms`) beats local `ray-named-actor` (`13.09 ms`).
   - **32 MiB `fresh`:** `ctwo-fastdb-stream` hits **3.762 ms P50 / 4.697 ms P95 (`8.31 GiB/s`)** and `ctwo-fastdb-unary` hits **4.099 ms P50 / 4.696 ms P95 (`7.62 GiB/s`, `152.8 MiB` server RSS)** — beating both `ray-named-actor` (`6.031 / 7.746 ms`) and `ray-compiled-graph` (`3.963 / 5.079 ms`), while server peak RSS drops **4.1×–5.0× vs `0.11.0`** (`759.1 MiB → 185.2 MiB` stream / `152.8 MiB` unary).
4. **Monolithic `256 MiB / 512 MiB / 1 GiB` single frames vs `4 GiB` bounded-memory streamed migration:**
   - At **256 MiB**, all three C-Two FastDB arms beat `ray-named-actor`: `ctwo-fastdb-stream` (`62.17 ms` `published` / `64.57 ms` `fresh`) and `ctwo-fastdb-unary` (`62.62 / 63.90 ms` `published`, `64.69 / 67.44 ms` `fresh`) outperform `ray-named-actor` (`76.16 ms` `published` / `92.57 ms` `fresh`), while `ray-compiled-graph` fails with `OutOfDiskError` on all `>= 256 MiB` frames.
   - At **512 MiB–1 GiB** single monolithic frames, payloads exceed C-Two's `64 MiB` buddy segment ceiling and take `Dedicated` SHM (per-call `shm_open`/`ftruncate`/`mmap`/page-fault/`munmap` on both peers), sustaining **3.6–4.25 GiB/s** (`277.5 ms` `1 GiB published` unary, `254.6 ms` `1 GiB fresh` unary — **4.6×–5.5× faster than `ctwo-pickle-arrays`**), whereas `ray-named-actor` achieves `14.2–22.7 GiB/s` via persistent zero-copy Plasma mappings (while `ray-compiled-graph` fails `OutOfDiskError`).
   - For multi-hundred-MiB to multi-GiB transfers, **streaming in `32 MiB` chunks over `@cc.stream` inside a strict `1 GiB` role budget (`4 GiB = 128 × 32 MiB`)** stays entirely inside warm buddy SHM:
     - **`published` (`4.0 GiB`):** C-Two completes in **0.290 s (`13.80 GiB/s`, P50 `2.25 ms`, P95 `2.67 ms`)** using **64.0 MiB** server pool and **64.0 MiB** client pool (`+96.1 MiB` client live RSS), **1.99× faster than Ray** (`0.577 s / 6.93 GiB/s`, P95 `12.10 ms`, `+1,472.3 MiB` client live RSS).
     - **`fresh` (`4.0 GiB`):** C-Two completes in **0.448 s (`8.92 GiB/s`, P50 `3.48 ms`, P95 `4.05 ms`)** inside **64.0 MiB** server pool / **32.0 MiB** client pool (`185.0 MiB` server peak RSS), with **1.29× tighter P95** than Ray (`0.400 s / 10.01 GiB/s`, P95 `5.21 ms`, `1.5 GiB` Plasma).
5. **Multi-subscriber fan-out (`32 MiB`, `R = 4`) & Kostya (`1M` rows):**
   - Direct IPC `@cc.topic` (`32 MiB, R = 4`) reaches **22.10 GiB/s** (`5.34 ms` P50, `8.44 ms` P95, `25.3 ms` total burst publish for 8 × 32 MiB — **2.95× faster than the `2026-11-21` baseline `7.48 GiB/s`**), with single-hop `c3 relay` at **7.68 GiB/s** (`+39.6%`) and two-hop `dual_relay` at **6.17 GiB/s** (`+29.9%`).
   - Direct IPC `@cc.stream` fan-out (`32 MiB, R = 4`) reaches **20.47 GiB/s** (**3.74× faster than `2026-11-21` `5.48 GiB/s`**).
   - Structured Kostya (`1,000,000` rows): `ctwo-fastdb-packed` drops from `27.57 ms →` **14.210 ms P50 / 14.627 ms P95** — **3.85× faster than `ray-named-actor-arrays`** (`54.77 ms`) and **10.18× faster than `ctwo-pickle-arrays`** (`144.64 ms`).

---

## Copy-Count Lower Bound (SHM Path, `fastdb4py 0.3.0` + `WritableSlab`)

| Stage | `0.11.0` (`fastdb4py 0.2.4`) Unary / Stream | **Current (`fastdb4py 0.3.0` + `WritableSlab`) Unary / Stream / Topic** |
| --- | ---: | ---: |
| FastDB image → Python `bytes` → send `Vec` | 3 / 2 | **0** (`BuildPlan` executes directly into `WritableSlab`; `Payload` copies via `memoryview()`) |
| Write sender SHM → read SHM into receiver warm `RecycledBytes` (ADR 0001) | 2 / 2 | **2** |
| Receiver `bytes(mv)` → ctypes copy → FastDB `open_copy` | 3 / 3 | **0** (`Payload.open_external(ExternalBytes(native_buf))` + `acc.memoryview()`) |
| **One-way full-payload copy lower bound** | **8 / 7** | **2** |
| — C-Two transport share | 2 / 2 | **2** |
| — Python / `fastdb4py` glue share | 6 / 5 | **0** |

## Suite 0: Single-Process FastDB 0.3.0 Stage Decomposition (`p50 ms`)

All runs reported `fastdb_execution_mode = DIRECT` (`1`), `fastdb_staging_bytes = 0` (`11` measured rounds after `3` warmup rounds).

| Logical Size | Encoded Size | `build_external_plan` | `execute_direct_into_buffer` | `memoryview_export` | `open_external(mv)` | `open_copy(bytes(mv))` (ref) | `view` + NumPy sum | `release` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 256 KiB | 262,744 B | 0.0223 ms | 0.0324 ms | 0.0064 ms | **0.0186 ms** | 0.0350 ms | 0.0463 ms | 0.0007 ms |
| 4 MiB | 4,194,904 B | 0.0223 ms | 0.1152 ms | 0.0067 ms | **0.0190 ms** | 0.2542 ms | 0.1400 ms | 0.0007 ms |
| 32 MiB | 33,555,032 B | **0.0527 ms** | 0.8122 ms | 0.0136 ms | **0.0200 ms** | 2.0679 ms | 0.9097 ms | 0.0023 ms |

## Suite 1: KB-Scale Bidirectional Coupling Exchange (`60` rounds, `10` warmup)

| Logical Size | Arm | Path / Topology | P50 (ms) | P95 (ms) | P99 (ms) | Req/s (P50) | Share of 20 ms step (P95) |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 KiB | `ctwo-fastdb-unary` | direct IPC (inline, 1,624 B) | 0.5262 | **0.7254** | **0.7495** | 1,900.4 | 3.50% |
| 1 KiB | `ctwo-pickle-arrays` | direct IPC (Python-only) | 0.1793 | 0.4928 | **0.8797** | 5,576.2 | 2.40% |
| 1 KiB | `ray-named-actor` | dynamic named actor (inline) | **0.4775** | 0.7861 | 1.0114 | **2,094.4** | 3.78% |
| 1 KiB | `ray-compiled-graph` | static compiled DAG | **0.1702** | **0.3224** | 4.1150 | **5,876.6** | **1.59%** |
| 4 KiB | `ctwo-fastdb-unary` | direct IPC (inline, 4,696 B) | 0.4832 | 0.6691 | 0.7717 | 2,069.7 | 3.24% |
| 16 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 16,984 B) | **0.5028** | **0.7713** | **0.9745** | **1,988.7** | 3.71% |
| 16 KiB | `ctwo-pickle-arrays` | direct IPC (Python-only) | **0.2017** | **0.2422** | **0.2450** | **4,957.7** | **1.20%** |
| 16 KiB | `ray-named-actor` | dynamic named actor (inline) | 0.6880 | 1.4610 | 2.2747 | 1,453.4 | 6.81% |
| 16 KiB | `ray-compiled-graph` | static compiled DAG | 0.2269 | 0.6253 | 1.3830 | 4,407.7 | 3.03% |
| 64 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 66,136 B) | 0.6153 | 1.2539 | 2.0007 | 1,625.2 | 5.90% |

## Suite 2: Full-Field Grid Snapshots & Live Streams (`256 KiB` → `1 GiB`)

### 2A. Direct IPC Size Ladder (`ctwo-fastdb-unary` vs `ctwo-fastdb-stream`)

| Size | Mode | Unary P50 / P95 (ms) | Unary GiB/s | Stream P50 / P95 (ms) | Stream Steady GiB/s | Server Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 256 KiB | published | 0.255 / 0.425 | 0.96 | 0.205 / 0.328 | 1.15 | 57.8 |
| 256 KiB | fresh | 0.324 / 0.549 | 0.75 | 0.244 / 0.371 | 0.94 | 57.8 |
| 1 MiB | published | 0.310 / 0.470 | 3.15 | 0.204 / 0.430 | 4.14 | 61.0 |
| 1 MiB | fresh | 0.438 / 0.580 | 2.23 | 0.326 / 0.619 | 2.57 | 62.7 |
| 4 MiB | published | 0.633 / 1.062 | 6.17 | **0.414 / 0.586** | **8.67** | 72.9 |
| 4 MiB | fresh | 0.952 / 1.125 | 4.10 | **0.808 / 1.059** | **4.83** | 73.1 |
| 16 MiB | published | 1.870 / 2.104 | 8.36 | **1.363 / 2.175** | **10.98** | 121.0 |
| 16 MiB | fresh | 2.300 / 2.812 | 6.79 | **1.939 / 2.898** | **7.48** | 120.9 |
| 32 MiB | published | **3.122 / 3.427** | **10.01** | **2.497 / 4.216** | **11.70** | 184.9 |
| 32 MiB | fresh | **4.099 / 4.696** | **7.62** | **3.762 / 4.697** | **8.08** | 185.2 |
| 256 MiB | published | **62.622 / 63.903** | **3.99** | **62.167 / 117.402** | 3.49 | 1,081.0 |
| 256 MiB | fresh | **64.687 / 67.436** | **3.86** | **64.568 / 80.190** | 3.75 | 1,081.1 |
| 512 MiB | published | 121.908 / 122.242 | 4.10 | 117.601 / 209.656 | 3.88 | 2,104.9 |
| 512 MiB | fresh | 124.751 / 127.535 | 4.01 | 125.248 / 167.264 | 3.62 | 2,105.2 |
| 1 GiB | published | 277.454 / 372.373 | 3.60 | 369.289 / 701.247 | 2.21 | 4,153.7 |
| 1 GiB | fresh | **254.560 / 256.291** | **3.93** | 262.469 / 313.703 | 3.67 | 4,153.3 |

### 2B. Cross-Arm Comparison (`4 MiB`, `32 MiB`, `256 MiB`, `512 MiB`, `1 GiB`)

| Size | Mode | `ctwo-fastdb-stream` P50 / P95 (GiB/s) | `ctwo-fastdb-unary` P50 / P95 (GiB/s) | `ctwo-fastdb-hold` P50 (GiB/s) | `ctwo-pickle-arrays` P50 (GiB/s) | `ray-named-actor` P50 / P95 (GiB/s) | `ray-compiled-graph` P50 / P95 (GiB/s) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 MiB | published | **0.414 / 0.586 ms (9.43)** | 0.633 / 1.062 ms (6.17) | 0.762 ms (5.13) | 1.498 ms (2.61) | 3.746 / 5.001 ms (1.04) | 1.250 / 12.037 ms (3.13) |
| 4 MiB | fresh | 0.808 / **1.059 ms** (4.83) | 0.952 / 1.125 ms (4.10) | 0.896 ms (4.36) | 1.556 ms (2.51) | 1.921 / 2.271 ms (2.03) | **0.590** / 2.899 ms (**6.62**) |
| 32 MiB | published | **2.497 / 4.216 ms (12.52)** | 3.122 / **3.427 ms** (10.01) | 3.151 ms (9.92) | 12.283 ms (2.54) | 13.092 / 14.274 ms (2.39) | 13.960 / 23.951 ms (2.24) |
| 32 MiB | fresh | **3.762 / 4.697 ms (8.31)** | 4.099 / **4.696 ms** (7.62) | 4.026 ms (7.76) | 12.988 ms (2.41) | 6.031 / 7.746 ms (5.18) | 3.963 / 5.079 ms (7.89) |
| 256 MiB | published | **62.167 / 117.402 ms (4.02)** | 62.622 / **63.903 ms** (3.99) | 63.279 ms (3.95) | 137.628 ms (1.82) | 76.155 / 79.391 ms (3.28) | `OutOfDiskError` |
| 256 MiB | fresh | **64.568 / 80.190 ms (3.87)** | 64.687 / **67.436 ms** (3.86) | 66.745 ms (3.75) | 112.380 ms (2.22) | 92.572 / 94.496 ms (2.70) | `OutOfDiskError` |
| 512 MiB | published | 117.601 / 209.656 ms (4.25) | 121.908 / 122.242 ms (4.10) | 118.469 ms (4.22) | 335.337 ms (1.49) | **22.470 / 62.748 ms (22.25)** | `OutOfDiskError` |
| 512 MiB | fresh | 125.248 / 167.264 ms (3.99) | 124.751 / 127.535 ms (4.01) | 126.777 ms (3.94) | 223.193 ms (2.24) | **33.790 / 95.682 ms (14.80)** | `OutOfDiskError` |
| 1 GiB | published | 369.289 / 701.247 ms (2.71) | 277.454 / 372.373 ms (3.60) | 367.213 ms (2.72) | 1,282.466 ms (0.78) | **44.077 / 81.354 ms (22.69)** | `OutOfDiskError` |
| 1 GiB | fresh | 262.469 / 313.703 ms (3.81) | 254.560 / 256.291 ms (3.93) | 256.557 ms (3.90) | 1,398.434 ms (0.72) | **70.345 / 82.933 ms (14.22)** | `OutOfDiskError` |

Explicit 1-hop `c3 relay` stream (`ctwo-fastdb-relay-stream`) reaches **1.944 ms (`2.01 GiB/s`) `published` / 1.905 ms (`2.05 GiB/s`) `fresh`** at `4 MiB`, and **11.096 ms (`2.82 GiB/s`) `published` / 11.087 ms (`2.82 GiB/s`) `fresh`** at `32 MiB` — faster over HTTP relay than local `ray-named-actor` on `32 MiB published` (`13.092 ms`).

## Suite 5: `4 GiB` Bounded-Memory Streamed Migration (`128 × 32 MiB` in `1 GiB` RAM Budget)

| Mode | Arm | Total Moved | Elapsed (s) | Effective GiB/s | Per-Chunk P50 / P95 (ms) | Server Pool / Client Pool Peak (MiB) | Server Peak RSS (MiB) | Client Live RSS Delta (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| published | `ctwo-fastdb-stream-migration` | 4.0 GiB | **0.290** | **13.80** | 2.25 / **2.67** | **64.0 / 64.0** | **184.6** | **+96.1** (`107.5 → 203.6`) |
| published | `ray-actor-stream-migration` | 4.0 GiB | 0.577 | 6.93 | **1.55** / 12.10 | `1.5 GiB` Plasma | — | +1,472.3 (`204.3 → 1,676.6`) |
| fresh | `ctwo-fastdb-stream-migration` | 4.0 GiB | 0.448 | 8.92 | 3.48 / **4.05** | **64.0 / 32.0** | **185.0** | **+32.0** (`1,676.6 → 1,708.6`) |
| fresh | `ray-actor-stream-migration` | 4.0 GiB | **0.400** | **10.01** | **2.39** / 5.21 | `1.5 GiB` Plasma | — | +0.0 (`1,676.6 → 1,676.7`) |

## Suites 3 & 4: Multi-Subscriber Fan-Out (`32 MiB`, `8` Frames/Reader, `R = 1, 2, 4`)

| Transport | Primitive | `R` | Publish (ms) | Elapsed (s) | Per-Reader P50 / P95 (ms) | Per-Reader FPS | Agg GiB/s | Server Pool Peak (MiB) | Server Peak RSS (MiB) | Relay A / B Peak RSS (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct_ipc` | `@cc.stream` | 1 | — | 0.0207 | 2.26 / 3.90 | 386.64 | 12.08 | 64.00 | 185.1 | — |
| `direct_ipc` | `@cc.topic` | 1 | 24.5 | 0.0351 | 3.93 / 6.12 | 227.84 | 7.12 | 384.06 | 409.1 | — |
| `direct_ipc` | `@cc.stream` | 2 | — | 0.0320 | 3.24 / 7.18 | 250.26 | 15.64 | 128.00 | 217.4 | — |
| `direct_ipc` | `@cc.topic` | 2 | 22.8 | 0.0377 | 4.31 / 7.24 | 212.10 | 13.26 | 384.06 | 409.1 | — |
| `direct_ipc` | `@cc.stream` | 4 | — | 0.0489 | 4.98 / 12.18 | 163.76 | 20.47 | 256.00 | 281.0 | — |
| `direct_ipc` | `@cc.topic` | 4 | 25.3 | **0.0452** | 5.34 / **8.44** | **176.80** | **22.10** | **384.06** | 409.3 | — |
| `explicit_relay` | `@cc.stream` | 4 | — | 0.1315 | 14.71 / 22.15 | 60.83 | 7.60 | 256.00 | 282.9 | — |
| `explicit_relay` | `@cc.topic` | 4 | 23.1 | **0.1302** | 14.71 / 29.31 | **61.47** | **7.68** | **384.06** | 410.6 | 342.7 / — |
| `dual_relay` | `@cc.topic` | 4 | 18.8 | 0.1620 | 16.52 / 42.23 | 49.39 | **6.17** | **384.06** | 410.4 | 321.1 / 134.8 |

## Suite 6: Structured Coordinates Replacement Baseline (Kostya, `1K` / `100K` / `1M` Rows)

| Rows (`N`) | Arm | RPC P50 (ms) | Extract P50 (ms) | Reduce P50 (ms) | Total P50 (ms) | Total P95 (ms) | Speedup vs `pickle-arrays` / `ray-actor` |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 1,000 | `ctwo-fastdb-packed` | 0.404 | 0.061 | 0.007 | 0.531 | 0.728 | 0.37× / 0.98× |
| 1,000 | `ctwo-pickle-arrays` | 0.184 | 0.000 | 0.006 | **0.198** | **0.416** | — |
| 1,000 | `ray-named-actor-arrays` | 0.505 | 0.000 | 0.008 | 0.521 | 0.730 | — |
| 100,000 | `ctwo-fastdb-packed` | 12.207 | 0.098 | 0.087 | 12.464 | 13.385 | 0.73× / 0.52× |
| 100,000 | `ctwo-pickle-arrays` | 8.325 | 0.000 | 0.089 | 9.059 | 9.309 | — |
| 100,000 | `ray-named-actor-arrays` | 5.492 | 0.000 | 0.307 | **6.508** | **6.709** | — |
| 1,000,000 | `ctwo-fastdb-packed` | 13.368 | 0.096 | 0.637 | **14.210** | **14.627** | **10.18× / 3.85×** |
| 1,000,000 | `ctwo-pickle-arrays` | 136.614 | 0.001 | 0.694 | 144.636 | 147.498 | — |
| 1,000,000 | `ray-named-actor-arrays` | 44.824 | 0.000 | 3.109 | 54.768 | 56.904 | — |

## Reproduction Commands

```bash
export CARGO_TARGET_DIR="$PWD/core/target"
cargo build --release --manifest-path cli/Cargo.toml
uv sync --frozen --group benchmark
uv run --no-sync maturin develop --release -m sdk/python/native/Cargo.toml

# 1. Guarded public-API smoke test (all 7 suites, C-Two only)
C2_RELAY_ANCHOR_ADDRESS= CTWO_TEST_C3_BIN="$CARGO_TARGET_DIR/release/c3" \
  uv run --no-sync pytest sdk/python/tests/integration/test_portable_payload_benchmark.py -v

# 2. Full 7-suite headless benchmark vs Ray 2.56
RAY_ENABLE_UV_RUN_RUNTIME_ENV=0 C2_RELAY_ANCHOR_ADDRESS= \
  .venv/bin/python sdk/python/benchmarks/portable_payload_benchmark.py \
  --suite all --include-ray \
  --c3-bin "$CARGO_TARGET_DIR/release/c3" \
  --output docs/acceptance/evidence/2026-11-22-kb-to-gb-slab-benchmark.json
```
