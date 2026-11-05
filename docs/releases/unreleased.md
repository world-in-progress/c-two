# Unreleased

The latest release is [C-Two 0.10.1](0.10.1.md) with the existing c3 0.6.0; its source, publication and cleanup evidence are in the [acceptance record](../acceptance/2026-10-30-0101-release-acceptance.md).

## Smaller public surface (breaking)

The Python and Rust SDKs now keep only the everyday API at their roots. Everything else moves into three modules, grouped by who uses it. The reasons and measurements are in the [interface review](../design/2026-10-31-api-surface-review.md). Update imports; no aliases or old paths remain.

### Python

The `c_two` root keeps:

- `crm`, `read`, `write`, `transfer`, `on_shutdown`
- `hold`, `HeldResult`
- `register`, `connect`, `close`, `unregister`, `serve`, `shutdown`, `server_address`, `with_call_options`
- `set_relay_anchor`, `set_server`, `set_client`, `set_transport_policy`, `set_call_execution_limits`
- `Runtime`, `ConcurrencyConfig`, `error`, and the three modules below, so `import c_two as cc` reaches `cc.observe.memory_stats()` without another import

Everything else moved:

| Module | Contents | Renamed |
| --- | --- | --- |
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

The `c_two` crate root keeps runtime, hosting, connection, configuration-source, concurrency and error types. Endpoint administration, diagnostics and contract types move to `c_two::endpoint`, `c_two::observe` and `c_two::contract`.

`direct_ipc_endpoint`, `ping_direct_ipc` and `shutdown_direct_ipc` take an `Option<&LocalEndpointContext>` and replace the `*_with_context` functions; `None` resolves the process configuration as before. Generated Rust modules use the new paths, so regenerate them.

### Validation

Local macOS validation passed on the integrated candidate, with C-Two and FastDB 0.2.4 checked out side by side, and the full fork CI run [41173746331](https://github.com/Dsssyc/c-two/actions/runs/41173746331) on `f0f9efaa` passed on Linux, macOS, Windows x64 and Windows ARM64.

## Defect fixes and dead-code removal (breaking)

These come from the [over-defense review](../design/2026-10-31-over-defense-review.md) and its [batch plan](../design/2026-10-31-defect-and-dead-code-batches.md).

### Fixed

- **Large chunked payloads.** Payloads larger than 64 MiB no longer fail when they travel as chunks, for example when shared memory is unavailable. A request is limited only by `max_payload_size` and `max_reassembly_bytes`; an oversized request reports a size error, not a protocol violation.
- **Malformed input is an error.**
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
| Rust: `ContractLimits`, `ContractLimitsProfile`, the `*_with_limits` functions, and the `profile` field of `ContractError::LimitExceeded` and of Python `CodegenError` | contract limits are fixed constants |
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

Before this release, the README files, reference pages, examples and Python type declarations were checked statically against the code and corrected; the examples were compiled, not run.

## Single-owner shared-memory release (breaking)

Each shared-memory block now has one owner ([ADR 0001](../decisions/0001-single-owner-release.md), [design](../design/2026-11-01-protocol-batch-design.md)). The owner lends the block; the receiver copies it into a local buffer and returns the loan in a release frame right after the copy. Requests are released before the resource method runs and responses before decoding or delivering a hold, so a slow resource method or a long `cc.hold` no longer keeps the peer's shared memory.

### Upgrade together

IPC handshake 15, SHM 4, C/FFI ABI 7 and relay mesh protocol 7 replace 14, 3, 6 and 6. A 0.10.x peer receives a typed version mismatch through the unchanged fixed preamble. Upgrade the Python and Rust SDKs, c3 and Node consumers on one host together, and stop old services before restarting.

### Behavior

- `cc.hold` and borrowed inputs keep local copies. `HeldResult` usage is unchanged: `release()` invalidates the payload owner once and frees the copy, and it no longer fails or retries because of transport memory.
- A peer that has not copied a block, for example a stopped process or one that does not read its connection, keeps that loan until it releases it or its connection ends (read EOF, or a read-side ECONNRESET on Unix). Nothing reclaims a loan after a wait or timeout.
- A low-level Host shutdown no longer seals the Runtime's call domain or outgoing clients; only an explicit `Runtime.shutdown()` / `cc.shutdown()` retires them. A low-level shutdown with no Host completes immediately.
- Request IDs are 64-bit end to end instead of being truncated to 32 bits inside the client, and generated TypeScript carries them and loan IDs as `bigint`.
- `max_frame_size` must be at least 526 bytes, the size of a full release frame.
- A peer that stops while it still holds loans keeps shutdown at `completed=False`, returned when the shutdown budget ends, until the peer releases them or its connection ends. 0.10.1 aborted such a connection at once, because the shared allocator let the server free the blocks itself. A server also closes its listener only after accepted calls and their replies drain, so a client that stops reading keeps the address bound until it reads again or its connection ends.
- A waiting budget, for a call, a connection, `Runtime.shutdown()` / `cc.shutdown()` or `cc.serve(shutdown_timeout=...)`, is accepted or rejected the same way on every platform: it must be finite, non-negative and within the range every supported monotonic clock can represent. Windows previously accepted values such as `1e19` seconds that Linux and macOS rejected, and an oversized low-level Host shutdown budget now returns a configuration error instead of panicking.
- A completed close or `Runtime.shutdown()` / `cc.shutdown()` leaves no idle shared-memory backing mapped or charged: a deactivated client drops its cached request and reassembly pools, and a server releases its idle backing before it reports Stopped or Failed. Blocks still on loan keep their backing until their last release, and a restarted server prewarms again.
- Deactivating a connection, after a release-channel violation, local bookkeeping exhaustion or a write timeout, stops new calls on it; calls already sent still receive their replies. Only a broken frame boundary fails the other calls in flight. A well-framed bad reply or chunk, or a local failure such as opening the peer's mapping, fails only its own call, as in 0.10.1.
- C-Two never abandons a dispatched call on its own. Replacing, discarding or evicting a cached client, in the SDK's IPC pool or in the relay, a relay shutdown, and `Runtime.shutdown()` / `cc.shutdown()` stop new calls but let calls already sent finish and deliver their results; the connection closes after them. 0.10.1 failed calls still in flight at shutdown as dispatch-uncertain. A caller stops waiting only through its own call deadline or a genuine failure (peer end, broken framing). Generated TypeScript `close()` likewise lets calls already sent, including chunked uploads, finish.
- A server that receives a well-framed request with a malformed descriptor or call control replies with an error for that call and keeps reading the connection; 0.10.1 skipped such a frame without replying. A call that reaches a server after its connection stopped admission receives `ResourceClosed` stating it was not executed. When a resource method has run but its reply cannot be allocated, the caller receives `ResourceOutputSerializing`.
- A server whose client reads slowly makes replies wait for writer queue space instead of ending the connection. Replies sent from the receive loop count toward `max_pending_requests`, and a call keeps its pending slot until its reply is written; when that budget is exhausted the server ends the connection with reason `local_close`.
- Small calls are slower, because every connection now writes through its own writer task. In paired CI runs against the code before this change (five alternating pairs on each platform), a 1-byte call's median latency is 1.14–1.21 times as long on Linux and Windows, about 40–70 µs more per call, and its throughput is 8–14% lower. On Linux and Windows, 64 KiB calls through the buddy pool take 0.92–1.07 times as long, and with the buddy pool disabled they are 10–24% slower; 4 MiB calls are 3–12% faster. macOS runners vary too much between pairs for a ratio (1-byte pairs range from 0.94 to 1.95 in one run), but a 1-byte call there also takes about 50–57 µs more. Prefer batch reads, or computing inside a resource method, over per-element calls; [the write fast-path experiment](../research/2026-11-02-ipc-write-fast-path-experiment.md) records why the gap was not closed in the write path.

### Added

- `ipc_overrides` keys `write_timeout`, `max_dedicated_segments` and `spill_threshold`, with `C2_IPC_WRITE_TIMEOUT`, `C2_IPC_MAX_DEDICATED_SEGMENTS` and `C2_IPC_SPILL_THRESHOLD` ([configuration](../reference/configuration.md)).
- `cc.connect(timeout=...)` also bounds the OS connection, the namespace gate or listener-lease wait and the handshake, which 0.10.1 limited to fixed five-second guards; without a timeout those guards remain. Generated TypeScript accepts `connectTimeoutMs` for the same attempt, and the ABI 7 connect entries take a nullable `timeout_millis` ([C ABI](../reference/c-abi.md)).
- `cc.serve(shutdown_timeout=30.0)` validates the budget on entry, calls `shutdown` once with it after a signal and returns the outcome; 0.10.1 looped until shutdown completed. A pending outcome prints a line saying cleanup continues.

### Removed

| Removed | Instead |
| --- | --- |
| The automatic IPC heartbeat: `heartbeat_interval`, `heartbeat_timeout`, `C2_IPC_HEARTBEAT_INTERVAL`, `C2_IPC_HEARTBEAT_TIMEOUT`, and the `heartbeat_timeout` end reason | Connections end on OS facts and calls on their own deadlines; manual ping remains |
| `abandoned_releases` in every `memory_stats()` scope and `holds.by_storage` | `holds` counts local copies with `active_holds`, `total_held_bytes`, `oldest_hold_seconds` and `by_direction` |
| The warning logged for a resource input retained over 60 seconds, `NativeServerBridge(hold_warn_seconds=...)`, `LocalCopyTracker.sweep_retained` and `RuntimeSession.sweep_local_copies` | `cc.observe.memory_stats()['holds']['oldest_hold_seconds']` |
| Python `MemPool.data_addr`, `seg_data_info`, `open_peer`, `ensure_peer_segment`, `read_at`, `free_at`, `destroy`, and `PoolConfig(dedicated_crash_timeout_secs=...)` | The local allocation APIs; a pool is freed with its last owner |
| Lifecycle snapshot `policy`, `phase`, `terminal`, `watcher_error` and `client_held_leases` (Python `ClientHeldLeases`; Rust `HostLifecyclePhase`, `HostClientHeldLeases`) | `finished`, `listener_closed` and `work_drained` |
| The dictionary returned by direct IPC `shutdown()` (`acknowledged`, `shutdown_started`, `server_stopped`, route outcomes); Rust `DirectIpcShutdownRouteOutcome` | `'initiated'`, `'absent'` or `'unconfirmed'`; Rust `DirectIpcShutdownOutcome::{Initiated, Absent, Unconfirmed}` |
| Route revisions in route bindings, call tokens, relay resolution, HTTP headers and the mesh; IPC `RouteWatch` and route-contract queries | Route UID, server instance and contract identify a route |
| C/FFI ABI 6 request and response pools, including `request_pool_read_local`, `request_pool_forget_consumed` and `response_pool_release`; TypeScript `createC2MemFfiNativeResponseShmReader`, the `responseShmReader` and `requestShmWriter` options, and block `allocationId`; the TypeScript binding's request and response pool types and factories, `C2MemFfiAbiSymbols` and `C2_MEM_FFI_MAX_IPC_SHM_SEGMENTS` | One ABI 7 memory context per verified connection (`C2MemFfiMemoryContext`, with `C2MemFfiBlock`, `C2MemFfiPreparedRequest` and `C2MemFfiWrite`); blocks carry `loanId: bigint`; `C2MemFfiNodeNativeSymbols` keeps only `symbols`, and the Node runtime no longer returns `responsePoolFactory` ([C ABI](../reference/c-abi.md)) |
| Rust Core `HeldResponse` leases and pins, `has_pending_release`, `abandon`, `RequestLease` and `ResponseLease`, and `c2_ipc::ResponseLease` with `SyncClient::lease_response` | `Held` and `HeldResponse` own a local copy |

### Further changes

Found by the strict comparison against 0.10.1 and not listed above:

- Python:
  - `BaseIPCOverrides`, `ServerIPCOverrides` and `ClientIPCOverrides` live in `c_two.config`; `InputLifetimeLike`, `CoreError.body` and `Server.get_slot_info` are removed, `CoreError.lifecycle_kind` no longer lists `relay_duplicate_route`, `relay_http` or `held_response_copy`, and a proxy's tracker is `local_copy_tracker` (was `lease_tracker`).
  - `c_two.mem`: `PoolAlloc.allocation_id` and `PoolStats.dedicated_pending_free_bytes` are removed; `max_dedicated_segments` above 65536 is rejected; a dedicated segment no longer has a 64-byte header.
- Rust: `Held::release` consumes the value even when invalidation fails or panics, and a later call returns `Ok`. `LifecycleError::HeldResponseCopy` is removed and `HeldResponseRelease` carries one required `invalidation_error`. `Runtime::shutdown_without_host` is removed; `Runtime::begin_session_retirement` with `RuntimeShutdownTarget::ClientsOnly` retires a Runtime that has no Host. `c_two::observe::ConnectionClosed::is_peer_end()` reports whether the peer ended the connection. The Core crate `c2-mem` changes its low-level API: `free_at` takes the allocation level instead of the data size and allocation ID, `release_handle` panics on a handle from another pool, `BuddySegment::allocator()` panics on a peer segment, and `destroy`, `gc_dedicated`, `SegmentHeader` and `ShmSpinlock` are removed. `c2-config`'s `CallTimeoutError` is replaced by `TimeoutError`, shared by every waiting budget. Receivers read a peer's blocks through `PeerMappingCache`; `MemPool::ensure_peer_segment`, `ensure_peer_dedicated`, `open_dedicated_at`, `validate_data_at`, `copy_data_at` and `data_ptr_at` are removed, and `MemPool::open_peer` only validates a peer's pool identity.
- TypeScript: options accept an explicit `undefined` for `ipcAddress`, `serverId`, `serverInstanceId`, the two `observe` callbacks and `C2Fetch.signal`. Node applies `write_timeout`, deactivates the connection on a zero loan id as Rust does, and accepts every dedicated segment index the wire carries.
- Wire: `RouteAck` (0x14), `RouteRecord.owner_epoch` and `lease_deadline_ms`, `RouteNack.nonce`, `DigestDiffEntry.hash` and the `FLAG_SHM`, `FLAG_POOL` and `FLAG_DISK_SPILL` flags are removed. A route's state is `ready` or `closed`, its reason is one of `register_committed`, `explicit_unregister` or `shutdown` and is always present, and the route directory and the mesh reject unknown fields.
- Connections:
  - Aborting or dropping a local stream no longer forces an OS disconnect: the handle closes when its last holder releases it, so a forked child that inherited the descriptor keeps the peer from seeing the end.
  - The close notification is published when the physical stream retires, not when the end is first recorded.
  - A write timeout deactivates the connection instead of aborting the stream.
  - Calls not yet written when a connection deactivates, or before it connects, fail as pre-dispatch and are retry-safe; 0.10.1 reported them as dispatch-uncertain.
  - A chunked reply larger than `max_reassembly_bytes` fails with a capacity error, like a request. Reassembly charges also count one byte per chunk for arrival tracking.
  - An early reply to a chunked request stops the upload at a frame boundary and keeps the connection; generated TypeScript previously reopened its connection.
  - A malformed shutdown frame closes the connection instead of receiving a negative acknowledgement.
- Shutdown and errors:
  - Repeating a low-level Rust `Runtime::shutdown` returns the complete journal instead of an empty one.
  - `ipc_client_close_details` lists only connections whose first end was not a local close, with the operation as the reason.
  - A failed request materialization reports `ResourceInputDeserializing` instead of `ProtocolViolation`.

### Validation

Host checks of each block are recorded in the [protocol batch acceptance record](../acceptance/2026-11-03-protocol-batch-acceptance.md). The exact candidate is `11f892145c439b3183ed125d32c1c59eb51f5ee6`, merged as `b81405fe` with the same tree: [Source Validation 41520721125](https://github.com/world-in-progress/c-two/actions/runs/41520721125) passed on every lane (Linux x64 with Python 3.12 and 3.14t, macOS arm64, Windows x64 and ARM64, workflow policy and tests). The paired IPC benchmark ran on `d772ca3a`, whose code is identical to that candidate, in [41642698332](https://github.com/Dsssyc/c-two/actions/runs/41642698332) against `b861f994`: Linux, macOS, Windows x64 and ARM64 passed with no candidate failures, and all 22 abnormal-client scenario and control pairs passed on each platform. An earlier run on `d646fc12` ([41444159385](https://github.com/Dsssyc/c-two/actions/runs/41444159385)) gave the same picture. Three earlier exact-candidate runs failed and are fixed by this batch: `abac9f8b` ([41368084528](https://github.com/Dsssyc/c-two/actions/runs/41368084528), paired [41382919073](https://github.com/Dsssyc/c-two/actions/runs/41382919073)), `26403fe5` ([41394772446](https://github.com/Dsssyc/c-two/actions/runs/41394772446), paired [41394919731](https://github.com/Dsssyc/c-two/actions/runs/41394919731)) and `63979601`, the first PR run ([41484700985](https://github.com/world-in-progress/c-two/actions/runs/41484700985)). Later documentation-only commits are not source commits.

## Payload buffer budget and chunked-call ordering (breaking)

These changes prepare server streams ([ADR 0003](../decisions/0003-server-streams.md), [design](../design/2026-11-03-streaming-rpc-design.md)); server streams themselves are not available yet. IPC handshake 15, SHM 4, C/FFI ABI 7 and relay mesh 7 are unchanged.

### Renamed settings

| Before | Now |
| --- | --- |
| `retained_input_budget_bytes` in Python `cc.set_call_execution_limits(...)` and Rust `CallExecutionLimits`, `C2_CALL_RETAINED_INPUT_BUDGET_BYTES`, c3 `--call-retained-input-budget-bytes` | `payload_buffer_budget_bytes`, `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES`, `--call-payload-buffer-budget-bytes` |
| IPC override `max_total_chunks`, `C2_IPC_MAX_TOTAL_CHUNKS` | `max_inflight_chunk_frames`, `C2_IPC_MAX_INFLIGHT_CHUNK_FRAMES` |

There are no aliases. The old Python keyword, override key and c3 flag are rejected; the old environment variables are ignored, so rename them where they are set.

### Behavior

- One payload buffer budget per Runtime and per relay replaces the retained-input budget. It charges a call's owned input until the transport has written or copied it, the HTTP request and response bodies C-Two holds, and the relay's copies of forwarded request bodies, for calls with and without a deadline. 0.10.1 charged only the input that finite-deadline calls kept, until their reply. The relay does not yet charge the upstream responses it sends back. See the [memory policy](../reference/memory-policy.md#payload-buffer-budget).
- When the budget is short, a call waits for capacity within its own deadline instead of failing, and a call without a deadline can wait without bound. A request larger than the whole budget, and unknown-length data that grows past its first reservation, fail at once with `CallCapacityExceeded` and `stage=payload_buffer_admission`. A hold of an HTTP response keeps its reservation until it is released.
- A call's input is released as soon as the transport has written or copied it, not when the reply arrives.
- `cc.observe.memory_stats()` adds `payload_buffer`, the budget's limit, use, peak and rejections, and `retired_payload_buffers` for earlier sessions. A wait is not counted as a rejection.
- A relay-aware call sends its business POST once. When the relay reports that the route is stale, the call fails with that pre-dispatch `RouteStale` error and the next call resolves the route again; 0.10.1 re-sent the call once from a retained copy of its input. Generated TypeScript clients still re-send once; they follow in a later change.
- A chunked IPC call reserves its route at its first chunk in the connection's receive loop, in frame order, as an unchunked call does, and takes the server-wide pending permit there. Under `exclusive` and `read_parallel`, a later call on the same connection can no longer run before an earlier chunked write.
- Registering or unregistering a route no longer makes a connection's receive loop wait, and reassembly copies no longer hold the pool or registry locks, so a large copy does not delay other calls or the expiry sweep.
- Contract v2 accepts `"mode": "server_stream"` on a method that returns a payload. The mode enters the ABI and signature fingerprints only for such methods, so unary contracts keep their fingerprints. Code generation rejects server-stream methods until the SDKs support them.
- A server prewarms its response pool once when it starts, no longer also when it is constructed.

### Validation

The exact candidate `787d4205b5e9a23505c22b8155cf18a5a92dcad2` passed [Source Validation 41857452303](https://github.com/Dsssyc/c-two/actions/runs/41857452303) on every lane: Linux x64 with Python 3.12 and 3.14t, macOS arm64, and Windows x64 and ARM64. The two full runs before it exposed three races in tests, which were fixed in the tests only. Each block was checked on macOS before it was merged, and the findings of independent reviews of the reassembly, budget and chunked-call changes were reproduced and fixed first; the [phase 1 acceptance record](../acceptance/2026-11-04-streaming-phase-1-acceptance.md) has the details.

## IPC 16 call records (breaking)

Calls now travel as IPC 16 call records; the [IPC handshake and call records](../reference/ipc-handshake.md#call-records-version-16) page gives the layout, and section 7 of the [streaming design](../design/2026-11-03-streaming-rpc-design.md) the reasons. SHM 4, C/FFI ABI 7 and relay mesh 7 are unchanged. The transport also carries server streams ([below](#server-streams-in-the-ipc-transport)), but the SDKs cannot use them yet.

### Upgrade together

IPC 16 replaces 15. A version 15 peer and a version 16 endpoint receive a typed version mismatch from each other through the unchanged fixed preamble. Upgrade the Python and Rust SDKs, c3 and generated TypeScript clients on one host together; generated TypeScript clients embed the transport and must be regenerated.

### Behavior

- A call that the server refused before it entered the resource now ends with the status NotExecuted and is reported as pre-dispatch with its canonical code and details. This covers a closed, removed or stale route, a contract or identity mismatch, server capacity and chunked input that timed out. IPC 15 reported these refusals as resource errors, which a relay turned into HTTP 500. Rust reports `IpcError::CallRejected`; Core and Python errors carry `transport_phase=pre_dispatch`; a relay answers with an HTTP status that follows the code and adds `dispatch_phase=pre_dispatch`; generated TypeScript raises `C2IpcCallRejectedError` with `retrySafe` true. C-Two still does not retry or replay the call itself.
- A resource error no longer carries the phase of a call the resource made: the client removes `transport_phase` and `dispatch_phase` from it, so a Rust resource that returns a nested call's refusal is not reported as not executed.
- The relay withdraws a route when the upstream resource reports, before execution, that the route is missing, removed or closed or that the contract or identity does not match, as it already did for the same facts at route acquisition. A stale route token withdraws nothing.
- Each connection's writer assigns a call's request ID when it first writes the call, so calls reach the server in request-ID order.
- Late chunks of a refused or finished request are discarded by the connection's request-ID high-water mark and never draw another reply, however late they arrive; IPC 15 kept a marker per request and answered a chunk delayed past `chunk_assembler_timeout` with a second error. An incomplete chunked input is refused when the GC sweep finds it timed out, without waiting for another chunk.
- A client keeps a sent call's record after the caller's deadline until its reply arrives, so a late reply and its SHM loan are settled on the original connection.
- If a server cannot finish a chunked reply after its first chunk, it deactivates the connection without a second reply; the other calls on that connection finish, and the client reports that call as dispatch-uncertain when the connection ends.
- A small unary call uses one frame in each direction.

### Removed

| Removed | Instead |
| --- | --- |
| IPC 15 business frames: call and reply controls, `FLAG_CALL_V2`, `FLAG_REPLY_V2`, `FLAG_CHUNK_LAST`, indexed chunk headers and the CTRL release frame | IPC 16 call records: OPEN, CALL_END, RELEASE and the server-stream records |
| Rust Core `c2_wire::control` (`CallControl`, `ReplyControl` and their codecs) and `c2_wire::chunk` headers | `c2_wire::rpc`, which also owns `RouteCallIdentity` and `EncodeError` |
| Settled-request markers in the chunk registry | The request-ID high-water mark |

### Validation

On macOS with the FastDB sibling, the Rust Core workspace (1732 tests), c3 (81), the Rust SDK (27) and the CI Python gates, including the TypeScript and portable phases and the direct/relay matrix, passed on the integrated tree. An independent review's four findings were reproduced and fixed first. The exact candidate `b5b4853cc52a2747380db9824b500903655cf8db` (the IPC 16 code of `ea20ea0c` plus its documentation) passed [Source Validation 41911808218](https://github.com/Dsssyc/c-two/actions/runs/41911808218) on every lane on the first attempt: Linux x64 with Python 3.12 and 3.14t, macOS arm64, and Windows x64 and ARM64.

## Server streams in the IPC transport

The IPC transport now carries server streams. The server sends each item as ITEM_BEGIN followed by ITEM_DATA, and the client grants storage and demand with CREDIT and stops a stream with CANCEL; the [server streams](../reference/ipc-handshake.md#server-streams) section gives the rules. The SDKs cannot declare or call server streams yet, and generated TypeScript, Node and the relay do not carry them. IPC 16, SHM 4, C/FFI ABI 7 and relay mesh 7 are unchanged.

### Behavior

- The canonical error registry adds `CallCancelled` (718), with the Python class `CallCancelled` and `ERROR_Code.ERROR_CALL_CANCELLED`. It reports a server stream whose cancellation has finished.
- A frame write that exceeds `write_timeout` still deactivates the connection, but a call record that the writer has already accepted keeps writing, and the call's later chunks and its reply follow that frame's real result. A chunked unary reply is therefore no longer cut off by a write that was slow but finished.
- Rust Core names chunked values with chunk instead of fragment: `FLAG_CHUNKED`, `FLAG_FIRST_CHUNK`, `ChunkPosition` and the `ValuePart::chunk` field replace `FLAG_FRAGMENTED`, `FLAG_FIRST_FRAGMENT`, `FragmentPosition` and `ValuePart::fragment`. The wire bits are unchanged.

### Validation

On macOS with the FastDB sibling, the Rust Core workspace (1791 tests), c3 and the Rust SDK (108 together) and the CI Python gates, including the TypeScript and portable phases, passed on the integrated tree. The three findings of an independent review were confirmed in the code and fixed first: a call record whose write timed out could leave a stream without CALL_END, a receiver could report the end of a stream ahead of an item already queued, and a chunked stream OPEN ignored an early CALL_END. The exact candidate `d6c808f996a51f7f43436395ad5fda7b6c0a0be1` (the code of `02b4c008` plus its documentation) passed [Source Validation 41936786846](https://github.com/Dsssyc/c-two/actions/runs/41936786846) on every lane after a rerun of the failed jobs: Linux x64 with Python 3.12 and 3.14t, macOS arm64, and Windows x64 and ARM64. The first attempt failed on Windows x64 in one c2-server shutdown test, whose reply read timed out; the rerun passed, the test was not reproduced on macOS, and its cause is still open in the backlog.

## Server streams in Rust

Rust resources and Rust clients can now use server streams over direct IPC ([ADR 0003](../decisions/0003-server-streams.md), [design](../design/2026-11-03-streaming-rpc-design.md)). The Python SDK, generated TypeScript and relay forwarding do not support them yet. IPC 16, SHM 4, C/FFI ABI 7 and relay mesh 7 are unchanged apart from the narrower SHM rule below.

### Behavior

- Code generation supports server-stream methods for the Rust target. The generated client method returns `c_two::ResponseStream<fastdb::Payload>`, an iterator of `Result` items, and its `hold_` variant returns `ResponseStream<c_two::Held<fastdb::Payload>>`. The generated `Service` method returns an owned `Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>`; each `next()` is one resource step under the route permit. Unary contracts generate the same code as before. See the [Rust SDK](../../sdk/rust/README.md#server-streams).
- Dropping a `ResponseStream` submits cancellation without waiting. `close()` waits up to 30 seconds for the original call to finish and `close_with_timeout(None)` without bound; the receipt reports whether the cleanup finished, the real outcome and the count of complete items. A call deadline covers the whole iteration and ends only the caller's wait.
- Runtime shutdown and route unregistration end the open streams of their routes between steps: the running step finishes, its item is still sent, the cleanup runs, and the stream ends with `CallCancelled` naming the cause. Shutdown or unregistration completes only after that cleanup and stays incomplete while a step, the cleanup or an item's delivery is stuck. Unary calls are unaffected.
- Every server stream holds a `max_outstanding_calls` slot until its real terminal, also without a deadline; unary calls without a deadline are still not counted. A finite unary call now releases its slot before its result reaches the caller, so a caller that sends the next call at once is no longer refused while the limit is full.
- The server no longer charges stream items to the payload buffer budget: an SHM item is written straight into its shared-memory allocation and an inline item is copied one frame at a time, so a Runtime that streams to itself over IPC cannot wait on its own reservation. The client still reserves each item in full before granting storage for it.
- An SHM ITEM_DATA must now carry the whole item; a partial SHM part, or an item that mixes SHM and inline parts, is a protocol error. The client copies a whole SHM item outside its receive loop, so other calls on the connection keep moving during the copy.
- A relay's HTTP forwarding refuses a stream method before dispatch with `UnsupportedCallMode`; the unary methods of the same contract still go through the relay.

### Validation

On macOS with the FastDB sibling, the Rust Core workspace (1827 tests, including the unary code-generation baseline), c3 and the Rust SDK (109 together) and the CI Python gates, including the TypeScript and portable phases, passed on the integrated tree. An independent review found two defects that the Host reproduced before they were fixed: a Runtime streaming to itself over IPC could wait forever on its own payload buffer budget, and shutdown or unregistration during a final step that ended the stream was reported as a natural end. The Host also found that the client copied a whole SHM item in its receive loop under the stream's lock; that copy now runs outside both. The exact candidate `ae4b0647a0e973048f68e4bb8ad0923e204bb043` (the code of `fd913fad` plus its documentation) passed [Source Validation 41962137139](https://github.com/Dsssyc/c-two/actions/runs/41962137139) on every lane on the first attempt: Linux x64 with Python 3.12 and 3.14t, macOS arm64, and Windows x64 and ARM64.
