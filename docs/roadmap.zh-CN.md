# C-Two 当前路线图

最后审阅和排序日期：2026-05-18。

这是 C-Two 当前 0.x 线的维护版路线图。`docs/plans/`、`docs/spikes/`、`docs/superpowers/` 下的历史设计文档是冻结档案，里面可能仍使用 ICRM、Component、`@cc.runtime.connect` 或旧包路径等过期术语。

## 状态定义

| 状态 | 含义 |
| --- | --- |
| 稳定 | 当前架构已经实现，并且会继续作为公开方向保留。 |
| 规划中 | 仍属于当前 C-Two 方向，但还没有作为完整公开能力实现。 |
| 远期 | 方向上重要，但依赖进一步设计、下游需求或前置能力。 |
| 历史档案 | 只作为上下文保留，不应当当成当前实现计划。 |

## 实现顺序

路线图工作应按以下顺序接手，除非后续 review 明确取代这个排序。这个顺序本身就是路线图的一部分：先稳定资源到 CRM 的投影、契约身份、codec 身份和调用 envelope，再扩展调用形态、语言 SDK 和治理表面，可以最大限度减少协议返工。

| 顺序 | 工作流 | 为什么排在这里 | 完成标准 |
| --- | --- | --- | --- |
| 1 | 资源优先 contract descriptor、codec refs、provider resolution 与 codegen foundation | 跨语言客户端不能依赖 Python annotation、pickle fallback 或临时 transferable hooks。已有资源类需要低侵入地投影为 CRM，同时 SDK 扩张前必须把契约身份和 payload codec 身份分开。 | `c-two.contract.v1` 已规范化、可 canonical hash、由 Rust 校验；`CodecRef` 作为 opaque payload ABI identity 已规范化；pickle 标记为 Python-only；primitive/control envelope 让 portable JSON-safe 参数不再依赖 pickle；provider resolution 能识别 built-in、可复用 py-arrow record、Arrow 显式 batch view、fastdb、标准外部格式和 opaque custom families；portable export 对 unresolved codec fail-fast；resource-to-CRM infer 与 SDK codegen 能生成经过校验的 artifact 和 codec implementation stub，且不会假装实现未知 payload codec。 |
| 2 | 契约版本兼容与 descriptor 稳定化 | semver 与 range matching 需要第 1 项的语言中立 descriptor，否则兼容规则会固化 Python descriptor 细节。 | 精确契约校验仍是安全底线；semver 或 range matching 由 Rust 所有；歧义匹配或 ABI 不兼容匹配返回结构化错误。 |
| 3 | unary 调用的 `auth_hook` 与 call metadata | metadata 会改变每一次调用的 envelope，所以必须在 async、streaming 或 SDK 扩张前落地。 | thread-local、direct IPC 和 HTTP relay 都能传递 metadata；hook 能接受或拒绝调用；策略仍留在 C-Two 之外。 |
| 4 | unary 调用的 dry-run hooks | dry-run 依赖 hook/metadata 表面，应该在更多调用形态需要同样语义前定义清楚。 | dry-run 不执行资源副作用；不支持的调用形态显式失败；hook 可见 metadata 已文档化。 |
| 5 | async unary API | async 应该扩展已经稳定的 unary envelope，而不是形成第二套协议设计。 | `cc.connect_async()` 和 async proxy/context manager 覆盖 thread-local、direct IPC 和 HTTP relay，且不回归同步 API。 |
| 6 | Rust 所有的 telemetry、backpressure 和自适应内存生命周期策略 | streaming 会放大内存保留和取消风险，所以运行时策略需要先存在。 | buddy decay、dedicated segment timeout、chunk assembler timeout 分开建模；telemetry 可测试；Python 仍只是薄 facade。 |
| 7 | Streaming RPC 与 pipeline 语义 | streaming 依赖 async、metadata、取消和背压。现有分块载荷传输只是传输原语。 | stream identity、生命周期帧、取消、顺序、错误传播和资源释放都有规范和端到端测试覆盖。 |
| 8 | Rust SDK 第一个真实切片 | Rust 可以直接复用现有 core crates，是验证跨语言契约可用性的最低摩擦路径。 | Rust client 能通过受支持 transport 调用 Python 托管的 CRM；契约不匹配会失败；文档包含可运行命令。 |
| 9 | TypeScript 客户端与 fastdb codec | 浏览器侧 SDK 依赖稳定 descriptor、codec 标注、metadata 和兼容语义。 | Node/browser 支持边界明确；只暴露 eligible codec；relay/control-plane 规则不会在 TypeScript 中被重新实现成另一套。 |
| 10 | 全局发现与命名空间治理 | relay mesh 已经传播 route；更广义的 discovery 是治理/admin 层，不能削弱生产调用的 contract-scoped resolution。 | discovery 通过显式 admin 表面返回候选 route metadata；正常调用仍拒绝 name-only 或 contract-mismatched resolution。 |

## 接手规则

后续 agent 和人类开发者应从上面第一个尚未完成的工作流开始，除非用户明确重定向范围。实现前先读这个路线图、相关当前代码和 vision 文档；资源优先契约和 codec-provider 工作尤其要先读 [`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md)；`docs/plans/`、`docs/spikes/`、`docs/superpowers/` 只作为历史证据，不作为当前事实源。

语言无关的运行时机制必须留在 Rust core 或 FFI 中。Python 和未来 SDK 应保持薄层：typed facade、transferable serialization 和语言 ergonomics。不要把 config resolution、relay routing、retries、caching、contract authority、scheduler policy、backpressure 或 memory lifecycle policy 放进单一 SDK。

不要创建投机性的 SDK 脚手架。新的 SDK 目录只有在同时包含可运行切片、测试或示例，以及证明端到端可用的文档时才应该出现。

每个工作流都应拆成小而可 review 的切片。涉及调用路径时，按适用范围验证 thread-local、direct IPC 和 HTTP relay；涉及协议时，在声明完成前补上 Rust core 测试和 Python 集成覆盖。

## 稳定能力

| 能力 | 当前状态 |
| --- | --- |
| 核心 RPC 框架 | CRM 契约、资源注册和客户端代理是当前公开模型。 |
| IPC 传输与共享内存 | 直连 IPC 和 Rust 所有的 SHM 分配是核心传输能力。 |
| HTTP 中继 | `c3 relay` 是独立的 Rust 中继运行时；Python SDK 不嵌入中继服务生命周期。 |
| 中继网格发现 | 中继节点会在 mesh 中传播带完整契约信息的路由注册和撤销。 |
| 分块载荷传输 | 大载荷可以通过 IPC/relay 的分块传输跨边界传递。这里指 payload chunking，不是用户语义上的 Streaming RPC。 |
| 心跳与连接管理 | IPC 和 relay 运行时负责连接健康和生命周期管理。 |
| 读/写并发控制 | CRM 方法元数据会进入原生 route concurrency enforcement。 |
| 统一配置 | Rust `c2-config` 是环境变量、`.env` 和默认值解析的事实源。 |
| 多平台 Python 打包 | CI 为支持的平台和 Python 版本构建并发布 wheel。 |
| 极端载荷磁盘溢出 | Rust 内存管理会在配置阈值要求时把超大载荷溢写到文件。 |
| Hold 模式与 `from_buffer` 零拷贝 | `cc.hold()` 和 transferable `from_buffer` 通过原生 lease accounting 支持保留式 buffer view。 |
| SHM 驻留监控 | `cc.hold_stats()` 暴露 SDK 可见的保留 buffer 统计。 |

## 规划中

| 能力 | 尚未完成 | 实现方向 |
| --- | --- | --- |
| 资源优先 contract descriptor、codec refs、provider resolution 与 codegen foundation | Python 侧现在已经有 `CodecRef`、`@cc.transferable(codec_ref=...)`、`cc.use_codec(...)`、`cc.bind_codec(...)`、resource/CRM conformance check、对 pickle-only wire ref fail-fast 的 portable export、Rust `c2-contract` 对 `c-two.contract.v1` 的校验、Rust `c2-codegen` TypeScript skeleton 生成、`c3 contract validate/export/infer/codegen typescript`、`cc.infer_crm_from_resource(...)`、用于 JSON-safe primitive/control 参数和返回值的 `c-two.control.json.v1`、可复用 `c_two.providers.arrow` 对 Arrow IPC record、普通调用物化但在 `cc.hold(...)` 下返回 `ArrowBatchView` 的 `list[record]` batch、通过 `HeldResult.buffer` 暴露 retained raw wire buffer、用于普通调用也返回 view 的显式 `arrow.Batch[record]`、可复用 `c_two.providers.custom` 对 portable custom codec stub 的打包、基于 CRM context 生成 Arrow schema identity 的 grid payload、可选 fastdb provider 冒烟覆盖，以及 fastdb 侧基于 `fastdb.schema.v1` 的 TypeScript helper stub 生成。后续剩余缺口是可复用 fastdb provider 打包，以及把真实 TypeScript/WASM payload codec runtime 接到已生成的 helper stub 后面。 | 保持 C-Two core codec-neutral。继续让 portable descriptor 的 canonical validation 留在 Rust `c2-contract`，让 primitive/control envelope 与大 payload codec 保持分离，让 provider 默认 identity 从 CRM context 派生而不是制造 record-local version sprawl，在 provider-specific runtime 完成前只生成诚实的 SDK skeleton、codec requirements 与 implementation stubs，并继续把 pickle 限定为 Python-only fallback。 |
| 契约版本兼容 | 精确 CRM 契约校验已经存在，但还没有 semver 或 range-based 兼容协商。 | 保留精确 route validation 作为安全底线。在 Rust `c2-contract` 中增加兼容匹配，再通过 Python 薄 facade 暴露，例如在 `cc.connect(...)` 上支持版本要求。 |
| `auth_hook` 与 call metadata | C-Two 还没有提供上层实现 AuthN/AuthZ 所需的 metadata 透传和 hook 表面。 | C-Two 提供机制，下游系统如 Toodle 保留策略。metadata 必须显式且保持契约作用域。 |
| dry-run 钩子 | endgame 架构把 dry-run hooks 列为 M1 收尾项，但公开契约还没有定义。 | 先定义 hook 语义：模拟什么、能看到哪些 metadata、哪些副作用必须禁止。 |
| 异步接口 | 目前没有公开的 `cc.connect_async()`，也没有 async CRM proxy/context manager API。 | 增加 `AsyncCRMProxy` 和原生支撑的异步调用路径。复用 Rust async IPC/HTTP 内核，不引入 Python-only transport stack。 |
| 自适应内存生命周期策略 | buddy pool decay、dedicated segment crash timeout、chunk assembler timeout 仍是固定策略。 | 先增加 Rust 所有的 telemetry 和策略配置，再让 timeout/decay 行为依据观测延迟和内存压力调整。 |
| Streaming RPC 与 pipeline 语义 | 分块载荷传输已经存在，但面向用户的 server/client/bidirectional streaming RPC 还没有。 | 在 async API 形状确定后定义 stream identity、生命周期帧、取消、顺序和背压。可以复用 payload chunking，但不要把两者混为一谈。 |

## 远期方向

| 能力 | 方向 |
| --- | --- |
| Rust SDK 第一个真实切片 | 先做可运行的 Rust SDK 切片，复用 `c2-wire`、`c2-ipc`、`c2-http` 和 `c2-contract`，不要创建空占位目录。 |
| TypeScript 客户端与 fastdb codec | 这是浏览器原生 CRM 客户端的 endgame 前提，但依赖 codec 标注、跨语言 descriptor 和稳定的兼容性故事。fastdb 应该作为第一个 codec 试点，而不是 CRM IDL 本身。 |
| 全局发现与命名空间治理 | relay mesh 已经处理路由传播。更广义的 discovery/admin 表面应返回候选 route metadata，不能用 name-only lookup 取代生产调用的 contract-scoped resolution。 |

## 历史参考

| 文档 | 现在应如何使用 |
| --- | --- |
| [`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md) | 当前资源优先 CRM 投影、opaque codec refs、provider resolution、portable export，以及 fastdb/py-arrow codec 试点方向。 |
| [`docs/plans/c-two-rpc-v2-roadmap.md`](./plans/c-two-rpc-v2-roadmap.md) | 历史路线图档案。仍有上下文价值，但旧术语和旧路径是预期现象。 |
| [`docs/vision/endgame-architecture.md`](./vision/endgame-architecture.md) | 长期架构边界和里程碑方向。用它理解能力为什么重要，不要把它当成逐项实现清单。 |
| [`docs/vision/sota-patterns.md`](./vision/sota-patterns.md) | RPC 模型和剩余高层缺口的设计模式参考。 |
