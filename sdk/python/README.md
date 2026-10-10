# C-Two Python SDK

C-Two exposes stateful Python resources through typed CRM contracts over same-process calls, local IPC, or an external HTTP relay. This package projects the shared Rust runtime into Python.

Python C-Two **0.7.4** uses c3 **0.3.3**. See the [Python usage guide](https://github.com/world-in-progress/c-two/blob/main/docs/python-usage.md), [configuration guide](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md) and [release notes](https://github.com/world-in-progress/c-two/blob/main/docs/releases/0.7.4.md).

```bash
uv pip install 'c-two==0.7.4'
```

FastDB is pinned to `fastdb4py==0.2.1`. See the [project quickstart](https://github.com/world-in-progress/c-two/blob/main/README.md) and [Chinese edition](https://github.com/world-in-progress/c-two/blob/main/README.zh-CN.md).

Local IPC uses Unix domain sockets or current-logon-SID Windows Named Pipes. Unix roots select the final endpoint directory, with short names and directory-relative connections. Rust owns `Persistent`/`OwnerBound` policies, structured shutdown and scoped maintenance. Install matching builds across communicating clients, resource servers and c3. See the [lifecycle guide](https://github.com/world-in-progress/c-two/blob/main/docs/local-endpoint-lifecycle.en.md).

`cc.connect(..., timeout=...)` bounds connection acquisition across pool waiting, handshake and route discovery. `cc.with_call_options(proxy, timeout=...)` sets business-call waiting separately. Custom Unix endpoint directories can use `0755` when the current user owns them and other identities cannot modify them; Windows uses its SID-scoped pipe namespace. See [connection deadlines](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md#connection-deadlines) and [endpoint configuration](https://github.com/world-in-progress/c-two/blob/main/docs/configuration.en.md#local-endpoint-directories).

IPC uses lazy buddy pools, idle decay and finite backing/reassembly budgets; `pool_enabled=False` skips buddy. `cc.memory_stats()` observes native accounting; held responses and borrowed inputs retain independent leases. The standalone relay has its own upstream policy. See the [memory guide](https://github.com/world-in-progress/c-two/blob/main/docs/memory-policy.en.md).

The [0.7.4 publication record](https://github.com/world-in-progress/c-two/blob/main/docs/reports/0.7.4-publication.md) identifies published sources, Linux/macOS and Windows Server 2022/2025 x64 gates, and verification of public artifacts. Windows 11 desktop, Windows ARM64 and Windows services are outside this validation.

## Development

Use canonical main for source development and run commands from the repository root. The matching c3 release's `rc-manifest.json` identifies release source:

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
- for full interoperability tests: the FastDB `v0.2.1` source checkout at `../fastdb`

Python resolves `fastdb4py==0.2.1` from PyPI. The Python native extension and
c3 pin FastDB source `4f99f86a662b0e950a0dd29800c25a1c9fca4def` for their source-mode static
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
