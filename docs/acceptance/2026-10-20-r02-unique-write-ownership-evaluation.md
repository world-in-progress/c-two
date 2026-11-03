# R02：唯一写所有权核查与最小修复方案

状态：2026-10-21 已按用户批准的完整边界实施，最终实现为 `2d1b1d1095f3ab6e4d229fb354262bfc25dd113e`；基线为 `a523967b3fc41082ce4273178ce3b15af3948960`，其生产代码与评估输入 `032d9e6a` 相同。以下方案与估算保留评估时的事实，实际增量、开销及验收见 [R02 验收记录](2026-10-21-r02-unique-write-ownership-acceptance.md)。R06/R07 继续独立处理。

## 已核实的问题

`MemHandle` 没有实现 `Clone`，但公开的 Buddy/Dedicated 变体允许用相同坐标重新构造另一份值。`MemPool::handle_slice_mut(&self, &mut MemHandle)` 只独占借用这份可重建的描述符，然后返回实际内存的 `&mut [u8]`。`validate_handle` 检查代际、映射跨度及自报容量，没有证明调用者独占该 allocation。`allocation_id` 由 R01 用于精确释放，不是不可复制的写入权限。源码见 [handle.rs](../../core/foundation/c2-mem/src/handle.rs)、[pool.rs](../../core/foundation/c2-mem/src/pool.rs)。

这还包括三条绕行路径：`handle_slice` / `copy_data_at` 可在可变切片存活时读取同一区域；同一 backing 可以经两个 peer/owner 映射分别取得访问；公开 `pool.segment().allocator().free(...)` 和另一个 peer 的 `free_at` 可绕过原 pool 的借用关系。一次 live-id 查询只能确认当时仍有该分配，不能授予独占访问或阻止查询之后释放。[Rust Reference](https://doc.rust-lang.org/reference/behavior-considered-undefined.html#undefinedalias) 对引用别名的约束适用于这些库内部创建的引用。

上层公开入口也在边界内：`RequestData::Shm`、`ResponseData::Shm` 可以手造，随后经 safe `RequestLease` / `ResponseLease` 复制或释放；公开 `ResponseMeta::ShmAlloc` 可被 safe callback 当成已准备好的响应返回。`Connection::read_peer_data/free_peer_block`、`ServerPoolState::copy_response/release_response` 及其裸 pool 暴露允许继续按描述符访问。只给底层调用加 `unsafe`，再保持这些任意坐标的 safe 包装，不能修复 R02。

Host 在仓库外使用真实 `c2-mem` path dependency，编译了带 `#![forbid(unsafe_code)]` 的六个反例函数：同 pool 双写、读写并存、双映射双写、共享 allocator 在借用期间释放、peer 在原借用期间释放、可变借用期间裸坐标复制。`cargo check --offline` 退出 0；仅做类型检查，没有运行这些函数、创建共享内存或制造 UB。静态调用链与编译结果共同证明 API 缺口，不把它描述为已观察到的业务数据损坏。

## 原设计理由与真实调用链

`424dce3e` 于 2026-04-01 引入统一 handle 访问，为 buddy、dedicated 和 file spill 提供共同接口。后来的 [chunk lifecycle 设计](../reference/memory-policy.md)明确使用 pool 的共享借用同时读取一个块、写入另一个块，以便直接搬运数据。这个目标合理；错误前提是把不同 handle 值当成不同 allocation 的证明。该历史文本也曾误述共享 pool 借用可能冲突，不能用它代替当前 Rust 借用规则。

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
| 公开 Rust `c2-server` / `c2-ipc` | SHM 变体、低层手工准备响应、lease 的 pool 绑定及公开 raw 方法发生变化；`IpcClient` 的注入构造器移入 `MemPool`，`SyncClient::connect/connect_with_endpoint` 移入 `Option<MemPool>`；`call_bound_prealloc` 删除重复长度参数。`CrmCallback` 和 `Server::response_pool_arc` 提供固定的 `ResponsePool`，原始可变访问为 unsafe，正式准备响应仍走原 helper。外部低层消费者须迁移。高层 Rust SDK 的 connect/call/held 和生成合同签名不改变 |
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

## 实施记录（2026-10-21）

用户已接受低层 Rust API 变更及无兼容入口，要求保持 SHM 3、IPC 12、C/FFI ABI 5、预算、快照及生命周期语义，R06/R07 不在实施范围。按以下顺序完成同一个修复；阶段划分只用于组织验证，不把尚未封闭的局部边界作为最终交付。

| 阶段 | 出口 | 当前状态 |
| --- | --- | --- |
| 基线反例 | 正向消费者可编译；独立非法所有权/原始访问/上层构造案例在旧实现被接纳，目标断言如实报红；保留源码与日志 | 完成：1 个合法正例、2 个借用/移动控制例、18 个漏洞反例；另以真实双客户端确认预分配请求错池读取 |
| 所有权迁移 | 私有 owner、raw 边界、发布/接收载体及全部直接消费者一次迁移；定向回归通过 | 完成：21 条编译探针及真实分配、预分配、prepared reply、重连和释放回归通过 |
| 候选验证 | 完整 relay、真实跨语言/持久连接、同环境性能与布局对照；socket/lease 零残留 | 完成：Core/relay 1716、Rust SDK 19、native 8；Python 三组及 18/18、12/12 矩阵通过；9 轮交替性能对照及端点验收完成 |
| 线上与独立审查 | 确切候选的适用 GitHub Actions 全部门禁及独立边界审查通过，处理全部阻断发现 | 完成：最终候选 Actions 39523167030 的 9/9 作业通过；多轮审查发现均已关闭，最终固定候选复核通过 |
| 收尾 | 最终验收记录、实际代码增量和开销；清理不再使用的本次构建产物后提交 | 验收记录与清单同步；本次可重建产物已清理，保留源码、日志和回执 |

本次诊断根目录为 `~/.codex/diagnostics/c2-r02-implementation-20261021-1zef_zmt/`。本机完整回归在隔离副本中仅重定向默认测试命名空间及对应路径期望，保留替换清单和源码哈希；GitHub Actions 验证未替换的候选源码。历史目录只核对原目录元数据，不扫描内部对象，也不参与任何清理。

基线门禁仅编译，不执行反例。合法 round-trip 为 exit 0，同 handle 双重可变借用被 E0499 拒绝，借用期间移动/释放被 E0505/E0502 拒绝；十条公开坐标与上层包装漏洞仍均为 exit 0，因此目标断言如实失败。Host 与独立补测任务均取得预期红例；原始 fixture、逐例源码哈希、命令与日志留在上述诊断目录。初版补测含过多报告辅助代码，已先从 896 行收敛为 408 行、14 个文件后集成，未新增依赖或测试框架；后续 fixture 保持同一意图并按实际新 API 核对拒绝原因。

实施中补齐了客户端接收 pool 槽位互换、Connection 暴露 pool 替换、dedicated backing 重绑三个独立入口。前两项仅编译，后一项另用不创建 payload 引用的元数据实验确认：旧 dedicated handle 在 destroy 后可被任意 raw mapping 重绑重新变为有效；实验创建的两个 backing 均以原生 destroy 后的 ENOENT 验收关闭。候选将 raw dedicated 接纳标为 unsafe，隐藏接收 pool 槽位，并使每次握手的接收任务与响应使用同一新容器，避免重连改写已持有回复的 pool。

本地 owner 身份从初稿的 UUID 比较收敛为不复用的进程内 `u64` 序号，使用 checked atomic update 在耗尽时拒绝继续创建；每个 pool 一次，不在每次分配上做原子操作。已有 UUID 仍只用于原 OS 命名和协议身份，没有改 SHM/IPC/C ABI。`MemHandle` 保留真实 `PoolAllocation` 元数据；`alloc_shm_handle()` 直接复用原 `alloc()` 的 GC、预算和错误路径。纯指针查询保持 safe，底层未检查偏移的 `add` 改成不解引用的 `wrapping_add`；实际 raw 读、释放、映射接纳承担明确 unsafe 前提。

生产实现草稿的独立任务未予验收。Host 预审发现新增的 `try_release_handle(&mut MemHandle)` 在成功释放后仍保留可用 owner，并发现 GC、原始槽位和多余测试包装层未收敛；停止该任务、保存未验证补丁后由 Host 修正。最终工作稿不包含这一接口、第二套 RequestBlock handle 锁或 test_raw 包装层。`ReassemblyBacking` 继续在其原有私有 state 中执行释放，失败保留 owner/charge，成功后按原顺序 drop；普通 `release_handle` 的既有失败策略保持，R07 未夹带修复。独立审查及完整候选验收尚待完成。

首轮固定候选 `047cd58b` 的独立审查发现三条预分配入口尚未封闭。补充基线证据确认：共享注入池在 fill callback 内 destroy、发布时扩大长度仍可编译；真实双客户端测试中，A 的块经 B 发布会读取 B 池内相同坐标的数据。前两条不执行危险内存访问；后一条使用独立块与正常复制，结束时先关闭连接和服务器，再报告预期红例。候选改为移动注入池、从 owner 取长度、在 writer 锁内核对请求池归属后才执行原 dispatch/permit 事务。错误释放继续回到块的原池，未新增 payload 拷贝、owner 锁或状态机。

完整 Core 回归同时发现两处测试迁移问题：响应 lease 的 buddy 释放断言错误，以及禁止请求物化的文本检查误匹配新的响应 `into_bytes()`。已恢复 buddy 真实分配计数归零、dedicated 经 read_done 后原生释放回收的断言，并将文本检查限定到请求转发段；未跳过测试或改变生产 relay 逻辑。首轮候选未验收，GitHub Actions run `39470524323` 已取消。

第二轮审查确认，`Arc` 相同仍不足以证明容器里的池没有被替换：公开异步注入入口和 callback 响应池可保留原池、换入另一池，造成握手与发布不一致。新增两条基线编译反例确认该 safe 入口，未执行重叠引用。异步注入现与同步入口一样移动 pool；服务端用仅包裹原 `RwLock<MemPool>` 的 `ResponsePool` 固定容器内容，safe 读取和正式准备 helper 保留，raw 可变操作及 test-support 的原始请求池访问须明确 unsafe。没有新增池、锁或计费状态。候选 `baa3025d` 虽已通过本地完整 Core/relay、Rust SDK 和原生绑定，仍因审查阻断撤回，Actions run `39473694958` 已取消；最终已改以 `2d1b1d10` 完成全部验证。

第三轮审查继续封闭只读 guard 的原锁泄露：`parking_lot` 的普通 `RwLockReadGuard::rwlock` 可还原可写容器，故 `ResponsePool::read` 改为复用原锁的 `MappedRwLockReadGuard`。新增对应外部反例，保留精确类型诊断。核对辅助诊断脚本时发现 `copy2` 保留时间戳可能让 Cargo 复用旧探针，已用逐例重新写入源码的方式重验整组基线：合法正例通过，2 个控制例按预期拒绝，18 个漏洞反例均被旧 API 接纳；以新的完整审计记录替代零散编译记录。仓内正式门禁一直使用重新写入方式。Python 的响应物化门禁同步更新到 `result.into_bytes()`，其错误阶段和禁止静默吞错的断言不变。
