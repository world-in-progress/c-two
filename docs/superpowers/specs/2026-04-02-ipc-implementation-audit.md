# IPC 实现审计报告

> **日期**: 2026-04-02
> **分支**: `fix/ipc-perf-regression`
> **背景**: v0.4.0 Rust Transport Sink 迁移后的全面实现审查

---

## 一、审计范围

本报告审查以下系统在当前代码中的实现完整性：

1. **内存分层策略** (T1→T2→T3→T4)
2. **Payload 智能路由** (inline / buddy / chunked)
3. **请求池与响应池对称性**
4. **Buddy 段生命周期管理**
5. **小负载性能差距根因**

---

## 二、内存分层策略 (MemPool)

### 2.1 设计目标

四层级内存分配，逐级降级：

| 层级 | 策略 | 触发条件 |
|:----:|------|---------|
| T1 | 在已有 buddy segment 上快速分配 | 默认路径，无 RAM 检查 |
| T2 | 创建新 buddy segment + 空闲后延迟回收 | 现有段空间不足 |
| T3 | Dedicated segment（超大块，用完即回收） | 请求超过 buddy 最大块 |
| T4 | 磁盘溢写 (mmap 临时文件) | RAM 不足时兜底 |

### 2.2 实现状态

| 层级 | 代码位置 | 实现 | 生产可达 | 问题 |
|:----:|---------|:----:|:--------:|------|
| T1 | `pool.rs:587-607` `alloc_buddy()` 遍历已有段 | ✅ | ✅ | 无 |
| T2 | `pool.rs:609-624` `alloc_buddy()` 创建新段 | ✅ | ✅ | GC 不自动触发 |
| T3 | `pool.rs:630-681` `alloc_dedicated()` | ✅ | ✅ | `gc_dedicated()` 仅在分配时触发 |
| T4 | `spill.rs` `should_spill()` + `create_file_spill()` | ✅ | ❌ | server/client 不可达 |

### 2.3 关键缺陷

#### 🔴 缺陷 A：Buddy 段自动回收机制未接入

**设计意图**：消费方读完数据后显式 `free_at()` → buddy 合并 → 检测 `alloc_count == 0`
→ 标记 `idle_since` → 延迟 N 秒后自动销毁空闲段。

**实际状态**：

- ✅ `free_at()` 正确标记 `idle_since`（`pool.rs:399-402`）
- ✅ `gc_buddy()` 正确检查延迟并销毁（`pool.rs:232-260`）
- ❌ **`gc_buddy()` 从未被自动调用** — 无定时器、无后台任务、`free_at()` 不触发
- ❌ `alloc_buddy()` 扩展前不尝试先回收空闲段

**调用点**：仅 `mem_ffi.rs:439`（Python `.gc()` 方法）和测试代码。

**影响**：长时间运行的进程会不断积累空 SHM 段而永不释放。

#### 🟡 缺陷 B：T4 磁盘溢写路径不可达

`alloc_handle()` 包含完整的 `should_spill()` → `create_file_spill()` 逻辑（`pool.rs:461-517`），
但 server (`server.rs:808`) 和 client (`client.rs:545`) 都调用 `alloc()` 而非 `alloc_handle()`。

`alloc()` 的链路是 T1→T2→T3，**跳过了 T4**。
仅 `ChunkAssembler`（`assembler.rs:74`）使用 `alloc_handle()` 可触发溢写。

---

## 三、Payload 智能路由

### 3.1 请求路径 (Client → Server)

FFI 入口：`client_ffi.rs` → `SyncClient::call()` → `IpcClient::call_full()` ✅ 智能路由已接入。

```
call_full() 决策树:
  pool 存在 AND data.len() > shm_threshold (4KB)?
    YES → call_buddy()           // buddy SHM 路径
          失败 → fall through
  data.len() > chunk_size (128KB)?
    YES → call_chunked()         // 分片传输
  else → call_inline()           // 内联 UDS
```

| 路径 | 触发条件 | 实现 | 生产可达 |
|------|---------|:----:|:--------:|
| inline | < 4KB | ✅ | ✅ |
| buddy | 4KB ~ 256MB | ✅ | ✅ |
| chunked | buddy 分配失败，或 > 128KB | ✅ | ✅（降级路径） |

### 3.2 响应路径 (Server → Client)

所有三种 dispatch 函数统一调用 `smart_reply_with_data()`。

```
smart_reply_with_data() 决策树:
  data.len() > shm_threshold (4KB)?
    YES → write_buddy_reply_with_data()    // buddy SHM
          分配失败 → fallback inline
    NO  → write_reply_with_data()          // 内联 UDS
```

| 路径 | 触发条件 | 实现 | 生产可达 |
|------|---------|:----:|:--------:|
| inline | < 4KB | ✅ | ✅ |
| buddy | > 4KB | ✅ | ✅ |
| chunked | 超大响应分片发送 | ❌ **未实现** | ❌ |

### 3.3 关键缺陷

#### 🟡 缺陷 C：响应无 chunked 路径

- 服务端无 `write_chunked_reply()` 函数
- 客户端 `decode_response()` 无 chunked 分支
- 最大响应受限于 `response_pool` 总量（默认 4×256MB = 1GB）
- 超过 1GB 的响应会 fallback 到 inline（单帧巨大写入）或分配失败

---

## 四、请求池与响应池对称性

### 4.1 结论：✅ 机制对称，无严重问题

二者共用同一套 `MemPool` + `BuddyAllocator` 实现，遵循相同的
OWNER alloc→write→send / CONSUMER open→read→free 模式。

```
请求方向 (client→server):
  CLIENT (owner):  pool.alloc() → write SHM → send buddy frame
  SERVER (consumer): open_segment(lazy) → read_peer_data() → pool.free_at()

响应方向 (server→client):
  SERVER (owner):  response_pool.alloc() → write SHM → send buddy frame
  CLIENT (consumer): open_segment(lazy) → read_and_free() → pool.free_at()
```

### 4.2 对称性矩阵

| 操作 | 请求路径 | 响应路径 | 对称 |
|------|---------|---------|:----:|
| 分配器 | `pool.alloc()` | `pool.alloc()` | ✅ |
| 写 SHM | `data_ptr() + copy_nonoverlapping` | `data_ptr() + copy_nonoverlapping` | ✅ |
| 帧编码 | `encode_buddy_payload()` | `encode_buddy_payload()` | ✅ |
| 惰性打开 | `PeerShmState.ensure_buddy_segment()` | `ServerPoolState.ensure_buddy_segment()` | ✅ |
| 读取 | `pool.data_ptr_at() + to_vec()` | `pool.data_ptr_at() + to_vec()` | ✅ |
| 释放 | `pool.free_at()` (connection.rs:246) | `pool.free_at()` (client.rs:104) | ✅ |
| 跨进程安全 | AtomicU64 bitmap + ShmSpinlock | 同上 | ✅ |

### 4.3 轻微不对称（仅配置差异，非结构问题）

| 参数 | 请求侧 | 响应侧 |
|------|--------|--------|
| 前缀格式 | `/cc3b{pid:08x}` | `/cc3r{pid:08x}{gen:02x}` |
| max_dedicated_segments | 4 | 8 |
| dedicated_gc_delay_secs | 0.0 | 60.0 |

消费侧 wrapper（`PeerShmState` vs `ServerPoolState`）各写了一份，但底层逻辑一致。

---

## 五、小负载性能差距

### 5.1 现象

64B IPC-bytes：v0.3 0.13ms → v0.4 0.21ms（1.6× 回归）

### 5.2 根因：非路径选择问题，而是 per-call 固定开销

64B 请求和响应均正确走 inline UDS 路径（< 4KB 阈值）。

差距来自 Rust↔Python 边界的数据拷贝和固定开销：

| # | 位置 | 操作 |
|---|------|------|
| 1 | `client_ffi.rs:85` | `data.to_vec()` Python bytes → Rust Vec |
| 2 | `client.rs:503` | 写入栈 buffer |
| 3 | `server_ffi.rs` | `PyBytes::new(py, payload)` Rust → Python |
| 4 | `server_ffi.rs` | CRM 返回 → `bytes.to_vec()` Python → Rust |
| 5 | `server.rs:790` | `Vec::with_capacity` + 2× `extend` |
| 6 | `frame.rs:93` | `encode_frame` 再创一个 Vec |
| 7 | `client.rs:864` | `payload[consumed..].to_vec()` |
| 8 | `client_ffi.rs:90` | `PyBytes::new(py, &bytes)` Rust → Python |

每次 64B 调用约 **7 次堆分配 + 10 次数据拷贝**，加上 tokio oneshot channel、
HashMap insert/remove、3-4 次 Mutex lock。对比 v0.3 约 3 次拷贝。

### 5.3 请求侧 vs 响应侧优化不对称

- **请求侧**（`call_inline`）：frame ≤ 1024B 时使用**栈 buffer**，单次 `write_all`
- **响应侧**（`write_reply_with_data`）：始终使用**堆 Vec**，两次 `extend_from_slice` +
  `encode_frame` 再分配一次

响应侧可以参照请求侧加入栈 buffer 优化。

---

## 六、性能基准 (v0.4 修复后)

基于 `fix/ipc-perf-regression` 分支 8 个 commit 后的测试结果：

| 大小 | Thread (ms) | IPC-bytes (ms) | IPC-dict (ms) | Relay (ms) |
|-----:|:----------:|:--------------:|:-------------:|:----------:|
| 64B | 0.0034 | 0.2100 | 0.3021 | 2.043 |
| 1KB | 0.0046 | 0.2708 | 0.2884 | 3.135 |
| 64KB | 0.0052 | 0.3172 | 0.3280 | 4.379 |
| 1MB | 0.0046 | 0.5480 | 0.7245 | 6.962 |
| 10MB | 0.0036 | 2.790 | 4.350 | 34.944 |
| 100MB | 0.0034 | 19.901 | 28.856 | 273.6 |

100MB IPC-bytes：v0.4 回归前 ~130ms → 修复后 19.9ms（6.5× 改善，接近 v0.3 基线 17.87ms）。

---

## 七、缺陷汇总与优先级

| # | 缺陷 | 严重度 | 修复难度 | 说明 |
|---|------|:------:|:--------:|------|
| A | `gc_buddy()` 无自动触发 → buddy 段永不回收 | 🔴 高 | 中 | 需要在 free 路径或 server 中接入定时/条件触发 |
| B | T4 磁盘溢写不可达 | 🟡 中 | 低 | server/client 改用 `alloc_handle()` 即可 |
| C | 响应无 chunked 路径 | 🟡 中 | 高 | 需新增 server 分片发送 + client 分片接收 |
| D | 小负载 per-call 开销 | 🟡 中 | 中 | 减少 FFI 边界拷贝，响应侧加栈 buffer |
| E | `alloc_buddy()` 扩展前不回收空闲段 | 🟡 低 | 低 | 扩展前先调用 `gc_buddy()` |

---

## 八、相关文件索引

| 文件 | 关键内容 |
|------|---------|
| `c2-mem/src/pool.rs` | MemPool 核心：alloc/free_at/gc_buddy/gc_dedicated/alloc_handle |
| `c2-mem/src/spill.rs` | T4 磁盘溢写：should_spill/create_file_spill |
| `c2-mem/src/alloc/buddy.rs` | BuddyAllocator：bitmap + spinlock 跨进程安全 |
| `c2-server/src/server.rs` | Server：response_pool/smart_reply_with_data/dispatch |
| `c2-server/src/connection.rs` | PeerShmState：惰性打开/read/free 客户端段 |
| `c2-ipc/src/client.rs` | Client：ServerPoolState/call_full/decode_response |
| `c2-ffi/src/client_ffi.rs` | Client FFI：SyncClient 包装 |
| `c2-ffi/src/server_ffi.rs` | Server FFI：dispatch callback |
| `c2-ffi/src/mem_ffi.rs` | MemPool FFI：gc()/free()/free_at() |
| `c2-wire/src/assembler.rs` | ChunkAssembler：唯一使用 alloc_handle() 的消费者 |


