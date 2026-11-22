# KB-to-GB Unified Slab & FastDB 0.3.0 Benchmark vs Ray 2.56 (Planned 0.12.0)

**Date:** 2026-11-22  
**Scope:** Reproducible headless evaluation of C-Two's Workstream 2 unified `WritableSlab` direct-SHM channel, FastDB `0.3.0` (`ABI-118`) zero-copy `ExternalBytes` / `memoryview()` / `BuildPlan` integration, buddy geometry & warm local buffer recycling, and KB-scale inline framing trimming under [ADR 0001](../decisions/0001-single-owner-release.md) single-owner copy-then-release semantics. Covers `1 KiB–1 GiB` single-frame payloads, `4 GiB` bounded-memory streamed migration inside a strict `1 GiB` role RAM budget, multi-subscriber fan-out (`@cc.stream` and `@cc.topic`, `R = 1, 2, 4`), and 1M-row structured coordinates (Kostya), paired on the identical Python 3.13.3 interpreter against `Ray 2.56.0` (`detached named actor`, `Ray Compiled Graphs`, and Ray actor streaming generator).

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
| Source commit (benchmark run) | `6861f95e` (+ SDK `OpenOptions` total-byte lift for `> 256 MiB` payloads & benchmark Ray isolation) |

## Key Findings (Summary)

1. **One-way SHM copy lower bound cut from `8 / 7` copies to `2` copies (ADR 0001 theoretical floor):**
   - Eliminated all `5–6` Python/`fastdb4py` glue copies across unary, server streams, pub/sub topics, and client requests: fresh `BuildPlan` executes directly into C-Two's `WritableSlab` SHM (`0` staging), published `Payload` copies once via `Payload.memoryview()` into SHM, receivers copy once from peer SHM into warm-recycled `RecycledBytes` before `RELEASE`, and `Payload.open_external(ExternalBytes(response))` + `acc.memoryview()` decode and reduce with **0 copies**.
   - Single-process `open_external(memoryview)` at `32 MiB` drops from **3.49 ms** (`0.2.4` `open_copy`) to **0.0183 ms** (**191× faster** than `0.2.4` `open_copy`, **104× faster** than `0.3.0` `open_copy`).
2. **KB-scale bidirectional coupling (`1 KiB` & `16 KiB`):**
   - Portable `ctwo-fastdb-unary` achieves **0.402 ms P50 / 0.589 ms P95** (`2,490 req/s`) at `1 KiB` and **0.457 ms P50 / 0.591 ms P95** (`2,187 req/s`, down from `0.618 / 0.799 ms`) at `16 KiB`, beating `ray-named-actor` at both sizes (`0.445 / 0.629 ms` at `1 KiB`; `0.499 / 0.858 ms` at `16 KiB`).
   - Python-only `ctwo-pickle-arrays` reaches **0.138 ms P50 / 0.188 ms P95** (`7,229 req/s`) at `1 KiB` and **0.141 ms P50 / 0.218 ms P95** at `16 KiB` (down `-35% P50 / -48% P95` from `0.217 / 0.422 ms`), beating static `ray-compiled-graph` at `1 KiB` P95 (`0.188 vs 0.215 ms`) and P99 (`0.234 vs 0.328 ms`) without static DAG compilation.
3. **Mid-to-large frames (`4 MiB` & `32 MiB`) — C-Two beats both Ray Named Actor and Ray Compiled Graphs:**
   - **4 MiB `published`:** `ctwo-fastdb-stream` hits **0.391 ms P50 / 0.563 ms P95 (`9.98 GiB/s`)** — **3.06× faster than `0.11.0`**, **5.66× faster than `ray-named-actor`** (`2.215 ms`), and beats `ray-compiled-graph` at both P50 (`0.443 ms`) and P95 (`2.053 ms`, **3.65× tighter**).
   - **32 MiB `published`:** `ctwo-fastdb-stream` hits **2.408 ms P50 / 3.169 ms P95 (`12.98 GiB/s`)** — **5.60× faster than `0.11.0`** (`13.50 ms`), **5.12× faster than `ray-named-actor`** (`12.32 ms`), and **5.33× faster than `ray-compiled-graph`** (`12.84 ms`). Even unary (`3.135 ms`, `9.97 GiB/s`) and `cc.hold` (`2.911 ms`, `10.73 GiB/s`) beat Ray by **~4×**.
   - **32 MiB `fresh`:** `ctwo-fastdb-stream` hits **3.839 ms P50 / 4.915 ms P95 (`8.14 GiB/s`)** — **5.22× faster than `0.11.0`** (`20.03 ms`), **1.59× faster than `ray-named-actor`** (`6.107 ms`), and **3.32× faster than `ray-compiled-graph`** (`12.74 ms`), while server peak RSS drops **4.1×** (`759.1 MiB → 184.9 MiB`) with zero cyclic GC retention.
4. **Monolithic `256 MiB / 512 MiB / 1 GiB` single frames vs `4 GiB` bounded-memory streamed migration:**
   - At **256 MiB**, `ctwo-fastdb-stream` achieves **56.89 ms (`4.39 GiB/s`) `published`** and **65.54 ms (`3.81 GiB/s`) `fresh`**, beating `ray-named-actor` (`95.69 ms` `published` / `89.20 ms` `fresh`), while `ray-compiled-graph` fails with `OutOfDiskError` on all `>= 256 MiB` frames.
   - At **512 MiB–1 GiB** single monolithic frames, payloads exceed C-Two's `64 MiB` buddy segment ceiling and take `Dedicated` SHM (per-call `shm_open`/`ftruncate`/`mmap`/page-fault/`munmap` on both peers), sustaining **3.3–4.66 GiB/s** (**2.0×–3.3× faster than `ctwo-pickle-arrays`**), while `ray-named-actor` achieves `13.9–22.6 GiB/s` on `512 MiB` and `1 GiB fresh` via persistent zero-copy Plasma mappings (and fails with `OutOfDiskError` on `1 GiB published` when actor retention + Plasma copy exceed `1.5 GiB` Plasma).
   - For multi-hundred-MiB to multi-GiB transfers, **streaming in `32 MiB` chunks over `@cc.stream` inside a strict `1 GiB` role budget (`4 GiB = 128 × 32 MiB`)** stays entirely inside warm buddy SHM:
     - **`published` (`4.0 GiB`):** C-Two completes in **0.297 s (`13.45 GiB/s`, P95 `2.68 ms`)** using **64.0 MiB** server pool and **64.0 MiB** client pool (`+96 MiB` client live RSS), **2.07× faster than Ray** (`0.614 s / 6.51 GiB/s`, P95 `12.86 ms`, `+1,472 MiB` client live RSS).
     - **`fresh` (`4.0 GiB`):** C-Two completes in **0.514 s (`7.78 GiB/s`, P95 `4.80 ms`)** inside **64.0 MiB** server pool / **32.0 MiB** client pool (`184.8 MiB` server peak RSS), with tighter P95 than Ray (`0.394 s / 10.14 GiB/s`, P95 `4.83 ms`, `1.5 GiB` Plasma).
5. **Multi-subscriber fan-out (`32 MiB`, `R = 4`) & Kostya (`1M` rows):**
   - Direct IPC `@cc.topic` (`32 MiB, R = 4`) reaches **21.43 GiB/s** (`5.23 ms` P50, `9.87 ms` P95, `26.7 ms` total burst publish for 8 × 32 MiB — **2.86× faster than the `2026-11-21` baseline `7.48 GiB/s`**), with single-hop `c3 relay` at **7.84 GiB/s** (`+42.5%`) and two-hop `dual_relay` at **5.56 GiB/s** (`+17.1%`).
   - Direct IPC `@cc.stream` fan-out (`32 MiB, R = 4`) reaches **20.80 GiB/s** (**3.80× faster than `2026-11-21` `5.48 GiB/s`**).
   - Structured Kostya (`1,000,000` rows): `ctwo-fastdb-packed` drops from `27.57 ms →` **14.264 ms P50 / 17.097 ms P95** — **3.75× faster than `ray-named-actor-arrays`** (`53.43 ms`) and **10.60× faster than `ctwo-pickle-arrays`** (`151.16 ms`).

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
| 256 KiB | 262,744 B | 0.0286 ms | 0.0332 ms | 0.0066 ms | **0.0190 ms** | 0.0372 ms | 0.0480 ms | 0.0007 ms |
| 4 MiB | 4,194,904 B | 0.1533 ms | 0.1197 ms | 0.0070 ms | **0.0190 ms** | 0.2550 ms | 0.1403 ms | 0.0007 ms |
| 32 MiB | 33,555,032 B | 0.7719 ms | 0.7898 ms | 0.0127 ms | **0.0183 ms** | 1.9134 ms | 0.8510 ms | 0.0020 ms |

## Suite 1: KB-Scale Bidirectional Coupling Exchange (`60` rounds, `10` warmup)

| Logical Size | Arm | Path / Topology | P50 (ms) | P95 (ms) | P99 (ms) | Req/s (P50) | Share of 20 ms step (P95) |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 KiB | `ctwo-fastdb-unary` | direct IPC (inline, 1,624 B) | **0.4016** | **0.5893** | **0.6215** | **2,489.9** | 2.86% |
| 1 KiB | `ctwo-pickle-arrays` | direct IPC (Python-only) | 0.1383 | **0.1883** | **0.2335** | 7,228.9 | **0.93%** |
| 1 KiB | `ray-named-actor` | dynamic named actor (inline) | 0.4445 | 0.6289 | 0.7767 | 2,249.5 | 3.05% |
| 1 KiB | `ray-compiled-graph` | static compiled DAG | **0.1272** | 0.2153 | 0.3280 | **7,861.1** | 1.06% |
| 4 KiB | `ctwo-fastdb-unary` | direct IPC (inline, 4,696 B) | 0.4797 | 0.5964 | 0.6562 | 2,084.8 | 2.90% |
| 16 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 16,984 B) | **0.4573** | **0.5913** | **0.6691** | **2,186.8** | 2.87% |
| 16 KiB | `ctwo-pickle-arrays` | direct IPC (Python-only) | 0.1412 | 0.2180 | 0.2672 | 7,079.6 | 1.08% |
| 16 KiB | `ray-named-actor` | dynamic named actor (inline) | 0.4993 | 0.8579 | 2.0072 | 2,002.7 | 4.11% |
| 16 KiB | `ray-compiled-graph` | static compiled DAG | **0.1057** | **0.1769** | **0.2090** | **9,460.0** | **0.88%** |
| 64 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 66,136 B) | 0.5410 | 0.7110 | 0.8172 | 1,848.3 | 3.43% |

## Suite 2: Full-Field Grid Snapshots & Live Streams (`256 KiB` → `1 GiB`)

### 2A. Direct IPC Size Ladder (`ctwo-fastdb-unary` vs `ctwo-fastdb-stream`)

| Size | Mode | Unary P50 / P95 (ms) | Unary GiB/s | Stream P50 / P95 (ms) | Stream Steady GiB/s | Server Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 256 KiB | published | 0.264 / 0.356 | 0.92 | 0.175 / 0.330 | 1.31 | 57.5 |
| 256 KiB | fresh | 0.366 / 0.497 | 0.67 | 0.254 / 0.424 | 0.87 | 58.0 |
| 1 MiB | published | 0.278 / 0.405 | 3.51 | 0.157 / 0.261 | 5.87 | 61.6 |
| 1 MiB | fresh | 0.451 / 0.585 | 2.16 | 0.357 / 0.602 | 2.52 | 63.0 |
| 4 MiB | published | 0.563 / 0.836 | 6.94 | **0.391 / 0.563** | **9.50** | 77.2 |
| 4 MiB | fresh | 1.058 / 1.720 | 3.69 | **0.806 / 1.016** | **4.72** | 74.1 |
| 16 MiB | published | 1.793 / 3.225 | 8.72 | **1.313 / 2.013** | **11.60** | 136.9 |
| 16 MiB | fresh | 3.023 / 4.248 | 5.17 | **2.461 / 3.171** | **6.27** | 121.0 |
| 32 MiB | published | 3.135 / 6.058 | 9.97 | **2.408 / 3.169** | **12.92** | 217.4 |
| 32 MiB | fresh | 4.774 / 7.640 | 6.55 | **3.839 / 4.915** | **7.80** | 184.9 |
| 256 MiB | published | 105.655 / 114.963 | 2.37 | 56.888 / 77.117 | 3.92 | 1,337.1 |
| 256 MiB | fresh | 90.861 / 92.035 | 2.75 | 65.543 / 73.572 | 3.79 | 1,080.8 |
| 512 MiB | published | 117.193 / 177.305 | 4.27 | 110.075 / 147.216 | 4.04 | 2,617.6 |
| 512 MiB | fresh | 133.788 / 231.277 | 3.74 | 140.352 / 157.654 | 3.45 | 2,104.6 |
| 1 GiB | published | 680.480 / 709.944 | 1.47 | 304.019 / 311.521 | 3.65 | 5,176.8 |
| 1 GiB | fresh | 279.033 / 409.640 | 3.58 | 292.945 / 316.296 | 3.47 | 4,154.0 |

### 2B. Cross-Arm Comparison (`4 MiB`, `32 MiB`, `256 MiB`, `512 MiB`, `1 GiB`)

| Size | Mode | `ctwo-fastdb-stream` P50 / P95 (GiB/s) | `ctwo-fastdb-unary` P50 (GiB/s) | `ctwo-fastdb-hold` P50 (GiB/s) | `ctwo-pickle-arrays` P50 (GiB/s) | `ray-named-actor` P50 / P95 (GiB/s) | `ray-compiled-graph` P50 / P95 (GiB/s) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 MiB | published | **0.391 / 0.563 ms (9.98)** | 0.563 ms (6.94) | 0.657 ms (5.94) | 1.249 ms (3.13) | 2.215 / 2.454 ms (1.76) | 0.443 / 2.053 ms (8.81) |
| 4 MiB | fresh | 0.806 / **1.016 ms** (4.85) | 1.058 ms (3.69) | 1.003 ms (3.89) | 1.346 ms (2.90) | 1.595 / 1.813 ms (2.45) | **0.618** / 2.397 ms (**6.32**) |
| 32 MiB | published | **2.408 / 3.169 ms (12.98)** | 3.135 ms (9.97) | 2.911 ms (10.73) | 8.347 ms (3.74) | 12.324 / 12.877 ms (2.54) | 12.840 / 15.352 ms (2.43) |
| 32 MiB | fresh | **3.839 / 4.915 ms (8.14)** | 4.774 ms (6.55) | 5.088 ms (6.14) | 9.216 ms (3.39) | 6.107 / 6.455 ms (5.12) | 12.742 / 24.313 ms (2.45) |
| 256 MiB | published | **56.888 / 77.117 ms (4.39)** | 105.655 ms (2.37) | 77.480 ms (3.23) | 146.948 ms (1.70) | 95.686 / 221.814 ms (2.61) | `OutOfDiskError` |
| 256 MiB | fresh | **65.543 / 73.572 ms (3.81)** | 90.861 ms (2.75) | 87.144 ms (2.87) | 125.483 ms (1.99) | 89.201 / 95.680 ms (2.80) | `OutOfDiskError` |
| 512 MiB | published | 110.075 / 147.216 ms (4.54) | 117.193 ms (4.27) | 107.231 ms (4.66) | 284.328 ms (1.76) | **22.096 / 61.061 ms (22.63)** | `OutOfDiskError` |
| 512 MiB | fresh | 140.352 / 157.654 ms (3.56) | 133.788 ms (3.74) | 140.243 ms (3.57) | 211.887 ms (2.36) | **35.959 / 93.442 ms (13.90)** | `OutOfDiskError` |
| 1 GiB | published | **304.019 / 311.521 ms (3.29)** | 680.480 ms (1.47) | 344.006 ms (2.91) | 868.043 ms (1.15) | `OutOfDiskError` | `OutOfDiskError` |
| 1 GiB | fresh | 292.945 / 316.296 ms (3.41) | 279.033 ms (3.58) | 275.512 ms (3.63) | 907.864 ms (1.10) | **67.122 / 102.817 ms (14.90)** | `OutOfDiskError` |

Explicit 1-hop `c3 relay` stream (`ctwo-fastdb-relay-stream`) reaches **1.423 ms (`2.74 GiB/s`) `published` / 1.926 ms `fresh`** at `4 MiB`, and **10.380 ms (`3.01 GiB/s`) `published` / 12.707 ms (`2.46 GiB/s`) `fresh`** at `32 MiB` — faster over HTTP relay than local `ray-named-actor` on `32 MiB published` (`12.324 ms`).

## Suite 5: `4 GiB` Bounded-Memory Streamed Migration (`128 × 32 MiB` in `1 GiB` RAM Budget)

| Mode | Arm | Total Moved | Elapsed (s) | Effective GiB/s | Per-Chunk P50 / P95 (ms) | Server Pool / Client Pool Peak (MiB) | Server Peak RSS (MiB) | Client Live RSS Delta (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| published | `ctwo-fastdb-stream-migration` | 4.0 GiB | **0.297** | **13.45** | 2.33 / **2.68** | **64.0 / 64.0** | **217.6** | **+96.1** (`111.1 → 207.1`) |
| published | `ray-actor-stream-migration` | 4.0 GiB | 0.614 | 6.51 | **1.50** / 12.86 | `1.5 GiB` Plasma | — | +1,472.1 (`207.8 → 1,679.9`) |
| fresh | `ctwo-fastdb-stream-migration` | 4.0 GiB | 0.514 | 7.78 | 3.97 / **4.80** | **64.0 / 32.0** | **184.8** | **+32.0** (`1,679.9 → 1,711.9`) |
| fresh | `ray-actor-stream-migration` | 4.0 GiB | **0.394** | **10.14** | **2.45** / 4.83 | `1.5 GiB` Plasma | — | +0.0 (`1,679.9 → 1,679.9`) |

## Suites 3 & 4: Multi-Subscriber Fan-Out (`32 MiB`, `8` Frames/Reader, `R = 1, 2, 4`)

| Transport | Primitive | `R` | Publish (ms) | Elapsed (s) | Per-Reader P50 / P95 (ms) | Per-Reader FPS | Agg GiB/s | Server Pool Peak (MiB) | Server Peak RSS (MiB) | Relay A / B Peak RSS (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct_ipc` | `@cc.stream` | 1 | — | 0.0210 | 2.47 / 4.34 | 381.21 | 11.91 | 64.00 | 217.7 | — |
| `direct_ipc` | `@cc.topic` | 1 | 23.5 | 0.0342 | 3.85 / 6.57 | 234.11 | 7.32 | 384.06 | 440.8 | — |
| `direct_ipc` | `@cc.stream` | 2 | — | 0.0328 | 3.28 / 7.85 | 244.09 | 15.26 | 128.00 | 249.0 | — |
| `direct_ipc` | `@cc.topic` | 2 | 23.7 | 0.0384 | 4.34 / 7.91 | 208.49 | 13.03 | 384.06 | 442.3 | — |
| `direct_ipc` | `@cc.stream` | 4 | — | 0.0481 | 4.85 / 11.48 | 166.37 | 20.80 | 256.00 | 313.5 | — |
| `direct_ipc` | `@cc.topic` | 4 | 26.7 | **0.0467** | 5.23 / **9.87** | **171.43** | **21.43** | **384.06** | 441.3 | — |
| `explicit_relay` | `@cc.stream` | 4 | — | 0.1363 | 16.79 / 21.38 | 58.69 | 7.34 | 256.00 | 314.6 | — |
| `explicit_relay` | `@cc.topic` | 4 | 26.8 | **0.1275** | **14.74** / 24.98 | **62.74** | **7.84** | **384.06** | 442.5 | 327.2 / — |
| `dual_relay` | `@cc.topic` | 4 | 23.6 | 0.1797 | 19.46 / 45.08 | 44.52 | 5.56 | **384.06** | 443.2 | 350.1 / 103.5 |

## Suite 6: Structured Coordinates Replacement Baseline (Kostya, `1K` / `100K` / `1M` Rows)

| Rows (`N`) | Arm | RPC P50 (ms) | Extract P50 (ms) | Reduce P50 (ms) | Total P50 (ms) | Total P95 (ms) | Speedup vs `pickle-arrays` / `ray-actor` |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 1,000 | `ctwo-fastdb-packed` | 0.390 | 0.057 | 0.005 | **0.510** | **0.551** | 0.37× / **1.07×** |
| 1,000 | `ctwo-pickle-arrays` | 0.177 | 0.000 | 0.005 | 0.190 | 0.230 | — |
| 1,000 | `ray-named-actor-arrays` | 0.522 | 0.000 | 0.008 | 0.544 | 0.724 | — |
| 100,000 | `ctwo-fastdb-packed` | 12.059 | 0.099 | 0.087 | 12.345 | 12.575 | 0.74× / 0.50× |
| 100,000 | `ctwo-pickle-arrays` | 8.261 | 0.000 | 0.086 | 9.079 | 9.378 | — |
| 100,000 | `ray-named-actor-arrays` | 5.433 | 0.000 | 0.093 | 6.228 | 6.370 | — |
| 1,000,000 | `ctwo-fastdb-packed` | 13.444 | 0.097 | 0.599 | **14.264** | **17.097** | **10.60× / 3.75×** |
| 1,000,000 | `ctwo-pickle-arrays` | 140.623 | 0.001 | 0.600 | 151.158 | 157.091 | — |
| 1,000,000 | `ray-named-actor-arrays` | 43.911 | 0.000 | 0.647 | 53.433 | 55.415 | — |

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
