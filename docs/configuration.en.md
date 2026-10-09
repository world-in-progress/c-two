# C-Two configuration

English · [简体中文](configuration.md)

Rust `c2-config` owns defaults, environment resolution and validation. SDKs provide code overrides; c3 provides command-line overrides. This guide covers process configuration, IPC, relay, concurrency and lifecycle settings.

## Configuration sources and freezing

Precedence is **explicit code or CLI > process environment > `.env` > Rust defaults**. The default environment file is `.env` in the current working directory. Set `C2_ENV_FILE` to another file, or to an empty string to disable file loading. [`.env.example`](../.env.example) contains the environment reference.

```bash
cp .env.example .env
C2_ENV_FILE=./runtime.env python resource.py
C2_ENV_FILE= python resource.py
```

```powershell
$env:C2_ENV_FILE = '.\runtime.env'
python resource.py
$env:C2_ENV_FILE = ''
```

Set process policy before registering resources or connecting clients. Each native configuration domain freezes when first used: local endpoints on the first bind/connect attempt, including a failed attempt; call admission on the first remote business call. Queries do not freeze configuration. Later environment changes do not alter a frozen choice.

## SDK and transport configuration

`cc.set_server(...)` sets server identity, IPC overrides and lifecycle policy. `cc.set_client(...)` sets client IPC overrides. `cc.set_transport_policy(...)` controls process transport policy.

```python
import c_two as cc

cc.set_transport_policy(
    shm_threshold=64 * 1024,
    remote_payload_chunk_size=1024 * 1024,
)
cc.set_server(
    server_id='my-server',
    ipc_overrides={'max_payload_size': 64 * 1024 * 1024, 'pool_enabled': False},
)
cc.set_client(ipc_overrides={'pool_enabled': False})
```

Typed overrides are `BaseIPCOverrides`, `ServerIPCOverrides` and `ClientIPCOverrides`. Server-only fields are rejected as client overrides. Native parsing validates all keys and values.

| Purpose | Code entry point | Environment | Default |
| --- | --- | --- | --- |
| Relay registration and discovery | `cc.set_relay_anchor(url)` | `C2_RELAY_ANCHOR_ADDRESS` | No relay |
| SHM/inline threshold | `cc.set_transport_policy(shm_threshold=...)` | `C2_SHM_THRESHOLD` | 4 KiB |
| Remote payload batch size | `cc.set_transport_policy(remote_payload_chunk_size=...)` | `C2_REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB |
| Server frame limit | `set_server(ipc_overrides={'max_frame_size': ...})` | `C2_IPC_MAX_FRAME_SIZE` | 2 GiB |
| Logical payload limit | `set_server(ipc_overrides={'max_payload_size': ...})` | `C2_IPC_MAX_PAYLOAD_SIZE` | 16 GiB |
| Pending requests per connection | `set_server(ipc_overrides={'max_pending_requests': ...})` | `C2_IPC_MAX_PENDING_REQUESTS` | 1,024 |
| Resource execution threads | `set_server(ipc_overrides={'max_execution_workers': ...})` | `C2_IPC_MAX_EXECUTION_WORKERS` | Host parallelism, clamped to 4–64 |

An explicit `address='ipc://...'` uses local IPC independently of relay discovery. The relay anchor handles registration and discovery; HTTP calls use the relay URL selected by resolution. Remote batch size controls C-Two payload batches, without specifying TCP packets or HTTP frames.

## IPC memory, chunks and heartbeat

Set the following fields through the appropriate endpoint's `ipc_overrides`. Allocation, accounting and fallback behavior are described in the [memory policy](memory-policy.en.md).

| Override | Environment | Default |
| --- | --- | --- |
| `pool_enabled` | `C2_IPC_POOL_ENABLED` | `true` |
| `pool_segment_size` | `C2_IPC_POOL_SEGMENT_SIZE` | 256 MiB |
| `max_pool_segments` | `C2_IPC_MAX_POOL_SEGMENTS` | 4 |
| `pool_prewarm_segments` | `C2_IPC_POOL_PREWARM_SEGMENTS` | 0 |
| `pool_min_retained_segments` | `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` | 0 |
| `pool_decay_seconds` | `C2_IPC_POOL_DECAY_SECONDS` | 60 seconds |
| `shm_backing_budget_bytes` | `C2_IPC_SHM_BACKING_BUDGET_BYTES` | 8 GiB |
| `file_backing_budget_bytes` | `C2_IPC_FILE_BACKING_BUDGET_BYTES` | 16 GiB |
| `live_reassembly_budget_bytes` | `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` | 8 GiB |
| `reassembly_segment_size` | `C2_IPC_REASSEMBLY_SEGMENT_SIZE` | 64 MiB |
| `reassembly_max_segments` | `C2_IPC_REASSEMBLY_MAX_SEGMENTS` | 4 |
| `max_total_chunks` | `C2_IPC_MAX_TOTAL_CHUNKS` | 512 |
| `chunk_size` | `C2_IPC_CHUNK_SIZE` | 128 KiB |
| `chunk_threshold_ratio` | `C2_IPC_CHUNK_THRESHOLD_RATIO` | 0.9 |
| `chunk_assembler_timeout` | `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` | 60 seconds |
| `chunk_gc_interval` | `C2_IPC_CHUNK_GC_INTERVAL` | 5 seconds |
| `max_reassembly_bytes` | `C2_IPC_MAX_REASSEMBLY_BYTES` | 8 GiB |
| `heartbeat_interval` (server) | `C2_IPC_HEARTBEAT_INTERVAL` | 15 seconds |
| `heartbeat_timeout` (server) | `C2_IPC_HEARTBEAT_TIMEOUT` | 30 seconds |

Pool segment capacity and the per-message payload limit are independent. Disabling the buddy pool still permits dedicated SHM, checked chunk transfer and file spill. A zero backing or reassembly budget rejects positive reservations; `pool_decay_seconds=0` reclaims idle segments on the next maintenance pass; `heartbeat_interval=0` disables heartbeat.

`cc.memory_stats()` observes native memory domains, and `cc.hold_stats()` observes SDK leases. These are accounting snapshots, without measuring total process RSS.

## Relay and c3

Relay runs as a separate process with its own upstream IPC configuration. An application's `cc.set_client()` does not configure c3.

```bash
c3 relay --bind 127.0.0.1:8300 --ipc-pool-enabled false
```

```dotenv
C2_RELAY_BIND=127.0.0.1:8300
C2_IPC_POOL_ENABLED=false
C2_RELAY_IDLE_TIMEOUT=60
```

| Purpose | CLI | Environment | Default |
| --- | --- | --- | --- |
| HTTP listener | `--bind` | `C2_RELAY_BIND` | `0.0.0.0:8080` |
| Relay identity | `--relay-id` | `C2_RELAY_ID` | UUID |
| Advertised URL | `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | Derived from bind |
| Mesh seeds | `--seeds` | `C2_RELAY_SEEDS` | Empty |
| Upstream IPC idle disconnect | `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | 60 seconds; `0` disables timed eviction |
| Forwarding transaction count | `--call-max-outstanding` | `C2_CALL_MAX_OUTSTANDING` | 1,024 per relay |
| Retained forwarding input | `--call-retained-input-budget-bytes` | `C2_CALL_RETAINED_INPUT_BUDGET_BYTES` | 16 GiB per relay |
| System HTTP proxy | — | `C2_RELAY_USE_PROXY` | `false` |
| Anti-entropy interval | — | `C2_RELAY_ANTI_ENTROPY_INTERVAL` | 60 seconds |
| Client route acquisition attempts | SDK native configuration | `C2_RELAY_ROUTE_MAX_ATTEMPTS` | 3; range 1–32, with `0` treated as 1 |

Pre-register upstreams as `--upstream NAME=SERVER_ID@ADDRESS`; `SERVER_ID` must match the IPC handshake. See the [c3 guide](../cli/README.md) for upstream budgets, mesh and dry-run options.

Relay freezes forwarding limits at startup and applies them to all forwarded business calls, including calls with unlimited caller waiting. A zero count limit rejects new forwards; a zero byte budget rejects positive input. Known-length input is reserved before dispatch; unknown-length input is checked as it grows. Capacity refusal preserves the route. These limits are independent of upstream IPC backing and reassembly budgets and do not measure process RSS.

Relay capacity rejection is fixed before business admission. For a request without Expect and with a valid declared length, the HTTP handler discards transport data frame by frame within that length and one two-second deadline. It does not aggregate the payload, decode it, invoke a resource or retry admission; capacity released during disposal does not change the rejection. Rejected Expect or unknown-length bodies are not read. Read faults, length mismatches and timeout preserve the original capacity error. An incomplete or faulty transport can prevent the client from receiving that response.

After an HTTP caller disconnects, an already-dispatched forward retains its input and upstream connection until actual completion. Relay shutdown stops admission and waits for forwarding and native client cleanup. A resource method that never returns continues to occupy its slot and delays shutdown.

## Local endpoint directories

| Platform | Endpoint | Location configuration |
| --- | --- | --- |
| Unix | UDS in the final owner-controlled directory | `cc.set_local_endpoint(root=...)`, `C2_IPC_ROOT`, c3 `--ipc-root`; default `/tmp/c2-<uidhex>` |
| Windows | Named Pipe scoped to the current logon SID | Automatic; Unix root overrides are rejected |

On Unix, the socket is `<root>/<32-character id>`. Applications provision custom directories. Mode `0755` is accepted when the current user owns the directory and has read, write and traversal access, with no group or other write permission. C-Two creates its default directory with mode `0700` and sets and verifies each socket as `0600` before listening. Spaces and Unicode are preserved. Bind, connect and liveness probes address the short name through an open directory descriptor, so filesystem directory-opening limits govern the configured path. C-Two manages its endpoint, lease and coordinator files, preserves unrelated files and directories, and does not change existing permissions or recursively create parents.

macOS also rejects extended ACL allow entries that can change directory contents, attributes or permissions, including inherited entries; read, traversal and deny entries are accepted.

```python
from pathlib import Path
import c_two as cc

ipc_dir = Path('/Users/me/Library/Application Support/my-app/ipc')
ipc_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
cc.set_local_endpoint(root=str(ipc_dir))
```

Resources, clients and relay upstreams sharing a local domain use the same directory. Root selection does not move SHM or file spill. The first local I/O attempt freezes this context, including a failed attempt. Credentials preserve their captured scope; controllers use exact credentials after confirming process exit. See the [lifecycle guide](local-endpoint-lifecycle.en.md) and [0.7.4 guide](releases/0.7.4.md).

## Connection deadlines

`cc.connect(CRM, name='resource', address='ipc://server', timeout=0.1)` sets one budget for connection acquisition. Omission or `None` adds no caller deadline and preserves existing phase guards. Zero expires at entry; negative and non-finite values are rejected. Pool waiting, connection, handshake, authoritative route lookup and relay discovery/acquisition share the budget; retries do not restart it. Rust exposes `ConnectOptions::new().with_timeout(Duration::from_millis(100))` and `Runtime::connect_with_options`.

Expiration returns `CallDeadlineExceeded` with `operation=connect`, `transport_phase=pre_dispatch` and the failed `stage` in details. Connection acquisition has not invoked a resource method. After connection, configure business-call waiting separately with `cc.with_call_options(...)`.

## Call deadlines and admission

| Setting | Entry point | Default |
| --- | --- | --- |
| Per-call waiting | `cc.with_call_options(proxy, timeout=...)` | Transport default |
| Logical relay call deadline | `C2_RELAY_CALL_TIMEOUT` | 300 seconds; `0` means unlimited |
| Finite-deadline call slots | `cc.set_call_execution_limits(max_outstanding_calls=...)` / `C2_CALL_MAX_OUTSTANDING` | 1,024 per Runtime |
| Retained input for those calls | `retained_input_budget_bytes=...` / `C2_CALL_RETAINED_INPUT_BUDGET_BYTES` | 16 GiB per Runtime |

Direct IPC waits indefinitely by default; an HTTP budget covers one logical call and its route reacquisition. `timeout=None` explicitly waits indefinitely; `0` expires before dispatch. Negative and non-finite values are rejected. Same-process synchronous calls accept inherited or unlimited waiting only.

An expired deadline ends caller waiting while the dispatched native transaction retains input until actual completion. A zero finite-call count rejects finite calls; a zero byte budget rejects positive input. Unlimited calls are outside these two finite-call admission limits. `cc.call_execution_snapshot()` observes active and retired domains. Examples and error phases are in the [deadline guide](releases/0.7.1.md#per-call-deadlines-for-remote-calls).

## Concurrency and lifecycle

`@cc.read` and `@cc.write` declare access semantics; writes are the default. Registration's `cc.ConcurrencyConfig` sets the route mode, `max_pending` and `max_workers`. The native route handle governs both same-process calls and remote dispatch. See the [Python SDK guide](python-usage.md).

`cc.LifecycleConfig` selects default `Persistent` or explicit `OwnerBound`. OwnerBound uses a private receiver capability prepared by the controller before readiness. `cc.shutdown(timeout=...)` bounds this caller's observation of native drain. Listener closure, work drain and held leases are separate observations; see the [lifecycle guide](local-endpoint-lifecycle.en.md).
