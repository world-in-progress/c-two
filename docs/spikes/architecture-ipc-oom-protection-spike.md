---
title: "IPC v3 OOM Protection — Inline Fallback 的局限性与工业级内存压力防护方案"
category: "Architecture"
status: "✅ Complete (superseded by Rust transport sink)"
priority: "High"
timebox: "1 week"
created: 2026-03-27
updated: 2026-03-27
owner: "soku"
tags: ["technical-spike", "architecture", "oom-protection", "ipc-v3", "memory-pressure"]
---

# IPC v3 OOM Protection — Inline Fallback 的局限性与工业级内存压力防护方案

## Summary

**Spike Objective:** Inline fallback 能否彻底解决 buddy pool 段分配失败导致的 OOM 问题？如果不能，需要设计什么样的多层防护机制？

**Why This Matters:** 当前 IPC v3 架构中，buddy pool 段是即时物理提交的（`ftruncate` 即分配）。当系统内存不足以开辟新 segment 时：
- **Server 侧**已有 inline fallback（优雅降级）
- **Client 侧**无 fallback（硬崩溃 → `CompoClientError` + 断连）
- **Inline 本身有 16MB 硬上限**（`max_frame_size`），超大 payload 走 inline 会撑爆 UDS socket

这意味着 inline fallback **只能缓解小 payload 碰巧需要新 segment 的场景**，对于真正的大 payload（科学计算中的 50MB–1GB 数组），inline 不是解决方案，而是另一个故障源。

**Timebox:** 1 week

**Decision Deadline:** 在 rpc_v2 模块稳定化之前必须确定防护策略

## Research Question(s)

**Primary Question:** 当 buddy pool 无法分配新 segment（内存不足）时，系统应如何优雅降级而非崩溃？

**Secondary Questions:**

- Inline fallback 在什么条件下有效？什么条件下反而有害？
- 工业界的 IPC/RPC 系统如何处理类似的内存压力场景？
- 是否需要引入分块传输（chunked streaming）来处理超大 payload？
- 是否需要 spill-to-disk 机制作为最后兜底？
- 如何在不引入过多复杂度的前提下构建多层防护？

## Investigation Plan

### Research Tasks

- [x] 分析当前 IPC v3 中 alloc 失败的完整传播链路（client / server 不对称行为）
- [x] 分析 inline fallback 的有效边界（max_frame_size = 16MB）
- [x] 调研 gRPC flow control / BDP 窗口机制
- [x] 调研 RDMA 内存注册压力处理策略
- [x] 调研 io_uring ring buffer 背压机制
- [x] 调研 ZeroMQ HWM（高水位线）机制
- [x] 调研 Apache Arrow Flight 大数据集传输策略
- [x] 调研 DPDK/SPDK 内存池耗尽处理
- [x] 调研 Plasma Store（Arrow）共享内存对象存储
- [x] 调研 POSIX SHM + mmap 内存提交语义（Linux vs macOS）
- [x] 调研 HTTP/2 / QUIC 分块流式传输
- [x] 综合分析并给出分层防护建议

### Success Criteria

**This spike is complete when:**

- [x] 明确 inline fallback 的适用边界和失效场景
- [x] 梳理出 ≥3 种工业级方案并评估适用性
- [x] 给出分层（L0–L3）防护建议及优先级
- [ ] 确认推荐方案的可行性（prototype 验证，如需要）

## Technical Context

**Related Components:**

| 组件 | 文件 | 角色 |
|------|------|------|
| Buddy Pool (Rust) | `rust/c2_buddy/src/pool.rs` | 段分配器，3 层 fallback：existing → new segment → dedicated |
| FFI 层 | `rust/c2_buddy/src/ffi.rs:216-223` | `alloc()` 失败 → `PyRuntimeError` |
| IPC v3 Client | `src/c_two/rpc/ipc/ipc_v3_client.py:268` | `alloc()` 失败 → 无 try-except → **硬崩溃** |
| IPC v3 Server | `src/c_two/rpc/ipc/ipc_v3_server.py:396-454` | `alloc()` 失败 → 两次 retry → inline fallback |
| Wire 帧格式 | `src/c_two/rpc/ipc/ipc_protocol.py:20-28` | `FLAG_BUDDY`, 无 streaming flag |
| IPCConfig | `src/c_two/rpc/ipc/ipc_protocol.py:65-130` | `max_frame_size=16MB`, `pool_segment_size=256MB` |
| SOTA Registry | `src/c_two/rpc_v2/registry.py:353-400` | `_make_ipc_config()` + 物理内存检测 |

**Dependencies:** 
- rpc_v2 模块稳定化依赖本 spike 的结论
- `cc.set_ipc_config()` 的参数设计需要考虑防护策略

**Constraints:**
- 必须兼容 macOS + Linux（`madvise` 行为不同）
- 不能引入新的外部依赖（如 Arrow、psutil）
- 热路径性能不能退化（当前 10MB P50=1.5ms）
- Python 3.14t free-threading 兼容

## Research Findings

### 1. Inline Fallback 的精确有效边界

**结论：Inline 只能覆盖 payload < 16MB 且 buddy pool 恰好段满的窄场景。**

当前 IPC v3 的 inline fallback 机制：

```
alloc() 成功但 is_dedicated=True → 立即 free → 编码为 inline frame → sendall()
alloc() 失败（异常）→ Server: catch + retry + inline fallback
                     → Client: 无 catch → CompoClientError → 断连
```

**有效条件（三者必须同时满足）：**
1. `payload_wire_size ≤ max_frame_size`（默认 16MB）
2. alloc 失败发生在 Server 侧（有 try-except）
3. UDS socket buffer 能容纳 inline frame

**失效场景：**

| 场景 | payload | 结果 |
|------|---------|------|
| 科学数组 50MB | > 16MB | inline 编码后 frame 超过 `max_frame_size`，被拒绝 |
| 科学数组 500MB | >> 16MB | 即使绕过 frame 检查，UDS sendall 会阻塞/超时 |
| Client 侧 alloc 失败 | 任意大小 | `RuntimeError` 直接冒泡为 `CompoClientError`，连接关闭 |
| 多 client 并发 | 累积 > segment | 第一个拿到 segment 的 client OK，后续 client 全崩 |

**关键数字：**
- `max_frame_size` = 16MB（`ipc_protocol.py:47`）
- `shm_threshold` = 4KB — payload > 4KB 就走 SHM 路径
- UDS `SO_SNDBUF` 通常 128KB（Linux）/ 变化（macOS）
- 即使提高 `max_frame_size`，UDS 不适合传输大 payload（内核缓冲区拷贝 + 上下文切换开销）

### 2. SHM 内存提交语义

**`shm_open` + `ftruncate` + `mmap` 的实际内存行为：**

| 阶段 | Linux (overcommit=1, 默认) | macOS |
|------|---------------------------|-------|
| `shm_open()` | 创建 inode，无物理页 | 创建 inode |
| `ftruncate(256MB)` | 扩展 inode，**不分配物理页** | 部分提交 |
| `mmap()` | 保留虚拟地址空间，**不分配物理页** | 保留 VA |
| 首次写入页 X | **Page fault → 分配物理页** | Page fault → 分配 |

**Linux 关键行为：**
- 默认 overcommit=1，可以 mmap 100GB 即使只有 8GB 物理 RAM
- 所有页面被写入时触发 **OOM Killer**（不是 alloc 返回错误）
- buddy allocator 初始化时会写入 metadata 页（`BuddyAllocator::init()`），这会触发部分物理页分配
- 但 **数据区域是惰性的** — 只有实际写入 payload 时才提交

**macOS 关键行为：**
- 更保守的 overcommit — `mmap()` 可能直接返回 `ENOMEM`
- 这意味着在 macOS 上，`create_segment()` 的 `mmap` 失败是合理的早期检测点

**结论：在 Linux 上，buddy pool 的 `create_segment()` 几乎不会因内存不足而失败（overcommit 允许），真正的 OOM 发生在写入时被 OOM Killer 杀死。这使得"拒绝分配"策略在 Linux 上需要主动检测内存压力，而非等待 OS 报错。**

### 3. 工业界方案调研摘要

调研了 10 个工业级系统的内存压力处理策略，按适用性排序：

#### Tier 1：高度适用（直接可借鉴）

**ZeroMQ HWM（高水位线）— 字节级背压**
- 核心机制：发送队列达到 HWM → `send()` 阻塞（blocking 模式）或返回 `EAGAIN`（non-blocking）
- 适用点：buddy pool 的 `total_bytes - free_bytes` = 已用字节数，天然对应 HWM
- 改造：实现**软限（70%，advisory warning）** + **硬限（100%，阻塞等待或报错）**
- C-Two 优势：`pool.stats()` 已经暴露了 `total_bytes` 和 `free_bytes`（`pool.rs:245-278`）

**HTTP/2 分块流式传输 — Chunked Streaming**
- 核心机制：大 payload 拆为多个 frame（默认 16KB–16MB），配合 flow window 逐块发送
- 适用点：解决 >256MB payload 超出单 segment 容量的根本问题
- 改造：新增 `FRAME_CHUNK_START / CHUNK_DATA / CHUNK_END / WINDOW_UPDATE` 帧类型
- 关键优势：**payload 不再受限于 segment_size**，理论上可传任意大小

**DPDK/SPDK 池耗尽检测 — 显式可用性查询**
- 核心机制：`rte_mempool_avail_count()` 显式查询可用缓冲区数量，由应用层决策
- 适用点：在 `alloc()` 之前检查 `pool.stats().free_bytes >= needed_size`
- 改造：Rust FFI 已暴露 `stats()` → Python 可在 alloc 前做 pre-check

#### Tier 2：部分适用（概念借鉴）

**RDMA 内存注册缓存 — SHM LRU**
- 核心机制：NIC 注册表 >80% 时 LRU 淘汰最久未用的注册区域
- 适用点：多连接场景下 SHM segment 的 LRU 回收
- 限制：C-Two 当前以同步 RPC 为主，连接数有限（2-10），LRU 收益不大

**Plasma Store 引用计数 + 磁盘溢出**
- 核心机制：对象引用计数 → 计数归零 + 内存 >90% 时溢出到磁盘
- 适用点：spill-to-disk 作为最后兜底
- 限制：需要额外的 daemon 进程，架构改动大

**gRPC Flow Control — Credit 窗口**
- 核心机制：接收方发放 credit，发送方持有 credit 才能发送
- 适用点：async server 场景下的背压
- 限制：C-Two 是同步 RPC，sender 已经阻塞等待 reply，无需窗口控制

#### Tier 3：不直接适用

**io_uring** — 需要 polling server 重写，Linux-only
**TCP Flow Control** — 需要 async 模型，当前同步 RPC 不需要

### 4. Client/Server 不对称问题详细分析

当前 alloc 失败传播链的完整追踪：

```
                    ┌─── Rust pool.alloc() ───┐
                    │  Layer 1: existing segs  │
                    │  Layer 2: new segment    │ ← 内存不足在此失败
                    │  Layer 3: dedicated seg  │ ← 也可能失败
                    └──────────┬───────────────┘
                               │ Err(String)
                    ┌──────────▼───────────────┐
                    │  FFI: PyRuntimeError     │
                    └──────────┬───────────────┘
                               │
              ┌────────────────┴────────────────┐
              │                                 │
     ┌────────▼─────────┐            ┌──────────▼──────────┐
     │   Client 侧      │            │   Server 侧         │
     │  ipc_v3_client    │            │  ipc_v3_server       │
     │  :268             │            │  :396-444            │
     │                   │            │                      │
     │  ❌ 无 try-except │            │  ✅ try-except       │
     │  RuntimeError     │            │  → 第1次失败: retry   │
     │  冒泡到 :297      │            │  → 第2次失败: warn    │
     │  catch Exception  │            │  → frame = None      │
     │  → close_conn()   │            │  → inline fallback   │
     │  → CompoClient    │            │  → encode_inline_    │
     │     Error         │            │     reply_frame()    │
     └──────────────────┘            └──────────────────────┘
              │                                 │
     连接关闭，调用者收到              Client 正常收到 reply
     异常，CRM 不可用                 （但仅当 reply ≤ 16MB）
```

**核心不对称：**
- Server 有两次 retry + inline fallback → 对 ≤16MB reply 优雅降级
- Client 零容错 → 任意大小 request 的 alloc 失败都是致命的

## Decision

### Recommendation: 四层防护体系（L0–L3）

不应依赖单一机制。推荐构建分层防护，每层覆盖不同的故障场景：

```
┌─────────────────────────────────────────────────────────────────┐
│  L0: 配置时预警                                                  │
│  cc.set_ipc_config() 阶段检测物理内存                            │
│  segment_size > RAM → Error                                      │
│  segment_size × max_segments > 75% RAM → Warning                 │
│  状态：✅ 已实现 (registry.py:_make_ipc_config)                  │
├─────────────────────────────────────────────────────────────────┤
│  L1: 运行时 pre-check（alloc 前检查）                            │
│  在 buddy pool alloc() 之前查询 pool.stats()                     │
│  如果 free_bytes < needed_size 且已达 max_segments → 不尝试分配   │
│  直接走降级路径（而非让 Rust 层失败后再 catch）                    │
│  复杂度：低（~50 行 Python）                                     │
├─────────────────────────────────────────────────────────────────┤
│  L2: 传输降级（分场景处理 alloc 失败后的行为）                    │
│                                                                  │
│  ┌─ payload ≤ 16MB ─→ inline fallback（client + server 都补全）  │
│  │  Client 侧补上 try-except → encode_inline_call_frame          │
│  │                                                               │
│  ├─ payload > 16MB ─→ SHM chunked streaming（新机制）            │
│  │  将大 payload 拆为 ≤segment_size 的 chunk                     │
│  │  复用 buddy pool 中已有的空闲空间逐块传输                      │
│  │  需要新增帧类型：CHUNK_START / CHUNK_DATA / CHUNK_ACK         │
│  │                                                               │
│  └─ payload > 16MB 且 pool 完全满 ─→ 报错 MemoryPressureError   │
│     明确告知调用方"当前内存不足以传输该 payload"                   │
│     调用方可选择：降低 payload 大小 / 等待重试 / 换用更大 pool     │
│                                                                  │
│  复杂度：inline 补全低（~30 行），chunked streaming 高（~500 行） │
├─────────────────────────────────────────────────────────────────┤
│  L3: 系统级防护（长期演进）                                      │
│  - madvise(MADV_DONTNEED) 回收已完成请求的 SHM 页                │
│  - 监控 /proc/self/status VmRSS（Linux）检测实际内存压力          │
│  - Spill-to-disk：pool 满时溢出到 tmpfile + mmap（最后手段）      │
│  复杂度：高（~800 行 + 跨平台适配）                              │
└─────────────────────────────────────────────────────────────────┘
```

### Rationale

**为什么不能只靠 inline fallback：**

1. **16MB 硬顶**：科学计算 payload 常在 50MB–1GB，inline 无法承载
2. **UDS 不适合大数据传输**：内核缓冲区拷贝 + 上下文切换，性能远差于 SHM 零拷贝
3. **Client 侧未覆盖**：当前只有 Server 有 fallback，Client alloc 失败直接崩

**为什么选择分层而非单一方案：**

| 单一方案 | 问题 |
|---------|------|
| 只做 pre-check | 无法处理 alloc 后的竞态（多线程并发分配） |
| 只做 inline fallback | 大 payload 无法覆盖 |
| 只做 chunked streaming | 实现复杂，小 payload 场景 overhead 不必要 |
| 只做 spill-to-disk | 性能代价大，且需跨平台适配 |

**分层的好处：**
- L0 + L1 覆盖 90% 的场景（配置错误 + 可预见的内存不足），实现成本最低
- L2 覆盖运行时实际故障，inline 补全是最小改动
- Chunked streaming 是长期必须的能力（解除 payload ≤ segment_size 的根本限制）
- L3 是锦上添花，可以后续迭代

### Implementation Notes

#### L1 Pre-check 实现思路

```python
# ipc_v3_client.py — alloc 前检查
stats = self._buddy_pool.stats()
can_alloc = (
    stats.free_bytes >= wire_size
    or len(self._buddy_pool.segments) < self._config.max_pool_segments
)
if not can_alloc:
    if wire_size <= self._config.max_frame_size:
        # 走 inline
        frame = encode_inline_call_frame(...)
    else:
        raise MemoryPressureError(
            f'Cannot allocate {wire_size} bytes: pool exhausted '
            f'({stats.free_bytes} free / {stats.total_bytes} total), '
            f'and payload exceeds inline limit ({self._config.max_frame_size})'
        )
```

#### L2 Client Inline Fallback 补全

```python
# ipc_v3_client.py:268 — 当前无 try-except，补上：
try:
    alloc = self._buddy_pool.alloc(wire_size)
    if alloc.is_dedicated:
        # ... existing dedicated fallback ...
except Exception:
    if wire_size <= self._config.max_frame_size - 16:
        # inline fallback (同 server 侧逻辑)
        inline_args = b''.join(args) if isinstance(args, (list, tuple)) else args
        frame = encode_inline_call_frame(
            request_id, method_name, inline_args, get_call_header_cache(),
        )
        sock.sendall(frame)
    else:
        raise MemoryPressureError(...) from None
```

#### L2 Chunked Streaming 概念设计

```
新帧类型（追加到 ipc_v3_protocol.py）：
  FLAG_CHUNK_START  = 1 << 7   # 开始分块传输
  FLAG_CHUNK_DATA   = 1 << 8   # 分块数据
  FLAG_CHUNK_ACK    = 1 << 9   # 接收方确认，可发送下一块

协议流程：
  Client                          Server
    │                               │
    │── CHUNK_START(total_size) ──→ │  "我要发 500MB，会分块"
    │                               │
    │── CHUNK_DATA(chunk_0) ──────→ │  chunk_0 写入 buddy block
    │                               │
    │←── CHUNK_ACK ────────────────│  "已读取 chunk_0，block 已释放"
    │                               │
    │── CHUNK_DATA(chunk_1) ──────→ │  复用同一 buddy block
    │                               │
    │←── CHUNK_ACK ────────────────│
    │         ...                   │
    │── CHUNK_DATA(chunk_N, LAST) → │  最后一块
    │                               │
    │←── REPLY ────────────────────│  CRM 执行完毕，返回结果

关键：每个 chunk 复用 buddy pool 中的同一块空间，
     所以即使 segment_size=256MB 也能传 1GB payload。
     代价：多了 N 次 UDS 控制消息往返（每次 ~10μs）。
     500MB / 256MB chunk = 2 次往返 ≈ 20μs 额外延迟（可忽略）。
```

### Follow-up Actions

- [ ] **P0 — L1 实现**：在 `_make_ipc_config` 和 client/server alloc 前加 pre-check
- [ ] **P0 — L2 Client inline 补全**：给 client 的 `alloc()` 调用补上 try-except + inline fallback
- [ ] **P0 — 新增 `MemoryPressureError`**：在 `c_two/error.py` 中定义，提供清晰的错误信息
- [ ] **P1 — L2 Chunked streaming 设计文档**：详细的帧格式 + 状态机 + 错误处理
- [ ] **P1 — L2 Chunked streaming 实现**：预计 ~500 行 Python + ~100 行 Rust
- [ ] **P2 — L3 madvise 集成**：连接关闭时调用 `madvise(MADV_DONTNEED)` 回收页面
- [ ] **P2 — L3 VmRSS 监控**：定期读取 `/proc/self/status` 检测实际内存压力
- [ ] 更新 `copilot-instructions.md` 中的 OOM 防护说明

## External Resources

- [gRPC Flow Control](https://grpc.io/docs/guides/flow-control/) — 窗口流控机制
- [DPDK Mempool Library](https://doc.dpdk.org/guides/prog_guide/mempool_lib.html) — 固定大小内存池管理
- [Arrow Flight RPC](https://arrow.apache.org/docs/format/Flight.html) — 流式大数据集传输
- [POSIX SHM Overview](https://man7.org/linux/man-pages/man7/shm_overview.7.html) — SHM 生命周期语义
- Linux `vm.overcommit_memory` — [kernel doc](https://www.kernel.org/doc/Documentation/vm/overcommit-accounting)
- ZeroMQ HWM — [zmq_setsockopt(ZMQ_SNDHWM)](https://libzmq.readthedocs.io/en/latest/zmq_setsockopt.html)

## Status History

| Date | Status | Notes |
| ---- | ------ | ----- |
| 2026-03-27 | 🔴 Not Started | Spike created and scoped |
| 2026-03-27 | 🟡 In Progress | Research commenced — 10 industrial systems analyzed |
| 2026-03-27 | 🟡 In Progress | Findings complete, L0–L3 recommendation documented |

---

_Last updated: 2026-03-27 by soku_
