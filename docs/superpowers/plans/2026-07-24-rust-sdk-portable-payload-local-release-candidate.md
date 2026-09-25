# C-Two Rust SDK and Portable-Payload Local Release Candidate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to execute this plan task by task. The active Goal requires the same context-owning primary agent to execute every task inline; do not delegate a task to a subagent.

**Date:** 2026-07-24
**Design authority:** [`../specs/2026-07-24-rust-sdk-portable-payload-local-release-candidate-design.md`](../specs/2026-07-24-rust-sdk-portable-payload-local-release-candidate-design.md)
**Starting C-Two commit:** `edcad96` (`dev-feature`)
**Consumed FastDB freeze:** `6b9d0a55f27bb22fd13f867f321db821f21e777c` (`socu/portable-payload-foundation`)
**Starting Toodle commit:** `8460fbee` (`dev-feature`)
**Local candidate versions:** C-Two Rust crates `0.1.0`, C-Two Python `0.5.1`, CLI `0.1.4`, FastDB Rust/Python `0.1.22`, FastDB TypeScript `0.0.3`
**Completion sentence:** **FastDB/C-Two Phase 0B upstream local release candidate complete**

## Objective

Build the complete local upstream release candidate that Toodle needs before it
can consume C-Two/FastDB: one language-neutral `c2-core`, a real user-facing
Rust SDK named `c-two`, Rust/Python portable-capability parity, bounded contract
admission, transport-neutral generated clients and services, a real 18-row
direct/relay matrix, real Node calls, and isolated package consumers.

This plan does not implement Toodle production integration. It also does not
publish, push, tag, release, select new public versions, or claim hosted CI.

## Global Execution Rules

- [ ] Before every task, retain the corresponding file from
  `.superpowers/sdd/c-two-lrc-task-1-brief.md` through
  `.superpowers/sdd/c-two-lrc-task-12-brief.md`, containing the task scope,
  exact starting commit, design sections, explicit non-goals, RED command,
  GREEN command, broad gates, and commit subject.
- [ ] Use true test-driven development: run the named RED test or package probe
  and retain its expected failure before changing production code.
- [ ] Do not write FastDB parsers, schema models, canonicalizers, object-graph
  logic, or payload engines in C-Two. FastDB Core and its official bindings
  remain the sole payload authority.
- [ ] `c2-core` may carry opaque bytes and generic external-cause fields, but it
  must not depend on the `fastdb` or `fastdb-sys` crates.
- [ ] Rust SDK code may depend on the official `fastdb` crate but must not expose
  any portable capability unavailable to Python.
- [ ] Keep C-Two user-facing Rust naming exact: component “Rust SDK”, path
  `sdk/rust`, Cargo package `c-two`, import `c_two`. Do not introduce `c2-sdk`.
- [ ] Clean-cut `core/runtime/c2-runtime` into `core/runtime/c2-core`; do not
  retain a package, module, alias, feature, re-export, or documentation shim
  named `c2-runtime`/`c2_runtime`.
- [ ] Do not add blanket Clippy suppression. A narrow FFI/generated-code
  allowance must name one lint and carry an adjacent reason.
- [ ] Run large FastDB native, Wasm, Rust, and wheel build families serially.
  Before each family, record `df -h .` and check all three repositories for
  active compiler/build processes.
- [ ] Preserve `../fastdb/fastcarto/fastdb/src/payload/build`; despite its name,
  it is source.
- [ ] Commit one scoped slice after its focused and affected broad gates pass.
  No task starts from an uncommitted previous task.
- [ ] After each implementation task, perform two same-agent passes: first
  specification/owner-boundary review, then correctness/lifetime/portability/
  packaging/maintenance review. Label them as same-agent evidence.
- [ ] Do not change package versions. Do not push, tag, publish, upload, create
  releases, or modify hosted infrastructure.

## Frozen Target API and Ownership Map

The implementation may improve private helpers, but it must preserve these
public responsibilities and names across tasks.

```rust
// c2-contract
pub enum ContractLimitsProfile { V1 }

pub struct ContractLimits {
    profile: ContractLimitsProfile,
    max_source_bytes: u64,
    max_json_values: u64,
    max_nesting_depth: u64,
    max_methods: u64,
    max_nested_fastdb_bytes: u64,
}

impl ContractLimits {
    pub const fn v1() -> Self;
    pub const fn profile(&self) -> ContractLimitsProfile;
    pub const fn with_max_source_bytes(self, limit: u64) -> Self;
    pub const fn with_max_json_values(self, limit: u64) -> Self;
    pub const fn with_max_nesting_depth(self, limit: u64) -> Self;
    pub const fn with_max_methods(self, limit: u64) -> Self;
    pub const fn with_max_nested_fastdb_bytes(self, limit: u64) -> Self;
}

impl ContractRelease {
    pub fn from_descriptor_json_with_limits(
        bytes: &[u8],
        limits: ContractLimits,
    ) -> Result<Self, ContractError>;

    pub fn descriptor(&self) -> &ValidatedContractDescriptor;
}

impl ContractReleaseRef {
    pub fn from_json_with_limits(
        bytes: &[u8],
        limits: ContractLimits,
    ) -> Result<Self, ContractError>;
}

pub fn derive_contract_fingerprints_json_with_limits(
    bytes: &[u8],
    limits: ContractLimits,
) -> Result<ContractFingerprints, ContractError>;

pub fn contract_descriptor_sha256_hex_with_limits(
    bytes: &[u8],
    limits: ContractLimits,
) -> Result<String, ContractError>;

// c2-core, re-exported by c-two
pub struct Runtime;
pub struct RuntimeOptions;

pub enum Connect {
    DirectIpc { address: String },
    ExplicitRelay { relay_url: String },
    RelayAware,
}

pub struct Client;
pub enum ObservedPath {
    DirectIpc,
    ExplicitRelay,
    RelayAwareLocalIpc,
    RelayAwareRelay,
}

pub struct ObservedRoute {
    pub route_uid: String,
    pub route_revision: u64,
}

impl Client {
    pub fn observed_route(&self) -> &ObservedRoute;
}

pub enum Error {
    Semantic(c2_error::C2Error),
    Contract(c2_contract::ContractError),
    Admission(c2_contract::ContractError),
    Transport(TransportError),
    Lifecycle(LifecycleError),
}

impl Runtime {
    pub fn connect(
        &self,
        expected: ExpectedRouteContract,
        mode: Connect,
    ) -> Result<Client, Error>;

    pub fn host(&self, options: HostOptions) -> Result<Host, Error>;
}

// The supertrait is private, so downstream code can use but cannot implement it.
#[doc(hidden)]
pub trait EncodedClient: private::Sealed {
    fn call_owned(&self, method: &str, request: &[u8])
        -> Result<Vec<u8>, Error>;
    fn call_held(&self, method: &str, request: &[u8])
        -> Result<HeldResponse, Error>;
}

pub trait EncodedService: Send + Sync + 'static {
    fn invoke(&self, method_index: u16, request: &[u8])
        -> Result<Vec<u8>, c2_error::C2Error>;
}

pub struct ServiceDefinition;
pub struct Host;
pub struct HostOptions;
pub struct Registration;
pub struct RegisterOutcome {
    pub route_name: String,
    pub route_uid: String,
    pub route_revision: u64,
    // Existing host and IPC facts remain unchanged.
}

// c-two Rust SDK
pub struct Held<T> {
    value: Option<T>,
    response: Option<c2_core::HeldResponse>,
    released: bool,
}
```

The generated Rust client owns a concrete `c_two::Client`. Generated code may
call the sealed encoded boundary, but cannot implement an arbitrary transport.
The generated service adapter alone opens/encodes official FastDB payloads and
implements `c_two::EncodedService`. `c2-core` sees only method indices, route
facts, opaque bytes, semantic errors, and lease callbacks.

Existing C2 error codes remain authoritative. Generated adapters use this exact
phase mapping:

| Failure phase | Existing `ErrorCode` |
| --- | --- |
| Client FastDB input digest/build/encode | `ClientInputSerializing` |
| Client response-lease byte extraction | `ClientOutputFromBuffer` |
| Client FastDB output open/digest/decode | `ClientOutputDeserializing` |
| Resource request-lease byte extraction | `ResourceInputFromBuffer` |
| Resource FastDB input open/digest/decode | `ResourceInputDeserializing` |
| User service failure without an existing semantic error | `ResourceFunctionExecuting` |
| Resource FastDB output digest/build/encode | `ResourceOutputSerializing` |

An existing `Error::Semantic(C2Error)` passes through unchanged. Route-state
and contract errors retain their existing 701–714 registry meanings. Local
contract/admission/transport/lifecycle variants are not serialized as a new
wire.

The low-level lifetime split is:

```rust
// c2-server
pub struct RequestLease(Option<RequestData>);
impl RequestLease {
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String>;
    pub fn release(&mut self);
}

// c2-ipc
pub struct ResponseLease {
    response: Option<ResponseData>,
    server_pool: Arc<Mutex<Option<ServerPoolState>>>,
    reassembly_pool: Arc<RwLock<MemPool>>,
}
impl ResponseLease {
    pub fn copy_bytes(&self) -> Result<Vec<u8>, String>;
    pub fn release(&mut self) -> Result<(), String>;
    pub fn into_owned_bytes(self) -> Result<Vec<u8>, String>;
}

// c2-core
pub struct HeldResponse { /* copied bytes plus live response lease */ }
impl HeldResponse {
    pub fn bytes(&self) -> &[u8];
    pub fn invalidate_then_release<F>(
        &mut self,
        invalidate: F,
    ) -> Result<(), LifecycleError>
    where
        F: FnOnce() -> Result<(), String>;
}
```

`RequestLease` releases on `Drop`. The generated FastDB borrowed-input guard is
created inside `EncodedService::invoke`, so it invalidates while unwinding or
returning before the outer Core callback drops `RequestLease`. For held
responses, `invalidate_then_release` always attempts the transport release
even when invalidation fails and returns a typed combined lifecycle error.

## Task 1: Add One Bounded Contract Admission Authority

**Design sections:** 10, 12, 17
**Commit:** `feat: bound contract admission`

**Create:**

- `core/foundation/c2-contract/src/admission.rs`
- `core/foundation/c2-contract/tests/admission.rs`

**Modify:**

- `core/foundation/c2-contract/Cargo.toml`
- `core/foundation/c2-contract/src/lib.rs`
- `core/foundation/c2-contract/src/descriptor.rs`
- `core/foundation/c2-contract/src/release.rs`
- `core/foundation/c2-contract/tests/descriptor.rs`
- `core/foundation/c2-contract/tests/release.rs`
- `core/protocol/c2-wire/src/handshake.rs`
- `core/protocol/c2-wire/src/tests.rs`
- `core/foundation/c2-codegen/src/compile.rs`
- `core/foundation/c2-codegen/src/lib.rs`
- `core/foundation/c2-codegen/tests/composition.rs`
- `cli/src/contract.rs`
- `cli/tests/contract_commands.rs`
- `sdk/python/native/src/wire_ffi.rs`
- `sdk/python/native/src/codegen_ffi.rs`
- `sdk/python/tests/unit/test_contract_codegen.py`
- `sdk/python/tests/unit/test_wire.py`

### Task 1.1 — Write the hostile RED corpus

- [x] Add exact-boundary tests for source bytes `16 MiB`, values `1_000_000`,
  root depth `1`, maximum depth `128`, methods `256`, and cumulative nested
  FastDB extraction bytes `16 MiB`.
- [x] Add one-over tests for every metric. Assert the exact
  `profile`, `metric`, `limit`, `observed`, and JSON path.
- [x] Count the root and every object value/array element; do not count object
  keys. Include arrays and objects that distinguish those rules.
- [x] Count identical nested FastDB bindings once per occurrence even though
  codegen later deduplicates by digest.
- [x] Test checked-add overflow and a `u64` policy limit that cannot be
  represented as a Rust allocation length (including `usize` conversion on
  narrower targets).
- [x] Feed a deeply nested hostile descriptor and prove rejection occurs in the
  bounded visitor, not after a `serde_json::Value` has been built.
- [x] Apply source/value/depth limits to `ContractReleaseRef`; assert method and
  nested-byte metrics are not consulted for a release reference.
- [x] Add an instrumentation-only parser counter proving CLI validate,
  release-ref, codegen, and Python codegen each admit descriptor bytes once.

Run RED:

```bash
cargo test --manifest-path core/foundation/c2-contract/Cargo.toml --test admission -- --nocapture
```

Expected RED: `ContractLimits`, `LimitExceeded`, and the bounded constructors do
not exist; the hostile-depth probe reaches the old unbounded parse.

### Task 1.2 — Implement the bounded visitor

- [x] Define the single default table in `admission.rs`:

```rust
pub const MAX_CONTRACT_METHODS: usize = 256;

impl Default for ContractLimits {
    fn default() -> Self {
        Self::v1()
    }
}
```

- [x] Add the direct `serde = "1"` dependency required by
  `DeserializeSeed`/`Visitor`; do not depend on a second JSON library.
- [x] Implement a `serde::de::DeserializeSeed`/`Visitor` that checks source size
  before parsing, increments the value count before constructing each value,
  carries root depth `1`, and derives array/object child paths.
- [x] Store limits as checked `u64` policy values and validate every conversion
  before parsing. Use checked addition for values and nested-byte totals.
- [x] Add:

```rust
ContractError::LimitExceeded {
    profile: ContractLimitsProfile,
    metric: ContractLimitMetric,
    limit: u64,
    observed: u64,
    path: String,
}
```

- [x] Reject `methods.len()` before allocating the method result vector or
  iterating method descriptors.
- [x] Canonicalize each opaque nested FastDB JSON value, add its byte length for
  every binding occurrence, and check the cumulative total before returning
  `NestedFastDbSpec`.
- [x] Make `ValidatedContractDescriptor::from_json` and
  `ContractRelease::from_descriptor_json` delegate to V1 bounded admission;
  add the explicit `*_with_limits` constructors and `ContractRelease::descriptor`.
- [x] Make `derive_contract_fingerprints_json` and
  `contract_descriptor_sha256_hex` delegate to V1-bounded variants so
  descriptor-authoring/fingerprint workflows cannot retain an unbounded parser.
- [x] Remove the public `validate_portable_contract_descriptor_value(&Value)`
  escape. Internal code that already owns a `Value` must serialize it and enter
  the same bounded byte admission path.
- [x] Make `ContractReleaseRef::from_json(bytes)` delegate to
  `ContractReleaseRef::from_json_with_limits(bytes, ContractLimits::default())`.
- [x] Move the hard method authority from `c2-wire` to
  `c2_contract::MAX_CONTRACT_METHODS`; remove the local `MAX_METHODS = 256`.

### Task 1.3 — Remove duplicate descriptor parsing

- [x] Clean-cut codegen to accept the admitted release:

```rust
pub fn compile_contract_artifacts(
    release: &ContractRelease,
    target: ContractCodegenTarget,
    options: &ContractCodegenOptions,
) -> Result<ContractArtifactSet, CodegenError>;
```

- [x] Use `release.descriptor()` throughout `compile.rs`; do not parse the
  descriptor bytes again.
- [x] In CLI `validate`, `release-ref`, and `codegen`, construct exactly one
  `ContractRelease` and derive digest/reference/artifacts from it.
- [x] In Python native codegen, admit bytes once outside `py.detach`, move the
  admitted release into the detached closure, and project the same typed
  `LimitExceeded` fields.
- [x] Remove or make crate-private any JSON helper that allows CLI/Python/
  codegen to bypass the bounded `ContractRelease` path.

Run focused GREEN:

```bash
cargo test --manifest-path core/foundation/c2-contract/Cargo.toml
cargo test --manifest-path core/protocol/c2-wire/Cargo.toml
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml
cargo test --manifest-path cli/Cargo.toml --test contract_commands
uv run pytest sdk/python/tests/unit/test_contract_codegen.py sdk/python/tests/unit/test_wire.py -q
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo test --manifest-path cli/Cargo.toml --all-features
cargo fmt --manifest-path sdk/python/native/Cargo.toml --all -- --check
uv run pytest sdk/python/tests/unit -q
git diff --check
```

Review before commit:

- [x] Search for every `serde_json::from_slice`/`from_str` that consumes an
  external contract descriptor or release reference and prove it is behind
  bounded admission.
- [x] Search for literal `256` in contract/wire method-capacity code and prove
  only tests/default declaration retain it.
- [x] Prove limit policy is absent from canonical descriptor bytes and digests.

## Task 2: Clean-Cut `c2-runtime` into `c2-core` and Centralize Errors

**Design sections:** 5, 6.1, 7.5, 9, 10, 20.2
**Commit:** `refactor: establish c2 core facade`

**Move:**

- `core/runtime/c2-runtime/Cargo.toml` → `core/runtime/c2-core/Cargo.toml`
- `core/runtime/c2-runtime/src/lib.rs` → `core/runtime/c2-core/src/lib.rs`
- `core/runtime/c2-runtime/src/identity.rs` → `core/runtime/c2-core/src/identity.rs`
- `core/runtime/c2-runtime/src/outcome.rs` → `core/runtime/c2-core/src/outcome.rs`
- `core/runtime/c2-runtime/src/session.rs` → `core/runtime/c2-core/src/session.rs`

**Create:**

- `core/runtime/c2-core/src/error.rs`
- `core/runtime/c2-core/tests/error_normalization.rs`

**Modify:**

- `core/Cargo.toml`
- `core/runtime/c2-core/Cargo.toml`
- `core/runtime/c2-core/src/lib.rs`
- `core/runtime/c2-core/src/session.rs`
- `core/foundation/c2-config/src/identity.rs`
- `core/foundation/c2-config/src/relay.rs`
- `core/foundation/c2-config/src/resolver.rs`
- `sdk/python/native/Cargo.toml`
- `sdk/python/native/src/runtime_session_ffi.rs`
- every `Cargo.lock` changed by the package rename:
  `core/Cargo.lock`, `sdk/python/native/Cargo.lock`

### Task 2.1 — Freeze rename and error RED tests

- [x] Add a repository boundary test that rejects `c2-runtime`, `c2_runtime`,
  `core/runtime/c2-runtime`, and Cargo dependencies on package `c2-runtime`
  outside historical documents.
- [x] Add compile tests for `c2_core::{Runtime, RuntimeOptions, Error}` and a
  failure test proving no `c2_runtime` import exists.
- [x] Add normalization tests for:
  - valid IPC C2E1 bytes → `Error::Semantic(C2Error)`;
  - valid HTTP `C2ErrorEnvelope` → the identical semantic error;
  - malformed semantic bytes/body → `ProtocolViolation`;
  - `ContractError::LimitExceeded` → `Error::Admission`;
  - other `ContractError` → `Error::Contract`;
  - pre-dispatch transport failure versus dispatch-uncertain transport failure.
- [x] Assert FastDB outer-cause field names are emitted by one Core helper:
  `cause_owner`, `fastdb_code`, `fastdb_symbol`, `fastdb_path`,
  `fastdb_message`, and `fastdb_details_json`.
- [x] Assert every generated client/resource phase uses the frozen error-code
  mapping above and an existing semantic error passes through unchanged.

Run RED:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-core --test error_normalization -- --nocapture
```

Expected RED: package `c2-core` and the normalized error facade do not exist.

### Task 2.2 — Perform the clean cut

- [x] Use `git mv` for the runtime directory and sources.
- [x] Change package/import identity exactly:

```toml
[package]
name = "c2-core"
version.workspace = true
edition.workspace = true

[lib]
name = "c2_core"
```

- [x] Rename `RuntimeSession` → `Runtime` and `RuntimeSessionOptions` →
  `RuntimeOptions` in the Rust authority. Keep the PyO3 class name
  `RuntimeSession` temporarily as a Python-language projection, not a Rust
  package/type alias.
- [x] Add direct dependencies on `c2-error`, `c2-ipc`, and the already-owned
  HTTP/server crates to `c2-core`; do not add FastDB.
- [x] Move HTTP semantic-envelope parsing from
  `sdk/python/native/src/http_ffi.rs` into `c2-core::error`.
- [x] Define typed `TransportPhase::{PreDispatch, DispatchUncertain}` and make
  fallback eligibility depend on that phase, never on string matching.
- [x] Implement one generic external-cause-to-C2-details function in Core. It
  accepts already-extracted owner/code/symbol/path/message/details and does not
  inspect a FastDB object.
- [x] Update the Python native manifest/import so it compiles against
  `c2-core`; do not migrate its duplicate orchestration until Task 7.

### Task 2.3 — Close the known Core strict-Clippy baseline

- [x] Replace two needless `as_bytes().len()` calls in
  `c2-config/src/identity.rs` and `relay.rs`.
- [x] Initialize server/client IPC configs with struct update syntax in
  `c2-config/src/resolver.rs` rather than assigning fields after `Default`.
- [x] Do not add `#[allow]`.

> Implementation note (2026-07-24): the initial four-warning inventory was
> incomplete. Once the earlier blockers were removed, Rust/Clippy 1.91 exposed
> the historical strict-warning backlog across `c2-mem`, `c2-config`,
> `c2-codegen`, `c2-wire`, `c2-ipc`, `c2-server`, `c2-http`, and `c2-core`.
> Task 2 restored the actual full-Core `-D warnings` baseline without
> `#[allow]` or `#[expect]` suppression.

Run focused GREEN:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-core --all-features
cargo test --manifest-path core/Cargo.toml -p c2-error --all-features
cargo test --manifest-path core/Cargo.toml -p c2-config --all-features
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo check --manifest-path sdk/python/native/Cargo.toml --all-targets
rg -n 'c2-runtime|c2_runtime|core/runtime/c2-runtime' \
  core cli sdk README.md AGENTS.md docs/roadmap.md docs/issues
git diff --check
```

The `rg` command must return only retained historical quotations explicitly
marked historical. Current manifests, code, README, AGENTS, roadmap, and Issue
guidance must return no match.

## Task 3: Make Request and Response Lease Ordering Explicit

**Design sections:** 8, 9, 17
**Commit:** `feat: centralize transport lease ordering`

**Create:**

- `core/runtime/c2-core/src/lifetime.rs`
- `core/runtime/c2-core/tests/lifetime_ordering.rs`

**Modify:**

- `core/runtime/c2-core/src/lib.rs`
- `core/transport/c2-server/src/dispatcher.rs`
- `core/transport/c2-server/src/lib.rs`
- `core/transport/c2-ipc/src/client.rs`
- `core/transport/c2-ipc/src/response.rs`
- `core/transport/c2-ipc/src/sync_client.rs`
- `core/transport/c2-ipc/src/lib.rs`
- `core/transport/c2-ipc/src/tests.rs`

### Task 3.1 — Write ordering and failure-path RED tests

- [x] Use an event log to assert borrowed input order:
  `callback-enter`, `fastdb-invalidate`, `callback-exit`,
  `request-lease-release`.
- [x] Repeat the assertion for normal return, adapter error, early return, and
  unwind through an SDK-owned invalidation guard.
- [x] Assert checked views/clones sharing the owner fail after invalidation while
  a detached materialized value remains valid.
- [x] Assert owned response order:
  `response-copy`, `response-lease-release`, `adapter-open-copy`, `return`.
- [x] Assert held response explicit `release()` and `Drop` both perform
  `fastdb-invalidate` before `response-lease-release`, exactly once.
- [x] Inject invalidation failure and response-release failure independently and
  together. Both actions must be attempted once; combined failure must retain
  both causes.
- [x] Exercise inline, buddy SHM, dedicated SHM, and reassembly-handle response
  variants.
- [x] Assert invalid SHM coordinates fail copy without deriving/freeing an
  allocation from unvalidated coordinates.

Run RED:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-core --test lifetime_ordering -- --nocapture
```

Expected RED: leases expose only consuming materialization/cleanup helpers and
cannot retain transport storage across an adapter scope.

### Task 3.2 — Add low-level lease primitives

- [x] Replace direct `RequestData::into_owned_bytes` use in Core-facing paths
  with `RequestLease`.
- [x] `RequestLease::copy_bytes(&self)` validates/copies without release;
  `release` consumes the underlying transport ownership once; `Drop` is the
  fail-safe.
- [x] Split `ServerPoolState::read_and_free` into checked `copy_response` and
  `release_response` operations, then retain `read_and_free` only as a composed
  internal convenience if existing relay code still consumes it atomically.
- [x] Implement `c2_ipc::ResponseLease` from `ResponseData` plus the exact
  server/reassembly pool Arcs. Do not expose raw pointers or coordinate-based
  release to SDK callers.
- [x] For `Inline(Vec<u8>)`, `copy_bytes` clones while `into_owned_bytes` moves.
  For SHM/handle variants, both paths validate before release.
- [x] Add `SyncClient::lease_response(ResponseData) -> ResponseLease`.

### Task 3.3 — Add Core-owned held sequencing

- [x] Implement `c2_core::HeldResponse` with private bytes/lease fields and an
  idempotent state bit.
- [x] `invalidate_then_release` must always run the release after the callback,
  even if the callback returns `Err`.
- [x] `Drop` may perform only best-effort transport cleanup because it has no
  FastDB object; SDK-owned `Held<Payload>` supplies the invalidation callback
  before its Core held response drops.
- [x] Document that unsafe raw pointers obtained outside checked FastDB access
  cannot be revoked.

Run focused GREEN:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-server --all-features
cargo test --manifest-path core/Cargo.toml -p c2-ipc --all-features
cargo test --manifest-path core/Cargo.toml -p c2-core --test lifetime_ordering -- --nocapture
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
git diff --check
```

Review before commit:

- [x] Trace ownership for all `RequestData` and `ResponseData` variants.
- [x] Prove every copy/read failure either retains a valid lease for safe
  cleanup or returns a typed combined cleanup error.
- [x] Prove Core contains no FastDB import, type, parser, or payload meaning.

## Task 4: Build the Single `c2-core` Client, Host, Route, and Retry Facade

**Design sections:** 5, 7, 9, 10, 11
**Commit:** `feat: unify c2 core client and host`

**Create:**

- `core/runtime/c2-core/src/client.rs`
- `core/runtime/c2-core/src/host.rs`
- `core/runtime/c2-core/tests/client_modes.rs`
- `core/runtime/c2-core/tests/host_routes.rs`

**Modify:**

- `core/runtime/c2-core/src/lib.rs`
- `core/runtime/c2-core/src/error.rs`
- `core/runtime/c2-core/src/session.rs`
- `core/runtime/c2-core/src/outcome.rs`
- `core/transport/c2-http/src/client/relay_aware.rs`
- `core/transport/c2-http/src/client/control.rs`
- `core/transport/c2-http/src/client/client.rs`
- `core/transport/c2-ipc/src/pool.rs`
- `core/transport/c2-ipc/src/sync_client.rs`
- `core/transport/c2-server/src/server.rs`
- `core/transport/c2-server/src/dispatcher.rs`

### Task 4.1 — Write connection-mode and retry RED tests

- [x] `DirectIpc` connects only to the supplied address, performs no relay
  resolution, and never falls back.
- [x] `ExplicitRelay` performs contract-scoped resolution and the data-plane
  call through the supplied relay URL even when a valid local IPC route exists.
- [x] `RelayAware` may select a verified local IPC candidate before dispatch;
  otherwise it selects an independent relay path.
- [x] All modes return the same concrete `c2_core::Client` and generated-facing
  `EncodedClient` behavior.
- [x] Record `ObservedPath` and path counters; assert explicit relay cannot be
  reported as direct and relay-aware local/relay selections are distinguishable.
- [x] A failed local candidate whose relay fallback resolves to the same IPC
  path yields semantic `FallbackDenied`.
- [x] `ContractMismatch`, `IdentityMismatch`, and `ProtocolViolation` are
  terminal and perform zero fallback attempts.
- [x] A stale cached route may be discarded and resolved once before dispatch;
  a second stale observation is terminal.
- [x] A service `C2Error` and a dispatch-uncertain connection loss perform zero
  automatic replay.
- [x] Route disappearance maps never-resolved, unavailable, removed, closed,
  and stale states to the existing error registry codes. It does not mutate a
  retained `ContractRelease`/`ContractReleaseRef`.

Run RED:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-core --test client_modes -- --nocapture
```

Expected RED: the existing session exposes pieces of relay resolution but no
single connection-mode client, sealed encoded boundary, or centralized retry
state machine.

### Task 4.2 — Implement the client facade

- [x] Define `Connect`, `Client`, `ObservedPath`, and the sealed
  `EncodedClient` exactly at the frozen boundary.
- [x] Keep low-level `SyncClient`, `RouteBinding`, `RelayAwareHttpClient`,
  pools, route tokens, and HTTP request objects private to `c2-core::client`.
- [x] Acquire/verify route identity before returning `Client`; generated code
  receives no separate binding.
- [x] Expose read-only `Client::expected_route()` and `Client::observed_path()`
  facts for generated verification and receipts; expose no mutable binding or
  transport handle.
- [x] Route `call_owned` through `ResponseLease::into_owned_bytes`.
- [x] Route `call_held` through `ResponseLease::copy_bytes` and return
  `HeldResponse` retaining the response lease.
- [x] Decode IPC C2E1 and HTTP error envelopes through `c2_core::error`, with
  identical `C2Error` code/name/message/details.
- [x] Represent “request may have been dispatched” explicitly in the low-level
  error returned to Core. Do not infer it from text or socket error kind.
- [x] Move global IPC/HTTP pool acquisition, configuration freeze, release, and
  shutdown ordering behind `Runtime`; do not expose a second process singleton
  in the Rust SDK.

### Task 4.3 — Implement the host facade

- [x] Define:

```rust
pub struct MethodDefinition {
    pub index: u16,
    pub name: &'static str,
    pub access: MethodAccess,
}

pub struct ServiceDefinition {
    release_ref: ContractReleaseRef,
    expected: ExpectedRouteContract,
    methods: Arc<[MethodDefinition]>,
    service: Arc<dyn EncodedService>,
}
```

- [x] Validate method indices/names against the admitted release before building
  a low-level `RouteBuildSpec`.
- [x] `Host::register` owns server creation, route construction, local
  registration, optional relay projection, rollback, and a returned
  `Registration`.
- [x] `Registration::close` and `Drop` are idempotent. Explicit close returns
  typed local/relay cleanup outcomes; Drop performs best-effort cleanup.
- [x] The Core callback wraps `RequestData` in `RequestLease`, copies bytes
  without release, calls `EncodedService::invoke`, and only then drops/releases
  the request lease.
- [x] Translate `C2Error` from an encoded service into existing C2E1 user-error
  bytes; malformed adapter behavior becomes `ProtocolViolation`.
- [x] Do not let callers construct a `c2-server` dispatcher, pool, route
  catalog, or IPC callback.

Run focused GREEN:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-core --test client_modes -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-core --test host_routes -- --nocapture
cargo test --manifest-path core/Cargo.toml -p c2-http --all-features
cargo test --manifest-path core/Cargo.toml -p c2-ipc --all-features
cargo test --manifest-path core/Cargo.toml -p c2-server --all-features
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
git diff --check
```

Review before commit:

- [x] Draw every pre-dispatch and post-dispatch edge from the tests and verify
  only proven pre-dispatch edges are retryable.
- [x] Verify explicit relay calls cross the relay data-plane counter.
- [x] Verify immutable release identity survives all route state transitions.
- [x] Verify no public Core type exposes a low-level transport implementation.

> Staged downstream note (2026-07-24): making the low-level route/server and
> relay orchestration methods crate-private intentionally turns the existing
> Python native implementation into the RED starting point for Task 7. Its
> direct imports of `RuntimeRouteSpec`/`RelayResolvedConnection` and calls to
> the old low-level lifecycle methods no longer compile. Do not reopen those
> Core internals as compatibility surface; Task 7 replaces the duplicate PyO3
> orchestration with `Client`, `Host`, and `Registration`.

## Task 5: Add the User-Facing Rust SDK `c-two`

**Design sections:** 6.2, 7, 8, 9, 10
**Commit:** `feat: add user facing Rust SDK`

**Create:**

- `sdk/rust/Cargo.toml`
- `sdk/rust/Cargo.lock`
- `sdk/rust/src/lib.rs`
- `sdk/rust/src/error.rs`
- `sdk/rust/src/held.rs`
- `sdk/rust/src/payload.rs`
- `sdk/rust/tests/public_api.rs`
- `sdk/rust/tests/payload_lifetime.rs`
- `sdk/rust/examples/client.rs`
- `sdk/rust/examples/host.rs`
- `sdk/rust/README.md`

**Modify:**

- `.gitignore`
- `README.md`
- `AGENTS.md`

### Task 5.1 — Write public API and parity RED tests

- [x] A clean external test crate can import only:

```rust
use c_two::{
    Connect, ContractLimits, ContractRelease, ContractReleaseRef, Error,
    HostOptions, Runtime, RuntimeOptions,
};
```

- [x] Assert `cargo metadata` reports package `c-two`, library target `c_two`,
  version `0.1.0`, and `publish = []`/false.
- [x] Compile-fail direct imports of `c2_ipc`, `c2_http`, `c2_server`,
  `SyncClient`, and `RouteBinding` from the SDK surface.
- [x] Compile-fail an external implementation of the sealed
  `EncodedClient`.
- [x] Exercise direct IPC, explicit relay, and relay-aware clients through the
  same `c_two::Client`.
- [x] Exercise host registration through `c_two::Runtime::host`; user code must
  not assemble a low-level server or callback.
- [x] Assert Rust `Error::Semantic` preserves exactly the same C2 error fields
  as the Python fixture.
- [x] Assert Rust FastDB cause projection writes the six frozen outer keys from
  the official `fastdb::PayloadError`.

Run RED:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --test public_api -- --nocapture
```

Expected RED: `sdk/rust` does not exist.

### Task 5.2 — Implement the thin SDK facade

- [x] Add the exact package manifest:

```toml
[package]
name = "c-two"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
name = "c_two"

[dependencies]
c2-core = { version = "0.1.0", path = "../../core/runtime/c2-core" }
c2-contract = { version = "0.1.0", path = "../../core/foundation/c2-contract" }
c2-error = { version = "0.1.0", path = "../../core/foundation/c2-error" }
fastdb = { version = "0.1.22", path = "../../../fastdb/bindings/rust/fastdb" }
```

- [x] Re-export stable contract/Core types; do not duplicate their state,
  defaults, or error registries.
- [x] Keep the generated-call interface under `c_two::generated` with
  `#[doc(hidden)]`; re-export the sealed Core trait, route/service definition,
  held response, and external-cause projection needed by generated modules.
- [x] Implement a thin FastDB error adapter that only reads official
  `PayloadError` fields and calls the Core external-cause helper.
- [x] Do not re-export FastDB types under a C-Two semantic namespace; generated
  and user code continues to name `fastdb::Payload`.

### Task 5.3 — Implement `Held<Payload>` and borrowed guard

- [x] `Held<fastdb::Payload>` stores the official payload and a Core
  `HeldResponse`; its constructor is crate-private/generated-only.
- [x] `Held::release()` calls Core `invalidate_then_release`, clears both owners,
  and is idempotent.
- [x] `Drop` executes the same order once. Because Drop cannot return an error,
  explicit `release` is the auditable path and Drop is best-effort.
- [x] Add a generated-facing borrowed payload guard whose Drop invalidates the
  FastDB owner. It must be constructed inside the encoded service call so it
  drops before Core releases `RequestLease`.
- [x] Keep all receive paths copy-backed. Do not call or introduce
  `open_external`, direct final backing, or raw transport-pointer ownership.

Run focused GREEN:

```bash
cargo fmt --manifest-path sdk/rust/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/rust/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo run --manifest-path sdk/rust/Cargo.toml --example client
cargo run --manifest-path sdk/rust/Cargo.toml --example host
```

The examples may use an in-process test host but must perform a real route-bound
call. They must not be compile-only examples.

Run affected broad gates:

```bash
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
rg -n 'c2-sdk|c2_sdk|c2-runtime|c2_runtime' sdk/rust core/runtime/c2-core README.md AGENTS.md
git diff --check
```

The boundary scan must return no match.

> Task 5 evidence (2026-07-24): the public API RED test first failed against
> the empty SDK facade, then passed with package `c-two` / library `c_two`.
> Focused SDK format, strict Clippy, all-feature tests, both real route-bound
> examples, Core workspace strict Clippy/tests, the legacy-name scan, and an
> additional SDK implementation scan forbidding transport crates and
> `open_external` all passed.

## Task 6: Generate Transport-Neutral Rust Clients and Typed Rust Services

**Design sections:** 7.3, 7.4, 8, 9, 10.3
**Commit:** `feat: generate Rust SDK clients and services`

**Create:**

- `core/foundation/c2-codegen/tests/generated_rust_sdk.rs`

**Modify:**

- `core/foundation/c2-codegen/src/targets.rs`
- `core/foundation/c2-codegen/src/compile.rs`
- `core/foundation/c2-codegen/tests/generated_targets.rs`
- `core/foundation/c2-codegen/tests/composition.rs`
- `sdk/python/tests/fixtures/portable_interop_rust.rs`
- `sdk/python/tests/integration/test_portable_payload_cross_language.py`
- `sdk/rust/src/lib.rs`
- `sdk/rust/src/payload.rs`

### Task 6.1 — Write generated-source RED assertions

- [x] Generate no-payload, `record.v1`, and `object_graph.v1` Rust trees.
- [x] Assert generated source contains `c_two::Client`,
  `c_two::generated::EncodedClient`, a typed `Service` trait, an adapter, and a
  `ServiceDefinition` factory.
- [x] Assert generated source includes the canonical descriptor bytes and
  derives `ContractRelease`, `ContractReleaseRef`, and expected route facts
  through `c2-contract`; it must not manually assemble an
  `ExpectedRouteContract`.
- [x] Assert it contains no `c2_ipc`, `c2_http`, `c2_server`, `SyncClient`,
  `RouteBinding`, pool, relay branch, or transport address.
- [x] Compile a clean fixture whose direct dependencies are only versioned
  `c-two` and `fastdb`; construct this fixture in a temporary directory inside
  `generated_rust_sdk.rs` so no path-bearing nested Cargo project enters the
  `c2-codegen` package. It implements generated `Service`, registers it, and
  calls the generated client.
- [x] Add a compile failure for a service with the wrong input/output payload
  shape and an executable negative case for a client built from a different
  release-derived expected route. The generated client accepts the
  transport-neutral `c_two::Client` type, so release identity is deliberately
  verified by `ContractClient::new` rather than encoded as a Rust type.
- [x] Preserve current FastDB generation provenance/digest assertions.

Run RED:

```bash
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml --test generated_rust_sdk -- --nocapture
```

Expected RED: generated Rust directly imports `c2_ipc`, stores a
`SyncClient`/`RouteBinding`, and emits no typed host service.

### Task 6.2 — Render the generated client seam

- [x] Render:

```rust
pub struct ContractClient {
    client: c_two::Client,
}

impl ContractClient {
    pub fn new(client: c_two::Client) -> Result<Self, c_two::Error>;
}
```

- [x] Render `CONTRACT_DESCRIPTOR_JSON` with
  `include_str!("../metadata/contract.json")`,
  `contract_release()`, `contract_release_ref()`, and `expected_route()`.
  `expected_route()` must call `ContractRelease::expected_route`.
- [x] `new` verifies the client’s read-only expected route facts against the
  generated release-derived route facts before storing it.
- [x] Generate one owned method for the normal copy-backed return and one
  explicitly named held method only for payload-returning methods.
- [x] Encode inputs with `Payload::require_spec_sha256` and
  `Payload::binary_bytes`; decode outputs with official `CompiledSpec`,
  `Payload::open_copy`, and a post-open digest check.
- [x] For no-payload methods, require exact empty input/output bytes.
- [x] Map FastDB errors through the SDK’s thin external-cause adapter; do not
  invent display-only error strings.
- [x] Use `ClientInputSerializing`, `ClientOutputFromBuffer`, and
  `ClientOutputDeserializing` at their exact frozen phases.

### Task 6.3 — Render the typed service seam

- [x] Generate a trait whose method shapes are exact:

```rust
pub trait Service: Send + Sync + 'static {
    fn no_payload(&self) -> Result<(), c_two::Error>;
    fn record_roundtrip(
        &self,
        input: &fastdb::Payload,
    ) -> Result<fastdb::Payload, c_two::Error>;
}
```

- [x] Generate an adapter implementing `c_two::generated::EncodedService`.
- [x] Decode a payload input into an SDK borrowed guard, invoke the user method,
  encode output, and invalidate the input owner on success, user error, encode
  error, and unwind before returning to Core.
- [x] Convert C-Two semantic errors without changing fields. Convert official
  FastDB failures using the frozen external-cause keys.
- [x] Use `ResourceInputFromBuffer`, `ResourceInputDeserializing`,
  `ResourceFunctionExecuting`, and `ResourceOutputSerializing` at their exact
  frozen phases.
- [x] Generate release/route/method facts and:

```rust
pub fn service_definition<S: Service>(
    route_name: impl Into<String>,
    service: S,
) -> Result<c_two::ServiceDefinition, c_two::Error>;
```

- [x] Ensure method index conversion to `u16` is checked and grounded in the
  contract-owned method cap.

### Task 6.4 — Replace the old low-level interop fixture

- [x] Rewrite `portable_interop_rust.rs` to consume `c_two` and the generated
  module; remove its manual low-level IPC/route/server assembly.
- [x] Preserve all existing record/graph logical assertions rather than reducing
  the fixture to a smoke call.
- [x] Keep the current direct 9-row fixture semantics intact; Task 8 expands
  transport/matrix coverage.

Run focused GREEN:

```bash
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
uv run pytest sdk/python/tests/integration/test_portable_payload_cross_language.py -q
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo fmt --manifest-path sdk/rust/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/rust/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
uv run pytest sdk/python/tests/integration/test_portable_payload_cross_language.py \
  sdk/python/tests/unit/test_contract_codegen.py -q
rg -n 'c2_ipc|c2_http|c2_server|SyncClient|RouteBinding' \
  sdk/python/tests/fixtures/portable_interop_rust.rs
git diff --check
```

The final `rg` must return no generated-consumer/fixture imports. Assertions
that forbid those names may remain in test code.

> Task 6 evidence (2026-07-24): the generated Rust source/compile RED tests
> first exposed the old low-level `c2-ipc` client and missing typed service,
> then passed for no-payload, record, and object-graph contracts. A clean
> generated consumer with only versioned `c-two` and official `fastdb`
> dependencies compiled and ran real direct-IPC client/host calls; the rewritten
> portable record/graph fixture compiled under the same dependency boundary.
> Core and Rust SDK format, strict Clippy, all-feature tests, generated-source
> boundary scans, and `git diff --check` passed. The two Python pytest commands
> still stop before collection while rebuilding the intentionally RED native
> extension recorded after Task 4: it imports now-private Core orchestration
> APIs and has not yet projected the centralized Core error surface. That is the
> Task 7 starting condition, not a reason to reopen Core compatibility APIs;
> the direct Python-driven runtime proof is therefore deferred to Task 7.

## Task 7: Migrate Python Native Orchestration onto `c2-core` and Close Parity

**Design sections:** 5, 7.5, 8, 9, 10, 11, 17.1
**Commit:** `refactor: project Python SDK through c2 core`

**Create:**

- `sdk/python/native/src/core_ffi.rs`
- `sdk/python/native/src/core_error_ffi.rs`
- `sdk/python/tests/unit/test_core_capability_parity.py`
- `sdk/python/tests/integration/test_core_transport_parity.py`
- `sdk/python/tests/integration/test_core_lifetime_parity.py`

**Modify:**

- `sdk/python/native/Cargo.toml`
- `sdk/python/native/src/lib.rs`
- `sdk/python/native/src/client_ffi.rs`
- `sdk/python/native/src/http_ffi.rs`
- `sdk/python/native/src/runtime_session_ffi.rs`
- `sdk/python/native/src/server_ffi.rs`
- `sdk/python/native/src/error_ffi.rs`
- `sdk/python/native/src/lease_ffi.rs`
- `sdk/python/native/src/response_backing.rs`
- `sdk/python/native/src/mem_ffi.rs`
- `sdk/python/native/src/shm_buffer.rs`
- `sdk/python/native/src/writable_sink.rs`
- `sdk/python/native/src/wire_ffi.rs`
- `sdk/python/src/c_two/error.py`
- `sdk/python/src/c_two/transport/registry.py`
- `sdk/python/src/c_two/transport/client/proxy.py`
- `sdk/python/src/c_two/transport/server/native.py`
- `sdk/python/src/c_two/transport/input_lifetime.py`
- `sdk/python/tests/unit/test_runtime_session.py`
- `sdk/python/tests/unit/test_error.py`
- `sdk/python/tests/unit/test_held_result.py`
- `sdk/python/tests/unit/test_input_lifetime.py`
- `sdk/python/tests/unit/test_sdk_boundary.py`
- `sdk/python/tests/integration/test_http_relay.py`
- `sdk/python/tests/integration/test_direct_ipc_contract_validation.py`
- `sdk/python/tests/integration/test_error_propagation.py`
- `sdk/python/tests/integration/test_transfer_hold.py`

### Task 7.1 — Write parity and ownership RED tests

- [x] Generate a machine-readable capability table from Rust and Python and
  compare exact support for descriptor/release identity, direct IPC, explicit
  relay, relay-aware selection, typed client/service, no-payload/record/graph,
  owned/held/borrowed lifetime, C2 errors, and FastDB causes.
- [x] Explicitly mark Python pickle/thread-local invocation as Python-only and
  nonportable; do not require Rust to copy it.
- [x] For identical direct/relay failures, assert Rust `Error::Semantic` and
  Python exception expose the same code/name/message/details.
- [x] Assert the six FastDB outer-cause fields are byte-for-byte equal across
  generated Rust and Python adapter failures.
- [x] Assert Python direct, explicit relay, and relay-aware calls are delegated
  through Core path counters, with no Python-native fallback loop.
- [x] Assert owned, held, and borrowed event order matches the Rust evidence,
  including invalidation failure and early-return paths.
- [x] Add source-boundary tests rejecting orchestration imports/identifiers from
  Python native:
  `ClientPool`, `SyncClient`, `RouteBinding`, `RelayAwareHttpClient`,
  `RelayIpcConnectError`, and independent fallback decision functions.

Run RED:

```bash
uv run pytest \
  sdk/python/tests/unit/test_core_capability_parity.py \
  sdk/python/tests/integration/test_core_transport_parity.py \
  sdk/python/tests/integration/test_core_lifetime_parity.py -q
```

Expected RED: Python native still owns its own IPC/HTTP pools, route binding,
relay fallback state machine, error parsing, and release sequencing.

### Task 7.2 — Reduce PyO3 to a language bridge

- [x] Make `RuntimeSession` a PyO3 projection over `Arc<c2_core::Runtime>`.
  Keeping the Python class spelling does not create a Rust compatibility alias.
- [x] Replace separate direct/HTTP/relay-connected inner enums with one
  `c2_core::Client`.
- [x] Retain current high-level Python behavior while routing direct IPC,
  explicit relay, relay-aware selection, calls, registration, close, and
  shutdown through Core.
- [x] Implement a Python callback bridge as `c2_core::EncodedService`. It enters
  Python, invokes the generated/Python adapter, and returns opaque bytes or an
  existing `C2Error`.
- [x] Use Core `HeldResponse::invalidate_then_release` from Python held-result
  release/finalization. Do not retain a separate Python-native ordering state
  machine.
- [x] Remove HTTP C2 envelope parsing, same-path fallback detection, contract/
  identity terminal classification, and IPC/HTTP pool ownership from the
  native binding.
- [x] Remove direct `c2-ipc`, `c2-http`, and `c2-server` normal dependencies
  from `sdk/python/native/Cargo.toml` once no language bridge code needs them.
  `c2-wire`/`c2-mem` may remain only for explicit low-level compatibility
  modules whose source-boundary test proves they do not orchestrate calls.
- [x] Preserve high-level Python public exceptions while sourcing their semantic
  fields and local category from `c2_core::Error`.

### Task 7.3 — Close all 43 current native strict-Clippy findings

- [x] Replace manual `b"B\0"` pointers with C string literals in
  `client_ffi.rs`, `response_backing.rs`, `shm_buffer.rs`, and
  `writable_sink.rs`.
- [x] Use struct update syntax for pool/runtime config initialization.
- [x] Replace redundant `map_err` closures and the needless mutable-slice
  borrow in `mem_ffi.rs`; use `RangeInclusive::contains` for the spill threshold.
- [x] Collapse the nested error-byte cast in `server_ffi.rs`.
- [x] Box/remove the current large relay-connected variants by replacing them
  with `c2_core::Client`; do not merely silence `large_enum_variant` or
  `result_large_err`.
- [x] Introduce a named decoded-call-control struct/type instead of the complex
  return tuple in `wire_ffi.rs`.
- [x] For PyO3 constructors/methods whose Python-call signature legitimately
  exceeds seven arguments, place a narrow
  `#[allow(clippy::too_many_arguments)]` on that one FFI function with the
  adjacent reason “PyO3 signature is the existing Python call boundary”.
  Do not apply module/crate-wide allows.
- [x] Remove any finding whose code disappeared during Core migration rather
  than preserving dead wrappers to make lint cleanup mechanical.

Run focused GREEN:

```bash
cargo fmt --manifest-path sdk/python/native/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/python/native/Cargo.toml --all-targets --no-deps -- -D warnings
uv run pytest \
  sdk/python/tests/unit/test_core_capability_parity.py \
  sdk/python/tests/integration/test_core_transport_parity.py \
  sdk/python/tests/integration/test_core_lifetime_parity.py -q
```

Run affected broad gates:

```bash
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo check --manifest-path sdk/python/native/Cargo.toml --all-targets
cargo clippy --manifest-path sdk/python/native/Cargo.toml --all-targets --no-deps -- -D warnings
uv run pytest sdk/python/tests -q
python3.10 -m compileall -q sdk/python/src
rg -n 'ClientPool|SyncClient|RouteBinding|RelayAwareHttpClient|RelayIpcConnectError' \
  sdk/python/native/src
git diff --check
```

The boundary scan may match only explicit low-level compatibility type names
inside tests/docs that assert their absence. Production native source must not
own them.

Review before commit:

- [x] Compare the generated Rust/Python capability receipt row by row.
- [x] Trace every Python finalizer and explicit release through Core once.
- [x] Prove all route selection/retry decisions are Core calls, not duplicated
  Python-native branches.

Observed Task 7 evidence:

- The focused Core parity suite passes with 7 tests. Rust and Python consume
  the same frozen six-field FastDB digest-mismatch vector, and Python compares
  direct IPC, explicit relay, and relay-aware semantic errors field for field
  while Core path counters prove the selected transports.
- The Python native extension passes format, check, and strict Clippy with
  `-D warnings`; the Rust `c-two` SDK passes format, strict Clippy, 10 tests,
  and doc tests; Python 3.10 and the project Python both compile the SDK source.
- The first complete Python rerun exposed a stale pooled IPC client after a
  Host restart at the same socket address. TDD added exact-client
  `release_if_same` / `discard_if_same` ownership in `c2-ipc` and one
  Core-owned pre-dispatch reconnect, so a late old-client drop cannot decrement
  a racing replacement. The zero-copy IPC file then passed 10 consecutive
  runs (70 tests), followed by the complete Python suite with 828 passed and
  1 skipped.
- A subsequent Core workspace rerun exposed test-only process relay
  environment interference. Standalone session tests now explicitly disable
  process relay discovery. The affected Core lib passed 5 consecutive runs,
  then the full Core workspace tests and workspace strict-Clippy gate passed.

## Task 8: Implement and Execute the Exact 18-Row Portable Matrix

**Design sections:** 13.4, 15, 17
**Commit:** `test: cover portable payload direct relay matrix`

**Create:**

- `.superpowers/sdd/c-two-lrc-task-8-brief.md` (ignored local execution evidence)
- `tools/local_rc/__init__.py`
- `sdk/python/tests/fixtures/portable_matrix.py`
- `sdk/python/tests/fixtures/portable_matrix_rust.rs`
- `sdk/python/tests/integration/test_portable_payload_matrix.py`
- `tools/local_rc/portable_matrix_receipt.py`
- `tests/repo/test_portable_matrix_receipt.py`
- `tests/fixtures/contracts/portable-no-payload.contract.json`
- `tests/fixtures/contracts/portable-record-v1.contract.json`
- `tests/fixtures/contracts/portable-object-graph-v1.contract.json`

**Modify:**

- `AGENTS.md`
- `core/runtime/c2-core/src/client.rs`
- `core/runtime/c2-core/src/lib.rs`
- `core/runtime/c2-core/src/outcome.rs`
- `core/runtime/c2-core/src/session.rs`
- `core/runtime/c2-core/tests/client_modes.rs`
- `core/transport/c2-http/src/client/relay_aware.rs`
- `sdk/rust/src/lib.rs`
- `sdk/rust/tests/public_api.rs`
- `sdk/python/native/src/core_ffi.rs`
- `sdk/python/native/src/runtime_session_ffi.rs`
- `sdk/python/src/c_two/transport/client/proxy.py`
- `sdk/python/src/c_two/transport/registry.py`
- `sdk/python/tests/fixtures/portable_interop.py`
- `sdk/python/tests/integration/test_core_transport_parity.py`
- `sdk/python/tests/integration/test_direct_ipc_contract_validation.py`
- `sdk/python/tests/integration/test_portable_payload_cross_language.py`
- `sdk/python/tests/integration/test_registry.py`
- `cli/tests/contract_commands.rs`
- `docs/issues/contract-release-deferred-capabilities.md`
- `docs/superpowers/plans/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md`

### Task 8.1 — Freeze the row set and receipt validator

- [x] Encode exactly these dimensions:

```python
PAYLOADS = ("no-payload", "record-v1", "object-graph-v1")
DIRECTIONS = (
    "rust-client__rust-host",
    "rust-client__python-host",
    "python-client__rust-host",
)
TRANSPORTS = ("direct", "relay")
```

- [x] Derive exactly 18 stable IDs with
  `f"{payload}__{direction}__{transport}"`.
- [x] Pin the exact ordered set:

```python
EXPECTED_ROW_IDS = (
    "no-payload__rust-client__rust-host__direct",
    "no-payload__rust-client__rust-host__relay",
    "no-payload__rust-client__python-host__direct",
    "no-payload__rust-client__python-host__relay",
    "no-payload__python-client__rust-host__direct",
    "no-payload__python-client__rust-host__relay",
    "record-v1__rust-client__rust-host__direct",
    "record-v1__rust-client__rust-host__relay",
    "record-v1__rust-client__python-host__direct",
    "record-v1__rust-client__python-host__relay",
    "record-v1__python-client__rust-host__direct",
    "record-v1__python-client__rust-host__relay",
    "object-graph-v1__rust-client__rust-host__direct",
    "object-graph-v1__rust-client__rust-host__relay",
    "object-graph-v1__rust-client__python-host__direct",
    "object-graph-v1__rust-client__python-host__relay",
    "object-graph-v1__python-client__rust-host__direct",
    "object-graph-v1__python-client__rust-host__relay",
)
```
- [x] Make `test_portable_matrix_receipt.py` reject a missing, duplicate,
  skipped, xfailed, unexpected, or non-passing row.
- [x] Require each row object to contain:
  row ID, both languages, transport, observed path, descriptor SHA-256,
  complete `ContractReleaseRef`, FastDB spec SHA-256 (or explicit null for
  no-payload), route UID/revision, logical result hash, client/host package
  hashes, c3 hash for relay, and status.
- [x] Reject a relay row unless observed path is `ExplicitRelay` and its
  request/response path counters are positive.
- [x] Reject absolute paths, duplicate JSON keys, unknown fields, and unstable
  row order.

Run RED:

```bash
uv run pytest tests/repo/test_portable_matrix_receipt.py -q
```

Expected RED: no exact row-set/receipt implementation exists.

### Task 8.2 — Build one real harness for all rows

- [x] Generate Rust and Python contract trees through the same `c3 contract
  codegen` path from the same descriptor bytes.
- [x] Build the Rust client/host fixture against `c-two` and generated code,
  never low-level C-Two crates.
- [x] Run Python rows through the installed/importable public `c_two` package,
  never direct native FFI orchestration.
- [x] For every relay row, launch the real local `c3` binary:

```bash
export C2_LRC_RELAY_PORT="$(uv run python -c \
  'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
c3 relay \
  --bind "127.0.0.1:${C2_LRC_RELAY_PORT}" \
  --relay-id matrix-relay \
  --advertise-url "http://127.0.0.1:${C2_LRC_RELAY_PORT}"
```

- [x] Register the route through the host’s Core facade, wait for
  contract-scoped resolution, perform the call through
  `Connect::ExplicitRelay`, then shut relay/host down and assert no child
  survives.
- [x] Reuse identical input and logical expected-result fixtures across direct
  and relay for each payload/direction.
- [x] Cover record `str`, `wstr`, bytes, nested list, null, and empty values.
- [x] Cover graph shared references, self-cycle, and mutual cycle using
  official FastDB graph APIs.
- [x] Hash a canonical logical-result receipt rather than serializing a C-Two
  invented graph format.

### Task 8.3 — Add targeted negative and lifetime matrix evidence

- [x] Test route mismatch, contract fingerprint mismatch, FastDB digest
  mismatch, and route disappearance over direct and relay where transport
  changes the boundary.
- [x] Test Rust/Python error parity where language changes projection.
- [x] Verify retained `ContractReleaseRef` after route disappearance.
- [x] Record borrowed invalidation, held invalidation-before-release, owned
  response release, and detached-materialization survival evidence.
- [x] No negative test is multiplied mechanically across all 18 positive rows;
  each test states which transport/language boundary it proves.

### Task 8.4 — Execute the development matrix

Run focused GREEN:

```bash
uv run pytest \
  sdk/python/tests/integration/test_portable_payload_matrix.py \
  tests/repo/test_portable_matrix_receipt.py -q -vv
```

At this task, package-hash fields may be populated from locally built
source-installed artifacts and the receipt must be labeled
`evidence_stage=development`. It is not the final candidate receipt. Task 11
reruns the same harness only from isolated package artifacts and writes the
retained candidate receipt.

Run affected broad gates:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo test --manifest-path cli/Cargo.toml --all-features
uv run pytest \
  sdk/python/tests/integration/test_portable_payload_cross_language.py \
  sdk/python/tests/integration/test_portable_payload_matrix.py -q
uv run pytest tests/repo/test_portable_matrix_receipt.py -q
git diff --check
```

Review before commit:

- [x] Independently enumerate the Cartesian product and compare all 18 IDs.
- [x] Inspect process/path counters for every relay row.
- [x] Verify no skip/xfail branch exists in the matrix harness.
- [x] Verify every child process, socket, SHM allocation, and temp directory is
  cleaned on success and failure.

Observed Task 8 evidence:

- The receipt-validator RED failed at collection because
  `tools.local_rc.portable_matrix_receipt` did not exist. The route-observation
  RED then failed compilation because `RegisterOutcome` did not expose the
  registered token and `Client` did not provide `observed_route()`.
- The focused matrix/validator run passes 50 tests: 18 exact positive rows, one
  complete-receipt assertion, five targeted negative/lifetime cases, and 26
  strict receipt-validator tests. An independent shell enumeration matches the
  receipt order byte for byte.
- A combined cross-language run exposed that relay catalog resolution can
  precede lazy data-plane-client readiness. The harness now polls the same
  contract- and token-scoped `/_probe` used by explicit-relay connection before
  making the one non-replayed business call. The final cross-language,
  18-row, and receipt suite passes 51 tests.
- The first complete Python rerun exposed seven stale tests that expected a
  leaked native `RuntimeError` for direct contract mismatch. Direct IPC now
  decodes the Core error envelope like explicit relay and relay-aware paths;
  the tests assert public `ContractMismatch`. The complete Python suite then
  passes 853 tests.
- Core format, workspace strict Clippy, and the complete Core workspace tests
  pass. CLI format, strict Clippy, and 45 tests pass. The `c-two` Rust SDK
  format, strict Clippy, 10 tests, and doc tests pass. Python native format and
  strict Clippy pass.
- The development receipt at
  `target/local-rc/portable-matrix-receipt.v1.json` has SHA-256
  `1031cb4a062fe2931842030cd0819644cbb93deff6512b860ea9571e59948146`,
  contains 18 passing rows split 9 direct/9 explicit relay, and every relay
  row records positive request/response evidence. Its local linked/source
  package hashes are deliberately not candidate-package evidence; Task 11
  replaces this ephemeral receipt.
- Same-agent specification/ownership review confirmed that Core remains
  payload-neutral, the Rust harness imports only `c-two`, generated modules,
  and official FastDB, Python does not orchestrate through `_native`, all relay
  rows start real `c3`, and FastDB remains clean at
  `6b9d0a55f27bb22fd13f867f321db821f21e777c`.
- Same-agent correctness/lifetime/portability/maintenance review confirmed the
  exact row set, connect-time route-token semantics, invalidation/release
  ordering, materialization survival, canonical error projection, absence of
  skip/xfail/opt-in branches, deterministic receipt encoding, and zero
  surviving matrix/relay/build processes or matrix socket/temp residue.

## Task 9: Execute Generated TypeScript Against Real Hosts

**Design sections:** 14.3, 16, 17
**Commit:** `test: execute generated TypeScript real calls`

**Create:**

- `sdk/python/tests/fixtures/typescript_real_call.mjs`
- `sdk/python/tests/integration/test_typescript_real_calls.py`
- `core/foundation/c2-codegen/tests/typescript_transport_contract.rs`
- `tools/local_rc/typescript_receipt.py`
- `tests/repo/test_typescript_receipt.py`

**Modify:**

- `core/foundation/c2-codegen/assets/typescript_transport.ts`
- `core/foundation/c2-codegen/src/targets.rs`
- `core/foundation/c2-codegen/tests/generated_targets.rs`
- `core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts`
- `core/foundation/c2-mem-ffi/bindings/typescript/package.json`
- `core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-binding.test.mjs`
- `core/foundation/c2-mem-ffi/bindings/typescript/tests/c2-mem-ffi-node-loader.test.mjs`

### Task 9.1 — Write transport-contract RED tests

- [x] Assert generated Node code declares and executes direct IPC, explicit
  relay, and relay-aware modes.
- [x] Assert direct IPC reaches a real route-bound host.
- [x] Assert explicit relay reaches a real `c3 relay` data-plane counter and
  cannot shortcut to local IPC.
- [x] Assert relay-aware records either verified local IPC or HTTP relay
  selection and denies same-path fallback.
- [x] Use at least one generated Rust host and one generated Python host across
  the TypeScript suite.
- [x] Reject a test double/simulated transport as evidence.

Run RED:

```bash
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml \
  --test typescript_transport_contract -- --nocapture
uv run pytest sdk/python/tests/integration/test_typescript_real_calls.py -q
```

Expected RED: generated TypeScript is only compile/typecheck and simulated
lifecycle evidence; real host calls and path receipts are absent.

### Task 9.2 — Close real payload and ownership behavior

- [x] Generate all TypeScript artifacts through `c3` from the same no-payload,
  record, and graph descriptors used by the matrix.
- [x] Load payload semantics only through the public installed
  `fastdb4ts/payload` API.
- [x] Execute no-payload, record, and graph calls over all three declared
  connection modes. Spread Rust/Python hosts across the suite so both execute.
- [x] Prove wrong FastDB digest is structurally rejected with the frozen outer
  cause fields.
- [x] Prove response close/release is idempotent, checked views fail after
  invalidation, and materialized values survive.
- [x] Feed an opaque response allocator without a public byte-visible view;
  assert C-Two rejects and releases it instead of reading provider-private
  fields.
- [x] Record Node version, platform, package hashes, descriptor/release/spec
  hashes, observed path, route facts, logical result, and cleanup status.
- [x] Mark browser runtime explicitly unverified; do not add browser support
  wording to current docs.

### Task 9.3 — Run source-stage real calls

Run focused GREEN:

```bash
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript test
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript run typecheck
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml \
  --test typescript_transport_contract
uv run pytest sdk/python/tests/integration/test_typescript_real_calls.py -q -vv
uv run pytest tests/repo/test_typescript_receipt.py -q
```

This is development evidence. Task 11 reruns the same suite in a clean project
installed only from npm tarballs and retains the candidate receipt.

Run affected broad gates:

```bash
cargo test --manifest-path core/foundation/c2-codegen/Cargo.toml
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript run pack:check
uv run pytest \
  sdk/python/tests/integration/test_typescript_real_calls.py \
  sdk/python/tests/unit/test_contract_codegen.py -q
git diff --check
```

Review before commit:

- [x] Inspect counters proving relay calls traversed relay and local-aware calls
  traversed IPC.
- [x] Verify installed FastDB public methods, not private fields, own all
  payload behavior.
- [x] Verify receipt claims Node on the audited platform only.

Observed Task 9 evidence:

- The initial transport-contract RED found that generated TypeScript had no
  auditable path observations, route-token-bound HTTP preparation, verified
  relay-aware local facts, real Node host calls, or strict receipt. A later
  controlled RED also proved that explicit relay incorrectly sent probe/call
  traffic to the resolution anchor and that canonical pre-dispatch errors were
  not recognized.
- The final controlled transport suite passes five tests. It compiles generated
  TypeScript with the audited compiler, proves resolve-anchor/data-plane
  separation, token-bound probe/call headers, complete local IPC
  identity/token verification, same-route fallback denial, pre-dispatch-only
  route reselection, and one-call behavior for `dispatch_uncertain`.
- The source-stage real-call suite passes 13 tests: all 12 exact rows
  (`no-payload`, `record-v1`, and `object-graph-v1` × direct IPC, explicit
  relay, relay-aware local IPC, and relay-aware HTTP) plus strict receipt
  creation. Every row runs generated code from real `c3`, locally packed
  `fastdb4ts` and `@c-two/c2-mem-ffi`, and a real Rust or Python host. No
  simulated transport contributes receipt evidence.
- The strict receipt validator passes 14 tests and freezes row order, host/path
  assignment, call counts, package and descriptor hashes, release/spec facts,
  the exact six-field FastDB digest cause on
  `record-v1__direct-ipc`, exactly-one opaque allocator release on
  `object-graph-v1__relay-aware-http`, audited Node platform, explicit browser
  `unverified`, canonical JSON, and cleanup.
- The development receipt at
  `target/local-rc/typescript-real-call-receipt.v1.json` has SHA-256
  `abf77a49de3338ef4cd6ecd1ea3fa7d94c286ae7f68ec77c59e73300d30028a6`.
  It records Node `v25.8.1` on `darwin/arm64`,
  `fastdb4ts` tarball SHA-256
  `219fcda8ae71ff97a8ddc0cf11a1edf6c2d7299b5371c32195f5bbd781080a9a`,
  `@c-two/c2-mem-ffi` tarball SHA-256
  `15c8c341ba8b29bd4d68b4b55d8ecfcde90806a0a0ea6a87c54ae05bcb2a3125`,
  and source-built `c3` SHA-256
  `2b048ddd036197125b078df3f91016689b92c6b6e18fe28acc5b555973cba317`.
  These remain development inputs; Task 11 replaces them with
  manifest-bound candidate evidence.
- The Node/POSIX package passes 28 tests, strict typecheck, and a clean tarball
  install/typecheck/native-lifecycle `pack:check`. Its existing private package
  manifest already had the correct runtime inventory and build hooks, so no
  gratuitous manifest edit was made. The new high-level runtime composes the
  native loader and therefore is tested in the loader suite rather than
  duplicating low-level binding tests.
- `c2-codegen` format, strict Clippy, all unit/integration/doc tests, and the
  combined TypeScript real-call/Python codegen suite pass. FastDB remains clean
  at frozen commit
  `6b9d0a55f27bb22fd13f867f321db821f21e777c`; its real Wasm/TypeScript payload
  tests passed during the source-stage build.
- Owner-issue review records the remaining browser-runtime, typed cross-SDK
  dispatch-phase, and JavaScript-safe route-revision limitations, the opaque
  allocator boundary, and the distinction between development package hashes
  and Task 11 candidate provenance. Process/socket inspection found no
  surviving host, relay, Node, or TypeScript IPC residue.

## Task 10: Make FastDB’s Own Package Boundary Produce a Local Candidate

**Repository:** `../fastdb`
**Design sections:** 2, 13.2, 13.4, 14
**FastDB commit:** `build: package portable payload candidate`

**Create in FastDB:**

- `tests/ci/build_local_payload_candidate.py`
- `tests/ci/test_build_local_payload_candidate.py`
- `tests/ci/test_fastdb_sys_packaged_modes.py`

**Modify in FastDB:**

- `bindings/rust/fastdb/Cargo.toml`
- `bindings/rust/fastdb-sys/build.rs`
- `tests/ci/check_rust_payload_package.py`
- `tests/ci/test_check_rust_payload_package.py`

FastDB Core source, headers, ABI declarations, schemas, canonicalization,
binary layout, runtime semantics, codegen semantics, and package versions are
frozen in this task.

### Task 10.1 — Write package and bundle RED probes

- [ ] Run and retain the current failure:

```bash
cargo package \
  --manifest-path bindings/rust/fastdb/Cargo.toml \
  --allow-dirty --no-verify
```

Expected RED: the path dependency on `fastdb-sys` has no version and cannot be
resolved from a packaged manifest.

- [ ] Add tests that package both `.crate` archives, unpack them, and reject:
  absolute source-machine paths, sibling checkout paths, Git/build/cache files,
  and missing README/license/build files.
- [ ] Add a packaged-source-mode probe that runs the extracted `fastdb-sys`
  crate with default/source mode and requires a precise failure naming:
  `FASTDB_PAYLOAD_LINK_MODE=source` is checkout-only and the packaged consumer
  must use `system` with `FASTDB_PAYLOAD_SYSTEM_LIB_DIR`.
- [ ] Add a packaged-system-mode probe that links and runs from an extracted
  Core/C ABI bundle outside the FastDB checkout.
- [ ] Add a candidate-bundle test requiring:
  `libfastdb.dylib` on macOS (platform equivalent elsewhere),
  `include/fastdb_payload.h`, exact ABI version, exact sorted 117 symbols,
  relative inventory, byte sizes, SHA-256 values, source commit, build command,
  toolchain, target/platform, verified platforms, and unverified platforms.
- [ ] Reject duplicate inventory paths and any absolute path in the manifest.

Run RED:

```bash
uv run pytest \
  tests/ci/test_check_rust_payload_package.py \
  tests/ci/test_fastdb_sys_packaged_modes.py \
  tests/ci/test_build_local_payload_candidate.py -q
```

### Task 10.2 — Repair only the FastDB-owned package seam

- [ ] Change the safe crate dependency to:

```toml
fastdb-sys = { version = "0.1.22", path = "../fastdb-sys" }
```

- [ ] In `fastdb-sys/build.rs`, detect the required checkout source markers
  before recursive traversal/CMake configuration. Emit the precise packaged
  source-mode boundary instead of a misleading missing-directory panic.
- [ ] Keep source mode as the default for the real FastDB checkout.
- [ ] Keep system mode strict: require an absolute canonical directory and the
  platform library filename; never search the C-Two/Toodle repositories.
- [ ] Do not bundle FastDB Core source into the Rust crate to disguise the
  checkout-only source mode.

### Task 10.3 — Build the owner-repository artifact set

- [ ] `build_local_payload_candidate.py` accepts an absent output directory and
  the two Python executable paths. Build mode fails closed if the destination
  exists; `--verify DIRECTORY` reconstructs and compares its manifest without
  rebuilding artifacts.
- [ ] Set `SOURCE_DATE_EPOCH` from the audited Git commit timestamp and `TZ=UTC`
  for every archive build; do not use wall-clock timestamps in retained
  content.
- [ ] Configure a clean Release Core build with:

```bash
export FASTDB_LRC_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/fastdb-lrc.XXXXXX")"
cmake -S fastcarto -B "$FASTDB_LRC_ROOT/fastdb-core" \
  -DBUILD_TESTING=OFF \
  -DBUILD_TOOLS=OFF \
  -DUSE_SWIG_PYTHON=OFF \
  -DUSE_SWIG_NODE=OFF \
  -DUSE_SWIG_GO=OFF \
  -DCMAKE_BUILD_TYPE=Release
cmake --build "$FASTDB_LRC_ROOT/fastdb-core" \
  --target fastdb --config Release --parallel
```

- [ ] Copy only the resulting dynamic library and public
  `fastcarto/fastdb/include/fastdb_payload.h` into the Core/C ABI bundle.
- [ ] Run
  `tools/check_payload_abi_symbols.py --build-dir "$FASTDB_LRC_ROOT/fastdb-core"`
  and record exact ABI-117.
- [ ] Build `fastdb-sys-0.1.22.crate` and `fastdb-0.1.22.crate`.
- [ ] Build Python sdist, current-runtime wheel, and CPython-3.10 wheel from the
  exact source commit. The artifact hash—not the reused local `0.1.22`
  metadata—disambiguates this candidate from an older registry artifact.
- [ ] Rebuild FastDB Wasm/TypeScript and create the `fastdb4ts-0.0.3.tgz`
  tarball with its `.wasm` inside the packed inventory.
- [ ] Write `fastdb-local-candidate-manifest.v1.json` after packaging by reading
  the archives/tarballs, not by trusting source manifests.

Run focused GREEN:

```bash
cargo fmt --manifest-path bindings/rust/Cargo.toml --all -- --check
cargo test --manifest-path bindings/rust/Cargo.toml --workspace --all-features
uv run pytest \
  tests/ci/test_check_rust_payload_package.py \
  tests/ci/test_fastdb_sys_packaged_modes.py \
  tests/ci/test_build_local_payload_candidate.py -q
```

Run affected FastDB broad gates:

```bash
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
uv build
bash ts/build-wasm.sh
npm --prefix ts/fastdb4ts run build
npm run test:ts
cargo test --manifest-path bindings/rust/Cargo.toml --workspace --all-features
git diff --check
```

Then execute the candidate helper into one absent temporary root. Re-run its
manifest constructor over the unchanged artifact directory and require
byte-identical manifest JSON; this proves deterministic evidence construction
without pretending every native compiler/archive format is reproducible.

```bash
uv run python tests/ci/build_local_payload_candidate.py \
  --output "$FASTDB_LRC_ROOT/candidate-a" \
  --python-current "$(command -v python3)" \
  --python-310 "$(command -v python3.10)"
uv run python tests/ci/build_local_payload_candidate.py \
  --verify "$FASTDB_LRC_ROOT/candidate-a"
```

Review before FastDB commit:

- [ ] Confirm no FastDB Core/C ABI/schema/manifest semantic source changed.
- [ ] Confirm exact ABI remains 117 and ABI version remains 1.
- [ ] Confirm the helper contains no C-Two route/CRM/relay/contract behavior.
- [ ] Confirm package versions, branch, remote, tags, and publication state are
  unchanged.

## Task 11: Package the Complete C-Two Closure and Prove Isolated Consumers

**Repository:** C-Two
**Design sections:** 13, 14, 15.5, 16, 17
**Commit:** `build: prove isolated local release candidate`

**Create:**

- `tools/local_rc/artifact_manifest.py`
- `tools/local_rc/build_candidate.py`
- `tools/local_rc/local_registry.py`
- `tools/local_rc/package_consumers.py`
- `tools/local_rc/process_guard.py`
- `tests/repo/test_local_candidate_manifest.py`
- `tests/repo/test_local_registry.py`
- `tests/repo/test_package_consumers.py`

**Modify:**

- `core/foundation/c2-codegen/Cargo.toml`
- `core/foundation/c2-mem-ffi/Cargo.toml`
- `core/foundation/c2-mem/Cargo.toml`
- `core/protocol/c2-wire/Cargo.toml`
- `core/runtime/c2-core/Cargo.toml`
- `core/transport/c2-http/Cargo.toml`
- `core/transport/c2-ipc/Cargo.toml`
- `core/transport/c2-server/Cargo.toml`
- `cli/Cargo.toml`
- `sdk/python/native/Cargo.toml`
- `sdk/rust/Cargo.toml`
- `core/Cargo.lock`
- `cli/Cargo.lock`
- `sdk/python/native/Cargo.lock`
- `sdk/rust/Cargo.lock`
- `core/foundation/c2-mem-ffi/bindings/typescript/package.json`
- `core/foundation/c2-mem-ffi/bindings/typescript/scripts/pack-check.mjs`
- `core/foundation/c2-mem-ffi/bindings/typescript/scripts/check-consumer-install.mjs`
- `tests/repo/test_python_package_release_workflow.py`
- `tests/repo/test_c3_tool.py`

### Task 11.1 — Freeze package-closure RED tests

- [ ] Scan every Cargo dependency table entering the candidate and fail any
  first-party `path` dependency without an exact compatible `version`.
- [ ] Package every first-party crate and inspect normalized `Cargo.toml`; fail
  any remaining dependency on an active source path or a missing version.
- [ ] Create a Rust consumer outside C-Two/FastDB/Toodle whose manifest contains
  only:

```toml
[dependencies]
c-two = "=0.1.0"
fastdb = "=0.1.22"
```

- [ ] Reject `[patch]`, `[replace]`, Git dependencies, path dependencies, and
  source aliases pointing to any active repository.
- [ ] Require an offline local registry made from `.crate` archives, not
  unpacked source directories.
- [ ] Require Python current/3.10 installs from `--no-index` wheelhouse with
  checkout `PYTHONPATH` removed.
- [ ] Require Node install/compile/run from local npm tarballs with no sibling
  path aliases.
- [ ] Add hostile manifest tests for duplicate paths, changed bytes, wrong hash,
  absolute path, unrecognized schema, wrong commit, and an artifact listed
  before it actually exists.

Run RED:

```bash
uv run pytest \
  tests/repo/test_local_candidate_manifest.py \
  tests/repo/test_local_registry.py \
  tests/repo/test_package_consumers.py -q
```

Expected RED: C-Two path dependencies are path-only, no archive registry
builder exists, and no isolated multi-language candidate consumer exists.

### Task 11.2 — Add version requirements without changing source truth

- [ ] Add `version = "0.1.0"` beside every C-Two first-party `path` dependency,
  including optional/build/dev dependencies that enter package validation.
- [ ] Keep the paths for source-checkout builds.
- [ ] Keep FastDB at `version = "0.1.22"` plus the sibling path wherever the
  official Rust projection is used.
- [ ] Regenerate every affected lockfile with the pinned Rust toolchain.
- [ ] Run a normalized-manifest scan after `cargo package`; source-tree paths
  must not survive into packaged dependency specifications.

### Task 11.3 — Build the Rust archive registry and consumer

- [ ] Pin the registry construction tool to
  `cargo-local-registry 0.2.12`:

```bash
cargo install cargo-local-registry --version 0.2.12 --locked
```

If the exact binary/version is already installed, record it and do not
reinstall.

- [ ] Generate one synthetic closure lockfile, then run:

```bash
export C2_LRC_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/c2-lrc.XXXXXX")"
export C2_LRC_REGISTRY="$C2_LRC_ROOT/registry"
cargo local-registry sync \
  "$C2_LRC_ROOT/closure/Cargo.lock" \
  "$C2_LRC_REGISTRY"
```

The pinned `cargo-local-registry 0.2.12` help proves that its `add` command
accepts a crate name/version to fetch from an upstream registry; it cannot
import a locally built `.crate` archive. Do not use the previously assumed
`add "$REGISTRY" "$archive"` spelling. Import every FastDB/C-Two first-party
archive through the tested `tools.local_rc.local_registry.add_crate_archive`
helper, which copies the exact archive bytes and writes Cargo's standard
checksum-backed local-index record from the packaged normalized manifest.
Then prove Cargo consumes that registry offline. This is an evidence-driven
correction to the plan, not a fallback to source paths.

- [ ] Break Cargo's unpublished-workspace bootstrap cycle without weakening
  the retained candidate:
  1. reconstruct both approved Git commits under one disposable source root;
  2. sync the external lockfile closure while the committed `version + path`
     edges are still resolvable;
  3. package the C-Two closure in dependency order, importing each archive
     into a disposable bootstrap registry before packaging its dependents;
  4. remove dependency paths from the C-Two packaging snapshot;
  5. build the retained C-Two archives against that archive-only bootstrap
     registry and import only those final bytes into the retained registry;
  6. regenerate the disposable snapshot lockfiles offline against the retained
     registry before building c3, Python, or Node artifacts.

  Cargo resolves every member of the unpublished workspace even when
  packaging one leaf, so importing final archives strictly one at a time
  cannot seed the first package. The bootstrap registry, its packages, both
  source snapshots, and their Cargo homes are deleted after construction.
  They are packaging inputs only: the retained registry and every isolated
  consumer contain final archives and version-only dependency specifications,
  with no source path fallback.

- [ ] Configure the isolated consumer by rendering the canonical run-specific
  registry path:

```python
config_text = f"""\
[source.crates-io]
replace-with = "local-candidate"

[source.local-candidate]
local-registry = "{registry_dir.as_posix()}"

[net]
offline = true
"""
```

- [ ] That temporary absolute path is test configuration and must not enter any
  artifact, source archive, or retained manifest.
- [ ] Run `cargo build --offline`, generated client/host examples, and a real
  route-bound call with:

```bash
FASTDB_PAYLOAD_LINK_MODE=system
FASTDB_PAYLOAD_SYSTEM_LIB_DIR="$C2_LRC_ROOT/candidate/fastdb/core/lib"
DYLD_LIBRARY_PATH="$C2_LRC_ROOT/candidate/fastdb/core/lib"
```

Use the platform runtime-library variable equivalent outside macOS.

- [ ] Scan `.crate` contents, normalized manifests, generated source, build
  diagnostics, and the final binary dependency report for `/Users/soku`,
  `../fastdb`, `../c-two`, `../toodle`, and the three repository roots.

### Task 11.4 — Build C-Two Python and Node artifacts

- [ ] Build C-Two Python sdist plus current-runtime and CPython-3.10 wheels,
  retaining version `0.5.1`.
- [ ] Combine them with Task 10 FastDB wheels in one isolated wheelhouse.
- [ ] Include and hash the complete runtime dependency wheel closure required
  by those first-party wheels. For the current candidate this is exactly
  `numpy==2.2.6` for CPython 3.10 and `numpy==2.5.1` for the current CPython
  3.14 runtime. Record both as `third-party` artifacts selected by the C-Two
  source commit; do not preinstall them from an index before the no-index
  proof.
- [ ] Treat that pair as an explicit Phase 0B closure, not a generic Python
  dependency-vendoring claim. A future first-party metadata change must fail
  the exact wheelhouse/manifest checks until the pinned closure and its
  platform evidence are deliberately updated.
- [ ] In two clean environments:

```bash
uv venv --python "$(command -v python3)" "$C2_LRC_ROOT/venv-current"
uv pip install --python "$C2_LRC_ROOT/venv-current/bin/python" \
  --no-index --find-links "$C2_LRC_ROOT/candidate/python" \
  fastdb4py==0.1.22 c-two==0.5.1
uv venv --python "$(command -v python3.10)" "$C2_LRC_ROOT/venv-310"
uv pip install --python "$C2_LRC_ROOT/venv-310/bin/python" \
  --no-index --find-links "$C2_LRC_ROOT/candidate/python" \
  fastdb4py==0.1.22 c-two==0.5.1
```

- [ ] With `PYTHONPATH`, editable sources, project virtualenv variables, and
  repository current directories removed, import, generate, host, call, hold,
  invalidate, materialize, and shut down through installed packages.
- [ ] Build and `npm pack` `@c-two/c2-mem-ffi@0.1.0`; inspect that the `.node`
  addon and `libc2_mem_ffi.dylib` platform artifact are in the tarball.
- [ ] Obtain a local tarball for the pinned TypeScript compiler
  `typescript@5.9.3`; treat it as a recorded build tool, not a C-Two runtime
  dependency.
- [ ] Install only the FastDB, C-Two native runtime, and TypeScript tarballs in
  a clean Node project. Generate through the candidate `c3` binary, compile
  without path aliases, and run the Task 9 real-call suite.

### Task 11.5 — Build the complete candidate manifest

- [ ] `build_candidate.py` accepts:
  exact three source commits, absent output directory, current/3.10 Python,
  audited Node/npm, and the Task 10 FastDB candidate directory.
- [ ] Build/package in dependency order, hash the local `c3` binary, and write
  `local-release-candidate-manifest.v1.json` only after every artifact exists.
- [ ] Propagate the package-input commit timestamp through
  `SOURCE_DATE_EPOCH` and set `TZ=UTC` for Cargo, Python, and npm archive
  construction.
- [ ] Every artifact entry records owner repository, source commit, package
  name/version, kind, archive inventory, bytes, SHA-256, build command,
  toolchain, target/platform, verified platforms, and unsupported/unverified
  platforms.
- [ ] Keep absolute execution paths in a non-retained run log only. The retained
  manifest contains relative artifact paths and portable command templates.
- [ ] Reconstruct the manifest twice over the same immutable artifact directory
  and require byte-identical JSON. The package-consumer proof must use the
  exact hashed artifacts in that retained manifest.

### Task 11.6 — Rerun final package-installed matrix and TypeScript suites

- [ ] Run the Task 8 matrix with Rust resolving only the local `.crate`
  registry and Python resolving only the wheelhouse.
- [ ] Require exactly 18/18, no skips/xfails, and candidate package hashes in
  every row.
- [ ] Run the Task 9 Node suite from tarballs only.
- [ ] Write candidate-stage receipts:
  - `portable-matrix-receipt.v1.json`;
  - `typescript-real-call-receipt.v1.json`;
  - `package-consumer-receipt.v1.json`.
- [ ] Validate every receipt against the complete candidate manifest.

Run focused GREEN:

```bash
uv run pytest \
  tests/repo/test_local_candidate_manifest.py \
  tests/repo/test_local_registry.py \
  tests/repo/test_package_consumers.py \
  tests/repo/test_portable_matrix_receipt.py \
  tests/repo/test_typescript_receipt.py -q
```

Execute the real candidate builder and consumers:

```bash
export C2_LRC_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/c2-lrc.XXXXXX")"
export C2_LRC_OUTPUT="$C2_LRC_ROOT/candidate"
uv run python -m tools.local_rc.build_candidate \
  --output "$C2_LRC_OUTPUT" \
  --fastdb-candidate "$FASTDB_LRC_ROOT/candidate-a" \
  --python-current "$(command -v python3)" \
  --python-310 "$(command -v python3.10)"

uv run python -m tools.local_rc.package_consumers \
  --candidate "$C2_LRC_OUTPUT" \
  --receipt "$C2_LRC_OUTPUT/package-consumer-receipt.v1.json"
```

Run affected broad gates:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo clippy --manifest-path cli/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path cli/Cargo.toml --all-features
cargo fmt --manifest-path sdk/rust/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/rust/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo fmt --manifest-path sdk/python/native/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/python/native/Cargo.toml --all-targets --no-deps -- -D warnings
uv run pytest sdk/python/tests -q
"$(command -v python3.10)" -m compileall -q sdk/python/src
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript test
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript run pack:check
uv run pytest tests/repo -q
git diff --check
```

Review before commit:

- [ ] Inspect every normalized Cargo manifest and package inventory.
- [ ] Prove the Rust consumer has no `[patch]`/path/Git escape.
- [ ] Prove both Python consumers import from their temporary site-packages.
- [ ] Prove Node resolves both first-party packages from temporary
  `node_modules`.
- [ ] Compare the 18-row and TypeScript receipt artifact hashes to the candidate
  manifest.
- [ ] Confirm no version/push/tag/publish/release action occurred.

## Task 12: Retain Evidence, Update Three-Repository Status, and Close the Goal

**Repositories:** FastDB, C-Two, Toodle
**Design sections:** 1–4, 17–22
**Commits:**

- FastDB: `docs: record C-Two local candidate handoff`
- C-Two: `docs: record upstream local candidate closure`
- Toodle: `docs: record upstream local candidate status`

**Create in FastDB:**

- `docs/issues/evidence/README.md`
- `docs/issues/evidence/fastdb-local-candidate-manifest.v1.json`

**Modify in FastDB:**

- `README.md`
- `docs/issues/0002-portable-payload-foundation-implementation-status.md`
- `docs/issues/README.md`

**Create in C-Two:**

- `docs/reports/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md`
- `docs/reports/evidence/local-release-candidate-manifest.v1.json`
- `docs/reports/evidence/portable-matrix-receipt.v1.json`
- `docs/reports/evidence/typescript-real-call-receipt.v1.json`
- `docs/reports/evidence/package-consumer-receipt.v1.json`
- `docs/reports/evidence/sdk-capability-parity.v1.json`

**Modify in C-Two:**

- `README.md`
- `AGENTS.md`
- `CHANGELOG.md`
- `docs/roadmap.md`
- `docs/issues/contract-release-deferred-capabilities.md`
- `docs/superpowers/specs/README.md`
- `docs/superpowers/plans/README.md`

**Modify in Toodle only:**

- `docs/11-development-roadmap.md`
- `docs/13-codex-first-slices.md`
- `docs/14-upstream-boundaries.md`
- `docs/issues/0002-c-two-contract-release-consumption.md`
- `docs/issues/0003-fastdb-c-two-portable-payload-clean-cut.md`
- `docs/issues/README.md`
- `CHANGELOG-RUST-FIRST.md`
- `MANIFEST.json`

The approved design used generic wording `CHANGELOG.md` for the Toodle
handoff. Repository reality is `CHANGELOG-RUST-FIRST.md`, already linked by
Toodle README and listed in `MANIFEST.json`; update that existing authority and
do not create a second Toodle changelog.

### Task 12.1 — Validate and retain small evidence only

- [ ] Validate every temporary receipt before copying it into Git.
- [ ] Retain JSON manifests/receipts/reports only. Do not commit `.crate`,
  wheel, sdist, npm tarball, dynamic library, native addon, generated project,
  local registry, wheelhouse, virtualenv, `node_modules`, or build tree.
- [ ] The FastDB evidence file contains only FastDB-owned entries from the
  complete candidate manifest and exactly matches their bytes/hashes.
- [ ] The C-Two report records:
  - starting and ending implementation commits for all three repositories;
  - every scoped implementation commit;
  - the later documentation-only closure commits separately;
  - exact package-input commits used to build artifacts;
  - toolchain/platform versions;
  - the complete SDK/parity inventory;
  - all 18 row IDs and receipt hash;
  - TypeScript mode/host/path coverage and receipt hash;
  - package artifact inventories/hashes and consumer receipt;
  - fresh versus retained evidence;
  - copy-backed/held/borrowed claims at their proven strength;
  - same-agent review status, never “independent review”;
  - every external action not performed.
- [ ] Make explicit that artifacts were built from the Task 10/11
  implementation commits and documentation closure commits follow them.
  Never imply the documentation-only commit hash is an artifact source hash.

### Task 12.2 — Update FastDB owner truth

- [ ] State P5/Core remains frozen, sole payload authority, exact ABI-117, and
  unchanged package versions.
- [ ] Replace the obsolete current claim that no C-Two composition proof exists
  with exact local Rust SDK, 18-row, Node, package, and consumer evidence.
- [ ] State local artifacts are not official/published and their reused version
  metadata is disambiguated by commit plus SHA-256.
- [ ] Keep every remaining FastDB limitation, reason, impact, owner, and exit
  condition. Do not close hosted/publication/browser items.
- [ ] Leave the FastDB documentation/evidence update uncommitted until Tasks
  12.5 and 12.6 validate the complete three-repository closure diff.

### Task 12.3 — Update C-Two current authority

- [ ] Mark `c2-core` as the single shared route/transport/runtime/lifecycle
  authority and Rust/Python as projections.
- [ ] Mark complete only rows with executable exit criteria satisfied:
  bounded admission, complete Rust SDK, local FastDB Rust/Python/TS package
  consumption, strict Core/native Clippy, 18-row local matrix, real Node role,
  and local package closure.
- [ ] Retain open rows for official immutable distribution, hosted verification,
  browser runtime, C++ SDK, streaming, post-dispatch retry/deduplication,
  compatibility ranges, Authority trust/signature/revocation, and Toodle
  consumption.
- [ ] Explain local candidate truth separately from official release truth.
- [ ] Update plan/spec indexes so the approved design and this implementation
  plan are the current continuation of the earlier composition plan.
- [ ] Leave the C-Two documentation/evidence update uncommitted until Tasks
  12.5 and 12.6 validate the complete three-repository closure diff.

### Task 12.4 — Update Toodle tracking without production integration

- [ ] Record FastDB owner implementation locally complete at its audited commit.
- [ ] Record C-Two `c2-core`, Rust SDK, parity, admission, 18/18, Node, and
  package status only with the retained receipt hashes.
- [ ] State official immutable artifacts remain unpublished.
- [ ] Keep Toodle `CRMContractRef`, canonical consumption, `toodle-c2` adapter,
  and Phase 0B consumer integration unimplemented.
- [ ] State Toodle Phase 0B remains incomplete until official artifacts are
  pinned and consumed.
- [ ] Add no crate, Rust/Python/TypeScript production code, transport, codec,
  sidecar, or local package copy.
- [ ] Update `MANIFEST.json` entries for every changed tracked file with actual
  byte size and SHA-256. Reject duplicate paths and verify listed order as
  stored; do not impose a new global lexical-order invariant.
- [ ] Leave the Toodle tracking update uncommitted until Tasks 12.5 and 12.6
  validate the complete three-repository closure diff.

### Task 12.5 — Run final fresh gates on the exact closure trees

FastDB:

```bash
uv run pytest tests/python -q
uv run python -m compileall -q python/fastdb4py tests/python
uv build
bash ts/build-wasm.sh
npm --prefix ts/fastdb4ts run build
npm run test:ts
cargo fmt --manifest-path bindings/rust/Cargo.toml --all -- --check
cargo test --manifest-path bindings/rust/Cargo.toml --workspace --all-features
uv run pytest tests/ci -q
git diff --check
```

Re-hash the exact Task 10 FastDB candidate directory, verify it against the
retained manifest, rerun exact ABI-117 against its dynamic library, and rerun
the packaged system-mode Rust consumer. Do not rebuild a different artifact
set from a later documentation-only commit and substitute it for the retained
candidate. Core correctness evidence from the unchanged frozen implementation
is labeled retained; the package/link/ABI consumer execution is fresh.

C-Two:

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace --all-features
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo clippy --manifest-path cli/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path cli/Cargo.toml --all-features
cargo fmt --manifest-path sdk/rust/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/rust/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path sdk/rust/Cargo.toml --all-features
cargo fmt --manifest-path sdk/python/native/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/python/native/Cargo.toml --all-targets --no-deps -- -D warnings
uv run pytest sdk/python/tests -q
"$(command -v python3.10)" -m compileall -q sdk/python/src
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript test
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript run typecheck
npm --prefix core/foundation/c2-mem-ffi/bindings/typescript run pack:check
uv run pytest tests/repo -q
git diff --check
```

Rerun the isolated Rust/Python/Node consumers, exact 18/18 matrix, TypeScript
real calls, and receipt validators from the exact hashed Task 10/11 candidate
artifacts still held in the execution-owned temporary root. This is a fresh
consumer execution over the same immutable candidate, not a package claim for
the later documentation-only closure commits.

Before recording results, prove the uncommitted closure diffs contain no
package-input production change after the candidate commits:

```bash
git -C ../fastdb diff --name-only "$FASTDB_PACKAGE_INPUT_COMMIT" -- \
  fastcarto bindings python ts schemas setup.py pyproject.toml MANIFEST.in
git diff --name-only "$C2_PACKAGE_INPUT_COMMIT" -- \
  core cli sdk tools tests pyproject.toml
```

Both commands must return no match except retained test/evidence tooling already
present in the package-input commits. If production/package input changed, go
back to the owning implementation task, commit the fix, rebuild the complete
candidate, rerun every consumer, and replace all dependent receipts.

After the commands finish, patch exact counts/durations/hashes into the reports
once, then rerun receipt/manifest/Markdown-link/YAML/JSON/`git diff --check`
validators. Report-only edits do not justify rewriting the already-tested
artifact source commit.

Toodle:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
jq empty MANIFEST.json
ruby -ryaml -e 'ARGV.each { |path| YAML.safe_load(File.read(path), [], [], false) }; puts "yaml: ok"' \
  examples/*.yaml .github/workflows/ci.yml
git diff --check
```

Run a Markdown relative-link verifier that ignores fenced code and external/
fragment/absolute links. Run a manifest verifier that:

```ruby
manifest = JSON.parse(File.read("MANIFEST.json")).fetch("files")
paths = manifest.map { |entry| entry.fetch("path") }
abort("duplicate manifest paths") unless paths.uniq.length == paths.length
manifest.each do |entry|
  path = entry.fetch("path")
  abort("missing: #{path}") unless File.file?(path)
  abort("byte mismatch: #{path}") unless entry.fetch("bytes") == File.size(path)
  abort("digest mismatch: #{path}") unless
    entry.fetch("sha256") == Digest::SHA256.file(path).hexdigest
end
```

Run forbidden-boundary scans in all three repositories:

```bash
rg -n 'c2-sdk|c2_sdk|c2-runtime|c2_runtime|call-db|org\\.fastdb\\.call-db' \
  core cli sdk README.md AGENTS.md docs/roadmap.md docs/issues

rg -n 'c-two|CRM|relay|ContractRelease' \
  ../fastdb/fastcarto/fastdb/src/payload \
  ../fastdb/bindings/rust \
  ../fastdb/python/fastdb4py/payload \
  ../fastdb/ts/fastdb4ts/src/payload

rg -n 'c2-ipc|c2-http|c2-server|fastdb|serde_json|pickle' \
  ../toodle/crates
```

Matches in historical/deferred documentation are reviewed manually; production
authority violations are blockers.

### Task 12.6 — Perform the two final same-agent audits

- [ ] Spec audit: map every design completion criterion 1–16 to a retained
  command, receipt, manifest entry, or documentation statement.
- [ ] Ownership audit: prove FastDB owns payload meaning, C-Two Core owns
  route/transport/lifecycle, SDKs are projections, and Toodle contains tracking
  only.
- [ ] Correctness audit: review dispatch uncertainty, route state mapping,
  error parity, contract limits, checked arithmetic, and malformed inputs.
- [ ] Lifetime audit: trace owned, held, borrowed, early return, adapter error,
  invalidation error, release error, unwind, and finalizer behavior.
- [ ] Portability audit: inspect all package archives and retained JSON for
  machine paths, sibling paths, undeclared platform claims, and browser claims.
- [ ] Package audit: compare normalized manifests, archive inventories, exact
  hashes, local-registry index entries, wheel metadata, npm tarball contents,
  and runtime resolution locations.
- [ ] Maintenance audit: reject duplicate authorities, compatibility shims,
  dead low-level wrappers, stale current docs, and unexplained test-only
  branches.
- [ ] Fix every material finding in a scoped follow-up commit in the owning
  repository. A production/package-input fix invalidates the candidate and
  returns execution to Tasks 10–11 for a complete rebuild and receipt refresh;
  a documentation-only fix reruns all documentation/manifest gates. Append the
  fix/evidence to the report. Do not describe these two passes as independent
  review.

### Task 12.7 — Commit documentation, clean outputs, and prove final state

- [ ] Commit the validated FastDB documentation/evidence as
  `docs: record C-Two local candidate handoff`.
- [ ] Commit the validated C-Two documentation/evidence as
  `docs: record upstream local candidate closure`.
- [ ] Commit the validated Toodle tracking/manifest update as
  `docs: record upstream local candidate status`.
- [ ] Verify each commit contains exactly its planned files and the committed
  blobs equal the just-validated working-tree blobs. Do not alter content while
  staging or committing.

- [ ] Record artifact hashes and receipts before cleanup.
- [ ] Check for active builds across all three repositories; stop only processes
  started by this plan.
- [ ] Remove the execution-owned candidate roots, local Cargo registry,
  wheelhouse, temporary virtualenvs, temporary Node projects, `node_modules`,
  generated project trees, native/Wasm build trees, and disposable Cargo
  targets created for final proof.
- [ ] Do not remove source, tracked evidence, `.venv`, user projects, or
  `../fastdb/fastcarto/fastdb/src/payload/build`.
- [ ] Recheck disk space and confirm no plan-owned child process, relay, socket,
  or SHM segment remains.
- [ ] Verify all three worktrees:

```bash
git -C ../fastdb status --short --branch
git -C . status --short --branch
git -C ../toodle status --short --branch
git -C ../fastdb log -8 --oneline
git -C . log -16 --oneline
git -C ../toodle log -8 --oneline
```

- [ ] Require clean worktrees and the expected scoped commit sequence.
- [ ] Only after every completion criterion is mapped and no required work
  remains, mark the active Goal complete.
- [ ] Use exactly this completion statement, with no Toodle/publication/hosted
  inflation:

> **FastDB/C-Two Phase 0B upstream local release candidate complete**

## Plan Self-Review Checklist

- [ ] Every approved design section 1–22 maps to at least one task/check.
- [ ] Every completion criterion 1–16 has an executable exit gate.
- [ ] Every code-changing task names exact files, a true RED command, focused
  GREEN command, affected broad gates, review, and scoped commit.
- [ ] `c2-core` public types remain byte/route/lifecycle oriented and contain no
  FastDB dependency.
- [ ] Rust SDK naming is `sdk/rust` / `c-two` / `c_two`, never `c2-sdk`.
- [ ] Generated Rust depends only on the supported `c-two` seam plus official
  `fastdb`.
- [ ] Python native loses duplicate orchestration and gains no Rust-only
  portable capability gap.
- [ ] Contract admission is bounded before unbounded JSON allocation and parsed
  once per external operation.
- [ ] Positive matrix identity is exactly 18 rows and relay evidence uses real
  `c3`.
- [ ] TypeScript claims real Node behavior on the audited platform only.
- [ ] Package proof consumes archives/tarballs/wheels outside sibling checkouts.
- [ ] FastDB package changes remain owner-generic and preserve ABI-117.
- [ ] Toodle receives no production code.
- [ ] Every intentionally unimplemented behavior remains in an owner Issue with
  reason, impact, owner/dependency, exit condition, and verification.
- [ ] No unresolved substitution token, abbreviated executable code, unresolved
  path, or “similar to” instruction remains.
- [ ] The only final product claim is the exact approved completion sentence.
