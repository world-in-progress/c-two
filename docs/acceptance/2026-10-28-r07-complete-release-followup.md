# R07：失败释放与 Prepared 发布界线

状态：用户已确认，已实施并于 2026-10-28 验收关闭，实现与测试来源 `000985b6417714df80e88fe56c50b1e5445853b8`，见[验收记录](2026-10-28-r07-failed-release-acceptance.md)；实施中的两处修订见第 10 节。未发版。生产基线为 `ba6179a937d26419cf1d0bf28e0d74ba5867c256`，规划输入 `d8ae4e76` 的生产代码与它相同。R07a 的四处低层失败返还及显式 request/response lease 保留已验收，不重复实现。

## 1. 处理边界与关闭标准

显式释放失败，原持有者保留原 lease/handle、原预算和释放责任，可以再次调用；失效只尝试一次，后续调用只重试原生清理。最终 Drop 或消费式出口不能再把责任返回持有者时，释放失败则保守放弃：返回或记录错误，不修改未经确认的分配，不退尚未释放的预算，每个实际放弃的释放义务计一次 `abandoned_releases`。不自动恢复，不建立错误历史。

`ShmSpinlock` 当前默认预算是 1000 万次迭代，临界区处理分配器位图；现有竞争反例使用人为持锁，没有真实使用事故证据。`Contended` 仅说明本次预算内未获得锁，持锁者可能存活、状态未知或 PID 已复用；也可能一直输掉 CAS 竞争，不能据此保证以后会恢复。poison 或已死持锁者使整段持续拒绝分配与释放，在属主进程退出前该段不可用。永久拒绝不偷锁、不重置锁字，也不把 `Contended` 变成无限重试策略。既有 GC 继续处理它原来负责的 backing，不负责恢复这些失败块。

本批验收后关闭 R07，条件是：释放失败不报告成功；已发布或可能被对端使用的块不会被发送端提前释放；失败要么保留给显式持有者重试，要么保守放弃、不退预算并计数；永久拒绝不改变共享锁字。放弃计数为零不证明不存在所有内存问题，非零也不等于仍泄漏相同数量的块。

最终 Drop 的后台接管决定不做；只有真实使用中的 `abandoned_releases` 出现非零，再重新评估。Host 析构另列：现有 Drop 创建关闭线程失败时保留错误、服务继续运行；本批不改变该责任及资源耗尽策略。R35、C04 与两份控制会话重构继续暂缓。

## 2. 本轮基线证据

macOS 隔离副本运行四项反例，全部在预期安全断言处失败，未应用生产修复。[固定收据](evidence/2026-10-28-r07-narrow-baseline/receipt.json)、[仅供隔离探针的补丁](evidence/2026-10-28-r07-narrow-baseline/baseline-test-only.patch)、[Prepared 日志](evidence/2026-10-28-r07-narrow-baseline/baseline.log)、[Held 日志](evidence/2026-10-28-r07-narrow-baseline/held-baseline.log)保留源码、命令输出和 SHA-256。

| 反例 | 实际结果 |
| --- | --- |
| 整帧 native write 完成，随后 flush 返回 BrokenPipe | 对端收到完整回复，携带 allocation id 67；发送端仍本地释放，活分配由 1 变 0；下一分配 id 131 复用相同 segment/offset。 |
| 整帧 native write 完成，flush Pending 到原写入期限 | 对端收到完整回复；推进 Tokio 测试时钟使原 30 秒期限到期，无放宽期限；同样本地释放并复用地址。 |
| Core Held 第一次释放被原 pool 身份校验拒绝，恢复原 pool 后再调用 | 第一次报错，第二次返回成功；原块仍在，512 字节重组预算仍计费。 |
| Core invalidator panic 被调用者捕获后再次释放 | 用户回调执行两次，未满足只失效一次。 |

Prepared 反例通过真实 `LocalStream::pair()`、原 `write_complete` 和原 `send_prepared_shm_response`；临时编译期夹具在 `LocalWriteHalf::poll_flush` 注入错误/等待，并在 pair 中显式传入本批独立 context；不修改 write、发送或释放逻辑。Windows 的实际 `poll_flush` 使用零字节写等待此前的 overlapped write 完成，因此“整帧写完”与“flush 完成”确实是两个边界；本次 macOS 故障注入证明代码缺陷，不冒充 Windows 自然故障实测。Held 反例受控替换并恢复原 pool，只验证上层失败保留；不把它当成真实使用中的争锁事故。

Core 探针首次构建遗漏编译时包含的合同夹具，补入同一提交的原文件后执行；失败日志保留，未改断言或运行时逻辑。初轮 Prepared 误把 `C2_IPC_ROOT` 当成 pair 的隔离设置，实际访问了默认命名空间；该次临时端点由原生 pair close 回收，没有手动清理历史文件。此轮记录不作为隔离验收。已修正为显式 `with_unix_root(...).endpoint(...)` 并重做四项反例，结果相同；最终任务根只余 `.gate/.gate.marker`，socket/lease 均为零，原 `write_complete` 与发送函数 SHA-256 未变。初轮日志及更正均随收据保留。Linux、Windows、完整 relay、跨语言和修复后回归留待实施验收，不把这些红色反例算作修复通过。

## 3. 单一 scope 计数

在现有 `MemoryBudget` 的共享 accounting state 中增加一个饱和 `u64` 计数，更新沿用原 accounting mutex；通过原 `BudgetSnapshot` → `MemoryScopeStats` → Python memory stats 链投影，不增设 tracker、scope 注册表或独立 observer。Rust 两种 snapshot 都增加 `pub abandoned_releases: u64`，计数写入口为 `MemoryBudget::record_abandoned_release(&self)`，Python 每个 scope 在 `limits`、`cells` 同级增加 `abandoned_releases: int`。不增加 `pending/failed` 或逐条历史。

计数的事件是“本进程放弃一个仍不能确认已完成的释放义务”，不是一次失败尝试。显式失败仍保留义务时不计；成功释放、正常移交给接收方时不计；最后出口失败或发送完成后结果不明、需要保守放弃时计一次。对端之后成功释放或 pool 真实 teardown，不回减历史计数。两端各有独立 scope，不跨进程汇总或推断重复泄漏。

最低层实际载体负责最后一次计数：重组由 `ReassemblyBacking`，接收 SHM 由 `ServerShmResponse`/`PeerShmRequest`，prepared 由 `PreparedShmResponse`，预分配由最后一个 `RequestReleaseState`。上层 lease、Core、SDK 和异常清理不为同一块重复计数。载体若在消费错误路径主动放弃，必须清空自己的义务，嵌套 Drop 不再尝试或再计。日志复用现有 tracing/warn 约定，记录原失败及 backing 身份；不缓存错误、不新增 reporter。accounting lock 内只更新数字，free 和日志在其锁外；pool 锁内不析构 SDK value、具体连接或 tracking 附件。

owner pool 直接使用既有 budget；peer pool 仍不承担生产者的 SHM backing 计费。接收载体在实际准入处捕获所属客户端或服务器的现有 `MemoryBudget` 克隆，供本端放弃计数；这是同一 accounting state 的引用，不是新的预算、资源 owner 或清理上下文。旧载体跨关闭、重连时仍记入原 scope，不从新连接当前槽位推断归属。

重组放弃继续扣住原容量，不把 trimmed 长度当退款额。用窄的 `BudgetReservation::abandon(self)` 消耗原 guard、放弃退款并计一次；其实现不保留 payload 或错误对象，不再靠 `mem::forget` 永久泄漏 accounting Arc。scope 尚有 owner 时原 `used_bytes` 不变；最后真实 owner 消失后，沿用原弱 observer 的自然脱离规则，不为累计计数延长 retired scope 寿命。该指标不承诺进程退出后可查，原生日志承担当时的失败记录。

`MemPool::release_handle` 保留 R07a 的原错误及原 handle 返回形状；raw 手动内存 API 的调用者继续承担自己获得的释放义务。传输/Core 的正式路径必须把失败 owner 留在原载体，或由它执行上述放弃；不通过给所有裸 `MemHandle::drop` 加自动 free/计数来干扰正常跨进程移交。

## 4. Core、Rust Held 和 Python HeldResult

Core 的既有 `released` 表示逻辑失效已尝试，lease `Option` 表示原生释放尚未完成。调用 invalidator 前先置位，Err 或 unwind 后都不能重放。借用 lease 清理，成功才清空；失效返回 Err 后仍尝试原生清理，首轮保留原错误顺序和双错误；Rust unwind 仍传播，lease 留给同一持有者后续清理或其 Drop。后续调用只处理原 lease，不再次执行失效函数。`is_released()` 继续是值访问状态；新增 `has_pending_release(&self) -> bool` 区分尚未结束的原生义务，不引入新状态机。

Core 具体 connection pin 在原生释放成功后归还，即使 invalidator 报错也不继续钉住；原生失败则原 Held 继续保有具体连接和 lease。最终 Drop 由原载体释放或保守放弃并计数，然后归还具体 pin，不建立计数 pin、后台锚或长期 connection 引用。

Rust `Held<T>` 拆开不可恢复的 `owners.take()`：值访问由 Core 逻辑失效状态决定，失败后保留原 response，重复 release 只做 native 清理。保留正常退出及 invalidator panic 时的既有 value/response 析构顺序。回调 panic 仍按 Rust 原 unwind 规则传播，不重放或转后台执行。

Python 正式 `hold()` 路径持有既有原生 response，不再把唯一清理责任藏在执行一次便清空的 closure 中；再次释放只调用该 response 的原生清理。手工 `HeldResult(release_cb=...)` 的任意用户回调仍只执行一次，不把它纳入原生重试。公开构造签名与普通 owned/pickle 路径保持。native 私有投影同时提供 `is_released` 与 `has_pending_release`，供正式 Held 分辨失效前拒绝、逻辑失效和仍待清理；不公开第二种清理机制。

原生导出校验仍在 `inner` 锁内先于失效及 lease 移除。边界因活跃导出拒绝时，不标记为已经执行 invalidator；正式 Python Held 根据 native 状态保留这次尚未进入的失效步骤。一旦失效已尝试，value/buffer 不可复活，后续只清理。PyCoreResponse 的 tracking 随真实原生义务保留：成功释放后归还；最终放弃时先计数、结束真实载体，再归还 tracking，不能让 active hold 被误报为成功释放，也不在放弃后永远留下虚假 active hold。自身 view 已 release、但其他导出使 native 拒绝时，不能把旧 memoryview 再当可用 buffer。

Python 双错误统一沿现有异常链保留主错误和本次 cleanup 原因，覆盖 Python 3.10；不再仅靠 `add_note` 成功与否来决定能否看到第二个错误，也不新增错误历史。原 invalidator 异常仍是主异常，不能覆盖原链条；FastDB 构造失败仍保留原 PayloadError，清理由原载体日志及计数报告。

## 5. 消费式出口和最终 Drop

`ResponseData::into_bytes`、两类 lease 的 `into_owned_bytes`、Core Held 拷贝失败、Rust FastDB 开启失败以及 request preparation 错误退出共用已有释放事务。copy 失败仍尝试 cleanup，copy/release 双失败保留原主次；copy 成功但 release 失败仍返回错误，不能返回 `Ok(bytes)` 掩盖失败。消费式返回形状保持，不添加“失败块仍可发布”的逃逸入口。

明确的最后出口使用消费式 `ReassemblyBacking::abandon(self)`，消耗原 state 后交给 reservation 放弃退款及计数；各 crate 内的 SHM carrier/lease 用私有 abandon 入口结束同一义务。消费出口已取得本次真实 release Err 时直接走 abandon，不让嵌套 Drop 再做一次自旋。直接丢弃活 owner 时，由最低层 Drop 尝试一次，失败后走同一 abandon。

最低层接收载体以现有 move-only owner 和一个明确的未完成义务保存状态；成功后解除，失败后保持。原 release、try_release、Drop 共用同一清理动作，外层 lease 不再额外兜一轮造成重复归还/计数。重组沿用原 state Option。没有存活持有者的最后一步失败就放弃，不等待未来业务调用、GC、timer 或后台 worker 来恢复。

无 waiter 的 `PendingResponse` 区分两种结果：本地 pool/permit 锁暂忙的 `Ok(false)` 保留原暂存/维护语义；实际释放 `Err` 必须退出该 pending 的恢复循环，不能继续每 5 ms 消耗共享锁的完整自旋预算。无人领取的 response 由原 carrier 明确放弃、计数并清空；request 只撤去 pending 的引用，存活 caller 仍持原释放事务，最后一个 `RequestReleaseState` 才进行最终结算。保留原信号、锁序及迟到回复的唯一责任，不加重试队列。

`RequestReleaseState` 沿用 Armed/Dispatched/Released、原 permit 与 pending 责任。发送后的 buddy 仍归接收方，不能因本地超时再 free；existing dedicated `freed_at/read_done/crash-timeout` 及 256 许可、两 worker 机制原样保留。只在最后原释放义务确实被放弃时计数，caller token 结束但 pending 仍持有时不计。`RequestPreparationError` 的失败块只允许继续清理，最终丢弃由同一原生载体结算。

## 6. Prepared 发布界线与 trailing-inline

复用现有 `write_complete`：加 `LocalWriteHalf::write_all_with_progress(bytes, &mut frame_written)` 窄入口，普通写与 prepared 共用同一 writer、AbortOnDrop、原期限、flush 和原生错误链。`frame_written` 是单次写的事实，在完整 `AsyncWriteExt::write_all` 返回后、flush 前立即置 true；LocalWriteHalf 不保存长期“上次写入”状态，不复制第二套收发逻辑。

Prepared 在 fill 之前绑定并持有原 response pool 的原锁与 handle。局部深化现有 `ResponsePool.inner` 为同一锁的 Arc，由其唯一生产 helper 把克隆交给 Prepared；不创建 `MemPoolOwner` 或另一种 allocator。Prepared 用 handle Option 表示唯一义务，显式 cleanup 成功后才清空。这样 fill 返回 Err、fill panic、frame 构造、等待 writer、未完整写入的错误/取消都由同一原 pool 尝试 cleanup；失败日志与原错误并存，无法返回时保守放弃并计数，不依赖回调再执行。

发送的局部 guard 同时持 Prepared 和 `frame_written`，传给公共写入函数的就是该 guard 的事实字段。这样 future 在 flush Pending 或错误传播途中被取消，析构仍能看到已经发布，不能等 await 返回后才更新 Prepared 状态。未完整成帧先沿原 AbortOnDrop 中断连接，再清理原分配；接收端仍先读取完整 payload 再准入 SHM。

整帧写完即移交 buddy 的跨进程释放权。之后 flush 错误、期限或取消不再本地 free，保留发送错误并按本端结果不明计一次放弃；对端可能已经或随后正常释放，因此该计数不叫“确认泄漏”。不发 ACK 查询、不重试业务调用、不重发同一 allocation。dedicated 在正常成功发送时保留原生命周期；在整帧已写但结果不明时同样禁止执行会提前回收对端可见存储的本地 release，保守保留原 backing/计费，不引入新的超时回收规则。dedicated GC 只扫描已有 freed_at 的条目；本分支未 settle 时，即使以后收到 read_done，也可能一直保留到原 pool teardown。不能把“可由原 GC 处理”写成必然退休承诺。

返回错误时不能用一次成功本地 free 覆盖发送错误；未发布 rollback 同时失败时保留初始错误和 cleanup 原因。正常写入已完成、随后 dedicated 本地 settle 失败也返回 cleanup 错误并计数，不以 `Ok` 吞掉；沿现有 Transport 分支记录，不再补发一帧 UserVisible 错误。最终 Drop/panic 没有 Result 出口，沿原日志报告 cleanup 失败并计数，不静默忽略。

`smart_reply_with_data` 只把 helper 的 `Ok(None)` 视为原 inline/chunked fallback。helper `Err` 原样转入现有 ResponseSendError；分配后的 fill/回滚错误不能被 fallback 擦掉变成成功。

服务端所有已解码、已通过 SHM 能力门的 Peer 请求共用同一个 `PeerShmRequest`：trailing-inline 的拷贝拼接、chunk 消费以及已拒绝/丢弃请求的清理一起收敛，删除 `free_peer_block` 把错误换成 `FreeResult::Normal` 的支路。复用握手保存的原 peer pool Arc，在首次可失败的映射/读取前建立唯一 carrier；能力门仍先于准入、映射或 cleanup，未协商 SHM 的伪造坐标不得取得释放权。

公开 `RequestLease::release() -> Result<(), String>` 保持；内部成功释放保留原 `FreeResult`，交给既有 `schedule_peer_buddy_gc_if_idle`，不为共享 caller 改公开返回类型。trailing-inline 通过同一 lease 拷贝、释放后拼接，chunk 消费错误不得继续按成功输入处理；拒绝/丢弃分支保留最初拒绝的主次，最终 cleanup 失败按原 carrier 放弃计数。移除 raw 旁路时同时替换全部调用点，保留 payload 字节、身份校验和 idle GC 原触发条件。

## 7. API、ABI 和协议

| 面 | 变化 |
| --- | --- |
| Rust statistics | `BudgetSnapshot`、`MemoryScopeStats` 新增 `abandoned_releases: u64`；使用 struct literal 的低层调用者需要更新，不留旧形状兼容。既有 memory stats 观察入口不变。 |
| Rust Core/Held | Core HeldResponse 新增 `has_pending_release()` 与消费式 `abandon(self)`；ResponseLease / RequestLease 新增消费式 `abandon(self)`，SDK Held 的 release 签名不变；显式失败保留、再次原生清理替代原先失败后空成功。 |
| Rust memory/local | `BudgetReservation::abandon(self)`、`ReassemblyBacking::abandon(self)` 和 MemoryBudget 的窄计数入口；LocalWriteHalf 增加共用 writer 的进度入口。无 typed allocator 错误改造，无裸 owner 全面重构。 |
| Python | `MemoryScopeStats` TypedDict 及各 scope 返回值增加一个字段；正式 Held 的失败后释放可以再次尝试，用户回调不重放。公开 HeldResult 构造、hold/release、memory_stats 入口签名不变；native 私有投影同步状态。 |
| ABI / 协议 | C/FFI ABI 6、IPC 14、SHM 3、业务帧、CRM hash、owner-control 与凭据格式全部不变，不加能力位、开关、旧格式兼容。 |
| 生命周期 | Host、Runtime、listener、relay 关闭事务不改；pin/tracking 在成功原生释放或最终放弃时结束，不把逻辑失效等同物理释放。 |

跨 crate 的最小终结链明确新增四个消费式 Rust 入口：`c2_wire::chunk::ReassemblyBacking::abandon(self)`、`c2_ipc::ResponseLease::abandon(self)`、`c2_server::RequestLease::abandon(self)`、`c2_core::HeldResponse::abandon(self)`。各上层入口只消耗原 Option 并向下调用同一个 carrier 放弃，不 free、不重试、不调用 invalidator，也不返还 handle 或 payload。它们属于低层 Rust API；高层 Rust Held 与 Python HeldResult 不增加公开 abandon。SHM carrier 的可更新 release/try_release、RequestLease 保留 FreeResult 等辅助方法仍留在所属 crate 内。

Core `from_response_lease` 的 copy 失败按 copy→一次 release→失败则 ResponseLease::abandon→返回原 copy/release 两因的顺序结束；不能 drop 一个仍 armed 的 lease 再触发一次释放。Rust FastDB 开启/消费失败的 SDK 清理，以及 PyO3 构造/解码消费错误的原生清理，在取得本次实际释放 Err 后转调 Core HeldResponse::abandon；最底层先计数并结束义务，再归还原具体 connection 和 tracking。原 PayloadError/Python 主异常保持，cleanup 原因沿已列错误链或原生日志报告。PyO3 仍复用 inner/export 边界：活跃导出拒绝没有进入原生 free，不能强行取走 inner；其导出者继续持原 holder，最后真正结束时才执行 native Drop。没有新封装 owner、第二套事务或后台执行器。

CoreCallback 的请求方向同样使用 `RequestLease::abandon(self)`：copy 失败先 release 一次，若也失败则终结原 request Option，再按原 protocol_violation 返回两因；service.invoke 期间继续保留原 request lease，调用后 release 失败则先 abandon，再按原错误主次返回，不提前释放、不增加输入拷贝。此处是资源回调的消费出口，不涉及另列的 Host 析构或关闭事务。

## 8. 替换范围、规模与成本

| 生产范围 | 替换内容 | 预计净增 |
| --- | --- | ---: |
| c2-mem / wire accounting | 一个 scope 计数、reservation 保守放弃、重组末端计数与日志 | 35–65 |
| c2-ipc / c2-server carriers | 原生义务保留、最低层 Drop/消费出口统一、Prepared 原锁复用、尾随参数旁路删除 | 170–280 |
| c2-local / Prepared 发送 | 单次整帧事实、取消 guard、成功/失败的 publication 收尾 | 65–100 |
| Core / 两 SDK / native 投影 | 失效仅一次、具体 pin/tracking 保留、Held 原生重试、stats 投影 | 130–205 |
| 合计 | 约 18–20 个生产文件；同一函数的搬移不计精简收益 | 400–650 |

测试预计净增 650–1000 行，以复用现有 pool、Held、memory stats 与真实跨语言夹具为主，合并吞错/错误成功的旧断言；不为每层再造模拟释放框架。文档及验收记录预计 140–220 行；历史基线的文本补丁、日志、receipt 单列为证据，不算生产实现或性能收益。此项属于正确性与可观察性修复，不声称净瘦身；实际超出区间须先重新核实原因，不能自动扩大范围。

成功路径成本限定为 Prepared 对原 pool 的 Arc 引用、必要接收载体对原 accounting state 的 Arc 引用、已有 Option/状态检查；计数及告警只在最后放弃分支更新。没有新 worker、队列、准入 ticket、额外配额或跨层错误类型体系。实施后对同环境小包、SHM、chunk、Held 成功释放做窄 benchmark，报告具体开销，不承诺性能收益。

## 9. 实施后回归与验收

先把本轮四项红色反例转成有效回归，再按 native carrier → Core/Held → Prepared/尾随参数的顺序修改；各段共用本方案的最终验收，不靠重复全套测试堆证据。

- 释放与统计：原分配成功/失败/显式再试/最终放弃，copy 与 cleanup 双错误；同一块嵌套 Drop 只计一次，PendingResponse 的暂忙/真实错误分流及 no-waiter 收尾；真实多 scope、关闭/重连后的旧 scope、退休观察、不改 byte cells；原生 raw owner 手动契约不变。
- Held：invalidator Ok/Err/panic、先失效后释放、失败 lease/pin/tracking 保留、成功后各归还一次、最终放弃后不留下虚假 active hold；活跃 memoryview 拒绝与借用快照、Rust panic 析构顺序及任意 Python release_cb 不重放，Python 3.10 与新解释器的主异常/cleanup 异常链齐全。
- Prepared：fill Err/panic、校验/回滚双失败、writer 等待取消、部分写、完整写后 flush Err/期限/取消；对端完整帧的分配不能被本地复用；buddy/dedicated、正常 read_done/GC、未收到帧的不明结果计数；helper Err 不进入成功 fallback，trailing-inline/chunk/拒绝丢弃的全部 Peer 路径及 idle GC 条件保持。
- 永久拒绝：原 1000 万次预算的代表性竞争、poison/dead-holder/PID 复用判断；锁字及 allocator 元数据不改、无后台恢复、预算饱和和无 timer 不隐藏退款。既有 read_done/crash-timeout GC 不额外改造。
- 平台：macOS 本机 UDS/POSIX SHM，隔离 Linux UDS/POSIX SHM，Windows 开发机优先验证 Named Pipe、真实 overlapped flush、named mapping 与 file spill；已通过且不受影响的旧验收不重跑。本轮 source candidate 的完整 relay、真实 Rust/Python 18 行及 Node 12 行跨语言、三系统及确切候选 GitHub Actions 按原门禁执行，含 Windows ARM64、Windows 2022/2025 与 Python 3.14t。
- 收尾：本批正常场景 socket/lease、SHM、spill 零残留；受控不可释放的反例不靠 free/reset 伪造成功，以隔离进程真实退出/原 owner teardown 后无 OS backing 残留验收，计费在放弃前不下降。保留文本日志与候选/run URL，清掉测试用 target、临时环境及 checkout；历史目录不动，发版仍等单独指令。

独立审查及差量复核已执行，意见已落实；最后请求方向补充由 Host 按审查具体建议核对，详情见[审查与基线复核记录](2026-10-28-r07-narrow-plan-review.md)。

## 10. 实施中的修订（2026-10-28）

验收时发现两处方案未覆盖的情形，由 Host 决定纳入本批并已告知用户：

- chunk 注册表的 abort、连接清理与 GC 抽取原先把永久释放错误与暂忙一样保留，`abort_reply_assembly` 会每 5 ms 重试且永不退出，接收任务卡住、close 无法确认，GC 每轮再耗一次完整自旋预算。这是基线就有的同类问题，违反第 1 节与第 5 节。现在暂忙照旧保留，真实错误交给原 `ReassemblyBacking` 放弃：计一次，扣住原计费，移除条目。若错误其实会自行消失，该块要到客户端重组池拆除时才回收，这与第 1 节对 `Contended` 的取舍一致。
- 第 5 节"request 只撤去 pending 的引用"在 pending 是 `RequestReleaseState` 唯一持有者时，会让最终 Drop 在 pending 表锁里阻塞重做一次释放，违反既有的"不在 pending 锁内等待 pool 锁"不变量。改为：拿到真实错误且唯一持有时直接最终放弃（计一次、置 released、归还 permit）；非唯一持有时只撤去引用。被省掉的那次重试不可能成功，因为已派发 dedicated 的错误只来自确定性的代次与身份校验，所以结果与原写法等价。所有引用与 permit 在 pending 表锁之外丢弃。

另按用户要求在本批完成以下发版前事项：Windows 真实 overlapped flush 的端到端回归（填满管道后发 prepared 帧并关闭对端，自然触发，不加测试开关）；在文档中写明计数可能被同用户异常对端抬高；Python cleanup 错误在 3.11 起通过 `add_note` 可见，同时保留 `__context__` 链给 3.10；新的对端请求回归同时在 Windows 上运行。Unix 上"整帧写完、flush 之前"的窗口实际不存在（UDS flush 为空操作，没有挂起点），因此 Unix 只做分层回归。
