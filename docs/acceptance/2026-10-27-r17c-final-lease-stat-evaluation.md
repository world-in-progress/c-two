# R17c：删除前末次 lease 核对的 I/O 原因保留

状态：R17c 子项验收完成。实现候选 `1e74b4c9463071ae6162619fc20eca4f55634b75` 已通过固定基线反例、候选定向回归、独立审查及确切来源的完整跨平台门禁。最初评估源码为 `d9e3ac9e0e0b40167fb4ec0022c76edae977e9b2`，隔离反例基线为 `f4ab1c7bf9e629a3d2e22df236d0ed7285c12670`。R17a、R17b 的既有验收继续有效；R17 仍部分完成。

验收与收尾：旧分类基线上的 2 个故障反例按预期失败、2 个控制通过；候选 4 个新增场景及 10 个相关既有测试通过。Host 复核日志、生产差异、格式与空白检查；独立只读审查未发现阻塞问题。14 个本轮记录的隔离端点目录已消失，专属 baseline/candidate Cargo target 均已清理。该证据只证明本轮创建的隔离资源已清理，不表示历史默认 IPC 目录为空。实际生产 +7/−1，净 +6；测试场景 +241、既有 cfg(test) 故障点 +14，测试合计 +255，非文档净增 261 行。测试量超过初估，源于覆盖四条调用链、两次目录短路及异常收尾，未新增夹具框架。完整 relay、跨语言、Linux/macOS/Windows x64/ARM64 与 Python 3.14t 由同源 [Source Validation 40462218729](https://github.com/Dsssyc/c-two/actions/runs/40462218729) 全部通过，详见[验收报告](2026-10-27-r17c-final-lease-stat-acceptance.md)及[机器证据](evidence/2026-10-27-r17c-final-lease-stat-acceptance.json)。本批未发版；专用 fork CI 分支已按确切候选 SHA 删除。

## 问题、原设计与选择理由

`core/transport/c2-local/src/unix_common.rs` 的 `lock_entry_matches` 已返回 `io::Result<bool>`：通过目录 fd 对 lease 做 `stat`，缺失、非普通文件或 inode 不同为 `Ok(false)`，真实系统错误仍是 `Err`。唯一消费者 `unlink_verified` 又用 `unwrap_or(false)` 将后者变成 `Unverified(InvalidOwnership)`，丢掉原 kind/errno。

这一步是删除 socket 前的最后一次 lease 身份核对。`75d518f0` 的共享 Unix 工具提取中已有该折叠，`9ebef7e5` 分离 macOS socket/协调目录后继续保留。目录 pin 与 lease inode 的复核防止删除被替换的对象，仍有职责；布尔表达让失败统一停止删除，但现有 `EndpointReapResult::IoError` 已能同时表达停止操作和真实原因，无须再抹去 Result。

R17 的 gate/context 分类涉及 `gate_entry_is_current`、`gate_still_named`、目录 policy 与真实 I/O 混合，以及 `same_as` 和 owner 层再次映射。本轮优先选择末次 lease stat：已有返回类型和四条调用链均能接住修正，不需要把目录错误类型或控制会话一起改造。

## 最小替换范围

生产只改 `unlink_verified` 中这一处条件展开，保留以下执行顺序：

1. socket 目录 pin 核验失败，仍返回原 `InvalidOwnership`，不查询后续 lease。
2. 协调目录 pin 核验失败，同样按原顺序短路；两次目录核验不合并。
3. 原位置调用一次 `lock_entry_matches`：`Ok(false)` 保留原拒绝；`Err(error)` 返回已有 `IoError(error)`；`Ok(true)` 继续。
4. 原 socket stat、类型/属主/身份核对及 `unlinkat` 保持原位置和结果。

不新增 helper、错误类型、缓存、重试或状态机，`lock_entry_matches` 的生产签名和验证职责保持。

| 直接调用链 | 现有承接方式与必须保持的行为 |
| --- | --- |
| stale bind → `remove_stale_managed_socket` | 已把 `IoError` 返回为原 `io::Error`；末次核对失败时，不删除旧 socket/lease、不建立替代监听。 |
| listener close/Drop → `retire_owned` | 已提前返回非成功结果，不继续删除 lease；显式 close 仍结束监听并释放 flock/descriptor，Drop 不新增诊断或重试。 |
| 凭据 reap / sweep → `reap_locked` | 已提前返回 `IoError`，sweep 使用既有 `io_errors` / `last_io_error`；Busy、旧凭据和前置身份拒绝顺序保持。 |
| 初始化回滚 → `BoundSocketGuard::withdraw` / Drop | Drop 保留原初始化错误；核对失败仍不删除 socket，`BindLeaseGuard` 看到 socket 尚在时仍保留 lease。不让清理错误覆盖初始错误。 |

## 公开影响与净量

公开 Rust/Python 签名、私有 helper 签名、凭据 JSON、C/FFI ABI 6、IPC 14、SHM 3、业务帧和生命周期策略不变。Windows 使用独立实现，生产代码不改。

有明确的错误观察语义变化：Linux/macOS 在末次 lease stat 失败时，reap 从 `unverified` 变为已有 `io-error`，Python 现有 `io_kind`、`raw_os_error` 和 `retryable=True` 投影随之生效；库不增加自动重试。stale bind 保留原系统原因，不再改报地址占用；显式 close 沿现有转换进入 I/O 失败路径。CLI reap 仍退出失败，sweep 仍按原 EOF/complete 约定结束，不承诺有 I/O 错误时必然改为失败退出。inspect 不调用这个删除 helper，行为不在本子项中改变。

| 范围 | 静态估算 | 收益 |
| --- | --- | --- |
| `unix_common.rs` 的生产条件展开 | 净增约 5–15 行 | 恢复原 I/O 原因；正常路径不增加 syscall、锁或分配。 |
| 既有 fault 枚举/注入点及 managed 测试 | 净增约 100–180 行 | 覆盖末次故障、对象保留和释放；复用现有私有目录与回收夹具。 |
| 生产与测试合计 | 预计净增 105–195 行 | 正确性修复，无净减量或性能提速声明；实施后按实际值复算。 |

## 基线与回归要求

先在旧分类上补末次 lease stat 的一次性 `EIO` 反例，再改生产分支。现有 `VerifyLeaseStat` 位于更早的 `verify_lease_entry`，不能当作末次核对的覆盖；在现有 fault 机制中补目标 syscall 的注入点，不新增框架。

定向场景包括：stale bind 的原 kind/errno；凭据 reap、sweep、显式 close 的错误及原 socket/lease 内容与身份保留；close/Drop 后原 flock 释放；故障解除后对有效记录正常 reap；目录身份已拒绝时不执行后续故障点；缺失、类型/身份不符仍为原 `InvalidOwnership`。初始化失败回滚继续验证原错误优先、socket 未撤回时 lease 保留。优先扩展既有真实 guard 和末次核对用例，不复制整套 R17b 夹具。

分类反例断言前完成本批自有资源的收尾，验收 socket/lease 零残留。保留已存在的目录替换、双目录、Busy、重复句柄、失败初始化和正常回收覆盖。随后完成完整 relay、适用 Rust/Python/Node 跨语言、三系统及确切候选 GitHub Actions（含 Windows ARM64/3.14t）和独立审查。Windows 开发机优先核对受影响范围；源码未变的已通过检查可复用，公共层变化才补对应实测。验证后及时清理构建产物。

主要风险是拆开条件时提前执行 lease stat、跳过目录短路，或让调用者在 I/O 失败后继续 unlink；上述基线和对象/锁断言针对这些风险。既有身份核验不是删除候选。

## 后续边界

R17c 只处理 `unlink_verified` 的末次 lease stat。R17 的 gate/context 分类（包括 `gate_entry_is_current`、`gate_still_named`）、目录 pin、`NamespaceDirectories::same_as`、sweep 中的目录失效分类和 owner context 映射继续单列，R17 整体仍为部分完成。目录检查同时表达真实 I/O 与策略拒绝，owner 层也会再次归类错误，需另行设计。C04、R35、两份控制会话重构和 R07 剩余所有权继续保持原边界。

两轮原生只读复核分别核查了较大的 gate/context 范围，以及本方案的四条直接调用链；后一轮确认本子项可独立成立，并指出早期 `VerifyLeaseStat` 不能代替新故障点。均未执行测试。hey-my-buddy health 返回 `C_TWO_LIFECYCLE` / 命名空间无法验证，本轮没有维护其历史目录，改由原生子代理完成独立复核。
