# 内存策略与生命周期改造验收

2026-09-29。实现源码为 C-Two `abfeaf71deaa9ff4428d0c043ab80e3210746f47`，FastDB 为 `4f99f86a662b0e950a0dd29800c25a1c9fca4def`。随后提交的文档不作为二进制源码。改动位于 [PR #162](https://github.com/world-in-progress/c-two/pull/162)，尚未合并或正式发布。

## 已实现的行为

Rust 统一约束各池角色：buddy 按需创建，可显式预热，也可回收至零；禁用 buddy 后，inline、dedicated SHM、分块传输和文件 backing 继续按能力与预算选择。发送端选择传输方式，接收端选择重组存储，原有分层回退关系保留。

SHM、文件和重组预算在分配前预留，沿实际 backing、队列、响应与 held 所有权转移；完成重组不会提前返还仍被占用的容量。文件重组保留原 backing，移除了无条件提升和整包临时复制。按方向的 `cc.memory_stats()`、保留租约统计与弱 retired 观察不会接管 payload 或池的释放权。

请求取消使用同一份释放状态及实际分配池。专用 backing 的回收以每进程最多 2 个线程、256 个许可约束；dispatch 与 permit 安装/释放受同一把锁保护。迟到响应会释放对应租约。关闭屏障在调用者原时限内为取消与 join 留出时间，避免等到时限耗尽后才取消。

TypeScript 生成的响应读取器可在空握手段列表之后打开惰性 backing；其真实容量、范围和 generation 仍由 Rust 检验。配置使用方式见 [内存策略指南](../memory-policy.md)。

## Hosted 验证

同一源码对的 [Windows Native 运行](https://github.com/world-in-progress/c-two/actions/runs/36552609987) 四个 job 全部成功：

| Runner | 基础原生测试 | 完整适用门禁 | Rust/Python 收据 | TypeScript 收据 | 内存矩阵 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Windows Server 2022 | 707 / 707 | 23 / 23 | 18 / 18 | 12 / 12 | 9 / 9 |
| Windows Server 2025 | 707 / 707 | 23 / 23 | 18 / 18 | 12 / 12 | 9 / 9 |

两套 full 的 JUnit 各记录 Python 865 项、Windows harness 34 项、portable 25 项、TypeScript 14 项，均为零失败、零错误、零跳过。严格矩阵行数由仓库现行验证器检查，不由 pytest 总数推算。两套 local-platform 原始日志均明确记录半帧接收关闭、writer-slot 未确认关闭、并发取消 Drop gate 三项回归通过。

[Linux CI](https://github.com/world-in-progress/c-two/actions/runs/36552610119) 的 6 个 job 全部通过，包含 Core、CLI、Python 3.12 和 3.14t。[Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/36552609986) 的 52 个构建、导入及消费验证 job 全部通过；这是候选包验证，不代表发布。

## 不可变产物核验

Host 下载并核对四个 ZIP 的 GitHub API SHA-256，再逐项核对 `run-evidence.json` 中 98 个内部文件的哈希与大小。源码标识、runner、全部适用门禁及进程退出字段一致。归档 ID、ZIP 摘要、内部清单、门禁和消费摘要保存于 [验收元数据](evidence/memory-native-2026-09-29-abfeaf7.json)。

每个 full 归档中的 c3.exe 均与该 job 的 Rust/Python 矩阵、TypeScript 收据、普通及非管理员 wheel 消费所使用的字节一致：

| Runner | c3.exe SHA-256 |
| --- | --- |
| Windows 2022 | `5ab46b7919abe64331ec2a871e77b2e07b9e83b8f64191e7cf2df0abcd6db8f7` |
| Windows 2025 | `a3ebe2fdc078034583cef1b691b09a88ea04160be85908dec49ace24457a289e` |

两种身份的 wheel 消费均验证了隔离安装来源、实际 wheel 哈希、direct/relay 模式、held checked view 失效及 borrowed 输入失效。进程退出、正常 shutdown、临时目录移除，以及非管理员账号和工作目录清理均有成功记录。9 行内存矩阵的子进程正常退出，无强制终止；各 worker 的 close/shutdown 成功，原生模块哈希在各自矩阵内一致。

所有下载物和二进制仅保留在 `/tmp/c2-memory-windows-abfe/`，未进入 Git。Host 验证器最终结果为 `PASS: 0 failure(s)`；检查器修正了 Cargo 单数 `running 1 test` 和 wheel METADATA 的 CRLF 解析，未修改原始产物或放宽通过条件。

## 审查与范围

Host 拒绝过只统计等待队列而漏掉正在执行任务的计数，也拒绝过独立读取许可标志与计数导致的检查竞争。GPT-6 Sol 修正为锁内一致快照；关闭失败进一步定位到生产时限分配，并保留真实事件驱动的未确认和可重试证明。相关记录见 [取消与回收](memory-request-cancellation.md)、[计数审查](memory-linux-cancellation-tests.md) 和 [关闭屏障](memory-close-barrier-review.md)。Buddy 完成状态没有直接充当验收。

基准记录首次/稳态延迟、吞吐、backing 预算峰值及独立 OS RSS。整包临时复制的移除有源码证据；逐处复制字节归因和优化百分比尚未测量，不由 payload 大小或 backing 容量推算。当前分块调用仍需完整重组后交给资源，不是增量消费 API。原始指针逃逸与 FastDB checked owner 的边界保持。

Windows 11 桌面及其他未执行环境没有通过声明。早期观察到的跨请求 chunk 调度现象仍是单独的后续调查；本轮没有宣称修复它。
