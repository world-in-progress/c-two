# 连接结束通知与 FastDB 0.2.3：验收记录

本轮实现已确认方案 A，并按后续授权更新 FastDB 依赖。实现候选为 `7bd749122f6bbd5824ed34e1884509b606783ddb`，FastDB 固定为 `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。完整跨平台门禁已通过，Windows 11 开发机的补充试跑被其应用控制策略阻止。C 所有者接管未实施。

## 实现与公开接口

Rust `Client::connection_observation()` / Python `cc.connection_observation(proxy)` 返回当前物理 IPC 连接的只读观察对象，可取快照或等待终态。Rust `Runtime::connection_events()` / Python `cc.connection_events()` 提供主机的 Opened/Closed 订阅。非 IPC 调用明确拒绝物理连接观察。

终态来自现有接收、心跳、关闭和发送取消边界：本地关闭、对端 DISCONNECT、EOF、连接重置、心跳超时、协议错误或 I/O 错误。每次连接只保留首个已知事实；不据此推断进程退出。重连有独立连接身份，旧观察者不能改变新连接。IPC 12、SHM 3、C/FFI ABI 5 和业务 wire 不变。

观察对象只持有元数据，不持有 Runtime、传输或内存 lease。通知不代替 drain；原 DISCONNECT/ACK、请求和响应释放继续完成。服务端队列复用 Tokio broadcast，容量 1024，首次订阅才分配；溢出返回明确 Lagged 并结束该订阅，已取得的单连接观察仍有效。晚订阅可以接收已有连接未来的 Closed，不伪造历史 Opened。

启动前合法配置替换通过已有 `transfer_unstarted_lifecycle_to` 事务转移事件源，Python 订阅与替换使用同一注册表锁。发送取消通过已有 AbortHandle 在 OS abort 前发布终态，写错误先记录真实诊断，没有新增 reader、轮询或释放状态机。

同环境 allocator 请求字节对照：未订阅源由 74,032 降为 32 字节；首次订阅后为 74,040 字节；释放后保留 0。这是实现阶段队列惰性化的分配计数，不是 RSS 或吞吐结论。

## 反例与测试修正

- 初审发现先订阅、再合法配置替换会结束订阅；旧候选公开 Python 反例 2 项失败，修正后的真实进程用例通过。
- 初审发现部分发送取消先 abort、后发布终态；将新增断言回填原候选，确定在“sender publishes before abort”失败；修正后通过。
- 原 dedicated 用例在 reply/read_done 后立即断言已完成异步回收。生产 release 路径与基线逐字相同；基线受控持有 retire executor 时证明回收仍可能待完成。用例改为等待既有 `close_shared_bounded`，原零残留断言保留。未把未插桩基线称为已复现随机失败。
- 原目录权限用例会与进程级 SweepLease 竞争。基线受控占用该 lease 后确定先得到 WouldBlock；测试改用已有子进程模式运行原测试体，保留 PermissionDenied 及诊断断言，生产错误顺序不变。

首次 CI `39561814405` 失败记录保留：新增 PyO3 Tokio builder 被既有 SDK/Core 边界检查拒绝；新增夹具调用 shutdown 后保留显式端点 context，污染后续 CRM/owner 用例。前者改为 Core 既有 runtime seam 的两个具体等待操作，Python 只做投影；后者使用已有 `_ProcessRegistry.reset()`。同一首次 CI wheel 的顺序反例为 1 通过/3 失败，只改夹具后为 13 通过。最终 native 的同组 13 项、边界 43 项、Core 4 个等待及 2 个真实连接测试通过；最终全矩阵验证二者修复。没有修改断言、期限或生产关闭策略来消除失败。

独立审查提出的极大 timeout 猜测，经锁定 Tokio 1.53.2 源码和公开 Python 实测排除：Duration 可以表示但 Instant 无法相加时，现有 Tokio 已使用 far-future deadline；关闭事件仍正常结束等待，没有增加额外分支。

Python 真实进程覆盖无在途调用时杀掉服务端、杀掉客户端、正常关闭、等待释放 GIL、配置替换接续和真实订阅溢出。原生覆盖实际重连、写取消、reader/heartbeat/close 竞争、ACK 与 drain 顺序以及观察者不保活连接。

## FastDB 更新

当前开发依赖统一为 FastDB 0.2.3：Python extra、Rust 依赖/锁文件、生成代码消费者、原生源码、构建脚本与 CI 固定值同步；已发布 0.7.6 的历史依赖说明保持原始事实。官方 release-manifest 的 SHA256 为 `a4cd7dd1a0634ab5ed15585c7064275ad68afebe7cd50a97139ec0d8b46a6f16`。

官方四个 CoreSDK 真包均通过外层与内部成员校验。准备脚本识别新增 Windows ARM64 包，复用 Windows DLL/import library 的严格校验。该能力不等于 C-Two 已发行或实测 ARM64；本轮 Windows 运行平台为 x64。[FastDB 0.2.3 发布](https://github.com/world-in-progress/fastdb/releases/tag/v0.2.3)声明 portable payload 语义与 ABI 117 保持。

## 验证

| 验证 | 结果与边界 |
| --- | --- |
| macOS 原生 local/ipc/server/Core/relay | 1153 通过，5 个既有子进程入口 ignored；完整 relay 416 项在其中 |
| Linux 原生同组 | 1137 通过，5 个既有子进程入口 ignored；完整 relay 416 项在其中 |
| FastDB 0.2.3 本机相关 Python | 348 通过，导出目录无 Git 元数据的 1 项检查跳过；该 Git 检查另在主仓库通过 |
| FastDB 0.2.3 真实 Rust/Python | 25 通过，含 direct/relay 18 行矩阵 |
| FastDB 0.2.3 Rust SDK/生成代码 | SDK 19、codegen 单元 2、生成 Rust 消费者 4 通过 |
| FastDB 依赖/构建脚本 | 460 通过，macOS 下 Windows npm 专用用例 1 项跳过；四个官方包真值验证实际执行 |
| GitHub Actions | 全部 9 个 jobs 成功；Linux/macOS full 各 25 门禁、Windows 2022/2025 full 各 26 门禁、Ubuntu Python 3.14t 16 门禁全部通过 |
| Windows 开发机 SSH | 候选 wheel/c3 哈希与 CI 收据一致；Win11 x64 的代码完整性策略在原生模块导入时阻止加载（事件 3077/3033），断连用例与安装包 smoke 未运行，不计为通过 |

前两行在升级 FastDB 前完成，所测传输源码此后未变；最终新组合由本机消费测试及本轮 CI 绑定。[Source Validation 39569384042](https://github.com/Dsssyc/c-two/actions/runs/39569384042) 的确切候选与分平台结果在机器记录中保存。四组 full 均实际执行完整 relay、Rust/Python/TypeScript 互通、9 行内存矩阵和安装包消费；Windows 普通用户消费的 administrator 字段为 false，进程与临时目录收尾确认成功。

本机为 macOS 27.0.1 arm64 / Rust 1.91.0。为缩小产物关闭 debug 后，本机工具链 strip 引发 LINKEDIT 对齐加载问题；仅隔离构建给 native crate 设置 `strip=none`，保留正常 wheel 修补，源码与 CI 配置无此分支。参见 [Rust 工具链问题](https://github.com/rust-lang/rust/issues/157750)。

## 审查、增量与清理

连接初审由新版 hey-my-buddy 执行，run `8b201401-b90b-4f71-8d1e-f95308348a98`；两项 P2 修正后，复审 run `4c466140-81c1-44fa-bc7a-df1914254899` 对 `7e0f1cac481bb9a7b85daaed11a4e7a7d9c1827b` 未发现阻塞项。FastDB 独立审查 run `eb5b1e0a-598c-425e-ad84-b48c010a4d04` 对 `9e8285c185c428daf0a1feef8d5e264cadc6c0ca` 认可实现进入 CI，指出两份当前开发指南遗漏；Host 已在最终候选同步。CI 修复独立审查 run `e4a7a46d-68a4-41e7-802f-18e8e2244245` 对 `6340dadcc49785d076ed2613e25e58f9a70ee035` 未发现阻塞项，和最终 CI 候选具有相同实现树。审查与实际运行结果分别记录，相关 Buddy 交付已验收。

以 A 开始时已核对 SHA256 的工作树为基线，排除先前 B 的实现和报告；Rust `cfg(test)` 内容计入测试。确切实现候选：生产 `+955/-83，净 +872`，测试 `+1571/-148，净 +1423`，构建/工具/锁文件 `+82/-74，净 +8`，文档 `+145/-15，净 +130`。其中 FastDB 子项没有生产运行时变动，测试净 +20、构建净 +8、文档净 +25。本报告和机器验收记录另计。

本机运行使用独立源码导出，仅替换本次默认 socket/coordination 命名空间，保留逐文件源码哈希及例外清单。macOS/Linux 最后检查均为 socket 0、lease 0；正常 gate/marker 保留。早先失败夹具的 1 对本次端点在确认所属进程退出后，经原生 inspect/credential/reap 精确回收，未手工 unlink。历史目录未作为测试或清理对象。

本机已清理编译 target（含 Rust SDK 嵌套测试 target）、wheel、虚拟环境和 CoreSDK 产物，Linux 本次容器已移除，保留日志和收据。Windows 任务环境在保留失败与系统事件证据后清理，未更改应用控制策略。任务环境原 pytest 9.1.1 安装只有元数据，已按 CI 的 9.0.3 重装以排除测试器缺失；之后才明确遇到原生模块的系统策略拒绝，不将这两种启动失败称为运行时回归。

当前本地分支、HEAD、标签未改变；连接通知与 FastDB 仅推送授权的 fork 验证分支，无发版。Dependabot PR #187 的修复与合并单独验收，不将此功能候选同步到上游 main。新的 OS 原因保留建议见 [独立方案](2026-10-21-local-io-cause.md)，尚未实施。
