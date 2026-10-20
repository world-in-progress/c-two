# R02：唯一写所有权核查与最小修复方案

状态：评估完成，待用户确认实施。生产输入为 `032d9e6ad56e7dd1612acd3ba0ab1e22e43e22e0`；清单同步提交为 `b2365378`，仅修改文档。本次没有修改生产代码或测试。R01 已完成精确分配释放身份，R03 已完成 shutdown 发送阶段修复；本项不随这两项关闭。

## 已核实的问题

`MemHandle` 没有实现 `Clone`，但公开的 Buddy/Dedicated 变体允许用相同坐标重新构造另一份值。`MemPool::handle_slice_mut(&self, &mut MemHandle)` 只独占借用这份可重建的描述符，然后返回实际内存的 `&mut [u8]`。`validate_handle` 检查代际、映射跨度及自报容量，没有证明调用者独占该 allocation。`allocation_id` 由 R01 用于精确释放，不是不可复制的写入权限。源码见 [handle.rs](../../core/foundation/c2-mem/src/handle.rs)、[pool.rs](../../core/foundation/c2-mem/src/pool.rs)。

这还包括三条绕行路径：`handle_slice` / `copy_data_at` 可在可变切片存活时读取同一区域；同一 backing 可以经两个 peer/owner 映射分别取得访问；公开 `pool.segment().allocator().free(...)` 和另一个 peer 的 `free_at` 可绕过原 pool 的借用关系。一次 live-id 查询只能确认当时仍有该分配，不能授予独占访问或阻止查询之后释放。[Rust Reference](https://doc.rust-lang.org/reference/behavior-considered-undefined.html#undefinedalias) 对引用别名的约束适用于这些库内部创建的引用。

上层公开入口也在边界内：`RequestData::Shm`、`ResponseData::Shm` 可以手造，随后经 safe `RequestLease` / `ResponseLease` 复制或释放；公开 `ResponseMeta::ShmAlloc` 可被 safe callback 当成已准备好的响应返回。`Connection::read_peer_data/free_peer_block`、`ServerPoolState::copy_response/release_response` 及其裸 pool 暴露允许继续按描述符访问。只给底层调用加 `unsafe`，再保持这些任意坐标的 safe 包装，不能修复 R02。

Host 在仓库外使用真实 `c2-mem` path dependency，编译了带 `#![forbid(unsafe_code)]` 的六个反例函数：同 pool 双写、读写并存、双映射双写、共享 allocator 在借用期间释放、peer 在原借用期间释放、可变借用期间裸坐标复制。`cargo check --offline` 退出 0；仅做类型检查，没有运行这些函数、创建共享内存或制造 UB。静态调用链与编译结果共同证明 API 缺口，不把它描述为已观察到的业务数据损坏。

## 原设计理由与真实调用链

`424dce3e` 于 2026-04-01 引入统一 handle 访问，为 buddy、dedicated 和 file spill 提供共同接口。后来的 [chunk lifecycle 设计](../superpowers/specs/2026-04-04-chunk-lifecycle-design.md)明确使用 pool 的共享借用同时读取一个块、写入另一个块，以便直接搬运数据。这个目标合理；错误前提是把不同 handle 值当成不同 allocation 的证明。该历史文本也曾误述共享 pool 借用可能冲突，不能用它代替当前 Rust 借用规则。

`0c9591e9` 补充 backing generation 和 trim 后仍按原容量释放；`72b23e45` 补充 backing 预算；`f5c9aa33` 将重组内存、原 pool 和预算放入不可复制的 `ReassemblyBacking`。这些分别解决代际、计费与局部生命周期。R01 的 `de218272` 又解决错误块及旧 allocation id 释放，均未收紧公开坐标到引用的接纳。因此 R02 的根源早于本轮瘦身。

| 路径 | 当前分配与访问 | 应复用的所有权事实 |
| --- | --- | --- |
| 分块重组 | `alloc_handle → ReassemblyBacking → write_at → handle_slice_mut`；读取经 `copy_bytes/with_slice` | carrier 已私有保存 handle、pool 和预算；保持 trim 原容量、失败释放不退款及 Drop 顺序 |
| IPC 请求及预分配 | `MemPool::alloc → RequestBlock → write_at/fill → try_dispatch` | 现有 Armed/Dispatched/Released 相位及精确 pool 归属；保留无 pool 锁的 fill callback |
| IPC 请求接收 | 收帧后创建 `RequestData::Shm`，Core callback 经 `RequestLease` 复制、调用、释放 | 真正收帧处取得的只读与释放权限；不能由外部裸描述符冒充 |
| IPC 响应 | 正常回复由 server 真分配、复制、编码；低层 `try_prepare_shm_response` 支持 callback 填充 | 发布前的唯一写权限；低层 helper 的仓内调用目前在测试，不冒充 Core callback 的主路径 |
| IPC/relay 响应消费 | 解码成 `ResponseData::Shm`，经 `ResponseLease` 物化并释放 | 接收事件与既有 server pool 绑定；relay 沿用同一消费链 |
| C/Python 低层投影 | C pool 已有 mutex/local_blocks；Python PoolAlloc 是坐标投影，raw 地址接口另有不安全使用前提 | 这些描述符不充当 Rust safe owner；同步迁移 Rust 调用，不新增 SDK 所有权账本 |

## 推荐的最小完整方案

1. 将现有 `MemHandle` 收为字段私有、不可 Copy/Clone 的 owner，仅由真实分配创建；保留三种存储能力。SHM owner 绑定原 pool incarnation、generation、allocation id、原容量和逻辑长度。绑定复用 pool 创建时已有的 UUID 身份，采用固定大小内部标识，不为每次分配复制 prefix 字符串或引入全局注册表。safe 读取借用 owner，safe 写入独占借用 owner；保留 pool 的共享借用，使不同 allocation 可以独立访问。FileSpill 继续由自身 mapping 持有，不强加 peer/owner pool 限制。
2. 明确区分原始 `PoolAllocation`/wire 描述符和 owner。描述符仍可复制和编码，但不能安全地构造 owner。原 `set_len` 改为受检的逻辑收缩入口；保留原容量释放。跨 pool、peer 冒充 owner、destroy 后旧代际访问在构造切片前拒绝。原 `ReassemblyBacking` 的 pool pin、预算与释放事务继续使用；所有权校验可以归入 handle，避免另保一套相同事实。
3. 完整收紧 raw 入口：按坐标复制/释放、allocator 释放和不能自行证明边界的指针运算收为 crate-private 或明确 `unsafe`；跨 crate 必需入口保留一个带精确 Safety 前提的低层接口。当前 `BuddyAllocator::data_ptr(offset)` 内部直接 `ptr.add`，也必须纳入这个边界，不能把它误当作无前提的指针查询。已经是 unsafe 的底层 slice 接口不再新增平行入口。R01 的 identity、跨度、错误顺序及实际释放检查继续执行，unsafe 不代替校验。
4. 将 `RequestData::Shm`、`ResponseData::Shm` 的字段封装为不可伪造、不可复制的接收载体，由实际收帧路径创建；载体绑定现有 pool 归属并只提供读/释放能力。`RequestLease` / `ResponseLease` 继续承接现有释放逻辑，不能接受任意坐标升级成权限。响应 lease 从响应载体获取 pool，不再允许调用者另配一个 pool。`Connection` / `ServerPoolState` 的 raw 访问同步收紧。`ResponseMeta::ShmAlloc` 改为接收真实准备完成的私有 SHM 载体，由原分配/填充 helper 产生，不能手写坐标发布。
5. 写入到发布的移交消费或撤销本地写权限；请求复用已有 `RequestBlock` 相位，响应在现有发送边界移交。把读/释放权限的接纳限定在协议边界，前提是对端已完成写入、读期间 allocation 不重用且只有一个释放方。沿用现有写完发布、读完释放的协议，不添加 SHM 读写锁、跨进程借用表、重试协议或兼容入口；不延长 callback 持锁时间。

这是一组不可拆掉安全闭环的局部接口迁移。仅改 `&mut self` 无法约束独立映射，还会扩大 pool 写锁范围；仅查 live-id 无法证明唯一借用；仅标 unsafe 不提供正常路径的安全 owner。推荐方案同时保留正常路径的 safe 使用方式和 raw 层的明确责任。

本项保证本地 safe Rust 调用者不能伪造、复制或重新升级上述内存访问权限。它依赖现有协作式 IPC 内存交接协议，不把新类型描述为能约束任意不合作进程。Python exporter/释放同步仍属 R06，释放失败后保留 owner/charge 的剩余问题仍属 R07；不在此次所有权迁移中改变它们的错误策略、预算归还或生命周期事务。

## API、ABI 与语义影响

| 表面 | 计划变化 |
| --- | --- |
| 公开 Rust `c2-mem` | 破坏性变更：不能再手造/修改坐标型 MemHandle；长度更新收窄；raw 访问/释放调用须在明确不安全前提下使用。正常分配、借用、写入、收缩和释放有一个 owner 入口，不留旧签名兼容 |
| 公开 Rust `c2-server` / `c2-ipc` | SHM 变体、低层手工准备响应、lease 的 pool 绑定及公开 raw 方法发生变化；外部低层消费者须迁移。现有高层 Rust SDK 的 connect/call/held 和生成合同签名不需要改变 |
| Python | 正式 FastDB、pickle、Core bytes 与高层调用接口保持。低层 PoolAlloc/raw 地址是现有坐标能力，不因 Rust owner 迁移获得新的唯一写保证；绑定内部 raw 调用与安全说明同步更新，不新增 Python owner 注册表 |
| C/FFI 与 Node/TypeScript | 预计维持 ABI 5、现有符号与 descriptor 布局。C 函数本来就是 Rust `unsafe extern C` 边界；补齐并核对原有独占访问、存活期和移交责任，保留现有内部锁/所有权表，不能用 C 边界包装出任意 safe Rust 访问 |
| SHM / IPC wire | 维持 SHM 3、IPC 12；现有 identity 字段足够，计划不增加表示或兼容分支 |
| 运行语义 | 合法调用、快照、预算、trim 原容量释放、配置及生命周期保持；过去可用 safe Rust 手造或重复授权内存的低层用法会被编译拒绝，或在明确 raw 边界承担不安全前提 |

“无需升级 ABI/协议”是当前设计结论，尚非候选实测结果。如果实施发现必须改变线上表示、Python 公开调用行为或生命周期策略，应回到方案确认，不能夹带变更。

## 改动范围、成本与风险

生产范围集中在 `c2-mem/{handle,pool,alloc/buddy,dedicated}.rs`、`c2-wire/chunk/backing.rs`、`c2-ipc/{client,response,sync_client}.rs`、`c2-server/{dispatcher,response,connection,server}.rs`，以及 Core、C/FFI、PyO3 的直接调用点；Rust SDK 及生成器仅在确有低层调用时迁移。不会借此重写 allocator、scheduler、relay 选路或 SDK serializer。

静态工程量估算：生产净增约 200–400 行，测试净增约 150–300 行，主要来自私有载体、受控交接、raw Safety 前提和独立负向编译证据；文档约 50–100 行。这不是已实现的净量，也不是瘦身收益。实施后按实际 Git 差异重新计数；不能为达到估值删除行为覆盖或再造通用框架。

运行时计划不增加 payload 拷贝，不增加全局/跨进程锁或轮询。owner 绑定增加固定大小身份元数据及 O(1) 比较，已有 Arc 和接收/释放状态尽量移动复用。实际对象布局、原子操作次数和性能必须由同环境测量核实，不预先宣称零开销。主要维护成本是跨 crate 低层 API 的一次迁移，以及每个 raw 信任边界的可审查前提；统一到现有分配、发送和收帧位置后，不再让各 SDK 解释坐标所有权。

主要回归风险是：发送/取消边界提前释放，接收载体绑定到可被替换的错误 pool，fill callback 被新锁包住而死锁，trim 后按逻辑长度释放，失败退款提前，以及为封字段误删 low-level 预分配能力。上述路径都必须有原行为对照；类型封装本身不能当作回归证据。

## 批准后的验证与出口

先在未修改生产实现的基线上补齐六类反例和正常分配对照。反例仅编译或验证拒绝，不执行重叠引用。修复后使用现有 Rust 编译失败测试方式，核对具体隐私/移动/借用/unsafe 诊断；不能让缺依赖或错误导入冒充拒绝。补上上层 SHM 载体手造、复制、错配 pool、发布后写入和借用期间释放的负例，并保留合法不同 allocation 并行写的正例。不引入 trybuild 或新测试框架。

行为回归复用现有 buddy/dedicated/spill、generation、trim、重组预算、Held/Borrowed、请求 fill callback、取消/panic、peer release、关闭/重启及配额恢复用例。特别保留 callback 无 pool 锁运行、复制/释放错误合并和拒绝后预算/计数不变。独立手写 wire golden 与 C 布局断言继续覆盖未变格式。

完成完整 Core/relay、真实 Rust/Python 与 TypeScript direct/relay、持久连接、C/Node 消费者及相关 Python 原生回归；公开 Rust 变更按确切候选提交运行 GitHub Actions 的适用 Linux、macOS、Windows 2022/2025 及 Python 3.14t 门禁。记录同环境 allocation/重组吞吐、对象布局与额外分配成本；独立审查必须沿 safe API 追到最终 raw 操作，不能只审最上层类型。

所有运行测试使用本次新建的隔离命名空间，以原生 close/reap 验收 socket/lease 零残留；验证结束后及时清理本次 target、native 和打包产物，保留源码、日志、收据。历史目录不扫描、不删除、不重建。只有这些出口完成，才把 R02 从“待修复”改成“已验收”。

## 本次评估证据

本次 `cargo check` 六类探针通过，源文件 SHA-256 为 `0d454cb3bf27d410ce310e7926151a454b0de9c49895782e726e3b8101dfe770`，未运行探针。`cargo clean` 已删除本次 174 个构建文件（约 13.3 MiB），隔离 target 已不存在；源码、锁文件、日志、结果及源码哈希保留在 `~/.codex/diagnostics/c2-r02-evaluation-20261020-umuo8wpf/`。没有访问历史 IPC 目录，没有重跑此前通过的完整回归。

新版 hey-my-buddy skill 0.50.0 的独立只读任务 `58238e5d-701d-49fd-9069-86c3e2d73e03` 核对固定输入 `032d9e6a`，确认本地、双映射、上层 safe 包装及发布侧的闭环要求。Host 核对了源码引用、空补丁及固定输出 `e61d11a7-d056-4b67-ad38-65e27c3d3209`；任务已验收，工作树已回收。该审查没有运行测试；其调用链结论与 Host 编译证据分别记录，不作为尚未实施方案的运行验收。
