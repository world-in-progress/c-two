# C-Two 内存改动审查与验证

本报告记录用户要求的旧测试追溯、运行时独立审查、问题修复与并行验证。PR 仍待用户明确授权。

本机测试源码为 `15578b5c8be1c010012ee44aee6bb776c435ec35`，旧基线为 `8777e2dd157942fe6f3cc90389763af35f5056bd`；FastDB 固定 `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。Windows 验证源码为 `2d360875b09a65324879cd489dc4096ea8657178`，相对本机提交只增加独立验证分支的 workflow 触发条件。实现文件逐字相同。

## 旧测试与改写追溯

最初比较固定在旧基线与 `1c78cd6e5fc49cd97126ec6f5707757cdaf99d6a`：Rust/Python/Node 源码测试定义共 2026 → 2274，1955 个正文保留，55 个修改、9 个改名、7 个删除、255 个新增。这个数量不是参数化后的执行项数。71 个旧可执行定义的修改/改名/删除及相关 helper 已逐项审查，见 [追溯账本](2026-10-03-memory-test-review-ledger.json)。完整 2.9 MB 提取清单保留在 Buddy 固定产物 `3090d647-d384-47df-ae2c-bd7e3ff72662` / 提交 `1495f87790df67f993d3e4bd973d8eef95e38269`，账本记录其 SHA-256；此处只提交审查所需的精简记录。

7 个删除涉及旧 spill 启发式、自动 file→SHM promotion、take_handle 和全局客户端 singleton。它们随获准的机制替换而删除，不宣称逐项语义完全等价。其余改写保留或强化旧行为保护。发现的 abort 用例弱化已恢复：保留同一池、确认 allocation count 归零，再禁止扩容/dedicated/file fallback 申请整块 64 KiB。4 个反向 mutation 分别撤销 owner 校验或只退款不 free，均准确触发断言失败；mutation 已恢复且未导入实现。

旧基线实际运行：Python 867 项零跳过，Core 961 项、Rust SDK 10 项、CLI 45 项及示例通过；Node 33 项、类型和打包检查通过。旧 repo 工具套件最初只失败于两项缺失的官方 FastDB 样本；恢复 public manifest pin 正确的 0.2.1 样本后，两项在新旧树均通过。保留原失败日志，没有用 skip 代替。

最终候选的 Python 918 个 JUnit/phase nodeid 中，旧 867 个全部保留，无遗漏、重复或跳过。源码定义清单本身不能证明运行行为，以上执行证据与逐项断言审查分别保留。

## 查出的问题与修复

| 问题 | 反例与最终保护 |
| --- | --- |
| 部分 buddy 帧后已关闭 proxy 仍占 69632 B | 只向 peer 写 12 字节、资源执行 0 次。关闭后 detach transport-owned slot；真实 RequestBlock/retire/carrier 继续持 pool。原探针在保留旧 proxy 时预算归零；同预算重新 acquire 成功。 |
| 响应容量不足变为 ClientCallResource | 真实服务执行一次后客户端拒绝重组。现在是 canonical 702，同时保留 DispatchUncertain、禁止自动重放、ping/其他 RID 可用与 guard 归还。 |
| 非法 shutdown timeout panic 并移出 Host | inf/NaN/负值/不可表示值在任何状态改变前得到 ValueError；旧 Host 可继续调用，正常 hook 恰好一次。 |
| 可变 pool 容器被替换后读/释放新 owner | carrier 捕获 incarnation；读写及阻塞/非阻塞释放在同一 guard 下校验 owner/authority，失败保留 state/charge。buddy/dedicated 坐标碰撞、peer 替换及恢复 owner 都有真实回归。 |
| 新 admission 分片锁与 carrier callback 形成 ABBA | 旧候选的确定性 watchdog 子进程失败。现在 shard 内只 try_write，竞争时在 shard 外等待，再重做实际原子决定；duplicate/GC/full-budget 分类保留。 |
| receiver 取消的同步 Drop 突破关闭时限 | 50 ms 关闭在旧候选耗时约 1.01 s；新候选约 1.45 ms 返回 false，保留完整 owner/charge，解锁后 retry true。真实 LocalStream 与 dedicated footer/read_done 对照通过。 |
| 非法单块 128/1/129 误判容量不足 | 在任何 reservation 前得到 Protocol，合法容量不足仍为 702；不会因预算状态改变分类。 |

合并时保留了同一 allocation guard 内的 incarnation 捕获、同一 release guard 内的 authority 校验，以及严格 abort 原池复用断言。两个修复任务均按最新 sealed artifact 验收；[最终独立关闭交接审查](2026-10-03-final-close-flow-review.md) 对固定 `15578b5` 未发现新增阻塞 finding。该审查是静态证据，与下面动态结果分开。

## 最终本机结果

固定源码上完整 Core all-features **1211 项**、Rust SDK **10 项**、native 参数测试 **2 项**、两个真实 Rust 示例通过；受影响 Python 定向 **53 项**、完整 Python **918 项**零失败/跳过；严格 Rust/Python **18 行**和 TypeScript **12 行**收据通过，并绑定同一实际 c3 字节。三个原始故障探针均转绿。

完整 repo 工具先前 **391 项**通过；并行资源租约 follow-up 的全部 **27 项**也通过（新增不同 TMPDIR 的真实进程竞争回归）。当前 Node **34 项**、旧 Node **33 项**及双方类型/打包检查通过；这些未受最后 wire/IPC 清理改动影响的检查没有重复执行。CLI **45 项**先前通过，最终 c3 已从固定源码重建并用于两套跨语言实调用收据。

本机 native SHA-256：`0d2a027ff2eae46b118ef0a33ba11ea4d62a5e26b7ea9c6e50e1ded954e79fab`；c3 SHA-256：`8506b8a2afb413d72376d5bc31eee81284fc1410e80f9088167de08692b90127`。命令、完整日志、退出码、JUnit/phase reports、plan、源码和构建来源保留在 `/tmp/c2-full-review-1002/final-local-ready/`。

## 并行执行

[本机入口](../../tools/dev/test_python.py) 将普通文件按 loadfile 分发给两个 worker，地址含 run/worker/PID；选端口至 relay readiness 使用跨进程锁。完整 portable 与 TypeScript 模块保持串行，动态收集 Counter 要求分组并集等于全套，每项执行一次，有缺输入、skip、失败或未确认清理都失败退出。资源锁使用稳定的用户缓存目录，避免不同 TMPDIR 绕过同一资源的租约。

一次同源码对照的普通 879 项从 **63.21 s → 33.56 s**，约 1.88 倍、减少 46.9%；最终源码普通组为 33.31 s。全流程动态构建也受缓存影响，因此没有把总时差全归因于 xdist，也没有宣称这是多轮性能基准或其他平台的保证。详见 [并行验证记录](2026-10-02-python-parallel-validation.md)。Windows 使用既有 native 门禁。

## 可观察的兼容性变化

这次包含公开行为及低层接口变化，不能只称内部优化：

| 变化 | 迁移/评估边界 |
| --- | --- |
| IPC 默认 prewarm/min-retained 为 0 | 空闲或刚连接时可能无 buddy 映射，首调用与回收时机改变。 |
| 有限默认 8/16/8 GiB 的 SHM/file/reassembly 三 cell | zero 禁止正 charge；此前可继续扩张的负载现在可能降级或明确容量失败。不是进程 RSS 上限，peer/SDK heap/网络缓冲不计入 owner backing 三 cell。 |
| pool_enabled=false | 只禁 buddy；dedicated、chunk、file 仍可用。禁本域 own SHM 用 shm_backing_budget_bytes=0；peer-opened mappings 是另一边界。 |
| 首次有效 connect 尝试冻结 policy，失败也冻结 | 配置应在连接前设置；同预算重连不会重置计费。 |
| ClientPool singleton 移到 Runtime | 低层 Rust 调用者显式持 pool；shutdown 真的关闭外部 client Arc 所指连接。已交付 held owner 仍有效。 |
| PoolStats.total_bytes/free_bytes/fragmentation_ratio 删除 | 用 buddy_data/occupied/idle、dedicated_mapped/pending_free 与 utilization_ratio，并结合 scope budget cells；旧字段访问需修改。 |
| ChunkAssembler/registry/carrier/token 接口变化 | finish 返回持 owner 的 ReassemblyBacking；旧 take_handle/promote 路径删除。低层直接调用者需迁移，普通应用使用 SDK facade。 |
| 新 prewarm/retained 关系、segment 索引范围验证与 bounded retire permits | 不再接受越界配置；byte budget 未满时也可能因退役 slot 上限拒绝未发布调用。 |

Rust 保持唯一策略、预算、缓存与释放权威；同进程直接调用仍零序列化，direct IPC 独立于 relay，SHM request 仍保持 RequestData::Shm。原 buddy复用→扩容→dedicated→checked chunk→接收端file 回退链没有换成另一套 SDK 逻辑。

## Windows 与当前交付状态

[Windows Native run 37034344557](https://github.com/Dsssyc/c-two/actions/runs/37034344557) 的四项任务及不可变产物验收均通过。独立验证分支 `socu/memory-audit-validation-20261002` 的源码 `2d360875b09a65324879cd489dc4096ea8657178` 与上述本机实现逐字相同，仅 workflow 触发条件不同；FastDB 固定同一 `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。

| 环境 | 完整门禁 | 基础门禁 | 18/12 行收据 | 隔离 wheel / 非管理员 wheel / 清理 |
| --- | --- | --- | --- | --- |
| Windows 2022 MSVC x64 | 23/23 | 通过 | 18/18、12/12 | 全部通过 |
| Windows 2025 MSVC x64 | 23/23 | 通过 | 18/18、12/12 | 全部通过 |

两套完整门禁还通过全部 9 行 IPC 内存矩阵。四个 ZIP SHA-256 均与 GitHub API digest 相同，所有内部文件的 bytes/hash 匹配 run manifest；每套下载 c3.exe 与本套两矩阵及两种 wheel 消费收据使用的字节相同。分别验证普通/非管理员消费者的 direct/relay、held checked-view 失效、4 个 borrowed input 失效、正常 host shutdown、进程/临时目录清理；非管理员 wrapper 的账号/工作区移除和零残余 PID 都通过。两平台二进制链接结果可不同，不把同一源码理解为不同 OS 镜像生成相同 EXE 字节。

[精简不可变证据](2026-10-03-windows-memory-audit-evidence.json) 记录 artifact ID、四个 API ZIP digest、每平台 c3/wheel hash、源码对和门禁计数。原始 ZIP、GitHub 元数据、run-evidence、完整日志和全部严格收据仅保留在 `/tmp/c2-full-review-1002/windows-37034344557/`，没有将二进制加入 Git。构建的 Python 验证候选版本为 0.6.0。

本次候选仅生成验证产物，本轮未执行正式发布；PR 未推进。Windows 11 桌面、browser/C++ C-Two SDK、其他本轮未执行环境不在本报告的通过范围。低层同一 IpcClient 对象跨 reconnect 的旧 SHM lease、跨 session sweep 的局部 ID 歧义以及 OS 调度/映射销毁的硬实时上限是已明确的边界；普通 facade 的 held/borrowed checked-owner 生命周期已由本轮测试覆盖。预算和关闭测试证明所列输入与交错，不是所有外部 unsafe alias 都可机械失效的保证。
