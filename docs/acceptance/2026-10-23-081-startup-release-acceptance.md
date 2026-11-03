# 0.8.1 启动期限与错误分类发布验收

[C-Two 0.8.1](https://pypi.org/project/c-two/0.8.1/) 已发布。发行源码为 `ff256e6a7538a8a0a81dcd19dfc640cce40f48a3`，来自 [PR #193](https://github.com/world-in-progress/c-two/pull/193)；其树与最终 PR 候选 `549ac8c82c0f28bfd5cf355d0d80bd161eed4c73` 完全一致。生产实现提交为 `88b8d88db1ee5809963cf6784cfef19446c49b29`，随后只修正测试与文档。本验收文档提交不是发行产物来源。c3 保持 0.4.0，FastDB 保持 0.2.3，没有重复发布 CLI、Rust crate 或 npm 包。

Python 现在可在注册前调用 `cc.set_server(startup_timeout=30.0)`，经 RuntimeSession 配置 Core 的原生启动期限。缺省仍为有限的 5 秒，显式 Rust HostOptions 优先；首次启动尝试冻结设置，失败也冻结，完整 shutdown 后可重新配置。启动超时提供 `category='lifecycle'`、`lifecycle_kind='startup_timeout'`、`startup_stage` 和 `timeout_seconds`；bind 失败保留结构化 OS 原因，移除错误的 `config error` 标签。真正的配置错误仍单独拒绝。具体前后行为和低层 Rust API 迁移见 [发布说明](../releases/0.8.1.md)。

IPC 12、SHM 3、C/FFI ABI 5、owner-control 1、业务帧与 CRM 哈希保持。没有 Python 计时器、环境变量测试逃逸入口或旧分类兼容层。预算覆盖原生 readiness 和 ping，owner 启动围栏也使用配置期限；之后的原生关闭屏障仍须完成，因此整个注册返回耗时可以超过等待预算。

## 基线与验证

修复前基线为 `e11a67915b53d0767ebb98e29d69853609cc01d5`。先加入仅在 `cfg(test)` 生效的真实 Host 启动屏障，默认等待在约 5.013 秒、显式 20 毫秒在约 22.6 毫秒到期，均复现错误的配置标签。Python 基线有 3 个预期失败：不接受新参数，以及真实权限不足和非目录 bind 仍带配置标签；原有 OS 字段断言通过。人工延迟证明调用链和期限，不证明下游运行器偶发拥塞的具体来源。

macOS 定向 Python 回归 101 项通过，6 项既有 Windows 专属场景按平台条件跳过；Rust 覆盖 Runtime 值继承、显式 HostOptions 优先、配置冻结、owner 围栏超时穿过 watch、readiness 与 ping 不重启预算。后续测试替身修正的 17 项、端点维护全文件 59 项通过。

Windows 11 x64 开发机使用生产提交 `88b8d88d`，Python 3.12.13、Rust 1.99.1、Node 22.12：启动/OS 原因/连接观察/所有者 28 项通过、5 项既有 Unix 条件跳过；Rust startup 4、完整 Rust relay 411、Python relay 35、portable 25 项通过，并保留精确 18 行 direct/relay 收据。26 份证据复制后逐一核对大小和 SHA-256。其生产代码与最终发行一致；后续测试修正由完整 CI 验证。

| 门禁 | 源码 | 终态 |
| --- | --- | --- |
| [最终 PR Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39846362470) | `549ac8c8` | 10/10 job 成功 |
| [主线 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39851100591) | `ff256e6a` | 10/10 job 成功 |
| [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/39856667306) | `ff256e6a` | 38 job 成功，1 个既定 sdist 重复构建分支跳过 |
| [Python 发布预演](https://github.com/world-in-progress/c-two/actions/runs/39860498395) | `ff256e6a` | 成功，按 dry-run 设计不上传 |
| [Python 正式发布](https://github.com/world-in-progress/c-two/actions/runs/39861186924) | `ff256e6a` | 校验与上传 2/2 job 成功 |

两次完整 Source Validation 覆盖 Linux、macOS、Windows 2022/2025、原生 Windows ARM64 与 Linux Python 3.14t，包含完整 relay、真实 Rust/Python/Node 调用和生命周期验证。发行矩阵包含 35 个 wheel、1 个 sdist、6 个 CLI 候选、2 个安装器、9 个 FastDB 证明文件及 56 份收据；本次只晋升 Python 包。sdist 按既有设计仅在 Linux x64 生成一次，Linux ARM64 的重复构建分支不执行，不是跳过运行时测试。

首轮 [PR 门禁](https://github.com/world-in-progress/c-two/actions/runs/39840936702) 失败记录保留。`FailingSwapSession` 缺少新投影导致六平台同一测试提前失败，仅补 `startup_timeout=None`，原断言不变。macOS 另一次 inspect 只记录 `io-error`，未记录具体 errno；受控持有新建私有根的 gate 时，0.8.0 和候选均复现 `WouldBlock`，不能反推最初 CI 原因。该 Unix 测试复用已有子进程助手隔离根目录，原 10 个断言及 Windows 分支保留；模拟共享 gate 竞争后通过且零残留。另一个 client_modes 旧夹具将自定义根与默认根混用，已在基线复现，统一原生端点上下文并显式 close 后原 15 项通过。没有生产重试或断言放宽。

## 发布、审查与增量

候选清单 SHA-256 为 `175c9b5ecc0b2d2f05954887e1c4a39cd9990549c579ccc09868455016ebb6c1`。发布复用这些已验证字节。PyPI 全部 36 个文件的名称、大小和 SHA-256 与正式晋升计划一致，均未 yanked；Windows ARM64 为 CPython 3.11、3.12、3.13、3.14、3.14t 共 5 个 wheel。macOS ARM64 在源码目录外的全新环境从官方 PyPI 安装，确认导入来自该环境；零期限结构化超时、30 秒配置后真实 IPC、重复 bind 保留 OS 原因并保持原 listener 存活、完整 shutdown 及 socket/lease 零残留均通过。普通导入未加载 FastDB、NumPy 或 PyArrow。

Buddy v0.50 完成设计、实现及两次测试修正的独立只读审查，无未解决阻断。审查没有代替执行测试；上述运行结果来自主任务、开发机与 CI 的实际执行。

| 范围（基线至最终候选） | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 190 | 47 | +143 |
| 测试 | 396 | 26 | +370 |
| 文档 | 116 | 15 | +101 |
| 版本与锁文件 | 2 | 2 | 0 |
| 合计 | 704 | 90 | +614 |

Rust 内嵌 `cfg(test)`、启动屏障与仅供测试的依赖计入测试；生产文件中的 API 注释计入生产。表格不包含后续纯发布验收记录；本轮是修复和配置投影，没有性能提升声明。R16 以及两份控制会话重构均未实施。

## 清理

Windows 确认任务进程为零并保留证据后，移除本次 D 盘验证树，逻辑大小 4,199,503,023 字节，工具链保留。本机分阶段删除候选 Cargo target、旧版本比较环境、临时安装及 SDK 库；最终阶段另移除 284,800,453 字节。验证 worktree 已归档，Buddy 审查 checkout 已回收，日志与收据保留。

本轮私有根零 socket/lease 残留后才删除目录。早期夹具失败留下的一个默认根测试端点，按地址、时间、退出进程与摘要确认归属后，经原生 inspect/reap 精确回收；没有批量清理历史记录。未承诺默认目录元数据完全未变，因为原生测试仍正常 bind/close。后续仓库同步与验收记录不改变已发布包的源码来源。

结构化来源、文件摘要与验收结果见 [evidence](evidence/2026-10-23-081-startup-release-acceptance.json)。
