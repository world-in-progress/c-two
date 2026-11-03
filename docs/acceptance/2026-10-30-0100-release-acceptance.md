# 0.10.0 发布验收

[C-Two 0.10.0](https://pypi.org/project/c-two/0.10.0/) 与 [c3 0.6.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.6.0) 已于 2026-10-30 发布。发行来源 `4b1d91610fd1a0745cea0b5bb841ab0928ed723b` 是 [PR #203](https://github.com/world-in-progress/c-two/pull/203) 的合并提交，内容树与 PR head `cceecc65` 相同；C-Two 0.10.0 依赖 FastDB 0.2.4。发版分两次进行：2026-10-29 从 `4c3e587d`（[PR #202](https://github.com/world-in-progress/c-two/pull/202) 的合并提交）开始的第一次尝试，在晋升预演时被拒，没有上传任何文件，随后因 FastDB 发现内存泄漏而暂停；修掉暴露的问题并升级到 FastDB 0.2.4 后，从新的 main 重新资格验证并发布。两次的晋升都只认 canonical `main` 上同一确切来源的完整 Source Validation，Python 与 CLI 的预演和正式晋升共用同一次 Release Candidate 的已验证字节。

## 第一次尝试（2026-10-29，未发布）

| 阶段 | 实际结果 |
| --- | --- |
| [PR #202 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/40843023208) | 来源 `6747bf73`，首轮 10/10 job 成功。 |
| [main push Source Validation](https://github.com/world-in-progress/c-two/actions/runs/40857187478) | 来源 `4c3e587d`。首轮 Windows 2025 与 Windows 11 ARM64 两个 source job 失败，两个汇总 job 随之失败；只重跑失败的 job，第 2 次尝试 10/10 成功。 |
| [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/40870564100) | 绑定上一行的运行；38 job 成功，1 个既定的 Linux ARM64 重复 sdist 分支跳过。 |
| [CLI 预演](https://github.com/world-in-progress/c-two/actions/runs/40874071148) / [Python 预演](https://github.com/world-in-progress/c-two/actions/runs/40874076753) | 均在核验候选时拒绝，没有上传任何文件。 |
| [main 完整资格验证](https://github.com/world-in-progress/c-two/actions/runs/40874415766) | 手动 `force_full`，来源 `4c3e587d`，首轮 10/10 job 成功；随后发版暂停，未用它构建候选。 |

预演拒绝候选，是因为重跑让两条线在同一运行里各留下两份同名的完整产物，而晋升核验要求绑定的运行每条线恰好一份；候选构建的入口检查只读运行与 job 元数据，接受了这次运行。按 [CI 说明](../reference/ci-cd.md)，失败的候选需要新的资格验证而不是绕过：首轮产物没有删除，门禁没有放宽，而是在同一来源上重新跑了一次完整的 Source Validation。

## 发版修复与 FastDB 0.2.4

暂停期间修掉第一次尝试暴露的三处问题，并在 FastDB 0.2.4 发布后升级，经 PR #203 合入：

- 发布工具（`60059bb3`）：候选构建入口与晋升核验共用 `require_full_source_artifacts`，入口也要求每条线恰好一份完整产物，跳过重复产物的运行，并说明要重新发起完整运行而不是重跑。`1b4a9701` 顺带改正其报错：不再提 rc-context.json，0 份说缺失或过期，多于 1 份才归因于重跑。
- Windows 2025 失败的 `c2-codegen` `generated_call_options_consumer`（`f1c6a97e`）：zero-bytes 一段在有期限调用返回后立即断言 `used_operations` 为 0，而 Core 按设计在唤醒调用方之后才归还 permit。测试改为有界等待，生产代码不改；在 `guard.finish(result)` 之后临时插入 50 ms 暂停，原测试在与 CI 相同的位置失败，修复后的测试加不加暂停都通过。
- Windows 11 ARM64 失败的 `test_powershell_selects_native_os_target`（`b0331f6f`、`6ff74cb7`）：失败的是 SysWOW64 参数，即 ARM64 上模拟运行的 x86 Windows PowerShell，30 秒的 pytest 限时先于子进程限时触发。ARM64 上三个宿主在不同运行之间相差约 4 倍（SysWOW64 最慢 49.9 秒），三个宿主改为共用只防挂死的限时：子进程 180 秒、pytest 210 秒；安装脚本不改。
- FastDB 0.2.4（`1b4a9701`，源码 `f82fa13adeae2b87a295bf984821c85c44b55a3d`）修复独立 FastDB 数据库、数值列缓存与映射存储的内存释放，portable payload 的 117 个符号 ABI 不变。Python extra、两处 Rust 精确锁定、CLI 与原生扩展静态链接的 `fastdb-sys` 源码 rev、Core SDK manifest 固定值、CI 与发布工具常量一起改到 0.2.4，五份 lockfile 只变 FastDB 条目。

fork 上的完整 CI [40900123908](https://github.com/Dsssyc/c-two/actions/runs/40900123908)（`53881d45`）、[40913057564](https://github.com/Dsssyc/c-two/actions/runs/40913057564)（`6ff74cb7`）与 [41002689847](https://github.com/Dsssyc/c-two/actions/runs/41002689847)（`1b4a9701`）都首轮 10/10 通过，每条线一份完整产物；最后一次的六条源码线都用了各自平台的 0.2.4 Core SDK 与 `fastdb4py==0.2.4`。本机核对与逐项证据见[精简批次记录](../design/2026-10-13-local-slimming-batches.md)。

## 发行门禁（2026-10-30）

| 阶段 | 实际结果 |
| --- | --- |
| [PR #203 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41008027942) | 来源 `cceecc65`，首轮 10/10 job 成功。 |
| [main push Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41010698934) | 来源 `4b1d9161`，首轮 10/10 job 成功，每条线恰好一份完整产物。 |
| [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/41013847454) | 绑定上一行的运行；38 job 成功，1 个既定的 Linux ARM64 重复 sdist 分支跳过。 |
| [CLI 预演](https://github.com/world-in-progress/c-two/actions/runs/41016119599) / [Python 预演](https://github.com/world-in-progress/c-two/actions/runs/41016124301) | 两份清单均验证成功，不上传。 |
| [CLI 发布](https://github.com/world-in-progress/c-two/actions/runs/41016376334) / [Python 发布](https://github.com/world-in-progress/c-two/actions/runs/41016379589) | 两条流程成功，直接晋升上述候选字节；PyPI 环境按用户的发版授权批准。 |

两份预演清单都绑定来源 `4b1d9161`、候选 41013847454 与正式资格验证 41010698934，版本为 C-Two 0.10.0、c3 0.6.0、FastDB 0.2.4（`f82fa13a`）。

## 发布核对

发行清单 SHA-256 为 `bbc0937cbeb503b1ab666b824a9003bbbaeaf9f2fe350ada3635536441dab2b2`，与 release 上 `rc-manifest.json` 的摘要一致。PyPI 全部 35 个 wheel、1 个 sdist 和 GitHub 全部 17 个附件的名称、大小、SHA-256 均与预演清单一致，没有 yanked 文件。Windows ARM64 的 cp311、cp312、cp313、cp314、cp314t 五个 wheel 齐全。`c3-v0.6.0` 标签指向发行来源 `4b1d9161`。发布后另做冒烟：在 macOS ARM64 的 CPython 3.13 上从 PyPI 安装 `c-two[fastdb]==0.10.0`，带入 `fastdb4py` 0.2.4，注册并调用一个同进程 CRM；从 release 下载 macOS ARM64 的 c3，SHA-256 校验通过，`--version` 为 0.6.0。结构化记录见 [evidence](evidence/2026-10-30-0100-release-acceptance.json)。

## 清理

fork 上的 `socu/0100-release-fixes`、`socu/0100-release-fixes-ci` 与 Codex 时期留下的 `socu/r15-expiry-gc-ci`、`socu/076-ci-release-gates` 都已包含在 main 中，按确切 SHA 删除；fork 上 CI 分支的 Actions 缓存一并删除。上游两个 Dependabot 分支按确切 SHA 删除：对应的 [PR #200](https://github.com/world-in-progress/c-two/pull/200) 已合入，[PR #201](https://github.com/world-in-progress/c-two/pull/201) 已关闭、被 #202 取代。本轮的本地工作树、已验收的 Buddy checkout 与构建产物均已清理，旧的 `slimming-batches` worktree 没有未提交内容，随其分支一起删除。第一次尝试的候选及其两次预演留在 Actions 记录里，不用于发布。本记录合入后删除本记录分支。
