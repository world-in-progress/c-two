# 并发注册与会话换代：独立待修项

状态：已隔离复现，尚未拟定或批准修复。不计入公开 Runtime 或失败启动回执的修复范围。

在 `e4ddcc85` 的 `_ProcessRegistry` 和公开 Runtime 候选上，测试专用 Event 将已有 Bridge.shutdown 停在原生关闭与旧路由 hook 完成之后、Python registry 换代之前。另一线程在同一实例注册新路由并成功返回，再放开旧 shutdown。两版结果一致：

- 旧 shutdown 返回 `completed=True`，结果只有旧路由。
- 公共实例换入空 session，names 为空；仍被本次探针保留引用的旧 Bridge 含新路由，其 Host 仍运行，显式 IPC 调用成功。
- 旧 hook 已执行一次，新 hook 尚未执行；显式关闭旧 Bridge 后新 hook 执行一次。
- 两份任务私有 root 在显式收尾后均只剩 gate/marker，没有 socket 或 lease。探针保留了旧 Bridge 用于观察和可靠收尾；这不证明没有外部引用时 Host 永不析构。

这是原有 register/shutdown 的交错问题；新增空 Host.start 让同一交界更易触达，但“start 在并发 shutdown 之前返回，随后被关闭”本身允许按重叠操作排序，不能单凭它认定回归。需要修的是新注册的关闭结果和 hook 归属，不能用持锁等待所有回调来堵住窗口，也不能仅检查 host_started 而漏掉客户端或配置发布。

后续方案须复用实例所有权边界，明确新操作与关闭换代的排序，同时保留 callback drain 和 hook 重入能力。当前批次保持既有会话换代规则，不增加 closed 位、计数器或锁协议；本记录不声称这类并发生命周期已修复。
