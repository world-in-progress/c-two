# 派发后调用结局查询与去重

状态：已采用（2026-11-23），拟定 0.13.0 实施。

## 决定

C-Two 为一元调用（unary call）引入**单次注册内存作用域的调用结局查询与去重机制**，采用**路线 1（零额外 RTT：显式提交 + 非阻塞只读查询 + 遇 Miss 绝不代执行的附着重放恢复）**，在不增加普通调用开销的前提下，补齐 `dispatch_uncertain` 之后的查证与安全恢复闭环。

### 1. 作用域、边界与三条正交操作

- **单次注册纯内存作用域：** 结局表归属 Host 上当前活跃的注册（`CrmRoute`），纯内存、不落盘；单路由注销或 Host 停机排空完成后随该注册释放，与 [ADR 0004](0004-route-name-identity.md) 一致（不向客户端外露 `route_uid`）。跨 Host 重启的持久操作日志与资源 revision 对账归上层编排与领域服务。
- **仅限一元调用，显式按次启用：** 不设全局或 `@cc.write` 自动生成开关；未带 `call_id` 的一元、服务端流与订阅调用保持**严格零额外字节与零哈希开销**。`@cc.stream` 与 `@cc.topic` 在编译期（Rust/TS）、包装期（Python）、C ABI 与协议解码边界一律拒绝 `call_id`。
- **`call_id` 语法与唯一性约定：** `1..=128` 字节 ASCII `[A-Za-z0-9_-]`，跨物理连接与跨客户端进程稳定。同一路由的客户端在旧操作仍可能被查询或恢复期间，不得把同一 `call_id` 复用于另一逻辑操作（重注册后若新操作复用了旧 `call_id` 与同一方法名，不带输入摘要的 `recover_call` 可能命中新操作；使用 UUIDv4/ULID 或单调操作键可天然满足）。
- **三条正交操作：**
  1. **带键提交（Keyed Unary Submit，`with_call_id`）：** 0 额外 RTT。命中本注册现有记录且 `(method_idx, SHA-256(input))` 一致时附着等待或重放终态；方法或输入摘要冲突时报 `721 CallIdConflict`；未命中则按常规顺序执行。*诚实边界：* 对已落入 `dispatch_uncertain` 的调用，若重复提交，在“首包极度迟到 + 重试已完成并被容量淘汰 + 首包随后到达”或“Host 重启换注册”情形下无法数学禁止二次执行；**强至多一次调用方应遵守“同一 `call_id` 只提交一次，遇 `dispatch_uncertain` 只走 `query_call`/`recover_call`，遇 `unknown`/`723` 交持久层对账”**。
  2. **非阻塞查询（`query_call`）：** 独立控制通道，绕过客户端/Host/relay 三层的业务调用槽 `N` 与 `max_pending`，绝不执行资源，返回固定有界小快照 `CallOutcome`（`in_flight` / `succeeded` / `failed` / `unknown`；注明 `in_flight` 包含尚未进入资源、仍可撤销的 `PendingInput` 阶段）。
  3. **类型化恢复（`recover_call`）：** 无请求载荷，**遇 Miss 绝不执行资源**，直接以 `723 CallOutcomeUnknown`（`NotExecuted`）拒收；方法不匹配报 `721 CallIdConflict`；命中 `PendingInput`/`InFlight` 时附着等待，命中 `Succeeded`/`Failed` 时重放完整响应或原 `C2Error`（结果未保留或已淘汰时报 `722 CallResultUnavailable`）。

### 2. Host 单锁状态机、唯一入口裁决与排空不变量

- **单锁结构 `HostOutcomeStore`：** Host 以一把短锁维护跨注册的结局索引 `(route_uid, call_id)`、按注册摘除索引与两条完成顺序队列，避免“路由锁 ↔ Host 锁”锁序反转：
  - **`settled_fifo`：** 每条进入终态（`Succeeded` / `Failed`，含零字节成功、超大未保留成功、仅留错误码的失败）都入队一次，严格保证全部终态元数据条数不超过 `outcome_max_entries`（哪怕 `outcome_max_retained_bytes == 0`）。
  - **`retained_fifo`：** 仅持有保留响应载荷或可变 `C2Error` 详情字节的终态条目入队；按 `outcome_max_retained_bytes` 或受压淘汰时原地降级为 `Succeeded(Evicted)` 或 `Failed(details_evicted)`（绝不删除终态元数据），将被淘汰所有者移出锁外 `drop`。
- **首帧拆分校验与取票、保序与代际钉住：**
  - `KeyedUnary` `OPEN` 首帧先完成路由、完整契约、方法存在性与 `Unary` 模式校验（此时尚未取路由 `pending_permit`），再入 `HostOutcomeStore` 锁查 `(route_uid, call_id)`：
    - **Miss：** 分配新 `generation` 插入 `PendingInput`，仅在此分支预约 `pending_permit`（保持 `exclusive`/`read_parallel` 到达顺序）。
    - **Hit 且 `method_idx` 冲突：** 立即返 `721`，不取票、不收续片，沿拒收路径释放首帧载体。
    - **Hit 且 `method_idx` 一致：** 钉住当前 `generation`（绝不滑向同名新代），不取第二张 `pending_permit`，在接收循环外收完输入并按逻辑序增量计算 SHA-256。
- **唯一资源入口裁决点：** 原调用收完输入并完成 `route_guard.commit()` 与本地物化后，在 `HostOutcomeStore` 临界区内执行原 `CallRecord` 的 `enter_resource()` CAS——**获胜后才在同一临界区内将精确 `(route_uid, call_id, generation)` 推进为 `InFlight`**，彻底关闭 `PendingInput → InFlight` 与 `retire_connection_calls()` 之间的双裁决竞态。若 `enter_resource()` 失败或进入资源前中止，则撤销该精确代际，唤醒等待者：`recover_call` 收 `723`，重复 `submit` 收 `NotExecuted`（`pre_dispatch`）。
- **独立于原连接的执行排空与一次性等待交付：**
  - `enter_resource()` 获胜后，由阻塞执行任务持有 `OutcomeExecutionGuard`（同时持住该路由 `scheduler` 排空凭据与 Host 执行凭据，覆盖**单路由 `unregister` 的 `route.scheduler.wait_drained_async()`** 与**Host 停机的 `wait_routes_drained()`**，且在任务 panic unwind 时同样负责结算 `Failed` 并唤醒等待者），不再挂靠原物理连接的 `claim_terminal()`。
  - 输出编码、`BuildPlan` 长度前置校验、`WritableSlab` 封存（`Arc<SealedShmBlock>`）或 inline `BudgetedBuffer` 编码以及确定性 `max_payload_size` 校验全部完成后，再原子发布终态。
  - 等待者（`recover_call` 与已释放自身输入缓冲/SHM/复制许可的匹配重复 `submit`）在锁内登记一次性 `oneshot::Sender<OutcomeDelivery>`；结算时锁内取走等待者列表、锁外直接送达同一不可变交付引用——因此即便 `outcome_max_retained_bytes == 0` 或紧接着发生第 4097 条淘汰，已附着在 `InFlight` 上的等待者仍必然收到本次结果。
  - 在 `c2-wire` 连接 `CallRecord` 上提供独立的重放完成状态转换，不要求重放连接进入 `Executing`。
  - 附着等待沿用一元调用槽位模型：占用本连接与 Host 的 `N` 槽直至原调用发布终态或该物理连接结束（不引入一元 `CANCEL` 或跨 relay 解除附着协议）；客户端局部 `timeout` 仅约束本地等待，非阻塞轮询应使用 `query_call`。
- **资源回调边界隔离：** 资源 A 调用资源 B 并原样抛出 `721` / `722` / `723` 时，在回调边界（`c2-core` 错误归一化、Rust Host 与 Python 绑定）统一包装为 `ResourceExecutionError`（码 3，原码与信息入 `cause`），确保顶层 `721` / `722` / `723` 仅由当前 Host 结局表独占铸造，绝不让失败的外层调用伪装为已成功。

### 3. 内存治理、受压淘汰与多借出交付

- **Host 级共享配置（`HostOptions`，全注册共享）：**
  - `outcome_max_entries: usize`（默认 `4096`，合理上界校验；仅约束已结算终态总条数，`PendingInput`/`InFlight` 受 `server.N` 约束绝不淘汰）。
  - `outcome_max_retained_bytes: Option<usize>`（默认按生效的 `server.M` 推导为 `min(64 MiB, server.M / 4)`；允许显式设为 `0` 进入纯元数据模式；校验 `<= server.M`）。
  - 暂不暴露公开 TTL，不设 `delivered/undelivered` 双队列或 UTC 水位。
- **选择性无自锁受压淘汰：** 严禁在 `DomainBudget` / `MemPool` 持锁栈内注册同步回调。区分池分配失败原因（将可缓解容量不足与零长/超限/不可表示分开），仅在可缓解容量不足且退出池/预算锁后，由 `Server` 调用 `HostOutcomeStore::shed_for_pressure(needed_bytes)`（锁外 `drop`），对整次分配动作最多额外重试 1 次；接入服务端请求、重组、本地副本、`ResponsePool`/Python sink/智能回复及同域流与主题分配路径。
- **计费与多借出隔离：** Inline `BudgetedBuffer` 与保留的 `C2Error` 可变详情按实际缓冲容量计入 `server.M` 与 `outcome_max_retained_bytes`；SHM 封存块按 buddy `actual_size` 计入 `outcome_max_retained_bytes`（物理段在 `server.M` 只计一次；仅总字节 `<= outcome_max_retained_bytes` 的结果挂入 `retained_fifo`）。`c2-server` writer 为共享 `Arc<SealedShmBlock>` 增加 `CALL_END` 独立每连接 `loan_id` 发送路径（遵循 [ADR 0001](0001-single-owner-release.md) 结算不变量），非 SHM 接收方直接分段读取避免逐连接整包复制；单条接收连接的写失败或断开只影响该次交付，绝不改写已发布的成功结局。

### 4. 线协议（IPC 18）、`c3 relay` HTTP 与错误码

- **IPC 18（干净切掉 IPC 17；保留 25B 握手前导、16B 帧头、SHM 4 与 mesh 7）：**
  - `CallMode` 枚举：`Unary = 0`（布局不变，0 字节额外开销）、`ServerStream = 1`、`Subscribe = 2`、**`KeyedUnary = 3`**（`u8` 长度 `1..=128` + 合法 `call_id`，仅匹配 `Unary`）、**`Recover = 4`**（`RouteCallIdentity` + `method_idx` + 非空 `call_id`，无请求体、拒收续片，仅匹配 `Unary`，按一元解码响应）。同步更新 `rpc.rs` 偏移/长度、出错路径 SHM 载体精确单次释放与 `c2-ipc/src/forward.rs` 分片宽度。
  - 新增独立非阻塞控制记录对 **`RpcRecordKind::QueryCall = 11`**（client → server）与 **`RpcRecordKind::QueryCallResult = 12`**（server → client）：复用每连接非零 `request_id`，无 SHM、无分片、有界小响应（`< 1 KiB`）；结果二选一为极简 `CallOutcome`（`status`, `method_name`, `result_available`, `result_unavailable_reason`, `error_code`, `error_details_available`，不携带 1 MiB 级 `C2Error` 正文）或结构化 `Error(C2Error)`（仅活注册 + 契约校验通过后的 Miss 才返 `unknown`）。
  - 客户端、Host 与 relay 三层均绕过业务槽 `N` 执行 `query_call`（各以有界控制并发限额 `Q_ctrl = clamp(N, 1, 64)` 约束）；Host 控制队列满时返回 `717` 而不停用物理连接，writer 在控制回包与业务 `CALL_END` 间公平调度，确保 `N_client = N_server = N_relay = 1` 且长写占满时仍可无阻塞查证且不饿死业务回包。
- **`c3 relay` HTTP（固定 3 段路径 + 查询参数 + 单值 `x-c2-call-id` 头）：**
  - 为彻底免疫契约允许的 `.`、`..`、`/`、`_resolve`、`_probe` 与非 ASCII UTF-8 路由名在 URL 归一时撞上旧版 2 段业务路径或现有控制通配路由，所有新端点使用无动态路径段的固定前缀：
    - `POST /_c2/calls/keyed?route=<pct>&method=<pct>`（`data_router`，禁用默认体上限）
    - `POST /_c2/calls/recover?route=<pct>&method=<pct>`（`data_router`，显式校验空请求体）
    - `GET /_c2/calls/outcome?route=<pct>`（`control_router`，独立控制并发限额，返回 `Cache-Control: no-store` 有界 JSON）
  - 三者均要求唯一合法的 `x-c2-call-id` 头与标准 `x-c2-expected-*` 契约头；旧 relay 必返 404（且客户端绝不把旧 relay 的裸 404 视作 `ResourceNotFound` 或 `pre_dispatch` 权威证明）。跨机调用沿用直连解析出的源 relay A，不新增跨 relay 一元二次转发。
- **规范错误码（`c2-error`）：**
  - **`721 CallIdConflict`**：IPC `NotExecuted` / 客户端 `pre_dispatch` / HTTP **409**（`dispatch_phase=pre_dispatch`）。同 `call_id` 方法或输入摘要冲突；不触发路由重获取，不置 `TERMINAL_REFUSAL`。
  - **`722 CallResultUnavailable`**：IPC **`Failed`** / 客户端 **`transport_phase = None`** / HTTP **500**（`C2E1`）。本 Host 确认原调用已执行成功，但结果未保留（`not_retained`）或已淘汰（`evicted`）；细节含 `outcome=succeeded` 与 `reason`。
  - **`723 CallOutcomeUnknown`**：IPC `NotExecuted` / 客户端 `pre_dispatch` / HTTP **409**（`dispatch_phase=pre_dispatch`）。`recover_call` 未命中当前注册记录（绝不代执行；仅证明本次恢复未入资源，不证明原调用未执行）；不触发路由重获取，不置 `TERMINAL_REFUSAL`。

### 5. 跨语言 SDK、同进程边界与 ABI 10

- **Python (`c_two`)：** 提供顶层 `cc.with_call_id(proxy.step, call_id)(*args)`、`cc.recover_call(proxy.step, call_id)()` 与 **`cc.query_call(proxy, call_id) -> CallOutcome`**（不往 CRM 代理上挂方法名，避免与用户契约方法冲突）。`cc.hold`、`cc.with_call_id`、`cc.recover_call` 共用统一修饰表示，保留既有 `cc.hold` 在流/主题上的独立用法并拒绝在流/主题上附加 `call_id` 或混用 submit/recover。带 `call_id` 的同进程调用统一经本 `Runtime` 的本机 IPC 编码路径（无注册名/无 session 的手工 `thread_local` 代理派发前拒绝 keyed 操作）。`CCBaseError` 对带 `call_id` 且落入 `dispatch_uncertain` 的错误暴露只读 `call_id`。
- **Rust (`c_two`)：** `CallOptions` 保持 `Copy` 仅含 `timeout`；新增 `UnaryCallOptions { timeout, call_id }`，提供 `Client::call_unary_with_options`、`recover_unary_call`、`query_call` 及对应 `c2-codegen` 生成入口。
- **Node / TypeScript（C/FFI ABI 9 → **ABI 10**）：** 同步更新 `c2-mem-ffi` C 头、Rust 导出、N-API loader、TS 类型与生成的 `typescript_transport.ts`（含 HTTP 500 `C2E1` 投影、查询 200 有界 JSON 解码、业务 POST `redirect: "error"` 及 C ABI 边界拒绝 stream 携带 `call_id`）。

## 考虑过而没有采用

- **预申请票据（`PrepareCall`）加 1 RTT 栅栏：** 能在单次注册内从数学上封禁迟到旧包，但给每次带键科学计算写调用强加 1 次额外往返；在已有显式提交 + 只读查证 + 遇 Miss 绝不代执行的 `recover_call` 下，收益与复杂度失衡。
- **`@cc.write` 默认自动生成 `call_id`：** 未标注方法默认即为写访问，会导致高频普通写全面承担哈希、内存与淘汰开销，同时掩盖调用方对唯一键生命周期的显式责任。
- **把 `call_id` 放进可复用 `CallOptions` 或把 `query_call` 挂在 CRM 代理上：** 前者极易因复用同一 `CallOptions` 触发 `721` 或串重放，后者污染用户 CRM 契约方法命名空间。
- **双淘汰队列（`delivered` / `undelivered`）与 `RELEASE` 交付确认：** 共享 IPC 连接在客户端超时后仍会发送 `RELEASE`，不能证明上层应用真正收到了结果；单一完成顺序 FIFO 更诚实且无额外状态。
- **在 `DomainBudget` / `MemPool` 持锁栈内注册同步淘汰回调：** `WritableSlab::try_alloc` 持池写锁调用域预算，回调里析构 `SealedShmBlock` 会重入自锁；改为锁外选择性 `shed_for_pressure` + 有界单次重试。
- **沿用 `POST /{route}/{method}` 可选请求头或 `/{route}/_recover/{method}` 路径：** 前者会让不支持的旧 relay 静默按普通未带键调用执行，后者在路由名为 `.`/`..` 时经标准 URL 归一后会撞上旧版 2 段路由。
- **在 `CallOutcome` 里内嵌完整 `C2Error` 正文：** 合法 `CALL_END` 错误可达近 1 MiB，放进查询控制帧会触发帧限并拖慢轻量查证；错误详情留给 `recover_call`。

依据：[一手来源调研](../research/2026-11-23-post-dispatch-outcome-and-dedup-survey.md)、[ADR 0001](0001-single-owner-release.md)、[ADR 0003](0003-server-streams.md)、[ADR 0004](0004-route-name-identity.md)、[ADR 0005](0005-publish-subscribe.md)。
