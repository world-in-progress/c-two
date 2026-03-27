---
title: "Rust HTTP Server 与共享内存零拷贝透传可行性调研"
category: "Performance"
status: "🟢 Complete"
priority: "High"
timebox: "1 week"
created: 2026-03-27
updated: 2026-03-27
owner: "soku"
tags: ["technical-spike", "performance", "zero-copy", "shared-memory", "rust", "http", "ipc"]
---

# Rust HTTP Server 与共享内存零拷贝透传可行性调研

## Summary

**Spike Objective:** 调研 Rust HTTP 服务器能否在接收 HTTP 请求时将 body bytes 直接写入共享内存（buddy allocator SHM），以及 CRM 服务器返回的 bytes 能否直接从 SHM 透传回 HTTP 响应，避免用户态多次内存拷贝。同时调研工业界分布式系统如何解决基于 HTTP 的跨节点大数据通信问题。

**Why This Matters:** SOTA 设计中的 Rust Routing Server 是 C-Two 跨节点 HTTP 路由的性能关键路径。链路为 `Browser/Client → HTTP → Rust Routing Server → IPC-v2 (SHM pool) → CRM Server (Python)`。对于 GIS 可视化等场景，单次 tile 请求返回的 mesh/raster 数据可达数十 MB 甚至更大。如果 HTTP 接收和 IPC 转发各引入一次全量内存拷贝，将造成显著的延迟和内存带宽瓶颈。

**Timebox:** 1 week

**Decision Deadline:** Rust Routing Server 实现方案确定前

## Research Question(s)

**Primary Question:** Rust HTTP Server（基于 hyper/axum）接收请求 body 时，能否直接将数据写入 buddy allocator 管理的 POSIX 共享内存段，实现零拷贝或最少拷贝？

**Secondary Questions:**

1. CRM Server 处理完请求后写入 SHM 的响应 bytes，能否零拷贝地通过 HTTP 响应返回给客户端？
2. 标准 HTTP 框架（hyper/actix-web）的数据接收路径中有哪些不可避免的内存拷贝？
3. Linux 内核提供了哪些零拷贝机制（io_uring ZC Rx、sendfile、splice/vmsplice）可以优化此路径？
4. 工业界分布式系统（Ray、Dask、gRPC、iceoryx）如何解决大数据量跨节点/跨进程通信的性能问题？
5. 针对 C-Two 的架构约束，最优的实现方案是什么？

## Investigation Plan

### Research Tasks

- [x] 分析 C-Two 现有 IPC v3 buddy allocator SHM 架构及数据流
- [x] 分析 Rust HTTP 框架（hyper/axum）的 body 接收内存模型
- [x] 调研 Linux 零拷贝网络接收机制（io_uring ZC Rx、MSG_ZEROCOPY）
- [x] 调研 Linux 零拷贝发送机制（sendfile、splice、vmsplice）
- [x] 调研 tokio-uring 的 fixed buffer / registered buffer 支持
- [x] 调研工业界零拷贝 IPC 方案（iceoryx、Shmipc、Ray Plasma）
- [x] 调研 RDMA 和内核旁路方案（DPDK、AF_XDP）
- [x] 综合分析并给出 C-Two Rust Routing Server 的推荐方案

### Success Criteria

**This spike is complete when:**

- [x] 明确 Rust HTTP Server → SHM 路径上的最少拷贝次数及其原因
- [x] 明确 SHM → HTTP Response 路径上的最少拷贝次数及其原因
- [x] 给出工业界主流方案的对比分析
- [x] 给出 C-Two Rust Routing Server 的具体推荐实现方案

## Technical Context

**Related Components:**
- `rust/c2_buddy/` — Buddy allocator SHM 实现（Rust + PyO3）
- `src/c_two/rpc_v2/` — RPC v2 模块（SharedClient、ServerV2、Wire v2）
- `src/c_two/rpc/ipc/` — IPC v3 协议实现
- `src/c_two/rpc/http/` — 当前 Python HTTP Server/Client 实现
- `doc/log/sota.md` — SOTA 设计文档，附录 Rust Routing Server 方案

**Dependencies:**
- Rust Routing Server 实现方案依赖此 spike 的结论
- `c2-wire` / `c2-ipc` / `c2-router` crate 的设计决策
- 跨语言 SDK 的技术路线

**Constraints:**
- 必须兼容现有 IPC v3 buddy allocator 协议（POSIX SHM + UDS 控制面）
- 目标平台 Linux（主要）和 macOS（开发），Windows 非优先
- HTTP/1.1 为最低要求，HTTP/2 可选
- 必须支持 64B 到 1GB+ 的 payload 范围

---

## Research Findings

### 一、数据流全景：从网卡到 CRM 再到客户端

为了准确评估拷贝次数，需要理解完整数据流中的每一步：

#### 请求路径（Request Path）

```
┌─────────┐    ┌──────────────────┐    ┌─────────────────┐    ┌──────────────┐
│   NIC   │───▶│  Kernel Socket   │───▶│  User-Space      │───▶│  Buddy SHM   │
│ (DMA)   │    │  Buffer (sk_buf) │    │  Framework Buf   │    │  (mmap'd)    │
└─────────┘    └──────────────────┘    └─────────────────┘    └──────────────┘
      DMA写入        ①内核→用户态拷贝       ②用户态→SHM拷贝
```

```
┌──────────────┐    ┌─────────────┐    ┌──────────────────┐
│  Buddy SHM   │───▶│  UDS Control │───▶│  CRM Server      │
│  (请求数据)   │    │  Frame (UDS) │    │  (Python/读SHM)  │
└──────────────┘    └─────────────┘    └──────────────────┘
    SHM已就绪       仅11字节控制帧        零拷贝memoryview
```

#### 响应路径（Response Path）

```
┌──────────────┐    ┌─────────────┐    ┌──────────────────┐
│  CRM Server  │───▶│  Buddy SHM  │───▶│  UDS Control     │
│  (写入SHM)   │    │  (响应数据)  │    │  Frame           │
└──────────────┘    └─────────────┘    └─────────────────┘
    CRM写入SHM       SHM已就绪          11字节控制帧
```

```
┌──────────────┐    ┌─────────────────┐    ┌──────────────────┐    ┌──────────┐
│  Buddy SHM   │───▶│  User-Space      │───▶│  Kernel Socket   │───▶│   NIC    │
│  (响应数据)   │    │  Framework Buf   │    │  Buffer (sk_buf) │    │  (DMA)   │
└──────────────┘    └─────────────────┘    └──────────────────┘    └──────────┘
    ③SHM→用户态拷贝      ④用户态→内核拷贝        DMA发送
```

**关键观察**: 在标准 TCP/HTTP 栈下，请求路径最少 2 次拷贝（①②），响应路径最少 2 次拷贝（③④）。

### 二、Rust HTTP 框架（hyper/axum）的内存模型分析

#### 2.1 hyper 的 Body 接收机制

hyper 是 Rust 生态中最成熟的 HTTP 实现，axum 构建于其上。其请求 body 处理流程：

1. **tokio TCP 读取**：tokio 的 `AsyncRead` 从内核 socket buffer 读取数据到用户态 `BytesMut` 缓冲区
   - 这是 **不可避免的第 ① 次拷贝**（`read(2)` / `recv(2)` 系统调用，内核→用户态）
   - hyper 内部维护一个 read buffer（默认 8KB-64KB），通过 `BytesMut` 管理

2. **HTTP 解析**：hyper 在 read buffer 上原地解析 HTTP headers
   - 使用 `httparse` crate，零拷贝引用（`&[u8]` 切片）
   - Headers 解析完成后，body 数据以 `Bytes` chunks 流式输出

3. **Body 流式传递**：hyper 的 `Body` trait 产出 `Frame<Bytes>` 流
   - `Bytes` 是引用计数的不可变字节缓冲区（类似 `Arc<[u8]>`）
   - **Body chunk 到 handler 的传递是零拷贝的**（仅增加引用计数）

```rust
// axum handler 接收 body 的典型模式
async fn handle(body: axum::body::Body) -> Response {
    // body.collect() 会将所有 chunks 收集为 Bytes
    // 或者可以 stream 处理每个 chunk
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes: Bytes = chunk?;
        // bytes 是 hyper read buffer 的零拷贝切片
        // 此时需要写入 SHM —— 这是第 ② 次拷贝
        shm_write(buddy_offset, &bytes);
    }
}
```

#### 2.2 关键限制：hyper 不支持自定义 recv buffer

hyper 内部使用 `tokio::io::AsyncRead` + `BytesMut` 作为 read buffer。**无法**直接让 hyper 将 `recv()` 的数据写入外部提供的 SHM 内存区域。原因：

- `AsyncRead::poll_read()` 的 `ReadBuf` 参数由调用方（hyper 内部）控制
- hyper 的 read buffer 是 `BytesMut`（堆分配），不支持自定义分配器
- HTTP 解析需要 header + body 在连续缓冲区中，无法预知 body 起始偏移

**结论**：使用 hyper/axum 时，**第 ① 次拷贝（内核→hyper buffer）不可避免**。

#### 2.3 替代方案：绕过 hyper 的可能性

| 方案 | 描述 | 可行性 | 代价 |
|------|------|--------|------|
| 自定义 `AsyncRead` | 包装 tokio socket，将 read 导向 SHM | ❌ 不可行 | HTTP 解析需要连续 buffer，header/body 边界未知 |
| 自定义 HTTP parser | 手写 HTTP 解析，直接读入 SHM | ⚠️ 理论可行 | 巨大工程量，安全风险，失去 hyper 生态 |
| 使用 `io_uring` 替代 | tokio-uring 提供 fixed buffer recv | ⚠️ 有限 | 仍需先接收后拷贝，且 io_uring HTTP 生态不成熟 |
| Raw socket + SHM | 完全绕过 HTTP 框架 | ❌ 不合理 | 等于重写 HTTP 栈 |

### 三、Linux 内核零拷贝机制深度分析

#### 3.1 接收方向：io_uring Zero-Copy Rx（Linux 6.15+）

io_uring 在 Linux 6.15 内核引入了真正的零拷贝网络接收：

**工作原理**：
- NIC 支持 header/data split：TCP/IP headers 进入正常内核内存，payload 直接 DMA 写入用户注册的 `mmap` 内存区域
- 用户通过 io_uring 注册内存区域和 refill ring，内核直接将网络数据 DMA 到用户内存
- **TCP/IP 协议栈仍然活跃**（不同于 DPDK），只是跳过了数据拷贝

**对 C-Two 的适用性分析**：

| 维度 | 评估 |
|------|------|
| 内核版本要求 | Linux 6.15+（2025 年发布），生产环境普及需时间 |
| NIC 硬件要求 | 需支持 header/data split 和可配置 RX 队列 |
| Rust 生态支持 | tokio-uring 尚未集成 ZC Rx，需直接使用 liburing FFI |
| 与 HTTP 框架集成 | 需要自定义 IO 层替换 tokio 标准 TCP，hyper 不直接支持 |
| 复杂度 | 极高：NIC 配置（ethtool）、flow steering、buffer 管理 |
| 收益 | 消除第 ① 次拷贝（内核→用户态），对大 payload 效果显著 |

**结论**：io_uring ZC Rx 是**终极方案**，但当前不实际——硬件要求高、内核版本新、Rust 生态不成熟。可作为**中长期演进方向**。

#### 3.2 接收方向：tokio-uring Fixed Buffers

tokio-uring 提供的 `FixedBufRegistry`/`FixedBufPool` 可以预注册内核 buffer：

```rust
use tokio_uring::buf::fixed::FixedBufPool;

tokio_uring::start(async {
    let pool = FixedBufPool::new(vec![vec![0u8; 4096]; 64]);
    // 注册 buffer 给内核，减少每次 recv 的 buffer 注册开销
    let stream = TcpStream::connect("...").await?;
    let buf = pool.try_next(4096).unwrap();
    let (res, buf) = stream.recv_fixed(buf).await;
});
```

**限制**：
- Fixed buffers 减少的是 **buffer 注册开销**（避免每次 `recv` 的 `get_user_pages`），不是真正的零拷贝
- 数据仍然从内核 socket buffer 拷贝到 fixed buffer（只是 fixed buffer 已预注册）
- **不能将 POSIX SHM mmap 区域注册为 fixed buffer**——io_uring 要求 buffer 是进程私有匿名页
- tokio-uring 不兼容标准 tokio runtime（不能与 hyper 共用）

**结论**：Fixed buffers 不解决核心问题，且与 hyper/axum 不兼容。

#### 3.3 发送方向：vmsplice + splice

对于响应路径（SHM → HTTP socket），Linux 提供了一条有潜力的零拷贝路径：

```
SHM (mmap) ──vmsplice──▶ pipe ──splice──▶ TCP socket ──DMA──▶ NIC
```

**工作原理**：
1. `vmsplice()`：将用户态内存页（包括 SHM mmap 页）pinning 到 pipe buffer，不拷贝数据
2. `splice()`：将 pipe buffer 中的页引用直接移动到 socket buffer，不拷贝数据
3. NIC 通过 DMA scatter-gather 直接从这些页读取数据

**对 C-Two 的适用性**：

| 维度 | 评估 |
|------|------|
| 零拷贝能力 | ⭐ SHM → socket 路径可以真正零拷贝（vmsplice 用 `SPLICE_F_GIFT`） |
| 内核支持 | 广泛（Linux 2.6.17+），成熟稳定 |
| 与 HTTP 框架集成 | ⚠️ 需要绕过 hyper 的写入路径，直接操作底层 fd |
| 安全性 | ⚠️ 必须确保 SHM 页在 socket 发送完成前不被释放/覆写 |
| 对齐要求 | 需要页对齐（4KB），buddy allocator 的 min_block=4KB 恰好满足 |
| HTTP 协议约束 | 需要先发送 HTTP headers（可能无法零拷贝），body 部分可以 |

**关键问题**：
- `vmsplice(SPLICE_F_GIFT)` 将页面所有权转移给内核——但 SHM 页是共享的，不能"赠送"
- 不使用 `SPLICE_F_GIFT` 时，内核可能仍然需要拷贝（取决于内核版本和 NIC 驱动）
- buddy allocator 的 block 可能在 splice 完成前被 free——需要延迟释放机制（已有 `_deferred_response_free` 先例）

**结论**：vmsplice+splice 对响应路径有潜力，但与 SHM 的交互有复杂的生命周期问题。实用性需要 PoC 验证。

### 四、工业界方案对比分析

#### 4.1 Ray — Plasma Object Store

Ray 是分布式计算领域的标杆框架，其对象传输架构值得深入分析：

**同节点通信（Zero-Copy）**：
- Plasma Object Store 使用 POSIX 共享内存（`/dev/shm`）
- Worker 进程通过 mmap 直接访问对象，真正的零拷贝
- 对象 sealed 后不可变，多个 reader 可安全并发访问

**跨节点通信（Non-Zero-Copy）**：
- 使用 gRPC 进行节点间对象传输
- 源 Plasma → 序列化 → gRPC → 网络 → 目标 Plasma → 反序列化
- **跨节点无法零拷贝**——物理内存不共享，这是物理定律的限制

**C-Two 可借鉴之处**：
- Ray 也接受了"跨节点必须拷贝"的现实，转而**优化序列化效率**和**减少拷贝次数**
- 同节点 IPC 才是零拷贝的战场——C-Two 的 buddy SHM 已实现此目标
- Ray 不走 HTTP 传输大对象，而是使用自定义 gRPC stream

#### 4.2 iceoryx — 真正的零拷贝 IPC

iceoryx（由 Eclipse 基金会维护）是目前最激进的零拷贝 IPC 方案：

**核心设计**：
- **Publisher 直接写入 SHM**：`loan() → write → publish()`模式
- 消费者直接读取 SHM 中的数据，全程零拷贝
- 使用 lock-free SPMC queue + SHM 内的 atomic 控制

**与 C-Two buddy allocator 的对比**：

| 维度 | iceoryx | C-Two Buddy SHM |
|------|---------|------------------|
| 内存模型 | Publisher 预分配 slot → 写入 → seal | Client alloc → write → send控制帧 |
| 零拷贝 | 发布者直接写入 SHM，消费者直接读取 | 同节点 memoryview 零拷贝 |
| 分配器 | 固定 slot size pool | Buddy（power-of-2，灵活大小） |
| 通知机制 | Waitset / listener | UDS 控制帧 |
| 适用场景 | 高频小消息（robotics、automotive） | 可变大小（KB-GB） |

**关键洞察**：iceoryx 之所以能做到"true zero-copy"，是因为它**不涉及网络 I/O**——数据始终在同一机器的 SHM 中。一旦涉及网络（HTTP/TCP），物理拷贝不可避免。

#### 4.3 ByteDance Shmipc — 服务网格零拷贝

字节跳动开源的 Shmipc 用于微服务间高性能 IPC：

**设计特点**：
- 专为 Service Mesh sidecar（Envoy → 业务进程）优化
- SHM 环形 buffer + eventfd 通知
- 对比 Unix Domain Socket 可提升 ~5x 吞吐

**与 C-Two 的相关性**：
- Shmipc 解决的是 **sidecar → 业务进程** 的本地通信，类似于 C-Two 的 **Routing Server → CRM Server** 链路
- 但 Shmipc 不解决 **外部 HTTP client → sidecar** 的接收问题——这段仍然是传统网络 I/O

#### 4.4 RDMA — 硬件级零拷贝

RDMA (Remote Direct Memory Access) 是工业界唯一实现**跨节点零拷贝**的技术：

**工作原理**：
- NIC 直接读写远端主机的内存，绕过 CPU 和操作系统
- 延迟 < 10μs，带宽可达 400Gbps
- 需要 InfiniBand 或 RoCEv2 网络基础设施

**对 C-Two 的评估**：

| 维度 | 评估 |
|------|------|
| 性能 | ⭐⭐⭐ 终极零拷贝，延迟和带宽最优 |
| 硬件要求 | ❌ 需要 RDMA NIC + 无损以太网交换机 |
| 部署复杂度 | ❌ 极高，需要专业网络工程师 |
| 与 HTTP 兼容性 | ❌ RDMA 不走 TCP/IP，与 HTTP 不兼容 |
| 目标用户匹配 | ❌ C-Two 面向科学计算，非 HPC 集群 |

**结论**：RDMA 不适合 C-Two 的定位。C-Two 的目标是让用户在普通硬件上获得接近极限的性能，而非要求专用网络基础设施。

#### 4.5 DPDK / AF_XDP — 内核旁路

**DPDK**（Data Plane Development Kit）完全绕过 Linux 内核网络栈：
- NIC 数据直接 DMA 到用户态 hugepage 内存
- 用户态轮询（poll mode driver）处理数据包
- 需要独占 NIC 或使用 SR-IOV

**AF_XDP** 是更轻量的内核旁路方案：
- 使用 eBPF 程序在 NIC 驱动层重定向特定数据包
- 数据包直接写入用户态 UMEM 区域
- 不需要独占 NIC

**对 C-Two 的评估**：
- DPDK/AF_XDP 消除了内核网络栈的拷贝，但需要**在用户态重新实现 TCP/IP + HTTP**
- 这等于放弃所有现有 HTTP 生态（TLS、HTTP/2、WebSocket 等）
- 运维复杂度极高，不适合 C-Two 的"开箱即用"目标

#### 4.6 工业界方案总结

| 方案 | 同节点零拷贝 | 跨节点零拷贝 | 与 HTTP 兼容 | 部署复杂度 | C-Two 适用性 |
|------|-------------|-------------|-------------|-----------|-------------|
| Ray Plasma | ✅ SHM mmap | ❌ gRPC 传输 | ⚠️ Ray Serve | 中 | 参考架构 |
| iceoryx | ✅ True ZC | ❌ 仅本地 | ❌ 无 HTTP | 低 | IPC 参考 |
| Shmipc | ✅ SHM ring | ❌ 仅本地 | ❌ sidecar 模式 | 中 | sidecar 参考 |
| RDMA | N/A | ✅ 硬件 ZC | ❌ 不兼容 | 极高 | ❌ 不适合 |
| DPDK/AF_XDP | ✅ 用户态 | ⚠️ 需重实现 | ❌ 需重写栈 | 极高 | ❌ 不适合 |
| **io_uring ZC Rx** | ✅ DMA→用户 | ❌ 仅本地 | ⚠️ 需定制 | 高 | 中长期方向 |
| **标准 TCP + SHM** | ⚠️ 1次拷贝 | ❌ 标准网络 | ✅ 完全兼容 | 低 | ✅ 推荐方案 |

**核心洞察**：工业界的共识是——**跨节点（跨网络）通信时，至少一次内存拷贝不可避免**。优化策略集中在：
1. **减少拷贝次数**（从 3-4 次降到 1-2 次）
2. **优化拷贝效率**（大页、SIMD memcpy、DMA）
3. **同节点用 SHM 完全避免拷贝**
4. **分离控制面和数据面**（小控制帧 + 大数据走 SHM）
