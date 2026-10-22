# bind/connect 系统原因保留验收

本批基线为 `bdc3197867f1eb7d4dd847c1fe60f6655efdf19b`；实现候选为 `57b1cab9c035bc706a1e1e6e3d4cc22bd2f60754`，FastDB 固定为 `abba785ef2a7d3fbee1bbc608393a3e6c7b042c4`。本地 HEAD、分支和标签未移动；候选只推送到 fork 的 `socu/local-io-cause-ci` 用于验证，未发版。

原生 bind/connect 边界保留 `EndpointIoError`，同一个 lifecycle watch 以 `ServerFailure` 传递启动失败，Core 暴露 `LocalIoCause`，Python 薄投影为 `io_kind`、`raw_os_error`、`io_operation`。Rust 的 Failed/Server 负载及 Connect/Bind/Failed 匹配臂属于已批准的 API 变化，没有兼容入口。IPC 12、SHM 3、C/FFI ABI 5、业务 wire、预算、身份及释放规则均未修改。

基线先运行同一真实 OS 矩阵：macOS、非 root Linux 各有 5 个预期失败，Windows 有 3 个预期失败，均缺少结构化原因。Windows 的原 0.2 秒 PIPE_BUSY 期限用例通过。失败断言发生在 shutdown、句柄关闭和子进程退出之后。

独立审查发现首版会误标无 OS 码的 5 秒 phase guard，并混合目录拒绝的外层 kind 与内层 errno。两项分别以真实 Windows 单实例繁忙管道、Unix 目录 rename 反例验证后修正；旧候选失败，新候选通过。原 0.2 秒总预算、默认 5 秒保护、外层 PermissionDenied 身份拒绝和原错误文字均保留。第二轮独立复审无阻塞，未增加递归错误查找、错误缓存或后台机制。

| 环境 | 定向 Python | 原生 Core/IPC/Local/Server/HTTP | 真实跨语言 |
| --- | --- | --- | --- |
| macOS arm64 | 84 通过，6 个 Windows 专属跳过 | 1,207 通过，5 个既有 ignored | Rust/Python direct/relay 25 通过 |
| Linux aarch64 容器 | 非 root UID 1000：47 通过，6 个 Windows 专属跳过 | 1,188 通过，5 个既有 ignored | 确切候选 GitHub Actions |
| Windows 11 x64 开发机 | 85 通过，5 个 Unix 专属跳过 | 1,080 通过，2 个既有 ignored | 确切候选 GitHub Actions |

三套原生验证都包含 c2-http 完整 relay 的 416 项。macOS 与 Windows 原生、Windows Python 使用 `3452d6e`；macOS Python 与 Linux 使用 `abee230`。本机跨语言 25 项作为补充回归，最终同源验收以五组 CI 的确切候选矩阵为准。`3452d6e` 相对 `abee230` 的 9 个 Rust 文件差异已逐文件重跑 rustfmt 比对，确认仅格式变化；`57b1cab` 只改 3 个测试/工具文件，AST 比对确认真实 OS 测试函数体不变，生产源码无差异。确切最终源码的完整平台结果见下表和机器可读证据。

首轮 [GitHub Actions](https://github.com/Dsssyc/c-two/actions/runs/39720048639) 发现本批测试接入遗漏：ordinary pytest 自身 exit 0，runner 因新平台专属用例未登记而拒绝 setup skip。补齐精确的 6 个 Windows、5 个 Unix 用例后，旧 runner 的 6 个反例先失败，新 runner 84 项通过；已有校验函数及所有业务测试断言未改，同平台、未知 ID、错误阶段和缺少前置条件仍失败。第三轮独立复审无阻塞，随后运行新候选的完整 CI；没有依靠原样重跑过关。

[GitHub Actions 完整门禁](https://github.com/Dsssyc/c-two/actions/runs/39722345497) 的 9 个 job 全部成功，源码严格对应上述候选。

| Source Validation | 结果 |
| --- | --- |
| windows-2022, py3.12 | 通过 |
| windows-2025, py3.12 | 通过 |
| ubuntu-24.04, py3.14t | 通过 |
| macos-15, py3.12 | 通过 |
| ubuntu-24.04, py3.12 | 通过 |

每组收据均核实 C-Two/FastDB 源码身份、Rust/Python 18 行 portable 矩阵、TypeScript 12 行真调用矩阵及进程收尾；没有未执行项冒充通过。

本批生产代码 +405/−170，净增 **235 行**；测试及测试工具 +304/−15，净增 **289 行**；合计净增 **524 行**。统计以本批基线为界，Rust 内嵌测试归入测试，不混入此前 owner/连接观察等未提交工作。生产增量比方案估算上限多 35 行，用于完整保留共享启动失败、标准错误链及期限/原生原因区别；测试及工具比估算上限多 9 行，来自平台适用清单及反例补齐。本批属于诊断缺口修复。文档 +85/−1，净增 **84 行**（含本报告及机器可读收据）。

macOS 只改变隔离副本的两个测试命名空间常量；新的 socket/协调根及任务临时根审计 socket/lease 均为零。Linux 容器两个测试 UID 的根均为零，容器已删除。Windows 真实用例断言句柄成功关闭、shutdown 完成和子进程退出；原生门禁包含旧客户端句柄存活后的重新监听。最终任务进程为空，任务目录已清理。历史目录未作修改或回收。

按文件大小统计，本机清理 2,230,338,278 字节构建产物，Windows 清理 2,229,827,979 字节任务产物；工具链、源文件、固定快照及验证日志保留。CI 产物由 GitHub 保留用于审计，本地仅保留文本收据，未保留编译包。两份会话重构继续暂缓。

初次 macOS 构建因本机 C++ 标准头文件查找失败，仅为任务构建补充 SDK 头路径。Linux 在基线构建后覆盖归档、复用同一 target 时出现陈旧 Core 产物，改为新容器直接构建候选后通过；未通过重跑掩盖测试失败。两次环境失败日志保留，未改变生产配置或放宽断言。

独立审查及测试交付均经新版 hey-my-buddy 的固定 artifact 验收并回收 checkout，Host 活跃/待审任务为零。此记录证明源码验证；不声称已发布或完成下游安装验收。详细门禁、日志 SHA256 与清理收据见 [证据](evidence/2026-10-22-local-io-cause.json)。
