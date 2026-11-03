# IPC 版本不符的固定握手前导与结构化诊断

状态：已完成实现、基线反例、定向与完整跨平台回归、延迟对照、端点零残留和独立审查；确切候选 `968f7d29153ff23fc967f767d97506823554de1d`，见[验收记录](2026-10-26-ipc14-version-diagnostics-acceptance.md)。本批不发版。基线输入 `79d544cc1efabaf34b3eac2af7075dc8a556cd43` 当时为 C-Two 0.9.0、IPC 握手 13、SHM 3、C/FFI ABI 6；本次仅握手切换为 14。R15 的已完成实现保持，R35 与两份控制会话重构继续暂缓。

## 已核对的原因与边界

普通客户端经 `IpcClient::do_handshake` 写出整份握手，再读取应答。`c2-wire::decode_handshake` 仅把首字节版本不符变成 `DecodeError::InvalidValue`；服务端 `handle_handshake` 将它变成 Protocol 错误，外层记录并断流，没有回绝消息。客户端因而先遇到 EOF，失去了服务端已经知道的版本事实。客户端目前还要收到完整应答 payload 才检查版本，不能仅凭一个固定前导判断。

Windows 当前名字是 `\\.\pipe\c_two-{scope}-{identity}`：scope 为进程 TokenUser SID 文本 UTF-8 的 SHA-256 小写全长十六进制；identity 为去除 `ipc://` 后 server_id 的原始 UTF-8 SHA-256 前 16 字节，以小写十六进制表示。它没有包版本或 IPC 版本。0.8.2 使用登录 SID，0.9.0 为跨登录连接改成用户 SID，这是旧/new 名称不同的源码原因；用户现场 Windows error 2 与之相符，本轮没有重跑现场探针。保持 0.9.0 算法即可让以后相同用户、相同逻辑地址的 RPC 客户端与服务相遇，不需要再换名字或查询旧名。

同一端点还复用 SIGNAL ping/shutdown 和 CTRL owner-control。它们在普通握手之前分流，owner-control 另有自己的版本与拒绝语义，私有 bootstrap 又是转交句柄的能力通道。本方案针对普通 RPC 的 `HANDSHAKE_VERSION`：在 HANDSHAKE 分流处先识别永久前导，不把另一套 RPC 协商加进独立控制协议，也不改变 owner ticket 或提交确认含义。未来文档中的“不同 IPC 版本可诊断”须明确指采用此固定前导的 RPC 连接，不能泛称任何本地通道、任意无响应进程都可报告版本。

## 最小协议切换

建议下一次 IPC 握手升至 14。复用现有 HANDSHAKE 帧的封套，永久固定下面的前导，作为版本选择之前就能识别的格式；即使未来业务帧布局再变，这个初始封套仍须按这些字节识别，不随业务帧编解码一起改。

| 字节 | 固定含义 |
| --- | --- |
| 0–3 | `u32 LE total_len = 21`，不含这四字节自身 |
| 4–11 | `u64 LE request_id = 0` |
| 12–15 | 请求 flags 为 `0x04`，应答为 `0x06`；不接受额外 flags |
| 16–19 | 固定 magic：ASCII `C2IP` |
| 20 | `1 = Hello`、`2 = Accepted`、`3 = VersionMismatch` |
| 21–24 | 发送方协议版本，`u32 LE`，范围 1…4294967295 |

仅前导使用上述 25 字节记录。它没有长度可变的诊断文本、支持版本列表、备用版本、保留字段或配置开关。版本号仅用于相等判断，不做降级协商。

1. 客户端在现有 OS 连接及原生身份检查之后，只发送 Hello 前导，等待固定应答；还不发送 SHM 名称、路由请求或业务数据。应用实例身份仍在同版的元数据 ACK 中按原顺序验证，版本不符时不声称已经验证了该实例。
2. 两端都先读固定 16 B 封套，核对 total_len=21、RID=0、精确方向 flags，再读固定 9 B 内容，核对 magic、kind 和版本；不得沿用长度驱动的 Rust payload 分配或 TS `readIpcFrame` 读取未知前导。服务端版本不等时写 VersionMismatch，携带服务端版本；相等时写 Accepted。
3. 只有 Accepted 且版本相等，客户端才发送原能力/SHM 元数据，服务端按原完整校验生成身份/路由/能力 ACK。删除旧元数据中的一字节版本字段及其重复检查，版本边界只由固定前导承担。
4. 初始协商在同一个握手调用栈内独占读取 Hello 和元数据，两个阶段各自核对 RID、方向和精确 flags；不退回普通分派循环等元数据，也不增加持久化的第二套状态。最终元数据 ACK 写完后，才发布 `handshake_done` 和 `ordinary_protocol`，开放业务及 heartbeat。重复 Hello、错序元数据、第二次握手及中途 owner 角色切换均终止连接。

源码还显示，现有 `handshake_done` 被写入却没有用于统一挡住未握手的 inline CALL/普通 CTRL。完整方案必须复用这个现成事实，在分配、路由准入或业务执行之前拒绝未完成握手的 CALL/路由 CTRL，并拒绝已建立连接上的第二次 HANDSHAKE；不能留下绕过版本协商的 RPC 通道。只保留已明确独立的 SIGNAL 控制和 OwnerControl 分流。这会收窄低层手工客户端“跳过握手直接发业务帧”的旧行为，须随 IPC 14 明确切换；测试中的原始业务连接统一迁移到现有真实握手 helper，原控制协议 fixture 保持独立。

版本 14 之前的 RPC 握手没有此格式。新端不解析旧 payload、不尝试旧管道名、不凭 EOF 或错误码 2 猜版本；与旧端混用仍是连接/协议失败。这不是新前导的测试替身：必须另用已发布的 0.9.0 wheel 验证两个方向，并保留 artifact 来源与哈希。

## 期限、关闭和身份

Rust/Python 客户端将两个握手阶段纳入同一个既有 `ConnectAttempt` 绝对期限和 handshake phase guard，不能各重新获得一份 timeout。Rust 与生成 TypeScript 均复用现有 connection close；协议不符是确定终态，不能进入 stale-pool 重连、relay HTTP fallback 或后台重试，也不能撤销仍有效的其他共享连接。当前生成 TS 的 IPC 握手没有独立的超时参数，本项不把 HTTP timer 搬来或新加配置；完整回绝立即终结，关闭必须唤醒挂起读写，不额外承诺无响应自定义 TS 连接的总期限。

服务端前导读取、回复写入及元数据读取均保留 shutdown/heartbeat 取消路径。初始协商期间禁止 heartbeat 插入业务 PING；复用现有尚未分类连接状态，最终元数据 ACK 写完后恢复普通协议状态。禁用 heartbeat 的原无 timer 策略不由本项重做。拒绝发送从开始时建立一个 `DEFAULT_CONNECT_TIMEOUT`（现为 5 秒）的绝对上界，write_all 与异步 flush 共用剩余时间，并允许 shutdown 取消；不得给每步重新发一份预算，也不能依赖持同一 writer 锁的 heartbeat 来保证写入有界。

Windows 拒绝按有界 write_all → 已有 LocalWriteHalf 异步 flush → 有界 peer-close 顺序收尾。现有控制 ACK helper 只提供至多 50 ms 的 peer-close 等待，本身没有 flush，也不证明交付。将其私有等待原语参数化：原 shutdown 调用仍用 50 ms；新版本拒绝一直保留到客户端关闭、shutdown 取消或同一个拒绝发送预算截止，不另设 50 ms 窗口抢先断管。这样慢于 50 ms、但仍在客户端原 handshake guard 内的读取不会仅因这个短窗丢失拒绝。异步 flush 只排空已有适配器写入，不冒充客户端消费证明。客户端读完固定回绝就形成结构化错误并在本地关闭，不等服务端 EOF；服务端关闭等待不能加到调用方返回延迟上。不要调用同步 `FlushFileBuffers` 占住 Tokio worker：Microsoft 文档明确它会等到客户端消费缓冲，而 DisconnectNamedPipe 会丢弃未读数据（[DisconnectNamedPipe](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-disconnectnamedpipe)、[FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)）。Named Pipe 必须实测完整回绝、延迟超过 50 ms 后读取、不关闭对端和 shutdown 竞争；不改变既有控制命令的 ACK 含义。

Unix 目录/lease 身份、Windows 管道对象属主、进程 TokenUser/完整性及进程 pin 全部先于前导发送；不得为了获得版本号绕过这些检查。缺监听者、权限或身份拒绝仍保留原分类。错误 magic、截断、矛盾的 Accepted/VersionMismatch、未收到完整应答及对端崩溃仍是原协议/I/O/期限错误，不能伪造 `peer_protocol_version`。这里只报告已成功读取的版本事实，不证明同用户进程是可信应用。

## 公开错误面

Python 继续只捕获现有 `c_two.error.CoreError`，不新增一个并行异常类。拟增加 `category='protocol_version_mismatch'`，以及类上声明并在所有实例初始化的 `local_protocol_version: int | None`、`peer_protocol_version: int | None`。仅该 category 两值必为 1…4294967295；其他错误为 None。例：本端 14、对端 15。`transport_kind='ipc'`、`transport_phase='pre_dispatch'`、`fallback_eligible=False`；OS cause 三字段为 None，不伪造系统错误码。它不新增 CRM 语义错误码或业务 error wire，`code`、`name`、`error_bytes` 保持 None。

Rust 的线格式解码只产出受检前导；`IpcError` 与 Core `Error` 保留类型化的版本不符及两个 u32，禁止从报错文本反向提取。Core relay-local 的 terminal 分类须显式接纳该错误，防止其落入已有的一般连接失败 fallback。Rust SDK 复用 Core 的 Error，不再包装一层。生成 TypeScript 沿用 `C2IpcTransportError`，增加类型化的版本不符原因与两个可空版本字段，同步实现相同前导和终态关闭；HTTP-only 加载和底层 `C2IpcConnection` 的 write/readExactly/close 接口不变。

TS relay-aware 的本地 `prepare` catch 当前捕获所有失败并尝试后续路由，必须在这个实际入口识别新错误，完成本地清理后原样抛出，不能改成 FallbackDenied 或继续 HTTP。若清理也失败，主错误和版本字段保留，收尾异常挂到标准 Error.cause，不能吞掉或覆盖主因。官方 Node close 使用本地 destroy 唤醒等待读写；自定义异步 close 的完成也必须只依赖本地收尾，不等待对端接受关闭，避免把新拒绝再次变成无限等待。

这会新增公开 Rust Error/IpcError 枚举分支，并把版本常量的表示扩至 u32；Python 的 category Literal 和错误字段、生成 TS 的错误观察面也变化。C/FFI ABI 6、SHM 3、正常业务帧、CRM hash、owner-control/ticket 编码不需要变化。低层手工握手调用者必须采用新时序；不保留旧单帧 RPC 握手入口。

Windows 端点约定同时固定 TokenUser 来源、SID 文本和 server_id 编码/大小写、SHA-256 截断、名称前缀和分隔符，以及监听租约 `Global\\c_two_listener-` 对管道名 UTF-16LE 的摘要。协议或包升级不得修改它们；不增加版本查询旁路、广播、注册文件或秘密 broker。

## 改动范围与成本

生产范围为 `c2-wire/handshake` 固定前导与元数据 codec、`c2-ipc` 握手读写及 typed error、`c2-server` 握手分流/收尾、Core 错误和 terminal 分类、Python 错误投影、生成 TypeScript transport。普通 IPC、relay 选出的本地 IPC、Rust/Python/Node 均进入同一协议边界。现有普通 handshake 完整校验、SHM 方向和代际校验保留；不新增通用会话框架。

本项是诊断正确性补强，预期代码净增。静态估算生产 +150–250 行、测试 +200–350 行，主要为固定字节 golden、真实双向拒绝和错误透传；该估算不是实现结果。优先替换当前“改首字节版本后只验证 decoder 报错”的测试及旧单帧 fixture，不叠加两套协议测试。文档更新协议说明、Python/Rust/TS 错误/API、Windows 命名约定及下一版发行说明；不借机更新无关能力或发布版本。

成功新建物理连接新增一次本地往返和一对 25 B 记录，删去原请求/应答各一字节版本后，握手总字节净增 48 B；已建立连接上的调用不增加往返。当前生成 TS 的 relay-aware 本地分支每次新建 transport，并由 `callPreparedLocalIpcAndClose` 在该次调用后关闭，因此该入口每次调用都会承担新增握手往返，必须单列测量；本项不顺手改其连接复用策略。版本不符只需 Hello/VersionMismatch 一次往返，不再发送或等待可变元数据。成功 connect 的额外时延和版本拒绝的 P50/P95 须在同环境测量，计时从 SDK 调用入口直到异常返回，包含失败清理，不能只量读帧耗时。成功首建、持续连接复用及 TS relay-aware 本地路径分开记录，不预先把本项称为性能优化，也不承诺各机器固定为用户实测的 40 ms。

## 实施后的验收

- 先在基线补齐反例：原服务面对手写异版本前导不能返回新回绝，原客户端无法投影结构化版本；保留真实失败证据。手写 golden 独立于生产 encoder，覆盖高/低版本、u32 边界、非完整/畸形记录及同版本成功。
- 在真实 Unix socket、Windows Named Pipe 上双向执行：只发固定前导的假客户端接真实服务；只实现固定前导的假服务接真实客户端。断言早于可变正文、路由准入、SHM 引用和业务调用失败，错误携带本端/对端正确数值。
- Core/Python/Rust/Node 验证字段、序列化、连接终态只发布一次、无等待线程/端点/lease 残留、失败候选未污染池，以及版本不符不进行 HTTP fallback 或自动重试。身份拒绝、NotFound、EOF、timeout 与 malformed 不得混成版本不符。
- 原 ping/shutdown、owner inspect/takeover、private bootstrap、准入恢复及 shutdown 排空全量回归；它们保持独立控制协议。完整 relay、18 行 Rust/Python、12 行 Node/TypeScript 及真实跨 session SHM 能力路径保持。
- 已发布 0.9.0 wheel 与新构建各当一次客户端/服务端；按相同用户、根目录及逻辑地址测试，保留发行物哈希。只断言旧新组合不执行业务并明确记录实际连接失败，不从 EOF、错误码或文本推断版本。
- Windows 优先开发机，Linux/macOS/Windows x64/ARM64 对确切候选执行 GitHub Actions。记录平台、架构、Python、原始日志与源码/wheel 哈希；未实测的平台明列缺口。只处理本批私有端点并及时清理构建产物，历史目录不动。

以上保留批准时的设计与静态估算；实施后的真实增量、失败处理、性能和完整平台结果以验收记录为准。

独立只读复核使用 hey-my-buddy：Windows 命名与分流 run `874a7ddf-d21e-4fd1-b638-c7e95932a5e2` / artifact `b904c0f1-1c88-4e1f-81c1-ba4962a89415`；方案审查 run `34ee5035-cdb9-4798-95cc-c95824006648` / artifact `0db5d0b7-2e0a-418b-80df-26a4d1f3ca46`。Host 已将 Node 实际 fallback catch、最终 ACK 后开放 heartbeat、固定长度读取、Windows 拒绝发送与等待预算四项要求纳入本方案，并明确未握手 RPC 的接纳围栏。审查没有执行测试，报告已接受，工作树已回收；原始记录保留于 `~/.codex/diagnostics/c2-ipc-version-design-21ir7k2f/`。

## 实施阶段

1. 固定基线反例与旧版 wheel 来源，先验证双向拒绝、未握手 RPC 及错误投影的旧行为。
2. 同步迁移 Rust/Core/Python 与生成 TypeScript，替换旧单帧 fixture；完成定向回归，保留控制协议边界。
3. 真实三端、旧版双向混用、成功连接与版本失败延迟对照；确切候选完整 relay、跨语言和 GitHub Actions。
4. 独立审查、端点零残留、代码净量与文档收尾后提交，清理构建产物，更新主线清单；不发布包。

实施复核补充：`max_frame_size` 可以合法地小于固定前导的 21 B body（需同步满足既有 `shm_threshold` 约束）。固定前导独立于可变帧上限；保留其长度/flags 严格校验，同时在普通分派前拒绝超限的其他帧，元数据仍使用原上限。不得把这一协议封套当作放宽业务帧配置的理由。
