# 失败启动清理结果：单独修复提案

状态：已单独获批实施，并与公开 Runtime 完成联合验收，见[最终记录](2026-10-28-public-runtime-acceptance.md)。公开 Runtime 已获批准；本项按该方案的基线门槛单独实施，不扩大到新的关闭策略。基线 `e4ddcc85`。以下保留批准时的反例与修复合同，Core 确定性交错与编译后的 Native 终态错误证据在最终记录中分别列明。

## 已取得的证据

隔离 Core 测试复用了现有 startup gate，以合法的 30 ms 启动 / 0 ms 清理预算固定交错，两条测试编译成功并在预期断言处失败：

| 场景 | 实际观察 |
| --- | --- |
| 首次启动失败，具体 Server 的关闭事务仍 pending | `shutdown_without_host(0)` 返回无 barrier/route/IPC 关闭错误。 |
| 旧 Persistent Host 已正常终态，新 Host 启动失败且关闭事务仍 pending | 保留的旧 Host 返回自己的无错误终态。 |

两条事务在放开 startup gate 后均完成；线程 join、随机端点目录关闭/删除均有记录。没有改清理期限或历史目录。Core 的 `shutdown_without_host` 本来只负责客户端，本结果不证明它违反自身合同；问题是 NativeSession 在失败启动后选择上述旧/空观察，再将其解释为本 session 的 completed。这个 Python 投影结论来自源码，尚未通过真实 PyO3 交错测试，不宣称下游已遇到本故障或默认五秒预算已实测复现。

## 最小修复范围

1. Core 通过**本次启动调用**交付该失败 Server 已存在的 `ShutdownTransaction` 的只读回执。回执只持有原事务的 Arc；其中 Server 仍是原 Weak 引用，不多保留 Host、pool 或 payload。
2. 保留 `Runtime::host(options) -> Result<Host, Error>` 的返回即 responsive 语义和原启动错误。将相同构造/等待实现抽为一个窄的 Core 入口，让 NativeSession 接到失败回执；普通 host 仍走相同实现。回执必须在本次失败清理路径内、按具体 Server 取得，不能事后按 server id 搜索，也不设 Runtime 级 last-failure 槽。
3. NativeSession 在**现有 host 锁保护的所有权记录**中保存该回执，旧终态 Host 及 route journal 保留。shutdown 选择本次失败回执时，在原调用期限内观察现有 Condvar/outcome；不能选择旧 Host 或空的 hostless 结果。
4. pending 返回 `completed=false`；事务发布终态后返回其原 outcome 与完成事实，清理错误仍保留。完成与无错误是两个判据，不能因终态含错误把它投影成无错误。重复观察读同一结果，不重发关闭、不修改 `delivered`、不再次消费 route journal。
5. 失败回执保留到原 session 换代，或下一次真实 Host 启动成功；新一次已构造的失败尝试只替换它自己的回执。NativeSession 继续用原 host 锁串行尝试；不同 Core Runtime clone 的启动调用各自得到回执，互不覆盖。现有 pending teardown 与旧注册消费门槛不变，不另加必须先消费回执的重启状态。
6. `native_terminal_outcome` 在有失败回执时按同一对象观察，不能再回报旧 Host 的完成。Python 的公开 Runtime 不新增 pending/closed 标记或另一套结果缓存。

原有 Server 关闭信号、Core 事务发起、监听/路由/客户端释放顺序及独立清理预算保持。实现须在原失败清理已经发起后交付原事务引用，避免为了取得回执额外调用一次带等待的 shutdown。原 startup 错误仍由 start/register 抛出；后续 shutdown 单独报告清理结果。

不采用“遍历并合并全部关闭事务”、仅检查 has_pending_teardown、覆盖旧 Host 槽或新建第二 Host。它们分别会混淆旧结果、漏掉终态错误或丢失原 journal。若实现发现旧 Python hook 尚未消费的受支持路径，先提供具体反例再调整，不能预设新的 journal 合并器。

## 接口、量级与验证

预计生产增加约 80–135 行，测试约 120–220 行；只作静态估算。Rust Core 增加窄的失败回执交付入口与不透明观察类型，既有 host 签名不变；Python 公共函数签名和 outcome 字段不变，completed 的对象选择得到修正。IPC 14、SHM 3、C/FFI ABI 6、owner 控制及凭据格式不变。不留旧错误选择分支或兼容别名。

批准后，先将两条反例接到真实新入口，证明 pending/终态/终态错误/重复观察及并发调用定点绑定；核对回执不修改 delivered、不重复 hook。编译后的 NativeSession/Bridge 测试覆盖首次失败、失败重启及原错误字段。现有 PyO3 构建没有 Core startup gate，需用测试构建可用的既有确定性控制，或明确区分 Core 确定性交错与 PyO3 常规集成证据；不使用反复 sleep/重跑制造通过。

与已批准 Runtime 候选一并完成完整 relay、跨语言、三系统、GitHub Actions、端点零残留和独立审查。Windows 优先开发机；构建产物及时清理。本次基线原始日志、测试补丁及观察记录保留在本任务独立验证目录，本提案已获批准，生产修复和最终门禁已完成，未发版。
