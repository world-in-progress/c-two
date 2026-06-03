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

## Basic Idea

- **Resource-oriented RPC** — C-Two exposes stateful resource objects through language SDKs. The Python SDK makes Python classes remotely accessible while preserving their object-oriented nature.

- **Zero-copy from process to data** — Same-process calls skip serialization entirely. Cross-process IPC can hold shared-memory buffers alive, letting FastDB checked views read columnar data directly from SHM when the caller explicitly retains the response.

- **Built for scientific workloads** — Portable CRM payloads use FastDB; Python-only prototypes can still use ordinary Python values. Large payloads use chunked transfer for data beyond 256 MB. The runtime is designed for computational workflows and stateful scientific resources.

- **Rust-powered core** — Shared transport, memory, wire codec, route-contract validation, relay, and configuration live in Rust so future SDKs reuse one runtime contract.

---

## Performance

End-to-end cross-process IPC benchmark using the Kostya-style coordinate schema: `row_id u32`, `x/y/z f64`, and `name STR`. Each call returns a cached coordinate table from a remote process and the client computes `sum(x + y + z)` from the received payload.

| Rows | FDB control default (ms) | FDB recommended default (ms) | FDB control retained (ms) | FDB recommended retained (ms) | Ray arrays (ms) | C-Two pickle arrays (ms) | **Recommended retained vs Ray arrays** |
|-----:|---:|---:|---:|---:|---:|---:|---:|
| 1 K | 1.28 | 1.33 | 1.30 | **1.12** | 6.51 | 0.57 | **5.8×** |
| 10 K | 1.83 | 1.27 | 1.52 | **1.26** | 7.42 | 1.24 | **5.9×** |
| 100 K | 5.10 | 2.64 | 4.91 | **1.91** | 8.22 | 9.69 | **4.3×** |
| 1 M | 36.39 | 12.29 | 29.85 | **8.36** | 48.59 | 130.65 | **5.8×** |
| 3 M | 147.89 | 39.08 | 93.80 | **23.29** | 164.32 | 529.47 | **7.1×** |

Row-oriented fallback paths are much slower at large sizes and are included only to show Python object materialization cost:

| Rows | C-Two pickle records (ms) | Ray records (ms) | **Recommended retained vs Ray records** |
|-----:|---:|---:|---:|
| 1 K | 0.95 | 6.23 | **5.6×** |
| 10 K | 6.09 | 11.62 | **9.2×** |
| 100 K | 67.25 | 56.31 | **29.5×** |
| 1 M | 861.50 | 546.27 | **65.3×** |
| 3 M | SKIP | SKIP | - |

- **FDB control** - ordinary `fdb.Batch.allocate(...)` resource code. It is kept as a benchmark control to show call-db repacking cost when resource output is built outside the CRM call envelope.
- **FDB recommended** - resource code builds the return payload with `fdb.require(fdb.batch(...))`; at 3M rows this lowers default-call p50 from 147.89 ms to 39.08 ms in this run.
- **Default / retained** - default calls return owned logical values after detaching from the transport buffer. Retained calls use `cc.hold(...)` so FastDB checked views can read from the retained response buffer until release.
- **Ray arrays** - Ray object-store transfer of NumPy columns plus the `name` string list.
- **Pickle arrays / records** - Python-only fallback baselines; records intentionally exercise row-oriented Python object overhead.

The FastDB rows above compare construction paths inside the same C-Two resource model: CRM contract -> resource instance -> typed client proxy.

> Apple M1 Max · measured May 27, 2026 · C-Two: Python 3.14.3 + NumPy 2.4.4 · Ray: Python 3.12 + NumPy 2.4.6 + Ray 2.55.1 · See [`sdk/python/benchmarks/kostya_ctwo_benchmark.py`](sdk/python/benchmarks/kostya_ctwo_benchmark.py), [`sdk/python/benchmarks/kostya_ray_benchmark.py`](sdk/python/benchmarks/kostya_ray_benchmark.py), and [`sdk/python/benchmarks/run_kostya_sweep.sh`](sdk/python/benchmarks/run_kostya_sweep.sh) for methodology.

---

## Quick Start

```bash
pip install c-two
```

### Define a FastDB-first resource contract

```python
import c_two as cc
import fastdb4py as fdb
import numpy as np


@fdb.feature
class Vertex:
    vertex_id: fdb.U32
    x: fdb.F64
    y: fdb.F64
    z: fdb.F64


@fdb.feature
class Node:
    node_id: fdb.U32
    weight: fdb.F64
    anchor: Vertex
    neighbors: list[Vertex]


@cc.crm(namespace='demo.geometry', version='0.1.0')
class Geometry:
    @cc.read
    def vertices(self, count: fdb.I32) -> fdb.Batch[Vertex]:
        ...

    @cc.read
    def nodes(self) -> fdb.Batch[Node]:
        ...
```

`vertices()` is the fixed-size columnar path. `nodes()` is the object-graph path
for nested resource data.

### Implement the resource object

```python
class GeometryResource:
    def vertices(self, count: fdb.I32) -> fdb.Batch[Vertex]:
        n = int(count)
        batch = fdb.require(fdb.batch(Vertex, rows=n))
        idx = np.arange(n, dtype=np.uint32)
        xyz = idx.astype(np.float64)
        batch.fill(vertex_id=idx, x=xyz, y=xyz + 10.0, z=xyz + 20.0)
        return batch

    def nodes(self) -> fdb.Batch[Node]:
        vertices = [
            Vertex(vertex_id=0, x=0.0, y=10.0, z=20.0),
            Vertex(vertex_id=1, x=1.0, y=11.0, z=21.0),
            Vertex(vertex_id=2, x=2.0, y=12.0, z=22.0),
        ]

        batch = fdb.Batch.allocate(Node, 0)
        batch.append(Node(
            node_id=100,
            weight=0.5,
            anchor=vertices[0],
            neighbors=[vertices[1], vertices[2]],
        ))
        return batch
```

### Use it locally (zero serialization)

```python
cc.register(Geometry, GeometryResource(), name='geometry')

with cc.connect(Geometry, name='geometry') as geometry:
    vertices = geometry.vertices(1_000)
    nodes = geometry.nodes()
```

### Use the same client API across processes

```python
# Server process
cc.set_relay_anchor('http://relay-host:8080')
cc.register(Geometry, GeometryResource(), name='geometry')

# Client process (separate terminal)
cc.set_relay_anchor('http://relay-host:8080')
with cc.connect(Geometry, name='geometry') as geometry:
    vertices = geometry.vertices(1_000_000)
```

The same CRM contract can be used in-process, over IPC, or through a relay.
Client code calls the typed proxy in each mode. Portable payloads are authored
with FastDB types; ordinary Python values are available for local Python-only
prototypes.

---

## Core Concepts

### CRM — Contract

A **CRM** (Core Resource Model) declares *which* methods a remote resource exposes. It's decorated with `@cc.crm()`, and method bodies are `...` (pure interface — no implementation).

```python
@cc.crm(namespace='demo.geometry', version='0.1.0')
class Geometry:
    @cc.read
    def vertices(self, count: fdb.I32) -> fdb.Batch[Vertex]:
        ...

    @cc.read
    def nodes(self) -> fdb.Batch[Node]:
        ...
```

Methods can be annotated with `@cc.read` (concurrent access allowed) or left as default write (exclusive access). Portable method inputs and outputs use FastDB scalar aliases, `fdb.Array[...]`, `fdb.Batch[...]`, and FastDB feature classes.

### Resource — Runtime Instance

A **resource** is a plain Python class that implements a CRM contract. It holds state and domain logic. No decorator is required; the framework discovers its methods through the CRM contract it was registered under. Use domain names such as `GeometryResource`.

In the example above, `GeometryResource` is the resource object. The CRM contract stays FastDB-first; the resource implementation is just ordinary Python code that returns FastDB values.

### Client — Consumer

Anything that calls `cc.connect(...)` is a **client** (or consumer / application code). The returned proxy is location-transparent — it works the same whether the resource lives in the same process or on a remote machine.

```python
geometry = cc.connect(Geometry, name='geometry')
vertices = geometry.vertices(1_000)
cc.close(geometry)

# Or with context manager:
with cc.connect(Geometry, name='geometry') as geometry:
    nodes = geometry.nodes()
```

### Server — Resource Host

A **server** is any process that calls `cc.register(...)` to host one or more resources, then usually `cc.serve()` to block on the request loop. One server process can host many resources (each under a unique `name`), and will auto-bind an IPC endpoint the first time a resource is registered.

```python
import c_two as cc

cc.register(Geometry, GeometryResource(), name='geometry')
cc.serve()                                     # blocks; Ctrl-C triggers graceful shutdown
```

- **Server ID** identifies the local IPC server instance. C-Two generates one on first registration unless you call `cc.set_server(server_id=...)` before registering.
- **Address** (`ipc://...`) is the internal local transport endpoint derived from the server ID. Inspect it with `cc.server_address()` only when a same-host process needs to connect directly.
- **`cc.serve()`** is optional — if your host process has its own event loop (web server, GUI, simulation), you can register resources and let them serve in the background while your main loop runs.
- A process can be both a server and a client at the same time (register some resources, connect to others).

### Relay — Distributed Discovery

An **HTTP relay** (`c3 relay`) is a lightweight broker that lets clients reach servers **by route name and CRM contract**, across machines. Servers announce their IPC address plus CRM tag and contract fingerprints to the relay when they register; clients ask the relay with the route name and the expected CRM contract derived from `cc.connect(CRMClass, name='...')`.

The `c3` command is C-Two's cross-language native CLI. From a source checkout, build and link a local development binary with `python tools/dev/c3_tool.py --build --link`. For released binaries, install the latest `c3` with:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

The Python SDK leaves relay lifecycle to `c3 relay`, Docker Compose, or your orchestrator. Point Python code at the relay anchor with `C2_RELAY_ANCHOR_ADDRESS` or `cc.set_relay_anchor()`. The anchor is the control-plane registration/name-resolution endpoint. Remote HTTP calls still go directly to the resolved route's `relay_url`; local direct IPC is used only when the anchor endpoint is loopback/local. Relay-aware clients preflight routes before the first call and re-resolve structured stale-route responses; set `C2_RELAY_ROUTE_MAX_ATTEMPTS` to tune the maximum route acquisition attempts (default `3`, valid range `1..=32`, `0` is treated as `1`). Set `C2_RELAY_CALL_TIMEOUT` to tune CRM call timeout seconds (default `300`; `0` disables the reqwest total timeout). Set `C2_REMOTE_PAYLOAD_CHUNK_SIZE` to tune C-Two remote payload batching for relay HTTP and future remote protocols (default `1048576`; max `134217728`). This setting controls C-Two payload batching, separate from TCP packet, HTTP/1 chunk, or HTTP/2 DATA frame boundaries. Ambiguous data-plane failures require caller-level retry policy. Relay resolve, probe, and call paths require route name plus CRM contract and reject mismatches.

```bash
# Start a relay anywhere reachable on your network
c3 relay --bind 0.0.0.0:8080
```

Relay HTTP and mesh endpoints are intended for a trusted network boundary. Production deployments should restrict access with infrastructure such as private networking, firewalls, Kubernetes NetworkPolicy, service mesh policy, or ingress authentication.

```python
# Server side — announce resources to the relay
cc.set_relay_anchor('http://relay-host:8080')
cc.register(Geometry, GeometryResource(), name='geometry')
cc.serve()

# Client side — resolve by name plus the Geometry CRM contract, no address needed
cc.set_relay_anchor('http://relay-host:8080')
geometry = cc.connect(Geometry, name='geometry')
```

Multiple relays can form a **mesh cluster** via gossip — any relay can resolve any resource registered anywhere in the mesh. The runnable example is listed in [Runnable Examples](#runnable-examples).

> **When do I need a relay?** Use a relay for cross-machine or name-and-contract-based discovery. Same-process and same-host IPC usage can connect directly.

### Payload Model — FastDB First

C-Two CRM payload planning has three internal outcomes: `FDB`, `PYTHON_PICKLE`, and `NO_PAYLOAD`. Portable, cross-language contracts use FastDB call-db payload ABI refs derived from `fastdb4py` annotations. Plain Python annotations run through Python pickle fallback for Python-only prototyping; strict portable export rejects those methods.

The payload ABI sits below C-Two's resource-first architecture. FastDB owns schema, storage layout, views, and allocator-facing payload construction. C-Two owns CRM route contracts, transport, retained-buffer leases, and contract/codegen orchestration, while runtime route/relay/IPC/scheduler layers treat FastDB payloads as opaque ABI-backed bytes.

The two important FastDB CRM shapes are the same two used in the quick start:

- `vertices() -> fdb.Batch[Vertex]`: a fixed-size columnar feature batch. This is the retained-view, high-throughput path.
- `nodes() -> fdb.Batch[Node]`: an object-graph batch with nested features and lists. This is portable FastDB call-db data with a different retained-view profile from fixed columnar data.

```python
@fdb.feature
class Vertex: ...

@fdb.feature
class Node:
    anchor: Vertex
    neighbors: list[Vertex]

@cc.crm(namespace='demo.geometry', version='0.1.0')
class Geometry:
    def vertices(self, count: fdb.I32) -> fdb.Batch[Vertex]: ...
    def nodes(self) -> fdb.Batch[Node]: ...
```

C-Two passes the CRM-derived FastDB binding to FastDB; user code stays at logical `Batch`, `Array`, scalar, and feature types.

Python-only resources may still use ordinary Python annotations and pickle for local prototypes. Those methods are intentionally diagnosed as nonportable by strict contract export/codegen.

### cc.hold() — Client-Side Borrowed Responses

On the client side, normal FastDB CRM calls copy the response payload into an owned local buffer before exposing the logical FastDB value, then release the transport buffer immediately. `cc.hold()` explicitly requests that the transport response buffer remain alive, enabling zero-copy reads when an FDB output payload supports retained views. The returned `cc.Held[R]` wraps the CRM logical return value, exposes the retained raw wire buffer as `.unsafe_buffer` for advanced use, and provides a three-layer safety net for buffer lifecycle:

1. **Explicit `.release()`** — preferred for complex workflows holding multiple buffers
2. **Context manager (`with`)** — recommended for single-buffer scopes
3. **`__del__` fallback** — last resort, emits `ResourceWarning` if you forget to release

```python
geometry = cc.connect(Geometry, name='geometry', address='ipc://server')

# Normal call — exposes an owned logical value after transport release.
vertices = geometry.vertices(1_000_000)

# Retained call — columnar FastDB views read from the retained response.
with cc.hold(geometry.vertices)(1_000_000) as held:
    vertices = held.value
    z_mean = vertices.column.z.to_numpy().mean()
```

> **When to use hold mode:** Large array/columnar data where deserialization dominates cost. For small payloads (< 1 MB), the overhead of tracking SHM lifecycle exceeds the copy cost.

`held.value` is the normal API. It uses FastDB checked views where possible, so child rows and columns fail fast after `held.release()`. `held.unsafe_buffer` is a raw `memoryview` escape hatch; raw NumPy arrays or other pointers derived from that buffer bypass FastDB owner checks. Materialize values with `fdb.materialize(...)` before storing them beyond the hold scope.

Object-graph responses such as `nodes()` are portable FastDB payloads. The current runtime returns them as materialized logical values without retained columnar `held.unsafe_buffer`.

---

### InputLifetime — Server-Side Borrowed Inputs

On the server side, FastDB inputs are materialized by default before the resource method is called. Use `cc.register(..., input_lifetime={...})` only when the resource method has the same FDB input signature as the CRM and is prepared to treat a buffer-view input as call-scoped borrowed data.

`cc.InputLifetime.BORROWED` is valid only for FDB payloads with buffer-view support. Use it separately from `bridge.input`; copy any data retained after the method returns with `fastdb4py.materialize(value)` or `value.to_owned()`.

## Runnable Examples

The quick start above shows the end-to-end authoring pattern. The repository examples provide runnable process layouts:

| Scenario | Entry points |
| --- | --- |
| Same-process local call | [`examples/python/local.py`](examples/python/local.py) |
| Direct IPC resource/client | [`examples/python/ipc_resource.py`](examples/python/ipc_resource.py), [`examples/python/ipc_client.py`](examples/python/ipc_client.py) |
| FastDB CRM and relay client | [`examples/python/fastdb_relay_resource.py`](examples/python/fastdb_relay_resource.py), [`examples/python/fastdb_relay_client.py`](examples/python/fastdb_relay_client.py) |
| Relay mesh | [`examples/python/relay_mesh/`](examples/python/relay_mesh/) |
| FastDB bridge examples | [`examples/python/grid/`](examples/python/grid/) |

### Server-Side Monitoring

Use `cc.hold_stats()` to monitor SHM buffers held by resource methods in hold mode:

```python
stats = cc.hold_stats()
# {'active_holds': 3, 'total_held_bytes': 52428800, 'oldest_hold_seconds': 12.5}
```

---

## Architecture

**C-Two organizes distributed programs around resources.**

In scientific computation, resources encapsulating complex state and domain-specific operations need to be organized into cohesive units. We call the contracts describing these resources **Core Resource Models (CRMs)**. Applications care more about *how to interact* with resources than *where they are located*. C-Two provides location transparency and uniform resource access, so any **client** can interact with a resource as if it were a local object.

<p align="center">
  <img src="docs/images/architecture.png" alt="C-Two architecture diagram" width="100%">
</p>

### Client Layer

Any code that calls `cc.connect(...)` consumes a resource. The returned proxy provides full type safety and location transparency, so client code can use the resource without tracking its process or machine placement.

- `cc.connect(CRMClass, name='...', address='...')` returns a typed CRM proxy
- The proxy supports context management: `with cc.connect(...) as x:` auto-closes
- For IPC and relay paths, the SDK derives the expected route contract from the CRM class and native code validates the route name, CRM tag, ABI hash, and signature hash before calls are made.

### Resource Layer

Server-side stateful instances exposed through standardized CRM contracts.

- **CRM contract**: Interface class decorated with `@cc.crm()`. Only methods declared here are remotely accessible.
- **Resource**: Plain Python class implementing the contract — state + domain logic, with no decorator required.
- **FDB payloads**: Portable CRM payloads are derived from `fastdb4py` annotations and represented as FastDB call-db payload ABI refs.
- **Python pickle fallback**: Plain Python types remain usable for Python-only prototyping, but strict portable export rejects them.
- **`@cc.read` / `@cc.write`**: Concurrency annotations — parallel reads, exclusive writes.
- **`@cc.on_shutdown`**: Lifecycle callback invoked when a resource is unregistered; it stays outside the RPC surface.

### Transport Layer

Protocol-agnostic communication with automatic protocol detection based on address scheme:

| Scheme | Transport | Use case |
|--------|-----------|----------|
| `thread://` | In-process direct call | Zero serialization, testing |
| `ipc:///path` | Unix domain socket + shared memory | Multi-process, same host |
| `http://host:port` | HTTP relay | Cross-machine, web-compatible |

The IPC transport uses a **control-plane / data-plane separation**: method routing flows through UDS inline frames while payload bytes are exchanged via shared memory. Normal FastDB calls detach from transport buffers before returning to user code; `cc.hold()` and `cc.InputLifetime.BORROWED` are the explicit lifetime controls for retained response buffers and call-scoped borrowed request buffers.

### Rust Native Layer

The core runtime is language-neutral Rust, and SDKs bind to the same core contracts. Performance-critical components are implemented in Rust and exposed to Python through a native extension built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs):

The Rust workspace contains 9 core crates organized in 4 layers (foundation → protocol → transport → runtime), plus the Python PyO3 extension under `sdk/python/native/`:

- **Contract Core (`c2-contract`)** — Language-neutral CRM route contract validation and canonical descriptor hashing.
- **Buddy Allocator** — Zero-syscall shared memory allocation for the IPC transport. Cross-process, lock-free on the fast path.
- **Wire Protocol** — Frame encoding, chunk assembly, and chunk registry for large-payload lifecycle management.
- **HTTP Relay** — High-throughput [axum](https://github.com/tokio-rs/axum)-based gateway bridging HTTP to IPC. Handles connection pooling and request multiplexing.

The Rust extension is compiled automatically during `pip install c-two` (from pre-built wheels) or `uv sync` (from source).

The `c3` command is distributed as a native CLI binary and built from the root `cli/` package. Install released binaries with:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

Source checkouts can link a local development binary with `python tools/dev/c3_tool.py --build --link`; published CLI artifacts are owned by the CLI release pipeline.

Portable CRM descriptors can be exported from Python CRM classes and validated by the Rust CLI before they are used as codegen input:

```bash
uv run python -m c_two.cli.contract export mypkg.contracts:Geometry --out geometry.contract.json
c3 contract artifacts mypkg.contracts:Geometry --python .venv/bin/python --out geometry.payload-abi-artifacts.json
c3 contract diagnose mypkg.contracts:Geometry --python .venv/bin/python --pretty
c3 contract export mypkg.contracts:Geometry --python .venv/bin/python --out geometry.contract.json
c3 contract validate geometry.contract.json
```

`c3 contract diagnose` reports fastdb-first portability warnings such as Python-only pickle fallback or a non-FastDB `PayloadAbiRef` before strict cross-language workflows fail, and the Rust CLI requires Python diagnostics to be a JSON array of objects before writing them. `c3 contract artifacts` exports FastDB ABI sidecar descriptors such as `fastdb.call-db.schema.v1` and root/dependency `fastdb.schema.v1` objects; C-Two runtime route/relay/IPC/scheduler/lease layers continue to treat FastDB storage internals as opaque. Feed that artifact bundle directly to C-Two codegen with `--fastdb-schema` and `--fastdb-out`:

```bash
c3 contract codegen typescript \
  geometry.contract.json \
  --out geometry.client.ts \
  --fastdb-schema geometry.payload-abi-artifacts.json \
  --fastdb-out geometry.fastdb.ts
```

Validated descriptors can generate TypeScript clients. FastDB-backed payloads are emitted with method wire specs, route fingerprints, a codec transport factory, `createHttpRelayEncodedTransport(...)` for explicit relay URLs, and `createRelayAwareHttpEncodedTransport(...)` for contract-scoped relay resolve with a contract-keyed route cache, current-route preference, payload-limit guardrails, stale-route invalidation/re-resolve, resolve transport-error/5xx retry, data-plane transport-error classification, `maxAttempts`, `routeCacheTtlMs`, `callTimeoutMs`, `resolveTimeoutMs`, construction-time HTTP option/base URL/fetch/header validation, and reserved C-Two expected-contract header protection. Use `--strict-codecs` when CI should fail until every FastDB ABI requirement has generated TypeScript support:

```bash
c3 contract codegen typescript geometry.contract.json --out geometry.client.ts
c3 contract codegen typescript geometry.contract.json --strict-codecs
```

For resource-first projects, `infer` can build a CRM projection descriptor from explicitly selected Python resource methods. Inference exposes only selected methods, and fastdb-first portable workflows require every selected method payload to resolve to a FastDB call-db `PayloadAbiRef` or no payload. Python-native primitives and containers are useful for Python-only prototypes; they fall back to `python-pickle-default` and are rejected by portable export. Explicit non-FastDB `PayloadAbiRef` values are internal diagnostic cases. Use `c3 contract infer --diagnose` to inspect those diagnostics before exporting, and `c3 contract infer --artifacts` to export FastDB ABI artifacts from the same inferred projection for C-Two codegen.

```bash
c3 contract infer mypkg.resources:GeometryResource \
  --python .venv/bin/python \
  --namespace mypkg.geometry \
  --version 0.1.0 \
  --name Geometry \
  --method vertices \
  --method nodes \
  --diagnose \
  --pretty

c3 contract infer mypkg.resources:GeometryResource \
  --python .venv/bin/python \
  --namespace mypkg.geometry \
  --version 0.1.0 \
  --name Geometry \
  --method vertices \
  --method nodes \
  --artifacts \
  --out geometry.payload-abi-artifacts.json

c3 contract infer mypkg.resources:GeometryResource \
  --python .venv/bin/python \
  --namespace mypkg.geometry \
  --version 0.1.0 \
  --name Geometry \
  --method vertices \
  --method nodes \
  --out geometry.contract.json
```

FastDB call-db is the portable CRM payload ABI. The Python fallback path uses ordinary Python annotations and pickle for local or Python-only IPC prototypes; strict portable export/codegen rejects it. The public package surface omits optional payload registry modules and a public codec registry.

To move a method onto the portable path, use `fastdb4py` scalar aliases, `Array[...]`, `Batch[...]`, and FastDB feature classes so the method plans as FastDB call-db. Unmarked Python payloads remain available in non-portable local/IPC prototype paths through pickle; strict portable export rejects pickle and non-FastDB `PayloadAbiRef` values. Extended schema code lives in the runnable FastDB examples.

---

## Installation

### From PyPI

```bash
pip install c-two
```

Pre-built wheels are available for:
- **Linux**: x86_64, aarch64
- **macOS**: Apple Silicon (aarch64), Intel (x86_64)
- **Python**: 3.10, 3.11, 3.12, 3.13, 3.14, 3.14t (free-threading)

If no pre-built wheel is available for your platform, pip will build from source (requires a [Rust toolchain](https://rustup.rs)).

### Development Setup

```bash
git clone https://github.com/world-in-progress/c-two.git
cd c-two
cp .env.example .env               # configure environment (optional)
uv sync                            # install dependencies + compile Rust extensions
uv sync --group examples           # install examples dependencies (pandas, pyarrow)
python tools/dev/c3_tool.py --build --link  # build and link native c3 for source checkouts
uv run pytest                      # run the test suite

# Python 3.10 compatibility check. C-Two keeps 3.10 support for downstream
# stacks such as Taichi that are still pinned to that runtime.
uv python install 3.10
uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs
```

> Requires [uv](https://github.com/astral-sh/uv) and a Rust toolchain.

---

## Roadmap

| Capability | Status |
|------------|--------|
| Core RPC framework (CRM + Resource + Client) | ✅ Stable |
| IPC transport with SHM buddy allocator | ✅ Stable |
| HTTP relay (Rust-powered) | ✅ Stable |
| Relay mesh with gossip-based discovery | ✅ Stable |
| Chunked payload transfer (payloads > 256 MB) | ✅ Stable |
| Heartbeat & connection management | ✅ Stable |
| Read/write concurrency control | ✅ Stable |
| Unified config architecture (Rust resolver SSOT) | ✅ Stable |
| CI/CD & multi-platform PyPI publishing | ✅ Stable |
| Disk spill for extreme payloads | ✅ Stable |
| FastDB held response views through `cc.hold()` | ✅ Stable |
| SHM residence monitoring (`cc.hold_stats()`) | ✅ Stable |
| Contract version compatibility negotiation | 🔜 Planned |
| `auth_hook` + call metadata | 🔜 Planned |
| Dry-run hooks | 🔜 Planned |
| Async interfaces | 🔜 Planned |
| Adaptive memory lifecycle policy | 🔜 Planned |
| Streaming RPC / pipeline semantics | 🔜 Planned |
| Cross-language clients (Rust first, TypeScript later) | 🔮 Future |
| Global discovery & namespace governance | 🔮 Future |

See the [current roadmap](docs/roadmap.md) for details. Historical roadmap notes remain archived under `docs/plans/`.

---

## License

[MIT](LICENSE)

---

<p align="center">Built for resource-oriented computation. Powered by Rust.</p>
