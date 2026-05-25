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

- **Resources, not services** — C-Two does not expose RPC endpoints. It exposes stateful resource objects through language SDKs. The Python SDK makes Python classes remotely accessible while preserving their object-oriented nature.

- **Zero-copy from process to data** — Same-process calls skip serialization entirely. Cross-process IPC can hold shared-memory buffers alive, letting you read columnar data (NumPy, Arrow, …) directly from SHM — no deserialization, no copies.

- **Built for scientific workloads** — The Python SDK has native support for Apache Arrow, NumPy arrays, and large payloads (chunked payload transfer for data beyond 256 MB). Designed for computational workloads, not microservices.

- **Rust-powered core** — Shared transport, memory, wire codec, route-contract validation, relay, and configuration live in Rust so future SDKs reuse one runtime contract.

---

## Performance

End-to-end cross-process IPC benchmark — same NumPy payload (`row_id u32` + `x,y,z f64`), same machine, same aggregation. Three transport modes compared:

| Rows | C-Two hold (ms) | Ray (ms) | C-Two pickle (ms) | **Hold vs Ray** |
|-----:|---:|---:|---:|---:|
| 1 K | **0.07** | 6.1 | 0.19 | **86×** |
| 10 K | **0.09** | 7.1 | 0.82 | **79×** |
| 100 K | **0.38** | 9.8 | 8.7 | **26×** |
| 1 M | **3.7** | 58 | 150 | **15×** |
| 3 M | **9.7** | 129 | 598 | **13×** |

- **C-Two hold** — SHM zero-copy via `np.frombuffer`; no serialization on read
- **Ray** — object store with zero-copy numpy support (Ray 2.55)
- **C-Two pickle** — standard pickle over SHM; included to show serialization cost

> Apple M1 Max · Python 3.13 · NumPy 2.4 · See [`sdk/python/benchmarks/unified_numpy_benchmark.py`](sdk/python/benchmarks/unified_numpy_benchmark.py) for full methodology.

---

## Quick Start

```bash
pip install c-two
```

### Define a resource contract and its implementation

```python
import c_two as cc

# CRM contract — declares which methods are remotely accessible
@cc.crm(namespace='demo.counter', version='0.1.0')
class Counter:
    def increment(self, amount: int) -> int: ...

    @cc.read
    def value(self) -> int: ...

    def reset(self) -> int: ...


# Resource — a plain Python class implementing the contract
class CounterImpl:
    def __init__(self, initial: int = 0):
        self._value = initial

    def increment(self, amount: int) -> int:
        self._value += amount
        return self._value

    def value(self) -> int:
        return self._value

    def reset(self) -> int:
        old = self._value
        self._value = 0
        return old
```

### Use it locally (zero serialization)

```python
cc.register(Counter, CounterImpl(initial=100), name='counter')
counter = cc.connect(Counter, name='counter')

counter.increment(10)    # → 110
counter.value()          # → 110
counter.reset()          # → 110 (returns old value)
counter.value()          # → 0

cc.close(counter)
```

### Or remotely — same API, relay discovery

```python
# Server process
cc.set_relay_anchor('http://relay-host:8080')
cc.register(Counter, CounterImpl(), name='counter')

# Client process (separate terminal)
cc.set_relay_anchor('http://relay-host:8080')
counter = cc.connect(Counter, name='counter')
counter.increment(5)     # works identically
cc.close(counter)
```

> **See [`examples/python/`](examples/python/) for complete runnable demos.**

---

## Core Concepts

### CRM — Contract

A **CRM** (Core Resource Model) declares *which* methods a remote resource exposes. It's decorated with `@cc.crm()`, and method bodies are `...` (pure interface — no implementation).

```python
@cc.crm(namespace='demo.greeter', version='0.1.0')
class Greeter:
    @cc.read                              # concurrent reads allowed
    def greet(self, name: str) -> str: ...

    @cc.read
    def language(self) -> str: ...
```

Methods can be annotated with `@cc.read` (concurrent access allowed) or left as default write (exclusive access).

### Resource — Runtime Instance

A **resource** is a plain Python class that implements a CRM contract. It holds state and domain logic. It is **not** decorated — the framework discovers its methods through the CRM contract it was registered under. Name the class by what it *is* (`GreeterImpl`, `PostgresGreeter`, `MultilingualGreeter`), not the interface.

```python
class GreeterImpl:
    def __init__(self, lang: str = 'en'):
        self._lang = lang
        self._templates = {'en': 'Hello, {}!', 'zh': '你好, {}!'}

    def greet(self, name: str) -> str:
        return self._templates.get(self._lang, 'Hi, {}!').format(name)

    def language(self) -> str:
        return self._lang
```

### Client — Consumer

Anything that calls `cc.connect(...)` is a **client** (or consumer / application code). The returned proxy is location-transparent — it works the same whether the resource lives in the same process or on a remote machine.

```python
greeter = cc.connect(Greeter, name='greeter')
greeter.greet('World')     # → 'Hello, World!'
cc.close(greeter)

# Or with context manager:
with cc.connect(Greeter, name='greeter') as greeter:
    greeter.greet('World')
```

### Server — Resource Host

A **server** is any process that calls `cc.register(...)` to host one or more resources, then usually `cc.serve()` to block on the request loop. One server process can host many resources (each under a unique `name`), and will auto-bind an IPC endpoint the first time a resource is registered.

```python
import c_two as cc

cc.register(Greeter, GreeterImpl(), name='greeter')
cc.register(Counter, CounterImpl(), name='counter')
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

The Python SDK does not embed or start the relay server; start `c3 relay`, Docker Compose, or your orchestrator separately, then point Python code at its relay anchor with `C2_RELAY_ANCHOR_ADDRESS` or `cc.set_relay_anchor()`. The anchor is the control-plane registration/name-resolution endpoint. Remote HTTP calls still go directly to the resolved route's `relay_url`; local direct IPC is used only when the anchor endpoint is loopback/local. Relay-aware clients preflight routes before the first call and re-resolve structured stale-route responses; set `C2_RELAY_ROUTE_MAX_ATTEMPTS` to tune the maximum route acquisition attempts (default `3`, valid range `1..=32`, `0` is treated as `1`). Set `C2_RELAY_CALL_TIMEOUT` to tune CRM call timeout seconds (default `300`; `0` disables the reqwest total timeout). Set `C2_REMOTE_PAYLOAD_CHUNK_SIZE` to tune C-Two remote payload batching for relay HTTP and future remote protocols (default `1048576`; max `134217728`). This is not a TCP packet, HTTP/1 chunk, or HTTP/2 DATA frame guarantee. Ambiguous data-plane failures are not replayed. Relay resolve, probe, and call paths reject name-only lookups and CRM contract mismatches instead of falling back to an untyped route with the same name.

```bash
# Start a relay anywhere reachable on your network
c3 relay --bind 0.0.0.0:8080
```

Relay HTTP and mesh endpoints are intended for a trusted network boundary. Do not expose them directly to the public internet; production deployments should restrict access with infrastructure such as private networking, firewalls, Kubernetes NetworkPolicy, service mesh policy, or ingress authentication.

```python
# Server side — announce resources to the relay
cc.set_relay_anchor('http://relay-host:8080')
cc.register(MeshStore, MeshStoreImpl(), name='mesh')
cc.serve()

# Client side — resolve by name plus the MeshStore CRM contract, no address needed
cc.set_relay_anchor('http://relay-host:8080')
mesh = cc.connect(MeshStore, name='mesh')
```

Multiple relays can form a **mesh cluster** via gossip — any relay can resolve any resource registered anywhere in the mesh. See the [Relay Mesh example](#relay-mesh--multi-relay-clusters) below.

> **When do I need a relay?** Only for cross-machine or name-and-contract-based discovery. Same-process and same-host (IPC) usage work without any relay.

### Payload Model — FDB First

C-Two CRM payload planning has three internal outcomes: `FDB`, `PYTHON_PICKLE`, and `NO_PAYLOAD`. Portable, cross-language contracts use FastDB call-db payload ABI refs derived from `fastdb4py` annotations. Plain Python annotations still run through Python pickle fallback for Python-only prototyping, but strict portable export rejects those methods.

Use the FastDB grid example when authoring portable payloads:

```python
import c_two as cc
import fastdb4py as fdb

@fdb.feature
class GridCell:
    global_id: fdb.I32
    level: fdb.I32

@cc.crm(namespace='demo.grid', version='0.1.0')
class Grid:
    def get_cells(self, level: fdb.I32, ids: fdb.Array[fdb.I32]) -> fdb.Batch[GridCell]:
        ...
```

For Python-only resources, use ordinary Python types and dataclasses. They are convenient locally, but they are intentionally diagnosed as nonportable by contract export.

### cc.hold() — Client-Side Borrowed Responses

On the client side, normal FastDB CRM calls materialize the response into owned values and release the transport buffer immediately. `cc.hold()` explicitly requests that the response buffer remain alive, enabling zero-copy reads when an FDB output payload supports retained views. The returned `cc.Held[R]` wraps the CRM logical return value, exposes the retained raw wire buffer as `.buffer` for advanced use, and provides a three-layer safety net for buffer lifecycle:

1. **Explicit `.release()`** — preferred for complex workflows holding multiple buffers
2. **Context manager (`with`)** — recommended for single-buffer scopes
3. **`__del__` fallback** — last resort, emits `ResourceWarning` if you forget to release

```python
grid = cc.connect(Compute, name='compute', address='ipc://server')

# Normal call — buffer released immediately after deserialize
result = grid.transform(matrix)

# Option 1: Context manager — clean for single holds
with cc.hold(grid.transform)(matrix) as held:
    data = held.value          # zero-copy NumPy array backed by SHM
    raw = held.buffer          # retained wire buffer, valid only inside the hold scope
    process(data)              # read directly from shared memory
# SHM buffer released on context exit

# Option 2: Explicit release — better for multiple concurrent holds
a = cc.hold(grid.transform)(matrix_a)
b = cc.hold(grid.transform)(matrix_b)
try:
    combined = np.concatenate([a.value.data, b.value.data])
    process(combined)
finally:
    a.release()
    b.release()
```

> **When to use hold mode:** Large array/columnar data where deserialization dominates cost. For small payloads (< 1 MB), the overhead of tracking SHM lifecycle exceeds the copy cost.

---

### InputLifetime — Server-Side Borrowed Inputs

On the server side, FastDB inputs are materialized by default before the resource method is called. Use `input_lifetime={...}` only when the resource method has the same FDB input signature as the CRM and is prepared to treat the value as call-scoped borrowed data.

```python
cc.register(
    GridFastdb,
    grid_resource,
    name='grid',
    input_lifetime={
        'get_grid_infos': cc.InputLifetime.BORROWED,
    },
)
```

`cc.InputLifetime.BORROWED` is valid only for FDB payloads with buffer-view support. It cannot be combined with `bridge.input`, and any data retained after the method returns must be copied first with `fastdb4py.materialize(value)` or `value.to_owned()`.

## Examples

### Single Process — Thread Preference

When `cc.connect()` targets a CRM registered in the same process, the proxy calls methods directly with **zero serialization overhead**.

```python
import c_two as cc

cc.register(Greeter, GreeterImpl(lang='en'), name='greeter')
cc.register(Counter, CounterImpl(initial=100), name='counter')

greeter = cc.connect(Greeter, name='greeter')
counter = cc.connect(Counter, name='counter')

print(greeter.greet('World'))    # → Hello, World!
print(counter.value())           # → 100
counter.increment(10)

cc.close(greeter)
cc.close(counter)
cc.shutdown()
```

> **Best for:** local prototyping, testing, single-machine computation.

### Multi-Process IPC — FDB Payloads

Separate server and client processes communicate over Unix domain sockets with shared memory. For portable structured payloads, define the CRM with `fastdb4py` annotations and let C-Two derive the FastDB call-db payload ABI. See `examples/python/grid/grid_fdb_crm.py`, `examples/python/fastdb_relay_resource.py`, and `examples/python/fastdb_relay_client.py` for a complete FDB-first resource and client.

> **Best for:** multi-process on same host, worker isolation, high-throughput local IPC.

### Cross-Machine — HTTP Relay

An HTTP relay bridges network requests to CRM processes running on IPC. CRM processes register with the relay, and clients discover resources by **route name plus expected CRM contract**. The CRM tag and contract hashes prevent accidental or stale matches when a relay mesh contains an old route or another resource with the same name.

**CRM Server** (`resource.py`):
```python
import c_two as cc

cc.set_relay_anchor('http://relay-host:8080')
cc.register(MeshStore, MeshStoreImpl(), name='mesh')
cc.serve()  # blocks until Ctrl-C
```

**Relay** — start via the `c3` CLI:
```bash
# Bind address from CLI flag, env var C2_RELAY_BIND, or .env file
c3 relay --bind 0.0.0.0:8080
```

Expose relay HTTP only inside a trusted deployment boundary. The relay mesh protocol assumes infrastructure-level access control, not public internet reachability.

**Client** (`client.py`):
```python
import c_two as cc

cc.set_relay_anchor('http://relay-host:8080')
mesh = cc.connect(MeshStore, name='mesh')  # relay resolves the name
mesh.get_mesh()
cc.close(mesh)
```

> **Best for:** network-accessible services, web integration, cross-machine deployment.

### Relay Mesh — Multi-Relay Clusters

Multiple relays form a **mesh network** with gossip-based route propagation. CRMs register with their local relay; clients discover resources across the entire cluster.

```bash
# Start relay A (seeds point to peer relays for auto-join)
c3 relay --bind 0.0.0.0:8080 --relay-id relay-a \
    --advertise-url http://relay-a:8080 --seeds http://relay-b:8080

# Start relay B
c3 relay --bind 0.0.0.0:8080 --relay-id relay-b \
    --advertise-url http://relay-b:8080 --seeds http://relay-a:8080
```

Mesh peer endpoints (`/_peer/*`) accept route gossip from configured peers and must be protected by the same trusted network boundary as the relay HTTP API.

CRM processes register with their local relay; the mesh propagates routes automatically. Clients can connect through **any** relay in the mesh using the same CRM class and route name.

> **Best for:** multi-node clusters, high availability, geographic distribution.

> **See [`examples/python/relay_mesh/`](examples/python/relay_mesh/) for a complete runnable mesh demo.**

### Server-Side Monitoring

Use `cc.hold_stats()` to monitor SHM buffers held by resource methods in hold mode:

```python
stats = cc.hold_stats()
# {'active_holds': 3, 'total_held_bytes': 52428800, 'oldest_hold_seconds': 12.5}
```

---

## Architecture

**The design philosophy of C-Two is not to define services, but to empower resources.**

In scientific computation, resources encapsulating complex state and domain-specific operations need to be organized into cohesive units. We call the contracts describing these resources **Core Resource Models (CRMs)**. Applications care more about *how to interact* with resources than *where they are located*. C-Two provides location transparency and uniform resource access, so any **client** can interact with a resource as if it were a local object.

<p align="center">
  <img src="docs/images/architecture.png" alt="C-Two architecture diagram" width="100%">
</p>

### Client Layer

Any code that calls `cc.connect(...)` to consume a resource. The returned proxy provides full type safety and location transparency — clients don't know (or care) where the resource is running.

- `cc.connect(CRMClass, name='...', address='...')` returns a typed CRM proxy
- The proxy supports context management: `with cc.connect(...) as x:` auto-closes
- For IPC and relay paths, the SDK derives the expected route contract from the CRM class and native code validates the route name, CRM tag, ABI hash, and signature hash before calls are made.

### Resource Layer

Server-side stateful instances exposed through standardized CRM contracts.

- **CRM contract**: Interface class decorated with `@cc.crm()`. Only methods declared here are remotely accessible.
- **Resource**: Plain Python class implementing the contract — state + domain logic. Not decorated.
- **FDB payloads**: Portable CRM payloads are derived from `fastdb4py` annotations and represented as FastDB call-db payload ABI refs.
- **Python pickle fallback**: Plain Python types remain usable for Python-only prototyping, but strict portable export rejects them.
- **`@cc.read` / `@cc.write`**: Concurrency annotations — parallel reads, exclusive writes.
- **`@cc.on_shutdown`**: Lifecycle callback invoked when a resource is unregistered (not exposed via RPC).

### Transport Layer

Protocol-agnostic communication with automatic protocol detection based on address scheme:

| Scheme | Transport | Use case |
|--------|-----------|----------|
| `thread://` | In-process direct call | Zero serialization, testing |
| `ipc:///path` | Unix domain socket + shared memory | Multi-process, same host |
| `http://host:port` | HTTP relay | Cross-machine, web-compatible |

The IPC transport uses a **control-plane / data-plane separation**: method routing flows through UDS inline frames while payload bytes are exchanged via shared memory — zero-copy on the data path. Normal FastDB calls materialize input and output values before releasing buffers; `cc.hold()` and `cc.InputLifetime.BORROWED` are the explicit lifetime controls for retained response buffers and call-scoped borrowed request buffers.

### Rust Native Layer

The core runtime is language-neutral Rust; SDKs bind to the same core contracts rather than reimplement transport behavior. Performance-critical components are implemented in Rust and exposed to Python through a native extension built with [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs):

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

Source checkouts can link a local development binary with `python tools/dev/c3_tool.py --build --link`; published CLI artifacts are owned by the CLI release pipeline rather than by any language SDK.

Portable CRM descriptors can be exported from Python CRM classes and validated by the Rust CLI before they are used as codegen input:

```bash
uv run python -m c_two.cli.contract export mypkg.contracts:Grid --out grid.contract.json
c3 contract artifacts mypkg.contracts:Grid --python .venv/bin/python --out grid.payload-abi-artifacts.json
c3 contract diagnose mypkg.contracts:Grid --python .venv/bin/python --pretty
c3 contract export mypkg.contracts:Grid --python .venv/bin/python --out grid.contract.json
c3 contract validate grid.contract.json
```

`c3 contract diagnose` reports fastdb-first portability warnings such as Python-only pickle fallback or a non-FastDB `PayloadAbiRef` before strict cross-language workflows fail, and the Rust CLI requires Python diagnostics to be a JSON array of objects before writing them. `c3 contract artifacts` exports FastDB ABI sidecar descriptors such as `fastdb.call-db.schema.v1` and root/dependency `fastdb.schema.v1` objects without making C-Two runtime route/relay/IPC/scheduler/lease layers parse FastDB storage internals. Feed that artifact bundle directly to C-Two codegen with `--fastdb-schema` and `--fastdb-out`:

```bash
c3 contract codegen typescript \
  grid.contract.json \
  --out grid.client.ts \
  --fastdb-schema grid.payload-abi-artifacts.json \
  --fastdb-out grid.fastdb.ts
```

Validated descriptors can generate TypeScript clients. FastDB-backed payloads are emitted with method wire specs, route fingerprints, a codec transport factory, `createHttpRelayEncodedTransport(...)` for explicit relay URLs, and `createRelayAwareHttpEncodedTransport(...)` for contract-scoped relay resolve with a contract-keyed route cache, current-route preference, payload-limit guardrails, stale-route invalidation/re-resolve, resolve transport-error/5xx retry, data-plane transport-error classification, `maxAttempts`, `routeCacheTtlMs`, `callTimeoutMs`, `resolveTimeoutMs`, construction-time HTTP option/base URL/fetch/header validation, and reserved C-Two expected-contract header protection. Use `--strict-codecs` when CI should fail until every FastDB ABI requirement has generated TypeScript support:

```bash
c3 contract codegen typescript grid.contract.json --out grid.client.ts
c3 contract codegen typescript grid.contract.json --strict-codecs
```

For resource-first projects, `infer` can build a CRM projection descriptor from explicitly selected Python resource methods. Inference does not expose all public methods, and fastdb-first portable workflows still require every selected method payload to resolve to a FastDB call-db `PayloadAbiRef` or no payload. Python-native primitives and containers are useful for Python-only prototypes, but they fall back to `python-pickle-default` and are rejected by portable export; explicit non-FastDB `PayloadAbiRef` values are internal diagnostic cases, not a public extension path. Use `c3 contract infer --diagnose` to inspect those diagnostics before exporting, and `c3 contract infer --artifacts` to export FastDB ABI artifacts from the same inferred projection for C-Two codegen.

```bash
c3 contract infer mypkg.resources:GridResource \
  --python .venv/bin/python \
  --namespace mypkg.grid \
  --version 0.1.0 \
  --name Grid \
  --method get_schema \
  --method subdivide_grids \
  --diagnose \
  --pretty

c3 contract infer mypkg.resources:GridResource \
  --python .venv/bin/python \
  --namespace mypkg.grid \
  --version 0.1.0 \
  --name Grid \
  --method get_schema \
  --method subdivide_grids \
  --artifacts \
  --out grid.payload-abi-artifacts.json

c3 contract infer mypkg.resources:GridResource \
  --python .venv/bin/python \
  --namespace mypkg.grid \
  --version 0.1.0 \
  --name Grid \
  --method get_schema \
  --method subdivide_grids \
  --out grid.contract.json
```

FastDB call-db is the portable CRM payload ABI. The Python fallback path uses ordinary Python annotations and pickle for local or Python-only IPC prototypes; it is intentionally rejected by strict portable export/codegen. C-Two no longer ships optional payload registry modules or a public codec registry.

```python
import fastdb4py as fdb

@fdb.feature
class GridAttribute:
    level: fdb.I32
    global_id: fdb.I32
    activate: fdb.BOOL
```

To move a method onto the portable path, use `fastdb4py` scalar aliases, `Array[...]`, `Batch[...]`, and `@fdb.feature` so the method plans as FastDB call-db. Unmarked Python payloads can still use pickle in non-portable local/IPC prototype paths, but strict portable export rejects pickle and non-FastDB `PayloadAbiRef` values.

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

| Feature | Status |
|---------|--------|
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
| Hold mode with `from_buffer` zero-copy | ✅ Stable |
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
