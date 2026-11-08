# C-Two configuration

This guide owns process, IPC, relay, concurrency and lifecycle configuration; Rust `c2-config` owns defaults, environment resolution and validation, SDKs provide code overrides, and c3 provides command-line overrides.

English · [简体中文](configuration.zh-CN.md)

## Configuration sources and freezing

IPC tuning values follow one rule: every `ipc_overrides` key `name` has the environment variable `C2_IPC_<NAME>` (upper case), resolved by the precedence below. A budget for a single operation is a parameter of that call, such as `connect(timeout=...)` or `shutdown(timeout=...)`.

Precedence is **explicit code or CLI > process environment > `.env` > Rust defaults**. The default environment file is `.env` in the current working directory. Set `C2_ENV_FILE` to another file, or to an empty string to disable file loading. [`.env.example`](../../.env.example) contains the environment reference.

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

Set Runtime policy before registering resources or connecting clients. Each native configuration domain freezes when first used: local endpoints on the first bind/connect attempt, including a failed attempt; call admission on the first remote business call. Queries do not freeze configuration. Later environment changes do not alter a frozen choice.

Python exposes `cc.Runtime()` for an independently configured instance. The module-level functions use the default Runtime; each explicit instance owns its code overrides and native session and does not copy the default Runtime's overrides. All instances still read the same process environment and `.env` through the native resolver. Configure a particular instance with `runtime.set_*()` before its freeze point instead of changing `os.environ` to target one Runtime. `None` retains each setter's existing clear/inherit meaning.

## SDK and transport configuration

`cc.set_server(...)` sets server identity, IPC overrides and lifecycle policy. `cc.set_client(...)` sets client IPC overrides. `cc.set_transport_policy(...)` controls the default Runtime transport policy; `runtime.set_transport_policy(...)` configures that explicit instance.

```python
import c_two as cc

cc.set_transport_policy(
    shm_threshold=64 * 1024,
    remote_payload_chunk_size=1024 * 1024,
)
cc.set_server(
    server_id='my-server',
    startup_timeout=30.0,
    ipc_overrides={'max_payload_size': 64 * 1024 * 1024, 'pool_enabled': False},
)
cc.set_client(ipc_overrides={'pool_enabled': False})
```

Typed overrides are `BaseIPCOverrides`, `ServerIPCOverrides` and `ClientIPCOverrides`. Server-only fields are rejected as client overrides. Native parsing validates all keys and values.

| Purpose | Code entry point | Environment | Default |
| --- | --- | --- | --- |
| Relay registration and discovery | `cc.set_relay_anchor(url)` | `C2_RELAY_ANCHOR_ADDRESS` | No relay |
| Server startup wait | `cc.set_server(startup_timeout=...)` | — | 5 seconds |
| SHM/inline threshold | `cc.set_transport_policy(shm_threshold=...)` | `C2_SHM_THRESHOLD` | 4 KiB |
| Remote payload batch size | `cc.set_transport_policy(remote_payload_chunk_size=...)` | `C2_REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB |
| Server frame limit | `set_server(ipc_overrides={'max_frame_size': ...})` | `C2_IPC_MAX_FRAME_SIZE` | 2 GiB |
| Logical payload limit | `set_server(ipc_overrides={'max_payload_size': ...})` | `C2_IPC_MAX_PAYLOAD_SIZE` | 16 GiB |
| Pending requests across the server | `set_server(ipc_overrides={'max_pending_requests': ...})` | `C2_IPC_MAX_PENDING_REQUESTS` | 1,024 |
| Resource execution threads | `set_server(ipc_overrides={'max_execution_workers': ...})` | `C2_IPC_MAX_EXECUTION_WORKERS` | Default uses host parallelism clamped to 4–64; explicit values must be positive and are not capped to 4–64 |

An explicit `address='ipc://...'` uses local IPC independently of relay discovery. The relay anchor handles registration and discovery; HTTP calls use the relay URL selected by resolution. Remote batch size controls C-Two payload batches, without specifying TCP packets or HTTP frames.

A Node connector from `createNodeIpcConnector` fixes its client settings when it is created. Its `requestShmThreshold` and `requestChunkSize` options set the SHM threshold and the client `chunk_size`; every other setting, including the payload buffer budget and `C2_CALL_MAX_OUTSTANDING`, comes from the environment, as for a Runtime without overrides.

## Server startup timeout

`cc.set_server(startup_timeout=seconds)` configures the Core startup wait before the first registration. The native `RuntimeSession` constructor and `set_server_options` accept the same keyword. Values must be finite, non-negative seconds representable by the native clock; zero expires immediately. Omitted/`None` selects the existing finite five-second default, not unlimited waiting. This default bounds unattended startup; it is not a performance guarantee for a busy runner. Applications should choose their deployment budget explicitly.

Core owns and freezes server configuration on the first Host start attempt, including failure. `runtime.start()` is also a first Host attempt, even with no CRM. The setting survives pre-use RuntimeSession replacement by `set_transport_policy`. `cc.shutdown()` or explicit `runtime.shutdown()` creates a configurable fresh session when it consumes a completed result. After pending, call SDK shutdown again; background completion alone does not replace the session. Rust resolution is explicit `HostOptions::with_startup_timeout` > `RuntimeOptions::server_startup_timeout` > five seconds. Readiness and its direct IPC ping share the wait budget. Configuration/contract validation precedes this wait, and cleanup after expiry retains its separate shutdown budget, so this is not a total wall-clock limit on `cc.register()` or `runtime.start()`.

Startup expiry raises public `c_two.error.CoreError` with `category='lifecycle'`, `lifecycle_kind='startup_timeout'`, `startup_stage` (`readiness` or `responsiveness`) and `timeout_seconds`. Its `io_operation`, `io_kind` and `raw_os_error` are `None`. Local bind failures retain their original structured OS cause and no longer carry the misleading `config error` message prefix. These diagnostics do not authorize retry or endpoint cleanup. See the [public error field table](local-endpoint-lifecycle.md#public-structured-python-errors) for types and value ranges.

## IPC memory and chunks

Set the following fields through the appropriate endpoint's `ipc_overrides`. Allocation, accounting and fallback behavior are described in the [memory policy](memory-policy.md).

| Override | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `pool_enabled` | `C2_IPC_POOL_ENABLED` | `true` | Enable or disable the buddy SHM pool; dedicated SHM remains available |
| `pool_segment_size` | `C2_IPC_POOL_SEGMENT_SIZE` | 256 MiB | Buddy pool segment size in bytes |
| `max_pool_segments` | `C2_IPC_MAX_POOL_SEGMENTS` | 4 | Maximum buddy pool segments, from 1 to 255 |
| `pool_prewarm_segments` | `C2_IPC_POOL_PREWARM_SEGMENTS` | 0 | Buddy segments mapped eagerly at startup; 0 keeps allocation lazy |
| `pool_min_retained_segments` | `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` | 0 | Minimum idle buddy segments kept after reclamation |
| `pool_decay_seconds` | `C2_IPC_POOL_DECAY_SECONDS` | 60 seconds | Idle segment decay time in seconds |
| `shm_backing_budget_bytes` | `C2_IPC_SHM_BACKING_BUDGET_BYTES` | 8 GiB | Byte budget for locally created buddy and dedicated SHM backing |
| `file_backing_budget_bytes` | `C2_IPC_FILE_BACKING_BUDGET_BYTES` | 16 GiB | Byte budget for locally created file backing |
| `live_reassembly_budget_bytes` | `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` | 8 GiB | Byte budget for incomplete and retained reassembly capacity |
| `reassembly_segment_size` | `C2_IPC_REASSEMBLY_SEGMENT_SIZE` | 64 MiB | Reassembly pool segment size in bytes |
| `reassembly_max_segments` | `C2_IPC_REASSEMBLY_MAX_SEGMENTS` | 4 | Maximum reassembly pool segments, from 1 to 255 |
| `max_inflight_chunk_frames` | `C2_IPC_MAX_INFLIGHT_CHUNK_FRAMES` | 512 | Server-wide limit on chunk frames processed at once; a chunk that finds no free slot fails its request with `ResourceUnavailable`. When that many reassemblies are in flight, admitting another first sweeps expired assemblies; the sweep never rejects |
| `chunk_size` | `C2_IPC_CHUNK_SIZE` | 128 KiB | Individual chunk size in bytes |
| `chunk_threshold_ratio` | `C2_IPC_CHUNK_THRESHOLD_RATIO` | 0.9 | Configured fraction of maximum frame size; accepted and validated, but not currently used by transport selection |
| `chunk_assembler_timeout` | `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` | 60 seconds | Incomplete chunk-reassembly timeout in seconds |
| `chunk_gc_interval` | `C2_IPC_CHUNK_GC_INTERVAL` | 5 seconds | Chunk garbage-collection sweep interval in seconds |
| `max_reassembly_bytes` | `C2_IPC_MAX_REASSEMBLY_BYTES` | 8 GiB | Maximum reassembly buffer memory in bytes |
| `write_timeout` | `C2_IPC_WRITE_TIMEOUT` | 30 seconds | Seconds one frame write may take before its connection is deactivated |
| `client_idle_timeout` | `C2_IPC_CLIENT_IDLE_TIMEOUT` | 60 seconds | Seconds a Runtime keeps a cached client connection that nothing references before it evicts the connection in the background; `0` disables timed eviction |
| `max_dedicated_segments` | `C2_IPC_MAX_DEDICATED_SEGMENTS` | 4 | Dedicated SHM segments one pool keeps at once, up to 65536 |
| `spill_threshold` | `C2_IPC_SPILL_THRESHOLD` | 0.8 | Fraction of observed available memory that new SHM backing may reach, from 0 to 1; 1 disables the check |

Pool segment capacity and the per-message payload limit are independent. Rounding `pool_segment_size` up to a power of two must fit in u32, so its maximum is 2 GiB. Disabling the buddy pool still permits dedicated SHM, checked chunk transfer and file spill. A zero backing or reassembly budget rejects positive reservations; `pool_decay_seconds=0` reclaims idle segments on the next maintenance pass.

`cc.observe.memory_stats()` observes native memory domains, and `cc.observe.memory_stats()["holds"]` observes SDK leases. These are accounting snapshots, without measuring total process RSS.

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
| Relay identity | `--relay-id` | `C2_RELAY_ID` | Generated `<hostname>_<pid>_<first 8 UUID characters>` |
| Advertised URL | `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | Derived from the bound address |
| Mesh seeds | `--seeds` | `C2_RELAY_SEEDS` | Empty |
| Upstream IPC idle disconnect | `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | 60 seconds; `0` disables timed eviction |
| Forwarding transaction count | `--call-max-outstanding` | `C2_CALL_MAX_OUTSTANDING` | 1,024 per relay |
| Payload buffer budget | `--call-payload-buffer-budget-bytes` | `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES` | 16 GiB per relay |
| System HTTP proxy | — | `C2_RELAY_USE_PROXY` | `false` |
| Anti-entropy interval | — | `C2_RELAY_ANTI_ENTROPY_INTERVAL` | 60 seconds; `0` disables anti-entropy |

The relay binds its listener before anything else starts. With port `0` it takes a free port, derives the advertised URL from the address it actually bound, and `c3 relay` prints that address in its `C-Two relay listening at` line; any other port derives the same URL as before.

`remote_payload_chunk_size` is the single remote-body batching policy, resolved by Rust from the typed transport override or `C2_REMOTE_PAYLOAD_CHUNK_SIZE`. It defaults to 1 MiB and accepts positive values up to 128 MiB. Relay HTTP uses it for outbound request bodies, and a relay copies each response or stream-item slice out of its upstream carrier in at most the lesser of this size and the upstream IPC `chunk_size`; `C2_IPC_CHUNK_SIZE` independently controls IPC wire chunks and reassembly. The remote value does not specify TCP packets or HTTP frame boundaries.

Before a call hands its input to a transport, Core acquires the client's route again at most once, under that call's deadline, when its target is known to be gone; this bound is fixed and is not configurable through SDK settings.

Pre-register upstreams as `--upstream NAME=SERVER_ID@ADDRESS`; `SERVER_ID` must match the IPC handshake. See the [c3 guide](../../cli/README.md) for upstream budgets, mesh and dry-run options.

Relay freezes forwarding limits at startup and applies them to all forwarded business calls, including calls with unlimited caller waiting. A zero count limit rejects new forwards; a zero payload buffer budget rejects every positive reservation. A known-length request body is reserved once before it is read; when the budget is short, the forward waits until capacity is released or the HTTP caller disconnects. An unknown-length body reserves its first part the same way, and later growth that finds no capacity fails at once. Capacity refusal preserves the route. These limits are independent of upstream IPC backing and reassembly budgets and do not measure process RSS; the [memory policy](memory-policy.md#payload-buffer-budget) lists what the payload buffer budget charges.

Relay capacity rejection is fixed before business admission. Every refusal the relay sends before it reads a request body, whether for a contract, identity or route mismatch, an invalid or oversized declared length, a rejected request format or capacity, first discards the body frame by frame within its declared length and one two-second deadline, so that a client still sending the body receives the refusal rather than a reset connection. Disposal does not aggregate the payload, decode it, invoke a resource or retry admission, and capacity released during disposal does not change a capacity refusal. Bodies of requests with Expect or of unknown length are not read. Read faults, excess bytes and the timeout preserve the original refusal. An incomplete or faulty transport can still prevent the client from receiving it.

After an HTTP caller disconnects, an already-dispatched forward keeps its upstream connection until actual completion; its request body is released once it has been copied upstream. Relay shutdown stops admission, ends the server streams it forwards between their steps with `CallCancelled` naming relay shutdown, and waits for forwarding and native client cleanup; unary calls already forwarded still finish. A resource method that never returns, or a stream client that has stopped reading, continues to occupy its slot and delays shutdown.

## Local endpoint directories

| Platform | Endpoint | Location configuration |
| --- | --- | --- |
| Unix | UDS in the final owner-controlled directory | `cc.endpoint.set_local_endpoint(root=...)`, `C2_IPC_ROOT`, c3 `--ipc-root`; default `/tmp/c2-v3-<uidhex>` |
| Windows | Named Pipe scoped to the current TokenUser SID | Automatic; Unix root overrides are rejected |

On Unix, the socket is `<root>/<32-character id>`. Applications provision custom directories. Mode `0755` is accepted when the current user owns the directory and has read, write and traversal access, with no group or other write permission. C-Two creates its default directory with mode `0700` and sets and verifies each socket as `0600` before listening. Spaces and Unicode are preserved. Bind, connect and liveness probes address the short name through an open directory descriptor, so filesystem directory-opening limits govern the configured path. C-Two manages its endpoint, lease and coordinator files, preserves unrelated files and directories, and does not change existing permissions or recursively create parents.

For the macOS default namespace, gate/marker/lease records live in `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`, separate from sockets under `/tmp/c2-v3-<uidhex>`; aliases share one coordinator. Both default locations change with namespace disk format 3. Non-default custom roots and Linux retain colocated records. The `v3` default-directory suffix is the namespace disk format version; custom roots gain no suffix. Different formats have separate default directories and need no same-path cold switch. Sharing one custom physical root across formats is unsupported; on `unsupported-namespace-format`, rebuild only after all old processes exit, cannot restart and the full root is safely archived offline. See the [lifecycle guide](local-endpoint-lifecycle.md) for record and exact-reap rules.

macOS also rejects extended ACL allow entries that can change directory contents, attributes or permissions, including inherited entries; read, traversal and deny entries are accepted.

```python
from pathlib import Path
import c_two as cc

ipc_dir = Path.home() / 'my-app' / 'ipc'
ipc_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
cc.endpoint.set_local_endpoint(root=str(ipc_dir))
```

Resources, clients and relay upstreams sharing a local domain use the same directory. Root selection does not move SHM or file spill. The first local I/O attempt freezes this context, including a failed attempt. Credentials preserve their captured scope; controllers use exact credentials after confirming process exit. See the [lifecycle guide](local-endpoint-lifecycle.md) and [0.7.4 guide](../releases/0.7.4.md).

## Connection deadlines

`cc.connect(CRM, name='resource', address='ipc://server', timeout=0.1)` sets one budget for connection acquisition. Omission or `None` adds no caller deadline and preserves existing phase guards. Zero expires at entry; negative and non-finite values are rejected. Pool waiting, connection, handshake, authoritative route lookup and relay discovery/acquisition share the budget; retries do not restart it. Rust exposes `ConnectOptions::new().with_timeout(Duration::from_millis(100))` and `Runtime::connect_with_options`.

Expiration returns `CallDeadlineExceeded` with `operation=connect`, `transport_phase=pre_dispatch` and the failed `stage` in details. Connection acquisition has not invoked a resource method. After connection, configure business-call waiting separately with `cc.with_call_options(...)`.

## Call deadlines and admission

| Setting | Entry point | Default | Meaning |
| --- | --- | --- | --- |
| Per-call waiting | `cc.with_call_options(proxy, timeout=...)` | Transport default | Caller waiting budget for one remote business call |
| Logical relay call deadline | `C2_RELAY_CALL_TIMEOUT` | 300 seconds; `0` means unlimited | Relay HTTP CRM call timeout in seconds; 0 disables the reqwest total timeout |
| Outstanding finite-deadline calls | `cc.set_call_execution_limits(max_outstanding_calls=...)` / `C2_CALL_MAX_OUTSTANDING` | 1,024 per Runtime | Maximum unfinished calls with a finite deadline, including calls that continue after their caller's deadline, plus every server stream until its real terminal |
| Payload buffer budget | `payload_buffer_budget_bytes=...` / `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES` | 16 GiB per Runtime | Byte budget for the payload buffers C-Two keeps for calls with and without a deadline; see [memory policy](memory-policy.md#payload-buffer-budget) |

Direct IPC waits indefinitely by default; an HTTP budget covers one logical call and its route reacquisition. `timeout=None` explicitly waits indefinitely; `0` expires before dispatch. Negative and non-finite values are rejected. Same-process synchronous calls accept inherited or unlimited waiting only.

Expiry ends the caller's wait, while a dispatched native transaction may continue to its real end; its input was already released when the transport copied it. A call deadline alone does not withdraw a relay route or invalidate an already acquired held response owner. The deadline error distinguishes expiry before dispatch from an uncertain dispatched call; do not replay a call after the latter outcome.

A zero finite-call count rejects every new finite call and every new server stream; unary calls without a deadline are not counted. A zero payload buffer budget rejects every positive reservation, for calls with and without a deadline. When the budget is short, a call waits for capacity within its own deadline; the [memory policy](memory-policy.md#payload-buffer-budget) describes what the budget charges and when a reservation fails at once. `cc.observe.call_execution_snapshot()` observes the call slots of active and retired domains, and `cc.observe.memory_stats()['payload_buffer']` observes the budget's bytes and rejections. Examples and error phases are in the [deadline guide](../releases/0.7.1.md#per-call-deadlines-for-remote-calls).

## Concurrency and lifecycle

`@cc.read` and `@cc.write` declare access semantics; writes are the default. Registration's `cc.ConcurrencyConfig` sets the route mode, `max_pending` and `max_workers`. The native route handle governs both same-process calls and remote dispatch. See the [Python SDK guide](python-usage.md).

Server `max_pending_requests` uses one semaphore for all connections, counting complete remote calls awaiting or using execution capacity; it is not multiplied by the connection count. `max_execution_workers` caps active remote callbacks separately. The route's own `max_pending` and `max_workers` limits are shared with same-process calls. See [execution admission](runtime-lifecycle.md#execution-admission).

After acquiring the Python instance lock, `cc.shutdown(timeout=...)` uses one deadline for Native Host, Core and pool lock waits and drain observation. The instance-lock wait is outside that deadline; expiry does not cancel an already-started native drain. Listener closure and work drain are separate observations; `cc.observe.memory_stats()` counts the local copies retained by holds. See the [lifecycle guide](local-endpoint-lifecycle.md).

## Native configuration authority

Paths: `sdk/python/src/c_two/config/`, `core/foundation/c2-config/`

Unified configuration is resolved by the Rust `c2-config` resolver. Python stores SDK code-level overrides and asks the native resolver for environment, `.env`, and default values. Relay server configuration belongs to the standalone Rust `c3 relay` runtime.

| File | Purpose |
| --- | --- |
| `settings.py` | `C2Settings` facade for Runtime-owned code overrides such as relay anchor and transport policy |
| `ipc.py` | Typed override schemas: `BaseIPCOverrides`, `ServerIPCOverrides`, `ClientIPCOverrides`; Rust owns defaults and validation |

IPC override key validation belongs to Rust/native config parsing. Python `ipc.py` may expose typed override schemas and forward mappings, but it must not keep allowed-key or forbidden-key validation tables.

Do not reintroduce Python-side default validation for IPC or relay internals. SDKs should provide typed override facades and leave environment/default resolution to Rust.

Scheduler-related config follows the same boundary. Python may expose `ConcurrencyConfig` and SDK-level enums, but Rust now owns the resolved route concurrency handle, including mode, `max_pending`, `max_workers`, and close state. Python must pass the full typed config into Rust, then treat the native handle as the source of truth for both same-process direct calls and remote dispatch. Do not keep a second Python-owned scheduler state or hidden default policy alive after registration.

Do not add environment variables for values that can be derived from existing canonical settings.

Runtime identity, pools, and transaction projections follow the [Runtime authority](runtime-lifecycle.md#runtime-authority-and-python-bindings).
