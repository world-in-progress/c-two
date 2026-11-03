# 过度防御复审

日期：2026-10-31。输入：upstream main `606b8f13`。范围：Rust core 全部 crate、Python SDK 与 PyO3 层、`c3` CLI、Rust SDK、Node 绑定与 codegen 的 TypeScript 资产；发布工具刚在[发版敏捷性审查](2026-10-30-release-agility-review.md)中审过，本次不重复。状态：只是审查，没有改动代码；实施哪些批次、按什么顺序，待用户决定。

R01–R39 出自 [2026-10-13 的防御性编程审查](../acceptance/2026-10-13-defensive-programming-review.md)，此后的修复和"保留"结论多由同一类实现方写成，其中不少修复本身又加了新的防御层。本次按用户要求，在继续挑选新批次之前，回头逐项复查现有实现里有没有防范实际不会发生的情形。

## 方法与判断依据

按代码区域分 8 路只读审查（内存、wire 与服务端、IPC 客户端、本地端点、Core runtime、HTTP relay、Python SDK、配置/codegen/CLI）。每条发现要说明：它防的场景在 C-Two 的实际使用中能否发生，相关不变量在哪里已经成立，判断错了会是直接失败还是静默损坏。Host 读代码核对真缺陷和大块删除，并用脚本复现了 B8。对最难裁决的 12 条并发与生命周期机制，另请一次独立审查专门寻找"实际能发生的场景"，再由 Host 按下面的依据裁决；它举出的场景不全采信。R07 的取舍另做失败实验。

判断依据来自 C-Two 的定位：同一用户的协作进程之间不设防，共享内存不是对付恶意同用户进程的安全边界；真实边界是来自其他进程或网络的数据、用户与 API 输入、崩溃或旧版本留下的文件与端点状态、操作系统错误与真实的系统行为、跨用户隔离、任意时刻的进程死亡、进程内真实的并发、公开 API 承诺的截止时间。边界上校验一次，边界之内相信不变量、出错就直接失败；只兜实际发生过的情形。

## 结论

- 找到 16 处真缺陷：不是防得太多，而是边界上少了处理，或者设了不该有的上限。B8 已复现：不走 SHM 时，超过 64 MiB 的分片请求失败；超过 4 GiB 的载荷在任何情况下都失败。新增的 Linux tmpfiles 问题会让长期运行的服务所在的端点命名空间失效，影响最大。
- 可删的过度防御与死代码按审查者的静态估计，生产代码约 8 千行、测试约 1 万行，最大的几块是 SyncClient 预分配 API、IPC 非阻塞结算层、分片排序闸门、relay 路由历史与围栏、无生产者的路由目录协议。
- 账本里有一批"保留/暂缓"结论不成立：R26 的 relay journal、R34 的"预留协议"、R25 保留的"服务端 sink"与"Rust 预分配"、B7a 坚持的两层重试，见第七节。
- 有 3 处需要先改结构才能删（Core 元数据锁的截止时间、IPC 结算层、分片排序）。R07（释放失败后的重试）由用户采用的单一属主方案取代，见第四节。

核对标记：**已复现** 指 Host 写脚本实际跑出结果；**已核对** 指 Host 读了相关代码或用 `rg` 确认了调用关系；**审查结论** 指只读审查给出、Host 尚未逐行复核，实施时由 Host 验收再核。

## 一、真缺陷（先修）

| ID | 现象 | 核对 | 建议 |
| --- | --- | --- | --- |
| B8 | 接收端把单个请求的分片上限取成全局在途分片数（默认 512），128 KiB 一片即 64 MiB 封顶，发送端的上限却是 65535 片。超过 4 GiB 的载荷必须分片，因此永远失败；Windows 跨会话、或 SHM 分配失败退回分片时，64 MiB 以上失败。错误报成 ProtocolViolation | 已复现：强制分片时 60 MiB 成功，100 MiB 报 `total_chunks 801 exceeds limit 512` | 单请求只受 max_payload_size 与 max_reassembly_bytes 约束，删去按片数的单请求上限；尺寸超限报尺寸错误 |
| D-tmp | Linux 默认端点命名空间在 `/tmp/c2-v3-<uid>`。systemd-tmpfiles（上游 `tmp.conf` 为 10 天，RHEL 默认启用，Debian 13 新装启用）会清理时间戳老化的文件，但跳过加了 BSD flock 的文件和目录。listener 存活期间只锁住自己的 lease，gate 的锁在每次操作后就释放，于是闲置超过 10 天时 `.gate`、`.gate.marker` 可能被删，之后 bind 报 CoordinatorMissing 或 InitializationIncomplete，close 与 reap 也拒绝，没有自动恢复 | 审查结论 + 第二审查独立确认；未在 Linux 上复现 | 先在 Linux 上复现；修法是 listener 存活期间对端点命名空间目录持有一把共享 flock（tmpfiles 对加锁目录跳过整棵子树） |
| D-drop | Drop 遇到 gate 争用时按设计放弃清理：8 路并行 Drop 256 对留下 50 组 socket/lease。随机地址的端点不会被同地址重绑回收，只能手动 sweep；历史上清出过 261 个与 19 对 | 已核对（账本 B16 的实测记录） | Drop 与 close 用同一个有界等待，或把 Drop 的收尾交给短期后台任务 |
| D4 | INIT_SCAN_LIMIT=64：自定义 root 里先有 64 个以上无关文件时，首次初始化每次 bind 都失败（`unix_managed.rs:716`） | 已核对；现有测试正断言这一行为 | 扫描到底 |
| A8 | pool_segment_size 落在 (2³¹, 2³²) 时，向上取 2 的幂超出 u32，buddy 层被静默关闭（`pool.rs:1196` 返回 0），c2-config 却接受该值 | 已核对 | c2-config 一处拒绝，删去"返回 0 即不建 buddy"的降级 |
| A9 | dedicated 段编号是永不复用的 u16，用满后该池不再分配 dedicated 段 | 已核对 | 按槽位代际（与 buddy 相同），并入第五节的协议批 |
| E-host | 低层 Host 关停会永久关闭整个 Runtime 的有截止时间的调用域，并断开该 Runtime 的全部对外 IPC 连接（`session.rs:1966`、`2367`），与"低层 Host 可重启"的承诺矛盾；只有 Rust API 走得到，sdk/rust 尚未发布 | 审查结论 + 第二审查独立确认 | 普通 Host 关停只收尾这个 Host；关闭调用域与断开客户端只属于显式的 Runtime 退休 |
| H3 | `C2_RELAY_ROUTE_MAX_ATTEMPTS` 被 `clamp(1, 2)`（`session.rs:1306`），文档的"默认 3、范围 1–32"不成立 | 已核对 | 删去该配置项，Core 固定"一次观察加至多一次刷新"，同步文档 |
| H10 | `c3 registry` 把缺字段或格式错的响应显示成"No routes registered."，peers 显示 "?"（R19） | 已核对 | 按类型反序列化，格式错就报错 |
| H11 | TS 资产照抄一份错误名表来核对 NACK 的冗余 name 字段，c2-error 新增错误码后会被拒为"非法 NACK 信封"，Rust 侧则降级为 Unknown | 已核对 | 只校验信封形状 |
| H12 | max_execution_workers 显式值硬上限 64，128 核机器无法配置 | 已核对 | 保留默认值的上限，去掉显式值的上限 |
| G4 | relay 连接路径把任何非语义异常改标成 `RegistryUnavailable: Relay unavailable`，例如空路由名 | 已核对 | 只映射语义错误，其余原样抛出 |
| B10 | 格式错误的调用控制帧只记一条 warn 就丢弃，不回复，对端一直等到超时 | 已核对 | 进程边界上的畸形输入直接断开连接 |
| C8 | 格式错误的 shutdown ACK 被当成"未确认"；Core 控制面把格式错误的地址变成 `Ok(false)`（E13） | 已核对（C8）/审查结论（E13） | 解析失败即报错 |
| F5c | mesh join 只拒绝更新的协议版本，接受更旧的 | 已核对 | 各处统一用相等比较 |
| F6 | HTTP `/_register` 对任何连接错误在 500 ms 内每 10 ms 重试，包括 AGENTS.md 规定为终止、不得重试的版本不匹配；CLI `--upstream` 路径另有一套注册与重试 | 已核对 | 两条注册路径合一，只对传输错误重试 |
| D9/F9 | 端点 sweep 地址上限 4096、relay owner 上限 1024，均无依据 | 审查结论 | 删去 |

## 二、死代码与没有使用者的表面

| ID | 内容 | 核对 | 粗估 |
| --- | --- | --- | --- |
| C1 | SyncClient 预分配 API、注入池与无池构造器：为已退役的 Python call_prepared 而建，R25 留下"Rust 预分配"却没有使用者；sdk/rust 未发布，且其测试断言无法导入 c2_ipc | 已核对（`call_bound_prealloc`、`pool_alloc_and_write` 无生产调用） | 生产 450–550，测试 1000+ |
| B1 | RouteWatch 流、RouteContract 查询、从未有发送方的 RouteAck、NACK nonce，以及恒定字段 owner_epoch、lease_deadline_ms（R34 以"预留协议"保留） | 已核对（RouteWatch 无发送方） | 生产 ~420，测试 ~200；协议变更 |
| F1 | relay 路由事件历史、watch 与 compaction（R26 以"将来的 watch"保留），没有读取方 | 已核对（`watch_from` 只有定义） | 生产 ~165，测试 ~190 |
| F7 | `/_admin/unregister`、RelayAwareHttpClient 的 call_phased、call_controlled、resolve_* 等无调用者；unregister 的 202/204/404 区分无人读 | 已核对（部分） | 生产 ~300 |
| G1 | 服务端 write-plan sink（R25 保留"服务端 sink"却无生产者；分发只返回 bytes） | 审查结论（审查者探针确认） | 生产 ~150，测试 ~90 |
| C3 | IPC 端 route-watch 恢复分支、dirty flag、ensure_route_contract | 审查结论 | 生产 ~130 |
| A3 | lease tracker 的 Transient 维度从未构造，leases 与 holds 两套计数恒等 | 已核对 | 生产 ~90，测试 ~40 |
| G6 | PyO3 层 R23 留下的 relay_anchor_address、route_names 参数与零调用方法 | 审查结论 | Rust 生产 ~90 |
| A2 | FreeResult 与 c2-server 的延迟 peer GC：free_at 已自己丢弃 peer 视图 | 审查结论 | 生产 ~45 |
| E3/E6/E7 | Core 的 delivered 一次性交接、只服务测试的 scope 重建回退、从未构造的错误变体 RelayDuplicateRoute 与 RelayHttp、恒为 false 的 http_clients_drained 与可推导的 ipc_clients_abnormally_closed | 已核对（E7） | 生产 ~60 |
| H2/H5 | c2-config 的完整 resolver 只有测试使用；contract 准入的可配置限额只被默认包装调用 | 已核对 | 生产 ~250 |
| G-slot | Python 注销时"未排空就保留槽位"的分支走不到：Core 的 unregister 阻塞到排空，close 恒为成功（`session.rs:1857`） | 已核对（探针加读代码） | 小 |
| 零碎 | A12、C8、B10、F10、G10、H13 各区的死 API 与不可达分支 | 审查结论 | 合计生产 ~400 |

## 三、重复校验

同一事实在多层再验，删去后由边界上的那一次校验负责。按区：

- 内存：A1，R01 的活记录已证明释放身份，位图扫描与按尺寸推 level 冗余，第二审查独立确认找不到反例；A4，同一条块规则验 5 层；A5，TS 未用的包装层，以及对自家原生输出的再验；A6，handle 的构造不变量每次访问再证 3–4 次；A11，预算上限存两份。
- wire 与服务端：B3，目录与编码器再验已在线边界验过的值，热路径每次调用克隆 7 个字符串；B6，Dispatcher 与 RouteCatalog 双份路由真相；B9，单一受信调用方的跨 server 身份检查。
- IPC 客户端：C4，Core 已冻结的配置在池里再比 16 个字段；C5，路由获取的外层重试与重复的合同校验；C6，对不会争用的锁做截止时间等待；C7，close 对自己的槽位 try_lock。
- 本地端点：D1、D2、D5、D6，gate 内对目录与条目的反复复查（见第四节 D 的前提）；D10，reap 的输入本已在凭据里。
- Core：E12，owner 接管每次提交复查文件系统 pin 约 5 次；E13，零碎重复。
- relay：F4，同一静态字段每条路径验 2–4 次；F8，客户端对 relay 已按完整合同过滤的结果再过滤并重取；F10，零碎。
- Python：G5，每次分发再验注册不变量；G9，Python 与 Rust 各验一遍同一规则。
- 配置、codegen 与 CLI：H1，生成的解码器重查 open 已固定的 spec 摘要（含 d7a74d9a 的清理告警路径）；H4，codegen 再验 FastDB Core 与自己的输出；H6，ServiceDefinition 同时收 release 与它自己的引用再交叉核对；H7，同一配置值验 2–5 次，f64 秒转 Duration 有五种写法；H8，ExpectedRouteContract 字段公开导致每层再验（C01 的另一种解法）；H9，CLI 读自己的凭据文件时防 FIFO 与路径替换。

## 四、并发与生命周期机制

| 项 | 审查结论 | 第二审查 | Host 裁决 |
| --- | --- | --- | --- |
| E1 Core 元数据锁的调用方截止时间 | 锁都很短，唯一实际失败正是这条规则造成的，改普通锁 | 部分反对：持有 RuntimeState 时会等池锁（`session.rs:581` 调 `prepare_seal`），注册回滚持锁调用 `block_on(unregister_route)` | 反驳成立（已核对）。先把慢等待移出 RuntimeState 的临界区，再对纯元数据锁用普通锁；截止时间只留在真正可能很长的等待上 |
| E2 每次操作新建 Tokio runtime | 改共享执行器，删去各处构建失败分支 | 同意；保留一次可失败的初始化 | 同意 |
| E4 retirement 的 pending 升级路径 | 新的退休事务等待原事务后走历史路径 | 同意，等待须在驱动线程内 | 同意 |
| C2 dedicated 请求退休执行器 | MemPool 自身已保留到 read_done 或崩溃超时 | 反对：已取消且客户端已关闭的请求，若服务端尚未打开映射，池被丢弃会使服务端打开失败 | 不采纳反对：那个调用没有人在等，buddy 块在同样情形下本来就这样处理（`detach_owner_pools`）。删去执行器 |
| C-U1 IPC 非阻塞结算与"未确认关闭"层 | 把 memcpy 移出池锁，改阻塞释放，约千行 | 部分同意：池锁下还有懒映射、分配、GC 等慢操作，只移 memcpy 不够 | 采纳：清理交给单一负责方、用阻塞锁，有截止时间的调用方等完成信号；删去轮询、try_lock 与 UnconfirmedIdle |
| B2 分片排序闸门 | 首片准入搬回接收循环，删去排序闸门 | 部分同意：大请求在重组池读锁下拷贝，首片插入要写锁，搬回接收循环会堵住该连接 | 采纳：先缩小重组池锁的范围（拷贝不持池锁），再搬回接收循环、删闸门 |
| F2 relay watch 闸门与 50 ms 轮询 | 每次调用已有实时查找，闸门重复；轮询每秒每路由 20 次 | 同意删闸门；轮询负责清理闲置的过期路由 | 删闸门；轮询保留但降到秒级，不做成配置 |
| F3 relay owner 替换围栏 | 删去死字段、重复分支与 12 字段比较 | 部分同意：epoch 与可替换状态必须保留，否则 A 在 B 探测后恢复会被替换，后续调用静默落到另一个有状态资源 | 采纳：删 owner_lease_deadline、owner_generation、重复的 evidence 分支，把 12 字段比较收成一次身份比较；保留 epoch、可替换状态、活动请求检查与最终探测 |
| A1 buddy 释放的位图扫描 | 冗余 | 同意，找不到反例 | 删 |
| D 端点命名空间复查 | 打开后只验一次，其余删去 | 部分同意：跨等待与跨 listener 寿命的复查能发现真实的外部修改者（tmpfiles）；写后立刻回读没有依据 | 先修 D-tmp（目录 flock）；之后复查只留在所有权、发布与 unlink 边界 |
| R07 释放失败后保留以便重试 | 生产中只有分配器锁会让释放失败 | 反对立即放弃：持锁进程被调试器或调度器挂起属于真实的瞬时争用，重试是文档承诺 | 见下文的失败实验；由单一属主方案取代 |

**R07 失败实验。** 在 macOS 上用当前代码的 Rust、Core 与 IPC 接口做了四组实验，没有改产品代码；Python 层、其他平台、调度器与虚拟机场景没有实测。结果：分配器锁被一个已挂起但活着的进程持有时，释放自旋约 1.5 秒（1.465–1.604 s）后失败，清理义务被保留；持锁进程恢复后，显式重试 10 µs 内成功。走最终 Drop 放弃时，计一次 abandoned_releases，那块内存一直占在服务端的池里，直到服务端进程退出。在持续负载下随机挂起服务端 240 次（服务端持锁时间只占 0.22%），以及用 40 个忙进程超订 CPU 180 秒，分别跑了 284 万次和 121 万次释放，都是 0 次失败。

下游能触发的只有一种情形：持锁的那一刻被挂起超过 1.5 秒，同时另一个进程正在往同一段里释放。调试器断点恰好停在分配器的临界区里会确定地触发，属于开发者调试；终端 Ctrl+Z、SIGSTOP 或调度器挂起单个作业，只有约 0.2% 的概率正好落在持锁时间里；虚拟机暂停、笔记本睡眠以及同一作业整体挂起，会让两端一起停下，不会触发；CPU 超订实测不触发。结论：这条路径真实存在，但只在开发调试或罕见的单进程挂起中出现，不是常规负载下的现象。

可选三种做法：一是维持现状（保留失败的释放，等调用方显式重试）；二是立即放弃并计数（最简单，代价是每次发生都占掉服务端池里的一块，直到服务端重启）；三是按持锁者的状态区分（锁的持有者还活着就一直等，已经死了才放弃并计数）。第三种既不泄漏，也能删掉"保留以便重试"的整套状态与公开语义。若在调用方原地等待，持锁者被挂起期间，Drop、`__del__`、解释器退出以及 relay 的 tokio 工作线程都会卡住，Ctrl+C 也打断不了；因此 Host 建议的改进版是：先原地尝试一次，遇到争用就交给后台的单一清理负责方（与 C-U1 共用同一机制），由它等到释放成功或判定持锁者已死。业界做法与更彻底的单一属主方向见[跨进程释放调研](../research/2026-10-31-cross-process-release-survey.md)；用户要求先评估单一属主方案，因为一旦采用，R07、A7 与 C-U1 的部分改造都会失去意义。用户 2026-10-31 同意采用单一属主方案，上面三种做法都不再实施；评估与待确认事项见[单一属主释放方案评估](2026-10-31-single-owner-release-evaluation.md)。

## 五、协议与格式（集中做一次）

B1 的无生产者消息与恒定字段；B4，route_revision 恒为 1，却随每帧、握手、relay header 与 mesh digest 传输；B7，shutdown ACK 的恒定字段、恒开的能力位、握手里的段列表；F5，mesh 消息里接收方会重算的 hash 与不参与排序的 removed_revision；A9，dedicated 槽位代际；若删 Windows 进程 pin（D7）还涉及 C ABI。合为一次握手版本（15）与一次 mesh 协议版本，Rust、Python、Node 同时改，按 AGENTS.md 用确切候选跑全平台门禁。

## 六、测试

钉源码文本、钉已删名字、假 owner 自证顺序的测试，随对应生产项一起删：B5、C8 的源码文本测试，E8，`test_sdk_boundary.py` 里约 104 处源码文本断言，H14。保留真实行为覆盖。

## 七、对账本保留结论的修正

| 账本结论 | 复审判断 |
| --- | --- |
| R26 relay journal 保留（"将来的 watch/list history 尚未退役"） | 不成立：没有读取方（F1）。删除 |
| R34 RouteAck 等"尚未退役的公开预留协议" | 不成立：RouteAck 从未有发送方，RouteWatch 与 RouteContract 已无客户端（B1）。随协议批删除 |
| R25 保留"服务端 sink、Rust 预分配及适配边界" | 不成立：sink 无生产者（G1），预分配无使用者（C1），duck-typed 客户端只服务测试替身（G2，需用户确认是否算扩展点） |
| B7a"两层重试不得合成一层" | 不成立：外层只覆盖查找结果经共享目录往返造成的窗口（C5） |
| C03 保留分片接纳的代际与围栏 | 有条件成立：在首片准入异步化的现有设计里必要，缩小池锁范围后可删（B2） |
| C01 因行数暂缓 | 改用另一种解法：校验过的构造器加私有字段，删去各层再验（H8） |
| R18 的 1024 owner 上限与容量恢复 | 上限无依据（F9） |
| R20 示例加载失败后的覆盖 | 可核销：被守护的加载与保存代码已随 `eee0fe57` 删除 |

## 八、建议批次

1. 真缺陷：第一节中不涉及协议的各项（B8、D-drop、D4、A8、H3、H10、H11、H12、G4、B10、C8、F5c、F6、D9、F9），按区域拆成 2–3 个微任务。
2. tmpfiles：先在 Linux 上复现，再实施目录 flock。
3. 死代码：第二节按区，机械删除。
4. 重复校验：第三节按区。
5. 结构调整（需要先出方案）：Core 生命周期（E1、E2、E4、E-host）、IPC 结算（C-U1、C2）、分片与池锁（B2）、relay（F2、F3、F6）。
6. 协议批：第五节。
7. 测试：随各批删除。

需要用户决定：relay 轮询的间隔；`/_admin/unregister` 是否有人手动使用（F7）；`crm.client = <对象>` 与 `HeldResult(release_cb=…)` 是否算扩展点（G2）；`c_two.config.settings` 的直接写入是否是受支持的用法（G8）。

## 附：各区未裁决的待议项

逐项实施时再定，不单独开批：A，死持有者段每次分配重复自旋、有限预算默认值是否属于"按通常情况设上限"、owner 与 peer 池是否拆成两个类型；B，按 ResponsePool 的方式封住池锁后删除每次访问的校验、接管窗口内的准入代际、握手路由表与 64 路由上限、执行调度器与 tokio 线程数两个上限；C，异常关闭诊断字段是否有人读、握手期间的瞬时连接重试是否实际出现过；D，gate 本身是否必要（与 D-tmp 一起看）、owner 票据的上下文摘要、sweep 是否需要分批；E，teardown 未完成时阻止 Host 重启的围栏、owner 命令每次复查 pin、三处带修订号的锁外解析；F，10 ms 清理轮询、每次调用前的实时查找；G，start 之后 set_server 的两种处理、relay 不可达时的错误归属、仅测试使用的 PyO3 表面；H，被代码覆盖值遮住的环境变量是否该报错、合同描述符的有界 JSON 解析器、错误 wire 的冗余 name 字段、TS 传输的注入接缝、owner 缺席宽限 60 s、FastDB 同摘要不同 manifest。
