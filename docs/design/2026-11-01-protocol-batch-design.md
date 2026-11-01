# 协议批详细设计

日期：2026-11-01；代码依据为本次输入 `dac630f0131b706402e1fb2b89204443a998a06e`。本稿按独立评估、Host 代码复核与用户当日同意的方向制定：请求与响应复制完成即释放传输租约，SDK hold 只保留本地副本；删除自动心跳及两项设置，保留手动 ping；本批不加每连接借出额度。只修订设计，未改代码、构建或运行测试。范围以 `docs/design/backlog.md:17-20` 为准，并将 E-host 并入本批；Host 同步待办中的旧状态。已有论证见[单一属主评估](2026-10-31-single-owner-release-evaluation.md)、[过度防御复审](2026-10-31-over-defense-review.md)与[共享内存借出调研](../research/2026-11-01-shared-memory-lending-survey.md)。用户 2026-11-01 批准本设计，包括第十节的 D7（按所列条件）与性能门槛。

引用中的 `c2-*` 按 `AGENTS.md:107-122` 的 crate 根展开，行号属于上述代码输入。本文先定义几个设计用语：“物理连接 / physical connection”是一对已建立的 OS 流，不是 proxy；“借出 / lending”是属主通过该连接发布一个分配供对端读取；“借出表 / loan book”按连接保存尚未释放的真实分配；“停用 / deactivation”停止业务准入而保留尚需结算的连接；“连接核心 / connection core”是统一管理该连接写队列、借出表和结束状态的内部对象，代码名 `ConnectionCore`。“释放 / release”统一指结束租约或分配；跨进程时借用方交付释放记录，属主收到后回收分配。下文不用“归还”另指同一动作。“属主 / pool owner”与待删除子系统的“所有者 / owner”仍按 `CONTEXT.md:59-68,126-128` 区分。

## 一、现状与需要替换的路径

客户端每个 IpcClient 创建一个请求池并写载荷，重组另池、共用 outgoing 预算；每个 Server 创建一个响应池供所有连接共用。两端按 prefix/坐标懒打开对端段（`c2-ipc/src/client.rs:2605-2638,3273-3303`；`c2-server/src/server.rs:592-606`）。

服务端释放请求调用客户端池 free_at，客户端释放响应调用服务端池 free_at，因此双方都写对端 allocator；Unix/Windows peer 映射均可写（`c2-server/src/dispatcher.rs:293-323`；`c2-ipc/src/client.rs:484-516`；`c2-mem/src/segment/unix.rs:90-128`、`windows.rs:91-103,155-165`）。SHM 3 共享头存 PID 锁字、计数、序列、位图和活记录（`c2-mem/src/alloc/buddy.rs:31-81,297-320`）。

dedicated 读端写 read_done，属主等标志/崩溃超时退映射和预算；u16 index 单调耗尽即拒绝。文件溢出关映射后关文件，不复用旧 backing 给新载荷（`c2-mem/src/dedicated.rs:1-20`；`pool.rs:489-529,1303-1362`；`docs/reference/memory-policy.md:53`）。

生产唯一 CoreCallback 在调用资源前已将请求复制进 `Vec`，租约却留到 `EncodedService.invoke` 返回之后；PyO3 移动同一 snapshot 并给 `memoryview`。响应 `HeldResponse::from_response_lease` 同样先复制进 `Vec`，却保留 `ResponseLease` 和连接；Python buffer 指向 `held.bytes()`，Rust `Held` 包的也是这个副本。两个方向在复制后都不再读远端 backing（`c2-core/src/host.rs:1244-1278`；`lifetime.rs:13-55`；`client.rs:283-302,383-448`；`sdk/python/native/src/core_ffi.rs:471-485,517-535`；`sdk/rust/src/held.rs:5-13`）。

下表区分真正的远端原地读入口与已经复制的路径，列的是当前源码及本批目标。正常 SDK 没有需要在方法执行或 hold 期间原地读远端 backing 的使用者；内部复制仍须在读完前保护映射。检出外低层 API 的调用者未查证，不据此增设原地读 SDK 接口。

| 路径与当前使用者 | 当前是否直接读远端 backing | 本批处理及依据 |
| --- | --- | --- |
| Core 响应 → Python `ResponseBuffer` / Rust `Held` | 只在复制时读；SDK 使用本地 `Vec` | 构造 hold 前消费 carrier 并释放；去连接 pin 与待结清状态（`c2-core/src/lifetime.rs:35-55`；`sdk/python/native/src/core_ffi.rs:482-485`；`sdk/rust/src/held.rs:10-13`）。 |
| `PeerShmRequest` → `RequestLease` → `CoreCallback` | 只在 `copy_data_at` 期间读；资源方法拿副本 | 服务端在回调边界消费请求，复制后释放，再将 `Vec` 移入回调；`CrmCallback` 不再交付可被资源代码延长的传输 carrier（`c2-server/src/dispatcher.rs:271-290,370-390,498-505`；`server.rs:3075-3131`；`c2-core/src/host.rs:1244-1266`）。 |
| SHM 请求分片、普通非 hold 响应、relay 响应物化 | 均已读成副本并释放 | `consume_peer_request`、`ResponseData::into_bytes` 继续消费后释放；分片重组 backing 在接收方本地（`c2-server/src/server.rs:3548-3564,3885-3903`；`c2-ipc/src/response.rs:172-177,246-260`；`c2-core/src/client.rs:268-272`；`c2-http/src/relay/router.rs:1740-1744`）。 |
| 低层 Rust `RequestLease` / `ResponseLease` | 公共读操作只返回 `Vec`，可延后第一次复制；当前 `copy_bytes` 不释放 | 统一使用消费式 `into_owned_bytes`；分离的复制辅助函数收为内部实现，丢弃未读载荷仍可直接释放。请求 carrier 留在传输内部，删除测试回调复制后仍保留到响应之后的用法（`c2-server/src/dispatcher.rs:405-462`；`c2-ipc/src/response.rs:181-260`）。 |
| 低层 Rust `MemPool::data_ptr_at` / `seg_data_info`、`ShmRegion::base_ptr`、`BuddySegment::base_ptr` / allocator 指针、`DedicatedSegment::data_ptr` | 原始指针能原地读；生产使用者是传输复制与 FFI 复制，未找到长期原地读的应用 | 映射和指针原语服务本地内存及复制；`seg_data_info` 另供 Python 裸导出与旧 FFI 段大小查询，二者退役后删除。参与协议借出的 raw 访问由调用者保留连接与映射到最后一读，裸指针本身不延寿（`c2-mem/src/pool.rs:838-903`；`buddy_segment.rs:55-64,76-89`；`alloc/buddy.rs:326`；`dedicated.rs:78-100`；`segment/unix.rs:149`、`windows.rs:116`；`c2-mem-ffi/src/lib.rs:816-838`）。 |
| `MemPool::handle_slice`、`ReassemblyBacking::with_slice` | 原地读接收方自己分配的 backing，含文件溢出；没有远端租约 | 本地 owner / 预算跟随真实 backing；SDK 消费后即释放这份重组存储。`with_slice` 的检出调用均在测试内，不把它算作生产远端读者（`c2-mem/src/pool.rs:989-1000,1048-1067,1098-1105`；`c2-wire/src/chunk/backing.rs:235-255`）。 |
| Python `MemPool.data_addr` / `seg_data_info` 与 peer 坐标入口 | 可向 Python 泄出远端地址；`read_at` 本身会复制 | 检出内只有 `test_open_peer_segment` 使用 peer 入口，两个地址导出没有调用。P8 删除 `data_addr`、`seg_data_info`、`open_peer`、`ensure_peer_segment`、`read_at`、`free_at` 及对应旧测试；本地分配 API 留（`sdk/python/native/src/mem_ffi.rs:292-307,411-450,537-574`；`sdk/python/tests/unit/test_mem_pool.py:193-225`）。 |
| C/Node FFI `response_pool_read` | Rust 短暂从远端指针复制到 `dst`；Node 不得到映射指针 | ABI 7 将复制、结束读区间和排释放记录合成一次消费，移除旧独立 peer free。`request_pool_read_local` 读取本进程请求池，只供发送方本地回读，不算远端借用（`c2-mem-ffi/src/lib.rs:910-947,1081-1121`；`c2-mem-ffi/bindings/typescript/native/node_c2_mem_ffi_loader.c:800-817`）。 |
| 生成 TS 的 SHM 响应 | 已复制到调用方 allocator 或新 `Uint8Array`，成功后立即释放 | 保持此契约，改接 ABI 7 的消费操作；当前 `reader.read` 抛错会绕过后面的释放，P8 同时覆盖复制失败的结算，不将成功路径的结论外推到失败路径（`c2-codegen/assets/typescript_transport.ts:2475-2512,2514-2536`；`c2-mem-ffi/bindings/typescript/src/index.ts:886-902,921-932`）。 |

因此决定 2、3 收缩为低层未消费 carrier、原始远端读区间与在途借出的安全约束，不再约束 SDK hold。没有生产使用者要求将远端租约延长到 hold 释放或资源方法返回。

所有关闭入口都须替换：

- EOF/读错/DISCONNECT 当前结束接收并丢流；ConnectionReset 只分为错误，未用于 peer end 结算（`c2-server/src/server.rs:2059-2073,2544-2555`；`c2-ipc/src/client.rs:5755-5763`；`c2-local/src/observation.rs:330-351`）。
- 自动心跳超时直接 abort 的入口随心跳整项删除；手动 ping 保留（`c2-server/src/heartbeat.rs:71-94`；`c2-ipc/src/lib.rs:32-34`）。
- 写中取消由 SendGuard 毒化流，写超时/失败由 AbortOnDrop 关全流（`c2-ipc/src/client.rs:2238-2297,3934-3969,4338-4371`；`c2-local/src/lib.rs:739-764`）。
- 协议错误 abort 或 break 后丢流（`c2-server/src/server.rs:2084-2092,2492-2498`）。
- 引用只阻止空闲淘汰，close/Drop 仍清 writer/abort 并发布终态（`c2-ipc/src/pool.rs:311-319`；`client.rs:5154-5258,5409`）。
- Host shutdown 打断读取、等连接表空才关 listener；Runtime 退休封池后 close_all 强关（`c2-server/src/server.rs:804-811,1600-1610,2069-2072`；`c2-ipc/src/pool.rs:1582-1627`；`c2-core/src/session.rs:2448-2468`）。

LocalStream::shutdown 仅 flush；AbortHandle 从共享状态取走 stream，Unix 调 SHUT_RDWR、Windows 调 CancelIoEx，不能仅靠保留旧 Arc 延长租约安全（`c2-local/src/lib.rs:423-452,676-678`；`unix.rs:47-50`；`windows.rs:204-218,256-266`）。

TS 有一个静态确认、尚未运行复现的问题：读取不处理 SIGNAL，按 request_id 严格匹配；服务端默认空闲 15 秒后发 request_id=0 的 PING，下一 Node 调用会先读到它而失败。实际在后续 interval/2 tick 发送，复现须确认 PING 已发，不能只睡恰好 15 秒（`c2-codegen/assets/typescript_transport.ts:598-614`；`c2-config/src/ipc.rs:196-197`；`c2-server/src/heartbeat.rs:25-39,59-94`）。

Host 在实施同一候选中替换现行生命周期规定：`docs/reference/payload-lifetime.md:39-41,58,68-70` 改为“请求与响应的传输租约在复制完成后立即结束；请求先释放再进入资源方法，响应先释放再解码或交付 hold；本地副本的 FastDB checked view 在 hold 释放或借用输入结束时失效”。`AGENTS.md:128,206` 的保留次序改为“复制完成 → 释放传输租约 → 移动同一 `Vec` 给 SDK / 回调”，继续要求 PyO3 移动 snapshot 并提供 `memoryview`。`memory-policy.md:138-148` 和 Rust/Python usage 同步去掉 hold 拖住传输池、连接及失败重试的承诺；这些文件由 Host 改，本 worker 只写本稿。

## 二、单一释放帧与无头 SHM

保留 16 B 信封 `[total_len:u32 LE][request_id:u64 LE][flags:u32 LE]`，total_len 不含自身 4 B（`c2-wire/src/frame.rs:1-26`）。唯一释放帧用新 `MsgType::Release=0x17`、request_id=0、FLAG_CTRL，服务端加 FLAG_RESPONSE；只允许握手完成后发送，不复用 0x16。

payload 为 `[type:u8=0x17][release_count:u8][loan_id:u64 LE × count]`。协议常量 `IPC_RELEASE_BATCH_CAPACITY=64`，count 为 1–64；整帧最小 26 B、满批 530 B，total_len 为 22/526。c2-config 的 max_frame_size 下限从 >16 提到 526；常量定义在 config，供 wire/FFI/资产生成共用，避免反向依赖（`c2-config/src/ipc.rs:451-455`）。64 只限定一帧，队列更多就连续发，不限制总量、不等满批或定时器。

写端空闲立即发；下一业务帧已排队时，写任务把独立释放帧和它拼进同一次写提交。线上仍两帧，每批多 16 B 帧头；业务 payload 不变，不加扩展标志、剥离规则、握手帧限值或 ACK，固定 preamble 保持独立（`docs/reference/ipc-handshake.md:11-30`）。

本批采用控制连接上的释放帧。每连接一个共享内存归还环只作后续备选：iceoryx2 在分配前取归还队列，io_uring 的缓冲环基准比逐次提交归还快约 1.7–2.7 倍；后者是 Linux NOP 基准，不是 C-Two 的跨平台收益证明。只有三平台基准达不到第八节门槛、且定位到释放通道时才重新评估，不在本批加入开关或双轨实现。环还需要独立可写共享区、逐条校验和池满时的等待 / 唤醒设计（`docs/research/2026-11-01-shared-memory-lending-survey.md:50-56,93-94`）。

接收边界一次验类型/方向/阶段/count/精确长度，再按本连接借出表移走真实 handle，本地释放，不按对端坐标 free。零/未知/重复/未借出/已释放 ID 报 ProtocolViolation 并停用，已结算的不回滚，剩余等 peer end。第二次查不到就不碰 allocator；正常 carrier 只发一次，不增历史 tombstone/重试，内层不重复验证。

IPC 15 descriptor 保持 23 B：`slot:u16, generation:u32, offset:u32, data_size:u32, kind:u8, loan_id:u64`，整数 LE，kind 为 0 buddy/1 dedicated。loan_id 替换 allocation_id 和 dedicated 零身份，在借出方进程内唯一（进程级计数器，checked 递增，0 不用；Host 2026-11-01 在 P3 定），借出表仍按物理连接分开，所以别的连接的 ID 在本连接就是未知 ID；它不是 request_id。表存真实分配/容量（旧式 `c2-wire/src/buddy.rs:1-32,78-100`）。

SHM 4 段内无头，数据从 0 开始；格式由握手确定，kind/generation 在 OS 名中，接纳 descriptor 按 OS 实际视图 span 和 checked 算术验范围一次。位图/记录/计数/线程锁移到属主，删 magic/version/data_offset/几何头与 dedicated 头；代价是段不能独立自描述（`c2-mem/src/alloc/buddy.rs:31-81`；`dedicated.rs:13-20`）。

peer OS 映射只读，属主可写；本地锁仍串行分配，锁内不等对端/应用/I/O。保留 move-only handle 和 unsafe/FFI 输入一次验证，内层信不变量；panic 直接失败，不夺锁。连接表限定释放权限，不承诺隔离恶意同用户或撤销 raw mapping（`docs/reference/memory-policy.md:47-55`；`docs/design/2026-10-31-over-defense-review.md:11`）。

两类段都用可复用 slot、checked u32 generation，名称含原 pool incarnation/kind/slot/generation；清空本地分配和借出才复用。代次耗尽只退休该槽，全部退休走既有分片/文件，不换池/prefix/连接。loan_id/request_id checked 递增，溢出报连接级错误并停用，不自动重连/重放；旧映射不初始化（`c2-mem/src/pool.rs:1303-1362`；`docs/reference/windows-native-implementation.md:56-64`）。

## 三、共享连接核心与两个方向的释放

共同状态机随 P3 放在拟新增 `c2-wire/src/connection/`，参照已有 chunk registry（`c2-wire/src/chunk/registry.rs:1-22,79-93`）。ConnectionCore 管借出表、释放队列、写帧进度、停用和 peer end；c2-ipc/c2-server 只接池、业务调度和 I/O，Node FFI 复用它，不各写合并/关闭算法。

ConnectionCore 写成不做 I/O 的同步状态机，与 chunk registry 一样只用 parking_lot。驱动方（c2-ipc、c2-server、Node FFI）把收到的帧、写完成和读端结束的事实交给它，再按它给出的下一步去读写；读端结束仍由 c2-local 判定，经驱动方转交。这样 c2-wire 不新增对 c2-local 与 Tokio 的依赖，仍只依赖 config、contract、error 与 mem，状态机也能脱离真实流单独测试；mem 保留本地分配，不反依赖 wire（`c2-wire/Cargo.toml:10-18`；`c2-local/Cargo.toml:10-18`；`c2-mem/Cargo.toml:13-19`）。

分配填满交写任务后，先在连接锁内登记并转移 handle，再提交第一字节。写前取消本地释放；提交后不能证明未完整送达，就留到释放记录/peer end，完整帧后的 flush 失败也算借出。快速释放、写完成、peer end 只移走一次 handle，结束同时封新借出；响应/截止时间不删借出表（`c2-ipc/src/client.rs:4246-4266`；`c2-server/src/server.rs:4403-4466`）。

SHM 接纳时备好不可复制的节点，绑定原连接实际 OS 引用、mapping guard、loan_id。复制期间保护远端读；复制完成立即结束这个读区间，把释放节点移交 ConnectionCore，再将本地字节交给下一层。此后 carrier 不可读，也不留给 SDK。Drop 不临时分配、不等网络；本地移交成功不表示属主已收，属主收到记录才回收。复制失败或载荷被丢弃同样结束读区间并移交释放，不把清理责任塞进一个失败的 hold（当前消费辅助函数见 `c2-ipc/src/response.rs:246-260`；`c2-server/src/dispatcher.rs:449-462`）。

请求路径定为 `RequestData → into_owned_bytes → 释放 / 本地重组退款 → CrmCallback::invoke(Vec<u8>) → EncodedService.invoke → PyCoreRequestBuffer → memoryview`。服务端消费放在原调度预约 / commit 后、回调前，不移动开始顺序；CoreCallback 直接转交这份 `Vec`，删除其方法返回后的释放分支。借用输入只延长本地 snapshot，到方法结束先失效 FastDB checked view，再结束本地 buffer。已复制的请求不会因慢资源方法继续占用客户端 SHM（`c2-server/src/server.rs:3075-3131`；`c2-core/src/host.rs:1244-1278`；`sdk/python/native/src/core_ffi.rs:517-535,598-610`）。

响应路径定为 `ResponseData → into_owned_bytes → 释放 / 本地重组退款 → HeldResponse(Vec<u8>) → SDK 解码 / hold`。inline `Vec` 直接移动；SHM 与重组 backing 复制一次后立即释放，HTTP 已有字节直接移入。hold 只拥有本地副本，释放时先失效 FastDB checked owner，再结束本地 buffer / export；Python 活动 export 拒绝释放仍适用，但不会阻止远端回收。SDK hold 的 `lease`、连接 pin、`has_pending_release`、失败后重试 / abandon 全部删除，原 invalidator once-only 和原异常保留（`c2-core/src/lifetime.rs:13-125`；`client.rs:383-455`；`sdk/python/native/src/core_ffi.rs:283-291,367-445`；`sdk/rust/src/held.rs:41-90`）。

写任务在当前整帧后优先发释放；节点总量由已接纳但尚未发送释放的租约决定，复制后移交队列的节点仍占其中一项。分片逐帧交 writer，让控制片间推进，不做 B2/业务公平。Rust 控制读取不等写锁 / 应用 / 业务许可，错误响应也排队，继续用已有 pending / 字节预算；消费复制由载荷处理路径完成，不另在读任务复制整份业务载荷。存储不足则报告并停用，控制不能穿越 OS 阻塞的半帧（`c2-ipc/src/client.rs:4338-4371`；`c2-server/src/server.rs:2499-2514`）。

Node 仍只在调用 / close 中读：先处理释放帧，再匹配本次响应。服务端在进入回调前排请求释放，writer 在响应前发出；Node 读到响应前已能结清该请求。响应复制后立即排释放并唤醒唯一 writer，不等下一调用。复制完成后的请求晚释放用法随旧回调入口删除，因此不再为 Node 空闲期的晚释放增加后台 reader、保留表或特殊恢复。响应终态仍不能冒充释放记录；写中取消等在途借出按第四节结算（`c2-codegen/assets/typescript_transport.ts:598-616,737-753`；`c2-server/src/dispatcher.rs:498-505`）。

决定 2 的目标是：低层尚未消费的 carrier 或确实仍读远端 backing 的 raw 使用者，保留原物理连接的 OS 引用与映射到最后一次读；重连不能把旧释放记录投到新连接。生产复制路径只保留到复制完成，SDK hold、借用输入及本地重组不保留远端连接或映射。独立使用本地内存原语不引入连接；只有参与协议借出的低层访问受此约束。响应池仍每 Server 共用，连接表限定各自释放权限，不承诺容量隔离（`c2-server/src/server.rs:592-606`；第一节路径清单）。

## 四、peer end、停用与关闭

L 是本方未结清借出数，B 是本方尚未结束的远端读区间数，包含待复制 carrier。SDK hold 和资源方法使用的本地副本均不计 B。停用封新业务、取消排队并返许可；未消费请求丢弃即排释放，active callback 只留本地 snapshot / 许可到返回。记录逐条结算 L，peer end 清剩余 L，DISCONNECT 仅请求停用（`docs/reference/runtime-lifecycle.md:39-41`；第一、三节的新契约）。

peer end 包括读 0 和 Unix 本地流读端 ECONNRESET；保留 reason=connection_reset/errno，不能把写错误也作证明。Linux AF_UNIX 关闭方接收队列非空会使对端读 ECONNRESET，这是 Host 核对的 unix_release_sock 行为，Host 在 macOS 对照为 EOF；本次未运行。当前代码只分错误原因，须补读端结算。Windows 读 109/232 仍为 EOF，233 不为 EOF（`c2-local/src/observation.rs:330-351`）。

本地 cancel/超时/一般 io_error/DISCONNECT 不清 L。畸形帧不猜边界，停用后固定缓冲丢弃到 peer end；解析健康时继续收释放、立即释放迟到 SHM。不可恢复读错却无结束证明，错误原样上报，L 留原池 teardown，不偷偷复用/重试；连接结束不撤销违规 raw mapping（`docs/reference/local-endpoint-lifecycle.md:189`）。

物理 close 需停用、B=0、L=0，锁内封新租约/借出，未发布写撤销。读端已不可恢复（没有结束证明的读错、接收任务结束）时，B=0 即可关闭物理流，剩余 L 留在原池直到池销毁，绝不在没有证明时提前释放；两端用同一规则，关闭事件只发布连接最先记下的首因（Host 2026-11-01 在 P4 验收中定）。已经开始写的调用被取消时不停用连接：写端写完这次请求剩下的帧（分片也一样），调用方得到取消，分相为 DispatchUncertain，其他调用不受影响。此时释放队列未发完也可关，对端会据 peer end 结清；peer end 先清 L，本地映射留到 B=0。不依赖 half-close 或双方都等 EOF 才关的新 ACK。

调用方按原截止时间返回，writer 仍拥有已开始帧，超时只停用 / 记原因，实际 I/O 失败不重发。local 用引用计数 owner 持有实际流，未结束的远端读区间强引用它；任务 / 执行器结束不能越过这个区间关句柄 / SHUT_RDWR，旧 abort 的 take 操作删除。复制完成后本地副本不再持此引用，hold 不延长关闭；释放队列由连接驱动方独立排空。已发布借出不随任务析构 free，无结算任务时留原池占用（`c2-local/src/lib.rs:423-452,739-764`）。

Host 业务完成看 listener_closed/work_drained，区别保留连接；每次启动本就新建 Server/响应池，不加启动代次（`c2-core/src/host.rs:967-985`；`c2-server/src/server.rs:592-606,1600-1610`）。E-host 明确从 backlog.md:23 的 Core 批并入：低层 Host shutdown 不关 Runtime 调用域/outgoing，显式退休才封；当前会关闭 call_execution_observer 并混用客户端 drain，P6 分开。原 ShutdownTransaction/失败启动 receipt/RuntimeShutdownReceipt 保留，Host 同步待办（`c2-core/src/session.rs:1903-1917,1971-1981,2448-2468`）。

- 借用方 kill -9 后，属主确认 peer end 清剩余借出；接收队列非空也须结清，继承 fd 尚在则等。进程死亡另看 OS wait。
- 属主崩溃后，已交付 hold 的本地副本继续可读；只有尚未复制的低层 carrier / raw 读者仍依赖旧映射。本方请求池据 peer end 结清；可读映射不证明业务存活。
- SIGSTOP / 调试器期间，未复制的借出仍不可复用；已复制的 SDK hold 不占远端块。自动心跳删除后，仅业务截止时间结束调用等待；没有触发停用的连接在恢复后继续使用，已超时调用按原结果结束。
- fork 父退出、子持 fd 无结束；最后副本关才回收。无 fd 继续读 mapping/提前半关读 raw alias 违反租约契约，不支持（`docs/design/2026-10-31-single-owner-release-evaluation.md:43-49`）。
- 强关/协议错/写错只停用，B 等读者，L 等记录/peer end；不可按等待时长强拆，输出坏且对端不结束可长期占用。
- Host shutdown/退休的 listener/queued/active 走原事务；连接/池留原 owner，退休封旧域，新 session 不接旧借出，完成与载荷保留分开。

决定 3 收缩为在途及低层远端访问的规则：未收到释放记录或 peer end，属主就不把已发布分配交给别的载荷；心跳与等待时长均不能代替读完证明。卡在复制之前、没有读取响应或堵住释放通道的对端，仍能保留 L；这项真实代价交给异常对端基准检验。SDK 长 hold 与慢资源方法已不再延长远端借出。调研里按心跳复用的 iceoryx 一代也把这种清理判为不安全，其他做法依赖 OS 结束证明或先复制（`docs/research/2026-11-01-shared-memory-lending-survey.md:7-13,21,39-46`）。

本批不加每连接借出额度、预留池或新配置。复制后释放把借出缩到正在进行的调用及其传输结算区间，不再随应用积累的 hold 增长；这不是固定字节上限，也不抹掉未读对端的在途占用。共享池用满时沿既有分配层尝试 buddy 复用 / 扩展、dedicated，SHM 不可用则小载荷内联、大载荷分片，接收端重组可落到文件；既有物理和预算边界都耗尽时返回原容量错误，载荷不截断。分配回退保障正确性，不能据此声称异常连接之间已有吞吐或内存隔离；按流与按连接的两级窗口留给第九节流式 RPC（`c2-mem/src/pool.rs:977-1020`；`docs/reference/memory-policy.md:19-23,37-41`；`docs/research/2026-11-01-shared-memory-lending-survey.md:25-34`）。

保留现有 ConnectionEvents/预算观察，停用不提前发物理 closed，最终保留首个原因/OS cause。不增公开字段；借出/租约/队列/句柄由 crate 内访问器测（`docs/reference/runtime-lifecycle.md:77-79`；`docs/reference/memory-policy.md:138,148`）。

## 五、删除清单与估计

以下 M1–M7 将内存、释放与本地副本记账分组估量，各项的文件归属由第七节实施表决定。单位为源码物理行粗估，新增不含测试，不是净减承诺。A1/A4 计 M1、R07 与 hold 传输义务计 M2、A9 计 M3、释放认证及 Python peer 裸入口计 M4、C7/R13 计 M5；M7 单计记账迁移，不重复计算 M2 的状态删除；B6 不重复计 B1 的 watch。

| 项目 | 生产删除 | 生产新增 | 旧测试删除 |
| --- | ---: | ---: | ---: |
| M1 共享分配器防御、A1/A4 | 500–800 | 130–250 | 350–600 |
| M2 R07、hold 传输义务与 pin | 280–500 | 20–60 | 300–550 |
| M3 dedicated/A9 | 130–240 | 50–110 | 100–200 |
| M4 无头 SHM/只读 peer/释放帧与 SDK 裸入口 | 280–480 | 150–260 | 150–280 |
| M5 C2/C-U1/C7/R13 | 1,150–1,650 | 120–230 | 450–850 |
| M6 共享连接核心与复制后释放接线 | 120–260 | 360–580 | 150–300 |
| M7 SDK 租约记账改成本地副本记账 | 350–500 | 100–180 | 150–300 |
| A6/A11 | 50–120 | 5–25 | 20–60 |
| B1/B3/B6 | 550–850 | 80–160 | 230–400 |
| B4 | 100–220 | 0–30 | 80–160 |
| B7 | 120–240 | 20–50 | 100–200 |
| F5 | 60–140 | 0–25 | 40–90 |
| B9 | 10–35 | 0–15 | 10–25 |
| D7（不实施，见第十节） | 0 | 0 | 0 |
| 自动心跳及两设置，整项删除 | 160–300 | 0–20 | 100–180 |
| E-host，新增纳入范围 | 20–60 | 20–60 | 0–20 |

M1 整删 `c2-mem/src/alloc/spinlock.rs`，其 365 行起为测试，不能把全部 619 行算生产。`alloc/buddy.rs` 去共享锁字/记录和重复位图扫描，`buddy_segment.rs`、`pool.rs` 改本地分配，free 用可信 handle 的 level（`buddy.rs:31-81,297-320`；`pool.rs:789`）。

M2 删除 allocator 争用失败的保留 / 重试和 `abandoned_releases` 投影；进一步删除 `HeldResponse.lease`、`connection`、`HeldPinRelease`、`with_connection`、`has_pending_release`、hold 的 `abandon` 及 Python `_abandon_after_failed_release`。涉及 `c2-mem/src/pool.rs`、`c2-ipc/src/{client,response}.rs`、`c2-core/src/{client,lifetime,error}.rs`、`sdk/rust/src/held.rs:41-90`、`sdk/python/native/src/core_ffi.rs:367-445`、`sdk/python/src/c_two/crm/transferable.py:97-126,142-174`。R07 剩下的仅是本地 buffer 的活动 export 拒绝、checked owner 先失效、invalidator once-only、原异常与本地清理次序；删除其传输错误分支及“SHM leak / pending native cleanup”旧文案。复制 / 接纳失败仍在消费边界报告，释放队列属于连接，不向 hold 转移待结清义务。

M3 删除 `c2-mem/src/dedicated.rs:1-135` 的 read_done、`pool.rs:489-550` 的 freed_at/超时 GC/退休查询，`pool.rs:1303-1362` 改槽代次。配置定义归 `c2-config/src/pool.rs:5-16,48-60`，投影归 `c2-config/src/ipc.rs:236-317`，去 dedicated_crash_timeout_secs；`c2-mem/src/config.rs:3` 重导出不单独定义第二份。映射、预算、文件 owner 留。

M4 在 `c2-mem/src/segment/{unix,windows}.rs`、`buddy_segment.rs`、`pool.rs` 删除共享头、peer allocator/free、可写映射；P8 删除第一节的 Python peer 裸入口及 B7 退役的 FFI 段列表查询后，P1 写者删除已无消费者的 `seg_data_info`。`c2-wire/src/buddy.rs` 换 descriptor，新增单一释放帧。全部段数据从 0 开始，旧几何 attach 删除。旧接缝见第一、二节。

M5 删除 `c2-ipc/src/client.rs:534-1119` 的 retire executor/容量许可，`pool.rs:391-870` 起的结算/maintenance、UnconfirmedIdle、try_lock 轮询；`client.rs:1262-1355,2552,5184-5332` 同链收敛。pending 不再兼任释放表，未发布分配由唯一准备者清理；M6 共享核心只记一次新增，两端不再各自实现 writer 合并、停用、peer end 状态机。

M6 的连接 / 映射 guard 只覆盖未消费载荷和实际远端读区间；删除为 SDK hold 延长连接、回调返回后才释放及 Node 空闲晚释放而设的接线。共同借出表、writer、peer end 仍负责未读 / 未发送的在途义务。请求回调改接 `Vec`，Core 不再复制已物化请求；响应 Core 使用消费式复制，不添加第二份 snapshot。

M7 的使用者已核对：`c2-mem/src/lease.rs:1-291` 只保存 SDK buffer 元数据，不管理 allocator 或传输租约；该 crate 内仅 `lib.rs:16,32-35` 导出它。生产引用来自 `c2-core/src/memory.rs:29-30,205-253` 与 Python native 的 `lease_ffi`、`core_ffi`、`runtime_session_ffi`，Rust SDK 没有独立的此类记账调用。响应与借用输入目前都传 `storage="inline"`（`sdk/python/native/src/core_ffi.rs:448-467,571-594`）。因此删除 c2-mem 的整个 SDK lease 模块及导出，将现有本地副本元数据与弱观察收敛到 `c2-core/src/memory.rs`，Python 只投影；不保留旧类型别名或无消费者的 transport storage 枚举。

`memory_stats().holds` 继续以现有 `active_holds`、`total_held_bytes`、`oldest_hold_seconds` 及 `by_direction` 统计仍由 SDK buffer 保留的本地副本；`total_held_bytes` 是副本逻辑字节数，不是 SHM 分配容量或 RSS。删除 `by_storage` 和 retained SHM / handle / file 的分类；`sweep_retained` 的 `storage` 同步删除，路由、方法、方向和年龄沿已有用途保留。传输分配在复制后排释放、属主收记录后回收；接收方重组完成后复制即释放本地重组分配及其容量预算，底层 SHM / 文件 backing charge 按实际解除映射或段 decay 退款。hold 的元数据不保留旧连接、池或仅因 hold 存在的空 retired backing 域；旧本地副本仍可汇总观察。结束 export / checked owner 后才丢本地 buffer 与对应计数，避免只改统计却把 `Vec` 留到外壳析构（`c2-mem/src/lease.rs:55-112,229-289`；`c2-core/src/memory.rs:155-172`；`sdk/python/native/src/core_ffi.rs:375-420`）。

A6/A11：`c2-mem/src/pool.rs:924,1049-1111` 去每次访问的重复证明；`budget.rs:121`、`c2-config/src/ipc.rs:113`、`c2-ipc/src/client.rs:2939` 去第二份限额/比较。配置只作输入，冻结后执行和观察使用同一 MemoryBudget，不让 config 反依赖 mem。

B1/B3/B6：`c2-wire/src/{msg_type,route_catalog_control,handshake}.rs` 删 RouteWatch、RouteContract 查询、RouteAck、NACK nonce、owner_epoch/lease_deadline_ms、watch revision；`c2-server/src/catalog.rs` 删事件缓存与第二份 route map。Dispatcher 为唯一真相，握手/lookup 投影已验引用，去七字符串克隆/重验。RouteContractWire 仍供 lookup，保留（`route_catalog_control.rs:51-169`；`catalog.rs:11-86,283`；`dispatcher.rs:706-735`）。

B4：wire、IPC directory、Core token、HTTP header/resolve/mesh、TS/Python/Rust 同删 route_revision/observed revision，不填常量 1。保 route_uid、server instance、契约及真实准入代次；TS 2^53 问题随字段消失（`c2-wire/src/handshake.rs:179-191`；`route_catalog_control.rs:89-93`；`c2-codegen/assets/typescript_transport.ts:1840,2327,3600,3859`）。

B7：ACK 只留单字节 MsgType::ShutdownAck，去 acknowledged/shutdown_started/server_stopped/route_outcomes 及空结构。owner 拒绝删、畸形 initiate 直接失败后不需负 ACK，Core 只投影已发起或未确认，完成看原 receipt。删恒开能力位、握手段列表、FFI 列表查询、TS eager-open；保真实方向 SHM（`c2-wire/src/shutdown_control.rs:5-35`、`handshake.rs:133-161,201-213`；`c2-server/src/server.rs:2720-2749`）。

F5：`c2-http/src/relay/{peer,types,route_table,peer_handlers}.rs` 去 DigestDiff Active/Deleted 冗余 hash、removed_revision、ROUTE_HASH_PEER_VERSION；保 DigestEntry 比较 hash，本地重算。RouteAnnounce 本来无 hash（`peer.rs:8-9,60-95,256-278,380-397`）。

B9：PreparedShmResponse::new 已 crate-private，producer 是固定 pool helper；去 `c2-server/src/dispatcher.rs:106`、`server.rs:4554` 的重验，unsafe write 仍履行原所有权契约（`dispatcher.rs:56-86,106-117`；`response.rs:30-38,74-86`）。

D7 不实施（见第十节）。原计划是：身份核验期间留临时 handle，删除长期 pin/finalizer，涉及 `c2-local/src/windows.rs:39-67,118-160`、`c2-mem-ffi/src/lib.rs:598-655`、`c2-mem-ffi/include/c2_mem_ffi.h:126-141`、`c2-mem-ffi/bindings/typescript/native/node_c2_mem_ffi_loader.c:1180-1185,1361-1377`。pipe owner、TokenUser、integrity、活性及 PID 回查仍在首字节前完成；第十节条件先闭合。这里的 process pin 不等于第四节租约需要的连接 OS 句柄引用。

自动心跳整项删除：`c2-server/src/heartbeat.rs`、server spawn / 收尾、公开 `run_heartbeat` / `HeartbeatResult`，以及 `heartbeat_interval` / `heartbeat_timeout`、`heartbeat_interval_secs` / `heartbeat_timeout_secs` 与 `C2_IPC_HEARTBEAT_INTERVAL` / `C2_IPC_HEARTBEAT_TIMEOUT` 在 c2-config、Python、CLI / env 的定义、投影、文档和旧测试。手动 PING / PONG 留。原 R35 因仅省禁用空任务而暂缓，本次按整项退役删除；自动探测不能证明远端读完，停用既不能回收未读块又妨碍暂停后恢复。存活结算依 OS 结束事实，调用等待依业务截止时间（`c2-config/src/ipc.rs:48-49,131-132,196-197,481-506`；`docs/design/2026-10-23-r35-heartbeat-task-ownership.md:3-13,29-31`；`docs/research/2026-11-01-shared-memory-lending-survey.md:39-46`）。

### 所有者子系统与保留部分

P0 是前置的所有者删除块，以下单独审查，粗估生产删 3.4–4.1 千行、增 40–140 行机械接线，旧测试/夹具删 4.6–6 千行；分项估计如下，不重复计算 owner_ffi。新增仅为保留普通 Host 代码的机械移动，不改协议释放/关闭行为。

- OwnerBound、监督、宽限：删 `c2-config/src/lifecycle.rs` 及导出、`c2-core/src/owner_bound.rs:133-625`、`host.rs:967-1128`、session owner attach 分支。`owner_bound.rs:17-130` 中性 snapshot/cell 移出，去 owner phase/policy；`c2-server/src/admission.rs:18-89` 锁/代次留，91-138 pause/control 删。估生产删 780–1,000、增 40–100，旧测试删 900–1,300。
- 恢复凭证、接管、控制协议：整删 `c2-core/src/owner_control.rs:1-939`、`c2-wire/src/owner_control.rs:1-340`、`c2-server/src/owner_control.rs:1-245`；`msg_type.rs:31,59` 去 0x16，`server.rs:2144-2230` 去 owner 角色分类/切换及 admin 拒绝。普通握手 fence/SIGNAL 留。估生产删 1,550–1,650、增 0–20，旧测试删 2,300–2,700。
- bootstrap、托管子进程：删 `c2-local/src/owner.rs`、`owner/{unix,windows,tests}.rs`、Windows OwnerClient/OwnerServer；Python owner_ffi 去 OwnedChild/shared reaper；`c2-local/src/lib.rs:37-63` owner 专用 EndpointContextBinding/bind 变体删，普通目录 pin 留。估生产删 800–1,000、增 0，旧测试删 500–800。
- SDK、测试：删 `sdk/rust/src/owner.rs`、导出和 examples/owned_child.rs；Python owner.py、config/lifecycle.py、native/src/owner_ffi.rs 控制投影/注册；`registry.py:338-386,450-452,1029-1075`、`runtime_session_ffi.rs:1165-1218,1840-1901` 去入口。删 c2-core/tests/owner_bound_lifecycle.rs、Python owner 集成/fixture/单测，普通 Host/exports/代次测试留。独立余量估生产删 250–450、增 0–20，旧测试删 900–1,200。

Python 删除 OwnerBootstrap、OwnerControl、OwnerRecoveryTicket、PreparedOwnerTakeover、OwnerStatus、OwnerControlError、OwnedChild、owner_control_pair、prepare_owner_takeover、spawn_owned_child、LifecycleConfig；Rust 另删 OwnerControlErrorKind、OwnerPhase、ServerLifecyclePolicy，set_server 的 lifecycle/owner_control 同删（`sdk/python/src/c_two/owner.py:2-14`；`sdk/rust/src/owner.rs:5-9`）。

普通 shutdown/lifecycle 留（`c2-core/src/host.rs:548-674`、`session.rs:1857-1890`）；端点凭据/inspect/reap/sweep 留（`c2-local/src/lib.rs:70-105`；`docs/reference/local-endpoint-lifecycle.md:132-156`）；admission 锁/代次和 relay route owner/liveness 留。Windows identity 测试删 bootstrap 断言，普通 pipe owner 检查仍执行（`tools/ci/source_validation.py:368-375`）。

当前文档更新由 Host 执行：CONTEXT、AGENTS、根 README、Rust README、Python usage、configuration、runtime / endpoint lifecycle、handshake 的 owner 名字、参数、示例，以及第一节列出的复制后释放与第五节的本地 hold 统计契约。主要接缝为 `docs/reference/local-endpoint-lifecycle.md:88-126,193`、`ipc-handshake.md:9,30-32`、`payload-lifetime.md:39-70`、`memory-policy.md:138-148`。旧计划有效说明先移 reference，再删 superseded owner 计划；历史 acceptance / release 保留。文档估删 300–500、增 100–180。新释放 / 故障测试估 850–1,500 行，Node / FFI 改写估 130–250 行；P9 的原始样本、同时起跑与异常基准另估新增 140–260 行。删除只钉旧源码 / 名字的测试，最终分源码、测试、文档核算。

## 六、其余并入项与版本

请求号线上 u64，pending/AtomicU32 为 u32，接收明确 as u32，确有高位截断。计数器、pending、取消/控制/分片 key 同改 u64，TS 用 bigint；两个低位相同高位不同的 ID 不能误投递，checked 溢出按第二节报连接级错误。估改 50–150 行（`c2-wire/src/frame.rs:24`；`c2-ipc/src/client.rs:2124-2159,2477,2638,5771`）。

R13 只查写前未发布分配，已验收的 snapshot/拼帧/prepare/send/inline 移交不重做。旧记录与当前 call_buddy_stream 取消后 alloc_count=0 的测试不一致；实施用原实验对照普通 SHM、stream prepare、服务端 prepared reply，缺细节由 Host 补，不编造 fail-first（`docs/design/2026-10-13-local-slimming-batches.md:34`；`c2-ipc/src/client.rs:4187-4206,8367-8415`；`c2-server/src/server.rs:11683`）。

Rust/Python/Node 同候选切 IPC 15、SHM 4、ABI 7、mesh 7，当前为 14/3/6/6（`c2-wire/src/handshake.rs:39`；`c2-mem/src/alloc/buddy.rs:24`；`c2-mem-ffi/src/lib.rs:15`；`c2-http/src/relay/peer.rs:8-9`）。25 B preamble 不变，旧 metadata/段头/ABI 符号组/别名/loader/JSON 字段全部删除；新版本包旧字段也拒绝。mesh equality 已修，F5c 不重做（`c2-http/src/relay/peer_handlers.rs:38-39,663`）。

ABI 7 删除旧 peer `response_pool_release`、请求 `forget_consumed` 与独立 read 后等待 SDK release 的拆分契约。native memory context 复用 ConnectionCore，绑定已经验证的连接；接纳后一次消费完成“只读复制到调用者 `dst` → 结束远端读 → 排释放”，失败和未读丢弃也在该边界结算。C descriptor 用 `loan_id`，未发布分配用 opaque owner；FFI 不加第二收包循环，Node 在调用 / close 中读控制帧，唯一 writer 立即发释放。旧 reader / writer 适配接口同候选删除。TS / Node 持续使用调用者 allocator 的本地 buffer，hold 不拖住远端连接，HTTP-only 不加载 native（`c2-mem-ffi/src/lib.rs:1081-1167`；`c2-codegen/assets/typescript_transport.ts:576-616,737-753,2470-2536`）。完整 R16 迁移仍属另批。

握手删除段列表，只交换 prefix、真实 SHM 方向能力和已有身份/路由字段；不新增 frame-limit 字段或 segment announcement。Windows 跨 session 继续 inline/chunk；端点命名、TokenUser、Unix namespace 3/credential 4 不变，区别于 owner recovery ticket（`docs/reference/ipc-handshake.md:62-64`；`docs/reference/local-endpoint-lifecycle.md:21-31`）。

## 七、实施顺序与文件责任

P0–P9 是实施审查单元编号。P0 先做纯删除并单独审查，之后冻结为实施基线；它与后续块可能先后修改同一混合文件，但不并行。后续块从该基线出发，各文件只有一个写者。`docs/**`、AGENTS.md、CONTEXT.md 和根 README 始终由 Host 维护，不放入任何委派块；表中范围均扣除这些路径，SDK/CLI README 也由 Host 维护并从 P7/P8 扣除。

P0 按第五节所有者清单删能力、导出、配置、测试和 0x16 角色；只机械移出保留的普通 Host 代码，不做单一属主、E-host、自动心跳或新 writer。审查检查普通 Host shutdown/lifecycle/admission/凭据仍在，owner 公开名字和无用壳消失。

配置、内存和共同核心严格按 P2、P1、P3 的顺序交付。原因是 c2-mem 依赖 c2-config、c2-wire 依赖 c2-mem；c2-ipc 集成测试依赖 c2-server，不能在服务端接线前要求客户端独立完整通过（`c2-mem/Cargo.toml:13-19`；`c2-wire/Cargo.toml:10-18`；`c2-ipc/Cargo.toml:25-27`）。

| 块 | 唯一写入范围 | 内容与退出检查 |
| --- | --- | --- |
| P2 配置/本地流 | c2-config/**、c2-local/**、c2-local-security/** | PoolConfig/超时投影、满批帧下限、删除心跳两设置；只在远端读区间保留 OS 引用、读端 ECONNRESET/Windows 分类、D7；配置和 native stream/身份测试。 |
| P1 内存，等 P2 | c2-mem/** | 无头 SHM、只读 peer、本地 allocator、槽代次/预算、去 read_done/跨进程锁；SDK lease 模块随 P6 接入 Core 本地记账时同候选删除；映射/旧代次/身份/复用检查。 |
| P3 共同核心，等 P1 | c2-wire/** | codec、单一释放帧、ConnectionCore 的队列/合并/发布/停用/peer end、边界测试和故障状态机；两端直接复用，不另造算法。 |
| P5 服务端，等 P3 | c2-server/** | 接业务调度与响应池，消费请求后释放，再把 `Vec` 交回调；删心跳/重复路由真相；验证 queued/active、复制失败、原开始顺序。 |
| P4 客户端，等 P5 | c2-ipc/** | 接请求池/pending/u64、消费式响应复制，删 C2/C-U1/C7；聚焦接线，再与真实服务端跑在途取消/peer end、relay unset/unavailable 的 direct IPC。 |
| P6 Core，等 P4/P5 | c2-core/** | hold 只留本地副本、删 pin/pending release/R07 传输分支；承接 M7 本地记账；E-host、退休/Host 重启、原收据。 |
| P7 relay，等 P3 字段，可与接线并行 | c2-http/**、cli/** | mesh 7/hash/revision、路由 token、删除心跳 CLI 投影；新旧双向拒绝、announce/withdraw/diff、普通维护。 |
| P8 SDK/ABI，等 P6/P7 | c2-mem-ffi/**、c2-codegen/**、sdk/python/**、sdk/rust/**、examples/** | ABI 7 一次消费、Node 控制读取/立即释放、删除 Python peer 裸入口；互通/本地 hold 统计/exports/FastDB checked view/borrowed。 |
| P9 整体验收，等全部 | .github/**、tools/**、tests/repo/**、必要根 manifest/lock | 全平台 runner、逐次原始延迟样本、多进程同时起跑、异常对端/回退/释放拥塞基准、P99/失败率/占用/句柄、receipt/source/hash；代码修正回交原块。 |

**给各块的共同要求。** 0.x 一次切干净：不留别名、旧路径、兼容参数或"已删除"的提示，不加防御性检查，校验只在边界上做一次，出错直接失败。缺陷修复先写能复现的测试；被取代的代码与测试一并删除。测试只用测试自己创建的端点，临时探针用新建的 0700 目录作 IPC 根；不启动、停止或重启任何日常服务。写入范围之外什么也不删，不用 git stash，不动分支与标签；要改范围外的文件就停下来问。依赖相邻 `../fastdb` 的门禁由 Host 补跑；需要链接 FastDB 时，按仓库脚本准备官方 CoreSDK 并以 system 方式链接。Python 套件经 `tools/dev/test_python.py` 运行，它拒绝没有登记的跳过。一步检查做完、后面不再需要的构建产物立即删掉，cargo 共用 `CARGO_TARGET_DIR=<检出>/core/target`，只留 `target/<块名>-validation/` 下的报告与日志。

P3 单独测共同算法；P4/P5 的测试不各复制一套状态机断言。M7 迁移由 P1 写者在 P6 / P8 接入时完成 c2-mem 的删除，各文件仍只有一个写者，整候选不留旧记账别名；这只是顺序，不作为中间状态的验收理由。跨语言测试归 P8，根锁文件由 P9 单一更新。先跑影响范围检查，接线齐后跑 core workspace，全量长任务 Host 统一调度，Cargo 共用 `<checkout>/core/target`。Host 另行更新 backlog、当前参考文档和下节术语，避免实施者同时写文档（`AGENTS.md:198-210`）。本次仅写本设计，未执行上述检查。

## 八、故障验收与基准

确切候选跑 GitHub Actions Linux、macOS、Windows x64/ARM64 与适用 Linux 3.14t，留 commit/run URL、FastDB 输入、native 架构、加载路径和 SHA-256。沿现 runner 跑 core/CLI/Rust SDK、Python full/native/3.10 syntax、Node loader/package、严格 18 行 Rust/Python 与 12 行 TS direct/relay、普通用户消费者和 Windows 跨 session；旧 run/交叉编译不替代（`AGENTS.md:7-9,93`；`.github/workflows/ci.yml:84-104`；`tools/ci/source_validation.py:368-458`；`docs/reference/development.md:49-57`）。

故障测试用 crate 内访问器，不增加公开观测：

- 双向断言复制完成就结束远端读，服务端请求释放先于回调，响应释放先于 SDK 解码 / 交付 hold；SHM 分配能在资源方法阻塞或 hold 尚活时复用且本地副本内容不变。覆盖复制失败、未读丢弃、一调用多分配、空闲立即发送、两帧同一次写、超过 64 条分多帧、片间控制。配置恰好允许 526、低于它拒绝；未知/重复/跨连接 ID、错误长度/阶段/方向、旧 IPC/SHM/ABI 均不得误释放。
- 客户端和 Host 分别在登记前、写中、整帧后、callback/hold 时 kill -9；增加受害方接收队列非空一档，在 Linux 和 macOS 都跑。先确认存在未读响应，再终止，保留读取结果及 errno，确认 Linux ECONNRESET/macOS EOF 都清剩余借出；Windows 终止/wait 单独留原生证据。
- SIGSTOP / 调试器在复制前或 raw 远端读期间停住并超过原心跳窗口，未结清块仍不复用；未触发停用时恢复后调用继续，已超时调用保持原结果，随后可结清释放。fork 父死子持 fd 时不结束，最后副本关闭才回收；exec CLOEXEC 与 Windows HANDLE 继承分别验证。不宣称支持无 fd 继续读 mapping。
- 分开验证两类寿命：SDK hold / export 活着时走第一节全部关闭入口，关闭和 peer mapping 回收不等 hold，本地值继续可读；释放 hold 才失效 checked view / 本地 buffer，活动 export 拒绝只影响本地释放。低层在复制中或 raw 远端读区间走同样关闭入口，原任务 / 执行器先结束也不能越过该区间制造 peer end；最后一读结束才可关。内部断言借出、映射 / 预算、帧队列、句柄寿命及无双重释放。
- `memory_stats()` 核对 hold 只增长本地副本逻辑字节；重组预算复制后即退款，SHM 借出属主收到记录即清，段 backing charge 按真实段寿命下降。长 hold 不增加远端映射 / 连接句柄，也不独自保留 retired backing 域；释放本地 buffer 后计数归零。Python / Rust 的 checked view、invalidator 异常、unwind 和原始异常语义均覆盖。
- E-host 检验低层 Host 关闭后 outgoing 与有限调用仍可用、可启动新 Server；显式退休封旧域；原 hold/receipt 不被新 session 接管。写中取消不 abort 共享流，不重放副作用；pending 满/错误积压时控制读取不等业务许可。
- Node 在旧基线上启用默认心跳，确认服务端已经发出 PING 后发起下一调用，复现 SIGNAL/request_id 问题。本次只有代码证据；候选删除自动心跳后验证同样空闲仍成功，手动 ping 单独验证。改测请求释放先于慢回调 / 响应、响应复制后空闲立即发释放，调用结束不用等下一次调用才退预算；复制 / allocator 失败也结算，旧晚释放回调不再作为 SDK 契约。
- backing 现象做透明转发器与完全不经转发器的 explicit IPC 对照，分正常释放/退出、kill、retire；记录进程/fd/handle、名称、内部借出/租约、peer end、unlink/open、OS wait 时间线。区分真实 owner 未结束、名称未 unlink、仅旧 mapping 可读，只清实验对象，不能先定转发器是根因（`docs/acceptance/2026-09-26-memory-policy.md:61-66`）。

正常性能沿用 `tools/benchmarks/ipc_memory.py:681-760` 既有九个场景：小 RPC、四客户端突发、4 MiB 科学载荷的 buddy 开 / 关对照，以及三个预算场景。Linux、macOS、Windows 在同机器、依赖 / profile / 配置下比较确切 baseline / candidate 的 P50、P99、吞吐，轮换运行并留 native hash；另加下列异常工作负载，不用正常场景的汇总掩盖正常客户端受到的影响。

- 一个客户端连续取得响应后长期 hold，另一个正常客户端同时调用同一 Host；用同一请求形状另跑慢资源方法对照。确认释放记录处理后借出不随 hold / 方法寿命延长，正常客户端持续推进，原副本不随池块复用改变。
- 一个客户端在接纳响应但尚未复制、或停止读取连接时卡住，另一个正常客户端同时运行；Linux / macOS 用受控停止点与 SIGSTOP，Windows 用原生停止点。分别记录卡住前、期间、恢复 / 退出后的正常客户端数据，覆盖仍真实占用池的在途情形。
- 用现有池大小与预算设置，使卡住连接的在途数据确实填满 buddy，再分别触发 dedicated、内联 / 分片及接收端文件重组；保留可用回退资源，断言正常客户端成功且内容完整。另设所有既有预算都耗尽的一档，断言原容量错误与清理，禁止截断；不把这一档当隔离或性能成功。
- 控制连接写端空闲、业务持续写、半帧写阻塞、连续超过 64 条释放积压分别测释放拥塞。确认 Rust 控制读取独立、可在分片间推进，恢复后排空且无重复释放；Node 响应复制后的释放不依赖下一调用。记录释放排队到属主结算的区间及积压，不把 OS 阻塞半帧说成可被控制帧越过。

每档异常场景均跑正常客户端独占、与异常客户端并行两种对照，并对 baseline / candidate 做相同负载的配对运行。逐进程记录正常客户端 P99、成功 / 失败 / 超时的数量与失败率、吞吐，双方当前 / 峰值 RSS、各 backing / 重组预算、在途借出 / 释放积压及 fd / HANDLE / mapping 数；按卡住、恢复、关闭事件留时间线。样本保留每次调用的进程、请求标识、起止 / 延迟、结果与配置 / commit / native hash，超时和失败不能从分母消失；P99 的样本集合与取法写入汇总。借出和队列使用内部探针，不新增产品观测项。

P9 补齐基准工具：当前只在内存中收集 `latencies`，输出 median / P95 等汇总，没有保留逐次样本；多 worker 由 controller 逐个启动，没有同时起跑屏障（`tools/benchmarks/ipc_memory.py:287-325,392-434,546-550`）。加入原始样本落盘、P99 / 对照汇总、多进程就绪后统一起跑与异常停止 / 恢复协调；异常一方进入指定状态后才开始正常方测量，记录屏障和停止点。P9 同时维护统计字段收缩后的读取，完整保留样本与失败摘要，不只补一个 P99 数字。

正常场景门槛建议维持：六个正常性能场景分别要求 candidate P50 不超过 baseline 的 105%，P99 不超过 110%，吞吐不低于 95%；按每平台至少五组配对运行的中位比值判断，批准时固定。重复相同正常场景累积样本时不改载荷、拓扑或配置；预算耗尽场景主要断言错误 / 清理正确。异常场景先以正常客户端有可用回退资源时无新增失败、数据完整、释放恢复后占用和句柄收敛为正确性条件，P99 / 失败率等原样对照，不套用正常场景的百分比，也不臆定隔离保证。超阈值先定位再由 Host 提交取舍；释放通道被证实为瓶颈才考虑第二节的共享内存归还环。copy-backed 继续按已实现的副本契约表述。

实施接线和故障证据齐备后做独立代码审查，再以确切候选通过全平台门禁，由 Host 核对后接受实施。发布/安装另行处理；日志/receipt/source/hash 留，产物无人再用后移除，不清历史端点（`AGENTS.md:9,201,208`）。

## 九、流式 RPC 八问与顺序

本批不加流编号、窗口消息或流式 SDK，只检查共享核心是否留出后续能力（`docs/design/backlog.md:21`）。八个问题分别如下：

1. 流式生产快于消费时，设计每流与每连接两级字节窗口：前者约束一条流积压，后者约束连接总缓冲，控制不占业务窗口。HTTP/2、QUIC 采用这两级窗口；iceoryx2 的最坏情况预留另需事先确定借用方数量和容量（`docs/research/2026-11-01-shared-memory-lending-survey.md:25-34`）。这些机制留给流式 RPC，本批不加额度或窗口消息；当前一次调用的复制后释放与既有分配回退足以维持载荷正确性，不能用文件溢出来替代未来的持续消费背压。
2. 双方互等时，业务队列有实际预算，控制不等业务许可。本批 Rust 读写独立、控制片间可发；Node 以后加流时再有连续读取，不把本批调用内读取承诺为流式实现。
3. 等输入/额度不占 blocking worker；领域操作才预约，不整个流持路由锁。本批保留 admission 锁/代次。
4. 每条目分开定义传输租约与本地载荷所有者；沿用本批复制后释放时，复制后排释放、属主收记录后结清 `loan_id`，checked view 到本地条目结束才失效。一调用可有多个 `loan_id`；若未来出现原地读使用者，再以其最后一次读决定传输释放。
5. 两方向发送结束与调用终态分开；取消不重放已发送副作用，晚释放仍有效。pending 与借出表分离。
6. 关闭预算约束调用者等待，主动取消流，但不强制复用仍在读的分配；连接仍按第四节结束。
7. 下一批做分片公平、首片准入和重组锁域；本批只保证控制跨片推进。
8. relay 每跳以有限字节队列和消费驱动读取传递背压，连同 HTTP 整体物化/内存预算设计，不只替换末端发送方式。

当前同连接同路由非分片在接收循环按帧序预约；exclusive/read_parallel 后到不能越过先到写，parallel、跨路由和独立 HTTP 无顺序；分片首片另任务预约可被非分片越过。本批不移动预约时点，也不恢复旧 Python 跨路由屏障（`docs/reference/runtime-lifecycle.md:39-41`）。

流式批建议将开始顺序定义为同连接同路由首帧/首片在接收循环的预约顺序，先做 B2/C03/C04/R14 再删排序闸门。流内按方向顺序交付，跨流操作遵守领域路由策略，不等整个长流结束；完成序与开始序分开，跨路由因果由应用 await。公开顺序契约在流式设计中另确认。

## 十、待用户决定与术语建议

已批准（用户 2026-11-01）。D7：删除连接期 process pin，仅在身份核验期临时持有，并随 ABI 7 切净；静态阅读不能证明 PID 复用竞态无影响，实施前独立评审明确退出 / PID 更换条件，在 Windows x64 / ARM64 原生执行。条件未闭合不批准该子项，身份检查仍在首字节前完成（`c2-local/src/windows.rs:39-67,118-160`；`c2-mem-ffi/src/lib.rs:598-655`）。结果（2026-11-01）：P2 的分析指出，现有的长期 pin 覆盖的是 `OpenProcess` 成功之后的 PID 复用窗口，从第一次取得 PID 到 `OpenProcess` 之间本来就没有覆盖；改成只在核验期临时持有，会在第一个字节之前和连接期间留下无人覆盖的进程退出与 PID 复用窗口，要证明安全还需要在 Windows 原生机器上验证旧管道的身份、句柄继承与复制行为。条件未闭合，按批准时的规则 D7 不实施，保留现有 pin，第五节的 D7 一行与 ABI 7 里相关的符号删除一并取消。性能门槛固定为第八节的 5% / 10% / 5%，不在测后修改。

### 建议由 Host 加入 CONTEXT.md

本稿开头已给操作定义，下面的中英词条由 Host 同步进 CONTEXT.md，本 worker 不修改术语表。借出 / lending：将本进程分配发布给某连接对端读取。借出表 / loan book：按物理连接保存尚未收到释放证明的真实分配。停用 / deactivation：拒绝新业务但不等于关闭 OS 流。物理连接 / physical connection：一次实际 OS 流连接，区别于 proxy。连接核心 / connection core：管理该流共同状态机的内部对象，代码名 `ConnectionCore`；不用 owner，避免与所有者混淆。

hold 改为“调用返回后继续保留 SDK 可用的本地响应副本，直到释放”；租约仍指传输对 buffer 的访问权，复制完成即结束，借用输入仍只在资源方法期间有效。载荷所有者与 checked view 的失效针对本地副本，不能再写成远端回收的前提（`CONTEXT.md:110-124`；`docs/reference/payload-lifetime.md:58,68-70`）。

释放 / release 改为“结束一个租约或分配；跨进程通过连接内记录通知属主，本地成功不等于属主已回收”。删除现定义中的 “a failed release stays with its holder”，因为 R07 的 allocator 失败保留 / 重试和 hold 待结清义务已删除；活动 export 拒绝和用户 invalidator 异常按本地 API 条件处理（`CONTEXT.md:146-148`）。归还不另设词条，统一称释放；第二、八节的“共享内存归还环”是调研备选的名字。peer end 可译“对端结束”，包括 EOF 和本地流读端 ECONNRESET。timeout 词条同时去掉已退役的 `C2_IPC_HEARTBEAT_TIMEOUT` 示例（`CONTEXT.md:154-155`）。

R13 原实验与当前入口对照、backing 无转发器对照已列验收；缺原实验细节及实际平台运行证据的部分仍未定。此次代码阅读另见 TS `reader.read` 异常绕过释放，已归第一节与 P8 的消费边界；未运行复现（`c2-codegen/assets/typescript_transport.ts:2483-2487,2516-2519`）。范围外仍是已有分片首片顺序、relay-aware `observed_route` 非末次调用回执、TS opaque allocator 解码；E-host 已移入本批。本次未增加已验证的范围外缺陷（`docs/design/backlog.md:21,23,25,29`；`docs/design/contract-release-deferred-capabilities.md:15,22`）。
