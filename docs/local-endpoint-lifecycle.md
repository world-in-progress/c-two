# 本地端点生命周期与运行器接入

本文以 Python 0.7.1 / c3 0.3.1 为使用基线；该版本是尚未发布的候选，可用性与验证进度见[发布说明](releases/0.7.1.md)。

本地端点只由 Rust c2-config::LocalEndpoint::from_address 按平台唯一派生。Unix 自动使用 `/tmp/c2-<uidhex>/v2.2/<sha>.sock`，包含当前用户私有托管目录、nonce gate/marker、监听租约结束前对原 gate inode 的 pin 和可退休端点租约；Windows 自动使用当前登录 SID 的 Named Pipe 及独立非继承内核监听租约。SDK、relay 和 c3 没有协议选择开关，调用方只传逻辑 ipc:// 地址。凭据中的 managed-v2 / named-pipe 仅为格式元数据，不能用于选择后端。旧 Unix 地址映射、永久每地址锁与绑定/回收分支，以及 `endpoint_protocol`、`C2_IPC_ENDPOINT_PROTOCOL`、`with_protocol` 选择器均已移除，0.7.1 不重新引入。

0.7.1 起 Unix 私有目录的绝对根目录可覆盖：显式代码 `cc.set_local_endpoint(root=...)` > CLI `--ipc-root` / 环境变量 `C2_IPC_ROOT` > `.env` > 默认 `/tmp`。root 只是承载 C-Two 私有目录的容器，布局变为 `<root>/c2-<uidhex>/v2.2/<sha>.sock`；容器目录必须由应用/部署预先创建，C-Two 只在其中初始化用户私有目录与版本叶子目录，绝不接管、清扫或删除容器内其他内容。root 只移动本地端点与协调文件，不移动 SHM 名称、文件 spill 后备或配置文件。Runtime 在第一次本地 bind/connect 尝试前冻结端点上下文，失败的尝试同样冻结；冻结后更改 root 返回明确的配置已冻结错误，修改环境变量不影响已有连接、监听器、凭据与 sweep。纯配置查询不产生文件系统 IO。同一逻辑地址在不同 root 中是不同端点：比较与缓存身份包含规范化 root 与命名空间，实际连接仍验证 server id、instance id 与路由契约。Windows 无此覆盖：`cc.set_local_endpoint(root=...)`、`C2_IPC_ROOT` 与 `--ipc-root` 在 Windows 被明确拒绝为不适用（不是静默忽略），端点保持当前登录 SID 的 Named Pipe，本版不提供绕过 SID 隔离的任意 pipe 路径参数。

凭据与上下文绑定：默认 root 的 Unix 端点继续输出既有 schema 2 凭据（语义固定为历史默认 `/tmp` 上下文），非默认 root 使用新增 schema 3，记录受验证的 root 命名空间及 incarnation、inode 等身份字段；Windows 凭据保持 kernel-managed schema 1，不被强加 Unix 文件系统字段。运行器在服务就绪后保存凭据，inspect/reap/sweep 严格按凭据捕获时的上下文执行；显式传入与凭据不一致的 root 会在接触目标前被拒绝，凭据也绝不会在另一个 root 中寻找同名端点。

默认生命周期为 Persistent，常驻服务直到显式关闭；普通业务连接断开、idle 驱逐、relay 重连不结束服务。只为一个控制器独占管理整个 Runtime 的专用子进程选择 OwnerBound。控制器通过 cc.owner_control_pair() 创建关系，用 cc.spawn_owned_child(receiver, program, args) 只向目标子进程转移 receiver，独自保存 keepalive。子进程显式调用 cc.adopt_owner_stdin()，并在 register() 前设置 cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0), owner_control=...)。

keepalive.shutdown() 或控制器退出产生 EOF；新准入停止，经过有限宽限期后进入原生排空。继承通道不可重连或接管；宽限期不提供恢复入口。OwnedChild.poll()/wait(timeout=...)/kill() 观察或管理进程，close() 只释放观察者，原生共享 reaper 继续负责 OS 回收。

cc.shutdown(timeout=...) 返回原生结构化完成结果；直连 IPC admin shutdown 的 ACK 仅证明已接受发起排空，不能用作 callback 排空完成或运行 hook 的依据。completed=false 时保留原 Session、bridge、路由及关闭 hook，稍后观察同一事务。监听器关闭、工作排空和 payload lease 是独立事实；端点回收不释放 held/borrowed，FastDB 检查视图由各自 lease 释放时失效。

运行器在服务就绪后调用 cc.inspect_endpoint(address)，保存 present 结果中的 EndpointCredential。present 只表示对象被观察，不证明进程存活。WouldBlock 是协调争用，不能视为死亡。子进程正常退出或 kill()+wait() 确认退出后，再用 cc.reap_endpoint(address, credential) 精确收尾；旧凭据不能删除新 incarnation。

需要续扫时使用 cc.sweep_endpoints(addresses=[本次运行的逻辑地址], max_entries=64, max_ms=10) 的 context manager 和 next_batch()；只在原生报告 round_complete 时记为本轮完成，中断或命名空间变化单独处理。c3 endpoint inspect/reap/sweep 调用同一原生机制，sweep 可重复传 --address，需要非默认 root 时传 --ipc-root（或环境变量 C2_IPC_ROOT）。运行器不拼 OS 路径，不直接 unlink。

Relay 发现跨 root 的行为：0.7.1 的独立 relay 在启动时解析并冻结自己的本地上游域上下文，一个 relay 本地域对应一个 root，上游资源与它使用一致配置；不同 root 可运行不同 relay。客户端在发现请求中携带 Core 派生的本地 namespace 标识（不含原始路径，不是授权凭据）；仅当 anchor 在本机、标识与 relay 本地上游上下文匹配且握手身份/契约均匹配时，自定义 root 路由才可能提供直连 IPC 候选，否则只提供 HTTP。namespace 不匹配是路径选择条件，不是路由失效：HTTP 路由保持可用，默认 root 的既有 IPC 快路径不变。Windows 侧由 backend、当前 logon scope 与布局版本构成标识，实际权限仍由 OS DACL、握手身份与完整路由契约检查。

对象替换、未知历史端点、损坏记录与部分初始化均保守报告 unverified，不能凭年龄、PID 或连接失败删除。从 0.6 升级时，同一通信组的 client、resource server 与 c3 必须同步替换；旧进程端点不会迁移，0.6/0.7 本地路径不能默认互通。0.7.1 保留 0.7.0 默认端点布局与 IPC wire 版本，未引入新的 wire 或默认端点差异；本候选未单独测试与已发布 0.7.0 包的互通。自定义 root 是本地域配置：共享该本地域的 resource server 与 relay（若有）需要采用对应的 root 配置。升级不删除旧 `/tmp/c_two_ipc` 历史数据，其清理需要独立范围和授权。Windows 端点回收报告内核管理或 not-applicable。初始登记任意位置崩溃后的自动恢复不在保证范围内；Windows 11 桌面等未验证环境见[发布说明](releases/0.7.1.md)。

Rust example: [owned_child](../sdk/rust/examples/owned_child.rs). Current candidate status: [0.7.1 release notes](releases/0.7.1.md). The 0.7.0-era [canonical endpoint validation](reports/canonical-local-endpoint-validation.md) stays as history. Prior optional-mechanism evidence remains [historical](reports/local-endpoint-final-validation.md).
