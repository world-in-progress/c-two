# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

See the [unreleased notes](docs/releases/unreleased.md).

## [0.12.0] — companion c3 0.8.0

Candidate; see the [0.12.0 release notes](docs/releases/0.12.0.md).

### Changed

- Breaking: coordinated protocol, ABI and dependency cut from `0.11.0` (`c3 0.7.0`). **IPC 17** (replaces 16), **C/FFI ABI 9** (replaces 8), and **FastDB `0.3.0`** (`ABI-118`, replaces `0.2.4` / `ABI-117`) introduce resource-owned publish/subscribe topics and zero-copy FastDB payload adapters while retaining SHM **4**, relay mesh **7**, Unix namespace format 3, Unix credential schema 4 and `C2E1` v1 error wire. Stop running services, upgrade Python/Rust SDKs, `c3` and Node consumers together (plus relays within one mesh), and regenerate Rust, Python and TypeScript contract clients.
- Breaking: Python `cc.register(...)` and `Runtime.register(...)` return a `Registration` handle (`name`, `publisher(topic)`, `is_closed`, `active_subscribers`) instead of a bare route-name `str`.
- Unified `WritableSlab` direct-SHM encoding across unary responses, server stream items, topic publications and client requests, paired with FastDB `0.3.0` `Payload.open_external` / `ExternalBytes` zero-copy decoding in Rust and Python SDKs (and automatic `OpenOptions.max_total_bytes` lift for `> 256 MiB` transport-admitted payloads), cutting the one-way SHM full-payload copy lower bound from **8 / 7 → 2 copies** (0 FastDB/Python glue copies) while keeping ADR 0001 copy-then-release semantics.
- Tuned buddy pool geometry (`4 × 64 MiB` at a `1 GiB` role budget) and added a bounded warm `RecycledBytes` local buffer pool across unary IPC, server streams, topic deliveries and server request copies; trimmed KB-scale inline framing, timer creation, read syscalls and Python positional argument binding overhead.

### Added

- Resource-owned publish/subscribe topics ([ADR 0005](docs/decisions/0005-publish-subscribe.md)): optional `"topics"` array in `c-two.contract.v2` (omitted when absent so legacy contract hashes remain byte-identical), registration-scoped `Publisher` with bounded history ring (`topic_capacity`) and atomic `initial_topics`, immutable `Delivery(value, seq, dropped)` with `start='latest'|'next'` and `on_lag='skip'|'error'`, same-host single-write multi-loan SHM 4 fan-out, and canonical error **`720 SubscriptionLagged`**.
- IPC 17 `Subscribe` call mode with `READY(start_seq)`, `SYNC` / `SYNC_ACK(head_seq)` and sequence-bearing `ITEM_BEGIN`; single-upstream `c3 relay` HTTP topic fan-out (`POST /{route_name}/_topics/{topic_name}`) and two-tier cross-relay fan-out (`Host → A → B → R`) with anti-loop `x-c2-relay-origin-only` and lightweight `/_confirm` late-joiner sync.
- SDK and codegen topic surfaces across Python (`@cc.topic`, `Subscription`, `cc.hold`), Rust (`Publisher`, `Subscription`, `Delivery`, `Connect::RelayAwareAt`), Node / TypeScript (`C2_MEM_FFI_ABI_VERSION 9`, unified `ipc://` + `http(s)://` endpoint context, `openSubscription`), and `c2-codegen` for all three languages.
- Python resource methods bound with `@cc.transfer(output=...)` accept `fastdb4py.payload.BuildPlan` returns and annotations (`BuildPlan`, `Payload | BuildPlan`, `Iterator[BuildPlan]`) for zero-staging direct SHM execution.
- Reproducible headless `1 KiB–1 GiB` + `4 GiB` bounded-memory streamed migration, multi-subscriber fan-out and Kostya benchmark suite vs Ray 2.56 ([2026-11-22 benchmark report](docs/acceptance/2026-11-22-kb-to-gb-slab-benchmark.md)) plus [pubsub.py](examples/python/pubsub.py) and [resident_simulation.py](examples/python/resident_simulation.py).

### Fixed

- Tightened `RuntimeSession` lock ordering and generation checks so Python objects are dropped and errors projected outside native locks; added dynamic fork-route stress coverage ([PR #234](https://github.com/world-in-progress/c-two/pull/234)).
- Idle local topic subscriptions settle immediately on close and cancel instead of waiting for the drain barrier.
- Released binding-created caller input `Payload.memoryview()` pins before awaiting replies and closed borrowed-mode deferred `BuildPlan` handles before input invalidation on error paths.

## [0.11.0] — companion c3 0.7.0

Published 2026-11-20. See the [0.11.0 release notes](docs/releases/0.11.0.md) and [release acceptance record](docs/acceptance/2026-11-20-0110-release-acceptance.md).

### Changed

- Breaking: coordinated protocol and ABI cut from `0.10.1` (`c3 0.6.0`). **IPC 16** (replaces 14; 15 was never published), **SHM 4** (replaces 3), **C/FFI ABI 8** (replaces 6; 7 was never published) and **relay mesh 7** (replaces 6) introduce single-owner shared-memory release, IPC 16 call records (`OPEN`, `CALL_END`, `RELEASE` and server-stream records) and direct Rust-backed Node clients. Upgrade the Python and Rust SDKs, `c3` and Node consumers on one host — and all relays communicating within one mesh — together, and regenerate Rust, Python and TypeScript contract clients. Each direction copies a payload and releases its loan right after the copy, so remote holds and borrowed inputs keep only local copies. Deactivation, eviction and shutdown stop new calls while letting unary calls already sent finish; pre-dispatch server refusals report `NotExecuted` (`transport_phase=pre_dispatch`) instead of resource errors.
- Breaking: generated TypeScript clients call the Rust `c2-core` client through C/FFI ABI 8 (`createNodeIpcConnector`, replacing `createNodeIpcConnect`) instead of framing IPC in TypeScript. Unary calls issued on one connection without awaiting now run concurrently instead of serially through a `callChain`.
- Breaking: capacity and configuration converge onto independent per-role domains (`client`, `server`, `relay`) with `(M, F, N)` budgets (`memory_budget_bytes`, `file_spill_budget_bytes`, `max_outstanding_calls`), 13 active Core configuration environment variables (down from 39 in `0.10.1`), symmetric `memory_stats()` (`client`, `server`, `retired`, `holds`) and call-slot-only `call_execution_snapshot()`. Python uses `set_client` / `set_server` (raising `CoreError` with `lifecycle_kind == 'config_frozen'` on post-freeze effective changes), Rust uses `RuntimeOptions::client_limits` / `HostOptions` / `DomainLimits`, `c3 relay` uses `--memory-budget-bytes` / `--file-spill-budget-bytes` alongside `--call-max-outstanding`, and Node uses connector capacity options with `connector.stats()`. Unreferenced IPC connections are evicted after a fixed 60-second idle interval in the background, and transport thresholds, chunk/frame sizes and pool geometry are fixed or derived internally.
- Breaking: the Python `c_two` root and Rust `c_two` crate root keep only the everyday API. Endpoint maintenance, diagnostics and contract tooling move to `endpoint`, `observe` and `contract` modules, and snapshots/endpoint results return typed dictionaries (with `lifecycle_snapshot()` and `shutdown_outcome()` returning `... | None`). Direct IPC `shutdown()` returns `'initiated'`, `'absent'` or `'unconfirmed'`; lifecycle snapshots report `finished`, `listener_closed` and `work_drained`; `cc.serve(shutdown_timeout=...)` returns the shutdown outcome; Rust `direct_ipc_endpoint`, `ping_direct_ipc` and `shutdown_direct_ipc` accept `Option<&LocalEndpointContext>` instead of `*_with_context`.
- Breaking: clients follow the route name ([ADR 0004](docs/decisions/0004-route-name-identity.md)). A route registered again at the same endpoint or relay serves existing clients without error, and a client whose target is known to be gone re-acquires the route by name before its next call; calls already sent are never replayed. Relays refuse duplicate registrations of names held in the mesh, unreachable relays retain their names (answering `ResourceUnavailable` / `ResourceAlreadyRegistered` with holder metadata) until recovery, withdrawal or `force-leave`, and new acquisitions fail with `RouteConflict` (719) if a healed partition or race leaves multiple live holders.
- Breaking: Python `c_two.error` converges onto `ErrorCode` (matching Rust variant names), `ResourceExecutionError` (code 3), `SerializationError` (codes 1, 2, 4, 5, 6 and 8, with `side` and `stage`) and 14 routing, admission and protocol subclasses; both `CoreError` and `CCError` expose a read-only `CCBaseError.transport_phase` matching Rust `c_two::Error::transport_phase()`, and `CoreError.semantic` replaces `CoreError.error_bytes`. `set_relay_anchor(None)` clears a Runtime's relay-anchor override and blank strings raise `ValueError`.
- `cc.connect(timeout=...)` (and TypeScript `connectTimeoutMs`) also bounds the OS connection, namespace gate or pipe wait, and handshake. A relay-aware call sends its input once instead of re-sending from a retained copy, chunked IPC calls reserve their route in frame order at their first chunk, and a relay forwards unary and stream responses slice by slice through a reusable download window.
- A frame write that exceeds the internal write timeout deactivates the connection without cutting off the call record being written; the call follows that frame's real result.
- Small IPC calls are slower: a 1-byte call takes about 40–70 µs more due to single-owner writer tasks. See the [0.11.0 release notes](docs/releases/0.11.0.md#notable-fixes-and-performance).

### Added

- Error code `CallCancelled` (718) for a server stream whose cancellation has finished, and pre-dispatch call-mode validation in IPC 16 `OPEN` using existing `UnsupportedCallMode` (716): over relay HTTP, an ordinary request to a stream method returns HTTP 406, while a stream-`Accept` request to a unary method returns HTTP 200 with a single `NotExecuted` `CALL_END` (716).
- Server streams for Rust resources and clients over direct IPC and relay HTTP: generated `ResponseStream` methods and `Service` iterators, cancellation on drop, `close()` with a 30-second default, and shutdown or unregistration ending open streams between steps with `CallCancelled`.
- Server streams for Python resources and clients over same-process calls, direct IPC and relay HTTP: `@cc.stream`, `cc.Stream` with `close(timeout=30.0)`, held items, context-preserving generator execution and generated Python stream methods; Python and Rust stream to each other.
- Server streams for generated TypeScript clients over direct IPC and relay-selected local IPC: `C2ResponseStream` methods, `C2HeldResult` items, cancellation when a `for await` loop exits early, and `close(timeoutMs)` with a 30-second default.
- Relay HTTP server-stream forwarding with `DELETE /_calls/{token}` cancellation preserving real terminal records and relay shutdown ending forwarded streams between steps with `CallCancelled`.
- Error code `RouteConflict` (719, HTTP 409) for a route name with live registrations on more than one relay, and `c3 registry force-leave` (`POST /_admin/force-leave`) to make a mesh forget an unreachable relay and its routes.
- Rust `c_two` crate root re-exports `ErrorCode`, `ErrorSide`, `ErrorStage`, `C2Error` and `TransportPhase`.

### Removed

- The owner subsystem ([ADR 0002](docs/decisions/0002-remove-owner-subsystem.md)), the automatic IPC heartbeat and its settings, route revisions, relay `POST /_echo`, and legacy C/FFI ABI 6 request/response pools and TypeScript `callChain` / custom socket injection.
- 30 environment variables from `0.10.1` (4 replaced by new names and 26 removed without aliases, plus 6 development-only intermediate keys) and superseded configuration surfaces (`set_transport_policy`, `set_call_execution_limits`, `ipc_overrides`, Rust `*IpcConfigOverrides` / `CallExecutionLimits*` / `MemoryBudgetLimits` / `call_timeout_secs`, retired `c3 0.6.0` relay pool/budget flags including `--call-retained-input-budget-bytes`, Node `createNodeIpcConnect` / `requestShmThreshold` / `requestChunkSize`).
- `cc.Held`, `cc.hold_stats()`, `MemoryLimits`, `MemoryScopeStats`, the public `ConcurrencyMode` and `InputLifetime` enums (use strings), default `namespace` / `version` on `@cc.crm`, contract inference (`infer_crm_from_resource`, `c3 contract infer`), the resource bridge (`cc.bridge`, `cc.ResourceBridge`, `register(bridge=...)`), and dead low-level Rust/Python surfaces (`FreeResult`, `SyncClient` preallocation, `ContractLimits*`, Python `MemPool` peer-read methods, producerless server writable sinks).
- Route UIDs on client surfaces (Python `route_uid`, Rust `ObservedRoute` and `observed_route()`, `x-c2-route-uid`), error codes `704 RouteStale`, `706` and `711` (permanently retired), the shared relay resolution cache, and TypeScript `maxAttempts` / `routeCacheTtlMs`.
- Python `c_two.config` (`C2Settings`, `settings`), `generate_crm_template`, `ERROR_Code`, the eight `side × stage` error subclasses and `RegistryUnavailable` / `RouteWatchUnavailable` subclasses, `CoreError.error_bytes`, `CCError.serialize` / `deserialize`, `_native.encode_error_wire` / `decode_error_wire_parts` and `c_two.transport.server.reply`.

### Fixed

- Updated `mio` to `1.2.4` across all lockfiles to fix a Windows Named Pipe use-after-free race confirmed by crash dump (`0xC0000005` when a listener closed or a pipe disconnected).
- A direct IPC shutdown ACK is now visible only after the server has closed admission to new calls on every connection.
- Large chunked payloads (>64 MiB), malformed control frames and acknowledgements, custom Unix IPC roots with more than 64 entries, contended listener drops, oversized pool segments, relay mesh version checks and registration retries, `c3 registry` on malformed responses, TypeScript NACKs with unknown error codes, and silent Python fallbacks. See the [0.11.0 release notes](docs/releases/0.11.0.md#notable-fixes-and-performance).
- A finite call releases its `max_outstanding_calls` slot before its result reaches the caller; generated TypeScript relay-aware clients reuse one local IPC connection per server instance; concurrent callers acquiring the same endpoint share one connection attempt and preserve native OS error codes.
- Once a Host stops admitting calls during shutdown, route lookup and listing report its routes closed with reason `shutdown`; on Windows, a client's `DISCONNECT` no longer loses to a failed reply flush as the connection's first close reason; a relay discards unread request bodies before sending pre-dispatch refusals so Windows clients receive the refusal instead of a connection reset.
- Releasing a held response no longer deadlocks with another thread exporting a buffer of the same response; a request racing relay route withdrawal reports `ResourceNotFound` instead of `ResourceUnavailable`; `c3 relay --bind` with port `0` advertises and prints the actual bound port; abandoning a relay HTTP stream body releases both `PullBody` and driver `ReusableWindow` handles before cancelling upstream.
- `c2-server` reports pre-dispatch unknown-method and request record/storage refusals as `ProtocolViolation` (713, `NotExecuted`) instead of codes 3 or 1, and post-execution unary response capacity exhaustion as `CallCapacityExceeded` (717, `Failed`) instead of code 2; outer transport phases take precedence in Core, Python and C ABI 8, unary Python clients pass non-semantic `CoreError` instances through unchanged, and closed `CRMProxy` handles fail locally with `ResourceClosed`.

## [0.10.1] — companion c3 0.6.0

Published 2026-10-30. See the [release notes](docs/releases/0.10.1.md).

### Fixed

- Python `cc.serve()` startup and shutdown messages use ASCII text, escape route names that stdout cannot encode, and flush output so piped banners appear immediately. Non-UTF-8 stdout no longer raises `UnicodeEncodeError` or skips graceful shutdown.
- A zero-timeout shutdown right after a completed shutdown no longer fails intermittently with `shutdown_wait_timeout` at `core_retirement_wait`; the Core shutdown driver wakes waiters only after releasing its locks and retirement resources.
- `generate_crm_template` reads the CRM source file with its declared encoding (UTF-8 by default) and writes the template as UTF-8, and `python -m c_two.cli.contract`, which the `c3 contract` subcommands run, writes its JSON as UTF-8 to files and stdout. Both previously used the platform's default encoding, so on Windows non-ASCII text such as Chinese docstrings could fail to decode, come out garbled, or be rejected by `c3` as non-UTF-8 output.

## [0.10.0] — companion c3 0.6.0

Published 2026-10-30. See the [release notes](docs/releases/0.10.0.md) for IPC 14 version diagnostics, Unix namespace format 3 and credential schema 4, listener credential snapshots, independent Python runtimes, failed-release ownership, the Rust 1.95 build requirement, FastDB 0.2.4 and the coordinated upgrade.

## [0.9.0] — companion c3 0.5.0

Published 2026-10-25. See the [release notes](docs/releases/0.9.0.md) for Windows user-scoped IPC, native server identity checks, cross-session transfer, IPC 13 and C/FFI ABI 6, plus the full wheel inventory and migration requirements.

## [0.8.2] — companion c3 0.4.1

### Added

- One bounded physical IPC event feed for managed outgoing and accepted connections, with `side`, shared observation IDs and cross-thread subscription close. Python waits through `ConnectionEvents.recv()`; the GIL is released.
- Public typed `c_two.error.CoreError`, sharing `CCBaseError` with semantic errors and retaining `RuntimeError` ancestry. Every structured field is present and documented.

### Fixed

- Relay upstream observers recover after temporary owner-capacity exhaustion through the existing retry loop. Status updates/removal use the original task generation under one lock, preserving independent unavailable-record lifetime and capacity/release semantics.

### Changed

- Removed Python `ConnectionObservation.wait_closed()`, its Core blocking bridge and `_native.CoreError`, without aliases. Low-level Rust observation constructors require `ConnectionSide`; the async native close primitive remains. IPC 12, SHM 3 and C/FFI ABI 5 are unchanged. See [0.8.2 migration notes](docs/releases/0.8.2.md).
- Removed the unused low-level Python `c_two._native.ShmBuffer` export and its native module without compatibility aliases. The existing Core request snapshot, response receive path and transport lease ordering are unchanged; R06 active buffer state, C/FFI ABI 5, IPC 12, SHM 3 and the Rust API are unchanged.
- Low-level TypeScript IPC connections require an idempotent `close()` that terminates pending reads and writes; custom Node sockets require `pause()` and `resume()`. There are no optional-close compatibility paths.
- Generated chunked calls receive replies during upload, close and join failed exchanges, and retain the first confirmed failure and exact SHM release. The next call reconnects under the existing identity checks; successful calls reuse their connection and failed calls are never replayed. The official Node connector pauses idle input. IPC 12, SHM 3 and C/FFI ABI 5 are unchanged.

## [0.8.1]

### Fixed

- Python `cc.set_server(startup_timeout=...)` configures the existing Core startup budget; explicit Rust HostOptions win, then Runtime configuration, then the finite five-second default. No Python timer is added.
- Startup expiry exposes `lifecycle_kind='startup_timeout'`, stage and configured seconds; local bind failures retain their OS cause and no longer appear as configuration errors. See [0.8.1 migration notes](docs/releases/0.8.1.md).

## [0.8.0] — companion c3 0.4.0

### Changed

- Ordinary SHM replies reuse `PreparedShmResponse` preparation and sending.
  The low-level Rust preparation API returns `Result<Option<PreparedShmResponse>, String>`;
  ordinary release uses the existing `free_at` bookkeeping and shared error
  messages. Sending failures never trigger fallback or replay.
- Allocation owners are move-only and pool-bound; low-level raw memory access
  requires explicit unsafe contracts. Consumers must migrate and recompile;
  there are no compatibility constructors.

- Breaking low-level Rust chunk API: `ChunkRegistry<C>` owns each assembly and
  its required context together. `take` / `take_id` replace `finish` / `abort_id`;
  `set_route_info`, `FinishedChunk` and Rust assembler route fields are removed
  without aliases. GC returns route permits with their assembly instead of a
  later reconciliation sweep. Python metadata access, admission and release
  ordering, IPC 12, SHM 3 and C/FFI ABI 5 retain their contracts.
- Breaking Rust error API: `ServerLifecycleState::Failed` and `LifecycleError::Server` now carry `ServerFailure`; `ServerError::Bind` / `Failed` and `IpcError::Connect` identify local endpoint failures. Core and the Rust SDK expose `LocalIoCause` through the existing error chain. There are no compatibility variants; IPC 12, SHM 3 and C/FFI ABI 5 are unchanged.
- Dependencies now pin FastDB 0.2.3 across Python, Rust,
  TypeScript validation and the verified Core SDK. SDK preparation also
  recognizes the official Windows ARM64 bundle. Portable payload semantics
  and FastDB ABI-117 are unchanged.
- Breaking low-level owner API: `owner_control_pair()` now returns `OwnerControl` and Rust `OwnerBootstrap` / Python `NativeOwnerBootstrap`, replacing the single-direction API without compatibility aliases. Explicit recovery tickets and one-use prepared takeover replace the old ownership relationship; `close()` starts grace, while `request_shutdown()` requests drain. Ordinary OwnerBound admin shutdown receives a negative ACK; Persistent and trusted in-process shutdown retain their behavior.
- OwnerMissing grace now preserves published Ready routes and their UID/revision while calls report `ResourceUnavailable`. Takeover must commit before the original monotonic deadline, consumes the old grant and advances the owner epoch; failed attempts cannot extend grace. Lost confirmation requires explicit recovery with saved tickets, without automatic replay or old/new-key fallback. Owner control uses independent version 1; IPC 12, SHM 3, C/FFI ABI 5 and business frames are unchanged.

### Added

- Windows ARM64 release wheels for Python 3.11–3.14 and 3.14t, plus the
  native `c3-aarch64-pc-windows-msvc.exe`. Source and package gates run on
  `windows-11-arm`, including cross-language calls and standard-user cleanup.
  The PowerShell installer selects the OS architecture even under emulation.
- Rust and Python expose read-only physical IPC connection observations and
  bounded server connection events on Unix sockets and Windows Named Pipes.
  Idle peers are observed by the existing receive loop; waiting requires no
  business call. Terminal reasons describe transport facts, not process death
  or completion of resource and payload cleanup.

### Fixed

- Local bind/connect failures retain their original OS kind and code through readiness waiters and Python `CoreError.io_kind`, `raw_os_error` and `io_operation`. Contextual messages, outer policy refusals, error ordering and retry budgets remain unchanged. Deadline, handshake and other non-local-I/O failures do not acquire an OS cause.
- A relay registration response arriving after owner loss or shutdown cannot
  reopen route admission with an obsolete registration token.

- Direct IPC admin shutdown retries only before control writing begins; loss
  of confirmation after submission does not replay the request. ACK still
  proves initiation only.
- Non-Hello owner setup refusal obeys the original setup deadline.
- Development dependencies use patched Pillow 13.0.0 and certifi 2026.10.19.
  Banner generation has a separate Python 3.11+ group; SDK and ordinary test
  environments retain Python 3.10 support.

See [0.8.0 migration and release scope](docs/releases/0.8.0.md).

## [0.7.6] — companion c3 0.3.5

- Fixed concurrent outgoing IPC shutdown reports that could label completed
  client cleanup as unconfirmed. See [the release notes](docs/releases/0.7.6.md).

## [0.7.5] — local preparation, companion c3 0.3.4

### Fixed

- Ordinary Python calls no longer install or import FastDB/NumPy. FastDB
  Payload contracts select `c-two[fastdb]`, pinned to `fastdb4py==0.2.2`;
  missing dependencies report that extra explicitly.
- Outgoing IPC shutdown bounds the DISCONNECT write/ACK exchange to 100 ms
  per connection, then observes the existing local cleanup barriers. Outcomes
  add `ipc_clients_abnormally_closed` and `ipc_client_close_details` with
  addresses/reasons. Server work drain and shutdown deadlines retain their
  meaning.
- Registration rejects implementation-only `@cc.on_shutdown` markers instead
  of silently ignoring them; the CRM contract remains the hook authority.
- Buddy releases validate the exact allocation instance, rejecting wrong
  parent/child coordinates and stale identifiers after same-location reuse.
- Default macOS coordination records survive system temporary-file cleaning;
  socket paths and strict identity/lifecycle rules are preserved. Failed
  initialization rolls back only its own newly created, unchanged lease.

### Changed

- SHM format 3, IPC handshake 12 and C/FFI ABI 5 require coordinated native
  upgrades. There are no old-format compatibility paths.
- Rust encoded services take owned request snapshots. IPC override types use
  flat fields; nested `BaseIpcConfigOverrides`, stored `max_pool_memory` and
  peer handshake shadow state are removed. Regenerate/recompile consumers.
- Obsolete Python wire/handshake projections, three Python `call_prepared`
  entries, the transfer `buffer` keyword and generated TypeScript Node IPC
  factories/types are retired. Use FastDB payloads and official connection
  injection; Python-only pickle and Core byte/preallocation APIs remain.
- HTTP discovery uses the strictly validated response header as namespace
  authority. Runtime/test cleanup removes duplicate state while preserving
  budgets, validation ordering, snapshots, leases and lifecycle authority.

See [upgrade boundaries and preparation status](docs/releases/0.7.5.md).
The three new Python/runtime fixes leave business CRM fingerprints unchanged;
SHM, IPC and FFI versions above include the prior accepted allocation repair.

## [0.7.0] — companion c3 0.3.0

This entry describes Python `c-two` 0.7.0 and its companion CLI `c3` 0.3.0.
See the [release and upgrade notes](docs/releases/0.7.0.md) for installation,
migration requirements and the release evidence.

### Added

- Rust-owned `Persistent` and explicit `OwnerBound` service lifecycles, inherited
  owner control, owned-child supervision and structured shutdown completion.
- Exact endpoint credentials, reaping and bounded maintenance scoped to a
  runner's own logical addresses.
- Finite SHM/file/reassembly budgets, `cc.memory_stats()` and independent relay
  upstream IPC policy with shared accounting across reconnects.

### Changed

- Unix IPC uses one private native v2.2 namespace. The superseded Unix backend
  and experimental backend selectors have been removed. Upgrade communicating
  clients, resource servers and c3 together; old endpoints are not migrated.
- Buddy allocation is lazy by default, with explicit prewarm and idle retention.
  `pool_enabled=False` skips buddy while dedicated SHM and checked transport
  fallback remain available.
- The C/Node memory binding uses ABI 3; addons and native libraries must match.
  Payload leases remain independent of endpoint and runtime shutdown.

### Validation status

- [The source validation](docs/acceptance/canonical-local-endpoint-validation.md)
  records Linux and Windows Server 2022/2025 gates, portable matrices and
  installed-wheel consumers. These are development artifacts, not published
  0.7 packages. The formal release retains its own source-bound candidate
  manifest and promotion evidence with the [c3 release](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.3.0).

## [0.6.0] — 2026-09-25

Python [c-two 0.6.0](https://pypi.org/project/c-two/0.6.0/) was published with
30 wheels and one sdist, alongside
[c3 0.2.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.2.0)
for Windows x64 and Linux/macOS x86_64/aarch64. This line includes portable
FastDB payloads and native Windows IPC and consumes FastDB 0.2.1. The Rust SDK
remains source-only; this entry does not imply a C-Two crates.io or npm release.

## Historical portable local candidate — 2026-07-24

The following record describes the earlier local candidate and its exact input
commits. Its publication statements concern that candidate, not the later 0.6.0
release or the 0.7 release.

### Added

- Added the supported Rust SDK at `sdk/rust` as Cargo package `c-two`, imported as `c_two`, with direct IPC, explicit relay, relay-aware clients, hosts, generated typed clients/services, structured errors, and checked FastDB lifetime adapters at parity with Python.
- Added bounded `ContractLimits`, the shared language-neutral `c2-core` client/host/runtime facade, exact 18-row Rust/Python direct/relay proof, and exact 12-row generated TypeScript Node real-call proof.
- Added a canonical 42-artifact local release-candidate manifest plus isolated version-only Rust, no-index CPython 3.10/current, and tarball-only Node consumer receipts.

### Changed

- Python now projects shared route, transport, retry, error-normalization, lease-ordering, and lifecycle behavior through `c2-core` instead of retaining a second SDK-local authority.
- Rust code generation now targets the supported `c_two` seam rather than asking users to assemble low-level transport crates.

### Release status

- The candidate is local and unpublished. Its exact package-input commits are C-Two `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02` and FastDB `7eb74734926bd8fe911229eee9744a6dd8172487`; no version bump, push, tag, hosted pass, publication, or release is implied.
- Official immutable distribution, browser runtime, C++ SDK, streaming, compatibility ranges, trust/signature/revocation, post-dispatch retry/deduplication, and Toodle consumption remain open.

## [0.4.7] — 2026-04-22

### Fixed

- Cross-machine relay traffic no longer goes through system `HTTP_PROXY` /
  `HTTPS_PROXY` by default. Forward proxies are known to normalize
  percent-encoded `%2F` in URL path segments to `/`, which broke resource
  names containing `/` (e.g., `nhri/simulator`) and silently rerouted CRM
  call traffic through the proxy. Both the Python registry's `urllib`
  client and all Rust `reqwest::Client` instances (CRM HTTP client, relay
  mesh disseminator, anti-entropy / failure-detection / route-pull loops,
  upstream IPC client builder) now bypass system proxies. Set
  `C2_RELAY_USE_PROXY=1` to opt back in.

  The `c3 registry list-routes / resolve / peers` admin commands also
  bypass system proxies for consistency with runtime traffic. If
  `reqwest::ClientBuilder::build()` ever fails, c-two now panics rather
  than silently falling back to a proxy-respecting `Client::default()`.
  The `C2_RELAY_USE_PROXY` flag is resolved through the shared Rust
  config layer so Python and Rust callers always see a consistent view
  (no cached pydantic settings).

### Added

- `C2_RELAY_USE_PROXY` env var (default: `false`) controlling whether
  c-two relay HTTP traffic honors system proxy environment variables.

## [0.4.3] — 2026-04-18

### Added

- `@cc.transfer()` decorator for per-method control over input/output transferable types and buffer mode
- Hold mode auto-detection: server automatically selects hold vs view based on `from_buffer` availability
- `cc.hold()` client-side API with `HeldResult` (context manager + explicit `.release()` + `__del__` fallback)
- `cc.hold_stats()` for server-side SHM buffer monitoring
- Relay mesh with gossip-based route propagation (`c3 relay --seeds`)
- Anti-entropy digest exchange for mesh consistency
- Chunked streaming for payloads beyond 256 MB
- Three-tier memory: buddy SHM → dedicated SHM → file-spill fallback
- Python 3.14t (free-threading) support

### Changed

- IPC addresses are now auto-generated by default (no need for explicit `set_ipc_address()`)
- `@transferable` metaclass auto-converts `serialize`/`deserialize`/`from_buffer` to static methods

## [0.3.0] — 2026-03-01

### Added

- IPC transport via Unix domain sockets + POSIX shared memory
- HTTP relay transport (`c3 relay`)
- Buddy allocator for shared memory management
- Wire protocol with frame encoding
- `@cc.crm()` contract decorator
- `@cc.transferable` custom serialization
- `@cc.read` / `@cc.write` concurrency annotations
- `cc.register()` / `cc.connect()` / `cc.close()` / `cc.shutdown()` API
- Thread-preference mode (zero serialization for same-process calls)
