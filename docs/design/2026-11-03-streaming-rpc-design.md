# 流式 RPC 设计

状态：已批准（2026-11-04），未实施。维护者按推荐裁定第十二节的七项，服务端结束流选 (b)，决定记在 [ADR 0003](../decisions/0003-server-streams.md)；执行计划在第十节，进度只记在[待办](backlog.md)。源码依据为 `645f06631450a5aa71a4d241757e98406c16c410`；源码推论与待运行验证分别标明，不把设计视为已有能力。

第一期做服务端流：一次输入、零到多个输出条目；同步生成器由 Rust 按步推进，等需求、额度或 I/O 时不占执行线程，每步结束释放路由许可。async unary 不作前置。unary 与流共用调用记录和传输实现；完整值接收方按整条预约，只有边收边转发的一跳使用有限滚动窗口。直接 IPC、同进程及 relay 的端到端证明完成后，才接受本期能力。

## 一、现状与依据

范围依据为批准时[待办](backlog.md)里的流式 RPC 与派发后查询两项、[单一属主评估](2026-10-31-single-owner-release-evaluation.md)的八问、[协议批设计](2026-11-01-protocol-batch-design.md)第三、四、九节、[路线图](roadmap.zh-CN.md)及现行 [Runtime 生命周期](../reference/runtime-lifecycle.md)、[内存策略](../reference/memory-policy.md)、[载荷生命周期](../reference/payload-lifetime.md)、[握手](../reference/ipc-handshake.md)、[契约与路由](../reference/contracts-and-routing.md)、[配置](../reference/configuration.md)。[HTTP 内存预算提案](2026-10-06-http-memory-budget.md)尚未实施，本稿把它纳入本期。

| 现状及证据 | 设计约束 |
| --- | --- |
| 帧已有 request_id 和 flags；连接核心不做 I/O，分别管理发布、释放与停用：`core/protocol/c2-wire/src/frame.rs:14`；`core/protocol/c2-wire/src/connection/mod.rs:279`、`:404`、`:411`。 | 请求号就是流标识，扩展同一个连接核心；调用终态不代替借出结算。 |
| unary 在接收循环预约并派发，回调取得路由/执行容量后进入 blocking worker：`core/transport/c2-server/src/server.rs:2512`、`:2520`、`:2548`、`:3067`、`:3116`。 | 保留 unary 的单次发送路径，不能为每次输入加一个额度往返；生成器只把一步交给 worker。 |
| 调度 ticket 控制读写顺序，guard 同时结束 pending 和 worker：`core/transport/c2-server/src/scheduler.rs:209`、`:395`、`:623`。 | 调用槽与每步路由许可分开记账，继续使用原调度器。 |
| 首片任务稍后预约；复制持重组池读锁，插入需写锁，feed 又持 shard 锁：`core/transport/c2-server/src/server.rs:2338`、`:3791`；`core/protocol/c2-wire/src/chunk/backing.rs:238`、`:259`；`core/protocol/c2-wire/src/chunk/registry.rs:317`、`:367`。 | B2 先缩复制锁，再移动首片准入，最后去排序闸门。 |
| unary 的接收循环已等待 dispatcher 的异步读锁：`core/transport/c2-server/src/server.rs:2462`、`:2520`、`:3142`。 | 这是现有 unary 的控制读取缺陷，也须修复；不能只处理分片。 |
| 结算状态可被 GC 删除：`core/protocol/c2-wire/src/chunk/registry.rs:155`、`:186`、`:471`；Node 严格匹配本次响应号：`core/foundation/c2-codegen/assets/typescript_transport.ts:641`。 | 已退役请求的识别不能依赖组装超时。 |
| relay 先取得完整响应字节，HTTP 客户端聚合 Vec：`core/transport/c2-http/src/relay/router.rs:1707`、`:1787`；`core/transport/c2-http/src/payload.rs:76`。reqwest 未启用 http2，axum 使用默认特性：`core/transport/c2-http/Cargo.toml:26`、`:40`；四份 Cargo.lock 均无 h2。 | 改原生响应读取和 Body 所有权，沿现有明文 HTTP、fetch 发送。 |
| Rust 生成资源 trait；Node 生成客户端并按整次调用串行：`core/foundation/c2-codegen/src/targets.rs:349`、`:927`；`core/foundation/c2-codegen/assets/typescript_transport.ts:532`。 | Node 需持续 reader；并发行为变化须公开确认，不能称为透明重构。 |

业界事实引用[流式调研](../research/2026-11-03-streaming-rpc-survey.md)第 1—9 节、[借出调研](../research/2026-11-01-shared-memory-lending-survey.md)第 1、2、5 节和[关闭调研](../research/2026-11-02-in-flight-calls-at-close-survey.md)第 1、4、5 节。它们区分阻塞线程、方向结束、调用终态、逐跳背压、已执行副作用和真实释放；调研中的未复现项仍未复现。Python context 与 hyper 的专项核对另在第三节给出一手来源。

## 二、第一期与执行模型

选择第一期只做服务端流，Python/Rust 提供资源和客户端，Node 提供客户端。资源返回同步生成器/迭代器，Rust 建立一份调用记录，保留原资源、准入凭证、调用槽和必要载荷；它们不等于路由许可。首次推进含建立迭代器及第一次 next；此后每次只执行到一个 yield、结束或异常。同一流最多一步在排队或执行，不设第二个步队列。

每一步在有需求、接收容量可推进时进入现有路由队尾，按方法的 read/write 语义取得路由许可和 worker；产出后结束它们，再做网络等待。资源须交付独立拥有、在发送期间不再修改的领域条目；共享可变视图须在本步导出独立快照，不能把它留到解锁后再读取。C-Two 新建的编码/本地副本在 Rust 预算边界接纳，容量等待留在许可外；不让生成器、GIL 回调或阻塞 worker 等消费者和额度。对未知长度编码，应在既有编码路径的分配边界逐块准入，验收必须证明其暂停不会把整条流留在线程池里。

调用开始后，后续推进和清理沿同一准入凭证继续；Host 排空、注销、缓存淘汰不把 continuation 当新调用拒绝。同名新资源不能接走旧流。远程调用的 `max_pending_requests`、路由 `max_pending` 计整个调用槽；`max_execution_workers`、路由 `max_workers` 只计执行中的步。同进程仍对象直传、只使用原生路由容量，不加入 IPC 的 pending/worker 池。

推论：生成器在两个 next 之间保存自身状态，不需要一个线程常驻；只在需求和容量具备后提交一次 blocking 工作，就能把等待移到 Rust。实施须证明这一点，不能把“循环 next、发送、等额度”整体放进一个 closure。单步计算本身可以很久，不按通常步长设超时，也不能抢占任意用户代码。

因此公开 async unary 不必先做。客户端流、双向流以后需要可挂起的资源桥：等输入/发送在路由许可外，领域操作才预约一步；Python coroutine/async generator、Rust Future/Stream 等可复用这个边界。只做 async unary 并不能证明跨 await 不持路由许可。本期不给未来方向增加状态或预留字段。长模拟和批量输出可用本期；持续双向耦合、分批输入待后一期。

建议顺序：本期共享连接/预算/调度 → 服务端流端到端验收 → 共享异步资源桥及 async unary → 客户端流、双向流。完整遥测、自适应策略另排；本期只交付预算已有形状的统计和正确性所需内部状态。

## 三、八个问题

### 3.1 积压、接收预约与两级窗口

额度表示接收方真实预约的存储，不能套在所有传输上。完整值接收方收到 ITEM_BEGIN 后检查长度并预约整条 N 字节；预约成功就一次授予整条，分片只负责传输与公平性。暂时没有容量则挂起该调用的预约，受其原截止时间及取消约束，reader 继续处理其他帧；就 B 的容量而言，只有 N 大于 B 本身才返回确定容量错误，资源步已执行时不能标为 pre_dispatch。输入/载荷格式及既有单消息限制仍独立检查。拒绝后 CALL_END 一次，后续片按第五节丢弃并释放。

只有边收边转发的接收方，例如 relay 的上游 IPC 一跳，声明有限滚动窗口并逐片返还：流级 S 约束该流的暂存，连接级 C 记同连接所有已授予且未结算的预约，二者从同一预算域兑现。令 F 为现有 IPC 片长、帧可用长度和远程发送粒度中的较小值，默认 S 为 min(B, F)，C 为该连接实际已预约额度之和，所有连接的预约连同域内其他存储不超过 B；多个连接不能重复许诺同一容量。第一期不暴露 S/C 设置。更深流水线只有测得需要后才经现有 CallOptions 按调用提出，不先加参数。

unary 的 OPEN 直接带输入、CALL_END 直接带结果；inline 时每方向一帧。大载荷保留 SHM 或分片选择，不等一个额外 CREDIT 往返，也不为滚动窗口把一次 SHM 交付拆成很多借出。SHM 的整份借出按实际 N 记在原借出表和 backing 预算中；relay 的小窗口只限定它新增的复制缓冲，不能把整份借出虚报为 S。

每流初始授予第一条需求；交付第 k 条给本地消费接口即授予第 k+1 条，不等应用再次调用 next，也不等 hold 释放。第一期最多一条尚未交付的结果，允许计算与消费重叠；这不是任意深度的预取队列。relay 的“本地消费接口”是下游 Body，实际消费者可能落后，边界见 3.8。

只保留一个扩大覆盖面的字节设置 `payload_buffer_budget_bytes`，环境变量 `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES`、relay CLI `--call-payload-buffer-budget-bytes`（沿用调用限额 `C2_CALL_` 与 `--call-` 的分组），记为 B，替换 retained-input 字节设置，不留别名。沿用其默认 16 GiB、完整 u64 范围，0 只拒绝正字节准入；Runtime 和 relay 各自共享一份，Host 的流本地副本使用同一规范默认值的独立接收域。依据为 `core/foundation/c2-config/src/call.rs:132`、`:140`、`:157` 及[配置](../reference/configuration.md)两处 retained-input 表。覆盖面在原 retained input 之外扩到 HTTP 缓冲及流的本地副本，不把所有 unary IPC 数据改成流控消息。SHM/file/reassembly 仍按原预算计物理存储。

B 不足时 next 等待，不因暂时竞争报告容量错误。等待只能等别的调用释放的容量：长度未知的数据按片增长时，调用已持有同一份数据的预约，拿不到新增预约就立即报确定的容量错误，不等待，否则单个调用会自等、多个调用会互相卡死；等待只留给第一份预约（长度已知时的整份，未知时的第一片）与流的条目（2026-11-04 实施块 3 时发现，对第十二节第 4 项的收窄）。调用方保留的多个 hold 都计 B；若它们用满 B，下一条会等到释放、原截止时间或取消。无限等待可能一直不返回；应用须释放不再需要的 hold，或配置能容纳同时保留数据的 B。C-Two 不偷释放、不让同一流的多 hold 报使用错误。调用本身保留的输入也是真实占用，预算应覆盖实际同时存活的数据，不能靠提前失效腾容量。

每份 C-Two 存储携带唯一 reservation；slice/clone 共享，最后实际引用结束才退款；复制过渡同时存在的存储须同时有预约。等待之前不先创建未计费副本，输入只保留到最后真实读者结束，不为可能重放多留一份。接收完整值可移动 Vec，不能多造一个快照；已预约的 SHM/文件重组存储也不能因 trim 提前退款。序列化临时工作区、应用材料化对象、peer 映射及 HTTP 库/内核缓冲须单列，B 不是 RSS 上限。

B 通过 `memory_stats()` 沿用 `limit_bytes/used_bytes/peak_bytes/rejected_allocations/rejected_bytes` 形状（`core/runtime/c2-core/src/memory.rs:216`），只给实际拒绝计 rejected，容量等待不算失败。窗口、需求和等待不增加公开观测面；测试读取内部状态和既有通知，不新增生产探针。流的调用槽复用现有 `C2_CALL_MAX_OUTSTANDING`（包括无限等待流）及服务端/路由容量，不能用零字节结果绕过调用数限制。

### 3.2 双方互等

选择唯一持续 reader/writer；reader 不等待资源、应用消费、预算或写锁。控制记录的必要容量随调用槽预留，不占业务额度；复制/预算等待属于原调用记录，不能每片生成一个无界等待任务。writer 在完整帧之间推进释放、额度和取消，按片轮转可写调用；CALL_END 不越过本调用应交付的数据。

第一期输入一次发完，不存在 handler 等下一个输入的循环。未来双向应用若两边都先无限写、再开始读，有限窗口仍会互等，须并行收发或定义领域轮次。任何控制都不能穿越 OS 已堵住的半帧；完全停读的同一物理连接没有持续进展保证。依据为流式调研第 2、3、6 节。

### 3.3 执行线程、路由许可与 Python context

建立、next、清理在同一调用记录中串行；只在可执行时取 worker 和路由许可。清理不需要新的调用槽、条目需求或业务额度，不能被慢消费者挡住。资源方法不跨 yield 保留独占锁；需要全流事务隔离时使用独立资源或领域快照，不能同时承诺全流独占和同路由其他操作可执行。

Python 每条流在开始时取一次 `contextvars.copy_context()`，建立生成器、每次 next 和 close 都经同一个 `ctx.run(...)`；不在每步重拷贝 worker 的 context。生成器本身不捕获 context，跨 worker 的 `decimal.localcontext()`、ContextVar token 等否则会串到推进线程。[PEP 567](https://peps.python.org/pep-0567/#abstract)明确不解决生成器隔离，并给出[跨线程 copy_context/run](https://peps.python.org/pep-0567/#offloading-execution-to-other-threads)；同一 Context 退出后可在另一线程再进入，不能并发进入，见 [Context 文档](https://docs.python.org/3/library/contextvars.html#contextvars.Context)。thread-local 不能代替这份流自己的 context；本稿不把它扩展成跨机器传播客户端 context 的功能。

### 3.4 条目生命周期

普通请求和完整响应保持“传输 carrier → 本地副本 → 结束远端读区间并排释放 → 资源/解码/hold”。PyO3 移动同一 Vec，给 Python memoryview；inline 能移动就移动。每条 portable 结果仍是 FastDB Payload，接收仍为 copy-backed，不宣称最终 SHM 原地构建。

普通迭代返回独立材料化值；hold 返回既有 HeldResult/Held，只保留本地副本。释放时先失效 FastDB 载荷所有者及 checked views，再释放副本与 B；活动 raw export 导致原有释放拒绝时，存储和账都留下。多份 hold 可并存，需求与其释放无关，只通过 B 形成背压。

借用输入仍由 registration 的 input_lifetime 选择。生成器可能跨 yield 使用初始输入，因此它的本地副本和受检所有者保留到生成器清理结束，随后先失效再释放；不保留客户端 SHM。Rust 返回独立寿命迭代器，不能 capture 工厂的 &Payload，需通过 FastDB 正式接口 materialize 成拥有的领域状态。

relay 是原生边收边转发路径：whole SHM 响应的 carrier 可保留到最后一片复制结束，每次只借用并复制 F 字节，借用 guard 不跨 await，复制完一份传输 carrier 才释放该 carrier。不能在尚需读取它时提前释放整份 loan。这个低层 Body 所有权不改变 SDK hold 只持本地副本的语义，也不承诺慢 HTTP 客户端不会延长上游的大借出。

### 3.5 终态、取消、句柄回收与截止时间

调用记录只有一个终态。自然耗尽及 finally 清理完成后给 OK；资源错误排在此前完整条目后。最后完整条目数只放 CALL_END，输入条数由 method mode 决定。半条目不交付，已经交付的条目不因后续异常、取消或 EOF 失效，领域副作用不回滚。

Python finalizer、Rust Drop、Node 最后流句柄回收或 iterator return 都把同一个 CANCEL 请求交给原调用记录，不等网络和清理；Python with 提前退出及 Node return 也立即返回。重复来源只留下同一取消意图。取消阻止尚未开始的下一步；已运行的步照常到暂停点，实际清理在路由许可下执行。已产出的结果不重放；接收方已放弃或真实断开时可丢弃待交付字节，但须结算原 carrier。清理后才形成 `CallCancelled=718`，沿既有 C2E1/CoreError 投影；当前规范码到 717（`core/foundation/c2-error/src/lib.rs:48`）。已自然结束的真实终态不被迟到 CANCEL 改写，清理异常保留真实错误。

只有显式 `close(timeout=30.0)` 等原调用记录完成：缺省 30 秒与 `cc.shutdown()` 的观察预算一致（`sdk/python/src/c_two/transport/registry.py:1107`），Rust 与 Node（30000 ms）用同一个值；None 无限、0 发出取消后立即观察。返回 completed=False 时清理继续。close 成功报告实际终态，不能把 CallCancelled 当自然 OK；context/finalizer 不暗中调用这个有等待的 close。句柄回收的及时性受语言 GC 约束，需要确定观察时使用显式 close。

调用截止时间涵盖编码、准入及整条迭代，不为每条 next 重置；到期只结束调用方等待。若随后外壳被回收，会另按上述句柄规则发 CANCEL，不能把它说成截止时间本身取消执行。未取得调用终态时按派发事实报告 pre_dispatch 或 DispatchUncertain，不自动重试、重放、对冲。默认流等待无限，不继承 HTTP unary 的 300 秒总调用默认；同进程本期仍明确拒绝有限调用截止时间，不能用 Python 计时器假装实现。

### 3.6 服务端结束流与关闭

Runtime shutdown 与路由注销都向本路由开着的流的调用记录提交同一个 CANCEL，复用 CANCEL → 清理 → CallCancelled，不设第二套强杀状态机，也不新增运维取消接口（维护者 2026-11-04 选定 (b)）。注销也这样做，是因为它和 shutdown 一样要等路由上的工作排空（`core/transport/c2-server/src/server.rs:1027`、`:1038`）；只对 shutdown 生效的话，开着无限流的路由永远注销不掉。注销这一条由 (b) 推出，维护者 2026-11-04 确认。这里作用于 Host 作为资源方的流；Runtime 作为客户端持有的流句柄仍按 3.5 回收或显式关闭。

停在 yield 或等需求、预算的流直接开始清理；正在运行的步照常完成，不再推进下一步。已产出结果和当前步结果先交付，再发送 CallCancelled，错误消息写明起因是 shutdown 还是注销；客户端没有自己取消却收到 CallCancelled，就是服务端结束了流。这改变了现行“shutdown 只停新调用、已派发的调用都跑完”对流的承诺，unary 不受影响；块 6 合入时同步 AGENTS.md、生命周期文档与发行说明。

不抢占正在运行的用户代码。消费者停读时当前步结果交付不出去，关闭就如实保持未完成；其他显式取消或下游已放弃时，按 3.5 结算不再交付的结果。关闭预算只界定观察，届满 receipt 如实报告 completed=False；listener_closed、work_drained、finished 分别按事实，不把 CANCEL 已排队当终止。已发布借出只凭释放记录/真实读端对端结束回收。

业界依据为流式调研第 7 节：grpc-go Stop、grpc-java shutdownNow、Python stop(grace) 提供强停；gRPC 指南要求为优雅关闭另设计时器强停。C-Two 采用更窄的协作式结束，不把这些强停先例改写成能够中断任意 handler。

停用、淘汰继续只封新调用/借出。旧流按原准入凭证执行；停用后的新输出走 owned 分片，已有 SHM 借出继续结算。物理关闭须等调用记录、已接纳写入、远端读区间及借出真实结束；driver 的 flight 计数投影同一准入凭证，不另建权威。真实对端结束时，当前资源步结束后清理；relay 下游真实断开向上游发同一 CANCEL，转发调用记录留到实际终态/故障。unary 已进入资源的方法仍继续执行。

### 3.7 大条目与公平性

完整值接收方预约整条后，N 可以大于转发窗口或片长，不需要等多个 CREDIT 才传完；只要已预约存储可用就连续传输，writer 按完整分片轮转，不让一份大分片序列独占连接。单次大 SHM 仍是一份真实借出，小描述符的发送不会像巨大 inline 帧一样长期占 writer。

边收边转发时，每复制并移交一片到已预约的下游缓冲才返还该片额度，Body 不取下一片时不扩大窗口。SHM 的 whole carrier 沿 3.4 分段读取，尚未复制的部分保持原 loan；普通 byte 分片依窗口交错。控制优先于下一片，不能穿透已阻塞半帧。每方向仅一个未完成条目、分片按序，位置及进度由接收方计数，不增加线上序号/偏移的第二份真相。

帧表示范围、本机地址空间、现有单消息/重组预算分别约束实际分配，整个流不加累计字节、条目数或固定时长上限。多个不释放的 peer 仍可能耗尽共享物理预算；不把小转发窗口称为全局 SHM 容量隔离。流式调研第 6 节支持按片轮转，借出调研第 2 节说明单方限额不等于为所有连接预留容量。

### 3.8 relay、HTTP 及 Body 背压

沿现有栈：请求体一次发完，响应体逐条流式发送；“一次”指一次业务输入，不要求物化整份请求，已知长度仍可经现有 sized-stream 传输。Node 继续用可注入 fetch（`core/foundation/c2-codegen/assets/typescript_transport.ts:178`、`:810`），Rust 继续 reqwest/axum；HTTP 不发送 CREDIT。取消用业务响应开头给出的调用令牌发 `DELETE /_calls/{token}`，relay 向上游 IPC 提交 CANCEL，原响应继续可读到终态；客户端中止 HTTP 请求/响应时，relay 观察到下游结束也向上游提交 CANCEL，只是客户端读不到终态（第十二节第 9 条）。其他共享上游调用不随之取消。经 relay 的调用只发一次业务 POST，不为重发保留输入：relay 在读请求体之前按请求头核对契约与路由 token，路由已过期就报派发前错误，由调用方决定是否重试，下一次调用重新解析路由。过期路由通常意味着资源换了实例，新实例没有原来的状态，应用应先从检查点恢复再继续按顺序调用（维护者 2026-11-04 确认）。

relay 只有在下游 Body 被拉取且取得本地缓冲预约时才从上游取下一片；多跳同样逐跳拉取。原生 IPC 响应必须提供绑定实际 carrier 的增量读接口，relay 不先调用 into_owned_bytes，不先组成完整条目 Vec。whole SHM 沿 3.4 保留真实 owner，逐片复制后交给 HTTP；Bytes clone/slice 保留同一 reservation，到最后实际引用结束退款，Body 自身 Drop 不是所有字节已经释放的证明。unary HTTP 调用的资源语义不变，只把响应转发改成这个 Body。

B 覆盖请求保留、成功/错误响应、HTTP 暂存及流本地副本，长度已知先预约、未知按片增长；临时不足等待而非失败（增长时例外，见 3.1），等待可由原截止时间/取消结束。请求保留在最后读者结束时释放，不能为了等响应强留完整请求；两个方向的最小发送缓冲在派发前从同一 B 留出，避免全部空间被输入占满。多跳各有自己的 B；SHM backing、HTTP 库内部空间和 OS 缓冲另记，不称整个路径只有一个窗口占用。

专项静态核对：当前 `core/Cargo.lock:637` 锁定 hyper 1.8.1。[HTTP/1 写循环](https://github.com/hyperium/hyper/blob/v1.8.1/src/proto/h1/dispatch.rs#L328-L373)先检查 can_buffer_body，不能缓冲就 poll_flush；只有恢复后才 poll_frame。[缓冲实现](https://github.com/hyperium/hyper/blob/v1.8.1/src/proto/h1/io.rs#L575-L581)按已有字节/队列容量判断，默认 flush_pipeline=false（同文件第 67 行）。这证明默认路径有发送缓冲门，未证明“每拉一次 Body 就是客户端已消费一次”：一次 frame 仍可越过软水位，已写入库/OS 的字节还能继续积压。本次只读源码，未运行慢读实验。

推论与代价：经 relay 的生成器可比最终消费者多推进几步，受各跳有限 Body/HTTP/TCP 缓冲及本期一条本地未交付结果限制，不承诺恰好一步。空条目也编码非零长度的条目头，不能绕过字节缓冲。可注入 fetch、外部代理若全量缓冲就会破坏此承诺；支持的连接器/部署路径须验证其有限缓冲行为。该提前执行的副作用边界交维护者确认，不能把 TCP 背压当成精确的应用需求确认。逐跳背压事实见流式调研第 8 节。

## 四、提交维护者确认的顺序契约

1. 同一物理连接的开始顺序，是 OPEN/首片在接收循环完成路由预约的顺序；预约不等待资源、重组复制或执行容量。输入未齐的早到写保留其位置。
2. 同一路由 exclusive 按预约开始；read_parallel 允许读并行，但后到读不越过已排队写；parallel 可重叠，不保证开始/完成序。不同连接没有“谁先发送”的全局顺序。
3. 流首次推进用最初预约；后续每步和清理在条件具备后进队尾。yield 结束路由许可，同路由 unary 可在下一步前执行；整条流不等于一个独占事务。
4. 同流条目按生产顺序交付，片按序写入；未来双向各方向独立，跨方向因果由领域协议定义。额度及交付不等于领域提交回执。
5. 完成序独立于开始序，每个调用唯一终态；长流不挡其他调用完成，后来的错误不能撤回先前完整条目或确定终态。
6. 跨路由没有连接级写屏障。应用先等前一调用确定完成，或等 CRM 明确声明“已提交”的条目，再发依赖调用；收到中间结果不表示整条流已完成。
7. Node IPC 去除 callChain 后，不等待就连续调用会由原来的整次串行变为并发；unary 用户也受影响。依据为 `core/foundation/c2-codegen/assets/typescript_transport.ts:532`。本期选择与 Rust 相同的路由预约契约，不另保一条 unary 串行通道；维护者须确认，发行说明必须写明，依赖原行为的应用用 await 表达顺序。

## 五、B2 与分片遗留

### 5.1 先缩锁、同步预约、再去闸门

先让已分配重组 backing 的实际所有权固定存储、范围和 reservation。复制只持本条目的读/写借用，不持公共池锁；registry 在短 shard 锁内取得调用记录/条目所有权后离锁复制，GC/take/结算通过同一个条目状态决定终态，实际析构在锁外。映射和分配留到最后真实借用结束，不能把裸指针带出 guard。finished/trim 后继续计真实容量，文件 backing 按原 owner 释放。

再把 dispatcher 发布缩为短的同步边界：契约准备、载荷和异步 I/O 在边界外，接收循环在边界内取得同一 dispatcher 的 route Arc 并预约，不增加旁路缓存。这个修复同时覆盖现有 unary 的 await（`core/transport/c2-server/src/server.rs:2462`、`:2520`、`:3142`），不是只为 B2 新增的条件。

随后首个 OPEN/首片在 reader 发布调用记录和准入凭证，再交有界任务分配/复制；后续片只找该调用记录。最后以同一调用记录的终态/所有权替换 `core/transport/c2-server/src/chunk_ordering.rs` 的排序门及专有围栏，包括 `core/transport/c2-server/src/server.rs:3712` 的 Pending 返回分支。当前门容量检查在帧许可先释放、拆除围栏尚存时仍有职责，结构替换前不能单独拿掉。

### 5.2 迟到续片与请求号高水位

新请求号在 writer 第一次提交线性点分配，使 OPEN 在线上严格递增；业务及有回复的控制使用同一序列，0 留给既有无关联控制。现状在 begin_pending 预先取号（`core/transport/c2-ipc/src/client.rs:1143`），并发取号本身不能证明写序，须同步调整。

连接核心保留已见 OPEN 最大号和活动调用表。新 OPEN 先推进最大号，再接纳或拒绝一次；不接受同号新调用。对活动调用只由其调用记录持有一次终态权。号不大于最大号、但已经退出活动表的续片只按可信帧边界接纳/释放 carrier 后丢弃，不重新建调用记录或发第二个错误；未来未打开请求的数据在协议边界拒绝。客户端也按活动调用表分派，迟到回复不能冒充下一次调用的响应。

因此“拒绝 → chunk_assembler_timeout 后 GC → 迟到续片 → 下一次 Node 调用”始终只给原请求一个终态；识别历史的事实是一个高水位，不是每个请求的永久 tombstone。借出表独立存活，迟到 RELEASE 仍按原连接结算。GC 只清真实重组存储；未进入资源的输入准备可按原组装超时确定拒绝，已执行流及等需求/额度的活动条目不按静默时长 GC。高水位不靠恢复重组代际守卫实现。

### 5.3 其余边界

| 项目 | 选择与退出条件 |
| --- | --- |
| C03 | 保留 assembly 与 pending 准入凭证的单一所有权；GC、拒绝、取消、断连使用同一调用记录，旧任务不再取得终态权。证明复制/析构交错不提前退款或释放别的调用槽。 |
| C04 | 使用真实额度/步骤/释放通知；测试复查最后外部 owner、锁外结算、原截止时间及无丢唤醒。没有统一通知源的旧 pool/relay timer 保留进展职责，不捆绑整批替换，见 [C04 复核](../acceptance/2026-10-26-c04-notification-reuse-review.md)。 |
| R14 | owner/generation/范围在 backing 接纳边界验证一次，内部使用受检所有权；保留真实预算、层级回退及退款。不增锁证明框架/缓存，不扩到 bitmap 清理，见[精简账本](2026-10-13-local-slimming-batches.md)。 |
| max_total_chunks | 单一用户设置改名 max_inflight_chunk_frames，只管分片处理容量；registry 的软 GC 触发点从它推导，不再给软维护另加配置。硬容量与软扫描现状分别为 `core/transport/c2-server/src/server.rs:558`、`:567` 和 `core/protocol/c2-wire/src/chunk/config.rs:33`、`core/protocol/c2-wire/src/chunk/registry.rs:268`。 |

## 六、公开接口与契约

Python 增加 CRM 标记 `@cc.stream`，第一期只表示服务端流，和 read/write、transfer 正交。Payload spec 继续只从显式 transfer 取得，注解不推断 FastDB 结构；资源是普通类。Python-only 编码仍逐条沿既有 codec，不能被当作 portable codegen 输入。

~~~python
from collections.abc import Iterator
import c_two as cc
from fastdb4py.payload import Payload

@cc.crm(namespace="science.sim", version="0.1.0")
class Simulation:
    @cc.stream
    @cc.transfer(input=PARAMETERS, output=STATE)
    def steps(self, parameters: Payload) -> Iterator[Payload]: ...

class BasinSimulation:
    def steps(self, parameters: Payload) -> Iterator[Payload]:
        model = initialize(parameters)
        try:
            while model.has_next():
                model.advance()
                yield model.export_owned_state()
        finally:
            model.finish()

cc.register(Simulation, BasinSimulation(), name="basin")
proxy = cc.connect(Simulation, name="basin", address="ipc://simulation")
results = cc.with_call_options(proxy, timeout=run_wait_seconds).steps(parameters)
try:
    for state in results:
        consume(state)
finally:
    outcome = results.close()
~~~

`Stream[T]` 提供迭代、context manager 和 close；context 提前退出只提交取消，须等清理时用上述显式 close 并检查 completed。`cc.hold(proxy.steps)(parameters)` 返回逐项 HeldResult 的流，多 hold 可并存；`input_lifetime={"steps": "borrowed"}` 仍只在 registration 选择，寿命见 3.4。with_call_options 仍是同一 proxy 视图及一次绝对截止时间，不给每条结果续期。

Rust/TypeScript 保留 `method_{index}_{name}` 和 hold_ 命名（`core/foundation/c2-codegen/src/targets.rs:1076`）。Rust 客户端调用 `client.method_0_steps(&parameters)?`，得到 `Result<ResponseStream<fastdb::Payload>, c_two::Error>` 的成功值，再逐项处理 Iterator 中的 Result。资源实现生成的 Service trait，方法返回 `Result<Box<dyn Iterator<Item=Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>`，工厂用 Box::new 包装独立拥有的领域迭代器；允许捕获领域 Arc，不能借用工厂参数/&self 越过返回。hold_method_0_steps 返回逐项 Held。native 调用记录只在清理步骤中析构资源迭代器，Rust Drop 的流外壳只提交 CANCEL，显式 close 才观察完成。

Node 的 `method_0_steps(input)` 返回实现 AsyncIterable 的 C2ResponseStream；hold_method_0_steps 逐项返回 C2HeldResult。`for await` 提前退出的 return 只提交取消，`await results.close()` 才等实际清理。资源侧本期只有 Python/Rust；Node 尚无 Host 注册桥（`core/foundation/c2-codegen/src/targets.rs:761`、`:927`），不生成不能注册的资源类型。若维护者要求 Node 也提供资源，须先扩充 Host callback/清理/注册范围及 Node 资源验收。

Node IPC 每个物理连接只有一个持续 reader，按请求号分派到有界调用记录；native endpoint context 复用 Rust 的额度、借出和终态。传输工厂的原生预算域跨连接、重连共享，不能每建连接重置 B。HTTP 继续 fetch，按响应 Body 增量解码；框架不增加第二套 TypeScript 状态机。

选择保留 `c-two.contract.v2`。只有服务端流方法写 `"mode": "server_stream"`，缺省即 unary；不输出 `"mode": "unary"` 的等价表示。流的 output binding 仍描述一条 Payload，return.kind 仍为 payload，允许零条结果。第一期不接受尚无实现的其他 mode。

代码依据：`core/foundation/c2-contract/src/descriptor.rs:123` 对输入 JSON 本身规范化，没有补缺省字段；`:490`、`:502` 分别显式构造两个指纹投影；`:587` 只拒绝不在 allowed 列表内的键。因此解析器只需允许并读取流的 mode，两个投影也只在流方法中加入这个键，所有 unary 的规范内容、descriptor digest、两个指纹及持久发布引用均可保持。旧解析器按未知字段拒绝流，不会误解为 unary。验证只在 descriptor/route 边界一次，codegen 使用 validated method，不设新契约种类或解码回退。

生成器须证明纯 unary 的契约/Payload 产物不需要重新生成；流方法的 mode 同时进入 ABI/signature，旧 unary 方法不可误匹配为流。另一个独立代价必须披露：当前 TypeScript 生成文件嵌入完整 transport（`core/foundation/c2-codegen/src/targets.rs:776`），即使其契约没变，也要随 IPC/ABI 更新这段传输产物，不能声称旧 Node 生成文件原样就能用新协议。

## 七、线上记录与版本

沿永久 25 字节前导及现有帧外壳，IPC 16 的业务帧用 RPC_RECORD，流标识直接使用 request_id。完整 unary 的两个方向分别只发 OPEN、CALL_END；小输入/结果内联，大值用原 SHM/分片传输选择。分片是同一条值的延续，到齐才视为该值记录完整，不为了流控先发无数据 OPEN 再等一次往返。

| 记录 | 本期内容及边界 |
| --- | --- |
| OPEN | 完整 RouteCallIdentity、method index、一次输入的存储表示和内容。mode 从已验证契约取得，流开始时隐含一条需求，不在线上传。 |
| ITEM_BEGIN | 流输出的 total_bytes；完整接收方异步预约整条并授予整条，转发方授予真实有限复制窗口。拒绝只生成一次 CALL_END。 |
| ITEM_DATA | 本条目的 inline 数据或 SHM carrier；位置、已收字节及条目次序由调用记录推导。每方向仅一个未完成条目。 |
| CREDIT | IPC 接收方对真实预约的授予/返还及下一条需求；连接核心原子更新本流和连接账。HTTP 不传此记录。 |
| CANCEL | 对现有调用记录提交取消意图，不能表示资源已经结束或副作用未发生。 |
| CALL_END | 唯一 status（Success、Failed、NotExecuted）；服务端流另带最终完整条目数，unary 不带，同帧带结果。资源执行和清理已结束，必要数据仍按 writer 顺序先交付。 |
| RELEASE | 既有连接级 loan_id 释放，独立于调用终态，仍可迟到。 |

业务 record kind 由 c2-wire 的单一 codec 定义，存储标志只在载荷记录有效，错误沿规范 C2E1。inline/unary 的长度直接来自帧，分片首部声明总长；分片隐含位置只有一份计数。接收边界一次验证组合、长度、连续接收状态及真实预约，内部信任受检对象。Node 的 u64 保持 bigint，到实际 Buffer/TypedArray 分配边界再检查转换；不加整个流累计 4 GiB 上限。

| 边界 | 提案 | 理由 |
| --- | --- | --- |
| IPC | 15 → 16 | 新流记录及统一请求生命周期，不保留旧业务解码；身份先于固定前导，精确版本拒绝。 |
| SHM | 保持 4 | 不改变共享字节布局或 loan 描述符；池短锁是进程内所有权调整。 |
| C/FFI ABI | 7 → 8 | native endpoint context 新增持续读/流事件与预算投影，结构/符号同批切换，不留旧 loader。 |
| relay mesh | 保持 7 | `core/transport/c2-http/src/relay/peer.rs:23` 仅传播现有身份、两个哈希、max_payload_size、route_uid 等；本期没有新增消息字段，流只改变哈希值。 |
| portable contract | 保持 v2 | 当前单一文法增加流方法 mode，unary 规范内容不改。 |

当前常量见 `core/protocol/c2-wire/src/handshake.rs:11`、`core/foundation/c2-mem/src/alloc/buddy.rs:7`、`core/foundation/c2-mem-ffi/src/lib.rs:12`、`core/transport/c2-http/src/relay/peer.rs:7`。HTTP 流响应使用明确的 record media type/版本和逐条长度，普通 unary HTTP 仍为一次结果；调用一旦派发不因不支持流而换路径重试。协议/ABI 切换须让旧进程排空，精确候选通过适用 CI；不强行中断旧调用来满足部署时间。

## 八、与派发后查询和去重的交叉

本期只在规范错误中保留调用定位符（server instance、原连接观察身份、request_id）及最后完整交付位置，不提供不能查询/跨重连的公开标识 API。调用记录只保留待交付条目、真实清理及原终态；已经消费的数据不为将来查询无限留存，hold 依自己的本地所有者释放。

后续查询/去重另需 ADR：稳定逻辑调用标识、身份、请求指纹、持久保留与过期事实。同标识重新提交不能重新执行生成器；“查终态”和“重放全部历史条目”是不同承诺，查无记录不能当作从未执行。无限流须依领域检查点，不默认保留全部历史；本期不预造日志、resume 或查询字段。

未派发失败可 pre_dispatch；派发后断连而未取得完整终态则 DispatchUncertain，已交付条目仍有效；已经取得的确定终态不能被后续 EOF 覆盖。最后交付 k 条不能证明资源只执行 k 步，不自动重试、重放或对冲有副作用的调用。

## 九、验收与基准

本节是未来实施出口，本次只做静态核对。Rust 的纯状态机验证预约/需求/取消/终态，真实 IPC 验证开始顺序、分片轮转、借出及 shutdown；不用源码字符串测试替代运行行为。调用数/窗口检查复用内部状态及现有通知，确定性交错使用测试内屏障，不增加生产探针。

B2 测试暂停一次复制，同时使首片预约、unary、释放和取消推进；GC/take 不提前析构 backing；早到写阻挡后到读；拒绝后等待超过 chunk_assembler_timeout 再发续片、随后 Node 调用，原请求仍只有一个终态。并发 writer 验证请求号高水位的线上次序。

执行测试打开多于 worker 数的流，全部停在 yield 或预算等待，路由许可/执行 worker 占用应为 0，短 unary 可完成；调用槽、借用输入继续记账。跨两个实际 worker 的 Python 生成器在 yield 前设置 ContextVar、进入 decimal.localcontext，下一步和 finally 仍见原值/可 reset 原 token，worker 外 context 不被污染；创建/next/close 均经同一 ctx。覆盖零/一/多条、异常、finally 异常、Python 3.10/3.14t、Vec 移动、GIL 等待释放及同进程对象直传/有限截止时间拒绝。

直接 IPC 排完整矩阵：Python/Rust 资源 × Rust/Python/Node 客户端 × materialized/hold，覆盖 SHM 可用/不可用、inline、大 SHM、分片和文件重组。relay 单跳选 Python→Rust、Node→Python，双跳选 Rust→Python，并在这些代表组合里覆盖普通结果/hold、错误和慢读。Node 验证一个持续 reader、并发两流加 unary；纯 unary 的原串行行为改变单独验证并写发行说明。

| 异常/边界 | 必须证明 |
| --- | --- |
| 交付 k 条后停止迭代 | 直接 IPC 最多再有一条已授权的未交付结果；句柄丢弃/with 提前退出发 CANCEL 且不等待；Rust ? 提前退出也归还实际调用槽。 |
| 多 hold 用满 B | 下一条等待而不报使用/容量错误，释放一份后恢复；有限截止时间如实返回，取消清理不受数据额度阻塞。 |
| 完全停读、SIGSTOP、不发释放 | 没有无界 C-Two 队列，借出不按时间复用；其他已获预约的调用推进；物理堵塞及关闭未完成如实报告。 |
| 取消/自然结束/EOF 竞争 | 正在运行的步照常完成，唯一终态，完整结果不被撤回；断连未知部分不重放。 |
| shutdown/注销与无限流 | 当前步结果先交付，再以 CallCancelled 结束，原 receipt 在清理后完成；卡住的步/清理或停读的消费者让关闭如实保持未完成；同路由 unary 照常跑完。 |
| relay 慢读、中止、Body/Bytes clone、重连 | Body 拉取次数与预产步数受实际缓冲限制；完整大 SHM 不被人为拆 loan；最后实际 owner 才退款，取消只影响本调用。 |

HTTP 验收须真实慢读而非只看源码：响应远大于 F，relay 的本地复制峰值按窗口/库缓冲增长，不构造完整结果 Vec；whole SHM 的源 backing/借出保留 N 单列，不冒充 F。逐跳记录 Body 拉取、源生成步数和消费者交付数，证明最大提前量及所有缓冲确有边界。未知长度、错误 Body、Content-Length 矛盾、B 临时不足、单条大于 B、源实际结束、bytes 跨 Body 析构均覆盖；默认 pure unary 保持单次 OPEN/CALL_END，无新 CREDIT 往返。

配对基准对照本次源提交的整份 unary、多次 unary 批次及同内容服务端流：首条延迟、逐条 p50/p95/p99、终态延迟、吞吐、CPU、复制及预算/RSS，另测大流与短 unary 混合的尾延迟。负载沿现有代表类，再加 B/F/SHM 边界、空条目、大于窗口条目、FastDB 连续数组/混合字段与 Python-only codec；不以某种通常大小作产品上限。现有 unary 中位延迟、p95 或吞吐若退化超过 5% 且超出噪声，先定位并交维护者裁决；更深预取须有测量依据。

exact implementation candidate 必须通过 GitHub Actions 的 Linux、macOS、Windows x64 及现有 Windows ARM64 原生 gate：Rust core/公开 SDK、Python 全套和现有 Python/Node portable 门禁、代表 relay 与文档检查。保留 3.10/3.14t 的适用矩阵；记录提交、run URL、OS/架构、工具链、配置、二进制 SHA-256 和原始基准。focused green、文档提交或只读审查都不等于跨平台接受。

## 十、执行计划

阶段只是排序：不允许临时协议或质量降级，每一块合入时都要达到可发布的质量。进度只记在[待办](backlog.md)。

共同要求：

- 各块在自己的检出里实施。参考文档、CONTEXT.md、AGENTS.md 与发行说明在合入时更新，实施方在报告里列出需要改的地方。
- Linux、macOS 与 Windows 同等对待，不先做 Unix 再补 Windows。
- 先写或改聚焦测试；删除的测试逐条归类：机制已删、别处已覆盖、仍然有效（改写成新测试）。
- 0.x 规则：一次切干净，不留别名与兼容路径；校验只在边界做一次；不为没发生过的情形设防。复制、释放、移动的顺序不变；直接 IPC 在 relay 未设或不可用时仍完整可用。

| 块 | 内容 | 写入范围 | 依赖 | 退出条件 |
| --- | --- | --- | --- | --- |
| 1 重组所有权与短锁 | 5.1 第一段；C03、R14 中属于重组存储的部分 | c2-mem（`retention.rs` 除外）、c2-wire 的 chunk 模块，以及接口变化必需的调用点 | 无 | 复制不持公共池锁或 shard 锁；GC、take 与结算经同一条目状态定终态，析构在锁外；finished/trim 后仍计真实容量，文件重组按原 owner 退款；测试内屏障暂停一次复制时，其他调用的首片预约与释放照常推进。 |
| 2 契约 mode | 第六节的契约部分 | c2-contract；c2-codegen 中契约与 Payload 产物的部分；夹具 | 无 | v2 接受流方法的 `"mode": "server_stream"`，拒绝其他取值；两个投影只在流方法变化；现有 unary 契约的规范内容、digest、指纹、发布引用与 Payload 产物逐字节不变；codegen 在生成边界对流方法报明确错误，块 6、7 实现各目标的生成时删掉这条。 |
| 3 字节预算 B | 3.1 末三段、3.8 第三段；[HTTP 内存预算提案](2026-10-06-http-memory-budget.md) | c2-config、c2-mem 的 `retention.rs`、c2-core 的调用保留与 memory、c2-http 的载荷与转发、cli 的 relay 参数、Python 与 Rust SDK 的配置投影 | 无 | 只有一项设置，取代 retained-input，不留别名；缺省 16 GiB 与完整 u64 范围不变；临时不足时等待，单条大于 B 才拒绝；最后真实引用结束才退款；`memory_stats()` 形状不变；Python 入口等待预算时释放 GIL。relay 从上游 IPC 取得的响应要先预约再分配，需要块 8 的逐片读取入口，归块 8。 |
| 4 服务端调用记录与同步预约 | 5.1 后两段、5.3 | c2-server、c2-wire 的 chunk 模块、c2-config 的分片键 | 1、3 | dispatcher 发布是短的同步边界，接收循环不再等 dispatcher 的异步锁（包括现有 unary）；首片在 reader 发布调用记录与准入凭证；删除 `chunk_ordering.rs` 与 `server.rs` 的 Pending 分支；`max_total_chunks` 改名 `max_inflight_chunk_frames`；仍是 IPC 15，现有测试全过。 |
| 5 IPC 16 调用记录 | 5.2、第七节；3.1 的额度与需求、3.5 的终态与取消在传输层的部分 | c2-wire 的帧与连接核心、c2-server、c2-ipc、c2-error | 4 | 七种记录，旧业务解码删除；请求号在写端第一次提交时分配，迟到续片按高水位识别；unary 每方向一帧；`CallCancelled=718`；用 Rust 测试回调跑通服务端流的建立、按步推进、需求、取消与终态。 |
| 6 资源与 SDK | 第二节、3.3 至 3.6、第六节的 Python 与 Rust | c2-core 的 host、client、session；Python 的 native、proxy、server、transfer 与 CRM 元数据；sdk/rust；c2-codegen 的 Rust 与 Python 目标 | 2、3、5 | Python 与 Rust 资源按步推进，路由许可只在步内；contextvars；hold；句柄回收发 CANCEL；显式 close；shutdown 与注销在步之间结束流；同进程对象直传，拒绝有限截止时间。 |
| 7 Node | 第六节的 Node 部分、第四节第 7 条 | c2-mem-ffi 的 endpoint 与 C 头文件、TypeScript 绑定、c2-codegen 的 TypeScript 目标与 transport | 5，可与块 6 并行 | ABI 8；每条物理连接一个持续 reader；去掉 callChain；`C2ResponseStream`；fetch 增量读取；公开错误区分 pre_dispatch 与派发不确定。 |
| 8 relay | 3.4 末段、3.8 | c2-http 的 client、relay 与 payload；c2-ipc 的逐片响应读取入口；cli | 3、5，与块 6、7 协同 | 请求一次发完，响应逐条发送；不组成完整响应 Vec，上游响应每片先取得 B 的预约再复制；whole SHM 按片复制；取消只影响本调用；mesh 7 的消息不变；真实慢读测出预产步数的上限。 |
| 9 交叉验收 | 第九节 | 三语言测试、CI、参考文档、验收记录 | 全部 | 同一候选在 Linux、macOS、Windows x64 与 ARM64 的 CI 全绿；直接 IPC 与 relay 矩阵通过；[闲置驱逐审查](../acceptance/2026-10-10-idle-eviction-regression-audit.md)的六道门禁在 Python、Rust SDK 与生成的 Node 客户端上补齐，包括持久直连、relay-local 与 HTTP 全链和真实断流；配对基准对照实施前的 main，过第十二节第 7 项的门槛。 |

块 5 分两步实施（2026-11-05 补记）。生成的 TypeScript 客户端内嵌自己的 IPC 编解码（`core/foundation/c2-codegen/assets/typescript_transport.ts:1106` 的握手版本仍是 15），只改 Rust 一侧会让 Node 在块 7 合入之前连不上，违反上面每块合入时可发布的要求。5a 定下七种记录的字节布局，实现 OPEN、CALL_END 与 RELEASE、写端分配请求号、高水位、准入拒绝报派发前，以及块 5 的顺带项，把 Rust 与生成的 TypeScript 传输一起切到 IPC 16；TypeScript 保留现有的 callChain 与逐调用读取，由块 7 替换。写入范围在原表之外加上 c2-http 里 relay 对 IPC 准入错误的映射，以及 c2-codegen 的 TypeScript 传输及其测试。5b 实现 ITEM_BEGIN、ITEM_DATA、CREDIT、CANCEL 与 `CallCancelled=718`，用 Rust 测试回调跑通服务端流的建立、按步推进、需求、取消与终态，写入范围同原表。

块 6 也分两步实施（2026-11-05 补记）。块 6 跨 Rust Core、Rust SDK、Python 与两个代码生成目标，一次做完的差异比块 5b 还大，难以审查。6a 做 Rust Core 与 Rust SDK：Runtime 注册与推进流路由，服务端流条目改用所在 Runtime 的载荷缓冲预算，shutdown 与注销结束流，客户端的流句柄、截止时间、Drop 与 close，经 relay 的流方法在块 8 之前于派发前拒绝，以及代码生成的 Rust 目标；AGENTS.md 里 shutdown 的承诺随 6a 修改。6b 做 Python：`@cc.stream`、`Stream[T]`、hold、contextvars、借用输入、同进程对象直传与有限截止时间的拒绝、代码生成的 Python 目标，以及 Python 与 Rust 资源、客户端交叉的直接 IPC 矩阵。下面列给块 6 的顺带项按所在的层分开：有限调用名额的释放顺序与服务端流的预算归 6a，其余归 6b。两步合起来的退出条件与原表相同。

关键路径为块 1、3 → 4 → 5 → 6、7 → 8 → 9，块 1、2、3 可以同时开始。第一阶段是块 1 至 4，仍为 IPC 15，对外只有两项设置改名；第二阶段是块 5 至 8，IPC 16 与 ABI 8 同批切换；第三阶段是块 9。每个阶段结束时，整合分支跑一次全平台 CI。块 6 与块 7 都改 `c2-codegen/src/targets.rs`，各自只动本目标的生成函数，共用的方法遍历与 mode 处理由块 2 定下。

顺带做掉同一块里的小待办：

- 块 1：c2-mem 与 c2-local 的 `AtomicU64::fetch_update` 改为 `try_update`，先确认它在 Rust 1.95 上可用。
- 块 3：c2-http 连接池里的同一处 `fetch_update`。
- 块 4：删掉 c2-server 构造时多余的一次响应池预热。
- 块 5：c2-ipc 判定零借出 ID 改用 c2-wire 的专用错误类型，不再匹配字段名 `"loan id"`；给已接纳调用的回复不再借用 `WriteRequest::Control` 的名字；资源进程在执行前的路由准入拒绝（例如路由过期）要报成派发前失败，现在它经资源错误的路径报成 HTTP 500（`core/transport/c2-server/src/server.rs:3179`、`:3338`）。
- 块 5b：5a 的代码、注释、生成的 TypeScript 传输与测试名把分片叫作 fragment（`FLAG_FRAGMENTED`、`FLAG_FIRST_FRAGMENT`、`FragmentPosition` 等），术语表定的是 chunk，并明确要求避免 fragment；统一改为 chunk 的说法，线上的位值不变。
- 块 6：Python `register` 的 `input_lifetime` 写成 `Literal['materialized', 'borrowed']`，与服务端桥的私有别名 `_InputLifetime` 合成一处；服务端桥 `shutdown()` 的 5 秒缺省只在注册失败的回滚路径上用到（`sdk/python/src/c_two/transport/registry.py:529`），统一为 30 秒。建议一并处理：块 3 之后 `call_execution_snapshot()` 的 `max_retained_bytes`、`used_retained_bytes`、`peak_retained_bytes` 描述整份载荷缓冲预算，与 `memory_stats()['payload_buffer']` 重复，名字还是旧的 retained input（`core/foundation/c2-mem/src/retention.rs:416` 的注释承认了这一点）；字节只留在 `memory_stats()`，`call_execution_snapshot()` 只报有限调用的数量，`rejected_reservations` 先核实计的是哪一种拒绝，`registry.py` 里 `call_execution_snapshot` 的 docstring 同改。有限调用的名额在原生任务发布结果、唤醒调用方之后才释放（`core/runtime/c2-core/src/call_execution.rs` 的 `guard.finish(result)` 之后，0.7.1 起如此）：调用方一返回就发下一次调用时，在 `max_outstanding_calls` 用满的情况下可能被误拒，返回后立刻读快照也可能还看到这个名额；CI 上已有两次测试因此偶发失败（0.10.0 的 codegen 消费测试与第一阶段的 shutdown 测试，都只改了测试）。改为传输结束、发布结果之前先释放名额。块 5b 留下的一处：服务端流等存储额度时持有的条目复制，计入 c2-server 自建的一份默认大小的载荷缓冲预算（`stream_payload_budget`），不随 Runtime 的配置。6a 的处理与原设想不同，见第十一节 2026-11-05 的补记：服务端的流条目不再计入 B，这份自建预算已删除。
- 块 7：c2-mem-ffi 连接入口的 Rust 文档注释与 C 头文件对身份校验失败状态码的写法统一。Node 的 relay-aware 客户端与 Rust 一致，只发一次业务 POST，遇到过期路由报派发前错误、不再自动重发（现在仍会排除该 relay 后重发，`core/foundation/c2-codegen/assets/typescript_transport.ts:1074`）；FallbackDenied 保留首个 IPC 失败的类型化原因（`:1007`、`:2960` 仍丢失）。
- 块 8：`LocalEndpointNamespaceRequired` 的消息改为资源一方的本地端点命名空间。relay 现在先取得上游连接的租约、再等 B（`core/transport/c2-http/src/relay/router.rs:1937`、`:1966`），等待期间闲置回收不会收走这条连接；补上 B 等待与闲置驱逐、取消、续连交错的测试，必要时把预约移到租约之前，并在等待之后重新核对路由。

## 十一、与设计要求不一致之处

早期八问要求关闭预算内主动结束流，现行 AGENTS/Runtime 则要求 shutdown 只停新调用。维护者选定 (b)：shutdown 与注销让开着的流在步之间结束，当前步照常完成并交付，实际清理之后才报终态；预算超时仍不当作完成，也不强行中断当前步。AGENTS.md 的相应一句随块 6 改写。调研第 7 节的强停先例不足以代替这一产品裁决。

块 6a 的两处调整（2026-11-05 补记）。其一，服务端的流条目不再计入 B：等存储额度时只持有资源交出的条目，SHM 条目直接写进 SHM 分配（计 SHM 预算），inline 条目写每个分片时才从条目编码这一片（不超过 `chunk_size`，属于传输帧）。原设想让服务端的编码副本在 B 里预约，可同一个 Runtime 经真实 IPC 给自己开流时，客户端已为整条预约了 B，服务端再等同一份 B 就会互等，条目长于 B 的一半时必然卡住（独立审查发现，Host 用 B=7、一条 4 字节条目复现）；Rust 同进程正走真实 IPC，用户走得到。逐帧编码同时去掉了服务端整条的第二份副本。其二，IPC 16 收窄：一个 SHM 的 ITEM_DATA 必须装整条条目，部分 SHM 或同一条目里 SHM 与 inline 混用是协议错误。客户端要把整条 SHM 的复制移出接收循环、不持流的状态锁，部分 SHM 会要求逐调用的有序复制队列或拆分同一分配的写区；服务端本来只发整条 SHM，块 8 的 relay 也按整条 SHM carrier 分片读取，IPC 16 尚未发布，没有兼容负担。

块 6b 的一处取舍（2026-11-05 补记）。FastDB 0.2.4 的 Python 接口没有按区间读取载荷的正式入口，`Payload.binary_bytes()` 与 `Access.payload_bytes()` 都物化整条 bytes。Python 条目因此在它的推进里编码一次，得到条目自己持有的不可变 bytes，`write_range` 从这份 bytes 写进 SHM 分配或当前分片；它照 6a 的规则不计入 B，存到最后一片写出或流结束。在推进里编码也是必需的：生成器 yield 之后可能复用或改动同一个对象，必须在步内导出快照。同进程的流直接传对象，不编码。

块 7 的几处调整（2026-11-05 补记）。其一，ABI 8 没有照第七节的设想给 native endpoint context 加持续读与流事件，而是把 c2-core 的客户端整体投影给 Node：客户端域、端点、调用、本地缓冲与错误，共 28 个入口。连接、握手、唯一的 reader 与 writer、调用记录与借出都在 Rust 里，Node 不再读写帧；把验证过的连接转交给外部事件循环的入口与不带捕获上下文的端点解析随之删除。其二，“按请求号分派到有界调用记录”里的“有界”，按每条记录缓存的东西有界理解：交付槽一条，加最多一条已授权、尚未交付的条目，不限制记录的数量；没有截止时间的 unary 仍不占 `max_outstanding_calls`。其三，`requestShmThreshold` 与 `requestChunkSize` 属于客户端域那一个 Runtime，只在创建连接器时设定。其四，生成 transport 的 unary `call` 改收编码闭包，调用截止时间从编码之前算起，与流一致。其五，表格里块 7 的“fetch 增量读取”移出块 7，块 8 也不做（第十二节第 10 条）：生成的 TypeScript 经 HTTP 调用流方法时在派发前报 `UnsupportedCallMode`。

块 8 计划阶段的更正（2026-11-05 补记）。3.8 写了“多跳同样逐跳拉取”，但 relay 只把调用转给自己的本地路由（`core/transport/c2-http/src/relay/router.rs` 的 `validate_expected_crm_for_route` 只查本地路由），没有 relay 到 relay 的转发；客户端经 mesh 解析后直接找拥有路由的 relay。块 8 不新造多跳转发，那一句作废。另外，3.4 末段要求转发一跳把 whole SHM 的 carrier 保留到最后一片复制结束，这需要转发用的 IPC 客户端只有正窗口时就能收到 whole SHM 的 ITEM_DATA；复制只占窗口，SHM backing 仍计在服务端的 SHM 预算上。最终消费者的路径不变，仍先为整条预约 B、再授予整条的存储额度。

块 8a 的几处调整（2026-11-06 补记）。其一，终态记录是控制信息：服务端流的 IPC CALL_END，以及 HTTP 流的 CALL_END 与 FAILURE，接收方都不计入 B、也不为它等 B，长度上限由 IPC 帧的 u32 长度推出（`c2_wire::rpc::MAX_CALL_END_PAYLOAD_BYTES`，4294967282 字节），IPC 与 HTTP 共用；条目与 unary 经 HTTP 的响应体仍计入 B。原先客户端为终态预约 B，调用方的 hold 用满 B 时，一条 121 字节的 `CallCancelled` 终态读不进来，`close` 只能报派发不确定。其二，HTTP 流的条目改成 ITEM_BEGIN 加若干 ITEM_DATA：开头记录只给总长，数据分片各自带长度，Failed 的 CALL_END 可以跟在半条之后，客户端丢弃这半条，完整条目数只算完整的。最初的格式把整条长度写进条目头，资源在条目中途出错时 relay 无法在 HTTP 上交付真实终态，只能中止响应，把已结束的调用变成派发不确定（独立审查发现）。其三，服务端只要收到正的存储额度就可以发整条 SHM 条目，这一帧用掉已授予的全部额度；最终消费者照旧整条预约、整条授予，relay 每次只授予一个复制窗口，逐片复制、最后一片之后才结束读区间。其四，relay 先预约 B、再取上游连接的租约，取得后重新核对路由，等 B 期间不占连接；两个方向的最小发送区与输入在派发前一次原子预约。其五，经 relay 的慢读实测（macOS 本机回环 TCP、一跳、B=256 KiB、片宽 4 KiB）：64 字节条目最多提前 5179 步，128 KiB 条目 10 步，空条目在测试的 20 万条内一直推进到底。第十二节第 5 条预想的“多推进几步”只对大条目成立；提前量约等于 HTTP 与 TCP 缓冲能装下的记录数，按步数没有上界。

dispatcher 异步读锁不是 B2 才引入的隐患：现有 unary 接收循环已经在 `core/transport/c2-server/src/server.rs:2462`、`:2520` 等它，函数体在 `:3142`。第五节短发布修复同时覆盖现有 unary 和分片；只缩重组池锁不能证明控制持续推进。

维护者确认第一期 Node 只做客户端；现有 Node 只有客户端生成路径（`core/foundation/c2-codegen/src/targets.rs:761`、`:927`）。v2 保留不要求 unary 契约重发，但 Node 内嵌 transport 仍须随 IPC/ABI 更新，这是第六节给出代码证据的独立迁移代价。

块 5a 冻结线上布局时与本节原表有两处不同（2026-11-05 补记）：OPEN 不再带流的初始需求，因为第一期每条流固定从一条需求开始，mode 又来自已验证契约，这个字段能推导出来，unary 还恒为 0；unary 的 CALL_END 不带完整条目数，它对 unary 恒为 0，接收方按活动调用记录里的 mode 选布局。字节布局、标志位与请求号规则见 [IPC 握手与调用记录](../reference/ipc-handshake.md#call-records-version-16)。

## 十二、维护者的决定

2026-11-04 维护者按推荐批准：

1. 第一期只做服务端流，按步推进；async unary、客户端流与双向流随后；Node 只做客户端。
2. 第四节的顺序条文，包括 Node 去掉 callChain 后、不等待就连发的 unary 会并发；发行说明写明。
3. 服务端结束流选 (b)，不新增 `cancel_streams()`：shutdown 与注销让开着的流在步之间结束，见 3.6。注销的部分由 (b) 推出，维护者同日确认。
4. B 只有一项，缺省 16 GiB，沿原范围并扩到 HTTP 缓冲与流的本地副本；临时不足时等待；调用方的多个 hold 用满 B 时，下一条一直等到释放、截止时间或取消。
5. relay 靠逐跳的有限缓冲背压，经 relay 的生成器可能多推进几步；以真实慢读测出上限，不承诺成最终消费者的精确确认。
6. IPC 16、ABI 8，SHM 4、mesh 7 与契约 v2 不变；Node 生成的传输产物要随之重新生成；同进程的流暂不接受有限截止时间。
7. 现有 unary 的中位延迟、p95 或吞吐退化超过 5% 且超出噪声，先定位再交维护者裁决；exact candidate 过第九节的全平台门禁。更深的预取、公开的窗口遥测与持久查询，另有证据后再定。

2026-11-05 维护者为块 8 另定三项：

8. relay 关闭时照 Runtime shutdown 的规则结束经它转发、还开着的流：正在运行的那一步照常完成，已产出的条目照常转发，清理之后以写明 relay 关闭的 `CallCancelled` 结束；unary 照旧等它完成。IPC 16 的 CANCEL 记录因此带上取消起因，区分客户端放弃与生命周期结束。
9. 经 relay 的取消用按调用的令牌：业务 POST 的响应在正文之前给出随机、不可猜的令牌，`DELETE /_calls/{token}` 只提交 CANCEL，原响应继续可读，`close` 读到真实终态与完整条目数；令牌只在这次转发存在期间有效，不提供查询、恢复或重放。
10. 块 8 只做 relay 与 Rust、Python 客户端；生成的 TypeScript 经 relay 的流，随 TypeScript 运行时独立成包那一批再做，免得先手写一遍再重构，第九节里 Node 经 relay 的流相应顺延（2026-11-05 维护者同意）。

另外统一了 `close` 的缺省观察预算（3.5）。新术语已写进 [CONTEXT.md](../../CONTEXT.md)；全文既有 Runtime、Host、hold、借出、租约、停用、对端结束、预算和截止时间沿原义。

本稿尚未运行验证：跨 worker 的 native/GIL/context 行为、增量编码在容量等待时的线程占用、hyper/可注入 fetch/代理的真实缓冲及预产范围、性能和三平台证据。v2 的投影/规范化及 hyper 的缓冲门已静态核对，不能代替上述实验；持久结果查询/去重仍待独立调研与 ADR。
