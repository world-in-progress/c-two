# C-Two Current Roadmap

Last reviewed and ordered: 2026-07-24.

This is the maintained roadmap for C-Two's 0.x line. Current portable-payload authority is:

1. [`2026-07-24 portable-payload contract composition design`](./superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md)
2. [`2026-07-24 implementation plan`](./superpowers/plans/2026-07-24-portable-payload-contract-composition.md)
3. [`contract-release deferred capabilities`](./issues/contract-release-deferred-capabilities.md)

Documents under `docs/plans/`, `docs/reviews/`, and older `docs/superpowers/` dates are retained evidence. They are not current authority unless this roadmap explicitly says otherwise.

## Status Definitions

| Status | Meaning |
| --- | --- |
| Stable | Implemented and part of the current public direction. |
| Proven locally | Implemented and exercised at the audited source revisions, but not necessarily published or verified by hosted CI. |
| Planned | Required for the current direction but not yet a complete supported capability. |
| Future | Directionally useful after prerequisite design and evidence. |
| Archive | Historical evidence only. |

## Current Foundation

| Capability | State |
| --- | --- |
| Resource-oriented runtime | CRM contracts, resource registration, typed proxies, route-scoped exact contract admission, direct IPC, HTTP relay, relay mesh, concurrency, shutdown, and lifecycle remain the public runtime model. |
| Contract release identity | Rust `c2-contract` validates and canonicalizes `c-two.contract.v2`, derives exact route-independent `ContractReleaseRef` values, verifies resolved descriptor bytes, and adds a route name only when deriving `ExpectedRouteContract`. |
| Portable payload contract | A method declares zero or one input and zero or one output with `@cc.transfer(...)`; its nested `fastdb.payload.v1` value is opaque to C-Two and compiled by FastDB Core. |
| Artifact composition | Rust `c2-codegen` delegates nested specifications, preserves structured FastDB errors, verifies identity and hashes, composes C-Two plus FastDB artifacts deterministically, and atomically publishes a complete new regular-file tree at an absent destination. |
| Language projections | Python exposes the v2 authoring/runtime/codegen facade. Generated Rust and Python artifacts compile/import and run in real bidirectional payload calls. Generated TypeScript retains the existing C-Two transport surface and type-checks with Core-owned FastDB payload artifacts. |
| Runtime payload proof | Record, object-graph, and no-payload calls pass across Rust client → Python resource, Python client → Rust resource, and Rust client → Rust resource. The proof covers scalar/`str`/`wstr`/bytes/list/null values, sharing/cycles, materialization, structured mismatch errors, and checked invalidation. |
| Honest backing boundary | The proven Rust and Python receive adapters are copy-backed. `cc.hold()` and borrowed-input policy enforce owner/lease invalidation; they do not prove direct construction in final response shared memory. |
| Python-only prototype path | Ordinary Python methods without an explicit portable binding may still use pickle locally. Portable descriptor export/codegen rejects them with diagnostics. |

The local portable foundation consumes audited FastDB commit `6b9d0a55f27bb22fd13f867f321db821f21e777c`. FastDB package metadata remains 0.1.22, its Rust crates remain unpublished, and no FastDB version, push, tag, publication, or release is implied by this roadmap.

## Ordered Product Work

Start at the first incomplete item whose prerequisites and authorization are available. Do not bypass an owner-repository gap with a C-Two-local payload implementation.

| Order | Workstream | Why it comes here | Exit criteria |
| --- | --- | --- | --- |
| 1 | Immutable FastDB package distribution | The current integration relies on an audited sibling checkout/local wheel and cannot be reproduced from registries alone. | Authorized FastDB Rust/Python/TypeScript artifacts are immutable and fetchable; C-Two pins them and passes clean-environment package, codegen, runtime, and interoperability gates. |
| 2 | Complete Rust SDK | The real Rust proof currently composes lower-level public crates rather than one supported ergonomic facade. | One supported Rust SDK covers the proven IPC client/host plus HTTP/relay, discovery, lifecycle, contract operations, examples, and cross-language tests without capabilities unavailable to Python. |
| 3 | Contract compatibility | Exact release matching is the safety floor; semver/range rules need stable release content first. | Rust-owned rules reject ambiguity and ABI-incompatible matches and project identical behavior across SDKs. |
| 4 | Call metadata and admission hooks | Upper layers need a transport-consistent mechanism for identity and policy decisions, but C-Two must not own policy. | Thread-local, IPC, and relay calls carry bounded metadata; hooks can accept/reject calls; downstream systems remain the policy authority. |
| 5 | Dry-run mechanism | Impact analysis depends on the same explicit metadata/admission boundary. | Dry-run semantics state what is evaluated, which side effects are forbidden, and how unsupported methods fail. |
| 6 | Async unary API | Async should extend one stable unary contract instead of creating a second protocol. | Supported SDKs provide async proxy/context-manager behavior over the same route, error, payload, and lifetime semantics. |
| 7 | Telemetry, backpressure, and adaptive memory lifecycle | Streaming amplifies cancellation and retention risks. | Rust owns bounded telemetry and policy for pools, dedicated segments, chunks, queues, and cancellation; SDKs remain thin facades. |
| 8 | Streaming RPC | Existing chunking is byte transport, not a user-visible stream. | Stream identity, frames, ordering, cancellation, errors, backpressure, and resource release are specified and proven end to end. |
| 9 | Publishable TypeScript SDK/runtime | The generated transport foundation exists, but packaging and browser/Node boundaries are incomplete. | A supported package consumes the same contract/release/payload identities and proves its declared Node/browser lifetime and transport matrix. |
| 10 | Discovery and namespace governance | Relay mesh propagates live routes; broad search is an admin/governance concern. | A separate discovery surface returns candidate metadata while ordinary calls remain exact contract-scoped and never fall back to name-only admission. |

## Parallel Performance and Hardening Tracks

These tracks must not be smuggled into unrelated feature work or used to overstate current behavior:

| Track | Current limit | Closure |
| --- | --- | --- |
| Direct final backing | Resource-time construction into final C-Two backing is not implemented across Rust and Python. | A public FastDB backing adapter reports direct/staged behavior truthfully, avoids post-build repacking on the proven path, and passes fallback/lifetime tests in both SDKs. |
| Portable-payload benchmark | The current suite has correctness proof but no reviewed explicit-`Payload` throughput claim. | A reproducible benchmark records workload, environment, distributions, copy/direct/staged facts, and retained-owner behavior. |
| Outer descriptor limits | FastDB limits nested values, while C-Two has no versioned caller-configurable outer-document limits. | A Rust-owned `ContractLimits` API bounds source, structure, methods, and extracted nested bytes across Rust/Python/CLI. |
| Artifact publication hardening | Publication is new-tree-only and not a power-loss or hostile-parent durability receipt. | A versioned prior-manifest/update protocol and platform-specific durable/no-follow publication pass fault and race tests. |
| Strict Clippy baselines | Existing core/native warnings prevent repository-wide `-D warnings` claims. | Resolve or narrowly justify every diagnostic without blanket suppression and rerun complete functional gates. |

Every active limit, reason, impact, owner, dependency, and executable closure criterion lives in the [deferred-capabilities issue](./issues/contract-release-deferred-capabilities.md).

## Handoff Rules

- Read this roadmap, the current design/plan, the deferred-capabilities issue, and current code before implementation.
- Keep FastDB Core as the sole payload semantic/codegen authority. C-Two owns only outer contract, route/transport/lifecycle, generated CRM adapters, and final artifact composition.
- Keep cross-language runtime mechanisms in Rust core with thin SDK projections. Do not give the Rust SDK hidden payload capabilities or make Python the language-neutral authority.
- Do not create speculative SDK directories, compatibility aliases, payload sidecars, or alternative parsers.
- For call-path changes, verify thread-local, direct IPC, and relay behavior where applicable. For payload changes, prove Rust/Python equivalence, structured errors, and lifetime behavior.
- Record any deliberate limitation in `docs/issues/` before calling a slice complete.

## Historical References

| Document | How to use it now |
| --- | --- |
| [`2026-07-24 portable-payload contract composition design`](./superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md) | Current owner and architecture contract. |
| [`2026-07-24 implementation plan`](./superpowers/plans/2026-07-24-portable-payload-contract-composition.md) | Current implementation and verification sequence. |
| [`cross-language contract architecture`](./vision/cross-language-contract-codec-architecture.md) | Short current boundary summary. |
| [`endgame architecture`](./vision/endgame-architecture.md) | Long-term C-Two/Toodle/domain boundary. |
| [`c-two-rpc-v2 roadmap archive`](./plans/c-two-rpc-v2-roadmap.md) | Historical context only. |
