# 配置收敛执行计划

日期：2026-11-16（2026-11-17 维护者批准定稿）。依据：[交互面精简审查](2026-10-31-api-surface-review.md)第三节。状态：三平台基准实测、复合进程角色域隔离与服务端阻塞线程池推导均已由维护者确认。块 1–3 的 Core 配置、传输及生命周期实现已纳入当前源码；块 4 的配置模板、术语、参考文档和迁移说明已同步。SDK、CLI、ABI 8 与文档合并后的总门禁仍待完成。

本计划把现有的 40 个环境变量（含 `C2_ENV_FILE`）、三套重复的代码覆盖字典和分散的容量/并发计数，收敛为 **13 个表达部署意图的环境变量**与按角色正交的公开代码/CLI 入口，并在同一批闭合分布式场景下的准入、窗口复用、文件溢出可达性与线程池饥饿缺口，同时清理 `R09`、`R10` 与 `C4`。所有分块仅为实施先后，不保留任何旧名字、别名、字典覆盖键或兼容回退。

## 一、公开交互面（一个意图一条路径）

### 1. 保留的 13 个环境变量

单条消息上限、内存预算、文件溢出预算与执行线程数均不再局限于 IPC 内部机制（例如单条消息上限随路由注册通告给 `relay`，内存预算同时约束 IPC 与 HTTP 载荷缓冲，执行线程服务全部远程调用），统一去掉旧的 `C2_IPC_` / `C2_CALL_PAYLOAD_` 实现前缀：

| 类别 | 环境变量 | 默认值 | 代码 / CLI 入口 |
| --- | --- | --- | --- |
| 环境文件 | `C2_ENV_FILE` | 读取当前目录 `.env`（设为空串禁用） | Rust `ConfigSources`；进程环境 |
| 部署发现 | `C2_RELAY_ANCHOR_ADDRESS` | 未设置（仅直接 IPC） | Python `cc.set_relay_anchor(address)` / `Runtime.set_relay_anchor`；Rust `RuntimeOptions` / `HostOptions` |
| 本地端点 | `C2_IPC_ROOT` | Unix `/tmp/c2-v3-<uidhex>`（Windows 不适用） | Python `cc.endpoint.set_local_endpoint(root=...)`；Rust `LocalEndpointOptions`；`c3 relay --ipc-root` |
| 容量与准入 | `C2_MEMORY_BUDGET_BYTES` | 每个激活角色域默认 `16 GiB`（`0` 关闭该域正数分配与全部新业务调用） | Python `cc.set_client` / `cc.set_server(memory_budget_bytes=...)`；Rust `RuntimeOptions` / `HostOptions`；`c3 relay --memory-budget-bytes` |
| 容量与准入 | `C2_FILE_SPILL_BUDGET_BYTES` | 每个激活角色域默认 `16 GiB`（`0` 禁用文件溢出） | Python `cc.set_client` / `cc.set_server(file_spill_budget_bytes=...)`；Rust `RuntimeOptions` / `HostOptions`；`c3 relay --file-spill-budget-bytes` |
| 容量与准入 | `C2_CALL_MAX_OUTSTANDING` | 该域 `M == 0` 时为 `0`，否则 `max(64, floor(M / 65536))` | Python `cc.set_client` / `cc.set_server(max_outstanding_calls=...)`；Rust `RuntimeOptions` / `HostOptions`；`c3 relay --call-max-outstanding` |
| Host 承载 | `C2_MAX_PAYLOAD_SIZE` | `16 GiB`（须 `> 0`） | Python `cc.set_server(max_payload_size=...)`；Rust `HostOptions` |
| Host 承载 | `C2_MAX_EXECUTION_WORKERS` | 主机并行度夹在 `4..=64`（显式值无 64 上限，须 `>= 1`） | Python `cc.set_server(max_execution_workers=...)`；Rust `HostOptions` |
| `c3 relay` | `C2_RELAY_BIND` | `0.0.0.0:8080` | `c3 relay --bind`（`-b`） |
| `c3 relay` | `C2_RELAY_ID` | 自动生成 | `c3 relay --relay-id` |
| `c3 relay` | `C2_RELAY_ADVERTISE_URL` | 由实际绑定地址推导 | `c3 relay --advertise-url` |
| `c3 relay` | `C2_RELAY_SEEDS` | 空 | `c3 relay --seeds`（`-s`） |
| HTTP 代理 | `C2_RELAY_USE_PROXY` | `false` | 环境变量（进程级网络环境属性）；Rust `HostOptions` 可显式覆盖 |

此外，`cc.set_server(server_id=..., startup_timeout=...)` 与 `c3 relay --upstream`（`-u`）无对应环境变量，保持现状。

### 2. 删除的公开表面

- **Python SDK：** 删除 `cc.set_transport_policy`、`Runtime.set_transport_policy`、`cc.set_call_execution_limits`、`Runtime.set_call_execution_limits`；删除 `cc.set_client` 与 `cc.set_server` 的 `ipc_overrides` 参数；删除 `c_two.config` 中的 `BaseIPCOverrides`、`ServerIPCOverrides`、`ClientIPCOverrides` 及其校验表与 `settings.shm_threshold` / `settings.remote_payload_chunk_size`。`cc.set_client` 仅接收 `memory_budget_bytes`、`file_spill_budget_bytes`、`max_outstanding_calls`；`cc.set_server` 直接接收 `server_id`、`startup_timeout`、`max_payload_size`、`max_execution_workers`、`memory_budget_bytes`、`file_spill_budget_bytes`、`max_outstanding_calls`。
- **Rust Core 与 Rust SDK：** 删除 `BaseIpcConfig`、`ClientIpcConfig`、`ServerIpcConfig` 上已转为内部常量的公开配置字段与 `SERVER_IPC_OVERRIDE_KEYS`、`CLIENT_IPC_OVERRIDE_KEYS`；删除 `CallExecutionLimits` / `CallExecutionLimitsOverrides` 与 `MemoryBudgetLimits` 的拆分表面，统一为角色域容量配置（`DomainLimits` / `DomainLimitsPatch`）；`RuntimeOptions` 仅保留客户端域容量、本地端点与发现设置，全部 `Host` 身份、承载与服务端域容量归 `HostOptions`（删除 `RuntimeOptions` 中重复的 Server 配置入口）；删除 `ResolvedRelayClientConfig::call_timeout_secs`。
- **`c3 relay` CLI：** 删除 `--ipc-pool-enabled`、`--ipc-shm-backing-budget-bytes`、`--ipc-file-backing-budget-bytes`、`--ipc-live-reassembly-budget-bytes`、`--call-payload-buffer-budget-bytes`、`--idle-timeout`，由 `--memory-budget-bytes`、`--file-spill-budget-bytes` 与 `--call-max-outstanding` 替代；合并启动解析与 `--dry-run`，单次读取环境来源并展示单一 `relay` 域的有效 `M / F / N`。
- **Node / TypeScript（ABI 8）：** 删除 `createNodeIpcConnector` 的 `requestShmThreshold` 与 `requestChunkSize` 选项及对应的 C ABI JSON 参数与生成模板；每个 connector 对应一个只激活客户端域的 Core `Runtime`（不创建 `Host` 域），修复 `connector.stats()` 未激活时误调 `freeze()` 的副作用，直接只读投影完整客户端域快照。
- **环境变量与 `.env.example`：** 删除其余 27 个环境变量，不保留旧名读取或降级提示。

### 3. 双角色独立懒创建域、三态合并与冻结契约

在分布式流水线中间节点 `A -> B -> C` 中，进程 `B` 同时承担接收上游请求的 `Host` 与在回调线程内调用下游 `C` 的 `Client`。若两者共用同一预算或调用槽池，上游排队请求占满额度后会导致回调线程在出站预留上永久互等或使已修改状态的调用半途失败。因此每个 `Runtime` 在 Core 内维护**两个完全独立的按需懒创建角色域**（`client` 域与 `server` 域），`c3 relay` 则保持单一转发域：

- **按需懒创建：** 纯客户端进程仅在首次有效远端业务连接/调用前解析并冻结 `client` 域（`server` 域保持未激活，`frozen=false`）；纯服务端进程仅在 `Host` 启动时解析并冻结 `server` 域（`Host` 向 `relay` 注册/注销路由等控制流不激活业务 `client` 域）；同进程 Python `thread_local` 直调计入目标 `Host` 的 `N` 但不激活 `client` 域。复合进程同时激活两域时，两域各自独立计费（未写代码细分且同继承环境 `M` 时，两侧独立计费上限之和为 `2M`，不等于进程 RSS 上限）。
- **三态增量合并（`Keep / Inherit / Set`）：** 代码 setter 的全部可选参数默认采用未指定哨兵（`Keep` / Python `_UNSET`），保留此前已接受的代码覆盖意图；显式传入 `None`（`Inherit`）清除该字段的代码覆盖并恢复走环境变量/默认值；传入具体值（`Set`）更新该字段。每次调用在 Core 内构造该角色的完整候选配置原子校验提交，任一字段非法均不部分生效，且修改 `M` 不会冲掉此前显式设置的 `N`。
- **独立冻结与同效幂等：** `client` 域在首次远端出站尝试时冻结（绝不冻结 `server` 域）；`server` 域在 `Host` 启动时冻结（绝不被早期的 `connect` 抢占冻结）。域冻结后再次调用对应 setter，Core 以该角色冻结时捕获的环境来源合并求值：若完整有效配置与已冻结值完全一致（含无参空调与自动 `server_id` 沿用已捕获身份）则幂等成功；若任一有效值不同则立即抛出类型化 `ConfigFrozen` 错误（废除旧 `UserWarning` 静默忽略行为）。
- **Core 托管单 `Host` 生命周期与 Python 会话轮换：** Core `Runtime` 原子维护 `Host` 状态槽（并发同配置启动合并、运行中异配置拒绝、排空中拒绝重启直至原清理回执终态、重启复用服务端预算域的真实在途计数而不清零旧账并生成新 `server_instance_id`）。Python `Runtime.shutdown()` 仅在 `completed=True` 后轮换新会话，一致继承两个角色的显式代码配置意图，旧域未释放的真实占用与 `holds` 原子转入 `retired` / `holds` 持续可观测。

## 二、内部常量、三平台实测与几何/线程池推导

### 1. 固定内部常量

| 原配置项 | 固定值 | 依据 |
| --- | --- | --- |
| `SHM_THRESHOLD` | `16 KiB`（`16384`） | 三平台实测交叉点（严格 `payload_size > SHM_THRESHOLD` 判定，`<= 16 KiB` 走内联） |
| `CHUNK_SIZE` | `128 KiB`（`131072`） | IPC 标准分片粒度（窄窗口 `relay` 仍可按已预留上传窗口发送合法短片） |
| `REMOTE_PAYLOAD_CHUNK_SIZE` | `1 MiB`（`1048576`） | HTTP 远程传输流式切分粒度 |
| `MAX_HANDSHAKE_FRAME_BYTES` | `8 MiB`（`8388608`） | 覆盖 `MAX_ROUTES = 64` × `MAX_CONTRACT_METHODS = 256` 最坏合法握手（约 `4.13 MiB`） |
| `MAX_RPC_FRAME_BYTES` | `1 MiB`（`1048576`） | 覆盖 `128 KiB` 分片帧、64 路由目录响应与有界截断后的内联 `C2E1` 错误载荷，客户端与服务端对称校验 |
| `WRITE_TIMEOUT` | `30 s` | 单帧（不超过一个分片或控制帧）含 flush 的写超时，与总载荷大小无关 |
| `IDLE_CONNECTION_TIMEOUT` | `60 s` | 合并原 `client_idle_timeout` 与 `relay` 上游 `idle_timeout` |
| `POOL_DECAY_SECONDS` | `60 s` | 空闲 buddy 段回收窗口；`prewarm = 0`、`min_retained = 0` 固定 |
| `CHUNK_ASSEMBLER_TIMEOUT` | `60 s` | 自最后一次收片入队或实际拷贝活动起的重组超时，收齐后停止输入超时计时 |
| `CHUNK_GC_INTERVAL` | `5 s` | 过期重组扫描周期 |
| `RELAY_ANTI_ENTROPY_INTERVAL` | `60 s` | relay mesh 反熵周期 |
| `SPILL_THRESHOLD` | `0.8` | 新建 SHM 映射前对操作系统可用内存的启发式比例 |

`chunk_threshold_ratio` 与 `max_dedicated_segments` 直接删除（独占段槽位仅受线上协议段索引范围与内存预算约束）。

### 2. 三平台纯 Rust 真实 IPC 实测汇总（提交 `41296f9b`，IPC 16 双向回显，5 轮交替配对中位数）

各平台均采用严格 `payload_size > shm_threshold` 对比内联与 buddy SHM，并通过池计数与 `alloc_count == 0` 断言路径及 `RELEASE` 结算：

| 载荷大小 | macOS arm64（Rust 1.99.1）1C Inline / SHM P50 | macOS arm64 4C Inline / SHM P50 | Windows 11 x64（Rust 1.99.1）1C Inline / SHM P50 | Windows 11 x64 4C Inline / SHM P50 | Linux x86_64（Rust 1.100.0）1C Inline / SHM P50 | Linux x86_64 4C Inline / SHM P50 |
| --- | --- | --- | --- | --- | --- | --- |
| `1 KiB` | `24.8 / 28.9 µs` | `59.7 / 62.1 µs` | `50.5 / 75.3 µs` | `64.8 / 123.3 µs` | `183.0 / 202.1 µs` | `143.8 / 196.6 µs` |
| `2 KiB` | `24.3 / 36.3 µs` | `61.5 / 82.5 µs` | `49.5 / 70.7 µs` | `84.6 / 122.3 µs` | `167.2 / 219.0 µs` | `159.3 / 189.8 µs` |
| `4 KiB` | `24.9 / 36.3 µs` | `62.8 / 81.9 µs` | `62.5 / 72.9 µs` | `90.9 / 117.9 µs` | `170.8 / 181.2 µs` | `152.1 / 191.7 µs` |
| `8 KiB` | `36.0 / 35.8 µs` | `97.0 / 83.0 µs` | `74.2 / 80.4 µs` | `111.4 / 140.2 µs` | `180.9 / 201.4 µs` | `160.6 / 194.7 µs` |
| `16 KiB` | `47.3 / 38.7 µs` | `128.3 / 83.8 µs` | `97.3 / 77.7 µs` | `149.8 / 140.2 µs` | `199.3 / 202.9 µs` | `184.9 / 197.5 µs` |
| `32 KiB` | `68.7 / 42.2 µs` | `200.5 / 89.8 µs` | `166.2 / 83.5 µs` | `218.1 / 144.0 µs` | `207.6 / 211.7 µs` | `223.2 / 214.7 µs` |
| `64 KiB` | `100.9 / 48.2 µs` | `313.3 / 93.3 µs` | `366.3 / 91.9 µs` | `431.3 / 121.5 µs` | `368.1 / 219.5 µs` | `368.0 / 240.3 µs` |
| `128 KiB` | `171.0 / 51.4 µs` | `518.5 / 100.3 µs` | `551.5 / 107.2 µs` | `950.3 / 185.1 µs` | `660.4 / 214.8 µs` | `923.4 / 301.0 µs` |

段冷创建与稀疏性实测：macOS arm64 与 Linux x86_64（容器 `/dev/shm` `2 GiB`）上 `4..256 MiB` 段写页前物理块与映射 RSS 均为 `0`，写入 `4 KiB` 后仅占 `4 KiB`；Windows 11 x64 上首次懒创建 `64 MiB` 与 `256 MiB` 段时进程 `WorkingSet` 中位增量仅约 `736–748 KiB`，但 `CreateFileMappingW` 默认 `SEC_COMMIT` 立即扣减系统提交额度 `CommitTotal` 约 `+64.00 MiB` 与 `+257.20 MiB`（销毁池后完整归还）。热复用下三平台 `16 MiB` 与 `64 MiB` 的 P50 相差均在 `0.1%–1.7%` 以内，Linux 在 `1 MiB` 载荷下 `64 MiB` 比 `256 MiB` 快 `15.6%`。原始基准结果均保存在本地被忽略目录，单机微基准不替代跨平台正式门禁。

### 3. 段几何与服务端阻塞线程池推导公式

- **段几何由该域内存预算 `M` 确定性推导（`MAX_BUDDY_SEGMENT = 64 MiB`）：**
  - **主池（请求池 / 响应池）：** 令 `quarter = floor(M / 4)`；当 `M >= 256 KiB`（`quarter >= 64 KiB`）时，`max_pool_segments = min(16, floor(quarter / 65536))`（`M >= 4 MiB` 起为 `16` 段），`pool_segment_size = min(64 MiB, prev_pow2(floor(quarter / max_pool_segments)))`（默认 `M = 16 GiB` 时为 `64 MiB × 16 = 1 GiB`，对一切 `M >= 256 KiB` 严格满足主池总容量 `<= M / 4`，完整落在对端映射缓存 `255` 槽上限内，且将 Windows 单池首次建段的系统提交额度降低 4 倍）；当 `M < 256 KiB` 时不建 buddy 段，直接走内联、独占段或分片。
  - **重组池：** 令 `sixteenth = floor(M / 16)`；当 `M >= 1 MiB`（`sixteenth >= 64 KiB`）时，`reassembly_max_segments = min(4, floor(sixteenth / 65536))`（`M >= 4 MiB` 起为 `4` 段），`reassembly_segment_size = min(64 MiB, prev_pow2(floor(sixteenth / reassembly_max_segments)))`（默认 `M = 16 GiB` 时为 `64 MiB × 4 = 256 MiB`，对一切 `M >= 1 MiB` 严格满足重组 buddy 容量 `<= M / 16`）；当 `M < 1 MiB` 时不建重组 buddy 段，直接走独占段或文件溢出 `F`。
  - **单次重组上限：** 客户端取对端路由通告的 `route.max_payload_size`，服务端取自身的 `max_payload_size`，删除独立的 `max_reassembly_bytes`。
- **服务端 Tokio 阻塞线程池与内部拷贝门控由主机并行度 `P` 与 `W` 推导：**
  - 设 `P = available_parallelism()`（取不到时回退 `4`），`W = max_execution_workers`（默认 `clamp(P, 4, 64)`，显式值须 `>= 1`）：
  - 入站分片拷贝并发门控 **`C = clamp(P, 4, 64)`**，流条目编码（`stream::await_encoding`）并发门控 **`E = clamp(P, 4, 64)`**；
  - `ServerRuntimeBuilder::build` 显式校验算术不溢出（`checked_add`）并设置 `max_blocking_threads(T)`，其中 **`T = max(512, W + C + E + 16)`**，异步 I/O 线程保持 `2`。三类工作在提交阻塞闭包前各自获取对应许可并持有至闭包结束，恒留 `>= 16` 个阻塞线程供监听器启停与收尾，根除高 `W` 或高并发流编码对底层拷贝的饿死；同时解耦 `PreparedShmResponse::fill` 跨外部 `write_range` 的响应池读锁持有区间，避免阻塞读循环处理 `RELEASE`。

## 三、角色域 `(M, F, N)` 计费与准入闭合

### 1. 内存预算 `M` 与文件溢出预算 `F` 的真实计费边界

- **允许在调用截止时间内等待（仅发起方首次申请）：** 客户端输入序列化前的首次预留（发完输入即释放）、客户端接收服务端流条目的整条预留（预留成功才发 `CREDIT`）、客户端接收 HTTP 一元响应体预留、`relay` 转发入口首次 `reserve_send_buffers`（一次性预留输入 + 上传窗口 + 下载窗口）。
- **从不等待（`try_reserve` 立即成功或失败）：**
  1. 新建 buddy SHM 段或独占 SHM 段向 `M` 申请（已建 buddy 段内复用块不重复计费，段空闲回收时退回 `M`；服务端响应分配失败自动回退到单帧不超 `128 KiB` 的分片写出）；
  2. **分片重组与真实可达的文件溢出 `F`：** 移除合并后会阻断文件溢出的前置统一预检，重组分配在内存中建段/缓冲时计入 `M`，当 `M` 不足或触发 `SPILL_THRESHOLD` 时直接向 `F` 申请临时文件映射（不双计同一物理存储）；服务端重组完成生成资源方法的请求本地副本 `Vec<u8>` 时，真实向 `M_server` 申请本地副本字节（独占段可原子释放原段额度，共享 buddy 段与 `F` 文件按各自生命周期释放）；
  3. **服务端请求本地副本与每请求待拷贝分片队列：** 内联/单帧 `OPEN` 在插入活动调用表前先对请求字节执行 `M_server.try_reserve`；分片请求在读循环接纳每帧时，在复制内联字节前先对“帧载荷字节 + 队列节点存储开销”执行 `M_server.try_reserve`（同时刷新收片活跃时间戳），推入该请求自身的待拷贝帧队列，不再每帧派生独立 Tokio 任务；全 Server 按请求轮转调度最多 `C` 个阻塞拷贝任务，且续片仅在首片发布重组存储后才参与调度，真实拷贝完成后立即释放该帧的工作字节预留。

### 2. 在途调用上限 `N` 与六处准入/生命周期闭合

每个角色域（`client` 域、`server` 域）与每个 `relay` 域的在途调用上限 `N` 解析规则一致：当该域 `M == 0` 且显式配置 `L > 0` 时返回配置错误；当 `M == 0` 且未设正数 `L` 时 `N = 0`（关闭该域全部新业务准入，包括 0 字节调用与空流，保留拒绝、释放与关闭排空）；当 `M > 0` 时取显式 `L`（含 `0`）或默认 `max(64, floor(M / 65536))`。

1. **全调用统一覆盖与同进程直调错误边界：** 客户端域覆盖全部远端一元调用（含无截止时间调用）与服务端流；同进程 Python `thread_local` 直调立即计入目标 `Host` 的 `N`（不激活 `client` 域），且已进入本地资源方法后的异常出口统一经 Core 错误归一化剥离内层错误的 `transport_phase` / `dispatch_phase`，防止外层调用者将已改状态的失败误判为 `pre_dispatch` 重试；`relay` 每次转发共用 1 个 `relay` 域 `N`。名额一律立即给或立即拒（`717 CallCapacityExceeded`，`stage=call_execution_admission`，`relay` HTTP 状态码统一为 `503`），跟随真实执行/转发责任直至真实终态。
2. **服务端 `OPEN` 准入前移：** 拆分连接请求号高水位推进与 `CallTable` 活动记录插入；新 `OPEN` 先校验 `N_server` 与首帧/整包字节预算，通过后再建活动调用记录，拒绝时仅推进请求号高水位、归还附带的 SHM 借出并入队 `NotExecuted`，迟到续片由高水位直接丢弃。
3. **服务端控制回复每连接隔离：** 删除全 Server 共享的 `control_reply_budget` 信号量；每条物理连接的 `ConnectionWriter` 独立持有有界非阻塞控制槽 `Q_ctrl = min(max(1, N_server), 64)`，单连接不读响应仅触发该连接自身的 `reply_pending_capacity` 停用，绝不波及其他健康连接。
4. **删除 `relay` 公开 `/_echo`：** 从 `c2-http/src/relay/router.rs` 删除无外部使用者的 `POST /_echo` 路由、处理函数及自测 `data_plane_echo_allows_large_payloads`。
5. **`relay` 准入前错误回复不等待：** `budgeted_json_response` 等准入前拒绝路径改为不等待 `M` 的控制回复（`try_reserve` 失败时退回有界静态控制响应），防止满预算时堆积等待流。
6. **`relay` 一元响应名额持有与多片下载窗口复用：** `unary_response` 与 `stream_response` 均持有转发名额至响应 `Body` 完成/丢弃及真实上游终态；入口预留的下载窗口由该次转发自身持有并在每个 `Bytes` 切片释放后循环复用，整次响应结束才归还公共 `M`，消除持有上游重组缓冲期间与新入站请求二次争抢 `M`。

## 四、顺带清理与诊断观测结构

- **`R09` / `R10`（`EnvCatalog` 单次加载）：** 每个角色域激活解析与 `c3 relay` 启动解析各只读取一次 `EnvCatalog`，消除一次启动重复读取四次 `.env`。
- **`C4`（删除客户端池重复配置比对）：** 客户端传输参数全部成为常量、客户端域由 Core `Runtime` 唯一冻结后，删除 `c2-ipc/src/pool.rs` 中 19 个字段的 `differing_client_policy_fields` 与 `client_policy_mismatch`。
- **对称诊断快照（`cc.observe.memory_stats()` 与 `call_execution_snapshot()`）：**
  - `memory_stats()` 固定输出 `client`、`server`、`retired`、`holds` 四项：每个角色项含 `domain_id`、`generation`、`frozen`、`closed`、`memory`（五项计数单元或未激活时 `None`）、`file_spill`（五项计数单元或未激活时 `None`），删除旧 `runtime_outgoing`、`payload_buffer`、`retired_payload_buffers` 的重叠拆分；观察操作纯只读，绝不读取环境、创建域或触发冻结。
  - `call_execution_snapshot()` 按相同的 `client / server / retired` 角色结构投影各自域的 `max_operations`、`used_operations`、`peak_operations`、`rejected_reservations`、`closed`（未激活角色计数为 `None`）；Rust `Runtime` 同样可直接观察其关联 `Host` 域快照。

## 五、分块顺序与验收门禁

分块只为控制单次改动半径，全部合入同一特性分支 `socu/config-convergence` 后统一验证：

1. **块 1（`c2-config` 与 `c2-mem`）：** 单次 `EnvCatalog` 解析、13 个环境变量、三态配置补丁（`Keep / Inherit / Set`）、默认 `N` 公式与 `M == 0` 校验、`SHM_THRESHOLD = 16 KiB` 与 `MAX_BUDDY_SEGMENT = 64 MiB` 段几何推导函数，以及支持阻塞等待、非阻塞 `try_reserve`、可重用转发窗口与真实可达文件溢出 `F` 的统一角色域预算与调用槽原语。通过 `c2-config` 与 `c2-mem` 全量单测。
2. **块 2（`c2-wire`、`c2-ipc`、`c2-server`）：** 对称握手/业务帧长常量（`8 MiB` / `1 MiB`）与有界错误载荷、删除 `C4` 比对、显式派生服务端 Tokio 阻塞池 `T = max(512, W + C + E + 16)` 与 `C / E` 门控、解耦 `PreparedShmResponse::fill` 锁区间、服务端 `OPEN` 准入前移、每请求待拷贝分片队列与首片后轮转调度、重组到 `F` 回退及请求本地副本真实计费、每连接控制回复槽隔离。通过三者全量测试及高并发/窄窗口小分片/慢对端隔离用例。
3. **块 3（`c2-http` 与 `c2-core`）：** 删除 `C2_RELAY_CALL_TIMEOUT`、删除 `/_echo`、闭合准入前错误响应不等待、一元响应名额生命周期与多片下载窗口复用；在 `c2-core` 中落地按需懒创建的独立 `client` / `server` 角色域、Core 单 `Host` 状态槽、全调用 `N` 准入、三态合并与同效幂等冻结及对称观测投影。通过 `c2-http` 与 `c2-core` 全量测试（含 `A -> B -> C` 满载无互等、`B -> B` 自连接、`M = 0` 闭锁与 `Host` 重启计费保留测试）。
4. **块 4（SDK、CLI、ABI 8 与文档）：** 更新 Rust SDK（`RuntimeOptions` / `HostOptions`）、Python SDK（`set_client` / `set_server`、同进程直调计 `Host N` 与剥离内层 `pre_dispatch`、会话轮换原子移交与有类型观测快照）、`c3 relay` CLI、Node ABI 8（无副作用 `stats()` 与移除旧选项）及代码生成模板；同步更新 [`CONTEXT.md`](../../CONTEXT.md)、`AGENTS.md`、`.env.example`、`docs/reference/`（`configuration.md`、`memory-policy.md`、`runtime-lifecycle.md`、`python-usage.md`、`c-abi.md`）、`cli/README.md` 与 `docs/releases/unreleased.md`，删除所有只钉旧配置名的测试。
5. **总门禁：** 本地跑通 Rust 工作区、Rust SDK、原生嵌入测试、Python 全量测试（含 Python 3.10 语法检查与 18 行跨语言矩阵）、Node ABI 8 测试与仓库文档卫生检查，并在空 `HOME` / 故障注入下核验边界；随后在 GitHub Actions 上跑通 Linux（3.12 / 3.14t）、macOS、Windows x64 与 ARM64 全量 Source Validation 及配对 IPC 基准，保留提交哈希与运行记录。

块 4 的现行入口和诊断字段见[配置参考](../reference/configuration.zh-CN.md)、[内存策略](../reference/memory-policy.md#statistics-and-payload-lifetime)及[未发布变更说明](../releases/unreleased.md#configuration-convergence-breaking)。文档门禁覆盖链接、双语互链、路径和指令大小。跨平台验收以合并后的源码提交及运行记录为准。
