# 客户端连接事件与公开结构化错误

基线为 `7ff2bf512f0d8d151d32c5e90929ba66deda47ba`，Python 发行基线为 0.8.1。用户已授权实现、统一验证和发布。本页先固定接口与验收边界；R18 作为独立正确性修复并行推进，最终候选统一运行 GitHub Actions。实现、基线反例、独立审查、三系统完整门禁、官方安装与发布验收已完成，C-Two 0.8.2 / c3 0.4.1 已发布；来源、增量和清理见[验收报告](2026-10-23-082-client-observation-release-acceptance.md)。

## 连接事件：接入已有终态权威

现有链为 `Runtime.connection_events → ConnectionEventSource.subscribe`，只有 `Server` 取得这个 source；客户端 `IpcClient.connect` 创建未关联 source 的 `ConnectionPublisher`。两端现有接收循环、心跳与关闭 guard 已准确发布终态，本批只让 Runtime 的 outgoing `ClientPool → SyncClient → IpcClient` 共享同一事件源，不增加 reader、轮询、逐连接线程或第二套关闭状态。

`cc.connection_events()` 订阅本 Runtime 的受管理 outgoing IPC 连接以及服务端接受的连接，仍从订阅点起观察，不回放历史。每个事件增加 `side: Literal['client', 'server']`；`kind`、`connection_id` 与 `value` 形状保留。client 事件 ID 和 `cc.connection_observation(proxy).connection_id` 相同，关闭值来自同一个终态记录。opened 只证明物理 OS 连接建立/接受，不证明握手、合同或业务就绪；握手失败也必须成对收尾。重连产生新 ID；共享逻辑代理复用旧物理连接，不重复 opened。HTTP-only、同进程直接调用不虚构 IPC 事件。独立的原始 admin/owner 控制连接不改造成 Runtime 客户端缓存成员。

Python 的唯一阻塞观察入口为 `ConnectionEvents.recv(timeout=None)`，等待释放 GIL；增加 `events.close()`，只结束该订阅并唤醒其等待者，不关闭连接，幂等，后续 recv 抛 StopIteration。复用 Tokio broadcast（固定 1024 条、Lagged 终止订阅）并使用现成 CancellationToken 取消订阅等待；同步由 Core 持有，Python 不维护取消线程、计时器或事件汇总器。一个订阅只允许一个 recv 消费者，第二个并发 recv 明确拒绝，不能排在无限等待之后。

退役 Python `ConnectionObservation.wait_closed()` 及 Core 专为它提供的同步 `wait_connection_closed()` 桥，不留别名；保留 `connection_id` 与 `snapshot()`，供迟到观察、终态查询和事件关联。底层 Rust 异步 wait_closed 仍服务现有连接关闭屏障及可取消的单连接 future，不增设一套实现。观测对象与订阅仍只保留元数据，不保留 transport、payload lease、Host 或 Runtime。预使用 Runtime 替换保留订阅；自然 source 结束、显式 close、timeout、lag 的结果明确区分。

## 错误：公开现有投影，不重做分类

正式类型为 `c_two.error.CoreError`，继承 `CCBaseError` 和 `RuntimeError`；`CCError` 保持语义错误类型。PyO3 通过已导入的公开异常类构造同一实例，删除 `_native.CoreError` 导出，不增加公开别名或包装异常。Python 类声明各字段的类型和默认值，所有实例字段齐全；Native 仍是内容的唯一生产权威，不解析消息、不改变错误顺序或重试资格。

| 字段 | 类型与语义 |
| --- | --- |
| category | semantic / contract / admission / transport / lifecycle |
| io_operation | bind / connect / None；确切本地操作 |
| io_kind | permission_denied / not_found / connection_refused / address_in_use / other / None；沿用 EndpointIoError 分类 |
| raw_os_error | int / None；OS 原始 errno 或 Win32 错误码，无原生码的策略拒绝和非 I/O 错误为 None |
| transport_kind | ipc / http / None |
| transport_phase | str / None；Core 传输分类为 pre_dispatch / dispatch_uncertain，语义错误保留任意对端诊断值，均不自行授权重试 |
| code、name、message、details | 现有 canonical 错误值与诊断；保留 transport 外层消息和内层 semantic payload 的区别 |
| lifecycle_kind、startup_stage、timeout_seconds 等 | 逐项声明现有生命周期、启动、登记/撤回字段；不适用时为 None，独立 dict 不跨实例共享 |

现有 error_bytes 内部投影和 wire 解码不随本批 R32 重构。配置/期限/合同/握手错误不生成虚构的 OS 原因。更新当前 README、Python API/生命周期/配置文档及发布迁移说明；历史发行证据不改写。

## R18 与协议边界

R18 保留 1024 owner 上限、unpublished/closing 计数和释放顺序；容量拒绝保留 unavailable 并复用原 1 秒重试循环，永久 closed 才退出。不可用记录移入现有 UpstreamControls 共用一把锁但保留独立寿命，stop 不额外清空；任务状态更新复用现有 token fence，任务/原因清理原子完成。数据面不可用时仍实时核验，成功可以继续调用，不把标记解释为自动撤路由。R06/R07 及两份控制会话重构继续暂缓。

R18 没有 API 变化。连接事件和错误项改变本地 Python API，并同步调整必要的低层 Rust 观察接口；IPC 12、SHM 3、C/FFI ABI 5、owner-control 1、业务帧和 CRM 哈希不变，不增加网络字段、协议兼容或回退。发布门禁须覆盖这些删除和接口迁移后的确切候选。

## 实施顺序与验收

1. 在原生产实现补基线反例：R18 满容量后不恢复、客户端订阅缺失事件、公开 CoreError 缺失。记录实际失败断言，保持原来的正常行为对照。
2. 分别实现 R18、事件源接入与订阅关闭、公开错误类；迁移被替代的测试，完成定向回归，独立核对错误、终态及释放顺序。只保留一个 Python 等待入口。
3. 一个消费者观察至少三个真实服务的连接：杀死对端、正常停机、本地物理关闭；逻辑代理 close 不冒充物理关闭；共享连接与重连 ID、opened/closed 顺序、订阅前后行为、超时/取消/lag、GIL 与无主订阅回收均覆盖。真实错误测试验证公开捕获、完整字段、OS 无监听/重复 bind/权限不足及非 I/O 对照。
4. 完整 relay、真实跨语言与持久连接回归；macOS、Linux、Windows Named Pipe 实跑，Windows 优先开发机；最终统一运行 GitHub Actions 的 x64、ARM64 和 Python 3.14t 门禁。补齐独立审查，确认本次 socket/lease/进程零残留，报告生产、测试和文档实际增量。
5. 更新版本和发行说明，以同源已验证候选发布；核对包括 Windows ARM64 在内的 wheel 与安装行为。保留日志/摘要/收据，清理本次构建产物和临时 worktree，按既有流程同步本地、fork 与 upstream。历史 IPC 目录始终不在清理范围。

主要风险是错误类导入环、错误字段默认值遮蔽原有判断、订阅取消与收到事件同时就绪、重连将旧终态写进新代、事件对象意外持有资源，以及控制/观察任务关闭顺序。所有关闭通知都只描述连接事实，不证明进程退出、请求失败可重放或资源已排空。若测试证明本页某项需要扩大协议或生命周期策略，先停在该边界报告具体依据。
