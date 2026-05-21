# C-Two Examples

These examples use the optional examples dependencies. Install them first:

```bash
uv sync --group examples
```

Relay examples also require the standalone `c3` CLI. Install the latest released binary with:

```bash
curl -fsSL https://github.com/world-in-progress/c-two/releases/latest/download/c3-installer.sh | sh
```

From a source checkout, build and link it once before running relay examples, integration tests, or SDK development flows that depend on relay behavior:

```bash
python tools/dev/c3_tool.py --build --link
```

If the selected bin directory is not already on `PATH`, the tool prints the exact `export PATH=...` command.

## Python Examples

### Local Single-Process

`local.py` demonstrates local registration and thread-local calls in one Python process. It does not use IPC addresses or relay configuration.

```bash
uv run python examples/python/local.py
```

### IPC Direct Mode

Use this path for direct IPC between one resource process and one client on the same machine. The client always uses the explicit IPC address you pass it.

Start the Grid resource:

```bash
uv run python examples/python/ipc_resource.py
```

Copy the printed `ipc://...` address, then run the client in another terminal:

```bash
uv run python examples/python/ipc_client.py ipc://...
```

### Single Relay Mode

Use this path when a relay resolves the CRM by name. The resource and client are relay-only scripts.

Start the relay:

```bash
c3 relay --bind 127.0.0.1:8300
```

Start the Grid resource and register it with the relay:

```bash
uv run python examples/python/relay_resource.py --relay-url http://127.0.0.1:8300
```

Run the client through relay name resolution. No IPC address is needed:

```bash
uv run python examples/python/relay_client.py --relay-url http://127.0.0.1:8300
```

When the relay runs on the default address, the relay URL can come from the Rust-backed config resolver or the built-in example default:

```bash
uv run python examples/python/relay_resource.py
uv run python examples/python/relay_client.py
```

### Relay Mesh

`examples/python/relay_mesh/` is the advanced relay mesh example. Use the single-relay example above as the basic relay smoke test, then move to the mesh example when validating multi-relay route propagation.
