# C-Two Rust SDK

`sdk/rust` is the user-facing Rust projection of C-Two:

```text
Cargo package: c-two
Rust import:   c_two
version:       0.1.0
publish:       false
```

This source-only package has no crates.io release. It reuses `c2-contract` for release identity and `c2-core` for route, transport, retry, host, and lifetime behavior. It does not expose a second runtime state machine.

This guide describes the current source-only SDK, whose independent version remains 0.1.0. It requires Rust 1.95 or newer and matches Python C-Two 0.10.x / c3 0.6.0; see the [0.10.0 release notes](../../docs/releases/0.10.0.md) for the Rust API changes in that release. Rust `c2-core` owns shutdown drain transactions and local endpoint maintenance. SDKs use the one OS endpoint derived by `LocalEndpoint::from_address`; there is no backend selector. See [lifecycle integration](../../docs/reference/local-endpoint-lifecycle.md). Use matching builds and local endpoint configuration for clients, hosts and relays.

[IPC memory policy](../../docs/reference/memory-policy.md) is also shared Core behavior: lazy buddy allocation, idle decay, finite direction-scoped backing/reassembly budgets and checked transport fallback. Endpoint closure does not release retained payload owners or their budgets.

Connection acquisition accepts an optional total budget through `Runtime::connect_with_options(expected, mode, ConnectOptions::new().with_timeout(Duration::from_millis(100)))`. Omitted options add no caller deadline. The budget spans pool waits, connection, handshake, live route lookup and relay discovery. Expiration returns `CallDeadlineExceeded` with `operation=connect` and `transport_phase=pre_dispatch`. Business-call options remain independent; see [connection configuration](../../docs/reference/configuration.md#connection-deadlines).

## Lifecycle and diagnostic APIs

The crate root contains the Runtime, Host, client, registration and call types, their configuration and errors, `ContractRelease`, and `Held`. Supervisor, tooling and diagnostic APIs have one public path in these modules:

| Module | Audience and contents |
| --- | --- |
| `c_two::endpoint` | Supervisors and maintenance tools: endpoint contexts, credentials, inspection, reaping, sweeps, direct IPC ping/shutdown and local I/O causes. |
| `c_two::observe` | Monitoring and diagnostics: connection events, call admission and concurrency snapshots, memory statistics, host lifecycle, paths and routes. |
| `c_two::contract` | Contract tooling: validation errors, release references and expected route contracts. |

`c_two::endpoint::{direct_ipc_endpoint, ping_direct_ipc, shutdown_direct_ipc}` take the logical address followed by `Option<&LocalEndpointContext>`; ping and shutdown also take a `Duration`. Pass `Some(&context)` to use a captured local domain. `None` resolves the existing process configuration once, using platform defaults when no override is present.

| Entry point | Contract |
| --- | --- |
| `Client::connection_observation()` / generated client method | Returns the current physical IPC observation. `snapshot()` reads its terminal record; async `wait_closed(Option<Duration>)` waits without retaining the connection. Thread-local and HTTP clients reject this operation. |
| `Runtime::connection_events()` | Subscribes to future outgoing and accepted IPC connections without starting a host. Values expose `ConnectionSide`; async `recv(&self)` returns `Ok(None)` at end. `close()` wakes this subscriber only; lag or concurrent consumers return `ConnectionEventsError`. |
| `Host::endpoint_credential()` | Returns `Option<EndpointCredential>` from the existing lifecycle watch. Ready/Stopping retain the listener snapshot; terminal states clear it. No inspect, filesystem I/O or gate lock; a snapshot proves neither liveness nor cleanup authority. |
| `Error::local_io_cause()` | Returns `Option<LocalIoCause>` for bind/connect failures. `Bind(EndpointBindCause { error, unverified_reason })` preserves OS details or the typed binding refusal; `Connect(EndpointIoError)` keeps its OS cause. `error()` still returns the unchanged `EndpointIoError`. |
| `Error::ProtocolVersionMismatch { local, peer }` | A complete fixed RPC preamble reported a different nonzero `u32` version. Terminal before metadata or dispatch; no retry or relay fallback. |

Timeouts above use `Option<Duration>`. The [lifecycle guide](../../docs/reference/local-endpoint-lifecycle.md) specifies endpoint credentials, connection end reasons and observation ordering. The [0.8.0 Rust migration notes](../../docs/releases/0.8.0.md#rust-migration-and-transport-behavior) cover removed low-level APIs, move-only allocation owners, `ChunkRegistry<C>`, SHM response preparation and structured lifecycle failures. C-Two 0.10.0 uses [IPC handshake 14](../../docs/reference/ipc-handshake.md), keeping C/FFI ABI 6 and SHM 3. Recompile and upgrade peers together; pre-14 peers, including 0.9.x, yield generic connection/protocol failures.

The low-level readiness failure now exposes `ServerFailure.bind_cause: Option<EndpointBindCause>`; migrate field access and exhaustive matches to the new binding cause without an alias. Unix credentials use schema 4, with inode/ctime/incarnation and no persisted device. Namespace disk format 3 also versions the default directories; custom roots gain no suffix and old Unix credentials are rejected. Windows credentials retain schema 1.

## Portable payloads

Portable payloads keep their owner-qualified name:

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

C-Two-generated Rust modules provide typed clients and service definitions. They consume the hidden `c_two::generated` seam; ordinary applications should not construct encoded calls or service dispatch tables by hand.

All portable receive paths are currently copy-backed. `Held<Payload>` adds a checked lifetime: explicit `release()` and `Drop` invalidate the payload owner before releasing the C-Two response lease. Generated service inputs use the same ordering at callback exit. This is not a zero-copy claim, and unsafe raw pointers extracted outside FastDB's checked owner/view model cannot be revoked.

`Held` owns a local copy: the transport memory was released right after the copy, so a hold keeps no connection or peer mapping. `Held::release()` runs the invalidator once, including after a caught panic, then drops the local bytes; later releases are no-ops. `value()` becomes `None` when invalidation starts, and `is_released()` reports that state.

High-level `Held` keeps only `release()` and `Drop`. See [memory policy](../../docs/reference/memory-policy.md#statistics-and-payload-lifetime) for loans, release records and the hold statistics.

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC call. Their generated client and typed service modules use the SDK generated seam.
