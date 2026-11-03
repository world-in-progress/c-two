# R01：Buddy 分配释放身份的独立复现与修复方案

状态：**已实施并完成本机、跨平台和独立审查验收**。执行基线 `3f030897`，生产候选 `de218272`，独立审查后的测试补强 `141668dd`。评估输入 `3382b3ba81363453d06cfe701ca845d33052756c`；瘦身前对照 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad`。以下保留批准前的复现与设计记录，实测结果见文末。纯瘦身已收束，当前剩余状态见[批次清单](../design/2026-10-13-local-slimming-batches.md)。本项是正确性修复，不计作瘦身收益。

## 已执行的复现

本机 Darwin arm64，`rustc 1.91.0 (f8297e351 2025-10-28)`。将两个固定提交的 `buddy.rs`、`bitmap.rs`、`spinlock.rs` 原文分别取出，编译独立探针，在独占的对齐堆内存上运行真实分配器。探针只操作分配元数据和比较坐标，没有读取 payload、建立重叠 payload 切片或写入 payload。

两版得到相同结果：

| 操作 | 现状 | 后果 |
| --- | --- | --- |
| 活 8192 B 块，按 offset 0 的 4096 B 子块释放 | `Ok(())`，count 1→0，free_bytes 0→4096，`can_retire=true` | 随后分配 4096 B 返回 offset 0，与仍活的原块重叠。 |
| 活 8192 B 块，按 offset 4096 的 4096 B 子块释放 | 同样成功 | 随后返回 offset 4096，与原块另一半重叠。 |
| 活 4096 B 左子块、右子块空闲，按 8192 B 父块释放 | `Ok(())`，count 1→0，free_bytes 4096→12288 | 空闲量超过 8192 B 数据区，且错误允许退休；随后可分配重叠父块。 |
| 两个活 4096 B 子块，按 8192 B 父块释放 | `Ok(())`，count 2→1，free_bytes 0→8192 | 随后可分配与两个活子块重叠的父块。 |
| A 正常释放后，B 在同一 backing 的同 offset/level 分配，再用 A 的旧坐标释放 | `Ok(())`，B 的 count 1→0 | 旧释放误作用于 B，随后又能分配重叠块；这是同位置重用问题。 |

正常分配/释放/合并、非法 level、未对齐 offset、越界 offset、立即 double-free、合并后的 double-free 对照均通过。这里的“通过”指探针正确观察现有行为，不是缺陷已修复。

此外，使用当前真实 `c2-mem` crate，运行 owner 与独立子进程 peer 的 `MemPool::open_peer → ensure_peer_segment → validate_data_at → free_at`。错误子块、父块以及同位置重用三个场景均复现：范围校验 `Ok(())`，peer 释放成功，owner 观察到计数被修改和重叠坐标。合法 5000 B 逻辑长度向上取整为 8192 B 的释放对照通过。这覆盖了真实跨进程共享映射与池释放入口，**没有运行完整 IPC 业务请求或 relay 回归**。

四次池实验各有一个新建且唯一命名的 SHM 对象。子进程均通过 OS wait 确认退出，owner 原生 Drop 后四个名字再次打开均为 `ENOENT`。实验未创建 socket 或 endpoint lease，没有扫描、清理、重建历史目录。

证据目录：`~/.codex/diagnostics/c2-r01-evaluation-3382b3ba-6cx0jvuu/`，包含 `source-manifest.json`、三个 cargo 探针、`baseline.log`、`current.log`、`pool-probe.log`、环境和带日志 SHA-256 的 `receipt.json`。三个 `cargo run --offline` 均退出 0。两个输入的 `buddy.rs` 与 `spinlock.rs` SHA-256 完全相同；`bitmap.rs` 仅差此前删除的空区间初始化循环。结合两版实跑，可确认这个缺陷在瘦身前已经存在，不能归因于本轮瘦身。

## 原设计与调用链

当前 [free bitmap](../../core/foundation/c2-mem/src/alloc/bitmap.rs) 的 1 表示空闲；0 同时覆盖“分配器交付的活块”“拆分后的父节点”和“被大块覆盖的子节点”。`alloc_with_split` 消耗 donor 的空闲位，向下拆分时只把右 buddy 标为空闲，没有单独记录最终交付的分配。

历史 `f1102bb7` 为 `free` 增加 level、对齐、范围和当前节点 double-free 检查，`2ccfb33f` 再增加 free ancestor 检查以拒绝合并后的重复释放。它们保护的场景仍成立；但“目标不是空闲节点”并不证明“目标就是原分配”。饱和递减 count 只能防止数值下溢，不能恢复分配身份。

当前释放路径均汇入 [MemPool](../../core/foundation/c2-mem/src/pool.rs) 和同一共享分配器：

| 路径 | 释放依据及现有边界 |
| --- | --- |
| Rust `Allocation → PoolAllocation → MemPool::free` | 保存 offset/level/backing generation；结构可复制，尚无每次 allocation 的身份。 |
| IPC 请求 | client RequestBlock → 15 B BuddyPayload → server RequestData/RequestLease → peer `free_at`；接收端根据逻辑 data_size 向上取整推导层级。 |
| IPC 响应与 relay materialize | server SHM allocation → BuddyPayload → ResponseData/ResponseLease → `ServerPoolState::release_response` → `free_at`。范围校验没有证明 allocation 身份。 |
| chunk/reassembly | `MemHandle::Buddy` 保存 allocation_size；trim 只改逻辑 len，释放仍使用原容量。外层 ReassemblyBacking 继续验证 owner incarnation，并在 backing 释放后退款。 |
| Python | PyPoolAlloc、原始 free_at、SHM buffer 投影保存/重建坐标；低层原始入口没有分配实例标记。 |
| C FFI / Node / TypeScript | request/response block 描述符与 block-key 集合只携带 segment、generation、offset、byte_length 等。集合和 lease 状态避免部分重复释放，但不能代替跨进程的分配身份。 |

backing generation 解决段槽位被回收再创建后的旧引用；本次同位置重用发生在同一 generation 内。正常 lease 的一次性消费也不能替代原始释放入口的校验。

## 推荐的最小完整范围：释放时验证分配实例

只增加 allocated bitmap 能修复错误父块/子块释放，但仍接受上述旧坐标重放；不能据此关闭“原分配实例的释放所有权”问题。本方案推荐一次补齐实例标记，验收收在**释放授权与分配器状态**。旧坐标读取、safe Rust 可变别名、失败释放 owner 保留仍分别属于 R02/R05/R07 等事项，不暗中宣称已修复。

1. 在 buddy SHM 元数据中，为每个 min-block 起点保存一个 64-bit live record，0 表示没有活分配从这里开始。非零记录将分配层级与该 backing 内单调、不回绕的序号组成 `allocation_id`。建议低 6 bit 保存层级，其余保存序号；当前 u32 数据几何最多 32 层。对调用者它是原样传递的 u64，不由 SDK 推导。
2. SegmentHeader 增加一个分配序号字段。保持现有 free bitmap 承担查找/拆分/合并；不另叠加 allocated bitmap、进程内分配表、第二把锁或后台任务。整个记录写入、校验、清除与 bitmap/count 更新都在现有 ShmSpinlock 临界区完成。
3. 分配前先检查序号还能产生新值；成功交付的最终块写入新 record，Allocation 返回同一 id。序号耗尽时拒绝新的 buddy 分配，保持现有 Option/分配 tier 回退约定，不回绕、不复用旧 id，不改变活块的释放能力或 GC 策略。
4. 释放沿原顺序检查 level、offset、范围及已有 double-free 条件，然后要求起点 record、推导层级与调用方携带的 id 完全一致。只有匹配后才清 record、更新空闲位和计数并合并。子块、父块、另一活块和同位置旧 id 均不得改变 allocator 的位图、record、count、free_bytes，也不得触发活 backing 退休。
5. id 沿既有 Allocation/PoolAllocation、请求块、响应元数据、MemHandle、wire、Python/C/Node/TypeScript 链传递。C FFI 本地 block-key 也包含 id，避免把旧对象与同位置新对象当成同一键。buddy 的 id 必须非零；dedicated 保持现有不重用索引的模型，描述符新增字段固定为 0，file-spill 仍由自有 mapping 释放。

`free_at` 继续接受逻辑长度并按原规则向上取整：例如申请 5000 B 对应 8192 B 块，5000 B 仍能正常释放。这里比较实际分配层级，不新增“释放长度必须等于原请求字节数”的约束。chunk trim 继续使用原 allocation_size/id 释放。

这个 id 是分配实例标记，不是加密认证能力。共享映射持有者能观察元数据；方案不承诺防止恶意对端窃取另一个活 id。读取路径的任意坐标访问也没有因释放校验自动解决。现有 `release_handle`/Drop 对释放错误的投影或吞掉行为不在此改成新的重试/owner 框架；allocator 拒绝错误释放与 SDK 是否暴露错误是不同的验收层面。

## 改动范围、格式与代价

| 范围 | 拟变更 | API/ABI 或语义影响 |
| --- | --- | --- |
| c2-mem allocator / layout / pool / handle | live record、序号、同锁精确释放；layout/init/attach/预算使用同一新布局；Allocation、PoolAllocation、MemHandle::Buddy 携带 id | Rust 低层结构新增必需字段，free/free_at 等释放入口新增必需 id；不留无 id 的释放入口。safe Rust 别名模型不在本项重构。 |
| SHM header | 版本 2→3，新增 u64 序号；计划 header 56→64 B，随后是原 free bitmap、live records、对齐的数据区 | 共享存储布局 ABI 变化；严格拒绝旧段，不在线改写、收编或迁移历史对象。保留 PID/poison 字段及同锁退休判据。 |
| c2-wire、c2-ipc、c2-server | BuddyPayload 增 u64 id，15→23 B；请求/响应/失败清理都携带；握手 11→12 | IPC wire 破坏性变化，严格拒绝旧握手和旧描述符，不增加版本回退；CRM 合同和 FastDB payload 格式不变。 |
| c2-mem-ffi、C header、Node loader、官方 TS connector | request/response block 加 id，键与 Drop/释放均传递；ABI 4→5 | C block 计划 16→24 B；先校验 ABI 再操作。TypeScript 使用 bigint 表达 u64，Node 必须校验无损转换，不能用 number 承载或替缺失字段补值。 |
| PyO3 / Python 低层投影 | PyPoolAlloc/SHM buffer/原始 free_at 传递 id | Python 低层调用签名和属性变化；正常 cc.register/connect/call/hold 的签名不变。 |
| 生成 TypeScript transport 与真实消费者 | `assets/typescript_transport.ts` 的 descriptor、15 B codec、握手常量和 SHM writer/reader 协议同步变更 | 自定义 TS SHM writer/reader 须返回/接收 id；生成产物与 connector 同步升级，无旧形状兼容。通用连接注入和 HTTP-only 惰性加载保持。 |
| 文档与验证材料 | 当前 ABI/握手/共享段规范、低层签名和本次验证收据 | 历史 manifest/receipt 保留其真实版本及 source，不批量改写旧证据。 |

默认 4 KiB min-block 每块增加 8 B record，即数据容量约 0.1953%。256 MiB backing 的 record 为 512 KiB：当前 data_offset 20 KiB，拟为 532 KiB，实际映射及 owner 预算增加 512 KiB；2 GiB backing 增加 4 MiB。8 KiB 小 backing 的新 metadata 仍装在原 4 KiB header page 内。以上是布局算术，尚未实施或测量性能。

预算和压力判定继续使用实际映射字节；固定预算边缘可能因此更早拒绝 expansion 或选择既有 fallback，这是必须接受的配置容量影响。极小 min-block 的元数据比例更高；所有乘加、对齐、data_offset 的 u32 表示以及总映射的 isize 边界须在创建前检查，不能因新表产生截断。

当前 `compute_data_offset` 从 `total_size - HEADER_ALIGN` 猜数据区容量；当新 record 表大于数据区时，这个旧公式不再成立。创建端应把 `checked_layout` 已计算的数据容量随现有 BuddyLayout 交给 init，并核对同一公式，不另猜几何；因此低层 Rust init 签名也可能改为显式接收该布局。`attach` 在构造任何新指针前核对 header 最小长度、版本、几何、levels、metadata 终点和映射边界。现有合法且可表示的极小 min-block 几何必须继续支持，不新增任意下限；例如 8 KiB 数据区、8 B min-block 的新 metadata 对齐为 12 KiB，总映射应为 20 KiB。仅在算术、u32 data_offset 或 isize 映射范围确实不可表示时，于创建前拒绝。这只是新布局必须成立的接纳，不把 R05 其余问题宣称已完成。

选择共享记录是因为 peer 在另一进程执行释放，进程本地 HashMap 不是共同权威；在 payload 内插头则会改变可用容量、offset 和分配阶数。仅查祖先/计数、继续饱和减计数或补 allocated 位都无法识别同位置新实例。方案会净增必要状态与测试；当前没有候选补丁，不虚报实际增量或吞吐收益。

## 确认后的实施与回归出口

先将上述反例转成现有 allocator/pool 测试里的行为用例，在基线验证敏感性；旧实现不能满足“错误释放拒绝且状态不变”。复用现有测试组织，不新增测试框架。生产链及其测试一起迁移，在完整链路完成前不接纳半套新格式。

| 层面 | 必需回归 |
| --- | --- |
| allocator | 错误左右子块、父块、另一活块、旧 id 重放、缺失/零 id、层级不匹配；拒绝前后 free bitmap/live record/count/free_bytes 不变，后续分配不重叠、活块不能退休；保留正常分配、合并、立即及合并后 double-free、坏 offset/level。 |
| 身份与布局 | 同位置释放再分配得到不同 id，旧 id 无效；backing generation 仍拒绝旧段；计数临界值/耗尽不回绕；init/attach 一致、新段跨进程可见、旧版本及截断/越界几何拒绝；实际映射预算准确。golden 与预期容量独立手写。 |
| 同步与生命周期 | 现有多线程/多进程共享分配、death observer、panic poison、忙锁零计数、退休/重新映射继续成立。失败释放不修改其他分配；不偷锁、不新增 owner watcher、无新的生命周期策略。 |
| 真实释放链 | 请求与响应双向 SHM、所有失败/取消清理、预分配 writer、chunk trim/hold 原容量、服务端 sink；合法非 2 幂长度、dedicated 和 file-spill 继续工作。 |
| SDK / FFI / 生成器 | PyO3 raw free_at、Rust facade、C block/ABI、Node bigint 边界、TS writer/reader 与 block-key、旧 ABI/握手/描述符拒绝、生成 consumers 的真实 SHM 请求与响应。错误拒绝与现有幂等外观分层验证。 |
| 整体门禁 | 完整 relay、真实 Rust/Python 及 TypeScript direct/relay 矩阵、持久连接/重连与跨进程释放；本批自建 socket/lease/SHM 收尾验收；独立审查。 |
| GitHub Actions | 对确切候选提交跑 Source Validation 的 Ubuntu、macOS 15、Windows 2022/2025，以及 Python 3.14t 适用门禁，核查实际各 job 和日志；所有必需项通过后才提交验收结论。 |

用户已确认上述完整释放身份方案及 SHM 3 / IPC 12 / C ABI 5 等公开变化；实施和验收记录如下。

## 实施验收（2026-10-16，完成）

生产提交 `de218272ac6643400f0c3e81a75f0bef3a521284` 将共享 live record 和不回绕序号接入原锁；请求、响应、chunk trim、预分配、relay materialize、Python/C/Node/TypeScript 释放链均原样传递标识。SHM 3、IPC 12、C ABI 5 直接拒绝旧格式。`attach` 以 header 的逻辑几何为依据，允许 OS 映射的页取整尾部，容量不足仍拒绝。长度取整、原容量释放、计费和生命周期策略保持；计费依据仍是 creator 显式映射长度，不扩展为 OS 实际提交页统计。R02/R07 没有随本项关闭。

基线先补五个反例，原生产实现为 0 passed / 5 failed，18 个正常与非法坐标控制通过。候选的七个 R01 用例及真实 c2-mem 全部 185 项通过；独立 owner/peer 进程验证错误子块、父块、同位置旧标识不能修改 owner 状态，5000 B 按 8192 B 合法释放，四个自建 SHM 原生 Drop 后重开均为 `ENOENT`。未通过重叠 payload 切片或读写来证明缺陷。

本机隔离完整 Core workspace 为 1704 passed、5 项既有 ignored，含完整 relay 416；Rust SDK 19、native Rust 8。Node 为 55 passed、1 项 Windows 专属 skipped；官方 Python driver 为 ordinary 1206 passed / 5 项 Windows 专属 skipped、portable 25、TypeScript 14，严格核验 18/18 Rust/Python 和 12/12 TypeScript direct/relay 收据，包含持久 IPC 和重连。首次普通套件的 1 项失败来自隔离根对应断言漏替换；补齐副本的严格路径断言后完整 driver 通过。Git/Node 22 预检查和初始构建失败日志均保留，没有修改 runner、期限或放宽断言。

两份原生独立审查和随后两文件差量复核均无生产阻断。按 `hey-my-buddy` 0.42.1 进行只读独立审查，也无生产阻断；其零 id 和不足映射容量两条测试建议以 5 行补入现有用例，形成 `141668dd41871acf1cd0d241ad8c9d6691ff7ce4`。分别移除生产零值检查、容量检查的隔离变异体各有 1 项按预期失败，原实现各通过 1 项，随后真实 c2-mem 185 项再次通过。只读产物为空补丁，Host 核验固定输入与候选 tree 相同后完成精确 artifact 验收。

对执行基线统计：生产 +371/−114，净增 257 行；测试 +778/−205，净增 573 行；非文档共增 830 行；文档 +52/−12，净增 40 行（含本记录和清单更新），全部净增 870 行。Rust 内联 `cfg(test)` 与测试 hook 按测试统计，consumer-install 脚本计入测试夹具，未将整个 Rust 文件计为生产。

同环境 release 分配器微基准固定交替 5 轮，每个尺寸预热 1 万、测量 100 万对 alloc/free，backing 256 MiB、min-block 4 KiB：

| 请求尺寸 | 基线中位 ns/对 | 候选中位 ns/对 | 中位差 |
| --- | ---: | ---: | ---: |
| 4 KiB | 1860.13 | 1861.42 | +0.07% |
| 32 KiB | 490.08 | 490.58 | +0.10% |
| 1 MiB | 184.94 | 183.77 | −0.64% |

差异处于本次样本波动内，不据此声明 RPC 吞吐或 RSS。实测 layout 为 268455936→268980224 B，增加 524288 B（512 KiB，数据容量的 0.1953%）；2 GiB 数据区增加 4 MiB。wire 描述符 15→23 B、C block 16→24 B、header 56→64 B。预算边缘会因必要元数据增加更早触发原有回退。

最终 GitHub Actions 输入为 `141668dd`，[run 38899277267](https://github.com/world-in-progress/c-two/actions/runs/38899277267) 在 attempt 1 全部 9 个 job 通过。逐份核对 Ubuntu 24.04（Python 3.12/3.14t）、macOS 15、Windows 2022/2025 原始日志：适用的完整 Core/relay、Rust SDK、CLI、native、Node/打包、完整 Python 三组、wheel、IPC memory matrix 和 Windows 普通用户门禁均实际通过，FastDB source 固定为 `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。首个 run `38898542899` 因上述 5 行测试补强由并发规则取消；其汇总门禁对 `source=cancelled` 拒绝，不算通过证据。

本次隔离默认根及各轮私有测试根最终 socket/lease 均为零；历史两个默认根在开始及结束均不存在，没有清理、重建或冷切换。812 个跟踪文件逐一核对，仅在隔离副本替换登记的默认目录及对应严格路径断言。Python 完整运行后仅增加上述 5 行 Rust `cfg(test)`，生产前缀逐字一致；最终 c2-mem 与线上候选覆盖该补强。

证据目录 `~/.codex/diagnostics/c2-r01-zvd34zy6/` 保留基线红绿、完整回归、严格收据、两组变异实验、源码/包/原生二进制哈希、独立审查、性能样本和 `local-acceptance.json`；构建物与本机证据没有加入 Git。
