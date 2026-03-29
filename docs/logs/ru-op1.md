# IPC v3 Code Review Report

**审查日期**: 2026-03-25
**审查分支**: `rust-buddy`
**审查范围**: IPC v3 协议实现、Rust buddy allocator、Python 客户端/服务端、SHM pool、wire format
**审查重点**: 代码模块合理性、性能短板、优化合理性、安全漏洞

---

## 一、Rust Buddy Allocator (`rust/c2_buddy/`)

### CRITICAL

#### R-C1. Spinlock `with_lock` 不是 panic-safe 的 —— 闭包 panic 将永久死锁所有进程

**文件**: `rust/c2_buddy/src/spinlock.rs:82-89`

```rust
pub fn with_lock<F, R>(&self, f: F) -> R
where F: FnOnce() -> R,
{
    self.lock();
    let result = f();
    self.unlock();
    result
}
```

如果 `f()` panic（例如 bitmap 操作中的 debug_assert 触发），`unlock()` 永远不会被调用。在跨进程 SHM spinlock 场景下，这意味着**所有共享该 SHM 的进程将永久死锁**——它们会无限自旋（或命中 10M 次自旋后 panic）。这是代码库中最危险的 bug。

**修复**: 使用 RAII guard 模式：

```rust
pub fn with_lock<F, R>(&self, f: F) -> R
where F: FnOnce() -> R,
{
    self.lock();
    struct Guard<'a>(&'a ShmSpinlock);
    impl<'a> Drop for Guard<'a> {
        fn drop(&mut self) { self.0.unlock(); }
    }
    let _guard = Guard(self);
    f()
}
```

#### R-C2. Bitmap CAS 与 Spinlock 的不一致——未来维护陷阱

**文件**: `rust/c2_buddy/src/bitmap.rs:61-84`, `allocator.rs:190-191`

`bitmap::alloc_one()` 使用 `compare_exchange_weak` CAS 循环，但它始终在 spinlock 临界区内被调用。CAS 只可能因 spurious failure 而失败，不会因实际竞争而失败。然而 `free_one()` 和 `mark_used()` 使用无条件 `fetch_or`/`fetch_and`——同样在 spinlock 保护下是正确的，但不一致性令人困惑。

如果未来有人在 spinlock 之外调用 bitmap 方法，`alloc_one` 的 CAS 会给出线程安全的错觉，而 `free_one`/`mark_used` 则完全不同步。

**建议**: 要么 (a) 明确文档说明所有 bitmap 操作必须在 spinlock 下调用，要么 (b) 使所有 bitmap 方法一致地使用 CAS。

#### R-C3. `SegmentHeader` 对齐未对测试代码保证

**文件**: `rust/c2_buddy/src/allocator.rs:37-52, 104`

```rust
let header = unsafe { &mut *(base as *mut SegmentHeader) };
```

`SegmentHeader` 包含 `AtomicU64`（需 8 字节对齐）。SHM 中 `mmap()` 返回页对齐地址（安全），但测试中 `Vec<u8>::as_mut_ptr()` 不保证 8 字节对齐。

**建议**: 在 `init()` 中添加对齐断言：
```rust
assert!(base as usize % std::mem::align_of::<SegmentHeader>() == 0);
```

#### R-C4. `AtomicU64` 在某些目标平台上可能不是 lock-free 的

**文件**: `rust/c2_buddy/src/allocator.rs:48-50`

在 32 位平台上，`AtomicU64` 可能内部使用 mutex，这在跨进程 SHM 场景下不可用。

**建议**: 在初始化时添加：
```rust
assert!(AtomicU64::is_lock_free(), "AtomicU64 must be lock-free for cross-process SHM");
assert!(AtomicU32::is_lock_free(), "AtomicU32 must be lock-free for cross-process SHM");
```

#### R-C5. `update_stats_free` 在 double-free 时 `alloc_count` 会下溢

**文件**: `rust/c2_buddy/src/allocator.rs:306-310`

`free()` 没有验证被释放的 block 是否实际上已分配。Double-free 会导致 `alloc_count` u32 下溢、`free_bytes` 超过 `data_size`，静默腐蚀 pool 统计信息。

**建议**: 在释放前检查 bitmap 中该 block 是否标记为已使用。至少应 debug_assert `alloc_count > 0`。

### IMPORTANT

#### R-I1. `BuddyPool` 整体被 `Mutex` 包裹——读操作也需要独占锁

**文件**: `rust/c2_buddy/src/pool.rs`, `ffi.rs`

FFI 层用 `Mutex<BuddyPool>` 包裹整个 pool。这意味着只读操作（`data_ptr`、`read_at`、`stats`）也会阻塞分配操作。各 buddy segment 已有自己的 SHM spinlock 保护 alloc/free 临界区，外层 Mutex 主要是为了保护 `create_segment`（修改 `self.segments`）。

**建议**: 考虑使用 `RwLock<BuddyPool>` 或重构使只读操作取 `&self`。

#### R-I2. `DedicatedSegment` 大小截断为 `u32` —— 超 4GB 分配静默错误

**文件**: `rust/c2_buddy/src/pool.rs:392`

```rust
let alloc_size = seg.size() as u32;
```

`size()` 返回 `usize`（64 位平台可超 4GB），强转 `u32` 静默截断。设计文档提到 fastdb TileBoxTake 可达 500MB，未来可能更大。

**建议**: 将 `actual_size` 改为 `u64`，或添加 > 4GB 的显式检查。

#### R-I3. `Allocation::offset` 是 `u32`——限制数据区域最大 4GB

**文件**: `rust/c2_buddy/src/allocator.rs:58`

`block_to_offset` 中 `(block_idx * block_size) as u32` 无溢出检查。如果 segment_size 配置超过 4GB，offset 会溢出。

**建议**: 在 `BuddyAllocator::init` 中添加 `data_size <= u32::MAX as usize` 检查。

#### R-I4. `free()` 不验证输入——offset 和 level 被完全信任

**文件**: `rust/c2_buddy/src/allocator.rs:194-196, 270-298`

没有验证 `offset` 是否对齐到给定 `level` 的 block 大小，`level` 是否在范围内，或 block 是否实际上已分配。通过 FFI 传入错误值可导致 bitmap 腐蚀和静默内存损坏。

**建议**: 添加边界检查：`level < self.levels`、`offset % level_block_size == 0`、`block_idx < blocks_at_level`。

#### R-I5. `shm_unlink` 后 `shm_open(O_CREAT|O_EXCL)` 存在 TOCTOU 竞争

**文件**: `rust/c2_buddy/src/segment.rs:37-43`

在 `shm_unlink` 和 `shm_open` 之间，另一个进程可能创建同名 segment。无条件 `shm_unlink` 还会破坏不属于本进程的合法 segment。

**建议**: 使用足够唯一的命名（当前 PID-based 部分缓解），或在 unlink 前检查 header magic + PID。

#### R-I6. `ShmSegment::open` 映射 `max(actual_size, expected_size)` —— 可能 SIGBUS

**文件**: `rust/c2_buddy/src/segment.rs:111`

如果 `expected_size > actual_size`（实际文件小于预期），`mmap` 成功但访问超出实际文件大小的内存会触发 `SIGBUS`。

**建议**: 使用 `actual_size` 作为映射大小，或至少 assert `actual_size >= expected_size`。

#### R-I7. `next_dedicated_idx` 是 `u16`，65280 次分配后溢出回绕

**文件**: `rust/c2_buddy/src/pool.rs:86, 105, 248, 389`

dedicated segment index 从 256 开始递增，是 `u16`。在长时间运行的服务器中，经过 65280 次 dedicated 分配后会回绕到 0，与 buddy segment 索引冲突。

---

## 二、Python IPC v3 Server (`ipc_v3_server.py`, `ipc_v3_protocol.py`)

### CRITICAL

#### S-C1. `_resolve_request` 访问 `conn.seg_views` 无锁保护——与 `destroy()` 竞争

**文件**: `ipc_v3_server.py:394-415`

`_resolve_request` 无锁访问 `conn.seg_views[seg_idx]` 和 `conn.buddy_pool`。而 `reply()` 在 `_conn_lock` 下读取同一字段。如果 `destroy()` 在另一线程调用 `cleanup()`（设置 `seg_views = []` 并销毁 buddy_pool），`_resolve_request` 可能索引空列表（`IndexError`），或更糟——读取已 unmap 的 memoryview 导致 **segfault**。

**建议**: 要么 (a) 在 `_resolve_request` 中获取 `_conn_lock`，要么 (b) 确保 `destroy()` 先取消所有 client tasks 并 await 完成后再清理连接。

#### S-C2. 未验证 wire 数据中的 `seg_idx`——恶意客户端可触发 IndexError 崩溃

**文件**: `ipc_v3_server.py:407, 222`

`_resolve_request` 和 `reply` 使用 `decode_buddy_payload` 解码的 `seg_idx` 直接索引 `conn.seg_views`。恶意客户端可发送超出范围的 `seg_idx`。

**建议**: 在索引前添加边界检查：
```python
if seg_idx >= len(conn.seg_views):
    logger.warning('Conn %d: invalid seg_idx %d', conn.conn_id, seg_idx)
    return None
```

#### S-C3. `offset + data_size` 未验证——可能越界读取 SHM 导致 segfault

**文件**: `ipc_v3_server.py:408`

```python
wire_bytes = bytes(seg_mv[offset : offset + data_size])
```

`offset` 和 `data_size` 来自不可信的 wire payload。如果 memoryview 是通过 `ctypes.c_char.from_address` 构造的，越界读取将访问未映射内存，导致进程崩溃。

**建议**: 读取前验证：
```python
if offset + data_size > len(seg_mv):
    logger.warning('Conn %d: buddy read out of bounds', conn.conn_id)
    conn.buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
    return None
```

#### S-C4. Handshake `seg_count` 无上限——DoS 资源耗尽

**文件**: `ipc_v3_protocol.py:140`, `ipc_v3_server.py:442-443`

`decode_buddy_handshake` 读取 `seg_count` 为 `u16`（最大 65535），无上限检查。服务端循环调用 `open_segment` 打开所有声称的 segment。恶意客户端可发送 65535 个 segment 的 handshake，耗尽文件描述符和内存。

**建议**: 在 `_handle_buddy_handshake` 中验证 segment 数量不超过 `max_pool_segments`。

### IMPORTANT

#### S-I1. 单连接串行 `await fut` 阻塞全双工——与 "Full-duplex" 文档声明不符

**文件**: `ipc_v3_server.py:361`

```python
response_frame = await fut
```

每个连接的 handler 读取一帧、dispatch、然后 `await` 响应 future，直到响应完成才读下一帧。这意味着单个客户端连接同一时间只能有一个 in-flight 请求——与模块文档声称的 "Full-duplex" 矛盾。

**建议**: 如果 full-duplex 是设计目标，应将 await+write 部分用 `asyncio.create_task` 分离，读循环继续立即读取下一帧。

#### S-I2. 无 `max_pending_requests` 强制执行

**文件**: `ipc_v3_server.py:354-356`

配置有 `max_pending_requests = 1024`，但服务端从未检查 `_pending` 大小。慢后端 + 快客户端可导致 pending dict 无限增长。

#### S-I3. `destroy()` 未取消 pending futures 就清除

**文件**: `ipc_v3_server.py:138-139`

`_pending.clear()` 直接丢弃 futures 而不 cancel。任何正在 await 这些 futures 的协程将永久挂起。`cancel_all_calls()` 能正确 cancel，但 `destroy()` 未调用它。

**建议**: 在 `destroy()` 开头调用 `self.cancel_all_calls()`。

#### S-I4. Handshake 失败留下不一致状态——部分打开的 segment 未清理

**文件**: `ipc_v3_server.py:457-459`

如果 `open_segment` 对某些 segment 成功但后续 segment 失败，连接的 `buddy_pool` 有部分打开的 segment，但 `handshake_done` 仍为 `False`。部分打开的资源不会被清理。

**建议**: 在异常处理中调用 `conn.cleanup()` 释放已部分打开的资源。

#### S-I5. Handshake segment `name` 未做路径验证

**文件**: `ipc_v3_server.py:443`, `ipc_v3_protocol.py:150`

SHM segment `name` 从 UTF-8 wire 数据解码后直接传给 `buddy_pool.open_segment(name, size)`。含路径遍历字符（`../`）、null 字节或过长字符串的 name 可能导致问题。POSIX `shm_open` 要求 name 以 `/` 开头且不含其他 `/`。

**建议**: 传给 Rust 层前验证 segment name 格式。

---

## 三、Python IPC v3 Client (`ipc_v3_client.py`, `shm_pool.py`)

### CRITICAL

#### CL-C1. `call()` 中缩进错误导致 dedicated 路径双重发送帧

**文件**: `ipc_v3_client.py:207-211`

```python
                    else:
                        seg_mv = self._seg_views[alloc.seg_idx]
                        shm_buf = seg_mv[alloc.offset : alloc.offset + wire_size]
                        write_call_into(shm_buf, 0, method_name, args)

                        frame = encode_buddy_call_frame(
                        request_id, alloc.seg_idx, alloc.offset,
                        wire_size, alloc.is_dedicated,
                    )
                    sock.sendall(frame)  # <-- 这行在 if/else 同级，两个分支都会执行
```

`encode_buddy_call_frame` 调用的缩进不一致——参数在 `else` 块更深层，但关闭 `)` 与 `if/else` 同级。`sock.sendall(frame)` 在 `if alloc.is_dedicated` / `else` 同级，意味着：

- **`is_dedicated=True` 路径**：先在 line 201 发送 inline frame，然后 line 211 再发送一次——**双重发送，破坏协议流**。
- **`is_dedicated=False` 路径**：碰巧正确（frame 在更深作用域中赋值，Python 的作用域规则使其在外层可见）。

对比 `relay()` (lines 258-262)，其缩进是正确的。

**修复**: 将 lines 207-211 的 `encode_buddy_call_frame` + `sock.sendall` 正确缩进到 `else` 块内部。

#### CL-C2. 服务端返回的 `seg_idx` 和 `offset` 未做边界检查

**文件**: `ipc_v3_client.py:298-299`

```python
seg_mv = self._seg_views[seg_idx]
data = bytes(seg_mv[offset : offset + data_size])
```

`seg_idx` 和 `offset` 来自服务端的 wire 数据（`decode_buddy_payload`），无任何验证：
- `seg_idx >= len(self._seg_views)` → `IndexError` 崩溃
- `offset + data_size > len(seg_mv)` → 如果 memoryview 基于 ctypes raw address，越界读取可导致 segfault

**建议**: 添加显式边界检查。

#### CL-C3. Buddy block 在 `alloc` 和 `sendall` 之间异常泄漏

**文件**: `ipc_v3_client.py:193-211`

`call()` 中取得 buddy 分配后，如果 `write_call_into`（line 205）或 `sock.sendall`（line 211）抛异常，已分配的 buddy block 永远不会被释放。`except` 子句（line 219）调用 `_close_connection` 销毁整个 pool——但如果错误是瞬态的（例如写超时但 socket 未断），block 就泄漏了。`relay()` 同理。

**建议**: 在异常路径中释放 buddy block：
```python
try:
    sock.sendall(frame)
except Exception:
    self._buddy_pool.free_at(alloc.seg_idx, alloc.offset, wire_size, alloc.is_dedicated)
    raise
```

### IMPORTANT

#### CL-I1. Handshake 只发送 1 个 segment，但 pool 可能增长到多个

**文件**: `ipc_v3_client.py:135-168`

Handshake 强制创建 segment 0 并只发送该 segment 信息给服务端。如果 `alloc()` 后续触发创建新 segment（最多 `max_segments=4`），服务端永远不会被通知。Buddy call frame 引用的 `seg_idx` 指向服务端不知道的 segment。同样，`_seg_views` 只在 handshake 时填充，不会扩展。

这意味着在 segment 1-3 上的分配会引用不存在的 memoryview（`IndexError` at line 203），服务端也无法读取未被告知的 segment 数据。

**建议**: 要么 (a) 限制 pool 只使用 1 个 segment，(b) 新 segment 创建时发送 `CTRL_BUDDY_ANNOUNCE`，(c) 懒缓存新 segment views。

#### CL-I2. `_recv_exact` 每次调用都创建 `bytearray` + `bytes` 拷贝

**文件**: `ipc_v3_client.py:316-323`

每次调用分配一个 `bytearray`，用 `extend` 增长，再拷贝为 `bytes`。在热路径上每个 RPC 调用两次（header + payload）。对于 16 字节 header，`socket.recv(16)` 几乎总是一次返回完整数据。

**建议**: 添加快速路径：
```python
def _recv_exact(self, n: int) -> bytes:
    data = self._sock.recv(n)
    if len(data) == n:
        return data
    if not data:
        raise ConnectionResetError(...)
    buf = bytearray(data)
    while len(buf) < n:
        chunk = self._sock.recv(n - len(buf))
        ...
    return bytes(buf)
```

#### CL-I3. `_ensure_connection` 在 `_conn_lock` 内可能阻塞 10+ 秒

**文件**: `ipc_v3_client.py:90-96`

`call()` 和 `relay()` 在 `_conn_lock` 下调用 `_ensure_connection`，其中 `_connect` 有 10 秒超时，`_do_buddy_handshake` 涉及网络 I/O。如果一个线程在重连，所有其他线程阻塞 10+ 秒。

**建议**: 在锁外连接，然后原子性地换入新 socket。

#### CL-I4. `free_at` 异常被静默吞没

**文件**: `ipc_v3_client.py:301-304`

```python
try:
    self._buddy_pool.free_at(seg_idx, offset, data_size, is_dedicated)
except Exception:
    pass
```

如果释放失败（double-free、元数据损坏），客户端无法检测 pool 损坏。

**建议**: 至少记录日志。

#### CL-I5. `ping()` 和 `shutdown()` 异常时 socket 泄漏

**文件**: `ipc_v3_client.py:330-371`

两个静态方法创建 socket 但只在正常路径关闭。如果连接/发送/接收抛异常，socket 泄漏。

**建议**: 使用 `try/finally` 或 context manager。

#### CL-I6. `relay()` 返回 `bytes(response_bytes)` 多余拷贝

**文件**: `ipc_v3_client.py:271`

`_recv_response` 已经返回 `bytes`（line 299 做了 `bytes(seg_mv[...])`），`relay()` 再包一层 `bytes()` 是冗余拷贝。

---

## 四、测试与基准测试

### CRITICAL

#### T-C1. `test_concurrent_read_at_free_at` 实际上不是并发的——给出虚假信心

**文件**: `tests/unit/test_buddy_pool.py:273-311`

测试名称和文档字符串声称 "concurrent read_at/free_at under concurrency"，但 writer 和 reader 在主线程上顺序执行，没有任何线程化。这给出了跨进程 read/free 路径线程安全的虚假信心。

```python
# Line 302-306: 都在主线程顺序运行
allocs = []
count = 50
writer(allocs, count)   # 主线程
reader(allocs)          # 主线程
```

#### T-C2. `test_large_payload_uses_buddy` 从未验证 buddy 路径是否被使用

**文件**: `tests/unit/test_ipc_v3.py:182-218`

测试名称声称测试大 payload 走 buddy allocator SHM 路径，但测试只调用 `crm.greeting('Test')` 产生约 13 字节响应——远低于 `shm_threshold=1024`。没有断言或 instrumentation 确认 buddy 路径被走到。

#### T-C3. Benchmark `_percentile` 函数不正确

**文件**: `benchmarks/ipc_v2_vs_v3_benchmark.py:92-95`

百分位计算每次调用都排序（O(n log n)），使用的索引计算对标准百分位定义有 off-by-one 误差。不处理 `len(data) == 0`（会在 `min(idx, len(data) - 1)` 上产生 -1 索引）。影响所有报告的 P95/P99 值。

### IMPORTANT

#### T-I1. 无 buddy merge 正确性压力测试

**文件**: `tests/unit/test_buddy_pool.py`

Rust allocator 的递归 merge 路径（`free_and_merge`）从未被压力测试。没有测试：分配多个小 block、以各种顺序释放、验证完全合并回初始可用空间。

#### T-I2. 无 dedicated segment 限制耗尽测试

没有测试验证超过 `max_dedicated_segments` 限制时是否正确返回错误。`TestDedicatedFallback` 只分配 1 个 dedicated segment。

#### T-I3. 无 segment 限制耗尽测试

没有测试填满所有 buddy segment 和 dedicated segment 来验证三层降级策略的错误边界。

#### T-I4. 无跨进程测试

所有测试在单进程中运行。`test_open_segment` 用两个 pool handle 模拟跨进程，但 spinlock 的 `AtomicU32` CAS 和 SHM attach 路径从未在实际独立进程中测试。这很重要，因为原子操作在跨进程边界可能有不同行为。

#### T-I5. 无内存泄漏检测

`pool.destroy()` 后没有验证 SHM segment 是否实际被 unlink。泄漏的 POSIX SHM segment 会持续到重启。应在 destroy 后尝试打开 segment name，预期 `FileNotFoundError`。

#### T-I6. Benchmark 未控制 GC 和系统噪声

- 未在计时区间禁用 Python GC
- 未设置 CPU 亲和性或进程优先级
- 未报告标准差或置信区间
- 大尺寸（512MB, 1GB）的 3-5 轮太少，P99 of 5 samples 等于最大值

#### T-I7. `measure_v3.py` 正则解析脆弱

**文件**: `measure_v3.py:21-27`

测量脚本用正则解析 benchmark 输出，依赖精确的空格格式。如果 `print_table` 格式变更，脚本静默返回空数据。没有断言预期的 size 数量被解析。

#### T-I8. `test_ipc_v3.py` fixture 未验证服务端实际使用 v3

**文件**: `tests/unit/test_ipc_v3.py:82-110`

`ipc_v3_server` fixture 用 `ipc-v3://` 地址启动服务端，但从未断言 buddy handshake 完成或 SHM transport 被使用。如果实现静默回退到 v2 或 inline transport，所有测试仍会通过。

### MINOR

#### T-M1. 手动创建的 pool 在断言失败时不清理 SHM

多个测试手动创建 pool 并在末尾调用 `pool.destroy()`。如果断言在中途失败，`destroy()` 不会被调用，SHM segment 泄漏。应使用 `try/finally` 或 fixture。

#### T-M2. 硬编码 sleep 值导致 flaky tests

**文件**: `tests/unit/test_ipc_v3.py:100, 106, 145, 146`

服务端启动用 `time.sleep(0.1)` 轮询。在高负载下可能不稳定。应用事件/条件变量通知启动。

#### T-M3. `encode_ctrl_buddy_announce` / `decode_ctrl_buddy_announce` 无单元测试

`ipc_v3_protocol.py:160-181` 中的编解码函数没有任何测试覆盖。

---

## 五、跨模块架构问题

### A-1. 多 segment 生命周期管理断裂——客户端/服务端不同步

**涉及文件**: `ipc_v3_client.py:135-168`, `ipc_v3_server.py:427-462`, `pool.rs`

这是最重要的架构问题。客户端在 handshake 时只通知服务端 segment 0，但 Rust pool 配置允许最多 4 个 segment。当 buddy pool 因碎片或容量不足创建新 segment 时：

1. 客户端 `_seg_views` 不包含新 segment 的 memoryview → `IndexError`
2. 服务端不知道新 segment 存在 → 无法读取数据
3. 协议中已定义 `CTRL_BUDDY_ANNOUNCE` 但未被使用

当前代码在 `max_segments=1` 时安全，但配置默认值为 4，这是一个定时炸弹。

### A-2. Wire 数据全链路缺乏输入验证

`seg_idx`、`offset`、`data_size` 从 wire 解码后直接用于内存操作（memoryview 索引、Rust FFI 调用），无任何验证。这影响：

- 服务端 `_resolve_request`（S-C2, S-C3）
- 客户端 `_recv_response`（CL-C2）
- Rust FFI `free_at`、`read_at`（R-I4）

恶意或 buggy 对端可以：
- 触发 `IndexError` 崩溃
- 触发越界 SHM 读取（可能 segfault）
- 通过传入错误 offset/size 破坏 buddy allocator bitmap

### A-3. optimize.md 中 10MB/100MB 残余回退的根因分析可进一步深入

**文件**: `doc/optimize.md:384`

报告中将 10MB (+8.4%) 和 100MB (+7.1%) 的残余回退归因于 "buddy allocator 的固有 FFI 开销"。但每次 alloc/free 仅 ~1μs，对于 10MB 传输（~3ms）占比约 0.07%。更可能的原因是：

- Buddy segment 的 `memcpy` 路径与 v2 SharedMemory.buf 的 `memcpy` 路径之间存在内存访问模式差异（TLB miss、page fault 模式不同）
- v3 额外的 struct.pack/unpack 帧头开销在中等大小时相对占比更高
- v2 pool segment 是连续映射的单一 SharedMemory，v3 buddy block 可能位于不同的虚拟地址页

---

## 六、总结

### 问题统计

| 模块 | CRITICAL | IMPORTANT | MINOR |
|------|----------|-----------|-------|
| Rust Buddy Allocator | 5 | 7 | — |
| Python Server | 4 | 5 | — |
| Python Client | 3 | 6 | — |
| 测试/Benchmark | 3 | 8 | 3 |
| 跨模块架构 | — | 3 | — |
| **合计** | **15** | **29** | **3** |

### 最高优先级修复建议（按紧迫度排序）

1. **R-C1**: Spinlock panic-safety guard（2 行代码修复，避免永久死锁）
2. **CL-C1**: `call()` 缩进 bug（dedicated 路径双重发送帧，破坏协议）
3. **A-1 + CL-I1**: 多 segment 同步机制（要么限制为 1 segment，要么实现 announce）
4. **S-C2/S-C3/CL-C2**: Wire 数据边界验证（防止恶意输入导致崩溃/segfault）
5. **S-C4**: Handshake seg_count 上限（防止 DoS）
6. **R-C5 + R-I4**: free() 输入验证和 double-free 检测（防止 bitmap 腐蚀）
7. **T-C1**: 修复伪并发测试（当前无法检测真正的并发 bug）
8. **S-I1**: `_resolve_request` 与 `destroy()` 的竞争条件

### 性能优化方向建议

1. **R-I1**: `Mutex` → `RwLock`，减少读操作锁竞争
2. **CL-I2**: `_recv_exact` 快速路径，消除热路径上的 bytearray 分配
3. **S-I1**: 全双工 per-connection（如果设计意图是 full-duplex）
4. **CL-I6**: 消除 `relay()` 中的冗余 `bytes()` 拷贝

