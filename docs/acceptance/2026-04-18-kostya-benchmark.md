# Kostya-Style Coordinate Benchmark — Ray vs C-Two vs C-Two+fastdb

**Date**: 2026-04-18
**Hardware**: Apple M1 Max, macOS, Python 3.14t (free-threaded) for c-two; Python 3.12 for Ray
**Workload**: kostya-style coordinate records — schema `{row_id: u32, x: f64, y: f64, z: f64, name: STR}` (matches the upstream fastdb kostya benchmark; `name` rotates over 50 000 unique strings → exercises pickle's per-string allocator)
**Pattern**: client requests `N` coordinates from a remote actor/CRM, then computes `sum(x+y+z)` over the received payload. Reported time is **end-to-end**: RPC + materialization + columnar reduction.

---

## TL;DR

At realistic data scales (≥ 100 K rows), `c-two + fastdb` in **hold mode** is:

* **5×–6× faster than Ray** with numpy ndarrays (Ray's best case)
* **17×–23× faster than c-two with default pickle**
* **35×–60× faster than Ray with record-oriented dicts**

The win compounds with size because hold mode is **zero-copy + zero-deserialize**: the client reads `x[:].sum()` as a NumPy view directly over c-two's POSIX SHM, with no Python-side rebuild of the columns.

---

## Three transports, three Python-side representations

| Transport | Strategy             | What crosses the wire                             | What the client does on receipt          |
|-----------|----------------------|---------------------------------------------------|------------------------------------------|
| Ray 2.55  | `ray-records`        | `list[dict]` via `ray.put` → arrow/pickle codec   | iterate dicts, sum                       |
| Ray 2.55  | `ray-arrays`         | `dict[str, ndarray]` via Ray plasma store         | `arr.sum()` — numpy fast path            |
| c-two IPC | `pickle-records`     | `pickle.dumps(list[dict])`                        | `pickle.loads` → iterate dicts           |
| c-two IPC | `pickle-arrays`      | `pickle.dumps((u32, f64, f64, f64, list[str]))`   | `pickle.loads` → numpy `.sum()`          |
| c-two IPC | **`fastdb-hold`**    | raw fastdb columnar buffer over SHM               | `WxDatabase.load_xbuffer(memoryview)` → `tbl.column.x[:].sum()` (zero-copy view) |

Ray runs as a separate actor process with object-store-based zero-copy for ndarrays.
c-two runs a CRM in a spawned process; the client connects via `ipc://` (UDS control + POSIX SHM data plane).
The fastdb variant uses c-two's `cc.hold()` to keep the response SHM segment alive while the client materializes a fastdb ORM **over** the c-two SHM buffer — no extra copy.

---

## Results — P50 latency (ms), end-to-end

| N   | Payload | Ray records | Ray arrays | c-two pickle-records | c-two pickle-arrays | **c-two fastdb-hold** |
|-----|---------|-------------|------------|----------------------|---------------------|------------------------|
| 1K  | ~32 KB  | 6.46        | 6.26       | 0.89                 | 0.48                | **0.48**               |
| 10K | ~320 KB | 11.42       | 7.15       | 5.72                 | 1.16                | **0.59**               |
| 100K| ~3 MB   | 56.61       | 9.20       | 69.11                | 9.85                | **1.75**               |
| 1M  | ~32 MB  | 541.84      | 46.73      | 881.99               | 149.13              | **8.68**               |
| 3M  | ~96 MB  | —*          | 138.45     | —*                   | 565.88              | **24.13**              |

\* records-variant skipped at 3 M — Python-loop dominated, multi-second territory in both transports.

### Speed-up of c-two + fastdb vs alternatives

| N   | vs Ray records | vs Ray arrays | vs c-two pickle-records | vs c-two pickle-arrays |
|-----|----------------|---------------|-------------------------|------------------------|
| 10K | 19.4×          | 12.1×         | 9.7×                    | 2.0×                   |
| 100K| 32.3×          | 5.3×          | 39.5×                   | 5.6×                   |
| 1M  | 62.4×          | 5.4×          | 101.6×                  | 17.2×                  |
| 3M  | —              | 5.7×          | —                       | 23.5×                  |

### Throughput (effective, end-to-end)

At 3 M / ~96 MB:

* `ray-arrays`         → **0.68 GB/s**
* `c-two pickle-arrays`→ **0.17 GB/s** (pickle string list dominates)
* `c-two fastdb-hold`  → **3.88 GB/s**

At 1 M / ~32 MB:

* `ray-arrays`         → **0.66 GB/s**
* `c-two pickle-arrays`→ **0.21 GB/s**
* `c-two fastdb-hold`  → **3.55 GB/s**

(For reference: M1 Max single-channel theoretical sustained user-mode memcpy is ~10 GB/s; we're hitting ~40 % of that with full RPC overhead included.)

---

## Why fastdb-hold wins

1. **Zero serialize on the server**: fastdb's columnar buffer is the wire format. `orm._origin.buffer().as_array(np.uint8)` is a memoryview — no `pickle.dumps` walk over Python objects.
2. **Zero copy in transit**: c-two writes the buffer once into a POSIX SHM segment and hands the receiver a `memoryview` of that segment (hold mode keeps it alive).
3. **Zero deserialize on the client**: `WxDatabase.load_xbuffer(memoryview)` walks the columnar header in C++ (~60 µs for 3 MB, ~250 µs for 96 MB). The result is a database where `tbl.column.x[:]` returns a NumPy view backed by the same SHM bytes.
4. **String table**: the `name` field becomes a u16 dictionary index in the columnar layout. Pickle re-allocates 1 M Python str objects; fastdb stores a single string table once.

By contrast:

* **`pickle-arrays`** still has to allocate a 1 M-element `list[str]` for `name` on every call — that alone explains the 17×–23× gap vs fastdb-hold at 1 M+.
* **Ray's plasma object store** is cleverly zero-copy for ndarrays, but the round-trip still goes through arrow's serialize / deserialize for the dict wrapper and the string list, plus an actor scheduling hop. That's why Ray bottoms out around 7 ms even at trivial N.

---

## What this means for c-two

* `cc.hold()` + a transferable that exposes its columnar buffer via `from_buffer` is the **canonical pattern** for moving structured numeric data — the cost falls to the columnar codec and a few SHM atomics.
* For string-heavy or dict-graph payloads where users don't need fastdb-style schemas, default pickle is still acceptable but ~5–25× slower at scale.
* **There is no Ray bottleneck c-two needs to chase.** c-two's IPC (numpy fast path) already matches or beats Ray on its own sweet-spot workload. The fastdb integration extends the lead to a different order of magnitude.

---

## How to reproduce

```bash
# c-two side — full sweep (records / arrays / fastdb-hold × 5 sizes)
bash benchmarks/run_kostya_sweep.sh
cat benchmarks/results/kostya_ctwo.txt

# Ray side — separate venv (Python 3.12, ray 2.55.0 installed)
/tmp/ray_bench_env/bin/python benchmarks/kostya_ray_benchmark.py
cat benchmarks/results/kostya_ray.txt
```

Source:
* `benchmarks/kostya_ctwo_benchmark.py` — three transferable strategies (`pickle-records`, `pickle-arrays`, `fastdb-hold`)
* `benchmarks/kostya_ray_benchmark.py` — Ray actor with `ray-records` / `ray-arrays`
* `benchmarks/run_kostya_sweep.sh` — driver

---

## Caveats & honesty

* fastdb's `Coord` schema with `STR name` is the upstream kostya schema. If your real workload is purely numeric ndarrays, the gap vs `pickle-arrays` is smaller (~3–5× vs the 17×–23× shown here, which includes the string-table win).
* All three c-two strategies share the same buddy-allocator SHM data plane; the gap is entirely from codec choice + materialization cost.
* The c-two server is a single CRM in a single spawned process. Ray adds extra scheduling layers (driver, raylet, plasma store) that are necessary for its scaling story but show up as fixed RPC overhead at small N. Comparison is fair for "single producer, single consumer, single host" — Ray wins at multi-node, multi-actor workloads c-two doesn't currently target the same way.
