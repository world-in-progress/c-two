# 0.11.0 与 c3 0.7.0 发布验收

[C-Two 0.11.0](https://pypi.org/project/c-two/0.11.0/) 与配套原生 CLI [c3 0.7.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.7.0) 已于 2026-11-20 发布。两者共同依赖 FastDB 0.2.4（原生源码 `f82fa13adeae2b87a295bf984821c85c44b55a3d`），源码构建要求 Rust 1.95 或更新版本；Rust SDK、Core crates 与 Node binding 各自保持独立 0.1.0 源码版本，不发布至 crates.io 或 npm。

发行来源 `50be9a7966d6c8381715809440d69502c3cee5de` 是 [PR #232](https://github.com/world-in-progress/c-two/pull/232) 的普通合并提交，内容树 `8796bd2ef5dacf7c145bdf3e511172b4111ac063` 与 PR head `b413e89d12a65594d28a6599ab4953dfdfc63c15` 相同。本次也是 [PR #208](https://github.com/world-in-progress/c-two/pull/208) 精简发版流程后，`cli-release.yml` 与 `python-package-release.yml` 首次双通道协同发布。

## 纳入本版的主要批次

本版将协议与接口演进合并发布，使 0.10.1 / c3 0.6.0 用户一次性升级到 **IPC 16、SHM 4、C/FFI ABI 8、relay mesh 7**（Unix 命名空间磁盘格式 3、Unix 凭据 schema 4 与 `C2E1` v1 错误线格式不变）：

| 批次 | 主要内容 | 合入与验收记录 |
| --- | --- | --- |
| 接口精简、去 owner 与单一属主释放 | 根模块收口、`endpoint`/`observe`/`contract` 子模块、删除 owner 子系统（[ADR 0002](../decisions/0002-remove-owner-subsystem.md)）、单一属主 SHM 释放（[ADR 0001](../decisions/0001-single-owner-release.md)） | [PR #215](https://github.com/world-in-progress/c-two/pull/215)（`b81405fe`）、[验收记录](2026-11-03-protocol-batch-acceptance.md) |
| 服务端流、IPC 16 与 C/FFI ABI 8 | `@cc.stream` / `ResponseStream` / `C2ResponseStream`、IPC 16 call records、relay HTTP 服务端流与 C/FFI ABI 8 直调 Rust Core 客户端（[ADR 0003](../decisions/0003-server-streams.md)） | [PR #219](https://github.com/world-in-progress/c-two/pull/219)（`a5ab5eaa`）、[阶段 1](2026-11-04-streaming-phase-1-acceptance.md)、[流式验收](2026-11-08-streaming-acceptance.md) |
| 路由名身份 | 客户端跟随路由名、移除客户端 route UID、`RouteConflict`（719）与 `c3 registry force-leave`（[ADR 0004](../decisions/0004-route-name-identity.md)） | [PR #221](https://github.com/world-in-progress/c-two/pull/221)（`cc59ceee`）、[验收记录](2026-11-09-route-identity-acceptance.md) |
| Windows 稳定性与关闭确认围栏 | `mio` 升至 1.2.4 修掉转储证实的 `0xC0000005` 具名管道释放后使用，直连 shutdown ACK 仅在全连接停准入后可见 | [0.11.0 发布说明](../releases/0.11.0.md#notable-fixes-and-performance) |
| 配置与角色容量收敛 | `client`/`server`/`relay` 独立角色域 `(M, F, N)`、13 个 Core 环境变量、对称 `memory_stats()` | [PR #229](https://github.com/world-in-progress/c-two/pull/229)（`4a974c5a`）、[验收记录](2026-11-18-config-convergence-acceptance.md) |
| 错误体系收敛与残留接口收口 | `ErrorCode`、`ResourceExecutionError`、`SerializationError(side, stage)`、`CCBaseError.transport_phase`、`CoreError.semantic`、删除 `c_two.config` 与 `generate_crm_template`、修复 `c2-http` 丢弃流响应窗口归还 | [PR #230](https://github.com/world-in-progress/c-two/pull/230)（`07c56c0f`）、[验收记录](2026-11-20-error-and-api-convergence-acceptance.md) |
| 发版准备与全仓文档对齐 | 升级 `c-two` 至 `0.11.0`、`c2-cli` 至 `0.7.0`（零无关依赖漂移），整合 `docs/releases/0.11.0.md`、`CHANGELOG.md`、README 及全部参考文档 | [PR #232](https://github.com/world-in-progress/c-two/pull/232)（`50be9a79`，head `b413e89d`） |

## 发行门禁（2026-11-20）

全部门禁均一次性首轮通过，无重跑：

| 阶段 | 运行记录 | 实际结果 |
| --- | --- | --- |
| PR #232 四平台 `Source Validation` | [44722552798](https://github.com/world-in-progress/c-two/actions/runs/44722552798) | 来源 `b413e89d`，树 `8796bd2e`；9/9 全部第 1 次尝试通过（Linux x64 Python 3.12 + 3.10 + 3.14t、macOS arm64、Windows Server 2025 x64、Windows 11 ARM64、`workflow policy` 与两项汇总门禁），约 43.6 分钟。 |
| `main` 推送 `Source Validation` | [44729149514](https://github.com/world-in-progress/c-two/actions/runs/44729149514) | 来源 `50be9a79`；命中 PR #232 同树证据，跳过源码矩阵、仅跑 `changes` 与 `workflow policy`，1 分 42 秒通过。 |
| `Release Candidate` | [44729160630](https://github.com/world-in-progress/c-two/actions/runs/44729160630) | 合入后立即触发，来源 `50be9a79`；38 个作业全部成功、1 个既定的 `linux (aarch64-unknown-linux-gnu) / sdist` 跳过，约 24.8 分钟，保留 76 项产物含 `rc-receipt`。 |
| `CLI Release`（`c3-v0.7.0`） | [44732649732](https://github.com/world-in-progress/c-two/actions/runs/44732649732) | 绑定候选 `44729160630` 与同树源码证据 `44722552798`；核对 17/17 资产上传计划后经 `github-release` 环境审批发布，发布作业 9 秒完成（`2026-11-20T12:10:10Z`）。 |
| `Python Package Release`（`c-two 0.11.0`） | [44732655064](https://github.com/world-in-progress/c-two/actions/runs/44732655064) | 绑定同一候选 `44729160630` 与源码证据 `44722552798`；核对 36/36 文件上传计划后经 `pypi` 环境审批发布，发布作业 1 分 30 秒完成（`2026-11-20T12:11:36Z` 上传完毕，`12:12:37Z` 作业结束）。 |

从 PR #232 合入（`11:37:40Z`）到双通道全部发布完成（`12:12:37Z`）共 **35 分钟**（其中 `Release Candidate` 约 25 分钟，双通道准备、审批与发布约 9 分钟）。

## 发布后核对与真机冒烟

- **候选清单与 GitHub Release `c3-v0.7.0`：** 发布的 `rc-manifest.json` SHA-256 为 `87a9d8e2f6e2b7d7a19ff9992eab7ecff2023d90140cb4bcdbb0e84f95e82d08`，与晋升计划完全一致；标签 `c3-v0.7.0` 指向 `50be9a7966d6c8381715809440d69502c3cee5de`（非草稿、非预发布）；全部 17 个资产（6 平台二进制 + 6 个 `.sha256` + `c3-installer.sh` + `c3-installer.ps1` + `fastdb-LICENSE` + `fastdb-THIRD_PARTY_NOTICES.txt` + `rc-manifest.json`）的名称与字节大小逐一吻合。
- **PyPI `c-two==0.11.0`：** 查询 PyPI JSON 接口确认 36 个文件齐全（35 个 wheel，含 5 个 Windows ARM64 wheel，以及 1 个 sdist），0 个 yanked 文件，全部 36 个文件的 SHA-256 与晋升计划逐一相符。
- **本机真机冒烟（macOS arm64，CPython 3.13.3）：**
  - 从 GitHub Release `c3-v0.7.0` 下载 `c3-aarch64-apple-darwin` 与 `.sha256`，`shasum -a 256 -c` 通过，`c3 --version` 输出 `c3 0.7.0`，`c3 relay --bind 127.0.0.1:0 --dry-run` 正常解析并打印默认角色域预算（16 GiB / 16 GiB / 262144）。
  - 在全新隔离临时 venv 中以 `--no-cache` 从 PyPI 安装 `c-two[fastdb]==0.11.0`（自动拉入 `fastdb4py==0.2.4`），校验 `c_two.__version__ == '0.11.0'`，并通过显式 `cc.server_address()` 端到端跑通本地 IPC unary 调用、`cc.hold`、`@cc.stream` + `close()`、`cc.observe.memory_stats()` 与 `cc.shutdown()`（`completed=True`）。所有临时目录用完即删。

结构化验收证据见 [2026-11-20-0110-release-acceptance.json](evidence/2026-11-20-0110-release-acceptance.json)。

## 清理

PR #232 合入后已删除本地工作树 `.worktrees/release-0.11.0` 以及本地与 fork 上的 `socu/release-0.11.0` 分支，并清理了本地 `core/target` 编译产物与历史已归档批次的 CI 下载副本。本验收与收尾文档 PR 合入后删除对应的临时分支与工作树。
