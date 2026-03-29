# C-Two SHM Buddy Allocator 设计

## 一、设计背景

### 1.1 当前 SHM 方案的问题

当前 C-Two IPC-v2 的 SHM 数据平面存在以下局限：

- **固定 segment 占用**：每个连接预分配 256MB pool segment，无论实际使用多少。多连接场景下内存浪费严重。
- **per-request SHM 回退**：当 payload 超过 pool segment 大小时，退化为 per-request SHM（每次 `shm_open`/`ftruncate`/`mmap`/`munmap`/`shm_unlink`，共 5 个系统调用），性能下降 100-200 倍。
- **无弹性分配**：segment 内部不支持多请求复用，一个请求占住整个 segment，其他请求只能等待或走回退路径。

### 1.2 设计目标

用 **Buddy Allocator** 替代当前的固定 segment 方案，实现：

1. 零系统调用的动态分配/释放（常规场景）
2. 任意大小的 payload 支持（消除 per-request SHM 回退）
3. 多请求并发复用同一 segment
4. 碎片自动合并
5. 统一的 SHM 生命周期管理，同时支持 C-Two 现有 transferable 模式与未来 fastdb 零拷贝模式

## 二、Buddy Allocator 原理

### 2.1 核心思想

Buddy allocator 将一块连续内存按二叉树组织，每层的块大小是上一层的一半：

```
假设 256MB segment，最小分配块 4KB：

Level 0:  [======================== 256MB ========================]
Level 1:  [=========== 128MB ===========][=========== 128MB ===========]
Level 2:  [==== 64MB ====][==== 64MB ====][==== 64MB ====][==== 64MB ====]
  ...
Level 16: [4KB][4KB][4KB]...(65536 个最小块)
```

### 2.2 分配

```
分配 10MB 的请求：
  1. 向上取整到最近的 2 的幂 → 16MB (2^24)
  2. 在 Level 4 (16MB blocks) 中查找空闲块
  3. 如果没有空闲的 16MB 块，从 Level 3 (32MB) 取一个拆分为两个 16MB
  4. 返回 (offset, actual_size) 给调用方
```

### 2.3 释放与合并

```
释放一个 16MB 块：
  1. 标记为空闲
  2. 检查 buddy（同级的兄弟块）是否也空闲
  3. 如果是 → 合并为上一级的 32MB 块
  4. 递归向上合并，直到 buddy 不空闲或到达顶层
```

### 2.4 性能特征

| 操作 | 时间复杂度 | 实际耗时 (估计) |
|------|-----------|---------------|
| alloc | O(log N), N=层数 | ~50ns (16 层 bitmap 操作 + 1 次 CAS) |
| free + merge | O(log N) | ~30ns |
| per-request SHM (当前) | 5 次系统调用 | ~5-10μs |

**性能改进：100-200 倍。**

### 2.5 元数据开销

每层一个 bitmap，总计 `2 × (segment_size / min_block_size)` bits。对于 256MB / 4KB：

```
bitmap 总大小 = 2 × 65536 bits = 16KB
```

极小的元数据开销，全部存储在 SHM segment 的 header 区域中。

## 三、SHM Segment 内存布局

```
SHM Segment:
┌─────────────────────────────────────────────────────────────────┐
│ Header (4KB, 页对齐)                                            │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ magic: u32          (0xCC200001, 用于校验)                   │ │
│ │ version: u16        (分配器版本号)                            │ │
│ │ total_size: u64     (segment 总大小)                         │ │
│ │ data_offset: u32    (data region 起始偏移, 页对齐)            │ │
│ │ min_block: u32      (最小分配块, 默认 4KB)                    │ │
│ │ max_levels: u16     (buddy 层数)                             │ │
│ │ spinlock: u32       (原子自旋锁, 跨进程互斥)                   │ │
│ │ alloc_count: u32    (当前活跃分配数)                           │ │
│ │ free_bytes: u64     (当前可用字节数)                           │ │
│ │ bitmap[]:           (各层的空闲 bitmap, 紧凑存储)              │ │
│ └─────────────────────────────────────────────────────────────┘ │
│                                                                 │
│ Data Region (页对齐起始, 占满剩余空间)                             │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ [buddy block 0] [buddy block 1] [buddy block 2] ...        │ │
│ └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 跨进程同步

Header 中的 `spinlock` 是一个 `atomic_uint32`，通过 CAS (compare-and-swap) 实现跨进程互斥。由于 SHM 是 `mmap` 映射的，多个进程看到同一块物理内存，原子操作天然跨进程有效。

Spinlock 而非 mutex 的理由：buddy allocator 的临界区极短（几次 bitmap 操作），自旋比上下文切换高效。

## 四、多 Segment 管理

### 4.1 Buddy Pool

```python
class ShmBuddyPool:
    segments: list[ShmSegment]           # 多个 buddy segment
    dedicated_segments: list[ShmSegment] # 超大分配的专用 segment
    config: BuddyPoolConfig

    def alloc(self, size: int) -> Allocation:
        ...

    def free(self, allocation: Allocation):
        ...
```

### 4.2 配置参数

```python
@dataclass
class BuddyPoolConfig:
    segment_size: int = 256 * 1024 * 1024       # 256MB per segment
    min_block_size: int = 4096                   # 4KB 最小分配
    max_segments: int = 8                        # 常规 segment 上限 (总共 2GB)
    max_dedicated_segments: int = 4              # 超大分配的临时 segment 上限
    dedicated_gc_delay: float = 5.0              # dedicated segment free 后延迟 unlink (秒)
```

### 4.3 Segment 生命周期

- **初始**：创建 1 个 segment（懒创建，首次 alloc 时才 `shm_open`）
- **扩展**：当现有 segment 无法满足分配请求时，创建新 segment（直到 `max_segments`）
- **回收**：当一个 segment 的所有分配都被释放（`alloc_count == 0`）且不是唯一的 segment 时，可以 `shm_unlink` 回收
- **回收延迟**：避免频繁创建/销毁 segment 的抖动，空 segment 在空闲 `dedicated_gc_delay` 秒后才回收

## 五、内存超限处理

### 5.1 情况 A：数据比单个 segment 还大

比如 segment 大小 256MB，要塞入 500MB 的 fastdb 序列化结果。

**Buddy allocator 内部不可能分配超过 segment 大小的连续块。** 这是它的固有限制——buddy 的最大分配单元是整个 segment 的 data region。

**解法：Dedicated Segment（专用段）**

```
alloc(500MB):
    → 500MB > max_buddy_block (约 252MB)
    → 创建 dedicated SHM segment, size = 500MB (页对齐)
    → 整个 segment 只服务这一次分配，不走 buddy 管理
    → free() 时延迟 shm_unlink 整个 segment
```

这就是 Linux 内核 `kmalloc` 的做法——小分配走 slab/buddy，超大分配直接 `vmalloc` 拿整页。

**性能影响**：dedicated segment 需要 `shm_open` + `ftruncate` + `mmap`（创建）和 `munmap` + `shm_unlink`（释放），共 5 个系统调用。但对于 500MB 级别的数据，这 5 个系统调用的开销（~10μs）相对于传输耗时（~30ms）可以忽略。

### 5.2 情况 B：碎片化导致分配失败

Segment 有空间但不连续。例如 256MB segment，已分配 100MB，剩余 156MB 空闲但分散在多个不连续的 buddy block 中（64MB + 32MB + 32MB + 28MB...），此时要分配 128MB 的连续块会失败。

**三层降级策略：**

```
Layer 1: 尝试其他 segment
    for seg in segments:
        result = seg.buddy_alloc(128MB)
        if result: return result
    → 所有 segment 都碎片化了

Layer 2: 创建新 segment
    if len(segments) < max_segments:
        new_seg = create_segment(max(segment_size, requested_size))
        return new_seg.buddy_alloc(128MB)   # 新 segment 无碎片，一定成功
    → segment 数量也到上限了

Layer 3: 降级到 dedicated segment
    → 即使 max_segments 到了上限，dedicated segment 允许临时超限
    → create_dedicated(128MB)
    → 代价仅是 5 个系统调用，不会阻塞或失败
```

**不做 moving GC**：碎片整理需要移动已分配的 buffer，但 consumer 可能持有指向 SHM 的 memoryview，移动它会导致悬空指针。在跨进程 SHM 场景下这是不可解的。

**不做无限等待**：可能死锁（producer 等 consumer 释放，consumer 等 producer 发送下一步）。

### 5.3 完整分配决策流程

```
alloc(size):
    ┌─ size <= max_buddy_block?
    │
    │  YES:
    │   ├─ 遍历所有 segment 尝试 buddy_alloc
    │   │   ├─ 成功 → 返回 (seg_idx, offset, is_dedicated=false)
    │   │   └─ 全部失败
    │   │
    │   ├─ 创建新 segment 再试
    │   │   ├─ 成功 → 返回
    │   │   └─ 达到 max_segments
    │   │
    │   └─ 降级到 dedicated segment → 返回 (seg_idx, offset, is_dedicated=true)
    │
    │  NO (size > 单 segment 容量):
    │   └─ 直接创建 dedicated segment → 返回 (seg_idx, offset, is_dedicated=true)

free(seg_idx, offset, is_dedicated):
    ┌─ is_dedicated?
    │   ├─ YES → 延迟 shm_unlink 整个 segment     (5 syscalls, 罕见)
    │   └─ NO  → buddy_free + buddy_merge           (0 syscalls, 常规)
    │
    └─ 如果 segment 全空闲且不是最后一个 → 延迟回收
```

## 六、数据模式与 SHM 生命周期

### 6.1 两种数据模式

Buddy allocator 需要支持两种数据访问模式，它们共享相同的 SHM 分配/释放基础设施，但在 consumer 侧的数据访问方式不同：

| 维度 | Transferable 模式 (Phase 1) | fastdb 零拷贝模式 (Phase 2) |
|------|---------------------------|--------------------------|
| 序列化 | `Transferable.serialize()` → bytes | fastdb serialize → bytes |
| Consumer SHM 读取 | 零拷贝 memoryview 直读 SHM buddy block | 零拷贝 memoryview 直读 SHM buddy block |
| Consumer 访问 | `Transferable.deserialize(mv)` → Python 对象 | `ORM.load(mv)` → 直接在 SHM 上列式访问 |
| 总拷贝次数 | 2 次 (serialize + deserialize) | 1 次 (serialize 写入 SHM) |
| SHM 持有时间 | 短暂 (deserialize 完成即释放) | 长期 (直到所有派生 view 释放) |
| SHM 释放机制 | 与 fastdb 模式一致 | 引用计数 + GC 兜底 |
| 实现优先级 | **Phase 1 — 立即可用** | Phase 2 — 待 fastdb 接入完成 |

**关键设计原则**：两种模式共享完全相同的 SHM 生命周期管理机制（`ShmBacked` 包装器 + 引用计数）。Transferable 模式只是碰巧更早释放 SHM——因为 deserialize 将数据拷贝到了 Python 堆上，SHM 引用随即归零。

### 6.2 Transferable 模式数据链路 (Phase 1)

这是 C-Two 现有的数据传输模式，通过 `@transferable` 装饰器定义的 `serialize`/`deserialize` 方法进行数据编解码。

与当前 pool SHM 方案有两处关键改进：
1. SHM 分配从固定 segment 切换到 buddy allocator（精确分配、多请求并发）
2. **Consumer 侧 `deserialize()` 直接基于 SHM memoryview 操作**，省去当前 `read_from_pool_shm()` 中的 `memmove` 拷贝

当前管线中 `read_from_pool_shm()` 必须将 SHM 数据拷贝到 `AdaptiveBuffer`，原因是当前 pool SHM 一个请求占满整个 segment——读完必须立即拷出，否则下一个请求会覆写。Buddy allocator 中每个请求持有**独立的 block**，block 在 `free()` 前不会被复用，因此可以安全地将 SHM memoryview 直接传入 `wire.decode()` → `Transferable.deserialize()`。

pipeline 每一环均已支持 memoryview 输入：`wire.decode()` ✅、`pickle.loads()` ✅、用户自定义 `deserialize()` ✅。

```
=== Producer 侧 (CRM Server) ===

1. CRM 方法执行，返回结果对象 (e.g., GridAttribute)

2. Transferable.serialize(result) → bytes
   → 执行用户定义的 serialize 静态方法
   → 产生 bytes 对象 (Python 堆上)

3. C-Two 写入 SHM buddy block:
   (seg_idx, offset) = buddy_pool.alloc(len(data))
   pool.segments[seg_idx].buf[offset:offset+len(data)] = data    ← memcpy

4. 通过 UDS 发送通知帧: (seg_idx, offset, size, request_id)

=== Consumer 侧 (Component) ===

5. 收到通知帧, 获取 SHM 引用:
   shm_ref = ShmBacked(pool, seg_idx, offset, size)
   mv = shm_ref.memoryview                                       ← 零拷贝 (直接指向 SHM buddy block)

6. wire.decode(mv) → Envelope.payload (memoryview slice into SHM)  ← 零拷贝
   Transferable.deserialize(payload) → Python 对象
   → 执行用户定义的 deserialize 静态方法                            ← 反序列化 (deserialize 内部将数据构建为 Python 对象)
   → 这是整条链路中唯一的数据拷贝

7. SHM 立即释放 (deserialize 完成后 shm_ref 生命周期结束):
   shm_ref.__exit__() → buddy_pool.free(seg_idx, offset)         ← 零系统调用
```

> **3.14t 安全性**：Buddy pool 的 SHM segment 是连接级别长生命周期，`SharedMemory` handle 不会被频繁创建/销毁，因此不受 3.14t `SharedMemory.__del__` race condition 影响。

**与当前 IPC-v2 的对比**：

| 环节 | 当前 pool SHM | Buddy Allocator |
|------|-------------|----------------|
| 分配 | 占满整个 segment | 精确分配所需大小 |
| 并发 | 一个 segment 同时只能服务一个请求 | 多个请求共享同一 segment |
| Consumer SHM 读取 | memmove 拷贝到 AdaptiveBuffer | 零拷贝 memoryview 直读 SHM |
| 释放 | segment 级别回收 | block 级别回收 + buddy merge |
| 超大 payload | 退化为 per-request SHM (5 syscalls) | dedicated segment (同样 5 syscalls, 但自动降级) |

### 6.3 fastdb 零拷贝模式数据链路 (Phase 2, 后置)

> ⚠️ **此模式依赖 fastdb 的接入适配，当前处于设计预留阶段。实现优先级低于 Transferable 模式。**

fastdb 的核心优势在于它可以**直接基于内存 buffer 取值而无需反序列化**：

- `ORM.load(buffer)` 接受一个 contiguous buffer，直接在其上建立列式索引
- Feature 字段访问通过 `__array_interface__` 直接返回 buffer 上的 NumPy view
- 整个读取过程是零拷贝的——没有反序列化步骤

```
=== Producer 侧 (CRM Server) ===

1. fastdb ORM 操作:
   ORM.truncate() → 写入数据 → serialize → MemoryStream (C++ 堆上的 contiguous buffer)

2. Transferable.serialize() 将 MemoryStream 转为 Python bytes
   → C++ → Python 的一次拷贝 (不可避免, Python bytes 是不可变对象)

3. C-Two 写入 SHM buddy block:
   (seg_idx, offset) = buddy_pool.alloc(len(data))
   pool.segments[seg_idx].buf[offset:offset+len(data)] = data    ← memcpy (唯一一次)

4. 通过 UDS 发送通知帧: (seg_idx, offset, size, request_id)

=== Consumer 侧 (Component) ===

5. 收到通知帧, 获取 SHM 引用:
   shm_ref = ShmBacked(pool, seg_idx, offset, size)
   mv = shm_ref.memoryview                                       ← 零拷贝

6. fastdb ORM.load(mv):
   → 直接在 SHM 内存上建立列式索引                                  ← 零拷贝
   → Feature 字段访问直接返回 SHM 上的 NumPy view                  ← 零拷贝
   → 无反序列化步骤

7. Consumer 用完后 SHM 自动释放:
   → shm_ref 持有引用, 所有派生 view 释放后引用计数归零
   → buddy_pool.free(seg_idx, offset)                             ← 零系统调用
```

**fastdb 具体数据场景**（供未来参考）：

| fastdb 操作 | 典型大小 | Buddy 分配行为 |
|-------------|---------|---------------|
| ORM 单行查询结果 | 100B - 4KB | inline UDS, 不走 SHM |
| 批量 scalar 读取 | 4KB - 1MB | 分配一个小 buddy block (Level 12-16) |
| 列式 columnar 导出 (NumPy) | 1MB - 100MB | 分配一个中等 block (Level 4-8) |
| FastSerializer 图序列化 | 10MB - 1GB | 分配大 block, 可能 dedicated segment |
| TileBoxTake 多 tile 查询 | 50MB - 500MB | 分配大 block 或 dedicated segment |
| 前端可视化 mesh 资源 | 10MB - 200MB | 通过 Rust Router 路由, 共享 buddy pool |

### 6.4 统一的 SHM 生命周期管理

两种数据模式共享完全相同的 `ShmBacked` 生命周期管理器。差异仅在于 consumer 持有 `ShmBacked` 的时长。

#### ShmBacked 包装器

```python
class ShmBacked:
    """统一的 SHM buddy block 生命周期管理器

    Transferable 模式和 fastdb 模式共用此包装器。
    差异仅在于 consumer 持有 ShmBacked 的时长:
    - Transferable: deserialize 完成即释放 (context manager 短生命周期)
    - fastdb: 所有派生 view 释放后才释放 (引用计数长生命周期)
    """

    def __init__(self, pool, seg_idx: int, offset: int, size: int):
        self._pool = pool
        self._seg_idx = seg_idx
        self._offset = offset
        self._size = size
        self._released = False
        # GC 兜底: weakref callback 确保即使忘记显式释放也能回收
        self._release_fn = _make_release_closure(pool, seg_idx, offset)
        self._ref = weakref.ref(self, self._release_fn)

    @property
    def memoryview(self) -> memoryview:
        """零拷贝获取 SHM 数据的 memoryview"""
        seg = self._pool.segments[self._seg_idx]
        return seg.buf[self._offset:self._offset + self._size]

    # --- Context manager: 推荐用法 (Transferable 模式) ---

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.release()

    # --- 显式释放 ---

    def release(self):
        if not self._released:
            self._pool.free(self._seg_idx, self._offset)
            self._released = True
            self._pool = None


def _make_release_closure(pool, seg_idx, offset):
    """创建独立于 ShmBacked 实例的释放闭包, 供 weakref callback 使用"""
    def _release(ref):
        pool.free(seg_idx, offset)
    return _release
```

#### 两种模式的使用方式

```python
# --- Transferable 模式 (Phase 1): 短生命周期, context manager ---

def _handle_transferable_response(pool, seg_idx, offset, size, deserialize_fn):
    with ShmBacked(pool, seg_idx, offset, size) as shm_ref:
        mv = shm_ref.memoryview         # 零拷贝 slice
        result = deserialize_fn(mv)     # 反序列化: 数据拷出 SHM → Python 堆
    # __exit__ 自动调用 release() → buddy_pool.free()
    # SHM block 已回收, result 是独立的 Python 对象
    return result


# --- fastdb 模式 (Phase 2): 长生命周期, 引用计数 ---

def _handle_fastdb_response(pool, seg_idx, offset, size):
    shm_ref = ShmBacked(pool, seg_idx, offset, size)
    db = ORM.load(shm_ref.memoryview)   # 零拷贝: 直接在 SHM 上建索引
    # shm_ref 的生命周期绑定到 db 对象
    # 当 db 及其所有派生 NumPy view 被 GC 时, weakref callback 触发 free()
    return db, shm_ref  # caller 持有 shm_ref 直到不再需要
```

#### Buffer 引用追踪 (fastdb 模式的额外需求)

如果 Feature 的 memoryview 被 NumPy array 引用了，即使 Feature 对象释放了，NumPy array 仍在读 SHM。解法：让 SHM block 包装为一个 Python buffer 对象（实现 `__buffer__` protocol），Python 的 buffer export 引用计数会自动追踪所有派生的 memoryview。只有所有派生 view 都释放后，buffer 对象的引用计数才归零，才触发 `free()`。

> 此机制仅 fastdb 模式需要。Transferable 模式中 deserialize 完成后数据已在 Python 堆上，不存在 SHM 悬空引用问题。

#### 设计注意事项

1. **`__del__` 不可靠**：CPython 中循环引用场景下 `__del__` 可能不被调用。推荐用 `weakref.ref(obj, callback)` 作为 GC 兜底，同时鼓励用户用 context manager (`with` 语句) 显式释放。

2. **相比 Ray Plasma 更轻量**：Ray 需要分布式引用计数（跨节点追踪 ObjectRef），C-Two 的场景是单节点跨进程，Python 本地 refcount 即可，不需要分布式协调。

### 6.5 拷贝次数对比

| 环节 | 当前 pool SHM (Transferable) | Buddy + Transferable | Buddy + fastdb |
|------|------------------------------|----------------------|----------------|
| Producer serialize | 1 次 | 1 次 | 1 次 |
| 写入 SHM | 1 次 memcpy | 1 次 memcpy | 1 次 memcpy |
| Consumer SHM 读取 | 1 次 memmove (→ AdaptiveBuffer) | **零拷贝** (memoryview 直读) | **零拷贝** (memoryview 直读) |
| Consumer 反序列化 | 1 次 (deserialize) | 1 次 (deserialize) | 无 |
| **总拷贝次数** | **3 次** | **2 次** | **1 次** |
| SHM 持有时间 | ~μs (拷贝后立即归还 segment) | ~μs (deserialize 期间持有 block) | ~ms-s (整个 consumer 使用期间) |

Buddy allocator 相比当前方案，Transferable 模式从 3 次拷贝降至 2 次（省去 SHM → AdaptiveBuffer 的 memmove），fastdb 模式仅 1 次拷贝（Producer 写入 SHM）。两种模式在不同场景各有优势——Transferable 适合中等数据量的高频 RPC，fastdb 适合大规模数据的低频交换。

## 七、实现方案

### 7.1 语言选择：Rust

与 C-Two 已规划的 Rust Routing Server 共享构建链和分发流程：

- Buddy allocator 作为 `c2-ipc` Rust crate 的一部分
- 通过 `cffi` 暴露给 Python（接口极简, 3-4 个函数）
- 独立于 fastdb 的 SWIG 工具链, 不增加额外依赖

### 7.2 暴露给 Python 的接口

```python
# cffi 接口 (由 Rust cdylib 提供)

def pool_create(config: BuddyPoolConfig) -> PoolHandle:
    """创建 buddy pool (懒创建, 首次 alloc 时才 shm_open)"""

def pool_destroy(handle: PoolHandle):
    """销毁 pool, unlink 所有 segment"""

def pool_alloc(handle: PoolHandle, size: int) -> tuple[int, int, int, bool]:
    """分配内存, 返回 (seg_idx, offset, actual_size, is_dedicated)"""

def pool_free(handle: PoolHandle, seg_idx: int, offset: int, is_dedicated: bool):
    """释放内存 (buddy merge 或 segment unlink)"""

def pool_get_memoryview(handle: PoolHandle, seg_idx: int, offset: int, size: int) -> memoryview:
    """获取 SHM 中指定区域的零拷贝 memoryview"""

def pool_stats(handle: PoolHandle) -> PoolStats:
    """返回 pool 统计信息 (总大小, 已用, 空闲, segment 数, 碎片率)"""
```

### 7.3 Rust crate 组织

```
c-two-rs/
├── crates/
│   ├── c2-wire/          # wire format 编解码
│   ├── c2-buddy/         # buddy allocator (本文档描述的核心)
│   │   ├── src/
│   │   │   ├── allocator.rs    # buddy 分配/释放/合并
│   │   │   ├── bitmap.rs       # 层级 bitmap 操作
│   │   │   ├── segment.rs      # SHM segment 管理
│   │   │   ├── pool.rs         # 多 segment pool + dedicated segment
│   │   │   ├── spinlock.rs     # 跨进程原子自旋锁
│   │   │   └── ffi.rs          # C ABI 导出 (供 cffi 调用)
│   │   └── Cargo.toml
│   ├── c2-ipc/           # IPC-v2 client (UDS + buddy pool)
│   └── c2-router/        # HTTP routing server (axum + c2-ipc)
├── Cargo.toml            # workspace
└── ...
```

## 八、与当前设计的兼容性

### 渐进式替换路径

1. **保留 inline 路径**（< `shm_threshold`, 默认 4KB）：小数据走 UDS frame 内联, 零拷贝, 零额外 RT。这条路径无需改动。

2. **用 buddy pool 替换 pool SHM + per-request SHM**：
   - 中等数据（4KB - segment 容量）：buddy 分配, consumer 零拷贝 memoryview
   - 大数据（> segment 容量）：dedicated segment, 自动降级

3. **per-request SHM 回退路径完全消除**：buddy pool + dedicated segment 覆盖所有 size 范围。

### Wire format 变更

当前 `FLAG_POOL` 帧的 payload 是 `[1B segment_index][8B data_size]`。Buddy 方案需要扩展为：

```
FLAG_POOL payload (buddy 模式):
[1B segment_index][4B offset_in_data_region][8B data_size][1B flags]
                   ↑ 新增: block 在 segment 内的偏移
                                                          ↑ bit0: is_dedicated
```

兼容性：通过 handshake 版本号协商（当前已有 v3 机制），buddy 模式使用 v4 handshake。

## 九、与流式窗口方案的对比

| 维度 | 流式窗口 | Buddy Allocator |
|------|---------|----------------|
| 拷贝次数 | 2 次 (写入 + 拷出窗口) | 1 次 (写入 SHM) |
| Consumer 访问 | 拷贝到本地 buffer 后访问 | 零拷贝 memoryview 直读 SHM |
| fastdb 集成 | 需要反序列化 | 可直接在 SHM 上列式访问 |
| 大数据支持 | 流式分 chunk | dedicated segment |
| 碎片处理 | 不适用 (窗口固定大小) | buddy merge 自动合并 |
| 并发请求 | 窗口池复用 | 多 block 独立分配, 天然并发 |
| 实现复杂度 | 中 (Python) | 中高 (Rust, 但一次性投入) |
| 依赖 | 无 | Rust cffi |

**结论**：Buddy allocator 在拷贝次数和数据模式灵活性上优于流式窗口方案。Phase 1 先支持 C-Two 现有 transferable 模式（即时可用），Phase 2 再接入 fastdb 零拷贝模式（待 fastdb 适配完成）。统一的 `ShmBacked` 生命周期管理确保两种模式的 SHM 释放逻辑完全一致，是更适合 C-Two 长期架构的选择。
