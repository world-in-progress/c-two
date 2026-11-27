# `c3 relay` 入口分层、原生 TLS/HTTP2 与 `iroh` 跨 NAT 节点组网

状态：已采用（2026-11-27），拟定 0.13.0 / `c3` 0.9.0 实施。

## 决定

C-Two 升级 `c3 relay` 边缘传输并引入跨 NAT 加密组网，在 0.x 阶段做干净切割：将 `c3 relay` 入口按信任面彻底拆分为本机回环控制面（`--bind`）、公网/反代业务边缘面（`--edge-bind`）与跨机认证集群面（`iroh 1.5`）；边缘原生支持 TLS 1.3（含零域名持久化自签指纹固定）与 HTTP/2 / HTTP/1.1 共存，并将慢订阅者驱逐重构为协议无关的带外流级重置；跨机集群由 Mesh 8 升至 **Mesh 9**，通过纯 Rust `iroh 1.5` 与零知识盲跳板 `c3 bridge` 实现 P2P QUIC 优先直连与 HTTPS/WebSocket 密文回落，同时保持 `c2-core` 及各语言 SDK 零 `iroh`/QUIC 依赖。

### 1. 信任边界、依赖隔离与范围界定

- **业务语义始终只有一套：** 全部业务与控制面继续沿用 `/_c2/*` 与 `/_peer/*`（Mesh 9），不引入面向用户或 CRM 代码的传输协议头，也不建立第二套 RPC 状态机。
- **依赖严格分层隔离：**
  - `c2-core`、Python wheel、Rust SDK 与 Node C/FFI ABI 10 绝不引入 `iroh`、`iroh-relay` 或 QUIC 依赖，仅保留本机 IPC/SHM 与标准 HTTP/HTTPS（支持 HTTP/2 与 CA/指纹校验）；
  - `iroh 1.5` 与 `iroh-relay 1.5` 严格限定在 `c2-http` 的 `relay` feature 与 `c3` 二进制中。
- **真实信任边界：**
  - **不可信输入与外围网络：** 跨公网链路默认存在被动窃听与主动中间人；公网/反代边缘调用方不得触碰注册、管理或集群控制端点；`x-c2-relay-origin-only` 仅作为单跳防环头，绝不充当认证凭据。
  - **零知识盲跳板：** `c3 bridge` 仅负责 NAT 地址发现、节点会合与禁 UDP 时的密文转发，不运行任何 CRM 路由或资源计算，不持有端到端解密密钥，也**绝不与业务集群共享 `cluster_secret`**。
  - **严禁外连第三方公共服务与无校验加密：** 所有 `iroh` 端点基于 `presets::Minimal`（禁用公共中继与公共发现）；零域名场景采用持久化自签证书配精确叶证书 DER SHA-256 指纹固定或自定义根 CA，不提供跳过校验的生产开关。
- **明确不在范围：** 浏览器 WASM 运行时与 Web TS SDK 实现（仅守住边缘 HTTPS/H2 与未来 `wss://` 宿主通道抽象边界）、客户端流/双向流新 RPC 契约、上层 PKI/CA 签发与用户级 ACL，以及 `c3` 内置 ACME 自动签发。

### 2. 三面入口隔离与边缘原生 TLS

将 `c3 relay` 的接入面收口为两个 HTTP(S) 监听面与一个 `iroh` 集群面，共享同一 `RelayState` 与业务处理器：

1. **本机控制与锚点面（`--bind`，默认由 `0.0.0.0:8080` 收紧为 `127.0.0.1:8080`）：**
   - 强制仅允许 loopback 地址（拒绝非 loopback `--bind` 并引导使用 `--edge-bind`），明文 HTTP/1.1 + h2c。
   - 挂载完整本机面：`/_register`、`/_unregister`、`/_admin/*`、`/_peers`、回环多 relay 测试用 `/_peer/*` 以及全部业务与发现端点。
   - 仅当 `ListenerSurface::LocalControl`、`remote_addr.is_loopback()` 且 `local_namespace` 匹配时才在 `/_resolve` 中暴露 `ipc_address`、`server_id` 与 `server_instance_id`。
2. **公网/反代业务边缘面（`--edge-bind <ADDR>`，可选，默认关闭）：**
   - **仅挂载业务与发现面：** `/health`、`/_routes`、`/_resolve{,/*}`、`/_probe{,/*}`、`/_c2/calls/{call,keyed,recover,outcome}`、`/_calls/{token}`、`/_c2/topics/{subscribe,confirm}`；绝不挂载 `/_register`、`/_unregister`、`/_admin/*`、`/_peers`、`/_peer/*`。
   - **无条件禁止本地 IPC 暴露：** 即使 `--edge-bind 127.0.0.1:8443` 位于同机 Nginx/Caddy/Cloudflare Tunnel 之后，也不暴露本机 IPC 路径或实例身份。
   - **TLS 策略：**
     - 非 loopback `--edge-bind` 默认启用原生 TLS 1.3/1.2（ALPN `["h2", "http/1.1"]`）：优先 `--tls-cert` + `--tls-key`，未指定时在状态目录自动生成并持久化自签证书（Unix 目录 `0700` / 私钥 `0600`；Windows 使用 `c2-local-security` TokenUser-only ACL），SAN 从 `--advertise-url`、`--tls-san`、非通配绑定 IP 与回环地址推导，启动输出本机可达地址、外部可拨地址（绝不打印 `0.0.0.0`）与 `sha256:<hex>` 叶证书 DER 指纹；仅显式 `--insecure-plaintext` 时允许非 loopback 明文。
     - loopback `--edge-bind` 默认明文 HTTP/1.1 + h2c（专供同机反代），显式提供 `--tls-cert`/`--tls-key` 或 `--tls-auto` 时启用 TLS。
3. **跨机集群 Mesh 面（`iroh 1.5`）：**
   - 仅挂载 `/_peer/*`、origin-only `/_c2/*`、`/_probe`、`/_calls/{token}` 与 `/health`，不挂载 `/_register` 与 `/_admin/*`，且无条件不暴露本地 IPC。
   - 请求扩展携带经密码学验证的 `AuthenticatedIrohPeer { endpoint_id }`，所有 `/_peer/*` 强校验 `sender_relay_id ↔ endpoint_id` 一致性。

### 3. 原生 HTTP/2、流级带外慢读驱逐与统一客户端信任

- **服务端驱动与带外重置：**
  - 彻底退役 H1 耦合类型 `RelayListener`、`RelayStream`、`ConnectionControl`、`OptionalTopicConnection`，统一替换为协议无关的 `ResponseStreamControl`（`is_blocked()`、`changed`、`abort()`、`aborted()`），保留 `TopicHub` 现有 `frame_started`、`inflight_chunks` 与预算争用驱逐判定。
  - **HTTP/2 分支直接使用 `h2::server::handshake`：** 直接持有每条流的 `h2::SendStream`，写入循环并发监测 `poll_capacity()`（窗口为 0 时置 `write_blocked`）与带外 `control.aborted()`；驱逐触发时立即调用 `send_stream.send_reset(h2::Reason::CANCEL)` 并释放切片预算，既不等待 `Body` 轮询，也不连坐同连接兄弟流。
  - **HTTP/1.1 分支（`hyper::server::conn::http1`）：** 经轻量 IO 包装更新 `write_blocked`，带外 `abort()` 立即终止该单条 H1 连接且不写终止块 `0\r\n\r\n`。
  - **`iroh` QUIC 分支：** 直接对目标 `SendStream` 带外调用 `reset(...)`。
- **HTTP/2 窗口、保活与早拒绝：**
  - 禁用无上界 `adaptive_window(true)`，流窗口与连接窗口按角色内存预算 `M` 显式设界（且连接窗口显著大于单流窗口，防止单条慢流先耗尽整连接窗口）；文档明示 `M` 覆盖 C-Two 计费载荷与转发内存，内核 socket 缓冲与 TLS/h2 帧开销在 `M` 之外。
  - 不启用会主动斩断长耗时科学计算一元调用的短 H2 keep-alive 超时，延续 C-Two“连接终态来自 OS 事实、调用仅受自身 deadline 约束”的原则。
  - `finish_unread_body_rejection` 按 `TransportKind` 分流：`Http1` 保留有界 2s drain；`Http2` 先送完规范错误响应与 `END_STREAM`，若请求半端未结束再发 `RST_STREAM(NO_ERROR)`（RFC 9113 §8.1）；`IrohHttp1` 写完响应并 `finish()` 后仅对接收半端发 `STOP_SENDING`（`recv.stop(0)`）。
  - `topic_handler` 与 `topic_confirm_handler` 在 HTTP/2 下实际检测首帧，拒绝无 `Content-Length` / `Transfer-Encoding` 却携带非空 DATA 的请求（`400 ProtocolViolation / pre_dispatch`）。
- **统一客户端 TLS 信任、分池与防降级：**
  - `c2-config` 新增互斥配置 `C2_RELAY_CA_CERT`（自定义 PEM CA 根）与 `C2_RELAY_CERT_SHA256`（绑定目标 origin 的叶证书 DER SHA-256，自定义 `ServerCertVerifier` 仍完整执行 TLS 握手签名验签）；`c2-http`、`c2-core`、Python SDK、Node 原生连接器与 `c3 registry` CLI 共享同一构建器，pool 键纳入信任策略身份。
  - `HttpClient` 分离独立**控制池**（`/health`、`/_resolve`、`/_probe`、`QueryOutcome`、`DELETE /_calls/*`、`TopicConfirm`）与**数据池**（`/calls/{call,keyed,recover}`、`TopicSubscribe`），防止大载荷阻塞控制操作。
  - 当锚点为 `https://` 时，Rust 与 TypeScript 客户端均拒绝从 `/_resolve` 静默降级到非 loopback 的 `http://` 目标。
- **面向未来 Web TS 的边界：** 浏览器 `fetch()` 不提供应用级证书固定 API（需 CA 信任证书、已安装根 CA 或同源反代）；未来 CORS 需显式处理 `OPTIONS`、具体头白名单（Fetch 不支持 `x-c2-*` 通配）与 `Access-Control-Expose-Headers`；为 `Locality::Local` 预留未来 `wss://` 宿主会话抽象空间。

### 4. `c3 bridge` 盲跳板与 `iroh 1.5` 双通道 Mesh 9

- **`c3 bridge`（`iroh-relay 1.5.0` `server` feature）：**
  - 默认 `--bind 0.0.0.0:8443`、可选 `--quic-bind 0.0.0.0:7842`（可 `--no-quic` 关闭）、`--tls-cert/--tls-key` 或持久化自动自签证书；
  - 默认强制私有准入，复用 `iroh-relay` 原生 `Authorization: Bearer`（`ClientBuilder::auth_token` / `ClientRequest::auth_token()`）与可选 `--allow-peer <ENDPOINT_ID>` 白名单，在 `AccessControl::on_connect`（101 与身份握手后、入转发表前）拒绝未授权节点。
- **`c3 relay` 跨 NAT 组网（Mesh 8 → Mesh 9，`PROTOCOL_VERSION = 9`）：**
  - 持久化 Ed25519 `SecretKey`（所有者独享权限），构造 `Endpoint::builder(presets::Minimal)` + `RelayMode::Custom(private_bridge_map)`（携带显式 QAD 端口或禁用标记）。未显式传 `--relay-id` 时由持久化 `EndpointId` 确定性派生稳定 `relay_id`，避免重启残留旧持有者触发虚假 `719`。
  - **集群成员认证与跳板凭据彻底分离：** 支持 `--allow-mesh-peer <ENDPOINT_ID>`（在 `EndpointHooks::after_handshake` 直接校验）与 `--cluster-secret <SECRET>`（在独立预认证 ALPN `b"c2/mesh-auth/9"` 的端到端 QUIC TLS 1.3 内完成绑定双方 `EndpointId` 与随机 nonce 的 HMAC 互证，通过后再放行业务 ALPN）。
  - **双 ALPN 隔离控制与数据通道：** 同端点对每个对端维护 `b"c2/mesh-ctrl/9"`（`/_peer/*`、`/_probe`、`DELETE`、`QueryOutcome`、`TopicConfirm`、`/health`）与 `b"c2/mesh-data/9"`（`/calls/{call,keyed,recover}`、`TopicSubscribe`），各自拥有独立 QUIC 连接级流控与并发流配额，消除大流饿死控制面。
  - **完整 `PeerAddress` 与恢复探测收紧：** 引入边界校验的 `PeerAddress`（`http_url: Option<String>` + `iroh: Option<IrohPeerAddr>`，至少其一），贯通 `PeerInfo`、`PeerSnapshot`、`RouteEntry`、`RelayJoin`、`RouteAnnounce`、`FullSyncRoute`、`DigestDiffEntry::Active`、`route_entry_digest_hash`（仅哈希各节点一致认同的公告字段）、心跳、离开、force-leave 与 CLI 展示；死亡探测须核对目标身份与显式 Mesh 9 接受的 join 响应（不把泛化 `/health` 或 `200 ignored` 当恢复，且恢复不合并 bootstrap 快照）。

### 5. 锚点→源全路径透明代理与流式预算

- **`/_resolve` 显式代理目标与 `/_probe` 转发：**
  - 对纯 `iroh` peer 路由或经 `--edge-bind` 需由本次锚点代理的路由，resolve 响应显式标记 `proxy_via_anchor: true`，SDK 复用原配置锚点 base URL（保留协议、端口、TLS 信任与反代路径前缀，绝不从 `Host`/`X-Forwarded-Host` 猜测）。
  - `GET /_probe?route=...` 与 `GET /_probe/{*name}` 命中 `Locality::Peer` 时，若含 `x-c2-relay-origin-only: 1` 返回 `502 / 702 ResourceUnavailable (pre_dispatch)`，否则经控制通道向选定源做一次 origin-only 完整契约 `/_probe`，下游断开时立即取消预派发拨号/探测。
- **全业务、取消与主题语义对齐：**
  - 固定单次派发目标并原样保留源状态码、规范 `C2E1` 错误体与阶段（未知名字 `404/701`、`origin-only` 命中远端 `502/702 (pre_dispatch)`、`409/719`、`409/721`、`403/724`、`409/723`、`500/722`、`200 unknown` + `Cache-Control: no-store`）；`Recover` 校验空体、`KeyedUnary` 保留输入/键/元数据、`POST /_c2/calls/call` 保留 `Accept`；已开始向源发送但未获可信终态一律报 `dispatch_uncertain` 且不换源重发。
  - 流取消在交付 `200` + `x-c2-call-token` 前注册生命周期句柄绑定 `(local_token -> pinned_peer_id, remote_token_slot, merged_cancel_cause)`，覆盖远端 token 尚未返回时的断开/关闭竞态；`DELETE /_calls/{token}` 显式透传 `?cause=consumer|lifecycle`（修复现有丢 cause 问题）并发往捕获的原 `peer_id`，释放锁后等网络，`204` 表示已受理，原响应交付真实 `718` 原因与完整条目数。
  - `QueryOutcome` / `TopicConfirm` 继续使用独立 `Q_ctrl`，`DELETE` 与 `/_probe` 不占业务槽 `N`；两级 `TopicHub` 通过统一 `PeerHttpClient` 复用共享单上行汇聚与逐人准入确认。
  - 跨 relay 转发采用有界流式上传/下载窗口（不整包预留或缓冲大载荷），保留可信 `Content-Length`、`max_payload_size` 与未知长度 64 KiB / 411 边界，字节所有者释放时才归还 `M`。

## 影响与替代方案

- **已拒绝方案：**
  - 保留单一 `--bind 0.0.0.0:8080` 并复用同一 `Router` 挂 TLS：会把 `/_peer/*`、`/_register`、`/_admin/*` 一并暴露给公网，且反代回环流量会误获本机 IPC 与管理权限。
  - 仅靠 `Body::Err` 驱逐慢订阅者：`hyper 1.8.1` 在 H1 写缓冲满或 H2 流窗口为 0 时不再轮询 `Body`，无法驱逐已卡死的订阅流。
  - 引入 C/C++ NAT/QUIC FFI 库（`libdatachannel`/`libjuice`/`MsQuic` 等）：跨四平台工具链与许可成本高，且缺少一体化 QUIC 打洞与 HTTPS/WS 盲中继回落。
  - 让 `c3 bridge` 共用 `cluster_secret`：会破坏跳板机零知识边界，使跳板机具备业务入群权限。
- **0.x 破坏性切割：**
  - 退役 `RelayListener`、`RelayStream`、`ConnectionControl`、`OptionalTopicConnection`；
  - `c3 relay` 默认 `--bind` 改为仅限回环的 `127.0.0.1:8080`，外部业务流量迁至 `--edge-bind`；
  - 跨机 Mesh 升级至 Mesh 9（`PROTOCOL_VERSION = 9`），不兼容 Mesh 8，且不保留无认证的跨机公网 HTTP `/_peer/*`。
