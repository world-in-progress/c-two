# Explicit FastDB Payload, Server Stream, Fan-Out & Ray Comparison Benchmark (0.11.0)

**Date:** 2026-11-20  
**Scope:** Reproducible headless evaluation of C-Two 0.11.0 under its current single-owner copy-then-release semantics ([ADR 0001](../decisions/0001-single-owner-release.md)), covering explicit portable `fastdb4py.payload.Payload` unary calls, `cc.hold`, `@cc.stream`, explicit 1-hop `c3 relay` HTTP streams, multi-subscriber fan-out (`R = 1, 2, 4`), and fair same-interpreter comparison against `Ray 2.56.0` (`detached named actor` and `Ray Compiled Graphs`).

This record formally supersedes the invalidated 2026-04 zero-copy `hold` numbers in [2026-04-18-kostya-benchmark.md](2026-04-18-kostya-benchmark.md) (which relied on the retired peer-held SHM lease model and mixed Python 3.14t with Python 3.12). That historical record remains archived without rewriting.

- **Driver:** [portable_payload_benchmark.py](../../sdk/python/benchmarks/portable_payload_benchmark.py) (public APIs only, guarded by [test_portable_payload_benchmark.py](../../sdk/python/tests/integration/test_portable_payload_benchmark.py))
- **Frozen JSON evidence:** [2026-11-20-portable-payload-benchmark.json](evidence/2026-11-20-portable-payload-benchmark.json)

## Environment & Configuration

| Item | Value |
| --- | --- |
| OS / Arch / CPUs | Darwin 27.2.0, `arm64`, 10 logical CPUs, headless |
| Python | CPython 3.13.3 (`python_gil_enabled = true`) — identical interpreter for C-Two and Ray |
| C-Two / CLI / FastDB / NumPy | `c-two 0.11.0` (release native extension), `c3 0.7.0` (release build), `fastdb4py 0.2.4`, `numpy 2.4.4` |
| Ray | `ray 2.56.0` (`object_store_memory = 1.5 GiB`, `automatic_object_spilling_enabled = false`, default inline threshold `100 KiB`, Compiled Graph default `buffer_size_bytes = 1,000,000 B`, `max_inflight_executions = 10`) |
| C-Two role memory budget | `1 GiB` per role (`client` and `server`) |
| Source commit | `fb1cc978` |

## Key Findings (Summary)

1. **Where time actually goes on large explicit FastDB frames (32 MiB):** On the SHM path, a single-direction full-payload transfer performs a lower bound of **8 copies for unary** and **7 copies for a server stream item** — only **2** of those are C-Two's SHM write/receive transport copies; **5–6 copies** occur in the Python/`fastdb4py 0.2.4` glue (`binary_bytes()` = 2 copies, unary send `Vec` = 1, `payload_plan.py`'s `bytes(memoryview)` + `fastdb4py` `_input_bytes()` `from_buffer_copy` + C++ `open_copy` = 3 copies). At 32 MiB, `open_copy(bytes(mv))` alone takes **3.49 ms** vs **1.27 ms** for `open_external(ExternalBytes(bytes))`.
2. **Small-packet bidirectional coupling (16 KiB):** Portable `ctwo-fastdb-unary` achieves **0.618 ms P50 / 0.799 ms P95** (1,618 req/s; **3.84%** of a 20 ms step), outperforming `ray-named-actor` (**0.858 ms P50 / 1.272 ms P95**). Python-only `ctwo-pickle-arrays` (**0.217 ms P50 / 0.422 ms P95**) matches static `ray-compiled-graph` (**0.255 ms P50 / 0.423 ms P95**) without static DAG compilation (`1.45 ms` connect vs `~48 ms` graph compile).
3. **Full-field snapshots & live streams (4–32 MiB, `R = 1`):**
   - At **4 MiB** (published frame), direct IPC `ctwo-fastdb-stream` reaches **1.20 ms P50 (3.26 GiB/s)** — **2.8× faster than `ray-named-actor`** (3.37 ms P50) and even **2.0× faster than `ray-compiled-graph`** (2.42 ms P50). Explicit 1-hop `c3 relay` HTTP streaming reaches **2.17 ms P50 (1.80 GiB/s)** — still faster than local `ray-named-actor`.
   - At **32 MiB** (published frame), `ctwo-fastdb-stream` achieves **13.50 ms P50 (2.32 GiB/s)**, matching `ray-named-actor` (**13.76 ms P50, 2.27 GiB/s**) and beating `ray-compiled-graph` (**16.23 ms P50**). On **32 MiB fresh frames** (per-step rebuild from a live NumPy grid), `ctwo-fastdb-stream` takes **20.03 ms P50** (including 3.52 ms fresh FastDB build/execute + 3.49 ms `open_copy`), whereas `ray-named-actor` reaches **7.43 ms P50** via zero-copy Plasma ndarray mapping.
4. **Multi-subscriber fan-out amplification (`R = 1 → 4`, 32 MiB) — quantitative baseline for 0.12.0 Pub-Sub:** Under current 1-to-1 `@cc.stream`, scaling from `R = 1` to `R = 4` concurrent 32 MiB client processes pushes per-reader P95 from **38.46 ms → 54.36 ms** (direct IPC, with 4× redundant memory traffic `0.25 → 1.00 GiB` and server RSS climbing **310.8 → 502.9 MiB**) and **28.89 ms → 92.74 ms** (explicit 1-hop `c3 relay`, per-reader rate dropping **45.0 → 20.8 fps** with 4× duplicate bytes crossing the relay hop). This directly motivates adding an orthogonal 1-to-N publish/subscribe primitive in 0.12.0 (same-host single-writer shared-slot broadcast + `c3 relay` single-upstream fan-out).
5. **Structured coordinates replacement (1M rows, same Python 3.13.3):** Even under 0.11.0 copy-then-release semantics, portable `ctwo-fastdb-packed` finishes **1M rows** in **27.57 ms P50 / 28.20 ms P95**, remaining **5.86× faster than `ctwo-pickle-arrays`** (161.60 ms P50) and **2.10× faster than `ray-named-actor-arrays`** (57.80 ms P50, **2.53× faster at P95**).
6. **Upstream `fastdb4py 0.2.4` observations surfaced during measurement:**
   - **Element-list vs packed numeric buffer:** In `fastdb4py 0.2.4`, encoding a 256 KiB `float64` grid as `list<f64>` (`FixedRun`) takes ~47 ms end-to-end (`build + execute + open_copy + materialize`), whereas a portable `fastdb.payload.v1` record carrying structured metadata (`seq`, `rows`, `cols`) plus a contiguous little-endian `<f8` `bytes` field takes **~0.42 ms** (>100× faster) and enables bulk `np.frombuffer` reduction. All grid/coupling suites use the portable structured-header + packed-bytes contract.
   - **Cyclic garbage retention in `Payload.open_copy()`:** `fastdb4py 0.2.4`'s `_input_bytes()` allocates `(ctypes.c_uint8 * n).from_buffer_copy(...)` and calls `ctypes.cast(...)`, leaving a cyclic ctypes reference that retains the full payload buffer after `Payload.close()` until CPython's cyclic GC runs. This accounts for the uncollected client live-RSS jumps (`+104.8 MiB` on 4 MiB × 28 unary calls, `+352.3 MiB` on 32 MiB × 14 unary calls) and higher server RSS on `fresh` builds (`759 MiB` vs `310 MiB` on `published` 32 MiB) when `gc.collect()` is not forced between calls.

---

## Copy-Count Lower Bound (SHM Path, `fastdb4py 0.2.4`)

Excluding kernel page-zeroing, frame headers, initial NumPy allocation, and client-side reduction, a single-direction full-payload transfer on the SHM path performs at least:

| Stage | Unary (`Payload`) | Server Stream Item (`Payload`) |
| --- | ---: | ---: |
| FastDB image → native blob → Python `bytes` (`binary_bytes()`) | 2 | 2 |
| Python `bytes` → C-Two send `Vec` | 1 | 0 (`PyBackedBytes` written directly) |
| Write sender SHM → read SHM into receiver local `Vec` (ADR 0001) | 2 | 2 |
| `bytes(memoryview)` → ctypes `_input_bytes` copy → FastDB `open_copy` | 3 | 3 |
| **One-way full-payload copy lower bound** | **8** | **7** |
| — C-Two transport share | **2** | **2** |
| — Python / `fastdb4py` glue share | **6** | **5** |

## Suite 0: Single-Process FastDB Stage Decomposition (`p50 ms`)

All runs reported `fastdb_execution_mode = DIRECT` (`1`), `fastdb_staging_bytes = 0` (`11` measured rounds after `3` warmup rounds).

| Logical Size | Encoded Size | `build+freeze` | `execute` | `binary_bytes()` | `open_copy(bytes(mv))` | `open_external(bytes(mv))` | `open_external(bytes)` | `View` + NumPy sum | `release` |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 256 KiB | 262,744 B | 0.148 ms | 0.076 ms | 0.026 ms | 0.070 ms | 0.051 ms | 0.036 ms | 0.094 ms | 0.002 ms |
| 4 MiB | 4,194,904 B | 0.368 ms | 0.241 ms | 0.295 ms | 0.547 ms | 0.281 ms | 0.204 ms | 0.327 ms | 0.002 ms |
| 32 MiB | 33,555,032 B | 2.212 ms | 1.311 ms | 1.795 ms | 3.490 ms | 2.189 ms | 1.274 ms | 1.827 ms | 0.005 ms |

## Suite 1: Small-Packet Bidirectional Coupling Exchange (`60` rounds, `10` warmup)

Every round builds a fresh stamped vector on the client, verifies sequence + sum on the server, builds a fresh stamped response vector on the server, and verifies sequence + sum on the client. Note that `16 KiB` logical (`16,984 B` encoded) crosses C-Two's `16 KiB` SHM threshold while remaining below Ray's `100 KiB` inline threshold.

| Logical Size | Arm | Path / Topology | P50 (ms) | P95 (ms) | P99 (ms) | Req/s (P50) | Share of 20 ms step (P95) |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 4 KiB | `ctwo-fastdb-unary` | direct IPC (inline, 4,696 B) | 0.771 | 1.343 | 2.067 | 1,296.5 | 6.29% |
| 16 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 16,984 B) | 0.618 | 0.799 | 1.139 | 1,617.9 | 3.84% |
| 16 KiB | `ctwo-pickle-arrays` | direct IPC (Python-only) | 0.217 | 0.422 | 10.137 | 4,617.2 | 2.06% |
| 16 KiB | `ray-named-actor` | dynamic named actor (inline) | 0.858 | 1.272 | 2.069 | 1,166.0 | 5.98% |
| 16 KiB | `ray-compiled-graph` | static compiled DAG | 0.255 | 0.423 | 0.861 | 3,929.9 | 2.07% |
| 64 KiB | `ctwo-fastdb-unary` | direct IPC (SHM, 66,136 B) | 0.761 | 1.117 | 1.451 | 1,314.3 | 5.29% |

## Suite 2: Full-Field Grid Snapshots & Live Streams

Separates `published` (reading an already-published frame) from `fresh` (constructing a new stamped frame on every step). All arms perform identical sequence verification and full `float64` NumPy sum reduction, and release holds/ndarrays on every round (`client_holds_active = 0`).

### 2A. Direct IPC Size Ladder (`ctwo-fastdb-unary` vs `ctwo-fastdb-stream`)

| Size | Mode | Unary P50 / P95 (ms) | Unary GiB/s | Stream P50 / P95 (ms) | Stream Steady GiB/s | Server Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 256 KiB | published | 0.555 / 1.310 | 0.44 | 0.276 / 0.705 | 0.76 | 56.8 |
| 256 KiB | fresh | 0.688 / 0.923 | 0.35 | 0.478 / 1.132 | 0.41 | 64.9 |
| 1 MiB | published | 0.840 / 1.199 | 1.16 | 0.348 / 0.980 | 2.19 | 63.9 |
| 1 MiB | fresh | 1.286 / 1.972 | 0.76 | 0.772 / 1.233 | 1.20 | 94.9 |
| 4 MiB | published | 2.426 / 2.689 | 1.61 | 1.198 / 1.971 | 3.10 | 86.7 |
| 4 MiB | fresh | 3.898 / 4.300 | 1.00 | 2.422 / 3.749 | 1.52 | 200.1 |
| 16 MiB | published | 10.644 / 11.924 | 1.47 | 6.835 / 11.061 | 2.23 | 182.6 |
| 16 MiB | fresh | 14.964 / 15.833 | 1.04 | 11.109 / 14.541 | 1.39 | 407.0 |
| 32 MiB | published | 20.129 / 21.898 | 1.55 | 13.496 / 19.134 | 2.24 | 310.3 |
| 32 MiB | fresh | 29.128 / 39.783 | 1.07 | 20.027 / 27.336 | 1.48 | 759.1 |

### 2B. Full Cross-Arm Comparison at 4 MiB and 32 MiB

| Size | Mode | Arm | Topology / Path | P50 (ms) | P95 (ms) | GiB/s (P50) | Share of 50 ms step (P95) |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: |
| 4 MiB | published | `ctwo-fastdb-stream` | direct IPC stream | **1.198** | **1.971** | **3.26** | 3.79% |
| 4 MiB | published | `ctwo-pickle-arrays` | direct IPC (Python-only) | 1.689 | 1.973 | 2.31 | 3.80% |
| 4 MiB | published | `ctwo-fastdb-relay-stream` | explicit 1-hop `c3 relay` HTTP | 2.172 | 3.602 | 1.80 | 6.72% |
| 4 MiB | published | `ctwo-fastdb-hold` | direct IPC `cc.hold` | 2.400 | 2.666 | 1.63 | 5.06% |
| 4 MiB | published | `ctwo-fastdb-unary` | direct IPC unary | 2.426 | 2.689 | 1.61 | 5.10% |
| 4 MiB | published | `ray-compiled-graph` | static compiled DAG | 2.423 | 5.503 | 1.61 | 9.91% |
| 4 MiB | published | `ray-named-actor` | dynamic named actor | 3.369 | 4.966 | 1.16 | 9.03% |
| 4 MiB | fresh | `ray-compiled-graph` | static compiled DAG | **1.316** | 4.991 | **2.97** | 9.08% |
| 4 MiB | fresh | `ctwo-pickle-arrays` | direct IPC (Python-only) | 1.843 | **2.223** | 2.12 | 4.26% |
| 4 MiB | fresh | `ctwo-fastdb-stream` | direct IPC stream | 2.422 | 3.749 | 1.61 | 6.98% |
| 4 MiB | fresh | `ray-named-actor` | dynamic named actor | 2.912 | 8.334 | 1.34 | 14.29% |
| 4 MiB | fresh | `ctwo-fastdb-hold` | direct IPC `cc.hold` | 3.767 | 4.222 | 1.04 | 7.79% |
| 4 MiB | fresh | `ctwo-fastdb-unary` | direct IPC unary | 3.898 | 4.300 | 1.00 | 7.92% |
| 4 MiB | fresh | `ctwo-fastdb-relay-stream` | explicit 1-hop `c3 relay` HTTP | 4.015 | 6.101 | 0.97 | 10.88% |
| 32 MiB | published | `ctwo-fastdb-stream` | direct IPC stream | **13.496** | 19.134 | **2.32** | 27.68% |
| 32 MiB | published | `ray-named-actor` | dynamic named actor | 13.760 | **14.472** | 2.27 | 22.45% |
| 32 MiB | published | `ray-compiled-graph` | static compiled DAG | 16.226 | 20.938 | 1.93 | 29.52% |
| 32 MiB | published | `ctwo-pickle-arrays` | direct IPC (Python-only) | 16.342 | 16.852 | 1.91 | 25.21% |
| 32 MiB | published | `ctwo-fastdb-hold` | direct IPC `cc.hold` | 18.796 | 20.867 | 1.66 | 29.45% |
| 32 MiB | published | `ctwo-fastdb-unary` | direct IPC unary | 20.129 | 21.898 | 1.55 | 30.46% |
| 32 MiB | published | `ctwo-fastdb-relay-stream` | explicit 1-hop `c3 relay` HTTP | 22.884 | 31.094 | 1.37 | 38.34% |
| 32 MiB | fresh | `ray-named-actor` | dynamic named actor | **7.432** | **8.553** | **4.21** | 14.61% |
| 32 MiB | fresh | `ray-compiled-graph` | static compiled DAG | 16.589 | 32.893 | 1.88 | 39.68% |
| 32 MiB | fresh | `ctwo-pickle-arrays` | direct IPC (Python-only) | 16.730 | 18.096 | 1.87 | 26.57% |
| 32 MiB | fresh | `ctwo-fastdb-stream` | direct IPC stream | 20.027 | 27.336 | 1.56 | 35.35% |
| 32 MiB | fresh | `ctwo-fastdb-hold` | direct IPC `cc.hold` | 28.616 | 38.512 | 1.09 | 43.51% |
| 32 MiB | fresh | `ctwo-fastdb-unary` | direct IPC unary | 29.128 | 39.783 | 1.07 | 44.31% |
| 32 MiB | fresh | `ctwo-fastdb-relay-stream` | explicit 1-hop `c3 relay` HTTP | 30.993 | 41.409 | 1.01 | 45.30% |

*Semantic note on Ray Compiled Graphs:* `ray-compiled-graph` compiles a static actor DAG (`~48 ms` compile cost) and cannot dynamically register/unregister forked routes or cancel open server streams at runtime; C-Two's direct IPC and relay arms support full runtime route dynamics and stream cancellation.

## Suite 3: Multi-Subscriber Fan-Out Amplification (`R = 1, 2, 4` Concurrent Processes)

`R` separate client processes subscribe simultaneously to the same published frame stream (`16` frames/reader at 4 MiB; `8` frames/reader at 32 MiB) via current 1-to-1 `@cc.stream`.

| Transport | Frame Size | Readers (`R`) | Per-Reader P50 / P95 (ms) | Per-Reader FPS | Aggregate Moved (GiB) | Server Peak RSS (MiB) | Max Client Peak RSS (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct_ipc` | 4 MiB | 1 | 1.356 / 2.934 | 714.4 | 0.062 | 87.0 | 138.7 |
| `direct_ipc` | 4 MiB | 2 | 1.585 / 4.011 | 517.0 | 0.125 | 94.7 | 143.0 |
| `direct_ipc` | 4 MiB | 4 | 3.243 / 12.150 | 236.6 | 0.250 (4×) | 111.2 | 151.3 |
| `explicit_relay` | 4 MiB | 1 | 2.688 / 5.217 | 347.1 | 0.062 | 88.2 | 137.6 |
| `explicit_relay` | 4 MiB | 2 | 2.850 / 8.100 | 305.7 | 0.125 | 96.1 | 138.2 |
| `explicit_relay` | 4 MiB | 4 | 5.532 / 12.795 | 165.0 | 0.250 (4×) | 112.3 | 138.2 |
| `direct_ipc` | 32 MiB | 1 | 20.179 / 38.463 | 42.7 | 0.250 | 310.8 | 470.7 |
| `direct_ipc` | 32 MiB | 2 | 33.647 / 83.132 | 26.1 | 0.500 (2×) | 374.7 | 470.6 |
| `direct_ipc` | 32 MiB | 4 | 33.352 / 54.356 | 28.7 | 1.000 (4×) | 502.9 | 470.9 |
| `explicit_relay` | 32 MiB | 1 | 20.101 / 28.888 | 45.0 | 0.250 | 311.6 | 441.4 |
| `explicit_relay` | 32 MiB | 2 | 25.778 / 40.712 | 36.9 | 0.500 (2×) | 375.9 | 442.8 |
| `explicit_relay` | 32 MiB | 4 | 47.258 / 92.736 | 20.8 | 1.000 (4×) | 504.1 | 442.3 |

## Suite 4: Structured Coordinates Replacement Baseline (`1K`, `100K`, `1M` Rows)

Schema: `{row_id: u32, x: f64, y: f64, z: f64, name: 50,000 rotating strings}`, full `sum(x + y + z)` plus string verification, identical Python 3.13.3 runtime.

| Rows (`N`) | Arm | RPC P50 (ms) | Extract P50 (ms) | Reduce P50 (ms) | Total P50 (ms) | Total P95 (ms) | Speedup vs `pickle-arrays` / `ray-actor` |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 1,000 | `ctwo-fastdb-packed` | 0.564 | 0.063 | 0.014 | 0.718 | 0.979 | 0.41× / 1.00× |
| 1,000 | `ctwo-pickle-arrays` | 0.277 | 0.000 | 0.007 | 0.292 | 0.351 | — |
| 1,000 | `ray-named-actor-arrays` | 0.684 | 0.000 | 0.013 | 0.720 | 1.036 | — |
| 100,000 | `ctwo-fastdb-packed` | 14.151 | 0.151 | 0.088 | 14.473 | 14.798 | 0.67× / 0.45× |
| 100,000 | `ctwo-pickle-arrays` | 8.939 | 0.000 | 0.093 | 9.704 | 10.148 | — |
| 100,000 | `ray-named-actor-arrays` | 5.514 | 0.000 | 0.287 | 6.473 | 6.728 | — |
| 1,000,000 | `ctwo-fastdb-packed` | 26.142 | 0.661 | 0.612 | **27.568** | **28.196** | **5.86× / 2.10×** |
| 1,000,000 | `ctwo-pickle-arrays` | 153.214 | 0.001 | 0.672 | 161.596 | 166.680 | — |
| 1,000,000 | `ray-named-actor-arrays` | 47.213 | 0.001 | 3.181 | 57.795 | 71.395 | — |

*Why `100K` crosses over before `1M`:* `KOSTYA_SPEC` stores the 50,000 unique strings once in a `many str` entry (`name_table`), incurring a ~10 ms fixed UTF-8 table validation cost inside `open_copy` at both `100K` and `1M`. At `100K`, pickle and Ray allocate 100K Python strings faster than that fixed validation (~6.5–9.7 ms); at `1M`, allocating 1,000,000 Python strings dominates pickle (161.6 ms) and Ray (57.8 ms, P95 71.4 ms), while `ctwo-fastdb-packed` scales cleanly to 27.6 ms (P95 28.2 ms).

## Reproduction Commands

```bash
# 1. Guarded public-API smoke test (C-Two only, no Ray required)
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest \
  sdk/python/tests/integration/test_portable_payload_benchmark.py -v

# 2. Full 5-suite headless benchmark including Ray 2.56 & release c3 relay
export CARGO_TARGET_DIR="$PWD/core/target"
cargo build --release --manifest-path cli/Cargo.toml
uv sync --group benchmark
RAY_ENABLE_UV_RUN_RUNTIME_ENV=0 C2_RELAY_ANCHOR_ADDRESS= \
  .venv/bin/python sdk/python/benchmarks/portable_payload_benchmark.py \
  --suite all --include-ray \
  --c3-bin "$CARGO_TARGET_DIR/release/c3" \
  --output docs/acceptance/evidence/2026-11-20-portable-payload-benchmark.json
```
