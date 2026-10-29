# Runtime 关闭与会话换代：待修方案

状态：历史草案，已被[迟到连接与 Core 收尾完整方案](2026-10-28-runtime-retirement-core-plan.md)取代。下文保留原审查过程，不再作为实施依据；生产修复仍待用户确认。

## 已确认的原因

[受控复现](../reports/2026-10-27-runtime-concurrent-shutdown-followup.md)在基线和候选上固定了相同交错：A 已完成旧 Bridge 的 native shutdown 和旧 hook，尚未进入 Runtime 最终换代；B 在同一 NativeSession/Bridge 注册新路由成功；A 再换代。当前 Runtime.names 变空，探针保留的旧 Bridge 仍能通过 IPC 服务新路由，新 hook 为零；显式收尾旧 Bridge 后 hook 才执行一次。探针为观察/清理保留了引用，不能据此声称无外部引用时 Host 永不析构。

上一轮修复的是失败启动后错误选中空/旧关闭结果；本项是原生关闭完成与 SDK 会话发布之间的所有权缺口。单纯“start 在并发 shutdown 之前返回、随后被关闭”允许线性化，不能单凭它判错；本例缺失的是已成功注册的路由及 hook 的关闭归属。

源码中 `register/start` 在 `Runtime._lock` 内使用 NativeSession.ensure_host；旧 Persistent Host 终态且 registrations 已空时可再次启动。`Runtime.shutdown` 只在取快照和最终 `_swap_runtime_session` 时持锁，中间 Native 清 registrations/bridge，Python Bridge 消费 slots/hook。换代时只比较 session 对象相同，因此中间的新工作被当作旧会话一起丢弃。锁外等待本来是为了让在途 Python 方法和 hook 能重入；不能改成持锁等待原生排空。

## 推荐改法：一个 SDK 退役引用

复用原 `_lock`、native shutdown、Core journal、slots 消费及 handoff；仅在 Runtime 中增加一个私有的“正在退役的 session”引用，值为 None 或原 session。它表示 SDK 换代尚未收束，不表示 Host 已停止。它覆盖 native 终态之后的窗口，也覆盖没有 Host 的 client-only Runtime；不借用有限调用预算的 closed 位，不加计数器、计时器、工作线程或历史 session 列表。

这相对上一轮“不增加 Python closing 标记”的批准边界有变化，需要本方案确认。该引用不是第二份 Core 生命周期，原生阶段、预算、事务与释放顺序仍由 Core 决定。

一次关闭的顺序为：

1. 在实例锁外完成一次 native f64 提取和纯 `checked_timeout` 预检，不能持锁执行用户的数值转换。校验失败没有关闭副作用。建议统一公开 Runtime 的 timeout 为原生数值输入：有 server 时不再接受 `"5"` 这样的字符串，hostless 行为保持；这是需用户确认的小幅输入语义收窄。独立低层 Bridge 的现有转换先不变。
2. 再在原实例锁内捕获本次当前 session/bridge，登记其退役。这是新工作与关闭之间的排序点。准备阶段不预先绑定 native Host 或失败回执。
3. 锁外统一调用原 `NativeSession.shutdown(route_names=None, timeout_seconds=t)`，保留 Native 原有选择与执行时序：failed-start receipt 分支在 Host 锁内观察/清理；正常 Host 分支取出目标后锁外等待，完成后的 registrations/bridge 清理也在 Host 锁外。公共 Runtime 的退役引用负责阻止这段期间重启并插入新 Registration；低层并发调用不扩大保证。pending 保留原 session、slots 和退役引用；再次 shutdown 继续观察原有事务，不重放业务调用。
4. completed 后重新取得实例锁；仅当当前 session 仍是本次目标才准备替换。先完成原 handoff 中可失败的构造/采用，再发布新 session并唯一取走安全 hook slots；准备失败保留旧 session、slots、退役引用，可再次 shutdown 收束。旧并发观察者不能换走新 session 或清除新一轮退役。
5. 成功发布与 slots 消费结束后解除本次退役，释放实例锁和 slots 锁，再调用旧 hooks。hook 重入注册的新资源属于新 session，旧关闭结果不描述或清除它们。沿原 safe-close 判据和异常处理，不添加全局 hook 完成屏障。

从 `NativeServerBridge.shutdown` 提取安全 slots 消费与 hook 调用，让 Runtime 与独立 Bridge 共用；不复制关闭算法。现有代码以调用前 names 快照选 slot，不能原样搬走：修订为从 outcome 的 hook-safe 路由中取当前 slot，Runtime 在退役保护下唯一消费，锁外调用。独立 Bridge 同步复用这段投影，但不据此承诺其并发重启/同名再注册安全。现有 Core completed 与各错误、active_drained 仍分别解释，不升级为“所有并发观察者的 Python hook 都已返回”。

### 删除初稿的 Native 预准备对象

初稿的“已校验关闭调用”会预选 Host/failed-receipt/client-only 目标，再延后执行，破坏原有选择到清理的锁边界，因此删除。`route_names` 在有 Host 的 Native 关闭路径只作前置名单检查；真正关闭的路由集合来自 Host/Core journal。SDK 的 Python slots 只负责消费相应 hook，不能用可能过期的 Python 名单挡住整次关闭；低层 NativeSession 的显式名单参数与检查保留；Python Bridge 与 Runtime 都停止提交该快照名单，改传 None。

仅增约五行的私有纯超时预检，直接调用唯一 Rust `checked_timeout` 并返回已转换的 f64。现有 Native shutdown 入口继续校验，保护直接调用。这里**同一无状态函数会执行两次**，而非维护两份校验规则；这是在 SDK 提交退役前拒绝非法参数、同时保留原 Native 边界的明确取舍。没有绑定目标、缓存结果、未校验入口或新的命令类型；若要严格只执行一次而必须新增 opaque 参数类型/新执行 API，则本方案暂不引入。需要用户一并确认这一取舍。

native 执行或投影异常后不猜测关闭未开始，保留退役并允许再次 shutdown；纯参数校验发生在登记之前。超时仍是原生清理等待预算，不增加 Python 计时器，也不承诺覆盖任意 Python hook 的墙钟时间。

## 准入只覆盖会发布新运行时工作的路径

| 入口 | 规则 |
| --- | --- |
| register、start | 现有参数检查后，在原实例锁内检查退役；关闭期间明确拒绝，不能成功后丢失。 |
| remote connect（直连 IPC/HTTP/relay） | 在原会话快照锁内拒绝新准入。已捕获旧 session 的调用在最终发布点用同一个 ConnectAttempt 取锁，先保留 deadline 优先级，再验证 session 相同且未退役；失败沿既有 proxy/client 终止路径，不重试。 |
| thread-local connect、已返回 proxy、Held | 线程内连接不发布新的原生传输，可继续允许；其路由调度、已返回 proxy 的关闭、在途调用和 Held 存亡仍由原 owner/lease 决定，不给每次 RPC 加 gate。 |
| set_transport_policy | 已有 server 或 client_config_frozen 时保留原 warning/ignore；只在真正要修改覆盖值并调用 `_swap_runtime_session(preserve_server_identity=True)` 的分支检查退役，避免未确认关闭期间出现第二个发布者。该 setter 会换对象，并非简单“改旧 session 导致配置丢失”。 |
| 其他 setter | 保留原冻结/重置/handoff，不普遍新增拒绝。relay/调用预算/端点选择沿原 handoff；server/client IPC、startup/lifecycle/一次性 owner capability 仍按原关闭规则重置，不新增并发留存承诺。 |
| shutdown、unregister、close、release | 允许继续收束原目标，保留原生 pending/error/释放处理。并发 shutdown 只发布一次替换；hook 依原 slots 机制消费。 |
| 观察与独立 owner capability | 观察所捕获的 session，不驱动换代；旧事件源不转发。prepare_owner_takeover 按原上下文捕获与独立 capability 所有权处理，不能把 Runtime 退役当成撤销外部控制能力。 |

新准入拒绝采用现有 Native 在 draining 时的 `RuntimeError` 分类，文案明确“该 Runtime 的 shutdown 尚未收束”；不借用资源调用层的 `ResourceUnavailable`。仅新增并发退役分支的行为，非并发错误顺序保持；冻结/忽略的旧规则先于新 gate，不能把所有 setter 改成统一错误。pending 和 client-only pending 期间拒绝新的远程连接，也是需接受的语义变化。旧代理的后续调用仍按原生关闭结果失败。

## 范围、改动量及影响

生产替换范围为 `registry.py` 的上述发布入口/关闭收束、`server/native.py` 的 slots/hook 共用代码，以及 `runtime_session_ffi.rs` 的私有纯校验投影。不修改 Core Host/Persistent/OwnerBound 策略、Native 目标选择的锁顺序或失败启动回执。

静态估计生产净 +60–100 行、测试净 +140–240 行，文档约 +30–60 行；未实施，不能当作实测净量或性能收益。移除初稿 Native 预准备对象后，主要维护成本只剩一个 SDK 状态与固定交错测试。若实施需要通用命令框架、关闭任务队列或历史会话列表，则退回方案。

Python 公共签名保持；并发准入拒绝、hook 重入归属、Runtime 的 timeout 字符串拒绝和 SDK 不再提交 Python 诊断名单是可观察的语义调整。Rust SDK/Core 公开签名、C/FFI ABI 6、IPC 14、SHM 3、owner 控制及 wire 不变，不留旧协议兼容。低层 NativeSession/独立 Server 的并发 restart-close，以及私有 reset/解释器 atexit 期间创建新资源，不宣称随本项获得新的并发保证，继续单列。

## 实施后的验收门槛

本轮复用已保留的基线真实记录；没有重建产物或执行新的运行时实验。确认后先把确定性反例补到当前基线，再改生产：

1. 原 Event 窗口、空 start、无需 Event 的 hook 重入注册、client-only pending、真正会换代的 transport policy 分支、remote connect 的迟到发布；验证新资源不脱离所有者且 hook 恰当消费。
2. 在途 Python 方法/`__float__`/hook 重入不因锁而死锁；两个 shutdown 与后一轮关闭交错，不重复 hook、不清除新退役。thread-local 与其他 setter 的既有行为不被扩大拒绝。
3. 非法 timeout 不登记退役；pending 保留同一 session/事务；构造替换和采用退休观察失败后 journal/slots 不丢，重试只继续收束；上一轮五项 Core 回执、真实 Native 终态错误保真保持。
4. unregister 与 shutdown 交错，特别是 Python slot 尚未满足 safe-close、Native 登记已移除的路径。先证实 Core journal 能否给出后续结果；不能把“取消名单断言”写成该路径已全部修好。若涉及失败释放/登记消费责任的改变，另行提出对应修复，不偷偷并入。
5. 迟到 connect 必须检查物理连接收尾，不能只证明 proxy 未返回：原 `terminate()` 可能仅归还逻辑引用，且 Core pool 可重新建立；先在反例中观测真实关闭与原 observer 的统计归属。若现有 Core 释放事务不能覆盖，不以成功 Drop 推断已排空，也不在本方案里偷偷新增连接计数/关闭框架，应补齐最小调整后再实施该部分。
6. 原配置冻结/handoff、有限连接总预算及错误优先级、独立 Runtime、Held/预算/SHM 精确释放；完整 relay、Rust/Python/Node、三系统、GitHub Actions（含 Windows ARM64/3.14t）、端点零残留和独立审查。Windows 优先开发机，验证后清理产物；发版等待指令。

另有审查提出的静态线索：首次 register 回滚在 Runtime 锁内调用 Bridge.shutdown，若该路径已有 slot 且 hook 重入，可能自锁；目前未复现。本次“锁外 hook”承诺只覆盖公开 shutdown 换代，不把该线索写成已确认故障或已修复；若复现，复用同一提取函数另行给出回滚边界。

## 指定审查

通过新版 hey-my-buddy 0.55.2 skill，按用户指定直接委派 `claude / anthropic / claude-opus-5-5 / high`，只读工作树；未改变全局路由偏好。宏任务 `macro-d6108004-b1c3-4861-89b5-06b2d786b7c7`，微任务 `07bcd77b-b0db-48bb-8de0-ac0fbad194ed`。首轮结论为“有条件接受”，要求删除 Native 预准备对象并收窄准入；Host 已按当前源码校正 transport policy 的换对象行为和不存在的 `_client_connections` 字段引用。最终有效复核已取得完整二稿（SHA-256 `9071b1d5fa20e2d78d649078ff042b8f69bbe7fdf52e0a8ca6834e0d35c74a13`）。中间一次恢复会话未带入修订全文，不能算二稿复核；随后已明确提供全文。结论与 Host 核对见[审查记录](../reports/2026-10-28-runtime-shutdown-plan-review.md)。所有审查均为静态审查，不是实现或运行验证。


## 审查后尚未闭合的实施边界

Opus 指出的迟到 connect 问题有源码依据：旧 pool 的 `close_all` 完成后会重开 epoch；`Runtime.shutdown_transaction` 只克隆 pool，不移除它。先前捕获旧 session 的 connect 可以此时才 acquire 成功。Python `terminate()` 经 Native client.close 只丢掉 Core Client/ClientLease 的计数，不能证明物理关闭；若 exception traceback 或外部引用继续持有旧 session，不能依靠最后一次 Drop 完成清理。

Host 已补核 relay fast path：`connect_relay_aware → acquire_relay_ipc → acquire_ipc_client_with_deadline` 与直接 IPC 共用该 Runtime 的 client pool，所以不能仅处理显式 IPC 地址。HTTP 路径的已有 owner drop 也须单独核对，不能把 IPC 的结论外推为全部传输均已关闭。

审查建议增加私有 Native client-cache 关闭投影，复用 `observe_closes().close_all(剩余预算)` 和 `ipc_client_close_*` 结果字段。这比新造关闭框架小，但**仅加投影还不是完整方案**：必须同时说明预算已耗尽或关闭仍 pending 时，谁继续持有旧 pool、谁驱动后续收尾，以及旧 Host 已缓存 completed 时如何让结果保持真实。不得通过再给五秒、只打印错误、依靠异常释放时的 Drop，或向新 session 塞入另一套历史对象清单来掩盖。

因此，实施前的明确顺序是：先为原换代窗口和上述迟到 acquire 补隔离基线反例（保留旧 session/异常，避免靠析构让用例偶然通过）；据此把 client-cache pending 的所有权接回现有 Core 关闭事务，再评审这项 Native/Core 调整的实际范围。它会超出当前三个文件的纯 SDK 草案；未经补齐方案和用户确认，不直接实现。上述 +60–100 / +140–240 行只估算已收敛部分；审查建议的简单投影会把生产估计提高到 +75–125 行，但不能据此承诺完整修复的最终增量。

本文件保留了已核对的局部设计、审查阻塞和下一步验证边界，不声称当前整稿已可实施。生产、测试、协议、生命周期实现均未改动。
