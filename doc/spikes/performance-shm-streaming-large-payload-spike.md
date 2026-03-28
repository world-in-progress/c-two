---
title: "SHM 流式传输：处理超越 Segment 容量的大 Payload"
category: "Performance"
status: "🟡 In Progress"
priority: "High"
timebox: "1 week"
created: 2026-03-28
updated: 2026-03-28
owner: "soku"
tags: ["technical-spike", "performance", "streaming", "shm", "ipc-v3", "buddy-allocator"]
---

# SHM 流式传输：处理超越 Segment 容量的大 Payload

## Summary

**Spike Objective:** 设计一种机制，使 IPC-v3 协议能够传输超过单个 Buddy Segment 容量（默认 256 MB）的 payload，同时保持零拷贝优势和与现有并发请求模型的兼容性。

**Why This Matters:** 在 GIS 耦合模拟场景中，单次 RPC 调用可能传输 GB 级的网格/栅格数据（如 1 GB 的 mesh 数据）。当前实现中，payload 超过 `pool_segment_size`（默认 256 MB）时，buddy allocator 无法在已有 segment 中分配，退化到 dedicated segment。Dedicated segment 虽然底层使用 POSIX `shm_open()` + `MAP_SHARED`（理论上跨进程可访问），但其 `seg_idx` 从 256 起步，**从未通过握手通告给对端**，对端的 `seg_views[]` 无法索引到它（`seg_idx >= len(seg_views)` → 静默丢弃）。因此 client 代码遇到 dedicated 分配后立即 free 并回退到 inline fallback（上限 16 MB），最终抛出 `MemoryPressureError`。这意味着 **当前架构下单次 RPC 调用的有效载荷上限约为 256 MB**，这对于科学计算场景是不可接受的硬限制。

**Timebox:** 1 week

**Decision Deadline:** P1 milestone (v0.3.0) — 阻塞大 payload 场景的生产使用

## Research Question(s)

**Primary Question:** 如何在不破坏现有 Wire v2 协议兼容性和并发模型的前提下，支持传输超过单个 Buddy Segment 容量的 payload？

**Secondary Questions:**

- 流式分块（Chunked Streaming）是否是唯一可行方案？是否有替代方案（如跨 segment 分散写入、临时 mmap 大文件）？
- 如何在 32-bit flags 字段中编码流式控制信息（continuation/final 标记）而不破坏现有帧格式？
- 流式传输如何与 SharedClient 的并发请求模型（RID 多路复用 + `_send_lock` + `_alloc_lock`）共存？
- 服务端如何重组分块数据？是否需要临时缓冲区，还是可以流式传递给 CRM 方法？
- 对 Relay（Python/Rust）的影响——HTTP 层是否需要支持 chunked transfer encoding？

## Investigation Plan

### Research Tasks

- [x] 审计现有 Wire v2 帧格式和 flags 字段的扩展空间
- [x] 审计 SharedClient 的发送/接收路径和锁模型
- [x] 审计 ServerV2 的请求处理和响应路径
- [x] 审计 Buddy Allocator 的分配/释放接口
- [x] 审计 Relay 层（Python/Rust）的 payload 缓冲模型
- [x] 审计 SOTA 文档中关于 streaming 的规划
- [x] 分析可行的技术方案并对比 trade-off
- [ ] 设计详细的协议扩展方案
- [ ] 设计实现路径和改动范围

### Success Criteria

**This spike is complete when:**

- [x] 完成现有协议栈的全面审计
- [x] 明确当前的技术限制和瓶颈
- [x] 产出至少两个可行方案并完成对比分析
- [ ] 给出推荐方案及详细实现路径
- [ ] 明确改动范围和兼容性影响

## Technical Context

**Related Components:**

| 组件 | 文件 | 影响程度 |
|------|------|---------|
| Wire v2 编解码 | `rpc_v2/wire.py` | 高 — 需要新帧类型 |
| 协议标志位 | `rpc_v2/protocol.py` | 中 — 需要新 FLAG 常量 |
| SharedClient 发送 | `rpc_v2/client.py` | 高 — 分块发送逻辑 |
| SharedClient 接收 | `rpc_v2/client.py` | 高 — 分块重组逻辑 |
| ServerV2 请求处理 | `rpc_v2/server.py` | 高 — 分块接收和重组 |
| ServerV2 响应发送 | `rpc_v2/server.py` | 高 — 分块响应逻辑 |
| Buddy Allocator | `buddy/` (Rust) | 低 — 接口不变，只是多次 alloc |
| IPC 帧头格式 | `rpc/ipc/ipc_protocol.py` | 低 — 16B header 不变 |
| Python Relay | `rpc_v2/relay.py` | 中 — 需要流式转发 |
| Rust Relay | `_native/c2-relay/` | 中 — 需要流式转发 |
| HTTP Client | `rpc_v2/http_client.py` | 低 — HTTP 自身已支持大 body |
| Backpressure | `error.py` | 低 — MemoryPressureError 语义不变 |

**Dependencies:**
- roadmap §2.1 (SHM Pool 动态扩容) — 本 spike 是其超集
- roadmap §4.2 (Streaming RPC / Pipeline) — 本 spike 解决其前置问题
- Buddy allocator 的多 segment 支持已完成（`max_segments` + 弹性扩容）

**Constraints:**
- Wire v2 帧格式的 32-bit flags 字段有 bits 9-31 可用（23 bits 空间充裕）
- `FRAME_STRUCT = struct.Struct('<IQI')` — 16 字节 header 不可改变（兼容性）
- Buddy `alloc()` 返回的 `PoolAlloc` 包含 `seg_idx` + `offset` — 跨 segment 写入需要多次 `alloc()`
- `_send_lock` 保证帧原子性 — 流式发送需要在持锁期间发送多帧，或放松锁粒度
- `_recv_loop` 是单线程顺序读取 — 分块帧必须支持与其他 RID 的响应帧交错

---

## Research Findings

### 1. 现状分析：当前大 payload 的失败路径

当 payload 大小为 1 GB 时（`pool_segment_size` = 256 MB），调用链如下：

```
SharedClient.call(method, 1GB_data)
  → _try_buddy_alloc(1GB)
    → buddy_pool.alloc(1GB)
      → Layer 1: 扫描现有 segment (256MB) — 全部不够 → 失败
      → Layer 2: 创建新 segment — segment_size=256MB < 1GB → 不够
      → Layer 3: dedicated segment — 创建 1GB SHM
        → 返回 PoolAlloc(is_dedicated=True)
    → is_dedicated=True → free_at() + 检查 inline 上限
      → 1GB > max_frame_size(16MB)
        → raise MemoryPressureError ❌
```

**关键洞察：** Dedicated segment 虽然可以分配 1GB，且底层使用 POSIX `shm_open()` + `MAP_SHARED`（跨进程可访问的共享内存），但问题在于**协议层没有动态通告机制**——dedicated segment 的 `seg_idx`（≥256）从未通过握手通告给对端，对端的 `seg_views[]` 数组无法索引到它。因此 client 代码会立即释放它并回退到 inline。而 inline 上限是 16 MB，远小于 1 GB。

### 2. 可用的 Flag 位

```
32-bit flags 字段当前使用情况:

Bit 0:  FLAG_SHM           (legacy, ipc v2)
Bit 1:  FLAG_RESPONSE       (方向标记)
Bit 2:  FLAG_HANDSHAKE      (握手消息)
Bit 3:  FLAG_POOL           (legacy, ipc v2)
Bit 4:  FLAG_CTRL           (控制消息)
Bit 5:  FLAG_DISK_SPILL     (reserved, 未使用)
Bit 6:  FLAG_BUDDY          (buddy 分配的 SHM 块)
Bit 7:  FLAG_CALL_V2        (v2 调用帧)
Bit 8:  FLAG_REPLY_V2       (v2 回复帧)
Bit 9-31: ────────── 可用 (23 bits) ──────────
```

**可分配给 streaming:**
- `Bit 9: FLAG_STREAM_MORE` — 后续还有更多分块
- `Bit 10: FLAG_STREAM_FINAL` — 这是最后一个分块（或可与 bit 9 互斥，仅用 1 bit）

### 3. SharedClient 并发模型约束

```
Thread A (RID=1, 流式发送 1GB):     Thread B (RID=2, 普通请求):
├─ send_lock.acquire()                │ (等待 send_lock)
│  sendall(chunk_0, 256MB)            │
│  sendall(chunk_1, 256MB)            │
│  sendall(chunk_2, 256MB)            │
│  sendall(chunk_3, 256MB)            │
│  send_lock.release()                │
│                                     ├─ send_lock.acquire()
│                                     │  sendall(normal_frame)
│                                     │  send_lock.release()
└─ pending[1].wait()                  └─ pending[2].wait()
```

**问题:** 如果流式发送持有 `_send_lock` 整个过程，Thread B 会被阻塞直到 1 GB 传输完毕。

**解决思路:**
- **方案 A (帧级交错):** 每个分块是独立帧，`_send_lock` 每帧获取/释放一次。不同 RID 的帧可以交错。
- **方案 B (持锁批发):** 流式发送持有 `_send_lock` 直到全部分块发完。简单但阻塞其他请求。

### 4. 服务端重组约束

ServerV2 的 `_handle_client` 使用 asyncio StreamReader 顺序读取帧:

```python
async def _handle_client(self, reader, writer):
    while True:
        header = await reader.readexactly(16)
        total_len, request_id, flags = FRAME_STRUCT.unpack(header)
        payload = await reader.readexactly(total_len - 12)
        # 分发到 CRM 执行
```

**分块接收需要:**
- 识别 `FLAG_STREAM_MORE` → 累积到 per-RID 缓冲区
- 识别最后一个分块 → 拼装完整 payload → 分发执行
- 同时可能收到其他 RID 的帧（如果客户端采用帧级交错）

---

## 方案对比分析

### 方案 A: 多帧分块流式传输（Chunked Multi-Frame Streaming）

**核心思路:** 将大 payload 拆分为多个 buddy-block-sized 的分块，每个分块独立编码为一个完整的 UDS 帧，共享同一个 `request_id`，用 flag bit 标记 continuation/final。

#### 协议扩展

```
新增 Flag:
  Bit 9:  FLAG_CHUNKED (0x200) — 帧属于分块序列

新增 Chunk Header (在 buddy payload 之后、v2 control 之前):
  [4B chunk_idx LE]    — 分块序号 (0-based)
  [4B total_chunks LE] — 分块总数 (0 = 未知, 用于流式生产场景)
  共 8 字节

帧布局 (分块 buddy call):
  [16B frame header (flags = FLAG_BUDDY | FLAG_CALL_V2 | FLAG_CHUNKED)]
  [11B buddy_payload]
  [8B chunk_header]
  [v2_call_control]       ← 仅在 chunk_idx=0 时携带 route+method

帧布局 (分块 buddy reply):
  [16B frame header (flags = FLAG_RESPONSE | FLAG_BUDDY | FLAG_REPLY_V2 | FLAG_CHUNKED)]
  [11B buddy_payload]
  [8B chunk_header]
  [1B status]             ← 仅在最后一个 chunk 携带 status
```

#### 客户端发送流程

```python
def _send_chunked_v2(self, rid, method_name, data, name):
    chunk_size = self._config.pool_segment_size // 2   # 留出余量
    total = len(data)
    n_chunks = ceil(total / chunk_size)
    
    for i in range(n_chunks):
        start = i * chunk_size
        end = min(start + chunk_size, total)
        chunk_data = data[start:end]
        
        # 每个 chunk 独立 alloc + send
        alloc, shm_buf = self._try_buddy_alloc(len(chunk_data), 'v2-chunk')
        
        if alloc is None:
            # inline fallback per-chunk (如果 chunk_size <= max_frame_size)
            frame = encode_v2_inline_chunked_call(rid, name, method_idx,
                                                   chunk_data, i, n_chunks)
        else:
            shm_buf[:len(chunk_data)] = chunk_data
            frame = encode_v2_buddy_chunked_call(rid, alloc, name, method_idx,
                                                  i, n_chunks)
        
        with self._send_lock:
            sock.sendall(frame)
        
        # 注意: send_lock 每帧释放一次，允许其他 RID 交错
```

#### 服务端接收重组流程

```python
# server.py: _handle_client 中的分块缓冲

class _ChunkBuffer:
    """Per-RID chunk accumulator."""
    __slots__ = ('chunks', 'total_chunks', 'received', 'route', 'method_idx')
    
    def __init__(self, total_chunks, route, method_idx):
        self.chunks = [None] * total_chunks
        self.total_chunks = total_chunks
        self.received = 0
        self.route = route
        self.method_idx = method_idx
    
    def add(self, idx, data):
        self.chunks[idx] = data
        self.received += 1
        return self.received == self.total_chunks
    
    def assemble(self) -> bytes:
        return b''.join(self.chunks)

# In _handle_client:
_chunk_buffers: dict[int, _ChunkBuffer] = {}

if flags & FLAG_CHUNKED:
    chunk_idx, total_chunks = decode_chunk_header(payload, offset)
    
    if chunk_idx == 0:
        route, method_idx, _ = decode_call_control(payload, ctrl_offset)
        _chunk_buffers[request_id] = _ChunkBuffer(total_chunks, route, method_idx)
    
    buf = _chunk_buffers[request_id]
    buf.add(chunk_idx, shm_data)   # shm_data 读出后 free buddy block
    
    if buf.add returns True:
        full_payload = buf.assemble()
        del _chunk_buffers[request_id]
        # 分发到 CRM 执行
        await self._dispatch(buf.route, buf.method_idx, full_payload, ...)
```

#### 优点
- ✅ **帧级交错:** 不同 RID 的帧可以自由交叉，不阻塞小请求
- ✅ **内存效率:** 每个 chunk 用完即 free，客户端峰值内存 = chunk_size（而非整个 payload）
- ✅ **协议兼容:** 不识别 `FLAG_CHUNKED` 的老客户端不会误解（老服务端收到未知 flag 会忽略帧）
- ✅ **Backpressure 自然延伸:** 每个 chunk 独立走 `_try_buddy_alloc()`，pool 满时逐 chunk inline fallback

#### 缺点
- ❌ **服务端重组成本:** 需要 per-RID 的 `_ChunkBuffer`，最终 `b''.join()` 产生完整 payload 的内存拷贝
- ❌ **无法真正流式处理:** CRM 方法签名是 `def method(self, args) -> result`，必须等全部 chunk 到齐才能调用
- ❌ **chunk 乱序:** 帧级交错意味着 chunk 可能乱序到达（但单连接 UDS 保证 FIFO 顺序 — 不是问题）
- ❌ **send_lock 粒度:** 每帧独立获取 `_send_lock` → N 次 lock/unlock 开销

---

### 方案 B: 多 Segment 分散引用（Scatter-Reference）

**核心思路:** 单帧中携带多个 buddy block 引用，payload 分散在多个 segment 的不同 block 中。接收方按引用列表顺序读取并拼装。

#### 协议扩展

```
新增 Flag:
  Bit 9: FLAG_SCATTER (0x200) — 帧包含多个 buddy 引用

Scatter Payload Header:
  [1B n_refs]  — buddy 引用数量 (1-255)
  [n_refs × 11B buddy_payload]  — 每个引用指向一个 SHM block
  [v2_call_control]

帧布局:
  [16B frame header (flags = FLAG_BUDDY | FLAG_CALL_V2 | FLAG_SCATTER)]
  [1B n_refs]
  [11B buddy_ref_0][11B buddy_ref_1]...[11B buddy_ref_N]
  [v2_call_control]
```

#### 优点
- ✅ **单帧原子:** 一次 `sendall()` 发送整个帧，`_send_lock` 只获取一次
- ✅ **简单重组:** 接收方按引用顺序读取 SHM block，拼装为完整 payload
- ✅ **零额外帧开销:** 不增加帧数量，只增加 per-ref 的 11 字节元数据

#### 缺点
- ❌ **同时持有多个 buddy block:** 1 GB payload 需要 4 个 256MB block — 需要 4 个 segment 同时 alloc。如果 `max_segments=4`，pool 被一个请求完全占满
- ❌ **alloc() 多次可能部分失败:** 分配了 3 个 block 后第 4 个失败，需要回滚前 3 个的 free
- ❌ **阻塞时间长:** `_send_lock` 持有期间需要准备所有 block（alloc + 数据写入），阻塞其他请求
- ❌ **接收方内存峰值:** 需要同时读取 N 个 SHM block 的数据 → 重组时峰值 = 整个 payload

---

### 方案 C: 磁盘溢写 + SHM 元数据引用（Disk Spill）

**核心思路:** 超大 payload 写入临时文件（`/tmp` 或 `mmap`），帧中只传递文件路径/描述符，接收方直接 `mmap()` 读取。

#### 协议扩展

```
复用: FLAG_DISK_SPILL (Bit 5, 已预留)

Disk Spill Payload:
  [4B path_len LE][path UTF-8][8B data_size LE][8B offset LE]

帧布局:
  [16B frame header (flags = FLAG_DISK_SPILL | FLAG_CALL_V2)]
  [disk_spill_payload]
  [v2_call_control]
```

#### 优点
- ✅ **无 SHM 容量限制:** 磁盘空间远大于 SHM
- ✅ **Flag 已预留:** `FLAG_DISK_SPILL` (bit 5) 在 ipc_protocol.py 中已声明
- ✅ **零拷贝潜力:** 发送方 mmap 写入 → 接收方 mmap 读取（如果同节点）
- ✅ **不影响 buddy pool:** 大 payload 不消耗 SHM pool 资源

#### 缺点
- ❌ **性能降级严重:** 磁盘 I/O（即使 NVMe SSD ~3 GB/s）比 SHM (~20 GB/s memcpy) 慢 5-7x
- ❌ **仅限同节点:** 跨节点场景无法共享文件路径
- ❌ **安全风险:** 临时文件可能被其他进程读取
- ❌ **清理复杂:** 需要可靠的 GC 机制清理临时文件
- ❌ **不适用于 relay:** HTTP relay 转发时需要上传文件内容

---

### 方案对比矩阵

| 维度 | A: 多帧分块 | B: 分散引用 | C: 磁盘溢写 |
|------|-----------|-----------|-----------|
| **最大 payload** | 无限（逐 chunk 传输） | segment_size × 255 (~64 GB) | 无限（磁盘容量） |
| **客户端峰值内存** | chunk_size (~128MB) | 整个 payload | 接近零（mmap lazy） |
| **服务端峰值内存** | 整个 payload（重组） | 整个 payload（重组） | 接近零（mmap lazy） |
| **并发友好** | ✅ 帧级交错 | ❌ 长时间持锁 | ✅ 不占 SHM pool |
| **实现复杂度** | 中 — 分块/重组逻辑 | 低 — 单帧扩展 | 高 — 文件生命周期 |
| **传输性能** | 好 — SHM 级别 | 好 — SHM 级别 | 差 — 磁盘 I/O |
| **跨节点** | ✅ 每 chunk 可 inline | ✅ 每 ref 可 inline | ❌ 仅同节点 |
| **relay 兼容** | ✅ HTTP chunked encoding | ❌ 需要持有多 block | ❌ 无法转发文件 |
| **协议兼容** | ✅ 新 flag + 新帧 | ✅ 新 flag + 扩展 | ✅ 已预留 flag |
| **backpressure** | ✅ per-chunk 自然背压 | ⚠️ 需要 N 块同时可用 | ✅ 不消耗 SHM |

---

## Decision

### Recommendation

**推荐方案 A（多帧分块流式传输）+ 方案 C 作为 L3 兜底。**

方案 A 是唯一同时满足以下全部需求的方案：
1. 突破 segment_size 上限，理论支持无限大 payload
2. 保持并发友好（帧级交错不阻塞其他 RID）
3. 客户端峰值内存控制在 chunk_size 级别
4. 天然兼容 HTTP relay（chunked transfer encoding）
5. 为未来 P3 Streaming RPC (§4.2) 奠定帧格式基础

方案 B 的核心问题是**同时占用多个 segment** — 在默认配置下（4 segments × 256 MB），一个 1 GB 请求会完全耗尽 pool，阻塞所有其他请求。方案 C 可作为极端场景的 L3 兜底（当 SHM + inline 都失败时回退到磁盘），但不应作为主路径。

### Rationale

**为什么不是 B (分散引用)?**

B 的"单帧原子"优势被其致命缺陷抵消：
- 4 segment pool 被 1 个请求独占 → 其他并发请求全部 `MemoryPressureError`
- 部分分配失败的回滚逻辑增加复杂度
- `_send_lock` 长时间持有导致 head-of-line blocking

**为什么 A 优于 C (磁盘溢写)?**

- 性能差距: SHM memcpy ~20 GB/s vs NVMe SSD ~3 GB/s (6-7x)
- 跨节点支持: A 的 chunk 可 inline fallback 到 UDS 字节流 → relay 可转发到远程节点; C 的文件路径无法跨节点
- 实现简洁: A 是对现有帧格式的自然扩展; C 需要全新的文件生命周期管理

**A + C 组合策略:**

```
payload 大小决策树:
  ≤ 4 KB (shm_threshold)      → inline 直接传输
  ≤ segment_size (~256 MB)    → 单次 buddy alloc (现有路径)
  > segment_size               → 方案 A: 多帧分块 (chunk_size = segment_size / 2)
    若分块也失败 (pool 完全耗尽 + chunk > max_frame_size):
      → 方案 C: 磁盘溢写兜底 (FLAG_DISK_SPILL, 仅限同节点)
      → 若非同节点 → MemoryPressureError (不可恢复)
```

### Implementation Notes — 详细实现路径

#### Phase 1: 协议层 (wire.py + protocol.py)

**1.1 新增常量 (`protocol.py`)**

```python
FLAG_CHUNKED    = 1 << 9   # 0x200 — 帧属于分块序列
FLAG_CHUNK_LAST = 1 << 10  # 0x400 — 这是序列的最后一个分块

# Capability negotiation (handshake v5)
CAP_CHUNKED = 1 << 2       # 支持分块传输
```

> 使用两个 bit 而非单个 MORE/FINAL: 避免接收方需要持续检测下一帧。
> `FLAG_CHUNKED` 标记该帧是分块序列的一部分；`FLAG_CHUNK_LAST` 标记这是最后一块。
> 首块同时设置 `FLAG_CHUNKED` + `FLAG_CHUNK_LAST` 表示只有一个 chunk（退化为普通帧）。

**1.2 Chunk Header 编解码 (`wire.py`)**

```python
_CHUNK_HDR = struct.Struct('<HH')  # 4 bytes: chunk_idx(2B) + total_chunks(2B)
CHUNK_HEADER_SIZE = _CHUNK_HDR.size  # 4 bytes

def encode_chunk_header(chunk_idx: int, total_chunks: int) -> bytes:
    """Encode chunk metadata. total_chunks=0 means unknown (streaming producer)."""
    return _CHUNK_HDR.pack(chunk_idx, total_chunks)

def decode_chunk_header(data: bytes | memoryview, offset: int = 0) -> tuple[int, int]:
    """Returns (chunk_idx, total_chunks)."""
    return _CHUNK_HDR.unpack_from(data, offset)
```

> 使用 2B chunk_idx + 2B total_chunks = 4 字节（而非 4+4=8 字节）。
> 65535 chunks × 128 MB/chunk = 8 TB 理论上限，远超实际需求。
> total_chunks=0 保留用于未来的流式生产场景（生产者不知道总量）。

**1.3 分块帧编码器 (`wire.py`)**

```python
def encode_v2_buddy_chunked_call(
    request_id: int,
    seg_idx: int, offset: int, data_size: int, is_dedicated: bool,
    chunk_idx: int, total_chunks: int,
    name: str, method_idx: int,
) -> bytes:
    """Build a v2 chunked buddy call frame.
    
    Layout:
      [16B header (flags=FLAG_BUDDY|FLAG_CALL_V2|FLAG_CHUNKED[|FLAG_CHUNK_LAST])]
      [11B buddy_payload]
      [4B chunk_header]
      [v2_call_control]    ← only when chunk_idx == 0
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    chunk = encode_chunk_header(chunk_idx, total_chunks)
    
    flags = FLAG_BUDDY | FLAG_CALL_V2 | FLAG_CHUNKED
    if chunk_idx == total_chunks - 1:
        flags |= FLAG_CHUNK_LAST
    
    if chunk_idx == 0:
        ctrl = encode_call_control(name, method_idx)
        payload = buddy + chunk + ctrl
    else:
        payload = buddy + chunk
    
    return encode_frame(request_id, flags, payload)
```

#### Phase 2: 客户端发送 (client.py)

**2.1 分块阈值判断**

在 `SharedClient.call()` 中，当 `payload_size > self._config.pool_segment_size * 0.9` 时（留 10% 安全余量），切换到分块发送路径：

```python
_CHUNK_THRESHOLD_RATIO = 0.9

def call(self, method_name, data, *, name=None):
    wire_size = payload_total_size(data)
    
    if wire_size > int(self._config.pool_segment_size * _CHUNK_THRESHOLD_RATIO):
        return self._call_chunked(method_name, data, wire_size, name=name)
    else:
        return self._call_normal(method_name, data, wire_size, name=name)
```

**2.2 分块发送核心逻辑**

```python
def _call_chunked(self, method_name, data, total_size, *, name=None):
    """Send a large payload as multiple chunked frames."""
    chunk_size = self._config.pool_segment_size // 2  # 128 MB default
    n_chunks = ceil(total_size / chunk_size)
    
    route_name = name or self._default_name
    table = self._name_tables.get(route_name, self._method_table)
    method_idx = table.index_of(method_name)
    
    rid = self._next_rid()
    pending = PendingCall(rid)
    with self._pending_lock:
        self._pending[rid] = pending
    
    try:
        for i in range(n_chunks):
            start = i * chunk_size
            end = min(start + chunk_size, total_size)
            chunk_data = data[start:end] if isinstance(data, (bytes, memoryview)) \
                         else self._slice_scatter(data, start, end)
            chunk_len = end - start
            
            alloc, shm_buf = self._try_buddy_alloc(chunk_len, f'chunk-{i}')
            
            with self._send_lock:
                if alloc is not None:
                    shm_buf[:chunk_len] = chunk_data
                    frame = encode_v2_buddy_chunked_call(
                        rid, alloc.seg_idx, alloc.offset, chunk_len,
                        alloc.is_dedicated, i, n_chunks, route_name, method_idx,
                    )
                else:
                    frame = encode_v2_inline_chunked_call(
                        rid, route_name, method_idx,
                        chunk_data, i, n_chunks,
                    )
                self._sock.sendall(frame)
    except Exception:
        with self._pending_lock:
            self._pending.pop(rid, None)
        raise
    
    return pending.wait(timeout=self._config.call_timeout)
```

**关键设计决策:**
- `chunk_size = segment_size / 2` — 确保单个 segment 能同时容纳至少 2 个 chunk 的分配（一个正在传输，一个正在写入）
- `_send_lock` 包裹每个帧的 alloc+write+send — 保证帧原子性但允许帧间交错
- 发送失败时清理 pending — 接收方 chunk buffer 有超时 GC

#### Phase 3: 服务端接收重组 (server.py)

**3.1 ChunkAssembler 数据结构**

```python
@dataclass
class _ChunkAssembler:
    """Accumulates chunked frames for a single request."""
    total_chunks: int
    route_name: str
    method_idx: int
    chunks: list[bytes | None]
    received: int = 0
    created_at: float = field(default_factory=time.monotonic)
    
    def add(self, idx: int, data: bytes) -> bool:
        """Add a chunk. Returns True when all chunks received."""
        if self.chunks[idx] is not None:
            return False  # duplicate — ignore
        self.chunks[idx] = data
        self.received += 1
        return self.received == self.total_chunks
    
    def assemble(self) -> bytes:
        return b''.join(self.chunks)
```

**3.2 服务端处理流程修改**

```python
# In _handle_client:
chunk_assemblers: dict[int, _ChunkAssembler] = {}

async def _process_frame(self, request_id, flags, payload, conn):
    if flags & FLAG_CHUNKED:
        return await self._process_chunked_frame(
            request_id, flags, payload, conn, chunk_assemblers)
    else:
        return await self._process_normal_frame(...)

async def _process_chunked_frame(self, rid, flags, payload, conn, assemblers):
    # 1. Decode buddy reference + read SHM data
    is_buddy = bool(flags & FLAG_BUDDY)
    if is_buddy:
        seg_idx, offset, data_size, ... = decode_buddy_payload(payload)
        seg_mv = conn.seg_views[seg_idx]
        chunk_data = bytes(seg_mv[offset:offset + data_size])
        conn.buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
    else:
        chunk_data = payload[ctrl_consumed:]  # inline chunk
    
    # 2. Decode chunk header
    chunk_offset = BUDDY_PAYLOAD_SIZE if is_buddy else 0
    chunk_idx, total_chunks = decode_chunk_header(payload, chunk_offset)
    
    # 3. First chunk carries call control
    if chunk_idx == 0:
        ctrl_offset = chunk_offset + CHUNK_HEADER_SIZE
        route, method_idx, _ = decode_call_control(payload, ctrl_offset)
        asm = _ChunkAssembler(total_chunks, route, method_idx,
                               [None] * total_chunks)
        assemblers[rid] = asm
    
    asm = assemblers.get(rid)
    if asm is None:
        logger.warning('Chunk frame for unknown RID %d (chunk %d)', rid, chunk_idx)
        return
    
    # 4. Accumulate
    complete = asm.add(chunk_idx, chunk_data)
    
    if complete:
        full_payload = asm.assemble()
        del assemblers[rid]
        # 5. Dispatch to CRM
        slot = self._resolve_slot(asm.route_name)
        method_name = slot.method_table.name_of(asm.method_idx)
        await self._execute_and_reply(conn, rid, slot, method_name, full_payload, ...)
```

**3.3 Chunk Assembler GC (超时清理)**

```python
_CHUNK_TIMEOUT = 60.0  # 分块序列未完成的超时

async def _gc_chunk_assemblers(self, assemblers: dict):
    """Periodic cleanup of incomplete chunk sequences."""
    now = time.monotonic()
    expired = [rid for rid, asm in assemblers.items()
               if now - asm.created_at > _CHUNK_TIMEOUT]
    for rid in expired:
        logger.warning('Chunk sequence RID=%d timed out (%d/%d received)',
                       rid, assemblers[rid].received, assemblers[rid].total_chunks)
        del assemblers[rid]
```

#### Phase 4: 响应分块 (双向)

服务端的响应也可能超过 segment_size（CRM 返回大数据集）。复用相同的分块机制：

```python
# server.py: _send_chunked_v2_reply()
async def _send_chunked_v2_reply(self, conn, rid, result_bytes, writer):
    chunk_size = self._config.pool_segment_size // 2
    n_chunks = ceil(len(result_bytes) / chunk_size)
    
    for i in range(n_chunks):
        chunk = result_bytes[i * chunk_size : (i + 1) * chunk_size]
        # alloc + write to SHM + send frame
        ...
```

客户端 `_recv_loop` 中对称地实现 chunk 重组。

#### Phase 5: Relay 兼容 (relay.py + Rust relay)

**Python Relay:**
```python
# relay.py: 分块转发
async def handle_call(request: Request) -> StreamingResponse:
    # 方案 1: 完整接收后分块转发到 IPC
    body = await request.body()
    result = relay._do_call_chunked(client, route, method, body)
    return Response(content=result, ...)
    
    # 方案 2 (未来): HTTP chunked streaming → IPC chunked streaming
    # async for chunk in request.stream():
    #     client.send_chunk(rid, chunk)
```

**Rust Relay:** axum 的 `body::Bytes` 目前是完整接收。可通过 `body::Body` stream 接口支持流式接收，但这是 P3 目标。初版仍完整接收后内部分块传输到 IPC。

#### Phase 6: 能力协商 (protocol.py)

Handshake v5 中添加 `CAP_CHUNKED` 能力位:

```python
# 客户端握手请求:
caps = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED

# 服务端检查:
if not (client_caps & CAP_CHUNKED):
    # 老客户端，不支持分块 — 大 payload 走老路径（MemoryPressureError）
```

如果服务端不支持 `CAP_CHUNKED`，客户端 `_call_chunked()` 应降级为抛出 `MemoryPressureError`（保持向后兼容）。

### 改动范围总结

| 文件 | 改动类型 | 改动量 |
|------|---------|--------|
| `rpc_v2/protocol.py` | 新增常量 + CAP | ~10 行 |
| `rpc_v2/wire.py` | 新增 chunk 编解码器 | ~80 行 |
| `rpc_v2/client.py` | `_call_chunked()` + recv chunk | ~120 行 |
| `rpc_v2/server.py` | `_ChunkAssembler` + 处理逻辑 | ~100 行 |
| `rpc_v2/relay.py` | 分块转发适配 | ~30 行 |
| `rpc/ipc/ipc_protocol.py` | (不变 — flag 空间由 protocol.py 管理) | 0 行 |
| `tests/` | 分块单元测试 + 集成测试 + 背压测试 | ~300 行 |
| **总计** | | **~640 行** |

### Roadmap 更新建议

roadmap §2.1 "SHM Pool 动态扩容" 应更新为：

> **§2.1 大 Payload 分块传输 (Chunked Transfer)**
>
> **问题:** payload 超过 `pool_segment_size` 时（默认 256 MB），当前实现无法处理。
>
> **方案:** 多帧分块流式传输 — 在协议层新增 `FLAG_CHUNKED` / `FLAG_CHUNK_LAST`，
> 将大 payload 拆分为多个 buddy-block-sized 的独立帧，每帧携带 chunk 序号和总数。
> 接收方按 RID 重组。详见 `doc/spikes/performance-shm-streaming-large-payload-spike.md`。
>
> **改动范围:** `wire.py` + `protocol.py` + `client.py` + `server.py` + `relay.py` + 测试
>
> **预计:** 3-4d（含 Rust relay 适配 + 完整测试）

§4.2 "Streaming RPC / Pipeline" 的前置条件应更新为:

> **前置依赖:** §2.1 分块传输（提供帧级分块机制） — ~~SHM Pool 动态扩容~~

### Follow-up Actions

- [ ] 确认方案 A 的 chunk_size 策略: `segment_size / 2` vs 固定 128 MB vs 可配置
- [ ] 确认是否在 P1 阶段实现 C (磁盘溢写) 作为 L3 兜底，还是推迟到 P2
- [ ] 设计分块序列超时清理机制的参数（默认 60s 是否合适）
- [ ] 评估 `_send_lock` 每帧获取/释放的性能开销（benchmark）
- [ ] 确认 Rust relay 是否需要在 P1 同步支持分块，还是先只做 Python relay
- [ ] 更新 `doc/plan/c-two-rpc-v2-roadmap.md` 中的 §2.1 和 §4.2
- [ ] 创建实现任务拆分

## Status History

| Date | Status | Notes |
|------|--------|-------|
| 2026-03-28 | 🔴 Not Started | Spike created and scoped |
| 2026-03-28 | 🟡 In Progress | 完成全栈审计 + 三方案对比 + 推荐方案 A 及详细实现路径 |

---

_Last updated: 2026-03-28 by soku + Copilot_
