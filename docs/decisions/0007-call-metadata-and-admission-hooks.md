# 调用元数据与准入钩子

状态：已采用（2026-11-23），拟定 0.13.0 实施。

## 决定

C-Two 引入**跨传输统一的调用元数据（call metadata）**、**Host 级单一准入钩子（admission hook）**与**资源方法内只读调用上下文（call context）**，同时借 0.x 未发布协议窗口完成干净切割：IPC 扩展并入未发布的 **IPC 18** 与 **C/FFI ABI 10**，relay 切至 **Mesh 8**、**`application/vnd.c-two.topic-stream;version=2`** 与统一 **`/_c2/*`** 数据面路由，Rust 客户端退役 `UnaryCallOptions` 并全三端统一到 `with_call_options`。

### 1. 信任边界与机制定位

- **只提供机制，不内置策略（见[终局架构](../design/endgame-architecture.md)）：** C-Two 负责在 `thread_local`、直接 IPC 与 `c3 relay` HTTP 三条路径上安全、一致地传输有界调用元数据，并在进入资源、结局存储或主题订阅前调用 Host 注册的准入钩子。身份认证（JWT/OIDC/mTLS）、权限模型（ACL/RBAC）、多租户隔离、审批流与审计落盘全部归属上层应用。
- **真实信任边界：**
  - **不可信输入：** 远端 IPC/HTTP 调用方、线协议帧、HTTP 头、客户端传入的 `metadata`，以及嵌套 A→B 调用中下游资源抛出的错误（B 绝不能伪造外层 A 的顶层 `724 AdmissionDenied` / `pre_dispatch`）。
  - **可信组件：** 同进程 Rust/Python Host 代码、Host 自身注册的 `admission_hook`（出现未捕获异常或 panic 时一律 fail-closed），以及作为内部路由转发层的 `c3 relay`（relay 无策略裁决权，共享主题汇聚中每位订阅者的准入结论均来自持有该路由的 Host）。
  - **明确不在范围：** 同进程恶意内存篡改、钩子 OS 沙箱、TLS/mTLS 终结、响应元数据/trailers、冻结后热替换钩子、服务端 `async def` 资源方法，以及 TypeScript HTTP 服务端流（维持 `UnsupportedCallMode`）。

### 2. 调用元数据模型与三端统一调用选项

- **`CallMetadata`（`c2-config`）：**
  - 内部为 `Option<Arc<BTreeMap<String, String>>>`（空映射为 `None`，零堆分配、单字廉价 `Clone`）。
  - 单值有序映射，键严格升序；不支持多值键，不支持 `-bin` 二进制后缀。
  - **硬性边界：**
    - 条目上限：`MAX_CALL_METADATA_ENTRIES = 64`。
    - 键：`1..=64` 字节 ASCII `[a-z0-9_.-]`（严格小写）。
    - 值：`0..=4096` 字节可打印 ASCII（`0x20..=0x7E`，禁止控制字符、HTAB、CRLF 与非 ASCII），**禁止首尾空格（`0x20`）**。
    - 总字节上限：`sum(key.len() + value.len()) <= 16_384`（`MAX_CALL_METADATA_TOTAL_BYTES`）。
  - **跨传输规范化边界：** Rust/Python/TS SDK、IPC 编解码与 C/FFI ABI 严格拒绝大写键、重复键（含经自定义 `serde::de::Visitor` 拒绝重复 JSON 键）与首尾空格；HTTP 路径上 Axum/Hyper 按 RFC 9110/9112 在进入处理器前将字段名小写化并裁剪 HTTP OWS，relay 逐头枚举全部值以拒绝重复键并校验规范化后的键值，超出 Hyper HTTP/1 头数（100）或头缓冲上限的请求可在入路由前收到 HTTP `431`。
- **全三端统一 `CallOptions` / `with_call_options`：**
  - `with_call_options` 是三端承载可复用视图级 `timeout` 与 `metadata` 的唯一入口，统一覆盖普通一元、hold、带键一元、恢复、结局查询、服务端流、主题订阅与同进程 `thread_local`。
  - **Rust（`c2-config` / `c_two` / `c2-codegen`）：**
    - **退役 `UnaryCallOptions`：** `CallOptions` 由 `Copy` 改为廉价 `Clone`（保留 `CallOptions::new()` 与 `CallOptions::with_timeout(timeout)`，新增 `with_metadata`、`with_metadata_entry`、`extend_metadata`、`clear_metadata`、`metadata()`），成为 Rust SDK 唯一选项类型。
    - `Client::recover_unary_call`、`recover_unary_call_held`、`query_call` 及生成的 `ContractClient::recover_*` / `query_call` 移除冗余 `timeout` 参数，统一继承 `self.options` 中的 `timeout` 与 `metadata`。
    - 带键一元由 `*_with_options(UnaryCallOptions)` 改为 `Client::call_unary_with_call_id` / `call_unary_held_with_call_id` / `begin_keyed_call` 及 `ContractClient::{method}_with_call_id` / `hold_{method}_with_call_id`，仅传 `call_id`，同时继承 `self.options`。
  - **Python（`c_two`）：**
    - `cc.with_call_options(proxy, *, timeout=_UNSET, metadata=_UNSET)`：省略参数保持原视图值；`timeout=...` 重置为路径默认（`Inherit`）且保留元数据，`timeout=None` 为无时限；`metadata=None` 清空元数据，`metadata=Mapping[str, str]` 基于父视图浅拷贝覆盖合并并校验合并后上限（空 `{}` 保留原键）。
    - `cc.query_call(proxy, call_id, *, timeout=_UNSET)` 继承 `proxy` 的视图 `timeout` 与 `metadata`（可选 `timeout=` 仅覆写超时，不再重复传 `metadata=`）。
    - `_remote_snapshot` 与 `_outcome_target` 在 `timeout` 为 `Inherit` 时同样透传 `metadata`；`_outcome_target` 在获取本机 IPC 客户端之前即启动有限截止时间，覆盖建连与调用全过程。
    - 顶层仅新增 4 个导出（`AdmissionContext`、`AdmissionDecision`、`CallContext`、`current_call_context`）及 `cc.error.AdmissionDenied`。
  - **TypeScript（`c2-codegen`）：**
    - 生成的 `ContractClient` 新增 `withCallOptions({ timeoutMs?, metadata? })`，采用与 Python 一致的省略保留、`null` 清除/无限、对象覆盖合并语义。
    - 一元方法保留每调用 `{ callId?: string }` 并继承视图 `timeoutMs` 与 `metadata`；`recover_*`、`queryCall`、流与主题订阅统一继承视图选项。
    - 订阅超时优先级：`TopicSubscribeOptions.timeoutMs` > 视图 `timeoutMs` > 订阅默认无时限（`null` 显式无时限，视图 `0` 立即超时；relay 一元 300s 默认绝不套用到订阅；IPC `nativeLimit` 不得用传输默认值截断显式视图预算）。
    - `typescript_transport.ts` 将大小写不敏感的 `x-c2-meta-` 前缀保留给调用元数据，禁止通过传输级自定义头注入；元数据仅附着到业务调用与主题端点，不发往 `/_probe` 或 `/_resolve`。

### 3. 线协议（IPC 18）、`c3 relay` HTTP / Mesh 8 与 C/FFI ABI 10

- **IPC 18 扩展（并入未发布 IPC 18）：**
  1. **服务端单向握手能力位：** `CAP_ADMISSION_HOOK: u16 = 1 << 5`，仅合法出现在服务端→客户端握手 ACK（`response == true`），客户端主动宣告即拒绝；反映该 Host 冻结的 `admission_hook.is_some()`，relay 从实际建立上游的 IPC 连接读取。
  2. **紧凑规范元数据编码：** `[meta_count: u8 (0..=64)]` 后跟严格按键升序排列的 `[key_len: u8 (1..=64)][key][val_len: u16 LE (0..=4096)][val]`（最大 `16_577` 字节；空元数据仅占 **1 字节 `0x00`** 且零堆分配）。附于所有 `OPEN`（`Unary`、`ServerStream`、`KeyedUnary`、`Recover`、`Subscribe`）与 `QueryCall`（kind 11）。
  3. **分片宽度与载体巡检：** 发送端按 `min(CHUNK_SIZE, payload_upload_window, first_frame_remaining_space)` 一次性计算分片宽度（绝不从上传窗口扣减元数据长度，保留 1 字节窗口转发能力；空输入仍走 `Whole`）；`rpc::inspect_carrier` 跳过元数据段定位并释放 SHM 借出，保证语义非法的元数据也不会泄漏 SHM。
  4. **`KeyedUnary` 输入摘要与跨主体边界：** `HostOutcomeStore` 的 `(method_idx, SHA-256(input_bytes))` 明确**排除** `CallMetadata`，使同一主体在重发时可刷新短期 JWT 或轮换 `traceparent` 而不撞 `721`；需要跨主体隔离的上层必须在 `admission_hook` 内对 `KeyedUnary`、`Recover` 与 `QueryOutcome` 校验 `call_id` 归属。`QueryOutcome` 保持 `target = None`，绝不在钩子前偷看结局表拼装 `target`。
  5. **Subscribe `READY` 扩展至 16 字节：** `[start_seq: u64 LE][head_seq: u64 LE]`（`start_seq >= 1`），让主题汇聚创建者 S1 在零额外 RTT、无重复钩子调用下拿到准入时刻的原子 `head_seq`。
  6. **独立非阻塞主题准入控制记录对（`AdmitTopic = 13` / `AdmitTopicResult = 14`）：** 经该汇聚已绑定的 `IpcTopicSynchronizer` 在持有上游订阅的同一物理 IPC 连接上发送，分配独立写侧 `request_id`，受控制并发配额 `Q_ctrl` 约束。请求载荷 `[subscription_stream_id: u64 LE][metadata]`；响应载荷 `[0u8][head_seq: u64 LE]`（放行）或 `[1u8][有界 C2E1 错误字节 <= 4096 B]`（`724` / `717` / `713` / `707`）。
- **`c3 relay` HTTP 统一化、`topic-stream;version=2` 与 Mesh 8：**
  - **Mesh 7 → Mesh 8（`PROTOCOL_VERSION = 8`）：** 阻止旧版 `c3 0.8.0` 与新版 relay 互联。
  - **主题流媒体类型升至 `application/vnd.c-two.topic-stream;version=2`：** `200 OK` 必带 `x-c2-topic-admission-required: 0|1`（缺失/重复/非法报 `713`）与 `x-c2-topic-head-seq: <u64>`。
  - **固定 `/_c2/*` 数据面路由（退役 `POST /{route}/{method}` 与 `POST /{route}/_topics/{topic}[/_confirm]`）：**
    - `POST /_c2/calls/call?route=<pct>&method=<pct>`（`data_router`，一元与服务端流）
    - `POST /_c2/calls/keyed?route=<pct>&method=<pct>`（`data_router`）
    - `POST /_c2/calls/recover?route=<pct>&method=<pct>`（`data_router`）
    - `GET  /_c2/calls/outcome?route=<pct>`（`control_router`）
    - `POST /_c2/topics/subscribe?route=<pct>&topic=<pct>`（`data_router`，`Accept: application/vnd.c-two.topic-stream;version=2`）
    - `POST /_c2/topics/confirm?route=<pct>&topic=<pct>`（`data_router`，受 relay `Q_ctrl` 约束，携带 `x-c2-relay-origin-only: 1`、`x-c2-expected-registration`、`x-c2-topic-upstream-token` 与订阅者 `x-c2-meta-*`）
    - `DELETE /_calls/{token}`（`control_router`，不走元数据与准入钩子，保证令牌过期后仍可取消）。
    - **统一 HTTP 错误分类：** 将原有仅限 `outcome` 的裸 404 防护并入全部六条 `/_c2/*` 路由——非规范裸 404 保持 `dispatch_uncertain`，仅 `404` 加规范 `701` 信封且阶段不冲突时才认作 `ResourceNotFound`。
  - **头映射与独立有界控制错误通道：** 元数据 `(k, v)` ↔ HTTP `x-c2-meta-<k>: <v>`；`724` 映射为 HTTP `403 Forbidden`（`C2E1` JSON，`dispatch_phase = "pre_dispatch"`）。relay 返回有界规范错误体时不向业务内存 `M` 借配额（避免业务内存受压把 `718`/`724` 覆写为 `717`）；锚点 relay B 与 Rust/TS 客户端对称地通过有界控制读解析错误体，并统一校验 HTTP 状态码、`C2E1` 错误码与 `dispatch_phase`。经 relay 的服务端流同时接受 200 前 `403 + 724` 与 200 后 0 条目 `NotExecuted(724)`，均投影为 `724 AdmissionDenied`（`pre_dispatch`）。`724` 与单次调用的 `stage="call_metadata_validation"` `713` 均绝不将共享目标标为 `TERMINAL_REFUSAL`。
- **`TopicHub` 共享上游准入协议（单跳 A→H 与两跳 B→A→H）：**
  - `requires_admission == false` 时完全沿用 ADR 0005 批处理快路径（零额外 RTT）。
  - `requires_admission == true` 时：
    1. **创建者 S1 与并发等待者 S2 干净重竞争：** S1 携带自身 `metadata` 开启上游并经私有 `leader_result` 拿裁决。若 S1 遭遇私有拒收（`724` 或准入 `717`），仅向 S1 返回该错误；驱动标记 `opening_abandoned = true`，按 `Arc::ptr_eq` 从 `TopicHubTable` 摘除该实例并唤醒 `hub.changed`；等待者观察到 `opening_abandoned && source.is_none() && terminal.is_none()` 后返回 `RetryJoin`，释放旧 `JoinGuard` 并携自身 `metadata` 与原订阅槽重新竞选创建。若 S1 成功，直接用 `READY` / `x-c2-topic-head-seq` 的原子 `head_seq` 计算 `start_seq`（不重复触发钩子）。
    2. **后期加入者与 `x-c2-topic-upstream-token` 生命周期：** 在 A 处 S2 经 `IpcTopicSynchronizer` 发单次 `AdmitTopic`；在 B 处保存 A 返回给 `origin-only` 订阅的 `x-c2-call-token`，于每次 `POST /_c2/topics/confirm` 以 `x-c2-topic-upstream-token` 回传。A 将该令牌绑定到具体的 `(upstream_subscriber, hub, registration)`，在取消或脱离时立即吊销，await `AdmitTopic` 期间持有匹配汇聚引用并在 Host H 返回后复验令牌与汇聚活性——若已吊销则返 `707 ResourceClosed`（只隔离 B 的死上游，不波及 A 的本地订阅者）。`717` 与 `724` 均不隔离共享汇聚。
- **C/FFI ABI 10（`c2-mem-ffi`）：**
  - 原子更新 C 头（`c2_mem_ffi.h`）、Rust 导出、N-API 动态加载器（`node_c2_mem_ffi_loader.c`）与 TS 绑定：`call_begin`、`recover_call`、`query_call` 增加可空 `const char *metadata_json`；`subscription_open` 在 `options_json` 中扩展可选 `"metadata"` 字段。所有借用输入在返回前拷贝并严格校验（含重复 JSON 键拒绝），保留 `NULL = 无限`、`0 = 立即到期` 与 `query_call` 队列等待计时。

### 4. 错误码 `724`、准入钩子与 `c2-server` 状态机

- **规范错误码 `724 AdmissionDenied` 与回调边界隔离：**
  - 注册为 `CallStatus::NotExecuted`、`TransportPhase::PreDispatch`、HTTP `403`。钩子不可覆盖保留详情键（`transport`、`transport_phase`、`dispatch_phase`、`stage`、`call_id`）。
  - `ErrorCode::is_host_control()` 扩展为 `721 | 722 | 723 | 724`，在全部 4 处回调出口（`c2-core`、`c2-server` 一元/带键结算、`c2-server` 流 `crm_error_bytes`、Python `_invoke_local`）将资源内部未捕获的 `724` 包装为 `3 ResourceFunctionExecuting`（原码入 `cause_code=724`，剥离 `pre_dispatch`）。
- **Host 级配置、上下文与裁决模型：**
  - Rust 经 `HostOptions::with_admission_hook(Arc<dyn AdmissionHook>)`，Python 经 `cc.set_server(admission_hook=fn)` / `Runtime.set_server(admission_hook=fn)` 配置，随 `server` 角色冻结；Python `Runtime` 停机后再次 `register()` 沿服务端覆写交接态保留已配钩子（并发启动落败方不覆写胜出方），且全部 Python 回调/扩展对象的克隆、执行与释放严格在 Rust 锁外完成。冻结后传入 `Keep` 或同一对象（`Arc::ptr_eq` / Python `is`）幂等通过，其余变更报 `LifecycleError::ConfigFrozen`。
  - 只读 `AdmissionContext` 包含：`route_name`、`crm_ns`、`crm_name`、`crm_ver`、`target: Option<&str>`（方法名、主题名或 `QueryOutcome` 的 `None`）、`access: Option<MethodAccess>`（严格取自已校验契约声明的 `Read`/`Write`，不从调度锁推算；主题与 `QueryOutcome` 为 `None`）、`operation`（`Unary`/`ServerStream`/`Subscribe`/`Recover`/`QueryOutcome`）、`transport`（`ThreadLocal`/`Ipc`）、`call_id`、`metadata`。
  - `AdmissionDecision` 仅含两个变体：`Allow { extension }` 与 `Deny(AdmissionRejection { message, details })`。Python 钩子返回 `None` 或 `AdmissionDecision.allow(extension=...)` 放行，返回 `AdmissionDecision.deny(message, details=...)` 拒绝；**任何未捕获异常（含下游 `AdmissionDenied`）或未识别返回值一律 fail-closed 为 `724 AdmissionDenied`（`message="admission hook failed"`，`details["reason"]="hook_error"`）**。
  - `extension` 仅在真正进入执行的一元调用或已开启的服务端流存活；对 `QueryOutcome`、`Recover`、命中去重的重复 `KeyedUnary` 以及 `Subscribe` 在锁外立即丢弃。
- **`c2-server` 精确状态机、锁序与排空不变量：**
  - 无钩子时保持原同步零开销快路径；配置钩子时遵循六项不变量：
    1. **有界并发与禁重入约束：** Host 级钩子并发配额 `A = clamp(P, 4, 64)`（`admission_permits`）纳入 `derive_for_host`（`max_blocking_threads = max(512, W + C + E + A + 16)`）。**重入约束：** 钩子不得同步调用或查询**同一个 Host**（无论是在同一连接 FIFO 后排队，还是跨连接占满 `A` 配额时）。
    2. **`dispatcher.read()` 内同步预登记 `PendingAdmission`（全部 `OPEN` 变体）：** 在 `advance_open(id)` 与轻量结构校验后，于单次 `dispatcher.read()` 内确认 Host 与路由开放、捕获 `admission_generation` 与精确 `route_uid`、铸造连续 `FlightGuard`、占用业务槽（`reserve_operation()` / `reserve_subscription()`），并同步 `insert_open(id, ...)` 插入 `PendingAdmission` 状态的 `CallRecord`（`ServerStream` 同步 `stream_open`，`Subscribe` 同步 `subscribe_open` 并保留初始需求与逐项 `CREDIT` 校验，使 `READY` 前的早到 `CANCEL` 不会误断连接）。无记录的 `queue_open_refusal` 仅用于 `insert_open` 之前的拒收。
    3. **统一非淘汰的准入前输入暂存（`Whole` + `First`/`Continuation`）：** `PendingAdmission` 期间一律通过 `domain_budget.try_reserve_memory` 预约**非淘汰**角色内存（绝不调用 `shed_for_pressure`，也不入 `copy_scheduler`），在统一暂存槽保存帧字节与绑定连接的 `PeerShmRequest` 所有者。续片在单次原子锁更新中校验 `ValueReceive` 长度并挂载；若拒绝/退役先拿锁，续片观察到已退役状态后干净释放 SHM 借出，不误报协议错误。分片 GC 计时器仅在仍有缺失分片时计时。
    4. **每连接业务 FIFO 准入队列：** `Unary`、`KeyedUnary`、`ServerStream` 与 `Recover` 按到达顺序在懒启动的每连接 FIFO 任务中顺序执行钩子与准入后提交（`admit_with` / `recover_with` / `reserve_pending`），提交完立即推进下一节点（不等剩余分片、SHA-256、重放交付或资源执行）。对排队中的流/订阅收到早到 `CANCEL` 时立即退役其 `CallRecord` 并在 FIFO 节点标取消（O(1) 跳过、不持 `CallRecord`、不泄漏 `server.N`）。`Subscribe`、`QueryCall`、`AdmitTopic` 绕过业务 FIFO 并发执行。
    5. **控制操作双非阻塞配额、钉住注册与严格锁序：** `QueryCall` 与 `AdmitTopic` 均须非阻塞同时获取 `Q_ctrl` 与 `admission_permits.try_acquire_owned()`，任一饱和立即返 `717`（零钩子调用、不堆积无界等待者）；结构有效但元数据语义非法时返 `Error(713)` 而不关闭连接。`QueryCall` 在钩子前钉住 `route_uid`，放行后于 `dispatcher.read()` 内核验该 `route_uid` 仍存活后才查 `HostOutcomeStore`（否则返 `ResourceClosed`）。`AdmitTopic` 放行后严格按 **`dispatcher.read() → conn.calls`** 锁序（与路由注销的 `dispatcher.write() → conn.calls` 同向，杜绝 ABBA 死锁）校验该物理连接上对应 `subscription_stream_id` 仍为活跃未取消的同路由同主题 `Subscribe` 记录，并在同一锁边界读取 `head_seq_if_open()`。
    6. **连续排空与生命周期真实性：** `FlightGuard` 从预登记 → FIFO → 钩子任务 → 业务执行或终态写队列全程连续传递；阻塞钩子闭包本身持有 `admission_permits` 与 Host 活跃准入计数直至物理返回，确保 `Host` 停机等待全部运行中钩子及其终态回复完成。连接断开退役等待中的 `PendingAdmission` 记录；普通停用允许已收记录在既定 `admission_generation` 下完成。

### 5. 资源方法内的只读调用上下文与 `thread_local` 一致性

- **只读 `current_call_context()`：**
  - Rust：`c_two::current_call_context() -> Option<CallContext>`（含 `ctx.extension::<T>() -> Option<Arc<T>>`），由包裹 `CrmCallback::invoke` 与 `CrmStream` 每次 `open_stream` / `step` / `finish` 的 `thread_local!` RAII 栈守卫提供。
  - Python：`cc.current_call_context() -> CallContext | None`（`route_name`、`target`、`operation`、`call_id`、`metadata: MappingProxyType[str, str]`、`extension`），由 `contextvars.ContextVar` 提供。
- **同进程 `thread_local` 一致性与上下文遮蔽：**
  - 通过 `cc.connect` 代理发起的每条 `thread_local` 一元、流或主题调用，均先钉住已解析的 `(route_uid, resource/scheduler/RouteTopics)` 快照再跑 Host 钩子（若提交前已重注册则重新准入）。
  - `_outcome_target()` 将视图 `timeout` 与 `metadata` 一并传给本机 IPC 客户端，由 IPC Host 执行一次准入（`transport=Ipc`）。
  - Python `_ResourceStream` 显式接收被调方 `CallContext` 并在自身私有 `self._context` 中覆盖 `_start`、每次 `next()` 与 `_cleanup`，返回、异常或取消时恢复外层上下文。
  - **上下文遮蔽不变量：** 即便嵌套同进程内层调用 B 无钩子且元数据为空，B 也必须绑定自己的空 `CallContext`，确保 B 绝不会读到外层 A 的 `metadata` 或 `extension`。

## 考虑过而没有采用

- **路由级或方法级装饰器注册钩子：** 容易遗漏方法、`recover_call`、`query_call` 或主题订阅，污染领域资源类定义，且无法在无目标方法名的 `QueryOutcome` 控制面生效。
- **支持 multi-value 与 `-bin` 二进制元数据：** 会在 IPC 编码、HTTP 多头合并、JSON ABI 与三端 SDK 字典抽象间引入歧义和 Base64 负担，超出控制元数据需求。
- **将元数据纳入 `KeyedUnary` 的输入摘要（SHA-256）：** 会导致客户端在重试或恢复时刷新过期 JWT 或更换 `traceparent` 误撞 `721 CallIdConflict`。
- **保留旧版 `POST /{route}/{method}`、`POST /{route}/_topics/*` 或 Rust `UnaryCallOptions`：** 0.x 阶段不背双轨兼容负担；保留旧路径还会维持动态路径段与特殊路由名冲突的历史包袱，保留两套 Rust 选项类型则割裂了三端一致的 `with_call_options` 心智模型。
- **在 `TopicHub` 中复用创建者 S1 被拒后的同一汇聚实例或睡眠重试：** 会让并发等待者 S2 被 S1 的私有拒绝连坐或死锁；采用 `opening_abandoned` 摘除 + `RetryJoin` 重竞选既干净又无盲等。
- **在 Python 钩子里捕获异常并按异常类型/消息放行或定制 `724`：** 极易把钩子内部调用下游资源时意外未捕获的 `AdmissionDenied` 误判为当前 Host 主动拒收；统一要求显式返回 `AdmissionDecision.deny(...)`、一切异常一律 fail-closed 为 `hook_error` 更安全。

依据：[一手来源调研](../research/2026-11-23-call-metadata-and-admission-hooks-survey.md)、[ADR 0001](0001-single-owner-release.md)、[ADR 0003](0003-server-streams.md)、[ADR 0004](0004-route-name-identity.md)、[ADR 0005](0005-publish-subscribe.md)、[ADR 0006](0006-post-dispatch-outcome-and-dedup.md)。
