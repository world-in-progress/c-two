---
title: "IPC v3 控制面 Ring Buffer 替代锁机制可行性研究"
category: "Performance"
status: "🟢 Complete"
priority: "High"
timebox: "1 week"
created: 2026-03-26
updated: 2026-03-26
owner: "soku"
tags: ["technical-spike", "performance", "ipc-v3", "ring-buffer", "lock-free", "research"]
---

# IPC v3 控制面 Ring Buffer 替代锁机制可行性研究

## Summary

**Spike Objective:** 评估是否可以通过将 IPC v3 控制面（control plane）改造为基于 SHM Ring Buffer 的无锁架构，取代当前基于 UDS + threading.Lock 的同步机制，从而进一步提升 RPC 性能。

**Why This Matters:** IPC v3 当前的控制面使用 Unix Domain Socket (UDS) 进行帧传输，并依赖多把 Python `threading.Lock` 来保护共享状态（pending futures、deferred frees、connection map）。虽然数据面已通过 buddy allocator + SHM 实现了接近零拷贝的高吞吐，但控制面的 UDS syscall 开销和锁竞争可能成为小消息场景下的瓶颈。Ring Buffer 有潜力将控制面也迁移到 SHM，消除 UDS 系统调用，并通过无锁原子操作替代互斥锁。

**Timebox:** 1 week

**Decision Deadline:** 研究完成后决定是否进入实施阶段。

## Research Question(s)

**Primary Question:** 当前 IPC v3 控制面中的哪些锁机制可以被 SHM Ring Buffer 取代？替代后能带来多大的性能提升？

**Secondary Questions:**

1. 当前 IPC v3 中有几层锁/同步机制？它们各自保护什么状态？各自的竞争频率如何？
2. Ring Buffer 方案是否可以完全消除 UDS 控制通道，还是只能部分替代？
3. 跨进程 Ring Buffer 的唤醒机制（polling vs eventfd vs futex）在不同 OS 上的兼容性如何？
4. Ring Buffer 方案与当前 asyncio 事件循环模型如何共存？
5. 对于多连接（multi-client）场景，Ring Buffer 如何扩展？

## 当前架构分析：IPC v3 中的锁与同步机制

### 架构总览

IPC v3 采用**控制面/数据面分离**架构：
- **控制面（Control Plane）**: Unix Domain Socket (UDS)，传输 16 字节帧头 + 小 payload 或 buddy 引用（11 字节）
- **数据面（Data Plane）**: SHM buddy allocator，零拷贝传输大消息

```
┌────────────────────────────────────────────────────────────────────┐
│                        IPC v3 架构                                │
├────────────────────────────────────────────────────────────────────┤
│                                                                    │
│  Client (App Thread)                Server (Event Loop Thread)    │
│  ┌──────────────┐                   ┌──────────────────┐          │
│  │ _conn_lock   │◄── UDS Socket ──►│ asyncio loop      │          │
│  │ (Lock #1)    │   (Control Plane) │                  │          │
│  └──────┬───────┘                   │ ┌──────────────┐ │          │
│         │                           │ │ _conn_lock   │ │          │
│         │                           │ │ (Lock #2)    │ │          │
│         │                           │ ├──────────────┤ │          │
│  ┌──────┴───────┐                   │ │_pending_lock │ │          │
│  │ Buddy Pool   │◄── SHM ─────────►│ │ (Lock #3)    │ │          │
│  │ (spinlock)   │   (Data Plane)    │ ├──────────────┤ │          │
│  └──────────────┘                   │ │_deferred_lock│ │          │
│                                     │ │ (Lock #4)    │ │          │
│                                     │ └──────────────┘ │          │
│                                     │                  │          │
│                 Scheduler Thread ───┤  reply()         │          │
│                (cross-thread call)  │  call_soon_ts()  │          │
│                                     └──────────────────┘          │
└────────────────────────────────────────────────────────────────────┘
```

### 锁清单：5 层同步机制详解

#### 锁 #1: Client `_conn_lock` (Python threading.Lock)
- **位置**: `ipc_v3_client.py:83`
- **保护**: Socket 发送/接收的串行化（整个 call/relay 生命周期）
- **竞争**: 低——单连接串行 request-response，除非多线程共享同一 client
- **持锁时间**: 长——覆盖 alloc → sendall → recv_response 全流程
- **Ring Buffer 可替代性**: ✅ 高——Ring Buffer 天然串行化生产者写入

#### 锁 #2: Server `_conn_lock` (Python threading.Lock)  
- **位置**: `ipc_v3_server.py:107`
- **保护**: `_connections` dict（conn_id → BuddyConnection）
- **竞争**: 中——event loop 线程读取，reply() 从 scheduler 线程访问
- **持锁时间**: 短——只在 dict 查找/更新时持有
- **Ring Buffer 可替代性**: ⚠️ 中——连接管理是元操作，不适合 Ring Buffer

#### 锁 #3: Server `_pending_lock` (Python threading.Lock)
- **位置**: `ipc_v3_server.py:112`
- **保护**: `_pending` dict（str_rid → asyncio.Future）
- **竞争**: 高——每个请求都要 put/pop，event loop 和 scheduler 线程竞争
- **持锁时间**: 短——dict 操作
- **Ring Buffer 可替代性**: ✅ 高——可以用 Ring Buffer 的 slot 编号替代 future 映射

#### 锁 #4: Server `_deferred_frees_lock` (Python threading.Lock)
- **位置**: `ipc_v3_server.py:117`
- **保护**: `_deferred_frees` dict（str_rid → buddy block info）
- **竞争**: 高——与 pending_lock 类似，每请求至少两次访问
- **持锁时间**: 短——dict 操作
- **Ring Buffer 可替代性**: ✅ 高——可以嵌入 Ring Buffer slot 元数据

#### 锁 #5: Buddy Allocator SHM Spinlock (Rust AtomicU32)
- **位置**: `rust/c2_buddy/src/spinlock.rs:13`，`allocator.rs:82`
- **保护**: Buddy bitmap 的 alloc/free 操作（跨进程互斥）
- **竞争**: 低——临界区极短（~50ns bitmap 操作）
- **持锁时间**: 极短——纯内存操作，无 syscall
- **Ring Buffer 可替代性**: ❌ 低——buddy allocator 的分裂/合并操作本质上需要互斥

### 当前请求生命周期中的同步开销

```
Client call() 一次请求的锁获取序列:
─────────────────────────────────────────────────
1. [Client] _conn_lock.acquire()           ← Python Lock
2. [Client] buddy_pool.alloc()             ← Rust SHM spinlock
3. [Client] sock.sendall(frame)            ← UDS syscall (write)
4. [Client] sock.recv(header)              ← UDS syscall (read, blocking)
5. [Client] buddy_pool.free_at()           ← Rust SHM spinlock
6. [Client] _conn_lock.release()           ← Python Lock

Server _handle_client() 对应序列:
─────────────────────────────────────────────────
1. [Server/EventLoop] reader.readexactly() ← UDS syscall (read, async)
2. [Server/EventLoop] _conn_lock           ← Python Lock (snapshot buddy_pool)
3. [Server/EventLoop] _deferred_frees_lock ← Python Lock (register deferred)
4. [Server/EventLoop] _pending_lock        ← Python Lock (register future)
5. [Server/EventLoop] event_queue.put()    ← queue.Queue (内部 Lock)
6. [Server/EventLoop] await fut            ← asyncio.Future (等待 scheduler)
--- CRM 执行 (scheduler thread) ---
7. [Server/Scheduler] reply()              
8.    _pending_lock                         ← Python Lock (pop future)
9.    _conn_lock                            ← Python Lock (snapshot conn)
10.   _deferred_frees_lock                  ← Python Lock (pop deferred)
11.   buddy_pool.alloc()                    ← Rust SHM spinlock
12.   buddy_pool.free_at()                  ← Rust SHM spinlock (free request block)
13.   call_soon_threadsafe()                ← asyncio 跨线程唤醒
14. [Server/EventLoop] writer.write(frame)  ← UDS syscall (write)
```

**单次请求总锁获取次数**: ~12 次 Python Lock + 4 次 Rust spinlock + 4 次 UDS syscall

## Ring Buffer 方案设计

### 方案 A: SHM Ring Buffer 完全替代 UDS（激进方案）

#### 核心思路

用一对 SHM Ring Buffer 替代 UDS socket，实现完全基于共享内存的控制面通信：

```
┌────────────────────────────────────────────────────────────────┐
│              方案 A: 双 Ring Buffer 控制面                      │
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  Client                              Server                   │
│  ┌──────────┐   Request Ring (SHM)   ┌──────────┐            │
│  │ Producer  │ ──────────────────────►│ Consumer  │            │
│  └──────────┘                        └──────────┘            │
│  ┌──────────┐   Response Ring (SHM)  ┌──────────┐            │
│  │ Consumer  │ ◄──────────────────────│ Producer  │            │
│  └──────────┘                        └──────────┘            │
│                                                                │
│  Ring Buffer Layout (per ring, in SHM):                        │
│  ┌──────────────────────────────────────────────┐             │
│  │ [head: AtomicU64] [tail: AtomicU64]          │             │
│  │ [capacity: u64]   [entry_size: u32]          │             │
│  │ [wake_flag: AtomicU32]                       │ ← Header    │
│  ├──────────────────────────────────────────────┤             │
│  │ [slot 0] [slot 1] [slot 2] ... [slot N-1]   │ ← Data      │
│  └──────────────────────────────────────────────┘             │
│                                                                │
│  Slot Layout (fixed-size, e.g. 64 bytes):                     │
│  ┌──────────────────────────────────────────────┐             │
│  │ [state: AtomicU32] [request_id: u64]         │             │
│  │ [flags: u32]  [seg_idx: u16] [offset: u32]   │             │
│  │ [data_size: u32] [inline_data: remaining]     │             │
│  └──────────────────────────────────────────────┘             │
└────────────────────────────────────────────────────────────────┘
```

#### 工作流程

```
Client:                                Server:
1. head = ring.head.load(Acquire)      1. (poll loop) tail_local = ring.tail.load(Acquire)
2. write slot[head % cap]              2. if tail_local != head: process slot[tail % cap]
3. ring.head.store(head+1, Release)    3. ring.tail.store(tail+1, Release)
4. if server sleeping:                 4. write response to response_ring
   wake via eventfd/futex              5. if client sleeping: wake
5. spin-wait on response_ring          
6. read response slot                  
```

#### 优势
- **零 syscall 热路径**: 无 UDS read/write，纯内存操作 + 原子指令
- **无 Python Lock**: head/tail 原子操作天然保证 SPSC（单生产者单消费者）顺序
- **Cache-friendly**: 连续内存布局，可利用 CPU 缓存行预取
- **可预测延迟**: 无内核调度抖动

#### 劣势与风险
- **SPSC 限制**: 每个连接需要独立的 Ring Buffer 对，多 client 时 SHM 开销大
- **唤醒机制复杂**:
  - Linux: `eventfd` 或 `futex` wait/wake（高效）
  - macOS: **无 futex**，只能用 `pselect`/`kqueue` 或 busy-poll
  - 跨平台一致性差
- **与 asyncio 冲突**: Server 当前是 asyncio 事件循环，Ring Buffer polling 需要独立线程或将 eventfd 注册到事件循环
- **连接管理复杂化**: 当前 UDS 提供天然的连接语义（accept/close），Ring Buffer 需要自建连接状态机
- **容量固定**: Ring Buffer 满时需要 backpressure 或阻塞
- **实现成本**: 需要在 Rust FFI 层实现 Ring Buffer，Python 端通过 ctypes 操作原子变量

### 方案 B: 保留 UDS，用 Ring Buffer 替代 Server 内部锁（温和方案）

#### 核心思路

保留 UDS 作为控制面传输，但用 SHM Ring Buffer 替代 Server 内部的 `_pending_lock` / `_deferred_frees_lock` 跨线程同步：

```
┌────────────────────────────────────────────────────────────────┐
│          方案 B: UDS 保留 + 内部 Ring Buffer                    │
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  Server Event Loop Thread          Scheduler Thread            │
│  ┌──────────────────┐              ┌──────────────────┐       │
│  │ UDS read frame   │              │ CRM execute      │       │
│  │ decode wire      │              │ build reply      │       │
│  │ enqueue request  │──Request──►  │                  │       │
│  │   to ring buffer │  Ring (SPSC) │                  │       │
│  │                  │              │ enqueue reply    │       │
│  │ dequeue reply   ◄──Reply────── │   to ring buffer │       │
│  │   from ring buf  │  Ring (SPSC) │                  │       │
│  │ writer.write()   │              │                  │       │
│  └──────────────────┘              └──────────────────┘       │
│                                                                │
│  替代效果:                                                      │
│  - _pending_lock ──► Request Ring slot 携带 future ref         │
│  - _deferred_frees_lock ──► Request Ring slot 携带 free info   │
│  - call_soon_threadsafe() ──► Response Ring + eventfd 唤醒     │
└────────────────────────────────────────────────────────────────┘
```

#### 优势
- **保留 UDS 连接语义**: accept/close/多路复用不变，改动小
- **消除高频锁**: `_pending_lock` 和 `_deferred_frees_lock` 每请求各 2 次，替换为无锁 Ring
- **与 asyncio 兼容**: 可以把 Ring Buffer 的 eventfd 注册到事件循环的 `add_reader()`
- **渐进式迁移**: 可以先替换内部锁，稳定后再考虑替换 UDS

#### 劣势
- **仍有 UDS syscall**: 控制面延迟未消除
- **收益有限**: Python Lock 在无竞争时开销仅 ~100ns，SPSC Ring 的原子操作 ~10ns，差距不大
- **复杂度**: 需要管理 Ring 容量、满队列 backpressure

### 方案 C: 混合方案——SHM Ring Buffer 控制面 + 保留 buddy 数据面

#### 核心思路

只替换 UDS → SHM Ring Buffer 用于帧级别通信（16 字节头 + 11 字节 buddy payload = 27 字节），
保留 buddy allocator 用于大消息数据面。这是方案 A 的简化版，针对 SPSC 场景优化。

```
Ring Buffer 帧格式 (per slot, 64 bytes cache-line aligned):
┌────────────────────────────────────────────────────────────────┐
│ Byte 0-3:   sequence (AtomicU32) — slot 状态标记              │
│ Byte 4-7:   flags (u32)                                       │
│ Byte 8-15:  request_id (u64)                                  │
│ Byte 16-17: seg_idx (u16)      ← buddy ref                   │
│ Byte 18-21: offset (u32)       ← buddy ref                   │
│ Byte 22-25: data_size (u32)    ← buddy ref                   │
│ Byte 26:    buddy_flags (u8)                                  │
│ Byte 27-63: inline payload (37 bytes) 或 padding              │
└────────────────────────────────────────────────────────────────┘
```

**SPSC 无锁协议**:
- Producer 写入 slot 数据后，`sequence.store(seq + 1, Release)` 发布
- Consumer 轮询 `sequence.load(Acquire)`，当 seq 匹配时消费
- 无需 CAS，只用 load/store 即可保证 SPSC 正确性

#### 唤醒策略（关键问题）

| 策略 | Linux | macOS | 延迟 | CPU 占用 |
|------|-------|-------|------|----------|
| Busy-poll (spin) | ✅ | ✅ | ~50ns | 100% 一个核 |
| eventfd + epoll | ✅ | ❌ | ~1-3μs | 低 |
| kqueue pipe | ❌ | ✅ | ~1-3μs | 低 |
| futex | ✅ | ❌ (无原生支持) | ~1μs | 低 |
| Adaptive (spin → yield → sleep) | ✅ | ✅ | 50ns-10μs | 自适应 |
| pthread_cond on SHM | ✅ | ✅ | ~2μs | 低 |

**推荐**: Adaptive spin + OS-specific fallback

## 量化分析：各环节延迟分解

### 当前 IPC v3 延迟组成（基于已有 benchmark 数据）

```
延迟组成（64B 小消息, echo 场景，估算值）:
─────────────────────────────────────────────────
UDS write (client → server):     ~0.5-1.0 μs   (sendall syscall)
UDS read  (server ← client):     ~0.5-1.0 μs   (readexactly syscall)  
Python Lock acquire/release ×12:  ~1.2 μs       (~100ns each, uncontended)
Rust spinlock ×4:                 ~0.2 μs       (~50ns each)
asyncio future create+resolve:    ~0.5-1.0 μs
call_soon_threadsafe:             ~1.0-2.0 μs   (pipe write + epoll wake)
Wire decode/encode:               ~0.2-0.5 μs
event_queue.put/get:              ~0.5 μs       (queue.Queue 内部 Lock)
UDS write (server → client):     ~0.5-1.0 μs
UDS read  (client ← server):     ~0.5-1.0 μs
─────────────────────────────────────────────────
总控制面开销估算:                  ~5-9 μs
CRM 函数执行:                     (应用依赖)
```

### Ring Buffer 方案 A 预期延迟（乐观估计）

```
Ring Buffer 方案延迟（64B 小消息，SPSC）:
─────────────────────────────────────────────────
SHM Ring write (client → server): ~0.05 μs  (store + atomic release)
SHM Ring read  (server ← client): ~0.05 μs  (load + atomic acquire)
唤醒 (adaptive spin hit):         ~0.05-0.1 μs
Wire decode/encode:               ~0.2-0.5 μs
Rust spinlock ×4:                 ~0.2 μs    (buddy alloc 不变)
SHM Ring write (server → client): ~0.05 μs
SHM Ring read  (client ← server): ~0.05 μs
─────────────────────────────────────────────────
总控制面开销估算:                  ~0.7-1.0 μs
```

### 潜在加速比

| 消息大小 | 当前 P50 (估) | Ring Buffer P50 (估) | 加速比 | 备注 |
|----------|--------------|---------------------|--------|------|
| 64B | ~8 μs | ~1 μs | ~8× | 控制面主导 |
| 4KB | ~12 μs | ~5 μs | ~2.4× | 开始 SHM 拷贝 |
| 1MB | ~400 μs | ~395 μs | ~1% | 数据面主导 |
| 100MB | ~3 ms | ~3 ms | ~0% | 完全数据面主导 |

**关键洞察**: Ring Buffer 对小消息（< 4KB）有显著加速，但对大消息（数据面主导）几乎无影响。

## 技术难点与风险评估

### 难点 1: macOS 缺乏高效跨进程唤醒原语

macOS 不支持 `futex`，`eventfd` 也不可用。可选方案：
- **Mach semaphore**: `semaphore_create`/`semaphore_signal`/`semaphore_wait` — 可跨进程，但需要 Mach port 传递
- **POSIX 共享信号量**: `sem_open` — macOS 支持但 `sem_timedwait` 不可用
- **kqueue + pipe**: 可行但引入 fd，回到类 UDS 模式
- **Busy-poll with yield**: 最简单但浪费 CPU
- **`__ulock_wait`/`__ulock_wake`**: macOS 私有 API，等价于 futex，但非公开

**评估**: macOS 是 C-Two 的重要开发平台（ARM64 测试环境）。缺乏 futex 是一个重大障碍。
如果用 busy-poll，小消息的延迟收益被 CPU 浪费抵消；如果用 pipe/kqueue，则回退到 fd-based 唤醒，
与 UDS 方案的 syscall 开销相当，Ring Buffer 的核心优势丧失。

### 难点 2: Python GIL / free-threading 下的原子操作

C-Two 目标支持 Python 3.14t (free-threading)。关键问题：
- Python `ctypes` 不能直接执行 atomic load/store with memory ordering
- 必须通过 Rust FFI（c2_buddy）暴露 Ring Buffer 的原子操作
- 每次 Ring Buffer 操作都是 FFI 调用（PyO3 开销 ~50-100ns）
- 在 free-threading 模式下，Python 线程间的 Ring Buffer 访问需要确保内存可见性

**评估**: FFI 调用开销可能抵消 Ring Buffer 相比 Python Lock 的优势（Lock ~100ns vs FFI ~50-100ns）。

### 难点 3: 与 asyncio 事件循环的集成

当前 Server 使用 asyncio `start_unix_server`，UDS 读写与事件循环天然集成。
改为 Ring Buffer 后：
- 选项 1: 独立 polling 线程 → 需要新的跨线程通知机制
- 选项 2: 周期性 polling task → 增加延迟（至少一个 asyncio 循环周期 ~10-100μs）
- 选项 3: eventfd + `add_reader()` → 仅 Linux，回到 fd-based
- 选项 4: 放弃 asyncio，用纯线程 → 重写 Server 架构

**评估**: 无论哪个选项，都会显著增加架构复杂度或在某些平台上回退到 fd-based 唤醒。

### 难点 4: 多 Client 连接扩展

当前 UDS server 天然支持多 client（accept → 独立 StreamReader/Writer）。
Ring Buffer SPSC 语义要求每个连接一对 Ring Buffer：
- N clients → 2N Ring Buffers → 2N × ring_size SHM
- Server 需要同时 poll 多个 Ring Buffer（MPSC 或 per-client polling）
- 连接建立/销毁需要额外的控制通道（又回到 UDS？）

**评估**: 多连接场景下 Ring Buffer 的 SHM 开销和 polling 复杂度远超 UDS epoll。

### 难点 5: Buddy Allocator 的 spinlock 无法被 Ring Buffer 替代

Buddy allocator 的 alloc/free 涉及 bitmap 扫描、块分裂、伙伴合并——
这些是**读-修改-写**的复合操作，不能表达为 SPSC 队列操作。
spinlock 临界区极短（~50ns），且已经是性能最优的选择。

**评估**: 即使控制面完全改为 Ring Buffer，buddy spinlock 仍然存在。
这意味着 Ring Buffer 只能替代 Python 层的锁，无法消除所有锁。

## 替代方案对比

### 方案 D: 优化现有架构（推荐的替代方案）

与其引入 Ring Buffer 的全新架构，不如优化当前 UDS + Lock 架构的瓶颈：

#### D1: 合并 pending_lock 和 deferred_frees_lock

```python
# 当前: 两把独立锁，每请求各 2 次 = 4 次锁操作
with self._pending_lock:
    self._pending[str_rid] = fut
with self._deferred_frees_lock:
    self._deferred_frees[str_rid] = (...)

# 优化: 合并为一把锁 _request_lock，减少到 2 次
with self._request_lock:
    self._pending[str_rid] = fut
    self._deferred_frees[str_rid] = (...)
```

**预期收益**: 每请求减少 2 次 Lock acquire/release（~200ns）

#### D2: 用 slot array 替代 dict + lock

```python
# 当前: dict + lock (哈希 + 锁开销)
# 优化: 固定大小 slot array，request_id 作为 index
_MAX_PENDING = 1024
_pending_slots: list[asyncio.Future | None] = [None] * _MAX_PENDING
_deferred_slots: list[tuple | None] = [None] * _MAX_PENDING

# 无锁写入（单写者场景）：
_pending_slots[request_id % _MAX_PENDING] = fut
# 无锁读取（单读者场景）：
fut = _pending_slots[request_id % _MAX_PENDING]
```

**前提**: 需要确保 event loop 线程和 scheduler 线程对同一 slot 的访问满足 SWSR（单写单读）。
对于当前的 serial request-response per connection 模型，这是成立的。

**预期收益**: 消除 pending_lock 和 deferred_frees_lock 的全部开销（~400ns/req）

#### D3: 用 `os.eventfd` (Linux) 替代 `call_soon_threadsafe`

```python
# 当前: call_soon_threadsafe 经过 asyncio self-pipe trick (~1-2μs)
loop.call_soon_threadsafe(self._resolve_future, fut, frame)

# 优化 (Linux 3.14+): eventfd 直接注册到事件循环
efd = os.eventfd(0)
loop.add_reader(efd, self._drain_replies)
# scheduler 线程: 写入 reply ring + eventfd_write
os.eventfd_write(efd, 1)
```

**预期收益**: 减少 ~0.5-1μs 的 asyncio self-pipe 开销

#### D4: 批量帧发送（减少 UDS syscall）

当 server 有多个 pending reply 时，合并到一次 `writev()` 调用：

```python
# 当前: 每个 reply 一次 writer.write()
writer.write(response_frame)

# 优化: 批量 drain
frames = self._drain_reply_queue()  # 一次性取所有
writer.writelines(frames)           # writev syscall
await writer.drain()
```

**预期收益**: 高并发时减少 syscall 次数

### 方案对比总结

| 维度 | 方案 A (全 Ring Buffer) | 方案 B (内部 Ring) | 方案 C (混合) | 方案 D (优化现有) |
|------|----------------------|-------------------|-------------|-----------------|
| 小消息加速 | ~8× | ~1.5× | ~4× | ~1.5-2× |
| 大消息加速 | ~0% | ~0% | ~0% | ~0% |
| 实现复杂度 | 极高 | 高 | 高 | 低 |
| macOS 兼容 | ⚠️ 差 | ✅ 好 | ⚠️ 差 | ✅ 好 |
| asyncio 兼容 | ❌ 需重写 | ✅ 可集成 | ⚠️ 困难 | ✅ 无改动 |
| 多 client | ⚠️ 复杂 | ✅ 不影响 | ⚠️ 复杂 | ✅ 不影响 |
| 回归风险 | 高 | 中 | 高 | 低 |
| Buddy 锁消除 | ❌ | ❌ | ❌ | ❌ |
| 投入产出比 | 低 | 中 | 低 | **高** |

## Decision

### Recommendation

**暂不采用 Ring Buffer 方案。推荐方案 D（优化现有架构）作为短期改进路径。**

Ring Buffer 改造控制面的理论收益主要集中在小消息场景（< 4KB），但面临以下不可忽视的障碍：

1. **macOS 兼容性缺陷**: 无 futex/eventfd 使得高效唤醒无法跨平台统一实现
2. **Buddy spinlock 不可替代**: 数据面的核心同步原语无法被 Ring Buffer 取代
3. **asyncio 集成困难**: 需要重写 Server 架构或引入额外线程
4. **Python FFI 开销**: 通过 Rust FFI 操作原子变量的开销可能抵消 Ring Buffer 优势
5. **投入产出比不理想**: 大消息（C-Two 的核心场景：科学计算数据传输）完全不受益

### Rationale

IPC v3 的性能瓶颈主要在**数据面**（序列化 / SHM 拷贝），而非控制面。
已有 benchmark 数据显示：
- 10MB: P50 = 0.4ms (24GB/s) — 接近内存带宽极限
- 100MB: P50 = 3ms (32GB/s) — 已非常高效
- 1GB: P50 = 27ms (37GB/s) — pickle 序列化是主要开销

控制面的 ~5-9μs 开销在大消息场景中可忽略不计（<1%）。
只有在高频小消息 RPC 场景下（类似 Redis），Ring Buffer 才有显著价值，
但这不是 C-Two 的核心使用场景（分布式科学计算，大 payload）。

方案 D 的优化（合并锁、slot array、eventfd）可以用极低的实现成本获得 ~30-50% 的小消息延迟改善，同时保持架构稳定性和跨平台兼容性。

### Implementation Notes

如果未来决定推进 Ring Buffer 方案，建议的实施路径：

1. **Phase 0（当前推荐）**: 实施方案 D 优化，建立小消息 benchmark 基线
2. **Phase 1**: 在 Rust c2_buddy 中实现 SPSC Ring Buffer，纯 Rust 单元测试验证
3. **Phase 2**: 实现 Linux-only 的 eventfd 唤醒 + Ring Buffer 控制面原型
4. **Phase 3**: 对比 benchmark，决定是否继续
5. **Phase 4**: 解决 macOS 兼容性（adaptive spin + `__ulock` 私有 API）

### Follow-up Actions

- [ ] 实施方案 D1: 合并 `_pending_lock` 和 `_deferred_frees_lock`
- [ ] 实施方案 D2: 评估 slot array 替代 dict + lock 的可行性
- [ ] 建立小消息（64B-4KB）latency benchmark，量化控制面开销占比
- [ ] 如果小消息性能成为实际瓶颈，重新评估 Ring Buffer 方案

## Investigation Plan

### Research Tasks

- [x] 分析 IPC v3 当前的所有锁/同步机制及其竞争频率
- [x] 设计 Ring Buffer 替代方案（A/B/C 三种粒度）
- [x] 评估跨平台唤醒机制（Linux vs macOS）
- [x] 量化分析各环节延迟贡献
- [x] 评估与 asyncio 事件循环的集成难度
- [x] 对比 Ring Buffer vs 优化现有架构的投入产出比
- [ ] 建立小消息 benchmark 验证估算值

### Success Criteria

- [x] 明确当前控制面的锁机制和延迟瓶颈
- [x] 给出是否采用 Ring Buffer 的明确建议
- [x] 如果不采用，给出替代优化方案
- [ ] Proof of concept 验证替代方案（方案 D）的收益

## External Resources

- [Linux futex(2) man page](https://man7.org/linux/man-pages/man2/futex.2.html) — futex 语义
- [io_uring 作为 IPC 通道的探索](https://unixism.net/loti/) — io_uring 在控制面的潜力
- [Disruptor pattern (LMAX)](https://lmax-exchange.github.io/disruptor/) — 高性能 Ring Buffer 设计模式
- [folly::ProducerConsumerQueue](https://github.com/facebook/folly/blob/main/folly/ProducerConsumerQueue.h) — Facebook SPSC 无锁队列
- [iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) — C-Two 已进行过的 iceoryx2 spike（zero-copy IPC）

## Status History

| Date | Status | Notes |
|------|--------|-------|
| 2026-03-26 | 🔴 Not Started | Spike created and scoped |
| 2026-03-26 | 🟡 In Progress | 完成架构分析和方案设计 |
| 2026-03-26 | 🟢 Complete | 结论：暂不采用 Ring Buffer，推荐优化现有架构（方案 D） |

---

_Last updated: 2026-03-26 by soku_
