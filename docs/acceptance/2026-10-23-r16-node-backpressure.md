# R16：Node 请求收发与终止处理收敛方案

状态：已按用户确认的修订方案完成验收，并随 0.8.2 发布，见[发布记录](2026-10-23-082-client-observation-release-acceptance.md)。基线为 0.8.1 验收后的 `764d39b03971119bc73301eb1d4d58465dce7649`，实现候选为 `02be11ea9ed0f8254838c39f42003c75891d86d7`；三系统、完整 relay、跨语言、性能、零残留及独立审查见[验收记录](2026-10-23-r16-node-exchange-acceptance.md)。用户于 2026-10-23 明确放弃 `writableLength` 补丁。此前未提交的三处 pause/resume 候选曾按保存的完整补丁撤回，恢复基线后再开始本轮；复现脚本、补丁与日志保留。

## 已确认问题与设计边界

0.8.1 的生成调用经 `createIpcEncodedTransport.call`，先等待 `writeIpcCallRequestFrames` 完成，再进入 `readIpcFrame`。官方 `NodeIpcConnection` 将所有 data 事件存入私有 chunks 队列，无人调用 readExactly 时仍会持续预读。该队列原先承担跨片拼接、并发读取及不足量 EOF 后保留尾部，不是可以直接删除的死代码。

初版只按读取需求暂停接收。在同一已发布 0.8.1 Rust 服务端、64 MiB 请求、4 KiB 分片、64 KiB 重组预算和 max_total_chunks=2 的隔离对照中，基线约 215 ms 返回拒绝；初版在 5 秒观察期内未完成，停在第 473 帧，反向 Node 缓冲约 65 KiB、待写 4,116 字节。两个服务实例均通过原生 shutdown 完成，私有根零 socket/lease 残留。该实验确认的是特定条件下的循环等待，不将观察期称为生产超时策略。

原因是双方进度互相依赖：客户端等写完才读，服务端提前拒绝后仍可能对后续分片回错，反向缓冲满后占住回复写锁和分片 permit，最终接收循环也停在写拒绝回复上。原 `writableLength` 原型恢复了拒绝返回，但仍靠写入期间持续缓存回复推进，且单次性能比较不是稳定收益证明。该原型不进入正式方案。

## 实施范围

本批收敛现有 Node 分片调用的一次收发交换，保留同连接的 `callChain` 串行调用、现有编码和解码、路由获取与绑定、连接关闭和 SHM 释放入口。只在当前调用期间推进接收，不增加常驻后台任务、多请求 pending Map、第二套连接缓存或新的协议帧。

1. 保留原发送模式选择和本地预检顺序，只对实际走 chunked 的请求并行接收回复。将既有 request id、flags、长度及终态错误解析移入该交换，不保留第二份解析；chunked 成功回复也由原组帧逻辑推进接收。只有全部请求写入成功后才能调用用户 allocator、更新成功观察或交付业务结果。未交付的普通回复字节按原 owned buffer 寿命释放，SHM 回复按第 5 项精确归还。inline 与 SHM 请求沿用当前发送事务。
2. 匹配且有效的终态拒绝到达后，使用现有 discardConnection 关闭该连接，停止后续分片并唤醒在途读写。当前 Node transport 同一连接只有一个执行中的调用；后续排队调用等待本次双方收尾，再按原身份和 token 约束重新建连。失败调用不自动重放。
3. 读失败、协议校验失败和有效拒绝都能主动关闭连接，不能只在两次 write 之间检查标志，也不能等阻塞中的 write 自行返回。主错误必须在关闭之前确定：写入先失败时保留原写错误；接收先确认拒绝或失败时，关闭诱发的本地 I/O 错误不得覆盖它。返回调用结果或推进 callChain 前，发送和接收都必须结束，不能只 Promise.race 后丢下未结束任务。并行阶段采用先确认失败的规则，是原“总先处理写错误”的明确语义调整；本地参数校验顺序不变。
4. 分片调用以错误结束时丢弃该连接，避免已在途的重复拒绝污染下一调用。该行为也可能使本来在完整上传后才发生的业务错误失去连接复用，属于需要接受的语义变化。完整成功路径继续复用连接。
5. chunked 与 SHM 请求发送路径互斥，不把新并发状态带进请求 SHM 的发布、消费确认和释放事务。chunked 请求仍可能收到 SHM 回复：发送失败时若首帧已经完整且有效，复用 buildIpcResponseShmBlock 和既有 reader.release，通过该握手绑定的 prefix、generation 和 allocationId 归还未交付分配；不先创建用户 allocator 结果。未完成或非法描述符不能作为清理授权，未配置 reader 时也不能另造释放入口。不能以“连接关闭会回收全部 SHM”代替精确释放证明。这是实现与审查的阻断项。
6. 完成上述交换后，接收按 readExactly 的实际需求推进，空闲时暂停预读。保留现有拼接及 EOF 尾部行为，不增加 writableLength、容量开关或另一套接收队列。合法大读取仍能逐片取得数据，不能因固定暂停水位卡死。

这里限制的是无人消费时的私有预读积压；在途合法回复仍需其经过验证的存储量。Node 高水位不是进程内存硬上限，不把本项描述为连接全部内存或用户持有结果的总预算。

## 公开接口和语义

`C2IpcConnection.close` 与官方 `C2NodeIpcConnection.close` 改为必需，契约为幂等且能使在途 readExactly/write 结束；移除旧可选 close 分支，不留兼容。官方 Node 实现已有该能力；低层注入实现和测试替身需同步。Node socket 类型需要原生 pause/resume，取消 writableLength 提案。

普通生成客户端使用配套更新的官方连接器时，业务方法、参数、返回值和调用方式不变，也不要求每次业务调用手工 close。破坏性影响集中在自行实现 C2IpcConnection / C2NodeIpcConnection 并注入 connect 的调用者：缺少 close 的实现不再满足类型和入口校验；已有 close 必须实际终止等待，不能只提供空方法。自行注入 C2NodeIpcSocket 的实现及测试替身需提供 pause/resume，原生 node:net.Socket 已有这两个方法。连接对象的 close 与连接器工厂释放 endpoint snapshot 的 close 仍是各自现有职责，不合并。

生成的业务调用接口、HTTP-only 惰性加载、完整合同和 route/server incarnation 绑定、发送后不重试继续保留。Rust 公开 API、C/FFI ABI 5、IPC 12、SHM 3、CRM 哈希和业务帧格式不变。空闲时正常 EOF 的观察仍可能延后；此 Node 字节接口没有空闲断连订阅承诺，Rust/Python 连接观察与 OwnerBound 监督不经该队列。直接使用低层字节接口、自行组织双向大写入的调用者需要同时推进读取，不能继续依赖未消费数据无限预读；正式生成客户端由本次交换调整满足该条件。

不新增默认期限或 SDK 轮询。明确 close 后不得有继续发送的旧任务；重新连接不能放宽旧 server identity、route UID/revision 的校验。

本方案的收益是及时处理拒绝、停止无效上传、消除已复现的循环等待，以及限制空闲私有预读。不能把这些修正统称为“代价”。明确的取舍是：失败的 chunked 调用丢弃连接后，下一调用增加一次连接和握手；低层注入类型必须提供 close；并行失败的主错误改为先确认者。正常成功路径保持复用，失败后若没有下一调用也不主动重连。prepareRoute 原本就在每次 call 中执行 ROUTE_LOOKUP，不能把这次原有路由获取再算成新增一轮。丢弃规则也覆盖完整上传后才收到的业务错误，不仅覆盖 I/O 或分片格式错误。释放遗漏、重复释放、错误覆盖、正常调用挂起、旧身份被重连接受均属于必须排除的实现回归，不作为可接受取舍。

错误规则依据因果关系区分主错误与主动关闭的副作用，不把每个事件简单视为等价候选。已完整校验的拒绝是主错误时，随后的 socket closed/write failed 不得替换它；若独立写失败先被确认且没有有效回复，保留写失败，不根据残缺帧推断服务端拒绝，也不无限等待一个更具体的理由。收发必须完成收尾，SHM 释放及其原有错误合并语义仍执行，不能因已有主错误就跳过释放。作为设计参照，[Node 22 pipeline 源码](https://github.com/nodejs/node/blob/v22.x/lib/internal/streams/pipeline.js)保留主错误并关闭其他流，也允许替换仅描述提前关闭的 ERR_STREAM_PREMATURE_CLOSE；[gRPC Go ClientStream](https://pkg.go.dev/google.golang.org/grpc#ClientStream)区分发送中止与经 RecvMsg 取得的 RPC 状态。它们说明应保留有效失败原因，不能把“任何最早错误都永不可替换”当作统一规则；本批不据此引入通用错误优先级框架。

## 不纳入本批的方案

完整 Node → Rust Core encoded-call 投影方向成立，但当前 C ABI/NAPI 只具备内存和端点能力。迁移还需异步 Runtime/Client 绑定、身份重连投影、配置冻结、错误和 payload 所有权、单一 native 实现库以及打包验证；保留现有低层注入时 TypeScript 协议仍不能退役。将其直接称为复用现成函数会隐瞒新增成本，另立迁移评估。

不将 ChunkAdmissionGate 改成长期拒绝缓存。它当前是首片准入排序和代际清理围栏，终态清理后移除条目；延长条目寿命会改变 max_total_chunks 容量及同 request id 的复用规则。本批由 Node 终态关闭现有连接完成收尾，服务端沿用已有 disconnect 清理。全局抑制重复分片错误可另作正确性评估，不能为本批临时增加 tombstone、TTL 或第二张表。

不直接用 Node read(size) 或 async iterator 替换 readExactly。Node 单次 read(size) 上限为 1 GiB，当前生成回复上限接近 2 GiB；不足量 EOF 和并发读也存在语义差异。额外分段回滚与串行队列可能抵消删除收益。参考 [Node Readable 文档](https://nodejs.org/docs/latest-v22.x/api/stream.html#readablereadsize)。

## 替换范围、成本和验收

生产范围为 `core/foundation/c2-codegen/assets/typescript_transport.ts` 的分片调用组织、首帧校验与收尾，以及 `core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts` / `node-shims.d.ts` 的读取驱动和 close 契约。测试复用既有 `typescript_transport_contract.rs`、Node binding/socket/loader 测试和真实 TypeScript 持久连接夹具；合并重复建连收尾，不新增通用模拟框架。HTTP、Core allocator/owner-control 和 server 准入实现不进入变更。

最终生产新增 152、删除 177，净减 25 行；测试与夹具新增 307、删除 88，净增 219 行，合计净增 194 行，文档另计。本批按正确性与背压修复记账。实现复用既有解析、关闭和释放，合并重复发送与观察路径；发送早停复用 connectionPromise/callChain，没有引入另一套停止状态、调度器、接收队列或通用框架。测试新增真实失败与释放交错覆盖，同时合并原有 Node 读取夹具。

实施顺序为基线反例 → 收发与终态调整 → 空闲背压 → 完整验收，收发调整和空闲背压作为同一完整候选交付。针对性覆盖拒绝到达时当前 write 仍挂起、最后一片与回复竞态、读取先失败、成功回复已到而写失败、SHM 请求/回复的精确释放、close 唤醒、再次空闲、并发读、EOF 尾部和大于高水位读取；所有错误不得自动重放业务。不能以固定帧号或短时间阈值作为进度证明，使用夹具事件确认被阻塞的写入实际结束且未发送分片不再发出。持久连接验证失败后的下一调用、server 重启/route 替换拒绝以及无关排队调用不被误报成功。

随后完成完整 relay、真实 Rust/Python/Node 调用、Windows 开发机优先的三系统验证、适用 Windows ARM64 GitHub Actions、同环境正常调用与提前拒绝性能对照、端点零残留和独立审查。接口变化必须绑定确切候选提交和全部必需 Actions 终态。历史目录不作清理目标，验证后删除本轮构建和安装产物。

## 设计核查结果

两份只读核查分别评估 Core 复用边界与现有 Node 交换路径。Core 迁移的范围分析已核对；Node 核查报告的部分建议未采纳：仅帧间早停不能唤醒当前阻塞写入，写完后收到错误也不能证明没有重复回复，连接 EOF 不证明响应 SHM 已精确回收，且只做早停而保留空闲无限预读不能完成 R16。报告把 Rust 的 request-id 分发与 Node 的串行读取混为一谈，这一结论经 recv_loop_inner / complete_unary_pending 源码复核后纠正。最终方案由 Host 据此修订；这些是静态设计结论，不是新实现的回归通过记录。
