# Unreleased

The latest release is [C-Two 0.10.1](0.10.1.md) with the existing c3 0.6.0; its source, publication and cleanup evidence are in the [acceptance record](../acceptance/2026-10-30-0101-release-acceptance.md).

## Smaller public surface (breaking)

The Python and Rust SDKs now keep only the everyday API at their roots. Everything else moves into four modules, grouped by who uses it. The reasons and measurements are in the [interface review](../design/2026-10-31-api-surface-review.md). Update imports; no aliases or old paths remain. Protocol, ABI, SHM and storage formats are unchanged.

### Python

The `c_two` root keeps:

- `crm`, `read`, `write`, `transfer`, `on_shutdown`
- `hold`, `HeldResult`
- `register`, `connect`, `close`, `unregister`, `serve`, `shutdown`, `server_address`, `with_call_options`
- `set_relay_anchor`, `set_server`, `set_client`, `set_transport_policy`, `set_call_execution_limits`
- `Runtime`, `ConcurrencyConfig`, `error`, and the four modules below, so `import c_two as cc` reaches `cc.observe.memory_stats()` without another import

Everything else moved:

| Module | Contents | Renamed |
| --- | --- | --- |
| `c_two.owner` | owner control, recovery tickets, takeover, owned children, `LifecycleConfig` | `NativeOwnerBootstrap` → `OwnerBootstrap`; use `OwnerBootstrap.adopt_owner_stdin()` instead of `cc.adopt_owner_stdin()` |
| `c_two.endpoint` | endpoint contexts and credentials, `inspect_endpoint`, `reap_endpoint`, `sweep_endpoints`, `set_local_endpoint`, `server_credential`, `server_id` | — |
| `c_two.observe` | connection events, `memory_stats` and its types, `call_execution_snapshot` | `native_lifecycle_snapshot` → `lifecycle_snapshot`, `native_terminal_outcome` → `shutdown_outcome` |
| `c_two.contract` | descriptor export, release reference, diagnostics and code generation | `export_contract_descriptor` → `export_descriptor`, `export_contract_release_ref` → `release_ref`, `contract_descriptor_diagnostics` → `diagnostics`, `compile_contract_artifacts` → `compile_artifacts`, `ContractArtifact` → `Artifact`, `ContractArtifactSet` → `ArtifactSet`, `ContractCodegenError` → `CodegenError`, `ContractCodegenTarget` → `CodegenTarget` |

Module functions act on the default Runtime. `Runtime` methods of the same name act on an explicit instance.

### Removed

| Removed | Use instead |
| --- | --- |
| `cc.Held` | `cc.HeldResult` |
| `cc.hold_stats()` | `c_two.observe.memory_stats()["holds"]`; the `budget_cells_note` text field is also gone from the result |
| `ConcurrencyMode` and `InputLifetime` | the strings `'parallel'`, `'exclusive'`, `'read_parallel'` and `'materialized'`, `'borrowed'` |
| defaults for `@cc.crm(namespace=..., version=...)` | write both explicitly |
| `infer_crm_from_resource` and `c3 contract infer` | write a CRM class, then export it with `c3 contract export` |
| the resource bridge: `cc.bridge`, `cc.ResourceBridge`, `register(bridge=...)` | register a small adapter class that converts between `Payload` and domain values; every call path invokes it the same way |

`shutdown`, `call_execution_snapshot`, `lifecycle_snapshot`, `shutdown_outcome`, `inspect_endpoint` and `reap_endpoint` now return typed dictionaries. `c_two.error` declares `__all__`.

### Rust

The `c_two` crate root keeps runtime, hosting, connection, configuration-source, concurrency and error types. Endpoint administration, owner control, diagnostics and contract types move to `c_two::endpoint`, `c_two::owner`, `c_two::observe` and `c_two::contract`.

`direct_ipc_endpoint`, `ping_direct_ipc` and `shutdown_direct_ipc` take an `Option<&LocalEndpointContext>` and replace the `*_with_context` functions; `None` resolves the process configuration as before. Generated Rust modules use the new paths, so regenerate them.

### Validation

Local macOS validation passed on the integrated candidate, with C-Two and FastDB 0.2.4 checked out side by side, and the full fork CI run [41173746331](https://github.com/Dsssyc/c-two/actions/runs/41173746331) on `f0f9efaa` passed on Linux, macOS, Windows x64 and Windows ARM64.

## Defect fixes and dead-code removal (breaking)

These come from the [over-defense review](../design/2026-10-31-over-defense-review.md) and its [batch plan](../design/2026-10-31-defect-and-dead-code-batches.md).

### Fixed

- **Large chunked payloads.** Payloads larger than 64 MiB no longer fail when they travel as chunks, for example when shared memory is unavailable. A request is limited only by `max_payload_size` and `max_reassembly_bytes`; an oversized request reports a size error, not a protocol violation.
- **Malformed input is an error.**
  - A malformed call-control frame closes the connection with a protocol error, instead of leaving the caller to wait until its deadline.
  - A malformed shutdown acknowledgement, and a malformed control address, raise errors instead of reporting "not acknowledged" or `False`.
  - `c3 registry` reports malformed relay responses instead of printing "No routes registered."
- **Local endpoints.**
  - A custom IPC root that already holds more than 64 unrelated entries initializes normally.
  - Listeners dropped while another process briefly holds the namespace gate retire their socket and lease in the background; if the gate stays held, the complete record is left for the reaper.
- **Configuration.** A `pool_segment_size` whose buddy layout would overflow `u32` is rejected when configured, instead of silently disabling buddy shared memory. Explicit `max_execution_workers` values above 64 are accepted; only the default stays capped.
- **Relay.**
  - Mesh peers must speak the same protocol version; older peers are no longer accepted.
  - The HTTP `/_register` path and `c3 relay --upstream` share one registration that retries only transport errors, never a version mismatch.
- **TypeScript.** Generated TypeScript accepts NACKs that carry error codes it does not list.
- **Python.**
  - Relay connection errors keep their own type instead of all becoming `RegistryUnavailable`.
  - `cc.close()` with something other than a connected CRM raises `TypeError`.
  - A closed proxy no longer breaks `hasattr()`.
  - Signature errors in transfer bindings are reported instead of silently treated as "no payload".

### Removed

| Removed | Instead |
| --- | --- |
| `C2_RELAY_ROUTE_MAX_ATTEMPTS` | relay-aware clients always make one route observation plus at most one refresh |
| the 4096-address sweep cap and the relay's 1024-owner cap | — |
| the duplicate `active_leases` and `total_leased_bytes` counters in `memory_stats()["holds"]`, and `retention` in lease snapshots | `active_holds`, `total_held_bytes` |
| `http_clients_drained` and `ipc_clients_abnormally_closed` in shutdown outcomes | count `ipc_client_close_details` |
| Rust: `FreeResult` | `free_at` and `release_handle` return `()` |
| Rust: the `SyncClient` preallocation API, injected pools and pool-less `IpcClient` constructors | the configured `IpcClient::with_config` / `with_endpoint` constructors |
| Rust: IPC route-watch recovery, the relay route event history, and unused `RelayAwareHttpClient` entry points | — |
| Rust: `ContractLimits`, `ContractLimitsProfile` and the `*_with_limits` functions | contract limits are fixed constants |
| Rust: the never-constructed `RelayDuplicateRoute` and `RelayHttp` errors, and the test-only aggregate config resolver | — |
| Python native: the producerless server-side writable sink | resources return bytes or buffer-protocol data |

Rust `MemPool` constructors now return `Result` instead of panicking on an invalid name or geometry.

### Validation

Local macOS validation passed on the integrated candidate, with C-Two and FastDB 0.2.4 checked out side by side, and the full fork CI run [41192397960](https://github.com/Dsssyc/c-two/actions/runs/41192397960) on `e1630cf1` passed on Linux, macOS, Windows x64 and Windows ARM64. An earlier run on `8b756c74` found two Core tests that the batches had left stale; both were corrected before this run.

## Owner subsystem removed (breaking)

The owner subsystem is removed without replacement ([ADR 0002](../decisions/0002-remove-owner-subsystem.md)). No known consumer used it, and no internal mechanism depends on it. Supervise host processes with ordinary process management and shut hosts down with `shutdown()`.

| Removed | Notes |
| --- | --- |
| Python `c_two.owner`: `OwnerBootstrap`, `OwnerControl`, `OwnerRecoveryTicket`, `PreparedOwnerTakeover`, `OwnerStatus`, `OwnerControlError`, `OwnedChild`, `owner_control_pair`, `prepare_owner_takeover`, `spawn_owned_child` | The `c_two` root keeps the `endpoint`, `observe` and `contract` modules. |
| `LifecycleConfig` and the `lifecycle` / `owner_control` arguments of `set_server` | Every host runs until it is shut down. |
| Rust `c_two::owner`, `OwnerControlErrorKind`, `OwnerPhase`, `ServerLifecyclePolicy` | — |
| The IPC owner-control connection role and its `0x16` frame | Ordinary handshake and SIGNAL control are unchanged. |
| The `owner_control` startup stage and the owner phases of host lifecycle snapshots | Startup stages are `readiness` and `responsiveness`. |

## Documentation layout

The documentation is organized by role, so links into the old tree no longer resolve. Current behavior is in `docs/reference/`, open work and plans in `docs/design/` with the [backlog](../design/backlog.md), and verification records in `docs/acceptance/`; the [documentation index](../README.md) routes each subject to the document that owns it. [`CONTEXT.md`](../../CONTEXT.md) is the glossary, with Chinese and English terms. AGENTS.md now fits within the 32 KiB that Codex reads.

### Validation

The full fork CI run [41218012397](https://github.com/Dsssyc/c-two/actions/runs/41218012397) on `08b8c65c` passed on Linux, macOS, Windows x64 and Windows ARM64.

## Single-owner shared-memory release (breaking)

Each shared-memory block now has one owner ([ADR 0001](../decisions/0001-single-owner-release.md), [design](../design/2026-11-01-protocol-batch-design.md)). The owner lends the block; the receiver copies it into a local buffer and returns the loan in a release frame right after the copy. Requests are released before the resource method runs and responses before decoding or delivering a hold, so a slow resource method or a long `cc.hold` no longer keeps the peer's shared memory.

### Upgrade together

IPC handshake 15, SHM 4, C/FFI ABI 7 and relay mesh protocol 7 replace 14, 3, 6 and 6. A 0.10.x peer receives a typed version mismatch through the unchanged fixed preamble. Upgrade the Python and Rust SDKs, c3 and Node consumers on one host together, and stop old services before restarting.

### Behavior

- `cc.hold` and borrowed inputs keep local copies. `HeldResult` usage is unchanged: `release()` invalidates the FastDB owner once and frees the copy, and it no longer fails or retries because of transport memory.
- A peer that has not copied a block, for example a stopped process or one that does not read its connection, keeps that loan until it releases it or its connection ends (read EOF, or a read-side ECONNRESET on Unix). Nothing reclaims a loan after a wait or timeout.
- A low-level Host shutdown no longer seals the Runtime's call domain or outgoing clients; only an explicit `Runtime.shutdown()` / `cc.shutdown()` retires them. A low-level shutdown with no Host completes immediately.
- Request IDs are 64-bit end to end instead of being truncated to 32 bits inside the client, and generated TypeScript carries them and loan IDs as `bigint`.
- `max_frame_size` must be at least 526 bytes, the size of a full release frame.
- A peer that stops while it still holds loans keeps shutdown at `completed=False`, returned when the shutdown budget ends, until the peer releases them or its connection ends. 0.10.1 aborted such a connection at once, because the shared allocator let the server free the blocks itself.

### Removed

| Removed | Instead |
| --- | --- |
| The automatic IPC heartbeat: `heartbeat_interval`, `heartbeat_timeout`, `C2_IPC_HEARTBEAT_INTERVAL`, `C2_IPC_HEARTBEAT_TIMEOUT`, and the `heartbeat_timeout` end reason | Connections end on OS facts and calls on their own deadlines; manual ping remains |
| `abandoned_releases` in every `memory_stats()` scope, `holds.by_storage`, and `storage` in hold sweeps | `holds` counts local copies with `active_holds`, `total_held_bytes`, `oldest_hold_seconds` and `by_direction` |
| Python `MemPool.data_addr`, `seg_data_info`, `open_peer`, `ensure_peer_segment`, `read_at`, `free_at`, `destroy`, and `PoolConfig(dedicated_crash_timeout_secs=...)` | The local allocation APIs; a pool is freed with its last owner |
| Lifecycle snapshot `phase`, `terminal` and `client_held_leases` (Python `ClientHeldLeases`; Rust `HostLifecyclePhase`, `HostClientHeldLeases`) | `finished`, `listener_closed` and `work_drained` |
| The dictionary returned by direct IPC `shutdown()` (`acknowledged`, `shutdown_started`, `server_stopped`, route outcomes); Rust `DirectIpcShutdownRouteOutcome` | `'initiated'`, `'absent'` or `'unconfirmed'`; Rust `DirectIpcShutdownOutcome::{Initiated, Absent, Unconfirmed}` |
| Route revisions in route bindings, call tokens, relay resolution, HTTP headers and the mesh; IPC `RouteWatch` and route-contract queries | Route UID, server instance and contract identify a route |
| C/FFI ABI 6 request and response pools, including `request_pool_read_local`, `request_pool_forget_consumed` and `response_pool_release`; TypeScript `createC2MemFfiNativeResponseShmReader`, the `responseShmReader` and `requestShmWriter` options, and block `allocationId` | One ABI 7 memory context per verified connection; blocks carry `loanId: bigint` ([C ABI](../reference/c-abi.md)) |
| Rust Core `HeldResponse` leases and pins, `has_pending_release`, `abandon`, `RequestLease` and `ResponseLease` | `Held` and `HeldResponse` own a local copy |

### Further changes

Found by the strict comparison against 0.10.1 and not listed above:

- Python:
  - `BaseIPCOverrides`, `ServerIPCOverrides` and `ClientIPCOverrides` live in `c_two.config`; `InputLifetimeLike`, `CoreError.body` and `Server.get_slot_info` are removed, and a proxy's tracker is `local_copy_tracker` (was `lease_tracker`).
  - `c_two.mem`: `PoolAlloc.allocation_id` and `PoolStats.dedicated_pending_free_bytes` are removed; `max_dedicated_segments` above 65536 raises `ValueError`; a dedicated segment no longer reserves a 64-byte header (0.10.1 could write 64 bytes past it).
- Rust: `Held::release` consumes the value even when invalidation fails or panics, and a later call returns `Ok`. The Core crate `c2-mem` changes its low-level API: `free_at` takes the level, `ensure_peer_dedicated` takes the generation, `release_handle` panics on a handle from another pool, `BuddySegment::allocator()` panics on a peer segment, and `destroy`, `gc_dedicated`, `SegmentHeader` and `ShmSpinlock` are removed.
- TypeScript: options accept an explicit `undefined` for `ipcAddress`, `serverId`, `serverInstanceId`, the two `observe` callbacks and `C2Fetch.signal`.
- Wire: `RouteAck` (0x14), `RouteRecord.owner_epoch` and `lease_deadline_ms`, `RouteNack.nonce`, `DigestDiffEntry.hash` and the `FLAG_SHM`, `FLAG_POOL` and `FLAG_DISK_SPILL` flags are removed. A route's state is `ready` or `closed`, its reason is one of `register_committed`, `explicit_unregister` or `shutdown` and is always present, and the route directory and the mesh reject unknown fields.
- Connections:
  - Aborting or dropping a local stream no longer forces an OS disconnect: the handle closes when its last holder releases it, so a forked child that inherited the descriptor keeps the peer from seeing the end.
  - The close notification is published when the physical stream retires, not when the end is first recorded.
  - A write timeout deactivates the connection instead of aborting the stream.
  - Calls not yet written when a connection deactivates, or before it connects, fail as pre-dispatch and are retry-safe; 0.10.1 reported them as dispatch-uncertain.
  - A chunked reply larger than `max_reassembly_bytes` fails with a capacity error, like a request.
  - A malformed shutdown frame closes the connection instead of receiving a negative acknowledgement.
- Shutdown and errors:
  - Repeating a low-level Rust `Runtime::shutdown` returns the complete journal instead of an empty one.
  - `ipc_client_close_details` lists only connections whose first end was not a local close, with the operation as the reason.
  - A failed request materialization reports `ResourceInputDeserializing` instead of `ProtocolViolation`.
  - Unregistering the same route twice concurrently returns `True` both times.

### Validation

Pending: host checks of each block are recorded in the [backlog](../design/backlog.md); the exact-candidate CI run and the paired benchmarks follow the last block.
