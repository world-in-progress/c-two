# 并发注册与会话换代：基线与修复范围

状态：原注册交错与四项迟到连接基线均已复现；用户批准的[完整方案](2026-10-28-runtime-retirement-core-plan.md)已实施并完成完整三端 CI 验收，见[验收记录](2026-10-28-runtime-retirement-acceptance.md)。这项后续修复与此前公开 Runtime/失败启动回执保持分开记录。

在 `e4ddcc85` 的 `_ProcessRegistry` 和公开 Runtime 候选上，测试专用 Event 将已有 Bridge.shutdown 停在原生关闭与旧路由 hook 完成之后、Python registry 换代之前。另一线程在同一实例注册新路由并成功返回，再放开旧 shutdown。两版结果一致：

- 旧 shutdown 返回 `completed=True`，结果只有旧路由。
- 公共实例换入空 session，names 为空；仍被本次探针保留引用的旧 Bridge 含新路由，其 Host 仍运行，显式 IPC 调用成功。
- 旧 hook 已执行一次，新 hook 尚未执行；显式关闭旧 Bridge 后新 hook 执行一次。
- 两份任务私有 root 在显式收尾后均只剩 gate/marker，没有 socket 或 lease。探针保留了旧 Bridge 用于观察和可靠收尾；这不证明没有外部引用时 Host 永不析构。

这是原有 register/shutdown 的交错问题；新增空 Host.start 让同一交界更易触达，但“start 在并发 shutdown 之前返回，随后被关闭”本身允许按重叠操作排序，不能单凭它认定回归。需要修的是新注册的关闭结果和 hook 归属，不能用持锁等待所有回调来堵住窗口，也不能仅检查 host_started 而漏掉客户端或配置发布。

后续方案须复用实例所有权边界，明确新操作与关闭换代的排序，同时保留 callback drain 和 hook 重入能力。最初评估没有直接修改生产规则；后续批准的 Core 退役方案复用已有事务和原锁，新增拒绝事实从 Core 读取，不保存 Python closed 位。当前实现与验证结果以上述独立验收记录为准。
