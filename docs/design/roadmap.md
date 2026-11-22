# C-Two Roadmap

Status refreshed: 2026-11-22.

This page records the direction of C-Two's 0.x line: the current foundation, and the exit criteria of the product capabilities that are not yet built. Open work, its order and its status live only in the [backlog](backlog.md), a Chinese working record.

The current release candidate is C-Two 0.12.0 with c3 0.8.0 (IPC 17, SHM 4, C/FFI ABI 9, relay mesh 7), adding resource-owned publish/subscribe topics (same-host sealed SHM fan-out and single-hop / two-tier `c3 relay` HTTP fan-out) and unified `WritableSlab` + FastDB 0.3.0 zero-copy payload paths; see the [0.12.0 release notes](../releases/0.12.0.md) (and the latest published release, [C-Two 0.11.0 / c3 0.7.0](../releases/0.11.0.md)). FastDB is pinned to 0.3.0 (native source `ff097cca9c29cadc289e2a437f4720ecc56015c4`, ABI-118). The Rust SDK, Core and TypeScript bindings keep independent 0.1.0 versions, with no C-Two crates.io or npm publication.

`docs/reference/` owns current behavior. [CI and release qualification](../reference/ci-cd.md) distinguishes source validation, package validation and publication; each release retains its exact source and artifact receipts.

## Current Foundation

| Capability | State |
| --- | --- |
| Resource-oriented runtime | CRM contracts, resource registration, typed proxies, route-scoped exact contract admission, direct IPC, HTTP relay, relay mesh, concurrency, shutdown, and lifecycle remain the public runtime model. |
| Contract release identity | Rust `c2-contract` validates and canonicalizes `c-two.contract.v2`, derives exact route-independent `ContractReleaseRef` values, verifies resolved descriptor bytes, and adds a route name only when deriving `ExpectedRouteContract`. |
| Portable payload contract | A method declares zero or one input and zero or one output with `@cc.transfer(...)`; its nested `fastdb.payload.v1` value is opaque to C-Two and compiled by FastDB Core. |
| Artifact composition | Rust `c2-codegen` delegates nested specifications, preserves structured FastDB errors, verifies identity and hashes, composes C-Two plus FastDB artifacts deterministically, and atomically publishes a complete new regular-file tree at an absent destination. |
| Shared runtime authority | Language-neutral `c2-core` is the single owner of route selection, client/host calls, retry classification, error normalization, the copy-then-release order of transport payloads, and runtime lifecycle. Rust and Python are projections; Python retains only Python-specific authoring/invocation glue and its explicitly nonportable pickle/thread-local path. |
| Language projections | The supported Rust user package is `c-two` / `c_two`; generated Rust targets that facade. Python projects the same Core through PyO3. Generated TypeScript runs against real Rust/Python hosts under Node while browser runtime remains unverified. |
| Runtime payload proof | The exact 18-row matrix of no-payload, record and object-graph methods across Rust client → Python host, Python client → Rust host and Rust client → Rust host, over direct IPC and explicit relay, is a normal gate. It covers scalar/`str`/`wstr`/bytes/list/null values, sharing/cycles, materialization, structured mismatch errors, and checked invalidation. |
| Generated TypeScript proof | The July local candidate receipt contains exactly 12/12 passing Node rows across direct IPC, explicit relay, relay-aware verified local IPC, and relay-aware HTTP with real `c3`, Rust/Python hosts, package tarballs, route/path observations, lifetime negatives, and non-replay checks. |
| Bounded admission and strict gates | Fixed contract admission limits bound outer contract input before unbounded work. Core and Python native pass strict Clippy without blanket suppression, followed by complete language and repository gates. |
| Local package closure | The July local candidate's canonical 42-artifact manifest binds FastDB and C-Two implementation commits. Version-only Rust, no-index CPython 3.10/current, and tarball-only Node consumers passed outside sibling/source checkouts. |
| Local endpoint and lifecycle | One OS-derived endpoint, physical connection observations and structured bind/connect causes share Core on Linux, macOS and Windows x64/ARM64. Exact/scoped native maintenance retains identity checks. See the release notes for distribution gates and evidence. |
| IPC and relay memory policy | Single-owner release: only the pool owner changes its allocator, a loan ends with the peer's release record or peer end, and each direction copies a payload into a local copy and releases its lease before handing it on. Lazy buddy allocation, idle decay, independent `client`, `server` and `relay` role-domain `(M, F, N)` budgets, and budgeted relay HTTP upload/download streaming windows are implemented; broader cancellation/queue telemetry remains open. |
| Honest backing boundary | Over IPC SHM, direct-build-eligible FastDB `BuildPlan` values execute straight into `WritableSlab` shared memory (`0` staging copies), while receivers copy once from peer SHM into warm-recycled local `RecycledBytes` before `RELEASE` and open payloads zero-copy via FastDB `0.3.0` `ExternalBytes` (ADR 0001 2-copy floor). Holds and borrowed inputs own only local copies. |
| Python-only prototype path | Ordinary Python methods without an explicit portable binding may still use pickle locally. Portable descriptor export/codegen rejects them with diagnostics. |

The historical July local candidate consumed FastDB implementation commit `7eb74734926bd8fe911229eee9744a6dd8172487` from C-Two implementation commit `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02`. Its metadata was FastDB 0.1.22/0.0.3 and C-Two 0.1.0/0.5.1 without publication. Those exact bytes and receipts remain historical.

## Product Capabilities

These capabilities are not yet built; the backlog records their order. Do not bypass a gap in an owner repository such as FastDB with a C-Two-local payload implementation.

| Capability | Why it is needed | Exit criteria |
| --- | --- | --- |
| Official immutable FastDB and C-Two package distribution | FastDB 0.3.0 is published across PyPI, crates.io, npm and GitHub Release, and Python and c3 releases use a coordinated source-bound pipeline; official C-Two Rust/TypeScript distribution remains open. | Authorized FastDB and C-Two Rust/Python/TypeScript/CLI artifacts are immutable and fetchable; production manifests pin them and repeat clean-environment package, codegen, runtime, and interoperability gates without local registries or sibling paths. |
| Contract compatibility | Exact release matching is the safety floor; semver/range rules need stable official release content first. | Rust-owned rules reject ambiguity and ABI-incompatible matches and project identical behavior across SDKs. |
| Call metadata and admission hooks | Upper layers need a transport-consistent mechanism for identity and policy decisions, but C-Two must not own policy. | Thread-local, IPC, and relay calls carry bounded metadata; hooks can accept/reject calls; downstream systems remain the policy authority. |
| Dry-run mechanism | Impact analysis depends on the same explicit metadata/admission boundary. | Dry-run semantics state what is evaluated, which side effects are forbidden, and how unsupported methods fail. |
| Async unary API | Async should extend one stable unary contract instead of creating a second protocol. | Supported SDKs provide async proxy/context-manager behavior over the same route, error, payload, and lifetime semantics. |
| Telemetry, backpressure, and adaptive memory lifecycle | Streaming amplifies cancellation and retention risks. | Rust owns bounded telemetry and policy for pools, dedicated segments, chunks, queues, and cancellation; SDKs remain thin facades. |
| Streaming RPC | Server streams are delivered ([ADR 0003](../decisions/0003-server-streams.md)); client and bidirectional streams, and generated TypeScript clients reading streams through a relay, are not built yet. | Stream identity, frames, ordering, cancellation, errors, backpressure, and resource release are specified and proven end to end. |
| Resource-owned publish/subscribe | Delivered in the 0.12.0 candidate ([ADR 0005](../decisions/0005-publish-subscribe.md)): contract-level topics with unchanged legacy fingerprints, registration-scoped publishers and immutable deliveries over IPC 17, sealed multi-loan same-host SHM 4 fan-out, two-tier single-upstream `c3 relay` fan-out (`Host → A → B → R`, preserving mesh 7), `c2-codegen` topic surfaces in all three targets, bidirectional Rust ↔ Python portable E2E proof, Node C/FFI ABI 9 subscriptions, four-platform CI qualification and `R = 1/4` fan-out benchmarks. | A 1-to-N topic reaches local and remote subscribers without per-subscriber serialization, shared-memory writes or cross-machine transfer amplification, with truthful lag, capacity and lifecycle terminals. |
| Publishable TypeScript SDK/runtime | The generated Node proof exists, but official packaging and browser boundaries remain incomplete. | A supported package consumes the same contract/release/payload identities and proves its declared Node/browser lifetime and transport matrix. |
| Discovery and namespace governance | Relay mesh propagates live routes; broad search is an admin/governance concern. | A separate discovery surface returns candidate metadata while ordinary calls remain exact contract-scoped and never fall back to name-only admission. |

## Performance and Hardening Tracks

These tracks must not be smuggled into unrelated feature work or used to overstate current behavior:

| Track | Current limit | Closure |
| --- | --- | --- |
| Direct final backing | Delivered across Python (`BuildPlan` → `WritableSlab` direct SHM + `ExternalBytes` zero-copy receive) and Rust (`ExternalBytes` zero-copy receive) on FastDB 0.3.0; Rust send-side direct `BuildPlan` into `WritableSlab` and TypeScript zero-copy backing remain open. | A public FastDB backing adapter reports direct/staged behavior truthfully, avoids post-build repacking on the proven path, and passes fallback/lifetime tests in both SDKs. |
| Portable-payload benchmark | Delivered for Python + `c3 relay` across `1 KiB–1 GiB` single frames, `4 GiB` bounded-memory streamed migration, multi-subscriber fan-out (`@cc.stream` and `@cc.topic`), and 1M-row Kostya vs Ray 2.56 ([2026-11-22 report](../acceptance/2026-11-22-kb-to-gb-slab-benchmark.md)). | A reproducible benchmark records workload, environment, distributions, copy/direct/staged facts, and retained-owner behavior. |
| Artifact publication hardening | Publication is new-tree-only and not a power-loss or hostile-parent durability receipt. | A versioned prior-manifest/update protocol and platform-specific durable/no-follow publication pass fault and race tests. |

The [deferred-capabilities record](contract-release-deferred-capabilities.md) keeps the reasons, owners and closure criteria of the open contract-release items.

## References

- [Portable contracts and routing](../reference/contracts-and-routing.md#portable-contract-v2-and-artifact-composition): the current architecture contract.
- [Rust SDK](../../sdk/rust/README.md) and [development qualification](../reference/development.md).
- The July [local-candidate plan](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate-plan.md) and [closure report](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md): exact commits, hashes, receipts and claim strength of that historical candidate.
- [Endgame architecture](endgame-architecture.md): the long-term C-Two/Toodle/domain boundary.
