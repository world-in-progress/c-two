# R13：IPC inline 响应直接移交已有字节所有权

状态：2026-10-24 已完成实施与验收，实现为 `1ac341506acbafcd28382deae291359abbf03aa9`，固定基线为 `3e575acbb4c30414f06caaeb7e6e8c84e503ff3c`；实际增量、性能及完整门禁见[验收记录](2026-10-24-r13-inline-response-ownership-acceptance.md)。本项针对每次 IPC inline 响应的重复分配和复制，属于运行时成本优化；生产与测试净增，不能记为代码净减。R35 已暂缓，公开心跳 API 删除不混入；R06/R07、两份控制会话重构及其他响应载体的所有权策略也不扩展。

## 实际调用链与历史理由

基线中的普通 Python 远程调用也通过 `PyNativePreparedCall::finish` 调用 Core `EncodedCall::call_held`，并非只有 `cc.hold` 才经过这条路径。IPC 接收端已将成功 inline payload 放入独占 `ResponseData::Inline(Vec<u8>)`；私有 `call_ipc` 随后通过 `SyncClient::lease_response` 将它包进 `ResponseLease`。Core `HeldResponse::from_response_lease` 再调用 `lease.copy_bytes()`，inline 分支执行 `Vec::clone()`，并把原 Vec 保留在 lease 中。一个非空 payload 此时有两个副本，直到 transport lease 释放。

可直接复用的能力已经存在：`ResponseData` 能携带完整传输所有权；Rust owned 调用使用 `ResponseLease::into_owned_bytes` 移动 inline Vec；HTTP held 调用使用 `HeldResponse::from_owned_bytes`。后者保留相同的失效、释放状态和 bytes 观察方式，Core 随后照常附加连接保活，不需要新的响应类型、通用 owner 框架或公开接口。

`b042d84b` 引入集中复制与 transport lease 顺序，`38a1b4e3` 接入统一 Core client/host；当时建立 lease 还需要补入 server/reassembly pool，统一经过 lease 有实际职责。`6e6519a8` 引入 Python Core 响应投影，`5ed04149` 明确 Python 快照及逃逸 writer 所有权边界。随后 `f5c9aa33`、`047cd58b` 把重组预算及 SHM pool 身份分别放入载体；当前 `SyncClient::lease_response` 只执行 `ResponseLease::new(response)`。因此现在可以在 Core 既有构造边界区分已拥有的 inline bytes 与需要保留 lease 的 SHM/Handle。

这里没有发现两次实际 free：`ResponseLease::release` 先 take，Drop 再调用为空操作；不能用此作为删除 Drop、失效状态或其他载体释放流程的理由。服务端 `materialize_python_bytes` 的初始复制仍建立 Python 可变 buffer / 只读别名快照，write_into 分支已经移动唯一 Vec，也不在本项删除范围。

## 最小生产与测试范围

生产仅修改 `core/runtime/c2-core/src/client.rs`：私有 `call_ipc` 直接返回已有 `ResponseData`；owned 路径使用已有 `ResponseData::into_bytes()`；held 路径在一个具体的私有构造方法中把 `Inline(bytes)` 移入已有 `from_owned_bytes(bytes)`，其余变体立即建立 `ResponseLease` 并进入现有 `from_response_lease`。保留最后的 `with_connection(inner)`。这个方法只表达三种既有载体的构造选择，没有新状态、策略开关、缓存或可注入适配层。

| 范围 | 预计物理行变化，含注释与空行 | 收益与维护成本 |
| --- | ---: | --- |
| `client.rs` 生产部分 | +7 至 +15 | 加一个直接的载体构造分支，复用 owned 与 lease 两条既有实现；退出 inline 重复快照及其多余 lease 持有。 |
| 同文件私有构造单测，必要时调整既有 lifetime 用例 | +25 至 +45 | 非空 Vec 地址连续性、内容、空响应、失效回调及幂等释放；复用现有 SHM/Handle、deadline 与保活测试，不复制整套夹具。 |
| 生产与测试合计 | 预计净增 +32 至 +60 | 行数不构成瘦身收益；每个非空 inline 响应省一次 payload 分配、一次 N 字节复制及持有期间的一份重复数据。 |

上表是批准方案的预算，实际 numstat 与同环境测量随验收记录报告。新增维护面控制在一个具体构造方法及直接行为断言，没有为地址/capacity 测试增添公开 getter、计数器或框架。普通 Python IPC 和 Rust held inline 调用受益；Rust owned inline 已有移动路径，HTTP 与 SHM/Handle 不宣称获得此次收益。延迟与吞吐须区分构造微基准和真实调用；不能把少一个副本换算成等额 RSS 降幅。

## API、协议与回归风险

公开 Rust/Python/TypeScript API 均不变，不删除 `HeldResponse::from_response_lease` 或 `SyncClient::lease_response`；它们仍保留显式租约构造语义。C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧、CRM 哈希及生成器不变，也不引入两套协议或兼容入口。

inline 已经是接收任务交出的唯一 Rust Vec，直接移动不借用网络缓冲、Python 对象或 FastDB owner，不改变请求/响应跨语言快照边界。PyCoreResponse 的 data_len、buffer 导出检查、retention 记录和失效状态继续使用现有实现。SHM/Handle 继续保留原池身份、代际、精确释放和完整容量计费，不能一律调用 into_owned_bytes 而提前归还其存储。

最重要的风险是 raw `ResponseData` 的短暂所有权窗口：SHM 载体需要及时进入 lease，收到响应与建立 lease 之间不能插入 await、用户回调或可失败处理。私有分类必须在同一次 poll 中完成；错误路径仍沿用原归一化，之后交给原调用执行事务管理取消和迟到结果。成功仍保留 owner 失效 → transport 释放 → connection 释放的顺序；存储释放 → budget 退款、连接关闭后的 held 使用以及失效/释放双错均保持。

## 确认后的验证顺序

先在基线补充内容、空响应和释放行为覆盖，并在隔离诊断中记录非空 inline Vec 进入 held 前后的地址，证明现有分支发生复制；候选对实际使用的私有构造方法断言地址保持。使用现有 bytes 观察，不增加生产观测接口。基线对照只证明本项重复复制，不能把它说成现有返回数据错误或 SHM 生命周期缺陷。

在同一主机、同一配置下比较小型 inline payload 的构造成本和真实 IPC 调用；报告 payload 尺寸、循环次数、构建模式、时间及分配/副本证据。普通调用与 held 调用分别记录，零长度单独处理；结果不稳定时如实报告，不能将微基准倍率当成端到端加速。实现若超出上述范围或引入第二套释放状态，停止扩展并重新确认。

随后运行 Core client modes / lifetime ordering / 四类载体的 deadline 与迟到响应、Python 快照与逃逸 writer、真实 FastDB held/borrowed、完整 relay 及 Rust/Python/Node 跨语言回归。Windows 优先开发机 Named Pipe，再以确切候选完成现行 GitHub Actions 三系统验证，包含 Windows ARM64 及适用 3.14t。只验收本批自建端点的 socket/lease/Named Pipe 零残留，及时清理构建产物；历史目录不动。通过独立实现审查后再提交，按实际结果报告代码增量与运行时收益。

## 筛选证据

Host 已核对上述 Core/PyO3/IPC 调用链、原始提交、两种既有构造器、接收端 inline Vec 来源、retention 投影及 native 迟到结果处置。hey-my-buddy 独立只读分析 run `ada7448b-61bb-422f-87c3-c3537bf82963`、最终 artifact `2db9e0dc-5eb5-4bbd-8b66-ca7b9aba2dd7` 得到同一候选，确认服务端没有可以直接删除的重复物化事务；交付补丁为空。这是方案筛选证据，实施候选仍需独立审查。

基线新增内容、空响应及幂等释放覆盖通过，现有 lifetime ordering 5 项通过；把非空 Vec 地址连续性改为必需断言后，旧生产路径稳定失败于 `inline bytes must move`。候选沿用同一断言并通过。随后完成针对性回归、完整 relay、Rust/Python/Node 跨语言、三系统、同环境性能、端点零残留及独立实现审查；GitHub Actions 的同一候选 10/10 通过。生产实际净增 7 行、测试净增 30 行；没有测得稳定的端到端提速，具体样本、首轮失败及验证修正保留在验收记录中。
