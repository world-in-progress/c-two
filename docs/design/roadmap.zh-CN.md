# C-Two 路线图

状态更新：2026-11-03。

本页写 C-Two 0.x 的方向：现有的地基，以及还没做的产品能力各自的完成标准。还没做的事、它们的顺序与进度只记在[待办](backlog.md)里。

最新发版是 C-Two 0.10.1 与 c3 0.6.0。此后 `main` 合入了接口精简、缺陷与死代码修复、删除 owner 子系统、单一属主释放与服务端流（IPC 16、SHM 4、C/FFI ABI 8、relay mesh 7），尚未发版，见[未发布的改动](../releases/unreleased.md)。FastDB 固定 0.2.4（原生源码 `f82fa13adeae2b87a295bf984821c85c44b55a3d`）。Rust SDK、Core 与 TypeScript 绑定各自保持 0.1.0，C-Two 不在 crates.io 或 npm 发布。

`docs/reference/` 负责现行行为。[CI 与发行流程](../reference/ci-cd.md)区分源码验证、包验证与发布；各次发行保留确切源码与产物收据。

## 当前地基

| 能力 | 当前状态 |
| --- | --- |
| 面向资源的 runtime | CRM contract、resource registration、typed proxy、按路由的精确契约准入、direct IPC、HTTP relay、relay mesh、并发、shutdown 与 lifecycle 仍是公共 runtime model。 |
| Contract release identity | Rust `c2-contract` 校验并 canonicalize `c-two.contract.v2`，派生与路由无关的精确 `ContractReleaseRef`，验证 resolved descriptor bytes，并只在派生 `ExpectedRouteContract` 时加入 route name。 |
| Portable payload contract | Method 用 `@cc.transfer(...)` 声明零或一个 input 与零或一个 output；nested `fastdb.payload.v1` value 对 C-Two 是 opaque，并交给 FastDB Core 编译。 |
| Artifact composition | Rust `c2-codegen` 委托 nested specification、保留 structured FastDB error、校验 identity/hash、确定性组合 C-Two 与 FastDB artifacts，并在不存在的 destination 原子发布完整 regular-file tree。 |
| 共享的 runtime 权威 | 与语言无关的 `c2-core` 是路由选择、client/host 调用、重试分类、错误归一化、传输载荷"先复制后释放"的顺序与 runtime lifecycle 的唯一权威。Rust 与 Python 是投影；Python 只保留 Python 专有的编写与调用胶水，以及明确不可移植的 pickle 与 thread-local 路径。 |
| Language projection | 用户面 Rust SDK 为 `c-two` / `c_two`，生成的 Rust 以它为目标。Python 经 PyO3 投影同一个 Core。生成的 TypeScript 在 Node 下对真实的 Rust/Python host 运行；浏览器 runtime 仍未验证。 |
| Runtime payload proof | 精确的 18 行矩阵是常规门禁：no-payload、record 与 object-graph 方法，覆盖 Rust client → Python host、Python client → Rust host、Rust client → Rust host，经 direct IPC 与 explicit relay。覆盖 scalar/`str`/`wstr`/bytes/list/null、sharing/cycle、materialize、structured mismatch error 与 checked invalidation。 |
| 生成的 TypeScript 证明 | 7 月本地候选的收据含 12/12 通过的 Node 行：direct IPC、explicit relay、relay-aware 已验证本地 IPC 与 relay-aware HTTP，使用真实的 `c3`、Rust/Python host、包 tarball，并有 route/path 观测、lifetime 反例与不重放检查。 |
| 有界准入与严格门禁 | 固定的契约准入上限在无界工作之前限住外层契约输入。Core 与 Python native 通过不带整体豁免的严格 Clippy，之后是完整的语言与仓库门禁。 |
| 本地包闭环 | 7 月本地候选的规范 42 产物清单绑定 FastDB 与 C-Two 的实现提交。只按版本的 Rust、无索引的 CPython 3.10/当前版本与只用 tarball 的 Node 使用方，在 sibling/源码检出之外通过。 |
| 本地端点与生命周期 | 唯一 OS 派生、物理连接通知及结构化 bind/connect 原因在 Linux、macOS 和 Windows x64/ARM64 共用 Core；精确/scoped 原生维护保留身份校验。发行门禁与证据见发布说明。 |
| IPC 内存策略 | 单一属主释放：只有属主修改自己的分配器；借出以对端的释放记录或对端结束为止；两个方向都先把载荷复制成本地副本、释放租约，再往下交。lazy buddy、idle decay 与三个有限方向预算已实现；HTTP 缓冲治理、更广的取消/队列遥测仍未闭环。 |
| 真实 backing 边界 | 已证明的 Rust/Python receive adapter 是 copy-backed。hold 与借用输入都只持有本地副本，都不代表已直接构造到最终的共享内存。 |
| Python-only prototype | 没有 portable binding 的普通 Python method 仍可在本地使用 pickle；portable descriptor export/codegen 会诊断并拒绝它们。 |

历史上的 7 月本地候选从 C-Two 实现提交 `bf6f5c950959bcd2723cf3c7bfe772c9ee91dc02` 使用 FastDB 实现提交 `7eb74734926bd8fe911229eee9744a6dd8172487`，当时的版本元数据为 FastDB 0.1.22/0.0.3 与 C-Two 0.1.0/0.5.1，未出版。那些确切字节与收据只作为历史证据。

## 产品能力

这些能力还没做，顺序记在待办里。不得用 C-Two 自有的载荷实现绕过上游仓库（例如 FastDB）的缺口。

| 能力 | 为何需要 | 完成标准 |
| --- | --- | --- |
| 官方不可变的 FastDB 与 C-Two 包分发 | FastDB 0.2.4 已发布，Python 与 c3 的发版走同一源码的协调流程；官方 C-Two Rust/TypeScript 分发仍未闭环。 | 经授权的 FastDB 与 C-Two Rust/Python/TypeScript/CLI 产物不可变且可获取；生产清单固定它们，并在不用本地 registry 与 sibling 路径的前提下重复 clean-environment package、codegen、runtime 与 interoperability 门禁。 |
| Contract compatibility | 精确的 release 匹配是安全底线；semver/range 规则必须先有稳定的正式 release 内容。 | Rust 拥有的规则拒绝歧义与 ABI 不兼容的匹配，并在各 SDK 投影同一行为。 |
| Call metadata 与 admission hooks | 上层需要与传输一致的 identity/policy 机制，但 policy 不属于 C-Two。 | Thread-local、IPC 与 relay call 携带有界 metadata；hook 可准入/拒绝；下游系统仍是 policy 的权威。 |
| Dry-run mechanism | Impact analysis 依赖同一个显式 metadata/admission 边界。 | Dry-run 明确评估什么、禁止哪些副作用，以及不支持的方法怎样失败。 |
| Async unary API | Async 应扩展同一个稳定的 unary contract，而不是形成第二套协议。 | 受支持的 SDK 在同一 route/error/payload/lifetime 语义上提供 async proxy/context manager。 |
| Telemetry、backpressure 与 adaptive memory lifecycle | Streaming 会放大取消与保留的风险。 | Rust 拥有 pool、dedicated segment、chunk、queue 与 cancellation 的有界遥测与策略；SDK 保持 thin facade。 |
| Streaming RPC | 服务端流已经交付（[ADR 0003](../decisions/0003-server-streams.md)）；客户端流、双向流，以及生成的 TypeScript 客户端经 relay 读取流，都还没有做。 | Stream identity、frame、ordering、cancellation、error、backpressure 与资源释放被规格化并端到端证明。 |
| 可发布的 TypeScript SDK/runtime | 生成的 Node 证明已有，但官方打包与 browser 边界尚未闭环。 | 一个受支持的 package 消费相同的 contract/release/payload identity，并证明声明的 Node/browser lifetime 与 transport 矩阵。 |
| Discovery 与 namespace governance | Relay mesh 传播 live route；广义搜索属于 admin/governance。 | 独立的 discovery surface 返回候选 metadata，普通调用仍按精确契约，绝不退化为只按名字准入。 |

## 性能与加固

这些轨道不得偷偷混入无关的功能，也不得用来夸大当前能力：

| 轨道 | 当前限制 | 完成标准 |
| --- | --- | --- |
| Direct final backing | 尚未跨 Rust/Python 实现在资源执行时直接构造到最终的 C-Two backing。 | 公开的 FastDB backing adapter 如实报告 direct/staged，已证明的路径没有构建后的重新打包，并在两个 SDK 通过 fallback/lifetime 测试。 |
| Portable-payload benchmark | 当前有正确性证明，但没有经审阅的显式 `Payload` 吞吐结论。 | 可复现的基准记录 workload、环境、分布、copy/direct/staged 事实与 hold 保留载荷所有者时的行为。 |
| Artifact publication hardening | 当前只支持 new-tree 发布，不是断电或敌对父目录下的持久性收据。 | 带版本的 prior-manifest/update 协议与各平台 durable/no-follow 发布通过故障与竞态测试。 |

[契约发布的延后能力](contract-release-deferred-capabilities.md)保留其中仍开着的各项的理由、负责方与完成标准。

## 参考

- [可移植契约与路由](../reference/contracts-and-routing.md#portable-contract-v2-and-artifact-composition)：现行的架构契约。
- [Rust SDK](../../sdk/rust/README.md)与[开发验证](../reference/development.md)。
- 7 月[本地候选实施计划](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate-plan.md)与[闭环报告](../acceptance/2026-07-24-rust-sdk-portable-payload-local-release-candidate.md)：那次历史候选的确切提交、哈希、收据与结论强度。
- [远期架构](endgame-architecture.md)：C-Two、Toodle 与领域之间的长期边界。
