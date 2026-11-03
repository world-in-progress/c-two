# 公开现有独立 Runtime：接口与所有权方案

状态：已按批准范围实施并完成验收，构建来源 `e0b5af2d`，见[最终记录](2026-10-28-public-runtime-acceptance.md)；未发版。方案审查的源码基线为 `a80380db`；本文替代未实施的窄 `probe_local_ipc` 草案。用户进一步要求直接公开已有能力，因此不附带一次性关闭状态、Runtime 上下文管理器、临时 namespace 自动删除或新的探测协议。下文保留批准时的方案与基线证据，验收结论以完成后的独立记录为准。

## 目标与直接复用

公开 `cc.Runtime()`，让调用方拥有一份现有 Python 注册对象。`_ProcessRegistry` 的实例方法直接成为 Runtime 的实现，单例获取与进程信号适配留在模块入口；不保留一份旧 registry 再复制一份新 Runtime。模块级 `cc.register/connect/set_*/shutdown` 委托默认 Runtime，显式实例执行同一方法。原生 `RuntimeSession` 继续是内部投影，复用同一 Core Runtime、Host、桥接、连接池、配置解析、预算与关闭事务；不把内部 `register_route` 等编码/调度接口整体提升为正式 Python 能力。

这覆盖两类用途：应用在私有 root 上用空 Host 做启动前检查；科研/嵌入程序分别托管和消费自己的资源，配置、Python 绑定及原生会话互不串用。独立表示所有权归属，不是进程、安全或物理内存隔离。同一 relay 的路由约束、同一 root 中相同 server_id 的监听冲突仍由原生层拒绝。

现有链的依据：`registry.py` 的 register/connect/unregister/shutdown 已使用 self 的 session 和 bridge；Core RuntimeState 已持有本实例的 IPC pool、连接事件源、配置与调用预算。需要拆开的主要是全局 settings、默认实例获取和进程信号。`Host::host` 等待 `Server::wait_until_responsive`，成功前已完成真实 PING/PONG；HMB 预检可以使用该既有启动能力，不必新建 raw-listener 探测器或再发一次 ping。

## 公开接口草案

```python
runtime = cc.Runtime()                  # 独立实例；不绑定、不冻结配置。
runtime.set_local_endpoint(root=owned_root)  # Unix 已存在的最终目录。
runtime.set_server(startup_timeout=30.0)
runtime.start()                         # 空 Host 也可用；原生就绪与 PONG。
address = runtime.server_address()
credential = runtime.server_credential()
outcome = runtime.shutdown(timeout=30.0)
```

`Runtime()` 无新增配置对象或一长串构造参数；使用现有 `set_*` 方法及签名。主接口如下；现有实例方法直接复用，当前仅在模块层提供的观察/接管方法增加薄实例投影：

| 接口 | 语义 |
| --- | --- |
| `set_local_endpoint`、`set_server`、`set_client`、`set_transport_policy`、`set_relay_anchor`、`set_call_execution_limits` | 只修改本实例代码覆盖或 native session 配置；校验、冻结、False/0 与错误顺序仍属于原生 resolver/Runtime。 |
| `register(CRM, resource, *, name, concurrency=None, bridge=None, input_lifetime=None)` | 本实例拥有 Python 绑定，Core 拥有注册事务；保留现有返回值、回滚、调度和关闭回调。 |
| `connect(CRM, *, name, address=None, timeout=None)` | 只查本实例的本地槽；显式地址走原 IPC/HTTP 分支；同一个 ConnectAttempt 覆盖所有等待。 |
| `unregister(name)` | 仅注销本实例路由；沿原 close outcome 调用本实例 shutdown hook。 |
| `start() -> None` | 复用现有 Bridge 构造与 start 链；允许零路由，不注册虚构 CRM，不隐式修改 root 或 relay 选择。 |
| `server_id()`、`server_address()`、`server_credential()` | 投影本实例当前 session；credential 的 Ready/Stopping/终态规则不变。 |
| `local_endpoint_context()` | 新增薄投影，读取本 session 上下文；不创建或冻结端点。 |
| `shutdown(*, timeout=30.0) -> dict` | 结束当前原生会话并返回原结构化 outcome；保留现有完成判据和会话换代。 |
| `connection_events()`、`native_lifecycle_snapshot()`、`native_terminal_outcome()`、`memory_stats()`、`hold_stats()`、`call_execution_snapshot()` | 当前实例的既有原生观察；不是全进程聚合。 |
| `prepare_owner_takeover(ticket)` | 用本实例的端点上下文执行现有接管准备；票据、身份和宽限期规则不变。 |

`cc.close(proxy)` 和 CRM 的 context manager 按 proxy 自己携带的 client/lease 关闭，不通过默认 Runtime 查找、也不另加 Runtime owner token；关闭不改变 proxy 的原始归属。去掉默认实例初始化的内部副作用，代理关闭的参数、结果及释放语义保持。Runtime 不再额外提供容易与关闭自身混淆的 close(proxy) 方法。`hold`、`with_call_options`、`connection_observation` 继续跟随传入 proxy；资源/票据创建函数继续是现有独立入口。

`cc.serve()` 仍负责默认 Runtime 的进程信号与阻塞等待。显式 Runtime 的 start/register 已会启动原生后台 Host，由嵌入它的应用控制主线程和停止时机；本批不公开 Runtime.serve，不增加进程信号协调器或每实例观察线程。atexit 只处理默认实例；显式实例由其创建者调用 shutdown。

## 配置、连接与关闭归属

每个 Runtime 拥有一份现有 `C2Settings` 代码覆盖对象及一份现有 native session。默认 Runtime 使用现行公开 `c_two.config.settings`，显式 Runtime 使用新的空覆盖对象，不能复制默认实例已设置的值。`C2Settings` 继续只是代码覆盖，不保存原生默认值或已冻结配置。修正其 resolver helper 对全局 settings 的回读，所有实例路径均显式传入本实例来源。

进程环境与 `.env` 仍是所有实例可见的原生配置来源，遵守原先逐配置域的解析/冻结时机；本批不加入环境快照、inherit-env 开关或一套新 resolver。调用方需要独立配置时使用实例 set_*，不通过修改 os.environ 配置另一个 Runtime。None 继续表示原入口的清除覆盖/继承含义，不新解释成“禁用环境”。

Bridge 构造预检必须读取该 session 的实际 server IPC 覆盖与 SHM threshold，不能在 `_resolve_server_ipc_config(..., settings=None)` 中落回默认实例。把 Core 现有 server_ipc_config 解析投影给 native 预检复用；保持预检时机和异常转换，删除 Bridge 的全局配置依赖。

start 在已有实例锁下同步本实例 relay 覆盖，复用 register 里的“取得/创建真实 Bridge”代码，再调用同一个 Bridge.start。不产生“有 Host 却没有 Python Bridge”的第二条公开启动路径。这样 start 后首次 register 的 created_server 为 false，注册失败只回滚本次路由，不会把预先启动的 Host 当作本次创建物关闭。空 Bridge 也走原生配置冻结判据；生命周期观察直接投影 session，不以 Bridge 是否存在推断 Host 终态。若现有 ensure_host_started 等待仍持 GIL，只在该原生等待外层采用既有 py.detach，不新建工作线程。

relay setter 先完成参数规范化和 native 接纳，成功后才提交本实例设置；失败时两层值均保持原样。在现有实例锁下排序 setter、需要的 relay 同步与 session 发布，connect 继续使用自己的 ConnectAttempt 取得此锁。直接 IPC 和本地槽仍不触发 relay 同步。换代在发布新 session 前应用本实例 override，确保 shutdown → start 不意外改读环境中的另一个 relay。默认设置与实例设置的同步各用原有一份值，不加配置 journal 或额外锁。

两个 Runtime 的无地址同名注册各自解析本地槽；跨实例通信显式传地址，Unix 还须使用相同端点上下文。不会扫描进程内其他 Runtime，也不会将另一实例的 Python 对象当作自己的 thread-local 资源。相同外部 endpoint/relay 路由产生的冲突保持原规则。

proxy、在途调用和 Held 对象保有原有实际 owner。shutdown 处理本 Runtime 拥有的 Host、路由和缓存，不能提前释放外部仍持有的 payload 或宣称撤销所有外部引用。一个 Runtime 的 shutdown 不能关闭另一 Runtime 的连接、路由、事件源或预算；不新增全局实例表或“关闭全部实例”逻辑。

**保留当前可复用行为。** shutdown 未完成时保持原 session、Host、路由 journal 和 native 关闭事务，后续 shutdown 观察同一事务。完成后沿现有 `_swap_runtime_session` 建立空的新 session，显式与默认 Runtime 一致；不添加 closed/shutdown_requested 位或另一套生命周期。新活动属于下一会话，不能把原生旧会话的终态理解为 Python Runtime 对象永久不可用。

换代同样保留原规则：端点选择、SHM/chunk 代码覆盖、调用预算覆盖和实例 relay 设置沿原 handoff 保留；server identity、server/client IPC overrides、startup timeout、lifecycle/一次性 owner capability 按当前 shutdown 路径重置。退休 memory/call 统计只跟随原真实 owner 的弱观察，不人为退款。旧 connection_events 订阅仍绑定旧事件源，不承诺跨会话连续；需要下一会话事件时重新订阅，不增加转发器。调用者保存 shutdown 返回值作为本次关闭结果，当前 session 的 lifecycle 查询不会充当所有历史会话的日志。inspect/reap/sweep 与独立 admin helper 已可接收 context；显式实例使用它们时传入 runtime.local_endpoint_context()，不新增重复的实例维护接口。

本批不增加 Runtime 的 `with` 自动关闭语义，不新增关闭异常包装。显式 shutdown 返回的 completed 和错误字段仍需由调用方判断；不会把有界等待届满包装成已关闭。业务异常与 shutdown outcome 分别保留，避免以清理结果吞掉原异常。

## HMB 预检与临时目录

Unix 临时目录由应用通过标准库创建并拥有，例如 `tempfile.mkdtemp(dir=application_owned_parent)`；传给 set_local_endpoint 的是这个完整目录。Runtime 借用目录，只按原 close/reap 规则处理自己的 socket/lease，不删除 gate/marker、父目录或无关内容。确认 completed、runtime_barrier_error/route_close_error/ipc_client_close_error 为空，并逐路由确认 active_drained 且 close_error 为空后，应用对自己独占新建的临时目录执行标准库清理；目录并非共享 root，禁止把这种例程用于默认目录或已有应用 namespace。

预检步骤是：创建私有目录 → 创建 Runtime 并配置 root/startup_timeout → start 等待现有 Host 就绪与 PONG → 读取自身 credential（如需诊断）→ shutdown 并检查原 outcome → 清理应用创建的目录。等待释放 GIL；不用 socket 完整路径绑定、文件出现轮询、私有导入或新的 probe 函数。成功路径、正常绑定拒绝及单列失败启动清理门槛均已完成验收，结果见最终验收记录。

启动异常必须保留并在收尾后抛出；只有确认终态且全部清理检查通过，才可删除本次独占的新临时目录。关闭异常或清理未确认时，应用保留原错误、shutdown outcome 与临时路径；不能在 finally 中无条件 rmtree，也不能将一个外层 TemporaryDirectory context 的退出视为原生进程已经排空。文件删除失败由标准 OSError 保留 errno/path。完整例程展示两类错误同时保留和“确认关闭才删目录”的判断，不作为新的 SDK 管理框架。Held/backing 生命周期保持，IPC root 不管理 SHM/file-spill 的目录。

Windows 不传 Unix root，使用本 Runtime 自动生成的唯一 server id 和 TokenUser 范围 Named Pipe；没有需要删除的 namespace 文件夹。start、PONG、shutdown 与身份核对沿正式后端。这里的隔离是同用户范围内不同逻辑端点，不能声称创建了独立 TokenUser namespace；关闭遵守已有句柄/IOCP 语义。

科学计算需要更完整的运行验证时，可在实例上注册真实 CRM 资源，并用显式 IPC address 连接和调用，复用同一正式握手、契约校验、FastDB/lease 路径。预检不自动插入隐藏 CRM，也不改变上层科学资源模型。start 的响应失败继续遵守现有 startup_timeout/readiness/responsiveness 诊断；不能因本批公开入口便宣称内部 boolean ping 已具有新的 OS 错误保真能力。真正公开 connect 的结构化 OS 原因保持；通用 ping 的诊断增强仍单列。

## 现有失败启动清理：基线验证门槛，单列修复

源码中 wait_until_responsive 失败后直接 shutdown_and_wait 的结果被忽略，HostInner.Drop 随后进入既有 Core shutdown transaction；PyRuntimeSession 只保存成功 Host。两种交错需要在基线验证：首次启动失败后没有 Host；旧 Persistent Host 已终态、下一次启动失败后槽中仍保存旧 Host。后者的 shutdown 可能读到旧终态，不能只验证 hostless 分支，也不能把旧 Host 的完成当作新失败尝试的完成。

方案批准前只有源码疑点；批准后已用现有 startup gate / shutdown transaction 夹具确认两条基线反例，回执修复获单独批准，详见[修复方案](2026-10-27-failed-start-cleanup-observation.md)。原验收门槛为：要求本次相关清理是否完成及错误能被准确观察。如果基线通过，就保留证据而不加生产分支。如果证明存在错误完成报告，公开门面的验收暂停在该问题，另给具体最小修复及其语义影响，经用户确认后处理；本方案不预先授权遍历/合并所有事务或改生命周期。

修复评估必须识别失败尝试对应的具体 Server/已有事务，保留 delivered 一次性交付与旧 route journal，同一 deadline 内观察原 Condvar/outcome，并区分终态与清理无错误。不能增加第二份 Host、Python pending/closed 状态或再发一轮 shutdown 来猜测完成。此项是不能虚报“可清目录”的门槛，不构成直接公开 Runtime 的新增管理机制；在证据未补齐前不接受异常路径零残留结论。

## 替换范围、影响与验收

| 范围 | 预计变更 |
| --- | --- |
| Python registry 与导出 | 原 `_ProcessRegistry` 实例逻辑直接公开为 Runtime；默认获取/atexit 保留为模块私有适配；补 start、上下文和现有观察的薄投影。关闭 proxy 的纯操作不再初始化默认实例。 |
| settings / Bridge / native 配置投影 | 全局读取改为实例来源；Bridge 预检使用 session 实际配置；保留原生校验与冻结顺序。不复制注册/连接实现。 |
| 失败启动关闭结果 | 先做两组基线反例；若证实问题，单列方案再确认，不混入本批已拟公开的接口范围。 |
| 现有测试 | `_ProcessRegistry` 私有夹具迁为 Runtime 与默认入口；默认路径、真实实例路径共用场景参数，替换重复准备代码，不复制整套测试。 |
| 文档与示例 | 同步 README、Python API、配置/生命周期说明、Rust/Python能力边界与未发布说明；增加应用拥有临时目录的公开接口示例。旧窄 probe 草案退出实施队列。 |

公开 Python 新增 Runtime 与实例方法；现有模块函数签名及运行语义保留（代理关闭去掉默认实例初始化副作用），`_ProcessRegistry` 不留内部兼容别名。Native RuntimeSession 不整体公开，不增加任意编码器或原始路由输入入口。Rust Runtime/Host 的现有签名不变；若为 Bridge 直接复用 Core 解析而公开现有只读 server_ipc_config 方法，则只增加该配置投影。修正独立配置对象回读全局值属于配置隔离语义修复；relay 设置失败不再残留 Python 覆盖值是错误路径一致性修复；失败启动关闭结果如需修复，另行列明 Rust/Python 语义影响。IPC 14、SHM 3、C/FFI ABI 6、业务帧、owner 控制协议和凭据 schema 4 均不变。没有协议兼容分支，也不增加第三方依赖。

静态估算：公开/去单例耦合与上述必要适配，生产净 +90–200 行；测试净 +200–320 行，文档/示例约 +80–140 行。失败启动结果若证实需修复，单列测量，不在这里虚估实现量。大量现有方法仅调整归属，不计成新机制；这些数字是静态估算，不是补丁测量，也不作为压缩必要断言的目标。主要维护成本是实例配置与默认入口的边界测试。本方案不再承担旧 probe 的离线 coordinator 终结、raw connect/accept、结果数组和专用同步 worker 适配。

验收按以下顺序进行，先补基线反例，再改实现：

1. 两个私有实例的 settings/Bridge 预检串用反例；默认实例已配置与未创建两种情况；Host 首次启动失败、旧终态 Host 后重新启动失败的受控交错；若证实错误完成报告，按上述门槛停下单列。
2. 两实例同名资源、不同 root/配置/预算；本地无序列化路径和跨实例显式 IPC；关闭 A 后 B 的既有连接/事件/注册仍可用。proxy.close、with_call_options 与 Held 始终保有原 owner；不为测试加第二套 fake Runtime。
3. 冻结、False/0、非法环境值与错误顺序；无路由 start 后改配、start 后首次注册失败不得关闭旧 Host、失败 relay 设置原子性与 shutdown → start 保留覆盖；pending shutdown、正常换代、退休统计和旧事件订阅边界；原 hook 调用次数及 owner-bound 票据语义。
4. macOS 长目录（超过 sun_path、含空格/中文）、真实 sandbox network-bind 拒绝；Linux 长目录与真实权限拒绝；正常/失败收尾后本次 socket、lease、应用临时目录零残留。同步文件系统调用与关闭不作硬实时承诺。
5. Windows 开发机优先跑真实 Named Pipe 启动/PONG、两实例隔离、重复绑定、关闭和再使用；Unix root 明确拒绝，不能跳过共同 Runtime 用例。CI 包含 Windows 2022/2025、Windows ARM64、Linux/macOS 和 Python 3.14t。
6. 最终精确候选完成完整 relay、真实 Rust/Python/Node 跨语言及既有矩阵、独立审查与 GitHub Actions。Python/Rust错误、配置和连接生命周期保持同一 Core 权威。验证后清理本批产物，历史目录和下游仓库/状态不动；发版等待单独指令。

方案阶段完成两项独立初审及修订后的静态复核；初审发现的 Bridge/Host 归属、relay 提交顺序和配置来源已写入上述必要适配。失败启动清理疑点后来由确定性 Core 反例证实，并作为[单列修复](2026-10-27-failed-start-cleanup-observation.md)获批；修复及编译后 Native、三端和最终 CI 验收已在上述最终记录中完成。本文保留批准时的设计依据。
