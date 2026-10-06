# 本地 owner 控制原语：接收端单次激活与 Windows watcher 收口

日期：2026-10-06（窄修正同日更新）。输入 HEAD：`4cda55d70bfcfbbfbea0a38a03712e86bc347033`（Buddy 工作区快照，父提交 `0f72d072e3f4f5aa3c51c8251c15d306fc1cafd0` 为方案提交）。
范围：`core/transport/c2-local/src/owner.rs`、`core/transport/c2-local/src/owner/{unix,windows}.rs`、`core/transport/c2-local/tests/owner_control.rs` 与本报告。未 push、未开 PR、未发布、未改版本、未触碰其他 checkout。`Cargo.toml`/`Cargo.lock` 无改动。

本轮只收口既有 OwnerControl OS 原语切片，不重建旧任务 `a74a0e63…` 的实现，不做 Core/Python/CLI/OwnerBound 策略/v2 端点，不引入新抽象或重构扩张。

## 一、问题与官方依据

旧切片在 Windows `Receiver::wait_closed` 里每次等待都 `DuplicateHandle` 复制接收端句柄，再用 `NamedPipeServer::from_raw_handle` 新建一个 Tokio watcher，取消一次等待就丢弃该 watcher。这违反两条硬约束：

1. 关联到 I/O 完成端口的句柄不应再复制或继承。微软 [`CreateIoCompletionPort`](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-createiocompletionport) 页面说明：最好不要通过句柄继承或 `DuplicateHandle` 来共享已关联 I/O 完成端口的文件句柄，因为用这类副本执行的操作仍会产生完成通知，需要仔细权衡；同页还说明一个句柄同一时刻只能关联一个完成端口、关联持续到句柄关闭，且完成端口句柄属于创建它的进程、不能在进程之间共享。
2. Tokio 的 [`NamedPipeServer::connect`](https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.NamedPipeServer.html#method.connect) 与 [`AsyncFd::async_io`](https://docs.rs/tokio/latest/tokio/io/unix/struct.AsyncFd.html#method.async_io) 都是 cancel-safe：取消等待不丢事件、不要求重建注册。因此“取消后重新 `DuplicateHandle` + 重建 watcher”既非必要，也在同一 file object 上制造第二个注册句柄。`NamedPipeServer::from_raw_handle` 的文档也说明它在运行时之外或 I/O 未启用时会报错，即该调用本身就是一次真实注册。

Unix 侧原本在同步的 `pair()` 中即建 `AsyncFd`（需要运行时上下文），且 `take_stdio` 可在激活后拿走已注册的描述符。两端因此有不一致的“激活”语义。

窄修正补记（本轮新增）：Windows 句柄在首次观察到对端关闭后再做第二次重叠读，可能返回 `ERROR_PIPE_NOT_CONNECTED` 而不是原来的 EOF。该 private capability 不支持重连，所以“对端已关闭”必须是接收端的终态事实，而不是 watcher 的临时读数：首次观察到 EOF / 对端 broken pipe / no data 后，后续 `wait_closed` 必须稳定返回同一个 `Ok(())`，不得再发起新的重叠操作。

## 二、修改内容

统一为两阶段生命周期：**未激活**（持有未注册的原始 fd/handle，可整体转交子进程）与**已激活**（持有唯一 watcher，可反复等待）。

`src/owner/unix.rs`
- `Receiver` 改为 `{ pending: Option<OwnedFd>, active: Option<AsyncFd<OwnedFd>> }`。`pair()`/`adopt()` 只做创建、`dup` 与校验，不再注册 reactor，也不再要求 `pair()` 运行于 tokio 运行时内。
- `wait_closed` 首次调用把同一个描述符移入 `AsyncFd::with_interest(…, Interest::READABLE)`；之后每次等待（含取消后重试）复用同一注册。取消只丢弃 readiness guard。
- `take_stdio` 改为 `&mut self`，激活后返回 `InvalidInput` 且不取出、不关闭描述符；未激活时把 `OwnedFd` 交给 `Stdio`。
- 新增 `is_activated()`；`shutdown` 依次取走 `active`、`pending`，各自只关闭一次。

`src/owner/windows.rs`
- `Receiver` 改为 `{ pending: Option<OwnedHandle>, active: Option<NamedPipeServer>, connected: bool, peer_closed: bool }`；删除 `duplicate_noninheritable` 与 `DuplicateHandle` 复制路径。
- `pair()` 仍以 `CreateNamedPipeW`（`PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE`，byte 模式，`PIPE_REJECT_REMOTE_CLIENTS`，单实例，随机私有管道名）建接收端，以同步 `CreateFileW` 建 keepalive 客户端；两者都显式清除 `HANDLE_FLAG_INHERIT`，且都不注册完成端口。
- `wait_closed` 首次调用用 `from_raw_handle(pending.into_raw_handle())` **移动**这一个句柄到唯一 `NamedPipeServer`，即唯一一次 IOCP 关联；此后 `connect()`（Tokio 文档保证 cancel-safe）与 `read` 都在该 watcher 上继续，`connected` 只在连接完整成功后置位。取消等待不会复制句柄、不会新建 watcher、不会二次注册。
- `peer_closed` 是公共接收端保存的真实已观察闭合终态：`read` 返回 `Ok(0)` 或返回 `ERROR_BROKEN_PIPE`/`ERROR_NO_DATA` 时置位并返回 `Ok(())`；`wait_closed` 在入口即短路返回 `Ok(())`，不再发起第二次重叠读，因此 Windows 二次读不可能把它变成 `ERROR_PIPE_NOT_CONNECTED`。本地 I/O 错误（包括未记录对端 EOF 时的 `ERROR_PIPE_NOT_CONNECTED`）照常返回给调用方，不会被洗成“对端已关闭”；`shutdown()` 清除该标志，所以关停后的等待与转交仍是 `BrokenPipe`。命名由 `is_closed_pipe` 改为 `is_peer_closed`，避免把本地关闭语义混入对端状态判断。
- `take_stdio`/`is_activated`/`shutdown` 语义与 Unix 完全一致；`shutdown`/`Drop` 丢弃 watcher，注销并关闭真实句柄一次。
- 句柄/ACL 信任边界写明：`pair()` 生成的句柄非继承、ACL 限定当前 logon SID；`adopt()` 只校验对象类型、pipe 端、byte 模式与 overlapped 属性，不证明句柄创建者身份，也不声称对任意外部输入句柄做过 ACL 认证——来源授权归可信 launcher，且普通业务连接从不走 adopt。

`src/owner.rs`
- 模块文档新增“生命周期契约”：控制端只有 `take_stdio` 一个转交点；接收进程只有首个 `wait_closed` 一次激活；激活后禁止再转交；显式 shutdown/Drop 释放真实 watcher。
- 公开方法 `into_stdio(self)` 更名为 `take_stdio(&mut self)`，因为拒绝路径必须无损：激活后调用要返回明确错误而**不能**顺带销毁一个仍然可用的 watcher（`&mut self` 也让 clippy 的 `wrong_self_convention` 不再成立）。
- 为两个 `unsafe` 采纳函数补 `# Safety` 段，并写明 source/stdin 复制的所有权：子进程只拥有自己的 `dup`/`DuplicateHandle` 副本，继承来的源 fd/handle（stdin）仍归其原所有者，本模块从不关闭它；副本保持未注册直到首次 `wait_closed`。
- 模块文档新增“Terminal closure and trust boundary”一节：对端闭合是可复述的终态、本地错误不得伪装成对端 EOF；并明确 `unsafe adopt` 的来源授权责任在可信 launcher，不宣称对任意外部句柄做过 ACL 认证。

## 三、生命周期与所有权契约

| 阶段 | 控制器进程 | 目标子进程 | 无关子进程 |
| --- | --- | --- | --- |
| 创建 | 两侧句柄非继承；接收端未注册、未激活 | — | 无法取到任何一端 |
| 转交 | 只能 `take_stdio()`（未激活）；成功后本端接收端关闭 | 经显式 `Stdio` 或句柄值继承得到同一 pipe 端 | `FD_CLOEXEC` / 非继承句柄，拿不到能力 |
| 激活 | 激活后 `take_stdio()` 返回 `InvalidInput`，不破坏 watcher | `from_inherited_fd`/`from_inherited_handle` 复制源句柄并拥有副本 | — |
| 等待 | — | 首次 `wait_closed` 建立唯一 watcher；取消后可再等待，不重注册 | — |
| 关闭 | keepalive `shutdown`/Drop 关闭写端，接收端见 EOF | 接收端 `shutdown`/Drop 注销并关闭 watcher 一次 | — |

不变量：任一时刻只有一个字段拥有该 OS 句柄；任一 pipe/file object 只注册一次；不存在“已注册句柄再传给另一进程”的路径；不存在永久阻塞线程或后台任务。

## 四、本轮真实结果（macOS 主机）

环境：aarch64-apple-darwin，cargo/rustc 1.91.0，锁定的 tokio 1.53.1、mio 1.2.0。

| 命令（均 `CARGO_BUILD_JOBS=2`） | 结果 |
| --- | --- |
| `cargo test --manifest-path core/Cargo.toml -p c2-local --test owner_control` | 10 passed / 0 failed |
| `cargo test --manifest-path core/Cargo.toml -p c2-local --lib` | 16 passed / 0 failed |
| `cargo check --manifest-path core/Cargo.toml -p c2-local --target x86_64-pc-windows-msvc --all-targets` | exit 0 |
| `cargo clippy --manifest-path core/Cargo.toml -p c2-local --all-targets`（主机） | exit 0，0 warning |
| `cargo clippy --manifest-path core/Cargo.toml -p c2-local --target x86_64-pc-windows-msvc --all-targets` | exit 0，0 warning |
| `cargo fmt --manifest-path core/Cargo.toml -p c2-local -- --check` | clean（exit 0） |

`owner_control` 集成测试覆盖：EOF；取消一次等待后再次等待（并断言首次等待后才 `is_activated()`、取消不清除激活）；**新增** 观察到 EOF 后连续三次再等待都稳定返回 `Ok`（跨平台，锁死 Windows 二次读不得变为 `ERROR_PIPE_NOT_CONNECTED`）；激活后 `take_stdio` 被拒（`InvalidInput` 且 watcher 仍可等待）；激活后 `shutdown` 幂等并释放 watcher（再等待返回 `BrokenPipe`，此时转交也返回 `BrokenPipe`）；无关子进程得不到 keepalive（控制器关闭后 250 ms 内观察到 EOF，而该子进程仍在运行）；目标子进程经显式 `Stdio` 接收能力、在子进程内确认 `is_activated()`、确认 `take_stdio` 被拒，并在控制器关闭后写出 `closed`；错误 FD/handle 与非 pipe 类型被拒（`EBADF`/`ERROR_INVALID_HANDLE`）。每个测试各自创建私有 pipe，不扫描任何历史 IPC 目录，不串行化其他测试。本轮只重跑 `owner_control`（9 → 10 个测试）与 `c2-local --lib`，未重复原 local 全量套件。

## 五、编译边界与未运行环境

- 本机只有 macOS 主机。Windows 侧结果是**真实编译**：`c2-local` 库、单元测试与 `owner_control` 集成测试都为 `x86_64-pc-windows-msvc` 通过 `cargo check`（用 `--message-format json` 确认 `target.name == "owner_control"` 的目标确实被编译），且 clippy 零告警；未产生可执行文件，也未做链接。
- **未运行**：任何 Windows 真实进程执行、IOCP/事件时序、`ERROR_BROKEN_PIPE`/`ERROR_NO_DATA`/`ERROR_PIPE_NOT_CONNECTED` 实机路径、ACL 与句柄继承的实机验证；Linux 亦未执行。Windows 真运行留给后续 GitHub Actions 作业，本报告不声称 Windows 行为已通过。
- 因此 Windows 特有的“取消后不重注册”与“EOF 终态后二次等待稳定返回 `Ok`”只有源码结构（句柄移动进唯一 watcher、无 `DuplicateHandle`、`peer_closed` 短路）、跨平台 Linux/macOS 上的实测行为与 Tokio/微软文档依据，不含 Windows 实机测量。跨平台新测试只能在 Unix 上真跑，它证明的是公共状态机契约，Windows 走的是同一状态机的 Windows 分支。
- 未运行无关全量 CI；本轮只跑 `c2-local` 聚焦目标。

## 六、未纳入本切片

OwnerBound 状态机与 Runtime 接入、Core/Python/CLI 面、v2 端点命名空间与锁回收、Windows 实机验收均不在本轮范围。`docs/issues/local-endpoint-lifecycle-plan.md` 中 P3 的控制器/服务接入仍待后续任务。

## 参考

- [CreateIoCompletionPort（完成端口关联、DuplicateHandle/继承限制、句柄只关联一个端口）](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-createiocompletionport)
- [Tokio `NamedPipeServer::connect`（Cancel safety）](https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.NamedPipeServer.html#method.connect)
- [Tokio `NamedPipeServer::from_raw_handle`（消费所有权、需要运行时与 I/O）](https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.NamedPipeServer.html#method.from_raw_handle)
- [Tokio `AsyncFd::async_io`（Cancel safety、WouldBlock 重试）](https://docs.rs/tokio/latest/tokio/io/unix/struct.AsyncFd.html#method.async_io)
- [本地端点生命周期方案](../issues/local-endpoint-lifecycle-plan.md)
