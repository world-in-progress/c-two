# AGENTS.md

This file contains repository-specific guidance for Codex and other coding agents working on C-Two. Follow these instructions together with the user's current request.

## 0.x Development Constraint

C-Two is currently in the 0.x line. Do not preserve backwards compatibility for incorrect, experimental, or superseded internal APIs unless the user explicitly asks for a compatibility window. Prefer clean cuts over compatibility shims: remove obsolete code paths, tests, docs, and module surfaces rather than leaving dangling fallback behavior or zombie modules that future maintainers must carry. This applies to agent work as well: when a mechanism moves from an SDK into the Rust core, update or remove stale SDK-side guidance in the same change instead of documenting both paths as equally supported.

## Project Overview

C-Two is a resource-oriented RPC runtime that enables remote invocation of stateful resource classes across processes and machines. It is designed for distributed scientific computation, not traditional microservices.

The core abstraction is resources, not services.

- CRM (Core Resource Model): the contract class decorated with `@cc.crm(...)`. It declares remotely callable methods. Method bodies are `...`.
- Resource: the runtime instance implementing a CRM contract. It is a plain Python class, not decorated. Name it by domain semantics.
- Client: any code that calls `cc.connect(...)` to consume a resource. There is no separate "Component" abstraction.

## Build, Test, And Run

Package manager: `uv`, not direct `pip`. Build backend: `maturin`, which compiles the Rust native extension through PyO3.

```bash
# Install dependencies and compile the Rust native extension
uv sync

# Force rebuild after Rust source changes
uv sync --reinstall-package c-two

# Run the full Python test suite. Empty C2_RELAY_ANCHOR_ADDRESS avoids env interference.
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30

# Ensure the Python 3.10 compatibility syntax check executes instead of
# skipping. Keep 3.10 coverage because downstream Taichi-based stacks can still
# be pinned to Python 3.10.
uv python install 3.10
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs

# Run one test file
uv run pytest sdk/python/tests/unit/test_wire.py -q

# Run one test class or function
uv run pytest sdk/python/tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Rust core tests
cargo test --manifest-path core/Cargo.toml --workspace

# Python SDK native extension and tests
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30

# Single-process example with thread preference
uv run python examples/python/local.py

# IPC example, two terminals
uv run python examples/python/crm_process.py
uv run python examples/python/client.py <address>

# Relay mesh example, three terminals
python tools/dev/c3_tool.py --build --link
c3 relay --bind 0.0.0.0:8300
uv run python examples/python/relay_mesh/resource.py
uv run python examples/python/relay_mesh/client.py

# Example dependencies such as pandas and pyarrow
uv sync --group examples

# CLI tool
c3 --version
c3 relay --upstream grid=my_server@ipc://my_server --bind 0.0.0.0:8080
```

Tests use `pytest` with a 30-second per-test timeout. Tests live under `sdk/python/tests/unit/` and `sdk/python/tests/integration/`, with shared fixtures under `sdk/python/tests/fixtures/`. The minimum-supported-Python syntax test discovers `python3.10` or `uv python find 3.10`; run `uv python install 3.10` before the suite if you need to guarantee it does not skip. For broader 3.10 smoke coverage without replacing the default `.venv`, use a separate environment:

```bash
UV_PROJECT_ENVIRONMENT=.venv-py310 C2_RELAY_ANCHOR_ADDRESS= uv run --python 3.10 pytest sdk/python/tests/unit/test_python_examples_syntax.py -q --timeout=30 -rs
```

## Architecture

C-Two has a language-neutral Rust core and language SDKs. Python is the current SDK surface, not the canonical home for generic runtime mechanisms. The Python SDK owns Python domain logic, CRM contracts, Python resource invocation, serialization orchestration, and same-process direct-call glue. Rust owns shared transport, memory, wire codec, CRM route contract validation and fingerprints, route concurrency enforcement and state, HTTP relay, and configuration resolution. PyO3/maturin bridges Rust into Python as `c_two._native`.

### CRM Layer

Path: `sdk/python/src/c_two/crm/`

- CRM contracts are interface classes decorated with `@cc.crm(namespace='...', version='...')`.
- Only methods in the contract are exposed remotely.
- CRM route contracts are identified by route name plus the CRM namespace, CRM name, CRM version, ABI hash, and signature hash. Python may compute the descriptor/fingerprints from the CRM class, but Rust `c2-contract` validates the complete expected route contract at IPC and relay boundaries.
- Resource implementations are plain Python classes and are not decorated.
- `@transferable` marks custom data types that cross the wire. It converts the class into a dataclass and registers `serialize`, `deserialize`, and optional `from_buffer`.
- `from_buffer` enables zero-copy buffer views for hold mode.
- CRM methods can use `@cc.read` or `@cc.write`; writes are the default.
- `@cc.transfer()` is metadata-only for CRM contract methods. It does not wrap the function.
- `@on_shutdown` marks one public method as a shutdown callback. It is not exposed through RPC.

### Client Layer

Any code that calls `cc.connect(CRMClass, name='...', address='...')` is a client. The returned proxy is a typed object matching the CRM contract and supports context-manager close:

```python
with cc.connect(Grid, name='grid', address='ipc://server') as grid:
    result = grid.subdivide_grids([1], [0])
```

There is no separate `compo/` module, no `@cc.runtime.connect` decorator, and no "Component" type.

### Config Layer

Paths: `sdk/python/src/c_two/config/`, `core/foundation/c2-config/`

Unified configuration is resolved by the Rust `c2-config` resolver. Python stores SDK code-level overrides and asks the native resolver for environment, `.env`, and default values. Relay server configuration belongs to the standalone Rust `c3 relay` runtime.

| File | Purpose |
| --- | --- |
| `settings.py` | `C2Settings` facade for SDK code overrides such as `cc.set_relay_anchor()` and process transport policy |
| `ipc.py` | Typed override schemas: `BaseIPCOverrides`, `ServerIPCOverrides`, `ClientIPCOverrides`; Rust owns defaults and validation |

IPC override key validation belongs to Rust/native config parsing. Python `ipc.py` may expose typed override schemas and forward mappings, but it must not keep allowed-key or forbidden-key validation tables.

Config priority chain:

```text
explicit kwargs / cc.set_*() > process environment / .env > Rust defaults
```

Do not reintroduce Python-side default validation for IPC or relay internals. SDKs should provide typed override facades and leave environment/default resolution to Rust.

Scheduler-related config follows the same boundary. Python may expose `ConcurrencyConfig` and SDK-level enums, but Rust now owns the resolved route concurrency handle, including mode, `max_pending`, `max_workers`, and close state. Python must pass the full typed config into Rust, then treat the native handle as the source of truth for both same-process direct calls and remote dispatch. Do not keep a second Python-owned scheduler state or hidden default policy alive after registration.

Runtime-session config follows the same ownership rule. Rust `c2-runtime::RuntimeSession` owns process server identity, canonical `ipc://` address derivation, server IPC override storage/projection, direct IPC client acquire/release, client IPC config projection/freeze, route registration transactions, unregister/shutdown transaction outcomes, relay projection, relay-backed name resolution, explicit HTTP relay contract validation, and low-level HTTP client pool projection. Python may expose typed override facades and forward them into the native session, but must not keep separate `_server_ipc_overrides`, `_client_config`, `_client_ipc_overrides`, `_pool_config_applied`, server-id, server-address, direct `RustClientPool` or `RustHttpClientPool` authority, `_http_pool`, `_rollback_registration`, relay control-client caches, or independent route unregister/shutdown ordering authority in `registry.py`. Python still owns Python CRM local bindings and invokes `@on_shutdown` callbacks exactly once from native structured outcomes.

### Transport Layer

Path: `sdk/python/src/c_two/transport/`

The Python transport layer is a thin orchestration shell around a Rust-native core. Python handles CRM registration, Python callback dispatch, serialization orchestration, and same-process direct-call glue. Rust handles IPC, wire framing, SHM, response allocation/transport selection, route concurrency enforcement, runtime session state, HTTP relay transport, and relay-aware route fallback. Response allocation must enforce native IPC payload limits before allocating owned fallback buffers, skip buddy SHM when buddy wire metadata cannot represent the payload, and use checked chunked fallback rather than truncating frame or chunk metadata.

| File | Purpose |
| --- | --- |
| `registry.py` | SOTA API: `cc.register()`, `cc.connect()`, `cc.close()`, `cc.shutdown()`, `cc.set_server()`, `cc.set_client()` |
| `protocol.py` | Re-export facade for Rust handshake codec, route info, and flag constants |
| `wire.py` | `MethodTable` and thin FFI wrappers for wire control functions |
| `server/native.py` | `NativeServerBridge` exported as `Server` |
| `server/scheduler.py` | `ConcurrencyConfig` facade plus thin native route-concurrency adapter for same-process direct calls |
| `server/reply.py` | `unpack_icrm_result` and `wrap_error` |
| `client/proxy.py` | `CRMProxy` for thread-local, IPC, and HTTP modes |
| `client/util.py` | Thin native-backed `ping()` and `shutdown()` probes for direct IPC |

`registry.py` asks native `RuntimeSession.ensure_server_bridge()` for the server bridge, `RuntimeSession.acquire_ipc_client()` for direct IPC clients, `RuntimeSession.connect_explicit_relay_http()` for explicit HTTP relay clients, and `RuntimeSession.connect_via_relay()` for relay name resolution. It consumes native unregister/shutdown outcomes before mutating Python local bindings. Do not reintroduce `_relay_control_client`, `_relay_control_address`, `_relay_control_client_for()`, `_http_pool`, direct `RustHttpClientPool.instance()`, or direct construction of `RustRelayAwareHttpClient` in `registry.py`.

Transport modes:

- Thread-local: same-process `cc.connect()` returns a zero-serialization proxy. It passes Python objects directly and may use the native route-concurrency handle through a thin Python adapter; do not route this path through Rust bytes dispatch for symmetry.
- IPC (`ipc://`): UDS control channel plus POSIX SHM data plane through Rust. Remote IPC concurrency semantics and route capacity limits belong to Rust `c2-server`; Python only projects the native handle for same-process calls.
- HTTP (`http://`): relay-based cross-machine transport through Rust.

Direct IPC is a complete standalone mode. `cc.connect(..., address='ipc://...')` must bypass relay discovery and remain usable when no relay is configured or a relay environment variable points at an unavailable server. Relay is only a discovery/forwarding projection above IPC; do not make relay the owner of IPC registration, scheduling, or connection establishment.

Direct IPC `shutdown("ipc://...")` under `c_two.transport.client.util` is an admin/control-plane helper for high-privilege same-host supervisors. It stops the addressed native IPC server; it is not ordinary CRM business-client behavior and is distinct from top-level `cc.shutdown()`, which cleans up the current process registry. Its direct IPC control acknowledgement is the initiate phase only: it proves the server accepted shutdown and entered draining, not that active callbacks have drained or shutdown hooks are safe to run. Completion and route close outcomes must be observed through `RuntimeSession.shutdown()` / bridge barriers or a future explicit wait helper. Future name-based or HTTP/relay-propagated shutdown must be designed as an explicit authenticated admin control plane with clear route-level vs server-level scope, not as a hidden normal RPC side effect.

Server lifecycle/readiness belongs to Rust. Python `NativeServerBridge` may expose `start()`, `is_started()`, and shutdown facades, but must delegate readiness to native `RustServer.start_and_wait()` and native state projection. Do not reintroduce Python socket-file polling, `os.path.exists(socket_path)` readiness checks, or Python-owned `_started` authority. Each native start attempt must be fenced in Rust before readiness waiters run: reset stale `Stopped` / `Failed` lifecycle state and the one-shot shutdown signal, then observe readiness for that attempt. A Rust `Server` may only unlink its IPC socket path after that concrete instance successfully bound the socket; failed duplicate startups must not remove another server's active socket path. Bridge shutdown must still drive the idempotent native runtime barrier through `RuntimeSession.shutdown()` so the PyO3 runtime handle drains even when native lifecycle state is already stopped by an external direct IPC shutdown signal. That native runtime barrier is a lifecycle fence: after it returns, runtime-owned server work must be terminal (`Initialized`, `Stopped`, or `Failed`) so a following start attempt does not observe stale `Stopping` state.

Python resource servers create an auto-generated `ipc://` address. Use `cc.server_address()` after registration only when a same-host process needs to connect directly.

Relay anchor priority:

```text
cc.set_relay_anchor() > C2_RELAY_ANCHOR_ADDRESS > none
```

`C2_RELAY_ANCHOR_ADDRESS` is the SDK process's relay anchor for control-plane registration and name resolution. It is not necessarily the relay that will carry every data-plane call: after resolution, remote HTTP calls use the selected route's `relay_url` directly. Direct IPC selection from relay resolution is allowed only when the anchor endpoint is loopback/local; nonlocal relay responses must be treated as HTTP relay targets even if they include an `ipc_address`.

Relay resolution and data-plane calls are contract-scoped, not name-only. `cc.connect(CRMClass, name='...')` derives an `ExpectedRouteContract` from the CRM class and the route name. Runtime relay resolve, probe, and call paths must carry and validate the full route name, CRM tag, ABI hash, and signature hash. Do not add production APIs, fallback behavior, cache keys, or wire paths that resolve, probe, or call a CRM route by name alone. A future "find by name" or diagnostic relay-mesh lookup must be a separate discovery/admin surface that returns candidate route metadata and CRM tags; it must not be reused as the normal CRM call path.

Relay-discovered IPC fast paths must validate endpoint identity in Rust before accepting the IPC client. The relay response identity (`server_id` and `server_instance_id`) must match the identity returned by the IPC handshake, and then the requested route name must exist. Do not reintroduce Python-side or route-name-only trust decisions for relay IPC.

The Python SDK does not embed or start a relay server. Start `c3 relay` outside the SDK, through the CLI, Docker Compose, Kubernetes, or other orchestration. Do not add SDK APIs that embed, start, or manage relay-server lifecycle.

### Rust Native Layer

Paths: `core/`, `sdk/python/native/`

`core/` is a language-neutral Cargo workspace. The Python SDK owns its PyO3 extension crate under `sdk/python/native/`.

| Layer | Crate | Purpose |
| --- | --- | --- |
| foundation | `c2-contract` | Route contract validation and canonical descriptor hashing |
| foundation | `c2-config` | Unified IPC and relay configuration structs/resolvers |
| foundation | `c2-error` | Canonical error registry and `code:message` wire codec |
| foundation | `c2-mem` | Buddy allocator, SHM regions, unified memory pool |
| protocol | `c2-wire` | Wire protocol codec, frames, chunk assembler, chunk registry |
| transport | `c2-ipc` | Async IPC client, UDS, SHM, chunked transfer |
| transport | `c2-server` | Tokio UDS server with per-connection state and peer SHM lazy-open |
| transport | `c2-http` | HTTP client, relay-aware client, and HTTP relay server behind `relay` feature |
| runtime | `c2-runtime` | Process runtime session, route transactions, client pools, relay projection |
| sdk/python/native | `c2-python-native` | PyO3 bindings for `c_two._native` |

Memory subsystem:

- Allocation tiers: buddy SHM, dedicated SHM, file spill.
- `MemHandle` abstracts buddy, dedicated, and file-spill handles.
- `c2-mem` owns SDK-visible buffer lease accounting. Lease tracking records metadata and retention state only; it must not read payload bytes, allocate a second buffer, or replace `MemPool::free_at()` / `release_handle()` as the memory release authority.
- SHM segment names are deterministic: `{prefix}_b{idx:04x}` for buddy and `{prefix}_d{idx:04x}` for dedicated.
- Server lazy-opens peer segments from prefix and index. There is no explicit segment announcement protocol.

### CLI

The `c3` command is implemented by the root `cli/` Rust package. Python does not own CLI behavior. For source-checkout development, build and link a local `c3` with:

```bash
python tools/dev/c3_tool.py --build --link
```

Do not add CLI command behavior under `sdk/python/src/c_two`.

`c3 relay` options:

| Option | Env var | Default | Purpose |
| --- | --- | --- | --- |
| `--bind` | `C2_RELAY_BIND` | `0.0.0.0:8080` | HTTP listen address |
| `--seeds` | `C2_RELAY_SEEDS` | empty | Comma-separated seed relay URLs for mesh |
| `--relay-id` | `C2_RELAY_ID` | auto UUID | Stable relay identifier |
| `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | derived | Publicly reachable URL for peers |
| `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | `60` | IPC idle disconnect timeout in seconds; `0` disables time-based eviction |
| `--upstream` | none | empty | Pre-register upstream as `NAME=SERVER_ID@ADDRESS`; `SERVER_ID` must match the IPC server handshake identity |

Relay-aware clients use `C2_RELAY_ROUTE_MAX_ATTEMPTS` to cap route acquisition attempts before reporting failure. This setting belongs to the client side, not the relay server resolver.

## Key Conventions

### Import Style

Import the package as `c_two` and alias it as `cc`:

```python
import c_two as cc
```

### CRM Contract Pattern

CRM contract classes are interfaces. Method bodies are `...`. The `@cc.crm()` decorator requires `namespace` and `version`. Do not use an `I` prefix on the contract class name.

```python
@cc.crm(namespace='cc.demo', version='0.1.0')
class Grid:
    def some_method(self, arg: int) -> str:
        ...
```

### Transferable Pattern

`serialize`, `deserialize`, and `from_buffer` are written as regular methods. The `TransferableMeta` metaclass converts them to static methods. Do not add `@staticmethod` yourself.

```python
@cc.transferable
class MyData:
    value: int

    def serialize(data: 'MyData') -> bytes:
        ...

    def deserialize(data: bytes) -> 'MyData':
        ...

    def from_buffer(buf: memoryview) -> 'MyData':
        ...
```

### Transfer Decorator Pattern

`@cc.transfer()` is metadata-only. It attaches `__cc_transfer__` to CRM contract methods and does not wrap the function.

```python
@cc.crm(namespace='ns', version='0.1.0')
class MyResource:
    @cc.transfer(input=MyData, output=MyData, buffer='hold')
    def process(self, data: MyData) -> MyData:
        ...
```

### Hold Mode Pattern

`cc.hold()` wraps a CRM proxy bound method for client-side SHM retention. It returns `HeldResult` with `.value` and `.release()`. Safety layers are explicit release, context manager, and `__del__` fallback.

Retained buffer accounting is Rust-owned. `cc.hold()` and `HeldResult` are Python SDK facades over native SDK-visible buffer leases. Inline, SHM, handle, and file-spill buffers can all be retained leases; do not special-case hold as SHM-only and do not reintroduce Python weakref registries for held buffers.

```python
with cc.hold(proxy.method)(args) as held:
    data = held.value

a = cc.hold(proxy.method)(args_a)
b = cc.hold(proxy.method)(args_b)
try:
    process(a.value, b.value)
finally:
    a.release()
    b.release()
```

When `@cc.transfer(buffer=None)`, the framework checks whether the input transferable has `from_buffer`. If yes, it uses hold mode; otherwise it uses view mode.

### SOTA API Pattern

The registry exposes a flat top-level API on the `cc` namespace:

```python
import c_two as cc

# Server side
cc.set_relay_anchor('http://relay-host:8080')
cc.set_transport_policy(shm_threshold=64 * 1024)
cc.set_server(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
cc.register(Grid, grid_instance, name='grid')
cc.register(Network, net_instance, name='network')
cc.serve()

# Client side, same process: thread preference
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Client side, direct IPC
cc.set_client(ipc_overrides={'pool_segment_size': 2 * 1024 * 1024 * 1024})
grid = cc.connect(Grid, name='grid', address='ipc://my_server')
grid.some_method(arg)
cc.close(grid)

# Client side, relay-based name resolution and routing
cc.set_relay_anchor('http://relay-host:8080')
grid = cc.connect(Grid, name='grid')
grid.some_method(arg)
cc.close(grid)

# Cleanup
cc.unregister('grid')
cc.shutdown()
```

The `name` parameter in `cc.register()` is a user-chosen routing key. It is not the CRM namespace. Multiple resources using different CRM contracts, or the same CRM contract with different instances, can coexist under distinct names. For remote IPC/relay paths, name is necessary but not sufficient: the native runtime also matches the expected CRM tag and contract hashes derived from the client's CRM class.

### Error Handling

Errors are modeled as `CCError` subclasses with numeric `ERROR_Code` values. Rust `core/foundation/c2-error` owns the canonical error registry and `code:message` wire codec. Python generates `ERROR_Code` from `_native.error_registry()` and delegates `CCError.serialize()` / `CCError.deserialize()` to `_native.encode_error_wire()` and `_native.decode_error_wire_parts()`. Do not reintroduce Python-side parsing of the error wire payload, and do not use or add legacy-named error codec APIs. Error classes are named by location, for example `ResourceDeserializeInput`, `ClientSerializeInput`, `ResourceExecuteFunction`, and `ClientCallResource`. Enum values live under `ERROR_AT_RESOURCE_*` and `ERROR_AT_CLIENT_*`. Transfer hook errors are split by hook type: `deserialize()` failures remain `ERROR_AT_RESOURCE_INPUT_DESERIALIZING` or `ERROR_AT_CLIENT_OUTPUT_DESERIALIZING`, while `from_buffer()` / `fromBuffer()` failures use `ERROR_AT_RESOURCE_INPUT_FROM_BUFFER` or `ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER`. Do not label zero-deserialization buffer view construction as deserialization. On any `from_buffer()` failure before a resource/user receives a retained value, release the native memoryview and buffer owner immediately so Rust lease stats do not retain stale holds.

### Naming

- CRM contract classes: plain names, no `I` prefix.
- Resource implementation classes: domain names such as `NestedGrid` or `PostgresVectorLayer`; `{ContractName}Impl` is acceptable for generated code.
- Transferable classes: descriptive data names such as `GridAttribute`.
- Routing names: user-chosen strings passed to `cc.register(name=...)` and `cc.connect(name=...)`; distinct from CRM `namespace`.

### Performance-Sensitive Code

Wire codec and transport code in Rust (`c2-wire`, `c2-ipc`, `c2-mem`) prioritize zero-copy and single-allocation patterns. Python `wire.py` retains `MethodTable`, `payload_total_size`, and thin FFI wrappers. The thread-local transport skips serialization entirely. SHM segment names are deterministic for lazy peer-side opening. The buddy allocator's `alloc()` and `free_at()` are thread-safe. When changing remote dispatch or scheduler code, keep the SHM request path as `RequestData::Shm` / `RequestData::Handle` to `PyShmBuffer` to Python `memoryview`; do not materialize SHM or handle payloads into Python `bytes` before invoking resource code. Response allocation follows the same boundary: Python server dispatch may return serialized `bytes` or buffer-protocol data, but must not receive native response pools, compare result length against `shm_threshold`, return SHM coordinate tuples, or call `bytes(memoryview)` for transport. Native PyO3/c2-server response preparation must attempt large-buffer SHM writes before materializing owned inline bytes. Thread-local same-process calls must continue to pass Python objects directly instead of being routed through Rust serialized dispatch for symmetry.

## Environment Variables

| Variable | Purpose | Default |
| --- | --- | --- |
| `C2_RELAY_ANCHOR_ADDRESS` | SDK relay anchor URL for CRM registration and client name resolution | none |
| `C2_RELAY_ROUTE_MAX_ATTEMPTS` | Relay-aware client route acquisition attempts | `3` |
| `C2_RELAY_CALL_TIMEOUT` | Relay HTTP CRM call timeout seconds; `0` disables the reqwest total timeout | `300` |
| `C2_REMOTE_PAYLOAD_CHUNK_SIZE` | C-Two remote payload body batching size for relay HTTP and future remote protocols; not a TCP packet, HTTP/1 chunk, or HTTP/2 DATA frame guarantee | `1048576` |
| `C2_RELAY_BIND` | Relay HTTP listen address for `c3 relay --bind` | `0.0.0.0:8080` |
| `C2_RELAY_ID` | Stable relay identifier for mesh protocol | auto UUID |
| `C2_RELAY_ADVERTISE_URL` | Publicly reachable URL announced to mesh peers | derived |
| `C2_RELAY_SEEDS` | Comma-separated seed relay URLs for mesh mode | none |
| `C2_RELAY_IDLE_TIMEOUT` | Upstream IPC idle disconnect timeout in seconds; `0` disables time-based eviction | `60` |
| `C2_RELAY_ANTI_ENTROPY_INTERVAL` | Anti-entropy digest exchange interval in seconds | `60.0` |
| `C2_RELAY_USE_PROXY` | Use system proxy variables for relay HTTP clients | `false` |
| `C2_SHM_THRESHOLD` | Payload size threshold for SHM vs inline | `4096` |
| `C2_IPC_POOL_SEGMENT_SIZE` | Buddy pool segment size in bytes | `268435456` |
| `C2_IPC_MAX_POOL_SEGMENTS` | Max buddy pool segments, 1 to 255 | `4` |
| `C2_IPC_POOL_DECAY_SECONDS` | Idle segment decay time | `60.0` |
| `C2_IPC_POOL_ENABLED` | Enable or disable SHM pool | `true` |
| `C2_IPC_MAX_FRAME_SIZE` | Max inline frame size | `2147483648` |
| `C2_IPC_MAX_PAYLOAD_SIZE` | Max single-call payload size | `17179869184` |
| `C2_IPC_MAX_PENDING_REQUESTS` | Max concurrent pending requests per connection | `1024` |
| `C2_IPC_MAX_EXECUTION_WORKERS` | Max blocking execution workers for server-side resource callbacks | available parallelism clamped to `4..=64` |
| `C2_IPC_HEARTBEAT_INTERVAL` | Heartbeat interval seconds; `0` disables | `15.0` |
| `C2_IPC_HEARTBEAT_TIMEOUT` | Heartbeat timeout seconds | `30.0` |
| `C2_IPC_MAX_TOTAL_CHUNKS` | Max total in-flight chunks across all connections | `512` |
| `C2_IPC_CHUNK_GC_INTERVAL` | Chunk GC sweep interval seconds | `5.0` |
| `C2_IPC_CHUNK_THRESHOLD_RATIO` | Fraction of max frame size that triggers chunking | `0.9` |
| `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` | Chunk assembler timeout seconds | `60.0` |
| `C2_IPC_MAX_REASSEMBLY_BYTES` | Max reassembly buffer memory | `8589934592` |
| `C2_IPC_CHUNK_SIZE` | Individual chunk size | `131072` |
| `C2_IPC_REASSEMBLY_SEGMENT_SIZE` | Reassembly pool segment size | `67108864` |
| `C2_IPC_REASSEMBLY_MAX_SEGMENTS` | Max reassembly pool segments | `4` |
| `C2_ENV_FILE` | Path to `.env`; empty string disables env-file loading | `.env` |

The Rust `c2-config` resolver loads `.env` and process environment values. Precedence is explicit code overrides, then process environment / `.env`, then defaults. See `.env.example` for the full reference.

Do not add environment variables for values that can be derived from existing canonical settings. Pool memory limits should be derived from segment size and segment count.

## Python Version

Requires Python 3.10 or newer. Keep Python 3.10 compatibility intentional: downstream Taichi-based workloads can still be pinned to 3.10 even when normal development prefers Python 3.12 and free-threading validation targets 3.14t. Use modern type hints such as `list[int]`, `str | None`, and `tuple[...]`. Free-threading Python is a target platform; be cautious with C extensions, shared mutable state, and GIL assumptions.

## Agent Working Rules

- Read the existing code before changing it.
- Prefer `rg` and `rg --files` for repository searches.
- Keep Python SDKs as glue around Rust mechanisms. Do not move shared transport, resolver, route fallback, or protocol mechanisms into Python SDK code.
- Use Rust core crates for cross-language behavior so later SDKs do not have to reimplement mechanisms independently.
- Do not treat the Python SDK as the reference implementation for generic runtime behavior. If behavior is language-neutral, prefer a Rust-core owner with thin SDK facades.
- Preserve direct IPC as relay-independent. If touching registration, client routing, or runtime session code, include checks for explicit `ipc://` connections with relay unset or unavailable.
- Preserve zero-copy boundaries. If touching wire, SHM, scheduler, or native callback code, include checks that large SHM-backed payloads are not converted to Python `bytes` on the remote IPC path.
- For bug fixes and behavior changes, add or update focused tests first when feasible, then implement the correct production-grade code change needed to satisfy the verified behavior; do not use phase boundaries to justify temporary shims or lower-quality shortcuts.
- When work is split into phases, treat the split as sequencing only. Write the phase boundaries, exit criteria, and follow-up items into the plan document before implementation, keep that plan updated as the authoritative record, and finish each phase with docs that make the remaining work explicit.
- Do not revert unrelated user changes in a dirty worktree.
- Avoid destructive git commands unless the user explicitly asks for them.
- When editing Markdown prose such as README or AGENTS content, keep each paragraph on one physical line and let editors/renderers handle visual wrapping; do not hard-wrap prose unless a table, list, code block, or generated artifact requires it.
- For reviews, lead with concrete findings ordered by severity and include file and line references.
