# 本地绑定与连接失败的系统原因保留

状态：2026-10-22 已完成。实施基线为 `bdc3197867f1eb7d4dd847c1fe60f6655efdf19b`，确切候选 `57b1cab9c035bc706a1e1e6e3d4cc22bd2f60754` 已通过真实三端、完整 relay、跨语言、GitHub Actions 全部门禁及独立审查；测试端点零残留，构建产物已清理。验收与实际增量见 [报告](2026-10-22-local-io-cause.md)。两份会话重构、R17、历史目录及本地分支/标签保持原边界。

以下保留方案确认时的调查与设计记录；实施后的接口和验证结论以验收报告为准。

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

原设计理由已核实：`6d66e00f` 引入 readiness watch 时采用可 Clone/Eq 的 `Failed(String)`，以便多个等待者观察同一启动结果；`a9111b81` 为 Unix 目录错误补充 owner/mode 上下文，却将原始错误格式化后包装，损失了 raw errno。现有端点维护 API 已有 `EndpointIoError`，只是没有贯通普通连接/启动链。保留 watch 的共享结果和诊断文字，替换丢失信息的载荷即可。

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

具体边界：连接在 `IpcClient::connect_with_deadline` 的实际 `LocalStream::connect` 返回处标记，不能将同为 `pre_dispatch` 的 SHM 预热、握手或合同错误标成 connect；启动在 `LocalListener::bind` / `bind_with_context_binding` 返回处标记，不能把后续 close 或 owner 策略失败标成 bind。低层错误增加具体 connect/bind 来源，原普通读写 I/O 仍承担自身职责；相应重试分类调用点同步迁移，重试条件和总预算保持。

`ServerLifecycleState::Failed` 与 `LifecycleError::Server` 共用一个可克隆的失败值，包含原消息及可选的本地 bind 原因。沿同一个 watch、ServerError 和 Core Error 传递，不新增 `last_error`、旁路缓存或后台任务。Unix 目录上下文保留原 `io::Error` 作为 source，`EndpointIoError` 从该确定来源保留 kind/raw code。Python 只投影 Core 提供的 `io_kind`、`raw_os_error`、`io_operation`；不自行归类系统码。已有码值的权限/缺失等类别使用固定文档集合，非本地 I/O 错误不伪造操作或系统码。原端点维护 API 的 ErrorKind 文本格式不随本批改造。

| 层 | 生产替换范围 | 测试替换/补强范围 |
| --- | --- | --- |
| c2-local | 复用 EndpointIoError；Unix 目录上下文保留 source；保持 UDS/Named Pipe 原生实现 | 现有目录权限、缺失和重绑定用例增加真实 kind/raw code 断言；补实际 DACL 拒绝 |
| c2-ipc / c2-server | 实际 connect/bind 标记、source 转发；替换 watch 的失败字符串；迁移原重试匹配 | 现有连接失败/启动失败用例补操作断言；并发 readiness 观察及失败后重启 |
| c2-core / Rust SDK / PyO3 | 替换字符串化错误转换；Core 原因访问与薄投影 | 公开 Rust/Python 错误属性；握手、合同、配置、期限及关闭错误不被误标 |

静态估算：生产净增 120–200 行，测试在替换只看文本的断言并复用现有隔离/关闭夹具后净增 180–280 行，合计净增 300–480 行，文档另计；尚未形成实现补丁，不能当成实际统计。收益是恢复已存在的系统原因，让下游能删除替代传输预检和文字解析；不承诺 C-Two 仓库净减码，也不将下游尚未删除的代码记入收益。现有 ExternalCause 用于适配器语义错误，OS 诊断仍留在本地错误链，不进入业务 error wire。

直接替换公开 Rust 的 `ServerLifecycleState::Failed` 和
`LifecycleError::Server` 负载会影响构造与模式匹配，实施前需要明确接受
这项 Rust API 变化。Python 保留 `CoreError` 分类和原有属性，增加原因
属性。C/FFI ABI 5、IPC 12、SHM 3、业务帧和编码均无须改变；错误顺序、
预算、释放、fallback 和生命周期策略不变。该工作增加必要的诊断数据，
收益是让使用方删除替代传输预检和文本解析，不作为净减码项目。

此外，低层 IpcError / ServerError 对 connect/bind 的具体表示会增加匹配臂，公开 Rust 的穷尽匹配和上述载荷构造必须同步迁移；不保留旧的 connect/bind 字符串表示或兼容入口。Python 异常分类、现有属性及人类可读上下文保留，新原因属性不授予启动、接管、清理或回退权限。

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

先将真实条件的结构化断言放入未修复基线，记录预期失败后再改生产链。Windows 优先在开发机上完成真实权限、缺失、冲突、繁忙及旧句柄重启；公开 Rust API 变化再以 GitHub Actions 补足确切候选的 Linux/macOS/Windows 门禁。复用既有 close/reap 并核对端点、进程与构建产物收尾。R17 的 inspect/reap 折叠、两份控制会话重构、默认目录及生命周期策略不混入本批。

通过后再由下游使用相同真实传输矩阵验收。实现、发布和下游验收分别记录。
