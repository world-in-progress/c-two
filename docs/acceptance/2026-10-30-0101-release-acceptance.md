# 0.10.1 发布验收

[C-Two 0.10.1](https://pypi.org/project/c-two/0.10.1/) 已于 2026-10-30 发布。这是只含 Python 包的补丁，继续配套 c3 0.6.0，不发新的 CLI、Rust crate 或 npm 包；依赖 FastDB 0.2.4。发行来源 `1a69b78a44f58ceaa671fcf6b0c892b0206f4c45` 是 [PR #208](https://github.com/world-in-progress/c-two/pull/208) 的合并提交，内容树与 PR head `2036a527` 相同。从 `028e791d` 开始的第一次资格验证两次因与改动无关的原因失败，没有构建候选；用户随即暂停发版，先修暴露出的偶发失败，再精简发版流程。0.10.1 是走精简后流程的第一个版本：发行证据按 Git 内容树绑定，选用 PR #208 对 `2036a527` 的完整 Source Validation，合入后 main 上不再重跑完整验证。

## 修复内容

| 修复 | 实现提交 | 合入 |
| --- | --- | --- |
| `cc.serve()` 在标准输出不是 UTF-8 时不再以 `UnicodeEncodeError` 退出：固定文字只用 ASCII，消息按标准输出自己的编码写出 | `51cb141e` | [PR #206](https://github.com/world-in-progress/c-two/pull/206)（`028e791d`） |
| 已完成的关停之后，零期限的关停不再偶发报 `core_retirement_wait` 超时 | `57d27fc2` | [PR #207](https://github.com/world-in-progress/c-two/pull/207)（`399286f5`） |
| CRM 模板生成与契约导出按 UTF-8 读写，不随平台代码页变化 | `73845204`，测试修正 `2036a527` | PR #208（`1a69b78a`） |

各项的原因、复现与本机核对见[发行说明](../releases/0.10.1.md)与[精简批次记录](../design/2026-10-13-local-slimming-batches.md)。

## 第一次资格验证（未发布）

| 阶段 | 实际结果 |
| --- | --- |
| [PR #206 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41033002408) | 来源 `26c15dfe`。三条 Windows 线通过，新用例在 Windows 默认代码页下真正运行；macOS 与两条 Linux 线失败：Python 套件运行器只接受冻结登记、原因逐字匹配的平台跳过，Windows 默认代码页一项在 POSIX 上的跳过没有登记。 |
| [PR #206 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41036889359) | 来源 `4f509564`，登记该跳过后首轮 10/10，PR 以 `028e791d` 合入。 |
| [main push Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41039841188) | 来源 `028e791d`。Windows 2025 上撞上 0.10.0 起就有的偶发失败：`test_runtime_session_lifecycle.py` 第 115 行，第一次关停完成后，零期限的第二次关停报 `core_retirement_wait` 超时。 |
| [main 完整资格验证](https://github.com/world-in-progress/c-two/actions/runs/41044322502) | 手动 `force_full`，来源 `028e791d`；macOS runner 与 GitHub 失联。 |

按当时的规则，发行用的 Source Validation 不能重跑，只能在 main 上重新发起完整运行，所以一处与改动无关的失败就要再等一轮完整验证。用户在这里暂停发版，要求先修这个偶发失败，并另开会话审查发版流程。

## 偶发失败的修复（PR #207）

关停驱动线程在仍持有 RuntimeState 锁时唤醒等待者，被唤醒的调用方紧接着发起零期限关停，拿不到这把锁。修复（`57d27fc2`）让驱动先释放锁与退休资源、最后唤醒等待者；把旧顺序的空隙拉宽 50 ms 时原测试 5/5 失败，新顺序加同样的暂停 5/5 通过。[PR #207 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41055663146)（`e7f9f090`）首轮 10/10，以 `399286f5` 合入，main push [41059030180](https://github.com/world-in-progress/c-two/actions/runs/41059030180) 首轮 10/10。用户决定等发版流程精简后再发，没有用这次运行构建候选。

## 发版流程精简（PR #208）

审查过程与取舍见[发版敏捷性审查](../design/2026-10-30-release-agility-review.md)，现行规则见 [CI 说明](../reference/ci-cd.md)。PR #208 的改动：

- 发行证据绑定 Git 内容树：晋升接受 canonical 仓库里 `ci.yml` 的成功完整运行（push、workflow_dispatch 或 pull_request），只要它测试的提交与发行提交的内容树相同。PR 运行检出的就是 PR head，比较是精确的；main push 的内容树已在 PR 上通过时只跑 workflow policy。
- 源码产物名带运行尝试次数（`-a<attempt>`），晋升取每个必需 job 的最后一次尝试，失败的源码线可以只重跑那一条。
- Release Candidate 不再等 main 的 Source Validation，证据只在晋升时选定一次；晋升对 Source Validation 收据的重复深度核验一并删除。候选字节、清单、收据与远端状态的核对不变。
- 去掉预演输入：准备阶段把发布计划写进 step summary，发布等环境审批（`pypi`，以及新建的 `github-release`）。
- 平台跳过统一用 `platform('windows'|'posix')` 一个标记，运行器只接受这一种跳过原因，三张冻结的 nodeid 表删除。`c_two` 内按默认编码的 I/O 让 SDK 测试失败，由此发现并修掉上表第三项。
- Windows x64 只保留 Server 2025 一条源码线，即候选构建所用的镜像。

| 阶段 | 实际结果 |
| --- | --- |
| [fork 完整 CI](https://github.com/Dsssyc/c-two/actions/runs/41061490204) | 来源 `34a3127e`（编码修复之前），缓存为空，首轮 9/9，21.4 分钟。 |
| [PR #208 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41066825012) | 来源 `49e63bc7`。Ubuntu 3.12 线的 sccache 读不到 Actions 缓存后端，启动失败（基础设施）。Windows 2025 与 Windows 11 ARM64 上各有且只有 2 项失败，都是新用例 `test_contract_export_cli_writes_utf8_stdout_in_order` 的 cp1252 与 gbk 参数：测试包装假标准输出的 `TextIOWrapper` 没有指定 `newline`，Windows 上把 `\n` 写成 `\r\n`，产品代码无误。两个汇总 job 随之失败。 |
| [PR #208 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41074242324) | 来源 `2036a527`（测试改为 `newline="\n"`），首轮 9/9，16.6 分钟。 |

## 发行门禁（2026-10-30）

| 阶段 | 实际结果 |
| --- | --- |
| [main push Source Validation](https://github.com/world-in-progress/c-two/actions/runs/41077179561) | 来源 `1a69b78a`。只跑 `changes` 与 workflow policy；内容树已在 PR 上通过，源码线全部跳过，1 分 49 秒。 |
| [Release Candidate](https://github.com/world-in-progress/c-two/actions/runs/41077234739) | 合入后立即触发，来源 `1a69b78a`；38 job 成功，1 个既定的 Linux ARM64 重复 sdist 分支跳过，25 分钟。 |
| [Python 发布](https://github.com/world-in-progress/c-two/actions/runs/41081182309) | 准备阶段选中 41074242324 作为证据（pull_request，测试提交 `2036a527`，内容树 `26400c10`，与发行来源相同），6 个必需 job 都是第 1 次尝试，4 条完整平台线各有一份 `-a1` 产物；计划上传 36 个文件。核对计划后，`pypi` 环境按用户的发版授权批准；发布 job 4 分钟。 |

从 PR #208 合入（15:03:31Z）到 PyPI 发布完成（15:35:47Z）共 32 分钟，其中 Release Candidate 25 分钟，准备、审批与发布 6 分钟。0.10.0 同样一路首轮通过，从合入到发布完成用了 65 分钟：main 上的完整资格验证 32 分钟，Release Candidate 21 分钟，两次预演、审批与两次发布约 11 分钟。

## 发布核对

候选清单 SHA-256 为 `43706736c5ad0d952c2f668c131c1461947196fb972e354433ab019f4591f744`，从候选运行的 `rc-receipt` 产物重新计算一致，清单记录的来源为 `1a69b78a`；计划的 36 个文件与清单的名称、大小、SHA-256 逐一相符。PyPI 全部 35 个 wheel 与 1 个 sdist 的名称、大小、SHA-256 均与计划一致，没有 yanked 文件；Windows ARM64 的 cp311、cp312、cp313、cp314、cp314t 五个 wheel 齐全。

发布后在 macOS ARM64 的 CPython 3.13 上从 PyPI 安装 `c-two[fastdb]==0.10.1`，带入 `fastdb4py` 0.2.4，注册并调用一个同进程 CRM。另以 cp1252 标准输出、关闭 UTF-8 模式运行 `cc.serve()`，注册一个中文路由名：横幅只含 ASCII，路由名显示为 `回声`，收到 SIGINT 后正常关停，以 0 退出。同一脚本在 0.10.0 上打印横幅时抛 `UnicodeEncodeError`，以 1 退出。结构化记录见 [evidence](evidence/2026-10-30-0101-release-acceptance.json)。

## 清理

fork 上的 `socu/release-agility-review` 已包含在 main 中，按确切 SHA（`2036a527`）删除；它在 fork 上的 Actions 缓存与上游 PR #208 的缓存一并删除。#206、#207 的分支、worktree 与缓存在各自合入后已清理，诊断与实现用的 Buddy checkout 都已回收。第一次资格验证的两次运行留在 Actions 记录里，没有构建候选。本记录合入后删除本记录分支及其 worktree。
