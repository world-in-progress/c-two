# C-Two 演进历程 · NotebookLM 汇报稿

> 本文档为汇报用材料，面向 NotebookLM 生成 PPT。章节按「一节 = 一张或一组幻灯片」组织。
> 覆盖：起点与动机 → 性能优化 → SOTA API → Rust 内核重构 → 分布式 Relay 发现 → 未来。
> 所有数据源自 `docs/` 下的实验日志、spike、设计 spec 与基准报告。

---

## 目录

1. [起点：为什么要做 C-Two](#1-起点为什么要做-c-two)
2. [老 C-Two 的局限](#2-老-c-two-的局限)
3. [性能优化主线：IPC v2 → IPC v3](#3-性能优化主线ipc-v2--ipc-v3)
4. [IPC v3 五大关键优化](#4-ipc-v3-五大关键优化)
5. [关键 Benchmark 数据](#5-关键-benchmark-数据)
6. [SOTA API：隐式 Server · 注册-获取 · 线程优惠](#6-sota-api隐式-server--注册-获取--线程优惠)
7. [Rust 内核重构：七 Crate 四层工作区](#7-rust-内核重构七-crate-四层工作区)
8. [两轴兜底机制：发送链 + 接收链](#8-两轴兜底机制发送链--接收链)
9. [Relay 分布式资源发现](#9-relay-分布式资源发现)
10. [Relay QPS 优化：700 → 35,900](#10-relay-qps-优化700--35900)
11. [未来：端到端 Endgame 架构](#11-未来端到端-endgame-架构)
12. [总结：从性能到生态](#12-总结从性能到生态)

---

## 1. 起点：为什么要做 C-Two

### 1.1 背景：GIS 紧耦合建模的通信瓶颈

- **场景**：水动力、交通、洪涝等领域的**跨模型耦合**（coupled modeling），需要在多个计算内核之间频繁交换网格、状态、物理参数。
- **传统做法**：
  - **进程内**：OpenMP / 共享指针 → 性能极致，但模型必须同一进程。
  - **HPC 集群**：MPI-3 RMA / Infiniband RDMA → 性能高，但耦合需要同一二进制、同一部署。
- **工程痛点**：模型越来越异构（Python / C++ / Rust / Fortran 混合），团队越来越分布式（前端 / 后端 / 算法各管一块），传统紧耦合方案可扩展性差。

### 1.2 C-Two 的核心命题

> **把"资源"抽象成一等公民的 RPC 原语，让紧耦合与松耦合在同一 API 下自由切换。**

- **资源（Resource）不是 Service**：持久、有状态的领域对象（如网格、图层、物理参数库）。
- **CRM 契约**：Pythonic 的接口类声明；同一契约，线程内零拷贝、进程间共享内存、跨主机 HTTP。
- **部署拓扑无关**：同一份模型代码，单进程跑测试、多进程跑仿真、多主机跑集群，业务代码完全不改。

### 1.3 设计目标（写入 README 作为公共承诺）

| 目标 | 承诺 |
|---|---|
| **性能** | 大 payload 接近 SHM 硬件上限（>10 GB/s），小 payload < 0.2ms |
| **简单** | `cc.register` + `cc.connect` 两个 API 覆盖 90% 场景 |
| **兼容** | 线程 / 进程 / 跨主机三种模式统一 |
| **可演进** | Rust 内核 + Python 外壳，未来可对接 TS / C++ 客户端 |

---

## 2. 老 C-Two 的局限

### 2.1 旧架构：`memory://` 文件式 IPC

- 通信通过**共享文件 + 轮询**（polling）实现。
- 每个请求：写文件 → 对端轮询发现 → 读文件 → 写响应 → 对端轮询 → 读取。
- 小 payload 延迟 ~30ms（轮询间隔决定），吞吐天花板 ~0.5 GB/s。

### 2.2 老版本的三类硬伤

| 问题 | 影响 |
|---|---|
| **轮询而非事件驱动** | 延迟大、CPU 空转 |
| **序列化强依赖 pickle** | 无法跨语言，无法零拷贝 |
| **协议绑定死** | 一种协议一份 Server/Client，切换要改代码 |

### 2.3 从"协议硬绑"到"协议隐式"

- 老版本：用户要手写 `ipc://xxx` / `memory://yyy` / `http://zzz`，每种协议都有独立 Server/Client 配对。
- SOTA 观察（见 `docs/vision/sota-patterns.md`）：
  - 资源注册后，协议选择应当**对调用者不可见**。
  - 同进程 `cc.connect` 返回真实对象引用；跨进程走 IPC；跨主机走 HTTP。
  - 这是 v0.3 SOTA API 的起点。

---

## 3. 性能优化主线：IPC v2 → IPC v3

### 3.1 IPC v2（过渡期）

- **控制面**：Unix Domain Socket（UDS），事件驱动取代文件轮询。
- **数据面**：`multiprocessing.SharedMemory` 池，大 payload 走 SHM，小的 inline 走 UDS。
- 成功解决了轮询延迟问题，但：
  - SHM 池用 Python `multiprocessing` 封装，分配开销高（~100μs 级别）。
  - 每次分配都要跨 Python/OS 边界，大并发下 CPU 吃紧。
  - 序列化仍走 pickle，大 payload 吞吐受限。

### 3.2 IPC v3（当前主线）

- **控制面**：UDS + 预编码帧头 + scatter write（`writev`）。
- **数据面**：Rust 原生 Buddy Allocator + POSIX `shm_open`，双向 SHM 池。
- **关键飞跃**：把整条数据通路（分配、编码、读写、解包）从 Python 下沉到 Rust。

### 3.3 v2 vs v3 对比（realistic @transferable workload）

| Payload | IPC v2 | IPC v3 | 加速比 |
|---|---|---|---|
| 64 B | 0.138 ms | 0.115 ms | 1.2× |
| 1 MB | 2.01 ms | 0.44 ms | 4.6× |
| 10 MB | 24.2 ms | 2.8 ms | 8.6× |
| 100 MB | 218 ms | 21 ms | **10.4×** |
| 1 GB | 4842 ms | 936 ms | 5.1× |

来源：`docs/logs/bench-realistic.md`、`docs/logs/ipc-v3-optimization-report.md`。

### 3.4 和"零拷贝理想"对比

- `memory://`（纯文件轮询）→ `ipc://`（IPC v3）：
  - geomean P50 ≥10MB：**10.7×** 提速
  - 峰值吞吐：**0.44 GB/s → 5.03 GB/s**（11.4×）
  - 64B 小请求：**271×**（轮询 31ms → 事件 0.12ms）

---

## 4. IPC v3 五大关键优化

所有优化都围绕一个主题：**消除热路径上的所有堆分配与拷贝**。

### 4.1 Rust Buddy Allocator（零拷贝数据面）

- **原理**：Power-of-2 分块 + 位图 + CAS 原子操作。
- **并发安全**：原子 CAS 保证多进程共享 SHM 下的分配/释放安全。
- **崩溃恢复**：基于 PID 的 spinlock owner 检查 + 启动期 GC，自动清理前一进程崩溃残留的 segment。
- **Idle GC**：长时间未用的 segment 自动回收，平衡内存占用。
- **兜底**：小/中块走 buddy 池；超大块直接 `shm_open` 开独立 segment；RAM 紧张时降级为文件 spill。

### 4.2 零拷贝 Wire 协议

- **预编码帧头**：Rust 栈上 `[0u8; 16]` 缓冲，`encode_into` 就地写。
- **memoryview 全链路**：Python 侧从接收到分发全程只传 `memoryview`，不做 `bytes(x)` 拷贝。
- **Scatter write（`writev`）**：`IoSlice::new(&header_buf) + IoSlice::new(payload)`，一次系统调用发送多段数据。
- **Inline 快速路径**：<4KB payload 直接嵌入 UDS 帧，完全绕过 SHM 分配。

### 4.3 Buddy Handshake（双向 SHM 池）

- **建连握手**：客户端 ↔ 服务端双向交换各自的 SHM 池前缀（`/cc3c` 客户端发送池、`/cc3r` 服务端响应池等）。
- **Lazy open**：对端根据前缀 + 索引按需 `shm_open` 对应 segment，无需显式声明协议。
- **全双工**：请求走客户端池，响应走服务端池，无锁并行。

### 4.4 弹性 Segment 扩容 + Lazy Idle GC

- **弹性扩容**：初始 1 个 buddy segment，当 `alloc` 失败且未达 `max_segments` 时惰性 `create_segment()` 新开一段（上限默认 4 段 × 256MB = 1GB）。
- **尾部回收**：`gc_buddy` 只回收列表**尾部**空闲 segment，永远保留至少 1 段，避免段索引（已写入 wire frame）失效。
- **Idle 判定**：每个 segment 空闲时写入 `idle_since` 时间戳，达 `buddy_idle_decay_secs`（默认 60s）才能被回收。
- **Lazy 触发**：没有后台线程；只在下一次 `create_segment()` 尝试扩容前检查是否能回收空位。零空转开销。

### 4.5 Dedicated 大块兜底 + Flag-Based 崩溃恢复

- **Dedicated 路径**：当单次 payload 超过 `segment_size`（默认 256MB）或所有 buddy 段都满，直接 `shm_open` 一个独立 SHM segment 承载该次请求。
- **正常 GC**：reader 读完后通过 SHM 头部原子位写 `read_done=1`，creator 侧 `gc_dedicated` 扫到该标志立即 `shm_unlink` 回收。
- **崩溃兜底**：`dedicated_crash_timeout_secs`（默认 60s）兜底——若 reader 进程崩溃导致 `read_done` 永远不被置位，creator 侧超时强制回收，防止 SHM 泄漏。
- **Reader 侧独立 GC**：peer 端读完立刻本地 `munmap`（不做 `shm_unlink`，由 creator 负责）。

### 4.6 2GB Segment：消除 Dedicated 路径

来源：`docs/superpowers/reports/2026-04-03-bidirectional-zero-copy-benchmark.md`

- 旧配置：256MB/seg，超过 256MB 的 payload 自动走 dedicated 路径（每请求独立 `shm_open`）。
- 新配置：**2GB/seg**，覆盖 99% 场景的单请求大小，全部走 buddy 快速路径。
- **性能翻倍**：
  - 500MB：303ms → 58ms（**5.16×**）
  - 1GB：1134ms → 509ms（2.23×）
- **Buddy 路径有效吞吐**：
  - 10MB：12.4 GB/s
  - 100MB：18.8 GB/s
  - 500MB：17 GB/s（接近硬件上限）

---

## 5. 关键 Benchmark 数据

### 5.1 端到端基准（`bench-realistic.md`）

Realistic workload = @transferable 真实结构体 + 典型尺寸分布。

| 维度 | memory:// | ipc-v3 | 相对提升 |
|---|---|---|---|
| **Geomean P50（≥10MB）** | 384.8 ms | 35.8 ms | **10.7×** |
| **峰值吞吐** | 0.44 GB/s | 5.03 GB/s | **11.4×** |
| **64 B 延迟** | 31.1 ms | 0.12 ms | **271×** |
| **1 GB 往返** | 3827 ms | 729 ms | 5.3× |

### 5.2 单向 SHM 效率（`2026-04-03-bidirectional-zero-copy-benchmark.md`）

Buddy segment 从 256MB 扩到 2GB 后，单向吞吐：

| Payload | 吞吐 |
|---|---|
| 1 MB | 5.2 GB/s |
| 10 MB | 12.4 GB/s |
| 100 MB | **18.8 GB/s** |
| 500 MB | 17 GB/s |

接近本机 DDR4 SHM 硬件上限。

### 5.3 Dict/Bytes 比率（`2026-04-04-post-optimization-benchmark.md`）

Python 侧自动 transferable 的 pickle 开销是否被吸收？

| 阶段 | dict 相对 bytes 的倍数 |
|---|---|
| 优化前 | 1.2× – 1.8× |
| 优化后 | **~1.0×**（完全吸收） |

说明：Python 侧 pickle overhead 在 v3 架构下被 SHM 零拷贝和 scatter write 完全覆盖，用户无需为便利付性能税。

### 5.4 Relay HTTP QPS（`docs/logs/relay-qps-optimization.md`）

| 阶段 | QPS | 相对基线 |
|---|---|---|
| 初始 Rust Relay | 700 | 1× |
| Phase 1（Rust 优化完成） | 9,500 | 14× |
| Phase 2（Python Server 优化完成） | **35,883** | **51×** |

每秒 3.5 万次 HTTP → IPC → CRM 调用。详见 §10。

---

## 6. SOTA API：隐式 Server · 注册-获取 · 线程优惠

### 6.1 设计哲学

来源：`docs/vision/sota-patterns.md`

> **"CRM Server 不是服务，是资源托管器。"**

核心三条：
1. **资源对象的 Server 启动应是隐式行为**，与业务计算共存一个进程，最大化局部性。
2. **C-Two 是注册-获取式**，协议选择对调用者透明。
3. **线程优惠**：同进程 `cc.connect` 返回真实对象引用，零序列化开销。

### 6.2 核心 API（当前 v0.3）

```python
import c_two as cc

# —— 服务端 ——
cc.register(Grid, grid_instance, name='grid')   # 隐式启动 IPC server
cc.serve()                                        # 可选：阻塞主线程（不调用则非阻塞）

# —— 客户端 ——
grid = cc.connect(Grid, name='grid')              # 自动选择协议
grid.some_method(arg)                             # 直接调用
cc.close(grid)
```

### 6.3 三种协议的透明切换

| 场景 | 判定 | 协议 | 开销 |
|---|---|---|---|
| 同进程 | `name` 已注册在本进程 | `thread://`（零开销） | 直接对象引用 |
| 同主机 | `name` 注册在另一进程 | `ipc://`（UDS + SHM） | ~0.1ms |
| 跨主机 | 通过 Relay 解析 | `http://`（Rust relay 转发） | ~1-5ms |

调用者代码**完全相同**。部署拓扑变化无需改业务代码。

### 6.4 角色自由叠加

一个进程可以**同时**：
- 注册若干资源（资源托管者）
- 消费其它资源（客户端）
- 在资源方法内部再调用外部资源（递归组合）

这是对主流架构的映射：

| 主流架构 | C-Two |
|---|---|
| Stateless Service | 纯客户端进程（不调 `cc.register`） |
| Database / State Store | Resource（被注册的对象） |
| gRPC / SDK | CRM 契约（`@cc.crm` 类） |
| Service Mesh | Relay Mesh |
| Control Plane | Toodle（未来） |

---

## 7. Rust 内核重构：七 Crate 四层工作区

来源：`docs/superpowers/specs/2026-04-05-native-workspace-restructure-design.md`

### 7.1 架构图

```
        ┌─────────────────────────────────┐
        │  Python (c_two/*.py)            │
        │  crm/ · transport/ · config/    │
        └──────────────┬──────────────────┘
                       │ PyO3 FFI
        ┌──────────────▼──────────────────┐
        │  c2-ffi (bridge)                │   ← 唯一 Python 入口
        └──┬────────┬──────────┬──────────┘
           │        │          │
    ┌──────▼──┐  ┌─▼────┐   ┌─▼──────┐
    │ c2-ipc  │  │c2-srv│   │ c2-http│   ← transport/
    └───┬─────┘  └──┬───┘   └───┬────┘
        │           │           │
        └───────────┴───────────┘
                    │
           ┌────────▼────────┐
           │    c2-wire      │   ← protocol/
           └────────┬────────┘
                    │
        ┌───────────┴───────────┐
        │                       │
   ┌────▼─────┐          ┌─────▼──────┐
   │ c2-mem   │          │ c2-config  │   ← foundation/
   └──────────┘          └────────────┘
```

### 7.2 Crate 职责清单

| 层 | Crate | 职责 |
|---|---|---|
| foundation | `c2-config` | 统一 IPC 配置结构（Base/Server/Client） |
| foundation | `c2-mem` | Buddy 分配器、SHM 区域、三级 MemPool |
| protocol | `c2-wire` | Wire 协议编解码、帧格式、ChunkRegistry |
| transport | `c2-ipc` | 异步 IPC 客户端（UDS + SHM），分块传输 |
| transport | `c2-server` | Tokio 服务端，连接状态、peer SHM lazy-open |
| transport | `c2-http` | HTTP 客户端 + HTTP Relay 服务端 |
| bridge | `c2-ffi` | PyO3 绑定，聚合上述所有 crate |

### 7.3 Python 侧瘦身（`docs/architecture-review-v030.md`）

| 文件 | v0.3 | v0.4（Rust 下沉后） |
|---|---|---|
| `transport/client/core.py` | 1017 行 | **85 行**（兼容薄壳） |
| `transport/server/core.py` | 911 行 | **删除**（由 Rust `c2-server` 取代） |
| `transport/wire.py` | 505 行 | **93 行**（只保留 MethodTable） |

Python 专注业务编排（注册、调度、序列化元数据），Rust 负责热路径（字节流、内存、并发）。

---

## 8. 两轴兜底机制：发送链 + 接收链

Rust 内核的内存兜底**不是一条链**，而是两条独立的退化轴：发送侧（客户端编码请求 / 服务端编码响应）走 **T1→T2→T3→Chunk Streaming**；接收侧（组装对端 chunk）走 **T1→T2→T3→FileSpill**。两轴共用 buddy 池但走不同的 allocate API（`pool.alloc()` vs `pool.alloc_handle()`）。

### 8.1 发送轴兜底（`pool.alloc()`：SHM-only）

```
           ≤4KB              ≤ max_frame_size
              ▼                     ▼
     ┌──────────────┐     ┌─────────────────────────┐
     │ Inline UDS   │     │  Buddy Pool (T1-T3)     │
     │ (绕过 SHM)   │     │                         │
     └──────────────┘     │ T1: buddy 位图分配      │
                          │ T2: 扩容新 segment      │
                          │ T3: Dedicated shm_open  │
                          └─────────────────────────┘
                                      │
                                      ▼ SHM 全满 / 分配失败
                          ┌─────────────────────────┐
                          │  T4: Chunk Streaming    │
                          │  UDS inline 流式发送    │
                          │  (128KB / chunk)        │
                          └─────────────────────────┘
```

- **T1**：Buddy 位图分配（首选）。
- **T2**：当前 segment 满 → 扩容新 segment（受 `max_segments` 限制）。
- **T3**：单次 payload 超 `segment_size` 或 max 段用尽 → Dedicated SHM。
- **T4**：T1-T3 全失败 → 把 payload 按 `chunk_size`（默认 128KB）切片走 UDS inline 控制通道流式发送，零 SHM 依赖。**这是纯兜底**，正常路径下不会触发；仅用于极端 SHM 受限或 `/dev/shm` 满盘的场景。

### 8.2 接收轴兜底（`pool.alloc_handle()`：SHM + FileSpill）

```
             对端 chunk / 响应数据
                      ▼
          ┌─────────────────────────┐
          │  Buddy Pool (T1-T3)     │
          │ 同发送轴的 SHM 三级链   │
          └─────────────────────────┘
                      │
                      ▼ SHM 全满 + spill_threshold 触发
          ┌─────────────────────────┐
          │  T4: FileSpill          │
          │  磁盘 mmap 回退         │
          │  (reassembly-only)      │
          └─────────────────────────┘
```

- **FileSpill 仅在接收/重组路径生效**（`ChunkAssembler`、响应 pool），发送路径没有该兜底。
- **触发条件**：通过 `spill_threshold`（默认 0.8）和可用物理内存判定，当 SHM 分配失败且判定为内存紧张时切到磁盘 mmap。
- **回升**：`chunk/promote.rs` 在 reassembly 完成后会尝试把 FileSpill handle 升级回 SHM（若此时有 buddy 空位可用）。

> **实现关键**：`MemHandle` enum（`Buddy` / `Dedicated` / `FileSpill`）对上层屏蔽三种存储介质，业务代码不感知兜底发生。

### 8.3 三个容易混淆的大小参数

| 参数 | 默认值 | 语义 |
|---|---|---|
| `shm_threshold` | 4 KB | ≤ 此值走 inline UDS，> 此值走 SHM |
| `chunk_size` | 128 KB | Chunk Streaming 兜底的切片粒度（仅在 T4 发送兜底时生效） |
| `segment_size` | 256 MB | Buddy pool 单段大小，决定何时切 Dedicated |
| `max_frame_size` | **2 GB（服务端默认）** | 单 frame 上限，防 DoS |
| `max_payload_size` | 2 GB | 单次调用总负载上限 |

> **注意**：`max_frame_size` 不是 128KB。混淆的根源是 `chunk_size=128KB` 只是兜底的切片尺寸；正常路径单帧可达 2GB，完全走 SHM 零拷贝。

### 8.4 崩溃恢复与并发安全

- **PID-based spinlock recovery**：分配器锁记录 owner PID。如果 owner 进程已死，下一个抢锁者清理残留状态继续。
- **Buddy idle GC**：`gc_buddy()` 懒惰回收尾部空闲 segment（见 §4.4）。
- **Dedicated 双轨 GC**：`read_done` flag 正常路径 + `dedicated_crash_timeout_secs` 崩溃兜底（见 §4.5）。
- **原子 CAS 操作**：Buddy bitmap 的 `alloc()/free_at()` 完全无锁，天然支持多进程并发。

### 8.5 内存压力防御（`docs/memory-pressure-backpressure.md`）

| 层级 | 阶段 | 机制 |
|---|---|---|
| **L0** | 配置时 | 物理内存校验：`segment_size > phys_ram` 直接报错 |
| **L1** | 接收侧 | FileSpill 兜底（见 §8.2） |
| **L2** | 发送侧 | SHM 全满 → Chunk Streaming 兜底（见 §8.1 T4）/ `MemoryPressureError` |

### 8.6 安全机制（Wire / 协议层）

- **Frame size 限制**：`max_frame_size=2GB` 硬上限防 DoS。
- **SHM name 校验**：防路径注入。
- **Double-free guard**：分配位图 + owner 检查。
- **TOCTOU 保护**：segment 打开后重新验证 magic/version。
- **Stale cleanup**：进程崩溃后 SHM 残留由下一个启动者 GC。

---
## 9. Relay 分布式资源发现

来源：`docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md`

### 9.1 从「地址耦合」到「名字解析」

**老的痛点**：
```python
cc.connect(Grid, name='grid', address='ipc:///tmp/node3_grid_uuid')
```
客户端必须知道目标进程的确切地址。跨主机部署时手工维护地址极其脆弱。

**新的目标**：
```python
cc.connect(Grid, name='grid')   # 不再需要 address
```
Relay Mesh 自动解析 `name → 所在主机 → 直连`。

### 9.2 架构：去中心化 Mesh

```
┌──────────┐   gossip   ┌──────────┐   gossip   ┌──────────┐
│ Relay-A  │◄─────────►│ Relay-B  │◄─────────►│ Relay-C  │
│ Node-1   │            │ Node-2   │            │ Node-3   │
│          │            │          │            │          │
│ Route    │ full copy  │ Route    │ full copy  │ Route    │
│ Table    │◄─────────►│ Table    │◄─────────►│ Table    │
│          │            │          │            │          │
│ 本地 CRM │            │ 本地 CRM │            │ 本地 CRM │
└──────────┘            └──────────┘            └──────────┘
     ▲ IPC                 ▲ IPC                  ▲ IPC
  Client-1              Client-2               Client-3
```

**三大特性**：
- **无单点故障**：每个 Relay 持有完整 RouteTable。
- **本地零跳**：同节点 CRM 直接 IPC。
- **跨节点单跳**：解析在本地 Relay 完成，直连目标 Relay。

### 9.3 名字解析四层优先级

| 优先级 | 判定 | 传输 | 延迟 |
|---|---|---|---|
| 1 | 同进程已注册 | thread-local proxy | ~0 |
| 2 | 本 Relay 有 LOCAL 路由 | IPC → 目标 CRM | ~0.3ms |
| 3 | 本 Relay 有 PEER 路由 | HTTP → 目标 Relay | ~1-5ms |
| — | 都没有 | `ResourceNotFound` | — |

### 9.4 Gossip 协议

所有对等通信走 HTTP 端点（复用 axum 栈）：

| 端点 | 用途 |
|---|---|
| `/_peer/join` | 新 Relay 向 seed 宣告 |
| `/_peer/sync` | 请求完整 RouteTable + PeerList |
| `/_peer/announce` | 广播路由或 Relay 状态变化 |
| `/_peer/heartbeat` | 周期心跳 |
| `/_peer/leave` | 优雅下线通知 |
| `/_peer/digest` | 反熵（anti-entropy）digest 交换 |

**核心消息**：
```rust
enum PeerMessage {
    RouteAnnounce { name, relay_id, relay_url, ipc_address, crm_ns, crm_ver, registered_at },
    RouteWithdraw { name, relay_id },
    RelayJoin    { relay_id, url },
    RelayLeave   { relay_id },
    Heartbeat    { relay_id, route_count },
    DigestExchange { digest: HashMap<(String, String), u64> },
    DigestDiff     { missing: Vec<RouteEntry>, extra: Vec<(String, String)> },
}
```

### 9.5 一致性保证

- **全副本 + 幂等 upsert**：所有 gossip 操作以 `(name, relay_id)` 为主键 upsert，重放无副作用。
- **确定性排序**：同名多实例时，按 `(registered_at, relay_id)` 排序，所有节点解析到同一个目标。
- **反熵修复**：每 60 秒与一个对等 Relay 交换 digest，网络分区恢复后自动收敛。
- **崩溃安全**：SQLite WAL 模式持久化；启动时清除本地残留路由；从 seed 全量同步 PEER 路由。

### 9.6 兜底：去中心化但允许「独立运行」

- 如果所有 seed 都不可达 → 独立运行（只有本地 CRM），每 10s 重试连接。
- 一旦 seed 恢复，完成 join 协议，加入 Mesh。

---

## 10. Relay QPS 优化：700 → 35,900

来源：`docs/logs/relay-qps-optimization.md`

**51× 总提速**，来自 Rust + Python 两阶段优化。

### 10.1 Phase 1：Rust 侧（700 → 9,500）

| # | 优化 | QPS | 增益 |
|---|---|---|---|
| 0 | baseline | 4,868 | — |
| 3 | `std::Mutex` 替换 `tokio::Mutex` | 6,695 | +37% |
| 7 | 零分配 frame write（`writev` + 栈上 header） | 9,543 | +100% vs baseline |

**关键洞察 1**：IPC 调用微秒级持锁，不跨 `.await`，`std::sync::Mutex` 比 `tokio::sync::Mutex` 永远更快。

**关键洞察 2**：
```rust
// Before: 3 次分配
let frame = [header, payload].concat();
stream.write_all(&frame).await?;

// After: 零分配 + writev
let mut header_buf = [0u8; 16];
let bufs = [IoSlice::new(&header_buf), IoSlice::new(payload)];
stream.write_vectored(&bufs).await?;
```

### 10.2 Phase 2：Python 侧（9,500 → 35,883）

| # | 优化 | QPS | 增益 |
|---|---|---|---|
| 1 | 流水线化（`await` → `create_task`） | 18,057 | **+91%** |
| 7 | FastDispatcher（SimpleQueue 替代 ThreadPoolExecutor） | 24,583 | +18% |
| 9 | 2 workers 优于 4/8（降低锁竞争） | 26,747 | +9% |
| 12 | 零 Task 快速路径（读循环直接分发） | 33,517 | **+16%** |
| 13 | dispatch cache（路由级缓存） | **35,883** | +7% |

**关键洞察 3**：单连接串行处理阻塞并发请求。`asyncio.create_task` 让读下一帧与上一请求执行并行，收益最大（+91%）。

**关键洞察 4**：EXCLUSIVE 模式下只许 1 线程执行 CRM，**2 workers 最优**（1 执行 + 1 准备），更多 worker 增加竞争反而降速。

**关键洞察 5**：事件循环只做 I/O + 控制面，worker 线程自包含构建回复帧，通过 `call_soon_threadsafe` 写回。

### 10.3 架构示意

```
asyncio event loop                 worker threads (×2)
──────────────────                 ───────────────────
UDS recv ─→ readexactly(16)
         unpack header
         readexactly(payload)
         ├─ cache hit:
         │   SimpleQueue.put ──→ q.get()
         │   continue (read next)    ↓
         │                           exec_fast(method, args)
         │                           with exclusive_lock:
         │                             result = method(args)
         │                           ↓
         │                           encode_inline_reply(req_id, result)
         │   ←── call_soon_threadsafe ─┘
         │   writer.write(frame)
         │
         └─ fallback → asyncio.create_task(handler)
```

---

## 11. 未来：端到端 Endgame 架构

来源：`docs/vision/endgame-architecture.md`

### 11.1 四层体系

```
┌────────────────────────────────────────────────────────┐
│  L4 · Gridmen                                          │
│       人在回路的智能地理编辑器                         │
├────────────────────────────────────────────────────────┤
│  L3 · 地理资源目录                                     │
│       IVectorLayer / IRasterLayer / ITiledGrid / ...   │
├────────────────────────────────────────────────────────┤
│  L2 · Toodle                                           │
│       Tag-Oriented Open Data Linking Environment       │
│       身份认证 / 授权 / Workspace / 扩展注册表 / Agent │
├────────────────────────────────────────────────────────┤
│  L1 · C-Two                                            │
│       CRM 契约 / register-get / IPC + HTTP / Relay     │
│       auth_hook / metadata passthrough                 │
└────────────────────────────────────────────────────────┘
```

**机制不政策**（Mechanism not Policy）：C-Two 只提供机制，策略交由上层。

### 11.2 C-Two 的护城河边界

| ✅ 放进 C-Two | ❌ 不放进 C-Two |
|---|---|
| 零拷贝、异步、buffer 生命周期 | 认证、授权、租户 |
| 方法级并发（`@cc.read`/`@cc.write`） | 多节点主从仲裁 |
| CRM 版本、Hold 语义、Metadata 透传 | Workspace、Project、图层树 |
| **fastdb 零反序列化 codec** | 文件格式、地理语义 |
| Tag 作为**路由提示** | Tag 作为**身份或能力** |

### 11.3 两个即将补齐的关键能力

| 能力 | 作用 |
|---|---|
| **`auth_hook` + call metadata 透传** | 让 Toodle 注入签名身份、拦截非法调用 |
| **TypeScript 客户端 + fastdb 统一 codec** | 前端一等公民，浏览器直连 CRM，不过翻译层 |

这两点到位后，Toodle 只需「往上加」，不需要回头改 C-Two。

### 11.4 Agent 视角：CRM 作为能力边界

> **Toodle 改名 = Tag-Oriented Open Data Linking Environment**
> 标签给资源**赋予语义**；CRM 给资源**圈出行为边界**。
> 二者结合形成一个**可被人和 Agent 共同导航**的资源网络。

- Agent 通过 CRM 契约调度外部能力（类型化接口，而非自由文本）。
- 人在回路：LLM 当前难以低成本完成强目视解译任务（如遥感影像矢量化），人仍是地理数据的责任人。
- Agent 本身是无状态客户端，可重启、可并行；它调用的一切都是资源。

### 11.5 建模扩展（Computational Extension）

- 建模扩展 ≈ 一个包含资源使用/创建/更新能力的"组件"。
- 可自包含 CRM（如潜水方程模型自管运行时状态）。
- 可依赖外部 CRM（通过 CRM 契约访问通用地理资源）。
- 外部也可通过扩展自暴露的 CRM 访问模拟运行时。

这种「自包含 + 可依赖 + 可被依赖」是 C-Two 同时服务 HPC 紧耦合和分布式松耦合的统一根基。

---

## 12. 总结：从性能到生态

### 12.1 已完成里程碑

| 里程碑 | 核心成果 |
|---|---|
| IPC v2 | UDS 事件驱动取代文件轮询 |
| IPC v3 | Rust Buddy SHM，**10.7× 大 payload 提速**，峰值 **18.8 GB/s** |
| SOTA API | `cc.register/cc.connect`，协议透明，部署拓扑无关 |
| Rust 内核重构 | 7 crate 4 层架构，Python 代码减少 80%+ |
| 兜底机制 | 三级内存 + 三层防御，崩溃自恢复 |
| Relay Mesh | 去中心化资源发现，gossip + 反熵，无单点 |
| Relay QPS | **700 → 35,900 (51×)** |

### 12.2 技术哲学

1. **热路径下沉 Rust**：Python 管编排，Rust 管字节。
2. **零拷贝优先**：从 wire 协议到 SHM 分配，每一步消除不必要的拷贝/分配。
3. **协议透明**：调用者不关心协议，只关心资源语义。
4. **机制不政策**：C-Two 只做机制，策略/语义交给上层（Toodle）。

### 12.3 下一步

- **TypeScript 客户端** + **fastdb 统一 codec**：前端一等公民。
- **`auth_hook`**：Toodle 可以在 CRM 调用链中注入身份与策略。
- **Toodle L2 起步**：标签图、Workspace、扩展注册表、Agent runtime。
- **Gridmen L4 演进**：从水动力网格专用编辑器扩展为通用地理编辑平台。

### 12.4 一句话总结

> **C-Two 把「紧耦合的性能」与「松耦合的可演进」统一在同一个 API 之下。**
> 从单进程 HPC 到跨主机分布式、到未来的浏览器直连与 Agent 调度 —— 同一份业务代码，一路能跑。

---

## 附录 A · 文档索引

| 主题 | 路径 |
|---|---|
| 愿景架构 | `docs/vision/endgame-architecture.md` |
| SOTA API 设计 | `docs/vision/sota-patterns.md` |
| IPC v3 综合报告 | `docs/logs/ipc-v3-optimization-report.md` |
| Realistic Benchmark | `docs/logs/bench-realistic.md` |
| General Benchmark | `docs/logs/bench-general.md` |
| 双向零拷贝 | `docs/superpowers/reports/2026-04-03-bidirectional-zero-copy-benchmark.md` |
| 优化后对比 | `docs/superpowers/reports/2026-04-04-post-optimization-benchmark.md` |
| Relay QPS 优化 | `docs/logs/relay-qps-optimization.md` |
| Relay Mesh 设计 | `docs/superpowers/specs/2026-07-20-relay-mesh-resource-discovery-design.md` |
| Rust Workspace 重构 | `docs/superpowers/specs/2026-04-05-native-workspace-restructure-design.md` |
| 三级内存兜底 | `docs/superpowers/specs/2026-03-30-unified-memory-fallback-design.md` |
| 内存压力防御 | `docs/memory-pressure-backpressure.md` |
| 架构评审 v0.3 | `docs/architecture-review-v030.md` |
| 综合审计 | `docs/reports/2026-07-20-comprehensive-audit.md` |

---

*本文档用于 NotebookLM 生成 PPT。各章节已按幻灯片粒度组织；关键数据表与架构图可直接映射为单张幻灯片。*
