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
| 1 | FastDB-first CRM ABI、bridge 与 codegen foundation | 跨语言客户端不能依赖 Python annotation、pickle fallback 或临时 transferable hooks。已有资源类仍需要低侵入地投影为 CRM，但这个工作流的 portable payload ABI 应收敛到 fastdb call-db，而不是开放式载荷子系统。 | `c-two.contract.v1` 仍是由 Rust 校验的 callable contract；portable method input/output wire refs 解析为 fastdb call-db `PayloadAbiRef`；Python 原生 fallback 明确标记为 Python-only，并在 schema/codegen 工作流里给出诊断；`cc.register(...)` 支持 per-method resource bridge，且覆盖 thread-local、direct IPC 和 relay；fastdb 暴露 scalar、single feature、`Array[T]`、`Batch[T]`、tuple return 与 object-graph planning；grid 用 fastdb 证明普通调用和 held 调用端到端可用，portable descriptor 中没有 `python-pickle-default`。 |
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

后续 agent 和人类开发者应从上面第一个尚未完成的工作流开始，除非用户明确重定向范围。实现前先读这个路线图、相关当前代码、[`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md)，以及当前 fastdb-first 计划 [`docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`](./plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md)；更早的 `docs/plans/`、`docs/spikes/`、`docs/superpowers/` 文件只作为历史证据，除非当前路线图明确链接它。

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
| FastDB-first CRM ABI、bridge 与 codegen foundation | C-Two 现在拥有 `c_two.fastdb` integration helpers、FastDB call-db planning、bridge derivation、通过 `c3 contract codegen typescript --fastdb-schema` 生成 FastDB-backed TypeScript helper，以及跨 transport 的 FastDB ABI 冒烟验证。FastDB 拥有 authored value types、schema/storage/serialization/view/runtime substrate 和 `fastdb4py` annotation surface。 | Portable CRM payload 只走 FastDB call-db 或 no-payload；pickle 只保留为 Python-only fallback；C-Two runtime 的 route/relay/IPC/scheduler/lease 层仍不解析 FastDB 存储内部结构。 |
| TypeScript IPC SHM response reader seam | 生成的 `createIpcEncodedTransport(...)` 现在会解码 Rust buddy SHM reply metadata，保留 server handshake 中的 SHM prefix/segment metadata，调用注入式 `responseShmReader.read(block, destination?)` 与 `responseShmReader.release(block)` hooks，并能把 allocator-owned destination view 传给 reader，因此 FastDB-owned payload ingress 也覆盖 SHM reply，同时不会返回 alias 已释放 mapping 的普通 payload；生成的 dedicated-only Node/POSIX helper 还能读取 dedicated response segment 并标记 `read_done`，已有 structural native-buddy backend wrapper，并且 Rust `c2-mem-ffi` 已提供 response-pool substrate，可以打开 deterministic server buddy segment、复制到 caller-owned destination，并通过 c2-mem `free_at` 释放已读取的 non-dedicated block；现在还已有可构建的 `@c-two/c2-mem-ffi` TypeScript binding-package adapter surface，generated c2-mem-ffi facade helper 可把 lower-level response pool factory 适配为 `responseShmReader` hook；generated TypeScript 也已有 structural `createNodeIpcConnect({ net })` connector，并用真实 `node:net` Unix-domain-socket frame smoke 验证生成 IPC frame 流量；FastDB grid TypeScript smoke 还会把生成的 IPC 路径与 c2-mem-ffi Node loader 组合到真实 Python/Rust direct IPC server，证明 Rust buddy response 可以通过 c2-mem-ffi 读取、复制到 FastDB-owned 或 generated owned-byte destination 并释放。缺少 reader、缺少 release hook、畸形 buddy metadata、超限 SHM data size、invalid allocator output 和 dedicated helper 误用 non-dedicated block 都会在 transport 边界失败。 | C-Two generated IPC helper 继续聚焦 runtime：生成的 Node/POSIX helper 已覆盖 dedicated block 的 file-system read/write、`read_done` 标记和 release/unlink 基础路径，native-buddy wrapper 只委托给注入 backend，真正的 c2-mem allocator mapping/free 留在 Rust FFI/runtime 层而不是 generated TypeScript；剩余工作是初始 grid proof 之外的更广泛真实 IPC 覆盖、可发布 runtime packaging，以及初始 Node/POSIX C ABI loader 之外的 Bun/Deno 或等价 native/FFI loader。 |
| TypeScript IPC chunked request transport | 生成的 `createIpcEncodedTransport(...)` 现在暴露 `requestChunkSize`，小请求仍走 inline，较大请求会发送与 Rust 兼容的 chunked call frames，发送前要求 server 宣告 `CAP_CHUNKED`，写入前拒绝非法 chunk size 和无法用 chunk header 表达的 chunk count，并在 chunked request 部分写失败后关闭连接。 | 这补上 FastDB call-db payload 和其它 codec bytes 的 request-side chunked bytes fallback；当没有配置 `requestShmWriter`，或 runtime constraints 不允许 SHM 时，它仍是大 payload 的通用 bytes 路径。 |
| TypeScript IPC request SHM writer transport | 生成的 `createIpcEncodedTransport(...)` 现在接受 `requestShmWriter` 与 `requestShmThreshold`，会在 client handshake 中宣告 writer 的 SHM prefix 与 buddy segments，把较大请求发送为 Rust-compatible `FLAG_CALL_V2 | FLAG_BUDDY` frame，payload 形态为 `[buddy_payload][call_control]`，并在发送前校验 block metadata 的 u16/u32 范围和已宣告 segment range。生成代码会 snapshot writer metadata，invalid 或 write-failure block 会本地 release，buddy frame 写失败会关闭连接，non-dedicated 成功 block 交给 Rust server 的 cross-process free authority，dedicated block 只在收到 server response 证明已消费后 release；生成的 dedicated-only Node/POSIX writer helper 可以创建 page-aligned dedicated segment 并在 release 时 unlink，native-buddy wrapper 可以把 non-dedicated writer seam 委托给注入 backend，Rust `c2-mem-ffi` 已提供 request-side C ABI request-pool substrate，`@c-two/c2-mem-ffi` 已提供可构建的 TypeScript binding-package adapter surface，generated c2-mem-ffi facade helper 也可把 lower-level request pool 适配为 `requestShmWriter` hook；generated TypeScript 也已有 structural `createNodeIpcConnect({ net })` connector，并用真实 `node:net` Unix-domain-socket frame smoke 验证生成 IPC frame 流量；FastDB grid TypeScript smoke 还会把生成的 IPC 路径与 c2-mem-ffi Node loader 组合到真实 Python/Rust server，证明 request buddy frame 会被 Rust authority 接受和消费，并在本地标记 consumed 而不做 client-side release。 | C-Two generated IPC helper 继续聚焦 runtime：生成的 Node/POSIX helper 已覆盖 dedicated request create/write/unlink 和 dedicated response read/`read_done` release；剩余工作是初始 grid proof 之外的更广泛真实 IPC 覆盖、可发布 runtime packaging，以及初始 Node/POSIX C ABI loader 之外的 Bun/Deno 或等价 native/FFI loader。 |
| FastDB bridge coverage status note | 最近的 bridge guardrail 切片已经把上面宽泛描述中的显式覆盖面继续扩大：scalar bridge input/output coercion、WSTR/BYTES feature-output bridge 与 held-view access、下游 fastdb4ts WSTR/single-BYTES retained call-db column runtime 与 object-graph row runtime、generated-helper WSTR/single-BYTES object-graph smoke coverage、abstract container annotation、tuple-row 与 variadic tuple row container、split-column container、table-object factory、column-container input/output，以及 internal non-FastDB diagnostics 都已经有 source-to-sink 测试。 | 后续 bridge 工作应被理解为这些显式形态之外的广义自动合成，而不是这些已由 active plan phase 覆盖形态的证明缺口。 |
| FastDB call-db strict codec shape guardrail | C-Two strict TypeScript codegen 现在会拒绝省略 `schema="fastdb.call-db.schema.v1"`、使用其它 schema、省略 `schema_sha256`、或省略 `bytes` capability 的 `org.fastdb.call-db` wire ref，避免 strict client 生成 FastDB codec registry 无法满足的 codec requirement。 | C-Two 只在 strict-mode admission rule 中维护当前 portable CRM ABI 的最小形状校验；更深的 fastdb schema/table 校验继续由 FastDB codegen/runtime 拥有。 |
| TypeScript held `buffer-view` capability guardrail | C-Two 生成的 client 和 codec stub 只有在 output codec requirement 声明 `buffer-view` 时才会使用 `fromBuffer`；FastDB 生成的 registry 也遵循同一规则，因此 columnar held output 可以返回 `FastdbC2CallDbView`，而 object-graph 或 decode-only output 会走 `decode` 物化。 | 把 `buffer-view` 当成 contract capability，而不是实现对象上碰巧存在的函数。Generated binding registry 只有在 descriptor 声明能力时才能暴露 hook，C-Two 生成的 hold path 也必须先检查 descriptor capability 再信任 hook。 |
| TypeScript relay timeout parity | 生成的 TypeScript HTTP relay transport 现在用 `callTimeoutMs` 限制显式 relay 调用，并用 `resolveTimeoutMs` 限制 relay-aware route resolve；两个选项都会拒绝小数、负数、非有限值和过大值，因此只有显式 `0` 表示禁用超时。 | timeout、route cache、retry 和 contract routing 行为继续归 C-Two 生成 transport 所有，而不是塞进 FastDB helper；helper 代码只在 C-Two transport 之上组合 codec。 |
| TypeScript route cache TTL guardrail | 生成的 TypeScript relay-aware transport 现在对 `routeCacheTtlMs` 使用同样的非负安全整数毫秒 guardrail，避免小数被截断成 `0` 后静默禁用 route cache。 | cache 语义必须显式：`0` 才禁用缓存，正安全整数按完整 route contract identity 缓存，非法值在 transport 构造期失败。 |
| TypeScript route cache invalidation scope | 生成的 TypeScript relay-aware transport 现在用与 cache lookup 相同的完整 route-contract key 失效 stale route cache entry，因此一个 CRM contract 的 stale route 不会驱逐同名 route 下另一个 contract 的有效缓存。 | route cache storage、refresh、stale invalidation 和 current-route preference 都必须按完整 CRM contract identity 定界，而不是只按 route name 定界。 |
| TypeScript resolve transport retry parity | 生成的 TypeScript relay-aware resolve 现在会把 fetch failure、resolve body failure、invalid resolve JSON 和 resolve timeout 归类为 `C2HttpTransportError`，并像 Rust relay-aware resolve 一样按 `maxAttempts` 重试。 | 只重试 control-plane resolve transport/5xx 类错误和 stale-route data-plane 类错误；普通 CRM method failure 或模糊 data-plane failure 不做 replay。 |
| TypeScript maxAttempts guardrail | 生成的 TypeScript relay-aware transport 现在对齐 Rust route-attempt 语义：`0` 归一为一次尝试，安全整数 `1..=32` 有效，负数、小数、非有限值和过大值都会在构造期失败。 | retry policy 必须确定并在构造期校验；不能把用户输入截断或 clamp 成另一套 retry 策略。 |
| TypeScript route contract identity guardrail | 生成的 TypeScript HTTP relay transport 现在会在构造 expected CRM headers 或 resolve query 前校验 route contract schema、非空 CRM namespace/name/version 以及 ABI/signature hash。 | 公开 bytes transport 遇到调用方传入的 malformed contract identity 应在本地失败；generated client 仍使用生成常量，但 helper API 不能让坏 identity 进入 relay/network 路径。 |
| TypeScript route contract text guardrail | 生成的 TypeScript HTTP relay transport 现在会用 Rust `c2-contract` 的 CRM tag 边界校验调用方传入的 route contract namespace/name/version 文本，包括 255 字节 UTF-8 wire length、首尾空白、control characters 和 path/tag separators。 | 公开 bytes transport 必须在构造 expected-contract headers 或 relay resolve query 前，与 Rust 保持一致地校验 contract identity 文本。 |
| TypeScript route path text guardrail | 生成的 TypeScript HTTP relay transport 现在会在 resolve 或 data-plane fetch 前校验 `routeName` 和 method 字符串，包括 255 字节 UTF-8 wire length、首尾空白、control characters 和 separator policy，同时保留 `/` 作为合法 route name 字符。 | route 与 method path component 属于本地 SDK 边界输入；畸形文本不能被编码成 relay URL，generated TypeScript 必须与 Rust route/method validation 保持一致。 |
| TypeScript HTTP transport boundary guardrail | 生成的 TypeScript HTTP relay transport 现在会在构造期或 resolve-payload normalization 阶段校验 transport options、没有 whitespace、credentials、query 或 fragment 的绝对 HTTP(S) base/anchor/resolved relay URL、`fetch` 和用户 headers；非保留 custom header 可以贯通，但 `Content-Type` 与 C-Two expected-contract headers 不能被覆盖，包括大小写变体。 | route contract 与 relay URL authority 继续归 C-Two generated transport 所有。FastDB codec registry 这类 FastDB helper 只应组合在该 transport 之上，不应接收或改写保留的 HTTP identity headers，也不应继承畸形 relay endpoint。 |
| TypeScript HTTP response length guardrail | 生成的 TypeScript HTTP relay transport 现在会在 payload allocator 调用、成功响应 body stream 读取、无 allocator 或 CRM-error `arrayBuffer()` 物化前拒绝大于 `2_147_483_647` 的声明 `Content-Length`。 | 声明响应长度校验属于 C-Two transport 边界，避免 FastDB/WASM allocator 或普通 generated client 把超大声明 payload 当成 allocation request。 |
| TypeScript IPC response frame length guardrail | 生成的 TypeScript IPC transport 现在会在调用 `readExactly(payloadLen)` 或进入 payload allocator 前，拒绝大于 `2_147_483_647` 的声明 frame payload length。 | generated direct IPC response read 必须在 frame header 边界就有上限；FastDB/WASM allocator 只应看到经过 C-Two transport 拒绝超大声明后的 payload length。 |
| TypeScript IPC chunked response assembly guardrail | 生成的 TypeScript IPC transport 现在只有在校验 chunk metadata、`CHUNK_LAST`、chunk index、稳定的 total size/chunk count 和最终 `total_size` 完整性后，才会组装 chunked success reply。 | chunked IPC response integrity 属于 C-Two transport 边界；FastDB 这类 helper 只应接收完成后的 payload bytes 或 allocator-owned payload，而不是 IPC chunk metadata。 |
| TypeScript HTTP response length-match guardrail | 生成的 TypeScript HTTP relay transport 现在会拒绝无 allocator 成功响应和 500 CRM-error binary body 中，`arrayBuffer()` 物化后的 byte length 与声明 `Content-Length` 不一致的响应。 | 让 binary response integrity check 在 allocator-backed stream read 和普通物化响应路径之间保持一致。 |
| TypeScript HTTP text body length guardrail | 生成的 TypeScript HTTP relay transport 现在会在读取 non-500 data-plane error text body 和 relay resolve text body 前拒绝超大的声明 `Content-Length`，覆盖 resolve error 与 resolve JSON 响应。 | text-body control-plane 和 error-path read 也必须位于与 binary payload read 相同的 transport 边界之后，但不把 JavaScript string length 伪装成 byte-level integrity check。 |
| TypeScript HTTP custom header guardrail | 生成的 TypeScript HTTP relay transport 现在会在显式 HTTP relay 和 relay-aware transport 上拒绝不符合 HTTP token 语法的 custom header name，以及包含 CR、LF 或 NUL 的 header value；`__proto__` 这类有 JavaScript object prototype 语义但仍是合法 token 的 header name 会通过 null-prototype normalization 保留为普通数据属性。 | custom header forwarding 只对合法普通 header 开放；generated transport 必须在 fetch 看到调用方元数据前，本地阻止 header injection，也不能因为对象原型语义丢掉合法 caller header。 |
| FastDB direct-engine BOOL producer parity | FastDB Python `ColumnEngine`、`ObjectEngine` 和固定表 `Table.fill(...)` 现在会在进入 C numeric writer 前，对 scalar `BOOL` producer field 使用同一套显式 bool parser；mutable engine 也会在 list writer 前覆盖 native `list[BOOL]`。 | 让 FastDB 自身 storage engine 与 call-db、bridge、fastdb4ts 的 producer 语义保持一致，同时不让 C-Two core 解析 FastDB field metadata。 |
| TypeScript data-plane transport error parity | 生成的显式 HTTP relay call 现在会把 fetch failure 以及成功/CRM-error 响应体读取 failure 包装为 `C2HttpTransportError`，对齐 Rust 的 `HttpError::Transport` 边界，但不增加 data-plane replay。 | transport 分类要方便 SDK 调用方判断错误，但仍保持普通 CRM/data-plane failure 不做模糊重放的规则。 |
| TypeScript resolve payload-shape retry parity | 生成的 TypeScript relay-aware resolve 现在会把 non-array resolve payload 和 malformed route payload shape 归类为 `C2HttpTransportError`，使 `maxAttempts` 能重试 Rust 侧 resolve JSON/serde transport error 对应的错误类。 | 只重试 malformed control-plane resolve payload 与其它 resolve transport/5xx failure；contract mismatch 保持不可重试，data-plane call 仍只在明确 stale-route 错误时重放。 |
| FastDB-first diagnose/artifact payload guardrail | `c3 contract diagnose`、`c3 contract infer --diagnose` 和 `c3 contract infer --artifacts` 现在都会在写出前校验 Python 诊断或 artifact 输出必须是 JSON object array，避免 portability warning pipeline 静默持久化不可消费的 payload，并让 resource-first 用户能在 portable export 失败前检查 inferred Python-only fallback、非 FastDB `PayloadAbiRef` warning 和导出 payload ABI artifacts。 | Python-native fallback 与非 FastDB `PayloadAbiRef` 在 fastdb-first 工作流中仍只是诊断入口，但 CRM-first 和 resource-first CLI 诊断产物以及 artifact bundle 都必须先做到机械可消费，用户才能依赖它修 contract/codegen。 |
| FastDB codec requirement key guardrail | FastDB helper generation 现在会把 optional media type 写入 codec binding，并拒绝同一 id/hash 的 FastDB refs 在 version、schema、media type 或 capabilities 上产生互相冲突的 C-Two registry key。 | method wire specs、C-Two codec stubs 与 FastDB codec registry 必须使用同一套 id/version/schema/hash/media/capability identity；不能再只按 schema hash 静默合并 refs。 |
| FastDB mutable abstract container bridge ergonomics | FastDB bridge derivation 现在会在与 `Mapping`、`Sequence` 相同的 mapping/sequence-shaped resource bridge families 中接受 `MutableMapping` 和 `MutableSequence` 标注。 | bridge 便利性留在 FastDB-owned helpers；C-Two 只在注册和调用边界应用派生出的 `cc.bridge(...)` hooks。 |
| FastDB fixed tuple return bridge | FastDB bridge derivation 现在会把固定长度 CRM tuple return 的每个 item 分别映射到已有 scalar、`Array`、single-feature 或 `Batch` output converter，并在派生期和运行期都有 arity guardrail。 | tuple-return 适配保持为 FastDB-owned bridge 逻辑，而不是 C-Two 内置 fastdb 特例；C-Two 只保存并调用最终派生出的 `cc.bridge(...)` output hook。 |
| FastDB table-like batch output bridge | FastDB bridge derivation 现在会通过 row-record export 方法把 DataFrame/Arrow/Polars 风格表对象适配为 `Batch[Feature]` 输出，再进入通用迭代逻辑，包括对象本身不可迭代但能导出 rows 的情况。 | 依赖特定表格库的识别不进入 C-Two；FastDB 负责标准化 row records，保留自定义 resource 返回对象原有的 row-iterable 行为，并用同一个 hook 覆盖 thread-local、direct IPC 和 relay。 |
| FastDB table-object batch input bridge | FastDB bridge derivation 现在可以把 `Batch[Feature]` row records 构造成 table/dataframe 风格 resource 参数，支持可用的 `from_records`、`from_pylist`、`from_dicts` 或单参数构造器，并对 malformed batch 与坏构造器早失败。 | 表格库适配继续留在 FastDB-owned bridge helpers；C-Two 只接收 `cc.bridge(input=...)` hook，并在 thread-local、direct IPC、relay 中一致应用。 |
| FastDB table-like batch input producer support | FastDB call-db serialization 与 derived bridge input hooks 现在会在 `Batch[Feature]` 输入校验前标准化 `to_pylist()`、`to_dicts()`、`to_dict("records")` 这类 row-record exporter。 | producer 侧表格便利性继续留在 FastDB；C-Two 在远程路径只看到 opaque call-db bytes，在 thread-local 路径只调用派生 bridge hook。 |
| FastDB Batch column item annotation guardrail | FastDB bridge derivation 现在会拒绝 `Batch[Feature]` tuple-column、dict/mapping-column 与 split-column resource 形态里的嵌套或非标量 item 注解，例如 `tuple[list[list[int]], list[int]]` 或 `Mapping[str, Sequence[list[int]]]`。 | grid 风格列式 resource 适配继续由 FastDB 拥有，并在 derivation 期完成校验；C-Two 只执行已经证明 column item shape 是 scalar-compatible 的 bridge hook。 |
| FastDB scalar Array bridge guardrail | FastDB `coerce_array(...)` 和派生的 `Array[Scalar]` output hook 现在会拒绝 mapping，避免 Python 把 mapping keys 误当成 array 元素。 | scalar-array 适配要和 batch、column malformed-source guardrail 保持一致；C-Two 仍只调用 FastDB-owned bridge hook。 |
| FastDB scalar Array producer iterable support | FastDB call-db serialization 现在会在 columnar 与 object-graph profiles 中接受 `Array[Scalar]` 输入和输出的普通 non-mapping、non-bytes-like iterable；FastDB bridge derivation 会把 CRM `Array[Scalar]` input 映射到 scalar sequence resource 参数（`list[...]`、`Sequence[...]`、`Iterable[...]`、`Iterator[...]`、`MutableSequence[...]` 及 aliases）或 tuple resource 参数（`tuple[Scalar, ...]`、bare `tuple` 与 bare `typing.Tuple`）并在派生期应用 annotation guardrail；C-Two FastDB integration smokes 用 tuple 和 sequence array input 覆盖 thread-local/direct IPC/relay，也用 tuple array output 覆盖 direct IPC/relay。 | scalar-array producer 与 bridge 便利性继续留在 FastDB；C-Two 在远程路径仍只看到 opaque call-db bytes，本地路径只调用派生 bridge hook。 |
| FastDB scalar Array input annotation guardrail | FastDB bridge derivation 现在会在 CRM `Array[Scalar]` input 上拒绝 mapping、set、fixed tuple、nested sequence item 和非标量 tuple item resource annotation，bridge 尚未注册前就失败。 | 防止 list-shaped half-bridge 进入 C-Two；被接受的 bridge hook 必须是带显式 item coercion 的 scalar sequence 或 tuple 转换。 |
| FastDB scalar bridge resource annotation guardrail | FastDB bridge derivation 现在会在 CRM scalar input/output bridge 上拒绝 mapping 或 sequence 这类非标量 resource annotation，避免生成误导性的 scalar hook。 | scalar bridge 适配继续由 FastDB 拥有并在 derivation 期校验；C-Two 只执行 resource 侧已经证明是 scalar-shaped 的 hook。 |
| FastDB single Feature bare-tuple bridge | FastDB bridge derivation 现在会把 bare `tuple` 和 bare `typing.Tuple` resource annotation 当作 single `Feature` input/output 的 schema-order field tuple，并在 output hook 运行期检查 arity。 | bare tuple 便利性继续留在 FastDB-owned bridge helpers；fixed tuple validation 仍保持显式，C-Two 只执行最终的 bridge hooks。 |
| FastDB mapping-row annotation guardrail | FastDB bridge derivation 现在会拒绝 key type 不是 `str` 的显式 mapping-shaped single-feature 与 batch row annotation。 | schema field-name 所有权留在 FastDB bridge helpers；C-Two 注册 bridge 之前就阻止非字符串 key 被误当成 feature field name。 |
| FastDB variadic tuple row Batch bridge | FastDB bridge derivation 现在会把 `tuple[Row, ...]` 识别为不可变的 `Batch[Feature]` row sequence 输入/输出，而固定长度 tuple 标注仍表示 tuple columns。 | 这属于 FastDB-owned bridge ergonomics；C-Two 只在 thread-local、direct IPC 和 relay 中执行最终派生出的 `cc.bridge(...)` hooks。 |
| FastDB batch sequence row annotation guardrail | FastDB bridge derivation 现在会拒绝 `Batch[Feature]` input/output row sequence 中类似 `list[int]` 的显式标量 item 注解，同时保留 bare `list` 和 `list[Feature]` 的 feature-row 路径。 | 防止错误 resource 注解延迟成运行期 row-field 错误；C-Two 仍只应用 FastDB-owned `cc.bridge(...)` hook。 |
| FastDB input hold ownership guardrail | FastDB call-db input transferable 在 `from_buffer` 只是物化值时会退出 C-Two 自动 input hold；grid direct IPC smoke 证明 `get_grid_infos` 调用后不会留下活跃 `resource_input` lease。 | 区分 input 物化和 output held view；只有真正返回 resource-facing retained view 的 FastDB hook 才应该自动选择 C-Two input hold。 |
| FastDB raw feature input hold ownership guardrail | Raw fastdb feature transferable 现在也使用相同的 auto-input-hold opt-out，因为它的 `from_buffer` 只是物化 feature 对象，而不是返回 retained resource-facing view。 | 让 raw feature FastDB 公共面和 call-db input ownership 规则保持一致；portable CRM 方法仍应优先使用 call-db envelope。 |
| FastDB free-threading row-read safety | FastDB `Table` row access、`iter_reuse()` 和 fallback string lookup 现在会用 per-table `RLock` 序列化 native `tryGetFeature(...)` row materialization；当前 free-threaded Python 环境下的 FastDB all-in-one Python suite 已经通过。 | FastDB 内部 native row materialization 的线程安全留在 FastDB；C-Two 只依赖稳定 FastDB 行为，不拥有 FastDB 内部锁。 |
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
| TypeScript 客户端与 fastdb codec | 这是浏览器原生 CRM 客户端的 endgame 前提，但依赖 fastdb-first CRM ABI、bridge 语义、codegen helpers 和稳定的兼容性故事。fastdb 拥有 portable payload ABI 和 codec runtime，C-Two 拥有 route/contract 语义。 |
| 全局发现与命名空间治理 | relay mesh 已经处理路由传播。更广义的 discovery/admin 表面应返回候选 route metadata，不能用 name-only lookup 取代生产调用的 contract-scoped resolution。 |

## 历史参考

| 文档 | 现在应如何使用 |
| --- | --- |
| [`docs/vision/cross-language-contract-codec-architecture.md`](./vision/cross-language-contract-codec-architecture.md) | 当前资源优先 CRM 投影、`PayloadAbiRef` identity、portable export，以及 fastdb-first 修订方向。 |
| [`docs/plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md`](./plans/2026-05-18-fastdb-first-crm-abi-and-bridge.md) | 当前 active implementation plan：让 fastdb call-db 成为 portable CRM payload ABI，并补齐 resource bridge。 |
| [`docs/plans/c-two-rpc-v2-roadmap.md`](./plans/c-two-rpc-v2-roadmap.md) | 历史路线图档案。仍有上下文价值，但旧术语和旧路径是预期现象。 |
| [`docs/vision/endgame-architecture.md`](./vision/endgame-architecture.md) | 长期架构边界和里程碑方向。用它理解能力为什么重要，不要把它当成逐项实现清单。 |
| [`docs/vision/sota-patterns.md`](./vision/sota-patterns.md) | RPC 模型和剩余高层缺口的设计模式参考。 |
