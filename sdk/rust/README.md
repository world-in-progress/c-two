# C-Two Rust SDK

`sdk/rust` is the user-facing Rust projection of C-Two:

```text
Cargo package: c-two
Rust import:   c_two
version:       0.1.0
publish:       false
```

This source-only package has no crates.io release. It reuses
`c2-contract` for release identity and `c2-core` for route, transport, retry,
host, and lifetime behavior. It does not expose a second runtime state machine.

This guide describes the current source-only SDK, whose independent version remains 0.1.0. It requires Rust 1.95 or newer and matches Python C-Two 0.10.x / c3 0.6.0; see the [0.10.0 release notes](../../docs/releases/0.10.0.md) for the Rust API changes in that release. Rust `c2-core` owns default `Persistent`, explicit `OwnerBound`, shutdown drain transactions and local endpoint maintenance. SDKs use the one OS endpoint derived by `LocalEndpoint::from_address`; there is no backend selector. See [lifecycle integration](../../docs/local-endpoint-lifecycle.en.md) and the [owned-child example](examples/owned_child.rs). Use matching builds and local endpoint configuration for clients, hosts and relays.

[IPC memory policy](../../docs/memory-policy.en.md) is also shared Core behavior:
lazy buddy allocation, idle decay, finite direction-scoped backing/reassembly
budgets and checked transport fallback. Endpoint closure does not release
retained payload owners or their budgets.

Connection acquisition accepts an optional total budget through
`Runtime::connect_with_options(expected, mode, ConnectOptions::new().with_timeout(Duration::from_millis(100)))`.
Omitted options add no caller deadline. The budget spans pool waits,
connection, handshake, live route lookup and relay discovery. Expiration
returns `CallDeadlineExceeded` with `operation=connect` and
`transport_phase=pre_dispatch`. Business-call options remain independent;
see [connection configuration](../../docs/configuration.en.md#connection-deadlines).

## Lifecycle and diagnostic APIs

| Entry point | Contract |
| --- | --- |
| `Client::connection_observation()` / generated client method | Returns the current physical IPC observation. `snapshot()` reads its terminal record; async `wait_closed(Option<Duration>)` waits without retaining the connection. Thread-local and HTTP clients reject this operation. |
| `Runtime::connection_events()` | Subscribes to future outgoing and accepted IPC connections without starting a host. Values expose `ConnectionSide`; async `recv(&self)` returns `Ok(None)` at end. `close()` wakes this subscriber only; lag or concurrent consumers return `ConnectionEventsError`. |
| `Host::endpoint_credential()` | Returns `Option<EndpointCredential>` from the existing lifecycle watch. Ready/Stopping retain the listener snapshot; terminal states clear it. No inspect, filesystem I/O or gate lock; a snapshot proves neither liveness nor cleanup authority. |
| `owner_control_pair()` | Returns `OwnerControl` and the one-use `OwnerBootstrap` for a trusted launcher. `recovery_ticket(timeout)` issues the current recovery grant; `revoke_recovery(timeout)` revokes it. |
| `Runtime::prepare_owner_takeover(ticket)` | Freezes the configured endpoint context. Save `successor_ticket().export()` before one `commit(timeout)`; a successful commit returns the new `OwnerControl`. `inspect(timeout)` observes state without proving commit success. |
| `Error::local_io_cause()` | Returns `Option<LocalIoCause>` for bind/connect failures. `Bind(EndpointBindCause { error, unverified_reason })` preserves OS details or the typed binding refusal; `Connect(EndpointIoError)` keeps its OS cause. `error()` still returns the unchanged `EndpointIoError`. |
| `Error::ProtocolVersionMismatch { local, peer }` | A complete fixed RPC preamble reported a different nonzero `u32` version. Terminal before metadata or dispatch; no retry or relay fallback. |

Timeouts above use `Option<Duration>`. The [lifecycle guide](../../docs/local-endpoint-lifecycle.en.md) specifies ticket protection, original grace deadlines, `NotSent`/`Indeterminate` outcomes and observation ordering. The [0.8.0 Rust migration notes](../../docs/releases/0.8.0.md#rust-migration-and-transport-behavior) cover removed low-level APIs, move-only allocation owners, `ChunkRegistry<C>`, SHM response preparation and structured lifecycle failures. C-Two 0.10.0 uses [IPC handshake 14](../../docs/ipc-handshake.md), keeping C/FFI ABI 6 and SHM 3. Recompile and upgrade peers together; pre-14 peers, including 0.9.x, yield generic connection/protocol failures.

The low-level readiness failure now exposes `ServerFailure.bind_cause:
Option<EndpointBindCause>`; migrate field access and exhaustive matches to
the new binding cause without an alias. Unix credentials use schema 4, with
inode/ctime/incarnation and no persisted device. Namespace disk format 3
also versions the default directories; custom roots gain no suffix and old
Unix credentials are rejected. Windows credentials retain schema 1.

## Portable payloads

Portable payloads keep their owner-qualified name:

```rust
use c_two::{Connect, ContractLimits, ContractRelease, Runtime};
use fastdb::Payload;
```

C-Two-generated Rust modules provide typed clients and service definitions.
They consume the hidden `c_two::generated` seam; ordinary applications should
not construct encoded calls or service dispatch tables by hand.

All portable receive paths are currently copy-backed. `Held<Payload>` adds a
checked lifetime: explicit `release()` and `Drop` invalidate the FastDB owner
before releasing the C-Two response lease. Generated service inputs use the
same ordering at callback exit. This is not a zero-copy claim, and unsafe raw
pointers extracted outside FastDB's checked owner/view model cannot be
revoked.

`Held::release()` retries a failed native cleanup using the original response; the invalidator runs once, including after a caught panic. `value()` becomes `None` when invalidation starts. Core `HeldResponse::is_released()` describes that access state, while `has_pending_release()` reports native obligations. A native failure keeps the exact connection pin until cleanup succeeds or the final owner abandons it.

The low-level consuming `abandon(self)` entries on Core `HeldResponse`, `ResponseLease`, `RequestLease` and `ReassemblyBacking` end an unconfirmed obligation without another free, invalidation or refund. High-level `Held` keeps only `release()` and `Drop`. Final failures log/return their cause and increment the original scope's saturating `abandoned_releases` once. Both `BudgetSnapshot` and `MemoryScopeStats` expose this field; it counts historical abandonment rather than live leaked allocations. An abnormal same-user SHM peer can assert invalid, replayed or stale allocation coordinates; unreleaseable asserted references also count, so the value can exceed actual unreleased allocations. No background recovery is added. See [memory policy](../../docs/memory-policy.en.md#statistics-and-payload-lifetime).

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC
call. Their generated client and typed service modules use the SDK generated seam.
