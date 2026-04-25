# Copilot Instructions for C-Two

## Project Overview

C-Two is a **resource-oriented RPC framework** for Python that enables remote invocation of stateful resource classes across processes and machines. It is designed for distributed scientific computation — not traditional microservices.

The core abstraction is **not services, but resources**. Terminology:
- **CRM** (Core Resource Model) = the *contract* class decorated with `@cc.crm(...)`. Declares which methods are remotely callable. Method bodies are `...`.
- **Resource** = the *runtime instance* implementing a CRM contract. A plain Python class, not decorated. Named by domain semantics (`NestedGrid` implements `Grid`).
- **Client** = any code that calls `cc.connect(...)` to consume a resource. There is no separate "Component" abstraction.

## Build, Test & Run

Package manager: **uv** (not pip directly). Build backend: **maturin** (auto-compiles Rust native extensions via PyO3).

```bash
# Install dependencies + compile Rust native extension
uv sync

# Force rebuild after Rust source changes (uv sync alone may skip it)
uv sync --reinstall-package c-two

# Run the full test suite (C2_RELAY_ADDRESS= avoids env interference)
C2_RELAY_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30

# Run a single test file
uv run pytest sdk/python/tests/unit/test_wire.py -q

# Run a single test class or function
uv run pytest sdk/python/tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Rust type-check (no PyO3 link needed)
cd core && cargo check --workspace

# Rust unit tests (pure Rust, no Python linkage)
cd core && cargo test -p c2-mem -p c2-wire

# Note: c2-ffi tests need Python linkage — use `cargo check` not `cargo test`

# Run example (single-process, thread preference)
uv run python examples/python/local.py

# Run IPC example (two terminals)
uv run python examples/python/crm_process.py    # terminal 1: server
uv run python examples/python/client.py <address>  # terminal 2: client

# Run relay mesh example (three terminals)
c3 relay -b 0.0.0.0:8300                                 # terminal 1: relay
uv run python examples/python/relay_mesh/resource.py      # terminal 2: CRM server
uv run python examples/python/relay_mesh/client.py        # terminal 3: client

# Install examples dependencies (pandas, pyarrow — needed for grid example)
uv sync --group examples

# CLI tool
c3 --version
c3 relay --upstream ipc://my_server --bind 0.0.0.0:8080
```

Tests use **pytest** with a 30-second per-test timeout. Tests live under `sdk/python/tests/unit/` and `sdk/python/tests/integration/`, with shared fixtures in `sdk/python/tests/fixtures/` (see `Hello` CRM contract and `HelloImpl` resource).

## Architecture

Two-language design: Python owns domain logic (CRM + Resource + client code); Rust owns transport, memory, and wire codec. PyO3/maturin bridges them as `c_two._native`.

### 1. CRM Layer (`sdk/python/src/c_two/crm/`)
- **CRM contract**: An interface class decorated with `@cc.crm(namespace='...', version='...')` that declares which methods are remotely accessible. Only methods in the contract are exposed. Method bodies are `...` (ellipsis).
- **Resource**: A plain Python class implementing a CRM contract — holds state and domain logic. Not decorated. Name it by domain semantics (`NestedGrid`, `PostgresVectorLayer`), never by the contract name.
- **`@transferable`**: Decorator for custom data types that need to cross the wire. Automatically makes classes into dataclasses and registers `serialize`/`deserialize`/`from_buffer` as static methods. Without `@transferable`, pickle is used as fallback.
- **`from_buffer`**: Optional third method on transferable types. Provides zero-copy buffer views (e.g., `np.frombuffer`). When present, server-side auto-detects hold mode.
- **Method access**: CRM contract methods can be annotated with `@cc.read` or `@cc.write` (default: write) to control concurrency — the scheduler allows parallel reads but exclusive writes.
- **`@cc.transfer()`**: Per-method metadata decorator for CRM contract methods. Allows explicit `input`, `output` transferable and `buffer` mode ('view'|'hold'|None). Does NOT wrap the function — consumed by `@cc.crm` at class decoration time.
- **`@on_shutdown`**: Marks a single public method as shutdown callback (called when a resource is unregistered; not exposed via RPC).

### 2. Client Layer
- Any code that calls `cc.connect(CRMClass, name='...', address='...')` is a **client**. The returned proxy is a typed object matching the CRM contract.
- The proxy supports `with`: `with cc.connect(Grid, name='grid') as g: ...` auto-closes on exit.
- There is no separate `compo/` module, no `@cc.runtime.connect` decorator, and no "Component" type.

### 3. Config Layer (`sdk/python/src/c_two/config/`)

Unified configuration with Python as the single source of truth, passed through to Rust via FFI.

| File | Purpose |
|------|---------|
| `settings.py` | `C2Settings` pydantic model — all `C2_*` env vars, `.env` loading (`extra='ignore'`), relay server config |
| `ipc.py` | Frozen dataclasses: `BaseIPCConfig`, `ServerIPCConfig`, `ClientIPCConfig` + `build_server_config()` / `build_client_config()` |

Config priority chain: explicit kwargs (`cc.set_*()`) → environment variables / `.env` → class defaults.

### 4. Transport Layer (`sdk/python/src/c_two/transport/`)

The transport layer is a thin Python orchestration shell around a Rust-native core. Python handles CRM registration, scheduling, and serialization; Rust handles IPC, wire framing, SHM, and HTTP relay.

**Key files:**

| File | Purpose |
|------|---------|
| `registry.py` | SOTA API surface: `cc.register()`, `cc.connect()`, `cc.close()`, `cc.shutdown()`, `cc.set_server()`, `cc.set_client()` etc. |
| `protocol.py` | Re-export facade for Rust handshake codec — `Handshake`, `RouteInfo`, flag constants |
| `wire.py` | `MethodTable` — maps CRM contract method names to indices for wire dispatch; thin FFI wrappers |
| `server/native.py` | `NativeServerBridge` (exported as `Server`) — Python↔Rust server bridge |
| `server/scheduler.py` | Read/write-aware resource method execution scheduler |
| `server/reply.py` | `unpack_icrm_result` + `wrap_error` — resource reply handling |
| `server/hold_registry.py` | `HoldRegistry` — weakref-based tracking of hold-mode SHM buffers |
| `client/proxy.py` | `CRMProxy` — unified proxy supporting both thread-local and IPC modes |
| `client/util.py` | Standalone `ping()` and `shutdown()` — lightweight server probes via raw UDS |

**Transport modes:**
- **Thread-local** (same process): `cc.connect()` returns a zero-serialization proxy that calls resource methods directly.
- **IPC** (`ipc://`): UDS control channel + POSIX SHM data plane via Rust (`c2-ipc`, `c2-server`).
- **HTTP** (`http://`): HTTP relay for cross-machine transport via Rust (`c2-http`).

**Address priority:** `cc.set_address()` > `C2_IPC_ADDRESS` env var > auto-generated UUID path.

**Relay priority:** `cc.set_relay()` > `C2_RELAY_ADDRESS` env var > none (standalone mode).

### 5. Rust Native Layer (`core/`)

A Cargo workspace of 7 crates organized in 4 layers, compiled into a single `c_two._native` Python extension module:

| Layer | Crate | Purpose |
|-------|-------|---------|
| foundation | `c2-config` | Unified IPC configuration structs (Base/Server/Client), shared by all Rust crates |
| foundation | `c2-mem` | Buddy allocator, SHM regions, unified MemPool (buddy/dedicated/file-spill) |
| protocol | `c2-wire` | Wire protocol codec, frame encoding, ChunkAssembler, ChunkRegistry |
| transport | `c2-ipc` | Async IPC client (UDS + SHM), chunked transfer |
| transport | `c2-server` | Tokio-based UDS server with per-connection state and peer SHM lazy-open |
| transport | `c2-http` | HTTP client for relay transport + HTTP relay server (axum → IPC, behind `relay` feature) |
| bridge | `c2-ffi` | PyO3 bindings: `mem_ffi`, `wire_ffi`, `ipc_ffi`, `server_ffi`, `client_ffi`, `relay_ffi`, `http_ffi` |

**Memory subsystem (`c2-mem`):**
- Three-tier allocation: (1) Buddy SHM for small/medium, (2) Dedicated SHM for oversized, (3) File-spill for RAM-scarce fallback.
- `MemHandle` enum (`Buddy`/`Dedicated`/`FileSpill`) abstracts all three.
- SHM segment naming is deterministic: `{prefix}_b{idx:04x}` (buddy) / `{prefix}_d{idx:04x}` (dedicated). Server lazy-opens peer segments by deriving names from prefix + index — no explicit announcement required.

**Import:** `from c_two.mem import MemPool, PoolConfig, MemHandle, ChunkAssembler`

### CLI (`sdk/python/src/c_two/cli.py`)
The `c3` CLI provides `relay` (HTTP relay server) and `dev` (developer tools) commands.

**`c3 relay`** options (all support env vars via `C2Settings` / `.env`):

| Option | Env var | Default | Purpose |
|--------|---------|---------|---------|
| `--bind` | `C2_RELAY_BIND` | `0.0.0.0:8080` | HTTP listen address |
| `--seeds` | `C2_RELAY_SEEDS` | `""` | Comma-separated seed relay URLs for mesh |
| `--relay-id` | `C2_RELAY_ID` | auto UUID | Stable relay identifier |
| `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | `""` | Publicly reachable URL for peers |
| `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | `300` | IPC idle disconnect timeout (seconds) |

Priority: CLI flag → env var / `.env` → hardcoded default.

## Key Conventions

### Import Style
The package is imported as `c_two` but aliased as `cc`:
```python
import c_two as cc
```

### CRM Contract Definition Pattern
CRM contract classes are interfaces — method bodies are `...` (ellipsis). The `@cc.crm()` decorator requires `namespace` and `version` (semver string). Do **not** use an `I`-prefix on the contract class name:
```python
@cc.crm(namespace='cc.demo', version='0.1.0')
class Grid:
    def some_method(self, arg: int) -> str:
        ...
```

### Transferable Pattern
`serialize`, `deserialize`, and `from_buffer` are written as regular methods but the `TransferableMeta` metaclass converts them to `@staticmethod` automatically — do **not** add `@staticmethod` yourself:
```python
@cc.transferable
class MyData:
    value: int
    
    def serialize(data: 'MyData') -> bytes:
        ...
    def deserialize(arrow_bytes: bytes) -> 'MyData':
        ...
    def from_buffer(buf: memoryview) -> 'MyData':
        ...  # optional: zero-copy view for hold mode
```

### Transfer Decorator Pattern
`@cc.transfer()` is a metadata-only decorator applied to CRM contract methods. It attaches `__cc_transfer__` but does NOT wrap the function:
```python
@cc.crm(namespace='ns', version='0.1.0')
class MyResource:
    @cc.transfer(input=MyData, output=MyData, buffer='hold')
    def process(self, data: MyData) -> MyData: ...
```

### Hold Mode Pattern
`cc.hold()` wraps a CRM-proxy bound method for client-side SHM retention. Returns `HeldResult` with `.value` and `.release()`. Three-layer safety: explicit `.release()`, context manager, `__del__` fallback:
```python
# Context manager (single hold)
with cc.hold(proxy.method)(args) as held:
    data = held.value  # zero-copy view backed by SHM
# SHM released on context exit

# Explicit release (multiple holds)
a = cc.hold(proxy.method)(args_a)
b = cc.hold(proxy.method)(args_b)
try:
    process(a.value, b.value)
finally:
    a.release()
    b.release()
```

Buffer mode auto-detection: when `@cc.transfer(buffer=None)` (default), the framework checks if the input transferable has `from_buffer` → hold, else → view.

### Client Usage Pattern
Any code that calls `cc.connect(...)` is a client. The returned proxy is typed to the CRM contract and supports `with` for auto-close:
```python
with cc.connect(Grid, name='grid', address='ipc://server') as grid:
    result = grid.subdivide_grids([1], [0])
# proxy auto-closed on exit
```

### SOTA API Pattern

The registry exposes a flat top-level API on the `cc` namespace:

```python
import c_two as cc

# Server side
cc.set_address('ipc://my_server')                       # optional: explicit address
cc.set_relay('http://relay-host:8080')                   # optional: relay for name resolution
cc.set_server(pool_segment_size=2*1024*1024*1024)        # optional: tune IPC config
cc.register(Grid, grid_instance, name='grid')           # register a Grid resource
cc.register(Network, net_instance, name='network')      # multiple resources in one process
cc.serve()                                               # block until Ctrl-C

# Client side (same process → thread preference, zero serde)
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side (remote process → IPC, direct address)
cc.set_client(pool_segment_size=2*1024*1024*1024)        # optional: tune client config
grid = cc.connect(Grid, name='grid', address='ipc://my_server')
grid.some_method(arg)
cc.close(grid)

# Client side (relay-based name resolution)
cc.set_relay('http://relay-host:8080')
grid = cc.connect(Grid, name='grid')                    # relay resolves name → IPC address
grid.some_method(arg)
cc.close(grid)

# Cleanup
cc.unregister('grid')
cc.shutdown()
```

The `name` parameter in `cc.register()` is a user-chosen routing key — it is **not** the CRM namespace. Multiple resources using different CRM contracts (or even the same CRM contract with different instances) can coexist under distinct names.

### Hold Stats API
```python
# Server-side monitoring of hold-mode SHM buffers
stats = cc.hold_stats()
# {'active_holds': N, 'total_held_bytes': M, 'oldest_hold_seconds': T}
```

### Error Handling
Errors are modeled as `CCError` subclasses with numeric `ERROR_Code` values. Errors serialize to/from bytes for wire transfer. Error classes are named by location: `ResourceDeserializeInput`, `ClientSerializeInput`, `ResourceExecuteFunction`, `ClientCallResource`, etc. Enum values live under `ERROR_AT_RESOURCE_*` and `ERROR_AT_CLIENT_*`.

### Naming
- CRM contract classes: plain names, **no `I` prefix** (e.g., `Grid`, not `IGrid`)
- Resource (implementation) classes: named by domain semantics — `NestedGrid`, `PostgresVectorLayer`, or the fallback `{ContractName}Impl` pattern used by the generator
- Transferable classes: descriptive data names (e.g., `GridAttribute`, `GridSchema`)
- Routing names: user-chosen strings passed to `cc.register(name=...)` and `cc.connect(name=...)` — distinct from the CRM `namespace`

### Performance-Sensitive Code
Wire codec and transport code in Rust (`c2-wire`, `c2-ipc`, `c2-mem`) prioritize zero-copy and single-allocation patterns. The Python `wire.py` retains `MethodTable` (method-name → index mapping) used by `NativeServerBridge` for dispatch, `payload_total_size` (scatter-write aware), and thin wrappers for `decode_call_control`/`encode_reply_control`/`decode_reply_control` that preserve Python default arguments (`offset=0`, `error_data=None`). The `thread://` transport skips serialization entirely. SHM segments use deterministic naming for lazy peer-side opening — no explicit segment announcement protocol is needed. The buddy allocator's `alloc()`/`free_at()` are thread-safe.

### Environment Variables
| Variable | Purpose | Default |
|----------|---------|---------|
| `C2_IPC_ADDRESS` | Override auto-generated IPC server address | auto UUID |
| `C2_RELAY_ADDRESS` | HTTP relay URL for CRM registration and client name resolution | (none) |
| `C2_RELAY_BIND` | Relay HTTP listen address (`c3 relay --bind`) | `0.0.0.0:8080` |
| `C2_RELAY_ID` | Stable relay identifier for mesh protocol | auto UUID |
| `C2_RELAY_ADVERTISE_URL` | Publicly reachable URL announced to mesh peers | (none) |
| `C2_RELAY_SEEDS` | Comma-separated seed relay URLs for mesh mode | (none) |
| `C2_RELAY_IDLE_TIMEOUT` | Upstream IPC idle disconnect timeout in seconds | `300` |
| `C2_RELAY_ANTI_ENTROPY_INTERVAL` | Anti-entropy digest exchange interval in seconds | `60.0` |
| `C2_SHM_THRESHOLD` | Payload size threshold for SHM vs inline | 4096 (4 KB) |
| `C2_IPC_POOL_SEGMENT_SIZE` | Buddy pool segment size in bytes | 268435456 (256 MB) |
| `C2_IPC_MAX_POOL_SEGMENTS` | Max buddy pool segments (1–255) | 4 |
| `C2_IPC_MAX_POOL_MEMORY` | Max total pool memory in bytes | segment_size × max_segments |
| `C2_IPC_POOL_DECAY_SECONDS` | Idle segment decay time | 60.0 |
| `C2_IPC_MAX_FRAME_SIZE` | Max inline frame size | 131072 (128 KB) |
| `C2_IPC_MAX_PAYLOAD_SIZE` | Max single-call payload size | 2147483648 (2 GB) |
| `C2_IPC_HEARTBEAT_INTERVAL` | Heartbeat interval seconds (0 disables) | 30.0 |
| `C2_ENV_FILE` | Path to `.env` file; empty string disables | `.env` |

All `C2_*` variables can be set in `.env` (loaded via pydantic-settings). See `.env.example` for the full reference.

## Python Version

Requires Python ≥ 3.10. Uses modern type hints (`list[int]`, `str | None`, `tuple[...]`). Free-threading (3.14t) is a target platform — be cautious with C extensions and GIL assumptions.
