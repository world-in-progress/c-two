# C-Two 配置指南

本指南覆盖 C-Two 0.7.1 / c3 0.3.1 的 SDK、IPC、relay、并发和生命周期配置。Rust `c2-config` 负责默认值、环境解析和校验；SDK 提供代码接口，c3 提供命令行接口。

## 配置来源与生效时机

SDK 配置优先级为 **显式代码配置 > 进程环境变量 > `.env` > Rust 默认值**。c3 的显式命令行参数覆盖环境和默认值。

默认读取当前工作目录的 `.env`。`C2_ENV_FILE` 指定其他文件，空字符串关闭文件读取。可从 [`.env.example`](../.env.example) 复制完整环境变量模板：

```bash
cp .env.example .env
C2_ENV_FILE=./runtime.env python resource.py
C2_ENV_FILE= python resource.py
```

Windows PowerShell 的等价设置：

```powershell
$env:C2_ENV_FILE = '.\runtime.env'
python resource.py
$env:C2_ENV_FILE = ''
```

在注册资源、连接客户端之前设置进程策略。各原生配置域在首次使用时冻结：本地端点在第一次 bind/connect 尝试时冻结，调用准入在第一次远程业务调用时冻结。查询端点或调用统计不冻结配置。改变运行中进程的环境变量不改变已经冻结的选择。

## SDK 服务端、客户端与传输

服务端通过 `cc.set_server(...)` 设置身份、IPC overrides 和生命周期；客户端通过 `cc.set_client(...)` 设置自己的 IPC overrides。进程传输策略通过 `cc.set_transport_policy(...)` 设置：

```python
import c_two as cc

cc.set_transport_policy(
    shm_threshold=64 * 1024,
    remote_payload_chunk_size=1024 * 1024,
)
cc.set_server(
    server_id='my-server',
    ipc_overrides={
        'max_payload_size': 64 * 1024 * 1024,
        'pool_enabled': False,
    },
)
cc.set_client(ipc_overrides={'pool_enabled': False})
```

Typed overrides 为 `BaseIPCOverrides`、`ServerIPCOverrides`、`ClientIPCOverrides`；服务端专有字段不作为客户端 overrides。全部键及值的校验由原生配置解析完成。

| 用途 | 代码入口 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| Relay 注册与名称发现 | `cc.set_relay_anchor(url)` | `C2_RELAY_ANCHOR_ADDRESS` | 无 relay |
| SHM/inline 分界 | `cc.set_transport_policy(shm_threshold=...)` | `C2_SHM_THRESHOLD` | 4 KiB |
| 远程载荷分块大小 | `cc.set_transport_policy(remote_payload_chunk_size=...)` | `C2_REMOTE_PAYLOAD_CHUNK_SIZE` | 1 MiB |
| 单个服务端 frame 限额 | `set_server(ipc_overrides={'max_frame_size': ...})` | `C2_IPC_MAX_FRAME_SIZE` | 2 GiB |
| 单条逻辑载荷限额 | `set_server(ipc_overrides={'max_payload_size': ...})` | `C2_IPC_MAX_PAYLOAD_SIZE` | 16 GiB |
| 每连接待处理请求 | `set_server(ipc_overrides={'max_pending_requests': ...})` | `C2_IPC_MAX_PENDING_REQUESTS` | 1,024 |
| Resource 执行线程 | `set_server(ipc_overrides={'max_execution_workers': ...})` | `C2_IPC_MAX_EXECUTION_WORKERS` | Host 并行度，限制在 4–64 |

显式 `address='ipc://...'` 连接直接使用本地端点。Relay anchor 负责注册和发现，HTTP 调用使用解析结果中的目标 relay URL。远程分块大小控制 C-Two 载荷批次，不等同于 TCP 包或 HTTP frame 大小。

## IPC 内存、分块与心跳

下面的 pool、budget、reassembly 字段通过对应端的 `ipc_overrides` 设置。具体分配、释放和回退语义见[内存策略](memory-policy.md)。

| Override 字段 | 环境变量 | 默认值 |
| --- | --- | --- |
| `pool_enabled` | `C2_IPC_POOL_ENABLED` | `true` |
| `pool_segment_size` | `C2_IPC_POOL_SEGMENT_SIZE` | 256 MiB |
| `max_pool_segments` | `C2_IPC_MAX_POOL_SEGMENTS` | 4 |
| `pool_prewarm_segments` | `C2_IPC_POOL_PREWARM_SEGMENTS` | 0 |
| `pool_min_retained_segments` | `C2_IPC_POOL_MIN_RETAINED_SEGMENTS` | 0 |
| `pool_decay_seconds` | `C2_IPC_POOL_DECAY_SECONDS` | 60 秒 |
| `shm_backing_budget_bytes` | `C2_IPC_SHM_BACKING_BUDGET_BYTES` | 8 GiB |
| `file_backing_budget_bytes` | `C2_IPC_FILE_BACKING_BUDGET_BYTES` | 16 GiB |
| `live_reassembly_budget_bytes` | `C2_IPC_LIVE_REASSEMBLY_BUDGET_BYTES` | 8 GiB |
| `reassembly_segment_size` | `C2_IPC_REASSEMBLY_SEGMENT_SIZE` | 64 MiB |
| `reassembly_max_segments` | `C2_IPC_REASSEMBLY_MAX_SEGMENTS` | 4 |
| `max_total_chunks` | `C2_IPC_MAX_TOTAL_CHUNKS` | 512 |
| `chunk_size` | `C2_IPC_CHUNK_SIZE` | 128 KiB |
| `chunk_threshold_ratio` | `C2_IPC_CHUNK_THRESHOLD_RATIO` | 0.9 |
| `chunk_assembler_timeout` | `C2_IPC_CHUNK_ASSEMBLER_TIMEOUT` | 60 秒 |
| `chunk_gc_interval` | `C2_IPC_CHUNK_GC_INTERVAL` | 5 秒 |
| `max_reassembly_bytes` | `C2_IPC_MAX_REASSEMBLY_BYTES` | 8 GiB |
| `heartbeat_interval`（服务端） | `C2_IPC_HEARTBEAT_INTERVAL` | 15 秒 |
| `heartbeat_timeout`（服务端） | `C2_IPC_HEARTBEAT_TIMEOUT` | 30 秒 |

`pool_segment_size` 是池段容量，`max_payload_size` 是单条消息限额，两者独立。关闭 buddy 池仍允许 dedicated SHM、分块传输和文件溢出。Backing budget 和 live-reassembly budget 的 `0` 拒绝正数预留；`pool_decay_seconds=0` 在下一维护周期回收空闲段；`heartbeat_interval=0` 关闭心跳。

`cc.memory_stats()` 查看原生内存域，`cc.hold_stats()` 查看 SDK 租约，二者均为只读统计。它们不表示整个进程的 RSS。

## Relay 与 c3

Relay 单独运行，拥有自己的上游 IPC 配置。应用进程的 `cc.set_client()` 不跨进程设置 c3。

```bash
c3 relay --bind 127.0.0.1:8300 --ipc-pool-enabled false
```

环境文件的等价配置：

```dotenv
C2_RELAY_BIND=127.0.0.1:8300
C2_IPC_POOL_ENABLED=false
C2_RELAY_IDLE_TIMEOUT=60
```

| 用途 | 命令行参数 | 环境变量 | 默认值 |
| --- | --- | --- | --- |
| HTTP 监听地址 | `--bind` | `C2_RELAY_BIND` | `0.0.0.0:8080` |
| Relay 标识 | `--relay-id` | `C2_RELAY_ID` | UUID |
| 对外公布地址 | `--advertise-url` | `C2_RELAY_ADVERTISE_URL` | 从 bind 派生 |
| Mesh seeds | `--seeds` | `C2_RELAY_SEEDS` | 空 |
| 上游 IPC 空闲断连 | `--idle-timeout` | `C2_RELAY_IDLE_TIMEOUT` | 60 秒，`0` 关闭时间驱逐 |
| 系统 HTTP proxy | — | `C2_RELAY_USE_PROXY` | `false` |
| 反熵交换间隔 | — | `C2_RELAY_ANTI_ENTROPY_INTERVAL` | 60 秒 |
| 客户端路由获取次数 | SDK 原生配置 | `C2_RELAY_ROUTE_MAX_ATTEMPTS` | 3，范围 1–32，`0` 按 1 |

预注册上游的语法为 `--upstream NAME=SERVER_ID@ADDRESS`，身份与 IPC handshake 一致。上游 budget 参数、mesh 和 dry-run 用法见 [c3 指南](../cli/README.md)。

## 本地端点位置与平台差异

| 平台 | 端点 | 位置配置 |
| --- | --- | --- |
| Unix | 私有用户目录内的 UDS | 代码 `cc.set_local_endpoint(root=...)`、环境 `C2_IPC_ROOT`、c3 `--ipc-root`，默认 `/tmp` |
| Windows | 当前登录会话 SID 下的 Named Pipe | 自动选择；Unix root 配置不适用 |

Unix root 必须是已有的绝对容器路径。应用负责创建容器，C-Two 创建其内的用户私有目录和版本目录。共享本地域的资源进程、客户端和 relay 使用相同 root；root 不改变 SHM 或 file-spill 的位置。配置在首次本地 I/O 尝试时冻结，包括失败的尝试。

端点凭据保留其原生 scope。控制器在确认子进程退出后使用精确凭据清理；完整规则与 SDK/CLI 示例见[生命周期指南](local-endpoint-lifecycle.md)。

## 调用等待与准入

| 配置 | 入口 | 默认值 |
| --- | --- | --- |
| 单次调用等待 | `cc.with_call_options(proxy, timeout=...)` | 继承传输默认 |
| Relay 逻辑调用期限 | `C2_RELAY_CALL_TIMEOUT` | 300 秒，`0` 为无限 |
| 有限调用名额 | `cc.set_call_execution_limits(max_outstanding_calls=...)` / `C2_CALL_MAX_OUTSTANDING` | 1,024 / Runtime |
| 有限调用保留输入 | `retained_input_budget_bytes=...` / `C2_CALL_RETAINED_INPUT_BUDGET_BYTES` | 16 GiB / Runtime |

直连 IPC 默认无限等待；HTTP 的预算覆盖一个逻辑调用和其内的路由重取。`timeout=None` 显式无限，`0` 在派发前到期，负数和非有限值在入口拒绝。同进程同步调用只接受继承或无限策略。

期限结束调用方等待，已派发的原生事务仍持有输入直到真正完成。有限调用的数量上限 `0` 拒绝所有新有限调用，字节预算 `0` 只拒绝正数输入；无限调用不受这两项准入限额约束。`cc.call_execution_snapshot()` 查看当前及退休域的真实计费。示例与错误阶段见[调用期限指南](releases/0.7.1.md#per-call-deadlines-for-remote-calls)。

## 并发与资源生命周期

`@cc.read`、`@cc.write` 声明方法访问方式，未标注方法采用写访问。注册时的 `cc.ConcurrencyConfig` 设置路由 mode、`max_pending` 和 `max_workers`；Native route handle 同时约束同进程直调和远程派发。配置样例见 [Python SDK 指南](python-usage.md)。

`cc.LifecycleConfig` 选择默认 `Persistent` 或显式 `OwnerBound`。后者使用控制器预先准备的私有 owner capability。`cc.shutdown(timeout=...)` 的期限约束本次关闭观察；监听关闭、工作排空和 held lease 释放分别报告。完整接入方式见[生命周期指南](local-endpoint-lifecycle.md)。
