# 发版敏捷性审查

日期：2026-10-30。输入：upstream main `028e791d`（0.10.1 发版提交）。范围：`docs/ci-cd.md`、`.github/workflows/`、`.github/scripts/`、`tools/ci/`、`tools/dev/test_python.py`、`tools/local_rc/`、`tests/repo/`，以及测试与生产代码里和发版相关的机制。状态：用户已于 2026-10-30 批准方案 A（含 A5 取消预演）；方案 A 经 [PR #208](https://github.com/world-in-progress/c-two/pull/208) 合入（`1a69b78a`），0.10.1 是第一个走新流程的版本，从合入到发布完成 32 分钟，见[发布验收](../acceptance/2026-10-30-0101-release-acceptance.md)。第 4 节其余事项仍待决定。

## 结论

- 一个很小的修复，今天从开 PR 到发布，一切顺利也要约 95 分钟（0.10.0 实测）；遇到一次偶发失败就要再加 30–60 分钟。0.10.1 在 09:22 开 PR，到 11:45 已跑了四轮完整验证（两轮 PR、两轮 main），仍未进入候选构建，用户随后暂停了 0.10.1。
- 慢的主因是流程结构，不在单个 job：发版要求在 main 上对一棵已经验证过的树再跑一轮完整 Source Validation；候选构建排在这轮之后，并绑定它的 run ID；“不许重跑”使任何一次偶发失败都退回一轮新的完整运行。最近四次合并的提交树都与通过了 PR CI 的 PR 头完全相同，其中三次的 main 重复运行因偶发失败变红。
- “不许重跑”不是安全要求，而是产物命名造成的：重跑失败的 job 时，同一运行里会出现两份同名产物，晋升核验分不清哪一份对应哪一次尝试，就整体拒绝。在名字里带上尝试序号，规则就可以取消。
- 不少门禁在不同层重复校验同一件事：晋升会重新下载并逐项复验 Source Validation 的内部回执，测试运行器另有一张冻结的平台跳过表，FastDB 版本在约 20 处重复钉死并互相断言。在可查的记录里，它们拦下的只有工具自身造成的不一致（重复产物、漏登记的跳过），没有拦下过产品问题；代价却很具体：冻结表让 0.10.1 白跑一轮 PR CI，0.2.4 升级改了 39 个文件。
- 方案 A（一两天的工具改动）：验证结果按树绑定，失败只重跑失败的那条线，候选不再等 main 上的验证。小修复预计约 50 分钟发出；一次偶发失败只多花一条线的重跑时间（≤22 分钟）。
- 方案 B（在 A 之后分批做）：删重复与历史工具约 8 千行，把候选关键路径从 21 分钟压到约 13 分钟，FastDB 版本只在依赖声明处出现一次，去掉遮蔽用户环境的 `PYTHONUTF8=1`。小修复预计约 40 分钟。

## 1. 实测耗时

数据来自 GitHub Actions API（job 与 step 的开始、结束时间）和 job 日志里源验证驱动打印的每个 gate 的秒数。

### 1.1 各阶段

| 阶段 | 实测 | 代表运行 |
| --- | --- | --- |
| PR 上的 Source Validation（完整） | 20–28 分钟 | [41036889359](https://github.com/world-in-progress/c-two/actions/runs/41036889359) 22.4 分钟（缓存热），[41008027942](https://github.com/world-in-progress/c-two/actions/runs/41008027942) 27.8 分钟 |
| 只改文档的 PR / push | 2.0–2.4 分钟 | [41017782061](https://github.com/world-in-progress/c-two/actions/runs/41017782061)、[41019176059](https://github.com/world-in-progress/c-two/actions/runs/41019176059) |
| main push 的 Source Validation | 21–32 分钟（10 月最长 59 分钟，含当时 ARM64 的 Arrow 构建） | [41010698934](https://github.com/world-in-progress/c-two/actions/runs/41010698934) 31.9 分钟（版本号变动，缓存半冷） |
| Release Candidate | 21–25 分钟，39 个 job | [41013847454](https://github.com/world-in-progress/c-two/actions/runs/41013847454) 21.2 分钟 |
| 预演（CLI 与 Python 并行） | 0.8–1.7 分钟 | [41016119599](https://github.com/world-in-progress/c-two/actions/runs/41016119599)、[41016124301](https://github.com/world-in-progress/c-two/actions/runs/41016124301) |
| 正式发布 | CLI 1.2 分钟；Python 6.2 分钟（核验 1.5，审批 0.5，上传 36 个文件 4.0） | [41016376334](https://github.com/world-in-progress/c-two/actions/runs/41016376334)、[41016379589](https://github.com/world-in-progress/c-two/actions/runs/41016379589) |

### 1.2 Source Validation 的关键路径

完整运行由 `changes`（0.1 分钟）→ `workflow policy`（1.4–2.1 分钟）→ 六条线并行组成。六条线要等 policy 结束才开始，所以每次运行都先串行等约 2 分钟。

| 条线 | job 时长（缓存热，41036889359） | job 时长（缓存半冷，41010698934） |
| --- | --- | --- |
| windows-11-arm, py3.12 | 20.0（其中验证步骤 14.9） | 30.1（验证步骤 26.0） |
| macos-15, py3.12 | 17.4 | 23.4 |
| windows-2022, py3.12 | 13.1 | 22.5 |
| windows-2025, py3.12 | 11.8 | 22.6 |
| ubuntu-24.04, py3.12 | 10.1 | 12.8 |
| ubuntu-24.04, py3.14t（python 范围） | 7.5 | 11.1 |

关键路径始终是 Windows ARM64。验证步骤是 `tools/ci/source_validation.py` 的一条串行 gate 链，除四个准备 gate 并行外，其余依次执行。缓存热时各 gate 的秒数：

| gate | ARM64 | macOS | Win 2025 | Ubuntu 3.12 |
| --- | --- | --- | --- | --- |
| core-test | 202 | 382 | 152 | 161 |
| python-suite | 172 | 275 | 138 | 224 |
| c2-mem-build | 125 | 9 | 87 | 17 |
| fastdb-npm-install + c2-mem-npm-install | 79 | 5 | 14 | 5 |
| python-wheel-build | 62 | 32 | 25 | 14 |
| source-harness-tests | 45 | 33 | 26 | 38 |
| rust-sdk-test | 31 | 41 | 35 | 9 |
| core-check | 35 | 30 | 19 | 17 |
| windows-standard-user-consumer | 20 | – | 9 | – |
| 其余 gate 合计 | 120 | 105 | 77 | 47 |
| 合计 | 891 | 912 | 582 | 532 |

验证步骤之外，每个 job 还有约 5 分钟的工具安装、缓存恢复、证据上传与收尾。

### 1.3 Release Candidate 的关键路径

候选的 `context` job 先调用 `check-source`，要求同一提交已有完整成功的 Source Validation，所以候选只能在 main 上那轮验证结束后才开始。之后的关键路径是 Intel macOS：c3 构建 4.9 分钟 → `wheels (3.13-3.14t)` 15.6 分钟（3.13 构建加验证 7.5，3.14 为 1.9，3.14t 为 4.2；FastDB 没有 Intel macOS wheel，每个 ABI 都要从 sdist 构建）→ manifest 0.2 分钟。其他 job 都在 11 分钟内结束。10 月 22 日改为只由手动触发以来，7 次候选全部首轮成功。

### 1.4 两次真实发版的时间线

0.10.0（10-30，顺利的一次）：PR CI 05:21–05:49 → 合并 → main Source Validation 05:51–06:23 → 06:24 触发候选，06:45 完成 → 06:47 预演 → 06:49 发布，06:56 完成。开 PR 到发布约 95 分钟，其中真正构建发布字节的只有候选那 21 分钟。

0.10.0 第一次尝试（10-29）：合并后的 main 运行首轮在 Windows 2025 与 ARM64 各遇到一个偶发失败（36.8 分钟）；约 43 分钟后只重跑失败的 job，整个运行在开始 102 分钟后才通过；候选 25 分钟通过；两个预演都拒绝了它，因为重跑让两条线各留下两份同名完整产物（“retains 2 … artifacts”）；只好再发起一轮完整运行（21 分钟）。之后因 FastDB 内存泄漏暂停。

0.10.1（10-30，未完成）：09:22 开 PR，首轮 PR CI 在 macOS 与两条 Linux 线失败——新测试在非 Windows 上的平台跳过没有登记进 `tools/dev/test_python.py` 的冻结表；补登记后 09:55–10:17 通过；10:19 合并，main 运行在 Windows 2025 遇到偶发失败（`test_runtime_session_lifecycle.py` 中零超时的第二次 shutdown）；10:55 重新发起的完整运行 [41044322502](https://github.com/world-in-progress/c-two/actions/runs/41044322502) 里，其他五条线都已通过，macOS 线在 11:00 进入 Emscripten 缓存步骤后再无进展，job 跑了 47 分钟，11:45 以 “The hosted runner lost communication with the server” 失败，日志与证据产物都没有上传——这是 GitHub runner 掉线，与代码无关。按现行规则，这一轮也不能靠重跑 macOS 线补齐，只能再来一轮完整验证。同一来源 `028e791d` 上的两轮 main 验证（10:19–11:45）都因与改动无关的原因失败，没有留下可用的证据；用户随后暂停了 0.10.1。

### 1.5 失败后退回到哪一步

| 失败点 | 今天退回到 | 代价 |
| --- | --- | --- |
| PR CI 某条线偶发失败 | 重跑失败的 job（PR 运行不作发版证据，允许重跑） | 约 15–22 分钟 |
| PR CI 撞上冻结跳过表 | 改表、推新提交、重跑完整 PR CI | 约 22 分钟，外加一次修改与审查 |
| main 运行某条线偶发失败 | 新发起完整运行；重跑会被晋升拒绝 | 22–32 分钟，外加发现与重新触发的人工间隔（10-29 为 43 分钟） |
| main 运行靠重跑通过 | 候选能建出来，预演时被拒，再来一轮完整运行和一轮候选 | 约 45–55 分钟 |
| 候选某个 job 失败 | 重新构建整个候选（产物先上传后验证，重跑会留下重复产物） | 约 21–25 分钟（10 月未发生） |
| 某条线遇到 runner 掉线等基础设施故障 | 与偶发失败相同：新发起完整运行 | 发现故障的时间（41044322502 的 job 跑了 47 分钟才判定失败）加 22–32 分钟 |

### 1.6 10 月的失败统计

- upstream 上 10 月共 138 次 Source Validation，其中 84 次是完整运行（大于 10 分钟）。
- main 上的 17 次完整运行有 4 次首轮失败（24%），且都只有一两条线失败：[38941100899](https://github.com/world-in-progress/c-two/actions/runs/38941100899)（Ubuntu，原因未查）、40857187478（Windows 2025 `generated_call_options_consumer`；ARM64 PowerShell 安装器测试超时）、[41024013834](https://github.com/world-in-progress/c-two/actions/runs/41024013834)（Windows 2025 `c2-ipc` `shutdown_ack_only_proves_initiation_and_retries_before_sending`）、41039841188（Windows 2025 零超时第二次 shutdown）。最近三次都落在 Windows 2025。
- 最近四次合并（`4c3e587d`、`4b1d9161`、`169dc53c`、`028e791d`）的提交树都与对应 PR 头完全相同，而这四个 PR 头的完整 PR CI 都已通过。main 上的那一轮是同一棵树的第二次验证，四次里三次因偶发失败变红。
- 发版要连过 PR CI 与 main 运行两轮完整验证。按 24% 的单轮失败率，两轮都首轮通过的概率约为 58%。

## 2. 逐项分类

判断标准：对应真实发生过的问题或用户明确的约束，就保留，必要时精简实现；同一事实在不同层重复检查、防的是没发生过的情形，或者由工具自身的偶然设计造成，就合并或删除。

### 2.1 保留：对应真实风险

| 机制 | 依据 |
| --- | --- |
| 六条线的完整 Source Validation（Linux、macOS、Windows x64、Windows ARM64、free-threaded） | 用户约束：协议与 ABI 变更需要 Linux、macOS、Windows x64/ARM64 的 Actions 证据。10 月确有只在单一平台暴露的问题，例如 ARM64 上的 PowerShell 宿主、owner 接管开发分支上只在两条 Windows 线失败的 core-test 与 Python 套件（[39612054566](https://github.com/Dsssyc/c-two/actions/runs/39612054566)）。 |
| 候选在原生 runner 上构建 35 个 wheel、6 个 c3、1 个 sdist，并对每个 ABI 跑安装后的生命周期 | 这是发布字节本身；35 个 wheel 是 35 份不同的二进制（含 3.14t 与 ARM64）。它是并行的，主要花的是机时，不在墙钟上。 |
| 晋升只搬运候选字节：API digest、manifest 哈希、产物清单 | 用户约束：发布的字节就是验证过的字节，来源可追溯。 |
| 远端状态检查：同字节可续传，不同字节拒绝覆盖 | 发布可能中途断开；PyPI 本身不可覆盖，GitHub release 资产可以被覆盖，需要拦住。 |
| Trusted Publisher 与 `pypi` 环境审批 | 用户约束。 |
| `changes` 范围选择 | 只改文档时 2 分钟即可，实测有效。 |
| workflow policy（actionlint 与 `tests/repo`） | 工作流写错会直接拖累发版；保留，但不必挡在六条线前面（见 D7）。 |
| 测试结束后清理进程树，有残留进程就判失败 | 真实发生过：并行派生的子进程继承了测试探针的文件句柄，锁迟迟不释放（`c2-local` 偶发失败的根因）。 |
| 每个 gate 的超时与聚合预算 | 子进程变慢或卡住时要有界地失败；ARM64 上模拟的 x86 PowerShell 曾在一次运行里慢到约 50 秒。 |
| Windows standard-user 消费者、Windows 管道外来 owner 身份测试 | 产品在 Windows 上依赖 SID/DACL 语义，管理员与普通用户行为不同。 |
| IPC 内存矩阵基准本身 | 几秒钟，守的是内存回收回归；只删它的回执复验（见 D10）。 |

### 2.2 合并或删除：重复、冗余或过度防御

| 编号 | 机制 | 证据与问题 | 处理 |
| --- | --- | --- | --- |
| D1 | main push 对同一棵树再跑完整 Source Validation，发版只认 main 上的运行 | 最近四次合并都是同树，main 重复运行三次变红；PR 运行在 upstream 的 Actions 里执行同一棵树（含同一份工作流）。 | A1：证据按树绑定 |
| D2 | “每条线恰好一份完整产物”，禁止重跑 | 产物名只含条线与提交，不含尝试序号；10-29 两个预演因此拒绝了本可发布的候选。规则防的不是任何风险：重新跑一轮完整运行同样是“再掷一次骰子”，而且要掷六条线。 | A2：产物名带尝试序号，按每条线最新一次尝试选取 |
| D3 | 候选开始前必须等 Source Validation 完成，并把那次 run ID 写进 `rc-context.json` | 串行多花 22–32 分钟；main 运行一旦偶发失败，已经通过的候选也随之作废。晋升本来就会再检查一次 Source Validation。 | A3：候选与验证解耦，晋升时再选证据 |
| D4 | 晋升下载五条线的完整产物，重新加载 `source_validation.py` 推导 gate 清单，逐项复验回执、内部哈希与冻结的 FastDB 提交（`verify_source_validation` 及其辅助函数约 260 行） | 这些都是 Source Validation job 自己已经强制检查过的：job 成功就意味着它们成立。“用受信任的 main 代码复验候选”在这个项目里不成立，因为候选就构建在 main 上。每次改 gate 名还要同步改晋升，否则发版时才失败。 | A4：只查元数据 |
| D5 | 预演与正式发布分两次触发 | 正式发布的 prepare job 做的就是同一套核验；人工把关已经由 `pypi` 环境审批承担。 | A5：一次触发，审批前看 prepare 的计划 |
| D6 | `tools/dev/test_python.py` 的冻结平台跳过表（精确到参数化 ID 与原因文本） | 每个测试的平台条件已经写在测试里，这张表是第二份拷贝；0.10.1 因漏登记一个跳过白跑一轮 PR CI。它要防的“缺前置条件时静默跳过”可以用一条规则实现。 | A7：平台跳过只能来自一个统一标记，其他跳过一律失败 |
| D7 | 六条线要等 workflow policy 结束才开始 | 每次运行多等约 2 分钟；“工具出错就别浪费 runner”省的是机时，花的是每次的墙钟。 | A6：并行，汇总 job 仍要求两者都成功 |
| D8 | 晋升逐项复验候选回执的内部字段（生命周期清理标志、FastDB 决策绑定等，`verify_candidate_receipts` 约 230 行） | 候选的各个 job 已经验证过，回执不是 passed 时 job 就失败；manifest job 还会再核对一次清单。 | B2：只保留字节与清单绑定 |
| D9 | 安全解包校验器 `tools/local_rc/artifact_manifest.py`（796 行），用于解包本仓库自己的候选产物 | 防的是恶意 ZIP（路径穿越、符号链接），而这些产物由同一仓库、同一工作流生成；Python 的 `zipfile` 解包本身已经处理绝对路径与 `..`。 | B2：改用普通解包加哈希比对 |
| D10 | `source_validation.py` 的 `matrix_evidence_problem`（约 115 行）复验 IPC 矩阵回执 | 防的是 “`--only` 子集也能退出 0”，但驱动从不传 `--only`；基准失败时本来就非零退出。 | B2：删除，依赖退出码 |
| D11 | `test_python.py` 运行前后重算 c3、native、fixture、tarball、CoreSDK 的哈希；四次收集加分区校验 | 防的是本机另一个智能体在测试途中替换文件，以及 xdist 分发出错。CI 里没有并发写者；分发出错时 pytest 自己就会失败。 | B2：删除；保留进程清理与跳过规则 |
| D12 | FastDB 版本与源码提交在约 20 处重复钉死并互相断言（ci.yml、两个候选工作流、晋升脚本、`prepare_fastdb_sdk.py`、`test_python.py`、`source_validation.py`、测试夹具） | 0.2.4 升级（`1b4a9701`）改了 39 个文件。能从依赖声明推导出来的值，不该再做成常量。 | B3：只在依赖声明处出现一次 |
| D13 | 所有工作流与测试入口全局设 `PYTHONUTF8=1`，包括模拟用户的安装后冒烟（`windows_wheel_smoke.py`、`windows_standard_user.ps1`），还有测试断言它必须为 1 | 遮住了 0.10.1 修的那个面向用户的崩溃。用户环境没有这个变量。 | B4：去掉；需要 UTF-8 的测试自己指定编码 |
| D14 | 每条线都跑一遍 `tests/repo` 的子集（source-harness-tests）、`core-check`、`fastdb-rust-test`（FastDB 自己的测试），以及非 Linux 线上的 wheel 构建与安装消费者 | `tests/repo` 已在 policy 里跑过，只有 PowerShell 安装器测试与平台有关，而它正是 ARM64 偶发失败的来源；`core-test` 已经编译全部目标；安装消费者在候选里覆盖全部 35 行和两种 Windows 架构的普通用户。 | B5：只留平台相关的部分 |
| D15 | `.github/workflows/local-endpoint-validation.yml`（456 行） | upstream 从未运行过，fork 上最后一次运行在 10-21；每次 FastDB 升级都要同步改它。 | B2：删除；其中跨用户权限检查若仍需要，并入 Source Validation |
| D16 | `tools/local_rc/build_candidate.py`、`local_registry.py`、`package_consumers.py`（共 3436 行）及其测试（约 1140 行） | 七月 Phase 0B 本地候选的工具，除自身测试外没有任何调用者；那次候选的证据已保存在 `docs/reports/` 与 Git 历史里。 | B2：删除 |
| D17 | 候选里 Intel macOS 的三个 ABI 在同一个 job 里串行 | 关键路径 15.6 分钟（属于提速，不算防御）。 | B1：拆成每个 ABI 一个 job |

### 2.3 测试与生产代码

生产代码里“真实问题与想象中的防线”的逐项复核，已经在 [2026-10-13 防御式编程复核](../acceptance/2026-10-13-defensive-programming-regression-reassessment.md) 中按 R01–R39、T01–T07、C01–C07 做过，后续进度记在批次台账里；本文不重做，只补充与发版直接相关的发现：

- 10 月卡住发版的偶发失败分三类：测试里写死的短超时（codegen 消费者，已修；`c2-ipc` 用 1 秒的 `PROBE_BUDGET` 断言 ACK 语义，在繁忙的 Windows 2025 上超时，未修），测试环境本身（ARM64 上模拟的 x86 PowerShell 每次运行要编译 `Add-Type`，已修），以及产品语义（零超时的第二次 shutdown 在 `try_lock_until` 遇到短暂锁竞争时失败，另行修复中）。前两类应在测试里改：验证语义的测试给宽裕的超时，验证超时行为的测试留出余量。第三类是真实的产品边角，应在产品里修。
- 已核对的测试接缝（`c2-ipc` control 的 `test_seam`）在 `#[cfg(test)]` 里；生产代码中没有读取测试专用环境变量（`CTWO_*`、`C2_PORTABLE_MATRIX_*`、`C2_TYPESCRIPT_*`）的地方。
- 验收方案 A 时又遇到一处同类偶发失败：`tests/repo/test_local_test_platform.py::test_stop_cleans_orphan_grandchild_without_touching_unrelated_process` 在整组运行时失败一次，单独重跑 3/3 通过；它用 5 秒的固定超时等待子进程启动与退出，属于“测试里写死的短超时”。
- 规模：Python SDK 源码 6.8 千行，测试 2.8 万行、937 个测试；Rust 共 2094 个测试。粗略检索：Python 测试中零超时或亚秒超时 92 处，`time.sleep` 70 处；Rust（含生产代码）1 秒及以下的 `Duration` 字面量 528 处。这些数字没有逐项分类，只说明“短超时断言”这一类偶发失败的来源还有不少；建议遇到一处修一处，不做一次性的大扫除。
- `PYTHONUTF8=1` 一项（D13）同时属于测试与产品：测试环境与用户环境不一致，产品缺陷就藏了起来。

### 2.4 候选与 PR CI 的分工

候选做四件事：在 6 个目标上用 release 配置构建 6 个 c3 和 35 个 wheel（Linux 在 manylinux2014 容器里，Python 3.10–3.14t），在 Linux 上构建 sdist 并从 sdist 隔离构建一次 wheel；把每个产物装进干净环境跑一遍安装后生命周期；Windows 两种架构上再以普通用户跑一遍；最后把全部字节的哈希汇成 manifest。PR CI 用 debug 构建在 6 条线上测源码，只覆盖 py3.12（Linux 另有 3.14t），没有 Intel macOS 和 Linux ARM64 线，也从不构建发布用的字节。

两者的检查确有重叠，但花在重叠上的时间很少。Intel macOS 关键路径 job 的实测：3.13 的 release 编译 4 分 46 秒，从 sdist 构建 FastDB 2 分 12 秒（FastDB 没有发布 Intel macOS 的 wheel），安装后生命周期约 4 秒；3.14 与 3.14t 的生命周期也各约 5 秒。所以候选的耗时几乎都在构建发布字节上，这部分 PR CI 给不了，只用 PR CI 的结论也省不掉。

真正重复的有两处，方向与直觉相反：一是 PR CI 在每条线上都做了一个小号候选（release wheel 构建、安装后消费者、Windows 普通用户），在关键路径 ARM64 线上约 1.5–3 分钟，见 D14 与 B5；二是候选的 3.12 行把同一份已经过哈希核对的字节重新下载，再跑一遍生命周期，见 B1。要明显缩短候选，办法不是删检查，而是把它从关键路径上挪开：按 ABI 拆开 Intel macOS（B1），或让它与 PR CI 同时跑（见第 4 节第 7 项）。

## 3. 方案

### 方案 A：一棵树只验证一次，失败只重跑失败的那条线

改的都是发版工具，不碰产品代码；建议放进一个 PR（A5 依赖仓库设置，可以稍后单独做）。

A1 证据按树绑定。“同树”指两个提交的 Git tree 哈希相同，也就是文件内容完全一致。晋升时，在发版提交本身及与它关联的 PR 头（`GET /repos/{repo}/commits/{sha}/pulls`）中，找一次成功的完整 Source Validation，满足：运行在 canonical 仓库，工作流路径是 `.github/workflows/ci.yml`，事件为 push、workflow_dispatch 或 pull_request，被测提交与发版提交同树。对 pull_request 运行，去掉 `head_repository` 必须是 canonical 仓库的要求。理由：这次运行执行的正是将要发布的那棵树，包括同一份工作流；要让它给出假的结果，只能改动这棵树本身，而这棵树就是要发布的东西，在 main 上再跑一次也防不住。同一个查找函数也给 `changes` 用：main push 时如果同树已有成功的完整运行，就只跑 policy。把它放在一个脚本里，晋升与范围选择共用。

A2 允许重跑 Source Validation。`ci.yml` 的产物名加上 `-a${{ github.run_attempt }}`。晋升按每个必需 job 最新一次尝试的序号选产物，删掉“重复产物说明有人重跑”的规则与相应文档。候选不在 A2 之内：候选的 job 之间按产物名传递字节，名字带上尝试序号会让只重跑一部分 job 的下游找不到上游产物；它要先改成“检查全部通过后才上传”，放在 B1 里做。10 月 22 日以来候选 7 次全部首轮成功，暂时按现状处理：候选失败就重建整个候选。

A3 候选不等验证。删掉 `release-candidate.yml` 的 “Require completed full source validation before packaging” 步骤和 `check-source` 子命令，`rc-context.json` 去掉 `source_validation_run_id`（模式改为 v2，晋升只认 v2）。晋升时再用 A1 找证据。合并后可以立即触发候选。

A4 晋升只在边界上查一次。删掉 `verify_source_validation` 的深度复验、`expected_source_full_gates`、`_source_fastdb_pin`、`SOURCE_VALIDATION_FASTDB_*` 等常量和对应的测试；只要求必需 job 的最新尝试成功，并且对应的条线产物存在。候选字节的 digest、manifest 与清单核验，以及远端状态检查，照旧保留。

A5 一次触发。去掉 `dry_run` 输入；prepare job 把发布计划写进 step summary，publish job 等环境审批：PyPI 用已有的 `pypi` 环境，CLI 新建 `github-release` 环境并设必需审批人（需要用户在仓库设置里建）。这样只剩一道人工关口，就在真正发布之前。

A6 `source` job 只依赖 `changes`，与 workflow policy 并行；汇总 job 仍要求两者都成功。

A7 平台跳过规则。在 `sdk/python/tests/conftest.py` 定义唯一的平台标记，它跳过时给出固定前缀的原因；`test_python.py` 只接受这个前缀的跳过，其他跳过一律失败。删掉三张冻结表和 `tests/repo/test_python_test_runner.py` 里钉表的测试，把现有的平台跳过改用这个标记。

方案 A 下一个小修复的预计时长（缓存热，无偶发失败）：PR CI 约 20 分钟 → 合并并触发候选约 2 分钟 → 候选 21 分钟 → prepare 1.5 分钟、审批约 1 分钟、上传 4 分钟 → 共约 50 分钟。PR CI 遇到一次偶发失败，只重跑那条线，再加 ≤22 分钟；候选失败仍要重建整个候选，再加约 21 分钟（B1 之后改为只重跑失败的 job）。按 0.10.1 的实际经过推算：09:45 PR CI 通过（冻结表的问题不再出现）→ 合并 → 候选 09:47–10:08 → 约 10:17 发布；而实际上到 11:45，第四轮完整验证因 runner 掉线失败，仍未进入候选构建。

风险与对策：

- 接受 PR 运行作证据，是信任模型的变化。对策：严格同树比较，限定 canonical 仓库与 `ci.yml`，要求完整的 job 清单。剩下的差异是 PR 运行与发版之间 runner 镜像可能更新；候选仍在当时的镜像上构建并跑全部 35 行安装后生命周期。PR 运行的产物保留 14 天，过期就在 main 上 `force_full`，与现在相同。
- 接受重跑，可能让一个真实的间歇性缺陷靠运气通过。现有规则同样防不住（新跑一轮也是再碰一次运气），而且要多碰六条线。对策：发版路径上的每次偶发失败都在台账里记一条，并按 2.3 的分类修根因；是否在审批前处理完，由人决定。
- 候选不等验证，验证真失败时会白花一次候选的机时。A1 之后，PR CI 通过才能合并，这种情况很少。
- 删掉深度复验后，如果源验证驱动有缺陷，比如跳过了某个 gate 却报告成功，晋升不会再发现。驱动是仓库内经过审查与测试的代码，它的退出码就是契约；同一件事查一次就够。
- A5 必须先建好 `github-release` 环境，否则 CLI 发布不会暂停。对策：在环境建好之前，A5 先不合入。

迁移步骤：

1. 用户对第 4 节的事项拍板（A1、A5 与先后顺序已定）。
2. 实现 A1–A4、A6、A7，一个 PR。本地跑 actionlint 与 `tests/repo`；PR CI 完整通过。
3. 下一次发版（可以就是 0.10.1）作为验收：合并后立即触发候选，晋升用 PR 运行的证据，这一次保留预演，确认新的证据查找与产物选取的结果。
4. 用户在仓库设置里建 `github-release` 环境（可选：打开 “Require branches to be up to date before merging”，让同树成为常态）；然后合入 A5。
5. 同步 `docs/ci-cd.md`；`AGENTS.md` 里关于 Actions 证据的约束不变。

### 方案 B：在 A 之上瘦身与提速

各项相互独立，可以分批做；B1–B5 都不碰产品语义。

- B1 候选重整：Intel macOS 的两个 ABI 组拆成每个 ABI 一个 job，与 Windows 相同，关键路径从 c3 4.9 + 15.6 分钟变为 c3 4.9 + 单个 ABI 约 7.5 分钟，候选约 13 分钟；3.12 行不再把同一份字节重新下载后再跑一遍生命周期，Windows 普通用户检查保留；每个 job 在全部检查通过后才上传产物，这样候选也能只重跑失败的 job。
- B2 删除：D8–D11、D15、D16 列出的代码，以及它们在 `tests/repo` 里的测试（`test_release_candidate_promotion.py` 2862 行、`test_source_validation.py` 739 行、`test_python_test_runner.py` 515 行都会大幅缩小）。估计共约 8 千行。`AGENTS.md` 中关于本地候选证据的段落改为只指向历史报告。
- B3 FastDB 版本只出现一次：版本只写在 `sdk/python/pyproject.toml` 的 extra、`sdk/rust` 与 `c2-codegen` 的 Cargo 依赖里，源码提交只写在 `cli/` 与 `sdk/python/native/` 的 `fastdb-sys` 补丁里；工作流、`prepare_fastdb_sdk.py`、测试运行器与夹具在运行时从这些声明读取，不再保存常量，也不再互相断言。目标：下次升级只改依赖声明与锁文件。
- B4 不再让 `PYTHONUTF8=1` 遮住产品缺陷。缺陷本身与这个变量无关，它只让 CI 看不见：Windows 用户的 Python 用系统代码页（英文系统 cp1252，中文系统 GBK），`cc.serve()` 的崩溃就是用户先遇到的。这一类缺陷分两种，各用最小的办法兜住：一是读写文件或子进程文本时不指定编码。`python -X warn_default_encoding` 会对每一处发出 `EncodingWarning`，而且在 UTF-8 模式下照样发出（已实测），所以在 SDK 测试里把来自 `c_two` 包的 `EncodingWarning` 当作错误，任何平台、任何开发机上都能拦住，不必等 Windows CI；已实测它指向 `c_two/crm/template.py:31` 读取用户 CRM 源文件的那一行，同文件写出生成代码与 `c_two/cli/contract.py` 写出契约文件也属此类，先把这 3 处改为显式 UTF-8。二是向非 UTF-8 的标准输出打印，`EncodingWarning` 管不到，由 0.10.1 那样的针对性测试覆盖，并让 Windows 上的安装后冒烟（最接近用户的环境）不再设 `PYTHONUTF8`。工具自己的输入输出改为显式 UTF-8，不再用会传给子进程的环境变量；全局去掉这个变量并不必要。
- B5 条线 gate 精简：source-harness-tests 只在 Windows 线跑 PowerShell 安装器测试；`core-check` 只在 Linux 跑；删掉 `fastdb-rust-test`；wheel 构建与安装消费者只留 Linux 一条作为早期信号，其余由候选覆盖。ARM64 线预计再省 2.5–3 分钟。
- B6 修完已知的偶发失败：`c2-ipc` 的 1 秒 `PROBE_BUDGET`（测试侧），零超时第二次 shutdown（产品侧，另行修复中）。
- B7 Windows x64 只保留一条线：用户 2026-10-30 同意，保留 Server 2025（与候选构建 x64 wheel 的 `windows-latest` 一致），去掉 Server 2022；已并入方案 A 的改动。Windows ARM64 线不受影响。约束要求的是 Windows x64 与 ARM64，不是两个 Server 版本；代价是少了 Server 2022 的覆盖，收益是少一条线的偶发失败暴露与机时，墙钟不变（关键路径在 ARM64）。

方案 A 加 B 之后的预计时长：PR CI 约 17–18 分钟 → 约 2 分钟 → 候选约 13 分钟 → 约 7 分钟，共约 40 分钟。

### 时长对比

| 情形 | 今天 | 方案 A | 方案 A+B |
| --- | --- | --- | --- |
| 小修复，一切顺利 | 约 95 分钟（实测） | 约 50 分钟 | 约 40 分钟 |
| 遇到一次偶发失败 | 加 30–60 分钟，并退回一轮完整运行（实测） | 加一条线的重跑 ≤22 分钟 | 加 ≤20 分钟 |
| 某条线遇到 runner 掉线 | 发现故障（41044322502 为 47 分钟）后再来一轮完整运行 | 发现故障后只重跑那条线 | 同左 |
| FastDB 小版本升级 | 改 39 个文件（实测） | 不变 | 只改依赖声明与锁文件 |

### 未采纳：在发版 PR 上直接构建候选

如果按同样的同树论证，接受在 PR 运行里构建的发布字节，合并后可以立即晋升，小修复约 30 分钟。没有采纳：这会让 fork PR 上下文构建的字节直接进入发布，信任模型的改动比 A1 大；而且需要按“是否改了版本号”来决定是否构建候选，又多出一条规则。A 加 B 的 40 分钟已经满足“合理时间内安全发出”。

## 4. 用户决定

| 事项 | 状态 |
| --- | --- |
| 1. 接受同树的 PR 运行作为发版证据（A1） | 2026-10-30 随方案 A 批准 |
| 2. 取消预演，改为一次触发加环境审批（A5） | 批准；CLI 发布前需要用户建 `github-release` 环境，届时给出具体步骤 |
| 3. 删除 `local-endpoint-validation.yml` 与 `tools/local_rc` 的本地候选工具（D15、D16） | 待定 |
| 4. 不再让 `PYTHONUTF8=1` 遮住产品缺陷（B4） | 批准，按 B4 的做法实施（不全局去掉），与方案 A 同一个 PR；3 处产品修复进 0.10.1 |
| 5. 只保留一条 Windows x64 线（B7） | 批准；保留 Server 2025，已并入方案 A |
| 6. 方案 A 与 0.10.1 的先后 | 方案 A 先落地 |
| 7. 候选与 PR CI 同时跑（可选，见下） | 待定 |

第 7 项：把发版 PR 的分支同时推到 upstream，在 upstream 上对它触发候选，晋升接受与发版提交同树的候选。候选从关键路径上挪开，小修复约 30 分钟。代价：发版分支要推到 upstream；候选在合并前构建，PR 再改就要重建；c3 的标签仍打在 main 的发版提交上（产物不嵌入提交号，已核对 `cli/` 与 `sdk/python/native/` 都没有 build.rs）。它与第 3 节“未采纳”的做法的区别是：字节在 canonical 仓库里构建，不在 fork PR 的上下文里。

## 5. 方案 A 执行计划

两个互不重叠的微任务，可以并行：

- 微任务 1，发版证据：A1、A2、A3、A4、A5、A6。涉及 `.github/workflows/ci.yml`、`release-candidate.yml`、`cli-release.yml`、`python-package-release.yml`、`.github/scripts/promote_release_candidate.py`、`.github/scripts/ci_scope.py`（同树查找放在一个两处共用的脚本里），`docs/ci-cd.md`，以及 `tests/repo` 中对应的测试。验收：actionlint 与 `tests/repo` 全部通过；用伪造的 API 数据覆盖同树 PR 运行被接受、不同树被拒绝、重跑后按最新尝试选产物、一条线缺产物被拒绝、main push 遇到同树已验证时只跑 policy；预演输入、`check-source` 子命令、深度复验与相关常量全部删除，没有留下兼容分支。
- 微任务 2，平台跳过：A7。涉及 `sdk/python/tests/conftest.py`、现有带平台跳过的测试文件、`tools/dev/test_python.py`、`tests/repo/test_python_test_runner.py`。验收：三张冻结表删除；平台跳过只来自统一标记，其他任何跳过仍判失败；原来冻结表里的每个用例改用标记后，在对应平台上仍被跳过、在其他平台上照常执行。

两者合入后在 fork 上跑一次完整 CI（推送前问用户），然后开 PR；0.10.1 用新流程发布，作为方案 A 的实际验收。实现方不删除文件以外的任何东西，不用 git stash，不动分支与标签。

进展（2026-10-30）：两个微任务都已验收，与 Host 的两处改动一起集成在分支 `socu/release-agility-review` 上：平台标记（test: express platform skips with one marker…）、发版证据（ci: bind release evidence to Git trees…）、贡献者文档里的跳过规则、B7（ci: keep one Windows x64 source lane on Server 2025）。微任务 1 首轮有两处问题，由同一微任务修复：只改文档的 push 被误判为需要完整验证；step summary 代码在两个工作流里各复制了一份。本机复核：actionlint 无报错，`tests/repo` 除上面那个已知的偶发失败外全部通过，受影响的 SDK 测试文件在 macOS 上 94 passed、11 个 `platform: windows only` 跳过。分支已同步到包含 #207 的 main。fork 上的完整 CI [41061490204](https://github.com/Dsssyc/c-two/actions/runs/41061490204)（方案 A 加 B7，fork 缓存为空）首轮 9/9 通过，墙钟 21.4 分钟；五条完整线在开始后 0.2 分钟就启动（原来要等 policy 约 2 分钟），证据产物名带 `-a1`。B4 随后实施（编码修复与 `EncodingWarning` 门禁），由 PR CI 一并验证。

## 6. 验证边界

- 实测：第 1 节的所有耗时来自 Actions API 与 job 日志；失败原因来自 job 日志与 check-run 注解；四次合并的同树比较来自本地 `git rev-parse <commit>^{tree}`。
- 推算：方案 A 与 B 的时长，是用实测的阶段时长与 10 月的失败率估出来的，没有实际运行；B1 的 13 分钟假设拆开后单个 Intel ABI 的 job 与现在组内的第一个 ABI 一样长。
- 未核实：`38941100899` 的失败原因；A1 的 `commits/{sha}/pulls` 查询在 squash 合并下的行为（本项目目前用 merge 提交，同树判断不受影响）；B4 需要修改多少测试。
- 41044322502 的 macOS 线失败原因取自 check-run 注解（runner 失联）；GitHub 侧为何在缓存步骤处失联、为何 job 跑了 47 分钟才判定失败，没有更多信息（日志没有上传）。只有这一次，所以本文不为它加超时之类的新机制；它说明的是基础设施故障同样会落到“不许重跑”上。
