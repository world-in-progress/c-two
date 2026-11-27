# Configuration and defaults

[简体中文](configuration.zh-CN.md)

## Configuration sources and freezing

Rust Core resolves settings in this order: **explicit code or CLI > process environment > `.env` > defaults**. By default it reads `.env` in the current directory. Set `C2_ENV_FILE` in the process environment to select a different file or set it to an empty string to disable file loading. Rust can select sources explicitly with `ConfigSources`. Each role activation captures one `EnvCatalog` for its resolution. Relay startup and `--dry-run` use the same resolver and each reads one catalog. [Environment template](../../.env.example)

## Environment variables

The capacity variables apply independently to each active role domain. `M` is the memory budget in bytes, `F` the file spill budget in bytes, and `N` the outstanding-call limit.

| Environment variable | Default | Explicit entry or purpose |
| --- | --- | --- |
| `C2_ENV_FILE` | Current directory's `.env` | Process environment or Rust `ConfigSources` |
| `C2_RELAY_ANCHOR_ADDRESS` | Unset, direct IPC | `cc.set_relay_anchor(address)` / `Runtime.set_relay_anchor` (`None` clears the code override; blank strings raise `ValueError`), Rust `RuntimeOptions` / `HostOptions` |
| `C2_IPC_ROOT` | Unix `/tmp/c2-v3-<uidhex>` | `cc.endpoint.set_local_endpoint(root=...)`, Rust `LocalEndpointOptions`, c3 `--ipc-root`. Windows uses named pipes. |
| `C2_MEMORY_BUDGET_BYTES` | 16 GiB per active domain | `memory_budget_bytes`, `--memory-budget-bytes` |
| `C2_FILE_SPILL_BUDGET_BYTES` | 16 GiB per active domain | `file_spill_budget_bytes`, `--file-spill-budget-bytes`. Zero disables file spill; used by chunked unary reassembly and relay topic hubs (not client stream/topic receivers or Host topic history). |
| `C2_CALL_MAX_OUTSTANDING` | 0 if `M == 0`, otherwise `max(64, floor(M / 65536))` | `max_outstanding_calls`, `--call-max-outstanding` |
| `C2_MAX_PAYLOAD_SIZE` | 16 GiB | Host's single-payload limit, `max_payload_size` must be positive |
| `C2_MAX_EXECUTION_WORKERS` | Host parallelism clamped to 4–64, with fallback parallelism 4 | Host's `max_execution_workers`. An explicit value must be at least 1 and may exceed 64. |
| `C2_OUTCOME_MAX_ENTRIES` | 4096 | Host's `outcome_max_entries`, at least 1. Caps settled call outcomes across all registrations. |
| `C2_OUTCOME_MAX_RETAINED_BYTES` | `min(64 MiB, server.M / 4)` | Host's `outcome_max_retained_bytes`, from 0 through `server.M`. Zero selects metadata-only retention. |
| `C2_RELAY_BIND` | `127.0.0.1:8080` | `c3 relay --bind` / `-b` |
| `C2_RELAY_EDGE_BIND` | Unset | `c3 relay --edge-bind` |
| `C2_RELAY_TLS_CERT`, `C2_RELAY_TLS_KEY` | Unset | `--tls-cert`, `--tls-key` |
| `C2_RELAY_TLS_AUTO` | `false` | `--tls-auto` |
| `C2_RELAY_TLS_SAN` | Empty | `--tls-san` |
| `C2_RELAY_STATE_DIR` | `~/.c3-relay` | `--state-dir` |
| `C2_RELAY_INSECURE_PLAINTEXT` | `false` | `--insecure-plaintext` |
| `C2_RELAY_ID` | Generated | `c3 relay --relay-id` |
| `C2_RELAY_ADVERTISE_URL` | Derived from the active listener and transport | `c3 relay --advertise-url` |
| `C2_RELAY_SEEDS` | Empty | `c3 relay --seeds` / `-s`, comma-separated relay URLs |
| `C2_RELAY_USE_PROXY` | `false` | Opt in to process HTTP proxy settings for relay traffic. Rust `HostOptions::with_relay_use_proxy` can override it. |

When `M == 0`, the role refuses positive memory reservations and all new business calls, including empty calls and streams. An explicit positive `N` with zero `M` is a configuration error. With positive `M`, an explicit `N`, including 0, takes precedence over the derived default. Refusals, releases and shutdown drain can still proceed with admission closed. [Memory policy](memory-policy.md)

## Role domains and SDK configuration

A Core Runtime creates its `client` domain at the first remote business connection or call attempt and its `server` domain on the first Host start attempt, including startup that fails after freezing. Starting a Host or registering and unregistering its routes through a relay leaves the business client domain inactive. Python `thread_local` calls count against the target Host's `N`. If a process hosts a resource that calls a downstream resource remotely, the two roles have separate budgets and slots. When both inherit the same `M`, their combined accounting limit is `2M`. Process RSS includes other memory.

Python `cc.set_client` and `cc.set_server` accept the three capacity fields in the table. `cc.set_server` also accepts `server_id`, `startup_timeout`, `max_payload_size`, `max_execution_workers` and `admission_hook`. Identity, startup timeout and `admission_hook` have code entries only. Use the corresponding methods on an explicit `cc.Runtime()` to configure that instance. In Rust, `RuntimeOptions` owns client capacity, local endpoint and discovery options. `HostOptions` owns server identity, startup, carrying limits, server capacity and the optional admission hook (`HostOptions::with_admission_hook` / `without_admission_hook`). A Node connector owns a Core Runtime with a client domain only and accepts the three capacity options described in the [C ABI](c-abi.md#client-domain-endpoints-and-calls).

Set the Host's call outcome limits with `cc.set_server(outcome_max_entries=..., outcome_max_retained_bytes=...)` or the corresponding `Runtime.set_server` method. Rust exposes `HostOptions::with_outcome_max_entries` and `HostOptions::with_outcome_max_retained_bytes`. The entry limit counts every settled outcome, including outcomes with no retained bytes. The byte budget is shared across the Host's registrations and defaults to one quarter of the effective server memory budget, rounded down and capped at 64 MiB. At zero, a successful empty response is still recoverable because it needs no retained bytes. [Outcome retention](memory-policy.md#call-outcome-retention)

Updates merge field by field. Omission (`Keep`, Python `_UNSET`) keeps the accepted code override. `Inherit` (Python `None`) clears it so the environment and default apply. `Set` supplies an explicit value. Core validates and commits the complete candidate together. Changing `M` keeps an explicit `N` unless the same update clears or replaces it. Lowering `server.M` below the resolved explicit outcome byte budget fails validation unless the same update makes the retention budget fit.

After a role freezes, Core resolves setter updates against that role's captured sources. Invalid candidates fail configuration validation first; an update with identical effective settings succeeds, including an empty update and reuse of an automatically chosen `server_id`. A valid change to any effective setting fails with Rust `LifecycleError::ConfigFrozen` (projected in Python as `c_two.error.CoreError` with `lifecycle_kind == 'config_frozen'`; there is no separate `ConfigFrozen` Python exception class). The other role can still be configured before it freezes. [Runtime lifecycle](runtime-lifecycle.md)

An explicit `ipc://` address uses direct IPC independently of the relay anchor. Route discovery may select a different relay for HTTP calls. [Contracts and routing](contracts-and-routing.md#relay-discovery-and-forwarding)

## Server startup timeout

Set the native startup wait with `cc.set_server(startup_timeout=...)` in seconds or Rust `HostOptions::with_startup_timeout`. The default is five seconds. Use finite non-negative seconds in Python. Zero expires immediately. Readiness and its direct IPC ping share that budget. Configuration and contract validation precede the wait, and cleanup after expiry has its own shutdown budget. Startup expiry projects as `CoreError.lifecycle_kind == 'startup_timeout'` with `startup_stage` (`readiness` or `responsiveness`) and `timeout_seconds`. [Runtime lifecycle](runtime-lifecycle.md#startup-and-readiness)

## Relay and c3

A standalone relay resolves one `relay` domain at startup. Its memory, file spill and call slots cover business forwarding and its upstream IPC storage across physical connections and reconnects. `c3 relay --dry-run` validates and prints the effective `M`, `F` and `N` without listening. `--upstream` (`-u`) accepts a route and its upstream identity on the CLI only. [c3 relay](../../cli/README.md#relay)

The control listener accepts loopback addresses only. The optional edge listener serves discovery and RPC. The edge defaults to persistent self-signed TLS on non-loopback addresses and plaintext on loopback. The explicit certificate and key must be supplied together. Explicit credentials, `--tls-auto` and `--insecure-plaintext` are mutually exclusive. The SAN and state directory settings apply to automatic TLS.

## Fixed transport constants

| Constant | Value | Use |
| --- | --- | --- |
| `SHM_THRESHOLD` | 16 KiB | IPC payloads at or below this size are inline. Larger payloads can use SHM. |
| `CHUNK_SIZE` | 128 KiB | Standard IPC chunk size. A narrow relay upload window can send shorter valid chunks. |
| `REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB | HTTP payload batching. TCP packet and HTTP frame boundaries are independent. |
| `MAX_HANDSHAKE_FRAME_BYTES` | 8 MiB | Handshake frame bound (also used for route-catalog replies matching an in-flight control request), covering up to 64 routes with 256 methods each. |
| `MAX_RPC_FRAME_BYTES` | 1 MiB | Symmetric RPC frame bound, including chunks and bounded inline error payloads. |
| `WRITE_TIMEOUT` | 30 s | Write and flush budget for one frame. |
| `IDLE_CONNECTION_TIMEOUT` | 60 s | Idle client and relay upstream connection eviction. |
| `POOL_DECAY_SECONDS` | 60 s | Idle buddy segment reclamation. Zero prewarmed and zero minimum retained segments. |
| `CHUNK_ASSEMBLER_TIMEOUT` | 60 s | From last chunk enqueue or copy activity while input remains incomplete. |
| `CHUNK_GC_INTERVAL` | 5 s | Scan interval for expired reassemblies. |
| `RELAY_ANTI_ENTROPY_INTERVAL` | 60 s | Relay mesh anti-entropy interval. |
| `SPILL_THRESHOLD` | 0.8 | Available-memory heuristic for new SHM mappings. |

## Segment geometry and server threads

The request and response main pools each derive their geometry from `B = floor(M / 4)` and cap the segment count at 16. The reassembly pool uses `B = floor(M / 16)` and caps the count at 4. Let `T = min(4, segment_cap)`, `min_size = 64 KiB` and `max_size = 64 MiB`. Below `B = min_size`, buddy allocation is disabled. Below `B = T × min_size`, use segments of `min_size` and count `floor(B / min_size)`. From there, the segment size is `min(max_size, prev_pow2(floor(B / T)))` and the count is `min(segment_cap, floor(B / segment_size))`. Here `prev_pow2` rounds down to the largest power of two. The total capacity of each enabled pool fits its share, and created backing is charged to the domain. At the default 16 GiB, each main pool is at most 16 × 64 MiB and the reassembly pool is at most 4 × 64 MiB. A 1 GiB domain gives each main pool 4 × 64 MiB. Below 256 KiB for the main pool or 1 MiB for the reassembly pool, use the applicable inline, dedicated SHM, chunk or file fallback. A single reassembly is bounded by the route’s advertised `max_payload_size` on the client and the Host’s own value on the server.

Each role domain can reuse up to four idle local buffers of at least 256 KiB each. The idle capacity is bounded by 2 GiB and by the unreserved portion of `M`. When a reservation needs room, the domain evicts its oldest idle buffers first. Idle buffers do not contribute to `used_bytes` or `peak_bytes`.

Let `P` be host parallelism (fallback 4), `W` the configured `max_execution_workers`, and `C = E = A = clamp(P, 4, 64)` the incoming chunk copy, stream item encoding and Host admission hook concurrency limits. The server runs two asynchronous I/O threads and sets Tokio's blocking thread limit to `T = max(512, W + C + E + A + 16)` with checked addition. Permits are acquired before blocking work is submitted and held until it returns, leaving at least 16 threads for listener startup, shutdown and cleanup.

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

Rust and Python direct IPC and relay HTTP business calls, as well as generated TypeScript direct IPC calls, wait without a deadline by default. Generated TypeScript explicit relay and relay-aware transports default `callTimeoutMs` to `300000` ms for unary calls (including relay-aware calls routed over local IPC); pass `callTimeoutMs: 0` to disable that default. Subscriptions never inherit the 300 s unary relay default; generated TypeScript subscription deadlines follow `TopicSubscribeOptions.timeoutMs` > `withCallOptions` view `timeoutMs` > unlimited (`null` is explicit unlimited; `0` expires immediately). Across Rust (`CallOptions`, replacing retired `UnaryCallOptions`), Python (`cc.with_call_options(proxy, *, timeout=..., metadata=...)`) and generated TypeScript (`client.withCallOptions({ timeoutMs?, metadata? })`), reusable client views carry both per-call deadlines and bounded `CallMetadata` across unary, keyed, recovery, query, stream and topic operations. Zero expires before dispatch; negative and non-finite values are rejected. Synchronous same-process calls accept inherited or unlimited waiting. Connection deadlines are independent.

Expiry ends caller waiting. A dispatched call may still run to its real terminal, holding its call slot until then. Only pre-dispatch failures allow safe resubmission. Remote unary calls, including `recover_call`, and server streams count against `N` regardless of deadline. `query_call` has a separate control limit. See [execution admission](runtime-lifecycle.md#execution-admission) and [diagnostic snapshots](memory-policy.md#statistics-and-payload-lifetime).
