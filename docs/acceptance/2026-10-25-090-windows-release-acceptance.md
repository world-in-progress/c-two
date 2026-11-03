# 0.9.0 Windows 用户作用域与发布验收

[C-Two 0.9.0](https://pypi.org/project/c-two/0.9.0/) 与 [c3 0.5.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.5.0) 已发布。发行来源为 `3f223d658117511073ae549565ad2c49af163a87`，来自 [PR #197](https://github.com/world-in-progress/c-two/pull/197)，其内容树与候选 `0e63094ab2ef9add7e9d4dd0e6c4873d09a7e175` 相同。生产实现冻结在 `b9efb2d9706a7563ddaacfe2db2dc2d953c8b86e`；后续候选只追加文档并清理一行测试空行。FastDB 保持 0.2.3；本验收的后续纯文档提交不是发行产物来源。

## 行为与证据边界

Windows 端点、DACL、listener lease 与属主上下文统一绑定 TokenUser；真实连接句柄在发送之前核对管道对象属主、服务端 TokenUser 和至少 Medium 的完整性。跨 TokenSessionId 的连接复用 owned/inline/chunked 通路，双方不发布不可访问的 SHM 引用。IPC 13、C/FFI ABI 6 是本次明确的协议切换；SHM 3、owner-control 1、业务帧与 CRM 哈希保持。迁移要求见 [0.9.0 发布说明](../releases/0.9.0.md)。

实际 Windows 11 开发机证据区分同用户不同 SSH 登录、同用户不同会话号与不同本地账户。用户自行完成并提交第二账户抢注记录，Host 未重新执行该实验。已核实的候选记录证明 Python connect/ping/shutdown、Rust connect/owner inspect 以及补充 Node 连接在异属主管道上零应用字节退出；正牌先监听和关闭后重启的结果与既有生命周期回归结合验收。主实验里的 Node 模块加载失败不计通过，只使用修正后的补充记录；Rust 的 route-not-found 记录只证明连接与握手，不冒充成功业务调用。

原始提交证据含 168 个文件，清单 SHA-256 为 `c9eb262fc57d0dc55c06bfc8343d31cd4bce3c18d06bdd813f1c4ea35c030513`。原始票据及个人账户资料不入库。历史 Python 原生构建来源为 `aa9a5bb56359bc71e1c51a45e1bbaa6edf2d1e7b`，测试来源为 `6b2b176217dc9177f4fbe05dddc838c8d54deb30`；两者生产及 Python/Rust SDK 内容一致，差异仅在五个 TypeScript 测试/夹具文件。保留原始构建日志、实际加载文件哈希与 Node 包字节对照；后补采集的哈希不表述为编译时签名证明。

第二账户记录暴露的属主错误分类已修复：原生 I/O 链保存有类型的身份拒绝，inspect 返回 `identity_mismatch`，发送前 commit 保留 `not_sent` 和该原因。普通权限不足和进程退出不自动归为身份不符。Node 区分 `IDENTITY_MISMATCH`、`PERMISSION_DENIED` 和 `IO_ERROR`，沿用原异常类，没有新增 JavaScript `.code` 或 `.status` 属性。

## 已执行验证

| 来源 | 结果与适用范围 |
| --- | --- |
| `6b2b1762`，[完整 fork Source Validation](https://github.com/Dsssyc/c-two/actions/runs/40088986489) | 10 个 job 成功，含 Linux、macOS、Windows 2022/2025、原生 Windows ARM64、Python 3.14t；完整 relay 和跨语言证据保留。 |
| `b9efb2d9`，macOS 与 Windows 11 开发机 | owner 公共 inspect/commit、C/FFI 分类、手写 C header golden、真实 Node addon 错误投影通过；Windows 原生扩展重建与导入通过。 |
| `b9efb2d9`，[针对性 Actions](https://github.com/Dsssyc/c-two/actions/runs/40112888982) | Linux x64、Windows ARM64 各六项通过，下载产物摘要与收据源码均已核对。工作流包装提交 `93a07c3f` 显式检出并验证 `b9efb2d9`。 |
| owner 反例 | 在实际 candidate 连接边界恢复旧 `map_err(unavailable)` 后，公共调用测试以 Unavailable/IdentityMismatch 不符失败；恢复修复后通过，任务端点零残留。 |
| R06 | 四个基线受控竞态反例失败，修复后 native payload 8 项及真实 Python 失败/幂等用例通过；生产只复用已有 inner/lease 锁。完整平台结果包含于上述工程门禁。 |

R06 的同环境 ABBA 测量使用真实生产扩展、每轮九个样本。32/4096 字节 request/response view 的中位数约由 103–104 ns 变为 100–101 ns；IPC 中位数约为 61.3→56.2 µs 和 119.6→119.4 µs。未观察到开销退化，这些结果不构成普适加速承诺；未增加 payload 复制、线程或 timer。回调重入和 R07 不在本批。

独立审查分别覆盖 R06、Windows 实现、用户提交证据、构建来源和最终错误传播。错误传播审查指出最初 helper-only 测试不足，Host 补成公共 inspect/commit 调用并完成实际旧映射反例。最后一次发布门禁审查通过 hey-my-buddy run `e21b76d5-e02e-470a-8e47-df4ae847d280` 交付，固定产物 `4260880c-c1e3-4549-b608-a4447b83e1ac` 已验收、checkout 已回收；该只读审查不代替发行运行。

身份错误收尾相对 `6b2b1762` 的实际净量：生产 +40 行、测试 +149 行、文档 +4 行，合计 +193 行；临时 CI 包装的 97 行不进入发行源码。本次追加的验收文档及末尾空行清理另计，不作为运行时收益。

## 开发机清理

Windows 重新启动时间为 `2026-10-24T17:04:10.4182720Z`，已与重启前记录区分。五个指定临时账户此前已删除；重启后五个 profile 均为未加载，通过 Windows 原生 profile 删除接口逐一删除，对应目录、账户和任务均不存在。未强卸注册表或手工删除用户 profile 目录。

在确认无本批活动进程后，删除任务 Cargo target、Node build/dist/node_modules、Python 原生文件、隔离基线环境、下载缓存和临时 SDK。随后独立扫描确认本批源码目录无剩余 target 或编译文件，Git 跟踪内容保持 `b9efb2d9`。按清理前清单可确证的文件长度至少 6,303,635,896 字节；这不是磁盘空闲量的精确变化。Rust 工具链、源码、构建日志和验收收据保留，历史 IPC 目录未触碰。

清理脚本第一次在删除后的统计阶段遇到 PowerShell hashtable 的 Measure-Object 错误；删除动作已执行，后续独立存在性、Git 和二进制扫描均确认结果，错误日志保留，不把该次非零退出标成通过。

## 发行门禁

既有发布器要求 canonical `main` 上同一确切来源的完整 Source Validation，不接受 fork、祖先提交或跨运行拼接。用户明确授权这一轮必要的完整发行门禁。上游 main push 的运行直接复用，没有追加同源 dispatch；只构建一次 Release Candidate，Python 与 CLI 的演练及正式晋升共用该 run 的已验证字节。

| 阶段 | 实际结果 |
| --- | --- |
| [PR Source Validation](https://github.com/world-in-progress/c-two/actions/runs/40157625282) | 最终 10/10 job 成功；仅对失败的 macOS job 复核一次，其他 source 结果复用。 |
| [上游 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/40161591075) | 来源 `3f223d65`，首轮 10/10 job 成功。 |
| [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/40164835157) | 38 job 成功，1 个既定的 Linux ARM64 重复 sdist 分支跳过。 |
| [Python 预演](https://github.com/world-in-progress/c-two/actions/runs/40165944800) / [CLI 预演](https://github.com/world-in-progress/c-two/actions/runs/40165942688) | 两份清单均成功验证，不上传。 |
| [Python 发布](https://github.com/world-in-progress/c-two/actions/runs/40166223230) / [CLI 发布](https://github.com/world-in-progress/c-two/actions/runs/40166221456) | 两条流程成功，直接晋升上述候选字节。 |

PR 首轮 macOS 的既有 `shutdown_join_retires_socket_after_namespace_gate_contention` 测试在两秒关闭探测处超时，server 组其余 184 项及后续 Python/relay/跨语言检查通过。与 0.8.2 比较，该测试、Server run/drain/close 函数和 Unix listener 回收实现逐字节相同。隔离单项及诊断循环未重现；[macOS 15 独立诊断](https://github.com/Dsssyc/c-two/actions/runs/40160295344) 的原 server 组十轮、每轮 185 项均通过。这些结果不证明根因已修复。Host 没有修改生产代码、期限或断言；原失败的 macOS job 单独复核一次通过，随后 canonical main 首轮完整通过。原超时保持未定位记录，未以猜测补丁或略过测试处理。

发行清单 SHA-256 为 `08ba971cabcfdde421629f41f4b79d6f689afebbbbe213eb43de374bcf6b657c`。PyPI 全部 35 个 wheel、1 个 sdist 和 GitHub 全部 17 个附件的名称、大小、SHA-256 均与预演清单一致；没有 yanked 文件。Windows ARM64 的 cp311、cp312、cp313、cp314、cp314t 五个 wheel 齐全。实际 `c3-v0.5.0` 标签指向发行来源 `3f223d65`。第一次 PyPI JSON 读取显示上传期间的 23 文件视图，刷新后得到完整 36 文件并逐一匹配摘要；没有重新上传。

## 发行范围的实际增量

下表绑定 0.8.2 主线 `548710a8` 至发行内容树，Rust cfg(test) 字段、语句和模块按测试分类，生产文件内的 API 注释计入生产。后续验收文档另计。

| 范围 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 1684 | 554 | +1130 |
| 测试与测试入口 | 2584 | 602 | +1982 |
| 文档 | 631 | 69 | +562 |
| 版本与依赖清单 | 9 | 9 | 0 |
| 合计 | 4908 | 1234 | +3674 |

Windows 用户作用域与身份修复子项生产净增 1455 行、测试净增 1919 行、文档净增 81 行。R06 子项生产净增 4 行、测试净增 236 行、文档净增 61 行；它们是功能与正确性增量，其他已验收的 Python 退役和 inline 移交按各自记录统计。结构化来源、门禁与发布核对见 [evidence](evidence/2026-10-25-090-windows-release-acceptance.json)。

本轮临时 profile、编译产物及已验收 Buddy checkout 均已清理。应用拒绝归档另一个任务拥有的旧 `slimming-batches` worktree，保留其源码和 `socu/076-ci-release-gates` 分支。本验收合入后同步本地、fork 与 upstream，并清理本轮已结束的验证分支；同步不改变已发布包的来源。
