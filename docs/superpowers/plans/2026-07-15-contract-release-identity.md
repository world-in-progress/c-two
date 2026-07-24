# Contract Release Identity Implementation Plan

> **Historical foundation / partially superseded:** The route-independent release-identity mechanism remains current, but the descriptor shape and implementation sequence below predate `c-two.contract.v2`. Use the [`2026-07-24 design`](../specs/2026-07-24-portable-payload-contract-composition-design.md) and [`implementation plan`](./2026-07-24-portable-payload-contract-composition.md) for current work.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a route-independent, content-addressed C-Two CRM contract release identity with one Rust authority and matching Rust CLI/Python projections.

**Architecture:** Keep `c-two.contract.v1` as the immutable release content. `c2-contract` parses and canonicalizes it into `ValidatedContractDescriptor`, wraps it as `ContractRelease`, derives a strict `c-two.contract-release-ref.v1`, verifies stored refs, and adds a route name only when projecting to `ExpectedRouteContract`. Python and CLI pass candidate descriptor bytes to this Rust path and never reimplement release validation or digest construction.

**Tech Stack:** Rust 2024, `serde_json`, `sha2`, `thiserror`, PyO3 0.29, Python 3.10+, Clap 4, pytest, assert_cmd.

## Global Constraints

- Work only on the C-Two `dev-feature` branch.
- Follow `docs/superpowers/specs/2026-07-15-contract-release-identity-design.md` as the approved source of truth.
- Do not introduce Toodle types, Authority policy, signatures, resolver/storage behavior, semver ranges, FastDB runtime parsing, or a placeholder Rust SDK.
- The descriptor digest is the release content identity; CRM identity in the ref is verified self-description.
- `route_name`, route token, ABI hash, and signature hash do not enter `ContractReleaseRef`.
- Rust owns descriptor validation, canonical descriptor bytes, release digest, reference construction, and reference verification.
- Every production-code behavior starts with a focused failing test and an observed RED result.
- Keep each task in one atomic commit and run `git diff --check` before committing.

---

## File Structure

### Shared fixtures

- `tests/fixtures/contracts/portable-release.contract.json`: readable source descriptor containing one no-payload method and one opaque FastDB call-db method.
- `tests/fixtures/contracts/portable-release.canonical.json`: exact compact Rust canonical descriptor.
- `tests/fixtures/contracts/portable-release.ref.json`: exact compact canonical release reference.

### Rust authority

- `core/foundation/c2-contract/src/descriptor.rs`: descriptor validation, canonical JSON, typed extraction, and `ValidatedContractDescriptor`.
- `core/foundation/c2-contract/src/release.rs`: `ContractDescriptorDigest`, `ContractRelease`, `ContractReleaseRef`, mismatch fields, strict reference parsing, verification, and runtime projection.
- `core/foundation/c2-contract/src/lib.rs`: route-specific types/errors, module declarations, and public re-exports.
- `core/foundation/c2-contract/tests/descriptor.rs`: validated descriptor and canonicalization integration tests.
- `core/foundation/c2-contract/tests/release.rs`: release/ref golden, strictness, mismatch, and route-projection integration tests.

### Python projection

- `sdk/python/native/src/wire_ffi.rs`: thin PyO3 functions backed by `ContractRelease`.
- `sdk/python/src/c_two/crm/descriptor.py`: public descriptor/ref export facade.
- `sdk/python/src/c_two/crm/__init__.py`: CRM package export.
- `sdk/python/src/c_two/__init__.py`: top-level `cc.export_contract_release_ref` export.
- `sdk/python/tests/unit/test_contract_export.py`: public and native golden tests.
- `sdk/python/tests/unit/test_sdk_boundary.py`: guardrail against Python-owned release identity.

### CLI projection

- `cli/src/contract.rs`: `c3 contract release-ref` command.
- `cli/tests/contract_commands.rs`: help, path, stdin, output, pretty, and invalid-input tests.

### Documentation

- `docs/issues/contract-release-deferred-capabilities.md`: limitations with reason, impact, owner, and exit criteria.
- `AGENTS.md`, `README.md`, `docs/roadmap.md`, `docs/vision/endgame-architecture.md`: current ownership and Toodle-boundary wording.

---

### Task 1: Centralize Validated Descriptor Authority

**Files:**
- Create: `tests/fixtures/contracts/portable-release.contract.json`
- Create: `tests/fixtures/contracts/portable-release.canonical.json`
- Create: `core/foundation/c2-contract/src/descriptor.rs`
- Create: `core/foundation/c2-contract/src/release.rs`
- Create: `core/foundation/c2-contract/tests/descriptor.rs`
- Modify: `core/foundation/c2-contract/src/lib.rs:1-644`

**Interfaces:**
- Consumes: existing `ContractError`, `validate_crm_tag`, `validate_contract_text_field`, `PORTABLE_CONTRACT_SCHEMA`.
- Produces: `ContractDescriptorDigest` and `ValidatedContractDescriptor::from_json`, plus existing descriptor functions re-exported without call-site changes.

- [ ] **Step 1: Add the readable shared descriptor fixture**

Create `tests/fixtures/contracts/portable-release.contract.json` with this exact JSON:

```json
{
  "schema": "c-two.contract.v1",
  "crm": {
    "namespace": "test.contract-release",
    "name": "Portable",
    "version": "0.1.0"
  },
  "fingerprints": {
    "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
  },
  "methods": [
    {
      "access": "read",
      "buffer": "view",
      "name": "ping",
      "parameters": [],
      "return": {"kind": "none"},
      "wire": {"input": null, "output": null}
    },
    {
      "access": "write",
      "buffer": "view",
      "name": "echo",
      "parameters": [
        {
          "name": "value",
          "kind": "POSITIONAL_OR_KEYWORD",
          "default": {"kind": "missing"},
          "type": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.fastdb.call-db",
              "version": "1",
              "schema": "fastdb.call-db.schema.v1",
              "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "capabilities": ["bytes", "buffer-view"],
              "portable": true
            }
          }
        }
      ],
      "return": {
        "kind": "codec",
        "codec": {
          "kind": "codec_ref",
          "id": "org.fastdb.call-db",
          "version": "1",
          "schema": "fastdb.call-db.schema.v1",
          "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "capabilities": ["bytes", "buffer-view"],
          "portable": true
        }
      },
      "wire": {
        "input": {
          "kind": "codec_ref",
          "id": "org.fastdb.call-db",
          "version": "1",
          "schema": "fastdb.call-db.schema.v1",
          "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "capabilities": ["bytes", "buffer-view"],
          "portable": true
        },
        "output": {
          "kind": "codec_ref",
          "id": "org.fastdb.call-db",
          "version": "1",
          "schema": "fastdb.call-db.schema.v1",
          "schema_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "capabilities": ["bytes", "buffer-view"],
          "portable": true
        }
      }
    }
  ]
}
```

Create `tests/fixtures/contracts/portable-release.canonical.json` with this exact
single line and no trailing newline:

```json
{"crm":{"name":"Portable","namespace":"test.contract-release","version":"0.1.0"},"fingerprints":{"abi_hash":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","signature_hash":"abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"},"methods":[{"access":"read","buffer":"view","name":"ping","parameters":[],"return":{"kind":"none"},"wire":{"input":null,"output":null}},{"access":"write","buffer":"view","name":"echo","parameters":[{"default":{"kind":"missing"},"kind":"POSITIONAL_OR_KEYWORD","name":"value","type":{"codec":{"capabilities":["bytes","buffer-view"],"id":"org.fastdb.call-db","kind":"codec_ref","portable":true,"schema":"fastdb.call-db.schema.v1","schema_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","version":"1"},"kind":"codec"}}],"return":{"codec":{"capabilities":["bytes","buffer-view"],"id":"org.fastdb.call-db","kind":"codec_ref","portable":true,"schema":"fastdb.call-db.schema.v1","schema_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","version":"1"},"kind":"codec"},"wire":{"input":{"capabilities":["bytes","buffer-view"],"id":"org.fastdb.call-db","kind":"codec_ref","portable":true,"schema":"fastdb.call-db.schema.v1","schema_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","version":"1"},"output":{"capabilities":["bytes","buffer-view"],"id":"org.fastdb.call-db","kind":"codec_ref","portable":true,"schema":"fastdb.call-db.schema.v1","schema_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","version":"1"}}}],"schema":"c-two.contract.v1"}
```

Its SHA-256 must be:

```text
cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867
```

- [ ] **Step 2: Write the failing descriptor integration tests**

Create `core/foundation/c2-contract/tests/descriptor.rs`:

```rust
use c2_contract::ValidatedContractDescriptor;

const SOURCE: &str = include_str!(
    "../../../../tests/fixtures/contracts/portable-release.contract.json"
);
const CANONICAL: &str = include_str!(
    "../../../../tests/fixtures/contracts/portable-release.canonical.json"
);
const DIGEST: &str = "cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867";

#[test]
fn validated_descriptor_extracts_identity_and_canonical_content() {
    let descriptor = ValidatedContractDescriptor::from_json(SOURCE.as_bytes()).unwrap();

    assert_eq!(descriptor.canonical_json(), CANONICAL.trim_end());
    assert_eq!(descriptor.contract_schema(), "c-two.contract.v1");
    assert_eq!(descriptor.crm_namespace(), "test.contract-release");
    assert_eq!(descriptor.crm_name(), "Portable");
    assert_eq!(descriptor.crm_version(), "0.1.0");
    assert_eq!(descriptor.abi_hash(), "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    assert_eq!(descriptor.signature_hash(), "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
    assert_eq!(descriptor.descriptor_sha256().as_str(), DIGEST);
}

#[test]
fn formatting_and_key_order_do_not_change_descriptor_identity() {
    let source = ValidatedContractDescriptor::from_json(SOURCE.as_bytes()).unwrap();
    let reordered = format!(
        "{{\"methods\":{},\"fingerprints\":{},\"crm\":{},\"schema\":\"c-two.contract.v1\"}}",
        serde_json::from_str::<serde_json::Value>(SOURCE).unwrap()["methods"],
        serde_json::from_str::<serde_json::Value>(SOURCE).unwrap()["fingerprints"],
        serde_json::from_str::<serde_json::Value>(SOURCE).unwrap()["crm"],
    );
    let reordered = ValidatedContractDescriptor::from_json(reordered.as_bytes()).unwrap();

    assert_eq!(source.canonical_json(), reordered.canonical_json());
    assert_eq!(source.descriptor_sha256(), reordered.descriptor_sha256());
}

#[test]
fn descriptor_content_change_changes_digest() {
    let original = ValidatedContractDescriptor::from_json(SOURCE.as_bytes()).unwrap();
    let changed = SOURCE.replace("\"name\": \"ping\"", "\"name\": \"health\"");
    let changed = ValidatedContractDescriptor::from_json(changed.as_bytes()).unwrap();

    assert_ne!(original.descriptor_sha256(), changed.descriptor_sha256());
}
```

- [ ] **Step 3: Run the descriptor test and observe RED**

Run:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-contract --test descriptor
```

Expected: compilation fails because `c2_contract::ValidatedContractDescriptor` does not exist.

- [ ] **Step 4: Introduce the nominal digest and validated descriptor modules**

Create `release.rs` initially with the digest type:

```rust
use crate::{ContractError, validate_contract_hash};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContractDescriptorDigest(String);

impl ContractDescriptorDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        validate_contract_hash("descriptor_sha256", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContractDescriptorDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ContractDescriptorDigest {
    type Err = ContractError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}
```

Create `descriptor.rs` with this public surface:

```rust
use crate::{ContractDescriptorDigest, ContractError, PORTABLE_CONTRACT_SCHEMA};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedContractDescriptor {
    canonical_json: String,
    contract_schema: String,
    crm_namespace: String,
    crm_name: String,
    crm_version: String,
    abi_hash: String,
    signature_hash: String,
    descriptor_sha256: ContractDescriptorDigest,
}

impl ValidatedContractDescriptor {
    pub fn from_json(json_bytes: &[u8]) -> Result<Self, ContractError> {
        let value: Value = serde_json::from_slice(json_bytes)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        validate_portable_contract_descriptor_value(&value)?;
        let canonical_json = canonical_json(&value);
        let root = object_at(&value, "$")?;
        let crm = object_at(required(root, "$", "crm")?, "$.crm")?;
        let fingerprints = object_at(
            required(root, "$", "fingerprints")?,
            "$.fingerprints",
        )?;
        let descriptor_sha256 = ContractDescriptorDigest::parse(
            sha256_hex(canonical_json.as_bytes()),
        )?;
        Ok(Self {
            canonical_json,
            contract_schema: string_at(
                required(root, "$", "schema")?,
                "$.schema",
            )?.to_string(),
            crm_namespace: string_at(
                required(crm, "$.crm", "namespace")?,
                "$.crm.namespace",
            )?.to_string(),
            crm_name: string_at(
                required(crm, "$.crm", "name")?,
                "$.crm.name",
            )?.to_string(),
            crm_version: string_at(
                required(crm, "$.crm", "version")?,
                "$.crm.version",
            )?.to_string(),
            abi_hash: string_at(
                required(fingerprints, "$.fingerprints", "abi_hash")?,
                "$.fingerprints.abi_hash",
            )?.to_string(),
            signature_hash: string_at(
                required(fingerprints, "$.fingerprints", "signature_hash")?,
                "$.fingerprints.signature_hash",
            )?.to_string(),
            descriptor_sha256,
        })
    }

    pub fn canonical_json(&self) -> &str { &self.canonical_json }
    pub fn contract_schema(&self) -> &str { &self.contract_schema }
    pub fn crm_namespace(&self) -> &str { &self.crm_namespace }
    pub fn crm_name(&self) -> &str { &self.crm_name }
    pub fn crm_version(&self) -> &str { &self.crm_version }
    pub fn abi_hash(&self) -> &str { &self.abi_hash }
    pub fn signature_hash(&self) -> &str { &self.signature_hash }
    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest { &self.descriptor_sha256 }
}
```

Move the existing descriptor-specific functions from `lib.rs` into
`descriptor.rs` without behavior changes:

- `contract_descriptor_sha256_hex`;
- `validate_portable_contract_descriptor_json`;
- `validate_portable_contract_descriptor_value`;
- `validate_fingerprints` through `validate_buffer`;
- JSON object/array/string helpers;
- descriptor `invalid(...)`;
- `canonical_json` and lowercase SHA-256 rendering.

Change `validate_portable_contract_descriptor_json(...)` to call
`ValidatedContractDescriptor::from_json(json_bytes).map(|_| ())`. Keep
`validate_portable_contract_descriptor_value(...)` for callers that already
hold a `serde_json::Value`. Make `canonical_json(...)` and `sha256_hex(...)`
`pub(crate)` for `release.rs`.

In `lib.rs`, add module declarations and re-exports:

```rust
mod descriptor;
mod release;

pub use descriptor::{
    ValidatedContractDescriptor,
    contract_descriptor_sha256_hex,
    validate_portable_contract_descriptor_json,
    validate_portable_contract_descriptor_value,
};
pub use release::ContractDescriptorDigest;
```

Keep route validators, `ExpectedRouteContract`, `ContractError`, constants, and
their existing tests in `lib.rs`.

- [ ] **Step 5: Run focused and existing Rust tests GREEN**

Run:

```bash
cargo fmt --manifest-path core/Cargo.toml --all
cargo test --manifest-path core/Cargo.toml -p c2-contract
```

Expected: descriptor integration tests and all pre-existing `c2-contract` tests pass.

- [ ] **Step 6: Commit the descriptor authority**

```bash
git add tests/fixtures/contracts/portable-release.contract.json \
  tests/fixtures/contracts/portable-release.canonical.json \
  core/foundation/c2-contract/src/lib.rs \
  core/foundation/c2-contract/src/descriptor.rs \
  core/foundation/c2-contract/src/release.rs \
  core/foundation/c2-contract/tests/descriptor.rs
git diff --cached --check
git commit -m "refactor: centralize validated contract descriptors"
```

---

### Task 2: Add Contract Release and Strict Release References

**Files:**
- Create: `tests/fixtures/contracts/portable-release.ref.json`
- Create: `core/foundation/c2-contract/tests/release.rs`
- Modify: `core/foundation/c2-contract/src/release.rs`
- Modify: `core/foundation/c2-contract/src/lib.rs`

**Interfaces:**
- Consumes: `ValidatedContractDescriptor`, `ContractDescriptorDigest`, existing route validation.
- Produces: `CONTRACT_RELEASE_REF_SCHEMA`, `ContractRelease`, `ContractReleaseRef`, `ContractReleaseRefField`, typed release-ref errors, and runtime projection.

- [ ] **Step 1: Add the exact canonical reference fixture**

Create `tests/fixtures/contracts/portable-release.ref.json` as this single line:

```json
{"contract_schema":"c-two.contract.v1","crm":{"name":"Portable","namespace":"test.contract-release","version":"0.1.0"},"descriptor_sha256":"cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867","schema":"c-two.contract-release-ref.v1"}
```

- [ ] **Step 2: Write failing release/ref tests**

Create `core/foundation/c2-contract/tests/release.rs` with focused tests covering
the approved public API:

```rust
use c2_contract::{
    ContractError, ContractRelease, ContractReleaseRef, ContractReleaseRefField,
    CONTRACT_RELEASE_REF_SCHEMA,
};

const DESCRIPTOR: &str = include_str!(
    "../../../../tests/fixtures/contracts/portable-release.contract.json"
);
const REFERENCE: &str = include_str!(
    "../../../../tests/fixtures/contracts/portable-release.ref.json"
);

#[test]
fn release_derives_the_golden_route_independent_reference() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let reference = release.reference();

    assert_eq!(reference.schema(), CONTRACT_RELEASE_REF_SCHEMA);
    assert_eq!(reference.contract_schema(), "c-two.contract.v1");
    assert_eq!(reference.crm_namespace(), "test.contract-release");
    assert_eq!(reference.crm_name(), "Portable");
    assert_eq!(reference.crm_version(), "0.1.0");
    assert_eq!(reference.to_canonical_json().unwrap(), REFERENCE.trim_end());
    let encoded = reference.to_canonical_json().unwrap();
    for forbidden in [
        "route_name",
        "route_token",
        "abi_hash",
        "signature_hash",
        "authority",
        "publisher",
        "storage_url",
        "resolver_url",
        "signature",
        "compatibility_range",
    ] {
        assert!(!encoded.contains(forbidden), "unexpected field: {forbidden}");
    }
}

#[test]
fn release_reference_round_trips_and_verifies() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let reference = ContractReleaseRef::from_json(REFERENCE.as_bytes()).unwrap();

    reference.verify_release(&release).unwrap();
    assert_eq!(reference, release.reference());
}

#[test]
fn reference_parser_rejects_unknown_fields_and_bad_digest_text() {
    let unknown = REFERENCE.trim_end().replace(
        "\"schema\":\"c-two.contract-release-ref.v1\"",
        "\"extra\":true,\"schema\":\"c-two.contract-release-ref.v1\"",
    );
    assert!(matches!(
        ContractReleaseRef::from_json(unknown.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.extra"
    ));

    let uppercase = REFERENCE.trim_end().replace(
        "cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867",
        "CFE58B74B47EFD2A866120CCE103049C68284DDE2CF66CFB95CCE920AD9C9867",
    );
    assert!(matches!(
        ContractReleaseRef::from_json(uppercase.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. })
            if path == "$.descriptor_sha256"
    ));
}

#[test]
fn verification_identifies_each_mismatch_field() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    for (needle, replacement, field) in [
        ("c-two.contract.v1", "c-two.contract.v2", ContractReleaseRefField::ContractSchema),
        ("test.contract-release", "test.other", ContractReleaseRefField::CrmNamespace),
        ("Portable", "Other", ContractReleaseRefField::CrmName),
        ("0.1.0", "0.2.0", ContractReleaseRefField::CrmVersion),
        (
            "cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ContractReleaseRefField::DescriptorSha256,
        ),
    ] {
        let candidate = REFERENCE.trim_end().replacen(needle, replacement, 1);
        let reference = ContractReleaseRef::from_json(candidate.as_bytes()).unwrap();
        assert!(matches!(
            reference.verify_release(&release),
            Err(ContractError::ReleaseRefMismatch { field: actual, .. }) if actual == field
        ));
    }
}

#[test]
fn release_projects_verified_runtime_contract_only_after_route_is_supplied() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let expected = release.expected_route("catalog/grid").unwrap();

    assert_eq!(expected.route_name, "catalog/grid");
    assert_eq!(expected.crm_ns, "test.contract-release");
    assert_eq!(expected.crm_name, "Portable");
    assert_eq!(expected.crm_ver, "0.1.0");
    assert_eq!(expected.abi_hash, "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    assert_eq!(expected.signature_hash, "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
}
```

Add these exact strictness and derivation tests to the same file:

```rust
#[test]
fn reference_parser_rejects_malformed_wrong_schema_unknown_nested_and_missing() {
    assert!(matches!(
        ContractReleaseRef::from_json(br#"{"#),
        Err(ContractError::InvalidReleaseRefJson(_))
    ));

    let wrong_schema = REFERENCE.trim_end().replace(
        "c-two.contract-release-ref.v1",
        "c-two.contract-release-ref.v2",
    );
    assert!(matches!(
        ContractReleaseRef::from_json(wrong_schema.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.schema"
    ));

    let unknown_nested = REFERENCE.trim_end().replace(
        "\"version\":\"0.1.0\"",
        "\"extra\":true,\"version\":\"0.1.0\"",
    );
    assert!(matches!(
        ContractReleaseRef::from_json(unknown_nested.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. }) if path == "$.crm.extra"
    ));

    let missing = REFERENCE.trim_end().replace(
        ",\"descriptor_sha256\":\"cfe58b74b47efd2a866120cce103049c68284dde2cf66cfb95cce920ad9c9867\"",
        "",
    );
    assert!(matches!(
        ContractReleaseRef::from_json(missing.as_bytes()),
        Err(ContractError::InvalidReleaseRef { path, .. })
            if path == "$.descriptor_sha256"
    ));
}

#[test]
fn release_rejects_invalid_runtime_route_text() {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    assert!(matches!(
        release.expected_route("bad\\route"),
        Err(ContractError::Separator { field: "route name" })
    ));
}

#[test]
fn descriptor_mutation_changes_the_derived_reference_digest() {
    let original = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let changed = DESCRIPTOR.replace("\"name\": \"ping\"", "\"name\": \"health\"");
    let changed = ContractRelease::from_descriptor_json(changed.as_bytes()).unwrap();

    assert_ne!(
        original.reference().descriptor_sha256(),
        changed.reference().descriptor_sha256(),
    );
}

#[test]
fn no_payload_release_is_valid_and_fastdb_codec_remains_digest_covered() {
    let mut no_payload: serde_json::Value = serde_json::from_str(DESCRIPTOR).unwrap();
    no_payload["methods"].as_array_mut().unwrap().truncate(1);
    ContractRelease::from_descriptor_json(no_payload.to_string().as_bytes()).unwrap();

    let original = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let changed = DESCRIPTOR.replacen(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        1,
    );
    let changed = ContractRelease::from_descriptor_json(changed.as_bytes()).unwrap();
    assert_ne!(original.descriptor_sha256(), changed.descriptor_sha256());
}
```

- [ ] **Step 3: Run the release test and observe RED**

Run:

```bash
cargo test --manifest-path core/Cargo.toml -p c2-contract --test release
```

Expected: compilation fails because `ContractRelease`, `ContractReleaseRef`,
`ContractReleaseRefField`, and `CONTRACT_RELEASE_REF_SCHEMA` do not exist.

- [ ] **Step 4: Add typed release-reference errors**

Add the mismatch-field type to `release.rs` beside `ContractReleaseRef`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContractReleaseRefField {
    ContractSchema,
    CrmNamespace,
    CrmName,
    CrmVersion,
    DescriptorSha256,
}

impl std::fmt::Display for ContractReleaseRefField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ContractSchema => "contract_schema",
            Self::CrmNamespace => "crm.namespace",
            Self::CrmName => "crm.name",
            Self::CrmVersion => "crm.version",
            Self::DescriptorSha256 => "descriptor_sha256",
        })
    }
}
```

Extend `ContractError` in `lib.rs` with:

```rust
#[error("contract release reference must be valid JSON: {0}")]
InvalidReleaseRefJson(String),
#[error("contract release reference invalid at {path}: {message}")]
InvalidReleaseRef { path: String, message: String },
#[error("contract release reference mismatch at {field}: expected {expected:?}, got {actual:?}")]
ReleaseRefMismatch {
    field: ContractReleaseRefField,
    expected: String,
    actual: String,
},
```

`lib.rs` re-exports this one definition from `release.rs`; do not add a second
root-level enum.

- [ ] **Step 5: Implement release and strict reference behavior**

Extend `release.rs` with these public types and constants:

```rust
pub const CONTRACT_RELEASE_REF_SCHEMA: &str = "c-two.contract-release-ref.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRelease {
    descriptor: ValidatedContractDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContractReleaseRef {
    contract_schema: String,
    crm_namespace: String,
    crm_name: String,
    crm_version: String,
    descriptor_sha256: ContractDescriptorDigest,
}
```

Implement the approved methods exactly:

```rust
impl ContractRelease {
    pub fn from_descriptor_json(bytes: &[u8]) -> Result<Self, ContractError> {
        Ok(Self {
            descriptor: ValidatedContractDescriptor::from_json(bytes)?,
        })
    }

    pub fn canonical_descriptor_json(&self) -> &str {
        self.descriptor.canonical_json()
    }

    pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest {
        self.descriptor.descriptor_sha256()
    }

    pub fn reference(&self) -> ContractReleaseRef {
        ContractReleaseRef {
            contract_schema: self.descriptor.contract_schema().to_string(),
            crm_namespace: self.descriptor.crm_namespace().to_string(),
            crm_name: self.descriptor.crm_name().to_string(),
            crm_version: self.descriptor.crm_version().to_string(),
            descriptor_sha256: self.descriptor.descriptor_sha256().clone(),
        }
    }

    pub fn expected_route(
        &self,
        route_name: impl Into<String>,
    ) -> Result<ExpectedRouteContract, ContractError> {
        let expected = ExpectedRouteContract {
            route_name: route_name.into(),
            crm_ns: self.descriptor.crm_namespace().to_string(),
            crm_name: self.descriptor.crm_name().to_string(),
            crm_ver: self.descriptor.crm_version().to_string(),
            abi_hash: self.descriptor.abi_hash().to_string(),
            signature_hash: self.descriptor.signature_hash().to_string(),
        };
        validate_expected_route_contract(&expected)?;
        Ok(expected)
    }
}
```

Implement `ContractReleaseRef::from_json(...)` with manual `serde_json::Value`
inspection so syntax errors map to `InvalidReleaseRefJson` and every structural
error carries a stable JSON path. Allow only root keys `schema`,
`contract_schema`, `crm`, `descriptor_sha256`; allow only CRM keys `namespace`,
`name`, `version`; require `schema == CONTRACT_RELEASE_REF_SCHEMA`. Parse
`contract_schema` as a non-empty C-Two contract text field, but do not require
it to equal `PORTABLE_CONTRACT_SCHEMA` during ref parsing: a syntactically valid
ref for a different descriptor schema must reach `verify_release(...)` and
return `ContractReleaseRefField::ContractSchema`. Descriptor construction still
accepts only `PORTABLE_CONTRACT_SCHEMA`. Validate each CRM field with the
existing C-Two contract-text validator. Remap every contract-schema/CRM
validation failure to `InvalidReleaseRef` at its exact JSON path; do not leak a
descriptor-oriented field label from the reusable validator. Validate the digest through
`ContractDescriptorDigest::parse(...)` and remap failures to path
`$.descriptor_sha256`.

Implement canonical reference JSON with a `serde_json::json!` value passed to
`descriptor::canonical_json(...)`. Add these accessors:

```rust
pub fn schema(&self) -> &str;
pub fn contract_schema(&self) -> &str;
pub fn crm_namespace(&self) -> &str;
pub fn crm_name(&self) -> &str;
pub fn crm_version(&self) -> &str;
pub fn descriptor_sha256(&self) -> &ContractDescriptorDigest;
pub fn to_canonical_json(&self) -> Result<String, ContractError>;
pub fn verify_release(&self, release: &ContractRelease) -> Result<(), ContractError>;
```

`schema()` returns `CONTRACT_RELEASE_REF_SCHEMA`; it is not stored as a mutable
field. `verify_release(...)` compares, in order, contract schema, CRM namespace,
CRM name, CRM version, and descriptor digest. Return the first typed
`ReleaseRefMismatch`, with the ref value in `expected` and the reconstructed
release value in `actual`.

Re-export from `lib.rs`:

```rust
pub use release::{
    CONTRACT_RELEASE_REF_SCHEMA,
    ContractDescriptorDigest,
    ContractRelease,
    ContractReleaseRef,
    ContractReleaseRefField,
};
```

- [ ] **Step 6: Run release and workspace tests GREEN**

Run:

```bash
cargo fmt --manifest-path core/Cargo.toml --all
cargo test --manifest-path core/Cargo.toml -p c2-contract
cargo test --manifest-path core/Cargo.toml --workspace
```

Expected: all release/ref, descriptor, existing contract, and dependent core
crate tests pass.

- [ ] **Step 7: Commit the Rust release identity**

```bash
git add tests/fixtures/contracts/portable-release.ref.json \
  core/foundation/c2-contract/src/lib.rs \
  core/foundation/c2-contract/src/release.rs \
  core/foundation/c2-contract/tests/release.rs
git diff --cached --check
git commit -m "feat: add contract release identity"
```

---

### Task 3: Project Release Identity Through Python Without a Second Authority

**Files:**
- Modify: `sdk/python/native/src/wire_ffi.rs:549-600`
- Modify: `sdk/python/src/c_two/crm/descriptor.py:325-349`
- Modify: `sdk/python/src/c_two/crm/__init__.py:4-11`
- Modify: `sdk/python/src/c_two/__init__.py:8-46`
- Modify: `sdk/python/tests/unit/test_contract_export.py`
- Modify: `sdk/python/tests/unit/test_sdk_boundary.py`

**Interfaces:**
- Consumes: `ContractRelease::from_descriptor_json`, canonical descriptor JSON, canonical ref JSON.
- Produces: native `canonicalize_portable_contract_descriptor`, native `contract_release_ref_json`, public `cc.export_contract_release_ref`.

- [ ] **Step 1: Write failing Python public/native projection tests**

Add to `test_contract_export.py`:

```python
from pathlib import Path


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[4]


def test_native_release_projection_matches_shared_golden_vectors():
    from c_two._native import (
        canonicalize_portable_contract_descriptor,
        contract_release_ref_json,
    )

    fixture_dir = _repo_root() / 'tests/fixtures/contracts'
    descriptor = (fixture_dir / 'portable-release.contract.json').read_bytes()
    canonical = (fixture_dir / 'portable-release.canonical.json').read_text().strip()
    reference = (fixture_dir / 'portable-release.ref.json').read_text().strip()

    assert canonicalize_portable_contract_descriptor(descriptor) == canonical
    assert contract_release_ref_json(descriptor) == reference


def test_export_contract_release_ref_is_rust_derived_and_route_independent():
    import fastdb4py as fdb

    @cc.crm(namespace='test.release-export', version='0.1.0')
    class Portable:
        def echo(self, value: fdb.I32) -> fdb.I32:
            ...

    descriptor = cc.export_contract_descriptor(Portable)
    reference = json.loads(cc.export_contract_release_ref(Portable))
    from c_two._native import contract_release_ref_json

    assert reference == json.loads(contract_release_ref_json(descriptor.encode()))
    assert reference['schema'] == 'c-two.contract-release-ref.v1'
    assert reference['contract_schema'] == 'c-two.contract.v1'
    assert reference['crm'] == {
        'name': 'Portable',
        'namespace': 'test.release-export',
        'version': '0.1.0',
    }
    assert 'route_name' not in reference
    assert 'abi_hash' not in reference
    assert 'signature_hash' not in reference


def test_pretty_descriptor_and_reference_preserve_release_identity():
    @cc.crm(namespace='test.release-pretty', version='0.1.0')
    class Ping:
        def ping(self) -> None:
            ...

    compact_descriptor = cc.export_contract_descriptor(Ping)
    pretty_descriptor = cc.export_contract_descriptor(Ping, pretty=True)
    compact_reference = cc.export_contract_release_ref(Ping)
    pretty_reference = cc.export_contract_release_ref(Ping, pretty=True)
    from c_two._native import contract_release_ref_json

    assert json.loads(compact_descriptor) == json.loads(pretty_descriptor)
    assert json.loads(compact_reference) == json.loads(pretty_reference)
    assert contract_release_ref_json(pretty_descriptor.encode()) == compact_reference
```

Add this source-boundary test to `test_sdk_boundary.py`:

```python
def test_contract_release_identity_is_not_reimplemented_in_python():
    source_path = (
        Path(__file__).resolve().parents[2]
        / 'src'
        / 'c_two'
        / 'crm'
        / 'descriptor.py'
    )
    tree = ast.parse(source_path.read_text(encoding='utf-8'))
    release_export = next(
        node
        for node in tree.body
        if isinstance(node, ast.FunctionDef)
        and node.name == 'export_contract_release_ref'
    )

    native_imports = {
        alias.name
        for node in ast.walk(release_export)
        if isinstance(node, ast.ImportFrom) and node.module == 'c_two._native'
        for alias in node.names
    }
    calls = {
        node.func.id
        for node in ast.walk(release_export)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
    }
    all_imports = {
        alias.name
        for node in ast.walk(tree)
        if isinstance(node, ast.Import)
        for alias in node.names
    }
    release_literals = {
        node.value
        for node in ast.walk(release_export)
        if isinstance(node, ast.Constant) and isinstance(node.value, str)
    }

    assert 'contract_release_ref_json' in native_imports
    assert 'contract_release_ref_json' in calls
    assert 'hashlib' not in all_imports
    assert 'descriptor_sha256' not in release_literals
```

- [ ] **Step 2: Rebuild/run focused tests and observe RED**

Run:

```bash
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/unit/test_contract_export.py \
  sdk/python/tests/unit/test_sdk_boundary.py -q --timeout=30
```

Expected: import/attribute failures for the two native functions and
`cc.export_contract_release_ref`.

- [ ] **Step 3: Add thin native Rust functions**

Add to `wire_ffi.rs`:

```rust
#[pyfunction]
fn canonicalize_portable_contract_descriptor(payload: &[u8]) -> PyResult<String> {
    c2_contract::ContractRelease::from_descriptor_json(payload)
        .map(|release| release.canonical_descriptor_json().to_string())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

#[pyfunction]
fn contract_release_ref_json(payload: &[u8]) -> PyResult<String> {
    c2_contract::ContractRelease::from_descriptor_json(payload)
        .and_then(|release| release.reference().to_canonical_json())
        .map_err(|error| PyValueError::new_err(error.to_string()))
}
```

Register both functions beside the existing descriptor validation/hash helpers.
Do not expose Rust `ContractRelease` fields as separately constructible Python
arguments.

- [ ] **Step 4: Make Python descriptor/ref output consume Rust authority**

Refactor `descriptor.py` to use one private helper:

```python
def _canonical_portable_descriptor(descriptor: dict[str, Any]) -> str:
    from c_two._native import canonicalize_portable_contract_descriptor

    candidate = json.dumps(descriptor, sort_keys=True, separators=(',', ':')).encode()
    return canonicalize_portable_contract_descriptor(candidate)


def _pretty_json(compact: str) -> str:
    return json.dumps(json.loads(compact), sort_keys=True, indent=2) + '\n'
```

Change `export_contract_descriptor(...)` to return the Rust compact result or
`_pretty_json(compact)`. Add:

```python
def export_contract_release_ref(
    crm_class: type,
    methods: list[str] | None = None,
    *,
    pretty: bool = False,
) -> str:
    from c_two._native import contract_release_ref_json

    descriptor = export_contract_descriptor(crm_class, methods)
    compact = contract_release_ref_json(descriptor.encode())
    return _pretty_json(compact) if pretty else compact
```

Export this function from `c_two.crm` and the top-level `c_two` package, and add
it to `__all__`.

- [ ] **Step 5: Rebuild and run focused Python tests GREEN**

Run:

```bash
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/unit/test_contract_export.py \
  sdk/python/tests/unit/test_sdk_boundary.py -q --timeout=30
```

Expected: all focused contract export and SDK-boundary tests pass.

- [ ] **Step 6: Commit the Python projection**

```bash
git add sdk/python/native/src/wire_ffi.rs \
  sdk/python/src/c_two/crm/descriptor.py \
  sdk/python/src/c_two/crm/__init__.py \
  sdk/python/src/c_two/__init__.py \
  sdk/python/tests/unit/test_contract_export.py \
  sdk/python/tests/unit/test_sdk_boundary.py
git diff --cached --check
git commit -m "feat: expose contract release references to Python"
```

---

### Task 4: Add the Language-Neutral CLI Release-Ref Command

**Files:**
- Modify: `cli/src/contract.rs:12-32,154-163,397-420`
- Modify: `cli/tests/contract_commands.rs:235-380`

**Interfaces:**
- Consumes: `ContractRelease::from_descriptor_json` and `ContractReleaseRef::to_canonical_json`.
- Produces: `c3 contract release-ref PATH [--out PATH] [--pretty]`, with `-` stdin support.

- [ ] **Step 1: Write failing CLI tests**

Extend `contract_commands.rs`:

```rust
const RELEASE_DESCRIPTOR: &str = include_str!(
    "../../tests/fixtures/contracts/portable-release.contract.json"
);
const RELEASE_REFERENCE: &str = include_str!(
    "../../tests/fixtures/contracts/portable-release.ref.json"
);

#[test]
fn contract_help_lists_release_ref() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("release-ref"));
}

#[test]
fn contract_release_ref_accepts_file_and_writes_canonical_output() {
    let tempdir = tempfile::tempdir().unwrap();
    let descriptor = tempdir.path().join("contract.json");
    std::fs::write(&descriptor, RELEASE_DESCRIPTOR).unwrap();

    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "release-ref", descriptor.to_str().unwrap()])
        .assert()
        .success()
        .stdout(format!("{}\n", RELEASE_REFERENCE.trim_end()));
}

#[test]
fn contract_release_ref_accepts_stdin_and_pretty_output_file() {
    let tempdir = tempfile::tempdir().unwrap();
    let output = tempdir.path().join("release-ref.json");
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args([
        "contract", "release-ref", "-", "--pretty", "--out",
        output.to_str().unwrap(),
    ])
    .write_stdin(RELEASE_DESCRIPTOR)
    .assert()
    .success()
    .stdout(predicate::str::is_empty());

    let written = std::fs::read_to_string(output).unwrap();
    assert!(written.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&written).unwrap(),
        serde_json::from_str::<serde_json::Value>(RELEASE_REFERENCE).unwrap(),
    );
}

#[test]
fn contract_release_ref_rejects_invalid_descriptor() {
    let mut cmd = Command::cargo_bin("c3").unwrap();
    cmd.args(["contract", "release-ref", "-"])
        .write_stdin(r#"{"schema":"not-c-two"}"#)
        .assert()
        .failure()
        .stderr(predicate::str::contains("contract descriptor invalid"));
}
```

- [ ] **Step 2: Run CLI tests and observe RED**

Run:

```bash
cargo test --manifest-path cli/Cargo.toml --test contract_commands contract_release_ref
```

Expected: failures because Clap has no `release-ref` subcommand.

- [ ] **Step 3: Implement the CLI command**

Add:

```rust
#[derive(Debug, Args)]
pub struct ReleaseRefArgs {
    /// Descriptor JSON path, or "-" to read from stdin.
    pub path: String,
    /// Write release-reference JSON to this file instead of stdout.
    #[arg(long)]
    pub out: Option<String>,
    /// Pretty-print release-reference JSON.
    #[arg(long)]
    pub pretty: bool,
}
```

Add `ReleaseRef(ReleaseRefArgs)` to `ContractCommand`, dispatch it from `run`,
and implement:

```rust
fn release_ref(args: ReleaseRefArgs) -> Result<()> {
    let payload = read_payload(&args.path)?;
    let release = c2_contract::ContractRelease::from_descriptor_json(payload.as_bytes())
        .map_err(|error| anyhow!("{error}"))?;
    let compact = release
        .reference()
        .to_canonical_json()
        .map_err(|error| anyhow!("{error}"))?;
    let output = if args.pretty {
        let value: serde_json::Value = serde_json::from_str(&compact)
            .map_err(|error| anyhow!("release reference serialization failed: {error}"))?;
        serde_json::to_string_pretty(&value)
            .map(|value| value + "\n")
            .map_err(|error| anyhow!("release reference serialization failed: {error}"))?
    } else {
        compact
    };
    write_payload(&output, args.out.as_deref())
}
```

Do not invoke `run_python_contract` from this command.

- [ ] **Step 4: Run CLI tests GREEN**

Run:

```bash
cargo fmt --manifest-path cli/Cargo.toml --all
cargo test --manifest-path cli/Cargo.toml --test contract_commands
```

Expected: all CLI contract tests pass, including existing export/validate tests.

- [ ] **Step 5: Commit the CLI projection**

```bash
git add cli/src/contract.rs cli/tests/contract_commands.rs
git diff --cached --check
git commit -m "feat: add contract release-ref CLI"
```

---

### Task 5: Record Limitations and Correct Cross-Repository Boundaries

**Files:**
- Create: `docs/issues/contract-release-deferred-capabilities.md`
- Modify: `AGENTS.md:79-95`
- Modify: `README.md:230-292,515-540`
- Modify: `docs/roadmap.md:18-30,115-121`
- Modify: `docs/vision/endgame-architecture.md:7-21,29-55,174-227,459-483`

**Interfaces:**
- Consumes: implemented release/ref behavior and approved design non-goals.
- Produces: truthful owner boundaries, deferred-capability exit criteria, and current Toodle terminology.

- [ ] **Step 1: Add the deferred-capabilities issue**

Create `docs/issues/contract-release-deferred-capabilities.md` with:

```markdown
# Contract Release Deferred Capabilities

**Date:** 2026-07-15
**Status:** Open
**Scope:** Capabilities intentionally not provided by the route-independent ContractRelease slice

## Implemented Baseline

C-Two can validate and canonicalize `c-two.contract.v1`, construct an immutable
content-addressed `ContractRelease`, persist a strict
`c-two.contract-release-ref.v1`, verify that reference against resolved
descriptor bytes, and derive an `ExpectedRouteContract` only after a route name
is supplied.

## Open Capabilities

| Capability | Current limitation | Why not in this slice | Impact | Owner | Exit criteria |
| --- | --- | --- | --- | --- | --- |
| Contract compatibility | Release matching is exact by descriptor digest; no semver/range acceptance exists. | Compatibility requires explicit ABI/signature evolution rules after exact identity is stable. | Consumers cannot request a compatible range and must pin one release. | C-Two | Rust-owned compatibility rules reject ambiguity and ABI-incompatible matches with structured errors and cross-language vectors. |
| Resolver and storage | C-Two defines no registry, URL, object store, or digest resolver. | Storage and distribution topology are consumer/deployment concerns. | A ref alone cannot fetch its descriptor. | Catalog/deployment owner | A consumer resolves bytes by digest, then passes them through `ContractRelease` verification without C-Two owning catalog state. |
| Signature and trust | A release digest proves integrity, not publisher identity, authorization, or revocation state. | C-Two does not own Authority identity, key distribution, policy, or revocation. | Callers must not treat a valid release as trusted by itself. | Authority/policy owner, including Toodle where applicable | A downstream signed envelope binds the C-Two ref to its Authority trust model without changing C-Two release identity. |
| Rust SDK | No supported `sdk/rust` client/server facade consumes the release yet. | The SDK must be a real end-to-end consumer of existing core transports, not an empty package. | Rust applications use low-level core crates and lack a stable SDK facade. | C-Two | Rust client and host slices reuse `c2-wire`, `c2-ipc`, `c2-http`, `c2-server`, `c2-runtime`, and `c2-contract`, with runnable documentation. |
| FastDB Rust call-db runtime | Rust cannot yet consume FastDB call-db bindings/owned views through a FastDB-owned stable API. | FastDB owns schema, storage, encode/decode, owned bytes, and view lifetime. C-Two must not copy them. | Portable payload proof is limited to descriptor identity and opaque bytes. | FastDB | FastDB provides a stable Rust or C-ABI-backed Rust runtime with binding validation, encode/decode, owned bytes, retained views, and golden vectors. |
| Bidirectional Rust/Python proof | No Rust host/client cross-language CRM proof is included. | It depends on both the real Rust SDK and FastDB Rust runtime for the payload-bearing path. | ContractRelease is proven as an identity primitive, not as complete SDK interoperability. | C-Two with FastDB dependency | Rust client to Python CRM, Python client to Rust CRM, and Rust-to-Rust calls pass for no-payload and one FastDB request/response; mismatch fails structurally. |

## Guardrail

Do not add a JSON/pickle transport, a C-Two-owned FastDB parser, an unsigned
"trusted" flag, or a placeholder SDK to make these rows appear complete.
```

- [ ] **Step 2: Update C-Two contract documentation**

In `AGENTS.md`, add beside the CRM route-contract bullet:

```markdown
- A persistent CRM contract release is a Rust-validated `ContractReleaseRef`
  derived from canonical `c-two.contract.v1` content. It never contains a route
  name. `ExpectedRouteContract` is derived later from a validated release plus
  a runtime route name.
```

In README, add a `Contract Releases — Persistent Identity` subsection before
the payload model. Document `cc.export_contract_release_ref(...)` and:

```bash
c3 contract release-ref contract.json
```

State explicitly that the digest proves content integrity, not publisher trust,
and that storage/resolution is outside C-Two. Add “Contract release identity” as
implemented in the roadmap table while keeping compatibility and cross-language
SDK rows planned/future.

Update roadmap order 1 exit criteria to include deterministic
`ContractReleaseRef` vectors and exact release verification. Keep Rust SDK at
order 8 and link its missing capability to the deferred issue; do not claim
this contract-core work is an SDK.

- [ ] **Step 3: Correct the stale Toodle boundary in the living endgame document**

Replace every “Tag-Oriented Open Data Linking Environment” occurrence with
“Trust-Oriented Open Distributed Linking Environment”. Replace the L2 summary
so it says:

```markdown
Toodle is a broader trust-oriented open resource environment. It owns durable
Resource identity and revision, resource graph relationships, optional tree
views, tag/search projections, policy, Resource Service declarations, runtime
activation, and federation. C-Two supplies CRM contract and runtime mechanisms;
a live CRM route is a runtime projection of a Resource Service, not the durable
Toodle Resource itself.
```

In section 4.2, define graph nodes as durable Resource or Resource Service
identities rather than CRM instances. State that resource trees are optional
views over the graph, not a mandatory second identity system. In Workspace
examples, refer to Resource/Resource Service references and resolve active CRM
routes through C-Two only at runtime.

In the ownership table, add C-Two-owned canonical CRM descriptor and
`ContractReleaseRef`; add Toodle-owned Resource identity/revision, Resource
Service declaration, graph/tree view, policy, activation, and federation. Keep
domain file formats and algorithms outside C-Two.

- [ ] **Step 4: Check documentation consistency**

Run:

```bash
rg -n "Tag-Oriented Open Data Linking Environment|节点.*CRM 实例" \
  AGENTS.md README.md docs/roadmap.md docs/vision/endgame-architecture.md
rg -n "ContractReleaseRef|contract release-ref|Trust-Oriented Open Distributed" \
  AGENTS.md README.md docs/roadmap.md docs/vision/endgame-architecture.md \
  docs/issues/contract-release-deferred-capabilities.md
git diff --check
```

Expected: the first command returns no matches; the second returns the new
contract and Toodle-boundary text; diff check exits zero.

- [ ] **Step 5: Commit documentation and the issue**

```bash
git add AGENTS.md README.md docs/roadmap.md \
  docs/vision/endgame-architecture.md \
  docs/issues/contract-release-deferred-capabilities.md
git diff --cached --check
git commit -m "docs: define contract release boundaries"
```

---

### Task 6: Run Full Verification and Review the Completed Slice

**Files:**
- Verify all files changed by Tasks 1-5.
- Modify only files required to repair a verification or review finding.

**Interfaces:**
- Consumes: the complete implementation and accepted design.
- Produces: fresh evidence that the slice is correct, formatted, boundary-safe, and ready to remain on `dev-feature`.

- [ ] **Step 1: Run Rust formatting, lint, and tests**

```bash
cargo fmt --manifest-path core/Cargo.toml --all -- --check
cargo clippy --manifest-path core/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path core/Cargo.toml --workspace
cargo fmt --manifest-path cli/Cargo.toml --all -- --check
cargo clippy --manifest-path cli/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path cli/Cargo.toml
```

Expected: every command exits zero with no test failure or Clippy warning.

- [ ] **Step 2: Rebuild the Python extension and run focused plus full tests**

```bash
uv sync --reinstall-package c-two
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/unit/test_contract_export.py \
  sdk/python/tests/unit/test_sdk_boundary.py -q --timeout=30
C2_RELAY_ANCHOR_ADDRESS= uv run pytest sdk/python/tests/ -q --timeout=30
uv python install 3.10
C2_RELAY_ANCHOR_ADDRESS= uv run pytest \
  sdk/python/tests/unit/test_python_examples_syntax.py::test_python_examples_compile_on_minimum_supported_python \
  -q --timeout=30 -rs
```

Expected: focused and full suites pass; the minimum-supported-Python test runs
under Python 3.10 rather than skipping.

- [ ] **Step 3: Run boundary and artifact checks**

```bash
rg -n "toodle::|use .*fastdb|extern crate .*fastdb" \
  core/foundation/c2-contract/src sdk/python/native/src/wire_ffi.rs
cargo run --manifest-path cli/Cargo.toml -- contract release-ref \
  tests/fixtures/contracts/portable-release.contract.json
git diff --check
git status --short --branch
```

Expected: the import scan returns no production dependency match; CLI output
equals `tests/fixtures/contracts/portable-release.ref.json`; diff check exits
zero; only intentional committed changes remain and the worktree is clean.

- [ ] **Step 4: Review against the approved design**

Read `docs/superpowers/specs/2026-07-15-contract-release-identity-design.md`
line by line and verify:

- release content remains `c-two.contract.v1` without a wrapper artifact;
- reference fields exactly match the approved wire shape;
- all ref fields derive from a validated descriptor;
- runtime route is added only by `expected_route`;
- Python and CLI use Rust authority;
- no deferred capability is represented as implemented;
- Toodle wording is current and C-Two does not duplicate its product model.

Record any discovered gap as a focused RED test before repairing production
code. Re-run the relevant focused checks after each repair.

- [ ] **Step 5: Commit verification-driven repairs if any**

If verification required code or documentation repairs, stage only those files,
run their focused tests again, and commit:

```bash
git diff --cached --check
git commit -m "fix: close contract release verification gaps"
```

If no repair was required, do not create an empty commit.

- [ ] **Step 6: Capture final branch evidence**

```bash
git status --short --branch
git log --oneline --decorate -8
git diff --stat origin/dev-feature...HEAD
```

Expected: current branch is `dev-feature`, the worktree is clean, and the
history shows atomic design, plan, Rust authority, Python, CLI, and documentation
commits.
