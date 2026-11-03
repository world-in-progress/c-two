# C-Two 瘦身建议的历史动机与回归复核

日期：2026-10-13。输入仍为 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad`。本轮逐项复核[首轮报告](2026-10-13-defensive-programming-review.md)的 R01–R39、T01–T07、C01–C07，通过 Git 引入/修复历史、当前源码和设计文档，重新判断哪些前提已经失效、哪些仍保护真实行为。以下结论优先于首轮的删除建议；首轮已同步修正主要误判。


状态入口于 2026-10-25 核对至主线 `2e9a40d2`：后续修复与 0.9.0 交付见[发布验收](2026-10-25-090-windows-release-acceptance.md)。当前数量、剩余子项、暂缓理由和复核顺序统一由[批次记录顶部](../design/2026-10-13-local-slimming-batches.md)维护；阶段净量另见[历史收束记录](2026-10-13-local-slimming-execution.md#closeout)。下表保留 `2ffe356f` 的历史动机判断，不把 R04、R06 等已完成修复重新列为当前待办，也不将后来完成的子项追溯为原输入已通过。
二轮最重要的调整是：无仓内生产读取者只证明当前依赖关系，不能单独证明信息或公开能力没有价值；两次校验只有在输入、锁、owner 和时间边界都相同时，才有合并依据；测试即使没有显式 assert，也可能保护不抛错、OS 创建/析构、默认值或夹具敏感性。应该先建立较小但完整的正确模型，再撤去由旧模型产生的分支。

较强的局部精简证据仍然存在，例如 FIFO 已取代的 waiting_writers 计数、bridge 工厂与构造器对同一值的重复检查、generated service_definition 重建同一 release、紧邻的空 handle 重验、不可达重试尾部，以及退出生产调用链的旧发布脚本。R01–R07 等所有权和同步问题应单独作为正确性修复推进；它们不能作为删除现有保护的理由。

本轮使用三路 buddy 只读复核，Host 独立检查改变分类的关键证据并整合。各工作树保持固定提交、无源码差异，均已验收并回收。历史报告中的通过记录只用来理解设计；本轮没有修改生产代码、执行候选删除实验、编译、运行回归或测量性能。表中的回归行为是落地验收条件。

## Relay 事件日志的信息是否有用

R26 的“可直接删除”建议撤回，改为暂保留的设计候选。提交 `9f4992ef` 从引入时就把它定义为有界、按 revision 排列的 authority watch history，并用 `cfg_attr(not(test), allow(dead_code))` 保留尚未消费的原语。设计文档[明确规定具体 mesh consumer 需要时才暴露 watch/list](../reference/contracts-and-routing.md)，并[记录了事件保留、tombstone 同边界压缩和 peer-leave revision](../reference/contracts-and-routing.md)。没有证据显示它曾是生产消费链、后来因迁移成为残留。

它保存的信息并非当前路由表的完全重复。[Upserted](https://github.com/world-in-progress/c-two/blob/2ffe356f15d8ce1c8fad9e08a91ffff808d34dad/core/transport/c2-http/src/relay/route_table.rs#L58) 保留当时的完整 RouteEntry；Removed 保留 route/relay 身份、route_uid、catalog_revision、removed_revision，以及 tombstone 删除或 peer leave 原因。覆盖或删除路由后，旧实例、旧合同、修改顺序及删除原因不能由当前 routes/tombstones 完整重建。这些信息可以支撑未来增量同步、断点恢复或诊断。

当前消费关系也有明确边界：relay 的 `watch_from` 只见测试调用，没有公开 HTTP 或诊断读取入口；数据面使用 resolve/probe/call token，anti-entropy 使用当前 routes/tombstones/hash。实际 [GC 运维日志](https://github.com/world-in-progress/c-two/blob/2ffe356f15d8ce1c8fad9e08a91ffff808d34dad/core/transport/c2-http/src/relay/background.rs#L100) 直接读取 TombstoneGcEntry，不依赖这个 journal。`c2-server` 的 [IPC catalog watch](https://github.com/world-in-progress/c-two/blob/2ffe356f15d8ce1c8fad9e08a91ffff808d34dad/core/transport/c2-server/src/server.rs#L2411) 则有真实消费者。三种机制必须分别判断。

journal 当前是有界内存历史，尚未构成持久化审计或已交付的诊断能力。其条数上限为 1024，工作发生在路由变更路径；本轮没有测得值得牺牲计划能力的 CPU/内存成本。因此保留它是在保留明确规划的能力基础，也不应把它描述为当前 RPC 正确性的必要依赖。

如果以后明确裁撤这项能力，可以只删除 events/watch_from/min_watch_revision 和专属 clone，同时同步设计文档。必须保留 catalog/compaction revision、tombstone 防复活、peer leave 每条删除推进 revision、现有 GC 诊断和 IPC catalog watch。验收应以同一 upsert/remove/peer-leave/GC 序列，比较变更前后的 resolve、digest/full-sync、tombstone、revision 与 GC 输出；不能直接删除“事件相关”整段代码。本轮尚未进行这种候选 A/B 验证。

## 实现条目的逐项复核

表中的提交号来自当前仓库 Git 记录。“推断”表示历史没有直接说明作者动机。局部精简仍须运行受影响的现有行为测试；“条件重构”表示目标类型或所有权落地之前，现有检查继续保留。

| 编号与判断 | 当初的理由及今天的作用 | 最小边界与回归验收 |
| --- | --- | --- |
| R01 · 保留 P1，独立修复 | `f1102bb7` 保护 double-free、非法 level/offset；`2ccfb33f` 增加合并后的祖先检查。它们仍有用，但 USED 位不能证明目标块实际分配。 | 建立精确活分配证明后承接现有检查。错误子区间释放必须保持位图、alloc_count、free_bytes 不变，活块不退休、不重叠；正常释放、合并后重复释放及 generation 拒绝继续成立。 |
| R02 · 保留 P1，独立修复 | `424dce3e` 用统一 handle 访问 buddy/dedicated/file；`0c9591e9` 补 generation 和 trim 原容量。便利性有理由，safe API 的唯一写所有权仍未建立。 | 入站坐标与私有 allocation owner 分开，同时约束重复打开同 backing。仅字段私有或单个入口改 `&mut self` 不足。保留正常读写、trim 释放、外部跨度拒绝，验证非法别名无法由 safe API 构造。 |
| R03 · 保留 P1，控制语义修复 | `026f06dd` 的探测需要跨暂时不存在/满 backlog 重试；`c8298c3f` 明确 shutdown ACK 只证明 initiate。 | 仅 shutdown 区分 NotSent 与 DispatchUncertain，包含外层超时阶段；保留发送前重试。完整/部分发送后丢 ACK 不得重发给替换实例，原实例只进入一次 drain。 |
| R04 · 保留 P1，状态事务修复 | `c8298c3f` 令牌保障 relay 最终发布前不接业务；`6e38c28e` 增加 OwnerMissing 提前关闭。当前 token 已经 move-only。 | 绑定具体 UID/登记代际，开放与关闭共享同步边界。保留 shutdown generation；旧 token 不开同名 B，OwnerMissing 竞争后 callback 为 0，正常最终发布可开放。 |
| R05 · 保留，补完整接纳 | 早期 attach 依赖本方合法初始化；`93321ed6` 版本 2 保护 64 位锁布局；`72b23e45` 主要修创建几何。 | safe open 验证完整 header/bitmap/data 布局并冻结本地几何；共享锁/计数仍动态。拒绝短 header、越界 offset、非法 block/levels、旧版本，保留 Unix/Windows lazy-open。 |
| R06 · 保留，同步边界修复 | `6e6519a8` 的 exports 阻止 memoryview 存活时失效/释放 owner。为何放在锁外，未找到明确说明。 | export 检查与 release 在同一 owner 锁中；保留 exports 和失效回调。拒绝释放时 owner/lease 仍有效，解除导出后恰好释放一次。当前证据不支持直接 UAF 结论。 |
| R07 · 保留，限缩重试承诺 | `424dce3e` 原先忽略 free 错；`ba1b2914` 为 dedicated 延迟 GC 增加 FreeResult；没有发现吞错策略承诺。 | 保留未释放 owner/charge并投影错误。竞争恢复后可重试；poison/dead-holder 要按 backing 拒绝/退场处理，不能承诺普通重试恢复。公开缺陷与未证实的业务影响分开。 |
| R08 · 改为显式解析策略变更 | `657891a6`、`dd014af8` 规定来源优先级和 numeric parse failure；未明确说被覆盖的坏 IPC 值必须忽略。call limits 后来明确使用 winner-first。 | 可以统一先选来源再解析，但要记录错误行为改变。验证 valid override+bad env、未覆盖坏 env、base/flat 顺序、process/env-file、零值及空值；空字符串统一报错另作策略决定。 |
| R09 · 保留窄配置快照优化 | `a9111b81` 整理接纳，独立 resolver 保持路径隔离。ConfigSources 已捕获一次进程环境，变化风险主要来自反复读取 `.env`。 | 一次 EnvCatalog 派生这四项 relay 设置，不换成宽泛全配置解析。保持 deadline、attempt clamp、错误类别，显式 IPC 继续忽略无关坏 relay 配置。 |
| R10 · 限缩为条件重构 | `2ccfb33f` 接纳 bridge，`307ad191` 补 borrowed 门禁，`89fc9f8d`/`6e6519a8` 接纳 canonical/native 合同。borrowed 要求注解、精确位置参数且禁止 bridge.input，普通 conformance 不等价。 | 同次注册可共享归一化/合同快照；独立入口和 native 接纳仍保留。计划不能默默冻结每次 getattr 的动态 Resource 方法。验证注解/构造/hook 次数、错误顺序、方法/access/shutdown、动态修改及副作用前拒绝。 |
| R11 · 条件重构 | `c8298c3f` 两阶段登记比较 relay 声明与执行事实；`38a1b4e3` Host 才从同源生成两份输入。当前双输入接口仍需要失配保护。 | 先取消独立 RuntimeRouteSpec 输入、从 BuiltRoute 派生，后迁移失配测试。reserve 的同值 wire 检查可局部收敛，但重复/pending/关闭等当前状态继续核验。 |
| R12 · 降为条件优化 | `aa4ba87d`/`ee91bfa3` 修 late route；`737d13d1` 改 endpoint 共享；`a9111b81` 修 exact-token 正缓存和单 route 错误关闭共享连接。 | endpoint connector 与一次精确 live acquisition 分离，保留 await 后 owner/lease 栅栏。观察 late route、真实 idle reconnect、A 失败 B 成功、A 在途不被关闭、替换 owner 拒绝；不能退回握手或正缓存 authority。 |
| R13 · 限于拥有型移动及纯编码 | `6da0ba7d`/`1439e0b5` 的副本使 spawn 不借 receive-loop frame；`38a1b4e3`/`6e6519a8` 使 Python exporter 不借 Rust 栈；`5ed04149` 明确可变/只读别名和 bytes 子类 snapshot 边界。 | 已独占 request Vec 可研究直接移入 exporter；响应快照、初始 owner 转换不可整删。验证可变 backing 隔离、finalizer 线程、borrowed/held、释放错误、帧字节与部分发送隔离；普通整批锁和流式逐帧锁不强行合并。 |
| R14 · 仅锁内局部去重有依据 | `5292a998` 保护共享 pool 容器替换；`72b23e45`/`cb9c0e5d`/`365cf948` 保护已映射复用、GC 后重试、实际 backing 计费及较小 dedicated 回退。bitmap 注释明确 CAS 是锁前遗留。 | CheckedSpan 不能逃离当前 guard；保留 owner/generation、压力、退款、SHM-only 与 file fallback。锁 token 封闭后可简化 bitmap CAS。验证 tier、charge、多进程合并和 owner 替换；公开 pressure_denied_backings 计数变化也要解释。后续 B18 已完成私有 SHM selector 去重；GC 时机、计数、释放及公开接口保持，其余子项仍待设计。 |
| R15 · 降为维护进度优化 | `952b7014` 及 chunk-lifecycle 设计要求软阈值 GC、不因软阈值拒请求、registry 与 runtime 解耦；timer 由 transport 拥有。 | 同步 sweep 仍可能在下一次 admission 前释放预算。限频/合并必须保留无 timer 使用下的回收与恢复准入，争用后 duplicate/GC 重验继续保留；性能优先级待测。 |
| R16 · 新容量设计，非局部删减 | `2ccfb33f` 将 Node data 事件适配为 readExactly；分片与消费解耦有必要，未发现有意无界的设计理由。 | 容量和读取进度一起管理。当前读者等整帧才 take，若低水位 pause 小于合法帧会死锁。验证大读取进展、队列上界、顺序、EOF 缓冲、close 唤醒及 Unix/Windows/注入连接。 |
| R17 · 保留错误分类修复 | `da5ca3be`、`5ed04149`、`b0031040` 建立固定 gate 和目录描述符核验。没有发现把 stat I/O 错误隐藏掉的设计承诺。 | IoError 不报 AlreadyAbsent；RecordMismatch 当前仍保守拒绝，影响程度不同。保留第二次 inode/gate/credential 核对，失败不删除对象，扫描仍受预算约束。 |
| R18 · 保留 observer 状态修复 | `512a3654` 建 control observer，`1a6eed40` 增加不可用状态，`5ed04149` 增加受限 owner。永久 Closed 时退出合理，Capacity 不应等同。 | 容量拒绝保持显式观察不可用并在原 owner 内恢复，保留上限和数据面 live acquire。验证满容量、释放后恢复、shutdown 终止等待、旧 owner 不影响新实例；不能据此声称数据面必然调用错路由。 |
| R19 · 保留，限定响应契约 | `40152815` 迁移 CLI 时引入 Value 兜底。显示可用字段是合理推断，未找到畸形容器必须显示为空的承诺。 | 强制外层形状和必需身份字段，保留未知附加字段/未知状态文本；避免强制 CLI 接纳全部内部 RouteEntry。分别验证合法空、畸形 200、正常 peers 和 HTTP 错误。 |
| R20 · 保留防覆盖修复 | `dc89b8a4` 同时引入加载失败默认初始化及原路径保存。便利性可推断，允许覆盖坏文件无明确证据。 | 只有确认不存在才新建；不能用隐藏访问错误的 exists 判定。损坏/schema/读取拒绝不得生成可写回默认资源，原字节保持；正常新建/加载/保存继续可用。孤立 Redis helper 可退出。 |
| R21 · 修模型，保留 formatter 职责 | `fdd5fcce` 引入 AST 模板，`1932a079` 改术语后保留旧装饰/继承输出。工具仍公开导出。 | 生成普通 Resource，修 ast_node 失效兜底；验证导入、补实现后真实 register/call、继承/default 参数/依赖/shutdown。varargs/kwargs 测的是 formatter，不能仅凭 CRM 不支持就删覆盖；公开工具退役另定。 |
| R22 · 旧 selector 可删，测量路径须保留 | `ed096c5a` 建立 14 项矩阵，`89fc9f8d` 删除 12 个实现却遗留 sweep；手工控制路径后来仍补齐 instance/contract/payload limit。 | selector 单源、旧标签退出、NumPy 微基准如实命名。迁移控制面必须保留 `address=relay_url` 强制 HTTP，避免默认 resolve 选择 loopback IPC 污染结果；保留真实身份、header、失败退出及清理诊断。 |
| R23 · 有效性问题与签名退役分开 | `f0fe1345` 把 timeout 交给 native，`6e6519a8` 改为校验后丢弃，迁移到唯一 Core owner。独立 Server 仍需要配置接纳。 | 现有 session 路径可去影子字段；独立构造由 Core 接纳。明确撤销 timeout 签名或恢复 Core 预算；恢复时非法值测试继续有用。验证启动/重复启动、外部 stop 后 barrier/重启、真实配置和 identity。 |
| R24 · 允许明确退役旧关键字 | `307ad191` 将 borrowed 权限移到 registration，`89fc9f8d` 将 buffer 允许集缩至 view；旧 hold 拒绝文案曾有迁移诊断价值。 | 在 0.x 整链撤销旧参数和方法级元数据；旧关键字明确拒绝。保留 MATERIALIZED/BORROWED 运行时分流、bridge.input 冲突门禁及客户端 hold；验证 checked view 失效和默认输入可存活。 |
| R25 · 降为协议封闭后的优化 | `454f7dc5` 能力探测区分 prepared IPC 与当时 HTTP；`fd7636c4`/`c7a853c3` 保护 Closed 投影且避免误判业务 RuntimeError。CRMProxy 工厂仍接收公开 client/scheduler。 | 先规定受支持协议，再在该协议内删 getattr/text fallback。验证 thread 直接对象、IPC/HTTP 先准备后序列化、失败退款、缺能力接纳错误、Closed 上下文，以及业务 `RuntimeError('route closed')` 不被误判。 |
| R26 · 撤回直接删除，暂保留 | `9f4992ef` 和当前计划明确保留未来 relay watch/list history。旧快照及删除原因有独有信息；目前无生产读取。 | 见上文。能力取舍前保留；若退役，限制到 journal 面，不删除 current authority、revision、tombstone、GC 日志和实际 IPC watch。 |
| R27 · 维持局部删除 | `026f06dd` 旧调度确实用 waiting_writers 阻止新读者；`c8298c3f` 改 FIFO 后，计数失去决策和 snapshot 消费者。 | 删除计数/登记布尔/加减；queue、ticket、notify、pending/active 保留。用现有读写公平、async/blocking FIFO、取消队首、read-parallel 测试验收。 |
| R28 · 维持纯值合并，保护边界不合并 | `2ccfb33f` bridge 构造/工厂同值校验；`89fc9f8d` 冻结 PayloadBinding。Rust `c96011da`/`661ee342`/`986c5278` 保护 peer 接纳、hash 和原子更新；`07296832` 保护 stale owner。 | 工厂依靠构造器，validated envelope 用 into_parts，私有同值语法检查可收敛。保留 peer 活性/URL、owner/locality、缺 owner、tombstone 时序和无部分 mutation；拒绝类型/文案及 borrowed kind 行为保持。 |
| R29 · 维持简单重复，FFI 缓存有条件 | `53bf9c68` 生成 service 重建 release；`5893adbf` 特意交叉检查 FastDB digest/canonical/manifest/capabilities；`b0654721` 产生 canonical release ref。 | `release.reference()`、生成 u16、内部 typed JSON projection 可局部落地。编译缓存只限同次、相同字节和选项，保留 outer_path/provenance/identity conflict。wire preparation 不削弱公共 identity、短 buffer 和入站检查。 |
| R30 · 配置类型重构后再删除 | `657891a6` 引入 base/flat 与派生 max_pool_memory；flat 面向 SDK 是合理推断。`01a67728` 修 relay upstream 完整策略传播。 | 原复核要求先统一有效配置，保留 checked overflow、优先级、输出投影和 cache equality。后续 B10/B15 与两次 R30 续批已完成私有去重、嵌套 base 退役和派生字段删除；R08 已完成保留原行为的解析收敛。当前边界及验收见[批次记录](../design/2026-10-13-local-slimming-batches.md)，不再列为待讨论的类型重构。 |
| R31 · 维持重复状态合并 | `717020f7` 建 Held/BorrowedPayload；`6b017f2e` 原子冻结 config；`5ed04149` 加 revision 重验；`def93646` 明确 sweep iterator 先于 lease 析构。 | Option 配对 owner、直接 Payload、Option 作为冻结事实、Option<OpenSweep> 均可用。保留 invalidation-before-release、失败/Drop 幂等、setter/首次连接竞争、提前失败不误冻结，以及公开 closed 投影与释放顺序。 |
| R32 · 限于局部转换，保留公共语义 | `0dcb11d6`/`ca0fd812` 统一 native wire/registry；`6e6519a8` 的 Transport 外层 message 不等于内部 semantic message；`def93646` 凭据支持存储和跨进程传递。 | 只绕过完整 Semantic 的内部 wire 往返；公开 ERROR_CODE 可由 registry 派生并保持值。保留 Transport 内层错误、unknown/name mismatch、phase/cause、canonical 字节和外部 credential JSON/reap/Windows 语义。后续 B19 已完成 Rust 错误码单一声明；其余转换与序列化子项仍未实施。 |
| R33 · 维持具体孤儿删除 | `af85b2c2` 后计划曾有意保留通用 wrap_error；今天无调用且协议已失效。Core 接管后私有 config helper 失去消费者；`849f905d` 的 process_guard 仅自身测试使用。 | 删除确切符号并同步仍列 wrap_error 的 AGENTS 指导；保留 native config、manifest/receipt 和 package_consumers._stop_host 的真实清理。Windows 顶层 kill 不等于进程树清理，不能借机合并不同终止语义。 |
| R34 · 私有副本与公开能力分开 | `0c9591e9` prefix/generation lazy-open、`a9111b81` TS live lookup使部分副本失去用途。设计明确 RouteAck 预留诊断/flow-control；Connection 有公开观察方法。 | 私有未读快照可清；RouteAck/公开 accessors 需退役决定，协议 0x14 保留。减少 TS 对象构造仍完整消费/校验 wire；保留 CAP、prefix/generation、compaction list→lookup、late route/旧 token 拒绝。 |
| R35 · 仅内部任务优化 | disabled park 统一 task handle/abort 收尾是合理推断；未找到必须 spawn 的明确动机。run_heartbeat/HeartbeatResult 为公开 Rust 面。 | interval=0 可不 spawn、用 Option handle 收尾；不顺带删公开 Disabled 或改默认值。验证禁用不发 ping但通信/关闭正常，启用时 idle ping、timeout、写失败一致；收益待测。 |
| R36 · 维持已证明的局部删除 | `ee91bfa3` 重试修复权威获取；`a9111b81` 再修正缓存。bitmap 几何、同锁 retention 总和提供局部不变量；_created_at 曾标作 GC metadata，但当前只读 last_activity。 | 删不可达尾部、空区间循环、紧邻重验、被全域检查蕴含的局部加法及未读字段。保留最终实际错误、重试次数/等待/live lookup、closed/limit/overflow及失败不改 charge；标签/诊断行为不意外改变。 |
| R37 · 维持旧脚本退出 | `a5ffa4e0`/`d69016b0` 原用于避免重复发布；`f04f6d5c` 改 verified-artifact promotion，`90828387` 在新链实现完整版本 skip。 | 删除旧脚本和专属测试/CI 条目；新链的自动 skip、手工 collision、partial retry、byte identity、非 404/网络错误拒绝继续保留。新旧实现无需保持并存。 |
| R38 · 仅同步 readiness 后的等待 | `dbb63969`/`580a1abf` 加 0.3 秒等待；异步启动缓冲为推断。`f0fe1345`/`6e6519a8` 后有 native readiness。普通 bytes/dict 返回不带 hold owner。 | 两个 fixture 在 register 已保证 listener/route ready 时可去 sleep，保留 startup/restart 测试；普通结果的 release 探测可删。跨进程输出、业务延迟和 GC decay 等待不能套用。 |
| R39 · 撤回“全路径配置已验证” | `5c11dfda` 的统一命令结果需要处理异种结果；`da5ca3be` 私有 expected 入口都传 Some；`466e9fd3` 参数接纳还有诊断优先级；`acb74901`/`5ed04149` body 校验保护 chunk 与 retained owner。 | 操作专属结果/必需 proof 可收敛，保留真实 mismatch/当前 inode 核验。RelayAwareClientConfig 仍是 raw u64，new_with_control 未校验 chunk；须先统一所有入口，才迁移 helper(0) 和错误释放测试。0/1 参数提前拒绝会改变错误 path/顺序，应明确记录。 |

## 测试建议的修正

| 编号 | 历史动机与重新判断 | 后续保留或替代的行为 |
| --- | --- | --- |
| T01 | `f04f6d5c` 的源码词串门禁为 canonical repo、main push 和受验候选服务。首轮“全注释仍通过”反例成立，但结构 lint 本身有价值。 | YAML jobs/needs/if 解析、AST/编译限制与真实 promotion 负例互补；全注释、错 needs、条件移到无效 job 应被拒。保留 provenance、字节相同、manual collision、自动 skip 等完整门禁。 |
| T02 | name-only 测试 resolver 在合同化后成为第二套算法，可迁移。`4e94fe6f` 明文把禁用重试作为同夹具、同事件、同预算的反事实敏感性证明，撤回直接删除它的建议。 | resolver 用生产 resolve_matching 验完整合同/local-first/排序/dead-peer。重试正例与“禁用后正例不能成功”的证据继续存在；如重构 seam，提供等价敏感性验证。 |
| T03 | `b042d84b` 的部分顺序由测试安排，`6e6519a8` 假 Python owner不能证明完整 lease；`5ed04149` FakePayloadOwner 测 scope/retention 独立性则有效。 | 纠正过强的集成命名，替换假集成；保留直接调用真实 HeldResponse 的 primitive、异常组合和 budget 单测。真实 Rust/Python/FastDB/transport 链覆盖正常、错误、取消和 late result 的顺序及恰好一次清理。 |
| T04 | `32b0ace7`/`7bfef112` 使用阻塞 readline；`e526d99a` 的宽异常观察服务于 drain/GIL 回归。场景仍必要，判据有盲点。 | 沉默和无换行输出都受同一预算，方案适用 Windows pipe；失败关闭并 wait 自有进程。join 后断言所有线程结束及完成数量；ResourceClosed/route 明确匹配，其他异常失败。 |
| T05 | `986c5278` 登记门禁增强后 no-fallback 用例缩成 400；`6e6519a8` hash 投影断言退出后标题未改。HTTP digest 负例混入坏 hash，重连用例先等 observer 删路由。 | 如实保留注册拒绝/观察撤路由覆盖，另隔离变量验证真正重连、业务/HTTP次数为零、ABI/signature 分别拒绝。公共 hash helper 自己会拒非法 CRM，需明确错误分类或独立 canonical-hash 夹具。shutdown hash 对照保持同一 CRM 身份。 |
| T06 | 弱 GC 用例同时被 floor=1 和默认 decay=60 秒掩盖；只改 floor 不够。`2ec9cd82` expansion 容忍 dedicated 曾保护安全回退。OS create/drop 与非零内存探测有 smoke 价值。 | 可由已有 floor=0、decay=0、真实读写及释放后归零用例替代弱 GC。buddy 扩容与 fallback 分别验证 tier。合并 create/drop 到内容/清理测试；物理内存 provider 和平台 smoke 明确区分，保留 Windows 删除和进程终止清理。 |
| T07 | 两项“类型测试”还保护 5.0/131072 默认值；release_unknown 明确保护不 panic；CAS stress 虽不确定命中 CAS-loss，但保护多线程有界完成和锁归零。撤回按无显式断言整删的建议。 | 默认 golden 合并后再去类型赋值，构造器默认传播仍测；未知 release no-op/其他条目不变仍测；guard 构造绑定前不删二次 attach 拒绝。改合法未监听地址测试连接失败，CAS-loss 可注入或如实改名。 |

## 原设计候选的边界

| 编号 | 复核后判断 | 必须保住的边界 |
| --- | --- | --- |
| C01 | 私有不可变合同仍是合理目标；`33b0f303`、`2590745f`、`ee91bfa3`、`a9111b81` 分别保护语法、call token 和 live acquisition，职责不同。 | 只删除同一本地值的重复语法接纳；ExpectedRouteContract 收窄不代表另一公开 RouteCallIdentity 自动可信。远端合同、UID/revision、server instance、late route、重连、local/relay 选择继续验证。 |
| C02 | `b042d84b` 已有 RequestLease；候选是将其提前覆盖裸 RequestData 接纳，并共享目录/执行登记实体。现有泄漏未证实。 | 拒绝/取消与 callback 移交恰好释放一次，保留 copy/release 错误组合。Catalog 与 Dispatcher 间有 await，未建立原子一致结果前不删第二次身份核验。 |
| C03 | 原报告的局部 GC 风险仍待复现；`f5c9aa33`/`15578b5c` 已有 ChunkAssemblyId、publication guard、线性点和 abort fence，不能描述为全链缺世代。 | 先按具体 assembly identity 条件删除 stale pending，或证明同代 owning。受控“旧项采样→退出→同 key 新代”不能归还新 permit；保留 timeout/rollback/abort 和 finished backing 计费。 |
| C04 | 5 ms/10 ms/50 ms 分别观察 drain、外部最后 Arc、动态 route authority；`a9111b81` 最近还修复了清理和正缓存失效。 | 通知必须覆盖状态发布、最后 owner 消失、native 终态及真实控制变化后，再逐项移除 timer。保留无丢唤醒、aggregate deadline、unconfirmed owner、transport error 不撤路由；性能尚未测。 |
| C05 | `5ed04149` 明文保证 appended context symbols 下旧 ABI 3 仍有效；`b0031040` 后默认 Unix 不足以代表 Windows/注入连接/查询。`09166a19` 的 SyncClient 预分配用于 FFI 零复制，`ac4ca86f` 刚补精确 pool owner。 | 0.x 可以 clean cut，但须明确 ABI/公开能力退役、同步 loader/SDK/docs，不能保持旧 ABI 承诺却强制新符号。保留 RequestBlock 直接 fill、panic/error 精确释放、context freeze、HTTP-only 惰性和 Unix/Windows；协议 0x0C/0x0D/0x14 不重编号。 |
| C06 | 撤回“完成契约未核实”的表述。[round_complete](https://github.com/world-in-progress/c-two/blob/2ffe356f15d8ce1c8fad9e08a91ffff808d34dad/core/transport/c2-local/src/lib.rs#L191) 明确只表示该 batch 到达目录 EOF；与局部 io_errors 共存合法。 | 可以另设计严格 CLI 成功策略或枚举失败投影，但这是退出语义变化。覆盖跨 batch 累计、候选错误、ReadDir 错误、namespace 替换与中断；保留 SweepLease 的并发资源控制。 |
| C07 | Python 的当前名称是 unpack_resource_result；meta 仅包装当前类字典，未证明所有继承/自定义适配返回同一形状。relay 缺 namespace 只允许匹配默认 context，已有真实缺省域用途。 | 注册协议封闭后再收敛 shape/direction，不默默冻结动态方法。relay 可用明确 enum 表达，但保留缺省/default、custom-root、metadata 冲突/部分缺失、remote anchor、HTTP-only、handshake/合同及禁止失败 endpoint 重放。 |

## 建议的落地顺序与证据要求

第一批只纳入有具体替代或纯局部不变量的项目：R27 的死计数；R28 的 Python 工厂重复校验与明确 validated 对象的移动；R29 的 release.reference；R31 的私有状态配对；R33/R37 的确切孤儿与退出脚本；R36 的不可达尾部、空循环和紧邻重验；满足 readiness 前提的 R38。每批保持外部输入/错误、资源释放和可观察状态，运行受影响现有测试。公开字段、错误常量、Wire codec、平台能力不随同删除。

所有权和状态修复单独提交：R01–R07、R17/R18 等应以明确反例及原有保障验收。RegistrationPlan、配置来源选择、chunk capacity、公开构造/ABI 退役、GC 维护和轮询通知属于独立设计改造；不能夹在“纯瘦身”里静默改变错误顺序、默认值、HTTP/IPC 路径或清理进度。

测试迁移采取先建立替代证据、再删除旧证明的顺序。关键行为集合包括：late route 与真实 idle reconnect；同名 route/owner 替换；单路由失败不损坏共享在途调用；完整合同/token 校验；借用失效先于租约释放；可变 Python exporter 快照；预算拒绝/退款/恢复准入；启动与 drain/restart；Unix/Windows endpoint 和 ABI；明确 HTTP 基准路径；发布 provenance 和已发布版本行为。较弱旧测试可以被更强用例合并，但不以“fake”“源码检查”或“没有 assert”标签直接判死。

本轮交付的是经历史反证校准的精简边界。尚未实施的候选，其“简化后无回归”应由上述行为验证、必要的受控竞争实验和改后差异来建立；当前没有据此宣称性能提升或改后测试通过。
