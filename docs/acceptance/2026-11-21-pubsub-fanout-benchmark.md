# Resource-Scoped Publish/Subscribe vs Server Stream Fan-Out Benchmark (Planned 0.12.0)

**Date:** 2026-11-21  
**Scope:** Reproducible headless, same-commit paired evaluation comparing C-Two's new 1-to-N `@cc.topic` publish/subscribe primitive ([ADR 0005](../decisions/0005-publish-subscribe.md), IPC 17, SHM 4 single-write multi-loan, two-tier `c3 relay` fan-out) against 1-to-1 `@cc.stream` fan-out (`fanout` baseline from [2026-11-20-portable-payload-benchmark.md](2026-11-20-portable-payload-benchmark.md)) across `R = 1, 2, 4` concurrent independent client processes at `4 MiB` (16 frames/reader) and `32 MiB` (8 frames/reader).

- **Driver:** [portable_payload_benchmark.py](../../sdk/python/benchmarks/portable_payload_benchmark.py) (`--suite fanout` and `--suite pubsub_fanout`, public APIs only, guarded by [test_portable_payload_benchmark.py](../../sdk/python/tests/integration/test_portable_payload_benchmark.py))
- **Frozen JSON evidence:** [2026-11-21-pubsub-fanout-benchmark.json](evidence/2026-11-21-pubsub-fanout-benchmark.json)

## Environment & Configuration

| Item | Value |
| --- | --- |
| OS / Arch / CPUs | Darwin 27.2.0, `arm64`, 10 logical CPUs, headless |
| Python | CPython 3.13.3 (`python_gil_enabled = true`) |
| Rust / C-Two / CLI / FastDB / NumPy | Rust 1.99.1, `c-two` source candidate (package version `0.11.0`, release native extension), `c3` source candidate (`0.7.0`, release static build), `fastdb4py 0.2.4`, `numpy 2.4.4` |
| Role memory budget | `1 GiB` per role (`client`, `server`, `c3 relay`) |
| Source commit | `a84211d0` |

## Key Findings

1. **Direct IPC (`Host → R` via shared sealed SHM 4, `32 MiB`, 8 frames):**
   - At `R = 4` concurrent readers (`1.00 GiB` aggregate moved), `@cc.topic` lowers per-reader P50 from **20.51 ms → 14.46 ms** (**-29.5%**), lifts per-reader effective rate from **43.80 → 59.80 fps** (**+36.5%**), raises aggregate throughput from **5.48 → 7.48 GiB/s**, and keeps the publisher decoupled (**63.0 ms** total emit time for all 8 × 32 MiB frames).
   - Host role pool peak (`server_pool_peak_mib`) stays **strictly invariant at 256.13 MiB across `R = 1, 2, 4`** (one sealed SHM block per published frame in the `capacity = 9` burst ring), whereas `@cc.stream` scales linearly **32.02 → 64.03 → 128.06 MiB** (`1× → 2× → 4×`).
2. **Single-hop `c3 relay` (`Host → A → R`, `32 MiB`, 8 frames):**
   - At `R = 4`, `@cc.topic` lowers per-reader P50 from **28.65 ms → 18.29 ms** (**-36.1%**) and lifts aggregate throughput from **4.06 → 5.50 GiB/s** (**+35.4%**, **44.01 vs 32.50 fps**), while Relay A reads each frame once over IPC (`relay_a_peak_rss_mib` stays bounded around **186.5–205.1 MiB** across `R = 1, 2, 4`).
   - At `4 MiB, R = 4`, `@cc.topic` lowers P50 from **3.38 ms → 2.39 ms** (**-29.2%**) and lifts aggregate throughput from **3.80 → 4.48 GiB/s** (**+17.9%**).
3. **Two-tier cross-relay fan-out (`Host → A → B → R`, isolated IPC roots):**
   - Even traversing **two HTTP relay hops**, `dual_relay` `@cc.topic` at `32 MiB, R = 4` achieves **37.99 fps / 4.75 GiB/s** (**21.36 ms P50**) — still **+16.9% faster than single-hop `@cc.stream`** (**32.50 fps / 4.06 GiB/s**).
   - Source Relay A's peak RSS stays flat across `R = 1, 2, 4` (**187.9 / 185.6 / 195.0 MiB** at 32 MiB; **113.9 / 118.1 / 122.3 MiB** at 4 MiB) because Relay B maintains exactly one upstream HTTP topic stream to Relay A and performs the `R×` fan-out locally (`relay_b_peak_rss_mib` = **110–135 MiB** at 32 MiB, **34–46 MiB** at 4 MiB).
4. **Root-cause optimization caught during benchmarking (`a84211d0`):**
   - Initial measurement exposed a **3×–5× slowdown** on relay `@cc.topic` (`6.05 ms` at 4 MiB / `46.02 ms` at 32 MiB, `R = 1`) caused by a hardcoded `4096`-byte downstream staging window in `core/transport/c2-http/src/relay/topic_hub.rs`, which shattered every 32 MiB frame into 8,193 serial HTTP chunks. Aligning the topic hub send window with `router.rs`'s `128 KiB` `CHUNK_SIZE` cap (while preserving fair-share shrinking under tight budgets) immediately delivered a **3.0×–5.4×** relay speedup.
5. **Measurement methodology notes (transparent accounting):**
   - *Ring depth vs `server_pool_peak_mib`:* To verify zero dropped frames (`on_lag='error'`, `dropped_total = 0`) under an unpaced burst matching `stream_published`, `pubsub_fanout` sets `topic_capacity = frames + 1` (`17` at 4 MiB, `9` at 32 MiB). All 8 × 32 MiB frames therefore coexist briefly in the ring (`256.13 MiB`) at `R = 1, 2, 4`; paced steady-state workloads with default `topic_capacity = 1` retain only the latest frame regardless of `R`.
   - *Warmup RSS vs pool peak:* Each reader process executes one warmup `snapshot_published()` unary call before the barrier; `fastdb4py 0.2.4`'s `binary_bytes()` cyclic GC retention (documented in the 0.11.0 benchmark) adds ~32 MiB/reader to Host process RSS (`536 → 569 → 633 MiB`) even while `server_pool_peak_mib` remains strictly flat at `256.13 MiB`.
   - *First-frame P95:* `pubsub_fanout` starts each subscriber's clock at barrier release prior to `emit_burst()`, so `frame_ms[0]` includes trigger RPC + cold pipeline fill across hops and determines the 8-frame P95, whereas steady-state per-frame P50 and total wall elapsed time (`max_reader_elapsed_s`) drop by **27%–51%**.

---

## Paired Results: `32 MiB` (`8` Frames/Reader, `33,555,032 B` Encoded)

All rows verified exact sequence continuity, `dropped_total = 0` (for `@cc.topic`), and full `float64` NumPy sum checksums on every delivered frame.

| Transport | Primitive | `R` | Publish (ms) | Elapsed (s) | Per-Reader P50 / P95 (ms) | Per-Reader FPS | Agg GiB/s | Server Pool Peak (MiB) | Server Peak RSS (MiB) | Relay A / B Peak RSS (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct_ipc` | `@cc.stream` | 1 | — | 0.0981 | 9.80 / 20.25 | 81.54 | 2.55 | 32.02 | 311.8 | — |
| `direct_ipc` | `@cc.topic` | 1 | 40.6 | **0.0685** | **5.71** / 22.66 | **116.81** | **3.65** | 256.13 | 536.1 | — |
| `direct_ipc` | `@cc.stream` | 2 | — | 0.1258 | 14.30 / 23.70 | 63.61 | 3.98 | 64.03 | 376.0 | — |
| `direct_ipc` | `@cc.topic` | 2 | 45.3 | **0.0897** | **9.37** / 26.61 | **89.23** | **5.58** | **256.13** | 568.6 | — |
| `direct_ipc` | `@cc.stream` | 4 | — | 0.1826 | 20.51 / 31.97 | 43.80 | 5.48 | 128.06 | 504.3 | — |
| `direct_ipc` | `@cc.topic` | 4 | 63.0 | **0.1338** | **14.46** / 34.64 | **59.80** | **7.48** | **256.13** | 633.0 | — |
| `explicit_relay` | `@cc.stream` | 1 | — | 0.1582 | 17.10 / 28.76 | 50.56 | 1.58 | 32.02 | 313.3 | — |
| `explicit_relay` | `@cc.topic` | 1 | 45.3 | **0.0961** | **8.46** / 38.71 | **83.26** | **2.60** | 256.13 | 537.3 | 186.5 / — |
| `explicit_relay` | `@cc.stream` | 2 | — | 0.1830 | 20.91 / 36.93 | 43.71 | 2.73 | 64.03 | 377.3 | — |
| `explicit_relay` | `@cc.topic` | 2 | 48.6 | **0.1264** | **11.12** / 51.13 | **63.28** | **3.96** | **256.13** | 570.2 | 205.1 / — |
| `explicit_relay` | `@cc.stream` | 4 | — | 0.2462 | 28.65 / 45.13 | 32.50 | 4.06 | 128.06 | 505.6 | — |
| `explicit_relay` | `@cc.topic` | 4 | 55.1 | **0.1818** | **18.29** / 59.05 | **44.01** | **5.50** | **256.13** | 633.7 | 191.7 / — |
| `dual_relay` | `@cc.topic` | 1 | 45.9 | 0.1287 | 9.17 / 62.14 | 62.15 | 1.94 | 256.13 | 538.1 | 187.9 / 134.7 |
| `dual_relay` | `@cc.topic` | 2 | 45.9 | 0.1369 | 12.48 / 58.23 | 58.42 | 3.65 | **256.13** | 569.5 | 185.6 / 110.3 |
| `dual_relay` | `@cc.topic` | 4 | 50.6 | **0.2106** | **21.36** / 69.25 | **37.99** | **4.75** | **256.13** | 633.9 | 195.0 / 120.6 |

## Paired Results: `4 MiB` (`16` Frames/Reader, `4,194,904 B` Encoded)

| Transport | Primitive | `R` | Publish (ms) | Elapsed (s) | Per-Reader P50 / P95 (ms) | Per-Reader FPS | Agg GiB/s | Server Pool Peak (MiB) | Server Peak RSS (MiB) | Relay A / B Peak RSS (MiB) |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct_ipc` | `@cc.stream` | 1 | — | 0.0153 | 0.91 / 2.53 | 1047.65 | 4.09 | 16.00 | 88.0 | — |
| `direct_ipc` | `@cc.topic` | 1 | 11.3 | 0.0183 | 1.09 / 3.37 | 872.00 | 3.41 | 128.00 | 148.6 | — |
| `direct_ipc` | `@cc.stream` | 2 | — | 0.0240 | 1.29 / 3.87 | 667.90 | 5.22 | 16.00 | 96.2 | — |
| `direct_ipc` | `@cc.topic` | 2 | 12.0 | **0.0233** | 1.34 / 4.39 | **685.35** | **5.35** | **128.00** | 152.8 | — |
| `direct_ipc` | `@cc.stream` | 4 | — | 0.0363 | 2.00 / 4.96 | 441.31 | 6.90 | 32.00 | 112.8 | — |
| `direct_ipc` | `@cc.topic` | 4 | 17.3 | **0.0324** | **1.68** / 5.18 | **493.33** | **7.71** | **128.00** | 161.3 | — |
| `explicit_relay` | `@cc.stream` | 1 | — | 0.0321 | 1.90 / 4.20 | 499.01 | 1.95 | 16.00 | 89.6 | — |
| `explicit_relay` | `@cc.topic` | 1 | 12.3 | **0.0250** | **1.27** / 5.86 | **639.46** | **2.50** | 128.00 | 150.4 | 111.1 / — |
| `explicit_relay` | `@cc.stream` | 2 | — | 0.0454 | 2.48 / 8.14 | 352.09 | 2.75 | 16.00 | 98.2 | — |
| `explicit_relay` | `@cc.topic` | 2 | 14.3 | **0.0427** | **1.99** / 7.80 | **374.76** | **2.93** | **128.00** | 153.5 | 119.3 / — |
| `explicit_relay` | `@cc.stream` | 4 | — | 0.0659 | 3.38 / 12.33 | 242.95 | 3.80 | 32.00 | 113.6 | — |
| `explicit_relay` | `@cc.topic` | 4 | 13.9 | **0.0559** | **2.39** / 10.61 | **286.44** | **4.48** | **128.00** | 161.8 | 127.6 / — |
| `dual_relay` | `@cc.topic` | 1 | 12.9 | 0.0390 | 1.32 / 12.63 | 410.73 | 1.60 | 128.00 | 149.6 | 113.9 / 33.7 |
| `dual_relay` | `@cc.topic` | 2 | 11.9 | 0.0501 | 1.78 / 15.70 | 319.19 | 2.49 | **128.00** | 154.0 | 118.1 / 40.5 |
| `dual_relay` | `@cc.topic` | 4 | 12.3 | 0.0697 | 2.72 / 22.34 | 229.53 | 3.59 | **128.00** | 161.9 | 122.3 / 45.6 |

## Reproduction Commands

```bash
export CARGO_TARGET_DIR="$PWD/core/target"
cargo build --release --manifest-path cli/Cargo.toml

# 1. Guarded smoke test including direct IPC, single-relay and dual-relay topics
C2_RELAY_ANCHOR_ADDRESS= CTWO_TEST_C3_BIN="$CARGO_TARGET_DIR/release/c3" \
  uv run --no-sync pytest sdk/python/tests/integration/test_portable_payload_benchmark.py -v

# 2. Paired full fan-out runs (4 MiB x 16 frames, 32 MiB x 8 frames, R = 1, 2, 4)
C2_RELAY_ANCHOR_ADDRESS= NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
  .venv/bin/python sdk/python/benchmarks/portable_payload_benchmark.py \
  --suite fanout --c3-bin "$CARGO_TARGET_DIR/release/c3" --output fanout.json

C2_RELAY_ANCHOR_ADDRESS= NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost \
  .venv/bin/python sdk/python/benchmarks/portable_payload_benchmark.py \
  --suite pubsub_fanout --c3-bin "$CARGO_TARGET_DIR/release/c3" --output pubsub_fanout.json
```
