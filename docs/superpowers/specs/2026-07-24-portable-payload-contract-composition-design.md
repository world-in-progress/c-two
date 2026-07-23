# C-Two Portable Payload Contract and Composition Design

**Date:** 2026-07-24
**Status:** Approved by the active FastDB/C-Two goal for implementation
**Scope:** `c-two.contract.v2`, nested FastDB delegation, deterministic multi-owner artifact composition, Rust/Python/TypeScript projections, and the payload-bearing runtime proof

## 1. Context

C-Two currently has two valid foundations and one obsolete integration layer. The valid foundations are the Rust-owned route-independent `ContractRelease` identity and the Rust-owned route, relay, transport, scheduler, and lease runtime. The obsolete layer is the pre-0.2 FastDB call-db integration: Python code in C-Two infers FastDB shapes, builds FastDB schema descriptors, computes schema identities, interprets call envelopes, and generates FastDB TypeScript helpers. FastDB's accepted portable-payload architecture has now replaced that layer with a C++ Core-owned `fastdb.payload.v1`, stable C ABI, official Rust/Python/TypeScript projections, and Core-owned four-language codegen.

The clean target is not a compatibility adapter around call-db. `c-two.contract.v2` is the C-Two super-schema. It contains each method's nested FastDB specification as a JSON value. C-Two owns the method and binding relationship, but it does not interpret the nested specification. FastDB Core compiles that value and returns its canonical identity, manifest, runtime behavior, and payload-only artifacts. C-Two then combines those artifacts with its own contract, route, and transport artifacts.

This design keeps the larger product direction intact. C-Two remains a resource-RPC runtime rather than a FastDB frontend. FastDB remains a generic payload/storage library rather than a CRM or transport framework. Toodle is not modified and does not receive a copied codec or transport implementation.

## 2. Goals

1. Replace `c-two.contract.v1` with a clean `c-two.contract.v2` outer contract that embeds nested FastDB JSON values instead of referencing call-db sidecars.
2. Keep Rust `c2-contract` as the only C-Two authority for outer validation, canonicalization, contract fingerprints, release identity, and nested-spec extraction.
3. Pass every extracted nested value to the official FastDB Core projection without inspecting FastDB type, profile, layout, binary, digest, graph, view, materialization, or codegen semantics in C-Two.
4. Keep `c3` as the project codegen entry while using FastDB's in-memory `ArtifactSet` as one input to C-Two's final composition.
5. Provide a general deterministic artifact composer that can combine independently owned artifact sets without weakening path, hash, collision, or publication safety.
6. Generate capability-equivalent Rust and Python payload-facing contract surfaces and preserve the existing formal TypeScript transport surface through the new binding model.
7. Prove real generated artifact compilation/import, record and object-graph payload operation, Rust/Python interoperability, no-payload regression, and checked invalidation.
8. Remove obsolete call-db authority, public surfaces, CLI flags, tests, examples, and current documentation in the same clean cut.
9. State every intentional limitation in `docs/issues/` with its reason, impact, owner, and exit criteria.

## 3. Non-Goals

- No FastDB parser, normalizer, canonicalizer, digest, schema algebra, layout planner, binary reader/writer, builder, graph algorithm, view implementation, materializer, or generator in C-Two.
- No JSON or pickle transport presented as a portable alternative to FastDB. Python pickle remains only a Python-local prototype fallback and is rejected by portable export/codegen.
- No domain-object, dataframe, GIS, or arbitrary annotation bridge synthesis in C-Two. Applications may adapt domain objects outside the portable payload boundary.
- No claim that FastDB constructs directly in C-Two response SHM. The first completed runtime path may serialize a completed FastDB payload and let C-Two transport those bytes.
- No contract registry, resolver, Authority, trust, policy, Toodle resource, or deployment object.
- No FastDB version bump, crate publication, Python publication, push, tag, or release.
- No Kubernetes work.
- No empty Rust SDK package. The Rust surface must be a real consumer of `c2-contract`, `c2-ipc`, and the official `fastdb` crate.

## 4. Authority and Dependency Direction

```text
c2-contract
  owns c-two.contract.v2 outer validation, canonical JSON,
  route fingerprints, release identity, and opaque nested-value extraction
        |
        v
c2-codegen
  calls official fastdb Rust projection
  receives Core identity/manifest/ArtifactSet
  generates C-Two artifacts
  composes and publishes one final artifact tree
        |
        +--> c3 CLI
        |
        +--> c-two Python native projection

FastDB C++ Core
  owns nested fastdb.payload.v1 semantics, binary/runtime/lifetime,
  structured errors, and payload-only codegen
```

`c2-contract` has no FastDB dependency. It recognizes only the C-Two binding discriminator `kind: "fastdb"` and treats `spec` as an opaque JSON value. `c2-codegen` is the integration point because codegen and final project composition are C-Two concerns. It consumes the public `fastdb` Rust crate; it does not call private C++ APIs or copy C ABI declarations.

The FastDB Rust crates are still `publish = false` and no release operation is authorized. During this goal C-Two therefore consumes the sibling checkout through a clearly recorded local path dependency. This is local integration evidence, not a claim that the dependency can already be fetched from a package registry. The distribution limitation and its closure condition remain in the owner Issue until a separately authorized FastDB release exists.

## 5. `c-two.contract.v2`

### 5.1 Selected wire shape

```json
{
  "schema": "c-two.contract.v2",
  "crm": {
    "namespace": "demo.payload",
    "name": "PayloadEcho",
    "version": "0.1.0"
  },
  "fingerprints": {
    "abi_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "signature_hash": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
  },
  "methods": [
    {
      "access": "read",
      "name": "ping",
      "parameters": [],
      "return": {"kind": "none"},
      "bindings": {"input": null, "output": null}
    },
    {
      "access": "write",
      "name": "roundtrip",
      "parameters": [
        {
          "name": "payload",
          "kind": "POSITIONAL_OR_KEYWORD",
          "default": {"kind": "missing"},
          "type": {"kind": "payload"}
        }
      ],
      "return": {"kind": "payload"},
      "bindings": {
        "input": {
          "kind": "fastdb",
          "spec": {
            "schema": "fastdb.payload.v1",
            "profile": "record.v1",
            "entries": [],
            "components": []
          }
        },
        "output": {
          "kind": "fastdb",
          "spec": {
            "schema": "fastdb.payload.v1",
            "profile": "record.v1",
            "entries": [],
            "components": []
          }
        }
      }
    }
  ]
}
```

The nested `spec` is not a sidecar reference and is not a second independently resolved document. It is part of the contract release content. The outer release digest therefore covers the exact nested JSON value while FastDB Core still owns the nested value's canonical payload digest.

### 5.2 Outer validation

Rust rejects unknown outer fields and validates:

- the exact root keys `schema`, `crm`, `fingerprints`, and `methods`;
- `schema == "c-two.contract.v2"`;
- CRM identity text and duplicate method names;
- method access as `read` or `write`;
- parameter names, explicit non-variadic parameter kind, and missing-default shape;
- the relationship between parameters, return shape, and input/output bindings;
- a binding object with exact keys `kind` and `spec`, where `kind == "fastdb"`;
- lowercase 64-byte route fingerprints and their Rust-derived projection values.

Rust deliberately does not require `spec` to be an object and does not inspect any field inside it. A null, scalar, array, malformed object, wrong FastDB schema, unsupported profile, invalid type, or resource-limit violation reaches FastDB Core and returns a FastDB-owned structured error. This preserves the owner boundary and makes delegation mechanically testable.

### 5.3 Method shape invariants

- `bindings.input == null` requires zero parameters.
- A FastDB input binding requires exactly one explicit parameter whose type is `{"kind":"payload"}` and whose default is `{"kind":"missing"}`.
- `bindings.output == null` requires `return.kind == "none"`.
- A FastDB output binding requires `return.kind == "payload"`.
- Variadic positional and keyword parameters are not portable.
- Method array order is significant because it is the stable transport method-index order.

The portable method signature intentionally exposes one payload envelope rather than duplicating FastDB entry/component types in C-Two annotations. Generated FastDB artifacts provide the typed builder/view surface inside that envelope.

### 5.4 Fingerprints

Route fingerprints remain C-Two facts. Rust derives and verifies them from two explicit projections:

- ABI projection: CRM identity, ordered method names, and opaque input/output binding values;
- signature projection: CRM identity, ordered method names, access, parameter metadata, and return metadata.

The ABI projection includes the nested JSON value as opaque C-Two release content. It is not called a FastDB digest and does not replace the Core-returned FastDB SHA-256. Equivalent JSON object key order produces the same outer projection hash; FastDB's own canonical identity is separately queried and embedded in generated artifact provenance.

### 5.5 Lifetime is not part of the nested specification

`buffer`, hold state, input borrowing, route token, relay location, lease identity, SHM coordinates, and allocator policy are absent from `c-two.contract.v2` payload bindings. They belong to C-Two runtime configuration or call-site policy:

- normal receive opens an owned FastDB payload;
- `cc.hold(...)` retains the C-Two response lease and invalidates FastDB payload/views before release;
- server borrowed-input policy remains explicit registration policy;
- C-Two may later provide a final backing to FastDB, but only after a public backing adapter proves true resource-time construction.

## 6. Rust Object Model

`ValidatedContractDescriptor` retains canonical descriptor JSON, CRM identity, fingerprints, release digest, and ordered method descriptors. Each method exposes optional `NestedFastDbSpec` values:

```rust
pub struct NestedFastDbSpec {
    outer_path: String,
    canonical_json: String,
}

pub enum BindingDirection {
    Input,
    Output,
}

pub struct ValidatedMethodDescriptor {
    name: String,
    access: MethodAccess,
    input: Option<NestedFastDbSpec>,
    output: Option<NestedFastDbSpec>,
}
```

The nested canonical JSON is only a deterministic serialization of the opaque JSON value extracted by C-Two. It is the byte input to `fastdb::CompiledSpec::compile(...)`; C-Two never treats it as FastDB canonical output. `c2-codegen` replaces it with the Core-returned canonical JSON, digest, manifest, and artifacts in compiled facts.

`ContractRelease` and `c-two.contract-release-ref.v1` remain route-independent. The reference schema does not change because its shape and meaning do not change; its `contract_schema` value becomes `c-two.contract.v2`.

## 7. FastDB Delegation and Error Preservation

For every ordered input/output binding, `c2-codegen`:

1. reads the `NestedFastDbSpec` canonical JSON bytes;
2. calls `fastdb::CompiledSpec::compile(...)`;
3. reads Core canonical JSON, SHA-256, profile/capabilities, and manifest;
4. invokes Core codegen for the selected target;
5. copies each owned artifact out of the Core `ArtifactSet`;
6. verifies the artifact SHA-256 against the returned bytes;
7. prefixes the artifact with a C-Two-owned deterministic project path;
8. records binding path, Core digest, manifest, target, path, kind, and hash in the C-Two composition manifest.

An outer error does not flatten a FastDB error into display text:

```rust
CodegenError::FastDb {
    binding_path,
    code,
    symbol,
    path,
    message,
    details_json,
}
```

Python raises a C-Two exception carrying the same fields as attributes. CLI display includes the outer binding path and FastDB cause, while programmatic Rust/Python callers can branch on the fields without parsing text.

## 8. Artifact Composition

### 8.1 Generic artifact record

```text
ContractArtifact
  relative_path
  kind
  bytes
  sha256
  owner/provenance
```

The composer is generic. It knows path, bytes, hash, kind, and provenance, but it does not know FastDB types or C-Two transport syntax. FastDB and C-Two artifact producers remain independently testable.

### 8.2 Portable path contract

Every artifact path must:

- be non-empty, relative, slash-separated UTF-8;
- contain no empty, `.` or `..` segment;
- contain no backslash, NUL, control character, drive prefix, URI prefix, or leading slash;
- use portable ASCII segment characters `[A-Za-z0-9._-]`;
- avoid Windows reserved device names and trailing dot/space;
- remain within the documented per-segment and total byte limits.

The composer rejects duplicate paths even if bytes are identical. It also rejects file/directory prefix conflicts such as `a` with `a/b`, and it recomputes every SHA-256 before accepting the record. Final artifacts are sorted lexicographically by normalized path.

### 8.3 Deterministic layout

For one selected target:

```text
metadata/
  contract.json
  contract-release-ref.json
  composition-manifest.json
rust/ | python/ | typescript/
  c_two_contract...
  payloads/
    <fastdb-sha256>/
      <Core-returned artifact path>
```

Identical nested specs for the same target produce one FastDB artifact subtree and multiple binding references to the same digest. A repeated producer record is not silently accepted; deduplication happens before composition by verified FastDB identity.

No artifact contains timestamps, random identifiers, checkout paths, temporary directory names, or machine state.

### 8.4 Publication

The library publishes a complete artifact set into a newly created destination tree:

1. validate the entire in-memory set;
2. create a sibling staging directory;
3. create directories and files with no-follow/create-new behavior;
4. re-read and verify written hashes;
5. rename the complete staging tree to the absent destination;
6. remove staging state on failure.

The first slice intentionally does not merge into an existing source tree. Incremental merge would permit stale files, partial updates, and symlink traversal unless it also owns a prior manifest and rollback protocol. Higher-level project generators can compose all concerns into a fresh tree and then decide how to adopt it. Existing-tree replacement remains an Issue with explicit exit criteria rather than an unsafe shortcut.

## 9. Codegen and SDK Surfaces

### 9.1 Rust

The public `c2-codegen` API compiles a validated contract into an in-memory `ContractArtifactSet`. Generated Rust code embeds the C-Two release facts, references Core-generated payload modules, validates payload provenance through FastDB's public `require_spec_sha256`, and uses route-bound `c2-ipc` calls for opaque bytes. Server helpers decode/encode with `fastdb::Payload::open_copy` and `binary_bytes`.

The Rust surface does not query private resolved FastDB types. Typed entry/component ergonomics come only from the Core-generated Rust artifact.

### 9.2 Python

The Python native extension calls the same Rust `c2-codegen` function and projects the same in-memory artifacts and structured errors. Generated Python contract code uses explicit FastDB specs and the official `fastdb4py.payload` `CompiledSpec`, `Payload`, builder, view, materialize, and invalidate operations.

Python authoring uses an explicit binding:

```python
@cc.crm(namespace="demo.payload", version="0.1.0")
class PayloadEcho:
    @cc.transfer(input=REQUEST_SPEC, output=RESPONSE_SPEC)
    def roundtrip(self, payload: Payload) -> Payload: ...
```

C-Two does not infer FastDB schemas from Python annotations. A portable bound method has one `Payload` parameter and/or one `Payload` return. Non-bound ordinary Python values may still use pickle for a Python-only prototype, but portable export/codegen rejects that method.

### 9.3 TypeScript

The existing generated C-Two transport, route validation, HTTP/IPC safety, and hold interfaces remain C-Two-owned. Under v2, the internal codec requirement is derived from the Core-returned payload digest instead of a call-db `PayloadAbiRef`. C-Two generates a small adapter that uses the official FastDB TypeScript `Payload.binaryBytes`, `Payload.openCopy`, provenance guard, and invalidation APIs. Core-generated TypeScript artifacts remain the only typed payload helpers.

No legacy `--fastdb-schema`, `--fastdb-out`, Python helper subprocess, or call-db strict-codec admission remains.

### 9.4 CLI

`c3` remains the write entry:

```text
c3 contract codegen rust DESCRIPTOR --out-dir DEST
c3 contract codegen python DESCRIPTOR --out-dir DEST
c3 contract codegen typescript DESCRIPTOR --out-dir DEST
```

All three commands validate the outer contract, delegate nested specs, compose the complete set, and publish one new tree. The CLI does not write FastDB artifacts before C-Two composition succeeds.

## 10. Runtime Semantics and Honest Backing Claims

The first portable runtime representation is the official FastDB `Payload` owner:

- sender validates the payload against the binding's Core digest and reads `binary_bytes`;
- C-Two transports opaque bytes through its normal inline/SHM/chunk selection;
- receiver uses the official FastDB `open_copy` path;
- views are created and materialized through FastDB;
- retained or borrowed C-Two lease closure invokes FastDB `Payload.invalidate` before transport release;
- raw pointer/NumPy escape hatches, if any are introduced by a language binding, remain explicitly unsafe and are not claimed revocable.

This path contains copies. It does not prove direct construction into the final C-Two response backing and does not claim zero-copy. A future direct-backing slice must use FastDB's public backing contract at resource execution time, prove direct/staged reports, and support both Rust and Python before documentation may make a stronger claim.

## 11. Clean Cut

The following current surfaces are removed rather than adapted:

- `sdk/python/src/c_two/fastdb/call_db.py`;
- `sdk/python/src/c_two/fastdb/typescript.py`;
- the C-Two-owned FastDB schema/type bridge implementation in `sdk/python/src/c_two/fastdb/bridge.py`;
- `PayloadAbiRef`, call-db artifact sidecars, and method payload ABI inference;
- `org.fastdb.call-db`, `org.fastdb.columnar`, `org.fastdb.object-graph`, `fastdb.call-db.schema.v1`, and `fastdb.schema.v1` current production assumptions;
- `c3 contract artifacts`, call-db diagnostics, `--fastdb-schema`, `--fastdb-out`, and the Python FastDB TypeScript generator subprocess;
- call-db examples, tests, README guidance, roadmap claims, and active AGENTS instructions.

Historical plans may retain old evidence only when prominently marked historical/superseded. Current guidance, public surfaces, package inventories, and tests must contain only the v2 architecture.

## 12. Verification

Required focused proof:

1. v2 canonical identity is stable across formatting and object key order.
2. nested spec changes change the outer release and ABI fingerprint.
3. invalid outer binding shape fails in C-Two with an outer path.
4. malformed nested FastDB values reach Core and preserve code, symbol, path, message, and details.
5. identical nested specs yield identical Core digest in Rust and Python.
6. FastDB artifact hashes are reverified; duplicate, invalid, prefix-conflicting, and mismatched records fail deterministically.
7. repeated generation yields identical paths, bytes, hashes, manifests, and order.
8. generated Rust compiles, generated Python imports, and generated TypeScript type-checks.
9. record payloads cover nullable values, `str`, `wstr`, bytes, and recursive lists.
10. object-graph payloads cover roots, refs, sharing, self-cycle, and mutual cycle.
11. real build/open/view/materialize/invalidate paths run through official FastDB APIs.
12. Rust client to Python resource, Python client to Rust resource, and Rust-to-Rust payload calls succeed; a wrong payload digest fails structurally.
13. no-payload methods and route/release identity still work.
14. held/borrowed values fail after invalidation, without claiming unsafe raw aliases can be revoked.
15. source scans find no C-Two FastDB parser/canonicalizer/digest/layout/binary/graph/materialization/codegen implementation.

The full repository gates remain those in the active goal. Hosted CI is reported as unrun unless it actually runs.

## 13. Known Limits Tracked in Issues

The owner Issue remains open for:

- exact-only contract release matching;
- external resolver/storage and trust/signature concerns;
- complete Rust HTTP/relay ergonomic SDK beyond the real payload-bearing IPC slice;
- FastDB Rust crate/package distribution before an authorized release;
- copy-backed Python/Rust receive and the absence of proven direct final-backing construction;
- new-tree-only atomic artifact publication;
- C-Two C++ SDK/codegen, which does not yet exist;
- hosted CI and release operations.

These limits do not justify reintroducing call-db or a C-Two-owned FastDB implementation.

## 14. Acceptance Criteria

1. `c-two.contract.v2` is the only current portable contract schema.
2. Every FastDB binding is a nested value inside its method binding.
3. `c2-contract` never validates nested FastDB semantics.
4. FastDB Core is called for compile, canonical identity, manifest, runtime, and codegen.
5. C-Two composes verified FastDB and C-Two artifacts into one deterministic set and publishes it atomically as a new tree.
6. Rust and Python expose the same compilation and payload capabilities; TypeScript retains its existing transport capabilities under the new binding.
7. Generated Rust/Python/TypeScript artifacts compile/import/type-check and run against the official FastDB projections.
8. Real cross-language payload calls and no-payload regressions pass.
9. Legacy call-db authority and current guidance are removed.
10. All remaining limits have an owner and closure criterion in `docs/issues/`.
