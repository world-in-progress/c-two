# C-Two Python SDK

C-Two exposes stateful Python resources through typed CRM contracts over same-process calls, local IPC, or an external HTTP relay. This package projects the shared Rust runtime into Python.

Python C-Two **0.9.0** pairs with c3 **0.5.0**. See the [release and migration notes](https://github.com/world-in-progress/c-two/blob/main/docs/releases/0.9.0.md).

Install the matching published versions:

```bash
uv pip install 'c-two==0.9.0'
# Only for FastDB Payload contracts:
uv pip install 'c-two[fastdb]==0.9.0'
```

The base package has no Python runtime dependencies. Ordinary CRM definition,
registration, direct/IPC/relay calls and errors do not load FastDB or NumPy.
The `fastdb` extra pins `fastdb4py==0.2.3`; using FastDB payloads without it
reports the extra to install. The exact pin selects the validated 0.x binding
instead of allowing an untested later native interface.

Local IPC uses Unix domain sockets or TokenUser-scoped Windows Named Pipes. Unix roots select the final endpoint directory, with short names and directory-relative connections. Rust owns `Persistent`/`OwnerBound` policies, structured shutdown and scoped maintenance. Install matching builds across communicating clients, resource servers and c3. See the [lifecycle guide](https://github.com/world-in-progress/c-two/blob/main/docs/local-endpoint-lifecycle.en.md).

`cc.connect(..., timeout=...)` bounds connection acquisition across pool waiting, handshake and route discovery. `cc.with_call_options(proxy, timeout=...)` sets business-call waiting separately. Custom Unix endpoint directories can use `0755` when the current user owns them and other identities cannot modify them; Windows uses its SID-scoped pipe namespace. See [connection deadlines](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md#connection-deadlines) and [endpoint configuration](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md#local-endpoint-directories).

`cc.set_server(startup_timeout=30.0)` sets the native startup wait before registration. Omitted/`None` retains the finite five-second default; zero expires immediately. Startup expiry is identified by `CoreError.lifecycle_kind == 'startup_timeout'`, with `startup_stage` and `timeout_seconds`; bind I/O failures keep their OS fields without a configuration-error label. See [server startup deadlines](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md#server-startup-deadlines) for freezing and cleanup semantics.

IPC uses lazy buddy pools, idle decay and finite backing/reassembly budgets; `pool_enabled=False` skips buddy. `cc.memory_stats()` observes native accounting; held responses and borrowed inputs retain independent leases. The standalone relay has its own upstream policy. See the [memory guide](https://github.com/world-in-progress/c-two/blob/main/docs/memory-policy.en.md).

Use `cc.connection_events()` for one cancellable, bounded stream of incoming and outgoing physical IPC events, tagged by `side`. `ConnectionObservation.snapshot()` retains late inspection; its old blocking `wait_closed()` is removed. Physical observations work without a pending call. OwnerBound hosts support explicit, single-use recovery tickets during the original grace period. Bind/connect failures preserve `io_kind`, `raw_os_error` and `io_operation` on public `cc.error.CoreError`. These features share the Rust implementation on macOS, Linux and Windows x64 and ARM64; see the lifecycle guide for API examples.

## Development

Use canonical main for source development and run commands from the repository root. Python patch-release provenance is recorded with its release notes; the c3 release's `rc-manifest.json` identifies that CLI release's source, which can differ from a later Python patch:

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
```

Prerequisites:

- Python 3.10 or newer
- Python 3.12 for standard local development
- Python 3.14.3t when testing free-threading support
- Rust toolchain
- `uv`
- for full interoperability tests: the FastDB `v0.2.3` source checkout at `../fastdb`

The default workspace dev group selects `c-two[fastdb]` and resolves
`fastdb4py==0.2.3` from PyPI. Bare package installation omits it. The Python native extension and
c3 pin FastDB source `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4` for their source-mode static
builds. Core and Rust SDK tests use the published Rust bindings with the matching
[FastDB Core SDK](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.3)
in system link mode. Golden specs and TypeScript interoperability fixtures still
need the sibling source checkout; it is not a Python package override.

Install dependencies and build the Python native extension. This also compiles
the required Rust core crates; no separate Rust prebuild step is needed:

```bash
FASTDB_PAYLOAD_LINK_MODE=source uv sync
```

Rebuild the native extension after changing Rust code:

```bash
FASTDB_PAYLOAD_LINK_MODE=source uv sync --reinstall-package c-two
```

Relay-dependent tests and examples require the standalone `c3` binary. From a
source checkout, build and link it before running relay flows:

```bash
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link
```

The Python SDK does not embed or start a relay server. Start the standalone
Rust relay with `c3 relay`, Docker Compose, or orchestration such as
Kubernetes, then point Python code at its relay anchor with
`C2_RELAY_ANCHOR_ADDRESS` or `cc.set_relay_anchor()`. The anchor is used for
registration and name resolution. Remote HTTP calls still use the resolved
route's `relay_url` directly, and local direct IPC is selected only for a
loopback/local anchor.
Relay-aware clients preflight routes before the first call and re-resolve
structured stale-route responses; set `C2_RELAY_ROUTE_MAX_ATTEMPTS` to tune the
maximum route acquisition attempts (default `3`, valid range `1..=32`, `0` is
treated as `1`). Ambiguous data-plane failures are not replayed.

After the source-mode build, configure the absolute Core SDK library directory
for Rust consumers spawned by interoperability tests. Follow the
development setup in the repository README and
[Windows guide](https://github.com/world-in-progress/c-two/blob/main/docs/windows-native-usage.md)
for platform-specific loader paths and test prerequisites. Keep the existing
extension build while running the Python tests:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests -q -n 4 --timeout=30
```

The four-worker command is for Linux/macOS; omit `-n 4` on Windows. See the [test guide](https://github.com/world-in-progress/c-two/blob/main/sdk/python/tests/README.md) for prerequisites and longer real-idle and interoperability cases.

Run Rust core checks when validating shared native runtime changes:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
```

For CLI build, link, and test commands, see the [CLI guide](https://github.com/world-in-progress/c-two/blob/main/cli/README.md).

## Examples

Python examples live under `../../examples/python/`:

```bash
uv sync --group examples
uv run python examples/python/local.py
```

Relay examples also need a running standalone relay, for example:

```bash
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link
c3 relay --bind 127.0.0.1:8080
```

## Benchmarks

Python-specific benchmarks live in `benchmarks/`:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
```
