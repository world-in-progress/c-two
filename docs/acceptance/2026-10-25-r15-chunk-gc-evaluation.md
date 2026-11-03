# R15：分片 GC 重复扫描评估与最小复用方案

状态：2026-10-25 用户已批准按本边界实施，接受小幅代码增量和压力日志频率变化。已完成实施及全部验收，实现为 `e1e6cda304393c279a3d3086c091c7512070e17f`，基线为 `2e9a40d25cbe4bf8c9b1bc34aa33120023b6723b`，即 C-Two 0.9.0 / c3 0.5.0；IPC 13、SHM 3、C/FFI ABI 6 保持。下文保留方案时的历史、基线实验与静态估算，实施结果另见[验收记录](2026-10-25-r15-chunk-gc-acceptance.md)。R35、两份控制会话重构、R07 失败释放策略与历史目录均不在范围内。

本项限定为“复用完整 GC 已获得的时间事实，跳过确定无对象过期的压力扫描”，按性能优化独立计账。原方案预计生产和测试合计净增约 70–120 行；实际生产净增 18 行、测试净增 108 行，合计 126 行。超出估算的部分用于复用现有真实锁、预算与 context Drop 验证进度，没有新框架。基线与候选的 180 组同环境样本已完成，完整 CI 首轮 10/10、实现及性能证据独立复核均通过；此处不把基线诊断对照记作候选收益。

## 实施阶段与出口

1. 先在未修改的生产基线补齐无 timer、预算与释放覆盖，保留新增优化判据在基线失败的证据。
2. 实施单一私有过期下界缓存，运行针对性回归；不扩展压力触发或重试语义。
3. 完成同环境 registry 和真实分片 RPC benchmark、Windows 开发机定向验证及确切候选的完整 GitHub Actions 门禁。
4. 独立审查、socket/lease 零残留与实际增量核算通过后提交，并清理构建产物。若 benchmark 未证明收益或出现回归，如实报告并处理，不用阈值关闭对照替代候选结果。

## 调用链与原设计理由

服务端首块先通过 chunk processing / admission gate，再取得 route pending permit，最后进入 `server.chunk_registry.insert(...)`。registry 达到 `soft_limit` 或其 `total_bytes >= max_reassembly_bytes` 时先做完整 GC，然后按 Duplicate、几何、预算、分配的既有顺序接纳。成功条目同时持有 backing 与 `ChunkRouteAdmission`；GC 仍须先释放 backing/charge，再在 shard 锁外释放 context/permit。`Server::run` 另有周期 `gc_sweep`，用于没有新请求时的过期回收；route 准入已满时，新请求可能在进入 registry 前就被拒绝，因此不能删掉这个 timer。

客户端回复首块走 `insert_reply → insert`。`IpcClient::spawn_maintenance` 定期调用 `try_gc_sweep`，跳过忙 shard 或无法释放的 backing，保留完整 owner 等下次维护；它不是插入失败后的补救步骤。插入失败会终结对应调用，不进行 GC 后自动重试。独立 Rust `ChunkRegistry` 不拥有 runtime 或 timer，调用者可显式执行 `gc_sweep`；原压力扫描提供另一条有后续 insert 时的回收途径。

原始设计 `729c7b3b`（[2026-04-04 设计](../reference/memory-policy.md)）明确 timer 归 transport，registry 保持 runtime 无关。`952b7014` 引入软阈值扫描：先尝试回收，再警告并继续接纳，阈值本身不拒绝。`461ffeab` 接入服务端周期清理；`f5c9aa33` 后真正的硬准入由共享 reassembly budget 承担，完成/trim 后的 backing 仍保持原容量计费；`15578b5c` 增加非阻塞维护；C03 `5015b39b` 将 route permit 与同代 assembly 合并。同步压力扫描至今有独立进度职责，原设计没有提供“扫描后近期不会过期”的复用事实。

## 已测重复成本与进度

使用真实 `ChunkRegistry`、`MemPool`、SHM 和预算，在 macOS 27.0.1 arm64、Apple M1 Max、Rust 1.91.0 release 构建下测量。每个规模取 9 组、每组 2048 次插入并撤销一个临时 assembly，交替先后次序；存量对象全部新鲜，持续保留。压力组使用原 `soft_limit=512`；对照只把该阈值提高到 `u32::MAX`，其余相同。未安装 tracing subscriber，因此数字不包含压力日志输出成本。完整数据与环境见[测量收据](evidence/2026-10-25-r15-gc-evaluation.json)。

| 在途 assembly 数 | 原压力配置：插入＋撤销中位数 | 提高阈值的诊断对照 | 单独完整扫描中位数 |
| ---: | ---: | ---: | ---: |
| 32 | 0.582 µs | 0.585 µs | 0.253 µs |
| 512 | 2.170 µs | 0.401 µs | 1.726 µs |
| 1024 | 4.464 µs | 0.442 µs | 3.979 µs |

512/1024 条时，每次压力插入仍扫描 16 个 shard，并检查全部存量条目的过期时间；一组 2048 次插入对应 2048 次完整扫描、32768 次 shard 加锁，均没有可回收对象。约 1.77/4.02 µs 的差额与独立扫描成本相符。32 条低于阈值，两组差异处于噪声内。这是基线工作量归因及收益空间，不是已实现加速；正常低于阈值的流量、真实分片 RPC 吞吐、Linux/Windows 成本均未在本轮测量。

以下实验没有 Tokio runtime 或后台 GC timer，使用 128 B 的真实 reassembly budget、两份 64 B backing，分别位于不同 shard；过期情景在原 20 ms timeout 后操作。显式 GC 及随后重试仅为实验对照，不是新增生产重试策略。

| 情景 | 基线事实 | 本方案的进度边界 |
| --- | --- | --- |
| 达到 count 或 registry 字节阈值，存量已过期 | 下一次 insert 同步回收两份旧 backing，成功接纳 64 B；预算由 128 B 变为 64 B，没有拒绝计数。 | 到达过期下界后仍执行原完整扫描，保持这条无 timer 的恢复路径，包括其他 shard 的过期对象。 |
| 达到阈值，但对象全部新鲜、预算已满 | 扫描无回收，仍返回 Capacity；两份原 owner 与 128 B 计费保留，拒绝计数增加一次。 | 只省去已证明无效的扫描，保留同一次预算拒绝及计数。 |
| 实际共享预算小于 registry 阈值，存量已过期 | 原条件未触发扫描，insert 仍被拒；显式 GC 后再次 insert 才成功。 | 保持既有边界，不声称自动解决所有预算不足。 |
| 一份结果已 take/finish 并继续持有，另一份 registry 条目已过期 | registry 只计 64 B，实际预算仍计 128 B；原 insert 不触发 GC，显式 GC 释放 registry 中那份后才可接纳。 | 已完成或其他 registry 持有的 backing 不能由本 registry 提前释放；实际预算与 registry 计数不能互相替代。 |
| route pending 或 chunk admission 在 insert 前满额 | 代码确认拒绝可能发生在 registry 之前；本轮未运行该服务端集成情景。 | 继续由原 timer、显式 GC 或正常完成归还许可；压力扫描优化不接管这些准入点。 |

没有新流量、没有 timer、也没有显式 GC 时，本项不会自行唤醒或回收。原压力条件只看已登记字节，不包含本次新增容量；临近预算上限的大请求也不保证触发扫描。若要让“未到原阈值的任意预算不足”也同步恢复，需要另行设计准入错误、计费和重试语义，不能隐藏在本次优化中。

## 最小候选

只修改 `core/protocol/c2-wire/src/chunk/registry.rs`。增加一份私有 `Mutex<Option<Instant>>`，保存完整扫描证明的最早可能过期时刻；复用现有 parking_lot，不新增依赖、线程、timer、配置或通用 GC 管理器。本机该字段类型大小为 24 B，其他平台以实际布局为准。

1. 保留原压力触发条件。条件成立后复制缓存值并立即释放缓存锁；只有检查时刻严格早于已证明的下界，才跳过本次自动扫描。没有证明、恰好到达下界或已超过下界时，仍调用原 `gc_sweep`。
2. 完整 `gc_sweep` 继续按统一的扫描开始时刻判过期、逐 shard 加锁、锁外析构。利用原遍历同时收集未过期条目的最早 `last_activity`，不加第二遍扫描。
3. 取 `min(扫描开始时刻, 最早存活 last_activity) + assembler_timeout` 为下界；空表使用扫描开始时刻。完整扫描及原析构结束后才发布。`checked_add` 无法表示时写入 None，表示没有可用于跳过的证明，继续原扫描行为。
4. 显式 `gc_sweep` 和服务端周期 GC 始终执行完整扫描。`try_gc_sweep` 保持现状，不用“本轮没释放对象”或 `remaining` 推导缓存，因为它可能跳过忙 shard/失败 backing。

证明依据是：成功 feed 只把时间向后刷新；已扫描 shard 后来加入的条目，其时间不早于扫描开始；删除条目不会产生更早的过期时间。不能使用扫描结束时刻，也不能按固定 cooldown 猜测过期点。每次完整扫描独立得到保守下界，并发扫描乱序发布最多使后续多扫，不会漏掉过期条目；不增加扫描 owner 或等待者队列，也不承诺把同时开始的多个扫描合并成一次。缓存锁不得跨 shard/pool 操作、实际扫描或 context Drop，防止引入锁序或回调重入死锁。

## 替换范围、净量与语义

| 范围 | 替换/保留内容 | 预计净行数，含必要注释 |
| --- | --- | ---: |
| registry 生产部分 | 原压力分支增加下界判断；完整扫描在原遍历中形成证明；一个私有字段与初始化。保留现有两种 GC、计数和所有权链。 | +20 至 +30 |
| registry 现有测试 | 将 `soft_limit_triggers_gc` 收敛成新鲜时跳过、到期后回收的真实路径；复用现有竞争、预算、context Drop 用例补并发与边界。 | +50 至 +90 |
| IPC/server 生产与协议 | 保留维护任务、计时配置、错误投影与全部调用入口。 | 0 |
| 生产＋测试合计 | 静态维护预算；尚无候选 diff，不据此报告实际减量。 | +70 至 +120 |

收益集中在达到原阈值、且完整 GC 已证明尚无过期对象的时间窗口：省去重复的 16 次 shard 加锁和全表年龄检查。低于阈值不增加缓存锁；每个 registry 增加一个字段，每次完整 GC 增加最早时间比较和一次短锁写入。高频显式 GC 调用者的这部分新增成本也须在候选对照中记录。不能把诊断对照中取消扫描的差额全部预先记作候选收益。

Rust/Python/TypeScript 公开签名与配置保持，`GcStats` 不增加字段，C/FFI ABI、IPC/SHM、业务帧及生命周期策略不变。保留 Duplicate→几何→Capacity 顺序、跨锁争用后的重新检查、预算拒绝计数、同代 teardown 和 backing/charge 先于 permit 的释放顺序。自动压力日志将从“每个压力 insert 一条”收敛为“实际执行压力扫描时一条”；这项日志频率变化须一并确认，不能继续在跳过时写“已扫描”。

## 回归与确认后的出口

先在基线补齐已有回收语义的覆盖，再加入能够区分是否发生无效扫描的候选对照。复用现有真实 shard/pool 锁与同步工具：让一个无关 shard 保持锁定，证明新鲜压力插入可完成；解除锁后仍完整收尾，不能用源代码词串或单纯检查缓存字段代替工作量证明。新增覆盖必须包含扫描中插入/刷新、慢扫描跨过下界、并发扫描乱序发布、context Drop 重入、零 timeout、相等边界、时间加法不可表示以及部分 `try_gc_sweep` 不发布完整证明。

现有 `typed_gc_and_duplicate_are_rechecked_after_pool_contention`、`cleanup_releases_backing_before_context_outside_locks`、`try_gc_file_busy_shard_and_backing_are_retryable`、`finished_and_trimmed_backing_stays_charged`、`shared_budget_bounds_independent_registries` 的断言继续保留；无 timer 及预算情景合入相应既有用例，不新建测试框架。涉及时间的控制必须尊重生产 `last_activity` 只前移的不变量，不能在缓存建立后任意倒拨时间，再把这种非法内部状态当作生产反例。

批准后先运行上述针对性回归，完成同环境基线/候选交替测量，同时记录低压力、512/1024 新鲜条目、单线程/多线程压力和真实分片调用的变化，验证新短锁不会成为共享热点；再完成完整 relay、跨语言、Linux/macOS/Windows x64/ARM64 的适用 GitHub Actions 门禁，Windows 定向验证优先开发机。只处理本批端点并验收零残留，完成独立审查后提交。若出现进度、错误顺序或释放语义偏差，或实际维护增量超出上述预算，应回到方案确认，不能靠扩充旁路或放宽断言完成。

本轮独立静态审查为 hey-my-buddy run `aff52710-0be9-4e53-9472-620e643cf39b`、产物 `1321c8d8-1a03-41ee-b8ca-7f48fcfc58e2`，已由 Host 核对并验收空补丁。审查支持下界证明的条件，要求保留上述预算与部分扫描边界；它没有执行性能或回归测试。本轮基线实验创建 59 个独立 pool，77 个 backing 名在原生 Drop 后重开均失败，预算归零；未创建 IPC socket/lease，未触碰历史目录。已清理 96.5 MiB 构建产物及审查工作树，仅保留源码探针、日志和收据。初次临时探针编译的统计字段拼写错误已在执行前修正并保留日志，不涉及生产修改。
