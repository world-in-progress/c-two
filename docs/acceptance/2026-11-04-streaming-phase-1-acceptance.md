# 流式 RPC 第一阶段验收记录

日期：2026-11-04。基线：上游 main `d772ca3a`。依据：[流式 RPC 设计](../design/2026-11-03-streaming-rpc-design.md)第十节与 [ADR 0003](../decisions/0003-server-streams.md)。第一阶段是块 1 至 4，仍为 IPC 15、SHM 4、C/FFI ABI 7 与 relay mesh 7，服务端流本身还不可用。代码与文档在分支 `socu/streaming` 上，尚未合入上游 main。

## 确切候选与 CI

确切候选是 `787d4205b5e9a23505c22b8155cf18a5a92dcad2`。[Source Validation 41857452303](https://github.com/Dsssyc/c-two/actions/runs/41857452303) 在它上面每条线都通过：Linux x64 的 Python 3.12 与 3.14t、Windows x64 与 ARM64 以及 workflow policy 在第一次尝试通过；macOS arm64 的作业前两次尝试都因 GitHub 托管运行器容量不足没有分到运行器，排队 15 分钟后被取消，于是只重跑这一条线，第三次尝试通过。确切候选之后只有文档提交。

在它之前的两次全平台 CI 各暴露出测试里的竞态，修正都只改测试，产品代码没有改。

第一次全平台 CI 是 `9793d7984762d7799f5654c363aee67dea04048c` 上的 [Source Validation 41834607846](https://github.com/Dsssyc/c-two/actions/runs/41834607846)。Linux x64 的 Python 3.12 与 3.14t、macOS arm64 与 workflow policy 通过；Windows x64 与 ARM64 只有 core-test 一项失败，两条线都失败在同一个测试上：c2-wire 的 `typed_gc_and_duplicate_are_rechecked_after_pool_contention`，`active_count()` 读到 2，预期是 1。其余门禁在两条 Windows 线上都通过，包括 Python 套件、直接 IPC 与 relay 矩阵、Node 测试和标准用户的消费测试。

原因在测试。块 1 之后，GC 先把过期条目的状态写成 Retired，`contains()` 随即返回 false，之后才 detach 这个条目、把计数减一；测试等到 `contains()` 变成 false 就断言计数，没有先等 GC 线程返回，所以会读到中间状态。产品里 `active_count()` 只用来触发提前清扫和填写统计，这个短暂的中间状态不影响正确性。在 macOS 上把这两步之间的窗口拉开 50 ms，测试以同样的 `left: 2, right: 1` 失败；把计数断言移到 join GC 线程之后，窗口拉开时也通过，GC 线程在测试放开公共池写锁之前就返回了。修正是 `d3125b15`，只改这个测试；之后 c2-wire 的 152 项 lib 测试通过，这个测试循环 30 次都通过。

修正后的第二次全平台 CI 是 `d3125b1583158258732992c58605e0aad47e6a36` 上的 [Source Validation 41840192706](https://github.com/Dsssyc/c-two/actions/runs/41840192706)。Windows x64、macOS arm64、Linux x64 的 Python 3.14t 与 workflow policy 通过，上面那个测试在两条 Windows 线上都通过了；另有两处偶发失败，都在测试里，第一次 run 在这两处都通过：

- Linux x64 的 Python 3.12：core-test 超时，挂住的是 c2-server 的 `raw_buddy_request_releases_before_blocked_callback_completes`。这个测试是协议批加的，读到 release 帧就断言回调已经开始；而服务端按设计先复制请求、发出 release，再执行资源方法，这个前提并不成立。断言失败之后，被阻塞的回调永远等不到放行，运行时退出时一直等它，测试就挂住了。在 macOS 上让回调晚开始 200 ms，测试同样先断言失败、再挂住。修正 `2ab164c4` 改为有上限地等待回调开始，并在测试失败时也放行回调：回调晚开始 200 ms 时测试 0.22 秒通过，人为注入一次失败时 1 秒内报出失败；c2-server 的 210 项 lib 测试通过，这个测试循环 30 次都通过。
- Windows ARM64：Python 的 `test_call_options.py::test_retired_domain_and_shutdown_report_unfinished_charges_honestly[rpc-ipc_connection0]` 在第二次 shutdown 报告完成之后，读到 `used_operations` 仍为 1。有限调用的名额由原生任务持有，任务发布了迟到的回复之后才释放，可能晚于 shutdown 观察到连接排空；这个顺序从 0.7.1 起就是这样，块 3 之前名额随输入一起，也在发布结果之后释放。在 macOS 上把发布结果与释放名额之间拉开 300 ms，测试在同一处失败。修正 `787d4205` 改为有上限地等待名额归零，与同一文件里已有的写法一致；拉开窗口时通过，正常构建下这个文件 46 项通过、1 项跳过，循环 20 次都通过。名额晚于唤醒调用方释放，还会让紧接着发出的下一次调用在名额用满时被误拒，这一项作为产品修正记在设计第十节块 6 的顺带项里。

配对基准没有在第一阶段跑，按计划放在块 9。

## 各块的验收

每块合入前，都在与合入提交同树的候选上于 macOS 复跑受影响的测试，范围写在各块之下；其中的 Python 门禁是 CI 自己的 `tools/ci/source_validation.py --scope python`，跑测试的块还比对了测试前后的 IPC 命名空间。改动删除路径或守卫的块 1、3、4 另做了只读的独立审查，发现都先复现、修正后才合入。

- **块 1，重组存储的所有权与短锁（`9823cd05`）。** 重组 backing 有了固定的所有者，复制只持本条目的借用，不再持公共池锁或 shard 锁。审查查出两处：内存紧张时触发的 GC 会等一个正在写入的条目；已结算的旧凭证会新建一个 Pending 条目再被拒绝。两处都已修正。复跑：core 工作区 1701 项通过、5 项忽略，Python 门禁通过，命名空间不变；c3 与 Rust SDK 这一块没有改动，没有单独复跑。
- **块 2，契约 mode（`30cc1d63`）。** v2 契约接受流方法的 `"mode": "server_stream"`，拒绝其他取值；mode 只在流方法上进入两个指纹，现有 unary 契约的规范内容、指纹与生成产物逐字节不变，由黄金夹具守住；代码生成对流方法报明确错误。复跑在并排的 FastDB 上进行：c2-contract 48 项、c2-codegen 34 项通过，包括生成的 Rust SDK 与 TypeScript 目标检查；core 工作区与 Rust SDK 做了全目标的编译检查。
- **块 3，载荷缓冲预算（`56c82026`）。** 一项设置取代 retained-input 预算，覆盖调用输入、HTTP 缓冲与 relay 的请求体复制，有无截止时间的调用都计入；不足时在调用自己的截止时间内等待，长度未知的数据增长时不等待、立即失败。审查查出两处：调用输入一直计费到回复到达，等待准入时两个调用会互相等待；c2-ipc 收集 relay 分片的 inline 复制没有计费。两处都已修正：输入在传输复制之后释放，inline 复制先预约再分配。原来 relay-aware 客户端在路由过期后用保留的输入重发一次，这次随"不为可能的重放多留一份"删除；维护者 2026-11-04 确认只发一次业务 POST，理由记在设计 3.8 节与 ADR 0003。复跑：core 1720、c3 81、Rust SDK 25 项通过，Python 门禁通过，命名空间不变。
- **块 4，服务端调用记录与同步预约（`5b5abbba`）。** 接收循环在短的同步边界内预约路由，不再等 dispatcher 的异步锁；分片请求在首片处发布调用记录与准入凭证，按帧序预约；排序闸门 `chunk_ordering.rs` 删除；`max_total_chunks` 改名为 `max_inflight_chunk_frames`。审查查出一处竞态：分片复制出错时，帧许可在错误结算之前就已归还，后续合法分片可以补齐并取走唯一的回复权，已经检测出错误的请求仍会执行。修正后错误在 blocking 任务里、帧许可归还之前结算。把新增的屏障测试放到修正前的实现上运行，测试失败，输出 `callbacks=1, discarded=0, reply=Success`；修正后通过。复跑：core 1723 项通过、5 项忽略，c3 81、Rust SDK 26 项通过，Python 门禁通过，命名空间不变。

另有两个提交：`667142f3` 只修正块 3 留下的两处 rustfmt 格式（CI 不检查格式）；`9793d798` 更新参考文档、术语表与发布说明。

## 留给后续各块的

- relay 发回的上游响应还没有计入载荷缓冲预算，要等块 8 的逐片读取入口。relay 先取得上游连接的租约、再等预算，等待期间闲置回收收不走这条连接，也归块 8。
- 生成的 TypeScript 客户端在路由过期后仍会重发一次，FallbackDenied 也还会丢掉首个 IPC 失败的原因，归块 7。
- 资源进程在执行前拒绝路由准入（例如路由过期）时，错误经资源错误的路径报成 HTTP 500，调用方看不出它发生在派发之前，归块 5。
- `call_execution_snapshot()` 里的三个 retained 字节字段与 `memory_stats()['payload_buffer']` 重复，建议在块 6 只留一处，待维护者决定。
- 有限调用的名额在唤醒调用方之后才释放，名额用满时紧接着的下一次调用可能被误拒，归块 6。
- [闲置驱逐审查](2026-10-10-idle-eviction-regression-audit.md)的六道门禁在三种语言上补齐，归块 9。
