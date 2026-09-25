# Rust SDK and Portable-Payload Local Release Candidate

- **Date:** 2026-07-24
- **Status:** Complete as a local, unpublished Phase 0B upstream release candidate
- **Scope:** FastDB owner package, C-Two Core/Rust/Python/TypeScript composition, isolated package consumers, executable parity, and three-repository handoff
- **Review:** Same context-owning agent; not independent and not delegated

## Outcome

FastDB implementation commit `7eb74734926bd8fe911229eee9744a6dd8172487` packages the frozen C++ Core/C ABI and official Rust, Python, and TypeScript projections without changing ABI-117 or package versions. C-Two implementation commit `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02` consumes those exact FastDB artifacts, packages the language-neutral Core plus supported SDKs, generates all three C-Two target trees through `c3`, and passes isolated Rust, CPython 3.10/current, and Node consumers.

The complete candidate manifest contains 42 artifacts and has SHA-256 `73d723dbec919d20c229b6fa441750571efcb0937bd2b3c9f84260807c11e0f1`. Candidate-stage receipts contain exactly 18/18 passing Rust/Python direct/relay rows, 12/12 passing generated TypeScript Node rows, and 4/4 passing isolated package consumers. No row is skipped or xfailed.

This is local evidence only. No version bump, push, tag, publication, upload, hosted run, or release was performed. Package version strings reused by the local candidate are disambiguated by source commit and SHA-256 and must not be resolved from public registries as if they represented these bytes.

## Authority and owner boundaries

- FastDB C++ Core remains the sole authority for `fastdb.payload.v1`, canonical identity, binary layout, build/open/view/materialize/invalidate behavior, backing reports, errors, and payload-only code generation. Its C ABI remains exactly 117 sorted `fdb_payload_v1_*` symbols.
- `c2-core` is the single language-neutral owner of route selection, transport calls, host registration, retry classification, error normalization, lease ordering, and runtime lifecycle. It has no FastDB dependency.
- The Rust SDK is directory `sdk/rust`, Cargo package `c-two`, and Rust import `c_two`. It adapts official `fastdb::Payload` values but does not rename or reimplement them.
- Python projects the same Core through the PyO3 native facade while retaining only Python CRM authoring, resource invocation, serialization glue, and explicitly nonportable pickle/thread-local behavior.
- Generated TypeScript uses C-Two transport/runtime packages plus Core-owned FastDB payload artifacts. Its real-call evidence is Node-only on the recorded host and is not browser proof.
- Toodle remains a downstream consumer and receives tracking documentation only in this goal. It does not receive a private codec, transport, sidecar, package copy, or production adapter implementation.

## Source and commit record

| Repository | Starting point | Package-input implementation commit | Documentation closure |
|---|---|---|---|
| FastDB | `6b9d0a55f27bb22fd13f867f321db821f21e777c` | `7eb74734926bd8fe911229eee9744a6dd8172487` | Later commit with subject `docs: record C-Two local candidate handoff`; not an artifact source |
| C-Two | `edcad96e2452c6889d859d75ac996c82fe9fa416` | `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02` | The commit containing this report, with subject `docs: record upstream local candidate closure`; not an artifact source |
| Toodle | `8460fbee2da1a5ad5e2c20569c2c15a37e4541cc` | No package or production implementation change | Later tracking-only commit with subject `docs: record upstream local candidate status` |

The documentation-only commits follow candidate construction. They are intentionally absent from artifact provenance because a commit cannot both contain a self-referential final hash and be the source of already-frozen bytes.

### Scoped implementation commits

FastDB:

1. `7eb74734926bd8fe911229eee9744a6dd8172487 build: package portable payload candidate`

C-Two:

1. `a62b9ea4b038ee38ea486b9aa3601ff31083110d docs: plan Rust SDK local release candidate`
2. `56b0fb7b36101e0647df38ba72422748c374097d feat: bound contract admission`
3. `676c641efb2d0e73bb2858ae231eebb4fd0b8c37 refactor: establish c2 core facade`
4. `710a664d64dd9d7e2aafc1c5f87c3907f0dd6ee8 feat: centralize transport lease ordering`
5. `033a14b6f654101e052b19300f0f28fc0a7f22e9 feat: unify c2 core client and host`
6. `24365cca06a00c728e51c3572e688282c5917599 feat: add user facing Rust SDK`
7. `a0662ad9abb8af0de8a3b548beccf90451d7618f feat: generate Rust SDK clients and services`
8. `c3bcf5a2dde900249f8225ef355e0039408745de refactor: project Python SDK through c2 core`
9. `6fa259f53c8f99160b76bb32e71a6f80618bcbd7 test: cover portable payload direct relay matrix`
10. `f31ac0811f25a29d92cdc2b36a7c31e812785289 test: execute generated TypeScript real calls`
11. `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02 build: prove isolated local release candidate`

Toodle has no scoped implementation commit in this goal.

## Retained evidence

Only manifests, receipts, the parity record, and this report are retained in Git. Package archives, binaries, dynamic libraries, native addons, generated projects, local registries, wheelhouses, virtual environments, `node_modules`, and build trees are deliberately excluded.

| Evidence | SHA-256 | Meaning |
|---|---|---|
| [`local-release-candidate-manifest.v1.json`](evidence/local-release-candidate-manifest.v1.json) | `73d723dbec919d20c229b6fa441750571efcb0937bd2b3c9f84260807c11e0f1` | Complete 42-artifact candidate, archive inventories, build provenance, platform, and source commits |
| [`package-consumer-receipt.v1.json`](evidence/package-consumer-receipt.v1.json) | `858c9e5a208cd1dfe63c666c7450cac8d78f41c4b6c69b4249c6591bdfc80897` | Four isolated archive-only consumers bound to the candidate manifest |
| [`portable-matrix-receipt.v1.json`](evidence/portable-matrix-receipt.v1.json) | `44608ba23d823fb550a67e3f18751d42c6dfdad39b62cd71c8d0a70c6e00a72d` | Exact 18-row Rust/Python direct/relay candidate matrix |
| [`typescript-real-call-receipt.v1.json`](evidence/typescript-real-call-receipt.v1.json) | `3ed4ef96c8c11aad38a1acc468d6ded8812cac238b527c4913f1fc5c21544c60` | Exact 12-row generated TypeScript Node real-call matrix |
| [`sdk-capability-parity.v1.json`](evidence/sdk-capability-parity.v1.json) | `c38499711115a2d8530f49c4ead31a73fe02c1ccf2d05a331845ee14fb6f4654` | Core-generated 12-row Rust/Python capability inventory |
| FastDB [`fastdb-local-candidate-manifest.v1.json`](../../../fastdb/docs/issues/evidence/fastdb-local-candidate-manifest.v1.json) | `9a1c7c83dca16237257dcc50d5917f10d032e02ffceae1278ae331d101b69d32` | Seven FastDB-owned package artifacts and exact ABI-117 |

The FastDB link above is a sibling-repository reference for a coordinated local workspace and is not used by package consumers. The C-Two manifest embeds the same FastDB manifest bytes and hash.

## Platform and toolchains

| Fact | Recorded value |
|---|---|
| Candidate platform | `darwin-arm64` |
| Host | macOS `26.5.2`, Darwin `25.5.0`, `arm64` |
| Rust | `rustc 1.91.0 (f8297e351 2025-10-28)` |
| Cargo | `cargo 1.91.0 (ea2d97820 2025-10-10)` |
| Cargo local registry tool | `cargo-local-registry 0.2.12` |
| CMake | `4.3.2` |
| Native compiler | Apple Clang `21.0.0 (clang-2100.1.1.101)` |
| Emscripten | `5.0.2-git` |
| SWIG | `4.4.1` |
| CPython minimum | `3.10.17` |
| CPython current candidate | `3.14.5` |
| Node | `v25.8.1` |
| npm | `11.11.0` |
| TypeScript package | `5.9.3` |
| uv | `0.10.12 (00d72dac7 2026-03-19 aarch64-apple-darwin)` |

Only `darwin-arm64` is verified by this candidate. Linux aarch64/x86-64 are unverified, Windows is unsupported by this candidate construction, and browser runtime is unverified.

## Package and artifact closure

The candidate manifest contains 30 C-Two-owned entries, nine FastDB-owned entries, two third-party NumPy runtime wheels, and one TypeScript build-tool tarball. Every entry records owner, path, kind, package identity, exact bytes, SHA-256, complete internal inventory, build command, toolchain, platform, target, and source commit.

### FastDB package inputs

| Artifact | SHA-256 |
|---|---|
| Core/C ABI bundle | `eaea8ff354da069957fefbed07aab268f6e343091b7400d4d12be9ec515fff58` |
| FastDB manifest | `9a1c7c83dca16237257dcc50d5917f10d032e02ffceae1278ae331d101b69d32` |
| CPython 3.10 `fastdb4py==0.1.22` wheel | `1d8ff0e41a719cdb26d8ceeec6f0b5629ac6bcf6ed8a52b477007f39e815e4b4` |
| Current `fastdb4py==0.1.22` wheel | `bbf45f30faa5e80fedf6f0523efb77443db43e35385eb12f5c2867214dabebdd` |
| `fastdb4py==0.1.22` sdist | `421852e7dd23be321c66b1dbf0675962f5332e37a74cde805d21c183da679fa9` |
| `fastdb==0.1.22` crate | `43f0915631a63fe40711a3945fcbf1c5489c4db4536888dc79acc229b661ca9e` |
| `fastdb-sys==0.1.22` crate | `a57bce3c3564356dcbabc1c0e3905d62eb859147fc35279c33b4f151fa460367` |
| `fastdb4ts==0.0.3` tarball | `219fcda8ae71ff97a8ddc0cf11a1edf6c2d7299b5371c32195f5bbd781080a9a` |

### C-Two and runtime package inputs

| Artifact | SHA-256 |
|---|---|
| `c3==0.1.4` binary | `898bf3ad6b03535f330adbf943f216534c44fb423d98cf2b5ae2d84c80a7f8ab` |
| `c-two==0.1.0` Rust crate | `fe150f89bf83673125dd0d8f46525d350128e3db48fbf8bb55cc59c5d9c9354e` |
| `c-two==0.5.1` Python sdist | `cb29b01d76c0534419682a8038538bfae01443c5623e18ddaf6922299b0ada22` |
| CPython 3.10 `c-two==0.5.1` wheel | `d7a0f3fd585eac376a1679a76e53a29a78d2623240dbfd96b3e91e2dc7d96f65` |
| Current `c-two==0.5.1` wheel | `3f3b18e3654ccc790584ea6a7e7486edfd9b56d058d6c4815d6391cdfdc9c224` |
| `@c-two/c2-mem-ffi==0.1.0` tarball | `0865b5827f6847ad9c161f86b2fb3f07cabfd9da45e2e38a461af4dbc7908651` |
| CPython 3.10 NumPy `2.2.6` wheel | `37e990a01ae6ec7fe7fa1c26c55ecb672dd98b19c3d0e1d1f326fa13cb38d163` |
| Current NumPy `2.5.1` wheel | `efd736408cc97c79b9e6917338dfc8f06013b2274f992e96b1d9a81a71e2a2c2` |
| TypeScript `5.9.3` build-tool tarball | `10e108c9cf7d5f2879053dff18515fb405abf2ccef63eaaf017d9c571687a1d3` |

The manifest additionally contains exact version-only archives for `c2-cli`, `c2-codegen`, `c2-config`, `c2-contract`, `c2-core`, `c2-error`, `c2-http`, `c2-ipc`, `c2-mem`, `c2-mem-ffi`, `c2-python-native`, `c2-server`, and `c2-wire`; all nine generated contract trees; the contract/FastDB fixtures; and the candidate proof harness. Their complete hashes and inventories are authoritative in the retained manifest rather than duplicated incompletely here.

### Isolated consumers

| Consumer | Isolation contract | Result |
|---|---|---|
| Rust | Version-only manifests, offline retained local registry, no source/path dependency | Passed real call using `c-two` and `fastdb` crate hashes above |
| CPython 3.10 | `--no-index`, installed wheels only, exact NumPy runtime closure | Passed real call and checked lifetime |
| Current CPython | `--no-index`, installed wheels only, exact NumPy runtime closure | Passed real call and checked lifetime |
| Node | Exact tarballs only, no sibling aliases | Passed generated real call using `fastdb4ts`, `@c-two/c2-mem-ffi`, and TypeScript tarballs |

The package-consumer receipt is bound to candidate manifest SHA-256 `73d723dbec919d20c229b6fa441750571efcb0937bd2b3c9f84260807c11e0f1` and records cleanup of its temporary environments, hosts, and relays.

## Rust/Python SDK capability parity

| Capability | Authority | Rust | Python | Portable |
|---|---|---|---|---|
| Descriptor/release identity | `c2-contract` | projection | native projection | yes |
| Route lifecycle | `c2-core` | facade | native facade | yes |
| Direct IPC | `c2-core` | supported | supported | yes |
| Explicit relay | `c2-core` | supported | supported | yes |
| Relay-aware calls | `c2-core` | supported | supported | yes |
| Generated client/service | `c2-codegen` | supported | supported | yes |
| No-payload/record/object graph | FastDB | supported | supported | yes |
| Owned/held/borrowed lifetime | `c2-core` + FastDB | projection | projection | yes |
| Structured C-Two error | `c2-error` | typed | typed exception | yes |
| Structured FastDB cause | FastDB + `c2-error` | preserved | preserved | yes |
| Python pickle/thread-local | Python SDK | not copied | explicitly nonportable | no |
| Advanced runtime embedding | `c2-core` | public crate | native binding | yes |

The checked-in parity JSON is byte-for-byte equivalent to the Core-generated native receipt. Syntax differences such as RAII versus context managers are projections rather than capability differences.

## Exact 18-row Rust/Python matrix

The three stable logical identities are:

| Payload | Contract descriptor SHA-256 | FastDB spec SHA-256 | Logical result SHA-256 |
|---|---|---|---|
| no payload | `a1e05b1f83b46bbe1aec46b91e953fdd12daea6067279e3cf3735d919d59987e` | none | `5c1bab644702feb18828ad26f7ab3b9560732c2d9fee50401f9c45a3d9a654ba` |
| `record.v1` | `0eeab1f5cdc6bac8ae7880794dd1b2c1f8eaac34e8e63c9139d6c40a266ce10c` | `95100cd0926d9da453919b68bcee70d3cbfe78a4b3e899d60b04efa96e5745b5` | `39367eb718976ba4e9ad7a5e20f6a88caf73f8f825fed43193d2cec2ab45858d` |
| `object_graph.v1` | `a60647161cc12058cc829fdbaa30f999ffad40888ef24172fe3d4dd86bb4c98d` | `661e2e5fef945b44c4111e72fe4321a9d1659eca7849b170b6996dccc3acd35a` | `d074886a31eb4ad65bcd0163cc120eaa1ea76f6e97f18f9124bb008bab9d0e88` |

Every row records one non-replayed business request and response, an observed route UID/revision, the expected `DirectIpc` or `ExplicitRelay` path, exact package hashes, contract-release ref, descriptor/spec identity, and logical result:

1. `no-payload__rust-client__rust-host__direct`
2. `no-payload__rust-client__rust-host__relay`
3. `no-payload__rust-client__python-host__direct`
4. `no-payload__rust-client__python-host__relay`
5. `no-payload__python-client__rust-host__direct`
6. `no-payload__python-client__rust-host__relay`
7. `record-v1__rust-client__rust-host__direct`
8. `record-v1__rust-client__rust-host__relay`
9. `record-v1__rust-client__python-host__direct`
10. `record-v1__rust-client__python-host__relay`
11. `record-v1__python-client__rust-host__direct`
12. `record-v1__python-client__rust-host__relay`
13. `object-graph-v1__rust-client__rust-host__direct`
14. `object-graph-v1__rust-client__rust-host__relay`
15. `object-graph-v1__rust-client__python-host__direct`
16. `object-graph-v1__rust-client__python-host__relay`
17. `object-graph-v1__python-client__rust-host__direct`
18. `object-graph-v1__python-client__rust-host__relay`

Receipt SHA-256 is `44608ba23d823fb550a67e3f18751d42c6dfdad39b62cd71c8d0a70c6e00a72d`.

## Generated TypeScript real calls

The TypeScript candidate runs under Node `v25.8.1` on darwin arm64. It installs only the retained package tarballs, generates all contract trees through the retained `c3`, and calls real Rust or Python hosts. The exact rows are:

| Row | Host | Observed path |
|---|---|---|
| `no-payload__direct-ipc` | Rust | `DirectIpc` |
| `no-payload__explicit-relay` | Python | `ExplicitRelay` |
| `no-payload__relay-aware-local-ipc` | Python | `RelayAwareLocalIpc` |
| `no-payload__relay-aware-http` | Rust | `RelayAwareHttp` |
| `record-v1__direct-ipc` | Rust | `DirectIpc` |
| `record-v1__explicit-relay` | Python | `ExplicitRelay` |
| `record-v1__relay-aware-local-ipc` | Python | `RelayAwareLocalIpc` |
| `record-v1__relay-aware-http` | Rust | `RelayAwareHttp` |
| `object-graph-v1__direct-ipc` | Rust | `DirectIpc` |
| `object-graph-v1__explicit-relay` | Python | `ExplicitRelay` |
| `object-graph-v1__relay-aware-local-ipc` | Python | `RelayAwareLocalIpc` |
| `object-graph-v1__relay-aware-http` | Rust | `RelayAwareHttp` |

The receipt also proves the frozen FastDB digest mismatch cause, checked-view invalidation, materialization survival, idempotent release, opaque allocator rejection/release, route-token-bound probes and calls, same-path fallback denial, pre-dispatch-only HTTP retry, and dispatch-uncertain non-replay. Receipt SHA-256 is `3ed4ef96c8c11aad38a1acc468d6ded8812cac238b527c4913f1fc5c21544c60`. It explicitly records `browser_runtime: unverified`.

## Lifetime claim strength

- Owned Rust and Python responses open a copy-backed FastDB payload owner. They do not prove response-SHM direct construction.
- Held responses keep the C-Two response lease and FastDB owner together; release invalidates the checked FastDB owner before releasing the transport lease.
- Borrowed service inputs are call-scoped; return, early error, adapter error, unwind, and shutdown paths invalidate the checked owner before releasing the request lease.
- Materialized FastDB values survive source-owner invalidation.
- Rust request adaptation uses bounds-checked owned bytes after transport storage; Python uses the same copy-backed receive meaning.
- `unsafe_buffer`, raw NumPy arrays, and raw pointers remain explicit escapes whose aliases cannot be mechanically revoked.
- Direct/staged FastDB backing reports remain authoritative inside FastDB. This C-Two candidate does not claim resource-time construction into final transport backing.

## Fresh versus retained evidence

Retained evidence consists of the immutable FastDB and C-Two candidate artifacts, their canonical manifests, prior FastDB P1-P5 Core correctness gates, and the implementation-task receipts. Fresh candidate evidence consists of FastDB manifest revalidation against all seven owner artifacts, exact ABI-117 inspection of the retained dynamic library, a packaged system-mode Rust consumer, the complete affected FastDB and C-Two source gates, and a cloned replay of the isolated Rust/Python/Node consumers plus both runtime matrices. The production/package inputs remain exactly the implementation commits `7eb74734926bd8fe911229eee9744a6dd8172487` and `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02`; only closure documentation differs.

The fresh FastDB gates pass 344 Python tests in 2.90 seconds, Python package construction, native/Wasm construction, 57 TypeScript tests, the Rust workspace tests, and 118 CI tests plus 120 subtests in 30.34 seconds. The retained FastDB candidate independently passes its canonical manifest verifier, exact 117-symbol ABI check, and relocated system-mode `fastdb`/`fastdb-sys` Rust consumer.

The fresh C-Two implementation-tree gates pass Core, CLI, Rust SDK, and Python native formatting and strict Clippy; their Rust test suites; 866 Python SDK tests in 98.91 seconds using the exact candidate npm tarballs; Python 3.10 compileall; 28 Node tests plus typecheck and pack check; and 126 repository tests in 1.23 seconds. The focused manifest/registry/consumer/receipt validator suite passes 87 tests in 0.40 seconds.

The cloned candidate replay passes all 4/4 isolated consumers, exactly 18/18 Rust/Python rows, and exactly 12/12 generated TypeScript rows with no skip or xfail. Its package-consumer receipt is byte-identical to the retained receipt. The regenerated portable and TypeScript receipts have transient SHA-256 values `53134a9d0b5096a1bdce99ba7bf92fa638ecb43fe4a0de3dc5f045fe487b6316` and `cb4883aefd658835a3b9ad773415d89c0fdd9ba99a64b445317fc686e97b2366`; exhaustive comparison shows that their only differences from the retained receipts are the expected fresh `route.uid` values for the 18 and 12 newly registered runtime routes. Every stable contract, package, path, result, counter, lifetime, error, and platform field is identical. These transient receipts are not retained and do not replace the original evidence hashes listed above.

Documentation-only closure edits do not invalidate those artifacts because package-input production diffs from the two implementation commits are required to remain empty. Final documentation, JSON, relative-link, manifest, boundary, and `git diff --check` gates are rerun after this report is written.

## Same-agent audits

The context-owning agent completed both the specification/ownership audit and a separate correctness/lifetime/portability/package/maintenance audit. This preserves the full cross-repository context requested by the user but is explicitly not independent or subagent review.

The specification/ownership pass mapped all 16 approved completion criteria to source, package, receipt, documentation, final-gate, or cleanup evidence. Cargo metadata confirms that `c2-core` has no FastDB dependency; Rust and Python both consume that Core; exactly one `ContractLimits` Rust definition exists; generated Rust code uses only the supported `c_two` seam and the official `fastdb` crate; the checked-in 12-row parity JSON is byte-identical to the Core-generated receipt; and production scans find no public `call-db`, `call_db`, `c2-sdk`, or replacement FastDB authority. Result: 0 Critical, 0 Important, and 0 unresolved material Minor ownership findings.

The correctness/lifetime/portability/package/maintenance pass reviewed route-token binding, pre-dispatch-only retry, dispatch-uncertain non-replay, checked admission arithmetic, structured error causes, response invalidation before lease release, callback-scoped borrowed input cleanup, archive inventories, version-only/offline Rust resolution, no-index Python installation, tarball-only Node resolution, toolchain/platform claims, and cleanup behavior. The fresh candidate replay and complete source gates above close its executable checks. It found one documentation-only stale future-tense dependency in Toodle TD-0002; that text was corrected to require official immutable artifacts and Toodle adapter consumption, and `MANIFEST.json` was regenerated. Result after correction: 0 Critical, 0 Important, and 0 unresolved material Minor findings.

Any production/package-input finding would have invalidated the candidate and required a complete rebuild. No such finding occurred. The final post-commit worktree/gate verification and removal of execution-owned rebuildable outputs complete criteria 14 and 15 without changing candidate provenance.

## Intentionally open capabilities

- Official immutable FastDB and C-Two package distribution and downstream version pinning
- Hosted Linux/macOS/Windows verification, version changes, push, tag, publication, and release
- Browser TypeScript runtime and browser-compatible ownership/transport packaging
- C-Two C++ route/transport SDK and C++ CRM codegen
- Streaming, cancellation/backpressure, and async unary APIs
- Post-dispatch retry/deduplication and a structured cross-SDK transport-failure phase
- Compatible contract ranges beyond exact descriptor identity
- Authority signature, publisher trust, authorization, and revocation
- Toodle `CRMContractRef`, official artifact consumption, `toodle-c2`, Rust `IToodle`, and Phase 0B consumer integration
- Direct construction into final C-Two response backing and a revocable raw-buffer abstraction
- Authenticated SHM allocation capabilities, full `u64` TypeScript route revisions, existing-tree update, crash-durable/hostile-parent publication, and non-regular artifact metadata

Every deliberate limit retains its reason, impact, owner, dependency, and executable exit criterion in [`contract-release-deferred-capabilities.md`](../issues/contract-release-deferred-capabilities.md). Local candidate truth is not official release truth.

## External actions not performed

- No push to any remote
- No tag creation or replacement
- No package registry publication
- No GitHub/GitLab release
- No artifact upload
- No hosted CI invocation or hosted-pass claim
- No pull request creation
- No Toodle production integration
- No Kubernetes work
