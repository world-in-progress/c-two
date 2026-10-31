# 协议批详细设计

日期：2026-11-01；代码依据为输入 `030d3cfe0b532a3708e66c6f45a52116a5789c97`。本稿按独立审查和 Host 复核修订，未改代码、构建或运行测试。范围以 `docs/design/backlog.md:17-20` 为准，并明确将 E-host 并入本批。方向与所有者子系统删除已采用；决定 2、3 及第十节取舍须随本设计批准。已有论证引用[单一属主评估](2026-10-31-single-owner-release-evaluation.md)与[过度防御复审](2026-10-31-over-defense-review.md)，不复述。

引用中的 `c2-*` 按 `AGENTS.md:107-122` 的 crate 根展开，行号属于上述代码输入。本文先定义几个设计用语：“物理连接 / physical connection”是一对已建立的 OS 流，不是 proxy；“借出 / lending”是属主通过该连接发布一个分配供对端读取；“借出表 / loan book”按连接保存尚未释放的真实分配；“停用 / deactivation”停止业务准入而保留尚需结算的连接；“连接负责方 / connection coordinator”是统一管理该连接写队列、借出表和结束状态的内部对象，代码名拟为 `ConnectionCore`。“释放 / release”统一指结束租约或分配；跨进程时借用方交付释放记录，属主收到后回收分配。下文不用“归还”另指同一动作。“属主 / pool owner”与待删除子系统的“所有者 / owner”仍按 `CONTEXT.md:59-68,126-128` 区分。

## 一、现状与需要替换的路径

客户端每个 IpcClient 创建一个请求池并写载荷，重组另池、共用 outgoing 预算；每个 Server 创建一个响应池供所有连接共用。两端按 prefix/坐标懒打开对端段（`c2-ipc/src/client.rs:2605-2638,3273-3303`；`c2-server/src/server.rs:592-606`）。

服务端释放请求调用客户端池 free_at，客户端释放响应调用服务端池 free_at，因此双方都写对端 allocator；Unix/Windows peer 映射均可写（`c2-server/src/dispatcher.rs:293-323`；`c2-ipc/src/client.rs:484-516`；`c2-mem/src/segment/unix.rs:90-128`、`windows.rs:91-103,155-165`）。SHM 3 共享头存 PID 锁字、计数、序列、位图和活记录（`c2-mem/src/alloc/buddy.rs:31-81,297-320`）。

dedicated 读端写 read_done，属主等标志/崩溃超时退映射和预算；u16 index 单调耗尽即拒绝。文件溢出关映射后关文件，不复用旧 backing 给新载荷（`c2-mem/src/dedicated.rs:1-20`；`pool.rs:489-529,1303-1362`；`docs/reference/memory-policy.md:53`）。

生产唯一 CoreCallback 先复制请求，invoke 返回后、生成响应前释放传输租约，PyO3 移动 snapshot 并给 memoryview；低层接口允许延长 RequestData，晚释放当前来自测试回调（`c2-core/src/host.rs:1236-1278`；`c2-server/src/dispatcher.rs:370-380,498-505`；`c2-ipc/src/tests.rs:931`；`docs/reference/payload-lifetime.md:68`）。响应 carrier 保留旧 peer pool，重连换容器；Core hold 有连接引用但不阻止池强关，Python 活动 export 拒绝释放（`c2-ipc/src/response.rs:26-41,101-156`；`client.rs:3278-3303,3335-3339`；`c2-core/src/client.rs:383-448`；`sdk/python/native/src/core_ffi.rs:283-291,392-420`）。

所有关闭入口都须替换：

- EOF/读错/DISCONNECT 当前结束接收并丢流；ConnectionReset 只分为错误，未用于 peer end 结算（`c2-server/src/server.rs:2059-2073,2544-2555`；`c2-ipc/src/client.rs:5755-5763`；`c2-local/src/observation.rs:330-351`）。
- 心跳超时直接 abort（`c2-server/src/heartbeat.rs:71-94`）。
- 写中取消由 SendGuard 毒化流，写超时/失败由 AbortOnDrop 关全流（`c2-ipc/src/client.rs:2238-2297,3934-3969,4338-4371`；`c2-local/src/lib.rs:739-764`）。
- 协议错误 abort 或 break 后丢流（`c2-server/src/server.rs:2084-2092,2492-2498`）。
- 引用只阻止空闲淘汰，close/Drop 仍清 writer/abort 并发布终态（`c2-ipc/src/pool.rs:311-319`；`client.rs:5154-5258,5409`）。
- Host shutdown 打断读取、等连接表空才关 listener；Runtime 退休封池后 close_all 强关（`c2-server/src/server.rs:804-811,1600-1610,2069-2072`；`c2-ipc/src/pool.rs:1582-1627`；`c2-core/src/session.rs:2448-2468`）。

LocalStream::shutdown 仅 flush；AbortHandle 从共享状态取走 stream，Unix 调 SHUT_RDWR、Windows 调 CancelIoEx，不能仅靠保留旧 Arc 延长租约安全（`c2-local/src/lib.rs:423-452,676-678`；`unix.rs:47-50`；`windows.rs:204-218,256-266`）。

TS 有一个静态确认、尚未运行复现的问题：读取不处理 SIGNAL，按 request_id 严格匹配；服务端默认空闲 15 秒后发 request_id=0 的 PING，下一 Node 调用会先读到它而失败。实际在后续 interval/2 tick 发送，复现须确认 PING 已发，不能只睡恰好 15 秒（`c2-codegen/assets/typescript_transport.ts:598-614`；`c2-config/src/ipc.rs:196-197`；`c2-server/src/heartbeat.rs:25-39,59-94`）。

## 二、单一释放帧与无头 SHM

保留 16 B 信封 `[total_len:u32 LE][request_id:u64 LE][flags:u32 LE]`，total_len 不含自身 4 B（`c2-wire/src/frame.rs:1-26`）。唯一释放帧用新 `MsgType::Release=0x17`、request_id=0、FLAG_CTRL，服务端加 FLAG_RESPONSE；只允许握手完成后发送，不复用 0x16。

payload 为 `[type:u8=0x17][release_count:u8][loan_id:u64 LE × count]`。协议常量 `IPC_RELEASE_BATCH_CAPACITY=64`，count 为 1–64；整帧最小 26 B、满批 530 B，total_len 为 22/526。c2-config 的 max_frame_size 下限从 >16 提到 526；常量定义在 config，供 wire/FFI/资产生成共用，避免反向依赖（`c2-config/src/ipc.rs:451-455`）。64 只限定一帧，队列更多就连续发，不限制总量、不等满批或定时器。

写端空闲立即发；下一业务帧已排队时，写任务把独立释放帧和它拼进同一次写提交。线上仍两帧，每批多 16 B 帧头；业务 payload 不变，不加扩展标志、剥离规则、握手帧限值或 ACK，固定 preamble 保持独立（`docs/reference/ipc-handshake.md:11-30`）。

接收边界一次验类型/方向/阶段/count/精确长度，再按本连接借出表移走真实 handle，本地释放，不按对端坐标 free。零/未知/重复/未借出/已释放 ID 报 ProtocolViolation 并停用，已结算的不回滚，剩余等 peer end。第二次查不到就不碰 allocator；正常 carrier 只发一次，不增历史 tombstone/重试，内层不重复验证。

IPC 15 descriptor 保持 23 B：`slot:u16, generation:u32, offset:u32, data_size:u32, kind:u8, loan_id:u64`，整数 LE，kind 为 0 buddy/1 dedicated。loan_id 替换 allocation_id 和 dedicated 零身份，按连接方向分配，不是 request_id；表存真实分配/容量（旧式 `c2-wire/src/buddy.rs:1-32,78-100`）。

SHM 4 段内无头，数据从 0 开始；格式由握手确定，kind/generation 在 OS 名中，接纳 descriptor 按 OS 实际视图 span 和 checked 算术验范围一次。位图/记录/计数/线程锁移到属主，删 magic/version/data_offset/几何头与 dedicated 头；代价是段不能独立自描述（`c2-mem/src/alloc/buddy.rs:31-81`；`dedicated.rs:13-20`）。

peer OS 映射只读，属主可写；本地锁仍串行分配，锁内不等对端/应用/I/O。保留 move-only handle 和 unsafe/FFI 输入一次验证，内层信不变量；panic 直接失败，不夺锁。连接表限定释放权限，不承诺隔离恶意同用户或撤销 raw mapping（`docs/reference/memory-policy.md:47-55`；`docs/design/2026-10-31-over-defense-review.md:11`）。

两类段都用可复用 slot、checked u32 generation，名称含原 pool incarnation/kind/slot/generation；清空本地分配和借出才复用。代次耗尽只退休该槽，全部退休走既有分片/文件，不换池/prefix/连接。loan_id/request_id checked 递增，溢出报连接级错误并停用，不自动重连/重放；旧映射不初始化（`c2-mem/src/pool.rs:1303-1362`；`docs/reference/windows-native-implementation.md:56-64`）。

## 三、共享连接核心与两个方向的释放

共同状态机随 P3 放在拟新增 `c2-wire/src/connection/`，参照已有 chunk registry（`c2-wire/src/chunk/registry.rs:1-22,79-93`）。ConnectionCore 管借出表、释放队列、写帧进度、停用和 peer end；c2-ipc/c2-server 只接池、业务调度和 I/O，Node FFI 复用它，不各写合并/关闭算法。

Host 批注（2026-11-01）：ConnectionCore 写成不做 I/O 的同步状态机，与 chunk registry 一样只用 parking_lot。驱动方（c2-ipc、c2-server、Node FFI）把收到的帧、写完成和读端结束的事实交给它，再按它给出的下一步去读写；读端结束仍由 c2-local 判定，经驱动方转交。这样 c2-wire 不新增对 c2-local 与 Tokio 的依赖，仍只依赖 config、contract、error 与 mem，状态机也能脱离真实流单独测试；mem 保留本地分配，不反依赖 wire（`c2-wire/Cargo.toml:10-18`；`c2-local/Cargo.toml:10-18`；`c2-mem/Cargo.toml:13-19`）。

分配填满交写任务后，先在连接锁内登记并转移 handle，再提交第一字节。写前取消本地释放；提交后不能证明未完整送达，就留到释放记录/peer end，完整帧后的 flush 失败也算借出。快速释放、写完成、peer end 只移走一次 handle，结束同时封新借出；响应/截止时间不删借出表（`c2-ipc/src/client.rs:4246-4266`；`c2-server/src/server.rs:4403-4466`）。

SHM 接纳时备好不可复制的节点，绑定原连接实际 OS 引用、mapping guard、loan_id。释放先失效 FastDB checked owner，再结束 view/export，最后交释放队列；Drop 不临时分配、不等网络。本地成功不表示属主已收，peer 已结束则只放本地映射；保留 export 拒绝/invalidator once-only/原异常（`docs/reference/payload-lifetime.md:41,56-70`；`sdk/python/native/src/core_ffi.rs:392-420`）。

服务端最后一个 RequestData 使用者结束才发请求释放，通常在响应前，晚释放仍独立发；收到响应不能释放请求。客户端 materialize 的传输租约结束或 hold release/Drop 才发响应释放，属主收到才回收（`c2-core/src/host.rs:1236-1278`；`c2-server/src/dispatcher.rs:498-505`；`c2-codegen/assets/typescript_transport.ts:616,750-753`）。

写任务在当前整帧后优先发释放；节点总量由接纳租约决定。分片逐帧交 writer，让控制片间推进，不做 B2/业务公平。Rust 读任务不等写锁/应用/业务许可，错误响应也排队，继续用已有 pending/字节预算、不复制完整载荷；存储不足则报告并停用。控制不能穿越 OS 阻塞的半帧（`c2-ipc/src/client.rs:4338-4371`；`c2-server/src/server.rs:2499-2514`）。

Node 只在调用/close 中读：先处理释放帧，再等本次响应。响应后才到的请求释放等下一调用或关闭结算，空闲时仍占池预算。唯一生产 CoreCallback 在回复前释放，writer 先发释放帧，正常路径本次可结清；晚释放来自测试回调，协议仍支持，不以响应终态代替（`c2-core/src/host.rs:1236-1278`；`c2-ipc/src/tests.rs:931`；`c2-codegen/assets/typescript_transport.ts:598-616`）。

决定 2：远端 SHM hold 留原 OS 引用/映射，重连仍向旧连接发记录；inline/本地重组/文件 hold 只留本地 owner/预算。属主死后已映射 hold 可读到释放。响应池仍每 Server 共用，按连接表隔离借出（`c2-server/src/server.rs:592-606`）。

## 四、peer end、停用与关闭

L 是本方未结清借出数，B 是本方仍读的对端租约数。停用封新业务、取消排队并返许可；active callback 留请求/许可到返回。记录逐条结算 L，peer end 才清剩余 L，DISCONNECT 仅请求停用（`docs/reference/runtime-lifecycle.md:39-41`）。

peer end 包括读 0 和 Unix 本地流读端 ECONNRESET；保留 reason=connection_reset/errno，不能把写错误也作证明。Linux AF_UNIX 关闭方接收队列非空会使对端读 ECONNRESET，这是 Host 核对的 unix_release_sock 行为，Host 在 macOS 对照为 EOF；本次未运行。当前代码只分错误原因，须补读端结算。Windows 读 109/232 仍为 EOF，233 不为 EOF（`c2-local/src/observation.rs:330-351`）。

本地 cancel/超时/一般 io_error/DISCONNECT 不清 L。畸形帧不猜边界，停用后固定缓冲丢弃到 peer end；解析健康时继续收释放、立即释放迟到 SHM。不可恢复读错却无结束证明，错误原样上报，L 留原池 teardown，不偷偷复用/重试；连接结束不撤销违规 raw mapping（`docs/reference/local-endpoint-lifecycle.md:189`）。

物理 close 需停用、B=0、L=0，锁内封新租约/借出，未发布写撤销。此时释放队列未发完也可关，对端会据 peer end 结清；peer end 先清 L，本地映射留到 B=0。不依赖 half-close 或双方都等 EOF 才关的新 ACK。

调用方按原截止时间返回，writer 仍拥有已开始帧，超时只停用/记原因，实际 I/O 失败不重发。local 改用引用计数 owner 持有实际流，借入租约强引用它；任务/执行器先结束也不能关句柄/SHUT_RDWR，旧 abort 的 take 操作删除。已发布借出不随任务析构 free，无结算任务时留原池占用（`c2-local/src/lib.rs:423-452,739-764`）。

Host 业务完成看 listener_closed/work_drained，区别保留连接；每次启动本就新建 Server/响应池，不加启动代次（`c2-core/src/host.rs:967-985`；`c2-server/src/server.rs:592-606,1600-1610`）。E-host 明确从 backlog.md:23 的 Core 批并入：低层 Host shutdown 不关 Runtime 调用域/outgoing，显式退休才封；当前会关闭 call_execution_observer 并混用客户端 drain，P6 分开。原 ShutdownTransaction/失败启动 receipt/RuntimeShutdownReceipt 保留，Host 同步待办（`c2-core/src/session.rs:1903-1917,1971-1981,2448-2468`）。

- 借用方 kill -9 后，属主确认 peer end 清剩余借出；接收队列非空也须结清，继承 fd 尚在则等。进程死亡另看 OS wait。
- 属主崩溃后 allocator 结束，已映射 hold 留到本地释放，本方请求池据 peer end 结清；可读映射不证明业务存活。
- SIGSTOP/调试器期间无 peer end 不复用；建议删心跳后仅调用截止时间结束等待，恢复可继续。保留心跳则超时停用会拒绝恢复后的下一调用。
- fork 父退出、子持 fd 无结束；最后副本关才回收。无 fd 继续读 mapping/提前半关读 raw alias 违反租约契约，不支持（`docs/design/2026-10-31-single-owner-release-evaluation.md:43-49`）。
- 强关/协议错/写错只停用，B 等读者，L 等记录/peer end；不可按等待时长强拆，输出坏且对端不结束可长期占用。
- Host shutdown/退休的 listener/queued/active 走原事务；连接/池留原 owner，退休封旧域，新 session 不接旧借出，完成与载荷保留分开。

保留现有 ConnectionEvents/预算观察，停用不提前发物理 closed，最终保留首个原因/OS cause。不增公开字段；借出/租约/队列/句柄由 crate 内访问器测（`docs/reference/runtime-lifecycle.md:77-79`；`docs/reference/memory-policy.md:138,148`）。

## 五、删除清单与估计

以下 M1–M6 只是把内存和释放路径分成六组以便估量；各项的文件归属由第七节实施表决定。单位为源码物理行粗估，新增不含测试，不是净减承诺。A1/A4 计 M1、A9 计 M3、释放认证计 M4、C7/R13 计 M5，B6 不重复计 B1 的 watch。

| 项目 | 生产删除 | 生产新增 | 旧测试删除 |
| --- | ---: | ---: | ---: |
| M1 共享分配器防御、A1/A4 | 500–800 | 130–250 | 350–600 |
| M2 R07 保留重试 | 220–450 | 60–140 | 250–500 |
| M3 dedicated/A9 | 130–240 | 50–110 | 100–200 |
| M4 无头 SHM/只读 peer/释放帧 | 150–300 | 160–280 | 120–250 |
| M5 C2/C-U1/C7/R13 | 1,150–1,650 | 120–230 | 450–850 |
| M6 共享连接核心与两端接线 | 100–220 | 450–700 | 150–300 |
| A6/A11 | 50–120 | 5–25 | 20–60 |
| B1/B3/B6 | 550–850 | 80–160 | 230–400 |
| B4 | 100–220 | 0–30 | 80–160 |
| B7 | 120–240 | 20–50 | 100–200 |
| F5 | 60–140 | 0–25 | 40–90 |
| B9 | 10–35 | 0–15 | 10–25 |
| D7 | 30–70 | 5–20 | 20–50 |
| 自动心跳及两设置，建议删除 | 160–300 | 0–20 | 100–180 |
| E-host，新增纳入范围 | 20–60 | 20–60 | 0–20 |

M1 整删 `c2-mem/src/alloc/spinlock.rs`，其 365 行起为测试，不能把全部 619 行算生产。`alloc/buddy.rs` 去共享锁字/记录和重复位图扫描，`buddy_segment.rs`、`pool.rs` 改本地分配，free 用可信 handle 的 level（`buddy.rs:31-81,297-320`；`pool.rs:789`）。

M2 删除 allocator 争用失败的保留/重试和 abandoned_releases 投影，涉及 `c2-mem/src/pool.rs`、`c2-ipc/src/{client,response}.rs`、`c2-core/src/client.rs`、`sdk/rust/src/held.rs:44-58,82-89`、Python native/core_ffi 与 `sdk/python/src/c_two/crm/transferable.py:61-118,166-174`。export 拒绝、invalidator once-only、原异常链保留（`docs/reference/memory-policy.md:142-146`）。

M3 删除 `c2-mem/src/dedicated.rs:1-135` 的 read_done、`pool.rs:489-550` 的 freed_at/超时 GC/退休查询，`pool.rs:1303-1362` 改槽代次。配置定义归 `c2-config/src/pool.rs:5-16,48-60`，投影归 `c2-config/src/ipc.rs:236-317`，去 dedicated_crash_timeout_secs；`c2-mem/src/config.rs:3` 重导出不单独定义第二份。映射、预算、文件 owner 留。

M4 在 `c2-mem/src/segment/{unix,windows}.rs`、`buddy_segment.rs`、`pool.rs` 删除共享头、peer allocator/free、可写映射；`c2-wire/src/buddy.rs` 换 descriptor，新增单一释放帧。全部段数据从 0 开始，旧几何 attach 删除。旧接缝见第一、二节。

M5 删除 `c2-ipc/src/client.rs:534-1119` 的 retire executor/容量许可，`pool.rs:391-870` 起的结算/maintenance、UnconfirmedIdle、try_lock 轮询；`client.rs:1262-1355,2552,5184-5332` 同链收敛。pending 不再兼任释放表，未发布分配由唯一准备者清理；M6 共享核心只记一次新增，两端不再各自实现 writer 合并、停用、peer end 状态机。

A6/A11：`c2-mem/src/pool.rs:924,1049-1111` 去每次访问的重复证明；`budget.rs:121`、`c2-config/src/ipc.rs:113`、`c2-ipc/src/client.rs:2939` 去第二份限额/比较。配置只作输入，冻结后执行和观察使用同一 MemoryBudget，不让 config 反依赖 mem。

B1/B3/B6：`c2-wire/src/{msg_type,route_catalog_control,handshake}.rs` 删 RouteWatch、RouteContract 查询、RouteAck、NACK nonce、owner_epoch/lease_deadline_ms、watch revision；`c2-server/src/catalog.rs` 删事件缓存与第二份 route map。Dispatcher 为唯一真相，握手/lookup 投影已验引用，去七字符串克隆/重验。RouteContractWire 仍供 lookup，保留（`route_catalog_control.rs:51-169`；`catalog.rs:11-86,283`；`dispatcher.rs:706-735`）。

B4：wire、IPC directory、Core token、HTTP header/resolve/mesh、TS/Python/Rust 同删 route_revision/observed revision，不填常量 1。保 route_uid、server instance、契约及真实准入代次；TS 2^53 问题随字段消失（`c2-wire/src/handshake.rs:179-191`；`route_catalog_control.rs:89-93`；`c2-codegen/assets/typescript_transport.ts:1840,2327,3600,3859`）。

B7：ACK 只留单字节 MsgType::ShutdownAck，去 acknowledged/shutdown_started/server_stopped/route_outcomes 及空结构。owner 拒绝删、畸形 initiate 直接失败后不需负 ACK，Core 只投影已发起或未确认，完成看原 receipt。删恒开能力位、握手段列表、FFI 列表查询、TS eager-open；保真实方向 SHM（`c2-wire/src/shutdown_control.rs:5-35`、`handshake.rs:133-161,201-213`；`c2-server/src/server.rs:2720-2749`）。

F5：`c2-http/src/relay/{peer,types,route_table,peer_handlers}.rs` 去 DigestDiff Active/Deleted 冗余 hash、removed_revision、ROUTE_HASH_PEER_VERSION；保 DigestEntry 比较 hash，本地重算。RouteAnnounce 本来无 hash（`peer.rs:8-9,60-95,256-278,380-397`）。

B9：PreparedShmResponse::new 已 crate-private，producer 是固定 pool helper；去 `c2-server/src/dispatcher.rs:106`、`server.rs:4554` 的重验，unsafe write 仍履行原所有权契约（`dispatcher.rs:56-86,106-117`；`response.rs:30-38,74-86`）。

D7：身份核验期间留临时 handle，删除长期 pin/finalizer，涉及 `c2-local/src/windows.rs:39-67,118-160`、`c2-mem-ffi/src/lib.rs:598-655`、`c2-mem-ffi/include/c2_mem_ffi.h:126-141`、`c2-mem-ffi/bindings/typescript/native/node_c2_mem_ffi_loader.c:1180-1185,1361-1377`。pipe owner、TokenUser、integrity、活性及 PID 回查仍在首字节前完成；第十节条件先闭合。这里的 process pin 不等于第四节租约需要的连接 OS 句柄引用。

自动心跳删除项包括 `c2-server/src/heartbeat.rs`、server spawn/收尾、公开 run_heartbeat/HeartbeatResult，以及 c2-config、Python、CLI/env 的两设置投影、文档与旧心跳测试；手动 PING/PONG 保留。原 R35 因仅省禁用空任务而暂缓，本次删除理由是决定 3 下自动停用不回收资源且破坏恢复使用，不沿用旧捆绑收益（`docs/design/2026-10-23-r35-heartbeat-task-ownership.md:3-13,29-31`）。

### 所有者子系统与保留部分

P0 是前置的所有者删除块，以下单独审查，粗估生产删 3.4–4.1 千行、增 40–140 行机械接线，旧测试/夹具删 4.6–6 千行；分项估计如下，不重复计算 owner_ffi。新增仅为保留普通 Host 代码的机械移动，不改协议释放/关闭行为。

- OwnerBound、监督、宽限：删 `c2-config/src/lifecycle.rs` 及导出、`c2-core/src/owner_bound.rs:133-625`、`host.rs:967-1128`、session owner attach 分支。`owner_bound.rs:17-130` 中性 snapshot/cell 移出，去 owner phase/policy；`c2-server/src/admission.rs:18-89` 锁/代次留，91-138 pause/control 删。估生产删 780–1,000、增 40–100，旧测试删 900–1,300。
- 恢复凭证、接管、控制协议：整删 `c2-core/src/owner_control.rs:1-939`、`c2-wire/src/owner_control.rs:1-340`、`c2-server/src/owner_control.rs:1-245`；`msg_type.rs:31,59` 去 0x16，`server.rs:2144-2230` 去 owner 角色分类/切换及 admin 拒绝。普通握手 fence/SIGNAL 留。估生产删 1,550–1,650、增 0–20，旧测试删 2,300–2,700。
- bootstrap、托管子进程：删 `c2-local/src/owner.rs`、`owner/{unix,windows,tests}.rs`、Windows OwnerClient/OwnerServer；Python owner_ffi 去 OwnedChild/shared reaper；`c2-local/src/lib.rs:37-63` owner 专用 EndpointContextBinding/bind 变体删，普通目录 pin 留。估生产删 800–1,000、增 0，旧测试删 500–800。
- SDK、测试：删 `sdk/rust/src/owner.rs`、导出和 examples/owned_child.rs；Python owner.py、config/lifecycle.py、native/src/owner_ffi.rs 控制投影/注册；`registry.py:338-386,450-452,1029-1075`、`runtime_session_ffi.rs:1165-1218,1840-1901` 去入口。删 c2-core/tests/owner_bound_lifecycle.rs、Python owner 集成/fixture/单测，普通 Host/exports/代次测试留。独立余量估生产删 250–450、增 0–20，旧测试删 900–1,200。

Python 删除 OwnerBootstrap、OwnerControl、OwnerRecoveryTicket、PreparedOwnerTakeover、OwnerStatus、OwnerControlError、OwnedChild、owner_control_pair、prepare_owner_takeover、spawn_owned_child、LifecycleConfig；Rust 另删 OwnerControlErrorKind、OwnerPhase、ServerLifecyclePolicy，set_server 的 lifecycle/owner_control 同删（`sdk/python/src/c_two/owner.py:2-14`；`sdk/rust/src/owner.rs:5-9`）。

普通 shutdown/lifecycle 留（`c2-core/src/host.rs:548-674`、`session.rs:1857-1890`）；端点凭据/inspect/reap/sweep 留（`c2-local/src/lib.rs:70-105`；`docs/reference/local-endpoint-lifecycle.md:132-156`）；admission 锁/代次和 relay route owner/liveness 留。Windows identity 测试删 bootstrap 断言，普通 pipe owner 检查仍执行（`tools/ci/source_validation.py:368-375`）。

当前文档删除/更新由 Host 执行：CONTEXT、AGENTS、根 README、Rust README、Python usage、configuration、runtime/endpoint lifecycle、handshake 的 owner 名字、参数、示例；主要为 `docs/reference/local-endpoint-lifecycle.md:88-126,193`、`ipc-handshake.md:9,30-32`。旧计划有效说明先移 reference，再删 superseded owner 计划；历史 acceptance/release 保留。文档估删 250–450、增 60–100。新释放/故障测试估 700–1,300 行，Node/FFI 改写估 160–300 行；删除只钉旧源码/名字的测试，最终分源码、测试、文档核算。

## 六、其余并入项与版本

请求号线上 u64，pending/AtomicU32 为 u32，接收明确 as u32，确有高位截断。计数器、pending、取消/控制/分片 key 同改 u64，TS 用 bigint；两个低位相同高位不同的 ID 不能误投递，checked 溢出按第二节报连接级错误。估改 50–150 行（`c2-wire/src/frame.rs:24`；`c2-ipc/src/client.rs:2124-2159,2477,2638,5771`）。

R13 只查写前未发布分配，已验收的 snapshot/拼帧/prepare/send/inline 移交不重做。旧记录与当前 call_buddy_stream 取消后 alloc_count=0 的测试不一致；实施用原实验对照普通 SHM、stream prepare、服务端 prepared reply，缺细节由 Host 补，不编造 fail-first（`docs/design/2026-10-13-local-slimming-batches.md:34`；`c2-ipc/src/client.rs:4187-4206,8367-8415`；`c2-server/src/server.rs:11683`）。

Rust/Python/Node 同候选切 IPC 15、SHM 4、ABI 7、mesh 7，当前为 14/3/6/6（`c2-wire/src/handshake.rs:39`；`c2-mem/src/alloc/buddy.rs:24`；`c2-mem-ffi/src/lib.rs:15`；`c2-http/src/relay/peer.rs:8-9`）。25 B preamble 不变，旧 metadata/段头/ABI 符号组/别名/loader/JSON 字段全部删除；新版本包旧字段也拒绝。mesh equality 已修，F5c 不重做（`c2-http/src/relay/peer_handlers.rs:38-39,663`）。

ABI 7 删除 peer response_pool_release、request forget_consumed，native memory context 使用共同 ConnectionCore，绑定已经验证的 native 连接及实际 OS 句柄引用，管理借出、只读块、释放和 peer end。C descriptor 用 loan_id，未发布分配用 opaque owner；FFI 不加第二收包循环，Node 按第三节只在调用/close 中读取，释放帧走唯一 writer，旧 reader/writer 适配接口同时删除。TS 仍复制到调用者 allocator 后释放，不新增零复制 hold，HTTP-only 不加载 native（`c2-mem-ffi/src/lib.rs:951-1005,1125-1167`；`c2-codegen/assets/typescript_transport.ts:576-616,737-753`）。这不是完整 R16 迁移。

握手删除段列表，只交换 prefix、真实 SHM 方向能力和已有身份/路由字段；不新增 frame-limit 字段或 segment announcement。Windows 跨 session 继续 inline/chunk；端点命名、TokenUser、Unix namespace 3/credential 4 不变，区别于 owner recovery ticket（`docs/reference/ipc-handshake.md:62-64`；`docs/reference/local-endpoint-lifecycle.md:21-31`）。

## 七、实施顺序与文件责任

P0–P9 是实施审查单元编号。P0 先做纯删除并单独审查，之后冻结为实施基线；它与后续块可能先后修改同一混合文件，但不并行。后续块从该基线出发，各文件只有一个写者。`docs/**`、AGENTS.md、CONTEXT.md 和根 README 始终由 Host 维护，不放入任何委派块；表中范围均扣除这些路径，SDK/CLI README 也由 Host 维护并从 P7/P8 扣除。

P0 按第五节所有者清单删能力、导出、配置、测试和 0x16 角色；只机械移出保留的普通 Host 代码，不做单一属主、E-host、自动心跳或新 writer。审查检查普通 Host shutdown/lifecycle/admission/凭据仍在，owner 公开名字和无用壳消失。

配置、内存和共同核心严格按 P2、P1、P3 的顺序交付。原因是 c2-mem 依赖 c2-config、c2-wire 依赖 c2-mem；c2-ipc 集成测试依赖 c2-server，不能在服务端接线前要求客户端独立完整通过（`c2-mem/Cargo.toml:13-19`；`c2-wire/Cargo.toml:10-18`；`c2-ipc/Cargo.toml:25-27`）。

| 块 | 唯一写入范围 | 内容与退出检查 |
| --- | --- | --- |
| P2 配置/本地流 | c2-config/**、c2-local/**、c2-local-security/** | PoolConfig/超时投影、满批帧下限、心跳配置；OS 引用所有权、读端 ECONNRESET/Windows 分类、D7；配置和 native stream/身份测试。 |
| P1 内存，等 P2 | c2-mem/** | 无头 SHM、只读 peer、本地 allocator、槽代次/预算、去 read_done/跨进程锁；映射/旧代次/身份/复用检查。 |
| P3 共同核心，等 P1 | c2-wire/** | codec、单一释放帧、ConnectionCore 的队列/合并/发布/停用/peer end、边界测试和故障状态机；两端直接复用，不另造算法。 |
| P5 服务端，等 P3 | c2-server/** | 接业务调度与响应池，删心跳/重复路由真相；只验证接线、queued/active、晚释放、当前顺序。 |
| P4 客户端，等 P5 | c2-ipc/** | 接请求池/pending/u64/响应租约，删 C2/C-U1/C7；先聚焦接线，再与真实服务端跑取消/hold/peer end、relay unset/unavailable 的 direct IPC。 |
| P6 Core，等 P4/P5 | c2-core/** | E-host、退休/Host 重启/旧 hold、原收据；不重新实现关闭状态机。 |
| P7 relay，等 P3 字段，可与接线并行 | c2-http/**、cli/** | mesh 7/hash/revision、路由 token、心跳 CLI 投影；新旧双向拒绝、announce/withdraw/diff、普通维护。 |
| P8 SDK/ABI，等 P6/P7 | c2-mem-ffi/**、c2-codegen/**、sdk/python/**、sdk/rust/**、examples/** | ABI 7、Node 调用内控制读取、互通/exports/FastDB checked view/borrowed。 |
| P9 整体验收，等全部 | .github/**、tools/**、tests/repo/**、必要根 manifest/lock | 全平台 runner、基准、receipt/source/hash；代码修正回交原块。 |

P3 单独测共同算法；P4/P5 的测试不各复制一套状态机断言。跨语言测试归 P8，根锁文件由 P9 单一更新。先跑影响范围检查，接线齐后跑 core workspace，全量长任务 Host 统一调度，Cargo 共用 `<checkout>/core/target`。Host 另行更新 backlog、当前参考文档和下节术语，避免实施者同时写文档（`AGENTS.md:198-210`）。本次仅写本设计，未执行上述检查。

## 八、故障验收与基准

确切候选跑 GitHub Actions Linux、macOS、Windows x64/ARM64 与适用 Linux 3.14t，留 commit/run URL、FastDB 输入、native 架构、加载路径和 SHA-256。沿现 runner 跑 core/CLI/Rust SDK、Python full/native/3.10 syntax、Node loader/package、严格 18 行 Rust/Python 与 12 行 TS direct/relay、普通用户消费者和 Windows 跨 session；旧 run/交叉编译不替代（`AGENTS.md:7-9,93`；`.github/workflows/ci.yml:84-104`；`tools/ci/source_validation.py:368-458`；`docs/reference/development.md:49-57`）。

故障测试用 crate 内访问器，不增加公开观测：

- 双向释放覆盖一调用多分配、请求释放晚于响应、短/长 hold、空闲立即发送、两帧同一次写、超过 64 条分多帧、片间控制。配置恰好允许 526、低于它拒绝；未知/重复/跨连接 ID、错误长度/阶段/方向、旧 IPC/SHM/ABI 均不得误释放。
- 客户端和 Host 分别在登记前、写中、整帧后、callback/hold 时 kill -9；增加受害方接收队列非空一档，在 Linux 和 macOS 都跑。先确认存在未读响应，再终止，保留读取结果及 errno，确认 Linux ECONNRESET/macOS EOF 都清剩余借出；Windows 终止/wait 单独留原生证据。
- SIGSTOP/调试器超过原心跳窗口，块仍不复用；恢复后调用可继续、释放可结清。fork 父死子持 fd 时不结束，最后副本关闭才回收；exec CLOEXEC 与 Windows HANDLE 继承分别验证。不宣称支持无 fd 继续读 mapping。
- hold/export 活着时走第一节全部关闭入口；原任务或执行器先结束时，借入租约的 OS 引用仍在，不得向对端制造结束事实；租约释放后可结束。公开观察沿原契约，内部断言借出、映射/预算、帧队列、句柄寿命及无双重释放。
- E-host 检验低层 Host 关闭后 outgoing 与有限调用仍可用、可启动新 Server；显式退休封旧域；原 hold/receipt 不被新 session 接管。写中取消不 abort 共享流，不重放副作用；pending 满/错误积压时控制读取不等业务许可。
- Node 在旧基线上启用默认心跳，确认服务端已经发出 PING 后发起下一调用，复现 SIGNAL/request_id 问题。本次只有代码证据。建议方案删除自动心跳后验证同样空闲仍成功；若保留心跳，必须先处理 PING/PONG，再匹配业务响应。测试回调响应后才发请求释放，Node 空闲时保持占用，下次调用和 close 都能结算。
- backing 现象做透明转发器与完全不经转发器的 explicit IPC 对照，分正常释放/退出、kill、retire；记录进程/fd/handle、名称、内部借出/租约、peer end、unlink/open、OS wait 时间线。区分真实 owner 未结束、名称未 unlink、仅旧 mapping 可读，只清实验对象，不能先定转发器是根因（`docs/acceptance/2026-09-26-memory-policy.md:61-66`）。

性能只用 `tools/benchmarks/ipc_memory.py:681-760` 既有九个场景：小 RPC、四客户端突发、4 MiB 科学载荷的 buddy 开/关对照，以及三个预算场景；不新增工作负载。三个平台在相同机器、依赖/profile/配置下比较确切 baseline/candidate 的 P50、P99、吞吐，轮换运行并留原样本、native hash。现有输出有 median/P95/吞吐，P9 只补同一组延迟样本的 P99 与对照汇总（`tools/benchmarks/ipc_memory.py:403-434`）。

建议在批准时事先定为：六个正常性能场景分别要求 candidate P50 不超过 baseline 的 105%，P99 不超过 110%，吞吐不低于 95%；按每平台至少五组配对运行的中位比值判断。可重复相同场景累积样本，不改载荷、拓扑或配置；预算场景主要断言失败/清理正确，不把一两次调用的分位数设为性能门禁。若超阈值，先定位再由 Host 提交取舍，不测完再改标准。其余内存/借出/句柄/释放帧指标进入上面的故障断言，现有预算观察保留，copy-backed 不改称零复制。

实施接线和故障证据齐备后做独立代码审查，再以确切候选通过全平台门禁，由 Host 核对后接受实施。发布/安装另行处理；日志/receipt/source/hash 留，产物无人再用后移除，不清历史端点（`AGENTS.md:9,201,208`）。

## 九、流式 RPC 八问与顺序

本批不加流编号、窗口消息或流式 SDK，只检查共享核心是否留出后续能力（`docs/design/backlog.md:21`）。八个问题分别如下：

1. 生产快于消费时，每方向按消息、字节、未释放借出及连接总预算推进额度；等消费，不用 dedicated/文件掩盖积压。本批单分配释放独立于调用终态。
2. 双方互等时，业务队列有实际预算，控制不等业务许可。本批 Rust 读写独立、控制片间可发；Node 以后加流时再有连续读取，不把本批调用内读取承诺为流式实现。
3. 等输入/额度不占 blocking worker；领域操作才预约，不整个流持路由锁。本批保留 admission 锁/代次。
4. 每条目有独立载荷所有者/租约，checked view 失效后释放，一调用可有多个 loan_id。
5. 两方向发送结束与调用终态分开；取消不重放已发送副作用，晚释放仍有效。pending 与借出表分离。
6. 关闭预算约束调用者等待，主动取消流，但不强制复用仍在读的分配；连接仍按第四节结束。
7. 下一批做分片公平、首片准入和重组锁域；本批只保证控制跨片推进。
8. relay 每跳以有限字节队列和消费驱动读取传递背压，连同 HTTP 整体物化/内存预算设计，不只替换末端发送方式。

当前同连接同路由非分片在接收循环按帧序预约；exclusive/read_parallel 后到不能越过先到写，parallel、跨路由和独立 HTTP 无顺序；分片首片另任务预约可被非分片越过。本批不移动预约时点，也不恢复旧 Python 跨路由屏障（`docs/reference/runtime-lifecycle.md:39-41`）。

流式批建议将开始顺序定义为同连接同路由首帧/首片在接收循环的预约顺序，先做 B2/C03/C04/R14 再删排序闸门。流内按方向顺序交付，跨流操作遵守领域路由策略，不等整个长流结束；完成序与开始序分开，跨路由因果由应用 await。公开顺序契约在流式设计中另确认。

## 十、待用户决定与术语建议

决定 2 建议采用：远端 SHM hold 保留实际连接 OS 引用与映射；代价是长 hold 保留句柄和原池预算，活动 export 仍由使用者结束。决定 3 建议采用：属主停用只停业务，不按任意等待时长回收；SIGSTOP、继承 fd、不确定 I/O 可长期占用，peer end 不撤销违反契约的 raw alias。

心跳建议整删自动探测及两设置，保留手动 ping。超时按决定 3 只能停用，不能回收仍借出的块，连接仍可能关不掉；对端恢复后的下一调用反而被拒。若用户选择保留，超时仍只停用，并必须让 TS 在调用/close 读取中处理 PING/PONG；禁用时不建空任务。保留方案估删 15–40/增 20–60 行，整删估计见第五节。R35 公开函数/枚举的删除依据是整项心跳退役，不能仅借省空任务删除（`docs/design/2026-10-23-r35-heartbeat-task-ownership.md:3-13,29-31`）。

D7 建议删除连接期 process pin，仅在身份核验期临时持有，并随 ABI 7 切净；静态阅读不能证明 PID 复用竞态无影响，实施前独立评审明确退出/PID 更换条件，在 Windows x64/ARM64 原生执行。条件未闭合不批准该子项，不删身份检查或保留两套 ABI。性能门槛建议在批准时接受第八节的 5%/10%/5% 值，届时固定，不在测后决定。

### 建议由 Host 加入 CONTEXT.md

本稿开头已给操作定义，下面是待 Host 定稿的中英词条；本 worker 不修改术语表。借出 / lending：将本进程分配发布给某连接对端读取。借出表 / loan book：按物理连接保存尚未收到释放证明的真实分配。停用 / deactivation：拒绝新业务但不等于关闭 OS 流。物理连接 / physical connection：一次实际 OS 流连接，区别于 proxy。连接负责方 / connection coordinator：管理该流共同状态机的内部对象；此处不用 owner，避免与所有者混淆。

释放 / release 建议改成“结束一个租约或分配；跨进程通过连接内记录通知属主，本地成功不等于属主已回收”。删除现定义中的 “a failed release stays with its holder”，因为 R07 的 allocator 失败保留/重试已删除；活动 export 拒绝和用户 invalidator 异常是具体 API 前置条件，不再概括为该旧承诺（`CONTEXT.md:146-148`）。归还不另设词条，统一称释放；peer end 可译“对端结束”，包括 EOF 和本地流读端 ECONNRESET，避免把所有结算都描述成 read 返回 0。

R13 原实验与当前入口对照、backing 无转发器对照已列验收，静态未定的部分不冒充复现。范围外仍是已有分片首片顺序、relay-aware observed_route 非末次调用回执、TS opaque allocator 解码；E-host 已明确移入本批。此次未运行并验证新的范围外缺陷（`docs/design/backlog.md:21,23,25,29`；`docs/design/contract-release-deferred-capabilities.md:15,22`）。
