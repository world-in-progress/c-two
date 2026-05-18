# C-Two Current Roadmap

Last reviewed and ordered: 2026-05-18.

This is the maintained roadmap for C-Two's current 0.x line. Historical design notes under `docs/plans/`, `docs/spikes/`, and `docs/superpowers/` are frozen archives; they may use older terminology such as ICRM, Component, `@cc.runtime.connect`, or old package paths.

## Status Definitions

| Status | Meaning |
| --- | --- |
| Stable | Implemented in the current architecture and expected to remain part of the public direction. |
| Planned | Still needed for the current C-Two direction, but not yet implemented as a complete public capability. |
| Future | Directionally important, but dependent on further design, downstream demand, or prerequisite capabilities. |
| Archive | Historical context only; do not treat as the current implementation plan. |

## Implementation Sequence

Roadmap work should be picked up in this order unless a later review explicitly supersedes it. The ordering is part of the roadmap: it minimizes protocol churn by stabilizing resource-to-CRM projection, contract identity, codec identity, and call envelopes before adding new call shapes, language SDKs, or governance surfaces.

| Order | Workstream | Why it comes here | Exit criteria |
| --- | --- | --- | --- |
| 1 | Resource-first contract descriptor, codec refs, provider resolution, and codegen foundation | Cross-language clients cannot depend on Python annotations, pickle fallback, or ad hoc transferable hooks. Existing resource classes need a low-intrusion path into CRM projection, while contract identity must stay split from payload codec identity before SDK expansion. | `c-two.contract.v1` is specified, canonically hashed, and Rust-validated; `CodecRef` is specified as an opaque payload ABI identity; pickle is marked Python-only; primitive/control envelopes no longer use pickle for portable JSON-safe parameters; provider resolution can identify built-in, reusable py-arrow records, Arrow explicit batch views, fastdb, standard external, and opaque custom families; portable export fails on unresolved codecs; resource-to-CRM infer and SDK codegen produce validated artifacts and codec implementation stubs without pretending to implement unknown payload codecs. |
| 2 | Contract version compatibility and descriptor stability | Semver and range matching need the language-neutral descriptor from order 1; otherwise compatibility rules would hard-code Python descriptor details. | Exact contract validation remains the safety floor; semver or range matching is Rust-owned; ambiguous or ABI-incompatible matches fail with structured errors. |
| 3 | `auth_hook` and call metadata for unary calls | Metadata changes every call envelope, so it must land before async, streaming, or SDK expansion multiply the number of call paths. | Thread-local, direct IPC, and HTTP relay paths can pass metadata; hooks can accept or reject calls; policy remains outside C-Two. |
| 4 | Dry-run hooks for unary calls | Dry-run depends on the hook/metadata surface and should be defined before more call shapes need the same semantics. | Dry-run does not execute resource side effects; unsupported call shapes fail explicitly; hook-visible metadata is documented. |
| 5 | Async unary API | Async should extend a stable unary envelope instead of becoming a second protocol design. | `cc.connect_async()` and async proxy/context-manager behavior exist for thread-local, direct IPC, and HTTP relay without regressing the sync API. |
| 6 | Rust-owned telemetry, backpressure, and adaptive memory lifecycle policy | Streaming will amplify memory retention and cancellation risks, so runtime policy needs to exist first. | Buddy decay, dedicated segment timeout, and chunk assembler timeout are separately modeled; telemetry is testable; Python remains a thin facade. |
| 7 | Streaming RPC and pipeline semantics | Streaming depends on async, metadata, cancellation, and backpressure. Existing chunked payload transfer is only a transport primitive. | Stream identity, lifecycle frames, cancellation, ordering, error propagation, and resource release are specified and covered by end-to-end tests. |
| 8 | Rust SDK first real slice | Rust can reuse the existing core crates directly and is the lowest-friction proof that the cross-language contract is usable. | A Rust client can call a Python-hosted CRM through a supported transport; contract mismatch fails; docs include runnable commands. |
| 9 | TypeScript client with fastdb codec | Browser-facing SDK work depends on stable descriptors, codec annotations, metadata, and compatibility semantics. | Node/browser support boundaries are explicit; only eligible codecs are exposed; relay/control-plane rules are not reimplemented differently in TypeScript. |
| 10 | Global discovery and namespace governance | Relay mesh already propagates routes; broader discovery is a governance/admin layer and must not weaken production contract-scoped resolution. | Discovery returns candidate route metadata through an explicit admin surface; normal calls still reject name-only or contract-mismatched resolution. |

## Handoff Rules

Future agents and human developers should start from the first incomplete workstream in the sequence above unless the user explicitly redirects the scope. Before implementation, read this roadmap, the relevant current code, and the vision docs, especially [`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md) for resource-first contract and codec-provider work; treat `docs/plans/`, `docs/spikes/`, and `docs/superpowers/` as historical evidence, not as the current source of truth.

Keep language-neutral runtime mechanisms in Rust core or FFI. Python and future SDKs should stay thin: typed facades, transferable serialization, and language ergonomics only. Do not put config resolution, relay routing, retries, caching, contract authority, scheduler policy, backpressure, or memory lifecycle policy in a single SDK.

Do not create speculative SDK scaffolding. A new SDK directory should appear only with a runnable slice, tests or examples, and documentation that proves the slice works end to end.

Each workstream should be split into small reviewable slices. For call-path changes, verify thread-local, direct IPC, and HTTP relay behavior when applicable; for protocol-affecting changes, include Rust core tests and Python integration coverage before calling the slice complete.

## Stable

| Capability | Current state |
| --- | --- |
| Core RPC framework | CRM contracts, resource registration, and client proxies are the active public model. |
| IPC transport with shared memory | Direct IPC and Rust-owned SHM allocation are core transport capabilities. |
| HTTP relay | `c3 relay` is the standalone Rust relay runtime; the Python SDK does not embed relay server lifecycle. |
| Relay mesh discovery | Relay peers propagate contract-scoped route announcements and withdrawals through the mesh. |
| Chunked payload transfer | Large payloads can cross IPC/relay boundaries through chunked transport. This is payload chunking, not user-facing Streaming RPC. |
| Heartbeat and connection management | IPC and relay runtimes own connection health and lifecycle management. |
| Read/write concurrency control | CRM method metadata feeds native route concurrency enforcement. |
| Unified configuration | Rust `c2-config` is the source of truth for environment, `.env`, and default resolution. |
| Multi-platform Python packaging | CI builds and publishes Python wheels for supported platforms and Python versions. |
| Disk spill for extreme payloads | Rust memory management can spill oversized payloads to files when configured thresholds require it. |
| Hold mode and `from_buffer` zero-copy | `cc.hold()` and transferable `from_buffer` support retained buffer views through native lease accounting. |
| SHM residence monitoring | `cc.hold_stats()` exposes SDK-visible retained buffer statistics. |

## Planned

| Capability | What remains | Implementation direction |
| --- | --- | --- |
| Resource-first contract descriptor, codec refs, provider resolution, and codegen foundation | Python now has `CodecRef`, `@cc.transferable(codec_ref=...)`, `cc.use_codec(...)`, `cc.bind_codec(...)`, resource/CRM conformance checks, portable export fail-fast for pickle-only wire refs, Rust `c2-contract` validation for `c-two.contract.v1`, Rust `c2-codegen` TypeScript skeleton generation, `c3 contract validate/export/infer/codegen typescript`, `cc.infer_crm_from_resource(...)`, `c-two.control.json.v1` for JSON-safe primitive/control parameters and returns, reusable `c_two.providers.arrow` packaging for Arrow IPC records, `list[record]` batches that materialize normally but return `ArrowBatchView` under `cc.hold(...)`, retained raw wire buffer access via `HeldResult.buffer`, explicit `arrow.Batch[record]` for ordinary-call view returns, reusable `c_two.providers.custom` packaging for portable custom codec stubs, CRM-context generated Arrow schema identities for grid payloads, optional fastdb provider smoke coverage, and fastdb-owned TypeScript helper stub generation from `fastdb.schema.v1`. Remaining later gaps are reusable fastdb provider packaging and wiring real TypeScript/WASM payload codec runtimes behind the generated helper stubs. | Keep C-Two core codec-neutral. Keep canonical portable descriptor validation in Rust `c2-contract`, keep primitive/control envelopes separate from large payload codecs, derive provider defaults from CRM context instead of record-local version sprawl, generate only honest SDK skeletons plus codec requirements and implementation stubs until provider-specific runtimes exist, and keep pickle as Python-only fallback. |
| Contract version compatibility | Exact CRM contract validation exists, but semver or range-based compatibility negotiation does not. | Keep exact route validation as the safety floor. Add compatibility matching above it in Rust `c2-contract`, then expose a thin Python facade such as a version requirement on `cc.connect(...)`. |
| `auth_hook` and call metadata | C-Two does not yet provide the metadata pass-through and hook surface needed by upper layers to implement AuthN/AuthZ. | Put mechanism in C-Two, keep policy in downstream systems such as Toodle. Metadata must remain explicit and contract-scoped. |
| Dry-run hooks | The endgame architecture names dry-run hooks as an M1 closing item, but the public contract is not specified yet. | Specify the hook semantics before implementation: what is simulated, what metadata is visible, and what side effects are forbidden. |
| Async interfaces | There is no public `cc.connect_async()` or async CRM proxy/context-manager API. | Add `AsyncCRMProxy` and native-backed async call paths. Reuse Rust async IPC/HTTP internals instead of introducing a Python-only transport stack. |
| Adaptive memory lifecycle policy | Buddy pool decay, dedicated segment crash timeout, and chunk assembler timeouts are fixed-policy mechanisms. | Add Rust-owned telemetry and policy knobs first, then make timeout/decay behavior respond to observed latency and memory pressure. |
| Streaming RPC and pipeline semantics | Chunked payload transfer exists, but user-facing server/client/bidirectional streaming RPC does not. | Define stream identity, lifecycle frames, cancellation, ordering, and backpressure after the async API shape is clear. Reuse payload chunking where appropriate without conflating it with streaming calls. |

## Future

| Capability | Direction |
| --- | --- |
| Rust SDK first real slice | Start with a real Rust SDK slice that reuses `c2-wire`, `c2-ipc`, `c2-http`, and `c2-contract` instead of creating placeholder directories. |
| TypeScript client with fastdb codec | This is an endgame prerequisite for browser-native CRM clients, but it depends on codec annotations, cross-language descriptors, and a stable compatibility story. fastdb should be the first codec pilot, not the CRM IDL itself. |
| Global discovery and namespace governance | Relay mesh already handles route propagation. A broader discovery/admin surface should return candidate route metadata and must not replace production contract-scoped resolution with name-only lookup. |

## Historical References

| Document | How to use it now |
| --- | --- |
| [`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md) | Current architecture direction for resource-first CRM projection, opaque codec refs, provider resolution, portable export, and fastdb/py-arrow codec pilots. |
| [`docs/plans/c-two-rpc-v2-roadmap.md`](./plans/c-two-rpc-v2-roadmap.md) | Historical roadmap archive. It contains useful context, but old terminology and old paths are expected. |
| [`docs/vision/endgame-architecture.md`](./vision/endgame-architecture.md) | Long-term architecture boundary and milestone direction. Use it to understand why a capability matters, not as an implementation checklist. |
| [`docs/vision/sota-patterns.md`](./vision/sota-patterns.md) | Design-pattern reference for the RPC model and remaining high-level gaps. |
