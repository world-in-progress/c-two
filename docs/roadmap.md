# C-Two Current Roadmap

Last reviewed and ordered: 2026-07-24.

This is the maintained roadmap for C-Two's 0.x line. Current portable-payload authority is:

1. [`2026-07-24 portable-payload contract composition design`](./superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md)
2. [`2026-07-24 Rust SDK and local-candidate design`](./superpowers/specs/2026-07-24-rust-sdk-portable-payload-local-release-candidate-design.md)
3. [`2026-07-24 Rust SDK and local-candidate implementation plan`](./superpowers/plans/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md)
4. [`2026-07-24 local-candidate closure report`](./reports/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md)
5. [`contract-release deferred capabilities`](./issues/contract-release-deferred-capabilities.md)

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
| Shared runtime authority | Language-neutral `c2-core` is the single owner of route selection, client/host calls, retry classification, error normalization, transport lease ordering, and runtime lifecycle. Rust and Python are projections; Python retains only Python-specific authoring/invocation glue and its explicitly nonportable pickle/thread-local path. |
| Language projections | The supported Rust user package is `c-two` / `c_two`; generated Rust targets that facade. Python projects the same Core through PyO3. Generated TypeScript runs against real Rust/Python hosts under Node while browser runtime remains unverified. |
| Runtime payload proof | The candidate receipt contains exactly 18/18 passing no-payload, record, and object-graph rows across Rust client → Python host, Python client → Rust host, and Rust client → Rust host over direct IPC and explicit relay. It covers scalar/`str`/`wstr`/bytes/list/null values, sharing/cycles, materialization, structured mismatch errors, and checked invalidation. |
| Generated TypeScript proof | The candidate receipt contains exactly 12/12 passing Node rows across direct IPC, explicit relay, relay-aware verified local IPC, and relay-aware HTTP with real `c3`, Rust/Python hosts, package tarballs, route/path observations, lifetime negatives, and non-replay checks. |
| Bounded admission and strict gates | Versioned `ContractLimits` bounds outer contract input before unbounded work. Core and Python native pass strict Clippy without blanket suppression, followed by complete language and repository gates. |
| Local package closure | One canonical 42-artifact manifest binds FastDB and C-Two implementation commits. Version-only Rust, no-index CPython 3.10/current, and tarball-only Node consumers pass outside sibling/source checkouts. |
| Honest backing boundary | The proven Rust and Python receive adapters are copy-backed. `cc.hold()` and borrowed-input policy enforce owner/lease invalidation; they do not prove direct construction in final response shared memory. |
| Python-only prototype path | Ordinary Python methods without an explicit portable binding may still use pickle locally. Portable descriptor export/codegen rejects them with diagnostics. |

The local candidate consumes FastDB implementation commit `7eb74734926bd8fe911229eee9744a6dd8172487` from C-Two implementation commit `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02`. FastDB package metadata remains 0.1.22/0.0.3 and C-Two candidate metadata remains 0.1.0/0.5.1 without publication. Source commit plus SHA-256 identifies these local bytes; no version, push, tag, hosted pass, publication, or release is implied by this roadmap.

## Ordered Product Work

Start at the first incomplete item whose prerequisites and authorization are available. Do not bypass an owner-repository gap with a C-Two-local payload implementation.

| Order | Workstream | Why it comes here | Exit criteria |
| --- | --- | --- | --- |
| 1 | Official immutable FastDB and C-Two package distribution | Local archive-only consumers are complete, but the exact artifacts are not published or hosted. | Authorized FastDB and C-Two Rust/Python/TypeScript/CLI artifacts are immutable and fetchable; production manifests pin them and repeat clean-environment package, codegen, runtime, and interoperability gates without local registries or sibling paths. |
| 2 | Contract compatibility | Exact release matching is the safety floor; semver/range rules need stable official release content first. | Rust-owned rules reject ambiguity and ABI-incompatible matches and project identical behavior across SDKs. |
| 3 | Call metadata and admission hooks | Upper layers need a transport-consistent mechanism for identity and policy decisions, but C-Two must not own policy. | Thread-local, IPC, and relay calls carry bounded metadata; hooks can accept/reject calls; downstream systems remain the policy authority. |
| 4 | Dry-run mechanism | Impact analysis depends on the same explicit metadata/admission boundary. | Dry-run semantics state what is evaluated, which side effects are forbidden, and how unsupported methods fail. |
| 5 | Async unary API | Async should extend one stable unary contract instead of creating a second protocol. | Supported SDKs provide async proxy/context-manager behavior over the same route, error, payload, and lifetime semantics. |
| 6 | Telemetry, backpressure, and adaptive memory lifecycle | Streaming amplifies cancellation and retention risks. | Rust owns bounded telemetry and policy for pools, dedicated segments, chunks, queues, and cancellation; SDKs remain thin facades. |
| 7 | Streaming RPC | Existing chunking is byte transport, not a user-visible stream. | Stream identity, frames, ordering, cancellation, errors, backpressure, and resource release are specified and proven end to end. |
| 8 | Publishable TypeScript SDK/runtime | The generated Node proof exists, but official packaging and browser boundaries remain incomplete. | A supported package consumes the same contract/release/payload identities and proves its declared Node/browser lifetime and transport matrix. |
| 9 | Discovery and namespace governance | Relay mesh propagates live routes; broad search is an admin/governance concern. | A separate discovery surface returns candidate metadata while ordinary calls remain exact contract-scoped and never fall back to name-only admission. |

## Parallel Performance and Hardening Tracks

These tracks must not be smuggled into unrelated feature work or used to overstate current behavior:

| Track | Current limit | Closure |
| --- | --- | --- |
| Direct final backing | Resource-time construction into final C-Two backing is not implemented across Rust and Python. | A public FastDB backing adapter reports direct/staged behavior truthfully, avoids post-build repacking on the proven path, and passes fallback/lifetime tests in both SDKs. |
| Portable-payload benchmark | The current suite has correctness proof but no reviewed explicit-`Payload` throughput claim. | A reproducible benchmark records workload, environment, distributions, copy/direct/staged facts, and retained-owner behavior. |
| Artifact publication hardening | Publication is new-tree-only and not a power-loss or hostile-parent durability receipt. | A versioned prior-manifest/update protocol and platform-specific durable/no-follow publication pass fault and race tests. |

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
| [`2026-07-24 Rust SDK and local-candidate design`](./superpowers/specs/2026-07-24-rust-sdk-portable-payload-local-release-candidate-design.md) | Current continuation that freezes Core/SDK parity, candidate packaging, and executable closure. |
| [`2026-07-24 Rust SDK and local-candidate plan`](./superpowers/plans/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md) | Completed implementation and verification sequence for the local candidate. |
| [`2026-07-24 local-candidate closure report`](./reports/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md) | Exact commits, hashes, receipts, claim strength, and open external actions. |
| [`cross-language contract architecture`](./vision/cross-language-contract-codec-architecture.md) | Short current boundary summary. |
| [`endgame architecture`](./vision/endgame-architecture.md) | Long-term C-Two/Toodle/domain boundary. |
| [`c-two-rpc-v2 roadmap archive`](./plans/c-two-rpc-v2-roadmap.md) | Historical context only. |
