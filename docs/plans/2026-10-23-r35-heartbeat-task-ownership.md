# R35：心跳任务归属与终止结果收敛方案

状态：2026-10-24 用户确认继续暂缓，尚未实施。原分析输入为 `3e575acbb4c30414f06caaeb7e6e8c84e503ff3c`，即 R32 凭据子项验收后的主分支；R13 inline 子项验收后仍维持此决定。下面保留此前分析供复核，不再作为待执行方案；两份控制会话重构、R15 的 GC 准入进度、R17 的维护错误分类及剩余错误投影继续独立保留。

暂缓理由：真正改变任务运行方式的仅是“禁用心跳时不创建空任务”，收益集中在禁用配置，任务和资源成本尚未实测；它不要求删除公开 `run_heartbeat`、`HeartbeatResult` 或模块导出。此前把局部资源优化与公开 API 退役捆绑，夸大了整体机制收敛的收益。两者必须分别评估，不能靠接口删除补足净减量。连接事件只报告已知断连，心跳仍可发现进程未退出但连接失去响应的情况；启用/禁用配置均继续保留。下表的 25–45 行是原捆绑方案的静态估算，不是当前批准范围、独立空任务优化收益或已实现减量。

## 架构、调用链与原设计

心跳属于 Rust `c2-server` 的物理连接生命周期。当前 `handle_connection` 先登记连接并创建 `RegisteredConnection`，随后无条件 spawn `run_heartbeat`。当 `heartbeat_interval_secs == 0` 时，该任务只执行永久 pending，仍持有连接、writer 和配置；连接结束时再 abort。`Server::new` 及显式 context 构造入口已经调用 `ServerIpcConfig::validate`，拒绝非有限和负 interval，因此内部只需消费既有启用条件，无须增加接纳层。

`429ee2829`（2026-04-01）同时引入公开 `HeartbeatResult`、独立探测函数和禁用时的 pending 分支；当时 handler 持有必需的心跳句柄并统一 abort。让禁用态也服从同一收尾形状是合理的历史解释，但提交没有记载“禁用时必须创建任务”的要求。`00270c6a` 后句柄已经成为 `RegisteredConnection` 的 `Option`，正常关闭、owner-control 角色切换和 Drop 都已处理 None。旧清单提出的可选任务所有权已成为现行能力，无须再造一层管理器。

当前启用态已经通过 `Connection::end` 发布 `HeartbeatTimeout`，通过 `end_io` 发布 `heartbeat_write` 及 OS 原因。生产调用者从不读取任务返回的 `TimedOut` / `WriteError`，而 `Disabled` 位于永不返回的 pending 之后。连接事件及 snapshot 是正式终止事实，结果枚举是第二份、信息更少的终止表示。仓内唯一生产调用位于 `server.rs`；公开导出仍可能有仓外消费者，删除必须作为明确的低层 Rust API 变更确认。

## 原提案范围与估算（已暂缓）

| 范围 | 替换内容 | 预计物理行净变化，含注释与空行 |
| --- | --- | ---: |
| `core/transport/c2-server/src/server.rs` | 复用 `Option<JoinHandle<()>>`；只在 interval > 0 时创建任务。三处已有 abort / join / Drop 收尾保持原位置。 | 约 +2 |
| `core/transport/c2-server/src/heartbeat.rs` 生产部分 | 探测函数收为内部返回 `()`；删除 `HeartbeatResult` 和禁用永久 pending。保留启用态 timer、PING 编码与实际错误发布。 | 约 −17 |
| `core/transport/c2-server/src/lib.rs` | 心跳模块私有化，删除函数和枚举的公开重导出。 | −1 |
| 同模块及 `server.rs` 既有测试 | 删除 park / Disabled 仿制收尾；将独立拼帧断言合并到真实收到的 PING 字节断言，保留独立手写 golden；在既有 handler 用例补禁用时无主动帧且仍可通信、正常关闭及 peer EOF 的观察；补实际写失败的关闭原因覆盖。 | 约 −10 至 −25 |
| 生产与测试合计 | 不增加状态、helper、配置项、框架或依赖。 | 预计净减约 25–45 |

以上是静态预算，不是已测净量。测试预算包含实际写失败覆盖，不靠删除旧断言或保留只测枚举的替身取得减量；实现后分别核算生产、测试和文档。`build_ping_frame` 继续复用现有 wire encoder，不为扩大减量单独改拼帧实现。

运行时收益明确集中在禁用配置：每条连接少一个 Tokio task、两个额外 Arc 持有和一次配置克隆，连接关闭也少一次取消/等待。旧任务本来就在休眠，不据此宣称周期性 CPU 或普通 RPC 吞吐提升。确认后在隔离运行时用同一组连接记录基线/候选存活任务数，作为这项资源收益的验收；无需向生产增加任务计数接口。

## 原提案 API 与语义影响（未批准）

原提案包含删除 `c2_server::{run_heartbeat, HeartbeatResult}` 及 `c2_server::heartbeat` 外部入口，影响手动运行该协程的低层 Rust 调用者。这不是省掉禁用态空任务的必要条件，现已从下一批候选中撤下；未来若有独立接口设计需求，再单独论证和确认。原提案未涉及用户 Rust SDK `c_two`、Python 和 TypeScript 的现行配置与观察接口，也未涉及 C/FFI ABI 5、IPC 12、SHM 3、owner control 1、业务帧和 PING/PONG 字节。

正常运行语义保持：interval=0 仍禁用主动探测和心跳超时，不影响被动 EOF、Named Pipe 原生事件或本地关闭；启用态仍跳过首次 tick，按原 interval/2 检查 idle，按原 timeout 发布关闭并中断 I/O。配置校验、默认值和错误顺序不移动。正 interval 的普通协议角色确认前仍执行超时保护，但不主动注入 PING；识别 owner-control 后仍先 abort 并等待原心跳任务，再使用原 setup deadline。owner_missing、接管、准入及回收策略均不扩入本项。

正常 handler 收尾继续按取消 queued work → abort/join 心跳 → wait_idle → chunk 清理执行，最后先释放 writer/reader，再析构 registration；Drop 仍在原位置发布关闭、取消仍持有的心跳及撤销登记。保留首次关闭事实胜出、单次事件发布、inflight 独立排空和 SHM 精确释放。禁用态不创建任务后没有对应任务持有者需要等待；启用态的取消及释放顺序保持。

## 重新评估时须保留的回归边界

先在现行生产代码上运行调整后的行为覆盖：真实 PING 完整字节、静默 peer 的 heartbeat timeout、实际写失败、禁用时超过所设 timeout 仍无主动帧且后续 PING/PONG 正常、peer EOF 与本地关闭。删除 Disabled 结果断言前保留并验证已有关闭原因、OS cause、EOF、inflight 和单次发布断言。既有 `sdk/python/tests/integration/test_heartbeat.py` 的禁用/启用业务调用与关闭用例继续使用；其禁用用例本身没有直接读取帧，不能代替 Rust 的无主动帧检查。

主要回归风险是条件放错后改变未识别角色的超时、角色切换漏取消、关闭抢先覆盖 heartbeat 原因，以及用旧枚举断言替代真实 I/O 事实。针对这些边界运行 `c2-server` 心跳与连接收尾、Core connection observation / owner-bound lifecycle、Python heartbeat / connection observation / direct IPC，再完成完整 relay、真实 Rust/Python/Node 链。Windows 优先使用开发机的 Named Pipe；确切实现提交还须通过现行 GitHub Actions Linux、macOS、Windows x64/ARM64 与适用 3.14t 门禁。基线/候选均只使用本批隔离端点，验收 socket/lease/Named Pipe 零残留，并及时清理构建产物；历史目录不作为清理对象。

同步当前配置/观察说明与 `docs/releases/unreleased.md` 的低层 Rust 迁移条目，不改历史报告。实现完成后做独立审查，记录实际净量和任务计数对照；通过验收后再提交，不提前把 R35 计作完成。

## 本轮筛选与复核证据

Host 已核对实际调用方、两处 Server 构造校验、任务三处收尾、原始 Rust 引入提交及现有连接事件。R32 剩余错误转换仍须区分外层诊断和内部 semantic message；当前 `CoreError.error_bytes` 已属于公开接口，其收敛需要独立设计，暂不替换。R15 的同步 GC 仍承担无 timer 时的准入恢复，不能只删除扫描。R39 的异种成功结果主要收益已由 `2946340d` 实现；剩余注销转换主要搬移校验与边界分类，私有 expected Option 仅约两行收益，均不单独开批。

hey-my-buddy 独立只读复核 run `d89aa766-64b6-4c90-8572-6e7744b921aa`、artifact `6604201c-102f-4190-86bd-dfd2b099b9b8` 支持 R35 的现有 Option / 结果退役边界，并建议 R39 暂缓；固定输出仍为分析输入提交，patch 为空。Host 已核对并验收该分析，随后把真实 PING golden 合并及写失败覆盖一并计入本方案测试预算。本轮只编辑方案及清单，没有修改生产/测试、构建、执行候选回归或测量性能。
