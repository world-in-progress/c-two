# 0.8.1 启动期限与错误分类修复

用户批准优先修复 Python 服务注册不能配置启动期限、以及启动超时和本地 I/O 失败被显示为配置错误的问题，完成验证后发布 C-Two 0.8.1。分析基线为 `e11a67915b53d0767ebb98e29d69853609cc01d5`；R16 已批准方案在本次发布之后继续，两个控制会话重构仍暂缓。

## 范围与设计

已核实调用链为 Python `cc.register()` → `NativeServerBridge.register_crm()` → native `RuntimeSession.register_route()` → `ensure_host()` → Core `Runtime.host(HostOptions::default())` → `Server.wait_until_responsive()`。默认 `startup_timeout` 为 5 秒，Python 原配置入口没有传递它。就绪超时直接构造 `ServerError::Config`，而 `ServerError::Failed` 的 Display 也统一增加 `config error`，后者错误地覆盖了已经保留的 bind 系统原因。

在现有 `cc.set_server(...)` 和 native RuntimeSession 配置入口增加启动期限，Core 保持配置权威，Python 只投影秒数。Rust `HostOptions.with_startup_timeout()` 的显式值优先。未指定时保留现有 5 秒默认，目的是维持无人设置时的有限启动等待和所有者启动边界，不把它当作任何机器的性能保证；部署方可依据自己的启动预算覆盖。参数验证、冻结及失败重试时的配置行为须有真实覆盖。

启动期限到期使用可编程识别的原生错误分类，经现有错误链投影到 Python CoreError，不能依靠消息解析或伪造本地 I/O 原因。就绪、就绪后 ping 及所有者启动围栏中的期限须检查一致性。真正的配置错误仍是配置错误，本地 bind 失败保持原 EndpointIoError 信息并移除错误的配置标签。

保持 IPC 12、SHM 3、C/FFI ABI 5、业务帧、CRM 哈希、身份校验、准入和释放策略；不增加旧分类别名或兼容路径，不改历史 IPC 目录。若发现必须调整这些边界，先记录并讨论。

## 执行阶段与退出条件

1. 基线反例：在生产修复之前补确定性的就绪延迟/期限测试及 Python 配置和错误分类反例；记录实际失败，区分人工延迟复现与下游运行器拥塞的原因推断。钩子只用于测试，不提供环境变量或发行包逃逸入口。
2. 最小修复：贯通 Core、PyO3 和 Python 配置，保留原生就绪与关闭机制；修复超时及 I/O 分类；针对性回归通过后接受独立审查。
3. 候选验收：完整 relay、真实跨语言、适用 Linux/macOS/Windows x64 与 ARM64 门禁；Windows 优先开发机。每个结果绑定实际源码提交，验收本轮 socket/lease 零残留，及时清理已不用的构建产物。
4. 0.8.1 发布：同步 API、配置文档、README 与发布说明中的前后行为；更新版本并使用既有候选构建和发布流程。发布后核对 PyPI 全部 wheel/sdist 的名称与摘要，特别是 win_arm64，不将构建成功等同于已发布。

## 当前状态

基线反例已完成。仅 `cfg(test)` 的 Host 线程屏障阻挡真实启动，默认期限在约 5.013 秒、显式 20 毫秒在约 22.6 毫秒触发，两者都错误地显示 `config error`，原生关闭已完成且本轮端点根零 socket/lease 残留。Python 3.12 基线有 3 个预期失败：`cc.set_server(startup_timeout=0)` 不接纳参数，以及真实权限不足、非目录 bind 错误仍含配置标签；原 I/O 字段断言通过。此实验复现期限与分类，未把人工阻挡等同于已查明下游 CI 拥塞的具体来源。

独立设计复核完成，要求补齐 owner fence 超时信息穿过 ServerFailure、Core 原子配置冻结、RuntimeSession 替换时的期限迁移，以及超时后的原生收尾。启动等待预算与随后清理耗时分开说明。候选 `88b8d88d` 的独立只读实现审查无阻断，两处 Rust 指南中的结构体字段和变体清单已补齐。Windows 11 开发机完成启动、系统原因、连接观察、所有者、完整 relay 和真实跨语言验证；最终候选及主线的三系统 GitHub Actions 全部门禁通过，包含原生 Windows ARM64。

Python 3.12 的候选定向回归为 101 passed、6 个 Windows 专属场景在 macOS 上按原平台条件跳过；这些场景仍须在 Windows 实跑。测试覆盖参数接纳、非法值不修改配置、零期限、session 替换保留设置、失败后的冻结与完整 shutdown 后重配、配置错误优先级及真实 IPC 调用。原生测试已覆盖 Runtime 值继承、显式 Rust 值优先、owner 围栏超时穿过 watch、就绪和 ping 不重启预算。

隔离回归还暴露了旧 `client_modes` 夹具的目录不一致：Runtime 读取了 runner 的自定义根目录，但模拟 relay header、低层 listener 和 inspector pool 仍选择平台默认目录。修复前基线 `4cc46e9e` 在同一私有根目录复现；夹具改为传递同一原生上下文并显式 close listener 后，原 15 个断言全部通过，私有根目录零 socket/lease 残留。首次失败运行落入默认命名空间的一个 `freeze-stall` 测试端点，经记录地址、日志创建时间窗口、测试进程已退出及记录摘要确认归属，再通过原生 inspect/reap 精确回收；未批量清理历史条目。该测试修正单独提交为 `4437fdab`，不计作生产修复。

首轮 PR 完整门禁发现一个本批遗漏：内存观察测试的 `FailingSwapSession` 替身没有新增的 `startup_timeout` 投影，导致构造失败路径在原断言之前遇到 AttributeError。macOS 定向运行复现同一失败；只补 `startup_timeout = None` 后，原内存观察文件和启动配置测试共 17 项通过。生产代码及原断言未改，失败门禁保留为诊断证据。

macOS 门禁另有一次 incarnation 测试的单次 inspect 返回 `io-error`，原断言未输出具体原因。隔离占住本轮新建目录的 gate 后，已发布的 0.8.0 与当前 0.8.1 都使原测试在同一断言以 `WouldBlock` 失败；两次原生 shutdown 后零 socket/lease 残留。这证明共享目录竞争是原测试未隔离的已存在条件，不能倒推原 CI 未记录的具体 errno。该 Unix 分支改为复用已有子进程助手及独立临时根，原身份断言保留并附诊断；Windows 分支保持。全文件 59 项通过，在模拟其他 worker 持有共享 gate 时隔离分支也通过且零残留。未增加生产重试、轮询或放宽检查。

0.8.1 已从主线 `ff256e6a7538a8a0a81dcd19dfc640cce40f48a3` 的已验证候选发布，PyPI 的 35 个 wheel 和 1 个 sdist 全部与晋升计划名称、大小及 SHA-256 一致，其中 5 个为 Windows ARM64。macOS 从官方 PyPI 安装后的启动配置、结构化失败、真实 IPC 和关闭验收通过，隔离根目录零 socket/lease 残留。本轮临时构建与安装环境已清理；R16 尚未实施。完整来源、门禁、审查、增量和清理记录见 [发布验收](2026-10-23-081-startup-release-acceptance.md)。
