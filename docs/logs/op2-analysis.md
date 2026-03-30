# IPC v3 大载荷优化审计报告 (op2-analysis)

**审计日期**: 2026-03-25
**审计对象**: `doc/ru-op2.md` 中描述的 8 个保留实验
**审计重点**: (1) 是否存在"作弊"——仅对 benchmark 有效而无实际应用场景；(2) 是否引入安全性和并发漏洞
**审计基准**: `doc/ru-op1.md` 中指出的已有问题

---

## 审计方法

对每个优化实验，从四个维度严格审查：

1. **作弊性 (Benchmark Gaming)**: 优化是否仅在 echo 基准测试中生效？对真实 CRM 方法（有数据变换）是否有效？
2. **安全性回归**: 是否引入 use-after-free、越界访问、SHM 数据泄露？
3. **并发性回归**: 是否引入新的竞争条件、死锁、锁争用？
4. **正确性**: 边界条件处理、错误路径、协议兼容性

---

## 一、实验 1：Server 零拷贝请求读取 + 延迟释放

### 作弊性评估：**非作弊——对所有 CRM 方法有效**

此优化将 `_resolve_request` 中的 `bytes(seg_mv[offset:offset+size])` 替换为直接返回 `memoryview` 切片。`wire.decode()` 全链路支持 memoryview 输入，`Envelope.payload` 也是 memoryview 切片，因此**所有 CRM 方法**都受益于此优化——不仅限于 echo。只要 CRM 方法接收超过 SHM 阈值的参数，就免去了一次完整的 memcpy。

### 安全性回归：**存在 use-after-free 窗口** [IMPORTANT]

**问题 OPT-S1**: 延迟释放设计创造了一个窗口：SHM memoryview 仍然活跃，但 buddy block 可能被并发路径释放。

具体场景：`_resolve_request` 返回 `wire_mv`（指向 SHM 的 memoryview）并将释放信息注册到 `_deferred_frees[str_rid]`。如果 `destroy()` 被调用（停服），`conn.cleanup()` 执行 `buddy_pool.destroy()` 解除 SHM 映射，但已发出的 `wire_mv` 引用仍然存在。此时 ctypes `from_address()` 构造的 buffer 并不真正持有内存所有权——底层地址已失效，访问将 segfault。

**缓解措施已有**：`_handle_client` 的 `finally` 块在调用 `conn.cleanup()` 前会清理 deferred frees。但 `destroy()` 路径缺少对 `_deferred_frees` 的清理。

**建议**: 在 `destroy()` 中添加 `_deferred_frees` 的排空逻辑。

### 并发性回归：**deferred frees 在 `destroy()` 中泄漏** [CRITICAL]

**问题 OPT-S2**: `destroy()` 清空 `_pending` 并清理连接，但从未排空 `_deferred_frees`。如果服务器在请求 in-flight 期间被销毁（`_resolve_request` 已注册 deferred free，但 `reply()` 未消费），这些 buddy block 被遗漏。虽然 buddy pool 随后被销毁（block 随 pool 消失），但簿记不一致可能掩盖 bug。

### 资源管理：**decode() 异常导致 deferred free 孤立** [SUGGESTION]

如果 `_resolve_request` 注册了 deferred free，但随后的 `decode(wire_bytes)` 抛异常，deferred free 条目在连接关闭时才被清理（`finally` 块扫描）。连接不会立即关闭（异常被捕获），但当前实现中异常确实导致连接关闭。

**建议**: 在 `decode()` 失败时主动释放 deferred free 并 `continue` 循环，而非杀死整个连接。

---

## 二、实验 5：跳过 bytes 类型的 pickle 序列化

### 作弊性评估：**技术上合法，但实际受益范围极窄** [IMPORTANT]

逐一检查代码库中所有 `@cc.icrm` 接口定义后发现：

| 类别 | 匹配情况 | 代表接口 |
|------|---------|---------|
| **benchmark echo** (`bytes -> bytes`) | 10+ 个接口 | `IEcho.echo(data: bytes) -> bytes` |
| **真实应用接口** | 0 个 | `IHello.greeting(name: str) -> str`, `IGrid.get_schema() -> GridSchema`, etc. |
| **部分匹配** (仅输出为 bytes) | 3 个 benchmark 接口 | `IGenerator.generate(size: int) -> bytes` |

**结论**: 代码库中**零个真实应用 CRM 方法**使用 `(bytes) -> bytes` 签名。所有触发输入+输出双重快速路径的方法均为 benchmark echo 方法。

这不是传统意义上的"作弊"——优化逻辑是正确的，任何用户定义的 `bytes -> bytes` 接口都能受益。但 ru-op2.md 的 90.3% 标题数字几乎完全来自此优化路径，对结构化类型（`str`, `int`, `list`, 自定义 dataclass）的 CRM 方法无任何帮助。

### 正确性问题：**空 bytes `b""` 被静默转为 None** [CRITICAL]

**问题 OPT-T1**: 输入反序列化器中：

```python
def deserialize(data: memoryview | bytes):
    if not data:      # <-- b"" 的 truthiness 为 False
        return None   # <-- 空 bytes 被错误地返回 None
    return data
```

Python 中 `not b""` 为 `True`，因此传入空 bytes 参数会被反序列化为 `None`。CRM 方法将收到 `None` 而非 `b""`，导致 `TypeError` 或静默行为错误。

**修复**: 将 `if not data:` 改为 `if data is None:`。

### 正确性问题：**输入序列化有 pickle fallback 但反序列化无对应处理** [IMPORTANT]

**问题 OPT-T2**: 输入序列化器在参数非 bytes/memoryview 时 fall back 到 `pickle.dumps(data)`。但反序列化器**始终**原样返回数据。这意味着如果有人传入非 `bytes` 值（类型标注为 `bytes` 但运行时传入 `str`），序列化端 pickle 了该值，反序列化端却原样返回 pickle bytes——CRM 方法收到的是 pickle 编码的垃圾数据。

**建议**: serialize 中移除 pickle fallback，改为 `raise TypeError`：
```python
def serialize(data) -> bytes:
    if isinstance(data, (bytes, memoryview)):
        return data
    raise TypeError(f"Expected bytes, got {type(data).__name__}")
```

### 正确性问题：**输出反序列化与输入反序列化策略不一致**

输出路径的 deserializer 有 pickle 回退：
```python
lambda data: data if isinstance(data, (bytes, memoryview)) else pickle.loads(data) if data else None
```

但输入路径的 deserializer 完全没有 pickle 回退。如果协议两端版本不一致（一端有 fast path，另一端没有），输入路径会静默传递 pickle bytes 而非反序列化对象。

### 文档漂移

ru-op2.md §4.2 的伪代码展示 `all(p == bytes for p in param_types)` 和 `*args lambda`，但实际实现使用 `_is_single_bytes_param` 和单参数函数。实际代码比文档描述更保守（更安全），但文档具有误导性。

---

## 三、实验 6：Server 零拷贝响应路径

### 作弊性评估：**非作弊——但收益有条件**

跳过 `materialize_response_views` 对任何返回 memoryview 的 CRM 方法都有效（例如返回 mmap 文件的切片）。但对于构造新 `bytes` 对象的 CRM 方法（这是常见情况），不存在 memoryview 需要物化，优化为 no-op。

### 正确性问题：**与实验 7 存在隐式依赖** [IMPORTANT]

**问题 OPT-S4**: 跳过物化是实验 7（reuse 检测）的**前提条件**。如果 `materialize_response_views=True`，memoryview 会被拷贝为 bytes，此时 `isinstance(result_bytes, memoryview)` 为 False，reuse 路径永远不会触发。这两个优化之间的依赖关系未在代码中文档化。

**建议**: 在 `materialize_response_views=False` 的设置处添加注释，说明这是 reuse 优化的前提。

---

## 四、实验 7：零拷贝 Echo 响应（原地 Header 覆写）

### 作弊性评估：**几乎纯粹的 benchmark 优化** [CRITICAL 概念性问题]

**问题 OPT-S5**: `result_bytes.obj is seg_obj` 身份检查仅在 CRM 方法**返回其输入的原始 memoryview**（即 echo/passthrough）时通过。对于任何以下情况，检查都会失败并退回普通路径：

- CRM 方法反序列化输入并序列化不同的响应 → 返回新 `bytes`
- CRM 方法对数据做任何变换 → 返回新 `bytes`
- CRM 方法返回来自不同来源的 memoryview（如缓存） → `.obj` 身份不匹配

**这是文档中最大的"作弊"点。** 优化本身不有害（检查失败时正确降级），但它为 echo benchmark 添加了 30+ 行热路径代码和一个新的协议扩展（`BUDDY_REUSE` 帧），而这些代码在真实 CRM 工作负载中从不触发。

ru-op2.md 的标题声明 "90.3% 降低" 在很大程度上依赖此优化（从 19.54ms 降至 13.89ms，贡献了 28.9% 的总改善）。

**结论**: 不是"不诚实的作弊"——优化在前置条件满足时确实生效。但将其作为通用 IPC 优化来呈现是有误导性的。真实 CRM 方法不会触发 reuse 路径。

### 安全性回归：**原地 SHM 写入有损坏风险** [IMPORTANT]

**问题 OPT-S6**: `reply()` 中 lines 263-264 的覆写：

```python
seg_mv[reply_start] = MsgType.CRM_REPLY
struct.pack_into('<I', seg_mv, reply_start + 1, 0)
```

在无锁状态下写入 SHM。`seg_mv` 在 `_conn_lock` 下快照（line 243），但实际写入发生在锁外。如果连接被并发销毁（另一线程调用 `conn.cleanup()`），`seg_mv` 可能指向已 unmap 的内存——**写入已 unmap 内存会 segfault**。

**问题 OPT-S7**: 覆写修改了客户端的请求块。BUDDY_REUSE 帧通知客户端从请求块读取响应。但在服务端写入和客户端读取之间，如果客户端已超时并决定释放该块，服务端的写入将损坏已释放的内存。

### 并发性回归：**TOCTOU 竞争** [IMPORTANT]

**问题 OPT-S9**: Reuse 路径中 `_deferred_frees` 被分两次获锁读取和弹出：

1. Line ~253: 在锁下读取 `_deferred_frees[rid_key]`
2. Line ~271: 在另一次锁获取中弹出 `_deferred_frees.pop(rid_key, None)`

在这两次锁获取之间，另一线程可能通过 `_free_deferred`（超时/取消路径）弹出同一条目。此时：
- `pop()` 返回 None，不会 double-free
- 但 BUDDY_REUSE 帧已构造完毕并将被发送——客户端收到的 BUDDY_REUSE 帧引用了一个已被释放的 block

**修复**: 合并读取和弹出为单次锁获取：
```python
with self._deferred_frees_lock:
    info = self._deferred_frees.pop(rid_key, None)
if info is not None and len(info) >= 8:
    # reuse 逻辑...
```

### 正确性问题：**reuse 检测的假阳性风险** [SUGGESTION]

`result_bytes.obj is seg_obj` 身份检查假设"相同的底层对象 + 相同长度 = 相同的数据区域"。如果 CRM 方法返回了同一 segment 的**不同切片**（碰巧长度相同），身份检查通过但 offset 不匹配——服务端会向客户端发送错误的 offset。

实际风险极低（CRM 方法无法直接访问 `seg_mv` 对象），但逻辑上不够严谨。

---

## 五、实验 8：Client 零拷贝读取

### 作弊性评估：**非作弊——对所有 CRM 方法有效**

`_recv_response()` 返回 SHM memoryview 而非 `bytes`，减少一次 memcpy。这对所有通过 buddy 路径传输的响应都有效，不限于 echo。

### 正确性：**已验证**

内联 CRM_REPLY 解析正确处理了：
- 最小大小检查：`total_size < 5`
- 类型检查：`mv[0] != _CRM_REPLY_TYPE`
- 错误路径：`err_len > 0` 读取错误 bytes，释放 block，反序列化错误
- 空 payload：`data_size == 0` 返回 `b''`

### 正确性边界问题：**err_len 未做上限验证** [SUGGESTION]

**问题 OPT-C-S1**: 如果 SHM 被损坏，`err_len = _U32_LE.unpack_from(mv, 1)[0]` 可能超过 `total_size - 5`，导致 `mv[5:err_end]` 读取超出响应数据范围的 SHM 内存（其他 buddy block 的数据）。这不会 segfault（整个 segment 都已映射），但会读取垃圾数据。

**建议**: 添加 `if err_end > total_size:` 验证。

---

## 六、实验 9：内联 CRM_REPLY 解析

### 作弊性评估：**非作弊——对所有 buddy 路径响应有效**

消除 `Envelope` 对象创建和 `MsgType` 枚举查找。绝对值小（~2-5μs），但在 10MB P50 ≈ 1ms 级别有可测量的占比。对所有通过 buddy 路径的响应都有效。

### 安全性/正确性：**无问题**

预编译 `struct.Struct('<I')` 和预缓存 `MsgType.CRM_REPLY`。逻辑等价于原来的 `decode()` 路径，只是跳过了对象包装层。

---

## 七、实验 16：暖缓冲区 + memoryview 直通

### 作弊性评估：**实验本身合法，但其代码现在是死代码** [IMPORTANT]

**问题 OPT-C-I1**: 实验 17（零拷贝 SHM 响应）完全取代了实验 16 的暖缓冲区路径。在当前代码中：

```python
if data_size >= _WARM_BUF_THRESHOLD:  # >= 1MB
    # 实验 17 直接返回 SHM memoryview
    self._deferred_response_free = free_info
    return mv[5:]   # <-- 在实验 16 的路径之前返回
```

因此以下基础设施全部成为死代码：
- `_response_buf`, `_response_buf_addr`, `_ensure_response_buf()`
- `_memmove`, `_memset`, libc 加载（`ctypes.CDLL(ctypes.util.find_library('c'))`）

**影响**:
1. 模块级 `ctypes.CDLL(ctypes.util.find_library('c'))` 在 import 时执行，增加启动延迟
2. 多余的实例属性增加每个客户端对象的内存占用
3. 增加代码复杂度和维护负担

**建议**: 移除死代码，或将其保留为 fallback 但用标志控制。

### memoryview 直通（`transferable.py`）：**合法且有效**

输出反序列化器中 `data if isinstance(data, (bytes, memoryview)) else pickle.loads(data)` 允许 memoryview 不被转为 bytes 就通过。这对实验 17 也是必需的——如果反序列化器将 memoryview 转为 bytes，零拷贝收益就会被抵消（如实验 15 的失败所证明）。

---

## 八、实验 17：零拷贝 SHM 响应（延迟释放）——最关键的审计点

### 作弊性评估：**优化真实有效，但对一般使用场景不安全** [CRITICAL]

实验 17 是整组优化中贡献最大的单点改善（从 6.92ms → 3.58ms，-48.3%）。其核心是：`call()` 对 >= 1MB 的响应直接返回指向 SHM 的 memoryview，buddy block 延迟到下一次 `call()` 时才释放。

**90.3% 的标题数字在 echo benchmark 中是真实的**，因为 echo 的使用模式恰好满足安全前提：

```python
for _ in range(rounds):
    result = crm.echo(payload)  # 旧 result 的引用在此被替换
    # CPython 引用计数立即释放旧 memoryview
```

**但这个前提在一般使用中不成立：**

#### 场景 1：累积结果

```python
results = []
for chunk_id in range(100):
    results.append(crm.get_chunk(chunk_id))  # 第2次 call 释放第1次的 SHM block
    # results[0] 现在指向已释放的 SHM 内存！
```

#### 场景 2：跨线程传递

```python
result = client.call('process', big_data)
executor.submit(consume_result, result)  # worker 线程持有 memoryview
client.call('next_step', small_data)     # 释放 SHM block → worker 线程 use-after-free
```

#### 场景 3：回调/异步

```python
async def process():
    result = await asyncio.to_thread(client.call, 'method', data)
    await asyncio.sleep(1)  # 其他代码可能调用 client.call
    process(result)  # result 可能已失效
```

### 安全性回归：**use-after-free 导致 segfault 或数据损坏** [CRITICAL]

**问题 OPT-C1 (最严重)**:

返回的 `memoryview` 指向共享内存。`_deferred_response_free` 在下一次 `call()` 开始时执行 `_flush_deferred_response_free()`，调用 `buddy_pool.free_at()` 释放 buddy block。一旦释放：

1. **该 block 可被重新分配**: 下一次 `alloc()` 可能返回同一 block，新请求的数据会覆写旧结果
2. **如果 segment 被回收**: 整个 SHM 映射可能被 unmap，此时访问 memoryview 导致 **segfault**
3. **跨进程污染**: 如果 buddy block 被服务端分配给另一个连接的响应，客户端读到的是其他连接的数据

**ru-op2.md 的安全性论证有三个假设，逐一审查**：

| 假设 | 评估 |
|------|------|
| "Client 的 `call()` 在 `_conn_lock` 下串行执行" | ✅ 正确，`_conn_lock` 保证串行 |
| "调用者在下一次 `call()` 之前有完整的时间窗口读取数据" | ❌ **假设过强**——调用者可能存储引用、传给其他线程、或在回调中延迟使用 |
| "CPython 引用计数保证旧 memoryview 在赋新值时引用归零" | ❌ **仅在单一循环变量模式下成立**。列表、字典、闭包、线程变量都会延长引用生命周期 |

**结论**: 此优化本质上是一个**借用语义 (borrow)** 而非**所有权语义 (ownership)**，但 Python 的 API 合同（返回值归调用者所有）不支持借用语义。调用者无法从类型签名或文档中得知返回的 memoryview 有有限生命周期。

### 并发性评估：**在 `_conn_lock` 保护下是安全的**

`_deferred_response_free` 的读写都在 `_conn_lock` 内（`call()` line 236, `relay()` line 329, `terminate()` line 376）。两个线程不可能同时操作此字段。

但 `_conn_lock` 在整个 `call()` 期间持有（包括网络 I/O），意味着 `terminate()` 必须等待当前 `call()` 完成（最多 10 秒超时）。在此等待期间，上一次响应的 SHM block 保持分配状态，可能造成短暂的内存浪费。

### 建议修复方案

**方案 A（推荐）——opt-in 零拷贝**：

```python
def call(self, method, *args, zero_copy=False):
    ...
    if zero_copy and data_size >= _WARM_BUF_THRESHOLD:
        self._deferred_response_free = free_info
        return mv[5:]  # 调用者明确选择借用语义
    else:
        payload = bytes(mv[5:])  # 默认安全路径
        self._free_buddy_response(free_info)
        return payload
```

**方案 B——弱引用回调**：

使用 `weakref.ref` 监控返回的 memoryview wrapper 对象，当其被 GC 时自动释放 buddy block。但 memoryview 本身不支持 weakref，需要额外包装。

**方案 C——文档警告 + 断言**：

至少在返回值的 docstring 中大字标明 "WARNING: 返回的 memoryview 仅在下一次 call() 之前有效"。在 `_flush_deferred_response_free` 中添加断言检查旧 memoryview 的引用计数 ≤ 1。

---

## 九、ru-op1 修复状态审查

在分析 op2 优化之前，确认 ru-op1 中指出的关键问题是否在 `fix/ipc-v3-remediation` 分支中被修复。

| ru-op1 问题 | 状态 | 说明 |
|-------------|------|------|
| **CL-C1** (call() 缩进 bug 双重发送) | ✅ 已修复 | dedicated 路径正确发送 inline frame 后 return |
| **CL-C2** (无 seg_idx 边界检查) | ✅ 已修复 | `_recv_response` 验证 `seg_idx >= len(self._seg_views)` 和 `offset+size` |
| **CL-C3** (buddy block 异常泄漏) | ✅ 已修复 | write+send 包在 try/except 中，except 中 free_at |
| **CL-I1** (单 segment 但配置允许多个) | ⚠️ 缓解 | `max_segments=1` 防止 pool 增长，但协议级 announce 未实现 |
| **S-C1** (`_resolve_request` 无锁访问) | ✅ 已修复 | 现在在 `_conn_lock` 下访问 `conn.seg_views` |
| **S-C2** (wire seg_idx 无验证) | ✅ 已修复 | 添加边界检查 |
| **S-C3** (offset+data_size 无验证) | ✅ 已修复 | 添加边界检查 |
| **S-C4** (handshake seg_count DoS) | ✅ 已修复 | 添加 `max_pool_segments` 限制 |
| **S-I5** (segment name 无验证) | ✅ 已修复 | regex 验证 segment name |
| **R-C1** (spinlock panic-safety) | 未确认 | 需独立检查 Rust 代码变更 |

**新增问题**: `_seg_base_addrs` 在 `_close_connection()` 中未清除（`_seg_views` 已清除），留下指向已 unmap SHM 的悬空指针。当前因死代码路径（暖缓冲区）不被执行而无害，但若代码被激活则危险。

---

## 十、作弊性综合评估

### 优化分层分析

将 8 个保留实验按 **"对真实工作负载的适用性"** 分为三层：

| 层级 | 实验 | echo 适用 | 非 echo 适用 | 评估 |
|------|------|-----------|-------------|------|
| **通用优化** (传输层) | #1 (server 零拷贝读) | ✅ | ✅ | 合法 |
| | #8 (client 零拷贝读) | ✅ | ✅ | 合法 |
| | #9 (内联 CRM_REPLY) | ✅ | ✅ | 合法 |
| **条件优化** (需匹配前提) | #6 (跳过物化) | ✅ | 仅返回 mv 的方法 | 合法但收益有条件 |
| | #5 (bytes pickle 跳过) | ✅ | 仅 `bytes` 签名方法 | **实质仅 benchmark 受益** |
| | #16 (暖缓冲区) | — | — | **死代码** |
| **Echo 专属优化** | #7 (reuse 原地覆写) | ✅ | ❌ (极罕见匹配) | **几乎纯 benchmark 优化** |
| | #17 (零拷贝 SHM 返回) | ✅ | ⚠️ (不安全) | **有安全隐患的 benchmark 优化** |

### 数字拆解

ru-op2.md 声称 **90.3% 总改善**（36.9ms → 3.58ms）。按各实验贡献拆解：

| 实验 | 改善量 (ms) | 占总改善比 | 通用性 |
|------|------------|-----------|--------|
| #1 server 零拷贝读 | 6.80 | 20.4% | **通用** |
| #5 bytes pickle 跳过 | 6.16 | 18.5% | ⚠️ 仅 bytes 签名 |
| #6 server 零拷贝响应 | 4.41 | 13.2% | 条件性 |
| #7 reuse 原地覆写 | 5.65 | 17.0% | ❌ echo 专属 |
| #8 client 零拷贝读 | 4.39 | 13.2% | **通用** |
| #9 内联解析 | 1.21 | 3.6% | **通用** |
| #16 暖缓冲区 | 1.37 | 4.1% | ❌ 死代码 |
| #17 零拷贝 SHM 返回 | 3.34 | 10.0% | ❌ 不安全 |

**通用优化总贡献**: 12.40ms / 33.32ms = **37.2%** 的总改善
**Echo 专属/受限优化**: 20.93ms / 33.32ms = **62.8%** 的总改善

### 核心结论

**如果剥离 echo 专属优化（#5 bytes 快速路径、#7 reuse、#17 零拷贝 SHM 返回），对一般 CRM 工作负载的实际改善约为 37%（从 36.9ms 降至约 23ms），而非声称的 90.3%。**

这不意味着 echo 优化"无价值"——它们在其适用场景下确实有效。但报告标题的 90.3% 数字对不了解上下文的读者具有误导性。

---

## 十一、问题汇总

### CRITICAL

| ID | 问题 | 实验 | 影响 |
|----|------|------|------|
| OPT-C1 | 实验 17 返回的 SHM memoryview 在下次 `call()` 后失效，但 API 合同未告知调用者。跨线程使用或存储引用导致 use-after-free (segfault/数据损坏) | #17 | 生产环境崩溃 |
| OPT-T1 | 空 bytes `b""` 被 `if not data:` 静默转为 `None`，CRM 方法收到错误参数 | #5 | 静默数据错误 |
| OPT-S2 | `destroy()` 不排空 `_deferred_frees`，请求 in-flight 时的 buddy block 被遗漏 | #1 | 资源泄漏 |

### IMPORTANT

| ID | 问题 | 实验 | 影响 |
|----|------|------|------|
| OPT-S1 | SHM memoryview 可在 `destroy()` 后被访问（ctypes `from_address` 不持有内存所有权） | #1 | 并发停服时 segfault |
| OPT-S4 | 实验 6 跳过物化是实验 7 reuse 检测的隐式前提，未文档化 | #6→#7 | 维护风险 |
| OPT-S5 | 实验 7 reuse 检测几乎只在 echo 场景触发——30+ 行代码 + 新协议帧仅服务于 benchmark | #7 | 代码膨胀 |
| OPT-S6 | SHM 原地覆写在无锁状态下执行，并发 `destroy()` 可导致写入 unmap 内存 | #7 | 并发 segfault |
| OPT-S9 | TOCTOU 竞争：`_deferred_frees` 读和弹出分两次获锁，已释放的 block 可能被 BUDDY_REUSE 帧引用 | #7 | 客户端读已释放内存 |
| OPT-T2 | 输入 serialize 有 pickle fallback 但 deserialize 无对应处理，类型不匹配时静默传递 pickle garbage | #5 | 静默数据错误 |
| OPT-C-I1 | 实验 16 暖缓冲区基础设施全部成为死代码（被实验 17 取代），但 import 时仍加载 libc | #16 | 多余依赖/代码 |
| OPT-C-I2 | `_seg_base_addrs` 在 `_close_connection()` 中未清除，留下悬空指针 | #16 | 潜在悬空指针 |

### SUGGESTION

| ID | 问题 | 实验 |
|----|------|------|
| OPT-S3 | `decode()` 异常后杀死整个连接而非跳过单条消息 | #1 |
| OPT-C-S1 | `err_len` 未做上限验证（SHM 损坏时读取垃圾数据） | #8 |
| OPT-S8 | reuse 检测的 `obj is seg_obj` 身份检查理论上可能假阳性 | #7 |
| — | ru-op2.md §4.2 伪代码与实际实现不一致（实际更保守/安全） | #5 |

---

## 十二、建议优先行动

### 必须在合并前修复

1. **OPT-C1 (实验 17 use-after-free)**: 改为 opt-in 零拷贝（方案 A），或默认走安全的 `bytes()` 拷贝路径
2. **OPT-T1 (空 bytes → None)**: `if not data:` 改为 `if data is None:`
3. **OPT-S2 (destroy 不排空 deferred frees)**: 在 `destroy()` 中添加排空逻辑
4. **OPT-S9 (TOCTOU 竞争)**: 合并 deferred frees 的读取和弹出为单次锁获取

### 建议修复

5. **OPT-C-I1 (死代码)**: 移除暖缓冲区基础设施
6. **OPT-T2 (serialize/deserialize 不对称)**: serialize 中移除 pickle fallback，改为 `raise TypeError`
7. **OPT-S4 (隐式依赖)**: 在 `materialize_response_views=False` 处添加注释

### 文档建议

8. 报告中明确区分"通用优化"和"echo 专属优化"，给出两组数字
9. 实验 7 的 reuse 路径应标注为 "benchmark/passthrough fast path"
10. 实验 17 如果保留 opt-in 模式，需在 API 文档中明确借用语义约束

