# 独立本地 IPC 探测：方案与公开接口草案

状态：待用户确认，未实施。基于已验收实现候选 `9f9d4de3` 与文档收尾 `c8b34d7e` 评估。需求来自用户指定的 independent-runtime 说明；本方案只涉及 C-Two，不改下游代码或状态。当前这一项不授权发版。

## 选择与现有调用链

建议只公开 `cc.probe_local_ipc`，用于“当前进程能否在指定位置绑定并连接本地 IPC”的启动前检查。暂不公开 `RuntimeSession`：它同时包含 Host、注册、relay、配置冻结、观察器和关闭事务，远超过预检所需；直接公开后仍无法自动删除固定协调文件。

现有私有调用链为 `RuntimeSession.ensure_host_started` → `Runtime::host` → `Server`；`ping_direct_ipc` → Core control → IPC PING/PONG。前者建立完整服务运行时，后者有重试并将底层错误转为 Configuration 字符串，不能满足本任务的逐阶段原生错误要求。保留既有 ping 的行为；本任务不顺带重做其错误分类。

预检直接复用 `c2-local::LocalListener::bind/accept/close`、`LocalStream::connect` 和原生目录操作。它确认真实 OS listener 已绑定且连接已被接受，不执行 CRM 注册、RPC 握手、SHM 分配、业务调用或 relay 发现，也不把成功解释为日后另一个进程必然能启动。Windows 使用同一套 Named Pipe 后端及既有服务端身份核对。

## Python 公开草案

```python
def probe_local_ipc(*, root: str | None, timeout: float) -> dict[str, object]:
    ...

# Unix：root 是已有、由调用方拥有的父目录。
result = cc.probe_local_ipc(root=str(application_probe_directory), timeout=1.0)

# Windows：Named Pipe 没有文件系统 root。
result = cc.probe_local_ipc(root=None, timeout=1.0)
```

两个参数均显式提供。Unix 的 `root` 必须存在，满足既有 owner/access/no-other-write 规则；它不是一个将被清空的目录。函数只在其中新建一个随机 0700 私有子目录，用它作本次 namespace，结束后删除该子目录。调用方目录及原有内容保持原样。Unix `root=None` 与 Windows 指定文件系统 root 均在产生资源前报 `ValueError`；不退回进程默认目录，不忽略平台不适用的输入。`timeout` 为有限正秒数，复用原生时长转换；入口记录一个截止时间，在准备与同步 bind 前后检查，并把剩余时间交给 connect/accept；不按阶段重置。同步文件操作和 bind 不能被期限抢占，bind 返回时若已超时，立即进入收尾而不再连接。关闭、清理和 worker join 不因该期限被取消，总调用时间可以超过 timeout。等待释放 GIL。

只返回一种结构化结果，不再同时抛一套探测专用异常或公开第二个 Runtime 句柄。参数形状和数值无效仍采用既有 TypeError/ValueError 约定。

```python
# 所有本次资源已确认收尾：
{"status": "ok", "failures": [], "remaining_root": None}

# 绑定被 OS 拒绝，收尾成功：
{"status": "failed", "failures": [
    {"stage": "bind", "status": "io-error", "reason": "PermissionDenied",
     "io_kind": "PermissionDenied", "raw_os_error": 1}
], "remaining_root": None}
```

上述 errno 只作示例，实际原值由系统返回。顶层 status 只有 `ok` / `failed`。failures 先保留最多一个操作主因，再按 close、cleanup 顺序追加各自独立失败。connect/accept 并发时保留最先观察到的失败；同一轮二者均已就绪失败时固定 connect 优先，随后取消另一 future，由取消产生的错误不追加。这里记录观察顺序，不声称推断内核故障的先后时间。收尾不能覆盖主因，同一个原因不重复存两份。每项字段固定：stage 为 `prepare/bind/connect/accept/close/cleanup`，status 复用端点维护的 `io-error/unverified/busy/stale-target` 分类，纯 OS io-error 的 reason 与 io_kind 均使用 inspect 的 ErrorKind 拼写，raw_os_error 保留原 errno。绑定错误若从既有 EndpointBindCause 取得 unverified_reason，则分类为 unverified，reason 使用其 as_str，io_kind=InvalidData，raw_os_error=None；其他 unverified 结果沿用原维护投影。busy 为 coordinator-held，stale-target 为 identity-mismatch。没有把身份拒绝伪装成 OS 权限失败。不存在的字段为 None。TimedOut 不伪造 errno。

只有失败时确有本次目录尚未收回，remaining_root 才返回其路径，便于调用方记录或后续人工处理；它不授予清理权限。Windows 没有目录，恒为 None。正常成功、正常 I/O 拒绝及期限到期且收尾成功，都不遗留文件。若权限或身份变化阻止清理，明确返回 failed 和 cleanup 原因，不能承诺失败情况下仍强制删净。

## 原生所有权与复用

Unix 使用已有 UUID 与目录描述符操作，在验证后的父目录内一次 `mkdirat(..., 0700)` 创建新子目录，并保存本次的目录 pin。地址随机生成，不返回供外部连接。bind/connect 沿正式目录相对寻址路径，长父路径不进入 sun_path。配置来源明确使用空 ConfigSources 加本次 root，不读取主 registry 或继承 relay、池配置。macOS 原生别名判定可能只读 stat 默认目录身份；保留该核验，但不会在默认 namespace 建端点、取 gate、改记录或清理文件，不新增“跳过身份核验”的预检旁路。

在一个原生任务中并发推进 connect/accept，二者使用同一剩余期限；不循环 ping、不重试、不创建观察线程或后台服务。同步适配复用 `c2-ipc::control::blocking` 的现有短生命周期 worker：把该私有 helper 自己的 spawn/runtime/join 错误保留为 `io::Result`，调用内容继续返回自身结果。原 ping/shutdown 的公开返回类型、重试与错误行为保持，新增 probe 的外层创建失败归 prepare。不另写一套 Tokio 运行器，不公开这个内部 helper。返回前 join 完成，不留下探测 worker。

收尾顺序固定为：取消/结束 connect 与 accept → 关闭两端 stream → 现有 listener.close 退休 socket/lease → 终结本次独占私有 namespace → worker 退出并 join。这里明确增加的是一次性私有目录的离线终结，不是普通 namespace 的在线回收能力。该子目录只在本次调用中创建和使用，不发布地址、上下文或可延续的 listener，后续调用总建新目录，不恢复该目录。调用内所有可创建/打开端点的工作结束后才开始终结。

终结复用原 gate、目录 pin 与现场 stat：先核对只剩本次 gate/marker、socket/lease 已退，再对原 gate 做一次非阻塞独占锁尝试。锁被持有则返回 cleanup/busy，不删除协调项；锁取得后保持到目录终结结束。核对原目录和两协调项的本次创建身份，先删除已核验的 marker，最后删除 gate，再在原父目录 fd 下 rmdir 子目录；不扫描、收养或递归删除陌生项。已有 opener 若还在等旧 gate，解锁后必须通过既有 gate-current 校验，此时名称已消失/改变，会拒绝继续使用；若新 opener 在最后删除窗口创建了新条目，rmdir 会失败，报告 cleanup 失败并保留新条目，不能宣称目录已清空。部分删除不回写 marker 或重建 gate，这个一次性目录不再投入运行。替换注入必须证明检查点发现的新 inode 被拒绝；继续保留现有目录 pin/现场身份检查，不能把 stat 到 unlink/rmdir 的两个系统调用描述为原子操作，也不新增抵抗任意同 UID 绕过 gate 恶意改名的承诺。若实现无法保留这些原有围栏，就退回方案讨论，不加回退或弱化校验。

普通 namespace 的 gate/marker 在线寿命、close/reap/sweep 权限和历史目录处理均保持。这个新入口的清理权限来自“本次新建且已结束全部工作”的局部所有权，不能从普通 EndpointCredential、任意 root 或文件年龄取得。

Windows 用随机逻辑地址派生当前 TokenUser 范围的管道与 Global listener lease；绑定、连接身份核对、accept 和 Drop/close 全走正式后端。close 的 NotApplicable 表示无需文件清理，属于成功收尾；不误报探测不适用。Windows 原生 close 目前没有可报告的失败结果：先对 pending instance 做既有 best-effort disconnect，再释放自身句柄/lease，IOCP 取消沿现行机制完成。因此只承诺本调用的句柄所有权与 worker 已释放，不声称每个内核异步取消都已同步完成，也不虚构 close OS 原因。Unix close 可报告的维护失败仍逐项保留。验收用真实管道证明没有本次仍可接入的 listener，而不新增生产轮询。不会触碰主运行时的管道或事件流。

Rust SDK 拟同步公开等价的 `probe_local_ipc(root: Option<&Path>, timeout: Duration) -> LocalIpcProbeResult`，结果包含同一 failures 与 remaining_root；Core/SDK 只转出底层类型及入口。低层原生 probe 状态仅在本次调用栈拥有资源，PyO3 只投影结构化结果，不保留 Python 配置或状态副本。现有 inspect/reap/sweep、CoreError、RuntimeSession 私有边界不改。业务 wire、IPC 14、SHM 3、ABI 6、凭据 schema 4、owner 协议及正常生命周期策略不变。

## 改动量与验证

预计生产代码净增约 220–360 行，测试约 240–380 行，另有接口和迁移文档。它是补公开能力，不计瘦身收益；主要维护成本为“本次新建 namespace 的离线收尾”与阶段错误投影。复用原生传输、目录身份、关闭和同步执行能力；不引入服务注册、公共配置对象、通用 cleanup 框架或兼容入口。实际行数实施后统计；若清理需要扩大普通 namespace 回收权限，应停止并重新确认设计。

先固定基线：现有公开 API 缺该入口，原生正式路径可在长目录绑定；保留用户报告的 sandbox 现象为外部证据，本轮尚未自行执行。批准后覆盖：

- Unix 深目录超过 sun_path、中文/空格路径；成功前后父目录内容相同。
- macOS 真实 sandbox 拒绝 network-bind，保留 bind/PermissionDenied/原 errno；Linux 用真实权限拒绝验证，不假称同一种沙箱。
- bind/connect/accept 超时与失败、close 失败、cleanup 失败和连续双失败；首因及收尾原因均保留，无权限放宽或未知目录删除。
- 父目录、子目录或协调项替换/陌生项；不误删替换者。成功条件必须包括本次 socket、lease、协调文件、临时目录和 worker 全部收回；Windows 按既有句柄/IOCP 语义验收，不把内核管理对象当作磁盘文件。
- 主 Runtime 未使用时不创建/冻结它；主服务及既有 IPC 连接活着时，预检前后配置、地址、事件订阅和连续调用保持。污染 C2_IPC_ROOT/relay 环境也不能重定向探测。
- Windows 开发机真实 Named Pipe 成功与原生身份错误；文件系统 root 输入明确不适用；CI 再覆盖 Windows x64/ARM64、Linux/macOS、完整 relay 与 Rust/Python/Node 互操作。
- 既有 ping/shutdown 的针对性回归，验证 blocking helper 收窄没有改变其错误、期限或重试。最终精确候选跑 GitHub Actions，独立审查后提交并清理构建产物。

批准前不实施、不增测试钩子、不发布。下游替换私有导入由其仓库自行完成。

已完成两轮独立静态方案复核，并收窄了 gate 终结证明、deadline、Windows close 可观察边界及并发首因规则；没有运行本方案的动态实验，也没有实现其接口。
