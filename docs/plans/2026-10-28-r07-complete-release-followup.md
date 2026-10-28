# R07 后续：失败释放的持有者与最终收尾

状态：独立审查完成，等待用户确认；尚未实施。生产基线 `ba6179a937d26419cf1d0bf28e0d74ba5867c256`；已审查的设计文本为 `cc212251`，共享 pin/统计上下文的澄清为 `155beafe`。本轮不修改生产代码或测试，不构建、不发版；不改本地分支和标签，不接触历史命名空间。

## 1. 要完成的边界

R07a 已解决显式失败仍有接收者时的低层 owner 保留。后续需要闭合从 Core/Held、消费式出口、原生载体最终析构，到 Host 关闭的责任链。目标是可恢复竞争结束后，真实分配及相应预算仍能由一个明确的持有者释放；不能用逻辑失效、回执有错误、peer unmap 或归还 permit 代替释放完成。

这里的“最小”指复用现有释放权威和执行者，不保证代码净减。SHM 分配、已发布跨进程释放义务、映射退休、用户值失效和连接关闭是不同事实，不能合并成一个 released 位。

本轮不扩展回调重入、任意 `Registration::Drop` 自等、私有 reset/atexit 或两份控制会话重构。裸 `MemHandle` 继续遵循现有手动释放及 pool 寿命契约；本方案的自动析构保证针对正式路径持有的 request/response/reassembly/prepared 载体及错误清理对象，不把裸坐标变成释放权限。

## 2. 当前调用链与原设计理由

- `lifetime::HeldResponse::invalidate_then_release` 先取走 lease，失败后仍标记 released；公开 Core wrapper 随即归还 connection pin。Rust Held 先取走全部 owners，Python HeldResult 撤销一次性 callback。底层 R07a 保留的 carrier 因上层退出而再次进入 best effort Drop。
- `ResponseLease/RequestLease::into_owned_bytes`、Core Held 构造失败、FastDB `open_held` 构造失败及请求准备失败，消费了上层接收者；返回错误本身不能接住尚未完成的释放。
- `ReassemblyBacking::Drop` 最后失败时丢 handle、forget reservation。这样避免虚假退款，但同时失去了再次清理的真实载体。原 pool GC 不会凭 alloc_count 找回这个 owner。
- peer SHM 的接收方最后 unmap 不会修改生产者的 live allocation record。连接及维护任务已结束时，不能承诺“下一次 GC”一定存在。
- `PreparedShmResponse` 当前不持原 pool；fill/panic/消费式回滚、发送 future 取消等路径需要共享一个真实清理 guard。server trailing-inline request 还有绕过 `PeerShmRequest` 并吞掉 free 错误的分支。
- 普通 `HostInner::Drop` 经旧 Core shutdown 入口首次创建关闭线程；创建失败可发布 error outcome 而 Server 仍活。新显式 Runtime retirement 的预交接保护没有覆盖最后 Host Drop。

原有 RAII 与消费式接口减少了正常路径状态，保守预算避免超卖，独立 Core worker 避免 caller 的短预算取消实际收尾。这些理由继续成立；需要改变的是失败所有权的去向和执行责任。

已有 macOS/Windows 原生隔离记录见 [R07 评估](2026-10-26-r07-failed-release-ownership-evaluation.md)。Core/Held 的相关代码与该实验时相同；本轮不把旧实验或已有 CI 写成新方案已实现的证据。

## 3. Core/Held：一次失效，多次原生清理

保留现有 `released` 与 lease Option，分别表达业务值已失效和物理清理是否仍待完成，不增加第二套持久状态机。

1. 在调用 invalidator 前提交“已尝试失效”，包括 invalidator 返回错误和 unwind；之后不再重放用户回调。
2. 借用原 lease 释放，成功才清空。失败原地保留实际 carrier、原 pool 和预算。
3. 再次 `release()` 只尝试原生清理。`value`/buffer 一经失效就不可恢复访问；保留的仅是清理权。
4. 将 connection pin 和现有 BufferLeaseGuard 绑定到同一原生清理义务，直到实际释放、按原协议转交释放权或原 pool 完成真实 teardown 后才归还。invalidator 失败但 transport 成功，应立即归还这些持有。
5. 保留失效先于 transport release，以及 panic 时 response 清理先于 value 析构的顺序。使用局部析构 guard 保证 unwind 也进入同一清理入口，不 catch 后吞掉 panic。

Rust Held 的 value 与 response 不再作为一个不可重试的 `owners.take()` 整体消费；已失效 value 可以销毁，response 清理义务单独保留。Python 正式 Held 路径保留原生 response，而非保留并重放任意 `release_cb`。私有 PyCoreResponse 的 tracking 和 export 检查继续受原 inner 边界保护，active export 仍须在 invalidator/释放前拒绝；tracking 不能复活已失效载体。

## 4. 原生最终 Drop：复用既有退休执行器

单靠各 pool 的同步 GC 不能闭合最后 peer owner 的恢复。推荐把已有 dedicated-retire 执行器的生命周期管理下沉到 `c2-mem`，让同一个有界执行器接收具体的原生释放工作；保留 dedicated 的 read_done/crash-timeout 路径，不另建全局 reaper、通用任务接口或闭包重试队列。

复用现有 worker 不等于免费。普通 SHM/reassembly pool 在准入时建立退休持有；此前仅走这些路径的进程也可能启动既有退休 worker。HTTP-only 和不参与 IPC 接收的纯 owned 数据不因此启动它；已经协商可接收 SHM/分片的 IPC 连接，即使实际只传小包，也可能为后续接收预先准备该 worker，不能宣称纯 inline IPC 一定无此成本。

### 接管对象

释放工作只有具体的数据：原 pool 绑定、真实移动的 MemHandle 或一次性 received-admission 描述、可选原 BudgetReservation，以及一次性原生生命周期锚。工作不能读取/发布 payload，不能执行 fill 或 SDK invalidator。

`ServerShmResponse` 和 `PeerShmRequest` 成为最低层释放义务载体；裸 ResponseData/RequestData 被丢弃也由此受保护。ReassemblyBacking 把原 handle 与 reservation 一起交接。RequestBlock 继续由原 Armed/Dispatched/Released 事务决定何时可以交接，不增加第二套请求 phase。

生命周期锚只持原生 counted-entry pin 和 tracking guard，不接收用户回调。把现有 ClientLease 的 PoolReferences 归还权放入共享的 counted pin；克隆同一个 pin 只共享既有归还义务，不按 clone 次数增加引用计数。pin 只持计数/时间戳状态，不持 SyncClient、ClientPool、Core ClientConnection 或 Runtime。正常 Held 继续按原方式保留具体连接的生命周期；失败对象仍活着时保留原连接与 lease，只有最终移交清理时才把纯计数 pin 留给原生 work，避免 work/quarantine 反过来阻止 Runtime/缓存 teardown。显式 Runtime close 仍可关闭连接而保留真实 payload pool。

原 `ClientLease::into_client` 的显式归还契约保持：取得显式 counted reference 后释放它自己的共享 pin，不能把整个共享 pin 标成 disabled，连带取消尚存 native work 的归还义务。节点的 pin 附件是现有共享原生对象的 Arc 克隆，最终 Drop 不为它分配 Box/执行闭包。

锚随实际清理工作保留，完成后一次归还。pool 内清理记录不得反向强持 pool/Runtime/Client；执行器的 queued/in-flight 工作才强持实际 owner。隔离记录可持上述纯计数 pin 和 tracking guard，原 pool teardown 时在 backing/reservation 之后归还；它们没有反向运输引用。生命周期附件只在 Core/native 装配到真实 release work，不形成独立 anchor 工作或 SDK 自定义回调接口。

将现有 ResponsePool 的“固定 owner、只读映射 guard、raw mutation 明确 unsafe”能力下沉为 `c2_mem::MemPoolOwner`，正式载体共享这一个不可替换的 owner。它封装原 MemPool 和现有锁，不另建 allocator。客户端 ServerPoolState 改持此固定 owner，载体捕获该 owner，而非到 worker 中查客户端当前的 Option；reconnect 仍创建新 incarnation，旧载体保留旧 owner。原 Arc<Mutex<MemPool>>/Arc<RwLock<MemPool>> 的正式可替换槽不再作为 native carrier 的公开输入面，不同时维护两套绑定方式。

每个 owner 的归还/隔离状态只保存实际清理内容，不反向强持 owner。排队/执行的 work 强持 owner；终态拒绝时把 handle/reservation/tracking/纯计数 pin 移入该 owner 的 quarantine，再撤掉 work 的 owner 引用。这样最后实际 owner 能自然销毁 backing，然后销毁隔离内容。raw MemPool 的手动 destroy 保持原行为；新 owner 的安全 API 不开放替换/销毁活池的入口，unsafe 原池访问明确禁止这样做。该边界继续拒绝旧 generation 和错误 owner，不给旧载体增加访问新池的能力。

### 准入与容量

交接空间和执行器可用性在 allocation/received-admission 时准备，不能到 Drop 才分配槽位、申请预算或创建线程。服务监听/IPC 连接在允许资源帧前建立 worker 存活预留；本地 allocation 前创建空清理节点，接收循环在读取可能携带 SHM 的下一帧前准备接收节点。已收到的唯一描述符必须先移入该节点，不能在交付/取消后重新申请接管空间。预留随活载体/记录延续，worker 不因暂时空队列而提前退出。

保留原 dedicated 的 256 个 publication permits 及最多 2 个 worker；普通释放工作不挤占该 256 个许可，也不另设一个隐藏的“最多 256 个 Held/响应”配额。普通清理节点在原资源准入时一对一预留，活载体→待清理工作是移动，同一实际资源不能同时占两份。队列不接受独立于真实 owner 的任意工作，不生成 payload 副本。

这是随真实资源数量增长的元数据，不是新的进程级固定内存上限。peer mappings 原本不向接收方的 owner SHM cell 计费，本项不伪造这笔计费，也不把原预算改作通用元数据预算。本方案明确选择不新增独立的元数据配额 M；因此普通释放的队列总数可能超过 256，只能声明每个尚未结束的真实资源至多一节点，不能声明固定总队列内存或整个进程 RSS 上限。原生退休器的线程数仍固定，Drop 交接不分配新节点，dedicated 自身的固定许可上限保持。这一选择避免为修复释放而新增业务准入拒绝和接收背压。

预留不是口头许可：在原 executor 锁内，按所有活预留（活载体、queued/in-flight、隔离项以及当前接收空节点）的总数，提前用 `VecDeque::try_reserve` 保证最坏同时交接的 ready-queue 容量，再发放 ticket；不是只按当前 queue.len 预留。具体清理节点在准入时就存在，移入 quarantine 也复用它的链结，不到 Drop 再扩容。ticket 只在义务结束后归还；现有 GC/退休维护可将多余容量收敛至全部当前活预留所需容量，不得按 queue.len 缩掉将来交接空间。最后一个预留结束、worker 依原空闲规则退出时回收空存储。pool/执行器锁内只更新这些元数据，不做 free、I/O、回调或附件析构。

`MemPoolOwner::new` 只建立固定所有权，不创建线程、不引入消费式构造失败。worker/node 的可失败准备在资源创建/接收之前进行；失败不移动现有 pool/handle。已经接到的 carrier 持原预留，不受后续接收或关闭的准备失败影响。

### 进度与失败分类

- 显式 release 失败：原调用对象继续持有，调用者可以再次尝试；最后 Drop 交接后，由既有执行器继续尝试。
- 活 pool：allocation/admission 前、现有 GC 和 close/retirement 入口先处理已接管记录，再判断新预算，避免预算饱和阻断回收。
- 无 timer、连接已关或最后外部 pool 引用释放：退休执行器持原 owner，继续推进同一释放及 GC，不依赖下一次业务调用。
- 有界共享锁竞争：只重试同一原分配；每轮有界且公平，不能让一个 backing 阻塞全部退休工作，也不能重启业务请求或扩大原调用期限。
- poison、dead-holder、incarnation/generation/allocation_id/span 拒绝：不重置、不偷锁、不自由重试。移回原 pool 的 quarantine，记录不含 pool/Runtime/Client 强引用；执行器撤掉自己的 pool 引用，原 pool 因而能够真实 teardown。其他真实 owner 仍活着时记录及计费保留；原 backing 结束后才归还本地 reservation、tracking 和纯计数 pin。peer 的生产者计费仍由生产者决定，不能从接收方 unmap 推出退款。

原 worker 改为公平短步：锁外执行一条释放/GC 步骤，未完成则将同一节点放回队尾，复用现有 Condvar 的有界定时等待。一个 dedicated 的 read_done 等待或一个 contended pool 不能长期霸占 worker。接管步骤不再调用 ensure_workers_locked；单个 allocator unwind 在 worker 边界保住同一节点，按 typed poison 处置，原 worker 继续处理下一项，不让单项 unwind 逃出线程再寄望 Drop 重建。worker 只有在所有活预留结束后才按原空闲规则退出。该捕获不吞业务 panic、不重新发布请求。Contended 不以尝试次数或等待时长耗尽作为“已释放”，原调用期限仍只约束调用方。

这里无 timer 指关闭 Tokio maintenance/time driver 后仍有进度；复用已有原生 Condvar 定时重试。shared unlock 没有跨进程通知，不能额外承诺完全无轮询。原 dedicated 工作也只以实际 entry 退休为结束，不能 elapsed bound 到期就放弃最后原 pool 引用；read_done/crash-timeout 的政策决定仍完全由原 MemPool GC 作出。

保留 `SpinlockError` 的类型直到原生释放策略边界，只有 `Contended` 允许恢复性重试；invalid free 与锁拒绝分开。错误文字只在展示/Python 投影时形成，不能解析 String 决定策略。读取原计数或 is_poisoned 也不能替代本次释放的结果。

## 5. 消费式出口和 Prepared

消费式 API 可以保留原返回形状，前提是失败前已把待清理的真实载体交给上述原生 owner；否则不得只返回 String 后丢掉它。copy 成功而 release 失败仍返回错误；copy/release 双错误按原顺序组合。

替换范围包括 `ResponseData::into_bytes`、两类 lease 的 `into_owned_bytes`、Core Held 的构造失败、Rust FastDB `open_held` 失败、RequestPreparationError 的最终析构，以及 Prepared 的创建/填充/回滚/取消。

Prepared 在调用 fill 前即绑定原 pool 并安装 guard。未开始 publication 的失败只能清理，不恢复成可发布块。成功发布 buddy 时显式移交跨进程释放权；不能由通用 Drop 再本地 free。发送取消/部分写必须按原帧提交边界判断，不能将 write error 一概视为“未发送”。dedicated 继续使用原 freed_at/read_done/GC 语义；待映射退休期间原 backing charge 保持。

具体复用 `LocalWriteHalf::write_all_with_timeout` 下的 `write_complete`/AbortOnDrop：增加 `write_all_with_progress(bytes, &mut frame_written)` 窄入口，共用同一个写入、原 timeout、flush、OS 错误保留和取消中断实现。布尔值在 write_all 完整完成、flush 之前置 true。Prepared 的局部 guard 借用该事实：writer 等待/未完整成帧的取消沿原 abort 后清理；true 后即使 flush 错误/取消也只能按已发布移交，不能重新本地 free。不在 LocalWriteHalf 上保存“最后一次写”的长期状态，不复制第二套 writer，不更改普通写入调用者的接口。

接收端当前先读完整 payload 再 admit SHM，因此整帧完成是明确的发布界线；两端原帧字节顺序和请求发送后的不重试政策保持。已写出的帧在接收方尚未 admit 前断开，仍不能凭发送方超时推导远端已放弃；这一原有 publication 不确定性沿原 backing teardown 规则，不属于已准入 carrier 的 Contended 恢复保证。

trailing-inline request 在分支前建立实际 PeerShmRequest，复用同一 copy/release 边界，删除吞错旁路。已交付响应不塞回 pending waiter；现有 pending 仅管理其原来的未交付响应及 dispatched request。

## 6. Host 的关闭责任

每个具体 Host 的现有 host thread 在启动时取得 Runtime、Server 和同一 ShutdownTransaction 的强执行责任，直到写入并通知最终 Core outcome。线程不强持 HostInner；HostInner、failed-start receipt 和 Runtime retirement 只观察/请求同一事务。

预建事务由已有 Server shutdown watch/生命周期区分“准备完成”和“已请求”，不另建一套 Server 生命周期。更新 shutdown_pending/ensure_no_pending_teardown 的判据，不能因预建 outcome=None 而阻塞正常注册。

- 最后 Host Drop：只发幂等请求，走原同步 admission/关闭信号；不创建 Core 关闭线程/runtime，不等待 Condvar，不 join 自己。
- 显式 Host shutdown：请求相同工作，在原绝对 deadline 内观察；pending 不写终态，不取消 driver。回调/host 执行上下文不得同步等自身 drain。
- Persistent、OwnerBound、pre-ready 拒绝、bind 失败和 readiness 超时：现有 host thread 最后进入同一个 Core 收尾段。不能丢弃仍持回调和 route journal 的 run future。
- Thread 已接管后的失败启动：启动 caller 只请求并观察同一 Core receipt，不抢先消费 Server route journal；原启动错误保留。
- Thread 尚未创建的失败：没有活 Host/listener/route，无后台关闭接收者；不制造成功 cleanup receipt，也不关闭 Runtime 以前已有的 outgoing clients。该现有失败语义变化必须经用户确认。

预建事务在 host thread 成功建立后的原 Core 发布临界区登记，线程先经过同一 fence 再执行，失败不留下没有执行者的 pending 条目。该事务在 Starting/Ready 时只是准备好的 journal，Server 的现有 Stopping/终态与 shutdown watch 表达关闭请求；不添加 Armed/Requested 的第二套生命周期。

迟到首次 run 复用现有 run_entered 和 admission 锁：本 attempt 已进入 Stopping、且 run 尚未进入时，不再调用 begin_start_attempt 或 bind。运行入口及 Ready 发布都在同一 admission fence 下核对关闭事实，不能在启动 caller 到期后复位关闭信号或短暂重新发布 Ready；已运行实例的重复 run 仍按原错误拒绝。不新增公共 attempt token 或另一套 run 入口。

线程复用已有 server runtime 驱动原 Server completion 和 Core 收尾；本项不预设更改公开 `Server::run` 签名，也不承诺移除 Server 内所有已有 blocking I/O。现有具体 listener 的 close/端点错误继续经 Server 发布；本地清理失败必须保留实际失败，不伪造 Stopped/safe-close。

收尾遵守原路由统一关准入→回调 drain→accepted connections/GC/listener 关闭→Core 观察完成→精确 relay 撤回及 journal 的顺序。按具体 Server 的完整 registration scopes 收尾，不单凭 Host.route_names 或名字推断成功。route/relay 已确认副作用不重放。

Pending retirement 在原 Core 锁序内将 seed 和封存范围交给同一 host driver；caller 到期后责任仍在线程。终态 Host 后新产生的 outgoing scope 及 ClientsOnly，继续用已验收的“显式准备成功后才 seal”的 Core driver，不让旧 host thread 常驻待命。

保持 Core 状态→pool state/close transaction→outcome 的锁序。工作和回调、网络/端点清理都在锁外执行；driver 发布 outcome 后再释放其 Runtime/Server 持有。旧 journal 与 completed/safe-close 的区别保留。

## 7. 对外影响

| 面向 | 方案中的影响 |
| --- | --- |
| Python `hold`/HeldResult、Runtime/shutdown | 调用签名保持；重复 release 可继续原生清理但不重放 invalidator，失效后不能访问 value。失败启动在 host thread 未创建时不再顺带关闭旧 outgoing clients。 |
| Rust Held/Core Held | 保留 release 和 consuming 的主签名；增加 `has_pending_release(&self) -> bool` 只读投影，供桥接及调用者区分逻辑失效和物理待清理；connection pin 归还按真实释放完成。 |
| 低层 Rust allocator/pool | `BuddyAllocator::free`、`MemPool::free/free_at` 的错误改为 `ReleaseFailure`；它区分 `Allocator(SpinlockError)` 与 `InvalidAllocation(String)`。`ReleaseHandleError.reason` 同步改为该类型，仍携带原 MemHandle。Display 保留原上下文与错误顺序；不保留旧 String 返回入口。 |
| 原生载体和退休代码 | 下沉已有 ResponsePool 固定 owner 能力，形成不可失败的 `MemPoolOwner::new(MemPool, ReleaseCleanupContext) -> MemPoolOwner`；ReassemblyBacking 准入参数改为该 owner，取代可替换 Arc 锁输入。IpcClient/SyncClient 的注入 MemPool 所有权入口保持，由其内部封装。raw received-admission 桥保留唯一释放义务的 unsafe 契约；worker/node 准备失败走原 allocation/connection 的错误返回，不消费既有 owner。退休器下沉及相关低层 Rust 输入迁移不留旧 owner 入口。 |
| c2-local 写入 | 新增 `LocalWriteHalf::write_all_with_progress(&mut self, bytes: &[u8], frame_written: &mut bool) -> io::Result<()>`；只是原写入实现的完成事实投影，普通 write_all/with_timeout 仍各有其现有用途，不作为旧 Prepared 兼容分支。 |
| raw `MemPool::destroy` | 保持原手动 API 及旧句柄 generation 拒绝；正式 native carrier 使用固定 owner，其安全面不能 replace/destroy 活池。原池的 unsafe 访问契约与 compile-fail 覆盖同步迁移。 |
| 内存观察 | `c2_mem::ReleaseCleanupStats { pending: u64, failed: u64 }` 为 Copy 快照；`BudgetSnapshot` 和 Core `MemoryScopeStats` 增加 `release_cleanup`，Python 原 memory stats 的各 scope 增加同名 `{pending, failed}` 对象。pending 计恢复性待处理，failed 计拒绝修改且仍持有的隔离义务；一次只归入一类。weak observer 只持计数，不能成为 cleanup owner；错误本身沿原 Result/原生日志报告，不建错误历史队列。 |
| Host/failed-start receipt | Host 驱动与请求/观察责任变化；Host 未创建的错误不再交付声称已做 Core cleanup 的 receipt。Rust SDK 和 Native 仍暴露相同主入口，不新增第二套 close API。 |
| 协议及 ABI | IPC 14、SHM 3、C/FFI ABI 6、业务帧、CRM hash、属主协议及端点/凭据格式均不变；不新增旧格式兼容。 |

inline/owned Held 仍保持原 pin 生命周期，不借本项提前触发其 idle eviction；最终移交给后台的清理 work 不持整个 Core connection 的配置/连接权威。失败释放后的 retained 统计跟随真实原生清理义务，可能在业务值已失效后仍非零；这是修正后的可观察失败语义，不代表还能读取 value。

统计归属在 owner 创建时固定：`MemoryBudget::release_cleanup_context()` 返回只含同一 scope 元数据的 `ReleaseCleanupContext`，不暴露 reserve/free；Runtime outgoing 和 Server 各自把它交给本 scope 的所有固定 owner。peer owner 同样取得接收方的清理统计上下文，但其 `MemPool::budget` 仍为 None，不增加接收方 SHM backing 计费。活 ticket/隔离记录保持该纯元数据状态可观察，既有 retired weak observer 不会因 payload cell 为零就漏掉 pending；不能为此让 observer 强持任何 pool/client/runtime。上下文绑定是 Rust 内部域归属，不是新用户配置项。

## 8. 改动量与维护成本

以下为静态范围预算，不是实施结果。退休器的代码移动应单独统计，不把移动行数计成净减或性能收益；最终按生产、测试、文档分别报告 numstat。

| 范围 | 替换内容 | 预计净增 |
| --- | --- | ---: |
| c2-mem/c2-wire/c2-ipc/c2-server | typed 释放、具体接管、原退休器复用、peer/reassembly/Prepared/consuming 归一 | 500–800 行 |
| Core、Rust/Python SDK 与 Native | Held 的失效/释放、pin/tracking 迁移、Host 唯一 driver/回执、观察投影 | 300–500 行 |
| 现有测试及必要测试 seam | 替换假成功/提前清空断言，补最终 Drop 和关闭责任反例 | 1100–1800 行 |
| 文档 | 本方案、API/生命周期/内存说明、未来发行说明与验收记录 | 180–280 行 |

预计生产净增 800–1300 行，测试净增 1100–1800 行。这是正确性补强，不能当作瘦身收益。若独立审查发现必须新增机制或生产增量超过该范围，应先重新收敛方案，不能借实施之名继续扩张。

成功路径不新增 payload 复制或网络请求，但会增加有限接管元数据/准入准备及原生生命周期锚；第一次需要真实 SHM 清理责任时可能启动已有退休 worker。失败工作保留的容量不提前退款，预算饱和是可见代价。确认实施后必须做同环境成功小调用、SHM/chunk/hold-release 和失败竞争的性能/内存对照，报告实际开销，不声称本项提速。

## 9. 实施顺序与验收

方案确认后分三个可审查步骤，全部完成后才作为发版前置项验收；中间不能宣布 R07 已闭合：

1. 在未改生产的基线补反例：Core/Rust/Python Held 的释放失败后重试、consuming 双错误、最终载体析构和最后 peer owner、Prepared fill/panic/取消，以及 Host 的线程/runtime 创建失败。先证明反例有效。
2. 实施原生接管与 typed 释放，随之迁移 Core/Held/consuming/Prepared；真实身份及预算断言保留，移除被取代的逐层 best effort、forget 和吞错测试。
3. 实施 host thread 的单一关闭责任及 receipt 合并，完成联合针对性/完整回归、同环境成本对照和独立代码审查，再提交验收记录；不发版。

针对性验收必须覆盖：

- contention 持续跨过原 spin budget，第一次失败保留同一 allocation_id/generation；释放竞争后，最终 Drop 已交接的工作无需新业务调用也能归零。重复释放不 double-free、不双退预算。
- no timer、预算饱和、connection 已关、reconnect 换池、最后 Held/pool owner；旧 raw pool destroy 的 generation 拒绝，以及固定 owner 无法从安全 API 替换的 compile-fail。错误 pool 永远不能领取旧记录。
- poison/dead-holder 不改锁字；隔离期间 charge 保持。实际 pool teardown 按映射/文件→reservation→tracking/pin 的顺序收尾，不能伪造对端退款。
- invalidator 错误与 panic 一次；copy/release 双错误完整；active export 拒绝在前、tracking 不复活。成功释放、失败再释放和最终析构都保持快照。
- RequestBlock 的 armed / dispatched buddy / dedicated 分别验收；接管工作不能发布或重放填充，dedicated 原 256 许可和旧保留超时继续成立。
- Prepared 在填充错误/panic、copy fallback 错误、writer 等待取消、部分发送/发送后取消、成功发布下逐项验证，不能在对端仍有释放权时本地 free。
- Host 最后 Drop 和 host/callback 线程上最后释放不自等；已接管后注入关闭 worker/runtime 创建失败仍由原执行者收尾；线程创建前失败不影响旧 clients。
- startup timeout 后迟到线程不重新 Ready；duplicate bind 不删其他 listener；OwnerBound 的拒绝、接管、宽限与退出原因保持；relay final publish 与 shutdown 竞争按完整 scope 精确收束。
- 原 receipt 的 pending/终态错误/成功三种事实，以及 retirement 升级在 publication 前后两种胜负；不能重放 Server journal 或 relay 副作用。

平台范围：本机 macOS、隔离 Linux、Windows 开发机真实 Named Pipe/SHM/file；Windows 优先开发机。联合完成完整 relay、Rust/Python 18 行及 Node 12 行真实跨语言矩阵；对精确实现候选运行 GitHub Actions 的 Linux、macOS、Windows 2022/2025、Windows ARM64、Python 3.14t 适用门禁。Rust API 改动按破坏性更改验收，保留源 SHA/run URL。

每个隔离 fixture 的 listener/socket/lease、SHM 映射和 spill 文件都做零残留验收；永久拒绝场景只处理本批独立资源，不对历史命名空间做 reap/扫尾。验证完成即清理本批 target/env/临时工作树，保留文本证据。

## 10. 本轮交付状态

本轮完成基线源码核查、既有证据复核、方案及独立审查。Host 责任调查 run `cf23489f-e0d1-4658-9fe7-cb790e19d3f0` 和内存接管调查 run `45974a84-6b84-46de-861e-4c626ddd04db` 的事实已由 Host 核查；候选方案不是这些 worker 的实施授权。

最终独立审查使用 hey-my-buddy run `fa5bf0dc-ef64-4cca-8e31-eb2ba202c378`：先审查固定 Git 文档 `cc212251` 和生产源码 `ba6179a9`，再复核 `155beafe` 的 8 增 4 删澄清。两轮结论均无方案级阻塞；审查指出生产 800–1300 行预算可能靠近上沿，具体实现仍须以反例、成本数据及完整门禁验收。worker 未修改任何文件，固定输出为空补丁；这不是测试通过记录。

原最终审查的首轮在 buddy validation 队列停留、没有启动 native attempt，已保留其取消事实；同一 run 恢复为可用的独立审查者后完成上述两轮，没有将排队或取消当成通过。所有本批调查/审查已接受并回收 checkout，仅保留文本记录和固定 Git 引用。

没有实施、没有新性能数据、没有新三端/CI 通过声明，也未发版。原生进度保证限定在已取得释放义务的载体及可恢复竞争；永久拒绝与未准入 publication 不确定性按本文明确的边界报告，不能借“R07 后续完成”扩大为任意故障下逐分配均可回收。
