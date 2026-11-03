# 连接终态、准入竞态与所有者接管：最小方案

2026-10-22：非 Hello 拒绝分支已纳入原 setup 期限，保留身份、准入、错误顺序和释放语义。基线反例、针对性回归、完整 relay、macOS/Linux/Windows 开发机与 Windows CI、端点零残留和独立审查均完成，生产净增 0 行；[验收记录](2026-10-21-owner-takeover.md#%E9%9D%9E-hello-%E6%8B%92%E7%BB%9D%E6%9C%9F%E9%99%90%E4%BF%AE%E5%A4%8D)。本次构建产物已清理，本地分支、标签和历史目录未操作；两份会话重构方案继续暂缓。

状态：最小方案已确认；A 连接通知、B 准入竞态修复已完成各自验收，C 所有者接管已完成独立审查、性能对照与精确候选三系统验收，两项收尾局部精简也已实施并完成定向验收。基线 `0acf784f6742487cd619a7d419379f693434a337`；发布比较为 0.7.6（`fe43f20257947c466d8908d7e47d5a36ead35f99`）。2026-10-21。

## 三项独立边界

不再增加所有连接共用的角色首帧，不增加 `Suspended` 路由 wire 状态。逐项核对后，本方案不需要修改握手版本 12、业务帧、调用/回复编码、路由目录表示、SHM 3 或 C/FFI ABI 5。接管仅增加一组独立定版的 owner 控制消息，复用现有帧。若实施发现上述未改部分必须改变，应先给出具体反例和范围修订，不自动扩大改动。

| 项目 | 必需 API 变化 | 必需控制协议变化 | 理由 |
| --- | --- | --- | --- |
| A：连接结束通知 | Rust/Python 增加只读连接观察、等待和服务端订阅 | 无 | 现有接收循环已经观察 EOF/reset/DISCONNECT/心跳结果，缺的是应用出口 |
| B：准入竞态 | 无公开签名变化；内部登记 token 增加精确身份和准入代次 | 无 | 迟到 final 响应不能重开已关闭的准入 |
| C：所有者接管 | 替换单向 owner 类型；增加票据、准备/提交接管；调度错误增加临时不可用投影 | 新 owner 控制消息；现有 negative shutdown ACK 表示 OwnerBound 拒绝未鉴权停机 | 新控制者必须证明能力，原子接替存活关系；旧 owner 不能绕过撤销 |

被替代的单向 owner 实现、旧类型/别名/格式同步删除。未改变的业务协议继续只有一种表示。新 owner 协议只接受其唯一版本，不降级，不回退到普通 admin 或 HTTP/relay。

## 证据与原设计

原设计用私有继承通道把所有者与普通客户端分开：readiness 前 arm，EOF 后关准入，宽限期后进入原生 drain。客户端断连和 relay 驱逐不能终止所有权。Core 明文说明首版不重连、不接管，这是跨平台能力边界。

此前已在 macOS arm64 用真实 OwnerBound、原生主机和 IPC 调用，配合受控 loopback HTTP relay 登记应答复现。屏障只扣住 final 响应；该夹具不冒充完整真实 relay。

| 候选 / 场景 | final 后 phase | 实际结果 |
| --- | --- | --- |
| 0.7.6，无 owner 丢失 | Armed | 登记成功、回调 1 次，对照通过 |
| 0.7.6，owner EOF，final 成功 | OwnerMissing | 回调 1 次，安全断言如预期失败 |
| 0.7.6，owner EOF，final 409 | OwnerMissing | 登记失败、回调 0 次，对照通过 |
| 当前源码，无 owner 丢失 | Armed | 回调 1 次，对照通过 |
| 当前源码，owner EOF，final 成功 | OwnerMissing | 回调 1 次，安全断言如预期失败 |

五例均在断言前完成原生 shutdown，socket/lease 零残留。隔离源码核对 845 个 tracked 路径，生产源码未改；已清理产物 366,462,281 字节。本版未重跑这些实验；Linux/Windows 没有这组新实测。[证据记录](evidence/2026-10-21-owner-missing-relay-registration-repro.json)保留版本、wheel 和夹具/日志 SHA。

根因链路：`Runtime::register_route` → `commit_reserved_route_closed` → await relay final → `open_route_admission`。最后一步只检查停机代次和 Stopping，owner EOF 不改变它们。来源为 `c2-core/src/session.rs`、`owner_bound.rs` 和 `c2-server/src/server.rs`。

[原路由设计](../reference/contracts-and-routing.md)把 Ready、合同/身份有效和准入开放列为独立条件。Ready 不保证执行容量。故主机暂停可以保留已发布路由的身份，由准入拒绝调用，无需新增 wire 状态或撤回后重登记。

## A：复用原生终态，新增观察 API

每次物理连接一个终态记录和通知源，来自现有接收循环、心跳和关闭路径。它替代底层重复的 live/disconnect 权威；不顺带改变 SDK wrapper 的逻辑 close 约定，不另起 reader、watchdog、轮询线程或长业务调用。观察对象仅持身份/元数据，不持 stream、pool、runtime、resource 或 payload lease。

观察/订阅不能启动 server、建立连接或解析/冻结 IPC 配置；订阅后仍允许原本合法的启动前配置。共享终态元数据放在现有 Core/transport crate，不为此新增 crate 或通用事件框架。

原因限定为 local_close、peer_close_message、peer_eof、connection_reset、heartbeat_timeout、protocol_error、io_error。只有收到 DISCONNECT 才称对端明确请求关闭；EOF 不推断进程死亡。两端原因可以不同；诊断仅含有界操作/OS 错误，不含 payload/secret。

终态提交一次，在传输不可用时发布，先于回调 drain/内存释放。心跳先发布 timeout 再 abort，本地 close 同理；drop/unpolled-task 兜底不覆盖已有原因。取消等待只取消 waiter；晚等待返回同一终态。重连产生新观察，不暗中换绑旧对象。

| Rust / Python API | 合同 |
| --- | --- |
| `Client.connection_observation()` / `cc.connection_observation(proxy)` | 当前 IPC 物理连接；不触发连接/重连；thread-local、HTTP-only 明确不可用 |
| `ConnectionObservation.snapshot()`、`wait_closed(timeout)` | 原生状态/等待；Rust 有 async 等待，Python 阻塞等待释放 GIL |
| `Runtime.connection_events()` / `cc.connection_events()` | 自订阅生效点接收本 runtime 的 Opened(observation)、Closed(record) |

服务端用 Tokio bounded broadcast，内部固定容量 1024，不新增配置。溢出明确 Lagged 并结束该订阅，不静默丢失、不阻塞传输、不存无限历史。已取得的单连接观察仍可等待自身终态。需要完整记录须在监听启动前订阅；晚订阅覆盖之后事件（也包括旧连接之后的关闭），不伪造历史 Opened。runtime 退出前发布已接受连接终态再结束 feed，订阅不能延长 runtime 生命。

连接 ID 不是 PID/用户/CRM 身份。多个代理可能共享连接；服务端看到直接相连的 relay。通知不证明业务排空，不自动释放 Held，也不触发 OwnerBound 关闭。本项不增加 CRM 参数、调用上下文框架或 wire 字段。

## B：使迟到登记 token 失效

用一个小型原生准入状态/代次替换独立关闭 bool 和 shutdown-only 代次，复用现有登记串行化边界。owner 丢失、终态关闭使旧登记 token 失效。token 绑定 server instance、route UID/revision 和准入代次，不仅按 name 找回权限；公开 token 继续不透明，无新 SDK 参数。

最终开门和关闭在同一短临界区提交，不能只在 await 前补读 bool。先取得登记/dispatcher 访问，再最终复核状态和 route 身份；锁内无网络等待、async 锁或用户回调。EOF 关闭权威后，迟到 final 成功只能失败收尾。失败回滚按原 UID/revision 清理本地路由及已捕获 relay scope，不能误删同名新登记。

B 保持原验证/错误顺序、失败来源、relay 撤回、drain/lease 语义，不引入恢复或路由目录状态变化。原反例、正常/失败对照、跨 await 关闭、迟到回滚与同名替换构成独立验收。无公开签名、错误码、wire 变化；C 在同一准入权威上增加有权限的恢复，不再建第二套开关。

既有 `begin_start_attempt` / readiness fence 和 Persistent 的 stop/start 仍须成立。新启动使用新准入代次，使上次运行的 token 失效，但不自动重开已终止路由。OwnerBound 重启仍需有效的新 bootstrap，旧 grant 不随启动复原；不把跨进程重启当作宽限期内接管。

## C：统一 owner session，限定新增控制面

初始继承与接管通道统一为双向 owner session，由 Core 同一个 supervisor 处理控制消息和 EOF。Unix 用非阻塞 CLOEXEC socketpair；Windows 用本地、当前登录 SID、byte-mode duplex Named Pipe。receiver 转交子进程后才注册 reactor/IOCP；已注册的 Windows 文件对象不迁移后重注册。初始信任仍来自受控继承，readiness 前必须 arm。

`owner_control_pair()` 返回 `OwnerControl` 与一次性 `OwnerBootstrap`，替换旧 Keepalive/Receiver 及 Python native receiver 类型，不留别名。`adopt_owner_stdin()` 仍显式，改验新双向对象。启动后的 `attach_owner_control` 仍拒绝，不能绕过票据。unsafe raw fd/handle adoption 仍由可信 launcher 保证来源，不新增 safe 任意句柄升级。

公开替换清单包括 Rust `OwnerControlKeepalive`、`OwnerControlReceiver`，Python `OwnerControlKeepalive`、`NativeOwnerReceiver`。Rust `Runtime::attach_owner_control` 的参数类型随之改为 bootstrap；Python `spawn_owned_child(receiver, ...)` 保留参数名，只接受新 bootstrap 投影。旧 keepalive 的 `shutdown()` 等同 close 的别名删除，新 `close()` 与 `request_shutdown()` 含义分开。底层公开 `MsgType` 增加 OwnerControl，`SchedulerAcquireError` / Core `RouteConcurrencyError` 增加临时不可用分支；Rust 穷举匹配须同步迁移。无需给 HostLifecycleSnapshot 增加字段。

### 唯一新增消息组

复用 `[length][request_id][flags][payload]` 与 `FLAG_CTRL`，增加 OwnerControl 消息类型（当前空闲 `0x16`），payload 为 `[tag][owner_version=1][operation][body]`。body 仅含身份、代次、票据及有界结果，不含业务参数、FastDB/pickle 或 SHM。最大 4 KiB，按 header/tag 检查再分配 body；版本、格式、未知操作直接拒绝。普通握手/Ping/route control/call/reply 表示保持。

首个普通合法帧固定普通连接角色；首个 OwnerControl hello 固定 owner-control 角色。共享帧解析，之后不能相互升级/混用。选择 owner 角色不授予权限；只有能力提交可以。私有继承通道由构造固定角色，用同一 owner 消息协议。普通连接发送 owner 消息、owner 连接发送业务或普通管理消息均拒绝。

首帧未分类时沿用现有 heartbeat timeout 截止规则，但不发送业务 Ping；首个普通帧后启用原业务 Ping。heartbeat 禁用时不虚构新默认期限。owner 角色选定后退出未分类观察：OwnerMissing 候选受原 grace 截止限制；Armed 只允许一次有界 hello/status，拒绝接管并关闭候选。正式 owner session 以通道终态判活，不增加 owner 心跳或 relay 闲置驱逐。整个入口只有原生 IO/定时事件，不另加轮询。

新 owner 候选的服务端 setup 上限复用 `c2-local::DEFAULT_CONNECT_TIMEOUT`（当前 5 秒），从识别 owner 类型时计算；OwnerMissing 时再取与原 grace deadline 的较早者。它独立于远端声称的预算，不因拆包或请求重来而重置，无新增配置项。初始私有通道仍用既有 startup fence；readiness 前的票据请求只在其原预算内等原生 readiness，不能先发行未绑定 endpoint 的能力。

必须接受并验证的低层时序差异：未发首帧的对端不再先收到业务 Ping；其原有回收期限不能放宽。标准 SDK 客户端主动发送握手/探针首帧，格式不变。此必要分类调整不成为全连接新首帧改造。

操作仅有 hello/status、get_recovery_ticket、revoke_recovery、takeover、request_shutdown。每 session 一个在途控制请求，复用 request_id；无通用管理 RPC、远程 owner 入口或第二监听地址。OwnerBound 的普通 shutdown（含 pre-handshake/duplicate initiate）复用现有 negative ACK 拒绝；有效 owner 用新控制请求停机。Persistent 管理行为与 in-process Runtime shutdown 保留。

### 能力、期限与失去确认

Core 只存当前 owner session/epoch/phase/deadline 和最多一份 grant。仅当前有效 session 可取/撤 grant。OS CSPRNG 产生 256-bit secret，绑定 instance、owner epoch、逻辑地址及原生 endpoint context 身份。重复 get 返回同一当前 grant；revoke 幂等清空，再发行才换 secret。每次变更在提交点核对当前 session/epoch/phase；旧 session 已认证但迟到的 revoke/shutdown 无副作用。

票据是不透明、repr 脱敏、不隐式 pickle 的值，仅显式 export/import。它含命名空间身份摘要，不含让 SDK 自动采用任意目录的指令。新控制者使用既有 Runtime endpoint 配置；路径别名按原生目录身份验证，错 root/instance 拒绝，不切默认目录。应用负责在控制者之外保存票据；C-Two 不建 secret 文件、历史表或后台恢复器。

由现有 Runtime 准备接管：本地生成下一 epoch 的 successor_ticket，允许发送前保存；Core 原子授权安装前它尚无效。提交同时携带当前票据与继任提案。secret 用现成 constant-time 原语比较；不自制挑战/签名服务，不增加全局 challenge 槽。

提交须同时满足 OwnerMissing、单调时钟 `now < 原 deadline`、未 shutdown、grant 匹配、候选未被观察为终态。所有 await 后再核对；在同一短临界区消费旧 grant、安装继任 grant、提升 epoch、替换监督通道并恢复主机准入。旧 EOF/timer/命令带旧 epoch，不能作用于新 owner。两名持同票据者最多一人成功；无成功 ACK 前失去通道监督的移交窗口。

| Rust / Python API | 合同 |
| --- | --- |
| `OwnerControl.recovery_ticket(timeout)` / `revoke_recovery(timeout)` | 当前 session 的发行/撤销；不隐式持久化 |
| `Runtime.prepare_owner_takeover(ticket)` / `cc.prepare_owner_takeover(ticket)` | 使用现有 endpoint context 本地准备，提供可预存的 successor_ticket |
| `PreparedOwnerTakeover.inspect(timeout)` | 仅 instance/owner epoch/phase，不证明自己的事务曾成功 |
| `PreparedOwnerTakeover.commit(timeout)` | 一次控制提交；成功返回 OwnerControl |
| `OwnerControl.request_shutdown(timeout)` / `close()` | 前者请求原生 drain，ACK 只证明 initiate；后者结束所有者关系并进入 grace |

控制 API 的结构化结果区分格式/版本/身份拒绝、owner 仍在、grant 失效、grace 到期、shutdown、确定未发送、发送后结果不确定；不新增一套全局错误注册表。SDK 等待使用一个原生绝对预算。尚未取得 session 串行槽的 waiter 超时仅结束自己的等待，不能关闭别人正在使用的 owner session。取得槽并开始写入后，部分写/应答超时才使该专用会话退出，不把迟到应答交给后一个请求。

| 边界 | 结果 |
| --- | --- |
| `now >= deadline`，即使 timer 调度晚 | 拒绝并走原 grace-expired 路径，尝试不延长窗口 |
| 错票据/半包/竞争失败 | 不修改 owner、grant、准入或原 deadline |
| shutdown 先提交 | 接管拒绝；接管先提交后，可信 in-process shutdown 仍可停止主机 |
| 已发送，ACK 丢失/调用方超时 | indeterminate，关闭候选，不自动重放、不报告成功 |
| 曾提交但随即断连/控制者死 | 新 epoch 进入正常 grace，预存 successor 可继续接管 |
| inspect 后状态变化 | commit 重新验 grant；不允许把 inspect 当授权或成功收据 |
| WatcherIoError/Draining/Finished | 不恢复，保留原失败诊断和 drain |

indeterminate 保留准备对象/票据信息。应用可按当前 epoch 显式选取已保存票据再尝试；旧 grant 未消费才有效，消费后只有已授权 successor 有效，竞争者获胜/撤销则自己的票据拒绝。无自动 old/new-key fallback、永久收据表或失败猜成功。撤销范围为 C-Two owner 权限，同用户 OS kill/debug 和应用自身凭据不由该协议撤销。

### 只恢复主机 gate，不重开路由

OwnerMissing 令 B 的主机准入暂停并换代，拒绝新许可、通知等待者；pending 许可记住代次，在所有等待后、开始资源回调之前复核。已取得 route guard 但仍等待全局 execution slot 或 spawn_blocking 调度也算尚未最终提交，不能漏检。执行许可的最终提交点与暂停串行化；一旦越过该点便属于已准入工作，即使 OS 随后延迟执行其回调，也保留原 active/drain/lease 规则。Python thread-local 走同一原生 gate。

所有同时访问 gate 和 scheduler 的路径固定 gate → scheduler 锁顺序；等待前释放锁，通知等待者遵守先登记通知再复核状态的规则，防止错过 pause。Core 独占构建时取得的所有者准入控制权，不向 SDK 暴露通用 resume 开关；跨 crate 的内部接口只服务这一权威，不能新增 safe 外部强制开门入口。计数仍由原 guard 增减，不另建执行计数/释放状态机。

暂停不关闭各 route scheduler，不把 Ready 改成 Draining/Closed，不发 withdrawal。恢复只打开主机 gate，无“遍历路由逐个重开”。显式 close/unregister 自己的状态继续生效；Closed(RegisterCommitted) 仍须有效 final token，B 使旧 token 失效；旧 pending 睡过完整暂停/恢复仍不能成为新许可。

本项需给公开 Rust `RouteConcurrencyError` 及底层 scheduler 错误增加临时不可用分支，Python 投影到既有 ResourceUnavailable。远程也用现有 error reply 的 ResourceUnavailable，不加 wire 字段。身份、合同、方法校验保留原先后，暂停不能掩盖身份错误。

现有 scheduler snapshot 的 `closed` / `is_unconstrained` 从 route 状态和主机 gate 一致投影，不另存 bool；暂停期间不得显示仍可无约束准入。计数和 held owner 仍由原 guard 管理，主机暂停不伪造 active/pending 或 lease 归零。

明确的行为变化：grace 内 catalog/relay 仍可发现 Ready，connect 可成功，但调用被主机 gate 拒绝；错误由终态 ResourceClosed 改为 ResourceUnavailable。Ready 与执行准入分离有原设计依据，但这是对当前 OwnerMissing 观察的修订。帧已发送时保持现有 transport phase/重试规则，不能仅凭该错误码或文案制造 pre-dispatch 重试证明，不自动重放旧业务请求。

现有 relay attestation 看同一 Ready 即保留登记；`semantic_withdrawal_reason` 不把普通 CrmError/ResourceUnavailable 当撤销。故 watcher、FULL_SYNC、缓存绑定可继续持有同一 UID/revision，无暂停同步协议、tombstone 修补或重登记重试。grace 到期才走既有终态关闭/撤回；显式关闭、注销、合同或实例不匹配照旧处理。真实 relay 回归必须证明 idle eviction/reconnect、relay 重启也成立，不能只靠此静态推理。

## 替换范围、成本与回归

A 改 `c2-ipc` 接收终态、`c2-server` connection/heartbeat、Core client/runtime 与 SDK 投影。B 改 Server 登记临界区、Core 精确回滚和现有生命周期测试。C 复用 A/B，再替换 `c2-local/owner` 两个平台实现、Core supervisor、一个 owner wire 模块及 SDK owner 投影。业务编码器、SHM allocator、C FFI 表和 Node owner-management 面不扩展。

测试进入现有 connection、owner_bound_lifecycle、registration、relay、SDK 集成测试，删除被替代的单向 owner 夹具，不建框架。只新增独立手写 owner golden；现有握手、业务、SHM golden 须逐字节不变。新成本为终态元数据、有订阅时的 bounded feed、pending 代次、一个 owner session/grant；无 payload 拷贝、owner 轮询、每票据文件。共享 gate 有同步成本，实施时做同环境对照，不在设计阶段虚报净减量或零开销。

| 范围 | 必需覆盖 | 平台 |
| --- | --- | --- |
| A | 分别杀 client/server；idle client 无在途调用；DISCONNECT/reset/心跳与 abort 竞争；未 poll task；晚/取消等待；pool 共享；Lagged；观察不保活 runtime/payload，不启动或冻结配置 | Linux/macOS UDS、Windows 2022/2025 Named Pipe；Rust/Python及现有 Node IPC |
| B | 已复现 final 成功反例、正常/失败对照；跨 await 关闭、迟到回滚与同名替换；错误顺序、精确 scope、原 stop/start fence 与旧 token 失效 | 三系统 Core/Python、真实 relay |
| C 原生 | readiness、错对象/模式、重复/迟到 attach；Unix exec/raw-fork；Windows 未注册转交/单 IOCP/重复 EOF；业务断连和 relay 驱逐不改变 owner | 三系统原生和 Rust/Python 真子进程 |
| C 入口 | 新控制版本/大小/未知操作；角色混用；分帧、沉默初始连接的原截止；heartbeat disabled；普通首帧不变；pre-handshake/duplicate shutdown 拒绝；无 pool/SHM 副作用 | 三系统真实流，现有 golden 不变 |
| C 权限与期限 | 发行 ACK 丢失、重复 get、撤销/再发；脱敏；root/别名/目录替换/instance；重放、双竞争者；旧命令/EOF/timer；deadline 前/等于/之后；setup 超时、串行槽 waiter 超时隔离、半包、ACK 丢失、立即死、successor 恢复 | 三系统真实进程和受控 native 事件，不延长断言过关 |
| C 准入 | idle/active/route queued/global queued/spawn_blocking queued；睡过 pause-resume 的旧请求；未完成登记、关闭/注销/替换；Held、预算、快照、错误顺序 | 远程 IPC、thread-local、大 payload |
| C relay | 旧/新连接；Ready 可发现但拒调；恢复不换身份；cache、idle eviction、FULL_SYNC、relay down/restart、双 root、最终 withdrawal 和 grace-expired reason | 完整 relay、Rust/Python/Node 跨语言及持久连接 |
| 收尾 | graceful close；OS kill 后 wait 确认再 native reap；socket/lease/kernel handle 零残留 | 仅本次测试端点，排除历史目录 |

确认后按 B、A、C 实施：每项先基线补反例，再实现、针对性回归和独立审查。C 必须连同准入与 admin 限制形成完整候选；不接受只有 takeover API 的中间产物。Python 3.10 与已配置 free-threading 门禁保留，mock 不替代完整 relay/真实跨语言。

owner 类型替换和 OwnerMissing 行为变化仍需确切候选 GitHub Actions Linux/macOS/Windows 全部门禁及源 commit/run URL；不升 IPC/ABI 不能豁免。本地分支/标签不动，不发版；后续授权仅向 fork 的专用验证分支推送 CI 候选。验证后及时清理本次产物，保留源/日志/证据，不碰历史目录。

## 独立审查

本版经 hey-my-buddy 0.50.0 独立只读审查：run `50e336dc-3efb-4e5b-bbc0-b596b9fb985f`，固定输入 `d29a7ec7817e2a1577264f13c6435b7ae5f18165`，artifact `b44a9516-4fc3-40c7-b45e-30dd80033664`。结论无设计阻塞项，明确认可 A/B 不改 wire、C 的 Ready/gate 分离及 scoped owner 协议。Host 已核对输入文档字节、空输出 patch 与上述源码依据，并验收该审查。

Host 收尾补全了具体公开类型清单、原 start fence、候选 setup 上限、串行槽等待者超时隔离、snapshot 投影等验收约束。上述记录为实施前的设计/静态边界审查，不作为生产实现、构建或平台运行验收；分批实施状态见下文。

2026-10-21：用户批准按 B、A 顺序分批实施。B 的基线反例、实际修改、独立审查修正、完整 relay、本机端点验收及清理记录见[准入修复验收](2026-10-21-admission-race-repair.md)。本批不含 A/C；Linux、Windows 运行证据不由 macOS 结果替代。

2026-10-21：C 的完整验收源为 `ea511c7a00be024134bade2cc3e5a137f06a96fe`；[验收与精简复审](2026-10-21-owner-takeover.md)记录能力、准入、期限、跨平台及清理结果。9 个 CI jobs 和 118 个适用门禁全部通过。随后两项局部精简 `b346e2b3` 已完成 macOS/Linux 定向回归、端点零残留及独立审查，并补齐 Windows 定向 CI 与开发机 9 项实测；原完整 CI 记录仍绑定上述源。
