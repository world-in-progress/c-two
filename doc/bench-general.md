# IPC v3 General Benchmark Report

## 概述

本报告记录使用 **通用 `@transferable` 自定义类型**（非 `bytes` 快速路径）的 IPC v2 vs v3 端到端性能对比。
此前的 benchmark 使用 `bytes` 类型参数/返回值，走的是框架内置的零拷贝快速路径（serialize/deserialize 均为 identity 函数），
不代表真实业务场景。本次 benchmark 使用 struct-based 自定义序列化，反映实际使用中的完整开销。

### 测试环境

- **平台**: macOS ARM64 (Apple Silicon)
- **Python**: 3.14t (free-threading build)
- **协议**: IPC v3 (buddy allocator + SHM) vs IPC v2 (pool SHM + streaming)
- **轮次**: 100 rounds per size, 5 warmup
- **日期**: 2026-03-25

### Transferable 类型定义

```python
@cc.transferable
class Payload:
    tag: int          # 8B metadata
    timestamp: float  # 8B metadata
    blob: bytes       # bulk binary data

    def serialize(data: 'Payload') -> bytes:
        header = struct.pack('<Qd', data.tag, data.timestamp)  # 16B header
        return header + data.blob  # ← 此处产生一次完整拷贝

    def deserialize(raw: bytes | memoryview) -> 'Payload':
        if isinstance(raw, memoryview):
            raw = bytes(raw)          # ← 此处 memoryview→bytes 拷贝
        tag, ts = struct.unpack_from('<Qd', raw, 0)
        blob = raw[16:]               # ← 此处 slice 拷贝
        return Payload(tag=tag, timestamp=ts, blob=blob)
```

这个类型模拟了真实场景（如 DataFrame chunk、图像 tile、仿真网格切片）：少量元数据 + 大块二进制数据。

---

## 1. 原始数据

### IPC v3（@transferable）— 100 rounds

| Size | P50 (ms) | P99 (ms) | Throughput (GB/s) | ops/s |
|------|----------|----------|-------------------|-------|
| 64B | 0.138 | 0.269 | — | 7231.1 |
| 1KB | 0.142 | 0.329 | 0.01 | 7029.9 |
| 4KB | 0.147 | 0.337 | 0.03 | 6810.4 |
| 64KB | 0.145 | 0.253 | 0.42 | 6880.7 |
| 1MB | 0.423 | 0.581 | 2.31 | 2361.3 |
| 10MB | 2.895 | 3.735 | 3.37 | 345.4 |
| 50MB | 10.994 | 19.066 | 4.44 | 91.0 |
| 100MB | 21.770 | 34.019 | 4.49 | 45.9 |
| 500MB | 139.648 | 213.169 | 3.50 | 7.2 |
| 1GB | 890.792 | 1177.096 | 1.12 | 1.1 |

### IPC v2（@transferable）— 100 rounds

| Size | P50 (ms) | P99 (ms) | Throughput (GB/s) | ops/s |
|------|----------|----------|-------------------|-------|
| 64B | 0.144 | 0.257 | — | 6960.6 |
| 1KB | 0.146 | 0.313 | 0.01 | 6835.7 |
| 4KB | 0.156 | 0.360 | 0.02 | 6422.3 |
| 64KB | 0.334 | 0.771 | 0.18 | 2998.1 |
| 1MB | 2.013 | 2.945 | 0.49 | 496.8 |
| 10MB | 24.217 | 56.284 | 0.40 | 41.3 |
| 50MB | 107.354 | 129.390 | 0.45 | 9.3 |
| 100MB | 218.742 | 457.236 | 0.45 | 4.6 |
| 500MB | 1234.873 | 1631.517 | 0.40 | 0.8 |
| 1GB | 4842.026 | 5870.287 | 0.21 | 0.2 |

---

## 2. v3 vs v2 对比

| Size | v3 P50 | v2 P50 | Δ P50 | 加速比 | v3 Throughput |
|------|--------|--------|-------|--------|---------------|
| 64B | 0.138ms | 0.144ms | +3.7% | 1.04× | — |
| 1KB | 0.142ms | 0.146ms | +2.8% | 1.03× | 0.01 GB/s |
| 4KB | 0.147ms | 0.156ms | +5.7% | 1.06× | 0.03 GB/s |
| 64KB | 0.145ms | 0.334ms | +56.4% | 2.30× | 0.42 GB/s |
| 1MB | 0.423ms | 2.013ms | +79.0% | 4.76× | 2.31 GB/s |
| 10MB | 2.895ms | 24.217ms | +88.0% | 8.36× | 3.37 GB/s |
| 50MB | 10.994ms | 107.354ms | +89.8% | 9.76× | 4.44 GB/s |
| 100MB | 21.770ms | 218.742ms | +90.0% | 10.05× | 4.49 GB/s |
| 500MB | 139.648ms | 1234.873ms | +88.7% | 8.84× | 3.50 GB/s |
| 1GB | 890.792ms | 4842.026ms | +81.6% | 5.44× | 1.12 GB/s |

**Geometric Mean P50 (10MB–1GB):**
- v3: **38.645ms**
- v2: **320.851ms**
- **总体加速: 8.3×**

---

## 3. bytes 快速路径 vs @transferable 对比

此前使用 `echo(data: bytes) -> bytes` 的 benchmark 走的是框架内置快速路径（serialize/deserialize 为 identity 函数）。
对比两组 v3 数据，可以精确量化序列化层的开销：

| Size | bytes P50 | @transferable P50 | 序列化开销倍数 | 序列化耗时估算 |
|------|-----------|-------------------|---------------|---------------|
| 64B | 0.120ms | 0.138ms | 1.15× | ~0.02ms |
| 64KB | 0.123ms | 0.145ms | 1.18× | ~0.02ms |
| 1MB | 0.179ms | 0.423ms | 2.36× | ~0.24ms |
| 10MB | 0.738ms | 2.895ms | 3.92× | ~2.16ms |
| 50MB | 2.582ms | 10.994ms | 4.26× | ~8.41ms |
| 100MB | 6.170ms | 21.770ms | 3.53× | ~15.60ms |
| 500MB | 25.465ms | 139.648ms | 5.48× | ~114.18ms |
| 1GB | 51.088ms | 890.792ms | 17.44× | ~839.70ms |

### 关键发现

**序列化开销随数据规模呈超线性增长**：
- ≤64KB: 序列化几乎无感（<0.02ms），传输延迟主导
- 1MB–10MB: 序列化占总延迟的 57%–75%
- 100MB+: 序列化占总延迟的 72%–94%
- 1GB: 序列化消耗 ~840ms（94%），传输仅 ~51ms（6%）

---

## 4. 瓶颈分析

### 4.1 序列化路径拷贝计数

一次完整的 echo round-trip 中，`@transferable` 的 `Payload` 类型经历以下拷贝：

**客户端发送 (serialize)**:
1. `struct.pack('<Qd', tag, ts)` → 16B header（可忽略）
2. `header + data.blob` → **整个 blob 的完整拷贝**（1GB = 1次 1GB 拷贝）
3. 框架将序列化结果写入 SHM（`write_call_into`）→ **第2次完整拷贝**

**服务端处理 (deserialize + re-serialize)**:
4. `bytes(raw)` memoryview→bytes 转换 → **第3次完整拷贝**
5. `raw[16:]` slice → **第4次完整拷贝**（Python bytes slice 产生新对象）
6. CRM 执行 `echo` 返回原对象
7. 响应 serialize: `header + data.blob` → **第5次完整拷贝**
8. 框架将结果写入 SHM → **第6次完整拷贝**

**客户端接收 (deserialize)**:
9. warm buffer memmove from SHM → **第7次完整拷贝**
10. `bytes(raw)` 转换 → **第8次完整拷贝**
11. `raw[16:]` slice → **第9次完整拷贝**

**总计: 9次完整数据拷贝**（vs bytes 快速路径的 2次）

### 4.2 1GB 超线性增长原因

1GB 时序列化开销是 500MB 的 7.4 倍（而非预期的 2 倍），原因：
- **内存分配器压力**: 1GB `bytes` 对象分配触发 `mmap` 系统调用（超出 `brk` 阈值）
- **TLB miss**: 1GB 数据超出 L3 缓存，每次拷贝产生大量 TLB miss
- **Python GC 压力**: 多个临时 1GB `bytes` 对象的分配/释放触发 GC 扫描
- **内存带宽饱和**: 9次 × 1GB = 9GB 总内存传输量，接近 DDR5 单通道极限

---

## 5. 吞吐量曲线分析

v3 的吞吐量呈现一个明显的 **倒U型曲线**：

```
v3 Throughput (GB/s):
  64B   →  ~0     (延迟主导，数据量太小)
  64KB  →  0.42   (开始发挥 SHM 优势)
  1MB   →  2.31   (SHM + buddy allocator 高效)
  10MB  →  3.37   ↑ 上升
  50MB  →  4.44   ↑ 上升
  100MB →  4.49   ← 峰值吞吐
  500MB →  3.50   ↓ 序列化拷贝开始拖累
  1GB   →  1.12   ↓ 急剧下降（9次拷贝 + GC 压力）
```

**峰值在 50MB–100MB**：此时传输层高效（buddy alloc + SHM），而序列化拷贝尚未成为主导瓶颈。
超过 100MB 后，Python 层的多次 `bytes` 对象创建和内存带宽饱和拖累了整体性能。

v2 的吞吐量则始终平坦（~0.4 GB/s），因为 v2 的半双工流式传输本身就是瓶颈，序列化开销相对可忽略。

---

## 6. 结论与优化方向

### 当前状态总结

| 场景 | v3 加速比 (vs v2) | 瓶颈 |
|------|-------------------|------|
| bytes 快速路径 | **44.3×** (geomean) | 传输层（已接近理论极限） |
| @transferable 通用路径 | **8.3×** (geomean) | 序列化层（9次拷贝） |

v3 传输层已高度优化（buddy allocator + SHM + warm buffer），但 **序列化层现在是最大瓶颈**，
尤其在 100MB+ 规模下贡献了 72%–94% 的端到端延迟。

### 推荐优化方向

#### P0: Scatter-Write Transferable（零拷贝序列化）

允许 `@transferable` 的 `serialize` 返回多段 buffer 而非单个 `bytes`，
框架直接将各段 scatter-write 到 SHM，消除 `header + blob` 拼接拷贝：

```python
@cc.transferable
class Payload:
    def serialize(data: 'Payload') -> tuple[bytes, bytes]:  # 多段返回
        header = struct.pack('<Qd', data.tag, data.timestamp)
        return (header, data.blob)  # 无拼接，直接双段写入 SHM
```

预期收益：消除发送端 1 次完整拷贝，接收端 1 次完整拷贝。1GB 时可节省 ~200ms。

#### P1: memoryview-aware Deserialize

修改 `deserialize` 约定，允许接收 `memoryview` 并直接切片，不做 `bytes()` 转换：

```python
def deserialize(raw: memoryview) -> 'Payload':
    tag, ts = struct.unpack_from('<Qd', raw, 0)
    blob = bytes(raw[16:])  # 仅在最终需要时转换（而非先全量转换再 slice）
    return Payload(tag=tag, timestamp=ts, blob=blob)
```

预期收益：消除 `bytes(raw)` 全量转换 + slice 拷贝 → 每次 deserialize 减少 1-2 次拷贝。

#### P2: Arena Allocator for Serialize

为大数据序列化提供框架级预分配 buffer，避免 Python 层的 `bytes` 对象反复分配：

```python
# 框架提供 buffer，serialize 直接写入
def serialize_into(data: 'Payload', buf: bytearray, offset: int) -> int:
    struct.pack_into('<Qd', buf, offset, data.tag, data.timestamp)
    buf[offset+16 : offset+16+len(data.blob)] = data.blob
    return 16 + len(data.blob)
```

预期收益：消除 Python `bytes` 对象分配的 mmap 系统调用开销。

#### P3: 选择性 Direct-Pass 模式

对于 echo 类场景（CRM 不修改数据），支持 "direct-pass" 模式，
服务端 deserialize 返回的对象直接被 serialize 引用，不产生中间拷贝。

---

## 附录: 测试脚本

测试脚本位于 `temp/v2_v3_general_bench.py`。

