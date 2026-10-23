# C-Two 局部精简批次记录与收束

## 纯瘦身收束与当前剩余清单（2026-10-23 同步）

用户确认收束纯瘦身，停止按减行数继续挑批次。当时最后一个纯瘦身实现提交为 `6afec0f3`（R25 三个 Python prepared-call 入口退役），验收记录提交为 `3382b3ba`。收束时的筛选基于 `3382b3ba`，当时没有新的生产代码或测试修改；下表中的行数是静态方案估算，不是已实现的净减量。后续 R01、R02、R03、R16、R18 正确性修复单列记录，不计作纯瘦身收益。既有验证仍只绑定各批实现提交。

本节按 C-Two 0.8.2 / c3 0.4.1 的发行来源 `2aa60fe0` 同步；R16、旧 ShmBuffer 退役及 R18 已随本次发布交付，见[发布验收](../reports/2026-10-23-082-client-observation-release-acceptance.md)。后文 2026-10-14 的收束表和 B1–B19 记录保留各阶段的历史事实。完成的子项不代表同编号下所有候选已经完成。C03 与 R13 的 SHM prepare/send 子项已随 0.8.0 交付；R01、R02、R03 的独立验收分别见 [R01 记录](2026-10-16-r01-allocation-release-evaluation.md)、[R02 记录](../reports/2026-10-21-r02-unique-write-ownership-acceptance.md)及下文。R06/R07 未随这些批次扩展实施。

原清单 39 个 R 条目的分类如下。R04 已由准入竞态修复 B 及所有者接管验收覆盖；R13 完成了批准的子项，仍归入部分完成。R16 已按修订方案完成需求驱动读取与分片收发/终态处理。C01–C07 设计候选及 T01–T07 测试建议与 R 条目交叉，不重复计入总数；其中 C03、C05、C07 已交付批准的局部子项。bind/connect 系统原因保留不等同于 R17 的 inspect/reap 修复；C03 也未完成 R14/R15 的其他范围。R18 的 observer 容量恢复与状态代际修复已单独验收。当前数量为 14 / 13 / 11 / 1。

| 状态 | 数量 | 条目 |
| --- | ---: | --- |
| 已完成，当前无剩余子项 | 14 | R01、R02、R03、R04、R08、R11、R16、R18、R23、R24、R27、R30、R31、R37 |
| 已完成部分子项 | 13 | R12、R13、R14、R22、R25、R28、R29、R32、R33、R34、R36、R38、R39 |
| 尚未单独验收，待复核或实施 | 11 | R05–R07、R09、R10、R15、R17、R19–R21、R35 |
| 明确保留 | 1 | R26 |

R13 已交付子项生产净减 104 行、测试净增 59 行，合计净减 45 行；C03 已验收的生产与测试合计净减 155 行不重复累计。Windows ARM64 支持、安全依赖修复、发布校验修复及文档属于各自交付范围；原增减和发行门禁见 [0.8.0 验收记录](../reports/2026-10-23-r13-080-release-acceptance.md)。R16 本批生产净减 25 行、测试净增 219 行，合计净增 194 行；完整门禁、性能与清理见 [R16 验收记录](../reports/2026-10-23-r16-node-exchange-acceptance.md)。其他暂缓项未随本批扩展。

2026-10-23：按单独确认的[方案](2026-10-23-orphan-shm-buffer-retirement.md)退役无构造调用者的旧 Python `ShmBuffer` 投影，接受低层 Python 导出删除。候选 `fa62f7a3` 生产净减 338 行、测试净增 2 行，合计净减 336 行；完整 relay、真实跨语言、三系统、GitHub Actions 10/10（含 Windows ARM64 与 3.14t）、端点零残留及独立审查完成，见[验收记录](../reports/2026-10-23-shm-buffer-retirement.md)。本批已随 0.8.2 发布，活跃 buffer 生产实现与基线相同。R06 导出/释放同步和 R07 失败释放仍独立待办，原 39 项状态数量不变；两份控制会话重构继续暂缓。

### 继续暂缓、不单独开批的项目

下表保留原评估的理由与行数估算，未按 0.8.0 重新测算；重启时须重新核实调用链、原设计理由和生产代码与测试的整体净收益，再确认方案。

| 项目 | 当前结论与暂缓理由 | 重新启动条件 |
| --- | --- | --- |
| C01 · 私有不可变合同 | 同一本地值的语法重验可删约 18–25 行；受检构造器、六个访问接口和调用点迁移预计使生产代码净增约 20–40 行，测试还需迁移。远端合同、live token、UID/revision、实例与重连校验仍有职责。 | 有独立的公开 Rust 接口设计目标；不能为局部去重改公开字段。 |
| C02 · 提前 RequestLease | 显式释放保证请求 backing 先于 route/execution permit 归还，chunk 错误路径还须在等待回复前释放。提前包装仍需显式释放和 callback 所有权移交，净减接近零，取消测试增加。 | 单独证明具体取消泄漏或所有权缺陷，再按正确性修复设计；保留跨 await 的 Catalog/Dispatcher 身份检查。 |
| R14 · handle 只读校验复用 | 私有只读指针 helper 预计使生产减 40–55 行；补齐无效空 handle、allocation 长度、过期世代与错误顺序后，整体预计只减 0–35 行。原始指针借用边界也增加审查负担。 | 与有证据的所有权修复自然合并时再评估；不单独引入 CheckedSpan、缓存或锁证明框架。 |
| C07 · Python direction/结果形状 | 服务端假 CRM 实例与双向 wrapper 的重新设计需要明确 callable 协议，并处理公开 direction、直接调用、自定义带 binding 方法及测试迁移。整体净收益未证明。普通未包装继承方法不满足 descriptor binding 接纳，不能误称已有正式可运行能力。 | 先决定公开 Python 调用约定；不混入已完成的 HTTP namespace 子项，也不重新把任意自定义编码器作为正式 portable 能力。 |
| R21 · Resource 模板链 | 生成器仍输出装饰并继承 CRM 的旧模型，AST fallback 读取未保存的 ast_node。整链可毛删约 613 行，但会撤销公开模板生成能力；现有 c2-codegen 不提供等价 Resource skeleton。 | 单独决定修复工具或退役功能；不能把工具层删除记为生产运行时收益。 |

该轮筛选已做调用链、引入历史和独立原生只读复核，只形成暂缓结论，没有形成这些项目的候选实现或性能收益证据。

### 条目边界与剩余范围

| 项目 | 剩余范围及处理边界 |
| --- | --- |
| R01 | 精确释放身份已实施并完成本机、跨平台和独立审查验收；采用 SHM 3 / IPC 12 / C ABI 5，不留旧格式兼容。safe Rust 读取/唯一写别名已由 R02 修复；失败释放 owner 仍属 R07。 |
| R03 | shutdown 发送阶段修复已验收：发送前失败可重试，开始控制写后丢确认不再重放；首次端点不存在的处理、ping、原总预算和 ACK 仅 initiate 的语义保持。 |
| R02 | 唯一写所有权已按完整边界验收：真实分配 owner、接收/发布载体与原 pool 绑定，raw 访问明确 unsafe，封闭可替换 pool 及只读 guard 还原写锁入口。低层 Rust API 无兼容迁移，SHM 3 / IPC 12 / C ABI 5 保持；不替代 R06/R07。 |
| R04 | 已完成：token 绑定 server instance、UID/revision 与准入代次，关门使旧 token 失效，接管恢复不能复活；同名替换及 owner_missing 后迟到 relay final 的反例有独立覆盖。见[准入修复验收](../reports/2026-10-21-admission-race-repair.md)及[三系统复核](../reports/2026-10-22-release-foundation-review.md)。 |
| R05–R07、R17 | SHM 布局接纳、导出/释放同步、失败释放的 owner/charge、端点 inspect/reap I/O 分类，按各自反例修复。R01 已承接 SHM 3 布局安全接纳所需的边界；R05 须据新布局重新复核，不能直接套用旧反例或视为整项完成。 |
| R09、R10 | relay 配置快照可能改变读取/错误观察时点；注册快照局部去重约 20 行，独立 borrowed 校验、动态 Resource 方法查找及错误顺序有真实职责。继续暂缓。R08 原环境拒绝语义的解析收敛和 R30 已批准子项已经完成。 |
| R12、R28 剩余 | 已完成可证明的同值重验删除、登记事实与 validated relay 表示收敛；跨 await 的 authority、owner/locality、代际、peer 活性和错误接纳继续保留。新增删除需另证局部不变量。 |
| R13 剩余 | 请求所有权移动、IPC 拼帧及 SHM prepare/send 合并均已验收。响应快照与初始转换仍有数据寿命职责；写锁前取消保留未发布分配直到 pool 收尾已在基线复现，本批保持该行为，须作为独立所有权/生命周期问题评估。不得把本子项交付计作 R13 整项完成。 |
| R14 其他、R15、C03、C04 | C03 的 chunk assembly/pending permit 单一所有权子项已完成，代际与准入围栏保留。bitmap 锁证明、GC admission 进度及轮询改通知仍暂缓。不为精简删除跨进程锁、回收进度、预算与唤醒保护。 |
| R16 | 已按[修订方案](2026-10-23-r16-node-backpressure.md)验收：分片上传同步推进接收，失败关闭并汇合，成功保持连接复用，空闲读取使用原生背压；低层 TS close/pause/resume 契约已同步，不留兼容。生产净减 25、测试净增 219，属于正确性修复。完整 Node→Core 投影迁移需另立方案，不作为本批交付内容。 |
| R18 | 已按正确性修复验收并随 c3 0.4.1 发布：容量拒绝复用原 retry，永久关闭才退出；status 与任务共锁、按原 token 更新，保留不可用记录的独立寿命、1024 owner 上限、代际及释放顺序。生产 +49、测试 +345，不计作瘦身收益。见[验收记录](../reports/2026-10-23-082-client-observation-release-acceptance.md)。 |
| R19、R20 | CLI 畸形成功响应和示例加载失败后的覆盖行为，属于错误/数据保护修复，不作为纯瘦身执行。 |
| R22 剩余 | benchmark selector、旧 sweep 和测量标签仅为工具整理。已完成的 SDK 登记/撤回与强制 HTTP 实跑保持。 |
| R25 剩余 | 三个旧 call_prepared 入口已退役；FastDB 正式路径、Python-only pickle、Core bytes、服务端 sink、Rust 预分配及当前 client/scheduler 适配边界保留。新的协议收窄需独立决定。R23 bridge、R24 旧 buffer 参数链已完成。 |
| R26 | relay journal 明确保留；旧快照及删除原因有独有信息，未来 watch/list history 尚未退役，也没有支持删除的成本测量。 |
| R29、R32、R33 剩余 | codegen 同次缓存/typed projection、结构化错误与凭据内部序列化、Python 错误常量及孤立 wrap_error。0.8.2 的公开 CoreError 是现有错误链投影，不代表 R32 内部序列化收敛已经完成。R32 凭据子项的[单一原生投影方案](2026-10-23-r32-native-endpoint-credential.md)已确认，实施后正在验收；预计生产与测试合计净减 35–55 行。生成 release 复用、call-control 写入去重和 Rust 错误码单一声明已完成；其余转换子项净收益仍不足，Transport 内外层错误、外部 JSON/wire 与错误顺序保留。 |
| R34、R35 | peer SHM 私有副本已退出；RouteAck 是尚未退役的公开预留协议，禁用 heartbeat 的任务优化净收益未证实。C05 ABI 4 完整端点能力子项已完成，不借机删除预分配能力。 |
| R36、R38、R39、C06 剩余 | 已完成的死状态、不可达尾部和 readiness 后等待不再重开；真实等待、维护 EOF/错误退出、操作错误映射和外部 chunk 配置接纳仍有职责。私有 proof 或专属结果类型须先证明整体收益。 |
| T01–T07 剩余 | 源码词串门禁、假 owner、阻塞读取、重连/GC 负例及默认值证明，随对应正确性事项替换；不为测试行数牺牲行为覆盖。已迁移的真实生命周期、协议边界和手写 golden 不回退。 |
| 两份控制会话重构 | 继续暂缓。连接通知、所有者接管及非 Hello setup 期限修复的交付不等于批准会话层重构；后续仍先证明能整体消除重复收发与复杂状态，核实身份、准入、期限、错误顺序和释放语义，再确认方案。 |
| 已完成且无新剩余子项 | R11 登记事实、R27 FIFO 后的死计数、R30 已批准配置子项、R31 owner/冻结事实配对、R37 旧发布脚本等，维持各批验收状态。 |
| 本机历史目录 | 不清理、不重建、不冷切换，也不将未知路径消失认定为原生回收成功。新实验仅处理本次自己创建且可精确识别的资源。 |

后续破坏性 API/ABI/协议变更继续要求对确切实现提交运行 GitHub Actions 的适用 Linux、macOS、Windows x64/ARM64 门禁；协议变更不保留旧表示兼容。生产代码与测试一起核算净收益，每项完成针对性回归、完整 relay、适用跨语言验证、端点零残留和独立审查后再推进，验证后及时清理构建产物。已通过且不受影响的验证不重复运行。2026-10-16 的纯文档收束没有触发新 CI，也没有使用 hey-my-buddy；后续修复各自记录验证与审查。

2026-10-22：两份控制会话重构方案继续暂缓。Claude 提出的本地 bind/connect 结构化 OS 原因已按[验收记录](../reports/2026-10-22-local-io-cause.md)完成：实现候选 `57b1cab9`，真实三系统、完整 relay、跨语言及 [GitHub Actions 9/9 门禁](https://github.com/Dsssyc/c-two/actions/runs/39722345497)通过，端点验收与产物清理完成。该项与 R17 的 inspect/reap 错误折叠不是同一入口，不将其计作 R17 已完成，也不把诊断增量计作瘦身收益。

### R13 · SHM 响应准备与发送收敛（2026-10-23，验收及发布完成）

固定基线 `0f63d97e9ab951957490108de83b17b81580578d`，先补行为覆盖的快照为 `89b20a0fe6efc5345765096e435fd491d1600413`，最终实现为 `4cf80c707ed4cf605d0ce4a0ccb98ca27484c474`。普通字节回复复用既有 `try_prepare_shm_response` 和 `PreparedShmResponse`，删除第二套 buddy 分配、复制、元数据与写失败释放链。prepared 发送分支抽为一个具体私有函数；未增加框架、状态机、后台任务或兼容入口。

用户接受低层 Rust 返回类型改为 `Result<Option<PreparedShmResponse>, String>`、普通回复采用既有 `free_at` 的 GC 空闲记账时点和统一诊断文案。max_payload 先于错池检查，分配失败仅在写前回退，写失败直接返回；原池身份、快照、预算、写锁及取消边界、buddy 交接与 dedicated 释放语义保留。ABI 5、IPC 12、SHM 3 与业务 wire 未改。

基线实际覆盖普通/预制、buddy/dedicated 写失败、非整幂长度与旁存分配、完整分块字节以及真实写锁前取消。候选将两个失败测试合并为四种组合，保留所有实质断言。基线已证实写锁前取消保留未发布分配直到 pool 收尾，候选保留该行为；其修复需要单独所有权方案，不扩入本批。

实际生产新增 49、删除 153，净减 104 行；测试新增 131、删除 72，净增 59 行；合计净减 45 行。没有吞吐或延迟提升声明。针对性回归、完整 relay、真实跨语言、端点零残留及独立审查均已完成；macOS/Linux/Windows x64 与新增 Windows ARM64 的完整源码门禁绑定发行源码 `96fe6583`，对应 [Source Validation 39790864318](https://github.com/world-in-progress/c-two/actions/runs/39790864318) 和 [Release Candidate 39799778643](https://github.com/world-in-progress/c-two/actions/runs/39799778643)。C-Two 0.8.0 / c3 0.4.0 已发布，36 个 PyPI 文件及 17 个 GitHub 附件与候选字节一致；完整结果见 [R13 与发布验收](../reports/2026-10-23-r13-080-release-acceptance.md)。发布门禁、ARM64 追加及文档增量单列，不并入上述 R13 净减量。

### C03 · chunk 重组与路由许可单一所有权（2026-10-22，完成）

原只读评估固定于 `862f6fa93a0b49f0a35a4492414e2d790c731140`，当时未修改生产代码和测试，也未构建。原链为：首块取得 route pending permit → registry 分配并登记 assembly → 写 route/method 副本 → 第二张表登记 route/permit → feed → 分别取两张表 → finish → execute。GC、断连和发布回滚必须分别收尾，GC 另扫描 pending 表与 registry 对账。

原设计有实际职责：`c8298c3f` 引入的 permit 使未拼完请求也占用 route 的 `max_pending`，可被关闭/排空策略观察；`f5c9aa33` 增加的代际 ID、发布回滚和准入围栏，保护取消、panic 和同 key 接替。当前 `c2-wire` 管理 backing，`c2-server` 管理路由，两张表来自分层后的所有权分离。本次要合并存储和移交，不撤掉这些职责。

最小替换范围：

- `c2-wire/src/chunk/registry.rs` 的既有 registry 条目携带类型参数 `C`；server 使用现有 `ChunkRouteAdmission` 的 route/method/permit，client 使用 `()`。一次插入发布 assembly 与上下文；插入拒绝把上下文交还调用方。wire 层不解释路由，不依赖 server，也不增加 hook、策略 trait、第二套管理器或后台任务。
- 复用现有移除及代际检查，取回 `(ChunkAssembler, C)` 后由调用方调用已有 `ChunkAssembler::finish()`。这使 finish 失败时调用方仍持有 permit，能保持跨错误回复 await 的寿命。按代际取回用于发布回滚；guard 保留，删除其双表协调与多余状态。
- `c2-server/src/server.rs` 删除 `chunk_route_pending`、专属锁、store/take/stale sweep、双发布失败处理及 route/method 对账。`c2-wire/src/assembler.rs` 删除 route/method 的可选副本，退役 `set_route_info`、`FinishedChunk` 旧表示；IPC reply 调用点直接取得 backing。
- 继续使用 `chunk_admission_gate` 的 Pending/Admitted/Refused/Aborted、原子 commit 与代际 ID；保留首块/后续块排序、跨 await 准入校验、GC 时机、预算、soft limit、trim 原容量计费及 backing 释放权。R15 admission 进度与 R07 失败释放问题不混入。

释放顺序是实施约束：普通 abort、GC、断连仍按 backing→permit；发布 guard 回滚仍按 permit→backing；insert 拒绝和 finish 失败仍持有 permit 直到错误回复完成或任务取消。上下文析构放在 shard/pool 锁外。`try_*` 清理失败须保留整个条目与 charge；成功移出的条目沿用已有锁外释放方式，不能直接在 `retain` 内析构上下文。

| 范围 | 原方案的替换与预计物理行净量（含注释） |
| --- | --- |
| 生产 | 双表辅助、发布/对账、回滚和 wire 元数据约 164 行可替换；回补上下文移交、错误返还、锁外析构和调用点约 50–90 行。保守预计净减 70–110 行。 |
| 测试 | server 的 `chunked_reassembly_via_registry`（50 行）与 `chunked_gc_sweep_releases_stale_route_pending_capacity`（60 行）合入已有 registry 字节/trim 测试和真实 timeout→容量恢复→再次调用测试。原断言职责保留；按代际回滚、持有 permit 等待错误回复、finish 错误和锁外析构补在现有用例。新增/迁移约 90–140 行，预计净减 20 至净增 30 行。 |
| 合计 | 生产与测试预计净减 40–130 行；主要收益是消除一张所有权表、一把锁、一轮 GC 对账及两份路由元数据。若实际完整覆盖使净收益不成立，重新评估范围，不削减断言凑减量。 |

需要明确接受的影响：低层公开 Rust `ChunkRegistry` 类型和插入/取回接口、`ChunkAssembler` 元数据字段及 `FinishedChunk` 导出改变，所有消费者同步迁移，不留旧入口；高层 Rust/Python SDK 签名、C/FFI ABI 5、IPC 12、SHM 3 和业务 wire 不变。GC 会在该条目移除收尾时归还其 permit，取消原来等待另一次 stale sweep 的滞后；半状态对账的补救错误分支随表示退役。外部帧/路由/容量校验顺序与对应错误保持。这里有清理可见时点变化，不能称为完全无行为变化。

主要回归风险为提前释放 permit、在锁内析构造成锁序问题、旧 guard 误删新代际，以及 reply 侧持有 backing 的计费/池寿命缩短。泛型有两个正式实例，可能增加编译代码量；不承诺吞吐提升。GC 对账与同 key 接替的风险目前仅静态分析，未动态复现，不作为已确认生产故障报告。

确认后的顺序：先在原实现补齐错误回复阻塞和既有释放顺序的行为证据，再迁移单一所有权并收敛重复测试；覆盖取消/panic/断连/timeout、同 key 新代际、buddy/inline/file backing、池锁竞争及 retained reply。完成完整 relay、真实跨语言、macOS/Linux/Windows（Windows 优先开发机）、端点零残留和独立审查；低层 Rust API 变更须对确切候选完成 GitHub Actions 适用跨平台门禁。验证后及时清理产物。

原方案经 hey-my-buddy 0.50.0 独立只读复核，保留释放顺序、GC 时序和测试成本约束。用户随后接受低层 Rust API 与 GC permit 归还时点变化，批准实施。

实施基线为 `ef7098b16d4ae205fcfd674a8d4669478a6da921`，最终实现及测试候选为 `9839951ece928585a3dc2cf08c6c3716ce15f9ec`。实际生产新增 179、删除 381，净减 202 行；测试新增 346、删除 299，净增 47 行；非文档合计净减 155 行。移除一张表、一把锁、GC 对账和重复路由元数据；没有新增管理器、后台任务或兼容入口。Python 手工 assembler 元数据留在既有 PyO3 owner 内，与 consumed 状态一起移动，高层接口保持。

先在基线补齐真实写锁阻塞下的 permit 寿命、池锁竞争下的普通 abort 与发布回滚释放顺序，并用提前归还 permit、反转回滚顺序两个变异证明断言有效。Python 元数据的 finish、失败 finish、abort 三条覆盖也先通过基线。候选复用并扩展真实 timeout/容量恢复、代际、预算和锁外析构测试，退役两个重复用例及一条要求旧补救分支存在的源码词串测试。

macOS 完整 Core 分段共 1805 项通过（5 项既有 ignored），含 relay 416，Rust SDK 19；Linux aarch64 运行时与完整 relay 1225 项通过（4 项既有 ignored）；Windows 开发机完整 Core 1668 项通过，其中缺少 MSVC 环境导致的 C 头文件测试在补齐环境后单独通过。macOS 与 Windows 的 assembler 19、真实跨语言 25 及严格 18 行矩阵均通过；Windows Python 按仓库标准使用 FastDB source 模式，Rust 消费者使用官方 SDK。未将缺少官方 Linux arm64 SDK 的本地容器记作跨语言通过。

[GitHub Actions 39734314720](https://github.com/Dsssyc/c-two/actions/runs/39734314720) 对确切候选完成 9/9 门禁：Linux、macOS、Windows 2022/2025 及 Python 3.14t；FastDB 来源为 `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。五个 source 任务均通过各自适用门禁与 Rust/Python 18 行、TypeScript 12 行矩阵。此前候选发现的 PyO3 消费者迁移遗漏及旧源码词串测试已修正后更换候选，未通过重跑相同候选或放宽断言验收。

三轮固定候选及差量的 buddy 独立审查无阻断，Host 已验收并回收审查工作树。Unix 隔离 socket/lease 为零，Windows 本批进程及 C2 命名管道为零；历史目录身份与内容未变。本机约 3.4 GiB、Windows 约 4.2 GiB 构建及临时产物已清理，Windows Rust 工具链保留。本地 HEAD、分支和标签未动，仅向 fork 验证分支推送 CI 候选；两份控制会话重构继续暂缓。

## R02 唯一写所有权（2026-10-21，完成）

基线 `a523967b`，实现候选 `2d1b1d1095f3ab6e4d229fb354262bfc25dd113e`。生产代码净增 204 行，测试净增 526 行；属于正确性修复。18 个漏洞反例先在基线被接纳，候选的 21 条编译探针及真实双客户端错池回归通过；本机完整 Core/relay 1716、Rust SDK 19、native 8，Python ordinary/portable/TypeScript 1231/25/14，以及 18/18 Rust/Python、12/12 TypeScript direct/relay 矩阵通过。

[Actions 39523167030](https://github.com/world-in-progress/c-two/actions/runs/39523167030) 对确切候选的 Linux、macOS、Windows 2022/2025、Python 3.14t 门禁全部通过。新版 buddy 多轮独立审查发现均已关闭，最终固定候选复核通过；本批 socket/lease 零残留，历史目录未动，可重建产物已清理。同环境分配微基准增加约 9.6–15.5 ns（4.7%–6.7%），只读 guard/重组中位数变化低于 1%；未声明 RPC 性能提升。完整来源、接口边界、布局开销、审查及实际增量见 [R02 验收记录](../reports/2026-10-21-r02-unique-write-ownership-acceptance.md)。

## R03 shutdown 发送阶段（2026-10-19，完成）

基线为 0.7.6 的 `fe43f20257947c466d8908d7e47d5a36ead35f99`，实现候选为 `f340d4ec4054a6319f4a0a9128e2b8004b193866`。直接 IPC admin shutdown 仅重试发送前失败；开始控制写后丢失确认，沿用现有未确认结果并停止重发。私有 exchange 区分 NotSent 与 DispatchUncertain，一个单调的 write_started 标记覆盖写失败、读失败和外层超时。ping、首次端点不存在的处理与权限错误顺序、原总预算、ACK 仅证明 initiate、公开 API/ABI、wire、身份校验与生命周期策略保持。

先在原生产实现补齐反例：9 项中 7 项通过，ACK EOF 与外层超时两项均失败于替换监听器收到 shutdown 重放；原生 close 完成后才执行行为断言。修复后相同测试模块 9/9 通过。正常 ACK 与发送前拒绝恢复复用原 responder，两个真实实例替换反例共用一个测试函数；ping 丢回复后重试保持，未新增测试框架。

相对 `fe43f202`，生产代码净增 14 行、测试净增 100 行、文档净增 22 行（含 AGENTS.md 清理规则），合计净增 136 行。按物理行统计，Rust test_seam、测试模块及 cfg(test) 连接拒绝注入计入测试。R03 是独立正确性修复，这些增量不计作瘦身收益。

本机完整 Core 1715 passed（含 relay 416）、Rust SDK 19；Python ordinary 1231、portable 25、TypeScript 14，收据为 18/18 Rust/Python 和 12/12 TypeScript direct/relay 行；Node 61 passed。本机 Python 5 项及 Node 1 项 Windows 专属用例由线上 Windows 覆盖。Core 的 4 个 ignored 是父测试调用的子进程夹具，另 1 个要求特权跨用户环境；未新增 skip/ignored。

[GitHub Actions 39131157512](https://github.com/world-in-progress/c-two/actions/runs/39131157512) 在 attempt 1 对确切候选 `f340d4ec` 完成 9/9 作业。五份源验证收据逐一绑定该 SHA 与 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，118 个适用步骤全部 passed：Linux/macOS 各 25、Windows 2022/2025 各 26、Python 3.14t 为 16。四个 full 作业均实际通过 R03 的 EOF、外层超时、发送前重试和 ping 回归；relay 在 Unix 为 416、Windows 为 411，差异仅为原有 5 个 cfg(unix) 用例。五个平台的跨语言矩阵均为 18/18 和 12/12，两个 Windows 普通用户消费者通过，进程与 reader 收尾收据通过。

新版 hey-my-buddy 0.45.3 的固定候选独立只读审查无阻断发现；Host 补做基线补丁重建及候选测试模块一致性核对。实际 socket/lease 零残留，历史 IPC 目录元数据未变；本机验证副本仅替换隔离命名空间，线上验证原始默认路径源码。原始命令、红绿日志、收据、审查、端点验收与 CI 文本证据保留于 `~/.codex/diagnostics/c2-r03-20261018-912ege8c/`。

用户要求清理后，已删除 79 个 Cargo 构建目录及散落的 native、dist、打包产物，保留源码、Git、日志与收据。收尾仅复核现有证据并修改文档，没有重跑已通过的测试、重建产物或触发新 CI；下载附件仅在内存核对归档哈希，落盘仅保留 JSON、JUnit 与文本日志。AGENTS.md 已加入测试或工作结束后及时清理无用产物的要求；后续文档提交不是新的实现候选。

## R01 精确分配释放身份（2026-10-16，完成）

执行基线 `3f030897`，生产候选 `de218272`，独立审查后测试补强 `141668dd`。原共享锁内的 live record 绑定 allocation id 与层级，释放拒绝错误父块、子块和同 backing 同位置旧标识；全链迁移 SHM 3 / IPC 12 / C ABI 5，不留旧格式兼容。保留长度取整、trim 原容量、creator 映射字节计费及原身份、代际与生命周期策略。R02/R07 保持独立。

生产净增 257 行，测试净增 573 行，文档净增 40 行，合计净增 870 行，作为正确性修复成本记录。本机完整 Core 1704 passed（含 relay 416）、Rust SDK 19、native Rust 8；Python ordinary 1206、portable 25、TypeScript 14，严格收据 18/18 与 12/12。既有平台专属跳过由线上适用平台执行。两份原生审查及差量复核和新 skill 0.42.1 的 buddy 只读审查无生产阻断；采纳两条测试建议的 5 行补强以双变异红绿验证，最终 c2-mem 185 项通过。

默认 256 MiB/4 KiB backing 增加 512 KiB（0.1953%）；wire 描述符和 C block 各增加 8 B。固定 5 轮交替分配微基准中位差 −0.64%～+0.10%，处于本次样本波动内，不声明 RPC 吞吐收益。本次 socket/lease 零残留，历史目录状态未变。最终 [GitHub Actions 38899277267](https://github.com/world-in-progress/c-two/actions/runs/38899277267) 对 `141668dd` 在 attempt 1 全部 9 项通过；已核对五份平台原始日志，包含完整 relay、跨语言矩阵、wheel/IPC memory 和 Windows 普通用户门禁。完整范围、样本和证据见 [R01 记录](2026-10-16-r01-allocation-release-evaluation.md#实施验收2026-10-16完成)。

## R25 旧 Python prepared-payload 入口退役（2026-10-16，完成）

执行基线 `3b191b52`，实现候选 `6afec0f36f4b0a21a3b5edc913814c5f8e05bf0b`。按用户确认的 FastDB 架构边界，删除 `CRMProxy.call_prepared`、`CoreClient.call_prepared` 和 `NativePreparedCall.call_prepared`，不留兼容。旧链原为 FastDB-owned write plan 服务，不将任意自定义编码器作为正式能力维护；现行 FastDB Payload、Python-only pickle、Core 字节接口、服务端 sink、Rust 预分配以及自定义 client/scheduler 适配协议保留。预算、期限、快照、释放、Rust API、C/FFI ABI 与 wire 均未改变。

生产仅删除三个方法共 65 行，无新增运行时分支或抽象。测试新增 18、删除 99，净减 81 行；非文档合计净减 146 行。单次消费用例迁到真实 `call(data)`；原 writer 失败段改为计费后的非法 buffer 失败，保留预算归零、不能再次消费、资源零调用及后续正常调用断言。旧 writer 专属桩和端点用例退出，既有服务端 sink 成功写入、异常与逃逸 view 保活的 Rust 测试保持。公开 Python 方法删除是明确的能力退役，不承诺旧手工 plan 调用等价迁移，也未宣称性能提升。

调整后的两个真实 IPC 用例先在原实现通过，三个 API 缺席断言在基线均按预期失败。候选本机隔离通过 native Rust 8、IPC 202 + 7、完整 relay 416，共 633 项；重建 native/CLI 后 Python 定向 375、真实 TypeScript 及持久连接 15、Rust/Python 跨语言 25、收据校验 40 项通过。完整覆盖 12/12 TypeScript 及 18/18 Rust/Python direct/relay 矩阵；本机 5 项 Windows 专属跳过由线上 Windows 门禁执行。实际加载基线/候选扩展逐一比较三个类的属性集合，仅缺少对应 `call_prepared`。

[GitHub Actions 38879398685](https://github.com/world-in-progress/c-two/actions/runs/38879398685) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。已逐组核对 Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁和 Ubuntu/Python 3.14t 对应门禁；ordinary、portable、TypeScript 分组均通过，两个 Windows 包含普通用户消费者验证。

独立原生静态审查无阻断性发现，固定补丁 SHA-256 为 `2bcf3cb547405a23a38ca04bc5bc369c3e0693d584763539d08e47f55276d9a7`。历史状态措辞的非阻断建议留作后续；错误文案未改。Host 逐字核对两个生产文件除三处删除外完全一致，服务端 sink、正式 serializer、适配协议、Core 与 Rust 预分配文件同基线一致；811 个跟踪文件逐项核对验证副本与候选，仅含记录中的隔离路径映射。

本批 socket/lease 零残留，历史路径快照未变，未使用 buddy。验证复用已验收且哈希一致的 FastDB/TypeScript 包，C-Two native、CLI 和 Node native 包均从本批隔离源码重建；线上独立构建原始默认路径源码。命令、原始日志、源/包哈希、API 对照、跨语言收据、审查及端点验收位于 `~/.codex/diagnostics/c2-r25-64a02321/`。

## 生成 TypeScript Node IPC 工厂退役（2026-10-16，完成）

执行基线 `dc7cfd24`，实现候选 `9a50c7e88a67a590b351a92cb19faedb0dfd36b1`。用户接受删除生成模块的 `createNodeIpcConnect`、`C2NodeIpcSocket`、`C2NodeIpcNet`、`C2NodeIpcConnectOptions` 及专属 socket 缓冲/等待状态链，不留兼容入口。通用 `C2IpcConnect` / `C2IpcConnection` 注入保留，官方 `@c-two/c2-mem-ffi` 连接器生产源码逐字不变；配置冻结、HTTP-only 惰性加载、身份与生命周期、Rust API、C/FFI ABI 和 wire 保持。

生产仅删除 313 行；测试新增 39、删除 1，净增 38 行；非文档合计净减 275 行。三个删除区间之外的生成传输代码逐字不变，未新增框架、依赖或替代状态。现有编译夹具补通用注入及四项旧导出拒绝，官方 socket 用例补半帧 EOF 拒绝与剩余缓冲读回；旧导出存在断言退出，现有真实协议与连接测试保留。两份历史文档注明旧工厂退役及当前官方连接器的注入方式，不声明运行性能提升。

补测先在原实现通过生成传输 7 项、官方 socket 4 项；四个退役导出的负对照均按预期失败。候选本机隔离通过 codegen 32、完整 relay 416、Node 60 项；重建 native/CLI 后 Python 定向 157、真实 TypeScript 及持久连接 15、Rust/Python 跨语言 25、收据校验 40 项通过。包含完整 12/12 TypeScript direct/relay 及 18/18 Rust/Python 矩阵；持久连接验证迟注册、丢失回复不重放、重连、路由替换和进程重启身份边界。Node 1 项、Python 5 项本机 Windows 专属跳过由线上 Windows 门禁覆盖。

[GitHub Actions 38869121561](https://github.com/world-in-progress/c-two/actions/runs/38869121561) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。已逐组核对 Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁和 Ubuntu/Python 3.14t 对应门禁；ordinary、portable、TypeScript 分组均通过，两个 Windows 包含普通用户消费者验证。

本地负对照首次因诊断命令漏传动态库搜索路径而未执行；补齐启动环境后得到四项预期拒绝。首轮 Node 为 58 通过、2 失败，均因隔离副本漏改默认路径正则期望；补齐两处精确映射后全组 60 通过、1 Windows 专属跳过。原日志保留，仓内路径、断言和期限没有修改或放宽。验证复用此前已验收且哈希一致的 FastDB/TypeScript 包，C-Two native 包及生成器均从本批隔离源码重建；线上独立构建原始默认路径源码。

独立原生审查指出的一处旧阶段文档时态已修正，第二次独立复核确认仅文档差异、原审代码与测试补丁逐字一致，无遗留发现；最终补丁 SHA-256 为 `324acea25c5d206d30571fdd3a087a264d7f1db45f08c9a8d8b143201fcd6433`。本批 socket/lease 零残留，历史路径快照未变，未使用 buddy。811 个跟踪文件逐项核对验证副本与候选一致，仅含记录中的隔离路径映射；命令、原始日志、源/包哈希、跨语言收据、两轮审查及端点验收位于 `~/.codex/diagnostics/c2-ts-node-5674d433/`。

## Python wire/handshake 投影链退役（2026-10-16，完成）

执行基线 `9561055d`，实现候选 `bb46d0a2e89d82eea04fbb47d20ca6e120ffe190`。用户接受删除低层 Python API：4 个协议 PyClass、13 个 codec、35 个常量导出以及 `transport.protocol` 退出，不留兼容入口；`transport.wire` 仅保留 `MethodTable`。5 个合同函数原样移至 `contract_ffi.rs`，现有 `_native` 名称和注册顺序保持，其中 4 个仍由 Python descriptor 生产路径调用，独立合同验证入口也保留。Rust API、C/FFI ABI、wire、身份检查与生命周期保持；`mem_ffi` 仍使用 `c2-wire`，依赖未删。

生产新增 57、删除 871，净减 814 行；测试新增 197、删除 615，净减 418 行；非文档合计净减 1232 行。两份 Python 测试的 44 个协议投影用例退出，7 个 MethodTable 用例合并为 5 个；Rust 新增 7 个边界测试并扩展现有方法数上限覆盖，包含握手计数与字段截断、call/reply 边界、255 字节 route 和零 payload 上限。现有独立手写 golden 保留；新增 Rust 覆盖较方案估算多约 30 行，用于具体错误字段和双向边界，未新增框架或声明性能提升。

新增覆盖先在原生产实现通过完整 c2-wire 150 项。候选本机隔离通过 wire 150、contract 47、完整 relay 416、codegen 32，共 645 项 Rust 测试；重建 native/CLI 后 Python 定向 386 项通过，本地 5 个 Windows 专属项由线上 Windows 门禁覆盖。真实 Rust/Python 跨语言 25 项通过，包含完整 18/18 direct/relay 矩阵；收据校验另 26 项通过。 新扩展确认 52 个退役符号和旧模块均不存在，5 个合同入口可调用；Host 与独立审查逐字核对合同函数体及 MethodTable 与基线一致。

[GitHub Actions 38858773392](https://github.com/world-in-progress/c-two/actions/runs/38858773392) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。已逐组核对日志：Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁，以及 Ubuntu/Python 3.14t 对应门禁；ordinary、portable、TypeScript 分组均通过，两个 Windows 包含普通用户消费者验证。

独立原生只读审查无 actionable findings，固定补丁 SHA-256 为 `135c499470e50f3e9b9e1921abe2a6734b80b6c3c16ca889166cd7465f0f99fe`。本批作用域 socket/lease 零残留，历史路径快照未变，未使用 buddy。隔离副本仅重定向测试默认路径及对应期望；完整命令、产物哈希、API 清单、基线与候选结果、跨语言收据、审查和端点验收位于 `~/.codex/diagnostics/c2-python-wire-6f21c0c3/`。

## R30 派生配置状态与缓存诊断收敛（2026-10-16，完成）

执行基线 `2fc8c4c2`，实现候选 `12c4ffa31360269ea65c77e42dd6720642d19aa7`。用户接受删除公开 Rust `BaseIpcConfig::max_pool_memory`，不留兼容字段或 getter；默认赋值、派生一致性校验及缓存派生字段比较退出。resolver 保留原位置的 checked_mul 及溢出错误文案，Python 已验证配置字典继续计算输出该值。Rust 结构体构造、布局、Debug 和缓存诊断少了该派生字段；18 个真实字段的完整配置比较、名称顺序、浮点行为和 none 诊断保持，配置冻结、实际三个预算、C/FFI ABI、wire 与生命周期不变。

生产新增 31、删除 116，净减 85 行；测试新增 22、删除 38，净减 16 行；非文档合计净减 101 行。缓存诊断复用原有局部宏，以一次字段声明替代名称/两侧字段的重复书写；测试删除派生字段同步赋值、专属计算 helper 和不可再构造的派生字段失配。现有真实池用例增强精确诊断和双字段顺序；既有 Python mapping 用例使用三组独立手写乘积期望。未新增框架或声明性能提升。

调整后的基线通过配置 135、真实缓存 1、Python 配置 106 项。候选本机隔离通过配置 135、IPC 202 + 7、server 160、Core 201、完整 relay 416，共 1121 项 Rust 测试；Core 四个 ignored 项为父测试显式启动的子进程入口。从候选重建 native/CLI 后，Python 配置、真实 runtime、代理、relay/mesh、跨模式、生命周期及端点回归 383 项通过；本地 5 个 Windows 专属项由线上 Windows 门禁覆盖，无非预期失败。 临时差分探针分别编译基线/候选真实诊断函数，对 18 个字段逐项、组合顺序、四种 NaN 和正负零等共 25 组比较得到一致结果，探针未进入仓库。

[GitHub Actions 38851142449](https://github.com/world-in-progress/c-two/actions/runs/38851142449) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。已逐组核对日志：Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁，以及 Ubuntu/Python 3.14t 对应门禁；ordinary、portable、TypeScript 分组均通过，两个 Windows 包含普通用户消费者验证。

独立原生只读审查无 actionable findings，固定补丁 SHA-256 为 `c7da289b51cf762d2e85ff30b43538c1aade722d9d06e14abac6cb6aaba94c2d`。本批作用域 socket/lease 零残留，历史路径快照未变，未使用 buddy。隔离副本仅重定向测试默认路径及对应期望；完整命令、构建哈希、差分结果、审查、计数和端点收据位于 `~/.codex/diagnostics/c2-r30-derived-0435c3f9/`。

## C07 relay namespace 响应单一来源（2026-10-16，完成）

执行基线 `11e1582a`，实现候选 `e18d401fbe2ac53cfa9dc81e21d3892622786e9d`。用户接受 HTTP 发现协议语义变化：`/_resolve` 仅以严格校验的响应头 `x-c2-local-namespace` 提供整批 namespace，删除逐行 JSON 包装、汇总冲突状态与客户端缺头时的默认域推断。旧 JSON 字段不再读取、校验或回退；缺头或不匹配走 HTTP，重复/非法头仍拒绝。请求过滤、probe 头、HTTP-only 惰性、身份/完整契约/代际检查与释放语义保留；Rust 公开签名、C/FFI ABI 和 IPC wire 不变。

生产新增 15、删除 86，净减 71 行；测试新增 112、删除 110，净增 2 行；非文档合计净减 69 行。删去重复的 JSON 合并与 namespace 白盒覆盖，复用现有真实 HTTP 夹具覆盖默认/自定义 root 各七种响应，包括旧 JSON 冲突与非法值、缺头、错域、非法文本/字节及重复头；保留实际 HTTP 调用、IPC 选择与零 probe 断言。模拟 relay 增加真实服务端已发送的响应头，使身份和失败回退测试继续进入 IPC。未新增框架或声明性能提升。

夹具调整先在旧生产实现通过 HTTP client 89 和 Core client modes 15 项；新协议断言在基线 namespace 回归中出现两项预期失败，分别证明旧 JSON 冲突仍否决 IPC、服务端仍发送逐行字段。候选本机隔离通过完整 relay 416、Core 201、Python 277 项，含真实 relay 双 root 同名端点选路及冻结/身份边界；Core 四个 ignored 项为父测试显式启动的子进程入口，Python 五个 Windows 专属项交给 CI。Python 首轮 276 通过、1 失败，原因是验证副本只重定向了原生默认目录，漏改一处测试路径期望；补齐该隔离映射后原用例通过，仓内默认目录、断言和期限未改，首次日志保留。最终新增的非法旧 JSON 值矩阵已单独复核通过。

[GitHub Actions 38842069727](https://github.com/world-in-progress/c-two/actions/runs/38842069727) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。已逐组核对日志：Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁，以及 Ubuntu/Python 3.14t 对应门禁；ordinary、portable、TypeScript 分组均通过，两个 Windows 包含普通用户消费者验证。

两次独立原生只读审查完成；第一轮非阻塞建议已加入现有矩阵，最终补丁 `c22b46c6cd486ad4f4a4d1e64d25de2a67de85d5b646b3e22839fc9c14c6939f` 无 actionable findings。本批作用域 socket/lease 均为零，历史路径快照未变，未使用 buddy。隔离副本仅替换本批测试默认路径及对应期望；完整命令、初始失败、构建哈希、审查、计数和端点收据位于 `~/.codex/diagnostics/c2-c07-d9814e51/`。

## R29 call-control 私有写入校验收敛（2026-10-16，完成）

执行基线为 `73fba191`。用户批准只删除已被公开入口覆盖的私有检查：身份校验中五处重复长度检查退出，`push_text` / `write_text` 不再复验长度或逐字段容量、也不再返回 `Result`；固定长度开销明确为七个长度字节、八字节 revision 和两字节 method index。公开入口身份校验、整段容量检查、错误顺序与文案、失败时缓冲不变、IPC 跨 await 校验均保留；公开签名、错误枚举、API/ABI 和 wire 格式不变。

生产新增 25、删除 54，净减 29 行；测试新增 32、删除 63，净减 31 行；两个源码文件合计净减 60 行。三个重复 roundtrip/golden 用例退出，revision 17 的完整身份往返及跨语言独立手写 golden 保留；既有 offset、短缓冲与空身份用例增强为验证实际写入、前后哨兵、usize::MAX 拒绝和身份错误优先于容量错误。没有新增类型、缓存、测试框架或公开 unchecked 入口。

调整后的完整 wire 143 项先在原生产实现通过。候选本机 macOS 通过 contract 47、wire 143、server 160、IPC 202 + 7、完整 relay 419，共 978 项 Rust 测试；从候选重建 native/CLI 后，Python wire/security、SHM、HTTP/mesh、跨模式和生命周期回归 184 项通过，合计 1162 项，无跳过。独立构建目录中的临时新旧对照覆盖 211 种身份、三档 method index，共 633 组编码及 7596 组缓冲场景；编码字节、错误变体/字段/文案逐字一致，失败缓冲及成功写入前后哨兵均保持。对照探针未进入仓库，未声明性能提升。

独立原生审查无 actionable findings，补丁 SHA-256 为 `bd4f53b8265e6631b1dc45941611a13f1d477b9f7a0749fbfc2ab34926cf8d44`，审查、测试及最终源码一致。Host 逐字核对入站解码、reply 编解码、完整容量 gate、手写 golden 和 IPC client 未改，格式与 diff 检查通过。本批作用域 socket/lease 均为零，无须补做 reap；历史目录快照未变，未使用 buddy。本轮验证限本地 macOS，未新增 hosted CI 证据；隔离副本仅重定向测试默认路径。完整日志、对照结果、构建来源、审查及端点收据位于 `~/.codex/diagnostics/c2-r29-control-d96023b1/`。

## R34 服务端 peer SHM 状态收敛（2026-10-16，完成）

用户接受删除公开 Rust Connection 的 remote_segment_names/remote_segment_sizes，init_peer_shm 改为仅接收 prefix；不留兼容入口。退出段名/大小副本、首段尺寸及钳制警告，peer pool 的 segment_size 固定为合法初始化下限 8192。实际映射仍依据帧的 index/generation/span、真实 backing 大小和 allocator header；完整握手解码、身份与代际检查、读取/释放/GC 及生命周期策略保持，Python 接口、C/FFI ABI 和 wire 格式不变。

相对 82c63270，两个源码文件生产净减 37 行、测试净减 72 行，非文档合计净减 109 行。七个状态存储/钳制用例收成一个真实 peer cache 初始化用例；原释放测试改为真实握手、32 KiB 数据读回及释放计数，覆盖空列表及 0/4096/8191/8192/65536/u32::MAX 宣告尺寸，段名刻意与实际 backing 不同。其他真实 dispatch 与 chunk 拒绝释放用例退出重复段清单搭建；未新增框架。

替换测试先在原生产实现通过：connection 8 项及上述七档真实握手 1 项。首次精确筛选器缺少 server:: 前缀选中 0 项，明确不计为通过；修正后记录实际 1 passed。候选本机 macOS 通过 c2-mem 176 + 文档 1、wire 146、server 160、IPC 202 + 7、完整 relay 419，共 1111 项 Rust 测试；重建 native/CLI 后，Python SHM、held/borrowed、HTTP/mesh、跨模式和生命周期回归 194 项通过，合计 1305 项，无跳过。

实现 `bb024930bcea2f332e357c48935c83769fc96510` 的 [GitHub Actions 38835252206](https://github.com/world-in-progress/c-two/actions/runs/38835252206) 首次运行 9/9 成功，绑定 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。五组源码任务日志分别核对 Ubuntu/Python 3.12、Ubuntu/Python 3.14t、macOS 15、Windows 2022/2025 的适用完整门禁，包含两组 Windows 普通用户 wheel 消费者。Linux 原始 Core 日志确认新握手用例实际通过；run-evidence 的源码 SHA 与候选一致。

独立原生审查无 actionable findings，补丁 SHA-256 为 050bdf233fd66f0068601e2b00769fb0ee11647e2f90e28724ce8350fcec269f，测试、审查、线上候选源哈希一致。Host 逐字确认实际映射/读/释放/GC 函数未变；改动区域格式及 diff 检查通过，其他区域既有格式差异未扩展修改。本批所有隔离作用域 socket/lease 为零，无须补做 reap；历史目录身份快照未变，未使用 buddy。隔离副本仅重定向测试默认路径，交付路径不变。完整日志、来源哈希、差量统计、审查及端点收据位于 ~/.codex/diagnostics/c2-r34-32f1f02a/。

## 取消 Drop 测试夹具修复（2026-10-16，完成）

用户单独批准将 `ReceiverDropGuardForTest::drop` 的原 `release.recv()` 放入 `tokio::task::block_in_place`，让 SyncClient 多线程 runtime 在该 Drop 等待期间继续调度。原 entered/release 信号、关闭期限、非终态及重试断言全部保留；只改 `#[cfg(test)]` 代码，生产、API/ABI、wire 和生命周期策略无变化。测试新增 5、删除 1，净增 4 行；没有新增测试框架。

基于 `94a26f1b` 的隔离前后副本使用独立 Cargo target。本次新构建基线的原两测试顺序及完整串行 IPC 都通过，说明此前超时具有调度时序性；此前失败日志与线程采样保留，不把本轮表述成确定性红绿复现。候选原两测试顺序 2 passed，完整 IPC（含 test-support）串行与默认并行各 202 + 7 passed，无失败或忽略项。本轮验证范围为本机 macOS 的受影响完整 IPC 套件，没有重新运行 GitHub Actions，也不把上一批 CI 结果计作本补丁的线上验证。

独立原生审查无 actionable findings，确认唯一 gate 安装点运行在多线程 runtime，信号及 panic 解阻塞语义不变。Host 核对补丁 SHA-256 `5c9f2373e711cc497fb21952722a774874535f92d176e14eeee0383d48379b49` 与测试、审查源码一致；改动文件 rustfmt 与 diff 检查通过。全仓 fmt 检查发现其他五个文件的基线既有格式差异，未扩展修复。所有本批作用域 socket/lease 均为零，无须补做 reap；历史目录快照未变，未使用 buddy。命令、完整日志、二进制哈希、审查及零残留收据位于 `~/.codex/diagnostics/c2-receiver-drop-b5871ea7/`。

## R13 IPC 拼帧收敛（2026-10-16，完成）

用户确认移除拼帧中间 payload 缓冲。新增无状态 `c2-wire::frame::encode_frame_parts`，现有 `encode_frame` 保持签名并复用同一实现；普通/流式 chunk、SHM 请求元数据及大 inline/SHM 回复直接生成完整帧。成功回复使用已有 `STATUS_SUCCESS` 常量。原长度检查与错误顺序、小回复栈缓冲、整批/逐帧写锁、SendGuard、快照及 lease 释放保持；现有 API/ABI、wire 和生命周期策略不变。

实现 `f45517f785f8fe2d5a95a86894e7dbd99e8b83a0` 相对 `20796fb3`：生产新增 28、删除 52，净减 24 行；测试新增 55、删除 107，净减 52 行；非文档合计净减 76 行。单块与多块测试合并为单块/首块/中块/末块表，wire 测试增加独立手写 golden 与空片段组合；未新增框架。1,440 组新旧单片段/多片段编码逐字节一致。

本机隔离通过 wire 146、server 166、IPC 202 + 7、完整 relay 419 项，共 940 项 Rust 测试；Python 1,289 项及收据校验 26 项通过，含 18/18 Rust/Python direct/relay 和 12/12 TypeScript 真实调用矩阵。5 个 Windows 专属 Python 用例在本机按原条件跳过，由线上 Windows 门禁执行。

同环境性能对照使用 macOS arm64、CPython 3.13.3、相同 release 参数和独立扩展，固定三轮交替运行。多片段编码分配由 2 次降到 1 次；metadata、64 KiB、128 KiB、1 MiB 编码耗时中位数分别下降 34.5%、62.2%、68.8%、61.6%，其中 128 KiB 为 6.233 → 1.946 μs。真实 256 KiB chunk IPC 中位延迟为 1.424 → 1.298 ms（下降 8.9%），各轮仍有波动；SHM 场景观察到约 3.3% 下降，不作为稳定收益。首轮小包慢 7.8%、大 inline 慢 0.7%；随后固定四轮交替并充分预热，小包与大 inline 分别为 +0.6%、+1.3%，组间范围重叠，未证明稳定加速或退化。首轮与补充数据均保留，不把编码微基准外推为整体吞吐提升。

[GitHub Actions 38781805659](https://github.com/world-in-progress/c-two/actions/runs/38781805659) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功。五组平台日志均核对：Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁及 Ubuntu/Python 3.14t 对应门禁，Windows 包含普通用户 wheel 消费者验证。

额外的串行 IPC 验证在基线和候选均出现同一关闭测试超时。最小复现是 `close_reserves_time_to_join_an_aborted_receiver` 后接 `two_concurrent_closes_of_one_blocked_receiver_serialize_and_stay_retryable`；采样显示测试专用 Drop 的同步 recv 阻塞 Tokio worker，目标单跑通过。默认并行基线 203 + 7、候选独立 target 202 + 7 通过；这不表示串行夹具问题已修复。测试专用 `block_in_place` 修正当时待用户单独确认，没有混入本批；后续独立夹具修复见上节。

补充对照曾因两个快照共享 Cargo target 而复用基线二进制；已由旧测试仍在清单中发现并剔除该候选结果，改用独立 target 重建后确认 202 项新清单。原 native/CLI 及性能产物通过符号、嵌入源码路径和哈希核实属于各自版本。独立审查无发现，审查补丁与 CI/验收源码一致。

诊断失败留下的 6 对本批 socket/lease 已按运行记录核实归属和退出状态，再用原生凭据精确 reap；最终本批所有作用域 socket/lease 为零，历史目录快照未变，未使用 buddy。隔离副本仅重定向默认测试目录及对应期望，交付路径不变。完整命令、失败与成功日志、性能原始数据、产物哈希及验收证据保存在 `~/.codex/diagnostics/c2-r13-frame-87df291d/`。

## R23 Python bridge 单一会话（2026-10-15，完成）

用户确认将 `NativeServerBridge` / Python `Server` 固定绑定到构造时的 `RuntimeSession`，删除独立构造分支、重复身份与配置投影、操作级 session/relay 参数，以及无实际期限作用的 `start(timeout=...)`，不留兼容入口。这是低层 Python 签名变更；常规 `cc.*`、Rust API、C/FFI ABI、wire 和 Core 生命周期策略不变。保留构造阶段配置预检、错误顺序、borrowed 校验、动态 Resource 查找、真实 shutdown 期限，以及未完成关闭时的 slot/hook 保留与释放语义。

实现 `f94f1822464cb5d9f05309a62377bb85cfe9bfe3` 相对 `d140afc9`：生产新增 16、删除 96，净减 80 行；测试新增 185、删除 222，净减 37 行；非文档合计净减 117 行。配置测试直接覆盖现有 resolver，现有夹具增加 9 行 session 构造入口并迁移低层调用；退出影子配置与无效参数的旧证明，保留真实绑定/启动/关闭及错误顺序覆盖。实现内文档新增 15 行，另补本条验收记录；未新增运行时框架或声明性能提升。

基线配置/session 146 项通过，新的预检错误顺序与 owner 测试亦先在原实现通过；候选针对性 176 项通过。本机隔离完成完整 c2-http/relay 419 项、Python 1,289 项（普通 1,249、Git 元数据 1、portable 25、TypeScript 14）及收据校验 26 项，含 18/18 Rust/Python direct/relay 和 12/12 TypeScript 真实调用矩阵。本机 5 个 Windows 专属用例按既有平台条件跳过，由线上 Windows 门禁执行。

[GitHub Actions 38758318061](https://github.com/world-in-progress/c-two/actions/runs/38758318061) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功；Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁及 Ubuntu/Python 3.14t 对应门禁均已核对日志，Windows 包含普通用户 wheel 消费者验证。

本地完整 Python 首轮因验证脚本使用非 editable 安装，52 个显式从源码导入的子进程找不到 native 扩展；逐项核实后改用隔离源码的 editable 安装，确认父子进程加载同一扩展，再运行完整清单通过。首次失败日志与临时目录保留，生产代码和测试断言未改；Git 元数据用例在真实工作树单独执行。GitHub 使用其既有安装流程，未重跑。

独立原生审查无发现；审查补丁、验收源码与 CI 提交一致，扩展、CLI、包与矩阵收据哈希已核对。本批含首次失败目录在内的 socket/lease 零残留，历史目录快照未变，未使用 buddy。隔离副本仅重定向测试默认目录及对应路径期望，交付路径不变；完整命令、日志、产物哈希和验收证据保存在 `~/.codex/diagnostics/c2-r23-d29bbbb7/`。

## R31 Held owner 配对（2026-10-15，完成）

用户确认将 Rust `Held<T>` 的 value/response 收敛为一个私有 `Option<(T, HeldResponse)>`，删除无合法构造路径的半开状态分支。释放时分别建立局部 owner，保留正常路径先销毁 value、再销毁 response，以及 invalidator panic 时先析构 response、再析构 value 的顺序；Core 失效后释放租约、错误合并和幂等语义不变。公开 API/ABI、wire 和生命周期策略不变。

本批相对 `32e1362f`：生产新增 12、删除 34，净减 22 行；测试新增 32、删除 44，净减 12 行；非文档合计净减 34 行。新增 `#[cfg(test)]` 块计入测试，真实 Held 的失败/panic/重复释放测试替换了测试自造的 SdkHeldDropGuard；保留真实 FastDB owner 测试及 Core 复制/释放错误合并覆盖。新增单测先在原实现通过；8 个显式释放/Drop/错误/panic 顺序场景在基线与候选一致，故意调反局部声明顺序的对照在 panic 场景按预期被拒绝，未新增生产抽象或声明性能提升。

本地隔离回归通过 Rust SDK 19 项、Core 生命周期 5 项、生成 Rust 消费者 4 项、完整 c2-http/relay 419 项；Python 157 项（relay/lifetime 131、Git 元数据 1、portable 25），含 18/18 direct/relay 矩阵；矩阵校验器另 26 项通过。独立原生审查无发现，固定审查补丁与验收源码哈希一致；本批 socket/lease 零残留、历史目录快照未变，未使用 buddy。

本批验证范围为本机 macOS，未新增 hosted CI。隔离副本仅重定向测试默认 socket/协调目录及对应路径期望，交付源码路径不变；顺序探针、负对照、命令、日志、产物哈希及验收证据保存在 `~/.codex/diagnostics/c2-r31-e6591997/`。

## R24 旧 transfer buffer 配置链退出（2026-10-15，完成）

用户确认删除 Python `transfer` / `auto_transfer` 的旧 `buffer` 形参及声明侧元数据，不留兼容入口；显式传入 None、view、hold 或非法值均由签名拒绝为 TypeError。注册级独立 borrowed 校验、运行时 `_c2_input_buffer_mode`、客户端 hold、动态 Resource 方法查找和释放顺序保留。运行时 descriptor 仍输出 `buffer: "view"`；Rust API、C/FFI ABI、wire 和 Core 生命周期策略不变。

实现 `056ba4a1c50cdd2b921512d1768184a612892d9d` 相对 `9a882710`：生产新增 6、删除 41，净减 35 行；测试新增 6、删除 5，净增 1 行；非文档净减 34 行。两个入口各四种旧关键字值替换原 hold 迁移报错测试，8 项旧实现负对照均按预期失败、候选均通过。普通与 portable 合同的完整 descriptor/fingerprint、以及 portable release reference 快照与基线一致；未增加运行时抽象或声明性能提升。

本地隔离通过完整 Rust relay 419 项、Python 351 项（常规 325、Git 元数据 1、portable 25，含 18/18 direct/relay 矩阵及 Python 3.10 语法检查）。[GitHub Actions 38725348808](https://github.com/world-in-progress/c-two/actions/runs/38725348808) 绑定上述实现与 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，首次运行 9/9 成功；Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁及 Ubuntu/Python 3.14t 对应门禁均已核对日志，Windows 包含普通用户 wheel 消费者验证。

本地 portable 首轮遗漏专用 `C2_PORTABLE_MATRIX_C3_BIN`，夹具在错误的默认产物路径查找 CLI，造成 24 个 setup 错误；补齐变量并固定 CLI 副本后同一矩阵通过，生产代码与测试断言未改，各阶段产物哈希分别留档。独立审查无发现，审查、测试和提交源码一致；本批 socket/lease 零残留、历史目录快照未变，未使用 buddy。隔离副本仅重定向测试默认目录及对应期望，线上使用原路径。完整证据位于 `~/.codex/diagnostics/c2-r24-4b09f0f5/`。

## R08 IPC 字段解析收敛（2026-10-15，完成）

用户确认仅合并 IPC 环境赋值与代码覆盖的重复逻辑，保持原行为。16 个公共字段及 server/client 专属字段的环境键、解析器和错误顺序逐项核对；被代码覆盖的非法格式环境值仍拒绝，False/0、派生计算后最终校验及配置冻结保持。未采用先选来源再解析的语义方案；公开 API/ABI、wire 和生命周期不变，relay 配置与 R09 dotenv 快照仍不在本批范围。

实现 `0f31294acf7ae21d4f61f0337676ff04b8ccc39e` 相对 `379b389e`：生产新增 45、删除 220，净减 175 行；测试新增 62、删除 46，净增 16 行；非文档合计净减 159 行。原私有覆盖宏与两轮赋值收敛为一个私有字段宏，测试以独立手写字段表合并三段重叠覆盖，未新增框架或声明性能提升。

42 个字段/错误顺序场景先在原实现通过。首轮表中心跳超时误填 15（等于默认间隔），被原校验正确拒绝后改为合法的 20/45，未放宽断言。本机隔离回归通过配置 135、Core 202、完整 c2-http/relay 419 项；4 个 ignored 入口为父用例实际调用的子进程 fixture。Python 配置、冻结、HTTP/mesh、关闭、benchmark、跨模式、直连与真实闲置回归 242 项通过，归档缺少 Git 元数据的 1 项另在实际工作树通过，共 243 项。

[GitHub Actions 38703304629](https://github.com/world-in-progress/c-two/actions/runs/38703304629) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，9/9 首次运行成功；Ubuntu/Python 3.12、macOS 15、Windows 2022/2025 完整门禁及 Ubuntu/Python 3.14t 对应门禁均已核对日志。独立审查无发现，审查、测试与提交源码哈希一致；本批 socket/lease 零残留、历史目录快照未变，未使用 buddy。隔离副本仅重定向测试默认目录及匹配的路径期望，线上使用原路径。命令、日志、基线对照、产物哈希与验收证据保存在 `~/.codex/diagnostics/c2-r08-bcd260ad/`。

## C05 Node 端点能力收敛（2026-10-15，完成）

用户确认升级 C/Node ABI 4，要求完整 context 符号组，将 TypeScript capture 变为必需并删除 UnsupportedError，不保留旧 ABI 接纳。原来的可选组服务于 ABI 3 内追加能力；本批移除 loader 无 context fallback 与 legacySymbols，保留独立 C 查询 API、首次 IO 前纯查询、首次成功/失败 IO 的配置冻结、HTTP-only 惰性加载、严格身份与释放语义，wire 和 Core 生命周期策略不变。预分配等其他 C05 子项仍暂缓。

实现 `1bc6e7415f2497a464d18671963a1f4c146cab2d` 相对 `eebcc539`：生产净减 71 行、测试净减 56 行，非文档合计净减 127 行。现有旧库成功测试改为 ABI 3 提前拒绝和 ABI 4 缺 Unix connect 符号拒绝，保留 ownership/GC/冻结/真实连接用例；使用旧 loader 配 ABI 4 header 的负对照，两项新拒绝断言均失败，候选均通过。未增加运行时抽象或声明性能提升。

本地隔离通过完整 relay 419、FFI 27、Node 59 项及打包安装/类型检查；1 个 Windows 专属用例由线上执行。[GitHub Actions 38693012224](https://github.com/world-in-progress/c-two/actions/runs/38693012224) 绑定上述实现及 FastDB `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，9/9 成功，覆盖 Ubuntu/Python 3.12、Ubuntu/Python 3.14t、macOS 15、Windows 2022/2025 的对应完整门禁。独立审查无发现，审查后源码哈希匹配；本批 socket/lease 零残留，历史目录快照未变，未使用 buddy。后续破坏性 API/ABI 更改的线上跨平台要求已写入 AGENTS.md。

首轮 Node 真实长路径用例因本批显式容器太短，未达到 512 字节前置断言；改用更长的本批容器后该用例通过，生产代码与断言强度未改。隔离副本仅重定向测试默认目录与对应路径期望，线上使用原默认路径。命令、失败/成功日志、负对照、产物哈希、独立审查和端点验收保存在 `~/.codex/diagnostics/c2-c05-8f85ae7c/`；历史 ABI 收据保持原事实。

## R30 override 单一来源续批（2026-10-15，完成）

用户已确认从 `e2f98bd9` 删除公开 Rust `BaseIpcConfigOverrides` 及 Server/Client override 的嵌套 `base`，保留扁平字段，按原 `flat.or(base)` 迁移有效值，不留兼容入口。Python 接口、解析与回读顺序、配置冻结、C/FFI ABI、wire 和生命周期策略保持；resolved `BaseIpcConfig`、Python `BaseIPCOverrides`、共享 key 表与 R08 环境错误策略不在删除范围内。

先迁移测试，原实现的预算覆盖、relay 完整策略与配置冻结 3 项 Rust 用例及 2 项 Python 回读用例通过，再删除生产通道。6 个被扁平值遮蔽的设置退出，其他 base-only 有效值迁移；既有回读用例覆盖全部 16 个公共字段（chunk_size=None 不投影）、字段顺序、False/0 和副本隔离。生产净减 86 行、测试净增 7 行，非文档净减 79 行；未新增框架或测量性能收益。

候选在本机 macOS 通过配置 135、Core 202、完整 c2-http/relay 419、Rust SDK 18 项，共 774 项；4 个标记 ignored 的 Core 入口是由父用例实际调用的子进程 fixture。Python 配置/RuntimeSession/HTTP/mesh/关闭/跨模式/直连/真实闲置回归 203 项通过，另有 Git 元数据用例在实际工作树通过，共 204 项。CLI/PyO3 构建和 C/FFI crate 检查通过；独立原生审查无发现，审查后源码哈希一致。本批隔离 socket/lease 零残留、历史目录快照未变，未使用 buddy，未新增 hosted CI 证据。

隔离副本只重定向默认 socket/协调记录路径及匹配的测试期望。首次配置测试的两处失败来自遗漏的 3 个旧路径字面量；Rust SDK 首次启动缺少 FastDB 动态库搜索路径，补齐测试环境后验证通过，交付源码和断言强度未改。沿用已验证的本机 C++ 头文件路径和 Python strip=none 构建参数。失败/成功日志、产物哈希、统计及端点验收保存在 `~/.codex/diagnostics/c2-r30-12347302/`。

## R12 局部续批（2026-10-14，完成）

用户已确认从 `9e882f33` 继续：成功获取仅返回 lease 与 live binding，调用者持有原路由快照，退出重复快照和同值后置校验。第一次 `acquire_route`、live token 获取、跨 await 身份栅栏、错误顺序及释放语义保留；公开 API/ABI 不变，不扩展到 endpoint connector 重构。

先合并路由替换测试的重复搭建，3 项在原生产实现上通过，再收敛 `state.rs`/`router.rs` 及内部调用点。call/probe 保留独立用例并明确断言 `409 / 704 / RouteStale`，同合同替换的 no-replay 断言保留；memory 用例改为验证 live binding 的 UID/revision。

实际生产净减 53 行、测试净减 92 行，非文档合计净减 145 行。移除一次完整路由克隆及重复校验，未测量延迟或吞吐收益。以下历史收束数字仍绑定其原提交。

本机 macOS 隔离副本完成 c2-http 全部 419 项测试（含完整 relay）、Python HTTP/mesh/错误/关闭/benchmark/跨模式及直连回归 93 项；归档缺少 Git 元数据的 1 项另在真实工作树通过，共覆盖 Python 清单 94 项。真实 idle 用例保留两轮各至少 62 秒的等待。独立原生审查无发现；审查后源码 SHA256 核对一致。测试只在副本重定向默认 socket root 和协调记录目录，交付源码路径不变；本批 socket/lease 零残留，历史目录快照未变，未使用 buddy。本批未新增 hosted CI 证据。

构建记录：c3 首次构建缺少本机 C++ 标准头文件，通过既有 CPLUS_INCLUDE_PATH 修正。Python 首轮收集遇到 LINKEDIT 对齐错误，独立最小程序复现了 [Rust 调试信息剥离缺陷](https://github.com/rust-lang/rust/issues/157750)；仅测试构建设置 CARGO_PROFILE_RELEASE_STRIP=none 后扩展加载及回归通过，未修改仓库构建策略。原始失败、成功日志、产物哈希和端点验收保存在 `~/.codex/diagnostics/c2-r12-7fc68311/`。

<a id="closeout"></a>
## 本轮收束（2026-10-14）

本轮已按用户要求收束，停止扩展实现。统计从瘦身前 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad` 到 R13 实现 `e21e79c4652e8faf8b68f8c4c300a969dfd79daa`，共 37 个提交、92 个变更文件；本次收束记录仅修改文档，另行列账。实施分支为 `socu/slimming-batches`。

统计采用两端各类文件的物理行数之差，含空白与注释；Rust 内嵌 `cfg(test)` / `cfg(all(test, ...))` 及测试专用 hook 计入测试，代码内注释随所在代码计数。工具、示例、benchmark、CI 和 Cargo 配置单列；文档含审查、计划和指导文件。编译标记改变造成的未改文本归类迁移也计入对应类别；各类净量之和已与 `git diff --numstat --no-renames 2ffe356f e21e79c4` 核对。

| 范围 | 生产运行时 | 测试 | 工具/示例/CI/配置 | 文档 | 总净变化 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 局部瘦身与测试收敛，23 个提交 | -785 | -621 | -256 | +194 | -1468 |
| R22 benchmark 修复与工具精简，1 个提交 | 0 | -7 | -212 | +14 | -205 |
| 生命周期、环境接入与回归修复，7 个提交 | +367 | +638 | +33 | +52 | +1090 |
| R13 所有权传递，1 个提交 | +4 | +46 | 0 | +10 | +60 |
| 独立审查/计划记录，5 个提交 | 0 | 0 | 0 | +402 | +402 |
| 截至实现提交 e21e79c4 | **-414** | **+56** | **-435** | **+672** | **-121** |
| 本次收束记录 | 0 | 0 | 0 | +74 | +74 |
| 含收束记录的最终净量 | **-414** | **+56** | **-435** | **+746** | **-47** |

截至实现提交，Git 物理差异为新增 3455 行、删除 3576 行。生产代码与测试合计净减 358 行；加上工具/配置后，非文档部分净减 793 行。局部瘦身非文档净减 1662 行，R22 工具修复与精简净减 219 行，生命周期/验证修复净增 1038 行，R13 净增 50 行，分别计算。修复增量主要来自 macOS 双目录身份绑定及回收验证；R13 的生产增量为 4 行契约说明，执行逻辑净行数不变，新增测试 46 行。

### 已完成范围

| 范围 | 已交付结果及保留边界 |
| --- | --- |
| B1–B8：R27、R28 Python、R29 release、R31、R33、R36、R37、R38 的局部子项 | 删除失效调度计数、同值重验、孤儿 helper、旧发布探针、重试死状态和未读 TypeScript 转发；以已持有 Payload、已提交配置和配对 sweep owner 表达状态；合并重复测试，保留公平性、取消、错误和析构顺序。 |
| B9：R11、R39、T02 | 由 BuiltRoute 派生登记信息，退出双规格对账；操作直接返回各自结果；测试调用生产合同解析路径，退出两套 name-only 测试解析器。 |
| B10/B15、B11/B12、B13、B18、B19 | 分别完成 R30 私有 16 字段投影/覆盖去重、R28 validated FULL_SYNC/Digest 表示收敛、R34 私有握手/墓碑副本退出、R14 SHM tier 选择收敛、R32 Rust 错误码单一声明及独立手写 golden。公开配置类型、GC/压力计数、wire 身份与未知码语义保留。 |
| B14：R22 benchmark 修复与精简 | 复用既有 SDK 登记/撤回/关闭，修复过期身份调用及请求 token/header；数据计时仍显式走 HTTP，Core 的一致性、闲置驱逐与生命周期策略未改。 |
| B16/B17：测试收尾与初始化失败回滚 | 复用 close/reap 补齐测试收尾；只回滚本次新建且身份未变的 lease，保留既有未知对象的拒绝回收规则。 |
| 9ebef7e5：macOS 协调记录 | 仅默认命名空间的 gate/marker/lease 移到系统用户目录；socket 路径保持。双目录在同一 gate 下严格绑定身份，覆盖别名、目录替换、异常退出和重启回收；自定义 root、Linux 和 Windows 不改存储布局。 |
| ccb54a38 / cb449bff / 0aac23c8 / 20984527：验证修复 | 补齐 hey/Go 前置条件和故障夹具隔离；Windows 期限用例固定第二次 lookup 响应，保留原期限和严格 stage；FULL_SYNC 正式补入“合法路由在前、非法 CRM 在后”且不得部分提交。 |
| e21e79c4：R13 请求所有权传递 | Core 保留初始请求快照，Vec 直接移交 Python；Rust 公开 EncodedService 签名、生成器、实现与示例同步更新，旧生成模块须重新生成。未增加兼容双入口；响应快照、错误顺序与传输 lease 释放语义保留。 |

### 已验证结论

早期 31 个提交的逐项静态审计未发现可确证的新增 relay 功能回归；隔离 relay 对照的基线为 427 passed、候选为 419 passed，均无失败且 socket/lease 为零。测试数差异含用例合并，不能作为覆盖减少或提升的单独证据；没有将两端结果外推为每个中间提交均实跑。曾遗漏的 FULL_SYNC 路由顺序场景已由 `20984527` 纳入正式测试，58 项定向回归通过；故意逐路提交时旧用例通过、新用例失败。

已在隔离环境确认系统清理可删除协调身份记录；旧基线也具有相同失效路径。修复保持原 socket 位置与严格身份检查，迁移默认 macOS 协调记录。历史目录的本机冷切换没有执行。此前经授权并核实归属的 261 个端点已通过原生凭据精确 reap；截至 R13 验收，另外 12 个旧空 lease 仍保留。本次收束只读复核发现 `/tmp/c2-1f5`（亦即 `/private/tmp/c2-1f5`）已不存在，原因未核实，不能将路径消失计为原生回收成功；本次没有清理、重建或冷切换历史目录。

`0aac23c8` 的[修复后 CI](https://github.com/world-in-progress/c-two/actions/runs/38520945654)与 `e21e79c4` 的[最终实现 CI](https://github.com/world-in-progress/c-two/actions/runs/38551367021)均为 9/9 成功，覆盖 Ubuntu/Python 3.12、Ubuntu/Python 3.14t、macOS 15、Windows 2022/2025 的适用 gates。该证据绑定实现提交；后续收束提交只有文档，不把它描述为重新编译过的实现。

R13 本地通过 Core 202、Rust SDK 18、生成消费者 4、native payload 4、Python 定向 112 与 portable 25 项测试，共 365 项，另实际运行两个 Rust 示例；portable 包含完整 18/18 direct/relay 矩阵。新所有权测试在旧实现上按预期失败，在候选上验证正常/异常返回后的视图与释放守卫。默认命名空间用例在只重定向测试路径的隔离副本运行；性能对照使用未改默认路径的正式源码 wheel。独立审查无发现，各次本批端点验收 socket/lease 零残留。

R13 在同一 macOS arm64、CPython 3.13.3、相同依赖与 release 参数下，固定 3 轮交替、共 24 次真实 IPC benchmark。4 MiB 单 worker 中位延迟为 4.323 → 4.191 ms（下降 3.1%），4 worker 为 8.279 → 7.916 ms（下降 4.4%）；两组 P95 均下降约 4%。小请求样本有重叠波动，不计为稳定收益。确定的结构收益为每个经 Python 桥的非空请求少一次等长分配和复制；没有把它换算成等额 RSS 降幅，也没有为其他瘦身子项声明性能提升。

### 暂缓项与原因

以下仅记录 2026-10-14 收束时的状态，不是当前待办表。当时 R01–R07 尚未验收为已修复；后来 R01、R03 已完成，R08、R23、R24、R30、R31 等亦有续批。当前状态以本文顶部及对应验收记录为准，未完成项仍须按当时源码独立复核，不因其他批次 CI 通过而自动关闭。

| 条目 | 剩余范围与暂缓理由 |
| --- | --- |
| R01–R04 | 精确分配所有权、safe Rust 别名、shutdown 发送不确定性、登记代际/开放事务；P1 单独设计与修复，不能靠删保护或混入局部瘦身处理。 |
| R05–R07、R17–R18 | 完整 SHM 接纳、导出/释放同步、失败释放的 owner/charge、端点 I/O 错误分类、observer 容量恢复；涉及正确性和错误语义，需各自反例及最小方案。 |
| R08、R09、R30 剩余 | 配置解析优先级、一次 dotenv 快照、公开配置类型/派生字段；会改变读取时点或错误观察。私有投影与覆盖顺序去重已完成。 |
| R10 | 用户明确暂缓；约 20 行局部去重不足以抵消抽象与测试成本，独立 borrowed 校验、动态方法查找和错误顺序继续保留。 |
| R12、C01、C02 | 跨 await 的 live authority、endpoint/route 获取分离、不可变合同与更早接管 RequestLease；需证明共享连接、登记竞争和提前拒绝的所有权边界。 |
| R13 剩余 | 响应快照、初始 ownership 转换、inline/chunk/frame 编码副本；本轮只移动已拥有的 request Vec，其余需分别证明生命周期与测量收益。 |
| R14 剩余、R15、C03、C04 | CheckedSpan/bitmap 锁证明、GC admission 时机、chunk 代际回收、轮询改通知；不能丢回收进度、预算或唤醒，部分与 R01/R02 耦合。 |
| R16 | Node 接收容量和读取进度需要一起设计，简单 pause 可能阻塞合法大帧；不是局部删除。 |
| R19–R21 | CLI 畸形响应、示例文件加载后覆盖、公开 Resource 模板；属于错误/数据保护或公开生成行为修复，另行推进。 |
| R22 剩余 | benchmark selector/历史 sweep 与测量标签；控制面和 HTTP 实跑已修复，其余属于工具整理，不冒充运行时收益。 |
| R23–R25、C07 | timeout/旧 transfer 参数、公开 client/scheduler 适配协议、结果形状与 namespace 模型；需要能力退役或语义决定，保留当前动态查找及接纳边界。 |
| R26 | relay journal 保留。历史快照和删除原因有独有信息，计划能力尚未明确退役，也未测得足以支持删除的成本。 |
| R29、R32、R33 剩余 | wire writer、FFI 转换、错误/凭据序列化、公开常量及 wrap_error；局部净减量有限，缓存或兼容抽象可能增加维护面，公开观察需先决定。 |
| R31 剩余 | Held 的 owner 配对仍涉及 invalidator panic 与释放顺序；BorrowedPayload、配置冻结事实和 sweep 配对已完成。 |
| R34、R35、C05 | RouteAck/公开观察面、禁用 heartbeat 任务、ABI/预分配能力；须明确支持范围，不能按当前调用少就删除；收益或替代收尾尚未证实。 |
| R36/R38 剩余、C06 | 仅已证明的死状态、紧邻重验与 readiness 后等待已退出；其他等待和维护 EOF/退出语义仍有职责，不能泛化删除。 |
| T01–T07 剩余 | 词串门禁、假 owner、阻塞 readline、重连/GC 负例和默认值证明仍须逐项替换。T02 重复解析器、若干夹具、Digest/FULL_SYNC 顺序与错误码 golden 已收敛；ping 无重试敏感性等有效覆盖保留。 |
| 本机历史目录 | 末次验收快照中的 12 个空 lease 归属未全部确认；收束时旧根路径已缺失，原因未核实，未记为回收完成。恢复/冷切换未执行，不扩展本次排查或历史回收权限。 |

收束阶段仅整理文档、核对固定提交及既有证据，没有启动运行时、清理历史文件、使用 buddy 或新增下一批实现。以下 B1–B19 保留各批完成当时的记录；其中早期审查工具和当时的平台验证限制属于历史事实，当前状态以上述收束结论为准。

## B1–B19 历史实施记录

依据：[二轮复核](../reports/2026-10-13-defensive-programming-regression-reassessment.md)。起点为 `2ffe356f15d8ce1c8fad9e08a91ffff808d34dad`，实施分支 `socu/slimming-batches`。用户授权先实施证据充分、保持行为的局部精简；每批完成针对性回归和独立审查。Host 负责实现、验证与最终验收，独立审查通过 buddy 的只读固定快照执行。

首轮 B1–B6 已完成，18 个非文档文件新增 7 行、删除 594 行，净减少 587 行；退出 13 项只服务已删除实现的测试。每批均有独立提交、现有行为回归及固定候选独立审查，审查未发现阻塞问题，6 个审查工作树均已验收回收。运行证据来自本机 macOS arm64；未执行 Windows/Linux hosted CI、性能基准或发布，不推导跨平台全面通过或性能收益。relay journal 及 GC 运维日志保持，公开 API/ABI 和 P1 项目仍按下述边界另行推进。

| 批次 | 实现提交 | 实际回归 |
| --- | --- | --- |
| B1 | `306a922cd256631f80e20dede0b44815b7db90aa` | c2-server 168 passed |
| B2 | `d4d0e9012a653e2f867dbfe128d28ff707325453` | Python 30 passed |
| B3 | `dcb72f590c6cf6b9172011a8b5798af224975ff7` | codegen 2 + generated Rust SDK 4 passed |
| B4 | `b4c2a4ae7a084db00fc073e9e49500bba085ce2e` | c2-mem 177 + c2-wire 146 + c2-server 168 + 重建后 Python 30 passed |
| B5 | `77b3d9bd73c2b6163706891d7f2bdd25710ee187` | config、local candidate、consumer 及 RuntimeSession 113 passed |
| B6 | `8295266fe679c4aba52e31e97c4019a3a253fbc3` | workflow-policy 完整 12 文件清单 506 passed |

以上为各批实际运行次数，重复运行的测试不计为新增独立覆盖。两份历史报告的 280 个源码链接已固定到原审查提交并核对文件与行号，避免删除或移行后丢失依据。

## 批次与出口

各批按顺序推进，上一批实现、测试和独立审查均完成后再进入下一批；每批独立提交。测试失败先与基线和环境证据区分，任何实质性新改动都重跑对应验证。不为删除字段、恒空分支或重复调用编写镜像测试。

| 批次 | 对应条目与实现范围 | 针对性回归与审查边界 | 状态 |
| --- | --- | --- | --- |
| B1 | R27：仅删除 scheduler.waiting_writers、writer_wait_registered 及维护分支 | scheduler 全部测试，特别是读写公平、async/blocking FIFO、取消/关闭；确认 queue/ticket/notify/pending/active 不变；检查 SHM 路径未改变 | 已完成 |
| B2 | R28 Python：bridge 工厂依靠 ResourceBridge 构造校验；PayloadBinding 的 scoped-owner 能力依靠已验证 kind | resource conformance、input lifetime、payload binding 相关用例；非法 hook 错误和 borrowed 门禁不变 | 已完成 |
| B3 | R29：生成 service_definition 复用已有 release.reference() | codegen 单元/生成消费者测试；生成代码仍能编译使用，合同字节及公共生成表面不变 | 已完成 |
| B4 | R36 局部：bitmap 恒空循环、空 handle 紧邻重验、私有 retention 总和蕴含的局部加法检查、chunk 未读 _created_at | c2-mem bitmap/handle/retention 以及 chunk registry 测试；全域预算、错误计数、世代/跨度/锁、GC last_activity 不变 | 已完成 |
| B5 | R33 内部孤儿：私有 _resolve_client_ipc_config、三个 local_registry 闲置 helper、只由自身测试调用的 process_guard | native config 投影、local registry/package consumer 相关测试；保留实际 manifest/receipt 校验与 _stop_host 清理 | 已完成 |
| B6 | R37：退出生产发布流程的 check_version/check_cli_release 与专属测试，移除 CI 中对应测试选择 | 当前 release promotion 与 workflow 测试；自动 skip、手工冲突、partial retry、来源/字节验证不变 | 已完成 |

每批记录精确提交、命令、结果、独立审查结论及必要限制。实际发现前提不成立时，把该子项从本批撤出并记录原因；不以增加兼容层或放宽测试完成批次。

## 单独推进的范围

relay journal 保留。公开 API/ABI/语义变化先讨论：旧 transfer buffer 参数、timeout 签名、公开 client/scheduler 适配协议、ERROR_CODE、RouteAck/RouteContract、Connection/Heartbeat 观察面、ABI 3/预分配能力、配置解析错误策略、EOF/退出策略、Node 接收容量和基准选路。wrap_error 虽无调用但在活动指导中列出，本轮暂不把它随内部私有 helper 删除。

P1 的 R01 精确分配所有权、R02 safe Rust 别名、R03 shutdown 发送阶段和 R04 路由开放登记身份独立列项，保留复现/时序证据，先形成各自最小修复方案与回归要求，不混入上述精简提交。RegistrationPlan、复制/GC/轮询、跨 await 权威获取、OwnedHandle 等条件重构也另行设计。其余局部机会（IPC 重试尾部、内部状态配对、TypeScript 转发、已就绪 fixture 等待）在本轮各批稳定后按相同标准另立批次。

## 第二轮安排

规划输入为首轮完成后的 `08955f0a1c1e94e21da2a45ca5c04abba071dc49`，计划提交为 `60bac6541e2747adaa0c22e6f256adb34fe76589`。用户已同意执行 B7/B8，实际状态与验证见下方执行记录。继续保留 relay journal、公开 API/ABI 和现有错误语义；P1 具体接口/布局/错误投影仍按独立设计边界讨论，不混入本轮局部精简。实施前重新核对源提交与工作树，若前提发生变化，先更新对应边界。


B7/B8 六个子项的首次提交通过了回归与独立审查，但生产/工具净减少 58 行、测试净增加 163 行，整体反增 105 行，未达到本轮整体精简目标。后续将重复夹具和重叠用例合并：相对执行基线 60bac654，6 个生产/工具文件仍净减少 58 行，5 个测试文件净增加 51 行，11 个非文档文件合计净减少 7 行。首次验证记录见下表，测试收敛记录见文末。后续批次一并评估生产代码和常驻测试的维护成本，优先复用或替换已有行为用例。

| 已完成子项 | 实现提交 | 实际验证 |
| --- | --- | --- |
| B7a | `e4b947a4c57ccbdc15f07e1f8aed331b5aa8c4c0` | 首次 IPC 203 + deadline 9 + relay state 45 passed |
| B7b | `d24bc2406d2721cb11d35e4aa17289736c6260cf` | Node 59 passed / 1 Windows-only skipped；typecheck 与公开声明字节比对通过 |
| B7c | `b4e916c9aae66a270fc9ec7ed66f41fad0aa01e8` | Python 33 passed；SHM 定向 1 passed；bytes/dict smoke 完成 |
| B8a | `e9d0650c882fa95dd87d0b21a8ed037683321bed` | Rust SDK 4 + generated Rust SDK 4 passed |
| B8b | `2949cd719621ae9bb59a74cbb19eff31e5d0c44e` | Rust 140 + Python 107 passed；2 个 ignored 为父用例启动的子进程入口 |
| B8c | `89e3191d27ae32f85a386a4c36df52d15e9ee1b3` | Python 111 passed / 5 Windows-only skipped |

各批测试次数存在重叠，不相加为独立覆盖总数。B7a 因 buddy 升级围栏改用原生只读代理核对固定 diff/hash；其余 5 项通过 buddy 固定工作树审查，均已验收回收。所有实现提交的非文档内容已与相应审查输入逐文件核对。验证仍限本地 macOS；Windows、free-threading 并发、hosted CI 与发布未由本轮证明。relay journal、Held<T>、P1 修复及公开 API/ABI/错误策略议题仍保持独立。

### B7：剩余局部精简

B7 按下表顺序拆成三个独立提交；每项完成针对性回归和固定快照独立审查后再进入下一项。目标是撤去已失去作用的内部状态与等待，不合并不同层的重试或资源协议。

| 子项 | 精确范围与目标形态 | 必须保持的行为 | 回归与出口 |
| --- | --- | --- | --- |
| B7a · R36 | [IPC acquisition](../../core/transport/c2-ipc/src/client.rs#L4615) 的 lookup_route_contract_for_acquire、acquire_route、acquire_route_token：删除 last_not_found/last_unbound 和不可达尾部；循环消费固定退避序列，无下一次重试时直接返回当前实际结果 | 每层原有 6 次迭代、10/25/50/100/200 ms 退避、错误分类与最终错误内容；compaction/watch 恢复的额外查询；每次获取 live authority；外层原始 deadline。不得把两层预算合成一层，或以 unreachable/panic 代替尾部兜底 | c2-ipc 的 lib/tests，启用 test-support 使 pending 清理断言实际执行；c2-http 的 relay state acquisition 用例启用 relay feature。重点验收 deadline 内重试、缓存 token 仍做 live lookup、关闭/移除/同名重登记、最后一次成功与耗尽返回实际错误；仅在现有用例未覆盖后两项时补行为测试 |
| B7b · R36 | [TypeScript binding](../../core/foundation/c2-mem-ffi/bindings/typescript/src/index.ts#L438)：删除无后续读取的 options = loadOptions；normalizeRequestPool 直接调用 normalizeSegment，删除仅转发的 normalizeRequestSegment | addonPath 捕获、endpoint context 生命周期、segments 快照和冻结、完全相同的错误标签及元数据校验；包 exports、声明接口和 ABI 3 不变 | npm run typecheck 与 npm test（包含既有原生 addon 构建）；核对请求 metadata snapshot、非法 segment 拒绝、release/forgetConsumed/close。无需为删赋值或转发函数新增镜像测试 |
| B7c · R38，含窄 T04 判据修正 | 两份 IPC integration 文件中 register 后的 3 处 sleep(0.3)；segment_size_benchmark.py 中普通 bytes/dict 结果的 4 处 hasattr/release 探测。并发用例先补线程全部结束及 9 次调用完成的判据，再去对应等待 | register 的同步 native readiness；真实 IPC 数据完整性和清理；保留 GC/业务/跨进程等待以及真实 ResponseBuffer/HeldResult.release；benchmark 的结果持有、计时区间、选路与 close/unregister 保持 | test_zero_copy_ipc.py、test_ipc_buddy_reply.py、test_direct_ipc_control.py；检查启动返回即 ready、重启和重复启动失败路径。benchmark 仅做小负载 bytes/dict 调用 smoke，不运行全矩阵或据此宣称性能提升 |

B7a 的直接验收入口是 `core/transport/c2-ipc/tests/connect_timeout.rs` 中的 route_acquisition_respects_deadlines_and_last_lookup_result，已合并原退避 deadline、cached token live lookup 与末次结果用例；relay 还需 exact_token_ready_to_closed/removed/reregistered 的真实调用拒绝及 callback 为 0。Rust 改动后重新构建 Python native 再运行 B7c，避免沿用旧扩展的通过记录。平台声明以实际 gate 为准，保留既有 Linux/macOS/Windows CI，不把本地 macOS 运行替代跨平台通过。

B7c 的第三处等待位于 `test_ipc_buddy_reply.py::TestBuddyReply::test_concurrent_large_calls`；现有 join(timeout=20) 后仅断言 errors 为空，不能证明工作完成。补完成判据属于防止假通过的测试修缮，应独立核对清理和总超时，不通过增加 sleep 或放宽 timeout 消除失败。其他线程测试和广泛 T04 改造不随本项扩展。

### B8：私有状态收敛

R31 的独立只读复核 run 为 `eba0201a-513f-4df7-817c-ceee4d7bbc56`，固定输入与本轮规划一致。Host 已复核关键构造/释放/锁路径，验收报告并回收审查工作树。下面三项可按保持现有行为的方案各自落地；没有要求把全部额外测试缺口一次性铺开。

| 子项 | 最小设计 | 关键回归与边界 |
| --- | --- | --- |
| B8a · BorrowedPayload | 直接持有 Payload；payload() 直接借用；Drop 仍先 invalidate，再让字段析构 | sdk/rust 的 payload_lifetime 及 codegen generated_rust_sdk；正常、业务 Err、输出编码失败和 panic 出口的 checked view 失效。保持生成接口和 copy-backed 语义，不用字段形状测试代替生命周期测试 |
| B8b · client_config_frozen | 删除 RuntimeState 的重复 bool，以 frozen_client_config.is_some() 派生；公开 getter、setter 与生命周期转移两端的检查同步改投影 | 保留原 mutex、revision、锁外解析后的重验、原始 deadline；配置解析失败不冻结，首次有效尝试即冻结，随后连接失败或 shutdown 都不解冻。复用 client_modes/native_endpoint/endpoint_context/connect_timeout 与 Python RuntimeSession 用例；必要时补非法配置可修正的行为。若不改 revision 机制，不为删除 bool 另造同步 hook |
| B8c · PyEndpointSweep | Option<OpenSweep> 统一 iterator/lease/closed；OpenSweep 字段顺序为 iterator 后 lease；context、budget、batches 留在外层 | close/Drop/unwind 均先销毁 iterator，再归还 projection lease；EOF 或 round_interrupted 不等于公开 close；关闭后 context/protocol 仍可读。复用 endpoint maintenance/context 用例，补“旧 owner 二次 close/drop 不释放新 owner 的 lease”和“EOF 后仍占 lease”；保持 PyO3 借用门禁，不能用 GIL 假定替代同步 |

Held<T> 暂不并入 B8。两个 Option 的稳定状态确实配对，但直接保留 tuple 局部会把 invalidator panic 时的析构顺序从 response→value 改为 value→response。Host 的最小 Rust 探针验证了这一语言机制，日志在 `/tmp/c2-next-batch-plan-20261013/drop-order-probe.log`；它不是实际 transport 释放通过证据。先在原实现补真实 SDK Held<T> 的 invalidation Err、transport Err/组合错误和 unwind 清理顺序，再使用配对 Option，并分别声明局部以保持现有析构顺序。现有 Core 的替身 guard 和 SDK 的 owned-bytes 用例不足以单独证明真实响应租约释放。

### P1 的独立设计与实施入口

| 单元 | 先讨论的具体设计 | 修复前应固定的反例与出口 |
| --- | --- | --- |
| R01 + R02 · 分配身份与唯一写所有权 | 分配器记录精确活分配状态；区分入站原始坐标与 allocator 创建的私有 owner，切片借用绑定 owner。统一评估 SHM 布局、公开 Rust 接口及 FFI 影响；不能只改一个方法为 &mut self 或只隐藏 handle 字段 | R01 非法子区间释放被拒且 bitmap/count/free_bytes 不变，活块不退休或重叠；R02 非法可变别名由 safe API 无法构造，覆盖重复打开同 backing。保持正常读写、trim 原容量释放、generation 与多进程共享路径 |
| R03 · shutdown 发送阶段 | 区分 NotSent 与 DispatchUncertain，覆盖外层 timeout。发送前保持重试；开始控制写后丢确认不重发到同地址的新实例。先确定沿用现有未确认结果还是调整错误投影；ACK 仍只代表 initiate | 发送前暂不可用后成功；部分/完整发送后丢 ACK；原实例退出后同地址替换实例不收到重发；原始总预算不重置。该修复与 B7a 的业务 route acquisition 循环分开 |
| R04 · 开放登记事务 | token 绑定 route UID/具体登记；开放与 OwnerMissing/关闭共享同步边界，并核对待开放状态。保持 shutdown generation；状态锁不跨 callback drain | A 的旧 token 不能打开同名 B；OwnerMissing 与最终开放交错后 callback 为 0；正常 relay 最终发布仍可开放；catalog 与 scheduler 状态一致。先固定真实行为反例，再改事务和替换仅检查签名的测试 |

以上 P1 设计尚待讨论，优先级不因 B7 的简单程度而降低。它们独立提交、独立回归与审查，不混入精简提交。R08 配置错误策略、R23 timeout、R24 旧 transfer 参数、R25 公开适配协议，以及 RouteAck/Connection/Heartbeat 等公开观察面继续沿原清单另行讨论。

## 执行记录

### B1：调度器死计数

仅删除 scheduler.rs 的 waiting_writers、writer_wait_registered 及其维护分支，共 36 行；queue/ticket/notify/pending/active、SHM 载体和公开 snapshot 未变。基线 `6868ef91`，独立审查固定快照 `301e4dcff8a64727536149a1db85f4d9cd8cbb92`，Host 已逐字节确认候选源文件与审查快照一致。

验证：`CARGO_TARGET_DIR=/tmp/c2-slim-implementation-20261013/target cargo test --manifest-path core/Cargo.toml -p c2-server --lib --locked --offline`：168 passed，0 failed，包含 scheduler FIFO/关闭/取消及 buddy_dispatch_passes_shm_request_to_callback_without_inline_materialization。`rustfmt --check --edition 2024 core/transport/c2-server/src/scheduler.rs` 和 `git diff --check` 通过。日志：`/tmp/c2-slim-implementation-20261013/b1-server.log`。

独立审查 run `87bc99c8-6725-4629-9068-cb946ff8d38f` 未发现需修复问题，Host 已验收并回收审查工作树。审查为静态核验，168 项运行证据由 Host 提供。后续 Python 环境准备遇到本机 clang 缺省 C++ 头搜索路径问题，独立于本批 Rust 回归；最终进程级修复与失败尝试见 B2。

### B2：Python 构造不变量

B1 实现提交为 `306a922cd256631f80e20dede0b44815b7db90aa`。B2 仅修改 bridge.py 与 payload_plan.py，保留构造器、frozen、公开签名、错误类型/文案和独立 borrowed 门禁；合计新增 1 行、删除 9 行。独立审查快照 `48bfdf16ba814f88d4a04ae94c88fc87176d8580`，run `464176d5-5a81-4183-906b-d85a3f8c9bee`，无 actionable findings，Host 已验收并核对文件一致。

验证：Python 3.13.3；当前工作树的 Python 源码及本地构建的 `_native.cpython-313-darwin.so`。`C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/unit/test_resource_conformance.py sdk/python/tests/unit/test_input_lifetime.py sdk/python/tests/unit/test_portable_payload_runtime.py sdk/python/tests/integration/test_portable_payload_runtime.py -q --timeout=30`：30 passed。初次命令误写不存在的 test_resource_bridge.py，未收集测试；更正后结果日志为 `/tmp/c2-slim-implementation-20261013/b2-python-r2.log`。没有修改或新增测试；并非宣称所有 borrowed 签名组合都已动态覆盖。

构建环境：本机默认 clang++ 无法发现 SDK 的 C++ 标准头；仅设置 SDKROOT 也失败。限定本轮命令的 `CPLUS_INCLUDE_PATH=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/include/c++/v1` 后，最小编译及 `uv sync --frozen --offline` 成功。CXXFLAGS 的首次尝试被已有空 CMake cache 保留，失败未计入通过。CARGO_TARGET_DIR 为 `/tmp/c2-slim-implementation-20261013/target`；没有改系统/用户全局配置或依赖版本。生成测试使用 Cargo 已锁定的 FastDB 提交 `4f99f86a662b0e950a0dd29800c25a1c9fca4def` 的只读 sibling fixture 链接。

### B3：生成服务复用已接纳 release

B2 实现提交为 `d4d0e9012a653e2f867dbfe128d28ff707325453`。B3 仅将生成 service_definition 的第二次 contract_release_ref() 改为已有 release.reference()，公开签名和 MethodBinding.index 类型未变。独立审查快照 `704090018e6896f6cfdfa9efdc5a9120f4456c6e`，run `640dc776-b95e-4476-bef5-44b2e356e5e0`，无 actionable findings，Host 已验收并核对文件一致。隐藏的 debug admission probe 仍按实际接纳次数计数，重复工作由两次减为一次；release 模式不维护该计数。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-codegen --lib --test generated_rust_sdk --locked --offline`：lib 2 passed、generated Rust SDK 4 passed，含真实生成消费者编译/调用、错误 payload 类型拒绝及 portable fixture 编译。targets.rs 的 rustfmt 和 diff check 通过。日志 `/tmp/c2-slim-implementation-20261013/b3-codegen-r2.log`。生成 Rust 源码一行及其内容哈希预期改变；canonical 合同身份/元数据、公开接口及运行语义保持。

初次 Core 构建因 packaged fastdb-sys 禁止 source mode 而失败；随后运行仓库 prepare_fastdb_sdk.py 校验官方 CoreSDK，并限定该命令使用 FASTDB_PAYLOAD_LINK_MODE=system、FASTDB_PAYLOAD_SYSTEM_LIB_DIR 和 DYLD_LIBRARY_PATH。验证包为 `fastdb-core-0.2.1-aarch64-apple-darwin.tar.gz`，源 SHA `4f99f86a662b0e950a0dd29800c25a1c9fca4def`，归档 SHA256 `5f346177032c3c13838449db9ef30f43f3bd2b6d91241d28d2ac1edcd025ad33`；未改依赖版本。

### B4：局部内存与分块不变量

B3 实现提交为 `dcb72f590c6cf6b9172011a8b5798af224975ff7`。本批 4 文件新增 5 行、删除 15 行。bitmap 的循环上下界恒等；空 handle 保留入口首次完整校验；retention 的私有 permit charge 始终包含在同锁域总量中，全域加法通过即保证局部加法不溢出；chunk 只删除未读创建时间，活动时间与全部断言保留。独立审查快照 `a7113e1d863d9b1fdb0fc1df566fd246020b5fb8`，run `4fecae0c-6c4a-43bc-a927-7c1c031a5b3f`，无 actionable findings。Host 已核对全部文件一致、验收并回收审查工作树。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-mem -p c2-wire --lib --locked --offline`：177 + 146 passed；`cargo test --manifest-path core/Cargo.toml -p c2-server --lib --locked --offline`：168 passed，包含 SHM 无 inline materialization 回归。各命令设置本轮 CARGO_TARGET_DIR、空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS。4 文件 rustfmt 与 diff check 通过。重建 Python 原生扩展后，B2 同一组 4 文件测试再次 30 passed。日志分别为 `/tmp/c2-slim-implementation-20261013/b4-memory-wire.log`、`b4-server.log`、`b4-python-r2.log`。

Python 原生扩展初次 system mode 重建成功，但默认加载因缺少 libfastdb 的 LC_RPATH 失败，未收集测试。使用该 SDK 原有的 Git 依赖 source mode、限定进程的 CPLUS_INCLUDE_PATH 重新执行 `uv sync --frozen --offline --reinstall-package c-two` 成功；随后默认加载及上述 Python 回归通过，没有新增加载器配置或修改依赖。Core codegen 的已验证官方 SDK system mode 与 Python 原有源码构建分别遵守其既有依赖边界。

### B5：内部孤儿 helper

B4 实现提交为 `b4c2a4ae7a084db00fc073e9e49500bba085ce2e`。本批 4 文件仅删除 123 行，包括 process_guard 的唯一专属测试及两个失去用途的 import；没有新增实现或测试。Host 与独立审查均检查引用、动态加载、导出及活动指导，未发现运行消费者。native config、真实 manifest/receipt 校验和两条实际进程清理路径保留。独立审查快照 `63f3918f05617642d7844de02321220b609961d2`，run `7497f52d-994a-4be0-a67f-8605ef4c2fa3`，无 actionable findings。审查只读沙箱拒绝 here-document 临时文件，因此补充 AST 比对由 Host 执行：扣除明确删除的定义、专属测试和 imports 后，保留 AST 完全相同，候选文件与审查快照字节一致；Host 已验收并回收审查工作树。

验证：`C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest tests/repo/test_local_registry.py tests/repo/test_package_consumers.py tests/repo/test_local_candidate_manifest.py sdk/python/tests/unit/test_ipc_config.py sdk/python/tests/unit/test_runtime_session.py::test_native_runtime_session_projects_client_ipc_overrides_copy sdk/python/tests/unit/test_runtime_session.py::test_native_runtime_session_resolves_client_ipc_config -q --timeout=30`：113 passed，diff check 通过。日志 `/tmp/c2-slim-implementation-20261013/b5-regression.log`，结构比对 `/tmp/c2-slim-implementation-20261013/b5-structure-check.json`。实际 package_consumers.py 与 source_validation.py 字节未变；该结果不宣称 Windows 进程树终止已获动态证明，也不为仓外私有导入建立兼容承诺。

### B6：退役发布脚本

B5 实现提交为 `77b3d9bd73c2b6163706891d7f2bdd25710ee187`。本批 6 文件只删除 410 行，包括两个旧脚本、12 项专属测试、CI 中两个旧选择和 4 个旧路径断言。两个现行发布 workflow 与 promote_release_candidate.py 相对基线字节未变。独立审查快照 `49a132425f546d6d8262ee6db0fa914f46e3e1b0`，run `0aea2c40-e715-4dc5-8640-23e571806d8e`，无 actionable findings；Host 已核对所有候选路径与快照一致、验收并回收审查工作树。

验证按更新后 `.github/workflows/ci.yml` 的 workflow-policy 完整 12 文件选择列表执行，使用 `C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-project --offline --with pytest --with pytest-timeout --with packaging --with pyyaml --with maturin pytest --confcutdir=tests/repo ... -q --timeout=30`：506 passed，0 failed，53.91 秒；唯一 warning 来自故意构造重复 ZIP member 的负例。日志 `/tmp/c2-slim-implementation-20261013/b6-workflow-policy.log`，完整命令 `/tmp/c2-slim-implementation-20261013/b6-regression-command.txt`。包含 fake API 和实际 workflow shell 配合 fake gh 的 promotion 行为测试；本地通过不等于 hosted CI 或真实发布。diff check 通过，活动源码/测试/CI 中已无旧脚本引用。

### B7a：IPC acquisition 重试尾部

实现基线 `60bac6541e2747adaa0c22e6f256adb34fe76589`。三个循环改为消费固定延迟 iterator，在无下次重试时返回当前实际错误，生产代码净减少 35 行。首次提交新增一个真实 LocalStream 回归，以 6 个场景验证两入口的末次 Ready/NotFound/ContractMismatch；该测试先在旧实现通过，再在候选上通过。它不单独证明 lookup 与 bind 之间的竞争，或 watch/compaction 的恢复行为。

验证：`cargo test --manifest-path core/Cargo.toml -p c2-ipc --features test-support --lib --tests --locked --offline`：203 + 9 passed；`cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib relay::state::tests --locked --offline`：45 passed，含旧 token 拒绝、owner 替换与共享在途调用。rustfmt/diff check 通过。各命令用既有 CARGO_TARGET_DIR 与空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS；日志 `/tmp/c2-slim-round2-20261013/b7a-baseline.log`、`b7a-ipc.log`、`b7a-relay.log`。后续 Python 验证前已以既有 source mode/CPLUS_INCLUDE_PATH 重建 native，日志 `b7-native-build.log`。

buddy 初始 health 正常，但提交时 launcher 返回 UPGRADE_IN_PROGRESS，未创建 run、未进入路由。按原生只读审查替代，由 `/root/b7a_independent_review`（gpt-6-sol/high）完成独立审查，无 actionable findings。Host 及 reviewer 起止均核对固定 diff SHA256 `5c02f2d9b894edbae86ae1ecafe27e0ccb35f8760ac9976594f1f84a5e029d59` 和两个文件 hash，一致后验收；记录 `b7a-review-manifest.json`、`b7a-independent-review.txt`。审查未执行测试，不将源码等价性结论当运行证据。

### B7b：TypeScript 内部转发

B7a 实现提交为 `e4b947a4c57ccbdc15f07e1f8aed331b5aa8c4c0`。本项唯一实现文件 index.ts，新增 1 行、删除 11 行。独立审查 run `64e48cca-ff00-49bd-86e3-7046d400e398`，固定快照 `f8de43786eda0a2d49542e6ba3f471101d65094d`，无 actionable findings；Host 核对字节一致后验收回收。buddy 此时已恢复为 0.32.0。参数绑定/closure、getter 顺序与次数、this、错误类型与 label、冻结和释放均保持；删除 helper 会改变调试栈帧，不承诺源码反射文本不变。

验证：在 TypeScript 包目录运行 `npm run typecheck` 成功；以既有 CARGO_TARGET_DIR、CARGO_NET_OFFLINE=true 和空 C2_ENV_FILE/C2_RELAY_ANCHOR_ADDRESS 运行 `npm test`，含原生 Rust/C addon 构建：59 passed、0 failed、1 skipped（需要真实 Windows 的 current-SID 测试）。Node v25.8.1、npm 11.11.0。生成 index.d.ts 与删除前逐字节一致，SHA256 `a5f6e769498bbd868f1090a44e3fef7e21ece469e7b7b0e024cd409d7167b0cb`。日志 `/tmp/c2-slim-round2-20261013/b7b-typecheck.log`、`b7b-node.log`，diff check 通过。

### B7c：就绪等待与测试完成判据

B7b 实现提交为 `d24bc2406d2721cb11d35e4aa17289736c6260cf`。本项 3 文件新增 12 行、删除 16 行；3 处等待移除，4 处伪释放探测移除，并发用例改为同一 20 秒 join 预算、三个 worker 全部退出且各完成 3 次调用，proxy 使用同一关闭路径的 context manager。20 秒仅界定 join，不宣称能取消卡住线程或界定全部注册/teardown。独立审查 run `aa5ba7a6-f648-4e6e-9994-c7beefb73652`，固定快照 `5e80d0fbc731e29863fb0cae59cf4b4a7c4e8486`，无 actionable findings；Host 核对候选字节一致后验收回收。

验证：重建 native 后，原三文件基线 33 passed；保留旧等待加入完成判据后，该并发用例 1 passed；删等待后 `C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/integration/test_zero_copy_ipc.py sdk/python/tests/integration/test_ipc_buddy_reply.py sdk/python/tests/integration/test_direct_ipc_control.py -q --timeout=30`：33 passed。另实际调用 benchmark 两个入口（64 byte、2 MiB segment、原有轮次）均完成；只作 smoke，不使用中位耗时作性能结论。c2-server::buddy_dispatch_passes_shm_request_to_callback_without_inline_materialization 定向回归 1 passed。日志位于 `/tmp/c2-slim-round2-20261013/` 的 b7c-baseline.log、b7c-completion-baseline.log、b7c-regression.log、b7c-benchmark-smoke.log、b7-shm.log。diff check 通过。

### B8a：BorrowedPayload 直接持有

B7c 实现提交为 `b4e916c9aae66a270fc9ec7ed66f41fad0aa01e8`。本项唯一源码 payload.rs 新增 4 行、删除 9 行，删除只有构造 Some 和 Drop take 的 Option 状态；公开借用和生成接口、先失效再析构及 request lease 顺序保持。独立审查 run `36b12c3f-e10a-49a1-8559-fc9ba5582f07`、快照 `26c9c0fad493031c4b21b5ec10d70c4afff1d945`，无 actionable findings，Host 核对字节一致后验收回收。丢弃 invalidation Err 的原策略未改变；顺序等价不等于失效必然成功，也不承诺无 repr 的 Rust 类型布局不变。

验证：`cargo test --manifest-path sdk/rust/Cargo.toml --all-features --test payload_lifetime --locked --offline`：4 passed；`cargo test --manifest-path core/Cargo.toml -p c2-codegen --test generated_rust_sdk --locked --offline`：4 passed，含真实生成消费者的业务 Err、编码失败和 panic 出口。使用第一轮已验证官方 FastDB SDK 的 system mode、限定的 DYLD_LIBRARY_PATH、既有 target 和空 C2 环境；没有修改依赖。rustfmt/diff check 通过。日志 `/tmp/c2-slim-round2-20261013/b8a-payload-lifetime.log`、`b8a-generated.log`。

### B8b：客户端冻结事实唯一化

B8a 实现提交为 `e9d0650c882fa95dd87d0b21a8ed037683321bed`。生产 session.rs 新增 4 行、删除 7 行，frozen_client_config 的 Option 成为唯一冻结事实；public getter、setter 与 lifecycle handoff 两端仍在原锁中判断，revision、提交和 pool 路径均保持。首次提交新增真实 Runtime.connect 测试 42 行，先在旧实现通过：非法配置不冻结/不创建 outgoing domain，可由 setter 修正；后续失败仍冻结并拒 setter。第二次 is_err 只证明配置提交后的失败，单独不足以定位为 OS 阶段。

验证：Core lib 104、client_modes 15、connect_timeout 5、endpoint_context 12、native_endpoint 4，共 140 passed；endpoint_context 两个 ignored 是由父测试以 --ignored 启动的隔离子进程入口。命令为 `cargo test --manifest-path core/Cargo.toml -p c2-core --lib --test native_endpoint --test client_modes --test endpoint_context --test connect_timeout --locked --offline`，带既有 target 和空 C2 环境。source mode 重建 Python native 后，`uv run --no-sync pytest sdk/python/tests/unit/test_runtime_session.py sdk/python/tests/unit/test_ipc_config.py -q --timeout=30`：107 passed。rustfmt/diff check 通过。日志 `/tmp/c2-slim-round2-20261013/b8b-baseline.log`、`b8b-core.log`、`b8b-native-build.log`、`b8b-python.log`。

独立审查 run `2a590608-7558-458b-a75f-6f2293d5df11`、固定快照 `e46b3ab5dbf8184051b2781f545ae3221b7c007a`，无 actionable findings；Host 核对字节后验收回收。审查保留 endpoint 冻结与 client config 冻结的区别，以及 Python client_ipc_config 原有重新解析投影，未顺带做语义修改。

### B8c：sweep 的配对 owner

B8b 实现提交为 `2949cd719621ae9bb59a74cbb19eff31e5d0c44e`。生产 endpoint_ffi.rs 新增 22 行、删除 23 行，OpenSweep 将 iterator 与 projection lease 配对，PyEndpointSweep 只持一个 Option；成员声明顺序保证 iterator 先析构，lease 后释放。closed/ensure_open/repr 从 Option 派生，next_batch 先接纳开放 owner 再验证预算；EOF/round_interrupted 仍不自动关闭外层。context、budget、batches 留在外层，公开投影与 PyO3 借用形态保持。

强化旧 owner 重复 close/Drop 不释放新 owner lease，并新增 EOF 后仍占 lease、close 后属性可读和 closed 错误优先回归。第一次旧实现验证失败于“空 scope 一批必达 EOF”的夹具假设；默认目录仍被分批遍历。第二次不必要的 server 夹具启动返回 ENOENT。最终夹具直接使用已有、自有空 tmp_path 的 native context（Windows 用默认原生 context）与空 scope，无需启动 server 或选择历史端点；旧实现两用例均通过。新 owner 先到 EOF，避免 Core lease 遮蔽 projection lease 的错误释放。首次提交的测试文件新增 43 行、删除 4 行。

验证：既有 source mode/CPLUS_INCLUDE_PATH/target 下重新构建 Python native 成功；`C2_ENV_FILE= C2_RELAY_ANCHOR_ADDRESS= uv run --no-sync pytest sdk/python/tests/unit/test_endpoint_maintenance.py sdk/python/tests/unit/test_endpoint_context.py -q --timeout=30`：111 passed、5 skipped（Windows named-pipe 平台合同用例）。rustfmt/diff check 通过。日志 `/tmp/c2-slim-round2-20261013/b8c-baseline.log`、`b8c-baseline-r2.log`、`b8c-baseline-r3.log`、`b8c-native-build.log`、`b8c-python.log`；前两次失败不计为通过。本地未执行 Windows 或 free-threading 并发验证。

独立审查 run `4625622f-263e-4d35-ac0b-f1c90b264649`、固定快照 `8bda12ac2b50f490aca9be3a5be3ddb4688ef8a8`，无 actionable findings；Host 核对所有候选字节一致后验收回收。审查为源码证据，与 Host 运行结果分别记录。

### 测试收敛纠偏

基线 `8bb1d133c69693cd322d999f487f9f19fd17dc68`，仅改三个测试文件，净减少 112 行。IPC 三个用例共用 Peer/pool/清理，以普通和 exact-token 两入口各自验证 live 查询暂停、退避 deadline、末次 Ready/NotFound/ContractMismatch；响应保留 Rust 类型到发送时编码。Runtime 合并非法配置修正与有效配置后连接失败仍冻结的用例，保留显式有效 override、stats 和 ClientConfigFrozen。Python 合并 close/EOF/旧 owner 重复 close/Drop、关闭后属性和错误优先的生命周期断言，保留独立的并发、context exit 和 abandoned Drop 用例。

定向验证：IPC connect_timeout 7 passed（其中表内 10 个场景），Core native_endpoint 3 passed，Python endpoint_maintenance 59 passed，relay state 45 passed；rustfmt/diff check 通过。命令沿用上述 target 和空 C2 环境；日志在 `/tmp/c2-slim-round2-20261013/correction-{ipc-final,config-final,sweep,relay}.log`。测试名和数量因合并变化，不能据此相加或推断覆盖等量。

首次纠偏审查 run `f6c75a41-ff9f-45e7-a9da-04aa74f9e10c` 指出直接删除 IPC 末次结果用例会丢失独有保护，Host 采纳并改为合并用例。重审 run `7262da2f-f19b-40ed-9775-bcac9ca04daf` 因 serverOverloaded 失败，已结案回收，不计通过；随后原生独立代理 `/root/correction_final_independent_review`（gpt-6-sol/high）完成三个测试文件的窄审，无 actionable findings。Host 核对审查起止及最终三文件 diff SHA256 均为 `1c540ef6651b990dbd207fa4dd70f2116d6bf1494ab638ac7ff6348e7b4a3468`；运行证据与源码审查分别记录。

## 第三轮：登记与 relay 整体收敛

用户批准按 R11 → R39 → T02 顺序实施，每项同时收敛生产与测试，回归和独立审查完成后再推进。R22 基准清理顺序、R10/Python 快照、R30 公开配置类型及 P1 仍独立讨论；relay journal 保留。

R11 基线 `b9936f0aa18bb4319139e2e48fa6f72565cb58b4`，5 个 Rust 文件净减少 191 行（含内嵌测试）。删除 RuntimeRouteSpec、双规格构造及 Session 对账；同一 BuiltRoute 派生一次 relay expected。静态 wire 校验收口到 BuiltRoute 私有构造器，handle 由同一 scheduler 派生；reserve 保留动态状态检查。两个专属失配测试及双规格夹具退出，三个非法输入用例改测构造拒绝，未新增测试框架或公开 API。

R11 验证：Core lib 102、host_routes 5、client_modes 15、Server lib 168 passed；source mode 重建 Python native 后，runtime_session、runtime_session_lifecycle、core_transport_parity 首次 73 passed / 1 skipped（缺少可发现的 c3），构建并用 CTWO_TEST_C3_BIN 指定当前 c3 后 parity 文件 3 passed，补齐该跳过项。独立审查 run `9ede0945-0e80-4da2-b0f7-147cd0e140cd`、输入 `a9924810ca04f8500c6c53af566ee99a518b41f7` 无功能性阻塞；Host 清掉审查/编译器指出的五处失效测试导入后重跑 Core/Server lib，无 warning，并核对除此以外五文件与快照一致，审查已验收回收。日志和固定差异证据在 `/tmp/c2-slim-b9-20261013/r11-*`；本地 macOS 验证不代替 Windows、hosted CI 或二进制 ABI 验证。

R11 实现提交为 `e20a614cd77f817bf33cc2d7fb1b198f73e6ae5d`。R39 在此基础上移除 RouteCommand/execute 及混合成功结果，五种操作仍通过同一个 RouteAuthority 执行，登记/注销使用各自的成功结果；公开结果、错误映射、锁和清理顺序保留。测试迁移调用入口并删除恒定成功标签断言，4 个文件净减少 97 行。

R39 变更前后 `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib --locked --offline` 均 427 passed；重建 c3 并指定 CTWO_TEST_C3_BIN 后 Python core_transport_parity 3 passed。独立审查 run `883a09e3-76ab-49b9-b921-b8627b49450c`、输入 `5cdc6a9a9fa86ecaac25f39b1c9c8f1caa6fa28e` 无 actionable findings；Worker 可选机械对比未执行，Host 已在内存核对五个操作函数体除成功结果名称/unit外一致，并验证四文件与固定快照逐字节相同，审查已验收回收。日志及 manifest 在 `/tmp/c2-slim-b9-20261013/r39-*`；本轮未重建不含 relay server feature 的 Python native。

R39 实现提交为 `2946340d`。T02 在此基础上删除两个 cfg(test) name-only resolve 入口和专属源码签名检查，迁移 102 处调用：选路/排序/活性/投影走生产 resolve_matching，拒绝写入、撤回和保留状态直接观察存储；沿用现有夹具，6 文件净减少 107 行。Host 在内存核对运行时代码前缀，除 cfg(test) helper 删除与一行注释外逐字节一致。

T02 完整 relay lib 426 passed（退出一个旧源码检查，原为 427）；独立审查 run `78e1abf7-7553-47a3-be6c-521608e22112`、输入 `2bc9e7bd6cde0fa2281358e3ecc1432d86d572a5` 指出三处旧协议拒绝用例丢失隐含 peer 活性断言。Host 按意见显式检查 peer_is_alive，保留存储检查，其中一处替换较弱 has_peer；修后 peer_handlers 27 passed。六文件已核对等于快照加这三处指定断言，审查已验收回收；没有把未执行的 Worker 机械比较当通过。日志及 manifest 在 `/tmp/c2-slim-b9-20261013/t02-*`。

### B10：IPC overrides 私有投影去重

执行基线为 `87189c489da10d925704210e199a1c068657d33e`。本批选择 R30 的私有 PyO3 投影子项：`config_ffi.rs` 抽出 server/client 共用的 16 个 base 字段写入，沿用现有 resolved config 的分层方式；flat 字段仍按原顺序覆盖。公开配置类型、字段、解析与冻结逻辑均无改动，R30 的公开类型重构仍待讨论。生产代码净减少 40 行。

两项 RuntimeSession 配置复制测试合并为 server/client 参数化，保留原输入修改隔离，并补强 False、None 省略与返回字典副本隔离；三项内存预算 roundtrip 测试合并为正常值、零值、U64_MAX 参数化，保留双端的全部原输入与断言。两个测试文件净减少 38 行，3 个非文档文件合计净减少 78 行；没有新增测试框架、宏或依赖。

首次 `uv run --frozen --offline` 因自动重建 FastDB 缺失 C++ 头文件路径而在测试启动前失败。合并后的 `test_ipc_config.py`、`test_runtime_session.py` 在原已安装扩展上 107 passed；随后使用 source mode、既有共享 target 与 macOS SDK C++ include 路径重新 `uv sync --frozen --offline --reinstall-package c-two` 成功，候选扩展上相同两文件 107 passed，`git diff --check` 通过。Host 将新 helper 内联展开，整份 `config_ffi.rs` 与基线逐字节一致，覆盖字段次序、base/flat 优先级及错误传播的等价核对。

独立审查 run `412874ec-43ab-42e4-a261-c6c9fb62f5d6`、固定输入 `f10e0f0c99f08a137b5ac14d56fcf269e8a1e4f9` 无 actionable findings；Host 已核对三个文件与审查输入的 SHA-256 一致并验收回收。日志、机械对比与 manifest 位于 `/tmp/c2-slim-b10-20261013/r30-*`。验证范围为本地 macOS 的 native 构建与相关 Python 回归。

### B11：FULL_SYNC 的接纳与合并收敛

执行基线为 `4493724779455bea54c817c1e63b59220a9b18b7`。本批选择 R28 的 FULL_SYNC 子项：`ValidatedFullSync` 的字段私有，首次接纳经 `TryFrom<FullSyncEnvelope>` 完成，已完成静态字段、hash 和 owner 私有字段接纳；`merge_validated_snapshot` 直接消费它，退出 raw 合并入口及同值重验。缺失 owner、当前 peer 活性、自身/local 路由保护、replacement 完成后提交、tombstone 时序继续保留；替换后的 URL 来自已经验证的 snapshot peer。生产代码净减少 38 行，公开 API/ABI 和 wire 形状无改动。

测试经一个短 fixture helper 走真实 Envelope/ValidatedFullSync 接纳后再合并。六个非法快照用例合并为六种输入场景并统一检查路由、tombstone、peer 无部分修改；owner 私有字段断言并入完整 snapshot roundtrip。缺失 owner、peer 活性、URL 更新和 tombstone 新旧顺序用例全部保留并迁移入口。测试净减少 90 行，本批非文档代码合计净减少 128 行；R28 的 digest/envelope 其他子项尚未处理。

基线完整 relay lib 426 passed；先只迁移测试、保持原生产实现时，route_table 定向 55 passed；随后候选 `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib --locked --offline` 为 420 passed，`git diff --check` 通过。总数减少 6 项来自六拒绝用例合一及 owner 字段用例并入 roundtrip，原行为场景保留。Host 与独立审查均核对 replacement 提交、tombstone 和 peer 更新后半段与基线逐字节一致。验证范围为本地 macOS 的 c2-http relay feature；未据此声明跨平台或发布通过。

独立审查 run `011ceac7-fa53-4e7b-ad65-49f099058d0c`、固定输入 `bc0993985036e66f3408e82f8a026f890ea8d976` 无 actionable findings；Host 已复核构造不变量、动态状态边界与测试迁移，确认源文件 SHA-256 等于审查输入并验收报告。日志和 manifest 位于 `/tmp/c2-slim-b11-20261013/r28-*`。

### B12：Digest 消息表示与应用逻辑收敛

执行基线为 `557bfd8a`。本批继续 R28：`ValidatedRouteStateEnvelope` 以 Message/DigestDiff 互斥表示取代原始消息加可选字段副本，删除 HTTP 整体 clone、Active/Deleted 影子结构和二次字段搬运。校验时直接移动 owned 字段，Active 持有最终 RouteEntry，Deleted 仍保留原字段直到应用时才生成 tombstone。hash helper 已先接纳同一批静态字段，因此退出其后的重复语法检查；协议、期望 sender、字段、sender/hash 一致检查顺序保持。

HTTP 与两个后台消费位置共用 `RouteAuthority::apply_digest_diff`，继续执行原 announce/withdraw 操作。Host 和独立审查均确认这两个操作及 trusted_peer_owner 与基线逐字节一致，保留当前 peer 活性、owner 和当前 URL 核验；wire 类型、协议常量、外部接口与各 endpoint 返回行为不变。生产代码净减少 99 行。

两项 sender/CRM 拒绝测试合并为两个场景，均放入合法前缀验证整批拒绝；既有删除用例追加相同 digest 在 announce/heartbeat/leave 入口返回 OK 且不改状态的检查。active/deleted hash replay、旧协议、有效应用、URL 和 dead-peer 覆盖保留。测试净减少 10 行，四个非文档文件合计净减少 109 行。

基线完整 relay lib 420 passed；测试先在旧实现上 peer_handlers 26 passed；候选 `cargo test --manifest-path core/Cargo.toml -p c2-http --features relay --lib --locked --offline` 为 419 passed，一项数量差来自两拒绝用例合并。之后仅格式化所改函数，Host 在内存确认空白外内容一致；`git diff --check` 通过。独立审查 run `44f7b584-5124-4cf7-8f10-f77a2afa91f8`、固定输入 `1f280fb7d13234a7d553fcadca962346b29cabfb` 无 actionable findings，四文件与输入 SHA-256 一致并已验收。日志、差量和 manifest 位于 `/tmp/c2-slim-b12-20261013/r28-*`；验证范围为本地 macOS 的 relay feature。

### B13：私有握手快照与墓碑副本收敛

执行基线为 `5f152c62f3c86d84a38d914842740a8f764c3974`。本批处理 R34 的未读私有副本：删除 IpcClient.server_segments、RouteDirectory.catalog_revision；服务端 removed 墓碑仅保存实际查询的 route UID，contract/revision/reason 仍由 watch event 保留；TypeScript 握手不再构造和保存未读的 routes/methods 数组。全部握手字段读取、数值/文本校验、错误顺序与尾随字节检查保留，SHM metadata、capability、真实 revision、live lookup、watch/compaction 和旧 token 校验不变。

catalog 的 list/stale lookup/contract mismatch 三套相同夹具合一，保留原输入条件和 identity/revision/hash golden，并以完整记录相等断言覆盖返回内容；Removed 事件显式验证 UID/revision/reason。生产净减少 39 行、测试净减少 11 行，三个非文档文件合计净减少 50 行，无新增常驻测试框架。

Rust 基线 IPC lib 203 + integration 7 + server 168 passed；仅合并测试时 catalog 5 passed；候选 IPC 203 + 7、server 166 passed，减少两项为夹具合并。TypeScript 初次基线因缺少编译器依赖而失败，按锁文件 npm ci --ignore-scripts 后基线/候选 transport contract 均 7 passed，候选严格生成类型检查 1 passed。临时脚本使用两版完整 asset 的真实 decoder 对照 1361 个确定性输入，324 accepted、1037 rejected，保留的元数据或异常类型/完整文案一致；范围包含所有前缀截断、逐字节扰动、超范围 revision 和尾随字节，明确忽略退出的私有 routes 字段，脚本未进仓库。

独立审查 run `578ec828-491e-42dc-ae7a-4efda9de4473`、固定输入 `0401fca2d2aefb4ab9f1852c15b6c5b1bc8ea7a2` 无 actionable findings；Host 已核对三文件 SHA-256、实际回归及私有边界并验收报告，`git diff --check` 通过。日志、对照脚本和 manifest 位于 `/tmp/c2-slim-b13-20261013/r34-*`。验证限本地 macOS；R34 的公开协议面和其他构造优化仍未处理。

### B14：R22 基准工具复用既有 SDK 生命周期

执行基线为 `21cb8f63`。本批只修复和精简两个 Python benchmark 及相关测试；不计作生产运行时性能或架构收益。用户明确批准复用现有 SDK 登记、精确撤回和 drain-first 收尾，不修改 Core 的一致性、生命周期、公开 API/ABI 或 relay journal。

两个脚本通过 44 行 `_relay_support.session` 临时设置 anchor 并消费 SDK shutdown 结果，退出手写 register/unregister 报文、额外 IPC 身份回连、重复登记布尔及局部撤回。移除的调用实际依赖已不存在的 CoreClient.server_instance_id / RuntimeSession.release_ipc_client；旧 unregister 报文还因缺 instance/UID/revision 被 422 拒绝。真实 QPS 回归另确认旧请求缺 route token 而 400；现在由显式 HTTP SDK 连接获得既有 client.route_uid/revision，hey 继续发送完整 CRM 指纹和这两个 token，单参数 pickle 值对齐 SDK。计时范围及强制 HTTP 数据路径保留。

独立首审发现未完成 shutdown 仍冻结 Host anchor；无条件恢复会触发 native 错误并使 Python 配置与 native 不一致。Host 用真实 native 在途调用在原候选复现，修复为仅完成后恢复；未完成时保留冻结配置，有原异常则保留并诊断，否则终止 benchmark，避免污染后续测量。模拟 incomplete/shutdown 测试由两个真实 pending 场景替代。QPS payload 清理归到本轮成功写入后的作用域，离线预检不再删除既有文件。

重复报文模拟退出，保留 header、真实 HTTP 路径、输出和 descriptor 覆盖；复用现有 c3 fixture 验证两个小负载 benchmark、anchor 恢复和路由撤回，未新增常驻测试框架。工具代码净减少 212 行，测试净减少 7 行，四个非文档文件合计减少 219 行。

当前 c3 和 Python native 已从本批基线重建。最终 benchmark 13 + examples syntax 9 = 22 passed，无跳过；三个 benchmark 模块另以 Python 3.10 compile 通过。真实 hey 预热报告 496 次、正式 3 次响应，全部 200，单次响应内容为 Hello, Benchmark!，路由及自有 payload 文件已清除。真实 session 探针先调用 manager、后登记 builder，以 idle_timeout=1 的实际 sweeper 约 5 秒后确认驱逐；两条 route identity 保留，8 个并发 builder 调用和 manager 调用通过，最终精确撤回。既有 Rust production_idle_sweeper 回归也为 1 passed，覆盖活跃请求保留和并发共享重连。上述为本地 macOS 正确性证据，不构成吞吐提升或跨平台验收。

首审 run `dea9d4fc-5f4c-450b-95c4-b9d2e91ca51b` 的冻结 anchor 问题已复现并修复；第二轮 run `d3e92af8-c6a5-44f6-9432-b996c0fbca03`、固定输入 `f5be7af91639ec97801352cce50637a05a87b172` 无 actionable findings。Host 核对四文件 SHA-256 与固定输入一致，两份审查报告已验收并回收工作树。`git diff --check` 通过。日志、探针、红绿结果和哈希清单位于 `/tmp/c2-slim-b14-20261013/`。R22 的其他脚本 selector 项不在本批；后续优先回到生产运行时重复逻辑和复杂状态。

### B15：R30 私有 IPC override 赋值收敛

实现提交为 `5dbd6b26`，执行基线为 `7702233c0f868a30f2c4de584375f0a279e19468`。本批按确认方案，将 `resolver.rs` 的三份公共字段赋值函数收敛为一个模块私有宏；16 个字段的来源、目标、类型和赋值顺序均逐项核对一致。两个参数各求值一次，借用限定在独立块内；server/client 均保留先 base 后 flat 的覆盖顺序。逆向文本比较确认，除三个 helper 及四处调用外，其余生产代码逐字节一致。R08、公开配置类型、API/ABI、环境错误、派生溢出检查和生命周期策略均未改动。

在既有 `budget_limits_explicit_overrides_beat_env` 用例内增加两角色 base/flat 冲突、false/零值覆盖和派生结果断言；保留原有 None 及环境预算断言，不增加测试文件或测试框架。扩展后的用例先在原实现上 1 passed。生产代码净减少 103 行，测试增加 16 行，非文档代码合计净减少 87 行。

候选 c2-config lib 135 passed，relay upstream 实际配置传播及重连 4 passed；重建 Python native 后，`test_ipc_config.py` 与 `test_runtime_session.py` 共 107 passed，合计 246 项通过、无跳过。临时 native 新旧对照包含 server/client 各六组输入：默认值、全部 16 个字段、false/zero/None、有效 override 遇到非法数值或布尔环境值，以及派生溢出；12 组结果及异常类型、完整文案一致。对照脚本未进仓库，验证范围为本地 macOS。

独立审查 run `5bc00a2b-1c0a-4a9f-a6c5-440c2810773e`、固定输入 `b9b455b9f0f39311cb5c1685dd5a5fa2fe799be3` 无 actionable findings。Host 复核源码 SHA-256 与审查输入一致，核对实际回归后验收并回收审查工作树。日志、16 字段清单、差量审计及新旧对照位于 `/tmp/c2-slim-b15-20261013/`。

### B16：测试端点收尾与存量精确回收

执行基线为 `2f518d3ff86cc9ecad270d2d373f185c2238965a`。用户批准修复测试收尾，要求复用现有 close/reap、验收本次端点零残留，本轮不用 buddy。原生 Drop 在 gate 争用时有意释放句柄并保留记录；缺口是部分测试将 Drop 或 runtime 退出当成完整收尾。隔离诊断中，串行 Drop 32 对连接无残留，8 路并行 Drop 256 对留下 50 组 socket/lease；同样并行显式 close 为零，50 组均可用原生凭据精确回收。

`LocalStream::pair` 在连接尝试结束后通过 spawn_blocking 完成既有 close；普通 listener/codec/peer 夹具检查关闭结果，独立 race server 在 runtime 退出前等待 shutdown。deadline Peer 的取消会移交 listener，故在 readiness 保存 credential，在停止并 join 后对该实例有界 reap。真正测试 Drop、进程异常退出和 stale credential 的场景保留。Python namespace 夹具检查完整终态中的 runtime_barrier_error，精确回收并观察 absent；退出没有消费者且构造接口过期的 hello_server 及其专属准备链，共删除 51 行。生产编译的 pair 辅助函数净增 8 行，测试净增 48 行，非文档合计净增 56 行；本批为收尾缺陷修复，未引入测试框架或改变 Core 生命周期策略。

定向 Rust 回归为 IPC 203 + deadline integration 7、local lib 94 + integration 9 + 12、server 166，合计 491 passed；原有需 root 启动第二 UID 的 1 项测试 ignored。最终 Python endpoint maintenance/server/direct control/heartbeat 共 89 passed。三次运行均比对默认端点目录前后条目及 inode，新增 socket/lease 为零。首轮原生子代理审查指出 inspect 的 WouldBlock 重试遗漏及测试 README 的旧夹具引用，已修正；私有目录中以真实 flock 在 shutdown 前后制造争用，原单次 inspect 断言失败，修复后分别观察 present/absent 且无残留。临时探针未进入仓库。

261 个历史端点已逐项匹配 24 组 Rust 测试源码前缀、原租约地址及 device/inode/incarnation，再与 native inspect 凭据核对；原生 reap 返回 261 个 reaped，原 522 个 socket/lease 路径全部消失。没有使用 unlink 或目录清扫。另有 12 个无 owner 记录的旧空 lease 未纳入该清单，保留原状。最终默认根 socket 数为零，本次测试新增端点条目为零，不将命名空间 gate/marker 或未知历史条目计为已清理。

独立审查由原生子代理完成；第二轮对首审后改动复核无 actionable findings，其他已审源码哈希一致，固定候选补丁 SHA-256 为 `be2498625c2313de3ce2317358ff912f966ce677b29f2df43b36aa6b484a5223`。Host 核对回归、精确清理清单与源文件哈希，验证范围为本地 macOS。清理归属、native 凭据/结果、三份零残留 receipt、实际测试日志和争用红绿探针均在 `/tmp/c2-test-endpoint-cleanup-20261013/`。

### B17：初始化失败的新建 lease 回滚

执行基线为 `6788f5e19db9e6e367b8d41b074603a79e8106ae`。前轮确认初始化在 owner record 写入前失败会留下空 lease；12 个历史文件中仅 2 个能逐项关联到旧测试的 EPERM bind 失败，其余 10 个未确认具体来源。用户批准修复当前初始化回滚，要求只撤回本次新建且身份未变的 lease，保留历史对象回收规则，本轮继续不用 buddy。

bind 通过 `O_CREAT | O_EXCL` 区分新建与既有 lease；回滚在原 gate 和文件句柄仍持有时核对目录、gate、lease 身份及原生锁。已成功绑定 socket 时，先由既有 socket guard 撤回，确认路径消失后才撤 lease；未知 socket、替换项及失去原身份的对象继续保留。既有 lease 不获得新文件回滚权限，`reap`/`sweep`、Core 生命周期、公开 API/ABI 均未修改，显式 OFD 解锁保留。中英文生命周期指南说明当前回滚边界；历史验收报告补注新旧行为，未改写旧验证结果。

失败测试更新写入/部分写入、权限/listen、未知 socket 和 live duplicate 场景中的 socket/lease 断言；写入失败的重复夹具合并，既有空/损坏 lease 的身份、内容、维护拒绝与解锁断言并入原 live-duplicate 用例。新增 lease/gate 替换故障覆盖。生产净增 52 行，测试净增 69 行（其中 18 行为运行时文件内的 cfg(test) hook），非文档合计净增 121 行；这是初始化收尾缺陷修复，不计作瘦身收益。

先在旧生产实现上运行更新后的 record-failure 用例，确认因新 lease 残留退出 101；候选相同用例通过。最终 local 为 95 + 9 + 12、IPC 为 203 + 7、server 为 166，合计 492 passed，另有原来需要 root 启动第二 UID 的 1 项 ignored。重建 Python native 后，endpoint maintenance/context、RuntimeSession lifecycle、direct IPC control 和 server 六个文件共 171 passed、5 项 Windows 专属 skipped。本地 macOS 合计 663 项通过；不据此声明 Linux/Windows 实跑。

真实原生探针在私有目录使用超出 owner-record 上限的逻辑地址，bind 按原错误拒绝，返回时 socket 与 lease 均已不存在，随后原生定向 sweep 检查到 0 个端点；gate/marker 保留。Rust、Python 回归后分别比较默认目录条目与 inode，均无新增、消失或身份变化，socket 为零；历史 12 个空 lease 的 device/inode、权限、大小、mtime/ctime 和内容逐项保持不变，未执行历史清理。

原生子代理独立审查固定三源码文件的补丁，SHA-256 为 `86e60ab0d21b3281b0d4b93d39ac5fbc18f10a2db8963b826edc46e06d85298c`，无 actionable findings；Host 复核源码哈希与审查输入一致。目录项核验与 unlink 仍沿用既有合作式 gate 模型，不新增对不遵守 gate 的同 UID 进程的原子路径替换保证。`git diff --check` 通过。红绿日志、完整回归、真实探针、零残留 receipt、差量审计及独立审查收据位于 `/tmp/c2-new-lease-rollback-20261013/`。

### B18：R14 私有 SHM 分配流程收敛

执行基线为 `e0dda8fa2e8833afb400064c775be57061e37840`。用户确认本批只统一内存池分配流程，保留 GC 时机、错误顺序、预算/压力计数与释放语义；R01/R02、公开 API/ABI、布局和 GC 策略均不在范围内。本轮继续不用 buddy。

`pool.rs` 的 `alloc`、`alloc_handle`、`try_alloc_shm` 共用私有 `alloc_shm`，后者由既有 `alloc_buddy` 加入原入口的资格判断形成，保持 reuse → GC 有回收才 retry → 容量允许才 expand → dedicated 的顺序。`alloc` 的提前 dedicated GC 仍在零长度检查前；另两个入口保留原校验顺序，只有 `alloc_handle` 在最终 SHM 失败后尝试 file spill，各自错误投影不变。私有 `into_handle` 按真实 `is_dedicated` 投影，buddy 的 actual_size/generation 与请求 len 分别保留。Host 机械比较确认：五个指定函数外生产代码逐字节一致，共享 tier 原函数体除外层资格判断和缩进外一致，计费、压力、释放函数未修改。

两个基本 handle 用例合并为两个大小场景，各自创建新池，保留类型、长度和 dedicated 释放断言，并检查反向类型及 buddy 不返回 DedicatedFreed；其余测试不变。生产净减少 46 行，测试净减少 3 行，非文档合计净减少 49 行；未引入额外状态、策略枚举、测试框架或依赖。

仅合并测试时，原生产实现 c2-mem lib 176 passed。候选回归为 c2-mem 176、c2-wire 146、IPC 203 + 7、server 166，共 698 passed；另有 1 项 c2-mem compile-fail 文档测试通过，无忽略项。IPC 启用 test-support，使已有 pending 清理断言实际执行。重建 Python native 后，mem pool、buffer lease/native buffer lease、held result、input lifetime、zero-copy IPC、IPC reply 和 portable payload runtime 八个文件共 80 passed，无跳过。候选合计 779 项通过（含上述文档测试），验证范围为本机 macOS；未据此声明跨平台或吞吐提升。

Rust 与 Python 回归后分别比对默认端点目录条目及 inode，新增/消失/身份变化均为零，socket 为零；历史 12 个空 lease 的身份、权限、大小、mtime/ctime 和内容保持不变。未执行历史清理。六个改动函数与 rustfmt 输出一致，未格式化其他区域，`git diff --check` 通过。

原生子代理独立审查无 actionable findings。首审后仅将测试的一处 assert 按 rustfmt 换行，Host 和 reviewer 均确认非空白源码一致；最终源码补丁 SHA-256 为 `09c1fd6d21e79fbd6018e235f4ccaec7ed4bbfad2af51c05f54f7ee5f93531b0`，源文件哈希与最终审查输入一致。测试日志、结构比对、零残留 receipt、固定补丁及独立审查报告位于 `/tmp/c2-slim-r14-20261013/`。

### B19：R32 Rust 错误码单一声明

执行基线为 `d3fe71bc2bdabbe7de30d0537705b09051ee2b39`。用户批准只收敛错误码声明，保留名称、数值、registry 顺序、repr、公开签名及各入口未知码语义。本轮继续不用 buddy。

`c2-error/src/lib.rs` 用一个模块私有宏从既有 26 项声明生成同名 enum、同序 registry、穷尽的 name 匹配及 TryFrom。`repr(u16)`、六项 derive 和公开签名保持；未知数值仍返回 `Err(())`，由原调用方分别拒绝或保留上下文后降级。Host 机械核对声明列表逐字一致，ErrorCodeEntry、From 转换、其余生产代码（含完整 wire codec）均未改变。Python 直投、序列化、凭据投影不在本批。

四项重复测试合成独立手写的 26 项 golden 表，逐项核对 variant、数值、名称、registry 顺序、name 与 TryFrom；额外检查 9、700、718、9999 和 u16::MAX 拒绝。表没有从生产宏生成，先在旧实现上完整 c2-error 12 passed，再迁移生产声明；其余 display/wire 测试保留。生产净减少 119 行，测试净减少 53 行，非文档合计净减少 172 行；没有新增依赖或生成脚本。

首轮扩大 Rust 回归时，c2-error 12 passed，完整 relay 为 338 passed、81 failed；81 项全部在启动时报告 managed endpoint namespace is unverifiable。开始快照已证明默认 `/tmp/c2-1f5/.gate.marker` 缺失，和 B18 快照相比只有该条目消失；历史 12 个空 lease 均未变。重建前已安装的原生扩展实际 register 同样失败，inspect 报 initialization-incomplete，故不能将该轮当完整通过。缺失发生在本轮开始前，删除来源未查证；本批没有恢复 marker、搬移目录或放宽 native 接纳规则。

最终通过的定向 Rust 回归为 c2-error 12、c2-wire 146、HTTP client 89，以及 IPC 的错误阶段、旧 token、合同匹配和 canonical error 五项，共 252 passed。c3 与 Python native 均从候选源码重建；Python 使用本批预建的私有 IPC 根及显式选定的候选 c3，error、native error registry、mesh errors、Core transport parity 和 error propagation 五文件共 187 passed，包含真实 direct IPC、显式 HTTP 和 relay-aware 错误字段一致性。合计 439 项通过，无跳过；首轮失败和后续重复执行不计入此数。完整默认目录 relay gate 仍受上述环境限制，验证限本机 macOS。

Rust 与 Python 运行后默认目录条目及 inode 无新增、消失或替换，历史 12 个 lease 的身份、权限、大小、mtime/ctime 和内容保持不变。本批私有 IPC 根最终 socket/lease 均为零，只保留 native gate/marker；未执行历史清理。改动区域已按 rustfmt 格式化，其余区域未重排，`git diff --check` 通过。

原生子代理独立审查无 actionable findings；Host 核对最终源码与固定审查输入一致，补丁 SHA-256 为 `ee73f160dda7d9f175176dcc05336797a5b1c17cb46571a6a96b421ded40d250`。基线/候选日志、原目录故障复现、私有目录验收、结构比对、固定补丁及审查报告位于 `/tmp/c2-slim-r32-20261013/`。
