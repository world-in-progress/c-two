# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased] — target Python 0.7.0 / c3 0.3.0

These versions are in preparation. Version metadata is set to 0.7.0 / 0.3.0;
publication remains pending. See [preparation and upgrade notes](docs/releases/0.7.0.md).

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

- [The source validation](docs/reports/canonical-local-endpoint-validation.md)
  records Linux and Windows Server 2022/2025 gates, portable matrices and
  installed-wheel consumers. These are development artifacts, not published
  0.7 packages. The complete target release matrices remain pending.

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
release or the current 0.7 preparation.

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
