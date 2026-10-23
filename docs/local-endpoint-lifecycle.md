# 本地端点生命周期与运行器接入

[English](local-endpoint-lifecycle.en.md) · 简体中文

本文说明原生端点所有权、运行器接入与回收规则。目录配置见 [配置指南](configuration.md#本地端点位置与平台差异)。

本地端点由 Rust `c2-config` 统一派生，SDK、relay 和 c3 使用同一上下文。Unix 默认路径为 `/tmp/c2-<uidhex>/<32 字符标识>`，协调记录包含 nonce gate/marker 与可退休的监听租约；原 gate inode 保持固定直到监听租约释放。Windows 使用当前登录 SID 的 Named Pipe 和不可继承的内核监听租约。调用方传逻辑 `ipc://` 地址，系统自动选择后端；凭据中的 `managed-v2` / `named-pipe` 是格式元数据。

Unix 的最终端点目录由显式代码 `cc.set_local_endpoint(root=...)`、CLI `--ipc-root` 或环境变量 `C2_IPC_ROOT` 覆盖；代码/CLI 优先于进程环境，进程环境优先于 `.env`，默认 `/tmp/c2-<uidhex>`。端点路径为 `<root>/<32 字符标识>`，没有额外用户目录、版本目录或 `.sock` 后缀。自定义目录由应用预建，允许 `0755`：当前用户持有并具备读、写、遍历权限，组和其他用户不可写。默认目录创建为 `0700`；socket 在 listen 前设置并核实为 `0600`。Linux/macOS 的 bind、connect 和探测均通过目录句柄访问短名称，支持包含中文和空格的长目录，实际目录打开能力仍由文件系统决定。回收保留应用目录和无关文件。root 不移动 SHM、file spill 或配置文件。Runtime 在第一次本地 bind/connect 或准备 owner 接管时冻结上下文，失败尝试同样冻结；之后修改 root 返回配置已冻结错误。纯上下文查询不产生文件系统 IO。同一逻辑地址在不同 root 中使用独立上下文，实际连接仍验证 server id、instance id 与路由契约。Windows 自动使用当前登录 SID 的 Named Pipe，明确拒绝 Unix root 配置。

macOS 还会检查目录句柄上的扩展 ACL：只读、遍历和 deny 条目可用；授予新增、删除、修改属性或权限等能力的 allow 条目会被拒绝，包括继承条目。启动失败会显式释放监听租约锁，重复文件描述符不会继续占有该锁。

macOS 默认命名空间的 socket 路径仍在 `/tmp/c2-<uidhex>`；只有 `.gate`、`.gate.marker` 和 `<标识>.lease` 存放到 `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination`。这是系统提供的非临时用户目录，查询发生在原生 I/O 阶段。现存默认目录的路径别名按目录身份共用同一协调器，公开 namespace id 和凭据格式不变；显式别名缺失时仍由应用预建。Linux 与非默认自定义 root 的记录仍与 socket 同目录。

双目录使用同一 gate 锁并分别验证目录与对象身份，监听器还持有原目录句柄直到租约释放。正常 close/Drop 回收自身 socket 和 lease；异常退出后复用精确 reap、有预算的 sweep 或后续 bind。重启或系统清理使 socket 目录消失后，完整且已解锁的 lease 仍可回收，维护接口不会为此重建 socket 目录；存活租约仍拒绝第二次绑定。目录替换、损坏记录继续拒绝删除。

协调记录本身有清理场合：每个端点的 lease 随上述原生收尾撤回，失败回滚只撤回本次创建项；损坏历史 lease 保留待单独处理。gate/marker 是固定协调身份，在线不删除；没有新增后台清扫。切换旧版布局须先停止并禁止重启所有旧进程，将旧 socket 目录完整归档，再由新版初始化同名目录。该操作须单独安排；新版不在线迁移历史记录，也不与旧版共享该命名空间。

Unix bind 初始化失败时，只撤回本次新建且仍由原目录、gate 和文件身份确认的 lease；若已经绑定 socket，须先确认该 socket 已安全撤回。既有 lease、替换项及无法确认身份的对象继续保留。这一回滚利用当前调用仍持有的创建证明，不扩大历史损坏记录的 reap/sweep 权限，也不保证进程在初始化中崩溃后的自动清理。

Unix 凭据统一使用既有 schema 3，记录最终 root、namespace id、incarnation 与 socket 身份；编码、解码和 CLI 读取共用 32 KiB 上限。Windows 保持 kernel-managed schema 1。inspect/reap/sweep 使用凭据捕获的上下文，显式传入不一致的 root 在访问目标前被拒绝。凭据中的 socket 完整路径由原生配置派生，不作为输入字段；凭据自身不授予删除权限。

以下 owner 接口和 grace 语义适用于 C-Two 0.8.0，升级范围见[发布与迁移说明](releases/0.8.0.md)。默认生命周期为 `Persistent`，常驻服务直到显式关闭；普通业务连接断开、idle 驱逐、relay 重连不结束服务。只为一个控制器独占管理整个 Runtime 的专用子进程选择 `OwnerBound`。`control, receiver = cc.owner_control_pair()` 返回 `OwnerControl` 和一次性 `NativeOwnerBootstrap`；Rust 对应 `OwnerControl` 和 `OwnerBootstrap`。控制器保存 control，用 `cc.spawn_owned_child(receiver, program, args)` 只向目标子进程转移 bootstrap；`receiver` 是保留的参数名。子进程显式调用 `bootstrap = cc.adopt_owner_stdin()`，并在 `register()` 前设置 `cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0), owner_control=bootstrap)`。

初始关系使用私有双向通道，由 Core 在 readiness 前 arm；Unix 为非阻塞 CLOEXEC socketpair，Windows 为当前登录 SID 的本地双向 byte-mode Named Pipe，bootstrap 转交子进程后才注册 reactor/IOCP。继承来源由可信 launcher 保证，不能将普通业务 stdin 或任意句柄升级为 owner 能力。启动后不能通过再次 attach bootstrap 绕过票据；终态后重新启动 OwnerBound 主机需要新的 Runtime 和 bootstrap，旧票据不能为新 server instance 续权。readiness 前的票据请求只能在原请求预算内等到端点就绪后发行。

`control.close()`、销毁 control 或控制器退出结束所有者关系；原生观察到终态后进入 `OwnerMissing`，暂停主机准入并开始配置的 grace。`control.request_shutdown(timeout=...)` 则由当前有效 owner 直接请求原生排空，成功只证明 shutdown initiate；它不等待业务或进程退出。普通直连 IPC admin shutdown 对 OwnerBound 返回负 ACK，包括握手前和重复 initiate，不能用作接管或绕过 owner 撤销。Persistent 的 admin 行为和可信进程内 `Runtime.shutdown()` / `cc.shutdown()` 保持原语义。

`OwnerMissing` 期间已发布路由继续保持 `Ready`、原 route UID/revision 和 server instance，catalog/relay 不撤回登记；`connect()` 仍可能成功，调用通过既有 `ResourceUnavailable` 被拒绝，Python thread-local 调用也使用同一原生 gate。已准入工作与 held/borrowed lease 按原规则继续；尚未最终准入的旧 pending 请求不能睡过暂停/恢复后取得新许可。接管只恢复主机 gate，不重新登记路由，也不重开显式关闭或注销的路由；迟到的 relay final 仍须通过原登记 token 的身份和准入代次检查。帧已发送时沿用原 transport phase/重试规则，不能仅凭 `ResourceUnavailable` 推断 pre-dispatch 或自动重放业务。

接管必须在 `OwnerMissing` 且单调时钟 `now < 原 grace deadline` 时提交；达到 deadline 后即使 timer 尚未调度也会拒绝并进入原生排空。错票据、半包、竞争失败或 inspect 都不延长该期限。成功提交原子消费旧 grant、安装 successor grant、提升 owner epoch 并转移监督通道；同票据竞争最多一个赢家，旧票据、旧 epoch 的命令、EOF 和 timer 不再有权影响新 owner。`WatcherIoError`、`Draining` 和 `Finished` 不可通过接管恢复。

仅当前有效 session/epoch 可调用 `control.recovery_ticket(timeout=...)` 发行票据，或用 `control.revoke_recovery(timeout=...)` 幂等撤销当前 grant。重复取票返回同一当前 grant，撤销后再次发行才生成新 secret。`OwnerRecoveryTicket` 是 repr 脱敏、不隐式 pickle 的能力；显式 `ticket.export()` 导出包含 secret 的字节，Rust 用 `OwnerRecoveryTicket::import(&bytes)`、Python 用 `cc.OwnerRecoveryTicket.from_bytes(data)` 导入。应用负责将票据保存到控制器之外的可靠存储并保护这些字节，C-Two 不代建 secret 文件或后台恢复器。

恢复者先按既有方式配置 endpoint，再调用 `cc.prepare_owner_takeover(ticket)`；Rust 使用 `Runtime::prepare_owner_takeover(ticket)`。prepare 会冻结当前 Runtime 的 endpoint 配置并捕获原生目录身份，即使准备失败也不能改 root；它不启动主机或从票据采用目录。票据绑定逻辑地址、server instance、owner epoch 和原生 endpoint context 身份；同一目录的路径别名按原生 identity 验证，不以路径字符串替代。错 root/instance 或目录替换会被拒绝，inspect/commit 继续核验原身份；不自动切换默认 root。恢复票据与端点凭据都不是采用旧目录或替换目录的指令。

按 prepare、可选的 `prepared.inspect(timeout=...)`、保存 `prepared.successor_ticket.export()`、一次 `prepared.commit(timeout=...)` 的顺序接入。inspect 仅观察当前 identity/epoch/phase，不是授权或本次事务成功收据；commit 会重新验证并在成功时返回新的 `OwnerControl`。Python 的 `successor_ticket`、`recovery_ticket` 是属性，Rust 使用同名方法；successor 在服务端原子安装前无效，准备对象在任何结果后仍可导出两份票据。下面的 `save_ticket(name, data)` 与 `load_ticket(name)` 由应用实现，键名只示意一次恢复；原票据与 successor 必须分开保留，提交前的保存必须完成且能在控制器退出后读取。后续尝试使用独立记录，不覆盖仍可能有效的已保存票据。

```python
# 原控制器：子进程已 Ready，control 来自 cc.owner_control_pair()。
save_ticket("current", control.recovery_ticket(timeout=1.0).export())
```

```python
import c_two as cc

# 恢复控制器：已完成与原主机一致的 endpoint 配置。
ticket = cc.OwnerRecoveryTicket.from_bytes(load_ticket("current"))
prepared = cc.prepare_owner_takeover(ticket)
status = prepared.inspect(timeout=1.0)  # 仅观察，不能据此认定接管成功。
save_ticket("successor", prepared.successor_ticket.export())
control = prepared.commit(timeout=1.0)  # 每个准备对象只提交一次；保留返回的 control。
```

这些控制方法失败时抛出 `cc.OwnerControlError`；Python 的 `error.kind` / `error.cause_kind` 对应 Rust 的 `kind()` / `cause_kind()`。`NotSent`（`not_sent`）说明变更请求尚未开始发送；`Indeterminate`（`indeterminate`）说明发送已开始但没有有效确认或明确拒绝，候选会话会关闭，不能推断成功或未执行。完整的拒绝仍给出确定分类，例如 `owner_present`、`invalid_grant`、`grace_expired`、`identity_mismatch`、`shutting_down`。上例直接传播错误，每个准备对象无论结果都只允许调用一次 commit，NotSent 后重试也须重新 prepare；应用必须保留两份票据及对应的 epoch 信息。在不确定结果后，可用 inspect 观察当前 epoch/phase，再由应用显式选择已保存的原票据或 successor 创建新的准备对象；原 grant 未消费才仍有效，若已消费则只有已授权的 successor 有效，竞争者获胜或撤销也可能令自己的票据失效。不得自动依次尝试 old/new key，也不得从 epoch 变化认定自己的事务获胜。

每次控制调用使用一个原生绝对预算，每个 session 只有一个在途请求；等待串行槽超时只结束该 waiter，不能关闭其他调用的 owner session；取得槽后、首次写入前超时也保留健康会话。开始写入后丢失确认会终止该专用会话，迟到应答不能交给下一请求。候选 setup 上限为原生 5 秒与剩余原 grace deadline 的较早者，拆包或重试不重置它；不新增配置项。owner 控制独立使用 version 1、最多 4 KiB 的消息，复用既有帧封装；IPC 握手 12、SHM 3、C/FFI ABI 5 和业务帧、调用/回复编码保持不变。Rust 穷举匹配须增加 `MsgType::OwnerControl`、`SchedulerAcquireError::Unavailable` 和 `RouteConcurrencyError::Unavailable` 分支。旧单向 owner 类型、格式和别名已移除，无兼容入口；不向普通 admin、HTTP 或 relay 降级。

`OwnedChild.poll()` / `wait(timeout=...)` / `kill()` 观察或管理进程，`OwnedChild.close()` 只释放观察者，原生共享 reaper 继续负责 OS 回收。它与 `OwnerControl.close()` 的生命周期作用不同。

`cc.shutdown(timeout=...)` 返回原生结构化完成结果；有效 owner shutdown 或 Persistent admin shutdown 的 ACK 仅证明已接受发起排空，不能用作 callback 排空完成或运行 hook 的依据。`completed=false` 时保留原 Session、bridge、路由及关闭 hook，稍后观察同一事务。监听器关闭、工作排空和 payload lease 是独立事实；端点回收不释放 held/borrowed，FastDB 检查视图由各自 lease 释放时失效。

Persistent 的直连 IPC admin shutdown 只在尚未尝试写入控制请求时重试暂时连接失败。开始写入后，写入失败、ACK 丢失或超时均返回现有未确认结果，不再重连重发；未确认不证明服务端没有执行。这样同地址重新绑定的新实例不会收到本次调用的重发。ping 继续保留探测重试，首次观察到端点不存在的处理与原总预算保持。

运行器在服务就绪后调用 cc.inspect_endpoint(address)，保存 present 结果中的 EndpointCredential。present 只表示对象被观察，不证明进程存活。WouldBlock 是协调争用，不能视为死亡。子进程正常退出或 kill()+wait() 确认退出后，再用 cc.reap_endpoint(address, credential) 精确收尾；旧凭据不能删除新 incarnation。

需要续扫时使用 cc.sweep_endpoints(addresses=[本次运行的逻辑地址], max_entries=64, max_ms=10) 的 context manager 和 next_batch()；只在原生报告 round_complete 时记为本轮完成，中断或命名空间变化单独处理。c3 endpoint inspect/reap/sweep 调用同一原生机制，sweep 可重复传 --address，需要非默认 root 时传 --ipc-root（或环境变量 C2_IPC_ROOT）。运行器不拼 OS 路径，不直接 unlink。

Relay 发现跨 root 的行为：独立 relay 在启动时解析并冻结自己的本地上游域上下文，一个 relay 本地域对应一个 root，上游资源与它使用一致配置；不同 root 可运行不同 relay。客户端在发现请求中携带 Core 派生的本地 namespace 标识（不含原始路径，不是授权凭据）。`/_resolve` 响应的 `x-c2-local-namespace` 头是整批路由的唯一 namespace 来源，JSON 路由不再附加该字段。Rust/Core 客户端只在 anchor 为本机、响应头与本地上下文匹配且握手身份、契约与代际检查均通过时接受 IPC。缺头或不匹配时使用 HTTP，不推定默认 root，也不从旧 JSON 字段恢复 namespace；重复或非法响应头仍拒绝。HTTP 路由不会因 namespace 不匹配被撤销，HTTP-only 调用不要求本地端点能力。Windows 标识由 backend 与当前 logon scope 构成，实际权限仍由 OS DACL、握手身份与完整路由契约检查。

原生路由获取查询服务端当前权威状态，随后固定 route UID/revision 和 server instance。数据连接 idle 驱逐保留资源登记与 owner；旧代理不能借同名替换取得新资源。生成的 TypeScript 持久直连客户端也使用该控制查询和身份固定规则。

## 绑定与连接失败

从 0.8.1 起，启动期限通过 `CoreError.lifecycle_kind='startup_timeout'`、`startup_stage` 和 `timeout_seconds` 识别；本地 bind 失败的消息不再加错误的 `config error` 前缀，原 OS 字段保持。注册前可用 `cc.set_server(startup_timeout=...)` 设置等待预算；有限默认值、冻结与收尾边界见[启动期限说明](configuration.md#服务启动期限)。

公开的 `c_two.error.CoreError` 提供 `io_operation`（`bind` 或 `connect`）、`io_kind`（`permission_denied`、`not_found`、`connection_refused`、`address_in_use`、`other`）及 `raw_os_error`（原始 errno/Win32 码，无系统码时为 `None`）。这些字段描述实际本地操作；配置、握手、合同及期限失败没有本地原因，三个字段均为 `None`。Windows PIPE_BUSY 等待沿用原连接预算，期限耗尽不冒充 OS 错误；C-Two listener lease 冲突可以是 `address_in_use` 且没有 Win32 码。

Rust Core 与 SDK 错误通过 `local_io_cause() -> Option<LocalIoCause>` 暴露原因，`operation()` 和 `error()` 分别提供操作及已有 `EndpointIoError`。原 readiness watch 传递 `ServerFailure { message, bind_error, startup_timeout }`，替换 `ServerLifecycleState::Failed` 与 `LifecycleError::Server` 的字符串负载。低层穷尽匹配需要包含 `IpcError::Connect`、`ServerError::Bind` / `Failed` / `StartupTimeout`；Core 通过 `LifecycleError::StartupTimeout` 暴露启动期限。Rust 调用者需要重新编译，没有旧负载或兼容入口。ABI 与协议版本不变。

目录上下文保留原始 OS kind/code，外层目录策略仍按原规则拒绝。可读消息、错误顺序、重试分类和期限含义保持原样。原因字段不能证明进程死亡，也不授予启动、回退或清理权限；控制者仍须遵循已有监听所有权与严格身份回收机制。端点维护 API 沿用现有 ErrorKind 文本格式。

## 公开的 Python 结构化错误

直接捕获 `c_two.error.CoreError`。它继承 `CCBaseError` 和 `RuntimeError`；`CCError` 继续表示 canonical 语义错误。处理两类错误时捕获 `CCBaseError`；只允许语义错误穿过调用阶段包装时捕获 `CCError`。旧 `_native.CoreError` 导出已删除，不提供顶层别名或包装异常。

```python
from c_two.error import CoreError

try:
    resource = cc.connect(Grid, name="grid", address="ipc://worker")
except CoreError as exc:
    if exc.io_operation == "connect" and exc.io_kind == "permission_denied":
        raise PermissionError(exc.message) from exc
    raise
```

所有实例都具备类中声明的字段；可选值缺省为 `None`，`details` 为独立字典。`str(exc)` 与 `args[0]` 保留外层原始诊断；Python 复制和 pickle 往返保留字段，不改变 portable 错误协议。全部字段及取值范围见[字段表](local-endpoint-lifecycle.en.md#public-structured-python-errors)。

`category` 为 `semantic / contract / admission / transport / lifecycle`；`transport_kind` 为 `ipc / http / None`；`transport_phase` 为 `str / None`：Core 传输错误产生 `pre_dispatch / dispatch_uncertain`，分别表示业务发送前、不能确定对端是否执行；语义错误原样保留对端任意诊断字符串，不代表可信传输分类。`io_kind` 为 `permission_denied / not_found / connection_refused / address_in_use / other / None`；`raw_os_error` 仅保留真实 errno/Win32 码，无原始系统码的策略拒绝及非 I/O 失败为 `None`。这些字段不自行授权重试或清理。

`lifecycle_kind`、启动阶段/秒数、HTTP 状态、登记失败来源及 rollback/relay cleanup 结果继续来自原 Core 错误链；结构化字段并不重新执行事务。普通导入仍不加载 FastDB 或 NumPy。

## 观察 IPC 连接结束

在连接客户端或启动监听之前调用 `cc.connection_events()`，订阅本 Runtime 的受管理 outgoing IPC 连接以及服务端接受的连接；Rust 对应 `Runtime::connection_events()`。同一事件流通过 `event.side` 的 `client` / `server` 区分双方。订阅不启动主机、创建身份或冻结配置，预使用阶段的合法配置变更保留既有订阅。

Python `events.recv(timeout=None)` 返回 `ConnectionEvent`：`kind='opened'` 的 `value` 是 `ConnectionObservation`，`kind='closed'` 的 `value` 是 `ConnectionClosed`。两者都有 `connection_id` 和 `side`；客户端 ID 与 `cc.connection_observation(proxy).connection_id` 一致。opened 只证明 OS 物理连接建立或接受，不证明握手、合同或业务就绪。多代理可共享同一连接，逻辑代理 close 不单独产生物理关闭；重连分配新 ID。

```python
events = cc.connection_events()  # 在连接之前订阅
try:
    peers = [cc.connect(Grid, name='grid', address=address) for address in addresses]
    for event in events:
        if event.side == 'client' and event.kind == 'closed':
            print(event.connection_id, event.value.reason, event.value.operation)
finally:
    events.close()
```

`recv()` 等待时释放 GIL，超时返回 `None`，来源或订阅结束时抛 `StopIteration`。其他线程可以调用幂等的 `events.close()` 唤醒阻塞接收；它丢弃该订阅排队的元数据，不关闭物理连接或其他订阅。与 close 并发的接收可能已取得一条事件，之后的接收均结束。一个订阅同一时刻只允许一个消费者，并发接收抛 `RuntimeError`。一个消费者可观察多个连接，无须为每条连接各阻塞一个线程。HTTP-only、同进程调用不产生 outgoing IPC 事件，独立的原始 admin/owner 控制连接不被插入 Runtime 客户端缓存。

事件不回放历史，晚订阅可能只收到旧连接此后的关闭。队列首次订阅时分配，固定容量 1024；溢出抛 `ConnectionEventsLagged`，`missed` 表示遗漏数，并永久结束该订阅。Rust 的异步 `recv(&self)` 在结束时返回 `Ok(None)`，溢出为 `ConnectionEventsError::Lagged`，并发接收为 `ConcurrentReceive`。事件和订阅仅保留元数据，不保活 Runtime、transport、resource 或 payload lease。

`cc.connection_observation(proxy)` 保留当前物理连接的只读元数据；`snapshot()` 立即返回终态或 `None`，thread-local 和 HTTP 代理拒绝该查询。Python 的 `ConnectionObservation.wait_closed()` 和专用 Core 同步桥 `wait_connection_closed()` 已退役，等待统一走事件流，迟到查询使用 snapshot。底层 Rust 异步 `ConnectionObservation::wait_closed(Option<Duration>)` 仍用于原生关闭屏障和可取消的单连接 future，与汇总流读取同一终态权威。

原因仅为 `local_close`、`peer_close_message`、`peer_eof`、`connection_reset`、`heartbeat_timeout`、`protocol_error`、`io_error`。只有收到 DISCONNECT 才报告 `peer_close_message`；EOF/reset 不证明对端进程死亡，心跳超时也不证明死亡。两端的原因可以不同，首个终态保持不变。通知复用 Linux/macOS UDS 和 Windows Named Pipe 的接收事件及现有心跳，不依赖在途调用；通知先于工作排空和内存释放。仍须通过原生 shutdown/close 完成结果确认收尾，通过 OS wait 确认进程退出，再按凭据精确 reap。通知不会释放 Held，也不会把普通断连变成 OwnerBound 所有者丢失。

对象替换、未知端点、损坏记录与部分初始化报告 unverified，不能凭年龄、PID 或连接失败删除。同一通信组使用相同构建及对应的最终目录配置；路径调整直接替换旧布局，旧凭据和旧进程端点不做自动迁移。升级不清扫旧目录中的历史数据。Windows 端点回收报告内核管理或 not-applicable。初始登记任意位置崩溃后的自动恢复不在保证范围内。平台验证范围见对应的验收记录。

Rust 示例见 [owned_child](../sdk/rust/examples/owned_child.rs)，历史版本行为见 [0.7.4 指南](releases/0.7.4.zh-CN.md)。[早期原生端点验收](reports/canonical-local-endpoint-validation.md)与[端点验收](reports/local-endpoint-final-validation.md)保留各自的历史源码和范围。
