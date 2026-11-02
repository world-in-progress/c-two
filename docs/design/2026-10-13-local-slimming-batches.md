# C-Two 局部精简的暂缓项与条目边界

当前待办以[待办清单](backlog.md)为准；各批实现、发布与验证保留在[执行记录](../acceptance/2026-10-13-local-slimming-execution.md)。下表保留原评估的暂缓理由与范围，后续调整按[2026-10-31 复审](2026-10-31-over-defense-review.md)和待办推进。2026-11-03 核对：复审之后的几批顺带了结了以下各项，已从下面两张表里删去或改写：C02 讨论的 RequestLease 已不存在（借出在帧接纳时建立，复制之后、资源方法之前释放）；R35 与两份所有者控制会话重构随自动心跳与所有者子系统的删除（[ADR 0002](../decisions/0002-remove-owner-subsystem.md)）不复存在；R07 剩下的再评估条件、R13 剩下的取消路径、R19 与 R34 已完成。

### 继续暂缓、不单独开批的项目

下表的理由与行数估算绑定各自原评估基线，未按 0.9.0 重新测算；重启时须重新核实调用链、原设计理由和生产代码与测试的整体净收益，再确认方案。

| 项目 | 当前结论与暂缓理由 | 重新启动条件 |
| --- | --- | --- |
| C01 · 私有不可变合同 | 同一本地值的语法重验可删约 18–25 行；受检构造器、六个访问接口和调用点迁移预计使生产代码净增约 20–40 行，测试还需迁移。远端合同、live token、UID/revision、实例与重连校验仍有职责。 | 有独立的公开 Rust 接口设计目标；不能为局部去重改公开字段。 |
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
| R04 | 已完成：token 绑定 server instance、UID/revision 与准入代次，关门使旧 token 失效，接管恢复不能复活；同名替换及 owner_missing 后迟到 relay final 的反例有独立覆盖。见[准入修复验收](../acceptance/2026-10-21-admission-race-repair.md)及[三系统复核](../acceptance/2026-10-22-release-foundation-review.md)。 |
| R05 | 原范围已由 R01 覆盖。`BuddySegment::open → ShmRegion::open → BuddyAllocator::attach` 先核实际映射长度，再用创建端共用的 `checked_layout` 核对逻辑大小、offset、min_block 和 levels，最后构造 bitmap/live-record 指针。当前 attach 与畸形几何测试和已验收提交 `141668dd` 相同；旧版本、截断映射、错误几何及页取整容量已有覆盖，见 [R01 验收](../acceptance/2026-10-16-r01-allocation-release-evaluation.md)。不因没有名为 ValidatedLayout 的新类型继续保留同一待办，也不扩大为不合作进程任意修改共享内存的保证。 |
| R07 | 已完成。[R07a](../acceptance/2026-10-26-r07a-explicit-release-acceptance.md) 修复低层 `release_handle`、SyncClient 准备错误与显式 lease 的失败所有权；[收尾](../acceptance/2026-10-28-r07-failed-release-acceptance.md)完成 Core/Held 显式失败保留、消费与最终出口保守放弃并按 scope 计数、Prepared 整帧发布界线、对端请求与 chunk 注册表的真实错误处理及 Python 双错误可见性。最终 Drop 后台接管不做；单一属主释放（[ADR 0001](../decisions/0001-single-owner-release.md)）删除了失败释放的保留重试与 `abandoned_releases`，原先"出现非零再评估"的条件不复存在；Host 析构关闭责任另列。 |
| R17 | [R17a](../acceptance/2026-10-26-r17a-slot-io-acceptance.md)、[R17b](../acceptance/2026-10-26-r17b-lease-io-acceptance.md) 与 [R17c](../acceptance/2026-10-27-r17c-final-lease-stat-acceptance.md) 已分别验收：末次 slot stat、共享 verify_lease_entry/read_lease_record 和 `unlink_verified` 末次 lease stat 的真实 I/O 不再折叠为不存在或身份不符。bind 策略拒绝保留 InvalidData/原文案，公共 reap 的 Busy/旧凭据顺序及原生类/JSON、删除授权与身份围栏保留。R17c 确切来源 CI 10/10 jobs、六线合计 147 个适用 gate（其中 Python 3.14t 为 16 个）通过；生产净 +6、测试 +255。gate_still_named、gate_entry_is_current、目录 pin 分类、same_as、sweep 目录失效与 owner context 映射继续单列，R17 整体仍部分完成。 |
| R06 | 用户已批准[最小同步修复方案](../acceptance/2026-10-24-r06-python-buffer-lifecycle-synchronization.md)，复用现有 inner/lease 锁修复响应/请求导出、释放及 retained tracking 的接纳竞态，不新增状态框架或删公开能力。固定基线 `ea8345fa` 的四个受控反例按预期失败，修复后 native payload 8 项通过；完整回归、性能、三系统工程 CI、零残留和独立审查完成，Windows 五个临时 profile 与构建产物已清理，0.9.0 canonical 发行验收与发布均已完成，见[记录](../acceptance/2026-10-25-090-windows-release-acceptance.md)。R06 按正确性修复单列并计入完成；生产净增 4 行、测试净增 236 行。回调重入风险仍独立待评估。 |
| R09、R10 | 2026-10-24 复核 R09：四次 EnvCatalog 加载仍存在，复用现有窄 resolver 的最小方案生产约持平至增 5 行、测试增 25–50 行，尚无性能实测；提前读取 override 还会改变 state lock/deadline 与前三项解析的观察顺序。继续暂缓。R10 注册快照局部去重约 20 行，独立 borrowed 校验、动态 Resource 方法查找及错误顺序有真实职责。R08 原环境拒绝语义的解析收敛和 R30 已批准子项已经完成。 |
| R12、R28 剩余 | 已完成可证明的同值重验删除、登记事实与 validated relay 表示收敛；跨 await 的 authority、owner/locality、代际、peer 活性和错误接纳继续保留。新增删除需另证局部不变量。 |
| R13 剩余 | 请求所有权移动、IPC 拼帧、SHM prepare/send 合并及 [inline 响应所有权移交](../acceptance/2026-10-24-r13-inline-response-ownership-acceptance.md)均已验收。inline 子项复用已有 owned 构造器，省去 Core held 路径的一次 Vec 克隆及重复副本；API/ABI/协议不变，生产与测试实际净增 37 行，作为运行时成本优化单列，不记作代码净减。服务端 Python 初始快照仍有数据寿命职责。写锁前取消保留未发布分配的问题已随协议批 P4 修正，取消即释放未发布的准备分配（`c2-ipc` 的 `cancellation_before_writer_publication_releases_ordinary_and_stream_preparation`），R13 至此没有剩余。 |
| R15 | 完整 GC 下界复用已独立验收，无公开 API/ABI/协议变化。该边界内的重复压力扫描优化已完成；不承诺合并同时开始的扫描，也不扩大低于原压力阈值时的预算回收保证。上述扩展没有作为本项未交付承诺，若需要须另立设计。 |
| R14 其他、C03、C04 | C03 的 chunk assembly/pending permit 单一所有权子项已完成。准入终态与拆除围栏保留；重组代际（`ChunkAssemblyId`）与准入门按身份的核对已于 2026-11-03 随分片拒绝修复删除：它们只防同一连接上复用 request id，而 Rust 与 TypeScript 客户端都单调分配、不复用，独立审查也没有找到正确客户端下需要它们的路径。bitmap 锁证明、其他 GC admission 进度改造及轮询改通知仍暂缓。不为精简删除跨进程锁、回收进度、预算与唤醒保护。 |
| R16 | 已按[修订方案](../acceptance/2026-10-23-r16-node-backpressure.md)验收：分片上传同步推进接收，失败关闭并汇合，成功保持连接复用，空闲读取使用原生背压；低层 TS close/pause/resume 契约已同步，不留兼容。生产净减 25、测试净增 219，属于正确性修复。完整 Node→Core 投影迁移需另立方案，不作为本批交付内容。 |
| R18 | 已按正确性修复验收并随 c3 0.4.1 发布：容量拒绝复用原 retry，永久关闭才退出；status 与任务共锁、按原 token 更新，保留不可用记录的独立寿命、1024 owner 上限、代际及释放顺序。生产 +49、测试 +345，不计作瘦身收益。见[验收记录](../acceptance/2026-10-23-082-client-observation-release-acceptance.md)。 |
| R19、R20 | R19 已随缺陷批修复（复审的 H10：`c3 registry` 按类型反序列化，格式错误即报错）。R20 已核销：示例的文件加载与保存已随 `eee0fe57` 删除。 |
| R22 剩余 | benchmark selector、旧 sweep 和测量标签仅为工具整理。已完成的 SDK 登记/撤回与强制 HTTP 实跑保持。 |
| R25 剩余 | 三个旧 call_prepared 入口已退役；FastDB 正式路径、Python-only pickle、Core bytes、服务端 sink、Rust 预分配及当前 client/scheduler 适配边界保留。2026-10-24 在 `ed6c761a` 复核后继续暂缓：native 调度状态已集中，剩余 prepare_call 探测与 Closed 回退承担既有适配职责；收窄协议最多局部减约 13 行，六个 call-only fixture 及错误接纳覆盖预计使测试增 30–80 行，整体净增且须改变公开 Python 行为。R23 bridge、R24 旧 buffer 参数链已完成。 |
| R26 | relay journal 明确保留；旧快照及删除原因有独有信息，未来 watch/list history 尚未退役，也没有支持删除的成本测量。 |
| R33 · 旧 Python 手工分片投影 | 已按[批准方案](../acceptance/2026-10-24-r33-python-manual-assembly-retirement.md)退役 `c_two.mem.ChunkAssembler`、`c_two.mem.MemHandle` 及对应 `_native` 导出，不留兼容。实现 `921b48ee` 生产净减 271 行、测试净减 215 行，合计净减 486 行。基线覆盖、完整 relay、真实 Rust/Python/Node、Windows 开发机、GitHub Actions 首轮 10/10（含 Windows ARM64 与 3.14t）、本批端点零残留及独立审查见[验收记录](../acceptance/2026-10-24-r33-python-manual-assembly-retirement-acceptance.md)。Rust registry/assembler/backing、Rust 公开 API、MemPool 公开能力、预算/释放、C/FFI ABI 和协议保持；R06/R07 未混入。本子项已随 0.9.0 发布，R33 仍为部分完成。 |
| R29、R32、R33 剩余 | codegen 同次缓存/typed projection、结构化错误内部序列化、Python 错误常量及孤立 wrap_error。0.8.2 的公开 CoreError 是现有错误链投影，不代表 R32 内部错误转换收敛已经完成。R32 凭据子项已按[单一原生投影方案](../acceptance/2026-10-23-r32-native-endpoint-credential.md)验收：实现 `60656603`，生产净减 66、测试净增 14，合计净减 52 行；完整回归、三系统、GitHub Actions 10/10、端点零残留及独立审查见[验收记录](../acceptance/2026-10-23-r32-native-endpoint-credential-acceptance.md)。剩余错误投影在 `3e575acb` 上复核后继续暂缓：完整迁移包含调度错误和服务端反向转换，涉及至少 6 个生产、9 个测试文件，预计生产减 10–30、测试增 30–60 行；保留 error_bytes 就无法退出编码，替换该公开字段也只惠及错误路径，尚无性能证据。生成 release 复用、call-control 写入去重和 Rust 错误码单一声明已完成；Transport 内外层错误、外部 JSON/wire 与错误顺序保留，不混入无关 API 删除。 |
| R34、R35 | 已了结：peer SHM 私有副本已退出，RouteAck 随协议批退役；R35 随协议批整项删除自动心跳而不复存在。C05 ABI 4 完整端点能力子项已完成；SyncClient 的预分配 API 后来作为死代码在缺陷与死代码批删除。 |
| R36、R38、R39、C06 剩余 | 已完成的死状态、不可达尾部和 readiness 后等待不再重开；真实等待、维护 EOF/错误退出、操作错误映射和外部 chunk 配置接纳仍有职责。2026-10-24 复核 R39：操作专属成功结果已完成，reap 必需 proof 约减 6 行、HTTP body 复用约减 7–10 行，不单独开批；专属错误表示约持平至增 5 行，当前映射保留。 |
| T01–T07 剩余 | 源码词串门禁、假 owner、阻塞读取、重连/GC 负例及默认值证明，随对应正确性事项替换；不为测试行数牺牲行为覆盖。已迁移的真实生命周期、协议边界和手写 golden 不回退。 |
| 已完成且无新剩余子项 | R11 登记事实、R27 FIFO 后的死计数、R30 已批准配置子项、R31 owner/冻结事实配对、R37 旧发布脚本等，维持各批验收状态。 |
| 本机历史目录 | 不清理、不重建、不冷切换，也不将未知路径消失认定为原生回收成功。新实验仅处理本次自己创建且可精确识别的资源。 |

后续破坏性 API/ABI/协议变更继续要求对确切实现提交运行 GitHub Actions 的适用 Linux、macOS、Windows x64/ARM64 门禁；协议变更不保留旧表示兼容。生产代码与测试一起核算净收益，每项完成针对性回归、完整 relay、适用跨语言验证、端点零残留和独立审查后再推进，验证后及时清理构建产物。已通过且不受影响的验证不重复运行。2026-10-16 的纯文档收束没有触发新 CI，也没有使用 hey-my-buddy；后续修复各自记录验证与审查。

2026-10-22：两份控制会话重构方案继续暂缓。Claude 提出的本地 bind/connect 结构化 OS 原因已按[验收记录](../acceptance/2026-10-22-local-io-cause.md)完成：实现候选 `57b1cab9`，真实三系统、完整 relay、跨语言及 [GitHub Actions 9/9 门禁](https://github.com/Dsssyc/c-two/actions/runs/39722345497)通过，端点验收与产物清理完成。该项与 R17 的 inspect/reap 错误折叠不是同一入口，不将其计作 R17 已完成，也不把诊断增量计作瘦身收益。
