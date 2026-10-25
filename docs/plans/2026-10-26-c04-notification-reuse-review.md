# C04 通知复用复核

状态：2026-10-26 对 `968f7d29153ff23fc967f767d97506823554de1d` 只读核查源码和引入历史，未实施或测量。建议继续暂缓；没有可承诺的安全净减量。R35 与两份 owner 控制会话重构不在本次范围。

| 范围 | 原设计理由及复用边界 | 净收益判断 |
| --- | --- | --- |
| `c2-ipc/src/pool.rs::close_all_observed` 两处生产 5 ms 等待 | drain generation 在协调锁下发布，retired 状态允许在锁外完成，`prune_confirmed` 仅 try_lock 后通知，保留丢唤醒窗口的复查进度。逐记录 channel 能等待单条结算，但不能覆盖 generation、新登记关闭及报告聚合。 | 仅改等待表达式最多省约 0–4 行，不能删除事务/重试/报告状态，测试无明确减量。补齐跨来源通知反而增加协调成本。 |
| `relay/client_lifecycle.rs::run` 的 10 ms 周期 | 显式 publish/close/shutdown 有现成 Notify；最后外部裸 Arc 归还、连接终态、未确认清理后来具备成功条件，没有同一通知来源。终态先于任务 join 和资源释放，不能当关闭完成。 | 删除定时分支约 3 行会丢回收进度；完整替换需要新的引用包装或清理进度机制，当前没有整体净收益。 |
| `relay/upstream_control.rs::run_control_watch` 的 50 ms 周期 | 普通 IPC 不订阅路由更新，每轮真实完整合同 attestation 保证旧正缓存不隐藏路由移除。已有 RouteWatch 是有限批次回复，不是持续推送。 | 删除 sleep/常量约 2 行会忙循环或丢观测；改为持续目录推送属于另行设计的控制协议，不能捆绑为局部瘦身。 |

以上数字只描述机械删除的静态大小，不是可接受补丁的收益。`pool.rs::wait_until_draining_for_test` 的第三处 5 ms 是测试 helper，等待 acquire 实际检查的围栏；不是生产开销。它先查 cache 再等待 coordinator，若持 coordinator 反查 cache，会逆转生产的 cache → coordinator 锁顺序。

历史依据：`6b017f2e1aa7` 引入 IPC drain 事务；`a9111b814fff` 为 connect 总期限引入锁外原子状态、逐记录等待和有限 settlement，生产等待改为 5 ms 片段；`d39486b98a59` 修正测试对真实 acquire 围栏的观察；`7ab5a7882` 保留跨 shutdown 重试的异常详情。`5ed0414907fe` 引入 relay 单 observer 与容量限制，并将旧缓存检查改为实际 attestation；`512a3654a074` 引入 endpoint control watch；`fc8ad41d0761` 补齐容量恢复、准入退出和 token 条件发布。Host 已核对这些提交和相应差异。

保留的约束为：detach/登记原子性、元数据锁外关闭、同一个总期限、唯一重试 owner、未确认时精确 owner 仍可达、确认后才归还、异常详情不重复报告、1024 owner 容量、完整合同及 server instance 身份、旧 token 不能撤销新注册。Condvar 重新取锁仍可能等待，5 ms 不是整个 close 的硬实时期限。transport error 不授权撤路由，`Closed(RegisterCommitted)` 的发布早于开放顺序保持。

后续若有明确的正确性反例或资源成本证据，再选择单一子项。回归需覆盖检查与等待之间的通知、协调锁争用、并发 drain/discard、最后裸 Arc 归还、terminal 后资源仍忙、未确认后自动恢复、动态 unregister/同名重注册及旧 token；三系统 IPC/relay 与 Rust/Python/Node 矩阵仍保留。涉及持续推送时需单独说明协议切换，不能在 SDK 增加旁路。当前不改任何 API、ABI、协议或语义。

独立只读分析使用 hey-my-buddy run `b14c2cd8-7262-4b5b-b439-81f8d570e8ca`、固定空补丁 artifact `76ecf97c-e120-46e6-9489-1eab827fb039`。Host 复核后已接受并回收工作树；未把静态结论写成已运行的回归或性能结论。下一项建议单独评估 R07 的失败释放 owner/charge，先复现并确认最小方案。
