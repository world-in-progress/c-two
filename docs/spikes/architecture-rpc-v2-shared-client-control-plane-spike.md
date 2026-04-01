---
title: "RPC v2 架构：共享 Client + 控制面路由 + 模块重构"
category: "Architecture"
status: "✅ Complete (superseded by Rust transport sink)"
priority: "Critical"
timebox: "2 weeks"
created: 2026-03-26
updated: 2026-03-26
owner: "soku"
tags: ["technical-spike", "architecture", "rpc-v2", "shared-client", "control-plane", "ipc-v3"]
---

# RPC v2 架构：共享 Client + 控制面路由 + 模块重构

## Summary

**Spike Objective:** 设计全新的 `rpc_v2` 模块架构，核心变革为：(1) Client 面向 Server 而非特定 CRM 路由，可被多个 ICRM 消费端复用；(2) 路由信息（namespace）、方法名、错误元数据全部从数据面提升到控制面；(3) 作为独立模块 `src/c_two/rpc_v2/` 实现，以 IPC v3 为核心传输层，不再追求多协议通用的一致性抽象。

**Why This Matters:**
- 当前每个 ICRM 消费端独占一个 IPCv3Client，每个 Client 分配 256MB buddy SHM — 4 个 ICRM 消耗 1GB 内存，完全不可接受
- 当前 Client 是串行的（`_conn_lock` 锁全程持有），无法并发复用
- 当前 wire 格式将 method_name/error 与 payload 拼接在同一 buffer，大 payload 时不必要的 memcpy 开销
- 当前 `rpc/` 模块为适配 thread/memory/ipc/tcp/http 五种协议而做了大量抽象，修改极度困难
- SOTA 设计（`doc/log/sota.md`）的核心机制（隐式托管、register/connect、线程优惠）全部依赖此架构重构

**Timebox:** 2 weeks

**Decision Deadline:** 在 `cc.register()` / `cc.connect()` API 实现前必须确定，否则阻塞 SOTA 落地。

**前序 Spike 依赖:** `architecture-multi-crm-ipc-v3-server-spike.md`（方案 D 控制面路由决策）

## Research Question(s)

**Primary Question:** 如何设计一个以 IPC v3 为核心的、支持共享 Client 和控制面路由的 RPC v2 模块，使得多个 ICRM 消费端能复用同一个 Client 连接到同一 Server，同时通过引用计数管理 Client 生命周期？

**Secondary Questions:**

- Client 如何从当前的串行请求-响应模型升级为支持并发多路复用？
- 控制面（UDS inline frame）和数据面（SHM buddy block）如何分离？wire 格式如何重新设计？
- 引用计数 + 延迟销毁的 Client 池如何实现？
- `rpc_v2/` 模块如何复用 `rpc/` 中已高度优化的组件（buddy allocator、transferable、wire codec 的部分逻辑）？
- 线程优惠（thread-local CRM 直接返回）和 HTTP 透传在 rpc_v2 中如何简化实现？

## Investigation Plan

### Research Tasks

- [x] 分析当前 IPCv3Client 的串行限制（`_conn_lock` + `_recv_response`）
- [x] 分析当前 wire 格式的大 payload 拼接开销（`encode_call`/`encode_reply` + `write_call_into`/`write_reply_into`）
- [x] 分析 `rpc/` 模块中可复用 vs 需重写的组件
- [x] 分析 `compo/runtime_connect.py` 对 `Client` 的依赖面（仅 `call`/`relay`/`terminate`）
- [ ] 设计控制面帧格式（routing + metadata）与数据面帧格式（pure payload）的分离方案
- [ ] 设计并发多路复用的 Client 协议
- [ ] 设计引用计数 + 延迟销毁的 ClientPool
- [ ] 设计 `rpc_v2/` 模块目录结构
- [ ] PoC 验证并发 Client + 共享 buddy pool 的可行性

### Success Criteria

**This spike is complete when:**

- [ ] 确定控制面/数据面分离的帧格式二进制定义
- [ ] 确定并发多路复用 Client 的协议设计
- [ ] 确定 ClientPool 引用计数 + 延迟销毁策略
- [ ] 确定 `rpc_v2/` 模块结构和复用边界
- [ ] 给出清晰的实施路径和分阶段计划

## Technical Context

| 组件 | 当前状态 | 问题 | 目标 |
|------|---------|------|------|
| `IPCv3Client` | 1 Client = 1 UDS = 1 buddy pool (256MB) | 4 ICRM = 1GB | 1 Client 被 N 个 ICRM 共享 |
| `IPCv3Client.call()` | `_conn_lock` 锁全程持有，串行 | 无法并发 | 支持并发多路复用 |
| wire 格式 (`wire.py`) | `[type][method_len][method][payload]` 拼接 | 100MB payload 需 memcpy 拼接 | method_name 提升到控制帧 |
| `rpc/server.py` | 适配 5 种协议的统一 Server | 修改极难 | 以 IPC v3 为核心重新设计 |
| `rpc/client.py` | 地址前缀分派到 5 种 Client | 多协议负担 | 面向 IPC v3 + 线程优惠两种模式 |
| `compo/runtime_connect.py` | `Client(address)` 无池化 | 每次创建新 Client | `ClientPool.acquire(address)` |

**Related Components:** `ipc_v3_server.py`, `ipc_v3_client.py`, `ipc_v3_protocol.py`, `wire.py`, `server.py`, `client.py`, `runtime_connect.py`, `meta.py`

**Dependencies:** 本 spike 的结论直接影响 `cc.register()` / `cc.connect()` API 的实现方式

**Constraints:**
- 必须兼容现有 buddy allocator Rust 模块 (`c2_buddy`)
- 必须保持零拷贝 SHM 读取的性能优势
- 线程安全性要求：多个 ICRM 消费端从不同线程并发调用共享 Client
- Python ≥ 3.10，free-threading (3.14t) 为目标平台

## Research Findings

### 1. 当前架构的三大根本性问题

#### 1.1 Client 的串行瓶颈与资源浪费

**现状（`ipc_v3_client.py:207-293`）：**

```python
def call(self, method_name: str, data: bytes | None = None) -> bytes | memoryview:
    with self._conn_lock:           # ← 锁住整个请求-响应周期
        # ... 发送请求 ...
        sock.sendall(frame)
        # ... 阻塞等待响应 ...
        response_data, free_info = self._recv_response()  # ← 串行阻塞
```

**问题链：**
1. `_conn_lock` 在整个 `call()` 期间持有 → 任何并发调用必须排队
2. `_recv_response()` 是同步阻塞的 → 一个请求未返回前，连接完全被占用
3. 因此每个 ICRM 消费端必须有自己的 Client 实例
4. 每个 Client 实例创建一个独立的 buddy pool（默认 256MB SHM）
5. **4 个 ICRM 消费端 = 4 × 256MB = 1GB 纯 SHM 开销**

**代码验证——无任何池化机制（`runtime_connect.py:24-55`）：**

```python
def connect_crm(address: str, icrm_class: Type[ICRM] = None, **kwargs):
    client = Client(address, **kwargs)    # 每次创建新 Client，无池化
    _local.current_client = client
    try:
        icrm = icrm_class()
        icrm.client = client              # 1 ICRM ↔ 1 Client 强绑定
        yield icrm
    finally:
        client.terminate()                # 连接、SHM 全部销毁
```

#### 1.2 Wire 格式的数据面拼接开销

**CRM_CALL 当前格式：**
```
[1B type=0x03][2B method_name_len LE][method_name UTF-8][payload ...]
```

**问题（`wire.py:139-162` encode_call）：**

```python
def encode_call(method_name: str, payload: ...) -> bytearray:
    cached_header = _call_header_cache.get(method_name)
    buf = bytearray(header_len + payload_len)   # 一次分配 header + payload 的完整 buffer
    buf[:header_len] = cached_header             # 拷贝 header
    buf[header_len:] = payload                   # 拷贝 payload ← 100MB payload = 100MB memcpy
    return buf
```

即使通过 `write_call_into()` 直接写入 SHM（`wire.py:186-220`），method_name 仍然必须写在 payload 之前——这迫使 SHM allocation 必须为 `header_size + payload_size`，且 payload 必须被拷贝到 header 之后的偏移处。

**CRM_REPLY 同理（`wire.py:165-179`）：**
```
[1B type=0x04][4B error_len LE][error_bytes][result ...]
```
error 元数据和 result payload 拼接在同一 buffer 中。

**性能影响：**
- 100MB payload: 一次完整 memcpy ≈ 50-100ms（Python memcpy ~1-2 GB/s）
- 如果控制面元数据（method_name, error）与数据面（payload）分离，payload 可直接写入 SHM 的起始位置，无需预留 header 空间

#### 1.3 多协议抽象的历史包袱

**当前 `rpc/` 模块为 5 种协议设计统一抽象：**

| 协议 | 文件 | 代码量 |
|------|------|--------|
| thread:// | `thread/thread_server.py` + `thread_client.py` | ~11K |
| memory:// | `memory/memory_routing.py` + `memory_server/client` | ~10K |
| ipc:// (v2) | `ipc/ipc_server.py` + `ipc_client.py` + `ipc_protocol.py` | ~75K |
| ipc:// (v3) | `ipc/ipc_v3_server.py` + `ipc_v3_client.py` + `ipc_v3_protocol.py` | ~56K |
| tcp:// | `zmq/zmq_server.py` + `zmq_client.py` | ~9K |
| http:// | `http/http_server.py` + `http_client.py` | ~7K |

**`server.py:602-618` 的协议分派：**
```python
class Server:
    def __init__(self, config: ServerConfig):
        if config.bind_address.startswith('thread://'):
            self._server = ThreadServer(...)
        elif config.bind_address.startswith('memory://'):
            self._server = MemoryServer(...)
        elif config.bind_address.startswith(('ipc://', 'ipc-v3://')):
            self._server = IPCv3Server(...)
        elif config.bind_address.startswith('tcp://'):
            self._server = ZmqServer(...)
        elif config.bind_address.startswith('http://'):
            self._server = HttpServer(...)
```

这种 if-elif 链使得任何结构性改动都必须考虑 5 种协议的兼容性。而 SOTA 设计明确指出：**IPC v3 是核心传输层**，thread 只是本地优化，HTTP 只是透传机制。围绕 IPC v3 重新设计，不再需要统一抽象。


### 2. 核心设计决策：Client 面向 Server，而非面向 CRM 路由

#### 2.1 从"前序 Spike 方案 D"到"共享 Client"的演进

前序 Spike（`architecture-multi-crm-ipc-v3-server-spike.md`）的方案 D 做出了关键决策：
- **控制面路由**：namespace 在 handshake 阶段绑定到连接，CRM_CALL wire 格式不变
- **per-connection CRM 亲和**：一个连接绑定到一个 CRM

方案 D 的隐含假设是 **1 Client = 1 Connection = 1 CRM**。本 spike 对此进行关键升级：

```
方案 D（前序 Spike）:
  ICRM_A → Client_A → Connection_A → Server → CRM_A
  ICRM_B → Client_B → Connection_B → Server → CRM_B
  每个 Client 独立 buddy pool (256MB)，总计 512MB

本 Spike 方案:
  ICRM_A ─┐
           ├→ SharedClient → Connection → Server → CRM_A / CRM_B
  ICRM_B ─┘
  共享 1 个 buddy pool (256MB)，总计 256MB
```

**关键洞察：** 如果 Client 支持并发多路复用，那么一个 Client 完全可以同时为多个 ICRM 服务——每个请求在控制帧中携带 namespace，Server 根据 namespace 路由到正确的 CRM。

#### 2.2 HTTP 类比：路由属于控制面

这与 HTTP 协议的设计完全一致：
- **HTTP:** URL path（路由）在 request line，不在 body 中
- **RPC v2:** namespace + method_name 在控制帧，不在 SHM payload 中

```
HTTP:  GET /api/v1/grids HTTP/1.1\r\n   ← 控制面（路由+方法）
       Content-Length: 1048576\r\n       ← 控制面（元数据）
       \r\n
       <1MB binary body>                ← 数据面（纯 payload）

RPC v2: [control frame: ns=grid, method=subdivide, rid=42, size=1048576]  ← UDS inline
        [data: 1MB raw payload in SHM]                                     ← buddy block
```

#### 2.3 路由在控制面带来的额外收益

当 namespace 和 method_name 都在控制帧中时：
1. **SHM payload 是纯数据** — 序列化器可以直接写入 SHM offset 0，无需为 header 预留空间
2. **Server 端可以在读取 SHM 之前就完成路由** — 控制帧在 UDS 上 inline 传输，server 先读到路由信息，再去 SHM 读 payload
3. **错误信息不污染 SHM** — reply 的 error 在控制帧中，SHM 只存 result payload
4. **Wire 解析更简单** — SHM 中不再有混合格式，decode 就是 `buf[0:data_size]`

### 3. 控制面帧格式设计

#### 3.1 帧层架构

```
Transport Layer (UDS):
  ┌──────────────────────────────────────────────────┐
  │  Frame Header (16B, 与 v2/v3 兼容)                │
  │  [4B total_len][4B request_id][4B flags][4B rsrv] │
  ├──────────────────────────────────────────────────┤
  │  Frame Payload (variable)                         │
  │  - inline data, OR                                │
  │  - buddy pointer, OR                              │
  │  - control message (handshake/ctrl)               │
  └──────────────────────────────────────────────────┘
```

RPC v2 在此基础上，为 CRM_CALL 和 CRM_REPLY 引入**控制面扩展**：

#### 3.2 CRM_CALL v2 控制帧格式

```
Frame Header (16B):
  [4B total_len LE][4B request_id LE][4B flags LE][4B reserved]
  flags: FLAG_BUDDY (bit 6) | FLAG_CALL_V2 (bit 7, new)

Call Control Payload (UDS inline, appended to buddy pointer if FLAG_BUDDY):
  [1B ns_len][namespace UTF-8][2B method_idx LE][4B payload_size LE]

  - ns_len: namespace 长度 (0 = 使用连接默认 namespace，即兼容单 CRM 场景)
  - namespace: CRM 命名空间 (仅在 ns_len > 0 时存在)
  - method_idx: 预注册方法索引 (替代 method_name 字符串)
  - payload_size: SHM 中数据面 payload 的字节数
```

**方法索引 vs 方法名：**

当前 wire 格式传输 method_name 字符串（最大 255 字节），每次请求都重复。如果改为在 handshake 阶段交换方法列表（ICRM 的所有 public methods），后续请求只需传 2 字节的方法索引：

```
Handshake 阶段:
  Client → Server: "我要连接的 ICRM 有这些方法: [subdivide_grids, get_schema, ...]"
  Server → Client: "确认。方法映射: subdivide_grids=0, get_schema=1, ..."

后续每次 CRM_CALL:
  控制帧: method_idx=0 (替代 "subdivide_grids" 的 15 字节)
```

这是 gRPC 的 HPACK 头部压缩思想在 RPC 层面的应用。

#### 3.3 CRM_REPLY v2 控制帧格式

```
Frame Header (16B):
  flags: FLAG_RESPONSE | FLAG_BUDDY | FLAG_REPLY_V2 (bit 7, new)

Reply Control Payload (UDS inline):
  [1B status][4B payload_size LE][optional error data]

  status:
    0x00 = 成功，payload_size 指向 SHM 中的 result
    0x01 = 错误，error data 紧跟在 payload_size 之后（inline 传输）
    0x02 = 成功 + 零拷贝复用（server 原地写入 result 到 request block）

  error data (仅 status=0x01 时存在):
    [4B error_len LE][error_bytes]
```

**关键优化：成功路径（status=0x00）时，error 信息为零开销。** 当前格式每次 reply 都需要 4B error_len 即使没有错误。

#### 3.4 SHM 数据面格式

```
SHM buddy block (纯 payload):
  [raw serialized data, offset 0 to payload_size]
  无任何 header/metadata — 纯数据
```

**对比当前格式：**
```
当前:  [1B type][2B method_len][method_name][payload]   ← SHM 中有 header
RPC v2: [payload]                                        ← SHM 中纯数据
```

**收益：**
- serializer 可以直接 `pack_into(shm_buf, 0, ...)` 而非 `pack_into(shm_buf, header_offset, ...)`
- 不再需要 `write_call_into()` / `write_reply_into()` 的 header + payload 拼接逻辑
- buddy alloc 只需分配 `payload_size` 而非 `header_size + payload_size`

### 4. 并发多路复用 Client 设计

#### 4.1 当前串行模型 vs 目标并发模型

```
当前模型 (IPCv3Client):
  Thread A: call(method_A) ─── lock ─── send ─── recv ─── unlock
  Thread B: call(method_B) ─── [blocked] ─────────────── lock ─── send ─── recv ─── unlock

目标模型 (SharedClient):
  Thread A: call(ns_A, method_A) ─── send(rid=1) ─── [等待 rid=1 的 response]
  Thread B: call(ns_B, method_B) ─── send(rid=2) ─── [等待 rid=2 的 response]
  Recv线程:                          ─── recv(rid=2) → 唤醒 B ─── recv(rid=1) → 唤醒 A
```

#### 4.2 并发多路复用协议

**核心机制：request_id 匹配**

当前 IPC v3 协议的 frame header 已经包含 `request_id (4B)`。Server 端也已经用 `asyncio.Future` 按 `request_id` 匹配响应（`ipc_v3_server.py:588-595`）。

问题在 Client 端：当前 Client 发送请求后立即调用 `_recv_response()` 同步阻塞等待——它假设下一个到达的响应就是自己的。

**解决方案：引入 Client 端的 response dispatcher**

```python
class SharedClient:
    """支持并发多路复用的 IPC v3 Client。"""

    def __init__(self, server_address: str, config: IPCConfig = None):
        self._sock: socket.socket          # 唯一 UDS 连接
        self._buddy_pool: BuddyPoolHandle  # 唯一 buddy pool (256MB)
        self._seg_views: list[memoryview]   # 共享 segment views

        self._send_lock: threading.Lock     # 保护发送操作的原子性
        self._pending: dict[int, PendingCall]  # request_id → PendingCall
        self._recv_thread: threading.Thread    # 后台接收线程

        self._refcount: int = 0            # ICRM 引用计数
        self._ref_lock: threading.Lock

    def call(self, namespace: str, method_name: str, data: bytes | None) -> bytes | memoryview:
        """线程安全的并发调用。多个 ICRM 可以从不同线程同时调用。"""
        rid = self._next_rid()
        pending = PendingCall(rid)

        # 1. 注册 pending（无需锁——ConcurrentDict 或 pending_lock）
        self._pending[rid] = pending

        # 2. 编码控制帧 + 写 SHM（仅锁发送）
        with self._send_lock:
            if data and len(data) > self._config.shm_threshold:
                alloc = self._buddy_pool.alloc(len(data))
                seg_mv = self._seg_views[alloc.seg_idx]
                seg_mv[alloc.offset : alloc.offset + len(data)] = data  # 纯 payload 写入
                frame = encode_call_v2_frame(rid, namespace, method_name, len(data),
                                              alloc.seg_idx, alloc.offset)
            else:
                frame = encode_inline_call_v2_frame(rid, namespace, method_name, data)
            self._sock.sendall(frame)

        # 3. 等待 response（不持锁）
        return pending.wait(timeout=self._config.call_timeout)

    def _recv_loop(self):
        """后台线程：持续读取 response，按 request_id 分发到 pending callers。"""
        while self._running:
            header = self._sock.recv(16)
            total_len, request_id, flags = FRAME_STRUCT.unpack(header)
            payload = self._sock.recv(total_len - 12)

            pending = self._pending.pop(request_id, None)
            if pending is None:
                continue  # 已超时或已取消

            if flags & FLAG_BUDDY:
                # 从 SHM 中读取 result（控制帧只有元数据）
                result = self._read_buddy_response(payload)
                pending.set_result(result)
            else:
                pending.set_result(payload)

@dataclass
class PendingCall:
    rid: int
    event: threading.Event = field(default_factory=threading.Event)
    result: bytes | memoryview | None = None
    error: Exception | None = None

    def wait(self, timeout: float) -> bytes | memoryview:
        if not self.event.wait(timeout):
            raise TimeoutError(f'RPC call {self.rid} timed out')
        if self.error:
            raise self.error
        return self.result

    def set_result(self, data):
        self.result = data
        self.event.set()
```

#### 4.3 关键设计考量

**发送锁粒度：** `_send_lock` 只保护 `sendall()` 的原子性（防止两个线程的帧在 UDS 上交错），不保护整个 call 周期。这是并发的关键——发送完成后立即释放锁，其他线程可以立即发送。

**buddy pool 并发：** `c2_buddy` 的 `alloc()` 和 `free_at()` 是否线程安全？
- 当前 `IPCv3Client` 在 `_conn_lock` 内调用 `alloc()`，因此未测试并发安全性
- **需要验证**：如果 `c2_buddy` 不支持并发 alloc，需要在 SharedClient 中加 `_alloc_lock`
- 替代方案：per-thread buddy sub-allocator（从主 pool 批量预分配小块）

**SHM 读写并发安全：**
- 不同线程写入 SHM 的不同区域（不同 alloc offset）→ 天然安全
- recv_thread 读取 SHM 的 response 区域时，调用线程不会写同一区域 → 安全
- 唯一风险：deferred free —— 需要确保 recv_thread 完成数据拷贝后再 free buddy block

**Server 端并发感知：**
- Server 已经支持并发请求处理（`_handle_client` 使用 `asyncio.Future`）
- 当前 Server 用 `conn_id:request_id` 做复合键 → 天然支持同一连接的多个并发请求
- **无需修改 Server 的事件循环**

#### 4.4 与当前 IPCv3Client 的对比

| 维度 | 当前 IPCv3Client | SharedClient |
|------|-----------------|-------------|
| 并发能力 | 串行（全程锁） | 并发（仅发送时锁） |
| 线程模型 | 调用线程直接 recv | 后台 recv_thread 分发 |
| Buddy pool | 每实例独占 256MB | 共享 1 个 256MB |
| 连接数 | N 个 ICRM = N 个 UDS | N 个 ICRM = 1 个 UDS |
| SHM 格式 | `[header][payload]` | `[payload]`（纯数据）|
| 路由 | 无（1:1 绑定）| 控制帧中 namespace |
| 生命周期 | ICRM 直接 terminate | 引用计数 + 延迟销毁 |

### 5. 引用计数 + 延迟销毁的 ClientPool

#### 5.1 设计动机

```python
# 场景：多个 ICRM 消费端连接到同一 Server
crm_a: IGrid = cc.connect('region_1/grid')        # → acquires SharedClient for region_1
crm_b: ISolver = cc.connect('region_1/solver')     # → reuses same SharedClient
crm_c: IPhysics = cc.connect('region_1/physics')   # → reuses same SharedClient

cc.close(crm_a)   # refcount: 3 → 2，Client 不销毁
cc.close(crm_b)   # refcount: 2 → 1，Client 不销毁
cc.close(crm_c)   # refcount: 1 → 0，进入延迟销毁 (grace period)
                   # 30秒后若无新 ICRM 接入 → Client.terminate()
```

#### 5.2 ClientPool 实现

```python
class ClientPool:
    """进程级单例，管理到各 Server 的共享 Client 连接。"""

    _instance: ClassVar['ClientPool'] = None

    def __init__(self):
        self._clients: dict[str, SharedClient] = {}   # server_address → SharedClient
        self._lock = threading.Lock()

    @classmethod
    def get(cls) -> 'ClientPool':
        if cls._instance is None:
            cls._instance = cls()
        return cls._instance

    def acquire(self, server_address: str, namespace: str) -> SharedClient:
        """获取或创建到指定 Server 的共享 Client。引用计数 +1。"""
        with self._lock:
            client = self._clients.get(server_address)
            if client is None:
                client = SharedClient(server_address)
                self._clients[server_address] = client
            elif client._grace_timer is not None:
                # 在延迟销毁期间有新 ICRM 接入 → 取消销毁
                client._grace_timer.cancel()
                client._grace_timer = None
            client._refcount += 1
            return client

    def release(self, server_address: str):
        """引用计数 -1。当降为 0 时，启动延迟销毁。"""
        with self._lock:
            client = self._clients.get(server_address)
            if client is None:
                return
            client._refcount -= 1
            if client._refcount <= 0:
                # 启动 grace period（默认 30 秒）
                timer = threading.Timer(30.0, self._destroy, args=[server_address])
                timer.daemon = True
                timer.start()
                client._grace_timer = timer

    def _destroy(self, server_address: str):
        """Grace period 过期后真正销毁 Client。"""
        with self._lock:
            client = self._clients.get(server_address)
            if client is None or client._refcount > 0:
                return  # 已有新 ICRM 接入
            del self._clients[server_address]
        client.terminate()
```

#### 5.3 地址解析

**从 ICRM namespace 到 Server address + CRM namespace：**

```
cc.connect('region_1/grid')
  → server_address = 'ipc:///tmp/cc_region_1.sock'  (UDS socket path)
  → namespace = 'grid'                                (CRM 路由)

cc.connect('192.168.1.10:8080/grid')
  → server_address = 'http://192.168.1.10:8080'       (HTTP 远程)
  → namespace = 'grid'                                  (CRM 路由)
```

地址中最后一个 `/` 之后的部分是 CRM namespace，之前的部分确定 Server 地址和协议。这与 HTTP URL 的 `host:port/path` 模式一致。

### 6. `rpc_v2/` 模块架构

#### 6.1 设计原则

1. **以 IPC v3 为核心** — Server 和 Client 围绕 buddy allocator + UDS 设计，不考虑 ZMQ/旧 HTTP 的兼容
2. **线程优惠作为快速路径** — 同进程 CRM 直接返回 ICRM-wrapped 代理对象
3. **HTTP 作为透传层** — 仅实现 Client → Rust Router → IPC v3 的转发链
4. **复用已优化组件** — buddy allocator (`c2_buddy`)、transferable、error 系统直接复用
5. **不复用有历史包袱的组件** — `server.py` 的多协议 Server、`client.py` 的协议分派、EventQueue

#### 6.2 目录结构

```
src/c_two/rpc_v2/
├── __init__.py              # 导出: Server, SharedClient, ClientPool
├── server.py                # MultiCRMServer — 单进程多 CRM 托管
│                            #   - 直接包含 asyncio UDS 事件循环
│                            #   - 多 CRM 路由 (namespace → CRMSlot)
│                            #   - per-CRM Scheduler
│                            #   - buddy pool 管理
│
├── client.py                # SharedClient — 并发多路复用 Client
│                            #   - 后台 recv_thread
│                            #   - PendingCall 按 request_id 分发
│                            #   - 引用计数
│
├── pool.py                  # ClientPool — 进程级单例 Client 池
│                            #   - acquire/release + 延迟销毁
│                            #   - 地址 → SharedClient 映射
│
├── wire.py                  # Wire v2 codec — 控制面/数据面分离
│                            #   - encode_call_v2 (控制帧)
│                            #   - encode_reply_v2 (控制帧)
│                            #   - decode_call_v2 / decode_reply_v2
│                            #   - 方法索引注册
│
├── protocol.py              # IPC v3.1 协议扩展
│                            #   - Handshake v5 (namespace + method index exchange)
│                            #   - FLAG_CALL_V2 / FLAG_REPLY_V2
│                            #   - Buddy payload format (unchanged)
│
├── scheduler.py             # Per-CRM Scheduler (从 rpc/server.py 提取)
│                            #   - _Scheduler + ReadWriteLock
│                            #   - ConcurrencyMode (EXCLUSIVE, READ_PARALLEL)
│
├── proxy.py                 # ICRMProxy — 统一代理对象
│                            #   - 线程内: 直接委托 CRM (+ ReadWriteLock)
│                            #   - 进程间: SharedClient.call(namespace, method)
│                            #   - 跨节点: HTTP Client → Router → IPC
│
└── thread_local.py          # 线程优惠快速路径
                             #   - 检测同进程 CRM → 返回 LocalProxy
                             #   - LocalProxy 直接调用 CRM 方法 + 共享 Scheduler
```

#### 6.3 从 `rpc/` 复用的组件

| 组件 | 来源 | 复用方式 |
|------|------|---------|
| `transferable.py` | `rpc/transferable.py` | 直接 import（协议无关）|
| `c2_buddy` | `rust/c2_buddy/` | 直接 import（Rust FFI，协议无关）|
| `Envelope` | `rpc/event/envelope.py` | 可能修改或替换为更轻量结构 |
| `CCError` 系列 | `error/` | 直接 import（协议无关）|
| `ICRM meta` | `crm/meta.py` | 直接 import（协议无关）|
| `IPCConfig` | `rpc/ipc/ipc_protocol.py` | 可能重新定义或继承 |
| `_Scheduler` | `rpc/server.py:137-366` | 提取到 `scheduler.py` |
| `ipc_v3_protocol.py` | `rpc/ipc/ipc_v3_protocol.py` | 扩展 handshake v5 |

**明确不复用：**
- `rpc/server.py` 的 `Server` / `ServerConfig` / `ServerState`（多协议耦合）
- `rpc/client.py` 的 `Client`（协议分派逻辑）
- `rpc/event/event_queue.py`（可能被替换为更轻量机制）
- `rpc/ipc/ipc_v3_server.py`（需要大幅重构来支持多 CRM）
- `rpc/ipc/ipc_v3_client.py`（串行模型，需要重写为并发模型）

### 7. Server 端多 CRM 架构（基于前序 Spike 方案 D 演进）

#### 7.1 MultiCRMServer 设计

前序 Spike 方案 D 的 per-connection CRM 亲和在共享 Client 模型下需要调整：

```
方案 D (前序): 一个连接绑定一个 CRM
  → handshake 时绑定 namespace
  → 后续所有请求自动路由到绑定的 CRM

共享 Client 模型: 一个连接服务多个 CRM
  → handshake 时注册方法列表 (不绑定特定 CRM)
  → 每个请求的控制帧中携带 namespace → Server 按请求路由
```

**MultiCRMServer 核心结构：**

```python
@dataclass
class CRMSlot:
    """一个已注册的 CRM 资源。"""
    namespace: str
    crm: object                    # CRM 实例
    icrm: object                   # ICRM 实例 (direction='<-')
    scheduler: Scheduler           # 独立调度器 (per-CRM 并发控制)
    method_map: dict[str, int]     # method_name → method_idx

class MultiCRMServer:
    """单进程多 CRM 托管 Server。"""

    def __init__(self, bind_address: str, config: IPCConfig = None):
        self._slots: dict[str, CRMSlot] = {}         # namespace → CRMSlot
        self._global_method_idx: dict[str, int] = {}  # 全局 method_name → idx
        self._connections: dict[int, ConnectionState] = {}
        self._loop: asyncio.AbstractEventLoop
        self._sock_path: str

    def register(self, namespace: str, icrm_class: type, crm: object):
        """动态注册一个 CRM。Server 运行中也可调用。"""
        icrm = icrm_class()
        icrm.crm = crm
        icrm.direction = '<-'
        scheduler = Scheduler(...)
        slot = CRMSlot(namespace, crm, icrm, scheduler, {})
        # 构建方法映射
        for method_name in get_public_methods(icrm_class):
            idx = len(self._global_method_idx)
            self._global_method_idx[method_name] = idx
            slot.method_map[method_name] = idx
        self._slots[namespace] = slot

    def unregister(self, namespace: str):
        """动态注销一个 CRM。等待该 CRM 的所有 pending 请求完成后移除。"""
        slot = self._slots.pop(namespace, None)
        if slot:
            slot.scheduler.drain_and_shutdown()
```

#### 7.2 Per-request 路由（替代 per-connection 绑定）

```python
async def _handle_client(self, reader, writer):
    # Handshake: 交换方法列表，不绑定 CRM
    await self._handle_v5_handshake(conn, payload, writer)

    while True:
        header = await reader.readexactly(16)
        total_len, request_id, flags = FRAME_STRUCT.unpack(header)
        payload = await reader.readexactly(total_len - 12)

        if flags & FLAG_CALL_V2:
            # 解析控制帧: namespace + method_idx + payload_size
            ns, method_idx, payload_size = decode_call_v2_control(payload)
            slot = self._slots.get(ns)
            if slot is None:
                # 返回错误: namespace not found
                await self._send_error(writer, request_id, 'Namespace not found')
                continue

            # 读取 SHM 数据面 (如果有 buddy block)
            if flags & FLAG_BUDDY:
                wire_data = self._read_buddy_data(payload, conn)
            else:
                wire_data = extract_inline_data(payload)

            # 路由到对应 CRM 的 Scheduler
            method_name = slot.reverse_method_map[method_idx]
            slot.scheduler.submit(
                request_id=f'{conn.conn_id}:{request_id}',
                method=getattr(slot.icrm, method_name),
                args_bytes=wire_data,
                reply=lambda result: self._send_reply(conn, request_id, result),
            )
```

#### 7.3 Handshake v5 扩展

```
Client → Server (Handshake v5):
  [1B version=5]
  [2B seg_count LE][per-segment: [4B size LE][1B name_len][name UTF-8]]   ← buddy pool (同 v4)
  [2B capability_flags LE]                                                  ← 新增
    bit 0: supports_call_v2 (控制面路由)
    bit 1: supports_method_idx (方法索引)

Server → Client (Handshake v5 ACK):
  [1B version=5]
  [2B seg_count LE][per-segment: ...]   ← buddy pool ACK (同 v4)
  [2B capability_flags LE]               ← 新增
  [2B namespace_count LE]                ← 可用的 CRM 列表
  [per-namespace:
    [1B ns_len][namespace UTF-8]
    [2B method_count LE]
    [per-method: [1B name_len][method_name UTF-8][2B method_idx LE]]
  ]
```

**Handshake 完成后，Client 持有完整的方法索引表。** 后续 CRM_CALL 只需 2B method_idx。

**向后兼容：** v4 Client 连接到 MultiCRMServer 时，version=4 握手不包含 capability_flags → Server 按传统单 CRM 模式处理（使用默认 namespace）。

### 8. 性能影响分析

#### 8.1 内存节省（最显著收益）

| 场景 | 当前 | RPC v2 | 节省 |
|------|------|--------|------|
| 4 ICRM → 同一 Server | 4 × 256MB = 1024MB | 1 × 256MB = 256MB | **75%** |
| 8 ICRM → 同一 Server | 8 × 256MB = 2048MB | 1 × 256MB = 256MB | **87.5%** |
| 4 ICRM → 2 Server | 4 × 256MB = 1024MB | 2 × 256MB = 512MB | **50%** |
| N ICRM → 1 Server | N × 256MB | 256MB | **(N-1)/N** |

**注意：** buddy pool 默认 256MB 可以在共享模式下适当调大（如 512MB），因为多个 ICRM 可能同时请求 SHM 空间。但即使翻倍也只有 512MB，远小于 N × 256MB。

#### 8.2 UDS 连接数减少

| 场景 | 当前 | RPC v2 |
|------|------|--------|
| 4 ICRM → 同一 Server | 4 UDS connections | 1 UDS connection |
| Server file descriptors | 4 | 1 |
| asyncio tasks | 4 `_handle_client` | 1 `_handle_client` |

#### 8.3 Hot-path 性能

**控制帧编码开销（每次请求）：**
- namespace 路由: 1B ns_len + N bytes namespace（通常 5-20 字节）
- method_idx: 2B（替代原来的 2B method_len + N bytes method_name）
- payload_size: 4B
- **总计: ~10-30 字节 inline**，与当前基本持平

**SHM 写入开销：**
- 当前: `write_call_into()` = header (3 + method_len) + payload 拷贝
- RPC v2: 仅 payload 拷贝（header 在控制帧中，不在 SHM）
- **节省: 每次调用少拷贝 3 + method_len 字节**（对小 payload 影响微乎其微，对大 payload 几乎无影响）

**并发吞吐量（主要收益）：**
- 当前: 4 个 ICRM 串行使用各自 Client → 4 个 RTT 串行
- RPC v2: 4 个 ICRM 并发使用共享 Client → 4 个 RTT 并行
- **理论提升: N× 吞吐量**（受 Server 处理能力和 SHM 带宽限制）

**recv_thread 额外开销：**
- 新增 1 个后台线程 per SharedClient
- response dispatch: dict lookup + Event.set() ≈ 200ns
- **可忽略**

#### 8.4 零拷贝复用路径的影响

当前 IPC v3 Server 有精巧的零拷贝复用机制（`reply()` 中的 BUDDY_REUSE）：
- 当 CRM 方法返回的 result 恰好是 request payload 的子集时，原地覆写 header，避免 memcpy

在 RPC v2 中，由于 SHM 中只有纯 payload（无 header），零拷贝复用变得更简单：
- Server 检测到 result 是 request memoryview → 直接不拷贝，设置 reply 控制帧的 status=0x02
- 无需"覆写 header"——SHM 中没有 header 可覆写
- **零拷贝路径更清晰、更简洁**

### 9. 线程优惠与 HTTP 透传

#### 9.1 线程优惠（Thread-Local 快速路径）

SOTA 设计中最有价值的优化。在 rpc_v2 中的实现：

```python
class ICRMProxy:
    """统一代理对象 — 线程内和远程语义一致。"""

    def __init__(self, icrm_class: type, namespace: str,
                 client: SharedClient | None = None,
                 local_crm: object | None = None,
                 local_scheduler: Scheduler | None = None):
        self._icrm_class = icrm_class
        self._namespace = namespace
        self._client = client             # 远程路径
        self._local_crm = local_crm       # 线程优惠路径
        self._local_scheduler = local_scheduler

    def __getattr__(self, name):
        if self._local_crm is not None:
            # 线程优惠: 直接调用 CRM 方法，但通过 Scheduler 保证并发控制
            method = getattr(self._local_crm, name)
            return lambda *args, **kwargs: self._local_scheduler.submit_sync(method, *args, **kwargs)
        else:
            # 远程路径: 通过 SharedClient 发送 RPC
            return lambda *args, **kwargs: self._remote_call(name, *args, **kwargs)
```

```python
def connect(namespace: str, icrm_class: type = None) -> ICRMProxy:
    """SOTA cc.connect() 入口。自动选择最优路径。"""

    # 1. 检查是否有同进程的 CRM 注册
    local_slot = _process_registry.get(namespace)
    if local_slot is not None:
        # 线程优惠: 直接返回 CRM 代理
        return ICRMProxy(
            icrm_class=icrm_class or local_slot.icrm_class,
            namespace=namespace,
            local_crm=local_slot.crm,
            local_scheduler=local_slot.scheduler,
        )

    # 2. 进程间 IPC: 获取共享 Client
    server_address = resolve_server_address(namespace)
    client = ClientPool.get().acquire(server_address, namespace)
    return ICRMProxy(
        icrm_class=icrm_class,
        namespace=namespace,
        client=client,
    )
```

#### 9.2 HTTP 透传

SOTA 设计：`Browser (TS) → HTTP → Rust Router → IPC v3 → CRM Server`

在 rpc_v2 中，HTTP Client 只需要：
1. 将 method_name + serialized payload 打包成 HTTP body
2. 将 namespace 放在 URL path 中
3. 发送 HTTP request → Rust Router 按 namespace 路由 → IPC v3 → CRM

这完全不需要在 Python 中实现 HTTP Server —— Rust Router（SOTA 附录中的方案）承担此角色。
Python 侧只需要一个简单的 HTTP Client adapter：

```python
class HttpClientAdapter:
    """将 SharedClient 接口适配为 HTTP 调用。用于跨节点访问。"""

    def call(self, namespace: str, method_name: str, data: bytes | None) -> bytes:
        url = f'http://{self._host}:{self._port}/{namespace}/{method_name}'
        response = requests.post(url, data=data, headers={'Content-Type': 'application/octet-stream'})
        if response.status_code != 200:
            raise CCError(response.text)
        return response.content
```

### 10. 对当前代码的变更影响评估

#### 10.1 完全不改的文件

| 文件 | 原因 |
|------|------|
| `rpc/` 整个模块 | rpc_v2 是独立模块，不修改 rpc |
| `crm/meta.py` | ICRM 元数据，协议无关 |
| `rpc/transferable.py` | 序列化逻辑，协议无关 |
| `error/` | 错误类型，协议无关 |
| `rust/c2_buddy/` | Buddy allocator，已高度优化 |

#### 10.2 需要新建的文件

| 文件 | 内容 |
|------|------|
| `src/c_two/rpc_v2/__init__.py` | 模块导出 |
| `src/c_two/rpc_v2/server.py` | MultiCRMServer |
| `src/c_two/rpc_v2/client.py` | SharedClient |
| `src/c_two/rpc_v2/pool.py` | ClientPool |
| `src/c_two/rpc_v2/wire.py` | Wire v2 codec |
| `src/c_two/rpc_v2/protocol.py` | IPC v3.1 协议 |
| `src/c_two/rpc_v2/scheduler.py` | Per-CRM Scheduler |
| `src/c_two/rpc_v2/proxy.py` | ICRMProxy |
| `src/c_two/rpc_v2/thread_local.py` | 线程优惠 |

#### 10.3 需要少量修改的文件

| 文件 | 修改 |
|------|------|
| `src/c_two/__init__.py` | 添加 `from . import rpc_v2` |
| `src/c_two/compo/runtime_connect.py` | 可选：添加 `cc.connect()` 入口 |

### Prototype/Testing Notes

**PoC 验证优先级：**

1. **P0 — 并发 Client 可行性：** 编写测试：多个线程共享一个 IPCv3-like Client，通过 request_id 匹配 response。验证 UDS 上的帧不会交错、buddy pool 并发 alloc 是否安全。

2. **P1 — 控制面/数据面分离：** 在现有 IPC v3 帧格式上扩展一个 flag bit，验证 Server 可以先读控制帧再读 SHM 数据。

3. **P2 — ClientPool 引用计数：** 单元测试 acquire/release/grace-period 语义。

### External Resources

- 前序 Spike: `doc/spikes/architecture-multi-crm-ipc-v3-server-spike.md` — 方案 D 控制面路由决策
- SOTA 设计: `doc/log/sota.md` — §4.1 单端口多路复用, §1.2 register/connect API
- HTTP/2 multiplexing: https://httpwg.org/specs/rfc7540.html#StreamsLayer — 帧多路复用参考
- gRPC HPACK: https://grpc.io/docs/what-is-grpc/core-concepts/ — 方法索引压缩参考

## Decision

### Recommendation

采用 **"RPC v2 独立模块 + 共享 Client + 控制面路由"** 方案。这是对前序 Spike 方案 D 的进一步演进：

| 维度 | 前序 Spike 方案 D | 本 Spike 方案 |
|------|-------------------|---------------|
| Client 模型 | 1 Client = 1 CRM | 1 Client = 1 Server（多 CRM 共享）|
| 路由绑定 | Handshake 绑定（per-connection）| 每请求控制帧（per-request）|
| 并发能力 | 串行 | 并发多路复用 |
| 内存开销 | N × 256MB | 1 × 256MB |
| Wire 改动 | 不改 wire.py | 新 wire v2（控制面/数据面分离）|
| 代码位置 | 修改 rpc/ | 新建 rpc_v2/ |
| method_name | 字符串传输 | 方法索引（2B）|

### Rationale

1. **内存是最急迫的痛点** — 4 ICRM = 1GB 是不可接受的。共享 Client 直接将 N × 256MB 降为 256MB。

2. **并发是 SOTA 的基础** — `cc.connect()` 返回的多个 ICRM proxy 必然从不同线程/协程并发调用。如果底层 Client 是串行的，上层怎么优化都是空话。

3. **控制面路由是 HTTP 验证过的正确模式** — URL path 做路由、header 做元数据、body 做 payload 的三层分离已经被全球最大规模的分布式系统（HTTP）验证了 30 年。没理由在 RPC 层面重新发明轮子把路由信息塞进 payload。

4. **独立 rpc_v2 模块是最低风险路径** — 不改 rpc/，不破坏现有测试和用户，渐进迁移。`compo/runtime_connect.py` 只依赖 `Client` 接口，rpc_v2 提供兼容 API 即可。

5. **IPC v3 为核心消除了多协议税** — 当前 rpc/ 中 50%+ 代码是为适配 5 种协议而写的抽象层。以 IPC v3 为核心重新设计，代码量可能反而更少。

### Implementation Notes

#### Phase 1: 并发 SharedClient + ClientPool（最高优先级）

**目标：** 证明并发多路复用 Client 可行，解决内存问题。

**工作内容：**
- 实现 `SharedClient`：后台 recv_thread + PendingCall 分发 + send_lock
- 实现 `ClientPool`：acquire/release + 引用计数 + grace period
- 验证 `c2_buddy` 并发 alloc 安全性
- **暂不改 wire 格式** — 先用现有 wire.py 的 `encode_call` / `decode`，只是在 Client 层做并发
- **暂不做控制面分离** — 先解决内存 + 并发问题

**预期交付：** SharedClient 可被多个 ICRM 共享，内存从 N × 256MB 降为 1 × 256MB

#### Phase 2: 控制面/数据面分离 + Wire v2

**目标：** method_name/error 提升到控制帧，SHM 中只存纯 payload。

**工作内容：**
- 设计 Wire v2 帧格式（FLAG_CALL_V2 / FLAG_REPLY_V2）
- Handshake v5 扩展（namespace 列表 + 方法索引交换）
- Server 端 per-request 路由（替代 per-connection 绑定）
- 方法索引替代方法名字符串

**预期交付：** 完整的控制面路由 + 方法索引 + 纯 payload SHM

#### Phase 3: MultiCRMServer + SOTA API

**目标：** `cc.register()` / `cc.connect()` / 线程优惠

**工作内容：**
- MultiCRMServer：动态 register/unregister CRM
- `cc.connect()` 入口：自动选择线程优惠 / IPC / HTTP 路径
- ICRMProxy：统一代理对象
- 进程级注册表

**预期交付：** 完整的 SOTA API surface

### Follow-up Actions

- [ ] Phase 1 实现：SharedClient + ClientPool（可单独成 PR）
- [ ] 验证 c2_buddy 并发安全性（可能需要 Rust 端加 Mutex）
- [ ] Phase 2 实现：Wire v2 + Handshake v5
- [ ] Phase 3 实现：MultiCRMServer + SOTA API
- [ ] 更新 `doc/log/sota.md` 中的设计描述
- [ ] 编写迁移指南：rpc → rpc_v2

## Status History

| Date | Status | Notes |
|------|--------|-------|
| 2026-03-26 | 🔴 Not Started | Spike created and scoped |
| 2026-03-26 | 🟡 In Progress | 架构分析完成，三大问题识别，方案设计完成 |

---

_Last updated: 2026-03-26 by soku_
