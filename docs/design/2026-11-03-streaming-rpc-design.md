# 流式 RPC 设计

状态：供 Host 独立审查及用户裁决的设计稿，未实施。任务日期为 2026-11-03；源码依据为 `645f06631450a5aa71a4d241757e98406c16c410`。本次只新增本文，不改变代码、待办、术语表或现行参考文档，不构建、不运行服务。实施时各块必须同步所属参考文档；这里的接口、配置和版本号均为提案，不能当成已有能力。

选择第一期实现服务端流：一次输入，按消费需求返回零个或多个条目。资源使用同步生成器或迭代器，Rust 按一次推进调度，等待需求、额度和 I/O 时不占执行线程，路由访问许可在每次推进后释放。公开 async unary 不必先做；客户端流、双向流需要可挂起的处理函数，排在共享异步执行基础之后。第一期完成直接 IPC、同进程和经 relay 的端到端证明之后，才达到路线图中 Streaming RPC 的完成标准；只有 IPC 通过的阶段属于中间交付。

本稿同时决定两级窗口、逐条目的所有权、请求生命周期及 B2 的结构调整，并纳入 relay 响应整体物化和 HTTP 载荷预算。沿用一套 Rust 状态机、一套字节 reservation、一套路由调度器；不增加重放、恢复流、持久消息日志或第二套 Python 调度状态。下面标为“现状”的内容引用本次源码；业界事实仅使用三份已提供的调研记录，其版本与未核实范围保持原记录的边界。“推论”表示由证据得到、尚未运行证明的判断；“选择”表示拟实现的契约。

## 一、现状与设计依据

先读资料为 [待办](backlog.md)第 1、2 项及“待用户决定”、[单一属主评估](2026-10-31-single-owner-release-evaluation.md)的八问、[协议批设计](2026-11-01-protocol-batch-design.md)第三、四、九节、[Runtime 生命周期](../reference/runtime-lifecycle.md)、[内存策略](../reference/memory-policy.md)、[载荷生命周期](../reference/payload-lifetime.md)、[IPC 握手](../reference/ipc-handshake.md)、[契约与路由](../reference/contracts-and-routing.md)、[配置](../reference/configuration.md)、[HTTP 预算提案](2026-10-06-http-memory-budget.md)及[路线图](roadmap.zh-CN.md)。旧设计中的源码行号不作为本稿的当前证据。

| 现状 | 本次源码证据 | 对设计的约束 |
| --- | --- | --- |
| 帧已有 `u64 request_id`、`u32 flags` 和长度，尚无用户可见条目序号、额度、方向终态 | `core/protocol/c2-wire/src/frame.rs:14`、`:20`、`:87`；`core/protocol/c2-wire/src/flags.rs:1` | 流复用请求号，不再加一个等价的流号；现有分片不能当作业务条目。 |
| 连接核心不做 I/O，区分业务、控制、释放；停用停止新的发布，只有读端对端结束才能清理未释放的借出 | `core/protocol/c2-wire/src/connection/mod.rs:279`、`:404`、`:411`、`:434`、`:465` | 扩展同一个核心；已接纳调用的数据必须与新调用准入分开。 |
| Rust IPC 已有独立接收任务，pending 仍以一次响应的 oneshot 为单位 | `core/transport/c2-ipc/src/client.rs:1131`、`:1143`、`:1162`、`:2855` | 把 pending 的结果投影扩为有界条目接收和唯一调用终态，继续独立结算借出。 |
| 执行先等路由许可、全局执行许可，之后在 `spawn_blocking` 中复制请求并调用资源；许可覆盖整个 unary 回调 | `core/transport/c2-server/src/server.rs:3067`、`:3077`、`:3090`、`:3116` | 生成器不能被整个交给一个长驻 worker；须把资源推进与网络等待分开。 |
| 路由排队按 ticket 管读写顺序；pending、worker 和读写占用现在随同一个 guard 结束 | `core/transport/c2-server/src/scheduler.rs:209`、`:395`、`:427`、`:588`、`:623` | 保留开始顺序，拆开调用存续记账和每步访问许可。 |
| 分片接收循环先拿帧处理许可和排序闸门；首片任务稍后才预约路由 | `core/transport/c2-server/src/server.rs:2338`、`:2365`、`:2411`、`:3791`、`:3837` | B2 要把预约放回接收边界，且不能阻塞控制读取。 |
| dispatcher 查找本身还会等异步读锁；这不是重组池锁 | `core/transport/c2-server/src/server.rs:3132`、`:3142`、`:3192` | 仅缩小池锁不足以证明接收循环不会等待；须一并缩短路由发布边界。 |
| 重组复制持池读锁，插入要池写锁；registry 的 feed 又持 shard 锁执行复制 | `core/protocol/c2-wire/src/chunk/backing.rs:238`、`:259`、`:274`；`core/protocol/c2-wire/src/chunk/registry.rs:285`、`:317`、`:367` | 复制和释放必须依靠实际 backing 所有权，离开公共池锁和 registry shard 锁。 |
| 结算只在仍持有的请求状态上赢一次回复；GC 超时后可删除整项 | `core/protocol/c2-wire/src/chunk/registry.rs:155`、`:186`、`:435`、`:471` | 时间不能证明今后没有续片；需要连接级请求生命周期。 |
| Core 资源调用边界和 Python 桥均只交付一个结果，PyO3 移动输入 `Vec` | `core/runtime/c2-core/src/host.rs:38`、`:1079`；`sdk/python/src/c_two/transport/server/native.py:434`、`:467`；`sdk/python/native/src/core_ffi.rs:439` | 增加逐步推进的私有桥；输入副本和 Python 对象由真实所有者保留。 |
| 生成的 Rust 已有资源 trait；TypeScript 生成客户端 `Promise` 方法，传输用 `callChain` 串行调用，并严格匹配当前 request id | `core/foundation/c2-codegen/src/targets.rs:122`、`:349`、`:761`、`:927`；`core/foundation/c2-codegen/assets/typescript_transport.ts:369`、`:612`、`:641` | Node 要有持续读取和按请求分派，长流不能卡住同连接 unary；Node Host 不是当前已有的资源桥。 |
| relay 请求有 sized-stream 路径；响应仍先变成完整字节再包装 Body，客户端接收仍聚合 `Vec` | `core/transport/c2-http/src/relay/router.rs:1669`、`:1707`、`:1787`；`core/transport/c2-http/src/payload.rs:76` | 必须改上游接收接口和 Body 的所有权，不能只把现有 Vec 分段发送。 |
| v2 方法的返回仅有 `none/payload`，ABI 和 signature 的投影没有流的形态 | `core/foundation/c2-contract/src/descriptor.rs:272`、`:408`、`:482` | 流形态须进入两个指纹；旧描述不能被隐式解释成新描述。 |

业界依据按用途引用：[流式调研](../research/2026-11-03-streaming-rpc-survey.md)第 1 节说明 Python 同步栈把整个生成器放在线程池的代价，第 2、3、4、5、6 节分别覆盖消费驱动流控、半关闭、终态错误、顺序和分片轮转，第 7、8、9 节覆盖排空、逐跳背压和重试。[借出调研](../research/2026-11-01-shared-memory-lending-survey.md)第 1、2、5 节说明没有释放或 OS 结束证明不能复用借出、单方上限不等于共享池预留、映射与连接的生命周期不同。[关闭调研](../research/2026-11-02-in-flight-calls-at-close-survey.md)第 1、4、5 节区分 RPC 已终止、handler 仍运行、进程退出阻塞和结果是否可知。本文不把调研中的源码推论改写成已复现实验。

## 二、第一期的执行模型及 async 的顺序

### 2.1 一次推进是一个路由操作

客户端创建流就表达了取得第一条结果的需求；随后每次迭代最多表达一条新需求。Rust 在有需求、可发送窗口及初始本地缓冲准入时，才预约生成器的下一步。服务端首次操作包括建立迭代器并执行第一次 `next()`；后续每次只执行到一个 `yield`、结束或异常。返回的领域对象在许可内完成必要的快照或序列化，确保释放路由许可后，其他方法不能改变正在发送的条目。未知的结果尺寸只能在产出后确定；许可内只允许非阻塞地增加 reservation，失败就报告该调用的容量错误，不能拿着 worker 或路由许可等待内存。网络发送和等下一条需求在许可之外。

一个已开始的流持有调用存续记录、原资源的强引用和一份调用准入凭证，直到调用真正终止；它不持有读写访问许可或执行 worker。流中的每一步进入现有路由调度队列末尾，并按该方法的 `@cc.read` / 默认写策略取得短的访问许可。一个流同一时刻只有一个步骤在排队或执行，不新建可无限堆积的“步队列”。`max_pending_requests` 和路由 `max_pending` 计一个尚未终止的调用；`max_execution_workers` 和路由 `max_workers` 只计正在执行的步骤。拆开的是记账寿命，不另建调度器。

上述服务器全局限额描述远程执行路径。同进程仍只使用共享路由容量，不加入 IPC server 的 pending/worker 池；同步本地 next 在调用方线程执行一步，Rust 只授予和释放同一个原生路由许可，不能为统一外形再串行化或复制 Python 对象。

现状的 `SchedulerGuard` 析构同时减少 pending 和 active worker（`core/transport/c2-server/src/scheduler.rs:623`）；因此需要把调用准入凭证与临时步骤许可分开。一个已经进入资源方法的流，后续步骤仍属于同一个调用：Host 排空、路由注销或连接停用只封新调用，不能让下一步通过“新调用准入”而被拒绝。凭证绑定原 route UID、资源和接纳事实；同名新注册不能接走旧流。清理步骤也使用这份凭证。还没进入资源方法的调用若被拒绝，返回该调用自己的确定错误，不能无回复丢弃。

等待没有线程：尚未有需求或额度时，Rust 仅登记等待者及唤醒源，不提交 `spawn_blocking`；有条件时才取 worker 并运行一步；一步返回后，worker 和路由许可先结束，再排发送。Python 生成器在两个 `next()` 之间由对象保存执行位置，不需要栈所在的线程常驻。这是本设计的推论，实施必须用真实 worker 占用、调度事件和 GIL 行为证明，不能引用 gRPC 的同步实现作为已经满足的证据。禁止把“循环 next、发送、等额度”整体装进一个 blocking closure。

第一期支持的是在 `yield` 处暂停的生产者。方法不得在 `next()` 内自行等客户端下一条输入、窗口或 C-Two 的读写；这些等待属于 Rust 驱动。计算一小步本身可以很久，不设“通常应该多久”的超时，也不声称能抢占任意资源代码。生成器在两次推进之间不得保留应用自己取得的路由独占锁；需要跨条目隔离的模拟应使用独立资源或应用快照。若一项科学操作要求整个长流作为不可分事务，本模型不能同时给予它全程独占和其他方法可执行的承诺。

### 2.2 async unary 不作前置，客户端流与双向流随后接入

第一期没有“等下一个输入”的 handler，额度等待又在 handler 之外，故公开 async unary 并非必要条件。Rust 内部异步 I/O、可唤醒的等待和持续 reader 是必要基础，不等于已经提供公开 async unary。Python 第一阶段提供同步 `Stream`；Rust 提供迭代接口；Node 客户端用 `AsyncIterable` 接收，Promise 等待不占执行线程。

客户端流及双向流以后复用同一请求生命周期、两个发送方向、条目格式和窗口；只增加输入多条的契约形态及对应处理函数桥，不先实现备用模式、空字段或恢复接口。它们必须先有可挂起的资源处理方式：Python 的协程或异步生成器、Rust 的 Future/Stream、Node 的 Promise/async iterator。不能让同步 `next(input_iterator)` 在线程池里等输入。更不能让一个拿着路由写许可的协程跨 `await receive/send`；接收、发送等待在访问许可之外，领域状态变更才预约一步。公开 async unary 可以共享这套异步桥，但单做 async unary 并不能解决流处理函数跨 await 持锁的问题。

建议顺序是本期共享连接、调度和预算基础 → 服务端流端到端验收 → 共享异步资源执行桥与 async unary → 客户端流和双向流。遥测中证明窗口和生命周期所必需的计数与本期同时交付；自适应策略、完整遥测产品不作前置。长时间模拟可以逐步产出，分批输出数组可以逐条传输；持续耦合中需要双向增量输入的情形要等后一期，不能称本期服务端流已覆盖它。

## 三、八个问题的选择

### 3.1 积压、两级窗口与预算

选择每个发送方向独立的流级字节窗口和物理连接级字节窗口。发出数据必须同时取得两级额度；SHM 按所指向的载荷字节计费，不能只按小描述符计费。窗口只限未结算的传输积压，不是条目大小、总条目数或整个流的总字节上限。所有帧仍受现有帧大小和每条目载荷限制约束；不对整个流设置默认时长或累计大小上限。

另有每流一个条目需求位：第一条随打开流授予，下一条在消费者请求时授予。它既防止空条目无限积压，也让生成器不会预先产出任意多结果；这是拉取接口的语义，不是第二套消息队列配置。传输帧和未结算借出的数量由既有分片帧处理容量约束，一个已发布的 SHM 分片直到实际收到释放记录才返还相应发送占用。控制状态按活动调用预留，CREDIT 按调用及方向合并，取消最多一项，终态最多一项；不为每次唤醒建立独立无界任务。

流级和连接级字节额度在接收方把字节移入已经准入的条目存储后返还。SHM 的复制结束先结束远端读区间、排释放，再排对应的字节 CREDIT；发送方收到真实释放之前仍保留借出和其占用。单独收到 CREDIT 不能回收借出，单独收到释放也不代表应用消费了条目。条目需求只有在应用再请求一条时才增加；hold 尚未释放时不能增加。把“复制完成”和“应用消费”分成这两个计量点，是已有复制后释放规则的直接后果；早期评估中“释放记录本身就是全部流控额度”的说法不足以覆盖 inline 与 hold。

两级窗口由接收方按真实可承诺的容量授予，发送方还受自己的本地预算限制。新连接或新流授予额度前先预约其窗口容量，不能把同一份剩余容量许诺给所有连接。多个异常对端可以耗尽共同预算；此时新调用在派发前被拒绝或按自己的截止时间等待，不能承诺在有限内存中总能再接纳一个健康连接。已获得独立窗口的其他连接、流和控制帧仍应推进。此处采用按实际准入预留的方式，不假定预先知道连接规模，理由见借出调研第 2 节。

采用下列最少的独立配置维度；具体命名供用户确认，均由 c2-config 解析、校验和冻结，SDK 只投影：

| 设置 | 作用域与选择 | 为什么不能从现有值推导 |
| --- | --- | --- |
| `stream_window_bytes`，记为 S | 一个方向的一条流最多未结算的传输字节；S 不大于 C | IPC backing 预算不约束消费速度；`chunk_size` 决定发送粒度，不能决定跨网络时延所需的在途容量。 |
| `stream_connection_window_bytes`，记为 C | 一个方向的一条物理连接的总窗口；每次授予都由所属预算域兑现 | 流数在运行中变化，不能以 S 乘一个假定并发数代替连接总量，也不能让一条连接承诺整个共享池。 |
| `owned_payload_budget_bytes`，记为 B | 每个 Runtime outgoing 域、Host 接收/结果域、relay 实例各一份：C-Two 实际拥有的序列化缓冲、本地条目副本和 HTTP Body 缓冲；跨路由、连接及重连共享 | 现有 SHM/file/reassembly 预算不覆盖这些堆缓冲；只限 retained input 又漏掉响应及 HTTP 发送窗口。 |

建议 S、C、B 均为显式的有限字节预算，省略按 0 处理，0 拒绝相应正字节准入。启用流之前必须配置正的 S、C、B；HTTP 采用 B 后也需显式配置其正字节预算。这是需用户批准的 0.x 配置迁移，不借“常见载荷”制定不可解释的默认值。S、C 是授予上限，不能在预算不足时先超授再记账；窗口可以小于一个条目。现有正 `chunk_size`、`max_frame_size`、远程 `remote_payload_chunk_size` 继续决定单次传输量，零预算不被解释成无限。

B 复用 Rust reservation 机制，取代现有只计 retained input 的字节配置及 HTTP 预算提案中另造的一组字节计费，不留下旧环境变量、字段别名或两份可重复扣费的账。`C2_CALL_MAX_OUTSTANDING` 继续提供 Runtime/relay 的调用数容量，但本期所有流都纳入它，包含没有调用截止时间的流；服务端还受现有 `max_pending_requests` 和路由 pending 容量约束。零字节条目仍占调用槽及一个需求位。现有 unlimited unary 是否统一纳入调用数限制属于后续配置收敛，不借流式改变它的范围。

每块实际存储只有一个携带 reservation 的所有者，slice/clone 共享它，最后一个真实引用结束才退款。窗口预约和实际存储从同一 reservation 转移，不能把一份字节同时当作可用窗口和另一个无限制本地队列。接收完整条目时，重组到最终本地副本的过渡必须预留同时存在的实际存储，或移动本地 Vec 避免第二份；不能因“复制即退款”在副本仍存活时少记账。SHM/file/reassembly 的现有物理预算仍按各自存储计费，B 覆盖的堆副本不是同一物理 backing 的重复分配。

序列化之前获取容量：FastDB 通过其正式尺寸/构建接口提供所需长度，C-Two 不解析嵌套布局；Python pickle 使用可失败、随实际增长预留的私有编码缓冲，超预算就结束本调用并报告确定的编码/容量错误。资源代码自己创建的科学数组不计入传输预算；C-Two 的序列化副本计入 B。不能先生成无限大的 C-Two bytes 再补记账。条目尺寸由现有 `max_payload_size` 约束，正字节超过 B 无法交付完整值时如实拒绝该条目；已经执行过资源步骤时不把它标成 retry-safe。

B 统计 live、reserved、peak、rejected；窗口统计 granted、in-flight、waiting；另记活动调用、执行步骤、条目、借出及实际清理。观测不保留 payload 或连接。应用自行保留的 materialized 对象、FastDB 内部领域分配、对端映射、内核及 HTTP 库内部缓冲不自动计入 B，B 不能被称为 RSS 上限。网络库本身的窗口与内部容量须在 relay 验收中单独记录。

### 3.2 双方互等

选择始终运行的唯一 reader 和 writer；业务消费停下时，reader 仍解析并处理释放、额度、半关闭、取消、终态。业务帧不能在 reader 中等待应用或资源许可；它们只能进入预先准入的存储和有界处理槽。没有容量承诺的帧在边界拒绝该调用，解析仍可信时继续读取其他调用。

writer 在完整帧之间优先处理释放、额度和取消，再从可写调用中轮转数据；终态不得越过本调用已承诺交付的数据。控制免业务窗口，但不免真实存储和 OS 传输限制。OS 写到半帧后被停读对端堵住时，任何控制帧都不能穿过这半帧；窗口和小分片只能限制其影响，不能保证向一个完全不读取物理连接的对端发送取消成功。

未来双向流不承诺解除应用的逻辑互等。如果两端都先发送无限数据、再开始接收，两边窗口终会耗尽；需要并行收发任务或明确的领域轮次。共享异步桥使这些等待不占执行线程、也不持路由许可。第一期的单输入已半关闭，不引入客户端持续写输入的等待环。事实依据为流式调研第 2、3 节对控制读取和双向方向独立的说明。

### 3.3 执行线程与路由锁

采用第二节的一次推进模型。等需求、等字节、等帧处理容量、等路由 ticket、等执行 worker 均在 Rust 异步驱动中；只在条件具备后进入资源语言。一次推进完成快照后，先结束访问许可和 worker，再等待发送。不在生成器内部注入阻塞的 `send()`，不让 Python 桥管理额度。

取消时的 `close()` / 析构清理可能运行应用 `finally`，属于同一路由上的一次清理步骤；不能在网络 reader、语言外壳的 Drop 或持连接核心锁时执行。清理不需要条目需求、业务字节额度或新的调用槽，只等待原调用的路由步骤许可和执行容量，否则慢消费者会把自身取消也锁死。Python 生成器不能重入；建立、next、close 使用同一个 Rust 调用记录串行驱动，允许落在不同 blocking worker 上。资源若需要线程亲和性，须通过明确的注册能力声明在边界拒绝，或由资源提供自身独立执行设施；C-Two 不声称能从任意函数体检测线程亲和性，本期也不为每条流创建专属线程。单步代码永不返回的情况仍占 worker，关闭必须如实显示未排空。

### 3.4 条目生命周期

请求和每条响应均保留 `传输 carrier → 本地副本 → 结束远端读区间及排释放 → 解码/资源调用/hold` 的顺序。SHM 分片复制完马上释放，不留到条目消费结束；PyO3 移动同一个拥有字节的 Vec，给 Python `memoryview`，不增加第二份快照。inline Vec 能移动就移动。FastDB 接收仍是 copy-backed，流式和 transport SHM 都不是直接构建 FastDB 最终 backing 的证据。

普通迭代返回独立 materialized 值；交付后它归应用保存，本地传输副本在解码完成时释放。`cc.hold(stream_method)` 则让每条返回现有 `HeldResult`：先使 FastDB 载荷所有者及 checked views 失效，再释放本地副本和 B。活动 raw export 导致原有释放拒绝时，保留副本及 reservation，不能伪造释放。一个流同一时刻只交付一个尚未释放的 hold；请求下一条而上一 hold 仍有效时，返回本地使用错误，流本身继续存在，释放后可以再取。不同流和已 materialize 的值可由应用并存。

初始单个输入在第一期可以选 registration 的 `input_lifetime='borrowed'`，但其有效期是整个生成器调用存续期间，直到自然结束、取消清理结束或真实失败后的清理结束；不是每次 yield。生成器局部变量可能保留输入，不能在第一次 next 返回时失效。它只保留计入 B 的本地副本，不保留传输租约。每步输出快照完成后不得再依赖可变的借用视图；需跨输入寿命保留的领域值由应用通过 FastDB 正式 materialize 接口取得。未来输入多条时再逐条规定借用输入的处理边界，不在本期创造跨 yield 原地输入协议。

### 3.5 方向终态、调用终态、错误与取消

每个调用记录两份发送方向状态：`Open → SendEnded`；另有调用状态 `Active → Cleaning → Terminal`。第一期输入只有一条或无参，随后输入方向 SendEnded；控制仍可双向发，输出可以继续很久。输出 SendEnded 仅表示不再产出条目；资源清理、输入载荷失效以及该调用的终态仍须完成。借出结算独立于这两套状态，调用终态不能回收尚未释放的块。

自然结束和资源异常都形成唯一 `CALL_END(status, final_item_count)`。OK 只有在迭代器结束及资源清理完成后才能生成；非 OK 放在已完整产出的条目之后。已交付条目仍有效，不能因后续失败改成未知，也不回滚其副作用；条目的科学意义、是否是可提交检查点由 CRM 的领域契约说明。未组成完整条目的尾部不交付。调用终态若已被完整解析，就先保存确定结果；之后物理连接结束不能覆盖它。

显式 `Stream.close()`、Python 流 context manager 的提前退出、Node `for await` 的提前 return 表达调用方的取消意图，发送一次 `CANCEL`。CANCEL 是请求，不是终态；运行中的 next 继续到安全暂停点，Rust 再调度生成器清理，实际结束后回 CALL_END。close 的等待可以有它自己的观察预算，超时返回未完成并保留原清理记录，不能宣告调用已取消。若资源已经自然完成，先形成的真实终态获胜；若清理失败，返回实际清理错误并保留发生过取消的原因。

取消清理成功的调用终态使用新增规范错误 `CallCancelled=718`，通过同一 C2E1 codec 和既有公开 CoreError 投影，不增加另一套 Python 异常层；当前注册表到 717 且没有取消码（`core/foundation/c2-error/src/lib.rs:48`）。它明确表示调用因显式取消结束，不能被当作从未执行、自然 OK 或可自动重试。close 观察此确定终态可报告已完成，后续 next 则报告取消；清理异常仍保留其真实规范错误。实施时更新 errors 参考和三语言映射；不借本期清理其他尚待裁决的错误码。

取消不撤回已经执行的步骤，不重放、不补发有副作用的条目。调用方不再要的数据可以在明确取消后丢弃，但仍按实际 carrier 结束读区间和释放。下一条已产生但尚未完整发送的结果是否被丢弃，终态必须用最终完整条目数说明；已经开始的物理帧必须写完或按真实 I/O 故障结束，不能损坏同连接其他调用的帧边界。

调用截止时间只结束调用方自己的等待，不自动发送 CANCEL，不结束生成器，不续期，也不重置每条 next 的计时。已经派发而没拿到调用终态时返回现有 `DispatchUncertain` / `transport_phase='dispatch_uncertain'`；尚未派发的失败才是 pre_dispatch。若调用方想停止工作，需明确 close，或通过领域方法让资源自然退出。语言对象的普通 GC/Drop 只能移交原生存续记录，不能把无人引用当成远端副作用可取消的证明；Python `with` 和 Node iterator return 的明确结构化退出另按上述契约处理。Rust 用 `#[must_use]` 提醒显式消费或 close，不能以 Drop 强杀流。

### 3.6 关闭与不会自行结束的流

Runtime.shutdown、Host shutdown、路由注销、连接停用和缓存淘汰都停止新调用，保留已派发调用的真实执行、结果和结算。已开始流的下一步、接收额度和清理沿原准入凭证继续，不能因 Runtime 已退休就拒绝旧调用的 continuation。停用仍停止新的 SHM 借出；旧流之后要发的数据改走已有的 owned inline 分片，已发布借出继续等释放。不能为继续流而偷偷绕过“停用不发新借出”的规则。

连接的物理关闭门必须同时确认活动调用为零、已接纳帧写出/释放队列结束、远端读区间为零以及借出已结算。不能因为流当前停在 yield、没有 loan，就通过 try_close 关闭它。活动请求记录是这个判断的权威，现有 driver 的 flight guard 改为同一份接纳 token 的投影，不再另保一份可能与流状态失配的计数；本地 hold 仍不延长物理连接。

选择不在关闭预算内自动取消流。预算只界定此次 shutdown 观察等待；到期返回 `completed=False`，原 ShutdownTransaction / RuntimeShutdownReceipt 继续观察同一个资源、活动流、排队帧和借出。`work_drained` 须等生成器清理及已接纳回复真实排空；`listener_closed` 和 `finished` 分别按实际事实报告，不能以发送过取消或下层 future 返回冒充完成。旧 SDK session 在消费完成 receipt 之前不替换；本地 hold 不阻止传输结算，但继续占其真实本地字节预算。

一条无限生成器可以这样结束：应用消费到自己决定的边界并显式 close；资源的领域停止条件使其返回；或对端真实结束后，执行方结束当前步骤、清理不能继续交付的流。若应用没有这样的动作、对端仍在且健康、资源也永不返回，那么 shutdown 可以永远未完成。这是有状态调用契约的必然代价，不能同时保证有限关闭和不放弃已派发工作。应用若需要限定进程退出时间，应在 shutdown 前显式停止自己的流；进程监督者的终止属于真实故障，客户端仍按未知部分报告，不称优雅完成。

真实对端结束、不可恢复 framing/I/O 故障与 shutdown 不同：未得到终态的调用方报告派发不确定；资源当前已进入的方法步继续结束，再清理剩余生成器。relay 的下游真实连接结束可以向上游传递交付已失败的取消请求，仍要保留转发记录到上游真实终态或故障，不把 HTTP handler 的退出当作资源已经结束。普通 unary 的运行方法照旧继续，不因这个请求被强制中断。只停读、SIGSTOP、观察超时、写错误都不能冒充读端对端结束，也不能据此复用借出。依据为关闭调研第 1、4 节及借出调研第 1、5 节。

### 3.7 大条目与公平分片

一条目可以大于 S、C；发送的是不超过当前两级额度及传输粒度的片段。接收端先在 ITEM_BEGIN 的长度边界检查现有 `max_payload_size`、重组限制和实际存储预算，再允许分片写入。SDK 的完整值接收模式先预约整个条目的最终存储，再逐片复制并释放；每片移入这份已预约存储后即可返字节额度，无须等整个条目交付才返。因此“大于窗口”不会造成等不到整项额度的互等。完整条目放不下当前物理/本地预算时报告容量错误，不靠流式绕过真实容量。

writer 用同一个可写调用队列做按片轮转，一轮每个调用最多一片，控制在片间推进。片长取 `min(剩余字节, 流额度, 连接额度, IPC chunk_size, 帧剩余容量)`；HTTP 再受 `remote_payload_chunk_size` 的发送批次约束。没有单独的优先级协议或写目标调参。大 SHM 条目也须按窗口发布小的 loan，不能用一条指向整块大分配的描述符绕过额度。停用后发送 inline 分片也遵守同一个轮转。

同流同方向第一期只允许一个未完成条目，分片按连续 offset 到达；不同调用之间可以任意交错。一个大条目的重组仍会消耗它预约的存储，也可能用完路由配置容量；公平性承诺针对仍有额度和执行容量的其他流。不能保证向完全停读的同一 OS 流推进。已配置的单片物理上限来自帧长度字段和现有传输配置；整个调用没有从“常见数组”推导的累计上限。业界依据为流式调研第 6 节的按帧轮转及大帧延迟控制帧的事实。

### 3.8 经 relay 的端到端背压及 HTTP 预算

第一期选择一个 HTTP/2 双向传输体承载一条流调用：请求体发送打开、初始单输入、数据方向半关闭，之后仍保持控制记录可写；响应体发送条目、方向结束和调用终态。这里的“输入方向结束”只结束业务输入，不等于 HTTP request END_STREAM。这样客户端停止消费后可以停止发条目需求、仍发 CREDIT、释放和 CANCEL，不需要额外控制 POST、WebSocket 或轮询。每个 SDK/relay 连接器必须证明 HTTP 请求和响应同时推进；Node 使用原生 Node HTTP/2 连接器，不能把浏览器 fetch 的可移植性当成已成立。

这是本稿显式增加的传输条件：现有 HTTP/1 unary 仍是有效的一元入口；新流式入口要求 HTTP/2，不能悄悄降为全量 HTTP/1 或换一条调用重试。中间代理必须允许双向体且不全量缓冲，并按部署所需处理其自身计时器；这些是部署能力边界，不新增 C-Two 固定流空闲超时。流式调研第 8 节记录 HTTP/2 流控逐跳、中间层不透传 WINDOW_UPDATE，故 C-Two 的应用条目需求沿本协议转发，字节窗口则每跳按真实缓冲准入授予。

relay 为选定的上游调用保留一份记录，绑定完整契约、route UID、server instance、上下游请求号及其 reservation。它不解释 FastDB，不跟随同名新路由迁移已派发流。只在下游有已预约发送空间时从上游取得下一片；复制一片到自身拥有的有限缓冲后立即释放该片上游租约。Body 持有本地字节所有者，交给 HTTP 库的 Bytes 切片共享 reservation，最后一份实际引用结束后才退款。慢读使 relay 的本地窗口耗尽，继而停止上游额度；不能在中间层把整个条目或整个流读完再等待下游。

HTTP/2 自身两级窗口负责网络库内部背压，C-Two 窗口负责 C-Two 拥有的排队字节及应用需求，两者处在不同边界，不能把其中一层等同于全部端到端流控。初始需求、后续需求、取消和终态都走同一 record codec；relay 只映射本跳请求号，不以一个 HTTP/2 stream id 取代 C-Two 调用身份。每跳积压受其预算限制，整条路径占用是各跳实际占用之和；不声称任意多跳都只有一个窗口大小的总内存。

一并修正 unary 的 relay 响应：新增原生、绑定实际所有者的逐片响应读取边界，使 relay 可转发一个条目的字节而无需得到完整 `ResponseData::into_owned_bytes()`。最终 SDK 仍可收完整 unary 值；relay 不是这个完整值的消费者。新的分片格式用于 unary 及流式的大条目，避免 relay 拿到指向整份巨型响应的单个 SHM carrier 再持有到慢 HTTP Body 结束。已完整在本地拥有的 inline 小响应可直接移动，但它的所有字节仍计 B；不能声称将一个已构造的完整 Vec 包成分块 Body 就消除了整体物化。

HTTP 请求、成功响应、错误、未知长度接收以及所有转发跳都在分配前预约 B，未知长度按实际片增长。完整值消费者的 N 字节存储与 relay 的有限窗口采用同一所有者类型、不同消费模式。未知长度 unary 请求仍须完整形成一个输入后才执行，沿现有明确单消息限制；不能借更换 Body 接口宣称资源已经支持增量输入。HTTP 提案中的“Body 保留上游整份租约”在本稿替换成每片复制即释放，避免慢 HTTP 消费延长上游整个 SHM 借出。

请求保留与响应进展不能抢同一份未分配余量：调用派发前预约其初始输入和至少一个响应传输片所需的 B；relay 同时预约两个方向的一片和控制记录，不能全部给待响应输入。整条目长度出现后再预约完整值；准入不足返回该调用的容量终态，已执行过的步骤保持确定的副作用，不标为 pre_dispatch。预算无法支持最小推进量的调用在派发前拒绝；不通过额外无限缓冲解决。

## 四、提交用户确认的顺序契约

以下条文适用于 unary 与流式共用的直接 IPC 连接；跨独立 HTTP 请求没有 IPC 到达顺序的承诺。

1. 同一物理连接的调用开始顺序，是其 OPEN / 首片在接收循环完成路由预约的顺序。每个新请求的这个预约在读取下一新请求之前完成，不等待资源方法、重组复制或执行容量；输入尚未齐全的早到写仍保留其排队位置。
2. 同一路由的 `exclusive` 按预约顺序开始；`read_parallel` 允许先到的读并行，但后到读不能越过已经排队的写；`parallel` 允许重叠执行，不保证开始或完成次序。不同物理连接之间没有“谁先发”的全局顺序，实际路由预约是唯一执行边界。
3. 流的第一次资源推进使用最初预约；后续每次推进和清理在条件具备后进入队尾。一次 yield 之后释放路由访问许可，后来 unary 可以在该流下一步之前执行。一个流不是覆盖整个生命周期的独占事务；第一步被预约得早，不承诺其所有副作用都先于后来调用。
4. 同流同方向按从 0 开始的条目序号交付，分片 offset 连续，条目不能乱序或重复交付。未来双向流的两个方向各自保序，方向之间的因果由应用协议定义；窗口额度不等于领域操作提交回执。
5. 完成顺序不由开始顺序决定。每个调用独立给出唯一终态；长流不会要求其他调用等它全部结束。资源异常排在其先前完整条目之后；已经取得的结果不会被别的调用、停用或随后断连改成未知。
6. 不恢复跨路由的连接级写屏障。跨路由因果由应用先等待前一项确定完成，或等待 CRM 明确声明“状态已提交”的某一条目，再发后一调用。只取得一个中间结果不能推断整个流已经完成；需要跨资源原子操作时由领域资源定义。后续结果查询也不自动补上这种因果。

条文 1 中的“预约完成”只允许短的原生状态发布，不允许把现有 `.dispatcher.read().await` 原封不动搬进持续 reader。否则一条注册事务就可能阻塞连接的额度、释放和取消。第五节给出该前置改动；它是实施退出条件，不是靠超时兜底。

## 五、B2、C03、C04、R14 与分片请求生命周期

### 5.1 先缩锁，再移动准入，最后删除闸门

第一步把重组的实际存储所有权从“拷贝期间借用池容器”变成“分配时绑定的不可替换 backing 所有者”。分配和 owner/span 的验证仍在原池边界做一次；载荷所有者固定真实映射、范围和 reservation，复制只需要这一项的独占写权限或已完成项的只读借用，不持全池 RwLock，也不暴露可在锁外游离的裸指针。借用与析构相互排斥，最后读者结束后才释放原分配。池仍负责分配器，不新增第二个 allocator，也不恢复可替换的 receive-pool 槽。

registry 在短的 shard 锁内取得该项的所有权引用及状态，之后在条目自己的串行访问边界进行 feed/copy；GC、结算和 take 在同一状态上决定唯一终态，移出表后在锁外销毁 backing 和路由上下文。已取得的复制引用使真实存储不能提前析构，不能用延迟时间保证指针有效。finished/trim 后仍计原实际容量；文件溢出继续依真实文件 owner 清理，不将其提升到 SHM 或额外复制。当前锁依赖见 `core/protocol/c2-wire/src/chunk/backing.rs:238`、`:259`、`:303` 和 `core/protocol/c2-wire/src/chunk/registry.rs:367`。

第二步使 dispatcher 的路由发布只占一个短的同步状态边界：契约准备、payload 工作和异步控制 I/O 放在边界外；reader 在边界内取得同一个 dispatcher 所拥有的 route Arc，并同步完成原生 pending 预约。它仍是唯一权威，不增加一个可能过期的旁路路由缓存。对注册发布和注销原子性的既有检查继续保留；调用凭证在边界内固定 route UID 与接纳事实。

第三步在接收循环解析首个 OPEN / 首片的路由元数据，完成路由预约和请求状态发布，再让有界工作任务分配重组存储、复制数据。新线上格式把路由元数据放在小的 OPEN 中，不藏在需要打开 SHM 才能读到的业务字节里。后续帧只写已经发布的请求状态。初始存储尚未就绪的状态由同一请求记录表示；它不等待前一个任务去另表发布身份，也不创建每片一个无界 waiter。

最后删除 `core/transport/c2-server/src/chunk_ordering.rs`、其准入门和拆除围栏；替换后只有连接请求记录上的接纳/终态和实际所有权保护。当前 `Pending` 返回分支位于 `core/transport/c2-server/src/server.rs:3712`，waiter 的一次终态通知见 `core/transport/c2-server/src/chunk_ordering.rs:129`。删除旧门时一并删除该正常状态机走不到的分支，不把它搬成新的防御回退。旧注释把门容量检查说成重复不变量并不成立：帧许可先结束、拆除围栏尚存时会留下门项；整门删除前不得先删容量检查。

### 5.2 用请求生命周期解决迟到续片

选择同一物理连接上 OPEN 的请求号严格递增且不复用，编号在线程安全 writer 的“第一次提交”线性点分配，而非调用排队时提前分配。现状 Rust 在 `begin_pending` 提前取号（`core/transport/c2-ipc/src/client.rs:1143`），仅有单调取号不能证明并发写时 OPEN 的线上次序；本期须改变这个线性点，并让需要关联回复的控制请求也使用同一序列。ID 0 仍留给已有无关联控制及握手。

接收端保留一个已见 OPEN 最大号和一张活动请求表。见到新 OPEN，先推进最大号，再决定接纳或给该请求一次拒绝终态；见到序号不大于最大号的新 OPEN 为协议错误，不再建一个“重试调用”。普通数据、CREDIT、半关闭、取消只能作用于活动项。小于等于最大号而已不在活动表的续帧是已退役请求的迟到帧：只做帧边界、长度及 SHM carrier 接纳/释放，不重新创建项，也不再发业务错误。序号从未打开的未来请求数据在协议边界报错；解析边界仍可信时不破坏其他调用结果。

每个活动项有唯一回复/终态权和不可复制的接纳凭证；完整终态写出后退出活动表，旧任务仍带原状态，不能重新取得回复权。客户端按活动表分派，已退役请求的迟到终态不可能被当成“当前下一调用”的响应。借出表不跟活动项一起删除：迟到释放依原物理连接的 loan_id 结算，直到真实释放或对端结束。丢弃已退役数据也必须结束其远端读区间。

这样历史请求只占一个最大号，不积累每个已结束请求的 tombstone，也不依赖 `chunk_assembler_timeout` 的大小。GC 回收真实重组存储，不删除“这个号已经打开过”的事实；现有组装超时只可用于尚未进入资源方法的输入接收准备，返回该请求的一次确定拒绝。已经开始的流及其活动条目不按静默时长 GC，也不把正常等需求/额度算作组装失败；其占用由字节预算和活动调用数界定，直到显式取消、真实故障或自然结束再清理。不能给产出中的大条目套现有 60 秒默认值而自动放弃已派发工作。

这覆盖任务指定的反例：请求被拒绝 → 结算记录超时 GC → 较迟续片 → 下一次 Node 调用。续片此时仍小于已见 OPEN 最大号，被结算后丢弃，没有第二次回复。恢复 `ChunkAssemblyId` 不能解决“旧项已经不存在”的识别问题，因此不恢复代际守卫。序号空间到 `u64` 表示极限时只停止开新调用并排空旧连接，再建新物理连接；不回绕、不清表继续复用。这里是线上整数的表示边界，不是以工作量的通常规模定上限。

### 5.3 剩余项和配置的裁决

| 项目 | 本期选择与退出条件 |
| --- | --- |
| C03 | 保留已完成的 assembly 与 pending permit 单一所有权；请求终态、GC、拒绝、断连均通过同一个请求记录完成。删除旧门之后删其专有围栏；不删除实际 backing 借用、预算和唯一终态的保护。复现 GC 与 feed/取消交错，确保不会退还别的调用许可。 |
| C04 | 新流的需求、额度、步骤结束、最后一个 Body 引用消失、借出结算使用真实状态转换唤醒；先注册等待再检查条件，避免丢唤醒。没有统一发布源的旧 pool/relay 清理轮询不机械删除，保留其进展职责；要替换某处 timer，须先证明覆盖最后外部 owner、锁外结算和原截止时间。流式不承诺顺带完成所有 C04。历史边界见 [C04 复核](../acceptance/2026-10-26-c04-notification-reuse-review.md)。 |
| R14 | 在实际 backing 接纳时统一 owner、generation、allocation 长度和范围验证，内部借用依受检类型，不重复查坐标；不为省几行造 CheckedSpan、锁证明框架或缓存。budget、层级回退、退款及错误顺序仍保留；bitmap 或其他 allocator 清理不属于本期。边界见 [精简账本](2026-10-13-local-slimming-batches.md)第 12、34 行对应条目。 |
| `max_total_chunks` | 删除这个旧名，不拆成两个用户设置。硬用途改名 `max_inflight_chunk_frames`，只控制同时处理/尚未结算的分片帧数量；不再控制已删除的首片门。registry 提前清扫阈值从同一工作容量推导为内部维护阈值，仍只触发 GC、不拒绝。硬帧容量与软清扫是不同语义，但后者没有独立需要调节的产品理由。原映射为 `core/protocol/c2-wire/src/chunk/config.rs:33`、服务端容量为 `core/transport/c2-server/src/server.rs:558`、`:567`，软扫描为 `core/protocol/c2-wire/src/chunk/registry.rs:268`。 |

## 六、公开接口、CRM 与代码生成

以下均为拟议接口。第一期 portable 方法每个条目仍是一个 FastDB Payload，绑定描述的是单条目的输入/输出，不另造流容器 schema。Python-only pickle 可以逐条使用，但不成为跨语言生成输入。资源仍是普通类，不要求继承 CRM 或装饰资源类。

### 6.1 Python

增加唯一的 CRM 声明标记 `@cc.stream`，第一期精确表示服务端流，不提供尚未实现的 mode 参数。它与 `@cc.read` / `@cc.write`、`@cc.transfer` 正交，装饰顺序不改变元数据。CRM 的 `Iterator[Payload]` 注解提供类型提示，流形态以明确标记为准，FastDB 结构继续只从显式 transfer 取得；普通 Python 返回迭代器不会被猜成远程流。注册时一次校验资源是受支持的同步生成器/迭代器实现，拒绝 awaitable；不让 awaitable 进入 unary pickle 回退。

```python
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
                yield model.export_state()
        finally:
            model.finish()

cc.register(Simulation, BasinSimulation(), name="basin")
proxy = cc.connect(Simulation, name="basin", address="ipc://simulation")
bounded = cc.with_call_options(proxy, timeout=run_wait_seconds)
with bounded.steps(parameters) as results:
    for state in results:
        consume(state)

with cc.hold(bounded.steps)(parameters) as results:
    for held in results:
        with held:
            consume_checked(held.value)
```

`Stream[T]` 只增加迭代、流 context manager、`close(timeout=...)`、`call_id` 和读取已知终态的必要投影；不暴露 send、ack、窗口、loan_id 或内存池。进入/创建流会开始同一个 native 调用并请求首项；自然迭代结束才表示收到 OK 终态。提前退出 context 明确请求取消并按 close 的观察预算报告，不能静默吞掉未完成清理。普通 `for` 提前 break 不会自动调用 Python context manager，应用应使用上述 `with` 或显式 close。

`cc.hold` 的选择作用于每条结果，不返回一个保留整条流的 HeldResult。`@cc.transfer` 仍只绑定 Payload，不加入 `buffer='stream'` 或另一组 transfer 装饰器。借用输入仍只从 `cc.register(..., input_lifetime={"steps": "borrowed"})` 选择，其整体寿命见第三节。`cc.with_call_options` 产生同样的 proxy 视图；调用时生成一个绝对截止时间，涵盖初始编码、准入、首项和所有后续项，不以每次迭代重置。取首项或普通 next 的本地阻塞等待释放 GIL。

流方法的缺省等待为无限，显式 `timeout=None` 同义，`timeout=0` 在派发前到期；不把 HTTP unary 路径继承的总调用计时直接套给长流。有限预算只能来自该流的显式 CallOptions，仍复用 `CallTimeout::After`，不另造流超时配置。现有 Inherit 是由路径提供缺省值的政策（`core/foundation/c2-config/src/call.rs:32`），新流路径将其解析为无限；既有 unary 的 Inherit 不变。

同进程也使用同一个 Rust 准入/步骤调度权威，直接传对象而不为对称性编码成 bytes；条目和取消语义相同，原有 same-process 有限等待限制不得被 Python 自造计时器绕过。若本期要支持与远程一致的有限截止时间，须由原生调用存续记录驱动同进程步骤并单独验收；否则边界明确拒绝 finite timeout，不能表面接受后忽略。建议第一期保留明确拒绝，并将扩展与共享异步桥同批。

### 6.2 Rust

CRM 的权威输入仍是 portable descriptor；生成的客户端方法返回 `Result<ResponseStream<fastdb::Payload>, c_two::Error>`，其 `Iterator::Item` 为 `Result<fastdb::Payload, c_two::Error>`。Rust/TypeScript 继续使用现有 `method_{index}_{name}` 符号和 `hold_` 前缀规则（`core/foundation/c2-codegen/src/targets.rs:1076`），不借流式更改所有方法的名称；以下以第 0 个 CRM 方法 steps 为例。生成的 `hold_method_0_steps` 返回逐项 `Held<Payload>` 的流。生成的资源 trait 方法返回 `Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error>`；迭代器拥有其捕获的状态，不能借用一次调用栈或借来的 `&self` 越过返回。

```rust
impl generated::Service for BasinSimulation {
    fn method_0_steps(&self, input: &fastdb::Payload) -> Result<Box<dyn Iterator<Item = Result<fastdb::Payload, c_two::Error>> + Send>, c_two::Error> {
        let state = self.prepare_owned_state(input)?;
        Ok(Box::new(BasinSteps::new(state)))
    }
}

let mut results = client.method_0_steps(&parameters)?;
while let Some(item) = results.next() {
    consume(item?)?;
}
// 提前停止时显式 results.close(...)，并检查实际清理结果。
```

示例省略参数 schema、类型定义和错误转换，是接口形状，不是本次已编译示例。资源迭代器可以持有共享的领域状态 Arc，在每次 next 的原生路由许可期间访问。返回的 Box 具有独立寿命，不能捕获工厂参数 `&Payload` 的借用；需要跨步骤使用的输入须在工厂内通过 FastDB 正式接口 materialize 成拥有的领域状态。生成桥保留借用输入的正式载荷所有者到调用清理后再失效，不为返回带自引用借用的迭代器增加 unsafe 寿命延长。现有 `ServiceAdapter::invoke` 只返回一个 Vec（`core/foundation/c2-codegen/src/targets.rs:152`）；改为同一个私有 encoded service 分派下的 unary/逐步结果，生成代码不另写窗口或传输。原生调用记录持有迭代器，结束或取消时只在获许可的清理步骤中 drop 它，包含资源自定义析构的真实耗时；不能在任意 I/O 线程触发长清理并提前报告结束。

### 6.3 TypeScript / Node

同一个 descriptor 生成 `method_0_steps(input: Payload): C2ResponseStream<Payload>`，它实现 `AsyncIterable<Payload>`，以及必要的 `close`、`callId` 和终态投影。不是 `Promise<Payload[]>`，也不是先聚合后包装的 async generator。`hold_method_0_steps` 返回逐条 `C2HeldResult<Payload>` 的 AsyncIterable。客户端写法为：

```typescript
const results = client.method_0_steps(parameters);
try {
  for await (const state of results) {
    consume(state);
  }
} finally {
  await results.close();
}
```

Node 的 iterator `return()` 表达提前取消，`finally` 中的 close 观察原清理、不能重发。一个方向只允许一个 pending next；生成桥在语言调用边界拒绝并发 next，不在内部增加多消费者调度。两条流、unary 与控制共享原生连接核心，删除按整次调用串行的 `callChain`，每个已验证物理连接始终只有一个持续 reader；按请求号把数据送入各自有界流记录。Node I/O 适配器可以驱动 OS/HTTP/2，额度、请求终态、借出和关闭判断由扩展的 Rust endpoint context 投影，不能在 TypeScript 再实现一份。传输工厂持有一个原生调用/预算域，所有连接及重连共享它，不能每建一个 endpoint context 就重置 B 或调用数；这是新增绑定的必需能力，不假定现有逐连接 memory context 已经做到。

TypeScript 的资源声明可由同一 descriptor 生成结构化类型 `SimulationResource { method_0_steps(input: Payload): Iterable<Payload> }`，普通类的同步 `*method_0_steps` 即其实现形状，和 Python 一样每次 next 只推进一步。当前生成器没有 Node Host 注册桥；本稿建议第一期只交付 Node 客户端，不单凭一个结构化类型宣称这种资源可以注册。若用户要求第一期 Node 同时提供资源，则增加第十节 N 块：Rust Core Host 的 native callback 注册、Node event loop 的逐步桥、资源强引用和清理必须一起实现，普通同步生成器可用，异步资源 await 不在本期。这个范围裁决列入待决定事项，而不是留一个可调用却未实现的 register API。

### 6.4 契约描述与生成产物

选择将 `c-two.contract.v2` 退役为不可执行的历史格式，发布 `c-two.contract.v3`，所有方法必填 `mode: "unary" | "server_stream"`。当前 v2 的严格方法键和 `none/payload` 返回形状无法区分条目与整体结果（`core/foundation/c2-contract/src/descriptor.rs:279`、`:414`），不在同名 schema 下偷偷改变持久契约发布的含义。第一期不接受 client_stream 或 bidi 值；将来有真实实现再扩展，不预留空能力。

v3 仍保留相同根结构及每项 `name/access/parameters/return/bindings`；`return.kind='payload'` 表示输出条目是 Payload，`bindings.output` 描述一条的 FastDB spec，`mode` 决定数量。portable 服务端流必须有 output binding；允许零条结果，但不添加没实际用途的 void 条目。输入仍为零或一个 payload 参数。ABI、signature 的 canonical projection 都加入 mode，并升级对应投影 schema；条目序号、deadline、hold、借用政策、窗口、route name 不进入契约内容。相同 payload spec 仍由 FastDB 生成一次并复用，不复制一套流式 payload 编码。

Rust c2-contract 在 descriptor 外部边界完成一次形状、数量、mode 和指纹校验；内部 codegen 读取 validated method，不根据注解重新推断模式。Python authoring 和 Rust/Python/TypeScript 的生成输出同步改为 v3；原 v2 parser、writer、样例输入和可执行测试一起移除，冻结验收/发行记录保持史实。ContractReleaseRef 仍是契约发布，ExpectedRouteContract 仍负责绑定路由；不增设 StreamContract、另一套路由或能力探测。新旧契约不能精确匹配时在派发前失败。

## 七、线上记录与版本

### 7.1 一套调用记录和分片格式

选择保持现有固定帧外壳及永久 25 字节版本前导，用新的业务 record 取代 CALL_V2/REPLY_V2 的完整载荷表示及旧 `u16` 总分片数格式。不是在旧响应中猜“第二次响应就是流”。unary 和服务端流共享同一调用生命周期：前者零/一个输入、零/一个输出，后者零/一个输入、零到多条输出。SIGNAL、握手和仍有效的路由控制各有既有职责，释放记录继续使用 loan_id。

固定新业务标志为 `FLAG_RPC_RECORD = 1 << 7`，其载荷首字节是 record kind：OPEN=1、ITEM_BEGIN=2、ITEM_DATA=3、CREDIT=4、SEND_END=5、CANCEL=6、CALL_END=7。删除旧 CALL_V2/REPLY_V2、CHUNKED/CHUNK_LAST 名称及解码；复用位 7 只在已协商 IPC 16 中成立。RESPONSE 仍只表示 server→client；BUDDY 只允许用于 ITEM_DATA，不能再把路由元数据藏进 backing。RELEASE 保持原 CTRL 编码。HTTP 使用同一 record kind/内容及明确的长度边界，非 SHM 的 HTTP 数据不允许 BUDDY，不重新定义一种条目协议。

窗口建立也只用这些记录：握手完成后两端以 request_id 0 的 CREDIT 授予已预约的连接容量；OPEN 可捎带客户端已预约的输出流额度及首项需求。服务端接纳 OPEN/ITEM_BEGIN 并取得输入存储后，以该 request_id 的 CREDIT 授予输入额度，客户端收到之前不发送正字节 ITEM_DATA。ITEM_BEGIN 本身不占数据窗口，但受活动调用及每流一个条目的数量约束；拒绝时给一次 CALL_END。没有无条件给所有未来流的隐含初始窗口，也不新增一个 OPEN_ACK 状态机。

| 记录 | 字段与语义 |
| --- | --- |
| OPEN | 帧头 request_id 非零；现有完整 RouteCallIdentity、method index；首项需求及接收窗口。方法形态来自已经验证的 CRM，不再在调用体里传另一份可能相矛盾的 mode。控制请求与业务 OPEN 使用同一请求号分配线性点。 |
| ITEM_BEGIN | 方向由发送端和 RESPONSE 标记确定；`item_seq: u64`、`total_bytes: u64`。接收边界检查一次长度并预约存储，0 字节条目合法，仍消耗需求位。 |
| ITEM_DATA | 活动 request_id、`item_seq: u64`、`offset: u64`、本片存储表示与长度；inline 或现有带 loan_id 的 SHM 描述符。与 ITEM_BEGIN 一起替换旧 chunk header。最后 offset 到达 total_bytes 即完成，无需第二份 total_chunks 或 chunk_last 真相。 |
| CREDIT | request_id 指向流、返还字节及下一条需求增量；总是授予对端的发送方向，不再传重复方向字段。连接额度用 request_id 0 的同类记录表达。只授予已预约容量，最大值受已冻结预算约束；同一流的字节返还与需求是独立字段。 |
| SEND_END | 该方向最终完整条目数，不再发业务条目；不结束控制或另一方向，不等于调用完成。 |
| CANCEL | 活动 request_id、取消原因的既有错误/原因投影；表示取消请求，不是成功 ACK，也不作为副作用未发生的证明。 |
| CALL_END | request_id、唯一 status（OK 或规范错误）、最终完整输出条目数。清理完成后形成；错误不撤回前面条目。 |
| RELEASE | 沿现有连接级 loan_id 记录，独立结算传输所有权；可在调用终态后到达。 |

线上整数均按 c2-wire 统一的小端编码；错误仍用规范错误 codec。接收边界验证 flag/record 组合、活动请求、连续 item_seq/offset、长度和已授予额度，然后交受检记录给内部。内部不再重复解码或从错误文本猜类型。标识、长度和 offset 的整数范围由实际线上类型及本机地址空间决定；Node 对 u64 保留 bigint，只在已检查实际 Buffer/TypedArray 分配边界后转换长度，不能经 Number 丢精度。分片数不再受旧 u16 的偶然范围限制，也不能给流累计字节数加一个未声明的 4 GiB 上限。

OPEN 是小的元数据帧，输入数据另用 ITEM 记录；初始输入 SEND_END 之后仍可发控制。所有有载荷的数据片先占两级窗口和帧槽；SEND_END、CANCEL、CALL_END、RELEASE 不等业务额度。CALL_END 与 SEND_END 在本调用必要数据之后排队；错误取消可明确声明丢弃未完整条目，不能让控制优先级使 OK 越过尚未发送的结果。新核心用已接纳调用凭证区分“新业务”和“旧调用的后续帧”，替换当前靠 `WriteRequest::Control` 发送已接纳回复的命名混用。

### 7.2 版本选择与切换

| 边界 | 当前 → 本期提案 | 理由 |
| --- | --- | --- |
| IPC 握手 | 15 → 16 | 新调用/条目/窗口/终态及分片格式不兼容；固定前导和身份核验顺序保持。旧端只得到结构化 VersionMismatch，不发送或猜旧业务格式。 |
| SHM | 4 → 4 | backing 布局、只读 peer 映射、prefix/slot/generation 与 loan_id 不改变；短锁的存储所有权是进程内结构调整。若实现后来需要修改共享字节布局，须单独改版并重走验收，不能把 ABI 升级等同于 SHM 已兼容。 |
| C/FFI ABI | 7 → 8 | Node endpoint context 增加请求/流事件、持续读分派、额度和真实终态投影，结构/符号一起切换；不保留 ABI 7 loader 或另一条 Node 收包路径。 |
| relay mesh | 7 → 8 | mesh 的路由发布传播新的 v3 契约及流式 HTTP 数据入口能力边界，整个 relay 批次按精确版本部署，不让旧 relay 接到流调用后才落回全量路径。 |
| portable contract | v2 → v3 | 方法形态进入规范结构与两个指纹；旧格式无默认 mode 或兼容导入。 |

现有 IPC、SHM、ABI 与 mesh 常量分别见 `core/protocol/c2-wire/src/handshake.rs:11`、`core/foundation/c2-mem/src/alloc/buddy.rs:7`、`core/foundation/c2-mem-ffi/src/lib.rs:12`、`core/transport/c2-http/src/relay/peer.rs:7`；SHM 的格式版本通过握手协商，不恢复段头版本字段。IPC 15 的当前边界以 [握手参考](../reference/ipc-handshake.md)为准；实施须按 exact candidate 更新该页及规范常量，不以本设计稿视为已经升级。

HTTP 流式入口使用 `application/vnd.c-two.rpc-records;version=16`，复用同一个 record codec 版本，完成 OPEN 的契约/路由核验后才允许业务记录；多跳 relay 对精确 mesh 版本和数据入口执行相同校验。旧 unary HTTP 路径本身仍是支持的一元协议，不作为流式失败后的兼容回退；其整体物化实现被替换。不能同时保留 IPC 15 和 16、v2 和 v3、旧 chunk 和新 offset 分片两套解码。升级需让旧进程自然排空后启用新候选；不能强切现有已派发调用来满足部署时间。

## 八、与派发后结果查询和去重的交叉

第一期的调用定位符是原 server instance、调用方本地的原物理连接观察身份和 request_id 的不透明组合，流标识就是该 request_id；每条已交付结果再带一个仅供诊断/错误上下文的 item_seq。两端本地连接观察 ID 不必相同，这个组合只让调用方保存“哪个调用、最后交付到哪”，不是服务端可持久查询的全局标识，也不承诺跨重连可附着、查询或继续。连接结束后新建连接即新的调用；不自动把同样参数发过去，不将 item_seq 变成消息幂等键。第一期没有持久结果保留服务，也没有自动重放缓冲。

正常在线时只保留一个需求对应的待交付条目、必要的正在传输片、原始终态及真实清理记录；显式 hold 保留其本地副本。已消费条目不为未来查询无限保存。收到 terminal 的确定结果在本地记录中保留到调用对象/观察者完成消费；不能因随后连接结束覆盖它。结果被应用保存的寿命与 Runtime/连接的寿命分开。

第 2 项待办需要另行调研和 ADR：可持久保存的逻辑调用标识、调用者身份、跨进程唯一性、请求指纹、结果存储及保留政策。届时逻辑调用标识附着于这里的一份调用记录，重新发送相同标识是显式的查询/去重操作，不重新执行生成器；同 ID 不同请求内容必须在准入边界拒绝。本期不为尚未决定的存储增加 UUID 字段、保留时间设置或“或许能恢复”的 resume API；后续需要新的线上字段时按 0.x 规则改版。

对流，查询“已执行/终态”与取回“全部历史条目”是两种不同承诺。有限摘要至少要区分仍运行、已终止、曾记录但结果已过保留期，以及查无记录；查无记录不能被推断为从未执行。仅保存 CALL_END 和已产出序号不能证明调用方实际消费，消费确认又不能证明领域副作用提交。无限流的全历史重放需要应用的持久检查点/日志，不能作为通用查询的默认保留方式。客户端流、双向流还需逐输入消息的提交边界；本期明确不承诺它们。

故障报告逐项确定：未派发 OPEN 的失败是 pre_dispatch；已派发但未取得 CALL_END 的连接结束是 DispatchUncertain，已交付条目仍有效；已完整取得 CALL_END 后是那个确定终态，不能被后续 EOF 改成未知。只缺尾部条目时不把此前条目撤回，也不把“本地已看见 k 条”冒充服务器只执行了 k 步。错误投影保留原调用定位符、最后交付序号及真实 transport cause，供应用对账。

## 九、验收与测量

本节为未来实施验收条件，本次文档交付没有运行这些测试、基准或 CI。

### 9.1 状态机及真实运行测试

Rust 的 c2-wire 单独验证两方向状态、两个字节窗口、需求位、并发取消/自然结束的唯一终态、字节额度与 loan release 的独立性、终态后迟到帧和请求号不复用。测试应检查实际输出动作及所有者销毁，不只匹配源码文本。c2-server/c2-ipc 用真实本地流验证重组与混合 unary、流调用；Core 验证旧流的准入凭证跨 Host 排空/Runtime 退休仍有效，路线注销和同名新注册不会接走旧流。

B2 必须有确定性的交错测试：大条目复制被暂停时，另一个首片的预约、释放和取消能推进；在复制引用仍存活时 GC/take 不释放 backing；首片写预约先于后来 unary 写；后续读不能越过先前写；不同路由和 parallel 允许重叠；分片拒绝、等待超过 `chunk_assembler_timeout` 后发送迟到续片，再执行下一次 Node 调用，只出现原请求的一个终态。改变请求分配/写入次序的并发测试证明最大号规则，而不只是顺序调用测试。

执行证明使用至少多于配置 worker 数的“停在 yield、没有新需求”的真实流，同时发一个可立即执行的 unary：闲置长流占用的 blocking worker 和路由访问许可应为 0，unary 可完成；对应的调用槽及初始借用本地输入仍真实记账。另测一个 next 正在运行，取消/关闭返回未完成、方法继续、finally 实际结束后才发布终态。没有固定短 sleep 后猜成功的断言，采用生产状态事件或确定的屏障。

Python 覆盖 3.10 语法/运行支持边界及既有 3.14t gate：普通类生成器、零条、一条、多条、资源异常、首次异常、finally 异常、提前 context 退出、无限流显式 close、deadline 只结束等待、关闭不自动取消；普通结果和 hold、FastDB checked view 在 release 前后、活动 export 阻止 release、借用输入跨 yield 可用而清理后失效。证明 PyO3 移动原 Vec，复制后先释放请求再进入 next，等待 next 释放 GIL；同进程对象直传及 finite timeout 明确拒绝均需真实测试。

Rust 资源/客户端与 Python 资源/客户端使用相同 portable descriptor，Node 作为客户端加入交叉矩阵：每个受支持资源语言 × Rust/Python/Node 客户端 × 直接 IPC/单跳 relay/双跳 relay × materialized/hold；覆盖 SHM 能用及不能用两条路径、inline、分片、file reassembly。若用户选择 Node Host，再增加 Node 资源一整行，不以客户端通过替代。Node 同连接打开长流后，另一 unary、第二条流、释放和取消必须继续工作；持续 reader 只能有一个，close 等真实存续和借出结算。

relay 测试证明初始请求未结束时已经读到响应首片，反向控制能继续发送；缺失 HTTP/2/双向体能力在派发前明确失败。一个响应条目远大于 relay 窗口时，relay 不分配完整条目 Vec，buffer peak 不随该条目 N 增长，只随已授予窗口及已记录的 HTTP 库缓冲变化；最终 SDK 的完整值 N 字节保留须单列。未知长度、错误 Body、长度矛盾、预算不足、Body drop、Bytes clone 跨 Body 析构、旧 Body 跨缓存淘汰/重连都检查最后真实 owner 和 reservation。

### 9.2 异常客户端与关闭

| 情形 | 必须证明的结果 |
| --- | --- |
| 停止应用迭代但持续读控制 | 最多原来一条需求的结果，窗口受限，不再 next；其他有额度调用推进；显式 close 能清理。 |
| 慢消费、长 hold | 生产速率跟随需求，长 hold 不占对端 SHM；本地 B 和 checked view 寿命真实，不能把缺额度判故障。 |
| 完全停读或 SIGSTOP | 不新增无界队列，已借出不按时间复用；同物理连接控制可能受 OS 阻塞，但其他已准入连接继续；shutdown 预算到期报告未完成。 |
| 读 SHM 后不发释放 | 只占已授予窗口和帧/借出容量；不能凭 CREDIT 回收；peer 恢复后释放，或读端真实对端结束后才回收。 |
| 条目中途断开 | 不交付半条目，保留此前完整条目；缺 CALL_END 则 DispatchUncertain，当前资源步骤实际完成后清理，无重试。 |
| 取消与最后结果/EOF 竞争 | 已解析终态保持确定；取消不是未执行证明；每个调用终态一次，其他调用不受可读 framing 内的错误牵连。 |
| 无限流与停机 | Runtime/Host shutdown 不发隐式取消；观察预算到期为 `completed=False`，显式 close/领域停止条件后原 receipt 继续完成。 |

### 9.3 基准与平台门禁

基线为本次输入提交的现有 unary；实施候选和基线用相同 runner、工具链、FastDB、负载及配置配对测量。先记录：一个 unary 一次返回 N、多次 unary 每次返回一个批次、一个流返回相同批次；分开测首条可用延迟、逐条间隔 p50/p95/p99、最后终态延迟、每秒条目、有效字节吞吐、CPU、复制字节、worker/路由等待、窗口等待和分配次数。另测混合短 unary 与大条目长流时，短调用的尾延迟和各流有效吞吐，不能只测独占大数组吞吐。

负载选用现有 unary 基准的代表类，再加配置边界 S/C/片长前后、超过窗口的大条目、空/极小条目、FastDB 连续数组与混合字段、Python-only pickle、不可用 SHM 和文件重组。测试数量和持续时间由测量误差、暖机及资源周转周期确定并事先记录，不把某个数组大小或几秒测量当成产品上限。记录 C-Two 预算峰值与 RSS 两份数据；持有应用材料化结果造成的 RSS 增长不冒充窗口失效。

验收门槛分两层：正确性、无无界积压、无提前回收和真实终态必须全部通过；性能候选需相对同次 unary 基线报告区间及误差，现有 unary 的中位延迟、p95 延迟或吞吐若有超过 5% 且超出测量噪声的退化，须先定位并由用户决定后再接受。单条流窗口保守造成的吞吐不足不能靠偷偷预取无限条解决；只有测量证明需要时才调整显式窗口预算。首条延迟相对整份 unary 应报告随计算/分批情况的变化，不承诺每种负载都加速。

所有协议、ABI、契约和语言桥改变以同一个 exact implementation candidate 跑 GitHub Actions：Linux、macOS、Windows x64，以及现有 Windows ARM64 原生 gate；Rust core/workspace、公开 Rust SDK、Python 全套及 Python/Node portable 交叉矩阵、relay 和现有文档/运行器检查均需通过。适用的 Python 3.10 与 3.14t gate 保留，ARM64 沿现有支持矩阵，不能用 x64 进程代替原生运行。验收记录保留源码提交、Actions run URL、OS/架构、工具链、配置、二进制 SHA-256 和基准原始结果；本机 focused green、只读审查和文档提交都不等于接受。确认无活动使用者后及时删除可重建产物，保留日志与 receipt。

## 十、实施分块、依赖与退出条件

本节只规定 sequencing 和出口；活动状态仍只由 `docs/design/backlog.md` 维护，本稿不另建进行中的任务清单。用户批准之前不实施。本次唯一新增文件的范围不授权修改下表文件。

| 块 | 文件范围 | 依赖 | 退出条件 |
| --- | --- | --- | --- |
| A：所有权与短锁 | `core/foundation/c2-mem/`、`core/protocol/c2-wire/src/chunk/{backing,registry}.rs`、`assembler.rs`，相应内存/载荷参考 | 设计及所有权审查 | 实际 backing 在锁外复制仍有效，GC/释放唯一、预算不提前退款；复制不持池/shard 公共锁，文件与 SHM 两路径均通过。不开新协议。 |
| B：契约及公共形状 | `core/foundation/c2-contract/`、`core/foundation/c2-codegen/src/`、Python CRM metadata、三语言生成夹具，契约参考 | 用户批准接口和 v3 | mode 进入两个指纹，v2 活代码移除；统一生成形状及失败边界通过，不声称传输已支持流。可与 A、C 并行。 |
| C：预算所有者 | `c2-config/`、`c2-mem/src/budget.rs` 的复用、`c2-core` 调用保留域、`c2-http/src/payload.rs`，配置/内存参考 | 用户批准 S/C/B 及配置迁移 | 分配前准入、slice 最后引用、零预算、输入/响应进展、跨重连共享和退休统计通过；旧 retained-input 字节字段移除。接口可先于 A 并行，落地与实际所有者一起验收。 |
| D：连接、请求生命周期及 B2 | `c2-wire/src/{connection,frame,flags,chunk}`、`c2-server/src/{server,scheduler,dispatcher,writer,connection}.rs`、`c2-ipc/src/{client,connection,response}.rs`、规范 `c2-error` 注册表，握手/生命周期/errors 参考 | A、C 的接口稳定 | IPC 16；OPEN 在线性点编号，短 dispatcher 发布，首帧预约在 reader；删除排序闸门、旧 Pending 分支、旧 chunk 格式和 max_total_chunks；迟到续片反例及混合 unary 顺序通过。 |
| E：资源逐步桥及客户端 | `c2-core/src/{host,client,session}.rs` 与调用/生命周期模块、`sdk/rust/`、`sdk/python/native/`、Python proxy/server/transferable，SDK 参考 | B、C、D | Python/Rust 真资源与客户端逐步推进、hold/借用输入、取消/截止时间和排空符合正文；同进程语义核实，不能用模拟流替代。Rust 与 Python 的薄桥可分开实现，但共享 Core 权威只做一份。 |
| F：Node 持续读投影 | `c2-mem-ffi/` endpoint context/C header、TypeScript native binding、`c2-codegen/assets/typescript_transport.ts`，C ABI 与 Node 使用文档 | D；与 E 约定事件形状 | ABI 8、一个 reader、按请求分派；去整次调用 callChain 和 ABI 7 loader；Rust/Python 资源→Node 的直接 IPC、并发、释放及迟到帧通过。可与 E 并行。 |
| G：relay 与 HTTP | `c2-http/src/{client,relay,payload.rs}`、native 响应逐片入口、Node HTTP/2 连接器、CLI relay 配置与文档 | C、D；E/F 提供端到端调用 | 双向 HTTP/2 体、逐跳窗口和条目需求传递，unary relay 不整体物化，Body reservation 真寿命；mesh 8 切净，单跳/双跳及异常下游通过。内部 Body 与 HTTP 客户端适配可并行。 |
| N：可选 Node 资源 Host | Node SDK 层的 native Host 绑定、Core encoded service callback 投影、TypeScript 资源生成桥及资源测试；不让 foundation 的 c2-mem-ffi 反向拥有 Core Runtime | 用户明确要求；B、D、E 的 Core 形状 | 同步资源生成器在 Node event loop 每次只推进一步，等待 I/O 不占许可；注册、原资源强引用、finally/失败清理、注销与 shutdown 真终态、Node 资源跨语言整行均通过。没有这块就不承诺 Node Host。 |
| H：整体验收与文档 | 三语言测试、现有 repo/CI runner、`.github/workflows/`、`docs/reference/`、`CONTEXT.md`、`docs/acceptance/` | 所有选定块 | exact candidate 的全平台 CI 和配对基准完成，Host 独立复核，用户接受；backlog 删除完成项，仍未实施能力只留在 backlog。 |

关键路径是 A/C → D → E/F → G → H；B 可早做，最终 mode 和事件形状必须先冻结。E/F/G 的实现可在接口稳定后并行，但协议/ABI/mesh 最终只能集成为一个兼容自身的候选，不把中间协议当发布格式。N 若被选定则加入 H 的必需依赖。每块完成时更新其机制文档和验收证据，阶段完成不降低生产质量或允许留下临时兼容路径。

## 十一、与任务书不一致之处

第一处是早期八问第 6 问的“在关闭预算内主动取消还开着的流”，它与任务要求优先遵守的“C-Two 从不自行放弃已派发调用，关闭只停新调用”不能同时无条件成立。[单一属主评估](2026-10-31-single-owner-release-evaluation.md)第 6 问及[协议批设计](2026-11-01-protocol-batch-design.md)第九节仍保留该表述；现行 [Runtime 生命周期](../reference/runtime-lifecycle.md)的 shutdown 契约已要求继续在途调用。本稿选择后者，显式取消由调用方提出，shutdown 到期只报告未完成，不自行变成 CANCEL；这需要用户确认对旧草图的修正。

第二处是 B2 的隐含充分条件：缩小重组池锁以后，将现有首片准入直接搬回接收循环仍不能保证控制不受阻塞，因为 `core/transport/c2-server/src/server.rs:3142` 还等待 dispatcher 的异步锁；`:3811` 的注释也明确承认注册事务可使它停住。本稿增加短路由发布这一前置条件。它不恢复第二套路由权威，也不靠取消超时掩盖等待。

第三处是 TypeScript 的资源实现范围。如果任务书意指第一期三个语言都能提供资源，建议的“Node 客户端先交付”与之不一致。证据是 `core/foundation/c2-codegen/src/targets.rs:761` 的生成路径和 `:927` 的客户端方法，以及 `core/foundation/c2-mem-ffi/Cargo.toml:15` 起的 endpoint/memory 依赖边界；现有 Rust/ Python 资源桥不能被描述为 Node Host 已有。本文给出资源形状和可选 N 块，请用户在批准范围时裁决；未把这一缺口写成已支持。

## 十二、待用户决定及术语建议

建议用户一次确认以下相互独立的产品选择；本次设计交付不请求中途确认，也不实施这些裁决。

1. 是否接受第一期仅服务端流、同步生成器按一步调度，async unary 后做；客户端流/双向流等共享异步资源桥；Node 第一阶段仅客户端，或把 N 块纳入第一期。
2. 是否接受第四节六条顺序契约，尤其长流在 yield 处允许其他同路由操作交错，以及跨路由因果由应用等待明确提交边界。需要全流事务隔离的领域方法应另行建模。
3. 是否接受 shutdown 不自动取消、无限流可令 receipt 长期未完成；显式 close 仅是取消请求、清理预算只界定观察，截止时间不隐式取消。
4. 是否接受 S/C/B 显式有限预算、缺省 0 的启用方式，以及合并/替换 retained-input 字节设置；若需要开箱即用的正默认值，先指定目标部署和测量依据，再定默认，不以通常数组或常见时长猜上限。
5. 是否接受流式 relay 要求 HTTP/2 双向体、Node 原生连接器、浏览器不在本期支持范围；HTTP/1 unary 继续是独立有效能力。
6. 是否接受 contract v3、IPC 16、ABI 8、mesh 8、SHM 4 的同步切换，以及所有旧可执行格式和别名移除；同进程 finite timeout 暂保留明确拒绝。
7. 是否接受第九节先冻结的 unary 性能退化复核门槛和 exact candidate 三平台/Windows ARM64 门禁；更广 async unary、完整遥测和派发后查询仍需按 backlog 决定顺序。

建议实施时加入 `CONTEXT.md` 的新词如下，本次不修改术语表；既有 Runtime、Host、hold、借出、租约、停用、对端结束、预算和截止时间均沿原义。

| 中文 / English | 建议定义 |
| --- | --- |
| 一元调用 / unary call | 每个方向至多一个业务条目的调用；仍有独立调用终态。 |
| 服务端流 / server-streaming call | 单次输入、零到多个输出条目的一个调用。 |
| 客户端流 / client-streaming call | 多个输入条目、至多一个输出的调用；本期不实现。 |
| 双向流 / bidirectional-streaming call | 两个业务方向各自有序且可独立推进的一个调用；本期不实现。 |
| 条目 / item | 一次流迭代交付的完整领域值；可由多个分片传输，不把分片称为条目。 |
| 方向半关闭 / send half-close | 一方声明不再发该方向的业务条目，控制及另一方向继续。 |
| 调用终态 / call terminal outcome | 资源执行及清理真实结束后形成的唯一确定结果；与发送结束和借出结算分开。 |
| 窗口 / flow-control window | 接收方已预约并授予发送方的有限传输容量；不是整条流的大小上限。 |
| 条目需求 / item demand | 消费者允许生产者再推进一条的许可；第一期每流最多一条未结清需求。 |
| 资源推进 / resource step | 从取得本次路由访问许可到产出一个条目、结束或失败的同步执行区间，网络等待在区间之外。 |

本稿未能核实的地方：一次推进跨 worker 的 Python/GIL 与用户生成器行为、native Node 持续 reader、HTTP/2 双向体在选定客户端/relay 库中的实际推进及内部缓冲、所有新预算的性能和默认值、精确候选的三平台正确性，均只有设计和静态证据。调研列出的 gRPC/Envoy/Ray 未复现项仍未复现，不据其作 C-Two 性能或运行保证。派发后查询/去重的持久标识及保留政策尚无独立设计，本稿仅固定与流式交叉的语义边界。
