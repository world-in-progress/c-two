# 调用元数据与准入钩子：一手来源调研

日期：2026-11-23  
用途：为 [ADR 0007](../decisions/0007-call-metadata-and-admission-hooks.md) 提供业界一手机制对照、边界取舍与协议约束依据。

## 1. 背景与问题定义

C-Two 面向跨进程、跨机器的有状态科学计算资源 RPC（L1 传输与运行时层）。按[终局架构](../design/endgame-architecture.md)的职责切分，鉴权（JWT/OIDC/mTLS）、权限策略（RBAC/ACL）、多租户配额、审批工单与审计日志归属上层编排与领域服务（L2/L3），而不内置进 C-Two 内核；但上层要落地这些横切能力，L1 必须提供两项正交原语：

1. **调用元数据（call metadata）：** 随每次调用（含普通一元、带键一元、恢复、结局查询、服务端流、主题订阅及同进程 `thread_local`）一致传递的有界轻量键值对（如认证凭据、审批令牌、调用方声明与 `traceparent`），不污染领域方法的业务签名。
2. **准入钩子（admission hook）：** 在请求进入资源方法、结局存储或主题订阅之前，由 Host 统一裁决放行或拒绝的同步/阻塞拦截点，并能把钩子解析出的进程内可信对象（如已验签的主体信息）经只读调用上下文（call context）透传给当前调用，同时保持无钩子热路径零开销。

与无状态 RPC 框架不同，C-Two 具备五项结构性前提，直接决定了机制形态：
- **单机微秒级热路径 + 跨机 HTTP/relay 双路径 + 同进程 `thread_local`：** 三条传输必须共用同一数据模型、校验边界与客户端选项抽象。
- **共享内存单一属主释放（[ADR 0001](../decisions/0001-single-owner-release.md)）与非幂等排空语义：** 拒绝必须严格发生在进入资源之前（`NotExecuted` / `pre_dispatch`），同时精确释放已收 SHM 租约且绝不破坏连接顺序与排空不变量。
- **路由名即状态身份（[ADR 0004](../decisions/0004-route-name-identity.md)）与中继无策略权威：** `c3 relay` 只负责路由发现与转发，不持有业务策略；所有准入裁决必须来自持有该路由的 Host。
- **relay 主题汇聚（[ADR 0005](../decisions/0005-publish-subscribe.md)）的 1 对 N 共享上游：** 上游 IPC/HTTP 订阅在 relay 处只建一条，后续共享该汇聚的每个下游订阅者仍需单独过 Host 准入，且单个订阅者被拒不得拖垮整条共享上游。
- **派发后结局查询与去重（[ADR 0006](../decisions/0006-post-dispatch-outcome-and-dedup.md)）：** `query_call` 走独立非阻塞控制通道；`KeyedUnary` / `Recover` / `QueryOutcome` 需要一致的准入保护，且不能因为一次凭证刷新或链路追踪号变化而破坏幂等指纹。

## 2. 一手来源对照

### 2.1 gRPC HTTP/2 Metadata 与 Server Interceptors

一手来源：gRPC Core 规范 *gRPC over HTTP2*（`PROTOCOL-HTTP2.md`）、gRPC 官方文档 *Core concepts — Metadata*、gRPC Python `grpc.ServerInterceptor` 与 gRPC-Go `UnaryServerInterceptor` / `StreamServerInterceptor`。

- **核心机制：**
  1. **键与值的两类编码：** gRPC 基于 HTTP/2 HEADERS，键为小写 ASCII `[a-z0-9_.-]`；普通值为可打印 ASCII（`0x20..=0x7E`），携带二进制数据时通过 `-bin` 后缀自动做 Base64 编解码；单个键允许重复出现（multi-value）。
  2. **拦截器链与状态码：** 服务端将拦截器注册在 `Server` 级，按一元与流两类入口截获调用，返回 `PERMISSION_DENIED`（HTTP 403）或 `UNAUTHENTICATED`（HTTP 401）即可在进入方法实现前中止调用。
  3. **跨层传播与嵌套调用风险：** 拦截器把解析结果写进请求 `context`（Go `context.Context`、Python `contextvars`）；若服务 A 调用服务 B 时直接把 B 的 `PERMISSION_DENIED` 原样抛出，上游客户端无法分辨是“A 拒绝了调用方”还是“A 已经执行了一半、调用下游 B 时被 B 拒绝”。
- **对 C-Two 的启示：**
  - **收窄到单值纯 ASCII 映射：** 科学计算控制元数据（token、principal、traceparent、approval_id）几乎全部是短 ASCII 字符串。去掉 `-bin` 后缀与 multi-value 列表后，IPC 紧凑二进制编码、HTTP 单头映射、JSON ABI 与三端 SDK 字典/映射语义完全一一对应，彻底避免多值拼接歧义和隐式 Base64 开销。
  - **单一 Host 级错误码 + 回调边界隔离：** 鉴于 L1 不区分 AuthN 与 AuthZ 业务语义，统一使用一个规范码 **`724 AdmissionDenied`**（`NotExecuted` / `pre_dispatch` / HTTP `403`），具体原因留给 `message` 与结构化 `details`；同时在全部回调退出边界把下游逃逸出的 `724` 包装成 `3 ResourceFunctionExecuting`，杜绝伪造顶层 `pre_dispatch`。

### 2.2 Rust Tonic (`tonic::metadata` / `tower::Service`) 与 HTTP `Extensions`

一手来源：Tonic 文档 *`tonic::metadata::MetadataMap`*、*`tonic::service::Interceptor`* 与 `http::Extensions` / `tower::Layer`。

- **核心机制：**
  1. **请求头与进程内扩展分离：** Tonic 明确区分跨网络传输的 `MetadataMap`（纯字节/ASCII，只读于业务方）和仅活在服务端单次请求内存中的类型化 `Extensions`（`TypeMap` / `Arc<dyn Any + Send + Sync>`）。拦截器验签后把 `AuthenticatedPrincipal` 塞进 `Extensions`，业务处理函数直接取用，无需重复验签，也不会把内部对象序列化到线上。
  2. **同步拦截器与异步执行解耦：** Tonic 的 `Interceptor::call` 是同步函数，适合快速头校验；但真实业务常需访问外部缓存、本地数据库或执行带 I/O 的令牌校验，此时必须退回异步 `tower::Service` 并把阻塞工作隔离开网络读循环。
  3. **HTTP/1 与 HTTP/2 头部规范化差异：** `http` / Hyper 在路由进入前会自动把 HTTP 字段名转小写并按 RFC 9110/9112 裁剪首尾可选空白（OWS），且在进入业务路由前即受单请求头数与头部缓冲区硬上限约束（超限返回 `431`）。
- **对 C-Two 的启示：**
  - **采用“只读 `metadata` + 进程内 `extension`”双轨：** `AdmissionDecision::Allow { extension }` 允许钩子附带任意只存在于本次 Host 执行期内存中的 `Arc<dyn Any + Send + Sync>`（Python 为任意对象），经只读 `current_call_context()` 向资源方法按需暴露；不改领域方法签名，也不引入额外序列化协议。
  - **诚实写明跨传输规范化边界：** 在 SDK、IPC 与 C ABI 上严格拒绝大写键、重复键与首尾空格；在 HTTP 入口前由 Hyper 做的 OWS 裁剪与 `431` 限制如实记录，不假装所有传输在解析前逐字节等价。
  - **绝不在 `recv_loop` 同步跑用户钩子：** 钩子在有界 Host 并发配额 `A = clamp(P, 4, 64)` 下放到阻塞工作线程执行，确保网络读循环永不被慢钩子卡死。

### 2.3 Connect RPC / Twirp 与固定数据面路径设计

一手来源：Connect Protocol 规范（`connectrpc.com/docs/protocol/`）与 RFC 9110（*HTTP Semantics*）。

- **核心机制：**
  1. **显式内容类型版本化与错误信封：** Connect 通过固定媒体类型版本与结构化 JSON 错误体区分协议响应与反向代理/网关生成的裸 404/502，避免把中间层错配路由误判为权威的 `NotFound`。
  2. **HTTP 路径命名空间隔离：** 早期 RPC 框架把 `/{service}/{method}` 直接映射到根路径，一旦路由名含 `/`、`.`、`..` 或与控制端点前缀撞名，URL 归一化或路由优先级就会产生二义性。
  3. **预提交拒绝 vs 流式已提交拒绝：** HTTP 服务端流在发送 `200 OK` 响应头之前拒绝可直接返 `403`；若请求体边传边审、拒绝发生在 `200 OK` 已提交之后，只能通过流式终端帧表达 `NotExecuted` 错误，客户端须对两种载体投影出完全一致的错误语义。
- **对 C-Two 的启示：**
  - **统一收口到 `/_c2/*` 固定数据面路径：** ADR 0006 已经为 `keyed` / `recover` / `outcome` 引入 `/_c2/calls/*`；借 0.x 协议切换窗口，将普通调用（`/_c2/calls/call`）与主题订阅/确认（`/_c2/topics/subscribe`、`/_c2/topics/confirm`）全部收口到 `/_c2/*`，彻底退休 `POST /{route}/{method}` 与 `POST /{route}/_topics/*`，并把旧版仅对 `outcome` 生效的裸 404 保护推广为全部六条 `/_c2/*` 路由的统一错误分类器。
  - **流式准入双载体一致投影：** 经 relay 的服务端流同时支持 200 前 `403 + 724` 与 200 后 0 条目 `NotExecuted(724)` 两种拒绝载体，客户端统一归并为 `724 AdmissionDenied`（`pre_dispatch`）。

## 3. 机制对照与 C-Two 选型结论

| 维度 | gRPC | Tonic / Tower | Connect RPC | C-Two ADR 0007 |
| --- | --- | --- | --- | --- |
| 元数据模型 | multi-value + `-bin` 二进制 | `MetadataMap`（多值 + 二进制） | HTTP 头透传 | **单值有序纯 ASCII 映射**（≤64 项、键 ≤64B `[a-z0-9_.-]`、值 ≤4KiB 可打印 ASCII 无首尾空格、总计 ≤16KiB） |
| 客户端传递面 | 每调用 `metadata=` / `Context` | `Request::metadata_mut` | `CallOptions` / Headers | **全三端统一 `with_call_options`** 视图承载 `timeout` + `metadata`（Rust 退役 `UnaryCallOptions`） |
| 服务端钩子粒度 | Server 级 Unary + Stream 拦截器 | Tower Layer / Interceptor | Handler Interceptor | **Host 级单一 `admission_hook`**，随 `server` 角色冻结；无钩子时热路径零开销 |
| 钩子 → 资源透传 | `Context` / `contextvars` | `http::Extensions` | `Context` | 只读 **`current_call_context()`**（Rust `thread_local!` RAII / Python `ContextVar`）+ 仅活在执行期的进程内 `extension` |
| 拒绝错误码与隔离 | `PERMISSION_DENIED` / `UNAUTHENTICATED` | 同 gRPC | `permission_denied` / `unauthenticated` | 单一规范码 **`724 AdmissionDenied`**（`NotExecuted`/`pre_dispatch`/HTTP 403）+ **全 4 处回调边界包装隔离** |
| 共享上游扇出准入 | 无内建共享订阅汇聚 | 无内建共享订阅汇聚 | 无内建共享订阅汇聚 | **`TopicHub` 按需准入**：创建者首包经 `READY(start_seq, head_seq)` 零额外 RTT，被拒则 `opening_abandoned` 触发竞争者 `RetryJoin`；后续加入者经 `AdmitTopic` / `/_c2/topics/confirm` 独立过 Host 钩子 |
| 幂等键与元数据 | 无内建去重表 | 无内建去重表 | 无内建去重表 | `KeyedUnary` 的 `SHA-256(input)` 排除元数据，支持凭证刷新与追踪号轮换；跨主体 `call_id` 归属由上层在钩子内校验 |

## 4. 关键工程边界总结

1. **为什么选择 Host 级单一钩子，而不是路由级或方法级装饰器？** 鉴权与准入属于横切安全边界：若挂在单个方法装饰器上，既容易漏保新增方法、`recover_call`、`query_call` 或主题订阅，又会把安全逻辑耦合进领域资源类，且无法覆盖尚未进入资源对象的 `QueryOutcome` 控制面路径。Host 级单一钩子一次覆盖五种操作（`Unary`、`ServerStream`、`Subscribe`、`Recover`、`QueryOutcome`），并让 `AdmissionContext` 携带完整契约身份、目标名与契约声明的 `Read`/`Write` 访问级别，上层可在钩子内按需做方法或路由级分发。
2. **为什么不保留混合部署兼容层？** 0.x 阶段遵循干净切割原则：将 IPC 扩展并入尚未发布的 IPC 18 与 C/FFI ABI 10，relay 同步升至 Mesh 8 与 `topic-stream;version=2` 并统一 `/_c2/*` 路径，同时退役 Rust `UnaryCallOptions` 与旧 2 段 HTTP 路由，避免维护两套解析器与静默降级通道。
3. **为什么 `TopicHub` 的后期加入确认必须绑定上游 `call_token`？** 在两跳 relay（B→A→H）场景中，中继 B 与中继 A 之间共享一条上游订阅；当 B 发起 `/_c2/topics/confirm` 确认第 2 个下游订阅者时，若不把 A 签发的 `x-c2-call-token` 回传给 A 并在 Host H 返回后复验该令牌与汇聚实例活性，就可能把“B 已断连或已重连到新汇聚”的过期确认错配到另一条无关订阅上。绑定令牌并仅在失效时隔离 B 的上游，既保证准确授权，又不波及 A 的本地订阅者。
