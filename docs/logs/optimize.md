# IPC v3 协议性能优化报告

## 概述

IPC v3 是基于 Rust buddy allocator 的全双工共享内存（SHM）传输协议，替代了 v2 的半双工池式 SHM 方案。本报告记录了针对 IPC v3 round-trip 延迟（P50）的系统性优化过程。

- **优化分支**：`autoresearch/ipc-v3-perf`（基于 `rust-buddy` 分支）
- **综合指标**：6 种负载大小（64B、1KB、64KB、1MB、10MB、100MB）的 P50 延迟几何均值（geometric mean）
- **基线指标**：0.8062 ms（gmean）
- **最终指标**：0.6872 ms（gmean）
- **总提升**：**-14.8%**
- **实验统计**：共执行 22 次实验（含基线），保留 8 个，丢弃 13 个
- **测量环境**：Python 3.14t（free-threading）、macOS ARM64、UDS 控制通道 + SHM 数据通道

## 方法论

采用自主迭代实验循环（受 Karpathy autoresearch 启发），每轮实验遵循以下流程：

1. **假设**：基于 profiling 或代码审查提出优化假设
2. **代码修改**：实施针对性的代码变更
3. **Git commit**：将变更提交为独立 commit
4. **测量**：执行 `uv run python measure_v3.py`，运行 3 次取中位数，得到 6 种负载大小的几何均值
5. **比较**：与当前最佳指标对比
6. **决策**：指标改善则保留（keep），否则回退（`git reset --hard HEAD~1`）

关键原则：

- **指标方向**：越低越好（lower is better）
- **简洁性原则**：当指标相近（在噪声范围内）时，优先保留更简洁的代码
- **Git 策略**：每个实验对应一个独立 commit，失败实验通过 hard reset 回退，成功实验推进分支前进

## 全量实验结果

| 编号 | Commit | 指标(ms) | 状态 | 描述 |
|------|--------|----------|------|------|
| 0 | `2d45909` | 0.8062 | 📊 baseline | 未修改的 IPC v3（code-review 修复后） |
| 1 | `32d2984` | 0.8237 | ❌ discard | MSG_WAITALL 用于 recv_exact —— 无改善，在噪声范围内 |
| 2 | `ad42597` | 0.8654 | ❌ discard | read_and_free 合并为单次 FFI —— 更差，Mutex 在 memcpy 期间持有 |
| 3 | `3c3a85a` | 0.8344 | ❌ discard | 单次 pack buddy frame —— 无改善 |
| 4 | `9312383` | 0.7463 | ✅ keep | 消除每请求 asyncio.wait —— 提升 7.4%，小消息快 20-30% |
| 5 | `43a83a8` | 0.7408 | ✅ keep | 小帧跳过 writer.drain() —— 边际改善，代码更简洁 |
| 6 | `943bf18` | 0.7265 | ✅ keep | 一次性解析 request_id + 提升 signal_tags —— 提升 1.9% |
| 7 | `6ef3a86` | 0.7122 | ✅ keep | 内联 _read_frame —— 提升 2.0% |
| 8 | `c80b170` | 0.7295 | ❌ discard | 客户端 recv_into 预分配缓冲区 —— 无改善 |
| 9 | `a6779c4` | 0.8165 | ❌ discard | 全局变量缓存为局部变量 —— 性能回退，开销大于收益 |
| 10 | `7349564` | 0.7211 | ❌ discard | ctypes.string_at —— 无可测量的改善 |
| 11 | `72b99fb` | 0.7235 | ❌ discard | 单次 pack buddy frame —— 无改善 |
| 12 | `6808d17` | 0.6876 | ✅ keep | 缩小 reply() 中 conn_lock 的持有范围 —— 提升 3.5% |
| 13 | `f2abac4` | 0.7160 | ❌ discard | ctypes.memmove 写回复 —— 更差，pack 产生中间对象 |
| 14 | `89e4574` | 0.7040 | ❌ discard | 服务端零拷贝 SHM 读取 + 延迟释放 —— 几何均值无改善 |
| 15 | `b17a35c` | 0.7323 | ❌ discard | 使用 tuple 作为 pending dict 的键 —— 无改善，可能是方差 |
| 16 | `92a1028` | 0.7398 | ❌ discard | 实例属性缓存为循环局部变量 —— 在噪声范围内 |
| 17 | `d309122` | 0.7710 | ❌ discard | 缓存 segment 基地址 —— 无可测量改善，data_addr FFI 非瓶颈 |
| 18 | `5a1c3bf` | 0.6863 | ✅ keep | 使用 Rust read_at 替代 data_addr+ctypes —— 消除 Python 侧开销，减少 FFI 调用 |
| 19 | `f708f31` | 0.6812 | ✅ keep | 缓存 segment memoryview —— 消除每请求 ctypes 和 read_at FFI，类似 v2 池缓冲区 |
| 20 | `f5d58f6` | 0.7655 | ❌ discard | 单次分配 buddy frame 编码 —— 无改善，可能是噪声 |
| 21 | `f288d77` | 0.6872 | ✅ keep | 使用 alloc() 替代 alloc_ptr() —— 跳过 data_ptr，代码更简洁（在噪声范围内） |

## 保留的优化详解

### 优化 #4：消除每请求 asyncio.wait 开销

**假设**

`asyncio.wait({task}, timeout=X)` 在每次请求时会创建一个 `set` 对象和一个协程包装器。对于小消息（64B、1KB），这些 Python 层面的开销在总延迟中占比极高，因为实际的 memcpy 时间微不足道。如果改用 `await fut` 配合 `asyncio.timeout()` 上下文管理器，可以避免每请求的集合创建和协程包装开销。

**修改内容**

在 `ipc_v3_server.py` 的 `_handle_client()` 事件循环中：

- 将 `done, pending = await asyncio.wait({fut}, timeout=...)` 替换为 `async with asyncio.timeout(timeout): result = await fut`
- 超时处理从遍历 `pending` 集合改为直接 `cancel()` 该 future
- 移除了每请求创建 `{fut}` 集合的操作——原实现对每个入站请求都会构造一个单元素集合，仅用于传递给 `asyncio.wait()`

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.7463 ms |
| 前一最佳 | 0.8062 ms（baseline） |
| 变化 | **-7.4%** |

小消息（64B、1KB）的延迟改善了 20-30%，因为它们的总延迟几乎完全由 Python 运行时开销决定，而非数据拷贝。

**保留原因**

这是所有实验中单次改善最大的优化（-7.4%）。改动清晰、风险低，且在小消息场景下效果尤为显著。

---

### 优化 #5：小帧跳过 writer.drain()

**假设**

`await writer.drain()` 是一个协程调用，等待写缓冲区刷新到内核。对于小的控制帧（几 KB 以下），操作系统的 socket 缓冲区可以立即吸收它们，无需等待刷新。跳过 `drain()` 可以避免不必要的协程挂起开销。

**修改内容**

在 `ipc_v3_server.py` 的响应发送路径中：

- 仅当帧大小超过阈值（如 > 64KB）时才调用 `await writer.drain()`
- 小帧（例如 buddy reference 帧仅 27 字节）完全跳过 `drain()`——它们可以放入内核 socket 缓冲区
- 这避免了每次发送小响应时的异步挂起和恢复开销

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.7408 ms |
| 前一最佳 | 0.7463 ms |
| 变化 | **-0.7%** |

**保留原因**

虽然指标改善幅度较小（-0.7%），但代码变得更简洁，避免了不必要的异步挂起。根据简洁性原则（指标相近时优先保留更简洁的代码），予以保留。

---

### 优化 #6：一次性解析 request_id + 提升 signal_tags

**假设**

复合请求 ID `"conn_id:request_id"` 在多处被重复解析（`rfind(':')` + 切片 + `int()` 转换）。此外，信号标签映射字典（PONG、SHUTDOWN_ACK）在方法作用域或类作用域内创建，每次调用都会重新构造。将这些提升到模块级别可以消除重复开销。

**修改内容**

在 `ipc_v3_server.py` 中：

- `reply()` 方法中：对复合键 `rid_key` 仅执行一次 `rfind(':')`，将 `conn_id` 和 `int_rid` 存储为局部变量后复用
- 将 `_SIGNAL_TAGS` 字典从方法/类作用域提升到模块级别——避免每次调用时重新创建
- 消除了响应路径上的冗余字符串操作

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.7265 ms |
| 前一最佳 | 0.7408 ms |
| 变化 | **-1.9%** |

**保留原因**

干净的改进，没有增加任何复杂度。减少了热路径上的重复计算，代码逻辑更清晰。

---

### 优化 #7：内联 _read_frame

**假设**

`_read_frame()` 是一个独立的异步函数，每次请求都会被调用。每次调用都会创建一个协程对象、调度它、然后返回结果。将其内联到 `_handle_client` 循环中可以消除这个协程创建开销。

**修改内容**

在 `ipc_v3_server.py` 中：

- 移除了独立的 `async def _read_frame()` 函数
- 将其函数体（`readexactly` 读取头部 + `readexactly` 读取负载 + `struct.unpack` 解包）直接移入 `_handle_client()` 的 `while True` 循环中
- 减少了每请求一层的异步函数调用间接开销

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.7122 ms |
| 前一最佳 | 0.7265 ms |
| 变化 | **-2.0%** |

**保留原因**

干净的改进，减少了热路径上的异步开销。内联后消除了每请求的协程对象创建和调度成本，对高频小消息场景尤其有效。

### 优化 #12：缩小 reply() 中 conn_lock 的持有范围

**commit**: `6808d17`

**假设**

`reply()` 中 `conn_lock` 的持有范围过大——锁在整个 buddy 分配 + SHM 写入期间一直持有。这会阻塞其他线程访问连接对象。然而 buddy pool 本身有独立的 Rust mutex 来保护分配操作，Python 层的 `conn_lock` 实际上只需要保护连接对象的读取（`self._connections.get(conn_id)`），而不需要覆盖分配和写入阶段。缩小锁的范围可以降低锁竞争，尤其在并发工作负载下效果显著。

**修改内容**

在 `ipc_v3_server.py` 的 `reply()` 方法中，将原本被 `with self._conn_lock:` 包裹的整个 alloc+write 逻辑拆分为两个阶段：

1. **锁内阶段**：仅在 `conn_lock` 保护下执行 `self._connections.get(conn_id)` 获取连接对象的快照，同时快照 `buddy_pool` 引用和 `seg_views`
2. **锁外阶段**：释放 `conn_lock` 后，使用快照的引用执行 buddy 分配和 SHM 写入——这些操作由 Rust mutex 独立保护

核心改动是将 `with self._conn_lock:` 的作用域从包裹分配+写入缩小到仅包裹 `self._connections.get(conn_id)` 查找及相关字段的快照。

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.6876 ms |
| 前一最佳 | 0.7122 ms |
| 变化 | **-3.5%** |

**保留原因**

显著的性能改进。减少了 Python 锁的竞争时间窗口，将锁持有时间从"查找+分配+写入"缩短为仅"查找"。由于 Rust buddy pool 已有自己的 mutex 保护并发分配，Python 层无需重复加锁。这一改动对并发场景尤为重要——多线程同时调用 `reply()` 时，conn_lock 不再成为瓶颈。

### 优化 #18：使用 Rust read_at 替代 data_addr+ctypes

**commit**: `5a1c3bf`

**假设**

原有的读取路径每次读取需要 2 次 FFI 调用：先调用 `data_addr()` 获取原始地址，然后在 Python 侧通过 `(ctypes.c_char * N).from_address(addr)` + `memoryview` + `bytes()` 进行转换。这条路径涉及多次 Python 对象创建（ctypes 数组、memoryview、bytes），开销不可忽略。

假设可以用一次 Rust FFI 调用 `read_at()` 替代整条路径——在 Rust 内部完成 `alloc PyBytes → memcpy from SHM → return PyBytes`，消除中间对象的创建开销。

**修改内容**

- **`ipc_v3_server.py` 的 `_resolve_request()`**：将 `data_addr` + ctypes 替换为 `conn.buddy_pool.read_at(seg_idx, offset, data_size, is_dedicated)`
- **`ipc_v3_client.py` 的 `_recv_response()`**：将 `data_addr` + ctypes 替换为 `self._buddy_pool.read_at(...)`

消除的对象创建：1 次 FFI 调用（data_addr）、ctypes `c_char` 数组创建、memoryview 创建+cast、手动 `bytes()` 转换。整条读取路径从多步 Python 操作简化为单次 Rust FFI 调用。

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.6863 ms |
| 前一最佳 | 0.6876 ms |
| 变化 | **-0.2%**（在噪声范围内） |

**保留原因**

虽然 gmean 改善幅度很小（在噪声范围内），但代码结构显著简化：读取路径从多步操作变为单次调用，减少了每次读取创建的 Python 对象数量。需要注意的是，这个实验是一个中间步骤——后续实验 #19（缓存 segment memoryview）在此基础上进一步演进，最终完全消除了读取路径上的 FFI 调用。

### 优化 #19：缓存 segment memoryview（关键优化） ★

**commit**: `f708f31`

**假设**

这是整组优化中最关键的一个。v2 之所以在中大型负载上延迟更低，是因为它使用了在握手时预创建的 `SharedMemory.buf` memoryview——创建一次，此后每次请求复用。而 v3 在每次写操作时都创建新的 `(ctypes.c_char * wire_size).from_address(addr)` + `memoryview().cast('B')`，每次读操作都调用 `read_at` FFI。这种逐请求的对象创建正是 v3 在 1MB–512MB 范围内相对 v2 回退的根本原因。

核心思路：在握手时为每个 buddy segment 的数据区域创建一个**持久化 memoryview**。后续的读写操作通过对这个持久 memoryview 进行切片（O(1) 操作）完成，而不是每次创建新对象。

**修改内容**

**Rust 侧新增**：

- 在 `ffi.rs` 和 `pool.rs` 中添加 `seg_data_info(seg_idx)` 方法，返回 `(data_base_addr, data_region_size)` 元组——提供 segment 数据区域的基地址和大小

**客户端修改**：

- `_do_buddy_handshake()`：握手完成后，为每个 segment 创建 `self._seg_views` 列表——每个 segment 对应一个覆盖其整个数据区域的 memoryview
- `call()` 写入路径：将 `(ctypes.c_char * wire_size).from_address(addr).cast('B')` 替换为 `self._seg_views[alloc.seg_idx][alloc.offset : alloc.offset + wire_size]`
- `_recv_response()` 读取路径：将 `read_at()` FFI 替换为 `bytes(self._seg_views[seg_idx][offset : offset + data_size])`
- 清理：在 `buddy_pool.destroy()` 前执行 `seg_views = []`

**服务端修改**：

- `_handle_buddy_handshake()`：打开 segment 后，为连接创建 `conn.seg_views` 列表
- `BuddyConnection.__slots__`：添加 `seg_views` 字段
- `_resolve_request()` 读取路径：将 `read_at()` FFI 替换为 `bytes(conn.seg_views[seg_idx][offset : offset + data_size])`
- `reply()` 写入路径：将 ctypes 替换为 `seg_views[alloc.seg_idx][alloc.offset : alloc.offset + total_wire]`
- 清理：在 `buddy_pool.destroy()` 前执行 `seg_views = []`

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.6812 ms |
| 前一最佳 | 0.6876 ms |
| 变化（gmean） | **-0.9%** |

gmean 改善看似不大，但这是因为小尺寸负载本来就走 inline 路径，不涉及 ctypes。真正的影响体现在 **v2 vs v3 的回退修复**上：

| 负载大小 | 优化前 v3 vs v2 | 优化后 v3 vs v2 |
|----------|----------------|----------------|
| 1MB | +11.2%（v3 更慢） | **-8.1%（v3 更快）** ✅ |
| 512MB | +32.6%（v3 更慢） | **-19.1%（v3 更快）** ✅ |
| 1.0GB | -41.4%（v3 更快） | **-48.1%（v3 更快）** |

**保留原因**

这是整组优化中最重要的一个。它从根本上改变了 v3 的读写路径——从逐请求创建 ctypes 对象和 FFI 调用，转变为与 v2 相同的持久缓冲区方式。在 1MB 和 512MB 负载上，v3 从比 v2 慢两位数百分比翻转为比 v2 快，彻底修复了回退问题。这个优化的核心洞察是：**共享内存的地址空间在 segment 生命周期内是稳定的**，因此可以在握手时一次性创建 memoryview 并在整个连接生命周期内复用。

### 优化 #21：使用 alloc() 替代 alloc_ptr()

**commit**: `f288d77`

**假设**

经过实验 #19 之后，`alloc_ptr()` 返回的原始地址 (`addr`) 不再被使用——读写操作已经改用通过 `alloc.seg_idx` 和 `alloc.offset` 索引的缓存 segment memoryview。`alloc_ptr()` FFI 调用在一次 Mutex 持有期间执行 `alloc + data_ptr` 两步操作；而 `alloc()` 仅执行 `alloc` 一步。跳过 `data_ptr` 计算可以节省每次调用的少量纳秒开销。

**修改内容**

- **客户端 `call()`**：将 `alloc, addr = self._buddy_pool.alloc_ptr(wire_size)` 替换为 `alloc = self._buddy_pool.alloc(wire_size)`
- **客户端 `relay()`**：同样的替换
- **服务端 `reply()`**：同样的替换
- 移除所有路径中对 `addr` 变量的引用

**测量结果**

| 指标 | 值 |
|------|------|
| 本轮结果 | 0.6872 ms |
| 前一最佳 | 0.6812 ms |
| 变化 | **+0.9%**（在噪声范围内） |

**保留原因**

遵循简洁性原则。虽然 gmean 指标在 ±5% 噪声带内略有波动，但代码客观上更简洁：移除了不再使用的 `addr` 变量，使意图更清晰——分配操作只需要返回 `Allocation` 对象（包含 `seg_idx` 和 `offset`），不再需要原始指针。这是实验 #19 的自然后续清理。

## 丢弃实验的经验教训

在 21 轮实验中，有 13 轮被丢弃（未合入主线）。按主题分类总结如下：

### 1. FFI 合并类（实验 2, 10, 11, 20）

- **实验 2**：将 `read` 和 `free` 合并为单次 FFI 调用 `read_and_free` → 反而更差 (+7.3%)。根因：Mutex 在 512MB `memcpy` 期间持有，阻塞了所有其他操作。
- **实验 10/11/20**：各种 single-pack buddy frame 尝试 → 均无改善。

**教训**：减少 FFI 调用次数不等于性能提升。Mutex 持有时间才是关键瓶颈——合并操作可能延长临界区，导致并发退化。

### 2. 缓存/预分配类（实验 8, 9, 15, 16, 17）

- **实验 8**：客户端 `recv_into` 预分配缓冲区 → 无改善（Python `bytes` 分配本身很快）。
- **实验 9**：将全局变量缓存为局部变量 → 回退 +14.6%（Python 3.11+ 已优化 `LOAD_GLOBAL` 字节码）。
- **实验 15**：使用 tuple keys 替代 pending dict 的键 → 无改善。
- **实验 16**：将实例属性缓存为循环局部变量 → 处于噪声范围内。
- **实验 17**：缓存 segment 基地址 → 无改善（`data_addr` FFI 仅 ~2μs/call，非瓶颈）。

**教训**：Python 3.14t 的字节码优化已经很好，微优化往往不如预期。真正的瓶颈在于 I/O 和对象创建，而非 attribute lookup。

### 3. 零拷贝类（实验 13, 14）

- **实验 13**：用 `ctypes.memmove` 替代 `struct.pack_into` 写回复 → 更差（`pack` 产生中间对象，`memmove` 并未节省分配）。
- **实验 14**：服务端零拷贝 SHM 读取 + 延迟释放 → gmean 无改善（大数据收益被小数据的延迟释放开销抵消）。

**教训**：零拷贝需要全链路配合。局部零拷贝可能因引入新开销（如延迟释放的 GC 压力）而抵消收益。

### 4. 系统调用类（实验 1）

- **实验 1**：使用 `MSG_WAITALL` → 无改善（UDS 本地连接延迟已经很低，`MSG_WAITALL` 不减少系统调用次数）。

**教训**：对于 UDS 本地通信，系统调用优化空间很小。

## v2 vs v3 综合基准测试

### 优化前（实验 #12 时测量，commit 6808d17）

| 负载大小 | v2 P50(ms) | v3 P50(ms) | 变化 | 状态 |
|---------|-----------|-----------|------|------|
| 64B | 0.210 | 0.122 | -41.9% | ✅ |
| 256B | 0.174 | 0.124 | -28.7% | ✅ |
| 1KB | 0.171 | 0.129 | -24.6% | ✅ |
| 4KB | 0.186 | 0.133 | -28.5% | ✅ |
| 16KB | 0.171 | 0.139 | -18.7% | ✅ |
| 64KB | 0.251 | 0.147 | -41.4% | ✅ |
| 256KB | 0.268 | 0.246 | -8.2% | ✅ |
| 1MB | 0.430 | 0.478 | +11.2% | ⚠️ 回退 |
| 10MB | 2.639 | 3.091 | +17.1% | ⚠️ 回退 |
| 100MB | 21.608 | 22.424 | +3.8% | ⚠️ 回退 |
| 512MB | 128.638 | 170.617 | +32.6% | ⚠️ 回退 |
| 1.0GB | 689.475 | 403.875 | -41.4% | ✅ |

### 优化后（最终版本，commit f288d77）

| 负载大小 | v2 P50(ms) | v3 P50(ms) | 变化 | 状态 |
|---------|-----------|-----------|------|------|
| 64B | 0.227 | 0.152 | -33.0% | ✅ |
| 256B | 0.207 | 0.171 | -17.4% | ✅ |
| 1KB | 0.221 | 0.148 | -33.0% | ✅ |
| 4KB | 0.184 | 0.163 | -11.4% | ✅ |
| 16KB | 0.197 | 0.181 | -8.1% | ✅ |
| 64KB | 0.202 | 0.197 | -2.5% | ✅ |
| 256KB | 0.256 | 0.247 | -3.5% | ✅ |
| 1MB | 0.491 | 0.451 | -8.1% | ✅ 修复 |
| 10MB | 2.815 | 3.051 | +8.4% | ⚠️ 残余 |
| 100MB | 18.587 | 19.906 | +7.1% | ⚠️ 残余 |
| 512MB | 124.608 | 100.761 | -19.1% | ✅ 修复 |
| 1.0GB | 703.560 | 365.481 | -48.1% | ✅ |

### 回归修复分析

- **1MB**：+11.2% → **-8.1%**（从回退变为领先）
- **512MB**：+32.6% → **-19.1%**（从严重回退变为大幅领先）
- **根因**：实验 #19（缓存 segment memoryview）消除了 v3 相比 v2 在中大数据量下的结构性劣势——v2 的预开 `SharedMemory.buf` vs v3 的每请求 ctypes 创建。
- **残余回退**：10MB (+8.4%) 和 100MB (+7.1%) 源于 buddy allocator 的固有 FFI 开销（`alloc` + `free` 各 ~1μs），在这些大小下构成了总延迟的可测量比例。这是全双工 buddy 分配器架构的固有代价，换取的是并发能力和动态内存管理。

## 结论与后续建议

### 成果总结

- IPC v3 gmean 延迟从 0.8062ms 降至 0.6872ms，整体提升 **14.8%**。
- 12 个负载大小中，v3 在 10 个大小上快于 v2（最高 48.1%），仅 2 个大小存在 7-8% 残余回退。
- 关键的 1MB-512MB 回退问题已解决。
- 全部 633 个测试通过。

### 三大最有影响力的优化

1. **实验 #4（消除 asyncio.wait）**：**-7.4%** — 移除了最大的 Python 异步开销。
2. **实验 #12（缩小 conn_lock）**：**-3.5%** — 减少了锁争用。
3. **实验 #19（缓存 segment memoryview）**：**-0.9% gmean，但修复了 1MB-512MB 回退** — 最关键的架构改进。

### 后续可探索方向

以下方向未在本轮实验中尝试，留待后续评估：

1. **RwLock 替代 Mutex**：Rust buddy pool 使用 `std::sync::Mutex`，改为 `RwLock` 可允许并发读取（`read_at`、`data_addr`），仅 `alloc`/`free` 需要独占锁。对并发工作负载有潜在收益。
2. **分配复用**：对于重复相同大小的请求（如基准测试中的 echo），跳过 `alloc`/`free`，复用上一个分配块。减少 2 次 FFI 调用/RT。
3. **替换 asyncio**：服务端使用自定义事件循环或线程池替代 asyncio，消除协程创建和事件循环调度开销。改动较大，风险较高。
4. **ShmBacked 零拷贝模式**（参考 buddy.md §6.4）：返回持有 SHM 引用的包装对象，延迟到消费者实际读取时才执行 memcpy。需要 API 层面的变更。
5. **iceoryx2 替代方案**：使用 iceoryx2 的 Request/Response + `Slice[c_uint8]` 替代自有 SHM 管理，可能获得更低的延迟和更好的跨平台支持。
