# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

See the [unreleased notes](docs/releases/unreleased.md).

### Changed

- Breaking: the Python `c_two` root and the Rust `c_two` crate root keep only the everyday API. Owner control, endpoint maintenance, diagnostics and contract tooling move to the `owner`, `endpoint`, `observe` and `contract` modules, and several of their names lose redundant prefixes. Snapshot and endpoint results are typed.
- Breaking: Rust `direct_ipc_endpoint`, `ping_direct_ipc` and `shutdown_direct_ipc` take an optional endpoint context and replace the `*_with_context` functions.

### Removed

- `C2_RELAY_ROUTE_MAX_ATTEMPTS`, the sweep and relay owner caps, duplicate lease counters, two shutdown-outcome fields, and dead Rust surface: `FreeResult`, the `SyncClient` preallocation API, IPC route-watch recovery, the relay route event history, configurable contract limits, and never-constructed errors.
- `cc.Held`, `cc.hold_stats()`, the public `ConcurrencyMode` and `InputLifetime` enums (use string values), and the default namespace and version of `@cc.crm`.
- Contract inference (`infer_crm_from_resource`, `c3 contract infer`) and the resource bridge (`cc.bridge`, `cc.ResourceBridge`, `register(bridge=...)`).

### Fixed

- Large chunked payloads, malformed control frames and acknowledgements, custom IPC roots with many entries, contended listener drops, oversized pool segments, relay mesh versions and registration retries, `c3 registry` on malformed responses, TypeScript NACKs with new error codes, and several silent Python fallbacks. See the [unreleased notes](docs/releases/unreleased.md).

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
