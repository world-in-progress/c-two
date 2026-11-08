# C-Two Rust SDK

`sdk/rust` is the user-facing Rust projection of C-Two:

```text
Cargo package: c-two
Rust import:   c_two
version:       0.1.0
publish:       false
```

This source-only package has no crates.io release. It reuses `c2-contract` for release identity and `c2-core` for route, transport, retry, host, and lifetime behavior. It does not expose a second runtime state machine.

This guide describes the current source-only SDK, whose independent version remains 0.1.0. It requires Rust 1.95 or newer and uses IPC 16, SHM 4 and C/FFI ABI 8 with the Python SDK and c3 built from the same source. It cannot interoperate with published 0.10.x builds; see the [unreleased changes](../../docs/releases/unreleased.md). Rust `c2-core` owns shutdown drain transactions and local endpoint maintenance. SDKs use the one OS endpoint derived by `LocalEndpoint::from_address`; there is no backend selector. See [lifecycle integration](../../docs/reference/local-endpoint-lifecycle.md). Use matching builds and local endpoint configuration for clients, hosts and relays.

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

Timeouts above use `Option<Duration>`. The [lifecycle guide](../../docs/reference/local-endpoint-lifecycle.md) specifies endpoint credentials, connection end reasons and observation ordering. The [0.8.0 Rust migration notes](../../docs/releases/0.8.0.md#rust-migration-and-transport-behavior) cover removed low-level APIs, move-only allocation owners, `ChunkRegistry<C>`, SHM response preparation and structured lifecycle failures. Current source uses [IPC 16](../../docs/reference/ipc-handshake.md), SHM 4 and C/FFI ABI 7. Recompile and upgrade peers together: 0.10.x peers and IPC 15 builds receive a structured version mismatch through the unchanged fixed preamble; 0.9.x and earlier peers yield generic connection or protocol failures. See [Upgrade together](../../docs/releases/unreleased.md#upgrade-together).

The low-level readiness failure now exposes `ServerFailure.bind_cause: Option<EndpointBindCause>`; migrate field access and exhaustive matches to the new binding cause without an alias. Unix credentials use schema 4, with inode/ctime/incarnation and no persisted device. Namespace disk format 3 also versions the default directories; custom roots gain no suffix and old Unix credentials are rejected. Windows credentials retain schema 1.

## Portable payloads

Portable payloads keep their owner-qualified name:

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

C-Two-generated Rust modules provide typed clients and service definitions. They consume the hidden `c_two::generated` seam; ordinary applications should not construct encoded calls or service dispatch tables by hand.

All portable receive paths are currently copy-backed. `Held<Payload>` adds a checked lifetime: explicit `release()` and `Drop` invalidate the payload owner before dropping the local copy. Generated service inputs use the same ordering at callback exit. This is not a zero-copy claim, and unsafe raw pointers extracted outside FastDB's checked owner/view model cannot be revoked.

`Held` owns a local copy: the transport memory was released right after the copy, so a hold keeps no connection or peer mapping. `Held::release()` runs the invalidator once, including after a caught panic, then drops the local bytes; later releases are no-ops. `value()` becomes `None` when invalidation starts, and `is_released()` reports that state.

High-level `Held` keeps only `release()` and `Drop`. See [memory policy](../../docs/reference/memory-policy.md#statistics-and-payload-lifetime) for loans, release records and the hold statistics.

## Server streams

A contract method with `"mode": "server_stream"` returns zero or more items. For such a method the generated client's `method_{index}_{name}` returns `Result<c_two::ResponseStream<fastdb::Payload>, c_two::Error>`, and `hold_method_{index}_{name}` returns a `ResponseStream<c_two::Held<fastdb::Payload>>` whose items each keep their own local copy until released. `ResponseStream<T>` is an `Iterator<Item = Result<T, c_two::Error>>`: it ends with `None` after a natural end and with one error otherwise.

The generated `Service` trait method returns `Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>`. The iterator must own its state: it cannot borrow the method's input or `&self`, so copy what it needs into owned domain values or capture an `Arc`. Each `next()` runs as one resource step under the route permit, and the iterator is dropped in the stream's cleanup step.

Dropping a `ResponseStream` submits cancellation without waiting. `close()` submits cancellation and waits up to 30 seconds for the original call to finish; `close_with_timeout(None)` waits without bound and `close_with_timeout(Some(Duration::ZERO))` observes once. The returned `StreamCloseReceipt` reports `completed`, the real outcome as `terminal` and `complete_items`; when `completed` is false, the cleanup is still running and a later `close` can observe it again. A call deadline from the client's call options covers the whole iteration and is not reset for each item; its expiry ends only the caller's wait. Every stream holds one `max_outstanding_calls` slot until its real terminal. Runtime shutdown and route unregistration on the resource side, and the shutdown of a relay forwarding the stream, end an open stream between its steps: the client receives the items already produced and then `CallCancelled` naming the cause, while unary calls already sent still finish normally. Rust streams use real IPC, also within one process, and stream through a relay over HTTP with the same handle; there a resource can run ahead of a consumer that stops reading by as many items as the HTTP and TCP buffers hold, which for small items can be thousands. [Runtime lifecycle](../../docs/reference/runtime-lifecycle.md#execution-admission) describes steps, cancellation and shutdown, and [server streams](../../docs/reference/ipc-handshake.md#server-streams) the wire rules.

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC call. Their generated client and typed service modules use the SDK generated seam.
