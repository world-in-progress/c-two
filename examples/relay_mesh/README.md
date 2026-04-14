# Relay Mesh Resource Discovery Example

Demonstrates C-Two's **relay mesh** for automatic resource discovery.
Clients connect to CRM resources **by name** — no IPC addresses needed.

Reuses the `Grid` CRM and `IGrid` ICRM from `examples/grid/`.

## Prerequisites

Install the examples dependency group (includes pandas, numpy, pyarrow):

```bash
uv sync --group examples
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

```bash
# Terminal 1 — start the relay server
c3 relay -b 0.0.0.0:8300

# Terminal 2 — start the Grid CRM (auto-registers with relay)
uv run examples/relay_mesh/resource.**py**

# Terminal 3 — client discovers and uses Grid via relay
uv run examples/relay_mesh/client.py
```

## What this demonstrates

- **Name-based discovery**: client uses `cc.connect(IGrid, name='grid')` without
  knowing the CRM process's IPC address
- **Relay registration**: CRM process calls `cc.set_relay()` so `cc.register()`
  automatically announces the resource to the relay
- **Transparent transport**: the same ICRM interface works identically across
  thread-local, IPC, and HTTP relay modes
- **Lifecycle management**: `cc.serve()` blocks until SIGINT, then gracefully
  unregisters from the relay
