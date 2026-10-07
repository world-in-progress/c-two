# IPC memory benchmark harness

Final integration: the complete nine-row matrix passed with required native memory statistics on Windows 2022 and 2025. See [final validation](memory-native-final-validation.md). The development and recovery records below retain their original source boundaries. Runtime copy-byte attribution is not implemented; this harness records latency, throughput, backing/lease observations and RSS.

This report covers `tools/benchmarks/ipc_memory.py`, the end-to-end measurement driver for step 3 of the approved [memory plan](../plans/2026-09-26-memory-policy.md) ("比较四类负载 … 记录首次/稳态延迟、吞吐、复制量和峰值 backing/RSS") and for the still-pending "预算、各层回退和清理的负向测试，以及低负载/大消息基准" item. The work recovers the preserved draft on base `947415ba62eaf488f728bb554b74188295091a1e` and keeps the reviewed harness issues corrected; it changes the harness and this report only. No runtime, native, or production file was edited.

## What the harness guarantees

- One controller process per row. Workers connect to an explicit `ipc://` address, so every measured call uses real IPC and never same-process direct dispatch. `--topology shared` runs one server in the controller; `--topology pairs` gives each worker its own registered server.
- Every result keeps the exact invocation (`argv`, `worker_command`, `--list-rows` table, row `spec`) next to `calls` / `calls_completed`, `connect_ns`, `first_call_ns`, `median_call_ns`, `p95_call_ns` (nearest-rank over measured calls), `rpc_elapsed_ns`, and a derived `throughput_calls_per_s` that excludes import, connect, and payload-build time.
- Memory domains stay separate: OS RSS is a per-process high-water mark (`rss_high_water_bytes`), backing budgets are configuration (`overrides`), and retained transport buffers come from `cc.hold_stats()` (`held`, `after_release`). Nothing sums them, and no field is presented as physical RAM usage.
- Each success-mode run holds one response through `cc.hold(...)`, checks the value, releases it, and fails if `active_holds` is non-zero afterwards. The `held` counts are recorded verbatim from `cc.hold_stats()`; the echo route here is pickle-based and can legitimately report zero transport leases in every storage class while a value is held, so those counts are **not** a held-backing proof for any storage class. A held-backing claim would need a portable payload and runtime-side evidence that a lease of that storage class was actually retained.
- Geometry is scoped explicitly. `configure()` pins the primary request/response buddy pool (`pool_enabled`, `pool_segment_size`, `max_pool_segments=2`) **and** the separate chunk-reassembly buddy pool (`reassembly_segment_size=8 MiB`, `reassembly_max_segments=1`; the native defaults are 64 MiB × 4). `max_pool_segments` does not cap the reassembly pool, and neither cap bounds dedicated SHM mappings, which are per-allocation and limited only by the backing budgets and payload size. `--ipc-overrides` can still override any of these, and the effective values are recorded in each worker's `overrides` field.
- Process hygiene: worker joins are bounded by `--child-timeout`, row joins by `--row-timeout`. Only processes the driver started are signalled, and a matrix row timeout signals the session that row controller leads, but only after `os.getpgid` confirms that child still leads it. Every post-kill drain is bounded and closes its pipes, so a descendant holding an inherited pipe is reported as a drain failure instead of being waited on forever. `child_processes` (pid, exit code, forced-termination flag, drain result), `children_terminated_pids`, `expected_workers`, `spawned_child_count`, and per-process `cleanup` are recorded, and any forced termination is a failure rather than clean shutdown.
- Input bounds are validated before anything spawns: `--workers` is a positive integer ≤ 16 (`MAX_WORKERS`), and `--child-timeout` / `--row-timeout` must be finite positive values ≤ 600 s / 3600 s (`MAX_CHILD_TIMEOUT_S` / `MAX_ROW_TIMEOUT_S`). The same checks run on a `--row-spec` before any row controller or worker is created, so `nan`, `inf`, zero, negative, oversize, and over-count values all exit with a parser error instead of spawning processes.
- Provenance: `native_path`, `native_sha256`, and `native_repo` (repository HEAD plus tracked dirty paths) are read from the native extension that was actually imported. A missing module path or hash is a provenance failure in the worker, in the controller, and in the matrix aggregate; hash agreement is computed only over processes that actually reported a hash, so it can never be satisfied by an empty set. Controller and worker hashes must agree. The report never claims that the imported native was built from the harness's or a worker's Git HEAD.
- Expected-error mode (`--expect-error-substring`): a successful RPC fails validation; a raised error without the substring is reported as the wrong error; and an error raised during **connect** fails validation even when the substring matches and the control ping still answers, because only the call-stage capacity failure is the phenomenon under test. The artifact keeps the required substring/stage, the exact observed `error_type` + `error_message`, the recorded `stage`, `stage_matched`, `matched`, and `control_ping_ok`. After any such error the worker checks the direct IPC control ping and then runs normal client/server cleanup; every failure is appended to `failures` so the artifact still carries latencies, RSS, and cleanup for the failing run.
- `cc.memory_stats` snapshots are optional. When the public API is absent, each snapshot point is recorded as `{"available": false, "reason": ...}`. `--require-memory-stats` enforces every advertised point for the topology in use — in `shared` that is the worker's `before_connect` / `after_calls` / `after_cleanup` plus the controller server's `after_register` / `after_workers` / `after_cleanup`; in `pairs` it is the worker's three points, since each worker owns its server — instead of fabricating zeros. Post-shutdown unavailability can only be exempted through the explicit `--allow-unavailable-stats-after-shutdown` contract, and the exemption is written into the snapshot as a `contract` field; without that flag the after-cleanup points fail the run.
- Matrix completeness is explicit: `matrix.json` carries `complete`, which is true only when every row in the table ran. A successful `--only` subset exits 0 while `complete` stays false and the skipped rows stay listed under `rows_not_selected`. Matrix-level provenance problems are listed under `failures` and force a non-zero exit.

## Fixes applied to the preserved draft

1. `--worker` was parsed but never dispatched, so a spawned "worker" re-entered `run_controller` and recursively spawned another process chain. `main()` now dispatches `--worker` to `run_worker`. This was not theoretical: an unfixed copy of the draft in another recovery workspace ran such a chain; the Host quarantined that copy and stopped it (see "Host state during smoke").
2. Metadata capture no longer raises when distribution metadata is missing, and it records the imported native path/hash plus the owning repository HEAD and tracked dirtiness.
3. A validation failure inside a run is appended to `failures` instead of aborting the artifact, so cleanup, latency, and RSS evidence survive a payload mismatch or a failed connect.
4. The matrix driver starts each row controller as its own session leader, bounds the post-kill pipe drain, and records `rows_not_selected`, so a partial `--only` run can never be mistaken for a full matrix (`complete` is explicit).
5. The row controller now receives `--python`, so every nested process uses the interpreter recorded in the row spec.
6. Expected capacity failures are accepted only at the call stage; a connect-stage error with a matching substring and a live ping is now a reported failure with the exact error text preserved, instead of passing.
7. All post-kill `communicate`/`wait` calls are bounded and close their pipes, and a drain that cannot finish is a recorded failure; only the driver's own processes are signalled.
8. `--workers` and both join timeouts are range-checked before any spawn, on the CLI path and inside `--row-spec`.
9. Missing native path/hash is a provenance failure at worker, controller, and matrix level; hash agreement requires at least one reported hash and no inconsistency.
10. `--require-memory-stats` covers all advertised snapshot points including after cleanup, with the explicit post-shutdown exemption contract.

## Matrix rows

Nine rows cover the four approved workload classes. Each row is a distinct (workload, policy) cell; `--list-rows` prints the same table.

| Row | Workload | Payload / calls | Topology | Config |
| --- | --- | --- | --- | --- |
| `idle_small_rpc_default_buddy` | idle/small RPC | 1 B / 200 | shared, 1 worker | buddy on, 2 MiB segment, 2 primary segments |
| `idle_small_rpc_buddy_disabled` | idle/small RPC | 1 B / 200 | shared, 1 worker | `pool_enabled=false` |
| `burst4_shared_default_buddy` | four workers, one server | 64 KiB / 40 | shared, 4 workers | buddy on |
| `burst4_shared_buddy_disabled` | four workers, one server | 64 KiB / 40 | shared, 4 workers | `pool_enabled=false` |
| `science_payload_pairs_default_buddy` | multi-MiB science payload | 4 MiB / 8 | pairs, 1 worker | buddy on |
| `science_payload_pairs_buddy_disabled` | multi-MiB science payload | 4 MiB / 8 | pairs, 1 worker | `pool_enabled=false` |
| `forced_zero_shm_finite_file` | synthetic budget fallback | 2 MiB / 2 | shared, 1 worker | shm budget 0, file + live 64 MiB, expect success |
| `forced_zero_shm_finite_file_buddy_disabled` | synthetic budget fallback | 2 MiB / 2 | shared, 1 worker | same, `pool_enabled=false` |
| `forced_all_backing_zero_expect_capacity_error` | synthetic budget exhaustion | 2 MiB / 1 | shared, 1 worker | all three cells 0, expect error containing `memory budget cell` |

Every row pins a 2 MiB primary segment with at most 2 primary segments and one 8 MiB reassembly segment, so buddy-backed mapping in either pool stays small on a laptop. That pinning does **not** bound dedicated SHM mappings or file-spill backing; those follow the configured budgets (`shm_backing_budget_bytes`, `file_backing_budget_bytes`, `live_reassembly_budget_bytes`) and the payload sizes, and the forced rows are synthetic budget configurations, never real out-of-memory or full-disk pressure. On a fully wired candidate the two fallback rows must complete with byte-equal payloads while the SHM cell is zero, and the exhaustion row must raise exactly the capacity error, keep the server answering the direct IPC control ping, and still close the client and shut the server down.

## Exact invocation

```bash
REPO=<integrated checkout>
OUT=/tmp/c2-memory-matrix-final
# The read-only smoke venv is used as-is; -B and PYTHONDONTWRITEBYTECODE keep
# the import from writing bytecode into the Host tree.
PYTHONDONTWRITEBYTECODE=1 /tmp/c2-memory-venv/bin/python -B "$REPO/tools/benchmarks/ipc_memory.py" \
  --mode matrix --python /tmp/c2-memory-venv/bin/python \
  --memory-stats --output-dir "$OUT"
```

Add `--require-memory-stats` when the integrated candidate exposes `cc.memory_stats` and the run must fail closed instead of recording unavailable snapshots; the final integrated candidate exposes that API and both Windows full gates require it. Add `--allow-unavailable-stats-after-shutdown` only to adopt the explicit contract that post-shutdown snapshots may be unavailable; the exemption is recorded per snapshot. `--only <substring,...>` runs a subset (`matrix.json` then reports `complete: false`), `--list-rows` prints the row table, and a single measurement uses `--mode single` with `--output`. `--workers` is capped at 16; `--child-timeout` and `--row-timeout` must be finite positive and at most 600 s / 3600 s. Artifacts: `matrix.json` plus one `rows/<name>.json` per row.

Python 3.10 syntax gate (execute-only, no bytecode written):

```bash
/Users/soku/.local/share/uv/python/cpython-3.10-macos-aarch64-none/bin/python3.10 -c \
  "import pathlib; p=pathlib.Path('tools/benchmarks/ipc_memory.py'); compile(p.read_text(), str(p), 'exec')"
/Users/soku/.local/share/uv/python/cpython-3.10-macos-aarch64-none/bin/python3.10 -B tools/benchmarks/ipc_memory.py --list-rows
```

Commands as executed (from the recovery checkout, `<checkout>` below):

```bash
cd <checkout>
SMOKE=/tmp/c2-memory-harness-smoke/final2
VPY=/tmp/c2-memory-venv/bin/python
HM="tools/benchmarks/ipc_memory.py"

# 6-row matrix subset; the three forced-budget rows stay unselected
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --mode matrix --only idle_small,burst4,science \
  --python $VPY --memory-stats --output-dir "$SMOKE/matrix6"

# small shared server, 20 calls of 64 B
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --python $VPY --calls 20 --payload-bytes 64 \
  --output "$SMOKE/single_small.json"

# stats optional, then required, then required with the explicit post-shutdown exemption
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --python $VPY --calls 5 --payload-bytes 64 --memory-stats
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --python $VPY --calls 5 --payload-bytes 64 --require-memory-stats
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --python $VPY --calls 5 --payload-bytes 64 \
  --require-memory-stats --allow-unavailable-stats-after-shutdown

# bounded row join (forced termination must be reported, not hidden)
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --mode matrix --only idle_small_rpc_default_buddy \
  --row-timeout 0.35 --python $VPY --output-dir "$SMOKE/matrix_timeout"

# input validation: all of these must exit 2 before spawning anything
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --workers 17 --mode matrix --output-dir "$SMOKE/invalid"
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --child-timeout nan --mode matrix --output-dir "$SMOKE/invalid"
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --row-timeout 0 --mode matrix --output-dir "$SMOKE/invalid"
PYTHONDONTWRITEBYTECODE=1 $VPY -B "$HM" --row-spec '<bad spec: workers 99 / child_timeout -5 / missing keys>'
```

Two additional negative checks were driven from a scratch copy / scratch scripts under `$SMOKE` (the checked-in harness was not modified for them): a connect-stage error against a live server that lacks the `echo` route (`stage` server module in `$SMOKE/../stage/`), a fake worker interpreter that leaves a descendant holding the inherited pipes, a scratch copy of the harness with `runtime_provenance` forced to return nothing (missing-provenance), and an in-process run with the row table limited to the six unforced rows (complete-flag-true). The smoke runs needed no recursion-depth launcher: `--worker` dispatches to `run_worker`, the worker-count checks confirm exactly `--workers` children per controller, and the workspace's harness process pattern was checked after the runs — the final check was empty.

## Executed evidence

All commands above ran in the recovery checkout at input commit `57b3a21d072e1c37d55fbf4901eaa6aeca9c8e50`, using the uncommitted working-tree copy of `tools/benchmarks/ipc_memory.py` that this recovery delivers (the harness-repo HEAD recorded inside those artifacts is therefore still `57b3a21d...`), with `/tmp/c2-memory-venv/bin/python` (Python 3.13.3) as driver and worker interpreter. The imported module was `/Users/soku/.codex/worktrees/c2-memory-policy/c-two/sdk/python/src/c_two/__init__.py` (editable `c_two.pth`), native `_native.cpython-313-darwin.so` SHA-256 `a9482acffbb4912cc8bf11553731e58d7f15a67c5ef3fa38916c6ddf0bb3230f`, from repository HEAD `365cf9486c059058205b860aee59e782f0e01c96` with `tracked_dirty: true` (`docs/plans/2026-09-26-memory-policy.md`). Package metadata reported `c-two 0.6.0`, `fastdb4py 0.2.1`. The imported native is therefore **not** proven to be a build of this worktree's HEAD or of `947415b`; it is the accepted Host build plus the pressure/stats slice, and it is dirty.

| Case | Invocation (abbreviated) | Result |
| --- | --- | --- |
| 6-row matrix subset | `--mode matrix --only idle_small,burst4,science --memory-stats` | exit 0, `passed 6 / failed 0`, `complete: false`, `rows_not_selected: 3`, native-hash agreement true, 2.39 s wall |
| small shared server | `--mode single --calls 20 --payload-bytes 64` | exit 0; 20/20 calls, first 346 µs, median 66 µs, p95 101 µs; 1 child, exit 0; `server_shutdown` `ok`; no leftover processes |
| connect-stage error with matching substring and live ping | live server has only route `other`; `--worker --expect-error-substring 'route not found'` | exit 1; `matched: true`, `control_ping_ok: true`, `stage: connect`, `stage_matched: false`; failure states the call-stage requirement; `shutdown` `ok`. The same invocation against the pre-fix copy exited 0 with `failures: []` (baseline captured before the fix) |
| wrong error raised | `--worker --address ipc:///tmp/.../absent-server.sock --expect-error-substring 'memory budget cell'` | exit 1; `matched: false`, `stage: connect`, `control_ping_ok: false`, `shutdown` `ok` |
| optional stats unavailable | `--memory-stats --calls 5` | exit 0; all six snapshot points report `available: false` with the missing-API reason; no zeros written |
| required stats missing | `--require-memory-stats --calls 5` | exit 1; failures at all six points, including worker and server `after_cleanup`; cleanup still `ok` |
| required stats with explicit post-shutdown exemption | `--require-memory-stats --allow-unavailable-stats-after-shutdown` | exit 1; only `before_connect`/`after_calls`/`after_register`/`after_workers` fail; both `after_cleanup` snapshots carry the `contract` string; no silent pass |
| required stats, `pairs` topology | `--topology pairs --require-memory-stats --calls 2 --payload-bytes 64` | exit 1; only the worker's three advertised points exist and all three fail; the controller has no server stats in this topology |
| bounded row timeout | `--mode matrix --only idle_small_rpc_default_buddy --row-timeout 0.35` | row `ok false`, `terminated_by_matrix true`, `exit_code 1`, failure text states forced termination; clean drain; zero leftover harness processes |
| inherited-pipe drain | scratch fake worker interpreter leaves `sleep 45` holding the pipes; `--child-timeout 2` | exit 1 after 32 s (2 s join + 30 s bounded drain); `child_processes[0].drain` reports that the pipes stayed open and were closed; the run never waits indefinitely |
| invalid arguments | `--workers 17`, `--workers 0`, `--child-timeout -1|nan|inf|601`, `--row-timeout 0|1e9`, bad `--row-spec` (workers 99, child_timeout −5, missing keys) | exit 2 for each with the exact parser error; nothing spawned |
| missing native path/hash | scratch copy with `runtime_provenance` forced to return nothing; `--mode matrix --only idle_small_rpc_default_buddy` | exit 1; worker failure, controller failure, matrix `rows_missing_provenance: [row]`, `agreement: false`, and a matrix-level failure — never an empty-set agreement |
| complete flag true | in-process run with the row table limited to the six unforced rows | exit 0, `complete: true`, `rows_not_selected: 0`, `agreement: true` |
| worker dispatch bound | `burst4_shared_default_buddy` inside the subset run | `spawned_child_count: 4` for `--workers 4`; four child records, each exit 0, no forced terminations, no leftovers |
| Python 3.10 | `compile(...)` and `python3.10 -B ipc_memory.py --list-rows` | syntax OK, row table printed |

Per-row figures from the 6-row matrix subset (harness-validation values only; no improvement claim is made). For the four-worker rows the first/median/p95 columns are the worst worker and throughput is the sum across workers:

| Row | calls | connect | first | median | p95 | throughput |
| --- | --- | --- | --- | --- | --- | --- |
| `idle_small_rpc_default_buddy` | 200/200 | 738 µs | 380 µs | 52 µs | 110 µs | 15 272 calls/s |
| `idle_small_rpc_buddy_disabled` | 200/200 | 776 µs | 368 µs | 59 µs | 91 µs | 15 189 calls/s |
| `burst4_shared_default_buddy` | 4 × 40/40 | 795 µs | 517 µs | 189 µs | 298 µs | 21 691 calls/s |
| `burst4_shared_buddy_disabled` | 4 × 40/40 | 794 µs | 572 µs | 289 µs | 549 µs | 13 301 calls/s |
| `science_payload_pairs_default_buddy` | 8/8 | 1 938 µs | 4 987 µs | 3 873 µs | 4 987 µs | 253 calls/s |
| `science_payload_pairs_buddy_disabled` | 8/8 | 2 131 µs | 4 727 µs | 3 838 µs | 4 727 µs | 252 calls/s |

All six rows exited 0 with per-worker exit code 0, no `failures`, and `server_shutdown` `ok`. The successful single run's artifact records the pinned geometry (`max_pool_segments: 2`, `reassembly_segment_size: 8388608`, `reassembly_max_segments: 1`) and a pickle-based hold with zero transport leases in every storage class, which is exactly why the hold is recorded rather than presented as backing proof.

## Unexecuted

1. `forced_zero_shm_finite_file`, `forced_zero_shm_finite_file_buddy_disabled`, and `forced_all_backing_zero_expect_capacity_error` were not run. Runtime budget wiring is still unfinished: the current candidate projects the three finite limits as configuration but does not enforce the SHM cell at allocation time (a probe with `shm_backing_budget_bytes: 0` and a 512 KiB payload succeeded), and live reassembly charging is still being implemented per the plan's implementation tracking. These rows encode post-integration expectations and their checks must not be weakened to make a partially wired candidate pass; the Host runs them after final runtime integration.
2. The control-ping-success assertion after a real capacity error belongs to the unexecuted exhaustion row. The ping *failure* branch was executed against an address with no server, and the stage gate was executed against a live server whose ping succeeded while connect failed.
3. `--require-memory-stats` success path needs an integrated `cc.memory_stats`; only the fail-closed missing-API paths (with and without the post-shutdown exemption) are executed here.
4. The full nine-row matrix is unexecuted because of item 1; `complete: true` was exercised with the six-row unforced table only.
5. Not covered by design or by this recovery: Windows, relay/HTTP paths, same-process direct dispatch, a full Python 3.10 run (the read-only venv and the compiled native are 3.13), and dedicated-mapping or file-spill capacity measurements. The Python 3.10 gate here is syntax and CLI only.

## Host state during smoke

The runaway chain from the unfixed copy of the draft was active during the earlier recovery session; it is no longer running. The Host quarantined only that unaccepted script (backup `/tmp/c2-memory-runaway-ipc_memory.original.py`), force-terminated the exact owned processes, verified zero remaining, and reclaimed 1341 owned orphan sockets through native c2-local lock/liveness/inode checks; evidence is in `/tmp/c2-memory-runaway-cleanup.json` and `/tmp/c2-memory-orphan-cleanup/results.json`. That workspace was not executed or altered by this recovery.

Every figure in this report, including the table above, is harness-validation evidence only, captured while the machine was still shared with other work. No smoke latency here is final performance evidence, and no baseline comparison is implied; the Host's post-integration matrix run is the acceptance gate.

## Host corrections and verification

The Host corrected the final review findings: hidden row specifications require exact JSON booleans and a null or nonempty expected-error string; matrix rows require both a boolean success record and process exit 0; malformed producer JSON fails closed. Matrix identity comparison now includes the shared controller server as well as all workers. These changes are covered by 14 repository regression tests, including a producer that emits success JSON but exits 1. The tests are included in regular CI and the Windows harness gate.

The Host ran a separate guarded smoke with one worker, five 64-byte RPCs, and a Python launcher that refuses unexpected recursive depth. It exited 0 with one spawned/reaped child, no forced termination, no failures, and server shutdown reported complete. Python 3.10 syntax passed. Evidence: `/tmp/c2-memory-guarded-bench/single.json` and `/tmp/c2-memory-guarded-bench/repo-tests.log`. Full runtime-budget measurements remain a later combined gate.
