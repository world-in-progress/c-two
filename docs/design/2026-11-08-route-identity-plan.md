# 路由身份的执行计划

状态：维护者 2026-11-08 按推荐批准，包括第二节的决定 A 至 F 与第三节的用词；实施中。

本计划实施 [ADR 0004](../decisions/0004-route-name-identity.md)，即[待办](backlog.md)“接下来”的第 1 项。依据是 ADR 0004、[单一持有者调研](../research/2026-11-06-single-owner-naming-survey.md)、[流式第二、三阶段验收记录](../acceptance/2026-11-08-streaming-acceptance.md)里的已知问题 D5，以及[闲置驱逐审查](../acceptance/2026-10-10-idle-eviction-regression-audit.md)的六道门禁。这一批要在任何包含 IPC 16 的版本发布之前合入 main；main 从 `a5ab5eaa`（world-in-progress/c-two#219）起已经包含 IPC 16。整合分支 `socu/route-identity` 从上游 main 的 `fd33e140` 切出，它与 `a5ab5eaa` 只差 world-in-progress/c-two#220 的文档。第一节的现状来自对 `a5ab5eaa` 源码的静态阅读，没有运行验证；第七节列出留给各块核实的地方。

## 一、现状

route UID 现在是调用标识的一部分。IPC 16 的 OPEN 带七项身份，其中一项是 `route_uid`（`core/protocol/c2-wire/src/rpc.rs:57`）；服务端发现它与当前注册不同，就以 `RouteStale`（704）拒绝（`core/transport/c2-server/src/server.rs:3472`）。路由查询 RouteLookup 带调用方观察到的 UID，可能答 `Stale`（`core/protocol/c2-wire/src/route_catalog_control.rs:80`、`:104`）。客户端在获取时绑定一个 route token（`core/transport/c2-ipc/src/client.rs:621`、`:2861`），relay-local 获取绑定 relay 公布的 UID（`core/runtime/c2-core/src/client.rs:931`）。C ABI 8 的 prepare 接受一个可选的期望 route token，结果里返回 `routeUid`（`core/foundation/c2-mem-ffi/include/c2_mem_ffi.h:102`）。

代理在连接时就固定下来。Core 的 `Client` 持有一个传输：一条池化的 IPC 连接加路由绑定，或者一个 relay-aware 的 HTTP 客户端；它从不重新获取，`observed_path` 与 `ObservedRoute` 也在连接时定下（`core/runtime/c2-core/src/client.rs:46`、`:89`）。Python 的同进程代理直接持有资源对象和它的调度器（`sdk/python/src/c_two/transport/client/proxy.py:79`）。IPC 代理与同进程代理在连接结束或注册被替换之后一直失败，直到应用重新 connect；HTTP 代理收到一次 `RouteStale` 之后，下一次调用重新解析。

经 relay 的 HTTP 调用每次都经 Runtime 共享的解析缓存（30 秒过期，`core/transport/c2-http/src/client/control.rs:37`）重新排序候选，并把缓存里当前的 route token 放进 `x-c2-route-uid` 头（`core/transport/c2-http/src/client/relay_aware.rs:576`；relay 一侧见 `core/transport/c2-http/src/relay/router.rs:53`、`:590`），relay 以 409 `RouteStale` 拒绝过期的 token。D5 就出在这个共享缓存上。流式块 9b7b 按 (relay URL, route UID) 排除候选（`relay_aware.rs:806`）、relay-local 获取在 `RouteStale` 时不排除端点（`relay_aware.rs:1011`），以及生成的 TypeScript 传输按 route token 排除本地候选（`core/foundation/c2-codegen/assets/typescript_transport.ts:834`），都只因 route token 而存在。

relay 网格允许同名注册并存。注册时只检查本 relay 的本地路由（`core/transport/c2-http/src/relay/authority.rs:409`），对端公布一个本 relay 也持有的名字照样接受（`authority.rs:879`）；对端只接受由所属 relay 自己公布的路由（`authority.rs:954`）。解析先返回本地路由，再按 (registered_at, relay_id) 返回存活对端的路由（`core/transport/c2-http/src/relay/route_table.rs:316`）。按缺省设置，对端 15 秒没有心跳记为 Suspect、不参与解析，30 秒记为 Dead 并删除它的全部路由（`core/transport/c2-http/src/relay/background.rs:140`、`:173`）。对端消息与全量同步都带 route_uid（`core/transport/c2-http/src/relay/peer.rs:22`、`:70`，`core/transport/c2-http/src/relay/types.rs:224`），mesh 协议是 7（`peer.rs:7`）。

relay 联系不上本地 Host 时不撤下它的注册：upstream control 只在 Host 答复路由不存在、已撤下、已关闭、契约不符，或者同一地址换成了另一个服务器身份时撤下，传输错误不撤下任何东西（`core/transport/c2-http/src/relay/upstream_control.rs:90`）。同一个 relay 上同名的新注册替换旧持有者时，连不上旧端点就当作它已经退出（`authority.rs:967`、`:1006`）。

错误码方面，704 由服务端、relay 与 c2-ipc 产生；706 `WriteConflict` 与 711 `RouteCatalogCompacted` 没有生产者。Python 为这三个码各有错误类、`ERROR_Code` 成员、映射与 `__all__` 条目。Host 一侧，Rust SDK 导出的 `Registration` 经 `outcome().route_uid` 给出 route UID（`core/runtime/c2-core/src/outcome.rs:6`）；Python 的注册结果里也有它，但 `register_crm` 没有交给调用方。

## 二、需要维护者决定的事

A 与 B 是 ADR 0004 留给本计划的；C 至 F 是读代码时发现、ADR 没有写到或者写得不够确定的地方。每一项都给出建议。

### A. 怎样明确放手失联 relay 的名字

ADR 0004 规定，失联或被判死的 relay 名下的名字不参与解析、也不能在别处注册，直到那个 relay 回来后自己撤下，或者部署方明确放手。建议做成“强制离开 / force-leave”：部署方对网格里任一个在线的 relay 执行 `c3 registry force-leave <relay-id> --relay <url>`，它调用新的管理端点 `POST /_admin/force-leave`。收到请求的 relay 只在目标 relay 在它看来不是 Alive（Suspect 或 Dead）时接受，对 Alive 的 relay 和对它自己都拒绝。接受之后，它忘掉目标 relay：删除它的对端记录、它名下的路由与墓碑，这些名字从此可以在别处注册与解析；同时用新的对端消息 `RelayForceLeave` 通知其他存活的对端，每个对端按自己看到的状态同样处理，自己仍看到目标 relay 存活的对端（例如处在分区的另一侧）不处理。被强制离开的 relay 回来之后照常加入，把它仍在线的注册当作新的公布；期间同名的注册已经出现在别处的，按路由冲突报出。

强制离开按 relay 而不按名字：放手的依据是“这个 relay 联系不上”，对它名下每个名字都一样，按名字再设一种操作没有不同的用途。不经对端消息传播、要部署方逐个调用每个 relay 的做法不建议：漏掉的那个 relay 会继续拒绝这些名字的注册。现有规则只接受所属 relay 自己公布的路由，被忘掉的 relay 的路由应当不会经还没处理这条消息的对端的反熵回来，块 5 用测试确认。

### B. mesh 协议要不要升版本

建议不升，仍是 mesh 7。mesh 7 还没有发布过：它由 `516e3cec`（2026-11-01）引入，2026-10-30 的 `c3-v0.6.0` 标签里还是 mesh 6，[未发布的发行说明](../releases/unreleased.md)第 125 行把它列为下次发布的变化。这与 IPC 16 的情形相同，而 ADR 0004 已经定下 IPC 16 不升。这一批对 mesh 7 的改动是：RouteAnnounce、DigestDiff 的 Active 与全量同步的路由去掉 route_uid，摘要哈希随之改变；增加 `RelayForceLeave`；Dead 对端的路由不再删除，这一条是行为，不改消息。从 world-in-progress/c-two#215（2026-11-03）合入到这一批之间用 main 构建的 c3 relay 说的是旧形状的 mesh 7，不能与这一批之后的构建组成网格；这些构建都没有发布过。发行说明照 IPC 16 的写法，要求客户端与 c3 relay 一起升级。

### C. “对调用方透明”的边界

ADR 0004 写着闲置回收、进程重启或换一台机器之后的重新注册对调用方透明，同时调用只发一次。实现时，代理可能在调用之前就知道那次注册已经不在，也可能只能从调用本身得知。

如果它在把一次调用的输入交给传输之前就已经知道，例如 IPC 连接已经对端结束、同进程代理所指的注册已经撤下、上一次调用在当前目标那里于派发前被告知路由不在（没有这个名字、已撤下、已关闭、契约不符），或者上一次调用连不上那个目标，代理就先按名字重新获取，再发这次调用，调用方看不到变化。

如果它只能从这次调用得知，也就是它的端点或 relay 仍然在线，但那里已经没有这个名字，注册换到了别的端点或 relay，那么这次调用的输入已经交给了传输，而 C-Two 不为重发保留输入（[ADR 0003](../decisions/0003-server-streams.md) 仍然成立的那条理由），所以这次调用在派发前失败、可以安全重试，下一次调用才跟过去。

同一个端点或同一个 relay 上的重新注册按名字接纳，无论哪种情形都不报错；注册换到另一个 relay 时，HTTP 代理在旧 relay 上会有一次这样的失败。建议接受这个边界，并在 ADR 0004 加一条日期补记写明。另一个做法是把输入保留到目标给出派发前的结论，再在同一次调用里改发；ADR 0003 因为要把输入一直保留到回复而没有采用。

### D. Host 崩溃之后名字留在它的 relay 上

按 ADR 0004 的唯一性，只要一个 relay 还记着某个名字的注册，别的 relay 就拒绝注册这个名字。Host 崩溃之后，它的 relay 联系不上它，但传输错误不撤下任何东西，所以名字一直留在这个 relay 上：在同一个 relay 重新注册仍然可以，因为替换时连不上旧端点就当作它已经退出；换一台机器、经另一个 relay 注册，会报 `ResourceAlreadyRegistered`，直到原来的 relay 重启，或者部署方用已有的 `POST /_admin/unregister` 在那个 relay 上撤下它。

自动撤下要先证明 Host 已经退出，而 C-Two 有意不做这种判断：[本地端点生命周期](../reference/local-endpoint-lifecycle.md)规定年龄、PID 与连接失败都不能作为删除的依据，`reap` 也要由监护进程先确认子进程退出。替换时把连不上当作退出，是因为那时有一个已经核实的新持有者在等，判断错了也只是这个 relay 改用新的持有者；只凭联系不上就撤下、把名字放给整个网格，没有这样的依据。建议这一批不加机制，在[契约与路由](../reference/contracts-and-routing.md)和 [c3 的说明](../../cli/README.md)里写明这条恢复途径。`/_admin/unregister` 因此有了明确的用途，这与待决定的 F7（有没有人手动使用它）有关；如果希望部署方不必手写 HTTP 请求，可以在块 6 给 c3 加一个对应的命令。

### E. 新错误码与失联时报什么

路由冲突用新的 719 `RouteConflict`，HTTP 状态 409，属于派发前；细节写明路由名，以及各次注册所在的 relay ID、relay URL 与注册时间。704 退役之后不再分配给别的含义，706 与 711 同样，因为一个编号的含义不应随写入的时间而变；`c2-error` 在注册表旁记下这三个退役的编号。

名字只由失联的 relay 持有时，解析报 `ResourceUnavailable`（702），细节写明那个 relay 与它的对端状态，而不报 `ResourceNotFound`：联系不上不等于已经没了。在别处注册这样的名字报 `ResourceAlreadyRegistered`（703），细节相同。

`ResourceRemoved`（708）在端点上保持原义：这个端点曾有这个名字的注册，已经撤下，现在没有在线的注册。它与 `ResourceClosed` 的细节不再带 route UID。

### F. Host 一侧的 route UID

ADR 0004 列出的是代理上的 route UID（Rust `observed_route()`、Python `route_uid`）。Rust SDK 还经 `Registration::outcome().route_uid` 把它交给 Host。Host 用不到它：向 relay 登记与撤下由 Core 完成。建议从 Rust 的注册结果与 Python 的注册结果投影里一并删掉，route UID 只留在 Core、c2-server 与 relay 内部；测试改用 Host 看得到的事实或测试专用的接缝区分两次注册。

## 三、用词

ADR 0004 的几处用词与[术语表](../../CONTEXT.md)冲突或重复。下面的建议随计划一起批准，已写进 ADR 0004 的日期补记与术语表；术语表里路由冲突的定义同时改为按路由名，并包括两个 relay 同时接受同一个新名字的情形。

- ADR 写“运维方明确释放”。术语表里“释放 / release”指租约、分配或 hold 的结束。建议这个操作叫“强制离开 / force-leave”，与 relay 主动离开网格的 `RelayLeave` 对应。建议的条目：**强制离开 / force-leave**: The deployer's declaration that an unreachable relay has left the mesh. Every relay forgets that relay and its routes, and its route names can be registered elsewhere. _Avoid_: release (for this operation), evict.
- ADR 写“它持有的名字仍然保留”。术语表把“保留”定为 hold 在中文里的动词。建议这种情形写作“名字仍归失联的 relay”，不另立术语。
- ADR 里的“运维方”与“部署方”指同一方。建议统一为“部署方 / deployer”。建议的条目：**部署方 / deployer**: The people or systems that run a C-Two deployment's relays and Hosts. They choose route names, resolve route conflicts and force unreachable relays to leave. _Avoid_: operator, 运维方.

## 四、执行计划

块只是排序：每一块合入整合分支时都要达到可发布的质量，不留临时协议、别名或兼容路径。进度只记在待办。

共同要求：

- 每一块在自己的检出里，从整合分支的最新提交开始实施。参考文档、CONTEXT.md、AGENTS.md、ADR 补记与发行说明在合入时更新，实施方在报告里列出需要改的地方。
- Linux、macOS 与 Windows 同等对待。
- 先写或改聚焦测试。块改变了的行为，钉着旧行为的测试随块改写，其余现有测试不动。删除的测试逐条归类：机制已删、别处已覆盖、仍然有效（改写成新测试）。测试的要求引用 ADR 与参考文档的原句，不转述。
- 0.x 规则：一次切干净，不留别名与兼容路径；校验只在边界做一次；不为没发生过的情形设防。复制、释放、移动的顺序不变；直接 IPC 在 relay 未设或不可用时仍完整可用。每一处删掉的检查都要对得上 ADR 0004 或本计划的某一条，对不上的保留。
- 默认忽略的新子进程测试登记到 `tools/dev/test_local.py` 的 `RUST_IGNORED`。
- 构建产物随用随清，所有 cargo 命令共用 `CARGO_TARGET_DIR=<检出>/core/target`。

### 块 1：IPC 调用标识与 ABI 8 的 prepare 去掉 route UID

范围：c2-wire 的 OPEN 身份改为六项（`route_name`、`crm_ns`、`crm_name`、`crm_ver`、`abi_hash`、`signature_hash`）；握手的路由表去掉 route_uid；RouteLookup 去掉 `observed_route_uid` 与 `Stale` 应答，`Removed`、`Closed` 不再带 route UID。路由目录的记录（`RouteRecordWire`）保留 route_uid，只供 relay 在注册时核对。c2-server 按路由名与契约身份接纳调用，dispatcher 记下撤下过的名字，不再记名字到 UID 的映射。c2-ipc 的调用绑定不再带 UID，删除 `acquire_route_token`、`bind_cached_route_token` 与 `IpcError::RouteStale`。c2-core 的 relay-local 获取改为按名字获取，照旧核对服务器身份；删除 `ObservedRoute` 与 `Client::observed_route()`，Python 代理的 `route_uid` 与 Rust SDK 导出的 `ObservedRoute` 随之删除，注册结果去掉 route_uid（决定 F）。relay 向上游转发时改为按名字获取；它为注册、替换与撤下保留的 UID 核对不变。c2-mem-ffi 的 prepare 去掉期望的 route token 参数，结果 JSON 去掉 `routeUid`；C 头文件、TypeScript 绑定与生成的 TypeScript 传输里调用 prepare 的地方随之修改。加回块 9b8b 去掉、留到这一批的 relay-local 首次获取用例：同名重新注册之后，第一次按名字获取应当在 relay-local 上拿到新的注册，不退回 HTTP，也不报 `FallbackDenied`（当时它报 `FallbackDenied`，原因是 `RouteStale`）。

检查：c2-wire、c2-server、c2-ipc、c2-core、c2-http 与 c2-mem-ffi 的测试，Rust SDK 测试，Python 全量，Node 绑定与 TypeScript 代码生成的测试，可移植矩阵。OPEN、握手、RouteLookup 与 prepare 的编解码用字节级测试确认没有 route UID。直接 IPC 与 relay-local 各跑一遍真实进程：路由在同一个服务器实例上撤下再注册之后，旧代理与新的获取都按名字到达新的注册；中间没有在线注册时，调用在派发前失败，资源一次也没有被调用。同一个服务器实例经 relay 重新注册之后，relay 的本地记录换成新的注册，迟到的、针对上一次注册的撤下不会撤掉新的注册。relay 未设与不可用时直接 IPC 照常。

退出条件：OPEN 的身份是六项；IPC 与 ABI 上再没有 route UID，IPC 一侧不再产生 704；同一端点上的重新注册对旧代理不报错；relay 的注册、替换与撤下核对照旧；relay-local 首次获取用例加回并通过；改写的测试已归类；全平台 CI 通过。

### 块 2：relay 的 HTTP 调用与探测去掉 route UID，退役 704，删除 706 与 711

范围：relay 不再读写 `x-c2-route-uid`（一元调用、服务端流与 `/_probe`），删除 409 `RouteStale` 应答；`/_resolve` 与 `/_probe` 的结果去掉 route_uid。relay 为一次调用获取上游时，发现本地注册已被替换，就用当前的注册；内部的 UID 比较只把两次注册的连接槽分开，不到达调用方。relay-aware 客户端的调用不带 route token，删除 `route_is_stale` 与按 (relay URL, route UID) 排除候选的逻辑；relay-local 获取失败时按端点排除，不再有 `RouteStale` 的例外；`FallbackDenied` 照旧保留第一个 IPC 失败的类型化原因。生成的 TypeScript 传输同样去掉 `x-c2-route-uid`、按 route token 的排除与对 `RouteStale` 的处理。c2-error 删除 704、706 与 711，记下这三个编号不再分配；Python 删除对应的错误类、`ERROR_Code` 成员、映射与 `__all__` 条目，Rust 与 TypeScript 的映射随之删除。

检查：c2-http、c2-core、Rust SDK、Python 全量、TypeScript 代码生成与 Node 绑定的测试。用真实 relay 重跑 D5 的场景，改成正向断言：另一个获取刷新了共享缓存之后，旧的 HTTP 代理进入同一个 relay 上的新注册，调用方看不到 route UID。在 HTTP 请求、响应、头与错误细节里确认没有 route UID。产品代码里查不到 `RouteStale`、`WriteConflict`、`RouteCatalogCompacted` 与它们的编号，只剩退役编号的记录；`RouteWatchUnavailable`（712）不变。

退出条件：客户端与 relay 之间不再交换 route UID；704、706、711 从注册表与三种 SDK 中删除并记为不再分配；同一 relay 上的重新注册对旧的 HTTP 代理不报错；改写的测试已归类；全平台 CI 通过。

### 块 3：代理跟随路由名：Rust Core、Python 与 Rust SDK

范围：Core 的 `Client` 保存期望的契约、连接方式与当前目标，不再在连接时固定传输。按决定 C，它在把一次调用的输入交给传输之前，若已经知道当前注册不在了，就按原来的连接方式重新获取：直接 IPC 回到同一个地址，接受那里新的服务器实例；经 relay 的连接（relay-aware 与显式 relay）在网格里重新解析，路径可能在 relay-local 与 HTTP 之间改变，relay-local 照旧核对 relay 给出的服务器身份。触发重新获取的是路由事实（没有这个名字、已撤下、已关闭、契约不符）与连接层面的失败（对端结束、连不上），容量与截止时间的拒绝不触发。重新获取算这次调用的派发前阶段，受这次调用的截止时间约束；同一个代理上同时等待的调用共用一次重新获取；已经发出的调用照旧报完成或派发不确定，从不重放。经 relay 的 HTTP 代理停在当前的 relay 上，不再每次调用经共享缓存重新排序候选；共享缓存只为新的获取服务，代理重新获取时让它失效。`observed_path()` 报当前路径，`connection_observation()` 报当前的物理连接，同一个代理经 `with_call_options` 得到的视图共用同一个当前目标。Python 的同进程代理发现它的注册已撤下时，在这次调用进入资源之前按 `cc.connect` 的顺序重新解析（先本进程，再 relay）；参数还在手里，所以在同一次调用里完成。远程代理由 Core 跟随；同一个名字之后出现在本进程里时，远程代理不改走同进程调用。`cli/README.md` 里“一次路由观察，至多一次派发前刷新”的说法随之改写。

检查：Core 与 Rust SDK 的单元测试和门禁测试，Python 全量与门禁。用真实进程跑：同一个服务器实例重新注册，不报错；Host 进程重启，直接 IPC 与 relay-local 的下一次调用在对端结束之后先重新获取，发出中的调用报派发不确定；Host 换到另一个 relay，旧的 HTTP 代理那一次调用在派发前失败，下一次跟过去，relay-local 与 HTTP 之间能切换；同进程撤下再注册，同一次调用到达新的资源对象；八个调用同时触发重新获取，只建立一次连接；截止时间限制重新获取。对照下游 hey-my-buddy 用到的 `connection_observation`、连接事件与 `cc.close`，写明行为有没有变化。

退出条件：同一端点或 relay 上的重新注册对代理不可见；代理在已知注册结束时于下一次调用之前跟随，只在调用中得知时那次调用在派发前失败、下一次跟随；没有任何重放；第五节门禁 2 与门禁 5 的新表述在 Python 与 Rust SDK 上通过；全平台 CI 通过。

执行记录（2026-11-08）：确认计划时定了两点：显式 relay 的连接保持只走 HTTP，重新获取时从原来的 relay URL 解析整个网格；契约不符的拒绝只结束这一次调用或获取，下一次调用照样用期望的契约重新获取。验收中又定了几点：HTTP 目标只记 relay URL，不记注册的 `max_payload_size`，因为同一个 relay 上的重新注册可以改它，relay 按当前注册检查请求长度，客户端读响应只受自己的载荷缓冲预算与协议的报文上限约束；IPC 只在连接因对端关闭或重置而结束时重新获取，协议错误结束的连接不重新获取；端点上下文显式给出的期望身份由 Core 在每次获取时核对，生成的 TypeScript 在 relay-local 下用 relay 解析结果固定身份，旧上下文在对端换了实例后报 `IdentityMismatch`，TypeScript 层的跟随在块 4；Python 本进程代理在注册还在时照原来直接调用，只在原生准入报告路由已关闭之后才建立调用预算并跟随。同样的构建下，本进程调用从 3.61 µs 变为 3.76 µs，块 7 的配对基准一并核对。

### 块 4：代理跟随路由名：生成的 TypeScript 客户端

范围：Node 经 ABI 8 使用 Core 的客户端，IPC 上的跟随来自块 3；ABI 8 的端点上下文现在固定服务器身份，直接 IPC 要接受同一地址上的新服务器实例时，c2-mem-ffi 与 C 头文件随之修改。生成的 TypeScript 传输补上 relay-aware 的部分，规则同块 3：停在当前目标，已知注册结束时于下一次调用之前重新解析，relay-local 与 HTTP 之间可以切换。不改动 TypeScript 运行时的结构，拆包另由待办第 6 项做。

检查：TypeScript 代码生成与 Node 绑定的测试，Python 里调用生成的 Node 客户端的集成测试与门禁测试；块 3 的真实进程场景在生成的 Node 客户端上跑一遍。

退出条件：三种客户端的跟随行为一致；全平台 CI 通过。

执行记录（2026-11-08）：生成的 TypeScript 的三种传输各为每个（路由名，契约）保存一个当前目标，由同一个传输的客户端共用；只在已知目标不在了时，于下一次调用之前按调用的预算重新获取，同时等待的调用共用一次。relay-local 的目标经 relay 重新获取，新的端点上下文期望 relay 报出的身份；被替换的端点停止接纳，等它的调用结束后关闭。原来只为每次调用解析服务的路由缓存没有读者，连同 `maxAttempts`、`routeCacheTtlMs` 与按注册 `maxPayloadSize` 的请求预检一起删除。ABI 8 补了两个原生投影：按路由只读的连接快照（不做 I/O），以及 prepare 的按次时长（与显式的 `connectTimeoutMs` 取较小者）。验收中发现，直接 IPC 的调用在 prepare 之后、派发之前遇到 Host 重启时，Core 重新获取并由新实例执行，观察却仍报旧实例；改为由 Core 的原调用记录在把输入交给传输时写入实际派发目标（身份放在获取目标时建立的 `Arc` 里），`dispatch` 的结果带回它，观察据此报告每次调用自己的目标。

### 块 5：强制离开

范围：决定 A 的管理端点 `POST /_admin/force-leave`、对端消息 `RelayForceLeave` 与它的处理，`c3 registry force-leave`，以及[契约与路由](../reference/contracts-and-routing.md)与 `cli/README.md` 的说明。在块 6 之前，它忘掉的是 Suspect 对端仍留着的路由与对端记录；块 6 之后，失联 relay 的名字都要靠它或那个 relay 回来才能放手。

检查：route_table 与 authority 的单元测试。用三个真实的 c3 relay，缩短心跳间隔：停掉一个，等它变成 Suspect 或 Dead，对剩下任一个执行强制离开，另一个也忘掉它；对 Alive 的 relay 与对自己的请求被拒绝；被强制离开的 relay 回来之后照常加入并公布；还没处理这条消息的对端的反熵不会把它的路由带回来。

退出条件：部署方对任一个 relay 的一次操作让整个网格忘掉一个失联的 relay；拒绝条件与回来之后的行为有测试；文档写明；全平台 CI 通过。

执行记录（2026-11-08）：检查改用进程内 relay 的测试配置缩短心跳与探测，因为 c3 没有暴露这些时限，不为测试加配置项。验收时发现，被强制离开而又没有种子的 relay 在分区愈合后回不来：别的 relay 已经忘掉它，不再探测它；它的死对端探测恢复后只发摘要，对方不认识它而忽略。改为死对端探测恢复时先发 `RelayJoin`、再交换摘要，返回的全量同步不合并；普通的分区愈合也走这条路。另外，各 relay 发现失联的时刻不同，声明到达时仍把它看作 Alive 的 relay 不处理，部署方在那里再执行一次；退出条件里的“整个网格忘掉”以各 relay 已看到它失联为前提。

### 块 6：relay 网格里名字唯一

范围：注册时除了本地，还检查对端：名字已由存活的对端持有，或者仍归一个失联的 relay，都报 `ResourceAlreadyRegistered`，细节写明持有的 relay 与它的状态。失联（Suspect 或 Dead）relay 名下的路由不再删除，不参与解析；名字只由它们持有时，解析报 `ResourceUnavailable`。解析至多返回一次注册，删除按 (registered_at, relay_id) 排序的逻辑。同一个名字在多个 relay 上都有在线注册时（分区愈合之后，或者两个 relay 同时接受了同一个新名字），新的获取在派发前报 719 `RouteConflict`；已有的代理按块 3、块 4 的规则停在它当前的 relay 上，不跨到别的注册。`/_routes`（`c3 registry list-routes`）标出冲突的注册与仍归失联 relay 的路由，relay 在发现冲突与冲突解除时各记一条日志。对端消息与全量同步去掉 route_uid（决定 B）。c2-error 新增 719，Python 加 `RouteConflict` 类、`ERROR_Code` 成员与映射，Rust 与 TypeScript 加映射。按决定 D，在参考文档与 `cli/README.md` 写明 Host 崩溃之后名字怎样回来。顺带改掉 `sdk/python/src/c_two/error.py` 里重复注册错误把 route name 写成 resource name 的 docstring（待办第 5 项）。

检查：route_table、authority、background 与对端处理的单元测试。用真实的 c3 relay：在 A 注册之后，在 B 注册同名被拒绝；停掉 A，B 仍然拒绝、解析报 702，A 回来并由 Host 注销之后 B 可以注册；两个 relay 先各自独立启动、各注册同一个名字、再连成网格，两边新的获取都报 `RouteConflict`，细节列出两次注册，列表标出冲突，已有代理继续调用自己那一份，注销其中一份之后冲突解除，那一份的代理跟到留下的一份；Host 进程被杀之后，用 `/_admin/unregister` 在原 relay 上撤下，名字随即可以在另一个 relay 注册，并确认这次撤下传到了对端。这一块的差异另请一位审查者独立核对（见第六节）。

退出条件：ADR 0004 唯一性一节的每一条规则都有真实 relay 的测试；719 在三种 SDK 里可以按类型识别；全平台 CI 通过。

执行记录（2026-11-08）：确认计划时定了删除 Runtime 范围的共享解析缓存：每次新获取都要看到 relay 当前的结论才能如实报告 719 与 702，先失效再读就让缓存没有读者，删掉后流式批次的已知问题 D5 所依赖的机制也随之消失。703 的持有者细节原来到不了 Python：注册失败收据只留文本与状态，现在保留规范错误，Python 与 Rust 都能读到 `registrations`。独立审查发现：relay 处理主动离开时分两次写锁删除路由与对端记录，中间到达的旧公布会留下没有所属对端的路由，新的投影随之 panic、名字被一直挡住；改为在同一个写锁里先删路由再删对端记录。冲突比较原先在每次取写锁时扫全表，改为只在路由增删或对端存活状态变化时比较受影响的名字。实测确认 `/_admin/unregister` 会把撤下传到对端。relay 把上游获取时的 410、503、409 投影补上了 `dispatch_phase=pre_dispatch`。

### 块 7：交叉验收

范围：按第五节的表述，在 Python、Rust SDK 与生成的 Node 客户端上补齐六道门禁与两道新门禁，覆盖持久直连、relay-local 与 HTTP；直接 IPC 与 relay 矩阵和可移植矩阵；配对基准；参考文档、CONTEXT.md、AGENTS.md 的退役表、ADR 0004 的补记、发行说明与验收记录。

检查：配对基准（`ipc-memory-paired.yml`）以这一批的起点 `fd33e140` 为基线（代码与 `a5ab5eaa` 相同），门槛沿用[流式设计](2026-11-03-streaming-rpc-design.md)第十二节第 7 条：一元调用的中位延迟、p95 或吞吐退化超过 5% 且超出噪声，先定位，再交维护者裁决。确切候选在 Linux、macOS、Windows x64 与 ARM64 上的全平台 CI。

退出条件：就是这一批的完成标准：各块都已验收；确切候选的全平台 CI 通过；验收记录与 `docs/releases/unreleased.md` 写好；维护者发话之后开上游 PR。

执行记录（2026-11-09）：确认补测计划时定了证据的标准：在测试进程里用自己的 Runtime、经真实 IPC 端点提供服务的 Host 就是门禁说的真实 IPC 服务器，只有门禁的表述本身关乎进程的地方（真实的流断开与对端结束、Host 重启与发往旧进程的调用）才要独立的 Host 进程，已有测试因此不迁到子进程。门禁 4 原文的“token 过期”读作 B 在同一个端点上以同名重新注册：A 在途的调用与载体不受影响，B 的下一次调用到达新的注册。门禁 5 的“control watch 中断后恢复”由 Host 重启覆盖，Host 在线时只断控制连接没有单独测。全平台 CI 发现 relay 测试先探测空闲端口、放开之后再在它上面启动 relay，与并行测试争端口；改为 relay 先绑定监听地址并报出实际地址（`33ee933b`），c3 的 `--bind` 用端口 0 因此可用。补测与 CI 修正的验收见[验收记录](../acceptance/2026-11-09-route-identity-acceptance.md)。

### 顺序与并行

关键路径是块 1 → 2 → 3 → 4 → 6 → 7。块 4 在块 3 之后，因为 Node 在 IPC 上的跟随来自块 3 的 Core 客户端。块 5 在块 2 之后即可开始，与块 3、块 4 同时进行：块 5 只写 relay 的对端处理、管理端点与 c3，块 3 写 c2-core、c2-http 的客户端、Python 与 Rust SDK，块 4 写 c2-mem-ffi 与 TypeScript 代码生成，彼此不碰同一批文件。块 6 在块 4、5 之后：它与块 5 改同样的 relay 文件，它的冲突规则要靠块 3、4 让已有代理停在当前的 relay 上。这样排，每一块合入时都是一个完整的状态：块 2 之后同一端点与 relay 上的重新注册已经透明，块 3、4 之后代理跨端点与 relay 跟随，块 5 之后失联的 relay 可以放手，块 6 才让失联 relay 的名字一直归它。

顺带项：

- 待办第 3 项 relay 小节里“relay-aware 调用没有每次调用的路由回执，`observed_route` 仍是连接时的值”，随块 2、3 解决，届时从待办删掉。
- 待办第 5 项里 `error.py` 的 docstring，归块 6。
- `cli/README.md` 里客户端重新解析次数的说法，归块 3。
- AGENTS.md 的退役表加一行，归块 7：route token（`x-c2-route-uid`、prepare 的 `routeUid`、`ObservedRoute`、`observed_route()`、Python `route_uid`）、704 `RouteStale`、706、711 与按 route UID 排除候选，取代者是代理跟随路由名与 719 `RouteConflict`。

## 五、门禁的新表述

[闲置驱逐审查](../acceptance/2026-10-10-idle-eviction-regression-audit.md)是冻结的记录，它的门禁 2 与门禁 5 写着旧代理不接受新的注册。这一批之后，这两道门禁以下面的表述为准，其余四道不变；验收记录照录。

门禁 2：路由经过就绪、关闭、撤下与同名重新注册之后，新的获取与已有代理都到达当前的注册；没有在线注册时，调用在派发前失败，不进入任何资源；已经派发给旧注册的调用报它真实的终态或派发不确定，不重放；调用方看不到 route UID。

门禁 5：同名的 Host 重启、control watch 中断后恢复、旧候选与新持有者并发时，关于旧注册的证据不能撤下或关闭新的持有者；已有代理在下一次调用时跟到新的注册；发往旧进程、还没有结束的调用报完成或派发不确定。

新门禁 7（路由冲突）：两个 relay 在分区愈合后各有同一个名字的在线注册时，新的获取报 `RouteConflict` 并列出两次注册；已有代理继续用自己那一份；注销其中一份之后冲突解除，那一份的代理跟到留下的一份。

新门禁 8（失联 relay）：失联 relay 的名字不参与解析（报 702），也不能在别处注册（报 703），直到它回来后自己撤下，或者部署方对它强制离开。

## 六、验收、CI 与收尾

每一块交付之后独立验收：读完差异，在整合分支加上这份差异的候选上运行受影响的测试与共用的下游测试。块 1、2、3、5、6 改到协议、relay 或 Core 的客户端，在候选上跑一次完整检查，包括 Rust core 与 Rust SDK、原生测试、Python 全量、可移植矩阵、Node 绑定与 TypeScript 代码生成的测试，以及 `tests/repo`；块 4 跑 Node 与 TypeScript 相关的全部测试。会让产品出错、丢数据、不安全的缺陷，以及让完整检查不过的缺陷，退回原来的实施继续修；其余范围之内的缺陷记为下一块的顺带项。验收通过之后合入整合分支，再在 fork 上跑一次全平台 CI；并行的块各自验收、依次合入，每合入一块跑一次。

块 6 的差异另请一位审查者独立核对，双方互不看对方的结论；它的发现只算线索，复现之后才用。这一批只请这一次。

块 7 写验收记录与 `docs/releases/unreleased.md` 的一节。发行说明要写明：这是破坏性的改动，客户端与 c3 relay 一起升级；代理跟随路由名，以及决定 C 的边界；OPEN 与 ABI 8 prepare 的变化；704、706、711 退役，新增 719；代理与注册结果不再给出 route UID；relay 网格里名字唯一，失联 relay 的名字与强制离开；Host 崩溃之后名字怎样回来。推送上游与开 PR 等维护者发话。

## 七、留给各块核实的地方

本计划只读过源码，下面几处没有运行验证，归到对应的块：同一个服务器实例经 relay 重新注册时，relay 的本地记录是否换成新的注册（块 1）；relay 为调用获取上游时遇到替换，改用当前注册是否会让两次注册的连接槽混用（块 2）；把 `Client` 的目标改成可替换之后每次调用多出的开销，由块 7 的配对基准衡量（块 3）；ABI 8 的端点上下文重连到同一地址上的新服务器实例时怎样处理固定的身份（块 4）；被强制离开的 relay 的路由会不会经别的对端的反熵或全量同步回来（块 5）；`/_admin/unregister` 是否把撤下传到对端（块 6）。
