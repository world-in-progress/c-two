# 派发后调用结局查询与去重：一手来源调研

日期：2026-11-23  
用途：为 [ADR 0006](../decisions/0006-post-dispatch-outcome-and-dedup.md) 提供业界一手机制对照与取舍依据。

## 1. 背景与问题定义

C-Two 面向有状态、非幂等的科学计算资源 RPC。现有设计保证了：
- 派发前拒收（`NotExecuted` / `pre_dispatch`）可以安全重试；
- 一旦进入派发不确定窗口（`dispatch_uncertain`，如调用端超时、对端中途断连、relay/TCP 半断），C-Two 绝不静默重试，也绝不把不确定结果伪装为未执行。

但在真实长跑仿真与自动化编排中，调用方在收到 `dispatch_uncertain` 后仍面临两个现实缺口：
1. **查证（Query）：** 刚才那次写调用究竟还在执行、已经成功、已经失败，还是根本没到达当前注册？
2. **安全重放与附着（Dedup / Recover）：** 若调用仍在执行，能否附着等待结果而不触发第二次执行？若已执行完毕，能否取回结果（或确认结果已成功）而不重跑非幂等逻辑？

调研聚焦三类经过大规模生产验证的一手方案，并检验哪些前提适用于 C-Two 的单次注册内存模型。

## 2. 一手来源对照

### 2.1 Oracle Transaction Guard（`LTXID` / `GET_LTXID_OUTCOME`）

一手来源：Oracle Database *Development Guide — Using Transaction Guard*、`DBMS_APP_CONT.GET_LTXID_OUTCOME` 与 *Oracle Real Application Clusters Administration and Deployment Guide*。

- **核心机制：**
  1. **逻辑事务标识（`LTXID`）先于提交绑定：** 客户端/驱动在事务边界取得或携带唯一 `LTXID`，将其随提交原子写入数据库持久化的 `LTXID_HISTORY` 表。
  2. **独立于原连接的只读结局查证：** 通信中断后，调用方在新连接上调用 `DBMS_APP_CONT.GET_LTXID_OUTCOME(client_ltxid, committed, call_completed)`，返回两个正交布尔事实：事务是否已提交（`committed`）以及提交后的用户调用是否已完整返回（`call_completed`）。
  3. **强栅栏阻止迟到旧包（Fencing）：** 查证本身会把该 `LTXID` 在服务端置为**已封禁（blocked / fenced）**——若最初那个失联的旧提交包在网络延迟后才姗姗来迟，服务端检测到该 `LTXID` 已被查证封禁，直接拒绝提交，从根上关闭“查完说没提交 → 重试成功 → 迟到旧包再提交一次”的二次执行窗口。
  4. **保留期限有界：** `COMMIT_OUTCOME_RETENTION` 控制 `LTXID_HISTORY` 保留时长（默认 24 小时），超过保留期后无法再证明未提交，只能返回不确定/不可用。
- **对 C-Two 的启示：**
  - 把“查证结果”和“重新执行”拆开是防止迟到旧包二次执行的关键洞见。
  - 若不做持久 `LTXID` 表、又不引入预申请票据（+1 RTT），内存表一旦发生容量淘汰或 Host 重启，就无法像数据库那样用单次查证永久封禁未见过的 `call_id`。因此 C-Two 必须诚实区分 **`unknown`（当前注册未知，绝不等于“未执行”）** 与 **`not_found`**，且恢复接口遇 Miss **绝不代执行**。

### 2.2 IETF `draft-ietf-httpapi-idempotency-key-header` 与 Stripe Idempotency

一手来源：IETF HTTPAPI Working Group *The `Idempotency-Key` HTTP Request Header Field*（`draft-ietf-httpapi-idempotency-key-header`）与 Stripe API 文档 *Idempotent requests*。

- **核心机制：**
  1. **显式调用方键 + 请求指纹校验：** 客户端按操作生成高熵键（如 UUIDv4），随请求头发送。服务端把键与请求指纹（方法 + 完整输入摘要）绑定；同一键携带不同方法或不同载荷时返回 **`422 Unprocessable Content`**（或 `409`）拒绝，防止误用旧键静默重放不相关结果。
  2. **进行中冲突策略：** 并发重复提交命中正在执行的键时，IETF 草案允许返回 **`409 Conflict`**（`A request is outstanding for this Idempotency-Key`）要求客户端退避重查，也允许实现选择挂起附着等待终态。
  3. **错误响应缓存边界：** Stripe 明确区分**校验/派发前错误**（如参数错误，未触达业务变更，不锁定/不永久缓存或允许修正）与**执行期错误（包括 5xx）**——一旦进入业务状态机，服务端会固化并重放原终态，避免不确定副作用被重跑。
  4. **有限保留与重放语义：** Stripe 按 24 小时自动清理记录；清理后复用同一键会按新请求执行。
- **对 C-Two 的启示：**
  - **请求指纹（`method_idx + SHA-256(input)`）是硬前提**，否则调用方复用键发错参数会拿到牛头不对马嘴的旧结果。
  - 科学计算长写调用往往耗时数秒到数十分钟：直接返 409 迫使客户端轮询不如**提供“非阻塞 `query_call` + 附着等待的 `recover_call`/重复 `submit`”**，既能立即查态，又能免轮询地等待正在跑完的写调用。
  - HTTP 路径必须与普通 2 段业务路径 `/{route}/{method}` 严格隔离，防止把带键请求或恢复请求发给不支持的旧 relay 时被当成普通未带键调用静默执行。

### 2.3 Stanford SOSP 2015 `RIFL` 与 Raft 线性化会话（Ongaro Dissertation §6.3）

一手来源：Collin Lee et al., *Implementing Linearizability at Large Scale and Low Latency*（SOSP 2015, RIFL）与 Diego Ongaro, *Consensus: Bridging Theory and Practice*（Stanford PhD Thesis, 2014, §6.3 *Implementing linearizable semantics*）。

- **核心机制：**
  1. **单调客户端会话序号（`(client_id, sequence_number)`）：** 客户端先向服务端注册获取唯一 `client_id`，每次 RPC 携带递增 `seq` 与 `first_incomplete_seq`（回收水位），服务端据此精确知道哪些旧结果已被客户端确认收到、可以回收。
  2. **原子提交结果记录：** RIFL 将结果记录（Completion Record）与对象状态变更在同一次事务/日志条目中原子写入，保证“变更生效”与“结果可查”永不分裂。
  3. **有界结果表与回收代价：** Ongaro §6.3 特别指出：若服务端按内存上限淘汰了某个旧会话的记录，后续再收到该会话请求只能拒绝并报会话过期，而不能假装没执行过；并且无状态客户端或跨进程恢复很难安全推进 `first_incomplete_seq`（例如在共享传输连接上，传输层的 ACK/RELEASE 只说明对端接收线程收到了字节，不证明上层应用已经持久化收到结果）。
- **对 C-Two 的启示：**
  - **放弃伪造“已交付确认水位”：** C-Two 的 IPC `RELEASE` 只证明对端释放了 SHM 租约，在客户端本地超时后传输层仍会发送 `RELEASE`。把它当成“应用已收到结果”做优先淘汰是虚假不变量，不如用单一、诚实的完成顺序 FIFO 队列管理全部终态元数据与保留载荷。
  - **跨进程查证需求否定 `(client_id, seq)` 会话绑定：** 在科学计算流水线中，发起写调用的进程崩溃后，常由另一个监控/恢复进程接手查证同一 `call_id`，因此去重键必须由调用方直接给出、跨连接且跨客户端进程稳定，而非绑定某条连接或单个客户端注册号。
  - **明确 L1 与 L2/L3 职责切分：** RIFL 之所以能跨宕机保证严格一次，是因为它把结果表写进了共识状态机/持久化存储。C-Two 是无内置持久化日志的 RPC 运行时（L1），把“单次注册内纯内存去重与结局查证”做好做稳，同时把跨 Host 重启的持久操作日志与资源 revision 对账诚实地留给编排层（L2）和领域服务（L3），是最符合定位的架构边界。

## 3. 机制对照与 C-Two 选型结论

| 维度 | Oracle Transaction Guard | IETF `Idempotency-Key` / Stripe | RIFL / Raft §6.3 | C-Two ADR 0006（路线 1） |
| --- | --- | --- | --- | --- |
| 标识来源 | 服务端/驱动绑定 `LTXID` | 调用方高熵键 | `(client_id, seq)` | 调用方显式 `call_id`（跨连接、跨进程） |
| 额外 RTT | 需绑定事务/会话 | 0 RTT | 建会话需注册 `client_id` | **0 额外 RTT**（仅显式带键一元调用） |
| 迟到旧包防二次执行 | 持久化 `LTXID` 封禁 | 保留期内按指纹重放 | 持久日志 + 序号水位 | **提交只发一次 + 恢复遇 Miss 返 `723` 绝不代执行**；超注册/超容量交持久层对账 |
| 查证是否阻塞在业务槽 | 独立连接只读查询 | 无独立只读查询端点 | 依赖重发原请求 | **独立非阻塞 `query_call`**，绕过业务槽 `N` 与 `max_pending` |
| 大载荷响应保留 | 仅布尔结局（无业务载荷） | 缓存完整 HTTP 响应 | 持久保存结果直到确认 | Host 级 `(outcome_max_entries, outcome_max_retained_bytes)` 独立上限；允许 `0` 字节纯元数据模式；淘汰负载只降级不删终态元数据，重放时报 `722` |
| 故障域 | 跨数据库实例崩溃持久 | 服务端数据库 24h | 共识集群持久 | **单次 Host 注册内纯内存**；与 ADR 0004 一致 |

## 4. 关键工程边界总结

1. **为什么不选“预申请票据（+1 RTT）”？** C-Two 的核心热路径是同机微秒/亚毫秒级科学计算 RPC，把每次带键写调用强加 1 次前置往返收益不足；通过“显式提交只发一次 + `query_call` 只读查证 + `recover_call` 遇 Miss 绝不代执行”的三操作正交设计，既保住 0 额外 RTT，又堵住恢复路径误触发二次执行的缺口。
2. **为什么不对 `@cc.write` 默认自动生成 `call_id`？** C-Two 未标注方法默认即为写访问；自动生成既会给无须去重的普通高频写平白制造内存/哈希开销，又让调用方在无感情况下承担语义隐含约束。显式按次传入（`with_call_id`）让普通调用保持 0 字节、0 哈希、0 额外开销。
3. **为什么只覆盖一元调用？** 服务端流（`@cc.stream`）按条目推进且无单一原子终态载荷，主题（`@cc.topic`）已有注册内单调 `seq` 与历史环，二者均与一元去重模型正交；把范围收敛在一元调用上，协议与状态机最清晰。
