# Runtime 退役：迟到连接与 Core 收尾方案

状态：基线反例已完成；按 Opus 5.5 high 两轮复审的 B1–B5、R1–R3 收束，已完成复审，最后文字条件由 Host 落实，用户已确认并完成实施；最终 `f747a7c9` 的三端 Actions 10/10、147/147 适用门禁、零残留、性能和独立审查通过，见[验收记录](2026-10-28-runtime-retirement-acceptance.md)。本文接替上一稿的迟到连接阻塞；完整换代与 hook 顺序一并列明，不批准后再补所有权。

## 实证与不变量

基线为 `9e8da3aa`，生产源码为 `e0b5af2d`。macOS / CPython 3.12 的四项受控反例均命中：hosted、hostless 的旧 session 在公开 shutdown 已返回 completed/drained 后仍可建立 IPC；预算尚有剩余时真实 RPC 成功；在最终 Python 发布处耗尽原 ConnectAttempt 后，抛出原 CallDeadlineExceeded，但物理连接仍活着。逻辑 proxy close 不会断开该连接。再关闭旧 session，hostless 会重做缓存 drain，hosted 只重放旧 Host journal。测试一直保留旧 session 和错误 traceback，最后由真实 peer 的关闭屏障收尾，四份 root 均无 socket/lease。见[证据](evidence/2026-10-28-runtime-late-connect-baseline.json)与 `sdk/python/tests/unit/test_runtime_shutdown_publication.py`。

修复必须同时满足：关闭准入后不再增加这个旧 Runtime 的可发布连接；已经进入原生连接 I/O 的工作始终有真实 owner；调用方预算只限制其等待，不能取消或遗失已接管的清理；完成事实来自 Core 事务，不能把旧 Host 的 completed 当成整个 Runtime 的 completed。

## 推荐：扩展现有关闭事务的范围，取消 Python 退役状态

前稿只在 Python 加退役引用并在返回前 terminate，无法满足上述不变量，撤回。推荐复用 Core 的 `ShutdownTransaction`、原生 `ClientPool` epoch/retired records、`FreshClientGuard` 和 `ClientCloseObservation`，增加一次明确的 Runtime 退役范围。没有独立清理队列、历史 session 列表、每连接观察线程、重试业务请求或 Python 清理状态机。

Host 关闭与 Runtime 退役的对象不同：前者结束一个具体监听者，现有 Rust/低层 Native 的 Persistent Host 重启能力保留；后者结束即将被 Python SDK 替换的整个 Core Runtime，其连接池不再重新开放。这个区别必须由同一 Core 关闭机制表达，不能用循环调用旧 Host.shutdown 来冒充 Runtime 收尾。

Core 在原 RuntimeState 锁中保存本实例唯一的 Runtime 退役事务引用，None 表示未退役，Some 同时是拒绝新工作及重复观察的依据；不再在 Python 保存 `_shutdown_session`。Native 在原 Host 锁内选择唯一的当前目标（最新 failed-start receipt 优先，否则当前 Host，否则 client-only），并立即启动 Core 退役，不产生“预选目标、以后再执行”的对象。调用锁序仍为 SDK Runtime 锁 → Native Host 锁 → Core 短状态锁；等待和 I/O 均在这些锁外。

### 连接池的封闭与正在建连的 owner

1. 在 Core 发布退役事务的同一临界区，禁止以后创建或取得新的 client pool；已有池只做无 I/O 的永久 seal。可把现有 pool epoch 改为 `Option<u64>`：Some 继续表示可重开的 drain epoch，None 表示这个池永久退役。正常 `close_all` 的 epoch 递增保留 None，不将它重开。此状态替代/扩展原字段，不再叠加另一个布尔标记或新增用户选项。
2. 首次 acquire 和每次瞬时重试都在同一 state → close_txn 锁序内登记 Preparing，并检查原 epoch、closing 与 seal。提取一份登记逻辑复用；不能让第一次 retire 与下一次重试之间的无 record 空窗绕过 seal。重试被拒后没有任何新 I/O，原重试次数与总预算不变。在永久 seal 前已取得池引用、但尚未调用 acquire 的连接，也会在原 pool 锁内被拒；已经处于 I/O 的 acquire 仍由原 FreshClientGuard/Preparing record 持有。最终 pool publication 发现 seal 后走原精确 retire，不会把客户端放回缓存。Core 的普通直连与 `connect_relay_aware → acquire_relay_ipc` 共用这个池，必须同样覆盖。
3. 当前可重开 drain 有意不等待 Preparing，这是其原语义。Runtime 退役观察必须把 Preparing 也纳入“尚未结束”，复用 record 的事件通知等它转成 Closing/Confirmed/UnconfirmedIdle；不得只看 entries 为空。扩展既有 record 等待条件，不增加周期扫描或连接计数器。
4. `close_all`、FreshClientGuard 和现有有限 settlement 对同一 retired record 仍用原 claim 规则，不能同时执行两次 close。预算耗尽留下的 UnconfirmedIdle 由原 record 保留实际 Arc<SyncClient>，Core 退役 worker 继续对真实未确认的记录执行原 close barrier；确认后才清 owner。不能把最后一次 Drop 当成完成证据。
5. HTTP-only 不为 seal 创建 IPC 池或解析本地 namespace。原 HTTP 请求 owner/取消方式保留；已发出的发现请求仍受原预算约束，Core 最终发布与 SDK 最终发布均拒绝已退役实例的结果。本文不扩大为“shutdown 证明全部 HTTP keepalive socket 已关闭”，现有 IPC drain 字段也不作该保证。

### Core 事务如何接住收尾

复用原事务启动器、执行体与 receipt 观察，不能并行启动两个 driver drain 同一个池。按下面三个实际状态处理：

- 当前目标还没有事务：一次性准备所需 executor 与 driver，随后在原 Core 状态锁内把同一个事务 Arc 放入具体 Server 的 transaction map 和 Runtime 退役 slot，再 seal 池。唯一 driver 直接执行原 shutdown_transaction，不再调用会另行 spawn 的 Host.shutdown。Host、Drop、failed-start 观察同一个 Arc。
- 当前目标事务仍 pending：原 Core 状态锁和目标 outcome 锁内把同一个 Arc 标为本 Runtime 的退役 scope，seal 池；不再 spawn。现有 driver 在最后发布结果前必须用同一锁顺序检查是否升级，并完成 sealed pool 的 Preparing/retired 收尾。其 ClientCloseObservation 从初始 drain 保留到最终收尾，不能中途丢掉已观察的 ticket。
- 当前目标已经真终态：保持原 journal 不可变。新的 Runtime scope 只收当前 sealed pool 的剩余工作；Server/route/relay 清理不重放。旧 journal 用 terminal_outcome 非消费地读取，不经过 Runtime::shutdown 的 delivered 分支。新 observer 在 seal 时捕获尚未结束的 records，以免它们在 worker 调度前确认后丢失异常记录。历史已确认部分用原 journal，当前剩余部分按 ticket/原 observer 消费，不能按地址去重或双计。

这里“最后收尾”始终包含 seal 时仍在 entries 中的活连接、Preparing 和未确认 retired records。三种情形都必须执行原 close_all 的完整 detach→登记→关闭过程，不能只扫 record。seal 本身只禁止准入并捕获 observation，不把 entries 预先标成一个无人执行的 Closing 批次：否则升级时可能让原 driver 等待自己还没有处理的批次。entries 在原池里仍有强 owner，直到唯一 driver 通过既有 state→close_txn 临界区摘取并登记；完成判据同时要求 entries 为空和所有未确认 record 已结束。

准备与交接采用锁外重新准备规则：锁外的 pending 观察只决定暂时准备哪些资源，不能授权发布。Core 状态锁 → outcome 锁内再次核对；若准备时 pending、拿锁时已真终态，先放锁，按终态情况准备必要资源，再拿锁核对。固定目标终态不会回退，所以这一转换最多重来一次；若另一调用已建立 Runtime scope，直接使用同一 receipt。真正需要新 driver 时，thread spawn 在已核定状态的临界区内、seal/map/slot 发布之前执行，worker 的第一步取得同一 Core 锁；spawn 失败仍未转移所有权或改变准入，成功后再发布。不会预先停住一个闲置备用 worker。升级挂入与 driver 取执行上下文/决定发布都在同一个 outcome 临界区完成。

结果发布与 scope 升级必须在同一 Core 锁序下互斥：driver 若已发布终态，新的调用走第三种情形；若先升级，driver 必须完成扩大的范围才发布。一个正常 close_all 不能重新开放 sealed 池。Preparing 的等待使用现有 record.wait_until_settled，把状态条件扩大为 Preparing|Closing；set_state 的原通知负责唤醒，不另外轮询或计数。

本项只改变显式 Runtime 退役的前置与 scope-aware 收尾，不顺带重写无调用方的 HostInner::Drop 或 failed-start fallback。为本次 scope 准备所需 executor 与 driver（若复用已有 driver 则不再 spawn），失败发生在本次退役发布之前，Native 原 owner/slots 保留并返回错误。新 scope 只准备一个 executor，各阶段复用；升级 pending 事务时把准备好的执行上下文交给同一 driver 的 scope-aware 最终收尾，不让它在发布退役后因再次构造 executor 而丢掉目标。这个上下文只装本次执行资源，不是新命令/任务框架。

已有 Host receipt 可能含错误而 Server 仍活着，必须按真实 Server 生命周期分类，不能直接当成“目标已真终态”的第三种情形。若原 driver 仍在，scope-aware 最终收尾用已准备的 executor 继续原生关闭屏障；若原 driver 已退出，由本次唯一 driver 取得同一 Server 强 owner，走现有 close_business_admission/request_shutdown/observe_shutdown_completion 路径到真实终态。保留旧错误，原 route/relay 已完成的副作用不重放；只读旧 journal 与实际 Server 关闭 journal，safe-close 依据实际收尾结果。未观察到目标真实终态，Runtime receipt 就不能发布终态。

未参与本次退役的普通 Host Drop/failed-start 仍沿原返回形状和原收尾策略；不会因本项删除其非 fallible receipt 路径。它们最后一个 owner 与工作器创建同时失败的兜底属于单列的最终 Drop 问题，本项不承诺修好，也不靠新的 inline 无限等待或全局 reaper 兜住。这里的显式 begin 要么成功交给有 driver 的本次事务，要么在交接前失败并保留调用方 owner。driver 必须先写结果并通知，再释放持有的 Host clone，避免析构反过来等待它自己。

`completed=true` 只表示该 scope 的实际终态：目标 Server 的原生命周期/关闭屏障已终态，sealed pool 不再有 Preparing、Closing 或 UnconfirmedIdle。终态可以保留真实清理错误，错误不被清空，hook 仍看 safe-close；尚有真实 owner 工作就必须 pending。不能因一个错误字段而让已安全结束的 Runtime 永远锁死，也不能因 worker 返回了错误就丢掉仍在监听的 Server。Core 事务与 Native 字典投影使用同一观察，不再混用旧 Host 的 completed。

scope-aware 最终阶段对 hosted、升级和 ClientsOnly 三种情形一律持续到完成判据成立；不能沿用 hostless 原来只 drain 一轮的分支。Preparing 等原通知，UnconfirmedIdle 仍按既有有限 barrier 重试。

接手目标的强 owner 来源固定：Host 目标由 HostInner.server 取得；FailedStart 目标只从 receipt 已记录的 Weak<Server> 升级。Weak 已失效时没有这个 Server 对象可再关闭，只读其既有 journal，并收本 scope 的池；不另造“Drop 即已确认关闭”的判据。即使 transaction map 剪掉旧 key，本次仍直接持有 receipt 的事务 Arc，不按 key 重新寻找历史结果。

caller 的 timeout 仅控制这一轮等待，不作为 driver 的首次 close_all 预算。driver 复用 Host 的既有 shutdown_timeout（hostless 使用现有默认清理预算）及现有未确认 close 的一秒重试节奏；Preparing 用通知推进。没有额外业务重试，也不把清理预算重新加到已过期的 connect 调用上。caller/receipt 被丢弃后，已启动 driver 仍持有 Runtime、目标和池；完成后 receipt 仅持结果/弱身份，不制造 owner 自环或历史对象列表。

| 情况 | 实际 owner | 谁继续清理 | 调用方如何观察 |
| --- | --- | --- | --- |
| 原 connect 预算耗尽 | FreshClientGuard 转交的原 retired record；正在进行 I/O 时仍由该次 acquire 持有 | 原有限 settlement 与 Core 退役 driver 按原 claim 协调；无业务重试 | connect 仍抛原 CallDeadlineExceeded；Runtime.shutdown 的同一 receipt 返回 pending 或最终结果 |
| shutdown 等待预算为零/到期 | 已启动的 Core Runtime 事务及其目标、sealed pool；SDK 保留旧 session/slots | 原事务 driver 继续，预算不被重置到 connect 上 | 返回 completed=false；再次 shutdown 观察同一个事务，不重做旧 Host 业务清理 |
| 某次物理 close 仍 unconfirmed | RetiredClose.client 中的精确 Arc 与预算 owner | 同一 Core driver 在原 close barrier 上继续；Preparing 用原通知唤醒 | ipc_clients_drained 不得为 true；正常 waiter 期限独立 |
| Host 已 terminal，但还有旧池工作 | Runtime scope driver；旧 Host journal 仅作不可变输入 | 只收池中的真实剩余工作 | Runtime receipt 不直接返回缓存的 Host completed |
| 所有物理关闭已确认但有终态错误 | 按原返回值区分“事务终态”和“清理成功”；Held/RequestBlock 保留自己的 owner | 不重放已确认释放，不更改 R07 的最终 Drop 策略 | 原错误及 route/IPC 字段原样可见；hook 仍依 safe-close 判断 |

## Python / Native 收束与接口草案

公开 `Runtime.shutdown(timeout=...)` / `cc.shutdown` 签名不变。在实例锁内启动上述真实退役事务，锁外等待；pending 保留当前 NativeSession/slots。完成后，在原实例锁内先构造并采用新 session 的 handoff，再唯一取走 outcome 中 hook-safe slots，发布替换，锁外执行 hooks。失败构造不得先消费 slots；旧并发 observer 不能替换新 session。hook 重入注册属于新 session，旧结果不描述它。Bridge 与 Runtime 共用 slot 消费函数，均停止提交可能过期的 Python route_names 名单，低层显式名单检查保持。

低层 Rust 草案保留已有 FailedStartShutdownReceipt，避免为复用删除一个仍有独立含义的公开类型；新增 RuntimeShutdownReceipt，共用原 ShutdownTransaction 及 observe_with_timeout/terminal_outcome 实现，不复制观察逻辑、不建立别名。新增 `Runtime::begin_session_retirement(RuntimeShutdownTarget, deadline: Instant) -> Result<RuntimeShutdownReceipt, LifecycleError>`；target 是 Host(&Host)、FailedStart(&FailedStartShutdownReceipt)、ClientsOnly 的封闭三选一，边界一次核对 Runtime 归属，不能把 Runtime receipt 传回去形成等待自身。它是供已序列化 Host 所有者使用的低层入口，NativeSession 在原 Host 锁内捕获并立即开始；不是自动枚举任意 Rust Host 的公开高层能力。ClientsOnly 的 completed 只覆盖声明的 client scope，不能据此声称 Rust 调用方另行创建的 Host 已结束。原 Rust SDK Host 关闭/重启语义不变。暂不新增 Core current-Host 注册表或迁移 Native HostRecord，以免扩大到整个 Host 构造所有权重构。

Native 仅投影 begin + observe。begin 返回本次观察的原生 deadline 与只读 receipt，已经启动真实事务，不是新命令队列；观察在同一 deadline 内释放 GIL 等待。Rust 校验一次且在发布退役前完成；Python 的数值转换在实例锁外完成，内部回传 f64 后不会再调用用户 `__float__`。推荐统一成 Native f64 提取：Runtime 有 Host 时不再接受数值字符串，hostless 原行为保持。Python 实例锁的等待维持现状：register/start 可能持有它到各自操作完成，shutdown 不承诺覆盖这段等待的总墙钟期限。取得实例锁后创建一次 native shutdown deadline；Native Host 锁、Core 锁等待和 receipt 等待共用它：零预算只尝试立即取得短锁；若不能取得且事务还未开始，返回结构化 `shutdown_wait_timeout`（details.stage 标明 owner/core 等待、timeout_seconds 保留），不能说成一个已启动事务的 pending。已经有 receipt 后超时才返回 completed=false。failed-start receipt 的等待移到 Host 锁外，Core seal 已阻止新发布；Native 最终清表复用 registrations/server_bridge 的原锁，并校对同一目标/事务；不在 Host 锁内等待 receipt。Core seal 与 SDK 原实例锁的发布顺序负责阻止新 Native 工作混进来。Native 原 shutdown 在已进入 Runtime 退役时必须观察其 receipt，不能退回旧 Host/hostless 分支。

在 c2-ipc 增加类型化 `IpcError::PoolRetired`，首进、重试登记和最终入池都直接返回它，不匹配 Pool(String) 文案。sync_client 的阶段穷举明确把 PoolRetired 归为 PreDispatch；normalize_ipc_error 映射到 LifecycleError::RuntimeRetired；c2-http router 的穷举分支同步接入现有本地不可用处理，不加新的 wire 错误码。Core 在既有 deadline 与合同校验之后，以 `LifecycleError::RuntimeRetired` 拒绝退役实例的新 connect/Host；对应 CoreError.category=lifecycle、lifecycle_kind=runtime_retired，io_* 仍为 None。pool seal 的拒绝也映射为这个结构化原因，relay local_candidate_failure_is_terminal 必须认它：不能再触发 HTTP fallback、alias 重解析或瞬时重试；在已准入的 HTTP 元数据阶段结束后仍检查退役，HTTP-only 不创建 IPC 池。已捕获 session 的 Python connect 在最终发布点使用同一个 ConnectAttempt 再检查 session 身份与退役事实，失败只释放自己的逻辑 Client。物理关闭已经属于上述事务，无需在 Python finally 添加新 drain 调用。新增低层生命周期拒绝类别应直接表示 Runtime 已退役，不使用 config error；Python 使用已有 CoreError（RuntimeError 家族），不新建异常类；deadline 先到时仍是原 CallDeadlineExceeded。当真实工作无法确认关闭时，pending 没有总完成上界，不强退 lease 或伪报已停止。pending 期间新的 register/start/remote connect 被拒，即使 Core 后来完成，也要由下一次 shutdown 消费结果并换代后才能再次使用；这个对外变化需用户确认。

实际更换 session 的 transport-policy 分支同样检查原生退役事实；其他配置保留既有冻结、忽略、继承和重置规则。thread-local connect、已返回代理、Held、独立 owner capability 保持既有所有权，不对每次 RPC 叠加 gate。私有 reset/atexit 重入、低层 standalone Bridge 的任意并发重新绑定、register 失败回滚的静态 hook 线索仍单列，不能把本项作为它们已修复的依据。

## 范围、增量与验收

生产涉及 `c2-core/session.rs` 的事务 scope/receipt、`host.rs` 的共享启动与观察、`client.rs` 的准入/最终发布，`c2-ipc/pool.rs` 的永久 seal 与 Preparing 收尾观察，以及 `runtime_session_ffi.rs`、Python registry/Bridge 的投影/换代。没有 IPC 帧、SHM、FFI 结构改动；IPC 14、SHM 3、C/FFI ABI 6 不变。低层 Rust 新增 RuntimeShutdownReceipt / RuntimeShutdownTarget / begin_session_retirement、IpcError::PoolRetired 和 lifecycle 变体，Python CoreError 的 runtime_retired / shutdown_wait_timeout 取值，Native 内部 begin/observe 形状，pending 期间新操作被拒及随后显式消费换代、hostless 清理持续推进，以及 timeout 字符串拒绝均属需确认的变化。保留旧 Rust receipt 的真实作用，不保留任何被取代的实现或兼容分支。

前稿生产 +60–100 行不足以覆盖已证实的所有权边界，撤回该估计。当前估计生产净 +200–330 行（Core/池约 +130–230，Native/Python 约 +70–100），测试净 +280–450 行（已补基线约 160 行计入其中），文档净 +40–80 行；均为实施前估计，不是实测收益。不新增通用框架；若实际需要独立后台清理系统或历史 owner 列表，应拒绝该实现。

确认后先保留并跑红当前四项反例，再追加“Preparing 跨 seal、错误已返回但原 owner 未释放、zero/pending 重复观察同一 receipt、同地址新旧 incarnation、worker 创建失败、并发 shutdown、failed-start receipt 保真、hook 重入/换代失败”的既有层级用例。对 record/锁交错使用现有测试 hook，不靠加长 timeout、sleep 赌调度或弱化断言。

完成针对性回归后跑完整 relay、Rust/Python/Node 跨语言、三系统及 GitHub Actions（含 Windows ARM64/3.14t）。Windows 优先开发机；验收包括 retained Held/预算无提前退款、原始 deadline 不重置、正常连接路径没有额外线程/复制，以及 task-owned socket/lease 零残留。性能只测新增 Core/Pool gate 的普通连接开销与关闭期进度，不扩大成全业务 benchmark。历史目录及 hey-my-buddy 仓库不动，验证后删除本批可重建产物，不发版。


## 审查后的验收补充

四行已运行代码是基线诊断，不能原样把 expired 行当成修复后验收：Core 提前拒绝后会进不到最终发布 hook。实施时两条入口迟到用例断言结构化 runtime_retired；另将预算耗尽用例的停点移到“真实 native acquire 已入池、seal 之前”，并强制断言到达停点和原预算确已耗尽，不能用缺失 observation 的默认 True 空过。所有客户端关闭断言都在 peer.shutdown 前完成；最后 peer 收尾仅用于失败情况下的测试卫生。报告中“逻辑清理”包括成功路径显式 close 和错误路径现有 finally terminate，不推断其能物理关闭。

还须固定瞬时重试的 record 空窗、relay 不 fallback、spawn/executor 失败发生在发布之前、目标已由 owner/failed-start 终态且 journal 已读过、pending 时准入拒绝/再次消费恢复、升级与结果发布竞争。Core/池现有 driver/record 通知应直接作为这些测试的观察点，不另造仿制状态机。保留 Python 最终发布复核：它防止把已失去所属 session 的代理作为成功结果返回，意义与物理关闭不同；同一 ConnectAttempt 先检查原 deadline。暂不删除低层 Runtime::shutdown 的 delivered 行为；本项所有目标观察绕开消费分支，hook 唯一执行由 slots pop 保证。

指定审查、版本来源及逐项落实见[最终方案审查记录](2026-10-28-runtime-retirement-plan-review.md)。原始四项红灯作为基线证据保留；实现已把测试改为入口拒绝和真实入池后退役两组有效回归，最终验收以本轮记录为准。

实施接口落实：RuntimeShutdownTarget 借用调用方的 Host/failed-start receipt，事务成功交接时才持有克隆，避免失败参数路径消费最后一个 Host。重复调用只能观察已覆盖的目标，不能用 ClientsOnly 的 completed 冒充另一个未声明 Host 已停止。SDK 远程 connect 的起始拒绝直接复用 Core 入口，Python 只保留最终发布复核，不重复做起始 native 检查。

实施审查修正：新 scope 保留目标原有 Weak 身份，即使 failed-start Server 已析构也能重复观察同一事务；封池先按同一 deadline 取得原 state/coordinator 锁，成功准备 driver 后才 commit。Native begin 保存原生 Result，SDK 在释放实例锁后的 observe 才构造 Python 异常；原生低层 shutdown 的退役分支同样使用原 deadline，显式名单校验保留。低层只会产生 LifecycleError 的新入口直接返回该类型；retirement_receipt(deadline) 及 Host.shutdown_outcome_until(deadline) 提供有界只读观察。已在 driver 入口封闭并完成的池不再重复 drain；pending 升级与历史 journal 的剩余池仍必须收尾。生产/测试增量已超过预估，最终按实际 diff 报告。

SDK 新增退役 gate 的拒绝投影也移到 binding 锁外：私有 retired 观察即时读取既有 Core fence，不保存 Python 状态；锁内确认永久拒绝后，解锁再沿原 Core 错误链抛出。最终 remote connect 在原期限内锁住并捕获同 session 事实，解锁后先检查原 ConnectAttempt/Core，再报告身份变化；不可逆的退役状态保证这次成功可线性化于锁内观察点。Native 的错误初始化器重入测试在独立子进程执行，以免修改共享异常类干扰默认并行门禁。
