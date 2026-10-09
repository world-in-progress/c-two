# 本地端点生命周期与运行器接入

[English](local-endpoint-lifecycle.en.md) · 简体中文

本文说明原生端点所有权、运行器接入与回收规则。目录配置见 [配置指南](configuration.md#本地端点位置与平台差异)。

本地端点由 Rust `c2-config` 统一派生，SDK、relay 和 c3 使用同一上下文。Unix 默认路径为 `/tmp/c2-<uidhex>/<32 字符标识>`，最终目录包含 nonce gate/marker 与可退休的监听租约；原 gate inode 保持固定直到监听租约释放。Windows 使用当前登录 SID 的 Named Pipe 和不可继承的内核监听租约。调用方传逻辑 `ipc://` 地址，系统自动选择后端；凭据中的 `managed-v2` / `named-pipe` 是格式元数据。

Unix 的最终端点目录由显式代码 `cc.set_local_endpoint(root=...)`、CLI `--ipc-root` 或环境变量 `C2_IPC_ROOT` 覆盖；代码/CLI 优先于进程环境，进程环境优先于 `.env`，默认 `/tmp/c2-<uidhex>`。端点路径为 `<root>/<32 字符标识>`，没有额外用户目录、版本目录或 `.sock` 后缀。自定义目录由应用预建，允许 `0755`：当前用户持有并具备读、写、遍历权限，组和其他用户不可写。默认目录创建为 `0700`；socket 在 listen 前设置并核实为 `0600`。Linux/macOS 的 bind、connect 和探测均通过目录句柄访问短名称，支持包含中文和空格的长目录，实际目录打开能力仍由文件系统决定。回收保留应用目录和无关文件。root 不移动 SHM、file spill 或配置文件。Runtime 在第一次本地 bind/connect 尝试前冻结上下文，失败尝试同样冻结；之后修改 root 返回配置已冻结错误。纯查询不产生文件系统 IO。同一逻辑地址在不同 root 中使用独立上下文，实际连接仍验证 server id、instance id 与路由契约。Windows 自动使用当前登录 SID 的 Named Pipe，明确拒绝 Unix root 配置。

Unix 凭据统一使用既有 schema 3，记录最终 root、namespace id、incarnation 与 socket 身份；编码、解码和 CLI 读取共用 32 KiB 上限。Windows 保持 kernel-managed schema 1。inspect/reap/sweep 使用凭据捕获的上下文，显式传入不一致的 root 在访问目标前被拒绝。凭据中的 socket 完整路径由原生配置派生，不作为输入字段；凭据自身不授予删除权限。

默认生命周期为 Persistent，常驻服务直到显式关闭；普通业务连接断开、idle 驱逐、relay 重连不结束服务。只为一个控制器独占管理整个 Runtime 的专用子进程选择 OwnerBound。控制器通过 cc.owner_control_pair() 创建关系，用 cc.spawn_owned_child(receiver, program, args) 只向目标子进程转移 receiver，独自保存 keepalive。子进程显式调用 cc.adopt_owner_stdin()，并在 register() 前设置 cc.set_server(lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0), owner_control=...)。

keepalive.shutdown() 或控制器退出产生 EOF；新准入停止，经过有限宽限期后进入原生排空。继承通道不可重连或接管；宽限期不提供恢复入口。OwnedChild.poll()/wait(timeout=...)/kill() 观察或管理进程，close() 只释放观察者，原生共享 reaper 继续负责 OS 回收。

cc.shutdown(timeout=...) 返回原生结构化完成结果；直连 IPC admin shutdown 的 ACK 仅证明已接受发起排空，不能用作 callback 排空完成或运行 hook 的依据。completed=false 时保留原 Session、bridge、路由及关闭 hook，稍后观察同一事务。监听器关闭、工作排空和 payload lease 是独立事实；端点回收不释放 held/borrowed，FastDB 检查视图由各自 lease 释放时失效。

运行器在服务就绪后调用 cc.inspect_endpoint(address)，保存 present 结果中的 EndpointCredential。present 只表示对象被观察，不证明进程存活。WouldBlock 是协调争用，不能视为死亡。子进程正常退出或 kill()+wait() 确认退出后，再用 cc.reap_endpoint(address, credential) 精确收尾；旧凭据不能删除新 incarnation。

需要续扫时使用 cc.sweep_endpoints(addresses=[本次运行的逻辑地址], max_entries=64, max_ms=10) 的 context manager 和 next_batch()；只在原生报告 round_complete 时记为本轮完成，中断或命名空间变化单独处理。c3 endpoint inspect/reap/sweep 调用同一原生机制，sweep 可重复传 --address，需要非默认 root 时传 --ipc-root（或环境变量 C2_IPC_ROOT）。运行器不拼 OS 路径，不直接 unlink。

Relay 发现跨 root 的行为：独立 relay 在启动时解析并冻结自己的本地上游域上下文，一个 relay 本地域对应一个 root，上游资源与它使用一致配置；不同 root 可运行不同 relay。客户端在发现请求中携带 Core 派生的本地 namespace 标识（不含原始路径，不是授权凭据）；仅当 anchor 在本机、标识与 relay 本地上游上下文匹配且握手身份/契约均匹配时，自定义 root 路由才可能提供直连 IPC 候选，否则只提供 HTTP。namespace 不匹配是路径选择条件，不是路由失效：HTTP 路由保持可用，默认 root 的既有 IPC 快路径不变。Windows 侧由 backend 与当前 logon scope 构成标识，实际权限仍由 OS DACL、握手身份与完整路由契约检查。

原生路由获取查询服务端当前权威状态，随后固定 route UID/revision 和 server instance。数据连接 idle 驱逐保留资源登记与 owner；旧代理不能借同名替换取得新资源。生成的 TypeScript 持久直连客户端也使用该控制查询和身份固定规则。

对象替换、未知端点、损坏记录与部分初始化报告 unverified，不能凭年龄、PID 或连接失败删除。同一通信组使用相同构建及对应的最终目录配置；路径调整直接替换旧布局，旧凭据和旧进程端点不做自动迁移。升级不清扫旧目录中的历史数据。Windows 端点回收报告内核管理或 not-applicable。初始登记任意位置崩溃后的自动恢复不在保证范围内。平台验证范围见对应的验收记录。

Rust 示例见 [owned_child](../sdk/rust/examples/owned_child.rs)，版本行为见 [0.7.4 指南](releases/0.7.4.zh-CN.md)。[早期原生端点验收](reports/canonical-local-endpoint-validation.md)与[端点验收](reports/local-endpoint-final-validation.md)保留各自的历史源码和范围。
