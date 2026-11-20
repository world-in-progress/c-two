# 配置与默认值

[English](configuration.md)

## 配置来源与生效时机

Rust Core 按 **显式代码或 CLI 覆盖 > 进程环境变量 > `.env` > 默认值** 解析配置。默认读取当前工作目录的 `.env`。在进程环境中设置 `C2_ENV_FILE` 可选择其他文件，设为空字符串则关闭文件读取。Rust 可通过 `ConfigSources` 显式选择来源。每个角色域激活时捕获一次 `EnvCatalog` 用于解析。relay 启动与 `--dry-run` 共用解析器，各自只读取一次。见[环境变量模板](../../.env.example)。

## 环境变量

容量变量分别作用于每个已激活的角色域。`M` 为内存预算字节数，`F` 为文件溢出预算字节数，`N` 为在途调用上限。

| 环境变量 | 默认值 | 显式入口或用途 |
| --- | --- | --- |
| `C2_ENV_FILE` | 当前目录的 `.env` | 进程环境或 Rust `ConfigSources` |
| `C2_RELAY_ANCHOR_ADDRESS` | 未设置，仅直接 IPC | `cc.set_relay_anchor(address)` / `Runtime.set_relay_anchor`（传 `None` 清除代码覆盖，空白字符串抛 `ValueError`），Rust `RuntimeOptions` / `HostOptions` |
| `C2_IPC_ROOT` | Unix `/tmp/c2-v3-<uidhex>` | `cc.endpoint.set_local_endpoint(root=...)`、Rust `LocalEndpointOptions`、c3 `--ipc-root`。Windows 使用命名管道。 |
| `C2_MEMORY_BUDGET_BYTES` | 每个已激活的域 16 GiB | `memory_budget_bytes`、`--memory-budget-bytes` |
| `C2_FILE_SPILL_BUDGET_BYTES` | 每个已激活的域 16 GiB | `file_spill_budget_bytes`、`--file-spill-budget-bytes`。0 禁用文件溢出。 |
| `C2_CALL_MAX_OUTSTANDING` | `M == 0` 时为 0，其余为 `max(64, floor(M / 65536))` | `max_outstanding_calls`、`--call-max-outstanding` |
| `C2_MAX_PAYLOAD_SIZE` | 16 GiB | Host 单条载荷上限，`max_payload_size` 须为正数 |
| `C2_MAX_EXECUTION_WORKERS` | 主机并行度限制在 4–64，取不到时按 4 计算 | Host 的 `max_execution_workers`。显式值须至少为 1，可以超过 64。 |
| `C2_RELAY_BIND` | `0.0.0.0:8080` | `c3 relay --bind` / `-b` |
| `C2_RELAY_ID` | 自动生成 | `c3 relay --relay-id` |
| `C2_RELAY_ADVERTISE_URL` | 由实际绑定地址推导 | `c3 relay --advertise-url` |
| `C2_RELAY_SEEDS` | 空 | `c3 relay --seeds` / `-s`，逗号分隔的 relay URL |
| `C2_RELAY_USE_PROXY` | `false` | 允许 relay 流量使用进程 HTTP 代理设置。Rust `HostOptions::with_relay_use_proxy` 可以覆盖。 |

`M == 0` 时，该域拒绝正字节内存预约及全部新业务调用，包括空调用与空流。同时显式配置正数 `N` 会报配置错误。`M > 0` 时，显式 `N`（含 0）优先于推导值。关闭业务准入后，拒绝、释放和关闭排空仍可继续。见[内存策略](memory-policy.md)。

## 角色域与 SDK 配置

Core Runtime 在首次远端业务连接或调用尝试时创建 `client` 域，在首次 Host 启动尝试时创建 `server` 域。冻结后启动失败仍保持冻结。Host 启动及经 relay 注册或注销路由时，业务客户端域保持未激活。Python `thread_local` 调用计入目标 Host 的 `N`。如果同一进程承载资源并远端调用下游资源，两侧各自计费和占用调用槽。两侧继承相同 `M` 时，合计记账上限为 `2M`。进程 RSS 还包含其他内存。

Python `cc.set_client` 与 `cc.set_server` 接受表中的三个容量字段。`cc.set_server` 还接受 `server_id`、`startup_timeout`、`max_payload_size` 和 `max_execution_workers`。身份和启动超时只通过代码设置。显式创建的 `cc.Runtime()` 使用相应实例方法。在 Rust 中，`RuntimeOptions` 拥有客户端容量、本地端点和发现选项，`HostOptions` 拥有服务端身份、启动、承载上限和服务端容量。每个 Node 连接器拥有一个仅使用客户端域的 Core Runtime，接受 [C ABI](c-abi.md#client-domain-endpoints-and-calls) 所列的三个容量选项。

更新逐字段合并。省略参数（`Keep`，Python `_UNSET`）保留已接受的代码覆盖。`Inherit`（Python `None`）清除该字段的覆盖，恢复环境变量和默认值。`Set` 写入显式值。Core 校验整个候选配置并原子提交。修改 `M` 会保留显式 `N`，除非同一次更新清除或替换它。

角色冻结后，Core 使用该域冻结时捕获的来源解析更新。非法候选配置会先报配置校验错误；有效配置相同即成功，包括空更新和沿用自动选择的 `server_id`。任何通过校验但改变已冻结有效值的更新会报 Rust `LifecycleError::ConfigFrozen`（Python 投影为 `c_two.error.CoreError` 且 `lifecycle_kind == 'config_frozen'`，不存在独立的 `ConfigFrozen` Python 异常类）。另一角色仍可在自身冻结前配置。见 [Runtime 生命周期](runtime-lifecycle.md)。

显式 `ipc://` 地址独立于 relay 锚点，直接使用本地 IPC。路由发现可为 HTTP 调用选择其他 relay。见[契约与路由](contracts-and-routing.md#relay-discovery-and-forwarding)。

## 服务启动超时

用 `cc.set_server(startup_timeout=...)`（单位为秒）或 Rust `HostOptions::with_startup_timeout` 设置原生启动等待。默认五秒。Python 参数须为有限非负秒数，0 立即到期。就绪及随后的直接 IPC ping 共用这段预算。配置与契约校验在等待前执行，超时后的清理使用自己的关闭预算。启动超时投影为 `CoreError.lifecycle_kind == 'startup_timeout'`，并提供 `startup_stage`（`readiness` 或 `responsiveness`）和 `timeout_seconds`。见 [Runtime 生命周期](runtime-lifecycle.md#startup-and-readiness)。

## Relay 与 c3

独立 relay 启动时解析一个 `relay` 域。内存、文件溢出和调用槽覆盖业务转发及其上游 IPC 存储，跨物理连接和重连共用。`c3 relay --dry-run` 校验并显示有效的 `M`、`F`、`N`，不启动监听。`--upstream`（`-u`）仅通过 CLI 指定路由及上游身份。见 [c3 relay](../../cli/README.md#relay)。

## 固定传输常量

| 常量 | 值 | 用途 |
| --- | --- | --- |
| `SHM_THRESHOLD` | 16 KiB | IPC 载荷小于或等于此值时走内联，更大的载荷可以使用 SHM。 |
| `CHUNK_SIZE` | 128 KiB | 标准 IPC 分片大小，relay 上传窗口较窄时可发送合法短片。 |
| `REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB | HTTP 载荷分批大小。TCP 包和 HTTP 帧边界各自独立。 |
| `MAX_HANDSHAKE_FRAME_BYTES` | 8 MiB | 握手帧上限（也用于匹配在途控制请求的路由目录回复），覆盖最多 64 条路由、每条最多 256 个方法。 |
| `MAX_RPC_FRAME_BYTES` | 1 MiB | 双向 RPC 帧上限，覆盖分片和有界内联错误载荷。 |
| `WRITE_TIMEOUT` | 30 s | 单帧写入和 flush 的预算。 |
| `IDLE_CONNECTION_TIMEOUT` | 60 s | 客户端和 relay 上游连接的空闲驱逐时间。 |
| `POOL_DECAY_SECONDS` | 60 s | 空闲 buddy 段的回收时间，预热和最小保留段数均为 0。 |
| `CHUNK_ASSEMBLER_TIMEOUT` | 60 s | 输入尚未收齐时，从最后一次分片入队或实际拷贝活动起计时。 |
| `CHUNK_GC_INTERVAL` | 5 s | 扫描过期重组的周期。 |
| `RELAY_ANTI_ENTROPY_INTERVAL` | 60 s | relay mesh 反熵周期。 |
| `SPILL_THRESHOLD` | 0.8 | 新建 SHM 映射的可用内存启发式比例。 |

## 段几何与服务端线程

请求和响应主池各按 `B = floor(M / 4)` 推导，最多 16 段。重组池按 `B = floor(M / 16)` 推导，最多 4 段。各池段数为 `min(max_segments, floor(B / 65536))`，结果为零时跳过 buddy 分配。否则每段为 `min(MAX_BUDDY_SEGMENT, prev_pow2(floor(B / segment_count)))`，其中 `prev_pow2` 向下取最大的二次幂，`MAX_BUDDY_SEGMENT = 64 MiB`。各池 buddy 总容量受其份额约束，已创建的 backing 计入该域的 `M`。默认 `M` 下，主池最多 16 × 64 MiB，重组池最多 4 × 64 MiB。`M` 低于 256 KiB 时主池不建 buddy 段，低于 1 MiB 时重组池不建 buddy 段，按路径使用内联、独占 SHM、分片或文件溢出。单次重组上限在客户端取路由通告的 `max_payload_size`，在服务端取 Host 自身的值。

设 `P` 为主机并行度（取不到时为 4），`W` 为配置的 `max_execution_workers`，`C = E = clamp(P, 4, 64)` 分别限制入站分片拷贝和流条目编码的并发量。服务端使用两个异步 I/O 线程，通过检查加法溢出设置 Tokio 阻塞线程上限 `T = max(512, W + C + E + 16)`。三类工作都在提交阻塞闭包前取得许可，直到闭包返回才释放，为监听器启停与收尾留出至少 16 个线程。

## 本地端点位置与平台差异

| 平台 | 端点 | 位置配置 |
| --- | --- | --- |
| Unix | 配置目录内的 UDS | 代码 `cc.endpoint.set_local_endpoint(root=...)`、环境 `C2_IPC_ROOT`、c3 `--ipc-root`，默认 `/tmp/c2-v3-<uidhex>` |
| Windows | 当前用户 TokenUser SID 下的 Named Pipe | 自动选择；Unix root 配置不适用 |

Unix root 是最终端点目录，实际 socket 位于 `<root>/<32 字符标识>`。自定义目录由应用预建，允许 `0755`：当前用户持有目录，具备读、写、遍历权限，组和其他用户没有写权限。默认目录由 C-Two 创建为 `0700`。Socket 在开始监听前设置并核实为 `0600`。路径允许中文和空格，绑定、连接与探测通过目录句柄定位短名称，目录长度受文件系统限制。C-Two 只管理自身端点、租约及协调文件，不修改已有目录权限、不递归创建父目录、不删除应用目录或无关文件。

macOS 默认端点命名空间的 gate/marker/lease 协调记录放在 `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`，与 `/tmp/c2-v3-<uidhex>` 下的 socket 分离；默认目录的别名共用同一协调器。两个默认目录均随 namespace 磁盘格式 3 改名。非默认自定义 root 与 Linux 保持同目录布局。默认目录中的 `v3` 取自 namespace 磁盘格式版本；自定义 root 不追加版本名。不同格式各用自己的默认目录，旧默认目录无需同名冷切换。不同磁盘格式不得显式共用同一自定义物理 root；若出现 `unsupported-namespace-format`，须在全部旧进程退出并禁止重启、完整离线归档该 root 后重建。协调记录与精确回收规则见[生命周期指南](local-endpoint-lifecycle.md)。

macOS 会额外拒绝允许修改目录内容、属性或权限的扩展 ACL 条目，包括继承条目；只读、遍历和 deny 条目可用。

```python
from pathlib import Path
import c_two as cc

ipc_dir = Path.home() / 'my-app' / 'ipc'
ipc_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
cc.endpoint.set_local_endpoint(root=str(ipc_dir))
```

共享本地域的资源进程、客户端和 relay 上游使用相同目录。Root 不改变 SHM 或 file-spill 的位置。配置在首次本地 I/O 尝试时冻结，包括失败的尝试。端点凭据保留其捕获的作用域；控制器在确认进程退出后使用精确凭据清理。参见[生命周期指南](local-endpoint-lifecycle.md)和 [0.7.4 指南](../releases/0.7.4.md)。

## 连接截止时间

`cc.connect(CRM, name='resource', address='ipc://server', timeout=0.1)` 为本次连接获取设置总预算。省略或 `None` 不增加调用方截止时间，保留已有阶段保护；`0` 在入口到期，负数和非有限值被拒绝。预算贯穿池等待、连接、握手、路由查询及 relay 发现与获取，重试不重置预算。Rust 使用 `ConnectOptions::new().with_timeout(Duration::from_millis(100))` 和 `Runtime::connect_with_options`。

到期返回 `CallDeadlineExceeded`，details 中 `operation=connect`、`transport_phase=pre_dispatch` 和 `stage` 标明失败位置。连接操作尚未执行资源方法。成功连接后，业务调用的等待由 `cc.with_call_options(...)` 独立设置。

## 调用等待与准入

Rust 与 Python 的直接 IPC 和 relay HTTP 业务调用，以及生成的 TypeScript 直接 IPC 调用，默认均无截止时间。生成的 TypeScript 显式 relay 与 relay-aware 传输默认将 `callTimeoutMs` 设为 `300000` 毫秒（包括后者选中本机 IPC 的调用），传入 `callTimeoutMs: 0` 才取消默认期限。Python 使用 `cc.with_call_options(proxy, timeout=...)` 限制一次逻辑调用的等待，包括重新获取路由，或传入 `None` 表示无限等待。0 在派发前到期，负数和非有限值无效。同进程同步调用接受继承的等待设置或无限等待。连接截止时间独立设置。

到期结束调用方等待。已派发的调用可继续到真实终态，调用槽随之释放。只有派发前的失败可以安全重试。全部远端一元调用与服务端流均计入 `N`，包括无截止时间的调用。见[执行准入](runtime-lifecycle.md#execution-admission)和[诊断快照](memory-policy.md#statistics-and-payload-lifetime)。
