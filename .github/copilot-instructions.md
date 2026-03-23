# Copilot Instructions for C-Two

## Project Overview

C-Two is a **resource-oriented RPC framework** for Python that enables remote invocation of stateful resource classes across processes and machines. It is designed for distributed scientific computation — not traditional microservices.

The core abstraction is **not services, but resources**: CRMs (Core Resource Models) encapsulate persistent state and domain logic; Components consume them through ICRM interfaces with full location transparency.

## Build, Test & Run

Package manager: **uv** (not pip directly)

```bash
# Install dependencies
uv sync

# Run the full test suite
uv run pytest -q

# Run a single test file
uv run pytest tests/unit/test_encoding.py -q

# Run a single test class or function
uv run pytest tests/unit/test_transferable.py::TestTransferableDecorator::test_hello_data_round_trip -q

# Run examples (start server first, then client in separate terminal)
uv run python examples/server.py
uv run python examples/client.py

# CLI tool (installed as `c3`)
c3 --version
c3 build <project_path> --base-image python:3.12-slim
```

Tests use **pytest** with a 30-second per-test timeout. Test fixtures are parameterized across transport protocols (thread, memory, ipc, tcp, http). Tests live under `tests/unit/` and `tests/integration/`, with shared fixtures in `tests/fixtures/`.

## Architecture

Three-tier design with strict separation:

### 1. CRM Layer (`src/c_two/crm/`)
- **CRM**: A plain Python class holding state and implementing domain logic. Not decorated.
- **ICRM**: An interface class decorated with `@cc.icrm(namespace='...', version='...')` that declares which CRM methods are remotely accessible. Only methods in the ICRM are exposed.
- **`@transferable`**: Decorator for custom data types that need to cross the wire. Automatically makes classes into dataclasses and registers `serialize`/`deserialize` as static methods. Without `@transferable`, pickle is used as fallback.
- **Method access**: ICRM methods can be annotated with `@cc.read` or `@cc.write` (default: write) to control concurrency — the scheduler allows parallel reads but exclusive writes.

### 2. Component Layer (`src/c_two/compo/`)
- Components are client-side consumers of CRM resources.
- **Script-based**: Use `cc.compo.runtime.connect_crm(address, ICRMClass)` as a context manager to get a typed ICRM proxy.
- **Function-based**: Decorate functions with `@cc.compo.runtime.connect` (or `@cc.runtime.connect`). The first parameter must be typed as the ICRM class — the framework injects the connected instance automatically.

### 3. Transport Layer (`src/c_two/rpc/`)
Protocol is auto-detected from the address scheme:
| Scheme | Transport | Use case |
|---|---|---|
| `thread://` | ThreadServer/Client | In-process, skips serialization entirely |
| `memory://` | MemoryServer/Client | Shared memory, local |
| `ipc:///path` | IpcServer/Client | Inter-process (UDS control + SHM data) |
| `tcp://host:port` | ZmqServer/Client | Cross-machine |
| `http://host:port` | HttpServer/Client | Web-compatible |

The `Server` class in `rpc/server.py` and `Client` class in `rpc/client.py` are the unified entry points — they dispatch to protocol-specific implementations based on address prefix.

### Wire Protocol (`src/c_two/rpc/util/wire.py`)
The compact binary wire codec used by all non-thread transports. Little-endian format:
- **CRM_CALL**: `[1B type][2B method_len LE][method_name UTF-8][payload]`
- **CRM_REPLY**: `[1B type][4B error_len LE][error_bytes][result]`
- **Signals**: `[1B type]` (PING, PONG, SHUTDOWN variants — pre-encoded singletons)

Method names are **pre-encoded at ICRM registration time** (`preregister_methods()` in `meta.py`) to avoid repeated UTF-8 encoding and struct packing on every call. Decoding uses zero-copy `memoryview` slicing.

### Event System (`src/c_two/rpc/event/`)
Legacy communication layer using `Event`, `EventTag`, and `EventQueue`. Being migrated to the wire protocol — new code should use `wire.encode_call`/`wire.decode` directly. The bridge function `event_to_wire_bytes()` in `encoding.py` handles the transition.

### MCP Integration (`src/c_two/mcp/`)
Bridges C-Two components to the Model Context Protocol. `register_mcp_tools_from_compo_module()` auto-registers all component functions in a module as MCP tools. Peripheral/experimental — not part of core framework concerns.

### Seed / CLI (`src/c_two/seed/`, `src/c_two/cli.py`)
The `c3` CLI currently has one command: `build` — for generating Dockerfiles and building Docker images for CRM deployment.

## Key Conventions

### Import Style
The package is imported as `c_two` but aliased as `cc`:
```python
import c_two as cc
```

### ICRM Definition Pattern
ICRM classes are interfaces — method bodies are `...` (ellipsis). The `@cc.icrm()` decorator requires `namespace` and `version` (semver string):
```python
@cc.icrm(namespace='cc.demo', version='0.1.0')
class IGrid:
    def some_method(self, arg: int) -> str:
        ...
```

### Transferable Pattern
`serialize` and `deserialize` are written as regular methods but the `TransferableMeta` metaclass converts them to `@staticmethod` automatically — do **not** add `@staticmethod` yourself:
```python
@cc.transferable
class MyData:
    value: int
    
    def serialize(data: 'MyData') -> bytes:
        ...
    def deserialize(arrow_bytes: bytes) -> 'MyData':
        ...
```

### Component Function Pattern
The first parameter is always the ICRM type (injected by the framework). Callers never pass it:
```python
@cc.runtime.connect
def process(crm: IGrid, level: int) -> list[str]:
    return crm.subdivide_grids([level], [0])

# Called without the crm parameter:
result = process(1, crm_address='thread://server')
```

### ICRM Direction Convention
- `'->'` (default): Component-to-CRM direction (client side)
- `'<-'`: CRM-to-Component direction (server side, set internally when server creates the inverted ICRM)

### Error Handling
Errors are modeled as `CCError` subclasses with numeric `ERROR_Code` values. Errors serialize to/from bytes for wire transfer. Error classes are named by location: `CRMDeserializeInput`, `CompoSerializeInput`, `CRMExecuteFunction`, etc.

### ServerConfig Validation
`ServerConfig.__post_init__` validates that the CRM instance implements all public methods declared in the ICRM interface.

### Naming
- ICRM classes: prefixed with `I` (e.g., `IGrid`)
- CRM classes: plain names matching the resource (e.g., `Grid`)
- Transferable classes: descriptive data names (e.g., `GridAttribute`, `GridSchema`)
- Address constants: `SCREAMING_SNAKE_CASE` (e.g., `MEMORY_ADDRESS`, `TCP_ADDRESS`)

### Performance-Sensitive Code
Wire codec and transport code (`rpc/util/`, `rpc/ipc/`) prioritize zero-copy (`memoryview`), single-allocation patterns (`bytearray` + `struct.pack_into`), and pre-computation. Avoid introducing intermediate `bytes` copies on the hot path. The `thread://` transport skips serialization entirely via `call_direct`.

## Python Version

Requires Python ≥ 3.10. Uses modern type hints (`list[int]`, `str | None`, `tuple[...]`). Free-threading (3.14t) is a target platform — be cautious with C extensions and GIL assumptions.
