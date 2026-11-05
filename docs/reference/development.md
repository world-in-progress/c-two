# Source development and testing

This guide owns source-checkout setup, toolchain prerequisites, and local-candidate provenance.

For a shared macOS/Windows local entry with explicit dependency preparation, capability checks and retained test evidence, see the [local development test sidecar](local-development-testing.md). The existing CI and release gates remain separate.

For source development, check out canonical main from [world-in-progress/c-two](https://github.com/world-in-progress/c-two). The c3 release's `rc-manifest.json` identifies that CLI release's source; Python patch-release provenance is recorded with its release notes and can name a later source. A development checkout is not a release artifact.

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
# Full interoperability tests, golden fixtures, and TypeScript fixtures also
# need the pinned FastDB source checkout as a sibling:
git clone --branch v0.2.4 --depth 1 https://github.com/world-in-progress/fastdb.git ../fastdb
# Core and Rust SDK tests use the published Rust bindings in system mode.
# Extract the matching Core SDK from the FastDB 0.2.4 release first:
export FASTDB_PAYLOAD_LINK_MODE=system
export FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/absolute/path/to/fastdb-core-sdk/lib
case "$(uname -s)" in
  Darwin) export DYLD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
  Linux) export LD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
esac
FASTDB_PAYLOAD_LINK_MODE=source uv sync        # compile the Python extension with static FastDB
FASTDB_PAYLOAD_LINK_MODE=source uv sync --group examples  # optional: pandas
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link  # build and link the native c3 CLI
cp .env.example .env                          # optional: local environment configuration
```

Requires [uv](https://github.com/astral-sh/uv) and Rust 1.95 or newer. The full Python suite also requires `hey` v0.1.4 on PATH (`go install github.com/rakyll/hey@v0.1.4`). The setup above uses the Core SDK for development and testing. Deployable CLI and Python-extension builds select `FASTDB_PAYLOAD_LINK_MODE=source` and statically link the same immutable FastDB 0.2.4 source; this is separate from the Core/Rust SDK system-link contract. Run the Python suite in one process with `C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/ -q --timeout=30`. For parallel execution, `tools/dev/test_python.py` runs ordinary test files in parallel and the complete matrices serially; see [CONTRIBUTING.md](../../CONTRIBUTING.md) for usage. See the [test guide](../../sdk/python/tests/README.md) for focused commands and per-test timeout overrides. Run the Rust suites with `cargo test --manifest-path core/Cargo.toml --workspace` and `cargo test --manifest-path sdk/rust/Cargo.toml --all-features`. Python 3.10 remains the supported minimum. On Windows, follow the [Windows build guide](windows-native-usage.md).

The workspace dev group includes `c-two[fastdb]` and pins Python `fastdb4py==0.2.4`. Bare installs omit FastDB and NumPy; the native CoreSDK and Rust source pins above also select 0.2.4.

Banner generation uses the separate `banner` dependency group with patched Pillow 13 and Python 3.11+: `uv run --python 3.11 --group banner tools/dev/generate_banner.py`. Pillow is not required by the SDK or its test suite; those retain Python 3.10 support.

Full interoperability tests also require Node 22, CMake/Ninja and Emscripten 5.0.2 (with `emcmake` on `PATH`). Install both sets of TypeScript dependencies and the minimum Python interpreter before running those suites:

```bash
npm ci --prefix ../fastdb/ts/fastdb4ts
npm ci --prefix core/foundation/c2-mem-ffi/bindings/typescript
uv python install 3.10
```

The [CI setup](../../.github/workflows/ci.yml) records the pinned Emscripten setup and scopes FastDB system linking to Rust consumers. After building the Python extension, use `uv run --no-sync pytest ...` when running with that system-link environment so the test command does not rebuild the extension.

## Native extension conventions

New PyO3 `#[pyclass]` types use `frozen` by default so mutation is explicit through an interior-mutability primitive chosen for its access pattern: atomics for independent flags or counters, a mutex for short exclusive updates, a read/write lock for shared reads, and an async mutex only when a guard must cross an await. Store Python object references as `Py<T>` and enter Python through a Python token when invoking callbacks. The two current non-frozen exceptions are `PyEndpointSweep`, whose iterator advances its owned sweep state, and `PyRouteConcurrencyGuard`, whose one-use guard is consumed on release; review either exception's mutation and thread boundary before extending it. The frozen session, memory pool and client wrappers keep their mutable native state behind locks.

## Relay slow-reader measurement

The run-ahead figures in [Runtime lifecycle](runtime-lifecycle.md#execution-admission) come from an ignored c2-http test that runs only on demand, because it streams hundreds of thousands of items for several minutes. The ordinary suite keeps a short regression that checks that large items stall the TCP writer and keep the producer's lead below the stream length. Run the full measurement with:

```bash
CARGO_TARGET_DIR="$PWD/core/target" cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib \
  relay::block8_tests::slow_reader_records_real_tcp_stall_and_producer_distance \
  -- --exact --ignored --nocapture --test-threads=1
```

It prints one JSON row per carrier, repetition and item size, and the comment beside the test describes how producer and consumer progress are counted. The results depend on the operating system's TCP buffers.

## Portable payload matrix

The Rust/Python portable proof is the exact product of three payloads (`no-payload`, `record-v1`, `object-graph-v1`), three directions (Rust client to Rust host, Rust client to Python host, Python client to Rust host), and two transports (direct IPC and explicit HTTP relay): 18 required rows. `tools/local_rc/portable_matrix_receipt.py` defines the stable row IDs and verifies the `c-two.portable-matrix-receipt.v2` receipt; the integration matrix executes those rows with installed SDKs. Relay rows must use a real `c3 relay`, contract-scoped resolution and its HTTP data plane, without a relay-aware local IPC shortcut. Direct and relay rows use the same descriptor, release identity, FastDB spec digest, inputs and expected logical results. Missing, duplicated, skipped, xfailed or unexpected rows cannot establish the matrix proof; record the source commit and run or artifact identity with any result.

## Local candidate evidence and provenance

The complete Phase 0B upstream local candidate is recorded in [`docs/acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md`](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md). Package inputs are C-Two implementation commit `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02` and FastDB implementation commit `7eb74734926bd8fe911229eee9744a6dd8172487`; later documentation-only commits are never artifact source commits.

The retained canonical manifest and receipts under `docs/acceptance/evidence/` prove 42 exact artifacts, 4/4 isolated Rust/Python/Node consumers, 18/18 Rust/Python direct/relay rows, 12/12 generated TypeScript Node rows, and the 12-row SDK parity inventory. They are evidence records only. Do not add package archives, local registries, wheelhouses, native libraries, generated trees, or build outputs to Git.

FastDB 0.2.0 is now published separately; its release does not change these historical candidate inputs or prove C-Two publication. Official C-Two portable-package distribution, browser runtime, C++ C-Two SDK, compatibility ranges, trust/signature/revocation, streaming, post-dispatch retry/deduplication, and Toodle consumption remain open. Hosted evidence must name its actual source pair and passing gates; never infer it from the local candidate.
