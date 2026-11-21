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

`local.py` demonstrates local registration and thread-local calls in one Python process. It does not configure IPC addresses or a relay anchor itself; if `C2_RELAY_ANCHOR_ADDRESS` is set in your shell or `.env`, prefix the command with `C2_RELAY_ANCHOR_ADDRESS=` to keep registration local:

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run python examples/python/local.py
```

### Independent Runtime preflight

`independent_runtime.py` creates an explicit `cc.Runtime()`, starts an empty Host using its normal readiness/PONG path, and inspects its structured shutdown result. On Unix it creates a fresh private endpoint directory with the standard library and removes that directory only after completed, error-free cleanup. If cleanup is incomplete or reports an error, it keeps the path for the caller to inspect; a start error is re-raised even when cleanup succeeds. Windows uses the normal per-user Named Pipe endpoint and creates no Unix directory.

```bash
uv run python examples/python/independent_runtime.py
```

### Resident Simulation (Background Stepping & Dynamic Branching)

`resident_simulation.py` demonstrates a stateful scientific simulation that advances continuously in a background thread while serving short `@cc.read` snapshots, `@cc.write` interventions, `@cc.stream` frame subscriptions, and runtime `register` / `unregister` what-if branches over direct IPC under `ConcurrencyConfig(mode='read_parallel')`, stopping cleanly via `@cc.on_shutdown`. It uses only the Python standard library and base `c-two` (no optional examples dependencies required):

```bash
C2_RELAY_ANCHOR_ADDRESS= uv run python examples/python/resident_simulation.py
```

### Publish/Subscribe Topics

`pubsub.py` demonstrates resource-owned publish/subscribe topics in one process ([ADR 0005](../docs/decisions/0005-publish-subscribe.md)): `@cc.topic` declarations on a CRM contract, `cc.register(...)` returning a `Registration` whose `publisher(topic)` handle publishes immutable frames (with `topic_capacity` history depth and an atomically committed `initial_topics` frame), client-side topic members and `cc.with_call_options(...)` subscriptions with `start='latest'|'next'` and `on_lag='skip'|'error'`, `cc.hold(...)` deliveries retained as `HeldResult`, truthful `close()` receipts, and the `ResourceClosed` refusal a publisher sees after its registration ends:

```bash
uv run python examples/python/pubsub.py
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

### Resource Discovery via Relay

`examples/python/relay_mesh/` demonstrates resource discovery by name through one standalone relay started without seeds.
