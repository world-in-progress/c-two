# Local endpoint protocol routing: propagation slice

Baseline: `3c9cdbc88c17fbc105a396eda00491aa89ea07d2`.

The baseline commit is the Host-quarantined, **unverified** partial slice: ten
files and 212 lines that were never accepted as a verified implementation.
This run did not resume any earlier run and did not treat the quarantined patch
as sound. It inspected that patch, repaired the two real defects it contained,
completed the propagation surface across the Rust admin facades, the Rust SDK,
the TypeScript native transport, and the Python SDK, and sealed focused
evidence for each layer.

The rule this slice enforces is single-source, strict endpoint selection: one
resolved `LocalEndpointProtocol` names exactly one operating-system endpoint,
and no path — client construction, server bind, restart bind, relay
attestation, data-plane call, or administrative probe — falls back to another
endpoint namespace to find a live server.

## Repairs to the quarantined baseline

Two defects were found and fixed. Both were confirmed by failing focused tests
before the repair and passing after it.

### 1. `direct_ipc_endpoint` ignored the resolved process protocol

The quarantined patch routed `ping_direct_ipc` and `shutdown_direct_ipc`
through the resolved process client IPC policy but left `direct_ipc_endpoint`
on the hard `LegacyV1` derivation in `c2_ipc::local_endpoint_from_ipc_address`.
Under a resolved `managed-v2` policy the endpoint diagnostics named the legacy
path while the probe actually reached the managed path, so the two projections
of the same logical address disagreed.

`core/runtime/c2-core/src/control.rs` now resolves the process protocol first
and delegates to the explicit constructor:

```rust
pub fn direct_ipc_endpoint(address: &str) -> Result<LocalEndpoint, LifecycleError> {
    let protocol = resolved_admin_endpoint_protocol()?;
    direct_ipc_endpoint_with_protocol(address, protocol)
}
```

This is a single-derivation change, not a new fallback: the endpoint a caller
prints is now always the endpoint a probe dials.

### 2. `shutdown_with_protocol` misreported absence as a server it had stopped

Pinned by the failing test
`managed_admin_shutdown_cannot_stop_a_legacy_server` and its mirror. The
quarantined `shutdown` path returned

```text
acknowledged: true, shutdown_started: true, server_stopped: true
```

for an endpoint that did not exist under the requested protocol. That is not a
fallback bug — the probe genuinely never reached the other server — but it made
a wrong-namespace probe indistinguishable from a real remote stop, which is
exactly the signal an admin supervisor consumes.

Absence is now a named, endpoint-scoped outcome (`already_stopped()` in
`core/transport/c2-ipc/src/control.rs`): `acknowledged: true`,
`shutdown_started: false`, `server_stopped: true`. The `shutdown_started` flag
is the discriminator — only a live server that received the initiate frame sets
it. The existing test
`absent_endpoint_has_no_ping_and_is_already_stopped` keeps its original
semantics, and the new core tests assert the discriminating flag directly.

## Propagation surface

- **IPC client** (`core/transport/c2-ipc/src/client.rs`): `IpcClient`
  construction, reconnect, and transient-retry all reuse one derivation from
  `config.base.endpoint_protocol`.
- **IPC server** (`core/transport/c2-server/src/server.rs`): construction and
  restart bind both call `parse_local_endpoint_with_protocol` with the resolved
  config. The legacy wrapper is `#[cfg(test)]` so production code cannot derive
  a legacy endpoint from an unresolved config.
- **Relay** (`core/transport/c2-http/src/relay/authority.rs`,
  `.../relay/server.rs`): registration validation and the register-attestation
  client both read `config().upstream_ipc.base.endpoint_protocol`, so
  attestation and data plane name the same endpoint.
- **Rust SDK** (`sdk/rust/src/lib.rs`, `sdk/rust/Cargo.toml`): the explicit
  constructors and probes are re-exported as a thin facade, and
  `LocalEndpointProtocol` is re-exported from `c2-config` rather than
  redeclared, so callers cannot pass a name the resolver would reject.
- **PyO3** (`sdk/python/native/src/control_ffi.rs`): `ipc_endpoint_name`,
  `ipc_ping`, and `ipc_shutdown` gained an optional protocol with an explicit
  `#[pyo3(signature = (..., protocol=None))]`. PyO3 cannot infer a default for
  `Option`, so the attribute is required for the historical one-argument
  endpoint call and the two-argument ping/shutdown calls to keep working.
- **Python SDK** (`sdk/python/src/c_two/transport/client/util.py`): keyword-only
  `endpoint_protocol` passthrough on all three helpers; no Python-side
  validation of the protocol vocabulary.
- **TypeScript native transport** (`core/foundation/c2-mem-ffi`): the transport
  lives in `core/foundation/c2-mem-ffi`, not in a `sdk/typescript` directory.
  The client config parser and base projection now carry
  `C2LocalEndpointProtocol` (`legacy-v1` / `managed-v2`) and
  `endpointProtocol` on resolution and connect options; the C ABI
  `c2_mem_ffi_local_endpoint_len/_copy` take a protocol name, and the Node
  loader forwards it. The allocator and pool surfaces are untouched.

## Freeze and pool reuse

`BaseIpcConfig::endpoint_protocol` is a plain field of the `PartialEq` struct
that `ClientIpcConfig` compares as a whole. No new equality plumbing was added,
because the existing plumbing already covers it:

- `ClientPool::acquire` compares `entry.client.config() != &cfg` on a
  same-address hit and on a racing insert, so a cached connection built under a
  different protocol is rejected instead of reused.
- `Runtime::acquire_ipc_client` freezes the resolved client config — endpoint
  protocol included — under the `RuntimeState` lock before any connection I/O,
  and `set_client_ipc_overrides` returns `LifecycleError::ClientConfigFrozen`
  afterwards.

The focused test `client_cannot_change_the_endpoint_protocol_after_the_domain_freezes`
proves the freeze covers the protocol specifically: a failed connect still
freezes, and a later override that changes the protocol is rejected while the
frozen value is preserved.

## Focused validation

All commands ran in this workspace on macOS (arm64) unless noted. Every suite
below is a focused selector, never a full workspace build. Rows marked
**(re-verified)** were re-run after the platform-branch repair described under
"Platform branches", and their counts are the numbers this revision observed.

| Layer | Command | Result |
| --- | --- | --- |
| core endpoint propagation **(re-verified)** | `cargo test --manifest-path core/Cargo.toml -p c2-core --test endpoint_protocol` | 8 passed, 0 failed |
| c2-core lib | `cargo test --manifest-path core/Cargo.toml -p c2-core --lib` | 36 passed, 0 failed |
| c2-ipc | `cargo test --manifest-path core/Cargo.toml -p c2-ipc --lib` | 160 passed, 0 failed |
| c2-server protocol | `cargo test --manifest-path core/Cargo.toml -p c2-server --lib -- server_restart_reuses_the_resolved_endpoint_protocol server_new_default_config` | 2 passed, 0 failed |
| c2-http relay tests | `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib relay::server::tests` | 17 passed, 0 failed |
| c2-config | `cargo test --manifest-path core/Cargo.toml -p c2-config --lib` | 103 passed, 0 failed |
| Rust SDK **(re-verified)** | `FASTDB_PAYLOAD_LINK_MODE=system FASTDB_PAYLOAD_SYSTEM_LIB_DIR=/private/tmp/c2-memory-fastdb-sdk/lib DYLD_LIBRARY_PATH=... cargo test --manifest-path sdk/rust/Cargo.toml --test public_api` | 7 passed, 0 failed |
| TypeScript typecheck | `TSC=/opt/homebrew/bin/tsc node scripts/build-types.mjs --no-emit` | exit 0 |
| TypeScript node loader **(re-verified)** | `node --test tests/c2-mem-ffi-node-loader.test.mjs` | 15 passed, 0 failed |
| TypeScript binding | `node --test tests/c2-mem-ffi-binding.test.mjs` | 22 passed, 0 failed |
| C ABI projection | compiled probe against the real `libc2_mem_ffi.dylib` | passed |
| Python native crate | `cargo build --manifest-path sdk/python/native/Cargo.toml --lib` | exit 0 |
| Python wheel | `maturin build` from `sdk/python/native` | wheel built |

`C2_RELAY_ANCHOR_ADDRESS=` was set for every Rust invocation to keep relay
environment state out of the results.

## Platform branches

`managed-v2` is a Unix-only endpoint namespace. `c2-config` refuses to derive a
managed-v2 endpoint on Windows (`ErrorKind::Unsupported`), so a test that
unconditionally expects a successful `ManagedV2` derivation is wrong on
Windows, and correctness there means asserting the concrete negative
configuration branch instead of skipping or ignoring the case:

- `core/runtime/c2-core/tests/endpoint_protocol.rs` routes every managed-v2
  expectation through `platform_servers` and per-platform `cfg` blocks. Unix
  runs the real managed server through the public path — route registration,
  an `IpcClient`-backed direct call, restart identity, explicit admin
  ping/shutdown on the selected protocol, and a legacy probe that never crosses
  namespaces; nothing on that path is mocked. Windows asserts that a
  managed-v2 derivation, probe, shutdown probe and connect each fail as a
  normalized `LifecycleError::Configuration` naming the unsupported protocol
  and platform, and keeps the legacy Named Pipe on the real serving path.
- Legacy named-pipe coverage is *not* cfg-gated away on Windows: the
  `LegacyV1` serving, restart and cross-protocol-isolation cases execute there.
- `core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-node-loader.test.mjs`
  splits the same way: `win32` asserts the `managed-v2` refusal on both the
  module function and the runtime facade, and only the non-`win32` branch
  compares the two distinct endpoints.
- `sdk/rust/tests/public_api.rs` keeps the facade-reexport assertions
  platform-neutral and matches on the managed-v2 result: `Ok` on Unix, the
  concrete unsupported-platform error on Windows.

## Defects fixed by these tests

The first run of the new core test file failed three assertions; two were real
product defects (described above) and one was the test itself. That third case
was a genuine finding about test isolation rather than product behavior: an
unresolved-protocol assertion read the process environment while a sibling test
in the same binary mutates it, so the check is now serialized under an
environment lock and split into one default-policy test and one
environment-policy test.

Two further defects were fixed after independent Host review rather than by a
test run:

- The managed-v2 expectations were unconditional on Windows, which contradicts
  the production refusal described under "Platform branches". They are now
  platform-branched so Windows executes the negative configuration branch and
  asserts the real classification.
- `test_client_util_keeps_the_historical_call_shapes` probed a fixed, guessable
  address. Because `shutdown` is a real admin call, that name could address a
  developer's running server and stop it. Every probe in that file now derives
  a fresh UUID region per call, so the historical call shapes stay exercisable
  without touching a live service.
- `test_client_util_ping_and_shutdown_accept_only_canonical_protocols`
  asserted a `ValueError` that the SDK itself swallowed: `ping`/`shutdown`
  flattened every non-timeout native `ValueError` into `False` /
  `acknowledged: False`, so a non-canonical protocol was silently reported as
  an absent server. `util.py` now re-raises when the caller supplied an
  explicit `endpoint_protocol`, while an omitted protocol and an invalid
  address keep their historical return shapes. This was found by executing the
  new assertions against the freshly built native module, not by inspection.

## Not executed here

The following could not be executed in this workspace, and no claim is made
about them:

- **Windows.** This workspace is macOS (arm64); nothing on a Windows host was
  run, and this report makes Unix claims only. The Windows behaviour is
  covered by platform branches that execute the real Windows path rather than
  skipping it, but those branches are **not executed here**:
  `c2-config` rejects a `managed-v2` endpoint on Windows before any I/O
  (`ErrorKind::Unsupported`), so every managed-v2 expectation is written as a
  `#[cfg(windows)]` negative-configuration assertion — the exact rejection
  classification, not a skipped test and not a fake success — while legacy
  named-pipe coverage stays on the real serving path. The earlier revision of
  this report wrongly claimed the Windows assertions were merely
  `#[cfg(windows)]`-gated copies of the Unix success path; the unconditional
  `ManagedV2` successes that claim rested on were a real defect and have been
  replaced by the platform branches described above.
- **`pytest` collection of `sdk/python/tests/`.** No workspace `.venv` exists
  and `uv` cannot initialize its cache in this sandbox (`Operation not
  permitted` on `/Users/soku/.cache/uv`), so `pytest` cannot be installed or
  run here. The added cases in
  `sdk/python/tests/unit/test_ipc_address_validation.py` are therefore
  **authored but not executed under pytest**. Their behaviour was executed
  directly instead: the patched `sdk/python/native` crate was compiled
  (`cargo build --manifest-path sdk/python/native/Cargo.toml --lib`), the real
  `libc2_ffi.dylib` was loaded into CPython 3.13 with `DYLD_LIBRARY_PATH`
  pointing at the FastDB staging directory, and every assertion class was run
  against it — one-argument `ipc_endpoint_name`, two-argument and keyword
  forms of `ipc_endpoint_name` / `ipc_ping` / `ipc_shutdown`, the historical
  `ping`/`shutdown` return shapes on fresh UUID regions, the unchanged
  invalid-address shapes, per-call UUID isolation, strict `managed-v2`
  selection, and non-canonical protocol rejection. All passed.
- **TypeScript pack check.** `tests/c2-mem-ffi-pack-check.test.mjs` fails with
  `npm error code EPERM` against `/Users/soku/.npm/_cacache`, a pre-existing
  sandbox npm-cache permission problem unrelated to this change. The other two
  TypeScript test files pass; `node --test tests/c2-mem-ffi-node-loader.test.mjs`
  was re-run after the platform split and reports 15 passed, 0 failed.
- **Full workspace / cold C++ / Python rebuild gates.** Deliberately not run,
  per the budget constraint. The first `cargo check` of the Python native crate
  without the system link mode fails as designed in `fastdb-sys` because it
  would configure CMake inside the cargo git checkout
  (`Operation not permitted`); the documented system link mode
  (`/private/tmp/c2-memory-fastdb-sdk/lib`) was used instead.

## Scope

Changed: the ten quarantined files plus `sdk/rust/src/lib.rs`,
`sdk/rust/Cargo.toml`, `sdk/rust/Cargo.lock`,
`core/runtime/c2-core/tests/endpoint_protocol.rs` (new), `sdk/python/native/...`
necessary test/source, the `c2-http` relay tests, the necessary `c2-mem-ffi`
projection source and TypeScript config types and tests, the Python
`transport/client/util.py` thin kwargs, and the affected lockfile.

The repair revision touched only test and report files plus the two Python
files above: `core/runtime/c2-core/tests/endpoint_protocol.rs`,
`sdk/rust/tests/public_api.rs`,
`core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-node-loader.test.mjs`,
`sdk/python/tests/unit/test_ipc_address_validation.py`,
`sdk/python/src/c_two/transport/client/util.py`, and this report. No production
Rust source, no allocator or pool code, and no other project was changed by it.
`rustfmt` was run on the two modified Rust files only, and both are
`rustfmt --check` clean.

Untouched on purpose: the `owner-runtime` / Host / session surfaces, the v2
cleanup algorithm (a separate goal), `c2-mem` pool and allocator semantics, and
every other project. `c2-local`'s managed-v2 listener support already existed
and was reused unchanged.

`git add` was not run: the worktree index lives outside the allocated checkout
and writing it is denied in this sandbox. The Host integrates this report and
the working-tree changes through its own seal.
