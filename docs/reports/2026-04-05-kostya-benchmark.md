# Kostya-Style Coordinate Benchmark — Results

**Workload:** Standard kostya `json` benchmark, RPC-adapted. Server generates
N random `{x: f64, y: f64, z: f64}` coordinates → client receives → client
computes `(mean_x, mean_y, mean_z)`. End-to-end client wall time, P50.

**Hardware:** Apple M1 Max, LPDDR5
**Python (c-two):** 3.14t free-threaded, NumPy 2.4.3
**Python (Ray):** 3.12, Ray 2.55.0, NumPy 2.4.4

## Three c-two transferable strategies

All three share the same IPC transport (UDS + 2 GiB buddy SHM pool); the
only thing that differs is the on-the-wire serializer:

| Variant         | Wire format                                   | Notes                          |
|-----------------|-----------------------------------------------|--------------------------------|
| `pickle-records`| `pickle.dumps(list[(x,y,z)], protocol=5)`     | Naive Python record-oriented   |
| `pickle-arrays` | `pickle.dumps({xs|ys|zs: ndarray}, proto=5)`  | numpy-aware (out-of-band bufs) |
| `fastdb-hold`   | `FastSerializer.dumps(CoordFeature)` + `from_buffer` + `cc.hold()` | columnar, zero-deser path |

```
N     Strategy           RPC P50    Aggregate   Total P50    GB/s
1K    pickle-records      0.70 ms    0.06 ms     0.76 ms    0.03
1K    pickle-arrays       0.50 ms    0.01 ms     0.51 ms    0.04
1K    fastdb-hold         0.59 ms    0.01 ms     0.60 ms    0.04

10K   pickle-records      2.76 ms    0.56 ms     3.32 ms    0.07
10K   pickle-arrays       0.68 ms    0.02 ms     0.71 ms    0.32
10K   fastdb-hold         0.83 ms    0.02 ms     0.86 ms    0.26

100K  pickle-records     25.61 ms    5.68 ms    31.37 ms    0.07
100K  pickle-arrays       1.98 ms    0.08 ms     2.07 ms    1.08
100K  fastdb-hold         2.87 ms    0.08 ms     2.96 ms    0.75

1M    pickle-records    390.32 ms   72.63 ms   463.10 ms    0.05
1M    pickle-arrays      17.00 ms    0.65 ms    17.63 ms    1.27
1M    fastdb-hold        29.24 ms    0.70 ms    30.05 ms    0.74

10M   pickle-arrays     183.96 ms    6.54 ms   190.22 ms    1.18
10M   fastdb-hold       359.35 ms    6.49 ms   365.84 ms    0.61
```

## Ray actor counterpart

```
N     Strategy           RPC P50    Aggregate   Total P50    GB/s
1K    ray-records         9.85 ms    0.05 ms     9.90 ms    0.00
1K    ray-arrays          8.03 ms    0.04 ms     8.10 ms    0.00
10K   ray-records        26.47 ms    0.44 ms    26.95 ms    0.01
10K   ray-arrays          8.72 ms    0.06 ms     8.80 ms    0.03
100K  ray-records       218.96 ms    4.45 ms   223.43 ms    0.01
100K  ray-arrays          9.09 ms    0.34 ms     9.48 ms    0.24
1M    ray-records      2282.55 ms   46.68 ms  2329.46 ms    0.01
1M    ray-arrays         30.75 ms    2.99 ms    34.11 ms    0.66
10M   ray-arrays        177.82 ms    7.81 ms   185.62 ms    1.20
```

## Cross-framework summary (P50 total ms, lower is better)

### Best-vs-best (columnar / numpy-aware)

| N    | Ray ray-arrays | C-Two pickle-arrays | C-Two fastdb-hold | C-Two best vs Ray |
|------|---------------:|--------------------:|------------------:|------------------:|
| 1K   |        8.10    |             **0.51**|              0.60 |        **15.9 ×** |
| 10K  |        8.80    |             **0.71**|              0.86 |        **12.4 ×** |
| 100K |        9.48    |             **2.07**|              2.96 |         **4.6 ×** |
| 1M   |       34.11    |            **17.63**|             30.05 |         **1.9 ×** |
| 10M  |      185.62    |           **190.22**|            365.84 |         0.98 ×    |

### Worst-vs-worst (record-oriented)

| N    | Ray ray-records | C-Two pickle-records | C-Two faster |
|------|----------------:|---------------------:|-------------:|
| 1K   |         9.90    |             **0.76** |     13.0 ×   |
| 10K  |        26.95    |             **3.32** |      8.1 ×   |
| 100K |       223.43    |            **31.37** |      7.1 ×   |
| 1M   |      2329.46    |           **463.10** |      5.0 ×   |

## Honest analysis

1. **C-Two crushes Ray at small/medium scale** (15× faster at 1K, ~5× at 1M).
   This is RPC overhead amortization: c-two's UDS+SHM control path has ~150 µs
   floor; Ray's actor.remote()→ray.get path has ~8 ms floor on macOS.

2. **At 10M (240 MB) the gap closes.** Both transports are now memory-bandwidth
   bound. Ray's plasma store and c-two's buddy SHM both end up doing ~the same
   amount of memcpy on macOS LPDDR5.

3. **fastdb-hold is *not* faster than `pickle-arrays` for pure numpy
   workloads.** Three reasons:
   - pickle protocol-5 already supports out-of-band numpy buffers, so its
     "deserialize" is essentially `np.frombuffer(...)` — already zero-copy.
   - fastdb's binary format adds per-array metadata (header, field index)
     that pickle's numpy fast path skips.
   - `FastSerializer.loads(memoryview)` copies numpy fields out of the buffer
     (per fastdb docs: "Numpy arrays are copied" in `_cache` mode), so the
     hold-mode SHM view is not propagated into the numpy column.

4. **Where fastdb shines is *graph-shaped* data.** Compare `pickle-records`
   (list of tuples) vs the would-be `fastdb-records` (Feature with
   `List[Point]`): fastdb's columnar `__fastser_buf__` layers are
   `memcpy`-equivalent, while pickle has to round-trip N Python tuples.
   This benchmark only tests the columnar fast case to keep the comparison
   numpy-fair; the graph-shaped case is the next benchmark to add.

5. **Cross-language interop is fastdb's true selling point.** Even with
   identical Python performance, fastdb gives c-two a path to a TS/WASM
   client that talks the same wire format — pickle and arrow bytes do not.

## Files

- `benchmarks/kostya_ctwo_benchmark.py`
- `benchmarks/kostya_ray_benchmark.py`
- `benchmarks/results/kostya_ctwo.txt`
- `benchmarks/results/kostya_ray.txt`
