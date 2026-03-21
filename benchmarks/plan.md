# Memory RPC 优化计划

## 基准测试结果

> 测试环境：macOS / Apple M 系列芯片（内存带宽 ~400GB/s）

### 延迟测试 (ms) — 64B → 1GB 全量覆盖

| 协议 | 负载 | Rounds | P50 | P95 | P99 | Max | 有效带宽 |
|------|------|--------|-----|-----|-----|-----|---------|
| thread | 64B | 100 | 0.025 | 0.032 | 0.038 | 0.042 | 4.7 MB/s |
| thread | 1KB | 100 | 0.034 | 0.050 | 0.091 | 0.091 | 55.9 MB/s |
| thread | 64KB | 100 | 0.045 | 0.068 | 0.078 | 0.080 | 2.6 GB/s |
| thread | 1MB | 100 | 0.214 | 0.230 | 0.234 | 0.294 | **9.0 GB/s** |
| thread | 10MB | 20 | 1.944 | 2.160 | 2.570 | 2.672 | **9.9 GB/s** |
| thread | 100MB | 5 | 13.719 | 32.607 | 36.247 | 37.157 | **10.5 GB/s** |
| thread | 512MB | 3 | 89.877 | — | 243.337 | 246.469 | **7.3 GB/s** |
| thread | 1GB | 3 | 577.815 | — | 725.213 | 728.222 | **3.6 GB/s** |
| **memory** | **64B** | 100 | **30.972** | 38.928 | 41.031 | 41.856 | ~0 |
| **memory** | **1KB** | 100 | **28.033** | 37.387 | 40.294 | 45.097 | 0.1 MB/s |
| **memory** | **64KB** | 100 | **27.988** | 38.155 | 40.063 | 40.125 | 4.2 MB/s |
| **memory** | **1MB** | 100 | **34.551** | 37.637 | 41.325 | 57.316 | 58.6 MB/s |
| **memory** | **10MB** | 20 | **60.807** | 63.062 | 63.123 | 63.138 | 366 MB/s |
| **memory** | **100MB** | 5 | **206.687** | 227.351 | 230.666 | 231.495 | **948 MB/s** |
| **memory** | **512MB** | 3 | **1,019.644** | — | 1,049.659 | 1,050.271 | **1.0 GB/s** |
| **memory** | **1GB** | 3 | **2,641.720** | — | 3,328.785 | 3,342.807 | **774 MB/s** |

### 对比倍率与瓶颈分析

| 负载 | thread P50 | memory P50 | 倍率 | memory 固定开销占比 | 瓶颈类型 |
|------|-----------|-----------|------|-------------------|---------|
| 64B | 0.025ms | 30.97ms | **1,264x** | 100% | 纯协议开销 |
| 1KB | 0.034ms | 28.03ms | **832x** | 100% | 纯协议开销 |
| 64KB | 0.045ms | 27.99ms | **623x** | 100% | 纯协议开销 |
| 1MB | 0.214ms | 34.55ms | **161x** | 89.6% | 协议开销为主 |
| 10MB | 1.944ms | 60.81ms | **31x** | 50.9% | **混合瓶颈** |
| 100MB | 13.72ms | 206.69ms | **15x** | 15.0% | 数据传输为主 |
| 512MB | 89.88ms | 1,019.64ms | **11x** | ~3% | **文件 I/O 天花板** |
| 1GB | 577.82ms | 2,641.72ms | **4.6x** | ~1% | **文件 I/O 天花板** |

### 关键发现

1. **双重瓶颈，随负载大小切换**：
   - **小消息（≤64KB）**：延迟被 ~28ms 固定开销完全支配（轮询 + watchdog + 文件创建/删除）
   - **大消息（≥100MB）**：数据传输成为主要瓶颈，memory 带宽被钉在 **~1 GB/s**

2. **Memory 协议带宽天花板 ~1 GB/s**：
   - Thread 在 1-100MB 区间可达 9-10 GB/s（纯内存拷贝）
   - Memory 在 100MB-1GB 区间稳定在 ~1 GB/s — **这是文件系统 I/O 的物理极限**
   - 即使 M 系列芯片内存带宽 400GB/s，文件系统路径也无法逾越 ~1 GB/s

3. **Thread 协议在 1GB 时下降到 3.6 GB/s**：峰值 RSS 达 9.7GB，内存压力导致性能衰退

4. **瓶颈交叉点 ~10MB**：此处固定开销和数据传输各占 50%，是优化策略分界线

---

## 当前实现分析

### 架构概述

Memory 协议通过临时目录中的文件实现跨进程 IPC：

```
Client → 写请求文件 (.temp → .mem)
Server → watchdog 检测 → mmap 读取 → 删除请求文件
Server → 写响应文件 (.temp → .mem)
Client → 轮询检测 → mmap 读取 → 删除响应文件
```

### 每次 Round-Trip 的开销

| 步骤 | 操作 | 端 |
|------|------|------|
| 1. 序列化 + 写请求 | open, truncate, mmap, write, flush, close, rename | Client |
| 2. 检测请求到达 | watchdog on_moved → condition.notify | Server |
| 3. 扫描目录 + 查找文件 | os.listdir, stat 每个文件, 按 mtime 排序 | Server |
| 4. 读取请求 | open, mmap READ, read, close, unlink | Server |
| 5. 序列化 + 写响应 | open, truncate, mmap, write, flush, close, rename | Server |
| 6. 轮询响应存在性 | path.exists() 循环 (1ms→100ms 指数退避) | Client |
| 7. 读取响应 | open, mmap READ, read, close, unlink | Client |

**每次 round-trip 约 14 次文件系统调用**，加上 watchdog 事件传播延迟和轮询退避。

### 核心瓶颈

1. **每消息的文件 I/O**：14 次 syscall（create, mmap, rename, read, delete × 2）
2. **Client 轮询退避**：1ms 起始 → 1.5x 增长 → 100ms 封顶。约 15 次迭代后 client 每次轮询休眠 100ms — 延迟悬崖
3. **Server 目录扫描**：`os.listdir` + 每文件 `stat` — O(n) 待处理文件数
4. **Watchdog 延迟**：OS 相关的文件事件通知延迟（macOS FSEvents 可达 10-50ms）
5. **无数据复用**：每条消息创建+删除唯一文件，无重用

### 信号机制不对称

- **Server 端**：watchdog（被动，事件驱动）— 相对好
- **Client 端**：纯自旋轮询 + 指数退避 — 延迟和 CPU 都差

---

## 优化方案分析

### 需求约束

1. **跨平台**：Linux / macOS / Windows 均需支持
2. **数据规模**：从几十字节到 10GB+（必须覆盖大数据量）
3. **向后兼容**：保持 `memory://` 协议地址格式和 BaseClient/BaseServer 接口
4. **同机跨进程**：Server 和 Client 在同一台机器的不同进程中

### 方案对比

#### 方案 A：Unix Domain Socket（Linux/macOS）+ Named Pipe / TCP Loopback（Windows）

**原理**：用 `socket.AF_UNIX` 流式套接字替代文件，复用已有的 `add_length_prefix` / `parse_message` 帧协议。

| 维度 | 评估 |
|------|------|
| 小消息延迟 | ⭐⭐⭐⭐⭐ 无文件 I/O，无轮询，内核直接通知 |
| 大数据吞吐 | ⭐⭐⭐ 数据经过 socket buffer 拷贝，10GB 级别效率一般 |
| 信号机制 | 阻塞 recv — 无需轮询 |
| 跨平台 | Linux/macOS 原生；Windows 需回退到 TCP 127.0.0.1 或 Named Pipe |
| 复杂度 | 低 — 现有 TCP 实现可大量复用 |

**大数据问题**：10GB 数据通过 socket 传输时，数据会经过：用户空间→内核→用户空间（两次拷贝）。吞吐受 socket buffer 大小限制，但可通过调整 `SO_SNDBUF`/`SO_RCVBUF` 优化。

#### 方案 B：`multiprocessing.shared_memory` + 信号量

**原理**：预分配共享内存区域用于数据传输，配合信号量/Event 用于同步通知。

| 维度 | 评估 |
|------|------|
| 小消息延迟 | ⭐⭐⭐⭐ 内存拷贝 + 信号量唤醒 |
| 大数据吞吐 | ⭐⭐⭐⭐⭐ 零拷贝（生产者写入后消费者直接读取同一块内存） |
| 信号机制 | 信号量/Event — 无需轮询 |
| 跨平台 | Python 3.8+ 全平台支持（`multiprocessing.shared_memory`） |
| 复杂度 | 高 — 需要管理缓冲区大小、环形队列、并发控制 |

**大数据方案**：
- **小消息（< 阈值，如 64MB）**：使用固定大小的共享内存环形缓冲区
- **大消息（≥ 阈值）**：动态分配专用 `SharedMemory` 块，通过控制通道传递块名称
- 生产者写入后通过信号量通知消费者，消费者直接从同一物理内存页读取 — **真正的零拷贝**

#### 方案 C：mmap 文件 + 信号量（当前方案的增量优化）

**原理**：保留 mmap 文件方式，但用信号量/Event 替代 watchdog + 轮询，用固定文件替代每消息创建/删除。

| 维度 | 评估 |
|------|------|
| 小消息延迟 | ⭐⭐⭐⭐ 消除轮询和 watchdog 延迟 |
| 大数据吞吐 | ⭐⭐⭐⭐ mmap 已有良好的大文件支持 |
| 信号机制 | 信号量/Event — 无需轮询 |
| 跨平台 | 全平台（mmap 和信号量均跨平台） |
| 复杂度 | 中 — 增量改动，不需要重写整个传输层 |

**大数据方案**：mmap 天然支持大文件，OS 自动管理物理内存页映射，可处理超大文件。但仍涉及文件系统开销。

### 综合推荐

**推荐分层方案（Hybrid）**：

```
┌──────────────────────────────────────────────────────────┐
│                    Memory Protocol v2                      │
├──────────────────────────────────────────────────────────┤
│  控制通道 (Control Channel)                                │
│  ├─ Linux/macOS: Unix Domain Socket                       │
│  └─ Windows: TCP 127.0.0.1 (或 Named Pipe)               │
│  用途：消息帧传递、信号通知、小消息直传                      │
├──────────────────────────────────────────────────────────┤
│  数据通道 (Data Channel) — 仅大消息                        │
│  ├─ multiprocessing.shared_memory (首选)                   │
│  └─ mmap 文件 (回退)                                      │
│  用途：大数据块的零拷贝传输                                 │
├──────────────────────────────────────────────────────────┤
│  协议：                                                    │
│  ├─ payload < THRESHOLD → 直接通过控制通道发送               │
│  └─ payload ≥ THRESHOLD → 数据写入共享内存，                │
│       控制通道仅发送 {shm_name, offset, length}             │
└──────────────────────────────────────────────────────────┘
```

**优势**：
- 小消息：Socket 直传，延迟极低（< 0.1ms）
- 大消息：共享内存零拷贝，吞吐最优
- 无轮询：Socket 阻塞读 + 信号量，CPU 友好
- 跨平台：Socket 层自动选择平台最优实现
- 渐进式：可先实现 Socket 控制通道，后续加入共享内存数据通道

**阈值建议**：默认 64KB（可配置）。小于此值走 socket，大于此值走共享内存。

---

## 实施路线

### Phase 1：基准测试（当前）
创建 benchmark 量化当前问题，为优化提供数据基础。

### Phase 2：Socket 控制通道
替换文件 I/O 和轮询为 socket 通信，消除核心瓶颈。
- 所有大小的消息均通过 socket 传输
- 消除 watchdog 依赖
- 消除 client 轮询

### Phase 3：共享内存数据通道（大数据优化）
对超过阈值的大消息引入 `SharedMemory` 通道。
- 控制通道传递 shm 元数据
- 数据通道实现零拷贝

### Phase 4：基准对比
重新运行 benchmark，对比优化前后性能。

---

## Memory Protocol v2 详细优化设计（基于现有 benchmark）

> 目标：在不改变 `memory://` 地址语义、不破坏 `BaseClient/BaseServer` 接口的前提下，消除小消息固定开销，并打破大消息 ~1GB/s 文件 I/O 天花板。

### 一、量化目标（可验收）

> 说明：目标按阶段定义。先保证“确定可达”的下限，再根据 SharedMemory 基线实测决定上限，不在设计阶段直接承诺固定峰值。

| 指标 | 当前（v1） | Phase 2A（控制面 socket 化） | Phase 3A（SHM 单块） | Phase 3B（SHM 分块, 10GB+） |
|------|-----------|----------------------------|----------------------|-----------------------------|
| 64B P50 | ~31ms | **< 1ms** | 保持 | 保持 |
| 1MB P50 | ~35ms | **< 8ms** | **< 5ms** | 保持 |
| 100MB 有效带宽 | ~0.95GB/s | >= v1（不退化） | **>= 2x v1（>1.9GB/s）** | 继续优化 |
| 1GB 有效带宽 | ~0.77GB/s | >= v1（不退化） | **>= 1.5x v1（>1.15GB/s）** | **>= 2x v1（>1.54GB/s）** |
| CPU 空转 | 高（轮询） | **低（阻塞等待）** | 低 | 低 |
| 可靠性 | 文件事件+轮询脆弱 | 控制面稳定 | 数据面稳定 | 大数据分块稳定 |

附加验收原则（必须满足）：

1. **绝不退化**：任何 payload 在 `v2 auto` 下不低于 v1；
2. **可解释性**：输出 `control_rtt_ms` 与 `data_plane_bw`，能区分控制面与数据面瓶颈；
3. **上限声明受基线约束**：若 SharedMemory 基线不足，则自动下修目标，不硬性追 3~4GB/s。

### 二、双平面架构（Control Plane + Data Plane）

#### 2.1 控制平面（Control Plane）

负责：请求调度、信号通知、小消息直传、错误/关闭握手。

- Linux/macOS：`AF_UNIX` stream socket
- Windows：TCP loopback（`127.0.0.1` 随机端口）
- 通信方式：阻塞 `recv` + 长度前缀帧（复用 `add_length_prefix` / `parse_message`）
- 安全：服务端在控制元数据写入一次性 `auth_token`，客户端首帧携带 token 完成认证

#### 2.2 数据平面（Data Plane）

负责：大消息高吞吐传输。

- 首选：`multiprocessing.shared_memory`
- 回退：`mmap` 固定文件池（仅在 SharedMemory 不可用时启用）
- 仅传输数据，不承载调度信号；调度仍由控制平面负责

### 三、分流策略（关键）

基于已有数据（~10MB 为瓶颈交叉点），采用双阈值策略：

- `INLINE_THRESHOLD`（默认 1MB）：小于等于该值，直接走控制平面内联传输
- `SHM_THRESHOLD`（默认 8MB）：大于等于该值，强制走 SharedMemory
- `(1MB, 8MB)` 区间：自适应（根据最近窗口 RTT / 带宽选择）

默认建议：

```text
<= 1MB      : inline socket
1MB~8MB     : adaptive
>= 8MB      : shared memory
```

原因：1MB 仍明显受固定开销影响；10MB 已进入混合瓶颈区，8MB 作为保守切点更稳。

### 四、协议帧设计（v2）

#### 4.1 统一帧头（控制平面）

```text
MAGIC(4) | VERSION(1) | FLAGS(1) | TYPE(1) | RESERVED(1)
REQUEST_ID_LEN(2) | REQUEST_ID(N)
META_LEN(8) | META(JSON/BINARY)
PAYLOAD_LEN(8) | PAYLOAD(optional inline bytes)
```

- `MAGIC`：`C2M2`
- `VERSION`：`2`
- `TYPE`：`PING/CALL/REPLY/SHUTDOWN/ACK/ERROR`
- `FLAGS`：`INLINE=0x01`, `SHM_REF=0x02`, `COMPRESSED=0x04`（预留）

#### 4.2 CALL 元数据

```json
{
  "method": "echo",
  "encoding": "raw",
  "payload_mode": "inline|shm",
  "payload_bytes": 10485760,
  "shm": {
    "name": "c2m_region_pid_req",
    "offset": 0,
    "length": 10485760
  }
}
```

#### 4.3 REPLY 元数据

保持与现有 `CRM_REPLY` 语义一致：`error_bytes + result_bytes`，只是承载路径改为 inline/shm 二选一。

### 五、请求生命周期（状态机）

#### 5.1 Client 状态机

`INIT -> CONNECTED -> REQUEST_SENT -> WAIT_REPLY -> DONE/ERROR`

- `REQUEST_SENT` 后阻塞等待 socket 可读（无轮询 sleep）
- 超时直接抛 `CompoClientError`（保留现有错误语义）

#### 5.2 Server 状态机

`BOOT -> LISTENING -> DISPATCHING -> SHUTTING_DOWN -> STOPPED`

- 控制平面收包后立即入 `EventQueue`
- 继续复用 `server.py` 现有 `_serve` / `_process_event_and_continue` 流程
- 因此业务层（CRM/ICRM）不需要改动

### 六、SharedMemory 细节设计

#### 6.1 分配策略

- 小中消息（`< 64MB`）：共享内存池（分段复用，降低创建开销）
- 超大消息（`>= 64MB`）：按请求临时分配专用块，回复后回收

#### 6.2 生命周期与回收

1. 生产者创建 `SharedMemory(name, create=True, size=N)`
2. 写入后发送 descriptor（name/offset/length/checksum 可选）
3. 消费者 attach + 读取后发送 `DATA_CONSUMED`
4. 生产者收到 ACK 后 `close + unlink`

异常兜底：

- 服务端启动时扫描并清理过期 `c2m_*` 残留段（基于 pid + timestamp）
- 客户端崩溃时由 server GC 定期回收超时未 ACK 的块

#### 6.3 10GB+ 数据支持

- 不要求单块 10GB 一次性分配
- 使用 chunk descriptor 列表（默认串行消费，预留并行窗口）：

```json
{
  "payload_mode": "shm_chunks",
  "chunks": [
    {"name": "c2m_x_1", "offset": 0, "length": 1073741824},
    {"name": "c2m_x_2", "offset": 0, "length": 1073741824}
  ],
  "total_length": 2147483648
}
```

避免单次超大分配导致 RSS 峰值和分配失败。

#### 6.4 传输模式（串行优先，避免过度复杂化）

- **默认：串行分块（稳定优先）**
  - 控制简单，内存峰值可预测；
  - 容易做 ACK 与回收，不易泄漏。
- **可选：有限并行（实验特性）**
  - 仅在基线表明有效收益时启用；
  - 并发窗口默认 `2~4`，并增加背压与超时回收。

### 七、兼容与灰度策略

#### 7.1 对外兼容

- 地址不变：`memory://<region_id>`
- `Client` / `Server` 公共接口不变
- `EventTag` / `CCError` 上层语义不变

#### 7.2 协议协商

- `HELLO` 帧交换能力：`{"versions":[1,2], "features":["inline","shm"]}`
- 若任一端不支持 v2，自动回落到 v1（现有文件方案）
- 增加开关：
  - `C_TWO_MEMORY_PROTOCOL=v2|v1|auto`（默认 `auto`）
  - `C_TWO_MEMORY_INLINE_THRESHOLD`（默认 `1MB`）
  - `C_TWO_MEMORY_SHM_THRESHOLD`（默认 `8MB`）
  - `C_TWO_MEMORY_SHM_CHUNK_SIZE`（默认 `256MB`）

### 八、代码落点（文件级）

#### 新增

- `src/c_two/rpc/memory/v2/control_channel.py`
- `src/c_two/rpc/memory/v2/data_plane.py`
- `src/c_two/rpc/memory/v2/frame.py`
- `src/c_two/rpc/memory/v2/gc.py`
- `src/c_two/rpc/memory/v2/config.py`

#### 修改

- `src/c_two/rpc/memory/memory_client.py`：增加 v1/v2 backend 选择
- `src/c_two/rpc/memory/memory_server.py`：接入 v2 accept loop 与 descriptor 处理
- `src/c_two/rpc/memory/memory_routing.py`：改为优先走 v2 relay
- `src/c_two/rpc/client.py`：`memory://` 保持入口不变，仅内部选择 backend

### 九、风险与对策

| 风险 | 描述 | 对策 |
|------|------|------|
| SharedMemory 泄漏 | 进程异常退出导致未 unlink | 启动清理 + 周期 GC + TTL |
| Windows 行为差异 | AF_UNIX 支持不一致 | 统一走 TCP loopback |
| 超大 payload 内存峰值 | 单块分配失败 | chunk 化传输 + 限制单块上限 |
| 回退路径复杂 | v1/v2 双栈维护成本 | 明确 capability 协商 + 统一测试矩阵 |
| 目标过高不可达 | 实际 SHM 上限低于预期 | 先跑基线，目标按实测动态下修 |

### 十、验证计划（与 benchmark 对齐）

#### 10.1 功能验证

- 单元：frame 编解码、descriptor 校验、GC 逻辑
- 集成：`ping/call/relay/shutdown` 在 v2 下全覆盖
- 兼容：v2 client ↔ v1 server 自动回退

#### 10.2 性能验证

复用 `benchmarks/memory_benchmark.py`，新增：

- `--memory-backend v1|v2|auto`
- 输出分项指标：`control_rtt_ms`、`data_copy_ms`、`gc_overhead_ms`
- 增加 `--micro shm-copy`（纯 SharedMemory 读写基线）与 `--micro mmap-copy`（回退基线）

新增“上限校准”规则：

1. 先测纯 SHM 基线带宽 `BW_shm_raw`；
2. RPC 目标上限设为 `0.4~0.6 * BW_shm_raw`（留出序列化/调度开销）；
3. 若 `BW_shm_raw` 低于预期，则同步下修 100MB/1GB 目标，不制造不可达 KPI。

核心验收点：

1. 小消息（64B/1KB）P50 显著低于 1ms
2. 100MB~1GB 带宽相对 v1 显著提升（至少达到阶段目标）
3. 10GB 级分块传输可完成且无泄漏

### 十一、实施顺序（工程化）

1. **Phase 2A**：先落地 v2 控制平面（全部 inline，先不引入 SHM）  
2. **Phase 2B**：引入能力协商 + v1 自动回退  
3. **Phase 3A**：接入 SharedMemory 单块传输（>= 8MB）  
4. **Phase 3B**：接入 chunk 传输（10GB+）与 GC  
5. **Phase 4**：全量 benchmark 回归与阈值调优（1MB/8MB/16MB 对比）

### 十二、实施前补充实验（防止设计脱离现实）

在开始编码前先补 3 个微基准：

1. **SHM 原始读写基线**
   - 64MB / 256MB / 1GB 块；
   - 指标：单次写+读耗时、稳态带宽、RSS 峰值。

2. **控制面往返基线**
   - AF_UNIX / TCP loopback 的空载 RTT（64B 帧）；
   - 指标：P50/P99，验证 `<1ms` 目标是否合理。

3. **分块策略评估**
   - 串行 chunk vs 并行窗口（2/4）；
   - 指标：总耗时、峰值内存、失败恢复复杂度。

实验结论将直接决定：

- `SHM_THRESHOLD`（8MB 是否调整到 16MB）；
- `CHUNK_SIZE`（256MB/512MB）；
- 是否启用并行分块（默认不启用）。
