# 0.12.0 与 c3 0.8.0 发布验收

[C-Two 0.12.0](https://pypi.org/project/c-two/0.12.0/) 与配套原生 CLI [c3 0.8.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.8.0) 已于 2026-11-23（UTC）协同发布。两者共同依赖不可变 FastDB 0.3.0（ABI-118，原生源码 `ff097cca9c29cadc289e2a437f4720ecc56015c4`），源码构建要求 Rust 1.95 或更新版本；Rust SDK、Core crates 与 Node binding 各自保持独立 0.1.0 源码版本，不发布至 crates.io 或 npm。

发行来源 `f388cc1405ccf36741239ddda6bdeb6ee2e1d408` 是 [PR #238](https://github.com/world-in-progress/c-two/pull/238) 的普通合并提交，内容树 `1803a0d2aa0bde7384b7df77732e322d87ecada7` 与 PR head `b58c47ba56329daccdbfbecd8f5b083daff5b4c0` 相同。

## 纳入本版的主要批次

本版在 [C-Two 0.11.0 / c3 0.7.0](../releases/0.11.0.md)（`50be9a79`）之上将协议与依赖协同推进至 **IPC 17、SHM 4、C/FFI ABI 9、relay mesh 7 与 FastDB 0.3.0（ABI-118）**（Unix 命名空间磁盘格式 3、Unix 凭据 schema 4 与 `C2E1` v1 错误线格式不变）：

| 批次 | 主要内容 | 合入与验收记录 |
| --- | --- | --- |
| `RuntimeSession` 锁顺序加固与常驻模拟范式 | 在原生锁外释放 Python 对象与投影异常、动态分叉路由压测及 `resident_simulation.py` 示例 | [PR #234](https://github.com/world-in-progress/c-two/pull/234)（`25fe5eb1`）、[Source Validation 44755886087](https://github.com/world-in-progress/c-two/actions/runs/44755886087) |
| 可复现可移植载荷、流式与 Ray 基准基线 | 覆盖小包耦合、全场网格、多订阅者扇出与百万行 Kostya 的可复现对照套件 | [PR #235](https://github.com/world-in-progress/c-two/pull/235)（`c3067aab`）、[Source Validation 44853675128](https://github.com/world-in-progress/c-two/actions/runs/44853675128)、[基准记录](2026-11-20-portable-payload-benchmark.md) |
| 资源归属发布/订阅、IPC 17 与 C/FFI ABI 9 | [ADR 0005](../decisions/0005-publish-subscribe.md) 主题契约与 `Registration`、同机封存多借出 SHM 4 扇出、单跳/两级 `c3 relay` 扇出（保持 mesh 7）、`c2-codegen` 三语言主题生成、Rust ↔ Python 双向端到端与 Node ABI 9 订阅 | [PR #236](https://github.com/world-in-progress/c-two/pull/236)（`cc2bb65d`）、[Source Validation 44900009786](https://github.com/world-in-progress/c-two/actions/runs/44900009786)、[扇出基准](2026-11-21-pubsub-fanout-benchmark.md) |
| 统一 `WritableSlab` 直写 SHM、FastDB 0.3.0 零拷贝与 KB–GB 性能收口 | 四条发送路径直写 `WritableSlab` + `ExternalBytes` 零拷贝解码（单向 SHM 拷贝下界降至 2 次）、`4 × 64 MiB` buddy 几何与温 `RecycledBytes` 缓冲池、KB 级固定开销削减、以及 `1 KiB–1 GiB` + `4 GiB` 受限内存流式迁移基准 | [PR #237](https://github.com/world-in-progress/c-two/pull/237)（`1652f7c6`）、[Source Validation 45007713034](https://github.com/world-in-progress/c-two/actions/runs/45007713034)、[KB–GB 基准](2026-11-22-kb-to-gb-slab-benchmark.md) |
| 发版准备与全仓文档对齐 | 升级 `c-two` 至 `0.12.0`、`c2-cli` 至 `0.8.0`（零无关依赖漂移），整合 `docs/releases/0.12.0.md`、`CHANGELOG.md`、README 及全部参考文档 | [PR #238](https://github.com/world-in-progress/c-two/pull/238)（`f388cc14`，head `b58c47ba`） |

## 发行门禁（2026-11-22 / 2026-11-23 UTC）

| 阶段 | 运行记录 | 实际结果 |
| --- | --- | --- |
| PR #238 四平台 `Source Validation` | [45028460999](https://github.com/world-in-progress/c-two/actions/runs/45028460999) | 来源 `b58c47ba`，树 `1803a0d2`；9/9 全部通过（Linux x64 Python 3.12 + 3.10 + 3.14t、Windows Server 2025 x64、Windows 11 ARM64、`workflow policy` 首轮通过；`macos-15` 首轮因 runner 容量波动在编译期被平台取消，仅 `--failed` 重跑 `macos-15` 一次通过）。 |
| `main` 推送 `Source Validation`（晋升选用证据） | [45035920237](https://github.com/world-in-progress/c-two/actions/runs/45035920237) | 来源 `f388cc14`，树 `1803a0d2`；9/9 全部第 1 次尝试通过（含全部四平台源码门禁与 `workflow policy`，约 43 分钟），被 `cli-release.yml` 与 `python-package-release.yml` 晋升计划共同选为发布源码证据。 |
| `Release Candidate` | [45035990495](https://github.com/world-in-progress/c-two/actions/runs/45035990495) | 合入后在 `main` 精确提交 `f388cc14` 触发；38 个执行作业全部首轮通过、1 个既定的 `linux (aarch64-unknown-linux-gnu) / sdist` 跳过，约 17 分钟，产出 35 wheels + 1 sdist + 6 `c3` 二进制 + 2 安装脚本 + 56 份校验收据与 `rc-manifest.json`。 |
| `CLI Release`（`c3-v0.8.0`） | [45042114076](https://github.com/world-in-progress/c-two/actions/runs/45042114076) | 绑定候选 `45035990495` 与源码证据 `45035920237`；核对 17/17 资产上传计划后经 `github-release` 环境审批发布，发布作业 11 秒完成（`2026-11-23T01:18:48Z`）。 |
| `Python Package Release`（`c-two 0.12.0`） | [45042118341](https://github.com/world-in-progress/c-two/actions/runs/45042118341) | 绑定同一候选 `45035990495` 与源码证据 `45035920237`；核对 36/36 文件上传计划后经 `pypi` 环境审批发布，发布作业 1 分 30 秒完成（`2026-11-23T01:20:11Z` 上传完毕，`01:20:15Z` 作业结束）。 |

## 发布后核对与真机冒烟

- **候选清单与 GitHub Release `c3-v0.8.0`：** 发布的 `rc-manifest.json` SHA-256 为 `692297bff0924e9015e51a5fabed3de2e85700432ce58539fa0c9a4655b4554c`，与晋升计划完全一致；标签 `c3-v0.8.0` 指向 `f388cc1405ccf36741239ddda6bdeb6ee2e1d408`（非草稿、非预发布）；全部 17 个资产（6 平台二进制 + 6 个 `.sha256` + `c3-installer.sh` + `c3-installer.ps1` + `fastdb-LICENSE` + `fastdb-THIRD_PARTY_NOTICES.txt` + `rc-manifest.json`）的名称、字节大小与 SHA-256 逐一吻合。
- **PyPI `c-two==0.12.0`：** 查询 PyPI JSON 接口确认 36 个文件齐全（35 个 wheel，含 5 个 Windows ARM64 wheel，以及 1 个 sdist），0 个 yanked 文件，全部 36 个文件的字节大小与 SHA-256 与 `rc-manifest.json` 逐一相符。
- **本机真机冒烟（macOS arm64，CPython 3.13.3）：**
  - 从 GitHub Release `c3-v0.8.0` 下载 `c3-aarch64-apple-darwin` 与 `.sha256`，`shasum -a 256 -c` 通过，`c3 --version` 输出 `c3 0.8.0`，`otool -L` 确认无任何动态 `libfastdb` 依赖；`c3 relay --bind 127.0.0.1:0 --dry-run` 正常解析并打印默认角色域预算（16 GiB / 16 GiB / 262144），实测监听 `127.0.0.1:0` 后 `SIGINT` 正常退出（退出码 0）。
  - 在全新隔离临时 venv 中以 `--no-cache` 从 PyPI 安装 `c-two[fastdb]==0.12.0`（自动拉入 `fastdb4py==0.3.0`），校验 `c_two.__version__ == '0.12.0'`，通过跨独立 `cc.Runtime()` 客户端经本地 IPC 17 端到端跑通：FastDB `BuildPlan` 直写 SHM + 零拷贝 `Payload` 解码的 unary 调用、`cc.hold`、`@cc.stream` + `close()`、`@cc.topic`（`Registration`、`initial_topics`、`publisher()`、`start='latest'`、`Delivery(value, seq, dropped)` 与 `sub.close()` 回执）、`cc.observe.memory_stats()` 与双端 `shutdown()`（`completed=True`）。所有临时目录用完即删。

结构化验收证据见 [2026-11-23-0120-release-acceptance.json](evidence/2026-11-23-0120-release-acceptance.json)。

## 清理

PR #238 合入后已删除本地工作树 `.worktrees/release-0.12.0` 以及本地与 fork 上的 `socu/release-0.12.0` 分支，并清理了前置批次的 `core/target` 与临时基准构建产物。本验收与收尾文档 PR 合入后删除对应的临时分支与工作树。
