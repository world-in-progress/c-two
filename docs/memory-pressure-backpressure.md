# IPC v3 内存压力背压方案 (Memory Pressure Backpressure)

## 概述

C-Two 的 IPC v3 传输层使用 **Buddy Allocator** 管理共享内存池 (SHM Pool)，为跨进程
RPC 调用提供零拷贝数据通道。当 SHM 池耗尽时（并发负载高、payload 大），若无保护机制，
Rust FFI 层的 `alloc()` 会抛出 `RuntimeError`，导致客户端连接中断甚至进程崩溃。

本方案实现了 **三层纵深防御** (L0 → L2)，在不牺牲正常路径性能的前提下，为内存压力
场景提供优雅降级：

| 层级 | 阶段 | 机制 | 效果 |
|------|------|------|------|
| **L0** | 配置时 | 物理内存校验 | 阻止不可能成功的配置 |
| **L1** | — | *预留* | 未来可加 advisory pre-check |
| **L2** | 运行时 | try-except + inline fallback / MemoryPressureError | 优雅降级 |

---

## 1. L0：配置时物理内存校验

**位置**: `src/c_two/rpc_v2/registry.py` → `_make_ipc_config()`

在 CRM 注册时（`cc.register()`），框架自动构建 `IPCConfig`。此时执行两项检查：

### 1.1 硬性错误：segment_size > 物理内存

```python
if seg > phys:
    raise ValueError(
        f'pool_segment_size ({seg / (1 << 30):.1f} GB) exceeds '
        f'physical RAM ({phys / (1 << 30):.1f} GB) — '
        f'SHM allocation will fail'
    )
```

单个 segment 大于物理内存，`ftruncate()` 必然失败，直接报错。

### 1.2 软性警告：pool 上限 > 75% 物理内存

```python
if pool_max > int(phys * 0.75):
    log.warning('Theoretical pool ceiling %.1f GB exceeds 75%% of physical RAM ...')
```

由于 segment 是 **惰性分配**（仅在现有 segment 满时才创建新的），理论上限不等于实际
用量。警告提醒用户在高负载下监控内存。

### 物理内存探测

通过 `os.sysconf('SC_PHYS_PAGES')` × `os.sysconf('SC_PAGE_SIZE')` 获取，
跨 Linux/macOS 可用。探测失败时跳过检查（不阻断启动）。

---

## 2. L2：运行时背压保护

**核心思想**：在 buddy `alloc()` 外层包裹 try-except，根据 payload 大小决定
**inline 降级** 还是 **抛出 MemoryPressureError**。

### 2.1 决策流程

客户端发送 RPC 请求时，payload 的传输路径选择如下：

```
payload_size ≤ shm_threshold (4KB)?
  ├── YES → 直接 inline（UDS 帧内嵌数据）
  └── NO  → 尝试 buddy alloc()
               ├── 成功 (普通 segment) → SHM 零拷贝路径 ✓
               ├── 成功 (dedicated segment) → 跨进程不可读，释放后降级:
               │     ├── payload ≤ max_frame_size → inline 降级 ⚠
               │     └── payload > max_frame_size → MemoryPressureError ✗
               └── 失败 (alloc=None) → 池耗尽，降级:
                     ├── payload ≤ max_frame_size → inline 降级 ⚠
                     └── payload > max_frame_size → MemoryPressureError ✗
```

**关键阈值**:

| 参数 | 默认值 | 作用 |
|------|--------|------|
| `shm_threshold` | 4 KB | 小于此值直接走 inline，不分配 SHM |
| `max_frame_size` | 16 MB | inline 帧最大尺寸，超过则无法降级 |
| `pool_segment_size` | 256 MB | 单个 buddy segment 大小 |
| `max_pool_segments` | 4 | 最大 segment 数（惰性扩展） |
| `max_pool_memory` | 1 GB | 池总容量上限 |

### 2.2 Inline 降级 (Graceful Degradation)

当 buddy alloc 失败但 payload ≤ `max_frame_size` (默认 16MB) 时，数据通过 UDS
(Unix Domain Socket) 帧直接传输，绕过 SHM。

**代价**：
- 额外一次 `sendall()` 内存拷贝（相对 SHM 零拷贝有性能损失）
- UDS 受内核缓冲区限制（`SO_SNDBUF` 通常 128KB），大帧会阻塞发送线程

**优势**：
- 调用方完全无感知，RPC 调用正常返回
- 无需重试逻辑
- 客户端连接保持活跃，后续调用可恢复 SHM 路径

### 2.3 MemoryPressureError（不可降级）

当 buddy alloc 失败 **且** payload > `max_frame_size` 时，无法通过 inline 传输，
抛出 `MemoryPressureError`：

```python
class MemoryPressureError(CCBaseError):
    """Raised when a buddy pool allocation fails and the payload is too large
    for inline fallback (exceeds ``max_frame_size``).

    Callers may retry after freeing resources or reconfiguring the pool.
    """
```

**特性**：
- 继承 `CCBaseError`，可被框架现有错误处理链正确传播
- 客户端连接 **不会中断**，后续小 payload 调用可正常恢复
- 错误消息包含请求的字节数和 inline 上限，便于诊断

**调用方处理建议**：
```python
from c_two.error import MemoryPressureError

try:
    result = icrm.process_large_data(big_payload)
except MemoryPressureError:
    # 策略 1: 分片传输
    for chunk in split_payload(big_payload):
        icrm.process_chunk(chunk)
    # 策略 2: 等待后重试（等其他请求释放 SHM）
    time.sleep(0.1)
    result = icrm.process_large_data(big_payload)
    # 策略 3: 增大池配置
    cc.set_server_ipc_config(segment_size=512 * 1024 * 1024)
    cc.set_client_ipc_config(segment_size=512 * 1024 * 1024)
```

---

## 3. 保护覆盖范围

背压保护应用于 **所有** 客户端 alloc 路径，包括两套客户端实现：

### 3.1 Legacy IPCv3Client (`rpc/ipc/ipc_v3_client.py`)

单连接、单调用的传统客户端。保护点：

| 方法 | 保护内容 |
|------|----------|
| `call()` 内的 buddy alloc | try-except + inline/MemoryPressureError |
| dedicated segment 检查 | free + max_frame_size 校验 |

### 3.2 SharedClient (`rpc_v2/client.py`)

多路复用客户端，支持并发调用。**三个发送方法**均受保护：

| 方法 | 协议 | 保护内容 |
|------|------|----------|
| `_send_request_v1()` | v1 wire 格式 | alloc 失败 + dedicated 降级 |
| `_send_request_v2()` | v2 控制面路由 | alloc 失败 + dedicated 降级 |
| `_send_relay()` | v1 relay 透传 | alloc 失败 + dedicated 降级 |

每个方法的保护模式完全一致：

```python
with self._alloc_lock:
    try:
        alloc = self._buddy_pool.alloc(size)
    except Exception:
        alloc = None

    if alloc is None:
        if size <= self._config.max_frame_size:
            # inline 降级
            logger.debug('Buddy alloc failed for %d bytes, using inline fallback', size)
            ...send inline frame...
            return
        raise error.MemoryPressureError(...)

    if alloc.is_dedicated:
        self._buddy_pool.free_at(...)
        if size <= self._config.max_frame_size:
            ...send inline frame...
            return
        raise error.MemoryPressureError(...)

    # 正常 SHM 路径
    ...write to shm_buf, send buddy frame...
```

### 3.3 服务端 (`rpc_v2/server.py`)

服务端的 **响应路径** 已有内建保护（早于本方案）：

- `_send_v1_reply()`: try alloc → dedicated 检查 → `except: pass` → inline fallback
- `_send_v2_reply()`: 同上

服务端的降级对客户端完全透明——客户端收到的仍是正常的 reply 帧，
只是走了 inline 通道而非 SHM。

### 3.4 Dedicated Segment 特殊处理

Buddy allocator 在池内 segment 无法满足请求时，会创建一个 **dedicated segment**
（专用 segment）。但 dedicated segment 是进程本地的，**对端进程无法读取**。

因此所有 dedicated 分配结果必须：
1. 立即 `free_at()` 释放
2. 检查 `max_frame_size` 决定 inline 降级还是 MemoryPressureError

> ⚠️ 早期实现中 dedicated 路径未检查 `max_frame_size`，直接走 inline，
> 导致大 payload 发送到对端后因帧超限而 BrokenPipeError。已修复。

---

## 4. 可观测性 (Observability)

每次 inline 降级都会输出 `DEBUG` 级别日志：

```
DEBUG c_two.rpc_v2.client: Buddy alloc failed for 52480 bytes, using inline fallback (v2)
DEBUG c_two.rpc.ipc.ipc_v3_client: Buddy alloc failed for 102400 bytes, using inline fallback
```

生产环境中可通过日志聚合监控降级频率。频繁降级说明池容量不足，
应增大 `pool_segment_size` 或 `max_pool_segments`。

`MemoryPressureError` 的错误消息包含详细诊断信息：

```
MemoryPressureError: Buddy pool exhausted: cannot allocate 204800 bytes;
payload exceeds inline limit (8192)
```

---

## 5. 并发安全性

IPC v3 传输天然支持并发。背压机制在并发场景下的安全性保证：

| 组件 | 并发机制 | 背压影响 |
|------|----------|----------|
| Buddy pool `alloc()`/`free()` | Rust RwLock（线程安全） | 无额外锁 |
| SharedClient `_alloc_lock` | Python threading.Lock | 保护 alloc + seg_views 一致性 |
| SharedClient `_send_lock` | Python threading.Lock | 保护 socket 写入序列化 |
| ServerV2 asyncio loop | 单线程事件循环 + 线程池 | 响应路径天然序列化 |

**关键设计**：`_alloc_lock` 的持有范围覆盖 `alloc()` 到 `seg_views` 切片之间，
确保在多线程并发 `call()` 时不会出现 segment 索引与 memoryview 不一致。

inline 降级发生在 `_alloc_lock` 内部，因此降级决策和帧发送之间不存在竞态。

---

## 6. 测试覆盖

完整的背压测试位于 `tests/integration/test_backpressure.py`，共 **18 个测试用例**，
分为 8 个测试类：

| 测试类 | 场景 | 验证内容 |
|--------|------|----------|
| `TestClientInlineFallback` | 小 payload inline 降级 | 正确返回结果 |
| `TestMemoryPressureError` | 错误类属性 | 继承 CCBaseError、消息格式 |
| `TestServerInlineFallback` | 服务端响应降级 (20 次调用) | 客户端无感知 |
| `TestConcurrentBackpressure` | 8 线程并发 / 4 线程混合 | 无数据损坏、正确错误类型 |
| `TestRecoveryAfterPressure` | 降级后恢复正常 | 客户端连接保持活跃 |
| `TestLargePayloadPressureError` | 200KB payload + 8KB inline 限制 | 正确抛出 MemoryPressureError |
| `TestHighConcurrencyStress` | 16 线程 × 20 调用 / 共享客户端 | 数据完整性校验 |
| `TestEdgeCases` | 空 payload、3 轮压力/恢复循环 | 边界条件正确 |
| `TestSOTAAPIBackpressure` | `cc.register` + `cc.connect` | SOTA API 集成 |
| `TestLegacyClientBackpressure` | `rpc.Server` + `compo.runtime` | 传统客户端路径 |

**测试配置**：使用极小的 `IPCConfig` 强制触发压力场景：

```python
IPCConfig(
    pool_segment_size=65536,      # 64 KB（buddy 开销后可用约 32 KB）
    max_pool_segments=1,          # 禁止扩展
    max_pool_memory=65536,
    max_frame_size=8192,          # 8 KB inline 上限（用于测试 MemoryPressureError）
)
```

---

## 7. 配置调优指南

### 7.1 通过代码配置

```python
import c_two as cc

# 注册 CRM 之前设置
cc.set_server_ipc_config(
    segment_size=512 * 1024 * 1024,  # 512 MB per segment
    max_segments=2,                   # 最多 2 个 segment
)
cc.set_client_ipc_config(
    segment_size=512 * 1024 * 1024,
    max_segments=2,
)
```

### 7.2 通过环境变量配置

```bash
export C2_IPC_SEGMENT_SIZE=536870912    # 512 MB
export C2_IPC_MAX_SEGMENTS=2
```

代码设置优先级高于环境变量。

### 7.3 容量规划

| 场景 | 建议配置 | 说明 |
|------|----------|------|
| 轻量 RPC (payload < 1MB) | 默认 (256MB × 4) | 足够应对大多数场景 |
| 科学计算 (10-100MB payload) | 512MB × 2 | 减少 segment 数、增大单段容量 |
| 极大 payload (> 256MB) | 1GB × 1 | 单段最大化、避免碎片 |
| 多 ICRM 消费端 (> 4 个) | 64MB × 2 | 减小每个 client 的内存占用 |

> **经验法则**：`pool_segment_size` 应至少为最大 payload 的 2 倍
> （buddy allocator 向上取整到 2 的幂次会浪费约 50%）。

---

## 8. 架构图

```
                          客户端 (Client)
                               │
                    payload_size > shm_threshold?
                         ╱              ╲
                       NO                YES
                       │                  │
                  inline 直传        buddy alloc()
                   (UDS帧)          ╱     │      ╲
                              成功(普通)  成功(dedicated)  失败
                               │           │               │
                          SHM 零拷贝    free + 检查      检查 max_frame_size
                               │      max_frame_size    ╱            ╲
                               │      ╱        ╲     ≤ limit      > limit
                               │  ≤ limit   > limit     │            │
                               │     │         │    inline 降级  MemoryPressure
                               │  inline   MemoryPressure  ⚠️      Error ✗
                               │  降级 ⚠️   Error ✗
                               │
                          write to SHM
                               │
                        send buddy frame
                               │
                          ──── UDS ────
                               │
                          服务端 (Server)
```

---

## 9. 未来扩展 (L3)

当前方案覆盖 L0 和 L2。以下为未来可能的增强方向：

- **L1 Pre-check**：在 `alloc()` 前调用 `pool.stats()` 检查 `free_bytes`，
  若明显不足则直接跳到 inline/error 路径，避免 Rust FFI 开销。
  但由于 pre-check 与 alloc 之间存在 TOCTOU 竞态，仅作为优化而非安全保障。

- **L3 Chunked Streaming**：对超过 `max_frame_size` 的 payload，
  分片通过 UDS + SHM 混合传输。需要协议层支持流式 frame 标记。

- **L3 Spill-to-Disk**：当 SHM 耗尽且 inline 不可行时，
  将 payload 写入临时文件，通过 UDS 传递文件路径。
  延迟高但避免数据丢失。

- **madvise / MADV_FREE**：对长期空闲的 SHM segment 调用 `madvise(MADV_FREE)`
  释放物理页面（保留虚拟映射），减少常驻内存占用。
