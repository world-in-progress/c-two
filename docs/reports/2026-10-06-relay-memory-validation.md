# Relay 内存适配验收

本轮实现和验收已完成。范围是 relay 的上游 IPC 配置、共享预算、容量错误隔离与取消/清理生命周期；HTTP 缓冲预算和背压交付了设计，尚未实施。此记录衔接[此前内存改动整体审查](2026-10-03-memory-audit-validation.md)。

本机实施源码为 `d42c70ee53943ddfb6ba545f14c224b12f0fd31a`，本轮基线为 `b75e4d569fe7812edc0e70d88fbc306801529e01`。Windows 验证源码为 `ce01b462862603e18fd30655c5a7a502bd4128e5`，相对实施源码只增加独立 CI 分支的 workflow 触发条件；实现文件逐字相同。FastDB 固定 `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。后续文档提交不是构建产物的源码提交。

## 已完成的行为

- `RelayConfig.upstream_ipc` 经过原生 resolver 解析和验证，并由 RelayState 冻结。不同上游、初次连接与重连共享同一数据面预算。CLI 新增 `--ipc-pool-enabled` 及 SHM/file/live-reassembly 三项预算参数，dry-run 输出最终策略。`cc.set_client()` 仍只配置所在 SDK 进程；独立 `c3 relay` 有自己的配置入口。Rust 完整结构体字面量需要补充新字段。
- 已派发调用的响应重组容量不足保留 canonical 702 / `DispatchUncertain`，修复了因此误关健康共享连接的问题。另一个在途调用、ping 和后续调用均有真实 IPC/HTTP 回归；没有自动重放已派发调用。
- SHM 上传在尚未发布请求块时发生源错误，标记为 `PreDispatch` 并释放未发布块；进入实际帧发送之后仍保持保守分类。已经发送 chunk 的源错误继续关闭损坏的流。
- 上游 acquire 取消只撤销自己的连接尝试，避免卡住或回滚后来建立的连接。终止接收循环遇到短暂锁竞争时继续负责 pending 结算；在唤醒 Closed waiter 前先发布 disconnected。每次连接有独立状态，旧任务退出不能清除新连接的状态。
- 两项旧 owner 测试保留原有竞争下 50 ms 返回未确认、观测耗时小于 200 ms，以及状态/计费断言。释放人为竞争后的成功清理采用独立有限重试；模拟读者在 panic 时发送真实 `read_done`，避免污染后续退休任务。这个清理权限仅属于测试中唯一的模拟读者。

Buddy 交付后由 Host 检查固定差异、源码指纹与实际运行输出。所属宏任务的 20 个 Buddy 微任务均已验收，待 Host/待审查为 0。完整 relay 审查和最后一次连接状态增量审查均有独立只读 Buddy 记录。首次清理修复的独立审查发现了 disconnected 发布过晚的问题，该问题经修复和真实连接反例验证后关闭。

## 测试与产物证据

最终本机 Core all-features **1,239 项**、CLI **51 项**、Rust SDK **10 项**、native 参数测试 **2 项**通过。完整 Python **918 项**零失败、零跳过，执行 ID 与此前完整结果一致。本轮相对基线没有移除 Rust 测试定义，新增 **34 项 Rust/CLI 回归**均在本机和两套 Windows full 日志中出现。此前 867 项旧 Python 测试的保留关系见整体审查账本。

Rust 测试保持正常并行；本机普通 Python 测试使用两个 worker，跨语言构建/矩阵按既有资源隔离规则执行。没有将整套测试强制串行。

[Windows Native run 37378153376](https://github.com/Dsssyc/c-two/actions/runs/37378153376) 的同一源码对通过了全部适用门禁：

| Runner | 基础门禁 | 完整门禁 | Core / CLI | Python 与 harness | 严格矩阵 |
| --- | --- | --- | --- | --- | --- |
| Windows 2022 | 760 项通过 | 23/23 | 1,232 / 42 | 918 + 34 | Rust/Python 18 行，TypeScript 12 行 |
| Windows 2025 | 760 项通过 | 23/23 | 1,232 / 42 | 918 + 34 | Rust/Python 18 行，TypeScript 12 行 |

基础与完整门禁包含重叠测试，表中各列不相加为独立测试总数。Windows 的平台条件测试集合与 macOS 不同。

四个下载 ZIP 的 SHA-256 均匹配 GitHub API digest，内部文件大小与哈希逐项匹配清单。两套 full 各自的 18/12 行严格收据、9 行 IPC 内存矩阵、普通及非管理员 wheel 消费均通过。held/borrowed 生命周期、服务进程退出、临时目录清理及临时账号删除均已检查。每个下载的 `c3.exe` 与该 job 的矩阵及两种 wheel 消费收据所用字节一致。

完整指纹、artifact ID、实际执行 job/attempt、wheel/c3 哈希、测试保留清单和负向控制记录见[机器可读证据](evidence/2026-10-06-relay-memory-validation.json)。下载文件与完整日志保留在 `/tmp/c2-relay-followup-1006/`，二进制未加入 Git。

## 失败如何收口

早期 Windows run `37358977203` 在部分帧取消测试中等待 pending 清空超时。Host 通过每连接的受控竞争证明：旧的一次性 drain 会遗留清理责任，恢复旧行为的真实连接与单元反例均失败；修复后恢复通过。原始日志没有锁竞争现场，因此不把这一反例当成对那一次 Windows 调度的完整重建。

独立审查随后指出，等待结算完成才发布 disconnected 会让连接池短暂命中死连接。新用例持有真实 dedicated owner 锁，覆盖 Closed 通知、同地址 acquire、预算拒绝、有界关闭及新 epoch。恢复旧发布顺序会在 liveness 断言失败，修复后的 IPC 全套和两套 Windows 门禁均通过。

run `37364231858` 在 Host 因上述审查问题取消之前，Windows 2022 已出现 10 项退休测试失败。两个旧测试把解除人为竞争后下一次非阻塞 close 必须成功当作前提；失败退出还可能跳过 `peer.free`，使未读 backing 占住共享退休 worker。修复保留原有时限与内存断言，并补充异常清理。分别移除 caught-panic 和 Drop 的 `read_done` 后，新用例都在精确 backing 未退休的断言失败；恢复后 3 项定向测试通过。旧 Windows 记录没有具体 false 分支及在途 backing 身份，连锁机制的现场归因仍保留这个边界。

先前候选的 run `37367930487` 第一次 Windows 2025 full 因托管 runner 与服务器失联结束，没有上传完整日志或产物，具体原因未证实。只复跑该 job 后，同一旧源码通过了全部门禁；另三个成功 job 的原始产物保持不变，该轮证据完整保留。

Host 最后复核测试改写时发现，pool-only 持锁线程额外保留了 `RequestReleaseState` 的 Arc，使旧测试“pending 独占释放责任”的前提不再准确。新断言在旧 fixture 上确定性观测到 2 个引用而失败；持锁线程改为只持 pool 后，归属断言为 1，原测试及 IPC 全套通过。末次 13 行差异仅在测试代码中，生产部分逐字未改；最终本机和上述新 Windows run 均重新绑定到这一份源码。

Worker 沙箱中的监听/SHM `EPERM` 不计为通过或有效负向结果；相关验证均由 Host 在有实际 IPC/SHM 权限的环境补齐。

## 后续边界

`pool_enabled=false` 关闭 buddy，dedicated、checked chunk 和接收端文件后备仍可使用。上游数据面的统一预算不覆盖 HTTP 完整 body 缓冲、第三方堆内存或整个进程 RSS；控制 attestation/watch 仍是单独的惰性上下文。[HTTP 预算与背压设计](../plans/2026-10-06-http-memory-budget.md) 已列出分配前准入、响应进展和 Body 消费/断连责任，后续实施需要独立验收。

本轮 11 个修改的 Rust 文件格式检查通过。额外的全库 `cargo fmt --all --check` 在 8 个本轮未改文件上仍有既有差异，已记录为格式整理后续；没有为此扩大源码变更。

本轮没有调整正式版本、更新或合并 PR，也没有发布。`socu/memory-policy` 的新提交仍未推送；已推送的是独立 CI 验证分支。Windows 11 桌面、浏览器及其他未执行环境不在本次通过范围。原始 checkout 的 `main` 和 `.mimosa/` 未改动，既有回访保持暂停。
