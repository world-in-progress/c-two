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
  - Listeners dropped while another process holds the namespace gate no longer leave sockets and leases behind.
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
