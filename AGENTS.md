# AGENTS.md

This file contains repository-specific guidance for Codex and other coding agents working on C-Two. Follow these instructions together with the user's current request.

## 0.x Development Constraint

C-Two is currently in the 0.x line. Do not preserve backwards compatibility for incorrect, experimental, or superseded internal APIs unless the user explicitly asks for a compatibility window. Prefer clean cuts over compatibility shims: remove obsolete code paths, tests, docs, and module surfaces rather than leaving dangling fallback behavior or zombie modules that future maintainers must carry. This applies to agent work as well: when a mechanism moves from an SDK into the Rust core, update or remove stale SDK-side guidance in the same change instead of documenting both paths as equally supported.

Breaking API/ABI or protocol changes require GitHub Actions validation of the exact implementation candidate across the applicable Linux, macOS and Windows gates. Wait for all required jobs to succeed before accepting the change, and retain the source commit and run URL with the validation record. Protocol changes must remove retired representations and compatibility paths rather than supporting both old and new formats.

## Project Overview

C-Two is a resource-oriented RPC runtime that enables remote invocation of stateful resource classes across processes and machines. It is designed for distributed scientific computation, not traditional microservices.

The core abstraction is resources, not services.

- CRM (Core Resource Model): the contract class decorated with `@cc.crm(...)`. It declares remotely callable methods. Method bodies are `...`.
- Resource: the stateful object implementing a CRM contract. It is a plain Python class, not decorated. Name it by domain semantics.
- Client: any code that calls `cc.connect(...)` to consume a resource. There is no separate "Component" abstraction.

## Stateful RPC for scientific computation

C-Two resources hold state, and their calls are often non-idempotent and ordered by the caller, which awaits one call before issuing the next. Several choices that are safe for stateless RPC such as gRPC are therefore wrong here:

- Never turn a completed call into an unknown outcome. A failure fails only the call it belongs to; a connection is deactivated, and its other in-flight calls fail, only when its framing can no longer be trusted.
- Only pre-dispatch failures are retry-safe. A dispatch-uncertain call is never retried, replayed or hedged automatically; the caller decides.
- Lifecycle reports are truthful. `completed` means the real terminal state; a caller's budget bounds only its own wait and never cancels cleanup. A stopped or stuck peer keeps its loans until it releases them or its connection ends, and no block is reused without that proof.
- Do not size waits, payloads or budgets for the "usual" case. Durations and sizes that grow with a workload are exposed through the existing per-call or configuration path rather than fixed.

## Build, Test, And Run

Package manager: `uv`, not direct `pip`. Build backend: `maturin`, which compiles the Rust native extension through PyO3. Source builds require Rust 1.95 or newer; every user-facing Rust package declares that `rust-version`.

Native Rust tests embed Python and import the real public exception class. After building the extension, run `uv run python tools/dev/test_native.py`; this forwards that interpreter's library and installed-package paths to Cargo. Both shared CI and local test runners reuse this entry point.

```bash
# Share one Cargo build directory across all workspaces in this checkout.
export CARGO_TARGET_DIR="$PWD/core/target"

# Install dependencies and compile the Rust native extension
uv sync

# Force rebuild after Rust source changes
uv sync --reinstall-package c-two

# Run the full Python test suite. Empty C2_RELAY_ANCHOR_ADDRESS avoids env interference.
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30

# Ensure the Python 3.10 compatibility syntax check executes instead of
# skipping. Keep 3.10 coverage because downstream Taichi-based stacks can still
# be pinned to Python 3.10.
uv python install 3.10
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python -q --timeout=30 -rs

# Run one test file
uv run pytest sdk/python/tests/unit/test_wire.py -q

# Run one test class or function
uv run pytest sdk/python/tests/unit/test_held_result.py::TestHeldResultBasic::test_access_value -q

# Rust core tests
cargo test --manifest-path core/Cargo.toml --workspace

# User-facing Rust SDK, including real route-bound examples
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host

# Compiled Rust/Python portable proof and exact 18-row direct/relay matrix.
# These are normal gates, not opt-in tests.
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/integration/test_portable_payload_cross_language.py \
  sdk/python/tests/integration/test_portable_payload_matrix.py \
  -q --timeout=300
uv run pytest tests/repo/test_portable_matrix_receipt.py -q

# Python SDK native extension and tests
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30

# Single-process example with thread preference
uv run python examples/python/local.py

# IPC example, two terminals
uv run python examples/python/ipc_resource.py
uv run python examples/python/ipc_client.py <address>

# Relay mesh example, three terminals
python tools/dev/c3_tool.py --build --link
c3 relay --bind 0.0.0.0:8300
uv run python examples/python/relay_mesh/resource.py
uv run python examples/python/relay_mesh/client.py

# Example dependencies such as pandas
uv sync --group examples

# CLI tool
c3 --version
c3 relay --upstream grid=my_server@ipc://my_server --bind 0.0.0.0:8080
```

Windows ARM64 uses the same native source gates on `windows-11-arm`, including Rust/Python/Node interoperability and standard-user consumers. Release wheels cover Python 3.11–3.14 and 3.14t on ARM64; other platforms retain 3.10. Windows syntax-only 3.10 checks install/find `3.10-windows-x86_64-none`; runtime tests must use the native architecture.

The full Python suite runs the real relay benchmark and requires `hey` v0.1.4 on PATH. Install it with `go install github.com/rakyll/hey@v0.1.4` and expose the Go binary directory (`go env GOPATH` plus `/bin`, or `GOBIN`). Both CI suite entry points provision this prerequisite.

Tests use `pytest` with a 30-second per-test timeout. Tests live under `sdk/python/tests/unit/` and `sdk/python/tests/integration/`, with shared fixtures under `sdk/python/tests/fixtures/`. The minimum-supported-Python syntax test discovers `python3.10` or `uv python find 3.10`; run `uv python install 3.10` before the suite if you need to guarantee it does not skip. For broader 3.10 smoke coverage without replacing the default `.venv`, use a separate environment:

```bash
UV_PROJECT_ENVIRONMENT=.venv-py310 C2_RELAY_ANCHOR_ADDRESS= uv run --python 3.10 pytest sdk/python/tests/unit/test_python_examples_syntax.py -q --timeout=30 -rs
```

## Architecture and invariants

The language-neutral Cargo workspace is `core/`; the Python SDK owns its PyO3/maturin extension in `sdk/python/native/`, projecting Core as `c_two._native`.

| Layer | Crate | Purpose |
| --- | --- | --- |
| foundation | `c2-contract` | `c-two.contract.v2` validation, canonical descriptor hashing, release identity, and opaque nested-spec extraction |
| foundation | `c2-codegen` | FastDB Core delegation plus deterministic multi-owner artifact composition and new-tree publication |
| foundation | `c2-config` | Unified IPC and relay configuration structs/resolvers |
| foundation | `c2-error` | Canonical error registry and versioned `C2E1` wire codec |
| foundation | `c2-mem` | Buddy allocator, SHM regions, unified memory pool |
| foundation | `c2-mem-ffi` | Native memory and endpoint ABI, including verified Windows connection transfer |
| protocol | `c2-wire` | Wire protocol codec, frames, chunk assembler, chunk registry |
| transport | `c2-local` | Async Unix UDS / Windows Named Pipe streams, listener ownership and cancellation |
| transport | `c2-ipc` | Async IPC client, local streams, SHM, chunked transfer |
| transport | `c2-server` | Local server with per-connection state and peer SHM lazy-open |
| transport | `c2-http` | HTTP client, relay-aware client, and HTTP relay server behind `relay` feature |
| runtime | `c2-core` | Language-neutral runtime, route transactions, client pools, relay projection, and error normalization |
| sdk/rust | `c-two` / `c_two` | User-facing Rust facade, FastDB adapter, and checked held/borrowed owners |
| sdk/python/native | `c2-python-native` | PyO3 bindings for `c_two._native` |

- Rust Core owns generic runtime, transport, resolver, route, retry and error mechanisms; SDKs own language glue and domain authoring.
- Direct IPC remains a complete standalone mode when relay is unset or unavailable.
- Route acquisition and dispatch validate the route name, complete CRM identity, ABI hash and signature hash in Rust.
- Core owns deadlines, readiness, registration and lifecycle transactions; Python projects the native authority without retaining a second state machine.
- Each direction copies a transport payload into a local `Vec` and releases its lease before handing the bytes on: a request is released before the resource method runs, a response before decoding or delivering a hold; PyO3 moves that same `Vec` into its buffer projection without a second copy.
- `c2-core` does not depend on FastDB; Rust users name `fastdb::Payload`, and Rust and Python expose the same portable capabilities.
- Same-process calls pass Python objects directly and skip serialization.
- Python imports `c_two as cc`; Rust applications import the `c_two` facade and leave hidden `c_two::generated` seams to generated code.
- CRM classes use plain names without an `I` prefix; resource implementations use domain names, with `{ContractName}Impl` allowed for generated code.

The Phase 0B local candidate and its exact source provenance are recorded in the [local candidate evidence](docs/acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md); current qualifications are in the [development guide](docs/reference/development.md#local-candidate-evidence-and-provenance).

## Retired interfaces

Retired surfaces have no compatibility aliases; names of still-supported internal Rust types below identify forbidden SDK authority or application construction, not removal of those Core types.

| Retired interface or SDK pattern | Current owner or replacement |
| --- | --- |
| `@cc.transferable`, `@cc.transfer(buffer='hold')` | Use `@cc.transfer(input=..., output=...)`; select retained responses with `cc.hold(...)` and borrowed inputs at registration. |
| `compo/`, `@cc.runtime.connect`, `Component` | Use CRM contracts, plain resource classes and `cc.connect(...)` clients. |
| `_server_ipc_overrides`, `_client_config`, `_client_ipc_overrides`, `_pool_config_applied`, Python server-id/server-address authorities | Core Runtime and `RuntimeSession` own identity and configuration projection/freeze. |
| Direct `RustClientPool`, `RustHttpClientPool`, `RustHttpClientPool.instance()`, `_http_pool`, `_rollback_registration` in Python registry | Use native session acquire/connect and registration/unregister/shutdown outcomes. |
| `_relay_control_client`, `_relay_control_address`, `_relay_control_client_for()`, direct `RustRelayAwareHttpClient` construction in `registry.py` | Use `RuntimeSession.connect_via_relay()` and `connect_explicit_relay_http()` with full route contracts. |
| Python IPC/relay default and key validation tables, duplicate scheduler state | Rust `c2-config` validates defaults and overrides; the native route-concurrency handle owns scheduling. |
| Runtime context manager, `Runtime.serve()`, global Runtime registry, automatic endpoint directory deletion | Call explicit Runtime startup/shutdown; `cc.serve()` owns signals only for the default Runtime, and applications own custom directories. |
| Python pending/closed cleanup state, extra cleanup transactions, historical-outcome scans | Observe the concrete attempt’s original native `ShutdownTransaction` receipt. |
| Python startup timers, readiness polling, `os.path.exists(socket_path)`, `_started`, `config error` on `ServerError::Failed` | Use native lifecycle projection and `RustServer.start_and_wait()`; preserve typed startup failures. |
| `_native.CoreError`, legacy-named error codec APIs, Python error-wire parsers | Use public `c_two.error.CoreError` and the native canonical error registry/wire codec. |
| Name-only relay resolve/probe/call and Python or route-name-only relay IPC trust | Validate `ExpectedRouteContract` plus native server and server-instance identity. |
| SDK relay-server lifecycle APIs | Run standalone `c3 relay` through external orchestration. |
| Python `wait_closed()`, its Core blocking bridge, per-connection observation workers | Use `ConnectionEvents.recv()` and the existing Rust async close barrier. |
| Python `c_two.mem.ChunkAssembler`, `MemHandle`, `_native.ChunkAssembler`, `_native.MemHandle`, consumed/released manual wrappers | Use automatic Rust `ChunkRegistry` / `ChunkAssembler` / `ReassemblyBacking`; Rust memory APIs remain supported. |
| Replaceable receive-pool slots and safe coordinate-to-lease constructors | Frame admission mints pool-bound carriers; prepared SHM replies carry their allocated owner. |
| `endpoint_protocol`, protocol-selecting SDK/CLI APIs, logon-name fallback | `c2-config::LocalEndpoint::from_address()` derives one OS implementation and verifies native identity. |
| The owner subsystem: `OwnerBound`, owner control and its 0x16 frame, `OwnerBootstrap`, recovery tickets and takeover, owned children, `LifecycleConfig` | Removed without replacement ([ADR 0002](docs/decisions/0002-remove-owner-subsystem.md)); supervise host processes with ordinary process management and shutdown. |
| Handshake 14 or older, SHM 3, C/FFI ABI 6, relay mesh 6, old Unix credential schemas and namespace-format adoption | Use IPC 15, SHM 4, C/FFI ABI 7, relay mesh 7 and Unix credential schema 4; unsupported roots require offline archive/rebuild. |
| Second Node receive loop or compatibility ABI loader, raw socket/path/symbol injection, ABI 6 request/response pools and their split read/release | Use the complete ABI 7 endpoint-context symbols, the per-connection memory context and verified Windows connection transfer. |
| `CRMProxy.call_prepared`, `CoreClient.call_prepared`, `NativePreparedCall.call_prepared`, arbitrary client write plans | Use `begin_call` / `charge_input` / `call` / `close`, explicit FastDB payloads or Python-only pickle fallback. |
| Producerless server writable sinks and Rust IPC preallocation API | Core receives owned response bytes; `c2-server` selects allocation and transport. |
| Python held-buffer weakref registries; holds that retain transport memory (`HeldResponse` leases and pins, `abandon`, `_abandon_after_failed_release`) | A hold owns only a local copy; Core `LocalCopy*` accounting backs `memory_stats()['holds']`. |
| Automatic IPC heartbeat: `heartbeat_interval` / `heartbeat_timeout`, `C2_IPC_HEARTBEAT_INTERVAL` / `C2_IPC_HEARTBEAT_TIMEOUT`, the `heartbeat_timeout` end reason | Connection ends come from OS facts such as EOF and reset; calls wait on their own deadlines; manual ping remains. |
| Python peer memory entries `MemPool.data_addr`, `seg_data_info`, `open_peer`, `ensure_peer_segment`, `read_at`, `free_at`, `destroy`, and the c2-mem SDK lease module | Use the local allocation APIs; a pool is freed with its last owner. |
| Route revisions, `RouteWatch`, route-contract queries, `RouteAck`, NACK nonces and handshake segment lists | Identify routes by route UID, server instance and contract; the dispatcher is the single route truth; peers open SHM backings from prefix, slot and generation. |
| Host lifecycle `phase`, `terminal` and `client_held_leases` | Lifecycle snapshots report `finished`, `listener_closed` and `work_drained`. |
| The multi-field direct shutdown ACK (`acknowledged`, `shutdown_started`, `server_stopped`, route outcomes) | A one-byte ACK; direct shutdown returns `Initiated`, `Absent` or `Unconfirmed` (Python `'initiated'`, `'absent'`, `'unconfirmed'`), and completion is observed through the original receipt. |
| `PyShmBuffer`, `c_two._native.ShmBuffer`, Python wire codecs and handshake objects | Use native Core buffer owners; Python `wire.py` retains `MethodTable` and the five existing native contract projections remain available. |
| Python response-pool access, SHM coordinate tuples, threshold-based response dispatch and `bytes(memoryview)` transport conversion | Return serialized bytes or buffer-protocol data and leave native payload limits and transport allocation to `c2-server`. |

## Documentation rules and routing

**Instruction size.** Keep this file within 32 KiB: Codex reads at most `project_doc_max_bytes` (32 KiB by default) of the AGENTS.md files from the project root to its working directory and drops the rest. `tests/repo/test_docs_hygiene.py` fails above the limit. Mechanism detail belongs in `docs/reference/`, routed from the table below.

**Read before changing.** Before changing files under a path in this table, read the documents it names; they own that mechanism and its invariants. Update the owning document in the same change as the behavior.

**Documentation layout.** `docs/README.md` routes each subject to the document that owns it. `docs/reference/` states current behavior; `docs/decisions/` holds one ADR per decision, amended with dated notes rather than rewritten; `docs/design/` holds the backlog, the roadmap and plans in progress; `docs/acceptance/` holds verification records and evidence; `docs/research/` holds notes made before a decision. Before deleting a superseded document, move what still holds into its owning page. Name locations with `~` or a placeholder, never a machine's home directory.

**Terms.** [`CONTEXT.md`](CONTEXT.md) is the glossary. Use its words in code, comments, documentation and messages. When a term is defined or renamed, update `CONTEXT.md` and every current use in the same change; frozen records under `docs/acceptance/` and past release notes keep their wording.

| Path or mechanism | Owning documents to read before changing |
| --- | --- |
| `core/foundation/c2-mem/`, `core/foundation/c2-mem-ffi/` | [Memory policy](docs/reference/memory-policy.md), [C ABI](docs/reference/c-abi.md), [payload lifetimes](docs/reference/payload-lifetime.md) |
| `core/transport/c2-local/`, endpoint parts of `core/foundation/c2-config/` | [Local endpoint lifecycle](docs/reference/local-endpoint-lifecycle.md), [Windows native implementation](docs/reference/windows-native-implementation.md), [configuration](docs/reference/configuration.md) |
| `core/protocol/c2-wire/`, `core/transport/c2-ipc/`, `core/transport/c2-server/` | [IPC handshake](docs/reference/ipc-handshake.md), [memory policy](docs/reference/memory-policy.md), [payload lifetimes](docs/reference/payload-lifetime.md), [Runtime lifecycle](docs/reference/runtime-lifecycle.md) |
| Relay in `core/transport/c2-http/` | [Contracts and routing](docs/reference/contracts-and-routing.md), [configuration](docs/reference/configuration.md), [c3](cli/README.md) |
| `core/runtime/c2-core/` | [Runtime lifecycle](docs/reference/runtime-lifecycle.md), [contracts and routing](docs/reference/contracts-and-routing.md), [errors](docs/reference/errors.md) |
| `core/foundation/c2-contract/`, `core/foundation/c2-codegen/` | [Contracts and routing](docs/reference/contracts-and-routing.md), [c3 portable contracts](cli/README.md#portable-contracts) |
| `core/foundation/c2-error/` | [Errors](docs/reference/errors.md) |
| `sdk/python/` | [Python usage](docs/reference/python-usage.md), [Runtime lifecycle](docs/reference/runtime-lifecycle.md), [payload lifetimes](docs/reference/payload-lifetime.md), [errors](docs/reference/errors.md) |
| `sdk/rust/` | [Rust SDK](sdk/rust/README.md), [contracts and routing](docs/reference/contracts-and-routing.md), [payload lifetimes](docs/reference/payload-lifetime.md) |
| `cli/` | [c3](cli/README.md), [configuration](docs/reference/configuration.md) |
| Configuration in `core/foundation/c2-config/`, `sdk/python/src/c_two/config/`, `.env.example` | [Configuration](docs/reference/configuration.md), [Runtime lifecycle](docs/reference/runtime-lifecycle.md) |
| Build, CI and local runners | [Development](docs/reference/development.md), [CI and release qualification](docs/reference/ci-cd.md), [local development testing](docs/reference/local-development-testing.md), [Windows usage](docs/reference/windows-native-usage.md) |

## Python Version

Requires Python 3.10 or newer, including downstream Taichi stacks pinned to 3.10; use modern hints such as `list[int]`, `str | None`, and `tuple[...]`, and retain intentional 3.10 compatibility alongside Python 3.12 development and 3.14t free-threading validation, checking C extensions, shared state and GIL assumptions.

## Agent Working Rules

- Open work and pending decisions live only in `docs/design/backlog.md`. Read it before starting work and update it whenever an item's status changes; remove finished items and note them in the acceptance or release record. Plans and reviews keep the details; do not keep a second todo list elsewhere.
- Read the existing code before changing it.
- Prefer `rg` and `rg --files` for repository searches.
- Remove rebuildable artifacts as soon as later steps no longer need them, not only when a task finishes: Cargo target directories, `.venv` environments, native libraries, downloaded SDKs, dist bundles, wheels and package archives in validation directories and worktrees. Share one Cargo target directory per checkout, as CI does with `CARGO_TARGET_DIR=<checkout>/core/target`, instead of building a separate target for each workspace. Confirm that no active build, test or runtime still uses an artifact before removing it. Preserve source, Git state, test logs, receipts and explicitly retained deliverables; historical IPC directories and coordination files are not build-cleanup targets. Do not accumulate copies of build outputs across completed batches.
- Keep Python SDKs as glue around Rust mechanisms. Do not move shared transport, resolver, route fallback, or protocol mechanisms into Python SDK code.
- Use Rust core crates for cross-language behavior so later SDKs do not have to reimplement mechanisms independently.
- Do not treat the Python SDK as the reference implementation for generic runtime behavior. If behavior is language-neutral, prefer a Rust-core owner with thin SDK facades.
- Preserve direct IPC as relay-independent. If touching registration, client routing, or runtime session code, include checks for explicit `ipc://` connections with relay unset or unavailable.
- Preserve the copy, release, move ordering. If touching wire, SHM, scheduler, or native callback code, check that the transport lease ends right after the copy, before the resource method or a hold, and that PyO3 moves the owned request `Vec` into its buffer projection without a second copy and exposes a Python `memoryview`.
- Do not turn a transport-level SHM proof into a FastDB direct-backing claim. The current portable receive adapters are copy-backed; stronger wording requires resource-time construction into the final C-Two backing, truthful FastDB direct/staged reports, and Rust/Python proof.
- Keep local-candidate truth separate from official release truth. Reused version metadata identifies an artifact only together with its source commit and SHA-256; do not substitute registry packages for the retained candidate or call documentation-only commits package inputs.
- For bug fixes and behavior changes, add or update focused tests first when feasible, then implement the correct production-grade code change needed to satisfy the verified behavior; do not use phase boundaries to justify temporary shims or lower-quality shortcuts.
- When work is split into phases, treat the split as sequencing only. Write the phase boundaries, exit criteria, and follow-up items into the plan document before implementation, keep that plan updated as the authoritative record, and finish each phase with docs that make the remaining work explicit.
- Do not revert unrelated user changes in a dirty worktree.
- Avoid destructive git commands unless the user explicitly asks for them.
- When editing Markdown prose such as README or AGENTS content, keep each paragraph on one physical line and let editors/renderers handle visual wrapping; do not hard-wrap prose unless a table, list, code block, or generated artifact requires it.
- For reviews, lead with concrete findings ordered by severity and include file and line references.
