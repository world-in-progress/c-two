# R13 收敛、依赖安全修复与 0.8.0 发布验收

本记录对应 Python C-Two 0.8.0 与 c3 0.4.0。最终发行源码为 `96fe6583118025c0dc1ecbad7685fee5e315b87d`。先前的主线候选 `b8124f1a71b0c16f876c5af61df9cd5f06c01180` 与 PR #188 验证树一致；用户随后增加 Windows ARM64 范围，原 30-wheel 候选未发布。后续纯验收文档提交不作为发行产物来源。

R13 以 `0f63d97e9ab951957490108de83b17b81580578d` 为基线，先补测试于 `89b20a0fe6efc5345765096e435fd491d1600413`，最终实现为 `4cf80c707ed4cf605d0ce4a0ccb98ca27484c474`。普通字节响应复用既有 SHM 准备与 prepared 发送实现，删除重复的分配、复制、元数据和写失败释放代码。返回类型改为 `Result<Option<PreparedShmResponse>, String>`，普通回复采用既有 `free_at` 记账及统一诊断；身份、预算、快照、写锁、取消边界与发送后不重试保留。ABI 5、IPC 12、SHM 3 和业务 wire 保持。

| 范围 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| R13 生产 | 49 | 153 | -104 |
| R13 测试 | 131 | 72 | +59 |
| R13 合计 | 180 | 225 | -45 |
| 发布门禁修复：测试 | 70 | 11 | +59 |
| 发布门禁修复：工作流 | 4 | 1 | +3 |
| 安全依赖、工具与对应文档 | 131 | 124 | +7 |
| 版本与发布准备文档 | 148 | 48 | +100 |

此表截至最终 PR 候选，后续本验收记录另计。R13 的收益是消除第二套实现；本轮没有吞吐或延迟提升声明。写锁前取消保留未发布分配直到 pool 收尾，是基线已存在且按批准边界保留的行为，不算本批修复。两份控制会话重构及 R06/R07 等其他范围继续暂缓。

macOS 的基线 reply 8、response 5，以及候选 server 178、完整 relay 416、真实 portable 25 项通过。Linux 运行时与 relay 共 1225 项通过。Windows x64 开发机 Rust workspace 1668 项、portable 25 项及完整 18 行 direct/relay 收据通过。三端隔离实验端点零残留；本机历史目录身份和元数据未变。独立 R13 审查无阻断，最终仅删除一个未使用测试 import。

首次发布门禁发现两处验证问题。Python 3.14.9 的原生测试恢复了指向 3.14.8 的 Cargo/PyO3 链接配置；既有缓存身份现在包含实际 Python 版本、可执行文件和库/ABI 配置。六个反例在旧脚本失败，修复后完整工作流测试 21 项通过。另一处是观测测试在就绪探测 Opened 之后才订阅；现已在启动前订阅，逐一匹配全部探测的终态后再创建目标客户端。独立审查拒绝了固定只消费一对探测的首版；用 Notify 屏障使首轮超时、次轮到来后恢复，证实首版失败、最终版通过。普通与受控多轮探测均通过三系统补验，全部原目标断言保留，诊断注入没有入库。

修正后的 [PR Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39752678116) 对 `536ad936` 的 9 个必需 job 全部成功。主线、Release Candidate 与两个发布晋升的最终记录见随附证据。发布工作流复用同一候选字节，不重建或覆盖已存在发行文件。

Dependabot #37–46 的 10 条告警均已由 GitHub 标为 fixed，开放数为 0，修复随 [PR #188](https://github.com/world-in-progress/c-two/pull/188) 合并。依赖图确认 Pillow 13.0.0 和 certifi 2026.10.19。Pillow 限于 Python 3.11+ 的 banner 组，SDK 和普通测试保留 Python 3.10；真实读图/缩放/渲染和 3.10 开发环境均已验证。没有通过 dismiss 关闭告警。

Windows ARM64 追加由 [PR #189](https://github.com/world-in-progress/c-two/pull/189) 承载：原生 `windows-11-arm` 完整源码门禁、5 个 ARM64 Python ABI（3.11–3.14、3.14t）与 ARM64 c3；其他平台继续保留 Python 3.10，总矩阵为 35 wheel、6 CLI、1 sdist。安装器依据宿主 OS 架构选择目标。Python/Rust/Node 原生架构和实际 PE Machine 均须匹配；不使用 x64 仿真作为 ARM64 运行验收。对 Windows 最低版本语法检查显式使用 x64 Python 3.10，不用于运行时证明。

完整验证保留 PyArrow 示例：CI 采用固定 Arrow 24.0.0 上游 minimal Python preset，并遵循上游 MSVC ARM64 的 SIMD=NONE 配置构建该测试依赖。首轮审查发现 CSV 配置遗漏并修正，真实 ARM64 构建随后证实默认 xsimd/MSVC ARM64 不兼容；该失败和修正记录保留。新增的 macOS 安装器门禁发现本地 HTTP 镜像构造会依赖反向 DNS；DNS 被禁止的反例先在原夹具失败，使用绑定地址后原安装、校验和关闭断言通过，期限保持 30 秒。两处均完成独立复核。C-Two 运行时及 FastDB 优化配置不因这些验证端修正改变。

最终 ARM64 候选 `cef3ec6c019319b9d98ef4a86083b04aba8a369c` 的 [完整 PR 验证](https://github.com/world-in-progress/c-two/actions/runs/39784956099) 共 10 个 job 全部成功。ARM64 收据中的 26 项源码门禁全部通过，包括完整 relay、普通用户运行、18 行 Rust/Python direct/relay、12 行 TypeScript 调用以及 IPC 内存矩阵；Python/Rust/Node 为原生 ARM64，测试进程收尾均已确认。合并提交 `96fe6583118025c0dc1ecbad7685fee5e315b87d` 与该候选树完全一致。

后续受控诊断还确认两处测试观测假设：owner 测试共享默认目录时可在请求发出前遇到非阻塞 gate 竞争；回滚测试在 registry 移除后、permit 真正释放前观察 scheduler。隔离原 owner 测试上下文、使用现有 scheduler drain 通知后，原 17 个 owner 断言和原回滚断言保留；macOS owner 26、macOS server 178、Windows 开发机 server 176 项通过，端点和进程零残留。生产准入与释放顺序未改。

ARM64 实测进一步发现 x86 Windows PowerShell 的 .NET 架构查询报告仿真的 x64。安装器改用 Windows `IsWow64Process2` 返回的 native machine，自动探测最低要求为 Windows 10 version 1709 / Windows Server 2016 version 1709；显式目标覆盖保留。首个实测探针发现 `[ushort]` 别名不能用于 PowerShell 5.1，修正为 `[System.UInt16]` 后，原生 ARM64 的 PowerShell 7、System32 与 SysWOW64 三种宿主均通过，且最后完整源码门禁通过。旧失败保留为诊断记录，没有跳过断言或重复运行凑通过。

| ARM64 追加范围 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 安装器生产代码 | 22 | 2 | +20 |
| 构建与 CI | 271 | 90 | +181 |
| 测试 | 429 | 181 | +248 |
| 文档 | 36 | 19 | +17 |
| 合计 | 758 | 292 | +466 |

ARM64 追加没有增加 Core 运行时实现。以上增量相对 `b8124f1a`，包含受控测试修正，不与 R13 净减量混算。

README 中英文、Python API 索引、Rust SDK API 表与迁移入口、生命周期和 Windows 指南、CI/CD 与路线图已通过 [PR #190](https://github.com/world-in-progress/c-two/pull/190) 同步。17 个文件 +70/-72，净减 2 行；49 个 Python 导出和 157 处链接/锚点已核对。Rustdoc 修正了 takeover 准备会冻结端点配置的说明；可执行代码未变。

[主线完整 Source Validation](https://github.com/world-in-progress/c-two/actions/runs/39790864318) 的 10 个 job 全部通过。[发行候选](https://github.com/world-in-progress/c-two/actions/runs/39799778643) 来源仍为 `96fe6583`，38 个实际 job 全部成功，sdist 按既有设计只在 Linux x64 生成一次。清单包含 35 wheel、6 CLI、1 sdist、2 安装器、9 个 FastDB 证明文件和 56 份收据；清单 SHA-256 为 `e1a56d223b9e46ff8d1d08be27426b22626b1ccd2b45a78f417771bc3419129c`。

首次两个发布预演被新增 ARM64 校验错误拒绝：macOS Intel 从已验证 sdist 构建的 FastDB wheel 使用 universal2 兼容标签，校验却尝试由标签推断唯一实测平台。[PR #191](https://github.com/world-in-progress/c-two/pull/191) 改为使用已有逐平台 provisioning 收据绑定名称、摘要、cp312 ABI 和目标兼容性。独立审查又发现首版缺失文件名与 null 摘要可能比较相等；完整 prepare 反例先证明错误通过，再用单行 membership 检查修复。最终固定候选 `0c1cb3fe` 的 481 项校验测试、独立复审及 [CI](https://github.com/world-in-progress/c-two/actions/runs/39810872778) 通过。校验脚本净增 5 行、测试净增 51 行；C-Two wheel 仍只允许单一目标，实际发行字节未变。失败预演保留为诊断记录，不属于通过证据。

[Python 发布](https://github.com/world-in-progress/c-two/actions/runs/39812412013) 与 [CLI 发布](https://github.com/world-in-progress/c-two/actions/runs/39812418310) 均成功，分别通过 [Python 预演](https://github.com/world-in-progress/c-two/actions/runs/39811783634) 和 [CLI 预演](https://github.com/world-in-progress/c-two/actions/runs/39811789896)。[PyPI 0.8.0](https://pypi.org/project/c-two/0.8.0/) 的 36 个文件以及 [c3 0.4.0](https://github.com/world-in-progress/c-two/releases/tag/c3-v0.4.0) 的 17 个附件，名称、大小和 SHA-256 与候选计划全部一致；c3 标签指向发行源码 `96fe6583`。发布使用可信主线校验器 `bf26c1e5`，没有重新构建候选。PyPI 环境按已授权发版范围完成正式 reviewer 审核。

macOS ARM64 使用 PyPI 普通安装验证 0.8.0、连接通知和接管 API，确认 FastDB、NumPy 与 PyArrow 未被普通导入加载；另行下载已发布 c3、核对摘要并运行得到 `c3 0.4.0`。两项临时安装均已清理。Arrow/PyArrow 在本轮属于 grid 示例及完整测试依赖，正式 portable 编码入口仍为 FastDB。

Windows 独占临时验证目录清理前逐一核对已留存的 32 份证据，确认无任务进程，再移除最后 37,705,449 字节；Rust 工具链保留。本机额外清理 89,931,737 字节临时安装、SDK 库和下载文件，已完成 worktree 的环境及缓存另清理 221,290,721 字节。源码、Git 历史 bundle、日志与收据保留。历史 IPC 目录与协调文件未动，两份既有 stash 未改。验收文档和最终分支同步不改变已发布产物来源。

结构化验收记录见 [evidence](evidence/2026-10-23-r13-080-release-acceptance.json)。
