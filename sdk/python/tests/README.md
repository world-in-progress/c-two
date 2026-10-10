# Python tests and benchmarks

Run commands from the repository root. Follow the [development setup](../../../docs/development.md) for the matching FastDB Core SDK, sibling source fixtures, Node/Emscripten and Python 3.10 prerequisites. Build the Python native extension and standalone C3 before relay tests:

```bash
FASTDB_PAYLOAD_LINK_MODE=source uv sync
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link
```

The SDK does not embed a relay. Tests start their own `c3 relay` processes using the available binary.

## Run the suite

On Linux/macOS, run independent cases with four pytest workers:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests -q -n 4 --timeout=30
```

The shared fixtures isolate IPC names by run and worker and lock relay startup only through readiness. Windows uses the serial Python harness; omit `-n 4`. In PowerShell, clear inherited relay discovery with `$env:C2_RELAY_ANCHOR_ADDRESS = ''` before running the command.

The default timeout is 30 seconds per test. Tests that require longer waits declare their own timeout: the real idle-cleanup regression uses 240 seconds and waits through two actual 62-second idle periods. Portable interoperability gates use 300 seconds. Do not shorten those waits to test-only pool settings.

Run focused regressions after a relevant change:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest \
  sdk/python/tests/unit/test_connect_timeout.py \
  sdk/python/tests/integration/test_connect_timeout.py \
  sdk/python/tests/integration/test_client_pool_idle_cleanup.py \
  sdk/python/tests/integration/test_typescript_persistent_ipc.py \
  -q --timeout=300

# One existing test file
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/unit/test_wire.py -q --timeout=30

# Ensure the minimum-supported interpreter check runs.
uv python install 3.10
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest \
  sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python \
  -q --timeout=30 -rs
```

Python 3.10 is the supported minimum; a skipped interpreter check is not a passing compatibility result. For a broader 3.10 run, use a separate environment as described in the development guide.

## Coverage

| Location | Behavior |
| --- | --- |
| `unit/test_runtime_session.py`, `unit/test_runtime_session_lifecycle.py` | Native registry authority and lifecycle outcomes |
| `unit/test_call_options.py`, `integration/test_connect_timeout.py` | Call options and connection-acquisition deadlines |
| `integration/test_client_pool_idle_cleanup.py` | Two real processes, default idle grace and stable Unix socket counts |
| `integration/test_typescript_persistent_ipc.py` | Generated TypeScript bindings across route changes and reconnects |
| `integration/test_memory_capacity_errors.py`, `integration/test_memory_budget_lifetime.py` | Capacity errors, accounting and retained input/response lifetimes |
| `integration/test_owner_bound_lifecycle.py` | Owner capability and shutdown barriers |
| `integration/test_http_relay.py`, `integration/test_relay_mesh.py` | External relay and mesh discovery/forwarding |
| `integration/test_portable_payload_cross_language.py`, `integration/test_portable_payload_matrix.py` | Rust/Python portable payloads and the exact 18-row matrix |
| `integration/test_typescript_real_calls.py` | Generated TypeScript calls and payload lifetimes |

The SDK socket-count regression runs on Unix. Rust pool tests use actual local streams and peer EOF on Unix and Windows Named Pipes; Windows evidence comes from the Windows Native workflow. `protocol_address` currently supplies only unique logical `ipc://` addresses. Same-process and HTTP cases have dedicated fixtures.

Cross-language checks require the development prerequisites above:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest \
  sdk/python/tests/integration/test_portable_payload_cross_language.py \
  sdk/python/tests/integration/test_portable_payload_matrix.py \
  -q --timeout=300
uv run --no-sync pytest tests/repo/test_portable_matrix_receipt.py -q
cargo test --manifest-path core/Cargo.toml -p c2-ipc
```

See the [0.7.4 publication record](../../../docs/reports/0.7.4-publication.md) and [FD regression record](../../../docs/reports/0.7.4-fd-leak-regression.md) for executed source-specific evidence.

## Benchmarks

Benchmark scripts live in [benchmarks](../benchmarks). Run them from the repository root, for example:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync python sdk/python/benchmarks/segment_size_benchmark.py
```

`thread_vs_ipc_benchmark.py` compares same-process and IPC calls; `chunked_benchmark.py` exercises chunking; `relay_qps_benchmark.py` measures relay load. Each script controls its own payload sizes and rounds. Benchmark timings do not replace correctness, cleanup or platform validation.
