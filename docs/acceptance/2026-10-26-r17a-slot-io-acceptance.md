# R17a 端点 stat 错误分类验收

R17a 按[批准方案](2026-10-26-r17-slot-io-evaluation.md)实施。基线 `074d620b8c2aa3c59521c1614603ee62c363f9c7`；生产实现 `9212f3ed9a648421dd6d6c6c6a8c976aa000bfb8`，最终实现与测试来源 `f64afd9ed9aaa96ea7db72e0d3f7e1ca2650844a`。本记录及随后文档提交不作为构建来源。未 bump 版本或发版；R17 仅部分完成。

Unix sweep 缺 lease 后的 socket stat 现在显式区分 Some、None、Err，真实错误计入现有 io_errors 并保留 last_io_error。inspect 在原位置读取末次 lease stat 并保留 Result，先拒绝已经确定的 socket 身份变化，再传播 I/O 错误，最后执行原 lease/gate 核对。未改变公共凭据 reap、身份校验、预算、删除授权或生命周期；Rust/Python 签名、凭据 JSON、IPC 14、SHM 3、C/FFI ABI 6 及 Windows 生产实现保持。

| 分类 | 新增 | 删除 | 净量 |
| --- | ---: | ---: | ---: |
| 生产代码 | 22 | 17 | +5 |
| 测试 | 209 | 6 | +203 |
| 文档与验收证据 | 308 | 10 | +298 |

按 Git 行数（含注释、空行）统计，内嵌 cfg(test) 单独归入测试。生产与测试合计净增 208 行，属于正确性修复。生产在原估算内；测试比原 100–180 行估算上界多 23 行，包括真实 socket 替换与 I/O 的复合优先级、子进程隔离和公开投影覆盖。复用既有 fault 枚举、私有目录与原生 close/reap，没有新增框架。正常路径没有新增 syscall、锁、worker 或 payload 复制；未做性能测量，不声明提速。

macOS arm64 的未修基线先加入反例：缺 lease 的 stat 错误被计作 already_absent、末次 lease stat 错误被归为 RecordMismatch、双目录 sweep 错误也被计作 AlreadyAbsent，三项均按预期失败。复合控制先通过“实际 socket 替换仍优先 RecordMismatch”，再在 I/O 分类断言失败。错误注入位于真实 stat 返回值处，经过生产分类；不是伪造最终结果。候选 2 项目标测试、13 项 split 与 4 项相邻回归通过。测试在红断言前也收回本批 socket/lease。

Python/CLI 扩展现有测试，以 task-owned 0777 根目录的真实政策拒绝核对公开 io-error 投影、kind、空 errno、CLI 退出 1 和对象未创建；Windows 保留 kernel-managed 断言。这里验证同一投影，不声称 Python 端精确命中了末次 stat 窗口。CLI sweep 的 complete 仍只表示目录 EOF，错误计数需要另查。

Windows 11 x64 开发机执行 10 项凭据测试及独立原生观察探针，确认 KernelManaged、NotApplicable、错误端点 StaleTarget 与 scoped sweep；源码哈希已回读核对。该探针未启动 listener，因此不冒称 Windows 重现了 Unix stat 故障。

初轮 [Source Validation 40292592087](https://github.com/Dsssyc/c-two/actions/runs/40292592087) 的 Linux 完整线失败于原有 public sweep 断言 reaped=0。隔离 Linux aarch64 容器中，旧基线与独立重建候选均在默认 gate 被持有时得到 reaped=0、busy=2、complete=true，并命中相同断言；无竞争时均通过。复用 TestNamespace 改为私有 context 后，相同外部竞争下两版均回收成功。源码策略本就允许 Busy 后续再扫；原 CI 没记录瞬时 gate 持有者，受控实验证明竞争足以触发失败，不冒称捕获了原现场的持有者。

最终只把该测试的目录来源换成现有私有夹具，4 行增、4 行删，公共 bind/scope/sweep 派发、10 秒期限、单条预算与所有断言不变；gate 竞争与默认/双目录专项覆盖仍保留。实际最终源码在隔离 Linux 完整 c2-local 套件 118 项通过、1 项既有 ignored，socket/lease 零残留。首次共用 target 的候选诊断没有重新编译，记录弃用；新容器独立编译后完成上述候选对照。

最终 [Source Validation 40299316629](https://github.com/Dsssyc/c-two/actions/runs/40299316629) 对 `f64afd9ed9aaa96ea7db72e0d3f7e1ca2650844a` 在 attempt 1 全部 10/10 通过。六条平台线的 run-evidence 均验证源码、workflow SHA 与固定 FastDB 来源一致，147 个适用门禁 passed/exit 0 且进程已退出。每线严格验证 Rust/Python 18 行及 Node 12 行矩阵；完整 relay、原生 memory、公开 Python/CLI 投影、Windows 普通用户消费者均通过。Linux/macOS 原生日志确认目标错误分类与修订后的 public sweep 测试通过。

| 平台线 | 门禁 | Rust/Python | Node |
| --- | ---: | ---: | ---: |
| Linux x64 / Python 3.12 | 25 | 18/18 | 12/12 |
| macOS arm64 / Python 3.12 | 25 | 18/18 | 12/12 |
| Windows 2022 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 2025 x64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Windows 11 ARM64 / Python 3.12 | 27 | 18/18 | 12/12 |
| Linux x64 / Python 3.14t | 16 | 18/18 | 12/12 |

首轮的通过记录与失败现场均保留；因夹具已修订，最后重新执行确切提交的现有完整 Actions 门禁。本机及 Windows 开发机未受影响的成功检查直接复用。文本下载核 ZIP CRC，并逐一核 run-evidence 内的 SHA-256；仅记录 GitHub 的整包 digest，不宣称重新计算未下载二进制的整包哈希。

独立实现审查 run `39c3e032-69a2-4843-9016-eae5637ed70a`、artifact `10107c3c-7b6a-41ce-80a4-e3d5df25bd00`，固定输入 `caa68ca0`；夹具差异审查 run `19d62c69-a153-474e-8d99-d5945951e031`、artifact `ecebd83e-1902-4d07-931d-c98254176799`，固定输入 `26efd744`，均无阻断发现。前者提示的 Python 参数类型注解已补齐；后者确认私有 context 保留公共派发和全部断言。均为静态审查，未独立运行测试，Host 已接受报告并回收工作树。曾委派的公开测试编写任务因 buddy service-stop 取消，没有交付代码，由 Host 完成并结案。

本机基线/候选 target 分别清理 157729056 / 157730043 B；Windows target 清理 213807471 B，task temp 与归档均不存在。Linux 诊断容器、其 target/registry 及本次拉取的 Rust 镜像已移除。仅保留文本证据与小型源码，位于 `~/.codex/diagnostics/c2-r17a-implementation-j4rx68ra/`。历史命名空间未挂载到容器或执行维护；本地分支/标签未改，仅使用 fork 专用 CI 分支。专用 CI 分支在全部验收后按确切 SHA 删除。

[机器记录](evidence/2026-10-26-r17a-slot-io-acceptance.json)保留来源、平台、反例、独立审查、清理与 CI 核查摘要。R17 的 read_lease_record、verify_lease_entry、gate_still_named 分类继续单列；R07 剩余所有权、C04、R35 和两份控制会话重构继续暂缓。
