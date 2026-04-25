# C-Two Examples

These examples use the optional examples dependencies. Install them first:

```bash
uv sync --group examples
```

## Python Grid Example

The Grid example can run in two modes:

- IPC direct mode: one CRM process and one client on the same machine.
- Relay mode: a relay resolves the CRM by name, so the client does not need the IPC address.

### IPC Direct Mode

Start the CRM process:

```bash
uv run python examples/python/crm_process.py
```

Copy the printed `ipc://...` address, then run the client in another terminal:

```bash
uv run python examples/python/client.py ipc://...
```

### Relay Mode

Start the relay:

```bash
c3 relay -b 127.0.0.1:8300
```

Start the CRM process and register it with the relay:

```bash
uv run python examples/python/crm_process.py --relay-url http://127.0.0.1:8300
```

Run the client through relay name resolution. No IPC address is needed:

```bash
uv run python examples/python/client.py --relay-url http://127.0.0.1:8300
```

You can also use the environment variable:

```bash
C2_RELAY_ADDRESS=http://127.0.0.1:8300 uv run python examples/python/client.py
```

`examples/python/relay_client.py` is the HTTP-only version of the same client flow.
