# IPC v3 修缮任务提示词

> 将此文件内容作为提示词提供给 AI 编码助手（Claude Code / Copilot CLI / Cursor 等）。

---

## 背景

你正在 `rust-buddy` 分支上工作。该分支实现了基于 Rust buddy allocator 的 IPC v3 共享内存传输层，并经过了一轮大载荷性能优化。两份代码审查报告（`doc/ru-op1.md` 和 `doc/op2-analysis.md`）发现了多个安全性、并发性和正确性问题。

你的任务是**逐项修复这些问题**。修复必须满足以下约束：

1. **所有现有测试必须继续通过**（`uv run pytest -q` 全绿）
2. **不引入新的性能回退**——修复应最小化对热路径的影响
3. **每个修复对应一个独立 commit**，commit message 引用问题 ID（如 `fix(ipc-v3): R-C1 spinlock panic-safety guard`）
4. **修复顺序**按下面的优先级分组，组内可并行

---

## 第一组：合并前必须修复（CRITICAL）

### Fix 1: R-C1 — Spinlock panic-safety guard

**文件**: `rust/c2_buddy/src/spinlock.rs`
**问题**: `with_lock` 中如果闭包 panic，`unlock()` 永远不会被调用，导致所有共享 SHM 的进程永久死锁。
**修复**: 使用 RAII guard 模式，确保无论闭包是否 panic 都会执行 unlock：

```rust
pub fn with_lock<F, R>(&self, f: F) -> R
where
    F: FnOnce() -> R,
{
    self.lock();
    struct Guard<'a>(&'a ShmSpinlock);
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.0.unlock();
        }
    }
    let _guard = Guard(self);
    f()
}
```

### Fix 2: OPT-C1 — 实验 17 零拷贝 SHM 返回的 use-after-free

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`
**问题**: `call()` 对 >= 1MB 响应直接返回 SHM memoryview，在下一次 `call()` 时才释放 buddy block。如果调用者存储引用、传给其他线程、或在回调中使用，会触发 use-after-free（segfault 或数据损坏）。
**修复**: 默认走安全路径（`bytes()` 拷贝）。将零拷贝作为 opt-in：

```python
# 在 call() 中，当 free_info is not None 且 data_size >= _WARM_BUF_THRESHOLD 时：
# 始终走安全拷贝路径
payload = bytes(mv[5:])
self._free_buddy_response(free_info)
return payload
```

如果需要保留零拷贝选项，改为 opt-in 参数：在 `call()` 签名中添加 `_zero_copy: bool = False`，仅当显式传入 `True` 时才走延迟释放路径。同时在 docstring 中明确警告：返回的 memoryview 仅在下一次 `call()` 之前有效，不可存储或跨线程传递。

### Fix 3: OPT-T1 — 空 bytes `b""` 被静默转为 None

**文件**: `src/c_two/rpc/transferable.py`
**问题**: bytes 快速路径的输入反序列化器中 `if not data: return None`，Python 中 `not b""` 为 `True`，导致空 bytes 被错误转为 None。
**修复**: 将 `if not data:` 改为 `if data is None:`：

```python
# 找到 bytes 快速路径的 input deserializer（在 _is_single_bytes_param 分支内）
# 修改前：
if not data:
    return None
# 修改后：
if data is None:
    return None
```

### Fix 4: OPT-S2 — `destroy()` 不排空 `_deferred_frees`

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`
**问题**: `destroy()` 清空 `_pending` 并清理连接，但从未排空 `_deferred_frees`。in-flight 请求的 buddy block 被遗漏。
**修复**: 在 `destroy()` 的连接清理循环之前，排空 deferred frees：

```python
def destroy(self):
    # ... 现有的 cancel_all_calls / _pending.clear 逻辑 ...

    # 排空 deferred frees
    with self._deferred_frees_lock:
        deferred = dict(self._deferred_frees)
        self._deferred_frees.clear()
    for rid_key, info in deferred.items():
        try:
            pool, seg_idx, offset, data_size, is_dedicated = info[:5]
            pool.free_at(seg_idx, offset, data_size, is_dedicated)
        except Exception:
            pass  # pool 可能已销毁

    # ... 然后继续现有的连接清理循环 ...
```

### Fix 5: OPT-S9 — TOCTOU 竞争（reuse 路径 deferred frees 读/弹分离）

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`
**问题**: `reply()` 的 reuse 路径中，`_deferred_frees` 先读取再弹出，两次操作之间另一线程可能已弹出同一条目，导致 BUDDY_REUSE 帧引用已释放的 block。
**修复**: 合并读取和弹出为单次锁获取：

```python
# 在 reply() 的 reuse 检测逻辑中：
# 修改前（两次获锁）：
with self._deferred_frees_lock:
    info = self._deferred_frees.get(rid_key)
# ... 构造 reuse frame ...
with self._deferred_frees_lock:
    self._deferred_frees.pop(rid_key, None)

# 修改后（单次获锁，原子性 pop）：
with self._deferred_frees_lock:
    info = self._deferred_frees.pop(rid_key, None)
if info is not None and len(info) >= 8 and info[5] is not None:
    # ... reuse 逻辑 ...
else:
    # ... 普通 alloc+copy 路径 ...
```

### Fix 6: R-C5 + R-I4 — free() 无输入验证 + double-free stats 下溢

**文件**: `rust/c2_buddy/src/allocator.rs`
**问题**: `free()` 不验证 offset 对齐、level 范围、或 block 是否已分配。double-free 导致 `alloc_count` u32 下溢。
**修复**: 在 `free_and_merge` 开头添加验证：

```rust
fn free_and_merge(&self, offset: u32, level: u16) {
    // 验证 level 在范围内
    assert!((level as usize) < self.levels, "free: level {} out of range (max {})", level, self.levels);

    let block_size = self.min_block << (self.levels - 1 - level as usize);
    // 验证 offset 对齐
    assert!(offset as usize % block_size == 0, "free: offset {} not aligned to block size {}", offset, block_size);

    let block_idx = (offset as usize / block_size) as u32;
    // 验证 block 当前已分配（bitmap 中标记为 used）
    debug_assert!(
        self.bitmap.is_used(level as usize, block_idx),
        "free: double-free detected at offset={}, level={}", offset, level
    );

    // ... 原有的 free + merge 逻辑 ...
}
```

同时在 `update_stats_free` 中添加饱和减法或检查：

```rust
fn update_stats_free(&self, size: usize) {
    let header = unsafe { &*(self.base as *const SegmentHeader) };
    let prev = header.alloc_count.fetch_sub(1, Ordering::Release);
    debug_assert!(prev > 0, "alloc_count underflow in free");
    header.free_bytes.fetch_add(size as u64, Ordering::Release);
}
```

---

## 第二组：重要修复（IMPORTANT）

### Fix 7: R-C3 + R-C4 — SegmentHeader 对齐和 lock-free 断言

**文件**: `rust/c2_buddy/src/allocator.rs`
**修复**: 在 `BuddyAllocator::init()` 开头添加：

```rust
assert!(base as usize % std::mem::align_of::<SegmentHeader>() == 0,
    "SHM base pointer must be aligned to {}", std::mem::align_of::<SegmentHeader>());
assert!(AtomicU64::is_lock_free(), "AtomicU64 must be lock-free for cross-process SHM");
assert!(AtomicU32::is_lock_free(), "AtomicU32 must be lock-free for cross-process SHM");
```

### Fix 8: R-I1 — BuddyPool Mutex → RwLock

**文件**: `rust/c2_buddy/src/ffi.rs`, `rust/c2_buddy/src/pool.rs`
**问题**: 整个 pool 被 `Mutex` 包裹，只读操作（`data_ptr`, `stats`, `read_at`, `seg_data_info`）也需要独占锁。
**修复**: 将 `Mutex<BuddyPool>` 改为 `RwLock<BuddyPool>`。将 `data_ptr_at`, `read_at`, `stats`, `seg_data_info` 改为取 `&self`（只读）并在 FFI 层用 `pool.read().unwrap()` 获取读锁。仅 `alloc`, `free_at`, `create_segment`, `open_segment`, `destroy` 等修改操作使用 `pool.write().unwrap()` 获取写锁。

### Fix 9: OPT-T2 — transferable bytes 快速路径 serialize/deserialize 不对称

**文件**: `src/c_two/rpc/transferable.py`
**问题**: 输入 serialize 有 pickle fallback 但 deserialize 无对应处理。
**修复**: 移除输入 serialize 的 pickle fallback，改为严格类型检查：

```python
def serialize(data) -> bytes:
    if isinstance(data, (bytes, memoryview)):
        return data
    raise TypeError(f"bytes fast path: expected bytes or memoryview, got {type(data).__name__}")
```

### Fix 10: OPT-C-I1 — 移除实验 16 暖缓冲区死代码

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`
**问题**: 实验 17 完全取代了实验 16，暖缓冲区基础设施（`_response_buf`, `_ensure_response_buf`, libc memmove/memset 加载）成为死代码，但 import 时仍加载 libc。
**修复**: 移除以下内容：
- 模块级的 `ctypes.CDLL(ctypes.util.find_library('c'))`, `_memmove`, `_memset` 声明
- `__init__` 中的 `_response_buf`, `_response_buf_addr`, `_seg_base_addrs` 初始化
- `_ensure_response_buf()` 方法
- `_WARM_BUF_THRESHOLD` 常量（如果 Fix 2 改为默认安全路径，此常量仅用于 threshold 判断，may still be needed）

注意：如果保留 opt-in 零拷贝（Fix 2 方案 A），`_WARM_BUF_THRESHOLD` 仍然需要，但暖缓冲区不需要。

### Fix 11: OPT-C-I2 — `_close_connection()` 清除 `_seg_base_addrs`

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`
**修复**: 在 `_close_connection()` 中添加 `self._seg_base_addrs = []`，与 `self._seg_views = []` 保持一致。

### Fix 12: R-I5 — shm_unlink + shm_open TOCTOU

**文件**: `rust/c2_buddy/src/segment.rs`
**问题**: `shm_unlink` 后 `shm_open(O_CREAT|O_EXCL)` 之间存在 TOCTOU 竞争。
**修复**: 将 `O_CREAT | O_EXCL` 改为 `O_CREAT | O_RDWR`（不使用 `O_EXCL`）。或者在 `shm_open` 成功后检查 header magic 验证归属。

### Fix 13: R-I6 — ShmSegment::open 映射超出文件大小

**文件**: `rust/c2_buddy/src/segment.rs`
**修复**: 在 `open()` 中添加断言：

```rust
assert!(actual_size >= expected_size,
    "SHM segment actual size {} < expected size {}", actual_size, expected_size);
let map_size = actual_size;  // 使用实际大小，不取 max
```

### Fix 14: S-I1 — 全双工声明 vs 串行 await 实现

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`
**问题**: 模块文档声称 "Full-duplex"，但 `_handle_client` 中 `await fut` 阻塞读循环。
**修复二选一**:
- (a) 如果设计意图确实是半双工：修改文档，移除 "full-duplex" 声明
- (b) 如果设计意图是全双工：将 `await fut` + 写响应拆分为 `asyncio.create_task`，读循环继续读下一帧

### Fix 15: S-I3 — `destroy()` 不 cancel pending futures

**文件**: `src/c_two/rpc/ipc/ipc_v3_server.py`
**修复**: 在 `destroy()` 开头调用 `self.cancel_all_calls()`，然后再 `_pending.clear()`。

---

## 第三组：建议修复（SUGGESTION / MINOR）

### Fix 16: R-C2 — Bitmap 方法添加文档

**文件**: `rust/c2_buddy/src/bitmap.rs`
**修复**: 在 `BuddyBitmap` 的 module doc 或 struct doc 中添加：
```rust
/// SAFETY: All methods on `BuddyBitmap` assume the caller holds the segment's `ShmSpinlock`.
/// The atomic operations are used for cross-process visibility, NOT for lock-free concurrency.
```

### Fix 17: OPT-S4 — 文档化 materialize_response_views 与 reuse 的依赖

**文件**: `src/c_two/rpc/server.py` 或 `src/c_two/rpc/ipc/ipc_v3_server.py`
**修复**: 在 `materialize_response_views=False` 设置处添加注释：

```python
# NOTE: materialize_response_views=False is a prerequisite for the
# zero-copy reuse optimization (BUDDY_REUSE path) in reply().
# If set to True, memoryview results are copied to bytes before reply(),
# which disables reuse detection (isinstance check fails on bytes).
```

### Fix 18: OPT-C-S1 — 客户端内联 CRM_REPLY 的 err_len 边界检查

**文件**: `src/c_two/rpc/ipc/ipc_v3_client.py`
**修复**: 在读取 error bytes 前添加验证：

```python
err_len = _U32_LE.unpack_from(mv, 1)[0]
if err_len > 0:
    err_end = 5 + err_len
    if err_end > len(mv):
        self._free_buddy_response(free_info)
        raise error.CompoClientError(f'Corrupted CRM_REPLY: err_len={err_len} exceeds data size={len(mv)}')
    # ... 原有错误处理 ...
```

### Fix 19: R-I2 + R-I3 — u32 截断保护

**文件**: `rust/c2_buddy/src/pool.rs`, `rust/c2_buddy/src/allocator.rs`
**修复**:
- `pool.rs` 中 `let alloc_size = seg.size() as u32` 前添加 `assert!(seg.size() <= u32::MAX as usize)`
- `allocator.rs::init()` 中添加 `assert!(data_size <= u32::MAX as usize, "data region exceeds u32::MAX")`

---

## 验证步骤

每完成一组修复后：

```bash
# 1. 运行完整测试套件
uv run pytest -q

# 2. 如果修改了 Rust 代码，先编译
cd rust/c2_buddy && cargo build --release && cd ../..

# 3. 运行性能回归检测（确认不引入回退）
uv run python measure_v3.py

# 4. 检查 git diff 确认改动最小化
git diff --stat
```

---

## 不要做的事

- **不要** 移除实验 7 的 reuse 路径——它在其适用场景下是有效的，只需修复并发缺陷
- **不要** 重构不相关的代码——保持改动最小化
- **不要** 修改 IPC v2 代码或 MCP/CLI 代码
- **不要** 修改现有测试的断言逻辑（可以新增测试）
- **不要** 在 commit message 中使用 `--no-verify` 跳过 hooks

---

## 相关文件速查

| 文件 | 说明 |
|------|------|
| `rust/c2_buddy/src/spinlock.rs` | SHM 跨进程自旋锁 |
| `rust/c2_buddy/src/allocator.rs` | Buddy 分配/释放/合并核心算法 |
| `rust/c2_buddy/src/bitmap.rs` | 层级 bitmap 操作 |
| `rust/c2_buddy/src/segment.rs` | SHM segment 管理（创建/打开/映射） |
| `rust/c2_buddy/src/pool.rs` | 多 segment pool + dedicated segment |
| `rust/c2_buddy/src/ffi.rs` | PyO3 FFI 导出层 |
| `src/c_two/rpc/ipc/ipc_v3_server.py` | IPC v3 服务端 |
| `src/c_two/rpc/ipc/ipc_v3_client.py` | IPC v3 客户端 |
| `src/c_two/rpc/ipc/ipc_v3_protocol.py` | Wire format 编解码 |
| `src/c_two/rpc/transferable.py` | bytes 快速路径 + memoryview 直通 |
| `src/c_two/rpc/server.py` | `materialize_response_views` 标志 |
| `doc/ru-op1.md` | 第一轮代码审查报告 |
| `doc/op2-analysis.md` | 第二轮优化审计报告 |
