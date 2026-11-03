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

### 五、C-Two Rust Routing Server 的现实拷贝分析

基于以上调研，让我们精确分析 C-Two 架构下的实际拷贝次数：

#### 5.1 当前 Python 实现的拷贝次数

```
Request:  NIC → kernel → uvicorn buffer → Python bytes → wire encode → Client SHM → CRM read
          拷贝:  ①          ②              ③              ④             ⑤(memoryview)
          
Response: CRM SHM → Client bytes → wire encode → Python bytes → uvicorn → kernel → NIC
          拷贝:     ⑥             ⑦             ⑧             ⑨          ⑩
```

**Python 方案**: 请求路径 ~4 次拷贝，响应路径 ~4 次拷贝，总计 **~8 次拷贝**。

#### 5.2 Rust Routing Server（标准方案）的拷贝次数

```
Request:  NIC → kernel → hyper Bytes → memcpy到SHM → CRM memoryview读取
          拷贝:  ①          ②            零拷贝
          
Response: CRM写入SHM → memcpy到hyper Bytes → kernel → NIC
          拷贝:           ③                    ④
```

**Rust 标准方案**: 请求路径 2 次拷贝，响应路径 2 次拷贝，总计 **4 次拷贝**。
相比 Python 减少 50%。

#### 5.3 Rust 优化方案的拷贝次数

进一步优化的核心思路：**让 hyper 的 Bytes 直接引用 SHM 内存**。

**请求路径优化——Streaming Write to SHM**：

```rust
async fn relay_request(body: axum::body::Body, pool: &BuddyPool) {
    // 1. 预分配 SHM block（基于 Content-Length 或初始估计）
    let alloc = pool.alloc(content_length)?;
    let shm_slice = pool.as_mut_slice(&alloc);
    
    // 2. 流式写入：每个 chunk 直接 memcpy 到 SHM 的对应偏移
    let mut offset = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        shm_slice[offset..offset + bytes.len()].copy_from_slice(&bytes);
        offset += bytes.len();
    }
    // 总拷贝：kernel→hyper(①) + hyper→SHM(②) = 2次
    // 但第②次是streaming的增量拷贝，不需要先完整接收再整体拷贝
}
```

**响应路径优化——SHM Bytes 封装**：

```rust
// 方案 A: 从 SHM 构造 Bytes（需要一次拷贝）
fn shm_to_response(pool: &BuddyPool, alloc: &Alloc, size: usize) -> Response {
    let data = pool.read_bytes(alloc, size); // 1次 SHM → heap 拷贝
    Response::new(Body::from(data))          // Bytes 到 hyper 零拷贝
    // kernel 写入 socket: ③
}

// 方案 B: 自定义 Body 流式从 SHM 读取（仍需拷贝，但分块）
struct ShmBody { pool: Arc<BuddyPool>, alloc: Alloc, offset: usize, size: usize }

impl http_body::Body for ShmBody {
    fn poll_frame(&mut self, ..) -> Frame<Bytes> {
        let chunk_size = min(65536, self.size - self.offset);
        let chunk = self.pool.read_bytes_at(self.alloc, self.offset, chunk_size);
        self.offset += chunk_size;
        Frame::data(Bytes::from(chunk)) // 分块拷贝，内存友好
    }
}
```

**Rust 优化方案总计**: 请求 2 次，响应 2 次，共 **4 次拷贝**——但每次拷贝都是高效的：
- 拷贝操作是 **streaming** 的（不需要先完整接收再整体拷贝）
- 可以利用 Rust 的 SIMD `memcpy` 优化
- SHM 是 mmap 内存，对 CPU cache 友好

#### 5.4 理论最优方案（io_uring ZC Rx + vmsplice）

```
Request:  NIC → DMA直入用户内存 → 用户内存=SHM → CRM读取
          拷贝:  零(DMA)            零               零
          
Response: CRM写入SHM → vmsplice→pipe→splice→socket → NIC
          拷贝:           零(页表操作)                零(DMA)
```

**理论最优**: 请求 0 次用户态拷贝，响应 0 次用户态拷贝。
但需要 Linux 6.15+ 内核、支持 header/data split 的 NIC、以及完全自定义的 I/O 层。

### 六、性能影响量化估计

虽然有 4 次拷贝，但让我们量化这些拷贝的实际性能影响：

#### 内存拷贝带宽参考值（现代 x86_64）

| Payload Size | memcpy 耗时（单核） | 内存带宽利用 | 相对于网络延迟 |
|-------------|---------------------|-------------|---------------|
| 64 B | < 50 ns | N/A | 可忽略 |
| 4 KB | ~200 ns | ~20 GB/s | 可忽略 |
| 1 MB | ~50 μs | ~20 GB/s | < 1% of RTT |
| 10 MB | ~500 μs | ~20 GB/s | ~5% of RTT |
| 100 MB | ~5 ms | ~20 GB/s | ~50% of RTT |
| 1 GB | ~50 ms | ~20 GB/s | > RTT |

（注：RTT 按典型数据中心 ~10ms 估算）

#### C-Two 典型场景分析

| 场景 | Payload 大小 | 拷贝开销(4次) | 网络传输时间(1Gbps) | 拷贝占比 |
|------|-------------|--------------|-------------------|---------|
| API 调用 | 64B-4KB | < 1μs | ~40μs | < 3% |
| 中型数据 | 100KB-1MB | ~100μs | ~8ms | ~1% |
| GIS Tile | 10MB-50MB | ~5ms | ~400ms | ~1% |
| 大型网格 | 100MB-1GB | ~50-200ms | ~4s | ~5% |

**关键结论**：对于通过 HTTP 传输的数据（意味着要跨网络），**memcpy 开销在绝大多数场景下不是瓶颈**。网络传输时间是主导因素。

**例外情况**：同节点 HTTP relay（Routing Server 和 CRM Server 在同一机器）。此时网络延迟近零，memcpy 成为主要开销。但即使如此：
- 10MB: 4次 memcpy ≈ 2ms，仍然可接受
- 100MB+: 考虑使用 Content-Length 分流，大 payload 直接走 IPC-v2（绕过 HTTP）

---

## Decision

### Recommendation

**采用"标准 Rust 方案 + 流式 SHM 写入"，总计 4 次拷贝，不追求内核级零拷贝。**

具体实现策略分为三层：

#### 第一层：Baseline（立即实现）

```
HTTP Request → hyper Bytes → streaming memcpy → Buddy SHM → UDS控制帧 → CRM
HTTP Response ← hyper Bytes ← memcpy ← Buddy SHM ← UDS控制帧 ← CRM
```

- 使用 **axum + hyper + tokio** 标准栈
- 请求 body 流式写入预分配的 SHM block
- 响应从 SHM 读取后构造 `Bytes` 返回
- **4 次拷贝**，但每次都是 Rust 优化的 SIMD memcpy
- 预期吞吐量：50K-200K req/s（小 payload），数 GB/s（大 payload）

#### 第二层：小 Payload 内联优化（紧随实现）

```
对于 payload < 4KB (shm_threshold):
HTTP Request → hyper Bytes → 直接内联在UDS帧中 → CRM
HTTP Response ← hyper Bytes ← 直接内联在UDS帧中 ← CRM
```

- 复用 IPC v3 的 inline 路径，**跳过 SHM 分配和写入**
- 小 payload（API 调用、元数据查询）仅 **2 次拷贝**
- 绝大多数 API 调用都走这条快速路径

#### 第三层：大 Payload 流式优化（渐进实现）

```
对于 payload > 1MB:
请求: hyper chunk → 直接追加写入SHM block（避免内存中完整缓冲）
响应: SHM block → 分块构造 Body stream（避免一次性复制到堆）
```

- 自定义 `http_body::Body` 实现，分块从 SHM 读取响应
- 请求 body streaming 直接写入 SHM，不在用户态缓冲完整 payload
- 内存占用从 O(payload_size) 降为 O(chunk_size)

### Rationale

1. **实用主义优于理论完美**：4 次拷贝在 HTTP 场景下的实际性能影响 < 5%。追求 0 拷贝的工程代价（自定义 I/O 层、内核版本要求、硬件要求）与收益不成比例。

2. **与 SOTA 设计一致**：sota.md 附录明确指出 "纯字节透传" 和 "axum (基于 hyper + tokio, 零拷贝 body 处理)"。这里的"零拷贝"指的是 hyper 内部的 body chunk 传递（Bytes 引用计数），不是内核级零拷贝。

3. **工业界验证**：Ray、gRPC、iceoryx 等主流系统均接受"网络 I/O 需要拷贝"的现实，转而优化**拷贝次数**和**拷贝效率**。C-Two 采用同样策略。

4. **渐进式优化路径清晰**：
   - 短期：标准方案（4 次拷贝）→ 大幅超越 Python 方案（~8 次拷贝）
   - 中期：vmsplice 响应路径 PoC → 可能减少到 3 次
   - 长期：io_uring ZC Rx → 可能减少到 1 次

5. **buddy allocator 的天然优势**：C-Two 的 buddy SHM 已为零拷贝 IPC 提供了基础。Routing Server 作为 SHM 的另一个 writer/reader，本质上与 SharedClient 在同一个零拷贝框架内。真正消耗大的 CRM 计算结果（GB 级 mesh 数据）在 CRM→SHM 写入时已完成，Routing Server 只是搬运。

### Implementation Notes

#### 关键实现要点

1. **Content-Length 预分配**：如果 HTTP 请求带 `Content-Length`，一次性 `alloc(content_length)` SHM block，避免多次分配和碎片。

2. **Chunked Transfer 的 SHM 写入策略**：
   - 初始分配一个合理大小的 block（如 64KB）
   - 如果不够，释放旧 block，重新分配 2x 大小
   - 或者使用 dedicated segment 回退

3. **SHM memoryview 生命周期管理**：
   - 响应的 SHM block 必须在 HTTP 响应完全发送后才能释放
   - 实现 deferred free：在 `Body::poll_frame()` 返回 EOF 时触发 `free_at()`
   - 参考 Python 端 `_deferred_response_free` 的设计

4. **连接池**：Routing Server 维护到各 CRM Server 的持久 IPC 连接池（bb8 或自建），避免每次 relay 创建/销毁连接。

5. **Wire v2 透传**：Routing Server 不需要解析 wire 内容（method name、error code），只需要：
   - 从 HTTP body 读取原始 bytes → 写入 SHM
   - 发送 UDS 控制帧（route name + method index 由 HTTP URL path 或 header 提供）
   - 等待 UDS 响应控制帧
   - 从 SHM 读取响应 bytes → 写入 HTTP response body

#### 架构示意

```
                    ┌─────────────────────────────────────────┐
                    │         Rust Routing Server              │
                    │                                         │
  HTTP Client ──────│──▶ axum handler                         │
       ▲            │       │                                 │
       │            │       │ Bytes (hyper零拷贝引用)          │
       │            │       ▼                                 │
       │            │   streaming memcpy                      │
       │            │       │                                 │
       │            │       ▼                                 │
       │            │   ┌─────────────┐   UDS控制帧(11B)      │
       │            │   │ Buddy SHM   │──────────────────────▶│──▶ CRM Server
       │            │   │ Pool (mmap) │◀──────────────────────│◀── (Python)
       │            │   └─────────────┘   UDS响应帧(11B)      │
       │            │       │                                 │
       │            │       │ memcpy (分块读取)                │
       │            │       ▼                                 │
       │            │   Bytes → Response                      │
  HTTP Client ◀─────│──◀ axum response                        │
                    └─────────────────────────────────────────┘
```

### Follow-up Actions

- [ ] 实现 Rust Routing Server 的 baseline 版本（axum + c2-ipc crate）
- [ ] 基准测试：Python router vs Rust router 在 64B-1GB payload 范围的延迟和吞吐量对比
- [ ] PoC: 验证 vmsplice+splice 在响应路径上的可行性和性能收益
- [ ] 评估 io_uring ZC Rx 的长期集成路线图
- [ ] 实现 SHM Body streaming（自定义 `http_body::Body`）优化大 payload 的内存占用

---

### External Resources

- [io_uring zero copy Rx — Linux Kernel Documentation](https://docs.kernel.org/next/networking/iou-zcrx.html)
- [Efficient zero-copy networking using io_uring — Kernel Recipes 2024](https://kernel-recipes.org/en/2024/schedule/efficient-zero-copy-networking-using-io_uring/)
- [tokio-uring fixed buffers API](https://docs.rs/tokio-uring/latest/tokio_uring/buf/fixed/index.html)
- [hyper::body — Rust Docs](https://docs.rs/hyper/latest/hyper/body/)
- [Implementing True Zero-Copy Communication with iceoryx2](https://ekxide.io/blog/how-to-implement-zero-copy-communication/)
- [Introducing Shmipc — CloudWeGo](https://www.cloudwego.io/blog/2023/04/04/introducing-shmipc-a-high-performance-inter-process-communication-library/)
- [Ray Plasma Object Store](https://ray-core.readthedocs.io/en/snapshot/plasma.html)
- [RDMA Explained — DigitalOcean](https://www.digitalocean.com/community/conceptual-articles/rdma-high-performance-networking)
- [Zero-Copy I/O Techniques — Codemia](https://codemia.io/blog/path/Zero-Copy-IO-From-sendfile-to-iouring--Evolution-and-Impact-on-Latency-in-Distributed-Logs)
- [Achieving Zero-Copy Data Parsing in Rust Web Services](https://leapcell.io/blog/achieving-zero-copy-data-parsing-in-rust-web-services-for-enhanced-performance)
- [Linux Zero-Copy: vmsplice Between Processes](https://linuxvox.com/blog/linux-zero-copy-transfer-memory-pages-between-two-processes-with-vmsplice/)
- [Achieving Zero-copy Serialization for Datacenter RPC (IEEE)](https://ieeexplore.ieee.org/document/10253859)

---

## Status History

| Date | Status | Notes |
|------|--------|-------|
| 2026-03-27 | 🔴 Not Started | Spike created and scoped |
| 2026-03-27 | 🟡 In Progress | Research commenced: web search + codebase analysis |
| 2026-03-27 | 🟢 Complete | 结论：采用标准 Rust 方案（4 次拷贝），流式 SHM 写入，不追求内核级零拷贝。工业界验证此为最佳实践。 |

---

_Last updated: 2026-03-27 by soku_
