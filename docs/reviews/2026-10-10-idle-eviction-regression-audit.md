# 历史闲置驱逐故障与精简回归审查

日期：2026-10-10。当前源码：`4cc5b4d229d21f00fc25619e8490169b2246edd1`。历史依据：[排查闲置续连报错](codex://threads/019e88aa-45f2-74f3-9683-b364bf090f87)。本轮核对历史根因、当前调用链和现有测试，并运行独立反例；生产代码未修改。

当前结构已处理原故障链的多个环节，但还不能判定全部覆盖。Rust 的精确 token acquire 存在旧缓存成功分支；TypeScript 持久直连仍依赖握手快照，默认重连还可接受同名替换。这些问题在精简之前就存在，应先建立回归测试并修复，再合并相关路由和连接实现。另有共享连接清理风险与诊断信息丢失，证据强度分别列明。

本报告补充[精简候选回归复核](2026-10-09-slimming-regression-review.md)，并收紧其中关于 endpoint connection、route binding 和跨 SDK 行为的结论。上轮 397 项 HTTP/Relay 测试及 7 项 mesh 集成通过，不能代替下文缺失场景的证明。

## 历史故障实际发生在哪里

历史会话中的业务顺序是：客户端先连接一个 manager；同一个 IPC server 随后动态注册 builder；客户端复用之前的连接去取得 builder。旧客户端只知道首次握手的路由快照，误报缺少 builder，再静默转到同一个本地 relay 的 HTTP 数据面。业务两次使用 builder 之间有长时间间隔，于是出现 relay upstream 的闲置驱逐和续连错误。曾经把一般 I/O 错误当作路由失效的处理，又可能错误撤回仍然存在的路由。

因此需要分别验证动态发布、缓存中的旧路由、数据连接重建、路由所有权与错误分类。单独延长 idle timeout、关闭 manager proxy，或改成每条 route 独占连接，均不能完整解决原问题。tombstone 的正常 GC 日志也不能单独证明仍活着的业务路由被删除。

历史会话后面的审查已指出：只在缓存缺失时 lookup，能够看到新增路由，却仍可能接受已关闭、删除或替换的缓存记录。最后一次整体审查也未把当时实现判为完整解决。旧的 watch-first 方案随后被当前[route token 与 endpoint connection 设计](../vision/route-token-endpoint-connection-architecture.md)取代；当前约束是 acquire 查询服务端权威状态、绑定不变、call 再校验 token。无需为了复现旧方案的形状而恢复直连后台 watcher。

历史取证定位保存在[证据记录](../reports/evidence/2026-10-10-idle-eviction-regression-audit.json)中，仅记录会话与相关 turn ID，不复制下游私有业务日志。

## 当前覆盖与缺口

### 1. Rust 普通 acquire 已修正，精确 token acquire 仍可接受旧记录

[`IpcClient::acquire_route()`](../../core/transport/c2-ipc/src/client.rs)（4230 起）每次先执行 `lookup_route_contract_for_acquire()`。后注册路由不再只由握手判断；原来的正缓存问题也不会通过这个入口直接成功。

同文件 `acquire_route_token()`（4348 起）却先执行 `bind_cached_route_token()`，命中后在 4356–4357 直接返回，甚至早于 dirty 检查。该入口服务于 [Core relay-local acquire](../../core/runtime/c2-core/src/client.rs)（577）和 [Relay upstream acquire](../../core/transport/c2-http/src/relay/state.rs)（529）。缓存记录与请求 token 相等，只能说明双方持有同一个旧观察，不能证明当前服务端仍接受该路由。

本轮用当前源码编译并运行真实本地 IPC 反例：注册并 acquire `grid`，服务端完成 unregister，然后在同一连接用旧 UID/revision 再 acquire。结果为：

- 精确 token acquire 成功返回旧绑定；普通 acquire 返回 route removed。
- 用旧绑定实际调用，服务端返回 `ResourceRemoved`（708）；业务回调次数为 0。
- 客户端、服务端正常关闭，探针 socket 已移除。

这确认了 acquire 语义缺口，同时证明本场景的服务端 call-time gate 仍有效。不能将它描述为已经调用了错误资源；也不能因为调用阶段最终拒绝，就把 acquire/probe 的成功视为正确。修复方向是让精确 token 获取也查询当前 authority，并在取得当前记录后比较预期 token。当前测试未涵盖此正缓存反例。

### 2. TypeScript 持久直连保留了两类历史问题

[`createIpcEncodedTransport()`](../../core/foundation/c2-codegen/assets/typescript_transport.ts) 缓存 `connectionPromise`（424–438），`prepareRoute()` 只从 `open.handshake.routes` 选择路由（440–450），没有 live lookup。模板由 C-Two 生成，但这部分协议和连接状态在 TS 自己的实现中，Rust 普通 acquire 的修正不能自动覆盖它。

本轮直接导入当前 TS 源文件，注入字节连接，使用已核验的 C-Two 0.7.3 原生 c2-wire 编码真实握手帧。旧 transport 先 prepare manager；可用握手夹具随后增加 builder；同一个 transport prepare builder 报握手中没有该路由，期间只有一次连接和一次握手写入。新建 transport 使用新增后的握手，prepare builder 成功。这是确定性的 transport 层反例，未使用真实 OS 端点，也没有执行业务方法。

另一个对照验证了重连身份：第一次 prepare 后注入一次调用写入失败，使现有实现清空连接缓存；下一次 prepare 的握手分别返回同名新 UID、同名新 server instance。默认选项均接受；显式传入首次的 `expectedRouteToken` / `expectedServerIdentity` 时均拒绝。相关选项在 212–213 可省略，生成的 ContractClient 也只保存 transport 与 route name，见 [`targets.rs`](../../core/foundation/c2-codegen/src/targets.rs)（859–863）。这是重新取得绑定时的身份接受问题；探针没有证明业务已执行，也没有证明失败调用被自动重放。

必须区分具体路径：TS relay-aware local IPC 每次调用新建临时 transport，并传入 resolve 的 server/instance/token（778–789），prepare 后调用并关闭；本报告没有把持久直连反例推广为所有 TS 路径。现有 HTTP 路径也区分 pre-dispatch 与 uncertain，不能通过“自动重试一次”修正直连缓存时顺带放宽重放条件。

精简 Node transport 或将它接入共享 Core 之前，要保留自定义连接注入、不可变 route binding、串行链和取消/关闭语义。应以实际生成客户端的持续调用补集成测试，不能仅用第一次新连接成功的矩阵代替。

### 3. 单路由语义失败会进入共享连接关闭路径，仍需并发验证

[`RelayState::acquire_upstream_for_route()`](../../core/transport/c2-http/src/relay/state.rs) 在 control watch 不可用后的 live verify 返回路由语义错误时（501–525），以及精确 token acquire 失败时（529–550），都会驱逐当前 client，并调用 `close_failed_acquire_client()`。该 helper（575–581）调用 `close_shared_bounded()`，关闭的是同 endpoint 共享的 `IpcClient`。

[`conn_pool::evict_client()`](../../core/transport/c2-http/src/relay/conn_pool.rs)（689–702）校验 slot 状态和具体 client 的 Arc 身份，没有 `active_requests` 条件。`should_evict()`（765–774）确实保护在途请求，但该条件属于闲置扫描路径，不能覆盖上述显式驱逐。native close 则会关闭流、终止接收并结算 pending（`c2-ipc/src/client.rs:4505–4605`）。

需要验证的复合场景是：A、B 两条路由共用健康连接，A 的请求正在执行，B 的 acquire 因 route closed/removed/stale 失败，B 的清理是否导致 A 失去结果。当前源码确认会走整连接关闭；本轮未取得完整并发运行证据，因此“在途 A 的实际失败结果”仍记为待证实。独立探针任务被子代理服务的内容检查拦截，未创建或运行测试，没有把此项计为已复现。

修复精确 token acquire 时尤其要同步检查这里：增加权威查询会更早产生真实语义错误，如果原样保留整连接关闭，可能扩大其影响。健康连接上的单路由语义失败应与流损坏/连接失效分别处理；不能一律删除底层驱逐或关闭机制。

### 4. FallbackDenied 丢失首个 IPC 失败原因

[Core relay-local 分支](../../core/runtime/c2-core/src/client.rs)（650）用 `Err(_)` 丢弃非终止类 acquire 错误，再去排除失败候选、寻找其他路径。[`fallback_denied_body()`](../../core/transport/c2-http/src/client/relay_aware.rs)（757–787）保留候选地址、server/instance 与 route token，却未包含原 IPC 错误及分类。

这是源码确认的诊断缺口：终止类错误仍直接返回，禁止经同失败端点转 HTTP 的行为也仍在；缺的是其他候选耗尽时原故障原因的保留。历史排查正是被 fallback 隐藏根因拖长，当前设计也要求返回 direct IPC failure details。精简错误投影时应保留 typed cause，不能仅剩地址和 `FallbackDenied`。

## 仍有效的结构与现有测试边界

| 历史约束 | 当前依据 | 证明范围与剩余边界 |
| --- | --- | --- |
| 后注册路由可在已有连接上取得 | 普通 acquire 发 live lookup；`pooled_direct_client_observes_route_registered_after_handshake` | Rust 普通入口已有真实 server 覆盖；精确 token 和 TS 正/负缓存不能由此推断。 |
| 闲置连接与路由所有权分离 | `UpstreamEndpointKey` 包含 address、server ID、instance ID；owner-only slot 与惰性数据 client 分离 | 保留共享 endpoint pool。registration attestation 不能因某个 disposable client 关闭而丢失。 |
| idle 不驱逐在途连接，并发续连合并 | `idle_entries_do_not_evict_active_connected_client`、`active_disconnected_client_is_not_evicted_until_request_finishes`、`concurrent_acquire_after_eviction_shares_one_reconnect` | 证明 pool 的相应分支；不覆盖所有显式错误清理路径，也不等于真实 timer/OS 断连的完整序列。 |
| 后注册、闲置后仍能调用 | `relay_late_route_survives_existing_endpoint_and_idle_eviction`；`concurrent_calls_after_idle_eviction_do_not_report_unreachable` | 测试手动触发 eviction；没有完整重现长生命周期 SDK 客户端、计时器驱逐与 ConnectionReset 的组合。 |
| 已取得绑定不能改指同名新对象 | immutable `RouteBinding`、服务端 token gate；Python `test_ipc_proxy_keeps_acquired_route_token_after_reregister` | Core 生产连接还固定 client Arc 并验证 server instance；不要误称 RouteBinding 自己存储全部 endpoint 身份。TS 默认重连存在上面的差异。 |
| I/O、watch 断开与容量不足不等于撤路由 | `semantic_withdrawal_reason_separates_authority_from_transport`、`watch_transport_failure_does_not_withdraw_route`、`relay_reply_capacity_preserves_shared_connection_and_inflight_call` | `RouteStale` 本身也不是撤路由依据。identity mismatch 的影响范围是相应 endpoint 实例；单 route 错误不能扩散到其他资源。 |
| 失败本机 IPC 不绕回相同端点；不重放不确定调用 | Core 保留候选 UID/revision；排除地址/server/instance；TS HTTP 区分明确 pre-dispatch 与 uncertain | 行为机制仍在；FallbackDenied 的原始 cause 丢失。仍需与后注册、idle、重连组合运行。 |
| watch 不决定直连正确性 | 当前设计明确 demote watch；relay upstream control 仍有观察、恢复和身份核对职责 | `production_direct_ipc_does_not_start_route_watch_task` 是源码断言。部分 watch-unavailable 用例注入 client 状态，不能称为真实断线恢复测试。 |
| 跨语言矩阵保留原传输路径 | TS real-call fixtures、18/12 行 portable 矩阵 | 当前 TS 实例通常在路由注册后启动、完成一次调用；`idle` 相关 SHM 测试覆盖 backing 回收，不能替代 relay 连接闲置回归。 |

去中心化 relay 的 owner generation、lease epoch、slot 身份、route UID/revision、tombstone、权威缺失修复及不同 catalog revision 分别保护不同状态变化。此次历史复核没有给出删除这些机制的依据。更少的表、字段或函数只有在上述状态仍被正确区分时才算精简。

## 相关精简前的回归门禁

先把当前失败场景纳入测试并修复，再做行为等价的结构收敛。每个测试记录实际 path、server/instance、route UID/revision、业务回调次数、重连次数与清理结果；既验证成功，也验证不应成功的 acquire 和调用。

1. 长生命周期客户端先连接 manager，同 endpoint 后注册 builder，保持连接复用取得 builder；显式 IPC 与 relay-local 都保持原路径，无同端点 HTTP fallback。
2. 路由 ready → closed、removed、同名重新注册之后，普通与精确 token acquire 均返回当前语义；旧代理不接受新实例，旧调用不进入业务。保留服务端 call-time 检查以覆盖 acquire 之后的竞态。
3. 使用短 idle timeout 加状态等待验证实际后台驱逐，再由多个调用者并发重连；owner/route 保留、同端点只建立所需连接。在另一个用例中注入真实流断开，区分安全获取阶段恢复与请求已发送后的不确定结果。
4. A、B 共用连接：A 在途时 B 关闭、移除或 token 过期；验证路由错误局部化、A 的结果与 carrier 生命周期不被提前终止。同时保留真正损坏连接必须关闭的反例。
5. 同名 server 重启、control watch 中断恢复、旧 candidate 与新 owner 并发：旧证据不能撤回或关闭新 owner，不能将新 server/route 绑定给旧代理。缺失或压缩 watch 历史时仍依据 authority 恢复。
6. 接口层验收包含 Python、用户 Rust SDK、实际生成的 Node 客户端；覆盖持久直连及 relay-aware local/HTTP。fallback 被拒时保留首个 IPC typed cause；已经 dispatch 或状态不确定的调用不自动重放。

纯编码或无效参数清理可按各自影响面独立推进。涉及 endpoint pool、路由获取、fallback、watch 或 Node transport 的精简，必须先满足本节门禁，并保留现有 HTTP/Relay、mesh 与跨语言矩阵。核心逻辑共用不等于 OS 行为相同：Linux/macOS UDS 和 Windows 2022/2025 Named Pipe 的空闲、关闭、重启与清理分别需要实际证据。

本轮新增验证只有 macOS 上的真实 Rust IPC 反例，以及 Node v25.8.1 上注入字节连接的两个反例。没有执行 Windows/Linux 或完整下游长时间任务，没有重跑未受修改影响的旧全套测试；没有实现修复、创建 PR 或发布版本。探针退出码 0 表示成功复现所断言的差异，不能作为缺陷已修复的结果。命令、原始结果、源码及日志哈希见[证据记录](../reports/evidence/2026-10-10-idle-eviction-regression-audit.json)。
