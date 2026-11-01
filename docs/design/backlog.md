# 待办

尚未完成的工作与待用户决定的事项都列在这里，细节写在各自的文档里。做完的条目从这里删掉，并在验收或发行记录里注明。顺序由用户定；Host 的建议标明是建议。

## 进行中

- **已验收、待合入上游的三批。** 三个分支依次叠放，都只在本地：
  - 接口精简第一批（`socu/interface-slimming`）：[接口审查](2026-10-31-api-surface-review.md)第一、二节与决定 2、3。fork 全量 CI [41173746331](https://github.com/Dsssyc/c-two/actions/runs/41173746331)（`f0f9efaa`）在 Linux、macOS、Windows x64 与 ARM64 上通过。
  - 过度防御复审的真缺陷与死代码两批（`socu/over-defense-batches`）：见[执行计划](2026-10-31-defect-and-dead-code-batches.md)。fork 全量 CI [41192397960](https://github.com/Dsssyc/c-two/actions/runs/41192397960)（`e1630cf1`）在四个平台上通过。
  - 文档与 AGENTS.md 重组（`socu/docs-restructure`）：见[执行计划](2026-10-31-docs-and-agents-restructure.md)。AGENTS.md 从 64,905 字节减到 22,243 字节；`docs/` 按角色分成六个目录；新增术语表 `CONTEXT.md`，并统一了现行文档的用词。fork 全量 CI [41218012397](https://github.com/Dsssyc/c-two/actions/runs/41218012397)（`08b8c65c`）在四个平台上通过。
  - 之后：用户 2026-11-01 决定这一轮的工作做完再向上游开 PR，不开半截的。在此之前，fork 上的 `socu/docs-restructure-ci`（`08b8c65c`，含整个叠放）是这三批唯一的远端副本，保留。

## 接下来（Host 建议的顺序，待用户确认）

同一块代码里的事合成一批做，免得先做的被后面的批次推翻。

1. **协议批。** 核心是[单一属主释放](2026-10-31-single-owner-release-evaluation.md)：握手 15、SHM 格式 4、C ABI 7，Rust、Python、Node 同时改，按 AGENTS.md 用确切候选跑全平台门禁。
   - 一并做：[复审](2026-10-31-over-defense-review.md)第五节的 B1、B4、B7、F5、A9（删 Windows 进程 pin 时含 D7）；单一属主要删掉的 C-U1 结算层、C2 退休执行器与 R07 的保留重试；同在释放路径上的内存重复校验 A1、A4、A6、A11，wire 与服务端的 B3、B6、B9，以及 C7；R13 剩下的"写锁前取消时保留未发布的分配"；客户端请求 id 计数器是 `AtomicU32` 而线上格式是 u64，核实是否截断（`c2-ipc/src/client.rs`）；SHM 释放坐标无法认证的问题；心跳两项设置与 R35 的去留；E-host（低层 Host 关闭不退休整个 Runtime），因为它与本批的关闭路径是同一处改动。B4 删掉恒为 1 的 `route_revision` 之后，TypeScript 里它超过 2^53−1 就失真的问题随之消失。
   - 删除所有者子系统（[接口审查](2026-10-31-api-surface-review.md)第六节）：`OwnerBound` 生命周期、所有者监督与缺席宽限、恢复凭证与接管、托管子进程，以及 Python 与 Rust 的公开名字、文档与测试；握手 15 同时去掉所有者控制这种连接角色与 0x16 帧。普通 Host 也在用的关闭、生命周期观测、准入锁与代次和端点凭据保留，不留空的 API 壳。依据：用户 2026-11-01 的标准是"下游看不见、内部机制也不需要，就可以删"；独立评估与 Host 核对都满足（C ABI、CLI、relay、端点维护都不依赖它）。按文件粗算约 3.7 千行生产代码、5.9 千行测试与夹具。
   - 步骤：详细设计（消息格式、借出表、两个方向的发送时机、各条关闭路径的失败场景、端到端基准与验收计划，附流式 RPC 草图）→ 独立审查 → 用户批准，同时确认单一属主的决定 2、3 → 实施。
   - 状态：用户 2026-11-01 批准[详细设计](2026-11-01-protocol-batch-design.md)，包括 D7（按所列条件）与性能门槛；决定记为 [ADR 0001](../decisions/0001-single-owner-release.md) 与 [ADR 0002](../decisions/0002-remove-owner-subsystem.md)。实施在分支 `socu/protocol-batch` 上按设计第七节的顺序进行，P0（删除所有者子系统）已验收合入（代码 `bdb5242a`、文档 `b861f994`），Host 在并排 FastDB 0.2.4 的本机补跑了全部门禁；P2（配置与本地流）已验收合入（`db4d02d6`），本机补跑范围内测试 268 项通过；Linux 上"对端带未读数据关闭"读到 ECONNRESET 的一档与 Windows 原生检查，随整批候选的全平台 CI 补齐。D7 按批准条件不实施：分析表明改为临时持有会留下无人覆盖的 PID 复用窗口，保留现有 pin。P1（内存）已验收合入（`05c287eb`），本机不经沙箱复跑 c2-mem 144 项通过；验收中查出 dedicated 段按 4 KiB 写死取整，在 16 KiB 页的机器上会少记预算，已改为按真实页大小。P3（wire 编码与连接核心）已验收合入（`e1b3c62f`），本机复跑 c2-wire 128 项通过；`loan_id` 改为在借出方进程内唯一。P5（服务端接线）已验收合入（`d438a1c9`），本机复跑 c2-wire 与 c2-server 305 项通过，含 SHM 测试；验收中补了停用后仍可写控制帧的入口。P4（客户端接线）已验收合入（`643be217`），本机不经沙箱复跑 c2-ipc、c2-server 与 c2-wire 420 项通过，含 SHM 测试。验收中 Opus 独立审查查出调用送达之后的响应失败被归为派发前（上层可能重复执行）、客户端周期回收被误删、约 78 项仍然有效的行为失去测试覆盖，均已修正；本机复跑又查出 P5 的回归：删掉事件缓存之后，已提交、尚未开放准入的路由被投影成 `Closed:ExplicitUnregister`，relay 与 Runtime 的注册证明会失败（P5 验收时没有测试覆盖这条投影），修正合进 P4，路由准入改为每个调度器一个三态值。连接停用时还没写出的调用改报派发前。P7（relay）已验收合入（`516e3cec`），本机复跑 c2-http（relay）、c2-ipc、c2-server 与 c2-wire 839 项通过：mesh 7，route revision 与 digest 冗余 hash 删除；验收中又查出 P4 的回归：分片回复被重组预算拒绝时停用了整条连接，relay 也淘汰了共享客户端，修正为只让该调用失败、丢弃该回复余下的分片，连接与共享客户端保留；不带契约的注册（CLI `--upstream` 与手工 `/_register`）只接手已经开放的路由，带契约的注册照旧接纳已提交、尚未开放的路由；这不是行为变化：两条路径都先重建路由表，重建一直只保留已开放的路由，旧测试直接读连接时由握手填入的路由表才看到了未开放的路由，测试已改为断言产品的实际行为；c2-ipc 已有的写入暂停点开放给 test-support，供 relay 的部分写入回归测试使用。P6（Core）已验收合入（`eea414c3`），本机复跑 c2-core、c2-ipc 与 c2-server 508 项通过：hold 只留本地副本，本地副本的记账从 c2-mem 收进 c2-core（`LocalCopy*`，术语表新增"本地副本 / local copy"），低层 Host 关闭不再封 Runtime 的调用域与 outgoing，Host 线程等连接结清再销毁执行器，生命周期快照改为 `finished`。术语表的 hold、租约、释放三条与 AGENTS.md 的租约次序已随 P6 改写；`payload-lifetime.md`、`memory-policy.md` 与 Python、Rust 用法等 SDK 一侧的文档随 P8 改。P8 拆成三块并行，写入范围互不重叠：P8a（c2-mem-ffi 的 C ABI 7、Node 加载器、TS 传输，连同路由状态线上枚举的清理）、P8b（Python SDK，连同 c2-mem 的 lease 模块与 `seg_data_info` 的删除）、P8c（Rust SDK）；跨语言的互通测试在三块都合入后由 Host 统一跑。P8c 已验收合入（`c48b8481`），本机复跑 c2-ipc、c2-core 333 项与 Rust SDK 25 项通过；验收中查出 B7 把确认缩成一个字节后，直连 shutdown 把不存在的端点报告成"已发起"，Host 裁定改为 `Initiated` / `Absent` / `Unconfirmed` 三种结果，对应 P4 之前的三种组合（c2-ipc 定义，Core 原样导出），Python 的投影随 P8b 改为三个字符串。P8b 已验收合入（`799c65dc`），本机复跑 c2-core 221、c2-mem 138、native 18 项通过，Python（不含 TS 与跨语言矩阵）只余三项等 P8a；验收中查出 P6 的回归：Core 的 `shutdown_without_host` 被改成永久退休，Python 无 Host 的低层关闭因此在注册失败回滚时退休了整个 Runtime，修正为无 Host 的低层关闭立即完成、不退休，并删除这个已无产品调用者的函数。P8a 的独立审查（Opus）查出 Node 加载器参数顺序写反（Node 的 SHM 与释放接收失效）、复制成功后写释放失败被报成调用失败、接纳边界违规不一律停用等，均已修正；Python 套件另查出 P4 删掉了客户端关闭时的 DISCONNECT、P5 让服务端一进入关闭就停用全部连接而丢掉活动调用的响应，前者已修，后者在 P8a 修正中。合入后要核对：P8b 删掉的 `test_generated_typescript_reader_opens_lazy_empty_handshake` 守的"握手之后才出现的响应段由 TS 按需打开"仍然有效，确认互通矩阵覆盖了它，否则补回改写版；`tools/local_rc` 里两份收据校验器仍要求 revision，随 P9 改。现行的生命周期契约（AGENTS.md、`payload-lifetime.md`、`memory-policy.md`、Python 与 Rust 用法）和术语表里 hold、释放两条，由 Host 在行为改变的那一块合入时同步改。
   - P8 顺带：`sdk/python/tests/.../test_frozen_peer_shutdown.py:113-115` 仍期望 `disconnect_ack_timeout`，P4 之后不再出现；Python 的协议版本测试要改为 15。
   - P5 / P8 合入时：`ConnectionEndReason` 的 `heartbeat_timeout` 已在 P2 删除，`docs/reference/local-endpoint-lifecycle.md` 里关闭原因的列表随心跳整项删除一起改。
   - P8 顺带：B6 删掉事件缓存之后，路由状态与原因的线上枚举里有些变体已经没有生产者（例如状态 `Pending`、`Draining`、`Removed`，原因 `RegisterPrepared`、`ContractMismatch`），服务端也总是填 `state_reason`；核对后连同 TS 传输里对应的字符串一起删除，`state_reason` 不再可选。这会改到 c2-wire 与 c2-ipc，开工时给 P8 加上这两处范围。
   - relay 区顺带：`relay/conn_pool.rs` 为替代 P4 删掉的 `force_connected`，在生产代码里加了只在测试构建生效的连通状态覆盖（`test_assume_connected` 等），之后在 relay 的结构调整里看能否改用真实连接，移出生产路径。
   - P8 顺带：P6 把 Host 生命周期快照改为一个布尔 `finished`，删掉了 `HostLifecyclePhase` 与 `client_held_leases`；Rust SDK 的 re-export 与 Python 投影（`phase`、`terminal`、`client_held_leases`）随 P8 改成同一个 `finished`。P6 把本地副本的记账改名为 `LocalCopy*`，Python native 的 `lease_ffi` 等投影随 P8 改名，c2-mem 的 lease 模块随之删除。
2. **流式 RPC**（[路线图](roadmap.zh-CN.md)第 7 项，用户确认是必备能力）：排在协议批之后，设计要回答单一属主评估"与流式 RPC 的关系"一节的八个问题，并定下同一连接内调用的顺序契约。现状（2026-11-01 核实，写在[运行时生命周期](../reference/runtime-lifecycle.md)）：非分片的 IPC 调用在接收循环里按帧序预约路由，`exclusive` 与 `read_parallel` 下后到的调用不会先于先到的写执行；`parallel` 与不同路由之间没有顺序；分片请求的首片在另一个任务里才预约，后到的非分片调用可能先执行。旧 Python 服务端的连接级写屏障连跨路由也保证顺序，Rust 里没有。一并设计：分片的结构调整（B2：先缩小重组池锁，再把首片准入搬回接收循环、删排序闸门，分片请求也就按帧序预约了；以及 C03、C04、R14 的剩余）；relay 的 HTTP 响应现在整体物化，改为流式发送；HTTP 载荷的内存预算与消费生命周期（[提案](2026-10-06-http-memory-budget.md)，未实施）。
3. **按区域的结构调整与重复校验。** 不碰协议，可以与协议批的设计并行；各区先出方案：
   - **Core 与 Python：** E1、E2、E4、E13；C5、C6；G5、G9；R06 回调重入导致自锁的风险。顺带：`c2-core/src/control.rs` 里 `direct_ipc_endpoint`、`ping_direct_ipc`、`shutdown_direct_ipc` 每次克隆上下文，可以直接借用（随协议批 P6 做）；`register` 的签名写 `Literal['materialized', 'borrowed']`，服务端桥里另有私有别名 `_InputLifetime`，统一成一处。
   - **本地端点：** tmpfiles（D-tmp）先在 Linux 上复现 systemd-tmpfiles 清掉 gate 与 marker 的情形，再实施目录 flock（本机与 Windows 测试机都不是 Linux）；之后 D1、D2、D5、D6、D10 的复查只留在所有权、发布与 unlink 边界；R17 剩下的 gate/context 与目录 pin 的错误分类。顺带：c2-local 的 Drop 在门锁短暂争用后由后台线程完成退役，`managed_drop_after_gate_contention_settles_ownership` 两种终态都接受，要有一个能观察后台重试的点才能确定地守住。
   - **relay：** F2（删 watch 闸门，轮询降到秒级）、F3、F6；F4、F8、F10；relay-aware 调用没有每次调用的路由回执，`observed_route` 仍是连接时的值；F7 剩下的 `/_admin/unregister` 与注销 202/204/404 的区分，等用户答复。顺带：`LocalEndpointNamespaceRequired` 的消息写成 "the resource namespace"，应为资源一方的本地端点命名空间。 `c3 relay --upstream` 只在 relay 启动时登记一次：路由那时还没开放就报 "does not export route"，`c3 relay` 直接退出；登记成功之后，上游重启（身份改变）或路由被注销时，relay 会把它撤下，也不会再自己重新接手，只能重启 relay 或手工 `POST /_register` 补上。用户 2026-11-01 要求记入：改为启动后持续重试、路由开放后接手、上游重启后重新接手；待定的是重试的节奏与上限、启动时登记失败是否仍让 `c3 relay` 退出、以及与资源进程经 relay anchor 自行登记同一路由时谁优先。
   - **契约、代码生成与 CLI：** H1、H4、H6、H8（用校验过的构造器加私有字段，取代 C01）、H9；R29 等剩下的 codegen 同次缓存与 typed projection。
4. **配置收敛**（[接口审查](2026-10-31-api-surface-review.md)第三节）：逐项表定下之后进行，心跳两项随协议批定。与 B8、H3、H7、H12、C4 交叉，R09、R10 的 EnvCatalog 多次加载一并处理。`chunk_threshold_ratio`（`C2_IPC_CHUNK_THRESHOLD_RATIO`）已证实没有作用：只在 c2-config 里解析与校验，c2-ipc 只在比较两份客户端池配置时用到它，分片判断不读它；连同环境变量、Python 字段与测试一起删除。
5. **错误类型**（接口审查第四节）：协议批之后。一并：删除从没产生过的错误码 706 `WriteConflict`，连同 Python 的错误类、映射与相关测试（用户 2026-10-31 同意；704 是还在使用的 `RouteStale`，不动）；`sdk/python/src/c_two/error.py` 里重复注册错误的 docstring 写成 resource name，改为 route name；TypeScript 公共错误暴露 `pre_dispatch` 与 `dispatch_uncertain` 的区分；R32、R33 等剩下的结构化错误内部序列化、Python 错误常量与孤立的 `wrap_error`。
6. **TypeScript 与 Node：** R16 剩下的完整 Node→Core 投影迁移；解码不透明的响应 allocator。
7. **发版工具，方案 B**（[发版敏捷性审查](2026-10-30-release-agility-review.md)第三、四节）：与产品代码无关，可以随时插入。B1 候选重整、B2 删除约 8 千行（`tools/local_rc` 与 `local-endpoint-validation.yml` 仍在）、B3 FastDB 版本只写一处、B5 精简各条线的 gate、B6 修 `PROBE_BUDGET` 的偶发失败。

各批一并删除只钉源码文本或已删名字的测试（复审第六节）；复审附录里各区未裁决的待议项，实施到对应区域时再定。[瘦身账本](2026-10-13-local-slimming-batches.md)暂缓表里的其余各项，包括两份控制会话重构，仍按账本写明的暂缓条件。

## 要核实的事

- TS 传输不处理 SIGNAL 帧（`readIpcFrame` 不过滤，回复按 request id 严格匹配），而服务端在连接空闲 15 秒后发 PING：空闲超过 15 秒的 Node 客户端，下一次调用会读到 PING 而失败，空闲超过 30 秒则连接已被服务端关闭。2026-11-01 设计审查时从代码上看出，还没有运行复现。复现后由用户定：先出补丁，还是随协议批的心跳去留一起处理。
- [内存策略的验收记录](../acceptance/2026-09-26-memory-policy.md)里，探针退出后客户端的 backing 仍能打开；那次探针经过透明 socket 转发器，还没有做不经转发器的对照。协议批会改 backing 的所有权，可以在设计时一并核实。

## 不排期

有了使用者或测得需要时，再登记成具体条目：

- [契约发布的延后能力](contract-release-deferred-capabilities.md)里仍开着的几项：Python portable 参数形状的扩展、原始 `unsafe_buffer` 别名的访问边界、产物里的非普通文件、C++ SDK，以及 descriptor 存储、签名与信任这类下游 catalog 的事。
- 内存：buddy 中间空洞的复用；逐处复制字节的归因测量。

## 将来

- **多副本一致性**（relay mesh 原先设想的第二、三阶段）：2026-07 的草稿设想过同一资源在多个节点上的副本：路由带版本号，写成功后经 gossip 通知其他副本过期，写之前广播意图做跨节点写锁，读到过期副本时按策略处理，并提供内置的刷新方式与一致性级别。这些从没实现，现在不同节点上的同名路由彼此不知道对方。用户 2026-10-31 认为那套做法不一定是最佳策略，留作蓝图：协议批与流式 RPC 落地之后，或者有下游需要同一资源的多个副本时，再从头讨论设计。原文见上游的[固定版本](https://github.com/world-in-progress/c-two/blob/606b8f13290c15ebff3a04a02e2ac5c6463afa7c/docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md)第二、三阶段。

## 待用户决定

- 配置项的逐项表。
- relay 轮询的间隔（复审建议秒级，不做成配置）。
- `/_admin/unregister` 有没有人手动使用（F7）。
- `crm.client = <对象>` 与 `HeldResult(release_cb=…)` 算不算扩展点（G2）。
- `c_two.config.settings` 的直接写入是否受支持（G8）。
- R21：Resource 模板生成器输出的仍是装饰并继承 CRM 的旧模型，c2-codegen 没有等价的 Resource 骨架；修好这个工具，还是退役这项功能（约 613 行）。
- 发版方案 B 的决定 3、7。
- 单一属主的决定 2、3：hold 与连接绑定；属主一方结束连接时只停止服务。随详细设计确认。

## 顺带项

下一批碰到测试运行器时顺带做掉：

- 本地直接跑 pytest 时，TypeScript 测试要先构建 c2-mem-ffi 的 Node 绑定（`core/foundation/c2-mem-ffi/bindings/typescript` 下 `npm ci && npm run build`）。没构建时，有 2 项报错、约 30 项被跳过，看上去像回归。可以让夹具缺这个绑定时直接报出来，或者在开发文档里写明这一步。
- `tests/repo/test_python_test_runner.py::test_parent_exit_with_live_descendant_is_not_success` 在 2026-11-01 P0 验收的本机全量里失败过一次：期望"进程组仍存活"，实际得到 `[Errno 1] Operation not permitted`，单独重跑通过。进程组退出与发信号之间可能有竞态，碰到运行器时核实。
- 直接用 pytest 跑 Python 套件时，`test_typescript_persistent_ipc.py` 依赖别的测试先构建好 fastdb4ts；运行顺序不同就会报"尚未构建"。CI 的运行器会预先构建，所以只在本地出现。

## 下游跟进（由用户决定）

- **hey-my-buddy：** 导入改到新的子模块，`ConcurrencyMode` 改为字符串。配置收敛之后，它设的 9 个 IPC 覆盖键大多会消失。
- **toodle 与一个水文模拟后端：** 升级时核对导入；后者用到了 `ConcurrencyMode`，要改为字符串。
