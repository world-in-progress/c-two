# R17a：端点 slot stat 错误分类最小方案

状态：R17a 已按批准边界完成；生产 +5、测试 +203 行，属于正确性修复。实施基线 `074d620b`，最终实现/测试来源 `f64afd9e`；真实三端、完整 relay/跨语言、确切来源 Actions 10/10、端点零残留及独立审查见[验收记录](2026-10-26-r17a-slot-io-acceptance.md)。本批未发版，构建产物已清理。原评估基线 `dfa03d16`；IPC 14、SHM 3、C/FFI ABI 6 不变。R17 仅部分完成，其他保守身份分类、R07 剩余所有权、C04、R35 和两份控制会话重构继续单列。

## 实施前调用链、原设计与问题

`EndpointDirectory::stat` 使用 dirfd 相对的 `fstatat(AT_SYMLINK_NOFOLLOW)`；只把 ENOENT 转成 `Ok(None)`，其余错误保持 Err。两个上层分支再次用 `.ok().flatten()` 丢掉了这个区分：

| 入口及范围 | 当前结果 | 拟修正结果 |
| --- | --- | --- |
| `ManagedSweep::inspect_entry → reap_slot_with_directory`，已确认没有 lease，随后 socket stat 失败 | 计入 `AlreadyAbsent` / `already_absent`，实际没有观察到 socket 是否存在 | 原 `IoError`，计入 `io_errors`，保留 `last_io_error` 的 kind/errno |
| 公共 `inspect_endpoint → inspect_managed_at`，读取记录后最后一次 lease stat 失败 | `Unverified(RecordMismatch)`，丢失 OS 原因 | 在未先确认 socket 身份不符时返回原 `IoError` |

公共凭据式 `reap_endpoint → reap_locked` 已对对应 socket/lease stat 显式传播错误，第一项不是该公共入口的缺陷。旧清单的“缺 lease 的 reap 路径”需限定为 sweep 内部路径。当前两个分支都不会因此额外删除未知对象；问题是错误观察、类别与维护计数失真，不应夸成越权清理漏洞。

`da5ca3be` 初次建立固定 gate/owner record 及回收时已包含两处折叠；`9ebef7e5` 迁移 macOS 双目录时延续了原表达。第二次 socket/lease 核对用于拒绝读取期间被替换的对象，避免签发混合身份凭据；gate 无法阻止不遵守协议的目录修改，职责仍然成立。未找到把真实 stat 错误隐去的设计承诺。相邻分支与现有结果枚举已经支持 `IoError`，修正可直接复用。

## 替换范围与顺序

生产仅改 `core/transport/c2-local/src/unix_managed.rs` 两块：缺 lease 分支对 `slot_name(...).and_then(socket_stat)` 的 `Some / None / Err` 显式匹配；inspect 最后一次 lease stat 保留 Result，在原位置执行该 syscall，先判断已确定的 socket identity 变化，再展开 Result，最后保留原 lease 同一文件与 gate 核对。每项检查执行一次，不新增错误类型、helper、配置、缓存、后台重试或兼容入口。

inspect 不能直接在读取 Result 时提前 `Err → IoError`，因为原分支中已确定的 socket 替换优先得到 RecordMismatch。建议的顺序如下：

1. 原 namespace、gate、记录读取、首次身份核对及最后一次 socket stat 的拒绝顺序保持；socket 确实消失仍是 Absent。
2. 在原位置缓存最后一次 lease stat Result；如果已经确认 socket device/inode/record identity 不符，仍返回 RecordMismatch。
3. 展开缓存 Result：真实 Err 返回 IoError；成功的 None/不同 lease 仍是 RecordMismatch；随后保留原 gate 核对，全部满足才返回 Present。

Host 对 socket 同一文件、record identity、四种 lease 结果和 gate 状态的 32 个分类组合做了方案模型核对；只有“socket 身份仍一致、末次 lease stat 失败”的两个组合由 RecordMismatch 变为 IoError。该模型不是原生并发运行证据。gate 与 lease 同时不可验证时，仍按上述既有读取/拒绝顺序处理，不新造身份恢复或重试策略。

本批不更改 `verify_lease_entry`、`read_lease_record`、`gate_still_named` 中把其他失败保守归为身份不可验证的契约；它们还连接 bind 回滚和 listener 关闭，整体统一错误类型会扩大生命周期与启动错误影响面，需要另外评估。也不改变公共 reap 中 Busy 先于后续 incarnation 比较的现有顺序。

## 公开影响与维护成本

Rust/Python 的公开签名、原生 EndpointCredential 类及外部 JSON、C/FFI ABI、IPC/SHM 和业务帧均不改。Windows 使用独立的 KernelManaged/NotApplicable 实现，生产路径不变。Linux、macOS 的目标故障分类有明确语义变化：Python inspect 从 unverified 变为现有 io-error，按原投影带 `io_kind`、`raw_os_error`、`retryable=True`；这不是新增自动重试。Endpoint API 的现有 ErrorKind 拼写保持，不与 CoreError 的字段约定混改。

Rust/Python sweep 的错误计数及最后 I/O 原因会恢复；CLI sweep 只输出 ioErrors，不提供 last_io_error，且到 EOF 仍可 complete/退出 0。本批保留这一遍历完成约定，不能宣称修复后 CLI sweep 必然退出失败。CLI inspect 已有 io-error/退出 1 投影，形状不改。

| 范围 | 估算 | 收益与成本 |
| --- | --- | --- |
| 生产两处分类分支 | 约 20–40 行改写，净增 0–15 行 | 消除 Result→Option 的错误信息损失；保留既有身份围栏，无性能收益声明 |
| 原有 fault 枚举、managed/split fixtures 与公开投影测试 | 预计净增 100–180 行 | 定向命中原生分支并核 kind/errno、计数与未删除对象，不建立测试框架 |
| 生产与测试合计 | 预计净增 100–195 行 | 作为正确性修复，不能记作净减量；实际值在实现后重算 |

## 方案确认前的证据与验收要求

本机 macOS arm64 的 task-owned 双目录机制实验已验证：目录 FD 保持打开时，去掉目录遍历权限仍使 socket 和 lease 的无跟随 stat 返回 EACCES/13；恢复权限后 inode 不变，ENOENT/2 控制也成立。本批 socket/lease 已删除，临时根已移除，未访问历史命名空间。它没有端到端命中完整原生 gate/record 检查后的两个目标窗口，也没有执行 Linux/Windows 实验；不能写作公共 API 已复现。

实施应先在未修基线补目标反例。复用 thread-local、one-shot 的既有 fault 设施，在两个目标 stat 的 Result 处注入错误，经过原分类分支后观察结果，不直接伪造最终 IoError；真实缺失、未知 socket、Busy、旧凭据、目录/lease 替换、双目录别名及原生关闭断言保留。优先扩展现有夹具和测试，核对失败时对象身份及内容不变、错误计数不再混入 already_absent、错误的 kind/errno 未丢失；对复合故障验证已确定 socket 身份不符仍优先。既有 after_gate barrier 太早、整目录权限测试也会在更早位置拒绝，不能把它们当作两处定向红例。

Python/CLI 复用现有真实权限失败用例检查同一 IoError 投影，原生目标窗口由定向分支测试负责；不为端到端注入增加公开测试开关，也不把组合的分层证据说成 Python 端精确命中该窗口。随后做针对性回归、完整 relay、适用跨语言、三系统和确切候选 GitHub Actions，Windows 优先开发机核未改平台投影；socket/lease 零残留、独立审查后提交。已通过且不受影响的验证不重复跑，产物及时清理；不发版。

独立只读核查使用 hey-my-buddy run `cb5f0be3-0163-4bd4-a14e-e778a864691e`、artifact `6fe2fd5b-6ecb-4c59-aac2-35c5671e2843`，输入为上述基线，仅核源码、历史和既有测试，没有构建或运行。其报告指出直接提前返回会改变复合故障优先级，Host 据此细化为缓存 Result 的顺序方案。完整摘要与证据哈希见[评估记录](evidence/2026-10-26-r17-slot-io-evaluation.json)；原始文本在 `~/.codex/diagnostics/c2-r17-evaluation-yig7hv6m/`。
