# 资源归属的发布/订阅

状态：已采用（2026-11-20），拟定 0.12.0 实施。

## 决定

C-Two 新增与一元调用（unary call）和服务端流（server stream）正交的第三种连接原语：**资源归属的发布/订阅（Publish/Subscribe）**，用于将一个有状态资源的输出以 1-to-N 方式广播给多个本地与远端订阅者，消除多订阅者场景下服务端的 `R×` 重复序列化/共享内存写入与跨机 `R×` 重复网络传输。

### 1. 契约与编程面

- **契约保留 `c-two.contract.v2`：** 在描述符根对象新增可选的 `"topics"` 数组；无主题的契约在规范 JSON、ABI 投影与 signature 投影中均省略 `"topics"`（显式空数组被拒绝），因此既有 unary 与 server stream 契约的 `descriptor_sha256`、`abi_hash` 与 `signature_hash` 逐字节不变，旧解析器按未知根键拒绝含主题的契约。
- 每个主题声明 `name`、`output`（固定为 `{"kind": "payload"}`）与 `binding`（FastDB 载荷规范），0 输入、无 `access` 读写锁类别。可移植契约的主题输出只接受 FastDB 载荷；Python 私有 pickle 主题沿用 Python 私有描述符路径并拒绝可移植导出。主题按名字升序规范化，禁止重复名及与方法同名，计入契约准入上限，允许仅含主题的契约。
- **发布句柄绑定单次注册：** 资源类保持普通领域对象。注册路由返回 `Registration`（Python `Runtime.register()` 与 Rust 对齐，改为返回带 `.name` 与 `.publisher(topic)` 的 `Registration`，由 Runtime 保持注册存活，不因返回值丢弃而自动注销）。发布句柄绑定该次注册身份与序号空间：注册关闭后句柄失效并报 `ResourceClosed`（707），绝不写入后续同名新注册，也不阻止 Host 回收。要求路由一开放即可读的初态，作为注册事务参数在开放前以同一发布逻辑提交，失败一并回滚。
- **订阅交付：** 每次迭代返回不可变的 `Delivery(value, seq, dropped)`。显式 `cc.hold` 投影为 `Delivery[HeldResult[T]]`，由 `.value` 承担释放责任；关闭订阅不使已交付值失效。同进程直连对可变 Python 对象在 `publish()` 时取不可变快照，防止发布者后续修改或单个读者释放影响其他读者。

### 2. 同机 1-to-N 共享内存（维持 ADR 0001 与 SHM 4）

- 不采用跨进程无锁覆写环或超时回收。SHM 4 线格式不变，Host 维持 [ADR 0001](0001-single-owner-release.md) 单一属主释放。
- Host 在 `publish()` 时只编码并写入一次不可变封存的共享内存块（buddy 或 dedicated，按真实 backing 计入一次 `server.M`）；小帧、空帧、SHM 不可达或停用后业务由同一封存值走 inline/chunk，不重复执行业务序列化。
- 每个已授条目需求且上一笔 SHM 借出已结算的本地订阅者，在其各自连接的借出表上获得独立 `loan_id` 指向同一封存块，经既有 `PeerMappingCache` 只读映射、复制到订阅者自己的 `client` 域本地副本后立即发 `RELEASE`。
- 写前撤销释放未暴露引用；`begin_io` 之后，每条连接的借出仅由该连接的合法 `RELEASE` 或读侧 `peer end` 结算，绝不因连接状态对象析构提前回收。当帧退出历史环且全部已发布借出结算后，Host 才回收该块。
- Host 主题历史不隐式溢写磁盘（`F`）。无法为新帧取得预算内存储时，`publish()` 立即以 `CallCapacityExceeded`（717）失败，保留上一帧且不消耗 `seq`。

### 3. 历史环、起点与落后策略

- 每次注册的每个主题维护独立单调递增的 `u64` 序号（从 `1` 开始）与有界历史环 `capacity`（默认 `1`），注册开放期间始终保留最新一次成功发布。**`publish()` 绝不阻塞在任何慢订阅者上。**
- 订阅声明两个选项：
  - 起点 `start`：`latest`（默认）或 `next`。在源注册接纳的原子时刻读取尾序号 `h`：`latest` 取预期首号 `n = max(h, 1)`，`next` 取 `n = h + 1`。加入前的历史不计 `dropped`。附着成功后先发小控制记录 `READY(start_seq)`，空主题亦可确认订阅生效。
  - 落后策略 `on_lag`：`skip`（默认）或 `error`。当订阅具备需求且选下一帧时，若 `n` 仍在环内则按序交付；若 `n` 已淘汰，`skip` 跳至最新可得帧 `s` 并由接收方按 `(预期号, s)` 计出 `dropped = s - n`，`error` 在发后继帧前以 `SubscriptionLagged`（720）终结该订阅。已开始发送的帧（`ITEM_BEGIN` 之后）绝不中途替换。

### 4. 线协议（IPC 17）、并发与 `(M, F, N)` 边界

- 协议由 IPC 16 干净切至 **IPC 17**（保留 25 字节固定版本前导与 SHM 4；移除 IPC 16 兼容）。复用 `OPEN / ITEM_BEGIN / ITEM_DATA / CREDIT / CANCEL / CALL_END / RELEASE` 调用记录族，新增 `CallMode::Subscribe` 与 `READY` 确认记录；Subscribe `OPEN` 携带完整路由契约身份、有界 `topic` 名与 `start`/`on_lag`（握手元数据不增 topic 表）；订阅的 `ITEM_BEGIN` 附源 `seq`。
- 订阅不占用路由并发许可（`read_parallel`/`exclusive`）或执行工作线程，也不计入普通方法 `max_pending`，但纳入原注册的排空屏障（`wait_routes_drained` 同步等活跃订阅在选定帧排空后终结）。
- 订阅全生命周期占用所属角色域业务槽 `N`（不新增槽位配置项）。内部准入规则：`N ≥ 2` 时长期订阅最多占 `N - 1` 槽，至少保留 1 槽给普通控制调用；`N = 1` 容许 1 个总操作；`N = 0` 全拒。relay 共享上游作为已有下游转发的共同工作，最后一个下游的槽保留至上游清理完毕，保证 `N = 1` 仍可建立首订阅。

### 5. `c3 relay` 两级单上行扇出（保留 mesh 7）

- `c3 relay` 新增 `TopicHubTable` 与专用订阅 HTTP 入口（`POST /{route_name}/_topics/{topic_name}`，带版本化媒体类型与 `x-c2-call-token` 取消通道），支持两级单上行扇出：
  - **源 relay（A，持有本地路由）：** 对同一活跃注册与主题只向 Host 开 **1 条上游 IPC 订阅**，按可独立释放的分片组合成单份 `Arc<HubFrame>`（按 1 份计入 `relay` 域 `M`/`F`），扇出给全部下游 HTTP 订阅者。
  - **锚点 relay（B，路由在远端 A）：** 客户端在 `relay-aware` 模式下经本地锚点 B 订阅远端主题，B 向源 A 只开 **1 条跨机 HTTP 上游订阅**（携带单跳防环头 `x-c2-relay-origin-only: 1`，A 只对本地路由接纳，绝不二次转发），再在 B 本机经 loopback HTTP 扇出给 B 上的 `R` 个客户端，消除跨机链路 `R×` 带宽放大。B 暂不增设本地 `c2-server` IPC 服务端。
- **不破坏 ADR 0004 解析：** Hub 命中绝不替代按名解析；新获取仍先检路由冲突（719）与失联（702）。并发首订阅合并为一次上行建连；最后一个下游离开才撤上行；旧上游迟到的终态只清理对应 hub 实例。
- **跨 relay 起点与注册校验：** 迟加入 B 上已有活跃 hub 时，由一次合并后的轻量源端确认（不传载荷）核实上游仍服务当前注册并取得源尾序号，确保跨机 `start=latest/next` 满足因果一致。
- **慢下游隔离：** Hub 上游以内部 `skip` 驱动、保留源 `seq`，各下游游标独立执行 `skip`/`error`。预算受压时依次淘汰可弃历史、跳过未开始帧、单点终结仍卡住旧分片回收的停滞 TCP 下游（维持记录边界，绝不拼接半帧或阻塞快读者）。
- **relay mesh 保持版本 7：** 主题已包含在契约身份（`abi_hash`/`signature_hash`）中，无需改 gossip/full-sync 格式。

### 6. 错误码、终态与跨语言范围

- 规范错误表仅新增一个错误码：**`720 SubscriptionLagged`**（携带 `expected_seq`、`available_first_seq`、`available_last_seq`、`dropped`）。未知主题在已匹配契约上沿用 `713 ProtocolViolation`（严禁报 `701`，避免触发路由重获取）；容量不足沿用 `717 CallCapacityExceeded`；生命周期终止与主动取消沿用 `718 CallCancelled` 并写明原因。`704`、`706`、`711` 保持退役。
- **单次订阅绝不跨越两次注册：** 注销、Host 停机或 relay 停机时原子封住发布、新订阅与选下一帧，已选定的当前帧完成交付后以 `718 CallCancelled` 结束（标明 `route_unregistered`/`host_shutdown`/`relay_shutdown`）；代理下次 `subscribe()` 按 ADR 0004 自动跟随当前注册。
- 0.12.0 覆盖 Rust Core、`c3 relay`、`c2-codegen`、Python SDK、Rust SDK 与 Node 客户端（C ABI 8 → **ABI 9**，由 Core 统一执行 IPC/relay-local/经锚点 HTTP 订阅）。

## 考虑过而没有采用

- **跨进程无锁覆写环（seqlock / 定时复用槽位）：** 违背 ADR 0001；读写并发覆写对 Rust/FastDB 造成未定义行为，超时复用无法撤销对端映射。
- **跨进程只读零拷贝订阅视图：** 会把 Host `server.M` 的回收交由最慢 Python/SDK 对象或无法撤销的 NumPy 别名决定，破坏角色域隔离。
- **把主题塞入 `methods` 作为第三种 `MethodMode`：** 迫使 Host 与生成器为无输入、无读写锁、资源主动推送的主题伪造 `access` 与方法回调。
- **资源对象持 `Publisher` 字段由 Host 隐式绑定：** 动态 fork 复制资源易串写，注册前预算归属模糊，Rust `Arc<Resource>` 需要额外 setter/内部可变性，注销重绑易误写新注册。
- **给 `c3 relay` 增加本地 `c2-server` IPC 服务端：** 引入第二套端点命名空间、SHM 池、身份与关停路径；跨机主要瓶颈在网口，先用两级 HTTP 扇出实测后再议。
- **按最慢下游推进 relay 上游 `CREDIT`：** 一个慢远端消费者会拖垮同一主题的所有快订阅者。

依据：[ADR 0001](0001-single-owner-release.md)、[ADR 0003](0003-server-streams.md)、[ADR 0004](0004-route-name-identity.md)。
