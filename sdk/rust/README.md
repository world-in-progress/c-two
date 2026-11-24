# C-Two Rust SDK

`sdk/rust` is the user-facing Rust projection of C-Two:

```text
Cargo package: c-two
Rust import:   c_two
version:       0.1.0
publish:       false
```

This source-only package has no crates.io release. It reuses `c2-contract` for release identity and `c2-core` for route, transport, retry, host, and lifetime behavior. It does not expose a second runtime state machine.

This guide describes the current source-only SDK, whose independent version remains 0.1.0. It requires Rust 1.95 or newer and uses IPC 18, SHM 4, C/FFI ABI 10, relay mesh 8 and FastDB 0.3.0. Use matching builds and local endpoint configuration for clients, Hosts and relays. Rust `c2-core` owns shutdown drain transactions and local endpoint maintenance. SDKs derive the OS endpoint from `LocalEndpoint::from_address`. See [lifecycle integration](../../docs/reference/local-endpoint-lifecycle.md).

A Runtime resolves its client and server role domains independently. Use `RuntimeOptions::client_limits` for client capacity and `RuntimeOptions` for the local endpoint and discovery, and `Runtime::update_client_config` / `Runtime::update_server_config` with `ConfigField::{Keep, Inherit, Set}` to adjust overrides before a role freezes. Put the Host's identity, startup timeout, carrying limits and server capacity on `HostOptions`. Each role freezes on first use and accepts subsequent changes with the same effective settings. [Configuration](../../docs/reference/configuration.md#role-domains-and-sdk-configuration)

```rust
use c_two::{HostOptions, Runtime, RuntimeOptions};

let mut runtime_options = RuntimeOptions::default();
runtime_options.client_limits.memory_budget_bytes = Some(128 * 1024 * 1024);
runtime_options.client_limits.max_outstanding_calls = Some(128);
let runtime = Runtime::new(runtime_options)?;

let host_options = HostOptions::default()
    .with_server_id("grid-host")
    .with_max_payload_size(64 * 1024 * 1024)
    .with_memory_budget_bytes(256 * 1024 * 1024)
    .with_file_spill_budget_bytes(512 * 1024 * 1024)
    .with_max_outstanding_calls(256);
let host = runtime.host(host_options)?;
```

Idle buddy mappings are reclaimed automatically. Capacity is charged to the role that owns the storage and released with its owner. [Memory policy](../../docs/reference/memory-policy.md)

Connection acquisition accepts an optional total budget through `Runtime::connect_with_options(expected, mode, ConnectOptions::new().with_timeout(Duration::from_millis(100)))`. Omitted options add no caller deadline. The budget spans pool waits, connection, handshake, live route lookup and relay discovery. Expiration returns `CallDeadlineExceeded` with `operation=connect` and `transport_phase=pre_dispatch`. Business-call options are independent: use `Client::with_call_options` with `CallOptions` and `CallTimeout::{Inherit, Unlimited, After}`; see [connection configuration](../../docs/reference/configuration.md#connection-deadlines).

## Lifecycle and diagnostic APIs

The crate root contains the Runtime, Host, client, registration and call types (including `CallOptions`, `CallMetadata`, `CallMetadataError`, `AdmissionHook`, `AdmissionContext`, `AdmissionDecision`, `AdmissionRejection`, `AdmissionOperation`, `AdmissionTransport`, `CallContext` and `current_call_context`), their configuration and errors, `ContractRelease`, `Held`, `ResponseStream`, `StreamCloseReceipt`, and the topic types `Publisher`, `Subscription`, `Delivery`, `TopicStart`, `LagPolicy`, `TopicConfig`, `SubscribeOptions`, `TopicCloseCause` and `TopicCloseReceipt`. Supervisor, tooling and diagnostic APIs have one public path in these modules:

| Module | Audience and contents |
| --- | --- |
| `c_two::endpoint` | Supervisors and maintenance tools: endpoint contexts, credentials, inspection, reaping, sweeps, direct IPC ping/shutdown and local I/O causes. |
| `c_two::observe` | Monitoring and diagnostics: connection events, call admission and concurrency snapshots, memory statistics, host lifecycle, paths and routes. |
| `c_two::contract` | Contract tooling: validation errors, release references and expected route contracts (for example `c_two::contract::ExpectedRouteContract`, `ContractReleaseRef` and `ContractError`). |

`c_two::endpoint::{direct_ipc_endpoint, ping_direct_ipc, shutdown_direct_ipc}` take the logical address followed by `Option<&LocalEndpointContext>`; ping and shutdown also take a `Duration`. Pass `Some(&context)` to use a captured local domain. `None` resolves the existing process configuration once, using platform defaults when no override is present.

| Entry point | Contract |
| --- | --- |
| `Client::connection_observation()` / generated client method | Returns the current physical IPC observation. `snapshot()` reads its terminal record; async `wait_closed(Option<Duration>)` waits without retaining the connection. HTTP clients reject this operation. After the client follows its route to a new connection, an observation taken earlier still reports the old connection's end. |
| `Client::observed_path()` | Reports the path of the client's current target. A client follows its route name: when its target is known to be gone, the next call acquires the route again before sending ([route bindings](../../docs/reference/contracts-and-routing.md#route-bindings-lookup-and-fallback)), which may change this path; views made with `with_call_options` share the target. |
| `Runtime::memory_stats()` | Read-only `client`, `server`, `retired` and `holds` statistics, including the associated Host's memory and file spill cells. |
| `Runtime::call_execution_snapshot()` | Read-only `client`, `server` and `retired` call slot statistics. Observing either snapshot leaves inactive roles unfrozen. |
| `Runtime::connection_events()` | Subscribes to future outgoing and accepted IPC connections without starting a host. Values expose `ConnectionSide`; async `recv(&self)` returns `Ok(None)` at end. `close()` wakes this subscriber only; lag or concurrent consumers return `ConnectionEventsError`. |
| `Host::endpoint_credential()` | Returns `Option<EndpointCredential>` from the existing lifecycle watch. Ready/Stopping retain the listener snapshot; terminal states clear it. No inspect, filesystem I/O or gate lock; a snapshot proves neither liveness nor cleanup authority. |
| `Error::transport_phase()` | Returns `Option<TransportPhase>` (`PreDispatch` or `DispatchUncertain`) established by Core across transport, admission, protocol-version and normalized semantic errors; resource failures return `None`. Root re-exports `ErrorCode`, `ErrorSide`, `ErrorStage`, `C2Error` and `TransportPhase`. |
| `Error::local_io_cause()` | Returns `Option<LocalIoCause>` for bind/connect failures. `Bind(EndpointBindCause { error, unverified_reason })` preserves OS details or the typed binding refusal; `Connect(EndpointIoError)` keeps its OS cause. `error()` still returns the unchanged `EndpointIoError`. |
| `Error::ProtocolVersionMismatch { local, peer }` | A complete fixed RPC preamble reported a different nonzero `u32` version. Terminal before metadata or dispatch; no retry or relay fallback. |

Timeouts above use `Option<Duration>`. The [lifecycle guide](../../docs/reference/local-endpoint-lifecycle.md) specifies endpoint credentials, connection end reasons and observation ordering. The [0.8.0 Rust migration notes](../../docs/releases/0.8.0.md#rust-migration-and-transport-behavior) cover removed low-level APIs, move-only allocation owners, `ChunkRegistry<C>`, SHM response preparation and structured lifecycle failures.

The low-level readiness failure now exposes `ServerFailure.bind_cause: Option<EndpointBindCause>`; migrate field access and exhaustive matches to the new binding cause without an alias. Unix credentials use schema 4, with inode/ctime/incarnation and no persisted device. Namespace disk format 3 also versions the default directories; custom roots gain no suffix and old Unix credentials are rejected. Windows credentials retain schema 1.

## Portable payloads

Portable payloads keep their owner-qualified name:

```rust
use c_two::{Connect, ContractRelease, Runtime};
use fastdb::Payload;
```

C-Two-generated Rust modules provide typed clients and service definitions. They consume the hidden `c_two::generated` seam; ordinary applications should not construct encoded calls or service dispatch tables by hand.

Rust opens portable payloads directly over the local bytes after Core releases the transport storage. Generated service inputs and ordinary unary outputs move their local vectors into FastDB. Ordinary stream and topic deliveries copy their bytes into an independent vector and release the Core delivery budget at decode completion. `Held<Payload>` keeps the original local response as its FastDB backing and invalidates the payload owner on `release()` or `Drop`. Generated service inputs invalidate their owner when the callback or stream cleanup ends.

Core releases transport memory immediately after taking the local copy, and `Held` owns only that copy. `Held::release()` runs the invalidator once, including after a caught panic. FastDB waits for active `Access` pins to drain during invalidation. Drop any pins held on the same thread before releasing the hold. `value()` becomes `None` when invalidation starts, and `is_released()` reports that state. Later releases are no-ops.

High-level `Held` uses `release()` and `Drop` as its release entry points alongside `value()` and `is_released()`. See [memory policy](../../docs/reference/memory-policy.md#statistics-and-payload-lifetime) for loans, release records and the hold statistics.

## Keyed unary calls

Pass a stable call ID to `method_{index}_{name}_with_call_id` or `hold_method_{index}_{name}_with_call_id` once per logical unary operation. Set the timeout and metadata on the client view with `with_call_options`. An uncertain dispatch can carry its key in `Error::call_id()`. Use `query_call(id)` to observe the current registration without taking a business call slot, or `recover_method_{index}_{name}(id)` to await or replay its original result without an input payload. For a held result use `recover_hold_method_{index}_{name}`. Each replay has an independent local lifetime.

`HostOptions::with_outcome_max_entries` and `with_outcome_max_retained_bytes` bound retention across the Host's registrations. Recovery can report `CallResultUnavailable` for a successful call whose payload is missing, or `CallOutcomeUnknown` if the current registration has no record. The caller can reconcile unknown outcomes against its own durable state. See [ADR 0006](../../docs/decisions/0006-post-dispatch-outcome-and-dedup.md).

Set metadata with `CallOptions::with_metadata` or `with_metadata_entry`. Configure a Host's admission hook through `HostOptions::with_admission_hook`. A refusal surfaces as `ErrorCode::AdmissionDenied` before dispatch. Inside an executing unary or stream callback, `current_call_context()` exposes the request metadata and an optional typed extension provided by the hook.

## Server streams

A contract method with `"mode": "server_stream"` returns zero or more items. For such a method the generated client's `method_{index}_{name}` returns `Result<c_two::ResponseStream<fastdb::Payload>, c_two::Error>`, and `hold_method_{index}_{name}` returns `Result<c_two::ResponseStream<c_two::Held<fastdb::Payload>>, c_two::Error>` whose items each keep their own local copy until released. `ResponseStream<T>` is an `Iterator<Item = Result<T, c_two::Error>>`: it ends with `None` after a natural end and with one error otherwise.

The generated `Service` trait method returns `Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>`. The iterator must own its state: it cannot borrow the method's input or `&self`, so copy what it needs into owned domain values or capture an `Arc`. Each `next()` runs as one resource step under the route permit, and the iterator is dropped in the stream's cleanup step.

Dropping a `ResponseStream` submits cancellation without waiting. `close()` submits cancellation and waits up to 30 seconds for the original call to finish; `close_with_timeout(None)` waits without bound and `close_with_timeout(Some(Duration::ZERO))` observes once. The returned `StreamCloseReceipt` reports `completed`, the real outcome as `terminal` and `complete_items`; when `completed` is false, either cleanup is still running or the wait budget expired before acquiring the stream's internal lock (in which case both `terminal` and `complete_items` are `None`), and a later `close` can observe it again. A call deadline from the client's call options covers the whole iteration and is not reset for each item; its expiry ends only the caller's wait. Every stream holds one `max_outstanding_calls` slot until its real terminal. Runtime shutdown and route unregistration on the resource side, and the shutdown of a relay forwarding the stream, end an open stream between its steps: the client receives the items already produced and then `CallCancelled` naming the cause, while unary calls already sent still finish normally. Rust streams use real IPC, also within one process, and stream through a relay over HTTP with the same handle; there a resource can run ahead of a consumer that stops reading by as many items as the HTTP and TCP buffers hold, which for small items can be thousands. [Runtime lifecycle](../../docs/reference/runtime-lifecycle.md#execution-admission) describes steps, cancellation and shutdown, and [server streams](../../docs/reference/ipc-handshake.md#server-streams) the wire rules.

## Topics

A contract topic (`"topics"` in `c-two.contract.v2`, [ADR 0005](../../docs/decisions/0005-publish-subscribe.md)) is a resource-scoped 1-to-N channel with one FastDB payload output. For a declared topic the generated module exposes a `TopicBinding` table, a typed client method `subscribe_topic_{index}_{name}(&self, options: c_two::SubscribeOptions) -> Result<c_two::Subscription<fastdb::Payload>, c_two::Error>` honoring the client's call options, a `topic_service` builder configuring each topic's bounded history ring `capacity` and atomic `initial` frames committed before the route opens, and `registration_topics(&Registration)` borrowing strongly typed `Publisher<fastdb::Payload>` handles from one committed registration. Contracts with topics only (no methods) generate an empty `Service` trait with the same registration and subscription surface.

`Publisher::publish(&self, value)` encodes the value once, moves the bytes into the sealed topic frame and returns the frame's sequence number. The handle is bound to one registration: after it ends, `publish` fails with `ResourceClosed` (707) and never writes to a later registration of the same route name. `Subscription::next_delivery()` blocks for the next `Delivery<T>` (`value`, `seq`, `dropped`); `next_delivery_held()` returns `Delivery<Held<Payload>>` with the same local backing and release semantics as unary holds, and closing the subscription never invalidates an already-delivered hold. `SubscribeOptions` selects the start point (`TopicStart::Latest` or `Next`) and the lag policy: `LagPolicy::Skip` jumps to the newest retained frame reporting the skipped count in `dropped`, while `LagPolicy::Error` terminates the subscription with `SubscriptionLagged` (720). Unregistering the route, host shutdown or relay shutdown end a live subscription with `CallCancelled` (718) naming the cause; the next `subscribe` follows the route name to the current registration. Dropping a `Subscription` submits cancellation without waiting; `close()` and `close_with_timeout(Option<Duration>)` observe the real terminal `TopicCloseReceipt`.

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC call using `c_two::generated` (`EncodedService` and method tables) directly to illustrate the low-level seam behind generated modules.
