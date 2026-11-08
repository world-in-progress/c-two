# 路由名是资源状态的唯一身份

状态：已采用（2026-11-06），实施中（[执行计划](../design/2026-11-08-route-identity-plan.md) 2026-11-08 批准）。

路由名加契约身份，标识一份资源的状态；契约身份代表它的性质。一次注册是这份资源在某个 Host 上的一段服务期，route UID 只在 C-Two 内部区分不同的注册，用于注册、替换与撤下时的核对，不再出现在客户端看得到的接口和调用标识里。名字的范围：经 relay 连接时是整个 relay mesh，直接 IPC 时是那个端点。前提是两次注册之间，资源的状态和性质没有被 C-Two 之外的操作改变；跨注册的状态延续由 Host 负责，Host 若要让客户端知道这已是另一份状态，就换一个路由名。

代理跟随路由名。`cc.connect` 返回的代理认的是路由名加契约身份，不是某一次注册。服务端与 relay 按路由名和契约身份接纳调用，不再因为调用带的 route UID 较旧而拒绝。代理留在服务它当前那次注册的端点或 relay 上；那次注册不在了，代理在发出下一次调用之前按名字重新解析。闲置回收、进程重启或换一台机器之后的重新注册，因此对调用方透明。调用仍然只发一次：注册结束时正在进行的调用照旧报完成或派发不确定，从不重放；名字下没有在线注册时，调用在派发前失败，可以安全重试。经 relay 的代理跟随到另一台机器上的注册后，路径可能在 relay-local 与 HTTP 之间改变。

relay mesh 内的唯一性，不靠共识：

- 一个 relay 内，同一个名字同时最多一次在线注册（现行规则）。
- relay 按它所知的对端路由，拒绝注册已由存活对端持有的名字，报 `ResourceAlreadyRegistered`。
- 对端 relay 失联或被判死时，它持有的名字仍然保留：不参与解析，也不能在别处注册，直到那个 relay 回来后自己撤下，或者运维方明确释放。联系不上不等于已经没了。
- 分区期间允许注册新名字。分区愈合、relay 探测到对端恢复并交换路由摘要之后，同一个名字若在多个 relay 上都有在线注册，就处于路由冲突：新的按名字获取在派发前失败，报路由冲突错误，细节列出各次注册所在的 relay 与注册时间；已有代理继续用它当前那次注册，冲突期间不跨到别的 relay 上的注册；relay 的路由列表和日志标出冲突。C-Two 不裁决，也不停止任何一方；部署方注销其中一份之后冲突自动解除，被注销那份的代理随即跟随留下的那份。
- 解析不再按“先本地、再按 (registered_at, relay_id)”在同名注册之间挑选。
- relay 不落盘，分区期间重启的 relay 记不得另一侧持有的名字；这种情形在愈合后按路由冲突报出，不另加机制。

边界：C-Two 负责按名字解析与唯一性检查，没有中层也能跨机器按名字连接、跟随重新注册。名字的分配与治理属于中层或应用（例如 Toodle）：谁能用哪个名字、避免撞名的约定、资源是否公开给别人发现、资源目录、信任与访问控制。副本也属于中层，C-Two 没有副本组，relay mesh 原先设想的第二、三阶段（多副本一致性）移出 C-Two 的待办。跨 relay 的自动接管需要一个 lease 权威、Host 失去持有权时自行停止接收调用、以及有界的时钟走速，C-Two 不提供；可接入的解析器或权威等真实需要出现再加。本决定不恢复 [ADR 0002](0002-remove-owner-subsystem.md) 删掉的所有者子系统。

信任模型：现在的 relay 由同一部署方运行，按崩溃故障考虑，对端消息不经认证。这是本决定的前提，不是永久性质；将来若由不同各方接入计算资源，需要准入权威，可能还需要拜占庭容错，届时另行设计。

随之改变：

- IPC 16 的调用标识与 C ABI 8 的 prepare 去掉 route UID；两者都还没发布，不必再升版本。经 relay 的调用与探测也去掉 route UID，这是客户端与 relay 之间的不兼容改动，客户端与 c3 relay 要一起升级，发行说明写明。mesh 协议是否要升版本（例如明确释放要不要经对端消息传播）在实施计划里定。
- 客户端不再会遇到 `RouteStale`（704），它从规范错误表退役；relay 内部按 route UID 的核对改用内部错误，不再映射到公开错误码。新增路由冲突错误码；从没产生过的 `WriteConflict`（706）同批删除。
- 代理不再暴露 route UID（Rust `observed_route()`、Python `route_uid`）。
- 流式块 9b7b 加上的按 route UID 排除候选的逻辑（HTTP 与 relay-local 两处）不再需要，一并删除。
- 实施在流式批次之后、任何包含 IPC 16 的版本发布之前；流式批次把 D5（旧的 HTTP 代理可能一次错误也收不到就进了新的注册）记为已知问题。

术语（已写入 CONTEXT.md）：

- **路由 / route**：A route name together with the contract identity it serves; it names one resource's state. Within its scope (the relay mesh, or one endpoint for direct IPC) at most one registration of it is live at a time. _Avoid_: binding, endpoint
- **注册 / registration**：One period in which a Host serves a route, from `cc.register` until the route is unregistered or the Host stops. A route may have many registrations over time; the route UID tells them apart inside C-Two and is not shown to clients. _Avoid_: instance, activation, incarnation
- **路由冲突 / route conflict**：The state in which more than one relay holds a live registration of the same route after a partition. New acquisitions fail until only one registration is left; C-Two never chooses between them. _Avoid_: duplicate route, split brain

考虑过而没有采用：

- 旧代理先收到一次 `RouteStale` 再跟随：每次重新注册都给每个旧代理一次错误，各条路径仍是两套规则。
- 旧代理一直报错，直到重新 connect：把重连与替换代理对象的负担推给每个下游应用。
- 维持现状，经共享的解析缓存跟随：报不报错取决于缓存时效和同一进程里的其他代码。
- relay 之间跑 Raft，或接入 etcd：relay 要持久化日志，至少三个才能容忍一个故障，分区时少数派一侧不能注册。
- 指定一个负责注册的 relay：它宕机时，所有注册与重新注册都停下。
- 分区愈合后自动留一个、停掉另一个（Erlang global 的缺省做法）：会悄悄丢掉一侧分区期间的改动。
- 只要还有对端联系不上，就拒绝所有新名字：一个宕机的 relay 在被释放之前，会挡住整个 mesh 的新注册。
- 把注册与解析整个交给中层：没有中层时 C-Two 就不能跨机器按名字连接，跟随也要依赖外部的解析器。

依据：[单一持有者调研](../research/2026-11-06-single-owner-naming-survey.md)、[契约与路由](../reference/contracts-and-routing.md)、[ADR 0002](0002-remove-owner-subsystem.md)、[ADR 0003](0003-server-streams.md)。ADR 0003 不自动重发的两条理由里，“过期路由通常意味着新实例没有原来的状态”由本决定取代，“要把输入保留到回复”仍然成立；ADR 0003 为此加了补记。

补记（2026-11-08）：维护者按推荐批准了[执行计划](../design/2026-11-08-route-identity-plan.md)，留给计划的事与读代码时发现的几处如下定下。上文的“运维方明确释放”做成强制离开（force-leave）：部署方对任一个在线的 relay 执行一次，经对端消息传遍网格，每个联系不上那个 relay 的 relay 都忘掉它与它名下的路由；术语表里“释放”指租约、分配或 hold 的结束，“保留”是 hold 的动词，所以上文“名字仍然保留”指名字仍归失联的 relay，“运维方”与“部署方”同指部署方（deployer）。mesh 协议不升版本：mesh 7 与 IPC 16 一样还没有发布。透明有一处边界：代理在把一次调用的输入交给传输之前就知道注册已不在（连接对端结束、同进程的注册已撤下、上一次调用在派发前被告知路由不在或连不上目标）时，先按名字重新获取，调用方看不到变化；只能从这次调用得知时（端点或 relay 仍在线，名字已换到别处），这次调用在派发前失败、可以安全重试，下一次调用才跟过去，因为输入不为重发保留。同一个端点或 relay 上的重新注册按名字接纳，不报错。两个 relay 同时接受同一个新名字时，同样按路由冲突报出。Host 崩溃之后，名字留在它的 relay 上：C-Two 不凭联系不上判断 Host 已退出，部署方在同一个 relay 上重新注册，或用 `/_admin/unregister` 在那个 relay 上撤下它。路由冲突的错误码是 719 `RouteConflict`；名字只归失联的 relay 时，解析报 `ResourceUnavailable`（702），在别处注册报 `ResourceAlreadyRegistered`（703）；704、706 与 711 不再分配给别的含义。Host 一侧的注册结果也不再给出 route UID。
