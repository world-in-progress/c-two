# Portable Payload Contract and Composition Implementation Plan

**Date:** 2026-07-24
**Design:** [`../specs/2026-07-24-portable-payload-contract-composition-design.md`](../specs/2026-07-24-portable-payload-contract-composition-design.md)
**Starting C-Two commit:** `f03b8e8`
**Consumed FastDB freeze:** `6b9d0a55f27bb22fd13f867f321db821f21e777c`
**Execution rule:** Each task uses a retained `.superpowers/sdd/c-two-task-<n>-brief.md`, true RED, focused GREEN, affected broad gates, a scoped local commit, and two-pass primary-agent review before the next task.

## Task 1: Freeze the v2 Outer Contract

**Files:** `core/foundation/c2-contract/**`, shared contract fixtures, focused Rust tests, Python native contract projection tests, contract release docs/Issue.

1. Add v2 fixtures with no-payload and nested FastDB-bound methods.
2. Write RED tests for exact v2 outer shape, method/binding consistency, Rust-derived fingerprint verification, opaque nested extraction, release identity, and malformed nested values surviving outer validation.
3. Replace the v1 codec-ref validator with the v2 method/binding validator.
4. Expose ordered method metadata and `NestedFastDbSpec` with exact outer paths and deterministic JSON bytes.
5. Preserve `ContractRelease`/`ContractReleaseRef` route independence and update goldens to `contract_schema: c-two.contract.v2`.
6. Project the same validator/canonicalizer/fingerprint facts through the Python native extension.
7. Run focused Rust/Python/CLI contract tests, core workspace tests, and `git diff --check`.
8. Commit as `feat: define nested portable payload contract v2`.

## Task 2: Add Core Delegation and Generic Artifact Composition

**Files:** `core/foundation/c2-codegen/**`, `core/Cargo.toml`, focused fixtures/tests, FastDB local dependency metadata, Issue status.

1. Write RED tests for nested delegation, exact Core digest/manifest, structured FastDB causes, artifact hash verification, portable paths, duplicates, prefix conflicts, deterministic order, and identical-spec deduplication.
2. Add the local prerelease dependency on the official sibling FastDB Rust crate only in the integration/codegen layer.
3. Implement `ContractArtifact`, provenance, `ContractArtifactSet`, `ArtifactComposer`, and typed `CodegenError`.
4. Compile every nested value through `fastdb::CompiledSpec`, query Core facts, invoke Core codegen, and prefix returned artifacts without inspecting FastDB semantics.
5. Generate target-independent contract/release/composition metadata.
6. Implement fail-closed new-tree publication with staging, create-new writes, written-hash verification, cleanup, and final rename.
7. Run focused tests twice for determinism, core workspace tests, FastDB Rust focused regression, and `git diff --check`.
8. Commit as `feat: delegate payload codegen and compose artifacts`.

## Task 3: Project Composition Through `c3` and Python

**Files:** `cli/**`, `sdk/python/native/**`, `sdk/python/src/c_two/codegen.py`, public exports, CLI/Python tests.

1. Write RED CLI tests for `rust`, `python`, and `typescript` `--out-dir` generation and fail-closed destination behavior.
2. Replace legacy FastDB schema/helper flags and Python subprocess codegen with the Rust `ContractArtifactSet` path.
3. Add Python in-memory artifact projection and structured `ContractCodegenError` attributes backed by the same Rust implementation.
4. Generate equivalent Rust/Python contract modules and adapt the existing TypeScript transport generator to the v2 FastDB digest-backed codec requirement.
5. Add official FastDB payload adapters for all generated targets without rendering FastDB types in C-Two.
6. Compile/import/type-check actual generated outputs in clean temporary project trees.
7. Run CLI, native-extension, Python SDK, TypeScript, core, package, and documentation focused gates.
8. Commit as `feat: expose portable contract composition to c3 and sdk`.

## Task 4: Replace Python Call-DB Runtime with Explicit Payload Owners

**Files:** `sdk/python/src/c_two/crm/**`, `sdk/python/src/c_two/fastdb/**` deletion, Python public exports, focused runtime tests and examples.

1. Write RED tests for `@cc.transfer(input=spec, output=spec)`, explicit `Payload` annotations, Core digest guards, send/open-copy, view/materialize/invalidate, held release, borrowed-input invalidation, and Python-only pickle rejection during portable export.
2. Add the thin Python binding that compiles through `fastdb4py.payload.CompiledSpec` and transports only `Payload.binary_bytes`.
3. Make Python descriptor authoring produce a v2 candidate and pass it through Rust validation/canonicalization/fingerprint authority.
4. Remove call-db inference, `PayloadAbiRef`, schema sidecars, FastDB bridge/type planning, TypeScript helper generation, and obsolete public exports.
5. Keep thread-local direct calls and no-payload paths unchanged.
6. State that receive is copy-backed and direct C-Two final-backing construction is not implemented.
7. Run focused runtime/descriptor/lifetime tests, the affected Python suite, minimum Python syntax, and source boundary scans.
8. Commit as `refactor: replace call-db with portable payload owners`.

## Task 5: Prove Rust/Python Runtime Composition

**Files:** generated-artifact harness, Rust/Python interop fixtures, minimal reusable C-Two request materialization helper if required, proof report.

1. Generate record and object-graph Rust/Python contract/payload artifacts through `c3`/the shared library path.
2. Compile a real Rust client and host against C-Two core plus the official FastDB Rust crate.
3. Import the generated Python package against the official local FastDB Python projection.
4. Prove Rust client to Python resource, Python client to Rust resource, and Rust-to-Rust calls.
5. Cover `str`, `wstr`, bytes, nested lists, nullability, sharing, self-cycle, mutual cycle, build, open, view, materialize, and invalidate.
6. Prove route mismatch, payload digest mismatch, lease invalidation, and no-payload behavior fail/succeed through their correct owner.
7. Delete every disposable Cargo/CMake/wheel/generated build tree after recording commands, counts, and hashes.
8. Commit as `test: prove Rust Python portable payload interop`.

## Task 6: Complete the Clean Cut and Documentation

**Files:** `AGENTS.md`, README files, roadmap/vision/current provider docs, examples/tests, `docs/issues/**`, package inventories and repository gates.

1. Remove current call-db, old FastDB engine/type, sidecar artifact, and legacy CLI guidance.
2. Mark historical plans as superseded where a reader could mistake them for current authority.
3. Update the deferred-capabilities Issue with closed rows, partially completed rows, and every remaining limitation.
4. Add exact local proof results, platform/tool versions, artifact inventories/hashes, commit IDs, and hosted/release status.
5. Run the complete C-Two gate from the active goal plus package/docs/forbidden-term checks.
6. Run a primary-agent spec-compliance review of the frozen complete diff.
7. Run a separate primary-agent code-quality review covering correctness, portability, lifetime, determinism, path safety, resource accounting, packaging, and test credibility.
8. Fix all material findings, rerun affected complete gates, clean build artifacts, and commit as `docs: record portable payload composition closure`.

## Final Evidence

The final report must identify:

- FastDB freeze `6b9d0a5` and C-Two implementation/fix/closure commits;
- exact generated artifact inventories and SHA-256 values;
- exact Rust/Python/TypeScript compile/import/type-check results;
- exact interop and full-suite counts;
- copy/direct/staged and lifetime claims at their proven strength;
- the primary-agent review result as non-independent;
- remaining Issue rows and owners;
- hosted CI as unrun unless actually run;
- unchanged package versions and absence of push/tag/publish/release.
