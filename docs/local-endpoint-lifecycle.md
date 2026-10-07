# 本地端点生命周期与运行器接入

本文以 Python 0.7.0 / c3 0.3.0 为使用基线。版本可用性、升级与验证进度见[发布说明](releases/0.7.0.md)。

本地端点只由 Rust c2-config::LocalEndpoint::from_address 按平台唯一派生。Unix 自动使用 `/tmp/c2-<uidhex>/v2.2/<sha>.sock`，包含当前用户私有托管目录、nonce gate/marker、监听租约结束前对原 gate inode 的 pin 和可退休端点租约；Windows 自动使用当前登录 SID 的 Named Pipe 及独立非继承内核监听租约。SDK、relay 和 c3 没有协议选择开关，调用方只传逻辑 ipc:// 地址。凭据中的 managed-v2 / named-pipe 仅为格式元数据，不能用于选择后端。旧 Unix 地址映射、永久每地址锁与绑定/回收分支，以及 `endpoint_protocol`、`C2_IPC_ENDPOINT_PROTOCOL`、`with_protocol` 选择器均已移除。

默认生命周期为 Persistent，常驻服务直到显式关闭；普通业务连接断开、idle 驱逐、relay 重连不结束服务。只为一个控制器独占管理整个 Runtime 的专用子进程选择 OwnerBound。控制器通过 cc.owner_control_pair() 创建关系，用 cc.spawn_owned_child(receiver, program, args) 只向目标子进程转移 receiver，独自保存 keepalive。子进程显式调用 cc.adopt_owner_stdin()，并在 register() 前设置 cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0), owner_control=...)。

keepalive.shutdown() 或控制器退出产生 EOF；新准入停止，经过有限宽限期后进入原生排空。继承通道不可重连或接管；宽限期不提供恢复入口。OwnedChild.poll()/wait(timeout=...)/kill() 观察或管理进程，close() 只释放观察者，原生共享 reaper 继续负责 OS 回收。

cc.shutdown(timeout=...) 返回原生结构化完成结果；直连 IPC admin shutdown 的 ACK 仅证明已接受发起排空，不能用作 callback 排空完成或运行 hook 的依据。completed=false 时保留原 Session、bridge、路由及关闭 hook，稍后观察同一事务。监听器关闭、工作排空和 payload lease 是独立事实；端点回收不释放 held/borrowed，FastDB 检查视图由各自 lease 释放时失效。

运行器在服务就绪后调用 cc.inspect_endpoint(address)，保存 present 结果中的 EndpointCredential。present 只表示对象被观察，不证明进程存活。WouldBlock 是协调争用，不能视为死亡。子进程正常退出或 kill()+wait() 确认退出后，再用 cc.reap_endpoint(address, credential) 精确收尾；旧凭据不能删除新 incarnation。

需要续扫时使用 cc.sweep_endpoints(addresses=[本次运行的逻辑地址], max_entries=64, max_ms=10) 的 context manager 和 next_batch()；只在原生报告 round_complete 时记为本轮完成，中断或命名空间变化单独处理。c3 endpoint inspect/reap/sweep 调用同一原生机制，sweep 可重复传 --address。运行器不拼 OS 路径，不直接 unlink。

对象替换、未知历史端点、损坏记录与部分初始化均保守报告 unverified，不能凭年龄、PID 或连接失败删除。从 0.6 升级时，同一通信组的 client、resource server 与 c3 必须同步替换；旧进程端点不会迁移，0.6/0.7 本地路径不能默认互通。升级不删除旧 `/tmp/c_two_ipc` 历史数据，其清理需要独立范围和授权。Windows 端点回收报告内核管理或 not-applicable。初始登记任意位置崩溃后的自动恢复不在保证范围内；Windows 11 桌面等未验证环境见[发布说明](releases/0.7.0.md)。

Rust example: [owned_child](../sdk/rust/examples/owned_child.rs). Current execution status: [canonical endpoint validation](reports/canonical-local-endpoint-validation.md). Prior optional-mechanism evidence remains [historical](reports/local-endpoint-final-validation.md).
