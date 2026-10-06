# 本地端点生命周期与运行器接入

Rust Core 负责服务准入、排空、关闭事务及回收判断。Python/Rust SDK 投影原生接口；业务连接断开不代表服务结束。默认生命周期为 `Persistent`，默认端点协议为 `legacy-v1`。

Unix 需要让端点锁元数据随实例退休时，服务端、客户端和 relay 上游应一致配置 `endpoint_protocol=managed-v2`。当前托管目录格式为 `v2.2`，由 `c2-config` 统一派生；gate/marker 格式 2 同时核验设备号、inode 和随机 nonce。新目录不自动采用旧 v2 或 legacy 记录。Windows 使用既有 Named Pipe 和当前登录 SID 内核租约，`managed-v2` 会明确拒绝。

```python
import c_two as cc

# Unix 服务进程，在 register() 前设置。
cc.set_server(ipc_overrides={'endpoint_protocol': 'managed-v2'})

# Unix 客户端进程，在 connect() 前设置。
cc.set_client(ipc_overrides={'endpoint_protocol': 'managed-v2'})
```

只为一个控制器独占管理整个 Runtime 的专用子进程选择 `OwnerBound`。控制器通过 `cc.owner_control_pair()` 创建私有关系，用 `cc.spawn_owned_child(receiver, program, args)` 只向该子进程转移 receiver，独自保存 keepalive。子进程显式调用 `cc.adopt_owner_stdin()`，并在注册前调用：

```python
cc.set_server(
    lifecycle=cc.LifecycleConfig.owner_bound(owner_missing_grace_seconds=3.0),
    owner_control=cc.adopt_owner_stdin(),
)
```

`keepalive.shutdown()` 或控制器退出产生 EOF。服务暂停新准入，经过有限宽限期后排空。当前继承通道不可重连或接管。共享多个运行的常驻 worker、relay 保持 `Persistent`。`OwnedChild.poll()`、`wait(timeout=...)`、`kill()` 观察或管理进程；`close()` 只释放观察者，原生共享 reaper 继续持有并回收 OS 进程，不能代替关闭 keepalive。Rust 用法见 [owned_child 示例](../sdk/rust/examples/owned_child.rs)。

`cc.shutdown(timeout=...)` 返回原生结构化结果。`completed=false` 时保留原 Session、bridge、路由及关闭 hook，稍后继续观察同一事务。监听器关闭、工作排空和客户端仍保留的 payload lease 是分别报告的事实。held/borrowed 的正常 FastDB 检查视图在各自 lease 释放时失效；端点回收不释放它们。

运行器应在服务就绪后调用 `cc.inspect_endpoint(address, endpoint_protocol=...)`，保存 `present` 结果中的 `EndpointCredential`。`WouldBlock` 只表示短暂协调锁争用，不能当作死亡；`unverified`、其他 I/O 错误要按返回原因处理。`present` 也只说明观察到对象，不证明进程存活。子进程正常退出或 `kill()` 加 `wait()` 确认退出后，用 `cc.reap_endpoint(address, credential)` 精确收尾。旧凭据不能删除新 incarnation；`reaped`、`already-absent`、`busy`、`stale-target`、`unverified`、`io-error` 有不同含义。Windows 报告内核管理或 `not-applicable`，不能伪造 Unix 回收结果。

需要续扫时使用 `cc.sweep_endpoints(protocol, addresses=[本次运行的逻辑地址], max_entries=64, max_ms=10)` 的 context manager 和 `next_batch()`。每批及整轮完成、中断、命名空间变化均以原生返回结果为准；单批时间预算是调度目标。`c3 endpoint inspect/reap/sweep` 提供同一机制，sweep 可重复传 `--address`。运行器不得直接拼接 OS 路径或 unlink。

`legacy-v1` 的永久 rendezvous 锁仍保留。没有有效记录的旧 socket、部分初始化、记录损坏或已替换的 gate/marker 保留并报告 `unverified`；年龄、连接失败或 PID 不能授权删除。初始登记任意位置崩溃后的自动恢复不在当前承诺内。维护需要一个仍运行的 Runtime 或显式运行器/CLI，系统不会启动额外守护进程。

本次 C-Two 提供运行器接入能力与真实子进程验证。hey-my-buddy 已安装运行器采用这些新接口、正式软件包发布及 Windows 11 桌面验收是分别需要实际证据的后续事项。源码与产物证据见[最终验收报告](reports/local-endpoint-final-validation.md)。
