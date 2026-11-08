# C-Two 配置指南

本指南负责进程、IPC、relay、并发和生命周期配置；Rust `c2-config` 负责默认值、环境解析和校验，SDK 提供代码覆盖值，c3 提供命令行覆盖值。

[English](configuration.md) · 简体中文

## 配置来源与生效时机

IPC 的调优值遵循一条规则：`ipc_overrides` 的每个键 `name` 都有环境变量 `C2_IPC_<NAME>`（大写），按下面的优先级解析。单次操作的预算是这次调用的参数，例如 `connect(timeout=...)`、`shutdown(timeout=...)`。

优先级为 **显式代码或命令行 > 进程环境变量 > `.env` > Rust 默认值**。默认读取当前工作目录的 `.env`。`C2_ENV_FILE` 指定其他文件，空字符串关闭文件读取。[`.env.example`](../../.env.example) 提供环境变量参考。

```bash
cp .env.example .env
C2_ENV_FILE=./runtime.env python resource.py
C2_ENV_FILE= python resource.py
```

```powershell
$env:C2_ENV_FILE = '.\runtime.env'
python resource.py
$env:C2_ENV_FILE = ''
```

在注册资源、连接客户端之前设置 Runtime 策略。各原生配置域在首次使用时冻结：本地端点在第一次 bind/connect 尝试时冻结，包括失败的尝试；调用准入在第一次远程业务调用时冻结。查询端点或调用统计不冻结配置。改变运行中进程的环境变量不改变已经冻结的选择。

Python 提供 `cc.Runtime()` 来创建独立配置的实例。模块函数使用默认 Runtime；每个显式实例拥有自己的代码覆盖值和原生会话，不复制默认实例的覆盖值。所有实例仍通过原生 resolver 读取同一进程环境和 `.env`。需要配置某个实例时，在对应冻结点前调用 `runtime.set_*()`，不要通过修改 `os.environ` 定向配置另一个 Runtime。`None` 保持各 setter 原有的清除覆盖或继承语义。

## SDK 服务端、客户端与传输

服务端通过 `cc.set_server(...)` 设置身份、IPC overrides 和生命周期；客户端通过 `cc.set_client(...)` 设置自己的 IPC overrides。默认 Runtime 的传输策略通过 `cc.set_transport_policy(...)` 设置；显式实例使用自己的 `runtime.set_transport_policy(...)`：

```python
import c_two as cc

cc.set_transport_policy(
    shm_threshold=64 * 1024,
    remote_payload_chunk_size=1024 * 1024,
)
cc.set_server(
    server_id='my-server',
    startup_timeout=30.0,
    ipc_overrides={'max_payload_size': 64 * 1024 * 1024, 'pool_enabled': False},
)
cc.set_client(ipc_overrides={'pool_enabled': False})
```

Typed overrides 为 `BaseIPCOverrides`、`ServerIPCOverrides`、`ClientIPCOverrides`；服务端专有字段不作为客户端 overrides。全部键及值的校验由原生配置解析完成。

| 用途 | 代码入口 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| Relay 注册与名称发现 | `cc.set_relay_anchor(url)` | `C2_RELAY_ANCHOR_ADDRESS` | 无 relay |
| 服务启动等待 | `cc.set_server(startup_timeout=...)` | — | 5 秒 |
| SHM/inline 分界 | `cc.set_transport_policy(shm_threshold=...)` | `C2_SHM_THRESHOLD` | 4 KiB |
| 远程载荷分片大小 | `cc.set_transport_policy(remote_payload_chunk_size=...)` | `C2_REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB |
| 单个服务端 frame 限额 | `set_server(ipc_overrides={'max_frame_size': ...})` | `C2_IPC_MAX_FRAME_SIZE` | 2 GiB |
| 单条逻辑载荷限额 | `set_server(ipc_overrides={'max_payload_size': ...})` | `C2_IPC_MAX_PAYLOAD_SIZE` | 16 GiB |
| 整个服务端待处理请求 | `set_server(ipc_overrides={'max_pending_requests': ...})` | `C2_IPC_MAX_PENDING_REQUESTS` | 1,024 |
| Resource 执行线程 | `set_server(ipc_overrides={'max_execution_workers': ...})` | `C2_IPC_MAX_EXECUTION_WORKERS` | 默认按 Host 并行度取 4–64；显式值必须为正，且不受 4–64 的限制 |

显式 `address='ipc://...'` 连接独立于 relay 发现直接使用本地 IPC。Relay anchor 负责注册和发现，HTTP 调用使用解析结果中的目标 relay URL。远程分片大小控制 C-Two 载荷批次，不等同于 TCP 包或 HTTP frame 大小。

`createNodeIpcConnector` 创建的 Node 连接器在创建时固定自己的客户端设置：选项 `requestShmThreshold` 与 `requestChunkSize` 设定 SHM 阈值与客户端的 `chunk_size`，其余设置（包括载荷缓冲预算与 `C2_CALL_MAX_OUTSTANDING`）都来自环境变量，与没有覆盖项的 Runtime 相同。

## 服务启动超时

在首次注册前用 `cc.set_server(startup_timeout=seconds)` 设置 Core 的启动等待预算。原生 `RuntimeSession` 构造器和 `set_server_options` 接受同名参数。数值必须有限、非负且能由原生时钟表示；零表示立即到期。省略或 `None` 使用原有的 5 秒有限默认值，不表示无限等待。保留默认值是为了约束无人配置时的启动等待，不保证繁忙运行器能在 5 秒内就绪；应用应明确选择自己的部署预算。

Core 在首次 Host 启动尝试时冻结服务配置，失败也保持冻结；空 Host 的 `runtime.start()` 同样是首次启动尝试。启动前通过 `set_transport_policy` 替换 RuntimeSession 时会保留该设置。`cc.shutdown()` 或显式 `runtime.shutdown()` 消费已完成结果后产生的新 session 可以重新配置；返回 pending 后，须再次调用 SDK shutdown 消费结果，后台完成本身不会换代。Rust 的优先级为显式 `HostOptions::with_startup_timeout` > `RuntimeOptions::server_startup_timeout` > 5 秒。就绪和后续直连 IPC ping 共用等待预算。配置、合同验证发生在等待之前；超时后的原生收尾仍使用独立的 shutdown 预算，因此这不是 `cc.register()` 或 `runtime.start()` 的总墙钟时限。

启动到期产生公开的 `c_two.error.CoreError`：`category='lifecycle'`、`lifecycle_kind='startup_timeout'`，并携带 `startup_stage`（`readiness` 或 `responsiveness`）及 `timeout_seconds`。三个本地 I/O 字段 `io_operation`、`io_kind`、`raw_os_error` 均为 `None`。真实 bind 失败保持原结构化系统原因，消息不再添加错误的 `config error` 前缀。这些诊断不提供重试或清理端点的权限。完整类型及取值范围见[公开错误字段表](local-endpoint-lifecycle.md#public-structured-python-errors)。

## IPC 内存与分片

下面的 pool、budget、reassembly 字段通过对应端的 `ipc_overrides` 设置。具体分配、释放和回退语义见[内存策略](memory-policy.md)。

| Override 字段 | 环境变量 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `pool_enabled` | `C2_IPC_POOL_ENABLED` | `true` | 启用或禁用 buddy SHM 池；独立 SHM 仍可用 |
| `pool_segment_size` | `C2_IPC_POOL_SEGMENT_SIZE` | 256 MiB | Buddy 池每段的字节数 |
| `max_pool_segments` | `C2_IPC_MAX_POOL_SEGMENTS` | 4 | Buddy 池段数上限，范围为 1 到 255 |
| `pool_prewarm_segments` | `C2_IPC_POOL_PREWARM_SEGMENTS` | 0 | 启动时主动映射的 buddy 段数；0 保持惰性分配 |
| `pool_min_retained_segments` | `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` | 0 | 回收后保留的最少空闲 buddy 段数 |
| `pool_decay_seconds` | `C2_IPC_POOL_DECAY_SECONDS` | 60 秒 | 空闲段的衰减时间，单位为秒 |
| `shm_backing_budget_bytes` | `C2_IPC_SHM_BACKING_BUDGET_BYTES` | 8 GiB | 本方向创建的 buddy 与独立 SHM 后备字节预算 |
| `file_backing_budget_bytes` | `C2_IPC_FILE_BACKING_BUDGET_BYTES` | 16 GiB | 本方向创建的文件后备字节预算 |
| `live_reassembly_budget_bytes` | `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` | 8 GiB | 未完成及仍被保留的重组容量字节预算 |
| `reassembly_segment_size` | `C2_IPC_REASSEMBLY_SEGMENT_SIZE` | 64 MiB | 重组池每段的字节数 |
| `reassembly_max_segments` | `C2_IPC_REASSEMBLY_MAX_SEGMENTS` | 4 | 重组池段数上限，范围为 1 到 255 |
| `max_inflight_chunk_frames` | `C2_IPC_MAX_INFLIGHT_CHUNK_FRAMES` | 512 | 服务端同时处理的分片帧数上限；拿不到名额的分片让它的请求以 `ResourceUnavailable` 失败。在途重组达到这个数目时，接纳下一个之前先清扫一次过期的重组，清扫本身从不拒绝 |
| `chunk_size` | `C2_IPC_CHUNK_SIZE` | 128 KiB | 单个分片的字节数 |
| `chunk_threshold_ratio` | `C2_IPC_CHUNK_THRESHOLD_RATIO` | 0.9 | 最大 frame 大小的配置比例；接受并校验，但当前传输选择不使用此值 |
| `chunk_assembler_timeout` | `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` | 60 秒 | 未完成分片重组的超时秒数 |
| `chunk_gc_interval` | `C2_IPC_CHUNK_GC_INTERVAL` | 5 秒 | 分片垃圾回收扫描的间隔秒数 |
| `max_reassembly_bytes` | `C2_IPC_MAX_REASSEMBLY_BYTES` | 8 GiB | 重组缓冲内存的最大字节数 |
| `write_timeout` | `C2_IPC_WRITE_TIMEOUT` | 30 秒 | 写一帧允许的秒数，超时就停用这条连接 |
| `client_idle_timeout` | `C2_IPC_CLIENT_IDLE_TIMEOUT` | 60 秒 | Runtime 缓存的客户端连接不再被引用之后保留的秒数，到时在后台驱逐；`0` 关闭定时驱逐 |
| `max_dedicated_segments` | `C2_IPC_MAX_DEDICATED_SEGMENTS` | 4 | 一个池同时保有的独立 SHM 段数，最多 65536 |
| `spill_threshold` | `C2_IPC_SPILL_THRESHOLD` | 0.8 | 新建 SHM backing 可占到的可用内存比例，取 0 到 1；1 表示不检查 |

`pool_segment_size` 是池段容量，向上取 2 的幂后须能用 u32 表示，即最大为 2 GiB；`max_payload_size` 是单条消息限额，两者独立。关闭 buddy 池仍允许 dedicated SHM、分片传输和文件溢出。Backing budget 和 live-reassembly budget 的 `0` 拒绝正数预留；`pool_decay_seconds=0` 在下一维护周期回收空闲段。

`cc.observe.memory_stats()` 查看原生内存域，`cc.observe.memory_stats()["holds"]` 查看 SDK 租约，二者均为只读统计。它们不表示整个进程的 RSS。

## Relay 与 c3

Relay 单独运行，拥有自己的上游 IPC 配置。应用进程的 `cc.set_client()` 不跨进程设置 c3。

```bash
c3 relay --bind 127.0.0.1:8300 --ipc-pool-enabled false
```

```dotenv
C2_RELAY_BIND=127.0.0.1:8300
C2_IPC_POOL_ENABLED=false
C2_RELAY_IDLE_TIMEOUT=60
```

| 用途 | 命令行参数 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| HTTP 监听地址 | `--bind` | `C2_RELAY_BIND` | `0.0.0.0:8080` |
| Relay 标识 | `--relay-id` | `C2_RELAY_ID` | 生成的 `<hostname>_<pid>_<UUID 前 8 位>` |
| 对外公布地址 | `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | 从 bind 派生 |
| Mesh seeds | `--seeds` | `C2_RELAY_SEEDS` | 空 |
| 上游 IPC 空闲断连 | `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | 60 秒，`0` 关闭时间驱逐 |
| 转发事务数量 | `--call-max-outstanding` | `C2_CALL_MAX_OUTSTANDING` | 1,024 / Relay |
| 载荷缓冲预算 | `--call-payload-buffer-budget-bytes` | `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES` | 16 GiB / Relay |
| 系统 HTTP proxy | — | `C2_RELAY_USE_PROXY` | `false` |
| 反熵交换间隔 | — | `C2_RELAY_ANTI_ENTROPY_INTERVAL` | 60 秒，`0` 关闭反熵 |

`remote_payload_chunk_size` 是唯一的远程 body 分批策略，由 Rust 从传输策略覆盖值或 `C2_REMOTE_PAYLOAD_CHUNK_SIZE` 解析；默认 1 MiB，允许大于零且不超过 128 MiB。Relay HTTP 将其用于发出的请求 body；relay 从上游 carrier 复制响应或流条目时，每片不超过它与上游 IPC `chunk_size` 中较小的一个；`C2_IPC_CHUNK_SIZE` 独立控制 IPC 线格式分片与重组。远程配置值不指定 TCP packet 或 HTTP frame 的边界。

Core relay-aware 客户端进行一次路由观察，随后至多进行一次派发前刷新；此上限固定，不通过 SDK 设置配置。

预注册上游的语法为 `--upstream NAME=SERVER_ID@ADDRESS`，身份与 IPC handshake 一致。上游 budget 参数、mesh 和 dry-run 用法见 [c3 指南](../../cli/README.md)。

Relay 在启动时冻结转发限额，所有业务转发均受约束，包括调用方无限等待的请求。数量限额 `0` 拒绝全部新转发；载荷缓冲预算 `0` 拒绝所有正字节的预约。已知长度的请求体在读取之前一次预约；预算不足时，转发等到有容量释放，或者 HTTP 调用方断开。未知长度的请求体以同样方式预约第一段，之后增长时没有容量就立即失败。超限返回容量错误，不撤销路由。这些限额独立于上游 IPC backing 和 reassembly 预算，不代表整个进程内存；载荷缓冲预算计入哪些数据，见[内存策略](memory-policy.md#payload-buffer-budget)。

Relay 容量不足时，拒绝结果在业务准入前确定。Relay 在读请求体之前给出的每一个拒绝，无论是契约、身份、路由或路由令牌不符，声明长度非法或超限，请求格式被拒，还是容量不足，都先按帧丢弃请求体，受声明长度和 2 秒的总截止时间约束，让仍在发送请求体的客户端收到拒绝，而不是被重置的连接。丢弃时不聚合整包、不解码、不调用资源、不再次申请业务准入，期间容量释放也不改变容量拒绝。带 Expect 头或长度未知的请求体不读取；读取错误、多余字节或超时保留原来的拒绝。未完成或错误的传输仍可能使客户端收不到它。

HTTP 调用方断开后，已经派发的转发仍持有上游连接，直到实际完成；它的请求体在复制到上游之后就释放。Relay 关闭先停止准入，让经它转发的服务端流在步之间结束，以写明 relay 关闭的 `CallCancelled` 收尾，再等待转发和原生客户端清理；已经转发的 unary 调用照常完成。始终不返回的资源方法，或已停止读取的流客户端，会持续占用名额，也会阻塞关闭。

## 本地端点位置与平台差异

| 平台 | 端点 | 位置配置 |
| --- | --- | --- |
| Unix | 配置目录内的 UDS | 代码 `cc.endpoint.set_local_endpoint(root=...)`、环境 `C2_IPC_ROOT`、c3 `--ipc-root`，默认 `/tmp/c2-v3-<uidhex>` |
| Windows | 当前用户 TokenUser SID 下的 Named Pipe | 自动选择；Unix root 配置不适用 |

Unix root 是最终端点目录，实际 socket 位于 `<root>/<32 字符标识>`。自定义目录由应用预建，允许 `0755`：当前用户持有目录，具备读、写、遍历权限，组和其他用户没有写权限。默认目录由 C-Two 创建为 `0700`。Socket 在开始监听前设置并核实为 `0600`。路径允许中文和空格，绑定、连接与探测通过目录句柄定位短名称，目录长度受文件系统限制。C-Two 只管理自身端点、租约及协调文件，不修改已有目录权限、不递归创建父目录、不删除应用目录或无关文件。

macOS 默认端点命名空间的 gate/marker/lease 协调记录放在 `confstr(_CS_DARWIN_USER_DIR)/c2-ipc-coordination-v3`，与 `/tmp/c2-v3-<uidhex>` 下的 socket 分离；默认目录的别名共用同一协调器。两个默认目录均随 namespace 磁盘格式 3 改名。非默认自定义 root 与 Linux 保持同目录布局。默认目录中的 `v3` 取自 namespace 磁盘格式版本；自定义 root 不追加版本名。不同格式各用自己的默认目录，旧默认目录无需同名冷切换。显式共享同一自定义物理 root 不受支持；若出现 `unsupported-namespace-format`，须在全部旧进程退出并禁止重启、完整离线归档该 root 后重建。协调记录与精确回收规则见[生命周期指南](local-endpoint-lifecycle.md)。

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

| 配置 | 入口 | 默认值 | 含义 |
| --- | --- | --- | --- |
| 单次调用等待 | `cc.with_call_options(proxy, timeout=...)` | 继承传输默认 | 单次远程业务调用的调用方等待预算 |
| Relay 逻辑调用截止时间 | `C2_RELAY_CALL_TIMEOUT` | 300 秒，`0` 为无限 | Relay HTTP CRM 调用超时秒数；0 关闭 reqwest 总超时 |
| 有限调用名额 | `cc.set_call_execution_limits(max_outstanding_calls=...)` / `C2_CALL_MAX_OUTSTANDING` | 1,024 / Runtime | 带有限截止时间、尚未结束的调用数上限，包括调用方已不再等待、仍在继续的调用，另加每条服务端流直到它的真实终态 |
| 载荷缓冲预算 | `payload_buffer_budget_bytes=...` / `C2_CALL_PAYLOAD_BUFFER_BUDGET_BYTES` | 16 GiB / Runtime | C-Two 为调用保留的载荷缓冲的字节预算，有无截止时间的调用都计入；见[内存策略](memory-policy.md#payload-buffer-budget) |

直连 IPC 默认无限等待；HTTP 的预算覆盖一个逻辑调用和其内的路由重取。`timeout=None` 显式无限，`0` 在派发前到期，负数和非有限值在入口拒绝。同进程同步调用只接受继承或无限策略。

截止时间到达后结束调用方等待，已经派发的原生事务仍可能继续，直到真实结束；它的输入在传输复制之后就已释放。单次调用截止时间到期不会撤销 relay 路由，也不会使已经取得的 held 响应的载荷所有者失效。错误区分派发前到期与派发状态不确定；后者不能自动重放调用。

有限调用的数量上限 `0` 拒绝所有新的有限调用与新的服务端流；没有截止时间的一元调用不计入这一上限。载荷缓冲预算 `0` 拒绝所有正字节的预约，有无截止时间的调用都一样。预算不足时，调用在自己的截止时间之内等待容量；预算计入哪些数据、什么情况下预约立即失败，见[内存策略](memory-policy.md#payload-buffer-budget)。`cc.observe.call_execution_snapshot()` 查看当前及退休域的调用名额，`cc.observe.memory_stats()['payload_buffer']` 查看这项预算的字节与拒绝次数。示例与错误阶段见[调用截止时间指南](../releases/0.7.1.md#per-call-deadlines-for-remote-calls)。

## 并发与资源生命周期

`@cc.read`、`@cc.write` 声明方法访问方式，未标注方法采用写访问。注册时的 `cc.ConcurrencyConfig` 设置路由 mode、`max_pending` 和 `max_workers`；Native route handle 同时约束同进程直调和远程派发。配置样例见 [Python SDK 指南](python-usage.zh-CN.md)。

服务端 `max_pending_requests` 是所有连接共用的一个信号量，计算等待或使用执行容量的完整远程调用，不随连接数倍增。`max_execution_workers` 单独限制正在执行的远程回调。路由自己的 `max_pending` 和 `max_workers` 也约束同进程调用。机制见[执行准入](runtime-lifecycle.md#execution-admission)。

`cc.shutdown(timeout=...)` 在取得 Python 实例锁后，用一个截止时间约束 Native Host、Core 与 pool 的锁等待和本次关闭观察；Python 实例锁等待不计入该截止时间，已启动的原生收尾不会被等待超时取消；监听关闭与工作排空是两项独立观察，hold 保留的本地副本由 `cc.observe.memory_stats()` 计数。完整接入方式见[生命周期指南](local-endpoint-lifecycle.md)。

## 原生配置权威

路径：`sdk/python/src/c_two/config/`、`core/foundation/c2-config/`。

统一配置由 Rust `c2-config` resolver 解析。Python 保存 SDK 代码级覆盖值，并请求原生 resolver 获取环境、`.env` 和默认值。Relay 服务配置属于独立的 Rust `c3 relay` runtime。

| 文件 | 职责 |
| --- | --- |
| `settings.py` | `C2Settings` 外观，提供 Runtime 所有的 relay anchor、传输策略等代码覆盖值 |
| `ipc.py` | 类型化覆盖结构：`BaseIPCOverrides`、`ServerIPCOverrides`、`ClientIPCOverrides`；Rust 拥有默认值与校验 |

IPC 覆盖键的校验属于 Rust/原生配置解析。Python `ipc.py` 可以暴露类型化覆盖结构并转交映射，但不得保留允许键或禁止键的校验表。

不要恢复 Python 侧 IPC 或 relay 内部默认值校验。SDK 应提供类型化覆盖外观，由 Rust 解析环境和默认值。

调度配置遵循相同边界。Python 可以暴露 `ConcurrencyConfig` 和 SDK 级枚举，但 Rust 拥有解析后的路由并发 handle，包括 mode、`max_pending`、`max_workers` 和关闭状态。Python 必须把完整的类型化配置传给 Rust，并将原生 handle 作为同进程直调和远程派发共同的事实来源。注册后不得保留第二份 Python 调度状态或隐藏的默认策略。

不要为能够从既有规范设置推导的值增加环境变量。

Runtime 身份、池和事务投影遵循 [Runtime 权威](runtime-lifecycle.md#runtime-authority-and-python-bindings)。
