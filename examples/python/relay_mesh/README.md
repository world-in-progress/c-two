# Relay Mesh Resource Discovery Example

Demonstrates C-Two's **relay mesh** for automatic resource discovery.
Clients connect to CRM resources **by name** — no IPC addresses needed.

Reuses the `Grid` contract and `NestedGrid` resource from `examples/python/grid/`.

This is the advanced multi-relay example. For the basic relay smoke test, use
`examples/python/relay_resource.py` and `examples/python/relay_client.py`.

## Prerequisites

Install the examples dependency group (includes pandas and numpy):

```bash
uv sync --group examples
```

Build and link `c3` once from a source checkout:

```bash
python tools/dev/c3_tool.py --build --link
```

## Architecture

```
┌──────────────────┐         ┌──────────────────┐
│  CRM Process A   │──reg──▶│                  │
│  (Grid resource) │         │   Relay Server   │◀──resolve── Client
│                  │◀──ipc──│   :8300           │
└──────────────────┘         └──────────────────┘
```

1. **Relay** — `c3 relay` HTTP relay server that maintains a route table
2. **CRM Process** — registers `Grid` resource with the relay by name
3. **Client** — resolves `grid` by name via relay, then calls methods

## Run (3 terminals)

The resource and client use `http://127.0.0.1:8300` by default. Set
`C2_RELAY_ANCHOR_ADDRESS` only if you start the relay at a different address.
For multi-host runs, keep relay HTTP and `/_peer/*` mesh endpoints inside a
trusted network boundary. Do not expose this demo relay directly to the public
internet.

```bash
# Terminal 1 — start the relay server
c3 relay --bind 127.0.0.1:8300

# Terminal 2 — start the Grid CRM (auto-registers with the default relay)
uv run python examples/python/relay_mesh/resource.py

# Terminal 3 — client discovers and uses Grid via the default relay
uv run python examples/python/relay_mesh/client.py
```

## What this demonstrates

- **Name-based discovery**: client uses `cc.connect(Grid, name='grid')` without
  knowing the CRM process's IPC address
- **Relay registration**: CRM process calls `cc.set_relay_anchor()` so `cc.register()`
  automatically announces the resource to the relay
- **Transparent transport**: the same CRM contract works identically across
  thread-local, IPC, and HTTP relay modes
- **Lifecycle management**: `cc.serve()` blocks until SIGINT, then gracefully
  unregisters from the relay
