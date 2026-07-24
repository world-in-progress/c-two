# C-Two Python SDK

This directory contains the Python package for C-Two.

## Development

Run development commands from the repository root.

Prerequisites:

- Python 3.10 or newer
- Python 3.12 for standard local development
- Python 3.14.3t when testing free-threading support
- Rust toolchain
- `uv`
- the audited compatible FastDB source checkout at `../fastdb`

The complete portable-payload integration is currently proven only against
that sibling FastDB checkout. The corresponding immutable Rust, Python, and
TypeScript packages have not yet been released, so a registry-only environment
cannot reproduce this development branch. See
[`contract-release-deferred-capabilities.md`](../../docs/issues/contract-release-deferred-capabilities.md).

Install dependencies and build the Python native extension. This also compiles
the required Rust core crates; no separate Rust prebuild step is needed:

```bash
uv sync
```

Rebuild the native extension after changing Rust code:

```bash
uv sync --reinstall-package c-two
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

Run the Python SDK tests:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests -q --timeout=30
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
