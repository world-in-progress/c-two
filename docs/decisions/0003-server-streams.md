# 服务端流

状态：已采用（2026-11-04），未实施。

第一期只做服务端流：一次输入，返回零到多个条目，用 `@cc.stream` 声明。资源写成普通的同步生成器，Rust 每次推进一步；每一步取得路由许可和执行线程，产出后就释放，等需求、预算或网络时不占它们，同路由的其他调用可以在两步之间执行。async unary、客户端流与双向流以后建在一个共享的可挂起资源桥上，不是本期的前提。Node 只做客户端。

线上格式（IPC 16）：unary 与流共用一套调用记录，记录种类为 OPEN、ITEM_BEGIN、ITEM_DATA、CREDIT、CANCEL、CALL_END 与 RELEASE。请求号就是流标识，在写端第一次提交时分配；每条连接记住已见请求号的高水位，迟到的续片据此识别后丢弃，不再依赖重组超时。unary 每个方向只发一帧（OPEN 带输入，CALL_END 带结果），不为流控多一次往返。额度由接收方授予：收完整值的一方在 ITEM_BEGIN 时按整条预约，只有边收边转发的一跳（relay 的上游 IPC）用有限的滚动窗口。交付第 k 条就允许生产第 k+1 条。C ABI 升到 8；SHM 4 与 relay mesh 7 不变。

契约保留 `c-two.contract.v2`：只在流方法上写 `"mode": "server_stream"`，缺省就是 unary，所以 unary 方法的规范内容、digest 与指纹都不变；旧解析器按未知字段拒绝流方法，不会误当 unary。

relay 不上 HTTP/2：请求体一次发完，响应体逐条流式发送，取消就是中止这次 HTTP 请求。背压靠各跳的有限缓冲，所以经 relay 时生成器可能比最终消费者多推进几步，上限以真实慢读测出。

预算：`payload_buffer_budget_bytes`（缺省 16 GiB）取代 retained-input 预算，不留别名，覆盖面扩到 HTTP 缓冲与流的本地副本。预算暂时不够时下一条等待，不报错；调用方自己的 hold 用满预算时也一样等。

取消与结束：丢弃流对象、`with` 提前退出或 Node 的 return 都发出 CANCEL，不等待；显式 `close(timeout=...)` 才等清理结束。取消不打断正在运行的推进：它照常结束并交付，之后不再推进，清理完以 `CallCancelled`（718）结束。Runtime shutdown 与路由注销都要等路由上的工作排空，所以对开着的流做同一件事。这改变了"shutdown 只停新调用、已派发的调用都跑完"对流的承诺，unary 不受影响。注销的部分由 shutdown 的决定推出，维护者同日确认。截止时间只结束调用方的等待，不取消执行；流缺省不设截止时间，同进程的流暂不接受有限截止时间。

顺序：同一条物理连接上，调用按接收循环预约路由的顺序开始；流的每一步重新排到路由队尾。Node 去掉整次串行的 callChain 之后，不等待就连发的 unary 会并发执行，依赖原行为的应用要用 await 表达顺序，发行说明写明。

考虑过而没有采用：

- 新增 `cancel_streams()`，由运维显式结束流：运维忘了调用，shutdown 就永远完成不了，最后只能杀进程，在途调用全成派发不确定。
- 每条流一个常驻线程：慢消费者会占满线程池。
- 先做 async unary：它不是服务端流的前提，也证明不了跨 await 不持路由许可。
- 所有 IPC 数据都走窗口：unary 每次多一个额度往返。
- HTTP/2 全双工：要换 HTTP 栈、加新依赖，换来的只是 relay 少多跑几步。
- 契约升到 v3：所有 unary 契约都要重新导出。

依据：[流式 RPC 设计](../design/2026-11-03-streaming-rpc-design.md)、[流式 RPC 调研](../research/2026-11-03-streaming-rpc-survey.md)、[ADR 0001](0001-single-owner-release.md)。
