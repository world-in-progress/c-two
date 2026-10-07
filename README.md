<p align="center">
  <img src="docs/images/logo.png" width="150">
</p>

<h1 align="center">C-Two</h1>

<p align="center">
  A resource-oriented RPC runtime — turn stateful classes into location-transparent distributed resources.
</p>

<p align="center">
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/pypi/v/c-two" alt="PyPI" /></a>
  <a href="https://pypi.org/project/c-two/"><img src="https://img.shields.io/badge/Python-3.10%2B-blue" alt="Python 3.10+" /></a>
  <img src="https://img.shields.io/badge/free--threading-3.14t-blue" alt="Free-threading" />
  <a href="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml"><img src="https://github.com/world-in-progress/c-two/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/world-in-progress/c-two" alt="License" /></a>
</p>

<p align="center">
  <a href="README.zh-CN.md">中文版</a>
</p>

---

## What is C-Two

C-Two is a resource-oriented RPC runtime for distributed scientific computation. Instead of carving state out of objects into stateless services, you keep stateful classes — simulations, indexes, instrument controllers — and expose them behind a CRM (Core Resource Model) contract. Any client then calls the resource as if it were a local object, in the same process, on the same host, or on another machine.

- **Resources, not services** — a CRM contract class declares *which* methods a resource exposes; a plain Python class implements them with real state; anything that calls `cc.connect(...)` is a client.
- **Explicit transports** — same-process calls pass Python objects directly with zero serialization; same-host IPC uses Unix domain sockets or Windows Named Pipes with native shared-memory payload transport; cross-machine calls go through an HTTP relay run by the standalone `c3` CLI.
- **Portable payloads** — methods that cross language boundaries bind an official FastDB `Payload` explicitly with `@cc.transfer(...)`; ordinary Python values remain available for Python-scoped prototyping.
- **One Rust core** — routing, client/host calls, retry classification, wire codec, shared memory, relay transport, and configuration live in language-neutral Rust crates; the Python and Rust SDKs are facades over the same runtime.

## Version and runtime

This guide describes Python C-Two **0.7.0** / c3 **0.3.0**. See [release preparation](docs/releases/0.7.0.md) for version availability, upgrades and validation progress.

| Area | Capability and boundary |
| --- | --- |
| Python | Typed CRM, same-process calls, portable FastDB payloads and native IPC. |
| c3 | Standalone relay, contract tooling and scoped endpoint maintenance; Unix and Windows x64 installation entries are below. |
| FastDB dependency | [FastDB 0.2.1](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.1) is published; Python and Rust dependencies are pinned to 0.2.1, with native source builds pinned to `4f99f86a662b0e950a0dd29800c25a1c9fca4def`. |
| Source validation | [Canonical validation](docs/reports/canonical-local-endpoint-validation.md) records Linux and Windows Server 2022/2025 x64 CPython 3.12 source gates plus macOS component evidence and its rerun limits. Its exact source pair is C-Two `e49652e85a384f1dd3489f393c13f4d9fff27f9f` / FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`; this does not prove all 0.7 release packages/ABIs. Windows 11 desktop, ARM64 and services remain unvalidated. |
| Rust / TypeScript | Rust SDK, Core and TypeScript package versions remain 0.1.0; no C-Two crates.io or npm release. Rust SDK has `publish = false`; generated Node interoperability is proven, browser runtime remains open. |
| Benchmarks | No reviewed throughput benchmark exists for the portable `Payload` API. Historical numbers from removed integrations are not valid claims for the current architecture. |

The 0.x line takes clean cuts over compatibility shims. For 0.7, upgrade communicating clients, resource servers and `c3` together: the Unix endpoint namespace changes from `/tmp/c_two_ipc` to the private native v2.2 namespace. Old endpoints do not migrate automatically.

### Local endpoints, lifecycle and memory

Rust derives one local endpoint per OS, with no backend selector. Services remain `Persistent` by default; dedicated controller-owned children can opt into `OwnerBound`. Shutdown completion, listener closure and retained payload leases are separate observations. See the [lifecycle integration guide](docs/local-endpoint-lifecycle.md).

IPC buddy pools allocate lazily, can decay to zero idle mappings, and share finite backing/reassembly budgets per runtime direction. `pool_enabled=False` skips buddy allocation while dedicated SHM, checked chunks and receiving-side file spill remain available. The standalone relay has its own upstream IPC policy. See the [memory guide](docs/memory-policy.md) and [CLI guide](cli/README.md).

## Installation

### Python 0.7.0

In an activated environment, select the version explicitly; `fastdb4py==0.2.1` is installed automatically:

```bash
uv pip install 'c-two==0.7.0'
```

Use the same command in Windows PowerShell. See [release preparation](docs/releases/0.7.0.md) for package availability and the [Windows guide](docs/windows-native-usage.md) for platform instructions.

### Development checkout

For source development, check out canonical main from [world-in-progress/c-two](https://github.com/world-in-progress/c-two). Use the matching c3 release's `rc-manifest.json` to identify release source; a development checkout is not a release artifact.

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
# Full interoperability tests, golden fixtures, and TypeScript fixtures also
# need the pinned FastDB source checkout as a sibling:
git clone --branch v0.2.1 --depth 1 https://github.com/world-in-progress/fastdb.git ../fastdb
# Core and Rust SDK tests use the published Rust bindings in system mode.
# Extract the matching Core SDK from the FastDB 0.2.1 release first:
export FASTDB_PAYLOAD_LINK_MODE=system
export FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/absolute/path/to/fastdb-core-sdk/lib
case "$(uname -s)" in
  Darwin) export DYLD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
  Linux) export LD_LIBRARY_PATH="$FASTDB_PAYLOAD_SYSTEM_LIB_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
esac
FASTDB_PAYLOAD_LINK_MODE=source uv sync        # compile the Python extension with static FastDB
FASTDB_PAYLOAD_LINK_MODE=source uv sync --group examples  # optional: pandas/pyarrow
FASTDB_PAYLOAD_LINK_MODE=source python tools/dev/c3_tool.py --build --link  # build and link the native c3 CLI
cp .env.example .env                          # optional: local environment configuration
```

Requires [uv](https://github.com/astral-sh/uv) and a Rust toolchain. The setup above uses the Core SDK for development and testing. Deployable CLI and Python-extension builds select `FASTDB_PAYLOAD_LINK_MODE=source` and statically link the same immutable FastDB 0.2.1 source; this is separate from the Core/Rust SDK system-link contract. Run the Python suite with `C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q`, and the Rust suites with `cargo test --manifest-path core/Cargo.toml --workspace` and `cargo test --manifest-path sdk/rust/Cargo.toml --all-features`. Python 3.10 remains the supported minimum. On Windows, follow the [Windows build guide](docs/windows-native-usage.md) instead of the script above.

Full interoperability tests also require Node 22, CMake/Ninja and Emscripten
5.0.2 (with `emcmake` on `PATH`). Install both sets of TypeScript dependencies
and the minimum Python interpreter before running those suites:

```bash
npm ci --prefix ../fastdb/ts/fastdb4ts
npm ci --prefix core/foundation/c2-mem-ffi/bindings/typescript
uv python install 3.10
```

The [CI setup](.github/workflows/ci.yml) records the pinned Emscripten setup and
scopes FastDB system linking to Rust consumers. After building the Python
extension, use `uv run --no-sync pytest ...` when running with that system-link
environment so the test command does not rebuild the extension.

### c3 CLI: Linux / macOS

Python 0.7.0 pairs with c3 0.3.0. Availability of the [release entry](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.3.0) is tracked in [release preparation](docs/releases/0.7.0.md). Pass the version explicitly; the installer verifies the checksum:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh -s -- --version 0.3.0
```

### c3 CLI: Windows x64

Use the standalone [Windows x64 executable](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.0/c3-x86_64-pc-windows-msvc.exe), its [SHA-256 file](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.0/c3-x86_64-pc-windows-msvc.exe.sha256), or the [PowerShell installer](https://github.com/world-in-progress/c-two/releases/download/c3-v0.3.0/c3-installer.ps1). See the [Windows guide](docs/windows-native-usage.md#install-python-and-c3) for checksum verification and installation with `-Version 0.3.0`. After verifying the downloaded executable:

```powershell
.\c3-x86_64-pc-windows-msvc.exe --version
.\c3-x86_64-pc-windows-msvc.exe relay --bind 127.0.0.1:8300
```

The [CLI guide](cli/README.md#windows-x64) covers directories and options. No administrator privileges are required for the per-user installer; this exe target does not cover Windows ARM64.

## Quickstart

Save the following as `counter.py`:

```python
import c_two as cc


@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...


class CounterResource:
    """Plain Python class — state plus domain logic, no decorator."""

    def __init__(self, initial: int = 0):
        self._value = initial

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def value(self) -> int:
        return self._value


cc.register(Counter, CounterResource(initial=10), name='counter')

with cc.connect(Counter, name='counter') as counter:
    print(counter.increment(3))   # 13
    print(counter.increment(4))   # 17
    print(counter.value())        # 17

cc.unregister('counter')
cc.shutdown()
```

Run it with:

```bash
pip install c-two
python counter.py
```

Expected output:

```text
13
17
17
```

The example assumes no relay is configured. If your environment sets `C2_RELAY_ANCHOR_ADDRESS` and no relay is running, launch with `C2_RELAY_ANCHOR_ADDRESS= python counter.py` so registration stays local.

How the pieces map to the model:

- **CRM contract** — `Counter` is the interface: decorated with `@cc.crm(namespace=..., version=...)`, method bodies are `...`. Only contract methods are remotely callable. `@cc.read` marks a method as safe for concurrent access; unmarked methods take exclusive write access.
- **Resource** — `CounterResource` is a plain class holding real state (`self._value`). The framework discovers its methods through the CRM contract it was registered under; no decorator is needed.
- **Server** — `cc.register(...)` hosts the resource under a routing `name` and binds an IPC endpoint automatically; `cc.serve()` would block on the request loop if you were hosting for other processes. A process can register some resources and connect to others at the same time.
- **Client** — `cc.connect(Counter, name='counter')` returns a typed, location-transparent proxy. With no address and no relay it resolves to the same-process resource and passes Python objects directly, skipping serialization entirely.

The same CRM contract works unchanged over IPC or a relay:

```python
# Direct IPC, same host — the address comes from the hosting process
# (cc.server_address()) or your own ipc:// assignment:
counter = cc.connect(Counter, name='counter', address='ipc://my_server')

# Cross-machine — point both sides at a relay, then connect by name:
cc.set_relay_anchor('http://relay-host:8080')
counter = cc.connect(Counter, name='counter')
```

See [Runnable examples](#runnable-examples) for complete two-process and relay-mesh layouts.

## Transports

| Mode | How it is selected | Transport | Best for |
| --- | --- | --- | --- |
| Same process | `cc.connect(...)` with no address and no relay, target registered locally | Direct call, zero serialization | Tests, composing resources in one process |
| IPC | explicit `address='ipc://...'` | Unix domain sockets / Windows Named Pipes; payload bytes can travel through native shared memory | Multiple processes on one host |
| HTTP relay | `cc.set_relay_anchor(...)` or `C2_RELAY_ANCHOR_ADDRESS`, then connect by name | HTTP to the relay's resolved route | Cross-machine calls and name-based discovery |

Direct IPC is relay-independent: an explicit `ipc://` address bypasses relay discovery and works with no relay configured. When a relay *is* used, resolution is contract-scoped — the client derives the expected route contract from its CRM class, and the runtime matches the route name, CRM tag, ABI hash, and signature hash before any call. Relay responses select HTTP unless the anchor is loopback, in which case a direct IPC fast path may be used after validating the handshake server and instance identities as well as the expected route contract. Ambiguous data-plane failures are not automatically replayed.

The relay is a separate process you run and monitor yourself — `c3 relay`, Docker Compose, or your orchestrator; the Python SDK never embeds one. Multiple relays can form a gossip-based mesh in which any relay resolves routes registered anywhere in the mesh. Relay endpoints are intended for a trusted network boundary: restrict access with private networking, firewalls, or equivalent infrastructure. See [.env.example](.env.example) for the full tuning surface (timeouts, chunk sizes, pool limits, route attempt caps).

IPC can fall back to checked chunks when SHM is unavailable; relay HTTP responses are materialized before being sent in chunks. That is transport batching under a request/response model, not a streaming-RPC API — streaming call semantics are not implemented.

## Portable payloads (FastDB)

Portable methods use the explicit FastDB binding below.

Methods that must move structured data across language boundaries bind an explicit FastDB specification. FastDB Core owns the nested payload semantics — validation, canonical identity, binary layout, builders, views, invalidation, and payload-only codegen — while C-Two owns the outer contract, routing, transport, and lifetimes:

```python
import json

import c_two as cc
from fastdb4py.payload import BuildPolicy, Builder, CompiledSpec, Payload

VALUE_SPEC = {
    "schema": "fastdb.payload.v1",
    "profile": "record.v1",
    "entries": [
        {"id": "value", "cardinality": "one",
         "type": {"kind": "u8", "nullable": False}},
    ],
    "components": [],
}


@cc.crm(namespace='demo.payload', version='0.1.0')
class Echo:
    @cc.transfer(input=VALUE_SPEC, output=VALUE_SPEC)
    def echo(self, payload: Payload) -> Payload: ...


def build_value(value: int) -> Payload:
    spec = CompiledSpec.compile(json.dumps(VALUE_SPEC).encode())
    builder = Builder.create(spec)
    builder.entry_begin(0, 1).value_u8(value)
    plan = builder.freeze()
    builder.close()
    try:
        return plan.execute(BuildPolicy.ALLOW_STAGING).payload
    finally:
        plan.close()
        spec.close()


class EchoResource:
    def echo(self, payload: Payload) -> Payload:
        return payload


cc.register(Echo, EchoResource(), name='echo')
source = build_value(7)
try:
    with cc.connect(Echo, name='echo') as echo:
        result = echo.echo(source)
        result.close()
finally:
    source.close()
    cc.unregister('echo')
    cc.shutdown()
```

A portable method carries zero or one `Payload` envelope in each direction; C-Two embeds each nested specification as an opaque JSON value and never reinterprets it. Methods without an explicit binding can still use ordinary Python values for Python-scoped prototyping, but portable descriptor export and codegen diagnose and reject them.

## Payload lifetimes

The proven portable receive path opens a **copy-backed** FastDB owner. `cc.hold()` is a lifetime contract, not a zero-copy claim: it retains the C-Two response lease together with the payload owner and guarantees that release invalidates the FastDB owner and its checked views *before* the lease goes back.

```python
with cc.hold(echo.echo)(source) as held:
    payload = held.value                    # FastDB Payload owner; checked views stay valid
    ...
# leaving the block (or held.release()) invalidates owner and views, then frees the lease
```

Release is layered: explicit `.release()`, the `with` context manager, and a `__del__` fallback that warns if you forget both. `cc.hold_stats()` reports active holds for monitoring.

`held.unsafe_buffer` exposes the retained raw wire buffer as a `memoryview` escape hatch. Raw NumPy arrays or pointers derived from it bypass FastDB's checked owner/view model and **cannot be revoked mechanically** — materialize values through FastDB before storing them beyond the hold scope.

On the server side, portable inputs are owned by default. Registering with `cc.register(..., input_lifetime={...: cc.InputLifetime.BORROWED})` opts specific methods into call-scoped borrowed inputs: C-Two invalidates the payload and its checked views before releasing the request lease when the call returns or raises. Do not retain a borrowed payload or view after the method returns.

## Entry points

### Python SDK

The main user surface, shown throughout this README. The top-level `cc` namespace groups:

- Authoring: `@cc.crm`, `@cc.read`, `@cc.write`, `@cc.on_shutdown`, `@cc.transfer`, `cc.hold`, `cc.InputLifetime`
- Registry: `cc.register`, `cc.connect`, `cc.close`, `cc.unregister`, `cc.serve`, `cc.shutdown`, `cc.server_address`, `cc.set_server`, `cc.set_client`, `cc.set_relay_anchor`, `cc.set_transport_policy`
- Contracts: `cc.export_contract_descriptor`, `cc.export_contract_release_ref`, `cc.compile_contract_artifacts`, `cc.infer_crm_from_resource`
- Lifecycle and maintenance: `cc.LifecycleConfig`, `cc.owner_control_pair`, `cc.spawn_owned_child`, `cc.adopt_owner_stdin`, `cc.inspect_endpoint`, `cc.reap_endpoint`, `cc.sweep_endpoints`
- Monitoring: `cc.hold_stats`; `cc.memory_stats` (read-only)

### Rust SDK

The user-facing Rust SDK lives at [`sdk/rust`](sdk/rust/README.md): Cargo package `c-two`, imported as `c_two`, version 0.1.0, `publish = false` (source-only; no crates.io release). It reuses the same `c2-core` runtime as Python for direct IPC, explicit relay, and relay-aware calls, and provides generated typed clients and service traits. Portable values remain official `fastdb::Payload` owners:

```rust
use c_two::{Connect, ContractLimits, ContractRelease, Runtime};
use fastdb::Payload;
```

Building it requires the same FastDB Core SDK system-link environment as the development setup above. From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

### c3 CLI

`c3` is the cross-language native CLI (built from the root `cli/` package):

```bash
# Run a relay anywhere reachable on your network
c3 relay --bind 0.0.0.0:8080
# Pre-register an upstream: NAME=SERVER_ID@ADDRESS (IDs must match the IPC handshake)
c3 relay --upstream grid=my_server@ipc://my_server

# Contract tooling — export from Python CRM classes, validate, derive identity, generate code
c3 contract export mypkg.contracts:Geometry --python .venv/bin/python --out geometry.contract.json
c3 contract validate geometry.contract.json
c3 contract release-ref geometry.contract.json
c3 contract codegen rust geometry.contract.json --out-dir generated-rust   # also: python, typescript
```

`c3 contract diagnose` reports which methods remain Python-only before portable export fails. Generated trees contain the validated contract metadata plus FastDB Core-owned payload modules; each destination directory must not exist yet. See `c3 --help` and `c3 relay --help` for the full option surface.

## Runnable examples

| Scenario | Entry points |
| --- | --- |
| Same-process local call | [`examples/python/local.py`](examples/python/local.py) |
| Direct IPC resource/client | [`examples/python/ipc_resource.py`](examples/python/ipc_resource.py), [`examples/python/ipc_client.py`](examples/python/ipc_client.py) |
| Relay resource/client | [`examples/python/relay_resource.py`](examples/python/relay_resource.py), [`examples/python/relay_client.py`](examples/python/relay_client.py) |
| Relay mesh | [`examples/python/relay_mesh/`](examples/python/relay_mesh/) |
| Python-only grid prototype | [`examples/python/grid/`](examples/python/grid/) |
| Portable payload runtime and lifetime proof | [`sdk/python/tests/integration/test_portable_payload_runtime.py`](sdk/python/tests/integration/test_portable_payload_runtime.py) |
| Rust/Python generated-artifact interop proof | [`sdk/python/tests/integration/test_portable_payload_cross_language.py`](sdk/python/tests/integration/test_portable_payload_cross_language.py) |

## Documentation

| Topic | Document |
| --- | --- |
| Contributing | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Repository guide for agents and maintainers | [AGENTS.md](AGENTS.md) |
| Roadmap | [docs/roadmap.md](docs/roadmap.md) · [中文](docs/roadmap.zh-CN.md) |
| Changelog | [CHANGELOG.md](CHANGELOG.md) |
| 0.7.0 / c3 0.3.0 preparation and upgrade | [docs/releases/0.7.0.md](docs/releases/0.7.0.md) |
| Local endpoint lifecycle | [docs/local-endpoint-lifecycle.md](docs/local-endpoint-lifecycle.md) |
| IPC memory policy | [docs/memory-policy.md](docs/memory-policy.md) |
| Windows build and usage | [docs/windows-native-usage.md](docs/windows-native-usage.md) |
| Windows implementation record | [docs/windows-native-implementation.md](docs/windows-native-implementation.md) |
| Environment variable reference | [.env.example](.env.example) |
| Rust SDK boundary | [sdk/rust/README.md](sdk/rust/README.md) |
| Deferred capabilities and open boundaries | [docs/issues/contract-release-deferred-capabilities.md](docs/issues/contract-release-deferred-capabilities.md) |

## License

[MIT](LICENSE)

---

<p align="center">Built for resource-oriented computation. Powered by Rust.</p>
