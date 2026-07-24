# C-Two 当前路线图

最后审阅与排序：2026-07-24。

这是 C-Two 0.x 的当前维护路线图。Portable-payload 的权威顺序为：

1. [`2026-07-24 portable-payload contract composition design`](./superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md)
2. [`2026-07-24 implementation plan`](./superpowers/plans/2026-07-24-portable-payload-contract-composition.md)
3. [`contract-release deferred capabilities`](./issues/contract-release-deferred-capabilities.md)

`docs/plans/`、`docs/reviews/` 与较早日期的 `docs/superpowers/` 文档只保留历史证据；除非本路线图明确引用，否则不再是当前权威。

## 状态定义

| 状态 | 含义 |
| --- | --- |
| Stable | 已实现，并属于当前公共方向。 |
| 本地已证明 | 已在审计后的源码 revision 上实现并执行，但不代表已经发布或 hosted CI 已通过。 |
| Planned | 当前方向需要，但尚未形成完整受支持能力。 |
| Future | 依赖进一步设计、证据或前置能力。 |
| Archive | 仅作为历史证据。 |

## 当前地基

| 能力 | 当前状态 |
| --- | --- |
| 面向资源的 runtime | CRM contract、resource registration、typed proxy、contract-scoped exact admission、direct IPC、HTTP relay、relay mesh、并发、shutdown 与 lifecycle 仍是公共 runtime model。 |
| Contract release identity | Rust `c2-contract` 校验并 canonicalize `c-two.contract.v2`，派生 route-independent exact `ContractReleaseRef`，验证 resolved descriptor bytes，并只在派生 `ExpectedRouteContract` 时加入 route name。 |
| Portable payload contract | Method 用 `@cc.transfer(...)` 声明零或一个 input 与零或一个 output；nested `fastdb.payload.v1` value 对 C-Two 是 opaque，并交给 FastDB Core 编译。 |
| Artifact composition | Rust `c2-codegen` 委托 nested specification、保留 structured FastDB error、校验 identity/hash、确定性组合 C-Two 与 FastDB artifacts，并在不存在的 destination 原子发布完整 regular-file tree。 |
| Language projection | Python 暴露 v2 authoring/runtime/codegen facade；generated Rust/Python artifact 已实际 compile/import 并完成双向 payload call；generated TypeScript 保留既有 C-Two transport surface，并与 Core-owned FastDB payload artifact 一起 type-check。 |
| Runtime payload proof | Record、object-graph 与 no-payload method 已通过 Rust client → Python resource、Python client → Rust resource、Rust client → Rust resource。覆盖 scalar/`str`/`wstr`/bytes/list/null、sharing/cycle、materialize、structured mismatch error 与 checked invalidation。 |
| 真实 backing 边界 | 已证明的 Rust/Python receive adapter 是 copy-backed。`cc.hold()` 与 borrowed-input policy 负责 owner/lease invalidation，不代表已直接构造到最终 response shared memory。 |
| Python-only prototype | 没有 portable binding 的普通 Python method 仍可在本地使用 pickle；portable descriptor export/codegen 会诊断并拒绝它们。 |

当前本地 portable 地基消费审计后的 FastDB commit `6b9d0a55f27bb22fd13f867f321db821f21e777c`。FastDB package metadata 仍为 0.1.22，Rust crates 尚未发布；本路线图不暗示任何 FastDB version、push、tag、publish 或 release 已发生。

## 有序产品工作

从授权与前置条件均可用的第一个未完成项开始。不得用 C-Two 自有 payload 实现绕过 owner repository 的缺口。

| 顺序 | Workstream | 为何在这里 | Exit criteria |
| --- | --- | --- | --- |
| 1 | Immutable FastDB package distribution | 当前集成依赖审计后的 sibling checkout/local wheel，无法仅从 registry 复现。 | 经授权的 FastDB Rust/Python/TypeScript artifacts 可按 immutable identity 获取；C-Two 固定它们并通过 clean-environment package、codegen、runtime 与 interoperability gates。 |
| 2 | 完整 Rust SDK | 真实 Rust proof 目前组合 lower-level public crates，而不是一个受支持的 ergonomic facade。 | 一个受支持 Rust SDK 覆盖已证明 IPC client/host、HTTP/relay、discovery、lifecycle、contract operations、examples 与 cross-language tests，且不拥有 Python 无法获得的能力。 |
| 3 | Contract compatibility | Exact release matching 是安全底线；semver/range 规则必须建立在稳定 release content 上。 | Rust-owned 规则拒绝 ambiguity 与 ABI-incompatible match，并在各 SDK 投影同一行为。 |
| 4 | Call metadata 与 admission hooks | 上层需要 transport-consistent identity/policy mechanism，但 policy 不属于 C-Two。 | Thread-local、IPC 与 relay call 携带有界 metadata；hook 可准入/拒绝；下游系统仍是 policy authority。 |
| 5 | Dry-run mechanism | Impact analysis 依赖同一个显式 metadata/admission boundary。 | Dry-run 明确评估内容、禁止的副作用与 unsupported method failure。 |
| 6 | Async unary API | Async 应扩展同一个稳定 unary contract，而不是形成第二协议。 | 受支持 SDK 在同一 route/error/payload/lifetime semantics 上提供 async proxy/context manager。 |
| 7 | Telemetry、backpressure 与 adaptive memory lifecycle | Streaming 会放大 cancellation 与 retention 风险。 | Rust 拥有 pool、dedicated segment、chunk、queue 与 cancellation 的有界 telemetry/policy；SDK 保持 thin facade。 |
| 8 | Streaming RPC | 现有 chunking 是 byte transport，不是 user-visible stream。 | Stream identity、frame、ordering、cancellation、error、backpressure 与 resource release 被规格化并端到端证明。 |
| 9 | 可发布 TypeScript SDK/runtime | Generated transport 地基已存在，但 package 与 browser/Node 边界尚未闭环。 | 一个受支持 package 消费相同 contract/release/payload identity，并证明声明的 Node/browser lifetime 与 transport matrix。 |
| 10 | Discovery 与 namespace governance | Relay mesh 传播 live route；广义搜索属于 admin/governance concern。 | 独立 discovery surface 返回 candidate metadata，普通 call 仍保持 exact contract-scoped，绝不退化为 name-only admission。 |

## 并行性能与加固轨道

这些轨道不得偷偷混入无关 feature，也不得用来夸大当前能力：

| 轨道 | 当前限制 | Closure |
| --- | --- | --- |
| Direct final backing | 尚未跨 Rust/Python 实现 resource-time construction into final C-Two backing。 | Public FastDB backing adapter 真实报告 direct/staged，已证明路径无 post-build repack，并在两个 SDK 通过 fallback/lifetime tests。 |
| Portable-payload benchmark | 当前有 correctness proof，但没有审阅后的显式 `Payload` throughput claim。 | 可复现 benchmark 记录 workload、environment、distribution、copy/direct/staged facts 与 retained-owner behavior。 |
| Outer descriptor limits | FastDB 会限制 nested value，但 C-Two 尚无 versioned caller-configurable outer-document limits。 | Rust-owned `ContractLimits` API 在 Rust/Python/CLI 一致限制 source、structure、method 与 extracted nested bytes。 |
| Artifact publication hardening | 当前只支持 new-tree publication，不是 power-loss 或 hostile-parent durability receipt。 | Versioned prior-manifest/update protocol 与 platform-specific durable/no-follow publication 通过 fault/race tests。 |
| Strict Clippy baseline | 既有 core/native warning 阻止 repository-wide `-D warnings` 声明。 | 不用 blanket suppression 地修复或精确说明每条 diagnostic，并重跑完整 functional gates。 |

所有 active limit、原因、影响、owner、dependency 与 executable closure criteria 都记录在[延后能力 Issue](./issues/contract-release-deferred-capabilities.md)。

## Handoff 规则

- 实现前阅读本路线图、当前 design/plan、deferred-capabilities Issue 与当前代码。
- FastDB Core 始终是 payload semantic/codegen 的唯一权威。C-Two 只拥有 outer contract、route/transport/lifecycle、generated CRM adapter 与最终 artifact composition。
- Cross-language runtime mechanism 放在 Rust core，并通过 thin SDK projection 暴露；不得给 Rust SDK 隐藏 payload 能力，也不得让 Python 成为 language-neutral authority。
- 不创建 speculative SDK directory、compatibility alias、payload sidecar 或 alternative parser。
- 修改 call path 时按需验证 thread-local、direct IPC 与 relay；修改 payload 时证明 Rust/Python equivalence、structured error 与 lifetime behavior。
- 任何刻意保留的限制都必须先写入 `docs/issues/`。

## 历史参考

| 文档 | 当前用途 |
| --- | --- |
| [`2026-07-24 portable-payload contract composition design`](./superpowers/specs/2026-07-24-portable-payload-contract-composition-design.md) | 当前 owner 与 architecture contract。 |
| [`2026-07-24 implementation plan`](./superpowers/plans/2026-07-24-portable-payload-contract-composition.md) | 当前 implementation 与 verification sequence。 |
| [`cross-language contract architecture`](./vision/cross-language-contract-codec-architecture.md) | 当前边界摘要。 |
| [`endgame architecture`](./vision/endgame-architecture.md) | C-Two/Toodle/domain 长期边界。 |
| [`c-two-rpc-v2 roadmap archive`](./plans/c-two-rpc-v2-roadmap.md) | 仅作历史背景。 |
