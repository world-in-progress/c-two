# TypeScript response reader: lazy SHM peer open after an empty handshake

Baseline: `c66617019a63f53473a71f11f88dbd4382f4e2e6`. Scope: the TypeScript adapter gap exposed by the lazy-SHM work; no wire version, contract, routing, or FastDB boundary changes.

The strict 12-row TypeScript real-call matrix failed two rows, `record-v1__relay-aware-local-ipc` and `object-graph-v1__relay-aware-local-ipc`, with `C2IpcTransportError: C-Two c2-mem-ffi response SHM reader requires segmentSize when the server handshake does not advertise the response buddy segment` raised from `resolveC2MemFfiResponseSegmentSize`. Both failing rows use a Python host (`_host_language` maps `relay-aware-local-ipc` to the Python host), and `configure_python_runtime` sets `shm_threshold=1`, so every non-empty reply is a buddy SHM reply on the direct IPC path. The Rust host rows (`direct-ipc`, `relay-aware-http`) keep the default threshold and their small payloads stay inline, which is why only the two Python-host IPC rows exercised the failing adapter branch.

The handshake segment list is a descriptive snapshot, not a publication of everything the server will ever create. The Python server builds its response pool lazily (`pool_prewarm_segments` default 0) and retires idle buddy segments down to `pool_min_retained_segments` default 0, so the direct IPC handshake for these rows legitimately advertised no response buddy segment even though the reply that arrived right after was buddy-backed. Core already treats this as normal: `c2-ipc`'s `ServerPoolState` opens the exact peer backing by prefix/index/generation with `2 x min_block_size` as the minimum legal validation size and derives the real geometry from the mapped backing, and `c2-mem-ffi`'s native response pool uses the same `MemPool::ensure_peer_segment(prefix, index, generation, min_size)` seam. Only the generated TypeScript adapter still demanded the server's segment size.

The fix is confined to the bootstrap value the TypeScript reader hands to the peer pool factory. `resolveC2MemFfiResponseBootstrapSegmentSize` now resolves in this order: an explicit `segmentSize` option, then the handshake snapshot for the referencing index, then the minimum legal peer pool geometry (`2 x minBlockSize`, mirroring `ServerPoolState::MIN_PEER_BUDDY_SEGMENT_SIZE`). The removed `minBlockSize` fallback for dedicated blocks was also not a legal pool configuration (`segment_size >= 2 x min_block_size`), so dedicated-only responses now get the same legal floor. The name change makes the distinction explicit: this value is an open-side capacity floor used for admission, and c2-mem remains authoritative for backing capacity, byte range, and generation. No prewarm was forced, no 256 MiB default was hardcoded, the client never guesses the server's segment size, SHM is not disabled, and the handshake snapshot is not reused after lazy retirement.

The c2-mem-ffi TypeScript binding documents the same projection boundary: request pools create backings with the configured `segmentSize`, while response pools open peer backings and treat it only as a bootstrap floor whose real geometry comes from the mapped segment.

## Regressions

Native peer regressions (`core/foundation/c2-mem-ffi/src/lib.rs`) run against a real lazy owner `MemPool` that pre-creates nothing and has a different `segment_size` (1 MiB) than the peer bootstrap floor (8 KiB):

- `response_pool_bootstrap_floor_reads_and_releases_late_server_segment` proves a late-created backing is opened, read, and released through the peer with the minimum legal bootstrap capacity, and that the release reaches the owner allocator.
- `response_pool_rejects_unbacked_generation_before_late_open` proves an unbacked generation is rejected (`PoolError`) instead of fabricated, that the rejected call does not poison the real coordinates, and that release cannot profit from the failure.
- `response_pool_reopens_late_generation_after_owner_retires_backing` retires the idle owner backing, recreates the slot with a fresh generation, and proves the peer pool lazy-opens the new generation.
- `response_pool_rejects_out_of_range_bootstrap_geometry` proves a coordinate beyond the real backing data capacity is `InvalidArgument` while the true coordinates still read and release.

Generated TypeScript regression (`core/foundation/c2-codegen/tests/typescript_transport_contract.rs`) compiles the generated module and drives `createC2MemFfiNativeResponseShmReader` under Node with a recording pool factory:

- `generated_c2_mem_ffi_reader_bootstraps_unadvertised_lazy_segments` proves an empty advertised list resolves to the bootstrap floor instead of throwing, one pool is reused per owner prefix, a second prefix opens its own pool, an advertised snapshot seeds capacity, dedicated blocks only need the legal floor, and an explicit configuration wins.

Real native binding regression (`core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-node-loader.test.mjs`) uses the packaged addon and dylib:

- `c2-mem-ffi response pool bootstraps a differently sized owner backing` composes a real 1 MiB owner pool with a response pool created at the 8 KiB bootstrap floor, reads and releases the block, rejects a double release, and rejects an unbacked generation.

Generated-artifact regression (`sdk/python/tests/integration/test_typescript_real_calls.py`) runs the compiled contract module that the c3 binary actually generated together with the packed native binding:

- `test_generated_typescript_reader_opens_lazy_empty_handshake` feeds the reader an empty advertised snapshot for a real owner backing, asserts the 8 KiB bootstrap floor was requested, and asserts the bytes read back match. A stale generated transport fails this test rather than silently inheriting the previous receipt, and the packaged native binding is exercised against a real owner backing, which addresses the stale-generator risk without touching the shared venv or another worktree.

## Verification

Affected checks were run in this checkout with an independent environment (`CARGO_TARGET_DIR` under the workspace, `FASTDB_PAYLOAD_LINK_MODE=system`, `FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib`, `CARGO_BUILD_JOBS=2`), never the Host venv:

- `cargo test --locked --manifest-path core/Cargo.toml -p c2-mem-ffi`: 23 passed, 0 failed (exit 0).
- `cargo test --locked --manifest-path core/Cargo.toml -p c2-codegen`: all suites passed, including `typescript_transport_contract` 6/6 and `generated_targets` 7/7 (exit 0).
- `node --test tests/*.test.mjs` in `core/foundation/c2-mem-ffi/bindings/typescript` after `node scripts/build-runtime.mjs` with the audited TypeScript 5.9.3 compiler: 34 passed, 0 failed (exit 0), plus `build-types.mjs --no-emit` clean.
- The lazy-handshake script used by the integration regression was additionally executed standalone against the freshly built binding and a locally compiled `typescript_transport.ts`: `{"ownerSegmentSize":1048576,"bootstrapSegmentSize":8192,"byteLength":1024}`.

Not verified here: the full 12-row TypeScript matrix. It needs the rebuilt c3 CLI, the rebuilt Python native extension, and the FastDB TypeScript package; building the FastDB package would write to another worktree, which this task forbids. The Host rebuilds and runs the strict 12-row real-call matrix as the final gate, and the new generated-artifact regression is what prevents a stale c3/native pair from passing that gate.

## Host integration verification

Host reviewed the fixed artifact `b8dc4c49-3e32-4379-ab55-b0405c26939c` and confirmed the native changes are regression tests only; response geometry and generation validation still belong to the existing c2-mem opener. In the integration checkout, the affected c2-mem-ffi/codegen suites passed, c3 was rebuilt, and the real TypeScript integration file passed 14/14 tests. The strict receipt validator accepted all 12 rows and matched the tested c3 bytes (SHA-256 `d58b2e779b1dc4ddb6672536be7fdab4e6c8758550b24064b066d621d24d7f88`). Raw evidence is retained under `/tmp/c2-ts-lazy-acceptance-0929/`. This local result does not establish Windows completion. Host corrected the test documentation to distinguish stale-generator detection from exercising the existing native binding.
