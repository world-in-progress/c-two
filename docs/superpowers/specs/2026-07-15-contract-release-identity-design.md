# C-Two Contract Release Identity Design

> **Historical foundation / partially superseded:** The route-independent release-identity mechanism remains current, but the descriptor shape below predates `c-two.contract.v2`. Use the [`2026-07-24 design`](./2026-07-24-portable-payload-contract-composition-design.md) and [`implementation plan`](../plans/2026-07-24-portable-payload-contract-composition.md) for current work.

**Date:** 2026-07-15
**Status:** Approved for implementation planning
**Scope:** Route-independent CRM contract release identity in C-Two, with Rust authority and Rust CLI/Python projections

## 1. Context

C-Two already owns the portable `c-two.contract.v1` descriptor, CRM route
fingerprints, strict descriptor validation, canonical JSON hashing, and runtime
`ExpectedRouteContract` matching. The current runtime object combines a route
name with CRM namespace, name, version, ABI hash, and signature hash. That is
correct for route acquisition and call admission, but it is not a persistent
contract release identity because route names belong to runtime instances.

Downstream catalogs and lockfiles need to name one immutable CRM contract
release before a route exists and after a route disappears. C-Two must define
that identity because C-Two owns CRM contract semantics. A downstream system
must not infer it from C-Two JSON, and C-Two must not import downstream
Authority, policy, storage, or trust objects to provide it.

## 2. Goals

1. Define a route-independent, content-addressed CRM contract release in the
   Rust `c2-contract` crate.
2. Make Rust the only authority for descriptor validation, canonicalization,
   digest calculation, release construction, and release-reference
   verification.
3. Provide a versioned language-neutral `ContractReleaseRef` wire shape for
   catalogs, lockfiles, Python, CLI, and future SDKs.
4. Derive the existing runtime `ExpectedRouteContract` from a validated release
   plus a route name.
5. Give Rust callers typed mismatch errors rather than requiring error-text
   parsing.
6. Prove the contract with shared Rust, Python, and CLI golden fixtures.
7. Correct C-Two documentation that still uses the old Toodle name or treats a
   Toodle Resource as equivalent to a live CRM route.

## 3. Non-Goals

- No contract registry, resolver, artifact store, or network fetch API.
- No publisher signature, Authority identity, trust decision, revocation, or
  policy object.
- No semantic-version range or compatibility negotiation.
- No Toodle types or dependencies in C-Two.
- No FastDB schema parsing or FastDB storage/runtime implementation in
  `c2-contract`.
- No complete Rust client/server SDK in this slice.
- No compatibility wrapper around an incorrect release model. C-Two is 0.x;
  the new model is the only release-identity surface introduced here.

## 4. Considered Approaches

### 4.1 Digest-only reference

A reference containing only the descriptor SHA-256 is compact and avoids
duplicated fields. It is insufficient as the public reference because a
catalog cannot identify the CRM contract without first resolving the complete
descriptor. It also makes diagnostics and indexing unnecessarily opaque.

### 4.2 Validated release plus self-describing reference

This is the selected approach. A `ContractRelease` owns one validated canonical
descriptor. A `ContractReleaseRef` carries the descriptor schema, verified CRM
identity, and descriptor digest. The digest is the content identity; the CRM
fields are verified self-description and indexing fields.

### 4.3 Signed release envelope

A signed envelope could assert publisher identity and trust. C-Two does not own
the Authority, key distribution, revocation, or policy decision required to
interpret such a signature. Adding that envelope here would cross the product
boundary, so signature and trust remain downstream concerns.

## 5. Core Object Model

The existing `c-two.contract.v1` descriptor is the immutable release content.
C-Two will not wrap it in a second `c-two.contract-release.v1` document. That
avoids two nested artifacts with competing digests.

### 5.1 `ValidatedContractDescriptor`

`ValidatedContractDescriptor` is produced only by parsing descriptor JSON
through the Rust validator. It owns:

- canonical compact descriptor JSON;
- descriptor schema;
- CRM namespace, name, and version;
- ABI hash and signature hash;
- canonical descriptor SHA-256.

Construction performs parsing, unknown-field rejection, portable descriptor
validation, canonicalization, and typed field extraction once. Callers cannot
construct it from independently supplied fields.

### 5.2 `ContractRelease`

`ContractRelease` wraps one `ValidatedContractDescriptor` and represents one
immutable content-addressed release. Its public behavior is:

```rust
impl ContractRelease {
    pub fn from_descriptor_json(bytes: &[u8]) -> Result<Self, ContractError>;
    pub fn canonical_descriptor_json(&self) -> &str;
    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest;
    pub fn reference(&self) -> ContractReleaseRef;
    pub fn expected_route(
        &self,
        route_name: impl Into<String>,
    ) -> Result<ExpectedRouteContract, ContractError>;
}
```

`expected_route(...)` is the only transition from release identity to the
existing runtime route contract. It injects the supplied route name and copies
the verified CRM identity and fingerprints from the release.

### 5.3 `ContractReleaseRef`

`ContractReleaseRef` is a strict, versioned, route-independent reference:

```json
{
  "schema": "c-two.contract-release-ref.v1",
  "contract_schema": "c-two.contract.v1",
  "crm": {
    "namespace": "demo.grid",
    "name": "Grid",
    "version": "0.1.0"
  },
  "descriptor_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

The wire object rejects unknown fields at the root and inside `crm`. The
descriptor digest is the release content identity. CRM identity fields are
extracted from and verified against the descriptor; they are not an
independent authority.

The reference deliberately omits:

- route name and route token;
- ABI hash and signature hash;
- Authority or publisher identity;
- storage location or resolver URL;
- signature or trust decision;
- compatibility range.

ABI and signature fingerprints remain projections of the full release. A
runtime that wants to call a route must resolve the descriptor, reconstruct the
release, verify the stored reference, and then derive an
`ExpectedRouteContract`.

### 5.4 Digest type

`ContractDescriptorDigest` is a nominal type that accepts exactly 64 lowercase
ASCII hexadecimal characters. It prevents descriptor identity from being
silently mixed with arbitrary strings. This slice does not change the existing
runtime route fingerprint field types because doing so would expand the change
into every transport and relay call path without improving release identity.

## 6. Module Boundaries

The implementation remains in the existing `c2-contract` crate.

- `descriptor.rs` owns validated descriptor parsing, typed field extraction,
  canonical descriptor JSON, and descriptor digest calculation.
- `release.rs` owns `ContractDescriptorDigest`, `ContractRelease`,
  `ContractReleaseRef`, reference JSON, verification, and runtime projection.
- `lib.rs` exports the public contract surface and retains route-specific
  validation APIs.

Existing validation helpers may move behind these modules where needed, but
the change must not refactor unrelated transport/runtime crates or create a new
placeholder crate.

## 7. Canonicalization and Identity

Rust owns canonicalization. `ContractRelease::from_descriptor_json(...)`
parses the input, validates it as `c-two.contract.v1`, produces compact
canonical JSON, and hashes those canonical bytes with SHA-256.

Whitespace, indentation, and JSON object key order therefore do not affect the
release identity. A semantic descriptor change does affect it. Pretty-printed
output is a presentation form, not the bytes used as identity.

The existing generic `contract_descriptor_sha256_hex(...)` remains available
because current Python descriptor construction also uses canonical JSON hashing
for ABI/signature substructures. Release construction does not rely on callers
invoking that helper first.

## 8. Rust, Python, and CLI Projections

### 8.1 Rust

Rust exposes the typed objects and methods from section 5. Existing
`validate_portable_contract_descriptor_json(...)` delegates to the same
validated descriptor path so release and standalone validation cannot drift.

`ContractReleaseRef` supports strict JSON parsing, canonical compact JSON
serialization, and verification against a `ContractRelease`:

```rust
impl ContractReleaseRef {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ContractError>;
    pub fn to_canonical_json(&self) -> Result<String, ContractError>;
    pub fn verify_release(
        &self,
        release: &ContractRelease,
    ) -> Result<(), ContractError>;
}
```

### 8.2 Python

Python continues to build a candidate descriptor from a CRM class, but it no
longer decides the compact canonical form. New native functions return the Rust
canonical descriptor and release-reference JSON.

The public Python facade adds:

```python
def export_contract_release_ref(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str: ...
```

`export_contract_descriptor(..., pretty=False)` also returns the Rust canonical
compact descriptor. Pretty output is produced only after Rust validation and
canonicalization. This slice does not add a freely constructible Python
`ContractRelease` dataclass, which would invite a second validation model.

### 8.3 CLI

The Rust CLI adds a language-neutral command:

```text
c3 contract release-ref DESCRIPTOR_PATH [--out PATH] [--pretty]
```

`DESCRIPTOR_PATH` accepts `-` for stdin. The command reads a descriptor,
constructs a Rust `ContractRelease`, and writes its reference. It does not
start Python.

`c3 contract export` remains the Python-reflection path from a CRM class to a
descriptor. `c3 contract validate` retains its current purpose and output.

## 9. Consumer Data Flow

```text
Python CRM class or descriptor file
        |
        v
ContractRelease::from_descriptor_json
        |-- strict descriptor validation
        |-- Rust canonicalization
        |-- descriptor SHA-256
        |-- CRM identity/fingerprint extraction
        `-- ContractReleaseRef derivation

Catalog or lockfile stores ContractReleaseRef
        |
        v
consumer-owned descriptor resolution by digest
        |
        v
ContractRelease reconstruction + ref verification
        |
        v
release.expected_route(route_name)
        |
        v
existing IPC/relay route acquire and call admission
```

C-Two does not specify how a consumer stores or resolves the descriptor. A
downstream catalog may embed it, store it in an object store, or use another
content-addressed mechanism, but it must pass the resolved bytes back through
C-Two validation before use.

## 10. Error Model

Rust callers must be able to branch on mismatch fields without parsing display
text. `ContractError` therefore gains typed release-reference errors:

- `InvalidReleaseRefJson(String)`;
- `InvalidReleaseRef { path, message }`;
- `ReleaseRefMismatch { field, expected, actual }`.

The mismatch field is an enum covering:

- contract schema;
- CRM namespace;
- CRM name;
- CRM version;
- descriptor SHA-256.

Descriptor validation continues to use its existing typed variants. Python
maps release construction/reference failures to `ValueError`. The CLI reports
the failing path or mismatch field and exits non-zero.

## 11. Test Design

One repository-level fixture set is consumed by Rust, Python, and CLI tests. It
contains a portable descriptor, its exact canonical descriptor, its fixed
descriptor digest, and its canonical release reference. The fixture includes a
no-payload method and a FastDB call-db method so the test proves both supported
portable wire shapes without teaching `c2-contract` FastDB storage semantics.

Required tests:

1. JSON key order, whitespace, and indentation variants yield the same
   canonical descriptor, digest, and release reference.
2. A method, wire ref, or other descriptor content change changes the digest.
3. Wrong contract schema, CRM namespace/name/version, or descriptor digest
   yields the exact `ReleaseRefMismatch` field.
4. Unknown reference fields, malformed nested CRM shape, wrong reference
   schema, and non-lowercase/non-64-byte digest are rejected.
5. A no-payload descriptor creates a release and reference.
6. A FastDB call-db codec ref remains opaque to `c2-contract` while its
   declared schema/hash are covered by the descriptor digest.
7. `expected_route(...)` copies exact CRM identity and fingerprints and adds
   only the supplied route name.
8. Release/reference JSON contains no route, Authority, storage URL, signature,
   or compatibility-range fields.
9. Python compact descriptor output exactly equals Rust canonical output;
   pretty output reconstructs the same release reference.
10. Python release-ref export returns the shared canonical golden reference.
11. CLI path, stdin, `--out`, `--pretty`, and invalid descriptor behavior are
    covered.
12. Boundary tests prevent production Python code from independently hashing
    or validating release-reference fields.

## 12. Documentation Alignment

The implementation change updates:

- `AGENTS.md` and README to distinguish a persistent contract release from a
  runtime route contract;
- `docs/roadmap.md` so release identity is part of the current contract
  foundation without claiming the complete Rust SDK is implemented;
- `docs/vision/endgame-architecture.md` to replace the obsolete
  “Tag-Oriented Open Data Linking Environment” name and the incorrect equation
  of a Toodle Resource with a live CRM instance.

C-Two documentation states only the integration boundary. Toodle is a broader
Trust-Oriented Open Distributed Linking Environment that owns Resource
identity, revision, resource graph and optional tree views, policy, Resource
Service, activation, and federation. C-Two owns CRM contracts and runtime
mechanisms. C-Two does not copy or redefine the complete Toodle product model.

## 13. Explicitly Deferred Capabilities

`docs/issues/contract-release-deferred-capabilities.md` records every known
limitation with current state, reason, impact, owner, and exit criteria:

- semantic-version/range compatibility;
- release resolver, registry, and storage;
- publisher signature, Authority trust, and revocation;
- complete Rust client/server SDK;
- FastDB Rust call-db runtime;
- Rust/Python bidirectional CRM interoperability proof.

The issue remains open after this slice. Implemented release identity must not
be described as solving any of these capabilities.

## 14. Acceptance Criteria

1. A Rust caller can load one `c-two.contract.v1` descriptor into a typed
   `ContractRelease`, obtain a deterministic `ContractReleaseRef`, serialize
   it, parse it, and verify it against the release.
2. No release/ref field is independently supplied to `ContractRelease`.
3. A release ref never contains route identity or downstream trust/storage
   fields.
4. Rust, Python, and CLI agree on the same golden reference bytes.
5. Python compact descriptor canonicalization is Rust-owned.
6. Existing route validation and contract export behavior remain covered.
7. C-Two documentation uses the current Toodle name and does not reduce a
   Toodle Resource to a live CRM instance.
8. Deferred capabilities are explicitly tracked rather than represented by
   placeholder APIs.
9. The work is committed on `dev-feature` and passes the Rust workspace,
   focused Python contract tests, CLI tests, formatting, and diff checks.
