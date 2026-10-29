# IPC RPC 握手与固定版本前导规范

状态：本文描述 C-Two 0.10.0 起使用的 IPC 握手版本 14 的规范边界，作为以后协议演进的依据；0.9.x 使用无固定前导的握手 13，两者不兼容。实现背景与批准边界见 [方案](plans/2026-10-25-ipc-version-mismatch.md)；版本变更见 [0.10.0 更新说明](releases/0.10.0.md)。本次实现的三系统门禁、旧版混用边界及延迟证据见[验收记录](reports/2026-10-26-ipc14-version-diagnostics-acceptance.md)。

## 适用范围

本规范只约束普通 RPC 连接的 `HANDSHAKE` 版本协商，即 Rust/Python 客户端与生成 TypeScript 客户端在 Unix UDS / Windows Named Pipe 上发起的业务握手。SIGNAL ping/shutdown 控制、OwnerControl（独立版本 1）、私有 owner bootstrap 通道都不使用本文的前导，也不因 RPC 版本变化而改变。“不同 IPC 版本可诊断”仅指采用本固定前导的 RPC 连接，不能泛称任意本地通道或无响应进程都可报告版本。

## 永久固定前导（25 字节）

版本选择之前，两端先交换一条固定 25 字节记录。它是版本无关的识别格式：即使未来业务帧或元数据布局再变，这个初始封套仍须按这些字节识别，不随业务帧编解码一起改。

| 字节 | 固定含义 |
| --- | --- |
| 0–3 | `u32 LE total_len = 21`，不含这四字节自身 |
| 4–11 | `u64 LE request_id = 0` |
| 12–15 | 请求 flags 为 `0x04`，应答为 `0x06`；不接受额外 flags |
| 16–19 | 固定 magic：ASCII `C2IP` |
| 20 | `1 = Hello`、`2 = Accepted`、`3 = VersionMismatch` |
| 21–24 | 发送方协议版本，`u32 LE`，范围 1…4294967295 |

读写纪律：两端先读固定 16 B 封套，核对 `total_len=21`、`RID=0` 和精确方向 flags，再读固定 9 B 内容，核对 magic、kind 和版本；不得沿用长度驱动的 payload 分配或 TS `readIpcFrame` 读取未知前导。版本值 0 按畸形记录拒绝。固定前导独立于 `max_frame_size`；元数据、业务和其他控制帧仍按原上限拒绝，只有长度与 flags 都满足固定格式的前导例外。版本号仅用于相等判断，不做降级协商；前导没有长度可变的诊断文本、支持版本列表、备用版本、保留字段或配置开关。

## 协商时序与 RPC 阶段围栏

客户端在既有 OS 连接和原生身份检查之后只发送 Hello 前导并等待固定应答，此时不发送 SHM 名称、路由请求或业务数据。服务端版本不等时写 VersionMismatch 并携带服务端版本；相等时写 Accepted。只有收到 Accepted 且版本相等，客户端才发送原能力/SHM 元数据，服务端按原完整校验生成身份/路由/能力 ACK；旧元数据中的一字节版本字段已删除，版本边界只由固定前导承担。

初始协商在同一个握手调用栈内独占读取 Hello 和元数据，两个阶段各自核对 RID、方向和精确 flags；不退回普通分派循环等元数据，也没有第二套持久化状态。最终元数据 ACK 写完后才发布 `handshake_done` 和 `ordinary_protocol`，开放业务与 heartbeat；握手完成前，除独立 SIGNAL 控制外的 CALL/普通 CTRL 帧以 `frame_before_handshake` 终止连接，已建立连接上的第二次 HANDSHAKE 以 `duplicate_handshake` 终止，ordinary 角色确立后的 owner 角色切换以 `owner_role_change` 终止。没有绕过版本协商的 RPC 通道。

## Malformed 与 VersionMismatch 的完整区分

畸形记录是协议错误，不是版本事实：封套长度/RID/flags 错误、magic 错误、未知 kind、版本为 0、截断、矛盾记录（Accepted 携带不同版本、VersionMismatch 携带本端版本）以及对端崩溃、EOF、超时，都保持既有的 decode/protocol/IO 错误分类，不伪造 `peer_protocol_version`。只有完整解码出的 VersionMismatch 且对端版本不等于本端时，才产生类型化的版本不符错误并携带两个 u32。低层实现不得从错误文本反向提取版本。

## 期限、终态与关闭

Rust/Python 客户端把两个握手阶段纳入调用方既有的同一个 `ConnectAttempt` 绝对期限和 handshake phase guard，不各自重新获得预算。生成 TypeScript 沿用既有连接接口；本项不增加握手 timer 或配置，不能承诺无响应自定义连接的总期限。版本不符是确定终态：不进入 stale-pool 重连、relay HTTP fallback 或后台重试，不撤销仍有效的其他共享连接；客户端读完固定回绝即形成结构化错误并在本地关闭，不等服务端 EOF。服务端前导读取、回复写入及元数据读取保留 shutdown/heartbeat 取消路径；初始协商期间禁止 heartbeat 插入业务 PING。

服务端拒绝发送从开始时建立一个 `DEFAULT_CONNECT_TIMEOUT`（5 秒）绝对上界，writer 获取、write_all、异步 flush 与有界 peer-close 等待共用同一剩余时间，shutdown 可取消；不给每步重新发预算，也不依赖持同一 writer 锁的 heartbeat 保证写入有界。版本拒绝的 peer-close 等待持续到客户端关闭、shutdown 取消或该预算截止，不另设原控制 ACK 的 50 ms 短窗。禁止调用同步 `FlushFileBuffers` 占住 Tokio worker：按 Microsoft 文档它会等到客户端消费缓冲，而 `DisconnectNamedPipe` 会丢弃未读数据。异步 flush 只排空已有适配器写入，不证明客户端已消费。

## 身份先于版本

Unix 目录/lease 身份、Windows 管道对象属主、进程 TokenUser/完整性及进程 pin 全部先于前导发送；不得为获得版本号绕过这些检查。缺监听者、权限或身份拒绝保留原分类。版本事实只报告已成功读取的内容，不证明同用户进程是可信应用。应用实例身份仍在同版元数据 ACK 中按原顺序验证；版本不符时不声称已验证该实例。

## 公开错误面

Python 继续只捕获 `c_two.error.CoreError`：新增 `category='protocol_version_mismatch'`，以及类上声明并在所有实例初始化为 `None` 的 `local_protocol_version: int | None`、`peer_protocol_version: int | None`；仅该 category 两值为 1…4294967295，其他错误均为 `None`（例：本端 14、对端 15）。该错误 `transport_kind='ipc'`、`transport_phase='pre_dispatch'`、`fallback_eligible=False`；`code`、`name`、`error_bytes` 与 OS cause 三字段均为 `None`，不新增 CRM 语义错误码或业务 error wire。

Rust 线格式解码只产出受检前导；`c2_ipc::IpcError::ProtocolVersionMismatch { local: u32, peer: u32 }` 与 Core `Error::ProtocolVersionMismatch { local: u32, peer: u32 }` 是仅有的类型化版本不符分支（Core 归一化直接映射，Rust SDK 复用 Core Error 不再包装），穷举匹配须补齐；畸形仍走 `DecodeError` / `Protocol` 分类，错误相位为 pre-dispatch。生成 TypeScript 沿用 `C2IpcTransportError`，新增只读 `category: "protocol_version_mismatch" | null` 与可空 `localProtocolVersion` / `peerProtocolVersion`；本地收尾失败时主错误与版本字段保留，收尾异常挂到标准 `Error.cause`。`C2IpcConnection` 的 write/readExactly/close 接口与 HTTP-only 加载不变。

## Relay 边界

Core relay-local 路由获取把类型化版本不符列为终态：完成本地清理后原样抛出，不继续后续路由、不改成 FallbackDenied、不落回一般连接失败 fallback，也不自动重试。HTTP relay 的远端上游是另一回事：relay 自身作为 IPC 客户端连接上游资源，其版本不符由 relay 投影为既有 `ProtocolViolation`（HTTP 502）并附 `route`、`transport=ipc`、`local_protocol_version`、`peer_protocol_version` details——这是 relay 与它上游的一对版本，不是把 relay 版本当调用方本端，调用方自身本端不因远端路由失配而变化。

## 兼容性

版本 14 之前的 RPC 握手没有此格式：新端不解析旧 payload、不尝试旧管道名、不凭 EOF 或错误码 2 猜版本；与旧端（含 0.9.x 及更早 wheel）混用仍是连接/协议失败，旧 wheel 一侧只能观察到 generic connection failure，没有类型化投影。低层手工握手调用者必须采用新时序；旧单帧 RPC 握手入口不保留。C/FFI ABI 6、SHM 3、owner control 1、正常业务帧/call/reply 布局与 CRM hash 不因本变化改变。

## Windows 固定命名

Windows 管道名为 `\\.\pipe\c_two-{scope}-{identity}`：scope 是进程 TokenUser SID 文本（如 `S-1-5-…`）UTF-8 字节的 SHA-256 小写全长十六进制（64 字符）；identity 是去除 `ipc://` 后 server_id 原始 UTF-8（保留大小写）SHA-256 前 16 字节的小写十六进制（32 字符）。监听唯一性由 `Global\c_two_listener-` 前缀接管道名 UTF-16LE 编码（逐 UTF-16 码单元按 LE 字节）SHA-256 小写十六进制构成的用户限权互斥体维护。名称中不含包版本或 IPC 协议版本；协议或包升级不得修改 TokenUser 来源、SID 文本与 server_id 编码/大小写、SHA-256 截断、名称前缀和分隔符，也不新增版本查询旁路、广播、注册文件或秘密 broker。

## 成本边界

成功新建物理连接新增一次本地往返和一对 25 B 记录；删除原请求/应答元数据各一字节版本后，握手总字节净增 48 B。已建立连接上的调用不增加往返。版本不符只需 Hello/VersionMismatch 一次往返，不发送也不等待可变元数据。当前生成 TypeScript 的 relay-aware 本地分支每次调用新建 transport 并在该次调用后关闭，该入口每次调用都承担新增握手往返，须单列测量；本规范不改变其连接复用策略。成功首建、持续复用与版本拒绝的实测时延及各平台验收结果以实测记录为准，本文不预先声明数值。
