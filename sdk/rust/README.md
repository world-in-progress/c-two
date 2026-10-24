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

This guide uses the shared runtime behavior of Python 0.9.0 / c3 0.5.0;
this SDK retains its independent 0.1.0 version. See [release notes](../../docs/releases/0.9.0.md) for configuration and upgrade details.
Rust `c2-core` owns default `Persistent`, explicit `OwnerBound`, shutdown drain
transactions and local endpoint maintenance. SDKs use the one OS endpoint
derived by `LocalEndpoint::from_address`; there is no backend selector.
See [lifecycle integration](../../docs/local-endpoint-lifecycle.en.md) and the
[owned-child example](examples/owned_child.rs). Use matching builds and local endpoint configuration for clients, hosts and relays.

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
| `owner_control_pair()` | Returns `OwnerControl` and the one-use `OwnerBootstrap` for a trusted launcher. `recovery_ticket(timeout)` issues the current recovery grant; `revoke_recovery(timeout)` revokes it. |
| `Runtime::prepare_owner_takeover(ticket)` | Freezes the configured endpoint context. Save `successor_ticket().export()` before one `commit(timeout)`; a successful commit returns the new `OwnerControl`. `inspect(timeout)` observes state without proving commit success. |
| `Error::local_io_cause()` | Returns `Option<LocalIoCause>` for actual bind/connect failures. Its operation and `EndpointIoError` retain the OS cause; non-I/O errors have none. |

Timeouts above use `Option<Duration>`. The [lifecycle guide](../../docs/local-endpoint-lifecycle.en.md) specifies ticket protection, original grace deadlines, `NotSent`/`Indeterminate` outcomes and observation ordering. The [0.8.0 Rust migration notes](../../docs/releases/0.8.0.md#rust-migration-and-transport-behavior) cover removed low-level APIs, move-only allocation owners, `ChunkRegistry<C>`, SHM response preparation and structured lifecycle failures. IPC handshake 13 and C/FFI ABI 6 are clean version cuts; SHM 3 remains. Recompile and upgrade peers together as described in the [0.9.0 migration notes](../../docs/releases/0.9.0.md).

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

From the C-Two checkout:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

Both examples create an in-process host and perform a real route-bound IPC
call. Their generated client and typed service modules use the SDK generated seam.
