# IPC v2 设计文档

## 现状分析

当前 `memory://` 协议采用基于文件系统的消息传递：

- **控制面**: 依赖 `watchdog` 文件系统监听 + 轮询（1ms→100ms 指数退避）
- **数据面**: 通过 `mmap` 读写临时文件，原子 `rename` 保障一致性
- **开销**: 每次请求约 14 次系统调用（创建/写入/重命名/读取/删除 × 请求+响应）

### 性能基线 (macOS M 系列)

| 载荷 | memory:// P50 | thread:// P50 | 差距 |
|------|--------------|--------------|------|
| 64B  | 30.97ms      | 0.024ms      | 1264× |
| 1MB  | 34.55ms      | 0.214ms      | 161× |
| 100MB| 206.69ms     | 13.82ms      | 15× |
| 1GB  | 2641ms       | 575ms        | 4.6× |

## IPC v2 设计目标

| 指标 | v1 (memory://) | v2 目标 |
|------|----------------|---------|
| 64B 延迟 | ~31ms | <1ms |
| 1GB 吞吐 | ~774 MB/s | >1.5 GB/s |
| Windows | 未验证 | 第一阶段 |
| 平台兼容 | macOS/Linux | macOS/Linux/Windows |

## 双面架构

```
Router                          Worker
  │                               │
  ├── 控制面 (Control Plane) ──────┤  低延迟信令
  │   UDS / TCP loopback          │  请求/响应元数据
  │                               │
  ├── 数据面 (Data Plane) ─────────┤  高吞吐载荷传输
  │   SharedMemory                │  大数据零拷贝
  │                               │
```

### 控制面

**职责**: 传输请求/响应信令（request_id、method_name、状态码、数据面引用）

| 平台 | 实现 | 备注 |
|------|------|------|
| Linux/macOS | Unix Domain Socket | 最低延迟，内核态拷贝 |
| Windows | TCP `127.0.0.1` loopback | Named Pipes 也可行但 API 差异大 |

**协议格式**:
```
[4B total_length][4B request_id_length][request_id][4B flags][payload_or_shm_ref]

flags:
  bit 0: inline (0) vs shared_memory (1)
  bit 1: request (0) vs response (1)
  bit 2-7: reserved
```

当 `flags.inline == 1` 时，`payload_or_shm_ref` 是 SharedMemory 名称。
当 `flags.inline == 0` 时，`payload_or_shm_ref` 是实际数据。

### 数据面

**职责**: 传输大载荷，实现零拷贝或单次拷贝

**实现**: `multiprocessing.shared_memory.SharedMemory`

- **跨平台**: Python 3.8+ 标准库，Windows/Linux/macOS 均支持
- **生命周期**: 请求方创建 → 接收方读取 → 请求方释放
- **命名**: `cc_shm_{region_id}_{request_id}_{direction}`

### 自适应切换策略

| 载荷大小 | 传输方式 | 原因 |
|----------|----------|------|
| ≤ 1 MB | 内联 socket | SharedMemory 创建/销毁开销 > 传输开销 |
| 1-8 MB | 自适应 | 根据运行时统计动态选择 |
| ≥ 8 MB | SharedMemory | 避免多次内核拷贝 |

阈值可通过 `IPCConfig` 配置。

## Router → Worker 集成

```python
@dataclass
class IPCConfig:
    control_address: str = ''        # 自动分配 UDS/loopback
    inline_threshold: int = 1_048_576  # 1 MB
    shm_threshold: int = 8_388_608     # 8 MB
    shm_pool_size: int = 4            # 预分配 SharedMemory 块数
```

Router 通过 `attach()` 或 `register()` 获取 Worker 的 IPC 地址。
Worker 启动时创建 IPC 服务端并将地址注册到 Router。

### 连接建立流程

```
1. Worker 启动 → 创建 UDS/loopback 监听 → 注册到 Router
2. Router 收到外部 HTTP 请求 → 查路由表 → 获取 Worker IPC 地址
3. Router 通过 IPC 控制面发送请求
4. 若大载荷：Router 创建 SharedMemory → 写入数据 → 控制面发送 shm 引用
5. Worker 从控制面读取请求 → 若 shm 引用则从 SharedMemory 读取
6. Worker 处理完成 → 通过控制面发送响应（同样适用 shm 策略）
7. Router 读取响应 → 返回 HTTP
```

## Windows 兼容方案

| 组件 | Linux/macOS | Windows |
|------|-------------|---------|
| 控制面 | UDS (`/tmp/cc_ipc_{id}.sock`) | TCP loopback (`127.0.0.1:{port}`) |
| 数据面 | SharedMemory | SharedMemory (Windows 原生支持) |
| 路径 | `/tmp/...` | `%TEMP%\...` |

Python 的 `multiprocessing.shared_memory` 在 Windows 上基于
`CreateFileMapping` / `MapViewOfFile`，无需额外处理。

UDS 在 Windows 10 1803+ 也可用 (`AF_UNIX`)，但稳定性不如 TCP loopback。
第一阶段优先使用 TCP loopback 保证兼容性。

## memory:// v1 回退策略

v1 保留为：
- **测试环境**: 无需额外进程管理，文件系统天然隔离
- **兼容回退**: 当 UDS/loopback 不可用时降级
- **文档/教学**: 文件可见性便于调试

协议选择优先级:
```
ipc-v2:// > memory:// > thread://（仅同进程）
```

## 实现分阶段

### Phase 1: 控制面 socket 化
- 用 `asyncio` stream server (UDS/TCP) 替换文件轮询
- 小载荷直接内联传输
- 预期收益: 64B 延迟从 31ms 降至 <1ms

### Phase 2: SharedMemory 数据面
- 大载荷走 SharedMemory
- 实现 SharedMemory pool 复用
- 预期收益: 1GB 吞吐从 774 MB/s 提升至 >1.5 GB/s

### Phase 3: 自适应与监控
- 运行时统计驱动阈值调整
- 健康检查 / 连接复用 / 背压传播
- Router 级流量可观测性

## 接口草案

```python
# 新协议地址格式
'ipc-v2://worker_region_id'           # 自动选择最优控制面
'ipc-v2+uds:///tmp/cc_worker.sock'    # 强制 UDS
'ipc-v2+tcp://127.0.0.1:9100'         # 强制 TCP loopback

# IPCv2 Server (Worker 端)
class IPCv2Server(BaseServer):
    def __init__(self, bind_address: str, event_queue: EventQueue | None = None):
        ...
    def start(self): ...
    def reply(self, event: Event): ...
    def shutdown(self): ...
    def destroy(self): ...

# IPCv2 Client (Router 端)
class IPCv2Client(BaseClient):
    def __init__(self, server_address: str): ...
    def call(self, method_name: str, data: bytes | None = None) -> bytes: ...
    def relay(self, event_bytes: bytes) -> bytes: ...
```

集成到现有工厂模式：
```python
# client.py
if server_address.startswith('ipc-v2://'):
    return IPCv2Client

# server.py
if config.bind_address.startswith('ipc-v2://'):
    self.server = IPCv2Server(config.bind_address)
```
