# Runtime 退役完整方案：Opus 5.5 high 复审

状态：只读复审结束，最终结论为有条件接受；最后的交接规则、hostless 持续收尾及 TransportPhase 条件已由 Host 写入方案。尚无生产实现，待用户确认。接受 review 产物不等于接受或验证生产修复。

## 输入与证据

- hey-my-buddy skill 0.56.0；用户指定 `claude / anthropic / claude-opus-5-5 / high`。requested、resolved、observed model 回执一致，未修改路由偏好。
- 宏任务 `macro-d6108004-b1c3-4861-89b5-06b2d786b7c7`；复审微任务 `0ecd7f29-0520-4d9a-84e8-efcfb1add8f1`。
- 固定只读源码快照 `720ef9ad39d5602ee02974fa510bd96735b297ff`，生产基线 `9e8da3aa` / `e0b5af2d`；快照含 Host 新增基线反例和初稿。
- 修订稿以完整正文提供给后续重建会话：v4 SHA-256 `357dce88b840acbef938a323470ea8893f91ec30b703f76f8e0aca715cde7d05`，v5 SHA-256 `15797670bf6c4519f9ad258f22f1836e4f205097c434c2d7074f643c4a01ac83`。不把最初工作树中的旧稿误记为后续审查对象。
- 最终 attempt `b71932ef-3f30-4ba3-982f-20050f13bd26`，artifact `071ba9e0-a99e-40f7-bb0c-16793ca1e86e`。固定输出 commit 等于输入，changedPaths 为空；Host 重算空补丁 SHA-256 为 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`。
- Buddy 全程仅作静态审查，无构建或测试。四项真实基线、原生库及反例哈希、预期红灯和实验卫生由 Host 执行并核对，见[基线记录](2026-10-28-runtime-late-connect-baseline.md)。

## 审查与取舍

| 审查点 | 最终方案的处理 |
| --- | --- |
| 瞬时重试的无 record 空窗 | 首进和每次重试共用 state→close_txn 的 Preparing 登记，检查原 epoch/closing/seal，拒绝后无新 I/O。 |
| 两个 driver 重复 drain | 未开始目标共享一个事务/driver；pending 事务升级同一个 Arc；真终态后才单独收当前余量。原 observer 保留到本次最终收尾。 |
| entries 被遗漏 | 三类 scope 都最终执行完整 close_all，不能只扫 Preparing/retired。seal 不预挂无人处理的 Closing 批次，避免自等。 |
| completed 含义 | 目标实际终态且 entries/Preparing/未确认记录均结束才终结。终态错误保留，pending 不换代；hostless 同样持续推进。 |
| 预算耗尽 | 原 ConnectAttempt 不重置；caller 只停止等待，精确 owner 留在原生 record，由原 Core driver 继续，重复 shutdown 观察同一 receipt。 |
| 原生封闭拒绝 | IpcError::PoolRetired 类型化，normalize 为 RuntimeRetired，sync_client 明确 PreDispatch，relay 标为 terminal，router 穷举补齐。 |
| journal 一次性交付 | 本项只读 terminal_outcome，不读 delivered 的空结果；不顺带删除既有低层消费规则。 |
| Drop/failed-start 范围 | 不修改无调用方的原 fallback，不新增 inline 无限等待或 reaper。显式退役交接前失败仍由调用方持有 owner；原最终 Drop 风险单列 R07。 |
| 目标/执行上下文交接 | 采用审查提供的重新准备规则：锁内发现 pending 已变终态则放锁准备后再核对；固定目标只会完成一次。必要 spawn 在状态核定后、发布前，失败无已发布 scope。 |
| Host clone 自等 | 先发布结果/通知，再释放 driver 的 Host clone。FailedStart 从 receipt 的 Weak 升级；直接持有 receipt Arc，不依赖可能被裁剪的 map key。 |
| 不必要扩展 | 不新增 Core current-Host 注册表，不为复用删除 FailedStartShutdownReceipt；新低层入口限定已序列化的明确目标。 |

最后一轮 Opus 明确：R1、R3 已解决，R2 范围收窄没有发现新增无主 owner 的反例；要求补上交接规则、hostless 循环、Weak 目标及 PreDispatch。Host 已落实这些文字条件，未另跑第四轮模型审查。完整方案的实现还需要独立审查与真实回归，不能把本记录当作运行证明。

## 基线测试与实施门禁的区别

当前四行是基线诊断，不将修复后的提前拒绝当作 expired 路径验证。实现时两条迟到入口断言结构化 runtime_retired；另两条必须进入“真实入池后、seal 前”停点，强制确认停点和原预算耗尽。所有客户端关闭正确性断言放在 peer 收尾前；peer 最后关闭只保证失败实验不留端点。当前四项均红灯，原生行为未被修改。

[完整方案](2026-10-28-runtime-retirement-core-plan.md)明确了新增低层 Rust API/错误分类、pending 可无完成上界、显式消费后再换代，以及 Python 锁等待的既有边界；这些取舍需用户确认。IPC 14、SHM 3、C/FFI ABI 6 及业务 wire 不变。
