# C-Two Python SDK

C-Two exposes stateful Python resources through typed CRM contracts over
same-process calls, local IPC, or an external HTTP relay. This package projects
the shared Rust runtime into Python.

This source branch prepares Python 0.6.0 and c3 0.2.0; neither candidate has been
published yet. FastDB 0.2.1 is published and is the exact package dependency.
See the [project introduction and quickstart](https://github.com/Dsssyc/c-two/blob/dev-feature/README.md)
and its [Chinese edition](https://github.com/Dsssyc/c-two/blob/dev-feature/README.zh-CN.md).

Local IPC uses Unix domain sockets or Windows Named Pipes. The earlier Windows
source pair passed Windows Server 2022/2025 x64 tests; the updated release
candidate still requires its own CI results. Windows 11 desktop, ARM64, and
Windows services are not covered by that evidence.

## Development

Run development commands from the repository root.

Prerequisites:

- Python 3.10 or newer
- Python 3.12 for standard local development
- Python 3.14.3t when testing free-threading support
- Rust toolchain
- `uv`
- for full interoperability tests: the FastDB `v0.2.1` source checkout at `../fastdb`

Python resolves `fastdb4py==0.2.1` from PyPI. The Python native extension and
c3 use the official FastDB 0.2.1 source revision for their source-mode static
builds. Core and Rust SDK tests use the published Rust bindings with the matching
[FastDB Core SDK](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.1)
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
python tools/dev/c3_tool.py --build --link
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
[development setup](https://github.com/Dsssyc/c-two/blob/dev-feature/README.md#development-checkout)
and [Windows guide](https://github.com/Dsssyc/c-two/blob/dev-feature/docs/windows-native-usage.md)
for platform-specific loader paths and test prerequisites. Keep the existing
extension build while running the Python tests:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests -q --timeout=30
```

Run Rust core checks when validating shared native runtime changes:

```bash
cargo test --manifest-path core/Cargo.toml --workspace
```

For CLI build, link, and test commands, see [`../../cli/README.md`](../../cli/README.md).

## Examples

Python examples live under `../../examples/python/`:

```bash
uv sync --group examples
uv run python examples/python/local.py
```

Relay examples also need a running standalone relay, for example:

```bash
python tools/dev/c3_tool.py --build --link
c3 relay --bind 127.0.0.1:8080
```

## Benchmarks

Python-specific benchmarks live in `benchmarks/`:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run python sdk/python/benchmarks/segment_size_benchmark.py
```
