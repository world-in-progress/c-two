---
title: "IPC v3 单Server多CRM资源托管方案"
category: "Architecture"
status: "🟡 In Progress"
priority: "High"
timebox: "2 weeks"
created: 2026-03-26
updated: 2026-03-26
owner: "soku"
tags: ["technical-spike", "architecture", "ipc-v3", "multi-crm", "resource-hosting"]
---

# IPC v3 单Server多CRM资源托管方案

## Summary

**Spike Objective:** 探究如何在单个 IPC v3 Server 进程中托管多个 CRM 资源对象，通过 `address/namespace` 地址形式实现请求路由分流，从而减少多CRM场景下所需的进程数量。

**Why This Matters:** 当前架构中 `ServerConfig` 将单个 CRM 与单个 `bind_address` 强绑定（1:1），意味着 N 个 CRM 资源需要 N 个独立 Server 进程。在耦合水动力模拟等场景中，一个计算节点可能需要托管网格资源、物理参数库、数据IO服务等数十个 CRM，每个都启动独立进程造成极大的资源浪费和通信复杂度。本 spike 是 `doc/log/sota.md` 中"单端口多路复用"（§4.1）设计建议的首步落地探究。

**Timebox:** 2 weeks

**Decision Deadline:** 在 SOTA 架构重构的第一阶段开发前必须决定，以避免阻塞 `cc.register()` / `cc.connect()` API 的设计。

## Research Question(s)

**Primary Question:** 如何扩展 IPC v3 协议和 Server 架构，使单个 UDS endpoint 能够路由请求到多个不同 namespace 的 CRM 实例，同时保持零拷贝性能优势？

**Secondary Questions:**

- wire 协议（`CRM_CALL`）中如何最小化开销地嵌入 namespace 路由信息？
- 多个 CRM 实例如何共享同一个 buddy pool，还是各自独立的 pool？
- 每个 CRM 的 `_Scheduler` 是独立的（per-CRM 并发控制）还是共享的？
- Client 端的地址格式如何设计以支持 `ipc://region_id/namespace` 路由？
- 动态注册/注销 CRM 时如何保证已有连接的稳定性？

## Investigation Plan

### Research Tasks

- [ ] 分析当前 wire 协议 (`wire.py`) 中 `CRM_CALL` 格式，设计 namespace 字段的嵌入方案
- [ ] 分析 `Server` 类的初始化路径 (`server.py:602-645`)，设计多 CRM 配置结构
- [ ] 分析 IPC v3 Server 的请求分发路径 (`ipc_v3_server.py:559`)，设计 namespace 路由层
- [ ] 分析 `_Scheduler` 的并发控制模型，确定 per-CRM vs shared 调度策略
- [ ] 分析 `_dispatch_crm_call` (`server.py:457-485`)，重构为 namespace-aware 分发
- [ ] 评估 buddy pool 共享 vs 隔离的性能和安全性影响
- [ ] 设计 Client 端 namespace-aware 地址解析方案
- [ ] 创建 PoC 验证多 CRM 路由的可行性
- [ ] 编写性能基准测试，对比单 CRM vs 多 CRM server 的开销

### Success Criteria

**This spike is complete when:**

- [ ] 确定 wire 协议扩展方案，并给出二进制格式定义
- [ ] 确定 Server 端多 CRM 路由架构
- [ ] 确定 per-CRM 调度策略
- [ ] 确定 Client 端地址格式和解析方案
- [ ] 完成 PoC 代码验证关键路径可行
- [ ] 给出清晰的实施建议和后续 action items

## Technical Context

**Related Components:**

| 组件 | 文件 | 当前职责 | 需变更 |
|------|------|---------|--------|
| `ServerConfig` | `src/c_two/rpc/server.py:51-87` | 1:1 绑定 CRM ↔ bind_address | 新增 `MultiServerConfig`（不改 ServerConfig） |
| `Server` | `src/c_two/rpc/server.py:596-671` | 按协议前缀创建单一 BaseServer | 新增 `MultiServer` 类（不改 Server） |
| `_dispatch_crm_call` | `src/c_two/rpc/server.py:457-485` | `getattr(icrm, method_name)` 单 ICRM 分发 | 新增 `_dispatch_crm_call_multi` 基于 conn_id 路由 |
| `_Scheduler` | `src/c_two/rpc/server.py:137-366` | 单 CRM 的 ThreadPoolExecutor + RW Lock | 不变——per-CRM 独立实例 |
| `ServerState` | `src/c_two/rpc/server.py:368-401` | 持有单个 `crm`/`icrm`/`scheduler` | 新增 `MultiServerState`（conn_bindings 映射） |
| `wire.py` (CRM_CALL) | `src/c_two/rpc/util/wire.py:6-8` | `[1B type][2B method_len][method][payload]` | **不变** ✅ |
| `Envelope` | `src/c_two/rpc/event/envelope.py` | `method_name` + `payload`，无 namespace | **不变** ✅ |
| `IPCv3Server` | `src/c_two/rpc/ipc/ipc_v3_server.py:85-638` | 接受连接、解析帧、入队 EventQueue | Handshake v5 分支 + `BuddyConnection.crm_slot` |
| `IPCv3Client` | `src/c_two/rpc/ipc/ipc_v3_client.py:74-100` | `region_id` = address 去除协议前缀 | 解析 `ipc://region_id/namespace` + handshake 发 ns |
| `meta.py` (ICRM) | `src/c_two/crm/meta.py:49-109` | `__tag__ = namespace/class/version` | **不变** ✅ |
| `runtime_connect.py` | `src/c_two/compo/runtime_connect.py:24-55` | `Client(address)` → ICRM 绑定单一 client | **不变**（地址格式自然兼容） |

**Dependencies:**

- 本 spike 的结论将直接影响 `cc.register()` / `cc.connect()` API 的设计（sota.md §模式设计）
- Rust Routing Server（sota.md §附录）需要与新 wire 格式兼容
- HTTP routing 模式（`router.py`）需要感知 namespace 路由

**Constraints:**

- **零拷贝性能不能退化**：buddy pool 的 SHM memoryview 零拷贝路径必须保持
- **向后兼容**：新 wire 格式需能被检测，老 client 连接新 server 需有明确错误提示
- **Python ≥ 3.10**：可使用 `match` 语句等现代语法
- **free-threading (3.14t) 安全**：per-CRM 调度器的锁策略需兼容无 GIL 构建

## Research Findings

### 1. 当前架构瓶颈分析

#### 1.1 1:1 CRM-Server 绑定链

当前请求处理的完整链路为：

```
Client.call(method, data)
  → IPCv3Client: encode CRM_CALL wire bytes → buddy alloc → UDS send frame
  → IPCv3Server._handle_client(): read frame → _resolve_request() → decode(wire_bytes) → Envelope
  → EventQueue.put(envelope)
  → _serve() → _process_event_and_continue() → _dispatch_crm_call()
  → getattr(state.icrm, method_name)  ← 单一 ICRM 实例
  → _Scheduler.submit() → ThreadPoolExecutor → CRM method
  → reply() → buddy alloc/reuse → UDS write frame
```

**瓶颈在 `_dispatch_crm_call()` (server.py:457-485)**：直接对 `state.icrm`（单实例）做 `getattr`，无任何路由层。所有请求默认送往同一个 CRM。

#### 1.2 资源开销模型

当前 N 个 CRM 需要的资源：

| 资源 | 单 CRM Server | N 个 CRM Server |
|------|-------------|----------------|
| 进程数 | 1 | N |
| UDS 文件 | 1 | N |
| asyncio 事件循环线程 | 1 | N |
| EventQueue 消费线程 | 1 | N |
| ThreadPoolExecutor | 1 (1+ workers) | N (1+ workers each) |
| buddy pool SHM | 1 per connection | N per connection |

在耦合模拟场景中 N=10~50，资源浪费显著。

#### 1.3 wire 协议现状

当前 `CRM_CALL` 格式 (`wire.py:6-8`):
```
[1B msg_type=0x03][2B method_name_len LE][method_name UTF-8][payload ...]
```

**无 namespace 字段**。method_name 是唯一的路由信息，且 method name 仅在单个 ICRM 内有意义。如果两个 CRM 有同名方法（如 `get_info()`），当前架构无法区分。

### 2. 路由层定位分析：控制面 vs 数据面

#### 2.1 关键不变量：一个 ICRM 消费者 = 一个连接 = 一个 CRM 目标

通过分析消费侧代码 (`compo/runtime_connect.py:39-46`)，发现了一个关键的架构不变量：

```python
# runtime_connect.py:39-46 — connect_crm 创建 Client 的完整生命周期
client = Client(address)           # → 内部创建 IPCv3Client → 1个 UDS connection
icrm = icrm_class()               # 消费侧 ICRM 实例
icrm.client = client               # ICRM 绑定到唯一的 client
yield icrm                         # 整个生命周期内只访问一个 CRM
client.terminate()                 # 关闭时释放连接
```

**推论**：
1. 一个 ICRM 消费者实例 → 持有一个 `Client` → 内嵌一个 `IPCv3Client` → 维护一个 UDS 连接
2. 这个连接在整个生命周期内只会访问 **一个特定的 CRM namespace**
3. **Server 和 Client 是不对称的**：Server 可托管 N 个 CRM，但每个 Client 连接只导向固定的一个

#### 2.2 结论：namespace 路由属于控制面，不属于数据面

既然一个连接必然只导向一个 CRM，那么 namespace 信息只需在 **连接建立时** 确定一次（控制面），而非每条消息都携带（数据面）。这从根本上改变了设计方向：

| 维度 | 数据面方案（旧） | 控制面方案（新）✅ |
|------|---------------|----------------|
| **wire 格式** | 需新增 `CRM_CALL_V2 (0x13)` | **CRM_CALL (0x03) 不变** |
| **每条消息开销** | +2B crm_id | **0** |
| **路由时机** | 每条消息解码后查路由表 | 连接建立时一次性绑定 |
| **修改 wire.py** | 是——新增编解码函数 | **否** |
| **修改 envelope.py** | 是——新增 crm_id 字段 | **否** |
| **修改 msg_type.py** | 是——新增 CRM_CALL_V2 | **否** |
| **Rust SDK 影响** | 需实现新 wire 格式 | **wire 格式兼容，仅握手扩展** |
| **preregister_methods** | 不受影响 | **不受影响** |
| **向后兼容** | 新 msg_type 可区分 | handshake version 可区分 |

**控制面方案的核心优势：数据面完全不变，零热路径开销。**

#### 2.3 原方案 A/B/C 回顾（已废弃）

> ~~方案 A：CRM_CALL_V2 + namespace 前缀字段~~
> ~~方案 B：Handshake + 每条消息 crm_id~~
> ~~方案 C：method_name 复合编码（namespace::method）~~
>
> 以上三种方案均在数据面做文章，现已被控制面方案取代。保留此记录用于决策审计。

### 方案 D（推荐）：纯控制面路由 — Handshake 绑定 + per-connection CRM 亲和

**核心思路**：在 buddy handshake 阶段，client 声明目标 namespace，server 将该连接永久绑定到对应的 CRM slot。此后该连接上的所有 `CRM_CALL (0x03)` 消息自动路由到绑定的 CRM，**数据面 wire 格式完全不变**。

**实现路径**：

1. **Handshake v5 扩展**：在 buddy segment 交换之前增加 namespace 协商
2. **`BuddyConnection` 增加 CRM 亲和字段**：`conn.crm_slot: CRMSlot`
3. **Server 分发层**：从 `envelope.request_id`（已含 `conn_id`）反查连接的 CRM 绑定
4. **Client 地址解析**：`ipc://region_id/namespace` → socket path + handshake namespace

### 3. Server 端多 CRM 架构方案（控制面路由）

#### 3.1 核心数据结构重构

```python
# --- 新增：多 CRM 配置 ---
@dataclass
class CRMSlot:
    """一个 CRM 实例在 MultiServer 中的插槽。"""
    namespace: str
    crm: Any                  # CRM 实例
    icrm: Any                 # ICRM 实例（方向 '<-'）
    scheduler: _Scheduler     # 独立调度器
    on_shutdown: callable

@dataclass
class MultiServerConfig:
    """多 CRM 服务器配置。"""
    bind_address: str                           # 单一 UDS endpoint
    slots: dict[str, CRMSlotConfig]             # namespace → CRM 配置
    ipc_config: IPCConfig | None = None

@dataclass
class CRMSlotConfig:
    """单个 CRM 注册项的配置。"""
    crm: Any
    icrm: type
    concurrency: ConcurrencyConfig = field(default_factory=ConcurrencyConfig)
    on_shutdown: callable = lambda: None
```

#### 3.2 路由表设计（连接级绑定）

```python
class MultiServerState:
    """替代当前的 ServerState，支持多 CRM 路由。"""
    # namespace → CRMSlot（字符串查找，注册/注销时操作）
    slots: dict[str, CRMSlot]
    
    # conn_id → CRMSlot（连接级绑定，handshake 时建立）
    # 从 envelope.request_id 中提取 conn_id 即可查到绑定的 CRM
    conn_bindings: dict[int, CRMSlot]
    
    # 共享基础设施
    server: BaseServer
    event_queue: EventQueue
    stage: ServerStage
    lock: threading.RLock
    termination_event: threading.Event
    shutdown_events: list[threading.Event]
```

**关键区别**：不再需要 `slot_by_id: list[CRMSlot]`（数字 crm_id 索引），路由完全基于 `conn_id` → `CRMSlot` 的映射。这个映射在 handshake 时建立，连接关闭时移除。

#### 3.3 请求分发路径重构

```python
def _dispatch_crm_call_multi(state: MultiServerState, envelope: Envelope) -> None:
    """connection-aware CRM 请求分发（控制面路由）。
    
    利用已有的 request_id 格式 "{conn_id}:{request_id}" 提取 conn_id，
    查找该连接在 handshake 阶段绑定的 CRMSlot。
    CRM_CALL (0x03) wire 格式完全不变。
    """
    if state.stage is not ServerStage.STARTED:
        # error reply ...
        return

    # 1. 从 request_id 中提取 conn_id（已有格式，无额外开销）
    rid = envelope.request_id          # "3:42" 
    sep = rid.rfind(':')
    conn_id = int(rid[:sep])

    # 2. 查找该连接绑定的 CRM slot
    slot = state.conn_bindings.get(conn_id)
    if slot is None:
        # error: connection not bound to any CRM (handshake 未完成?)
        return

    # 3. 在目标 ICRM 上查找方法（与当前路径完全相同）
    method = getattr(slot.icrm, envelope.method_name, None)
    if method is None:
        # error: method not found
        return

    # 4. 提交到该 CRM 的独立调度器
    slot.scheduler.submit(
        request_id=envelope.request_id,
        method=method,
        args_bytes=envelope.payload,
        reply=state.server.reply,
    )
```

**与旧方案的对比**：
- ~~旧方案：从 `envelope.crm_id` 查 `slot_by_id[crm_id]`~~
- **新方案**：从 `envelope.request_id` 提取 `conn_id`，查 `conn_bindings[conn_id]`
- `request_id` 的 `"{conn_id}:{request_id}"` 格式已由 `ipc_v3_server.py:551` 建立，**无需任何新增字段**

#### 3.4 per-CRM 独立调度器的必要性

**必须使用 per-CRM 独立调度器**，原因：

1. **并发语义隔离**：CRM-A 可能是 `EXCLUSIVE` 模式（写操作互斥），CRM-B 可能是 `READ_PARALLEL`。共享调度器无法区分
2. **Reader-Writer Lock 作用域**：`_WriterPriorityReadWriteLock` 控制的是单个 CRM 的状态一致性。两个 CRM 的 read/write 操作互不影响
3. **故障隔离**：CRM-A 的方法执行异常不应阻塞 CRM-B 的 pending 队列
4. **max_pending 配额独立**：不同 CRM 可能有不同的并发容量

**实现**：每个 `CRMSlot` 持有独立的 `_Scheduler` 实例，各自拥有独立的 `ThreadPoolExecutor` + `_WriterPriorityReadWriteLock`。

### 4. Client 端地址格式设计

#### 4.1 地址格式方案

```
当前格式:  ipc://region_id
新格式:    ipc://region_id/namespace

示例:
  ipc://hydro_node_1/cc.grid        → 连接到 hydro_node_1 节点的 grid CRM
  ipc://hydro_node_1/cc.solver      → 连接到同一节点的 solver CRM
  ipc://hydro_node_1/cc.params      → 连接到同一节点的 params CRM
```

三个地址共享同一个 UDS socket (`/tmp/c_two_ipc/hydro_node_1.sock`)，通过 handshake 阶段注册不同的 namespace。

#### 4.2 Client 解析逻辑

```python
class IPCv3Client(BaseClient):
    def __init__(self, server_address: str, ipc_config: IPCConfig | None = None):
        # 解析 ipc://region_id/namespace
        raw = server_address.replace('ipc-v3://', '').replace('ipc://', '')
        parts = raw.split('/', 1)
        self.region_id = parts[0]
        self.namespace = parts[1] if len(parts) > 1 else None
        self._socket_path = _resolve_socket_path(self.region_id)
        # 无需 _crm_id — 路由在控制面完成，数据面不感知
```

#### 4.3 Handshake v5 — 纯控制面 namespace 绑定

当前 IPC v3 handshake (v4) 只交换 buddy pool segment 信息。扩展为 v5，在 buddy segment 交换前增加 namespace 协商：

```
Handshake v5 (控制面路由):

  Client → Server:
    [1B version=5]
    [1B ns_len][namespace UTF-8]              ← 新增：请求绑定的 CRM namespace
    [2B seg_count][per-segment: ...]          ← 原有：buddy segments（不变）
  
  Server → Client (ACK — 绑定成功):
    [1B version=5]
    [1B status=0x00]                          ← 成功
    [2B seg_count][per-segment: ...]          ← 原有：buddy segments（不变）
    
  Server → Client (NAK — namespace 不存在):
    [1B version=5]
    [1B status=0x01]                          ← 失败
    [1B err_len][error_msg UTF-8]             ← 错误描述
```

**关键变更点**：

1. **server 侧 `_handle_buddy_handshake()`**：解析 namespace 后查 `slots[namespace]`，在 `BuddyConnection` 上设置 `conn.crm_slot = slot`，同时将 `(conn_id, slot)` 注册到 `MultiServerState.conn_bindings`
2. **server 侧 `BuddyConnection` 新增字段**：`crm_slot: CRMSlot | None`
3. **client 侧 `_do_buddy_handshake()`**：在发送 buddy segments 前先编码 namespace
4. **client 侧检查 ACK status**：status=0x01 时抛出 `CCError` 提示 namespace 未注册

**与 v4 的向后兼容**：通过 `version` 字节区分。v4 的 version=4 不包含 namespace 字段；v5 的 version=5 总是包含。旧 server 收到 version=5 的 handshake 会因版本检查失败而拒绝，语义明确。

#### 4.4 Server 侧 handshake 处理伪代码

```python
async def _handle_buddy_handshake(self, conn, payload, writer):
    version = payload[0]
    
    if version == 5:
        # 解析 namespace
        ns_len = payload[1]
        namespace = bytes(payload[2:2+ns_len]).decode('utf-8')
        buddy_payload = payload[2+ns_len:]  # 剩余部分是 buddy segments
        
        # 查找 CRM slot
        slot = self._multi_state.slots.get(namespace)
        if slot is None:
            # 发送 NAK
            nak = _encode_handshake_nak(f"namespace '{namespace}' not registered")
            writer.write(encode_frame(0, FLAG_HANDSHAKE, nak))
            await writer.drain()
            return
        
        # 绑定连接到 CRM slot
        conn.crm_slot = slot
        self._multi_state.conn_bindings[conn.conn_id] = slot
        
        # 继续正常的 buddy segment 交换...
        segments = decode_buddy_handshake_v5_segments(buddy_payload)
        # ... (与 v4 相同的 buddy pool 初始化)
        
        # 发送 ACK (status=0x00) + buddy segments
        ack = _encode_handshake_ack(buddy_segments=[])
        writer.write(encode_frame(0, FLAG_HANDSHAKE, ack))
        await writer.drain()
    
    elif version == 4:
        # 向后兼容：无 namespace，走默认 CRM（如果只有一个）
        if len(self._multi_state.slots) == 1:
            slot = next(iter(self._multi_state.slots.values()))
            conn.crm_slot = slot
            self._multi_state.conn_bindings[conn.conn_id] = slot
        # ... (原有 v4 buddy handshake 逻辑)
```

### 5. Buddy Pool 共享策略

#### 方案对比

| 维度 | 共享 buddy pool | per-CRM buddy pool |
|------|---------------|-------------------|
| SHM segment 数量 | 1 组（共享） | N 组（每 CRM 独立） |
| 内存效率 | 高——所有 CRM 共享容量 | 低——每个 CRM 预分配固定容量 |
| 隔离性 | 低——一个 CRM 的大请求可能耗尽共享池 | 高——互不影响 |
| 实现复杂度 | 低——当前 per-connection 池不变 | 高——需 per-CRM-per-connection 池 |
| 零拷贝路径 | 不受影响——reuse 路径基于 seg_idx | 需维护 CRM → segment 映射 |

#### **推荐：共享 buddy pool（当前模型不变）**

理由：
1. buddy pool 是 **per-connection** 的（client 创建，server 通过 handshake 打开）。多个 CRM Client 连接到同一 server 时，每个连接天然有独立的 pool
2. 控制面路由方案下，一个连接只绑定一个 CRM，该连接的 pool 天然只服务于一个 CRM 的请求/响应——**隔离性与共享兼得**
3. 不需要修改 `BuddyConnection` 类或 pool 生命周期管理（仅新增 `crm_slot` 字段）

### 6. 动态注册/注销 CRM

#### 6.1 注册

```python
# 进程级单例 MultiServer
_multi_server: MultiServer | None = None

def register(namespace: str, icrm_cls: type, crm: Any, 
             on_shutdown: callable = None,
             concurrency: ConcurrencyConfig = None) -> None:
    """将 CRM 注册到进程级 MultiServer。首次调用时隐式启动 Server。"""
    global _multi_server
    if _multi_server is None:
        _multi_server = MultiServer(bind_address=_auto_address())
        _multi_server.start_background()
    
    _multi_server.add_crm(namespace, icrm_cls, crm, on_shutdown, concurrency)
```

#### 6.2 注销

```python
def unregister(namespace: str) -> None:
    """从 MultiServer 中移除 CRM。如果没有剩余 CRM，关闭 Server。"""
    if _multi_server is None:
        return
    _multi_server.remove_crm(namespace)
    if _multi_server.is_empty():
        _multi_server.shutdown()
        _multi_server = None
```

#### 6.3 安全性考量

- **已有连接的处理**：`remove_crm()` 时，已绑定该 namespace 的活跃连接在下一次 `CRM_CALL` 时收到错误回复 `"CRM namespace 'xxx' has been unregistered"`（`conn_bindings` 中的 slot 被标记为 invalid）
- **连接绑定清理**：连接关闭时自动从 `conn_bindings` 中移除（已有逻辑在 `_handle_client` 的 finally 块中，仅需追加一行 `state.conn_bindings.pop(conn_id, None)`）
- **线程安全**：`add_crm()` / `remove_crm()` 通过 `MultiServerState.lock` 保护；`conn_bindings` 的读写通过已有的 `_conn_lock` 保护

### 7. 对现有代码的变更影响评估（控制面方案）

#### 7.1 需要修改的文件

```
变更范围（按影响排序）——控制面方案大幅缩减了变更面：

HIGH IMPACT:
  src/c_two/rpc/server.py            # MultiServerState + _dispatch_crm_call_multi + conn_bindings
                                      # 但这是新增代码，不修改现有 Server 类

MEDIUM IMPACT:
  src/c_two/rpc/ipc/ipc_v3_protocol.py  # Handshake v5 编解码（新增函数，不改 v4）
  src/c_two/rpc/ipc/ipc_v3_server.py    # _handle_buddy_handshake 分支 v5
                                         # BuddyConnection 新增 crm_slot 字段
  src/c_two/rpc/ipc/ipc_v3_client.py    # 地址解析 + handshake 发送 namespace

LOW IMPACT:
  src/c_two/rpc/client.py               # Client 地址解析（提取 namespace 部分）
  tests/                                 # 新增多 CRM 测试
```

#### 7.2 不需要修改的文件（显著扩大）

```
完全无需变更（控制面方案的核心优势）：

  src/c_two/rpc/util/wire.py          ← 数据面不变！CRM_CALL 格式不变！
  src/c_two/rpc/event/envelope.py     ← 无需 crm_id 字段！
  src/c_two/rpc/event/msg_type.py     ← 无需 CRM_CALL_V2！
  src/c_two/rpc/ipc/ipc_protocol.py   # frame header 不变
  src/c_two/rpc/ipc/shm_pool.py       # SHM pool 不感知 CRM
  src/c_two/rpc/transferable.py        # 序列化层不变
  src/c_two/crm/meta.py               # ICRM 定义不变
  src/c_two/compo/                     # 组件层——但未来 cc.connect() 需要适配
  rust/c2_buddy/                       # buddy allocator 不感知业务
```

**对比旧方案**：旧的数据面方案需要修改 `wire.py`、`envelope.py`、`msg_type.py` 三个核心文件；控制面方案将这三个文件全部从变更列表中移除。

### 8. 性能影响分析（控制面方案）

#### 8.1 热路径额外开销

| 环节 | 当前开销 | 多 CRM 额外开销 | 影响评估 |
|------|---------|----------------|---------|
| CRM_CALL 编码 | `write_call_into()` 写 header + payload | **0** — wire 格式不变 | **零开销** |
| CRM_CALL 解码 | `decode()` 解析 method_name | **0** — 解码逻辑不变 | **零开销** |
| 请求路由 | `getattr(icrm, method)` | `conn_bindings[conn_id]` dict 查找 + `getattr` | **~15ns**，dict lookup O(1) |
| conn_id 提取 | 不适用 | `rid.rfind(':')` + `int()` | **~10ns**，已有模式复用 |
| Scheduler submit | 直接提交 | 相同——slot 内独立 scheduler | **0**，路径相同 |
| Buddy alloc/free | per-connection pool | 不变——pool 是连接级的 | **0**，无影响 |
| Handshake | 交换 buddy segments | +namespace 发送 + slot 查找 | **一次性**，<1μs |

**总结**：热路径额外开销约 **25ns/call**（仅 conn_id 提取 + dict 查找），与旧方案的 15-20ns 近似，但 **wire 编解码零开销** 是决定性优势。相比 IPC v3 的 P50 ~0.4ms（10MB），影响 < 0.006%。

#### 8.2 内存开销

| 资源 | 单 CRM | 多 CRM (N 个) | 变化 |
|------|--------|-------------|------|
| 进程 | 1 | 1 | **-（N-1）个进程** |
| UDS socket | 1 | 1 | **-（N-1）个 fd** |
| asyncio 线程 | 1 | 1 | **-（N-1）个线程** |
| EventQueue 线程 | 1 | 1 | **-（N-1）个线程** |
| _Scheduler | 1 | N (per-CRM) | 不变（总量一样） |
| ThreadPoolExecutor | 1 | N (per-CRM) | 不变（总量一样） |
| buddy pool | per-connection | per-connection | 不变 |

**结论**：多 CRM 方案在进程/线程/fd 资源上有 O(N) 级别的节省。

### Prototype/Testing Notes

#### PoC 验证路径（控制面方案 — 简化为 3 阶段）

1. **Phase 1: Server 路由层**
   - 实现 `MultiServerState` + `CRMSlot` + `conn_bindings`
   - 实现 `_dispatch_crm_call_multi()` 基于 conn_id 的路由分发
   - 两个 mock CRM 注册到同一 Server，验证路由正确
   - **注意**：wire.py / envelope.py / msg_type.py 全部不需要修改

2. **Phase 2: Handshake v5 + 连接绑定**
   - 在 `ipc_v3_protocol.py` 中新增 v5 handshake 编解码函数
   - `ipc_v3_server.py`: handshake 分支处理 version=5，解析 namespace 绑定 CRM slot
   - `ipc_v3_client.py`: 地址解析 `ipc://region_id/namespace`，handshake 发送 namespace
   - `BuddyConnection` 新增 `crm_slot` 字段

3. **Phase 3: 端到端测试**
   - 多个 client 分别连接同一 server 的不同 CRM namespace
   - 验证并发正确性、故障隔离、动态注册/注销
   - v4 client 连 MultiServer（只有一个 CRM 时向后兼容）

### External Resources

- [gRPC Service Multiplexing](https://grpc.io/docs/guides/) — 单端口多 service 的参考实现
- [Cap'n Proto — Multi-interface objects](https://capnproto.org/rpc.html) — 对象级路由参考
- [Ray Actor Model](https://docs.ray.io/en/latest/ray-core/actors.html) — 有状态资源托管参考
- `doc/log/sota.md` §4.1 — 项目内部的"单端口多路复用"设计建议

## Decision

### Recommendation

采用 **方案 D（纯控制面路由）** — Handshake v5 namespace 绑定 + per-connection CRM 亲和，结合 **per-CRM 独立调度器** 和 **共享 buddy pool** 的 Server 架构，实现单 IPC v3 Server 多 CRM 托管。

**核心原则：数据面（wire 协议）完全不变，路由信息仅在控制面（handshake）传递。**

### Rationale

1. **数据面零侵入**：`CRM_CALL (0x03)` wire 格式、`Envelope` 结构、`MsgType` 枚举 —— 全部不变。这是与旧方案（A/B/C）的根本区别
2. **架构不变量的精准利用**：一个 ICRM 消费者 = 一个 `Client` = 一个连接 = 一个 CRM 目标。连接级绑定是对这个不变量的最自然表达
3. **热路径零开销（编解码层面）**：路由成本仅为 `conn_id` 提取（已有的 `rfind(':')`）+ `dict` 查找，约 25ns/call，编解码无额外字节
4. **最小变更面**：不修改 `wire.py`、`envelope.py`、`msg_type.py`。Rust SDK 只需扩展 handshake，wire 格式兼容无需改动
5. **与 SOTA 目标对齐**：为 `cc.register()` / `cc.connect()` API 提供底层基础设施

### Implementation Notes

#### 实施分层策略（简化为 3 阶段）

```
Phase 1 (Server Core):
  server.py       — MultiServerState + CRMSlot + conn_bindings + _dispatch_crm_call_multi
  → 新增 MultiServer 类，不修改现有 Server 类
  → 旧 Server 继续工作（单 CRM 场景）

Phase 2 (IPC v3 Control Plane):
  ipc_v3_protocol.py  — Handshake v5 编解码（新增函数，v4 不变）
  ipc_v3_server.py    — _handle_buddy_handshake v5 分支 + BuddyConnection.crm_slot
  ipc_v3_client.py    — 地址解析 ipc://region_id/namespace + handshake 发 namespace
  client.py           — Client 地址解析
  → 控制面扩展，数据面不动

Phase 3 (API Surface):
  cc.register() / cc.connect() / cc.unregister()
  → 建立在 Phase 1-2 之上的用户面 API
  → 与 sota.md 中的模式设计对齐
```

**注意**：旧方案的 "Phase 0 (Foundation)" 整体移除 —— 不再需要修改 wire/envelope/msg_type。

#### 关键设计约束

1. **现有 Server 类不删除**：`MultiServer` 是新增类，`Server` 保留用于单 CRM 场景和向后兼容
2. **CRM_CALL (0x03) 唯一格式**：数据面只有一种 CRM_CALL，路由完全由控制面决定
3. **v4 handshake 向后兼容**：MultiServer 收到 v4 handshake 时，如果只注册了一个 CRM 则默认绑定到它，否则拒绝连接
4. **method name 预编码缓存全局共享**：多个 ICRM 的 method name 都注册到同一个 `_call_header_cache`，不受控制面路由影响
5. **thread:// 协议暂不涉及**：本 spike 聚焦 IPC v3 协议，thread:// 的多 CRM 支持（直接返回实例）是后续 spike
6. **`conn_bindings` 的生命周期管理**：连接关闭时自动清理绑定（追加到 `_handle_client` 的 `finally` 块）

### Follow-up Actions

- [ ] 实施 Phase 1: MultiServer 核心架构 + 集成测试
- [ ] 实施 Phase 2: IPC v3 handshake v5 控制面扩展 + 端到端测试
- [ ] 实施 Phase 3: `cc.register()` / `cc.connect()` API 设计（需独立 spike）
- [ ] 更新 `doc/log/sota.md` 反映本 spike 的决策结论
- [ ] 评估 Rust Routing Server 对 handshake v5 的兼容性（wire 格式无需变更）
- [ ] 设计多 CRM 场景的性能基准测试

## Status History

| Date | Status | Notes |
| --- | --- | --- |
| 2026-03-26 | 🔴 Not Started | Spike 创建并完成初始调研：分析了 wire 协议、Server 架构、调度器、buddy pool，给出了三种 wire 扩展方案（A/B/C）对比 |
| 2026-03-26 | 🟡 In Progress | 方案修订：基于"一个 ICRM 消费者 = 一个连接 = 一个 CRM 目标"的架构不变量，将路由从数据面（per-message crm_id）移至控制面（handshake namespace 绑定）。新增方案 D 取代 A/B/C，数据面 wire 格式零变更，变更面大幅缩减（移除 wire.py/envelope.py/msg_type.py 修改） |

---

_Last updated: 2026-03-26 by soku_
