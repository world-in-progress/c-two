# 交互面精简审查

日期：2026-10-31。输入：upstream main `606b8f13`。状态：用户同意建议（2026-10-31）。第一、二节与决定 2、3 已实施并验收，2026-11-03 经 [world-in-progress/c-two#215](https://github.com/world-in-progress/c-two/pull/215) 合入上游 main，macOS 并排验证与 fork 全量 CI [41173746331](https://github.com/Dsssyc/c-two/actions/runs/41173746331)（Linux、macOS、Windows x64 与 ARM64）都已通过。第三节的逐项表已于 2026-11-16 经维护者批准，并于 2026-11-17 经三平台基准与复合进程审查补订定稿（见[配置收敛执行计划](2026-11-16-config-convergence-plan.md)）；第六节的所有者子系统已决定删除，并入协议批（2026-11-01）。

起因：用户要求按 C-Two 的品味审查各个交互面是否足够精简。理由不只是方便下游开发者：C-Two 不在模型已有的知识里，AI 只能把接口读进上下文，接口越大，越占上下文，也越容易丢掉关键细节，这不利于在 C-Two 之上继续做更复杂的下游应用。流式 RPC 是将来必须有的能力，它新增的接口也按本文的分层与预算来设计。

## 量一量

以下是 Host 实测，token 按字节数除以 4 估算：

| 交互面 | 现状 | 普通应用实际要用的 |
| --- | --- | --- |
| Python 顶层 `cc.*` | 79 个名字，签名加各自的文档约 6.8k token | 约 19 个，约 2.0k token |
| Rust SDK 根 `c_two::*` | 92 项（不含隐藏的 `generated` 模块） | 约 10 项 |
| 配置项 | 39 个环境变量，其中 IPC 27 个；多数在代码覆盖里还有一个同名的键 | 3–5 个 |
| 用户文档 | README 约 2.3k，`python-usage` 4.2k，`configuration` 3.9k，`memory-policy` 4.8k，`local-endpoint-lifecycle` 8.3k | README 加一份用法说明 |
| AGENTS.md | 64 KB，约 16k token；每个在本仓库工作的智能体都要读，包括每个微任务的 Worker | — |
| `c3` 命令行 | 4 组共 14 个命令 | 已经紧凑 |

Python 的 79 个名字里，日常接口与监督进程、工具链、诊断用的接口混在同一个命名空间，`dir(cc)` 和类型提示看不出哪些是日常用的。

## 下游的实际用法

Host 只读检索了已知的下游：

- **较早的科学计算下游**用的是已经退役的 `icrm`、`compo`、`rpc` 一代接口，不受本次改动影响，升级时本来就要迁移。
- **用当前接口的下游**大多只用日常接口；有一个还用 `ConcurrencyConfig` 与 `ConcurrencyMode`；另一个还用连接事件、端点维护（`set_local_endpoint`、`inspect_endpoint`、`reap_endpoint` 等）、`memory_stats`、`with_call_options`，以及 `set_server` 与 `set_client` 的代码覆盖。
- **没有下游在用的：** 所有者控制与托管子进程、`infer_crm_from_resource`、资源桥、`cc.Held`、`hold_stats`、Python 侧的契约导出与代码生成函数。有一个下游评估过所有者控制，明确不采用。
- **有一个下游为了得到轻量配置，设了 9 个内部开关：** 关掉 buddy 池，缩小重组段与重组上限，并限制分片总数、执行线程、pending、单条载荷与帧。它还用 `memory_stats()[scope] is not None` 来判断某个角色是否已经开始 I/O、配置是否还能改。这说明 C-Two 没有直接回答这个问题的接口。
- **轻量配置的来由。** 用户说明：池是为大载荷、高频的调用省去开辟内存的开销；这个下游的消息小、有上限，也不追求速度，用不上这份性能。Host 在私有 IPC 根下实测，条件是 macOS 27、同一进程里两个 Runtime 经 IPC 调用，取中位数：

  | 路径 | 8 KB | 64 KB | 1 MB | SHM 账面峰值 | 进程 RSS 增量 |
  | --- | --- | --- | --- | --- | --- |
  | 开池（默认） | 82 µs | 92 µs | 317 µs | 256.5 MiB | 7.1 MiB |
  | 关池 | 85 µs | 98 µs | 395 µs | 1.0 MiB | 7.9 MiB |
  | 全部内联（阈值设为 32 MiB） | 68 µs | 147 µs | 1442 µs | 0 | 8.4 MiB |

  - 256 MiB 的段是稀疏的：开池以后，进程的 RSS 并没有多出 256 MiB，这个数只记在地址空间与预算账上。Windows 上它是否占用提交额度，没有验证。
  - 关池对这个下游的代价很小，但也没有让它更轻。它真正需要的是上限：单条消息、帧与重组。
  - SHM 与内联的交叉点在 8 KB 到 64 KB 之间，默认阈值 4 KiB 偏低。

## 依据

- **一件事一种写法。** 没有别名，没有第二个构造入口，同一个值不接受两种拼写。
- **按受众分层。** 日常接口放在顶层：声明契约、注册、连接、hold、关闭。监督进程（所有者控制、端点维护）、工具链（契约导出与代码生成）和诊断（连接事件、内存统计、生命周期快照）各进一个子模块。分层不是隐藏：这些能力仍然公开，只是不和日常接口挤在一起。
- **返回值有类型。** 调用方不读文档也能看出结构。
- **开关表达意图。** 没有使用者的开关不留；能从别的设置推导出来的，不做成设置；必须留下的开关，表达用户的意图，而不是暴露内部机制。
- **名字说明是什么。** 公开名字说明它是什么，不说明它在哪里实现。

## 一、可以直接修的缺陷（Host 核对）

1. `cc.Held` 是 `HeldResult` 的别名（`sdk/python/src/c_two/crm/transferable.py:180`）。代码里没有用到它；唯一一处测试只断言它是别名（`sdk/python/tests/unit/test_held_result.py:12-13`），其余只出现在旧文档里。删掉别名和这条断言。
2. 资源桥有三种写法：`cc.bridge(...)`、`cc.ResourceBridge(...)`，以及在 `register(bridge=...)` 里直接写字典（`crm/bridge.py:31-36`、`:40-56`）。`bridge` 与 `infer_crm_from_resource` 都没有文档字符串，AI 只能去读源码。去留见第五节的决定 3。
3. 采纳所有者控制有两个入口：`cc.adopt_owner_stdin()` 与 `NativeOwnerBootstrap.adopt_owner_stdin()`，而后者的文档自称是唯一入口。留后者。
4. `@cc.crm` 的 `namespace` 与 `version` 有默认值 `'cc'` 和 `'0.1.0'`（`crm/meta.py:127`），AGENTS.md 却写着必填。契约身份不该有默认值，否则两个包里同名的类都会成为 `cc/<类名>/0.1.0`。改为必填。
5. AGENTS.md 与 `.env.example` 把 `C2_IPC_MAX_PENDING_REQUESTS` 写作"每条连接"的上限，代码里却是整个 Server 共用一个信号量（`core/transport/c2-server/src/server.rs:622`）。照说明配置会配错。它的去向在第三节的逐项表里定，文档随之改正。
6. `hold_stats()` 返回的字段与 `memory_stats()["holds"]` 完全相同（Host 运行比对），是重复的入口。删掉 `hold_stats()`。`memory_stats()` 的结果里还带着一句固定的说明文字 `budget_cells_note`，它应该写在文档字符串里，不该出现在数据里。
7. 7 个公开函数返回裸 `dict`：`shutdown`、`hold_stats`、`call_execution_snapshot`、`native_lifecycle_snapshot`、`native_terminal_outcome`、`inspect_endpoint`、`reap_endpoint`。同一个包里，`memory_stats` 却返回有类型的 `MemoryStats`。全部改为有类型的返回值。
8. `native_lifecycle_snapshot` 与 `native_terminal_outcome` 的名字带着实现位置，而且只在测试和诊断里用到。随分层移入诊断子模块，并改名；`NativeOwnerBootstrap` 同理，改为与 Rust 一致的 `OwnerBootstrap`。
9. `c_two.error` 没有 `__all__`，`IntEnum`、`Literal`、`Mapping`、`unique` 这些导入进来的名字都露在模块里。补上 `__all__`。
10. Rust 的端点函数成对出现：`ping_direct_ipc` 与 `ping_direct_ipc_with_context`、`shutdown_direct_ipc` 与 `shutdown_direct_ipc_with_context`、`direct_ipc_endpoint` 与 `direct_ipc_endpoint_with_context`（`sdk/rust/src/lib.rs:15-32`）。每对合成一个，用一个可选的上下文参数。

## 二、按受众分层

Python 的布局：

| 位置 | 内容 |
| --- | --- |
| `c_two` 顶层 | `crm`、`read`、`write`、`transfer`、`on_shutdown`、`hold`、`HeldResult`、`register`、`connect`、`close`、`unregister`、`serve`、`shutdown`、`server_address`、`with_call_options`、`set_relay_anchor`、`set_server`、`set_client`、`Runtime`、`ConcurrencyConfig`、`error` |
| `c_two.owner` | 所有者控制与托管子进程：`OwnerBootstrap`、`OwnerControl`、`OwnerRecoveryTicket`、`PreparedOwnerTakeover`、`OwnerStatus`、`OwnerControlError`、`OwnedChild`、`owner_control_pair`、`prepare_owner_takeover`、`spawn_owned_child`、`LifecycleConfig` |
| `c_two.endpoint` | 端点维护：`EndpointCredential`、`LocalEndpointContext`、`local_endpoint_context`、`EndpointSweep`、`inspect_endpoint`、`reap_endpoint`、`sweep_endpoints`、`set_local_endpoint`、`server_credential`、`server_id` |
| `c_two.observe` | 诊断：`connection_events`、`connection_observation` 及 5 个连接事件类型，`memory_stats` 及 4 个内存统计类型，`call_execution_snapshot`、`lifecycle_snapshot`、`shutdown_outcome` |
| `c_two.contract` | 工具链：`export_descriptor`、`release_ref`、`diagnostics`、`compile_artifacts`，以及 `Artifact`、`ArtifactSet`、`CodegenError`、`CodegenTarget` |

- **子模块内去掉重复的前缀。** 例如 `c_two.contract.export_descriptor`，而不是 `export_contract_descriptor`。
- **暂不动的。** `set_transport_policy` 与 `set_call_execution_limits` 留在顶层，去留由第三节的逐项表决定；`infer_crm_from_resource` 与资源桥暂不动，去留见决定 3。
- **默认实例与显式实例。** 模块级函数委托给默认 Runtime，`cc.Runtime()` 提供同样的方法，这是常见的"默认实例加显式实例"写法，予以保留。规则只有一条：子模块里的函数作用于默认 Runtime，`Runtime` 上的同名方法作用于显式实例。初稿曾建议给子模块函数加一个 `runtime` 参数；Host 复查后放弃，因为那样会出现两种指定实例的写法。
- **Rust 同样按模块分。** 根上只留启动 Runtime、承载服务、连接客户端和处理错误所需的类型，例如 `Runtime`、`Connect`、`ConnectOptions`、`Client`、`Host`、`HostOptions`、`ContractRelease`、`CallOptions`、`Error`、`Held`。其余按与 Python 相同的分组，进入 `endpoint`、`owner`、`observe`、`contract` 四个模块。生成代码引用的路径随 `c2-codegen` 的模板一起改。
- **效果。** AI 写一个普通应用时，只需读顶层，约 2k token；做监督进程或工具链时，再多读一个模块。

## 三、配置项

### 现状

2026-11-13 按 main `a68ffcb5` 重新清点：`.env.example` 列出 39 个环境变量，加上 `C2_ENV_FILE` 共 40 个，其中 `C2_IPC_*` 26 个。

- 示例只设置了 `C2_RELAY_ANCHOR_ADDRESS`；CI 只设置了它和 `C2_ENV_FILE`。
- 大多数 IPC 变量在 Python 测试里只出现一次，都在列举全部变量的配置解析测试 `sdk/python/tests/unit/test_ipc_config.py` 里；在 `docs/reference/configuration.md` 里也只出现一次。
- 已知需要轻量配置的场景只需指定单条消息上限、总内存预算和执行线程数。现有的代码覆盖键还要求部署方了解池、重组段、分片和帧。

10-31 的草案之后，这些设置已经变了：协议批删掉了心跳与 `C2_IPC_ENDPOINT_PROTOCOL`；缺陷批删掉了 `C2_RELAY_ROUTE_MAX_ATTEMPTS`（H3），并去掉了 `max_execution_workers` 显式值的 64 上限（H12）；流式批把 `max_total_chunks` 改为全服务端的 `max_inflight_chunk_frames`（B8）；载荷缓冲预算取代了 `C2_CALL_RETAINED_INPUT_BUDGET_BYTES`。草案里没有列出 `write_timeout`、`client_idle_timeout`、`max_dedicated_segments` 与 `spill_threshold`，下表补上。

### 收敛方向

- **不再提供池的开关。** 实测开池不额外占内存，关掉只会变慢。初稿建议让共享内存段自适应大小；量过之后撤回，因为删掉开关这个更小的办法就够了。
- **`shm_threshold` 改为常量。** 取值按三个平台实测的交叉点定。
- **部署方只表达意图。** 留下的设置是：单条消息上限、内存预算、文件溢出预算、执行线程数、端点根目录与 relay 锚点，以及 `c3 relay` 的部署参数。帧、分片、重组的各项限制都从它们推导。
- **定时器与调优常数改为固定常量。** 只有不随载荷与规模变化的时限才固定；会随部署变化又推导不出来的，仍经已有的途径暴露。

### 逐项表（2026-11-16 维护者批准）

部署与身份，全部保留：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `C2_ENV_FILE`、`C2_RELAY_ANCHOR_ADDRESS`、`C2_IPC_ROOT` | 保留 | 表达部署意图 |
| `set_server(server_id=…, startup_timeout=…)` | 保留 | 一个是身份；启动等待随部署而变 |
| `c3 relay` 的 `--bind`、`--relay-id`、`--advertise-url`、`--seeds` 与对应的环境变量，`C2_RELAY_USE_PROXY` | 保留 | relay 的部署参数；公司网络的代理是真实的部署需要 |

单条消息：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `max_payload_size`（16 GiB） | 保留，作为唯一的单条消息上限 | 部署方要表达的意图之一 |
| `max_reassembly_bytes`（8 GiB） | 删除，由 `max_payload_size` 得出 | 它限制一次重组的容量（`c2-wire/src/assembler.rs` 的检查），是第二个单条消息上限；默认值还比 `max_payload_size` 小，按代码，走分片的载荷超过 8 GiB 就会在重组时被拒绝，没有实际复现 |
| `max_frame_size`（2 GiB） | 删除，改为由分片大小得出的协议常量，客户端按同一个常量检查 | 正常路径上的帧不超过一个分片或内联阈值；客户端现在没有帧长上限，"待维护者决定"里的那一项随之解决；控制帧的长度在执行计划里逐一核对 |

内存：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `shm_backing_budget_bytes`（8 GiB）、`live_reassembly_budget_bytes`（8 GiB）、载荷缓冲预算 `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES`（16 GiB） | 合并为每个激活角色域（Runtime 的客户端域、Runtime 的 Host 域）与每个 relay 各一个内存预算（2026-11-17 补订） | 部署方关心的是内存上限，不必分别懂三个格子。复合进程在资源回调内发起下游调用（`A -> B -> C`）时，若客户端与 Host 共用同一池，排队的入站请求占满预算会导致回调内的阻塞出站预留永久互等；按角色懒创建独立域可消除跨角色挤占，纯客户端与纯 Host 进程仍只激活一个域 |
| `file_backing_budget_bytes`（16 GiB） | 保留，作为文件溢出预算（按角色域与 relay 独立） | 磁盘与内存是两种资源；内存预算耗尽时分片重组须能真实回退到文件溢出预算 |
| `spill_threshold`（0.8） | 固定常量 | 它取观察到的可用内存的比例，本身随机器伸缩；部署方的限制由内存预算表达 |
| `max_dedicated_segments`（4） | 删除；独占段的个数只受线上协议的段索引范围限制，字节由内存预算约束 | 个数上限按"通常情况"限制了同时在途的独占段，槽位用完时独占段分配失败（`c2-mem/src/pool.rs` 的 `alloc_dedicated`） |
| `max_inflight_chunk_frames`（512） | 删除公开计数，在途分片帧及其每请求待拷贝队列节点计入内存预算，内部阻塞拷贝并发度按硬件并行度推导 | 原槽位用完时请求以 `ResourceUnavailable` 失败；改为每请求帧队列并在首片发布重组存储后轮转调度，既兼容窄窗口小分片，又避免每帧派生任务或饿死首片 |

池与分片，改为内部常数或推导：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `pool_enabled`、`pool_prewarm_segments`、`pool_min_retained_segments` | 删除 | 开池前未建段不占内存，关掉只会变慢（10-31 实测）；预热与保留没有使用者 |
| `pool_decay_seconds`（60 秒） | 固定常量 | 调优常数 |
| `pool_segment_size`（256 MiB）、`max_pool_segments`（4）、`reassembly_segment_size`（64 MiB）、`reassembly_max_segments`（4） | 由内存预算推导；主池单段上限收为 `64 MiB`、最多 `16` 段，重组池最多 `4` 段 | 三平台实测确认 Unix 段映射完全稀疏，而 Windows `CreateFileMappingW`（`SEC_COMMIT`）在首次建段时按整段扣减系统提交额度，且 `64 MiB` 与 `256 MiB` 热复用延迟一致（Linux `1 MiB` 载荷下 `64 MiB` 更快），`64 MiB × 16` 保住 `1 GiB` 容量同时将 Windows 单池初始提交额度降低 4 倍 |
| `chunk_size`（128 KiB），Node 的 `requestChunkSize` | 固定常量 | 协议内部机制 |
| `chunk_threshold_ratio` | 删除 | 已证实没有作用（见待办第 3 项） |
| `chunk_assembler_timeout`（60 秒）、`chunk_gc_interval`（5 秒） | 固定常量 | 前者从一次重组最后的收片或拷贝活动算起，不随载荷变大 |
| `shm_threshold`（4 KiB），Node 的 `requestShmThreshold` | 固定常量 `16 KiB`（`16384`，严格 `>` 判定） | macOS arm64、Windows 11 x64 与 Linux x86_64 实测共同确认：`<= 16 KiB` 走内联避免小包建段，超过 `16 KiB` 走 SHM |
| `C2_REMOTE_PAYLOAD_CHUNK_SIZE`（1 MiB） | 固定常量 | 调优常数 |

这两项都改为常量之后，`cc.set_transport_policy(...)` 整个消失。

连接：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `write_timeout`（30 秒） | 固定常量 | 它限制写一帧（含 flush）的时间；帧不超过一个分片，写不完说明对端停止读取，与载荷大小无关 |
| `client_idle_timeout`（60 秒）与 `c3 relay` 的 `--idle-timeout` / `C2_RELAY_IDLE_TIMEOUT`（60 秒） | 合为一个固定常量 | 两处是同一件事：没有人引用的缓存连接多久后回收；它只影响重连的开销 |

执行与准入：

| 设置 | 建议 | 理由 |
| --- | --- | --- |
| `max_execution_workers`（默认按主机并行度取 4 到 64，显式值须 `>= 1`） | 保留；服务端 Tokio 阻塞线程池按 `max(512, W + C + E + 16)` 显式派生（`C`、`E` 由主机并行度夹在 `4..=64` 推导） | 执行线程数是进程的资源，随机器而变；显式拉高服务端阻塞池并为分片拷贝 `C` 与流编码 `E` 留出独立门控，防止大 `W` 或高并发流饿死底层拷贝 |
| `max_pending_requests`（整个 Server 1024） | 删除独立设置；服务端准入并入 Host 角色域的在途调用上限，排队请求的本地副本计入 Host 内存预算（2026-11-16 批准，2026-11-17 补订） | 服务端不必维护第二套命名与计数，且原计数约束不了大请求字节。实施时把请求号高水位推进与活动表插入拆开，OPEN 在建调用记录和复制本地副本之前先检查在途调用上限与内存预算，不足时在进入资源前以 `NotExecuted` 拒绝；控制回复脱离全 Server 共享信号量，改为每条物理连接独立有界（`min(max(1, N), 64)`），单连接满额只停用该连接的新准入；路由仍有自己的 `max_pending` |
| `C2_CALL_MAX_OUTSTANDING`、`set_client`/`set_server` 的 `max_outstanding_calls`、`--call-max-outstanding`（默认随该域内存预算推导） | 保留并统一为每个激活角色域（客户端域、Host 域）与每个 relay 的在途调用上限（2026-11-16 批准，2026-11-17 补订） | 有状态 RPC 在等待者断开或超时后不中止已越过派发边界的一元调用，relay 又把多个客户端复用到同一条上游 IPC 物理连接，0 字节调用与空闲服务端流无法单靠字节预算或文件描述符约束。统一覆盖全部一元调用（含无截止时间调用）与服务端流，同进程直调计入目标 Host 的 `N`（不激活客户端域）并在回调异常出口剥离内层 `pre_dispatch` 标记，IPC 自连由两侧角色域各自计 1 次，立即给或立即拒（`717 CallCapacityExceeded`），绝不等待。该域内存预算 `M == 0` 且显式设正数上限时报配置错误，`M == 0` 时上限为 `0`（关闭该域全部新业务调用，保留拒绝、释放与关闭），`M > 0` 且未显式设置时默认取 `max(64, floor(M / 65536))`。同批闭合 relay 的四处缺口：删除无使用者的公开 `POST /_echo` 及其自测，准入前错误响应改为不等待字节预算的控制回复，`unary_response` 将名额移入响应 `Body` 直至完成或丢弃，多片响应与流的下载窗口在该次转发内循环复用 |
| `C2_RELAY_CALL_TIMEOUT`（300 秒） | 删除；经 relay 的调用与直接 IPC 一样，默认不设截止时间 | 现在没有逐次截止时间的 HTTP 调用 300 秒后失败，并成为派发不确定，同一个调用走直接 IPC 却一直等。逐次调用的截止时间是限制等待的唯一途径 |
| `C2_RELAY_ANTI_ENTROPY_INTERVAL`（60 秒） | 固定常量 | mesh 的调优常数 |

按上表收敛后剩 13 个环境变量，全部去掉 `C2_IPC_` / `C2_CALL_PAYLOAD_` 等内部实现前缀（保留 `C2_IPC_ROOT`）；Python 公开代码入口收敛为对称的 `cc.set_client(...)` 与 `cc.set_server(...)`（删掉 `set_transport_policy`、`set_call_execution_limits` 与 `ipc_overrides` 字典），Rust 收敛为 `RuntimeOptions`（客户端域）与 `HostOptions`（Host 域），`c3 relay` 收敛为三项容量标志（见[配置收敛执行计划](2026-11-16-config-convergence-plan.md)）。同一批顺带处理 R09、R10（EnvCatalog 加载四次）与 C4（客户端池再比一遍 Core 已冻结的配置）。新的两个名字"内存预算"与"文件溢出预算"在实施时写进术语表，"超时"一条提到的已删设置随之改写。

## 四、错误类型

`c_two.error` 有 29 个错误类。其中 8 个按"出错的一侧 × 阶段"命名，例如 `ResourceDeserializeInput`、`ClientOutputFromBuffer`。应用真正要分支处理的是：找不到资源、资源不可用、超时、契约不匹配、资源方法自己抛了错。建议只为这些可据以行动的区别保留类，一侧与阶段改为错误上的字段。错误码属于 Rust 的错误注册表，并且在线上传输，所以这一项放在协议批之后。

## 五、用户决定（2026-10-31）

1. 第二节的分层与子模块名：同意。
2. `ConcurrencyMode` 与 `InputLifetime` 只收字符串字面量（`Literal[...]`），两个枚举类从公开接口中删掉：同意。类型检查照样能发现拼写错误，取值也直接写在签名里。用到 `ConcurrencyMode` 的下游升级时要改成字符串。
3. **契约推断与资源桥：同意删除**（已随 #215 实施）。删除 `infer_crm_from_resource`、`c3 contract infer`、`bridge`、`ResourceBridge` 与 `register(bridge=...)`。理由：没有下游在用；写一个契约类只要几行，而契约是跨语言的身份，应当显式写出，不该从实现的签名里推断；资源桥要做的转换，在资源方法里写一行就能完成。

   风险评估：

   - **失去的便利。** 资源桥让已有的领域类不用改动就能注册；推断让人不写契约类，也能从实现导出描述。替代写法：注册一个薄的适配类；或者先写契约类，再用 `c3 contract export`。
   - **与既定立场一致。** 7 月可移植载荷的重新设计已经删掉了旧的推断与资源桥，并写明领域适配归应用自己负责（`docs/issues/contract-release-deferred-capabilities.md` 第 15、55 行）。现存的通用资源桥与推断，是那次删除之后留下的。
   - **删除本身的风险。** 要改四处：注册、服务端对资源的包装、签名一致性检查、借用输入的校验；另外还有 CLI 的一个子命令和相关测试。改的时候要守住：普通的一致性检查不能被一并削弱。借用输入与资源桥不能同用的那条规则，随之消失。
   - **下游。** 已知的下游都没有用。此外有没有用户，无从得知，所以按 0.x 的做法，在发行说明里写明迁移方式。
4. 配置项按第三节收敛：同意这个做法；逐项表已于 2026-11-16 批准。
5. AGENTS.md 按下文重组：同意。

**AGENTS.md 重组的内容。** AGENTS.md 只留当前的架构地图、构建与测试命令和工作规则；机制细节放进各自的文档（内存、生命周期、传输），按路径链接；已退役接口的禁令合成一张简短的清单。下游应用的最短路径是 README 的快速开始，加上顶层模块的文档字符串；不另写一份给 AI 看的文档，免得多一份要同步的副本。

## 六、所有者子系统

决定（2026-11-01）：删除，并入协议批。用户的标准是"下游看不见、内部机制也不需要，就可以删"；独立评估与 Host 核对都满足：C ABI、CLI、relay 与端点维护都不依赖它，普通 Host 共用的关闭、生命周期观测、准入与端点凭据保留。

所有者控制包括：

- `OwnerBound` 生命周期、双工监督与缺席宽限；
- 恢复凭证与接管；
- 托管子进程；
- 在 Python 与 Rust 各约 12 个公开名字，`docs/local-endpoint-lifecycle.md` 的大部分篇幅。

已知的下游都没有用，其中一个评估后明确不采用。按"没有使用者的能力不留"，可以整体删除；但它是有意建成的能力，删不删由用户决定。在决定之前，分层把它移进 `c_two.owner`，不改它的行为。

## 七、执行计划

T1 与 T2 改的文件互不重叠，可以并行。

### T1：Python 交互面

- **写入范围：** `sdk/python/src/c_two/`（包括 `cli/`），`sdk/python/tests/`，`examples/python/`，`docs/python-usage*.md`，以及 README 中受影响的段落。
- **内容：**
  - 第二节的 Python 布局；
  - 第一节的第 1、3、4、6、7、8、9 条；
  - 决定 2。
- **不做：** 不动 AGENTS.md（由 Host 改）；不动 `infer_crm_from_resource`、资源桥、`set_transport_policy` 与 `set_call_execution_limits`；不改 Rust 与协议。
- **检查：**
  - 完整的 Python 测试；
  - Python 3.10 语法检查；
  - `tests/repo`。

### T2：Rust 交互面

- **写入范围：** `sdk/rust/`，`core/foundation/c2-codegen/`（生成代码引用的路径），以及 `sdk/rust/README.md`。
- **内容：** 第二节的 Rust 布局与第一节第 10 条。
  - 根上留哪些、各模块各放哪些，按第二节的规则分；
  - 拿不准归属的类型，列进报告，由 Host 定。
- **检查：**
  - `cargo test --manifest-path sdk/rust/Cargo.toml --all-features`；
  - `c2-codegen` 的测试；
  - 可移植载荷的跨语言与 18 行矩阵门禁（生成代码变了）。

### 给两个微任务的共同要求

- 一次切干净，不留别名与旧路径；被取代的代码、测试与文档一并删除。
- 不加防御性的检查。
- 只用私有的 IPC 根，不碰日常服务。
- 写入范围之外什么也不删，不用 git stash，不动分支与标签。
- 拿不准时停下来问。

### 之后的批次

- **配置收敛：** 用户决定逐项表之后进行。
- **AGENTS.md 重组：** 由 Host 在 T1、T2 合入后进行，以免同一段内容改两次。
- **错误类型：** 协议批之后。
- **决定 3 与第六节：** 等用户决定后，另开批次。

下游仓库不在本计划里改动；各下游怎样跟进，由用户决定。
