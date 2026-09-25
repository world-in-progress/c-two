# C-Two Rust SDK and Portable-Payload Local Release Candidate Design

**Date:** 2026-07-24
**Status:** Approved by the active FastDB/C-Two goal for implementation
**Scope:** the language-neutral C-Two Core facade, the user-facing Rust SDK, Rust/Python parity, contract admission, direct/relay portable-payload evidence, TypeScript real calls, and isolated local release-candidate packaging

## 1. Authority and relationship to earlier design

This document extends, rather than redefines, the accepted
[`C-Two Portable Payload Contract and Composition Design`](./2026-07-24-portable-payload-contract-composition-design.md).
That document remains authoritative for:

- `c-two.contract.v2`;
- route-independent `ContractRelease` and `ContractReleaseRef`;
- opaque extraction and FastDB delegation;
- deterministic multi-owner artifact composition;
- the removal of call-db and other C-Two-owned FastDB semantics.

This document is authoritative for the next local release-candidate closure:

- the supported external Rust boundary;
- the shared language-neutral runtime facade used by Rust and Python;
- connection, host, lifecycle, and error semantics;
- versioned outer descriptor limits;
- the complete 18-row direct/relay matrix;
- TypeScript calls against real C-Two hosts;
- package consumers isolated from sibling source checkouts.

Where an older current document says that a complete Rust SDK, relay portable
matrix, TypeScript real-call proof, or isolated package closure does not yet
exist, that statement remains true until the implementation and retained
evidence required by this design have landed. This design does not itself
claim implementation completion.

## 2. Context

The accepted portable-payload implementation established the correct owner
boundary:

- FastDB C++ Core is the sole authority for
  `fastdb.payload.v1` parsing, canonicalization, digest, type/layout, binary
  encoding, builders, views, materialization, invalidation, and payload
  codegen.
- C-Two owns the outer CRM contract, method bindings, exact contract release
  identity, routes, transports, relays, runtime lifecycle, leases, generated
  CRM adapters, and final artifact composition.

The implementation nevertheless stops short of a consumable release
candidate:

1. Rust real-call proof manually assembles low-level crates such as
   `c2-ipc`, `c2-server`, `c2-mem`, and `c2-contract`.
2. `c2-runtime` owns part of the process runtime but not one complete
   language-neutral client/host/relay facade.
3. Python native code still assembles client pools, server bridges, relay
   selection, and error conversions around those lower-level crates.
4. Generated Rust clients are tied directly to the IPC implementation and do
   not cover explicit relay or relay-aware connection.
5. The portable matrix proves 9 direct-IPC rows, not the required 18
   direct/relay rows.
6. Generated TypeScript has compile/typecheck and simulated-lifecycle
   evidence, but not real no-payload, record, and object-graph calls through
   its declared transports.
7. C-Two outer descriptor parsing has no caller-configurable, versioned
   admission limits.
8. FastDB and C-Two path dependency manifests and FastDB Core linking do not
   yet form an isolated Rust package closure.

The target is a clean external boundary, not a new semantic kernel. FastDB
Core remains frozen at the accepted payload authority and exact ABI-117. This
work may repair package manifests and build seams, but it must not redesign or
copy FastDB Core behavior.

## 3. Goals

1. Evolve the current language-neutral runtime into one `c2-core` facade that
   owns client, host, route, relay, lifecycle, and error normalization used by
   every supported SDK.
2. Add a user-facing Rust SDK at `sdk/rust`, with Cargo package name `c-two`
   and Rust import name `c_two`.
3. Ensure ordinary Rust callers and generated Rust code never need to assemble
   C-Two's low-level transport, server, memory, or wire crates.
4. Keep the Rust SDK and Python SDK capability-equivalent for portable
   contract, route, transport, payload, and lifetime behavior.
5. Admit every external contract descriptor through one Rust-owned,
   versioned `ContractLimits` authority.
6. Execute and retain all 18 portable-payload direct/relay rows, including
   negative and ownership evidence.
7. Execute generated TypeScript against real Rust or Python hosts using every
   transport that the generated Node surface publicly claims.
8. Produce FastDB and C-Two local package artifacts and prove real consumers
   in temporary trees that cannot import sibling source.
9. Close the existing strict Clippy baselines without blanket suppression.
10. Leave current C-Two, FastDB, and Toodle documentation accurately
    distinguishing local implementation, local release-candidate evidence,
    official immutable distribution, and Toodle consumption.

## 4. Non-goals

- No FastDB Core semantic redesign or ABI change.
- No FastDB parser, canonicalizer, digest, layout, binary, builder, view,
  materializer, graph algorithm, or code generator in C-Two.
- No restoration of call-db, sidecar schemas, old profiles, provider bridges,
  or Python subprocess codegen.
- No Toodle `CRMContractRef`, canonical protocol, state machine, or
  `toodle-c2` implementation.
- No raw-file/file-group, Snapshot, RustFS, lakeFS, federation, or Kubernetes
  work.
- No C-Two C++ SDK.
- No user-visible streaming/async RPC.
- No FastDB segmented/multipart backing or streaming builder.
- No direct final-backing or zero-copy claim.
- No portable-payload performance conclusion.
- No contract semver/range negotiation.
- No Authority signature, trust, revocation, or policy implementation.
- No browser-runtime support claim from Node-only TypeScript evidence.
- No version bump, external push, tag, registry publication, hosted release,
  or GitHub Release.
- No repository-wide Cargo workspace migration merely to add the Rust SDK.
  The core, CLI, Python native extension, and SDK remain distinct artifact
  boundaries unless implementation evidence proves consolidation necessary.

Every deferred capability remains recorded in an owner Issue with reason,
impact, owner, dependency, and an executable exit condition.

## 5. Selected architecture

```text
                           c2-contract
               contract.v2, release, ContractLimits
                                |
                                v
 c2-config   c2-error   c2-wire   c2-ipc   c2-http   c2-server   c2-mem
      \          |          |         |         |          |          /
       \---------+----------+---------+---------+----------+---------/
                                |
                                v
                            c2-core
          client, host, route, relay, lifecycle, lease ordering,
                         error normalization
                         /               \
                        v                 v
             Rust SDK (`c-two`)    Python native facade
                        \                 /
                         \               /
                         generated CRM adapters
                                |
                                v
                 official FastDB language projection
                         and FastDB C++ Core
```

`c2-core` is language-neutral. It may transport opaque payload bytes and own
lease ordering, but it does not import or inspect FastDB specifications,
payload types, binary layouts, or views.

The Rust SDK and Python native facade both call `c2-core`; neither implements
an independent route/relay/session state machine. Language adapters use the
official FastDB binding for payload construction and validation and attach
the resulting invalidation operation to the Core-owned lifetime sequence.

## 6. Naming and package layout

### 6.1 Language-neutral Core

The current package:

```text
core/runtime/c2-runtime
Cargo package: c2-runtime
Rust import: c2_runtime
```

is cleanly evolved and renamed to:

```text
core/runtime/c2-core
Cargo package: c2-core
Rust import: c2_core
```

There is no compatibility alias. C-Two is 0.x, all in-repository callers are
updated in the same cut, and retaining two runtime facades would make their
ownership ambiguous.

### 6.2 Rust SDK

The user-facing package is:

```text
sdk/rust
Cargo package: c-two
Rust import: c_two
component name: Rust SDK
```

The initial local-candidate manifest uses the current Rust workspace version
`0.1.0` and `publish = false`. This is a package identity required for local
Cargo evidence, not a registry release or an authorized final version choice.

Existing package metadata is not changed merely to make the candidate look
released:

| Artifact | Current metadata retained |
|---|---:|
| C-Two Python | `0.5.1` |
| C-Two CLI | `0.1.4` |
| C-Two Rust workspace/Core | `0.1.0` |
| FastDB Rust/Python | `0.1.22` |
| FastDB TypeScript | `0.0.3` |

Every retained local artifact identity includes the package version, source
commit, target, and SHA-256. A version string alone is never evidence that the
new API exists in a public registry.

## 7. Rust SDK public boundary

### 7.1 Contract and client flow

The ordinary Rust path is conceptually:

```rust
let release = ContractRelease::from_descriptor_json_with_limits(
    descriptor,
    ContractLimits::default(),
)?;
release_ref.verify_release(&release)?;

let client = runtime.connect(
    release.expected_route("service/route")?,
    Connect::DirectIpc(address),
)?;

let client = generated::ContractClient::new(client)?;
let result = client.record_roundtrip(&payload)?;
```

Exact method names may be adjusted during API implementation for Rust
conventions, but the public responsibilities are fixed:

- load and admit `c-two.contract.v2`;
- construct and verify `ContractRelease` and `ContractReleaseRef`;
- derive `ExpectedRouteContract` only from a validated release plus a route
  name;
- connect using a closed, typed mode;
- construct a generated route-bound client;
- call with no payload or an official FastDB `Payload`;
- return typed errors and checked ownership.

### 7.2 Connection modes

```text
Connect::DirectIpc
Connect::ExplicitRelay
Connect::RelayAware
```

All three produce the same route-bound `c_two::Client`. Generated clients do
not branch on transport.

- `DirectIpc` uses the explicitly supplied IPC address and expected route. It
  performs no relay lookup and no implicit fallback.
- `ExplicitRelay` performs resolution and data-plane calls through the
  explicitly supplied relay. It does not silently short-circuit to IPC.
- `RelayAware` resolves through a relay and may choose a verified local IPC
  candidate before dispatch. Its fallback rules are defined in section 11.

### 7.3 Generated client seam

Generated Rust code no longer imports `c2-ipc`, `c2-http`, `SyncClient`, or
`RouteBinding`. It consumes a stable encoded-call boundary supplied by the
Rust SDK.

The boundary is sealed to C-Two transport implementations. Generated code can
use it, while downstream code cannot bypass route, contract, identity, and
lifetime checks by providing an arbitrary transport that only happens to
return bytes.

Generated payload adapters continue to import the official `fastdb` crate.
C-Two does not re-export FastDB under a C-Two-owned semantic namespace.

### 7.4 Generated host seam

Generated Rust host output provides:

- a typed `Service` trait;
- release and route-contract facts;
- a method dispatch adapter;
- no-payload and FastDB payload decode/encode;
- a route definition consumable by the Rust SDK host facade.

The user implements the generated trait and registers it through `c_two`.
They do not construct a `c2-server` dispatcher, memory pool, route catalog, or
IPC handler directly.

### 7.5 Core escape hatch

Advanced Rust integrators, including a future Toodle adapter, may depend
directly on `c2-core`. This is an embedding and orchestration boundary, not a
second SDK and not access to additional FastDB semantics.

If a capability is truly required by C-Two consumers in more than one
language, it belongs in `c2-core` and receives at least Rust and Python
projections. Rust-only convenience may shape types idiomatically, but it
cannot create a Rust-only contract, route, payload, or lifetime meaning.

## 8. Client and host lifetime semantics

### 8.1 Owned result

The normal portable receive path remains copy-backed:

1. C-Two obtains the response and its response lease.
2. The generated language adapter opens an owned FastDB copy through the
   official binding.
3. C-Two releases the response lease.
4. The SDK returns the owned FastDB payload.

This proves ownership independence from the response backing. It does not
claim zero-copy.

### 8.2 Held result

The retained Rust result is conceptually `Held<Payload>`. Python retains its
corresponding held-result facade.

Explicit `release()` and `Drop`/finalization use one idempotent Core-owned
sequence:

1. invalidate the FastDB payload owner and checked views;
2. release the C-Two response lease;
3. mark the held result released.

The sequence remains true on adapter failure and early return. A language
binding may expose an explicitly unsafe raw-buffer escape hatch, but C-Two
does not claim that raw pointers extracted through such an escape can later
be revoked.

### 8.3 Borrowed host input

Generated Rust services receive a scoped `&Payload`; Python receives the
equivalent callback-scoped payload object.

After the callback completes:

1. the payload owner is invalidated;
2. the request lease is released;
3. checked views and clones sharing that owner fail;
4. detached values materialized through FastDB remain valid.

FastDB's owner semantics are authoritative. C-Two only owns callback and
transport lease ordering.

### 8.4 No privileged Rust receive path

The Rust SDK does not introduce `open_external`, direct final backing, or
another zero-copy receive mode while Python remains copy-backed. Such a path
requires a future cross-language design and evidence.

Python pickle and thread-local Python invocation remain explicitly
nonportable language behavior. They do not enter generated portable clients,
release descriptors, or cross-language acceptance.

## 9. Rust/Python capability parity

| Capability | Authority | Rust SDK | Python SDK |
|---|---|---|---|
| Descriptor admission and exact release identity | `c2-contract` | projection | native projection |
| Route registration, resolution, and lifecycle | `c2-core` | facade | native facade |
| Direct IPC | `c2-core` | supported | supported |
| Explicit relay | `c2-core` | supported | supported |
| Relay-aware connection | `c2-core` | supported | supported |
| Generated typed client and service | `c2-codegen` | supported | supported |
| No-payload, record, and object graph | FastDB language binding | supported | supported |
| Owned, held, and borrowed lifetime | Core ordering + FastDB owner | Rust projection | Python projection |
| Structured C-Two error | `c2-error` | typed | typed exception |
| Structured FastDB cause | FastDB error + C-Two outer mapping | preserved | preserved |
| Python pickle/thread-local invocation | Python-only | not copied | explicitly nonportable |
| Advanced runtime embedding | `c2-core` | public crate | native binding |

Differences in syntax, RAII, exceptions, or context managers are language
projections. Any unexplained difference in portable capability is a release
candidate blocker.

## 10. Unified error model

### 10.1 Canonical semantic error

The existing Rust-owned `C2E1` wire and `C2Error` registry remain the only
C-Two semantic error authority. No SDK-specific error wire is introduced.

`C2Error` may originate at the client, relay, or service. Rust and Python must
observe the same:

```text
code
name
message
details
```

IPC error bytes and HTTP error envelopes are decoded to the same type.
Malformed semantic error bytes become a typed protocol failure rather than a
display string.

### 10.2 Local SDK error

Rust exposes a local facade conceptually shaped as:

```text
c_two::Error
  Semantic(C2Error)
  Contract(...)
  Admission(...)
  Transport(...)
  Lifecycle(...)
```

Local variants organize failures that do not yet have cross-language semantic
meaning. They are not serialized as new error codes. Underlying sources are
retained for Rust error chaining and Python exception projection; low-level
`IpcError`, HTTP, or PyO3 errors are not flattened independently at multiple
call sites.

`c2-core` owns normalization. Rust SDK and Python native do not maintain
parallel transport-to-semantic mappings.

### 10.3 FastDB cause preservation

When a generated adapter turns a FastDB failure into the appropriate C-Two
client/resource phase error, it preserves:

```text
cause_owner=fastdb
fastdb_code
fastdb_symbol
fastdb_path
fastdb_message
fastdb_details_json
```

The field convention belongs to the C-Two outer error contract. Values come
only from the official FastDB error object. Rust and Python contract tests
assert exact field parity; neither projection assembles ad hoc prose in place
of those fields.

## 11. Relay and retry semantics

### 11.1 Route state is not release identity

`ContractRelease` is immutable, route-independent content identity. A route
is mutable runtime state.

| Runtime condition | Semantic error |
|---|---|
| Resource has never been resolved | `ResourceNotFound` |
| Known resource is temporarily unreachable | `ResourceUnavailable` |
| Previously observed route was removed | `ResourceRemoved` |
| Route exists but accepts no new calls | `ResourceClosed` |
| Client holds an older route token | `RouteStale` |

Route removal never deletes or mutates a logical release. A caller may still
verify the retained `ContractReleaseRef` after route disappearance. A later
connection must resolve fresh runtime state.

### 11.2 Fallback is pre-dispatch only

Automatic transport fallback is allowed only while C-Two can prove that the
CRM method has not been dispatched.

| Failure | Behavior |
|---|---|
| Verified local IPC candidate cannot be acquired before dispatch | relay-aware mode may choose an independent relay path |
| Relay path would return to the same failed local path | `FallbackDenied` |
| `ContractMismatch`, `IdentityMismatch`, or `ProtocolViolation` | terminal; no fallback |
| Cached route is stale before dispatch | discard cache and resolve at most once |
| Service returns `C2Error` | terminal; no fallback |
| Call may have been dispatched before connection loss | typed uncertain transport failure; no automatic replay |

C-Two currently has no call-identity/deduplication protocol capable of making
arbitrary non-idempotent replay safe. Post-dispatch retry is therefore
forbidden rather than hidden behind transport selection.

## 12. Versioned contract admission

### 12.1 Authority

`ContractLimits` is defined in `c2-contract` and re-used by `c2-core`, the
Rust SDK, Python native, CLI, and codegen. Python and CLI contain no second
default table.

```text
ContractLimitsProfile::V1
ContractLimits
ContractLimitMetric
ContractError::LimitExceeded
```

V1 defaults are:

| Metric | V1 default |
|---|---:|
| source bytes | 16 MiB |
| JSON values | 1,000,000 |
| JSON nesting depth | 128 |
| methods | 256 |
| cumulative extracted nested FastDB bytes | 16 MiB |

The source, value, and depth defaults align with the accepted FastDB JSON
admission baseline. The method maximum aligns with the existing C-Two wire
capacity. The nested-byte limit bounds cumulative C-Two extraction and
delegation work while every individual nested spec remains subject to
FastDB-owned limits.

These are control-plane limits. They are intentionally much smaller than the
default 16 GiB route payload capacity and do not constrain runtime payload
bytes.

### 12.2 Stable counting

- Source bytes are the exact input byte length before JSON parsing.
- The JSON value count includes the root and every object member value and
  array element; object keys are not values.
- Root depth is one; every nested array/object child increments the observed
  value depth.
- Method count is the exact outer `methods` array length.
- Nested FastDB bytes are the checked sum of deterministic extracted JSON
  bytes for every input/output binding occurrence. Repeated equal specs count
  repeatedly for admission even if codegen later deduplicates them by
  FastDB-owned digest.

All counters use checked arithmetic. Oversized or platform-unrepresentable
configuration is rejected before parsing.

### 12.3 One bounded admission

External descriptor handling is:

1. reject source size before parsing;
2. parse through a bounded Rust JSON admission visitor;
3. reject value/depth overflow with a stable limit metric;
4. validate the exact C-Two outer shape;
5. reject method count before large method allocation or iteration;
6. extract and cumulatively bound opaque nested JSON;
7. derive fingerprints and one validated `ContractRelease`;
8. reuse that release for release reference, expected route, and codegen.

The implementation must not first build an unbounded `serde_json::Value` and
only inspect limits afterward. It must also avoid the current duplicate parse
where codegen constructs a validated descriptor and then reparses the same
bytes for `ContractRelease`.

`ContractReleaseRef` JSON uses the same bounded source/value/depth machinery;
method and nested-spec metrics are not applicable to that smaller schema.

### 12.4 Error and profile evolution

`ContractError::LimitExceeded` carries at least:

```text
profile
metric
limit
observed
path
```

V1 defaults do not enter descriptor canonical bytes or digest because limits
are admission policy rather than contract content. Changing V1 silently is
forbidden. A future default change adds a new profile; explicit caller
overrides are recorded in local evidence.

The contract-owned hard method capacity is defined once in `c2-contract`, and
`c2-wire` consumes that constant. This removes a second independent `256`
authority without making `c2-contract` depend on the wire crate.

## 13. Local release-candidate artifact set

### 13.1 Truth boundary

This design produces:

> FastDB/C-Two Phase 0B upstream local release candidate complete

only after every acceptance condition is proven.

It does not produce:

- official FastDB or C-Two packages;
- a hosted release;
- a public version selection;
- a Toodle Phase 0B completion claim.

### 13.2 FastDB artifacts

From the frozen FastDB source revision:

- C++ Core/C ABI dynamic library;
- public C ABI header and exact ABI-117 receipt;
- `fastdb-sys` and `fastdb` `.crate` archives;
- Python sdist;
- wheel for the current Python runtime;
- CPython 3.10 wheel;
- `fastdb4ts` npm tarball including its Wasm artifact.

The Rust packages use the separate Core/C ABI bundle in isolated consumers.
`fastdb-sys` system mode receives an absolute library-bundle directory. The
crate does not assume an unpacked package still has the original repository's
`fastcarto/` source layout.

If the source-mode build remains checkout-only, the packaged error explains
that boundary precisely. This local candidate does not copy FastDB source or
semantics into C-Two to conceal it.

### 13.3 C-Two artifacts

- `.crate` archives for the Rust dependency closure required by `c2-core` and
  `c-two`;
- the `c-two` Rust SDK package;
- C-Two Python sdist and wheel;
- local `c3` binary;
- local npm tarballs and bundled native artifacts required by the generated
  Node direct-IPC surface, including the C-Two memory/IPC runtime package;
- Rust, Python, and TypeScript contract trees generated by one `c3`
  composition path;
- matrix and package receipts.

The public Rust SDK and generated consumers depend on versioned packages.
Every normal/build path dependency that enters the packaged FastDB/C-Two
closure carries a compatible version requirement; this applies to C-Two's
internal crates as well as FastDB. Source manifests use `version + path`, for
example:

```toml
fastdb-sys = { version = "0.1.22", path = "../fastdb-sys" }
```

Path remains the truthful source-checkout dependency until an authorized
external publication. Cargo's packaged normalized manifest can then resolve
the version from the local candidate source.

At a future authorized release, selecting new public versions, publishing the
dependency closure in order, and changing production consumers from
`version + path` to version-only resolution is one reviewed release
transaction. None of those external steps occurs in this goal.

### 13.4 Artifact manifest

`local-release-candidate-manifest.v1` records, for every artifact:

```text
owner repository
source commit
package name and version
artifact kind
relative inventory
byte size
SHA-256
build command
toolchain versions
target triple/platform
declared verified platforms
declared unverified/unsupported platforms
```

The manifest records package contents after packaging, not merely source-tree
intent. It rejects duplicate paths and absolute development-machine paths.

Large binaries remain outside Git. Small manifests, hashes, commands, and
reports are retained.

## 14. Isolated package consumers

### 14.1 Rust

1. Package the complete FastDB/C-Two Rust closure.
2. Construct a temporary local registry solely from the resulting `.crate`
   archives.
3. Create a consumer outside all three repositories.
4. Declare package name and version only.
5. Do not patch to the active FastDB or C-Two checkout.
6. Link the extracted FastDB Core/C ABI candidate in system mode.
7. compile and run generated client and host examples.
8. scan packaged manifests, sources, and build diagnostics for sibling paths
   and developer-machine absolute paths.

Testing an unpacked source directory through a new path dependency is not
package evidence.

### 14.2 Python

1. Build FastDB and C-Two sdists/wheels into an isolated wheelhouse.
2. Create clean current-Python and CPython-3.10 environments.
3. install with `--no-index --find-links=<wheelhouse>`.
4. clear source checkout `PYTHONPATH` and equivalent editable-install paths.
5. import, generate, host, call, hold, invalidate, and shut down through the
   installed packages.

The local FastDB wheel may still carry `0.1.22`; its commit and artifact hash
make clear that it is not the older registry artifact with the same metadata.

### 14.3 TypeScript

1. build FastDB TypeScript/Wasm;
2. package the C-Two Node memory/IPC runtime and its bundled native artifact;
3. create npm tarballs with `npm pack`;
4. install only those tarballs in a clean Node project;
5. generate the C-Two contract tree through `c3`;
6. compile without TypeScript path aliases to sibling source;
7. execute the real-call gates in section 16.

## 15. Portable-payload 18-row matrix

The exact Cartesian product is:

```text
payload:
  no-payload
  record.v1
  object_graph.v1

direction:
  Rust client -> Rust host
  Rust client -> Python host
  Python client -> Rust host

transport:
  direct IPC
  explicit relay
```

This yields exactly 18 required rows.

### 15.1 Row identity

Every row has a stable ID such as:

```text
record-v1__rust-client__python-host__relay
```

Parameterized test output and `portable-matrix-receipt.v1` enumerate all 18
expected IDs. A verifier rejects missing, duplicate, skipped, xfailed, or
unexpected rows.

### 15.2 Real path requirements

- Rust rows use the installed Rust SDK and generated contract surface, not the
  existing low-level fixture assembly.
- Python rows use the installed Python SDK.
- Every relay row starts a real `c3 relay`, registers a route, performs
  contract-scoped resolution, and sends the data-plane call through explicit
  HTTP relay.
- Relay matrix rows do not use relay-aware local IPC short-circuit.
- Direct and relay rows use the same descriptor bytes, release identity,
  FastDB spec digest, inputs, and logical expected results.

### 15.3 Data coverage

The record and graph fixtures jointly cover:

- `str`;
- `wstr`;
- bytes;
- nested lists;
- null values;
- empty values;
- shared references;
- self-cycle;
- mutual cycle.

Object-graph logical assertions use FastDB's official graph APIs rather than
inventing a C-Two JSON graph representation.

### 15.4 Negative and lifetime evidence

Targeted matrix-adjacent tests cover both direct and relay behavior:

- route mismatch;
- contract fingerprint mismatch;
- FastDB digest mismatch;
- route disappearance;
- retained `ContractReleaseRef` verification after route disappearance;
- borrowed input invalidation after callback;
- held result invalidation before lease release;
- owned result response-lease release;
- materialized values surviving owner invalidation.

The negative suite does not need to multiply every failure across all 18
positive rows, but each asserted semantic boundary must have Rust/Python and
direct/relay evidence where the boundary is transport- or language-sensitive.

### 15.5 Receipt

Each successful row records:

```text
row ID
client and host language
transport and observed path
descriptor SHA-256
ContractReleaseRef
FastDB spec SHA-256
route UID/revision
logical result receipt
client/host package hashes
relay/c3 binary hash when applicable
status
```

One broad test process exit code cannot substitute for the row receipt.

## 16. TypeScript real-call closure

The generated Node surface currently declares:

- direct IPC;
- explicit HTTP relay;
- relay-aware resolution/connection.

The local candidate must run real no-payload, record, and object-graph client
calls over each declared mode. At least one Rust host and one Python host are
covered across the TypeScript suite.

### 16.1 Transport evidence

- Direct IPC connects to a real route-bound C-Two server.
- Explicit relay starts a real `c3 relay`, registers the host, resolves the
  contract, and sends the call through the relay data plane.
- Relay-aware evidence distinguishes a verified local IPC selection from an
  HTTP relay selection and proves same-path fallback denial.
- Path counters or equivalent observable receipts prevent a typecheck or an
  accidental direct shortcut from being reported as relay evidence.

### 16.2 Payload and ownership evidence

- generated artifacts come from the same `c3` composition path as Rust and
  Python;
- the installed `fastdb4ts/payload` package owns payload semantics;
- no-payload, record, and object-graph calls execute;
- wrong FastDB digest is rejected structurally;
- response close/release is idempotent;
- checked views fail after invalidation;
- materialized values remain valid;
- an opaque response allocator without a public byte-visible view is rejected
  and released.

C-Two does not inspect private provider fields or invent a FastDB backing to
make an unsupported allocator appear successful.

### 16.3 Platform claim

This goal proves the generated Node role on the audited platform. It does not
promote that evidence into a browser-runtime support claim. Browser packaging,
transport, lifecycle, and real-browser proof remain an owner Issue.

If a currently declared Node transport cannot execute against a real host,
that is a release-candidate blocker. It is not replaced by a simulated
transport or omitted silently.

## 17. Strict engineering gates

### 17.1 Clippy

The current strict baseline is known to fail in both the core workspace and
Python native crate. The implementation closes those findings with scoped
code changes:

```text
cargo clippy --manifest-path core/Cargo.toml \
  --workspace --all-targets --all-features -- -D warnings

cargo clippy --manifest-path sdk/python/native/Cargo.toml \
  --all-targets --no-deps -- -D warnings
```

The Rust SDK and CLI receive equivalent format, strict Clippy, and complete
test gates.

Blanket `allow` is forbidden. A local FFI or generated-code allowance requires
a narrow lint name and adjacent rationale.

### 17.2 Full gates

The final candidate records:

- C-Two Rust format;
- core complete strict Clippy and tests;
- CLI format, strict Clippy, and tests;
- Rust SDK format, strict Clippy, tests, examples, and package consumer;
- Python native format/check/strict Clippy;
- complete Python suite;
- CPython 3.10 syntax/runtime gates;
- 18/18 matrix;
- generated TypeScript compile, package, and real-call gates;
- package inventories and isolated consumer gates;
- documentation links/data/current-semantics checks;
- `git diff --check`.

FastDB Core need not rerun unrelated P1-P5 work while its source remains
frozen. Any FastDB package-seam production change triggers the affected C++,
Rust, Python, TypeScript, and Wasm gates required by FastDB's repository
rules.

Toodle receives its required Rust and documentation gates after tracking
updates; it receives no production integration in this goal.

### 17.3 Evidence freshness

No pre-change command result proves the final commit. A retained result from a
frozen unchanged revision is labeled as retained evidence; every modified
surface is tested again after its final change.

### 17.4 Disk and process discipline

Before each large FastDB, Wasm, Rust, or wheel build, the implementation:

- records available disk space;
- checks for active builds in all three repositories;
- runs large build families serially;
- records hashes before deleting rebuildable outputs;
- preserves source, tracked files, and development environments;
- never deletes `fastcarto/fastdb/src/payload/build`, which is source;
- rechecks all three worktrees after cleanup.

## 18. Cross-repository handoff

### 18.1 FastDB

After final package and consumer evidence, update:

- `README.md`;
- `docs/issues/0002-portable-payload-foundation-implementation-status.md`;
- the relevant issue index and manifest.

The handoff states that FastDB P5 remains frozen, records the local C-Two SDK,
18-row, TypeScript, and package results, and states the exact hosted, version,
push, tag, publish, and release status. It removes the obsolete current claim
that no C-Two composition proof exists without rewriting historical evidence.

### 18.2 C-Two

Update:

- `docs/roadmap.md`;
- the current design/plan indexes;
- `docs/issues/contract-release-deferred-capabilities.md`;
- `README.md`;
- `AGENTS.md`;
- the retained local release-candidate report and receipts.

Rows close only when their executable exit criteria are satisfied. Remaining
limitations retain reason, impact, owner, dependency, and exit condition.

### 18.3 Toodle

Update tracking only:

- `docs/11-development-roadmap.md`;
- `docs/13-codex-first-slices.md`;
- `docs/14-upstream-boundaries.md`;
- TD-0002 and TD-0003;
- the issue index;
- `CHANGELOG.md`;
- `MANIFEST.json`.

The Toodle handoff distinguishes:

- FastDB owner implementation locally complete;
- C-Two Rust SDK/Core, 18-row, TypeScript, and package status supported by
  actual receipts;
- official immutable artifacts unpublished;
- Toodle `CRMContractRef` and `toodle-c2` unimplemented;
- Toodle Phase 0B incomplete until official artifacts are pinned and consumed.

No Toodle production code is added. Manifest verification checks path
uniqueness, actual bytes, and SHA-256 without assuming a global lexical order.

## 19. Implementation and review discipline

Implementation is divided into scoped commits for:

1. the accepted design;
2. `c2-core` clean cut and shared facade;
3. Rust SDK client/host/lifetime surface;
4. transport-neutral generated Rust clients/services;
5. structured error normalization and parity;
6. versioned `ContractLimits`;
7. strict Clippy closure;
8. 18-row matrix;
9. TypeScript real calls;
10. FastDB/C-Two package closure;
11. cross-repository documentation and retained report.

The later implementation plan gives every slice:

- a real failing test or package probe;
- focused green evidence;
- all affected full gates;
- one scoped commit;
- a same-agent specification/ownership review;
- a same-agent correctness, lifetime, portability, package, and maintenance
  review.

The reviews are explicitly same-agent evidence and are not represented as
independent review.

## 20. Rejected alternatives

### 20.1 Add only a Rust wrapper

Rejected because Python native would retain a second orchestration path for
server, pools, relay selection, and lifecycle. The two SDKs would drift even
if the wrapper looked ergonomic.

### 20.2 Keep `c2-runtime` and add a second `c2-core`

Rejected because wrapper layering would leave two plausible runtime
authorities. The 0.x clean cut evolves the existing owner into `c2-core`.

### 20.3 Put FastDB semantics in the Rust SDK or Core

Rejected because it would create a second parser/runtime authority and give
Rust capabilities that Python receives only through a different path.

### 20.4 Keep generated Rust tied to IPC

Rejected because generated contract code would encode transport policy and
could not use explicit relay or relay-aware clients through one stable
contract surface.

### 20.5 Retry after connection loss

Rejected because C-Two cannot prove that a non-idempotent call was not already
executed. Transport convenience cannot weaken execution semantics.

### 20.6 Validate packages through sibling paths

Rejected because source success does not prove package inventory, normalized
dependency metadata, FastDB Core linking, or isolated installation.

### 20.7 Use a simulated relay or TypeScript transport

Rejected because it proves only adapter typing. Real route registration,
resolution, data-plane behavior, lifecycle, and error propagation remain
untested.

### 20.8 Fold every Rust artifact into one root Cargo workspace

Rejected for this goal because Core, CLI, Python native, and Rust SDK have
different package/release roles, and versioned dependency closure is the
actual external-consumer requirement. A workspace migration remains possible
only if implementation produces a concrete ownership or reproducibility
defect that cannot be fixed at the package boundary.

## 21. Known limits and owner issues

The candidate continues to state:

- official FastDB/C-Two immutable packages are unpublished;
- local packages may retain metadata equal to older public artifacts and are
  disambiguated by commit and hash;
- Rust/Python portable receive is copy-backed;
- browser TypeScript runtime is not proven;
- C-Two C++ SDK does not exist;
- streaming RPC and post-dispatch retry/deduplication do not exist;
- contract compatibility remains exact release matching;
- Authority trust/signature/revocation remains outside C-Two;
- artifact publication hardening beyond the accepted new-tree boundary
  remains deferred;
- Toodle `CRMContractRef` and `toodle-c2` remain unimplemented.

The authoritative active limitation table is
[`contract-release deferred capabilities`](../../issues/contract-release-deferred-capabilities.md).
Implementation updates rows only when retained evidence meets their stated
exit condition; it does not delete rows to manufacture completion.

## 22. Completion criteria

This design is implemented only when all of the following are true:

1. FastDB Core remains the frozen sole payload semantic authority and ABI-117
   remains exact.
2. `c2-core` is the single language-neutral route/transport/runtime/lifecycle
   facade used by Rust and Python.
3. A real user-facing Rust SDK exists at `sdk/rust` with Cargo package
   `c-two` and import `c_two`.
4. Rust SDK client and host cover contract, direct IPC, explicit relay,
   relay-aware connection, lifecycle, and structured errors.
5. Generated Rust clients and hosts consume only the supported SDK seam plus
   the official FastDB crate.
6. Rust/Python parity has no unexplained portable capability difference.
7. `ContractLimits` is one Rust authority used by Rust, Python, CLI, and
   codegen and passes exact boundary tests.
8. The portable real-call matrix is exactly 18/18 with a complete receipt.
9. Generated TypeScript executes its declared Node transport/payload roles
   against real hosts.
10. Core and Python native strict Clippy pass without blanket suppression.
11. FastDB/C-Two package artifacts install and execute from isolated temporary
    consumers without sibling source.
12. Artifact manifests include inventory, bytes, SHA-256, source commit,
    toolchain, platform, and support boundaries.
13. FastDB, C-Two, and Toodle current documentation accurately distinguish
    local completion, official publication, and Toodle consumption.
14. Every required final gate passes on the final commits.
15. Three tracked worktrees are clean and rebuildable large outputs are
    removed.
16. The retained final report lists start/end commits, every scoped commit,
    the 18-row receipt, SDK/parity inventory, package manifests/hashes, fresh
    versus retained evidence, and every external action not performed.

The only permitted completion statement is:

> **FastDB/C-Two Phase 0B upstream local release candidate complete**

It must not be restated as “Toodle Phase 0B complete”, “official packages
released”, or “hosted matrix passed”.
