# IPC v3 @transferable 序列化优化报告

**分支**: `autoresearch/transferable-opt`  
**基线**: `d0a910d` (autoresearch/large-payload-perf)  
**最终**: `38fb11c`  
**实验总数**: 12 (7 保留, 5 丢弃)  

---

## 1. 问题背景

IPC v3 的 `bytes` 快速路径已实现 44.3× 对 v2 的加速（跳过序列化）。
但通过 `@transferable` 自定义类型的路径仅有 8.3× 加速——因为每次往返存在 **9 次完整数据拷贝**。

主要应用场景（10MB–1GB 科学数据传输）的性能被这些冗余拷贝严重拖慢。

### 拷贝来源分析（优化前）

| # | 位置 | 操作 | 类型 |
|---|------|------|------|
| 1 | Client serialize | `header + blob` 拼接 | 不必要 |
| 2 | Client write_call_into | payload → SHM memmove | **必要** |
| 3 | Server deserialize | `bytes(memoryview)` 转换 | 不必要 |
| 4 | Server com_to_crm | `bytes(raw)` 转换 | 不必要 |
| 5 | Server CRM echo | 返回相同对象（0 copy） | - |
| 6 | Server serialize | `header + blob` 拼接 | 不必要 |
| 7 | Server reply alloc | payload → SHM memmove | 不必要 |
| 8 | Client warm buffer | SHM → warm buf memmove | 不必要 |
| 9 | Client deserialize | `bytes(memoryview)` 转换 | 不必要 |

**目标**: 将 9 次拷贝降至理论最小值（1 次）。

---

## 2. 优化实验总览

| # | 实验 | Metric (ms) | 状态 | 改善 |
|---|------|------------|------|------|
| 0 | 基线 | 36.43 | baseline | - |
| 1 | memoryview-aware 标志 | 33.99 | ✅ keep | -6.7% |
| 2 | scatter-write serialize | 32.55 | ✅ keep | -4.2% |
| 3 | zero-copy deserialize | 26.23 | ✅ keep | -19.4% |
| 4 | tuple aliasing 修复 | 25.90 | ✅ keep | 正确性 |
| 5 | try_alloc_buddy | 27.63 | ❌ discard | 回归 |
| 6 | scatter-reuse 路径 | 26.54 | ❌ discard* | bug 掩盖 |
| 7 | auto_transfer 修复 | **6.81** | ✅ keep | **-81.3%** |
| 8 | deferred buddy free | **3.53** | ✅ keep | **-48.2%** |
| 9 | request block reuse | **3.44** | ✅ keep | -2.6% |
| 10 | single-recv 合并 | 3.47 | ❌ discard | 无效 |
| 11 | skip conn_lock | 3.48 | ❌ discard | 无效 |

*实验 6 的代码正确但因 auto_transfer bug 无法触发。实验 7 修复 bug 后自动激活。

**总改善**: 36.43ms → 3.44ms = **10.6× 加速**


---

## 3. 关键优化详解

### 3.1 `__memoryview_aware__` 标志（实验 1）

**文件**: `src/c_two/rpc/transferable.py`

在 `Transferable` 基类上添加 `__memoryview_aware__ = False` 类属性。当 `@transferable` 类将其设为 `True` 时，框架跳过 `com_to_crm` 和 `crm_to_com` 中的 `bytes(raw)` 转换，直接传递 memoryview。

```python
@cc.transferable
class MyData:
    __memoryview_aware__ = True
    ...
    def deserialize(raw: bytes | memoryview) -> 'MyData':
        # raw 可能是 memoryview，直接切片无拷贝
        return MyData(data=raw[16:])
```

**消除拷贝**: #3 (Server com_to_crm) + #9 (Client deserialize)

### 3.2 Scatter-Write Serialize（实验 2）

**文件**: `src/c_two/rpc/util/wire.py`, `ipc_v3_client.py`, `ipc_v3_server.py`

允许 `serialize()` 返回 `tuple` 而非拼接的 `bytes`。`write_call_into` 和 `write_reply_into` 逐段写入 SHM，避免中间拼接。

```python
def serialize(data) -> tuple:
    header = struct.pack('<Qd', data.tag, data.timestamp)  # 16B
    return (header, data.blob)  # 不拼接，scatter-write 逐段写入
```

**消除拷贝**: #1 (Client serialize 拼接) + #6 (Server serialize 拼接)

### 3.3 Scatter-Reuse 路径（实验 6 + 7）

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`

当 CRM echo 返回的 serialize 结果中 blob 仍指向请求 SHM 块的相同位置时（`blob.obj is seg_obj`），服务端仅写入 21 字节头部（5B reply header + 16B serialize header），blob 留在原位。

**核心条件**: `ctypes.addressof(blob) - ctypes.addressof(seg_mv) == expected_offset`

**消除拷贝**: #7 (Server reply alloc + memmove)

### 3.4 auto_transfer 修复（实验 7）— 最关键发现

**文件**: `src/c_two/rpc/transferable.py` 的 `auto_transfer()` 函数

**Bug**: `auto_transfer()` 的输入参数匹配使用 `inspect.signature()` 检查 `serialize()` 参数类型注解。当注解缺失或为字符串引用（`'Payload'`）时，比较失败，回退到 `create_default_transferable()`——基于 pickle 的序列化器。Pickle 将所有 memoryview 转为 bytes，**使所有零拷贝优化失效**。

**修复**: 添加 Priority 1 直接查找——当 ICRM 方法只有一个非 self 参数且类型在 `_TRANSFERABLE_MAP` 中时，直接使用该 Transferable（与输出匹配器相同策略）。

**影响**: METRIC 25.9 → 6.81 (81.3% 改善), 1GB P50: 664ms → 52ms

### 3.5 Deferred Buddy Free（实验 8）

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`

客户端不再将 SHM 响应数据 memmove 到 warm buffer，而是直接返回指向 SHM 的 memoryview。buddy block 的释放推迟到下一次 `call()` 调用的开头。

**消除拷贝**: #8 (Client warm buffer memmove)

### 3.6 Request Block Reuse（实验 9）

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`

当上一次响应使用了 scatter-reuse（同一 buddy block），下一次请求可以直接写入同一 block，跳过 `alloc()` + `free_at()` 的 Rust FFI 调用。

**条件**: `wire_size <= free_size && !is_dedicated && wire_size > shm_threshold`

**节省**: ~20μs/call (10MB: -10%)

---

## 4. 最终拷贝预算

优化后单次 echo round-trip 的数据拷贝：

| 步骤 | 操作 | 拷贝次数 |
|------|------|---------|
| Client serialize | 返回 tuple (header, memoryview) | **0** |
| Client write_call_into | scatter-write 到 SHM | **1** ← 唯一拷贝 |
| Server deserialize | memoryview 切片 | **0** |
| Server CRM echo | 返回原对象 | **0** |
| Server serialize | 返回 tuple + memoryview | **0** |
| Server scatter-reuse | 仅写 21B 头部 | **0** |
| Client response | 返回 SHM memoryview | **0** |
| Client deserialize | memoryview 切片 | **0** |
| **总计** | | **1 次** (理论最小值) |

---

## 5. 综合基准测试结果

**测试环境**: macOS ARM64, Python 3.14t, IPC v3 with buddy allocator  
**方法**: `@transferable` Payload (tag: int64, timestamp: float64, blob: bytes)

### 5.1 IPC v3 @transferable 最终性能

| Size | P50 (ms) | P99 (ms) | Throughput | Ops/s |
|-----:|----------:|----------:|----------:|------:|
| 64B | 0.120 | 0.245 | 0.00 GB/s | 7,742 |
| 1KB | 0.120 | 0.229 | 0.01 GB/s | 7,748 |
| 4KB | 0.223 | 0.376 | 0.02 GB/s | 4,242 |
| 64KB | 0.152 | 0.294 | 0.36 GB/s | 5,952 |
| 1MB | 0.154 | 0.293 | 5.91 GB/s | 6,055 |
| 10MB | 0.386 | 0.581 | 24.67 GB/s | 2,526 |
| 50MB | 1.435 | 1.782 | 33.33 GB/s | 683 |
| 100MB | 2.962 | 5.161 | 31.22 GB/s | 320 |
| 500MB | 12.365 | 13.542 | 38.94 GB/s | 80 |
| 1GB | 24.640 | 25.859 | 40.21 GB/s | 40 |

### 5.2 性能分析

**小数据 (64B–64KB)**: 固定开销主导 (~120μs = UDS round-trip + asyncio + Python)。
4KB 有异常高延迟 (223μs)，可能因跨 buddy 分配阈值。

**中等数据 (1MB–10MB)**: 吞吐量从 5.91 GB/s 攀升至 24.67 GB/s。
1MB 已接近 CPU cache line 优势消失的边界。

**大数据 (50MB–1GB)**: 稳定在 31–40 GB/s，逼近内存带宽上限。
1GB P50=24.6ms，接近裸 `ctypes.memmove` 的理论值 (24.5ms)。

### 5.3 理论极限对比

| 操作 | 1GB P50 |
|------|---------|
| 裸 memmove (regular buffer) | 24.5ms |
| 裸 memmove (cold SHM) | 31.2ms |
| **IPC v3 @transferable** | **24.6ms** |
| IPC v2 (半双工，pickle) | ~1,500ms |

IPC v3 1GB 性能仅比裸内存拷贝多 0.4%。
相比 IPC v2 提升 **约 61×**。

---

## 6. 性能瓶颈分析

### 6.1 当前瓶颈

经过 12 次实验，连续 3 次实验无法改善性能，确认已达平台期。剩余开销构成：

| 组件 | 估计开销 |
|------|---------|
| UDS round-trip (send+recv) | ~80μs |
| asyncio 事件循环调度 | ~30-50μs |
| Python 解释器开销 | ~20μs |
| **固定开销合计** | ~130μs |

对于 10MB payload，130μs 固定开销占总延迟 (390μs) 的 33%。
对于 1GB payload，固定开销占总延迟 (24.6ms) 的 <1%——性能完全由 memmove 带宽决定。

### 6.2 进一步优化方向（需架构变更）

以下优化超出本次迭代范围，但记录供未来参考：

1. **Rust 事件循环**: 用 Rust tokio 替代 Python asyncio，减少 ~50μs
2. **io_uring / kqueue 直通**: 绕过 asyncio 的通用 I/O 层
3. **双缓冲轮换**: 消除最后一次 memmove（write 到 buffer A 时 read buffer B）
4. **RDMA / persistent mapping**: 极端场景下消除所有内核态切换

---

## 7. 关键发现与教训

### 7.1 Bug 比优化更重要

实验 7 的 `auto_transfer` 修复带来了 81.3% 的改善——是所有优化中影响最大的。
教训：在追求微优化前，先确保框架的关键路径正确触发。一个让所有其他优化失效的 bug
比任何单个优化更有价值去修复。

### 7.2 零拷贝的杠杆效应

消除 1 次 1GB 的数据拷贝节省 ~25ms。在 9 次拷贝中消除 8 次 = 节省 ~200ms/GB。
对于高频大数据传输场景，零拷贝的收益是乘法级的。

### 7.3 Scatter-Reuse 的巧妙性

当 CRM 方法只是"传递"数据（常见于路由、代理、echo 场景），scatter-reuse
将服务端的写入量从 `payload_size` 降为 21 字节——这不是渐进改善，而是数量级跳跃。

---

## 8. 修改文件清单

| 文件 | 修改内容 |
|------|---------|
| `src/c_two/rpc/transferable.py` | `__memoryview_aware__`, com_to_crm/crm_to_com 守卫, auto_transfer 直接查找 |
| `src/c_two/rpc/util/wire.py` | `payload_total_size()`, tuple scatter-write |
| `src/c_two/rpc/ipc/ipc_v3_client.py` | deferred buddy free, request block reuse, `_close_connection` flush |
| `src/c_two/rpc/ipc/ipc_v3_server.py` | scatter-reuse 路径, tuple aliasing 修复 |

**测试状态**: 686 测试全部通过 ✅
