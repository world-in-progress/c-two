# 本地绑定与连接失败的系统原因保留

状态：调查及建议，尚未批准实施。连接结束通知候选为
`7bd749122f6bbd5824ed34e1884509b606783ddb`；本方案不包含在该候选中。

## 已核实的缺口

当前连接路径为 `LocalStream::connect → IpcError::Io → TransportError →
CoreError`。Rust 仍保存原始错误，但 `IpcError` 的标准错误链没有转发
source，Python 投影仅输出 transport kind、phase 和 fallback 属性。

启动路径另有两次信息丢失：`ServerLifecycleState::Failed(String)` 在
发布 readiness 失败时先将错误转成文本；等待者随后经
`ServerError::Config` 和 `LifecycleError::Server(String)` 收到文本。
Unix 目录打开失败的上下文包装也会将带 errno 的错误改造成只有 kind
和文本的错误。因此不能仅在 Python 绑定末端增加两个属性。

本次在 Windows 11 x64 的任务虚拟环境中安装 PyPI `c-two==0.7.6`，
通过公开入口实际得到：

| 真实条件 | OS / 本地原因 | Python 结果 |
| --- | --- | --- |
| 连接不存在的管道 | Win32 2 | transport / ipc / pre_dispatch，无结构化原因 |
| 连接 DACL 拒绝的管道 | Win32 5 | 同上 |
| 重复创建同一 C-Two listener | AddrInUse，无原始系统码 | lifecycle / server，无结构化原因 |

原生模块 SHA256 为
`16839977adf211b0563e3e472d5e02890728a89ca130aec775ab0e9fe5051640`。
所有测试句柄关闭后，三个任务管道均不存在。此结果证明已发布 Windows
包的表现，不把包版本代替源码提交身份。下游提供的 macOS 矩阵作为
外部证据保留，本次未重新执行该完整矩阵；Linux 行为尚需实测。

## 最小修改范围

1. 在实际的本地绑定或连接边界捕获一次原因，保留现有消息上下文。
   复用 `c2-local::EndpointIoError { kind, raw_os_error }`，携带实际操作；
   不解析文字，不从 `pre_dispatch` 推断操作，因为它也涵盖握手失败。
2. 将已有生命周期 watch 中的失败负载结构化，沿 Server、Core 原有错误链
   传递。继续由同一个启动 attempt fence 清除旧失败，不增加 `last_error`
   或第二份错误状态。
3. Core 提供原因访问，Python 薄投影沿用已有维护 API 的
   `io_kind`、`raw_os_error` 命名，并增加 `io_operation`（`bind` 或
   `connect`）。建议固定分类为 `permission_denied`、`not_found`、
   `connection_refused`、`address_in_use`、`other`。
   非本地 I/O 失败不伪造原因；纯所有权、模式或 lease 策略拒绝可有类别，
   原本没有系统码时 `raw_os_error` 为 `None`。
4. 文档及真实 OS 条件测试与实现一起更新。消息供人阅读，不成为程序接口。
   复用现有测试夹具和原生 close/reap，不增加探针、轮询或重试机制。

直接替换公开 Rust 的 `ServerLifecycleState::Failed` 和
`LifecycleError::Server` 负载会影响构造与模式匹配，实施前需要明确接受
这项 Rust API 变化。Python 保留 `CoreError` 分类和原有属性，增加原因
属性。C/FFI ABI 5、IPC 12、SHM 3、业务帧和编码均无须改变；错误顺序、
预算、释放、fallback 和生命周期策略不变。该工作增加必要的诊断数据，
收益是让使用方删除替代传输预检和文本解析，不作为净减码项目。

## 必须保留的事实边界

Windows 当前实现使用 `first_pipe_instance(false)`，监听唯一性由内核
lease 保证，以支持旧客户端句柄尚存时重新监听。独立 Win32 实验中，
重复 FIRST_PIPE_INSTANCE 在实例上限为 2 时返回 5，上限为 1 已满时
返回 231。不能将该实验条件当成当前 C-Two 的 listener 实现，也不能
把系统码 5 一律解释为沙盒拒绝。真实 C-Two lease 冲突已有明确
AddrInUse，应保留它；PIPE_BUSY 继续走已有期限与等待行为。

`not_found` / `connection_refused` 只描述本次连接操作，不能证明原服务
进程已退出。Unix 存在 bind 后、listen 前的窗口。使用方据此尝试启动
仍必须经过既有唯一启动/监听所有权机制；不能据此删除端点或覆盖活跃
owner。`fallback_eligible` 仅说明 dispatch 阶段的安全性，不授予清理权。

## 回归与验收

- macOS、非 root Linux：真实权限拒绝、ENOTDIR、缺失端点、保留 socket
  但失去 listener、目录不可访问；断言稳定类别、原始码和实际操作。
- Windows：真实 DACL 拒绝、缺失管道、C-Two listener lease 冲突、
  PIPE_BUSY 的原期限，以及旧客户端句柄存活时重启；保持原生机制。
- 同一启动失败的多个 readiness 等待者得到一致原因；失败后成功重启
  不残留旧原因；配置/合同/期限/握手错误不被误标为 OS 连接失败。
- 原完整 relay、跨语言、关闭与重启门禁，三系统确切候选 GitHub Actions，
  本次端点 socket/lease 零残留。历史目录不作为测试和清理对象。

通过后再由下游使用相同真实传输矩阵验收。实现、发布和下游验收分别记录。
