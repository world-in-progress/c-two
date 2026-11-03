# R06：Python buffer 导出与释放的最小同步修复

状态：最小同步修复、基线反例、同环境性能对照、完整 relay/跨语言/三系统工程验证、端点验收和独立审查已完成，已随 0.9.0 通过正式发行门禁并发布。固定基线为 `ea8345fafcb146b118afbf02148bb48f77f6e238`，R06 候选为 `627156265addbedaff80a7f17d62a9160b952d63`，后续 Windows 发布候选保留该生产实现。详见[验收记录](2026-10-25-090-windows-release-acceptance.md)。R06 单列为正确性修复；R35、R07 与两份控制会话重构继续暂缓。

## 为什么选择这一项

R09 的四次配置加载真实存在，但复用现有 `ResolvedRelayClientConfig` 和私有 resolver 后，生产仍约持平至增 5 行，必要测试约增 25–50 行；没有性能实测。直接聚合还会提前获取运行时 override，把 state lock/deadline 观察放到原前三项环境解析之前，因此不能声称只改变 `.env` 快照。R39 的操作专属成功结果已完成；剩余必需 proof 约减 6 行、HTTP body 复用约减 7–10 行，不单独开批。C04 的轮询承担锁外状态通知补偿和外部最后一个 Arc 消失的观察；R15 的同步 GC 仍负责无 timer 用法的准入恢复。本轮没有发现值得独立实施的新增纯瘦身候选。

R06 仍有可由源码建立的竞态，且可复用现有锁关闭窗口，不需要新状态类型、通用 buffer 框架或公开能力退役。它解决实际的释放/统计一致性问题；新增测试是正确性成本，不用其他删除抵消。

## 架构、调用链与历史依据

响应由 `EncodedCall::call_held()` 交给 `PyCoreResponse`，普通结果和 `cc.hold()` 均消费真实 `ResponseBuffer`。Python 持有结果释放时先解除自己的 view，再调用 `invalidate_then_release`；Core 的 `HeldResponse` 继续拥有失效、transport lease 释放和幂等语义。请求由 `CoreCallback` 获取快照，`PyCoreService` 将同一 Vec 移入 `PyCoreRequestBuffer`；服务端 Python dispatcher 创建 memoryview，borrowed 分支登记统计 lease。Core 仍在 service 返回后释放请求 transport lease。线程内 direct-call 不经过这两个字节 exporter。

源码位置为 `sdk/python/native/src/core_ffi.rs:261–535`、`core/runtime/c2-core/src/lifetime.rs:13–98`、`core/runtime/c2-core/src/host.rs:1095–1131`、`sdk/python/src/c_two/crm/transferable.py:226–288,634–665` 和 `sdk/python/src/c_two/transport/server/native.py:518–562`。扩展声明 `gil_used=false`，必须覆盖自由线程执行。

`6e6519a8` 把 Python 传输投影迁到 Core。其父提交 `client_ffi.rs:165–179` 已明确执行锁外快速拒绝、锁内重验以关闭 TOCTOU 窗口；新 `PyCoreResponse` 保留了锁外检查，却漏掉锁内重验。exports 本来就用于阻止活跃 memoryview 期间的失效和释放，保护本身有职责。`e21e79c4` 后续只把请求快照改为所有权移交；本批保留这条路径。

当前响应 `release` 和 `invalidate_then_release` 在获取 `inner` 前检查 exports；`__getbuffer__` 在 `inner` 内递增 exports。允许的交错是释放看到 0、新导出完成、释放随后继续。请求的 released、exports 和统计 lease 也分别同步，允许导出与释放同时成功；`track_retained` 还可能在 release 完成后装入新 guard，使统计重新出现 active hold。响应 tracking 已由 inner 锁保护，不存在后一种相同窗口。

静态交错现已由真实对象的受控反例确认：三个释放入口错误返回成功，迟到 tracking 在 release 后重新留下一个 active hold。基线反例提交为 `00f91575583194f2b084e171a110891b697dfe2a`，剥离 cfg(test) 钩子后生产代码与原基线逐字节相同。现有两类 exporter 都保留 Vec，导出持有 Python 对象强引用；证据支持释放围栏和统计状态缺口，不支持宣称 UAF。

## 最小替换范围

| 范围 | 改动 |
| --- | --- |
| `core_ffi.rs` 的 `PyCoreResponse` | 两个释放入口先获取现有 inner 锁，再执行现有 exports 检查；检查仅在共同接纳边界执行一次。 |
| `core_ffi.rs` 的 `PyCoreRequestBuffer` | 复用现有 lease Mutex，使 release 的检查/标记/取走 guard、getbuffer 的检查/导出、track_retained 的检查/登记各在同一锁内完成。 |
| 原子计数和只读路径 | 保留 exports 原子递减以及 request 的原子 released 观察；releasebuffer 不新增锁，Vec 不提前 take 或销毁。 |
| 现有 `payload_tests` | 使用真实 exporter、memoryview/PyBuffer 和 BufferLeaseTracker，补受控竞态及失败路径；保留地址、快照、逃逸 view 和释放后归零断言。 |
| `test_core_lifetime_parity.py` | 将只调用假 `CoreOrderedResponse` 的失败用例替换为真实 native 失败/幂等覆盖；保留 Python 层仍独有的顺序断言。 |

生产代码只改一个文件。不抽取共同释放函数，因为普通 release 遇到 transport 错误的提前返回，与 invalidate_then_release 保留 Python 主异常并清理统计 guard 的现有处理不同。响应外层 Option、重复长度及两个局部状态类型的整理不纳入本批；它们不是修复成立的前提。

| 静态预算，含空行 | 预计净变化 |
| --- | ---: |
| 生产 | +3 至 +10 行 |
| 测试，含仅 cfg(test) 的实例同步钩子及替换假对象用例 | +170 至 +260 行 |
| 生产与测试合计 | +173 至 +270 行 |

以上是设计时预算，不是最终候选的实际增量；实际实现与证据见验收记录。请求导出新增对既有 mutex 的一次短暂获取；没有新增 payload 复制、线程或 timer，延迟与争用开销仍需同环境测量。若实施出现超出该边界的状态机或明显超预算，先回到方案，不捆绑其他删除凑净量。

## API、ABI、协议与回归风险

Python/Rust 公开签名、导出、C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧和 CRM 哈希均不变。并发语义的修正明确为：活跃导出先完成时，释放返回现有 BufferError 且不失效 owner、不撤统计 lease；释放先完成时，新导出被拒，迟到 tracking 直接返回。顺序调用的错误类型/文案、释放后 len/bytes/导出行为、重复 release、重复 tracking 的 guard 替换均保留。

主要风险是锁范围、GIL 等待及错误路径被意外改写。响应仍沿用已有的失效回调与 Core 释放顺序；R07 的失败释放 owner/charge 策略不纳入。当前任意 Python invalidator 在非重入 inner 锁内运行，回调重入同一 response 存在既有自锁风险；本批不宣称解决该问题，也不以取出 held 后解锁的方式另造“正在释放”状态机。该风险保留为独立后续评估。

## 确认后的回归与验收

先在固定基线上增加实例专属、仅 cfg(test) 的一次性握手点，以有界 channel 驱动真实方法。握手点保持在锁前，等待在 GIL 外完成；不得在候选 owner 锁内等待另一线程完成导出。分别固定两个响应释放入口的检查/导出交错、请求释放/导出交错、请求 tracking/释放交错，记录基线反例，再迁移检查位置。禁止靠 sleep、概率重跑、直接篡改 exports/released 或仿制状态模型证明修复。

候选需证明拒绝释放时 invalidator 为 0 次、view 内容及统计保持；解除所有导出后恰好释放一次；迟到 tracking 不恢复 active hold；真实 invalidator 异常对象、附加错误及幂等行为保留。针对性运行 native payload、held/borrowed、统计及快照测试，再完成完整 relay、真实 Rust/Python/Node 跨语言、macOS/Linux/Windows 和确切候选 GitHub Actions，包含 Windows ARM64 与 3.14t。Windows 优先开发机。本批 socket/lease/Named Pipe 零残留、独立实现审查及构建产物及时清理均是出口条件；历史目录不动。

本轮独立只读复核使用 hey-my-buddy：R09 run `42899170-fb69-4925-aa32-770b535ca16b`、R39 run `78d4b6b1-d07b-422c-89a6-51b78e552fe6`、R06 run `801c0643-5fa6-46af-a208-10038864e524`，输入均为固定源码。Host 已核对关键源码和历史，修正了 R09 报告中“错误顺序不变”的判断，选择 R06 复用现有锁的最小方案；局部状态重构不采纳。这些报告没有运行验收结论。
