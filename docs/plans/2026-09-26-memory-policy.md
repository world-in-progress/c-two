# C-Two 0.6.0 内存侧优化建议

日期：2026-09-26。分析基线：发布源码 `d71fa93a1bf15cd4092b9e18700f676c5aff79c8`，当前检出 `8777e2dd157942fe6f3cc90389763af35f5056bd`。Host 核对这两个提交的 Core、Python SDK/native 与 Rust SDK 运行时代码无差异。正式 Python 包为 `c-two==0.6.0`，依赖 `fastdb4py==0.2.1`；c3 为 0.2.0。

发布信息：https://github.com/world-in-progress/c-two/releases/tag/c3-v0.2.0
PyPI：https://pypi.org/project/c-two/0.6.0/

## 建议目标

保留一套自动回退体系，让同一份 Rust 内存策略约束所有请求、响应、重组与提升路径。按需初始化，按预算扩容；轻负载场景可跳过 buddy，但继续使用 inline、小块传输、dedicated SHM 和文件回退。SDK 只投影配置，不复制策略。

需要分清两个连续决策：发送端先选 inline / 共享内存引用 / 分块传输；接收端对分块消息选择 buddy / dedicated / 文件 mmap 作为重组存储。现有分块 API 在完整消息收齐后调用资源，因此仍需要完整消息的存储空间。文件回退可以降低匿名内存压力，但仍消耗地址空间、页缓存、磁盘和 I/O；并非无限、零内存的兜底。真正的增量消费 API 可以以后单独设计。

## 当前证据

1. **buddy 禁用策略没有覆盖所有入口。** `core/transport/c2-ipc/src/pool.rs:47` 无条件创建并传入 `Some(pool)`；`client.rs:758` 的普通构造检查了开关，但 `client.rs:841` 的外部池路径直接接收池。`client.rs:855` 连接前 `ensure_ready()`，`core/transport/c2-server/src/server.rs:318` 创建响应池并 `ensure_ready()`，均会预热 buddy。客户端和服务端重组池的构造也没有该策略。默认主池段大小 256 MiB，见 `core/foundation/c2-config/src/ipc.rs:114`；这是容量配置，不是 RSS 测量。

2. **独立 wheel 探针重现了上述问题。** 从 PyPI 安装 0.6.0 到 uv 隔离环境，双方都设 `pool_enabled=False`、段大小 2 MiB、`shm_threshold=16 MiB`，经透明本地 socket 转发器捕获原生握手，再执行 1 字节 echo。两端各宣布一个 2 MiB buddy 数据段；通过只读 `shm_open` 和 `fstat` 确认两个 backing 实际存在，每个为 2,113,536 字节。没有以描述对象或映射容量推断 RSS。

3. **关闭池还关联了分块能力声明。** `core/transport/c2-ipc/src/client.rs:926` 在有池时声明 `CAP_CHUNKED`，无池时没有该标记。修复不能只把 `Some(pool)` 改成 `None`；分块能力应独立于 buddy 启用情况。握手当前从已存在的首段推导段大小，见 `client.rs:884` 与 `core/transport/c2-server/src/connection.rs:188`；惰性创建时应验证空段列表、两端不同配置、迟建段与 generation 安全，不假定删掉预热就足够。

4. **总重组预算目前是软限制。** `core/protocol/c2-wire/src/chunk/registry.rs:108` 达到总字节/数量限制后 GC 和告警，仍继续分配。`assembler.rs:62` 按 `total_chunks * chunk_size` 申请完整 backing。单消息检查存在，但不等价于并发总量上限。服务端已有 chunk 处理许可及 route admission；全局 pending request 许可在完成重组后才申请，见 `core/transport/c2-server/src/server.rs:2783`、`:2879`。不能声称目前完全没有背压，也不能把现有数量上限当作字节预算。

5. **文件提升会增加峰值分配。** `core/protocol/c2-wire/src/chunk/registry.rs:228` 对完成的文件重组自动尝试提升；`chunk/promote.rs:22` 在文件映射和 SHM 目标之外，再创建整包 `to_vec()`。`core/foundation/c2-mem/src/pool.rs:894` 的 `try_alloc_shm` 不做 RAM 检查。这既引入可消除的整包临时缓冲，也可能立刻推翻刚才的低内存回退选择。

6. **空闲池和压力决策仍有改进空间。** `core/foundation/c2-mem/src/pool.rs:278` 只回收尾部空闲段，始终保留至少一段。`alloc()`、`alloc_handle()`、`try_alloc_shm()` 的压力规则不同；`alloc_handle()` 检查的是请求大小，扩容却可能创建整个大段。该路径中创建 buddy 失败会直接转文件，不能把当前实现描述为每处都严格经过 dedicated。`spill.rs:110` 的系统可用内存快照只是启发式，不能替代对并发分配的原子预算预留。

## 三步落地

### 第一步：统一策略、惰性创建，消除已知额外分配

明确 buddy 的允许状态及预热策略，所有池角色投影同一套配置。禁用 buddy 仅跳过其复用与扩容；dedicated、分块和文件回退按各自能力继续可用。保留请求/响应方向隔离、唯一 pool incarnation、generation 检查和原有租约释放权威。

默认按需创建首段；高吞吐应用可显式预热。轻量配置无需为了一个小消息创建 dedicated：小消息继续走 inline，超过阈值才尝试允许的共享层。不要先引入多套互斥的传输实现，也不要把服务发现/CRM 信息塞进 allocator。

文件 backing 可直接交给已有 `RequestData::Handle` / `ResponseData::Handle` 路径消费，取消无条件提升。确有复用收益时再按策略、压力恢复水位和预算提升；提升的复制使用安全的非重叠源/目标视图或固定大小 scratch，去掉整包 `Vec`。

可配置空闲温存段数，轻量场景允许归零。先支持尾部及最后一个段安全退休；中间孔洞复用属于后续优化，必须保持线上的 slot/generation 语义。

### 第二步：在分配之前落实字节预算

区分 backing 已分配容量、活跃 payload/租约字节、在途重组预算、文件预算。buddy 扩容按实际段成本预留，dedicated 按实际 backing 成本预留；复用已有 buddy 不重复收取整段成本。预算以可释放的 guard 随真实 owner/lease 转移，持续覆盖重组完成后排队、执行、响应和 held 状态，避免 finish 后预算先归还、内存仍然存活。

总 payload/backing/disk 预算耗尽时采取有界等待或明确容量错误；匿名/SHM 预算耗尽而文件预算可用时允许文件回退。控制帧仍应能推进取消、释放和 shutdown，避免大消息占满数据预算后无法回收。系统压力感知作辅助，按实际拟新增容量判断，采用缓存和恢复水位避免反复 spill/promote。

### 第三步：以测量决定后续性能工作

统一报告池容量、实际在用、温存、dedicated、文件、重组、held 字节与 fallback 原因，并单独采集 OS RSS/commit。现有 `PoolStats.fragmentation_ratio` 的计算是 `1 - free/total`，更接近使用率；不要把它当成外部碎片率。真正碎片指标需要结合最大可分配块和请求分布。

比较四类负载：空闲/小 RPC、多进程 Buddy 突发、科学计算大 payload、强制内存/磁盘失败。记录首次/稳态延迟、吞吐、复制量和峰值 backing/RSS。FastDB 在资源执行时直接构建最终传输 backing 是另一个可评估方向，必须保持 FastDB 中立且有独立实验证明；本轮不把 prepared sink、`write_into` 或普通 hold 宣称为直接构建/零复制。

## 验收边界

- 禁用 buddy 后，从注册到收发、重组和可选提升均不创建 buddy 段；dedicated、inline、分块、文件仍各自可达。
- 空池握手、两端不同段大小、扩容、回收归零、旧 generation 拒绝，覆盖请求和响应。
- 精确故障注入依次覆盖 buddy 满/建段失败、dedicated 失败、文件失败、总预算耗尽；断言选中层和原因，避免靠真实机器 OOM 测试。
- 并发预算不能超额；超时、取消、断连、校验失败和 shutdown 都归还 guard；held 释放先失效 FastDB checked owner，再释放 backing。
- 提升不产生 payload 等大的临时堆缓冲；压力未恢复时不提升；失败保留原文件 backing 和字节内容。
- Windows 2022/2025 和 Unix 检验原生 mapping/临时文件清理，保持现有跨语言门禁。文件提升的复制减少是源码可证明的方向，吞吐/RSS 改善需 benchmark，当前没有给出改善百分比。

## 探针产物与未定事项

- `/tmp/c2-memory-060-wire-probe.py` 与 `.log`：已发布 wheel 的配置失效重现。
- `/tmp/c2-memory-060-wire-drop-probe.py` 与 `.log`：补充 proxy drop/GC 检查。
- 两个探针中服务端 backing 在 shutdown 后消失，客户端 backing 在探针退出后仍可打开；这些探针使用了透明 socket 转发器，尚未做无转发器对照及清理根因定位，列为待核实生命周期现象，不外推为所有正式客户端必现泄漏。
- Host 仅删除了两个已退出探针自身产生的剩余命名映射，清理回执为 `/tmp/c2-memory-060-wire-cleanup.json`、`/tmp/c2-memory-060-wire-drop-cleanup.json`。
- 本次未修改 C-Two 生产代码或运行完整工程测试。独立 Buddy 审查已完成，Host 的复核与修正记录见下。



## 实施跟踪

用户已批准按上述方案实现。集成分支为 `socu/memory-policy`；实现提交与分析基线分别记录。

- 待验收：统一 buddy 策略、惰性创建、明确预热和空闲温存配置。
- 待验收：文件重组保留 backing，移除自动提升和整包临时复制。
- 待验收：明确预算的所有者和共享范围，预留、转移及释放贯穿真实分配生命周期。
- 待验收：预算、各层回退和清理的负向测试，以及低负载/大消息基准。
- 待验收：集成的 Unix、本机 Python 与 Windows CI 验证。

不以单个 worker 完成或局部测试通过代表整体交付；各项状态由集成后的证据更新。
